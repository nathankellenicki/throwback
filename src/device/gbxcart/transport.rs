//! Serial transport for the GBxCart RW, behind a small trait so the whole backend
//! can run against a simulated device in tests.

use std::io::{Read, Write};
use std::time::Duration;

use crate::device::DeviceError;

/// Byte-level I/O the GBxCart backend needs. `SerialTransport` implements it over
/// the real CH340 serial port; the test suite implements it with a behavioral
/// device simulator.
pub trait Transport {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), DeviceError>;
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), DeviceError>;
    fn set_timeout(&mut self, t: Duration) -> Result<(), DeviceError>;
    /// Discard any pending input (used to resync after ack-uncertain commands).
    fn flush_input(&mut self);
}

/// GBxCart serial link speed. The device supports a 1.5 Mbaud upgrade opcode, but
/// 1 Mbaud is reliable everywhere (the CH340 driver on macOS in particular) and
/// is what we use unconditionally.
pub const BAUD: u32 = 1_000_000;

/// Post-write settle delay on macOS. The macOS CH340 driver drops or reorders
/// bytes when writes are issued back-to-back; a short sleep after each write
/// avoids it. Tune during hardware bring-up if throughput matters.
pub const MACOS_WRITE_DELAY: Duration = Duration::from_micros(1500);

pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialTransport {
    pub fn open(port_name: &str, timeout: Duration) -> Result<Self, DeviceError> {
        let port = serialport::new(port_name, BAUD).timeout(timeout).open()?;
        Ok(Self { port })
    }
}

impl Transport for SerialTransport {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), DeviceError> {
        self.port.write_all(buf)?;
        if cfg!(target_os = "macos") {
            self.port.flush()?;
            std::thread::sleep(MACOS_WRITE_DELAY);
        }
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.port.read_exact(buf)?;
        Ok(())
    }

    fn set_timeout(&mut self, t: Duration) -> Result<(), DeviceError> {
        self.port.set_timeout(t)?;
        Ok(())
    }

    fn flush_input(&mut self) {
        let saved = self.port.timeout();
        let _ = self.port.set_timeout(Duration::from_millis(20));
        let mut tmp = [0u8; 4096];
        while matches!(self.port.read(&mut tmp), Ok(n) if n > 0) {}
        let _ = self.port.set_timeout(saved);
    }
}
