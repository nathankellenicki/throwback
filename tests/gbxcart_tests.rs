//! GBxCart backend integration tests against the behavioral device simulator
//! (tests/gbxcart_sim). No hardware involved: the simulator enforces protocol
//! and mapper semantics, so these tests prove the driver's sequencing —
//! banking, access modes, RTC latching, EEPROM addressing — not just its byte
//! formatting.

mod gbxcart_sim;

use std::time::Duration;

use gbxcart_sim::*;
use throwback::cartridge::{self, CartridgeType};
use throwback::device::gbxcart::{GbxCart, Transport};
use throwback::device::{CartridgeDevice, ChipType, DeviceError};

const GBA_MAX_ROM: u32 = 32 * 1024 * 1024;

fn open(sim: SimGbxCart) -> GbxCart<SimGbxCart> {
    GbxCart::new(sim).expect("handshake against the simulator")
}

fn dmg_device(mbc: SimMbc, rom: Vec<u8>, ram_size: usize) -> GbxCart<SimGbxCart> {
    open(SimGbxCart::new(SimCart::Dmg(SimDmgCart::new(mbc, rom, ram_size))))
}

fn agb_device(rom: Vec<u8>, save: SimAgbSave) -> GbxCart<SimGbxCart> {
    open(SimGbxCart::new(SimCart::Agb(SimAgbCart::new(rom, save))))
}

// --- Handshake ---------------------------------------------------------------

#[test]
fn handshake_accepts_l_firmware() {
    let dev = open(SimGbxCart::new(SimCart::Empty));
    assert_eq!(dev.fw_ver, 15);
    assert_eq!(dev.pcb_ver, 6);
}

#[test]
fn handshake_rejects_unsupported_pcb() {
    let mut sim = SimGbxCart::new(SimCart::Empty);
    sim.pcb_ver = 2; // v1.1/v1.2 — pre-v1.4 board
    assert!(matches!(GbxCart::new(sim), Err(DeviceError::Protocol(_))));
}

#[test]
fn handshake_rejects_non_l_firmware() {
    let mut sim = SimGbxCart::new(SimCart::Empty);
    sim.cfw_id = b'R';
    assert!(matches!(GbxCart::new(sim), Err(DeviceError::Protocol(_))));
}

#[test]
fn handshake_rejects_old_l_firmware() {
    let mut sim = SimGbxCart::new(SimCart::Empty);
    sim.fw_ver = 11;
    assert!(matches!(GbxCart::new(sim), Err(DeviceError::Protocol(_))));
}

/// A port whose device never answers (an Arduino, a USB adapter...).
struct SilentPort;
impl Transport for SilentPort {
    fn write_all(&mut self, _buf: &[u8]) -> Result<(), DeviceError> {
        Ok(())
    }
    fn read_exact(&mut self, _buf: &mut [u8]) -> Result<(), DeviceError> {
        Err(DeviceError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "silence",
        )))
    }
    fn set_timeout(&mut self, _t: Duration) -> Result<(), DeviceError> {
        Ok(())
    }
    fn flush_input(&mut self) {}
}

#[test]
fn handshake_rejects_silent_ch340_device() {
    assert!(GbxCart::new(SilentPort).is_err());
}

// --- Detection & signature synthesis -----------------------------------------

#[test]
fn detects_dmg_cart_with_operator_compatible_signature() {
    let rom = make_gb_rom(0x03, 0x02, 0x02, "MBC1GAME");
    let mut dev = dmg_device(SimMbc::Mbc1, rom.clone(), 0x2000);

    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert!(info.present);
    assert_eq!(info.cart_type, CartridgeType::GB);
    assert_eq!(info.rom_size, 128 * 1024);
    assert_eq!(info.ram_size, 8 * 1024);
    assert_eq!(info.mbc_type, 0x03);
    assert_eq!(info.title_char, 'M');
    assert_eq!(info.header_checksum, rom[0x14D]);
    assert_eq!(
        info.global_checksum,
        u16::from_be_bytes([rom[0x14E], rom[0x14F]])
    );
}

