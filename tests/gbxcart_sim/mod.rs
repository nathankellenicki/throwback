//! Behavioral GBxCart RW v1.4 simulator for driver tests.
//!
//! Implements the device side of the L-firmware protocol over the backend's
//! `Transport` trait, with a *semantic* virtual cartridge behind it: reads only
//! return bank N's contents if the driver performed a correct MBC bank-switch
//! sequence, save writes only land if the access mode and CS pulse were set,
//! flash programming only sticks (AND-semantics) into sectors the driver
//! actually erased through the AMD command state machine, and so on. The
//! mapper/flash semantics here are implemented independently from the driver
//! (from GB hardware documentation), so a banking or sequencing bug in the
//! driver produces wrong data instead of a green test.

#![allow(dead_code)] // shared by several test crates; not all use everything

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use throwback::device::gbxcart::protocol::*;
use throwback::device::gbxcart::Transport;
use throwback::device::DeviceError;

// --- Fixture builders ---------------------------------------------------------

/// The standard Nintendo boot logo (0x104..0x134 of every GB cart header).
pub const GB_LOGO: [u8; 48] = [
    0xCE, 0xED, 0x66, 0x66, 0xCC, 0x0D, 0x00, 0x0B, 0x03, 0x73, 0x00, 0x83, 0x00, 0x0C, 0x00,
    0x0D, 0x00, 0x08, 0x11, 0x1F, 0x88, 0x89, 0x00, 0x0E, 0xDC, 0xCC, 0x6E, 0xE6, 0xDD, 0xDD,
    0xD9, 0x99, 0xBB, 0xBB, 0x67, 0x63, 0x6E, 0x0E, 0xEC, 0xCC, 0xDD, 0xDC, 0x99, 0x9F, 0xBB,
    0xB9, 0x33, 0x3E,
];

/// Build a GB ROM with a valid header. Every byte outside the header is a
/// deterministic per-bank sentinel, so a dump equality check proves the
/// driver's banking drove the simulated mapper correctly.
pub fn make_gb_rom(mbc_byte: u8, rom_code: u8, ram_code: u8, title: &str) -> Vec<u8> {
    let size = 32 * 1024 * (1usize << rom_code);
    let mut rom: Vec<u8> = (0..size)
        .map(|i| {
            let bank = (i / 0x4000) as u8;
            bank ^ (i as u8).rotate_left(3) ^ (i >> 8) as u8
        })
        .collect();
    rom[0x104..0x134].copy_from_slice(&GB_LOGO);
    for b in &mut rom[0x134..0x144] {
        *b = 0;
    }
    rom[0x134..0x134 + title.len()].copy_from_slice(title.as_bytes());
    rom[0x143] = 0x00; // DMG cart
    rom[0x147] = mbc_byte;
    rom[0x148] = rom_code;
    rom[0x149] = ram_code;
    rom[0x14A] = 0x01; // non-Japan
    rom[0x14C] = 0x00; // version
    let checksum = throwback::cartridge::gb_header_checksum(&rom).unwrap();
    rom[0x14D] = checksum;
    let global = throwback::cartridge::gb_global_checksum(&rom).unwrap();
    rom[0x14E..0x150].copy_from_slice(&global.to_be_bytes());
    rom
}

/// Build a GBA ROM with a valid header (fixed 0x96 byte + complement checksum)
/// and an embedded save-library marker so `detect_gba_save` works end-to-end.
pub fn make_gba_rom(title: &str, code: &str, save_marker: &[u8], size: usize) -> Vec<u8> {
    let mut rom: Vec<u8> = (0..size)
        .map(|i| ((i >> 2) as u8) ^ ((i >> 10) as u8).rotate_left(1))
        .collect();
    for b in &mut rom[0xA0..0xB0] {
        *b = 0;
    }
    rom[0xA0..0xA0 + title.len()].copy_from_slice(title.as_bytes());
    rom[0xAC..0xAC + code.len()].copy_from_slice(code.as_bytes());
    rom[0xB2] = 0x96;
    rom[0xBC] = 0x00; // version
    let checksum = throwback::cartridge::gba_header_checksum(&rom).unwrap();
    rom[0xBD] = checksum;
    if !save_marker.is_empty() {
        rom[0x400..0x400 + save_marker.len()].copy_from_slice(save_marker);
    }
    rom
}

// --- Virtual cartridges -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimMbc {
    None,
    Mbc1,
    Mbc1M,
    Mbc2,
    Mbc3, // also MBC30 (wider banks fall out naturally)
    Mbc5,
    Camera,
}

pub struct SimDmgCart {
    pub rom: Vec<u8>,
    pub ram: Vec<u8>,
    pub mbc: SimMbc,
    /// RTC "time" registers (sec, min, hour, day-low, day-ctrl); latched copy
    /// is captured by the 0x6000 latch sequence.
    pub rtc: [u8; 5],
    rtc_latched: [u8; 5],
    latch_state: u8,
    // Mapper registers.
    ram_enable: bool,
    bank_lo: u8,
    bank_hi: u8,
    mode: u8,
    ram_bank: u8, // MBC3: 0x08+ selects RTC registers
    /// Flash chip behind the ROM socket (flashcarts); None = mask ROM.
    pub flash: Option<SimFlashChip>,
}

