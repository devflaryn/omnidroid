//! Windows backend for the filesystem seam: the operations with no portable `std` spelling --
//! `pread`, `pwrite` and `statvfs`.
//!
//! Everything else in [`crate::fs`] is `std::fs` and is implemented once for all five targets.
//! These two are here because they need a *different* call per target rather than one portable
//! one — which is the distinction D22 drew and the reason this file is short.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetVolumeInformationW,
};

use super::error::{FsError, FsErrorKind, FsResult};
use super::VolumeStats;

/// `FILE_READ_ONLY_VOLUME` from `winnt.h`, one of `GetVolumeInformationW`'s flag bits.
///
/// Spelled here rather than imported: `windows-sys` files it under `Win32_System_SystemServices`,
/// and enabling that whole feature module for one documented constant is a larger dependency
/// surface than writing the number down. The value is stable published API — the same standing
/// the Linux UAPI numbers in `omni-android`'s adapter have.
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

/// `pread(2)` on Windows: `FileExt::seek_read` **with the descriptor's own offset saved and put
/// back**, because `seek_read` alone is not `pread`.
///
/// # MEASURED, and the first version of this function was wrong
///
/// `FileExt::seek_read` is `ReadFile` with an `OVERLAPPED` offset, and for a *synchronous* handle
/// Windows updates the file pointer from that `OVERLAPPED` — so the call leaves the cursor at the
/// end of what it read rather than where it found it. The standard library does not undo that;
/// its documentation says so, and this was measured rather than read:
///
/// | step | on a ten-byte file | expected of `pread` | what `seek_read` alone gave |
/// |---|---|---|---|
/// | `read(4)` | `0123`, cursor 4 | — | — |
/// | `pread(3, offset 7)` | `789` | cursor still 4 | cursor 10 |
/// | `read(3)` | — | `456` | **0 bytes: end of file** |
///
/// That is the exact silent-wrong-answer shape this seam exists to avoid: every call returns
/// `Ok`, nothing is reported, and the guest's *sequential* reads quietly skip to the end of the
/// file the first time anything preads. `pread`'s whole contract is that it does not disturb the
/// descriptor, which is why it is a primitive here rather than a seek and a read in the caller.
///
/// So the position is saved before and restored after, **including when the read itself fails** —
/// a failed `pread` that moved the cursor would be the same defect with an error code attached.
/// `Seek` is implemented for `&File`, so this needs no unique borrow and no `unsafe`.
pub(super) fn pread(file: &File, buf: &mut [u8], offset: u64) -> FsResult<usize> {
    let mut handle: &File = file;
    let saved = handle
        .stream_position()
        .map_err(|error| FsError::io("pread", "a descriptor", &error))?;
    let read = file.seek_read(buf, offset);
    let restored = handle.seek(SeekFrom::Start(saved));
    let count = read.map_err(|error| FsError::io("pread", "a descriptor", &error))?;
    restored.map_err(|error| FsError::io("pread", "a descriptor", &error))?;
    Ok(count)
}

/// `pwrite(2)` on Windows: `FileExt::seek_write` with the descriptor's own offset saved and put
/// back, for exactly [`pread`]'s measured reason.
///
/// `seek_write` is `WriteFile` with an `OVERLAPPED` offset, and on a synchronous handle Windows
/// moves the file pointer to the end of what it wrote -- the same behaviour `pread`'s table
/// measured for `seek_read`, from the same `OVERLAPPED` rule. Left alone, a guest that `pwrite`s a
/// page and then `write`s sequentially would put the second write after the page rather than
/// where its own offset was, and every call would report success. So the position is saved
/// before and restored after, including when the write fails.
///
/// One call, short writes reported: `pwrite`'s contract is not "write all of it", and a loop here
/// would make the same guest program behave differently on two hosts (see `pread`).
pub(super) fn pwrite(file: &File, buf: &[u8], offset: u64) -> FsResult<usize> {
    let mut handle: &File = file;
    let saved = handle
        .stream_position()
        .map_err(|error| FsError::io("pwrite", "a descriptor", &error))?;
    let written = file.seek_write(buf, offset);
    let restored = handle.seek(SeekFrom::Start(saved));
    let count = written.map_err(|error| FsError::io("pwrite", "a descriptor", &error))?;
    restored.map_err(|error| FsError::io("pwrite", "a descriptor", &error))?;
    Ok(count)
}

/// `fallocate(2)` mode 0 on Windows: the file made at least `end` bytes long, never shorter.
///
/// **Extending is allocating here.** `File::set_len` is `SetEndOfFile`, and on NTFS a file that is
/// not sparse -- none this seam creates is -- gets its clusters reserved when its end moves out:
/// the allocation size grows with the file size, and a volume without the space fails the call
/// (`ERROR_DISK_FULL`, `ENOSPC`) rather than a later write. A range inside the file is already
/// allocated for the same reason. So "`offset..offset + len` will not fail for space" is true of
/// this host once the file is `end` bytes long, which is the whole of `posix_fallocate`'s promise.
pub(super) fn allocate(file: &File, end: u64) -> FsResult<()> {
    let size = file.metadata().map_err(|error| FsError::io("fallocate", "a descriptor", &error))?.len();
    if end > size {
        file.set_len(end).map_err(|error| FsError::io("fallocate", "a descriptor", &error))?;
    }
    Ok(())
}

