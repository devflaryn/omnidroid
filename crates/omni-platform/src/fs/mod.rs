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
//! * **No `symlink`, `link`, `readlink`, `chmod`, `chown` or `utime`.** None is in the 188
//!   statically-reachable imports, and the first two are what make [`path`]'s symlink rule
//!   sufficient: the guest cannot create a link, so the set of links inside the root is fixed by
//!   whoever populated it.
//! * **No `chdir`/`getcwd`.** Also not reachable, which is why a relative guest path resolves
//!   against the root rather than against a working directory nothing can move.
//! * **No `fcntl`, `ioctl`, `dup` or `lseek`.** Not reachable either. `lseek`'s absence is why
//!   [`Filesystem::pread`] exists as its own primitive rather than being built from a seek and a
//!   read: a seek-and-read pair is not `pread`, because it moves the descriptor's own offset —
//!   which was MEASURED here and is written up in the Windows backend.

mod error;
pub mod path;

pub use error::{FsError, FsErrorKind, FsResult};
pub use path::{FinalLink, Resolved, NAME_MAX, PATH_MAX};

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

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

/// How many descriptors one guest instance may hold open at once.
///
/// **A policy number, and stated as one.** A guest that leaks descriptors in a loop would
/// otherwise hold as many host handles as it liked, and several instances share one host process.
/// Past this, [`Filesystem::open`] reports [`FsErrorKind::TooManyOpenFiles`], which becomes the
/// guest's `EMFILE` — the answer a real device gives when it hits `RLIMIT_NOFILE`, and one every
/// correct caller already has a branch for.
pub const MAX_OPEN_FILES: usize = 64;

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
}

#[derive(Debug)]
struct Table {
    open: BTreeMap<i32, Entry>,
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
            table: Mutex::new(Table {
                open: BTreeMap::from([
                    (STDIN_FD, Entry::Standard(StdStream::In)),
                    (STDOUT_FD, Entry::Standard(StdStream::Out)),
                    (STDERR_FD, Entry::Standard(StdStream::Err)),
                ]),
                dirs: BTreeMap::new(),
                next_dir: 1,
            }),
        })
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
        match table.open.remove(&fd) {
            // Dropping the entry closes the host handle. A close that the host itself fails is
            // not reportable through `Drop`, and POSIX already says the descriptor is gone
            // whatever `close` returned.
            Some(_) => Ok(()),
            None => Err(bad_fd("close", fd)),
        }
    }

    /// Whether this instance holds `fd`.
    #[must_use]
    pub fn is_open(&self, fd: i32) -> bool {
        self.table().open.contains_key(&fd)
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
            Some(Entry::Standard(_)) => Err(FsError::kinded(
                OP,
                format!("fd {fd}"),
                FsErrorKind::InvalidInput,
                "a standard stream is a pipe: it has no offset to read at (ESPIPE)",
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
            let Some(name) = name.to_str() else {
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
}
