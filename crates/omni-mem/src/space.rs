//! The per-instance guest address space.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omni_platform::vm::{self, Protection, Reservation};
use parking_lot::Mutex;

use crate::backing::Backing;
use crate::entry::{Entry, EntryMap, Owner, OsState, ViewId};
use crate::error::{platform, MemError, MemResult};
use crate::region::RegionInfo;

/// A guest virtual address. Identical to the host address it lives at: a guest pointer *is* a host
/// pointer (ARCHITECTURE.md section 1), so there is no translation and no distinct address type.
pub type GuestAddr = usize;

/// Identity of a guest mapping. Stable for the life of the mapping, and reported by
/// [`GuestSpace::regions`] so that consumers can tell one mapping from an adjacent one with the
/// same protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MappingId(pub u64);

/// The default guest address space size: 4 GiB.
///
/// A default, not an assumption. Measured: a 4 GiB reservation costs 0 bytes of commit charge and
/// 0 bytes of working set, an instance holding one costs 37.25 MB of commit charge in total, and
/// about 32,700 of them fit in one 128 TB address space (D10). Nothing in this crate is allowed to
/// assume this number — [`GuestSpaceConfig::size`] is honoured as given.
pub const DEFAULT_SPACE_SIZE: usize = 4 * 1024 * 1024 * 1024;

/// The default commit granule: 64 KiB.
///
/// # Why 64 KiB, measured on this machine
///
/// Committing a granule costs about **2.4 µs** almost independently of how big the granule is,
/// because the cost is two kernel calls — `split_placeholder` ≈ 1.1 µs and `commit_placeholder`
/// ≈ 0.8 µs, measured directly — plus this crate's own bookkeeping, and none of that is per-page
/// work. Measured end to end through [`GuestSpace::ensure_committed`] over a 64 MiB span, release
/// build:
///
/// | granule | per page | per granule |
/// |---|---|---|
/// | 4 KiB | 2414 ns | 2414 ns |
/// | 16 KiB | 596 ns | 2386 ns |
/// | **64 KiB** | **150 ns** | 2401 ns |
/// | 256 KiB | 35 ns | 2238 ns |
/// | 1 MiB | 9 ns | 2300 ns |
///
/// The number to compare against is the **381 ns** measured here for the first touch of a
/// committed page — the kernel soft fault, which no design can avoid (D10 measured 398 ns). At a
/// 4 KiB granule, commit costs 2414 ns/page, *worse* than the 2053 ns/fault of the VEH
/// demand-pager that D10 rejects outright: per-page commit is the same defect wearing a different
/// hat. At 64 KiB, commit costs 150 ns/page, well under the unavoidable fault, so it stops being
/// the bottleneck — while bounding speculative commit at 64 KiB, sixteen times less than the 1 MiB
/// end of D10's range, which matters because commit charge is the scarce resource (D10, Global
/// Constraint 6). Going beyond 64 KiB buys 141 ns/page and costs up to 960 KiB of commit per
/// touched granule, so 64 KiB is where the curve flattens relative to what it spends.
///
/// It also equals the Windows allocation granularity, so a granule never straddles one.
/// [`GuestSpaceConfig::commit_granule`] overrides it for a region known to be densely used.
pub const DEFAULT_COMMIT_GRANULE: usize = 64 * 1024;

/// Whether a mapping's pages are committed up front or on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPolicy {
    /// Commit the whole mapping now. Costs its full size in commit charge immediately — commit
    /// charge is debited at commit, not at first touch (D10) — so this is for mappings that are
    /// about to be written in full, such as a relocation scratch area.
    Eager,
    /// Commit nothing now; [`GuestSpace::ensure_committed`] commits in granules as the guest
    /// reaches each one. This is the default and the reason a guest can hold a large sparse
    /// mapping for nothing.
    Lazy,
}

/// Where a mapping must go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// It must land exactly here, or the call fails. This is what the ELF loader needs, so that
    /// every `PT_LOAD` lands at `base + p_vaddr`.
    ///
    /// Behaves like `mmap(MAP_FIXED_NOREPLACE)`, not `MAP_FIXED`: an occupied range is an error
    /// rather than something to silently unmap.
    Fixed(GuestAddr),
    /// Prefer this address, but take another if it is unavailable. `mmap` with a non-null address
    /// and no `MAP_FIXED`.
    Hint {
        /// The preferred address.
        address: GuestAddr,
        /// Alignment required of the address actually chosen.
        align: usize,
    },
    /// Anywhere with this alignment.
    ///
    /// `align` is honoured as given and nothing here assumes a value for it: every `PT_LOAD` in
    /// `libroblox.so` has `p_align = 0x4000`, which is 16 KiB and not the 4 KiB page size, and
    /// Apple silicon has 16 KiB pages.
    Anywhere {
        /// Alignment required of the chosen address. Must be a power of two.
        align: usize,
    },
}

/// How a guest address space is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestSpaceConfig {
    /// Size of the reservation, in bytes. Rounded up to a page. Costs no commit charge whatever it
    /// is, so this is not the number to economize on (D10).
    pub size: usize,
    /// Alignment of the reservation's base address. A power of two.
    pub base_alignment: usize,
    /// The lazy-commit granule. See [`DEFAULT_COMMIT_GRANULE`] for the measurements behind the
    /// default. Must be a non-zero multiple of the page size.
    pub commit_granule: usize,
}

impl Default for GuestSpaceConfig {
    fn default() -> Self {
        Self {
            size: DEFAULT_SPACE_SIZE,
            base_alignment: vm::allocation_granularity(),
            commit_granule: DEFAULT_COMMIT_GRANULE,
        }
    }
}

/// What [`GuestSpace::reclaim_idle`] gave back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reclaimed {
    /// Bytes of commit charge decommitted. This is the number that matters: it is what the system
    /// commit limit gets back, and it is why `reclaim_idle` uses `MEM_DECOMMIT` — `MEM_RESET` is
    /// the cheapest call available and returns exactly 0 (D10).
    pub bytes: usize,
    /// How many granules were decommitted.
    pub granules: usize,
    /// How many runs of adjacent free placeholders were merged back into single placeholders.
    /// Fragmentation of the placeholder set is what makes a later large mapping fail, so this is
    /// part of reclamation and not a separate concern.
    pub coalesced: usize,
}

/// A snapshot of what a guest address space currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceStats {
    /// Size of the reservation.
    pub reserved: usize,
    /// Bytes claimed by guest mappings, committed or not.
    pub mapped: usize,
    /// Bytes of private committed memory: the commit charge this space is responsible for, page
    /// tables aside.
    ///
    /// Pages of a file-backed view that were privatised by writing through a copy-on-write
    /// protection are **not** counted: the OS creates those on write, not on any call this crate
    /// makes, so counting them would mean guessing. Measure
    /// [`vm::process_commit_charge`](omni_platform::vm::process_commit_charge) when the real number
    /// matters.
    pub committed: usize,
    /// Bytes of committed memory marked idle by [`GuestSpace::advise_idle`], waiting for
    /// [`GuestSpace::reclaim_idle`].
    pub idle: usize,
    /// Bytes mapped from files.
    pub file_backed: usize,
    /// Free bytes.
    pub free: usize,
    /// The largest single free range, which is what decides whether a large mapping can be placed.
    pub largest_free: usize,
    /// Entries in the region map. Grows with fragmentation and is the cost side of
    /// [`GuestSpace::reclaim_idle`]'s coalescing.
    pub entries: usize,
}

