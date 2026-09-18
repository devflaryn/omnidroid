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
//!
//! We also bound the declared relocation count against the object's own loadable size, because a
//! fully-grouped group spends **zero** bytes per relocation and so a thirty-byte blob can declare
//! 2⁶². See [`Aps2Limits`] for the bound and the argument that it cannot reject a real binary.
//! Allocation is fallible throughout: this crate never aborts the host process on malformed
//! input, however hostile.

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

/// The resource bounds a packed-relocation decode is held to.
///
/// # Why a limit is unavoidable
///
/// A group whose flags share the offset delta, the `r_info` **and** the addend spends **zero**
/// bytes per relocation: all of its per-relocation fields live in the group header. That is not a
/// defect, it is the point of the format. The consequence is that a thirty-byte blob can
/// legitimately declare 2⁶² relocations, and no bound derived only from `blob.len()` can
/// distinguish that from a valid encoding. Without a limit, the streaming decoder runs
/// essentially forever and the `Vec` decoder tries to allocate terabytes.
///
/// # How the decoder is bounded, in four layers
///
/// Three of them need no external information at all, which matters because anything derived from
/// a header is only as trustworthy as the header:
///
/// 1. **A group whose relocations each consume bytes is bounded by the bytes that remain.** From
///    the group flags the decoder knows the minimum bytes each relocation must read; if
///    `size × min_bytes` exceeds what is left in the blob, the group is refused
///    ([`crate::ElfError::Aps2GroupLargerThanStream`]). This cannot reject a valid blob, because a
///    valid blob contains those bytes.
///
///    **Every one of `libroblox.so`'s 46,184 groups is of this kind**, so the real binary is
///    bounded without its headers being consulted at all. That is established by decoding it
///    with `image_span = 0` — which makes layers 2 and 3 reject any zero-cost group outright —
///    and still getting all 568,272 relocations. `observed_group_flags` cannot show this: it is
///    a union, and a union over groups says nothing about every group.
/// 2. **A zero-cost group whose shared offset delta is zero may hold one relocation.** All of its
///    relocations would be bit-identical — same target, same `r_info`, same addend — so every one
///    after the first is dead ([`crate::ElfError::Aps2DeadGroup`]).
/// 3. **A zero-cost group with a non-zero stride must fit in the image.** Its targets are
///    `o, o+d, …, o+(size-1)d`, and all must be mapped, so `(size-1) × |d| ≤ image_span`
///    ([`crate::ElfError::Aps2GroupExceedsImage`]). This is the only layer that needs the image,
///    and it is O(1) per group rather than per relocation.
/// 4. **The declared count itself**, checked before a single group is read, against
///    [`Self::max_relocations`].
///
/// # The count bound, and why it cannot reject a real binary
///
/// Every relocation must be *applied*, which means writing at least
/// [`Self::MIN_RELOCATION_FOOTPRINT`] bytes to a **mapped** byte of the object's image. So the
/// number of relocations with pairwise-distinct writes is at most
/// `LoadImage::mapped_bytes / MIN_RELOCATION_FOOTPRINT`. Exceeding that is a pigeonhole argument:
/// two relocations must write the same bytes, so one of them is dead — its effect entirely
/// overwritten by the other. No linker emits a dead relocation.
///
/// Two literal exceptions exist and are worth naming rather than glossing: `R_AARCH64_NONE` writes
/// nothing, and `R_AARCH64_COPY` writes `st_size`, which may be 0 or 1. Both may legally appear in
/// a dynamic table, so the bound is on *effective* relocations rather than on entries. At the
/// measured margins — 74× to 341× across the eleven ARM64 libraries in `Roblox-2.738.1397.apk`,
/// with `libroblox.so` 106× below its cap — a handful of no-op entries is immaterial. The test
/// suite asserts the margin for all eleven, so drift becomes visible long before anything is
/// rejected.
///
/// # And why there is also a flat ceiling
///
/// [`Self::MAX_RELOCATIONS`] caps the derived figure. The derived figure comes from validated
/// header fields — but validation only removes the *absurd* values. A plausible lie
/// (`p_memsz = 2^40`, overflowing nothing, contradicting nothing) would still buy a ceiling of
/// half a trillion, and a bound whose worst case is minutes of CPU and terabytes of requested
/// memory is not a bound. The flat ceiling depends on no file data at all.
/// `image::MAX_IMAGE_SPAN` closes the same hole from the other side by capping a forgeable input
/// back down to a bounded one; either alone would be enough, and having both means a mistake in
/// one is not fatal. [`Self::MAX_RELOCATIONS`] documents both the time and the memory it permits,
/// since quoting only the time would understate the price.
///
/// Note that validating each decoded `r_offset` against the loadable segments — a check the
/// applying loader wants anyway — is *not* a substitute for any of this: a group with a shared
/// offset delta of zero repeats one perfectly valid offset indefinitely. Layer 2 is the O(1)
/// version of that observation; a per-relocation check would not terminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aps2Limits {
    /// Reject a blob declaring more relocations than this.
    pub max_relocations: u64,
    /// The validated span of the loadable image, used by layer 3 above.
    pub image_span: u64,
}

