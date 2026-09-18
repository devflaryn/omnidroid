//! Applying relocations, in windows.
//!
//! # Why windows, and why this is the central constraint of the whole loader
//!
//! Copy-on-write is charged at **`protect` time, not at write time**. Measured twice
//! independently: an 8 MiB `ReadExecute` view costs +0.020 MiB of commit charge; protecting it to
//! `ReadWrite` costs **+8.020 MiB immediately**, before a single byte is written; writing pages adds
//! nothing further; restoring the original protection refunds everything except the pages actually
//! dirtied (D11, "Further corrections from building on it"). Re-measured while building this
//! loader: a 64 KiB window of an 8 MiB view costs +0.062 MiB while protected and +0.004 MiB after
//! restore, with one page written.
//!
//! So a loader that drops `libroblox.so` to `ReadWrite` to relocate it charges **109 MB of commit
//! per instance** for the duration, which is the exact thing the project's memory requirement
//! forbids, at the worst possible moment. Instead: protect a window, apply the relocations that land
//! in it, restore the window's original protection, advance. The transient charge is then the window
//! size, not the library size.
//!
//! Relocations are sorted by target before the sweep, so windows are disjoint and ascending within
//! a table and each page is made writable at most once.
//!
//! # `ABS64` is a 64-bit store
//!
//! All four supported types write eight bytes. An earlier draft of the plan recorded the 22
//! symbolic relocations in the `APS2` blob as `R_AARCH64_ABS32` (258); they are
//! **`R_AARCH64_ABS64` (257)**. Writing four bytes where eight are required would leave the high
//! half of 22 pointers holding whatever was there before, with a crash arbitrarily far from the
//! cause, so the store width is taken from the relocation type and nothing else.

use std::collections::BTreeMap;

use omni_mem::{GuestSpace, Protection};

use crate::consts::*;
use crate::loader::error::{LoadError, LoadResult};
use crate::loader::{LiveRange, SymbolBinding};
use crate::reloc::{Rela, RelocationTable};

/// The default relocation window: 64 KiB.
///
/// # Why 64 KiB, measured
///
/// The window size sets the transient copy-on-write charge, so the only reason not to make it tiny
/// is the cost of the two `protect` calls per window. Measured on `libroblox.so`, whose 568,806
/// relocations fall in 5.5 MB of writable image:
///
/// | window | windows opened | relocate wall-time | peak transient charge |
/// |---|---|---|---|
/// | 4 KiB | 2,438 | 20.9 ms | 4 KiB |
/// | 16 KiB | 646 | 14.9 ms | 16 KiB |
/// | **64 KiB** | **171** | **11.6 ms** | **64 KiB** |
/// | 256 KiB | 47 | 10.4 ms | 256 KiB |
/// | 1 MiB | 15 | 10.3 ms | 1 MiB |
///
/// (Release build, this machine, streaming the packed blob; see the Task 5 report for the full
/// table and the matching commit-charge column.)
///
/// 64 KiB is chosen because it is also [`omni_mem::DEFAULT_COMMIT_GRANULE`], so a window that lands
/// in `.bss` needs exactly one commit granule and never straddles two, and because it is the
/// Windows allocation granularity. The transient charge it costs is 1/1,700th of the library.
pub const DEFAULT_RELOCATION_WINDOW: usize = 64 * 1024;

/// What the relocation pass did. Every field is a count of something that happened, not a
/// restatement of the input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelocationStats {
    /// Relocations seen across every table.
    pub total: usize,
    /// Relocations that wrote to memory.
    pub applied: usize,
    /// `R_AARCH64_NONE` entries, which are padding and write nothing.
    pub none: usize,
    /// How many of each `ELF64_R_TYPE` were seen.
    pub by_type: BTreeMap<u32, usize>,
    /// Relocations from the `APS2` packed table(s).
    pub packed: usize,
    /// Relocations from `DT_JMPREL`.
    pub plt: usize,
    /// Relocations that referenced a symbol.
    pub symbolic: usize,
    /// Symbolic relocations whose symbol nothing supplied, and which therefore wrote a null.
    pub bound_to_null: usize,
    /// How many windows were opened.
    pub windows: usize,
    /// The largest window actually protected, in bytes. This is the peak transient copy-on-write
    /// charge the sweep can cost, since only one window is writable at a time.
    pub largest_window: usize,
    /// Total bytes protected to `ReadWrite` and back. Windows within a table are disjoint, so for a
    /// single-table object this is the number of distinct bytes made writable.
    pub windowed_bytes: usize,
    /// Bytes of anonymous `.bss` that had to be committed because a relocation targeted them.
    pub committed_for_relocation: usize,
    /// Peak process commit charge observed while a window was writable, when measurement was
    /// enabled.
    pub peak_commit_charge: Option<u64>,
}

