//! Flash-cart detection, erase, program, and verify tests against the
//! simulator's AMD flash state machine. The chip only erases through a correct
//! unlock sequence and programming is AND-semantics, so a driver that skips or
//! misorders erase produces wrong data — these tests can't pass by accident.

mod gbxcart_sim;

use std::cell::RefCell;

use gbxcart_sim::*;
use throwback::cartridge;
use throwback::device::gbxcart::{GbxCart, MbcKind};
use throwback::device::{CartridgeDevice, DeviceError};

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u8).wrapping_mul(31) ^ (i >> 8) as u8).wrapping_add(seed))
        .collect()
}

/// A DMG flashcart: MBC5 wiring, an AM29F016-style chip (2 MB, 64 KB sectors,
/// 0x555/0x2AA unlock) preloaded with an old game so detection works.
fn dmg_flashcart(old_rom: Vec<u8>) -> SimGbxCart {
    let mut cart = SimDmgCart::new(SimMbc::Mbc5, old_rom.clone(), 0x8000);
    let mut chip = SimFlashChip::new(2 * 1024 * 1024, &[0x01, 0xAD], (0x555, 0x2AA), 0x10000);
    chip.data[..old_rom.len()].copy_from_slice(&old_rom);
    cart.flash = Some(chip);
    SimGbxCart::new(SimCart::Dmg(cart))
}

/// An AGB flashcart: S29GL-style chip (unlock at word 0x555/0x2AA; the chip
/// model is byte-addressed, so 0xAAA/0x554) preloaded with an old game.
fn agb_flashcart(old_rom: Vec<u8>, size: usize) -> SimGbxCart {
    let mut chip = SimFlashChip::new(size, &[0x01, 0x00, 0x7E, 0x22], (0xAAA, 0x554), 0x20000);
    chip.data[..old_rom.len()].copy_from_slice(&old_rom);
    let mut cart = SimAgbCart::new(Vec::new(), SimAgbSave::None);
    cart.flash = Some(chip);
    SimGbxCart::new(SimCart::Agb(cart))
}

fn open(sim: SimGbxCart) -> GbxCart<SimGbxCart> {
    let mut dev = GbxCart::new(sim).expect("handshake");
    dev.read_cartridge_info().expect("detection");
    dev
}

// --- Detection ----------------------------------------------------------------

#[test]
fn flashcart_detected_as_writeable() {
    let old = make_gb_rom(0x19, 0x04, 0x03, "OLDGAME");
    let mut dev = open(dmg_flashcart(old));
    let packet = dev.detect_flashcart().unwrap();
    assert!(cartridge::flashcart_writeable(&packet));
    assert_eq!(packet[0], 0x21, "GB family marker with the flashable bit");
    assert_eq!(&packet[1..3], &[0x01, 0xAD], "raw flash ID embedded for debugging");
}

#[test]
fn retail_cart_detected_as_not_writeable() {
    let rom = make_gb_rom(0x10, 0x06, 0x03, "RETAIL");
    let cart = SimDmgCart::new(SimMbc::Mbc3, rom, 0x8000);
    let mut dev = open(SimGbxCart::new(SimCart::Dmg(cart)));
    let packet = dev.detect_flashcart().unwrap();
    assert!(!cartridge::flashcart_writeable(&packet));
    assert_eq!(packet[0], 0x20);
    assert!(packet[1..].iter().all(|&b| b == 0));
}

#[test]
fn unknown_flash_chip_is_not_writeable_and_refuses_write() {
    let old = make_gb_rom(0x19, 0x04, 0x00, "ODDCHIP");
    let mut sim = dmg_flashcart(old.clone());
    if let SimCart::Dmg(cart) = &mut sim.cart {
        cart.flash.as_mut().unwrap().id = vec![0x99, 0x99]; // not in the profile table
    }
    let mut dev = open(sim);
    let packet = dev.detect_flashcart().unwrap();
    assert!(!cartridge::flashcart_writeable(&packet));

    // write_rom refuses on its own — even a --force detect-bypass never
    // sends erase/program to an unidentified chip.
    let result = dev.write_rom(&old, 0, &|_| {}, &|_| {});
    assert!(matches!(result, Err(DeviceError::NotFlashable(_))));
}

#[test]
fn agb_flashcart_detected_as_writeable() {
    let old = make_gba_rom("OLDAGB", "BOLD", b"", 1 << 20);
    let mut dev = open(agb_flashcart(old, 4 << 20));
    let packet = dev.detect_flashcart().unwrap();
    assert!(cartridge::flashcart_writeable(&packet));
    assert_eq!(packet[0], 0x31);
}

// --- DMG write path -----------------------------------------------------------

#[test]
fn dmg_write_rom_erases_programs_and_verifies() {
    let old = make_gb_rom(0x19, 0x04, 0x03, "OLDGAME");
    let new = make_gb_rom(0x19, 0x05, 0x03, "NEWGAME"); // 1 MB
    let mut dev = open(dmg_flashcart(old));

    let messages = RefCell::new(Vec::new());
    dev.write_rom(&new, 0, &|_| {}, &|m| messages.borrow_mut().push(m.to_string()))
        .unwrap();

    match &dev.transport().cart {
        SimCart::Dmg(cart) => {
            let flash = cart.flash.as_ref().unwrap();
            assert_eq!(&flash.data[..new.len()], &new[..], "programmed image");
            // 1 MB over 64 KB sectors = 16 sector erases, in order.
            assert_eq!(flash.erased_sectors.len(), 16);
            assert!(!flash.chip_erased);
        }
        _ => unreachable!(),
    }

    let msgs = messages.borrow();
    for expected in ["Preparing cartridge...", "Erasing flash...", "Writing...", "Verifying..."] {
        assert!(msgs.iter().any(|m| m == expected), "missing progress message {expected:?}");
    }
}

