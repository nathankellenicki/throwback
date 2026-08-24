//! Nintendo Power "GB Memory" (DMG-MMSA-JPN / G-MMC1) image assembly and
//! extraction — pure logic, no device I/O.
//!
//! The GB Memory cart is 1 MiB of flash behind a G-MMC1 mapper, plus a hidden
//! 128-byte "map" sector the mapper consults at power-up to decide what boots.
//! At a Japanese kiosk it held the Nintendo Power **menu ROM** (first 128 KiB)
//! plus up to 7 games, with the menu's own game table (records at 0x1C000)
//! describing them and the map sector routing the hardware.
//!
//! This module builds that whole picture from ordinary Game Boy ROMs (the step
//! FlashGBX leaves to external tools), and extracts games back out of a dump.
//! It is device-agnostic: [`GbMemoryImage`] is the 1 MiB flash payload plus the
//! 128-byte map sector; a backend writes them to the two flash regions.
//!
//! Written clean-room from the documented G-MMC1 / menu-table / map-sector
//! formats (msinger's np_gb_memory doc; behavior of the gbnp and
//! GB-Memory-Binary-Maker tools). No third-party code was copied.

use std::fmt;

/// Total flash image size (1 MiB).
pub const IMAGE_SIZE: usize = 0x100000;
/// The menu ROM occupies the first 128 KiB block.
pub const MENU_SIZE: usize = 0x20000;
/// The hidden map sector is 128 bytes.
pub const MAP_SIZE: usize = 128;
/// Maximum number of games (8 map entries: 1 menu + 7 games).
pub const MAX_GAMES: usize = 7;
/// Bytes available for games after the menu (1 MiB − 128 KiB).
pub const GAME_BUDGET: usize = IMAGE_SIZE - MENU_SIZE;

/// One 128 KiB block, the packing granularity for games.
const BLOCK: usize = 0x20000;
/// The menu's own game-table entry lives at 0x1C000; game entries follow at
/// 0x1C200 + i*0x200. All within the menu ROM region (< MENU_SIZE).
const TABLE_BASE: usize = 0x1C000;
const TABLE_RECORD: usize = 0x200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GbMemoryError {
    NoGames,
    TooManyGames(usize),
    ImageTooLarge { used: usize, budget: usize },
    InvalidMenu,
    InvalidGame { index: usize, reason: &'static str },
}

impl fmt::Display for GbMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GbMemoryError::NoGames => write!(f, "no games supplied"),
            GbMemoryError::TooManyGames(n) => {
                write!(f, "too many games: {n} (max {MAX_GAMES})")
            }
            GbMemoryError::ImageTooLarge { used, budget } => write!(
                f,
                "games total {} bytes, over the {} byte budget",
                used, budget
            ),
            GbMemoryError::InvalidMenu => {
                write!(f, "supplied menu ROM is not a Nintendo Power menu (128 KiB, header \"NP M-MENU\")")
            }
            GbMemoryError::InvalidGame { index, reason } => {
                write!(f, "game {index}: {reason}")
            }
        }
    }
}

impl std::error::Error for GbMemoryError {}

/// A fully assembled GB Memory cart: the 1 MiB flash payload and the 128-byte
/// hidden map sector (written to separate flash regions by the backend).
pub struct GbMemoryImage {
    pub rom: Vec<u8>,
    pub map: [u8; MAP_SIZE],
}

/// A game recovered from a dumped image, trimmed to its real ROM size.
pub struct ExtractedGame {
    pub title: String,
    pub is_cgb: bool,
    pub data: Vec<u8>,
    /// Byte offset of this game's save in the shared 128 KiB SRAM.
    pub save_offset: usize,
    /// This game's save size in bytes (0 if it has no battery save).
    pub save_size: usize,
}

/// The 8-byte writer ID throwback stamps into carts it assembles (honest,
/// self-identifying — never a fake kiosk ID).
const WRITER_ID: [u8; 8] = *b"THROWBAK";

/// Provenance written into a freshly assembled cart: a real timestamp plus the
/// throwback writer ID. `blank()` leaves those fields empty (0xFF) — the caller
/// that reads the clock builds a dated one with `new()`. Restore/`--image`
/// never uses this (the original map is written verbatim).
#[derive(Clone, Copy)]
pub struct Stamp {
    /// 18 ASCII bytes formatted `MM/DD/YYYYHH:MM:SS`, or all-0xFF for blank.
    timestamp: [u8; 18],
    writer: [u8; 8],
}

impl Stamp {
    /// No provenance stamp (fields left 0xFF).
    pub fn blank() -> Self {
        Self { timestamp: [0xFF; 18], writer: [0xFF; 8] }
    }