impl SimDmgCart {
    pub fn new(mbc: SimMbc, rom: Vec<u8>, ram_size: usize) -> Self {
        Self {
            rom,
            ram: vec![0u8; ram_size],
            mbc,
            rtc: [0; 5],
            rtc_latched: [0; 5],
            latch_state: 0xFF,
            ram_enable: false,
            bank_lo: 1,
            bank_hi: 0,
            mode: 0,
            ram_bank: 0,
            flash: None,
        }
    }

    pub fn ram_enabled(&self) -> bool {
        self.ram_enable
    }

    fn reset_mapper(&mut self) {
        self.ram_enable = false;
        self.bank_lo = 1;
        self.bank_hi = 0;
        self.mode = 0;
        self.ram_bank = 0;
        self.latch_state = 0xFF;
    }

    /// The ROM bank currently mapped at 0x4000-0x7FFF (hardware semantics,
    /// implemented independently of the driver's bank-plan functions).
    fn switchable_bank(&self) -> usize {
        match self.mbc {
            SimMbc::None => 1,
            SimMbc::Mbc1 => {
                let lo = if self.bank_lo & 0x1F == 0 { 1 } else { self.bank_lo & 0x1F };
                ((self.bank_hi as usize & 0x03) << 5) | lo as usize
            }
            SimMbc::Mbc1M => {
                // Only 4 low bits wired; the 0->1 bump applies to the full
                // 5-bit register, so a written 0x10 passes as wired 0.
                let lo = if self.bank_lo & 0x1F == 0 { 1 } else { self.bank_lo } & 0x0F;
                ((self.bank_hi as usize & 0x03) << 4) | lo as usize
            }
            SimMbc::Mbc2 => {
                let b = self.bank_lo & 0x0F;
                if b == 0 { 1 } else { b as usize }
            }
            SimMbc::Mbc3 => {
                let b = self.bank_lo;
                if b == 0 { 1 } else { b as usize }
            }
            SimMbc::Camera => {
                let b = self.bank_lo & 0x3F;
                if b == 0 { 1 } else { b as usize }
            }
            SimMbc::Mbc5 => ((self.bank_hi as usize & 1) << 8) | self.bank_lo as usize,
        }
    }

    /// The ROM bank mapped at the fixed 0x0000-0x3FFF window.
    fn fixed_bank(&self) -> usize {
        match self.mbc {
            SimMbc::Mbc1 if self.mode == 1 => (self.bank_hi as usize & 0x03) << 5,
            SimMbc::Mbc1M if self.mode == 1 => (self.bank_hi as usize & 0x03) << 4,
            _ => 0,
        }
    }

    fn rom_byte(&mut self, abs: usize) -> u8 {
        // A flashcart answers through its chip model (autoselect IDs, erase
        // busy-polling); a mask ROM serves plain data.
        if let Some(flash) = &mut self.flash {
            return flash.read(abs);
        }
        self.rom.get(abs).copied().unwrap_or(0xFF)
    }

    pub fn bus_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.rom_byte(self.fixed_bank() * 0x4000 + addr as usize),
            0x4000..=0x7FFF => {
                self.rom_byte(self.switchable_bank() * 0x4000 + (addr as usize - 0x4000))
            }
            0xA000..=0xBFFF => self.ram_read(addr),
            _ => 0xFF,
        }
    }

    fn active_ram_bank(&self) -> usize {
        match self.mbc {
            SimMbc::Mbc1 | SimMbc::Mbc1M => {
                if self.mode == 1 { self.bank_hi as usize & 0x03 } else { 0 }
            }
            _ => self.ram_bank as usize & 0x0F,
        }
    }

    fn ram_read(&self, addr: u16) -> u8 {
        if !self.ram_enable {
            return 0xFF;
        }
        let off = addr as usize - 0xA000;
        match self.mbc {
            SimMbc::Mbc2 => {
                // 512 half-bytes; upper nibble reads as open bus (0xF).
                let i = off & 0x1FF;
                self.ram.get(i).map(|b| b | 0xF0).unwrap_or(0xFF)
            }
            SimMbc::Mbc3 if self.ram_bank >= 0x08 => {
                let reg = (self.ram_bank - 0x08) as usize;
                if reg < 5 { self.rtc_latched[reg] } else { 0xFF }
            }
            _ => {
                let i = self.active_ram_bank() * 0x2000 + off;
                self.ram.get(i).copied().unwrap_or(0xFF)
            }
        }
    }

    fn ram_write(&mut self, addr: u16, value: u8) {
        if !self.ram_enable {
            return; // dropped on the floor — exactly what hardware does
        }
        let off = addr as usize - 0xA000;
        match self.mbc {
            SimMbc::Mbc2 => {
                let i = off & 0x1FF;
                if let Some(b) = self.ram.get_mut(i) {
                    *b = value & 0x0F;
                }
            }
            SimMbc::Mbc3 if self.ram_bank >= 0x08 => {
                let reg = (self.ram_bank - 0x08) as usize;
                if reg < 5 {
                    self.rtc[reg] = value;
                    self.rtc_latched[reg] = value;
                }
            }
            _ => {
                let i = self.active_ram_bank() * 0x2000 + off;
                if let Some(b) = self.ram.get_mut(i) {
                    *b = value;
                }
            }
        }
    }

    pub fn bus_write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram_enable = value & 0x0F == 0x0A,
            0x2000..=0x3FFF => {
                if self.mbc == SimMbc::Mbc5 && addr >= 0x3000 {
                    self.bank_hi = value;
                } else {
                    self.bank_lo = value;
                }
            }
            0x4000..=0x5FFF => match self.mbc {
                SimMbc::Mbc1 | SimMbc::Mbc1M | SimMbc::Mbc5 => self.bank_hi = value,
                _ => self.ram_bank = value,
            },
            0x6000..=0x7FFF => {
                if self.mbc == SimMbc::Mbc3 {
                    if self.latch_state == 0 && value == 1 {
                        self.rtc_latched = self.rtc;
                    }
                    self.latch_state = value;
                } else {
                    self.mode = value & 1;
                }
            }
            0xA000..=0xBFFF => self.ram_write(addr, value),
            _ => {}
        }
    }
}

