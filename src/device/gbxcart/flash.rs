//! Flash-cart ROM writing: chip identification, erase, program, verify.
//!
//! The Operator's firmware recognizes and drives flashcarts itself; here the
//! host does it, driven by a curated table of flash-chip profiles matched by
//! JEDEC software ID. Unknown chips are refused cleanly — `write_rom` never
//! sends erase/program sequences to a cart it can't identify, even when the
//! CLI's `--force` bypasses the detect gate.
//!
//! The profile table is a bring-up seed: IDs marked "verify" are from public
//! JEDEC data and must be confirmed against real carts (the probe example
//! prints raw IDs; unknown carts get a one-line profile addition).

use std::time::{Duration, Instant};

use crc::{Crc, CRC_32_ISO_HDLC};

use crate::device::DeviceError;

use super::mbc::{rom_bank_writes, MbcKind};
use super::protocol::*;
use super::{CartFamily, GbxCart, Transport};

/// Flash command-set families, with their firmware SET_FLASH_CMD encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdSet {
    Amd = 1,
    Intel = 2,
}

/// Programming method, with firmware SET_FLASH_CMD encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Unbuffered,
    /// Buffered writes with the chip's buffer size in bytes.
    Buffered(u16),
}

/// Which pin strobes flash writes on DMG carts (AGB carts always use WR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WePin {
    Wr = 1,
    Audio = 2,
}

pub struct FlashProfile {
    pub name: &'static str,
    pub family: CartFamily,
    /// Raw ID byte prefixes as read from the bus after autoselect; a cart
    /// matches when its ID starts with any of these.
    pub flash_ids: &'static [&'static [u8]],
    pub cmd_set: CmdSet,
    pub method: Method,
    pub we_pin: WePin,
    /// Unlock cycle addresses (bus addresses on DMG, word addresses on AGB).
    pub unlock: (u32, u32),
    pub max_size: u32,
    /// 0 = chip-erase only.
    pub sector_size: u32,
    pub chip_erase_ms: u64,
}

const KB: u32 = 1024;
const MB: u32 = 1024 * 1024;

pub static FLASH_PROFILES: &[FlashProfile] = &[
    // --- DMG (x8 bus) --------------------------------------------------------
    FlashProfile {
        name: "AM29F016 (2 MB)", // verify on hardware
        family: CartFamily::Dmg,
        flash_ids: &[&[0x01, 0xAD]],
        cmd_set: CmdSet::Amd,
        method: Method::Unbuffered,
        we_pin: WePin::Wr,
        unlock: (0x555, 0x2AA),
        max_size: 2 * MB,
        sector_size: 64 * KB,
        chip_erase_ms: 60_000,
    },
    FlashProfile {
        name: "AM29F032/AM29F033 (4 MB)", // verify on hardware
        family: CartFamily::Dmg,
        flash_ids: &[&[0x01, 0x41], &[0x01, 0x45]],
        cmd_set: CmdSet::Amd,
        method: Method::Unbuffered,
        we_pin: WePin::Wr,
        unlock: (0x555, 0x2AA),
        max_size: 4 * MB,
        sector_size: 64 * KB,
        chip_erase_ms: 90_000,
    },
    FlashProfile {
        name: "SST39SF010/020/040 (128 KB-512 KB, insideGadgets)", // verify on hardware
        family: CartFamily::Dmg,
        flash_ids: &[&[0xBF, 0xB5], &[0xBF, 0xB6], &[0xBF, 0xB7]],
        cmd_set: CmdSet::Amd,
        method: Method::Unbuffered,
        we_pin: WePin::Wr,
        unlock: (0x5555, 0x2AAA),
        max_size: 512 * KB,
        sector_size: 4 * KB,
        chip_erase_ms: 15_000,
    },
    FlashProfile {
        name: "MX29LV320 (4 MB, insideGadgets)", // verify on hardware
        family: CartFamily::Dmg,
        flash_ids: &[&[0xC2, 0xA7], &[0xC2, 0xA8]],
        cmd_set: CmdSet::Amd,
        method: Method::Unbuffered,
        we_pin: WePin::Wr,
        unlock: (0xAAA, 0x555),
        max_size: 4 * MB,
        sector_size: 64 * KB,
        chip_erase_ms: 60_000,
    },
    // --- AGB (x16 bus; IDs as raw little-endian byte streams) ---------------
    FlashProfile {
        name: "S29GL/MSP55LV family (16-32 MB AGB)", // verify on hardware
        family: CartFamily::Agb,
        flash_ids: &[&[0x01, 0x00, 0x7E, 0x22]],
        cmd_set: CmdSet::Amd,
        method: Method::Buffered(0x400),
        we_pin: WePin::Wr,
        unlock: (0x555, 0x2AA), // word addresses (byte 0xAAA / 0x555)
        max_size: 32 * MB,
        sector_size: 128 * KB,
        chip_erase_ms: 240_000,
    },
    FlashProfile {
        name: "Intel 28F family (AGB)", // verify on hardware; Intel path untested
        family: CartFamily::Agb,
        flash_ids: &[&[0x89, 0x00]],
        cmd_set: CmdSet::Intel,
        method: Method::Unbuffered,
        we_pin: WePin::Wr,
        unlock: (0, 0),
        max_size: 32 * MB,
        sector_size: 128 * KB,
        chip_erase_ms: 240_000,
    },
];

