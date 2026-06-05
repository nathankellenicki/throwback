use std::fmt;

use crate::device::ChipType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CartridgeType {
    GB,
    GBA,
    /// SN Operator signature byte[2] == 0x50. The header (title/mapper/save) is
    /// confirmed against the dumped ROM via parse_snes_header.
    SNES,
}

/// Signature byte[2] cartridge-family markers reported by the Operator firmware.
const SIG_MARKER_GB: u8 = 0x20;
const SIG_MARKER_GBA: u8 = 0x30;
const SIG_MARKER_SNES: u8 = 0x50;
/// Offset in the SN Operator signature of the SNES ROM-size code (verified against
/// a Desert Strike cartridge: signature[0x10] == header rom_size code 0x0A == 1 MB).
const SIG_SNES_ROM_CODE: usize = 0x10;

impl fmt::Display for CartridgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CartridgeType::GB => write!(f, "GB/GBC"),
            CartridgeType::GBA => write!(f, "GBA"),
            CartridgeType::SNES => write!(f, "SNES"),
        }
    }
}

#[derive(Debug)]
pub struct CartridgeInfo {
    pub present: bool,
    pub cart_type: CartridgeType,
    pub rom_size: u32,
    pub ram_size: u32,
    /// Full game title, read from the cartridge's internal header (not the signature).
    /// `None` until populated by a header read.
    pub title: Option<String>,
    /// Overrides the displayed `Type:` line when a header read refines the family
    /// (e.g. GB → GBC via the CGB flag). `None` falls back to `cart_type`.
    pub type_label: Option<String>,
    /// Decoded region/destination, read from the cartridge's internal header.
    /// `None` until populated by a header read (GB destination / GBA game-code letter).
    pub region_label: Option<String>,
    /// Whether the cartridge's stored header checksum matches one computed from the
    /// header bytes. `None` until a header read lets us verify it (GB/GBA).
    pub checksum_valid: Option<bool>,
    /// Mask ROM version (GB 0x14C / GBA 0xBC). `None` until a header read.
    pub version: Option<u8>,
    // GB fields
    pub title_char: char,
    pub mbc_type: u8,
    pub header_checksum: u8,
    pub global_checksum: u16,
    // GBA fields
    pub game_code: [u8; 3],
    pub region: u8,
}

impl CartridgeInfo {
    pub fn from_bytes(data: &[u8; 64]) -> Self {
        let present = data[3] != 0 || data[4] != 0;
        let cart_type = match data[2] {
            SIG_MARKER_GB => CartridgeType::GB,
            SIG_MARKER_SNES => CartridgeType::SNES,
            // GBA reports 0x30; treat any other present marker as GBA.
            SIG_MARKER_GBA | _ => CartridgeType::GBA,
        };

        let title_char = data[0x0D] as char;
        let mbc_type = data[0x0E];
        let header_checksum = data[0x11];
        let global_checksum = u16::from_le_bytes([data[0x12], data[0x13]]);

        let game_code = [data[0x0E], data[0x0F], data[0x10]];
        let region = data[0x11];

        // Compute sizes from header codes
        let rom_size_code = data[0x0F];
        let ram_size_code = data[0x10];
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
            CartridgeType::SNES => {
                // The SN Operator relays the SNES header's ROM-size code in the
                // signature; size = 0x400 << code (e.g. 0x0A → 1 MB). We need this
                // up front because ReadGame masks to the requested size. Save (SRAM)
                // size isn't decoded from the signature yet, so it's derived from the
                // dumped header by parse_snes_header instead.
                let code = data[SIG_SNES_ROM_CODE];
                let rom = if (0x08..=0x10).contains(&code) {
                    0x400u32 << code
                } else {
                    0
                };
                (rom, 0)
            }
        };