/// `statvfs(3)` on Windows, from three Win32 queries and no invented numbers.
///
/// | `statvfs` field | where it comes from |
/// |---|---|
/// | `f_bsize`, `f_frsize` | `GetDiskFreeSpaceW`: sectors per cluster × bytes per sector |
/// | `f_blocks` | `GetDiskFreeSpaceExW`'s total bytes ÷ the cluster size |
/// | `f_bfree` | `GetDiskFreeSpaceExW`'s total free bytes ÷ the cluster size |
/// | `f_bavail` | `GetDiskFreeSpaceExW`'s free-bytes-available-to-caller ÷ the cluster size |
/// | `f_namemax` | `GetVolumeInformationW`'s maximum component length |
/// | `f_fsid` | `GetVolumeInformationW`'s volume serial number |
/// | `f_flag`'s `ST_RDONLY` | `GetVolumeInformationW`'s `FILE_READ_ONLY_VOLUME` |
///
/// **`GetDiskFreeSpaceExW` for the byte counts and `GetDiskFreeSpaceW` for the cluster size**, and
/// the split matters: the older call's cluster counts are 32-bit and saturate at about 8 TB, while
/// the newer one has no cluster size in it. Using only the older one would report a 16 TB volume
/// as an 8 TB one, and using only the newer one would leave `f_bsize` to be guessed.
///
/// The three fields it does **not** answer are `f_files`, `f_ffree` and `f_favail`, and the
/// caller writes zero into them. That is not a placeholder: zero is what Linux reports for a
/// filesystem with no fixed inode table — FAT and exFAT do exactly this — and NTFS has none
/// either, because its MFT grows. Any other number would be a count of something that does not
/// exist.
pub(super) fn volume_stats(path: &Path) -> FsResult<VolumeStats> {
    let root = volume_root(path)?;
    let wide = wide(&root);

    // Cluster geometry. `GetDiskFreeSpaceW`'s cluster counts are the part that saturates; its
    // sector geometry does not, and that is all that is taken from it.
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call, and the four
    // output pointers are to live locals of exactly the `u32` the signature requires. The call
    // reads the string and writes the four words, and does nothing else.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &raw mut sectors_per_cluster,
            &raw mut bytes_per_sector,
            &raw mut free_clusters,
            &raw mut total_clusters,
        )
    };
    if ok == 0 {
        return Err(last_error("statvfs", &root, "GetDiskFreeSpaceW"));
    }
    let block_size = u64::from(sectors_per_cluster) * u64::from(bytes_per_sector);
    if block_size == 0 {
        return Err(FsError::refused(
            "statvfs",
            root.clone(),
            "the volume reported a zero cluster size, and a zero `f_bsize` divides by zero in \
             any guest code that uses it",
        ));
    }

    // Byte counts, 64-bit and quota-aware.
    let mut available_to_caller: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: as above; three `u64` outputs to live locals.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut available_to_caller,
            &raw mut total_bytes,
            &raw mut total_free,
        )
    };
    if ok == 0 {
        return Err(last_error("statvfs", &root, "GetDiskFreeSpaceExW"));
    }

    // Volume identity and limits.
    let mut serial: u32 = 0;
    let mut name_max: u32 = 0;
    let mut flags: u32 = 0;
    // SAFETY: the two buffer arguments are null with a zero length, which this call documents as
    // "do not return the name"; the three output pointers are to live `u32` locals.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            core::ptr::null_mut(),
            0,
            &raw mut serial,
            &raw mut name_max,
            &raw mut flags,
            core::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(last_error("statvfs", &root, "GetVolumeInformationW"));
    }

    Ok(VolumeStats {
        block_size,
        blocks: total_bytes / block_size,
        blocks_free: total_free / block_size,
        blocks_available: available_to_caller / block_size,
        name_max: u64::from(name_max),
        filesystem_id: u64::from(serial),
        read_only: flags & FILE_READ_ONLY_VOLUME != 0,
    })
}

/// The volume root a path lives on, as the three queries want it: `\\?\C:\` or `\\?\UNC\srv\sh\`.
///
/// Built from the path's own prefix rather than by string surgery, because a canonical Windows
/// path is verbatim (`\\?\`) and a UNC one has two components in its prefix.
fn volume_root(path: &Path) -> FsResult<String> {
    use std::path::Component;
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(FsError::refused(
            "statvfs",
            path.display().to_string(),
            "the host path has no volume prefix, so there is no volume to ask about",
        ));
    };
    let mut root = prefix.as_os_str().to_string_lossy().into_owned();
    if !root.ends_with('\\') {
        root.push('\\');
    }
    Ok(root)
}

/// A NUL-terminated UTF-16 string for a Win32 `W` entry point.
fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
}

/// The last Win32 error, classified by `std`'s own mapping rather than by a table here.
fn last_error(operation: &'static str, path: &str, api: &str) -> FsError {
    let error = std::io::Error::last_os_error();
    FsError::Io {
        operation,
        path: path.to_string(),
        kind: FsErrorKind::classify(&error),
        detail: format!("{api} failed: {error}"),
    }
}