    /// A dated stamp from a preformatted `MM/DD/YYYYHH:MM:SS` timestamp.
    pub fn new(timestamp: [u8; 18]) -> Self {
        Self { timestamp, writer: WRITER_ID }
    }
}

// --- Header decoding (map/table field derivation) ----------------------------

/// Map the cartridge-header MBC byte (0x147) to the G-MMC1 map's 3-bit MBC code
/// (0 none, 1 MBC1, 2 MBC2, 3 MBC3, 5 MBC5; unknown mappers emulate as MBC5).
fn map_mbc_code(mbc_byte: u8) -> u8 {
    match mbc_byte {
        0x00 | 0x08 | 0x09 => 0,
        0x01..=0x03 => 1,
        0x05 | 0x06 => 2,
        0x0F..=0x13 => 3,
        0x19..=0x1E => 5,
        _ => 5,
    }
}

fn is_mbc2(mbc_byte: u8) -> bool {
    mbc_byte == 0x05 || mbc_byte == 0x06
}

/// RAM size in KiB from the cartridge-header RAM byte (0x149). Note the header
/// order differs from the map's: header 0x04 = 128 KiB, 0x05 = 64 KiB.
fn ram_kib_from_header(ram_byte: u8) -> u32 {
    match ram_byte {
        0x01 => 2,
        0x02 => 8,
        0x03 => 32,
        0x04 => 128,
        0x05 => 64,
        _ => 0,
    }
}

/// Map a RAM size in KiB to the G-MMC1 map's 3-bit RAM code
/// (0 none, 1 2K, 2 8K, 3 32K, 4 64K, 5 128K).
fn map_ram_code(ram_kib: u32) -> u8 {
    match ram_kib {
        2 => 1,
        8 => 2,
        32 => 3,
        64 => 4,
        128 => 5,
        _ => 0,
    }
}

/// RAM size in KiB from the map's 3-bit RAM code (inverse of `map_ram_code`).
fn ram_kib_from_map_code(code: u8) -> u32 {
    match code {
        1 => 2,
        2 => 8,
        3 => 32,
        4 => 64,
        5 => 128,
        _ => 0,
    }
}

/// ROM size code from the header byte (0x148): 0=32K, 1=64K, … 5=1M. This is
/// also the map's ROM-size code for sizes up to 1 MiB. Returns None if the code
/// is larger than a GB Memory cart can hold (> 1 MiB).
fn rom_size_code(rom_byte: u8) -> Option<u8> {
    if rom_byte <= 5 { Some(rom_byte) } else { None }
}

/// Natural ROM size in bytes for a header size code.
fn rom_bytes_for_code(code: u8) -> usize {
    0x8000usize << code // 32 KiB << code
}

/// Number of 128 KiB blocks a game occupies (padded up to at least one block).
fn padded_blocks(code: u8) -> usize {
    rom_bytes_for_code(code).div_ceil(BLOCK).max(1)
}

/// Build a 3-byte G-MMC1 map entry.
/// byte0 = mbc<<5 | rom_size<<2 | (ram_size high 2 bits)
/// byte1 = (ram_size low bit)<<7 | rom_offset (×32 KiB)
/// byte2 = ram_offset (×2 KiB)
fn map_entry(mbc: u8, rom_code: u8, ram_code: u8, rom_off_32k: u8, ram_off_2k: u8) -> [u8; 3] {
    [
        ((mbc & 7) << 5) | ((rom_code & 7) << 2) | ((ram_code >> 1) & 3),
        ((ram_code & 1) << 7) | (rom_off_32k & 0x3F),
        ram_off_2k & 0x3F,
    ]
}

/// The menu's own map entry: MBC5, 128 KiB, no RAM, offset 0 → 0xA8 0x00 0x00.
const MENU_MAP_ENTRY: [u8; 3] = [0xA8, 0x00, 0x00];

// --- Detection ---------------------------------------------------------------

/// True if `header` is the Nintendo Power menu ROM (title "NP M-MENU" at 0x134,
/// mapper byte 0x19 at 0x147).
pub fn is_menu_rom(header: &[u8]) -> bool {
    header.len() >= 0x150
        && &header[0x134..0x134 + 9] == b"NP M-MENU"
        && header[0x147] == 0x19
}

// --- Map sector construction -------------------------------------------------

/// Per-game decoded fields used to build both the table records and map entries.
struct GameLayout {
    mbc: u8,
    rom_code: u8,
    ram_code: u8,
    ram_kib: u32,
    block: usize, // 128 KiB block offset in the image
    blocks: usize,
}