/// One instance's guest address space.
///
/// # The model
///
/// One large placeholder reservation, subdivided on demand. Address space is free and commit charge
/// is scarce (D10), so the reservation is generous, nothing inside it is committed until something
/// needs it, and [`unmap`](GuestSpace::unmap) and [`reclaim_idle`](GuestSpace::reclaim_idle) give
/// commit charge back with `MEM_DECOMMIT` while keeping the address space owned by this process.
/// Keeping it owned is not an optimisation: if the range were released, another allocation could be
/// placed at a guest address, and guest addresses *are* host addresses.
///
/// # Thread safety
///
/// `Send + Sync`. The region map is behind one lock, held only for the duration of a single
/// operation. Guest loads and stores do not go through here at all — they are ordinary memory
/// accesses — so this lock is not on the guest's hot path; only guest `mmap`-family calls are.
pub struct GuestSpace {
    base: GuestAddr,
    len: usize,
    page: usize,
    granule: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    map: EntryMap,
    reservation: Reservation,
    page: usize,
    granule: usize,
    cursor: GuestAddr,
    released: bool,
}

static NEXT_MAPPING: AtomicU64 = AtomicU64::new(1);
static NEXT_VIEW: AtomicU64 = AtomicU64::new(1);

impl GuestSpace {
    /// Reserve a guest address space with the default configuration.
    ///
    /// # Errors
    ///
    /// As [`GuestSpace::with_config`].
    pub fn new() -> MemResult<Self> {
        Self::with_config(GuestSpaceConfig::default())
    }

    /// Reserve a guest address space.
    ///
    /// Costs no commit charge: this is a pure `MEM_RESERVE` of a placeholder, measured at 0 bytes
    /// of commit and 0 bytes of working set at sizes up to 97.7 TB (D10).
    ///
    /// # Errors
    ///
    /// [`MemError::InvalidConfig`] for a zero or misaligned size, a granule that is not a non-zero
    /// multiple of the page size, or a base alignment that is not a power of two;
    /// [`MemError::Platform`] if the reservation itself fails.
    pub fn with_config(config: GuestSpaceConfig) -> MemResult<Self> {
        let page = vm::page_size();
        if config.size == 0 {
            return Err(MemError::InvalidConfig {
                field: "size",
                value: 0,
                reason: "must be greater than zero",
            });
        }
        if config.size % page != 0 {
            return Err(MemError::InvalidConfig {
                field: "size",
                value: config.size as u64,
                reason: "must be a multiple of the page size",
            });
        }
        if config.commit_granule == 0 || config.commit_granule % page != 0 {
            return Err(MemError::InvalidConfig {
                field: "commit_granule",
                value: config.commit_granule as u64,
                reason: "must be a non-zero multiple of the page size",
            });
        }
        if config.commit_granule > config.size {
            return Err(MemError::InvalidConfig {
                field: "commit_granule",
                value: config.commit_granule as u64,
                reason: "must not be larger than the guest address space",
            });
        }
        if !config.base_alignment.is_power_of_two() {
            return Err(MemError::InvalidConfig {
                field: "base_alignment",
                value: config.base_alignment as u64,
                reason: "must be a power of two",
            });
        }

        let reservation = vm::reserve_placeholder(config.size, config.base_alignment)
            .map_err(platform("GuestSpace::with_config", 0, config.size))?;
        let base = reservation.base();
        tracing::debug!(
            base = format_args!("{base:#x}"),
            size = config.size,
            granule = config.commit_granule,
            "reserved a guest address space"
        );
        Ok(Self {
            base,
            len: config.size,
            page,
            granule: config.commit_granule,
            inner: Mutex::new(Inner {
                map: EntryMap::new(base, config.size),
                reservation,
                page,
                granule: config.commit_granule,
                cursor: base,
                released: false,
            }),
        })
    }

    /// Base address of the guest address space.
    #[must_use]
    pub fn base(&self) -> GuestAddr {
        self.base
    }

    /// Size of the guest address space in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the space is empty. Always false: a zero-size space is rejected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// End address of the guest address space, exclusive.
    #[must_use]
    pub fn end(&self) -> GuestAddr {
        self.base + self.len
    }

    /// The commit granule in force for mappings made in this space.
    #[must_use]
    pub fn commit_granule(&self) -> usize {
        self.granule
    }

    /// The page size. Not assumed anywhere: 4096 on Windows and Linux, 16384 on Apple silicon.
    #[must_use]
    pub fn page_size(&self) -> usize {
        self.page
    }

    /// Whether `[address, address + len)` is inside this space.
    #[must_use]
    pub fn contains(&self, address: GuestAddr, len: usize) -> bool {
        address >= self.base
            && address <= self.end()
            && len <= self.end() - address
    }

    /// A pointer to a guest address, checked against the space's bounds.
    ///
    /// The pointer is only dereferenceable if the range is mapped and committed; this checks the
    /// space, not the mapping.
    ///
    /// # Errors
    ///
    /// [`MemError::OutsideSpace`] if the range is not inside this space.
    pub fn ptr(&self, address: GuestAddr, len: usize) -> MemResult<*mut u8> {
        self.check_range("ptr", address, len)?;
        Ok(address as *mut u8)
    }

    /// Map anonymous memory.
    ///
    /// With [`CommitPolicy::Lazy`] this costs no commit charge at all: the range is placeholder
    /// address space until [`ensure_committed`](GuestSpace::ensure_committed) covers it. With
    /// [`CommitPolicy::Eager`] the whole range is committed now, in granules, and is charged in
    /// full immediately.
    ///
    /// A mapping with [`Protection::None`] never commits anything, whatever the policy: an
    /// inaccessible page is exactly what an unreplaced placeholder already is, so paying commit
    /// charge for it would be paying for nothing. Raise it with
    /// [`protect`](GuestSpace::protect) and it becomes committable.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`], [`MemError::OutsideSpace`],
    /// [`MemError::AddressTaken`] for an occupied [`Placement::Fixed`], [`MemError::NoSpace`], or
    /// [`MemError::Platform`].
    pub fn map_anonymous(
        &self,
        placement: Placement,
        size: usize,
        protection: Protection,
        commit: CommitPolicy,
    ) -> MemResult<GuestAddr> {
        const OP: &str = "map_anonymous";
        let size = self.round_size(OP, size)?;
        let mut inner = self.inner.lock();
        let address = self.place(&mut inner, OP, placement, size)?;
        inner.make_exact_placeholder(OP, address, size, true)?;

        let owner = Owner {
            id: MappingId(NEXT_MAPPING.fetch_add(1, Ordering::Relaxed)),
            mapping_start: address,
            mapping_len: size,
            protection,
            granule: inner.granule,
            commit,
            backing: None,
            file_offset: 0,
        };
        inner.map.replace(
            address,
            size,
            // Anonymous memory is never a view, so there is no copy-on-write content to lose.
            Entry { len: size, os: OsState::Placeholder, owner: Some(owner), ever_writable: false },
        );

        if commit == CommitPolicy::Eager && protection != Protection::None {
            inner.commit_range(OP, address, size)?;
        }
        inner.validate();
        tracing::debug!(
            address = format_args!("{address:#x}"),
            size,
            %protection,
            ?commit,
            "mapped anonymous guest memory"
        );
        Ok(address)
    }

