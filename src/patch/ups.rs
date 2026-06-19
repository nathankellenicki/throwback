use super::{crc32, read_varint, PatchError};

const FMT: &str = "UPS";

/// A UPS (Universal Patching System) patch.
///
/// UPS encodes the difference between a source and target ROM as a series of
/// XOR hunks, and carries CRC32s of the source, the target, and the patch file
/// itself — so we can verify the user supplied the right base ROM and that the
/// result is correct.
pub struct UpsPatch {
    patch: Vec<u8>,
    input_size: usize,
    output_size: usize,
    /// Offset where the hunks begin (after the two size fields).
    body_start: usize,
    input_crc: u32,
    output_crc: u32,
    patch_crc: u32,
}

impl UpsPatch {
    pub fn load(bytes: &[u8]) -> Result<Self, PatchError> {
        if bytes.get(0..4) != Some(b"UPS1") {
            return Err(malformed("missing UPS1 magic"));
        }
        // Magic (4) + at least the 12-byte CRC trailer.
        if bytes.len() < 4 + 12 {
            return Err(PatchError::Truncated(FMT));
        }

        let mut pos = 4;
        let input_size = read_varint(bytes, &mut pos).ok_or(PatchError::Truncated(FMT))? as usize;
        let output_size = read_varint(bytes, &mut pos).ok_or(PatchError::Truncated(FMT))? as usize;
        let body_start = pos;

        let trailer = bytes.len() - 12;
        if body_start > trailer {
            return Err(PatchError::Truncated(FMT));
        }

        let input_crc = u32::from_le_bytes(bytes[trailer..trailer + 4].try_into().unwrap());
        let output_crc = u32::from_le_bytes(bytes[trailer + 4..trailer + 8].try_into().unwrap());
        let patch_crc = u32::from_le_bytes(bytes[trailer + 8..trailer + 12].try_into().unwrap());

        Ok(UpsPatch {
            patch: bytes.to_vec(),
            input_size,
            output_size,
            body_start,
            input_crc,
            output_crc,
            patch_crc,
        })
    }

    /// Apply the patch to `source`. When `verify` is true, the patch-file CRC,
    /// source size, source CRC, and target CRC are all checked; a source
    /// mismatch is a hard error (wrong base ROM).
    pub fn apply(&self, source: &[u8], verify: bool) -> Result<Vec<u8>, PatchError> {
        if verify {
            // Patch-file integrity: CRC32 over everything but the trailing CRC.
            if crc32(&self.patch[..self.patch.len() - 4]) != self.patch_crc {
                return Err(PatchError::PatchCrc(FMT));
            }
            if source.len() != self.input_size {
                return Err(PatchError::SourceSize {
                    format: FMT,
                    expected: self.input_size,
                    actual: source.len(),
                });
            }
            let actual = crc32(source);
            if actual != self.input_crc {
                return Err(PatchError::SourceCrc {
                    format: FMT,
                    expected: self.input_crc,
                    actual,
                });
            }
        }

        // Output is the source overlaid with XOR diffs: start from the source
        // bytes (truncated/zero-padded to the output size), then XOR each hunk.
        // Past the source's end the "source byte" is 0, which the zero padding
        // already gives us.
        let mut out = vec![0u8; self.output_size];
        let copy = source.len().min(self.output_size);
        out[..copy].copy_from_slice(&source[..copy]);

        let trailer = self.patch.len() - 12;
        let mut pos = self.body_start;
        let mut outpos: usize = 0;

        while pos < trailer {
            let rel = read_varint(&self.patch, &mut pos).ok_or(PatchError::Truncated(FMT))? as usize;
            outpos = outpos
                .checked_add(rel)
                .ok_or_else(|| malformed("hunk offset overflow"))?;

            // XOR patch bytes onto the output until a 0x00 terminator. The
            // terminator consumes one output position too (an unchanged byte).
            loop {
                let x = *self.patch.get(pos).ok_or(PatchError::Truncated(FMT))?;
                pos += 1;
                if outpos < self.output_size {
                    out[outpos] ^= x;
                } else if x != 0 {
                    // A non-zero diff past the declared output end is real data
                    // loss, not a harmless terminator overrun — reject it.
                    return Err(malformed("hunk writes past the output end"));
                }
                outpos += 1;
                if x == 0 {
                    break;
                }
            }
        }

        if verify {
            let actual = crc32(&out);
            if actual != self.output_crc {
                return Err(PatchError::TargetCrc {
                    format: FMT,
                    expected: self.output_crc,
                    actual,
                });
            }
        }

        Ok(out)
    }
}

fn malformed(detail: &str) -> PatchError {
    PatchError::Malformed { format: FMT, detail: detail.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::crc32;

    /// Build a valid UPS patch: magic + `body` (the two size varints and hunks)
    /// + the source/target/patch CRC trailer.
    fn build_ups(src: &[u8], tgt: &[u8], body: &[u8]) -> Vec<u8> {
        let mut p = b"UPS1".to_vec();
        p.extend_from_slice(body);
        p.extend_from_slice(&crc32(src).to_le_bytes());
        p.extend_from_slice(&crc32(tgt).to_le_bytes());
        let patch_crc = crc32(&p);
        p.extend_from_slice(&patch_crc.to_le_bytes());
        p
    }

    #[test]
    fn applies_simple_xor_diff() {
        let src = [0x10u8, 0x20, 0x30, 0x40];
        let tgt = [0x10u8, 0xFF, 0x30, 0x40];
        // input_size=4 (0x84), output_size=4 (0x84), hunk: rel=1 (0x81),
        // xor 0x20^0xFF=0xDF, terminator 0x00.
        let body = [0x84, 0x84, 0x81, 0xDF, 0x00];
        let p = UpsPatch::load(&build_ups(&src, &tgt, &body)).unwrap();
        assert_eq!(p.apply(&src, true).unwrap(), tgt);
    }

    #[test]
    fn wrong_source_is_hard_error_but_ignorable() {
        let src = [0x10u8, 0x20, 0x30, 0x40];
        let tgt = [0x10u8, 0xFF, 0x30, 0x40];
        let body = [0x84, 0x84, 0x81, 0xDF, 0x00];
        let p = UpsPatch::load(&build_ups(&src, &tgt, &body)).unwrap();

        let wrong = [0x99u8, 0x20, 0x30, 0x40];
        assert!(matches!(
            p.apply(&wrong, true),
            Err(PatchError::SourceCrc { .. })
        ));
        // --ignore-checksum (verify = false) applies regardless.
        assert!(p.apply(&wrong, false).is_ok());
    }

    #[test]
    fn target_larger_than_source_zero_pads() {
        // source 2 bytes, output 3 bytes; last byte is pure patch data (src=0).
        let src = [0xAAu8, 0xBB];
        let tgt = [0xAAu8, 0xBB, 0xCC];
        // input_size=2 (0x82), output_size=3 (0x83), hunk: rel=2 (0x82),
        // xor 0x00^0xCC=0xCC, terminator 0x00.
        let body = [0x82, 0x83, 0x82, 0xCC, 0x00];
        let p = UpsPatch::load(&build_ups(&src, &tgt, &body)).unwrap();
        assert_eq!(p.apply(&src, true).unwrap(), tgt);
    }
}
