//! Nintendo Power "GB Memory" (DMG-MMSA-JPN / G-MMC1) cartridge support.
//!
//! This is the cart the GB Operator physically cannot write: its flash is
//! hidden behind the G-MMC1 mapper, which must be woken and unlocked with raw
//! cartridge-bus writes before the flash is even visible. The GBxCart exposes
//! exactly that primitive (`0xB2` bus writes), so throwback can do it.
//!
//! The megabyte of flash is programmed with the L-firmware `FLASH_PROGRAM`
//! path (command-set GBMEMORY, method DMG_MMSA — the firmware runs the
//! per-page flash dance internally). The hidden 128-byte map sector uses the
//! dedicated MMSA opcode (`0xB7`). Sequences follow FlashGBX's behaviour
//! (Mapper.py `DMG_GMMC1`, Flashcart.py `Flashcart_DMG_MMSA`, LK_Device
//! `WriteROM_GBMEMORY`) — reimplemented from the protocol, not its code.
//!
//! Pure image assembly / extraction / map building lives in `crate::gbmemory`;
//! this module is only the on-device bus choreography.

use std::time::{Duration, Instant};

use crate::device::DeviceError;

use super::protocol::*;
use super::{GbxCart, Transport};

// G-MMC1 MMC register interface (bus addresses; 8-bit values).
const MMC_CMD: u16 = 0x0120; // command byte
const MMC_ARG1: u16 = 0x0121; // wakeup arg 1
const MMC_ARG2: u16 = 0x0122; // wakeup arg 2
const MMC_ADDR_HI: u16 = 0x0125; // direct-write address high / WP arg
const MMC_ADDR_LO: u16 = 0x0126; // direct-write address low / WP arg
const MMC_DATA: u16 = 0x0127; // direct-write data
const MMC_EXEC: u16 = 0x013F; // execute the staged command
const MMC_MAGIC: u8 = 0xA5; // execute magic value

// MMC command opcodes (written to MMC_CMD).
const MMC_SLEEP: u8 = 0x08; // undo wakeup
const MMC_WAKE: u8 = 0x09; // + ARG1=0xAA, ARG2=0x55
const MMC_MAP_FULL: u8 = 0x04; // expose whole 1 MiB MBC5-style
const MMC_WP_ARM: u8 = 0x0A; // + ADDR_HI=0x62, ADDR_LO=0x04
const MMC_WP_DISABLE: u8 = 0x02;
const MMC_MBC_ENABLE: u8 = 0x11;
const MMC_MBC_DISABLE: u8 = 0x10;
const MMC_FLASH_WRITE: u8 = 0x0F; // + ADDR_HI/LO + DATA

// Flash chip (Macronix behind the MMC) command values.
const FLASH_RESET: u8 = 0xF0;
const FLASH_ID: [u8; 2] = [0xC2, 0x89]; // GB Memory flash software ID
const MMC5_BANK_WRITE: u16 = 0x2100;

// FLASH_PROGRAM (method DMG_MMSA) config.
const CMD_SET_GBMEMORY: u8 = 0x00;
const METHOD_DMG_MMSA: u8 = 0x03;
const WE_PIN_WR: u8 = 0x01;
/// Firmware status poll: ready when `(sr & 0xB2) == 0x80` (fw >= 12).
const SR_MASK: u32 = 0xB2;
const SR_VALUE: u32 = 0x80;

const IMAGE_SIZE: usize = crate::gbmemory::IMAGE_SIZE;
const MAP_SIZE: usize = crate::gbmemory::MAP_SIZE;
/// GB Memory main-flash program chunk (firmware buffers 128-byte pages itself).
const GBM_CHUNK: usize = 0x400;
/// Host-side status-poll bit (DQ7).
const STATUS_READY: u8 = 0x80;
/// Shared SRAM: 16 banks × 8 KiB = 128 KiB, split across games by the map.
const SRAM_SIZE: usize = 128 * 1024;
const SRAM_BANK: usize = 0x2000;

impl<T: Transport> GbxCart<T> {
    // --- MMC primitives ------------------------------------------------------

