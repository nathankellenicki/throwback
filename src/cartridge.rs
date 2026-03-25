use std::fmt;

use crate::device::ChipType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CartridgeType {
    GB,
    GBA,
}

impl fmt::Display for CartridgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CartridgeType::GB => write!(f, "GB/GBC"),
            CartridgeType::GBA => write!(f, "GBA"),
        }
    }
}

#[derive(Debug)]
pub struct CartridgeInfo {
    pub present: bool,
    pub cart_type: CartridgeType,
    pub rom_size: u32,
    pub ram_size: u32,
    // GB fields
    pub title_char: char,
    pub mbc_type: u8,
    pub rom_size_code: u8,
    pub ram_size_code: u8,
    pub header_checksum: u8,
    pub global_checksum: u16,
    // GBA fields
    pub game_code: [u8; 3],
    pub region: u8,
}

impl CartridgeInfo {
    pub fn from_bytes(data: &[u8; 64]) -> Self {
        let present = data[3] != 0 || data[4] != 0;
        let cart_type = if data[2] == 0x20 {
            CartridgeType::GB
        } else {
            CartridgeType::GBA
        };

        let title_char = data[0x0D] as char;
        let mbc_type = data[0x0E];
        let rom_size_code = data[0x0F];
        let ram_size_code = data[0x10];
        let header_checksum = data[0x11];
        let global_checksum = u16::from_le_bytes([data[0x12], data[0x13]]);

        let game_code = [data[0x0E], data[0x0F], data[0x10]];
        let region = data[0x11];

        // Compute sizes from header codes
        let (rom_size, ram_size) = match cart_type {
            CartridgeType::GB => {
                let rom = if rom_size_code <= 8 {
                    32 * 1024 * (1u32 << rom_size_code)
                } else {
                    0
                };
                let ram = match ram_size_code {
                    0x00 => 0,
                    0x01 => 2 * 1024,
                    0x02 => 8 * 1024,
                    0x03 => 32 * 1024,
                    0x04 => 128 * 1024,
                    0x05 => 64 * 1024,
                    _ => 0,
                };
                (rom, ram)
            }
            CartridgeType::GBA => {
                // GBA: device doesn't report sizes, they come from database or ROM scan
                (0, 0)
            }
        };

        Self {
            present,
            cart_type,
            rom_size,
            ram_size,
            title_char,
            mbc_type,
            rom_size_code,
            ram_size_code,
            header_checksum,
            global_checksum,
            game_code,
            region,
        }
    }

    pub fn game_id(&self) -> String {
        match self.cart_type {
            CartridgeType::GB => format!(
                "{}{:02X}{:04X}",
                self.title_char.to_uppercase(),
                self.header_checksum,
                self.global_checksum,
            ),
            CartridgeType::GBA => format!(
                "{}{}",
                self.title_char,
                String::from_utf8_lossy(&self.game_code),
            ),
        }
    }

    pub fn mbc_name(&self) -> &'static str {
        match self.mbc_type {
            0x00 => "ROM Only",
            0x01 => "MBC1",
            0x02 => "MBC1+RAM",
            0x03 => "MBC1+RAM+Battery",
            0x05 => "MBC2",
            0x06 => "MBC2+Battery",
            0x08 => "ROM+RAM",
            0x09 => "ROM+RAM+Battery",
            0x0B => "MMM01",
            0x0C => "MMM01+RAM",
            0x0D => "MMM01+RAM+Battery",
            0x0F => "MBC3+Timer+Battery",
            0x10 => "MBC3+Timer+RAM+Battery",
            0x11 => "MBC3",
            0x12 => "MBC3+RAM",
            0x13 => "MBC3+RAM+Battery",
            0x19 => "MBC5",
            0x1A => "MBC5+RAM",
            0x1B => "MBC5+RAM+Battery",
            0x1C => "MBC5+Rumble",
            0x1D => "MBC5+Rumble+RAM",
            0x1E => "MBC5+Rumble+RAM+Battery",
            0x20 => "MBC6",
            0x22 => "MBC7+Sensor+Rumble+RAM+Battery",
            0xFC => "Pocket Camera",
            0xFD => "Bandai TAMA5",
            0xFE => "HuC3",
            0xFF => "HuC1+RAM+Battery",
            _ => "Unknown",
        }
    }
}