        Self {
            present,
            cart_type,
            rom_size,
            ram_size,
            title: None,
            type_label: None,
            region_label: None,
            checksum_valid: None,
            version: None,
            title_char,
            mbc_type,
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
            CartridgeType::SNES => String::new(),
        }
    }

    /// Whether the GB cartridge has a real-time clock. Only MBC3 variants with the
    /// Timer carry one: 0x0F (Timer+Battery) and 0x10 (Timer+RAM+Battery).
    pub fn has_rtc(&self) -> bool {
        matches!(self.mbc_type, 0x0F | 0x10)
    }

    /// Format the header checksum with a validity verdict when we've been able to
    /// recompute it from the header bytes (GB/GBA).
    fn checksum_line(&self) -> String {
        match self.checksum_valid {
            Some(true) => format!("0x{:02X} (valid)", self.header_checksum),
            Some(false) => format!("0x{:02X} (INVALID)", self.header_checksum),
            None => format!("0x{:02X}", self.header_checksum),
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

        match &self.type_label {
            Some(label) => writeln!(f, "Type:            {label}")?,
            None => writeln!(f, "Type:            {}", self.cart_type)?,
        }
        if let Some(title) = &self.title {
            writeln!(f, "Title:           {}", title)?;
        }
        // GB has a synthesized Game ID; GBA shows the real 4-char Game Code below
        // instead (printing both was redundant); SNES has neither in the signature.
        if self.cart_type == CartridgeType::GB {
            writeln!(f, "Game ID:         {}", self.game_id())?;
        }

        match self.cart_type {
            CartridgeType::GB => {
                writeln!(f, "MBC:             {} (0x{:02X})", self.mbc_name(), self.mbc_type)?;
                writeln!(f, "ROM Size:        {}", format_size(self.rom_size))?;
                if self.ram_size > 0 {
                    writeln!(f, "Save:            {}", format_size(self.ram_size))?;
                } else {
                    writeln!(f, "Save:            None")?;
                }
                if self.has_rtc() {
                    writeln!(f, "RTC:             Present")?;
                }
                if let Some(region) = &self.region_label {
                    writeln!(f, "Region:          {region}")?;
                }
                if let Some(version) = self.version {
                    writeln!(f, "Version:         {version}")?;
                }
                write!(f, "Header Checksum: {}", self.checksum_line())?;
            }
            CartridgeType::GBA => {
                // Full 4-char game code (AGB-XXXY): the signature's 3-byte code plus
                // the destination letter held in `region`.
                writeln!(
                    f,
                    "Game Code:       {}{}",
                    String::from_utf8_lossy(&self.game_code),
                    self.region as char,
                )?;
                match &self.region_label {
                    Some(region) => writeln!(f, "Region:          {region}")?,
                    None => writeln!(f, "Region:          0x{:02X}", self.region)?,
                }
                if let Some(version) = self.version {
                    writeln!(f, "Version:         {version}")?;
                }
                write!(f, "Header Checksum: {}", self.checksum_line())?;
            }
            CartridgeType::SNES => {
                if self.rom_size > 0 {
                    writeln!(f, "ROM Size:        {}", format_size(self.rom_size))?;
                }
                write!(f, "(dump ROM to read title, mapper, and save size from the header)")?;
            }
        }

        Ok(())
    }
}

