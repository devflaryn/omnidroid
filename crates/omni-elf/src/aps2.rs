//! The Android packed-relocation (`APS2`) decoder.
//!
//! # Why this exists
//!
//! `libroblox.so` has no `DT_RELA` and no `DT_RELR`. All 568,272 of its non-PLT relocations live
//! in a 2,100,778-byte `DT_ANDROID_RELA` blob (see `docs/DECISIONS.md` D9). A loader that does
//! not decode this applies zero relocations and fails much later, in unrelated code.
//!
//! # The format
//!
//! The blob is:
//!
//! ```text
//! "APS2"                      4 bytes of magic, not part of the SLEB128 stream
//! relocation_count            SLEB128
//! initial r_offset            SLEB128
//! group*                      until relocation_count relocations have been produced
//! ```
//!
//! and each group is:
//!
//! ```text
//! group_size                  SLEB128
//! group_flags                 SLEB128
//! [group_r_offset_delta]      SLEB128, iff GROUPED_BY_OFFSET_DELTA
//! [group r_info]              SLEB128, iff GROUPED_BY_INFO
//! [group addend delta]        SLEB128, iff (HAS_ADDEND | GROUPED_BY_ADDEND)
//! group_size × {
//!     [r_offset delta]        SLEB128, unless GROUPED_BY_OFFSET_DELTA
//!     [r_info]                SLEB128, unless GROUPED_BY_INFO
//!     [addend delta]          SLEB128, iff HAS_ADDEND and not GROUPED_BY_ADDEND
//! }
//! ```
//!
//! `r_offset` and `r_addend` are running totals carried across groups; `r_info` is a plain
//! value, not a delta. When a group does **not** set `HAS_ADDEND`, the running addend is reset
//! to zero rather than carried. When `GROUPED_BY_ADDEND` is set alongside `HAS_ADDEND`, one
//! delta is read and applied **once for the whole group**, so every relocation in it shares an
//! addend — that is what "grouped by addend" means, and it is what bionic's
//! `for_all_packed_relocs` does.
//!
//! # Divergences from bionic, all deliberate hardening
//!
//! bionic trusts the blob. We do not, because a decoder that silently produces the wrong number
//! of relocations is the failure mode this whole module exists to prevent. On top of bionic's
//! behaviour we reject: a declared count that disagrees with the decoded count, unconsumed
//! trailing bytes, a non-positive group size (which in bionic's loop shape cannot terminate),
//! a group that would overrun the declared count, unknown group-flag bits, and a truncated
//! stream (bionic aborts the process instead).

use crate::error::{ElfError, Result};
use crate::reloc::Rela;

/// `APS2`, the only packed-relocation format version Android has ever shipped.
pub const APS2_MAGIC: [u8; 4] = *b"APS2";

/// All relocations in the group share one `r_info`, stored once in the group header.
pub const RELOCATION_GROUPED_BY_INFO_FLAG: u64 = 1;
/// All relocations in the group share one `r_offset` delta, stored once in the group header.
pub const RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG: u64 = 2;
/// All relocations in the group share one addend delta, stored once in the group header.
pub const RELOCATION_GROUPED_BY_ADDEND_FLAG: u64 = 4;
/// The group carries addends at all. Without this, `r_addend` is reset to zero for the group.
pub const RELOCATION_GROUP_HAS_ADDEND_FLAG: u64 = 8;

const ALL_GROUP_FLAGS: u64 = RELOCATION_GROUPED_BY_INFO_FLAG
    | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
    | RELOCATION_GROUPED_BY_ADDEND_FLAG
    | RELOCATION_GROUP_HAS_ADDEND_FLAG;

/// Which Android tag the blob came from, which decides whether addends are legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedFormat {
    /// `DT_ANDROID_RELA`: relocations carry addends.
    Rela,
    /// `DT_ANDROID_REL`: they do not, and a group claiming otherwise is an error.
    Rel,
}

// ---------------------------------------------------------------------------------------------
// SLEB128
// ---------------------------------------------------------------------------------------------