fn layout_game(index: usize, game: &[u8]) -> Result<(u8, u8, u32, usize), GbMemoryError> {
    if game.len() < 0x150 {
        return Err(GbMemoryError::InvalidGame { index, reason: "ROM header too short" });
    }
    let code = rom_size_code(game[0x148])
        .ok_or(GbMemoryError::InvalidGame { index, reason: "ROM larger than 1 MiB" })?;
    let mbc = map_mbc_code(game[0x147]);
    let ram_kib = if is_mbc2(game[0x147]) {
        8 // MBC2 has internal battery RAM; give it an 8 KiB save slot.
    } else {
        ram_kib_from_header(game[0x149])
    };
    Ok((mbc, code, ram_kib, padded_blocks(code)))
}

/// Assemble the 128-byte map sector from the menu entry (or a single full-mode
/// game as entry 0) plus per-game entries, carrying cart-id and write count.
fn build_map(entry0: [u8; 3], games: &[GameLayout], old_map: Option<&[u8; MAP_SIZE]>) -> [u8; MAP_SIZE] {
    let mut map = [0xFFu8; MAP_SIZE];

    // Entry 0, then one entry per game.
    map[0..3].copy_from_slice(&entry0);
    let mut ram_off_kib = 0u32;
    for (i, g) in games.iter().enumerate() {
        let rom_off_32k = (g.block * 4) as u8; // 128 KiB block = 4 × 32 KiB
        let ram_off_2k = (ram_off_kib / 2) as u8;
        let entry = map_entry(g.mbc, g.rom_code, g.ram_code, rom_off_32k, ram_off_2k);
        let at = 3 + i * 3;
        map[at..at + 3].copy_from_slice(&entry);
        // Advance the SRAM cursor: every game reserves at least an 8 KiB slot
        // (matches the reference tools; collision-free even for no-save games).
        ram_off_kib += if g.ram_kib < 8 { 8 } else { g.ram_kib };
    }

    // Trailer: write count (0x6E), cart id (0x70), padding, and the mandatory
    // 0x00 at 0x7F (a non-zero byte there makes the mapper treat the whole map
    // as invalid and the cart boots dead).
    let write_count = match old_map {
        Some(m) => u16::from_le_bytes([m[0x6E], m[0x6F]]).saturating_add(1),
        None => 0,
    };
    map[0x6E..0x70].copy_from_slice(&write_count.to_le_bytes());
    match old_map {
        Some(m) => map[0x70..0x78].copy_from_slice(&m[0x70..0x78]),
        None => map[0x70..0x78].fill(0xFF),
    }
    map[0x78..0x7E].fill(0xFF);
    map[0x7E] = 0x00;
    map[0x7F] = 0x00;
    map
}

// --- Table record construction (cosmetic + load-bearing) ---------------------

/// Write one game-table record into the menu region of `rom`. Only menu_index,
/// f_offset, and f_size are load-bearing for booting; the rest is cosmetic and
/// filled for authenticity / tooling.
fn write_table_record(rom: &mut [u8], slot: usize, g: &GameLayout, title: &str, stamp: &Stamp) {
    let base = TABLE_BASE + (slot + 1) * TABLE_RECORD; // slot 0 game → record 1
    if base + TABLE_RECORD > rom.len() {
        return;
    }
    let rec = &mut rom[base..base + TABLE_RECORD];
    rec.fill(0x00);

    rec[0x00] = (slot + 1) as u8; // menu_index (1-based; 0/FF = empty)
    rec[0x01] = g.block as u8; // f_offset in 128 KiB blocks
    rec[0x02] = 0x00; // b_offset (cosmetic; real SRAM map is the map sector)
    rec[0x03..0x05].copy_from_slice(&(g.blocks as u16).to_le_bytes()); // f_size (128 KiB units)
    rec[0x05..0x07].copy_from_slice(&0u16.to_le_bytes()); // b_size (cosmetic)

    // game_code: "DMG -XXXX-  " (12 bytes), XXXX derived from the title.
    let mut code = *b"DMG -    -  ";
    for (i, c) in title.chars().filter(|c| c.is_ascii_alphanumeric()).take(4).enumerate() {
        code[5 + i] = c.to_ascii_uppercase() as u8;
    }
    rec[0x07..0x13].copy_from_slice(&code);

    // title: ASCII, space-padded, 44 bytes.
    let mut t = [b' '; 44];
    for (i, b) in title.bytes().take(44).enumerate() {
        t[i] = b;
    }
    rec[0x13..0x3F].copy_from_slice(&t);

    // title_graphic: 384-byte 2bpp tile strip.
    rec[0x3F..0x1BF].copy_from_slice(&render_title_graphic(title));

    // timestamp (18) + writer id (8): the real write date + THROWBAK, or 0xFF
    // when blank. padding (23), comment (16): cosmetic.
    rec[0x1BF..0x1D1].copy_from_slice(&stamp.timestamp);
    rec[0x1D1..0x1D9].copy_from_slice(&stamp.writer);
    rec[0x1D9..0x1F0].fill(0xFF);
    rec[0x1F0..0x200].fill(0xFF);
}

