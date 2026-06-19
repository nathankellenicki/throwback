use throwback::cartridge::*;
use throwback::device::ChipType;

#[test]
fn test_format_size() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(512), "512 B");
    assert_eq!(format_size(1024), "1 KB");
    assert_eq!(format_size(32 * 1024), "32 KB");
    assert_eq!(format_size(1024 * 1024), "1 MB");
    assert_eq!(format_size(32 * 1024 * 1024), "32 MB");
}

#[test]
fn test_cartridge_info_gb() {
    let mut sig = [0u8; 64];
    sig[2] = 0x20;
    sig[3] = 0x01;
    sig[4] = 0x01;
    sig[0x0D] = b'P';
    sig[0x0E] = 0x1B;
    sig[0x0F] = 0x05;
    sig[0x10] = 0x03;
    sig[0x11] = 0x97;
    sig[0x12] = 0x04;
    sig[0x13] = 0x7C;

    let info = CartridgeInfo::from_bytes(&sig);
    assert!(info.present);
    assert_eq!(info.cart_type, CartridgeType::GB);
    assert_eq!(info.title_char, 'P');
    assert_eq!(info.mbc_type, 0x1B);
    assert_eq!(info.mbc_name(), "MBC5+RAM+Battery");
    assert_eq!(info.rom_size, 1024 * 1024);
    assert_eq!(info.ram_size, 32 * 1024);
    assert_eq!(info.header_checksum, 0x97);
    assert_eq!(info.global_checksum, 0x7C04);
    assert_eq!(info.game_id(), "P977C04");
}

#[test]
fn test_cartridge_info_gba() {
    let mut sig = [0u8; 64];
    sig[2] = 0x30;
    sig[3] = 0x01;
    sig[0x0D] = b'A';
    sig[0x0E] = b'W';
    sig[0x0F] = b'R';
    sig[0x10] = b'P';

    let info = CartridgeInfo::from_bytes(&sig);
    assert!(info.present);
    assert_eq!(info.cart_type, CartridgeType::GBA);
    assert_eq!(info.game_code, [b'W', b'R', b'P']);
    assert_eq!(info.game_id(), "AWRP");
}

#[test]
fn test_cartridge_not_present() {
    let sig = [0u8; 64];
    let info = CartridgeInfo::from_bytes(&sig);
    assert!(!info.present);
}

#[test]
fn test_gb_rom_sizes() {
    for (code, expected) in [(0, 32768), (1, 65536), (2, 131072), (5, 1048576), (8, 8388608)] {
        let mut sig = [0u8; 64];
        sig[2] = 0x20;
        sig[3] = 0x01;
        sig[0x0F] = code;
        let info = CartridgeInfo::from_bytes(&sig);
        assert_eq!(info.rom_size, expected, "ROM code {}", code);
    }
}

#[test]
fn test_gb_ram_sizes() {
    for (code, expected) in [(0, 0), (1, 2048), (2, 8192), (3, 32768), (4, 131072), (5, 65536)] {
        let mut sig = [0u8; 64];
        sig[2] = 0x20;
        sig[3] = 0x01;
        sig[0x10] = code;
        let info = CartridgeInfo::from_bytes(&sig);
        assert_eq!(info.ram_size, expected, "RAM code {}", code);
    }
}

#[test]
fn test_detect_gba_save_eeprom() {
    let mut rom = vec![0u8; 1024];
    rom[100..108].copy_from_slice(b"EEPROM_V");
    let (chip, size) = detect_gba_save(&rom);
    assert_eq!(chip, ChipType::Eeprom);
    assert_eq!(size, 8 * 1024);
}

#[test]
fn test_detect_gba_save_flash1m() {
    let mut rom = vec![0u8; 1024];
    rom[100..109].copy_from_slice(b"FLASH1M_V");
    let (chip, size) = detect_gba_save(&rom);
    assert_eq!(chip, ChipType::Flash);
    assert_eq!(size, 128 * 1024);
}

#[test]
fn test_detect_gba_save_flash512() {
    let mut rom = vec![0u8; 1024];
    rom[100..110].copy_from_slice(b"FLASH512_V");
    let (chip, size) = detect_gba_save(&rom);
    assert_eq!(chip, ChipType::Flash);
    assert_eq!(size, 64 * 1024);
}