    /// Stage `cmd` in MMC_CMD and execute it.
    fn mmc_cmd(&mut self, cmd: u8) -> Result<(), DeviceError> {
        self.dmg_write(MMC_CMD, cmd)?;
        self.dmg_write(MMC_EXEC, MMC_MAGIC)
    }

    /// Wake the MMC command interface (required after power-up).
    fn mmc_wake(&mut self) -> Result<(), DeviceError> {
        self.dmg_write(MMC_CMD, MMC_WAKE)?;
        self.dmg_write(MMC_ARG1, 0xAA)?;
        self.dmg_write(MMC_ARG2, 0x55)?;
        self.dmg_write(MMC_EXEC, MMC_MAGIC)
    }

    /// Disable flash write protection (arm, then disable).
    fn mmc_disable_wp(&mut self) -> Result<(), DeviceError> {
        self.dmg_write(MMC_CMD, MMC_WP_ARM)?;
        self.dmg_write(MMC_ADDR_HI, 0x62)?;
        self.dmg_write(MMC_ADDR_LO, 0x04)?;
        self.dmg_write(MMC_EXEC, MMC_MAGIC)?;
        self.mmc_cmd(MMC_WP_DISABLE)
    }

    /// Direct flash bus write `flash[addr] = value`, bypassing the MBC
    /// (MMC command 0x0F). Used for all flash command-register writes.
    fn flash_write(&mut self, addr: u16, value: u8) -> Result<(), DeviceError> {
        self.dmg_write(MMC_CMD, MMC_FLASH_WRITE)?;
        self.dmg_write(MMC_ADDR_HI, (addr >> 8) as u8)?;
        self.dmg_write(MMC_ADDR_LO, (addr & 0xFF) as u8)?;
        self.dmg_write(MMC_DATA, value)?;
        self.dmg_write(MMC_EXEC, MMC_MAGIC)
    }

    /// Select a ROM bank for the switchable 0x4000 window. The G-MMC1 in
    /// map-full mode banks via the 0x2100 register ONLY — writing the MBC5
    /// 9th-bit register (0x3000) breaks its banking (verified on hardware:
    /// with a 0x3000 write every bank read returns the same data). 64 banks
    /// fit in the 8-bit 0x2100 register, so 0x3000 is never needed.
    fn select_bank(&mut self, bank: u32) -> Result<(), DeviceError> {
        self.dmg_write(MMC5_BANK_WRITE, (bank & 0xFF) as u8)
    }

    /// Issue an AMD-style flash command prefix `0x5555=AA, 0x2AAA=55, 0x5555=cmd`.
    fn flash_unlock_cmd(&mut self, cmd: u8) -> Result<(), DeviceError> {
        self.flash_write(0x5555, 0xAA)?;
        self.flash_write(0x2AAA, 0x55)?;
        self.flash_write(0x5555, cmd)
    }

    /// Read one status byte at flash bus address `addr` (via a normal ROM read).
    fn flash_status(&mut self, addr: u16) -> Result<u8, DeviceError> {
        Ok(self.read_dmg_rom_chunk(addr as u32, 1)?[0])
    }

    /// Poll the flash status at `addr` until DQ7 is set, bounded by `timeout`.
    fn poll_ready(&mut self, addr: u16, timeout: Duration) -> Result<(), DeviceError> {
        let start = Instant::now();
        loop {
            if self.flash_status(addr)? & STATUS_READY != 0 {
                return Ok(());
            }
            if start.elapsed() > timeout {
                return Err(DeviceError::Protocol(
                    "GB Memory flash operation timed out".into(),
                ));
            }
        }
    }

    // --- Detection -----------------------------------------------------------