    /// Map part of a file into the guest address space.
    ///
    /// This is the call the ELF loader uses for every `PT_LOAD`, with [`Placement::Fixed`] at
    /// `base + p_vaddr`. Both the address and `file_offset` are page-granular; the placeholder path
    /// is the only one on Windows that accepts a 4 KB-aligned file offset rather than a
    /// 64 KB-aligned one (D11).
    ///
    /// Read-only and execute-read views cost essentially no commit charge and are shared with every
    /// other instance mapping the same file — that is what makes `libroblox.so`'s ~109 MB of text
    /// shareable (D11). A [`Protection::ReadWrite`] view is copy-on-write and is charged its full
    /// size the moment it is mapped, so map `.text` [`Protection::ReadExecute`] and drop individual
    /// pages to [`Protection::ReadWrite`] for relocation instead of mapping the segment writable.
    ///
    /// [`Protection::None`] is honoured by mapping read-only and then protecting the pages down,
    /// because a view cannot be created with no access.
    ///
    /// # Errors
    ///
    /// As [`map_anonymous`](GuestSpace::map_anonymous), plus [`MemError::Platform`] wrapping
    /// [`VmError::FileNotOpenedExecutable`](omni_platform::vm::VmError::FileNotOpenedExecutable)
    /// when an executable view is asked of a file opened non-executable, or
    /// [`VmError::ViewPastEndOfFile`](omni_platform::vm::VmError::ViewPastEndOfFile).
    pub fn map_file(
        &self,
        backing: &Arc<Backing>,
        file_offset: u64,
        placement: Placement,
        size: usize,
        protection: Protection,
    ) -> MemResult<GuestAddr> {
        const OP: &str = "map_file";
        let size = self.round_size(OP, size)?;
        if file_offset % self.page as u64 != 0 {
            return Err(MemError::Misaligned {
                operation: OP,
                what: "file offset",
                value: file_offset,
                required: self.page as u64,
            });
        }
        let mut inner = self.inner.lock();
        let address = self.place(&mut inner, OP, placement, size)?;
        inner.make_exact_placeholder(OP, address, size, true)?;

        let view = inner.map_view(OP, backing, file_offset, address, size, protection)?;
        let owner = Owner {
            id: MappingId(NEXT_MAPPING.fetch_add(1, Ordering::Relaxed)),
            mapping_start: address,
            mapping_len: size,
            protection,
            granule: inner.granule,
            commit: CommitPolicy::Eager,
            backing: Some(Arc::clone(backing)),
            file_offset,
        };
        inner.map.replace(
            address,
            size,
            Entry {
                len: size,
                os: OsState::View { view },
                owner: Some(owner),
                // A ReadWrite view is PAGE_WRITECOPY, so it can hold privatised content from its
                // first write onwards.
                ever_writable: protection.is_writable(),
            },
        );
        inner.validate();
        tracing::debug!(
            address = format_args!("{address:#x}"),
            size,
            file_offset,
            %protection,
            backing = %backing.name(),
            "mapped a file into guest memory"
        );
        Ok(address)
    }

    /// Commit the granules covering `[address, address + len)` that are not committed yet.
    ///
    /// The lazy-commit driver. The request is expanded outwards to whole commit granules — measured
    /// at 124 ns/page against 1810 ns/page for per-page commit and 381 ns for the unavoidable first
    /// touch (see [`DEFAULT_COMMIT_GRANULE`]) — and clipped to the mapping, so committing one byte
    /// of a mapping commits one granule of it and no more.
    ///
    /// Ranges that are already committed, are file-backed, hold a [`Protection::None`] mapping, or
    /// are free address space are skipped rather than rejected, so a caller serving a guest fault
    /// does not have to know which of those it hit.
    ///
    /// Returns the number of bytes newly committed, which is what the commit charge went up by.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::OutsideSpace`], or [`MemError::Platform`] —
    /// `ERROR_COMMITMENT_LIMIT` (1455) when the system commit limit is reached.
    pub fn ensure_committed(&self, address: GuestAddr, len: usize) -> MemResult<usize> {
        const OP: &str = "ensure_committed";
        if len == 0 {
            return Err(MemError::ZeroSize { operation: OP });
        }
        self.check_range(OP, address, len)?;
        let mut inner = self.inner.lock();
        let committed = inner.commit_range(OP, address, len)?;
        inner.validate();
        Ok(committed)
    }

