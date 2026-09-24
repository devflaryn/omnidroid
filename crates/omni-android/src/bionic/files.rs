//! Files and directories: the eighteen descriptor-level symbols, and the three guest structures
//! they fill.
//!
//! `open`, `__open_2`, `close`, `read`, `pread`, `__write_chk`, `access`, `stat`, `fstat`,
//! `lstat`, `statvfs`, `rename`, `unlink`, `mkdir`, `rmdir`, `opendir`, `readdir`, `closedir`.
//! Bionic's `FILE *` layer sits on top of these and is in [`super::stdio`]. The kernel's `statfs`
//! and `fstatfs`, which the engine issues as raw syscalls rather than imports, are answered here
//! too (see [`statfs`] and [`fstatfs`]) and reached through `procenv`'s `syscall`.
//!
//! # Where the answers come from, and what refuses
//!
//! `omni-platform`'s [`fs`](omni_platform::fs) seam, which is a **rooted** descriptor table: every
//! guest path is resolved inside one host directory and a path that cannot be is refused by name.
//! An instance whose host never called [`Bionic::set_filesystem_root`](super::Bionic::set_filesystem_root)
//! has no filesystem at all, and every path-taking symbol here refuses naming that method.
//!
//! **That default is the same shape as `dl_iterate_phdr`'s and `HwcapPolicy::Undecided`'s, and
//! for the same reason.** A default root would have to be *somewhere* — the process's working
//! directory, or a temporary directory — and either would let a guest read and write host files
//! that nobody decided to expose. The host has to name the directory, and the fact that it had to
//! is the point.
//!
//! # The split between `-1` with `errno` and a refusal
//!
//! The same split `guestmem` and `clocks` draw, and the one this group has the most opportunities
//! to get wrong:
//!
//! * **Well-formed and legitimately failed → what Linux returns, with `errno` set.** A file that
//!   is not there is `ENOENT`, a second `mkdir` is `EEXIST`, `rmdir` on a non-empty directory is
//!   `ENOTEMPTY`, a descriptor that is not open is `EBADF`. That is the contract rather than a
//!   stub: guest code has a defined branch for every one of them and a real device produces them.
//! * **Cannot be carried out correctly → [`AbiError::Refused`], naming the symbol and the
//!   argument.** No filesystem root; a path that tries to leave the root; `access(X_OK)`; an
//!   `open` flag whose guarantee this layer cannot meet; a host failure `std::io::ErrorKind` does
//!   not classify, because an unclassified error given a specific errno is a guess.
//!
//! The third of those is the one worth stating twice. `FsErrorKind::Other` is **not** mapped to
//! `EIO`. `EIO` is a real answer that guest code retries or reports; an error nobody identified
//! deserves the refusal that names it.
//!
//! # Order: admit the guest's buffer, then touch the descriptor
//!
//! Every transfer here validates the guest's whole buffer **before** the first irreversible host
//! step, because a descriptor read is destructive on a pipe and a descriptor write is destructive
//! everywhere. D22 states the rule for `arc4random_buf`; phase 3b did not carry it across, which
//! is what the adapter review filed as **M1**. [`read_into_guest`] and [`write_from_guest`] carry
//! it now, [`readdir`] carries it for the directory stream, and [`read_into_guest`] is where the
//! argument — including the cross-thread unmapping this **cannot** defend against — is written
//! out. [`write_struct`] is the same rule for the fixed structures: one access, so a destination
//! that is only partly writable leaves the guest nothing rather than half a `struct stat`.
//!
//! # The layout trap, and what is ASSUMED
//!
//! Three guest structures are written here and **none of them is the host's**. Android arm64 is
//! LP64 with a 64-bit `time_t`, a 16-byte `struct timespec` and the kernel's own field order.
//! Every offset below is derived from a named source and marked; see [`STAT_BYTES`],
//! [`STATVFS_BYTES`] and [`DIRENT_BYTES`]. **There is no NDK on this machine**, which is the same
//! gap `omni-bionic`'s `layouts.rs`, `bionic::data::FILE_BYTES` and `omni_bionic::time::TM_BYTES`
//! already record, and the same discipline applies: the derivation is written out field by field
//! so it can be checked against a header rather than re-derived from memory.

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_mem::GuestAddr;
use omni_platform::fs::{
    AccessCheck, DirEntryInfo, FileKind, FileStat, Filesystem, FsErrorKind, FsResult,
    OpenFlags, ReadinessSource, RecordLock, VolumeStats, IO_BLOCK,
};

use crate::abi::Args;
use crate::boundary::ImportCall;
use crate::error::AbiResult;
use crate::mem::Blame;

use super::view::GuestView;
use super::{active, enter, STOP_SLICE};

// ================================================================== the guest's constants
//
// Linux UAPI, which is what `libroblox.so` was compiled against: `include/uapi/asm-generic/`.
// arm64 defines none of its own overrides for these headers, so the asm-generic values are the
// arm64 values. This is the same provenance the `clockid_t` numbers and the `AT_*` values in
// `clocks` and `procenv` have, and the opposite of bionic's private `_SC_*` numbering, which
// `sysconf` refuses precisely because it could not be checked.

/// `O_ACCMODE`, the mask over the three access modes.
const O_ACCMODE: i32 = 0o3;
/// `O_RDONLY`.
const O_RDONLY: i32 = 0o0;
/// `O_WRONLY`.
const O_WRONLY: i32 = 0o1;
/// `O_RDWR`.
const O_RDWR: i32 = 0o2;
/// `O_CREAT`.
const O_CREAT: i32 = 0o100;
/// `O_EXCL`.
const O_EXCL: i32 = 0o200;
/// `O_NOCTTY` — there is no controlling terminal here, so it describes a state that already holds.
const O_NOCTTY: i32 = 0o400;
/// `O_TRUNC`.
const O_TRUNC: i32 = 0o1000;
/// `O_APPEND`.
const O_APPEND: i32 = 0o2000;
/// `O_NONBLOCK` — a no-op on a regular file on Linux too.
const O_NONBLOCK: i32 = 0o4000;
/// `O_DSYNC` — a durability promise this layer does not make.
const O_DSYNC: i32 = 0o10000;
/// `FASYNC` — signal-driven I/O, and there are no signals here.
const O_ASYNC: i32 = 0o20000;
/// `O_DIRECT` — a promise to bypass the page cache.
const O_DIRECT: i32 = 0o40000;
/// `O_LARGEFILE` — always in effect on LP64.
const O_LARGEFILE: i32 = 0o100000;
/// `O_DIRECTORY`.
const O_DIRECTORY: i32 = 0o200000;
/// `O_NOFOLLOW` — already the standing policy: no path component may be a symlink.
const O_NOFOLLOW: i32 = 0o400000;
/// `O_NOATIME` — advisory.
const O_NOATIME: i32 = 0o1000000;
/// `O_CLOEXEC` — recorded on the descriptor for `fcntl(F_GETFD)`; nothing here execs.
const O_CLOEXEC: i32 = 0o2000000;
/// `O_SYNC`, which includes `O_DSYNC` in its own bit pattern.
const O_SYNC: i32 = 0o4010000;
/// `O_PATH` — a descriptor that is not open on the file at all.
const O_PATH: i32 = 0o10000000;
/// `O_TMPFILE`, which includes `O_DIRECTORY` in its bit pattern.
const O_TMPFILE: i32 = 0o20200000;

/// The flags that are honoured, plus the ones whose meaning is already true here.
const HONOURED: i32 = O_ACCMODE | O_CREAT | O_EXCL | O_TRUNC | O_APPEND | O_DIRECTORY;
/// The flags that are accepted and have nothing to do, each for a stated reason.
const ALREADY_TRUE: i32 =
    O_NOCTTY | O_NONBLOCK | O_LARGEFILE | O_NOFOLLOW | O_NOATIME | O_CLOEXEC;

/// `F_OK`.
const F_OK: i32 = 0;
/// `X_OK`.
const X_OK: i32 = 1;
/// `W_OK`.
const W_OK: i32 = 2;
/// `R_OK`.
const R_OK: i32 = 4;

/// The largest count `read`, `pread` and `write` accept, from Linux's own `SSIZE_MAX` rule.
///
/// Linux reports `EINVAL` for a count past this, because the return is a signed `ssize_t` and a
/// larger count could not be reported even if it were transferred. Repeating that rule is the
/// contract; it is also what stops a guest asking for `2^64 - 1` bytes.
const MAX_COUNT: u64 = i64::MAX as u64;

// ================================================================== `struct stat`

/// Bytes of the guest's `struct stat` on Android arm64.
///
/// **ASSUMED: derived from Linux UAPI `include/uapi/asm-generic/stat.h`, which arm64 uses
/// unmodified and which bionic's `<sys/stat.h>` matches field for field on LP64. Not verified
/// against an NDK — there is none on this machine.**
///
/// | offset | bytes | field |
/// |---|---|---|
/// | 0 | 8 | `unsigned long st_dev` |
/// | 8 | 8 | `unsigned long st_ino` |
/// | 16 | 4 | `unsigned int st_mode` |
/// | 20 | 4 | `unsigned int st_nlink` |
/// | 24 | 4 | `unsigned int st_uid` |
/// | 28 | 4 | `unsigned int st_gid` |
/// | 32 | 8 | `unsigned long st_rdev` |
/// | 40 | 8 | `unsigned long __pad1` |
/// | 48 | 8 | `long st_size` |
/// | 56 | 4 | `int st_blksize` |
/// | 60 | 4 | `int __pad2` |
/// | 64 | 8 | `long st_blocks` |
/// | 72 | 8 + 8 | `st_atime`, `st_atime_nsec` |
/// | 88 | 8 + 8 | `st_mtime`, `st_mtime_nsec` |
/// | 104 | 8 + 8 | `st_ctime`, `st_ctime_nsec` |
/// | 120 | 4 + 4 | `__unused4`, `__unused5` |
/// | **128** | | end |
///
/// **Why an error here is loud rather than silent, which is the only thing that makes a derived
/// layout acceptable.** Unlike `FILE`, a `struct stat` is *transparent*: the guest reads its
/// fields directly. A wrong offset therefore hands guest code a wrong size or a wrong mode. What
/// bounds the damage is that the two most-read fields sit at the two ends of the structure's
/// most-checked pair — `st_mode` at 16 and `st_size` at 48 — and both are asserted against a
/// file this layer created with a known length, from real guest code, in `tests/bionic.rs`. A
/// layout that were wrong would move `st_size` off a number the test knows.
pub const STAT_BYTES: usize = 128;

/// `S_IFMT`: the mask over the file-type bits of `st_mode`.
pub const S_IFMT: u32 = 0o170000;
/// `S_IFDIR`.
pub const S_IFDIR: u32 = 0o040000;
/// `S_IFREG`.
pub const S_IFREG: u32 = 0o100000;
/// `S_IFLNK`.
pub const S_IFLNK: u32 = 0o120000;
/// `S_IFCHR`.
pub const S_IFCHR: u32 = 0o020000;

/// The permission bits a readable-and-writable regular file reports.
const MODE_FILE_RW: u32 = 0o644;
/// The permission bits a read-only regular file reports.
const MODE_FILE_RO: u32 = 0o444;
/// The permission bits a writable directory reports.
const MODE_DIR_RW: u32 = 0o755;
/// The permission bits a read-only directory reports.
const MODE_DIR_RO: u32 = 0o555;

/// Build the guest's `st_mode` from what the host actually knows.
///
/// **The type bits are exact and the permission bits are DERIVED from one host fact**, and the
/// difference is stated rather than blurred:
///
/// * `S_IFREG`, `S_IFDIR`, `S_IFLNK`, `S_IFCHR` are facts. `S_ISDIR(st_mode)` is the most common
///   thing done with this field and it is right.
/// * The permission bits come from the host's read-only attribute, which is the only permission
///   `std` exposes on all five targets. They are **not** an ACL evaluation, and this layer says
///   so rather than implying one. A guest testing `st_mode & S_IWUSR` before writing gets the
///   same answer `access(W_OK)` would give it, which is the consistency that matters.
/// * A symbolic link is `0o777`, which is not a derivation at all: Linux reports exactly that for
///   every symlink, because a link's own permission bits are not used.
///
/// The alternative — refusing `stat` outright because Windows has no mode word — was considered
/// and rejected. `stat` is reachable and is mostly used to ask "is this a directory" and "how big
/// is it", both of which are exact here; refusing all of it to avoid approximating one field
/// would fail a correct program to avoid a field it is not reading.
#[must_use]
pub fn mode_for(stat: &FileStat) -> u32 {
    match stat.kind {
        FileKind::Regular => {
            S_IFREG | if stat.read_only { MODE_FILE_RO } else { MODE_FILE_RW }
        }
        FileKind::Directory => {
            S_IFDIR | if stat.read_only { MODE_DIR_RO } else { MODE_DIR_RW }
        }
        FileKind::Symlink => S_IFLNK | 0o777,
        // A standard stream. Linux reports a character device, and `0o666` is what `/dev/null`
        // and a redirected stream carry.
        FileKind::Other => S_IFCHR | 0o666,
    }
}

/// Encode a [`FileStat`] into the guest's 128-byte `struct stat`.
///
/// Every field, with what it is:
///
/// | field | what |
/// |---|---|
/// | `st_dev` | one non-zero number per instance root — this layer is one filesystem |
/// | `st_ino` | [`omni_platform::fs::identity`]: stable, distinct per path, never zero |
/// | `st_mode` | [`mode_for`] |
/// | `st_nlink` | **1**, for everything. Not 2 for a directory: btrfs reports 1 for directories, so the link-count optimisation has had to tolerate it for a decade, and nothing here can create a hard link |
/// | `st_uid`, `st_gid` | **0**. There is no user here. Inventing an app uid would be a number with nothing behind it, and 0 is the only value that is not one |
/// | `st_rdev` | 0. Nothing this layer can name is a device |
/// | `st_size` | exact |
/// | `st_blksize` | [`IO_BLOCK`], which is the size this layer really transfers in |
/// | `st_blocks` | `ceil(size / 512)`, the 512-byte units the field is defined in. Derived from the size rather than from allocation, so a sparse file over-reports — the direction that makes a copy correct rather than short |
/// | the three timestamps | the host's, as seconds and nanoseconds. A time the host does not keep is zero |
fn encode_stat(stat: &FileStat, device: u64) -> [u8; STAT_BYTES] {
    let mut out = [0u8; STAT_BYTES];
    let put64 = |at: usize, value: u64, out: &mut [u8; STAT_BYTES]| {
        out[at..at + 8].copy_from_slice(&value.to_le_bytes());
    };
    let put32 = |at: usize, value: u32, out: &mut [u8; STAT_BYTES]| {
        out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    };
    put64(0, device, &mut out);
    put64(8, stat.identity, &mut out);
    put32(16, mode_for(stat), &mut out);
    put32(20, 1, &mut out);
    put32(24, 0, &mut out);
    put32(28, 0, &mut out);
    put64(32, 0, &mut out);
    put64(40, 0, &mut out);
    put64(48, stat.size, &mut out);
    put32(56, IO_BLOCK as u32, &mut out);
    put32(60, 0, &mut out);
    // `st_blocks` is in 512-byte units by definition, whatever `st_blksize` says. Rounded up,
    // and `div_ceil` rather than `(n + 511) / 512` because the second overflows for a size near
    // `u64::MAX` -- which `st_size` cannot be today and which a release build would wrap.
    put64(64, stat.size.div_ceil(512), &mut out);
    for (offset, time) in [(72, stat.accessed), (88, stat.modified), (104, stat.created)] {
        let (seconds, nanos) = match time {
            Some(duration) => (duration.as_secs(), u64::from(duration.subsec_nanos())),
            None => (0, 0),
        };
        put64(offset, seconds, &mut out);
        put64(offset + 8, nanos, &mut out);
    }
    out
}