/// Extract the GB/GBC game title from a ROM header (bytes 0x0134..0x0143).
/// Stops at the first non-printable byte, which naturally excludes the null
/// padding and the CGB flag (0x80/0xC0) at 0x0143.
pub fn parse_gb_title(rom: &[u8]) -> Option<String> {
    if rom.len() < 0x144 {
        return None;
    }
    let title: String = rom[0x134..0x144]
        .iter()
        .take_while(|&&b| (0x20..0x7f).contains(&b))
        .map(|&b| b as char)
        .collect();
    let title = title.trim_end().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Extract the GBA game title from a ROM header (bytes 0xA0..0xAC).
/// The title is up to 12 bytes of uppercase ASCII, null-padded.
pub fn parse_gba_title(rom: &[u8]) -> Option<String> {
    if rom.len() < 0xAC {
        return None;
    }
    let title: String = rom[0xA0..0xAC]
        .iter()
        .take_while(|&&b| (0x20..0x7f).contains(&b))
        .map(|&b| b as char)
        .collect();
    let title = title.trim_end().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Decode the GB/GBC destination code at 0x014A. The Game Boy header only
/// distinguishes Japan from everywhere else (Pan Docs): 0x00 = Japan, 0x01 =
/// Non-Japan (overseas/international). Returns `None` if the header is short.
pub fn parse_gb_region(rom: &[u8]) -> Option<&'static str> {
    match rom.get(0x14A) {
        Some(0x00) => Some("Japan"),
        Some(0x01) => Some("Non-Japan (International)"),
        Some(_) => Some("Unknown"),
        None => None,
    }
}

/// Decode the GBA destination from the 4th character of the game code at 0xAF
/// (AGB-XXXY, where Y is the destination letter). Returns `None` if the header
/// is short or the letter isn't printable.
///
/// NOTE: unlike SNES, the GBA has no PAL/NTSC hardware split, so this letter is a
/// market/language code, not a hard region. In particular `E` is the *English*
/// release code used for carts sold in BOTH North America and Europe (e.g.
/// "Pokemon Ruby (USA, Europe)" is AXVE) — it does not mean USA-only. Telling
/// USA from Europe for an `E` cart isn't possible from the header; it needs a
/// CRC match against a No-Intro-style database.
pub fn parse_gba_region(rom: &[u8]) -> Option<String> {
    let c = *rom.get(0xAF)?;
    if !(0x20..0x7f).contains(&c) {
        return None;
    }
    let name = match c {
        b'J' => "Japan",
        b'E' => "USA/Europe (English)",
        b'P' => "Europe",
        b'D' => "Germany",
        b'F' => "France",
        b'I' => "Italy",
        b'S' => "Spain",
        b'H' => "Netherlands",
        b'K' => "Korea",
        b'X' => "Europe",
        b'U' => "Australia",
        b'A' => "Asia",
        b'C' => "China",
        _ => "Unknown",
    };
    Some(format!("{name} ({})", c as char))
}

/// Decode the SNES destination/region code (header byte 0x19). Values per the
/// SNESdev ROM-header table; 0x00/0x01 are NTSC, the rest are PAL variants.
pub fn snes_region_name(code: u8) -> &'static str {
    match code {
        0x00 => "Japan",
        0x01 => "USA",
        0x02 => "Europe/PAL",
        0x03 => "Scandinavia",
        0x04 => "Finland",
        0x05 => "Denmark",
        0x06 => "France (PAL)",
        0x07 => "Netherlands",
        0x08 => "Spain",
        0x09 => "Germany",
        0x0A => "Italy",
        0x0B => "China",
        0x0C => "Indonesia",
        0x0D => "South Korea",
        0x0E => "Common",
        0x0F => "Canada",
        0x10 => "Brazil",
        0x11 => "Australia",
        _ => "Unknown",
    }
}

/// Decoded MBC3 real-time clock. The registers track ELAPSED time since the clock
/// was last set (a running day/hour/minute/second counter), not wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcData {
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    /// 9-bit day counter (day-low byte + bit 0 of the control register).
    pub days: u16,
    /// Control bit 6: clock halted.
    pub halt: bool,
    /// Control bit 7: day counter overflowed past 511.
    pub day_carry: bool,
}

impl RtcData {
    /// Parse the device's 40-byte ReadRTC payload. It holds 5 registers (seconds,
    /// minutes, hours, day-low, day-control) as u32 little-endian, twice (current +
    /// latched); we decode the current set (the first five). `None` if too short.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 20 {
            return None;
        }
        let reg = |i: usize| payload[i * 4]; // low byte of each u32-LE register
        let dh = reg(4);
        Some(RtcData {
            seconds: reg(0),
            minutes: reg(1),
            hours: reg(2),
            days: ((dh as u16 & 1) << 8) | reg(3) as u16,
            halt: dh & 0x40 != 0,
            day_carry: dh & 0x80 != 0,
        })
    }

    /// Serialize back to the 40-byte payload (current + latched set to the same
    /// values), suitable for WriteRTC.
    pub fn to_payload(&self) -> Vec<u8> {
        let dh = ((self.days >> 8) as u8 & 1)
            | if self.halt { 0x40 } else { 0 }
            | if self.day_carry { 0x80 } else { 0 };
        let regs = [self.seconds, self.minutes, self.hours, self.days as u8, dh];
        let mut out = Vec::with_capacity(40);
        for _ in 0..2 {
            for &r in &regs {
                out.extend_from_slice(&(r as u32).to_le_bytes());
            }
        }
        out
    }

    /// Whether the registers are within MBC3 valid ranges. Out-of-range values
    /// (e.g. minutes > 59, hours > 23) indicate a dead/uninitialized RTC battery.
    pub fn is_valid(&self) -> bool {
        self.seconds <= 59 && self.minutes <= 59 && self.hours <= 23
    }
}

