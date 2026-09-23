//! Files and directories: the platform seam for descriptors, metadata and directory listings.
//!
//! Serves the guest's `open`, `close`, `read`, `pread`, `write`, `stat`, `fstat`, `lstat`,
//! `statvfs`, `access`, `rename`, `unlink`, `mkdir`, `rmdir`, `opendir`, `readdir` and
//! `closedir`. Bionic's `FILE *` layer — `fopen`, `fgets`, `fread`, `fwrite`, … — is **not** here:
//! it is stream logic over these primitives, it needs no OS of its own, and it lives in
//! `omni-bionic` on top of a trait that crate defines (D19: `cargo tree -p omni-bionic -e normal`
//! must stay one line).
//!
//! # Confinement is the first thing, not a feature
//!
//! A [`Filesystem`] **is** a root plus a descriptor table, and there is no constructor that does
//! not take a root. Every guest path is resolved inside it by [`path`]'s rules before any host
//! call is made, and a path that cannot be is refused by name. See that module: it is where the
//! policy is written down and where the hostile cases are enumerated.
//!
//! A host that has not supplied a root has no `Filesystem`, and the adapter above then refuses
//! every path-taking guest call naming the method that would supply one. "The guest cannot reach
//! an arbitrary host file" is therefore a property of the types rather than a check somebody has
//! to remember to make.
//!
//! # Two kinds of primitive, and only two of them need a backend
//!
//! The pattern D22 established for [`clock`](crate::clock) and [`process`](crate::process), with
//! the balance the other way round from `process` — here most operations are portable and two are
//! not:
//!
//! | primitive | how | Linux / macOS |
//! |---|---|---|
//! | [`Filesystem::open`], [`close`](Filesystem::close), [`read`](Filesystem::read), [`write`](Filesystem::write), [`flush`](Filesystem::flush) | `std::fs::File`, `Read`, `Write` | **implemented** — portable `std`, no backend |
//! | [`stat`](Filesystem::stat), [`lstat`](Filesystem::lstat), [`fstat`](Filesystem::fstat) | `std::fs::metadata`, `symlink_metadata`, `File::metadata` | **implemented** — portable `std` |
//! | [`rename`](Filesystem::rename), [`unlink`](Filesystem::unlink), [`mkdir`](Filesystem::mkdir), [`rmdir`](Filesystem::rmdir) | `std::fs`'s four of the same name | **implemented** — portable `std` |
//! | [`opendir`](Filesystem::opendir), [`readdir`](Filesystem::readdir), [`closedir`](Filesystem::closedir) | `std::fs::read_dir` | **implemented** — portable `std` |
//! | [`access`](Filesystem::access) | metadata plus an open probe | **implemented** — portable `std` |
//! | [`pread`](Filesystem::pread) | **backend**: `FileExt::seek_read` on Windows | **`Unsupported`**, naming `pread(2)` |
//! | [`statvfs`](Filesystem::statvfs) | **backend**: `GetDiskFreeSpaceExW` + `GetDiskFreeSpaceW` + `GetVolumeInformationW` | **`Unsupported`**, naming `statvfs(3)` |
//!
//! **The distinction is the one D22 wrote down, applied in the direction it points.** A primitive
//! that is one portable `std` call on all five targets is implemented once and does *not* get a
//! fabricated `Unsupported` arm, because that would be a false claim in the other direction — it
//! would assert that a file this process can open cannot be opened. A primitive that needs a
//! *different* call per target is not in that class: `pread` is `FileExt::seek_read` on Windows
//! and `FileExt::read_at` on unix, two different traits from two different modules, and `statvfs`
//! has no `std` spelling at all. Those two get the Windows implementation and a structural unix
//! half naming the POSIX call, exactly as `process::random_bytes` does.
//!
//! **Nothing here has been run on Linux or macOS.** The portable half is expected to work there
//! and has not been built, let alone tested; that is the standing position for all five targets
//! and it is not weakened by an implementation existing.
//!
//! # What is deliberately not on this seam
//!
//! * **No `symlink`, `link`, `readlink`, `chmod` or `chown`.** None is in the 188
//!   statically-reachable imports, and the first two are what make [`path`]'s symlink rule
//!   sufficient: the guest cannot create a link, so the set of links inside the root is fixed by
//!   whoever populated it.
//! * **No `chdir`/`getcwd`.** Also not reachable, which is why a relative guest path resolves
//!   against the root rather than against a working directory nothing can move.
//! * **No `fcntl`, `ioctl`, `dup` or `lseek`.** Not reachable either. `lseek`'s absence is why
//!   [`Filesystem::pread`] exists as its own primitive rather than being built from a seek and a
//!   read: a seek-and-read pair is not `pread`, because it moves the descriptor's own offset —
//!   which was MEASURED here and is written up in the Windows backend.
//!
//! # Sockets are descriptors here, and their *operations* are not
//!
//! D30 point 2: `poll`, `select`, `close` and `fcntl` observe **one** descriptor space, so a
//! socket is an [`Entry`] in this table and gets its number from [`Filesystem::attach_socket`].
//! What it does **not** get is a transfer path through [`Filesystem::read`] and
//! [`Filesystem::write`] — both refuse a socket by name and say where to go instead, because a
//! socket's failures (`ECONNRESET`, `ETIMEDOUT`, `ENOTCONN`) have no spelling in [`FsErrorKind`]
//! and a blocking socket cannot be waited out on a gate nothing in this process raises for it.
//! [`Filesystem::socket_at`] hands the socket back and [`net`](crate::net) does the rest.
//!
//! # Generated files are paths that name a fact, not a file
//!
//! [`Filesystem::serve_generated`] lets the layer above answer a path with bytes it produces --
//! `/proc/meminfo` and `/proc/self/statm` are why -- rather than with a file under the root. Like
//! the [`DEVICES`], such a path names no host file, so it is not a hole in the confinement; unlike
//! them, what it says is the caller's decision, and this seam only makes it behave as a read-only
//! file with Linux's `seq_file` rules (see `GeneratedFile`).

pub mod epoll;
mod error;
pub mod eventfd;
pub mod path;
pub mod pipe;
pub mod timerfd;

pub use error::{FsError, FsErrorKind, FsResult};
pub use epoll::{EpollMember, EpollOp, EPOLL_CLOEXEC};
pub use eventfd::{EventFd, EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE};
pub use path::{FinalLink, Resolved, NAME_MAX, PATH_MAX};
pub use pipe::{PipeEnd, Readiness, ReadyGate, PIPE_BUF, PIPE_CAPACITY};
pub use timerfd::{TimerFd, TFD_CLOEXEC, TFD_NONBLOCK, TFD_TIMER_ABSTIME};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The lowest descriptor number [`Filesystem::open`] hands out.
///
/// 0, 1 and 2 are the standard streams, which POSIX reserves and which every guest assumes are
/// already open. They are entries in the table from construction, so `write(1, …)` works before
/// anything has been opened and `close(1)` is a legal thing for a guest to do.
pub const FIRST_FD: i32 = 3;

/// `STDIN_FILENO`.
pub const STDIN_FD: i32 = 0;
/// `STDOUT_FILENO`.
pub const STDOUT_FD: i32 = 1;
/// `STDERR_FILENO`.
pub const STDERR_FD: i32 = 2;

/// How many descriptors one guest instance may hold open at once -- its `RLIMIT_NOFILE`.
///
/// **A policy number, and stated as one.** A guest that leaks descriptors in a loop would
/// otherwise hold as many host handles as it liked, and several instances share one host process.
/// Past this, [`Filesystem::open`] reports [`FsErrorKind::TooManyOpenFiles`], which becomes the
/// guest's `EMFILE` — the answer a real device gives when it hits `RLIMIT_NOFILE`, and one every
/// correct caller already has a branch for.
///
/// **1024, Linux's usual soft limit** -- the figure `bionic::net`'s `poll` bound already names as
/// that limit, and no more than the `FD_SETSIZE` a `select` caller can address. It was 64, and
/// MEASURED what that cost: on the first game join the engine had its caches, its HTTP
/// connections and its QUIC sockets open at once, its next `socket` answered `EMFILE`, and the
/// join's request to `gamejoin.roblox.com` failed as Roblox's "Http error" 529.
pub const MAX_OPEN_FILES: usize = 1024;

/// How many distinct missing paths [`Filesystem::open_misses`] keeps.
///
/// A bound rather than a growing list, because a guest that retries a missing file in a loop would
/// otherwise turn a diagnostic into a leak. 256 is far more than any run has produced and small
/// enough to print.
pub const MAX_RECORDED_MISSES: usize = 256;

/// How many directory streams one guest instance may hold open at once.
pub const MAX_OPEN_DIRS: usize = 16;

/// How many entries one directory listing may hold.
///
/// [`Filesystem::opendir`] snapshots the directory, so this is a bound on a host allocation the
/// guest triggers. Past it the call is **refused by name** rather than truncated: a short listing
/// is a believable wrong answer, and code that walks a directory to find a file would report the
/// file missing.
pub const MAX_DIR_ENTRIES: usize = 65_536;

/// The block size this layer transfers in, and therefore the one `st_blksize` reports.
///
/// **A fact rather than a plausible number.** Every chunked transfer in this seam and in the
/// `FILE *` layer above it moves at most this much at a time, so a guest that sizes its buffers
/// from `st_blksize` is sizing them to what actually happens. 4096 is also what Linux reports for
/// most filesystems, so it is not a surprising value to receive.
pub const IO_BLOCK: usize = 4096;

/// What a directory entry or a stat call found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    /// An ordinary file.
    Regular,
    /// A directory.
    Directory,
    /// A symbolic link — only ever reported by [`Filesystem::lstat`], which is the one call that
    /// describes a link rather than following it.
    Symlink,
    /// Something the host has and POSIX would call a device, socket or fifo.
    Other,
}

impl FileKind {
    fn of(file_type: &std::fs::FileType) -> FileKind {
        if file_type.is_file() {
            FileKind::Regular
        } else if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_symlink() {
            FileKind::Symlink
        } else {
            FileKind::Other
        }
    }
}

/// How a descriptor is to be opened.
///
/// Built by the caller from the guest's `O_*` bits, which are Linux UAPI numbers and are the
/// *guest's* ABI rather than this seam's — so the parsing, and the refusal of a flag whose
/// guarantee cannot be met, live in the adapter above. This struct is what survives that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenFlags {
    /// The descriptor may be read.
    pub read: bool,
    /// The descriptor may be written.
    pub write: bool,
    /// Create the file if it does not exist.
    pub create: bool,
    /// With [`create`](Self::create), fail if it already exists.
    pub exclusive: bool,
    /// Truncate an existing file to zero length.
    pub truncate: bool,
    /// Every write goes to the end of the file.
    pub append: bool,
    /// The path must name a directory, and the descriptor is one that cannot be read or written.
    pub directory: bool,
}

/// What [`Filesystem::access`] was asked to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessCheck {
    /// `F_OK`: does it exist.
    Exists,
    /// `R_OK`: could this process read it.
    Readable,
    /// `W_OK`: could this process write it.
    Writable,
}

/// The host facts one `stat` found.
///
/// **Host facts only.** How they become the guest's `struct stat` — the `S_IF*` bits, the 128-byte
/// arm64 layout, the `timespec` pairs — is guest ABI and belongs to the adapter. Nothing here
/// knows what a mode bit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// What the path names.
    pub kind: FileKind,
    /// Its length in bytes. Zero for a directory, which is what the host reports.
    pub size: u64,
    /// Whether the host's read-only attribute is set.
    ///
    /// This is the only permission `std` exposes on all five targets, and it is the only one the
    /// adapter derives a mode bit from. It is *not* an ACL evaluation.
    pub read_only: bool,
    /// Last access time, if the host keeps one.
    pub accessed: Option<Duration>,
    /// Last modification time, if the host keeps one.
    pub modified: Option<Duration>,
    /// Creation time, if the host keeps one.
    pub created: Option<Duration>,
    /// A stable identifier for this path — see [`identity`].
    pub identity: u64,
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// The entry's name, with no directory part.
    pub name: String,
    /// What it is.
    pub kind: FileKind,
    /// The same identifier [`FileStat::identity`] carries.
    pub identity: u64,
}

/// What one volume reports about itself.
///
/// Every field is a number the host was **asked for**. Nothing here is a constant this layer
/// chose, which is the whole point: a `statvfs` that invented a filesystem would be believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeStats {
    /// Bytes per allocation unit.
    pub block_size: u64,
    /// Allocation units on the volume.
    pub blocks: u64,
    /// Allocation units free.
    pub blocks_free: u64,
    /// Allocation units available to this process, which quotas can make smaller than
    /// [`blocks_free`](Self::blocks_free).
    pub blocks_available: u64,
    /// The volume's maximum component length.
    pub name_max: u64,
    /// The volume's own identifier.
    pub filesystem_id: u64,
    /// Whether the volume is mounted read-only.
    pub read_only: bool,
}

/// A stable 64-bit identifier for a host path, for the guest's `st_ino` and `d_ino`.
///
/// **Derived from the path, and that choice is the interesting one.** Windows' real file identity
/// (the volume serial plus the file index) is reachable only from an open handle, and opening a
/// *directory* needs a flag `std::fs` does not expose — so it is not available for every path
/// this seam stats.
///
/// The two alternatives were:
///
/// * **Zero.** Rejected outright. Real code compares `(st_dev, st_ino)` pairs to ask "are these
///   two names the same file", and with a constant zero the answer is always *yes* — every file
///   in the guest's world would be the same file. That is the worst available wrong answer.
/// * **A hash of the path**, which is this. Two different paths get different numbers (a 64-bit
///   FNV-1a collision is 2^-64 per pair), so the identity test answers "different files" for
///   different names. Its one inaccuracy is in the safe direction: two names for **one** file — a
///   hard link — are reported as two files, so code that would have aliased them copies instead.
///   Nothing the guest can call creates a hard link, so that case needs the host operator to have
///   made one.
///
/// Never zero: zero is the value no real inode has, so returning it would be indistinguishable
/// from "this layer has no answer".
#[must_use]
pub fn identity(path: &Path) -> u64 {
    // FNV-1a, 64-bit. Chosen because it is four lines, has no dependency and no seed to agree
    // on, and because this value must be *stable across runs* — a `RandomState` hash would give
    // one file two different inode numbers in two runs of the same guest.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// What one open descriptor is.
#[derive(Debug)]
enum Entry {
    /// A real file, with the access it was opened for.
    File {
        file: File,
        readable: bool,
        writable: bool,
        host: PathBuf,
        guest: String,
    },
    /// A directory opened with `O_DIRECTORY`: it can be `fstat`ed and closed and nothing else,
    /// which is what a directory descriptor is for.
    Directory { host: PathBuf, guest: String },
    /// One of the three standard streams.
    Standard(StdStream),
    /// A **character device** this seam serves itself, not a file under the root.
    ///
    /// See [`Device`]: the guest's `/dev/urandom` is the entropy source
    /// [`crate::process::random_bytes`] already provides, and it is not a path that can be
    /// confined into a host directory because it is not a file.
    Device(Device),
    /// A file whose bytes a [`Generator`] produces rather than a file under the root.
    ///
    /// See [`Filesystem::serve_generated`]. Here for [`Device`](Entry::Device)'s reason: it names
    /// no host path, so it cannot be confined into the root and must not be looked for there.
    Generated(GeneratedFile),
    /// One end of a pipe — the first kind here whose readiness depends on another descriptor.
    ///
    /// See [`pipe`]. It is in this table rather than in a namespace of its own because `poll` and
    /// `select` observe one descriptor space, and two allocators can hand out the same number.
    Pipe(pipe::PipeHandle),
    /// An `eventfd`: a counter whose readiness is its own value.
    ///
    /// See [`eventfd`]. Here for [`Pipe`](Entry::Pipe)'s reason — `poll` observes one descriptor
    /// space — and, unlike a pipe, it is **one** descriptor rather than two, so the read side and
    /// the write side of the same counter are the same number.
    EventFd(eventfd::EventFd),
    /// An epoll instance: an interest list over *other* descriptors.
    ///
    /// See [`epoll`]. The one kind whose readiness is not its own -- it is its members' -- so it
    /// is the one kind [`Entry::readiness`] does not answer, and asking refuses by name.
    Epoll(epoll::EpollSet),
    /// A timer: the one kind whose readiness changes with **time**, which no writer announces.
    ///
    /// See [`timerfd`]. [`ReadinessSource::Timer`] is how a waiter learns it must cap its wait at
    /// the deadline.
    TimerFd(timerfd::TimerFd),
    /// A socket: the first kind here whose readiness is the **operating system's** answer rather
    /// than this process's own state.
    ///
    /// See [`net`](crate::net). D30 point 2 requires it to be in this table and not in one of its
    /// own, for the reason [`Pipe`](Entry::Pipe) already gives and one more: `close`, `fcntl` and
    /// `poll` are written against *this* table, so a second allocator would hand out a number one
    /// of them would answer about the wrong object. Instance isolation follows from that rather
    /// than from anything in `net` — per-[`Filesystem`] is per-guest-instance, exactly as every
    /// other descriptor already is.
    ///
    /// # Why an `Arc<Mutex<..>>` where every other kind is held directly
    ///
    /// Because a socket operation can **wait on the host**, and every other kind here cannot. A
    /// pipe read is an in-process queue operation that returns immediately; a `recv` on a blocking
    /// socket is a call into the kernel that returns when a packet arrives. Holding the table lock
    /// across one would stop every *other* descriptor in this instance for the duration, including
    /// the `poll` on another thread that is waiting to learn the socket became readable.
    ///
    /// So the handle is cloned out from under the table lock — [`Filesystem::socket_at`] — and the
    /// socket's own lock is taken with the table lock released. The lock order is one edge, table
    /// then socket, and nothing takes it the other way round: [`Entry::readiness`] locks the socket
    /// while holding the table, and no path locks the table while holding a socket.
    Socket(Arc<Mutex<crate::net::Socket>>),
}

/// The readiness a socket reports when the host's own readiness call **fails**.
///
/// [`Entry::readiness`] is infallible and [`crate::net::Socket::readiness`] is not, which is the
/// one place the socket kind does not fit the table's existing shape. That seam's own
/// documentation names the answer and this is it: `error` is what a device reports as `POLLNVAL`
/// or `POLLERR`, and it is the only answer that does not invent readiness a caller would act on.
/// Reporting `readable` would send a caller into a `recv` that cannot work; reporting nothing at
/// all would make a broken socket indistinguishable from a quiet one, and a `poll` loop over it
/// would spin until its deadline with nothing to show for it.
const SOCKET_READINESS_UNAVAILABLE: Readiness =
    Readiness { readable: false, writable: false, hangup: false, error: true };

/// The failure readiness is an error and claims no readiness at all.
///
/// **A compile-time assertion rather than a test, because every term is a constant.** It was
/// written as a test first and clippy pointed out that the comparisons fold away — the same lint,
/// on the same ground, that moved `MAX_GUEST_FILES`'s ceiling check in `omni-android`'s bionic
/// adapter out of a test and up beside its constant. A test that asserts `true` asserts nothing,
/// and reads like a covered case.
///
/// What it pins is a relation to the two answers this constant must **not** be.
/// [`Readiness::ALWAYS`] would send a caller into a transfer it has no evidence for; an all-false
/// readiness would make a socket the host refuses to poll indistinguishable from a quiet one, and
/// a `poll` loop over that spins to its deadline with nothing to show for it.
const _: () = assert!(SOCKET_READINESS_UNAVAILABLE.error);
const _: () = assert!(!SOCKET_READINESS_UNAVAILABLE.readable);
const _: () = assert!(!SOCKET_READINESS_UNAVAILABLE.writable);
const _: () = assert!(
    SOCKET_READINESS_UNAVAILABLE.readable != Readiness::ALWAYS.readable
        || SOCKET_READINESS_UNAVAILABLE.writable != Readiness::ALWAYS.writable
        || SOCKET_READINESS_UNAVAILABLE.error != Readiness::ALWAYS.error
);

impl Entry {
    /// What this descriptor would do right now.
    ///
    /// **A total function over the table**, which is the successor to the argument
    /// `omni-android`'s `net` module used to make: `poll` reported every open descriptor as ready
    /// because every descriptor was a regular file, a directory or a standard stream, and that
    /// stopped being true the moment `pipe` was bound. Making readiness a `match` with no default
    /// arm means a sixth kind cannot be added without deciding its answer.
    fn readiness(&self) -> Option<Readiness> {
        Some(match self {
            // A regular file, a directory, a character device and a standard stream can none of
            // them block. `Readiness::ALWAYS` says why that is Linux's answer too.
            Entry::File { .. }
            | Entry::Directory { .. }
            | Entry::Standard(_)
            | Entry::Device(_)
            | Entry::Generated(_) => Readiness::ALWAYS,
            Entry::Pipe(handle) => handle.readiness(),
            Entry::EventFd(counter) => counter.readiness(),
            // An epoll descriptor is readable when a member has an event to report, which is a
            // question about every member at once and about more than one waiting side. No run
            // has asked it; `Filesystem::readiness` refuses by name.
            Entry::Epoll(_) => return None,
            // On the clock the guest's own `CLOCK_MONOTONIC` reads -- see `timerfd`.
            Entry::TimerFd(timer) => timer.readiness(crate::clock::monotonic_now()),
            // **The one kind whose answer is a question for the operating system**, and therefore
            // the one that can fail where this function cannot. See
            // [`SOCKET_READINESS_UNAVAILABLE`] for why a refused poll is reported as an error
            // rather than as "not ready": the two are different facts and a caller acts on them
            // differently.
            //
            // A poisoned socket lock is taken over rather than panicked on, for the reason
            // [`Filesystem::table`] gives for the table's own: a panic inside a handler is
            // reachable from guest code, and the state behind this lock is one socket.
            Entry::Socket(socket) => socket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .readiness()
                .unwrap_or(SOCKET_READINESS_UNAVAILABLE),
        })
    }
}

/// Where a descriptor's readiness comes from, which is what a caller waiting on a **mixed** set
/// has to know.
///
/// There is no single call that waits on this runtime's own descriptors and on the host's sockets
/// at once: the in-process half is a condition variable ([`ReadyGate`]) and the socket half is a
/// kernel object. A caller therefore tests everything, waits a slice on whichever side can wait,
/// and tests again — and to do that it has to be able to ask which side each descriptor is on.
///
/// Answering it here rather than leaving the caller to infer it from
/// [`Filesystem::pipe_end`] and friends is the difference between one decision and a growing list
/// of "and also an eventfd, and also a socket" tests at every wait site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadinessSource {
    /// A regular file, a directory, a character device or a standard stream: always ready, so
    /// there is nothing to wait for and a caller that waits on one waits for ever.
    Immediate,
    /// A pipe or an eventfd: in-process state, and [`Filesystem::wait_for_readiness`] is how a
    /// change in it is waited for.
    Gate,
    /// A socket: the host's own readiness call, [`crate::net::poll`], is the only thing that can
    /// wait for it.
    Host,
    /// A timerfd: its readiness changes when its **deadline** passes, which raises nothing -- so a
    /// waiter caps its wait at [`Filesystem::timer_deadline`] -- and when it is re-armed, which
    /// raises the gate. Both halves are needed: the first is the timer firing, the second is
    /// another thread moving it.
    Timer,
}

