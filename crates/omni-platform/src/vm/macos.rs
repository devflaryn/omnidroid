//! macOS backend for the virtual-memory seam.
//!
//! Implemented and run on macOS 26.5, Apple M1. Every host fact this file relies on was measured
//! on that machine first (the probe and its output are recorded in `docs/ports/macos.md`,
//! "Virtual memory"); the ones that shape the code are:
//!
//! * **The page is 16 KiB** (`sysconf(_SC_PAGESIZE)` = 16384), so every page-granular argument the
//!   seam checks is checked against 16 KiB, and the guest is told the same through `AT_PAGESZ`.
//! * **Address space is free, and so is "commit".** A 16 GiB `PROT_NONE` reservation moved
//!   `phys_footprint` by 0.00 MiB, and `mprotect`ing 1 GiB of it read-write moved it by 0.00 MiB
//!   too; touching every page then moved it by 1024.50 MiB. There is no commit charge on this
//!   host: memory is backed on first touch, which is exactly the owner's "back only on touch".
//! * **Only a fresh mapping both returns memory and reads back zero.** `madvise(MADV_FREE_REUSABLE)`
//!   dropped `phys_footprint` from 1025.39 to 513.39 MiB for 512 MiB but the first byte still read
//!   back its old value (1), and `MADV_DONTNEED` returned nothing measurable. Decommit must give
//!   back zero-filled pages on re-commit (D10's contract, and what a guest's fresh `mmap` relies
//!   on), so decommit here is `mmap(MAP_FIXED | MAP_ANON, PROT_NONE)` over the range: it returned
//!   the 512 MiB (`phys_footprint` 1.14 MiB afterwards), re-committed pages read 0, and it took
//!   5.8 ms for 512 MiB of touched pages (about 0.18 us per 16 KiB page).
//! * **A file cannot be mapped `PROT_EXEC` directly** (`mmap(..., PROT_READ | PROT_EXEC,
//!   MAP_PRIVATE, fd, ...)` fails with `EPERM` for an ordinary file), but a `PROT_READ` file view
//!   can be raised to `PROT_READ | PROT_EXEC` with `mprotect` (0, measured for private and shared
//!   views). So a [`Protection::ReadExecute`] view is two calls here and the page really is r-x.
//!   Guest code is never executed natively by the translating backend; the protection is what the
//!   guest asked for and what the region map reports.
//!
//! # What stands in for Windows' allocation records
//!
//! Windows tracks placeholders, views and allocation extents in the kernel, and the seam's contract
//! is written in those terms: a view replaces an **exact-size** placeholder, `unmap` refuses an
//! address that is not a view's base or a length that is not the view's, `release` refuses an
//! extent that is not the allocation's. `mmap(MAP_FIXED)` has none of that — it will happily
//! replace anything — so a faithful backend has to keep the records itself. [`REGISTRY`] is that
//! record: every range this backend hands out, keyed by base, with what it currently is. Every
//! refusal the Windows kernel makes on the seam's operations is made here from the registry, with
//! `EINVAL` as the OS code where Windows reports its own (the error *variants* are the seam's and
//! are identical across hosts; nothing in the workspace keys on the number).
//!
//! The lock is held across the one or two system calls each operation makes, so the registry and
//! the address space never disagree for another thread to see. Nothing under the lock touches
//! guest memory, so a guest fault taken while another thread holds it cannot deadlock on it.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{MapExecutability, OsError, Protection, ReservationKind, VmError, VmResult};

/// `EINVAL`: what `mmap` returns for a base address or file offset that is not page-aligned.
pub(super) const MISALIGNED_OS_ERROR: u32 = libc::EINVAL as u32;

// ---------------------------------------------------------------------------------------------
// Mach declarations libc does not carry.
// ---------------------------------------------------------------------------------------------

type KernReturn = libc::c_int;
type MachPort = libc::mach_port_t;
type VmProt = libc::c_int;

const KERN_SUCCESS: KernReturn = 0;
const VM_FLAGS_ANYWHERE: libc::c_int = 0x0001;
const VM_INHERIT_NONE: libc::c_uint = 2;
const VM_PROT_NONE: VmProt = 0;
const TASK_VM_INFO: libc::c_int = 22;

/// `task_vm_info_data_t` from `<mach/task_info.h>` up to its revision-2 fields, which is as far as
/// this backend reads. `#pragma pack(4)` there: the two `integer_t`s after `virtual_size` keep
/// every later 64-bit field 8-aligned anyway, and `packed(4)` says the same thing to Rust.
#[repr(C, packed(4))]
#[derive(Default, Clone, Copy)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
    min_address: u64,
    max_address: u64,
}