impl fmt::Display for RtcData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} days, {:02}:{:02}:{:02}",
            self.days, self.hours, self.minutes, self.seconds
        )?;
        if self.day_carry {
            write!(f, " (day overflow)")?;
        }
        if self.halt {
            write!(f, " (halted)")?;
        }
        Ok(())
    }
}

/// Compute the GB/GBC header checksum (the value stored at 0x14D). Per Pan Docs:
/// `x = 0; for 0x134..=0x14C: x = x - byte - 1`. Compare against rom[0x14D] to
/// validate. Returns `None` if the header is too short.
pub fn gb_header_checksum(rom: &[u8]) -> Option<u8> {
    if rom.len() < 0x14D {
        return None;
    }
    let mut x: u8 = 0;
    for &b in &rom[0x134..=0x14C] {
        x = x.wrapping_sub(b).wrapping_sub(1);
    }
    Some(x)
}

/// Compute the GBA header checksum (the value stored at 0xBD). Per GBATEK:
/// `chk = 0; for 0xA0..=0xBC: chk = chk - byte; chk = chk - 0x19`. Compare against
/// rom[0xBD] to validate. Returns `None` if the header is too short.
pub fn gba_header_checksum(rom: &[u8]) -> Option<u8> {
    if rom.len() < 0xBD {
        return None;
    }
    let mut chk: u8 = 0;
    for &b in &rom[0xA0..=0xBC] {
        chk = chk.wrapping_sub(b);
    }
    Some(chk.wrapping_sub(0x19))
}

/// Decode the SNES coprocessor from the cart-type byte (header 0x16) and, for the
/// Custom family, the chipset subtype byte (header base - 1, i.e. 0xFFBF/0x7FBF).
/// Returns `None` when the low nibble shows no coprocessor present. Family per the
/// SNESdev ROM-header table.
pub fn snes_coprocessor(cart_type: u8, subtype: u8) -> Option<&'static str> {
    // Low nibble 0x3..=0x6 means a coprocessor is present.
    if !matches!(cart_type & 0x0F, 0x03..=0x06) {
        return None;
    }
    Some(match cart_type >> 4 {
        0x0 => "DSP",
        0x1 => "SuperFX (GSU)",
        0x2 => "OBC1",
        0x3 => "SA-1",
        0x4 => "S-DD1",
        0x5 => "S-RTC",
        0xE => "Other (SGB/Satellaview)",
        0xF => match subtype {
            0x00 => "SPC7110",
            0x01 => "ST010/ST011",
            0x02 => "ST018",
            0x03 => "CX4",
            _ => "Custom",
        },
        _ => "Unknown",
    })
}

/// Classify a GB-family cartridge as GB or GBC from the CGB flag at 0x0143.
/// `0xC0` = CGB-only, `0x80` = CGB-enhanced but DMG-compatible (dual-mode),
/// anything else = original Game Boy. Falls back to "GB" if the header is short.
pub fn parse_cgb_flag(rom: &[u8]) -> &'static str {
    match rom.get(0x143) {
        Some(0xC0) => "GBC",
        Some(0x80) => "GB/GBC",
        _ => "GB",
    }
}

/// Detect the save type and size of a GBA ROM by scanning for save library strings.
pub fn detect_gba_save(rom: &[u8]) -> (ChipType, u32) {
    if find_bytes(rom, b"EEPROM_V").is_some() {
        // EEPROM is either 512 bytes (4Kbit) or 8 KB (64Kbit).
        // Always read 8KB — detect_eeprom_size() will determine the real size
        // by checking for mirroring after the data is read.
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

/// Detect actual EEPROM size from an 8KB read.
/// 512-byte EEPROM mirrors every 512 bytes when read as 8KB.
/// Returns the trimmed save data (512 bytes or 8KB).
pub fn detect_eeprom_size(data: &[u8]) -> Vec<u8> {
    if data.len() < 8 * 1024 {
        return data.to_vec();
    }

    let first_block = &data[..512];

    // Check if every 512-byte block is identical to the first
    let is_mirrored = (1..16).all(|i| {
        let block = &data[i * 512..(i + 1) * 512];
        block == first_block
    });

    if is_mirrored {
        // 512-byte EEPROM — data repeats every 512 bytes
        data[..512].to_vec()
    } else {
        data.to_vec()
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

/// SNES memory map / mapper mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnesMapper {
    LoRom,
    HiRom,
    ExHiRom,
}

impl fmt::Display for SnesMapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnesMapper::LoRom => write!(f, "LoROM"),
            SnesMapper::HiRom => write!(f, "HiROM"),
            SnesMapper::ExHiRom => write!(f, "ExHiROM"),
        }
    }
}

