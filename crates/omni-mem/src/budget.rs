//! The memory budget, with the part the OS counter cannot see reported alongside the part it can.

use core::fmt;

use omni_platform::vm;

use crate::arena::CodeArena;
use crate::error::{platform, MemResult};
use crate::space::GuestSpace;

/// What one instance is costing, from the two sources that have to be added together.
///
/// # Why this type exists
///
/// [`vm::process_commit_charge`] is the number the whole memory design budgets against (D10), and on
/// Windows it is `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`: **private** committed memory. Measured
/// during Task 2: a 4 MiB pagefile-backed section mapped twice, every page written, moved it by
/// 20480 bytes — page tables and nothing else. A code arena's memory is *shared* commit, so it
/// counts against the system commit limit while being invisible to the counter that is supposed to
/// be watching.
///
/// That gap matters more than it sounds, because the arena is expected to become the
/// fastest-growing consumer: the chosen CPU core commits tens of MiB of code cache per guest thread,
/// and Roblox is heavily multithreaded (D6). A budget that watched only `PrivateUsage` would show a
/// flat line while the system commit limit was being consumed.
///
/// So this type puts the two side by side and makes the total explicit. It is not a replacement for a
/// system-wide figure — `GlobalMemoryStatusEx` would give the real `Committed_AS` equivalent and is
/// not yet on the `omni-platform` seam — but it means the arena's contribution is *instrumented*
/// rather than merely documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitBudget {
    /// Private committed bytes as the OS reports them, from [`vm::process_commit_charge`]. Includes
    /// every guest mapping this process has committed, its page tables, and the Rust heap. Excludes
    /// pagefile-backed sections and file-backed views.
    pub process_private: u64,
    /// Bytes `omni-mem` believes it has committed in the guest address spaces it was given, summed
    /// from [`crate::SpaceStats::committed`]. A cross-check against `process_private` rather than a
    /// second source of truth: it counts what this crate asked for, not what the OS charged.
    pub guest_committed: usize,
    /// Bytes of pagefile-backed section mapped by the code arenas it was given, from
    /// [`crate::ArenaStats::mapped`]. **Not** included in `process_private`, and charged against the
    /// system commit limit in full at the moment each chunk is created.
    pub arena_mapped: usize,
}

impl CommitBudget {
    /// Measure the budget across the guest address spaces and code arenas given.
    ///
    /// # Errors
    ///
    /// [`MemError::Platform`](crate::MemError::Platform) if the OS refuses to report the process's
    /// commit charge.
    pub fn measure<'a, S, A>(spaces: S, arenas: A) -> MemResult<Self>
    where
        S: IntoIterator<Item = &'a GuestSpace>,
        A: IntoIterator<Item = &'a CodeArena>,
    {
        let process_private =
            vm::process_commit_charge().map_err(platform("CommitBudget::measure", 0, 0))?;
        Ok(Self {
            process_private,
            guest_committed: spaces.into_iter().map(|space| space.stats().committed).sum(),
            arena_mapped: arenas.into_iter().map(|arena| arena.stats().mapped).sum(),
        })
    }

    /// Everything this process is charging against the system commit limit, as far as it can be
    /// known from here: the private figure the OS reports plus the shared sections it leaves out.
    #[must_use]
    pub fn total_system_commit(&self) -> u64 {
        self.process_private + self.arena_mapped as u64
    }

    /// How much of [`total_system_commit`](CommitBudget::total_system_commit) is invisible to
    /// [`vm::process_commit_charge`].
    ///
    /// Zero until the first code block is emitted, and the number to watch once the translator is
    /// running.
    #[must_use]
    pub fn invisible_to_process_counter(&self) -> usize {
        self.arena_mapped
    }
}

impl fmt::Display for CommitBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MIB: f64 = (1024 * 1024) as f64;
        write!(
            f,
            "{:.3} MiB system commit = {:.3} MiB private (of which {:.3} MiB is guest mappings) \
             + {:.3} MiB code arena, which PrivateUsage does not count",
            self.total_system_commit() as f64 / MIB,
            self.process_private as f64 / MIB,
            self.guest_committed as f64 / MIB,
            self.arena_mapped as f64 / MIB,
        )
    }
}