extern "C" {
    /// What the `mach_task_self()` macro reads; set by libSystem before any Rust code runs.
    static mach_task_self_: MachPort;
    fn _dyld_get_image_header(image_index: u32) -> *const libc::c_void;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> libc::intptr_t;
    fn mach_vm_allocate(target: MachPort, address: *mut u64, size: u64, flags: libc::c_int)
        -> KernReturn;
    fn mach_vm_deallocate(target: MachPort, address: u64, size: u64) -> KernReturn;
    fn mach_vm_protect(
        target: MachPort,
        address: u64,
        size: u64,
        set_maximum: libc::boolean_t,
        new_protection: VmProt,
    ) -> KernReturn;
    fn mach_vm_remap(
        target: MachPort,
        target_address: *mut u64,
        size: u64,
        mask: u64,
        flags: libc::c_int,
        src_task: MachPort,
        src_address: u64,
        copy: libc::boolean_t,
        cur_protection: *mut VmProt,
        max_protection: *mut VmProt,
        inheritance: libc::c_uint,
    ) -> KernReturn;
}

fn task_self() -> MachPort {
    // SAFETY: `mach_task_self_` is initialised by libSystem before any Rust code runs and is never
    // written afterwards; reading a `mach_port_t` from it is what the `mach_task_self()` macro does.
    unsafe { mach_task_self_ }
}

// ---------------------------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------------------------

/// What a registered range currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An ordinary reservation from [`reserve`]: commit, decommit and protect anywhere inside it.
    Plain,
    /// An unreplaced placeholder piece.
    Placeholder,
    /// A placeholder replaced by private commit ([`commit_placeholder`]).
    Private,
    /// A file view that replaced a placeholder ([`map_file`]).
    View {
        /// Whether its file was opened [`MapExecutability::Executable`]: Windows caps every view
        /// of a non-executable section below execute, and so does this backend.
        executable: bool,
    },
    /// A view of a [`SharedSection`] at an address the OS chose ([`map_section`]).
    Section,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    len: usize,
    kind: Kind,
}

/// Every range this backend has handed out, keyed by base. See the module header.
static REGISTRY: Mutex<BTreeMap<usize, Entry>> = Mutex::new(BTreeMap::new());

fn registry() -> MutexGuard<'static, BTreeMap<usize, Entry>> {
    // A panic cannot happen while the lock is held -- nothing under it allocates on a path that
    // unwinds -- but if one ever did, the map is still a consistent record of the address space
    // (every mutation is a remove-then-insert of whole entries), so recovering it is sound.
    REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The entry containing `address`, as `(base, entry)`.
fn containing(map: &BTreeMap<usize, Entry>, address: usize) -> Option<(usize, Entry)> {
    let (&base, &entry) = map.range(..=address).next_back()?;
    (address < base + entry.len).then_some((base, entry))
}

/// The entry that contains all of `[address, address + size)`, if one does.
fn enclosing(map: &BTreeMap<usize, Entry>, address: usize, size: usize) -> Option<(usize, Entry)> {
    let (base, entry) = containing(map, address)?;
    let end = address.checked_add(size)?;
    (end <= base + entry.len).then_some((base, entry))
}

