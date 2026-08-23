//! insideGadgets GBxCart RW v1.4 backend.
//!
//! The GBxCart is the opposite of the Operator: a dumb serial-to-cartridge-bus
//! bridge where the host drives everything the Operator's firmware does
//! internally — MBC banking, save-chip access, flash algorithms, RTC latching.
//! This module is therefore a small "software Operator firmware": it probes the
//! inserted cart, caches its identity, and synthesizes Operator-shaped packets
//! (signature, flashcart-detect result) so the rest of throwback works unchanged.
//!
//! Protocol reference: notes/GBXCART-PROTOCOL.md. Targets PCB v1.4/v1.4a/b/c
//! (and the DMG-only Mini) running Lesserkuma's "L" firmware, version 12+.

pub mod protocol;
pub mod transport;

mod agb;
mod flash;
mod gbmemory;
mod mbc;
mod rtc;

pub use flash::{FlashProfile, FLASH_PROFILES};
pub use mbc::MbcKind;
pub use transport::{SerialTransport, Transport};

use std::time::Duration;

use crate::device::{CartridgeDevice, ChipType, DeviceError};
use protocol::*;

/// Normal command/response timeout once a device is claimed.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Bytes of ROM cached at detection time: one full GB bank 0 (covers the GB
/// header at 0x100-0x14F and the GBA header at 0x00-0xC0 with lots of margin).
const HEADER_SIZE: usize = 0x4000;

/// Which cartridge family the inserted cart belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CartFamily {
    Dmg,
    Agb,
}

/// Everything learned about the inserted cartridge at detection time. Later
/// trait calls (saves, RTC, flashing) consult this instead of re-deriving it.
struct CartState {
    family: CartFamily,
    /// First `HEADER_SIZE` bytes of ROM (GB bank 0 / GBA start).
    header: Vec<u8>,
    /// DMG mapper (meaningless for AGB carts).
    mbc: MbcKind,
    ram_size: u32,
    /// Flash profile, populated lazily by detect_flashcart / write_rom.
    /// `Some(None)` = probed, retail cart; `None` = not probed yet.
    flash: Option<Option<&'static FlashProfile>>,
}

pub struct GbxCart<T: Transport = SerialTransport> {
    io: T,
    pub fw_ver: u16,
    pub pcb_ver: u8,
    /// False on the DMG-only Mini board.
    agb_capable: bool,
    state: Option<CartState>,
}

impl GbxCart<SerialTransport> {
    /// Open and claim a GBxCart on a serial port. Fails fast (short timeout)
    /// when the port is some other CH340 device.
    pub fn open_port(port_name: &str) -> Result<Self, DeviceError> {
        let io = SerialTransport::open(port_name, Duration::from_millis(300))?;
        Self::new(io)
    }

    /// Scan for CH340 serial ports and claim the first one that passes the
    /// GBxCart handshake. Used by hardware tests and probe examples that need
    /// a GBxCart specifically (device::open() prefers Operators).
    pub fn open_first() -> Result<Self, DeviceError> {
        let ports = serialport::available_ports()?;
        let mut last = DeviceError::NotFound;
        for p in &ports {
            if let serialport::SerialPortType::UsbPort(usb) = &p.port_type
                && usb.vid == 0x1A86
                && usb.pid == 0x7523
            {
                match Self::open_port(&p.port_name) {
                    Ok(dev) => return Ok(dev),
                    Err(e) => last = e,
                }
            }
        }
        Err(last)
    }
}