impl RelocationStats {
    /// How many relocations of one type were seen.
    #[must_use]
    pub fn count_of_type(&self, ty: u32) -> usize {
        self.by_type.get(&ty).copied().unwrap_or(0)
    }
}

/// A window that is open right now: writable, waiting for the relocations that land in it.
#[derive(Debug, Clone, Copy)]
struct OpenWindow {
    start: usize,
    end: usize,
    /// The protection to restore, or `None` if the range was already writable and nothing was
    /// changed.
    restore: Option<Protection>,
}

/// The windowed relocation engine.
///
/// Relocations are fed in **one at a time**, so the same window logic serves both a materialised
/// table and the `APS2` blob streamed straight out of the decoder. A window stays open until a
/// relocation arrives that does not fit in it, which makes the window count a function of how sorted
/// the input is rather than a requirement on it: `libroblox.so`'s blob is emitted in ascending target
/// order apart from **two** descents out of 568,272.
///
/// Two descents are not two extra windows. A descent rewinds the cursor, so every window between the
/// descent's target and the point it came from is opened a second time: measured, streaming
/// `libroblox.so` opens **171** windows where sorting it first opens **87**. That is the honest cost
/// of not buffering — one descent early in the blob replays most of the writable image. The peak is
/// unaffected, because one window is writable at a time either way, and the measured wall-time is
/// better regardless, so sorting remains an optimisation rather than a requirement. If the ratio
/// ever matters, a chunked sort — buffer a bounded run, sort it, feed it — is the cheaper structure
/// than either extreme.
pub(crate) struct Relocator<'a> {
    space: &'a GuestSpace,
    base: usize,
    /// Mapped pieces of the image as they are protected *right now*, ascending and
    /// non-overlapping. Not the final protections: a writable segment is mapped `Read` and only
    /// becomes writable at the end of the load, so restoring a window to its eventual protection
    /// would leave `.data` read-only and skipping the protect would fault.
    ranges: &'a [LiveRange],
    bindings: &'a [SymbolBinding],
    window: usize,
    page: usize,
    measure_commit: bool,
    open: Option<OpenWindow>,
    pub(crate) stats: RelocationStats,
}

impl<'a> Relocator<'a> {
    pub(crate) fn new(
        space: &'a GuestSpace,
        base: usize,
        ranges: &'a [LiveRange],
        bindings: &'a [SymbolBinding],
        window: usize,
        measure_commit: bool,
    ) -> LoadResult<Self> {
        let page = space.page_size();
        if window == 0 || window % page != 0 {
            return Err(LoadError::InvalidWindow {
                size: window,
                why: "the relocation window must be a non-zero multiple of the page size",
            });
        }
        Ok(Self {
            space,
            base,
            ranges,
            bindings,
            window,
            page,
            measure_commit,
            open: None,
            stats: RelocationStats::default(),
        })
    }

    /// Apply one materialised table, sorting it first so the windows come out disjoint and ascending.
    ///
    /// Used for the plain `DT_RELA` / `DT_REL` / `DT_RELR` / `DT_JMPREL` tables, which the parser
    /// materialises anyway and which are small — the largest in the APK is 22,578 entries.
    pub(crate) fn apply_table(&mut self, table: &mut RelocationTable) -> LoadResult<()> {
        let from_plt = table.tag == "DT_JMPREL";
        let implicit = table.implicit_addend();
        // Stable, so two relocations on one target keep their file order — which is the only case
        // where order could matter, and reordering it would be a silent behaviour change.
        table.relocations.sort_by_key(|r| r.r_offset);
        for r in &table.relocations {
            self.feed(*r, implicit, from_plt)?;
        }
        self.close()
    }