    /// Whether the inserted cart is a GB Memory (G-MMC1). True either when the
    /// header is the NP menu ROM (menu-state cart) or when the flash software
    /// ID reads back `C2 89` after waking the mapper (full-mode single-game
    /// cart, whose header looks like an ordinary game). Requires a DMG cart.
    pub(super) fn detect_gb_memory(&mut self) -> Result<bool, DeviceError> {
        {
            let state = self.require_state()?;
            if state.family != super::CartFamily::Dmg {
                return Ok(false);
            }
            if crate::gbmemory::is_menu_rom(&state.header) {
                return Ok(true);
            }
        }
        // Full-mode carts look like a normal game; the only tell is the flash
        // ID behind the woken mapper. The probe is read-mostly and harmless on
        // a normal cart (a mask ROM ignores the command writes and returns ROM
        // bytes, not C2 89).
        self.enter_dmg_mode()?;
        self.mmc_wake()?;
        self.mmc_cmd(MMC_MBC_ENABLE)?;
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?;
        self.flash_unlock_cmd(0x90)?; // autoselect
        let id = self.read_dmg_rom_chunk(0, 2)?;
        self.flash_write(0x0000, FLASH_RESET)?;
        self.mmc_cmd(MMC_SLEEP)?;
        self.idle();
        Ok(id == FLASH_ID)
    }

    // --- Mapper setup --------------------------------------------------------

    /// Enable the mapper and expose the full 1 MiB (MBC5-style), then sleep.
    fn enable_mapper(&mut self) -> Result<(), DeviceError> {
        self.mmc_wake()?;
        self.mmc_cmd(MMC_MBC_ENABLE)?;
        self.mmc_cmd(MMC_MAP_FULL)?;
        self.flash_write(0x0000, FLASH_RESET)?;
        self.mmc_cmd(MMC_SLEEP)
    }

    /// Full wake + unlock so the flash is writable (FlashGBX `UnlockForWriting`).
    fn unlock_for_writing(&mut self) -> Result<(), DeviceError> {
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?; // A14 high for 0x5555 commands
        self.mmc_wake()?;
        self.mmc_cmd(MMC_MBC_ENABLE)?;
        self.mmc_disable_wp()?;
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?;
        self.flash_write(0x0000, 0xB0)?; // erase-suspend any stale erase
        // Unprotect sector 0 (60/40) so the mass erase can clear it too.
        self.flash_write(0x5555, 0xAA)?;
        self.flash_write(0x2AAA, 0x55)?;
        self.flash_write(0x5555, 0x60)?;
        self.flash_write(0x5555, 0xAA)?;
        self.flash_write(0x2AAA, 0x55)?;
        self.flash_write(0x0000, 0x40)?;
        self.poll_ready(0x0000, Duration::from_secs(5))
    }

    fn verify_flash_id(&mut self) -> Result<(), DeviceError> {
        self.flash_unlock_cmd(0x90)?;
        let id = self.read_dmg_rom_chunk(0, 2)?;
        self.flash_write(0x0000, FLASH_RESET)?;
        if id != FLASH_ID {
            return Err(DeviceError::NotFlashable(format!(
                "not a GB Memory cartridge (flash id {:02X} {:02X}, expected C2 89)",
                id[0], id[1]
            )));
        }
        Ok(())
    }

    // --- Erase / program -----------------------------------------------------

    fn chip_erase(&mut self, erase_progress: &dyn Fn(&str)) -> Result<(), DeviceError> {
        self.flash_write(0x5555, 0xAA)?;
        self.flash_write(0x2AAA, 0x55)?;
        self.flash_write(0x5555, 0x80)?;
        self.flash_write(0x5555, 0xAA)?;
        self.flash_write(0x2AAA, 0x55)?;
        self.flash_write(0x5555, 0x10)?;
        self.io.set_timeout(Duration::from_secs(5))?;
        let start = Instant::now();
        let mut last_msg = Instant::now();
        loop {
            if self.flash_status(0x0000)? & STATUS_READY != 0 {
                break;
            }
            if last_msg.elapsed() > Duration::from_millis(500) {
                erase_progress("Erasing...");
                last_msg = Instant::now();
            }
            if start.elapsed() > Duration::from_secs(60) {
                return Err(DeviceError::Protocol("GB Memory chip erase timed out".into()));
            }
        }
        self.io.set_timeout(Duration::from_secs(2))?;
        self.flash_write(0x0000, FLASH_RESET)?;
        self.mmc_cmd(MMC_MAP_FULL)
    }

