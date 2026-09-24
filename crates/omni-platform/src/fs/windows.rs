//! Windows backend for the filesystem seam: the operations with no portable `std` spelling --
//! `pread`, `pwrite` and `statvfs`.
//!
//! Everything else in [`crate::fs`] is `std::fs` and is implemented once for all five targets.
//! These two are here because they need a *different* call per target rather than one portable
//! one — which is the distinction D22 drew and the reason this file is short.
//!
//! # And one place where Windows refuses what Linux does: shortening a mapped file
//!
//! MEASURED (Windows 11 26200, NTFS, a `PAGE_READWRITE` section over a share-everything handle):
//!
//! | while the file has... | shorten (`SetEndOfFile`, `CREATE_ALWAYS`, `TRUNCATE_EXISTING`) | same size | grow |
//! |---|---|---|---|
//! | a live view | **`ERROR_USER_MAPPED_FILE` (1224)** | ok | ok |
//! | a live view, its section handle closed | **1224** | -- | -- |
//! | a section handle and no view | **1224** | -- | -- |
//! | neither | ok | ok | ok |
//!
//! Linux (and so Android) shortens a file whatever maps it: the file's size is what it was
//! truncated to, the bytes past it are gone for every reader, and the mapping's pages past the new
//! end raise `SIGBUS` if touched. MEASURED, the engine depends on it: it re-opens
//! `LocalStorage/memProfStorage<pid>.json` with `O_TRUNC` while an earlier `MAP_SHARED` mapping of
//! the same file is still live, and the 1224 killed a TaskScheduler worker five seconds into a
//! session.
//!
//! So a refused shortening is kept as a **logical end of file** -- see [`truncate`] -- and every
//! size-visible operation of the seam honours it; the host's own `SetEndOfFile` is applied once
//! the last section of the file is gone ([`section_closed`]) or at any later `close` or truncate
//! the host accepts. Nothing of this exists on Linux or macOS, which shorten natively.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use windows_sys::Win32::Foundation::{
    ERROR_USER_MAPPED_FILE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetFileInformationByHandle, GetVolumeInformationW,
    ReOpenFile, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
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
///
/// A file with a [logical end](truncate) is written as Linux writes one: a gap between that end
/// and `offset` reads as zeros, and a write past the end moves it.
pub(super) fn pwrite(file: &File, buf: &[u8], offset: u64) -> FsResult<usize> {
    let io = |error: std::io::Error| FsError::io("pwrite", "a descriptor", &error);
    let Some(mut records) = records_for(file).map_err(io)? else {
        return pwrite_host(file, buf, offset);
    };
    let Some(record) = records.record() else {
        return pwrite_host(file, buf, offset);
    };
    record.fill_gap_to(offset).map_err(io)?;
    let written = pwrite_host(file, buf, offset)?;
    record.written_to(offset + written as u64);
    records.settle_if_whole().map_err(io)?;
    Ok(written)
}

