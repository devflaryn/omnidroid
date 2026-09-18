//! Validation of the `PT_LOAD` set, and the measured extent of the loadable image.
//!
//! # Why this is its own step
//!
//! Anything derived from a program header is only as trustworthy as the header. The relocation
//! count ceiling in [`crate::aps2::Aps2Limits`] is computed from the loadable image, so `p_memsz`
//! and `p_vaddr` stop being descriptive data and become a *permission*: inflate them and the
//! ceiling rises with them. An attacker needs eight bytes to write `u64::MAX` over one
//! `p_memsz`.
//!
//! So the `PT_LOAD` set is validated **before** anything is derived from it, and every arithmetic
//! step is checked rather than saturating. Saturating would convert hostile input into a *larger*
//! permission, which is exactly backwards.

use crate::error::{ElfError, Result};
use crate::segment::Segment;

/// The measured, validated extent of an object's loadable image.
///
/// Every field here has survived [`LoadImage::validate`], so a consumer may derive limits from it.
///
/// `#[non_exhaustive]` is load-bearing, not tidiness: it is what makes "only `validate` can produce
/// one" true for code outside this crate, so a caller cannot assemble a flattering image by struct
/// literal and hand it to [`crate::aps2::Aps2Limits::for_image`]. Fields stay `pub` because reading
/// them is the point; only construction is restricted. In-crate tests may still build one
/// directly — `#[non_exhaustive]` does not apply within the defining crate — which is why the test
/// that needs a synthetic 4 GiB image lives in `aps2.rs` rather than in the integration suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoadImage {
    /// Lowest `p_vaddr` of any `PT_LOAD`.
    pub base_vaddr: u64,
    /// Highest `p_vaddr + p_memsz` of any `PT_LOAD`.
    pub end_vaddr: u64,
    /// `end_vaddr - base_vaddr`: the address space a loader must reserve.
    pub span: u64,
    /// The measure of the **union** of the `PT_LOAD` memory ranges — the bytes that are actually
    /// mapped, with the gaps between segments excluded.
    ///
    /// This is the quantity the pigeonhole bound in [`crate::aps2::Aps2Limits`] needs: a
    /// relocation must land on a mapped byte, and a gap between two segments is not one. It is
    /// never larger than [`Self::span`] and never larger than the sum of the `p_memsz` values, so
    /// it is the tightest of the three. On all eleven ARM64 libraries in the target APK the
    /// segments happen not to overlap, so here it equals that sum.
    pub mapped_bytes: u64,
    /// Largest `p_align` across the `PT_LOAD` segments: the mapping granularity the object needs.
    pub max_align: u64,
    /// How many `PT_LOAD` segments there are.
    pub segment_count: usize,
}

/// The largest loadable image this crate will accept.
///
/// Unlike the rest of [`LoadImage`], this is **chosen rather than derived**. It exists because
/// every field a bound could be derived from is a field an attacker types: validating them removes
/// the absurd values, but a *plausible* lie — `p_memsz = 2^40`, which overflows nothing and
/// contradicts no other header — would still buy a terabyte-wide image and a correspondingly
/// useless relocation ceiling. Its whole job is to **cap a forgeable input back down to a bounded
/// one**, so that everything derived from the image is derived from something bounded.
///
/// Why 4 GiB is safe for real input:
///
/// * It is 35× the largest image in the target APK (`libroblox.so`, 120,798,268 bytes of span).
/// * An AArch64 shared object larger than 4 GiB is not linkable in the first place. `ADRP`+`ADD`
///   reaches ±4 GiB, so both the small and large code models cap intra-object PC-relative
///   addressing there; a linker cannot emit a working `.so` past it.
///
/// Note what is *not* the reason: this is **not** about the cost of reserving the span.
/// `docs/DECISIONS.md` D10 measured that address-space reservation is free — 0 bytes of commit
/// charge, verified out to 97.7 TB — and says in as many words that it is not the thing to
/// economize on. A rationale resting on reservation cost would argue against its own constant.
pub const MAX_IMAGE_SPAN: u64 = 4 * 1024 * 1024 * 1024;

