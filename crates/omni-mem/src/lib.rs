//! Guest address-space management: reservation, lazy commit, decommit, fixed-address mapping, and
//! the dual-mapped JIT code arena. Built entirely on [`omni_platform::vm`]; there is no OS call and
//! no `cfg(target_os)` in this crate (Global Constraint 4).
//!
//! # The requirement this crate exists to satisfy
//!
//! Isolated guest address spaces, no large fixed RAM reservation, demand-driven usage, genuinely
//! reclaimable, and many instances without a huge pagefile. D10 established that this is achievable
//! and exactly how, by measurement:
//!
//! * **Address space is free.** A reservation costs 0 bytes of commit charge and 0 bytes of working
//!   set, verified to 97.7 TB. So [`GuestSpace`] reserves generously and never economizes here.
//! * **Commit charge is scarce**, and it is debited at commit, not at first touch: 1024 MB committed
//!   and never touched measured 1026.66 MB of commit charge against a 4.68 MB working set. So
//!   nothing here commits speculatively, and commit happens in granules as the guest arrives
//!   (see [`DEFAULT_COMMIT_GRANULE`] for the measurements that set the granule).
//! * **Only `MEM_DECOMMIT` reclaims.** `MEM_RESET`, `DiscardVirtualMemory`, `OfferVirtualMemory` and
//!   `EmptyWorkingSet` each measured **exactly 0.00 MB** of commit charge returned. `MEM_RESET` is
//!   also the cheapest of them, at 32.5 ns/page, so a reclamation path built on it would be fast,
//!   look correct in every functional test, and free nothing at all. [`GuestSpace::unmap`] and
//!   [`GuestSpace::reclaim_idle`] decommit, and the tests assert on measured commit charge rather
//!   than on a successful return.
//!
//! # What is here
//!
//! * [`GuestSpace`] — one instance's address space: [`map_anonymous`](GuestSpace::map_anonymous),
//!   [`map_file`](GuestSpace::map_file), [`unmap`](GuestSpace::unmap),
//!   [`protect`](GuestSpace::protect), [`ensure_committed`](GuestSpace::ensure_committed),
//!   [`reclaim_idle`](GuestSpace::reclaim_idle) and the [`regions`](GuestSpace::regions)
//!   enumeration.
//! * [`CodeArena`] — the JIT code arena. One writable view and one executable view of the same
//!   pages, so that emitting code never requires a page to be writable and executable at the same
//!   time (D12).
//! * [`CommitBudget`] — what an instance is costing. It exists because the arena's memory is
//!   *shared* commit, so it is charged against the system commit limit while being invisible to
//!   `process_commit_charge`, which is the counter everything else is budgeted against. Measured:
//!   a 4 MiB section mapped twice, every page written, moved that counter by 20480 bytes.
//!
//! # Two things that are not obvious and are load-bearing
//!
//! **Windows cannot partially unmap a view, and Android guests do partial `munmap`.** A view is
//! unmapped from its base address with no length, so [`GuestSpace::unmap`] emulates a partial unmap:
//! it unmaps the whole view and maps the surviving head and tail again from the same file at the
//! same addresses. Head, tail and a hole through the middle are all handled, and each surviving
//! piece keeps its own protection.
//!
//! **A placeholder cannot be split arbitrarily.** Measured: splitting a range that is already
//! exactly one placeholder fails (487), splitting a range that spans two adjacent placeholders fails
//! (87), and merging a range that holds only one placeholder fails (487). So the region map tracks
//! placeholder extents exactly and every boundary change is paired with the matching kernel call.
//! `crate::entry` documents the resulting invariant.

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod access;
mod arena;
mod backing;
mod budget;
mod entry;
mod error;
mod pager;
mod region;
mod space;

pub use arena::{
    ArenaConfig, ArenaId, ArenaStats, CodeArena, CodeBlock, DEFAULT_BLOCK_ALIGNMENT,
    DEFAULT_CHUNK_SIZE, DEFAULT_MAX_TOTAL,
};
pub use backing::{Backing, BackingId};
pub use budget::CommitBudget;
pub use error::{MemError, MemResult};
pub use access::{admit, admits_region, permits, scan_reach, Admitted, Refusal};
pub use pager::{DemandPager, PagerStats};
pub use region::{RegionInfo, RegionKind};
pub use space::{
    CommitPolicy, GuestAddr, GuestSpace, GuestSpaceConfig, MappingId, Placement, Reclaimed,
    SpaceStats, DEFAULT_COMMIT_GRANULE, DEFAULT_MAX_COMMITTED, DEFAULT_MAX_COMMIT_REQUEST,
    DEFAULT_SPACE_SIZE,
};

/// Re-exported from `omni-platform` so that callers do not need to depend on it directly to name a
/// protection. There is deliberately no writable-and-executable variant.
pub use omni_platform::fault::FaultAccess;
pub use omni_platform::vm::{MapExecutability, Protection};

/// This process's commit charge and working set, re-exported.
///
/// They are here so that a crate which only needs to *measure* memory does not have to depend on
/// `omni-platform` to do it. That is not a convenience: a crate holding `omni_platform::vm` in scope
/// also holds `vm::protect`, `vm::unmap` and `vm::map_file`, and those are unsafe to call on a guest
/// address without the region map that only this crate has — `vm::protect` in particular bypasses
/// the `ever_writable` gate that makes copy-on-write content survive a partial unmap. `omni-elf`
/// wanted `process_commit_charge` and nothing else, and paid for it with the whole seam in reach.
///
/// Commit charge is the number the memory design budgets against (D10, Global Constraint 6), so
/// tests assert what an operation actually cost rather than assuming; working set is exposed beside
/// it so the difference between *committed* and *touched* can be demonstrated rather than argued.
pub use omni_platform::vm::{process_commit_charge, process_working_set};

/// This process's memory as one snapshot -- the same two numbers as above and the three
/// `/proc/self/statm` needs besides -- re-exported for the reason those two are.
pub use omni_platform::vm::{process_memory, ProcessMemory};
