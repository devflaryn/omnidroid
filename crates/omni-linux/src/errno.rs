//! Linux errno values (asm-generic, which arm64 uses).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

pub type SysResult = Result<u64, Errno>;

macro_rules! errnos {
    ($($name:ident = $v:expr),* $(,)?) => { $(pub const $name: Errno = Errno($v);)* };
}
errnos! {
    EPERM = 1, ENOENT = 2, ESRCH = 3, EINTR = 4, EIO = 5, E2BIG = 7, EBADF = 9, EAGAIN = 11, ENOMEM = 12,
    EACCES = 13, EFAULT = 14, EBUSY = 16, EEXIST = 17, ENOTDIR = 20, EISDIR = 21, EINVAL = 22,
    ENODEV = 19, EMFILE = 24, ENOTTY = 25, ESPIPE = 29, EROFS = 30, EPIPE = 32, ERANGE = 34, ENAMETOOLONG = 36,
    ENOSPC = 28, ENOSYS = 38, ENOTEMPTY = 39, ELOOP = 40, ENOTSOCK = 88, EOPNOTSUPP = 95, EAFNOSUPPORT = 97,
    EADDRINUSE = 98, ENETUNREACH = 101, ENOTCONN = 107, ETIMEDOUT = 110, ECONNREFUSED = 111,
}

impl Errno {
    /// The syscall return value: `-errno` in two's complement.
    #[must_use]
    pub const fn as_return(self) -> u64 {
        (-(self.0 as i64)) as u64
    }
}