    /// Change the protection of a mapped range.
    ///
    /// Page-granular, and the range must be mapped end to end. Uncommitted granules of a lazy
    /// mapping record the new protection and are committed with it later, so raising a
    /// [`Protection::None`] mapping to [`Protection::ReadWrite`] does not commit anything by
    /// itself.
    ///
    /// On a file-backed view, [`Protection::ReadWrite`] means copy-on-write: writes privatise the
    /// pages they touch and never reach the file. That is the sequence the ELF loader needs for
    /// relocations — map `ReadExecute`, drop to `ReadWrite`, write, raise back to `ReadExecute` —
    /// and mapping read-only first and hoping to promote does **not** work (D11 correction).
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`], [`MemError::OutsideSpace`],
    /// [`MemError::NotMapped`] if any part of the range is free, or [`MemError::Platform`] —
    /// `ERROR_INVALID_PARAMETER` (87) when the protection exceeds what the backing file's section
    /// allows.
    pub fn protect(
        &self,
        address: GuestAddr,
        len: usize,
        protection: Protection,
    ) -> MemResult<()> {
        const OP: &str = "protect";
        let len = self.round_size(OP, len)?;
        self.check_aligned(OP, "address", address)?;
        self.check_range(OP, address, len)?;
        let mut inner = self.inner.lock();
        inner.require_mapped(OP, address, len)?;
        inner.protect_range(OP, address, len, protection)?;
        inner.validate();
        Ok(())
    }

    /// Unmap a range, returning its commit charge and keeping the address space reserved.
    ///
    /// The guest's `munmap`. Committed granules are decommitted, which is the only primitive
    /// measured to return commit charge — 256.50 MB back for a 256 MB range, against exactly 0.00
    /// for `MEM_RESET`, `DiscardVirtualMemory`, `OfferVirtualMemory` and `EmptyWorkingSet` (D10).
    /// The range becomes free placeholder address space that this process still owns; the outer
    /// reservation is never released, because a guest address that the OS considered free could be
    /// handed to something else, and guest addresses are host addresses.
    ///
    /// Unmapping a range that is partly or wholly free is not an error, matching `munmap`.
    ///
    /// # Partial unmap of a file-backed view
    ///
    /// Windows cannot partially unmap a view — `UnmapViewOfFile2` takes no length and unmaps from
    /// the view's base — so this **emulates** it: the whole view is unmapped and the surviving head
    /// and tail are re-mapped from the same file at the same addresses. Unmapping a view's head,
    /// its tail, or a hole through its middle all work, and the pieces keep their own protections
    /// because each surviving piece becomes a view in its own right.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`], [`MemError::OutsideSpace`], or
    /// [`MemError::Platform`].
    pub fn unmap(&self, address: GuestAddr, len: usize) -> MemResult<()> {
        const OP: &str = "unmap";
        let len = self.round_size(OP, len)?;
        self.check_aligned(OP, "address", address)?;
        self.check_range(OP, address, len)?;
        let mut inner = self.inner.lock();
        inner.unmap_range(OP, address, len)?;
        inner.validate();
        tracing::debug!(address = format_args!("{address:#x}"), len, "unmapped guest memory");
        Ok(())
    }

    /// Mark a committed range idle: the guest no longer needs its contents.
    ///
    /// The deferred half of `MADV_DONTNEED`. Nothing is decommitted and the contents survive until
    /// [`reclaim_idle`](GuestSpace::reclaim_idle) runs, so this is cheap and takes no kernel call.
    /// Page-granular. Returns the number of bytes marked.
    ///
    /// Use [`unmap`](GuestSpace::unmap) for the immediate semantics.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`], or [`MemError::OutsideSpace`].
    pub fn advise_idle(&self, address: GuestAddr, len: usize) -> MemResult<usize> {
        const OP: &str = "advise_idle";
        let len = self.round_size(OP, len)?;
        self.check_aligned(OP, "address", address)?;
        self.check_range(OP, address, len)?;
        let mut inner = self.inner.lock();
        let marked = inner.mark_idle(address, len);
        inner.validate();
        Ok(marked)
    }

    /// Decommit everything the guest has released, and defragment the placeholder set.
    ///
    /// Two jobs, both of which return a scarce resource:
    ///
    /// * Every granule marked by [`advise_idle`](GuestSpace::advise_idle) is decommitted with
    ///   `MEM_DECOMMIT`, which is the **only** primitive measured to give commit charge back.
    ///   `MEM_RESET` is the cheapest call available at 32.5 ns/page and returns exactly 0.00 MB
    ///   (D10), so it would pass any functional test while reclaiming nothing; it is not used here
    ///   and is not reachable through `omni-platform` at all.
    /// * Runs of adjacent free placeholders are merged back into single placeholders. Splitting is
    ///   one-way at the OS level, and a mapping request that spans two separately-freed ranges
    ///   fails with `ERROR_INVALID_PARAMETER` (87) until they are merged, so this is what keeps a
    ///   long-lived instance able to place large mappings. Measured at about 270 ns per merged
    ///   piece.
    ///
    /// # Errors
    ///
    /// [`MemError::Platform`] if a decommit or a coalesce fails.
    pub fn reclaim_idle(&self) -> MemResult<Reclaimed> {
        let mut inner = self.inner.lock();
        let reclaimed = inner.reclaim()?;
        inner.validate();
        tracing::debug!(
            bytes = reclaimed.bytes,
            granules = reclaimed.granules,
            coalesced = reclaimed.coalesced,
            "reclaimed idle guest memory"
        );
        Ok(reclaimed)
    }

    /// Every region of the space, free ranges included, in address order.
    ///
    /// Adjacent entries that the guest cannot tell apart — same mapping, same protection, same
    /// backing, contiguous file offsets — are merged, so this is shaped for `/proc/self/maps`
    /// synthesis and for the loaded-object bookkeeping `dl_iterate_phdr` needs: one element per
    /// line the guest would expect to read.
    #[must_use]
    pub fn regions(&self) -> Vec<RegionInfo> {
        self.inner.lock().regions(true)
    }

    /// Every *mapped* region, in address order: [`regions`](GuestSpace::regions) without the free
    /// ranges. This is the `/proc/self/maps` shape.
    #[must_use]
    pub fn mapped_regions(&self) -> Vec<RegionInfo> {
        self.inner.lock().regions(false)
    }

    /// The region containing an address, if it is mapped.
    #[must_use]
    pub fn region_at(&self, address: GuestAddr) -> Option<RegionInfo> {
        let inner = self.inner.lock();
        let start = inner.map.entry_start(address)?;
        let entry = inner.map.get(start)?;
        if entry.is_free() {
            return None;
        }
        Some(RegionInfo::from_entry(start, entry))
    }

    /// What the space currently holds.
    #[must_use]
    pub fn stats(&self) -> SpaceStats {
        self.inner.lock().stats()
    }

    /// Tear the space down, reporting any failure.
    ///
    /// Unmaps every view, decommits every committed granule, and releases the reservation. This is
    /// what [`Drop`] does too; the difference is that this returns the error instead of logging it,
    /// which is what a test asserting that teardown returns *all* the commit charge needs.
    ///
    /// # Errors
    ///
    /// [`MemError::Platform`] if any teardown step fails. The space is left as consistent as
    /// possible and the remaining steps are still attempted.
    pub fn close(self) -> MemResult<()> {
        let mut inner = self.inner.lock();
        inner.release_all()
    }

    // -------------------------------------------------------------------------------------------

    fn round_size(&self, operation: &'static str, size: usize) -> MemResult<usize> {
        if size == 0 {
            return Err(MemError::ZeroSize { operation });
        }
        let page = self.page;
        let rounded = size
            .checked_add(page - 1)
            .map(|s| s & !(page - 1))
            .ok_or(MemError::OutsideSpace {
                operation,
                address: 0,
                end: usize::MAX,
                space_base: self.base,
                space_end: self.end(),
                space_len: self.len,
            })?;
        Ok(rounded)
    }

    fn check_aligned(
        &self,
        operation: &'static str,
        what: &'static str,
        value: GuestAddr,
    ) -> MemResult<()> {
        if value % self.page != 0 {
            return Err(MemError::Misaligned {
                operation,
                what,
                value: value as u64,
                required: self.page as u64,
            });
        }
        Ok(())
    }

    fn check_range(
        &self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
    ) -> MemResult<()> {
        if !self.contains(address, len) {
            return Err(MemError::OutsideSpace {
                operation,
                address,
                end: address.saturating_add(len),
                space_base: self.base,
                space_end: self.end(),
                space_len: self.len,
            });
        }
        Ok(())
    }

    /// Resolve a [`Placement`] to an address whose range is entirely free.
    fn place(
        &self,
        inner: &mut Inner,
        operation: &'static str,
        placement: Placement,
        size: usize,
    ) -> MemResult<GuestAddr> {
        match placement {
            Placement::Fixed(address) => {
                self.check_aligned(operation, "fixed address", address)?;
                self.check_range(operation, address, size)?;
                inner.require_free(operation, address, size)?;
                Ok(address)
            }
            Placement::Hint { address, align } => {
                self.check_align_argument(operation, align)?;
                if address % align == 0
                    && address % self.page == 0
                    && self.contains(address, size)
                    && inner.require_free(operation, address, size).is_ok()
                {
                    return Ok(address);
                }
                inner.find_free(operation, size, align.max(self.page), address)
            }
            Placement::Anywhere { align } => {
                self.check_align_argument(operation, align)?;
                inner.find_free(operation, size, align.max(self.page), inner.cursor)
            }
        }
    }

    fn check_align_argument(&self, operation: &'static str, align: usize) -> MemResult<()> {
        if align == 0 || !align.is_power_of_two() {
            return Err(MemError::Misaligned {
                operation,
                what: "alignment",
                value: align as u64,
                required: 2,
            });
        }
        Ok(())
    }
}

impl core::fmt::Debug for GuestSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GuestSpace")
            .field("base", &format_args!("{:#x}", self.base))
            .field("len", &self.len)
            .field("commit_granule", &self.granule)
            .finish()
    }
}

impl Drop for GuestSpace {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        if let Err(error) = inner.release_all() {
            // Teardown failing means address space or commit charge has leaked for the life of the
            // process, which is exactly the kind of thing that must not be silent.
            tracing::error!(%error, base = format_args!("{:#x}", self.base), "guest address space teardown failed");
        }
    }
}

// -------------------------------------------------------------------------------------------
// The OS-touching half. Every placeholder boundary change is paired with its kernel call here.
// -------------------------------------------------------------------------------------------

impl Inner {
    #[inline]
    fn validate(&self) {
        #[cfg(debug_assertions)]
        self.map.check_invariants();
    }

    fn require_free(
        &self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
    ) -> MemResult<()> {
        let end = address + len;
        for start in self.map.starts_overlapping(address, len) {
            let entry = self.map.get(start).expect("entry vanished");
            if !entry.is_free() {
                return Err(MemError::AddressTaken {
                    operation,
                    requested: address,
                    requested_end: end,
                    conflict_start: start,
                    conflict_end: start + entry.len,
                    conflict: entry.describe(),
                });
            }
        }
        Ok(())
    }

    fn require_mapped(
        &self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
    ) -> MemResult<()> {
        for start in self.map.starts_overlapping(address, len) {
            let entry = self.map.get(start).expect("entry vanished");
            if entry.is_free() {
                return Err(MemError::NotMapped {
                    operation,
                    address,
                    end: address + len,
                    unmapped_start: start,
                    unmapped_end: start + entry.len,
                });
            }
        }
        Ok(())
    }

    /// First-fit search from `from`, wrapping once.
    ///
    /// Lookup by address is `O(log n)`; this search is linear in the number of *free runs*, which
    /// is a different and much smaller quantity, and it is only paid when the caller did not name
    /// an address. A size-indexed free list would make it logarithmic, and is not worth its
    /// invalidation rules until a profile says the guest's `mmap` rate needs it.
    fn find_free(
        &mut self,
        operation: &'static str,
        len: usize,
        align: usize,
        from: GuestAddr,
    ) -> MemResult<GuestAddr> {
        let mut runs: Vec<(GuestAddr, usize)> = Vec::new();
        for (start, entry) in self.map.iter() {
            if entry.is_free() {
                match runs.last_mut() {
                    Some((run_start, run_len)) if *run_start + *run_len == start => {
                        *run_len += entry.len;
                    }
                    _ => runs.push((start, entry.len)),
                }
            }
        }

        let fits = |run_start: GuestAddr, run_len: usize, lower: GuestAddr| -> Option<GuestAddr> {
            let run_end = run_start + run_len;
            let from = run_start.max(lower);
            let candidate = (from + align - 1) & !(align - 1);
            if candidate >= run_start && candidate < run_end && run_end - candidate >= len {
                Some(candidate)
            } else {
                None
            }
        };

        for &(run_start, run_len) in &runs {
            if run_start + run_len <= from {
                continue;
            }
            if let Some(address) = fits(run_start, run_len, from) {
                self.cursor = address + len;
                return Ok(address);
            }
        }
        for &(run_start, run_len) in &runs {
            if let Some(address) = fits(run_start, run_len, self.map.base()) {
                self.cursor = address + len;
                return Ok(address);
            }
        }

        let (free, largest) = self.map.free_summary();
        Err(MemError::NoSpace { operation, len, align, free, largest })
    }

