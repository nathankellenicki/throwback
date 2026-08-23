//! AGB (GBA) cartridge support: ROM reads and the three save-chip families
//! (SRAM, EEPROM, flash), matching the Operator's ChipType-driven interface.

use std::time::Duration;

use crate::device::{is_open_bus, ChipType, DeviceError};

use super::protocol::*;
use super::{GbxCart, Transport};

/// EEPROM size ids on the wire: 1 = 4 Kbit (512 B), 2 = 64 Kbit (8 KB).
const EEPROM_4K: u8 = 1;
const EEPROM_64K: u8 = 2;
const EEPROM_4K_BYTES: usize = 512;
const EEPROM_64K_BYTES: usize = 8 * 1024;

/// Save-flash chip types for CMD_AGB_CART_WRITE_FLASH_DATA.
const FLASH_CHIP_NORMAL: u8 = 1;
const FLASH_CHIP_ATMEL: u8 = 2;
/// Atmel AT29LV512 programs in 128-byte pages.
const ATMEL_PAGE: usize = 128;
/// Non-Atmel save flash erases in 4 KB sectors.
const FLASH_SECTOR: usize = 0x1000;
/// Save flash larger than 64 KB is banked in 64 KB halves.
const FLASH_BANK: usize = 0x10000;
/// ROM-dump interval at which the ADDRESS variable is explicitly re-set
/// (see dump_agb_rom — the firmware's auto-increment fails at 16 MB).
const ADDR_REANCHOR: usize = 0x10000;

/// Whether a wide (64 Kbit-addressed) EEPROM read looks like it came from a
/// smaller part: every 512-byte block identical, or every byte the same.
fn eeprom_degenerate(data: &[u8]) -> bool {
    let first = &data[..EEPROM_4K_BYTES];
    data.chunks(EEPROM_4K_BYTES).all(|c| c == first)
        || data.iter().all(|&b| b == data[0])
}

impl<T: Transport> GbxCart<T> {
    /// Dump up to `rom_size` bytes of AGB ROM, stopping early when open bus
    /// appears at a power-of-two boundary >= 1 MB (same early-out as the
    /// Operator path, so main.rs's 32 MB-request + trim flow works unchanged;
    /// zero/FF-padded carts read through to the end and rely on trim_gba_rom).
    pub(super) fn dump_agb_rom(
        &mut self,
        rom_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        self.enter_agb_mode()?;
        self.set_variable(VAR_TRANSFER_SIZE, MAX_BUFFER_READ as u32)?;

        let mut rom = Vec::with_capacity(rom_size as usize);
        let mut buf = vec![0u8; MAX_BUFFER_READ as usize];
        let mut anchored = false;
        while rom.len() < rom_size as usize {
            // Re-anchor the address register at every 64 KB boundary instead
            // of trusting auto-increment across the whole ROM: hardware-
            // verified (v1.4a, L14) that continuous incrementing stalls the
            // stream around the 16 MB (word 0x800000) boundary, while
            // explicitly set addresses read fine everywhere.
            if rom.len().is_multiple_of(ADDR_REANCHOR) || !anchored {
                self.set_variable(VAR_ADDRESS, (rom.len() as u32) >> 1)?;
                anchored = true;
            }
            self.io.write_all(&[CMD_AGB_CART_READ])?;
            if let Err(e) = self.io.read_exact(&mut buf) {
                // A 32 MB dump is ~8000 chunks over several minutes; the
                // occasional serial hiccup shouldn't kill the run. Reads are
                // idempotent, so resync and re-request this chunk.
                if !self.recover_read(&mut buf, rom.len(), &e)? {
                    return Err(e);
                }
            }

            let len = rom.len();
            if len >= 1024 * 1024 && len.is_power_of_two() && is_open_bus(&buf[..256]) {
                return Ok(rom);
            }

            let want = (rom_size as usize - rom.len()).min(buf.len());
            rom.extend_from_slice(&buf[..want]);
            progress(rom.len() as u32);
        }
        Ok(rom)
    }