impl fmt::Display for CartridgeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.present {
            return write!(f, "No cartridge inserted");
        }

        writeln!(f, "Type:            {}", self.cart_type)?;
        writeln!(f, "Game ID:         {}", self.game_id())?;

        match self.cart_type {
            CartridgeType::GB => {
                writeln!(f, "MBC:             {} (0x{:02X})", self.mbc_name(), self.mbc_type)?;
                writeln!(f, "ROM Size:        {}", format_size(self.rom_size))?;
                if self.ram_size > 0 {
                    writeln!(f, "RAM Size:        {}", format_size(self.ram_size))?;
                } else {
                    writeln!(f, "RAM Size:        None")?;
                }
                write!(f, "Header Checksum: 0x{:02X}", self.header_checksum)?;
            }
            CartridgeType::GBA => {
                writeln!(
                    f,
                    "Game Code:       {}",
                    String::from_utf8_lossy(&self.game_code)
                )?;
                write!(f, "Region:          0x{:02X}", self.region)?;
            }
        }

        Ok(())
    }
}

/// Detect the save type and size of a GBA ROM by scanning for save library strings.
pub fn detect_gba_save(rom: &[u8]) -> (ChipType, u32) {
    if find_bytes(rom, b"EEPROM_V").is_some() {
        (ChipType::Eeprom, 8 * 1024)
    } else if find_bytes(rom, b"FLASH1M_V").is_some() {
        (ChipType::Flash, 128 * 1024)
    } else if find_bytes(rom, b"FLASH512_V").is_some() {
        (ChipType::Flash, 64 * 1024)
    } else if find_bytes(rom, b"FLASH_V").is_some() {
        (ChipType::Flash, 64 * 1024)
    } else if find_bytes(rom, b"SRAM_V").is_some() || find_bytes(rom, b"SRAM_F_V").is_some() {
        (ChipType::Sram, 32 * 1024)
    } else {
        (ChipType::Unknown, 0)
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Detect the real ROM size of a GBA dump.
/// GBA ROMs are always power-of-two sized (1/2/4/8/16/32 MB).
/// Past the real ROM, the GBA bus returns open-bus values (incrementing u16)
/// or 0xFF padding. We check each power-of-two boundary to find where
/// real ROM data ends.
pub fn trim_gba_rom(rom: &[u8]) -> usize {
    let sizes: &[usize] = &[
        1 * 1024 * 1024,
        2 * 1024 * 1024,
        4 * 1024 * 1024,
        8 * 1024 * 1024,
        16 * 1024 * 1024,
        32 * 1024 * 1024,
    ];

    for &size in sizes {
        if size > rom.len() {
            continue;
        }
        // Check if data just past this boundary looks like open bus or padding.
        // Open bus: incrementing 16-bit values matching the address / 2.
        // Padding: all 0xFF.
        if size < rom.len() {
            let check_offset = size;
            let check_len = 32.min(rom.len() - check_offset);
            let region = &rom[check_offset..check_offset + check_len];

            // Only check for open bus pattern (00 00 01 00 02 00 03 00...).
            // 0xFF padding is NOT a reliable indicator — some ROMs have 0xFF
            // gaps between data sections (e.g. Pokemon Ruby has data at 14.7MB
            // but 0xFF at 8MB).
            let mut is_open_bus = check_len >= 4;
            for j in (0..check_len).step_by(2) {
                if j + 1 >= check_len { break; }
                let expected = (j / 2) as u16;
                let actual = u16::from_le_bytes([region[j], region[j + 1]]);
                if actual != expected {
                    is_open_bus = false;
                    break;
                }
            }

            if is_open_bus {
                return size;
            }
        }

        if size == rom.len() {
            return size;
        }
    }

    rom.len()
}

pub fn format_size(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{} B", bytes)
    }
}