/// Replace the entry at `base` by the pieces `[base, at)`, `[at, at + len)`, `[at + len, end)`,
/// keeping the outer pieces' kind and giving the middle one `middle`.
fn carve(map: &mut BTreeMap<usize, Entry>, base: usize, at: usize, len: usize, middle: Kind) {
    let outer = map.remove(&base).expect("carve is only called on a live entry");
    let end = base + outer.len;
    if at > base {
        map.insert(base, Entry { len: at - base, kind: outer.kind });
    }
    map.insert(at, Entry { len, kind: middle });
    if at + len < end {
        map.insert(at + len, Entry { len: end - (at + len), kind: outer.kind });
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------------------------

fn errno() -> u32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
}

fn os(operation: &'static str, address: usize, size: usize) -> VmError {
    VmError::Os { operation, address, size, source: OsError(errno()) }
}

fn refused(operation: &'static str, address: usize, size: usize, code: i32) -> VmError {
    VmError::Os { operation, address, size, source: OsError(code as u32) }
}

fn prot(protection: Protection) -> libc::c_int {
    match protection {
        Protection::None => libc::PROT_NONE,
        Protection::Read => libc::PROT_READ,
        Protection::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
        Protection::ReadExecute => libc::PROT_READ | libc::PROT_EXEC,
    }
}

fn round_up(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

/// Replace `[address, address + size)` with fresh anonymous `PROT_NONE` pages: the decommit
/// primitive (see the module header for why it is this and not an `madvise`).
fn fresh_reserved(address: usize, size: usize) -> Result<(), u32> {
    // SAFETY: MAP_FIXED over a range this backend owns (the caller checked the registry under its
    // lock). The old pages are discarded, which is the point; nothing in this process holds a
    // reference into them by the seam's contract.
    let mapped = unsafe {
        libc::mmap(
            address as *mut libc::c_void,
            size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(errno());
    }
    debug_assert_eq!(mapped as usize, address, "MAP_FIXED returns the requested base");
    mirrored(address, size, libc::PROT_NONE);
    Ok(())
}

fn mprotect(address: usize, size: usize, protection: libc::c_int) -> Result<(), u32> {
    // SAFETY: the range is one this backend owns (checked against the registry by the caller);
    // mprotect dereferences nothing.
    if unsafe { libc::mprotect(address as *mut libc::c_void, size, protection) } != 0 {
        return Err(errno());
    }
    mirrored(address, size, protection);
    Ok(())
}

/// Tell the hypervisor seam's stage-2 mirror what `[address, address + size)` now is: it has just
/// been replaced (`mmap(MAP_FIXED)`), re-protected, or unmapped (`PROT_NONE`). A guest running
/// natively under Hypervisor.framework otherwise keeps the old pages and ignores host protection
/// (MEASURED; `crate::hypervisor` and `docs/ports/macos-hvf.md`). Without the `hypervisor` feature
/// this is nothing; with it and no range attached it is one atomic load.
#[inline]
fn mirrored(address: usize, size: usize, protection: libc::c_int) {
    #[cfg(all(feature = "hypervisor", target_arch = "aarch64"))]
    crate::hypervisor::host_mapping_changed(address, size, protection);
    #[cfg(not(all(feature = "hypervisor", target_arch = "aarch64")))]
    let _ = (address, size, protection);
}

// ---------------------------------------------------------------------------------------------
// Seam implementation
// ---------------------------------------------------------------------------------------------

/// `sysconf(_SC_PAGESIZE)`: 16384 on Apple silicon (measured).
pub(super) fn page_size() -> usize {
    // SAFETY: sysconf takes an integer name and touches no memory.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    // sysconf cannot fail for _SC_PAGESIZE; a non-positive answer would be a broken libSystem, and
    // a 0 page size would make every alignment check divide by zero.
    assert!(value > 0, "sysconf(_SC_PAGESIZE) answered {value}");
    value as usize
}

/// There is no separate allocation granularity: `mmap` places a mapping at any page boundary.
pub(super) fn allocation_granularity() -> usize {
    page_size()
}

fn reserve_inner(operation: &'static str, size: usize, align: usize, kind: Kind) -> VmResult<usize> {
    let page = page_size();
    let len = round_up(size, page);
    let over = if align > page { len.checked_add(align) } else { Some(len) };
    let over = over.ok_or(VmError::Os {
        operation,
        address: 0,
        size,
        source: OsError(libc::ENOMEM as u32),
    })?;
    let mut map = registry();
    // SAFETY: a NULL hint lets the kernel choose; PROT_NONE grants no access, and an anonymous
    // private mapping is backed by nothing until a page is made accessible and touched.
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            over,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        return Err(os(operation, 0, size));
    }
    let raw = raw as usize;
    let base = if align > page { round_up(raw, align) } else { raw };
    // Trim the over-reservation. Unix, unlike Windows, can release part of a mapping, so the
    // alignment is bought by reserving `align` extra bytes and giving back both ends.
    // SAFETY: both ranges lie inside the mapping just made and nothing refers to them.
    unsafe {
        if base > raw {
            libc::munmap(raw as *mut libc::c_void, base - raw);
        }
        let tail = raw + over - (base + len);
        if tail > 0 {
            libc::munmap((base + len) as *mut libc::c_void, tail);
        }
    }
    map.insert(base, Entry { len, kind });
    Ok(base)
}

pub(super) fn reserve(size: usize, align: usize) -> VmResult<usize> {
    reserve_inner("reserve", size, align, Kind::Plain)
}

pub(super) fn reserve_placeholder(size: usize, align: usize) -> VmResult<usize> {
    reserve_inner("reserve_placeholder", size, align, Kind::Placeholder)
}

/// Carve an independent placeholder out of the placeholder that contains it. Bookkeeping only:
/// `mmap(MAP_FIXED)` needs no OS-level split, but the seam's contract (a view replaces an
/// exact-size piece; `release` frees exactly one piece) is written in terms of the pieces, so the
/// registry must know them.
pub(super) fn split_placeholder(piece_base: usize, size: usize) -> VmResult<()> {
    let mut map = registry();
    match enclosing(&map, piece_base, size) {
        Some((base, entry)) if entry.kind == Kind::Placeholder => {
            carve(&mut map, base, piece_base, size, Kind::Placeholder);
            Ok(())
        }
        // Windows rejects a split of anything that is not one placeholder.
        _ => Err(refused("split_placeholder", piece_base, size, libc::EINVAL)),
    }
}

/// Merge a run of adjacent placeholders that exactly tiles `[address, address + size)`.
pub(super) fn coalesce_placeholders(address: usize, size: usize) -> VmResult<()> {
    let mut map = registry();
    let end = address.checked_add(size);
    let mut cursor = address;
    let mut pieces = Vec::new();
    while Some(cursor) < end {
        match map.get(&cursor) {
            Some(entry) if entry.kind == Kind::Placeholder => {
                pieces.push(cursor);
                cursor += entry.len;
            }
            // Something other than a placeholder, or a range that does not start on a piece.
            _ => return Err(refused("coalesce_placeholders", address, size, libc::EINVAL)),
        }
    }
    if Some(cursor) != end {
        return Err(refused("coalesce_placeholders", address, size, libc::EINVAL));
    }
    for piece in pieces {
        map.remove(&piece);
    }
    map.insert(address, Entry { len: size, kind: Kind::Placeholder });
    Ok(())
}

/// `mprotect` inside a plain reservation or a private piece. Pages are backed on first touch, not
/// here (measured: 0.00 MiB of `phys_footprint` for 1 GiB made read-write).
pub(super) fn commit(address: usize, size: usize, protection: Protection) -> VmResult<()> {
    let map = registry();
    match enclosing(&map, address, size) {
        Some((_, entry)) if matches!(entry.kind, Kind::Plain | Kind::Private) => {
            mprotect(address, size, prot(protection)).map_err(|code| VmError::Os {
                operation: "commit",
                address,
                size,
                source: OsError(code),
            })
        }
        _ => Err(refused("commit", address, size, libc::EINVAL)),
    }
}

/// Replace an exact-size placeholder with private memory.
pub(super) fn commit_placeholder(
    address: usize,
    size: usize,
    protection: Protection,
) -> VmResult<()> {
    let mut map = registry();
    match map.get(&address) {
        Some(entry) if entry.kind == Kind::Placeholder && entry.len == size => {}
        _ => {
            return Err(VmError::PlaceholderNotExactSize {
                operation: "commit_placeholder",
                address,
                size,
                source: OsError(libc::EINVAL as u32),
            })
        }
    }
    // A placeholder's pages are always fresh anonymous `PROT_NONE` ones -- made by `reserve_*`,
    // `decommit_to_placeholder` or `unmap`, each through a new mapping -- so they read zero.
    mprotect(address, size, prot(protection)).map_err(|code| VmError::Os {
        operation: "commit_placeholder",
        address,
        size,
        source: OsError(code),
    })?;
    map.insert(address, Entry { len: size, kind: Kind::Private });
    Ok(())
}

/// Give back committed pages and keep the range reserved; they read zero if committed again.
pub(super) fn decommit(address: usize, size: usize) -> VmResult<()> {
    let map = registry();
    match enclosing(&map, address, size) {
        Some((_, entry)) if matches!(entry.kind, Kind::Plain | Kind::Private) => {
            fresh_reserved(address, size).map_err(|code| VmError::Os {
                operation: "decommit",
                address,
                size,
                source: OsError(code),
            })
        }
        _ => Err(refused("decommit", address, size, libc::EINVAL)),
    }
}

/// Decommit part or all of a private piece and make that part a placeholder again. Windows allows
/// a partial `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER` of a private region and keeps its neighbours'
/// contents; the neighbours here are untouched pages of the same mapping, so the same holds.
pub(super) fn decommit_to_placeholder(address: usize, size: usize) -> VmResult<()> {
    let mut map = registry();
    let Some((base, entry)) = enclosing(&map, address, size) else {
        return Err(refused("decommit_to_placeholder", address, size, libc::EINVAL));
    };
    if entry.kind != Kind::Private {
        return Err(refused("decommit_to_placeholder", address, size, libc::EINVAL));
    }
    fresh_reserved(address, size).map_err(|code| VmError::Os {
        operation: "decommit_to_placeholder",
        address,
        size,
        source: OsError(code),
    })?;
    carve(&mut map, base, address, size, Kind::Placeholder);
    Ok(())
}

/// `mprotect`, inside one committed or mapped range.
///
/// `mprotect` of a `MAP_PRIVATE` file view to read-write privatises the pages copy-on-write and of
/// a `MAP_SHARED` one keeps them shared, which is exactly what `Protection::ReadWrite` means on the
/// seam, so no resolution step is needed here the way Windows needs one.
pub(super) fn protect(address: usize, size: usize, protection: Protection) -> VmResult<()> {
    let map = registry();
    let Some((_, entry)) = enclosing(&map, address, size) else {
        return Err(refused("protect", address, size, libc::EINVAL));
    };
    match entry.kind {
        // Windows' `VirtualProtect` of reserved, uncommitted placeholder pages fails.
        Kind::Placeholder => return Err(refused("protect", address, size, libc::EINVAL)),
        // A non-executable section caps its views below execute on Windows (87, measured);
        // `mprotect` would not refuse, so the cap is enforced from the record.
        Kind::View { executable: false } if protection.is_executable() => {
            return Err(refused("protect", address, size, libc::EACCES))
        }
        _ => {}
    }
    mprotect(address, size, prot(protection)).map_err(|code| VmError::Os {
        operation: "protect",
        address,
        size,
        source: OsError(code),
    })
}

fn open_error(path: &Path, executability: MapExecutability, code: u32) -> VmError {
    VmError::FileOpen { path: path.display().to_string(), executability, source: OsError(code) }
}

fn file_len(fd: libc::c_int) -> Result<u64, u32> {
    // SAFETY: `stat` is plain data and `fstat` writes exactly one of it.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is a live descriptor owned by the caller and `stat` is writable.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(errno());
    }
    Ok(stat.st_size as u64)
}

pub(super) fn open_file_for_mapping(
    path: &Path,
    executability: MapExecutability,
) -> VmResult<MappableFile> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| open_error(path, executability, libc::EINVAL as u32))?;
    // SAFETY: `c_path` is NUL-terminated and outlives the call.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(open_error(path, executability, errno()));
    }
    // SAFETY: `open` just returned this descriptor and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let len = file_len(fd.as_raw_fd()).map_err(|code| open_error(path, executability, code))?;
    if len == 0 {
        return Err(VmError::EmptyFile { path: path.display().to_string() });
    }
    Ok(MappableFile { fd, len, executability, path: path.to_path_buf(), shared: false })
}