// --- Flash chip (AMD command set), used for DMG and AGB flashcarts ------------

pub struct SimFlashChip {
    pub data: Vec<u8>,
    pub id: Vec<u8>,
    /// Unlock addresses this chip decodes (absolute bus/word*2 addresses).
    pub unlock: (u32, u32),
    pub sector_size: usize,
    state: FlashState,
    autoselect: bool,
    /// Reads reported as "busy" (not yet 0xFF) after an erase, to exercise the
    /// driver's polling loop.
    busy_polls: u32,
    pub erased_sectors: Vec<usize>,
    pub chip_erased: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashState {
    Read,
    U1,
    U2,
    EraseU1,
    EraseU2,
    EraseReady,
    /// Save-flash only: a bank-switch command (0xB0) awaiting the bank byte.
    BankPending,
}

impl SimFlashChip {
    pub fn new(size: usize, id: &[u8], unlock: (u32, u32), sector_size: usize) -> Self {
        Self {
            // A never-erased chip full of old data — programming without a
            // correct erase produces visibly wrong contents (AND semantics).
            data: vec![0xA5; size],
            id: id.to_vec(),
            unlock,
            sector_size,
            state: FlashState::Read,
            autoselect: false,
            busy_polls: 0,
            erased_sectors: Vec::new(),
            chip_erased: false,
        }
    }

    /// Feed one command-register write into the AMD state machine.
    pub fn command(&mut self, addr: u32, value: u16) {
        let (a1, a2) = self.unlock;
        let v = value as u8;
        if v == 0xF0 {
            self.state = FlashState::Read;
            self.autoselect = false;
            return;
        }
        self.state = match (self.state, addr, v) {
            (FlashState::Read, a, 0xAA) if a == a1 => FlashState::U1,
            (FlashState::U1, a, 0x55) if a == a2 => FlashState::U2,
            (FlashState::U2, a, 0x90) if a == a1 => {
                self.autoselect = true;
                FlashState::Read
            }
            (FlashState::U2, a, 0x80) if a == a1 => FlashState::EraseU1,
            (FlashState::EraseU1, a, 0xAA) if a == a1 => FlashState::EraseU2,
            (FlashState::EraseU2, a, 0x55) if a == a2 => FlashState::EraseReady,
            (FlashState::EraseReady, a, 0x10) if a == a1 => {
                self.data.fill(0xFF);
                self.chip_erased = true;
                self.busy_polls = 2;
                FlashState::Read
            }
            (FlashState::EraseReady, sector, 0x30) => {
                let base = sector as usize / self.sector_size * self.sector_size;
                let end = (base + self.sector_size).min(self.data.len());
                self.data[base..end].fill(0xFF);
                self.erased_sectors.push(base);
                self.busy_polls = 2;
                FlashState::Read
            }
            // Anything else drops the sequence — a driver bug shows up as a
            // missing erase or a failed autoselect, not a silent success.
            _ => FlashState::Read,
        };
    }

    /// Value served for a data read at `abs` (busy polling emulated).
    pub fn read(&mut self, abs: usize) -> u8 {
        if self.autoselect {
            return self.id[abs % self.id.len()];
        }
        if self.busy_polls > 0 {
            self.busy_polls -= 1;
            return 0x00; // "not yet erased"
        }
        self.data.get(abs).copied().unwrap_or(0xFF)
    }

    /// Program bytes (flash AND-semantics: bits can only be cleared).
    pub fn program(&mut self, abs: usize, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(cell) = self.data.get_mut(abs + i) {
                *cell &= b;
            }
        }
    }
}

// --- AGB cart ----------------------------------------------------------------

pub enum SimAgbSave {
    None,
    Sram(Vec<u8>),
    /// (actual chip size in bytes, data). Reading with the wrong addressing
    /// width returns deterministic garbage, like real hardware.
    Eeprom { size: usize, data: Vec<u8> },
    /// Save flash: 64 KB or 128 KB (banked), with an Atmel flag.
    Flash { data: Vec<u8>, atmel: bool, bank: usize, state: FlashState, autoselect: bool, id: [u8; 2] },
}

impl SimAgbSave {
    /// Save-flash chip fixture (64 KB plain or 128 KB banked).
    pub fn flash(data: Vec<u8>, atmel: bool, id: [u8; 2]) -> Self {
        SimAgbSave::Flash { data, atmel, bank: 0, state: FlashState::Read, autoselect: false, id }
    }
}

