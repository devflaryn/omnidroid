//! Shared unix body of the virtual-memory seam.
//!
//! # Two halves, and only one of them is a backend
//!
//! * **The seam functions below** (`reserve`, `map_file`, ...) are what the [`macos`](super::macos)
//!   backend re-exports, and on macOS they are still **structural**: every one returns
//!   [`VmError::Unsupported`], naming itself and the platform, because nothing about the macOS
//!   memory model has been measured (16 KiB pages, `MAP_JIT`, no per-process commit accounting).
//!   An unmeasured implementation of the subsystem the whole design rests on would be believed.
//! * **[`posix`]** is the other half: thin, real wrappers over `mmap`, `mprotect`, `munmap`,
//!   `msync` and `fstat`, calls that exist with the same meaning in Linux's and macOS's libc. The
//!   [`linux`](super::linux) backend is built on them and defines every seam function itself,
//!   because the parts that decide *what* to call -- commit accounting, how decommit really
//!   returns memory, the placeholder ledger, `/proc/self` -- are Linux measurements (see that
//!   module), not POSIX.
//!
//! The two operations genuinely implemented here for both targets are [`page_size`] and
//! [`allocation_granularity`], because `sysconf(_SC_PAGESIZE)` is unambiguous and the seam's
//! shared argument validation needs a real page size to check against.
//!
//! # What implementing the macOS half involves
//!
//! Each function below carries the intended POSIX mapping in its doc comment, and the Linux backend
//! is the worked, measured example of all of them. Two things the Linux work established that the
//! macOS work should re-measure rather than assume:
//!
//! * **Placeholders need no kernel object, but they need a ledger.** `mmap(MAP_FIXED)` replaces
//!   any page-aligned sub-range of a mapping, so the Windows placeholder dance has no kernel
//!   counterpart -- but the seam's *validation* (an exact-size placeholder, a release that names a
//!   whole allocation, an unmap that names a whole view, a double release refused) has to come from
//!   somewhere, and on unix nothing in the kernel will refuse a `munmap` of the wrong range.
//! * **Commit charge is a Windows concept with a Linux measurement.** On Linux it is the
//!   `VM_ACCOUNT` charge (`Committed_AS`), and whether a call takes or returns it depends on the
//!   flags of the mapping it acts on (`MAP_NORESERVE`), not on the call. macOS has no such
//!   accounting at all.
// On Linux only the `posix` half and the page size are used: the Linux backend defines every seam
// function itself, so the structural ones below are macOS's alone and are dead code there.
#![cfg_attr(target_os = "linux", allow(dead_code))]

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

/// Intended: keep a duplicate of the caller's descriptor (`fcntl(F_DUPFD_CLOEXEC)`), with the
/// file's length from `fstat`. There is no section object: the sharing is chosen per view, by
/// `map_file` passing `MAP_SHARED` for a file made mappable here.
pub(super) fn share_file_for_mapping(
    _file: std::fs::File,
    _name: &Path,
) -> VmResult<MappableFile> {
    unsupported("share_file_for_mapping")
}

/// Intended: `msync(ptr, size, MS_SYNC)`, which on Linux ends in `vfs_fsync_range` and so needs no
/// separate device flush.
pub(super) fn sync_view(_file: &MappableFile, _address: usize, _size: usize) -> VmResult<()> {
    unsupported("sync_view")
}

/// Intended: `mmap(ptr, size, prot, MAP_PRIVATE | MAP_FIXED, fd, file_offset)`, with
/// [`Protection::ReadWrite`] as `MAP_PRIVATE` (copy-on-write, matching the Windows
/// `PAGE_WRITECOPY` view) rather than `MAP_SHARED` -- and `MAP_SHARED` for a file from
/// [`share_file_for_mapping`].
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

