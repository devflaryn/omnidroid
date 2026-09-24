//! Shared unix body of the filesystem seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: three operations implemented and run on Linux, one structural for macOS
//!
//! **[`pread`], [`pwrite`] and [`volume_stats`] are implemented here and have been run on Linux**
//! (x86-64, kernel 7.0, glibc 2.43, on ext4 and tmpfs). They are POSIX -- `pread(2)`,
//! `pwrite(2)`, `statvfs(3)` -- and are written once for both unix targets. **None has been run on
//! macOS**, and the macOS notes below are read from its headers, not measured.
//!
//! [`allocate`] is **not** POSIX-common in practice: macOS has no `posix_fallocate`, and its
//! `F_PREALLOCATE` is a different call with a different contract. It is implemented in
//! [`linux`](super::linux), and the body here stays the structural refusal macOS's backend
//! re-exports.
//!
//! The other operations the seam offers are `std::fs` and are implemented once, with no backend
//! and no `cfg`, because D22's other half says a fabricated `Unsupported` for something `std`
//! already does on all five targets is a false claim in the *other* direction.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::error::{FsError, FsResult};
use super::VolumeStats;

/// The platform this backend was compiled for, for error messages.
#[cfg_attr(target_os = "linux", allow(dead_code))] // only the macOS-only refusal below uses it
fn platform() -> &'static str {
    std::env::consts::OS
}

#[cfg_attr(target_os = "linux", allow(dead_code))] // only the macOS-only refusal below uses it
fn unsupported<T>(operation: &'static str, intended: &'static str) -> FsResult<T> {
    Err(FsError::Unsupported { operation, intended, platform: platform() })
}

/// `pread(2)`, as `std::os::unix::fs::FileExt::read_at`: **one call, a short read reported.**
///
/// **`pread` does not move the descriptor's offset**, which is the whole of why it is a primitive
/// here -- and on unix that is the kernel's own guarantee, so there is nothing to save and
/// restore, unlike the Windows half, whose `seek_read` was MEASURED moving it (D23).
///
/// **Not looped**, and that is a contract decision rather than an omission. `read_at` returns
/// what the one call returned, and `pread`'s contract is that it may return fewer bytes than
/// asked -- at the end of the file, and wherever the host decides. A loop here would turn one
/// guest `pread` into several host ones and make the guest's own short-read handling
/// unreachable; the Windows half makes the same choice, so one guest program behaves the same on
/// both hosts. A caller that wants "fill this buffer" loops itself, as the seam's own
/// `read_for_mapping` does. `EINTR` is reported rather than retried for the same reason: it is
/// one of `pread`'s answers, and on a regular file on a local volume the kernel does not give it.
pub(super) fn pread(file: &File, buf: &mut [u8], offset: u64) -> FsResult<usize> {
    file.read_at(buf, offset).map_err(|error| FsError::io("pread", "a descriptor", &error))
}

/// `pwrite(2)`, as `std::os::unix::fs::FileExt::write_at` -- the mirror of [`pread`]: one call,
/// the offset untouched by the kernel's guarantee, a short write reported rather than completed.
///
/// One Linux rule a caller can trip over and this does not paper over: on a descriptor opened
/// with `O_APPEND`, Linux's `pwrite` **appends regardless of the offset** (`pwrite(2)`, BUGS).
/// That is the host's documented behaviour and the guest's own kernel's, so it is passed through.
pub(super) fn pwrite(file: &File, buf: &[u8], offset: u64) -> FsResult<usize> {
    file.write_at(buf, offset).map_err(|error| FsError::io("pwrite", "a descriptor", &error))
}

/// Structural on macOS: Linux's is in [`linux`](super::linux), `fallocate(2)` mode 0 through
/// `libc::posix_fallocate`, which macOS does not have.
///
/// The decision in it: **`File::set_len` is not enough here**, as it is on Windows. Extending a
/// file with `ftruncate` on ext4, f2fs or APFS makes a sparse tail, so a later write into it can
/// still fail with `ENOSPC` -- exactly what `posix_fallocate` promises will not happen. The unix
/// half has to ask the kernel to allocate.
#[cfg_attr(target_os = "linux", allow(dead_code))] // macOS re-exports it; Linux has its own
pub(super) fn allocate(_file: &File, _end: u64) -> FsResult<()> {
    unsupported("fallocate", "fallocate(2) mode 0 via libc::posix_fallocate")
}

