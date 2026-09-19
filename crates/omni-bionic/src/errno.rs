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
    /// Interrupted system call.
    pub const EINTR: i32 = 4;
    /// Bad file descriptor.
    pub const EBADF: i32 = 9;
    /// Cannot allocate memory.
    pub const ENOMEM: i32 = 12;
    /// Permission denied.
    pub const EACCES: i32 = 13;
    /// Invalid argument.
    pub const EINVAL: i32 = 22;
    /// Numerical argument out of domain.
    pub const EDOM: i32 = 33;
    /// Result too large (range error).
    pub const ERANGE: i32 = 34;
    /// Function not implemented.
    pub const ENOSYS: i32 = 38;
}