    /// Make `[address, address + len)` be exactly one placeholder entry.
    ///
    /// This is where the three measured placeholder rules are obeyed. The range must already be
    /// covered by placeholder entries — free ones when `require_free`, otherwise entries of a
    /// single mapping — and afterwards there is one entry covering exactly the range, with the
    /// remainders on either side left as entries of their own.
    fn make_exact_placeholder(
        &mut self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
        require_free: bool,
    ) -> MemResult<()> {
        let end = address + len;
        let starts = self.map.starts_overlapping(address, len);
        assert!(!starts.is_empty(), "{operation}: no entries cover {address:#x}..{end:#x}");

        for &start in &starts {
            let entry = self.map.get(start).expect("entry vanished");
            assert_eq!(
                entry.os,
                OsState::Placeholder,
                "{operation}: {start:#x} is {} and cannot be carved",
                entry.os.describe()
            );
            if require_free {
                debug_assert!(entry.is_free(), "{operation}: {start:#x} is not free");
            }
        }

        let union_start = starts[0];
        let last = *starts.last().expect("checked non-empty");
        let union_end = last + self.map.get(last).expect("entry vanished").len;

        if starts.len() > 1 {
            // Several adjacent placeholders. Merging them is the only way to then split the exact
            // range out: a split that spans two placeholders fails with 87, and a coalesce of a
            // range holding one placeholder fails with 487 — so this is conditional on the count.
            let union_len = union_end - union_start;
            // SAFETY: every entry in the run is a placeholder this process owns, as asserted above,
            // and the map is the authority on that. Nothing is dereferenced; the call only merges
            // placeholder boundaries.
            unsafe { vm::coalesce_placeholders(union_start as *mut u8, union_len) }
                .map_err(platform(operation, union_start, union_len))?;
            let merged = self.map.get(union_start).expect("entry vanished").clone();
            self.map.replace(
                union_start,
                union_len,
                Entry {
                    len: union_len,
                    os: OsState::Placeholder,
                    owner: merged.owner,
                    ever_writable: false,
                },
            );
        }

        if union_start < address {
            self.split_placeholder(operation, union_start, address)?;
        }
        if union_end > end {
            self.split_placeholder(operation, address, end)?;
        }
        Ok(())
    }

    /// Split the placeholder entry starting at `start` at `at`, in the OS and in the map.
    fn split_placeholder(
        &mut self,
        operation: &'static str,
        start: GuestAddr,
        at: GuestAddr,
    ) -> MemResult<()> {
        debug_assert!(at > start);
        let len = at - start;
        let offset = start - self.map.base();
        let piece = vm::split_placeholder(&self.reservation, offset, len)
            .map_err(platform(operation, start, len))?;
        debug_assert_eq!(piece.base(), start, "a split must carve the range it was given");
        self.map.split_bookkeeping(start, at);
        Ok(())
    }

    /// Make sure an entry boundary exists at `at`, splitting the OS placeholder if the entry is one.
    fn ensure_boundary(&mut self, operation: &'static str, at: GuestAddr) -> MemResult<()> {
        if at == self.map.end() {
            return Ok(());
        }
        let start = match self.map.entry_start(at) {
            Some(start) => start,
            None => return Ok(()),
        };
        if start == at {
            return Ok(());
        }
        let entry = self.map.get(start).expect("entry vanished");
        if entry.os == OsState::Placeholder {
            self.split_placeholder(operation, start, at)?;
        } else {
            // Private memory and views may be carved in bookkeeping alone: a partial
            // `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER` of a private region is legal and leaves its
            // neighbours' contents intact (measured), and a view is allowed to span entries so that
            // parts of it can carry different protections.
            self.map.split_bookkeeping(start, at);
        }
        Ok(())
    }

    /// Commit the granules covering `[address, address + len)` that are not committed yet.
    fn commit_range(
        &mut self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
    ) -> MemResult<usize> {
        let end = address + len;
        let mut committed = 0;
        let mut position = address;

        while position < end {
            let Some(start) = self.map.entry_start(position) else { break };
            let entry = self.map.get(start).expect("entry vanished");
            let entry_end = start + entry.len;
            let Some(owner) = entry.owner.clone() else {
                position = entry_end;
                continue;
            };
            if entry.os != OsState::Placeholder || owner.protection == Protection::None {
                position = entry_end;
                continue;
            }

            // Expand to whole granules, counted from the mapping's start so that the boundaries do
            // not depend on how the mapping has since been carved up, then clip to this entry —
            // the entry is exactly one OS placeholder and a commit may not cross one.
            //
            // An eager mapping is granule-free by definition: the caller has said the whole thing
            // will be used, so committing it in one call rather than in `mapping_len / granule`
            // calls saves about 1.9 µs per granule and changes nothing about what is charged.
            let granule = match owner.commit {
                CommitPolicy::Eager => owner.mapping_len,
                CommitPolicy::Lazy => owner.granule,
            };
            let relative = position - owner.mapping_start;
            let granule_start = owner.mapping_start + (relative - relative % granule);
            let relative_end = end.min(entry_end) - owner.mapping_start;
            let granule_end = owner.mapping_start
                + relative_end.div_ceil(granule) * granule;
            let from = granule_start.max(start);
            let to = granule_end.min(entry_end);

            self.make_exact_placeholder(operation, from, to - from, false)?;
            // SAFETY: `[from, to)` is now exactly one unreplaced placeholder piece, which is the
            // contract of `commit_placeholder`. It is inside this process's reservation and no
            // reference into it exists: nothing has been able to touch it, because a placeholder is
            // inaccessible.
            unsafe { vm::commit_placeholder(from as *mut u8, to - from, owner.protection) }
                .map_err(platform(operation, from, to - from))?;
            self.map
                .get_mut(from)
                .expect("entry vanished")
                .os = OsState::Private { idle: false };
            committed += to - from;
            position = to;
        }
        Ok(committed)
    }

    fn map_view(
        &mut self,
        operation: &'static str,
        backing: &Arc<Backing>,
        file_offset: u64,
        address: GuestAddr,
        len: usize,
        protection: Protection,
    ) -> MemResult<ViewId> {
        // A view cannot be created with no access, so PROT_NONE is honoured in two steps.
        let create_with =
            if protection == Protection::None { Protection::Read } else { protection };
        // SAFETY: `[address, address + len)` is exactly one unreplaced placeholder piece, which is
        // `map_file`'s contract — `make_exact_placeholder` has just made it so, and the region map
        // is the authority on placeholder extents.
        unsafe { vm::map_file(backing.file(), file_offset, len, address as *mut u8, create_with) }
            .map_err(platform(operation, address, len))?;
        if protection == Protection::None {
            // SAFETY: the range is a live view this process owns, just created above.
            unsafe { vm::protect(address as *mut u8, len, Protection::None) }
                .map_err(platform(operation, address, len))?;
        }
        Ok(ViewId(NEXT_VIEW.fetch_add(1, Ordering::Relaxed)))
    }