/// `statvfs(3)`: the volume holding `path`, as the host reports it.
///
/// The decisions in it, and the first is the one this function exists to get right:
///
/// * **`f_frsize`, not `f_bsize`, is the unit the block counts are in.** `f_bsize` is the
///   *preferred I/O size*; `f_frsize` is the fundamental block size `f_blocks`, `f_bfree` and
///   `f_bavail` count (POSIX `<sys/statvfs.h>`). [`VolumeStats::block_size`] is documented as the
///   allocation unit the counts are in, so it is `f_frsize`. The two are equal on ext4 and tmpfs
///   -- MEASURED on every mount of this host, including squashfs (128 KiB each) and fuseblk (512)
///   -- which is exactly why getting them the wrong way round would never be noticed here; they
///   differ on NFS, where `f_bsize` is the transfer size. [`from_statvfs`] is the conversion,
///   tested on a record where they differ.
/// * **Read-only is `f_flag & ST_RDONLY`.** macOS's `statvfs` is a wrapper over `statfs` whose
///   flag word carries `MNT_*` bits; `MNT_RDONLY` is 1 there as `ST_RDONLY` is, and that equality
///   is read from the headers, not measured on a Mac.
/// * **The counts are widened, never narrowed.** `fsblkcnt_t` is 64-bit on Linux and 32-bit on
///   macOS, where a volume past 2^32 blocks saturates in the host's own answer before it reaches
///   here -- a macOS fact this function cannot repair.
/// * The guest is expecting **bionic's** `struct statvfs`, whose layout is the adapter's; this
///   returns host facts and nothing laid out.
pub(super) fn volume_stats(path: &Path) -> FsResult<VolumeStats> {
    use std::os::unix::ffi::OsStrExt;
    let shown = path.display().to_string();
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        FsError::refused(
            "statvfs",
            shown.clone(),
            "the resolved host path contains a NUL byte, which the confinement rules exclude, so \
             this path did not come from them",
        )
    })?;
    // SAFETY: a zeroed `statvfs` is valid plain data, and the call overwrites it entirely.
    let mut record: libc::statvfs = unsafe { core::mem::zeroed() };
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call, and `record` is a live,
    // uniquely-borrowed `statvfs` the call writes and nothing else.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut record) };
    if rc != 0 {
        return Err(FsError::io("statvfs", shown, &std::io::Error::last_os_error()));
    }
    from_statvfs(&record, &shown)
}

/// The host's `struct statvfs` as this seam's [`VolumeStats`]: `f_frsize` as the block size.
///
/// A zero `f_frsize` is refused rather than passed on -- it divides by zero in any guest code
/// that uses it, and no filesystem answers it -- which is the Windows half's rule for a zero
/// cluster size.
pub(super) fn from_statvfs(record: &libc::statvfs, path: &str) -> FsResult<VolumeStats> {
    #[allow(clippy::useless_conversion)] // `u64::from` is a widening on macOS and the identity here
    let block_size = u64::from(record.f_frsize);
    if block_size == 0 {
        return Err(FsError::refused(
            "statvfs",
            path.to_string(),
            "the volume reported a zero f_frsize, and a zero block size divides by zero in any \
             guest code that uses it",
        ));
    }
    #[allow(clippy::useless_conversion)]
    Ok(VolumeStats {
        block_size,
        blocks: u64::from(record.f_blocks),
        blocks_free: u64::from(record.f_bfree),
        blocks_available: u64::from(record.f_bavail),
        name_max: u64::from(record.f_namemax),
        filesystem_id: u64::from(record.f_fsid),
        read_only: record.f_flag & libc::ST_RDONLY != 0,
    })
}
