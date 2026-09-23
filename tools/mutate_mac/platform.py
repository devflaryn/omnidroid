"""macOS rows: the platform workstream (omni-platform vm/fs/net/process on macOS). Pure data; see
`__init__.py`."""

VM = "crates/omni-platform/src/vm/macos.rs"

VM_TESTS = [
    "cargo", "test", "-p", "omni-platform",
    "--test", "vm_macos", "--test", "vm_footprint_macos",
    "--no-fail-fast",
]

ROWS = [
    ("mac-plat-A1", "A", "decommit advises the pages free instead of replacing them, so they keep "
     "their contents",
     VM,
     """            fresh_reserved(address, size).map_err(|code| VmError::Os {
                operation: "decommit",""",
     """            // SAFETY: mutation.
            let _ = unsafe {
                libc::madvise(address as *mut libc::c_void, size, libc::MADV_FREE_REUSABLE)
            };
            mprotect(address, size, libc::PROT_NONE).map_err(|code| VmError::Os {
                operation: "decommit",""",
     VM_TESTS),
    ("mac-plat-A2", "A", "an executable file view is left read-only instead of raised to r-x",
     VM,
     """            Protection::ReadExecute => (libc::PROT_READ, Some(Protection::ReadExecute)),""",
     """            Protection::ReadExecute => (libc::PROT_READ, None),""",
     VM_TESTS),
    ("mac-plat-A3", "A", "commit_placeholder replaces a placeholder of any size",
     VM,
     """        Some(entry) if entry.kind == Kind::Placeholder && entry.len == size => {}
        _ => {
            return Err(VmError::PlaceholderNotExactSize {
                operation: "commit_placeholder",""",
     """        Some(entry) if entry.kind == Kind::Placeholder => {}
        _ => {
            return Err(VmError::PlaceholderNotExactSize {
                operation: "commit_placeholder",""",
     VM_TESTS),
    ("mac-plat-A4", "A", "unmap accepts an address inside a view as if it were the base",
     VM,
     """            if base != address {
                return Err(VmError::NotViewBase {""",
     """            if base != address && false {
                return Err(VmError::NotViewBase {""",
     VM_TESTS),
    ("mac-plat-A5", "A", "release frees whatever starts at the base, whatever its extent",
     VM,
     """    if entry.len != requested {
        return Err(VmError::ReleaseExtentMismatch { address: base, requested, actual: entry.len });
    }""",
     "",
     VM_TESTS),
    ("mac-plat-A6", "A", "a view of a non-executable file can be protected to execute",
     VM,
     """        Kind::View { executable: false } if protection.is_executable() => {
            return Err(refused("protect", address, size, libc::EACCES))
        }""",
     "",
     VM_TESTS),
    ("mac-plat-A7", "A", "a read-only descriptor is accepted as the backing of a shared view",
     VM,
     """    if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDWR {""",
     """    if flags < 0 {""",
     VM_TESTS),
    ("mac-plat-A8", "A", "decommitted pages stay accessible (reserved read-write, not PROT_NONE)",
     VM,
     """            address as *mut libc::c_void,
            size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,""",
     """            address as *mut libc::c_void,
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,""",
     VM_TESTS),
    ("mac-plat-A9", "A", "phys_footprint is read from the wrong ledger field (resident size)",
     VM,
     """pub(super) fn process_commit_charge() -> VmResult<u64> {
    Ok(task_vm_info()?.phys_footprint)
}""",
     """pub(super) fn process_commit_charge() -> VmResult<u64> {
    Ok(task_vm_info()?.resident_size)
}""",
     VM_TESTS),
    ("mac-plat-B1", "B", "commit backs every page at once (touches them), spending memory the "
     "guest has not used",
     VM,
     """        Some((_, entry)) if matches!(entry.kind, Kind::Plain | Kind::Private) => {
            mprotect(address, size, prot(protection)).map_err(|code| VmError::Os {
                operation: "commit",""",
     """        Some((_, entry)) if matches!(entry.kind, Kind::Plain | Kind::Private) => {
            let _ = mprotect(address, size, libc::PROT_READ | libc::PROT_WRITE);
            for offset in (0..size).step_by(page_size()) {
                // SAFETY: mutation.
                unsafe { ((address + offset) as *mut u8).write_volatile(0) };
            }
            mprotect(address, size, prot(protection)).map_err(|code| VmError::Os {
                operation: "commit",""",
     VM_TESTS),
    ("mac-plat-B2", "B", "reservations are made read-write (backed on touch) instead of PROT_NONE",
     VM,
     """            std::ptr::null_mut(),
            over,
            libc::PROT_NONE,""",
     """            std::ptr::null_mut(),
            over,
            libc::PROT_READ | libc::PROT_WRITE,""",
     VM_TESTS),
]