pub(super) fn share_file_for_mapping(file: std::fs::File, name: &Path) -> VmResult<MappableFile> {
    let fd = OwnedFd::from(file);
    let shown = || name.display().to_string();
    let len = file_len(fd.as_raw_fd())
        .map_err(|code| open_error(name, MapExecutability::NonExecutable, code))?;
    if len == 0 {
        return Err(VmError::EmptyFile { path: shown() });
    }
    // A shared writable view needs a descriptor open for writing (`mmap` answers `EACCES`
    // otherwise). Windows refuses the same handle when the section is created, with
    // ERROR_ACCESS_DENIED, and the seam documents the refusal here, so it is made here.
    // SAFETY: F_GETFL reads the descriptor's status flags and touches no memory.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(VmError::SectionCreate {
            path: shown(),
            len,
            section_protection: "PROT_READ | PROT_WRITE, MAP_SHARED",
            source: OsError(if flags < 0 { errno() } else { libc::EACCES as u32 }),
        });
    }
    Ok(MappableFile {
        fd,
        len,
        executability: MapExecutability::NonExecutable,
        path: name.to_path_buf(),
        shared: true,
    })
}

/// `msync(MS_SYNC)` writes the view's dirty pages to the file; `fsync` then pushes the file to the
/// device, which is Windows' `FlushViewOfFile` + `FlushFileBuffers` pair.
pub(super) fn sync_view(file: &MappableFile, address: usize, size: usize) -> VmResult<()> {
    // SAFETY: msync inspects page state for the range and dereferences nothing of ours.
    if unsafe { libc::msync(address as *mut libc::c_void, size, libc::MS_SYNC) } != 0 {
        return Err(os("sync_view", address, size));
    }
    // SAFETY: the descriptor is live for as long as `file` is.
    if unsafe { libc::fsync(file.fd.as_raw_fd()) } != 0 {
        return Err(os("sync_view", address, size));
    }
    Ok(())
}

