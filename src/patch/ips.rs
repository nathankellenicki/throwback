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
}
