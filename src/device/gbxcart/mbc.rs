//! DMG (GB/GBC) mapper support: bank-switch plans and ROM/RAM flows.
//!
//! The Operator's firmware does all of this internally; on the GBxCart the host
//! issues the MBC register writes itself. The bank-switch *plans* are pure
//! functions (unit-tested byte-for-byte); the flows below apply them via single
//! bus writes and stream the windows.

use crate::device::DeviceError;

use super::protocol::*;
use super::{GbxCart, Transport};

/// DMG bank size constants.
const ROM_BANK: usize = 0x4000;
const RAM_BANK: usize = 0x2000;
/// MBC2's internal RAM: 512 half-bytes (we transfer 512 bytes, low nibbles valid).
const MBC2_RAM: usize = 0x200;
/// MBC7's accelerometer-cart EEPROM is accessed via dedicated opcodes in
/// 32-byte chunks.
const MBC7_CHUNK: usize = 32;

/// The DMG mappers this backend knows how to drive. Derived from cartridge
/// header byte 0x147 (same table as `CartridgeInfo::mbc_name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbcKind {
    /// 32 KB ROM, no banking (plus the ROM+RAM 0x08/0x09 oddities).
    None,
    Mbc1,
    /// MBC1 multicart wiring (1 MB collections); detected by the duplicate-logo
    /// probe, never by header byte alone.
    Mbc1M,
    Mbc2,
    Mbc3,
    /// MBC30: MBC3 wiring with 8-bit ROM banking / 64 KB RAM (Pocket Monsters
    /// Crystal). Same register layout as MBC3.
    Mbc30,
    Mbc5,
    Mbc7,
    Huc1,
    Huc3,
    /// MAC-GBD, the Game Boy Camera mapper: MBC3-style banking, 128 KB RAM.
    Camera,
    Unknown(u8),
}

impl MbcKind {
    pub fn from_header_byte(b: u8) -> Self {
        match b {
            0x00 | 0x08 | 0x09 => MbcKind::None,
            0x01..=0x03 => MbcKind::Mbc1,
            0x05 | 0x06 => MbcKind::Mbc2,
            0x0F..=0x13 => MbcKind::Mbc3,
            0x19..=0x1E => MbcKind::Mbc5,
            0x22 => MbcKind::Mbc7,
            0xFC => MbcKind::Camera,
            0xFE => MbcKind::Huc3,
            0xFF => MbcKind::Huc1,
            other => MbcKind::Unknown(other),
        }
    }
}

/// Refine an `Mbc3` into `Mbc30` when the cart's geometry needs the wider
/// banking (ROM > 2 MB or 64 KB RAM).
pub fn refine_mbc3(kind: MbcKind, rom_size: u32, ram_size: u32) -> MbcKind {
    if kind == MbcKind::Mbc3 && (rom_size > 2 * 1024 * 1024 || ram_size >= 64 * 1024) {
        MbcKind::Mbc30
    } else {
        kind
    }
}

/// Bus writes that map ROM `bank` into the 0x4000-0x7FFF window.
///
/// Bank registers are written at 0x2100 (inside the 0x2000-0x3FFF range; the
/// conventional dumper address that also satisfies clone mappers sensitive to
/// A8). MBC1 banks 0x20/0x40/0x60 are unreachable by hardware design (the
/// 5-bit register maps 0 to 1); real >512 KB MBC1 carts are multicarts handled
/// as `Mbc1M`.
pub fn rom_bank_writes(kind: MbcKind, bank: u32) -> Vec<(u16, u8)> {
    match kind {
        MbcKind::None | MbcKind::Unknown(_) => vec![],
        MbcKind::Mbc1 => vec![
            (0x2100, (bank & 0x1F) as u8),
            (0x4000, ((bank >> 5) & 0x03) as u8),
        ],
        MbcKind::Mbc1M => vec![
            (0x4000, ((bank >> 4) & 0x03) as u8),
            (0x2100, (bank & 0x0F) as u8),
        ],
        MbcKind::Mbc2 => vec![(0x2100, (bank & 0x0F) as u8)],
        MbcKind::Mbc3 => vec![(0x2100, (bank & 0x7F) as u8)],
        MbcKind::Mbc30 | MbcKind::Camera | MbcKind::Huc1 | MbcKind::Huc3 | MbcKind::Mbc7 => {
            vec![(0x2100, (bank & 0xFF) as u8)]
        }
        MbcKind::Mbc5 => vec![
            (0x2100, (bank & 0xFF) as u8),
            (0x3000, ((bank >> 8) & 0x01) as u8),
        ],
    }
}

