//! Windows backend for the virtual-memory seam.
//!
//! Every rule this module encodes was measured on Windows 11 26200 by the probes described in
//! `docs/research/windows-memory-model.md`; nothing here is taken from documentation alone.
//!
//! # Symbol resolution
//!
//! `VirtualAlloc2`, `MapViewOfFile3` and `UnmapViewOfFile2` are **not exported from
//! `kernel32.dll`** on this platform. They live in `kernelbase.dll`, and `windows-sys` declares
//! them against the `api-ms-win-core-memory-l1-1-6`/`-l1-1-5` API sets rather than a real DLL, so
//! this module resolves them itself, once, with `GetProcAddress`, and caches the pointers. A
//! missing symbol produces [`VmError::MissingSymbol`], never a panic — the placeholder APIs need
//! Windows 10 1803 or later.
//!
//! Everything else (`VirtualAlloc`, `VirtualFree`, `VirtualProtect`, `VirtualQuery`,
//! `CreateFileW`, `CreateFileMappingW`, `K32GetProcessMemoryInfo`, `GetSystemInfo`) is exported
//! from `kernel32.dll` and is linked normally. As a consequence `reserve`, `commit`, `decommit`,
//! `protect`, `release`, `process_commit_charge` and `process_working_set` all work even if the
//! three dynamic symbols are unavailable; only the placeholder and file-mapping paths, and
//! reservations aligned above 64 KB, depend on them.

use std::ffi::c_void;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_EXECUTE, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
    MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_DECOMMIT, MEM_FREE, MEM_MAPPED,
    MEM_PRESERVE_PLACEHOLDER,
    MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{MapExecutability, OsError, Protection, ReservationKind, VmError, VmResult};

/// The code the kernel reports for a mapping whose base or file offset is not page-aligned.
pub(super) const MISALIGNED_OS_ERROR: u32 = 1132; // ERROR_MAPPED_ALIGNMENT

const ERROR_INVALID_ADDRESS: u32 = 487;

/// `MEM_COALESCE_PLACEHOLDERS`, declared here because `windows-sys` 0.61 does not export it.
///
/// The value is from `memoryapi.h`. It is passed to `VirtualFree` together with `MEM_RELEASE` and
/// merges a run of adjacent placeholders back into one, which is the inverse of
/// [`split_placeholder`] and the only way to satisfy a mapping that spans two ranges the guest
/// freed separately.
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x0000_0001;

// -------------------------------------------------------------------------------------------
// Dynamic symbols from kernelbase.dll
// -------------------------------------------------------------------------------------------

/// `MEM_EXTENDED_PARAMETER`, declared here rather than taken from `windows-sys` because the
/// generated form is a pair of anonymous unions that is awkward to fill in. The layout is a
/// tagged 64-bit word followed by a 64-bit value.
#[repr(C)]
#[derive(Clone, Copy)]
struct MemExtendedParameter {
    /// `Type` in the low 8 bits, `Reserved` in the remaining 56.
    type_and_reserved: u64,
    /// The union of `ULONG64 / PVOID / HANDLE / DWORD`; a pointer for our one use.
    value: u64,
}

/// `MEM_ADDRESS_REQUIREMENTS`: how `VirtualAlloc2` is asked for an alignment above 64 KB.
#[repr(C)]
#[derive(Clone, Copy)]
struct MemAddressRequirements {
    lowest_starting_address: *mut c_void,
    highest_ending_address: *mut c_void,
    alignment: usize,
}

/// `MemExtendedParameterAddressRequirements`.
const MEM_EXTENDED_PARAMETER_ADDRESS_REQUIREMENTS: u64 = 1;

type FnVirtualAlloc2 = unsafe extern "system" fn(
    process: HANDLE,
    base_address: *const c_void,
    size: usize,
    allocation_type: u32,
    page_protection: u32,
    extended_parameters: *mut MemExtendedParameter,
    parameter_count: u32,
) -> *mut c_void;

type FnMapViewOfFile3 = unsafe extern "system" fn(
    file_mapping: HANDLE,
    process: HANDLE,
    base_address: *const c_void,
    offset: u64,
    view_size: usize,
    allocation_type: u32,
    page_protection: u32,
    extended_parameters: *mut MemExtendedParameter,
    parameter_count: u32,
) -> *mut c_void;

type FnUnmapViewOfFile2 =
    unsafe extern "system" fn(process: HANDLE, base_address: *mut c_void, flags: u32) -> i32;

/// The three placeholder entry points, resolved once from `kernelbase.dll`.
struct KernelBase {
    virtual_alloc2: Result<FnVirtualAlloc2, OsError>,
    map_view_of_file3: Result<FnMapViewOfFile3, OsError>,
    unmap_view_of_file2: Result<FnUnmapViewOfFile2, OsError>,
}

/// The module these three symbols actually live in. Reported in errors so a failure names it.
const KERNELBASE: &str = "kernelbase.dll";

static KERNELBASE_SYMBOLS: OnceLock<KernelBase> = OnceLock::new();

