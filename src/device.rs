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
const CMD_DETECT_FLASHCART: u8 = 0x15;

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
    #[error("GB Operator not found")]
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

    fn drain(&mut self, n: usize) -> Result<(), DeviceError> {
        let mut buf = vec![0u8; n];
        self.port.read_exact(&mut buf)?;
        Ok(())
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

    pub fn read_cartridge_info(&mut self) -> Result<[u8; 64], DeviceError> {
        let packet = Self::build_command(CMD_READ_SIGNATURE, ChipType::Unknown, 0, 0);
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

    pub fn read_rom(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: impl Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = Self::build_command(CMD_READ_GAME, chip, rom_size, save_size);
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

    pub fn read_save(
        &mut self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: impl Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = Self::build_command(CMD_READ_SAVE, chip, rom_size, save_size);
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

        // Flush stale data
        self.read_until_timeout();

        let packet = Self::build_command(CMD_WRITE_SAVE, chip, rom_size, save_size);
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

    pub fn write_rom(
        &mut self,
        data: &[u8],
        progress: impl Fn(u32),
        erase_progress: impl Fn(&str),
    ) -> Result<(), DeviceError> {
        let rom_size = data.len() as u32;

        // Flush stale data
        self.read_until_timeout();

        // Step 1: DetectFlashcart — required before WriteGame to prevent USB reset
        erase_progress("Preparing cartridge...");
        let detect_packet = Self::build_command(CMD_DETECT_FLASHCART, ChipType::Unknown, 0, 0);
        self.send(&detect_packet)?;
        self.port.set_timeout(Duration::from_secs(30))?;
        let _ = self.read_until_timeout();

        // Step 2: WriteGame command
        erase_progress("Erasing flash...");
        let packet = Self::build_command(CMD_WRITE_GAME, ChipType::Unknown, rom_size, 0);
        self.send(&packet)?;

        // Wait for erase — read EE progress packets
        self.port.set_timeout(Duration::from_secs(10))?;
        let mut seen_erase = false;
        loop {
            let mut buf = [0u8; 64];
            match self.port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    if buf[0] == 0xEE {
                        seen_erase = true;
                        erase_progress("Erasing...");
                        continue;
                    }
                    if buf[0] == 0xC0 && buf[1] == 0xDE {
                        if buf[2] == 0x01 && seen_erase { break; }
                        continue;
                    }
                    if buf.iter().all(|&b| b == 0) {
                        if seen_erase { break; }
                        continue;
                    }
                }
                Ok(_) => {
                    if seen_erase { break; }
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if seen_erase { break; }
                    continue;
                }
                Err(_) => break,
            }
        }

        // Step 3: Write data — straight 64-byte chunks, no ACKs
        erase_progress("Writing...");
        self.port.set_timeout(TIMEOUT)?;

        for (i, chunk) in data.chunks(PACKET_SIZE).enumerate() {
            let mut buf = [0u8; PACKET_SIZE];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.port.write_all(&buf)?;
            progress(((i + 1) * PACKET_SIZE).min(data.len()) as u32);
        }

        Ok(())
    }
}