#[test]
fn detects_agb_cart_with_operator_compatible_signature() {
    let rom = make_gba_rom("AGBGAME", "BXYZ", b"SRAM_V113", 1 << 20);
    let mut dev = agb_device(rom, SimAgbSave::Sram(vec![0; 0x8000]));

    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert!(info.present);
    assert_eq!(info.cart_type, CartridgeType::GBA);
    assert_eq!(&info.game_code, b"BXY");
    assert_eq!(info.region, b'Z');
    assert_eq!(info.title_char, 'A');
    // Operator parity: sizes come from a dump + trim, not the signature.
    assert_eq!(info.rom_size, 0);
    assert_eq!(info.game_id(), "ABXY");
}

#[test]
fn empty_slot_reads_as_not_present() {
    let mut dev = open(SimGbxCart::new(SimCart::Empty));
    let sig = dev.read_cartridge_info().unwrap();
    assert!(!cartridge::CartridgeInfo::from_bytes(&sig).present);
}

#[test]
fn dirty_cart_reads_as_not_present() {
    let mut rom = make_gb_rom(0x00, 0x00, 0x00, "TETRIS");
    rom[0x14D] ^= 0xFF; // corrupted header checksum, like dirty contacts
    let mut dev = dmg_device(SimMbc::None, rom, 0);
    let sig = dev.read_cartridge_info().unwrap();
    assert!(!cartridge::CartridgeInfo::from_bytes(&sig).present);
}

#[test]
fn agb_cart_is_never_probed_at_5v() {
    let rom = make_gba_rom("SAFEGAME", "BSAF", b"SRAM_V113", 1 << 20);
    let mut dev = agb_device(rom, SimAgbSave::None);
    dev.read_cartridge_info().unwrap();
    // Safety property: a GBA cart must never see 5 V.
    assert!(!dev.transport().log.contains(&Event::Volt5));
}

#[test]
fn dmg_probe_happens_after_agb_probe_rejects() {
    let rom = make_gb_rom(0x00, 0x00, 0x00, "TETRIS");
    let mut dev = dmg_device(SimMbc::None, rom, 0);
    dev.read_cartridge_info().unwrap();
    let log = &dev.transport().log;
    let first_33 = log.iter().position(|e| *e == Event::Volt33).unwrap();
    let first_5 = log.iter().position(|e| *e == Event::Volt5).unwrap();
    assert!(first_33 < first_5, "3.3 V AGB probe must come before any 5 V");
}

#[test]
fn read_header_covers_full_titles() {
    let rom = make_gb_rom(0x00, 0x00, 0x00, "TITLETEST");
    let mut dev = dmg_device(SimMbc::None, rom, 0);
    dev.read_cartridge_info().unwrap();
    let header = dev.read_header().unwrap();
    assert_eq!(header.len(), 0x4000);
    assert_eq!(cartridge::parse_gb_title(&header).as_deref(), Some("TITLETEST"));

    let rom = make_gba_rom("LONGTITLE", "BABC", b"", 1 << 20);
    let mut dev = agb_device(rom, SimAgbSave::None);
    dev.read_cartridge_info().unwrap();
    let header = dev.read_header().unwrap();
    assert_eq!(cartridge::parse_gba_title(&header).as_deref(), Some("LONGTITLE"));
}

#[test]
fn mbc2_reports_shim_ram_size() {
    let rom = make_gb_rom(0x06, 0x02, 0x00, "MBC2GAME");
    let mut dev = dmg_device(SimMbc::Mbc2, rom, 0x200);
    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    // Header says no RAM; the cart really has 512 half-bytes. The synthesized
    // signature reports 2 KB so save flows engage.
    assert_eq!(info.ram_size, 2 * 1024);
}

// --- DMG ROM dumps (per mapper; equality proves banking) ----------------------