impl<T: Transport> GbxCart<T> {
    /// Handshake-verify and claim a transport. The caller should give the
    /// transport a short timeout so a non-GBxCart port is rejected quickly;
    /// on success the timeout is raised to the normal operating value.
    pub fn new(mut io: T) -> Result<Self, DeviceError> {
        io.flush_input();

        // PCB version via the OFW passthrough query. A foreign CH340 device
        // either times out or answers garbage — both reject the port.
        io.write_all(&[OFW_CMD_PCB_VER])?;
        let mut pcb = [0u8];
        io.read_exact(&mut pcb)?;
        let pcb_ver = pcb[0];
        if ![PCB_V1_4, PCB_V1_4ABC, PCB_MINI].contains(&pcb_ver) {
            return Err(DeviceError::Protocol(format!(
                "unsupported GBxCart PCB version {pcb_ver} (need v1.4-family)"
            )));
        }

        // OFW firmware version byte — informational only under L firmware.
        io.write_all(&[OFW_CMD_FW_VER])?;
        let mut ofw = [0u8];
        io.read_exact(&mut ofw)?;

        // L-firmware identity: size byte (8) + {cfw_id, fw_ver, pcb, build_ts}.
        io.write_all(&[CMD_QUERY_FW_INFO])?;
        let mut size = [0u8];
        io.read_exact(&mut size)?;
        if size[0] != 8 {
            return Err(DeviceError::Protocol(format!(
                "unexpected QUERY_FW_INFO size {}",
                size[0]
            )));
        }
        let mut body = [0u8; 8];
        io.read_exact(&mut body)?;
        let info = parse_fw_info(&body);
        if info.cfw_id != CFW_ID_L {
            return Err(DeviceError::Protocol(format!(
                "not an L-firmware GBxCart (cfw id 0x{:02X})",
                info.cfw_id
            )));
        }
        if info.fw_ver < MIN_FW_VER {
            return Err(DeviceError::Protocol(format!(
                "GBxCart firmware L{} is too old (need L{MIN_FW_VER}+; update with FlashGBX)",
                info.fw_ver
            )));
        }
        if info.fw_ver < TESTED_FW_VER {
            eprintln!(
                "Warning: GBxCart firmware L{} is older than the tested L{TESTED_FW_VER}; \
                 some operations may misbehave.",
                info.fw_ver
            );
        }

        // fw >= 12 appends: name string (len-prefixed), feature byte, bootloader byte.
        let mut name_len = [0u8];
        io.read_exact(&mut name_len)?;
        let mut tail = vec![0u8; name_len[0] as usize + 2];
        io.read_exact(&mut tail)?;

        io.set_timeout(TIMEOUT)?;
        let mut dev = Self {
            io,
            fw_ver: info.fw_ver,
            pcb_ver: info.pcb_ver,
            agb_capable: info.pcb_ver != PCB_MINI,
            state: None,
        };
        // Leave the bus in a safe idle state until a command needs it.
        dev.cmd_ack(&[CMD_SET_ADDR_AS_INPUTS])?;
        Ok(dev)
    }

    /// Access the underlying transport (used by tests to inspect simulator state).
    pub fn transport(&self) -> &T {
        &self.io
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.io
    }

    // --- Low-level helpers ---------------------------------------------------

    /// Read the single ack byte that follows most state-changing commands.
    /// Returns true when the device answered ACK_CONTINUE (0x03).
    fn expect_ack(&mut self) -> Result<bool, DeviceError> {
        let mut b = [0u8];
        self.io.read_exact(&mut b)?;
        match b[0] {
            ACK_OK => Ok(false),
            ACK_CONTINUE => Ok(true),
            ACK_ERROR => Err(DeviceError::Protocol("device reported an error (ack 0x02)".into())),
            other => Err(DeviceError::Protocol(format!(
                "unexpected ack byte 0x{other:02X}"
            ))),
        }
    }

    /// Send a command that is acknowledged with a status byte.
    fn cmd_ack(&mut self, frame: &[u8]) -> Result<(), DeviceError> {
        self.io.write_all(frame)?;
        self.expect_ack()?;
        Ok(())
    }

    /// Send a command and drain whatever it answers. Hardware-verified (L14,
    /// v1.4a): the idle/reset/bootup commands all ack 0x01, so normal call
    /// sites use `cmd_ack`; this variant remains only for the between-ops
    /// `idle()` parking, where the drain doubles as a resync barrier that
    /// swallows any stray bytes from an aborted operation.
    fn cmd_drain(&mut self, frame: &[u8]) -> Result<(), DeviceError> {
        self.io.write_all(frame)?;
        self.io.flush_input();
        Ok(())
    }