#[test]
fn test_detect_gba_save_flash() {
    let mut rom = vec![0u8; 1024];
    rom[100..107].copy_from_slice(b"FLASH_V");
    let (chip, size) = detect_gba_save(&rom);
    assert_eq!(chip, ChipType::Flash);
    assert_eq!(size, 64 * 1024);
}

#[test]
fn test_detect_gba_save_sram() {
    let mut rom = vec![0u8; 1024];
    rom[100..106].copy_from_slice(b"SRAM_V");
    let (chip, size) = detect_gba_save(&rom);
    assert_eq!(chip, ChipType::Sram);
    assert_eq!(size, 32 * 1024);
}

#[test]
fn test_detect_gba_save_none() {
    let rom = vec![0u8; 1024];
    let (chip, size) = detect_gba_save(&rom);
    assert_eq!(chip, ChipType::Unknown);
    assert_eq!(size, 0);
}

#[test]
fn test_detect_eeprom_size_mirrored() {
    let block: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
    let mut data = Vec::new();
    for _ in 0..16 {
        data.extend_from_slice(&block);
    }
    assert_eq!(data.len(), 8192);

    let result = detect_eeprom_size(&data);
    assert_eq!(result.len(), 512);
    assert_eq!(result, block);
}

#[test]
fn test_detect_eeprom_size_not_mirrored() {
    let mut data = vec![0u8; 8192];
    for i in 0..16 {
        data[i * 512] = i as u8;
        data[i * 512 + 1] = (i * 7) as u8;
    }

    let result = detect_eeprom_size(&data);
    assert_eq!(result.len(), 8192);
}

#[test]
fn test_detect_eeprom_size_small_input() {
    let data = vec![0u8; 256];
    let result = detect_eeprom_size(&data);
    assert_eq!(result.len(), 256);
}

#[test]
fn test_trim_gba_rom_open_bus_at_4mb() {
    let mut rom = vec![0xAA; 4 * 1024 * 1024];
    for i in 0..4 * 1024 * 1024 / 2 {
        let val = (i as u16).to_le_bytes();
        rom.push(val[0]);
        rom.push(val[1]);
    }
    assert_eq!(rom.len(), 8 * 1024 * 1024);
    assert_eq!(trim_gba_rom(&rom), 4 * 1024 * 1024);
}

#[test]
fn test_trim_gba_rom_no_open_bus() {
    let rom = vec![0xAA; 8 * 1024 * 1024];
    assert_eq!(trim_gba_rom(&rom), 8 * 1024 * 1024);
}

#[test]
fn test_trim_gba_rom_exact_size() {
    let rom = vec![0xAA; 4 * 1024 * 1024];
    assert_eq!(trim_gba_rom(&rom), 4 * 1024 * 1024);
}

/// Build a synthetic SNES ROM of `len` bytes with a valid header at `base`.
fn build_snes_rom(
    len: usize,
    base: usize,
    title: &str,
    map_mode: u8,
    cart_type: u8,
    rom_code: u8,
    ram_code: u8,
) -> Vec<u8> {
    let mut rom = vec![0u8; len];
    // Title: 21 bytes, space-padded.
    let mut title_bytes = title.as_bytes().to_vec();
    title_bytes.resize(21, b' ');
    rom[base..base + 21].copy_from_slice(&title_bytes);
    rom[base + 0x15] = map_mode;
    rom[base + 0x16] = cart_type;
    rom[base + 0x17] = rom_code;
    rom[base + 0x18] = ram_code;
    // checksum ^ complement must equal 0xFFFF, checksum != 0.
    let checksum: u16 = 0x1234;
    let complement: u16 = checksum ^ 0xFFFF;
    rom[base + 0x1C..base + 0x1E].copy_from_slice(&complement.to_le_bytes());
    rom[base + 0x1E..base + 0x20].copy_from_slice(&checksum.to_le_bytes());
    rom
}

#[test]
fn test_parse_snes_header_lorom() {
    // 256KB LoROM (rom_code 8), 8KB SRAM (ram_code 3), ROM+RAM+battery.
    let rom = build_snes_rom(256 * 1024, 0x7FC0, "TEST GAME", 0x20, 0x02, 0x08, 0x03);
    let h = parse_snes_header(&rom).expect("should detect LoROM header");
    assert_eq!(h.mapper, SnesMapper::LoRom);
    assert_eq!(h.rom_size, 256 * 1024);
    assert_eq!(h.ram_size, 8 * 1024);
    assert_eq!(h.save_chip, ChipType::Sram);
    assert!(h.title.starts_with("TEST GAME"));
}