fn kernelbase() -> &'static KernelBase {
    KERNELBASE_SYMBOLS.get_or_init(|| {
        // SAFETY: a NUL-terminated ASCII literal is a valid PCSTR, and kernelbase.dll is already
        // resident in every Win32 process, so this only takes a reference on it.
        let module = unsafe { LoadLibraryA(c"kernelbase.dll".as_ptr().cast::<u8>()) };
        if module.is_null() {
            let err = OsError(last_error());
            return KernelBase {
                virtual_alloc2: Err(err),
                map_view_of_file3: Err(err),
                unmap_view_of_file2: Err(err),
            };
        }
        // SAFETY: `module` is a live module handle; each name is a NUL-terminated C string.
        // The transmutes reinterpret a resolved export as a function pointer whose signature is
        // declared above to match the documented Win32 prototype of that export.
        unsafe {
            KernelBase {
                virtual_alloc2: resolve(module, c"VirtualAlloc2")
                    .map(|p| std::mem::transmute::<*const c_void, FnVirtualAlloc2>(p)),
                map_view_of_file3: resolve(module, c"MapViewOfFile3")
                    .map(|p| std::mem::transmute::<*const c_void, FnMapViewOfFile3>(p)),
                unmap_view_of_file2: resolve(module, c"UnmapViewOfFile2")
                    .map(|p| std::mem::transmute::<*const c_void, FnUnmapViewOfFile2>(p)),
            }
        }
    })
}

/// # Safety
///
/// `module` must be a live module handle.
unsafe fn resolve(
    module: windows_sys::Win32::Foundation::HMODULE,
    name: &core::ffi::CStr,
) -> Result<*const c_void, OsError> {
    match GetProcAddress(module, name.as_ptr().cast::<u8>()) {
        Some(p) => Ok(p as *const c_void),
        None => Err(OsError(last_error())),
    }
}

fn virtual_alloc2() -> VmResult<FnVirtualAlloc2> {
    kernelbase().virtual_alloc2.map_err(|source| VmError::MissingSymbol {
        symbol: "VirtualAlloc2",
        library: KERNELBASE,
        source,
    })
}

fn map_view_of_file3() -> VmResult<FnMapViewOfFile3> {
    kernelbase().map_view_of_file3.map_err(|source| VmError::MissingSymbol {
        symbol: "MapViewOfFile3",
        library: KERNELBASE,
        source,
    })
}

fn unmap_view_of_file2() -> VmResult<FnUnmapViewOfFile2> {
    kernelbase().unmap_view_of_file2.map_err(|source| VmError::MissingSymbol {
        symbol: "UnmapViewOfFile2",
        library: KERNELBASE,
        source,
    })
}

/// Whether all three placeholder entry points resolved.
///
/// Reached through [`vm::placeholder_api_available`](super::placeholder_api_available), which is
/// `cfg`-free, so that a test or a diagnostic can report the state of the seam's one runtime
/// dependency instead of inferring it from a failure — and can do so without naming a platform.
pub(super) fn placeholder_api_available() -> bool {
    let kb = kernelbase();
    kb.virtual_alloc2.is_ok() && kb.map_view_of_file3.is_ok() && kb.unmap_view_of_file2.is_ok()
}

/// The names of the symbols resolved from `kernelbase.dll`, paired with whether each resolved.
pub(super) fn placeholder_api_symbols() -> Vec<(&'static str, bool)> {
    let kb = kernelbase();
    vec![
        ("VirtualAlloc2", kb.virtual_alloc2.is_ok()),
        ("MapViewOfFile3", kb.map_view_of_file3.is_ok()),
        ("UnmapViewOfFile2", kb.unmap_view_of_file2.is_ok()),
    ]
}

// -------------------------------------------------------------------------------------------
// Small helpers
// -------------------------------------------------------------------------------------------

fn last_error() -> u32 {
    // SAFETY: GetLastError reads this thread's last-error value and cannot fail.
    unsafe { GetLastError() }
}

fn os(operation: &'static str, address: usize, size: usize) -> VmError {
    VmError::Os { operation, address, size, source: OsError(last_error()) }
}

/// What is actually at an address, to the extent it changes which Win32 flag a [`Protection`]
/// has to become.
///
/// There are three cases, not two, and conflating the last two is the trap described on
/// [`resolved_protection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionFlavour {
    /// Private committed memory, from `commit` or `commit_placeholder`.
    Private,
    /// A view created with a protection that keeps writes private to this mapping:
    /// `PAGE_READONLY`, `PAGE_WRITECOPY` or `PAGE_EXECUTE_READ`. Every view `map_file` can
    /// currently produce is one of these.
    PrivateView,
    /// A view created `PAGE_READWRITE` or `PAGE_EXECUTE_READWRITE`, where writes are shared with
    /// every other view of the same section.
    SharedWritableView,
}

/// The page protection a *private* page gets for a given [`Protection`].
fn private_protection(protection: Protection) -> u32 {
    match protection {
        Protection::None => PAGE_NOACCESS,
        Protection::Read => PAGE_READONLY,
        Protection::ReadWrite => PAGE_READWRITE,
        Protection::ReadExecute => PAGE_EXECUTE_READ,
    }
}