    fn set_variable(&mut self, var: Var, value: u32) -> Result<(), DeviceError> {
        self.cmd_ack(&set_variable_frame(var, value))
    }

    /// Single DMG bus write (bank registers, RAM enable, RTC registers...).
    pub(crate) fn dmg_write(&mut self, addr: u16, value: u8) -> Result<(), DeviceError> {
        self.cmd_ack(&dmg_write_frame(addr, value))
    }

    /// Stream `len` bytes via a read opcode. TRANSFER_SIZE and ADDRESS must be
    /// set beforehand; the firmware auto-increments its address, so this issues
    /// one opcode per TRANSFER_SIZE chunk. `len` must be a multiple of `chunk`.
    fn read_stream(&mut self, opcode: u8, chunk: usize, len: usize) -> Result<Vec<u8>, DeviceError> {
        debug_assert!(len.is_multiple_of(chunk));
        let mut out = vec![0u8; len];
        for part in out.chunks_mut(chunk) {
            self.io.write_all(&[opcode])?;
            self.io.read_exact(part)?;
        }
        Ok(out)
    }

    /// Set TRANSFER_SIZE + ADDRESS, then stream `len` bytes.
    fn read_at(
        &mut self,
        opcode: u8,
        address: u32,
        chunk: u16,
        len: usize,
    ) -> Result<Vec<u8>, DeviceError> {
        self.set_variable(VAR_TRANSFER_SIZE, chunk as u32)?;
        self.set_variable(VAR_ADDRESS, address)?;
        self.read_stream(opcode, chunk as usize, len)
    }

    // --- Mode entry ----------------------------------------------------------

    /// Enter AGB mode: 3.3 V, AGB bus timing, cart powered, bootup sequence run.
    pub(crate) fn enter_agb_mode(&mut self) -> Result<(), DeviceError> {
        self.cmd_ack(&[CMD_SET_MODE_AGB])?;
        self.cmd_ack(&[CMD_SET_VOLTAGE_3_3V])?;
        self.set_variable(VAR_AGB_READ_METHOD, 0)?;
        self.set_variable(VAR_CART_MODE, CART_MODE_AGB)?;
        self.set_variable(VAR_AGB_IRQ_ENABLED, 0)?;
        self.set_variable(VAR_ADDRESS, 0)?;
        self.power_on()?;
        self.cmd_ack(&[CMD_AGB_BOOTUP_SEQUENCE])?;
        Ok(())
    }

    /// Enter DMG mode: 5 V, DMG bus timing, cart powered, MBC reset.
    pub(crate) fn enter_dmg_mode(&mut self) -> Result<(), DeviceError> {
        self.cmd_ack(&[CMD_SET_MODE_DMG])?;
        self.cmd_ack(&[CMD_SET_VOLTAGE_5V])?;
        self.set_variable(VAR_DMG_READ_METHOD, DMG_READ_METHOD_A15)?;
        self.set_variable(VAR_CART_MODE, CART_MODE_DMG)?;
        self.set_variable(VAR_ADDRESS, 0)?;
        self.power_on()?;
        self.mbc_reset()?;
        Ok(())
    }

    /// Reset the MBC state (DMG mode).
    pub(crate) fn mbc_reset(&mut self) -> Result<(), DeviceError> {
        self.cmd_ack(&[CMD_DMG_MBC_RESET])
    }

    /// Power the cartridge slot if the firmware reports it off.
    fn power_on(&mut self) -> Result<(), DeviceError> {
        self.io.write_all(&[CMD_QUERY_CART_PWR])?;
        let mut b = [0u8];
        self.io.read_exact(&mut b)?;
        if b[0] == 0 {
            self.cmd_ack(&[CMD_CART_PWR_ON])?;
        }
        Ok(())
    }