/// Bus writes that map RAM `bank` into the 0xA000-0xBFFF window.
/// For MBC3-family carts only values 0-3 (7 for MBC30) are RAM — the RTC
/// registers live at 0x08+ and are never selected here.
pub fn ram_bank_writes(kind: MbcKind, bank: u32) -> Vec<(u16, u8)> {
    match kind {
        MbcKind::Mbc2 => vec![], // no RAM banking
        _ => vec![(0x4000, (bank & 0x0F) as u8)],
    }
}

/// Bus writes to enable external RAM access.
pub fn ram_enable_writes(kind: MbcKind) -> Vec<(u16, u8)> {
    match kind {
        // MBC1 additionally needs mode 1 for RAM banking to take effect.
        MbcKind::Mbc1 => vec![(0x0000, 0x0A), (0x6000, 0x01)],
        _ => vec![(0x0000, 0x0A)],
    }
}

/// Bus writes to disable external RAM access again (protects battery RAM from
/// bus noise; always issued after a save operation).
pub fn ram_disable_writes(kind: MbcKind) -> Vec<(u16, u8)> {
    match kind {
        MbcKind::Mbc1 => vec![(0x6000, 0x00), (0x0000, 0x00)],
        _ => vec![(0x0000, 0x00)],
    }
}

impl<T: Transport> GbxCart<T> {
    fn apply_writes(&mut self, writes: &[(u16, u8)]) -> Result<(), DeviceError> {
        for &(addr, value) in writes {
            self.dmg_write(addr, value)?;
        }
        Ok(())
    }

    /// Dump `rom_size` bytes of DMG ROM, driving the mapper bank by bank.
    pub(super) fn dump_dmg_rom(
        &mut self,
        mbc: MbcKind,
        rom_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        self.enter_dmg_mode()?;

        // 1 MB MBC1 carts are almost always multicarts with MBC1M wiring; the
        // duplicate-logo probe distinguishes them from true (rare) MBC1 carts.
        let mbc = if mbc == MbcKind::Mbc1 && rom_size >= 1024 * 1024 && self.probe_mbc1m()? {
            MbcKind::Mbc1M
        } else {
            mbc
        };

        let total_banks = (rom_size as usize / ROM_BANK).max(1);
        let mut rom = Vec::with_capacity(rom_size as usize);

        // MBC1 banking mode 0 so the 0x4000-window honours the upper-bank bits;
        // MBC1M mode 1 so the fixed window follows the outer bank (sub-bank 0
        // of each block is only reachable there).
        match mbc {
            MbcKind::Mbc1 => self.dmg_write(0x6000, 0x00)?,
            MbcKind::Mbc1M => self.dmg_write(0x6000, 0x01)?,
            _ => {}
        }

        // Bank 0 through the fixed window.
        rom.extend_from_slice(&self.read_dmg_rom_chunk(0, ROM_BANK.min(rom_size as usize))?);
        progress(rom.len() as u32);

        for bank in 1..total_banks as u32 {
            self.apply_writes(&rom_bank_writes(mbc, bank))?;
            let chunk = if mbc == MbcKind::Mbc1M && bank & 0x0F == 0 {
                // MBC1M maps sub-bank 0 of each outer block to the fixed
                // window, not the switchable one.
                self.read_dmg_rom_chunk(0, ROM_BANK)?
            } else {
                self.read_dmg_rom_chunk(0x4000, ROM_BANK)?
            };
            rom.extend_from_slice(&chunk);
            progress(rom.len() as u32);
        }

        Ok(rom)
    }

    /// MBC1M duplicate-logo probe: select bank 0x10 with plain-MBC1 registers
    /// and look for a second header logo — multicart wiring shows one, a true
    /// MBC1 cart shows game data.
    fn probe_mbc1m(&mut self) -> Result<bool, DeviceError> {
        self.apply_writes(&rom_bank_writes(MbcKind::Mbc1, 0x10))?;
        let window = self.read_dmg_rom_chunk(0x4000, 0x1000)?;
        self.apply_writes(&rom_bank_writes(MbcKind::Mbc1, 1))?;
        let header = &self.state.as_ref().expect("probe runs with cached state").header;
        Ok(window[0x104..0x134] == header[0x104..0x134])
    }