/// A signed-LEB128 reader over the packed-relocation stream.
///
/// Values are decoded into `i64` with wrapping arithmetic, matching bionic's `size_t`-based
/// decoder bit for bit: the `r_offset` deltas are genuinely signed (the encoder emits negative
/// deltas when relocation offsets are not monotonic), and sign extension is skipped once the
/// shift has reached 64 bits so that a 10-byte encoding of a large value is not corrupted.
#[derive(Debug, Clone)]
pub struct Sleb128Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Sleb128Decoder<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes consumed so far.
    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Total bytes available.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Bytes not yet consumed.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Decode the next value.
    ///
    /// Errors, rather than aborting as bionic does, when the stream runs out mid-value.
    #[inline]
    pub fn pop_front(&mut self) -> Result<i64> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = *self.data.get(self.pos).ok_or(ElfError::Aps2Truncated {
                offset: self.pos,
                total: self.data.len(),
            })?;
            self.pos += 1;
            // Bits past 64 are dropped, exactly as they are in bionic's `size_t` accumulator.
            if shift < 64 {
                value |= ((byte & 0x7f) as u64) << shift;
            }
            shift = shift.saturating_add(7);
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= (!0u64) << shift;
                }
                return Ok(value as i64);
            }
        }
    }
}

/// Encode a value as SLEB128. Test-support and round-trip checking only: nothing in the loader
/// writes packed relocations. Kept in the library so the unit tests exercise the same bit
/// layout the decoder reads rather than a second, separately-wrong encoder.
pub fn encode_sleb128(mut value: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (value as u8) & 0x7f;
        // Arithmetic shift: sign bits fill in from the left.
        value >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        let done = (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The decoder
// ---------------------------------------------------------------------------------------------

/// What the decoder measured while walking the blob.
///
/// `bytes_consumed == bytes_total` is the single strongest correctness signal available for this
/// format, which is why it is a first-class part of the result rather than a debug print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aps2Summary {
    /// The count the blob declares in its header.
    pub declared_count: u64,
    /// The number of relocations actually produced. Equal to `declared_count` on success.
    pub decoded_count: u64,
    /// Bytes read, including the 4-byte magic. Equal to `bytes_total` on success.
    pub bytes_consumed: usize,
    /// Size of the whole blob, as given by `DT_ANDROID_RELASZ`.
    pub bytes_total: usize,
    /// How many groups the stream was divided into. Diagnostics only.
    pub group_count: usize,
    /// The `r_offset` the header seeded the running total with.
    pub initial_offset: i64,
    /// The union of every group-flags value seen. Diagnostics only: it says which parts of the
    /// format a given binary actually exercises.
    pub observed_group_flags: u64,
}

/// Decoded packed relocations plus the measurements that prove the decode was exact.
#[derive(Debug, Clone)]
pub struct PackedRelocations {
    pub relocations: Vec<Rela>,
    pub summary: Aps2Summary,
}

/// Decode a `DT_ANDROID_RELA` blob into a `Vec`.
pub fn decode_rela(blob: &[u8]) -> Result<PackedRelocations> {
    decode(blob, PackedFormat::Rela)
}

/// Decode a `DT_ANDROID_REL` blob into a `Vec`.
pub fn decode_rel(blob: &[u8]) -> Result<PackedRelocations> {
    decode(blob, PackedFormat::Rel)
}

/// Decode a packed blob into a `Vec`, with the format given explicitly.
pub fn decode(blob: &[u8], format: PackedFormat) -> Result<PackedRelocations> {
    // The declared count is read before any allocation, but it is attacker-controlled data in
    // the general case, so the vector is grown as relocations arrive rather than reserved to a
    // declared size that could be 2^63.
    let mut relocations = Vec::new();
    let summary = decode_with(blob, format, |r| relocations.push(r))?;
    Ok(PackedRelocations {
        relocations,
        summary,
    })
}

/// Decode a packed blob, handing each relocation to `sink` as it is produced.
///
/// This is the allocation-free entry point; Task 5 can apply relocations straight from here
/// without materialising 568,272 × 24 bytes.
pub fn decode_with<F>(blob: &[u8], format: PackedFormat, mut sink: F) -> Result<Aps2Summary>
where
    F: FnMut(Rela),
{
    if blob.len() < APS2_MAGIC.len() {
        return Err(ElfError::Aps2TooShort(blob.len()));
    }
    let magic: [u8; 4] = [blob[0], blob[1], blob[2], blob[3]];
    if magic != APS2_MAGIC {
        return Err(ElfError::Aps2BadMagic(magic));
    }

    let mut dec = Sleb128Decoder::new(&blob[APS2_MAGIC.len()..]);

    let declared = dec.pop_front()?;
    if declared < 0 {
        return Err(ElfError::Aps2NegativeCount(declared));
    }
    let declared = declared as u64;

    let initial_offset = dec.pop_front()?;
    let mut r_offset = initial_offset as u64;
    let mut r_info: u64 = 0;
    let mut r_addend: i64 = 0;

    let mut decoded: u64 = 0;
    let mut group_count = 0usize;
    let mut observed_group_flags: u64 = 0;

    while decoded < declared {
        let group_index = group_count;
        let group_size = dec.pop_front()?;
        if group_size <= 0 {
            // bionic's loop is `idx += group_size`, so a zero or negative size either spins
            // forever or walks backwards. Refuse instead.
            return Err(ElfError::Aps2BadGroupSize {
                group_index,
                size: group_size,
            });
        }
        let group_size = group_size as u64;
        let would_be = decoded.saturating_add(group_size);
        if would_be > declared {
            return Err(ElfError::Aps2GroupOverrun {
                group_index,
                size: group_size,
                would_be,
                declared,
            });
        }

        let group_flags = dec.pop_front()? as u64;
        let unknown = group_flags & !ALL_GROUP_FLAGS;
        if unknown != 0 {
            return Err(ElfError::Aps2UnknownGroupFlags {
                group_index,
                unknown,
            });
        }
        observed_group_flags |= group_flags;
        group_count += 1;

        let grouped_by_offset_delta = group_flags & RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG != 0;
        let grouped_by_info = group_flags & RELOCATION_GROUPED_BY_INFO_FLAG != 0;

        let mut group_offset_delta: u64 = 0;
        if grouped_by_offset_delta {
            group_offset_delta = dec.pop_front()? as u64;
        }
        if grouped_by_info {
            r_info = dec.pop_front()? as u64;
        }

        // bionic switches on the two addend bits together; mirror that exactly, because the
        // three cases are genuinely different and an `if has_addend { if grouped { .. } }`
        // shape gets the GROUPED_BY_ADDEND-without-HAS_ADDEND case wrong.
        let addend_bits =
            group_flags & (RELOCATION_GROUP_HAS_ADDEND_FLAG | RELOCATION_GROUPED_BY_ADDEND_FLAG);
        let per_reloc_addend = addend_bits == RELOCATION_GROUP_HAS_ADDEND_FLAG;
        if format == PackedFormat::Rel && group_flags & RELOCATION_GROUP_HAS_ADDEND_FLAG != 0 {
            return Err(ElfError::Aps2AddendInRelFormat { group_index });
        }
        if per_reloc_addend {
            // Nothing in the group header; each relocation carries its own delta below. This is
            // what lld's encoder emits.
        } else if addend_bits
            == (RELOCATION_GROUP_HAS_ADDEND_FLAG | RELOCATION_GROUPED_BY_ADDEND_FLAG)
        {
            r_addend = r_addend.wrapping_add(dec.pop_front()?);
        } else {
            r_addend = 0;
        }

        for _ in 0..group_size {
            if grouped_by_offset_delta {
                r_offset = r_offset.wrapping_add(group_offset_delta);
            } else {
                r_offset = r_offset.wrapping_add(dec.pop_front()? as u64);
            }
            if !grouped_by_info {
                r_info = dec.pop_front()? as u64;
            }
            if per_reloc_addend {
                r_addend = r_addend.wrapping_add(dec.pop_front()?);
            }
            sink(Rela {
                r_offset,
                r_info,
                r_addend,
            });
        }
        decoded += group_size;
    }

    if decoded != declared {
        return Err(ElfError::Aps2CountMismatch { declared, decoded });
    }

    let bytes_consumed = APS2_MAGIC.len() + dec.position();
    if bytes_consumed != blob.len() {
        return Err(ElfError::Aps2TrailingBytes {
            consumed: bytes_consumed,
            total: blob.len(),
            remaining: blob.len() - bytes_consumed,
        });
    }

    Ok(Aps2Summary {
        declared_count: declared,
        decoded_count: decoded,
        bytes_consumed,
        bytes_total: blob.len(),
        group_count,
        initial_offset,
        observed_group_flags,
    })
}

// ---------------------------------------------------------------------------------------------
// Encoder, for tests only
// ---------------------------------------------------------------------------------------------

/// Build a minimal well-formed `APS2` blob from explicit relocations, using the simplest legal
/// encoding: one group, no sharing, per-relocation addends.
///
/// This exists so the negative tests (truncation, bad magic, count mismatch) can start from a
/// blob that is known good, instead of from bytes hand-typed by whoever wrote the test.
pub fn encode_rela_ungrouped(relocations: &[Rela]) -> Vec<u8> {
    let mut out = Vec::from(APS2_MAGIC);
    encode_sleb128(relocations.len() as i64, &mut out);
    encode_sleb128(0, &mut out); // initial r_offset
    if !relocations.is_empty() {
        encode_sleb128(relocations.len() as i64, &mut out); // group_size
        encode_sleb128(RELOCATION_GROUP_HAS_ADDEND_FLAG as i64, &mut out);
        let mut offset: u64 = 0;
        let mut addend: i64 = 0;
        for r in relocations {
            encode_sleb128(r.r_offset.wrapping_sub(offset) as i64, &mut out);
            offset = r.r_offset;
            encode_sleb128(r.r_info as i64, &mut out);
            encode_sleb128(r.r_addend.wrapping_sub(addend), &mut out);
            addend = r.r_addend;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(v: i64) {
        let mut buf = Vec::new();
        encode_sleb128(v, &mut buf);
        let mut dec = Sleb128Decoder::new(&buf);
        assert_eq!(dec.pop_front().unwrap(), v, "round-trip of {v} via {buf:02x?}");
        assert_eq!(dec.remaining(), 0, "encoding of {v} had trailing bytes");
    }

    #[test]
    fn sleb128_round_trips_boundaries() {
        // One byte either side of every 7-bit boundary, positive and negative, plus the
        // extremes. 63 is the largest positive one-byte value; 64 needs two because bit 6 is
        // the sign bit.
        for shift in 0..64u32 {
            let base = 1i64.checked_shl(shift).unwrap_or(0);
            for delta in [-2i64, -1, 0, 1, 2] {
                round_trip(base.wrapping_add(delta));
                round_trip(base.wrapping_neg().wrapping_add(delta));
            }
        }
        for v in [0i64, 1, -1, 63, 64, -64, -65, 127, 128, -128, -129, 8191, 8192, -8192, -8193] {
            round_trip(v);
        }
        round_trip(i64::MAX);
        round_trip(i64::MIN);
        round_trip(i64::MAX - 1);
        round_trip(i64::MIN + 1);
    }

    #[test]
    fn sleb128_known_encodings() {
        // Hand-checked against the DWARF spec's worked examples.
        let cases: &[(i64, &[u8])] = &[
            (2, &[0x02]),
            (-2, &[0x7e]),
            (127, &[0xff, 0x00]),
            (-127, &[0x81, 0x7f]),
            (128, &[0x80, 0x01]),
            (-128, &[0x80, 0x7f]),
            (129, &[0x81, 0x01]),
            (-129, &[0xff, 0x7e]),
            (0, &[0x00]),
            (63, &[0x3f]),
            (64, &[0xc0, 0x00]),
            (-64, &[0x40]),
            (-65, &[0xbf, 0x7f]),
        ];
        for (value, bytes) in cases {
            let mut buf = Vec::new();
            encode_sleb128(*value, &mut buf);
            assert_eq!(&buf[..], *bytes, "encoding {value}");
            assert_eq!(Sleb128Decoder::new(bytes).pop_front().unwrap(), *value);
        }
    }

    #[test]
    fn sleb128_ten_byte_extremes() {
        // i64::MIN and i64::MAX need ten bytes. The tenth byte only contributes one bit, and
        // sign extension must not be applied at shift == 63 + 7 == 70.
        let mut buf = Vec::new();
        encode_sleb128(i64::MIN, &mut buf);
        assert_eq!(buf.len(), 10, "i64::MIN encoding: {buf:02x?}");
        assert_eq!(Sleb128Decoder::new(&buf).pop_front().unwrap(), i64::MIN);
        buf.clear();
        encode_sleb128(i64::MAX, &mut buf);
        assert_eq!(buf.len(), 10, "i64::MAX encoding: {buf:02x?}");
        assert_eq!(Sleb128Decoder::new(&buf).pop_front().unwrap(), i64::MAX);
    }

    #[test]
    fn sleb128_truncated_is_an_error_not_a_panic() {
        // 0x80 sets the continuation bit with nothing following.
        let mut dec = Sleb128Decoder::new(&[0x80]);
        assert_eq!(
            dec.pop_front().unwrap_err(),
            ElfError::Aps2Truncated {
                offset: 1,
                total: 1
            }
        );
        let mut dec = Sleb128Decoder::new(&[]);
        assert!(matches!(
            dec.pop_front().unwrap_err(),
            ElfError::Aps2Truncated { offset: 0, total: 0 }
        ));
    }

    #[test]
    fn sleb128_overlong_padded_encoding_decodes_to_the_same_value() {
        // A non-canonical but legal encoding of 1: six redundant continuation bytes.
        let bytes = [0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        assert_eq!(Sleb128Decoder::new(&bytes).pop_front().unwrap(), 1);
        // And of -1.
        let bytes = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert_eq!(Sleb128Decoder::new(&bytes).pop_front().unwrap(), -1);
    }
}