    // --- Detection -----------------------------------------------------------

    /// Probe the slot and cache the cart's identity. AGB is probed first at
    /// 3.3 V — safe for a DMG cart, whereas probing DMG first would put 5 V on
    /// a GBA cart. Returns None when neither family validates (empty slot or
    /// dirty contacts — same "reads as no cartridge" behavior as the Operator).
    fn detect_cart(&mut self) -> Result<Option<()>, DeviceError> {
        self.state = None;

        if self.agb_capable {
            self.enter_agb_mode()?;
            let header = self.read_at(CMD_AGB_CART_READ, 0, MAX_BUFFER_READ, HEADER_SIZE)?;
            if agb_header_valid(&header) {
                // Operator parity: no ROM/save sizes for GBA — main.rs derives
                // them from a max-size dump + trim.
                self.state = Some(CartState {
                    family: CartFamily::Agb,
                    header,
                    mbc: MbcKind::None,
                    ram_size: 0,
                    flash: None,
                });
                self.idle();
                return Ok(Some(()));
            }
        }

        self.enter_dmg_mode()?;
        let header = self.read_dmg_rom_chunk(0, HEADER_SIZE)?;
        if gb_header_valid(&header) {
            let base_mbc = MbcKind::from_header_byte(header[0x147]);
            let rom_size = gb_rom_size(header[0x148]);
            let ram_size = gb_ram_size_with_mbc2_shim(&header, base_mbc);
            let mbc = mbc::refine_mbc3(base_mbc, rom_size, ram_size);
            self.state = Some(CartState {
                family: CartFamily::Dmg,
                header,
                mbc,
                ram_size,
                flash: None,
            });
            self.idle();
            return Ok(Some(()));
        }

        // The header didn't validate. It may still be a flashcart holding a
        // headerless or corrupt image (mid-development, or a previous write that
        // left non-ROM data) — such a cart must stay re-writeable. Probe for a
        // supported flash chip: if one answers, treat it as a present, flashable
        // DMG cart (MBC5 wiring, as all our DMG flashcarts use). An empty slot
        // has no chip and stays "not present".
        self.state = Some(CartState {
            family: CartFamily::Dmg,
            header,
            mbc: MbcKind::Mbc5,
            ram_size: 0,
            flash: None,
        });
        let matched = match self.probe_flash_id() {
            Ok(Some(id)) => flash::match_profile(CartFamily::Dmg, &id),
            _ => None,
        };
        if let Some(profile) = matched {
            if let Some(state) = self.state.as_mut() {
                state.flash = Some(Some(profile));
            }
            self.idle();
            return Ok(Some(()));
        }

        self.state = None;
        self.idle();
        Ok(None)
    }

    /// Read `len` bytes of DMG ROM address space starting at `address`
    /// (no banking — callers below 0x8000 only).
    pub(crate) fn read_dmg_rom_chunk(&mut self, address: u32, len: usize) -> Result<Vec<u8>, DeviceError> {
        self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_ROM_READ)?;
        let chunk = (MAX_BUFFER_READ as usize).min(len) as u16;
        self.read_at(CMD_DMG_CART_READ, address, chunk, len)
    }

    /// Park the bus between operations (drains rather than reads the ack, so
    /// it also resyncs the stream after an aborted operation).
    fn idle(&mut self) {
        let _ = self.cmd_drain(&[CMD_SET_ADDR_AS_INPUTS]);
    }

    /// Borrow the cached cart state, re-detecting if needed (every CLI flow
    /// calls read_cartridge_info first, so this is a safety net, not the norm).
    fn require_state(&mut self) -> Result<&CartState, DeviceError> {
        if self.state.is_none() {
            self.detect_cart()?;
        }
        self.state
            .as_ref()
            .ok_or(DeviceError::Unsupported("no cartridge inserted"))
    }
}

// --- Header validation & size derivation (pure) ------------------------------