impl LoadImage {
    /// Validate the `PT_LOAD` set and measure it.
    ///
    /// `file_len` is the real length of the byte slice, which is the one quantity in this whole
    /// computation that no header field can forge.
    ///
    /// The rules, and why each one is here:
    ///
    /// * **At least one `PT_LOAD`.** Without one there is no image, so a relocation has nowhere to
    ///   land and no virtual address can be translated.
    /// * **`p_offset + p_filesz <= file_len`**, checked for overflow. This is the anchor: it ties
    ///   a header to the bytes that actually exist.
    /// * **`p_filesz <= p_memsz`.** A segment with more file content than memory to hold it is
    ///   malformed; accepting it would mean the file image did not fit in the memory image.
    /// * **`p_vaddr + p_memsz` must not overflow.** This is what the reviewer's eight-byte
    ///   `u64::MAX` attack trips, and it must be an error rather than a saturated maximum.
    /// * **`p_align` is 0, 1, or a power of two**, and when it is greater than 1,
    ///   **`p_vaddr ≡ p_offset (mod p_align)`**. The congruence is required by the ELF gABI and is
    ///   what makes the segment mappable at all; all eleven libraries in the target APK satisfy
    ///   it. Without it a loader cannot place the segment from the file at any aligned address.
    /// * **The span must not overflow and must not exceed [`MAX_IMAGE_SPAN`].**
    /// * **The mapped-byte total must not overflow.**
    pub fn validate(segments: &[Segment], file_len: u64) -> Result<Self> {
        let mut loads: Vec<&Segment> = segments.iter().filter(|s| s.is_load()).collect();
        if loads.is_empty() {
            return Err(ElfError::NoLoadSegments);
        }

        let mut base_vaddr = u64::MAX;
        let mut end_vaddr = 0u64;
        let mut max_align = 0u64;

        for (index, s) in loads.iter().enumerate() {
            let file_end = s
                .p_offset
                .checked_add(s.p_filesz)
                .ok_or(ElfError::SegmentFileRangeOverflow {
                    index,
                    offset: s.p_offset,
                    filesz: s.p_filesz,
                })?;
            if file_end > file_len {
                return Err(ElfError::SegmentOutsideFile {
                    index,
                    offset: s.p_offset,
                    filesz: s.p_filesz,
                    file_len,
                });
            }
            if s.p_filesz > s.p_memsz {
                return Err(ElfError::SegmentFileSizeExceedsMemSize {
                    index,
                    filesz: s.p_filesz,
                    memsz: s.p_memsz,
                });
            }
            let mem_end =
                s.p_vaddr
                    .checked_add(s.p_memsz)
                    .ok_or(ElfError::SegmentMemRangeOverflow {
                        index,
                        vaddr: s.p_vaddr,
                        memsz: s.p_memsz,
                    })?;
            if s.p_align > 1 {
                if !s.p_align.is_power_of_two() {
                    return Err(ElfError::SegmentAlignNotPowerOfTwo {
                        index,
                        align: s.p_align,
                    });
                }
                let mask = s.p_align - 1;
                if s.p_vaddr & mask != s.p_offset & mask {
                    return Err(ElfError::SegmentAlignMismatch {
                        index,
                        vaddr: s.p_vaddr,
                        offset: s.p_offset,
                        align: s.p_align,
                    });
                }
            }

            base_vaddr = base_vaddr.min(s.p_vaddr);
            end_vaddr = end_vaddr.max(mem_end);
            max_align = max_align.max(s.p_align);
        }

        let span = end_vaddr
            .checked_sub(base_vaddr)
            .ok_or(ElfError::ImageSpanOverflow {
                base_vaddr,
                end_vaddr,
            })?;
        if span > MAX_IMAGE_SPAN {
            return Err(ElfError::ImageSpanTooLarge {
                span,
                limit: MAX_IMAGE_SPAN,
            });
        }

        // Measure of the union of the memory ranges. Sorting by start and merging is the only way
        // to avoid double-counting an overlap, which would inflate the figure a bound is derived
        // from — again, the unsafe direction.
        loads.sort_by_key(|s| s.p_vaddr);
        let mut mapped_bytes = 0u64;
        let mut cursor: Option<(u64, u64)> = None;
        for s in &loads {
            let start = s.p_vaddr;
            let end = s.p_vaddr + s.p_memsz; // checked above
            match cursor {
                Some((cs, ce)) if start <= ce => cursor = Some((cs, ce.max(end))),
                Some((cs, ce)) => {
                    mapped_bytes = mapped_bytes
                        .checked_add(ce - cs)
                        .ok_or(ElfError::ImageSpanOverflow {
                            base_vaddr,
                            end_vaddr,
                        })?;
                    cursor = Some((start, end));
                }
                None => cursor = Some((start, end)),
            }
        }
        if let Some((cs, ce)) = cursor {
            mapped_bytes = mapped_bytes
                .checked_add(ce - cs)
                .ok_or(ElfError::ImageSpanOverflow {
                    base_vaddr,
                    end_vaddr,
                })?;
        }

        Ok(LoadImage {
            base_vaddr,
            end_vaddr,
            span,
            mapped_bytes,
            max_align,
            segment_count: loads.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{PT_LOAD, PT_NOTE};
    use crate::segment::SegmentFlags;

    fn load(offset: u64, filesz: u64, vaddr: u64, memsz: u64, align: u64) -> Segment {
        Segment {
            p_type: PT_LOAD,
            p_flags: SegmentFlags::READ,
            p_offset: offset,
            p_vaddr: vaddr,
            p_paddr: vaddr,
            p_filesz: filesz,
            p_memsz: memsz,
            p_align: align,
        }
    }

    #[test]
    fn measures_a_gappy_image() {
        // Two segments 0x1000 apart with a 0x1000 gap: span counts the gap, mapped_bytes does not.
        let segs = [
            load(0, 0x1000, 0, 0x1000, 0x1000),
            load(0x1000, 0x1000, 0x3000, 0x1000, 0x1000),
        ];
        let img = LoadImage::validate(&segs, 0x2000).unwrap();
        assert_eq!(img.base_vaddr, 0);
        assert_eq!(img.end_vaddr, 0x4000);
        assert_eq!(img.span, 0x4000);
        assert_eq!(img.mapped_bytes, 0x2000, "the 0x1000 gap is not mapped");
        assert_eq!(img.max_align, 0x1000);
        assert_eq!(img.segment_count, 2);
    }

    #[test]
    fn overlapping_segments_are_counted_once() {
        let segs = [
            load(0, 0x1000, 0x1000, 0x2000, 1),
            load(0x1000, 0x1000, 0x2000, 0x2000, 1),
        ];
        let img = LoadImage::validate(&segs, 0x2000).unwrap();
        assert_eq!(img.span, 0x3000);
        assert_eq!(
            img.mapped_bytes, 0x3000,
            "0x1000..0x4000 mapped once, not 0x4000 bytes"
        );
    }

    #[test]
    fn rejects_a_memsz_that_overflows_the_address_space() {
        // The eight-byte attack: one p_memsz set to u64::MAX.
        let segs = [load(0, 0x1000, 0x1000, u64::MAX, 1)];
        assert_eq!(
            LoadImage::validate(&segs, 0x1000).unwrap_err(),
            ElfError::SegmentMemRangeOverflow {
                index: 0,
                vaddr: 0x1000,
                memsz: u64::MAX,
            }
        );
    }

    #[test]
    fn rejects_an_inflated_but_non_overflowing_memsz() {
        // A plausible lie: 1 TiB, which overflows nothing and contradicts no other field.
        let segs = [load(0, 0x1000, 0, 1 << 40, 1)];
        assert_eq!(
            LoadImage::validate(&segs, 0x1000).unwrap_err(),
            ElfError::ImageSpanTooLarge {
                span: 1 << 40,
                limit: MAX_IMAGE_SPAN,
            }
        );
    }

    #[test]
    fn rejects_segments_that_do_not_describe_real_bytes() {
        assert_eq!(
            LoadImage::validate(&[load(0, 0x2000, 0, 0x2000, 1)], 0x1000).unwrap_err(),
            ElfError::SegmentOutsideFile {
                index: 0,
                offset: 0,
                filesz: 0x2000,
                file_len: 0x1000,
            }
        );
        assert_eq!(
            LoadImage::validate(&[load(u64::MAX, 1, 0, 1, 1)], 0x1000).unwrap_err(),
            ElfError::SegmentFileRangeOverflow {
                index: 0,
                offset: u64::MAX,
                filesz: 1,
            }
        );
        assert_eq!(
            LoadImage::validate(&[load(0, 0x1000, 0, 0x800, 1)], 0x1000).unwrap_err(),
            ElfError::SegmentFileSizeExceedsMemSize {
                index: 0,
                filesz: 0x1000,
                memsz: 0x800,
            }
        );
    }

    #[test]
    fn rejects_bad_alignment() {
        assert_eq!(
            LoadImage::validate(&[load(0, 0x1000, 0, 0x1000, 3)], 0x1000).unwrap_err(),
            ElfError::SegmentAlignNotPowerOfTwo { index: 0, align: 3 }
        );
        // vaddr and offset must be congruent modulo p_align or the segment cannot be mapped.
        assert_eq!(
            LoadImage::validate(&[load(0x100, 0x1000, 0x200, 0x1000, 0x1000)], 0x2000).unwrap_err(),
            ElfError::SegmentAlignMismatch {
                index: 0,
                vaddr: 0x200,
                offset: 0x100,
                align: 0x1000,
            }
        );
        // Congruent but not aligned to zero is fine, which is the normal case.
        assert!(LoadImage::validate(&[load(0x1c0, 0x1000, 0x41c0, 0x1000, 0x4000)], 0x2000).is_ok());
    }

    #[test]
    fn rejects_an_object_with_no_pt_load() {
        let mut s = load(0, 0, 0, 0, 1);
        s.p_type = PT_NOTE;
        assert_eq!(
            LoadImage::validate(&[s], 0x1000).unwrap_err(),
            ElfError::NoLoadSegments
        );
    }
}
