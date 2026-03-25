use crc::{Crc, CRC_32_MPEG_2};
use rusb::{Context, DeviceHandle, UsbContext};
use std::time::Duration;
use thiserror::Error;

const VENDOR_IDS: &[(u16, &[u16])] = &[
    (0x16D0, &[0x123B, 0x123C, 0x123D]),
    (0x1D50, &[0x6018]),
];

const PACKET_SIZE: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_millis(500);

const CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_MPEG_2);

// Command bytes (DeviceProperties::Command)
const CMD_READ_GAME: u8 = 0x00;
#[allow(dead_code)]
const CMD_WRITE_GAME: u8 = 0x01;
const CMD_READ_SAVE: u8 = 0x02;
const CMD_WRITE_SAVE: u8 = 0x03;
const CMD_READ_SIGNATURE: u8 = 0x04;

// Streaming protocol magic
const MAGIC: [u8; 2] = [0xC0, 0xDE];

// Chip types (CartridgeProperties::Chip::Enum)
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
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),
    #[error("No response from device")]
    NoResponse,
}

pub struct Device {
    handle: DeviceHandle<Context>,
    ctrl_iface: u8,
    data_iface: u8,
    ep_out: u8,
    ep_in: u8,
}

// CDC ACM control requests
const CDC_SET_LINE_CODING: u8 = 0x20;
const CDC_SET_CONTROL_LINE_STATE: u8 = 0x22;
const CDC_REQUEST_TYPE: u8 = 0x21;

impl Device {
    pub fn open() -> Result<Self, DeviceError> {
        let ctx = Context::new()?;

        for device in ctx.devices()?.iter() {
            let desc = device.device_descriptor()?;
            let found = VENDOR_IDS.iter().any(|(vid, pids)| {
                desc.vendor_id() == *vid && pids.contains(&desc.product_id())
            });

            if found {
                let handle = device.open()?;

                // CDC ACM: claim control interface (0) + data interface (1)
                let (ctrl_iface, data_iface, ep_out, ep_in) = (0u8, 1u8, 0x01u8, 0x81u8);

                Self::try_claim(&handle, ctrl_iface);
                if !Self::try_claim(&handle, data_iface) {
                    return Err(DeviceError::NotFound);
                }

                // CDC ACM initialization
                let line_coding: [u8; 7] = [
                    0x00, 0xC2, 0x01, 0x00, // 115200 baud (LE)
                    0x00, // 1 stop bit
                    0x00, // no parity
                    0x08, // 8 data bits
                ];
                let _ = handle.write_control(
                    CDC_REQUEST_TYPE, CDC_SET_LINE_CODING, 0,
                    ctrl_iface as u16, &line_coding, TIMEOUT,
                );
                let _ = handle.write_control(
                    CDC_REQUEST_TYPE, CDC_SET_CONTROL_LINE_STATE, 0x03,
                    ctrl_iface as u16, &[], TIMEOUT,
                );

                return Ok(Self { handle, ctrl_iface, data_iface, ep_out, ep_in });
            }
        }

        Err(DeviceError::NotFound)
    }