    fn protect_range(
        &mut self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
        protection: Protection,
    ) -> MemResult<()> {
        let end = address + len;
        self.ensure_boundary(operation, address)?;
        self.ensure_boundary(operation, end)?;

        for start in self.map.starts_overlapping(address, len) {
            let entry = self.map.get(start).expect("entry vanished");
            let entry_len = entry.len;
            debug_assert!(start >= address && start + entry_len <= end);
            match entry.os {
                // An uncommitted granule has no pages to protect; it records the protection and is
                // committed with it when something reaches it.
                OsState::Placeholder => {}
                OsState::Private { .. } | OsState::View { .. } => {
                    // SAFETY: the range is committed private memory or a live view this process
                    // owns, and it is homogeneous — one entry is one OS state, so this never spans
                    // both, which `protect` requires.
                    unsafe { vm::protect(start as *mut u8, entry_len, protection) }
                        .map_err(platform(operation, start, entry_len))?;
                }
            }
            let entry = self.map.get_mut(start).expect("entry vanished");
            if protection.is_writable() && matches!(entry.os, OsState::View { .. }) {
                // From here on, this range may hold copy-on-write content that is not in the file,
                // and `unmap` has to preserve it across the re-map a partial unmap requires. Sticky:
                // lowering the protection again does not un-privatise a page that was written.
                entry.ever_writable = true;
            }
            if let Some(owner) = entry.owner.as_mut() {
                owner.protection = protection;
            }
        }
        Ok(())
    }

    fn unmap_range(
        &mut self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
    ) -> MemResult<()> {
        let end = address + len;
        self.ensure_boundary(operation, address)?;
        self.ensure_boundary(operation, end)?;

        let mut position = address;
        while position < end {
            let Some(start) = self.map.entry_start(position) else { break };
            let entry = self.map.get(start).expect("entry vanished").clone();
            let entry_end = start + entry.len;
            if entry.is_free() {
                position = entry_end;
                continue;
            }
            match entry.os {
                OsState::Placeholder => {
                    // Reserved but never committed: nothing to give back, just disown it.
                    self.map.free_range(start, entry.len);
                    position = entry_end;
                }
                OsState::Private { .. } => {
                    // SAFETY: the range is private committed memory this process owns, produced by
                    // `commit_placeholder`, and the guest has asked for it to be gone, so nothing
                    // may hold a reference into it. A partial release of a private region is legal
                    // and was measured to leave its neighbours' contents intact.
                    unsafe { vm::decommit_to_placeholder(start as *mut u8, entry.len) }
                        .map_err(platform(operation, start, entry.len))?;
                    self.map.free_range(start, entry.len);
                    position = entry_end;
                }
                OsState::View { view } => {
                    position = self.unmap_view(operation, view, start, address, end)?;
                }
            }
        }
        Ok(())
    }

    /// Unmap the part of a view that falls inside `[keep_out_start, keep_out_end)`, emulating a
    /// partial unmap.
    ///
    /// Windows unmaps a whole view from its base and cannot do less, so the whole view goes and the
    /// surviving pieces are mapped again from the same file at the same addresses. Returns the
    /// address to continue the unmap walk from.
    ///
    /// # Copy-on-write content has to be carried across the re-map
    ///
    /// A survivor that is mapped again comes back **from the file**, so anything written into it
    /// through a copy-on-write protection — every relocation the ELF loader applies, and every guest
    /// write to a private file mapping — would be silently lost. So before the view is destroyed,
    /// each survivor that has ever been writable is compared against a pristine view of the same file
    /// bytes, and the pages that differ are copied out; after the re-map they are written back, which
    /// privatises exactly those pages again and no others.
    ///
    /// Two things make that affordable. The `ever_writable` flag skips the comparison entirely for
    /// views that cannot hold privatised content, which is nearly all of them. And comparing rather
    /// than copying wholesale is what keeps file-backed sharing intact: copying a whole survivor back
    /// would privatise pages that are currently clean and shared, and D11's multi-instance argument
    /// rests on the guest library's text staying shared at near-zero commit charge.
    ///
    /// The comparison runs **before** anything is unmapped, so a failure there leaves the view
    /// untouched rather than half destroyed.
    fn unmap_view(
        &mut self,
        operation: &'static str,
        view: ViewId,
        touched: GuestAddr,
        keep_out_start: GuestAddr,
        keep_out_end: GuestAddr,
    ) -> MemResult<GuestAddr> {
        // The view's real extent: every entry sharing the view id, which is contiguous by
        // construction. This is also what `VmError::NotViewBase` and `ViewSizeMismatch` report, and
        // it is the number that makes the emulation possible.
        let mut view_start = touched;
        let mut view_end = touched + self.map.get(touched).expect("entry vanished").len;
        let pieces: Vec<(GuestAddr, usize, Owner, bool)> = {
            let mut pieces = Vec::new();
            for (start, entry) in self.map.iter() {
                if entry.os == (OsState::View { view }) {
                    let owner = entry.owner.clone().expect("a view always has an owner");
                    view_start = view_start.min(start);
                    view_end = view_end.max(start + entry.len);
                    pieces.push((start, entry.len, owner, entry.ever_writable));
                }
            }
            pieces
        };
        let view_len = view_end - view_start;

        // What survives the request, piece by piece. Each survivor becomes a view of its own, which
        // is what keeps a view that had been protected in parts from losing those protections.
        let mut survivors: Vec<Survivor> = Vec::new();
        for (start, len, owner, ever_writable) in pieces {
            for (piece_start, piece_len) in subtract(start, len, keep_out_start, keep_out_end) {
                survivors.push(Survivor {
                    start: piece_start,
                    len: piece_len,
                    owner: owner.clone(),
                    ever_writable,
                    preserved: Vec::new(),
                });
            }
        }

        // Before anything is destroyed: find the bytes that exist only in copy-on-write pages.
        for survivor in &mut survivors {
            if !survivor.ever_writable {
                continue;
            }
            if !survivor.owner.protection.is_readable() {
                // The comparison has to read the live pages. Raising a PROT_NONE survivor loses
                // nothing — it is about to be unmapped either way, and it is mapped again with its
                // own protection below.
                // SAFETY: the range is a live view this process owns.
                unsafe { vm::protect(survivor.start as *mut u8, survivor.len, Protection::Read) }
                    .map_err(platform(operation, survivor.start, survivor.len))?;
            }
            survivor.preserved = scan_for_copy_on_write(
                operation,
                survivor.start,
                survivor.len,
                &survivor.owner,
                self.page,
            )?;
            let bytes: usize = survivor.preserved.iter().map(|run| run.bytes.len()).sum();
            if bytes > 0 {
                tracing::debug!(
                    address = format_args!("{:#x}", survivor.start),
                    len = survivor.len,
                    runs = survivor.preserved.len(),
                    bytes,
                    "preserving copy-on-write content across a partial unmap"
                );
            }
        }

        // SAFETY: `[view_start, view_len)` is exactly one whole view produced by `map_file` — the
        // region map tracks which entries belong to which view — and the guest has asked for part
        // of it to be gone, so nothing may hold a reference into it. Unmapping preserves the
        // placeholder, so the address space stays owned by this process and no other allocation can
        // land at a guest address.
        unsafe { vm::unmap(view_start as *mut u8, view_len) }
            .map_err(platform(operation, view_start, view_len))?;
        self.map.replace(view_start, view_len, Entry::free(view_len));

        for survivor in &survivors {
            self.make_exact_placeholder(operation, survivor.start, survivor.len, true)?;
            let backing = survivor.owner.backing.clone().expect("a view always has a backing");
            let file_offset = survivor.owner.offset_at(survivor.start);
            let view = self.map_view(
                operation,
                &backing,
                file_offset,
                survivor.start,
                survivor.len,
                survivor.owner.protection,
            )?;
            self.map.replace(
                survivor.start,
                survivor.len,
                Entry {
                    len: survivor.len,
                    os: OsState::View { view },
                    owner: Some(survivor.owner.clone()),
                    ever_writable: survivor.ever_writable,
                },
            );
            restore_copy_on_write(operation, &survivor.preserved, survivor.owner.protection)?;
        }
        // Everything of this view that fell inside the request is free now, so the unmap walk
        // continues past it. The view may have extended beyond the request in either direction;
        // those parts have been mapped again and must not be revisited.
        Ok(view_end.min(keep_out_end))
    }