impl Aps2Limits {
    /// The fewest bytes any AArch64 relocation writes to its target.
    ///
    /// `R_AARCH64_ABS16` is the narrowest writing type in the ABI at two bytes — there is no
    /// `ABS8`, instruction-patching types write four and `TLSDESC` sixteen. Every type that
    /// actually appears in a dynamic table writes four or eight, so using two errs towards
    /// accepting input. See the struct docs for the two no-op exceptions.
    pub const MIN_RELOCATION_FOOTPRINT: u64 = 2;

    /// The flat ceiling on the derived count bound: 64 Mi relocations.
    ///
    /// Chosen, not derived — see the struct docs for why one chosen number is necessary. It is
    /// 118× `libroblox.so`'s 568,806, by far the largest count in the target APK.
    ///
    /// What the ceiling authorises, both halves of it, because quoting only the time would
    /// understate the cost:
    ///
    /// * **Time.** At the measured ~275–320 million relocations per second, 64 Mi bounds a hostile
    ///   streaming decode to roughly 0.2 s. Measured worst case for a blob that actually reaches
    ///   the ceiling is 33.5 ms, so the 0.2 s figure is conservative.
    /// * **Memory.** 64 Mi × `size_of::<Rela>()` = **1,536 MiB**, and an eighteen-byte blob is
    ///   enough to ask for all of it through [`decode`], because a fully-grouped group spends no
    ///   bytes per relocation. That request is *fallible* — it grows by `try_reserve` and returns
    ///   [`crate::ElfError::AllocationFailed`] rather than aborting — and [`decode_with`] allocates
    ///   nothing at all, which is why the streaming path is the one to prefer for untrusted input.
    ///   But 1.5 GiB is what this constant permits, and it should be read as part of its price.
    ///
    /// A caller with a bigger object, or a tighter memory budget, can set its own through
    /// [`Self::new`].
    pub const MAX_RELOCATIONS: u64 = 64 * 1024 * 1024;

    /// An explicit bound, for callers that know their own.
    pub const fn new(max_relocations: u64, image_span: u64) -> Self {
        Self {
            max_relocations,
            image_span,
        }
    }

    /// Derive the bounds from a **validated** [`crate::image::LoadImage`].
    ///
    /// Taking the whole `LoadImage` rather than a bare integer is deliberate: the type can only be
    /// produced by `LoadImage::validate`, so it is not possible to derive a limit from unchecked
    /// header fields by accident.
    pub const fn for_image(image: &crate::image::LoadImage) -> Self {
        let derived = image.mapped_bytes / Self::MIN_RELOCATION_FOOTPRINT;
        Self {
            max_relocations: if derived < Self::MAX_RELOCATIONS {
                derived
            } else {
                Self::MAX_RELOCATIONS
            },
            image_span: image.span,
        }
    }
}

