//! ROM patching: IPS, UPS, and BPS.
//!
//! [`Patch::load`] sniffs the magic bytes and returns the right variant; all
//! three apply through the same interface. UPS and BPS carry CRC32s of the
//! source and target ROMs, so they can verify the correct base ROM was supplied
//! and that the result is correct — stronger than the header-checksum heuristic
//! in [`validate_patched_rom`], which is the only check available for IPS.

mod bps;
mod ips;
mod ups;

pub use bps::BpsPatch;
pub use ips::{IpsError, IpsPatch};
pub use ups::UpsPatch;

use crc::{Crc, CRC_32_ISO_HDLC};
use thiserror::Error;

/// CRC-32/ISO-HDLC — the standard zlib/PNG CRC32 used by UPS and BPS. (Distinct
/// from the CRC-32/MPEG-2 the device protocol uses.)
const CRC32_ZLIB: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

pub(crate) fn crc32(data: &[u8]) -> u32 {
    CRC32_ZLIB.checksum(data)
}

/// Read a UPS/BPS variable-length integer at `*pos`, advancing it. Returns
/// `None` if the buffer ends mid-number or the value overflows `u64`.
pub(crate) fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut data: u64 = 0;
    let mut shift: u64 = 1;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        data = data.checked_add((b as u64 & 0x7f).checked_mul(shift)?)?;
        if b & 0x80 != 0 {
            return Some(data);
        }
        shift = shift.checked_shl(7)?;
        data = data.checked_add(shift)?;
    }
}

