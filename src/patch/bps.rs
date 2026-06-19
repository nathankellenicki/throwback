use super::{crc32, read_varint, signed_offset, PatchError};

const FMT: &str = "BPS";

/// A BPS (beat patch system) patch.
///
/// BPS is a delta format: the target is reconstructed from four action types
/// that copy runs from the source ROM, from literal patch data, or from
/// already-written target bytes. Like UPS it carries source/target/patch
/// CRC32s for verification.
pub struct BpsPatch {
    patch: Vec<u8>,
    source_size: usize,
    target_size: usize,
    /// Offset where the action stream begins (after sizes + metadata).
    body_start: usize,
    source_crc: u32,
    target_crc: u32,
    patch_crc: u32,
}

impl BpsPatch {
    pub fn load(bytes: &[u8]) -> Result<Self, PatchError> {
        if bytes.get(0..4) != Some(b"BPS1") {
            return Err(malformed("missing BPS1 magic"));
        }
        if bytes.len() < 4 + 12 {
            return Err(PatchError::Truncated(FMT));
        }

        let mut pos = 4;
        let source_size = read_varint(bytes, &mut pos).ok_or(PatchError::Truncated(FMT))? as usize;
        let target_size = read_varint(bytes, &mut pos).ok_or(PatchError::Truncated(FMT))? as usize;
        let metadata_size =
            read_varint(bytes, &mut pos).ok_or(PatchError::Truncated(FMT))? as usize;
        // Skip the (optional) XML metadata block.
        pos = pos
            .checked_add(metadata_size)
            .ok_or(PatchError::Truncated(FMT))?;

        let trailer = bytes.len() - 12;
        if pos > trailer {
            return Err(PatchError::Truncated(FMT));
        }
        let body_start = pos;

        let source_crc = u32::from_le_bytes(bytes[trailer..trailer + 4].try_into().unwrap());
        let target_crc = u32::from_le_bytes(bytes[trailer + 4..trailer + 8].try_into().unwrap());
        let patch_crc = u32::from_le_bytes(bytes[trailer + 8..trailer + 12].try_into().unwrap());

        Ok(BpsPatch {
            patch: bytes.to_vec(),
            source_size,
            target_size,
            body_start,
            source_crc,
            target_crc,
            patch_crc,
        })
    }

    /// Apply the patch to `source`. When `verify` is true, the patch-file CRC,
    /// source size, source CRC, and target CRC are all checked; a source
    /// mismatch is a hard error (wrong base ROM).
    pub fn apply(&self, source: &[u8], verify: bool) -> Result<Vec<u8>, PatchError> {
        if verify {
            if crc32(&self.patch[..self.patch.len() - 4]) != self.patch_crc {
                return Err(PatchError::PatchCrc(FMT));
            }
            if source.len() != self.source_size {
                return Err(PatchError::SourceSize {
                    format: FMT,
                    expected: self.source_size,
                    actual: source.len(),
                });
            }
            let actual = crc32(source);
            if actual != self.source_crc {
                return Err(PatchError::SourceCrc {
                    format: FMT,
                    expected: self.source_crc,
                    actual,
                });
            }
        }

        let trailer = self.patch.len() - 12;
        let mut out = vec![0u8; self.target_size];
        let mut outpos: usize = 0;
        let mut src_rel: usize = 0;
        let mut tgt_rel: usize = 0;
        let mut pos = self.body_start;

        while pos < trailer {
            let data = read_varint(&self.patch, &mut pos).ok_or(PatchError::Truncated(FMT))?;
            let command = data & 3;
            let length = (data >> 2) as usize + 1;

            match command {
                // SourceRead: copy `length` bytes from the source at the current
                // output offset.
                0 => {
                    for i in 0..length {
                        let o = outpos + i;
                        let b = *source.get(o).ok_or_else(|| malformed("SourceRead past source"))?;
                        *out.get_mut(o).ok_or_else(|| malformed("write past target"))? = b;
                    }
                }
                // TargetRead: copy `length` literal bytes from the patch stream.
                1 => {
                    for i in 0..length {
                        let b = *self.patch.get(pos).ok_or(PatchError::Truncated(FMT))?;
                        pos += 1;
                        *out.get_mut(outpos + i).ok_or_else(|| malformed("write past target"))? = b;
                    }
                }
                // SourceCopy: seek the source pointer by a signed delta, then
                // copy `length` bytes from there.
                2 => {
                    let raw = read_varint(&self.patch, &mut pos).ok_or(PatchError::Truncated(FMT))?;
                    src_rel = apply_signed(src_rel, signed_offset(raw))
                        .ok_or_else(|| malformed("SourceCopy offset out of range"))?;
                    for i in 0..length {
                        let b = *source
                            .get(src_rel)
                            .ok_or_else(|| malformed("SourceCopy past source"))?;
                        *out.get_mut(outpos + i).ok_or_else(|| malformed("write past target"))? = b;
                        src_rel += 1;
                    }
                }
                // TargetCopy: seek the target pointer by a signed delta, then
                // copy `length` bytes from already-written output. The copy is
                // byte-by-byte because the source range may overlap the bytes
                // being written (RLE-style runs).
                3 => {
                    let raw = read_varint(&self.patch, &mut pos).ok_or(PatchError::Truncated(FMT))?;
                    tgt_rel = apply_signed(tgt_rel, signed_offset(raw))
                        .ok_or_else(|| malformed("TargetCopy offset out of range"))?;
                    for i in 0..length {
                        let b = *out.get(tgt_rel).ok_or_else(|| malformed("TargetCopy out of range"))?;
                        *out.get_mut(outpos + i).ok_or_else(|| malformed("write past target"))? = b;
                        tgt_rel += 1;
                    }
                }
                _ => unreachable!("command is masked to two bits"),
            }

            outpos = outpos
                .checked_add(length)
                .ok_or_else(|| malformed("output offset overflow"))?;
        }

        if outpos != self.target_size {
            return Err(malformed("actions did not produce the declared target size"));
        }

        if verify {
            let actual = crc32(&out);
            if actual != self.target_crc {
                return Err(PatchError::TargetCrc {
                    format: FMT,
                    expected: self.target_crc,
                    actual,
                });
            }
        }

        Ok(out)
    }
}