fn dump_roundtrip(mbc_byte: u8, sim_mbc: SimMbc, rom_code: u8, ram_code: u8) {
    let rom = make_gb_rom(mbc_byte, rom_code, ram_code, "DUMPTEST");
    let mut dev = dmg_device(sim_mbc, rom.clone(), 0);
    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert_eq!(info.rom_size as usize, rom.len());
    let dump = dev
        .read_rom(ChipType::Unknown, info.rom_size, 0, &|_| {})
        .unwrap();
    assert_eq!(dump, rom, "dump must be byte-identical across all banks");
}

#[test]
fn dump_rom_no_mbc_32k() {
    dump_roundtrip(0x00, SimMbc::None, 0x00, 0x00);
}

#[test]
fn dump_rom_mbc1_512k() {
    dump_roundtrip(0x03, SimMbc::Mbc1, 0x04, 0x00);
}

#[test]
fn dump_rom_mbc2_256k() {
    dump_roundtrip(0x06, SimMbc::Mbc2, 0x03, 0x00);
}

#[test]
fn dump_rom_mbc3_2m() {
    dump_roundtrip(0x10, SimMbc::Mbc3, 0x06, 0x03);
}

#[test]
fn dump_rom_mbc30_4m() {
    // MBC3 header byte with 4 MB ROM -> MBC30 (8-bit banking).
    dump_roundtrip(0x10, SimMbc::Mbc3, 0x07, 0x05);
}

#[test]
fn dump_rom_mbc5_8m_uses_ninth_bank_bit() {
    dump_roundtrip(0x19, SimMbc::Mbc5, 0x08, 0x00);
}

#[test]
fn dump_rom_camera_1m() {
    dump_roundtrip(0xFC, SimMbc::Camera, 0x05, 0x04);
}

#[test]
fn dump_rom_mbc1_multicart_via_duplicate_logo_probe() {
    let mut rom = make_gb_rom(0x01, 0x05, 0x00, "MULTICART");
    // MBC1M multicarts repeat the header logo at the start of each 256 KB
    // sub-game; the probe looks for it in "bank 0x10".
    let logo: Vec<u8> = rom[0x104..0x134].to_vec();
    rom[0x40104..0x40134].copy_from_slice(&logo);
    let mut dev = dmg_device(SimMbc::Mbc1M, rom.clone(), 0);
    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    let dump = dev
        .read_rom(ChipType::Unknown, info.rom_size, 0, &|_| {})
        .unwrap();
    assert_eq!(dump, rom, "MBC1M dump must follow multicart wiring");
}

// --- DMG saves ----------------------------------------------------------------

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    // Aperiodic over any power-of-two window (mixes the high address byte),
    // so mirror-detection heuristics can't misread it as a repeated block.
    (0..len)
        .map(|i| ((i as u8).wrapping_mul(31) ^ (i >> 8) as u8).wrapping_add(seed))
        .collect()
}

fn dmg_save_roundtrip(mbc_byte: u8, sim_mbc: SimMbc, ram_code: u8, ram_len: usize) {
    let rom = make_gb_rom(mbc_byte, 0x05, ram_code, "SAVETEST");
    let mut dev = dmg_device(sim_mbc, rom, ram_len);
    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);
    assert_eq!(info.ram_size as usize, ram_len);

    let data = pattern(ram_len, 7);
    dev.write_save(ChipType::Unknown, info.rom_size, &data, &|_| {})
        .unwrap();
    let read = dev
        .read_save(ChipType::Unknown, info.rom_size, info.ram_size, &|_| {})
        .unwrap();
    assert_eq!(read, data);

    // Battery-cart safety: RAM must be disabled again after every save op.
    match &dev.transport().cart {
        SimCart::Dmg(cart) => assert!(!cart.ram_enabled(), "RAM left enabled after save op"),
        _ => unreachable!(),
    }
}

#[test]
fn save_roundtrip_mbc1_32k() {
    dmg_save_roundtrip(0x03, SimMbc::Mbc1, 0x03, 0x8000);
}