// ================================================================== `struct statvfs`

/// Bytes of the guest's `struct statvfs` on Android arm64.
///
/// **ASSUMED: derived from bionic's `<sys/statvfs.h>` for LP64, field by field. Not verified
/// against an NDK.**
///
/// | offset | bytes | field |
/// |---|---|---|
/// | 0 | 8 | `unsigned long f_bsize` |
/// | 8 | 8 | `unsigned long f_frsize` |
/// | 16 | 8 | `fsblkcnt_t f_blocks` |
/// | 24 | 8 | `fsblkcnt_t f_bfree` |
/// | 32 | 8 | `fsblkcnt_t f_bavail` |
/// | 40 | 8 | `fsfilcnt_t f_files` |
/// | 48 | 8 | `fsfilcnt_t f_ffree` |
/// | 56 | 8 | `fsfilcnt_t f_favail` |
/// | 64 | 8 | `unsigned long f_fsid` |
/// | 72 | 8 | `unsigned long f_flag` |
/// | 80 | 8 | `unsigned long f_namemax` |
/// | 88 | 24 | `unsigned int __f_reserved[6]` |
/// | **112** | | end |
///
/// On LP64 every one of `fsblkcnt_t`, `fsfilcnt_t` and `unsigned long` is eight bytes, which is
/// what makes this layout forced once the field *order* is right — the same argument
/// `omni_bionic::time::TM_BYTES` has, and a stronger one than `FILE_BYTES` has.
pub const STATVFS_BYTES: usize = 112;

/// `ST_RDONLY`, the only `f_flag` bit this layer can answer.
pub const ST_RDONLY: u64 = 1;

/// Encode a [`VolumeStats`] into the guest's `struct statvfs`.
///
/// **`f_files`, `f_ffree` and `f_favail` are zero, and that is an answer rather than a gap.** Zero
/// is what Linux reports for a filesystem with no fixed inode table — FAT and exFAT do exactly
/// this — and NTFS has none either, because the MFT grows. Any other number would be a count of
/// something that does not exist, which is the plausible-wrong-answer class: a guest that
/// computed "inodes remaining" from an invented `f_files` would refuse to write a file for a
/// reason nobody could find.
///
/// `f_bsize` and `f_frsize` are both the allocation unit, which is true on Windows: there is one
/// cluster size and no separate fragment size. On Linux they differ and the block counts are in
/// `f_frsize` units — a difference the unix backend's documentation records, because getting it
/// backwards produces a free-space figure wrong by a small integer factor and entirely plausible.
fn encode_statvfs(stats: &VolumeStats) -> [u8; STATVFS_BYTES] {
    let mut out = [0u8; STATVFS_BYTES];
    let fields: [u64; 11] = [
        stats.block_size,
        stats.block_size,
        stats.blocks,
        stats.blocks_free,
        stats.blocks_available,
        0,
        0,
        0,
        stats.filesystem_id,
        if stats.read_only { ST_RDONLY } else { 0 },
        stats.name_max,
    ];
    for (index, value) in fields.iter().enumerate() {
        out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    out
}

// ================================================================== `struct statfs`

/// Bytes of the **kernel's** arm64 `struct statfs`, which raw syscalls 43 (`statfs`) and 44
/// (`fstatfs`) write.
///
/// **From Linux's `include/uapi/asm-generic/statfs.h`, recalled rather than read — there is no
/// kernel tree on this machine.** arm64 has no `asm/statfs.h` override, and on a 64-bit target
/// `__statfs_word` is `__kernel_long_t`, so every word is eight bytes:
///
/// | offset | bytes | field |
/// |---|---|---|
/// | 0 | 8 | `f_type` |
/// | 8 | 8 | `f_bsize` |
/// | 16 | 8 | `f_blocks` |
/// | 24 | 8 | `f_bfree` |
/// | 32 | 8 | `f_bavail` |
/// | 40 | 8 | `f_files` |
/// | 48 | 8 | `f_ffree` |
/// | 56 | 8 | `__kernel_fsid_t f_fsid` (`int val[2]`) |
/// | 64 | 8 | `f_namelen` |
/// | 72 | 8 | `f_frsize` |
/// | 80 | 8 | `f_flags` |
/// | 88 | 32 | `f_spare[4]` |
/// | **120** | | end |
///
/// bionic's LP64 `struct statfs` (`__STATFS64_BODY` in `<sys/statfs.h>`) is the same twelve
/// fields in the same order with `uint64_t`/`fsblkcnt_t`/`fsfilcnt_t` words, which is what makes
/// the offsets forced once the order is right. `f_frsize` sitting **after** `f_namelen` is the
/// field most easily put in `statvfs`'s place instead.
pub const STATFS_BYTES: usize = 120;

/// `EXT4_SUPER_MAGIC`, the `f_type` answered for every volume under the root.
///
/// **A choice between two true answers, made for the installed base.** An app's data directory
/// is ext4 (`0xEF53`) on most devices shipped to date and f2fs (`0xF2F52010`) on many newer ones;
/// the host's NTFS is neither and has no Linux magic a guest would recognise. ext4 is the value
/// code that branches on `f_type` is most likely to have been written against — the usual reason
/// to ask is to tell a local disk from FUSE, NFS or a network share, and both candidates answer
/// "local disk" to that question identically. External storage's `Android/data` is bind-mounted
/// from the lower filesystem on Android 11+, so the same answer holds there.
const EXT4_SUPER_MAGIC: u64 = 0xEF53;
/// `PROC_SUPER_MAGIC`: a descriptor on, or a path to, a file this layer generates under `/proc`.
const PROC_SUPER_MAGIC: u64 = 0x9FA0;
/// `PIPEFS_MAGIC` (`"PIPE"`).
const PIPEFS_MAGIC: u64 = 0x5049_5045;
/// `SOCKFS_MAGIC` (`"SOCK"`).
const SOCKFS_MAGIC: u64 = 0x534F_434B;
/// `ANON_INODE_FS_MAGIC`, which eventfd, epoll and timerfd descriptors all live on.
const ANON_INODE_FS_MAGIC: u64 = 0x0904_1934;
/// `ST_VALID`: the kernel sets it in every `statfs` answer to say `f_flags` means something.
/// bionic's `statvfs` strips it, which is why [`encode_statvfs`] never writes it.
const ST_VALID: u64 = 0x0020;
/// `ST_RELATIME`, from `/proc`'s default mount (Android's init passes no atime flag, and the
/// kernel then defaults a new mount to `relatime`).
const ST_RELATIME: u64 = 0x1000;
/// `NAME_MAX`, which `simple_statfs` puts in a pseudo filesystem's `f_namelen`.
const PSEUDO_NAME_MAX: u64 = 255;

/// The eleven meaningful words of a `struct statfs`, before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatfsAnswer {
    kind: u64,
    block_size: u64,
    blocks: u64,
    blocks_free: u64,
    blocks_available: u64,
    files: u64,
    files_free: u64,
    fsid: u64,
    name_max: u64,
    fragment_size: u64,
    flags: u64,
}

impl StatfsAnswer {
    /// A volume under the root: the host's own numbers, as [`encode_statvfs`] reports them.
    ///
    /// The inode counts are zero for [`encode_statvfs`]'s reason, and `f_flags` is that call's
    /// `ST_RDONLY` plus the `ST_VALID` the kernel always adds — so `statvfs` and `statfs` on one
    /// path agree on every field they share, as bionic's `statvfs` (a wrapper over `statfs`)
    /// makes them agree on a device.
    fn volume(stats: &VolumeStats) -> Self {
        Self {
            kind: EXT4_SUPER_MAGIC,
            block_size: stats.block_size,
            blocks: stats.blocks,
            blocks_free: stats.blocks_free,
            blocks_available: stats.blocks_available,
            files: 0,
            files_free: 0,
            fsid: stats.filesystem_id,
            name_max: stats.name_max,
            fragment_size: stats.block_size,
            flags: ST_VALID | if stats.read_only { ST_RDONLY } else { 0 },
        }
    }

    /// A kernel pseudo filesystem, answered as the kernel's `simple_statfs` answers it:
    /// `f_bsize` (and so `f_frsize`) is the page size, `f_namelen` is `NAME_MAX`, and every count
    /// and the fsid are zero — which is not a gap here, because there is no volume behind a pipe,
    /// a socket or `/proc` for a count to be of.
    fn pseudo(kind: u64, page_size: u64, flags: u64) -> Self {
        Self {
            kind,
            block_size: page_size,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: 0,
            name_max: PSEUDO_NAME_MAX,
            fragment_size: page_size,
            flags,
        }
    }

    /// Encode into the kernel's layout; see [`STATFS_BYTES`].
    ///
    /// `f_fsid` is the 64-bit identifier split little-end first into `val[0]` and `val[1]`, which
    /// is ext4's `u64_to_fsid` and is also how bionic's `statvfs` rebuilds its `unsigned long`
    /// from the two ints — so the two calls report one identifier.
    fn encode(&self) -> [u8; STATFS_BYTES] {
        let mut out = [0u8; STATFS_BYTES];
        let words = [
            self.kind,
            self.block_size,
            self.blocks,
            self.blocks_free,
            self.blocks_available,
            self.files,
            self.files_free,
            self.fsid,
            self.name_max,
            self.fragment_size,
            self.flags,
        ];
        for (index, value) in words.iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

// ================================================================== `struct dirent`

/// Bytes of the guest's `struct dirent` on Android arm64.
///
/// **ASSUMED: derived from bionic's `<dirent.h>` for LP64, field by field. Not verified against
/// an NDK.**
///
/// | offset | bytes | field |
/// |---|---|---|
/// | 0 | 8 | `uint64_t d_ino` |
/// | 8 | 8 | `int64_t d_off` |
/// | 16 | 2 | `unsigned short d_reclen` |
/// | 18 | 1 | `unsigned char d_type` |
/// | 19 | 256 | `char d_name[256]` |
/// | 275 | 5 | padding to the structure's 8-byte alignment |
/// | **280** | | end |
///
/// bionic's `dirent` and `dirent64` are the same structure on LP64. `d_name` is a fixed 256-byte
/// array rather than a flexible member, which is why [`omni_platform::fs::NAME_MAX`] (255) is the
/// longest name `readdir` can return and why a longer one is refused by name rather than
/// truncated.
pub const DIRENT_BYTES: usize = 280;

/// Offset of `d_name` inside a `struct dirent`.
const D_NAME_OFFSET: usize = 19;
/// Bytes of `d_name`, including room for the terminator.
const D_NAME_BYTES: usize = 256;

/// `DT_DIR`.
const DT_DIR: u8 = 4;
/// `DT_REG`.
const DT_REG: u8 = 8;
/// `DT_LNK`.
const DT_LNK: u8 = 10;
/// `DT_UNKNOWN` — the honest answer for something this layer has no `DT_*` for, and one every
/// caller must already handle because several real filesystems return it for everything.
const DT_UNKNOWN: u8 = 0;

/// Encode one directory entry into the guest's `struct dirent`.
///
/// `d_off` is the index of the **next** entry. It is opaque to the guest by contract — only
/// `seekdir`/`telldir` interpret it, and neither is in the reachable import set — so an index is
/// as good as a byte offset and is the one that cannot be mistaken for a file position.
///
/// `d_reclen` is the whole fixed record. On a real device it is variable, because bionic fills
/// these from `getdents64` buffers; a fixed-size record is a conforming reading (the field means
/// "the length of this record") and it is what this layer really writes.
fn encode_dirent(entry: &DirEntryInfo, next: u64) -> [u8; DIRENT_BYTES] {
    let mut out = [0u8; DIRENT_BYTES];
    out[0..8].copy_from_slice(&entry.identity.to_le_bytes());
    out[8..16].copy_from_slice(&next.to_le_bytes());
    out[16..18].copy_from_slice(&(DIRENT_BYTES as u16).to_le_bytes());
    out[18] = match entry.kind {
        FileKind::Regular => DT_REG,
        FileKind::Directory => DT_DIR,
        FileKind::Symlink => DT_LNK,
        FileKind::Other => DT_UNKNOWN,
    };
    // The name is bounded by `NAME_MAX` (255) at the seam, so it fits with its NUL. Truncating
    // here would be a silent wrong answer; the seam refuses instead, which is why this slice
    // cannot be short.
    let name = entry.name.as_bytes();
    let take = name.len().min(D_NAME_BYTES - 1);
    out[D_NAME_OFFSET..D_NAME_OFFSET + take].copy_from_slice(&name[..take]);
    out
}

// ================================================================== errno mapping

/// The guest `errno` for a classified host failure, or `None` when there is not one.
///
/// **`FsErrorKind::Other` deliberately has no errno.** It is the kind `std::io::ErrorKind` could
/// not classify, and giving it `EIO` would hand guest code a specific, actionable failure for
/// something nobody identified — guest code retries `EIO`, reports it and moves on. A refusal
/// naming the symbol and the host's own message is what a reader three thousand initializers deep
/// can act on.
#[must_use]
pub fn errno_for(kind: FsErrorKind) -> Option<i32> {
    Some(match kind {
        FsErrorKind::NotFound => consts::ENOENT,
        FsErrorKind::PermissionDenied => consts::EACCES,
        FsErrorKind::AlreadyExists => consts::EEXIST,
        FsErrorKind::NotADirectory => consts::ENOTDIR,
        FsErrorKind::IsADirectory => consts::EISDIR,
        FsErrorKind::DirectoryNotEmpty => consts::ENOTEMPTY,
        FsErrorKind::InvalidInput => consts::EINVAL,
        FsErrorKind::StorageFull => consts::ENOSPC,
        FsErrorKind::FileTooLarge => consts::EFBIG,
        FsErrorKind::TooManyOpenFiles => consts::EMFILE,
        FsErrorKind::ReadOnlyFilesystem => consts::EROFS,
        FsErrorKind::BadDescriptor => consts::EBADF,
        FsErrorKind::NameTooLong => consts::ENAMETOOLONG,
        // **Added in M5, when `pipe` made a descriptor that can block.** Both are the guest's own
        // answers for a pipe and both have a branch in every correct caller: `EAGAIN` for a
        // non-blocking end with nothing to do, `EPIPE` for a write whose readers have all gone.
        FsErrorKind::WouldBlock => consts::EAGAIN,
        FsErrorKind::BrokenPipe => consts::EPIPE,
        // Added with `lseek`'s seam method, for the stream seeks SQLite's neighbour reached.
        FsErrorKind::NotSeekable => consts::ESPIPE,
        // `epoll_ctl` on a regular file or a directory, which the kernel refuses as EPERM.
        FsErrorKind::NotPollable => consts::EPERM,
        // `FsErrorKind::Other` and nothing else. It is spelled as a wildcard because the enum is
        // `#[non_exhaustive]`, and a kind added upstream without a decision here must refuse by
        // name rather than acquire a plausible errno.
        _ => return None,
    })
}

/// What one filesystem call produced: a value, or an `errno` the guest is to be told.
pub(super) enum Settled<T> {
    /// The call succeeded.
    Done(T),
    /// The call failed the way a real device fails, and this is the `errno` to report.
    Failed(i32),
}

/// Turn a seam result into either a value or an `errno`, refusing what cannot be either.
///
/// This is the one place the `-1`/refusal split described in this module's documentation is made,
/// so the split is a function rather than a rule repeated eighteen times.
pub(super) fn settle<T>(view: &GuestView<'_>, result: FsResult<T>) -> AbiResult<Settled<T>> {
    match result {
        Ok(value) => Ok(Settled::Done(value)),
        Err(error) => match error.kind().and_then(errno_for) {
            Some(errno) => Ok(Settled::Failed(errno)),
            None => Err(view.refusal(error.to_string())),
        },
    }
}

/// The instance's filesystem, or a refusal naming the method that would supply one.
pub(super) fn filesystem<'a>(view: &GuestView<'a>) -> AbiResult<&'a Filesystem> {
    view.active.bionic.filesystem().ok_or_else(|| {
        view.refusal(
            "this guest instance has no filesystem root. Every guest path -- `/data/...`, \
             `/system/...`, `/proc/...` -- is resolved inside one host directory the embedding \
             supplies with `Bionic::set_filesystem_root`, and none has been supplied. There is \
             deliberately no default: a default root would have to be the process's working \
             directory or a temporary one, and either would let untrusted guest code read and \
             write host files nobody decided to expose (D6: the APK under test is cheat-injected \
             and the executor is treated as hostile)",
        )
    })
}

