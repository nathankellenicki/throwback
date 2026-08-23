//! GBxCart RW v1.4 ("L" firmware) wire protocol: opcodes, firmware variables, and
//! pure frame-builder functions.
//!
//! Implemented from independently documented protocol facts (opcode map, framing,
//! and sequences recorded in notes/GBXCART-PROTOCOL.md) — not translated from any
//! other implementation. Single-byte binary opcodes; all multi-byte integers are
//! big-endian. Most state-changing commands are acknowledged with a single status
//! byte: 0x01 = OK, 0x03 = OK/continue streaming, 0x02 = device-reported error.

// --- Command opcodes ---------------------------------------------------------

pub const CMD_NULL: u8 = 0x30;
pub const CMD_QUERY_FW_INFO: u8 = 0xA1;
pub const CMD_SET_MODE_AGB: u8 = 0xA2;
pub const CMD_SET_MODE_DMG: u8 = 0xA3;
pub const CMD_SET_VOLTAGE_3_3V: u8 = 0xA4;
pub const CMD_SET_VOLTAGE_5V: u8 = 0xA5;
pub const CMD_SET_VARIABLE: u8 = 0xA6;
pub const CMD_SET_FLASH_CMD: u8 = 0xA7;
pub const CMD_SET_ADDR_AS_INPUTS: u8 = 0xA8;
pub const CMD_CLK_TOGGLE: u8 = 0xA9;
pub const CMD_GET_VARIABLE: u8 = 0xAD;

pub const CMD_DMG_CART_READ: u8 = 0xB1;
pub const CMD_DMG_CART_WRITE: u8 = 0xB2;
pub const CMD_DMG_CART_WRITE_SRAM: u8 = 0xB3;
pub const CMD_DMG_MBC_RESET: u8 = 0xB4;
pub const CMD_DMG_MBC7_READ_EEPROM: u8 = 0xB5;
pub const CMD_DMG_MBC7_WRITE_EEPROM: u8 = 0xB6;

pub const CMD_AGB_CART_READ: u8 = 0xC1;
pub const CMD_AGB_CART_WRITE: u8 = 0xC2;
pub const CMD_AGB_CART_READ_SRAM: u8 = 0xC3;
pub const CMD_AGB_CART_WRITE_SRAM: u8 = 0xC4;
pub const CMD_AGB_CART_READ_EEPROM: u8 = 0xC5;
pub const CMD_AGB_CART_WRITE_EEPROM: u8 = 0xC6;
pub const CMD_AGB_CART_WRITE_FLASH_DATA: u8 = 0xC7;
pub const CMD_AGB_BOOTUP_SEQUENCE: u8 = 0xC9;

pub const CMD_FLASH_PROGRAM: u8 = 0xD3;
pub const CMD_CART_WRITE_FLASH_CMD: u8 = 0xD4;
pub const CMD_CALC_CRC32: u8 = 0xD5;

pub const CMD_CART_PWR_ON: u8 = 0xF2;
pub const CMD_CART_PWR_OFF: u8 = 0xF3;
pub const CMD_QUERY_CART_PWR: u8 = 0xF4;
pub const CMD_PING: u8 = 0xFE;

// Official-firmware passthrough opcodes (single ASCII chars) still served by the
// L firmware; used only during the identification handshake.
pub const OFW_CMD_FW_VER: u8 = 0x56; // 'V'
pub const OFW_CMD_PCB_VER: u8 = 0x68; // 'h'

// --- Acknowledgement bytes ---------------------------------------------------

pub const ACK_OK: u8 = 0x01;
pub const ACK_ERROR: u8 = 0x02;
pub const ACK_CONTINUE: u8 = 0x03;

// --- PCB / firmware identification -------------------------------------------

/// PCB version bytes reported by OFW_CMD_PCB_VER that this backend supports.
pub const PCB_V1_4: u8 = 5;
pub const PCB_V1_4ABC: u8 = 6;
/// GBxCart RW Mini (DMG-only — no AGB slot wiring).
pub const PCB_MINI: u8 = 101;