pub(super) fn map_file(
    file: &MappableFile,
    file_offset: u64,
    size: usize,
    address: usize,
    protection: Protection,
) -> VmResult<()> {
    let mut map = registry();
    match map.get(&address) {
        Some(entry) if entry.kind == Kind::Placeholder && entry.len == size => {}
        _ => {
            return Err(VmError::PlaceholderNotExactSize {
                operation: "map_file",
                address,
                size,
                source: OsError(libc::EINVAL as u32),
            })
        }
    }
    // What the view is created as, and what it is protected to afterwards. A file view cannot be
    // created executable here (EPERM, measured) but can be raised to it; a shared view is created
    // writable and protected down, as the Windows backend does, so that raising it again later
    // keeps it shared.
    let (create, then) = if file.shared {
        (libc::PROT_READ | libc::PROT_WRITE, (protection != Protection::ReadWrite).then_some(protection))
    } else {
        match protection {
            Protection::ReadExecute => (libc::PROT_READ, Some(Protection::ReadExecute)),
            other => (prot(other), None),
        }
    };
    let sharing = if file.shared { libc::MAP_SHARED } else { libc::MAP_PRIVATE };
    // SAFETY: MAP_FIXED over an exact-size placeholder this backend owns (checked above, under the
    // lock); the placeholder's pages hold nothing.
    let mapped = unsafe {
        libc::mmap(
            address as *mut libc::c_void,
            size,
            create,
            sharing | libc::MAP_FIXED,
            file.fd.as_raw_fd(),
            file_offset as libc::off_t,
        )
    };
    if mapped == libc::MAP_FAILED {
        let code = errno();
        // A failed MAP_FIXED may already have removed what was there; put the placeholder back so
        // the registry stays true.
        let _ = fresh_reserved(address, size);
        return Err(VmError::Os { operation: "map_file", address, size, source: OsError(code) });
    }
    mirrored(address, size, create);
    if let Some(target) = then {
        if let Err(code) = mprotect(address, size, prot(target)) {
            let _ = fresh_reserved(address, size);
            return Err(VmError::Os { operation: "map_file", address, size, source: OsError(code) });
        }
    }
    let executable = file.executability == MapExecutability::Executable;
    map.insert(address, Entry { len: size, kind: Kind::View { executable } });
    Ok(())
}