/// The page protection a *file-backed view* gets for a given [`Protection`].
///
/// `ReadWrite` becomes `PAGE_WRITECOPY`, not `PAGE_READWRITE`: the file is opened without
/// `GENERIC_WRITE`, so a shared writable view fails with `ERROR_ACCESS_DENIED` (5), and a
/// writable view of the shared extraction cache is never what Omnidroid wants. Copy-on-write
/// privatises the pages that are written and leaves the file untouched.
///
/// `Protection::None` is rejected before this is reached.
fn view_protection(protection: Protection) -> u32 {
    match protection {
        Protection::None => PAGE_NOACCESS,
        Protection::Read => PAGE_READONLY,
        Protection::ReadWrite => PAGE_WRITECOPY,
        Protection::ReadExecute => PAGE_EXECUTE_READ,
    }
}

fn system_info() -> &'static SYSTEM_INFO {
    static INFO: OnceLock<SystemInfoCell> = OnceLock::new();
    &INFO
        .get_or_init(|| {
            let mut info = SYSTEM_INFO::default();
            // SAFETY: `info` is a live, correctly sized SYSTEM_INFO; GetSystemInfo only writes it.
            unsafe { GetSystemInfo(&mut info) };
            SystemInfoCell(info)
        })
        .0
}

/// Wrapper that lets `SYSTEM_INFO` live in a `OnceLock`.
///
/// `SYSTEM_INFO` contains two raw pointers (the min/max application addresses), which makes it
/// `!Send + !Sync`. They are process-constant integers describing the address space, not
/// dereferenceable data owned by a thread, so sharing the struct is sound.
struct SystemInfoCell(SYSTEM_INFO);

// SAFETY: the contents are process-wide constants filled in once by the kernel. The two pointer
// fields are never dereferenced by this crate.
unsafe impl Send for SystemInfoCell {}
// SAFETY: as above; the value is immutable after initialisation.
unsafe impl Sync for SystemInfoCell {}

/// The Win32 flag a [`Protection`] must become, given what is actually at the address.
///
/// A pure function so that the decision can be tested exhaustively without having to construct
/// every kind of region first — see the tests at the bottom of this module.
///
/// # The `ReadWrite` case, which is the only interesting one
///
/// `PAGE_READWRITE` and `PAGE_WRITECOPY` are not interchangeable and picking the wrong one is
/// rejected in both directions:
///
/// * [`RegionFlavour::Private`] needs `PAGE_READWRITE`; `PAGE_WRITECOPY` is refused.
/// * [`RegionFlavour::PrivateView`] needs `PAGE_WRITECOPY`; `PAGE_READWRITE` is refused with
///   `ERROR_ACCESS_DENIED` (5), because the file is opened without `GENERIC_WRITE`.
/// * [`RegionFlavour::SharedWritableView`] needs `PAGE_READWRITE`, and this case is a **trap**.
///
/// The trap: a D12 dual-mapped code arena is two views of one pagefile-backed section, one
/// `PAGE_READWRITE` and one `PAGE_EXECUTE_READ`. Its RW view is `MEM_MAPPED`, so a rule that said
/// "mapped means copy-on-write" would quietly turn the arena's writable view into a private one.
/// The emitter would keep writing happily, every write would land on a privatised page, and
/// **nothing would ever appear through the RX view** — a fault or a stale instruction stream far
/// from the cause, with no error anywhere. So the flavour is taken from the view's
/// `AllocationProtect` (the protection it was *created* with, which Windows preserves for the life
/// of the view) and a shared-writable view stays shared.
fn resolved_protection(protection: Protection, flavour: RegionFlavour) -> u32 {
    match flavour {
        RegionFlavour::Private | RegionFlavour::SharedWritableView => {
            private_protection(protection)
        }
        RegionFlavour::PrivateView => view_protection(protection),
    }
}

/// Classify what is at `address`, for [`resolved_protection`].
///
/// Anything that is not a mapped view — including an address `VirtualQuery` cannot describe — is
/// treated as private, which leaves the OS to reject a genuinely bad address rather than
/// second-guessing it here.
fn region_flavour(address: usize) -> RegionFlavour {
    let Some(mbi) = query(address) else {
        return RegionFlavour::Private;
    };
    if mbi.Type != MEM_MAPPED {
        return RegionFlavour::Private;
    }
    if mbi.AllocationProtect == PAGE_READWRITE || mbi.AllocationProtect == PAGE_EXECUTE_READWRITE {
        RegionFlavour::SharedWritableView
    } else {
        RegionFlavour::PrivateView
    }
}

/// The base and total length of the mapped view containing `address`.
///
/// A view is **not** one `MEMORY_BASIC_INFORMATION` region. Changing the protection of some of its
/// pages splits it into several regions, all of which keep the view's `AllocationBase`, so a single
/// `RegionSize` understates the view as soon as anything has called `protect` on part of it. The
/// walk below follows the regions forward from the allocation base for as long as they still belong
/// to the same view, which is the only way to learn a view's real extent after that has happened.
///
/// Returns `None` if `address` is not inside a mapped view at all.
fn view_extent(address: usize) -> Option<(usize, usize)> {
    let first = query(address)?;
    if first.Type != MEM_MAPPED {
        return None;
    }
    let base = first.AllocationBase as usize;
    if base == 0 {
        return None;
    }

    let mut end = base;
    loop {
        let Some(mbi) = query(end) else { break };
        // A different allocation base, or private memory, means we have walked off the end of this
        // view and into whatever was mapped next to it.
        if mbi.AllocationBase as usize != base || mbi.Type != MEM_MAPPED || mbi.RegionSize == 0 {
            break;
        }
        let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
        if region_end <= end {
            // Cannot happen for a well-formed region, but never spin on a malformed one.
            break;
        }
        end = region_end;
    }
    Some((base, end - base))
}