/// The custom-firmware lineage marker in QUERY_FW_INFO (`'L'`).
pub const CFW_ID_L: u8 = b'L';
/// Minimum L-firmware version the backend accepts (earlier firmware lacks acks
/// on setters and several variables the flows below rely on).
pub const MIN_FW_VER: u16 = 12;
/// Firmware version this backend is hardware-verified against (L14 on a
/// v1.4a board); older-but-accepted versions get a warning.
pub const TESTED_FW_VER: u16 = 14;

// --- Firmware variables (SET_VARIABLE / GET_VARIABLE) ------------------------
//
// Keys are namespaced by variable width: the same id means different things at
// different widths, so each constant carries its width alongside its key.

/// Variable width in bytes, sent as the `size` byte of SET/GET_VARIABLE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VarWidth {
    U8 = 1,
    U16 = 2,
    U32 = 4,
}

/// A firmware variable: (width, key id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Var(pub VarWidth, pub u32);

pub const VAR_ADDRESS: Var = Var(VarWidth::U32, 0x00);

pub const VAR_TRANSFER_SIZE: Var = Var(VarWidth::U16, 0x00);
pub const VAR_BUFFER_SIZE: Var = Var(VarWidth::U16, 0x01);

pub const VAR_CART_MODE: Var = Var(VarWidth::U8, 0x00);
pub const VAR_DMG_ACCESS_MODE: Var = Var(VarWidth::U8, 0x01);
pub const VAR_FLASH_COMMAND_SET: Var = Var(VarWidth::U8, 0x02);
pub const VAR_FLASH_METHOD: Var = Var(VarWidth::U8, 0x03);
pub const VAR_FLASH_WE_PIN: Var = Var(VarWidth::U8, 0x04);
pub const VAR_FLASH_PULSE_RESET: Var = Var(VarWidth::U8, 0x05);
pub const VAR_FLASH_SHARP_VERIFY_SR: Var = Var(VarWidth::U8, 0x07);
pub const VAR_DMG_READ_CS_PULSE: Var = Var(VarWidth::U8, 0x08);
pub const VAR_DMG_WRITE_CS_PULSE: Var = Var(VarWidth::U8, 0x09);
pub const VAR_DMG_READ_METHOD: Var = Var(VarWidth::U8, 0x0B);
pub const VAR_AGB_READ_METHOD: Var = Var(VarWidth::U8, 0x0C);
pub const VAR_AGB_IRQ_ENABLED: Var = Var(VarWidth::U8, 0x10);

// Values for VAR_CART_MODE.
pub const CART_MODE_DMG: u32 = 1;
pub const CART_MODE_AGB: u32 = 2;

// Values for VAR_DMG_ACCESS_MODE.
pub const DMG_ACCESS_ROM_READ: u32 = 1;
pub const DMG_ACCESS_RAM_READ: u32 = 3;
pub const DMG_ACCESS_RAM_WRITE: u32 = 4;

/// DMG_READ_METHOD value: A15-strobed reads. Hardware-verified (v1.4a, L14,
/// Pokemon Yellow): method 0 (RD strobe) returns deterministically corrupted
/// data, while A15 (1) and SlowA15 (2) both read correctly. A15 is the faster
/// of the two working methods.
pub const DMG_READ_METHOD_A15: u32 = 1;

/// Largest read chunk the firmware streams per read opcode.
pub const MAX_BUFFER_READ: u16 = 0x1000;
/// Largest write payload per save/flash chunk.
pub const MAX_BUFFER_WRITE: u16 = 0x400;

// --- Frame builders (pure; unit-tested) --------------------------------------

/// SET_VARIABLE: `[0xA6, width, key u32 BE, value u32 BE]`.
pub fn set_variable_frame(var: Var, value: u32) -> [u8; 10] {
    let mut f = [0u8; 10];
    f[0] = CMD_SET_VARIABLE;
    f[1] = var.0 as u8;
    f[2..6].copy_from_slice(&var.1.to_be_bytes());
    f[6..10].copy_from_slice(&value.to_be_bytes());
    f
}

/// GET_VARIABLE: `[0xAD, width, key u32 BE]` — device answers 4 bytes BE.
pub fn get_variable_frame(var: Var) -> [u8; 6] {
    let mut f = [0u8; 6];
    f[0] = CMD_GET_VARIABLE;
    f[1] = var.0 as u8;
    f[2..6].copy_from_slice(&var.1.to_be_bytes());
    f
}