#[test]
fn dmg_write_rom_skips_ff_chunks() {
    let old = make_gb_rom(0x19, 0x04, 0x00, "OLDGAME");
    // A 512 KB image whose second half is all 0xFF.
    let mut new = make_gb_rom(0x19, 0x04, 0x00, "SPARSE");
    for b in &mut new[0x40000..] {
        *b = 0xFF;
    }
    let mut dev = open(dmg_flashcart(old));
    dev.write_rom(&new, 0, &|_| {}, &|_| {}).unwrap();

    let sim = dev.transport();
    // 512 KB in 0x100 chunks would be 2048 program commands; the FF half must
    // have been skipped entirely.
    assert!(sim.program_chunks <= 1024, "FF chunks were not skipped: {}", sim.program_chunks);
    match &sim.cart {
        SimCart::Dmg(cart) => {
            assert_eq!(&cart.flash.as_ref().unwrap().data[..new.len()], &new[..]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn dmg_write_rom_detects_corruption_on_verify() {
    let old = make_gb_rom(0x19, 0x04, 0x00, "OLDGAME");
    let new = make_gb_rom(0x19, 0x04, 0x00, "BADLUCK");
    let mut sim = dmg_flashcart(old);
    sim.corrupt_flash_after_write = true;
    let mut dev = open(sim);

    let result = dev.write_rom(&new, 0, &|_| {}, &|_| {});
    assert!(
        matches!(result, Err(DeviceError::Protocol(ref m)) if m.contains("verification")),
        "expected verification failure, got {result:?}"
    );
}

#[test]
fn dmg_write_rom_too_large_is_refused() {
    let old = make_gb_rom(0x19, 0x04, 0x00, "OLDGAME");
    let huge = vec![0xA0; 4 * 1024 * 1024]; // 4 MB onto a 2 MB profile
    let mut dev = open(dmg_flashcart(old));
    let result = dev.write_rom(&huge, 0, &|_| {}, &|_| {});
    assert!(matches!(result, Err(DeviceError::NotFlashable(_))));
}

// --- AGB write path (buffered, continue-mode acks) ----------------------------

#[test]
fn agb_write_rom_buffered_programs_and_crc_verifies() {
    let old = make_gba_rom("OLDAGB", "BOLD", b"", 1 << 20);
    let new = make_gba_rom("NEWAGB", "BNEW", b"", 1 << 20);
    let mut dev = open(agb_flashcart(old, 4 << 20));

    dev.write_rom(&new, 0, &|_| {}, &|_| {}).unwrap();

    match &dev.transport().cart {
        SimCart::Agb(cart) => {
            let flash = cart.flash.as_ref().unwrap();
            assert_eq!(&flash.data[..new.len()], &new[..]);
            // 1 MB over 128 KB sectors = 8 erases.
            assert_eq!(flash.erased_sectors.len(), 8);
        }
        _ => unreachable!(),
    }
}

#[test]
fn agb_write_rom_detects_corruption_via_crc() {
    let old = make_gba_rom("OLDAGB", "BOLD", b"", 1 << 20);
    let new = make_gba_rom("NEWAGB", "BNEW", b"", 1 << 20);
    let mut sim = agb_flashcart(old, 4 << 20);
    sim.corrupt_flash_after_write = true;
    let mut dev = open(sim);

    let result = dev.write_rom(&new, 0, &|_| {}, &|_| {});
    assert!(
        matches!(result, Err(DeviceError::Protocol(ref m)) if m.contains("verification")),
        "expected CRC failure, got {result:?}"
    );
}

// --- Erase state machine ------------------------------------------------------

#[test]
fn erase_requires_full_unlock_sequence() {
    // Feed the sim chip a *wrong* sequence directly and confirm nothing
    // erases — establishing the FSM actually guards the tests above.
    let mut chip = SimFlashChip::new(0x20000, &[0x01, 0xAD], (0x555, 0x2AA), 0x10000);
    chip.command(0x555, 0xAA);
    chip.command(0x2AA, 0x55);
    chip.command(0x555, 0x30); // sector erase without the 0x80 arm
    assert!(chip.erased_sectors.is_empty());
    assert!(chip.data.iter().all(|&b| b == 0xA5), "data untouched");

    // And the correct sequence does erase.
    chip.command(0x555, 0xAA);
    chip.command(0x2AA, 0x55);
    chip.command(0x555, 0x80);
    chip.command(0x555, 0xAA);
    chip.command(0x2AA, 0x55);
    chip.command(0x10000, 0x30);
    assert_eq!(chip.erased_sectors, vec![0x10000]);
    assert!(chip.data[0x10000..].iter().all(|&b| b == 0xFF));
    assert!(chip.data[..0x10000].iter().all(|&b| b == 0xA5));
}

// --- Pure helpers -------------------------------------------------------------

#[test]
fn mbc_kind_exposed_for_flash_banking() {
    // write_rom always drives MBC5-style banking on DMG flashcarts.
    assert_eq!(MbcKind::from_header_byte(0x19), MbcKind::Mbc5);
}

#[test]
fn sim_pattern_is_aperiodic() {
    let p = pattern(8192, 0);
    assert_ne!(&p[..512], &p[512..1024], "fixture must not look mirrored");
}