/// Total extent of the allocation that *starts* at `address`, or `None` if `address` is not an
/// allocation base.
///
/// Needed because `MEM_RELEASE` takes no length and frees the allocation at the address it is given,
/// whatever its size. A placeholder that has been split is several allocations, so releasing its
/// parent's base address frees only the **first piece** and reports success — measured, and a silent
/// partial free is exactly the shape of bug that surfaces as a leak nobody can attribute. Like
/// [`view_extent`], this walks forward rather than trusting one `RegionSize`, because committing or
/// protecting part of a reservation splits it into several regions that all keep the same
/// `AllocationBase`.
fn allocation_extent(address: usize) -> Option<usize> {
    let first = query(address)?;
    if first.State == MEM_FREE || first.AllocationBase as usize != address {
        return None;
    }
    let mut end = address;
    loop {
        let Some(mbi) = query(end) else { break };
        if mbi.State == MEM_FREE
            || mbi.AllocationBase as usize != address
            || mbi.RegionSize == 0
        {
            break;
        }
        let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
        if region_end <= end {
            // Cannot happen for a well-formed region, but never spin on a malformed one.
            break;
        }
        end = region_end;
    }
    Some(end - address)
}

fn query(address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `mbi` is a live, correctly sized MEMORY_BASIC_INFORMATION. VirtualQuery accepts any
    // address value, including one that is not mapped, and reports failure by returning 0.
    let written = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 {
        None
    } else {
        Some(mbi)
    }
}

// -------------------------------------------------------------------------------------------
// Seam implementation
// -------------------------------------------------------------------------------------------

pub(super) fn page_size() -> usize {
    system_info().dwPageSize as usize
}

pub(super) fn allocation_granularity() -> usize {
    system_info().dwAllocationGranularity as usize
}

/// Reserve address space, optionally at an alignment above the allocation granularity.
///
/// Alignments up to `dwAllocationGranularity` come for free, because a reservation base is
/// rounded down to it anyway, so the common case does not touch `VirtualAlloc2` at all. A larger
/// alignment is requested with a `MEM_ADDRESS_REQUIREMENTS` extended parameter rather than by
/// over-reserving and trimming, because a partial `MEM_RELEASE` of an ordinary reservation is
/// rejected with `ERROR_INVALID_PARAMETER` (87).
fn reserve_inner(
    operation: &'static str,
    size: usize,
    align: usize,
    allocation_type: u32,
) -> VmResult<usize> {
    let granularity = allocation_granularity();
    let placeholder = allocation_type & MEM_RESERVE_PLACEHOLDER != 0;

    if align <= granularity && !placeholder {
        // SAFETY: a NULL base asks the OS to choose the address; MEM_RESERVE with PAGE_NOACCESS
        // commits nothing and grants no access, so no memory is made reachable by this call.
        let base = unsafe {
            VirtualAlloc(std::ptr::null(), size, allocation_type, PAGE_NOACCESS)
        };
        if base.is_null() {
            return Err(os(operation, 0, size));
        }
        return Ok(base as usize);
    }

    let alloc2 = virtual_alloc2()?;
    let mut requirements = MemAddressRequirements {
        lowest_starting_address: std::ptr::null_mut(),
        highest_ending_address: std::ptr::null_mut(),
        alignment: if align <= granularity { 0 } else { align },
    };
    let mut parameter = MemExtendedParameter {
        type_and_reserved: MEM_EXTENDED_PARAMETER_ADDRESS_REQUIREMENTS,
        value: std::ptr::addr_of_mut!(requirements) as u64,
    };
    let (parameters, count) = if requirements.alignment == 0 {
        (std::ptr::null_mut(), 0)
    } else {
        (std::ptr::addr_of_mut!(parameter), 1)
    };

    // SAFETY: `alloc2` is the resolved `VirtualAlloc2` and is called with its documented
    // signature. `parameters` is either NULL with a count of 0, or a pointer to one live
    // MEM_EXTENDED_PARAMETER whose `value` points at `requirements`, which outlives the call.
    // A NULL base asks the OS to choose the address; nothing is committed.
    let base = unsafe {
        alloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            size,
            allocation_type,
            PAGE_NOACCESS,
            parameters,
            count,
        )
    };
    if base.is_null() {
        return Err(os(operation, 0, size));
    }
    Ok(base as usize)
}

pub(super) fn reserve(size: usize, align: usize) -> VmResult<usize> {
    reserve_inner("reserve", size, align, MEM_RESERVE)
}

pub(super) fn reserve_placeholder(size: usize, align: usize) -> VmResult<usize> {
    reserve_inner(
        "reserve_placeholder",
        size,
        align,
        MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
    )
}

