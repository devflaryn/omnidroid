//! Turning the `PT_LOAD` set into a list of pieces to map, before anything is mapped.
//!
//! # Why a separate planning step
//!
//! Mapping is destructive and partially-applied work has to be torn down, so every question that
//! can be answered from the program headers alone is answered here: how the segments round to
//! pages, whether two of them collide, which bytes come from the file and which must be zero, and
//! what protection each piece ends up at. Nothing in this module touches memory, so the whole of it
//! is testable against synthetic and tampered headers with no address space at all.
//!
//! # The three cases that are not obvious
//!
//! * **`p_vaddr` is not page-aligned.** `libroblox.so`'s second and third `PT_LOAD` start at
//!   `…c1c0` and `…67c0`. A segment is therefore mapped from `page_down(p_vaddr)` with the file
//!   offset biased by the same amount, which is legal precisely because the ELF gABI requires
//!   `p_vaddr ≡ p_offset (mod p_align)` — enforced by
//!   [`LoadImage::validate`](crate::LoadImage::validate).
//! * **The file image's last page is not all segment.** The bytes between `p_vaddr + p_filesz` and
//!   the end of that page belong to whatever follows in the file. When `p_memsz > p_filesz` they
//!   are part of `.bss` and must read as zero, so they are zeroed after mapping rather than left as
//!   the next segment's data.
//! * **The file may not end on a page boundary.** A view cannot extend past the end of the section,
//!   so when the last page of a segment's file image is only partly present in the file — which
//!   happens for the small libraries in the APK, whose whole image is a few hundred bytes — that
//!   page is mapped anonymously and the bytes are copied into it. Mapping it from the file would
//!   fail with `ViewPastEndOfFile`, and rounding the file up would be a lie about its length.

use crate::consts::PT_LOAD;
use crate::loader::error::{LoadError, LoadResult};
use crate::segment::{Segment, SegmentFlags};
use omni_mem::Protection;

/// The largest `p_align` this loader will honour: 2 MiB.
///
/// Chosen, not derived. `p_align` is a file field, and the load base is aligned to the largest one
/// in the object, so an absurd value would turn into an absurd alignment request — and, because the
/// reserved span is measured from `align_down(base_vaddr, max_align)`, into a larger span than the
/// headers describe. Real values are 4 KiB (older NDKs), 16 KiB (`libroblox.so` and every library
/// in the APK) and 64 KiB; 2 MiB is the largest page size any supported target uses, so the cap
/// cannot reject a real object while keeping every quantity derived from `p_align` small.
pub const MAX_SEGMENT_ALIGN: u64 = 2 * 1024 * 1024;

/// What a piece of the image is backed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceSource {
    /// Mapped from the library file at this offset. Costs no commit charge while it stays
    /// read-only or execute-read, and is shared with every other instance mapping the same
    /// extraction-cache entry (D11).
    File {
        /// Page-aligned file offset.
        offset: u64,
    },
    /// Private anonymous memory holding `len` bytes copied from the file at `offset`, zero after
    /// that. Only used for a final page that the file does not fully contain.
    FileTailCopy {
        /// File offset of the first byte to copy.
        offset: u64,
        /// How many bytes to copy.
        len: usize,
    },
    /// Private anonymous zero memory: `.bss`.
    Zero,
}

/// One contiguous, page-aligned piece of the loaded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    /// Index of the `PT_LOAD` in the program-header table this piece belongs to.
    pub segment: usize,
    /// Page-aligned unbiased start address: add the load base to get the guest address.
    pub vaddr: u64,
    /// Length in bytes, a whole number of pages.
    pub len: usize,
    /// Where the bytes come from.
    pub source: PieceSource,
    /// The protection this piece must be left at once loading is finished.
    pub rest: Protection,
    /// Whether the piece is private anonymous memory rather than a file view. Relocating into one
    /// needs a commit, not a copy-on-write protect.
    pub anonymous: bool,
}

impl Piece {
    /// One past the last unbiased byte.
    #[must_use]
    pub fn vaddr_end(&self) -> u64 {
        self.vaddr + self.len as u64
    }
}

/// A zero-fill the loader owes after mapping: the tail of a segment's last file page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroFill {
    /// Unbiased start.
    pub vaddr: u64,
    /// Length in bytes. Never a whole page — a whole page of `.bss` is a [`PieceSource::Zero`]
    /// piece instead.
    pub len: usize,
}

/// The whole mapping plan for one object.
#[derive(Debug, Clone)]
pub struct LoadPlan {
    /// `align_down(base_vaddr, max_align)`: the unbiased address the reservation starts at.
    pub image_start: u64,
    /// `page_up(end_vaddr)`: one past the last unbiased byte of the image.
    pub image_end: u64,
    /// Bytes to reserve: `image_end - image_start`.
    pub span: usize,
    /// Alignment the load base must satisfy, so that every segment lands congruent to its
    /// `p_offset` modulo its own `p_align`.
    pub base_align: usize,
    /// The pieces to map, in ascending address order.
    pub pieces: Vec<Piece>,
    /// Zero-fills owed after mapping, in ascending address order.
    pub zero_fills: Vec<ZeroFill>,
}