    /// Apply one relocation, opening and closing windows as needed.
    pub(crate) fn feed(&mut self, r: Rela, implicit: bool, from_plt: bool) -> LoadResult<()> {
        self.stats.total += 1;
        if from_plt {
            self.stats.plt += 1;
        } else {
            self.stats.packed += 1;
        }

        let ty = r.r_type();
        let Some(size) = store_size(ty) else {
            // R_AARCH64_NONE is padding and writes nothing. Anything else is refused rather than
            // skipped: a skipped relocation leaves a pointer unrelocated and the crash happens
            // somewhere else entirely.
            if ty == R_AARCH64_NONE {
                self.count(ty);
                self.stats.none += 1;
                return Ok(());
            }
            return Err(unsupported(&r));
        };
        let (target, end) = self.target_range(&r, size)?;

        if !self.open.is_some_and(|w| target >= w.start && end <= w.end) {
            self.close()?;
            self.open_window(&r, target, end)?;
        }
        self.apply_one(&r, target, size, implicit)
    }

    /// Restore the open window's protection, if there is one.
    ///
    /// Called between windows, at the end of every table, and once more on the way out of a failed
    /// load — leaving `.text` writable on an error path would be worse than the error itself.
    pub(crate) fn close(&mut self) -> LoadResult<()> {
        let Some(w) = self.open.take() else { return Ok(()) };
        if let Some(restore) = w.restore {
            self.space.protect(w.start, w.end - w.start, restore)?;
        }
        Ok(())
    }

    fn open_window(&mut self, r: &Rela, target: usize, end: usize) -> LoadResult<()> {
        let range = *self.range_for(target, end, r)?;

        // The window starts at the page holding this target and runs for `window` bytes, clipped to
        // the mapped range — a window may never span two ranges, because restoring protection has to
        // restore each range's own.
        let win_start = target & !(self.page - 1);
        let mut win_end = win_start.saturating_add(self.window).min(range.end);
        if win_end < end {
            // A relocation wider than the window, or one straddling the window's last page. Grow to
            // cover it rather than failing to make progress on a window it cannot fit.
            win_end = end
                .checked_next_multiple_of(self.page)
                .ok_or(LoadError::AddressOverflow {
                    what: "relocation window end",
                    base: self.base,
                    vaddr: r.r_offset,
                })?
                .min(range.end);
        }
        if win_end < end {
            return Err(LoadError::RelocationTargetSpansRanges {
                ty: r.r_type(),
                r_offset: r.r_offset,
                target,
                end,
                range_end: range.end,
            });
        }
        let win_len = win_end - win_start;

        // Anonymous `.bss` is committed lazily when the caller asks for it, so a relocation into it
        // has to pay for its granule; a file view needs a copy-on-write protect instead.
        if range.anonymous {
            self.stats.committed_for_relocation += self.space.ensure_committed(win_start, win_len)?;
        }
        let restore = if range.protection.is_writable() {
            None
        } else {
            self.space.protect(win_start, win_len, Protection::ReadWrite)?;
            Some(range.protection)
        };
        self.open = Some(OpenWindow { start: win_start, end: win_end, restore });

        self.stats.windows += 1;
        self.stats.largest_window = self.stats.largest_window.max(win_len);
        self.stats.windowed_bytes += win_len;
        if self.measure_commit {
            if let Ok(charge) = omni_mem::process_commit_charge() {
                self.stats.peak_commit_charge =
                    Some(self.stats.peak_commit_charge.unwrap_or(0).max(charge));
            }
        }
        Ok(())
    }

    fn count(&mut self, ty: u32) {
        *self.stats.by_type.entry(ty).or_insert(0) += 1;
    }

    /// The biased `[target, end)` a relocation writes.
    fn target_range(&self, r: &Rela, size: usize) -> LoadResult<(usize, usize)> {
        let target = usize::try_from(r.r_offset)
            .ok()
            .and_then(|o| self.base.checked_add(o))
            .ok_or(LoadError::AddressOverflow {
                what: "relocation target",
                base: self.base,
                vaddr: r.r_offset,
            })?;
        let end = target.checked_add(size).ok_or(LoadError::AddressOverflow {
            what: "relocation target end",
            base: self.base,
            vaddr: r.r_offset,
        })?;
        Ok((target, end))
    }