/// Parsed SNES internal cartridge header.
#[derive(Debug, Clone)]
#[allow(dead_code)] // save_chip is consumed by the SNES save path, pending hardware
pub struct SnesHeader {
    pub title: String,
    pub mapper: SnesMapper,
    pub rom_size: u32,
    pub ram_size: u32,
    pub save_chip: ChipType,
    /// Raw destination/region code (header byte 0x19); decode with snes_region_name.
    pub region: u8,
    /// Coprocessor name (DSP/SA-1/SuperFX/…) decoded from the cart-type byte, if any.
    pub coprocessor: Option<&'static str>,
    /// Mask ROM version (header byte 0x1B).
    pub version: u8,
    /// Stored checksum and its complement (header 0x1E/0x1C). `checksum ^ complement
    /// == 0xFFFF` is a header-only self-consistency check (the true checksum vs the
    /// whole-ROM byte sum needs a full dump).
    pub checksum: u16,
    pub complement: u16,
}

impl fmt::Display for SnesHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Type:            SNES")?;
        writeln!(f, "Title:           {}", self.title.trim())?;
        writeln!(f, "Mapper:          {}", self.mapper)?;
        if let Some(co) = self.coprocessor {
            writeln!(f, "Coprocessor:     {co}")?;
        }
        writeln!(f, "ROM Size:        {}", format_size(self.rom_size))?;
        if self.ram_size > 0 {
            writeln!(f, "Save:            {}", format_size(self.ram_size))?;
        } else {
            writeln!(f, "Save:            None")?;
        }
        writeln!(f, "Region:          {} (0x{:02X})", snes_region_name(self.region), self.region)?;
        writeln!(f, "Version:         {}", self.version)?;
        // Header-only self-consistency; full verification needs the whole-ROM sum.
        let ok = self.checksum ^ self.complement == 0xFFFF;
        write!(
            f,
            "Checksum:        0x{:04X} (complement {})",
            self.checksum,
            if ok { "OK" } else { "BAD" }
        )
    }
}

// Field offsets within the SNES header (relative to the header base).
const SNES_HDR_TITLE: usize = 0x00;
const SNES_HDR_MAP_MODE: usize = 0x15;
const SNES_HDR_CART_TYPE: usize = 0x16;
const SNES_HDR_ROM_SIZE: usize = 0x17;
const SNES_HDR_RAM_SIZE: usize = 0x18;
const SNES_HDR_REGION: usize = 0x19;
const SNES_HDR_VERSION: usize = 0x1B;
const SNES_HDR_CHECKSUM_COMP: usize = 0x1C;
const SNES_HDR_CHECKSUM: usize = 0x1E;
const SNES_HDR_LEN: usize = 0x20;

// Candidate header base offsets by mapper.
const LOROM_BASE: usize = 0x7FC0;
const HIROM_BASE: usize = 0xFFC0;
const EXHIROM_BASE: usize = 0x40FFC0;