// --- Assembly ----------------------------------------------------------------

/// Assemble a menu-mode (multiboot) image: the NP menu ROM plus 1..=7 games.
pub fn assemble(
    menu: &[u8],
    games: &[Vec<u8>],
    old_map: Option<&[u8; MAP_SIZE]>,
    stamp: Stamp,
) -> Result<GbMemoryImage, GbMemoryError> {
    if games.is_empty() {
        return Err(GbMemoryError::NoGames);
    }
    if games.len() > MAX_GAMES {
        return Err(GbMemoryError::TooManyGames(games.len()));
    }
    if menu.len() < MENU_SIZE || !is_menu_rom(menu) {
        return Err(GbMemoryError::InvalidMenu);
    }

    // Decode + place each game, checking the budget.
    let mut layouts = Vec::with_capacity(games.len());
    let mut block = 1usize; // block 0 is the menu
    for (i, game) in games.iter().enumerate() {
        let (mbc, rom_code, ram_kib, blocks) = layout_game(i, game)?;
        layouts.push(GameLayout {
            mbc,
            rom_code,
            ram_code: map_ram_code(ram_kib),
            ram_kib,
            block,
            blocks,
        });
        block += blocks;
    }
    let used = (block - 1) * BLOCK;
    if used > GAME_BUDGET {
        return Err(GbMemoryError::ImageTooLarge { used, budget: GAME_BUDGET });
    }

    // Erased flash reads as 0xFF; unused bytes stay 0xFF so the flasher skips them.
    let mut rom = vec![0xFFu8; IMAGE_SIZE];
    rom[..MENU_SIZE].copy_from_slice(&menu[..MENU_SIZE]);

    for (i, (game, l)) in games.iter().zip(&layouts).enumerate() {
        let off = l.block * BLOCK;
        let end = (off + l.blocks * BLOCK).min(IMAGE_SIZE);
        let take = game.len().min(end - off);
        rom[off..off + take].copy_from_slice(&game[..take]);
        let title = crate::cartridge::parse_gb_title(game).unwrap_or_default();
        write_table_record(&mut rom, i, l, &title, &stamp);
    }

    let map = build_map(MENU_MAP_ENTRY, &layouts, old_map);
    Ok(GbMemoryImage { rom, map })
}

/// Assemble a full-mode image: a single game that boots directly, no menu.
pub fn assemble_full_mode(
    game: &[u8],
    old_map: Option<&[u8; MAP_SIZE]>,
    stamp: Stamp,
) -> Result<GbMemoryImage, GbMemoryError> {
    let (mbc, rom_code, ram_kib, blocks) = layout_game(0, game)?;
    let layout = GameLayout {
        mbc,
        rom_code,
        ram_code: map_ram_code(ram_kib),
        ram_kib,
        block: 0, // the single game sits at offset 0 and boots directly
        blocks,
    };

    let mut rom = vec![0xFFu8; IMAGE_SIZE];
    let take = game.len().min(IMAGE_SIZE);
    rom[..take].copy_from_slice(&game[..take]);

    // Entry 0 IS the game (no menu), so the mapper boots it directly.
    let entry0 = map_entry(layout.mbc, layout.rom_code, layout.ram_code, 0, 0);
    // No trailing game entries in full mode: the single game is entry 0.
    let mut map = build_map(entry0, &[], old_map);
    // In full mode the map sector itself carries the game's provenance
    // (no menu table), so stamp the timestamp + writer id there.
    map[0x54..0x66].copy_from_slice(&stamp.timestamp);
    map[0x66..0x6E].copy_from_slice(&stamp.writer);
    Ok(GbMemoryImage { rom, map })
}

// --- Extraction --------------------------------------------------------------

/// Extract the playable games from a dumped 1 MiB image + its map sector,
/// skipping the menu entry. Games are returned in map-entry (slot) order.
pub fn extract_games(image: &[u8], map: &[u8; MAP_SIZE]) -> Vec<ExtractedGame> {
    let mut out = Vec::new();
    // Up to 8 entries (menu + 7 games) at the front of the map.
    for k in 0..8 {
        let e = &map[k * 3..k * 3 + 3];
        if e == [0xFF, 0xFF, 0xFF] || e == [0x00, 0x00, 0x00] {
            continue;
        }
        let rom_code = (e[0] >> 2) & 7;
        let size = rom_bytes_for_code(rom_code);
        let off = (e[1] as usize & 0x3F) * 0x8000; // 32 KiB units
        if off + size > image.len() {
            continue;
        }
        let slice = &image[off..off + size];
        if is_menu_rom(slice) {
            continue; // the menu itself, not a game
        }
        let title = crate::cartridge::parse_gb_title(slice).unwrap_or_default();
        let is_cgb = slice.get(0x143).is_some_and(|&b| b & 0x80 != 0);
        // Save slot: RAM code spans byte0 (high 2 bits) and byte1 (low bit);
        // byte2 is the SRAM offset in 2 KiB units.
        let ram_code = ((e[0] & 0x03) << 1) | ((e[1] >> 7) & 1);
        let save_size = ram_kib_from_map_code(ram_code) as usize * 1024;
        let save_offset = (e[2] as usize & 0x3F) * 0x800; // 2 KiB units
        out.push(ExtractedGame {
            title,
            is_cgb,
            data: slice.to_vec(),
            save_offset,
            save_size,
        });
    }
    out
}