/// `false` (macOS): the placeholder path is not implemented here, so this process cannot place a
/// mapping at an address of its choosing. Reported rather than left to be discovered at the first
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
    /// Whether views are to be `MAP_SHARED`: set by [`share_file_for_mapping`].
    shared: bool,
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

    pub(super) fn is_shared(&self) -> bool {
        self.shared
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

/// Real POSIX primitives, shared by the unix backends that are implemented (Linux today).
///
/// Every function here is one libc call and nothing else: no policy, no bookkeeping, no choice of
/// flags. Each returns the `errno` of a failure as a bare `u32`, and the caller wraps it in the
/// [`VmError`] variant that names *its* operation, because only the caller knows which seam call
/// failed. They exist on Linux and macOS alike with the same meaning, which is the rule this file
/// keeps (the port brief: `unix.rs` is pure POSIX; anything Linux-only lives in `linux.rs`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) mod posix {
    use super::Protection;

    /// This thread's `errno`, as the raw code.
    pub(in super::super) fn errno() -> u32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
    }

    /// The `PROT_*` bits a [`Protection`] means. There is no writable-and-executable row, because
    /// there is no such [`Protection`].
    pub(in super::super) const fn prot_bits(protection: Protection) -> libc::c_int {
        match protection {
            Protection::None => libc::PROT_NONE,
            Protection::Read => libc::PROT_READ,
            Protection::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
            Protection::ReadExecute => libc::PROT_READ | libc::PROT_EXEC,
        }
    }

    /// `mmap(2)`. Returns the mapped address.
    ///
    /// # Safety
    ///
    /// With `MAP_FIXED` in `flags`, whatever is mapped at `[address, address + len)` is replaced
    /// without a trace; the caller must own that range and nothing may hold a reference into it.
    pub(in super::super) unsafe fn mmap(
        address: usize,
        len: usize,
        prot: libc::c_int,
        flags: libc::c_int,
        fd: libc::c_int,
        offset: u64,
    ) -> Result<usize, u32> {
        let Ok(offset) = libc::off_t::try_from(offset) else {
            return Err(libc::EOVERFLOW as u32);
        };
        // SAFETY: the caller's contract; mmap itself dereferences nothing in this process.
        let mapped = unsafe { libc::mmap(address as *mut libc::c_void, len, prot, flags, fd, offset) };
        if mapped == libc::MAP_FAILED {
            Err(errno())
        } else {
            Ok(mapped as usize)
        }
    }

    /// `mprotect(2)`.
    ///
    /// # Safety
    ///
    /// The caller owns `[address, address + len)`; lowering a protection under a live reference
    /// turns that reference's next use into a fault.
    pub(in super::super) unsafe fn mprotect(
        address: usize,
        len: usize,
        prot: libc::c_int,
    ) -> Result<(), u32> {
        // SAFETY: the caller's contract.
        if unsafe { libc::mprotect(address as *mut libc::c_void, len, prot) } != 0 {
            return Err(errno());
        }
        Ok(())
    }

    /// `munmap(2)`.
    ///
    /// # Safety
    ///
    /// The caller owns `[address, address + len)` and nothing may hold a reference into it.
    pub(in super::super) unsafe fn munmap(address: usize, len: usize) -> Result<(), u32> {
        // SAFETY: the caller's contract.
        if unsafe { libc::munmap(address as *mut libc::c_void, len) } != 0 {
            return Err(errno());
        }
        Ok(())
    }

    /// `msync(2)` with `MS_SYNC`: write a shared mapping's dirty pages back and wait for them.
    ///
    /// # Safety
    ///
    /// `[address, address + len)` is mapped in this process; nothing is dereferenced.
    pub(in super::super) unsafe fn msync(address: usize, len: usize) -> Result<(), u32> {
        // SAFETY: the caller's contract; msync reads page tables, not the memory.
        if unsafe { libc::msync(address as *mut libc::c_void, len, libc::MS_SYNC) } != 0 {
            return Err(errno());
        }
        Ok(())
    }

    /// `fstat(2)`'s `st_size`.
    pub(in super::super) fn file_len(fd: libc::c_int) -> Result<u64, u32> {
        // SAFETY: `stat` is plain data that fstat fills in; an all-zero value is a valid one.
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: `fd` is the caller's open descriptor and `stat` is live and correctly sized.
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(errno());
        }
        Ok(stat.st_size as u64)
    }

    /// Whether `fd` was opened for reading **and** writing (`fcntl(F_GETFL)`'s access mode).
    pub(in super::super) fn opened_read_write(fd: libc::c_int) -> Result<bool, u32> {
        // SAFETY: F_GETFL takes no argument and only reads the descriptor's flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(errno());
        }
        Ok(flags & libc::O_ACCMODE == libc::O_RDWR)
    }
}