/// A character device the guest can open by its POSIX path.
///
/// # Why the filesystem seam has devices at all
///
/// **M3's gate found it, at `init_array[3118]`**, and the message came from the guest's own C++
/// runtime: `libc++abi: terminating due to uncaught exception of type std::system_error:
/// random_device failed to open /dev/urandom: No such file or directory`. `std::random_device`
/// opens `/dev/urandom` and there is no way to satisfy it with a file — the guest reads from it
/// for the life of the process and any file would run out.
///
/// It is **not** a hole in D23's confinement. The confinement rule is that a guest path resolves
/// inside one host directory; `/dev/urandom` resolves to no host path at all, and what serves it
/// is `crate::process::random_bytes`, which is the same OS entropy source `arc4random_buf`
/// already answers from. Nothing here can reach the host filesystem through it.
///
/// The set is closed and small on purpose: a device is something this seam *implements*, and
/// every name added to [`DEVICES`] is a claim that it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// `/dev/urandom` and `/dev/random`: the host's entropy source.
    ///
    /// **The two are the same here, and on Linux since 5.6 they are the same there too** — the
    /// blocking pool was removed and `/dev/random` blocks only until the CRNG is initialised,
    /// which it is long before a process starts. There is no distinction left to model.
    ///
    /// Reads always fill the whole buffer, which is what a modern `/dev/urandom` does. Writes are
    /// accepted and discarded: writing to `/dev/urandom` stirs the kernel's pool and the bytes
    /// are consumed, so "accepted, no observable effect" is the contract rather than a shortcut.
    Random,
    /// `/dev/null`: reads end of file, writes are discarded.
    Null,
    /// `/dev/zero`: reads fill with zero bytes, writes are discarded.
    Zero,
}

/// Which device a guest path names, if any.
///
/// The path is put through [`path::resolve_lexically`] first, so the answer is about the path the
/// guest *means* rather than the bytes it typed: `/dev/./urandom`, `/dev//urandom` and
/// `/x/../dev/urandom` are all `/dev/urandom`, and none of them can slip past by spelling. A path
/// that is not resolvable at all is not a device, and the caller's ordinary resolution then
/// reports why.
#[must_use]
pub fn device_for(guest_path: &[u8]) -> Option<Device> {
    let resolved = path::resolve_lexically("device", guest_path).ok()?;
    let spelled = resolved.guest_path();
    DEVICES.iter().find(|(name, _)| *name == spelled).map(|(_, device)| *device)
}

/// What `stat` and `fstat` say about a device.
///
/// A character device: no size, no times, and an identity taken from its own path so that two
/// descriptors on one device compare equal and two on different devices do not.
fn device_stat(device: Device) -> FileStat {
    FileStat {
        kind: FileKind::Other,
        size: 0,
        read_only: false,
        accessed: None,
        modified: None,
        created: None,
        identity: identity(Path::new(
            DEVICES
                .iter()
                .find(|(_, candidate)| *candidate == device)
                .map_or("/dev", |(name, _)| *name),
        )),
    }
}

/// Every device path this seam serves, and what serves it.
///
/// Matched against the guest's path **after** `path::normalise` has canonicalised it, so
/// `/dev/./urandom` and `/dev//urandom` reach the same entry and no spelling slips past.
pub const DEVICES: &[(&str, Device)] = &[
    ("/dev/urandom", Device::Random),
    ("/dev/random", Device::Random),
    ("/dev/null", Device::Null),
    ("/dev/zero", Device::Zero),
];

/// What produces a generated file's bytes. See [`Filesystem::serve_generated`].
///
/// Called each time a read of the file **starts at offset 0**, and never otherwise, so what it
/// returns is what one reading of the file sees from start to end. An error is the file refusing
/// to exist in that reading: it reaches the caller as the `open` or `read` that asked.
pub type Generator = dyn Fn() -> FsResult<Vec<u8>> + Send + Sync;

/// One open descriptor on a generated file: where it has read to, and what it is reading.
///
/// # Why a snapshot, and why it is retaken only at offset 0
///
/// This is Linux's `seq_file`, which is what `/proc/meminfo` and `/proc/self/statm` are, carried
/// across as its two observable rules:
///
/// * **A read from offset 0 generates the file afresh.** A reader that keeps the descriptor open
///   and reads it again from the start sees new numbers -- MEASURED, the engine opens both files
///   once, keeps the descriptors, and `pread`s them at offset 0 every time it wants a reading
///   (`libroblox.so` link `0x22826c8` and `0x22829c0`). A snapshot taken at `open` and served for
///   ever would hand it the same memory figures for the life of the run.
/// * **A read that continues from where another left off continues the same text.** A reader that
///   takes a file in pieces must get the pieces of *one* file: a second generation between two
///   pieces could change a number's width and hand the reader half of one line and half of
///   another. This matters here, not only in principle: the adapter's `pread` moves a guest's
///   request in [`IO_BLOCK`] chunks and asks for the next chunk at the offset the last one
///   ended, so without this rule one guest `pread` could itself be two generations.
struct GeneratedFile {
    /// The guest path it was opened by, normalised, for diagnostics and `fstat`'s identity.
    guest: String,
    generate: Arc<Generator>,
    /// What the last generation produced. `None` only before the first read.
    snapshot: Option<Vec<u8>>,
    /// The descriptor's own offset, which `read` advances and `lseek` moves.
    position: u64,
}

impl core::fmt::Debug for GeneratedFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GeneratedFile")
            .field("guest", &self.guest)
            .field("snapshot_bytes", &self.snapshot.as_ref().map(Vec::len))
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl GeneratedFile {
    /// Copy what the file holds from `offset` into `buf`, generating it first if the read starts
    /// at the beginning. Short at the end, and zero past it, as a file is.
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> FsResult<usize> {
        if offset == 0 || self.snapshot.is_none() {
            self.snapshot = Some((self.generate)()?);
        }
        let bytes = self.snapshot.as_deref().unwrap_or_default();
        // An offset past what `usize` can hold is past the end of any file this process holds.
        let start = usize::try_from(offset).map_or(bytes.len(), |at| at.min(bytes.len()));
        let count = buf.len().min(bytes.len() - start);
        buf[..count].copy_from_slice(&bytes[start..start + count]);
        Ok(count)
    }

    /// `read(2)`: [`read_at`](Self::read_at) the descriptor's own offset, and advance it.
    fn read(&mut self, buf: &mut [u8]) -> FsResult<usize> {
        let count = self.read_at(buf, self.position)?;
        self.position += count as u64;
        Ok(count)
    }

    /// `lseek(2)` as `seq_lseek` answers it: `SEEK_SET` and `SEEK_CUR` move the offset, and
    /// `SEEK_END` is `EINVAL`, because a generated file has no end until it is generated.
    fn seek(&mut self, offset: i64, whence: i32) -> FsResult<u64> {
        const OP: &str = "lseek";
        let invalid = |why: &'static str| {
            FsError::kinded(OP, self.guest.clone(), FsErrorKind::InvalidInput, why)
        };
        let base: i64 = match whence {
            0 => 0,
            1 => i64::try_from(self.position)
                .map_err(|_| invalid("the current offset does not fit an off_t"))?,
            2 => {
                return Err(invalid(
                    "SEEK_END on a generated file, which has no size until it is read; Linux's \
                     seq_lseek answers EINVAL",
                ))
            }
            _ => return Err(invalid("whence is none of SEEK_SET, SEEK_CUR and SEEK_END")),
        };
        let target = base
            .checked_add(offset)
            .ok_or_else(|| invalid("the resulting offset overflows an off_t"))?;
        let target =
            u64::try_from(target).map_err(|_| invalid("the resulting offset is negative"))?;
        self.position = target;
        Ok(target)
    }
}

/// The generated files one [`Filesystem`] serves, by normalised guest path.
#[derive(Default)]
struct GeneratedFiles(BTreeMap<String, Arc<Generator>>);

impl core::fmt::Debug for GeneratedFiles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

/// What `stat` and `fstat` say about a generated file: what Linux says about a `/proc` file.
///
/// A regular file, **size zero** -- `/proc/meminfo` reports `st_size` 0 on a device, because its
/// length is not known until it is generated -- read-only, with no times this seam could vouch
/// for, and an identity taken from its path.
fn generated_stat(guest: &str) -> FileStat {
    FileStat {
        kind: FileKind::Regular,
        size: 0,
        read_only: true,
        accessed: None,
        modified: None,
        created: None,
        identity: identity(Path::new(guest)),
    }
}

/// Which standard stream a reserved descriptor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdStream {
    /// Reads end-of-file immediately.
    ///
    /// **A fact about this process, not a stub.** The guest was started with no terminal and no
    /// pipe on its standard input, so there is nothing to read and end-of-file is what a real
    /// process in that position gets. Blocking would be the wrong answer and inventing input
    /// would be a worse one.
    In,
    /// The host's standard output.
    Out,
    /// The host's standard error.
    Err,
}

/// One open directory stream: a snapshot, and how far through it the guest is.
#[derive(Debug)]
struct DirStream {
    entries: Vec<DirEntryInfo>,
    position: usize,
    guest: String,
}

/// One guest instance's filesystem: a root, a descriptor table and the open directory streams.
///
/// Per instance, never process-wide, for the same reason [`Bionic`] itself is: the runtime hosts
/// several isolated guests in one process and two of them sharing a descriptor table would mean
/// one guest's `close(7)` closing the other's file.
///
/// [`Bionic`]: https://docs.rs/omni-android
#[derive(Debug)]
pub struct Filesystem {
    root: PathBuf,
    table: Mutex<Table>,
    /// Guest paths `open` was asked for and could not produce. See [`Filesystem::open_misses`].
    misses: Mutex<Vec<Vec<u8>>>,
    /// Rises whenever any pipe in this instance changes state. See [`pipe::ReadyGate`]: it is how
    /// a caller waits for readiness without this seam ever choosing how long to wait.
    gate: Arc<ReadyGate>,
    /// The paths this instance serves from a generator. See [`Filesystem::serve_generated`].
    generated: Mutex<GeneratedFiles>,
}

#[derive(Debug)]
struct Table {
    open: BTreeMap<i32, Entry>,
    /// The descriptors carrying `FD_CLOEXEC`. A property of the **descriptor**, not of what it
    /// names, so it is kept beside the table rather than in an [`Entry`]; `close` takes a number
    /// out, so a number the next open reuses starts without it, as on Linux.
    close_on_exec: BTreeSet<i32>,
    dirs: BTreeMap<i32, DirStream>,
    next_dir: i32,
}

impl Filesystem {
    /// Build a filesystem confined to `root`.
    ///
    /// `root` is canonicalised once, here, so that the per-call path walk compares against a real
    /// absolute path. On Windows that canonical form is a verbatim `\\?\` path, which is a second
    /// line of defence under [`path`]'s rules rather than a replacement for them: Win32 does not
    /// strip trailing dots from a verbatim path and does not treat `NUL` as a device in one.
    ///
    /// # Errors
    ///
    /// [`FsError::Refused`] if `root` does not exist or is not a directory. A filesystem rooted
    /// at something that is not a directory would resolve every guest path onto a path that
    /// cannot exist, and every call would report `ENOENT` — a plausible answer that says nothing
    /// about the real mistake.
    pub fn new(root: impl AsRef<Path>) -> FsResult<Filesystem> {
        let given = root.as_ref();
        let canonical = std::fs::canonicalize(given).map_err(|error| {
            FsError::refused(
                "Filesystem::new",
                given.display().to_string(),
                format!(
                    "the guest's filesystem root could not be resolved: {error}. Every guest path \
                     is resolved inside this directory, so it has to exist before the guest runs"
                ),
            )
        })?;
        if !canonical.is_dir() {
            return Err(FsError::refused(
                "Filesystem::new",
                canonical.display().to_string(),
                "the guest's filesystem root is not a directory",
            ));
        }
        Ok(Filesystem {
            root: canonical,
            misses: Mutex::new(Vec::new()),
            table: Mutex::new(Table {
                open: BTreeMap::from([
                    (STDIN_FD, Entry::Standard(StdStream::In)),
                    (STDOUT_FD, Entry::Standard(StdStream::Out)),
                    (STDERR_FD, Entry::Standard(StdStream::Err)),
                ]),
                close_on_exec: BTreeSet::new(),
                dirs: BTreeMap::new(),
                next_dir: 1,
            }),
            gate: Arc::new(ReadyGate::default()),
            generated: Mutex::new(GeneratedFiles::default()),
        })
    }

    /// Serve `guest_path` from `generate` rather than from under the root.
    ///
    /// # What this is for, and what it is not
    ///
    /// A file whose contents are a **fact this process can state** rather than bytes anybody
    /// stored: `/proc/meminfo` and `/proc/self/statm` are the reason it exists. The caller decides
    /// which paths and what they say; this seam only makes the path behave as a file -- `open`,
    /// `read`, `pread`, `lseek`, `fstat`, `stat`, `access` and `close` -- and knows nothing about
    /// `/proc`. Every other guest path still resolves under the root, so this does not widen what
    /// the guest can reach: a generated file names no host path at all.
    ///
    /// A generated file is **read-only**, `0444`, which is what an app sees on `/proc`: opening it
    /// for writing or with `O_TRUNC` is `EACCES`, and nothing can create, rename or remove it.
    ///
    /// # Set once
    ///
    /// A path may be served once and never replaced, for the reason the root may be set once: a
    /// descriptor opened under one generator must not start reading another's file.
    ///
    /// # Errors
    ///
    /// As [`path::resolve_lexically`] for a path that cannot name anything, and
    /// [`FsError::Refused`] for a path that is one of the [`DEVICES`] or is already served.
    pub fn serve_generated(&self, guest_path: &[u8], generate: Arc<Generator>) -> FsResult<()> {
        const OP: &str = "serve_generated";
        let name = path::resolve_lexically(OP, guest_path)?.guest_path();
        if device_for(guest_path).is_some() {
            return Err(FsError::refused(
                OP,
                name,
                "the path is one of the devices this seam implements, and a device cannot also be \
                 a generated file",
            ));
        }
        let mut generated =
            self.generated.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if generated.0.contains_key(&name) {
            return Err(FsError::refused(
                OP,
                name,
                "the path is already served. It may be served once and never replaced: a \
                 descriptor opened on one generator must not start reading another's file",
            ));
        }
        generated.0.insert(name, generate);
        Ok(())
    }

    /// The generated file a guest path names, if it names one: its normalised path and generator.
    ///
    /// Normalised first, as [`device_for`] is, so `/proc/./meminfo` and `/proc//meminfo` reach the
    /// same file and no spelling slips past to the root.
    fn generated_for(&self, guest_path: &[u8]) -> Option<(String, Arc<Generator>)> {
        let name = path::resolve_lexically("generated", guest_path).ok()?.guest_path();
        let generated = self.generated.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let generate = Arc::clone(generated.0.get(&name)?);
        Some((name, generate))
    }

    /// `open` of a generated file: the flag checks a read-only `/proc` file makes, a first
    /// generation, and a descriptor.
    fn open_generated(
        &self,
        name: String,
        generate: Arc<Generator>,
        flags: OpenFlags,
    ) -> FsResult<i32> {
        const OP: &str = "open";
        if flags.directory {
            return Err(FsError::kinded(
                OP,
                name,
                FsErrorKind::NotADirectory,
                "O_DIRECTORY was given and a generated file is not a directory",
            ));
        }
        if flags.create && flags.exclusive {
            return Err(FsError::kinded(
                OP,
                name,
                FsErrorKind::AlreadyExists,
                "O_CREAT | O_EXCL on a file that exists (EEXIST)",
            ));
        }
        // Linux adds `MAY_WRITE` to the access check for `O_TRUNC` as well as for a writable
        // mode, and a `0444` file refuses both with `EACCES`.
        if flags.write || flags.truncate {
            return Err(FsError::kinded(
                OP,
                name,
                FsErrorKind::PermissionDenied,
                "a generated file is read-only (mode 0444), so opening it for writing or with \
                 O_TRUNC is EACCES -- what an app is told for a file under /proc",
            ));
        }
        // **Generated once here, before a descriptor exists**, so a file that cannot be produced
        // is refused by the `open` that asked for it, naming why, rather than handed out as a
        // descriptor whose first read fails. The bytes are kept: a first read that continues
        // from a nonzero offset reads them rather than generating a second time.
        let first = generate()?;
        let mut table = self.table();
        if table.open.len() >= MAX_OPEN_FILES {
            return Err(FsError::kinded(
                OP,
                name,
                FsErrorKind::TooManyOpenFiles,
                format!("this guest instance already holds {MAX_OPEN_FILES} descriptors"),
            ));
        }
        let fd = table.lowest_free_fd();
        table.open.insert(
            fd,
            Entry::Generated(GeneratedFile {
                guest: name,
                generate,
                snapshot: Some(first),
                position: 0,
            }),
        );
        Ok(fd)
    }

    /// The host directory every guest path resolves inside.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The table, with a poisoned lock taken over rather than panicked on.
    ///
    /// A panic in a handler is reachable from guest code (Global Constraint 11), and a poisoned
    /// mutex here means some earlier call panicked while holding it — which is a defect to report,
    /// not a reason to abort the host. The state behind the lock is a descriptor map, and the only
    /// way it can be inconsistent is that one entry was half-inserted.
    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Resolve a guest path to a host path inside the root.
    ///
    /// Public because the confinement rules are the part of this seam most worth testing directly,
    /// and because a caller that wants to report *which* host file a guest path became has no
    /// other way to ask.
    ///
    /// # Errors
    ///
    /// See [`path`]: [`FsError::Confined`] for a path that tries to leave the root, and
    /// [`FsError::Io`] for the length and emptiness rules a real device also enforces.
    pub fn resolve(
        &self,
        operation: &'static str,
        guest_path: &[u8],
        final_link: FinalLink,
    ) -> FsResult<PathBuf> {
        let resolved = path::resolve_lexically(operation, guest_path)?;
        path::locate(operation, &self.root, &resolved, final_link)
    }

    /// The absolute guest path some bytes resolve to, for a diagnostic.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve)'s lexical half.
    pub fn guest_path(&self, operation: &'static str, guest_path: &[u8]) -> FsResult<String> {
        Ok(path::resolve_lexically(operation, guest_path)?.guest_path())
    }

    // ---------------------------------------------------------------- descriptors

    /// `open(2)`: open a path and return the lowest free descriptor.
    ///
    /// # Errors
    ///
    /// [`FsError::Confined`] for a path that cannot be resolved inside the root,
    /// [`FsError::Io`] for a host failure, including [`FsErrorKind::TooManyOpenFiles`] when this
    /// instance already holds [`MAX_OPEN_FILES`].
    pub fn open(&self, guest_path: &[u8], flags: OpenFlags) -> FsResult<i32> {
        let outcome = self.open_inner(guest_path, flags);
        if outcome.is_err() {
            let mut misses = self.misses.lock().expect("the miss list is not poisoned");
            // Bounded, and **keeping the oldest**: the first thing a guest could not find is what
            // explains the rest, and a run that asks for one missing file in a retry loop must not
            // push it out with copies of itself.
            if misses.len() < MAX_RECORDED_MISSES && !misses.iter().any(|seen| seen == guest_path) {
                misses.push(guest_path.to_vec());
            }
        }
        outcome
    }

    /// Guest paths this instance was asked to open and could not, oldest first, deduplicated.
    ///
    /// # Why a seam needs this at all
    ///
    /// A missing file is not an error this layer can see the consequence of. `open` answers
    /// `ENOENT`, which is a perfectly ordinary thing for a guest to be told and to handle — so the
    /// failure is *correct here* and lands somewhere else entirely, in whatever the guest does
    /// without the file.
    ///
    /// **MEASURED, and it is what this was added for.** Roblox's settings fetch exchanged bytes
    /// over real sockets in both directions and then reported `HttpError: Unknown`, with no refusal
    /// and no dead thread anywhere in the run. The APK ships `assets/ssl/cacert.pem` — 228,725
    /// bytes of certificate authorities — and OpenSSL's compiled-in default store is
    /// `/actions-runner/_work/openssl/openssl/pkg/ssl/cert.pem`, a build machine's path that exists
    /// on no device. On Android the Java side puts the bundle where the engine can open it; D7 says
    /// the Java side is *defined* rather than executed, so if nothing here does it, nothing does.
    ///
    /// An `ENOENT` is the quietest possible failure and this is the only place that can say it
    /// happened.
    #[must_use]
    pub fn open_misses(&self) -> Vec<Vec<u8>> {
        self.misses.lock().expect("the miss list is not poisoned").clone()
    }

    fn open_inner(&self, guest_path: &[u8], flags: OpenFlags) -> FsResult<i32> {
        const OP: &str = "open";
        let shown = path::display(guest_path);
        if !flags.read && !flags.write && !flags.directory {
            return Err(FsError::kinded(
                OP,
                shown,
                FsErrorKind::InvalidInput,
                "a descriptor opened for neither reading nor writing",
            ));
        }
        // A device is checked before the path is resolved, because it is not a path under the
        // root and resolving it would answer `ENOENT` for something this seam does implement.
        if let Some(device) = device_for(guest_path) {
            let mut table = self.table();
            if table.open.len() >= MAX_OPEN_FILES {
                return Err(FsError::kinded(
                    OP,
                    shown,
                    FsErrorKind::TooManyOpenFiles,
                    format!("this guest instance already holds {MAX_OPEN_FILES} descriptors"),
                ));
            }
            if flags.directory {
                return Err(FsError::kinded(
                    OP,
                    shown,
                    FsErrorKind::NotADirectory,
                    "a character device is not a directory",
                ));
            }
            let fd = table.lowest_free_fd();
            table.open.insert(fd, Entry::Device(device));
            return Ok(fd);
        }
        // A generated file is checked here for the device's reason: it is not a path under the
        // root, and resolving it there would answer `ENOENT` for a file this instance serves.
        if let Some((name, generate)) = self.generated_for(guest_path) {
            return self.open_generated(name, generate, flags);
        }
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let mut table = self.table();
        if table.open.len() >= MAX_OPEN_FILES {
            return Err(FsError::kinded(
                OP,
                shown,
                FsErrorKind::TooManyOpenFiles,
                format!("this guest instance already holds {MAX_OPEN_FILES} descriptors"),
            ));
        }
        let fd = table.lowest_free_fd();
        let entry = if flags.directory {
            let metadata = std::fs::metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
            if !metadata.is_dir() {
                return Err(FsError::kinded(
                    OP,
                    shown,
                    FsErrorKind::NotADirectory,
                    "O_DIRECTORY was given and the path is not a directory",
                ));
            }
            Entry::Directory { host, guest: shown }
        } else {
            let mut options = std::fs::OpenOptions::new();
            options
                .read(flags.read)
                .write(flags.write)
                .create(flags.create && !flags.exclusive)
                .create_new(flags.create && flags.exclusive)
                .truncate(flags.truncate && flags.write)
                .append(flags.append && flags.write);
            let file = options.open(&host).map_err(|e| FsError::io(OP, &host, &e))?;
            Entry::File { file, readable: flags.read, writable: flags.write, host, guest: shown }
        };
        table.open.insert(fd, entry);
        Ok(fd)
    }