#[test]
fn test_parse_snes_header_hirom() {
    // 512KB HiROM (rom_code 9), no SRAM.
    let rom = build_snes_rom(512 * 1024, 0xFFC0, "HIROM TITLE", 0x21, 0x00, 0x09, 0x00);
    let h = parse_snes_header(&rom).expect("should detect HiROM header");
    assert_eq!(h.mapper, SnesMapper::HiRom);
    assert_eq!(h.rom_size, 512 * 1024);
    assert_eq!(h.ram_size, 0);
    assert_eq!(h.save_chip, ChipType::Unknown);
}

#[test]
fn test_parse_snes_header_strips_smc_copier_header() {
    // Prepend a 512-byte SMC header; the parser should strip it and still detect.
    let rom = build_snes_rom(256 * 1024, 0x7FC0, "SMC GAME", 0x20, 0x02, 0x08, 0x03);
    let mut with_smc = vec![0u8; 512];
    with_smc.extend_from_slice(&rom);
    let h = parse_snes_header(&with_smc).expect("should detect after stripping SMC header");
    assert_eq!(h.mapper, SnesMapper::LoRom);
    assert_eq!(h.rom_size, 256 * 1024);
}

#[test]
fn test_parse_snes_header_garbage() {
    let rom = vec![0xFFu8; 256 * 1024];
    assert!(parse_snes_header(&rom).is_none());
}

/// Real SN Operator signature captured from a Desert Strike cartridge (LoROM, 1 MB,
/// no save). Locks in the signature→size decode that drives the SNES dump.
fn desert_strike_signature() -> [u8; 64] {
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&[
        0x01, 0x01, 0x50, 0x01, 0x01, 0x14, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0xA4, 0x75,
        0x02, 0x0A, 0x44, 0x20, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x02,
        0x00, 0x00,
    ]);
    sig[32] = 0x01;
    sig
}

#[test]
fn test_cartridge_info_snes_from_signature() {
    let info = CartridgeInfo::from_bytes(&desert_strike_signature());
    assert!(info.present);
    assert_eq!(info.cart_type, CartridgeType::SNES);
    // byte[0x10] == 0x0A → 0x400 << 0x0A == 1 MB.
    assert_eq!(info.rom_size, 1024 * 1024);
    // SNES carts have no Operator-reported game ID.
    assert_eq!(info.game_id(), "");
}

#[test]
fn test_cartridge_info_snes_rom_size_codes() {
    // 0x400 << code across the plausible SNES range (256 KB .. 64 MB).
    for (code, expected) in [
        (0x08u8, 256 * 1024),
        (0x09, 512 * 1024),
        (0x0A, 1024 * 1024),
        (0x0C, 4 * 1024 * 1024),
        (0x10, 64 * 1024 * 1024),
    ] {
        let mut sig = [0u8; 64];
        sig[2] = 0x50; // SNES marker
        sig[4] = 0x01; // present
        sig[0x10] = code;
        let info = CartridgeInfo::from_bytes(&sig);
        assert_eq!(info.cart_type, CartridgeType::SNES);
        assert_eq!(info.rom_size, expected, "ROM code {code:#x}");
    }
}

#[test]
fn test_cartridge_info_snes_rom_size_code_out_of_range() {
    // Implausible codes must not produce a bogus (huge) size; fall back to 0.
    for code in [0x00u8, 0x07, 0x11, 0xFF] {
        let mut sig = [0u8; 64];
        sig[2] = 0x50;
        sig[4] = 0x01;
        sig[0x10] = code;
        let info = CartridgeInfo::from_bytes(&sig);
        assert_eq!(info.rom_size, 0, "code {code:#x} should not yield a size");
    }
}

#[test]
fn test_parse_gb_title() {
    // Build a 16 KB bank-0 read with a header title at 0x134.
    let mut rom = vec![0u8; 0x4000];
    rom[0x134..0x134 + 14].copy_from_slice(b"POKEMON YELLOW");
    rom[0x143] = 0x80; // CGB flag — must not bleed into the title
    assert_eq!(parse_gb_title(&rom).as_deref(), Some("POKEMON YELLOW"));
}