/// Decode a `DT_ANDROID_RELA` blob into a `Vec`.
pub fn decode_rela(blob: &[u8], limits: Aps2Limits) -> Result<PackedRelocations> {
    decode(blob, PackedFormat::Rela, limits)
}

/// Decode a `DT_ANDROID_REL` blob into a `Vec`.
pub fn decode_rel(blob: &[u8], limits: Aps2Limits) -> Result<PackedRelocations> {
    decode(blob, PackedFormat::Rel, limits)
}

/// Decode a packed blob into a `Vec`, with the format given explicitly.
///
/// Allocation is fallible throughout: a blob that declares more relocations than the process can
/// hold produces [`ElfError::AllocationFailed`] rather than aborting, and the vector grows
/// incrementally so a large declared count cannot cause a huge speculative reservation that a
/// later truncation error then throws away.
pub fn decode(
    blob: &[u8],
    format: PackedFormat,
    limits: Aps2Limits,
) -> Result<PackedRelocations> {
    let mut relocations: Vec<Rela> = Vec::new();
    let summary = decode_with(blob, format, limits, |r| {
        // `try_reserve(1)` is a length check when there is spare capacity and an amortised
        // (doubling) fallible growth when there is not. `push` alone aborts the process on OOM.
        relocations
            .try_reserve(1)
            .map_err(|_| ElfError::AllocationFailed {
                bytes: (relocations.len() + 1).saturating_mul(core::mem::size_of::<Rela>()),
            })?;
        relocations.push(r);
        Ok(())
    })?;
    Ok(PackedRelocations {
        relocations,
        summary,
    })
}