    /// `close(2)`.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold.
    pub fn close(&self, fd: i32) -> FsResult<()> {
        let mut table = self.table();
        table.close_on_exec.remove(&fd);
        match table.open.remove(&fd) {
            // Dropping the entry closes the host handle. A close that the host itself fails is
            // not reportable through `Drop`, and POSIX already says the descriptor is gone
            // whatever `close` returned.
            Some(_) => {
                // **And it leaves every interest list** -- see [`epoll`] for why a number reused
                // by the next `open` must not inherit a watch on the object this one named.
                for entry in table.open.values_mut() {
                    if let Entry::Epoll(set) = entry {
                        set.forget(fd);
                    }
                }
                Ok(())
            }
            None => Err(bad_fd("close", fd)),
        }
    }

    /// Whether this instance holds `fd`.
    #[must_use]
    pub fn is_open(&self, fd: i32) -> bool {
        self.table().open.contains_key(&fd)
    }

    /// The guest path `fd` was opened by, for a file, a directory or a generated file; `None`
    /// for every other kind, which no path names, and for a descriptor not held.
    ///
    /// **For a refusal to name the file, which a number does not.** MEASURED: two workers asked
    /// `mmap` for a writable shared mapping of descriptors 18 and 27, and nothing said what
    /// either was.
    #[must_use]
    pub fn guest_path_of(&self, fd: i32) -> Option<String> {
        match self.table().open.get(&fd)? {
            Entry::File { guest, .. } | Entry::Directory { guest, .. } => Some(guest.clone()),
            Entry::Generated(generated) => Some(generated.guest.clone()),
            _ => None,
        }
    }

    // ---------------------------------------------------------------- pipes and readiness

    /// `pipe(2)`: a read end and a write end, in that order.
    ///
    /// **Both descriptors are allocated or neither is.** A `pipe` that took the last free slot
    /// for its read end and then failed on its write end would leave the guest a descriptor it
    /// never learned the number of, which is a leak it cannot close.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::TooManyOpenFiles`] when two more descriptors would take this instance past
    /// [`MAX_OPEN_FILES`].
    pub fn pipe(&self) -> FsResult<(i32, i32)> {
        const OP: &str = "pipe";
        let mut table = self.table();
        if table.open.len() + 2 > MAX_OPEN_FILES {
            return Err(FsError::kinded(
                OP,
                "a pipe",
                FsErrorKind::TooManyOpenFiles,
                format!(
                    "this guest instance holds {} of {MAX_OPEN_FILES} descriptors and a pipe \
                     needs two more",
                    table.open.len()
                ),
            ));
        }
        let (read_end, write_end) = pipe::create(Arc::clone(&self.gate));
        let read_fd = table.lowest_free_fd();
        table.open.insert(read_fd, Entry::Pipe(read_end));
        let write_fd = table.lowest_free_fd();
        table.open.insert(write_fd, Entry::Pipe(write_end));
        Ok((read_fd, write_fd))
    }

    /// `eventfd(2)`: a counter with a descriptor.
    ///
    /// `initval` is what the counter starts at and `flags` is the `EFD_*` set. **A flag outside
    /// [`eventfd::KNOWN_FLAGS`] is refused rather than ignored**, which is the kernel's own answer
    /// and this seam's rule everywhere else: accepting a flag it cannot honour would tell the
    /// guest it got a behaviour it did not.
    ///
    /// `EFD_CLOEXEC` is recorded on the descriptor ([`Filesystem::is_close_on_exec`]) and is
    /// otherwise inert — there is no `exec` in this runtime — and refusing it would refuse the
    /// flag almost every real caller sets.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::TooManyOpenFiles`] past [`MAX_OPEN_FILES`], and
    /// [`FsErrorKind::InvalidInput`] for an unknown flag.
    pub fn eventfd(&self, initval: u64, flags: i32) -> FsResult<i32> {
        const OP: &str = "eventfd";
        let unknown = flags & !eventfd::KNOWN_FLAGS;
        if unknown != 0 {
            return Err(FsError::kinded(
                OP,
                "an eventfd",
                FsErrorKind::InvalidInput,
                format!(
                    "flags {flags:#o} contain {unknown:#o}, which `eventfd2` does not define. \
                     Ignoring it would be this seam accepting a request it cannot honour"
                ),
            ));
        }
        let mut table = self.table();
        if table.open.len() + 1 > MAX_OPEN_FILES {
            return Err(FsError::kinded(
                OP,
                "an eventfd",
                FsErrorKind::TooManyOpenFiles,
                format!(
                    "this guest instance holds {} of {MAX_OPEN_FILES} descriptors",
                    table.open.len()
                ),
            ));
        }
        let counter = eventfd::EventFd::new(
            initval,
            flags & eventfd::EFD_SEMAPHORE != 0,
            flags & eventfd::EFD_NONBLOCK != 0,
            Arc::clone(&self.gate),
        );
        let fd = table.lowest_free_fd();
        table.open.insert(fd, Entry::EventFd(counter));
        if flags & eventfd::EFD_CLOEXEC != 0 {
            table.close_on_exec.insert(fd);
        }
        Ok(fd)
    }

    /// `epoll_create1(2)`: a new, empty interest list, as a descriptor.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::TooManyOpenFiles`] past [`MAX_OPEN_FILES`].
    pub fn epoll_create(&self) -> FsResult<i32> {
        let mut table = self.table();
        if table.open.len() + 1 > MAX_OPEN_FILES {
            return Err(FsError::kinded(
                "epoll_create",
                "an epoll instance",
                FsErrorKind::TooManyOpenFiles,
                format!(
                    "this guest instance holds {} of {MAX_OPEN_FILES} descriptors",
                    table.open.len()
                ),
            ));
        }
        let fd = table.lowest_free_fd();
        table.open.insert(fd, Entry::Epoll(epoll::EpollSet::default()));
        Ok(fd)
    }

    /// `epoll_ctl(2)` on the interest list `epfd`.
    ///
    /// The kernel's answers, each from where the kernel decides it: `EBADF` for either descriptor
    /// not open; `EINVAL` for an `epfd` that is not an epoll instance or for `fd == epfd`;
    /// `EPERM` ([`FsErrorKind::NotPollable`]) for a regular file or a directory; `EEXIST` and
    /// `ENOENT` from the list itself. Nesting refuses by name -- see [`epoll`].
    ///
    /// # Errors
    ///
    /// As above.
    pub fn epoll_ctl(&self, epfd: i32, op: EpollOp, fd: i32, member: EpollMember) -> FsResult<()> {
        const OP: &str = "epoll_ctl";
        let mut table = self.table();
        let target = format!("epfd {epfd}, fd {fd}");
        match table.open.get(&epfd) {
            None => return Err(bad_fd(OP, epfd)),
            Some(Entry::Epoll(_)) => {}
            Some(_) => {
                return Err(FsError::kinded(
                    OP,
                    target,
                    FsErrorKind::InvalidInput,
                    "epfd is not an epoll descriptor",
                ))
            }
        }
        if fd == epfd {
            return Err(FsError::kinded(
                OP,
                target,
                FsErrorKind::InvalidInput,
                "an epoll instance cannot watch itself",
            ));
        }
        // What `fd` is decides whether it may be watched, **for every op, DEL included**: Linux's
        // `do_epoll_ctl` tests `file_can_poll` before it looks at the list at all.
        match table.open.get(&fd) {
            None => return Err(bad_fd(OP, fd)),
            // A generated file is here too: `/proc/meminfo` has no `poll` operation, so Linux's
            // `file_can_poll` is false for it exactly as for a regular file.
            Some(Entry::File { .. } | Entry::Directory { .. } | Entry::Generated(_)) => {
                return Err(FsError::kinded(
                    OP,
                    target,
                    FsErrorKind::NotPollable,
                    "a regular file or a directory is always ready and cannot be polled, so the \
                     kernel refuses to watch one (EPERM)",
                ))
            }
            Some(Entry::Epoll(_)) => {
                return Err(FsError::refused(
                    OP,
                    target,
                    "an epoll descriptor inside another's interest list: Linux allows nesting, \
                     and no run has reached it",
                ))
            }
            Some(_) => {}
        }
        let Some(Entry::Epoll(set)) = table.open.get_mut(&epfd) else {
            // Checked two statements up under this same lock, and nothing between yields.
            return Err(bad_fd(OP, epfd));
        };
        set.apply(op, fd, member).map_err(|refusal| match refusal {
            epoll::EpollRefusal::AlreadyWatched => FsError::kinded(
                OP,
                format!("epfd {epfd}, fd {fd}"),
                FsErrorKind::AlreadyExists,
                "fd is already in this interest list (EEXIST)",
            ),
            epoll::EpollRefusal::NotWatched => FsError::kinded(
                OP,
                format!("epfd {epfd}, fd {fd}"),
                FsErrorKind::NotFound,
                "fd is not in this interest list (ENOENT)",
            ),
        })
    }

    /// The interest list behind `epfd`, in ascending descriptor order.
    ///
    /// # Errors
    ///
    /// `EBADF` for a descriptor that is not open and `EINVAL` for one that is not an epoll
    /// instance -- `epoll_wait`'s own two answers.
    pub fn epoll_members(&self, epfd: i32) -> FsResult<Vec<(i32, EpollMember)>> {
        match self.table().open.get(&epfd) {
            None => Err(bad_fd("epoll_wait", epfd)),
            Some(Entry::Epoll(set)) => Ok(set.members()),
            Some(_) => Err(FsError::kinded(
                "epoll_wait",
                format!("fd {epfd}"),
                FsErrorKind::InvalidInput,
                "not an epoll descriptor",
            )),
        }
    }

    /// Whether `fd` is an epoll instance -- what a caller about to ask for readiness checks, so
    /// that an unanswered question refuses by name instead of reading as "not open".
    #[must_use]
    pub fn is_epoll(&self, fd: i32) -> bool {
        matches!(self.table().open.get(&fd), Some(Entry::Epoll(_)))
    }

    /// `timerfd_create(2)` on `CLOCK_MONOTONIC`, disarmed. The clock is the caller's decision to
    /// refuse; see [`timerfd`] for why it can only be this one.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::InvalidInput`] for a flag outside [`timerfd::KNOWN_FLAGS`], and
    /// [`FsErrorKind::TooManyOpenFiles`] past [`MAX_OPEN_FILES`].
    pub fn timerfd_create(&self, flags: i32) -> FsResult<i32> {
        const OP: &str = "timerfd_create";
        let unknown = flags & !timerfd::KNOWN_FLAGS;
        if unknown != 0 {
            return Err(FsError::kinded(
                OP,
                "a timerfd",
                FsErrorKind::InvalidInput,
                format!("flags {flags:#o} contain {unknown:#o}, which `timerfd_create` does not define"),
            ));
        }
        let mut table = self.table();
        if table.open.len() + 1 > MAX_OPEN_FILES {
            return Err(FsError::kinded(
                OP,
                "a timerfd",
                FsErrorKind::TooManyOpenFiles,
                format!(
                    "this guest instance holds {} of {MAX_OPEN_FILES} descriptors",
                    table.open.len()
                ),
            ));
        }
        let timer =
            timerfd::TimerFd::new(flags & timerfd::TFD_NONBLOCK != 0, Arc::clone(&self.gate));
        let fd = table.lowest_free_fd();
        table.open.insert(fd, Entry::TimerFd(timer));
        if flags & timerfd::TFD_CLOEXEC != 0 {
            table.close_on_exec.insert(fd);
        }
        Ok(fd)
    }

    /// `timerfd_settime(2)`: arm or disarm `fd`, returning `(remaining, interval)` as it was.
    ///
    /// # Errors
    ///
    /// `EBADF` for a descriptor that is not open and `EINVAL` for one that is not a timerfd --
    /// the kernel's two answers.
    pub fn timerfd_settime(
        &self,
        fd: i32,
        absolute: bool,
        value: Duration,
        interval: Duration,
    ) -> FsResult<(Duration, Duration)> {
        match self.table().open.get(&fd) {
            None => Err(bad_fd("timerfd_settime", fd)),
            Some(Entry::TimerFd(timer)) => {
                Ok(timer.settime(absolute, value, interval, crate::clock::monotonic_now()))
            }
            Some(_) => Err(FsError::kinded(
                "timerfd_settime",
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "not a timerfd",
            )),
        }
    }

    /// The next expiry of the timerfd `fd` on the monotonic clock, or `None` when `fd` is not an
    /// armed timerfd. What a waiter caps its wait at -- see [`ReadinessSource::Timer`].
    #[must_use]
    pub fn timer_deadline(&self, fd: i32) -> Option<Duration> {
        match self.table().open.get(&fd) {
            Some(Entry::TimerFd(timer)) => timer.deadline(),
            _ => None,
        }
    }

    /// The counter behind `fd`, or `None` when `fd` is not an eventfd.
    ///
    /// Diagnostic: the guest reads the counter with `read`, which is destructive, so a host that
    /// wants to observe one cannot do it that way.
    #[must_use]
    pub fn eventfd_value(&self, fd: i32) -> Option<u64> {
        match self.table().open.get(&fd) {
            Some(Entry::EventFd(counter)) => Some(counter.value()),
            _ => None,
        }
    }

    // ---------------------------------------------------------------- sockets

    /// Put a socket in this instance's descriptor table and hand back its number.
    ///
    /// **The number is this table's and the socket is [`net`](crate::net)'s**, which is D30 point
    /// 2 split exactly where it says to split it: that module creates a socket and hands out no
    /// descriptor, and this one owns every number a guest can see. A socket therefore competes for
    /// the same [`MAX_OPEN_FILES`] ceiling as a file and a pipe, which is the truth about a
    /// process rather than a convenience — `RLIMIT_NOFILE` on a device counts sockets too.
    ///
    /// Closing is [`close`](Self::close) and nothing else: dropping the entry drops the
    /// [`Socket`](crate::net::Socket), which closes the host descriptor, so there is no second
    /// close that could be forgotten.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::TooManyOpenFiles`] when one more descriptor would take this instance past
    /// [`MAX_OPEN_FILES`]. **The socket is dropped — and therefore closed — on that path**, which
    /// is the only correct thing to do with a host descriptor whose number the caller will never
    /// learn.
    pub fn attach_socket(&self, socket: crate::net::Socket) -> FsResult<i32> {
        const OP: &str = "socket";
        let mut table = self.table();
        if table.open.len() + 1 > MAX_OPEN_FILES {
            return Err(FsError::kinded(
                OP,
                "a socket",
                FsErrorKind::TooManyOpenFiles,
                format!(
                    "this guest instance holds {} of {MAX_OPEN_FILES} descriptors, and a socket \
                     is one of them: a device counts sockets against RLIMIT_NOFILE too",
                    table.open.len()
                ),
            ));
        }
        let fd = table.lowest_free_fd();
        table.open.insert(fd, Entry::Socket(Arc::new(Mutex::new(socket))));
        Ok(fd)
    }

