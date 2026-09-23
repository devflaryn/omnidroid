//! Linux backend for the filesystem seam.
//!
//! **Implemented, and run on Linux x86-64** (Ubuntu 26.04, kernel 7.0.0, glibc 2.43; ext4 and
//! tmpfs). Not run on Linux ARM64. The four operations with no portable `std` spelling are:
//!
//! | operation | here | call |
//! |---|---|---|
//! | [`pread`](super::unix::pread), [`pwrite`](super::unix::pwrite) | shared [`unix`](super::unix) | `pread(2)`/`pwrite(2)` via `FileExt`, one call, short results reported |
//! | [`volume_stats`](super::unix::volume_stats) | shared [`unix`](super::unix) | `statvfs(3)`, block counts in `f_frsize` units |
//! | [`allocate`] | **this file** | `posix_fallocate(3)`, whose error is its return value |
//!
//! Every other operation the seam offers is `std::fs` and is implemented once for all five
//! targets, which is D22's rule applied in the direction it points.
//!
//! # Confinement on this host, executed for the first time
//!
//! [`path`](super::path)'s symlink rules could never run on the Windows host whose suite was their
//! evidence (VERIFICATION entry 4: `WinError 1314`, and a junction as the stand-in). Here a
//! symlink is one unprivileged call, so `tests/fs_linux.rs` builds every shape of link a root
//! could contain -- a file link and a directory link out of the root, a link as an intermediate
//! component, a dangling link, a link to a sibling instance's root -- and drives every
//! path-taking operation through each. Findings are recorded in
//! `docs/ports/linux-notes/posix.md`.
//!
//! What is still true here and is not closed: the check and the use are two calls, so a symlink
//! created inside the root **between** them by a second party is followed. `openat(2)` with
//! `O_NOFOLLOW` per component from a descriptor held on the root would close it on this target;
//! that is a change to the shared resolver and to every operation that takes a path, and it is
//! recorded rather than made in a backend file.
//!
//! The guest is an Android ARM64 binary and on a Linux ARM64 host its own `struct stat` would be
//! the host's layout, so the adapter's encoder would be doing a conversion that happens to be the
//! identity -- the target where a layout error in it would be hardest to notice.

use std::fs::File;
use std::os::fd::AsRawFd;

use super::error::{FsError, FsResult};

pub(super) use super::unix::{pread, pwrite, volume_stats};