/// Apply a signed delta to an unsigned cursor, returning `None` on under/overflow.
fn apply_signed(base: usize, off: i64) -> Option<usize> {
    if off >= 0 {
        base.checked_add(off as usize)
    } else {
        base.checked_sub((-off) as usize)
    }
}

fn malformed(detail: &str) -> PatchError {
    PatchError::Malformed { format: FMT, detail: detail.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::crc32;

    /// Build a valid BPS patch: magic + `body` (sizes, metadata length, actions)
    /// + the source/target/patch CRC trailer.
    fn build_bps(src: &[u8], tgt: &[u8], body: &[u8]) -> Vec<u8> {
        let mut p = b"BPS1".to_vec();
        p.extend_from_slice(body);
        p.extend_from_slice(&crc32(src).to_le_bytes());
        p.extend_from_slice(&crc32(tgt).to_le_bytes());
        let patch_crc = crc32(&p);
        p.extend_from_slice(&patch_crc.to_le_bytes());
        p
    }

    #[test]
    fn source_read_and_target_read() {
        let src = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let tgt = [0xAAu8, 0xBB, 0x11, 0x22];
        // src_size=4 (0x84), tgt_size=4 (0x84), meta=0 (0x80),
        // SourceRead len2 (0x84), TargetRead len2 (0x85) + literals 0x11 0x22.
        let body = [0x84, 0x84, 0x80, 0x84, 0x85, 0x11, 0x22];
        let p = BpsPatch::load(&build_bps(&src, &tgt, &body)).unwrap();
        assert_eq!(p.apply(&src, true).unwrap(), tgt);
    }

    #[test]
    fn target_copy_overlap_is_rle() {
        // A 1-byte source expanded to four identical bytes via an overlapping
        // TargetCopy — the classic RLE case that must copy byte-by-byte.
        let src = [0x01u8];
        let tgt = [0x01u8, 0x01, 0x01, 0x01];
        // src_size=1 (0x81), tgt_size=4 (0x84), meta=0 (0x80),
        // SourceRead len1 (0x80), TargetCopy len3 (0x8B) with offset 0 (0x80).
        let body = [0x81, 0x84, 0x80, 0x80, 0x8B, 0x80];
        let p = BpsPatch::load(&build_bps(&src, &tgt, &body)).unwrap();
        assert_eq!(p.apply(&src, true).unwrap(), tgt);
    }

    #[test]
    fn wrong_source_is_hard_error_but_ignorable() {
        let src = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let tgt = [0xAAu8, 0xBB, 0x11, 0x22];
        let body = [0x84, 0x84, 0x80, 0x84, 0x85, 0x11, 0x22];
        let p = BpsPatch::load(&build_bps(&src, &tgt, &body)).unwrap();

        let wrong = [0x00u8, 0xBB, 0xCC, 0xDD];
        assert!(matches!(
            p.apply(&wrong, true),
            Err(PatchError::SourceCrc { .. })
        ));
        assert!(p.apply(&wrong, false).is_ok());
    }

    #[test]
    fn truncated_actions_error_not_panic() {
        // Declares a 4-byte target but supplies no actions to fill it.
        let src = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let tgt = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let body = [0x84, 0x84, 0x80]; // sizes + meta, no actions
        let p = BpsPatch::load(&build_bps(&src, &tgt, &body)).unwrap();
        assert!(matches!(p.apply(&src, true), Err(PatchError::Malformed { .. })));
    }
}