/// Read a guest path argument as bytes.
///
/// Goes through [`GuestMem::cstr`](crate::mem::GuestMem::cstr), so it is already bounded by that
/// function's 64 KiB `STRING_LIMIT` and an unterminated path is
/// [`AbiError::Unterminated`](crate::AbiError::Unterminated) rather than a scan off the end of a
/// mapping. The seam then applies `PATH_MAX` on top, which is the guest's own limit.
pub(super) fn path_for(view: &GuestView<'_>, pointer: u64, argument: usize) -> AbiResult<Vec<u8>> {
    if pointer == 0 {
        // POSIX: a null path is `EFAULT`. It arrives here as a refusal rather than as `-1`,
        // for `clocks`' stated reason -- guest code that ignores the return of a `stat` would
        // otherwise carry an unwritten structure forward with nothing to say what happened.
        return Err(view.refusal(format!("argument {argument} is a null path pointer")));
    }
    let at = guest_address(view, pointer)?;
    view.mem().cstr(at, Blame::new(view.symbol(), view.address(), argument))
}

/// Narrow a guest pointer to a host address, refusing rather than truncating.
fn guest_address(view: &GuestView<'_>, pointer: u64) -> AbiResult<GuestAddr> {
    GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))
}

/// Write a fixed-size structure into guest memory as one access.
///
/// One write rather than field by field, so a destination that is only partly writable leaves the
/// guest nothing rather than half a `struct stat` — the same reasoning `clocks::write_pair` gives.
fn write_struct(view: &GuestView<'_>, at: u64, bytes: &[u8], argument: usize) -> AbiResult<()> {
    let address = guest_address(view, at)?;
    view.mem().write_bytes(address, bytes, Blame::new(view.symbol(), view.address(), argument))
}

