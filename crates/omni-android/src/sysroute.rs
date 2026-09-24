//! Which arm64 Linux syscalls are answered by which libc import, and why each pairing is exact.
//!
//! # The rule
//!
//! A syscall is routed here **only when the kernel's ABI for it and the import's ABI are the same
//! call**: the same arguments in the same order and widths once the kernel's registers are read in
//! the table's order, the same structures at the pointers, and a result the boundary can convert
//! exactly -- the import answers `-1` and sets `errno`, the kernel answers `-errno`, and no routed
//! call can succeed with `-1` (a descriptor, a count, an offset, a mapping address, or zero). Where
//! bionic's wrapper *is* the syscall -- `read` is `__NR_read`, `access` is `faccessat(AT_FDCWD, ..)`
//! -- the handler that answers the wrapper answers the syscall.
//!
//! Both doors use this one table: a guest's own `SVC #0` (the boundary's `service_raw_syscall`,
//! kernel registers `x0`-`x5`) and libc's `syscall(number, ...)` (the same arguments one register
//! later). A number that is **not** here still goes to `bionic::procenv::syscall`, which answers
//! the few it emulates itself (futex, gettid, getrandom, rt_sigprocmask, statfs, fstatfs) and
//! refuses the rest by number and name.
//!
//! # What is deliberately not here
//!
//! * `readlinkat` (78), `getdents64` (61), `dup`/`dup3` (23/24), `readv` (65), `writev` (66): no
//!   bound import answers them. `readlink` and `writev` are imports of `libroblox.so` that nothing
//!   implements; `getdents64` has no equivalent at all (`readdir` walks a `DIR *`, not a buffer of
//!   records). A route to an unimplemented import would still refuse -- by the import's name, which
//!   says less than the syscall's.
//! * `exit` (93) and `exit_group` (94): ending a thread or the process from inside a syscall is not
//!   an answer the import path can give without deciding what happens to the guest's other threads,
//!   and `_exit`'s handler is the process-wide one, not the thread-wide `exit`.
//! * The `*at` calls with a descriptor-relative path: see [`Route::dirfd_path`].

use crate::error::AbiResult;
use crate::mem::{Blame, GuestMem};

/// `AT_FDCWD`: "relative to the working directory".
pub(crate) const AT_FDCWD: i32 = -100;
/// `AT_SYMLINK_NOFOLLOW`, `newfstatat`'s "describe the link, not its target".
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
/// `AT_NO_AUTOMOUNT`: do not trigger an automount. Nothing here automounts, so the flag describes a
/// state that already holds and is accepted.
const AT_NO_AUTOMOUNT: u64 = 0x800;

/// Which import answers a route.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target {
    /// Always this import.
    Import(&'static str),
    /// `newfstatat`: `stat`, or `lstat` when kernel argument 3 has `AT_SYMLINK_NOFOLLOW`.
    StatAt,
}

/// One routed syscall.
#[derive(Debug)]
pub(crate) struct Route {
    /// The asm-generic (arm64) number.
    pub(crate) number: u64,
    /// The kernel's name for it, for a refusal and the trace.
    pub(crate) name: &'static str,
    /// The import that answers it.
    pub(crate) target: Target,
    /// For each import argument in order, the kernel argument it is.
    pub(crate) args: &'static [u8],
    /// `(dirfd, path)` kernel arguments for an `*at` call, which is routed only when `dirfd` is
    /// `AT_FDCWD` or the path is absolute: the imports resolve against the working directory, and
    /// answering for another directory would describe the wrong file.
    pub(crate) dirfd_path: Option<(u8, u8)>,
}