/// DMG single bus write: `[0xB2, addr u32 BE, value u8]`.
pub fn dmg_write_frame(addr: u16, value: u8) -> [u8; 6] {
    let mut f = [0u8; 6];
    f[0] = CMD_DMG_CART_WRITE;
    f[1..5].copy_from_slice(&(addr as u32).to_be_bytes());
    f[5] = value;
    f
}

/// AGB single bus write: `[0xC2, word_addr u32 BE, value u16 BE]`.
/// `word_addr` is the byte address shifted right by one.
pub fn agb_write_frame(word_addr: u32, value: u16) -> [u8; 7] {
    let mut f = [0u8; 7];
    f[0] = CMD_AGB_CART_WRITE;
    f[1..5].copy_from_slice(&word_addr.to_be_bytes());
    f[5..7].copy_from_slice(&value.to_be_bytes());
    f
}

/// CLK_TOGGLE: `[0xA9, count u32 BE]` — pulses the clock line `count` times.
pub fn clk_toggle_frame(count: u32) -> [u8; 5] {
    let mut f = [0u8; 5];
    f[0] = CMD_CLK_TOGGLE;
    f[1..5].copy_from_slice(&count.to_be_bytes());
    f
}

/// CALC_CRC32: `[0xD5, length u32 BE]` — device answers the CRC as u32 BE.
pub fn calc_crc32_frame(length: u32) -> [u8; 5] {
    let mut f = [0u8; 5];
    f[0] = CMD_CALC_CRC32;
    f[1..5].copy_from_slice(&length.to_be_bytes());
    f
}

/// SET_FLASH_CMD: `[0xA7, cmd_set, method, we_pin, 6 × (addr u32 BE, value u16 BE)]`.
/// Unused slots must be zeroed. AGB addresses are pre-shifted (>> 1) by the caller.
pub fn set_flash_cmd_frame(
    cmd_set: u8,
    method: u8,
    we_pin: u8,
    cmds: &[(u32, u16)],
) -> [u8; 40] {
    assert!(cmds.len() <= 6, "SET_FLASH_CMD carries at most 6 command slots");
    let mut f = [0u8; 40];
    f[0] = CMD_SET_FLASH_CMD;
    f[1] = cmd_set;
    f[2] = method;
    f[3] = we_pin;
    for (i, &(addr, value)) in cmds.iter().enumerate() {
        let o = 4 + i * 6;
        f[o..o + 4].copy_from_slice(&addr.to_be_bytes());
        f[o + 4..o + 6].copy_from_slice(&value.to_be_bytes());
    }
    f
}

/// CART_WRITE_FLASH_CMD: `[0xD4, flashcart, count, count × (addr u32 BE, value u16 BE)]`.
/// Issues a batch of flash command-register writes on the cart bus.
pub fn write_flash_cmd_frame(flashcart: u8, cmds: &[(u32, u16)]) -> Vec<u8> {
    assert!(cmds.len() <= u8::MAX as usize);
    let mut f = Vec::with_capacity(3 + cmds.len() * 6);
    f.push(CMD_CART_WRITE_FLASH_CMD);
    f.push(flashcart);
    f.push(cmds.len() as u8);
    for &(addr, value) in cmds {
        f.extend_from_slice(&addr.to_be_bytes());
        f.extend_from_slice(&value.to_be_bytes());
    }
    f
}

/// PING (fw >= 15): `[0xFE, challenge]` — device answers `!challenge`.
pub fn ping_frame(challenge: u8) -> [u8; 2] {
    [CMD_PING, challenge]
}

/// Expected PING response for a challenge byte.
pub fn ping_response(challenge: u8) -> u8 {
    !challenge
}

/// Firmware identity reported by QUERY_FW_INFO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwInfo {
    pub cfw_id: u8,
    pub fw_ver: u16,
    pub pcb_ver: u8,
    pub build_ts: u32,
}