    fn mark_idle(&mut self, address: GuestAddr, len: usize) -> usize {
        // Boundary changes here are bookkeeping on private memory only, so they cannot fail; a
        // placeholder is skipped rather than split, because there is nothing committed to mark.
        let _ = self.ensure_boundary("advise_idle", address);
        let _ = self.ensure_boundary("advise_idle", address + len);
        let mut marked = 0;
        for start in self.map.starts_overlapping(address, len) {
            let entry = self.map.get_mut(start).expect("entry vanished");
            if start < address || start + entry.len > address + len {
                continue;
            }
            if let OsState::Private { idle } = &mut entry.os {
                if !*idle {
                    *idle = true;
                    marked += entry.len;
                }
            }
        }
        marked
    }

    fn reclaim(&mut self) -> MemResult<Reclaimed> {
        let mut reclaimed = Reclaimed::default();

        let idle: Vec<(GuestAddr, usize)> = self
            .map
            .iter()
            .filter(|(_, entry)| matches!(entry.os, OsState::Private { idle: true }))
            .map(|(start, entry)| (start, entry.len))
            .collect();

        for (start, len) in idle {
            // SAFETY: the range is private committed memory this process owns and the guest has
            // said it no longer needs the contents, so nothing may hold a reference into it.
            // `MEM_DECOMMIT` is the only primitive that returns commit charge (D10).
            unsafe { vm::decommit_to_placeholder(start as *mut u8, len) }
                .map_err(platform("reclaim_idle", start, len))?;
            let entry = self.map.get_mut(start).expect("entry vanished");
            entry.os = OsState::Placeholder;
            reclaimed.bytes += len;
            reclaimed.granules += 1;
        }

        // Defragment: merge every run of adjacent free placeholders into one placeholder.
        let mut runs: Vec<(GuestAddr, usize, usize)> = Vec::new();
        for (start, entry) in self.map.iter() {
            let mergeable = entry.is_free() && entry.os == OsState::Placeholder;
            match runs.last_mut() {
                Some((run_start, run_len, count)) if mergeable && *run_start + *run_len == start => {
                    *run_len += entry.len;
                    *count += 1;
                }
                _ if mergeable => runs.push((start, entry.len, 1)),
                _ => {}
            }
        }
        for (start, len, count) in runs {
            if count < 2 {
                continue;
            }
            // SAFETY: every entry in the run is a free placeholder this process owns.
            unsafe { vm::coalesce_placeholders(start as *mut u8, len) }
                .map_err(platform("reclaim_idle", start, len))?;
            self.map.replace(start, len, Entry::free(len));
            reclaimed.coalesced += count;
        }
        Ok(reclaimed)
    }

    fn regions(&self, include_free: bool) -> Vec<RegionInfo> {
        let mut out: Vec<RegionInfo> = Vec::new();
        for (start, entry) in self.map.iter() {
            if entry.is_free() && !include_free {
                continue;
            }
            let info = RegionInfo::from_entry(start, entry);
            match out.last_mut() {
                Some(previous) if previous.can_absorb(&info) => previous.absorb(&info),
                _ => out.push(info),
            }
        }
        out
    }

    fn stats(&self) -> SpaceStats {
        let mut stats = SpaceStats {
            reserved: self.map.end() - self.map.base(),
            mapped: 0,
            committed: 0,
            idle: 0,
            file_backed: 0,
            free: 0,
            largest_free: 0,
            entries: self.map.entry_count(),
        };
        for (_, entry) in self.map.iter() {
            if entry.is_free() {
                continue;
            }
            stats.mapped += entry.len;
            stats.committed += entry.committed_bytes();
            if matches!(entry.os, OsState::Private { idle: true }) {
                stats.idle += entry.len;
            }
            if matches!(entry.os, OsState::View { .. }) {
                stats.file_backed += entry.len;
            }
        }
        let (free, largest) = self.map.free_summary();
        stats.free = free;
        stats.largest_free = largest;
        stats
    }

    /// Unmap everything, then give the whole reservation back in one release.
    fn release_all(&mut self) -> MemResult<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;

        let mut first_error: Option<MemError> = None;
        let mut fail = |error: MemError| {
            if first_error.is_none() {
                first_error = Some(error);
            }
        };

        // 1. Turn everything back into placeholders: views unmapped whole, private memory
        //    decommitted. Every step is attempted even if an earlier one failed, because stopping
        //    early would leak the rest.
        let mut position = self.map.base();
        while position < self.map.end() {
            let Some(start) = self.map.entry_start(position) else { break };
            let entry = self.map.get(start).expect("entry vanished").clone();
            let entry_end = start + entry.len;
            match entry.os {
                OsState::Placeholder => {}
                OsState::Private { .. } => {
                    // SAFETY: private committed memory this process owns, produced by
                    // `commit_placeholder`, being torn down; the space is going away, so nothing
                    // may hold a reference into it.
                    let result =
                        unsafe { vm::decommit_to_placeholder(start as *mut u8, entry.len) };
                    if let Err(error) = result {
                        fail(platform("close", start, entry.len)(error));
                    }
                    self.map.free_range(start, entry.len);
                }
                OsState::View { view } => {
                    let mut view_end = entry_end;
                    for (other, candidate) in self.map.iter() {
                        if candidate.os == (OsState::View { view }) {
                            view_end = view_end.max(other + candidate.len);
                        }
                    }
                    let view_len = view_end - start;
                    // SAFETY: one whole view this process owns, being torn down; the map knows its
                    // extent because it knows which entries share the view id.
                    if let Err(error) = unsafe { vm::unmap(start as *mut u8, view_len) } {
                        fail(platform("close", start, view_len)(error));
                    }
                    self.map.replace(start, view_len, Entry::free(view_len));
                }
            }
            position = entry_end.max(position + self.page);
        }

        // 2. One reservation again. Coalescing is only legal — and only necessary — when the space
        //    was actually split; a coalesce of a range holding a single placeholder fails with 487.
        let base = self.map.base();
        let len = self.map.end() - base;
        if self.map.entry_count() > 1 {
            // SAFETY: after step 1 the whole space is placeholders this process owns.
            match unsafe { vm::coalesce_placeholders(base as *mut u8, len) } {
                Ok(()) => self.map.replace(base, len, Entry::free(len)),
                Err(error) => fail(platform("close", base, len)(error)),
            }
        }