pub(super) fn split_placeholder(piece_base: usize, size: usize) -> VmResult<()> {
    // SAFETY: MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER does not free address space and does not
    // change accessibility: it splits the enclosing placeholder so the sub-range becomes an
    // independent placeholder. Nothing is dereferenced. A range that is not part of a placeholder
    // is rejected by the kernel rather than silently freed.
    let ok = unsafe {
        VirtualFree(
            piece_base as *mut c_void,
            size,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if ok == 0 {
        return Err(os("split_placeholder", piece_base, size));
    }
    Ok(())
}

pub(super) fn commit(address: usize, size: usize, protection: Protection) -> VmResult<()> {
    // SAFETY: the caller's contract is that `[address, address + size)` lies inside a live
    // reservation it owns. MEM_COMMIT on an already-reserved range does not move it.
    let base = unsafe {
        VirtualAlloc(
            address as *const c_void,
            size,
            MEM_COMMIT,
            private_protection(protection),
        )
    };
    if base.is_null() {
        return Err(os("commit", address, size));
    }
    Ok(())
}

pub(super) fn commit_placeholder(
    address: usize,
    size: usize,
    protection: Protection,
) -> VmResult<()> {
    let alloc2 = virtual_alloc2()?;
    // SAFETY: `alloc2` is the resolved `VirtualAlloc2`, called with its documented signature and
    // no extended parameters. The caller's contract is that the range is exactly one unreplaced
    // placeholder piece it owns.
    let base = unsafe {
        alloc2(
            GetCurrentProcess(),
            address as *const c_void,
            size,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            private_protection(protection),
            std::ptr::null_mut(),
            0,
        )
    };
    if base.is_null() {
        let code = last_error();
        if code == ERROR_INVALID_ADDRESS {
            return Err(VmError::PlaceholderNotExactSize {
                operation: "commit_placeholder",
                address,
                size,
                source: OsError(code),
            });
        }
        return Err(VmError::Os {
            operation: "commit_placeholder",
            address,
            size,
            source: OsError(code),
        });
    }
    Ok(())
}

pub(super) fn decommit(address: usize, size: usize) -> VmResult<()> {
    // SAFETY: MEM_DECOMMIT keeps the address range reserved and only drops its physical/commit
    // backing. The caller's contract is that nothing holds a reference into the range.
    let ok = unsafe { VirtualFree(address as *mut c_void, size, MEM_DECOMMIT) };
    if ok == 0 {
        return Err(os("decommit", address, size));
    }
    Ok(())
}

pub(super) fn coalesce_placeholders(address: usize, size: usize) -> VmResult<()> {
    // SAFETY: MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS frees no address space. It merges the
    // adjacent placeholders covering `[address, address + size)` into a single placeholder, and is
    // rejected by the kernel if the range contains anything that is not a placeholder. Nothing is
    // dereferenced.
    let ok = unsafe {
        VirtualFree(
            address as *mut c_void,
            size,
            MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS,
        )
    };
    if ok == 0 {
        return Err(os("coalesce_placeholders", address, size));
    }
    Ok(())
}

pub(super) fn decommit_to_placeholder(address: usize, size: usize) -> VmResult<()> {
    // SAFETY: as `split_placeholder`: MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER returns the range to
    // placeholder state and keeps the address space owned by this process.
    let ok = unsafe {
        VirtualFree(
            address as *mut c_void,
            size,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if ok == 0 {
        return Err(os("decommit_to_placeholder", address, size));
    }
    Ok(())
}

/// Change protection, choosing the right Win32 flag for what is actually at `address`.
///
/// The region is classified with `VirtualQuery` and the flag chosen by [`resolved_protection`],
/// which costs one extra call and keeps `Protection::ReadWrite` meaning the same thing everywhere:
/// *writable, and writes go wherever writes to that region are supposed to go*. For a file view
/// that means copy-on-write, which is also exactly what `mprotect(PROT_READ | PROT_WRITE)` does to
/// a `MAP_PRIVATE` file mapping on unix — so the abstraction stays faithful rather than
/// Windows-shaped. For a shared-writable view, such as a dual-mapped code arena's RW view, it
/// stays shared; see [`resolved_protection`] for why that distinction is not optional.
///
/// This is what the ELF loader needs in order to apply relocations to file-backed `.text`: map the
/// segment `ReadExecute`, drop the affected pages to `ReadWrite` (privatising just those pages),
/// write, and raise them back to `ReadExecute`.
pub(super) fn protect(address: usize, size: usize, protection: Protection) -> VmResult<()> {
    let flags = resolved_protection(protection, region_flavour(address));
    let mut old = 0u32;
    // SAFETY: the caller's contract is that the range is committed or mapped memory it owns.
    // `old` is a live u32 that receives the previous protection.
    let ok = unsafe {
        VirtualProtect(address as *const c_void, size, flags, &mut old)
    };
    if ok == 0 {
        return Err(os("protect", address, size));
    }
    Ok(())
}

pub(super) fn unmap(address: usize, size: usize) -> VmResult<()> {
    unmap_inner("unmap", address, size, MEM_PRESERVE_PLACEHOLDER)
}

pub(super) fn unmap_and_release(address: usize, size: usize) -> VmResult<()> {
    unmap_inner("unmap_and_release", address, size, 0)
}

fn unmap_inner(
    operation: &'static str,
    address: usize,
    size: usize,
    flags: u32,
) -> VmResult<()> {
    let unmap2 = unmap_view_of_file2()?;

    // `UnmapViewOfFile2` takes no length: it unmaps the entire view that starts at the address it
    // is given. So `size` cannot be passed through to it and must be *checked* against the view's
    // real extent here, or a caller asking to unmap four pages of a hundred-page view would tear
    // down all hundred and be told it succeeded. Both directions of that mistake are refused:
    // an address inside a view, and a view base with the wrong length.
    if let Some((view_base, view_len)) = view_extent(address) {
        if view_base != address {
            return Err(VmError::NotViewBase {
                address,
                view_base,
                view_len,
                offset: address - view_base,
            });
        }
        if view_len != size {
            return Err(VmError::ViewSizeMismatch {
                operation,
                address,
                requested: size,
                view_len,
                surviving: view_len.saturating_sub(size),
            });
        }
    }

    // SAFETY: `unmap2` is the resolved `UnmapViewOfFile2`, called with its documented signature.
    // `MEMORY_MAPPED_VIEW_ADDRESS` is a one-pointer struct and is ABI-identical to the pointer it
    // wraps. The caller's contract is that `address` is the base of a view it owns and that
    // nothing holds a reference into it.
    let ok = unsafe { unmap2(GetCurrentProcess(), address as *mut c_void, flags) };
    if ok == 0 {
        return Err(os(operation, address, size));
    }
    Ok(())
}

pub(super) fn release(base: usize, len: usize, _kind: ReservationKind) -> VmResult<()> {
    // `MEM_RELEASE` must be given a size of 0, and then frees *the allocation that starts at `base`*
    // — which is not necessarily the range the caller's descriptor names. A placeholder that has
    // been split is several allocations, so a release of the parent base frees only the first piece
    // and returns success. That is checked here rather than left to the caller, because the failure
    // is silent: address space stays reserved for the life of the process with nothing pointing at
    // it. The OS rounds a reservation up to a whole page, so that is what the requested length is
    // compared as.
    let page = page_size();
    let requested = len.div_ceil(page) * page;
    if let Some(actual) = allocation_extent(base) {
        if actual != requested {
            return Err(VmError::ReleaseExtentMismatch {
                address: base,
                requested,
                actual,
            });
        }
    }
    // An address that is not an allocation base at all is left to the kernel, which rejects it with
    // ERROR_INVALID_ADDRESS (487, measured) — the code to look for after a double release.

    // SAFETY: MEM_RELEASE with a size of 0 releases the allocation that starts at `base`, whose
    // extent has just been checked against what the caller asked to release. Nothing is
    // dereferenced.
    let ok = unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) };
    if ok == 0 {
        return Err(os("release", base, len));
    }
    Ok(())
}

fn memory_counters() -> VmResult<PROCESS_MEMORY_COUNTERS_EX> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a live PROCESS_MEMORY_COUNTERS_EX whose `cb` states its real size.
    // K32GetProcessMemoryInfo is declared over the shorter PROCESS_MEMORY_COUNTERS, and writes
    // the `_EX` layout when `cb` says the buffer is that long — this is the documented contract
    // of the API and the reason `cb` exists.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::addr_of_mut!(counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return Err(os("process memory counters", 0, 0));
    }
    Ok(counters)
}