/// Match a raw autoselect ID against the profile table for one cart family.
pub fn match_profile(family: CartFamily, id: &[u8]) -> Option<&'static FlashProfile> {
    FLASH_PROFILES
        .iter()
        .filter(|p| p.family == family)
        .find(|p| p.flash_ids.iter().any(|want| id.starts_with(want)))
}

/// Bytes of ID captured by the autoselect probe.
const ID_LEN: usize = 8;
/// DMG unlock-address variants tried by the probe (the third is the SST
/// 0x5555-style addressing).
const DMG_UNLOCK_VARIANTS: [(u32, u32); 3] = [(0x555, 0x2AA), (0xAAA, 0x555), (0x5555, 0x2AAA)];
const AGB_UNLOCK_VARIANTS: [(u32, u32); 1] = [(0x555, 0x2AA)];

/// The `flashcart` byte of CART_WRITE_FLASH_CMD: command writes target the
/// flash cart bus with the configured WE pin.
const FLASHCART: u8 = 1;

/// Serial chunk size for FLASH_PROGRAM streaming (the TRANSFER_SIZE value).
const PROGRAM_CHUNK: usize = 0x100;

const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

impl<T: Transport> GbxCart<T> {
    /// Batch of flash command-register writes on the cart bus.
    fn flash_cmd(&mut self, cmds: &[(u32, u16)]) -> Result<(), DeviceError> {
        self.cmd_ack(&write_flash_cmd_frame(FLASHCART, cmds))
    }

    fn read_id_bytes(&mut self, family: CartFamily) -> Result<Vec<u8>, DeviceError> {
        match family {
            CartFamily::Dmg => self.read_dmg_rom_chunk(0, ID_LEN),
            CartFamily::Agb => self.read_at(CMD_AGB_CART_READ, 0, ID_LEN as u16, ID_LEN),
        }
    }

