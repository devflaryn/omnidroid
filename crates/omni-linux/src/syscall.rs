//! The arm64 syscall table: numbers, names, handlers, and the record of what was refused.
use std::borrow::Cow;
use std::collections::BTreeMap;

use parking_lot::Mutex;

use crate::errno::SysResult;
use crate::process::{Process, Task};

/// arm64 (asm-generic) syscall numbers this layer names.
pub mod nr {
    macro_rules! numbers {
        ($($name:ident = $v:expr => $s:expr),* $(,)?) => {
            $(pub const $name: u64 = $v;)*
            pub(crate) const NAMES: &[(u64, &str)] = &[$(($v, $s)),*];
        };
    }
    numbers! {
        GETCWD = 17 => "getcwd", DUP = 23 => "dup", DUP3 = 24 => "dup3", FCNTL = 25 => "fcntl",
        IOCTL = 29 => "ioctl", MKDIRAT = 34 => "mkdirat", UNLINKAT = 35 => "unlinkat",
        SYMLINKAT = 36 => "symlinkat", STATFS = 43 => "statfs", FSTATFS = 44 => "fstatfs",
        FACCESSAT = 48 => "faccessat", FCHMOD = 52 => "fchmod", FCHMODAT = 53 => "fchmodat",
        FCHOWNAT = 54 => "fchownat", FCHOWN = 55 => "fchown", CHDIR = 49 => "chdir", FCHDIR = 50 => "fchdir",
        FTRUNCATE = 46 => "ftruncate", OPENAT = 56 => "openat", CLOSE = 57 => "close", PIPE2 = 59 => "pipe2",
        GETDENTS64 = 61 => "getdents64", LSEEK = 62 => "lseek", READ = 63 => "read",
        WRITE = 64 => "write", READV = 65 => "readv", WRITEV = 66 => "writev",
        PREAD64 = 67 => "pread64", PWRITE64 = 68 => "pwrite64", PREADV = 69 => "preadv", PWRITEV = 70 => "pwritev", SENDFILE = 71 => "sendfile", PPOLL = 73 => "ppoll", INOTIFY_INIT1 = 26 => "inotify_init1", INOTIFY_ADD_WATCH = 27 => "inotify_add_watch", INOTIFY_RM_WATCH = 28 => "inotify_rm_watch", EVENTFD2 = 19 => "eventfd2", EPOLL_CREATE1 = 20 => "epoll_create1", EPOLL_CTL = 21 => "epoll_ctl", EPOLL_PWAIT = 22 => "epoll_pwait", TIMERFD_CREATE = 85 => "timerfd_create", TIMERFD_SETTIME = 86 => "timerfd_settime", TIMERFD_GETTIME = 87 => "timerfd_gettime",
        READLINKAT = 78 => "readlinkat", FSYNC = 82 => "fsync", FDATASYNC = 83 => "fdatasync",
        UTIMENSAT = 88 => "utimensat", RENAMEAT = 38 => "renameat", RENAMEAT2 = 276 => "renameat2", NEWFSTATAT = 79 => "newfstatat", FSTAT = 80 => "fstat",
        EXIT = 93 => "exit", EXIT_GROUP = 94 => "exit_group", SET_TID_ADDRESS = 96 => "set_tid_address",
        FUTEX = 98 => "futex", SET_ROBUST_LIST = 99 => "set_robust_list", NANOSLEEP = 101 => "nanosleep",
        CLOCK_GETTIME = 113 => "clock_gettime", CLOCK_GETRES = 114 => "clock_getres",
        CLOCK_NANOSLEEP = 115 => "clock_nanosleep", SCHED_SETPARAM = 118 => "sched_setparam", SCHED_SETSCHEDULER = 119 => "sched_setscheduler",
        SCHED_GETSCHEDULER = 120 => "sched_getscheduler", SCHED_GETPARAM = 121 => "sched_getparam",
        SCHED_GETAFFINITY = 123 => "sched_getaffinity",
        SCHED_YIELD = 124 => "sched_yield", SCHED_GET_PRIORITY_MAX = 125 => "sched_get_priority_max",
        SCHED_GET_PRIORITY_MIN = 126 => "sched_get_priority_min", KILL = 129 => "kill", TKILL = 130 => "tkill",
        TGKILL = 131 => "tgkill", SIGALTSTACK = 132 => "sigaltstack",
        RT_SIGACTION = 134 => "rt_sigaction", RT_SIGPROCMASK = 135 => "rt_sigprocmask",
        RT_SIGPENDING = 136 => "rt_sigpending", RT_SIGSUSPEND = 133 => "rt_sigsuspend", RT_SIGTIMEDWAIT = 137 => "rt_sigtimedwait", RT_SIGRETURN = 139 => "rt_sigreturn", RT_TGSIGQUEUEINFO = 240 => "rt_tgsigqueueinfo", UNAME = 160 => "uname", GETRLIMIT = 163 => "getrlimit",
        GETRUSAGE = 165 => "getrusage", UMASK = 166 => "umask", PRCTL = 167 => "prctl", GETTIMEOFDAY = 169 => "gettimeofday",
        GETPID = 172 => "getpid", GETPPID = 173 => "getppid", GETUID = 174 => "getuid",
        GETEUID = 175 => "geteuid", GETGID = 176 => "getgid", GETEGID = 177 => "getegid",
        GETTID = 178 => "gettid", SYSINFO = 179 => "sysinfo", SOCKET = 198 => "socket",
        SOCKETPAIR = 199 => "socketpair", BIND = 200 => "bind", LISTEN = 201 => "listen", ACCEPT = 202 => "accept",
        CONNECT = 203 => "connect", GETSOCKNAME = 204 => "getsockname", GETPEERNAME = 205 => "getpeername",
        SENDTO = 206 => "sendto", RECVFROM = 207 => "recvfrom", SETSOCKOPT = 208 => "setsockopt",
        GETSOCKOPT = 209 => "getsockopt", SHUTDOWN = 210 => "shutdown", SENDMSG = 211 => "sendmsg", RECVMSG = 212 => "recvmsg",
        ACCEPT4 = 242 => "accept4", SETPRIORITY = 140 => "setpriority", GETPRIORITY = 141 => "getpriority",
        PROCESS_VM_READV = 270 => "process_vm_readv", USERFAULTFD = 282 => "userfaultfd",
        BRK = 214 => "brk", MUNMAP = 215 => "munmap", MREMAP = 216 => "mremap",
        CLONE = 220 => "clone", EXECVE = 221 => "execve", MMAP = 222 => "mmap",
        MPROTECT = 226 => "mprotect", MSYNC = 227 => "msync", MLOCK = 228 => "mlock", MUNLOCK = 229 => "munlock", MLOCKALL = 230 => "mlockall", MUNLOCKALL = 231 => "munlockall", MLOCK2 = 284 => "mlock2", MADVISE = 233 => "madvise", PRLIMIT64 = 261 => "prlimit64",
        SECCOMP = 277 => "seccomp", GETRANDOM = 278 => "getrandom", MEMFD_CREATE = 279 => "memfd_create",
        MEMBARRIER = 283 => "membarrier", STATX = 291 => "statx", RSEQ = 293 => "rseq",
        CLONE3 = 435 => "clone3", FACCESSAT2 = 439 => "faccessat2",
        MOUNT = 40 => "mount", UMOUNT2 = 39 => "umount2", WAIT4 = 260 => "wait4", WAITID = 95 => "waitid", BPF = 280 => "bpf", FLOCK = 32 => "flock", CAPGET = 90 => "capget", CAPSET = 91 => "capset",
        SETREGID = 143 => "setregid", SETGID = 144 => "setgid", SETREUID = 145 => "setreuid", SETUID = 146 => "setuid",
        SETRESUID = 147 => "setresuid", GETRESUID = 148 => "getresuid", SETRESGID = 149 => "setresgid", GETRESGID = 150 => "getresgid",
        SETFSUID = 151 => "setfsuid", SETFSGID = 152 => "setfsgid", GETGROUPS = 158 => "getgroups", SETGROUPS = 159 => "setgroups",
        SETXATTR = 5 => "setxattr", LSETXATTR = 6 => "lsetxattr", FSETXATTR = 7 => "fsetxattr",
        GETXATTR = 8 => "getxattr", LGETXATTR = 9 => "lgetxattr", FGETXATTR = 10 => "fgetxattr",
        LISTXATTR = 11 => "listxattr", LLISTXATTR = 12 => "llistxattr", FLISTXATTR = 13 => "flistxattr",
        REMOVEXATTR = 14 => "removexattr", LREMOVEXATTR = 15 => "lremovexattr", FREMOVEXATTR = 16 => "fremovexattr",
    }
}