    /// Read `save_size` bytes of cartridge RAM (or MBC7 EEPROM).
    pub(super) fn read_dmg_save(
        &mut self,
        mbc: MbcKind,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        self.enter_dmg_mode()?;

        if mbc == MbcKind::Mbc7 {
            return self.read_mbc7_eeprom(save_size, progress);
        }

        self.apply_writes(&ram_enable_writes(mbc))?;
        let result = (|| {
            let mut save = Vec::with_capacity(save_size as usize);
            if mbc == MbcKind::Mbc2 {
                // 512 bytes of half-byte RAM, padded to the 2 KB the synthesized
                // signature reports (see gb_ram_size_with_mbc2_shim).
                self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_RAM_READ)?;
                self.set_variable(VAR_DMG_READ_CS_PULSE, 1)?;
                let mut data = self.read_at(CMD_DMG_CART_READ, 0xA000, MBC2_RAM as u16, MBC2_RAM)?;
                data.resize(save_size as usize, 0xFF);
                save = data;
                progress(save.len() as u32);
            } else {
                let banks = (save_size as usize).div_ceil(RAM_BANK);
                for bank in 0..banks as u32 {
                    self.apply_writes(&ram_bank_writes(mbc, bank))?;
                    self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_RAM_READ)?;
                    self.set_variable(VAR_DMG_READ_CS_PULSE, 1)?;
                    let want = RAM_BANK.min(save_size as usize - save.len());
                    let data = self.read_at(CMD_DMG_CART_READ, 0xA000, want.min(MAX_BUFFER_READ as usize) as u16, want)?;
                    save.extend_from_slice(&data);
                    progress(save.len() as u32);
                }
            }
            Ok(save)
        })();
        // Always drop RAM enable, even on error — leaving it asserted risks
        // battery-RAM corruption from bus noise.
        let disable = self.apply_writes(&ram_disable_writes(mbc));
        result.and_then(|save| disable.map(|()| save))
    }

    /// Write cartridge RAM (or MBC7 EEPROM).
    pub(super) fn write_dmg_save(
        &mut self,
        mbc: MbcKind,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError> {
        self.enter_dmg_mode()?;

        if mbc == MbcKind::Mbc7 {
            return self.write_mbc7_eeprom(data, progress);
        }

        self.apply_writes(&ram_enable_writes(mbc))?;
        let result = (|| {
            // MBC2: only the first 512 bytes are real RAM; the rest of the
            // 2 KB shim payload is padding.
            let data = if mbc == MbcKind::Mbc2 { &data[..MBC2_RAM.min(data.len())] } else { data };

            let mut written = 0usize;
            for (bank, bank_data) in data.chunks(RAM_BANK).enumerate() {
                if mbc != MbcKind::Mbc2 {
                    self.apply_writes(&ram_bank_writes(mbc, bank as u32))?;
                }
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
        let disable = self.apply_writes(&ram_disable_writes(mbc));
        result.and(disable)
    }

    /// MBC7 accelerometer-cart EEPROM read via the dedicated opcode
    /// (32-byte chunks). Hardware-unverified: no MBC7 cart on hand yet.
    fn read_mbc7_eeprom(&mut self, save_size: u32, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        self.set_variable(VAR_ADDRESS, 0)?;
        self.set_variable(VAR_TRANSFER_SIZE, MBC7_CHUNK as u32)?;
        let save = self.read_stream(CMD_DMG_MBC7_READ_EEPROM, MBC7_CHUNK, save_size as usize)?;
        progress(save.len() as u32);
        Ok(save)
    }

    /// MBC7 EEPROM write (32-byte chunks + ack). Hardware-unverified.
    fn write_mbc7_eeprom(&mut self, data: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        self.set_variable(VAR_ADDRESS, 0)?;
        self.set_variable(VAR_TRANSFER_SIZE, MBC7_CHUNK as u32)?;
        let mut written = 0usize;
        for chunk in data.chunks(MBC7_CHUNK) {
            self.io.write_all(&[CMD_DMG_MBC7_WRITE_EEPROM])?;
            self.io.write_all(chunk)?;
            self.expect_ack()?;
            written += chunk.len();
            progress(written as u32);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_byte_mapping() {
        assert_eq!(MbcKind::from_header_byte(0x00), MbcKind::None);
        assert_eq!(MbcKind::from_header_byte(0x03), MbcKind::Mbc1);
        assert_eq!(MbcKind::from_header_byte(0x06), MbcKind::Mbc2);
        assert_eq!(MbcKind::from_header_byte(0x10), MbcKind::Mbc3);
        assert_eq!(MbcKind::from_header_byte(0x1B), MbcKind::Mbc5);
        assert_eq!(MbcKind::from_header_byte(0x22), MbcKind::Mbc7);
        assert_eq!(MbcKind::from_header_byte(0xFC), MbcKind::Camera);
        assert_eq!(MbcKind::from_header_byte(0xFE), MbcKind::Huc3);
        assert_eq!(MbcKind::from_header_byte(0xFF), MbcKind::Huc1);
        assert_eq!(MbcKind::from_header_byte(0x42), MbcKind::Unknown(0x42));
    }

    #[test]
    fn mbc30_refinement() {
        assert_eq!(refine_mbc3(MbcKind::Mbc3, 4 * 1024 * 1024, 32 * 1024), MbcKind::Mbc30);
        assert_eq!(refine_mbc3(MbcKind::Mbc3, 2 * 1024 * 1024, 64 * 1024), MbcKind::Mbc30);
        assert_eq!(refine_mbc3(MbcKind::Mbc3, 2 * 1024 * 1024, 32 * 1024), MbcKind::Mbc3);
        assert_eq!(refine_mbc3(MbcKind::Mbc5, 8 * 1024 * 1024, 0), MbcKind::Mbc5);
    }

    #[test]
    fn mbc1_bank_plan() {
        assert_eq!(rom_bank_writes(MbcKind::Mbc1, 0x07), vec![(0x2100, 0x07), (0x4000, 0x00)]);
        // Bank 0x23 -> low 5 bits 0x03, upper bits 0x01.
        assert_eq!(rom_bank_writes(MbcKind::Mbc1, 0x23), vec![(0x2100, 0x03), (0x4000, 0x01)]);
    }

    #[test]
    fn mbc5_bank_plan_carries_ninth_bit() {
        assert_eq!(rom_bank_writes(MbcKind::Mbc5, 0x1FF), vec![(0x2100, 0xFF), (0x3000, 0x01)]);
        assert_eq!(rom_bank_writes(MbcKind::Mbc5, 0x0FF), vec![(0x2100, 0xFF), (0x3000, 0x00)]);
        // Bank 0 is directly mappable on MBC5 (unlike MBC1/3).
        assert_eq!(rom_bank_writes(MbcKind::Mbc5, 0), vec![(0x2100, 0x00), (0x3000, 0x00)]);
    }

    #[test]
    fn mbc2_bank_plan_masks_to_four_bits() {
        assert_eq!(rom_bank_writes(MbcKind::Mbc2, 0x0F), vec![(0x2100, 0x0F)]);
        assert_eq!(rom_bank_writes(MbcKind::Mbc2, 0x13), vec![(0x2100, 0x03)]);
    }

    #[test]
    fn mbc1m_bank_plan_splits_outer_and_inner() {
        assert_eq!(rom_bank_writes(MbcKind::Mbc1M, 0x17), vec![(0x4000, 0x01), (0x2100, 0x07)]);
    }

    #[test]
    fn ram_enable_disable_plans() {
        assert_eq!(ram_enable_writes(MbcKind::Mbc3), vec![(0x0000, 0x0A)]);
        assert_eq!(ram_disable_writes(MbcKind::Mbc3), vec![(0x0000, 0x00)]);
        // MBC1 needs banking mode 1 for RAM, dropped again on disable.
        assert_eq!(ram_enable_writes(MbcKind::Mbc1), vec![(0x0000, 0x0A), (0x6000, 0x01)]);
        assert_eq!(ram_disable_writes(MbcKind::Mbc1), vec![(0x6000, 0x00), (0x0000, 0x00)]);
    }

    #[test]
    fn ram_bank_plans() {
        assert_eq!(ram_bank_writes(MbcKind::Camera, 0x0C), vec![(0x4000, 0x0C)]);
        assert_eq!(ram_bank_writes(MbcKind::Mbc2, 0), vec![]);
    }
}
