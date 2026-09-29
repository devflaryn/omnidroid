//! The per-instance guest address space.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omni_platform::vm::{self, Protection, Reservation};
use parking_lot::{Mutex, MutexGuard};

use crate::backing::Backing;
use crate::cache::{self, Generation};
use crate::entry::{Entry, EntryMap, Owner, OsState, ViewId};
use crate::error::{platform, MemError, MemResult};
use crate::region::RegionInfo;

mod subpage_ops;
pub use subpage_ops::{AccessPtr, SplitStats};

/// A guest virtual address. Identical to the host address it lives at: a guest pointer *is* a host
/// pointer (ARCHITECTURE.md section 1), so there is no translation and no distinct address type --
/// except inside a space's [`LowWindow`] (D41), which only a space asked for one has.
pub type GuestAddr = usize;

/// Where a [`LowWindow`] ends: 4 GiB, the reach of ART's 32-bit object references.
pub const LOW_WINDOW_END: GuestAddr = 1 << 32;

/// The part of a guest space below [`LOW_WINDOW_END`] that is backed elsewhere in the host (D41,
/// `docs/ports/macos-low-window.md`): guest address `g` in it lives at host address `g + delta`.
///
/// For a host that can map nothing below 4 GiB -- macOS, whose arm64 `__PAGEZERO` is 4 GiB and
/// hard -- where ART must still have its heap and boot image there. Above the window a guest
/// address stays the host's, so a guest pointer the host reads in place (a Vulkan struct) is one,
/// as long as it was not placed in the window by address; placements without an address never are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LowWindow {
    /// The first guest address in the window: the space's base.
    pub start: GuestAddr,
    /// The guest address the window ends at, exclusive: [`LOW_WINDOW_END`]. The page below it is
    /// a host-owned guard, so no mapping straddles the seam between the based and identity halves.
    pub end: GuestAddr,
    /// What is added to a guest address below [`end`](Self::end) to reach its host address. A
    /// multiple of 4 GiB, so a host address's low 32 bits are the guest address.
    pub delta: usize,
}

impl LowWindow {
    /// The host address of guest address `address`: based below [`end`](Self::end), as dynarmic's
    /// fast path computes it (patch 0030), else the same.
    #[must_use]
    pub const fn host(&self, address: GuestAddr) -> usize {
        if address < self.end {
            address.wrapping_add(self.delta)
        } else {
            address
        }
    }

    /// The guest address the window's host address `host` stands for, if it is in the window.
    #[must_use]
    pub const fn guest(&self, host: usize) -> Option<GuestAddr> {
        if host >= self.start.wrapping_add(self.delta) && host < self.end.wrapping_add(self.delta) {
            Some(host - self.delta)
        } else {
            None
        }
    }
}

/// [`LowWindow::host`], or the address itself for a space without a window.
#[inline]
fn host_of(window: Option<LowWindow>, address: GuestAddr) -> usize {
    match window {
        Some(window) => window.host(address),
        None => address,
    }
}

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

/// The default ceiling on total commit charge one guest address space may hold: 3.5 GiB.
///
/// # Why there are two ceilings and not one
///
/// Commit charge is the scarce resource (D10, Global Constraint 6), and *every* quantity that
/// decides how much of it to spend arrives, somewhere up the stack, from a file. The measured case:
/// `p_memsz` of a `PT_LOAD` is attacker-controlled (D6 says a tampered library is the expected
/// input), the ELF loader turns the part of it past `p_filesz` into an anonymous `.bss` mapping, and
/// [`CommitPolicy::Eager`] commits a mapping in full. An **eight-byte** edit to `libroblox.so`'s
/// `p_memsz` was measured to take +1026.004 MiB of commit charge at 1 GiB and **+3406.664 MiB at
/// 3.3 GiB, with the load reporting success**, against the 16.7 MiB a stock load costs.
///
/// But the project goal has Roblox *legitimately* needing several GB during startup before settling
/// near 500 MB, and D10 validated exactly that: an instance grown to **3 GB of live use** and then
/// released, falling back to 513.656 MiB with its 4 GiB reservation intact. So a single ceiling
/// cannot do the job. Any number loose enough to permit 3 GB of legitimate growth is also loose
/// enough to permit a 3.3 GiB attack: **the two are separated by shape, not by size.** One is many
/// mappings growing over time; the other is *one* mapping committed in *one* call.
///
/// Hence the pair. [`DEFAULT_MAX_COMMIT_REQUEST`] is tight and does the security work, because
/// nothing legitimate commits a multi-gigabyte mapping in a single call. This constant is loose and
/// exists so that no single instance can turn its whole address space into commit charge.
///
/// The bound lives here, at the one place commit is actually performed, rather than in the loader,
/// because every future consumer needs it: D5 measured dynarmic's per-thread code caches at
/// 20–35 MiB **each** with no sharing between threads, and a loader-only fix would not cover them.
///
/// # Why 3.5 GiB
///
/// It is bracketed, not picked. The floor is the requirement: D10's measured 3 GB of live use has to
/// pass **at the defaults**, or the fix for the attack has broken the feature, so the ceiling must
/// clear 3 GB plus its page-table charge (`size / 512`, measured) — about 3078 MiB. The cap is
/// [`DEFAULT_SPACE_SIZE`]: total commit can never exceed the space's own size anyway, so a ceiling
/// at or above 4 GiB would be no ceiling at all. 3.5 GiB sits between them, leaving 512 MiB of
/// headroom over the validated scenario while still refusing to commit the whole space.
///
/// Within a 4 GiB space that is deliberately a **weak** bound, and it is meant to be: it is not what
/// stops the attack. It is what stops one instance from spending its entire address space, and it is
/// the per-instance budget to lower for a host running many instances. Scale it with
/// [`GuestSpaceConfig::size`] rather than inheriting this default for a much larger space.
pub const DEFAULT_MAX_COMMITTED: usize = 3584 * 1024 * 1024;

/// The default ceiling on a single commit request: 128 MiB.
///
/// **The tight half of the pair, and the one that refuses the tampered `p_memsz`.** A
/// [`CommitPolicy::Lazy`] mapping commits one granule — 64 KiB by default — per call, so this never
/// binds on the lazy path however large the mapping is, which is what lets an instance grow to
/// gigabytes under [`DEFAULT_MAX_COMMITTED`]. A [`CommitPolicy::Eager`] mapping commits its **whole
/// length in one call** by construction, so in practice this is the ceiling on how large an
/// eagerly-committed mapping may be — and a multi-gigabyte one is absurd for any real library.
///
/// # Why 128 MiB
///
/// Bracketed by measurement rather than rounded to taste. The largest private anonymous piece any
/// real library asks for, across all eleven `.so` in the APK:
///
/// | library | largest single anonymous piece |
/// |---|---|
/// | `libroblox.so` | **11,575,296 B** (11.04 MiB) — its `.bss` |
/// | `libbacktrace-native.so` | 24,576 B |
/// | six others | 4,096 B |
/// | three others | 0 |
///
/// So the real corpus has exactly one segment above 64 KiB, and the second-largest is **471x
/// smaller** than it. The other legitimate constraint is not a library at all: the D10 requirement
/// test grows an instance in eager 64 MiB chunks, which is the largest single eager commit anywhere
/// in the workspace and stands in for a guest asking for a large region up front.
///
/// That brackets the value between **64 MiB** (must pass) and **1,026 MiB** (the smaller demonstrated
/// attack; must fail). 128 MiB is the smallest power of two clear of the legitimate side with a
/// factor of two in hand. Margins: **11.6x** the largest real segment, 5461x the second-largest
/// library's, 2x the largest eager mapping in the suite; **8.4x** below the 1 GiB tamper and **27x**
/// below the 3.3 GiB one. Nothing measured lies between 64 MiB and 1 GiB, which is why a looser
/// value would buy nothing and a tighter one would start colliding with legitimate use.
///
/// It is the per-*request* limit and not a per-mapping one because a per-mapping total would have to
/// be recomputed by walking that mapping's entries on every granule commit, which is quadratic in
/// exactly the case lazy commit exists for. What a mapping accumulates over many granules is bounded
/// by [`DEFAULT_MAX_COMMITTED`] instead.
pub const DEFAULT_MAX_COMMIT_REQUEST: usize = 128 * 1024 * 1024;

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
    /// Reserve at exactly this address instead of where the host chooses (`None`). A guest address
    /// is a host address (D4), so a guest that needs low memory (ART's heap below 4 GiB) needs the
    /// reservation there.
    pub base: Option<GuestAddr>,
    /// Size of the reservation, in bytes. Rounded up to a page. Costs no commit charge whatever it
    /// is, so this is not the number to economize on (D10).
    pub size: usize,
    /// Alignment of the reservation's base address. A power of two.
    pub base_alignment: usize,
    /// The lazy-commit granule. See [`DEFAULT_COMMIT_GRANULE`] for the measurements behind the
    /// default. Must be a non-zero multiple of the page size.
    pub commit_granule: usize,
    /// Ceiling on the total *anonymous* commit charge this space may hold at once, in bytes.
    /// Non-zero.
    ///
    /// Unlike [`size`](Self::size), which costs nothing, this bounds a scarce resource. See
    /// [`DEFAULT_MAX_COMMITTED`] for why it exists, what it is measured against, and why the default
    /// is what it is. Reaching it is [`MemError::CommitCeiling`].
    ///
    /// **What it does not cover.** Copy-on-write charge raised by [`GuestSpace::protect`] and
    /// page-table charge both sit outside this ceiling *and* outside
    /// [`max_commit_request`](Self::max_commit_request). Neither is an amplification vector today,
    /// because copy-on-write charge is bounded by the size of the file being mapped, but a tampered
    /// binary can still provoke roughly 127 MiB per segment up to the loader's own 256 MiB cap. That
    /// is bounded, not unbounded — and it is the tight per-request bound, not this one, that refuses
    /// the unbounded case.
    pub max_committed: usize,
    /// Ceiling on a single commit request, in bytes. Non-zero.
    ///
    /// See [`DEFAULT_MAX_COMMIT_REQUEST`]. In practice this is the ceiling on the size of an
    /// eagerly-committed mapping, because [`CommitPolicy::Eager`] commits a mapping's whole length in
    /// one call. Reaching it is [`MemError::CommitRequestTooLarge`].
    pub max_commit_request: usize,
    /// Reserve [`base`](Self::base)'s range **around** whatever the host already holds inside it,
    /// rather than failing. Default `false`; only meaningful with `base: Some(_)` (a space placed by
    /// the host is free by construction, and this is ignored).
    ///
    /// Why: a guest address is a host address (D4), ART needs its boot image near 0x7000_0000 and
    /// its heap below 4 GiB, and on Windows every process has `KUSER_SHARED_DATA` at 0x7FFE_0000 --
    /// so one reservation of a low range that covers it fails with `ERROR_INVALID_ADDRESS`
    /// (measured: base 0x4000_0000 fails at any size, 0x1000_0000 + 1 GiB and 0x8000_0000 + 64 GiB
    /// succeed).
    ///
    /// With it, the space asks the host which sub-ranges are in use
    /// ([`vm::occupied_ranges`](omni_platform::vm::occupied_ranges)), rounds each out to the host's
    /// allocation granularity (and to this space's page) because the rest of a granule the host has
    /// touched cannot be reserved, and reserves one placeholder on each free range between them.
    /// Each in-use range becomes a permanent entry the host owns ([`RegionKind::Host`]): never free,
    /// never placed into by any [`Placement`], refused by `unmap`, `protect` and `discard`
    /// ([`MemError::HostOwned`]), and left alone by teardown. [`GuestSpace::base`] and
    /// [`GuestSpace::end`] still span the whole range, holes included.
    ///
    /// The host can take more of the range between the question and the reservations; the space
    /// then asks again, a few times, before giving up with the reservation's error.
    ///
    /// [`RegionKind::Host`]: crate::RegionKind::Host
    pub around_host: bool,
    /// Back the part of the space below [`LOW_WINDOW_END`] with a host reservation wherever the
    /// host chooses, addressed as `g + delta` ([`LowWindow`], D41), instead of at the guest
    /// addresses themselves. Default `false`. Needs `base: Some(_)` below `LOW_WINDOW_END`. The
    /// rest of [`size`](Self::size) is the identity: at 4 GiB when the host has it free there, else
    /// wherever the host puts it, with `[4 GiB, there)` one host-owned range -- so the space's
    /// [`len`](GuestSpace::len) can exceed `size`. [`around_host`](Self::around_host) is not used.
    ///
    /// Why: ART's heap and boot image must be below 4 GiB, and macOS maps nothing there
    /// (`vm::lowest_mappable_address` is 4 GiB on Apple silicon). Where the host can map low, the
    /// window is not needed, and it works just the same.
    pub low_window: bool,
    /// Ask for 4 KiB guest pages ([`crate::GUEST_PAGE`], the only value accepted): `map_anonymous`,
    /// `protect`, `unmap` and `discard` are then exact to 4 KiB whatever the host's page is. On a
    /// host whose page is larger and that can map a page twice (`vm::supports_alias`: macOS), the
    /// space keeps a sub-page overlay (`crate::subpage`); where the host page is 4 KiB it needs none
    /// and behaves exactly as without. `None` (the default): the space's page is the host's.
    pub guest_page: Option<usize>,
}

