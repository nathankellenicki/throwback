//! MBC3 real-time clock access, driven register-by-register over the cart bus.
//!
//! The Operator returns a 40-byte payload (5 registers x u32 LE, current then
//! latched). The MBC3 only exposes latched values to the bus, so both halves
//! are filled from one latched snapshot — which is also what emulators do with
//! the format.

use crate::device::DeviceError;

use super::protocol::*;
use super::{GbxCart, Transport};

/// RTC registers, in payload order: seconds, minutes, hours, day-low, day-ctrl.
const RTC_REGS: [u8; 5] = [0x08, 0x09, 0x0A, 0x0B, 0x0C];
/// Clock pulses between latch steps (the RTC needs a few clock edges to settle
/// the latch; the CLK line is wired on v1.4-family boards).
const LATCH_CLKS: u32 = 60;
/// Halt bit in the day-control register.
const HALT_BIT: u8 = 0x40;

impl<T: Transport> GbxCart<T> {
    /// Latch the clock: 0x6000 = 0 then 1, with clock pulses between steps.
    fn latch_rtc(&mut self) -> Result<(), DeviceError> {
        self.dmg_write(0x6000, 0x00)?;
        self.cmd_ack(&clk_toggle_frame(LATCH_CLKS))?;
        self.dmg_write(0x6000, 0x01)?;
        self.cmd_ack(&clk_toggle_frame(LATCH_CLKS))?;
        Ok(())
    }

    /// Map RTC register `reg` into the 0xA000 window and read one byte.
    fn read_rtc_reg(&mut self, reg: u8) -> Result<u8, DeviceError> {
        self.dmg_write(0x4000, reg)?;
        let data = {
            self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_RAM_READ)?;
            self.set_variable(VAR_DMG_READ_CS_PULSE, 1)?;
            self.read_at(CMD_DMG_CART_READ, 0xA000, 1, 1)?
        };
        Ok(data[0])
    }

    /// Map RTC register `reg` into the 0xA000 window and write one byte
    /// (via the CS-pulsed SRAM write path — RTC registers live in RAM space).
    fn write_rtc_reg(&mut self, reg: u8, value: u8) -> Result<(), DeviceError> {
        self.dmg_write(0x4000, reg)?;
        self.set_variable(VAR_DMG_ACCESS_MODE, DMG_ACCESS_RAM_WRITE)?;
        self.set_variable(VAR_DMG_WRITE_CS_PULSE, 1)?;
        self.set_variable(VAR_ADDRESS, 0xA000)?;
        self.set_variable(VAR_TRANSFER_SIZE, 1)?;
        self.io.write_all(&[CMD_DMG_CART_WRITE_SRAM])?;
        self.io.write_all(&[value])?;
        self.expect_ack()?;
        Ok(())
    }

    /// Read the clock as the Operator's 40-byte payload.
    pub(super) fn read_mbc3_rtc(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.enter_dmg_mode()?;
        self.dmg_write(0x0000, 0x0A)?; // RAM/RTC enable
        let result = (|| {
            self.latch_rtc()?;
            let mut regs = [0u8; 5];
            for (i, &reg) in RTC_REGS.iter().enumerate() {
                regs[i] = self.read_rtc_reg(reg)?;
            }
            let mut payload = Vec::with_capacity(40);
            for half in 0..2 {
                let _ = half; // current + latched: same snapshot (see module docs)
                for &r in &regs {
                    payload.extend_from_slice(&(r as u32).to_le_bytes());
                }
            }
            Ok(payload)
        })();
        let disable = self.dmg_write(0x0000, 0x00);
        result.and_then(|p| disable.map(|()| p))
    }

    /// Set the clock from the Operator's 40-byte payload (first half is used;
    /// registers at offsets 0, 4, 8, 12, 16).
    pub(super) fn write_mbc3_rtc(&mut self, payload: &[u8]) -> Result<(), DeviceError> {
        if payload.len() < 20 {
            return Err(DeviceError::Unsupported("RTC payload too short"));
        }
        let values: Vec<u8> = (0..5).map(|i| payload[i * 4]).collect();

        self.enter_dmg_mode()?;
        self.dmg_write(0x0000, 0x0A)?;
        let result = (|| {
            // Halt the clock so registers don't tick while being set.
            self.write_rtc_reg(0x0C, values[4] | HALT_BIT)?;
            for (i, &reg) in RTC_REGS[..4].iter().enumerate() {
                self.write_rtc_reg(reg, values[i])?;
            }
            // Day-control last, with the payload's own halt bit — this releases
            // the clock unless the caller asked for it to stay halted.
            self.write_rtc_reg(0x0C, values[4])?;
            self.latch_rtc()?;
            Ok(())
        })();
        let disable = self.dmg_write(0x0000, 0x00);
        result.and(disable)
    }
}