/// Check that `[address, address + size)` is exactly one view (or section view), naming the real
/// extent when it is not -- Windows' `NotViewBase`/`ViewSizeMismatch` refusals.
fn whole_view(
    map: &BTreeMap<usize, Entry>,
    operation: &'static str,
    address: usize,
    size: usize,
    allow_section: bool,
) -> VmResult<()> {
    match containing(map, address) {
        Some((base, entry))
            if matches!(entry.kind, Kind::View { .. })
                || (allow_section && entry.kind == Kind::Section) =>
        {
            if base != address {
                return Err(VmError::NotViewBase {
                    address,
                    view_base: base,
                    view_len: entry.len,
                    offset: address - base,
                });
            }
            if entry.len != size {
                return Err(VmError::ViewSizeMismatch {
                    operation,
                    address,
                    requested: size,
                    view_len: entry.len,
                    surviving: entry.len.saturating_sub(size),
                });
            }
            Ok(())
        }
        // Not a view at all: `UnmapViewOfFile2` rejects such an address (ERROR_INVALID_ADDRESS).
        _ => Err(refused(operation, address, size, libc::EINVAL)),
    }
}

pub(super) fn unmap(address: usize, size: usize) -> VmResult<()> {
    let mut map = registry();
    whole_view(&map, "unmap", address, size, false)?;
    fresh_reserved(address, size).map_err(|code| VmError::Os {
        operation: "unmap",
        address,
        size,
        source: OsError(code),
    })?;
    map.insert(address, Entry { len: size, kind: Kind::Placeholder });
    Ok(())
}

pub(super) fn unmap_and_release(address: usize, size: usize) -> VmResult<()> {
    let mut map = registry();
    whole_view(&map, "unmap_and_release", address, size, true)?;
    // SAFETY: the range is exactly one view this backend mapped (checked above, under the lock).
    if unsafe { libc::munmap(address as *mut libc::c_void, size) } != 0 {
        return Err(os("unmap_and_release", address, size));
    }
    mirrored(address, size, libc::PROT_NONE);
    map.remove(&address);
    Ok(())
}

pub(super) fn release(base: usize, len: usize, _kind: ReservationKind) -> VmResult<()> {
    let mut map = registry();
    let requested = round_up(len, page_size());
    let Some(entry) = map.get(&base).copied() else {
        // Not an allocation base: Windows answers ERROR_INVALID_ADDRESS (487), a double release.
        return Err(refused("release", base, len, libc::EINVAL));
    };
    if !matches!(entry.kind, Kind::Plain | Kind::Placeholder | Kind::Private) {
        // A view is given back with `unmap`/`unmap_and_release`, not `MEM_RELEASE`.
        return Err(refused("release", base, len, libc::EINVAL));
    }
    if entry.len != requested {
        return Err(VmError::ReleaseExtentMismatch { address: base, requested, actual: entry.len });
    }
    // SAFETY: exactly one allocation this backend made, whose extent was just checked.
    if unsafe { libc::munmap(base as *mut libc::c_void, requested) } != 0 {
        return Err(os("release", base, len));
    }
    mirrored(base, requested, libc::PROT_NONE);
    map.remove(&base);
    Ok(())
}

/// True: `mmap(MAP_FIXED)` places a mapping at any page boundary this process owns, and nothing is
/// resolved at run time to make that so.
pub(super) fn placeholder_api_available() -> bool {
    true
}

/// Empty: no symbol is resolved dynamically on macOS.
pub(super) fn placeholder_api_symbols() -> Vec<(&'static str, bool)> {
    Vec::new()
}

