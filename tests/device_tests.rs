use throwback::device::{ChipType, build_command, is_open_bus};

#[test]
fn test_build_command_signature() {
    let packet = build_command(0x04, ChipType::Unknown, 0, 0);
    assert_eq!(packet[0], 0x04);
    assert_eq!(packet[1], 0x00);
    assert_eq!(packet[2..6], [0, 0, 0, 0]);
    assert_eq!(packet[6..10], [0, 0, 0, 0]);
    let crc = u32::from_le_bytes([packet[60], packet[61], packet[62], packet[63]]);
    assert_ne!(crc, 0);
}

#[test]
fn test_build_command_read_game() {
    let packet = build_command(0x00, ChipType::Unknown, 0x100000, 0x8000);
    assert_eq!(packet[0], 0x00);
    assert_eq!(packet[1], 0x00);
    assert_eq!(packet[2..6], [0x00, 0x00, 0x10, 0x00]);
    assert_eq!(packet[6..10], [0x00, 0x80, 0x00, 0x00]);
}

#[test]
fn test_build_command_chip_types() {
    let p1 = build_command(0x02, ChipType::Eeprom, 0, 8192);
    assert_eq!(p1[1], 1);

    let p2 = build_command(0x02, ChipType::Sram, 0, 32768);
    assert_eq!(p2[1], 2);

    let p3 = build_command(0x02, ChipType::Flash, 0, 131072);
    assert_eq!(p3[1], 3);
}

#[test]
fn test_build_command_crc_changes_with_data() {
    let p1 = build_command(0x00, ChipType::Unknown, 0x100000, 0);
    let p2 = build_command(0x00, ChipType::Unknown, 0x200000, 0);
    assert_ne!(p1[60..64], p2[60..64]);
}

#[test]
fn test_build_command_crc_deterministic() {
    let p1 = build_command(0x04, ChipType::Unknown, 0, 0);
    let p2 = build_command(0x04, ChipType::Unknown, 0, 0);
    assert_eq!(p1, p2);
}

#[test]
fn test_build_command_detect_flashcart() {
    let packet = build_command(0x15, ChipType::Unknown, 0, 0);
    assert_eq!(packet[0], 0x15);
    assert_eq!(packet[1..10], [0; 9]);
}

#[test]
fn test_is_open_bus_valid() {
    let mut data = vec![0u8; 256];
    for i in 0..128 {
        let val = (i as u16).to_le_bytes();
        data[i * 2] = val[0];
        data[i * 2 + 1] = val[1];
    }
    assert!(is_open_bus(&data));
}

#[test]
fn test_is_open_bus_invalid() {
    assert!(!is_open_bus(&[0xFF; 256]));
    assert!(!is_open_bus(&[0x00; 256]));
}

#[test]
fn test_is_open_bus_too_short() {
    assert!(!is_open_bus(&[0, 0]));
    assert!(!is_open_bus(&[]));
}

#[test]
fn test_is_open_bus_real_data() {
    let data = [0xCE, 0xED, 0x66, 0x66, 0xCC, 0x0D, 0x00, 0x0B];
    assert!(!is_open_bus(&data));
}