/// Decode a packed blob, handing each relocation to `sink` as it is produced.
///
/// This is the allocation-free entry point; a loader can apply relocations straight from here
/// without materialising 568,272 × 24 bytes. The sink returns a [`Result`] so it can stop the
/// decode on the first relocation it rejects — checking `r_offset` against the loadable segments,
/// for instance — and so that the `Vec` wrapper above can fail on allocation instead of aborting.
pub fn decode_with<F>(
    blob: &[u8],
    format: PackedFormat,
    limits: Aps2Limits,
    mut sink: F,
) -> Result<Aps2Summary>
where
    F: FnMut(Rela) -> Result<()>,
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
    // Checked here, before a single group is read, so a hostile count costs one SLEB128 read and
    // produces nothing. See `Aps2Limits` for why the bound is necessary and why it is safe.
    if declared > limits.max_relocations {
        return Err(ElfError::Aps2CountExceedsLimit {
            declared,
            limit: limits.max_relocations,
        });
    }

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

        let mut group_offset_delta: i64 = 0;
        if grouped_by_offset_delta {
            group_offset_delta = dec.pop_front()?;
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

        // Layers 1 to 3 (see `Aps2Limits`). The group header has been fully read at this point,
        // so `dec.remaining()` is exactly the budget the per-relocation fields have to live in.
        let min_bytes_each = u64::from(!grouped_by_offset_delta)
            + u64::from(!grouped_by_info)
            + u64::from(per_reloc_addend);
        if min_bytes_each > 0 {
            // Layer 1: bounded by the blob alone, and sound — a valid blob holds these bytes.
            let needed = group_size.saturating_mul(min_bytes_each);
            if needed > dec.remaining() as u64 {
                return Err(ElfError::Aps2GroupLargerThanStream {
                    group_index,
                    size: group_size,
                    min_bytes_each,
                    needed,
                    remaining: dec.remaining() as u64,
                });
            }
        } else if group_offset_delta == 0 {
            // Layer 2: every relocation here would be bit-identical, so all but one are dead.
            if group_size > 1 {
                return Err(ElfError::Aps2DeadGroup {
                    group_index,
                    size: group_size,
                });
            }
        } else {
            // Layer 3: the targets stride across the image, so the reach must fit inside it.
            let stride = group_offset_delta.unsigned_abs();
            let reach = group_size
                .saturating_sub(1)
                .checked_mul(stride)
                .ok_or(ElfError::Aps2GroupExceedsImage {
                    group_index,
                    size: group_size,
                    stride,
                    reach: u64::MAX,
                    image_span: limits.image_span,
                })?;
            if reach > limits.image_span {
                return Err(ElfError::Aps2GroupExceedsImage {
                    group_index,
                    size: group_size,
                    stride,
                    reach,
                    image_span: limits.image_span,
                });
            }
        }

        for _ in 0..group_size {
            if grouped_by_offset_delta {
                r_offset = r_offset.wrapping_add(group_offset_delta as u64);
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
            })?;
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
    fn the_flat_ceiling_applies_even_to_an_enormous_validated_image() {
        // Lives here rather than in the integration suite because it needs a synthetic
        // `LoadImage`, and `LoadImage` is `#[non_exhaustive]` precisely so that no code outside
        // this crate can build one. Keeping the test in-crate is what lets that guarantee be real
        // instead of documented-but-bypassed.
        //
        // The derived bound comes from validated header fields, but validation only removes absurd
        // values. The flat ceiling depends on no file data at all, so it still holds when the image
        // is as large as the crate will ever accept.
        use crate::image::{LoadImage, MAX_IMAGE_SPAN};
        assert_eq!(MAX_IMAGE_SPAN, 4 * 1024 * 1024 * 1024);
        let huge = LoadImage {
            base_vaddr: 0,
            end_vaddr: MAX_IMAGE_SPAN,
            span: MAX_IMAGE_SPAN,
            mapped_bytes: MAX_IMAGE_SPAN,
            max_align: 0x1000,
            segment_count: 1,
        };
        // Unclamped the derived figure would be 2^31; the ceiling holds it to 64 Mi.
        assert_eq!(
            huge.mapped_bytes / Aps2Limits::MIN_RELOCATION_FOOTPRINT,
            2_147_483_648
        );
        let limits = Aps2Limits::for_image(&huge);
        assert_eq!(limits.max_relocations, Aps2Limits::MAX_RELOCATIONS);
        assert_eq!(limits.max_relocations, 67_108_864);
        assert_eq!(limits.image_span, MAX_IMAGE_SPAN);

        // And the memory that ceiling authorises, stated as a number so the doc comment on
        // MAX_RELOCATIONS cannot drift away from it.
        assert_eq!(
            Aps2Limits::MAX_RELOCATIONS * core::mem::size_of::<Rela>() as u64,
            1_610_612_736,
            "64 Mi relocations is 1,536 MiB of Rela"
        );

        // A blob one past the ceiling is refused, and it is tiny: eighteen bytes, which is the
        // measured size of the smallest input that would ask `decode` for the full 1,536 MiB.
        let mut blob = Vec::from(APS2_MAGIC);
        encode_sleb128(67_108_865, &mut blob);
        encode_sleb128(0x1000, &mut blob);
        encode_sleb128(67_108_865, &mut blob);
        encode_sleb128(
            (RELOCATION_GROUPED_BY_INFO_FLAG | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG) as i64,
            &mut blob,
        );
        encode_sleb128(8, &mut blob);
        encode_sleb128(crate::consts::R_AARCH64_RELATIVE as i64, &mut blob);
        assert_eq!(blob.len(), 18, "the blob that would ask for 1.5 GiB");
        assert_eq!(
            decode_rela(&blob, limits).unwrap_err(),
            ElfError::Aps2CountExceedsLimit {
                declared: 67_108_865,
                limit: 67_108_864,
            }
        );
        // One below the ceiling passes the count check, so the refusal above really is the
        // ceiling firing rather than something incidental to the blob's shape.
        let mut ok = Vec::from(APS2_MAGIC);
        encode_sleb128(1, &mut ok);
        encode_sleb128(0x1000, &mut ok);
        encode_sleb128(1, &mut ok);
        encode_sleb128(
            (RELOCATION_GROUPED_BY_INFO_FLAG | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG) as i64,
            &mut ok,
        );
        encode_sleb128(8, &mut ok);
        encode_sleb128(crate::consts::R_AARCH64_RELATIVE as i64, &mut ok);
        assert_eq!(decode_rela(&ok, limits).unwrap().relocations.len(), 1);
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
