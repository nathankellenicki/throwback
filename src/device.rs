use crc::{Crc, CRC_32_MPEG_2};
use std::io::{Read, Write};
use std::time::Duration;
use thiserror::Error;

const PACKET_SIZE: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(2);

const CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_MPEG_2);

// Command bytes
const CMD_READ_GAME: u8 = 0x00;
const CMD_WRITE_GAME: u8 = 0x01;
const CMD_READ_SAVE: u8 = 0x02;
const CMD_WRITE_SAVE: u8 = 0x03;
const CMD_READ_SIGNATURE: u8 = 0x04;
const CMD_READ_RTC: u8 = 0x09;
const CMD_WRITE_RTC: u8 = 0x10;
const CMD_DETECT_FLASHCART: u8 = 0x15;

/// MBC3 RTC payload: 5 registers (sec/min/hour/day-low/day-ctrl), current + latched,
/// each as a u32 little-endian (the standard emulator .rtc layout). 10 × 4 = 40 bytes.
const RTC_PAYLOAD_LEN: usize = 40;

// USB identifiers (VID 0x16D0 firmware 9+, 0x1D50 older).
const VID_NEW: u16 = 0x16D0;
const VID_OLD: u16 = 0x1D50;
/// SN Operator product ID (confirmed against hardware: "SN Operator", 0x16D0:0x123E).
const PID_SN_OPERATOR: u16 = 0x123E;

/// Bytes to read to cover a cartridge's internal header without a full dump.
/// GB header lives in bank 0 (title at 0x0134); SNES header sits near the end of
/// the first/second bank (LoROM 0x7FC0, HiROM 0xFFC0). Both devices clamp ReadGame
/// to the requested size and terminate with a DONE frame (verified on hardware).
const GB_HEADER_READ: u32 = 0x4000; // one 16 KB bank
const SNES_HEADER_READ: u32 = 0x10000; // 64 KB — covers LoROM and HiROM

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChipType {
    Unknown = 0,
    Eeprom = 1,
    Sram = 2,
    Flash = 3,
}