    /// The mapped range `[target, end)` starts in. `end` is allowed to lie past it; the caller
    /// reports that as [`LoadError::RelocationTargetSpansRanges`] once it knows the window.
    fn range_for(&self, target: usize, end: usize, r: &Rela) -> LoadResult<&'a LiveRange> {
        let at = self
            .ranges
            .partition_point(|m| m.start <= target)
            .checked_sub(1)
            .and_then(|i| self.ranges.get(i))
            .filter(|m| target < m.end);
        at.ok_or(LoadError::RelocationTargetUnmapped {
            ty: r.r_type(),
            type_name: type_name(r.r_type()),
            r_offset: r.r_offset,
            target,
            end,
        })
    }

    fn apply_one(&mut self, r: &Rela, target: usize, size: usize, implicit: bool) -> LoadResult<()> {
        let ty = r.r_type();
        self.count(ty);

        // SAFETY: `target` lies inside a mapped range of this space (checked by `range_for`), the
        // whole `[target, target + size)` lies inside the current window (checked by the caller),
        // and the window is protected `ReadWrite` for the duration of this call.
        let ptr = self.space.ptr(target, size)?;

        let addend = if implicit {
            // SAFETY: as above. The target is readable — every protection this loader rests a
            // mapped piece at other than `Protection::None` is readable, and a `Protection::None`
            // piece has been raised to `ReadWrite` by the window.
            unsafe { ptr.cast::<u64>().read_unaligned() as i64 }
        } else {
            r.r_addend
        };

        let value = match ty {
            R_AARCH64_RELATIVE => (self.base as u64).wrapping_add(addend as u64),
            R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT | R_AARCH64_ABS64 => {
                self.stats.symbolic += 1;
                let index = r.r_sym();
                if index == 0 {
                    return Err(LoadError::RelocationWithoutSymbol {
                        ty,
                        type_name: type_name(ty),
                        r_offset: r.r_offset,
                    });
                }
                match self.bindings.get(index as usize) {
                    Some(SymbolBinding::Value(v)) => v.wrapping_add(addend as u64),
                    Some(SymbolBinding::Unresolved) => {
                        // Nothing supplies it. The slot gets a null, which is what bionic does for
                        // a weak undefined symbol and what makes the 565 imports of `libroblox.so`
                        // enumerable without any of them resolving. The import is already recorded
                        // in `LoadedObject::imports`; a strong one is refused earlier when the
                        // policy says so.
                        self.stats.bound_to_null += 1;
                        0
                    }
                    Some(SymbolBinding::Ifunc) => {
                        return Err(LoadError::IfuncUnsupported {
                            name: format!("symbol index {index}"),
                        })
                    }
                    Some(SymbolBinding::Null) | None => {
                        return Err(crate::error::ElfError::SymbolIndexOutOfBounds {
                            index,
                            count: self.bindings.len() as u32,
                        }
                        .into())
                    }
                }
            }
            _ => return Err(unsupported(r)),
        };

        // SAFETY: as above. `write_unaligned` because nothing guarantees a relocation target is
        // eight-byte aligned, and a tampered `r_offset` certainly does not.
        unsafe { ptr.cast::<u64>().write_unaligned(value) };
        self.stats.applied += 1;
        Ok(())
    }
}

/// How many bytes a relocation type stores, or `None` if this loader does not implement it.
fn store_size(ty: u32) -> Option<usize> {
    match ty {
        R_AARCH64_RELATIVE | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT | R_AARCH64_ABS64 => Some(8),
        _ => None,
    }
}

fn type_name(ty: u32) -> &'static str {
    crate::consts::r_aarch64_name(ty).unwrap_or("unknown")
}

fn unsupported(r: &Rela) -> LoadError {
    let ty = r.r_type();
    LoadError::UnsupportedRelocation {
        ty,
        type_name: type_name(ty),
        r_offset: r.r_offset,
        why: match ty {
            R_AARCH64_IRELATIVE => "STT_GNU_IFUNC resolvers need the CPU backend; D9 measured that \
                                    no library in the APK has one",
            R_AARCH64_TLS_DTPREL64
            | R_AARCH64_TLS_DTPMOD64
            | R_AARCH64_TLS_TPREL64
            | R_AARCH64_TLSDESC => "ELF thread-local storage is deliberately not implemented: the \
                                    APK has no PT_TLS and no STT_TLS symbol anywhere (D9, D13)",
            R_AARCH64_COPY => "R_AARCH64_COPY is an executable-only relocation and cannot appear \
                               in a shared object",
            R_AARCH64_ABS32 | R_AARCH64_ABS16 | R_AARCH64_PREL64 | R_AARCH64_PREL32
            | R_AARCH64_PREL16 => "a static relocation type that a dynamic linker never applies",
            _ => "not a dynamic relocation type this loader implements",
        },
    }
}