#[must_use]
pub fn name_of(number: u64) -> Cow<'static, str> {
    nr::NAMES
        .iter()
        .find(|(n, _)| *n == number)
        .map_or_else(|| Cow::Owned(format!("syscall_{number}")), |(_, s)| Cow::Borrowed(*s))
}

pub type Handler = fn(&Process, &mut Task, [u64; 6]) -> SysResult;

/// Dense by number: a lookup is an index, not a search.
pub struct Table {
    entries: Vec<Option<Handler>>,
}

const TABLE_LEN: usize = 512;

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Table {
    #[must_use]
    pub fn new() -> Self {
        Self { entries: vec![None; TABLE_LEN] }
    }

    pub fn set(&mut self, number: u64, handler: Handler) {
        self.entries[number as usize] = Some(handler);
    }

    #[must_use]
    pub fn get(&self, number: u64) -> Option<Handler> {
        self.entries.get(usize::try_from(number).ok()?).copied().flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub what: String,
    pub count: u64,
    pub first_pc: u64,
    pub first_lr: u64,
}

/// Everything refused by name, once each, with its first caller: the kernel-side `Jni::misses`.
#[derive(Default)]
pub struct Refusals {
    seen: Mutex<BTreeMap<String, Refusal>>,
}

impl Refusals {
    pub fn record(&self, what: String, pc: u64, lr: u64) {
        let mut seen = self.seen.lock();
        let entry = seen.entry(what.clone()).or_insert_with(|| {
            tracing::warn!(what = %what, pc = format_args!("{pc:#x}"), lr = format_args!("{lr:#x}"), "refused");
            Refusal { what, count: 0, first_pc: pc, first_lr: lr }
        });
        entry.count += 1;
    }

    #[must_use]
    pub fn list(&self) -> Vec<Refusal> {
        self.seen.lock().values().cloned().collect()
    }

    #[must_use]
    pub fn report(&self) -> String {
        self.list()
            .iter()
            .map(|r| format!("  {} x{} (first pc {:#x}, lr {:#x})\n", r.what, r.count, r.first_pc, r.first_lr))
            .collect()
    }
}
