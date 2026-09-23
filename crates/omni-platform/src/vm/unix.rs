//! Shared unix body of the virtual-memory seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: structural, not implemented
//!
//! **Nothing in this module has ever been run, and nothing in it has been measured.** Omnidroid's
//! memory design was derived from probes on Windows (D10–D12); the equivalent probes on Linux and
//! macOS have not been written, so the numbers that the Windows backend encodes — what costs
//! commit charge, what reclaims it, what granularity each operation really enforces — are simply
//! unknown here.
//!
//! Rather than ship plausible-looking `mmap` code that has never faulted a page, every operation
//! returns [`VmError::Unsupported`], naming itself and the platform. A Linux or macOS build
//! therefore fails at the first virtual-memory call, immediately and with a message that says
//! why, instead of appearing to work. That is a deliberate reading of Global Constraint 1: an
//! unverified implementation of the *one* subsystem the whole design rests on would be worse than
//! an honest gap, because it would be believed.
//!
//! The two operations that are genuinely implemented are [`page_size`] and
//! [`allocation_granularity`], because `sysconf(_SC_PAGESIZE)` is unambiguous and because the
//! seam's shared argument validation needs a real page size to check against.
//!
//! # What implementing this involves
//!
//! Each function below carries the intended POSIX mapping in its doc comment. The shape of the
//! seam fits unix well in most places and badly in two:
//!
//! * **Placeholders have no unix analogue, and that is fine.** `mmap(MAP_FIXED)` can replace any
//!   page-aligned sub-range of an existing mapping directly, so [`reserve_placeholder`] is just
//!   [`reserve`] and [`split_placeholder`] is a no-op. Windows needs the placeholder dance to get
//!   the same effect. Because "no-op" and "not implemented" are indistinguishable to a caller,
//!   both return `Unsupported` here rather than `Ok(())` — an unimplemented backend must not have
//!   any operation that succeeds.
//! * **Commit charge is a Windows concept.** Linux does not debit a per-process commit charge on
//!   `mprotect`; the nearest equivalents are the system-wide `Committed_AS` in `/proc/meminfo`
//!   under `vm.overcommit_memory = 2`, and per-process `VmRSS`. macOS has neither. So
//!   [`process_commit_charge`] cannot be a faithful port, and what the tests written against it
//!   mean on these platforms has to be decided by measurement, not by translation.

use std::path::{Path, PathBuf};

use super::{MapExecutability, Protection, ReservationKind, VmError, VmResult};

/// `EINVAL`: what `mmap` returns for a base address or file offset that is not page-aligned.
pub(super) const MISALIGNED_OS_ERROR: u32 = 22;

/// The platform this backend was compiled for, for error messages.
fn platform() -> &'static str {
    std::env::consts::OS
}

fn unsupported<T>(operation: &'static str) -> VmResult<T> {
    Err(VmError::Unsupported { operation, platform: platform() })
}

/// `sysconf(_SC_PAGESIZE)`. Genuinely implemented: 4096 on Linux, 16384 on Apple silicon.
pub(super) fn page_size() -> usize {
    // SAFETY: sysconf takes an integer name and returns a long; _SC_PAGESIZE is always supported
    // and the call touches no memory.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value > 0 {
        value as usize
    } else {
        // sysconf cannot fail for _SC_PAGESIZE, but a 0 page size would make the seam's shared
        // alignment checks divide by zero, so fall back to the POSIX minimum rather than trap.
        4096
    }
}

/// On unix the mapping granularity is the page size; there is no separate 64 KB rule.
pub(super) fn allocation_granularity() -> usize {
    page_size()
}

/// Intended: `mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0)`,
/// over-reserving and trimming with `munmap` when `align` exceeds the page size.
pub(super) fn reserve(_size: usize, _align: usize) -> VmResult<usize> {
    unsupported("reserve")
}

/// Intended: identical to [`reserve`] — `mmap(MAP_FIXED)` needs no placeholder.
pub(super) fn reserve_placeholder(_size: usize, _align: usize) -> VmResult<usize> {
    unsupported("reserve_placeholder")
}