/// GBA header sanity: the fixed byte 0x96 at 0xB2 plus a valid complement
/// checksum — both are enforced by the GBA BIOS, so every bootable cart
/// (retail or homebrew) passes, while open-bus/garbage reads fail.
pub fn agb_header_valid(header: &[u8]) -> bool {
    header.len() > 0xBD
        && header[0xB2] == 0x96
        && crate::cartridge::gba_header_checksum(header) == Some(header[0xBD])
}

/// GB header sanity: valid header checksum at 0x14D (BIOS-enforced) plus a
/// non-degenerate logo region (rejects all-0x00/all-0xFF open-bus reads that
/// would otherwise checksum to a false positive).
pub fn gb_header_valid(header: &[u8]) -> bool {
    if header.len() < 0x150 {
        return false;
    }
    let logo = &header[0x104..0x134];
    if logo.iter().all(|&b| b == 0x00) || logo.iter().all(|&b| b == 0xFF) {
        return false;
    }
    crate::cartridge::gb_header_checksum(header) == Some(header[0x14D])
}

fn gb_rom_size(code: u8) -> u32 {
    if code <= 8 { 32 * 1024 * (1u32 << code) } else { 0 }
}

/// GB RAM size from the header code, with the MBC2 shim: MBC2 carts declare
/// RAM code 0x00 but carry 512x4-bit internal RAM; report 2 KB so callers'
/// `ram_size > 0` gates pass (reads pad 512 B to 2 KB, writes mask back).
fn gb_ram_size_with_mbc2_shim(header: &[u8], mbc: MbcKind) -> u32 {
    if mbc == MbcKind::Mbc2 {
        return 2 * 1024;
    }
    match header[0x149] {
        0x01 => 2 * 1024,
        0x02 => 8 * 1024,
        0x03 => 32 * 1024,
        0x04 => 128 * 1024,
        0x05 => 64 * 1024,
        _ => 0,
    }
}

// --- Signature synthesis (pure) ----------------------------------------------

/// Build an Operator-shaped 64-byte signature packet from a GB cart header, so
/// `CartridgeInfo::from_bytes` (and `info --raw`) work unchanged. Field layout
/// mirrors notes/OPERATOR-PROTOCOL.md §ReadSignature.
pub fn synthesize_gb_signature(header: &[u8]) -> [u8; 64] {
    let mut sig = [0u8; 64];
    sig[2] = 0x20; // family marker: GB
    sig[3] = 1; // present
    sig[0x0D] = header[0x134]; // first title char
    sig[0x0E] = header[0x147]; // MBC type
    sig[0x0F] = header[0x148]; // ROM size code
    sig[0x10] = if MbcKind::from_header_byte(header[0x147]) == MbcKind::Mbc2 {
        0x01 // MBC2 shim (see gb_ram_size_with_mbc2_shim)
    } else {
        header[0x149] // RAM size code
    };
    sig[0x11] = header[0x14D]; // header checksum
    // Global checksum: big-endian in the cart header, little-endian in the packet.
    sig[0x12] = header[0x14F];
    sig[0x13] = header[0x14E];
    sig
}

/// Build an Operator-shaped 64-byte signature packet from a GBA cart header.
/// Like the Operator, no ROM/save sizes are reported — callers derive them from
/// a max-size dump + trim.
pub fn synthesize_gba_signature(header: &[u8]) -> [u8; 64] {
    let mut sig = [0u8; 64];
    sig[2] = 0x30; // family marker: GBA
    sig[3] = 1; // present
    sig[0x0D] = header[0xA0]; // first title char
    sig[0x0E] = header[0xAC]; // game code bytes 1-3
    sig[0x0F] = header[0xAD];
    sig[0x10] = header[0xAE];
    sig[0x11] = header[0xAF]; // region letter
    sig
}

/// Signature packet for an empty (or unreadable) slot: presence bytes zero.
pub fn synthesize_absent_signature() -> [u8; 64] {
    [0u8; 64]
}

// --- CartridgeDevice ---------------------------------------------------------

