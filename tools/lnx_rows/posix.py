"""lnx-proc-, lnx-fs-, lnx-net-, lnx-clock-: the Linux halves of the process, fs and net seams, and
the clock seam's non-Windows timer-resolution arm.

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.

Commands are one test target each, so a row's catch list names the file that pins it. The rows
that mutate `fs/path.rs` and `clock.rs` mutate SHARED code, temporarily, to prove that the Linux
tests are detectors for it -- the files themselves are not changed by this worker.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

PLAT = "crates/omni-platform/src"
PROC_LINUX = f"{PLAT}/process/linux.rs"
PROC_UNIX = f"{PLAT}/process/unix.rs"
FS_LINUX = f"{PLAT}/fs/linux.rs"
FS_UNIX = f"{PLAT}/fs/unix.rs"
FS_PATH = f"{PLAT}/fs/path.rs"
NET_LINUX = f"{PLAT}/net/linux.rs"
NET_UNIX = f"{PLAT}/net/unix.rs"
CLOCK = f"{PLAT}/clock.rs"


def _test(*selector):
    return ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast", *selector]


# The library's own unit tests for one module (the positional is libtest's filter).
PROC_TESTS = _test("--lib", "process::")
FS_UNIT = _test("--lib", "fs::")
NET_UNIT = _test("--lib", "net::")
# The Linux integration files.
FS_TESTS = _test("--test", "fs_linux")
NET_TESTS = _test("--test", "net_loopback_linux")
CLOCK_TESTS = _test("--test", "clock_linux")

ROWS = [
    # ---- process ------------------------------------------------------------------------------
    # The getrandom loop: one short fill taken as the whole answer. Only the raw syscall returns
    # short on this host (glibc 2.43 answers from the vDSO), so the test drives the loop with it.
    ("lnx-proc-A1", "A", "getrandom's short fill is taken as the whole buffer",
     PROC_LINUX,
     """        filled += got;""",
     """        filled = out.len();""",
     PROC_TESTS),
    # EINTR reported instead of retried: unreachable through the kernel after the CRNG is seeded,
    # so the test drives it through fill_from with getrandom(2)'s documented contract.
    ("lnx-proc-A2", "A", "getrandom's EINTR is reported rather than retried",
     PROC_LINUX,
     """            if errno == libc::EINTR {
                #[cfg(test)]""",
     """            if errno == libc::EINTR && false {
                #[cfg(test)]""",
     PROC_TESTS),
    ("lnx-proc-A3", "A", "sched_getcpu replaced by the believable constant 0",
     PROC_LINUX,
     """    let cpu = unsafe { libc::sched_getcpu() };""",
     """    let cpu = 0;""",
     PROC_TESTS),
    # PRIO_PROCESS with the pid names the process's main thread, not the caller.
    ("lnx-proc-A4", "A", "the nice value is applied to the main thread (getpid) instead of the caller",
     PROC_LINUX,
     """    let tid = unsafe { libc::gettid() };""",
     """    let tid = unsafe { libc::getpid() };""",
     PROC_TESTS),
    ("lnx-proc-A5", "A", "a refused setpriority is reported as success",
     PROC_LINUX,
     """    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, this_thread(), nice) };
    if rc != 0 {""",
     """    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, this_thread(), nice) };
    if rc != 0 && false {""",
     PROC_TESTS),
    # The over-correction: answer the device's success for the host's EACCES, because AOSP's
    # RLIMIT_NICE 40 would have allowed it.
    ("lnx-proc-B1", "B", "EACCES from a forbidden raise is swallowed to match a device",
     PROC_LINUX,
     """    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, this_thread(), nice) };
    if rc != 0 {""",
     """    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, this_thread(), nice) };
    if rc != 0 && last_errno() != libc::EACCES {""",
     PROC_TESTS),
    ("lnx-proc-A6", "A", "the DMI vendor is returned untrimmed, newline and all",
     PROC_LINUX,
     """    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\\0');""",
     """    let text = &*text;""",
     PROC_TESTS),
    # A thread clock for the process clock: the multi-thread test's CPU-outruns-wall assertion.
    ("lnx-proc-A7", "A", "cpu_time reads the calling thread's clock, not the process's",
     PROC_UNIX,
     """libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &raw mut now)""",
     """libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut now)""",
     PROC_TESTS),

    # ---- fs -----------------------------------------------------------------------------------
    ("lnx-fs-A1", "A", "pread loops until the buffer is full instead of reporting a short read",
     FS_UNIX,
     """    file.read_at(buf, offset).map_err(|error| FsError::io("pread", "a descriptor", &error))""",
     """    let mut filled = 0;
    while filled < buf.len() {
        match file.read_at(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) => return Err(FsError::io("pread", "a descriptor", &error)),
        }
    }
    Ok(filled)""",
     FS_TESTS),
    ("lnx-fs-A2", "A", "pwrite is a seek and a write, which moves the descriptor's offset",
     FS_UNIX,
     """    file.write_at(buf, offset).map_err(|error| FsError::io("pwrite", "a descriptor", &error))""",
     """    use std::io::{Seek, Write};
    let mut handle: &File = file;
    handle
        .seek(std::io::SeekFrom::Start(offset))
        .and_then(|_| handle.write(buf))
        .map_err(|error| FsError::io("pwrite", "a descriptor", &error))""",
     FS_TESTS),
    # posix_fallocate returns the error; a body reading errno after a -1 that never comes answers Ok.
    ("lnx-fs-A3", "A", "posix_fallocate's error is read from errno after -1, so every failure is Ok",
     FS_LINUX,
     """        match rc {
            0 => return Ok(()),""",
     """        let rc = if rc == -1 { std::io::Error::last_os_error().raw_os_error().unwrap_or(0) } else { 0 };
        match rc {
            0 => return Ok(()),""",
     FS_UNIT),
    # ftruncate for posix_fallocate: the file has its length and no blocks.
    ("lnx-fs-A4", "A", "fallocate extends with ftruncate, leaving a sparse file",
     FS_LINUX,
     """        let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) };""",
     """        let rc = unsafe { libc::ftruncate(file.as_raw_fd(), len) };""",
     FS_TESTS),
    ("lnx-fs-A5", "A", "statvfs reports f_bsize as the unit the block counts are in",
     FS_UNIX,
     """    let block_size = u64::from(record.f_frsize);""",
     """    let block_size = u64::from(record.f_bsize);""",
     FS_UNIT),
    ("lnx-fs-A6", "A", "statvfs ignores ST_RDONLY",
     FS_UNIX,
     """        read_only: record.f_flag & libc::ST_RDONLY != 0,""",
     """        read_only: false,""",
     FS_UNIT),
    # SHARED fs/path.rs, rule 5: only the final component is checked for a link, so a directory
    # link on the way is followed. First executed on this host.
    ("lnx-fs-A7", "A", "rule 5 checks only the final component, following a directory link on the way",
     FS_PATH,
     """        match std::fs::symlink_metadata(&host) {""",
     """        match if index == last { std::fs::symlink_metadata(&host) } else { std::fs::metadata(&host) } {""",
     FS_TESTS),
    # Rule 5's lstat exception widened to every call: a final link is followed by open and stat.
    ("lnx-fs-A8", "A", "every call may end on a link, not only lstat",
     FS_PATH,
     """                if index == last && final_link == FinalLink::Describe {""",
     """                if index == last {""",
     FS_TESTS),
    # The over-correction: lstat refuses its final link too.
    ("lnx-fs-B1", "B", "lstat refuses a link as its final component",
     FS_PATH,
     """                if index == last && final_link == FinalLink::Describe {""",
     """                if false {""",
     FS_TESTS),

    # ---- net ----------------------------------------------------------------------------------
    ("lnx-net-A1", "A", "the keep-alive idle time is written to TCP_KEEPINTVL",
     NET_LINUX,
     """    set_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, value, "setsockopt", "setsockopt(TCP_KEEPIDLE)")""",
     """    set_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, value, "setsockopt", "setsockopt(TCP_KEEPIDLE)")""",
     NET_TESTS),
    # Windows' number for TCP_KEEPCNT (16) passed on Linux, where 16 is TCP_THIN_LINEAR_TIMEOUTS.
    ("lnx-net-A2", "A", "Winsock's TCP_KEEPCNT number (16) is used on Linux",
     NET_LINUX,
     """    set_int(inner, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, value, "setsockopt", "setsockopt(TCP_KEEPCNT)")""",
     """    set_int(inner, libc::IPPROTO_TCP, 16, value, "setsockopt", "setsockopt(TCP_KEEPCNT)")""",
     NET_UNIT),
    # SO_RCVBUF halved "to match what was asked": a buffer the kernel does not have.
    ("lnx-net-B1", "B", "the doubled SO_RCVBUF/SO_SNDBUF is halved to match the request",
     NET_UNIX,
     """    usize::try_from(value).map_err(|_| {
        // A negative buffer size""",
     """    usize::try_from(value / 2).map_err(|_| {
        // A negative buffer size""",
     NET_TESTS),
    ("lnx-net-A3", "A", "poll drops POLLHUP",
     NET_UNIX,
     """            hangup: fd.revents & libc::POLLHUP != 0,""",
     """            hangup: false,""",
     NET_TESTS),
    # POLLERR is reported unasked; masking it by the interest loses a reset to a reader.
    ("lnx-net-A4", "A", "poll masks POLLERR by the caller's interest",
     NET_UNIX,
     """            error: fd.revents & libc::POLLERR != 0,""",
     """            error: entry.interest.writable && fd.revents & libc::POLLERR != 0,""",
     NET_TESTS),
    ("lnx-net-A5", "A", "EINPROGRESS from a non-blocking connect is reported as a failure",
     NET_UNIX,
     """        libc::EINPROGRESS | libc::EALREADY => Ok(ConnectProgress::InProgress),""",
     """        libc::EALREADY => Ok(ConnectProgress::InProgress),""",
     NET_TESTS),
    ("lnx-net-A6", "A", "EALREADY from a second connect is reported as a failure",
     NET_UNIX,
     """        libc::EINPROGRESS | libc::EALREADY => Ok(ConnectProgress::InProgress),""",
     """        libc::EINPROGRESS => Ok(ConnectProgress::InProgress),""",
     NET_TESTS),
    ("lnx-net-A7", "A", "EISCONN from a connect on a connected socket is reported as a failure",
     NET_UNIX,
     """        libc::EISCONN => Ok(ConnectProgress::Connected),
        _ => Err(errno_error("connect", address.to_string(), "connect")),""",
     """        _ => Err(errno_error("connect", address.to_string(), "connect")),""",
     NET_TESTS),
    ("lnx-net-A8", "A", "a socket is created non-blocking, against the seam's contract",
     NET_LINUX,
     """    let fd = unsafe { libc::socket(address_family(family), kind | libc::SOCK_CLOEXEC, 0) };""",
     """    let fd = unsafe { libc::socket(address_family(family), kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };""",
     NET_UNIT),
    ("lnx-net-A9", "A", "a socket is created without close-on-exec",
     NET_LINUX,
     """    let fd = unsafe { libc::socket(address_family(family), kind | libc::SOCK_CLOEXEC, 0) };""",
     """    let fd = unsafe { libc::socket(address_family(family), kind, 0) };""",
     NET_UNIT),
    ("lnx-net-A10", "A", "an accepted socket is not close-on-exec",
     NET_LINUX,
     """        libc::accept4(raw(inner), core::ptr::null_mut(), core::ptr::null_mut(), libc::SOCK_CLOEXEC)""",
     """        libc::accept4(raw(inner), core::ptr::null_mut(), core::ptr::null_mut(), 0)""",
     NET_UNIT),
    # Winsock's PMTUD_STATE numbering on Linux: DO is 1 there, which is WANT here.
    ("lnx-net-A11", "A", "path-MTU DO is Winsock's number (1), which Linux reads as WANT",
     NET_LINUX,
     """        (IpFamily::V4, PathMtu::Do) => libc::IP_PMTUDISC_DO,""",
     """        (IpFamily::V4, PathMtu::Do) => 1,""",
     NET_TESTS),
    ("lnx-net-A12", "A", "IP_PMTUDISC_WANT, the kernel's default, is not the seam's 'not set'",
     NET_LINUX,
     """    if value == want {
        return Ok(None);
    }""",
     """    let _ = want;""",
     NET_TESTS),
    ("lnx-net-A13", "A", "SO_ERROR is read and thrown away",
     NET_UNIX,
     """    Ok((value != 0).then(|| kind_from_raw(value)))""",
     """    let _ = value;
    Ok(None)""",
     NET_TESTS),
    ("lnx-net-A14", "A", "EMSGSIZE is left unclassified",
     NET_UNIX,
     """        libc::EMSGSIZE => NetErrorKind::MessageSize,""",
     """        libc::EMSGSIZE => NetErrorKind::Other,""",
     NET_TESTS),
    # The over-correction: Windows' WSAESHUTDOWN row copied, although Linux answers EPIPE itself.
    ("lnx-net-B2", "B", "ESHUTDOWN is folded into EPIPE, copying a Winsock-only row",
     NET_UNIX,
     """        libc::EPIPE => NetErrorKind::BrokenPipe,""",
     """        libc::EPIPE | libc::ESHUTDOWN => NetErrorKind::BrokenPipe,""",
     NET_UNIT),
    ("lnx-net-A15", "A", "the v4 address is written byte-reversed into sin_addr",
     NET_UNIX,
     """            storage.v4.sin_addr.s_addr = u32::from_ne_bytes(octets);""",
     """            storage.v4.sin_addr.s_addr = u32::from_be_bytes(octets);""",
     NET_TESTS),
    ("lnx-net-A16", "A", "the port is not byte-swapped into sin_port",
     NET_UNIX,
     """            storage.v4.sin_port = port.to_be();""",
     """            storage.v4.sin_port = port;""",
     NET_TESTS),
    ("lnx-net-A17", "A", "getifaddrs' IPv6 entries are skipped",
     NET_UNIX,
     """            } else if family == libc::AF_INET6 {""",
     """            } else if false {""",
     NET_TESTS),
    ("lnx-net-A18", "A", "poll ignores its timeout and returns at once",
     NET_UNIX,
     """        let wait = poll_millis(deadline.saturating_duration_since(Instant::now()));""",
     """        let wait = poll_millis(Duration::ZERO.min(deadline.saturating_duration_since(Instant::now())));""",
     NET_TESTS),
    # The over-correction for the seam's "never longer than asked": the timeout rounded down, so a
    # guest's poll(150 ms) comes back after 149 -- an early timeout POSIX's "at least" forbids.
    ("lnx-net-B3", "B", "poll's timeout is rounded down to whole milliseconds",
     NET_UNIX,
     """    libc::c_int::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(libc::c_int::MAX)""",
     """    libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX)""",
     NET_TESTS),
    ("lnx-net-A19", "A", "SO_LINGER's on/off is dropped, so the abortive close is an ordinary one",
     NET_UNIX,
     """    let value = libc::linger { l_onoff: libc::c_int::from(on), l_linger: 0 };""",
     """    let value = libc::linger { l_onoff: 0 * libc::c_int::from(on), l_linger: 0 };""",
     NET_TESTS),

    # ---- clock (SHARED clock.rs, temporarily) --------------------------------------------------
    ("lnx-clock-A1", "A", "the non-Windows timer-resolution arm refuses",
     CLOCK,
     """fn backend_raise(_period_ms: u32) -> Result<(), TimerResolutionError> {
    Ok(())
}""",
     """fn backend_raise(_period_ms: u32) -> Result<(), TimerResolutionError> {
    Err(TimerResolutionError { requested_ms: _period_ms, code: 97 })
}""",
     CLOCK_TESTS),
    # The over-correction: a sleep rounded up to a 4 ms tick "to match" a tick-grained host.
    ("lnx-clock-B1", "B", "a sleep is rounded up to a 4 ms tick",
     CLOCK,
     """    std::thread::sleep(duration);""",
     """    std::thread::sleep(duration.max(Duration::from_millis(4)));""",
     CLOCK_TESTS),
]