#[test]
fn save_roundtrip_mbc3_32k() {
    dmg_save_roundtrip(0x10, SimMbc::Mbc3, 0x03, 0x8000);
}

#[test]
fn save_roundtrip_mbc5_8k() {
    dmg_save_roundtrip(0x1B, SimMbc::Mbc5, 0x02, 0x2000);
}

#[test]
fn save_roundtrip_camera_128k() {
    dmg_save_roundtrip(0xFC, SimMbc::Camera, 0x04, 0x20000);
}

#[test]
fn save_mbc2_nibble_ram() {
    let rom = make_gb_rom(0x06, 0x02, 0x00, "MBC2SAVE");
    let mut dev = dmg_device(SimMbc::Mbc2, rom, 0x200);
    let sig = dev.read_cartridge_info().unwrap();
    let info = cartridge::CartridgeInfo::from_bytes(&sig);

    // Write the 2 KB shim payload; only the first 512 bytes are real, and the
    // chip stores only low nibbles.
    let data = pattern(0x800, 3);
    dev.write_save(ChipType::Unknown, info.rom_size, &data, &|_| {})
        .unwrap();
    let read = dev
        .read_save(ChipType::Unknown, info.rom_size, info.ram_size, &|_| {})
        .unwrap();
    assert_eq!(read.len(), 0x800);
    for i in 0..0x200 {
        assert_eq!(read[i], data[i] & 0x0F | 0xF0, "nibble RAM byte {i}");
    }
    assert!(read[0x200..].iter().all(|&b| b == 0xFF), "shim padding");
}

// --- MBC3 RTC -----------------------------------------------------------------

#[test]
fn rtc_read_reports_latched_registers() {
    let rom = make_gb_rom(0x10, 0x06, 0x03, "PKMNTEST");
    let mut sim_cart = SimDmgCart::new(SimMbc::Mbc3, rom, 0x8000);
    sim_cart.rtc = [30, 45, 12, 100, 0x01]; // day 356 (bit 8 set)
    let mut dev = open(SimGbxCart::new(SimCart::Dmg(sim_cart)));
    dev.read_cartridge_info().unwrap();

    let payload = dev.read_rtc(0, 0).unwrap();
    assert_eq!(payload.len(), 40);
    // Both halves carry the same latched snapshot.
    assert_eq!(payload[..20], payload[20..]);
    let rtc = cartridge::RtcData::parse(&payload).unwrap();
    assert_eq!(rtc.seconds, 30);
    assert_eq!(rtc.minutes, 45);
    assert_eq!(rtc.hours, 12);
    assert_eq!(rtc.days, 356);
}

#[test]
fn rtc_write_sets_registers_and_halts_during_update() {
    let rom = make_gb_rom(0x10, 0x06, 0x03, "PKMNTEST");
    let mut dev = dmg_device(SimMbc::Mbc3, rom, 0x8000);
    dev.read_cartridge_info().unwrap();

    let payload = cartridge::RtcData {
        seconds: 11,
        minutes: 22,
        hours: 3,
        days: 300,
        halt: false,
        day_carry: false,
    }
    .to_payload();
    dev.write_rtc(0, 0, &payload).unwrap();

    match &dev.transport().cart {
        SimCart::Dmg(cart) => {
            assert_eq!(cart.rtc[0], 11);
            assert_eq!(cart.rtc[1], 22);
            assert_eq!(cart.rtc[2], 3);
            assert_eq!(cart.rtc[3], 300u16.to_le_bytes()[0]);
            assert_eq!(cart.rtc[4] & 0x01, 1); // day bit 8
            assert_eq!(cart.rtc[4] & 0x40, 0, "halt must be released at the end");
            assert!(!cart.ram_enabled());
        }
        _ => unreachable!(),
    }

    // Read-back through the driver agrees.
    let back = cartridge::RtcData::parse(&dev.read_rtc(0, 0).unwrap()).unwrap();
    assert_eq!(back.days, 300);
    assert_eq!(back.seconds, 11);
}