    /// The socket `fd` names, as a handle that outlives the table lock.
    ///
    /// **The clone is the point.** Every operation on a socket is a call that may wait on the
    /// host, and a caller that held the table lock across one would stop this instance's other
    /// descriptors for its duration. So the handle is cloned out here, the table lock is released
    /// when this returns, and the caller locks the socket itself. See [`Entry::Socket`] for the
    /// lock order that makes that safe.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold, and
    /// [`FsErrorKind::NotASocket`] for one that is open and is not a socket — which is `ENOTSOCK`,
    /// and is what a device answers a `connect` on a file.
    pub fn socket_at(&self, fd: i32) -> FsResult<Arc<Mutex<crate::net::Socket>>> {
        const OP: &str = "socket_at";
        match self.table().open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::Socket(socket)) => Ok(Arc::clone(socket)),
            Some(_) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::NotASocket,
                "the descriptor is open and is not a socket, so a socket operation on it is \
                 ENOTSOCK. It is reported rather than answered, because every other descriptor \
                 kind here would have to invent a peer, a family or a readiness it does not have",
            )),
        }
    }

    /// Whether `fd` is a socket. `false` for every other kind, including one that is not open.
    #[must_use]
    pub fn is_socket(&self, fd: i32) -> bool {
        matches!(self.table().open.get(&fd), Some(Entry::Socket(_)))
    }

    /// Which side of a mixed wait `fd` is on, or `None` when it is not open.
    ///
    /// See [`ReadinessSource`]. A **total** function over the descriptor kinds, with no default
    /// arm, for [`Entry::readiness`]'s reason: a seventh kind must decide which side it waits on
    /// rather than inherit an answer.
    #[must_use]
    pub fn readiness_source(&self, fd: i32) -> Option<ReadinessSource> {
        match self.table().open.get(&fd)? {
            Entry::File { .. }
            | Entry::Directory { .. }
            | Entry::Standard(_)
            | Entry::Device(_)
            | Entry::Generated(_) => Some(ReadinessSource::Immediate),
            Entry::Pipe(_) | Entry::EventFd(_) => Some(ReadinessSource::Gate),
            Entry::Socket(_) => Some(ReadinessSource::Host),
            // Its members may be on both sides, so no one answer is true; `readiness` refuses an
            // epoll descriptor before a caller would ask where to wait for it.
            Entry::Epoll(_) => None,
            Entry::TimerFd(_) => Some(ReadinessSource::Timer),
        }
    }

    /// What `fd` would do right now, as `poll` and `select` ask it.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold.
    pub fn readiness(&self, fd: i32) -> FsResult<Readiness> {
        match self.table().open.get(&fd) {
            None => Err(bad_fd("readiness", fd)),
            Some(entry) => entry.readiness().ok_or_else(|| {
                FsError::refused(
                    "readiness",
                    format!("fd {fd}"),
                    "fd is an epoll descriptor, whose readiness is its members' and is not \
                     answered here -- nested epoll, or poll/select/ALooper_addFd on an epoll \
                     descriptor. Linux allows it; no run has reached it",
                )
            }),
        }
    }

    /// Whether `fd` is one end of a pipe, and which end.
    ///
    /// `None` for every other kind, including a descriptor that is not open — a caller that needs
    /// to distinguish those two asks [`is_open`](Self::is_open).
    #[must_use]
    pub fn pipe_end(&self, fd: i32) -> Option<PipeEnd> {
        match self.table().open.get(&fd) {
            Some(Entry::Pipe(handle)) => Some(handle.end()),
            _ => None,
        }
    }

    /// How many descriptors in **this instance** still hold the write end of the pipe `fd` is an
    /// end of, or `None` when `fd` is not a pipe.
    ///
    /// # Why a caller wants this and not `readiness`
    ///
    /// It is the fact an *indefinite* wait is decided on. `ALooper_pollOnce(-1)` and `poll(fds,
    /// n, -1)` are legitimate exactly when something in this runtime can still make the
    /// descriptor ready; for a pipe read end, that is a write end still being open. The readiness
    /// table cannot answer it — a read end reports `hangup` only when it has *both* no writer and
    /// nothing buffered, so `!hangup` is also true for a pipe whose last writer has gone and left
    /// bytes behind, which is a wait that ends once and then never again.
    ///
    /// **A count and not a boolean**, so the caller states its own threshold and a reader of the
    /// call site can see which one it chose.
    #[must_use]
    pub fn pipe_writers(&self, fd: i32) -> Option<usize> {
        match self.table().open.get(&fd) {
            Some(Entry::Pipe(handle)) => Some(handle.pipe().writers()),
            _ => None,
        }
    }

    /// Whether `O_NONBLOCK` is set on `fd`.
    ///
    /// Only a pipe can carry the flag here, because only a pipe can block. For every other kind
    /// this answers `false`, which is the truth rather than a default: a regular file is never
    /// non-blocking because it never blocks.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold.
    pub fn is_nonblocking(&self, fd: i32) -> FsResult<bool> {
        match self.table().open.get(&fd) {
            None => Err(bad_fd("is_nonblocking", fd)),
            Some(Entry::Pipe(handle)) => Ok(handle.nonblocking()),
            Some(Entry::EventFd(counter)) => Ok(counter.nonblocking()),
            Some(Entry::TimerFd(timer)) => Ok(timer.nonblocking()),
            // The socket's own record of the flag, which is what `set_nonblocking` put on the
            // host descriptor. Read back from the socket rather than remembered here, so that the
            // answer to `fcntl(F_GETFL)` cannot disagree with what the kernel was told.
            Some(Entry::Socket(socket)) => Ok(socket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .nonblocking()),
            Some(_) => Ok(false),
        }
    }

    /// Whether `fd` carries `FD_CLOEXEC` -- `fcntl(F_GETFD)`.
    ///
    /// **Recorded and reported, and inert**: nothing in this runtime execs, so the flag changes
    /// nothing a guest can observe except this answer -- which is why the answer has to be the
    /// flag the guest set (by `O_CLOEXEC`, `SOCK_CLOEXEC`, `EFD_CLOEXEC`, `TFD_CLOEXEC`,
    /// `EPOLL_CLOEXEC`, `fopen`'s `e`, or `F_SETFD`) rather than a constant.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold.
    pub fn is_close_on_exec(&self, fd: i32) -> FsResult<bool> {
        let table = self.table();
        if !table.open.contains_key(&fd) {
            return Err(bad_fd("is_close_on_exec", fd));
        }
        Ok(table.close_on_exec.contains(&fd))
    }

    /// Set or clear `FD_CLOEXEC` on `fd` -- `fcntl(F_SETFD)`, and every call that creates a
    /// descriptor with its own close-on-exec flag. See [`Filesystem::is_close_on_exec`].
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold.
    pub fn set_close_on_exec(&self, fd: i32, on: bool) -> FsResult<()> {
        let mut table = self.table();
        if !table.open.contains_key(&fd) {
            return Err(bad_fd("set_close_on_exec", fd));
        }
        if on {
            table.close_on_exec.insert(fd);
        } else {
            table.close_on_exec.remove(&fd);
        }
        Ok(())
    }

    /// Set or clear `O_NONBLOCK` on `fd`.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold, and
    /// [`FsError::Refused`] for one that cannot carry the flag. **Refused rather than ignored**:
    /// a caller that set `O_NONBLOCK` on a regular file and was told it worked would believe a
    /// read could report `EAGAIN`, and this seam would never produce one.
    pub fn set_nonblocking(&self, fd: i32, nonblocking: bool) -> FsResult<()> {
        const OP: &str = "set_nonblocking";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::Pipe(handle)) => {
                handle.set_nonblocking(nonblocking);
                Ok(())
            }
            Some(Entry::EventFd(counter)) => {
                counter.set_nonblocking(nonblocking);
                Ok(())
            }
            Some(Entry::TimerFd(timer)) => {
                timer.set_nonblocking(nonblocking);
                Ok(())
            }
            // **The host is told, not just this table.** A pipe's flag is a field this process
            // reads on every operation; a socket's is `ioctl(FIONBIO)` on a kernel object, and a
            // flag recorded here but never set there would make the guest's `fcntl(F_SETFL,
            // O_NONBLOCK)` read back as done while every `recv` still blocked.
            Some(Entry::Socket(socket)) => socket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_nonblocking(nonblocking)
                .map_err(|error| {
                    FsError::refused(
                        OP,
                        format!("fd {fd}"),
                        format!("the host refused to change the socket's blocking mode: {error}"),
                    )
                }),
            // A file, a directory, a device and a standard stream all answer `Readiness::ALWAYS`,
            // so `O_NONBLOCK` on one is a request with nothing to change. Linux accepts it there;
            // this seam does not pretend to, because accepting it would be this layer claiming a
            // behaviour it cannot produce.
            Some(_) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "O_NONBLOCK is only meaningful on a descriptor that can block, and the only kind \
                 here that can is a pipe",
            )),
        }
    }

    /// Take or release a POSIX (`fcntl`) record lock on `fd` -- **granted, because in this
    /// runtime nothing can hold a lock that conflicts with it**.
    ///
    /// # Why granting is the lock and not a stub of one
    ///
    /// A POSIX record lock belongs to a **process**, and a process's own locks never conflict
    /// with each other: a new lock over a range it already holds replaces the old one, and
    /// `F_GETLK` reports only a lock "that would prevent this lock from being created", which
    /// the caller's own cannot be (POSIX `fcntl`, "Record locking"). So the only thing that can
    /// refuse a lock is *another process* holding a conflicting one, and the only thing a lock
    /// table is for is deciding that.
    ///
    /// Every descriptor here was opened under this instance's root, which is per instance
    /// (D30 (3)), and an instance is one OS process (`ARCHITECTURE.md` section 7). No other
    /// process locks these files through this seam, so the kernel's answer to every lock this
    /// guest can ask for is success, and a table recording them would never be consulted.
    ///
    /// **What would falsify it**, so it can be checked rather than trusted: an embedding that
    /// points two *processes* at one root. They would each be told they hold an exclusive lock,
    /// and this would need host locks -- which on Windows are mandatory and per handle, so they
    /// are not POSIX locks either, and are not a drop-in.
    ///
    /// MEASURED reader: the engine's embedded SQLite, `unixFileLock` at `libroblox.so` link
    /// `0x22d72a4`, taking `F_SETLK` on SQLite's `PENDING_BYTE` (`0x40000000`, one byte) from
    /// `0x22d70d0` on a guest worker -- which was killed by the refusal this replaces.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor this instance does not hold, and for a
    /// [`RecordLock::Shared`] on one not open for reading or a [`RecordLock::Exclusive`] on one
    /// not open for writing -- POSIX's own `EBADF` for exactly those two. [`FsError::Refused`]
    /// for anything but a regular file: Linux locks pipes and sockets too, and no run has asked.
    pub fn record_lock(&self, fd: i32, lock: RecordLock) -> FsResult<()> {
        const OP: &str = "record_lock";
        let table = self.table();
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::File { readable, writable, .. }) => match lock {
                RecordLock::Shared if !*readable => Err(FsError::kinded(
                    OP,
                    format!("fd {fd}"),
                    FsErrorKind::BadDescriptor,
                    "a shared (F_RDLCK) lock needs a descriptor open for reading",
                )),
                RecordLock::Exclusive if !*writable => Err(FsError::kinded(
                    OP,
                    format!("fd {fd}"),
                    FsErrorKind::BadDescriptor,
                    "an exclusive (F_WRLCK) lock needs a descriptor open for writing",
                )),
                RecordLock::Shared | RecordLock::Exclusive | RecordLock::Release => Ok(()),
            },
            Some(_) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "a record lock on a descriptor that is not a regular file: Linux allows one, and \
                 no run has reached it, so it is refused by name rather than granted unmeasured",
            )),
        }
    }

    /// The readiness generation, read **before** testing readiness so that a change arriving
    /// between the test and the wait cannot be missed.
    #[must_use]
    pub fn ready_generation(&self) -> u64 {
        self.gate.generation()
    }

    /// Wait until some pipe in this instance changes state, or until `timeout` elapses.
    ///
    /// Returns whether anything changed. `seen` is a generation from
    /// [`ready_generation`](Self::ready_generation) taken before the caller tested readiness.
    ///
    /// **The caller supplies the bound and there is no overload that does not.** D16's
    /// runaway-guest defence is built from step budgets a sleeping thread does not consume, so how
    /// long a guest may block is the adapter's policy, not this seam's.
    pub fn wait_for_readiness(&self, seen: u64, timeout: Duration) -> bool {
        self.gate.wait(seen, timeout)
    }

    /// How many descriptors this instance holds, standard streams included.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.table().open.len()
    }

    /// How many directory streams this instance holds.
    #[must_use]
    pub fn dir_count(&self) -> usize {
        self.table().dirs.len()
    }

    /// `read(2)`: read into `buf`, returning how many bytes arrived.
    ///
    /// A short read is not an error and zero means end of file, which is `read`'s own contract.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`], or [`FsErrorKind::IsADirectory`] for a descriptor opened
    /// with `O_DIRECTORY`.
    pub fn read(&self, fd: i32, buf: &mut [u8]) -> FsResult<usize> {
        const OP: &str = "read";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::Standard(StdStream::In)) => Ok(0),
            Some(Entry::Standard(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::BadDescriptor,
                "the standard output streams are not open for reading",
            )),
            Some(Entry::Directory { guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::IsADirectory,
                "a directory descriptor cannot be read; `readdir` is the call for that",
            )),
            Some(Entry::Device(device)) => match device {
                // **A short read is not modelled and must not be**: a modern `/dev/urandom`
                // fills the whole buffer, and `std::random_device` reads four bytes at a time
                // without a loop. A partial fill would leave the rest of the caller's buffer
                // holding whatever was there, which is the believable-wrong-answer shape.
                Device::Random => {
                    crate::process::random_bytes(buf).map_err(|error| {
                        FsError::refused(
                            OP,
                            "/dev/urandom",
                            format!(
                                "the host entropy source failed: {error}. Filling the buffer from \
                                 a pseudo-random sequence would satisfy every test that checked \
                                 the bytes had changed and would not be entropy"
                            ),
                        )
                    })?;
                    Ok(buf.len())
                }
                Device::Null => Ok(0),
                Device::Zero => {
                    buf.fill(0);
                    Ok(buf.len())
                }
            },
            // The table lock is held across the generation, as it is across a pipe operation, and
            // for the same reason it is safe: a generator reads facts and takes no lock of this
            // seam's, so the order is one edge -- the table, then whatever the generator reads.
            Some(Entry::Generated(file)) => file.read(buf),
            // **Holding the table lock across a pipe operation is safe because nothing in `pipe`
            // waits.** The lock order is one edge — the table, then that pipe's own state, then
            // the ready gate — and no path takes them the other way round: a thread waiting for
            // readiness holds only the gate. See `pipe`'s module documentation for why the wait
            // belongs to the caller and not to this seam.
            Some(Entry::Pipe(handle)) => handle.read(buf),
            // As the pipe above: the table lock is held, and nothing in `eventfd` waits.
            // Linux: an epoll descriptor has no `read`, and asking is `EINVAL`.
            Some(Entry::Epoll(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "an epoll descriptor cannot be read or written (EINVAL)",
            )),
            Some(Entry::EventFd(counter)) => counter.read(buf),
            Some(Entry::TimerFd(timer)) => timer.read(buf, crate::clock::monotonic_now()),
            // **`read` on a socket is `recv`, and it is refused here rather than performed, for
            // one reason: the failure would lose its classification on the way out.**
            //
            // A socket fails in kinds this seam has no spelling for — `ECONNRESET`,
            // `ECONNREFUSED`, `ETIMEDOUT`, `ENOTCONN` — and [`FsErrorKind`] is deliberately small
            // and describes files. Performing the `recv` here would mean either flattening those
            // onto the nearest file errno, which is the plausible-wrong-answer shape Global
            // Constraint 1 forbids (a guest told `EPIPE` for a reset connection retries the wrong
            // thing), or widening a *filesystem* error type with a dozen network kinds that every
            // other operation on this seam can never produce.
            //
            // The second reason is about waiting rather than failing, and it is what settles it:
            // a blocking pipe read is waited out on [`Filesystem::wait_for_readiness`], and that
            // gate **never rises for a socket** — nothing in this process changes a socket's
            // state. A caller that reached a socket through this path would therefore wait its
            // whole budget on a descriptor that was ready the moment it started.
            //
            // So the socket path is [`Filesystem::socket_at`] plus
            // [`Socket::recv`](crate::net::Socket::recv), which keeps the [`NetError`] the
            // adapter turns into the guest's errno. Reachable from this public API by any caller
            // that has a socket's number, which is why it is a refusal and not a `debug_assert`.
            //
            // [`NetError`]: crate::net::NetError
            Some(Entry::Socket(_)) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "fd is a socket. `read` on one is `recv`, and this seam does not perform it: a \
                 socket fails with ECONNRESET, ECONNREFUSED, ETIMEDOUT and ENOTCONN, none of \
                 which FsErrorKind can express, and a blocking socket read cannot be waited out \
                 on this instance's readiness gate because nothing in this process ever raises it \
                 for a socket. Take the socket with `Filesystem::socket_at` and call \
                 `omni_platform::net::Socket::recv`, which keeps the NetError kind the caller \
                 needs to report",
            )),
            Some(Entry::File { file, readable, guest, .. }) => {
                if !*readable {
                    return Err(FsError::kinded(
                        OP,
                        guest.clone(),
                        FsErrorKind::BadDescriptor,
                        "the descriptor was not opened for reading",
                    ));
                }
                let guest = guest.clone();
                file.read(buf).map_err(|e| FsError::io(OP, &guest, &e))
            }
        }
    }

    /// `pread(2)`: read at an absolute offset **without moving the descriptor's own offset**.
    ///
    /// The one operation here that has no single portable `std` spelling, and therefore the one
    /// that has a backend. See the module table.
    ///
    /// # Errors
    ///
    /// [`FsError::Unsupported`] on Linux and macOS, naming `pread(2)`.
    pub fn pread(&self, fd: i32, buf: &mut [u8], offset: u64) -> FsResult<usize> {
        const OP: &str = "pread";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            // A character device has no offset to read at. Linux answers `ESPIPE` for a `pread`
            // on one, which is the same answer it gives for a pipe and is what the caller's own
            // fallback to `read` branches on.
            Some(Entry::Device(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a character device has no offset, so there is nothing to read *at*",
            )),
            // What the engine does with `/proc/meminfo` and `/proc/self/statm`: keep the
            // descriptor and `pread` it at 0 for each new reading. See `GeneratedFile` for why a
            // read at 0 generates and a read that continues does not.
            Some(Entry::Generated(file)) => file.read_at(buf, offset),
            Some(Entry::Standard(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a standard stream is a pipe: it has no offset to read at (ESPIPE)",
            )),
            // `ESPIPE` for real this time, and for the reason the arm above borrows: a pipe is a
            // queue and there is no position in it to read from.
            // An eventfd is a counter, not a stream: there is no position in it either, and its
            // read is destructive, so a `pread` that "did not move the offset" would still consume
            // the counter. `ESPIPE` is what Linux answers.
            Some(Entry::EventFd(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "an eventfd is a counter with no offset to read at (ESPIPE), and its read                  consumes what it returns",
            )),
            Some(Entry::TimerFd(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::NotSeekable,
                "a timerfd has no offset (ESPIPE)",
            )),
            Some(Entry::Epoll(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::NotSeekable,
                "an epoll descriptor has no offset (ESPIPE)",
            )),
            Some(Entry::Pipe(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a pipe has no offset to read at (ESPIPE)",
            )),
            // A socket is a stream of packets the network delivered, not a stored object with
            // positions in it: there is nothing at offset *n* to read, and Linux answers `ESPIPE`
            // here exactly as it does for a pipe. This arm is a refusal for the same reason as
            // the pipe's — and *not* the refusal `read` and `write` give, because that one is
            // about a classification this seam cannot carry, while this one is about an operation
            // that does not exist on a socket at all.
            Some(Entry::Socket(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a socket has no offset to read at (ESPIPE): what arrives is what the network \
                 delivered, in the order it delivered it",
            )),
            Some(Entry::Directory { guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::IsADirectory,
                "a directory descriptor cannot be read",
            )),
            Some(Entry::File { file, readable, guest, .. }) => {
                if !*readable {
                    return Err(FsError::kinded(
                        OP,
                        guest.clone(),
                        FsErrorKind::BadDescriptor,
                        "the descriptor was not opened for reading",
                    ));
                }
                let guest = guest.clone();
                backend::pread(file, buf, offset).map_err(|error| match error {
                    FsError::Io { kind, detail, .. } => FsError::Io {
                        operation: OP,
                        path: guest,
                        kind,
                        detail,
                    },
                    other => other,
                })
            }
        }
    }

    /// A regular file's bytes from `offset`, for an `mmap` of it: `buf` filled as far as the file
    /// goes, and how far that was.
    ///
    /// Linux's answers for what cannot be mapped (`do_mmap`): no such descriptor is `EBADF`, and a
    /// file not open for reading is `EACCES` -- not `pread`'s `EBADF`, which is why this is its own
    /// operation. A descriptor that is not a regular file is refused by name: Linux maps some
    /// devices and answers `ENODEV` for the rest, and no run has asked for either.
    ///
    /// # Errors
    ///
    /// As above, and the host's own read failure as it reports it.
    pub fn read_for_mapping(&self, fd: i32, buf: &mut [u8], offset: u64) -> FsResult<usize> {
        const OP: &str = "mmap";
        let table = self.table();
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::File { readable: false, guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::PermissionDenied,
                "a file mapping of a descriptor not open for reading (EACCES)",
            )),
            Some(Entry::File { file, guest, .. }) => {
                let mut filled = 0usize;
                while filled < buf.len() {
                    let read = backend::pread(file, &mut buf[filled..], offset + filled as u64)
                        .map_err(|error| match error {
                            FsError::Io { kind, detail, .. } => {
                                FsError::Io { operation: OP, path: guest.clone(), kind, detail }
                            }
                            other => other,
                        })?;
                    if read == 0 {
                        break;
                    }
                    filled += read;
                }
                Ok(filled)
            }
            Some(_) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "a mapping of a descriptor that is not a regular file: Linux maps some devices \
                 and answers ENODEV for everything else, and no run has asked for either",
            )),
        }
    }

    /// A duplicate of a regular file's host handle, for a **shared writable** `mmap` of it: the
    /// handle [`vm::share_file_for_mapping`](crate::vm::share_file_for_mapping) builds the section
    /// over, so that the mapping writes the very file the descriptor names.
    ///
    /// A duplicate, not the descriptor itself, because the mapping outlives the descriptor:
    /// MEASURED, the engine's `MappedFile` (`libroblox.so` link `0x2273210`) closes its descriptor
    /// on the instruction after `mmap` returns and goes on writing through the mapping.
    ///
    /// Linux's answers (`do_mmap`), in its order: no such descriptor is `EBADF`; a file not open
    /// for reading is `EACCES`; and a `MAP_SHARED` mapping with `PROT_WRITE` of a file not open for
    /// writing is `EACCES` too -- which is why a descriptor opened `O_RDONLY` fails here although
    /// [`read_for_mapping`](Self::read_for_mapping) would take it. Anything that is not a regular
    /// file is refused by name, as there.
    ///
    /// # Errors
    ///
    /// As above, and the host's own failure to duplicate the handle as it reports it.
    pub fn share_for_mapping(&self, fd: i32) -> FsResult<File> {
        const OP: &str = "mmap";
        let table = self.table();
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::File { readable: false, guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::PermissionDenied,
                "a file mapping of a descriptor not open for reading (EACCES)",
            )),
            Some(Entry::File { writable: false, guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::PermissionDenied,
                "a MAP_SHARED mapping with PROT_WRITE of a descriptor not open for writing \
                 (EACCES)",
            )),
            Some(Entry::File { file, guest, .. }) => {
                file.try_clone().map_err(|error| FsError::io(OP, guest, &error))
            }
            Some(_) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "a mapping of a descriptor that is not a regular file: Linux maps some devices \
                 and answers ENODEV for everything else, and no run has asked for either",
            )),
        }
    }

    /// `fsync(2)`: push a regular file's data and metadata to the device.
    ///
    /// `File::sync_all`, portable `std` -- `FlushFileBuffers` on Windows, `fsync` on unix -- so no
    /// backend. A descriptor with nothing to synchronise (pipe, socket, eventfd, timerfd, epoll,
    /// standard stream, device) is `EINVAL`, which is Linux's answer for "a special file which
    /// does not support synchronization".
    ///
    /// # Errors
    ///
    /// As above; `EBADF` for no such descriptor; and [`FsError::Refused`] for a **directory**:
    /// Linux syncs one (SQLite does, after creating a journal), `std` has no directory handle to
    /// flush, and no run has reached it yet -- so it refuses by name rather than answering.
    pub fn fsync(&self, fd: i32) -> FsResult<()> {
        const OP: &str = "fsync";
        let table = self.table();
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::File { file, guest, .. }) => {
                file.sync_all().map_err(|error| FsError::io(OP, guest, &error))
            }
            Some(Entry::Directory { guest, .. }) => Err(FsError::refused(
                OP,
                guest.clone(),
                "fsync on a directory: Linux supports it and `std` has no directory handle to \
                 flush; no run has reached it, so it is refused by name rather than answered",
            )),
            // A generated file is in this group because `/proc` files have no `fsync` operation,
            // and `vfs_fsync_range` answers `EINVAL` for a file without one.
            Some(
                Entry::Standard(_)
                | Entry::Device(_)
                | Entry::Generated(_)
                | Entry::Pipe(_)
                | Entry::EventFd(_)
                | Entry::Socket(_)
                | Entry::Epoll(_)
                | Entry::TimerFd(_),
            ) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "the descriptor has nothing to synchronise (EINVAL)",
            )),
        }
    }

    /// `ftruncate(2)`: make a regular file exactly `length` bytes long.
    ///
    /// `File::set_len`, portable `std`. Linux's order and answers (`do_sys_ftruncate`): a negative
    /// length is `EINVAL` (the caller's check, since this takes a `u64`), no such descriptor is
    /// `EBADF`, and anything that is not a regular file **open for writing** is `EINVAL`.
    ///
    /// # Errors
    ///
    /// As above.
    pub fn ftruncate(&self, fd: i32, length: u64) -> FsResult<()> {
        const OP: &str = "ftruncate";
        let table = self.table();
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::File { file, writable: true, guest, .. }) => {
                file.set_len(length).map_err(|error| FsError::io(OP, guest, &error))
            }
            Some(_) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "not a regular file open for writing (EINVAL)",
            )),
        }
    }

    /// `fallocate(2)` with mode 0, which is what `posix_fallocate` is on bionic: make sure the
    /// bytes `offset..offset + len` of a regular file are allocated, extending it when the range
    /// ends past it and never shortening it.
    ///
    /// Linux's order (`vfs_fallocate`), after the caller's `EINVAL` for a negative offset or a
    /// length that is not positive: no such descriptor, or one not open for writing, is `EBADF`;
    /// a pipe or socket is `ESPIPE`; a directory is `EISDIR`; anything else that is not a regular
    /// file is `ENODEV` -- refused by name here, since this seam has no kind for it and no run
    /// has asked; an end past `i64::MAX` is `EFBIG`. What "allocated" takes is the
    /// backend's -- see each one's `allocate`.
    ///
    /// # Errors
    ///
    /// As above, and the host's own failure (`ENOSPC` for a full volume) as it reports it.
    pub fn fallocate(&self, fd: i32, offset: u64, len: u64) -> FsResult<()> {
        const OP: &str = "fallocate";
        let table = self.table();
        let refuse = |kind: FsErrorKind, why: &'static str| {
            Err(FsError::kinded(OP, format!("fd {fd}"), kind, why))
        };
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            // A generated file is never open for writing, and `vfs_fallocate` checks that first.
            Some(Entry::File { writable: false, .. } | Entry::Generated(_)) => {
                refuse(FsErrorKind::BadDescriptor, "not open for writing (EBADF)")
            }
            Some(Entry::File { file, .. }) => {
                let Some(end) = offset.checked_add(len).filter(|end| i64::try_from(*end).is_ok())
                else {
                    return refuse(FsErrorKind::FileTooLarge, "the range ends past i64::MAX (EFBIG)");
                };
                backend::allocate(file, end)
            }
            Some(Entry::Pipe(_) | Entry::Socket(_)) => {
                refuse(FsErrorKind::NotSeekable, "a pipe or socket (ESPIPE)")
            }
            Some(Entry::Directory { .. }) => refuse(FsErrorKind::IsADirectory, "a directory (EISDIR)"),
            Some(
                Entry::Standard(_)
                | Entry::Device(_)
                | Entry::EventFd(_)
                | Entry::Epoll(_)
                | Entry::TimerFd(_),
            ) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "fallocate on a descriptor that is neither a regular file, a pipe, a socket nor a                  directory: Linux answers ENODEV, which this seam has no kind for, and no run has                  reached it -- so it is refused by name rather than answered",
            )),
        }
    }

    /// `lseek(2)`: move the descriptor's offset and return where it now is.
    ///
    /// Portable `std` -- `Seek` for `&File` -- so no backend. `whence` is Linux's numbering
    /// (`SEEK_SET` 0, `SEEK_CUR` 1, `SEEK_END` 2); anything else, and a position that would be
    /// negative, is `EINVAL`, which is what Linux answers and what `std` reports as `InvalidInput`.
    /// A descriptor with no position -- pipe, socket, eventfd, standard stream -- is `ESPIPE`
    /// ([`FsErrorKind::NotSeekable`]).
    ///
    /// # Errors
    ///
    /// As above; [`FsErrorKind::BadDescriptor`] for no such descriptor, and [`FsError::Refused`]
    /// for a directory or a device: Linux seeks both, and no run has asked.
    pub fn seek(&self, fd: i32, offset: i64, whence: i32) -> FsResult<u64> {
        use std::io::{Seek, SeekFrom};
        const OP: &str = "lseek";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            // `seq_lseek`'s rules -- see `GeneratedFile::seek`. Moving back to 0 is how a reader
            // that uses `read` rather than `pread` asks for a new generation.
            Some(Entry::Generated(file)) => file.seek(offset, whence),
            Some(
                Entry::Pipe(_)
                | Entry::Socket(_)
                | Entry::EventFd(_)
                | Entry::Standard(_)
                | Entry::Epoll(_)
                | Entry::TimerFd(_),
            ) => {
                Err(FsError::kinded(
                    OP,
                    format!("fd {fd}"),
                    FsErrorKind::NotSeekable,
                    "the descriptor has no position to move (ESPIPE)",
                ))
            }
            Some(Entry::Directory { .. } | Entry::Device(_)) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "a seek on a directory or a device: Linux allows both, and no run has reached \
                 one, so it is refused by name rather than answered unmeasured",
            )),
            Some(Entry::File { file, guest, .. }) => {
                // Shared borrows: the table is taken mutably only for the generated arm above.
                let (file, guest): (&File, &String) = (file, guest);
                // **Resolved to an absolute position here, and the host only ever sees
                // `SEEK_SET`.** MEASURED: `SEEK_CUR` to a negative position reaches Windows as
                // `ERROR_NEGATIVE_SEEK`, which `std` does not classify, so the guest would have been
                // refused where Linux answers `EINVAL`. Doing the arithmetic here -- checked,
                // because every term is the guest's -- makes the answer the same on every host.
                let mut handle: &File = file;
                let io = |error: std::io::Error| FsError::io(OP, guest, &error);
                let invalid = |why: &'static str| {
                    FsError::kinded(OP, guest.clone(), FsErrorKind::InvalidInput, why)
                };
                let base: i64 = match whence {
                    0 => 0,
                    1 => i64::try_from(handle.stream_position().map_err(io)?)
                        .map_err(|_| invalid("the current offset does not fit an off_t"))?,
                    2 => i64::try_from(file.metadata().map_err(io)?.len())
                        .map_err(|_| invalid("the file's size does not fit an off_t"))?,
                    _ => return Err(invalid("whence is none of SEEK_SET, SEEK_CUR and SEEK_END")),
                };
                let target = base
                    .checked_add(offset)
                    .ok_or_else(|| invalid("the resulting offset overflows an off_t"))?;
                let target =
                    u64::try_from(target).map_err(|_| invalid("the resulting offset is negative"))?;
                handle.seek(SeekFrom::Start(target)).map_err(io)
            }
        }
    }

    /// `pwrite(2)`: write at an absolute offset **without moving the descriptor's own offset**.
    ///
    /// [`pread`](Filesystem::pread)'s mirror, arm for arm: a descriptor with no position in it
    /// answers `ESPIPE` (as `InvalidInput`), a directory `EISDIR`, and a file not open for writing
    /// `EBADF`. Added when the engine's SQLite reached it.
    ///
    /// # Errors
    ///
    /// As above, and [`FsError::Unsupported`] on Linux and macOS, naming `pwrite(2)`.
    pub fn pwrite(&self, fd: i32, buf: &[u8], offset: u64) -> FsResult<usize> {
        const OP: &str = "pwrite";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::Device(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a character device has no offset, so there is nothing to write *at* (ESPIPE)",
            )),
            Some(Entry::Generated(file)) => Err(FsError::kinded(
                OP,
                file.guest.clone(),
                FsErrorKind::BadDescriptor,
                "a generated file is never open for writing",
            )),
            Some(Entry::Standard(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a standard stream is a pipe: it has no offset to write at (ESPIPE)",
            )),
            Some(Entry::EventFd(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "an eventfd is a counter with no offset to write at (ESPIPE)",
            )),
            Some(Entry::TimerFd(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::NotSeekable,
                "a timerfd has no offset (ESPIPE)",
            )),
            Some(Entry::Epoll(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::NotSeekable,
                "an epoll descriptor has no offset (ESPIPE)",
            )),
            Some(Entry::Pipe(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a pipe has no offset to write at (ESPIPE)",
            )),
            Some(Entry::Socket(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a socket has no offset to write at (ESPIPE)",
            )),
            Some(Entry::Directory { guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::IsADirectory,
                "a directory descriptor cannot be written",
            )),
            Some(Entry::File { file, writable, guest, .. }) => {
                if !*writable {
                    return Err(FsError::kinded(
                        OP,
                        guest.clone(),
                        FsErrorKind::BadDescriptor,
                        "the descriptor was not opened for writing",
                    ));
                }
                let guest = guest.clone();
                backend::pwrite(file, buf, offset).map_err(|error| match error {
                    FsError::Io { kind, detail, .. } => FsError::Io {
                        operation: OP,
                        path: guest,
                        kind,
                        detail,
                    },
                    other => other,
                })
            }
        }
    }

    /// `write(2)`: write `buf`, returning how many bytes were taken.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor that is not open, or is not open for
    /// writing.
    pub fn write(&self, fd: i32, buf: &[u8]) -> FsResult<usize> {
        const OP: &str = "write";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            // Every device here accepts everything and keeps none of it, which is what the three
            // of them do on a device: writing to `/dev/null` and `/dev/zero` discards, and
            // writing to `/dev/urandom` stirs the kernel's pool and consumes the bytes. The
            // whole buffer is taken, because a short write here would be invented.
            Some(Entry::Device(_)) => Ok(buf.len()),
            // `open` refuses a generated file for writing, so a descriptor on one is read-only
            // and a write is `EBADF`, as it is on any descriptor opened `O_RDONLY`.
            Some(Entry::Generated(file)) => Err(FsError::kinded(
                OP,
                file.guest.clone(),
                FsErrorKind::BadDescriptor,
                "a generated file is never open for writing",
            )),
            Some(Entry::Standard(StdStream::In)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::BadDescriptor,
                "the standard input stream is not open for writing",
            )),
            Some(Entry::Standard(stream)) => {
                let stream = *stream;
                write_standard(stream, buf)
            }
            Some(Entry::Directory { guest, .. }) => Err(FsError::kinded(
                OP,
                guest.clone(),
                FsErrorKind::IsADirectory,
                "a directory descriptor cannot be written",
            )),
            // See the note in `read`: one lock order, and nothing in `pipe` waits.
            Some(Entry::Pipe(handle)) => handle.write(buf),
            // Linux: an epoll descriptor has no `write`, and asking is `EINVAL`.
            Some(Entry::Epoll(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "an epoll descriptor cannot be read or written (EINVAL)",
            )),
            Some(Entry::EventFd(counter)) => counter.write(buf),
            // Linux: a timerfd is armed with `timerfd_settime`, and `write` on one is `EINVAL`.
            Some(Entry::TimerFd(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a timerfd cannot be written (EINVAL); it is armed with timerfd_settime",
            )),
            // `write` on a socket is `send`, and it is refused here for the two reasons the
            // `read` arm gives at length: the failure kinds do not survive the trip through
            // `FsErrorKind`, and a blocking send cannot be waited out on a gate nothing raises
            // for a socket. `Filesystem::socket_at` plus `Socket::send` is the route.
            Some(Entry::Socket(_)) => Err(FsError::refused(
                OP,
                format!("fd {fd}"),
                "fd is a socket. `write` on one is `send`; take the socket with \
                 `Filesystem::socket_at` and call `omni_platform::net::Socket::send`, which \
                 keeps the NetError kind — see this seam's `read` for why performing it here \
                 would flatten ECONNRESET and EPIPE onto the same answer",
            )),
            Some(Entry::File { file, writable, guest, .. }) => {
                if !*writable {
                    return Err(FsError::kinded(
                        OP,
                        guest.clone(),
                        FsErrorKind::BadDescriptor,
                        "the descriptor was not opened for writing",
                    ));
                }
                let guest = guest.clone();
                file.write(buf).map_err(|e| FsError::io(OP, &guest, &e))
            }
        }
    }

    /// Push anything this layer is holding for `fd` at the operating system.
    ///
    /// **This is `fflush`, not `fsync`, and the difference is the whole of why it is a no-op for a
    /// file.** C's `fflush` promises that the *stream's* buffer has reached the OS; it promises
    /// nothing about the disk, which is `fsync`'s job. Every write on this seam goes straight to
    /// the host with no buffer of ours in between, so that promise is already kept before the call
    /// is made. Returning success is therefore the contract being satisfied rather than a stub —
    /// and calling `sync_all` here instead would be a *stronger* guarantee than `fflush` makes,
    /// bought with a disk round trip per call.
    ///
    /// The standard streams are flushed for real, because [`std::io::Stdout`] does have a buffer.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor that is not open.
    pub fn flush(&self, fd: i32) -> FsResult<()> {
        const OP: &str = "flush";
        let mut table = self.table();
        match table.open.get_mut(&fd) {
            None => Err(bad_fd(OP, fd)),
            Some(Entry::Standard(StdStream::Out)) => std::io::stdout()
                .flush()
                .map_err(|e| FsError::io(OP, "stdout", &e)),
            Some(Entry::Standard(StdStream::Err)) => std::io::stderr()
                .flush()
                .map_err(|e| FsError::io(OP, "stderr", &e)),
            Some(_) => Ok(()),
        }
    }

    // ---------------------------------------------------------------- metadata

    /// `stat(2)`: describe what a path names, following a final symlink — which this layer
    /// refuses to do, so a symlink anywhere in the path is a confinement refusal.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), plus [`FsError::Io`] for a host failure.
    pub fn stat(&self, guest_path: &[u8]) -> FsResult<FileStat> {
        const OP: &str = "stat";
        if let Some(device) = device_for(guest_path) {
            return Ok(device_stat(device));
        }
        if let Some((name, _)) = self.generated_for(guest_path) {
            return Ok(generated_stat(&name));
        }
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let metadata = std::fs::metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        Ok(describe(&host, &metadata))
    }

    /// `lstat(2)`: describe what a path names **without** following a final symlink.
    ///
    /// The one call that may end on a link, because describing one is its entire purpose. It does
    /// not read the link's target, so nothing here leaves the root.
    ///
    /// # Errors
    ///
    /// As [`stat`](Self::stat).
    pub fn lstat(&self, guest_path: &[u8]) -> FsResult<FileStat> {
        const OP: &str = "lstat";
        if let Some(device) = device_for(guest_path) {
            // A device node is not a symbolic link, so `lstat` and `stat` agree about it.
            return Ok(device_stat(device));
        }
        if let Some((name, _)) = self.generated_for(guest_path) {
            // Nor is a generated file.
            return Ok(generated_stat(&name));
        }
        let host = self.resolve(OP, guest_path, FinalLink::Describe)?;
        let metadata = std::fs::symlink_metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        Ok(describe(&host, &metadata))
    }

    /// `fstat(2)`: describe what a descriptor is open on.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a descriptor that is not open.
    pub fn fstat(&self, fd: i32) -> FsResult<FileStat> {
        const OP: &str = "fstat";
        let table = self.table();
        match table.open.get(&fd) {
            None => Err(bad_fd(OP, fd)),
            // As a standard stream: a character device, with no size and no times. The
            // identity is the device's own path, so two descriptors on `/dev/urandom` compare
            // equal and one on `/dev/null` does not — which is what `st_rdev` gives on a device.
            Some(Entry::Device(device)) => Ok(FileStat {
                kind: FileKind::Other,
                size: 0,
                read_only: false,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new(
                    DEVICES
                        .iter()
                        .find(|(_, candidate)| candidate == device)
                        .map_or("/dev", |(name, _)| *name),
                )),
            }),
            Some(Entry::Generated(file)) => Ok(generated_stat(&file.guest)),
            // A standard stream is a character device, which is what a real `fstat` on one
            // reports. Nothing is invented: there is no size, no time and no path.
            Some(Entry::Standard(_)) => Ok(FileStat {
                kind: FileKind::Other,
                size: 0,
                read_only: false,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new(match fd {
                    STDIN_FD => "/dev/stdin",
                    STDOUT_FD => "/dev/stdout",
                    _ => "/dev/stderr",
                })),
            }),
            // A FIFO, which is what `S_ISFIFO` tests for and what `fstat` on a pipe reports on a
            // device. **The size is the bytes currently buffered**, which is what Linux puts in
            // `st_size` for a pipe — not zero, and not the capacity. The identity distinguishes
            // the two ends, because they are two descriptions of one object and a guest that
            // compared them is entitled to see that they differ.
            // **`st_size` is zero and the counter is not reported.** An eventfd has no size on
            // Linux, and putting the counter here would make `fstat` a second, non-destructive
            // way to read it -- which is a behaviour a device does not have and which a guest
            // that found it would be entitled to rely on.
            // An anonymous inode, as the eventfd arm below, and for the same reason nothing about
            // the interest list is reported: `fstat` is not how Linux describes one.
            Some(Entry::TimerFd(_)) => Ok(FileStat {
                kind: FileKind::Other,
                size: 0,
                read_only: false,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new("/proc/self/fd/anon_inode:[timerfd]")),
            }),
            Some(Entry::Epoll(_)) => Ok(FileStat {
                kind: FileKind::Other,
                size: 0,
                read_only: false,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new("/proc/self/fd/anon_inode:[eventpoll]")),
            }),
            Some(Entry::EventFd(_)) => Ok(FileStat {
                kind: FileKind::Other,
                size: 0,
                read_only: false,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new("/proc/self/fd/anon_inode:[eventfd]")),
            }),
            // **Zero size, and the peer is not described.** Linux's `fstat` on a socket reports
            // `S_IFSOCK` with `st_size` 0 and no times; the address, the peer and the connection
            // state are `getsockname`, `getpeername` and `getsockopt(SO_ERROR)`, and none of them
            // is a question `fstat` answers. Putting the receive queue's depth in `st_size` — the
            // way the pipe arm below legitimately does — would be inventing a number Linux does
            // not put there, and `FIONREAD` is the call that does answer it.
            //
            // The identity distinguishes a socket from every other anonymous descriptor kind and
            // does **not** distinguish two sockets from each other, which is the same limit the
            // eventfd arm has: `identity` is derived from a path and two sockets have none.
            Some(Entry::Socket(_)) => Ok(FileStat {
                kind: FileKind::Other,
                size: 0,
                read_only: false,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new("/proc/self/fd/socket")),
            }),
            Some(Entry::Pipe(handle)) => Ok(FileStat {
                kind: FileKind::Other,
                size: handle.pipe().buffered() as u64,
                read_only: handle.end() == PipeEnd::Read,
                accessed: None,
                modified: None,
                created: None,
                identity: identity(Path::new(match handle.end() {
                    PipeEnd::Read => "/proc/self/fd/pipe:read",
                    PipeEnd::Write => "/proc/self/fd/pipe:write",
                })),
            }),
            Some(Entry::Directory { host, .. }) => {
                let metadata = std::fs::metadata(host).map_err(|e| FsError::io(OP, host, &e))?;
                Ok(describe(host, &metadata))
            }
            Some(Entry::File { file, host, .. }) => {
                let metadata = file.metadata().map_err(|e| FsError::io(OP, host, &e))?;
                Ok(describe(host, &metadata))
            }
        }
    }

    /// `access(2)`.
    ///
    /// # What is answered and what is refused
    ///
    /// `F_OK` is existence, which is a fact. `R_OK` and `W_OK` are answered by **probing** — by
    /// asking the host to open the file the way the guest asks about — because that is the only
    /// answer that is not a guess: Windows' ACLs are not POSIX mode bits, and the read-only
    /// attribute `std` exposes is not the write permission.
    ///
    /// `X_OK` is **not** here, and the adapter refuses it by name. There is no execute permission
    /// on a Windows file to report, the read-only attribute says nothing about it, and both
    /// available answers are believable and wrong: `0` tells the guest it may execute a file this
    /// runtime cannot execute at all, and `-1`/`EACCES` reports a policy decision nobody made.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::NotFound`] when the path does not exist and [`FsErrorKind::PermissionDenied`]
    /// when the probe was refused — which is `access`'s own contract.
    pub fn access(&self, guest_path: &[u8], check: AccessCheck) -> FsResult<()> {
        const OP: &str = "access";
        // A generated file exists and is readable, and is not writable: `0444`, as `open` says.
        if let Some((name, _)) = self.generated_for(guest_path) {
            return match check {
                AccessCheck::Exists | AccessCheck::Readable => Ok(()),
                AccessCheck::Writable => Err(FsError::kinded(
                    OP,
                    name,
                    FsErrorKind::PermissionDenied,
                    "a generated file is read-only (mode 0444)",
                )),
            };
        }
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let metadata = std::fs::metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        match check {
            AccessCheck::Exists => Ok(()),
            AccessCheck::Readable => {
                if metadata.is_dir() {
                    // A directory is readable if it can be listed, which is the probe that
                    // corresponds to `R_OK` on one.
                    std::fs::read_dir(&host).map(|_| ()).map_err(|e| FsError::io(OP, &host, &e))
                } else {
                    File::open(&host).map(|_| ()).map_err(|e| FsError::io(OP, &host, &e))
                }
            }
            AccessCheck::Writable => {
                if metadata.permissions().readonly() {
                    return Err(FsError::kinded(
                        OP,
                        host.display().to_string(),
                        FsErrorKind::PermissionDenied,
                        "the host's read-only attribute is set",
                    ));
                }
                if metadata.is_dir() {
                    // There is no portable probe for "may I create an entry in this directory"
                    // that does not create one, so the read-only attribute is the whole answer
                    // for a directory and it has already been checked.
                    Ok(())
                } else {
                    // `write(true)` without `truncate` or `create` opens the existing file and
                    // changes nothing about it.
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&host)
                        .map(|_| ())
                        .map_err(|e| FsError::io(OP, &host, &e))
                }
            }
        }
    }

    /// `statvfs(3)`: what the volume holding a path reports about itself.
    ///
    /// # Errors
    ///
    /// [`FsError::Unsupported`] on Linux and macOS, naming `statvfs(3)`.
    pub fn statvfs(&self, guest_path: &[u8]) -> FsResult<VolumeStats> {
        const OP: &str = "statvfs";
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        // The path must exist: `statvfs` is defined on a path, and answering for the root when
        // the guest named something that is not there would describe a different volume in
        // principle and hide the guest's own mistake in practice.
        if !host.exists() {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::NotFound,
                "statvfs is defined on a path that exists",
            ));
        }
        backend::volume_stats(&host)
    }

    // ---------------------------------------------------------------- the namespace

    /// `rename(2)`. Both paths are confined independently.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve) for either path, plus [`FsError::Io`].
    pub fn rename(&self, from: &[u8], to: &[u8]) -> FsResult<()> {
        const OP: &str = "rename";
        let old = self.resolve(OP, from, FinalLink::Refuse)?;
        let new = self.resolve(OP, to, FinalLink::Refuse)?;
        std::fs::rename(&old, &new).map_err(|e| FsError::io(OP, &old, &e))
    }

    /// `unlink(2)`.
    ///
    /// A directory is `EISDIR` rather than removed: `unlink` on a directory is not `rmdir`, and
    /// `std::fs::remove_file` on Windows does not make the distinction the way POSIX does.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), plus [`FsError::Io`].
    pub fn unlink(&self, guest_path: &[u8]) -> FsResult<()> {
        const OP: &str = "unlink";
        // `Describe`: unlinking a symlink removes the link, and POSIX is explicit that `unlink`
        // never follows one. Refusing here would make a link inside the root impossible to
        // remove, which is the over-correction rather than the defence.
        let host = self.resolve(OP, guest_path, FinalLink::Describe)?;
        let metadata =
            std::fs::symlink_metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        if metadata.is_dir() {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::IsADirectory,
                "unlink does not remove directories; rmdir does",
            ));
        }
        std::fs::remove_file(&host).map_err(|e| FsError::io(OP, &host, &e))
    }

    /// `utime(2)`: set a **regular file's** access and modification times.
    ///
    /// Through `std::fs::File::set_times`, on the file opened for writing -- which is the access
    /// that carries the right to change its attributes, on Windows as on POSIX. A directory is
    /// refused by name rather than guessed at: its times need a handle this seam does not open,
    /// and no run has asked for one. MEASURED reader: the engine's HTTP cache, on a second
    /// launch of a kept data directory.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), [`FsError::Io`] for a file that cannot be opened for
    /// writing or stamped, and [`FsErrorKind::Other`] for a directory.
    pub fn set_times(
        &self,
        guest_path: &[u8],
        accessed: std::time::SystemTime,
        modified: std::time::SystemTime,
    ) -> FsResult<()> {
        const OP: &str = "utime";
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let metadata = std::fs::metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        if metadata.is_dir() {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::Other,
                "a directory's times are not set by this seam -- no run has asked for one",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&host)
            .map_err(|e| FsError::io(OP, &host, &e))?;
        file.set_times(std::fs::FileTimes::new().set_accessed(accessed).set_modified(modified))
            .map_err(|e| FsError::io(OP, &host, &e))
    }

    /// `mkdir(2)`. One directory, never the parents: POSIX's `mkdir` does not create a path.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), plus [`FsError::Io`].
    pub fn mkdir(&self, guest_path: &[u8]) -> FsResult<()> {
        const OP: &str = "mkdir";
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        std::fs::create_dir(&host).map_err(|e| FsError::io(OP, &host, &e))
    }

    /// `rmdir(2)`. Empty directories only, which is `rmdir`'s contract.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), plus [`FsError::Io`] — [`FsErrorKind::DirectoryNotEmpty`]
    /// for a directory that still has entries.
    pub fn rmdir(&self, guest_path: &[u8]) -> FsResult<()> {
        const OP: &str = "rmdir";
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let metadata = std::fs::metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        if !metadata.is_dir() {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::NotADirectory,
                "rmdir removes directories only",
            ));
        }
        std::fs::remove_dir(&host).map_err(|e| FsError::io(OP, &host, &e))
    }

    // ---------------------------------------------------------------- directories

    /// `opendir(3)`: snapshot a directory and return a handle to walk it.
    ///
    /// # Why a snapshot rather than a live iterator
    ///
    /// POSIX leaves the effect of modifying a directory during a walk unspecified, so a snapshot
    /// is a conforming reading — and it is the one that cannot hold a host directory handle open
    /// for as long as the guest keeps the `DIR *`, which matters when the guest chooses how long
    /// that is. [`MAX_DIR_ENTRIES`] bounds the allocation, and a directory past it is refused by
    /// name rather than truncated.
    ///
    /// # `.` and `..` are synthesised, and they are facts
    ///
    /// `std::fs::read_dir` does not yield them and every POSIX directory has them. Code that
    /// walks a directory skips them by name, and code that counts entries expects them, so
    /// omitting them would be a wrong answer in both directions. They are the first two entries,
    /// which is their conventional order.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), plus [`FsError::Io`] and [`FsError::Refused`] for a
    /// directory too large or holding a name that cannot be represented.
    pub fn opendir(&self, guest_path: &[u8]) -> FsResult<i32> {
        const OP: &str = "opendir";
        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let shown = path::resolve_lexically(OP, guest_path)?.guest_path();
        let metadata = std::fs::metadata(&host).map_err(|e| FsError::io(OP, &host, &e))?;
        if !metadata.is_dir() {
            return Err(FsError::kinded(
                OP,
                shown,
                FsErrorKind::NotADirectory,
                "opendir names a directory",
            ));
        }
        let mut table = self.table();
        if table.dirs.len() >= MAX_OPEN_DIRS {
            return Err(FsError::kinded(
                OP,
                shown,
                FsErrorKind::TooManyOpenFiles,
                format!("this guest instance already holds {MAX_OPEN_DIRS} directory streams"),
            ));
        }
        let parent = host.parent().unwrap_or(&host).to_path_buf();
        let mut entries = vec![
            DirEntryInfo {
                name: ".".to_string(),
                kind: FileKind::Directory,
                identity: identity(&host),
            },
            DirEntryInfo {
                name: "..".to_string(),
                kind: FileKind::Directory,
                identity: identity(&parent),
            },
        ];
        for entry in std::fs::read_dir(&host).map_err(|e| FsError::io(OP, &host, &e))? {
            let entry = entry.map_err(|e| FsError::io(OP, &host, &e))?;
            if entries.len() >= MAX_DIR_ENTRIES {
                return Err(FsError::refused(
                    OP,
                    shown,
                    format!(
                        "the directory holds more than {MAX_DIR_ENTRIES} entries. A truncated \
                         listing is a believable wrong answer -- code walking it to find a file \
                         would report the file missing -- so this refuses instead"
                    ),
                ));
            }
            let name = entry.file_name();
            let Some(host_name) = name.to_str() else {
                return Err(FsError::refused(
                    OP,
                    shown,
                    format!(
                        "the entry `{}` is not valid Unicode, so it has no byte form to hand the \
                         guest. Skipping it would silently hide a file from a directory walk",
                        name.to_string_lossy()
                    ),
                ));
            };
            // As the guest wrote it: a stand-in back to the character it stores (`path`'s
            // `STORED_AS_STAND_IN`), before the guest's own `d_name` bound is applied to it.
            let name = path::guest_component(host_name);
            let name = name.as_str();
            if name.len() > NAME_MAX {
                return Err(FsError::refused(
                    OP,
                    shown,
                    format!(
                        "the entry `{name}` is {} bytes and the guest's `struct dirent` holds a \
                         256-byte `d_name`, so it cannot be returned without truncating it",
                        name.len()
                    ),
                ));
            }
            let kind = entry
                .file_type()
                .map(|t| FileKind::of(&t))
                .unwrap_or(FileKind::Other);
            entries.push(DirEntryInfo {
                name: name.to_string(),
                kind,
                identity: identity(&entry.path()),
            });
        }
        let id = table.next_dir;
        table.next_dir = table.next_dir.saturating_add(1);
        table.dirs.insert(id, DirStream { entries, position: 0, guest: shown });
        Ok(id)
    }

    /// `readdir(3)`: the next entry of a directory stream, or `None` at its end.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a handle this instance does not hold.
    pub fn readdir(&self, dir: i32) -> FsResult<Option<DirEntryInfo>> {
        let mut table = self.table();
        let Some(stream) = table.dirs.get_mut(&dir) else {
            return Err(bad_fd("readdir", dir));
        };
        let entry = stream.entries.get(stream.position).cloned();
        if entry.is_some() {
            stream.position += 1;
        }
        Ok(entry)
    }

    /// How far through a directory stream the guest is, for a test that wants to see it.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a handle this instance does not hold.
    pub fn dir_position(&self, dir: i32) -> FsResult<(usize, usize)> {
        let table = self.table();
        let Some(stream) = table.dirs.get(&dir) else {
            return Err(bad_fd("readdir", dir));
        };
        Ok((stream.position, stream.entries.len()))
    }

    /// The guest path a directory stream was opened on.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a handle this instance does not hold.
    pub fn dir_path(&self, dir: i32) -> FsResult<String> {
        let table = self.table();
        table
            .dirs
            .get(&dir)
            .map(|stream| stream.guest.clone())
            .ok_or_else(|| bad_fd("readdir", dir))
    }

    /// `closedir(3)`.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] for a handle this instance does not hold.
    pub fn closedir(&self, dir: i32) -> FsResult<()> {
        let mut table = self.table();
        match table.dirs.remove(&dir) {
            Some(_) => Ok(()),
            None => Err(bad_fd("closedir", dir)),
        }
    }
}

