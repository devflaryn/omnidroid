//! macOS backend for the filesystem seam: the operations with no portable `std` spelling --
//! `pread`, `pwrite`, `fallocate` and `statvfs`.
//!
//! Implemented and run on macOS 26.5 (APFS). Three things differ from Linux and are handled here
//! rather than assumed:
//!
//! * **`pread`/`pwrite` are the real calls**, through `std::os::unix::fs::FileExt`, and unlike
//!   Windows' `seek_read` they never move the descriptor's offset -- so there is no save-and-restore
//!   dance here, and the test that pins Windows' fix pins this too.
//! * **There is no `posix_fallocate`.** The documented macOS route is `fcntl(F_PREALLOCATE)` with an
//!   `fstore_t` (contiguous first, then any extents), which reserves blocks past the end of file
//!   without changing its length, followed by `ftruncate` to move the end into them.
//! * **`statvfs(3)` is a compatibility wrapper over `statfs(2)`**, so `statfs` is asked directly:
//!   its `f_flags` carries `MNT_RDONLY` in the mount numbering (translated, not passed through) and
//!   its counts are in `f_bsize` units. `statfs` has no name-length field; `pathconf(_PC_NAME_MAX)`
//!   is the host's answer for that.

use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::error::{FsError, FsErrorKind, FsResult};
use super::VolumeStats;

/// `pread(2)`: one call, short reads reported, the descriptor's offset untouched.
pub(super) fn pread(file: &File, buf: &mut [u8], offset: u64) -> FsResult<usize> {
    file.read_at(buf, offset).map_err(|error| FsError::io("pread", "a descriptor", &error))
}

/// `pwrite(2)`: one call, short writes reported, the descriptor's offset untouched.
pub(super) fn pwrite(file: &File, buf: &[u8], offset: u64) -> FsResult<usize> {
    file.write_at(buf, offset).map_err(|error| FsError::io("pwrite", "a descriptor", &error))
}

/// `fallocate(2)` mode 0: the file made at least `end` bytes long, with the blocks for it
/// **allocated**, never shorter.
///
/// `ftruncate` alone would leave a sparse tail on APFS, and a later write into it could still fail
/// with `ENOSPC` -- which is exactly what `posix_fallocate` promises will not happen. So the blocks
/// are asked for with `F_PREALLOCATE` first (`F_PEOFPOSMODE`: counted from the physical end of
/// file), contiguous if the volume can, any extents otherwise, and only then is the length moved.
/// A range already inside the file is already allocated, so a request that does not extend the file
/// changes nothing.
pub(super) fn allocate(file: &File, end: u64) -> FsResult<()> {
    let size = file.metadata().map_err(|error| FsError::io("fallocate", "a descriptor", &error))?.len();
    if end <= size {
        return Ok(());
    }
    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATECONTIG | libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: (end - size) as libc::off_t,
        fst_bytesalloc: 0,
    };
    // SAFETY: `store` is a live fstore_t the call reads and updates; the descriptor is `file`'s.
    let mut rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };
    if rc == -1 {
        // No contiguous run of that size: any extents will do, which is all fallocate promises.
        store.fst_flags = libc::F_ALLOCATEALL;
        // SAFETY: as above.
        rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };
    }
    if rc == -1 {
        let error = std::io::Error::last_os_error();
        return Err(FsError::io("fallocate", "a descriptor", &error));
    }
    file.set_len(end).map_err(|error| FsError::io("fallocate", "a descriptor", &error))
}