    /// Program the 1 MiB image via the firmware DMG_MMSA path, banked MBC5-style.
    fn program_image(&mut self, image: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        self.cmd_ack(&set_flash_cmd_frame(
            CMD_SET_GBMEMORY,
            METHOD_DMG_MMSA,
            WE_PIN_WR,
            &[],
        ))?;
        if self.fw_ver >= 12 {
            self.set_variable(VAR_STATUS_REGISTER_MASK, SR_MASK)?;
            self.set_variable(VAR_STATUS_REGISTER_VALUE, SR_VALUE)?;
        }
        self.set_variable(VAR_TRANSFER_SIZE, GBM_CHUNK as u32)?;
        self.io.set_timeout(Duration::from_secs(10))?;

        let mut written = 0usize;
        for (bank, bank_data) in image.chunks(0x4000).enumerate() {
            self.select_bank(bank as u32)?;
            self.set_variable(VAR_DMG_ROM_BANK, bank as u32)?;
            let (_, window) = Self::dmg_window((bank * 0x4000) as u32);
            let mut engaged = false;
            let mut synced = false;
            let mut offset = 0usize;
            for chunk in bank_data.chunks(GBM_CHUNK) {
                if chunk.iter().all(|&b| b == 0xFF) {
                    engaged = false;
                    synced = false;
                } else {
                    if !synced {
                        self.set_variable(VAR_ADDRESS, window + offset as u32)?;
                        synced = true;
                    }
                    if !engaged {
                        self.io.write_all(&[CMD_FLASH_PROGRAM])?;
                    }
                    self.io.write_all(chunk)?;
                    engaged = self.expect_ack()?;
                }
                offset += chunk.len();
                written += chunk.len();
                progress(written as u32);
            }
        }
        self.io.set_timeout(Duration::from_secs(2))?;
        Ok(())
    }

    /// Erase the hidden map sector and write the 128-byte map via the MMSA
    /// opcode (`0xB7`). The map is always written this way, even on new
    /// firmware (LK_Device `WriteROM_GBMEMORY`).
    fn write_map(&mut self, map: &[u8]) -> Result<(), DeviceError> {
        // Erase the hidden sector.
        self.unlock_for_writing()?;
        self.flash_unlock_cmd(0x60)?;
        self.flash_unlock_cmd(0x04)?;
        self.poll_ready(0x0000, Duration::from_secs(5))?;
        // Enter hidden-map program mode.
        self.flash_unlock_cmd(0x60)?;
        self.flash_unlock_cmd(0xE0)?;
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?;
        self.mmc_cmd(MMC_MBC_DISABLE)?;
        self.mmc_cmd(MMC_SLEEP)?;

        // Per-page (single 128-byte page at address 0) setup, then stream via 0xB7.
        self.set_variable(VAR_TRANSFER_SIZE, MAP_SIZE as u32)?;
        self.set_variable(VAR_ADDRESS, 0)?;
        self.mmc_wake()?;
        self.mmc_cmd(MMC_MBC_ENABLE)?;
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?;
        self.flash_unlock_cmd(0xA0)?; // program setup
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?; // bank 1
        self.mmc_cmd(MMC_MBC_DISABLE)?;
        self.mmc_cmd(MMC_SLEEP)?;

        self.io.write_all(&[CMD_DMG_MBC6_MMSA_WRITE_FLASH])?;
        self.io.write_all(&map[..MAP_SIZE])?;
        self.expect_ack()?;

        // Trigger programming (second write to the last buffer slot), then poll.
        self.dmg_write(0x007F, 0xFF)?;
        self.poll_ready(0x007F, Duration::from_secs(2))?;
        Ok(())
    }

    /// Reset the flash + mapper to a clean, readable (map-full) state.
    fn gbm_reset(&mut self) -> Result<(), DeviceError> {
        self.dmg_write(MMC5_BANK_WRITE, 0x01)?;
        self.flash_write(0x4080, FLASH_RESET)?;
        self.mmc_cmd(MMC_MAP_FULL)?;
        self.mmc_cmd(MMC_SLEEP)
    }

    // --- Public read/write ---------------------------------------------------