fn task_vm_info() -> VmResult<TaskVmInfo> {
    let mut info = TaskVmInfo::default();
    let mut count = (std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<libc::natural_t>())
        as libc::mach_msg_type_number_t;
    // SAFETY: `info` is a live, writable TaskVmInfo and `count` says how many 32-bit words of it
    // the kernel may write -- exactly its size, so it cannot write past it.
    let kr = unsafe {
        libc::task_info(
            task_self(),
            TASK_VM_INFO as libc::task_flavor_t,
            std::ptr::addr_of_mut!(info).cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(VmError::Os {
            operation: "task_info(TASK_VM_INFO)",
            address: 0,
            size: 0,
            source: OsError(kr as u32),
        });
    }
    Ok(info)
}

/// `phys_footprint`: the memory this process is charged for -- its dirty private pages, plus what
/// the compressor and swap hold of them. **Not commit charge**, which this host does not have (see
/// the module header): it rises when a page is first touched, not when it is made accessible. It is
/// the number Activity Monitor calls "Memory" and the one the kernel's memory-pressure policy acts
/// on, which makes it the scarce resource here in the sense D10 means.
pub(super) fn process_commit_charge() -> VmResult<u64> {
    Ok(task_vm_info()?.phys_footprint)
}

/// `resident_size`: pages of this process in physical memory, shared ones included.
pub(super) fn process_working_set() -> VmResult<u64> {
    Ok(task_vm_info()?.resident_size)
}

/// The executable image's code span: every section of the main image flagged as holding
/// instructions (`S_ATTR_PURE_INSTRUCTIONS` or `S_ATTR_SOME_INSTRUCTIONS`), slid.
fn executable_code() -> VmResult<core::ops::Range<usize>> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;
    const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
    const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;
    const HEADER_BYTES: usize = 32;
    const SEGMENT_BYTES: usize = 72;
    const SECTION_BYTES: usize = 80;

    // SAFETY: image 0 is the main executable, which dyld keeps mapped for the life of the process.
    let header = unsafe { _dyld_get_image_header(0) } as usize;
    if header == 0 {
        return Err(VmError::ExecutableImage { base: 0, reason: "dyld reports no image 0" });
    }
    // SAFETY: as above; the slide is a plain integer.
    let slide = unsafe { _dyld_get_image_vmaddr_slide(0) } as usize;
    let refuse = |reason: &'static str| VmError::ExecutableImage { base: header, reason };
    // SAFETY (all reads below): each is inside the mapped Mach-O header, whose extent is
    // `HEADER_BYTES + sizeofcmds`; every offset is checked against that before it is read.
    let read32 = |at: usize| unsafe { core::ptr::read_unaligned((header + at) as *const u32) };
    let read64 = |at: usize| unsafe { core::ptr::read_unaligned((header + at) as *const u64) };
    if read32(0) != MH_MAGIC_64 {
        return Err(refuse("image 0 is not a 64-bit Mach-O"));
    }
    let commands = read32(16) as usize;
    let limit = HEADER_BYTES + read32(20) as usize;
    let mut at = HEADER_BYTES;
    let mut span: Option<(usize, usize)> = None;
    for _ in 0..commands {
        if at + 8 > limit {
            return Err(refuse("a load command lies past sizeofcmds"));
        }
        let (command, size) = (read32(at), read32(at + 4) as usize);
        if size < 8 || at + size > limit {
            return Err(refuse("a load command has an impossible size"));
        }
        if command == LC_SEGMENT_64 {
            let sections = read32(at + 64) as usize;
            if SEGMENT_BYTES + sections * SECTION_BYTES > size {
                return Err(refuse("a segment's sections overrun its command"));
            }
            for index in 0..sections {
                let section = at + SEGMENT_BYTES + index * SECTION_BYTES;
                let flags = read32(section + 64);
                if flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS) == 0 {
                    continue;
                }
                let start = read64(section + 32) as usize + slide;
                let end = start + read64(section + 40) as usize;
                span = Some(span.map_or((start, end), |(low, high)| (low.min(start), high.max(end))));
            }
        }
        at += size;
    }
    let (start, end) = span.ok_or_else(|| refuse("the image has no section holding instructions"))?;
    Ok(start..end)
}

/// One `task_info(TASK_VM_INFO)` snapshot, so the fields are consistent with each other.
///
/// * `address_space` is `virtual_size`, which on macOS includes the dyld shared cache every process
///   maps (several GiB): Linux's `total_vm` counts every mapping too, and this is this host's.
/// * `resident_shared` is `external`: resident file-backed pages, the shareable ones. Clamped to
///   `resident` so the documented `resident_shared <= resident` holds even though the two are
///   separate counters in the kernel's snapshot.
/// * `commit_charge` is `phys_footprint`; see [`process_commit_charge`].
pub(super) fn process_memory() -> VmResult<super::ProcessMemory> {
    let info = task_vm_info()?;
    let resident = info.resident_size;
    Ok(super::ProcessMemory {
        address_space: info.virtual_size,
        resident,
        resident_shared: info.external.min(resident),
        commit_charge: info.phys_footprint,
        executable_code: executable_code()?,
    })
}