    /// Recover a failed ROM-chunk read: flush the stream, re-anchor ADDRESS
    /// at `offset`, and re-request. Returns Ok(true) with `buf` filled on
    /// success; Ok(false) if the error isn't a timeout (caller propagates).
    fn recover_read(
        &mut self,
        buf: &mut [u8],
        offset: usize,
        err: &DeviceError,
    ) -> Result<bool, DeviceError> {
        if !matches!(err, DeviceError::Io(e) if e.kind() == std::io::ErrorKind::TimedOut) {
            return Ok(false);
        }
        for _ in 0..3 {
            self.io.flush_input();
            self.set_variable(VAR_TRANSFER_SIZE, buf.len() as u32)?;
            self.set_variable(VAR_ADDRESS, (offset as u32) >> 1)?;
            self.io.write_all(&[CMD_AGB_CART_READ])?;
            if self.io.read_exact(buf).is_ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn read_agb_save(
        &mut self,
        chip: ChipType,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        self.enter_agb_mode()?;
        match chip {
            ChipType::Sram => self.read_agb_sram(save_size as usize, progress),
            ChipType::Eeprom => self.read_agb_eeprom(save_size as usize, progress),
            ChipType::Flash => self.read_agb_flash_save(save_size as usize, progress),
            ChipType::Unknown => Err(DeviceError::Unsupported(
                "GBA save type could not be determined",
            )),
        }
    }

    pub(super) fn write_agb_save(
        &mut self,
        chip: ChipType,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError> {
        self.enter_agb_mode()?;
        match chip {
            ChipType::Sram => self.write_agb_sram(data, progress),
            ChipType::Eeprom => self.write_agb_eeprom(data, progress),
            ChipType::Flash => self.write_agb_flash_save(data, progress),
            ChipType::Unknown => Err(DeviceError::Unsupported(
                "GBA save type could not be determined",
            )),
        }
    }

    // --- SRAM ----------------------------------------------------------------

    fn read_agb_sram(&mut self, len: usize, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        let chunk = (MAX_BUFFER_READ as usize).min(len);
        self.set_variable(VAR_TRANSFER_SIZE, chunk as u32)?;
        self.set_variable(VAR_ADDRESS, 0)?;
        let mut save = Vec::with_capacity(len);
        while save.len() < len {
            self.io.write_all(&[CMD_AGB_CART_READ_SRAM])?;
            let mut buf = vec![0u8; chunk.min(len - save.len())];
            // TRANSFER_SIZE-sized reads; the final short read only happens when
            // len isn't chunk-aligned (GBA SRAM sizes always are).
            self.io.read_exact(&mut buf)?;
            save.extend_from_slice(&buf);
            progress(save.len() as u32);
        }
        Ok(save)
    }

    fn write_agb_sram(&mut self, data: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        self.set_variable(VAR_ADDRESS, 0)?;
        self.set_variable(VAR_TRANSFER_SIZE, MAX_BUFFER_WRITE as u32)?;
        let mut written = 0usize;
        for chunk in data.chunks(MAX_BUFFER_WRITE as usize) {
            self.io.write_all(&[CMD_AGB_CART_WRITE_SRAM])?;
            self.io.write_all(chunk)?;
            self.expect_ack()?;
            written += chunk.len();
            progress(written as u32);
        }
        Ok(())
    }

    /// Single byte into SRAM address space (used for save-flash command
    /// registers at 0x5555/0x2AAA and bank switching).
    fn sram_byte_write(&mut self, addr: u32, value: u8) -> Result<(), DeviceError> {
        self.set_variable(VAR_ADDRESS, addr)?;
        self.set_variable(VAR_TRANSFER_SIZE, 1)?;
        self.io.write_all(&[CMD_AGB_CART_WRITE_SRAM])?;
        self.io.write_all(&[value])?;
        self.expect_ack()?;
        Ok(())
    }

    /// Single byte from SRAM address space (status polling, chip-ID reads).
    fn sram_byte_read(&mut self, addr: u32) -> Result<u8, DeviceError> {
        self.set_variable(VAR_ADDRESS, addr)?;
        self.set_variable(VAR_TRANSFER_SIZE, 1)?;
        self.io.write_all(&[CMD_AGB_CART_READ_SRAM])?;
        let mut b = [0u8];
        self.io.read_exact(&mut b)?;
        Ok(b[0])
    }

    // --- EEPROM --------------------------------------------------------------

    fn read_eeprom_raw(&mut self, size_id: u8, len: usize) -> Result<Vec<u8>, DeviceError> {
        let chunk = (MAX_BUFFER_READ as usize).min(len);
        self.set_variable(VAR_AGB_IRQ_ENABLED, 1)?; // needed on 32 MB carts
        self.set_variable(VAR_TRANSFER_SIZE, chunk as u32)?;
        self.set_variable(VAR_ADDRESS, 0)?;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            self.io.write_all(&[CMD_AGB_CART_READ_EEPROM, size_id])?;
            let mut buf = vec![0u8; chunk];
            self.io.read_exact(&mut buf)?;
            out.extend_from_slice(&buf);
        }
        self.set_variable(VAR_AGB_IRQ_ENABLED, 0)?;
        Ok(out)
    }

    /// EEPROM read with the Operator-parity size shim. main.rs always requests
    /// 8 KB and trims by mirror detection afterwards. A 4 Kbit part addressed
    /// with the 64 Kbit protocol can't answer real 8 KB — its output is
    /// degenerate (the ignored upper address bits wrap into 512-byte mirrors,
    /// or the interface desyncs into uniform bytes). So: read wide first; only
    /// when that looks degenerate, re-read narrow and tile the real 512 bytes
    /// to 8 KB, which `detect_eeprom_size` then trims exactly as it would an
    /// Operator read. (A genuinely-mirrored 64 Kbit save takes the narrow path
    /// too, but its content is identical after the trim — same as on the
    /// Operator.)
    fn read_agb_eeprom(&mut self, len: usize, _progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        if len <= EEPROM_4K_BYTES {
            return self.read_eeprom_raw(EEPROM_4K, EEPROM_4K_BYTES);
        }
        let large = self.read_eeprom_raw(EEPROM_64K, EEPROM_64K_BYTES)?;
        if !eeprom_degenerate(&large) {
            return Ok(large);
        }
        let small = self.read_eeprom_raw(EEPROM_4K, EEPROM_4K_BYTES)?;
        let mut tiled = Vec::with_capacity(EEPROM_64K_BYTES);
        while tiled.len() < EEPROM_64K_BYTES {
            tiled.extend_from_slice(&small);
        }
        Ok(tiled)
    }

    fn write_agb_eeprom(&mut self, data: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        let size_id = if data.len() <= EEPROM_4K_BYTES { EEPROM_4K } else { EEPROM_64K };
        // EEPROM programs in 8-byte pages with per-page delays; give the
        // firmware room before each chunk's ack.
        self.io.set_timeout(Duration::from_secs(10))?;
        self.set_variable(VAR_AGB_IRQ_ENABLED, 1)?;
        self.set_variable(VAR_ADDRESS, 0)?;
        self.set_variable(VAR_TRANSFER_SIZE, 0x100)?;
        let mut written = 0usize;
        let result = (|| {
            for chunk in data.chunks(0x100) {
                self.io.write_all(&[CMD_AGB_CART_WRITE_EEPROM, size_id])?;
                self.io.write_all(chunk)?;
                self.expect_ack()?;
                written += chunk.len();
                progress(written as u32);
            }
            Ok(())
        })();
        let _ = self.set_variable(VAR_AGB_IRQ_ENABLED, 0);
        self.io.set_timeout(Duration::from_secs(2))?;
        result
    }

    // --- Save flash (64 KB / 128 KB) ----------------------------------------

    /// Switch the active 64 KB bank on a 128 KB save-flash part.
    fn flash_save_bank(&mut self, bank: u8) -> Result<(), DeviceError> {
        self.sram_byte_write(0x5555, 0xAA)?;
        self.sram_byte_write(0x2AAA, 0x55)?;
        self.sram_byte_write(0x5555, 0xB0)?;
        self.sram_byte_write(0x0000, bank)?;
        Ok(())
    }

    /// Read the save-flash software ID (vendor, device). Atmel parts answer
    /// vendor 0x1F and need page-mode programming.
    fn flash_save_id(&mut self) -> Result<[u8; 2], DeviceError> {
        self.sram_byte_write(0x5555, 0xAA)?;
        self.sram_byte_write(0x2AAA, 0x55)?;
        self.sram_byte_write(0x5555, 0x90)?;
        let vendor = self.sram_byte_read(0x0000)?;
        let device = self.sram_byte_read(0x0001)?;
        // Exit software-ID mode.
        self.sram_byte_write(0x5555, 0xAA)?;
        self.sram_byte_write(0x2AAA, 0x55)?;
        self.sram_byte_write(0x5555, 0xF0)?;
        Ok([vendor, device])
    }

    fn read_agb_flash_save(&mut self, len: usize, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        let mut save = Vec::with_capacity(len);
        let banks = len.div_ceil(FLASH_BANK);
        for bank in 0..banks {
            if banks > 1 {
                self.flash_save_bank(bank as u8)?;
            }
            let want = FLASH_BANK.min(len - save.len());
            let chunk = (MAX_BUFFER_READ as usize).min(want);
            self.set_variable(VAR_TRANSFER_SIZE, chunk as u32)?;
            self.set_variable(VAR_ADDRESS, 0)?;
            let mut got = 0usize;
            while got < want {
                self.io.write_all(&[CMD_AGB_CART_READ_SRAM])?;
                let mut buf = vec![0u8; chunk];
                self.io.read_exact(&mut buf)?;
                got += buf.len();
                save.extend_from_slice(&buf);
                progress(save.len() as u32);
            }
        }
        if banks > 1 {
            self.flash_save_bank(0)?;
        }
        Ok(save)
    }

    /// Poll a save-flash address until it reads back `expected` (erase/program
    /// completion), bounded by `tries`.
    fn poll_flash_save(&mut self, addr: u32, expected: u8, tries: u32) -> Result<(), DeviceError> {
        for _ in 0..tries {
            if self.sram_byte_read(addr)? == expected {
                return Ok(());
            }
        }
        Err(DeviceError::Protocol("GBA save flash erase timed out".into()))
    }

    fn write_agb_flash_save(&mut self, data: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        let id = self.flash_save_id()?;
        let atmel = id[0] == 0x1F;
        let chip_type = if atmel { FLASH_CHIP_ATMEL } else { FLASH_CHIP_NORMAL };
        let (page, erase_needed) = if atmel {
            (ATMEL_PAGE, false) // Atmel page writes erase internally
        } else {
            (FLASH_SECTOR, true)
        };

        self.io.set_timeout(Duration::from_secs(10))?;
        let result = (|| {
            let mut written = 0usize;
            let banks = data.len().div_ceil(FLASH_BANK);
            for (bank, bank_data) in data.chunks(FLASH_BANK).enumerate() {
                if banks > 1 {
                    self.flash_save_bank(bank as u8)?;
                }
                for (i, sector) in bank_data.chunks(page).enumerate() {
                    let addr = (i * page) as u32;
                    if erase_needed {
                        // AMD-style 4 KB sector erase, polled to 0xFF.
                        self.sram_byte_write(0x5555, 0xAA)?;
                        self.sram_byte_write(0x2AAA, 0x55)?;
                        self.sram_byte_write(0x5555, 0x80)?;
                        self.sram_byte_write(0x5555, 0xAA)?;
                        self.sram_byte_write(0x2AAA, 0x55)?;
                        self.sram_byte_write(addr, 0x30)?;
                        self.poll_flash_save(addr, 0xFF, 10_000)?;
                    }
                    self.set_variable(VAR_ADDRESS, addr)?;
                    self.set_variable(VAR_TRANSFER_SIZE, page as u32)?;
                    self.io.write_all(&[CMD_AGB_CART_WRITE_FLASH_DATA, chip_type])?;
                    self.io.write_all(sector)?;
                    self.expect_ack()?;
                    written += sector.len();
                    progress(written as u32);
                }
            }
            if banks > 1 {
                self.flash_save_bank(0)?;
            }
            Ok(())
        })();
        self.io.set_timeout(Duration::from_secs(2))?;
        result
    }
}