// --- AGB ----------------------------------------------------------------------

#[test]
fn agb_rom_dump_stops_at_open_bus_and_trims() {
    let rom = make_gba_rom("TRIMTEST", "BTRM", b"SRAM_V113", 4 << 20);
    let mut dev = agb_device(rom.clone(), SimAgbSave::None);
    dev.read_cartridge_info().unwrap();

    let dump = dev.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, &|_| {}).unwrap();
    assert_eq!(dump.len(), 4 << 20, "dump must stop at the open-bus boundary");
    assert_eq!(dump, rom);
    assert_eq!(cartridge::trim_gba_rom(&dump), 4 << 20);
}

#[test]
fn agb_zero_padded_cart_reads_through_16mb_and_trims() {
    // Hardware scenario (Advance Wars 2 on a v1.4a): the cart pads past its
    // ROM with 0x00 — not open bus — so the dump must read the full 32 MB,
    // survive the firmware's 16 MB auto-increment stall (by re-anchoring the
    // address register), and rely on trim_gba_rom's uniform-tail detection.
    let rom = make_gba_rom("PADDED", "BPAD", b"FLASH_V102", 4 << 20);
    let mut cart = SimAgbCart::new(rom.clone(), SimAgbSave::None);
    cart.pad = Some(0x00);
    let mut dev = open(SimGbxCart::new(SimCart::Agb(cart)));
    dev.read_cartridge_info().unwrap();

    let dump = dev.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, &|_| {}).unwrap();
    assert_eq!(dump.len(), GBA_MAX_ROM as usize, "no early exit on constant padding");
    assert_eq!(&dump[..rom.len()], &rom[..]);
    assert!(dump[rom.len()..].iter().all(|&b| b == 0x00));
    assert_eq!(cartridge::trim_gba_rom(&dump), 4 << 20, "uniform tail must trim");
}

#[test]
fn agb_sram_roundtrip() {
    let rom = make_gba_rom("SRAMTEST", "BSRM", b"SRAM_V113", 1 << 20);
    let mut dev = agb_device(rom, SimAgbSave::Sram(pattern(0x8000, 9)));
    dev.read_cartridge_info().unwrap();

    let save = dev.read_save(ChipType::Sram, 0, 0x8000, &|_| {}).unwrap();
    assert_eq!(save, pattern(0x8000, 9));

    let new = pattern(0x8000, 42);
    dev.write_save(ChipType::Sram, 0, &new, &|_| {}).unwrap();
    let back = dev.read_save(ChipType::Sram, 0, 0x8000, &|_| {}).unwrap();
    assert_eq!(back, new);
}

#[test]
fn agb_eeprom_64k_reads_directly() {
    let rom = make_gba_rom("EEPTEST", "BEEP", b"EEPROM_V124", 1 << 20);
    let data = pattern(8192, 5);
    let mut dev = agb_device(rom, SimAgbSave::Eeprom { size: 8192, data: data.clone() });
    dev.read_cartridge_info().unwrap();

    let save = dev.read_save(ChipType::Eeprom, 0, 8192, &|_| {}).unwrap();
    assert_eq!(save, data);
    // A real 64 Kbit part is not mirrored, so the trim keeps all 8 KB.
    assert_eq!(cartridge::detect_eeprom_size(&save), data);
}

#[test]
fn agb_eeprom_4k_shim_produces_mirrors_for_trim() {
    let rom = make_gba_rom("EEPSMALL", "BEPS", b"EEPROM_V124", 1 << 20);
    let data = pattern(512, 77);
    let mut dev = agb_device(rom, SimAgbSave::Eeprom { size: 512, data: data.clone() });
    dev.read_cartridge_info().unwrap();

    // main.rs always asks for 8 KB; the shim detects the 4 Kbit part (whose
    // 64 Kbit-addressed read is garbage, not mirrors) and tiles the real
    // 512 bytes so the standard mirror-trim recovers them.
    let save = dev.read_save(ChipType::Eeprom, 0, 8192, &|_| {}).unwrap();
    assert_eq!(save.len(), 8192);
    assert_eq!(cartridge::detect_eeprom_size(&save), data);
}