    fn try_claim(handle: &DeviceHandle<Context>, iface: u8) -> bool {
        if handle.kernel_driver_active(iface).unwrap_or(false) {
            let _ = handle.detach_kernel_driver(iface);
        }
        handle.claim_interface(iface).is_ok()
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

    fn send(&self, packet: &[u8; PACKET_SIZE]) -> Result<(), DeviceError> {
        self.handle.write_bulk(self.ep_out, packet, TIMEOUT)?;
        Ok(())
    }

    fn read_packet(&self) -> Result<(usize, [u8; PACKET_SIZE]), DeviceError> {
        let mut buf = [0u8; PACKET_SIZE];
        let n = self.handle.read_bulk(self.ep_in, &mut buf, READ_TIMEOUT)?;
        Ok((n, buf))
    }

    /// Read the next non-empty, non-magic packet (skips ZLPs, zero packets, and C0DE frames).
    /// Use for ROM reads where initial zeros are protocol padding.
    fn read_data_packet(&self, max_attempts: usize) -> Result<[u8; PACKET_SIZE], DeviceError> {
        for _ in 0..max_attempts {
            let (n, buf) = match self.read_packet() {
                Ok(r) => r,
                Err(DeviceError::Usb(rusb::Error::Timeout)) => continue,
                Err(e) => return Err(e),
            };
            if n == 0 { continue; }
            if buf[0] == MAGIC[0] && buf[1] == MAGIC[1] { continue; }
            if buf.iter().all(|&b| b == 0) { continue; }
            return Ok(buf);
        }
        Err(DeviceError::NoResponse)
    }

    /// Drain the fixed protocol padding after a ready packet.
    /// The device always sends ~7 padding packets (mix of zeros, 0xAA fill, etc.)
    /// between the C0DE ready frame and actual data. We skip exactly 7 full
    /// non-ZLP, non-C0DE packets.
    fn drain_padding(&self) -> Result<(), DeviceError> {
        let mut drained = 0;
        for _ in 0..50 {
            let (n, buf) = match self.read_packet() {
                Ok(r) => r,
                Err(DeviceError::Usb(rusb::Error::Timeout)) => continue,
                Err(e) => return Err(e),
            };
            if n == 0 { continue; }
            if buf[0] == MAGIC[0] && buf[1] == MAGIC[1] { continue; }
            drained += 1;
            if drained >= 7 { return Ok(()); }
        }
        Ok(())
    }

    /// Wait for a C0DE ready packet for the given command.
    fn wait_for_ready(&self, _command: u8) -> Result<(), DeviceError> {
        for _ in 0..50 {
            let (n, buf) = match self.read_packet() {
                Ok(r) => r,
                Err(DeviceError::Usb(rusb::Error::Timeout)) => continue,
                Err(e) => return Err(e),
            };
            if n >= 4 && buf[0] == MAGIC[0] && buf[1] == MAGIC[1] && buf[2] == 0x00 {
                return Ok(());
            }
        }
        Err(DeviceError::NoResponse)
    }

    /// Drain remaining packets until C0DE done or timeout. Best-effort, never errors.
    fn drain_until_done(&self) {
        for _ in 0..10 {
            match self.read_packet() {
                Ok((n, buf)) if n >= 4 && buf[0] == MAGIC[0] && buf[1] == MAGIC[1] && buf[2] == 0x01 => return,
                Err(DeviceError::Usb(rusb::Error::Timeout)) => return,
                _ => continue,
            }
        }
    }

    fn send_ack(&self) -> Result<(), DeviceError> {
        let zeros = [0u8; PACKET_SIZE];
        self.send(&zeros)
    }

    pub fn read_cartridge_info(&self) -> Result<[u8; 64], DeviceError> {
        let packet = Self::build_command(CMD_READ_SIGNATURE, ChipType::Unknown, 0, 0);
        self.send(&packet)?;
        self.wait_for_ready(CMD_READ_SIGNATURE)?;

        let data = self.read_data_packet(50)?;

        // Drain the done packet
        self.drain_until_done();

        Ok(data)
    }

    pub fn read_rom(
        &self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: impl Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = Self::build_command(CMD_READ_GAME, chip, rom_size, save_size);
        self.send(&packet)?;
        self.wait_for_ready(CMD_READ_GAME)?;

        // Send ACK to start data transfer
        self.send_ack()?;

        // Drain the 7 protocol padding packets
        self.drain_padding()?;

        let mut rom = Vec::with_capacity(rom_size as usize);
        let mut chunk_count: u32 = 0;

        while rom.len() < rom_size as usize {
            let (n, buf) = match self.read_packet() {
                Ok(r) => r,
                Err(DeviceError::Usb(rusb::Error::Timeout)) => continue,
                Err(e) => return Err(e),
            };
            if n == 0 { continue; }
            // Skip C0DE framing packets
            if buf[0] == MAGIC[0] && buf[1] == MAGIC[1] { continue; }

            let remaining = rom_size as usize - rom.len();
            let copy_len = n.min(remaining);
            rom.extend_from_slice(&buf[..copy_len]);
            chunk_count += 1;

            progress(rom.len() as u32);

            if chunk_count % 320 == 0 && rom.len() < rom_size as usize {
                self.send_ack()?;
            }
        }

        self.drain_until_done();
        Ok(rom)
    }

    pub fn read_save(
        &self,
        chip: ChipType,
        rom_size: u32,
        save_size: u32,
        progress: impl Fn(u32),
    ) -> Result<Vec<u8>, DeviceError> {
        let packet = Self::build_command(CMD_READ_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;
        self.wait_for_ready(CMD_READ_SAVE)?;

        // Skip protocol padding (same as ROM: ~7 packets after ready before real data)
        self.drain_padding()?;

        let mut save = Vec::with_capacity(save_size as usize);

        while save.len() < save_size as usize {
            let (n, buf) = match self.read_packet() {
                Ok(r) => r,
                Err(DeviceError::Usb(rusb::Error::Timeout)) => continue,
                Err(e) => return Err(e),
            };
            if n == 0 { continue; }
            if buf[0] == MAGIC[0] && buf[1] == MAGIC[1] { continue; }

            let remaining = save_size as usize - save.len();
            let copy_len = n.min(remaining);
            save.extend_from_slice(&buf[..copy_len]);

            progress(save.len() as u32);
        }

        self.drain_until_done();
        Ok(save)
    }

    pub fn write_save(
        &self,
        chip: ChipType,
        rom_size: u32,
        data: &[u8],
        progress: impl Fn(u32),
    ) -> Result<(), DeviceError> {
        let save_size = data.len() as u32;
        let packet = Self::build_command(CMD_WRITE_SAVE, chip, rom_size, save_size);
        self.send(&packet)?;
        self.wait_for_ready(CMD_WRITE_SAVE)?;

        for (i, chunk) in data.chunks(PACKET_SIZE).enumerate() {
            let mut buf = [0u8; PACKET_SIZE];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.handle.write_bulk(self.ep_out, &buf, TIMEOUT)?;

            // Wait for per-chunk ACK (skip ZLPs and zeros)
            for _ in 0..20 {
                match self.read_packet() {
                    Ok((n, _)) if n > 0 => break,
                    Ok(_) => continue,
                    Err(DeviceError::Usb(rusb::Error::Timeout)) => continue,
                    Err(e) => return Err(e),
                }
            }

            progress(((i + 1) * PACKET_SIZE).min(data.len()) as u32);
            std::thread::sleep(Duration::from_micros(100));
        }

        self.drain_until_done();
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(self.data_iface);
        let _ = self.handle.release_interface(self.ctrl_iface);
    }
}