/// Intended: a no-op, since `mmap(MAP_FIXED)` may replace any page-aligned sub-range.
pub(super) fn split_placeholder(_piece_base: usize, _size: usize) -> VmResult<()> {
    unsupported("split_placeholder")
}

/// Intended: a no-op, as [`split_placeholder`] is — `mmap(MAP_FIXED)` needs no placeholder and
/// therefore never fragments one, so there is nothing to merge back. Returns `Unsupported` for the
/// same reason as [`split_placeholder`]: an unimplemented backend must not have any operation that
/// succeeds.
pub(super) fn coalesce_placeholders(_address: usize, _size: usize) -> VmResult<()> {
    unsupported("coalesce_placeholders")
}

/// Intended: `mprotect(ptr, size, prot)` on a `PROT_NONE` reservation.
pub(super) fn commit(_address: usize, _size: usize, _protection: Protection) -> VmResult<()> {
    unsupported("commit")
}

/// Intended: identical to [`commit`].
pub(super) fn commit_placeholder(
    _address: usize,
    _size: usize,
    _protection: Protection,
) -> VmResult<()> {
    unsupported("commit_placeholder")
}

/// Intended: `madvise(MADV_DONTNEED)` on Linux (`MADV_FREE_REUSABLE` on macOS) followed by
/// `mprotect(PROT_NONE)`, which must be shown to actually return the resource before it is
/// believed — the Windows equivalents `MEM_RESET`, `DiscardVirtualMemory` and `OfferVirtualMemory`
/// all measured as returning exactly 0 bytes (D10), and the unix calls have not been measured.
pub(super) fn decommit(_address: usize, _size: usize) -> VmResult<()> {
    unsupported("decommit")
}

/// Intended: identical to [`decommit`].
pub(super) fn decommit_to_placeholder(_address: usize, _size: usize) -> VmResult<()> {
    unsupported("decommit_to_placeholder")
}

/// Intended: `mprotect(ptr, size, prot)`.
pub(super) fn protect(_address: usize, _size: usize, _protection: Protection) -> VmResult<()> {
    unsupported("protect")
}

/// Intended: `open(path, O_RDONLY)`. There is no section object and no equivalent of the Windows
/// rule that executability must be chosen at open time — `mmap` takes `PROT_EXEC` per view — so
/// [`MapExecutability`] would be recorded and used only to keep the error behaviour identical
/// across platforms. On macOS, hardened runtime and code signing add constraints that have not
/// been investigated.
pub(super) fn open_file_for_mapping(
    _path: &Path,
    _executability: MapExecutability,
) -> VmResult<MappableFile> {
    unsupported("open_file_for_mapping")
}

/// Intended: `mmap(ptr, size, prot, MAP_PRIVATE | MAP_FIXED, fd, file_offset)`, with
/// [`Protection::ReadWrite`] as `MAP_PRIVATE` (copy-on-write, matching the Windows
/// `PAGE_WRITECOPY` view) rather than `MAP_SHARED`.
pub(super) fn map_file(
    _file: &MappableFile,
    _file_offset: u64,
    _size: usize,
    _address: usize,
    _protection: Protection,
) -> VmResult<()> {
    unsupported("map_file")
}

/// Intended: `mmap(ptr, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED)`, which
/// replaces the file mapping with anonymous reserved space and so is the analogue of unmapping
/// while preserving the placeholder.
pub(super) fn unmap(_address: usize, _size: usize) -> VmResult<()> {
    unsupported("unmap")
}

/// Intended: `munmap(ptr, size)`.
pub(super) fn unmap_and_release(_address: usize, _size: usize) -> VmResult<()> {
    unsupported("unmap_and_release")
}

/// Intended: `munmap(base, len)`.
pub(super) fn release(_base: usize, _len: usize, _kind: ReservationKind) -> VmResult<()> {
    unsupported("release")
}