/// Which Epilogue device (and therefore which cartridge family) we're talking to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// GB Operator — GB/GBC/GBA cartridges, legacy protocol.
    GbOperator,
    /// SN Operator — SNES/Super Famicom cartridges, streaming protocol.
    SnOperator,
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("Operator device not found")]
    NotFound,
    #[error("Serial port error: {0}")]
    Serial(#[from] serialport::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("No response from device")]
    NoResponse,
    #[error("{0}")]
    Unsupported(&'static str),
}

/// Check if a chunk looks like GBA open bus (sequential u16 from 0).
pub fn is_open_bus(data: &[u8]) -> bool {
    if data.len() < 4 { return false; }
    for j in (0..data.len() - 1).step_by(2) {
        let expected = (j / 2) as u16;
        let actual = u16::from_le_bytes([data[j], data[j + 1]]);
        if actual != expected { return false; }
    }
    true
}

pub fn build_command(command: u8, chip: ChipType, rom_size: u32, save_size: u32) -> [u8; PACKET_SIZE] {
    let mut packet = [0u8; PACKET_SIZE];
    packet[0] = command;
    packet[1] = chip as u8;
    packet[2..6].copy_from_slice(&rom_size.to_le_bytes());
    packet[6..10].copy_from_slice(&save_size.to_le_bytes());

    let checksum = CRC.checksum(&packet[..60]);
    packet[60..64].copy_from_slice(&checksum.to_le_bytes());
    packet
}

/// Operations any Operator device exposes. Object-safe so the right implementation
/// can be selected at runtime by device kind (`Box<dyn CartridgeDevice>`).
pub trait CartridgeDevice {
    /// Read the 64-byte cartridge signature packet.
    fn read_cartridge_info(&mut self) -> Result<[u8; 64], DeviceError>;

    /// Read just enough of the ROM to cover the cartridge's internal header (full
    /// title etc.) without dumping the whole ROM. The byte count is device-specific.
    fn read_header(&mut self) -> Result<Vec<u8>, DeviceError>;

    fn read_rom(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError>;

    fn read_save(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError>;

    fn write_save(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError>;

    fn write_rom(
        &mut self,
        data: &[u8],
        save_size: u32,
        progress: &dyn Fn(u32),
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError>;

    /// Probe whether the inserted cart is a writeable flashcart (read-only, no
    /// erase). Returns the raw FlashcartDetectionResult packet; decode with
    /// `cartridge::flashcart_writeable`. Unsupported devices error.
    fn detect_flashcart(&mut self) -> Result<[u8; 64], DeviceError> {
        Err(DeviceError::Unsupported("flashcart detection not supported on this device"))
    }

    /// Read the MBC3 real-time clock (40-byte payload: 5 registers, current +
    /// latched). Only meaningful on GB carts with an RTC; unsupported devices error.
    fn read_rtc(&mut self, _rom_size: u32, _save_size: u32) -> Result<Vec<u8>, DeviceError> {
        Err(DeviceError::Unsupported("RTC is not supported on this device"))
    }

    /// Write the MBC3 real-time clock (40-byte payload, same layout as read_rtc).
    fn write_rtc(
        &mut self,
        _rom_size: u32,
        _save_size: u32,
        _data: &[u8],
    ) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported("RTC is not supported on this device"))
    }
}

/// Find the first Operator serial port, returning its name and the device kind.
fn find_operator() -> Result<(String, DeviceKind), DeviceError> {
    let ports = serialport::available_ports()?;
    ports
        .iter()
        .find_map(|p| match &p.port_type {
            serialport::SerialPortType::UsbPort(usb)
                if usb.vid == VID_NEW || usb.vid == VID_OLD =>
            {
                let kind = if usb.pid == PID_SN_OPERATOR {
                    DeviceKind::SnOperator
                } else {
                    DeviceKind::GbOperator
                };
                Some((p.port_name.clone(), kind))
            }
            _ => None,
        })
        .ok_or(DeviceError::NotFound)
}

/// Open whichever Operator device is connected, dispatching to the right protocol
/// implementation based on the detected product ID.
pub fn open() -> Result<Box<dyn CartridgeDevice>, DeviceError> {
    let (port_name, kind) = find_operator()?;
    let io = Serial::open(&port_name)?;
    Ok(match kind {
        DeviceKind::GbOperator => Box::new(LegacyDevice { io }),
        DeviceKind::SnOperator => Box::new(StreamingDevice { io }),
    })
}

/// Shared serial transport + low-level framing helpers. The command/response framing
/// (`C0 DE` magic, zero-padding packets, 64-byte command packets, CRC-32/MPEG-2) is
/// identical between the legacy and streaming protocols at the serial layer — verified
/// against an SN Operator via a `ReadSignature` probe — so it lives here and is shared
/// by both device implementations.
struct Serial {
    port: Box<dyn serialport::SerialPort>,
}

impl Serial {
    fn open(port_name: &str) -> Result<Self, DeviceError> {
        let mut port = serialport::new(port_name, 115200).timeout(TIMEOUT).open()?;
        // Assert DTR+RTS as the device init sequence requires.
        let _ = port.write_data_terminal_ready(true);
        let _ = port.write_request_to_send(true);
        Ok(Self { port })
    }

    fn send(&mut self, packet: &[u8; PACKET_SIZE]) -> Result<(), DeviceError> {
        self.port.write_all(packet)?;
        Ok(())
    }

    fn send_ack(&mut self) -> Result<(), DeviceError> {
        let zeros = [0u8; PACKET_SIZE];
        self.send(&zeros)
    }

    fn drain(&mut self, n: usize) -> Result<(), DeviceError> {
        let mut buf = vec![0u8; n];
        self.port.read_exact(&mut buf)?;
        Ok(())
    }

    /// Quickly drain any pending input (stale bytes from a prior op) using a short
    /// timeout, so a fresh read doesn't have to eat the full 2s read timeout when
    /// the buffer is already clean.
    fn flush_input(&mut self) {
        let saved = self.port.timeout();
        let _ = self.port.set_timeout(Duration::from_millis(20));
        let mut tmp = [0u8; 4096];
        while matches!(self.port.read(&mut tmp), Ok(n) if n > 0) {}
        let _ = self.port.set_timeout(saved);
    }

    /// Read until timeout, return all bytes received.
    fn read_until_timeout(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match self.port.read(&mut tmp) {
                Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                _ => break,
            }
        }
        buf
    }

    /// Send ReadSignature and return the first real (non-framing, non-padding) data packet.
    fn read_signature(&mut self) -> Result<[u8; 64], DeviceError> {
        let packet = build_command(CMD_READ_SIGNATURE, ChipType::Unknown, 0, 0);
        self.send(&packet)?;

        let buf = self.read_until_timeout();

        let mut data = [0u8; 64];
        let mut i = 0;
        while i + 64 <= buf.len() {
            let chunk = &buf[i..i + 64];
            i += 64;
            if chunk[0] == 0xC0 && chunk[1] == 0xDE { continue; }
            if chunk.iter().all(|&b| b == 0) { continue; }
            if chunk.iter().all(|&b| b == 0 || b == 0xAA) { continue; }
            data.copy_from_slice(chunk);
            return Ok(data);
        }

        Err(DeviceError::NoResponse)
    }

    fn read_game(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        // Clear any stale bytes (e.g. a trailing frame from a prior read_save) so the
        // 512-byte lead-in stays aligned when ops are chained (read-camera --framed).
        self.flush_input();
        let packet = build_command(CMD_READ_GAME, chip, rom_size, save_size);
        self.send(&packet)?;
        self.drain(512)?;

        let total_packets = rom_size as usize / 256;
        let mut rom = Vec::with_capacity(rom_size as usize);

        for i in 0..total_packets {
            if i % 64 == 0 {
                self.send_ack()?;
            }
            let mut buf = [0u8; 256];
            self.port.read_exact(&mut buf)?;

            let len = rom.len();
            if len >= 1024 * 1024 && len.is_power_of_two() && is_open_bus(&buf) {
                return Ok(rom);
            }

            rom.extend_from_slice(&buf);
            progress(rom.len() as u32);
        }

        Ok(rom)
    }

    fn read_save_data(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = build_command(CMD_READ_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;
        self.drain(512)?;

        let total_packets = save_size as usize / 256;
        let mut save = Vec::with_capacity(save_size as usize);

        for _ in 0..total_packets {
            let mut buf = [0u8; 256];
            self.port.read_exact(&mut buf)?;
            save.extend_from_slice(&buf);
            progress(save.len() as u32);
        }

        // Consume the trailing DONE frame + padding so a chained op (e.g. read-camera
        // --framed reads the save then the ROM) doesn't desync on leftover bytes.
        let _ = self.read_until_timeout();
        Ok(save)
    }

    fn write_save_data(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError> {
        let save_size = data.len() as u32;

        // Flush stale data
        self.read_until_timeout();

        let packet = build_command(CMD_WRITE_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;
        self.drain(512)?;

        self.port.set_timeout(Duration::from_secs(20))?;

        for (i, chunk) in data.chunks(PACKET_SIZE).enumerate() {
            let mut buf = [0u8; PACKET_SIZE];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.port.write_all(&buf)?;

            if i == 0 {
                self.port.set_timeout(TIMEOUT)?;
            }

            progress(((i + 1) * PACKET_SIZE).min(data.len()) as u32);
        }

        Ok(())
    }

    /// Probe the cart for flashcart detection (GB Operator). READ-ONLY — sends
    /// DetectFlashcart (0x15) and returns the FlashcartDetectionResult packet. No
    /// erase/write. Framing: READY `C0 DE 00 15`, 512-byte lead-in, result packet,
    /// DONE `C0 DE 01 15`, trailer. Verified: a flashcart returns flash-chip
    /// descriptors, a retail cart returns `0x20` + zeros.
    fn detect_flashcart_result(&mut self) -> Result<[u8; 64], DeviceError> {
        self.flush_input();
        let packet = build_command(CMD_DETECT_FLASHCART, ChipType::Unknown, 0, 0);
        self.send(&packet)?;
        self.port.set_timeout(TIMEOUT)?;

        // Skip the READY frame + lead-in zeros; the first non-framing, non-zero
        // 64-byte packet is the result. Bounded so we never spin.
        for _ in 0..32 {
            let mut buf = [0u8; 64];
            self.port.read_exact(&mut buf)?;
            let is_frame = buf[0] == 0xC0 && buf[1] == 0xDE;
            if is_frame && buf[2] == 0x01 {
                break; // DONE frame before any result
            }
            if !is_frame && buf.iter().any(|&b| b != 0) {
                let _ = self.read_until_timeout(); // drain trailer
                return Ok(buf);
            }
        }
        let _ = self.read_until_timeout();
        Err(DeviceError::NoResponse)
    }

    /// Read the MBC3 RTC (GB Operator). Framing matches the other reads: send
    /// ReadRTC, drain the 512-byte READY lead-in, read the 40-byte payload, drain
    /// the trailing padding + DONE frame. Verified on a Pokemon Crystal cartridge:
    /// READY `C0 DE 00 09 F6` / DONE `C0 DE 01 09 F6`, payload = 10 × u32-LE (5 RTC
    /// registers, current + latched).
    fn read_rtc_data(&mut self, rom_size: u32, save_size: u32) -> Result<Vec<u8>, DeviceError> {
        self.read_until_timeout(); // flush stale

        let packet = build_command(CMD_READ_RTC, ChipType::Unknown, rom_size, save_size);
        self.send(&packet)?;
        self.drain(512)?; // READY frame + lead-in

        let mut buf = [0u8; RTC_PAYLOAD_LEN];
        self.port.read_exact(&mut buf)?;

        let _ = self.read_until_timeout(); // padding + DONE frame + trailer
        Ok(buf.to_vec())
    }

    /// Write the MBC3 RTC (GB Operator). Symmetric to `read_rtc_data`: send
    /// WriteRTC, drain the 512-byte READY lead-in, stream the 40-byte payload, drain
    /// the DONE frame + trailer. Verified (framing) on Pokemon Crystal: READY
    /// `C0 DE 00 10 EF` / DONE `C0 DE 01 10 EF`. (A cart with a dead RTC battery
    /// won't persist the values, but the transfer completes.)
    fn write_rtc_data(
        &mut self,
        rom_size: u32,
        save_size: u32,
        data: &[u8],
    ) -> Result<(), DeviceError> {
        self.read_until_timeout(); // flush stale

        let packet = build_command(CMD_WRITE_RTC, ChipType::Unknown, rom_size, save_size);
        self.send(&packet)?;
        self.drain(512)?; // READY frame + lead-in

        self.port.set_timeout(Duration::from_secs(5))?;
        self.port.write_all(data)?;
        self.port.flush()?;
        self.port.set_timeout(TIMEOUT)?;

        let _ = self.drain(512); // DONE frame + trailer
        Ok(())
    }

    /// Streaming-protocol ROM read (SN Operator). Unlike the legacy `read_game`,
    /// the device pure-streams the entire ROM with no per-chunk ACK handshake:
    /// after the 512-byte lead-in (READY frame + zero padding) it sends exactly
    /// `rom_size` payload bytes, then a `C0 DE 01` DONE frame. Verified against a
    /// Desert Strike cartridge: a 1 MB request returned exactly 0x100000 bytes
    /// framed by READY/DONE, with no ACKs sent. Injecting legacy ACK packets here
    /// would risk desyncing the stream, so this path sends none.
    fn stream_game(
        &mut self,
        rom_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        // Flush any stale bytes (e.g. a trailing DONE frame from a prior op).
        self.flush_input();

        let packet = build_command(CMD_READ_GAME, ChipType::Unknown, rom_size, 0);
        self.send(&packet)?;
        self.drain(512)?; // READY frame + zero-padding lead-in

        let target = rom_size as usize;
        let mut rom = Vec::with_capacity(target);
        let mut buf = [0u8; 4096];
        while rom.len() < target {
            let want = (target - rom.len()).min(buf.len());
            self.port.read_exact(&mut buf[..want])?;
            rom.extend_from_slice(&buf[..want]);
            progress(rom.len() as u32);
        }

        // Consume the trailing DONE frame + padding (a fixed 512-byte block, same as
        // the lead-in) so the next command starts from a clean buffer. Best-effort.
        let _ = self.drain(512);

        Ok(rom)
    }

    /// Streaming-protocol save read (SN Operator). Mirrors `stream_game`: after the
    /// 512-byte lead-in (READY frame + padding) the device pure-streams exactly
    /// `save_size` SRAM bytes, then a `C0 DE 01 02` DONE frame + 512-byte trailer.
    /// Verified against a Donkey Kong Country cartridge (HiROM, 2 KB SRAM): a
    /// ReadSave request returned exactly 0x800 bytes framed by READY/DONE with no
    /// ACKs. `rom_size` is still sent so the firmware can map the cart's SRAM.
    fn stream_save(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: &dyn Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        self.flush_input();

        let packet = build_command(CMD_READ_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;
        self.drain(512)?; // READY frame + zero-padding lead-in

        let target = save_size as usize;
        let mut save = Vec::with_capacity(target);
        let mut buf = [0u8; 4096];
        while save.len() < target {
            let want = (target - save.len()).min(buf.len());
            self.port.read_exact(&mut buf[..want])?;
            save.extend_from_slice(&buf[..want]);
            progress(save.len() as u32);
        }

        let _ = self.drain(512); // trailing DONE frame + padding
        Ok(save)
    }

    /// Streaming-protocol save write (SN Operator). Symmetric to `stream_save`:
    /// send WriteSave, drain the device's 512-byte READY lead-in (`C0 DE 00 03`),
    /// pure-stream the SRAM bytes (no per-chunk ACKs), then drain the 512-byte DONE
    /// trailer (`C0 DE 01 03`). Verified against Donkey Kong Country with a SAFE
    /// round-trip (write the just-read bytes back, re-read byte-identical).
    fn stream_save_write(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        data: &[u8],
        progress: &dyn Fn(u32),
    ) -> Result<(), DeviceError> {
        self.flush_input();

        let packet = build_command(CMD_WRITE_SAVE, chip, rom_size, data.len() as u32);
        self.send(&packet)?;
        self.drain(512)?; // READY frame + zero-padding lead-in

        self.port.set_timeout(Duration::from_secs(20))?;
        for (i, chunk) in data.chunks(4096).enumerate() {
            self.port.write_all(chunk)?;
            if i == 0 {
                self.port.set_timeout(TIMEOUT)?;
            }
            progress((i * 4096 + chunk.len()) as u32);
        }
        self.port.flush()?;

        let _ = self.drain(512); // trailing DONE frame + padding
        Ok(())
    }

    /// Write a ROM to a flashcart (GB Operator). Reconstructed to match Playback's
    /// byte-level transcript exactly (captured 2026-06-05 on a homebrew flashcart):
    ///
    /// 1. DetectFlashcart (required before WriteGame); drain its response.
    /// 2. WriteGame — the device then ERASES the flash itself, streaming 64-byte
    ///    `0xEE..` progress packets (~9 of them, ~500 ms apart) while the host only
    ///    LISTENS. (`save_size` is sent so the firmware can size the operation.)
    /// 3. CRITICAL TIMING: the instant erase ends (the first non-EE packet after the
    ///    EE run), the device expects the ROM data IMMEDIATELY — if the host dawdles
    ///    it times out and resets the USB (this was the long-standing bug). So we
    ///    stream the ROM the moment erase completes, blind, in 64-byte packets with
    ///    no reads/delays — exactly as Playback does (1024 back-to-back writes).
    fn write_game(
        &mut self,
        data: &[u8],
        save_size: u32,
        progress: &dyn Fn(u32),
        erase_progress: &dyn Fn(&str),
    ) -> Result<(), DeviceError> {
        let rom_size = data.len() as u32;
        self.flush_input();

        // Step 1: DetectFlashcart, then drain its READY/result/DONE response.
        erase_progress("Preparing cartridge...");
        let detect = build_command(CMD_DETECT_FLASHCART, ChipType::Unknown, 0, 0);
        self.send(&detect)?;
        self.port.set_timeout(Duration::from_millis(1500))?;
        let _ = self.read_until_timeout();

        // Step 2: WriteGame — device-driven erase, host listens for 0xEE packets.
        erase_progress("Erasing flash...");
        let packet = build_command(CMD_WRITE_GAME, ChipType::Unknown, rom_size, save_size);
        self.send(&packet)?;

        // Read 64-byte-aligned packets: skip the READY frame + lead-in zeros, count
        // the 0xEE erase-progress packets, and break the instant a non-EE packet
        // arrives after the erase run (the terminator) — then write WITHOUT delay.
        self.port.set_timeout(Duration::from_secs(5))?;
        let mut seen_erase = false;
        loop {
            let mut buf = [0u8; 64];
            self.port.read_exact(&mut buf)?;
            if buf[0] == 0xEE {
                seen_erase = true;
                erase_progress("Erasing...");
                continue;
            }
            if seen_erase {
                break; // first non-EE packet after the erase run = erase complete
            }
            // else: READY frame / lead-in zeros (before erase starts) — keep reading.
        }

        // Step 3: Stream the ROM immediately — blind 64-byte packets, no ACKs/reads.
        erase_progress("Writing...");
        self.port.set_timeout(TIMEOUT)?;
        for (i, chunk) in data.chunks(PACKET_SIZE).enumerate() {
            let mut buf = [0u8; PACKET_SIZE];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.port.write_all(&buf)?;
            progress(((i + 1) * PACKET_SIZE).min(data.len()) as u32);
        }
        self.port.flush()?;

        let _ = self.drain(64); // best-effort: consume any trailing status packet
        Ok(())
    }
}

/// GB Operator — GB/GBC/GBA cartridges over the legacy protocol.
pub struct LegacyDevice {
    io: Serial,
}

impl LegacyDevice {
    /// Open a GB Operator directly (used by hardware integration tests).
    #[allow(dead_code)]
    pub fn open() -> Result<Self, DeviceError> {
        let (port_name, _) = find_operator()?;
        Ok(Self { io: Serial::open(&port_name)? })
    }
}

impl CartridgeDevice for LegacyDevice {
    fn read_cartridge_info(&mut self) -> Result<[u8; 64], DeviceError> {
        self.io.read_signature()
    }

    fn read_header(&mut self) -> Result<Vec<u8>, DeviceError> {
        // GB header is in bank 0; one 16 KB bank covers it. Drain the trailing
        // DONE frame so the port is left clean.
        let data = self.io.read_game(ChipType::Unknown, GB_HEADER_READ, 0, &|_| {})?;
        let _ = self.io.drain(512);
        Ok(data)
    }

    fn read_rom(&mut self, chip: ChipType, rom_size: u32, save_size: u32, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        self.io.read_game(chip, rom_size, save_size, progress)
    }

    fn read_save(&mut self, chip: ChipType, rom_size: u32, save_size: u32, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        self.io.read_save_data(chip, rom_size, save_size, progress)
    }

    fn write_save(&mut self, chip: ChipType, rom_size: u32, data: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        self.io.write_save_data(chip, rom_size, data, progress)
    }

    fn write_rom(&mut self, data: &[u8], save_size: u32, progress: &dyn Fn(u32), erase_progress: &dyn Fn(&str)) -> Result<(), DeviceError> {
        self.io.write_game(data, save_size, progress, erase_progress)
    }

    fn detect_flashcart(&mut self) -> Result<[u8; 64], DeviceError> {
        self.io.detect_flashcart_result()
    }

    fn read_rtc(&mut self, rom_size: u32, save_size: u32) -> Result<Vec<u8>, DeviceError> {
        self.io.read_rtc_data(rom_size, save_size)
    }

    fn write_rtc(&mut self, rom_size: u32, save_size: u32, data: &[u8]) -> Result<(), DeviceError> {
        self.io.write_rtc_data(rom_size, save_size, data)
    }
}

/// SN Operator — SNES/Super Famicom cartridges over the streaming protocol.
///
/// Signature read and ROM dump are verified against a real cartridge (Desert
/// Strike): the signature framing is identical to legacy, and ROM reads pure-stream
/// the whole cart with no ACK handshake (see `Serial::stream_game`). Save read/write
/// still delegate to the legacy helpers and remain unverified for SNES (SRAM sizing
/// and flow control — see notes/PROTOCOL.md "Open Questions").
pub struct StreamingDevice {
    io: Serial,
}

impl CartridgeDevice for StreamingDevice {
    fn read_cartridge_info(&mut self) -> Result<[u8; 64], DeviceError> {
        self.io.read_signature()
    }

    fn read_header(&mut self) -> Result<Vec<u8>, DeviceError> {
        // SNES header is near the end of the first/second bank; 64 KB covers
        // LoROM (0x7FC0) and HiROM (0xFFC0). stream_game drains its own trailing frame.
        self.io.stream_game(SNES_HEADER_READ, &|_| {})
    }

    fn read_rom(&mut self, _chip: ChipType, rom_size: u32, _save_size: u32, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        // SNES dumps use the streaming flow (no per-chunk ACKs); see Serial::stream_game.
        self.io.stream_game(rom_size, progress)
    }

    fn read_save(&mut self, chip: ChipType, rom_size: u32, save_size: u32, progress: &dyn Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        // SNES saves pure-stream like ROM reads (no per-chunk ACKs); see stream_save.
        self.io.stream_save(chip, rom_size, save_size, progress)
    }

    fn write_save(&mut self, chip: ChipType, rom_size: u32, data: &[u8], progress: &dyn Fn(u32)) -> Result<(), DeviceError> {
        // SNES saves pure-stream like ROM reads (no per-chunk ACKs); see stream_save_write.
        self.io.stream_save_write(chip, rom_size, data, progress)
    }

    fn write_rom(&mut self, data: &[u8], save_size: u32, progress: &dyn Fn(u32), erase_progress: &dyn Fn(&str)) -> Result<(), DeviceError> {
        self.io.write_game(data, save_size, progress, erase_progress)
    }
}