/// Decode a BPS signed offset: magnitude in the high bits, sign in bit 0.
pub(crate) fn signed_offset(v: u64) -> i64 {
    let mag = (v >> 1) as i64;
    if v & 1 != 0 { -mag } else { mag }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PatchError {
    #[error("unrecognised patch format (expected IPS \"PATCH\", UPS \"UPS1\", or BPS \"BPS1\")")]
    UnknownFormat,
    #[error(transparent)]
    Ips(#[from] IpsError),
    #[error("truncated {0} patch")]
    Truncated(&'static str),
    #[error("malformed {format} patch: {detail}")]
    Malformed { format: &'static str, detail: String },
    #[error("{format} patch is for a different ROM: expects a {expected}-byte source, got {actual} bytes")]
    SourceSize { format: &'static str, expected: usize, actual: usize },
    #[error("source ROM checksum mismatch: {format} patch expects CRC32 0x{expected:08X}, ROM is 0x{actual:08X} (wrong base ROM?)")]
    SourceCrc { format: &'static str, expected: u32, actual: u32 },
    #[error("patched ROM checksum mismatch: {format} patch expects CRC32 0x{expected:08X}, result is 0x{actual:08X}")]
    TargetCrc { format: &'static str, expected: u32, actual: u32 },
    #[error("{0} patch file is corrupt (internal CRC32 mismatch)")]
    PatchCrc(&'static str),
}

/// A loaded patch in any supported format.
pub enum Patch {
    Ips(IpsPatch),
    Ups(UpsPatch),
    Bps(BpsPatch),
}

impl Patch {
    /// Parse a patch, detecting the format from its magic bytes.
    pub fn load(bytes: &[u8]) -> Result<Patch, PatchError> {
        match bytes.get(0..4) {
            Some(b"UPS1") => Ok(Patch::Ups(UpsPatch::load(bytes)?)),
            Some(b"BPS1") => Ok(Patch::Bps(BpsPatch::load(bytes)?)),
            _ if bytes.get(0..5) == Some(b"PATCH") => Ok(Patch::Ips(IpsPatch::load(bytes)?)),
            _ => Err(PatchError::UnknownFormat),
        }
    }

    /// Human-readable format name, for messages.
    pub fn format_name(&self) -> &'static str {
        match self {
            Patch::Ips(_) => "IPS",
            Patch::Ups(_) => "UPS",
            Patch::Bps(_) => "BPS",
        }
    }

    /// Whether the format embeds source/target CRC32s. When true, [`apply_into`]
    /// (with verification on) fully checks correctness, so the caller need not
    /// fall back to [`validate_patched_rom`].
    ///
    /// [`apply_into`]: Self::apply_into
    pub fn has_checksums(&self) -> bool {
        matches!(self, Patch::Ups(_) | Patch::Bps(_))
    }

    /// Apply the patch, consuming an owned source buffer (avoids a copy for IPS,
    /// which patches in place). When `verify` is true, CRC-bearing formats check
    /// the source and target checksums and treat a mismatch as a hard error.
    pub fn apply_into(&self, source: Vec<u8>, verify: bool) -> Result<Vec<u8>, PatchError> {
        match self {
            Patch::Ips(p) => Ok(p.apply_into(source)),
            Patch::Ups(p) => p.apply(&source, verify),
            Patch::Bps(p) => p.apply(&source, verify),
        }
    }

    /// Borrowing convenience wrapper around [`apply_into`](Self::apply_into).
    pub fn apply(&self, source: &[u8], verify: bool) -> Result<Vec<u8>, PatchError> {
        match self {
            Patch::Ips(p) => Ok(p.apply(source)),
            Patch::Ups(p) => p.apply(source, verify),
            Patch::Bps(p) => p.apply(source, verify),
        }
    }
}

/// The outcome of validating a patched ROM's header checksum.
#[derive(Debug)]
pub enum Validation {
    /// A recognised format's header checksum matched.
    Ok,
    /// A recognised format's header checksum did not match — likely the wrong
    /// base ROM or a corrupt result. The string describes the mismatch.
    Mismatch(String),
    /// The ROM format was not recognised, so there is nothing to validate
    /// (e.g. SNES, which has no simple header checksum, or homebrew). The
    /// string explains why validation was skipped.
    Skipped(String),
}

/// The first bytes of the Nintendo boot logo stored at 0x104 in every GB/GBC
/// ROM. An exact match reliably identifies a Game Boy ROM.
const GB_LOGO_PREFIX: [u8; 8] = [0xCE, 0xED, 0x66, 0x66, 0xCC, 0x0D, 0x00, 0x0B];

fn is_gb_rom(rom: &[u8]) -> bool {
    rom.get(0x104..0x10C) == Some(&GB_LOGO_PREFIX[..])
}

/// GBA headers carry a fixed 0x96 byte at offset 0xB2.
fn is_gba_rom(rom: &[u8]) -> bool {
    rom.get(0xB2) == Some(&0x96)
}

/// Validate the patched ROM's header checksum if we recognise the format.
///
/// The format is detected by header *content*, not ROM length: a GBA ROM is
/// far larger than the GB header offsets, so dispatching on length alone would
/// always treat a GBA ROM as a GB ROM and never reach the GBA path. We check
/// the GB boot logo first (an exact 8-byte match), then the GBA fixed-value
/// byte.
///
/// Supports Game Boy / Game Boy Color (header checksum at 0x14D) and Game Boy
/// Advance (header checksum at 0xBD), reusing the canonical checksum routines
/// in [`crate::cartridge`]. SNES has no simple header checksum, so an
/// unrecognised ROM yields [`Validation::Skipped`] rather than an error.
///
/// This is the only correctness check available for IPS patches; UPS and BPS
/// carry their own CRC32s and verify during [`Patch::apply_into`].
pub fn validate_patched_rom(rom: &[u8]) -> Validation {
    if is_gb_rom(rom) {
        match (crate::cartridge::gb_header_checksum(rom), rom.get(0x14D)) {
            (Some(actual), Some(&expected)) if actual == expected => Validation::Ok,
            (Some(actual), Some(&expected)) => Validation::Mismatch(format!(
                "GB/GBC header checksum mismatch: expected 0x{expected:02X}, got 0x{actual:02X}"
            )),
            _ => Validation::Skipped("ROM too short for a GB header".to_string()),
        }
    } else if is_gba_rom(rom) {
        match (crate::cartridge::gba_header_checksum(rom), rom.get(0xBD)) {
            (Some(actual), Some(&expected)) if actual == expected => Validation::Ok,
            (Some(actual), Some(&expected)) => Validation::Mismatch(format!(
                "GBA header checksum mismatch: expected 0x{expected:02X}, got 0x{actual:02X}"
            )),
            _ => Validation::Skipped("ROM too short for a GBA header".to_string()),
        }
    } else {
        Validation::Skipped(format!("unrecognised ROM format ({} bytes)", rom.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_by_magic() {
        // "PATCH" + "EOF" is a minimal valid IPS patch.
        assert!(matches!(Patch::load(b"PATCHEOF"), Ok(Patch::Ips(_))));
        assert!(matches!(Patch::load(b"XYZ123"), Err(PatchError::UnknownFormat)));
    }

    fn gb_rom(len: usize) -> Vec<u8> {
        let mut rom = vec![0u8; len];
        rom[0x104..0x10C].copy_from_slice(&GB_LOGO_PREFIX);
        rom
    }

    #[test]
    fn validate_gb_good_header() {
        let mut rom = gb_rom(0x150);
        // For 25 zero bytes at 0x134..=0x14C the checksum is -25 (0xE7).
        rom[0x14D] = 0xE7;
        assert!(matches!(validate_patched_rom(&rom), Validation::Ok));
    }

    #[test]
    fn validate_gb_bad_header() {
        let mut rom = gb_rom(0x150);
        rom[0x14D] = 0xFF;
        assert!(matches!(validate_patched_rom(&rom), Validation::Mismatch(_)));
    }

    #[test]
    fn validate_gba_header_reachable() {
        // A GBA-sized ROM must validate via the GBA path, not be shadowed by the
        // GB branch (regression test for length-based dispatch).
        let mut rom = vec![0u8; 1024 * 1024];
        rom[0xB2] = 0x96; // GBA fixed byte
        rom[0xBD] = crate::cartridge::gba_header_checksum(&rom).unwrap();
        assert!(matches!(validate_patched_rom(&rom), Validation::Ok));

        rom[0xBD] = rom[0xBD].wrapping_add(1);
        assert!(matches!(validate_patched_rom(&rom), Validation::Mismatch(_)));
    }

    #[test]
    fn validate_unknown_format_skipped() {
        // No GB logo, no GBA fixed byte → nothing to validate (e.g. SNES).
        let rom = vec![0u8; 0x200];
        assert!(matches!(validate_patched_rom(&rom), Validation::Skipped(_)));
    }
}
