use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IpsError {
    #[error("invalid IPS header: expected 'PATCH'")]
    InvalidHeader,
    #[error("truncated IPS patch")]
    Truncated,
    #[error(
        "unexpected trailing data after EOF marker ({0} byte(s)); the patch may be corrupt \
         or contain a record targeting the reserved 0x454F46 offset"
    )]
    TrailingData(usize),
}

const EOF_MARKER: u32 = 0x45_4F_46; // "EOF" as a big-endian 24-bit value

#[derive(Debug)]
enum Record {
    Data { addr: usize, data: Vec<u8> },
    Rle { addr: usize, value: u8, count: usize },
}

/// An IPS (International Patching System) patch.
#[derive(Debug)]
pub struct IpsPatch {
    records: Vec<Record>,
    truncate: Option<usize>,
}

impl IpsPatch {
    /// Parse an IPS patch from raw bytes.
    pub fn load(patch: &[u8]) -> Result<Self, IpsError> {
        if patch.get(0..5) != Some(b"PATCH") {
            return Err(IpsError::InvalidHeader);
        }

        let mut i = 5;
        let n = patch.len();
        let mut records = Vec::new();
        let mut truncate = None;
        let mut eof_reached = false;

        while i + 3 <= n {
            let addr = read_u24(patch, i) as usize;
            i += 3;

            if addr as u32 == EOF_MARKER {
                eof_reached = true;
                // A clean patch ends here, optionally followed by exactly one
                // 3-byte truncate length. Anything else is either trailing junk
                // (some tools pad the file) or a record whose target address
                // collided with "EOF" (0x454F46) followed by more records —
                // both are ambiguous, so reject rather than silently truncating
                // to a bogus length or dropping the remaining records.
                match n - i {
                    0 => {}
                    3 => truncate = Some(read_u24(patch, i) as usize),
                    extra => return Err(IpsError::TrailingData(extra)),
                }
                break;
            }

            if i + 2 > n {
                return Err(IpsError::Truncated);
            }
            let len = read_u16(patch, i) as usize;
            i += 2;

            if len == 0 {
                // RLE record: 2-byte count + 1-byte value.
                if i + 3 > n {
                    return Err(IpsError::Truncated);
                }
                let count = read_u16(patch, i) as usize;
                i += 2;
                let value = *patch.get(i).ok_or(IpsError::Truncated)?;
                i += 1;
                records.push(Record::Rle { addr, value, count });
            } else {
                let data = patch.get(i..i + len).ok_or(IpsError::Truncated)?.to_vec();
                i += len;
                records.push(Record::Data { addr, data });
            }
        }

        if !eof_reached {
            return Err(IpsError::Truncated);
        }

        Ok(IpsPatch { records, truncate })
    }

    /// Apply the patch to a ROM, returning the patched ROM.
    ///
    /// The ROM is grown with zero padding if a patch record targets an address
    /// beyond its current length, and it is truncated if the patch specifies a
    /// truncate length.
    pub fn apply(&self, rom: &[u8]) -> Vec<u8> {
        self.apply_into(rom.to_vec())
    }

    /// Like [`apply`](Self::apply) but consumes an owned ROM buffer and patches
    /// it in place, avoiding a copy of the (potentially large) input.
    pub fn apply_into(&self, mut out: Vec<u8>) -> Vec<u8> {
        for rec in &self.records {
            match rec {
                Record::Data { addr, data } => {
                    if *addr + data.len() > out.len() {
                        out.resize(*addr + data.len(), 0);
                    }
                    out[*addr..*addr + data.len()].copy_from_slice(data);
                }
                Record::Rle { addr, value, count } => {
                    if *addr + *count > out.len() {
                        out.resize(*addr + *count, 0);
                    }
                    for b in &mut out[*addr..*addr + *count] {
                        *b = *value;
                    }
                }
            }
        }

        if let Some(len) = self.truncate {
            out.truncate(len);
        }

        out
    }
}

fn read_u16(buf: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([buf[i], buf[i + 1]])
}

fn read_u24(buf: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([0, buf[i], buf[i + 1], buf[i + 2]])
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

    fn patch_bytes(records: &[(u32, &[u8])], truncate: Option<u32>) -> Vec<u8> {
        let mut out = b"PATCH".to_vec();
        for (addr, data) in records {
            out.extend_from_slice(&addr.to_be_bytes()[1..]);
            out.extend_from_slice(&(data.len() as u16).to_be_bytes());
            out.extend_from_slice(data);
        }
        out.extend_from_slice(b"EOF");
        if let Some(len) = truncate {
            out.extend_from_slice(&len.to_be_bytes()[1..]);
        }
        out
    }

    fn rle_bytes(addr: u32, count: u16, value: u8) -> Vec<u8> {
        let mut out = b"PATCH".to_vec();
        out.extend_from_slice(&addr.to_be_bytes()[1..]);
        out.extend_from_slice(&0u16.to_be_bytes()); // length 0 => RLE
        out.extend_from_slice(&count.to_be_bytes());
        out.push(value);
        out.extend_from_slice(b"EOF");
        out
    }

    #[test]
    fn apply_simple_data_record() {
        let rom = vec![1, 2, 3, 4];
        let patch = IpsPatch::load(&patch_bytes(&[(1, &[0xAA, 0xBB])], None)).unwrap();
        assert_eq!(patch.apply(&rom), vec![1, 0xAA, 0xBB, 4]);
    }

    #[test]
    fn apply_zero_extends_rom() {
        let rom = vec![1, 2];
        let patch = IpsPatch::load(&patch_bytes(&[(4, &[0xCC])], None)).unwrap();
        assert_eq!(patch.apply(&rom), vec![1, 2, 0, 0, 0xCC]);
    }

    #[test]
    fn apply_rle_record() {
        let rom = vec![1, 1, 1, 1];
        let patch = IpsPatch::load(&rle_bytes(2, 2, 0xFF)).unwrap();
        assert_eq!(patch.apply(&rom), vec![1, 1, 0xFF, 0xFF]);
    }

    #[test]
    fn apply_truncate() {
        let rom = vec![1, 2, 3, 4];
        let patch = IpsPatch::load(&patch_bytes(&[], Some(2))).unwrap();
        assert_eq!(patch.apply(&rom), vec![1, 2]);
    }

    #[test]
    fn invalid_header_rejected() {
        assert!(matches!(
            IpsPatch::load(b"NOTPATCH").unwrap_err(),
            IpsError::InvalidHeader
        ));
    }

    #[test]
    fn truncated_record_rejected() {
        // Header + an incomplete 3-byte address.
        let patch = b"PATCH\x00\x00";
        assert!(matches!(IpsPatch::load(patch).unwrap_err(), IpsError::Truncated));
    }

    #[test]
    fn trailing_junk_after_eof_rejected() {
        // EOF marker followed by 2 bytes of padding (not a valid 3-byte truncate).
        let mut patch = patch_bytes(&[], None);
        patch.extend_from_slice(&[0xDE, 0xAD]);
        assert!(matches!(
            IpsPatch::load(&patch).unwrap_err(),
            IpsError::TrailingData(2)
        ));
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