pub struct SimAgbCart {
    pub rom: Vec<u8>,
    pub save: SimAgbSave,
    /// Flash chip behind the ROM socket (AGB flashcarts).
    pub flash: Option<SimFlashChip>,
    /// What reads past the ROM return: None = GBA open bus (incrementing
    /// words); Some(b) = a constant padding byte (hardware-observed: Advance
    /// Wars 2 pads with 0x00 all the way to 32 MB).
    pub pad: Option<u8>,
}

impl SimAgbCart {
    pub fn new(rom: Vec<u8>, save: SimAgbSave) -> Self {
        Self { rom, save, flash: None, pad: None }
    }

    /// ROM-space byte at `abs`; beyond the ROM it's open bus or padding.
    pub fn rom_byte(&mut self, abs: usize) -> u8 {
        if let Some(flash) = &mut self.flash {
            return flash.read(abs);
        }
        match self.rom.get(abs) {
            Some(&b) => b,
            None => match self.pad {
                Some(b) => b,
                None => {
                    let word = (abs / 2) as u16;
                    word.to_le_bytes()[abs % 2]
                }
            },
        }
    }

    fn save_flash_read(&mut self, addr: usize) -> u8 {
        match &mut self.save {
            SimAgbSave::Flash { data, bank, autoselect, id, .. } => {
                if *autoselect {
                    return id[addr % 2];
                }
                data.get(*bank * 0x10000 + addr).copied().unwrap_or(0xFF)
            }
            _ => 0xFF,
        }
    }

    fn save_flash_write(&mut self, addr: usize, value: u8) {
        let SimAgbSave::Flash { data, bank, state, autoselect, .. } = &mut self.save else {
            return;
        };
        if value == 0xF0 && addr == 0x5555 {
            *state = FlashState::Read;
            *autoselect = false;
            return;
        }
        *state = match (*state, addr, value) {
            (FlashState::Read, 0x5555, 0xAA) => FlashState::U1,
            (FlashState::U1, 0x2AAA, 0x55) => FlashState::U2,
            (FlashState::U2, 0x5555, 0x90) => {
                *autoselect = true;
                FlashState::Read
            }
            (FlashState::U2, 0x5555, 0xB0) => FlashState::BankPending,
            (FlashState::BankPending, 0x0000, b) => {
                *bank = b as usize & 1;
                FlashState::Read
            }
            (FlashState::U2, 0x5555, 0x80) => FlashState::EraseU1,
            (FlashState::EraseU1, 0x5555, 0xAA) => FlashState::EraseU2,
            (FlashState::EraseU2, 0x2AAA, 0x55) => FlashState::EraseReady,
            (FlashState::EraseReady, sector, 0x30) => {
                let base = *bank * 0x10000 + (sector & !0xFFF);
                let end = (base + 0x1000).min(data.len());
                data[base..end].fill(0xFF);
                FlashState::Read
            }
            _ => FlashState::Read,
        };
    }
}

// --- The simulator ------------------------------------------------------------

pub enum SimCart {
    Dmg(SimDmgCart),
    Agb(SimAgbCart),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ModeDmg,
    ModeAgb,
    Volt33,
    Volt5,
    PowerOn,
    Idle,
    MbcReset,
    AgbBootup,
    BusWrite(u16, u8),
    ClkToggle(u32),
}

/// What the parser is waiting for next.
enum Expect {
    Opcode,
    /// Raw payload bytes for a data-carrying command.
    Data { cmd: u8, arg: u8, len: usize },
    /// FLASH_PROGRAM continue-mode: next chunk arrives with no opcode.
    FlashData { len: usize },
    /// Variable-length CART_WRITE_FLASH_CMD: waiting for header/pairs.
    FlashCmdHeader,
}

pub struct SimGbxCart {
    pub cart: SimCart,
    pub log: Vec<Event>,
    pub fw_ver: u16,
    pub pcb_ver: u8,
    pub cfw_id: u8,
    /// Flip one bit of flash data after programming (verification tests).
    pub corrupt_flash_after_write: bool,
    /// Answer the next acked command with ACK_ERROR (error-path tests).
    pub fail_next_ack: bool,
    /// FLASH_PROGRAM chunks received (FF-skip evidence).
    pub program_chunks: usize,

    out: VecDeque<u8>,
    buf: Vec<u8>,
    expect: Expect,
    mode: u32, // CART_MODE value: 0 none, 1 DMG, 2 AGB
    powered: bool,
    vars: HashMap<(u8, u32), u32>,
    /// Current address cursor (bytes for DMG/SRAM, words for AGB ROM).
    address: u32,
    /// Words advanced by auto-increment since the last explicit ADDRESS set.
    /// Hardware truth (v1.4a, L14): the firmware's auto-increment stalls when
    /// it crosses word 0x800000 (byte 16 MB); explicit sets work everywhere.
    addr_incremented: u32,
    flash_cfg: Option<(u8, u8, u8)>, // (cmd_set, method, we_pin)
    /// Bytes accumulated toward the current buffered-write group.
    buffer_fill: usize,
    corrupted: bool,
}

impl SimGbxCart {
    pub fn new(cart: SimCart) -> Self {
        Self {
            cart,
            log: Vec::new(),
            fw_ver: 15,
            pcb_ver: 6,
            cfw_id: b'L',
            corrupt_flash_after_write: false,
            fail_next_ack: false,
            program_chunks: 0,
            out: VecDeque::new(),
            buf: Vec::new(),
            expect: Expect::Opcode,
            mode: 0,
            powered: false,
            vars: HashMap::new(),
            address: 0,
            addr_incremented: 0,
            flash_cfg: None,
            buffer_fill: 0,
            corrupted: false,
        }
    }