/// Parse the fixed 8-byte QUERY_FW_INFO body: `{cfw_id, fw_ver u16 BE, pcb u8, ts u32 BE}`.
pub fn parse_fw_info(body: &[u8; 8]) -> FwInfo {
    FwInfo {
        cfw_id: body[0],
        fw_ver: u16::from_be_bytes([body[1], body[2]]),
        pcb_ver: body[3],
        build_ts: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_variable_frame_layout() {
        let f = set_variable_frame(VAR_ADDRESS, 0x1234_5678);
        assert_eq!(f[0], 0xA6);
        assert_eq!(f[1], 4); // u32 width
        assert_eq!(&f[2..6], &[0, 0, 0, 0]); // ADDRESS key 0
        assert_eq!(&f[6..10], &[0x12, 0x34, 0x56, 0x78]); // big-endian value
    }

    #[test]
    fn set_variable_frame_widths() {
        assert_eq!(set_variable_frame(VAR_TRANSFER_SIZE, 0x1000)[1], 2);
        assert_eq!(set_variable_frame(VAR_CART_MODE, CART_MODE_DMG)[1], 1);
        let f = set_variable_frame(VAR_AGB_IRQ_ENABLED, 1);
        assert_eq!(&f[2..6], &[0, 0, 0, 0x10]); // key 0x10, big-endian
    }

    #[test]
    fn get_variable_frame_layout() {
        let f = get_variable_frame(VAR_DMG_ACCESS_MODE);
        assert_eq!(f, [0xAD, 1, 0, 0, 0, 0x01]);
    }

    #[test]
    fn dmg_write_frame_layout() {
        let f = dmg_write_frame(0x2100, 0x07);
        assert_eq!(f, [0xB2, 0, 0, 0x21, 0x00, 0x07]);
    }

    #[test]
    fn agb_write_frame_layout() {
        // Byte address 0xC4 -> word address 0x62.
        let f = agb_write_frame(0xC4 >> 1, 0xBEEF);
        assert_eq!(f, [0xC2, 0, 0, 0, 0x62, 0xBE, 0xEF]);
    }

    #[test]
    fn clk_toggle_frame_layout() {
        assert_eq!(clk_toggle_frame(60), [0xA9, 0, 0, 0, 60]);
    }

    #[test]
    fn calc_crc32_frame_layout() {
        assert_eq!(calc_crc32_frame(0x0010_0000), [0xD5, 0x00, 0x10, 0x00, 0x00]);
    }

    #[test]
    fn set_flash_cmd_frame_layout() {
        let f = set_flash_cmd_frame(1, 1, 1, &[(0x555, 0xAA), (0x2AA, 0x55)]);
        assert_eq!(f[0..4], [0xA7, 1, 1, 1]);
        // Slot 0: addr 0x555, value 0xAA.
        assert_eq!(f[4..10], [0, 0, 0x05, 0x55, 0x00, 0xAA]);
        // Slot 1: addr 0x2AA, value 0x55.
        assert_eq!(f[10..16], [0, 0, 0x02, 0xAA, 0x00, 0x55]);
        // Slots 2..5 zeroed.
        assert!(f[16..40].iter().all(|&b| b == 0));
    }

    #[test]
    #[should_panic]
    fn set_flash_cmd_frame_rejects_more_than_six() {
        let seven = [(0u32, 0u16); 7];
        set_flash_cmd_frame(1, 1, 1, &seven);
    }

    #[test]
    fn write_flash_cmd_frame_layout() {
        let f = write_flash_cmd_frame(0, &[(0x555, 0xAA), (0x2AA, 0x55), (0x555, 0x90)]);
        assert_eq!(f.len(), 3 + 3 * 6);
        assert_eq!(f[0..3], [0xD4, 0, 3]);
        assert_eq!(f[3..9], [0, 0, 0x05, 0x55, 0x00, 0xAA]);
        assert_eq!(f[15..21], [0, 0, 0x05, 0x55, 0x00, 0x90]);
    }

    #[test]
    fn ping_roundtrip() {
        assert_eq!(ping_frame(0x5A), [0xFE, 0x5A]);
        assert_eq!(ping_response(0x5A), 0xA5);
        assert_eq!(ping_response(0x00), 0xFF);
    }

    #[test]
    fn parse_fw_info_fields() {
        let body = [b'L', 0x00, 0x0F, 6, 0x6A, 0x1F, 0x00, 0x5E];
        let info = parse_fw_info(&body);
        assert_eq!(info.cfw_id, b'L');
        assert_eq!(info.fw_ver, 15);
        assert_eq!(info.pcb_ver, 6);
        assert_eq!(info.build_ts, 0x6A1F_005E);
    }
}
