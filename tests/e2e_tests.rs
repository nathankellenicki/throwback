//! End-to-end tests — require an Operator with a specific cartridge inserted.
//! GB tests need a GB Operator + Pokemon Yellow; the SNES test needs an SN
//! Operator + Desert Strike (Europe).
//! Run with: cargo test --test e2e_tests -- --ignored

use throwback::cartridge::{parse_snes_header, CartridgeInfo, CartridgeType, SnesMapper};
use throwback::device::{CartridgeDevice, ChipType, LegacyDevice};
use sha2::{Digest, Sha256};

const POKEMON_YELLOW_ROM_SHA256: &str =
    "8cbaa499397e4f1a679c992ea9382a2dd7942ab398b48c19829c2d9529de47bf";

// Desert Strike: Return to the Gulf (Europe), 1 MB LoROM. Byte-for-byte verified
// against a reference dump.
const DESERT_STRIKE_ROM_SHA256: &str =
    "793d755dd2e43ee4e36434edec2983f19545403eb848a54a14cac6f492033a52";

fn open_device() -> LegacyDevice {
    LegacyDevice::open().expect("GB Operator not found — is it plugged in with Pokemon Yellow?")
}

#[test]
#[ignore]
fn e2e_dump_rom_matches_known_hash() {
    let mut device = open_device();
    let sig = device
        .read_cartridge_info()
        .expect("Failed to read cartridge info");

    // Verify it's a GB cart
    assert_eq!(sig[2], 0x20, "Expected GB cart (0x20), got 0x{:02X}", sig[2]);

    // ROM size code 5 = 1MB
    let rom_size_code = sig[0x0F];
    let rom_size = 32 * 1024 * (1u32 << rom_size_code);
    let ram_size_code = sig[0x10];
    let ram_size = match ram_size_code {
        0 => 0,
        1 => 2048,
        2 => 8192,
        3 => 32768,
        4 => 131072,
        5 => 65536,
        _ => 0,
    };

    let rom = device
        .read_rom(ChipType::Unknown, rom_size, ram_size, &|_| {})
        .expect("Failed to dump ROM");

    assert_eq!(rom.len(), rom_size as usize);

    let hash = hex::encode(Sha256::digest(&rom));
    assert_eq!(
        hash, POKEMON_YELLOW_ROM_SHA256,
        "ROM hash mismatch — is Pokemon Yellow inserted?"
    );
}

#[test]
#[ignore]
fn e2e_snes_dump_rom_matches_known_hash() {
    // Uses the PID-dispatching factory so it picks the SN Operator's streaming path.
    let mut device =
        throwback::device::open().expect("Operator not found — is the SN Operator plugged in?");

    let sig = device
        .read_cartridge_info()
        .expect("Failed to read cartridge info");

    let info = CartridgeInfo::from_bytes(&sig);
    assert_eq!(
        info.cart_type,
        CartridgeType::SNES,
        "Expected SNES cart (signature byte[2]=0x{:02X})",
        sig[2]
    );
    assert_eq!(info.rom_size, 1024 * 1024, "Expected Desert Strike (1 MB)");

    let rom = device
        .read_rom(ChipType::Unknown, info.rom_size, 0, &|_| {})
        .expect("Failed to dump SNES ROM");
    assert_eq!(rom.len(), info.rom_size as usize);

    // The dumped header should parse cleanly and self-identify.
    let header = parse_snes_header(&rom).expect("dumped ROM has no valid SNES header");
    assert_eq!(header.mapper, SnesMapper::LoRom);
    assert_eq!(header.rom_size, 1024 * 1024);
    assert!(
        header.title.starts_with("Desert Strike"),
        "unexpected title: {:?}",
        header.title
    );

    let hash = hex::encode(Sha256::digest(&rom));
    assert_eq!(
        hash, DESERT_STRIKE_ROM_SHA256,
        "ROM hash mismatch — is Desert Strike (Europe) inserted?"
    );
}

#[test]
#[ignore]
fn e2e_save_write_roundtrip() {
    let mut device = open_device();
    let sig = device
        .read_cartridge_info()
        .expect("Failed to read cartridge info");

    assert_eq!(sig[2], 0x20, "Expected GB cart");

    let rom_size_code = sig[0x0F];
    let rom_size = 32 * 1024 * (1u32 << rom_size_code);
    let ram_size_code = sig[0x10];
    let ram_size = match ram_size_code {
        0 => 0,
        1 => 2048,
        2 => 8192,
        3 => 32768,
        4 => 131072,
        5 => 65536,
        _ => 0,
    };

    assert!(ram_size > 0, "Cart has no save RAM");

    // Read save
    let save1 = device
        .read_save(ChipType::Unknown, rom_size, ram_size, &|_| {})
        .expect("Failed to read save (first read)");
    assert_eq!(save1.len(), ram_size as usize);

    // Write it back
    device
        .write_save(ChipType::Unknown, rom_size, &save1, &|_| {})
        .expect("Failed to write save");

    // Reopen device to get a clean serial state after write
    drop(device);
    std::thread::sleep(std::time::Duration::from_secs(1));
    let mut device = open_device();

    // Read again
    let save2 = device
        .read_save(ChipType::Unknown, rom_size, ram_size, &|_| {})
        .expect("Failed to read save (second read)");

    assert_eq!(save1, save2, "Save data changed after write roundtrip");
}