pub(super) fn process_commit_charge() -> VmResult<u64> {
    Ok(memory_counters()?.PrivateUsage as u64)
}

pub(super) fn process_working_set() -> VmResult<u64> {
    Ok(memory_counters()?.WorkingSetSize as u64)
}

// -------------------------------------------------------------------------------------------
// Files opened for mapping
// -------------------------------------------------------------------------------------------

/// A file open for mapping, together with its section object.
///
/// The section is created when the file is opened, because the section protection decides the
/// maximum protection of every view for the life of the mapping — an executable view requires
/// both a file handle carrying `GENERIC_EXECUTE` and a `PAGE_EXECUTE_READ` section, and neither
/// can be added afterwards (D11). Creating it here is what turns "`.text` can never be made
/// executable" from a late, confusing failure into an error at the point the file was opened.
pub struct MappableFile {
    file: HANDLE,
    section: HANDLE,
    len: u64,
    executability: MapExecutability,
    path: PathBuf,
}

// SAFETY: a Win32 HANDLE is process-wide, not thread-owned, and `MappableFile` only ever passes
// its handles to APIs that are safe to call from any thread. Nothing in it is a thread-affine
// resource.
unsafe impl Send for MappableFile {}
// SAFETY: as above, and every field is immutable after construction.
unsafe impl Sync for MappableFile {}

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
        // SAFETY: both handles were created by this type and are closed exactly once, here.
        // Closing them does not invalidate views already mapped from the section: the kernel
        // keeps the section and the file alive until the last view is unmapped.
        unsafe {
            CloseHandle(self.section);
            CloseHandle(self.file);
        }
    }
}