#[test]
fn agb_eeprom_write_roundtrip_both_sizes() {
    for (size, marker) in [(512usize, "BEPA"), (8192, "BEPB")] {
        let rom = make_gba_rom("EEPWRITE", marker, b"EEPROM_V124", 1 << 20);
        let mut dev = agb_device(rom, SimAgbSave::Eeprom { size, data: vec![0xFF; size] });
        dev.read_cartridge_info().unwrap();

        let data = pattern(size, 21);
        dev.write_save(ChipType::Eeprom, 0, &data, &|_| {}).unwrap();
        let save = dev.read_save(ChipType::Eeprom, 0, 8192, &|_| {}).unwrap();
        assert_eq!(cartridge::detect_eeprom_size(&save), data);
    }
}

#[test]
fn agb_flash_save_64k_roundtrip() {
    let rom = make_gba_rom("FLSHTEST", "BFLS", b"FLASH512_V131", 1 << 20);
    let save = SimAgbSave::flash(pattern(0x10000, 60), false, [0xBF, 0xD4]); // SST 39VF512
    let mut dev = agb_device(rom, save);
    dev.read_cartridge_info().unwrap();

    let read = dev.read_save(ChipType::Flash, 0, 0x10000, &|_| {}).unwrap();
    assert_eq!(read, pattern(0x10000, 60));

    let new = pattern(0x10000, 61);
    dev.write_save(ChipType::Flash, 0, &new, &|_| {}).unwrap();
    let back = dev.read_save(ChipType::Flash, 0, 0x10000, &|_| {}).unwrap();
    assert_eq!(back, new, "write must erase sectors before programming");
}

#[test]
fn agb_flash_save_128k_banked_roundtrip() {
    let rom = make_gba_rom("FLSHBANK", "BFLB", b"FLASH1M_V103", 1 << 20);
    let save = SimAgbSave::flash(pattern(0x20000, 90), false, [0xC2, 0x09]); // Macronix MX29L010
    let mut dev = agb_device(rom, save);
    dev.read_cartridge_info().unwrap();

    let read = dev.read_save(ChipType::Flash, 0, 0x20000, &|_| {}).unwrap();
    assert_eq!(read, pattern(0x20000, 90), "both banks must be read");

    let new = pattern(0x20000, 91);
    dev.write_save(ChipType::Flash, 0, &new, &|_| {}).unwrap();
    let back = dev.read_save(ChipType::Flash, 0, 0x20000, &|_| {}).unwrap();
    assert_eq!(back, new);
}

#[test]
fn agb_flash_save_atmel_page_writes() {
    let rom = make_gba_rom("FLSHATML", "BFLA", b"FLASH512_V131", 1 << 20);
    let save = SimAgbSave::flash(pattern(0x10000, 33), true, [0x1F, 0x3D]); // Atmel AT29LV512
    let mut dev = agb_device(rom, save);
    dev.read_cartridge_info().unwrap();

    let new = pattern(0x10000, 34);
    dev.write_save(ChipType::Flash, 0, &new, &|_| {}).unwrap();
    let back = dev.read_save(ChipType::Flash, 0, 0x10000, &|_| {}).unwrap();
    assert_eq!(back, new);
}

// --- Error paths --------------------------------------------------------------

#[test]
fn device_error_ack_maps_to_protocol_error() {
    let rom = make_gb_rom(0x03, 0x02, 0x02, "ERRTEST");
    let mut dev = dmg_device(SimMbc::Mbc1, rom, 0x2000);
    dev.read_cartridge_info().unwrap();
    dev.transport_mut().fail_next_ack = true;
    let result = dev.write_save(ChipType::Unknown, 0, &pattern(0x2000, 1), &|_| {});
    assert!(matches!(result, Err(DeviceError::Protocol(_))));
}
