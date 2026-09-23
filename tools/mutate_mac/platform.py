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

FS = "crates/omni-platform/src/fs/macos.rs"
PROCESS = "crates/omni-platform/src/process/macos.rs"
LIB_TESTS = ["cargo", "test", "-p", "omni-platform", "--lib", "--no-fail-fast"]

ROWS += [
    ("mac-plat-C1", "A", "fallocate only moves the end of file, leaving a sparse tail",
     FS,
     """    // SAFETY: `store` is a live fstore_t the call reads and updates; the descriptor is `file`'s.
    let mut rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };""",
     """    let _ = &mut store;
    let mut rc = 0;""",
     LIB_TESTS),
    ("mac-plat-C2", "A", "fallocate to a smaller end shrinks the file",
     FS,
     """    if end <= size {
        return Ok(());
    }""",
     """    if end == size {
        return Ok(());
    }""",
     LIB_TESTS),
    ("mac-plat-C3", "A", "pread moves the descriptor offset (seek then read)",
     FS,
     """    file.read_at(buf, offset).map_err(|error| FsError::io("pread", "a descriptor", &error))""",
     """    use std::io::{Read, Seek, SeekFrom};
    let mut handle: &File = file;
    handle.seek(SeekFrom::Start(offset)).map_err(|error| FsError::io("pread", "a descriptor", &error))?;
    handle.read(buf).map_err(|error| FsError::io("pread", "a descriptor", &error))""",
     LIB_TESTS),
    ("mac-plat-C4", "A", "statvfs never reports a read-only volume",
     FS,
     """        read_only: stats.f_flags & libc::MNT_RDONLY as u32 != 0,""",
     """        read_only: false,""",
     LIB_TESTS),
    ("mac-plat-C5", "A", "sched_getcpu answers a constant 0",
     PROCESS,
     """    u32::try_from(cpu).map_err(|_| ProcessError::Errno {""",
     """    let cpu: libc::size_t = 0;
    u32::try_from(cpu).map_err(|_| ProcessError::Errno {""",
     LIB_TESTS),
    ("mac-plat-C6", "A", "the audio band (nice -16) gets USER_INITIATED instead of USER_INTERACTIVE",
     PROCESS,
     """        i32::MIN..=-11 => QOS_CLASS_USER_INTERACTIVE,""",
     """        i32::MIN..=-11 => QOS_CLASS_USER_INITIATED,""",
     LIB_TESTS),
    ("mac-plat-C7", "B", "nice 0 raises the thread above where it started",
     PROCESS,
     """        0 => QOS_CLASS_DEFAULT,""",
     """        0 => QOS_CLASS_USER_INITIATED,""",
     LIB_TESTS),
    ("mac-plat-C8", "A", "process CPU time is read from the thread's clock, not the process's",
     PROCESS,
     """    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now) } != 0 {""",
     """    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) } != 0 {""",
     LIB_TESTS),
]

NET = "crates/omni-platform/src/net/macos.rs"
NET_TESTS = [
    "cargo", "test", "-p", "omni-platform",
    "--test", "net_loopback", "--test", "net_macos", "--test", "net_seam", "--lib",
    "--no-fail-fast",
]

ROWS += [
    ("mac-plat-D1", "A", "sockets are created without SO_NOSIGPIPE, so a write to a dead peer "
     "kills the process",
     NET,
     """    set_int(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1).map_err(|code| {
        NetError::kinded(
            operation,""",
     """    set_int(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 0).map_err(|code| {
        NetError::kinded(
            operation,""",
     NET_TESTS),
    ("mac-plat-D2", "A", "accepted sockets are left without SO_NOSIGPIPE",
     NET,
     """    set_int(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1).map_err(|code| {
        NetError::kinded(
            "accept",""",
     """    set_int(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 0).map_err(|code| {
        NetError::kinded(
            "accept",""",
     NET_TESTS),
    ("mac-plat-D3", "A", "a hung-up socket is not reported writable, so a refused connect never "
     "settles",
     NET,
     """            writable: revents & libc::POLLOUT != 0 || (hangup && entry.interest.writable),""",
     """            writable: revents & libc::POLLOUT != 0,""",
     NET_TESTS),
    ("mac-plat-D4", "A", "EINPROGRESS from a non-blocking connect is reported as a failure",
     NET,
     """        libc::EINPROGRESS | libc::EALREADY | libc::EINTR => Ok(ConnectProgress::InProgress),""",
     """        libc::EALREADY | libc::EINTR => Ok(ConnectProgress::InProgress),""",
     NET_TESTS),
    ("mac-plat-D5", "A", "the v4 address is byte-swapped into sin_addr",
     NET,
     """                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(octets) },""",
     """                sin_addr: libc::in_addr { s_addr: u32::from_be_bytes(octets) },""",
     NET_TESTS),
    ("mac-plat-D6", "A", "the keep-alive idle time uses Linux's option number (TCP_KEEPIDLE = 4)",
     NET,
     """    set(inner, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, value, "setsockopt(IPPROTO_TCP, TCP_KEEPALIVE)")""",
     """    set(inner, libc::IPPROTO_TCP, 4, value, "setsockopt(IPPROTO_TCP, TCP_KEEPALIVE)")""",
     NET_TESTS),
    ("mac-plat-D7", "A", "a reused descriptor inherits the previous socket's path-MTU mode",
     NET,
     """    PathMtuRecord::born(fd);
    if let Err(error) = prepare(fd, "socket", &what) {""",
     """    if let Err(error) = prepare(fd, "socket", &what) {""",
     NET_TESTS),
    ("mac-plat-D8", "A", "an interface address listed on two interfaces is reported twice",
     NET,
     """                        let ip = std::net::IpAddr::from(v6.sin6_addr.s6_addr);
                        if !addresses.contains(&ip) {
                            addresses.push(ip);
                        }""",
     """                        let ip = std::net::IpAddr::from(v6.sin6_addr.s6_addr);
                        addresses.push(ip);""",
     NET_TESTS),
    ("mac-plat-D9", "B", "a readiness wait of under a millisecond is rounded down to a spin",
     NET,
     """    let millis = timeout.as_micros().div_ceil(1000);""",
     """    let millis = timeout.as_micros() / 1000;""",
     NET_TESTS),
]