        // 3. Release. If the coalesce failed the space is still fragmented, so fall back to
        //    releasing each placeholder individually rather than leaking all of it.
        if self.map.entry_count() == 1 {
            if let Err(error) = vm::release(self.reservation) {
                fail(platform("close", base, len)(error));
            }
        } else {
            let pieces: Vec<(GuestAddr, usize)> =
                self.map.iter().map(|(start, entry)| (start, entry.len)).collect();
            for (start, piece_len) in pieces {
                let offset = start - base;
                match self.reservation.subrange(offset, piece_len, vm::ReservationKind::Placeholder)
                {
                    Ok(piece) => {
                        if let Err(error) = vm::release(piece) {
                            fail(platform("close", start, piece_len)(error));
                        }
                    }
                    Err(error) => fail(platform("close", start, piece_len)(error)),
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// How much of a survivor is compared against the file at a time.
///
/// 1 MiB bounds the temporary mapping, costs three kernel calls per megabyte, and is a multiple of
/// the allocation granularity so that the placeholder the comparison view needs is exact.
const SCAN_WINDOW: usize = 1024 * 1024;

/// A piece of a view that survives a partial unmap, and the content that has to survive with it.
struct Survivor {
    start: GuestAddr,
    len: usize,
    owner: Owner,
    ever_writable: bool,
    preserved: Vec<Dirty>,
}

/// A run of bytes that exists only in a copy-on-write page, and that re-mapping would lose.
struct Dirty {
    address: GuestAddr,
    bytes: Vec<u8>,
}

/// A temporary read-only view of a backing file's bytes *as they are on disk*, mapped outside the
/// guest address space purely to compare against.
///
/// Mapping the same section again is the only way to get an authoritative answer to "what would a
/// fresh `map_file` of this range produce", which is exactly the question that decides whether a
/// page holds privatised content. Reading the file through an ordinary file handle would answer a
/// subtly different question and would need a second I/O path.
struct PristineView {
    reservation: Reservation,
    len: usize,
    mapped: bool,
}

impl PristineView {
    fn map(
        operation: &'static str,
        backing: &Backing,
        file_offset: u64,
        len: usize,
    ) -> MemResult<Self> {
        let granularity = vm::allocation_granularity();
        let reserve_len = len.div_ceil(granularity) * granularity;
        let reservation = vm::reserve_placeholder(reserve_len, granularity)
            .map_err(platform(operation, 0, reserve_len))?;
        let mut view = Self { reservation, len, mapped: false };
        if reserve_len > len {
            // A view needs a placeholder of exactly its own size.
            let piece = vm::split_placeholder(&reservation, 0, len)
                .map_err(platform(operation, reservation.base(), len))?;
            debug_assert_eq!(piece.base(), reservation.base());
        }
        // SAFETY: `[base, base + len)` is exactly one unreplaced placeholder piece, reserved by this
        // call and split to size, so nothing else in the process can be using it. The view is
        // read-only, so it cannot modify the file or privatise anything.
        unsafe {
            vm::map_file(backing.file(), file_offset, len, view.as_mut_ptr(), Protection::Read)
        }
        .map_err(platform(operation, reservation.base(), len))?;
        view.mapped = true;
        Ok(view)
    }

    fn as_ptr(&self) -> *const u8 {
        self.reservation.base() as *const u8
    }

    fn as_mut_ptr(&self) -> *mut u8 {
        self.reservation.base() as *mut u8
    }
}

impl Drop for PristineView {
    fn drop(&mut self) {
        let granularity = vm::allocation_granularity();
        let reserve_len = self.len.div_ceil(granularity) * granularity;
        let split = reserve_len > self.len;

        if self.mapped {
            // SAFETY: one whole view, mapped by `map` and unmapped exactly once here. The
            // comparison has finished, so nothing holds a reference into it.
            if let Err(error) = unsafe { vm::unmap_and_release(self.as_mut_ptr(), self.len) } {
                tracing::error!(%error, "releasing a pristine comparison view failed");
            }
        } else {
            let head = if split {
                self.reservation.subrange(0, self.len, vm::ReservationKind::Placeholder).ok()
            } else {
                Some(self.reservation)
            };
            if let Some(head) = head {
                if let Err(error) = vm::release(head) {
                    tracing::error!(%error, "releasing a pristine comparison placeholder failed");
                }
            }
        }
        if split {
            match self.reservation.subrange(
                self.len,
                reserve_len - self.len,
                vm::ReservationKind::Placeholder,
            ) {
                Ok(rest) => {
                    if let Err(error) = vm::release(rest) {
                        tracing::error!(%error, "releasing a comparison view's tail failed");
                    }
                }
                Err(error) => tracing::error!(%error, "naming a comparison view's tail failed"),
            }
        }
    }
}

/// The pages of `[address, address + len)` whose contents differ from the backing file.
///
/// Those are the pages a copy-on-write write has privatised. Comparing content answers the question
/// exactly, and it answers it in the direction that matters: a page that happens to equal the file
/// needs no preserving, because re-mapping produces the same bytes. Detecting privatisation directly
/// would be cheaper but is not reliable — a privatised page still reports `MEM_MAPPED` (Task 1), and
/// `QueryWorkingSetEx`'s shared bit only means anything for pages that are resident at the moment it
/// is asked.
fn scan_for_copy_on_write(
    operation: &'static str,
    address: GuestAddr,
    len: usize,
    owner: &Owner,
    page: usize,
) -> MemResult<Vec<Dirty>> {
    let backing = owner.backing.as_ref().expect("a view always has a backing");
    let mut dirty: Vec<Dirty> = Vec::new();
    let mut offset = 0;
    while offset < len {
        let window = SCAN_WINDOW.min(len - offset);
        let live = address + offset;
        let pristine = PristineView::map(operation, backing, owner.offset_at(live), window)?;

        let mut position = 0;
        while position < window {
            let step = page.min(window - position);
            // SAFETY: both are live mappings of at least `step` bytes from `position` — the live view
            // because the caller owns it and has made it readable, and the pristine one because it
            // was just mapped over `window` bytes.
            let (live_page, file_page) = unsafe {
                (
                    std::slice::from_raw_parts((live + position) as *const u8, step),
                    std::slice::from_raw_parts(pristine.as_ptr().add(position), step),
                )
            };
            if live_page != file_page {
                match dirty.last_mut() {
                    // Adjacent dirty pages become one run, so a large privatised region costs one
                    // buffer and one protect-write-protect cycle rather than one per page.
                    Some(last) if last.address + last.bytes.len() == live + position => {
                        last.bytes.extend_from_slice(live_page);
                    }
                    _ => dirty.push(Dirty { address: live + position, bytes: live_page.to_vec() }),
                }
            }
            position += step;
        }
        drop(pristine);
        offset += window;
    }
    Ok(dirty)
}

/// Write preserved copy-on-write content back into a re-mapped survivor.
///
/// Each run is made writable, written, and returned to the protection the piece is recorded with.
/// Writing through `Protection::ReadWrite` on a private file view is a copy-on-write write, so this
/// privatises exactly the pages that were privatised before and leaves every clean page shared.
fn restore_copy_on_write(
    operation: &'static str,
    preserved: &[Dirty],
    protection: Protection,
) -> MemResult<()> {
    for run in preserved {
        let len = run.bytes.len();
        // SAFETY: the range is part of a live view this process has just mapped, and is page-aligned
        // and a whole number of pages because every mapping length here is.
        unsafe { vm::protect(run.address as *mut u8, len, Protection::ReadWrite) }
            .map_err(platform(operation, run.address, len))?;
        // SAFETY: the range is now writable and `len` bytes long, and the source is a heap buffer
        // that cannot overlap a mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(run.bytes.as_ptr(), run.address as *mut u8, len);
        }
        // SAFETY: as above. Restoring the recorded protection keeps the region map truthful.
        unsafe { vm::protect(run.address as *mut u8, len, protection) }
            .map_err(platform(operation, run.address, len))?;
    }
    Ok(())
}

/// `[start, start + len)` minus `[hole_start, hole_end)`, as up to two surviving pieces.
fn subtract(
    start: GuestAddr,
    len: usize,
    hole_start: GuestAddr,
    hole_end: GuestAddr,
) -> Vec<(GuestAddr, usize)> {
    let end = start + len;
    let mut pieces = Vec::new();
    if hole_start > start {
        pieces.push((start, hole_start.min(end) - start));
    }
    if hole_end < end {
        pieces.push((hole_end.max(start), end - hole_end.max(start)));
    }
    pieces.retain(|&(_, len)| len > 0);
    pieces
}

#[cfg(test)]
mod tests {
    use super::subtract;

    #[test]
    fn subtract_covers_head_tail_middle_and_everything() {
        // A hole through the middle leaves two pieces.
        assert_eq!(subtract(100, 100, 140, 160), vec![(100, 40), (160, 40)]);
        // Unmapping the head leaves the tail.
        assert_eq!(subtract(100, 100, 100, 140), vec![(140, 60)]);
        // Unmapping the tail leaves the head.
        assert_eq!(subtract(100, 100, 160, 200), vec![(100, 60)]);
        // Unmapping everything leaves nothing.
        assert_eq!(subtract(100, 100, 100, 200), Vec::new());
        // A hole that extends past both ends leaves nothing.
        assert_eq!(subtract(100, 100, 0, 1000), Vec::new());
        // A hole that does not intersect leaves the whole range, as one piece.
        assert_eq!(subtract(100, 100, 300, 400), vec![(100, 100)]);
    }
}