impl Table {
    /// The lowest descriptor not in use, which is what POSIX's `open` promises.
    ///
    /// Lowest-free rather than a monotonic counter, and the difference is observable: guest code
    /// that closes a descriptor and opens another expects to get the same number back, and
    /// `dup2`-shaped idioms depend on it. The scan is over at most [`MAX_OPEN_FILES`] entries.
    fn lowest_free_fd(&self) -> i32 {
        let mut candidate = FIRST_FD;
        while self.open.contains_key(&candidate) {
            candidate += 1;
        }
        candidate
    }
}

/// Write to one of the host's standard streams.
///
/// The guest's standard output goes to the host's, because there is nothing else it could mean
/// and because the engine's own diagnostics are worth having during a run of 3,594 initializers.
/// It is not the log sink: [`crate::log`] is where a *log record* with a priority and a tag goes,
/// and a `write(1, …)` is bytes with neither.
fn write_standard(stream: StdStream, buf: &[u8]) -> FsResult<usize> {
    let result = match stream {
        StdStream::Out => std::io::stdout().write(buf),
        StdStream::Err => std::io::stderr().write(buf),
        StdStream::In => unreachable!("the caller has already rejected a write to stdin"),
    };
    result.map_err(|e| FsError::io("write", "a standard stream", &e))
}