    fn var(&self, v: Var) -> u32 {
        *self.vars.get(&(v.0 as u8, v.1)).unwrap_or(&0)
    }

    fn transfer_size(&self) -> usize {
        self.var(VAR_TRANSFER_SIZE) as usize
    }

    fn push(&mut self, bytes: &[u8]) {
        self.out.extend(bytes);
    }

    fn ack(&mut self) {
        if self.fail_next_ack {
            self.fail_next_ack = false;
            self.push(&[ACK_ERROR]);
        } else {
            self.push(&[ACK_OK]);
        }
    }

    // -- Bus glue -------------------------------------------------------------

    fn dmg_read_stream(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let access = self.var(VAR_DMG_ACCESS_MODE);
        let cs_pulse = self.var(VAR_DMG_READ_CS_PULSE) == 1;
        // Hardware truth (v1.4a, L14): DMG_READ_METHOD 0 (RD strobe) returns
        // deterministically corrupted data; only A15 (1) / SlowA15 (2) work.
        let method_ok = matches!(self.var(VAR_DMG_READ_METHOD), 1 | 2);
        let live = self.mode == 1 && self.powered && method_ok;
        for _ in 0..len {
            let addr = self.address as u16;
            let byte = match &mut self.cart {
                SimCart::Dmg(cart) if live => {
                    // RAM reads require the RAM access mode + CS pulse config;
                    // without them the read floats.
                    if (0xA000..=0xBFFF).contains(&addr) {
                        if access == DMG_ACCESS_RAM_READ && cs_pulse {
                            cart.bus_read(addr)
                        } else {
                            0xFF
                        }
                    } else {
                        cart.bus_read(addr)
                    }
                }
                _ => 0xFF,
            };
            out.push(byte);
            self.address += 1;
        }
        out
    }

    fn agb_read_stream(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let live = self.mode == 2 && self.powered;
        for _ in 0..len / 2 {
            // The auto-increment stall: crossing word 0x800000 without a
            // fresh explicit ADDRESS set kills the stream (the host's
            // read_exact then comes up short, as observed on hardware).
            if self.address == 0x0080_0000 && self.addr_incremented > 0 {
                return out;
            }
            let abs = self.address as usize * 2;
            let (b0, b1) = match &mut self.cart {
                SimCart::Agb(cart) if live => {
                    (cart.rom_byte(abs), cart.rom_byte(abs + 1))
                }
                _ => (0xFF, 0xFF),
            };
            out.push(b0);
            out.push(b1);
            self.address += 1;
            self.addr_incremented += 1;
        }
        out
    }

