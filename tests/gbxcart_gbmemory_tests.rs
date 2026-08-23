//! GB Memory (G-MMC1) device-flow tests against the focused G-MMC1 simulator.
//!
//! These exercise the on-device choreography — wake/unlock, chip-erase gating,
//! banked megabyte programming, the 0xB7 map-write path, and map read-back —
//! end to end, so the full write→read→extract round-trip is validated before
//! any collectible-cart hardware run. The pure image/map/table correctness is
//! covered separately by the src/gbmemory.rs unit tests.

mod gbxcart_sim;

use gbxcart_sim::{make_gb_rom, SimGbMemory};
use throwback::device::gbxcart::GbxCart;
use throwback::device::CartridgeDevice;
use throwback::gbmemory;

/// A minimal but valid NP menu ROM: correct menu header so `is_menu_rom` and
/// the header-based detection accept it. Content beyond the header is filler
/// (the map — not the menu's own table — drives extraction).
fn make_menu_rom() -> Vec<u8> {
    let mut rom = vec![0xFFu8; gbmemory::MENU_SIZE];
    // Nintendo logo (required for a valid GB header).
    rom[0x104..0x134].copy_from_slice(&gbxcart_sim::GB_LOGO);
    for b in &mut rom[0x134..0x144] {
        *b = 0;
    }
    rom[0x134..0x134 + 15].copy_from_slice(b"NP M-MENU  MENU");
    rom[0x147] = 0x19; // MBC5
    rom[0x148] = 0x05; // 1 MB
    rom[0x149] = 0x00;
    rom[0x14A] = 0x00;
    let ck = throwback::cartridge::gb_header_checksum(&rom).unwrap();
    rom[0x14D] = ck;
    let gck = throwback::cartridge::gb_global_checksum(&rom).unwrap();
    rom[0x14E..0x150].copy_from_slice(&gck.to_be_bytes());
    rom
}

fn open(sim: SimGbMemory) -> GbxCart<SimGbMemory> {
    GbxCart::new(sim).expect("handshake")
}

#[test]
fn detects_menu_state_cart() {
    let menu = make_menu_rom();
    let mut dev = open(SimGbMemory::new(&menu));
    dev.read_cartridge_info().unwrap();
    assert!(dev.is_gb_memory().unwrap(), "menu-state cart must be detected");
}

#[test]
fn detects_full_mode_cart_via_flash_id() {
    // A single-game cart: header looks like an ordinary game, so detection
    // must fall through to the flash-ID probe (the sim answers C2 89).
    let game = make_gb_rom(0x1B, 0x05, 0x03, "SOLOGAME");
    let mut dev = open(SimGbMemory::new(&game));
    dev.read_cartridge_info().unwrap();
    assert!(dev.is_gb_memory().unwrap(), "full-mode cart must be detected");
}

#[test]
fn multiboot_write_read_extract_roundtrip() {
    let menu = make_menu_rom();
    let games = vec![
        make_gb_rom(0x03, 0x03, 0x03, "GAME ONE"), // MBC1 256K 32K
        make_gb_rom(0x1B, 0x02, 0x02, "GAME TWO"), // MBC5 128K 8K
        make_gb_rom(0x13, 0x04, 0x03, "GAME THREE"), // MBC3 512K 32K
    ];
    let image = gbmemory::assemble(&menu, &games, None, gbmemory::Stamp::blank()).expect("assemble");

    let mut dev = open(SimGbMemory::new(&menu));
    dev.read_cartridge_info().unwrap();
    assert!(dev.is_gb_memory().unwrap());

    // Full device write: unlock -> chip-erase -> program 1 MB -> map -> reset
    // -> read-back verify (write_gb_memory returns Err on any mismatch).
    dev.write_gb_memory(&image.rom, &image.map, &|_| {}, &|_| {})
        .expect("write_gb_memory (includes read-back verify)");

    // Independent read-back + extraction.
    let (rom_back, map_back) = dev.read_gb_memory(&|_| {}).expect("read_gb_memory");
    assert_eq!(rom_back, image.rom, "flash image round-trips");
    assert_eq!(map_back, image.map.to_vec(), "map sector round-trips");

    let map_arr: [u8; gbmemory::MAP_SIZE] = map_back.try_into().unwrap();
    let extracted = gbmemory::extract_games(&rom_back, &map_arr);
    assert_eq!(extracted.len(), games.len(), "all games recovered");
    for (i, g) in games.iter().enumerate() {
        assert_eq!(&extracted[i].data, g, "game {i} byte-identical after round-trip");
    }
}

#[test]
fn full_mode_write_roundtrip() {
    let game = make_gb_rom(0x1B, 0x05, 0x03, "SOLOGAME"); // MBC5 1M 32K
    let image = gbmemory::assemble_full_mode(&game, None, gbmemory::Stamp::blank()).expect("assemble full");

    let mut dev = open(SimGbMemory::new(&game));
    dev.read_cartridge_info().unwrap();
    dev.write_gb_memory(&image.rom, &image.map, &|_| {}, &|_| {})
        .expect("full-mode write + verify");

    let (rom_back, map_back) = dev.read_gb_memory(&|_| {}).expect("read back");
    assert_eq!(rom_back, image.rom);
    let map_arr: [u8; gbmemory::MAP_SIZE] = map_back.try_into().unwrap();
    let extracted = gbmemory::extract_games(&rom_back, &map_arr);
    assert_eq!(extracted.len(), 1);
    assert_eq!(&extracted[0].data, &game);
}