impl<T: Transport> CartridgeDevice for GbxCart<T> {
    fn read_cartridge_info(&mut self) -> Result<[u8; 64], DeviceError> {
        if self.detect_cart()?.is_none() {
            return Ok(synthesize_absent_signature());
        }
        let state = self.state.as_ref().expect("state cached by detect_cart");
        Ok(match state.family {
            CartFamily::Dmg => synthesize_gb_signature(&state.header),
            CartFamily::Agb => synthesize_gba_signature(&state.header),
        })
    }

    fn read_header(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.require_state()?;
        Ok(self.state.as_ref().unwrap().header.clone())
    }

    fn read_rom(
        &mut self,
        _chip: ChipType,
        rom_size: u32,
        _save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let family = self.require_state()?.family;
        let rom = match family {
            CartFamily::Dmg => {
                let mbc = self.state.as_ref().unwrap().mbc;
                self.dump_dmg_rom(mbc, rom_size, progress)
            }
            CartFamily::Agb => self.dump_agb_rom(rom_size, progress),
        };
        self.idle();
        rom
    }

    fn read_save(
        &mut self,
        chip: ChipType,
        _rom_size: u32,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let family = self.require_state()?.family;
        let save = match family {
            CartFamily::Dmg => {
                let mbc = self.state.as_ref().unwrap().mbc;
                self.read_dmg_save(mbc, save_size, progress)
            }
            CartFamily::Agb => self.read_agb_save(chip, save_size, progress),
        };
        self.idle();
        save
    }

    fn write_save(
        &mut self,
        chip: ChipType,
        _rom_size: u32,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError> {
        let family = self.require_state()?.family;
        let result = match family {
            CartFamily::Dmg => {
                let (mbc, ram_size) = {
                    let s = self.state.as_ref().unwrap();
                    (s.mbc, s.ram_size)
                };
                // main.rs sizes the payload from the signature's ram_size;
                // clamp defensively so a mismatched file can't overrun banks.
                let len = (data.len() as u32).min(ram_size) as usize;
                self.write_dmg_save(mbc, &data[..len], progress)
            }
            CartFamily::Agb => self.write_agb_save(chip, data, progress),
        };
        self.idle();
        result
    }

    fn write_rom(
        &mut self,
        data: &[u8],
        _save_size: u32,
        progress: &dyn Fn(u32),
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError> {
        let result = self.flash_rom(data, progress, erase_progress);
        self.idle();
        result
    }

    fn detect_flashcart(&mut self) -> Result<[u8; 64], DeviceError> {
        self.require_state()?;
        let packet = self.flashcart_probe_packet();
        self.idle();
        packet
    }

    fn read_rtc(&mut self, _rom_size: u32, _save_size: u32) -> Result<Vec<u8>, DeviceError> {
        let state = self.require_state()?;
        if state.family != CartFamily::Dmg {
            return Err(DeviceError::Unsupported("RTC is only available on GB cartridges"));
        }
        let payload = self.read_mbc3_rtc();
        self.idle();
        payload
    }

    fn write_rtc(&mut self, _rom_size: u32, _save_size: u32, data: &[u8]) -> Result<(), DeviceError> {
        let state = self.require_state()?;
        if state.family != CartFamily::Dmg {
            return Err(DeviceError::Unsupported("RTC is only available on GB cartridges"));
        }
        let result = self.write_mbc3_rtc(data);
        self.idle();
        result
    }

    fn is_gb_memory(&mut self) -> Result<bool, DeviceError> {
        self.detect_gb_memory()
    }

    fn read_gb_memory(&mut self, progress: &dyn Fn(u32)) -> Result<(Vec<u8>, Vec<u8>), DeviceError> {
        self.read_gb_memory(progress)
    }

    fn read_gb_memory_map(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.read_gb_memory_map()
    }

    fn write_gb_memory(
        &mut self,
        image: &[u8],
        map: &[u8],
        progress: &dyn Fn(u32),
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError> {
        self.write_gb_memory(image, map, progress, erase_progress)
    }
}