/// A file opened for mapping. macOS has no section object: sharing and executability are chosen
/// per `mmap`, so this is the descriptor plus what the seam decided about it at open time.
pub struct MappableFile {
    fd: OwnedFd,
    len: u64,
    executability: MapExecutability,
    path: PathBuf,
    /// Whether views are `MAP_SHARED`: set by [`share_file_for_mapping`].
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

/// Anonymous memory that can be mapped more than once: the D12 code arena's section.
///
/// Held as an **anchor** mapping of the whole section, `PROT_NONE`, which owns the VM object; each
/// [`map_section`] is a `mach_vm_remap` of the anchor with `copy = FALSE`, so every view is another
/// set of page-table entries over the same pages. Measured on this host: a remapped view raised to
/// r-x executed an instruction written through a read-write view of the same pages.
pub struct SharedSection {
    anchor: u64,
    len: u64,
}

impl SharedSection {
    pub(super) fn len(&self) -> u64 {
        self.len
    }
}

impl Drop for SharedSection {
    fn drop(&mut self) {
        // The VM object is reference-counted by its mappings, so views made from this section
        // stay valid after the anchor goes -- the Windows section-handle semantics.
        // SAFETY: the anchor is a mapping this value owns and nothing else refers to it.
        unsafe { mach_vm_deallocate(task_self(), self.anchor, self.len) };
    }
}

pub(super) fn create_shared_section(size: u64) -> VmResult<SharedSection> {
    let len = round_up(size as usize, page_size()) as u64;
    let failed = |source: u32| VmError::SectionCreate {
        path: "<anonymous>".to_string(),
        len: size,
        section_protection: "mach_vm_allocate + mach_vm_remap(copy = FALSE)",
        source: OsError(source),
    };
    let mut anchor = 0u64;
    // SAFETY: `anchor` receives the address the kernel chooses.
    let kr = unsafe { mach_vm_allocate(task_self(), &mut anchor, len, VM_FLAGS_ANYWHERE) };
    if kr != KERN_SUCCESS {
        return Err(failed(kr as u32));
    }
    // The anchor itself is never touched: only its views are. Its maximum protection stays what
    // anonymous memory has (read, write, execute), which is what lets one view be r-x.
    // SAFETY: the anchor was just allocated by this function.
    let kr = unsafe { mach_vm_protect(task_self(), anchor, len, 0, VM_PROT_NONE) };
    if kr != KERN_SUCCESS {
        // SAFETY: as above.
        unsafe { mach_vm_deallocate(task_self(), anchor, len) };
        return Err(failed(kr as u32));
    }
    Ok(SharedSection { anchor, len })
}

pub(super) fn map_section(
    section: &SharedSection,
    offset: u64,
    size: usize,
    protection: Protection,
) -> VmResult<usize> {
    let mut map = registry();
    let mut view = 0u64;
    let (mut current, mut maximum) = (0, 0);
    // SAFETY: remaps `size` bytes of the anchor (bounds checked by the seam against the section's
    // length) at an address the kernel chooses; `copy = FALSE` shares the pages.
    let kr = unsafe {
        mach_vm_remap(
            task_self(),
            &mut view,
            size as u64,
            0,
            VM_FLAGS_ANYWHERE,
            task_self(),
            section.anchor + offset,
            0,
            &mut current,
            &mut maximum,
            VM_INHERIT_NONE,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(VmError::Os {
            operation: "map_section",
            address: 0,
            size,
            source: OsError(kr as u32),
        });
    }
    let view = view as usize;
    if let Err(code) = mprotect(view, size, prot(protection)) {
        // SAFETY: the view was just made by this function.
        unsafe { libc::munmap(view as *mut libc::c_void, size) };
        return Err(VmError::Os { operation: "map_section", address: view, size, source: OsError(code) });
    }
    map.insert(view, Entry { len: size, kind: Kind::Section });
    Ok(view)
}

impl MappableFile {
    /// For tests in this module: the descriptor's number.
    #[cfg(test)]
    fn raw_fd(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry refuses what Windows' kernel refuses, and accepts what it accepts, over one
    /// placeholder's life: split, replace, decommit back, coalesce, release.
    #[test]
    fn a_placeholder_lives_the_windows_life() {
        let page = page_size();
        let base = reserve_placeholder(8 * page, page).expect("a placeholder");
        // Not an exact-size piece: refused.
        assert!(matches!(
            commit_placeholder(base, page, Protection::ReadWrite),
            Err(VmError::PlaceholderNotExactSize { .. })
        ));
        split_placeholder(base + page, 2 * page).expect("split");
        commit_placeholder(base + page, 2 * page, Protection::ReadWrite).expect("replace");
        // SAFETY: just committed read-write.
        unsafe { *((base + page) as *mut u8) = 7 };
        // A coalesce over a private piece is refused.
        assert!(coalesce_placeholders(base, 8 * page).is_err());
        decommit_to_placeholder(base + page, 2 * page).expect("decommit to placeholder");
        coalesce_placeholders(base, 8 * page).expect("coalesce");
        // Release of the wrong extent is refused with the real one named.
        match release(base, 4 * page, ReservationKind::Placeholder) {
            Err(VmError::ReleaseExtentMismatch { actual, .. }) => assert_eq!(actual, 8 * page),
            other => panic!("expected ReleaseExtentMismatch, got {other:?}"),
        }
        release(base, 8 * page, ReservationKind::Placeholder).expect("release");
        assert!(release(base, 8 * page, ReservationKind::Placeholder).is_err(), "double release");
    }

    #[test]
    fn a_decommitted_page_reads_zero_when_committed_again() {
        let page = page_size();
        let base = reserve(4 * page, page).expect("reserve");
        commit(base, 4 * page, Protection::ReadWrite).expect("commit");
        // SAFETY: committed read-write above.
        unsafe { *((base + page) as *mut u8) = 0xAB };
        decommit(base + page, page).expect("decommit");
        commit(base + page, page, Protection::ReadWrite).expect("recommit");
        // SAFETY: committed again.
        assert_eq!(unsafe { *((base + page) as *const u8) }, 0);
        release(base, 4 * page, ReservationKind::Plain).expect("release");
    }

    #[test]
    fn the_file_descriptor_of_a_mappable_file_is_its_own() {
        let dir = std::env::temp_dir().join(format!("omni-vm-fd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("f");
        std::fs::write(&path, [1u8; 4096]).expect("write");
        let file = open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
        assert!(file.raw_fd() >= 0);
        assert_eq!(file.len(), 4096);
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