/// The routed syscalls, each with why the kernel's call and the import's are the same.
pub(crate) static ROUTES: &[Route] = &[
    // bionic's `open` is `openat(AT_FDCWD, path, flags, mode)`. MEASURED: /proc/self/maps.
    Route { number: 56, name: "openat", target: Target::Import("open"), args: &[1, 2, 3], dirfd_path: Some((0, 1)) },
    // bionic's `close` is `__NR_close`; its EINTR-to-0 rewrite cannot arise here (no signals).
    Route { number: 57, name: "close", target: Target::Import("close"), args: &[0], dirfd_path: None },
    // bionic's `lseek` on LP64 is `__NR_lseek`: `off_t` is one 64-bit register on both sides.
    Route { number: 62, name: "lseek", target: Target::Import("lseek"), args: &[0, 1, 2], dirfd_path: None },
    // `__NR_read` exactly. MEASURED need: the worker's next death after fstatfs (2026-09-25).
    Route { number: 63, name: "read", target: Target::Import("read"), args: &[0, 1, 2], dirfd_path: None },
    // `__NR_write` exactly.
    Route { number: 64, name: "write", target: Target::Import("write"), args: &[0, 1, 2], dirfd_path: None },
    // LP64: `pread` and `pread64` are one function, the offset one register (no pair split on arm64).
    Route { number: 67, name: "pread64", target: Target::Import("pread"), args: &[0, 1, 2, 3], dirfd_path: None },
    // As `pread64`.
    Route { number: 68, name: "pwrite64", target: Target::Import("pwrite"), args: &[0, 1, 2, 3], dirfd_path: None },
    // `stat`/`lstat` are `fstatat(AT_FDCWD, path, buf, 0 or AT_SYMLINK_NOFOLLOW)`; arm64's kernel
    // `struct stat` is asm-generic's, which is bionic's (`files::STAT_BYTES`).
    Route { number: 79, name: "newfstatat", target: Target::StatAt, args: &[1, 2], dirfd_path: Some((0, 1)) },
    // `__NR_fstat` exactly, into the same `struct stat`.
    Route { number: 80, name: "fstat", target: Target::Import("fstat"), args: &[0, 1], dirfd_path: None },
    // bionic's `access` is `faccessat(AT_FDCWD, path, mode, 0)`; syscall 48 has no flags argument
    // (that is `faccessat2`, 439), so there is nothing in kernel argument 3 to honour.
    Route { number: 48, name: "faccessat", target: Target::Import("access"), args: &[1, 2], dirfd_path: Some((0, 1)) },
    // bionic's `fcntl` on LP64 is `__NR_fcntl`; the third argument is one register either way.
    Route { number: 25, name: "fcntl", target: Target::Import("fcntl"), args: &[0, 1, 2], dirfd_path: None },
    // bionic's `ioctl` is `__NR_ioctl`; the handler refuses the requests it does not model by name.
    Route { number: 29, name: "ioctl", target: Target::Import("ioctl"), args: &[0, 1, 2], dirfd_path: None },
    // `__NR_nanosleep` exactly: two `struct timespec *`.
    Route { number: 101, name: "nanosleep", target: Target::Import("nanosleep"), args: &[0, 1], dirfd_path: None },
    // bionic's `clock_gettime` is the vDSO or `__NR_clock_gettime`, one answer either way.
    Route { number: 113, name: "clock_gettime", target: Target::Import("clock_gettime"), args: &[0, 1], dirfd_path: None },
    // `__NR_sched_yield` exactly.
    Route { number: 124, name: "sched_yield", target: Target::Import("sched_yield"), args: &[], dirfd_path: None },
    // `__NR_getpid` exactly.
    Route { number: 172, name: "getpid", target: Target::Import("getpid"), args: &[], dirfd_path: None },
    // The guest-memory four are **exit-path** (re-entrant) handlers, and both doors reach them
    // there: a raw `SVC` is already serviced after the run loop has exited, and a routed
    // `syscall()` import call defers to the exit path before its handler runs.
    // `munmap(addr, len)` exactly.
    Route { number: 215, name: "munmap", target: Target::Import("munmap"), args: &[0, 1], dirfd_path: None },
    // arm64 has no `mmap2`: `__NR_mmap` takes a byte offset, as `mmap` does.
    Route { number: 222, name: "mmap", target: Target::Import("mmap"), args: &[0, 1, 2, 3, 4, 5], dirfd_path: None },
    // `mprotect(addr, len, prot)` exactly.
    Route { number: 226, name: "mprotect", target: Target::Import("mprotect"), args: &[0, 1, 2], dirfd_path: None },
    // `madvise(addr, len, advice)` exactly.
    Route { number: 233, name: "madvise", target: Target::Import("madvise"), args: &[0, 1, 2], dirfd_path: None },
];

/// The route for a syscall number, if it has one.
#[must_use]
pub(crate) fn route(number: u64) -> Option<&'static Route> {
    ROUTES.iter().find(|route| route.number == number)
}

/// A route decided against one call's arguments.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Resolved {
    /// The import that answers it.
    pub(crate) symbol: &'static str,
    /// The kernel argument each import argument is.
    pub(crate) args: &'static [u8],
}

/// Decide which import answers `route` for these kernel arguments, or why none can.
///
/// `Ok(Err(why))` is a refusal the caller names with its own door; `Err` is a guest-memory fault
/// reading the path's first byte.
pub(crate) fn resolve(
    route: &'static Route,
    kernel: &[u64; 6],
    mem: &GuestMem,
    at: omni_mem::GuestAddr,
) -> AbiResult<Result<Resolved, String>> {
    if let Some((dirfd, path)) = route.dirfd_path {
        let dirfd = kernel[usize::from(dirfd)] as u32 as i32;
        let path = kernel[usize::from(path)] as omni_mem::GuestAddr;
        let blame = Blame::new(route.name, at, 1);
        let absolute = mem.read_bytes(path, 1, blame)?.first() == Some(&b'/');
        if dirfd != AT_FDCWD && !absolute {
            return Ok(Err(format!(
                "{} relative to descriptor {dirfd}: this layer's imports resolve against the \
                 working directory only, and answering for another directory would describe the \
                 wrong file",
                route.name
            )));
        }
    }
    let symbol = match route.target {
        Target::Import(symbol) => symbol,
        Target::StatAt => {
            let flags = kernel[3];
            match flags & !AT_NO_AUTOMOUNT {
                0 => "stat",
                AT_SYMLINK_NOFOLLOW => "lstat",
                other => {
                    return Ok(Err(format!(
                        "newfstatat with flags {flags:#x}: {other:#x} is beyond \
                         AT_SYMLINK_NOFOLLOW and AT_NO_AUTOMOUNT, and `stat`/`lstat` answer \
                         neither AT_EMPTY_PATH (describe the descriptor itself) nor anything else"
                    )));
                }
            }
        }
    };
    Ok(Ok(Resolved { symbol, args: route.args }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every number appears once, and every argument list names kernel arguments 0-5 only.
    #[test]
    fn each_number_is_routed_once_to_kernel_arguments_that_exist() {
        for (index, route) in ROUTES.iter().enumerate() {
            assert!(
                ROUTES[index + 1..].iter().all(|other| other.number != route.number),
                "{} is routed twice",
                route.number
            );
            assert!(route.args.iter().all(|&arg| arg < 6), "{}: a seventh argument", route.name);
        }
        assert_eq!(route(63).map(|r| r.name), Some("read"));
        assert!(route(98).is_none(), "futex stays with procenv's own emulation");
        assert!(route(94).is_none(), "exit_group is not routed");
    }
}