impl Default for GuestSpaceConfig {
    fn default() -> Self {
        Self {
            base: None,
            size: DEFAULT_SPACE_SIZE,
            base_alignment: vm::allocation_granularity(),
            commit_granule: DEFAULT_COMMIT_GRANULE,
            max_committed: DEFAULT_MAX_COMMITTED,
            max_commit_request: DEFAULT_MAX_COMMIT_REQUEST,
            around_host: false,
            low_window: false,
            guest_page: None,
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

/// The smallest page an arm64 Linux guest is built for, and the granularity
/// [`GuestSpace::discard`] accepts **whatever the host's page is**.
///
/// On Windows and x86-64 Linux the host page is this size too. On Apple silicon it is 16 KiB,
/// and `AT_PAGESZ` says so, but code built with a compile-time 4 KiB page still hands
/// `madvise(MADV_DONTNEED)` ranges on 4 KiB boundaries -- the pinned case is
/// `madvise(map + 4096, 4096, MADV_DONTNEED)`, which a 16 KiB-granular check refused with `EINVAL`
/// (2026-09-25).
pub const SMALL_PAGE: usize = crate::subpage::GUEST_PAGE;

/// How `[address, address + len)` lies across pages of one size: the part before the first page
/// boundary inside it, the whole pages, and the part after the last. Each part is `(start, len)`
/// and absent when empty. See [`split_at_pages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSplit {
    /// The start of the range when it does not begin on a page boundary: from `address` to the
    /// next boundary, or to the range's end if that comes first -- which makes it the whole range
    /// when the range starts off a boundary and ends before the next one.
    pub head: Option<(GuestAddr, usize)>,
    /// Every page lying entirely inside the range.
    pub whole: Option<(GuestAddr, usize)>,
    /// The end of the range when it does not end on a page boundary: from the last boundary inside
    /// the range (the range's own start, when that is one) to its end. Absent when the head
    /// already reaches the end.
    pub tail: Option<(GuestAddr, usize)>,
}

impl PageSplit {
    /// The partial parts, head first: what cannot be handed back a page at a time.
    pub fn partial(&self) -> impl Iterator<Item = (GuestAddr, usize)> {
        self.head.into_iter().chain(self.tail)
    }
}

/// Split `[address, address + len)` at the boundaries of `page`-sized pages.
///
/// Pure, and `page` is a parameter rather than the host's so that the split a 16 KiB host makes
/// can be checked on a 4 KiB one. The three parts tile the range exactly, in order.
///
/// # Panics
///
/// If `page` is not a power of two, or `address + len` overflows.
#[must_use]
pub fn split_at_pages(address: GuestAddr, len: usize, page: usize) -> PageSplit {
    assert!(page.is_power_of_two(), "a page size is a power of two, not {page:#x}");
    let end = address.checked_add(len).expect("the range wraps the address space");
    let mask = page - 1;
    // `then`, not `then_some`: the length is only computed when it is not negative.
    let piece = |from: GuestAddr, to: GuestAddr| (from < to).then(|| (from, to - from));
    // The first boundary at or after `address` (clamped to `end`, so a range inside one page
    // gives a head that is the whole range) and the last at or before `end`. When the range lies
    // inside one page, `first` is past `last` and there are no whole pages.
    let first = address.checked_add(mask).map_or(end, |a| (a & !mask).min(end));
    let last = end & !mask;
    let head = (address & mask != 0).then(|| piece(address, first)).flatten();
    let whole = piece(first, last);
    let tail_from = last.max(head.map_or(address, |(at, len)| at + len));
    let tail = (end & mask != 0).then(|| piece(tail_from, end)).flatten();
    PageSplit { head, whole, tail }
}

/// What [`GuestSpace::discard`] did to a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Discarded {
    /// Bytes of whole host pages decommitted: their commit charge is back, and the demand pager
    /// commits them again as zero-filled pages on the next touch.
    pub decommitted: usize,
    /// Bytes zeroed in place, because they share a host page with bytes outside the range that
    /// the guest still owns. Those pages stay committed.
    pub zeroed: usize,
}

/// A snapshot of what a guest address space currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceStats {
    /// Size of the reservation: the space's whole range, including any ranges the host holds
    /// inside it ([`GuestSpaceConfig::around_host`]), which count as neither `mapped` nor `free`.
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
/// accesses. **Guest pointer checks do**: every import handler's bounds check and `omni-cpu`'s
/// instruction fetch ask [`crate::admit`], which asks [`region_at`](GuestSpace::region_at). Those
/// are answered from a per-thread cache with no lock and no shared write while the map has not
/// changed; `crate::cache` has the measurement that made that necessary and the ordering argument
/// that makes it exact. Every path that takes the lock to change the map goes through
/// `GuestSpace::write`, which is what tells the cache.
pub struct GuestSpace {
    base: GuestAddr,
    len: usize,
    page: usize,
    granule: usize,
    /// The part below 4 GiB backed elsewhere in the host, when asked for (D41). Fixed for the
    /// space's life, so it is read without the lock.
    window: Option<LowWindow>,
    inner: Mutex<Inner>,
    /// The 4 KiB overlay, when [`GuestSpaceConfig::guest_page`] asked for it and the host needs
    /// and can give one (`crate::subpage`).
    sub: Option<crate::subpage::SubPagesHandle>,
    /// Bumped by every write section, under the lock and before the write; read without the lock
    /// by `crate::cache` to decide whether a remembered entry is still true.
    generation: Generation,
}

/// The map lock, held for reading only.
///
/// Only `Deref`, deliberately: the per-thread cache is only sound if every change to the map bumps
/// [`GuestSpace::generation`], and the way to make "every" true is for a path that did not bump to be
/// unable to change anything. Such a path does not compile.
struct MapRead<'a>(MutexGuard<'a, Inner>);

impl core::ops::Deref for MapRead<'_> {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