/// `statvfs(3)` from `statfs(2)` and `pathconf(3)`, no invented numbers.
///
/// | `statvfs` field | where it comes from |
/// |---|---|
/// | `f_bsize`, `f_frsize` | `statfs.f_bsize`, the unit `f_blocks`/`f_bfree`/`f_bavail` count in |
/// | `f_blocks`, `f_bfree`, `f_bavail` | `statfs` directly |
/// | `f_namemax` | `pathconf(path, _PC_NAME_MAX)` |
/// | `f_fsid` | `statfs.f_fsid`, its two 32-bit words |
/// | `f_flag`'s `ST_RDONLY` | `statfs.f_flags & MNT_RDONLY` |
pub(super) fn volume_stats(path: &Path) -> FsResult<VolumeStats> {
    let shown = path.display().to_string();
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        FsError::refused("statvfs", shown.clone(), "the host path contains a NUL byte")
    })?;
    // SAFETY: `statfs` is plain data; the call writes exactly one.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `stats` is writable.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stats) } != 0 {
        return Err(last_error("statvfs", &shown, "statfs"));
    }
    let block_size = u64::from(stats.f_bsize);
    if block_size == 0 {
        return Err(FsError::refused(
            "statvfs",
            shown,
            "the volume reported a zero block size, and a zero `f_bsize` divides by zero in any \
             guest code that uses it",
        ));
    }
    // SAFETY: as above; pathconf reads the path and returns a long.
    let name_max = unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_NAME_MAX) };
    if name_max < 0 {
        return Err(last_error("statvfs", &shown, "pathconf(_PC_NAME_MAX)"));
    }
    // `fsid_t` is `struct { int32_t val[2]; }` (<sys/_types/_fsid_t.h>); libc keeps the field
    // private, so it is read as the two words it is.
    // SAFETY: `fsid_t` is `repr(C)` and exactly two `i32`s.
    let fsid: [i32; 2] = unsafe { std::mem::transmute(stats.f_fsid) };
    Ok(VolumeStats {
        block_size,
        blocks: stats.f_blocks,
        blocks_free: stats.f_bfree,
        blocks_available: stats.f_bavail,
        name_max: name_max as u64,
        filesystem_id: (u64::from(fsid[0] as u32) << 32) | u64::from(fsid[1] as u32),
        read_only: stats.f_flags & libc::MNT_RDONLY as u32 != 0,
    })
}

/// The last OS error, classified by `std`'s own mapping.
fn last_error(operation: &'static str, path: &str, api: &str) -> FsError {
    let error = std::io::Error::last_os_error();
    FsError::Io {
        operation,
        path: path.to_string(),
        kind: FsErrorKind::classify(&error),
        detail: format!("{api} failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("omni-fs-macos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir.join(name)
    }

    #[test]
    fn pread_and_pwrite_leave_the_descriptor_offset_alone() {
        let path = scratch("offset");
        std::fs::write(&path, b"0123456789").expect("write");
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("open");
        let mut head = [0u8; 4];
        file.read_exact(&mut head).expect("read 4");
        let mut tail = [0u8; 3];
        assert_eq!(pread(&file, &mut tail, 7).expect("pread"), 3);
        assert_eq!(&tail, b"789");
        assert_eq!(pwrite(&file, b"XY", 0).expect("pwrite"), 2);
        assert_eq!(file.stream_position().expect("position"), 4, "the cursor did not move");
        let mut next = [0u8; 3];
        file.read_exact(&mut next).expect("read 3");
        assert_eq!(&next, b"456");
        file.seek(SeekFrom::Start(0)).expect("seek");
        let mut all = Vec::new();
        file.read_to_end(&mut all).expect("read all");
        assert_eq!(all, b"XY23456789");
    }

    #[test]
    fn allocate_extends_with_blocks_behind_it_and_never_shrinks() {
        use std::os::unix::fs::MetadataExt;
        let path = scratch("allocate");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"abc").expect("write");
        allocate(&file, 1 << 20).expect("allocate 1 MiB");
        let meta = file.metadata().expect("metadata");
        assert_eq!(meta.len(), 1 << 20);
        // The blocks are really there: st_blocks counts 512-byte units actually allocated. A plain
        // ftruncate leaves this near zero on APFS.
        assert!(meta.blocks() * 512 >= 1 << 20, "only {} bytes allocated", meta.blocks() * 512);
        allocate(&file, 10).expect("a smaller end is not a shrink");
        assert_eq!(file.metadata().expect("metadata").len(), 1 << 20);
    }

    #[test]
    fn volume_stats_are_the_hosts_own() {
        let stats = volume_stats(&std::env::temp_dir()).expect("statvfs of the temp dir");
        assert!(stats.block_size >= 512 && stats.block_size.is_power_of_two());
        assert!(stats.blocks > 0 && stats.blocks_free <= stats.blocks);
        assert!(stats.blocks_available <= stats.blocks_free);
        assert_eq!(stats.name_max, 255, "APFS allows 255-byte names");
        assert!(!stats.read_only);
        // The sealed system volume is mounted read-only on every current macOS.
        assert!(volume_stats(Path::new("/System")).expect("statvfs of /System").read_only);
    }
}