/// Admit the **whole** of a guest transfer buffer, and hand back its base address.
///
/// D22's rule for `arc4random_buf` — *validate the entire destination before producing a single
/// byte* — spelled once so the three descriptor transfers share it rather than restate it. The
/// argument for doing it this way, and the limits of what it buys, are in [`read_into_guest`].
///
/// Returning the base address is the other half: every chunk of the loop is then `base + done`
/// inside a range already admitted, which is why the loops here no longer add a guest-chosen
/// `count` onto a guest-chosen pointer. That sum was `buffer + done` on `u64`, and `VERIFICATION`
/// entry 3 is about what a release build does with arithmetic on guest-controlled values.
fn transfer_buffer(
    view: &GuestView<'_>,
    buffer: u64,
    count: u64,
    write: bool,
    argument: usize,
) -> AbiResult<GuestAddr> {
    let at = guest_address(view, buffer)?;
    // The same target-width refusal [`guest_address`] makes for a pointer, made for a length. On
    // an LP64 host it cannot fire, because every caller has already rejected a `count` above
    // `MAX_COUNT`; on a 32-bit one it is the difference between a refusal and a truncated length
    // that would admit less memory than the transfer goes on to touch.
    let len = usize::try_from(count)
        .map_err(|_| view.refusal("a transfer length wider than the host's usize"))?;
    view.mem().checked_ptr(at, len, write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(at)
}

// ================================================================== opening

/// Turn the guest's `O_*` word into [`OpenFlags`], refusing what cannot be honoured.
///
/// Three groups, and the third is the one that matters:
///
/// * **Honoured**: the access mode, `O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_APPEND`, `O_DIRECTORY`.
/// * **Accepted with nothing to do**, each because what it asks for is already true here:
///   `O_NOCTTY` (no controlling terminal exists), `O_NONBLOCK` (a no-op on a regular file on
///   Linux too), `O_LARGEFILE` (always in effect on LP64), `O_NOFOLLOW` (no path component may
///   be a symlink, which is stronger), `O_NOATIME` (advisory), `O_CLOEXEC` (nothing execs; recorded for `F_GETFD`).
///   Ignoring these is conforming rather than convenient — each one's *contract* is satisfied.
/// * **Refused by name**: `O_SYNC` and `O_DSYNC` promise the data is durable when the write
///   returns, which this layer does not do; `O_DIRECT` promises the page cache is bypassed;
///   `O_PATH` and `O_TMPFILE` ask for a different object entirely; `FASYNC` asks for a signal.
///   Each has a *believable* wrong answer available — accept it and do nothing — and each would
///   be a promise broken silently. This is `guestmem`'s `mlock` decision applied to flags.
///
/// A bit nobody has defined is refused with the bits named, rather than masked away.
fn parse_open_flags(view: &GuestView<'_>, flags: i32) -> AbiResult<OpenFlags> {
    let access = flags & O_ACCMODE;
    let (read, write) = match access {
        O_RDONLY => (true, false),
        O_WRONLY => (false, true),
        O_RDWR => (true, true),
        _ => {
            // `O_ACCMODE` itself (3) is not an access mode; Linux reports EINVAL, and so does the
            // seam, so this is left to the seam rather than duplicated here.
            (false, false)
        }
    };
    for (bit, name, why) in [
        (O_SYNC, "O_SYNC", "the data is on stable storage when the write returns"),
        (O_DSYNC, "O_DSYNC", "the data is on stable storage when the write returns"),
        (O_DIRECT, "O_DIRECT", "transfers bypass the page cache"),
        (O_PATH, "O_PATH", "the descriptor refers to the path and not to the open file"),
        (O_TMPFILE, "O_TMPFILE", "an unnamed temporary file is created in the directory"),
        (O_ASYNC, "FASYNC", "SIGIO is delivered when the descriptor becomes ready"),
    ] {
        // `O_SYNC` and `O_TMPFILE` each contain another flag's bits, so the test is for the whole
        // pattern rather than for any bit of it.
        if flags & bit == bit {
            return Err(view.refusal(format!(
                "the guest opened a file with `{name}`, which promises that {why}. This layer \
                 cannot make that promise, and accepting the flag and doing nothing would break \
                 it silently -- which is the answer `mlock` was refused for (D21)"
            )));
        }
    }
    let known = HONOURED | ALREADY_TRUE | O_SYNC | O_DSYNC | O_DIRECT | O_PATH | O_TMPFILE | O_ASYNC;
    let unknown = flags & !known;
    if unknown != 0 {
        return Err(view.refusal(format!(
            "the guest opened a file with the flag bits {unknown:#o}, which this layer has no \
             name for. Masking them away would carry out a call that asked for something nobody \
             here understood"
        )));
    }
    Ok(OpenFlags {
        read,
        write,
        create: flags & O_CREAT != 0,
        exclusive: flags & O_EXCL != 0,
        truncate: flags & O_TRUNC != 0,
        append: flags & O_APPEND != 0,
        directory: flags & O_DIRECTORY != 0,
    })
}

/// `int open(const char *pathname, int flags, ... /* mode_t mode */)`
///
/// **Variadic, and the third argument is read from `X2`.** AAPCS64's *Linux* variant passes
/// anonymous arguments in the ordinary registers, so `mode` is in `X2` exactly as a third fixed
/// argument would be. (Apple's arm64 puts every variadic argument on the stack, which D18 records
/// as one of the four places the ARM64 path nearly closed — but that is a rule about the *host*
/// ABI when Omnidroid calls out, not about the Android guest calling in.) The register is read
/// only when `O_CREAT` is set, because a caller that did not pass a mode left whatever was there.
///
/// **`mode` is read and not applied**, and that is worth stating plainly: Windows has no POSIX
/// permission bits, so a file the guest asked to create with `0600` is created with the
/// permissions it inherits from the instance's root directory. The confinement boundary is the
/// root, and protecting the root is the host operator's job; within it, this layer cannot make
/// one guest file less readable than another. It is recorded in the refusal-free path rather than
/// turned into a refusal, because refusing every `open(O_CREAT)` would stop the engine writing
/// any file at all.
pub(super) fn open(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, flags, _mode) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let parsed = parse_open_flags(&view, flags)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.open(&bytes, parsed))? {
            Settled::Done(fd) => record_close_on_exec(&view, fs, fd, flags & O_CLOEXEC != 0)?,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// Record `O_CLOEXEC` (or its per-call spelling) on a descriptor just made, and hand the
/// descriptor back. The flag is the descriptor's; see [`Filesystem::is_close_on_exec`].
pub(super) fn record_close_on_exec(
    view: &GuestView<'_>,
    fs: &Filesystem,
    fd: i32,
    on: bool,
) -> AbiResult<i32> {
    if on {
        if let Settled::Failed(errno) = settle(view, fs.set_close_on_exec(fd, true))? {
            // The descriptor was made a moment ago under the same table; losing it here is
            // this layer's defect, and it says so rather than returning a descriptor without
            // the flag the caller asked for.
            return Err(view.refusal(format!(
                "descriptor {fd} was just created and then not found to record FD_CLOEXEC on                  (errno {errno})"
            )));
        }
    }
    Ok(fd)
}

/// `int __open_2(const char *pathname, int flags)`
///
/// bionic's FORTIFY form of `open`. It is **not** variadic — HANDOFF records that explicitly —
/// and its whole purpose is to catch a call that passes `O_CREAT` without the `mode` argument
/// `O_CREAT` requires. On a device that is `__fortify_fatal`; here it is a refusal naming the
/// symbol, which is the same treatment every other `_chk` function gets.
pub(super) fn open_2(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, flags) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if flags & O_CREAT != 0 || flags & O_TMPFILE == O_TMPFILE {
            return Err(view.refusal(
                "the guest called `__open_2` with O_CREAT or O_TMPFILE. That form takes no `mode` \
                 argument, so the file would be created with whatever was in the register -- \
                 which is why bionic's FORTIFY build calls `__fortify_fatal` here. The call site \
                 is a miscompiled or hand-written `open` with a missing third argument",
            ));
        }
        let bytes = path_for(view.blaming(0), path, 0)?;
        let parsed = parse_open_flags(&view, flags)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.open(&bytes, parsed))? {
            Settled::Done(fd) => record_close_on_exec(&view, fs, fd, flags & O_CLOEXEC != 0)?,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int close(int fd)`
pub(super) fn close(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let fd = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        // A `FILE *` built over this descriptor would be left pointing at a closed one, which is
        // exactly what closing a descriptor out from under a stream does on a real device. The
        // stream's next operation reports `EBADF`, which is the contract.
        match settle(&view, fs.close(fd))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== transfers

/// Read `count` bytes from `fd` into guest memory, in [`IO_BLOCK`] pieces.
///
/// Returns the bytes transferred, or the `errno` for a failure that happened before any byte was.
/// A failure *after* some bytes have been transferred reports the short count, which is what
/// `read(2)` does: the error is reported by the next call.
///
/// # The whole destination is admitted before the descriptor is touched
///
/// Adapter review finding **M1**, and it is D22's `arc4random_buf` rule carried across: *nothing
/// irreversible happens until the guest-supplied destination has been validated.* Phase 3b's loop
/// read a chunk from the descriptor and **then** tried to place it, so a buffer writable for its
/// first page and not its second took bytes out of the descriptor and reported the call as a
/// failure. **The believable wrong answer that hid it** is that the guest was told the call
/// failed, so nothing was read — true of the *return value*, false of the descriptor, and guest
/// memory looked clean either way because the placing access is checked before it copies.
///
/// **For a pipe those bytes are gone.** A pipe read is destructive and there is nothing to seek
/// back to. The glue's command pipe carries one `APP_CMD_*` byte per message (§5.2), and a lost
/// one is a guest that waits forever rather than a guest that retries.
///
/// ## A regular file could be recovered from, and is still treated the same way
///
/// A `read` on a regular file *could* be undone by seeking the offset back; a `read` on a pipe
/// could not. One rule covers both, and the reasons are worth naming because the asymmetry is
/// real:
///
/// * the recovery exists for only one of the two descriptor kinds, so a design resting on it
///   leaves the destructive kind — the one that loses data — uncovered, and this layer would have
///   to branch on a kind it does not otherwise need to know;
/// * a seek back is not a restoration when the descriptor is shared. Nothing stops two guest
///   threads reading one descriptor, so rewinding after another thread's `read` has moved the
///   offset corrupts *that* read instead of repairing this one;
/// * admitting the destination first costs one range check per call, happens before any host call
///   at all, and is the same sentence for both kinds.
///
/// ## Why the whole destination rather than chunk by chunk
///
/// The loop moves [`IO_BLOCK`] at a time so that a guest-chosen `count` never becomes a host-side
/// buffer of that size — the reason the chunking is here, and it is unchanged. Validating each
/// chunk just before its own descriptor read would keep the invariant too, and would deliver the
/// chunks that do fit. It was rejected: the only way to report the rest is a **short count**, and
/// a short `read` is precisely how a drained pipe and an ended file announce themselves. A guest
/// whose buffer is half unmapped would be told *that is all there is* — a plausible wrong answer,
/// which Global Constraint 1 forbids — where admitting the whole buffer tells it by name, with
/// the address and the length, that its own destination is not writable.
///
/// ## What this does **not** promise
///
/// It promises that **no byte leaves the descriptor unless the entire destination was writable at
/// the moment it was checked.** It does not promise the destination is still writable when the
/// bytes arrive. Another guest thread may `munmap` or `mprotect` the range between the check and
/// the write — `guestmem` binds both — and no check on this side of the boundary can close that
/// window, because the window *is* the transfer. When it happens the bytes already read are lost
/// and the call refuses by name. That is a property of guest memory rather than of this function,
/// and [`GuestMem::checked_ptr`](crate::mem::GuestMem::checked_ptr) says the same of its own
/// result: the check has happened, and no claim is made about other guest threads.
fn read_into_guest(
    view: &mut GuestView<'_>,
    fd: i32,
    buffer: u64,
    count: u64,
    offset: Option<u64>,
) -> AbiResult<Settled<i64>> {
    let fs = filesystem(view)?;
    // Before a byte leaves the descriptor. See this function's documentation for what that does
    // and does not buy.
    let base = transfer_buffer(view, buffer, count, true, 1)?;
    let mut blocking = BlockingWait::new();
    let mut done = 0u64;
    let mut chunk = vec![0u8; IO_BLOCK];
    while done < count {
        blocking.observe(fs);
        let want = ((count - done) as usize).min(IO_BLOCK);
        let read = match offset {
            None => fs.read(fd, &mut chunk[..want]),
            Some(base) => {
                // The guest chose the offset and the count, so the sum is guest arithmetic:
                // `checked_add` rather than `+`, because a release build wraps and a wrapped
                // offset reads the wrong part of the file with every call reporting success.
                let Some(at) = base.checked_add(done) else {
                    return Ok(Settled::Failed(consts::EINVAL));
                };
                fs.pread(fd, &mut chunk[..want], at)
            }
        };
        match settle(view, read)? {
            Settled::Done(0) => break,
            Settled::Done(got) => {
                // `done < count` and `[base, base + count)` was admitted above, so this offset
                // is inside a range this call has already checked and needs no arithmetic guard
                // of its own — the bound is the validation, not the addition.
                let at = base + done as usize;
                view.mem().write_bytes(
                    at,
                    &chunk[..got],
                    Blame::new(view.symbol(), view.address(), 1),
                )?;
                done += got as u64;
            }
            Settled::Failed(errno) => {
                if done == 0 {
                    if blocking.should_wait(fs, fd, errno) {
                        blocking.wait(view, fs, fd)?;
                        continue;
                    }
                    return Ok(Settled::Failed(errno));
                }
                // **A pipe that gave some bytes and then had no more is a complete `read`.**
                // POSIX: a read from a pipe returns as soon as at least one byte is available,
                // and never waits for the rest of the buffer. Waiting here would turn a
                // one-byte `APP_CMD` into a hang.
                break;
            }
        }
    }
    // `done <= count <= MAX_COUNT`, so the cast cannot make a negative count.
    Ok(Settled::Done(done as i64))
}

/// The wait a **blocking** descriptor owes when the seam says the call would block.
///
/// `omni-platform`'s pipe never waits and never decides how long a guest may block — that is this
/// layer's policy, and it is the one `poll`, `select`, `nanosleep` and a blocking socket apply:
/// **as long as a device would** -- here, until the descriptor is ready -- in parks of at most
/// [`STOP_SLICE`], with the stop switch read between them.
///
/// **It used to give up after [`MAX_SLEEP_SECONDS`](super::MAX_SLEEP_SECONDS) and refuse**, for
/// D16's reason: a sleeping thread executes no guest instructions, so no step budget can end one,
/// and a blocking `read` on a pipe nobody writes to would be a host thread nothing could stop. A
/// refusal ends the guest thread, though, and a writer that is slow is not a guest defect. What
/// answers D16 now is [`Bionic::stop_guest_threads`](super::Bionic::stop_guest_threads): a
/// thread waiting here is ended by teardown within one park, by name, and by nothing else.
///
/// **Non-blocking is the common case on the startup path and costs nothing**: §5.2 sets
/// `O_NONBLOCK` on both ends of both of the glue's pipes, so this type never waits there.
struct BlockingWait {
    /// The readiness generation as it was **before** the attempt that is about to be made.
    ///
    /// This is the whole of why the wait cannot lose a wakeup. A write that lands between the
    /// seam saying `EAGAIN` and the wait starting raises the generation past this value, so the
    /// wait returns at once instead of sleeping through the event it was waiting for. Reading the
    /// generation *after* the failed attempt is the other order, and it is the 1.0104 s lost
    /// wakeup `VERIFICATION.md` entry 11 records.
    seen: u64,
}

impl BlockingWait {
    fn new() -> BlockingWait {
        BlockingWait { seen: 0 }
    }

    /// Record the generation before an attempt. Called on every pass of the transfer loop.
    fn observe(&mut self, fs: &Filesystem) {
        self.seen = fs.ready_generation();
    }

    /// Whether this failure is one a blocking descriptor waits out rather than reports.
    fn should_wait(&self, fs: &Filesystem, fd: i32, errno: i32) -> bool {
        errno == consts::EAGAIN && fs.is_nonblocking(fd).is_ok_and(|nonblocking| !nonblocking)
    }

    /// Wait one park for the descriptor's readiness to change, or refuse by name if the runtime
    /// is stopping.
    ///
    /// **After the attempt, not before it**: the caller tried the transfer and the seam said it
    /// would block, so a descriptor that was ready has already been answered, stopping or not.
    fn wait(&mut self, view: &GuestView<'_>, fs: &Filesystem, fd: i32) -> AbiResult<()> {
        if view.active.bionic.guest_threads_stopping() {
            return Err(view.refusal(format!(
                "a blocking `{}` on fd {fd} was waiting for the descriptor to become ready when \
                 this runtime asked its guest threads to stop. It never became ready, so there is \
                 no value to return that would be true: a short count or EAGAIN would tell a \
                 blocking descriptor something only a non-blocking one can be told, and \
                 -1/EINTR would name a signal that was never delivered because this runtime has \
                 no signal delivery",
                view.symbol()
            )));
        }
        fs.wait_for_readiness(self.seen, STOP_SLICE);
        Ok(())
    }
}

/// `ssize_t read(int fd, void *buf, size_t count)`
///
/// The whole of `buf` is admitted before the descriptor is read from. [`read_into_guest`] is
/// where review finding **M1** was closed and where what that does and does not promise — the
/// pipe, the regular file, and the cross-thread race it cannot close — is written out.
pub(super) fn read(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = transfer(c, &state, |view| {
        if count > MAX_COUNT {
            return Ok(Settled::Failed(consts::EINVAL));
        }
        if count == 0 {
            // C: zero bytes, and the descriptor is not even checked for readability by POSIX.
            // The seam is not called at all, so a zero-length read at a null pointer -- which is
            // legal C -- does not fault.
            return Ok(Settled::Done(0));
        }
        read_into_guest(view, fd, buf, count, None)
    })?;
    c.ret().u64(result as u64);
    Ok(())
}

/// `ssize_t pread(int fd, void *buf, size_t count, off_t offset)`
///
/// Same destination rule as [`read`], through the same [`read_into_guest`]. `pread` does not move
/// the descriptor's offset, so a failed one is the *least* costly of the three to get wrong —
/// which is exactly why it is stated here rather than left to be inferred: the rule is about the
/// order of the two steps, not about how much a particular descriptor loses.
pub(super) fn pread(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count, offset) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()? as i64)
    };
    let state = active(c.symbol(), c.address())?;
    let result = transfer(c, &state, |view| {
        if count > MAX_COUNT || offset < 0 {
            // A negative offset is `EINVAL` on Linux, and it is the value a guest that computed
            // one by subtraction would arrive with.
            return Ok(Settled::Failed(consts::EINVAL));
        }
        if count == 0 {
            return Ok(Settled::Done(0));
        }
        read_into_guest(view, fd, buf, count, Some(offset as u64))
    })?;
    c.ret().u64(result as u64);
    Ok(())
}

/// `off_t lseek(int fd, off_t offset, int whence)`
///
/// `Filesystem::seek`, which `fseeko` already answers through: `EINVAL` for an unknown `whence`
/// or a position that would be negative, `ESPIPE` for a pipe or a socket, `EBADF` for no such
/// descriptor. Bound for raw syscall 62, which the boundary answers through this import: a
/// TaskScheduler worker's raw-`SVC` file sequence reached `openat`, `fstatfs` and then `read`
/// (2026-09-25), and a scan that reads a file by offset seeks it.
pub(super) fn lseek(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, offset, whence) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()? as i64, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        match settle(&view, fs.seek(fd, offset, whence))? {
            Settled::Done(position) => i64::try_from(position).map_err(|_| {
                view.refusal(format!(
                    "lseek({fd}) reached position {position}, past the largest off_t; Linux \
                     answers EOVERFLOW, which no guest file here can be large enough to need"
                ))
            })?,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().u64(result as u64);
    Ok(())
}

/// `int ftruncate(int fd, off_t length)`
///
/// `Filesystem::ftruncate`, with the one check that belongs on this side of the seam: a negative
/// `off_t` is `EINVAL` before the descriptor is looked at, which is the kernel's order.
///
/// MEASURED reader: a guest worker (thread 34 in the run that found it) once the engine's storage
/// layer had initialised on the settings success path. The thread died on the `Unbound` this
/// replaces.
pub(super) fn ftruncate(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, length) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()? as i64)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if length < 0 {
            view.set_errno(consts::EINVAL);
            -1
        } else {
            let fs = filesystem(&view)?;
            match settle(&view, fs.ftruncate(fd, length as u64))? {
                Settled::Done(()) => 0,
                Settled::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int posix_fallocate(int fd, off_t offset, off_t len)`
///
/// bionic's is `fallocate(fd, 0, offset, len)` with the failure **returned** rather than left in
/// `errno` -- an `ErrnoRestorer` puts `errno` back -- so every answer here is the return value and
/// `errno` is never written. A negative offset or a length that is not positive is `EINVAL`, the
/// kernel's first check; the rest is `Filesystem::fallocate`.
///
/// MEASURED reader: a guest worker once the engine was creating its Vulkan device (thread 12 of
/// the run that found it), which died on the `Unbound` this replaces. That run then could not be
/// stopped: two threads sat in `pthread_mutex_lock` past the 60-second stop.
pub(super) fn posix_fallocate(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, offset, len) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()? as i64, a.next_u64()? as i64)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let view = enter(c, &state);
        if offset < 0 || len <= 0 {
            consts::EINVAL
        } else {
            let fs = filesystem(&view)?;
            match settle(&view, fs.fallocate(fd, offset as u64, len as u64))? {
                Settled::Done(()) => 0,
                Settled::Failed(errno) => errno,
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int fsync(int fd)`
///
/// The seam's `File::sync_all` for a regular file, `EINVAL` for a descriptor with nothing to
/// synchronise, and a refusal by name for a directory -- see `Filesystem::fsync`.
///
/// MEASURED reader: the engine's SQLite, on the thread that took the record lock and wrote its
/// pages -- a transaction's commit. The guest thread died on the `Unbound` this replaces.
pub(super) fn fsync(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let fd = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        match settle(&view, fs.fsync(fd))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `ssize_t pwrite(int fd, const void *buf, size_t count, off_t offset)`
///
/// [`write`] at an offset, through the same [`write_from_guest`] -- so the same rule holds: the
/// whole source is admitted before a byte reaches the file. The descriptor's own offset is not
/// moved, which is `Filesystem::pwrite`'s job and was measured to need doing on Windows.
///
/// MEASURED reader: the engine's embedded SQLite, on the thread that had taken its record lock
/// and asked `geteuid` -- SQLite writes its pages with `pwrite` where the platform has it, which
/// Android does. The guest thread died on the `Unbound` this replaces.
pub(super) fn pwrite(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count, offset) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()? as i64)
    };
    let state = active(c.symbol(), c.address())?;
    let result = transfer(c, &state, |view| {
        if count > MAX_COUNT || offset < 0 {
            // A negative offset is `EINVAL` on Linux, as for `pread`.
            return Ok(Settled::Failed(consts::EINVAL));
        }
        if count == 0 {
            return Ok(Settled::Done(0));
        }
        write_from_guest(view, fd, buf, count, Some(offset as u64))
    })?;
    c.ret().u64(result as u64);
    Ok(())
}

/// Write `count` bytes of guest memory to `fd`, in [`IO_BLOCK`] pieces.
///
/// Returns the bytes transferred, or the `errno` for a failure that happened before any byte was.
/// A failure *after* some bytes have gone reports the short count, which is what `write(2)` does:
/// the error is reported by the next call.
///
/// **A short write is a real answer here, and it was not before.** Until a pipe existed, every
/// descriptor took whatever it was given; a pipe takes what fits. `Settled::Done(0)` therefore
/// ends the loop rather than spinning — a descriptor that took nothing and reported no error has
/// no more room, and the count so far is what the guest is told.
///
/// # The whole source is admitted before the descriptor is touched
///
/// The mirror of [`read_into_guest`]'s rule, and the reason finding **M1** named `__write_chk`
/// beside `read` and `pread`. Here the irreversible side effect is on the far side: phase 3b's
/// loop copied one [`IO_BLOCK`] out of guest memory, handed it to the descriptor, and only then
/// looked at the next chunk — so a source readable for its first page and not its second put a
/// page into a **pipe** and then reported the whole call as a failure. A byte in a pipe cannot be
/// taken back out; the reader has already been told a message started, and for the glue's command
/// pipe half a message is a command.
///
/// The one-chunk case hid it, which is why the regression test has to cross a chunk boundary: for
/// `count <= IO_BLOCK` the single `read_bytes` already failed before any host write, so the defect
/// is invisible below 4 KiB and certain above it.
///
/// The same two limits apply as for the read direction: the promise is that **no byte reaches the
/// descriptor unless the entire source was readable at the moment it was checked**, and a range
/// another guest thread unmaps mid-transfer is a race no check here can close.
fn write_from_guest(
    view: &mut GuestView<'_>,
    fd: i32,
    buf: u64,
    count: u64,
    offset: Option<u64>,
) -> AbiResult<Settled<i64>> {
    let fs = filesystem(view)?;
    // Before a byte reaches the descriptor. See this function's documentation.
    let base = transfer_buffer(view, buf, count, false, 1)?;
    let mut blocking = BlockingWait::new();
    let mut done = 0u64;
    let mut chunk = vec![0u8; IO_BLOCK];
    while done < count {
        blocking.observe(fs);
        let want = ((count - done) as usize).min(IO_BLOCK);
        // Inside the range admitted above, exactly as in `read_into_guest`.
        let at = base + done as usize;
        let bytes = view.mem().read_bytes(at, want, Blame::new(view.symbol(), view.address(), 1))?;
        chunk[..want].copy_from_slice(&bytes);
        let wrote = match offset {
            None => fs.write(fd, &chunk[..want]),
            Some(start) => {
                // Guest arithmetic, so checked: `read_into_guest`'s reason, in this direction a
                // wrapped offset *overwrites* the wrong part of the file with every call succeeding.
                let Some(at) = start.checked_add(done) else {
                    return Ok(Settled::Failed(consts::EINVAL));
                };
                fs.pwrite(fd, &chunk[..want], at)
            }
        };
        match settle(view, wrote)? {
            Settled::Done(0) => break,
            Settled::Done(took) => done += took as u64,
            Settled::Failed(errno) => {
                if done == 0 {
                    if blocking.should_wait(fs, fd, errno) {
                        blocking.wait(view, fs, fd)?;
                        continue;
                    }
                    return Ok(Settled::Failed(errno));
                }
                // **A blocking write that has already moved some bytes keeps going**, which is
                // the one place `write` differs from `read`: POSIX says a blocking write to a
                // pipe transfers the whole request, so a partial write here is the pipe being
                // full rather than the request being satisfied. It is bounded by the same
                // deadline, so a reader that never drains still ends in a refusal.
                if blocking.should_wait(fs, fd, errno) {
                    blocking.wait(view, fs, fd)?;
                    continue;
                }
                break;
            }
        }
    }
    Ok(Settled::Done(done as i64))
}

/// `ssize_t write(int fd, const void *buf, size_t count)`
///
/// **Bound in M5, and outside the 188** — the reachable list puts it in the Tier C section, and
/// what needs it is `android_native_app_glue`, which writes one `APP_CMD_*` byte into the pipe
/// per message (`jni-surface.md` §5.2, and the engine carries the glue's own diagnostic
/// `"Failure writing android_app cmd: %s"` in `.rodata`).
///
/// `__write_chk` has been here since phase 3b and is the FORTIFY form of this; both now go
/// through the same loop, so a difference between them can only be the `buf_size` check that is
/// the whole point of the FORTIFY form.
pub(super) fn write(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = transfer(c, &state, |view| {
        if count > MAX_COUNT {
            return Ok(Settled::Failed(consts::EINVAL));
        }
        if count == 0 {
            // C: zero bytes. POSIX leaves the result unspecified for anything but a regular file
            // and says nothing is written; the seam is not called at all, so a zero-length write
            // at a null pointer does not fault.
            return Ok(Settled::Done(0));
        }
        write_from_guest(view, fd, buf, count, None)
    })?;
    c.ret().u64(result as u64);
    Ok(())
}

/// `ssize_t __write_chk(int fd, const void *buf, size_t count, size_t buf_size)`
///
/// bionic's FORTIFY form of `write`. `buf_size` is the compiler's knowledge of how large the
/// object at `buf` really is, so `count > buf_size` is a **detected buffer overrun in guest
/// code** — the call would read past the end of the object. That is a refusal naming the symbol
/// and both numbers, exactly as `__memcpy_chk` and `__strlen_chk` already are; reporting it as a
/// short write would lose the finding.
///
/// The whole of `buf` is admitted before the descriptor is written to — see [`write_from_guest`],
/// the source-side half of review finding **M1**. That check is after the FORTIFY one, and in
/// that order: `count > buf_size` is a fact about guest code that holds whatever guest memory
/// looks like, and it is the more specific finding of the two.
pub(super) fn write_chk(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count, buf_size) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = transfer(c, &state, |view| {
        if count > buf_size {
            return Err(view.refusal(format!(
                "__write_chk: the guest asked to write {count} bytes out of an object the \
                 compiler says is {buf_size} bytes. The call would read past the end of it, which \
                 is a detected buffer overrun in guest code -- bionic's FORTIFY build aborts here"
            )));
        }
        if count > MAX_COUNT {
            return Ok(Settled::Failed(consts::EINVAL));
        }
        if count == 0 {
            return Ok(Settled::Done(0));
        }
        write_from_guest(view, fd, buf, count, None)
    })?;
    c.ret().u64(result as u64);
    Ok(())
}

/// The `ssize_t`-returning shape: run `body`, set `errno` and return `-1` on a failure.
fn transfer(
    c: &mut ImportCall<'_, '_>,
    state: &super::Active,
    body: impl FnOnce(&mut GuestView<'_>) -> AbiResult<Settled<i64>>,
) -> AbiResult<i64> {
    let mut view = enter(c, state);
    match body(&mut view)? {
        Settled::Done(value) => Ok(value),
        Settled::Failed(errno) => {
            view.set_errno(errno);
            Ok(-1)
        }
    }
}

// ================================================================== the working directory

/// `char *getcwd(char *buf, size_t size)`
///
/// **The guest's working directory is the confinement root, and from inside it that is `/`.**
///
/// There is no `chdir` here and no per-process directory to change: every guest path is
/// resolved against the one root the host supplied (D23), so the only directory the guest can
/// be *in* is that root, and the only name it has for it is `/`. Answering the host's own
/// working directory would be a fact about this process rather than about the guest, and it
/// would name a path outside the confinement boundary -- which is the direction D23's whole
/// design exists to stop.
///
/// **Not among the 188 statically-reachable imports.** M4's gate found it: the engine calls it
/// from `MainGameActivity.nativeSetAssetPath`, and D17 records 188 as a lower bound.
///
/// The error cases are the POSIX ones, and they are errors rather than refusals because a
/// caller that passes a one-byte buffer is asking a question with a defined answer:
/// `EINVAL` for a zero `size` with a non-null `buf`, `ERANGE` when the name does not fit.
pub(super) fn getcwd(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (buf, size) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let mut view = enter(c, &state);
    // The root, NUL included.
    const CWD: &[u8] = b"/ ";
    if buf == 0 {
        // glibc and bionic both allocate in this case. There is no guest allocator this layer
        // can call, and handing back a pointer into host-owned memory would give the guest an
        // address it would later `free`. `EINVAL` with null is what POSIX defines for a null
        // `buf` outside that extension.
        view.set_errno(consts::EINVAL);
        c.ret().u64(0);
        return Ok(());
    }
    if size == 0 {
        view.set_errno(consts::EINVAL);
        c.ret().u64(0);
        return Ok(());
    }
    if size < CWD.len() as u64 {
        view.set_errno(consts::ERANGE);
        c.ret().u64(0);
        return Ok(());
    }
    let address = guest_address(&view, buf)?;
    view.mem().write_bytes(address, CWD, Blame::new(view.symbol(), view.address(), 0))?;
    c.ret().u64(buf);
    Ok(())
}

// ================================================================== metadata

/// `int access(const char *pathname, int mode)`
///
/// `F_OK`, `R_OK` and `W_OK` are answered by asking the host. **`X_OK` is refused by name**, and
/// the reason is the one Global Constraint 1 is about: Windows has no execute permission on a
/// file, the read-only attribute says nothing about one, and *both* available answers are
/// believable and wrong. Returning `0` tells the guest it may execute a file this runtime cannot
/// execute at all; returning `-1`/`EACCES` reports a policy decision nobody made.
pub(super) fn access(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        if mode & !(R_OK | W_OK | X_OK) != 0 {
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        if mode & X_OK != 0 {
            return Err(view.refusal(
                "the guest asked `access(path, X_OK)`. Windows has no execute permission on a \
                 file and the read-only attribute says nothing about one, so both answers \
                 available here are believable and wrong: `0` tells the guest it may execute a \
                 file this runtime cannot execute at all, and `-1`/EACCES reports a policy \
                 decision nobody made",
            ));
        }
        let fs = filesystem(&view)?;
        let checks: &[AccessCheck] = if mode == F_OK {
            &[AccessCheck::Exists]
        } else if mode == R_OK {
            &[AccessCheck::Readable]
        } else if mode == W_OK {
            &[AccessCheck::Writable]
        } else {
            &[AccessCheck::Readable, AccessCheck::Writable]
        };
        let mut code = 0;
        for check in checks {
            if let Settled::Failed(errno) = settle(&view, fs.access(&bytes, *check))? {
                view.set_errno(errno);
                code = -1;
                break;
            }
        }
        code
    };
    c.ret().i32(result);
    Ok(())
}

/// The shared body of `stat` and `lstat`: resolve, describe, encode, write.
fn stat_path(c: &mut ImportCall<'_, '_>, follow: bool) -> AbiResult<()> {
    let (path, buf) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        let device = view.active.bionic.filesystem_device();
        let described = if follow { fs.stat(&bytes) } else { fs.lstat(&bytes) };
        match settle(&view, described)? {
            Settled::Done(stat) => {
                write_struct(view.blaming(1), buf, &encode_stat(&stat, device), 1)?;
                0
            }
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int stat(const char *pathname, struct stat *statbuf)`
pub(super) fn stat(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    stat_path(c, true)
}

/// `int lstat(const char *pathname, struct stat *statbuf)`
///
/// The one call that may end on a symbolic link, because describing one without following it is
/// its entire purpose. It reports `S_IFLNK` and never reads the link's target, so nothing here
/// leaves the instance's root.
pub(super) fn lstat(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    stat_path(c, false)
}

/// `int fstat(int fd, struct stat *statbuf)`
pub(super) fn fstat(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let device = view.active.bionic.filesystem_device();
        match settle(&view, fs.fstat(fd))? {
            Settled::Done(stat) => {
                write_struct(view.blaming(1), buf, &encode_stat(&stat, device), 1)?;
                0
            }
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int statvfs(const char *path, struct statvfs *buf)`
pub(super) fn statvfs(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, buf) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.statvfs(&bytes))? {
            Settled::Done(stats) => {
                write_struct(view.blaming(1), buf, &encode_statvfs(&stats), 1)?;
                0
            }
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// Whether a guest path names something under `/proc`, after lexical resolution.
fn under_proc(fs: &Filesystem, guest_path: &[u8]) -> bool {
    fs.guest_path("statfs", guest_path)
        .is_ok_and(|path| path == "/proc" || path.starts_with("/proc/"))
}

/// What `statfs` answers for a path: `/proc`'s pseudo filesystem for a file this layer generates
/// there, the host volume's numbers for everything else.
///
/// **`/proc` is decided by the path, and existence by `stat`.** Every file under `/proc` here is
/// one [`Filesystem::serve_generated`] produces and none is under the root, so asking the host
/// volume about one would answer `ENOENT` for a file `open` serves. `stat` is the call that already
/// knows which `/proc` paths exist, and its failure is this call's failure.
fn statfs_of_path(
    view: &GuestView<'_>,
    fs: &Filesystem,
    guest_path: &[u8],
) -> AbiResult<Settled<StatfsAnswer>> {
    if under_proc(fs, guest_path) {
        let page = view.active.bionic.space_page_size() as u64;
        return Ok(match settle(view, fs.stat(guest_path))? {
            Settled::Done(_) => {
                Settled::Done(StatfsAnswer::pseudo(PROC_SUPER_MAGIC, page, ST_VALID | ST_RELATIME))
            }
            Settled::Failed(errno) => Settled::Failed(errno),
        });
    }
    Ok(match settle(view, fs.statvfs(guest_path))? {
        Settled::Done(stats) => Settled::Done(StatfsAnswer::volume(&stats)),
        Settled::Failed(errno) => Settled::Failed(errno),
    })
}

/// Write a settled `statfs` answer or set its `errno`, returning the import's `0` or `-1`.
fn finish_statfs(
    view: &mut GuestView<'_>,
    answer: Settled<StatfsAnswer>,
    buf: u64,
) -> AbiResult<i32> {
    Ok(match answer {
        Settled::Done(answer) => {
            write_struct(view.blaming(2), buf, &answer.encode(), 2)?;
            0
        }
        Settled::Failed(errno) => {
            view.set_errno(errno);
            -1
        }
    })
}

/// `int statfs(const char *path, struct statfs *buf)`, reached as `syscall(43, path, buf)` or a
/// raw `SVC #0` with `x8 = 43`.
///
/// **Not a bound import**: no library of either APK imports `statfs`, `fstatfs`, `statfs64` or
/// `fstatvfs` (their `.dynsym`s, read 2026-09-24). The engine asks the kernel directly, so this is
/// reached only through [`super::procenv`]'s `syscall`, in the import convention (`-1` and
/// `errno`); the raw-`SVC` path converts that to `-errno` itself. Argument numbers in a refusal
/// are `syscall`'s: 0 is the syscall number, 1 the path, 2 the buffer.
///
/// The error split is [`statvfs`]'s: a path that is not there is `ENOENT`, a null path or an
/// unwritable buffer is a refusal naming the argument.
pub(super) fn statfs(c: &mut ImportCall<'_, '_>, path: u64, buf: u64) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(1), path, 1)?;
        let fs = filesystem(&view)?;
        let answer = statfs_of_path(&view, fs, &bytes)?;
        finish_statfs(&mut view, answer, buf)?
    };
    c.ret().i32(result);
    Ok(())
}

/// `int fstatfs(int fd, struct statfs *buf)`, reached as `syscall(44, fd, buf)` or a raw `SVC #0`
/// with `x8 = 44`. MEASURED need: a TaskScheduler worker (start link `0x2869934`) died on the
/// refusal of raw syscall 44 in the Pet Simulator 99 world (2026-09-24).
///
/// # What each descriptor kind answers
///
/// * **Not open → `EBADF`**, the kernel's answer. The kernel takes the descriptor as an
///   `unsigned int`, so only its low 32 bits are the descriptor.
/// * **A file, a directory or a generated `/proc` file → what [`statfs`] answers for the path it
///   was opened by.** Every one of these was opened by a guest path and none moves, so the path
///   is the volume. The one way that can fail is a file unlinked while open, which Linux still
///   answers for and which here has no path left to ask about. `fstatfs` has no `ENOENT` to give
///   — an open descriptor is on *some* filesystem — so that is refused by name rather than
///   reported as an error the call cannot produce.
/// * **A pipe → `PIPEFS_MAGIC`, a socket → `SOCKFS_MAGIC`, an eventfd, epoll or timerfd →
///   `ANON_INODE_FS_MAGIC`**, each with `simple_statfs`'s shape (see
///   [`StatfsAnswer::pseudo`]). These are what Linux answers, and they are answered rather than
///   refused because every field is a constant of the kernel's rather than a property of a device:
///   those three filesystems are kernel-internal mounts with no size, no inodes to count, no fsid
///   and no mount options, so `f_flags` is `ST_VALID` alone. **ASSUMED from `fs/statfs.c` and
///   `fs/libfs.c` as recalled, not measured on a device.**
/// * **A standard stream or a character device (`/dev/null`, `/dev/urandom`) → refused by name.**
///   On a device those live on `/dev`'s tmpfs, whose block counts are a share of the device's RAM
///   this layer does not model, and the host's standard streams are not files on any Android
///   filesystem at all. No run has reached either.
pub(super) fn fstatfs(c: &mut ImportCall<'_, '_>, fd: u64, buf: u64) -> AbiResult<()> {
    // `unsigned int fd`, as the kernel declares it: the upper half of the register is not part of
    // the descriptor.
    let fd = fd as u32 as i32;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let page = view.active.bionic.space_page_size() as u64;
        let answer = if !fs.is_open(fd) {
            Settled::Failed(consts::EBADF)
        } else if fs.pipe_end(fd).is_some() {
            Settled::Done(StatfsAnswer::pseudo(PIPEFS_MAGIC, page, ST_VALID))
        } else if fs.is_socket(fd) {
            Settled::Done(StatfsAnswer::pseudo(SOCKFS_MAGIC, page, ST_VALID))
        } else if fs.is_epoll(fd)
            || fs.eventfd_value(fd).is_some()
            || fs.readiness_source(fd) == Some(ReadinessSource::Timer)
        {
            Settled::Done(StatfsAnswer::pseudo(ANON_INODE_FS_MAGIC, page, ST_VALID))
        } else if let Some(guest) = fs.guest_path_of(fd) {
            match statfs_of_path(&view, fs, guest.as_bytes())? {
                Settled::Failed(errno) if errno == consts::ENOENT => {
                    return Err(view.refusal(format!(
                        "fstatfs on descriptor {fd}, opened as `{guest}`, which is no longer \
                         there. Linux answers for an unlinked file's volume through the open \
                         descriptor; this layer finds the volume by the path, and fstatfs has no \
                         ENOENT to report instead"
                    )));
                }
                settled => settled,
            }
        } else {
            return Err(view.refusal(format!(
                "fstatfs on descriptor {fd}, a standard stream or a character device. On a device \
                 those are on /dev's tmpfs, whose block counts are a share of the device's RAM \
                 that this layer does not model, and a host standard stream is on no Android \
                 filesystem at all; every f_type available is a guess"
            )));
        };
        finish_statfs(&mut view, answer, buf)?
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== the namespace

/// The shared body of the one-path namespace calls.
fn one_path(
    c: &mut ImportCall<'_, '_>,
    action: impl FnOnce(&Filesystem, &[u8]) -> FsResult<()>,
) -> AbiResult<()> {
    let path = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, action(fs, &bytes))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int rename(const char *oldpath, const char *newpath)`
///
/// Both paths are confined independently, so a rename cannot be used to move a file out of the
/// root by naming the destination cleverly.
pub(super) fn rename(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (from, to) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let old = path_for(view.blaming(0), from, 0)?;
        let new = path_for(view.blaming(1), to, 1)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.rename(&old, &new))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int unlink(const char *pathname)`
pub(super) fn unlink(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    one_path(c, |fs, path| fs.unlink(path))
}

/// `int mkdir(const char *pathname, mode_t mode)`
///
/// **`mode` is read and not applied.** Windows has no POSIX permission bits, so a directory the
/// guest asks to create with `0700` is created with what it inherits from the instance's root. It
/// is recorded here rather than turned into a refusal because refusing every `mkdir` would stop
/// the engine creating any directory at all, and because the security boundary this design relies
/// on is the **root** — which the host operator supplies and protects — rather than the
/// permissions of one directory inside it.
pub(super) fn mkdir(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, _mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.mkdir(&bytes))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int utime(const char *filename, const struct utimbuf *times)`
///
/// `struct utimbuf` on LP64 is `{ time_t actime; time_t modtime; }`, two 64-bit seconds; a null
/// `times` means **now** for both, as POSIX says. Returns 0, or -1 with `errno`.
///
/// MEASURED reader: a guest worker (started at link `0x2217f04`) on the **second** launch of a
/// kept data directory, under the engine's HTTP cache (`0x2313b04`) -- stamping a cached
/// response it has just used, a path a fresh install never takes.
pub(super) fn utime(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, times) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let (accessed, modified) = if times == 0 {
            let now = std::time::SystemTime::now();
            (now, now)
        } else {
            let raw = view.mem().read_bytes(
                guest_address(&view, times)?,
                16,
                Blame::new(view.symbol(), view.address(), 1),
            )?;
            let seconds = |at: usize| i64::from_le_bytes(raw[at..at + 8].try_into().expect("eight bytes"));
            let when = |seconds: i64| {
                let magnitude = std::time::Duration::from_secs(seconds.unsigned_abs());
                if seconds >= 0 {
                    std::time::UNIX_EPOCH + magnitude
                } else {
                    std::time::UNIX_EPOCH - magnitude
                }
            };
            (when(seconds(0)), when(seconds(8)))
        };
        let fs = filesystem(&view)?;
        match settle(&view, fs.set_times(&bytes, accessed, modified))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int rmdir(const char *pathname)`
pub(super) fn rmdir(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    one_path(c, |fs, path| fs.rmdir(path))
}

/// `int remove(const char *pathname)`
///
/// bionic's own, in `libc/stdio/stdio.cpp` -- not the lstat-then-choose of the BSD `remove.c`:
///
/// ```c
/// if (unlink(path) != -1) return 0;
/// if (errno != EISDIR) return -1;
/// return rmdir(path);
/// ```
///
/// So a file (or a link, which `unlink` never follows) is unlinked, an empty directory is
/// removed, and every other failure is `unlink`'s `errno`, a missing path's `ENOENT` among them;
/// a non-empty directory is `rmdir`'s `ENOTEMPTY`. Returns 0, or -1 with `errno`. One
/// difference, in what C17 7.21.4.1 leaves unspecified: after a directory is removed, bionic's
/// `errno` still holds `unlink`'s `EISDIR`, and this leaves it as it was.
///
/// MEASURED need: a guest thread died on it unbound on the second launch of an account's kept
/// storage (Linux), and the session hung behind it.
pub(super) fn remove(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    one_path(c, |fs, path| match fs.unlink(path) {
        Err(error) if error.kind() == Some(FsErrorKind::IsADirectory) => fs.rmdir(path),
        unlinked => unlinked,
    })
}

// ================================================================== directories

/// `DIR *opendir(const char *name)`
///
/// The returned `DIR *` is the address of **this stream's own `struct dirent` slot** in the
/// adapter's arena, which makes it a real guest pointer the guest can compare and store, and
/// makes `readdir`'s "the returned pointer is valid until the next call on this `DIR`" trivially
/// true: the pointer is the slot, and the slot belongs to the stream.
///
/// Returns `NULL` with `errno` on failure.
pub(super) fn opendir(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let path = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.opendir(&bytes))? {
            Settled::Done(id) => match state.bionic.attach_dir(id) {
                Some(pointer) => pointer as u64,
                None => {
                    // The arena ran out of slots before the seam's own ceiling did. Reported as
                    // EMFILE, which is what a real device reports when a process cannot open
                    // another stream, rather than as a refusal: it is a resource limit, and
                    // guest code has a branch for it.
                    let _ = fs.closedir(id);
                    view.set_errno(consts::EMFILE);
                    0
                }
            },
            Settled::Failed(errno) => {
                view.set_errno(errno);
                0
            }
        }
    };
    c.ret().u64(result);
    Ok(())
}