struct Inner {
    map: EntryMap,
    /// The OS placeholders this space reserved, in address order: one for the whole space, or --
    /// reserved [`around_host`](GuestSpaceConfig::around_host) -- one per free range between the
    /// host's. A split or coalesce never crosses from one to the next (the host's range lies
    /// between them), and teardown releases each exactly once.
    reservations: Vec<Reservation>,
    page: usize,
    granule: usize,
    cursor: GuestAddr,
    /// Where a placement without an address starts again when it wraps: the space's base, or the
    /// end of its [`LowWindow`], which only a placement by address may use.
    floor: GuestAddr,
    /// [`GuestSpace::window`], for the host calls made under the lock.
    window: Option<LowWindow>,
    /// The rest of a [`LowWindow`]'s 4 GiB -- below the base (the guest's null page and all near
    /// it) and from the seam up -- held inaccessible for as long as the space lives, so that
    /// nothing else in the process is ever placed where a guest pointer there would reach it.
    held: Vec<Reservation>,
    released: bool,
    /// Bytes of private committed memory this space currently holds, maintained incrementally
    /// because [`Inner::commit_range`] has to know it on every granule and walking the map there
    /// would make lazy commit quadratic. `validate` asserts it against a walk in debug builds, so a
    /// drift is a failing test rather than a ceiling that quietly stops binding.
    committed: usize,
    max_committed: usize,
    max_commit_request: usize,
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
        Self::build(config, vm::page_size())
    }

    /// Reserve a guest address space that works at `page` bytes a page rather than at the host's.
    ///
    /// **What a host with larger pages does, reproduced on one with smaller.** Every alignment,
    /// rounding and split this space makes is at `page`, and a `page` that is a multiple of the
    /// host's is always a legal thing to ask the host for, so a 16 KiB space on a 4 KiB host
    /// behaves as the Apple silicon one does -- which is how the sub-page paths of
    /// [`discard`](GuestSpace::discard) are tested on every host, not only on the Mac.
    ///
    /// # Errors
    ///
    /// [`MemError::InvalidConfig`] for a `page` that is not a power of two or not a multiple of
    /// the host's page size, and otherwise as [`GuestSpace::with_config`].
    pub fn with_page_size(config: GuestSpaceConfig, page: usize) -> MemResult<Self> {
        if !page.is_power_of_two() || page % vm::page_size() != 0 {
            return Err(MemError::InvalidConfig {
                field: "page",
                value: page as u64,
                reason: "must be a power of two and a multiple of the host's page size",
            });
        }
        Self::build(config, page)
    }

    fn build(config: GuestSpaceConfig, page: usize) -> MemResult<Self> {
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
        if config.guest_page.is_some_and(|g| g != crate::subpage::GUEST_PAGE) {
            return Err(MemError::InvalidConfig {
                field: "guest_page",
                value: config.guest_page.unwrap_or(0) as u64,
                reason: "only 4096 is supported",
            });
        }
        if !config.base_alignment.is_power_of_two() {
            return Err(MemError::InvalidConfig {
                field: "base_alignment",
                value: config.base_alignment as u64,
                reason: "must be a power of two",
            });
        }
        // A ceiling of zero would refuse every commit, which is not a usable space; and there is no
        // "unlimited" value, deliberately. Saturating or absent limits are how hostile input turns
        // into a larger permission, which is backwards.
        if config.max_committed == 0 {
            return Err(MemError::InvalidConfig {
                field: "max_committed",
                value: 0,
                reason: "must be greater than zero; commit charge is the scarce resource and this \
                         space would be unable to commit anything",
            });
        }
        if config.max_commit_request == 0 {
            return Err(MemError::InvalidConfig {
                field: "max_commit_request",
                value: 0,
                reason: "must be greater than zero; commit charge is the scarce resource and this \
                         space would be unable to commit anything",
            });
        }
        if config.max_commit_request > config.max_committed {
            return Err(MemError::InvalidConfig {
                field: "max_commit_request",
                value: config.max_commit_request as u64,
                reason: "must not exceed max_committed; a single request the total ceiling would \
                         refuse anyway is a limit that never binds",
            });
        }

        // At least page-aligned: the host's allocation granularity already is, for the host's own
        // page, and is not for a larger page asked of `with_page_size`.
        let mut window = None;
        let mut held = Vec::new();
        let mut size = config.size;
        let (reservations, hosts) = match config.base {
            Some(at) if config.low_window => {
                let reserved = reserve_with_window(at, config.size, page)?;
                window = Some(reserved.window);
                size = reserved.len;
                held = reserved.held;
                (reserved.reservations, reserved.hosts)
            }
            Some(at) if config.around_host => reserve_around_host(at, config.size, page)?,
            Some(at) => (
                vec![vm::reserve_placeholder_at(at, config.size)
                    .map_err(platform("GuestSpace::with_config", at, config.size))?],
                Vec::new(),
            ),
            None if config.low_window => {
                return Err(MemError::InvalidConfig {
                    field: "low_window",
                    value: 1,
                    reason: "needs a base: the window is the part of the space below 4 GiB",
                })
            }
            None => (
                vec![vm::reserve_placeholder(config.size, config.base_alignment.max(page))
                    .map_err(platform("GuestSpace::with_config", 0, config.size))?],
                Vec::new(),
            ),
        };
        let base = config.base.unwrap_or_else(|| reservations[0].base());
        // A placement without an address never lands in the window: what the host reads in place
        // (D41) is always placed that way.
        let floor = window.map_or(base, |w| w.end.min(base + size));
        tracing::debug!(
            base = format_args!("{base:#x}"),
            size,
            granule = config.commit_granule,
            host_ranges = hosts.len(),
            window = ?window,
            "reserved a guest address space"
        );
        Ok(Self {
            base,
            len: size,
            page,
            granule: config.commit_granule,
            window,
            inner: Mutex::new(Inner {
                map: EntryMap::with_hosts(base, size, hosts),
                reservations,
                page,
                granule: config.commit_granule,
                cursor: floor,
                floor,
                window,
                held,
                released: false,
                committed: 0,
                max_committed: config.max_committed,
                max_commit_request: config.max_commit_request,
            }),
            generation: Generation::new(),
            sub: (config.guest_page.is_some() && page > crate::subpage::GUEST_PAGE && vm::supports_alias())
                .then(|| crate::subpage::SubPagesHandle::new(page / crate::subpage::GUEST_PAGE)),
        })
    }

    /// Take the map lock to **change** the map, telling every thread's cache first.
    ///
    /// The bump is made with the lock held and before the caller has touched anything, and it is
    /// made whether or not the caller then changes anything: a spurious bump costs one refill per
    /// thread, and a missing one is a stale answer. See `crate::cache` for why this is the order.
    fn write(&self) -> MutexGuard<'_, Inner> {
        let inner = self.inner.lock();
        self.generation.bump();
        inner
    }

    /// Take the map lock to **read** the map. See [`MapRead`].
    fn read(&self) -> MapRead<'_> {
        MapRead(self.inner.lock())
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

    /// The page the guest maps, protects and unmaps at: [`crate::GUEST_PAGE`] when this space keeps
    /// a 4 KiB overlay, otherwise [`page_size`](Self::page_size).
    #[must_use]
    pub fn guest_page_size(&self) -> usize {
        if self.sub.is_some() {
            crate::subpage::GUEST_PAGE
        } else {
            self.page
        }
    }

    /// Whether this space keeps the 4 KiB overlay (`crate::subpage`).
    #[must_use]
    pub fn subpages_active(&self) -> bool {
        self.sub.is_some()
    }

    /// Whether the host page holding `address` traps: some 4 KiB of it allows an access the host
    /// page refuses, and a served access goes through its alias. Lock-free, and one load when
    /// nothing traps; always `false` without the overlay (SUBPAGE-ORDER 2).
    #[inline]
    #[must_use]
    pub fn is_trapping(&self, address: GuestAddr) -> bool {
        match &self.sub {
            Some(sub) if address >= self.base && address < self.end() => sub.is_trapping((address - self.base) / self.page),
            _ => false,
        }
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
        if let Some(window) = self.window {
            // A range across the seam has no one host pointer; the guard page below it is never
            // mapped, so no mapped range is one.
            if address < window.end && address + len > window.end {
                return Err(MemError::OutsideSpace {
                    operation: "ptr",
                    address,
                    end: address + len,
                    space_base: self.base,
                    space_end: self.end(),
                    space_len: self.len,
                });
            }
        }
        Ok(self.host_addr(address) as *mut u8)
    }

    /// The host address guest address `address` lives at: the same, except in the space's
    /// [`LowWindow`] (D41). Not checked against the space; [`ptr`](Self::ptr) is.
    #[inline]
    #[must_use]
    pub fn host_addr(&self, address: GuestAddr) -> usize {
        host_of(self.window, address)
    }

    /// The guest address host address `host` stands for, if it is one of this space's: what a host
    /// fault's address means to the guest.
    #[inline]
    #[must_use]
    pub fn host_to_guest(&self, host: usize) -> Option<GuestAddr> {
        match self.window {
            Some(window) => window
                .guest(host)
                .or_else(|| (host >= window.end && host >= self.base && host < self.end()).then_some(host)),
            None => (host >= self.base && host < self.end()).then_some(host),
        }
    }

    /// The part of this space below 4 GiB that is backed elsewhere in the host, if it has one.
    #[must_use]
    pub fn low_window(&self) -> Option<LowWindow> {
        self.window
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
        if self.sub.is_some() {
            // 4 KiB guest pages (`subpage`): a fixed map is exact to 4 KiB; any other is placed on
            // a host page of its own, and what it does not use of its last one is a hole.
            let guest_len = self.check_guest_range(OP, placement_address(placement).unwrap_or(self.base), size)?;
            let mut inner = self.write();
            if let Placement::Fixed(address) = placement {
                if self.needs_overlay(&inner, address, guest_len) {
                    self.sub_apply(&mut inner, OP, address, guest_len, subpage_ops::SubOp::Map(protection, commit))?;
                    inner.validate();
                    return Ok(address);
                }
            }
            let host_len = self.round_size(OP, guest_len)?;
            let address = self.map_anonymous_locked(&mut inner, OP, placement, host_len, protection, commit)?;
            if guest_len < host_len {
                self.sub_apply(&mut inner, OP, address + guest_len, host_len - guest_len, subpage_ops::SubOp::Unmap)?;
            }
            inner.validate();
            return Ok(address);
        }
        let size = self.round_size(OP, size)?;
        let mut inner = self.write();
        let address = self.map_anonymous_locked(&mut inner, OP, placement, size, protection, commit)?;
        inner.validate();
        Ok(address)
    }

    /// `map_anonymous` with the lock held and `size` whole host pages: placement, the map entry
    /// and an eager commit.
    fn map_anonymous_locked(
        &self,
        inner: &mut Inner,
        operation: &'static str,
        placement: Placement,
        size: usize,
        protection: Protection,
        commit: CommitPolicy,
    ) -> MemResult<GuestAddr> {
        const OP: &str = "map_anonymous";
        let _ = operation;
        let address = self.place(inner, OP, placement, size)?;
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
            label: crate::label::current(),
        };
        inner.map.replace(
            address,
            size,
            // Anonymous memory is never a view, so there is no copy-on-write content to lose.
            Entry { len: size, os: OsState::Placeholder, owner: Some(owner), ever_writable: false },
        );

        if commit == CommitPolicy::Eager && protection != Protection::None {
            if let Err(error) = inner.commit_range(OP, address, size) {
                // The mapping exists in the map and in the OS by now, so a failed commit has to
                // undo it or the caller is handed an error *and* a mapping. That matters most for
                // the case this path exists to refuse: a rejected commit ceiling must leave no
                // address space claimed, or refusing a tampered library would itself become the
                // denial of service.
                if let Err(rollback) = inner.unmap_range(OP, address, size) {
                    tracing::error!(
                        %rollback,
                        address = format_args!("{address:#x}"),
                        size,
                        "could not undo a mapping whose eager commit failed"
                    );
                }
                return Err(error);
            }
        }
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
        let mut inner = self.write();
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
            label: crate::label::current(),
        };
        inner.map.replace(
            address,
            size,
            Entry {
                len: size,
                os: OsState::View { view },
                owner: Some(owner),
                // A ReadWrite view is PAGE_WRITECOPY, so it can hold privatised content from its
                // first write onwards. **Unless the backing is shared**: then every write is in
                // the file already, a survivor mapped again from it gets that content back by
                // construction, and there is nothing only a copy-on-write page holds.
                ever_writable: protection.is_writable() && !backing.is_shared(),
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
    /// at 150 ns/page at the default 64 KiB granule against 2414 ns/page for per-page commit, and
    /// 381 ns for the unavoidable first touch (see [`DEFAULT_COMMIT_GRANULE`] for the whole table) —
    /// and clipped to the mapping, so committing one byte of a mapping commits one granule of it and
    /// no more.
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
        // Already committed -- the common case, the kernel reading a system call's arguments --
        // changes nothing, so it neither bumps the generation (which makes every thread's
        // remembered regions stale, sending each `region_at` back to the lock) nor stays long.
        if !self.read().needs_commit(address, len) {
            return Ok(0);
        }
        let mut inner = self.write();
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
    /// [`MemError::NotMapped`] if any part of the range is free, [`MemError::HostOwned`] if any
    /// part of it is the host's (nothing is changed), or [`MemError::Platform`] —
    /// `ERROR_INVALID_PARAMETER` (87) when the protection exceeds what the backing file's section
    /// allows.
    pub fn protect(
        &self,
        address: GuestAddr,
        len: usize,
        protection: Protection,
    ) -> MemResult<()> {
        const OP: &str = "protect";
        if self.sub.is_some() {
            let len = self.check_guest_range(OP, address, len)?;
            let mut inner = self.write();
            inner.refuse_host(OP, address & !(self.page - 1), self.round_size(OP, len + address % self.page)?)?;
            if self.needs_overlay(&inner, address, len) {
                self.sub_apply(&mut inner, OP, address, len, subpage_ops::SubOp::Protect(protection))?;
                inner.validate();
                return Ok(());
            }
            drop(inner);
        }
        let len = self.round_size(OP, len)?;
        self.check_aligned(OP, "address", address)?;
        self.check_range(OP, address, len)?;
        let mut inner = self.write();
        inner.refuse_host(OP, address, len)?;
        inner.require_mapped(OP, address, len)?;
        inner.protect_range(OP, address, len, protection)?;
        inner.validate();
        Ok(())
    }

    /// Write `bytes` at `address`, **bypassing the pages' own protection**, the way the kernel
    /// writes into a process for `process_vm_writev` or `ptrace(POKETEXT)` -- a debugger patching
    /// read-only code, or a self-decrypting library writing its plaintext into its own `.text`
    /// after it has already set that code back to read-only.
    ///
    /// The bytes' pages must be mapped (a hole is [`MemError::NotMapped`], as a bad address is
    /// `EFAULT` to those syscalls). A page that is not writable is transiently made writable --
    /// copy-on-write on a file view, exactly the loader's relocation dance ([`protect`]) -- written,
    /// and restored to its own protection before this returns, all under the one lock and with no
    /// guest code running between, so no guest thread ever observes the page writable and no live
    /// translation is touched (the caller invalidates if it patched code that had already run).
    ///
    /// Returns the number of bytes written (all of them, or an error and none).
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::OutsideSpace`], [`MemError::NotMapped`] for a hole,
    /// [`MemError::HostOwned`], or [`MemError::Platform`].
    pub fn write_forced(&self, address: GuestAddr, bytes: &[u8]) -> MemResult<usize> {
        const OP: &str = "write_forced";
        if bytes.is_empty() {
            return Ok(0);
        }
        let len = bytes.len();
        self.check_range(OP, address, len)?;
        if self.sub.is_some() {
            let mut inner = self.write();
            if self.needs_overlay(&inner, address & !(self.page - 1), self.round_size(OP, len + address % self.page)?) {
                let mut done = 0;
                for (at, piece, tracked) in self.pieces_by_tracking(&inner, address, len) {
                    let chunk = &bytes[at - address..at - address + piece];
                    if tracked {
                        self.write_tracked(&mut inner, OP, at, chunk)?;
                    } else {
                        drop(inner);
                        self.write_forced(at, chunk)?;
                        inner = self.write();
                    }
                    done += piece;
                }
                inner.validate();
                return Ok(done);
            }
        }
        let page = self.page;
        let span = address & !(page - 1);
        let span_end = (address + len + page - 1) & !(page - 1);
        let span_len = span_end - span;

        let mut inner = self.write();
        inner.refuse_host(OP, span, span_len)?;
        inner.require_mapped(OP, span, span_len)?;
        // Lazy pages in the span get their backing so the raw write below lands; a view or already
        // committed private range is left as it is (`commit_range` skips non-placeholders).
        inner.commit_range(OP, span, span_len)?;
        // Carve the map at the span's edges so every overlapping entry is fully inside it.
        inner.ensure_boundary(OP, span)?;
        inner.ensure_boundary(OP, span_end)?;

        // Which sub-ranges are not writable, and the protection to restore each to.
        let to_flip: Vec<(GuestAddr, usize, Protection)> = inner
            .map
            .starts_overlapping(span, span_len)
            .into_iter()
            .filter_map(|start| {
                let entry = inner.map.get(start).expect("entry vanished");
                let protection = RegionInfo::from_entry(start, entry).protection;
                (!protection.is_writable()).then_some((start, entry.len, protection))
            })
            .collect();
        // Drop each to plain read-write for the copy -- never writable-and-executable. Nothing
        // runs while this lock is held, so execute is not needed during the write, and asking for
        // W+X here would be refused on macOS's hardened runtime (EACCES). This is the loader's own
        // relocation dance: `ReadExecute` down to `ReadWrite` (copy-on-write on a file view), write,
        // and back up.
        for &(start, entry_len, _) in &to_flip {
            inner.protect_range(OP, start, entry_len, Protection::ReadWrite)?;
        }

        // SAFETY: `host_addr` is where the guest address lives (D4, D41), the span is mapped,
        // committed, and every page in it is now writable, and `check_range` bounded the write to
        // the space. No guest code runs here, so nothing executes a page mid-flip.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host_addr(address) as *mut u8, len);
        }

        for (start, entry_len, protection) in to_flip {
            inner.protect_range(OP, start, entry_len, protection)?;
        }
        inner.validate();
        Ok(len)
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
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`], [`MemError::OutsideSpace`],
    /// [`MemError::HostOwned`] if the range reaches into one the host holds -- refused whole, so the
    /// parts on either side stay mapped -- or
    /// [`MemError::Platform`].
    pub fn unmap(&self, address: GuestAddr, len: usize) -> MemResult<()> {
        const OP: &str = "unmap";
        if self.sub.is_some() {
            let len = self.check_guest_range(OP, address, len)?;
            let mut inner = self.write();
            inner.refuse_host(OP, address & !(self.page - 1), self.round_size(OP, len + address % self.page)?)?;
            if self.needs_overlay(&inner, address, len) {
                self.sub_apply(&mut inner, OP, address, len, subpage_ops::SubOp::Unmap)?;
                inner.validate();
                return Ok(());
            }
            drop(inner);
        }
        let len = self.round_size(OP, len)?;
        self.check_aligned(OP, "address", address)?;
        self.check_range(OP, address, len)?;
        let mut inner = self.write();
        inner.refuse_host(OP, address, len)?;
        inner.unmap_range(OP, address, len)?;
        self.forget_aliases(address, len);
        inner.validate();
        tracing::debug!(address = format_args!("{address:#x}"), len, "unmapped guest memory");
        Ok(())
    }

    /// Write every **shared** file view in `[address, address + len)` back to its file, and the
    /// file to the device: the guest's `msync(MS_SYNC)`. Returns how many bytes of shared view the
    /// range held, which is what was written back; anonymous memory and private file views have
    /// nothing of their own to write and are skipped, as Linux skips them.
    ///
    /// Page-granular. **Free address space in the range is skipped too, not refused**, which is
    /// `mm/msync.c`'s rule: it writes back the mapped parts and only then reports `ENOMEM` for the
    /// hole. Whether there was one is the caller's to find out and report.
    ///
    /// # The flush runs with the map unlocked
    ///
    /// The views are found under the lock and written back after it is released, because a
    /// write-back is disk I/O -- the engine syncs 100 MiB mappings -- and any bounds check of any
    /// import handler that misses its thread's cache, and every pager fault, takes this lock (see
    /// [`map_lock_is_held`](GuestSpace::map_lock_is_held)). The backings are held by `Arc` across
    /// the gap, so the section and its file handle cannot go away under the flush. A guest thread
    /// that unmaps the range *during* its own `msync` is racing itself, as it would be on Linux;
    /// here the flush then fails, or writes back whatever view has replaced it, and touches no
    /// memory either way.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`], [`MemError::OutsideSpace`], or
    /// [`MemError::Platform`] when the host's write-back fails.
    pub fn sync(&self, address: GuestAddr, len: usize) -> MemResult<usize> {
        const OP: &str = "sync";
        let len = self.round_size(OP, len)?;
        self.check_aligned(OP, "address", address)?;
        self.check_range(OP, address, len)?;
        let end = address + len;
        let views: Vec<(GuestAddr, usize, Arc<Backing>)> = {
            let inner = self.read();
            inner
                .map
                .starts_overlapping(address, len)
                .into_iter()
                .filter_map(|start| {
                    let entry = inner.map.get(start)?;
                    let backing = entry.owner.as_ref()?.backing.as_ref()?;
                    if !matches!(entry.os, OsState::View { .. }) || !backing.is_shared() {
                        return None;
                    }
                    let from = start.max(address);
                    let to = (start + entry.len).min(end);
                    Some((from, to - from, Arc::clone(backing)))
                })
                .collect()
        };
        let mut synced = 0;
        for (from, piece, backing) in views {
            // SAFETY: the region map, which is the authority on what is at every guest address,
            // said a moment ago that `[from, from + piece)` is a view of `backing`, and the `Arc`
            // keeps that section alive. Nothing is dereferenced: the host call inspects and writes
            // back the range's pages, and if a concurrent unmap has changed the range since, it
            // fails or writes back whichever view is there now -- see this method's documentation.
            unsafe { vm::sync_view(backing.file(), self.host_addr(from) as *mut u8, piece) }
                .map_err(platform(OP, from, piece))?;
            synced += piece;
        }
        Ok(synced)
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
        let mut inner = self.write();
        let marked = inner.mark_idle(address, len);
        inner.validate();
        Ok(marked)
    }

    /// [`reclaim_idle`](GuestSpace::reclaim_idle) for the idle entries inside
    /// `[address, address + len)` only: what `madvise` needs once it has just marked that range.
    ///
    /// **Why a range version exists.** `reclaim_idle` walks every entry of the map to find idle
    /// ones and then walks it again to coalesce free placeholders. MEASURED (Windows, the Pet
    /// Simulator 99 world, 2026-09-25): once `MADV_FREE` also reclaimed at once, that whole-map
    /// walk under the write lock was 16% of all in-handler samples. A call that marked one range
    /// has nothing to reclaim outside it, so this visits only the entries overlapping it and
    /// leaves coalescing of free space to [`reclaim_idle`](GuestSpace::reclaim_idle) and `unmap`.
    ///
    /// # Errors
    ///
    /// [`MemError::Platform`] if a decommit fails.
    pub fn reclaim_idle_in(&self, address: GuestAddr, len: usize) -> MemResult<Reclaimed> {
        let mut inner = self.write();
        let reclaimed = inner.reclaim_in(address, len)?;
        inner.validate();
        Ok(reclaimed)
    }

    /// The guest's `MADV_DONTNEED` on private anonymous memory: afterwards every byte of
    /// `[address, address + len)` reads zero, and **no byte outside it changes**.
    ///
    /// Granular at [`SMALL_PAGE`] (4 KiB), not at this space's page. The range is split at host
    /// pages ([`split_at_pages`]):
    ///
    /// * **Whole host pages** are marked idle and decommitted at once, as
    ///   [`advise_idle`](GuestSpace::advise_idle) and
    ///   [`reclaim_idle_in`](GuestSpace::reclaim_idle_in) do: their commit charge comes back, and
    ///   the demand pager commits them again zero-filled on the next touch.
    /// * **A part of a host page** cannot be decommitted without taking the rest of that page with
    ///   it, and the rest is the guest's. So it is **zeroed in place**, and the page stays
    ///   committed. A page whose protection is not writable is raised to writable for the write
    ///   and put back, under the map lock; a guest thread storing to one of its other bytes in that
    ///   window succeeds where it would have faulted, which is the only difference and is a race
    ///   the guest has with its own `madvise` anyway.
    ///
    /// On a host whose page is 4 KiB (Windows, x86-64 Linux) a 4 KiB-aligned range is all whole
    /// pages and this is exactly the mark-and-reclaim it always was. On Apple silicon's 16 KiB
    /// pages, a 16 KiB-granular check refused the engine's 4 KiB `madvise` ranges with `EINVAL`
    /// (2026-09-25), and rounding one up to 16 KiB instead would wipe up to 12 KiB of live heap
    /// beside it.
    ///
    /// Uncommitted pages already read zero and are left alone -- zeroing one would commit it,
    /// which is the opposite of what the call asks for. File views are left alone too, as
    /// `advise_idle` leaves them.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::Misaligned`] for an address that is not a multiple of
    /// [`SMALL_PAGE`], [`MemError::OutsideSpace`], [`MemError::HostOwned`] if the range reaches
    /// into one the host holds (nothing is changed), or [`MemError::Platform`] if a decommit or a
    /// protection change fails.
    pub fn discard(&self, address: GuestAddr, len: usize) -> MemResult<Discarded> {
        const OP: &str = "discard";
        if len == 0 {
            return Err(MemError::ZeroSize { operation: OP });
        }
        if address % SMALL_PAGE != 0 {
            return Err(MemError::Misaligned {
                operation: OP,
                what: "address",
                value: address as u64,
                required: SMALL_PAGE as u64,
            });
        }
        // A length that cannot be rounded up is outside any space, and `check_range` says so.
        let len = len.checked_next_multiple_of(SMALL_PAGE).unwrap_or(usize::MAX);
        self.check_range(OP, address, len)?;
        let mut inner = self.write();
        if self.sub.is_some() {
            inner.refuse_host(OP, address & !(self.page - 1), self.round_size(OP, len + address % self.page)?)?;
            let mut discarded = Discarded::default();
            for (at, piece, tracked) in self.pieces_by_tracking(&inner, address, len) {
                if tracked {
                    discarded.zeroed += self.zero_tracked(&mut inner, OP, at, piece)?;
                } else {
                    let d = self.discard_untracked(&mut inner, OP, at, piece)?;
                    discarded.decommitted += d.decommitted;
                    discarded.zeroed += d.zeroed;
                }
            }
            inner.validate();
            return Ok(discarded);
        }
        let discarded = self.discard_untracked(&mut inner, OP, address, len)?;
        inner.validate();
        Ok(discarded)
    }

    /// `discard` of a range with no tracked host page, the lock held.
    fn discard_untracked(&self, inner: &mut Inner, operation: &'static str, address: GuestAddr, len: usize) -> MemResult<Discarded> {
        const OP: &str = "discard";
        let _ = operation;
        let split = split_at_pages(address, len, self.page);
        let mut discarded = Discarded::default();
        inner.refuse_host(OP, address, len)?;
        if let Some((at, whole)) = split.whole {
            inner.mark_idle(at, whole);
            discarded.decommitted = inner.reclaim_in(at, whole)?.bytes;
        }
        for (at, part) in split.partial() {
            discarded.zeroed += inner.zero_in_place(OP, at, part)?;
        }
        if let Some((at, whole)) = split.whole {
            // A decommitted page's alias would hold its old memory (SUBPAGE-ORDER 5).
            self.forget_aliases(at, whole);
        }
        Ok(discarded)
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
        let mut inner = self.write();
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
        let inner = self.read();
        self.guest_view_regions(inner.regions(true), true)
    }

    /// Every *mapped* region, in address order: [`regions`](GuestSpace::regions) without the free
    /// ranges. This is the `/proc/self/maps` shape.
    #[must_use]
    pub fn mapped_regions(&self) -> Vec<RegionInfo> {
        let inner = self.read();
        self.guest_view_regions(inner.regions(false), false)
    }

    /// The parts of `[address, address + len)` that hold anything: committed private memory and
    /// file views, in address order, clipped to the range. The rest is free space or a lazy
    /// mapping's uncommitted granules, which read as zeros by construction -- what a copy of the
    /// range (the guest's `mremap`) need not touch, and must not, or it commits them.
    #[must_use]
    pub fn held_ranges(&self, address: GuestAddr, len: usize) -> Vec<(GuestAddr, usize)> {
        let inner = self.read();
        let end = address.saturating_add(len);
        let mut out: Vec<(GuestAddr, usize)> = Vec::new();
        for start in inner.map.starts_overlapping(address, len) {
            let Some(entry) = inner.map.get(start) else { continue };
            if !matches!(entry.os, OsState::Private { .. } | OsState::View { .. }) {
                continue;
            }
            let (s, e) = (start.max(address), (start + entry.len).min(end));
            if s >= e {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.0 + last.1 == s => last.1 += e - s,
                _ => out.push((s, e - s)),
            }
        }
        out
    }

    /// Every mapped region, as [`mapped_regions`](GuestSpace::mapped_regions) gives them, each
    /// with the [`MapLabel`](crate::MapLabel) of the mapping it belongs to -- who asked for it, as
    /// the [`label_scope`](crate::label_scope) in force when it was mapped said. For a memory
    /// report; nothing decides anything on a label.
    #[must_use]
    pub fn labelled_regions(&self) -> Vec<(RegionInfo, crate::MapLabel)> {
        self.read().labelled_regions()
    }

    /// The region containing an address, if it is mapped.
    ///
    /// Answered from this thread's cache when the map has not changed since this thread last looked
    /// at an **anonymous** entry covering `address` -- with no lock and nothing written that another
    /// thread reads -- and otherwise under the lock, which refills the cache. Either way the answer
    /// is the one a locked lookup at the current generation returns, field for field; `crate::cache`
    /// has the argument. A file-backed region always takes the lock, because rebuilding its
    /// [`RegionKind::File`](crate::RegionKind::File) would mean cloning the file's name, and that
    /// clone is a write to a reference count every thread shares.
    #[must_use]
    pub fn region_at(&self, address: GuestAddr) -> Option<RegionInfo> {
        if let Some(region) = self.remembered(address).and_then(|hit| hit.anonymous_region()) {
            cache::answered(cache::Answered::Region);
            return Some(region);
        }
        self.region_at_locked(address)
    }

    /// [`region_at`](GuestSpace::region_at) under the lock, remembering what it found.
    fn region_at_locked(&self, address: GuestAddr) -> Option<RegionInfo> {
        let (at, region) = {
            let inner = self.read();
            let start = inner.map.entry_start(address)?;
            let entry = inner.map.get(start)?;
            if entry.is_free() {
                return None;
            }
            // Read under the same lock as the entry: the tag says which map this came from. With
            // the 4 KiB overlay, the guest's view of it (SUBPAGE-ORDER 4).
            let info = self.guest_view_of(RegionInfo::from_entry(start, entry), address)?;
            (self.generation.locked(), info)
        };
        cache::remember(&self.generation, at, &region);
        Some(region)
    }

    /// The entry covering `address`, if this thread remembers it and the map has not changed since.
    /// No lock. A miss says nothing about the address; ask [`region_at`](GuestSpace::region_at).
    #[inline]
    pub(crate) fn remembered(&self, address: GuestAddr) -> Option<cache::Remembered> {
        cache::lookup(&self.generation, address)
    }

    /// Whether any page of `[at, at + len)` is mapped executable right now.
    ///
    /// One walk over the map's own entries, free ranges included, so a large hole costs one step
    /// rather than one per page. A range running past the space answers for the part inside it.
    /// What it is for: a translating backend can only hold translations of executable pages, so
    /// an unmap, reprotect or discard over a range with none has nothing to invalidate.
    #[must_use]
    pub fn any_executable(&self, at: GuestAddr, len: usize) -> bool {
        let inner = self.read();
        // A tracked host page's execute is its parts' (the host page never has it, `subpage`).
        if self.any_executable_part(at, len) {
            return true;
        }
        let end = at.saturating_add(len);
        let mut cursor = at;
        while cursor < end {
            let Some(start) = inner.map.entry_start(cursor) else { return false };
            let Some(entry) = inner.map.get(start) else { return false };
            if RegionInfo::from_entry(start, entry).protection.is_executable() {
                return true;
            }
            let next = start.saturating_add(entry.len);
            if next <= cursor {
                return false;
            }
            cursor = next;
        }
        false
    }

    /// Whether the region map's lock is held **right now**, by anyone.
    ///
    /// # Why a space needs this and a caller cannot get it any other way
    ///
    /// One `Mutex` guards the whole map, and **every** guest memory check can end up in it:
    /// `region_at` is on the path of every bounds check in every import handler, and the pager
    /// takes it to commit a page on a fault. Most checks are answered from a per-thread cache
    /// without it (`crate::cache`), but any check that misses -- which is every check on every
    /// thread after any change to the map -- waits for it. So a thread that holds it while blocked
    /// stops the entire runtime, and from outside that looks exactly like the guest having stopped
    /// — the import census freezes, no guest instructions are executed, and nothing names a lock.
    ///
    /// A **fault handler holds it without ever crossing the boundary**, which is what makes this
    /// invisible to the crossing records: the pager is not an import, so a thread inside it is
    /// reported as being in guest code.
    ///
    /// `try_lock` rather than a flag, so it costs nothing until something asks. It is a
    /// diagnostic and deliberately not a synchronisation primitive: `false` means only that it
    /// was free at the instant of the call.
    #[must_use]
    pub fn map_lock_is_held(&self) -> bool {
        self.inner.try_lock().is_none()
    }

    /// What the space currently holds.
    #[must_use]
    pub fn stats(&self) -> SpaceStats {
        self.read().stats()
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
        let mut inner = self.write();
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
        // An alignment larger than the space cannot be satisfied by any address in it, and
        // `find_free` rounds up to it: at `1 << 63` that computation panics in a debug build and
        // wraps to a spurious `NoSpace` in release. Refused here with the value, which is the only
        // answer that says what was wrong.
        if align > self.len {
            return Err(MemError::AlignmentTooLarge {
                operation,
                align,
                space_len: self.len,
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
        let mut inner = self.write();
        if let Some(alias) = self.sub.as_ref().and_then(|sub| sub.state.lock().alias.take()) {
            if let Err(error) = vm::release(alias) {
                tracing::error!(%error, "the 4 KiB overlay's alias reservation could not be released");
            }
        }
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
    /// The host address of guest address `address` ([`LowWindow`], D41).
    #[inline]
    fn host(&self, address: GuestAddr) -> usize {
        host_of(self.window, address)
    }

    #[inline]
    fn validate(&self) {
        #[cfg(debug_assertions)]
        {
            self.map.check_invariants();
            // The commit ceiling is only as good as the running total it is compared against, and
            // that total is maintained by hand at four places. Asserting it against a walk of the
            // map is what stops a missed decrement from quietly turning the ceiling off — or a
            // missed increment from making it refuse legitimate commits later.
            let walked: usize = self
                .map
                .iter()
                .filter(|(_, entry)| matches!(entry.os, OsState::Private { .. }))
                .map(|(_, entry)| entry.len)
                .sum();
            assert_eq!(
                walked, self.committed,
                "the running committed total drifted from the region map"
            );
        }
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
            // Checked: rounding up to a large alignment near the top of the address space overflows,
            // and the wrapped result would be a *lower* address that passes the range test below.
            // `check_align_argument` already caps `align` at the space length, so this is the second
            // line rather than the only one.
            let candidate = from.checked_add(align - 1)? & !(align - 1);
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
            // Wrapping never reaches below the floor for a search that started above it.
            let lower = if from >= self.floor { self.floor } else { self.map.base() };
            if let Some(address) = fits(run_start, run_len, lower) {
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
            unsafe { vm::coalesce_placeholders(self.host(union_start) as *mut u8, union_len) }
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
        // The OS placeholder the split is inside. A placeholder entry never crosses from one to
        // the next, because the host's range lies between them and a free entry never spans it.
        let host = self.host(start);
        let reservation = self.reservation_at(host);
        let offset = host - reservation.base();
        let piece = vm::split_placeholder(reservation, offset, len)
            .map_err(platform(operation, start, len))?;
        debug_assert_eq!(piece.base(), host, "a split must carve the range it was given");
        self.map.split_bookkeeping(start, at);
        Ok(())
    }

    /// The reservation containing **host** address `address`, which must be one of this space's own.
    fn reservation_at(&self, address: usize) -> &Reservation {
        let index = self.reservations.partition_point(|r| r.end() <= address);
        let reservation = &self.reservations[index];
        assert!(
            reservation.base() <= address,
            "{address:#x} is in none of this space's reservations: it is the host's"
        );
        reservation
    }

    /// Refuse a range that reaches into one the host holds: see [`MemError::HostOwned`].
    fn refuse_host(&self, operation: &'static str, address: GuestAddr, len: usize) -> MemResult<()> {
        match self.map.host_overlapping(address, len) {
            Some((host_start, host_end)) => Err(MemError::HostOwned {
                operation,
                address,
                end: address + len,
                host_start,
                host_end,
            }),
            None => Ok(()),
        }
    }

    /// Make sure an entry boundary exists at `at`, splitting the OS placeholder if the entry is one.
    ///
    /// A range the host holds is never split: a boundary asked for inside one is not made, and
    /// every path that would then act on the pieces refuses such a range first
    /// ([`refuse_host`](Self::refuse_host)) or skips entries it does not wholly cover.
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
        if entry.is_host() {
            return Ok(());
        }
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

    /// Refuse a commit that would break either ceiling.
    ///
    /// Called from [`Inner::commit_range`] immediately before the `vm::commit_placeholder` that would
    /// spend the charge, which is the only place in this crate that spends any. Put here rather than
    /// in a caller because every caller — the ELF loader today, the CPU backend's per-thread code
    /// caches at M2 — needs the same bound, and a bound enforced by each caller separately is a bound
    /// the next caller forgets.
    fn check_commit_allowed(
        &self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
    ) -> MemResult<()> {
        if len > self.max_commit_request {
            return Err(MemError::CommitRequestTooLarge {
                operation,
                address,
                requested: len,
                limit: self.max_commit_request,
            });
        }
        // `committed` never exceeds the space size and `len` never exceeds `max_commit_request`, so
        // this cannot overflow; `checked_add` says so rather than relying on it.
        let would_total = self.committed.checked_add(len).ok_or(MemError::CommitCeiling {
            operation,
            address,
            requested: len,
            committed: self.committed,
            would_total: usize::MAX,
            limit: self.max_committed,
        })?;
        if would_total > self.max_committed {
            return Err(MemError::CommitCeiling {
                operation,
                address,
                requested: len,
                committed: self.committed,
                would_total,
                limit: self.max_committed,
            });
        }
        Ok(())
    }

    /// Commit the granules covering `[address, address + len)` that are not committed yet.
    /// Whether [`commit_range`](Self::commit_range) of this range would commit anything: some
    /// entry in it is a mapping's placeholder, accessible.
    fn needs_commit(&self, address: GuestAddr, len: usize) -> bool {
        let end = address + len;
        let mut position = address;
        while position < end {
            let Some(start) = self.map.entry_start(position) else { return false };
            let Some(entry) = self.map.get(start) else { return false };
            if entry.os == OsState::Placeholder && entry.owner.as_ref().is_some_and(|o| o.protection != Protection::None) {
                return true;
            }
            position = start + entry.len;
        }
        false
    }

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

            // The ceilings, checked **before** the kernel call and before the placeholder is
            // carved, because after either of those the charge has already been taken or the map has
            // already been changed. Both are plain comparisons on `usize` with no saturation
            // anywhere: a saturating limit converts hostile input into a larger permission.
            self.check_commit_allowed(operation, from, to - from)?;

            self.make_exact_placeholder(operation, from, to - from, false)?;
            // SAFETY: `[from, to)` is now exactly one unreplaced placeholder piece, which is the
            // contract of `commit_placeholder`. It is inside this process's reservation and no
            // reference into it exists: nothing has been able to touch it, because a placeholder is
            // inaccessible.
            unsafe { vm::commit_placeholder(self.host(from) as *mut u8, to - from, owner.protection) }
                .map_err(platform(operation, from, to - from))?;
            self.map
                .get_mut(from)
                .expect("entry vanished")
                .os = OsState::Private { idle: false };
            self.committed += to - from;
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
        unsafe { vm::map_file(backing.file(), file_offset, len, self.host(address) as *mut u8, create_with) }
            .map_err(platform(operation, address, len))?;
        if protection == Protection::None {
            // SAFETY: the range is a live view this process owns, just created above.
            unsafe { vm::protect(self.host(address) as *mut u8, len, Protection::None) }
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
                // `protect` refuses a range that reaches one; nothing to record on it either way.
                OsState::Host => continue,
                // An uncommitted granule has no pages to protect; it records the protection and is
                // committed with it when something reaches it.
                OsState::Placeholder => {}
                OsState::Private { .. } | OsState::View { .. } => {
                    // SAFETY: the range is committed private memory or a live view this process
                    // owns, and it is homogeneous — one entry is one OS state, so this never spans
                    // both, which `protect` requires.
                    unsafe { vm::protect(self.host(start) as *mut u8, entry_len, protection) }
                        .map_err(platform(operation, start, entry_len))?;
                }
            }
            let entry = self.map.get_mut(start).expect("entry vanished");
            let shared = entry
                .owner
                .as_ref()
                .and_then(|owner| owner.backing.as_ref())
                .is_some_and(|backing| backing.is_shared());
            if protection.is_writable() && matches!(entry.os, OsState::View { .. }) && !shared {
                // From here on, this range may hold copy-on-write content that is not in the file,
                // and `unmap` has to preserve it across the re-map a partial unmap requires. Sticky:
                // lowering the protection again does not un-privatise a page that was written.
                // A view of a shared backing is excluded for `map_file`'s reason: its writes are
                // in the file, not in private pages.
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
                    unsafe { vm::decommit_to_placeholder(self.host(start) as *mut u8, entry.len) }
                        .map_err(platform(operation, start, entry.len))?;
                    self.committed -= entry.len;
                    self.map.free_range(start, entry.len);
                    position = entry_end;
                }
                OsState::View { view } => {
                    position = self.unmap_view(operation, view, start, address, end)?;
                }
                // Never the space's to unmap. `unmap` refuses such a range before it gets here;
                // an internal caller that reaches one leaves it as it is.
                OsState::Host => position = entry_end,
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
    /// # What a failure costs, at each of the three stages
    ///
    /// The comparison runs **before** anything is unmapped, so a failure *there* leaves the view
    /// untouched. That is the only stage with that property, and this comment used to claim it for the
    /// whole operation.
    ///
    /// Once the view is unmapped there is no way back, because the pages that held the privatised
    /// content are gone: the OS destroyed them with the view. So the two later stages are best-effort
    /// and loud rather than transactional.
    ///
    /// * **A survivor cannot be re-mapped.** Its preserved content is unrecoverable. The remaining
    ///   survivors are still attempted, so the loss is confined to that one piece, every lost range is
    ///   logged at `error` level, and the call returns
    ///   [`MemError::UnmapEmulationLostContent`] naming how many ranges and bytes went. The region map
    ///   stays truthful — the range reads as free placeholder, which is what it is.
    /// * **A survivor is re-mapped but its content cannot be protected back.** The content is intact
    ///   and the range is left *writable*, so the region map is updated to record `ReadWrite` before
    ///   the error is returned. A map that claimed the old protection would be a map that lies about
    ///   protection, which is exactly what `validate` exists to prevent.
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
                unsafe { vm::protect(self.host(survivor.start) as *mut u8, survivor.len, Protection::Read) }
                    .map_err(platform(operation, survivor.start, survivor.len))?;
            }
            survivor.preserved = scan_for_copy_on_write(
                operation,
                self.window,
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
        unsafe { vm::unmap(self.host(view_start) as *mut u8, view_len) }
            .map_err(platform(operation, view_start, view_len))?;
        self.map.replace(view_start, view_len, Entry::free(view_len));

        let mut first_error: Option<MemError> = None;
        let mut lost_ranges = 0usize;
        let mut lost_bytes = 0usize;
        for survivor in &survivors {
            let preserved_bytes: usize =
                survivor.preserved.iter().map(|run| run.bytes.len()).sum();
            match self.remap_survivor(operation, survivor) {
                Ok(()) => {
                    if let Err(failure) = restore_copy_on_write(
                        operation,
                        self.window,
                        &survivor.preserved,
                        survivor.owner.protection,
                    ) {
                        for &(address, len) in &failure.left_writable {
                            tracing::error!(
                                address = format_args!("{address:#x}"),
                                len,
                                wanted = %survivor.owner.protection,
                                "a re-mapped range could not be protected back and is left \
                                 writable; the region map now records that rather than what was \
                                 asked for"
                            );
                            self.record_protection(operation, address, len, Protection::ReadWrite);
                        }
                        if first_error.is_none() {
                            first_error = Some(failure.source);
                        }
                    }
                }
                Err(error) => {
                    // The view is already gone, so the privatised pages this piece held no longer
                    // exist anywhere. Nothing can recover them; the remaining survivors are still
                    // attempted so the loss stays confined to this one.
                    lost_ranges += 1;
                    lost_bytes += preserved_bytes;
                    tracing::error!(
                        %error,
                        address = format_args!("{:#x}", survivor.start),
                        len = survivor.len,
                        preserved_bytes,
                        "a surviving piece of a partially unmapped view could not be mapped again; \
                         any copy-on-write content it held is unrecoverable"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(source) = first_error {
            if lost_ranges > 0 {
                return Err(MemError::UnmapEmulationLostContent {
                    operation,
                    view_start,
                    view_len,
                    lost_ranges,
                    lost_bytes,
                    source: Box::new(source),
                });
            }
            return Err(source);
        }
        // Everything of this view that fell inside the request is free now, so the unmap walk
        // continues past it. The view may have extended beyond the request in either direction;
        // those parts have been mapped again and must not be revisited.
        Ok(view_end.min(keep_out_end))
    }

    /// Map one surviving piece of a partially-unmapped view again, at the same address.
    fn remap_survivor(&mut self, operation: &'static str, survivor: &Survivor) -> MemResult<()> {
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
        Ok(())
    }

    /// Record a protection the OS already has, without calling the OS.
    ///
    /// For the one case where the two have diverged and the OS is right: a protect that failed on the
    /// way *back* from a copy-on-write write-back left the range writable. Boundary changes here are
    /// bookkeeping splits of a live view, so they cannot fail.
    fn record_protection(
        &mut self,
        operation: &'static str,
        address: GuestAddr,
        len: usize,
        protection: Protection,
    ) {
        let _ = self.ensure_boundary(operation, address);
        let _ = self.ensure_boundary(operation, address + len);
        for start in self.map.starts_overlapping(address, len) {
            let entry = self.map.get_mut(start).expect("entry vanished");
            if start < address || start + entry.len > address + len {
                continue;
            }
            if let Some(owner) = entry.owner.as_mut() {
                owner.protection = protection;
            }
        }
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

    /// Zero `[at, at + len)`, which lies inside one page, **where it is committed private memory**,
    /// and return how many bytes were written. See [`GuestSpace::discard`].
    ///
    /// Entry boundaries are page-aligned, so one entry covers the whole page. An uncommitted page
    /// already reads zero, free address space has nothing to zero, and a file view is left as
    /// `advise_idle` leaves it; each of those is zero bytes.
    fn zero_in_place(
        &mut self,
        operation: &'static str,
        at: GuestAddr,
        len: usize,
    ) -> MemResult<usize> {
        let page_start = at & !(self.page - 1);
        debug_assert!(at + len <= page_start + self.page, "a partial piece crosses a page");
        let Some(start) = self.map.entry_start(at) else { return Ok(0) };
        let entry = self.map.get(start).expect("entry vanished");
        debug_assert!(start <= page_start && page_start + self.page <= start + entry.len);
        let (OsState::Private { .. }, Some(owner)) = (&entry.os, entry.owner.as_ref()) else {
            return Ok(0);
        };
        let protection = owner.protection;
        let raise = !protection.is_writable();
        if raise {
            // SAFETY: the page is committed private memory this process owns (the map says so,
            // under its lock); a protection change dereferences nothing.
            unsafe { vm::protect(self.host(page_start) as *mut u8, self.page, Protection::ReadWrite) }
                .map_err(platform(operation, page_start, self.page))?;
        }
        // SAFETY: `[at, at + len)` is inside that committed page, which is writable now. The guest
        // has said it no longer needs these bytes; the bytes beside them are not written.
        unsafe { std::ptr::write_bytes(self.host(at) as *mut u8, 0, len) };
        if raise {
            // SAFETY: as above; this puts back the protection the map records.
            unsafe { vm::protect(self.host(page_start) as *mut u8, self.page, protection) }
                .map_err(platform(operation, page_start, self.page))?;
        }
        Ok(len)
    }

    /// Decommit each idle `(start, len)` entry to a placeholder, counting it into `reclaimed`.
    fn decommit_idle(&mut self, idle: Vec<(GuestAddr, usize)>, reclaimed: &mut Reclaimed) -> MemResult<()> {
        for (start, len) in idle {
            // SAFETY: the range is private committed memory this process owns and the guest has
            // said it no longer needs the contents, so nothing may hold a reference into it.
            // `MEM_DECOMMIT` is the only primitive that returns commit charge (D10).
            unsafe { vm::decommit_to_placeholder(self.host(start) as *mut u8, len) }
                .map_err(platform("reclaim_idle", start, len))?;
            let entry = self.map.get_mut(start).expect("entry vanished");
            entry.os = OsState::Placeholder;
            self.committed -= len;
            reclaimed.bytes += len;
            reclaimed.granules += 1;
        }
        Ok(())
    }

    /// The idle entries overlapping `[address, address + len)`, decommitted. See
    /// [`GuestSpace::reclaim_idle_in`].
    fn reclaim_in(&mut self, address: GuestAddr, len: usize) -> MemResult<Reclaimed> {
        let mut reclaimed = Reclaimed::default();
        let idle: Vec<(GuestAddr, usize)> = self
            .map
            .starts_overlapping(address, len)
            .into_iter()
            .filter_map(|start| {
                let entry = self.map.get(start)?;
                matches!(entry.os, OsState::Private { idle: true }).then_some((start, entry.len))
            })
            .collect();
        self.decommit_idle(idle, &mut reclaimed)?;
        Ok(reclaimed)
    }

    fn reclaim(&mut self) -> MemResult<Reclaimed> {
        let mut reclaimed = Reclaimed::default();

        let idle: Vec<(GuestAddr, usize)> = self
            .map
            .iter()
            .filter(|(_, entry)| matches!(entry.os, OsState::Private { idle: true }))
            .map(|(start, entry)| (start, entry.len))
            .collect();
        self.decommit_idle(idle, &mut reclaimed)?;


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
            unsafe { vm::coalesce_placeholders(self.host(start) as *mut u8, len) }
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

    fn labelled_regions(&self) -> Vec<(RegionInfo, crate::MapLabel)> {
        let mut out: Vec<(RegionInfo, crate::MapLabel)> = Vec::new();
        for (start, entry) in self.map.iter() {
            let Some(owner) = entry.owner.as_ref() else { continue };
            let info = RegionInfo::from_entry(start, entry);
            // `can_absorb` requires the same mapping, and so the same label.
            match out.last_mut() {
                Some((previous, _)) if previous.can_absorb(&info) => previous.absorb(&info),
                _ => out.push((info, owner.label)),
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
            // The host's ranges are neither free nor the guest's: in `reserved`, and nowhere else.
            if entry.is_free() || entry.is_host() {
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
                // The host's: not this space's to tear down.
                OsState::Placeholder | OsState::Host => {}
                OsState::Private { .. } => {
                    // SAFETY: private committed memory this process owns, produced by
                    // `commit_placeholder`, being torn down; the space is going away, so nothing
                    // may hold a reference into it.
                    let result =
                        unsafe { vm::decommit_to_placeholder(self.host(start) as *mut u8, entry.len) };
                    if let Err(error) = result {
                        fail(platform("close", start, entry.len)(error));
                    }
                    self.committed -= entry.len;
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
                    if let Err(error) = unsafe { vm::unmap(self.host(start) as *mut u8, view_len) } {
                        fail(platform("close", start, view_len)(error));
                    }
                    self.map.replace(start, view_len, Entry::free(view_len));
                }
            }
            position = entry_end.max(position + self.page);
        }

        // Steps 2 and 3 run once per reservation: one for an ordinary space, one per free range
        // between the host's for a space reserved around them. A coalesce or release never
        // crosses from one to the next, and the host's ranges between them are never touched.
        let reservations = self.reservations.clone();
        for reservation in reservations {
            // The map is the guest's; the reservation is the host's (they differ in a window).
            let host_base = reservation.base();
            let base = self.window.and_then(|w| w.guest(host_base)).unwrap_or(host_base);
            let len = reservation.len();

            // 2. One placeholder again. Coalescing is only legal — and only necessary — when the
            //    reservation was actually split; a coalesce of a range holding a single placeholder
            //    fails with 487.
            if self.map.starts_overlapping(base, len).len() > 1 {
                // SAFETY: after step 1 the whole reservation is placeholders this process owns.
                match unsafe { vm::coalesce_placeholders(host_base as *mut u8, len) } {
                    Ok(()) => self.map.replace(base, len, Entry::free(len)),
                    Err(error) => fail(platform("close", base, len)(error)),
                }
            }

            // 3. Release. If the coalesce failed the reservation is still fragmented, so fall back
            //    to releasing each placeholder individually rather than leaking all of it.
            if self.map.starts_overlapping(base, len).len() == 1 {
                if let Err(error) = vm::release(reservation) {
                    fail(platform("close", base, len)(error));
                }
            } else {
                let pieces: Vec<(GuestAddr, usize)> = self
                    .map
                    .starts_overlapping(base, len)
                    .into_iter()
                    .map(|start| (start, self.map.get(start).expect("entry vanished").len))
                    .collect();
                for (start, piece_len) in pieces {
                    let offset = start - base;
                    match reservation.subrange(offset, piece_len, vm::ReservationKind::Placeholder) {
                        Ok(piece) => {
                            if let Err(error) = vm::release(piece) {
                                fail(platform("close", start, piece_len)(error));
                            }
                        }
                        Err(error) => fail(platform("close", start, piece_len)(error)),
                    }
                }
            }
        }
        for reservation in std::mem::take(&mut self.held) {
            let (base, len) = (reservation.base(), reservation.len());
            if let Err(error) = vm::release(reservation) {
                fail(platform("close", base, len)(error));
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// What [`reserve_around_host`] reserved: the placeholders, in address order, and the host's ranges
/// between them as `(start, len)`.
type AroundHost = (Vec<Reservation>, Vec<(GuestAddr, usize)>);

/// How many times [`reserve_around_host`] asks the host again after the host took part of the
/// range between the question and the reservation.
const AROUND_HOST_ATTEMPTS: usize = 4;

/// Reserve a space with a [`LowWindow`] (D41): the window's part (`[base, LOW_WINDOW_END - page)`)
/// at `W + g` for a 4 GiB-aligned `W` the host chooses, the page below `LOW_WINDOW_END` as a
/// host-owned guard, and the rest of `size` above 4 GiB as the identity: at 4 GiB itself when the
/// host has that much free there, else wherever the host chooses (`H`), `[4 GiB, H)` then being one
/// host-owned range. Returns the reservations sorted by host address, the host's ranges by guest
/// address, the window, and the space's length (`H + rest - base`, or `size`).
///
/// Why the identity part moves: on macOS nearly everything from the executable up to ~448 GiB is
/// the host's -- the dyld shared region's reservation and the GPU carveout (MEASURED on the M1,
/// macOS 27: fixed allocations at 16, 64 and 128 GiB are `KERN_NO_SPACE`, and the host's own choice
/// for 64 GiB was `0x7b_2640_0000`). The guest does not care where its high memory is: what it
/// places without an address goes to the lowest free range above 4 GiB (`omni-linux`'s `mm`).
/// What [`reserve_with_window`] reserved.
struct WithWindow {
    /// The space's own reservations, sorted by host address.
    reservations: Vec<Reservation>,
    /// The host's ranges inside the space, by guest address.
    hosts: Vec<(GuestAddr, usize)>,
    window: LowWindow,
    /// The space's length.
    len: usize,
    /// The window's 4 GiB outside the space's part of it, held inaccessible.
    held: Vec<Reservation>,
}

fn reserve_with_window(base: GuestAddr, size: usize, page: usize) -> MemResult<WithWindow> {
    const OP: &str = "GuestSpace::with_config";
    let end = base.checked_add(size).ok_or(MemError::InvalidConfig {
        field: "size",
        value: size as u64,
        reason: "runs past the end of the address space",
    })?;
    let guard = LOW_WINDOW_END - page;
    if base >= guard {
        return Err(MemError::InvalidConfig {
            field: "base",
            value: base as u64,
            reason: "a low window needs a base below 4 GiB",
        });
    }
    let low_end = end.min(guard);
    let low_len = low_end - base;

    let mut attempt = 0;
    let (low, delta, held) = loop {
        attempt += 1;
        // Where the host has 4 GiB free, found by reserving it and giving it back; then the window's
        // three parts of it by address -- below the base, the space's, and from its end up -- the
        // first and last held inaccessible, so a guest null pointer (and anything near 0 or the
        // seam) reaches nothing of the host's and faults as on a device. Something else can take
        // part of it in between: give back what was got and ask again.
        let probe = vm::reserve_placeholder(LOW_WINDOW_END, LOW_WINDOW_END)
            .map_err(platform(OP, base, LOW_WINDOW_END))?;
        let delta = probe.base();
        vm::release(probe).map_err(platform(OP, delta, LOW_WINDOW_END))?;
        let parts = [(delta, base), (delta + base, low_len), (delta + low_end, LOW_WINDOW_END - low_end)];
        let mut got = Vec::new();
        let mut failed = None;
        for (at, len) in parts.into_iter().filter(|&(_, len)| len > 0) {
            match vm::reserve_placeholder_at(at, len) {
                Ok(r) => got.push(r),
                Err(error) => {
                    failed = Some(platform(OP, at, len)(error));
                    break;
                }
            }
        }
        match failed {
            None => {
                let low = got.remove(usize::from(base > 0));
                break (low, delta, got);
            }
            Some(error) => {
                for r in got {
                    let _ = vm::release(r);
                }
                if attempt >= AROUND_HOST_ATTEMPTS {
                    return Err(error);
                }
                tracing::debug!(%error, attempt, "the host took part of the window's range; asking again");
            }
        }
    };
    let window = LowWindow { start: base, end: LOW_WINDOW_END, delta };

    let mut reservations = vec![low];
    let mut hosts = Vec::new();
    let mut len = size;
    if end > guard {
        hosts.push((guard, end.min(LOW_WINDOW_END) - guard));
    }
    if end > LOW_WINDOW_END {
        let high_len = end - LOW_WINDOW_END;
        let high = vm::reserve_placeholder_at(LOW_WINDOW_END, high_len).or_else(|_| {
            // Not free at 4 GiB (macOS): the host's choice, 4 GiB-aligned.
            vm::reserve_placeholder(high_len, LOW_WINDOW_END)
        });
        match high {
            Ok(high) => {
                let at = high.base();
                if at > LOW_WINDOW_END {
                    hosts.push((LOW_WINDOW_END, at - LOW_WINDOW_END));
                    len = at + high_len - base;
                }
                reservations.push(high);
            }
            Err(error) => {
                for reservation in reservations.drain(..).chain(held) {
                    if let Err(release) = vm::release(reservation) {
                        tracing::error!(%release, "could not give back a window's reservation");
                    }
                }
                return Err(platform(OP, LOW_WINDOW_END, high_len)(error));
            }
        }
    }
    reservations.sort_by_key(Reservation::base);
    tracing::debug!(delta = format_args!("{delta:#x}"), low_len, len, "reserved a low window");
    Ok(WithWindow { reservations, hosts, window, len, held })
}

/// Reserve `[base, base + size)` around what the host holds in it: one placeholder per free range,
/// and the host's ranges, `(start, len)`, for the region map. See
/// [`GuestSpaceConfig::around_host`].
fn reserve_around_host(
    base: GuestAddr,
    size: usize,
    page: usize,
) -> MemResult<AroundHost> {
    const OP: &str = "GuestSpace::with_config";
    let granule = vm::allocation_granularity().max(page);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let occupied = vm::occupied_ranges(base, size).map_err(platform(OP, base, size))?;
        let hosts = host_holes(base, size, &occupied, granule);
        let pieces = free_between(base, size, &hosts);
        if pieces.is_empty() {
            return Err(MemError::InvalidConfig {
                field: "base",
                value: base as u64,
                reason: "is the start of a range the host holds all of; there is nothing to reserve \
                         around",
            });
        }
        let mut reserved: Vec<Reservation> = Vec::with_capacity(pieces.len());
        let mut failure = None;
        for &(at, len) in &pieces {
            match vm::reserve_placeholder_at(at, len) {
                Ok(reservation) => reserved.push(reservation),
                Err(error) => {
                    failure = Some(platform(OP, at, len)(error));
                    break;
                }
            }
        }
        let Some(error) = failure else {
            if !hosts.is_empty() {
                tracing::debug!(
                    base = format_args!("{base:#x}"),
                    size,
                    ?hosts,
                    "reserved a guest address space around the host's ranges"
                );
            }
            return Ok((reserved, hosts));
        };
        // Give back what this attempt got, whatever happens next: a piece left reserved would be
        // address space nothing owns.
        for reservation in reserved {
            if let Err(release) = vm::release(reservation) {
                tracing::error!(%release, "could not give back part of a failed reservation");
            }
        }
        if attempt >= AROUND_HOST_ATTEMPTS {
            return Err(error);
        }
        tracing::debug!(%error, attempt, "the host took part of the range; asking again");
    }
}

/// The host's ranges inside `[base, base + size)`, as the region map records them: each occupied
/// `(start, end)` rounded out to `granule` -- the rest of a granule the host has used cannot be
/// reserved -- clipped to the space, and merged where they meet. `(start, len)`, in address order.
fn host_holes(
    base: GuestAddr,
    size: usize,
    occupied: &[(usize, usize)],
    granule: usize,
) -> Vec<(GuestAddr, usize)> {
    debug_assert!(granule.is_power_of_two());
    let end = base + size;
    let mut holes: Vec<(GuestAddr, GuestAddr)> = Vec::new();
    for &(start, stop) in occupied {
        let from = (start & !(granule - 1)).max(base);
        let to = stop.checked_next_multiple_of(granule).unwrap_or(usize::MAX).min(end);
        if from >= to {
            continue;
        }
        match holes.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => holes.push((from, to)),
        }
    }
    holes.into_iter().map(|(from, to)| (from, to - from)).collect()
}

/// The ranges of `[base, base + size)` between `holes` (sorted, disjoint), as `(start, len)`.
fn free_between(base: GuestAddr, size: usize, holes: &[(GuestAddr, usize)]) -> Vec<(GuestAddr, usize)> {
    let mut pieces = Vec::new();
    let mut cursor = base;
    for &(start, len) in holes {
        if start > cursor {
            pieces.push((cursor, start - cursor));
        }
        cursor = start + len;
    }
    if cursor < base + size {
        pieces.push((cursor, base + size - cursor));
    }
    pieces
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
    low_window: Option<LowWindow>,
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
                    std::slice::from_raw_parts(host_of(low_window, live + position) as *const u8, step),
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

/// What a failed write-back left behind.
struct RestoreFailure {
    /// The first failure, which is what the caller propagates.
    source: MemError,
    /// Ranges that are now *writable* although the caller asked for something else, because the
    /// protect on the way back failed. The caller must record this in the region map: a map that
    /// claims a protection the OS does not have is the thing `validate` exists to prevent.
    left_writable: Vec<(GuestAddr, usize)>,
}

/// Write preserved copy-on-write content back into a re-mapped survivor.
///
/// Each run is made writable, written, and returned to the protection the piece is recorded with.
/// Writing through `Protection::ReadWrite` on a private file view is a copy-on-write write, so this
/// privatises exactly the pages that were privatised before and leaves every clean page shared.
///
/// Best-effort across runs: the content is only in these heap buffers, so stopping at the first
/// failure would throw away the runs after it. Every run is attempted, the first error is kept, and
/// any range left writable is reported so the map can be told the truth.
fn restore_copy_on_write(
    operation: &'static str,
    window: Option<LowWindow>,
    preserved: &[Dirty],
    protection: Protection,
) -> Result<(), Box<RestoreFailure>> {
    let mut source: Option<MemError> = None;
    let mut left_writable: Vec<(GuestAddr, usize)> = Vec::new();
    for run in preserved {
        let len = run.bytes.len();
        // SAFETY: the range is part of a live view this process has just mapped, and is page-aligned
        // and a whole number of pages because every mapping length here is.
        if let Err(error) = unsafe { vm::protect(host_of(window, run.address) as *mut u8, len, Protection::ReadWrite) }
        {
            // Nothing was written and nothing was changed, so this run is still at the protection the
            // map records; only the content is lost.
            if source.is_none() {
                source = Some(platform(operation, run.address, len)(error));
            }
            continue;
        }
        // SAFETY: the range is now writable and `len` bytes long, and the source is a heap buffer
        // that cannot overlap a mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(run.bytes.as_ptr(), host_of(window, run.address) as *mut u8, len);
        }
        // SAFETY: as above. Restoring the recorded protection keeps the region map truthful — and
        // when it fails, the map has to be corrected instead, which is what `left_writable` is for.
        if let Err(error) = unsafe { vm::protect(host_of(window, run.address) as *mut u8, len, protection) } {
            left_writable.push((run.address, len));
            if source.is_none() {
                source = Some(platform(operation, run.address, len)(error));
            }
        }
    }
    match source {
        Some(source) => Err(Box::new(RestoreFailure { source, left_writable })),
        None => Ok(()),
    }
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
    use super::{free_between, host_holes, subtract};

    #[test]
    fn host_ranges_are_rounded_out_to_the_granule_clipped_and_merged() {
        const G: usize = 0x1_0000;
        let base = 0x1000_0000;
        let size = 0x100_0000;
        // KUSER_SHARED_DATA's shape: one 4 KiB page, which costs the whole 64 KiB granule.
        assert_eq!(
            host_holes(base, size, &[(base + 0x7_0000, base + 0x7_1000)], G),
            vec![(base + 0x7_0000, G)]
        );
        // Two in one granule and one in the next become one range; one straddling each end of
        // the space is clipped to it.
        let occupied = [
            (base - 0x1000, base + 0x1000),
            (base + 0x10_2000, base + 0x10_3000),
            (base + 0x10_8000, base + 0x11_1000),
            (base + size - 0x1000, base + size + 0x5000),
        ];
        let holes = host_holes(base, size, &occupied, G);
        assert_eq!(holes, vec![(base, G), (base + 0x10_0000, 2 * G), (base + size - G, G)]);
        assert_eq!(
            free_between(base, size, &holes),
            vec![(base + G, 0x10_0000 - G), (base + 0x12_0000, size - 0x12_0000 - G)]
        );
        // Nothing held: the whole space is one free range.
        assert_eq!(host_holes(base, size, &[], G), Vec::new());
        assert_eq!(free_between(base, size, &[]), vec![(base, size)]);
    }

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

/// The address a placement names, if it names one exactly.
fn placement_address(placement: Placement) -> Option<GuestAddr> {
    match placement {
        Placement::Fixed(address) => Some(address),
        Placement::Hint { .. } | Placement::Anywhere { .. } => None,
    }
}
