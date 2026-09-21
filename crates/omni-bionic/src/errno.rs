//! Linux errno numbers, as they must appear in the **guest's** errno.
//!
//! The development host is Windows (different errno numbering), so every value is defined
//! here; the host's numbers are never used. The values below are the Linux kernel UAPI
//! numbering (`include/uapi/asm-generic/errno-base.h` and `errno.h`), which is what bionic
//! exposes on arm64 — VERIFIED against the kernel UAPI source for every constant defined
//! here; only constants reachable by this crate's functions are listed.
//!
//! Note on the kernel↔glibc split (bionic follows the kernel side): `EAGAIN`, `EDEADLK`,
//! `ENOSYS`, `ENOTEMPTY`, `ELOOP`, `EOVERFLOW`, `ECANCELED` and friends are *not* renumbered
//! the way glibc renumbers them on Linux; bionic keeps kernel numbering for the common codes.

/// `errno` value constants (Linux numbering; guest `errno` is `int`, i32).
pub mod consts {
    /// Operation not permitted.
    pub const EPERM: i32 = 1;
    /// No such file or directory.
    pub const ENOENT: i32 = 2;
    /// No such process: a `pthread_t` no live thread answers to.
    ///
    /// POSIX's own answer for `pthread_join`, `pthread_detach` and `pthread_getschedparam`
    /// given an id that names no thread, and what bionic's `__pthread_internal_find` produces
    /// when its table has no entry.
    pub const ESRCH: i32 = 3;
    /// Interrupted system call.
    pub const EINTR: i32 = 4;
    /// Input/output error.
    pub const EIO: i32 = 5;
    /// Bad file descriptor.
    pub const EBADF: i32 = 9;
    /// Cannot allocate memory.
    pub const ENOMEM: i32 = 12;
    /// Permission denied.
    pub const EACCES: i32 = 13;
    /// Resource temporarily unavailable (EWOULDBLOCK); sem_trywait on zero, futex
    /// on a non-matching value.
    pub const EAGAIN: i32 = 11;
    /// Device or resource busy: pthread_mutex_trylock on a held mutex.
    pub const EBUSY: i32 = 16;
    /// File exists: `open(O_CREAT|O_EXCL)` on a path that is already there, `mkdir` twice.
    pub const EEXIST: i32 = 17;
    /// Not a directory: a path component that had to be one is not.
    pub const ENOTDIR: i32 = 20;
    /// Is a directory: `unlink` or `read` on one.
    pub const EISDIR: i32 = 21;
    /// Too many open files in this process: the descriptor ceiling.
    pub const EMFILE: i32 = 24;
    /// File too large.
    pub const EFBIG: i32 = 27;
    /// No space left on device.
    pub const ENOSPC: i32 = 28;
    /// Illegal seek: an offset operation on a pipe or a standard stream.
    pub const ESPIPE: i32 = 29;
    /// Broken pipe: a write to a pipe whose every read end is closed.
    ///
    /// On a device this arrives with `SIGPIPE`, whose default disposition terminates the process,
    /// so a caller normally sees the signal rather than the errno. This runtime delivers no
    /// signals (D24), so the errno is the whole of what the guest gets — which is what a caller
    /// that has set `SIG_IGN` sees on a device, and is the form every correct caller branches on.
    pub const EPIPE: i32 = 32;
    /// Read-only filesystem.
    pub const EROFS: i32 = 30;
    /// Invalid argument.
    pub const EINVAL: i32 = 22;
    /// Numerical argument out of domain.
    pub const EDOM: i32 = 33;
    /// Result too large (range error).
    pub const ERANGE: i32 = 34;
    /// Resource deadlock avoided: ERRORCHECK relock by the owner.
    pub const EDEADLK: i32 = 35;
    /// File name too long: past `PATH_MAX` or `NAME_MAX`.
    pub const ENAMETOOLONG: i32 = 36;
    /// Function not implemented.
    pub const ENOSYS: i32 = 38;
    /// Directory not empty: `rmdir` on a directory that still has entries.
    pub const ENOTEMPTY: i32 = 39;
    /// Too many levels of symbolic links.
    pub const ELOOP: i32 = 40;
    /// Value too large for the defined data type.
    pub const EOVERFLOW: i32 = 75;
    /// Address family not supported by protocol: `inet_ntop` given an `af` that is neither
    /// `AF_INET` nor `AF_INET6`.
    pub const EAFNOSUPPORT: i32 = 97;
    /// Operation not supported (Linux: 95 on most architectures; arm64 uses the
    /// asm-generic numbering where ENOTSUP == EOPNOTSUPP == 95).
    pub const ENOTSUP: i32 = 95;
    /// Operation timed out: POSIX timed-lock/cond waits. LINUX VALUE (110);
    /// Windows' WSAETIMEDOUT is 10060 and its ERROR_SEM_TIMEOUT is 121 — both
    /// different, which is exactly why the constant is defined here.
    pub const ETIMEDOUT: i32 = 110;
    /// Owner died (robust mutexes; defined for completeness of the mutex error set).
    pub const EOWNERDEAD: i32 = 130;
    /// State not recoverable (robust mutexes).
    pub const ENOTRECOVERABLE: i32 = 131;
}