#[test]
fn test_parse_gb_title_space_padded_and_empty() {
    // Trailing spaces are trimmed.
    let mut rom = vec![0u8; 0x4000];
    rom[0x134..0x134 + 16].copy_from_slice(b"TETRIS          ");
    assert_eq!(parse_gb_title(&rom).as_deref(), Some("TETRIS"));

    // All-zero header → no title.
    let blank = vec![0u8; 0x4000];
    assert_eq!(parse_gb_title(&blank), None);

    // Too short to contain a header.
    assert_eq!(parse_gb_title(&[0u8; 16]), None);
}

#[test]
fn test_parse_cgb_flag() {
    let mut rom = vec![0u8; 0x4000];

    rom[0x143] = 0xC0; // CGB-only
    assert_eq!(parse_cgb_flag(&rom), "GBC");

    rom[0x143] = 0x80; // CGB-enhanced, DMG-compatible
    assert_eq!(parse_cgb_flag(&rom), "GB/GBC");

    rom[0x143] = 0x00; // original Game Boy
    assert_eq!(parse_cgb_flag(&rom), "GB");

    // Short buffer → safe fallback.
    assert_eq!(parse_cgb_flag(&[0u8; 16]), "GB");
}

#[test]
fn test_parse_gb_region() {
    let mut rom = vec![0u8; 0x4000];

    rom[0x14A] = 0x00; // Japan
    assert_eq!(parse_gb_region(&rom), Some("Japan"));

    rom[0x14A] = 0x01; // overseas
    assert_eq!(parse_gb_region(&rom), Some("Non-Japan (International)"));

    rom[0x14A] = 0x7F; // undefined
    assert_eq!(parse_gb_region(&rom), Some("Unknown"));

    // Too short to reach 0x14A.
    assert_eq!(parse_gb_region(&[0u8; 16]), None);
}

#[test]
fn test_parse_gba_region() {
    // Region is the 4th game-code char at 0xAF (AGB-XXXY).
    let mut rom = vec![0u8; 0x4000];

    rom[0xAF] = b'P';
    assert_eq!(parse_gba_region(&rom).as_deref(), Some("Europe (P)"));

    // 'E' is the English-market code (NA + Europe), not USA-only.
    rom[0xAF] = b'E';
    assert_eq!(parse_gba_region(&rom).as_deref(), Some("USA/Europe (English) (E)"));

    rom[0xAF] = b'J';
    assert_eq!(parse_gba_region(&rom).as_deref(), Some("Japan (J)"));

    rom[0xAF] = b'Q'; // printable but unmapped
    assert_eq!(parse_gba_region(&rom).as_deref(), Some("Unknown (Q)"));

    rom[0xAF] = 0x00; // non-printable
    assert_eq!(parse_gba_region(&rom), None);

    assert_eq!(parse_gba_region(&[0u8; 16]), None);
}