/// Turn a host `Metadata` into the facts this seam reports.
fn describe(host: &Path, metadata: &std::fs::Metadata) -> FileStat {
    FileStat {
        kind: FileKind::of(&metadata.file_type()),
        size: metadata.len(),
        read_only: metadata.permissions().readonly(),
        accessed: since_epoch(metadata.accessed().ok()),
        modified: since_epoch(metadata.modified().ok()),
        created: since_epoch(metadata.created().ok()),
        identity: identity(host),
    }
}

/// A host timestamp as a duration since the Unix epoch.
///
/// A time before 1970 is reported as `None` rather than as a negative duration, which is the same
/// reading [`crate::clock::realtime_now`] gives: there is no other answer that is a `Duration`,
/// and the caller's own answer for `None` is a zero timestamp rather than a wrong one.
fn since_epoch(time: Option<SystemTime>) -> Option<Duration> {
    time.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
}

/// What a POSIX record lock asks for, which is all [`Filesystem::record_lock`] decides about one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLock {
    /// `F_RDLCK`: a shared lock, which needs the descriptor open for reading.
    Shared,
    /// `F_WRLCK`: an exclusive lock, which needs it open for writing.
    Exclusive,
    /// `F_UNLCK`: a release, which needs neither.
    Release,
}

/// The `EBADF` every descriptor operation shares.
fn bad_fd(operation: &'static str, fd: i32) -> FsError {
    FsError::kinded(
        operation,
        format!("fd {fd}"),
        FsErrorKind::BadDescriptor,
        "this guest instance holds no such descriptor",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself, built without a dependency.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let mut at = std::env::temp_dir();
            at.push(format!(
                "omni-fs-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&at);
            std::fs::create_dir_all(&at).expect("a scratch directory");
            Scratch(at)
        }

        fn fs(&self) -> Filesystem {
            Filesystem::new(&self.0).expect("a filesystem over the scratch root")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_flags() -> OpenFlags {
        OpenFlags { write: true, create: true, truncate: true, ..OpenFlags::default() }
    }

    fn read_flags() -> OpenFlags {
        OpenFlags { read: true, ..OpenFlags::default() }
    }

    /// **`set_times` stamps the file's times**, read back from the host's own metadata; a
    /// directory is refused and a missing file is `NotFound`.
    #[test]
    fn set_times_stamps_a_file_and_refuses_a_directory() {
        let scratch = Scratch::new("settimes");
        let fs = scratch.fs();
        std::fs::write(scratch.0.join("f"), b"x").expect("a file");
        std::fs::create_dir(scratch.0.join("d")).expect("a directory");
        let at = |seconds: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        fs.set_times(b"/f", at(1_000_000_000), at(1_234_567_890)).expect("stamped");
        let modified = std::fs::metadata(scratch.0.join("f")).and_then(|m| m.modified()).expect("mtime");
        assert_eq!(modified, at(1_234_567_890));
        let error = fs.set_times(b"/d", at(1), at(1)).expect_err("a directory");
        assert_eq!(error.kind(), Some(FsErrorKind::Other), "{error}");
        let error = fs.set_times(b"/missing", at(1), at(1)).expect_err("no such file");
        assert_eq!(error.kind(), Some(FsErrorKind::NotFound), "{error}");
    }

    /// **A descriptor names the path it was opened by**, and only while it is held: two files
    /// opened side by side each answer their own, a standard stream answers none, and a closed
    /// descriptor answers none.
    #[test]
    fn a_descriptor_names_the_guest_path_it_was_opened_by() {
        let scratch = Scratch::new("pathof");
        let fs = scratch.fs();
        fs.mkdir(b"/data").expect("mkdir");
        let first = fs.open(b"/data/first.db", write_flags()).expect("open");
        let second = fs.open(b"/data/second.db-shm", write_flags()).expect("open");
        assert_eq!(fs.guest_path_of(first).as_deref(), Some("/data/first.db"));
        assert_eq!(fs.guest_path_of(second).as_deref(), Some("/data/second.db-shm"));
        assert_eq!(fs.guest_path_of(1), None, "stdout names no path");
        fs.close(first).expect("close");
        assert_eq!(fs.guest_path_of(first), None, "a closed descriptor names nothing");
    }

    /// A round trip through the seam: create, write, close, reopen, read back the same bytes.
    #[test]
    fn a_file_written_through_the_seam_reads_back_byte_for_byte() {
        let scratch = Scratch::new("roundtrip");
        let fs = scratch.fs();
        let fd = fs.open(b"/hello.txt", write_flags()).expect("open for writing");
        assert!(fd >= FIRST_FD, "fd {fd} collides with a standard stream");
        assert_eq!(fs.write(fd, b"the quick brown fox").expect("write"), 19);
        fs.close(fd).expect("close");
        assert!(!fs.is_open(fd), "the descriptor is gone after close");

        let fd = fs.open(b"/hello.txt", read_flags()).expect("open for reading");
        let mut buf = [0u8; 32];
        let read = fs.read(fd, &mut buf).expect("read");
        assert_eq!(&buf[..read], b"the quick brown fox");
        assert_eq!(fs.read(fd, &mut buf).expect("read at the end"), 0, "end of file is zero");
        fs.close(fd).expect("close");
        // Closing twice is EBADF, not a silent success.
        assert_eq!(fs.close(fd).unwrap_err().kind(), Some(FsErrorKind::BadDescriptor));
    }

    /// **The confinement property, end to end.** No guest path reaches a host file outside the
    /// root, whatever shape it is written in.
    ///
    /// The file it tries to read is one this test creates *outside* the root, so a traversal that
    /// worked would be visible as content rather than as an absence.
    #[test]
    fn no_guest_path_can_reach_a_file_outside_the_root() {
        let outer = Scratch::new("outer");
        std::fs::write(outer.0.join("secret.txt"), b"HOST SECRET").expect("the bait");
        let inner = outer.0.join("root");
        std::fs::create_dir_all(&inner).expect("the guest root");
        let fs = Filesystem::new(&inner).expect("a filesystem");

        for attempt in [
            "/../secret.txt",
            "/../../secret.txt",
            "../secret.txt",
            "/a/../../secret.txt",
            "/./../secret.txt",
            "//../secret.txt",
            r"/..\secret.txt",
            "/a/b/c/../../../../secret.txt",
        ] {
            let result = fs.open(attempt.as_bytes(), read_flags());
            match result {
                Err(FsError::Confined { .. }) => {}
                Err(FsError::Io { kind: FsErrorKind::NotFound, .. }) => {
                    // The lexical rules clamped it at the root, so it named a file inside the
                    // root that does not exist. That is the defence working, not a gap.
                }
                other => panic!("`{attempt}` produced {other:?} rather than being confined"),
            }
        }
        // And the same for every other path-taking operation, because a defence applied in one
        // place is the shape of every published traversal bug.
        assert!(fs.stat(b"/../secret.txt").is_err());
        assert!(fs.lstat(b"/../secret.txt").is_err());
        assert!(fs.unlink(b"/../secret.txt").is_err());
        assert!(fs.rename(b"/../secret.txt", b"/x").is_err());
        assert!(fs.rename(b"/x", b"/../secret.txt").is_err());
        assert!(fs.access(b"/../secret.txt", AccessCheck::Exists).is_err());
        // `mkdir` and `rmdir` are the two that must be checked by their *effect* rather than by
        // their return, because a climb they absorbed lands on a legitimate name inside the root
        // and legitimately succeeds. What must never happen is a directory appearing beside the
        // bait, so that is what is asserted.
        let _ = fs.mkdir(b"/../stolen");
        let _ = fs.mkdir(b"/../../stolen");
        assert!(
            !outer.0.join("stolen").exists(),
            "a guest mkdir created a directory outside its root"
        );
        assert!(
            inner.join("stolen").exists(),
            "and the climb was absorbed rather than refused, landing inside the root"
        );
        let _ = fs.rmdir(b"/..");
        assert!(inner.is_dir(), "a guest rmdir removed its own root");
        // The bait is still there and was never read or removed.
        assert_eq!(
            std::fs::read(outer.0.join("secret.txt")).expect("the bait survives"),
            b"HOST SECRET"
        );
    }

    /// A symlink out of the root is refused, and `lstat` still describes it.
    ///
    /// Skipped when the host will not create a symlink — on Windows that needs Developer Mode or
    /// an elevated process — because a test that silently passes by not testing anything is worse
    /// than one that says it did not run.
    #[test]
    fn a_symlink_is_refused_everywhere_except_lstat() {
        let outer = Scratch::new("symlink");
        std::fs::write(outer.0.join("secret.txt"), b"HOST SECRET").expect("the bait");
        let inner = outer.0.join("root");
        std::fs::create_dir_all(&inner).expect("the guest root");
        let link = inner.join("escape");
        #[cfg(target_os = "windows")]
        let made = std::os::windows::fs::symlink_file(outer.0.join("secret.txt"), &link).is_ok();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outer.0.join("secret.txt"), &link).is_ok();
        // A file symlink needs SeCreateSymbolicLinkPrivilege, which an unelevated Windows session
        // without Developer Mode does not have -- MEASURED on this host: `WinError 1314`. That made
        // this test SILENTLY SKIP, so rules 5 and 6 had never executed here at all, on the machine
        // whose green suite was the evidence for them. A directory JUNCTION needs no privilege and
        // is a reparse point that Rust's `is_symlink` reports true for, so it exercises the same
        // branch. Falling back to one is what makes this test a test on this host.
        #[cfg(target_os = "windows")]
        let made = made || {
            let target = outer.0.join("linked");
            std::fs::create_dir_all(&target).expect("the junction target");
            std::fs::write(target.join("secret.txt"), b"HOST SECRET").expect("the bait");
            std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !made {
            panic!(
                "this host can create neither a symbolic link nor a directory junction, so rules 5                  and 6 cannot be exercised. Failing loudly rather than skipping: a silent skip is                  how these two rules came to have no coverage at all."
            );
        }
        let fs = Filesystem::new(&inner).expect("a filesystem");
        let error = fs.open(b"/escape", read_flags()).expect_err("a symlink is not followed");
        assert!(matches!(error, FsError::Confined { .. }), "{error}");
        let error = fs.stat(b"/escape").expect_err("stat follows, so it is refused");
        assert!(matches!(error, FsError::Confined { .. }), "{error}");
        // `lstat` describes the link itself and never reads its target.
        let described = fs.lstat(b"/escape").expect("lstat describes a link");
        assert_eq!(described.kind, FileKind::Symlink);
    }

    /// `st_ino` is stable, distinct per path, and never zero.
    ///
    /// **The failure this exists for is a constant.** A zero or shared identity makes every
    /// `(st_dev, st_ino)` comparison answer "the same file", which is the worst wrong answer
    /// available from a `stat`.
    #[test]
    fn the_identity_is_stable_distinct_and_never_zero() {
        let a = identity(Path::new("/data/one"));
        let b = identity(Path::new("/data/two"));
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b, "two paths must not share an inode number");
        assert_eq!(a, identity(Path::new("/data/one")), "and it must be stable across calls");
    }

    /// The descriptor table: the standard three are open from the start, and the numbering is
    /// lowest-free.
    #[test]
    fn the_standard_streams_are_open_and_the_numbering_is_lowest_free() {
        let scratch = Scratch::new("fds");
        let fs = scratch.fs();
        for fd in [STDIN_FD, STDOUT_FD, STDERR_FD] {
            assert!(fs.is_open(fd), "fd {fd} must be open before anything else is");
        }
        assert_eq!(fs.open_count(), 3);
        let a = fs.open(b"/a", write_flags()).expect("a");
        let b = fs.open(b"/b", write_flags()).expect("b");
        assert_eq!((a, b), (3, 4));
        fs.close(a).expect("close a");
        let c = fs.open(b"/c", write_flags()).expect("c");
        assert_eq!(c, 3, "the lowest free descriptor comes back, as POSIX requires");
        // Reading a standard stream is end of file; writing to stdin is EBADF.
        let mut buf = [0u8; 4];
        assert_eq!(fs.read(STDIN_FD, &mut buf).expect("stdin reads EOF"), 0);
        assert_eq!(
            fs.write(STDIN_FD, b"x").unwrap_err().kind(),
            Some(FsErrorKind::BadDescriptor)
        );
    }

    /// The descriptor ceiling is `EMFILE` and not a panic, and it is a real bound.
    #[test]
    fn a_guest_that_leaks_descriptors_is_stopped_with_emfile() {
        let scratch = Scratch::new("emfile");
        let fs = scratch.fs();
        let mut opened = 0usize;
        loop {
            match fs.open(format!("/f{opened}").as_bytes(), write_flags()) {
                Ok(_) => opened += 1,
                Err(error) => {
                    assert_eq!(error.kind(), Some(FsErrorKind::TooManyOpenFiles), "{error}");
                    break;
                }
            }
            assert!(opened < MAX_OPEN_FILES + 8, "the ceiling never fired");
        }
        assert_eq!(opened, MAX_OPEN_FILES - 3, "three of the {MAX_OPEN_FILES} are the standard streams");
        assert_eq!(fs.open_count(), MAX_OPEN_FILES);
    }

    /// The namespace operations, including the distinctions `std::fs` does not make for us.
    #[test]
    fn the_namespace_operations_keep_posixs_own_distinctions() {
        let scratch = Scratch::new("namespace");
        let fs = scratch.fs();
        fs.mkdir(b"/dir").expect("mkdir");
        assert_eq!(
            fs.mkdir(b"/dir").unwrap_err().kind(),
            Some(FsErrorKind::AlreadyExists),
            "a second mkdir is EEXIST"
        );
        assert_eq!(
            fs.mkdir(b"/missing/deep").unwrap_err().kind(),
            Some(FsErrorKind::NotFound),
            "mkdir does not create parents"
        );
        let fd = fs.open(b"/dir/file", write_flags()).expect("a file in it");
        fs.write(fd, b"body").expect("write");
        fs.close(fd).expect("close");
        // unlink refuses a directory; rmdir refuses a file.
        assert_eq!(fs.unlink(b"/dir").unwrap_err().kind(), Some(FsErrorKind::IsADirectory));
        assert_eq!(fs.rmdir(b"/dir/file").unwrap_err().kind(), Some(FsErrorKind::NotADirectory));
        assert_eq!(
            fs.rmdir(b"/dir").unwrap_err().kind(),
            Some(FsErrorKind::DirectoryNotEmpty),
            "rmdir removes empty directories only"
        );
        fs.rename(b"/dir/file", b"/moved").expect("rename");
        assert_eq!(fs.stat(b"/moved").expect("the new name").size, 4);
        assert_eq!(fs.stat(b"/dir/file").unwrap_err().kind(), Some(FsErrorKind::NotFound));
        fs.rmdir(b"/dir").expect("now empty");
        fs.unlink(b"/moved").expect("unlink");
        assert_eq!(fs.unlink(b"/moved").unwrap_err().kind(), Some(FsErrorKind::NotFound));
    }

    /// `stat` reports the size and kind exactly, and `access` answers what it can.
    #[test]
    fn stat_and_access_report_facts() {
        let scratch = Scratch::new("stat");
        let fs = scratch.fs();
        let fd = fs.open(b"/f", write_flags()).expect("open");
        fs.write(fd, &[7u8; 1234]).expect("write");
        let by_fd = fs.fstat(fd).expect("fstat");
        fs.close(fd).expect("close");
        let by_path = fs.stat(b"/f").expect("stat");
        assert_eq!(by_path.kind, FileKind::Regular);
        assert_eq!(by_path.size, 1234);
        assert_eq!(by_fd.identity, by_path.identity, "one file, one identity");
        assert_eq!(by_fd.size, by_path.size);
        fs.mkdir(b"/d").expect("mkdir");
        assert_eq!(fs.stat(b"/d").expect("stat a directory").kind, FileKind::Directory);
        fs.access(b"/f", AccessCheck::Exists).expect("F_OK on a file that exists");
        fs.access(b"/f", AccessCheck::Readable).expect("R_OK on a readable file");
        fs.access(b"/f", AccessCheck::Writable).expect("W_OK on a writable file");
        fs.access(b"/d", AccessCheck::Readable).expect("R_OK on a listable directory");
        assert_eq!(
            fs.access(b"/nope", AccessCheck::Exists).unwrap_err().kind(),
            Some(FsErrorKind::NotFound)
        );
        assert_eq!(fs.fstat(999).unwrap_err().kind(), Some(FsErrorKind::BadDescriptor));
    }

    /// **A name with a character Windows reserves is created, found and listed as the guest wrote
    /// it**, and the host file carries the stand-in.
    ///
    /// MEASURED: the engine's content cache is named after URLs --
    /// `ContentProvider_…/rbxthumb://type=AvatarHeadShot&…` -- an ordinary path on a device.
    #[test]
    fn a_name_with_a_character_windows_reserves_round_trips_through_the_seam() {
        let scratch = Scratch::new("standin");
        let fs = scratch.fs();
        fs.mkdir(b"/cache").expect("mkdir");
        // Before it exists, the answer a device gives: not found, not refused.
        assert_eq!(
            fs.stat(b"/cache/rbxthumb://type=a&w=48").unwrap_err().kind(),
            Some(FsErrorKind::NotFound)
        );
        fs.mkdir(b"/cache/rbxthumb:").expect("a directory named with a colon");
        let path: &[u8] = b"/cache/rbxthumb://type=a&w=48?x*y";
        let fd = fs.open(path, write_flags()).expect("a file named with `?` and `*`");
        assert_eq!(fs.write(fd, b"thumb").expect("write"), 5);
        fs.close(fd).expect("close");
        assert!(fs.stat(path).is_ok(), "found again by the same name");
        let dir = fs.opendir(b"/cache").expect("opendir");
        let mut names = Vec::new();
        while let Some(entry) = fs.readdir(dir).expect("readdir") {
            names.push(entry.name);
        }
        fs.closedir(dir).expect("closedir");
        assert!(names.contains(&"rbxthumb:".to_string()), "listed as written: {names:?}");
        // On the host: the stand-ins, so no reserved character reached a host call.
        let host = scratch.0.join("cache").join("rbxthumb\u{F03A}").join("type=a&w=48\u{F03F}x\u{F02A}y");
        assert!(host.is_file(), "{} is the host file", host.display());
    }

    /// A directory stream yields `.`, `..` and then the entries, once each, and then nothing.
    #[test]
    fn a_directory_stream_yields_dot_dotdot_and_then_every_entry_once() {
        let scratch = Scratch::new("dir");
        let fs = scratch.fs();
        fs.mkdir(b"/d").expect("mkdir");
        for name in ["a", "b", "c"] {
            let fd = fs.open(format!("/d/{name}").as_bytes(), write_flags()).expect("a file");
            fs.close(fd).expect("close");
        }
        fs.mkdir(b"/d/sub").expect("a subdirectory");
        let dir = fs.opendir(b"/d").expect("opendir");
        let mut names = Vec::new();
        while let Some(entry) = fs.readdir(dir).expect("readdir") {
            names.push((entry.name, entry.kind));
        }
        assert_eq!(names[0], (".".to_string(), FileKind::Directory));
        assert_eq!(names[1], ("..".to_string(), FileKind::Directory));
        let mut rest: Vec<_> = names[2..].to_vec();
        rest.sort();
        assert_eq!(
            rest,
            vec![
                ("a".to_string(), FileKind::Regular),
                ("b".to_string(), FileKind::Regular),
                ("c".to_string(), FileKind::Regular),
                ("sub".to_string(), FileKind::Directory),
            ]
        );
        // Past the end it stays `None` rather than wrapping.
        assert_eq!(fs.readdir(dir).expect("past the end"), None);
        assert_eq!(fs.readdir(dir).expect("still past the end"), None);
        assert_eq!(fs.dir_position(dir).expect("position"), (6, 6));
        fs.closedir(dir).expect("closedir");
        assert_eq!(fs.closedir(dir).unwrap_err().kind(), Some(FsErrorKind::BadDescriptor));
        assert_eq!(
            fs.opendir(b"/nope").unwrap_err().kind(),
            Some(FsErrorKind::NotFound),
            "opendir on a path that is not there"
        );
    }

    /// Two directory streams are independent, which is what a per-`DIR` position means.
    #[test]
    fn two_directory_streams_over_one_directory_do_not_share_a_position() {
        let scratch = Scratch::new("twodirs");
        let fs = scratch.fs();
        fs.mkdir(b"/d").expect("mkdir");
        let first = fs.opendir(b"/d").expect("first");
        let second = fs.opendir(b"/d").expect("second");
        assert_ne!(first, second);
        assert_eq!(fs.readdir(first).expect("a").map(|e| e.name), Some(".".to_string()));
        assert_eq!(
            fs.readdir(second).expect("b").map(|e| e.name),
            Some(".".to_string()),
            "the second stream starts at the beginning, not where the first one is"
        );
        assert_eq!(fs.dir_count(), 2);
        fs.closedir(first).expect("close");
        fs.closedir(second).expect("close");
        assert_eq!(fs.dir_count(), 0);
    }

    /// `seek` moves the offset the next `read` uses, answers where it went, refuses a negative
    /// position with `InvalidInput`, and a pipe with `NotSeekable`.
    #[test]
    fn seek_moves_the_offset_the_next_read_uses() {
        let scratch = Scratch::new("seek");
        let fs = scratch.fs();
        let fd = fs.open(b"/f", write_flags()).expect("open");
        fs.write(fd, b"0123456789").expect("write");
        fs.close(fd).expect("close");
        let fd = fs.open(b"/f", read_flags()).expect("reopen");
        assert_eq!(fs.seek(fd, 6, 0).expect("SEEK_SET"), 6);
        let mut two = [0u8; 2];
        assert_eq!(fs.read(fd, &mut two).expect("read"), 2);
        assert_eq!(&two, b"67", "the read starts where the seek put it");
        assert_eq!(fs.seek(fd, 0, 1).expect("SEEK_CUR 0 is ftell"), 8);
        assert_eq!(fs.seek(fd, -3, 2).expect("SEEK_END"), 7);
        assert_eq!(fs.seek(fd, -1, 0).unwrap_err().kind(), Some(FsErrorKind::InvalidInput));
        assert_eq!(fs.seek(fd, -100, 1).unwrap_err().kind(), Some(FsErrorKind::InvalidInput));
        assert_eq!(fs.seek(fd, 0, 3).unwrap_err().kind(), Some(FsErrorKind::InvalidInput));
        assert_eq!(fs.seek(fd, 0, 1).expect("unchanged by the failures"), 7);
        let (read_end, _write_end) = fs.pipe().expect("a pipe");
        assert_eq!(fs.seek(read_end, 0, 1).unwrap_err().kind(), Some(FsErrorKind::NotSeekable));
    }

    /// `pwrite` writes at an offset and leaves the descriptor's own offset alone -- the
    /// `seek_write` half of `pread`'s measured defect -- or refuses by name with no backend.
    #[test]
    fn pwrite_does_not_move_the_descriptors_offset() {
        let scratch = Scratch::new("pwrite");
        let fs = scratch.fs();
        let fd = fs.open(b"/f", write_flags()).expect("open");
        fs.write(fd, b"0123456789").expect("write");
        match fs.pwrite(fd, b"ab", 2) {
            Ok(n) => {
                assert_eq!(n, 2);
                // The sequential offset is still 10, so this lands at the end, not after "ab".
                fs.write(fd, b"XY").expect("write");
                fs.close(fd).expect("close");
                let fd = fs.open(b"/f", read_flags()).expect("reopen");
                let mut all = [0u8; 16];
                let n = fs.read(fd, &mut all).expect("read");
                assert_eq!(&all[..n], b"01ab456789XY", "pwrite moved the descriptor's offset");
            }
            Err(error) => {
                assert!(matches!(error, FsError::Unsupported { .. }), "{error}");
                assert!(error.to_string().contains("pwrite(2)"), "{error}");
            }
        }
    }

    /// `pread` reads at an offset and leaves the descriptor's own offset alone — or refuses by
    /// name on a target with no backend.
    #[test]
    fn pread_does_not_move_the_descriptors_offset() {
        let scratch = Scratch::new("pread");
        let fs = scratch.fs();
        let fd = fs.open(b"/f", write_flags()).expect("open");
        fs.write(fd, b"0123456789").expect("write");
        fs.close(fd).expect("close");
        let fd = fs.open(b"/f", read_flags()).expect("reopen");
        let mut head = [0u8; 4];
        assert_eq!(fs.read(fd, &mut head).expect("read"), 4);
        assert_eq!(&head, b"0123");
        let mut at = [0u8; 3];
        match fs.pread(fd, &mut at, 7) {
            Ok(n) => {
                assert_eq!((&at[..n], n), (&b"789"[..], 3));
                // The sequential offset is untouched: the next read continues from 4.
                let mut next = [0u8; 3];
                assert_eq!(fs.read(fd, &mut next).expect("read"), 3);
                assert_eq!(&next, b"456", "pread moved the descriptor's own offset");
            }
            Err(error) => {
                assert!(error.is_unsupported(), "{error}");
                assert!(error.to_string().contains("pread(2)"), "{error}");
            }
        }
        fs.close(fd).expect("close");
    }

    /// `statvfs` answers with the host's own numbers, or refuses by name.
    ///
    /// **Structural rather than numeric**: the assertions are the relations that must hold for any
    /// volume, because a test that pinned a free-space figure would be asserting about this
    /// machine's disk. What it does catch is the failure that matters — a fabricated filesystem,
    /// which has no reason to satisfy `available <= free <= total` or to have a power-of-two block
    /// size.
    #[test]
    fn statvfs_answers_from_the_host_volume_or_refuses_by_name() {
        let scratch = Scratch::new("statvfs");
        let fs = scratch.fs();
        match fs.statvfs(b"/") {
            Ok(stats) => {
                assert!(stats.block_size > 0, "a zero block size divides by zero in guest code");
                assert!(
                    stats.block_size.is_power_of_two(),
                    "block size {} is not a power of two",
                    stats.block_size
                );
                assert!(stats.blocks > 0, "the volume holding the root has no blocks");
                assert!(stats.blocks_free <= stats.blocks, "more free blocks than blocks");
                assert!(
                    stats.blocks_available <= stats.blocks_free,
                    "more blocks available than free"
                );
                assert!(
                    (1..=NAME_MAX as u64 * 4).contains(&stats.name_max),
                    "name_max {} is not a component length",
                    stats.name_max
                );
            }
            Err(error) => {
                assert!(error.is_unsupported(), "{error}");
                assert!(error.to_string().contains("statvfs(3)"), "{error}");
            }
        }
        // It is defined on a path that exists.
        assert!(matches!(
            fs.statvfs(b"/nope"),
            Err(FsError::Io { kind: FsErrorKind::NotFound, .. }) | Err(FsError::Unsupported { .. })
        ));
    }

    /// The flag combinations that have a meaning, and the one that does not.
    #[test]
    fn the_open_flags_mean_what_they_say() {
        let scratch = Scratch::new("flags");
        let fs = scratch.fs();
        // Neither read nor write is EINVAL rather than a descriptor that can do nothing.
        assert_eq!(
            fs.open(b"/x", OpenFlags::default()).unwrap_err().kind(),
            Some(FsErrorKind::InvalidInput)
        );
        // O_CREAT|O_EXCL twice is EEXIST.
        let flags = OpenFlags { write: true, create: true, exclusive: true, ..OpenFlags::default() };
        let fd = fs.open(b"/x", flags).expect("first");
        fs.write(fd, b"first").expect("write");
        fs.close(fd).expect("close");
        assert_eq!(fs.open(b"/x", flags).unwrap_err().kind(), Some(FsErrorKind::AlreadyExists));
        // Without O_CREAT a missing file is ENOENT.
        assert_eq!(
            fs.open(b"/missing", read_flags()).unwrap_err().kind(),
            Some(FsErrorKind::NotFound)
        );
        // O_APPEND writes at the end whatever the offset is.
        let fd = fs
            .open(b"/x", OpenFlags { write: true, append: true, ..OpenFlags::default() })
            .expect("append");
        fs.write(fd, b"-second").expect("write");
        fs.close(fd).expect("close");
        assert_eq!(fs.stat(b"/x").expect("stat").size, 12);
        // O_TRUNC empties it.
        let fd = fs.open(b"/x", write_flags()).expect("truncate");
        fs.close(fd).expect("close");
        assert_eq!(fs.stat(b"/x").expect("stat").size, 0);
        // A descriptor opened for reading refuses a write, and the other way round.
        let fd = fs.open(b"/x", read_flags()).expect("read only");
        assert_eq!(fs.write(fd, b"no").unwrap_err().kind(), Some(FsErrorKind::BadDescriptor));
        fs.close(fd).expect("close");
        // O_DIRECTORY gives a descriptor that stats and nothing else.
        fs.mkdir(b"/d").expect("mkdir");
        let fd = fs
            .open(b"/d", OpenFlags { read: true, directory: true, ..OpenFlags::default() })
            .expect("O_DIRECTORY");
        assert_eq!(fs.fstat(fd).expect("fstat").kind, FileKind::Directory);
        let mut buf = [0u8; 4];
        assert_eq!(fs.read(fd, &mut buf).unwrap_err().kind(), Some(FsErrorKind::IsADirectory));
        fs.close(fd).expect("close");
        assert_eq!(
            fs.open(b"/x", OpenFlags { read: true, directory: true, ..OpenFlags::default() })
                .unwrap_err()
                .kind(),
            Some(FsErrorKind::NotADirectory)
        );
    }

    /// **`/dev/urandom` is a device, and it fills the whole buffer with entropy.**
    ///
    /// Three things at once, because each has a believable wrong answer next to it: the read
    /// returns the whole length (a short read would leave the rest of the caller's buffer
    /// holding whatever was there), it is not the same twice (a fixed pattern would satisfy any
    /// "it changed" assertion), and it is not all zero (which `/dev/zero` is, one entry along).
    #[test]
    fn dev_urandom_is_a_device_that_fills_the_buffer_with_entropy() {
        let scratch = Scratch::new("urandom");
        let fs = scratch.fs();
        let fd = fs.open(b"/dev/urandom", OpenFlags { read: true, ..OpenFlags::default() })
            .expect("a device descriptor");
        let mut first = [0u8; 64];
        let mut second = [0u8; 64];
        assert_eq!(fs.read(fd, &mut first).expect("a read"), first.len());
        assert_eq!(fs.read(fd, &mut second).expect("a read"), second.len());
        assert_ne!(first, second, "two reads of /dev/urandom must not agree");
        assert_ne!(first, [0u8; 64], "/dev/urandom is not /dev/zero");
        // `stat` describes it without a host path existing for it.
        assert_eq!(fs.stat(b"/dev/urandom").expect("stat").kind, FileKind::Other);
        fs.close(fd).expect("close");
    }

    /// `/dev/null`, `/dev/zero`, and the three writes that are accepted and kept by nobody.
    #[test]
    fn the_other_devices_answer_what_a_device_answers() {
        let scratch = Scratch::new("devices");
        let fs = scratch.fs();
        let read_write = OpenFlags { read: true, write: true, ..OpenFlags::default() };

        let null = fs.open(b"/dev/null", read_write).expect("/dev/null");
        let mut buf = [0xAAu8; 8];
        assert_eq!(fs.read(null, &mut buf).expect("a read"), 0, "/dev/null is end of file");
        assert_eq!(buf, [0xAAu8; 8], "and it writes nothing into the buffer");

        let zero = fs.open(b"/dev/zero", read_write).expect("/dev/zero");
        assert_eq!(fs.read(zero, &mut buf).expect("a read"), buf.len());
        assert_eq!(buf, [0u8; 8], "/dev/zero fills with zero");

        // Every device takes the whole write and keeps none of it, which is what the three of
        // them do on a device.
        for fd in [null, zero] {
            assert_eq!(fs.write(fd, b"discarded").expect("a write"), 9);
        }
        // A character device has no offset, so `pread` on one is not a short read: it is an error.
        assert!(fs.pread(zero, &mut buf, 0).is_err(), "a device has no offset to read at");
    }

    /// **The device list is closed**, and a path that merely looks like one is an ordinary path.
    ///
    /// The over-correction half: a rule that matched `/dev/` as a prefix would make every path
    /// under it a device, and `ENOENT` for a file that is not there is what the guest must get.
    #[test]
    fn a_path_that_is_not_one_of_the_four_devices_is_an_ordinary_path() {
        let scratch = Scratch::new("notdev");
        let fs = scratch.fs();
        assert!(device_for(b"/dev/urandom").is_some());
        for path in [
            &b"/dev/watchdog"[..],
            b"/dev/urandom2",
            b"/dev",
            b"/urandom",
            b"/proc/sys/vm/overcommit_memory",
        ] {
            assert!(
                device_for(path).is_none(),
                "`{}` is not one of the four devices this seam implements",
                path::display(path)
            );
        }
        assert_eq!(
            fs.open(b"/dev/watchdog", OpenFlags { read: true, ..OpenFlags::default() })
                .unwrap_err()
                .kind(),
            Some(FsErrorKind::NotFound),
            "a path that is not a device resolves under the root like any other"
        );
        // Spellings of the same device all reach it, because the path is normalised first.
        for path in [&b"/dev/./urandom"[..], b"/dev//urandom", b"/x/../dev/urandom"] {
            assert_eq!(
                device_for(path),
                Some(Device::Random),
                "`{}` names /dev/urandom",
                path::display(path)
            );
        }
    }

    // ============================================================== generated files

    /// A generator whose output **changes length** with every call, and a count of the calls.
    ///
    /// The length is what makes a torn read visible: two generations spliced at an offset cannot
    /// come out equal to either one, which a generator of fixed-width text could.
    fn counting_generator() -> (Arc<Generator>, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let generate: Arc<Generator> = Arc::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(generation_text(n))
        });
        (generate, calls)
    }

    /// What [`counting_generator`] produces on its `n`th call.
    fn generation_text(n: usize) -> Vec<u8> {
        format!("generation {n}: {}\n", "ab".repeat(n)).into_bytes()
    }

    fn calls(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// **A read from offset 0 generates; a read that continues does not.**
    ///
    /// Both halves of `GeneratedFile`'s rule, each against the mistake it rules out: a file
    /// generated once at `open` would give the engine one memory reading for the whole run, and a
    /// file generated on every read would splice two readings into one when read in pieces.
    #[test]
    fn a_generated_file_is_generated_at_each_read_from_zero_and_only_then() {
        let scratch = Scratch::new("generated-reads");
        let fs = scratch.fs();
        let (generate, count) = counting_generator();
        fs.serve_generated(b"/proc/meminfo", generate).expect("serve it");
        assert_eq!(calls(&count), 0, "serving a file generates nothing");

        let fd = fs.open(b"/proc/meminfo", read_flags()).expect("open the generated file");
        assert!(fd >= FIRST_FD);
        assert_eq!(calls(&count), 1, "`open` generates once, so a failure is the open's");

        // The engine's own shape: `pread` at 0 for each new reading, on one kept descriptor.
        let mut buf = [0u8; 256];
        let got = fs.pread(fd, &mut buf, 0).expect("pread at 0");
        assert_eq!(&buf[..got], generation_text(2).as_slice());
        let got = fs.pread(fd, &mut buf, 0).expect("pread at 0 again");
        assert_eq!(&buf[..got], generation_text(3).as_slice(), "a second reading is a new one");

        // Continuing from a nonzero offset reads the same generation's bytes.
        let got = fs.pread(fd, &mut buf, 5).expect("pread at 5");
        assert_eq!(calls(&count), 3, "a read that continues generates nothing");
        assert_eq!(&buf[..got], &generation_text(3)[5..]);
        // Past the end is end of file, not an error.
        assert_eq!(fs.pread(fd, &mut buf, 10_000).expect("pread past the end"), 0);

        // `read` in pieces: the first piece generates and the rest are the same file.
        let mut whole = Vec::new();
        let mut piece = [0u8; 3];
        loop {
            let got = fs.read(fd, &mut piece).expect("a sequential read");
            if got == 0 {
                break;
            }
            whole.extend_from_slice(&piece[..got]);
        }
        assert_eq!(calls(&count), 4, "one generation for the whole sequential reading");
        assert_eq!(whole, generation_text(4), "the pieces are one file, not a splice of several");

        // `lseek` back to 0 is how a sequential reader asks for a new reading; `SEEK_END` has no
        // end to seek from.
        assert_eq!(fs.seek(fd, 0, 0).expect("SEEK_SET 0"), 0);
        let got = fs.read(fd, &mut buf).expect("read after the rewind");
        assert_eq!(&buf[..got], generation_text(5).as_slice());
        assert_eq!(fs.seek(fd, -2, 1).expect("SEEK_CUR back two"), got as u64 - 2);
        let error = fs.seek(fd, 0, 2).expect_err("SEEK_END");
        assert_eq!(error.kind(), Some(FsErrorKind::InvalidInput), "{error}");
        let error = fs.seek(fd, -1, 0).expect_err("a negative offset");
        assert_eq!(error.kind(), Some(FsErrorKind::InvalidInput), "{error}");

        fs.close(fd).expect("close");
        assert!(!fs.is_open(fd));
    }

    /// **Read-only, and every operation says so the way Linux says it for a `/proc` file.**
    #[test]
    fn a_generated_file_is_a_read_only_regular_file_to_every_operation() {
        let scratch = Scratch::new("generated-readonly");
        let fs = scratch.fs();
        let (generate, _) = counting_generator();
        fs.serve_generated(b"/proc/self/statm", generate).expect("serve it");
        let path = b"/proc/self/statm";

        let with = |flags: OpenFlags| OpenFlags { read: true, ..flags };
        let none = OpenFlags::default();
        for (flags, kind, what) in [
            (OpenFlags { write: true, ..none }, FsErrorKind::PermissionDenied, "O_WRONLY"),
            (with(OpenFlags { write: true, ..none }), FsErrorKind::PermissionDenied, "O_RDWR"),
            (with(OpenFlags { truncate: true, ..none }), FsErrorKind::PermissionDenied, "O_TRUNC"),
            (with(OpenFlags { directory: true, ..none }), FsErrorKind::NotADirectory, "O_DIRECTORY"),
            (
                with(OpenFlags { create: true, exclusive: true, ..none }),
                FsErrorKind::AlreadyExists,
                "O_CREAT | O_EXCL",
            ),
        ] {
            let error = fs.open(path, flags).expect_err(what);
            assert_eq!(error.kind(), Some(kind), "{what}: {error}");
        }
        assert_eq!(fs.open_count(), 3, "no refused open left a descriptor behind");
        // `O_CREAT` without `O_EXCL` opens a file that exists, as it does anywhere.
        let fd = fs
            .open(path, OpenFlags { read: true, create: true, ..OpenFlags::default() })
            .expect("O_CREAT on an existing file");

        let error = fs.write(fd, b"x").expect_err("write");
        assert_eq!(error.kind(), Some(FsErrorKind::BadDescriptor), "{error}");
        let error = fs.pwrite(fd, b"x", 0).expect_err("pwrite");
        assert_eq!(error.kind(), Some(FsErrorKind::BadDescriptor), "{error}");
        let error = fs.fallocate(fd, 0, 1).expect_err("fallocate");
        assert_eq!(error.kind(), Some(FsErrorKind::BadDescriptor), "{error}");
        let error = fs.fsync(fd).expect_err("fsync");
        assert_eq!(error.kind(), Some(FsErrorKind::InvalidInput), "{error}");

        // A regular file of size 0 -- what `/proc` reports -- read-only, and the same through
        // the descriptor and through the path.
        let described = fs.fstat(fd).expect("fstat");
        assert_eq!(described.kind, FileKind::Regular);
        assert_eq!(described.size, 0);
        assert!(described.read_only);
        assert_eq!(fs.stat(path).expect("stat"), described);
        assert_eq!(fs.lstat(path).expect("lstat"), described);
        fs.access(path, AccessCheck::Exists).expect("F_OK");
        fs.access(path, AccessCheck::Readable).expect("R_OK");
        let error = fs.access(path, AccessCheck::Writable).expect_err("W_OK");
        assert_eq!(error.kind(), Some(FsErrorKind::PermissionDenied), "{error}");

        // Always ready, and not something epoll will watch.
        assert_eq!(fs.readiness(fd).expect("readiness"), Readiness::ALWAYS);
        assert_eq!(fs.readiness_source(fd), Some(ReadinessSource::Immediate));
        let epfd = fs.epoll_create().expect("an epoll instance");
        let member = EpollMember { events: 1, data: 0 };
        let error = fs.epoll_ctl(epfd, EpollOp::Add, fd, member).expect_err("epoll a /proc file");
        assert_eq!(error.kind(), Some(FsErrorKind::NotPollable), "{error}");

        // Nothing reached the host: the root is still empty.
        assert_eq!(
            std::fs::read_dir(&scratch.0).expect("list the root").count(),
            0,
            "a generated file must not be looked for, or created, under the root"
        );
    }

    /// **Every spelling of a served path reaches it, and nothing else does.**
    ///
    /// The over-correction half is the second loop: serving by prefix would make every `/proc`
    /// path a generated file, and `ENOENT` for a file that is not served is what the guest must
    /// get -- recorded as a miss, as any other.
    #[test]
    fn only_a_served_path_is_generated_and_every_spelling_of_it_is() {
        let scratch = Scratch::new("generated-paths");
        let fs = scratch.fs();
        let (generate, _) = counting_generator();
        fs.serve_generated(b"/proc/meminfo", Arc::clone(&generate)).expect("serve it");
        let spellings = [
            &b"/proc/meminfo"[..],
            b"/proc/./meminfo",
            b"/proc//meminfo",
            b"/x/../proc/meminfo",
            b"proc/meminfo",
        ];
        for path in spellings {
            let fd = fs.open(path, read_flags()).unwrap_or_else(|error| {
                panic!("`{}` names the served file: {error}", path::display(path))
            });
            fs.close(fd).expect("close");
        }
        let unserved =
            [&b"/proc/meminfo2"[..], b"/proc", b"/proc/self/status", b"/proc/self/meminfo"];
        for path in unserved {
            let error = fs.open(path, read_flags()).expect_err("not served");
            let shown = path::display(path);
            assert_eq!(error.kind(), Some(FsErrorKind::NotFound), "{shown}: {error}");
        }
        let misses = fs.open_misses();
        assert!(misses.contains(&b"/proc/self/status".to_vec()), "an unserved path is a miss");
        assert!(!misses.contains(&b"/proc/meminfo".to_vec()), "a served one is not");

        // Served once; and a device cannot be one.
        let error =
            fs.serve_generated(b"/proc/./meminfo", Arc::clone(&generate)).expect_err("twice");
        assert!(error.to_string().contains("already served"), "{error}");
        let error = fs.serve_generated(b"/dev/urandom", generate).expect_err("a device");
        assert!(error.to_string().contains("device"), "{error}");
    }

    /// **A file that cannot be produced is refused by the call that asked, by name.**
    #[test]
    fn a_generator_that_cannot_produce_the_file_refuses_the_call_that_asked() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let scratch = Scratch::new("generated-refusal");
        let fs = scratch.fs();
        let failing = Arc::new(AtomicBool::new(true));
        let switch = Arc::clone(&failing);
        let generate: Arc<Generator> = Arc::new(move || {
            if switch.load(Ordering::SeqCst) {
                Err(FsError::refused("generate", "/proc/meminfo", "nothing has said what it holds"))
            } else {
                Ok(b"fine\n".to_vec())
            }
        });
        fs.serve_generated(b"/proc/meminfo", generate).expect("serve it");

        let error = fs.open(b"/proc/meminfo", read_flags()).expect_err("the generator refuses");
        assert_eq!(error.kind(), None, "a refusal, not an errno: {error}");
        assert!(error.to_string().contains("nothing has said what it holds"), "{error}");
        assert_eq!(fs.open_count(), 3, "and no descriptor was handed out");

        failing.store(false, Ordering::SeqCst);
        let fd = fs.open(b"/proc/meminfo", read_flags()).expect("now it can be produced");
        failing.store(true, Ordering::SeqCst);
        let mut buf = [0u8; 16];
        let error = fs.pread(fd, &mut buf, 0).expect_err("a later reading that cannot be produced");
        assert_eq!(error.kind(), None, "{error}");
    }

    // ============================================================== pipes

    /// Bytes go in one end and come out of the other, in order, and the queue empties.
    #[test]
    fn a_pipe_carries_bytes_from_the_write_end_to_the_read_end_in_order() {
        let scratch = Scratch::new("pipe-roundtrip");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");
        assert!(read_fd >= FIRST_FD && write_fd > read_fd, "{read_fd} and {write_fd}");
        assert_eq!(fs.pipe_end(read_fd), Some(PipeEnd::Read));
        assert_eq!(fs.pipe_end(write_fd), Some(PipeEnd::Write));

        assert_eq!(fs.write(write_fd, b"abc").expect("write"), 3);
        assert_eq!(fs.write(write_fd, b"de").expect("write"), 2);
        let mut buf = [0u8; 8];
        assert_eq!(fs.read(read_fd, &mut buf).expect("read"), 5);
        assert_eq!(&buf[..5], b"abcde", "the bytes arrive in the order they were written");
        // And the queue is empty again, which `fstat`'s size reports.
        assert_eq!(fs.fstat(read_fd).expect("fstat").size, 0);
    }

    /// The ends are not interchangeable: reading the write end and writing the read end both fail.
    #[test]
    fn each_end_of_a_pipe_refuses_the_other_ends_operation() {
        let scratch = Scratch::new("pipe-ends");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");
        let mut buf = [0u8; 4];
        let error = fs.read(write_fd, &mut buf).expect_err("the write end is not readable");
        assert_eq!(error.kind(), Some(FsErrorKind::BadDescriptor), "{error}");
        let error = fs.write(read_fd, b"x").expect_err("the read end is not writable");
        assert_eq!(error.kind(), Some(FsErrorKind::BadDescriptor), "{error}");
    }

    /// An empty pipe with a live writer reports `WouldBlock`; this seam never waits.
    #[test]
    fn an_empty_pipe_reports_would_block_rather_than_waiting() {
        let scratch = Scratch::new("pipe-empty");
        let fs = scratch.fs();
        let (read_fd, _write_fd) = fs.pipe().expect("a pipe");
        let mut buf = [0u8; 4];
        let error = fs.read(read_fd, &mut buf).expect_err("nothing has been written");
        assert_eq!(error.kind(), Some(FsErrorKind::WouldBlock), "{error}");
    }

    /// **The clause end-of-file depends on.** With every write end closed, the read end is
    /// *readable* and reads zero — for ever, not once.
    #[test]
    fn a_pipe_whose_writers_have_all_closed_reads_end_of_file_and_reports_itself_readable() {
        let scratch = Scratch::new("pipe-eof");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");
        assert_eq!(fs.write(write_fd, b"tail").expect("write"), 4);
        fs.close(write_fd).expect("close the write end");

        // The buffered bytes still come out: closing a writer does not discard them.
        let mut buf = [0u8; 8];
        assert_eq!(fs.read(read_fd, &mut buf).expect("read"), 4);
        assert_eq!(&buf[..4], b"tail");

        let readiness = fs.readiness(read_fd).expect("readiness");
        assert!(
            readiness.readable,
            "a reader with no writers left must be readable, or a poll loop parks on it for ever"
        );
        assert!(readiness.hangup, "POLLHUP is what an emptied, writerless pipe reports");
        // Twice, because end of file is a state and not an event.
        assert_eq!(fs.read(read_fd, &mut buf).expect("read at EOF"), 0);
        assert_eq!(fs.read(read_fd, &mut buf).expect("read at EOF again"), 0);
    }

    /// A write with no reader left is `EPIPE`, and the write end reports `POLLERR`.
    #[test]
    fn a_write_to_a_pipe_with_no_readers_is_a_broken_pipe() {
        let scratch = Scratch::new("pipe-broken");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");
        fs.close(read_fd).expect("close the read end");
        let error = fs.write(write_fd, b"x").expect_err("no reader is left");
        assert_eq!(error.kind(), Some(FsErrorKind::BrokenPipe), "{error}");
        let readiness = fs.readiness(write_fd).expect("readiness");
        assert!(readiness.error, "POLLERR is what a writer with no readers reports");
        assert!(!readiness.writable);
    }

    /// A full pipe takes **what fits** rather than refusing the whole write.
    ///
    /// The believable wrong answer here is `WouldBlock` whenever the whole buffer does not fit,
    /// which makes a guest writing a large buffer spin against a pipe that is draining. POSIX
    /// guarantees atomicity only up to [`PIPE_BUF`]; past it a write may be split.
    #[test]
    fn a_write_larger_than_the_free_space_takes_what_fits_and_says_how_much() {
        // **The capacity against a literal, once.** Every other assertion here is written in
        // terms of `PIPE_CAPACITY`, which would make them all pass for any value of it — the
        // shape review finding M2 named, where a test asserts its own definition.
        assert_eq!(PIPE_CAPACITY, 65_536, "Linux's default pipe capacity");
        assert_eq!(PIPE_BUF, 4_096, "Linux's atomicity bound");
        let scratch = Scratch::new("pipe-full");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");
        let big = vec![0x5Au8; PIPE_CAPACITY + 4096];
        assert_eq!(
            fs.write(write_fd, &big).expect("write"),
            PIPE_CAPACITY,
            "a write past the capacity takes exactly the capacity"
        );
        assert!(!fs.readiness(write_fd).expect("readiness").writable, "the pipe is full");
        let error = fs.write(write_fd, b"more").expect_err("the pipe is full");
        assert_eq!(error.kind(), Some(FsErrorKind::WouldBlock), "{error}");

        // Drain one byte and exactly one byte's worth of room appears.
        let mut one = [0u8; 1];
        assert_eq!(fs.read(read_fd, &mut one).expect("read"), 1);
        assert!(fs.readiness(write_fd).expect("readiness").writable);
        assert_eq!(fs.write(write_fd, b"more").expect("write into the freed byte"), 1);
    }

    /// `O_NONBLOCK` is carried by a pipe and **refused** on anything that cannot block.
    #[test]
    fn only_a_pipe_carries_o_nonblock_and_the_others_refuse_it_rather_than_ignoring_it() {
        let scratch = Scratch::new("pipe-nonblock");
        let fs = scratch.fs();
        let (read_fd, _write_fd) = fs.pipe().expect("a pipe");
        assert!(!fs.is_nonblocking(read_fd).expect("a pipe answers"));
        fs.set_nonblocking(read_fd, true).expect("a pipe takes the flag");
        assert!(fs.is_nonblocking(read_fd).expect("a pipe answers"));
        fs.set_nonblocking(read_fd, false).expect("and gives it up");
        assert!(!fs.is_nonblocking(read_fd).expect("a pipe answers"));

        let fd = fs.open(b"/f.txt", write_flags()).expect("a file");
        assert!(!fs.is_nonblocking(fd).expect("a file answers false"));
        let error = fs.set_nonblocking(fd, true).expect_err("a file cannot block");
        assert!(matches!(error, FsError::Refused { .. }), "{error}");
        let error = fs.set_nonblocking(STDOUT_FD, true).expect_err("nor can a standard stream");
        assert!(matches!(error, FsError::Refused { .. }), "{error}");
    }

    /// Readiness is a **total function over the table**: every kind answers, and the kinds that
    /// cannot block answer `ALWAYS`.
    ///
    /// The successor to the closed-descriptor-space argument: `poll`'s old rule was "every open
    /// descriptor is ready", and this is the rule that replaced it.
    #[test]
    fn every_descriptor_kind_answers_a_readiness_and_only_a_pipe_can_be_unready() {
        let scratch = Scratch::new("pipe-total");
        let fs = scratch.fs();
        let file = fs.open(b"/f.txt", write_flags()).expect("a file");
        let dir = fs
            .open(b"/", OpenFlags { read: true, directory: true, ..OpenFlags::default() })
            .expect("a directory");
        let device = fs.open(b"/dev/null", read_flags()).expect("a device");
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");

        for (what, fd) in [("a file", file), ("a directory", dir), ("a device", device),
                           ("stdin", STDIN_FD), ("stdout", STDOUT_FD), ("stderr", STDERR_FD)] {
            assert_eq!(
                fs.readiness(fd).expect("every open descriptor answers"),
                Readiness::ALWAYS,
                "{what} cannot block, so it is always ready"
            );
        }
        // The pipe is the one that is not.
        assert!(!fs.readiness(read_fd).expect("readiness").readable, "an empty pipe is not readable");
        assert!(fs.readiness(write_fd).expect("readiness").writable, "an empty pipe is writable");

        let error = fs.readiness(4096).expect_err("a descriptor nobody opened");
        assert_eq!(error.kind(), Some(FsErrorKind::BadDescriptor), "{error}");
    }

    /// The ready gate rises when a pipe changes and a waiter with a deadline gives up on time.
    ///
    /// **Structural rather than timed.** The assertion is on the generation counter and on the
    /// boolean the wait returns, not on how long it took: `VERIFICATION.md` entry 6 is what a
    /// timing assertion here would become.
    #[test]
    fn the_ready_gate_rises_on_a_write_and_a_bounded_wait_gives_up() {
        let scratch = Scratch::new("pipe-gate");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");

        let before = fs.ready_generation();
        assert!(
            !fs.wait_for_readiness(before, Duration::from_millis(1)),
            "nothing changed, so the wait reports no change"
        );
        assert_eq!(fs.ready_generation(), before, "and an expired wait changes nothing");

        fs.write(write_fd, b"x").expect("write");
        assert_ne!(fs.ready_generation(), before, "a write is a readiness change");
        assert!(
            fs.wait_for_readiness(before, Duration::from_millis(1)),
            "a generation older than the current one returns immediately"
        );

        // And closing an end is a change too, which is what stops a waiter parking on a pipe
        // that can never produce another byte.
        let seen = fs.ready_generation();
        let mut buf = [0u8; 4];
        fs.read(read_fd, &mut buf).expect("drain");
        let seen_after_drain = fs.ready_generation();
        assert_ne!(seen_after_drain, seen, "a read frees room, which is a change for the writer");
        fs.close(write_fd).expect("close the write end");
        assert_ne!(fs.ready_generation(), seen_after_drain, "closing an end is a change");
    }

    /// A pipe needs two descriptors and takes neither when only one is free.
    #[test]
    fn a_pipe_is_all_or_nothing_against_the_descriptor_ceiling() {
        let scratch = Scratch::new("pipe-ceiling");
        let fs = scratch.fs();
        // Three standard streams are already in the table; fill it to one short of the ceiling.
        let mut held = Vec::new();
        while fs.open_count() < MAX_OPEN_FILES - 1 {
            held.push(fs.open(b"/dev/null", read_flags()).expect("a device descriptor"));
        }
        let at_ceiling = fs.open_count();
        let error = fs.pipe().expect_err("one free slot is not two");
        assert_eq!(error.kind(), Some(FsErrorKind::TooManyOpenFiles), "{error}");
        assert_eq!(fs.open_count(), at_ceiling, "a refused pipe leaks no descriptor");

        // One more freed slot and it fits exactly.
        fs.close(held.pop().expect("a held descriptor")).expect("close");
        let (read_fd, write_fd) = fs.pipe().expect("two free slots");
        assert_eq!(fs.open_count(), MAX_OPEN_FILES);
        assert_ne!(read_fd, write_fd);
    }

    /// `pread` on a pipe is `ESPIPE`, not a read from position zero.
    #[test]
    fn a_pipe_has_no_offset_to_pread_at() {
        let scratch = Scratch::new("pipe-pread");
        let fs = scratch.fs();
        let (read_fd, write_fd) = fs.pipe().expect("a pipe");
        fs.write(write_fd, b"abc").expect("write");
        let mut buf = [0u8; 3];
        let error = fs.pread(read_fd, &mut buf, 0).expect_err("a pipe has no offset");
        assert_eq!(error.kind(), Some(FsErrorKind::InvalidInput), "{error}");
        assert_eq!(buf, [0, 0, 0], "and it read nothing");
    }

    /// A root that is not a directory, or is not there, is refused when the filesystem is built.
    #[test]
    fn a_root_that_is_not_a_directory_is_refused_at_construction() {
        let scratch = Scratch::new("root");
        let error = Filesystem::new(scratch.0.join("nope")).expect_err("a missing root");
        assert!(matches!(error, FsError::Refused { .. }), "{error}");
        let file = scratch.0.join("a-file");
        std::fs::write(&file, b"x").expect("a file");
        let error = Filesystem::new(&file).expect_err("a root that is a file");
        assert!(matches!(error, FsError::Refused { .. }), "{error}");
    }
    // ============================================================ sockets in the same table
    //
    // **Compiled only where a backend exists** -- Windows, and Linux since its backend was
    // written -- for the reason `tests/net_loopback.rs` states at length: the macOS net backend
    // is structural, so `Socket::new` cannot produce a socket there and a test that "passed" by
    // asserting the refusal would be asserting the absence of an implementation rather than the
    // presence of one. Every socket below is unconnected and bound to nothing, and the policy is
    // `loopback_only`, so nothing here can reach the network even if a test were written wrongly.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    mod sockets {
        use super::*;
        use crate::net::{IpFamily, NetPolicy, Socket, SocketKind};

        /// An unconnected datagram socket under a policy that admits nothing off this machine.
        fn a_socket() -> Socket {
            Socket::new(SocketKind::Datagram, IpFamily::V4, Arc::new(NetPolicy::loopback_only()))
                .expect("a datagram socket")
        }

        /// A socket's number comes out of the **same** allocator a file's does, and `close` takes
        /// it back.
        ///
        /// D30 point 2 asserted as a property rather than as a comment: the defect it forbids is
        /// two allocators handing out one number, so the test opens a file *and* a socket and
        /// requires the numbers to differ — which is the thing a second allocator would get
        /// wrong, and which asserting either one alone cannot see.
        #[test]
        fn a_socket_and_a_file_cannot_be_handed_the_same_descriptor_number() {
            let scratch = Scratch::new("socket-fd");
            let fs = scratch.fs();
            let file = fs.open(b"/f.txt", write_flags()).expect("a file");
            let sock = fs.attach_socket(a_socket()).expect("a socket");
            assert!(sock >= FIRST_FD, "fd {sock} collides with a standard stream");
            assert_ne!(sock, file, "one descriptor space, one allocator");
            assert!(fs.is_socket(sock));
            assert!(!fs.is_socket(file), "a file is not a socket");
            assert!(fs.is_open(sock));

            fs.close(sock).expect("close");
            assert!(!fs.is_open(sock), "closing the descriptor closes the socket with it");
            assert!(!fs.is_socket(sock));
            assert_eq!(fs.close(sock).unwrap_err().kind(), Some(FsErrorKind::BadDescriptor));
        }

        /// `read` and `write` refuse a socket **by name**, and the refusal says where to go.
        ///
        /// The value of the assertion is in the second half. A refusal that merely said "not
        /// supported" would leave the next caller to guess, and the reason this path is a refusal
        /// at all — that a socket's failure kinds do not survive `FsErrorKind` — is exactly the
        /// kind of reasoning that goes missing between one milestone and the next.
        #[test]
        fn read_and_write_on_a_socket_refuse_by_name_and_name_the_route() {
            let scratch = Scratch::new("socket-rw");
            let fs = scratch.fs();
            let sock = fs.attach_socket(a_socket()).expect("a socket");

            let mut buf = [0u8; 8];
            let error = fs.read(sock, &mut buf).expect_err("read on a socket is recv");
            assert!(matches!(error, FsError::Refused { .. }), "{error}");
            let text = error.to_string();
            assert!(text.contains("socket_at"), "the refusal names the route: {text}");
            assert!(text.contains("recv"), "and the call to make: {text}");
            assert_eq!(buf, [0u8; 8], "and it read nothing into the caller's buffer");

            let error = fs.write(sock, b"bytes").expect_err("write on a socket is send");
            assert!(matches!(error, FsError::Refused { .. }), "{error}");
            let text = error.to_string();
            assert!(text.contains("socket_at"), "{text}");
            assert!(text.contains("send"), "{text}");
        }

        /// `pread` on a socket is `ESPIPE`, which is `InvalidInput` here — a *different* answer
        /// from `read`'s refusal, because it is a different fact.
        #[test]
        fn a_socket_has_no_offset_to_pread_at() {
            let scratch = Scratch::new("socket-pread");
            let fs = scratch.fs();
            let sock = fs.attach_socket(a_socket()).expect("a socket");
            let mut buf = [0u8; 4];
            let error = fs.pread(sock, &mut buf, 0).expect_err("a socket has no offset");
            assert_eq!(error.kind(), Some(FsErrorKind::InvalidInput), "{error}");
        }

        /// `O_NONBLOCK` set through the table reaches the **socket**, and reads back from it.
        ///
        /// Asserted through `Filesystem::socket_at` as well as through `is_nonblocking`, because
        /// a table that remembered the flag without telling the host would pass the second
        /// assertion on its own — and every `recv` would still block.
        #[test]
        fn the_nonblocking_flag_set_through_the_table_reaches_the_socket_itself() {
            let scratch = Scratch::new("socket-nonblock");
            let fs = scratch.fs();
            let sock = fs.attach_socket(a_socket()).expect("a socket");
            assert!(!fs.is_nonblocking(sock).expect("a fresh socket blocks, as socket(2) says"));

            fs.set_nonblocking(sock, true).expect("O_NONBLOCK");
            assert!(fs.is_nonblocking(sock).expect("the table's answer"));
            assert!(
                fs.socket_at(sock).expect("the handle").lock().expect("not poisoned").nonblocking(),
                "the flag was recorded in the table and never reached the socket"
            );

            fs.set_nonblocking(sock, false).expect("and back again");
            assert!(!fs.is_nonblocking(sock).expect("the table's answer"));
        }

        /// `fstat` on a socket reports no size and no times, which is what Linux reports.
        ///
        /// The size assertion is the one that matters: the pipe arm beside this one legitimately
        /// puts the buffered byte count in `st_size`, and copying that to a socket would invent a
        /// number Linux does not put there — `FIONREAD` is the call that answers it.
        #[test]
        fn fstat_on_a_socket_has_no_size_and_no_times() {
            let scratch = Scratch::new("socket-fstat");
            let fs = scratch.fs();
            let sock = fs.attach_socket(a_socket()).expect("a socket");
            let stat = fs.fstat(sock).expect("fstat");
            assert_eq!(stat.kind, FileKind::Other, "S_IFSOCK is not a regular file");
            assert_eq!(stat.size, 0);
            assert_eq!(stat.accessed, None);
            assert_eq!(stat.modified, None);
            assert_ne!(stat.identity, 0, "zero is the value no real inode has");
        }

        /// Every descriptor kind is on the side of a mixed wait that can actually wait for it.
        ///
        /// **Membership over all four kinds, not one of them** (VERIFICATION entry 1): a caller
        /// that alternates between the readiness gate and the host's `select` gets the *wrong*
        /// side wrong silently — it waits out its whole slice on a descriptor that was already
        /// ready — so each kind is named here rather than inferred.
        #[test]
        fn each_descriptor_kind_waits_on_the_side_that_can_wake_it() {
            let scratch = Scratch::new("socket-source");
            let fs = scratch.fs();
            let file = fs.open(b"/f.txt", write_flags()).expect("a file");
            let (read_fd, _write_fd) = fs.pipe().expect("a pipe");
            let event = fs.eventfd(0, 0).expect("an eventfd");
            let sock = fs.attach_socket(a_socket()).expect("a socket");

            assert_eq!(fs.readiness_source(file), Some(ReadinessSource::Immediate));
            assert_eq!(fs.readiness_source(STDOUT_FD), Some(ReadinessSource::Immediate));
            assert_eq!(fs.readiness_source(read_fd), Some(ReadinessSource::Gate));
            assert_eq!(fs.readiness_source(event), Some(ReadinessSource::Gate));
            assert_eq!(fs.readiness_source(sock), Some(ReadinessSource::Host));
            assert_eq!(fs.readiness_source(9_999), None, "a descriptor that is not open");
        }

        /// `socket_at` on a descriptor that is not a socket is `ENOTSOCK`, and on one that is not
        /// open is `EBADF`. The two are different facts and stay different answers.
        #[test]
        fn socket_at_tells_not_a_socket_apart_from_not_open() {
            let scratch = Scratch::new("socket-at");
            let fs = scratch.fs();
            let file = fs.open(b"/f.txt", write_flags()).expect("a file");
            assert_eq!(
                fs.socket_at(file).unwrap_err().kind(),
                Some(FsErrorKind::NotASocket),
                "a file is open and is not a socket"
            );
            assert_eq!(fs.socket_at(9_999).unwrap_err().kind(), Some(FsErrorKind::BadDescriptor));
        }

        /// A fresh socket is **not readable**, and it answers at all — which is what
        /// `Entry::readiness` having no default arm was for.
        ///
        /// `Readiness::ALWAYS` is asserted *against* rather than for: it is a regular file's
        /// answer, and a socket variant that had inherited it would send a caller into a `recv`
        /// with nothing behind it.
        #[test]
        fn a_socket_answers_readiness_from_the_host_rather_than_always() {
            let scratch = Scratch::new("socket-readiness");
            let fs = scratch.fs();
            let sock = fs.attach_socket(a_socket()).expect("a socket");
            let readiness = fs.readiness(sock).expect("the host answered");
            assert!(!readiness.readable, "nothing has arrived on an unconnected datagram socket");
            assert_ne!(readiness, Readiness::ALWAYS, "ALWAYS is a regular file's answer");
            assert!(!readiness.error, "and the socket is not broken");
        }

        /// A socket counts against the descriptor ceiling, exactly as a file and a pipe do.
        ///
        /// The refusal is `EMFILE` and the table does not grow — and the socket the caller never
        /// learned a number for is dropped, which is the only correct thing to do with a host
        /// descriptor nothing could ever close.
        #[test]
        fn a_socket_counts_against_the_same_descriptor_ceiling_a_file_does() {
            let scratch = Scratch::new("socket-ceiling");
            let fs = scratch.fs();
            let mut held = Vec::new();
            while fs.open_count() < MAX_OPEN_FILES {
                held.push(fs.attach_socket(a_socket()).expect("a socket below the ceiling"));
            }
            assert_eq!(fs.open_count(), MAX_OPEN_FILES);
            let error = fs.attach_socket(a_socket()).expect_err("one past the ceiling");
            assert_eq!(error.kind(), Some(FsErrorKind::TooManyOpenFiles), "{error}");
            assert_eq!(fs.open_count(), MAX_OPEN_FILES, "and the table did not grow");

            fs.close(held.pop().expect("a held socket")).expect("close");
            fs.attach_socket(a_socket()).expect("the freed slot is reusable");
        }
    }
}