/// `struct dirent *readdir(DIR *dirp)`
///
/// Returns `dirp` itself — the slot the entry was just written into — or `NULL` at the end of the
/// directory. **`NULL` at the end does not set `errno`**, which is how a caller distinguishes the
/// end of a directory from a failure: the documented idiom is to clear `errno`, call `readdir`,
/// and check `errno` if it returns `NULL`.
pub(super) fn readdir(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let dirp = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let Some(id) = state.bionic.dir_for(dirp) else {
            // A `DIR *` this instance never handed out. A refusal rather than `NULL`, because
            // `NULL` is `readdir`'s ordinary end-of-directory answer and a guest would read a
            // wild pointer as an empty directory.
            return Err(view.refusal(format!(
                "the guest called `readdir` with the DIR pointer {dirp:#x}, which this instance \
                 never returned from `opendir`. NULL is readdir's own end-of-directory answer, so \
                 answering with it would turn a wild pointer into an empty directory"
            )));
        };
        let fs = filesystem(&view)?;
        let position = match settle(&view, fs.dir_position(id))? {
            Settled::Done((position, _)) => position as u64,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                c.ret().u64(0);
                return Ok(());
            }
        };
        // **Admitted before the stream is advanced**, which is finding M1's shape in a third
        // place. `Filesystem::readdir` consumes an entry, and there is no `rewinddir` and no
        // `seekdir` here, so an entry that cannot be placed is an entry the guest can never
        // obtain — a directory silently one file short, which is the plausible-wrong-answer class
        // rather than a fault.
        //
        // The destination is this instance's own arena rather than a pointer the guest chose, so
        // it is writable unless the guest has reprotected or unmapped the arena under itself. It
        // **can**: `mprotect` and `munmap` are bound in `guestmem` and neither excludes the
        // arena. That is what makes this a check rather than reassurance (`VERIFICATION` entry
        // 12), and `a_readdir_that_cannot_write_its_entry_consumes_no_entry` is the input that
        // reaches it.
        let slot = guest_address(&view, dirp)?;
        view.mem().checked_ptr(
            slot,
            DIRENT_BYTES,
            true,
            Blame::new(view.symbol(), view.address(), 0),
        )?;
        match settle(&view, fs.readdir(id))? {
            Settled::Done(Some(entry)) => {
                let encoded = encode_dirent(&entry, position + 1);
                write_struct(view.blaming(0), dirp, &encoded, 0)?;
                dirp
            }
            // The end of the directory. `errno` is deliberately left alone.
            Settled::Done(None) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                0
            }
        }
    };
    c.ret().u64(result);
    Ok(())
}

