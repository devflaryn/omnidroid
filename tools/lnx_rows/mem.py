"""lnx-vm- / lnx-fault-: the Linux virtual-memory seam, the guest-fault seam, and pager precedence.

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.

Run under the build lock:
    flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-vm
    flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-fault
"""

VM_LINUX = "crates/omni-platform/src/vm/linux.rs"
FAULT_LINUX = "crates/omni-platform/src/fault/linux.rs"
FAULT_MOD = "crates/omni-platform/src/fault/mod.rs"
CPU_DYN = "crates/omni-cpu/src/dynarmic/mod.rs"


def platform(test, *extra):
    return ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast", "--test", test,
            *extra]


VM_TESTS = platform("vm_linux")
COMMIT_TESTS = platform("vm_commit_charge_linux")
FAULT_TESTS = platform("fault_linux")
CHAIN_TESTS = platform("fault_chain_linux")
FAULT_UNIT = ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast", "--lib",
              "fault::linux"]
PRECEDENCE = ["cargo", "test", "-p", "omni-cpu", "--release", "--no-fail-fast", "--test",
              "pager_precedence_linux"]

ROWS = [
    # ---- decommit must really return memory -----------------------------------------------------
    # The fix: decommit is a fresh PROT_NONE mapping over the range, the one call measured to return
    # both RSS and VM_ACCOUNT. The revert is MADV_DONTNEED + mprotect(PROT_NONE): the RSS comes back,
    # the commit charge does not -- Linux's MEM_RESET.
    ("lnx-vm-A1", "A", "decommit frees the pages with MADV_DONTNEED and keeps the commit charge",
     VM_LINUX,
     """    // SAFETY: the ledger says the range is inside a live plain reservation; the caller's contract is
    // that nothing holds a reference into it.
    unsafe { placeholder_over(address, size) }.map_err(|code| os(OP, address, size, code))
}""",
     """    // SAFETY: the ledger says the range is inside a live plain reservation; the caller's contract is
    // that nothing holds a reference into it.
    unsafe {
        libc::madvise(address as *mut libc::c_void, size, libc::MADV_DONTNEED);
        posix::mprotect(address, size, libc::PROT_NONE)
    }
    .map_err(|code| os(OP, address, size, code))
}""",
     COMMIT_TESTS),
    ("lnx-vm-A2", "A", "decommit_to_placeholder only mprotects, so the pager's commits never come back",
     VM_LINUX,
     """    // SAFETY: the ledger says the whole range is private commit this process owns; the caller's
    // contract is that nothing holds a reference into it.
    unsafe { placeholder_over(address, size) }.map_err(|code| os(OP, address, size, code))?;""",
     """    // SAFETY: the ledger says the whole range is private commit this process owns; the caller's
    // contract is that nothing holds a reference into it.
    unsafe { posix::mprotect(address, size, libc::PROT_NONE) }
        .map_err(|code| os(OP, address, size, code))?;""",
     COMMIT_TESTS),
    # ---- the reservation: MAP_NORESERVE measured and rejected ----------------------------------
    # The over-correction the brief itself suggested: with MAP_NORESERVE the reservation is no
    # cheaper (PROT_NONE is not accountable either way) and commit by mprotect stops being charged.
    ("lnx-vm-B1", "B", "reservations are MAP_NORESERVE, so commit is never charged",
     VM_LINUX,
     "const RESERVE_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;",
     "const RESERVE_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;",
     COMMIT_TESTS),
    # ---- the ledger's refusals ------------------------------------------------------------------
    ("lnx-vm-A3", "A", "release trusts any descriptor, so a double release or a split parent is munmapped",
     VM_LINUX,
     """    let Some(&piece) = map.get(&base) else {
        return Err(refused(OP, base, len));
    };""",
     """    let piece = map.get(&base).copied().unwrap_or(Piece { len: requested, kind: Kind::Plain });""",
     VM_TESTS),
    ("lnx-vm-A4", "A", "a placeholder of any size is replaced, with no exact-size check",
     VM_LINUX,
     """        Some(piece) if piece.kind == Kind::Placeholder && piece.len == size => Ok(()),""",
     """        Some(piece) if piece.kind == Kind::Placeholder => Ok(()),""",
     VM_TESTS),
    ("lnx-vm-A11", "A", "a split is not recorded, so its exact-size piece is refused as not a placeholder",
     VM_LINUX,
     """            retag(&mut map, piece_base, size, Kind::Placeholder);
            Ok(())""",
     """            let _ = size;
            Ok(())""",
     VM_TESTS),
    ("lnx-vm-A5", "A", "unmap of part of a view is carried out instead of refused",
     VM_LINUX,
     """            if piece.len != size {
                return Err(VmError::ViewSizeMismatch {""",
     """            if piece.len != size && false {
                return Err(VmError::ViewSizeMismatch {""",
     VM_TESTS),
    # ---- executability decided at open ----------------------------------------------------------
    ("lnx-vm-A6", "A", "a view of a non-executable file is raised to r-x",
     VM_LINUX,
     """            Kind::View { executable: false } if protection.is_executable() => {""",
     """            Kind::View { executable: false } if protection.is_executable() && false => {""",
     VM_TESTS),
    ("lnx-vm-B2", "B", "every file view is refused r-x, the executable library's text included",
     VM_LINUX,
     """            Kind::View { executable: false } if protection.is_executable() => {""",
     """            Kind::View { .. } if protection.is_executable() => {""",
     VM_TESTS),
    # ---- sharing ---------------------------------------------------------------------------------
    ("lnx-vm-A7", "A", "a shared file's view is MAP_PRIVATE, so its writes never reach the file",
     VM_LINUX,
     "    let sharing = if file.shared { libc::MAP_SHARED } else { libc::MAP_PRIVATE };",
     "    let sharing = libc::MAP_PRIVATE;",
     VM_TESTS),
    ("lnx-vm-B3", "B", "every file view is MAP_SHARED, so a copy-on-write view writes the cache file",
     VM_LINUX,
     "    let sharing = if file.shared { libc::MAP_SHARED } else { libc::MAP_PRIVATE };",
     "    let sharing = libc::MAP_SHARED;",
     VM_TESTS),
    ("lnx-vm-A8", "A", "a read-only descriptor is accepted as shareable and fails only at map time",
     VM_LINUX,
     "    if !writable {",
     "    if !writable && false {",
     VM_TESTS),
    ("lnx-vm-A9", "A", "the D12 section's views are MAP_PRIVATE, so the RX view never sees a write",
     VM_LINUX,
     "posix::mmap(0, size, posix::prot_bits(protection), libc::MAP_SHARED, section.fd.as_raw_fd(), offset)",
     "posix::mmap(0, size, posix::prot_bits(protection), libc::MAP_PRIVATE, section.fd.as_raw_fd(), offset)",
     VM_TESTS),
    # ---- commit charge ---------------------------------------------------------------------------
    ("lnx-vm-A10", "A", "commit charge sums every VMA, reservations included",
     VM_LINUX,
     """            if flags.split_whitespace().any(|flag| flag == "ac") {""",
     """            if !flags.is_empty() {""",
     COMMIT_TESTS),

    # ---- first place over dynarmic ---------------------------------------------------------------
    # The fix is two halves: the platform re-asserts first place, and omni-cpu asks it to after every
    # jit. Each is reverted on its own; either one puts dynarmic's handler in front of the pager at
    # the first jit, and the guest's demand faults then go through the fastmem fallback.
    ("lnx-fault-A1", "A", "reassert_precedence is a no-op on Linux, as on Windows",
     FAULT_MOD,
     """    #[cfg(target_os = "linux")]
    {
        backend::reassert_precedence()
    }""",
     """    #[cfg(target_os = "linux")]
    {
        Ok(())
    }""",
     PRECEDENCE),
    ("lnx-fault-A2", "A", "omni-cpu builds a jit and never re-asserts, so dynarmic's handler runs first",
     CPU_DYN,
     """        if shared.owns_guest_paging {
            if let Err(error) = DemandPager::reassert_precedence() {""",
     """        if shared.owns_guest_paging && false {
            if let Err(error) = DemandPager::reassert_precedence() {""",
     PRECEDENCE),
    ("lnx-fault-A3", "A", "a re-entered handler dispatches again instead of forwarding down: the loop",
     FAULT_LINUX,
     """    if CHAINING.with(Cell::get) > 0 {""",
     """    if CHAINING.with(Cell::get) > 0 && false {""",
     CHAIN_TESTS),
    ("lnx-fault-B1", "B", "a re-assertion re-installs even when first, and forwards to itself",
     FAULT_LINUX,
     """    let current = query(signal, index)?;
    if is_ours(&current) {
        return Ok(());
    }""",
     """    let current = query(signal, index)?;""",
     CHAIN_TESTS),
    ("lnx-fault-A4", "A", "a sent signal is dispatched to handlers as though it were a fault",
     FAULT_LINUX,
     """    if !is_page_fault {
        return None;
    }""",
     """    let _ = is_page_fault;""",
     FAULT_TESTS),
    ("lnx-fault-A5", "A", "the access kind is read from the present bit, not the write bit",
     FAULT_LINUX,
     "const PF_WRITE: i64 = 1 << 1;",
     "const PF_WRITE: i64 = 1 << 0;",
     FAULT_TESTS),
    ("lnx-fault-A6", "A", "release does not wait for a dispatch inside the handler",
     FAULT_LINUX,
     """    slot.handler.store(DRAINING, Ordering::SeqCst);
    if slot.active.load(Ordering::SeqCst) != 0 {""",
     """    slot.handler.store(DRAINING, Ordering::SeqCst);
    if slot.active.load(Ordering::SeqCst) != 0 && false {""",
     FAULT_UNIT),
    ("lnx-fault-A7", "A", "a slot being drained is unpublished with 0, so install can take it mid-drain",
     FAULT_LINUX,
     """    let Some(slot) = SLOTS.get(slot) else { return };
    slot.handler.store(DRAINING, Ordering::SeqCst);""",
     """    let Some(slot) = SLOTS.get(slot) else { return };
    slot.handler.store(0, Ordering::SeqCst);""",
     FAULT_UNIT),
    ("lnx-fault-A8", "A", "SIG_DFL at the end of the chain returns without resetting: an endless fault",
     FAULT_LINUX,
     """    default.sa_sigaction = libc::SIG_DFL;
    // SAFETY: installs SIG_DFL; async-signal-safe.
    unsafe { libc::sigaction(signal, &default, core::ptr::null_mut()) };""",
     """    default.sa_sigaction = libc::SIG_DFL;
    let _ = &default;""",
     CHAIN_TESTS),
]