#[test]
fn test_rtc_data() {
    // 5 registers (sec/min/hour/day-low/day-ctrl) as u32-LE, current + latched.
    let payload: Vec<u8> = [10u32, 30, 12, 5, 0, 10, 30, 12, 5, 0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let rtc = RtcData::parse(&payload).unwrap();
    assert_eq!((rtc.seconds, rtc.minutes, rtc.hours, rtc.days), (10, 30, 12, 5));
    assert!(rtc.is_valid());
    // Round-trips back to the same 40 bytes.
    assert_eq!(rtc.to_payload(), payload);

    // Day high bit (0x100) + carry flag from the control register.
    let mut p = vec![0u8; 40];
    p[3 * 4] = 0xFF; // day-low = 255
    p[4 * 4] = 0x81; // control: day bit 8 set + carry
    let r = RtcData::parse(&p).unwrap();
    assert_eq!(r.days, 511);
    assert!(r.day_carry && !r.halt);

    // Out-of-range values (dead battery) fail validation.
    let dead = RtcData { seconds: 47, minutes: 63, hours: 31, days: 511, halt: false, day_carry: true };
    assert!(!dead.is_valid());

    assert_eq!(RtcData::parse(&[0u8; 10]), None);
}

#[test]
fn test_gb_header_checksum() {
    // 0x134..=0x14C is 25 bytes; all-zero → x = -25 = 0xE7.
    let rom = vec![0u8; 0x4000];
    assert_eq!(gb_header_checksum(&rom), Some(0xE7));

    // Validity round-trip: stash the computed value at 0x14D.
    let mut rom2 = vec![0u8; 0x4000];
    rom2[0x140] = 0xAB;
    rom2[0x14D] = gb_header_checksum(&rom2).unwrap();
    assert_eq!(gb_header_checksum(&rom2), Some(rom2[0x14D]));

    assert_eq!(gb_header_checksum(&[0u8; 16]), None);
}

#[test]
fn test_gba_header_checksum() {
    // 0xA0..=0xBC is 29 bytes; all-zero → chk = -0x19 = 0xE7.
    let rom = vec![0u8; 0x4000];
    assert_eq!(gba_header_checksum(&rom), Some(0xE7));

    assert_eq!(gba_header_checksum(&[0u8; 16]), None);
}

#[test]
fn test_snes_coprocessor() {
    // Low nibble 0x3..=0x6 means a coprocessor is present.
    assert_eq!(snes_coprocessor(0x05, 0x00), Some("DSP")); // SMK: DSP + RAM + battery
    assert_eq!(snes_coprocessor(0x15, 0x00), Some("SuperFX (GSU)"));
    assert_eq!(snes_coprocessor(0x35, 0x00), Some("SA-1"));
    assert_eq!(snes_coprocessor(0xF3, 0x03), Some("CX4")); // custom + subtype
    assert_eq!(snes_coprocessor(0xF6, 0x00), Some("SPC7110"));
    // No coprocessor (low nibble 0/1/2).
    assert_eq!(snes_coprocessor(0x00, 0x00), None);
    assert_eq!(snes_coprocessor(0x02, 0x00), None);
}

#[test]
fn test_camera_photo_slots() {
    let mut save = vec![0u8; 0x20000];
    save[0x11B2..0x11B2 + 4].copy_from_slice(&[0x00, 0x01, 0x02, 0xFF]);
    assert_eq!(camera_photo_slots(&save), vec![0, 1, 2]);

    // 0xFF at the start = no photos.
    let mut empty = vec![0u8; 0x20000];
    empty[0x11B2] = 0xFF;
    assert!(camera_photo_slots(&empty).is_empty());

    // Too short for a directory.
    assert!(camera_photo_slots(&[0u8; 16]).is_empty());
}

#[test]
fn test_decode_camera_photo() {
    let mut save = vec![0u8; 0x20000];
    let base = 0x2000; // slot 0
    // Tile 0, row 0: both bitplanes set → pixel value 3 (darkest) across 8 px.
    save[base] = 0xFF;
    save[base + 1] = 0xFF;

    let img = decode_camera_photo(&save, 0).unwrap();
    assert_eq!(img.len(), CAMERA_PHOTO_WIDTH * CAMERA_PHOTO_HEIGHT);
    // Top-left 8 px = value 3 → 0x00 (black); next pixel is value 0 → 0xFF (white).
    for x in 0..8 {
        assert_eq!(img[x], 0x00);
    }
    assert_eq!(img[8], 0xFF);

    // Out-of-range slot.
    assert!(decode_camera_photo(&save, 1000).is_none());
}

#[test]
fn test_camera_frame_index() {
    let mut save = vec![0u8; 0x20000];
    save[0x2000 + 0xF54] = 5;
    assert_eq!(camera_frame_index(&save, 0), 5);
    save[0x2000 + 0xF54] = 0x99; // out of range → fallback 0
    assert_eq!(camera_frame_index(&save, 0), 0);
}

#[test]
fn test_decode_camera_photo_framed() {
    let mut save = vec![0u8; 0x20000];
    // Photo slot 0, tile 0 row 0 = value 3 (darkest) across 8 px.
    save[0x2000] = 0xFF;
    save[0x2001] = 0xFF;
    save[0x2000 + 0xF54] = 0; // frame 0
    // Frame 0 block (all zero → border tiles render as value 0 = white).
    let rom = vec![0u8; 0xD0000 + 0x688];

    let img = decode_camera_photo_framed(&save, &rom, 0).unwrap();
    assert_eq!(img.len(), CAMERA_FRAME_WIDTH * CAMERA_FRAME_HEIGHT);
    // Border corner (0,0): all-zero tile → value 0 → 0xFF.
    assert_eq!(img[0], 0xFF);
    // Photo composited at (16,16): its top-left pixel was value 3 → 0x00.
    assert_eq!(img[16 * CAMERA_FRAME_WIDTH + 16], 0x00);
    // Out-of-range slot.
    assert!(decode_camera_photo_framed(&save, &rom, 1000).is_none());
}

#[test]
fn test_trim_snes_rom() {
    // 4 MB over-read of a 2.5 MB cart: [0:2.5M] real, [2.5M:3M] mirror of [2M:2.5M],
    // [3M:4M] open bus. High-entropy pattern that varies with the upper address bits
    // so a 0.5 MB region isn't accidentally an internal mirror.
    let mut rom = vec![0u8; 0x400000];
    for i in 0..0x280000 {
        rom[i] = (i ^ (i >> 7) ^ (i >> 15)) as u8;
    }
    rom.copy_within(0x200000..0x280000, 0x280000); // mirror the 0.5 MB chunk
    for b in &mut rom[0x300000..0x400000] {
        *b = 0x0B; // open bus
    }
    assert_eq!(trim_snes_rom(&rom), 0x280000);

    // A clean power-of-2 ROM is returned unchanged.
    let mut po2 = vec![0u8; 0x100000];
    for i in 0..po2.len() {
        po2[i] = (i ^ (i >> 7) ^ (i >> 15)) as u8;
    }
    assert_eq!(trim_snes_rom(&po2), 0x100000);
}

#[test]
fn test_flashcart_writeable() {
    // Flashcart: first result byte has bit 0 set (observed 0x21 + descriptors).
    assert!(flashcart_writeable(&[0x21, 0x15, 0x02, 0x01]));
    // Retail mask ROM: 0x20 then zeros.
    assert!(!flashcart_writeable(&[0x20, 0, 0, 0]));
    // Family marker | flashable bit holds across families.
    assert!(flashcart_writeable(&[0x31]));
    assert!(!flashcart_writeable(&[0x30]));
    // Empty result.
    assert!(!flashcart_writeable(&[]));
}

#[test]
fn test_snes_region_name() {
    assert_eq!(snes_region_name(0x00), "Japan");
    assert_eq!(snes_region_name(0x01), "USA");
    assert_eq!(snes_region_name(0x02), "Europe/PAL");
    assert_eq!(snes_region_name(0x09), "Germany");
    assert_eq!(snes_region_name(0x11), "Australia");
    assert_eq!(snes_region_name(0xFE), "Unknown");
}

#[test]
fn test_parse_gba_title() {
    // Title lives at 0xA0, up to 12 bytes, uppercase ASCII, null-padded.
    let mut rom = vec![0u8; 0x4000];
    rom[0xA0..0xA0 + 12].copy_from_slice(b"ZELDABNALE00");
    assert_eq!(parse_gba_title(&rom).as_deref(), Some("ZELDABNALE00"));

    // Null padding inside a short title is excluded.
    let mut short = vec![0u8; 0x4000];
    short[0xA0..0xA0 + 6].copy_from_slice(b"METROI");
    assert_eq!(parse_gba_title(&short).as_deref(), Some("METROI"));

    // All-zero header → no title.
    assert_eq!(parse_gba_title(&vec![0u8; 0x4000]), None);

    // Too short to contain a header.
    assert_eq!(parse_gba_title(&[0u8; 16]), None);
}

#[test]
fn test_cartridge_info_marker_dispatch() {
    // byte[2] selects the family; GBA is the default for any other present marker.
    let cases = [
        (0x20u8, CartridgeType::GB),
        (0x30, CartridgeType::GBA),
        (0x50, CartridgeType::SNES),
        (0x99, CartridgeType::GBA),
    ];
    for (marker, expected) in cases {
        let mut sig = [0u8; 64];
        sig[2] = marker;
        sig[4] = 0x01; // present
        assert_eq!(CartridgeInfo::from_bytes(&sig).cart_type, expected, "marker {marker:#x}");
    }
}