#[cfg(test)]
mod tests {
    use super::consts::*;

    /// Every errno constant, pinned to its Linux value.
    ///
    /// **This module had no test at all**, which a mutation run found: renumbering `ETIMEDOUT` to
    /// Windows' `ERROR_SEM_TIMEOUT` (121) and `EAGAIN` off the kernel's value both left the whole
    /// suite passing. The values were right; nothing said so.
    ///
    /// That is the worst shape for this particular module. These numbers reach the guest, the
    /// development host is Windows — whose numbering is different, and whose `WSAETIMEDOUT` is
    /// 10060 and `ERROR_SEM_TIMEOUT` 121 — and a wrong one produces a guest that takes the wrong
    /// branch rather than a build that fails. A test that asserts the numbers one by one is dull,
    /// and it is the only thing that can detect this.
    ///
    /// Values are the Linux kernel UAPI numbering (`asm-generic/errno-base.h` for 1-34,
    /// `asm-generic/errno.h` above it), which is what bionic exposes on arm64. Written as literals
    /// on purpose: comparing a constant to itself would pass against any value.
    #[test]
    fn every_errno_is_the_linux_number() {
        // errno-base.h
        assert_eq!(EPERM, 1);
        assert_eq!(ENOENT, 2);
        assert_eq!(ESRCH, 3, "the thread-lifecycle group's own, added in phase 3c");
        assert_eq!(EINTR, 4);
        assert_eq!(EIO, 5);
        assert_eq!(EBADF, 9);
        assert_eq!(EAGAIN, 11, "EAGAIN == EWOULDBLOCK == 11 on Linux");
        assert_eq!(ENOMEM, 12);
        assert_eq!(EACCES, 13);
        assert_eq!(EBUSY, 16);
        assert_eq!(EEXIST, 17);
        assert_eq!(ENOTDIR, 20);
        assert_eq!(EISDIR, 21);
        assert_eq!(EMFILE, 24);
        assert_eq!(EFBIG, 27);
        assert_eq!(ENOSPC, 28);
        assert_eq!(ESPIPE, 29);
        assert_eq!(EPIPE, 32, "the pipe group's own, added in M5");
        assert_eq!(EROFS, 30);
        assert_eq!(EINVAL, 22);
        assert_eq!(EDOM, 33);
        assert_eq!(ERANGE, 34);
        // errno.h
        assert_eq!(EDEADLK, 35);
        assert_eq!(ENAMETOOLONG, 36);
        assert_eq!(ENOSYS, 38);
        assert_eq!(ENOTEMPTY, 39);
        assert_eq!(ELOOP, 40);
        assert_eq!(EOVERFLOW, 75, "the file-io group's own, added in phase 3b");
        assert_eq!(ENOTSUP, 95, "ENOTSUP == EOPNOTSUPP == 95 in the asm-generic numbering");
        assert_eq!(EAFNOSUPPORT, 97, "the network group's own, added in phase 3d");
        assert_eq!(ETIMEDOUT, 110, "the LINUX value; Windows' ERROR_SEM_TIMEOUT is 121");
        assert_eq!(EOWNERDEAD, 130);
        assert_eq!(ENOTRECOVERABLE, 131);
    }

    /// The codes this crate returns must be distinguishable from each other.
    ///
    /// A renumbering that collided two of them would let a caller take the wrong branch while every
    /// individual assertion above still held for the others.
    #[test]
    fn the_errno_values_are_all_distinct() {
        let all = [
            EPERM, ENOENT, ESRCH, EINTR, EIO, EBADF, EAGAIN, ENOMEM, EACCES, EBUSY, EEXIST, ENOTDIR,
            EISDIR, EMFILE, EFBIG, ENOSPC, ESPIPE, EPIPE, EROFS, EINVAL, EDOM, ERANGE, EDEADLK,
            ENAMETOOLONG, ENOSYS, ENOTEMPTY, ELOOP, EOVERFLOW, ENOTSUP, EAFNOSUPPORT, ETIMEDOUT,
            EOWNERDEAD, ENOTRECOVERABLE,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "two errno constants collide");
    }
}