/// `false`: the placeholder path is not implemented here, so this process cannot place a mapping at
/// an address of its choosing. Reported rather than left to be discovered at the first
/// [`map_file`] failure. Nothing is resolved dynamically on unix — `mmap(MAP_FIXED)` is a libc
/// symbol linked at build time — so the answer is a constant rather than a probe, and it is `false`
/// because the operation is missing, not because a symbol is.
pub(super) fn placeholder_api_available() -> bool {
    false
}

/// Empty: no symbol is resolved at runtime on unix, so there is nothing to report the state of.
pub(super) fn placeholder_api_symbols() -> Vec<(&'static str, bool)> {
    Vec::new()
}

/// No faithful equivalent exists; see the module documentation.
pub(super) fn process_commit_charge() -> VmResult<u64> {
    unsupported("process_commit_charge")
}

/// Intended: `VmRSS` from `/proc/self/status` on Linux, `TASK_BASIC_INFO.resident_size` on macOS.
pub(super) fn process_working_set() -> VmResult<u64> {
    unsupported("process_working_set")
}

/// Intended on Linux: the host's own `/proc/self/statm` (every field but `commit_charge`, which has
/// no per-process equivalent -- see the module documentation) and `dl_iterate_phdr`'s first entry
/// for the executable's `PF_X` span. On macOS: `task_info(TASK_VM_INFO)` and the main image's
/// `__TEXT` segment. Neither has been measured, so neither is written.
pub(super) fn process_memory() -> VmResult<super::ProcessMemory> {
    unsupported("process_memory")
}

/// The unix stand-in for a pagefile-backed section.
///
/// Unconstructible: [`create_shared_section`] never returns one.
pub struct SharedSection {
    /// Would hold the descriptor from `memfd_create` on Linux, or from `shm_open` where that is
    /// unavailable. macOS has neither and needs `MAP_JIT`, which is a different design (D12).
    fd: i32,
    len: u64,
}

impl SharedSection {
    pub(super) fn len(&self) -> u64 {
        self.len
    }
}

impl Drop for SharedSection {
    fn drop(&mut self) {
        // SAFETY: unreachable in practice, because no SharedSection can be constructed on this
        // platform. Written out so that an implementation does not silently leak the descriptor.
        unsafe { libc::close(self.fd) };
    }
}

/// Intended on Linux: `memfd_create` plus `ftruncate`, which needs Linux 3.17 or later and is
/// refused an executable mapping by some hardened configurations. On macOS the dual-mapped section
/// has no direct equivalent: `MAP_JIT` with `pthread_jit_write_protect_np` is per-*thread* state
/// rather than a second mapping, behaves quite differently, and needs its own measurement (D12).
pub(super) fn create_shared_section(_size: u64) -> VmResult<SharedSection> {
    unsupported("create_shared_section")
}

/// Intended: `mmap(NULL, size, prot, MAP_SHARED, fd, offset)` — `MAP_SHARED`, not `MAP_PRIVATE`,
/// or writes through the writable view would never reach the executable one.
pub(super) fn map_section(
    _section: &SharedSection,
    _offset: u64,
    _size: usize,
    _protection: Protection,
) -> VmResult<usize> {
    unsupported("map_section")
}

/// The unix stand-in for a file opened for mapping.
///
/// Unconstructible: [`open_file_for_mapping`] never returns one. It exists so that the seam's
/// public [`MappableFile`](super::MappableFile) has an inner type on these targets and the crate
/// compiles.
pub struct MappableFile {
    /// Would hold the file descriptor from `open(2)`, which an implementation would `mmap` from.
    fd: i32,
    len: u64,
    executability: MapExecutability,
    path: PathBuf,
}

impl MappableFile {
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    pub(super) fn executability(&self) -> MapExecutability {
        self.executability
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MappableFile {
    fn drop(&mut self) {
        // SAFETY: unreachable in practice, because no MappableFile can be constructed on this
        // platform. Written out so that an implementation of open_file_for_mapping does not
        // silently leak the descriptor.
        unsafe { libc::close(self.fd) };
    }
}