    fn agb_sram_read_stream(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            let addr = self.address as usize;
            let byte = match &mut self.cart {
                SimCart::Agb(cart) => match &mut cart.save {
                    SimAgbSave::Sram(data) => data.get(addr).copied().unwrap_or(0xFF),
                    SimAgbSave::Flash { .. } => cart.save_flash_read(addr),
                    _ => 0xFF,
                },
                _ => 0xFF,
            };
            out.push(byte);
            self.address += 1;
        }
        out
    }

    fn agb_eeprom_read_stream(&mut self, size_id: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            let addr = self.address as usize;
            let byte = match &mut self.cart {
                SimCart::Agb(cart) => match &cart.save {
                    SimAgbSave::Eeprom { size, data } => {
                        let addressed = if size_id == 1 { 512 } else { 8192 };
                        if addressed == *size {
                            data.get(addr % size).copied().unwrap_or(0xFF)
                        } else if addressed > *size {
                            // Wide-addressing a small part: the ignored upper
                            // address bits wrap — 512-byte mirrors.
                            data.get(addr % size).copied().unwrap_or(0xFF)
                        } else {
                            // Narrow-addressing a big part: the shift register
                            // misaligns and returns junk.
                            (addr as u8).wrapping_mul(7).wrapping_add(13)
                        }
                    }
                    _ => 0xFF,
                },
                _ => 0xFF,
            };
            out.push(byte);
            self.address += 1;
        }
        out
    }

    /// Resolve the current DMG bus address through the mapper to an absolute
    /// flash address (flash programming follows the MBC window).
    fn dmg_abs_addr(&self, bus: u32) -> usize {
        match &self.cart {
            SimCart::Dmg(cart) => {
                let bus = bus as usize;
                if bus < 0x4000 {
                    cart.fixed_bank() * 0x4000 + bus
                } else {
                    cart.switchable_bank() * 0x4000 + (bus - 0x4000)
                }
            }
            _ => bus as usize,
        }
    }

    // -- Command execution ----------------------------------------------------

    fn exec(&mut self, cmd: u8, frame: &[u8]) {
        match cmd {
            OFW_CMD_PCB_VER => {
                let v = self.pcb_ver;
                self.push(&[v]);
            }
            OFW_CMD_FW_VER => self.push(&[30]),
            CMD_QUERY_FW_INFO => {
                let mut body = vec![8u8, self.cfw_id];
                body.extend_from_slice(&self.fw_ver.to_be_bytes());
                body.push(self.pcb_ver);
                body.extend_from_slice(&0x6A00_0000u32.to_be_bytes());
                body.push(4); // name length
                body.extend_from_slice(b"gbxc");
                body.push(0b0000_0001); // features: power control
                body.push(1); // bootloader reset supported
                self.push(&body);
            }
            CMD_SET_MODE_DMG => {
                self.log.push(Event::ModeDmg);
                self.ack();
            }
            CMD_SET_MODE_AGB => {
                self.log.push(Event::ModeAgb);
                self.ack();
            }
            CMD_SET_VOLTAGE_3_3V => {
                self.log.push(Event::Volt33);
                self.ack();
            }
            CMD_SET_VOLTAGE_5V => {
                self.log.push(Event::Volt5);
                self.ack();
            }
            CMD_SET_VARIABLE => {
                let width = frame[1];
                let key = u32::from_be_bytes(frame[2..6].try_into().unwrap());
                let value = u32::from_be_bytes(frame[6..10].try_into().unwrap());
                self.vars.insert((width, key), value);
                if width == 4 && key == VAR_ADDRESS.1 {
                    self.address = value;
                    self.addr_incremented = 0;
                }
                if width == 1 && key == VAR_CART_MODE.1 {
                    self.mode = value;
                }
                self.ack();
            }
            CMD_GET_VARIABLE => {
                let width = frame[1];
                let key = u32::from_be_bytes(frame[2..6].try_into().unwrap());
                let v = *self.vars.get(&(width, key)).unwrap_or(&0);
                self.push(&v.to_be_bytes());
            }
            CMD_SET_ADDR_AS_INPUTS => {
                self.log.push(Event::Idle);
                self.ack();
            }
            CMD_CLK_TOGGLE => {
                let n = u32::from_be_bytes(frame[1..5].try_into().unwrap());
                self.log.push(Event::ClkToggle(n));
                self.ack();
            }
            CMD_QUERY_CART_PWR => {
                let p = self.powered as u8;
                self.push(&[p]);
            }
            CMD_CART_PWR_ON => {
                self.powered = true;
                self.log.push(Event::PowerOn);
                self.ack();
            }
            CMD_DMG_MBC_RESET => {
                if let SimCart::Dmg(cart) = &mut self.cart {
                    cart.reset_mapper();
                }
                self.log.push(Event::MbcReset);
                self.ack();
            }
            CMD_AGB_BOOTUP_SEQUENCE => {
                self.log.push(Event::AgbBootup);
                self.ack();
            }
            CMD_DMG_CART_READ => {
                let n = self.transfer_size();
                let data = self.dmg_read_stream(n);
                self.push(&data);
            }
            CMD_AGB_CART_READ => {
                let n = self.transfer_size();
                let data = self.agb_read_stream(n);
                self.push(&data);
            }
            CMD_AGB_CART_READ_SRAM => {
                let n = self.transfer_size();
                let data = self.agb_sram_read_stream(n);
                self.push(&data);
            }
            CMD_AGB_CART_READ_EEPROM => {
                let n = self.transfer_size();
                let data = self.agb_eeprom_read_stream(frame[1], n);
                self.push(&data);
            }
            CMD_DMG_CART_WRITE => {
                let addr = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as u16;
                let value = frame[5];
                self.log.push(Event::BusWrite(addr, value));
                let live = self.mode == 1 && self.powered;
                if let SimCart::Dmg(cart) = &mut self.cart
                    && live
                {
                    cart.bus_write(addr, value);
                }
                self.ack();
            }
            CMD_AGB_CART_WRITE => self.ack(),
            CMD_DMG_MBC7_READ_EEPROM => {
                // Minimal MBC7 model: EEPROM lives in cart.ram.
                let n = self.transfer_size();
                let mut data = vec![0xFFu8; n];
                if let SimCart::Dmg(cart) = &self.cart {
                    for (i, b) in data.iter_mut().enumerate() {
                        let addr = self.address as usize + i;
                        *b = cart.ram.get(addr).copied().unwrap_or(0xFF);
                    }
                }
                self.address += n as u32;
                self.push(&data);
            }
            CMD_CART_WRITE_FLASH_CMD => {
                let count = frame[2] as usize;
                for i in 0..count {
                    let o = 3 + i * 6;
                    let addr = u32::from_be_bytes(frame[o..o + 4].try_into().unwrap());
                    let value = u16::from_be_bytes(frame[o + 4..o + 6].try_into().unwrap());
                    match &mut self.cart {
                        SimCart::Dmg(cart) => {
                            // The flash chip sees the command through the MBC
                            // window (resolved with the pre-write mapping)...
                            let abs = if (addr as usize) < 0x4000 {
                                cart.fixed_bank() * 0x4000 + addr as usize
                            } else if (addr as usize) < 0x8000 {
                                cart.switchable_bank() * 0x4000 + addr as usize - 0x4000
                            } else {
                                addr as usize
                            };
                            if let Some(flash) = &mut cart.flash {
                                flash.command(abs as u32, value);
                            }
                            // ...and the MBC sees the same bus write — a driver
                            // whose command addresses clobber its own banking
                            // (e.g. 0x2AAA unlock on an MBC5 cart) fails here
                            // exactly as it would on hardware.
                            if (addr as usize) < 0x8000 {
                                cart.bus_write(addr as u16, value as u8);
                            }
                        }
                        SimCart::Agb(cart) => {
                            if let Some(flash) = &mut cart.flash {
                                // Word address on the wire; the chip model is
                                // byte-addressed throughout, so convert.
                                flash.command(addr * 2, value);
                            }
                        }
                        SimCart::Empty => {}
                    }
                }
                self.ack();
            }
            CMD_SET_FLASH_CMD => {
                self.flash_cfg = Some((frame[1], frame[2], frame[3]));
                self.ack();
            }
            CMD_CALC_CRC32 => {
                let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
                let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
                let mut digest = crc.digest();
                let start = self.address as usize * 2;
                let mut bytes = Vec::with_capacity(len);
                for i in 0..len {
                    let b = match &mut self.cart {
                        SimCart::Agb(cart) => cart.rom_byte(start + i),
                        _ => 0xFF,
                    };
                    bytes.push(b);
                }
                digest.update(&bytes);
                self.push(&digest.finalize().to_be_bytes());
            }
            CMD_PING => {
                let c = frame[1];
                self.push(&[!c]);
            }
            _ => panic!("sim: unhandled opcode 0x{cmd:02X}"),
        }
    }

    /// Data-carrying commands (opcode [+arg] already parsed; `data` follows).
    fn exec_data(&mut self, cmd: u8, arg: u8, data: &[u8]) {
        match cmd {
            CMD_DMG_CART_WRITE_SRAM => {
                let access = self.var(VAR_DMG_ACCESS_MODE);
                let pulsed = self.var(VAR_DMG_WRITE_CS_PULSE) == 1;
                let live = self.mode == 1 && self.powered;
                for &b in data {
                    let addr = self.address as u16;
                    // Semantic gate: without RAM-write access mode + CS pulse
                    // the write never reaches the chip.
                    if let SimCart::Dmg(cart) = &mut self.cart
                        && live
                        && access == DMG_ACCESS_RAM_WRITE
                        && pulsed
                    {
                        cart.bus_write(addr, b);
                    }
                    self.address += 1;
                }
                self.ack();
            }
            CMD_AGB_CART_WRITE_SRAM => {
                for &b in data {
                    let addr = self.address as usize;
                    if let SimCart::Agb(cart) = &mut self.cart {
                        match &mut cart.save {
                            SimAgbSave::Sram(mem) => {
                                if let Some(cell) = mem.get_mut(addr) {
                                    *cell = b;
                                }
                            }
                            SimAgbSave::Flash { .. } => cart.save_flash_write(addr, b),
                            _ => {}
                        }
                    }
                    self.address += 1;
                }
                self.ack();
            }
            CMD_AGB_CART_WRITE_EEPROM => {
                let size_id = arg;
                for &b in data {
                    let addr = self.address as usize;
                    if let SimCart::Agb(cart) = &mut self.cart
                        && let SimAgbSave::Eeprom { size, data: mem } = &mut cart.save {
                            let addressed = if size_id == 1 { 512 } else { 8192 };
                            if addressed == *size
                                && let Some(cell) = mem.get_mut(addr)
                            {
                                *cell = b;
                            }
                        }
                    self.address += 1;
                }
                self.ack();
            }
            CMD_AGB_CART_WRITE_FLASH_DATA => {
                let chip_type = arg;
                let base = self.address as usize;
                if let SimCart::Agb(cart) = &mut self.cart
                    && let SimAgbSave::Flash { data: mem, atmel, bank, .. } = &mut cart.save {
                        let matches_chip = (*atmel && chip_type == 2) || (!*atmel && chip_type == 1);
                        if matches_chip {
                            let off = *bank * 0x10000 + base;
                            for (i, &b) in data.iter().enumerate() {
                                if let Some(cell) = mem.get_mut(off + i) {
                                    if *atmel {
                                        *cell = b; // page write erases internally
                                    } else {
                                        *cell &= b; // needs a prior sector erase
                                    }
                                }
                            }
                        }
                    }
                self.address += data.len() as u32;
                self.ack();
            }
            CMD_DMG_MBC7_WRITE_EEPROM => {
                for (i, &b) in data.iter().enumerate() {
                    let addr = self.address as usize + i;
                    if let SimCart::Dmg(cart) = &mut self.cart
                        && let Some(cell) = cart.ram.get_mut(addr) {
                            *cell = b;
                        }
                }
                self.address += data.len() as u32;
                self.ack();
            }
            CMD_FLASH_PROGRAM => self.exec_flash_program(data),
            _ => panic!("sim: unhandled data opcode 0x{cmd:02X}"),
        }
    }

    fn exec_flash_program(&mut self, data: &[u8]) {
        let cfg = self.flash_cfg.expect("FLASH_PROGRAM before SET_FLASH_CMD");
        self.program_chunks += 1;
        let is_dmg = matches!(self.cart, SimCart::Dmg(_));
        let abs = if is_dmg {
            self.dmg_abs_addr(self.address)
        } else {
            self.address as usize * 2
        };
        match &mut self.cart {
            SimCart::Dmg(cart) => {
                if let Some(flash) = &mut cart.flash {
                    flash.program(abs, data);
                }
            }
            SimCart::Agb(cart) => {
                if let Some(flash) = &mut cart.flash {
                    flash.program(abs, data);
                }
            }
            SimCart::Empty => {}
        }
        self.address += if is_dmg { data.len() } else { data.len() / 2 } as u32;

        if self.corrupt_flash_after_write && !self.corrupted {
            self.corrupted = true;
            match &mut self.cart {
                SimCart::Dmg(cart) => {
                    if let Some(flash) = &mut cart.flash {
                        flash.data[1] ^= 0x40;
                    }
                }
                SimCart::Agb(cart) => {
                    if let Some(flash) = &mut cart.flash {
                        flash.data[1] ^= 0x40;
                    }
                }
                SimCart::Empty => {}
            }
        }

        // Buffered mode: mid-group chunks ack 0x03 and the next chunk arrives
        // as raw data (no opcode); the group-completing chunk acks 0x01.
        let buffered = cfg.1 == 2;
        let group = self.var(VAR_BUFFER_SIZE) as usize;
        self.buffer_fill += data.len();
        if buffered && group > 0 && self.buffer_fill < group {
            self.push(&[ACK_CONTINUE]);
            self.expect = Expect::FlashData { len: self.transfer_size() };
        } else {
            self.buffer_fill = 0;
            self.push(&[ACK_OK]);
        }
    }

    /// Try to consume one complete command from `buf`.
    fn pump(&mut self) {
        loop {
            match &self.expect {
                Expect::FlashData { len } => {
                    let len = *len;
                    if self.buf.len() < len {
                        return;
                    }
                    let data: Vec<u8> = self.buf.drain(..len).collect();
                    self.expect = Expect::Opcode;
                    self.exec_flash_program(&data);
                    continue;
                }
                Expect::Data { cmd, arg, len } => {
                    let (cmd, arg, len) = (*cmd, *arg, *len);
                    if self.buf.len() < len {
                        return;
                    }
                    let data: Vec<u8> = self.buf.drain(..len).collect();
                    self.expect = Expect::Opcode;
                    self.exec_data(cmd, arg, &data);
                    continue;
                }
                Expect::FlashCmdHeader => {
                    if self.buf.len() < 3 {
                        return;
                    }
                    let total = 3 + self.buf[2] as usize * 6;
                    if self.buf.len() < total {
                        return;
                    }
                    let frame: Vec<u8> = self.buf.drain(..total).collect();
                    self.expect = Expect::Opcode;
                    self.exec(CMD_CART_WRITE_FLASH_CMD, &frame);
                    continue;
                }
                Expect::Opcode => {}
            }

            let Some(&cmd) = self.buf.first() else { return };
            // Fixed frame lengths per opcode (including the opcode byte).
            let fixed_len = match cmd {
                OFW_CMD_PCB_VER | OFW_CMD_FW_VER | CMD_QUERY_FW_INFO | CMD_SET_MODE_AGB
                | CMD_SET_MODE_DMG | CMD_SET_VOLTAGE_3_3V | CMD_SET_VOLTAGE_5V
                | CMD_SET_ADDR_AS_INPUTS | CMD_DMG_MBC_RESET | CMD_AGB_BOOTUP_SEQUENCE
                | CMD_QUERY_CART_PWR | CMD_CART_PWR_ON | CMD_CART_PWR_OFF | CMD_DMG_CART_READ
                | CMD_AGB_CART_READ | CMD_AGB_CART_READ_SRAM | CMD_DMG_MBC7_READ_EEPROM => Some(1),
                CMD_PING | CMD_AGB_CART_READ_EEPROM => Some(2),
                CMD_CLK_TOGGLE | CMD_CALC_CRC32 => Some(5),
                CMD_SET_VARIABLE => Some(10),
                CMD_GET_VARIABLE | CMD_DMG_CART_WRITE => Some(6),
                CMD_AGB_CART_WRITE => Some(7),
                CMD_SET_FLASH_CMD => Some(40),
                _ => None,
            };
            if let Some(len) = fixed_len {
                if self.buf.len() < len {
                    return;
                }
                let frame: Vec<u8> = self.buf.drain(..len).collect();
                self.exec(cmd, &frame);
                continue;
            }
            // Data-carrying and variable-length commands.
            match cmd {
                CMD_DMG_CART_WRITE_SRAM | CMD_AGB_CART_WRITE_SRAM | CMD_DMG_MBC7_WRITE_EEPROM
                | CMD_FLASH_PROGRAM => {
                    // Opcode-led data chunk (buffered continue-mode chunks
                    // come through Expect::FlashData instead).
                    self.buf.drain(..1);
                    self.expect = Expect::Data { cmd, arg: 0, len: self.transfer_size() };
                }
                CMD_AGB_CART_WRITE_EEPROM | CMD_AGB_CART_WRITE_FLASH_DATA => {
                    if self.buf.len() < 2 {
                        return;
                    }
                    let arg = self.buf[1];
                    self.buf.drain(..2);
                    self.expect = Expect::Data { cmd, arg, len: self.transfer_size() };
                }
                CMD_CART_WRITE_FLASH_CMD => {
                    self.expect = Expect::FlashCmdHeader;
                }
                other => panic!("sim: unknown opcode 0x{other:02X}"),
            }
        }
    }
}

impl Transport for SimGbxCart {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), DeviceError> {
        self.buf.extend_from_slice(buf);
        self.pump();
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), DeviceError> {
        for b in buf.iter_mut() {
            match self.out.pop_front() {
                Some(v) => *b = v,
                None => {
                    return Err(DeviceError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "sim: read with no queued response",
                    )));
                }
            }
        }
        Ok(())
    }

    fn set_timeout(&mut self, _t: Duration) -> Result<(), DeviceError> {
        Ok(())
    }

    fn flush_input(&mut self) {
        self.out.clear();
    }
}
