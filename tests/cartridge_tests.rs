use operator::cartridge::*;
use operator::device::ChipType;

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
