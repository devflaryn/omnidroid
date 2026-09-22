//! Shared unix body of the filesystem seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: structural, not implemented — and it is two operations, not seventeen
//!
//! **Nothing in this module has ever been run.** It holds exactly the two operations that have no
//! single portable `std` spelling; the other fifteen the seam offers are `std::fs` and are
//! implemented once, with no backend and no `cfg`, because D22's other half says a fabricated
//! `Unsupported` for something `std` already does on all five targets is a false claim in the
//! *other* direction.
//!
//! So the honest statement about this crate on Linux and macOS is precise rather than blanket:
//! the fifteen portable operations are written and have never been built for those targets, and
//! these two are not written at all and say so by name.
//!
//! # What implementing these involves
//!
//! Neither is hard. Both have a decision in them that has to be made by reading, not guessing.

use std::fs::File;
use std::path::Path;

use super::error::{FsError, FsResult};
use super::VolumeStats;

/// The platform this backend was compiled for, for error messages.
fn platform() -> &'static str {
    std::env::consts::OS
}

fn unsupported<T>(operation: &'static str, intended: &'static str) -> FsResult<T> {
    Err(FsError::Unsupported { operation, intended, platform: platform() })
}

/// Intended: `pread(2)`, reachable from `std` as `std::os::unix::fs::FileExt::read_at`.
///
/// The decision in it: **`pread` may return short and may return `EINTR`**, and which of those a
/// correct implementation loops on is a contract question rather than a typing one. `read_at`
/// returns what the one call returned, so a caller that wants "fill this buffer" has to loop —
/// and `pread`'s own contract is that it does *not* fill, so looping here would be wrong. The
/// Windows half makes the same choice (`seek_read`, one call, short reads reported), and a unix
/// implementation that looped would make the same guest program behave differently on two hosts.
///
/// Note that this is **not** the "portable `std` needs no backend" case: `read_at` and Windows'
/// `seek_read` are two different traits in two different modules, so there is no one call to
/// write once. That is the whole reason this operation has a backend at all.
pub(super) fn pread(_file: &File, _buf: &mut [u8], _offset: u64) -> FsResult<usize> {
    unsupported("pread", "pread(2) via std::os::unix::fs::FileExt::read_at")
}

/// Intended: `pwrite(2)`, reachable from `std` as `std::os::unix::fs::FileExt::write_at` -- the
/// mirror of [`pread`], with the same one-call, short-writes-reported decision, and added with the
/// Windows half when the engine's SQLite first reached it. Not written here, for `pread`'s reason.
pub(super) fn pwrite(_file: &File, _buf: &[u8], _offset: u64) -> FsResult<usize> {
    unsupported("pwrite", "pwrite(2) via std::os::unix::fs::FileExt::write_at")
}

/// Intended: `statvfs(3)` on both, through `libc::statvfs`.
///
/// The decisions in it, and they differ between the two unix targets:
///
/// * **`f_frsize` and `f_bsize` are not the same field on Linux.** `f_bsize` is the preferred I/O
///   size and `f_frsize` is the fragment size the block counts are *in*; a caller computing free
///   bytes must multiply by `f_frsize`. Getting that pair the wrong way round produces a free
///   space figure that is wrong by a small integer factor and looks entirely plausible. The
///   Windows half sets both to the cluster size, which is true there.
/// * **macOS reports `f_flag` differently**: `MNT_RDONLY` is its own numbering rather than
///   POSIX's `ST_RDONLY`, so the flag word has to be translated rather than passed through.
/// * The guest is expecting **bionic's** `struct statvfs`, whose field order is not the host's on
///   either target, so the adapter's encoder stays the one place that layout lives. This function
///   returns host facts and nothing laid out.
pub(super) fn volume_stats(_path: &Path) -> FsResult<VolumeStats> {
    unsupported("statvfs", "statvfs(3)")
}