pub(super) fn open_file_for_mapping(
    path: &Path,
    executability: MapExecutability,
) -> VmResult<MappableFile> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();

    let access = match executability {
        MapExecutability::NonExecutable => GENERIC_READ,
        // Measured: without GENERIC_EXECUTE on the file handle, creating a PAGE_EXECUTE_READ
        // section fails with ERROR_INVALID_HANDLE (6).
        MapExecutability::Executable => GENERIC_READ | GENERIC_EXECUTE,
    };

    // SAFETY: `wide` is a live NUL-terminated UTF-16 path. The file is opened read-only and
    // shared for reading, so this cannot modify it; a NULL security descriptor and template
    // handle are the documented defaults.
    let file = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(VmError::FileOpen {
            path: path.display().to_string(),
            executability,
            source: OsError(last_error()),
        });
    }
    let guard = HandleGuard(file);

    let mut len: i64 = 0;
    // SAFETY: `guard.0` is a live file handle and `len` is a live i64 the call writes.
    let ok = unsafe { GetFileSizeEx(guard.0, &mut len) };
    if ok == 0 {
        return Err(VmError::FileOpen {
            path: path.display().to_string(),
            executability,
            source: OsError(last_error()),
        });
    }
    let len = len as u64;
    if len == 0 {
        // CreateFileMappingW with a maximum size of 0 means "the whole file", which cannot be
        // done for an empty file. Say so, instead of surfacing ERROR_FILE_INVALID.
        return Err(VmError::EmptyFile { path: path.display().to_string() });
    }

    let (section_protection, section_protection_name) = match executability {
        MapExecutability::NonExecutable => (PAGE_READONLY, "PAGE_READONLY"),
        MapExecutability::Executable => (PAGE_EXECUTE_READ, "PAGE_EXECUTE_READ"),
    };

    // SAFETY: `guard.0` is a live file handle. A NULL security descriptor and name are the
    // documented defaults, and a maximum size of 0 means "the size of the file".
    let section = unsafe {
        CreateFileMappingW(
            guard.0,
            std::ptr::null(),
            section_protection,
            0,
            0,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        return Err(VmError::SectionCreate {
            path: path.display().to_string(),
            len,
            section_protection: section_protection_name,
            source: OsError(last_error()),
        });
    }

    Ok(MappableFile {
        file: guard.into_raw(),
        section,
        len,
        executability,
        path: path.to_path_buf(),
    })
}

pub(super) fn map_file(
    file: &MappableFile,
    file_offset: u64,
    size: usize,
    address: usize,
    protection: Protection,
) -> VmResult<()> {
    let map3 = map_view_of_file3()?;
    // SAFETY: `map3` is the resolved `MapViewOfFile3`, called with its documented signature and
    // no extended parameters. `file.section` is a live section object. The caller's contract is
    // that `[address, address + size)` is exactly one unreplaced placeholder piece it owns; the
    // return value is checked before anything is read through it.
    let view = unsafe {
        map3(
            file.section,
            GetCurrentProcess(),
            address as *const c_void,
            file_offset,
            size,
            MEM_REPLACE_PLACEHOLDER,
            view_protection(protection),
            std::ptr::null_mut(),
            0,
        )
    };
    if view.is_null() {
        let code = last_error();
        return Err(match code {
            ERROR_INVALID_ADDRESS => VmError::PlaceholderNotExactSize {
                operation: "map_file",
                address,
                size,
                source: OsError(code),
            },
            MISALIGNED_OS_ERROR => VmError::Misaligned {
                operation: "map_file",
                what: "file offset or base address",
                value: file_offset,
                required: page_size() as u64,
                os_equivalent: OsError(code),
            },
            _ => VmError::Os {
                operation: "map_file",
                address,
                size,
                source: OsError(code),
            },
        });
    }
    debug_assert_eq!(
        view as usize, address,
        "MapViewOfFile3 into a placeholder must return the requested base"
    );
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Pagefile-backed sections, for the D12 dual-mapped code arena
// -------------------------------------------------------------------------------------------

/// A pagefile-backed section that can be mapped more than once.
///
/// Created `PAGE_EXECUTE_READWRITE`, which is the *section's* maximum protection and not any
/// page's protection: it is what allows one view to be `PAGE_READWRITE` and another
/// `PAGE_EXECUTE_READ`. No view this module can produce is ever both, because [`Protection`] has
/// no writable-and-executable variant.
pub struct SharedSection {
    section: HANDLE,
    len: u64,
}

// SAFETY: a Win32 HANDLE is process-wide, not thread-owned, and every API this type passes it to
// is callable from any thread. Both fields are immutable after construction.
unsafe impl Send for SharedSection {}
// SAFETY: as above.
unsafe impl Sync for SharedSection {}

impl SharedSection {
    pub(super) fn len(&self) -> u64 {
        self.len
    }
}

impl Drop for SharedSection {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `create_shared_section` and is closed exactly once,
        // here. Closing it does not invalidate views already mapped from it: the kernel keeps the
        // section alive until the last view is unmapped.
        unsafe { CloseHandle(self.section) };
    }
}