/// Round down to a page boundary.
#[must_use]
pub fn page_down(value: u64, page: usize) -> u64 {
    value & !(page as u64 - 1)
}

/// Round up to a page boundary, or `None` on overflow.
#[must_use]
pub fn page_up(value: u64, page: usize) -> Option<u64> {
    let mask = page as u64 - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

/// Translate `p_flags` into the protection a segment rests at.
///
/// A segment that is both writable and executable is **refused**. There is no [`Protection`]
/// variant for it by design (D12: the JIT arena exists so that no page is ever writable and
/// executable at once), so silently dropping one of the two bits would either make guest code
/// unrunnable or make a data page executable. Neither is a decision a loader should take quietly,
/// and no library in the APK has such a segment.
fn rest_protection(index: usize, flags: SegmentFlags) -> LoadResult<Protection> {
    let w = flags.contains(SegmentFlags::WRITE);
    let x = flags.contains(SegmentFlags::EXEC);
    let r = flags.contains(SegmentFlags::READ);
    match (r, w, x) {
        (_, true, true) => Err(LoadError::WritableExecutableSegment { index, flags: flags.bits() }),
        (_, true, false) => Ok(Protection::ReadWrite),
        (_, false, true) => Ok(Protection::ReadExecute),
        (true, false, false) => Ok(Protection::Read),
        (false, false, false) => Ok(Protection::None),
    }
}

impl LoadPlan {
    /// Build the plan from a parsed image.
    ///
    /// `file_len` is the real length of the library file, which is the one quantity here that no
    /// header field can forge.
    ///
    /// # Errors
    ///
    /// [`LoadError::AlignBelowPageSize`], [`LoadError::AlignTooLarge`],
    /// [`LoadError::SegmentsOverlap`], [`LoadError::WritableExecutableSegment`], or
    /// [`LoadError::AddressOverflow`].
    pub fn build(image: &crate::ElfImage<'_>, file_len: u64, page: usize) -> LoadResult<Self> {
        let load = image.load_image();
        let mut max_align = page as u64;
        for (index, seg) in image.segments().iter().enumerate() {
            if seg.p_type != PT_LOAD {
                continue;
            }
            // `LoadImage::validate` has already established that p_align is 0, 1, or a power of
            // two, and that p_vaddr ≡ p_offset (mod p_align). What it cannot know is the host page
            // size, so the mappability check lives here.
            if seg.p_align > 1 && seg.p_align < page as u64 {
                return Err(LoadError::AlignBelowPageSize {
                    index,
                    align: seg.p_align,
                    page_size: page,
                });
            }
            if seg.p_align > MAX_SEGMENT_ALIGN {
                return Err(LoadError::AlignTooLarge {
                    index,
                    align: seg.p_align,
                    limit: MAX_SEGMENT_ALIGN,
                });
            }
            max_align = max_align.max(seg.p_align);
        }

        let image_start = load.base_vaddr & !(max_align - 1);
        let image_end = page_up(load.end_vaddr, page).ok_or(LoadError::AddressOverflow {
            what: "page_up(end_vaddr)",
            base: 0,
            vaddr: load.end_vaddr,
        })?;
        let span = usize::try_from(image_end - image_start).map_err(|_| {
            LoadError::AddressOverflow { what: "image span", base: 0, vaddr: image_end }
        })?;

        // Segments in ascending p_vaddr order, so the overlap check is a single sweep and the
        // pieces come out sorted.
        let mut loads: Vec<(usize, Segment)> = image
            .segments()
            .iter()
            .enumerate()
            .filter(|(_, s)| s.p_type == PT_LOAD)
            .map(|(i, s)| (i, *s))
            .collect();
        loads.sort_by_key(|(_, s)| s.p_vaddr);

        let mut pieces = Vec::new();
        let mut zero_fills = Vec::new();
        let mut previous: Option<(usize, u64, u64)> = None;
        let file_page_end = page_down(file_len, page);

        for (index, seg) in loads {
            let mem_start = page_down(seg.p_vaddr, page);
            let mem_end = page_up(seg.vaddr_end(), page).ok_or(LoadError::AddressOverflow {
                what: "page_up(p_vaddr + p_memsz)",
                base: 0,
                vaddr: seg.vaddr_end(),
            })?;

            // Two segments cannot share a page: one file range would have to be mapped over the
            // other, and whichever lost would be silently wrong.
            if let Some((prev_index, prev_start, prev_end)) = previous {
                if mem_start < prev_end {
                    return Err(LoadError::SegmentsOverlap {
                        first: prev_index,
                        first_start: prev_start,
                        first_end: prev_end,
                        second: index,
                        second_start: mem_start,
                        second_end: mem_end,
                    });
                }
            }
            previous = Some((index, mem_start, mem_end));

            let rest = rest_protection(index, seg.p_flags)?;

            // The file-backed part: from the segment's first page up to the page that holds the
            // last byte of its file image.
            let file_need_end = if seg.p_filesz == 0 {
                mem_start
            } else {
                page_up(seg.p_vaddr + seg.p_filesz, page)
                    .ok_or(LoadError::AddressOverflow {
                        what: "page_up(p_vaddr + p_filesz)",
                        base: 0,
                        vaddr: seg.p_vaddr.saturating_add(seg.p_filesz),
                    })?
                    .min(mem_end)
            };

            if file_need_end > mem_start {
                // Congruence modulo p_align implies congruence modulo the page size, so this is a
                // page-aligned file offset. When p_align is 0 or 1 the gABI says nothing, so the
                // congruence is checked here instead of assumed.
                let bias = seg.p_vaddr - mem_start;
                if seg.p_offset < bias {
                    return Err(LoadError::SegmentOffsetBelowPageBias {
                        index,
                        offset: seg.p_offset,
                        vaddr: seg.p_vaddr,
                        bias,
                    });
                }
                let file_offset = seg.p_offset - bias;
                if file_offset % page as u64 != 0 {
                    return Err(LoadError::SegmentOffsetNotCongruent {
                        index,
                        offset: seg.p_offset,
                        vaddr: seg.p_vaddr,
                        page_size: page,
                    });
                }

                let want = file_need_end - mem_start;
                // A view may not run past the end of the section, and the section is exactly as
                // long as the file. Anything beyond the file's last whole page becomes a copy.
                let available = file_page_end.saturating_sub(file_offset);
                let view_len = want.min(available);
                if view_len > 0 {
                    pieces.push(Piece {
                        segment: index,
                        vaddr: mem_start,
                        len: usize::try_from(view_len).map_err(|_| LoadError::AddressOverflow {
                            what: "file view length",
                            base: 0,
                            vaddr: view_len,
                        })?,
                        source: PieceSource::File { offset: file_offset },
                        rest,
                        anonymous: false,
                    });
                }
                if view_len < want {
                    let tail_vaddr = mem_start + view_len;
                    let tail_len = want - view_len;
                    // Bytes of the segment's file image that fall in this final piece. Clipped to
                    // the file, so a truncated file copies what exists and zeroes the rest.
                    let copy_from = file_offset + view_len;
                    let copy_len = (seg.p_offset + seg.p_filesz)
                        .min(file_len)
                        .saturating_sub(copy_from)
                        .min(tail_len);
                    pieces.push(Piece {
                        segment: index,
                        vaddr: tail_vaddr,
                        len: usize::try_from(tail_len).map_err(|_| LoadError::AddressOverflow {
                            what: "file tail length",
                            base: 0,
                            vaddr: tail_len,
                        })?,
                        source: PieceSource::FileTailCopy {
                            offset: copy_from,
                            len: usize::try_from(copy_len).map_err(|_| {
                                LoadError::AddressOverflow {
                                    what: "file tail copy length",
                                    base: 0,
                                    vaddr: copy_len,
                                }
                            })?,
                        },
                        rest,
                        anonymous: true,
                    });
                }

                // The tail of the last file page that is `.bss` rather than file content. Only
                // owed for a piece that really came from the file: a FileTailCopy piece is
                // anonymous and already zero past its copied bytes.
                let seg_file_end = seg.p_vaddr + seg.p_filesz;
                if seg.p_memsz > seg.p_filesz && seg_file_end < mem_start + view_len {
                    let to = (mem_start + view_len).min(mem_end);
                    if to > seg_file_end {
                        zero_fills.push(ZeroFill {
                            vaddr: seg_file_end,
                            len: usize::try_from(to - seg_file_end).map_err(|_| {
                                LoadError::AddressOverflow {
                                    what: "zero-fill length",
                                    base: 0,
                                    vaddr: to - seg_file_end,
                                }
                            })?,
                        });
                    }
                }
            }

            // Whole pages of `.bss`.
            if mem_end > file_need_end {
                pieces.push(Piece {
                    segment: index,
                    vaddr: file_need_end,
                    len: usize::try_from(mem_end - file_need_end).map_err(|_| {
                        LoadError::AddressOverflow {
                            what: "bss length",
                            base: 0,
                            vaddr: mem_end - file_need_end,
                        }
                    })?,
                    source: PieceSource::Zero,
                    rest,
                    anonymous: true,
                });
            }
        }

        Ok(LoadPlan {
            image_start,
            image_end,
            span,
            base_align: usize::try_from(max_align).map_err(|_| LoadError::AddressOverflow {
                what: "base alignment",
                base: 0,
                vaddr: max_align,
            })?,
            pieces,
            zero_fills,
        })
    }

    /// Total bytes this plan maps from the file.
    #[must_use]
    pub fn file_backed_bytes(&self) -> usize {
        self.pieces.iter().filter(|p| !p.anonymous).map(|p| p.len).sum()
    }

    /// Total bytes this plan maps as private anonymous memory.
    #[must_use]
    pub fn anonymous_bytes(&self) -> usize {
        self.pieces.iter().filter(|p| p.anonymous).map(|p| p.len).sum()
    }
}