    /// Probe the inserted cart's flash software ID. Returns the raw ID bytes
    /// when a flash chip answered (ID differs from the ROM bytes), else None
    /// (retail mask ROM). Read-mostly: the autoselect/reset command writes are
    /// ignored by mask ROMs.
    pub(super) fn probe_flash_id(&mut self) -> Result<Option<Vec<u8>>, DeviceError> {
        let family = self.state.as_ref().expect("probe runs with cached state").family;
        match family {
            CartFamily::Dmg => self.enter_dmg_mode()?,
            CartFamily::Agb => self.enter_agb_mode()?,
        }
        let variants: &[(u32, u32)] = match family {
            CartFamily::Dmg => &DMG_UNLOCK_VARIANTS,
            CartFamily::Agb => &AGB_UNLOCK_VARIANTS,
        };
        let snapshot = self.read_id_bytes(family)?;
        for &we in &[WePin::Wr, WePin::Audio] {
            if family == CartFamily::Agb && we == WePin::Audio {
                continue; // AGB carts strobe WR only
            }
            self.set_variable(VAR_FLASH_WE_PIN, we as u32)?;
            for &(a1, a2) in variants {
                self.flash_cmd(&[(a1, 0xF0)])?; // reset
                self.flash_cmd(&[(a1, 0xAA), (a2, 0x55), (a1, 0x90)])?; // autoselect
                let id = self.read_id_bytes(family)?;
                self.flash_cmd(&[(a1, 0xF0)])?; // back to read mode
                if id != snapshot {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    /// Probe (or reuse) the flash profile for the inserted cart and synthesize
    /// the Operator-shaped DetectFlashcart result packet: byte 0 = family
    /// marker | 0x01 when writeable, raw ID bytes at 1.. for debuggability.
    pub(super) fn flashcart_probe_packet(&mut self) -> Result<[u8; 64], DeviceError> {
        let family = self.state.as_ref().expect("state cached").family;
        let marker: u8 = match family {
            CartFamily::Dmg => 0x20,
            CartFamily::Agb => 0x30,
        };

        let id = self.probe_flash_id()?;
        let profile = id.as_deref().and_then(|id| match_profile(family, id));
        if let Some(state) = self.state.as_mut() {
            state.flash = Some(profile);
        }

        let mut packet = [0u8; 64];
        match (&id, profile) {
            (Some(id), Some(_)) => {
                packet[0] = marker | 0x01;
                packet[1..1 + id.len().min(8)].copy_from_slice(&id[..id.len().min(8)]);
            }
            _ => packet[0] = marker,
        }
        Ok(packet)
    }

    /// Convert an absolute DMG flash address to (MBC5 bank, bus address).
    /// Shared with the GB Memory writer (`gbmemory.rs`).
    pub(super) fn dmg_window(abs: u32) -> (u32, u32) {
        let bank = abs / 0x4000;
        if bank == 0 { (0, abs) } else { (bank, 0x4000 + abs % 0x4000) }
    }

    pub(super) fn select_dmg_bank(&mut self, bank: u32) -> Result<(), DeviceError> {
        for (addr, value) in rom_bank_writes(MbcKind::Mbc5, bank) {
            self.dmg_write(addr, value)?;
        }
        Ok(())
    }

    /// Read one byte/word at an absolute flash address (for erase polling).
    fn poll_read(&mut self, family: CartFamily, abs: u32) -> Result<u8, DeviceError> {
        match family {
            CartFamily::Dmg => {
                let (bank, bus) = Self::dmg_window(abs);
                if bank > 0 {
                    self.select_dmg_bank(bank)?;
                }
                Ok(self.read_dmg_rom_chunk(bus, 1)?[0])
            }
            CartFamily::Agb => Ok(self.read_at(CMD_AGB_CART_READ, abs >> 1, 2, 2)?[0]),
        }
    }

    /// Erase enough flash to hold `len` bytes, reporting progress.
    fn erase_flash(
        &mut self,
        profile: &FlashProfile,
        len: u32,
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError> {
        let family = profile.family;
        let (a1, a2) = profile.unlock;
        self.io.set_timeout(Duration::from_secs(10))?;

        let poll_until_ff = |dev: &mut Self, abs: u32, deadline_ms: u64, erase_progress: &dyn Fn(&str)| {
            let start = Instant::now();
            let mut last_msg = Instant::now();
            loop {
                if dev.poll_read(family, abs)? == 0xFF {
                    return Ok(());
                }
                if last_msg.elapsed() > Duration::from_millis(500) {
                    erase_progress("Erasing...");
                    last_msg = Instant::now();
                }
                if start.elapsed() > Duration::from_millis(deadline_ms) {
                    return Err(DeviceError::Protocol(format!(
                        "flash erase timed out after {deadline_ms} ms ({})",
                        profile.name
                    )));
                }
            }
        };

        match profile.cmd_set {
            CmdSet::Amd if profile.sector_size > 0 => {
                let mut abs = 0u32;
                while abs < len {
                    if family == CartFamily::Dmg {
                        let (bank, bus) = Self::dmg_window(abs);
                        self.select_dmg_bank(bank)?;
                        self.flash_cmd(&[(a1, 0xAA), (a2, 0x55), (a1, 0x80), (a1, 0xAA), (a2, 0x55), (bus, 0x30)])?;
                    } else {
                        let word = abs >> 1;
                        self.flash_cmd(&[(a1, 0xAA), (a2, 0x55), (a1, 0x80), (a1, 0xAA), (a2, 0x55), (word, 0x30)])?;
                    }
                    poll_until_ff(self, abs, 30_000, erase_progress)?;
                    abs += profile.sector_size;
                }
            }
            CmdSet::Amd => {
                self.flash_cmd(&[(a1, 0xAA), (a2, 0x55), (a1, 0x80), (a1, 0xAA), (a2, 0x55), (a1, 0x10)])?;
                poll_until_ff(self, 0, profile.chip_erase_ms, erase_progress)?;
            }
            CmdSet::Intel => {
                // Intel block erase: unlock + erase-confirm per block, polled
                // via read of the block base. Hardware-unverified path.
                let mut abs = 0u32;
                while abs < len {
                    let addr = if family == CartFamily::Agb { abs >> 1 } else { abs };
                    self.flash_cmd(&[(addr, 0x60), (addr, 0xD0)])?; // unlock block
                    self.flash_cmd(&[(addr, 0x20), (addr, 0xD0)])?; // erase block
                    poll_until_ff(self, abs, 30_000, erase_progress)?;
                    self.flash_cmd(&[(addr, 0xFF)])?; // back to read array
                    abs += profile.sector_size;
                }
            }
        }
        self.io.set_timeout(Duration::from_secs(2))?;
        Ok(())
    }

    /// Configure the firmware's flash programming engine for `profile`.
    fn configure_program(&mut self, profile: &FlashProfile) -> Result<(), DeviceError> {
        let (a1, a2) = profile.unlock;
        let (method, buffer): (u8, u32) = match profile.method {
            Method::Unbuffered => (1, 0),
            Method::Buffered(b) => (2, b as u32),
        };
        let cmds: &[(u32, u16)] = match profile.cmd_set {
            // AMD byte/word program unlock prefix; the firmware appends the
            // program address/data cycles itself.
            CmdSet::Amd => &[(a1, 0xAA), (a2, 0x55), (a1, 0xA0)],
            // Intel word program command.
            CmdSet::Intel => &[(0, 0x40)],
        };
        self.cmd_ack(&set_flash_cmd_frame(
            profile.cmd_set as u8,
            method,
            profile.we_pin as u32 as u8,
            cmds,
        ))?;
        if buffer > 0 {
            self.set_variable(VAR_BUFFER_SIZE, buffer)?;
        }
        Ok(())
    }

    /// Program one buffer group. Serial chunks are PROGRAM_CHUNK-sized (the
    /// TRANSFER_SIZE variable, set once before the loop — never mid-stream,
    /// since an engaged firmware expects raw data, not commands); the firmware
    /// acks 0x03 while a buffered-write group is partially filled (more raw
    /// data expected, no opcode) and 0x01 at a group boundary (next chunk is
    /// opcode-led). Groups are the FF-skip granularity, so a skip always lands
    /// on a boundary where the firmware expects an opcode.
    fn program_group(&mut self, engaged: &mut bool, group: &[u8]) -> Result<(), DeviceError> {
        for chunk in group.chunks(PROGRAM_CHUNK) {
            if !*engaged {
                self.io.write_all(&[CMD_FLASH_PROGRAM])?;
            }
            self.io.write_all(chunk)?;
            *engaged = self.expect_ack()?;
        }
        Ok(())
    }

    /// Write `data` to the flashcart: identify, erase, program, verify.
    pub(super) fn flash_rom(
        &mut self,
        data: &[u8],
        progress: &dyn Fn(u32),
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError> {
        let family = self.require_state()?.family;

        erase_progress("Preparing cartridge...");
        let profile = match self.state.as_ref().and_then(|s| s.flash) {
            Some(cached) => cached,
            None => {
                let id = self.probe_flash_id()?;
                let p = id.as_deref().and_then(|id| match_profile(family, id));
                if let Some(state) = self.state.as_mut() {
                    state.flash = Some(p);
                }
                p
            }
        };
        let Some(profile) = profile else {
            return Err(DeviceError::NotFlashable(
                "this cartridge's flash chip was not recognized (retail cart, or a chip \
                 not yet in the supported profile list)"
                    .into(),
            ));
        };
        if data.len() as u32 > profile.max_size {
            return Err(DeviceError::NotFlashable(format!(
                "ROM is larger than this flashcart ({} > {} bytes, {})",
                data.len(),
                profile.max_size,
                profile.name
            )));
        }

        erase_progress("Erasing flash...");
        self.set_variable(VAR_FLASH_WE_PIN, profile.we_pin as u32)?;
        self.erase_flash(profile, data.len() as u32, erase_progress)?;

        // FF-skip granularity: a whole buffer group for buffered chips (the
        // firmware acks 0x03 mid-group and expects raw data, so commands can
        // only be interleaved at group boundaries), one chunk otherwise.
        let group_size = match profile.method {
            Method::Unbuffered => PROGRAM_CHUNK,
            Method::Buffered(b) => (b as usize).max(PROGRAM_CHUNK),
        };

        // Pad to a whole group with 0xFF (programming 0xFF into erased flash
        // is a no-op), so TRANSFER_SIZE never changes mid-stream and buffered
        // groups are always completely filled. The pad never extends past the
        // erased range: sector sizes are group multiples, so an exact-sector
        // ROM needs no padding at all.
        let padded_storage;
        let data: &[u8] = if data.len().is_multiple_of(group_size) {
            data
        } else {
            let mut v = data.to_vec();
            v.resize(data.len().next_multiple_of(group_size), 0xFF);
            padded_storage = v;
            &padded_storage
        };

        erase_progress("Writing...");
        self.configure_program(profile)?;
        self.io.set_timeout(Duration::from_secs(10))?;
        self.set_variable(VAR_TRANSFER_SIZE, PROGRAM_CHUNK as u32)?;

        let mut written = 0usize;
        match family {
            CartFamily::Dmg => {
                for (bank, bank_data) in data.chunks(0x4000).enumerate() {
                    let (_, window) = Self::dmg_window((bank * 0x4000) as u32);
                    self.select_dmg_bank(bank as u32)?;
                    let mut engaged = false;
                    let mut offset = 0usize;
                    let mut synced = false;
                    for group in bank_data.chunks(group_size) {
                        if group.iter().all(|&b| b == 0xFF) {
                            synced = false;
                        } else {
                            if !synced {
                                self.set_variable(VAR_ADDRESS, window + offset as u32)?;
                                synced = true;
                            }
                            self.program_group(&mut engaged, group)?;
                        }
                        offset += group.len();
                        written += group.len();
                        progress(written as u32);
                    }
                }
            }
            CartFamily::Agb => {
                let mut engaged = false;
                let mut offset = 0usize;
                let mut synced = false;
                for group in data.chunks(group_size) {
                    if group.iter().all(|&b| b == 0xFF) {
                        synced = false;
                    } else {
                        if !synced {
                            self.set_variable(VAR_ADDRESS, (offset as u32) >> 1)?;
                            synced = true;
                        }
                        self.program_group(&mut engaged, group)?;
                    }
                    offset += group.len();
                    written += group.len();
                    progress(written as u32);
                }
            }
        }
        self.io.set_timeout(Duration::from_secs(2))?;

        // Verify. AGB: firmware-side CRC32 over the linear ROM space. DMG:
        // the firmware CRC can't follow MBC banking, so read the whole image
        // back and compare — slower but airtight.
        erase_progress("Verifying...");
        match family {
            CartFamily::Agb => {
                self.set_variable(VAR_ADDRESS, 0)?;
                self.io.write_all(&calc_crc32_frame(data.len() as u32))?;
                let mut crc = [0u8; 4];
                self.io.set_timeout(Duration::from_secs(60))?;
                self.io.read_exact(&mut crc)?;
                self.io.set_timeout(Duration::from_secs(2))?;
                let device_crc = u32::from_be_bytes(crc);
                let host_crc = CRC32.checksum(data);
                if device_crc != host_crc {
                    return Err(DeviceError::Protocol(format!(
                        "post-write verification failed (device CRC {device_crc:08X} != {host_crc:08X})"
                    )));
                }
            }
            CartFamily::Dmg => {
                let readback = self.dump_dmg_rom(MbcKind::Mbc5, data.len() as u32, &|_| {})?;
                if readback != data {
                    return Err(DeviceError::Protocol(
                        "post-write verification failed (read-back differs)".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_matching_by_prefix() {
        let p = match_profile(CartFamily::Dmg, &[0x01, 0xAD, 0x01, 0xAD, 0, 0, 0, 0]).unwrap();
        assert_eq!(p.name, "AM29F016 (2 MB)");
        let p = match_profile(CartFamily::Dmg, &[0xBF, 0xB7, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(p.name.starts_with("SST39SF"));
        assert_eq!(p.unlock, (0x5555, 0x2AAA));
    }

    #[test]
    fn profile_matching_respects_family() {
        // The S29GL ID must not match in DMG family and vice versa.
        assert!(match_profile(CartFamily::Dmg, &[0x01, 0x00, 0x7E, 0x22]).is_none());
        assert!(match_profile(CartFamily::Agb, &[0x01, 0xAD]).is_none());
    }

    #[test]
    fn unknown_id_matches_nothing() {
        assert!(match_profile(CartFamily::Dmg, &[0xDE, 0xAD, 0xBE, 0xEF]).is_none());
        assert!(match_profile(CartFamily::Dmg, &[]).is_none());
    }

    #[test]
    fn dmg_window_math() {
        assert_eq!(GbxCart::<super::super::transport::SerialTransport>::dmg_window(0x0123), (0, 0x0123));
        assert_eq!(GbxCart::<super::super::transport::SerialTransport>::dmg_window(0x4000), (1, 0x4000));
        assert_eq!(GbxCart::<super::super::transport::SerialTransport>::dmg_window(0x2_8000), (10, 0x4000));
        assert_eq!(GbxCart::<super::super::transport::SerialTransport>::dmg_window(0x2_9234), (10, 0x5234));
    }
}