/// Score how well the header at `base` validates. Higher is better; `None` if the
/// region isn't present. The checksum/complement pair summing to 0xFFFF is the
/// strongest signal; a sane map-mode byte adds confidence.
fn snes_header_score(rom: &[u8], base: usize, expect_map_hi_bit: u8) -> Option<u32> {
    if base + SNES_HDR_LEN > rom.len() {
        return None;
    }
    let h = &rom[base..base + SNES_HDR_LEN];
    let mut score = 0;

    let checksum = u16::from_le_bytes([h[SNES_HDR_CHECKSUM], h[SNES_HDR_CHECKSUM + 1]]);
    let complement =
        u16::from_le_bytes([h[SNES_HDR_CHECKSUM_COMP], h[SNES_HDR_CHECKSUM_COMP + 1]]);
    if checksum ^ complement == 0xFFFF && checksum != 0 {
        score += 4;
    }

    // Map-mode low nibble: 0 = LoROM, 1 = HiROM, 5 = ExHiROM. High nibble 0x2/0x3.
    let map = h[SNES_HDR_MAP_MODE];
    if (map & 0xF0) == 0x20 || (map & 0xF0) == 0x30 {
        score += 1;
    }
    if (map & 0x0F) == expect_map_hi_bit {
        score += 2;
    }

    // Title bytes should be printable ASCII.
    let printable = h[SNES_HDR_TITLE..SNES_HDR_TITLE + 21]
        .iter()
        .filter(|&&b| (0x20..0x7f).contains(&b))
        .count();
    if printable >= 16 {
        score += 2;
    }

    // ROM size code should be plausible (256KB..64MB → 0x08..0x10).
    let rom_code = h[SNES_HDR_ROM_SIZE];
    if (0x08..=0x10).contains(&rom_code) {
        score += 1;
    }

    Some(score)
}

/// Parse the internal header of a dumped SNES ROM, detecting the mapper and reading
/// ROM/save sizes. Returns `None` if no plausible header is found.
///
/// Used to print title/mapper/save details after a SNES dump, and as a cross-check
/// on the ROM-size code the SN Operator reports in its signature.
pub fn parse_snes_header(rom: &[u8]) -> Option<SnesHeader> {
    // Strip a 512-byte SMC copier header if present (raw dumps won't have one).
    let rom = if rom.len() % 1024 == 512 { &rom[512..] } else { rom };

    let candidates = [
        (LOROM_BASE, SnesMapper::LoRom, 0x0u8),
        (HIROM_BASE, SnesMapper::HiRom, 0x1u8),
        (EXHIROM_BASE, SnesMapper::ExHiRom, 0x5u8),
    ];

    let (base, mapper, _) = candidates
        .into_iter()
        .filter_map(|(base, mapper, bit)| {
            snes_header_score(rom, base, bit).map(|s| (s, base, mapper))
        })
        // Require a minimum confidence so we don't latch onto garbage.
        .filter(|(score, _, _)| *score >= 6)
        .max_by_key(|(score, _, _)| *score)
        .map(|(_, base, mapper)| (base, mapper, ()))?;

    let h = &rom[base..base + SNES_HDR_LEN];

    let title = String::from_utf8_lossy(&h[SNES_HDR_TITLE..SNES_HDR_TITLE + 21]).into_owned();

    let rom_code = h[SNES_HDR_ROM_SIZE];
    let rom_size = 1024u32 << rom_code;

    let ram_code = h[SNES_HDR_RAM_SIZE];
    let ram_size = if ram_code == 0 { 0 } else { 1024u32 << ram_code };

    // SNES battery saves are SRAM. The cart-type byte's low nibble distinguishes
    // ROM (0) / ROM+RAM (1) / ROM+RAM+battery (2); coprocessor carts use the high
    // nibble but still back saves with SRAM.
    let save_chip = if ram_size > 0 { ChipType::Sram } else { ChipType::Unknown };
    let region = h[SNES_HDR_REGION];
    let version = h[SNES_HDR_VERSION];

    // Coprocessor: cart-type byte plus the subtype byte one before the header base
    // (0xFFBF / 0x7FBF), only meaningful for the Custom family.
    let cart_type = h[SNES_HDR_CART_TYPE];
    let subtype = if base > 0 { rom[base - 1] } else { 0 };
    let coprocessor = snes_coprocessor(cart_type, subtype);

    let complement = u16::from_le_bytes([h[SNES_HDR_CHECKSUM_COMP], h[SNES_HDR_CHECKSUM_COMP + 1]]);
    let checksum = u16::from_le_bytes([h[SNES_HDR_CHECKSUM], h[SNES_HDR_CHECKSUM + 1]]);

    Some(SnesHeader {
        title,
        mapper,
        rom_size,
        ram_size,
        save_chip,
        region,
        coprocessor,
        version,
        checksum,
        complement,
    })
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