/// `posix_fallocate(fd, 0, end)`: the bytes `0..end` of the file allocated, so that no write
/// into the range the guest named can fail for space; the file extended to `end` when it is
/// shorter, and never shortened.
///
/// # Allocation, not `set_len`, because the file system here is not NTFS
///
/// The Windows half extends with `SetEndOfFile`, which reserves clusters on NTFS. `ftruncate` on
/// ext4, tmpfs or f2fs does **not**: it makes a sparse tail, and a later write into it can still
/// fail with `ENOSPC` -- the one thing `posix_fallocate` promises will not happen. So this asks
/// the kernel to allocate. tmpfs and ext4 both implement `fallocate(2)` mode 0; on one that does
/// not, glibc emulates it by writing a zero byte into each block that is not already allocated,
/// which keeps the promise and preserves the file's contents.
///
/// # From 0, not from the guest's offset
///
/// The backend is handed only the range's **end** (the seam's signature, shared with Windows),
/// so it allocates `0..end`, a superset of the range the guest named. That keeps the promise and
/// can over-allocate one thing: holes the guest left *before* its offset in a sparse file, which
/// a file this runtime writes sequentially does not have. Allocating exactly the guest's range is
/// a signature change in shared code, and is recorded in the merge notes rather than made here.
///
/// # The error is the return value, and that is the trap
///
/// `posix_fallocate` returns an **error number** and does not set `errno`; it never returns
/// `-1`. A body that tested `rc == -1` and read `errno` would report every failure -- `ENOSPC`,
/// `EFBIG` -- as success, which for a call whose whole meaning is "a later write will not fail
/// for space" is the exact inversion of its promise. `EINTR` is retried: the call is idempotent,
/// and the guest has no signal delivery in this runtime that could have wanted to interrupt it.
pub(super) fn allocate(file: &File, end: u64) -> FsResult<()> {
    if end == 0 {
        // An empty range at offset 0: reachable from `Filesystem::fallocate(fd, 0, 0)`, which
        // does not check the length (the adapter answers a zero-length *guest* call EINVAL before
        // it gets there). `posix_fallocate` would say EINVAL; the Windows half answers `Ok` for
        // the same call, and nothing is left unallocated, so this matches it.
        return Ok(());
    }
    let Ok(len) = libc::off_t::try_from(end) else {
        return Err(FsError::kinded(
            "fallocate",
            "a descriptor",
            super::FsErrorKind::FileTooLarge,
            format!("the range ends at {end}, past off_t (EFBIG)"),
        ));
    };
    loop {
        // SAFETY: a live descriptor this `File` owns and two by-value offsets; no memory crosses.
        let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) };
        match rc {
            0 => return Ok(()),
            libc::EINTR => continue,
            errno => {
                return Err(FsError::io(
                    "fallocate",
                    "a descriptor",
                    &std::io::Error::from_raw_os_error(errno),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::unix::from_statvfs;
    use super::super::FsErrorKind;
    use super::*;

    /// A `struct statvfs` with every field distinct, **`f_bsize` unlike `f_frsize`** -- NFS's
    /// shape, where `f_bsize` is the 1 MiB transfer size and the counts are in 4 KiB fragments.
    ///
    /// Built by hand because no mount on this host has the two unequal (MEASURED: every mount,
    /// `stat -f -c '%s %S'`), so a live test cannot tell the two fields apart. This is the one
    /// place the conversion is shown picking the right one, and the live test in
    /// `tests/fs_linux.rs` shows the values reach it from a real `statvfs`.
    fn nfs_like() -> libc::statvfs {
        // SAFETY: plain data; every field this test reads is then written.
        let mut record: libc::statvfs = unsafe { core::mem::zeroed() };
        record.f_bsize = 1 << 20;
        record.f_frsize = 4096;
        record.f_blocks = 1_000_000;
        record.f_bfree = 600_000;
        record.f_bavail = 500_000;
        record.f_namemax = 255;
        record.f_fsid = 0x1234_5678_9abc;
        record.f_flag = libc::ST_RDONLY | libc::ST_NOSUID;
        record
    }

    #[test]
    fn the_block_size_is_the_fragment_size_the_counts_are_in() {
        let stats = from_statvfs(&nfs_like(), "/nfs").expect("a well-formed record");
        assert_eq!(stats.block_size, 4096, "f_frsize, not f_bsize's 1 MiB");
        assert_eq!(stats.blocks, 1_000_000);
        assert_eq!(stats.blocks_free, 600_000);
        assert_eq!(stats.blocks_available, 500_000);
        assert_eq!(stats.name_max, 255);
        assert_eq!(stats.filesystem_id, 0x1234_5678_9abc);
        assert!(stats.read_only, "ST_RDONLY is set among other flags");
        let mut writable = nfs_like();
        writable.f_flag = libc::ST_NOSUID;
        assert!(!from_statvfs(&writable, "/nfs").expect("record").read_only);
    }

    #[test]
    fn a_zero_fragment_size_is_refused_rather_than_divided_by() {
        let mut broken = nfs_like();
        broken.f_frsize = 0;
        let error = from_statvfs(&broken, "/nfs").expect_err("a zero block size");
        assert!(matches!(error, FsError::Refused { .. }), "{error}");
        assert!(error.to_string().contains("f_frsize"), "{error}");
    }

    /// `posix_fallocate`'s error arrives as its return value and is reported, not lost: a range
    /// past what the volume can hold is `EFBIG` or `ENOSPC`, and the file is left as it was.
    ///
    /// 1 PiB: past ext4's 16 TiB file limit (`EFBIG`) and past this host's tmpfs (`ENOSPC`, 3.6 GB
    /// here). Either is the kernel speaking; a body that read `errno` after a `-1` that never
    /// comes would answer `Ok(())` and fail this.
    #[test]
    fn a_range_the_volume_cannot_hold_is_the_hosts_error_and_changes_nothing() {
        let path = std::env::temp_dir().join(format!("omni-falloc-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("a scratch file");
        std::fs::write(&path, b"0123").expect("four bytes");
        let error = allocate(&file, 1 << 50).expect_err("1 PiB cannot be allocated here");
        let kind = error.kind();
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(kind, Some(FsErrorKind::StorageFull | FsErrorKind::FileTooLarge)),
            "{error}"
        );
        assert_eq!(file.metadata().expect("metadata").len(), 4, "a failed allocation grew the file");
    }
}
