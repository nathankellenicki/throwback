//! End-to-end hardware tests for the GBxCart RW v1.4 backend.
//!
//! These require a real GBxCart RW v1.4-family device (L firmware 12+) on USB
//! with the named cartridge inserted, so they are `#[ignore]` by default:
//!
//!   cargo test --test e2e_gbxcart_tests -- --ignored --test-threads=1
//!   (or the `cargo e2e-gbx` alias)
//!
//! Golden hashes are produced by dumping the same cartridge with a GB Operator
//! (`throwback dump-rom` + `shasum -a 256`) — cross-device byte-equality is
//! the parity check. Fill in the constants below for the carts on hand before
//! running; a test with an empty golden skips itself.
//!
//! Ordering note: run the read-only tests (info, dump) before save round-trips,
//! and only run flash tests against a sacrificial flashcart.

use sha2::{Digest, Sha256};
use throwback::cartridge::{self, CartridgeType};
use throwback::device::gbxcart::GbxCart;
use throwback::device::{CartridgeDevice, ChipType};

/// SHA-256 of the cart's ROM as dumped by the GB Operator. Empty = skip.
const GB_CART_ROM_SHA256: &str = "";
/// Title expected from `info` for the same cart (e.g. "TETRIS").
const GB_CART_TITLE: &str = "TETRIS";

fn open_gbxcart() -> GbxCart {
    GbxCart::open_first()
        .expect("GBxCart RW v1.4 not found — is it plugged in (and running L firmware 12+)?")
}

fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[test]
#[ignore]
fn e2e_handshake_reports_v1_4_l_firmware() {
    let dev = open_gbxcart();
    eprintln!("PCB version byte: {}, firmware L{}", dev.pcb_ver, dev.fw_ver);
    assert!(dev.fw_ver >= 12);
}

#[test]
#[ignore]
fn e2e_info_detects_gb_cart() {
    let mut dev = open_gbxcart();
    let sig = dev.read_cartridge_info().expect("signature");
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert!(info.present, "no cartridge detected — is one inserted (clean contacts)?");
    assert_eq!(info.cart_type, CartridgeType::GB);

    let header = dev.read_header().expect("header read");
    let title = cartridge::parse_gb_title(&header).expect("title");
    eprintln!("Detected: {title} ({}, ROM {} bytes, RAM {} bytes)", info.mbc_name(), info.rom_size, info.ram_size);
    assert_eq!(title, GB_CART_TITLE);
}

#[test]
#[ignore]
fn e2e_dump_rom_matches_operator_golden() {
    if GB_CART_ROM_SHA256.is_empty() {
        eprintln!("skipping: set GB_CART_ROM_SHA256 to the GB Operator dump hash first");
        return;
    }
    let mut dev = open_gbxcart();
    let sig = dev.read_cartridge_info().expect("signature");
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert!(info.present);

    let rom = dev
        .read_rom(ChipType::Unknown, info.rom_size, info.ram_size, &|_| {})
        .expect("ROM dump");
    assert_eq!(
        sha256(&rom),
        GB_CART_ROM_SHA256,
        "GBxCart dump differs from the GB Operator dump of the same cart"
    );
}

#[test]
#[ignore]
fn e2e_save_write_roundtrip() {
    // SAFE: reads the save, writes the identical bytes back, reads again.
    let mut dev = open_gbxcart();
    let sig = dev.read_cartridge_info().expect("signature");
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert!(info.present);
    assert!(info.ram_size > 0, "insert a battery-backed cart for this test");

    let save1 = dev
        .read_save(ChipType::Unknown, info.rom_size, info.ram_size, &|_| {})
        .expect("first save read");
    dev.write_save(ChipType::Unknown, info.rom_size, &save1, &|_| {})
        .expect("save write-back");
    let save2 = dev
        .read_save(ChipType::Unknown, info.rom_size, info.ram_size, &|_| {})
        .expect("second save read");
    assert_eq!(save1, save2, "save changed across a write-back round-trip");
}

#[test]
#[ignore]
fn e2e_rtc_read_parses() {
    // Requires an MBC3+RTC cart (e.g. Pokemon Crystal).
    let mut dev = open_gbxcart();
    let sig = dev.read_cartridge_info().expect("signature");
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert!(info.has_rtc(), "insert an MBC3+Timer cart (Pokemon Gold/Silver/Crystal)");

    let payload = dev.read_rtc(info.rom_size, info.ram_size).expect("RTC read");
    let rtc = cartridge::RtcData::parse(&payload).expect("RTC parse");
    eprintln!("RTC: {rtc}");
}

#[test]
#[ignore]
fn e2e_flashcart_detection_refuses_retail() {
    // Run this with a RETAIL cart inserted: it must NOT read as writeable.
    let mut dev = open_gbxcart();
    dev.read_cartridge_info().expect("signature");
    let packet = dev.detect_flashcart().expect("probe");
    eprintln!("detect packet: {:02X?}", &packet[..12]);
    assert!(
        !cartridge::flashcart_writeable(&packet),
        "retail cart misdetected as a writeable flashcart!"
    );
}

#[test]
#[ignore]
fn e2e_flashcart_detection_recognizes_flashcart() {
    // Run this with the sacrificial flashcart inserted. If it fails, note the
    // printed raw ID and add a profile for it in device/gbxcart/flash.rs.
    let mut dev = open_gbxcart();
    dev.read_cartridge_info().expect("signature");
    let packet = dev.detect_flashcart().expect("probe");
    eprintln!("detect packet: {:02X?}", &packet[..12]);
    assert!(
        cartridge::flashcart_writeable(&packet),
        "flashcart not recognized — its flash ID is printed above; add a profile"
    );
}