pub(super) fn create_shared_section(size: u64) -> VmResult<SharedSection> {
    let high = (size >> 32) as u32;
    let low = (size & 0xFFFF_FFFF) as u32;

    // SAFETY: INVALID_HANDLE_VALUE as the file handle asks for a pagefile-backed section, which is
    // the documented way to get anonymous shared memory. A NULL security descriptor and name are
    // the documented defaults, and the maximum size is given explicitly because there is no file
    // to take it from.
    let section = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_EXECUTE_READWRITE,
            high,
            low,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        return Err(VmError::SectionCreate {
            path: "<pagefile>".to_string(),
            len: size,
            section_protection: "PAGE_EXECUTE_READWRITE",
            source: OsError(last_error()),
        });
    }
    Ok(SharedSection { section, len: size })
}

pub(super) fn map_section(
    section: &SharedSection,
    offset: u64,
    size: usize,
    protection: Protection,
) -> VmResult<usize> {
    let map3 = map_view_of_file3()?;
    // The view is *shared*, so `Protection::ReadWrite` must become `PAGE_READWRITE` and never
    // `PAGE_WRITECOPY`: copy-on-write would privatise every write and nothing would ever reach the
    // paired executable view. That is the same decision `resolved_protection` makes for an
    // existing shared view, restated here for the moment of creation, where there is no
    // `AllocationProtect` to read back yet.
    let flags = private_protection(protection);

    // SAFETY: `map3` is the resolved `MapViewOfFile3`, called with its documented signature and no
    // extended parameters. `section.section` is a live section object; a NULL base asks the OS to
    // choose the address, and an allocation type of 0 asks for an ordinary view rather than a
    // placeholder replacement. The return value is checked before anything is read through it.
    let view = unsafe {
        map3(
            section.section,
            GetCurrentProcess(),
            std::ptr::null(),
            offset,
            size,
            0,
            flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.is_null() {
        let code = last_error();
        return Err(match code {
            MISALIGNED_OS_ERROR => VmError::Misaligned {
                operation: "map_section",
                what: "section offset",
                value: offset,
                required: allocation_granularity() as u64,
                os_equivalent: OsError(code),
            },
            _ => VmError::Os { operation: "map_section", address: 0, size, source: OsError(code) },
        });
    }
    Ok(view as usize)
}

/// Closes a handle unless ownership is taken out of it. Keeps the error paths of
/// [`open_file_for_mapping`] from leaking the file handle.
struct HandleGuard(HANDLE);

impl HandleGuard {
    fn into_raw(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: the guard owns exactly this handle, which is live and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

// -------------------------------------------------------------------------------------------
// Unit tests for the decisions that cannot be reached through the public seam yet
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive table for [`resolved_protection`]. This is a unit test rather than an
    /// integration test because the shared-writable case needs a pagefile-backed section with two
    /// views, which is the D12 code arena and is deliberately not implemented yet. The decision it
    /// depends on can still be pinned down now, so that the arena does not have to discover it.
    #[test]
    fn read_write_stays_shared_on_a_shared_view_and_becomes_copy_on_write_on_a_private_one() {
        use RegionFlavour::{Private, PrivateView, SharedWritableView};

        // The case that matters: ReadWrite must not become PAGE_WRITECOPY on a shared view, or a
        // dual-mapped code arena's writes would be privatised and never reach its RX view.
        assert_eq!(resolved_protection(Protection::ReadWrite, SharedWritableView), PAGE_READWRITE);
        assert_eq!(resolved_protection(Protection::ReadWrite, PrivateView), PAGE_WRITECOPY);
        assert_eq!(resolved_protection(Protection::ReadWrite, Private), PAGE_READWRITE);

        // Every other variant is the same whatever the region is.
        for flavour in [Private, PrivateView, SharedWritableView] {
            assert_eq!(resolved_protection(Protection::None, flavour), PAGE_NOACCESS, "{flavour:?}");
            assert_eq!(resolved_protection(Protection::Read, flavour), PAGE_READONLY, "{flavour:?}");
            assert_eq!(
                resolved_protection(Protection::ReadExecute, flavour),
                PAGE_EXECUTE_READ,
                "{flavour:?}"
            );
        }

        // And no Protection can ever resolve to a writable *and* executable Win32 flag, whatever
        // the region is. This is the W^X invariant restated at the layer that actually talks to
        // the OS, where a mis-mapped flag could reintroduce it without any API change.
        for protection in Protection::ALL {
            for flavour in [Private, PrivateView, SharedWritableView] {
                let flags = resolved_protection(protection, flavour);
                assert_ne!(flags, PAGE_EXECUTE_READWRITE, "{protection} on {flavour:?}");
                assert_ne!(flags, PAGE_EXECUTE_WRITECOPY, "{protection} on {flavour:?}");
            }
        }
    }

    /// `PAGE_EXECUTE_WRITECOPY`, referenced only to assert that nothing ever resolves to it.
    const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;

    /// A region we cannot describe is treated as private, so that the OS gets to reject a bad
    /// address instead of this module guessing at it.
    #[test]
    fn an_undescribable_address_is_classified_private() {
        // Address 0 is never mapped, and VirtualQuery describes it as FREE rather than failing,
        // so this pins the behaviour of both branches of `region_flavour`'s fallback.
        assert_eq!(region_flavour(0), RegionFlavour::Private);
    }
}