/// `int closedir(DIR *dirp)`
pub(super) fn closedir(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let dirp = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let Some(id) = state.bionic.detach_dir(dirp) else {
            view.set_errno(consts::EBADF);
            c.ret().i32(-1);
            return Ok(());
        };
        let fs = filesystem(&view)?;
        match settle(&view, fs.closedir(id))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== pipes and descriptor flags
//
// **Neither symbol is among the 188 the initializers reach**, and both are imports of
// `libroblox.so`. They are here because `jni-surface.md` §5.2 decodes `initializeNativeCode` and
// finds `pipe()` + `fcntl(F_SETFL, O_NONBLOCK)` twice before the call can return -- once in the
// constructor, for the `msgread`/`msgwrite` pair the `ALooper` watches, and once in
// `GameActivity_onCreate`, for the glue's own command pipe.
//
// Binding `pipe` is what ended the closed-descriptor-space argument `net` used to make; that
// module's documentation records what replaced it.

/// `FIONBIO`: set or clear non-blocking mode. Linux `asm-generic/ioctls.h`.
const FIONBIO: i32 = 0x5421;
/// `SIOCGIFCONF`: enumerate the host's configured interfaces. Linux `bits/ioctls.h`.
const SIOCGIFCONF: i32 = 0x8912;
/// `SIOCGIFFLAGS`: one interface's flags.
const SIOCGIFFLAGS: i32 = 0x8913;
/// `SIOCGIFADDR`: one interface's address.
const SIOCGIFADDR: i32 = 0x8915;

/// The `ioctl` requests `libroblox.so` actually passes, decoded from its own instructions.
///
/// **Every one of the five call sites, not a guess.** `ioctl` reaches `libroblox.so` through one
/// PLT stub at `0x62d7340` (its GOT slot is `0x67d2098`), and five `BL`s in `.text` target it.
/// Four of them load a literal request into `w1` immediately before the branch and the fifth is
/// a pass-through wrapper:
///
/// | call site | `w1` | request |
/// |---|---|---|
/// | `0x02956388` | `0x5421` | `FIONBIO`, with `x2` pointing at the `int` stored from its own `w1` one instruction earlier -- this is a `set_nonblocking(fd, on)` helper |
/// | `0x055b040c` | `0x8913` | `SIOCGIFFLAGS` |
/// | `0x061f4354` | `0x8912` | `SIOCGIFCONF` |
/// | `0x061f43d0` | `0x8915` | `SIOCGIFADDR` |
/// | `0x02955ffc` | -- | a three-instruction wrapper that passes its caller's request straight through |
///
/// That decoding is why this table is the whole reachable set rather than the ones that seemed
/// likely, and why the three `SIOC*` requests are **named and refused** rather than absent: a
/// refusal that says `SIOCGIFCONF` tells the next reader the engine was enumerating network
/// interfaces, which is a different problem from anything `FIONBIO` is about.
///
/// `FIONREAD` is deliberately not here. It is the request everybody expects beside `FIONBIO`,
/// **no call site passes it**, and listing it would make this table a catalogue of ioctl numbers
/// rather than a decoding of this binary.
const IOCTL_REQUESTS: &[(i32, &str)] = &[
    (0x5421, "FIONBIO"),
    (0x8912, "SIOCGIFCONF"),
    (0x8913, "SIOCGIFFLAGS"),
    (0x8915, "SIOCGIFADDR"),
];

/// Name an `ioctl` request, or render its number.
fn ioctl_request_name(request: i32) -> String {
    match IOCTL_REQUESTS.iter().find(|(number, _)| *number == request) {
        Some((_, name)) => format!("{name} ({request:#x})"),
        None => format!("request {request:#x}"),
    }
}

/// `int ioctl(int fd, int request, ...)` -- **`FIONBIO` only, and everything else by name.**
///
/// **MEASURED**: with `isspace` bound, M6's network run went one call further and the
/// client-settings thread died here -- `GuestThreadFailure { thread: 8, why: "the guest called
/// the imported symbol `ioctl` through its thunk at 0x28fa0c555e0, and nothing in the
/// compatibility layer implements it" }`.
///
/// # Why only one request, when four are reachable
///
/// [`IOCTL_REQUESTS`] decodes all five call sites, and the split between them is not arbitrary.
/// `FIONBIO` is a **descriptor** operation this runtime already implements: it is the same state
/// `fcntl(F_SETFL, O_NONBLOCK)` sets, on the same descriptor table, through the same
/// `Filesystem::set_nonblocking` -- so serving it adds no new behaviour, it routes a second
/// spelling to the first one's implementation.
///
/// The three `SIOC*` requests are the opposite: they enumerate the **host's** network interfaces
/// and report their names, flags and addresses. Nothing in `omni-platform` can answer them,
/// D30's isolation argument is about not handing the guest the host's network by accident, and
/// an invented interface list is the plausible-stub shape rule 1 exists for -- a caller that
/// asked which interfaces exist and was told `lo` would believe it.
///
/// # The three are declined with `EPERM`, and the reason is decoded rather than assumed
///
/// **This is the one judgement call in this file and it should be read as one.** The three are
/// not answered, and they are not a refusal that unwinds either. They return `-1` with `EPERM`:
/// *this instance is not permitted to enumerate the host's interfaces*, which is exactly what is
/// true and is a statement no caller can mistake for data.
///
/// The alternative -- `AbiError::Refused`, which is what an unimplemented capability gets
/// everywhere else in this layer -- was what this handler did first, and MEASURED it kills the
/// guest thread carrying the client-settings fetch. That is the correct answer when the caller
/// has no error path, because a thread that dies loudly is better than one that believes a lie.
/// **Here the caller has an error path, by name, and it was decoded before this was changed:**
///
/// * `0x061f4358`: `cmn w0, #1; b.eq 0x61f4668` -- the `SIOCGIFCONF` site tests for `-1` and
///   branches to code that passes the string `"ioctl(SIOCGIFCONF)"` (at `0x29797d`) to a
///   reporting function, stores `-1` and the captured `errno` into its result structure, and
///   returns. It does not abort, retry or dereference anything.
/// * `0x061f43d4` and `0x055b03dc`: the `SIOCGIFADDR` and `SIOCGIFFLAGS` sites test `-1` the same
///   way, inside the per-interface loop.
///
/// So the guest is told the call failed, it takes the path it already has for exactly that
/// failure, and nothing in this layer pretends. **What would falsify this judgement**: a run in
/// which the settings fetch does not complete *and* the reason traces back to one of these three
/// -- at which point the answer is a real interface enumeration behind a `NetPolicy` decision,
/// not a different errno. The refusal is one line away and is what to restore if that happens.
///
/// `EPERM` rather than `ENOTTY` or `EINVAL` deliberately: those two say the descriptor does not
/// support the request, which is false -- it is a socket and a device would answer. `EPERM` says
/// the caller may not, which is the truth and is the same sentence `NetPolicy` makes everywhere
/// else in this runtime.
///
/// # `x2` is a pointer and the value is read before anything is changed
///
/// `FIONBIO` takes a `const int *`, not an `int`. The decoded call site at `0x02956388` stores
/// its own `w1` into a stack slot and passes `x29-4`, which is exactly that shape. Reading it
/// through the guest view means a null or unmapped pointer is a fault naming the argument,
/// rather than a non-blocking flip decided by whatever the register happened to hold.
pub(super) fn ioctl(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, request, argument) = {
        let mut a: Args<'_> = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        if !fs.is_open(fd) {
            view.set_errno(consts::EBADF);
            c.ret().i32(-1);
            return Ok(());
        }
        if request == SIOCGIFCONF || request == SIOCGIFFLAGS || request == SIOCGIFADDR {
            // **A policy denial, reported as one, and it is the one judgement call in this file.**
            // See [`ioctl`]'s documentation for why this is `-1`/`EPERM` and not a refusal that
            // unwinds, and for the decoded evidence that the guest branches on the `-1`.
            view.set_errno(consts::EPERM);
            c.ret().i32(-1);
            return Ok(());
        }
        if request != FIONBIO {
            return Err(view.refusal(format!(
                "the guest called `ioctl(fd {fd}, {})`. This layer implements FIONBIO, and                  declines the three interface-enumeration requests by policy with EPERM.                  {request:#x} is neither: nothing in this binary was decoded passing it, so it                  is an argument nobody has measured and there is no honest answer to invent for                  it",
                ioctl_request_name(request)
            )));
        }
        if argument == 0 {
            // `FIONBIO` dereferences its argument; a device answers `EFAULT` and this layer has
            // no `EFAULT` in its errno table. `EINVAL` is the same shape of answer `getsockopt`
            // gives a null out-parameter in `net`, and it is a failure rather than a silent
            // no-op -- which is what a caller whose flag did not take effect needs.
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        let at = guest_address(&view, argument)?;
        let on = view.mem().read_i32(at, Blame::new(view.symbol(), view.address(), 2))? != 0;
        match settle(&view, fs.set_nonblocking(fd, on))? {
            Settled::Done(()) => 0,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `F_GETFD`: read the descriptor's flags, of which Linux defines one, `FD_CLOEXEC`.
const F_GETFD: i32 = 1;
/// `F_SETFD`: set them. Linux keeps `arg & FD_CLOEXEC` and ignores the rest, there being no
/// other descriptor flag to set.
const F_SETFD: i32 = 2;
/// `FD_CLOEXEC`, the one descriptor flag.
const FD_CLOEXEC: i32 = 1;
/// `F_GETFL`: read the descriptor's status flags. Linux `asm-generic/fcntl.h`.
const F_GETFL: i32 = 3;
/// `F_SETFL`: set the descriptor's status flags.
const F_SETFL: i32 = 4;
/// `F_GETLK`: would this record lock be granted? Linux `asm-generic/fcntl.h`; on LP64 there is
/// no separate `F_GETLK64`, the one command takes the one 64-bit `struct flock`.
const F_GETLK: i32 = 5;
/// `F_SETLK`: take or release a record lock, failing rather than waiting on a conflict.
const F_SETLK: i32 = 6;
/// `F_SETLKW`: the same, waiting on a conflict.
const F_SETLKW: i32 = 7;
/// `struct flock`'s `l_type` values, `asm-generic/fcntl.h`.
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
/// Bytes of LP64 `struct flock`: `short l_type; short l_whence; off_t l_start; off_t l_len;
/// pid_t l_pid;` at 0, 2, 8, 16 and 24, padded to 32. The offsets are the engine's own: SQLite's
/// lock path stores `l_type` at `[sp]`, `l_whence` at `[sp, #2]`, `l_start` at `[sp, #8]` and
/// `l_len` at `[sp, #0x10]` (`libroblox.so` link `0x22d70b0`-`0x22d70c8`).
const FLOCK_BYTES: usize = 32;

/// The `fcntl` commands this layer knows the names of.
///
/// **The whole list, not the ones that seemed likely.** A command absent from it still refuses,
/// with its raw number, which is still a measurement — but a named one tells whoever reads the
/// failure what the engine was trying to do.
const FCNTL_COMMANDS: &[(i32, &str)] = &[
    (0, "F_DUPFD"),
    (1, "F_GETFD"),
    (2, "F_SETFD"),
    (3, "F_GETFL"),
    (4, "F_SETFL"),
    (5, "F_GETLK"),
    (6, "F_SETLK"),
    (7, "F_SETLKW"),
    (8, "F_SETOWN"),
    (9, "F_GETOWN"),
    (10, "F_SETSIG"),
    (11, "F_GETSIG"),
    (1024, "F_SETLEASE"),
    (1025, "F_GETLEASE"),
    (1026, "F_NOTIFY"),
    (1030, "F_DUPFD_CLOEXEC"),
    (1031, "F_SETPIPE_SZ"),
    (1032, "F_GETPIPE_SZ"),
];

/// Name an `fcntl` command, or render its number.
fn fcntl_command_name(command: i32) -> String {
    FCNTL_COMMANDS
        .iter()
        .find(|(number, _)| *number == command)
        .map_or_else(|| format!("command {command}"), |(_, name)| (*name).to_string())
}

/// `int pipe(int pipefd[2])`
///
/// Writes the read end into `pipefd[0]` and the write end into `pipefd[1]` as **one** access, so
/// a destination that is only partly writable leaves the guest neither descriptor rather than
/// one — the same all-or-nothing shape [`write_struct`] exists for, and the direction review
/// finding M1 says to err in.
///
/// **If the write fails, both descriptors are closed again.** The other order is not available:
/// the numbers do not exist until the pipe does. Leaving them open would hand the guest a leak it
/// cannot close, because it never learned what to close.
pub(super) fn pipe(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let pipefd = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if pipefd == 0 {
            // POSIX: `EFAULT`. A refusal rather than `-1`, for the reason `path_for` gives — a
            // guest that ignored the return would carry two uninitialised descriptors forward.
            return Err(view.refusal("`pipe` was given a null `pipefd` pointer"));
        }
        let fs = filesystem(&view)?;
        match settle(&view, fs.pipe())? {
            Settled::Done((read_fd, write_fd)) => {
                let mut bytes = [0u8; 8];
                bytes[..4].copy_from_slice(&read_fd.to_le_bytes());
                bytes[4..].copy_from_slice(&write_fd.to_le_bytes());
                if let Err(error) = write_struct(&view, pipefd, &bytes, 0) {
                    let _ = fs.close(read_fd);
                    let _ = fs.close(write_fd);
                    return Err(error);
                }
                0
            }
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int fcntl(int fd, int cmd, ...)`
///
/// **The commands this runtime has something true to answer, and every other one refuses with the
/// command named.** `F_GETFL` and `F_SETFL` are what §5.2 needs: the glue sets `O_NONBLOCK` on both
/// ends of both pipes, and reads nothing back. `F_GETFD`/`F_SETFD` read and write the descriptor's
/// `FD_CLOEXEC`, recorded where the descriptor is made ([`record_close_on_exec`]).
///
/// Variadic. It is a fourteenth variadic import rather than a correction to Task 2's count of
/// thirteen, which was over the 188 — `fcntl` is outside them.
///
/// # What `F_SETFL` accepts, and why the rest is a refusal rather than a mask
///
/// Linux's `F_SETFL` ignores every bit except `O_APPEND`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME` and
/// `O_NONBLOCK`. Ignoring bits is exactly the believable-wrong-answer shape this project refuses:
/// a guest that set `O_ASYNC` and was told it worked would wait for a signal that never comes. So
/// `O_NONBLOCK` is honoured, a zero clears it, and any other bit is refused with the bits named.
///
/// # What `F_GETFL` does **not** answer
///
/// The access mode. This seam does not record `O_RDONLY`/`O_RDWR` in a form `fcntl` could return,
/// and a fabricated `O_RDWR` is precisely the value a guest would branch on. Only the one status
/// flag this layer models is answered; the rest of the word is zero, which is a true statement
/// about every flag `F_SETFL` here can set.
pub(super) fn fcntl(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, command, argument) = {
        let mut a: Args<'_> = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        if !fs.is_open(fd) {
            view.set_errno(consts::EBADF);
            c.ret().i32(-1);
            return Ok(());
        }
        match command {
            // **The close-on-exec flag, as the guest set it.** Inert here -- nothing execs -- but
            // reported truthfully. MEASURED why: pressing Play on a game page, a TaskScheduler
            // worker called `fcntl(64, F_GETFD)` and died on the refusal (2026-09-23).
            F_GETFD => match settle(&view, fs.is_close_on_exec(fd))? {
                Settled::Done(true) => FD_CLOEXEC,
                Settled::Done(false) => 0,
                Settled::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            },
            F_SETFD => {
                let on = argument as i32 & FD_CLOEXEC != 0;
                match settle(&view, fs.set_close_on_exec(fd, on))? {
                    Settled::Done(()) => 0,
                    Settled::Failed(errno) => {
                        view.set_errno(errno);
                        -1
                    }
                }
            }
            F_GETFL => match settle(&view, fs.is_nonblocking(fd))? {
                Settled::Done(true) => O_NONBLOCK,
                Settled::Done(false) => 0,
                Settled::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            },
            F_SETFL => {
                let flags = argument as i32;
                let unhandled = flags & !O_NONBLOCK;
                if unhandled != 0 {
                    return Err(view.refusal(format!(
                        "the guest called `fcntl(F_SETFL)` with {unhandled:#x} beyond O_NONBLOCK. \
                         Linux ignores every bit but O_APPEND, O_ASYNC, O_DIRECT, O_NOATIME and \
                         O_NONBLOCK, and ignoring one here would tell the guest a flag took \
                         effect when nothing in this runtime implements it -- a guest that set \
                         O_ASYNC would wait for a signal that never arrives"
                    )));
                }
                match settle(&view, fs.set_nonblocking(fd, flags & O_NONBLOCK != 0))? {
                    Settled::Done(()) => 0,
                    Settled::Failed(errno) => {
                        view.set_errno(errno);
                        -1
                    }
                }
            }
            F_GETLK | F_SETLK | F_SETLKW => {
                match record_lock(&view, fs, fd, command, argument)? {
                    Settled::Done(()) => 0,
                    Settled::Failed(errno) => {
                        view.set_errno(errno);
                        -1
                    }
                }
            }
            other => {
                return Err(view.refusal(format!(
                    "the guest called `fcntl` with {} on fd {fd}. This layer implements F_GETFL, \
                     F_SETFL(O_NONBLOCK) -- what the GameActivity glue needs for its two pipes \
                     (jni-surface.md §5.2), F_GETFD/F_SETFD (FD_CLOEXEC, recorded and inert) \
                     and the process-private record locks F_GETLK/F_SETLK/F_SETLKW the engine's \
                     SQLite takes. Every other command asks for something this runtime does not \
                     have: a second descriptor for one description, a signal owner, an \
                     open-file-description lock, or a pipe capacity this layer fixes at \
                     omni_platform::fs::PIPE_CAPACITY",
                    fcntl_command_name(other)
                )));
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `fcntl`'s three record-lock commands, over the `struct flock` at `at`.
///
/// Validated in **Linux's order**, because a caller can tell the orders apart by which `errno`
/// it gets: `l_type` (`EINVAL`), then `l_whence` and the range (`EINVAL`, or `EOVERFLOW` for an
/// end past `off_t`), and only then the descriptor's access mode (`EBADF`, `F_SETLK`/`F_SETLKW`
/// only -- `fcntl_getlk` does not check it). See [`Filesystem::record_lock`] for why a valid
/// request is granted: no other process can hold a conflicting lock, so there is nothing to wait
/// for (`F_SETLKW` never blocks) and nothing for `F_GETLK` to report (it answers `F_UNLCK` and
/// leaves every other field as it was, which is what Linux writes back when nothing conflicts).
///
/// `SEEK_CUR` and `SEEK_END` refuse by name: they need the descriptor's offset or size, and the
/// only measured caller -- SQLite -- always passes `SEEK_SET`.
fn record_lock(
    view: &GuestView<'_>,
    fs: &Filesystem,
    fd: i32,
    command: i32,
    at: u64,
) -> AbiResult<Settled<()>> {
    let pointer =
        usize::try_from(at).map_err(|_| view.refusal("a `struct flock *` wider than usize"))?;
    if pointer == 0 {
        // Linux: EFAULT. Refused, for the reason `pipe` refuses a null `pipefd`.
        return Err(view.refusal(format!(
            "`fcntl({})` was given a null `struct flock *`",
            fcntl_command_name(command)
        )));
    }
    let bytes = view.mem().read_bytes(
        pointer,
        FLOCK_BYTES,
        Blame::new(view.symbol(), view.address(), 2),
    )?;
    let l_type = i16::from_le_bytes([bytes[0], bytes[1]]);
    let l_whence = i16::from_le_bytes([bytes[2], bytes[3]]);
    let l_start = i64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
    let l_len = i64::from_le_bytes(bytes[16..24].try_into().expect("eight bytes"));

    let lock = match (command, l_type) {
        (F_GETLK | F_SETLK | F_SETLKW, F_RDLCK) => RecordLock::Shared,
        (F_GETLK | F_SETLK | F_SETLKW, F_WRLCK) => RecordLock::Exclusive,
        (F_SETLK | F_SETLKW, F_UNLCK) => RecordLock::Release,
        // `F_GETLK` with `F_UNLCK`, and any other type: `fcntl_getlk`/`flock_to_posix_lock`.
        _ => return Ok(Settled::Failed(consts::EINVAL)),
    };
    match l_whence {
        0 => {}
        1 | 2 => {
            return Err(view.refusal(format!(
                "`fcntl({})` with l_whence {} ({}): a lock relative to the descriptor's {} needs \
                 that value, and no run has reached one -- SQLite passes SEEK_SET",
                fcntl_command_name(command),
                l_whence,
                if l_whence == 1 { "SEEK_CUR" } else { "SEEK_END" },
                if l_whence == 1 { "offset" } else { "size" }
            )))
        }
        _ => return Ok(Settled::Failed(consts::EINVAL)),
    }
    // `flock_to_posix_lock`: a negative start is EINVAL, an end past `off_t` is EOVERFLOW, and a
    // negative length locks the bytes *before* the start, which must not reach below zero.
    if l_start < 0 {
        return Ok(Settled::Failed(consts::EINVAL));
    }
    if l_len > 0 && l_start.checked_add(l_len - 1).is_none() {
        return Ok(Settled::Failed(consts::EOVERFLOW));
    }
    if l_len < 0 && l_start.checked_add(l_len).is_none_or(|start| start < 0) {
        return Ok(Settled::Failed(consts::EINVAL));
    }

    if command == F_GETLK {
        // The descriptor must still be a file this seam locks; its access mode is not checked,
        // because Linux does not check it for a test. The requested type was validated above
        // and is not otherwise needed: nothing can conflict with it.
        if let Settled::Failed(errno) = settle(view, fs.record_lock(fd, RecordLock::Release))? {
            return Ok(Settled::Failed(errno));
        }
        write_struct(view, at, &F_UNLCK.to_le_bytes(), 2)?;
        return Ok(Settled::Done(()));
    }
    settle(view, fs.record_lock(fd, lock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // `a_blocking_transfer_gives_up_within_a_minute` stood here, the detector for the minute a
    // blocking transfer used to be refused after. There is no such minute now: the transfer waits
    // as a device does, and what ends it is the stop switch. The detector for that is end to end
    // -- `tests/bionic.rs`, `long_waits_on_guest_threads_end_when_the_runtime_stops` -- because a
    // stop-switch test that fails *joins with a timeout* on a guest thread rather than hanging
    // the suite, which is what forced the old one to be a unit test.

    /// The three structures are the sizes the guest's headers say, and their fields land where
    /// the tables above claim.
    ///
    /// **A layout test with literal offsets**, because the failure this catches is an offset that
    /// is plausible: a `struct stat` whose `st_size` is at 40 rather than 48 produces a size that
    /// is always zero, which reads as an empty file rather than as a mistake.
    #[test]
    fn the_guest_structures_are_the_sizes_and_offsets_their_headers_say() {
        assert_eq!(STAT_BYTES, 128, "asm-generic/stat.h on arm64");
        assert_eq!(STATVFS_BYTES, 112, "bionic <sys/statvfs.h> on LP64");
        assert_eq!(DIRENT_BYTES, 280, "bionic <dirent.h> on LP64");
        assert_eq!(DIRENT_BYTES % 8, 0, "a dirent ends on its own alignment");
        assert_eq!(D_NAME_OFFSET, 19);
        assert_eq!(D_NAME_OFFSET + D_NAME_BYTES, 275, "and the record pads from there to 280");

        let stat = FileStat {
            kind: FileKind::Regular,
            size: 0x1234_5678_9abc,
            read_only: false,
            accessed: Some(Duration::new(0x1111_2222, 333)),
            modified: Some(Duration::new(0x4444_5555, 666)),
            created: None,
            identity: 0xfeed_face_dead_beef,
        };
        let bytes = encode_stat(&stat, 0x99);
        let at64 = |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let at32 = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(at64(0), 0x99, "st_dev");
        assert_eq!(at64(8), 0xfeed_face_dead_beef, "st_ino");
        assert_eq!(at32(16), S_IFREG | MODE_FILE_RW, "st_mode");
        assert_eq!(at32(20), 1, "st_nlink");
        assert_eq!(at32(24), 0, "st_uid");
        assert_eq!(at32(28), 0, "st_gid");
        assert_eq!(at64(32), 0, "st_rdev");
        assert_eq!(at64(40), 0, "__pad1");
        assert_eq!(at64(48), 0x1234_5678_9abc, "st_size");
        assert_eq!(at32(56), IO_BLOCK as u32, "st_blksize");
        assert_eq!(at64(64), 0x1234_5678_9abcu64.div_ceil(512), "st_blocks");
        assert_eq!((at64(72), at64(80)), (0x1111_2222, 333), "st_atim");
        assert_eq!((at64(88), at64(96)), (0x4444_5555, 666), "st_mtim");
        assert_eq!((at64(104), at64(112)), (0, 0), "st_ctim, which the host did not keep");
    }

    /// `st_mode` reports the type exactly and the permissions from the one host fact there is.
    #[test]
    fn the_file_type_bits_are_exact_and_the_permission_bits_come_from_one_host_fact() {
        let base = FileStat {
            kind: FileKind::Regular,
            size: 0,
            read_only: false,
            accessed: None,
            modified: None,
            created: None,
            identity: 1,
        };
        assert_eq!(mode_for(&base) & S_IFMT, S_IFREG);
        assert_eq!(mode_for(&base) & 0o777, MODE_FILE_RW);
        let read_only = FileStat { read_only: true, ..base };
        assert_eq!(mode_for(&read_only) & 0o777, MODE_FILE_RO);
        // The writable bit tracks the one fact, which is what makes `stat` and `access(W_OK)`
        // agree with each other.
        assert_ne!(mode_for(&base) & 0o200, 0, "a writable file must report S_IWUSR");
        assert_eq!(mode_for(&read_only) & 0o200, 0, "a read-only file must not");
        let dir = FileStat { kind: FileKind::Directory, ..base };
        assert_eq!(mode_for(&dir) & S_IFMT, S_IFDIR);
        assert_eq!(mode_for(&dir) & 0o777, MODE_DIR_RW);
        let link = FileStat { kind: FileKind::Symlink, ..base };
        assert_eq!(mode_for(&link), S_IFLNK | 0o777, "Linux reports 0777 for every symlink");
        // A standard stream: a character device, which is what a real fstat on one reports.
        let other = FileStat { kind: FileKind::Other, ..base };
        assert_eq!(mode_for(&other) & S_IFMT, S_IFCHR);
    }

    /// `statvfs` puts every field where bionic's header says, and zeroes exactly three.
    #[test]
    fn statvfs_writes_the_volumes_numbers_and_zeroes_only_the_inode_counts() {
        let stats = VolumeStats {
            block_size: 4096,
            blocks: 1_000_000,
            blocks_free: 400_000,
            blocks_available: 300_000,
            name_max: 255,
            filesystem_id: 0xdead_beef,
            read_only: false,
        };
        let bytes = encode_statvfs(&stats);
        let at = |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        assert_eq!(at(0), 4096, "f_bsize");
        assert_eq!(at(8), 4096, "f_frsize");
        assert_eq!(at(16), 1_000_000, "f_blocks");
        assert_eq!(at(24), 400_000, "f_bfree");
        assert_eq!(at(32), 300_000, "f_bavail");
        assert_eq!((at(40), at(48), at(56)), (0, 0, 0), "the three inode counts");
        assert_eq!(at(64), 0xdead_beef, "f_fsid");
        assert_eq!(at(72), 0, "f_flag");
        assert_eq!(at(80), 255, "f_namemax");
        assert!(bytes[88..].iter().all(|b| *b == 0), "__f_reserved");
        let read_only = VolumeStats { read_only: true, ..stats };
        let bytes = encode_statvfs(&read_only);
        assert_eq!(u64::from_le_bytes(bytes[72..80].try_into().unwrap()), ST_RDONLY);
    }

    /// `statfs` is the kernel's asm-generic layout, not `statvfs`'s: `f_type` first, the fsid as
    /// two ints at 56, and `f_frsize` **after** `f_namelen`. Literal offsets, every word distinct,
    /// so a swapped pair cannot pass.
    #[test]
    fn statfs_is_the_kernels_layout_with_ext4_and_st_valid() {
        assert_eq!(STATFS_BYTES, 120, "asm-generic/statfs.h with 64-bit words");
        assert_eq!(EXT4_SUPER_MAGIC, 0xEF53);
        assert_eq!(PROC_SUPER_MAGIC, 0x9FA0);
        assert_eq!(PIPEFS_MAGIC.to_be_bytes()[4..], *b"PIPE");
        assert_eq!(SOCKFS_MAGIC.to_be_bytes()[4..], *b"SOCK");
        assert_eq!(ANON_INODE_FS_MAGIC, 0x0904_1934);
        assert_eq!((ST_VALID, ST_RELATIME), (0x20, 0x1000));
        let stats = VolumeStats {
            block_size: 4096,
            blocks: 1_000_000,
            blocks_free: 400_000,
            blocks_available: 300_000,
            name_max: 254,
            filesystem_id: 0x1122_3344_5566_7788,
            read_only: false,
        };
        let bytes = StatfsAnswer::volume(&stats).encode();
        let at = |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let at32 = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(at(0), 0xEF53, "f_type");
        assert_eq!(at(8), 4096, "f_bsize");
        assert_eq!(at(16), 1_000_000, "f_blocks");
        assert_eq!(at(24), 400_000, "f_bfree");
        assert_eq!(at(32), 300_000, "f_bavail");
        assert_eq!((at(40), at(48)), (0, 0), "f_files and f_ffree, as statvfs reports them");
        assert_eq!((at32(56), at32(60)), (0x5566_7788, 0x1122_3344), "f_fsid.val[0], val[1]");
        assert_eq!(at(64), 254, "f_namelen");
        assert_eq!(at(72), 4096, "f_frsize");
        assert_eq!(at(80), ST_VALID, "f_flags");
        assert!(bytes[88..].iter().all(|b| *b == 0), "f_spare");
        let bytes = StatfsAnswer::volume(&VolumeStats { read_only: true, ..stats }).encode();
        assert_eq!(u64::from_le_bytes(bytes[80..88].try_into().unwrap()), ST_VALID | ST_RDONLY);

        let bytes = StatfsAnswer::pseudo(PIPEFS_MAGIC, 16384, ST_VALID).encode();
        let at = |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        assert_eq!((at(0), at(8), at(72)), (PIPEFS_MAGIC, 16384, 16384), "type, bsize, frsize");
        assert_eq!((at(64), at(80)), (255, ST_VALID), "namelen, flags");
        assert!((16..64).step_by(8).all(|o| at(o) == 0), "no counts and no fsid");
    }

    /// A `struct dirent` carries its name NUL-terminated, with the type byte at 18.
    #[test]
    fn a_dirent_carries_its_name_terminated_and_its_type_where_the_header_says() {
        let entry = DirEntryInfo {
            name: "libroblox.so".to_string(),
            kind: FileKind::Regular,
            identity: 0x1234,
        };
        let bytes = encode_dirent(&entry, 7);
        assert_eq!(u64::from_le_bytes(bytes[0..8].try_into().unwrap()), 0x1234, "d_ino");
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 7, "d_off");
        assert_eq!(u16::from_le_bytes(bytes[16..18].try_into().unwrap()), 280, "d_reclen");
        assert_eq!(bytes[18], DT_REG, "d_type");
        assert_eq!(&bytes[19..31], b"libroblox.so");
        assert_eq!(bytes[31], 0, "d_name must be terminated");
        // The longest name the seam can produce still terminates inside the array.
        let long = DirEntryInfo {
            name: "x".repeat(omni_platform::fs::NAME_MAX),
            kind: FileKind::Directory,
            identity: 1,
        };
        let bytes = encode_dirent(&long, 0);
        assert_eq!(bytes[18], DT_DIR);
        assert_eq!(bytes[D_NAME_OFFSET + omni_platform::fs::NAME_MAX], 0, "the terminator fits");
        assert!(bytes[D_NAME_OFFSET + D_NAME_BYTES..].iter().all(|b| *b == 0), "and stays inside");
    }

    /// The `O_*` numbers are Linux's asm-generic values, which is what arm64 uses.
    ///
    /// Written as literals, because comparing a constant to itself passes against any value —
    /// the lesson `omni-bionic`'s errno table already carries.
    #[test]
    fn the_open_flags_are_the_linux_asm_generic_numbers() {
        assert_eq!([O_RDONLY, O_WRONLY, O_RDWR, O_ACCMODE], [0, 1, 2, 3]);
        assert_eq!(O_CREAT, 64);
        assert_eq!(O_EXCL, 128);
        assert_eq!(O_NOCTTY, 256);
        assert_eq!(O_TRUNC, 512);
        assert_eq!(O_APPEND, 1024);
        assert_eq!(O_NONBLOCK, 2048);
        assert_eq!(O_DSYNC, 4096);
        assert_eq!(O_ASYNC, 8192);
        assert_eq!(O_DIRECT, 16384);
        assert_eq!(O_LARGEFILE, 32768);
        assert_eq!(O_DIRECTORY, 65536);
        assert_eq!(O_NOFOLLOW, 131_072);
        assert_eq!(O_NOATIME, 262_144);
        assert_eq!(O_CLOEXEC, 524_288);
        assert_eq!(O_SYNC, 1_052_672, "O_SYNC contains O_DSYNC's bit");
        assert_eq!(O_PATH, 2_097_152);
        assert_eq!(O_TMPFILE, 4_259_840, "O_TMPFILE contains O_DIRECTORY's bit");
        assert_eq!([F_OK, X_OK, W_OK, R_OK], [0, 1, 2, 4]);
        assert_eq!(MAX_COUNT, 0x7fff_ffff_ffff_ffff, "Linux's SSIZE_MAX rule");
        // The two composite flags are why the refusal tests the whole pattern rather than a bit:
        // testing `flags & O_SYNC != 0` would refuse every `O_DSYNC` open as `O_SYNC`, and
        // testing `flags & O_TMPFILE != 0` would refuse every `O_DIRECTORY` open.
        assert_eq!(O_SYNC & O_DSYNC, O_DSYNC);
        assert_eq!(O_TMPFILE & O_DIRECTORY, O_DIRECTORY);
    }

    /// Every classified host failure has exactly one errno, and the unclassified one has none.
    #[test]
    fn every_host_failure_kind_maps_to_one_distinct_errno_except_the_unclassified_one() {
        let mapped = [
            (FsErrorKind::NotFound, consts::ENOENT),
            (FsErrorKind::PermissionDenied, consts::EACCES),
            (FsErrorKind::AlreadyExists, consts::EEXIST),
            (FsErrorKind::NotADirectory, consts::ENOTDIR),
            (FsErrorKind::IsADirectory, consts::EISDIR),
            (FsErrorKind::DirectoryNotEmpty, consts::ENOTEMPTY),
            (FsErrorKind::InvalidInput, consts::EINVAL),
            (FsErrorKind::StorageFull, consts::ENOSPC),
            (FsErrorKind::FileTooLarge, consts::EFBIG),
            (FsErrorKind::TooManyOpenFiles, consts::EMFILE),
            (FsErrorKind::ReadOnlyFilesystem, consts::EROFS),
            (FsErrorKind::BadDescriptor, consts::EBADF),
            (FsErrorKind::NameTooLong, consts::ENAMETOOLONG),
            // M5: a pipe is the only thing on this seam that can produce either.
            (FsErrorKind::WouldBlock, consts::EAGAIN),
            (FsErrorKind::BrokenPipe, consts::EPIPE),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (kind, expected) in mapped {
            assert_eq!(errno_for(kind), Some(expected), "{kind:?}");
            assert!(seen.insert(expected), "two kinds share errno {expected}");
        }
        // **The one that must NOT have an errno.** `EIO` is a real answer guest code retries and
        // reports; an error nobody identified deserves the refusal that names it.
        assert_eq!(
            errno_for(FsErrorKind::Other),
            None,
            "an unclassified host failure must be refused by name, not given EIO"
        );
    }
}
