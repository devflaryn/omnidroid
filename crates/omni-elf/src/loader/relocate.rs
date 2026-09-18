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
/// | window | windows | relocate wall-time | peak transient charge |
/// |---|---|---|---|
/// | 4 KiB | 1,352 | see the task report | 4 KiB |
/// | **64 KiB** | **86** | see the task report | **64 KiB** |
/// | 1 MiB | 8 | see the task report | 1 MiB |
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

/// The windowed relocation engine.
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
            stats: RelocationStats::default(),
        })
    }

    /// Apply one table. `relocations` is sorted in place by target, which is what makes the windows
    /// disjoint and ascending.
    pub(crate) fn apply_table(&mut self, table: &mut RelocationTable) -> LoadResult<()> {
        let from_plt = table.tag == "DT_JMPREL";
        let implicit = table.implicit_addend();
        // Stable, so two relocations on one target keep their file order — which is the only case
        // where order could matter, and reordering it would be a silent behaviour change.
        table.relocations.sort_by_key(|r| r.r_offset);

        let count = table.relocations.len();
        self.stats.total += count;
        if from_plt {
            self.stats.plt += count;
        } else {
            self.stats.packed += count;
        }

        let mut i = 0usize;
        while i < count {
            let r = table.relocations[i];
            let size = match store_size(r.r_type()) {
                Some(s) => s,
                None => {
                    // Not a store at all: R_AARCH64_NONE is padding. Anything else is refused.
                    if r.r_type() == R_AARCH64_NONE {
                        self.count(r.r_type());
                        self.stats.none += 1;
                        i += 1;
                        continue;
                    }
                    return Err(unsupported(&r));
                }
            };
            let (target, end) = self.target_range(&r, size)?;
            let range = self.range_for(target, end, &r)?;

            // The window starts at the page holding the first pending target and runs for
            // `window` bytes, clipped to the mapped range — a window may never span two ranges,
            // because restoring protection has to restore each range's own.
            let win_start = target & !(self.page - 1);
            let mut win_end = win_start.saturating_add(self.window).min(range.end);
            if win_end < end {
                // A single relocation wider than the window, or one straddling the window's last
                // page. Grow to cover it rather than looping forever on a window it cannot fit.
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

            // Anonymous `.bss` is mapped lazily when the caller asks for it, so a relocation into
            // it has to pay for its granule; a file view needs a copy-on-write protect instead.
            if range.anonymous {
                self.stats.committed_for_relocation +=
                    self.space.ensure_committed(win_start, win_len)?;
            }
            let needs_protect = !range.protection.is_writable();
            if needs_protect {
                self.space.protect(win_start, win_len, Protection::ReadWrite)?;
            }
            self.stats.windows += 1;
            self.stats.largest_window = self.stats.largest_window.max(win_len);
            self.stats.windowed_bytes += win_len;
            if self.measure_commit {
                if let Ok(charge) = omni_platform::vm::process_commit_charge() {
                    self.stats.peak_commit_charge =
                        Some(self.stats.peak_commit_charge.unwrap_or(0).max(charge));
                }
            }

            // Apply every relocation that fits entirely inside the window. The result is carried
            // rather than propagated so that the window's protection is always restored, even on a
            // hostile relocation in the middle of it: leaving `.text` writable on the way out of an
            // error path would be a worse outcome than the error itself.
            let mut outcome = Ok(());
            while i < count {
                let r = table.relocations[i];
                let ty = r.r_type();
                if ty == R_AARCH64_NONE {
                    self.count(ty);
                    self.stats.none += 1;
                    i += 1;
                    continue;
                }
                let Some(size) = store_size(ty) else {
                    outcome = Err(unsupported(&r));
                    break;
                };
                let (target, end) = match self.target_range(&r, size) {
                    Ok(t) => t,
                    Err(e) => {
                        outcome = Err(e);
                        break;
                    }
                };
                if target < win_start || end > win_end {
                    break;
                }
                match self.apply_one(&r, target, size, implicit) {
                    Ok(()) => {}
                    Err(e) => {
                        outcome = Err(e);
                        break;
                    }
                }
                i += 1;
            }

            if needs_protect {
                let restore = self.space.protect(win_start, win_len, range.protection);
                // A failed restore is the more serious of the two failures, so it wins.
                if outcome.is_ok() {
                    restore?;
                } else if let Err(e) = restore {
                    tracing::error!(
                        address = format_args!("{win_start:#x}"),
                        len = win_len,
                        %e,
                        "could not restore a relocation window's protection"
                    );
                }
            }
            outcome?;
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