    /// Read a 16 KiB bank window, re-selecting the bank and retrying on a
    /// transient serial timeout (the CH340 link occasionally drops mid-dump).
    /// Reads are idempotent, so a flush + re-read from the window is safe.
    fn read_bank_retry(
        &mut self,
        bank: Option<u32>,
        window: u32,
        len: usize,
    ) -> Result<Vec<u8>, DeviceError> {
        for attempt in 0..4 {
            if let Some(b) = bank {
                self.select_bank(b)?;
            }
            match self.read_dmg_rom_chunk(window, len) {
                Ok(data) => return Ok(data),
                Err(DeviceError::Io(e))
                    if e.kind() == std::io::ErrorKind::TimedOut && attempt < 3 =>
                {
                    self.io.flush_input();
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop returns on the last attempt")
    }

    /// Read the full 1 MiB flash image and the 128-byte hidden map sector.
    pub(super) fn read_gb_memory(
        &mut self,
        progress: &dyn Fn(u32),
    ) -> Result<(Vec<u8>, Vec<u8>), DeviceError> {
        self.enter_dmg_mode()?;
        // enable_mapper latches map-full and leaves the MMC ASLEEP. We read
        // with it asleep on purpose: the GB header title (0x134-0x143) overlaps
        // the MMC register window (0x120-0x13F), so an awake MMC intercepts
        // those reads and returns register values instead of the flash header.
        self.enable_mapper()?;

        // 64 banks × 16 KiB. Bank 0 is read through the fixed 0x0000 window;
        // banks 1+ through the switchable 0x4000 window (the standard MBC5
        // dump scheme — hardware-verified on Pokémon Yellow). Reading bank 0
        // through the switchable window does NOT return bank 0 on real
        // hardware, which silently loses the header.
        let mut rom = Vec::with_capacity(IMAGE_SIZE);
        rom.extend_from_slice(&self.read_bank_retry(None, 0x0000, 0x4000)?);
        progress(rom.len() as u32);
        for bank in 1..(IMAGE_SIZE / 0x4000) as u32 {
            let chunk = self.read_bank_retry(Some(bank), 0x4000, 0x4000)?;
            rom.extend_from_slice(&chunk);
            progress(rom.len() as u32);
        }

        let map = self.read_map()?;
        self.gbm_reset()?;
        self.idle();
        Ok((rom, map.to_vec()))
    }

    /// Read only the 128-byte map sector (cheap; used to carry over the
    /// cartridge ID and write count before a rewrite).
    pub(super) fn read_gb_memory_map(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.enter_dmg_mode()?;
        self.enable_mapper()?;
        self.mmc_wake()?;
        self.mmc_cmd(MMC_MBC_ENABLE)?;
        self.mmc_cmd(MMC_MAP_FULL)?;
        let map = self.read_map()?;
        self.gbm_reset()?;
        self.idle();
        Ok(map.to_vec())
    }

    /// Read the hidden 128-byte map sector (retrying if the mode switch failed).
    fn read_map(&mut self) -> Result<[u8; MAP_SIZE], DeviceError> {
        for _ in 0..5 {
            let baseline = self.read_dmg_rom_chunk(0, MAP_SIZE)?;
            self.mmc_wake()?;
            self.mmc_cmd(MMC_MBC_ENABLE)?;
            self.dmg_write(MMC5_BANK_WRITE, 0x01)?;
            // Expose the hidden map: 0x77 command, issued twice.
            self.flash_unlock_cmd(0x77)?;
            self.flash_unlock_cmd(0x77)?;
            self.dmg_write(MMC5_BANK_WRITE, 0x00)?;
            let data = self.read_dmg_rom_chunk(0, MAP_SIZE)?;
            self.mmc_cmd(MMC_SLEEP)?;
            if data != baseline {
                let mut map = [0u8; MAP_SIZE];
                map.copy_from_slice(&data);
                return Ok(map);
            }
        }
        Err(DeviceError::Protocol("could not read the GB Memory map sector".into()))
    }

    /// Read the full 128 KiB shared SRAM (16 banks × 8 KiB). In map-full mode
    /// the G-MMC1 emulates an MBC5, so RAM access is the ordinary MBC5 dance
    /// (enable at 0x0000, bank-select at 0x4000, read the 0xA000 window). The
    /// RAM registers don't overlap the MMC window (0x120-0x13F), so unlike the
    /// ROM header this reads cleanly with the MMC asleep.
    pub(super) fn read_gb_memory_sram(
        &mut self,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        self.enter_dmg_mode()?;
        self.enable_mapper()?;
        self.dmg_write(0x0000, 0x0A)?; // RAM enable
        let result = (|| {
            let mut sram = Vec::with_capacity(SRAM_SIZE);
            for bank in 0..(SRAM_SIZE / SRAM_BANK) as u32 {
                self.dmg_write(0x4000, (bank & 0x0F) as u8)?; // RAM bank select
                self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_RAM_READ)?;
                self.set_variable(VAR_DMG_READ_CS_PULSE, 1)?;
                let data = self.read_at(CMD_DMG_CART_READ, 0xA000, MAX_BUFFER_READ, SRAM_BANK)?;
                sram.extend_from_slice(&data);
                progress(sram.len() as u32);
            }
            Ok(sram)
        })();
        // Always drop RAM enable, even on error (bus-noise protection).
        let disable = self.dmg_write(0x0000, 0x00);
        self.gbm_reset()?;
        self.idle();
        result.and_then(|s| disable.map(|()| s))
    }

    /// Write the full 128 KiB shared SRAM back (MBC5-style, map-full mode).
    pub(super) fn write_gb_memory_sram(
        &mut self,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError> {
        if data.len() != SRAM_SIZE {
            return Err(DeviceError::Protocol("bad GB Memory SRAM size".into()));
        }
        self.enter_dmg_mode()?;
        self.enable_mapper()?;
        self.dmg_write(0x0000, 0x0A)?; // RAM enable
        let result = (|| {
            let mut written = 0usize;
            for (bank, bank_data) in data.chunks(SRAM_BANK).enumerate() {
                self.dmg_write(0x4000, (bank as u32 & 0x0F) as u8)?; // RAM bank select
                self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_RAM_WRITE)?;
                self.set_variable(VAR_DMG_WRITE_CS_PULSE, 1)?;
                self.set_variable(VAR_ADDRESS, 0xA000)?;
                self.set_variable(VAR_TRANSFER_SIZE, 0x200)?;
                for chunk in bank_data.chunks(0x200) {
                    self.io.write_all(&[CMD_DMG_CART_WRITE_SRAM])?;
                    self.io.write_all(chunk)?;
                    self.expect_ack()?;
                    written += chunk.len();
                    progress(written as u32);
                }
            }
            Ok(())
        })();
        let disable = self.dmg_write(0x0000, 0x00);
        self.gbm_reset()?;
        self.idle();
        result.and(disable)
    }

    /// Write a full 1 MiB image + map sector to the cart (chip-erases first).
    pub(super) fn write_gb_memory(
        &mut self,
        image: &[u8],
        map: &[u8],
        progress: &dyn Fn(u32),
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError> {
        if image.len() != IMAGE_SIZE || map.len() != MAP_SIZE {
            return Err(DeviceError::Protocol("bad GB Memory image/map size".into()));
        }
        self.enter_dmg_mode()?;
        erase_progress("Preparing cartridge...");
        self.enable_mapper()?;
        self.unlock_for_writing()?;
        self.verify_flash_id()?;

        erase_progress("Erasing flash...");
        self.unlock_for_writing()?;
        self.chip_erase(erase_progress)?;

        erase_progress("Writing games...");
        self.program_image(image, progress)?;

        erase_progress("Writing menu map...");
        self.write_map(map)?;
        self.gbm_reset()?;

        erase_progress("Verifying...");
        let (readback, map_readback) = self.read_gb_memory(&|_| {})?;
        if readback != image {
            return Err(DeviceError::Protocol(
                "post-write verification failed (ROM read-back differs)".into(),
            ));
        }
        // Verify the hidden map sector too — a bad map wouldn't show up in the
        // ROM comparison, but it decides which games boot and where each save
        // lives, so it must be exact.
        if map_readback != map {
            return Err(DeviceError::Protocol(
                "post-write verification failed (menu map read-back differs)".into(),
            ));
        }
        self.idle();
        Ok(())
    }
}