// --- Title graphic -----------------------------------------------------------

/// Render up to 24 characters of `title` into a 384-byte 2bpp Game Boy tile
/// strip (24 tiles, one tile tall). Foreground pixels are colour 3 (both
/// bitplanes set); background is colour 0. Unknown characters render as spaces.
pub fn render_title_graphic(title: &str) -> [u8; 384] {
    let mut out = [0u8; 384];
    for (i, ch) in title.chars().take(24).enumerate() {
        let glyph = font_glyph(ch.to_ascii_uppercase());
        let tile = &mut out[i * 16..i * 16 + 16];
        for (r, &row) in glyph.iter().enumerate() {
            // Colour 3 = both bitplanes carry the same row bitmap.
            tile[r * 2] = row;
            tile[r * 2 + 1] = row;
        }
    }
    out
}

/// A minimal 8×8 uppercase bitmap font (MSB = leftmost pixel), hand-authored
/// (public-domain glyph shapes; no third-party font data). Covers A–Z, 0–9,
/// space, and a few punctuation marks; anything else renders blank.
fn font_glyph(c: char) -> [u8; 8] {
    match c {
        'A' => [0x70, 0x88, 0x88, 0xF8, 0x88, 0x88, 0x88, 0x00],
        'B' => [0xF0, 0x88, 0x88, 0xF0, 0x88, 0x88, 0xF0, 0x00],
        'C' => [0x70, 0x88, 0x80, 0x80, 0x80, 0x88, 0x70, 0x00],
        'D' => [0xE0, 0x90, 0x88, 0x88, 0x88, 0x90, 0xE0, 0x00],
        'E' => [0xF8, 0x80, 0x80, 0xF0, 0x80, 0x80, 0xF8, 0x00],
        'F' => [0xF8, 0x80, 0x80, 0xF0, 0x80, 0x80, 0x80, 0x00],
        'G' => [0x70, 0x88, 0x80, 0xB8, 0x88, 0x88, 0x70, 0x00],
        'H' => [0x88, 0x88, 0x88, 0xF8, 0x88, 0x88, 0x88, 0x00],
        'I' => [0x70, 0x20, 0x20, 0x20, 0x20, 0x20, 0x70, 0x00],
        'J' => [0x38, 0x10, 0x10, 0x10, 0x90, 0x90, 0x60, 0x00],
        'K' => [0x88, 0x90, 0xA0, 0xC0, 0xA0, 0x90, 0x88, 0x00],
        'L' => [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0xF8, 0x00],
        'M' => [0x88, 0xD8, 0xA8, 0xA8, 0x88, 0x88, 0x88, 0x00],
        'N' => [0x88, 0xC8, 0xA8, 0x98, 0x88, 0x88, 0x88, 0x00],
        'O' => [0x70, 0x88, 0x88, 0x88, 0x88, 0x88, 0x70, 0x00],
        'P' => [0xF0, 0x88, 0x88, 0xF0, 0x80, 0x80, 0x80, 0x00],
        'Q' => [0x70, 0x88, 0x88, 0x88, 0xA8, 0x90, 0x68, 0x00],
        'R' => [0xF0, 0x88, 0x88, 0xF0, 0xA0, 0x90, 0x88, 0x00],
        'S' => [0x78, 0x80, 0x80, 0x70, 0x08, 0x08, 0xF0, 0x00],
        'T' => [0xF8, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00],
        'U' => [0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x70, 0x00],
        'V' => [0x88, 0x88, 0x88, 0x88, 0x88, 0x50, 0x20, 0x00],
        'W' => [0x88, 0x88, 0x88, 0xA8, 0xA8, 0xD8, 0x88, 0x00],
        'X' => [0x88, 0x88, 0x50, 0x20, 0x50, 0x88, 0x88, 0x00],
        'Y' => [0x88, 0x88, 0x50, 0x20, 0x20, 0x20, 0x20, 0x00],
        'Z' => [0xF8, 0x08, 0x10, 0x20, 0x40, 0x80, 0xF8, 0x00],
        '0' => [0x70, 0x88, 0x98, 0xA8, 0xC8, 0x88, 0x70, 0x00],
        '1' => [0x20, 0x60, 0x20, 0x20, 0x20, 0x20, 0x70, 0x00],
        '2' => [0x70, 0x88, 0x08, 0x30, 0x40, 0x80, 0xF8, 0x00],
        '3' => [0xF8, 0x08, 0x10, 0x30, 0x08, 0x88, 0x70, 0x00],
        '4' => [0x10, 0x30, 0x50, 0x90, 0xF8, 0x10, 0x10, 0x00],
        '5' => [0xF8, 0x80, 0xF0, 0x08, 0x08, 0x88, 0x70, 0x00],
        '6' => [0x30, 0x40, 0x80, 0xF0, 0x88, 0x88, 0x70, 0x00],
        '7' => [0xF8, 0x08, 0x10, 0x20, 0x40, 0x40, 0x40, 0x00],
        '8' => [0x70, 0x88, 0x88, 0x70, 0x88, 0x88, 0x70, 0x00],
        '9' => [0x70, 0x88, 0x88, 0x78, 0x08, 0x10, 0x60, 0x00],
        '-' => [0x00, 0x00, 0x00, 0xF8, 0x00, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x60, 0x60, 0x00],
        ':' => [0x00, 0x60, 0x60, 0x00, 0x60, 0x60, 0x00, 0x00],
        '!' => [0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x20, 0x00],
        '&' => [0x60, 0x90, 0x90, 0x60, 0x94, 0x88, 0x74, 0x00],
        '\'' => [0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00],
        _ => [0x00; 8], // space / unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGO: [u8; 48] = [
        0xCE, 0xED, 0x66, 0x66, 0xCC, 0x0D, 0x00, 0x0B, 0x03, 0x73, 0x00, 0x83, 0x00, 0x0C, 0x00,
        0x0D, 0x00, 0x08, 0x11, 0x1F, 0x88, 0x89, 0x00, 0x0E, 0xDC, 0xCC, 0x6E, 0xE6, 0xDD, 0xDD,
        0xD9, 0x99, 0xBB, 0xBB, 0x67, 0x63, 0x6E, 0x0E, 0xEC, 0xCC, 0xDD, 0xDC, 0x99, 0x9F, 0xBB,
        0xB9, 0x33, 0x3E,
    ];

    /// Build a synthetic GB ROM with a valid header. Bytes outside the header
    /// carry a per-offset sentinel so extraction round-trips can be verified.
    fn make_rom(mbc: u8, rom_code: u8, ram_code: u8, cgb: bool, title: &str) -> Vec<u8> {
        let size = 0x8000usize << rom_code;
        let mut rom: Vec<u8> = (0..size).map(|i| (i as u8) ^ (i >> 8) as u8).collect();
        rom[0x104..0x134].copy_from_slice(&LOGO);
        for b in &mut rom[0x134..0x144] {
            *b = 0;
        }
        rom[0x134..0x134 + title.len()].copy_from_slice(title.as_bytes());
        rom[0x143] = if cgb { 0x80 } else { 0x00 };
        rom[0x147] = mbc;
        rom[0x148] = rom_code;
        rom[0x149] = ram_code;
        rom[0x14A] = 0x01;
        rom[0x14C] = 0x00;
        rom[0x14D] = crate::cartridge::gb_header_checksum(&rom).unwrap();
        let g = crate::cartridge::gb_global_checksum(&rom).unwrap();
        rom[0x14E..0x150].copy_from_slice(&g.to_be_bytes());
        rom
    }

    fn make_menu() -> Vec<u8> {
        let mut rom = vec![0xFFu8; MENU_SIZE];
        rom[0x104..0x134].copy_from_slice(&LOGO);
        for b in &mut rom[0x134..0x144] {
            *b = 0;
        }
        rom[0x134..0x134 + 15].copy_from_slice(b"NP M-MENU  MENU");
        rom[0x147] = 0x19; // MBC5
        rom[0x148] = 0x02; // 128 KiB
        rom[0x149] = 0x00;
        rom[0x14D] = crate::cartridge::gb_header_checksum(&rom).unwrap();
        // Menu's own table entry at 0x1C000, plus empty (0xFF) game records.
        rom[TABLE_BASE] = 0x00;
        rom
    }

    fn decode_entry(e: &[u8]) -> (u8, u8, u8, usize, usize) {
        let mbc = e[0] >> 5;
        let rom_code = (e[0] >> 2) & 7;
        let ram_code = ((e[0] & 3) << 1) | (e[1] >> 7);
        let rom_off_32k = (e[1] & 0x3F) as usize;
        let ram_off_2k = (e[2] & 0x3F) as usize;
        (mbc, rom_code, ram_code, rom_off_32k, ram_off_2k)
    }

    #[test]
    fn is_menu_rom_detection() {
        assert!(is_menu_rom(&make_menu()));
        assert!(!is_menu_rom(&make_rom(0x1B, 0x05, 0x03, false, "TETRIS")));
        assert!(!is_menu_rom(&[0u8; 0x10])); // too short
    }

    #[test]
    fn assemble_single_game_layout() {
        let menu = make_menu();
        let game = make_rom(0x03, 0x03, 0x03, false, "GAME ONE"); // MBC1, 256K, 32K RAM
        let img = assemble(&menu, std::slice::from_ref(&game), None, Stamp::blank()).unwrap();

        assert_eq!(img.rom.len(), IMAGE_SIZE);
        // Menu copied verbatim, except the game-table records (0x1C200+) that
        // we populate inside the menu region.
        assert_eq!(&img.rom[..0x1C200], &menu[..0x1C200]);
        // Game at block 1 (0x20000), byte-identical.
        assert_eq!(&img.rom[BLOCK..BLOCK + game.len()], &game[..]);

        // Table record 1 (0x1C200): load-bearing fields.
        let rec = &img.rom[0x1C200..0x1C200 + 0x200];
        assert_eq!(rec[0x00], 1); // menu_index
        assert_eq!(rec[0x01], 1); // f_offset block
        assert_eq!(u16::from_le_bytes([rec[0x03], rec[0x04]]), 2); // f_size = 256K/128K

        // Map: entry 0 = menu, entry 1 = the game.
        assert_eq!(&img.map[0..3], &MENU_MAP_ENTRY);
        let (mbc, rom_code, ram_code, rom_off, ram_off) = decode_entry(&img.map[3..6]);
        assert_eq!(mbc, 1); // MBC1
        assert_eq!(rom_code, 3); // 256K
        assert_eq!(ram_code, 3); // 32K
        assert_eq!(rom_off, 4); // block 1 → 4 × 32K
        assert_eq!(ram_off, 0);
        assert_eq!(img.map[0x7F], 0x00);
    }

    #[test]
    fn assemble_multi_game_offsets_and_map() {
        let menu = make_menu();
        let games = vec![
            make_rom(0x03, 0x03, 0x02, false, "FIRST"),  // MBC1 256K 8K → blocks 1-2
            make_rom(0x00, 0x00, 0x00, false, "SECOND"), // ROM-only 32K → block 3
            make_rom(0x1B, 0x04, 0x03, true, "THIRD"),   // MBC5 512K 32K → blocks 4-7
        ];
        let img = assemble(&menu, &games, None, Stamp::blank()).unwrap();

        // Games land at blocks 1, 3, 4.
        assert_eq!(&img.rom[BLOCK..BLOCK + games[0].len()], &games[0][..]);
        assert_eq!(&img.rom[3 * BLOCK..3 * BLOCK + games[1].len()], &games[1][..]);
        assert_eq!(&img.rom[4 * BLOCK..4 * BLOCK + games[2].len()], &games[2][..]);

        // Map entries decode to the right offsets (32K units): 4, 12, 16.
        assert_eq!(decode_entry(&img.map[3..6]).3, 4);
        assert_eq!(decode_entry(&img.map[6..9]).3, 12);
        assert_eq!(decode_entry(&img.map[9..12]).3, 16);
        // SRAM offsets advance ≥8K each: 0, 8K(=4×2K), 16K(=8×2K).
        assert_eq!(decode_entry(&img.map[3..6]).4, 0);
        assert_eq!(decode_entry(&img.map[6..9]).4, 4);
        assert_eq!(decode_entry(&img.map[9..12]).4, 8);
        // Third game: MBC5, 512K, 32K.
        let (mbc, rom_code, ram_code, _, _) = decode_entry(&img.map[9..12]);
        assert_eq!((mbc, rom_code, ram_code), (5, 4, 3));
    }

    #[test]
    fn assemble_full_mode_entry_is_the_game() {
        let game = make_rom(0x03, 0x00, 0x00, false, "SOLO"); // MBC1 32K no RAM
        let img = assemble_full_mode(&game, None, Stamp::blank()).unwrap();
        assert_eq!(img.rom.len(), IMAGE_SIZE);
        assert_eq!(&img.rom[..game.len()], &game[..]); // game at offset 0
        let (mbc, rom_code, _, rom_off, _) = decode_entry(&img.map[0..3]);
        assert_eq!(mbc, 1);
        assert_eq!(rom_code, 0);
        assert_eq!(rom_off, 0); // boots directly from offset 0
        assert_eq!(img.map[0x7F], 0x00);
    }

    #[test]
    fn errors() {
        let menu = make_menu();
        assert!(matches!(assemble(&menu, &[], None, Stamp::blank()), Err(GbMemoryError::NoGames)));

        let small = make_rom(0x00, 0x00, 0x00, false, "X");
        let too_many: Vec<Vec<u8>> = (0..8).map(|_| small.clone()).collect();
        assert!(matches!(
            assemble(&menu, &too_many, None, Stamp::blank()),
            Err(GbMemoryError::TooManyGames(8))
        ));

        // 7 × 512K = 3584K, well over the 896K budget.
        let big = make_rom(0x1B, 0x04, 0x00, false, "BIG");
        let over: Vec<Vec<u8>> = (0..7).map(|_| big.clone()).collect();
        assert!(matches!(
            assemble(&menu, &over, None, Stamp::blank()),
            Err(GbMemoryError::ImageTooLarge { .. })
        ));

        // Not a menu ROM.
        assert!(matches!(
            assemble(&small, std::slice::from_ref(&small), None, Stamp::blank()),
            Err(GbMemoryError::InvalidMenu)
        ));
    }

    #[test]
    fn write_count_and_cart_id_carryover() {
        let menu = make_menu();
        let game = make_rom(0x00, 0x00, 0x00, false, "G");
        let mut old = [0xFFu8; MAP_SIZE];
        old[0x6E..0x70].copy_from_slice(&41u16.to_le_bytes());
        old[0x70..0x78].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let img = assemble(&menu, &[game], Some(&old), Stamp::blank()).unwrap();
        assert_eq!(u16::from_le_bytes([img.map[0x6E], img.map[0x6F]]), 42);
        assert_eq!(&img.map[0x70..0x78], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(img.map[0x7F], 0x00);

        // No old map → write count 0.
        let menu2 = make_menu();
        let game2 = make_rom(0x00, 0x00, 0x00, false, "G");
        let img2 = assemble(&menu2, &[game2], None, Stamp::blank()).unwrap();
        assert_eq!(u16::from_le_bytes([img2.map[0x6E], img2.map[0x6F]]), 0);
    }

    #[test]
    fn extract_round_trips_assembled_image() {
        let menu = make_menu();
        let games = vec![
            make_rom(0x03, 0x03, 0x02, false, "ALPHA"),
            make_rom(0x00, 0x00, 0x00, false, "BETA"),
            make_rom(0x1B, 0x02, 0x03, true, "GAMMA"),
        ];
        let img = assemble(&menu, &games, None, Stamp::blank()).unwrap();
        let got = extract_games(&img.rom, &img.map);

        assert_eq!(got.len(), 3); // menu skipped
        assert_eq!(got[0].title, "ALPHA");
        assert_eq!(got[1].title, "BETA");
        assert_eq!(got[2].title, "GAMMA");
        assert!(!got[0].is_cgb);
        assert!(got[2].is_cgb);
        // Byte-identical to the originals after trimming to real size.
        for (g, orig) in got.iter().zip(&games) {
            assert_eq!(&g.data, orig);
        }

        // Save slots decode back to the writer's SRAM layout: each game reserves
        // at least an 8 KiB slot, so ALPHA(8K)@0, BETA(no save) still advances
        // the cursor, GAMMA(32K)@16K.
        assert_eq!((got[0].save_offset, got[0].save_size), (0, 8 * 1024));
        assert_eq!(got[1].save_size, 0); // BETA is ROM-only, no battery save
        assert_eq!((got[2].save_offset, got[2].save_size), (16 * 1024, 32 * 1024));
    }

    #[test]
    fn extract_full_mode() {
        let game = make_rom(0x03, 0x01, 0x02, false, "ONLYONE"); // 64K
        let img = assemble_full_mode(&game, None, Stamp::blank()).unwrap();
        let got = extract_games(&img.rom, &img.map);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "ONLYONE");
        assert_eq!(got[0].data, game);
    }

    #[test]
    fn title_graphic_format() {
        let g = render_title_graphic("TETRIS");
        assert_eq!(g.len(), 384);
        // "TETRIS" = 6 tiles of non-blank data, rest zero.
        assert!(g[0..16].iter().any(|&b| b != 0)); // 'T'
        assert!(g[6 * 16..].iter().all(|&b| b == 0)); // nothing past 6 chars
        // Colour 3: the two bitplane bytes of each row are equal.
        for r in 0..8 {
            assert_eq!(g[r * 2], g[r * 2 + 1]);
        }
        // Blank title → all zero.
        assert!(render_title_graphic("").iter().all(|&b| b == 0));
        // Over-long title is capped at 24 tiles (384 bytes) without panicking.
        let long = "A".repeat(50);
        assert_eq!(render_title_graphic(&long).len(), 384);
    }
}
