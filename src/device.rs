use crc::{Crc, CRC_32_MPEG_2};
use std::io::{Read, Write};
use std::time::Duration;
use thiserror::Error;

const PACKET_SIZE: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(2);

const CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_MPEG_2);

// Command bytes
const CMD_READ_GAME: u8 = 0x00;
#[allow(dead_code)]
const CMD_WRITE_GAME: u8 = 0x01;
const CMD_READ_SAVE: u8 = 0x02;
const CMD_WRITE_SAVE: u8 = 0x03;
const CMD_READ_SIGNATURE: u8 = 0x04;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChipType {
    Unknown = 0,
    Eeprom = 1,
    Sram = 2,
    Flash = 3,
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("GB Operator not found — no serial port detected")]
    NotFound,
    #[error("Serial port error: {0}")]
    Serial(#[from] serialport::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("No response from device")]
    NoResponse,
}

pub struct Device {
    port: Box<dyn serialport::SerialPort>,
}

/// Check if a chunk looks like GBA open bus (sequential u16 from 0).
fn is_open_bus(data: &[u8]) -> bool {
    if data.len() < 4 { return false; }
    for j in (0..data.len() - 1).step_by(2) {
        let expected = (j / 2) as u16;
        let actual = u16::from_le_bytes([data[j], data[j + 1]]);
        if actual != expected { return false; }
    }
    true
}

impl Device {
    pub fn open() -> Result<Self, DeviceError> {
        let ports = serialport::available_ports()?;
        let port_name = ports
            .iter()
            .find_map(|p| match &p.port_type {
                serialport::SerialPortType::UsbPort(usb) => {
                    if usb.vid == 0x16D0 || usb.vid == 0x1D50 {
                        Some(p.port_name.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .ok_or(DeviceError::NotFound)?;

        let port = serialport::new(&port_name, 115200)
            .timeout(TIMEOUT)
            .open()?;

        Ok(Self { port })
    }

    fn build_command(command: u8, chip: ChipType, rom_size: u32, save_size: u32) -> [u8; PACKET_SIZE] {
        let mut packet = [0u8; PACKET_SIZE];
        packet[0] = command;
        packet[1] = chip as u8;
        packet[2..6].copy_from_slice(&rom_size.to_le_bytes());
        packet[6..10].copy_from_slice(&save_size.to_le_bytes());

        let checksum = CRC.checksum(&packet[..60]);
        packet[60..64].copy_from_slice(&checksum.to_le_bytes());
        packet
    }

    fn send(&mut self, packet: &[u8; PACKET_SIZE]) -> Result<(), DeviceError> {
        self.port.write_all(packet)?;
        Ok(())
    }

    fn send_ack(&mut self) -> Result<(), DeviceError> {
        let zeros = [0u8; PACKET_SIZE];
        self.send(&zeros)
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

    /// Read exactly `len` bytes from the stream, skipping C0DE framing packets
    /// and protocol padding (zero/AA-filled 64-byte blocks).
    fn read_data_stream(&mut self, len: usize, progress: impl Fn(u32)) -> Result<Vec<u8>, DeviceError> {
        let mut data = Vec::with_capacity(len);
        let mut pending = Vec::new();

        while data.len() < len {
            // Read more from port
            let mut tmp = [0u8; 4096];
            match self.port.read(&mut tmp) {
                Ok(n) if n > 0 => pending.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if data.is_empty() {
                        continue; // Keep waiting if we haven't gotten any data yet
                    }
                    return Err(DeviceError::NoResponse);
                }
                Err(e) => return Err(e.into()),
                _ => continue,
            }

            // Process complete 64-byte packets from pending buffer
            while pending.len() >= PACKET_SIZE && data.len() < len {
                let chunk = &pending[..PACKET_SIZE];

                // Skip C0DE framing
                if chunk[0] == 0xC0 && chunk[1] == 0xDE {
                    pending.drain(..PACKET_SIZE);
                    continue;
                }

                let remaining = len - data.len();
                let copy_len = PACKET_SIZE.min(remaining);
                data.extend_from_slice(&chunk[..copy_len]);
                pending.drain(..PACKET_SIZE);

                progress(data.len() as u32);
            }
        }

        Ok(data)
    }

    pub fn read_cartridge_info(&mut self) -> Result<[u8; 64], DeviceError> {
        let packet = Self::build_command(CMD_READ_SIGNATURE, ChipType::Unknown, 0, 0);
        self.send(&packet)?;

        let buf = self.read_until_timeout();

        // Scan on 64-byte boundaries for the signature data
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

    pub fn read_rom(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: impl Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = Self::build_command(CMD_READ_GAME, chip, rom_size, save_size);
        self.send(&packet)?;

        // Drain C0DE ready + padding (512 bytes) that arrives before ROM data
        let mut drain = [0u8; 512];
        self.port.read_exact(&mut drain)?;

        // Loop (romSize / 256) times:
        //   if i % 64 == 0: write 64-byte zero ACK, THEN read 256 bytes
        //   else: read 256 bytes
        // At each power-of-two boundary, check for open bus / end of ROM.
        let total_packets = rom_size as usize / 256;
        let mut rom = Vec::with_capacity(rom_size as usize);

        for i in 0..total_packets {
            if i % 64 == 0 {
                self.send_ack()?;
            }
            let mut buf = [0u8; 256];
            self.port.read_exact(&mut buf)?;

            // At power-of-two boundaries (>=1MB), check for open bus = end of ROM
            let len = rom.len();
            if len >= 1024 * 1024 && len.is_power_of_two() && is_open_bus(&buf) {
                rom.truncate(len);
                return Ok(rom);
            }

            rom.extend_from_slice(&buf);
            progress(rom.len() as u32);
        }

        Ok(rom)
    }

    pub fn read_save(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: impl Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = Self::build_command(CMD_READ_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;

        // Drain C0DE ready + padding (512 bytes)
        let mut drain = [0u8; 512];
        self.port.read_exact(&mut drain)?;

        // Read save data in 256-byte chunks, matching Playback
        let total_packets = save_size as usize / 256;
        let mut save = Vec::with_capacity(save_size as usize);

        for _ in 0..total_packets {
            let mut buf = [0u8; 256];
            self.port.read_exact(&mut buf)?;
            save.extend_from_slice(&buf);
            progress(save.len() as u32);
        }

        Ok(save)
    }

    pub fn write_save(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        data: &[u8],
        progress: impl Fn(u32),
    ) -> Result<(), DeviceError> {
        let save_size = data.len() as u32;
        let packet = Self::build_command(CMD_WRITE_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;

        // Drain C0DE ready + 7 padding packets = 512 bytes
        let mut drain = [0u8; 512];
        self.port.read_exact(&mut drain)?;

        // Write in 64-byte chunks with per-chunk ACK
        for (i, chunk) in data.chunks(PACKET_SIZE).enumerate() {
            let mut buf = [0u8; PACKET_SIZE];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.port.write_all(&buf)?;

            // Read ACK
            let mut ack = [0u8; 64];
            let _ = self.port.read(&mut ack);

            progress(((i + 1) * PACKET_SIZE).min(data.len()) as u32);
            std::thread::sleep(Duration::from_micros(100));
        }

        Ok(())
    }
}