/// [`pwrite`] as the host does it: `seek_write` with the offset put back.
fn pwrite_host(file: &File, buf: &[u8], offset: u64) -> FsResult<usize> {
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
///
/// A file with a [logical end](truncate) is measured by that end, as `fstat` reports it: a range
/// ending past it moves it, and the bytes it takes in read as zeros -- what `posix_fallocate` of a
/// file shortened on Linux gives.
pub(super) fn allocate(file: &File, end: u64) -> FsResult<()> {
    let io = |error: std::io::Error| FsError::io("fallocate", "a descriptor", &error);
    if let Some(mut records) = records_for(file).map_err(io)? {
        if let Some(record) = records.record() {
            if end > record.eof {
                record.fill_gap_to(end).map_err(io)?;
                if end > record.physical().map_err(io)? {
                    file.set_len(end).map_err(io)?;
                }
                record.written_to(end);
                records.settle_if_whole().map_err(io)?;
            }
            return Ok(());
        }
    }
    let size = file.metadata().map_err(io)?.len();
    if end > size {
        file.set_len(end).map_err(io)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- the logical end of file

/// A file's identity on this host: its volume's serial number and its file index, as
/// `GetFileInformationByHandle` reports them. Windows documents the pair as identifying one file
/// while it is open, whichever name reaches it -- so a rename (the engine renames a `.tmp` into
/// place) or a second name for the file does not lose its record, which a key made of the path
/// would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileId {
    volume: u32,
    index: u64,
}

/// One file shortened while mapped: where the guest's end of file is, and a handle of the
/// record's own to act on the file with.
#[derive(Debug)]
struct Logical {
    /// The file's size as every guest operation sees it. Never more than the host's size: a
    /// write or an allocation that would take it there takes the host's size with it.
    eof: u64,
    /// `GENERIC_READ | GENERIC_WRITE`, re-opened from the guest's handle (`ReOpenFile`), so the
    /// record can zero bytes, append and finally shorten the file whatever access the descriptor
    /// that asked had -- an `O_APPEND` handle here has no `FILE_WRITE_DATA` -- and after every
    /// descriptor is closed. A handle does not stop the host shortening the file; only a section
    /// does (MEASURED, the module table).
    file: File,
}

impl Logical {
    /// The host's size of the file.
    fn physical(&self) -> std::io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Before a write or an allocation that starts at `start`: the bytes between the end of file
    /// and `start` become part of the file, and on Linux they read as zeros. They were zeroed
    /// when the file was shortened, and are zeroed again because a store through a live view
    /// past the end may have landed in them since -- where Linux would have raised `SIGBUS`.
    fn fill_gap_to(&mut self, start: u64) -> std::io::Result<()> {
        if start > self.eof {
            let physical = self.physical()?;
            zero(&self.file, self.eof, start.min(physical))?;
        }
        Ok(())
    }

    /// After a write or allocation that reached `end`.
    fn written_to(&mut self, end: u64) {
        self.eof = self.eof.max(end);
    }

    /// Make the end of file `len`: the host's own shortening if it will do it now, the logical
    /// end if not. Returns whether the record is finished with -- the host's size is the file's.
    fn resize(&mut self, len: u64) -> std::io::Result<bool> {
        let physical = self.physical()?;
        if len < self.eof {
            // Bytes that stop being file. Linux's truncate zeroes the part of the last page past
            // the new end, and a later extension reads zeros; zeroing them here is both.
            zero(&self.file, len, self.eof.min(physical))?;
        } else {
            zero(&self.file, self.eof, len.min(physical))?;
        }
        // Growing, keeping the size, and shortening a file no section holds are all things the
        // host does -- and after any of them its size is the file's again.
        match self.file.set_len(len) {
            Ok(()) => Ok(true),
            Err(error) if is_user_mapped(&error) => {
                self.eof = len;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

/// Every file of this process that has a logical end of file, by identity.
///
/// **Process-wide, not per [`Filesystem`](super::Filesystem)**, because what it describes is the
/// host file: two guest instances holding one file, or a descriptor and a mapping in different
/// instances, see one size on Linux and must here.
static RECORDS: Mutex<BTreeMap<FileId, Logical>> = Mutex::new(BTreeMap::new());

/// How many entries [`RECORDS`] has, readable without its lock. Nearly always zero, and then every
/// operation of the seam is exactly what it was before logical ends existed: no identity query,
/// no lock.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// [`RECORDS`], locked, with the entry for one file picked out.
struct Records {
    map: MutexGuard<'static, BTreeMap<FileId, Logical>>,
    id: FileId,
}

impl Records {
    fn record(&mut self) -> Option<&mut Logical> {
        self.map.get_mut(&self.id)
    }

    fn remove(&mut self) {
        self.map.remove(&self.id);
        LIVE.store(self.map.len(), Ordering::Release);
    }

    /// Drop the record once the logical end has caught up with the host's size -- a write or an
    /// allocation that reached it leaves nothing for the host to shorten.
    fn settle_if_whole(&mut self) -> std::io::Result<()> {
        if let Some(record) = self.record() {
            if record.eof >= record.physical()? {
                self.remove();
            }
        }
        Ok(())
    }

    /// Try the host's shortening now; drop the record if it is done.
    fn apply(&mut self) {
        if let Some(record) = self.record() {
            if record.file.set_len(record.eof).is_ok() {
                self.remove();
            }
        }
    }
}

fn locked() -> MutexGuard<'static, BTreeMap<FileId, Logical>> {
    RECORDS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The records, locked and keyed to `file` -- or `None`, with no lock taken and no host call made,
/// when no file has a logical end at all.
fn records_for(file: &File) -> std::io::Result<Option<Records>> {
    if LIVE.load(Ordering::Acquire) == 0 {
        return Ok(None);
    }
    let id = file_id(file.as_raw_handle() as HANDLE)?;
    Ok(Some(Records { map: locked(), id }))
}

fn file_id(handle: HANDLE) -> std::io::Result<FileId> {
    // SAFETY: an all-zero `BY_HANDLE_FILE_INFORMATION` is valid plain data, and the call
    // overwrites it.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: `handle` is a live file handle the caller holds for the duration of the call, and
    // `info` is a live, uniquely borrowed record the call writes and nothing else.
    let ok = unsafe { GetFileInformationByHandle(handle, &raw mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileId {
        volume: info.dwVolumeSerialNumber,
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

fn is_user_mapped(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_USER_MAPPED_FILE as i32)
}

/// Zeros over `[from, to)` of `file`, through `WriteFile` -- which a live view of the file shows at
/// once (MEASURED): the view and the handle share the host's one cache of the file.
fn zero(file: &File, from: u64, to: u64) -> std::io::Result<()> {
    const CHUNK: usize = 64 * 1024;
    let zeros = [0u8; CHUNK];
    let mut at = from;
    while at < to {
        let take = usize::try_from(to - at).map_or(CHUNK, |left| left.min(CHUNK));
        let wrote = file.seek_write(&zeros[..take], at)?;
        if wrote == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        at += wrote as u64;
    }
    Ok(())
}

/// A handle of the record's own on the file `file` is open on: `GENERIC_READ | GENERIC_WRITE`,
/// sharing everything, as `std` shares.
fn reopen(file: &File) -> std::io::Result<File> {
    // SAFETY: `file`'s handle is live for the call. `ReOpenFile` opens a new handle to the same
    // file object and does not touch the one it is given.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle() as HANDLE,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` is a new, valid file handle this function owns and gives to the `File`.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

/// `ftruncate(2)`, and `open`'s `O_TRUNC`: make `file` exactly `len` bytes long **as Linux does,
/// mapped or not**.
///
/// The host's `SetEndOfFile` first. When it refuses with `ERROR_USER_MAPPED_FILE` -- a section of
/// the file is live, the module table -- the file keeps its host size and gets a **logical end of
/// file** at `len`, shared by every descriptor and mapping of the file in this process:
///
/// * `fstat`, `stat` and `lstat` report it as `st_size`; `read` and `pread` end there; `lseek`
///   `SEEK_END` counts from it; an `O_APPEND` write lands at it; a write or allocation past it
///   moves it, the gap reading as zeros; another truncate moves it again.
/// * The bytes between it and the host's size are **zeroed** now, so a later extension reads
///   zeros, as on Linux.
/// * When the host will shorten the file -- after the last section of it closes
///   ([`section_closed`], from the mapping seam), or at a `close` or truncate after that -- it is
///   shortened to the logical end and the record goes.
///
/// **What differs from Linux, and why it is accepted.** A live view keeps showing the file's pages
/// past the logical end, as zeros, and a store there lands in the host file (to be zeroed again by
/// an extension, and cut off by the final shortening). On Linux a *whole* page past the end raises
/// `SIGBUS` when touched -- only the rest of the last partial page reads zeros, as here. Touching
/// memory past the end of a file is undefined for a correct program, and the engine's
/// `memProfStorage` writer does not; raising `SIGBUS` would need this seam to own the view's page
/// protections, which the mapping seam does and this one cannot see. And a process that dies with
/// the view still live leaves the host file at its old size with a zeroed tail.
pub(super) fn truncate(file: &File, len: u64) -> std::io::Result<()> {
    if let Some(mut records) = records_for(file)? {
        if let Some(record) = records.record() {
            if record.resize(len)? {
                records.remove();
            }
            return Ok(());
        }
    }
    match file.set_len(len) {
        Ok(()) => Ok(()),
        Err(error) if is_user_mapped(&error) => {
            let own = reopen(file)?;
            let physical = own.metadata()?.len();
            zero(&own, len, physical)?;
            let id = file_id(own.as_raw_handle() as HANDLE)?;
            let mut map = locked();
            map.insert(id, Logical { eof: len, file: own });
            LIVE.store(map.len(), Ordering::Release);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// `open(2)` with the host's refusal of a truncating open turned into [`truncate`]: an `O_TRUNC`
/// open of a mapped file is refused whole by the host (1224, the module table), where Linux opens
/// it and shortens it.
pub(super) fn open(options: &std::fs::OpenOptions, host: &Path, truncating: bool) -> std::io::Result<File> {
    match options.open(host) {
        Ok(file) => {
            // The host shortened it, so nothing of an earlier logical end is left to honour.
            if truncating {
                forget(&file)?;
            }
            Ok(file)
        }
        Err(error) if truncating && is_user_mapped(&error) => {
            let mut again = options.clone();
            again.truncate(false);
            let file = again.open(host)?;
            truncate(&file, 0)?;
            Ok(file)
        }
        Err(error) => Err(error),
    }
}

fn forget(file: &File) -> std::io::Result<()> {
    if let Some(mut records) = records_for(file)? {
        if records.record().is_some() {
            records.remove();
        }
    }
    Ok(())
}

/// The logical end of the file `file` is open on, if it has one.
pub(super) fn logical_len(file: &File) -> Option<u64> {
    let mut records = records_for(file).ok()??;
    records.record().map(|record| record.eof)
}

/// The logical end of the file at `path`, if it has one -- for `stat`, which has a path and no
/// descriptor. The file is opened with no access at all (as `stat` needs none) only when some
/// file has a logical end.
pub(super) fn logical_len_at(path: &Path) -> Option<u64> {
    if LIVE.load(Ordering::Acquire) == 0 {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    logical_len(&file)
}

/// `write(2)` on a file that may have a logical end: at the descriptor's offset, or -- `O_APPEND`
/// -- at the end of file, which for such a file is the logical one and not the host's.
pub(super) fn write(file: &File, buf: &[u8], append: bool) -> std::io::Result<usize> {
    let mut handle: &File = file;
    let Some(mut records) = records_for(file)? else {
        return handle.write(buf);
    };
    let Some(record) = records.record() else {
        return handle.write(buf);
    };
    let start = if append { record.eof } else { handle.stream_position()? };
    record.fill_gap_to(start)?;
    let written = if append {
        // The descriptor's own handle appends at the host's end, so the record's writes instead,
        // and the descriptor's offset is left where Linux leaves it: after what was written.
        let written = record.file.seek_write(buf, start)?;
        handle.seek(SeekFrom::Start(start + written as u64))?;
        written
    } else {
        handle.write(buf)?
    };
    record.written_to(start + written as u64);
    records.settle_if_whole()?;
    Ok(written)
}

/// A descriptor on `file` is being closed: if the file has a logical end, try the host's
/// shortening, which succeeds once no section of the file is left.
pub(super) fn settle(file: &File) {
    if let Ok(Some(mut records)) = records_for(file) {
        records.apply();
    }
}

/// The mapping seam closed a section of the file `handle` is open on (`vm`'s `MappableFile`,
/// after its views are unmapped). If that was the file's last section and it has a logical end,
/// the host shortens it to that end now and the record goes; otherwise the next `close`, truncate
/// or section to go tries again.
pub(crate) fn section_closed(handle: HANDLE) {
    if LIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    if let Ok(id) = file_id(handle) {
        Records { map: locked(), id }.apply();
    }
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

#[cfg(test)]
mod tests {
    use super::super::{Filesystem, OpenFlags};
    use crate::vm::{self, MappableFile, Protection};
    use std::path::PathBuf;

    /// A scratch root that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let at = std::env::temp_dir()
                .join(format!("omni-logeof-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&at);
            std::fs::create_dir_all(&at).expect("a scratch directory");
            Scratch(at)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A live `PAGE_READWRITE` view of a guest descriptor's file, as the bionic layer makes one
    /// for a writable `MAP_SHARED` mapping: `share_for_mapping`, a section, a placeholder view.
    struct View {
        at: *mut u8,
        len: usize,
        file: Option<MappableFile>,
    }

    impl View {
        fn of(fs: &Filesystem, fd: i32, len: usize) -> View {
            let handle = fs.share_for_mapping(fd).expect("a handle to map");
            let file = vm::share_file_for_mapping(handle, std::path::Path::new("/f"))
                .expect("a section over the file");
            let reservation = vm::reserve_placeholder(len, vm::allocation_granularity())
                .expect("a placeholder");
            // SAFETY: `reservation` is one fresh placeholder of exactly `len` bytes.
            unsafe { vm::map_file(&file, 0, len, reservation.as_ptr(), Protection::ReadWrite) }
                .expect("a shared view");
            View { at: reservation.as_ptr(), len, file: Some(file) }
        }

        fn byte(&self, offset: usize) -> u8 {
            assert!(offset < self.len);
            // SAFETY: `offset` is inside the live view.
            unsafe { self.at.add(offset).read_volatile() }
        }

        fn store(&self, offset: usize, value: u8) {
            assert!(offset < self.len);
            // SAFETY: `offset` is inside the live, writable view.
            unsafe { self.at.add(offset).write_volatile(value) }
        }

        /// Unmap the view and close its section -- the order a region map drops a backing in.
        fn unmap(mut self) {
            // SAFETY: `[at, at + len)` is the one whole view `of` made.
            unsafe { vm::unmap_and_release(self.at, self.len) }.expect("unmap");
            drop(self.file.take());
        }
    }

    fn rw() -> OpenFlags {
        OpenFlags { read: true, write: true, create: true, ..OpenFlags::default() }
    }

    fn rw_trunc() -> OpenFlags {
        OpenFlags { truncate: true, ..rw() }
    }

    fn host_len(scratch: &Scratch, name: &str) -> u64 {
        std::fs::metadata(scratch.0.join(name)).expect("the host file").len()
    }

    /// A file of `len` bytes of `0xAA` under the root, open read-write, and a view over all of it.
    fn mapped(fs: &Filesystem, len: usize) -> (i32, View) {
        let fd = fs.open(b"/f", rw_trunc()).expect("open");
        assert_eq!(fs.write(fd, &vec![0xAA; len]).expect("write"), len);
        let page = vm::page_size();
        (fd, View::of(fs, fd, len.div_ceil(page) * page))
    }

    /// **The engine's sequence, and the host would refuse its second step.** `memProfStorage`:
    /// written, mapped `MAP_SHARED`, re-opened `O_TRUNC` while mapped -- which the host answers
    /// with `ERROR_USER_MAPPED_FILE` -- written again. Every size-visible operation reports the
    /// logical end, the view shows the new bytes, and the host file is shortened to it as soon as
    /// the view and its section are gone.
    #[test]
    fn a_logical_end_is_what_every_operation_sees_and_the_host_gets_it_at_unmap() {
        let scratch = Scratch::new("sequence");
        let fs = Filesystem::new(&scratch.0).expect("a filesystem");
        let (first, view) = mapped(&fs, 10_000);

        // The host itself still refuses: this is the case being handled, not a changed host.
        let refused = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(scratch.0.join("f"))
            .expect_err("a truncating open of a mapped file");
        assert_eq!(refused.raw_os_error(), Some(1224), "{refused}");

        let second = fs.open(b"/f", rw_trunc()).expect("O_TRUNC of a mapped file, as on Linux");
        assert_eq!(fs.fstat(second).expect("fstat").size, 0, "fstat: truncated");
        assert_eq!(fs.fstat(first).expect("fstat").size, 0, "every descriptor sees it");
        assert_eq!(fs.stat(b"/f").expect("stat").size, 0, "stat by path sees it");
        assert_eq!(fs.lstat(b"/f").expect("lstat").size, 0, "lstat by path sees it");
        assert_eq!(host_len(&scratch, "f"), 10_000, "the host file keeps its size meanwhile");
        assert_eq!(view.byte(0), 0, "the bytes past the logical end are zeroed");
        assert_eq!(view.byte(9_999), 0, "all of them");
        let mut buf = [0u8; 64];
        assert_eq!(fs.pread(first, &mut buf, 0).expect("pread"), 0, "pread: end of file");
        assert_eq!(fs.read(first, &mut buf).expect("read"), 0, "read at 10000: end of file");

        assert_eq!(fs.write(second, b"{\"mem\":1}").expect("write"), 9);
        assert_eq!(fs.fstat(second).expect("fstat").size, 9, "a write moves the end");
        assert_eq!(fs.stat(b"/f").expect("stat").size, 9);
        assert_eq!(fs.seek(second, 0, 2).expect("lseek SEEK_END"), 9, "SEEK_END counts from it");
        assert_eq!(fs.pread(second, &mut buf, 0).expect("pread"), 9, "pread ends at it");
        assert_eq!(&buf[..9], b"{\"mem\":1}");
        assert_eq!(fs.seek(second, 2, 0).expect("lseek"), 2);
        assert_eq!(fs.read(second, &mut buf).expect("read"), 7, "read ends at it");
        assert_eq!(view.byte(0), b'{', "the view shows the write");
        let mut into_mapping = [0u8; 32];
        assert_eq!(fs.read_for_mapping(second, &mut into_mapping, 0).expect("read"), 9);

        // `O_APPEND` lands at the logical end, not the host's.
        let append = OpenFlags { write: true, append: true, ..OpenFlags::default() };
        let tail = fs.open(b"/f", append).expect("an appending descriptor");
        assert_eq!(fs.write(tail, b"AB").expect("append"), 2);
        assert_eq!(fs.fstat(tail).expect("fstat").size, 11);
        assert_eq!(fs.pread(second, &mut buf, 0).expect("pread"), 11);
        assert_eq!(&buf[..11], b"{\"mem\":1}AB");

        // Shorter, then longer: the bytes between read as zeros.
        fs.ftruncate(second, 3).expect("ftruncate shorter");
        assert_eq!(fs.fstat(first).expect("fstat").size, 3);
        fs.ftruncate(second, 6).expect("ftruncate longer");
        assert_eq!(fs.pread(first, &mut buf, 0).expect("pread"), 6);
        assert_eq!(&buf[..6], b"{\"m\0\0\0");
        assert_eq!(host_len(&scratch, "f"), 10_000, "still mapped, still the host's size");

        view.unmap();
        assert_eq!(host_len(&scratch, "f"), 6, "the host file is shortened once nothing maps it");
        assert_eq!(fs.fstat(first).expect("fstat").size, 6);
        assert_eq!(std::fs::read(scratch.0.join("f")).expect("read"), b"{\"m\0\0\0");
        for fd in [first, second, tail] {
            fs.close(fd).expect("close");
        }
    }

    /// **A write past a logical end reads zeros in the gap** -- even where a store through the
    /// live view landed past the end in the meantime, which Linux would have met with `SIGBUS`.
    #[test]
    fn a_logical_end_leaves_zeros_in_a_gap_a_write_opens() {
        let scratch = Scratch::new("gap");
        let fs = Filesystem::new(&scratch.0).expect("a filesystem");
        let (fd, view) = mapped(&fs, 5_000);
        fs.ftruncate(fd, 10).expect("ftruncate a mapped file");
        assert_eq!(fs.fstat(fd).expect("fstat").size, 10);
        view.store(20, 0x77);
        view.store(40, 0x77);
        assert_eq!(fs.pwrite(fd, b"Z", 50).expect("pwrite past the end"), 1);
        assert_eq!(fs.fstat(fd).expect("fstat").size, 51);
        let mut buf = [0xFFu8; 64];
        assert_eq!(fs.pread(fd, &mut buf, 0).expect("pread"), 51);
        assert_eq!(&buf[..10], &[0xAA; 10], "the file up to the end it was given");
        assert_eq!(&buf[10..50], &[0u8; 40], "the gap, the view's stores into it included");
        assert_eq!(buf[50], b'Z');

        // `posix_fallocate` past the end: the same zeros.
        fs.fallocate(fd, 0, 60).expect("fallocate");
        assert_eq!(fs.fstat(fd).expect("fstat").size, 60);
        view.store(55, 0x77);
        fs.fallocate(fd, 0, 60).expect("fallocate within the end changes nothing");
        view.unmap();
        let disk = std::fs::read(scratch.0.join("f")).expect("read");
        assert_eq!(disk.len(), 60);
        assert_eq!(&disk[51..55], &[0u8; 4]);
        fs.close(fd).expect("close");
    }

    /// **A write that reaches the host's size ends the record**: there is nothing left to shorten,
    /// and unmapping afterwards leaves the file as long as it was written.
    #[test]
    fn a_logical_end_that_catches_up_with_the_host_is_forgotten() {
        let scratch = Scratch::new("catchup");
        let fs = Filesystem::new(&scratch.0).expect("a filesystem");
        let (fd, view) = mapped(&fs, 5_000);
        let again = fs.open(b"/f", rw_trunc()).expect("O_TRUNC while mapped");
        assert_eq!(fs.fstat(again).expect("fstat").size, 0);
        assert_eq!(fs.write(again, &vec![0x11; 7_000]).expect("write"), 7_000);
        assert_eq!(fs.fstat(again).expect("fstat").size, 7_000);
        assert_eq!(host_len(&scratch, "f"), 7_000);
        view.unmap();
        assert_eq!(host_len(&scratch, "f"), 7_000, "nothing shortened it afterwards");
        fs.close(again).expect("close");
        fs.close(fd).expect("close");
    }

    /// **The last section, not the first, lets the host shorten the file** -- and a `close` after
    /// the last one went is also when it happens, for a section this seam was not told about.
    #[test]
    fn a_logical_end_waits_for_the_last_section_and_a_close_applies_it() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        let scratch = Scratch::new("sections");
        let fs = Filesystem::new(&scratch.0).expect("a filesystem");
        let (fd, first) = mapped(&fs, 5_000);
        let second = View::of(&fs, fd, first.len);
        fs.ftruncate(fd, 100).expect("ftruncate while mapped twice");
        first.unmap();
        assert_eq!(host_len(&scratch, "f"), 5_000, "another section still holds the file");
        assert_eq!(fs.fstat(fd).expect("fstat").size, 100);

        // A section nobody tells the seam about: only a close can find it gone.
        let handle = fs.share_for_mapping(fd).expect("a handle");
        // SAFETY: `handle` is a live file handle; the defaults are documented.
        let section = unsafe {
            CreateFileMappingW(
                handle.as_raw_handle() as super::HANDLE,
                core::ptr::null(),
                PAGE_READWRITE,
                0,
                0,
                core::ptr::null(),
            )
        };
        assert!(!section.is_null());
        second.unmap();
        assert_eq!(host_len(&scratch, "f"), 5_000, "the raw section still holds it");
        // SAFETY: the section was created above and is closed once.
        unsafe { CloseHandle(section) };
        assert_eq!(host_len(&scratch, "f"), 5_000, "nothing has told the seam yet");
        fs.close(fd).expect("close");
        assert_eq!(host_len(&scratch, "f"), 100, "the close shortened it");
        drop(handle);
    }

    use std::os::windows::io::AsRawHandle;
}
