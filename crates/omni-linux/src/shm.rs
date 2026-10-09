//! Shared memory: `memfd_create`, `/dev/ashmem`, and `ASharedMemory`. One anonymous, growable,
//! host-backed file. A `MAP_SHARED` mmap of it maps the *same* host pages, so when its descriptor
//! is passed to another guest process over binder and mapped there, both see each other's writes
//! -- graphics buffers (gralloc), HIDL `hidl_memory`, ART's JIT cache, cutils' ashmem.
//!
//! Backed by a real host file (removed from the host filesystem once opened), because that is what
//! [`omni_mem::Backing::share`] maps into a guest space at its identity address, which is how the
//! omni-android path already shares a guest's writable file mapping.
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::errno::{Errno, EINVAL, EIO};

/// A shared-memory object. Its host file holds the bytes; `len` is what the guest set (`ftruncate`
/// / `ASHMEM_SET_SIZE`), which the mapping's length is taken from.
pub struct Shm {
    file: Mutex<std::fs::File>,
    /// The name a memfd or ashmem region was given: `/proc/self/maps` and `/proc/<pid>/fd` show it.
    pub name: String,
    /// Its size, in bytes. `memfd` grows only through `ftruncate`; ashmem through `ASHMEM_SET_SIZE`.
    len: AtomicU64,
    /// Ashmem's protection mask (`ASHMEM_SET_PROT_MASK`), which caps what a mapping may ask for.
    pub prot_mask: AtomicU64,
    host_path: std::path::PathBuf,
    /// Made here (its host file removed with it) or opened from another host process's.
    owned: bool,
    /// Handed to another host process (`crate::remote`), which opens the host file by its path --
    /// perhaps after this process lets the region go (a gralloc buffer the allocator made, sent on
    /// to SurfaceFlinger by the app), so the file is then left in place.
    crossed: std::sync::atomic::AtomicBool,
    /// A graphics buffer's region ([`Shm::as_graphics_buffer`]): read and written through a host
    /// view of its file rather than a file read or write per call.
    graphics: std::sync::atomic::AtomicBool,
    view: RwLock<Option<View>>,
    /// Pins of the view the GPU imported ([`Shm::pin_view`]).
    pinned: std::sync::atomic::AtomicU32,
}

/// A host view of a region's whole file, for [`Shm::read_at`] and [`Shm::write_at`]: a memory copy
/// where a file read or write went through the host's file system. MEASURED (Windows, Roblox's
/// Landing, 2026-09-28): the swapchain's release wrote each 1280x720 frame (3.6 MiB) into its
/// gralloc region in 22.5 ms of the 24 ms it took, and the composer read it back the same way.
///
/// Only for a region whose size does not change (a graphics buffer): a view -- even its section
/// alone -- stops the host shortening the file (`omni_platform::vm::share_file_for_mapping`), and
/// the view here lives as long as the region, in whichever host process made it.
struct View {
    base: usize,
    size: usize,
    /// The region's length when it was made.
    len: u64,
    /// Kept for the view's life; dropped after it is unmapped.
    _file: omni_platform::vm::MappableFile,
}

// SAFETY: `base` names a mapping this value owns; the bytes are shared memory, copied in and out
// under the region's lock.
unsafe impl Send for View {}
unsafe impl Sync for View {}

impl View {
    fn map(file: &std::fs::File, path: &std::path::Path, len: u64) -> Option<Self> {
        use omni_platform::vm;
        let page = vm::page_size();
        let size = usize::try_from(len).ok()?.div_ceil(page).checked_mul(page)?;
        let shared = vm::share_file_for_mapping(file.try_clone().ok()?, path).ok()?;
        let place = vm::reserve_placeholder(size, vm::allocation_granularity()).ok()?;
        let base = place.base();
        // SAFETY: `place` is one whole unreplaced placeholder of exactly `size` bytes.
        if unsafe { vm::map_file(&shared, 0, size, place.as_ptr(), vm::Protection::ReadWrite) }.is_err() {
            let _ = vm::release(place);
            return None;
        }
        Some(Self { base, size, len, _file: shared })
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `[base, base + size)` is the one view `map` made, and nothing refers into it.
        let _ = unsafe { omni_platform::vm::unmap_and_release(self.base as *mut u8, self.size) };
    }
}

/// **One page of words two host processes share** (`crate::remote`'s direct-access gate): a file
/// in [`host_dir`] mapped in each, so its words are one memory and atomic operations on them are
/// atomic across both processes. Made by one process ([`SharedPage::create`]), opened by the other
/// by its path ([`SharedPage::open`]).
pub(crate) struct SharedPage {
    view: View,
    path: std::path::PathBuf,
    _file: std::fs::File,
}

impl SharedPage {
    /// A new page, zeroed, its file named `<prefix>-<pid>` in [`host_dir`].
    pub(crate) fn create(prefix: &str) -> Option<Self> {
        let path = host_dir().join(format!("{prefix}-{}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(true);
        // Gone with the process that made it (Windows): every host process made one, and each
        // was left in %TEMP% (`omni-remote-gate-<pid>`, ~140 in a night of runs). The other
        // process opens it sharing deletion, which `std` asks for by default.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            options.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        let file = options.open(&path).ok()?;
        file.set_len(omni_platform::vm::page_size() as u64).ok()?;
        let view = View::map(&file, &path, omni_platform::vm::page_size() as u64)?;
        Some(Self { view, path, _file: file })
    }

    /// The page another process made, by its path: only a file in [`host_dir`] named with `prefix`.
    pub(crate) fn open(path: &std::path::Path, prefix: &str) -> Option<Self> {
        let named = path.parent() == Some(host_dir().as_path()) && path.file_name()?.to_str()?.starts_with(prefix);
        if !named {
            return None;
        }
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
        let len = file.metadata().ok()?.len();
        if len < 64 {
            return None;
        }
        let view = View::map(&file, path, len)?;
        Some(Self { view, path: path.to_path_buf(), _file: file })
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Word `i` of the page (`i` < 16).
    pub(crate) fn word(&self, i: usize) -> &std::sync::atomic::AtomicU32 {
        assert!(i < 16, "a shared page's word {i}");
        // SAFETY: the view is mapped read-write for at least 64 bytes (`create`/`open`) and lives
        // as long as `self`; word `i` is 4-aligned inside it.
        unsafe { &*((self.view.base + i * 4) as *const std::sync::atomic::AtomicU32) }
    }
}

/// Bytes of a graphics buffer's host view, borrowed ([`Shm::bytes`]).
pub struct ShmBytes<'a> {
    _view: parking_lot::RwLockReadGuard<'a, Option<View>>,
    ptr: *const u8,
    len: usize,
}

impl std::ops::Deref for ShmBytes<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `[ptr, ptr + len)` lies inside the view the guard keeps mapped (`Shm::bytes`).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// `OMNI_SHM_VIEW=0`: graphics buffers are read and written as files too (the old path, to compare).
fn views_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_SHM_VIEW").as_deref() != Ok("0"))
}

/// A region's host file is opened so it can be mapped executable too, as Linux maps any shared
/// memory `PROT_EXEC` on request (ART's JIT code cache is a memfd with a read-execute view).
fn executable_access(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const GENERIC_EXECUTE: u32 = 0x2000_0000;
        options.access_mode(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE);
    }
    options
}

/// Where regions' host files are made: `/dev/shm` where the host has one (Linux: tmpfs, memory
/// as a memfd is), else the temp directory. The temp directory can be a disk, where a dirty
/// `MAP_SHARED` page is written back while it is mapped -- a graphics buffer, every frame -- or a
/// size-capped tmpfs that the instance's own files fill (Ubuntu's `/tmp`). `OMNI_SHM_DIR` names
/// another.
#[must_use]
pub fn host_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(dir) = std::env::var_os("OMNI_SHM_DIR") {
            return dir.into();
        }
        let shm = std::path::Path::new("/dev/shm");
        let writable = shm.is_dir()
            && std::fs::metadata(shm).is_ok_and(|m| !m.permissions().readonly())
            && tempfile_in(shm);
        if writable { shm.to_path_buf() } else { std::env::temp_dir() }
    })
    .clone()
}

/// Whether a file can be made (and is removed again) in `dir`.
fn tempfile_in(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!("omni-shm-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// `OMNI_SHM_TEMP=1`: a region's host file is made `FILE_ATTRIBUTE_TEMPORARY` on Windows, so the
/// host keeps its dirty pages in memory rather than writing them back to the disk. **Off by
/// default**, because it was not measured to help:
///
/// Why it might: Windows has no `/dev/shm`, so a region is a file in `%TEMP%` -- on the system
/// disk -- and a graphics buffer is a file the swapchain and the composer rewrite whole every
/// frame (5.6 MB a buffer at 1575x890). A temporary file is the host's own answer for "a file that
/// is memory": the cache manager leaves its dirty pages in memory while there is memory for them.
/// MEASURED (Windows 11 26200, 32 GB, `shm_rewrite_load`: 8 such regions rewritten at ~50 Hz for
/// 40 s, ~120 GB of writes, through views and through file writes, temporary or not; the disk's
/// write bytes read around each run): **no write-back above the host's background** (450-1100 MB
/// per 51 s with a live session running, idle phases the same), so any is under ~6 MB/s -- the
/// views' and the cache's dirty pages were rewritten in memory either way. What it may still
/// change, unmeasured: pages written back when the host trims a working set under memory pressure.
/// Nothing else changes: the file is opened, crossed to another host process by
/// its path ([`Shm::host_path_crossing`], `crate::remote`) and mapped exactly as before, so every
/// way a region is shared keeps working. `FILE_FLAG_DELETE_ON_CLOSE` would also be the host's, but
/// is not safe here: a crossed region is opened by path after its maker may have let it go.
/// A pagefile-backed section would not fit either: a region grows (`ftruncate`), is read and
/// written as a file, and crosses by path.
#[cfg(windows)]
fn temporary_files() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_SHM_TEMP").as_deref() == Ok("1"))
}

impl Shm {
    /// Create one, `name` for `/proc` and diagnostics.
    pub fn create(name: &str) -> Result<Arc<Self>, Errno> {
        #[cfg(windows)]
        let temporary = temporary_files();
        #[cfg(not(windows))]
        let temporary = false;
        Self::create_in(name, &host_dir(), temporary)
    }

    /// [`create`](Self::create) in `dir`, its file `FILE_ATTRIBUTE_TEMPORARY` (Windows) if
    /// `temporary`.
    fn create_in(name: &str, dir: &std::path::Path, temporary: bool) -> Result<Arc<Self>, Errno> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        if temporary {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
            options.attributes(FILE_ATTRIBUTE_TEMPORARY);
        }
        #[cfg(not(windows))]
        let _ = temporary;
        let options = executable_access(&mut options);
        // A file already there under this process's pid and number is a dead process's: one that
        // had the same pid (the host reuses them) and was killed before it removed its regions.
        // It is removed and the name made again (or, if it cannot be, a name no other process had)
        // -- answering EIO here once failed a boot's first graphics buffer, and SurfaceFlinger
        // aborted on it at every restart (2026-10-09 14:51: 20,372 such files in %TEMP%,
        // `omni-shm-36916-*` left at 04:31 by a killed run).
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut host_path = dir.join(format!("omni-shm-{}-{n}", std::process::id()));
        let file = match options.open(&host_path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&host_path);
                match options.open(&host_path) {
                    Ok(file) => file,
                    Err(_) => {
                        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
                        host_path = dir.join(format!("omni-shm-{}-{n}-{nanos}", std::process::id()));
                        options.open(&host_path).map_err(|_| EIO)?
                    }
                }
            }
            Err(_) => return Err(EIO),
        };
        Ok(Arc::new(Self {
            file: Mutex::new(file),
            name: name.to_string(),
            len: AtomicU64::new(0),
            prot_mask: AtomicU64::new(0x7), // PROT_READ|WRITE|EXEC until ASHMEM_SET_PROT_MASK narrows it
            host_path,
            owned: true,
            crossed: std::sync::atomic::AtomicBool::new(false),
            graphics: std::sync::atomic::AtomicBool::new(false),
            view: RwLock::new(None),
            pinned: std::sync::atomic::AtomicU32::new(0),
        }))
    }

    /// Open another host process's region by its host file (`crate::remote`).
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn open_path(name: &str, host_path: &std::path::Path, len: u64) -> Result<Arc<Self>, Errno> {
        let file = executable_access(std::fs::OpenOptions::new().read(true).write(true)).open(host_path).map_err(|_| EIO)?;
        Ok(Arc::new(Self { file: Mutex::new(file), name: name.to_string(), len: AtomicU64::new(len), prot_mask: AtomicU64::new(0x7), host_path: host_path.to_path_buf(), owned: false, crossed: std::sync::atomic::AtomicBool::new(true), graphics: std::sync::atomic::AtomicBool::new(false), view: RwLock::new(None), pinned: std::sync::atomic::AtomicU32::new(0) }))
    }

    /// Its host file.
    #[must_use]
    pub fn host_path(&self) -> &std::path::Path {
        &self.host_path
    }

    /// Its host file, for another host process to open: kept from then on.
    #[must_use]
    pub fn host_path_crossing(&self) -> &std::path::Path {
        self.crossed.store(true, Ordering::SeqCst);
        &self.host_path
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Grow or shrink to `len` (`ftruncate`, `ASHMEM_SET_SIZE`).
    pub fn set_len(&self, len: u64) -> Result<(), Errno> {
        if len > 1 << 34 {
            return Err(EINVAL);
        }
        if self.pinned.load(Ordering::SeqCst) > 0 && len != self.len() {
            return Err(crate::errno::EBUSY);
        }
        // This process's own view goes first (a view stops the host shortening the file); the next
        // read or write maps the new length.
        let mut view = self.view.write();
        *view = None;
        self.file.lock().set_len(len).map_err(|_| EINVAL)?;
        self.len.store(len, Ordering::SeqCst);
        Ok(())
    }

    /// This region is a graphics buffer (a gralloc allocation, made here or received): its size
    /// never changes, and its pixels are read and written whole every frame, so from now on
    /// [`read_at`](Self::read_at) and [`write_at`](Self::write_at) go through a host view of it.
    pub fn as_graphics_buffer(&self) {
        self.graphics.store(true, Ordering::Relaxed);
    }

    /// `f` of the host view, made if needed, when this region is a graphics buffer.
    fn with_view<T>(&self, f: impl FnOnce(&View) -> T) -> Option<T> {
        if !self.graphics.load(Ordering::Relaxed) || !views_on() {
            return None;
        }
        {
            let view = self.view.read();
            if let Some(v) = view.as_ref().filter(|v| v.len == self.len()) {
                return Some(f(v));
            }
        }
        let mut view = self.view.write();
        if view.as_ref().is_none_or(|v| v.len != self.len()) {
            *view = None;
            *view = View::map(&self.file.lock(), &self.host_path, self.len());
        }
        view.as_ref().map(f)
    }

    /// **A graphics buffer's bytes where they are**: `[offset, offset + len)` of its host view (cut
    /// at the region's end), borrowed rather than copied out as [`read_at`](Self::read_at) does --
    /// what the composer reads a layer's pixels through under `compose_zero` (5.6 MB a layer a
    /// frame at 1575x890 no longer copied). `None` for a region that is not a graphics buffer, or
    /// with views off (`OMNI_SHM_VIEW=0`): read it then.
    ///
    /// The view cannot be unmapped while the guard lives (it holds the view's lock). The bytes are
    /// shared memory another host process writes: a reader takes them when nothing is writing --
    /// a released buffer after its copy has landed (`crate::gpu::native::wait_written`), as for
    /// `read_at`, which copies the same bytes at the same moment.
    #[must_use]
    pub fn bytes(&self, offset: u64, len: usize) -> Option<ShmBytes<'_>> {
        self.with_view(|_| ())?;
        // Recursive: a reader may hold another guard of this region (two layers, one buffer)
        // while a writer waits.
        let view = self.view.read_recursive();
        let v = view.as_ref().filter(|v| v.len == self.len())?;
        let n = usize::try_from(v.len.checked_sub(offset)?).ok()?.min(len);
        let ptr = (v.base + offset as usize) as *const u8;
        Some(ShmBytes { _view: view, ptr, len: n })
    }

    /// **Pin the host view** and answer the address of `[offset, offset + len)` in it, `len` rounded
    /// up to a 4 KiB page (`None`: not a graphics buffer, views off, or past the view): for the GPU
    /// to import as memory of its own (`gralloc_direct`, `crate::gpu::native`). While pinned the
    /// region cannot be resized ([`set_len`](Self::set_len) answers `EBUSY`), so the view -- which a
    /// resize would unmap under the GPU's import -- stays where it is until [`unpin_view`](Self::unpin_view).
    #[must_use]
    pub fn pin_view(&self, offset: u64, len: usize) -> Option<*mut u8> {
        let at = self.with_view(|v| {
            let end = usize::try_from(offset).ok()?.checked_add(len.div_ceil(4096) * 4096)?;
            (end <= v.size).then(|| {
                self.pinned.fetch_add(1, Ordering::SeqCst);
                (v.base + offset as usize) as *mut u8
            })
        });
        at.flatten()
    }

    /// **Compare and swap the little-endian `u64` at `offset`** (8-aligned), atomically across every
    /// host process mapping the region (one memory, one atomic instruction): `true` when it held
    /// `current` and now holds `new`. A region without a host view (not a graphics buffer, views
    /// off) is read and written instead, which is not atomic.
    pub fn cas_u64(&self, offset: u64, current: u64, new: u64) -> bool {
        if offset % 8 != 0 {
            return false;
        }
        let swapped = self.with_view(|v| {
            (offset as usize + 8 <= v.size).then(|| {
                // SAFETY: an 8-aligned word inside the mapped view (the view is page-aligned);
                // shared memory, so accessed atomically only.
                let word = unsafe { &*((v.base + offset as usize) as *const std::sync::atomic::AtomicU64) };
                word.compare_exchange(current.to_le(), new.to_le(), Ordering::SeqCst, Ordering::SeqCst).is_ok()
            })
        });
        if let Some(Some(done)) = swapped {
            return done;
        }
        let mut b = [0u8; 8];
        if self.read_at(&mut b, offset).is_err() || u64::from_le_bytes(b) != current {
            return false;
        }
        self.write_at(&new.to_le_bytes(), offset).is_ok()
    }

    /// Undo one [`pin_view`](Self::pin_view), once the GPU's import is freed.
    pub fn unpin_view(&self) {
        self.pinned.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
        let viewed = self.with_view(|v| {
            let n = usize::try_from(v.len.saturating_sub(offset)).unwrap_or(usize::MAX).min(buf.len());
            // SAFETY: `[offset, offset + n)` lies inside the view (`n` is cut at the region's
            // length), and `buf` holds `n` or more bytes outside it.
            unsafe { std::ptr::copy_nonoverlapping((v.base + offset as usize) as *const u8, buf.as_mut_ptr(), n) };
            n
        });
        if let Some(n) = viewed {
            return Ok(n);
        }
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(offset)).map_err(|_| EIO)?;
        file.read(buf).map_err(|_| EIO)
    }

    pub fn write_at(&self, bytes: &[u8], offset: u64) -> Result<usize, Errno> {
        // A write past the end grows the region, as a memfd's does.
        let end = offset + bytes.len() as u64;
        if end > self.len() {
            self.set_len(end)?;
        }
        let viewed = self.with_view(|v| {
            (end <= v.len).then(|| {
                // SAFETY: `[offset, end)` lies inside the view; `bytes` is outside it.
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (v.base + offset as usize) as *mut u8, bytes.len()) };
            })
        });
        if viewed.flatten().is_some() {
            return Ok(bytes.len());
        }
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(offset)).map_err(|_| EIO)?;
        file.write_all(bytes).map_err(|_| EIO)?;
        Ok(bytes.len())
    }

    /// A second handle to the same host file, for [`omni_mem::Backing::share`] to map. Both handles
    /// name one file, so a `MAP_SHARED` mapping of either is the same memory.
    pub fn dup_file(&self) -> Result<std::fs::File, Errno> {
        self.file.lock().try_clone().map_err(|_| EIO)
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        if self.owned && !self.crossed.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&self.host_path);
        }
    }
}

impl Shm {
    /// Sequential read at the file's own position (a plain `read`), advancing it.
    pub fn read_seq(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.file.lock().read(buf).map_err(|_| EIO)
    }

    /// Sequential write at the file's own position, advancing it (and `len`).
    pub fn write_seq(&self, bytes: &[u8]) -> Result<usize, Errno> {
        let mut file = self.file.lock();
        let n = file.write(bytes).map_err(|_| EIO)?;
        let end = file.stream_position().map_err(|_| EIO)?;
        drop(file);
        if end > self.len() {
            self.len.store(end, Ordering::SeqCst);
        }
        Ok(n)
    }

    /// Set the file's position (`lseek`), answering the new position.
    pub fn seek(&self, whence: u32, offset: i64) -> Result<u64, Errno> {
        let pos = match whence {
            0 => SeekFrom::Start(offset as u64),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(EINVAL),
        };
        self.file.lock().seek(pos).map_err(|_| EINVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dead process that had this pid left its regions' files (a killed run, the pid reused):
    /// they do not refuse this process's regions.
    #[test]
    fn a_dead_process_s_files_under_the_same_pid_do_not_refuse_a_region() {
        let dir = std::env::temp_dir().join(format!("omni-shm-stale-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The next numbers this process will take, each already a file.
        let first = Shm::create_in("probe", &dir, false).expect("made");
        let next: u64 = first.host_path().file_name().unwrap().to_string_lossy().rsplit('-').next().unwrap().parse().unwrap();
        drop(first);
        for n in next + 1..next + 400 {
            std::fs::write(dir.join(format!("omni-shm-{}-{n}", std::process::id())), b"stale").unwrap();
        }
        for _ in 0..8 {
            let shm = Shm::create_in("fresh", &dir, false).expect("made despite a stale file");
            shm.set_len(16).unwrap();
            let mut got = [1u8; 5];
            shm.read_at(&mut got, 0).unwrap();
            assert_eq!(got, [0; 5], "a new region, not the stale file's bytes");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A temporary region is one file shared as before: another opener by path (as another host
    /// process opens a crossed region) sees the view's writes, and the file is marked temporary
    /// only when asked.
    #[test]
    fn a_temporary_region_is_shared_by_path_as_before() {
        for temporary in [true, false] {
            let shm = Shm::create_in("test", &std::env::temp_dir(), temporary).expect("made");
            shm.set_len(8192).unwrap();
            shm.as_graphics_buffer();
            shm.write_at(b"pixels", 4096).unwrap();
            let other = Shm::open_path("test", shm.host_path_crossing(), 8192).expect("opened by path");
            let mut got = [0u8; 6];
            other.read_at(&mut got, 4096).unwrap();
            assert_eq!(&got, b"pixels");
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                let attributes = std::fs::metadata(shm.host_path()).unwrap().file_attributes();
                assert_eq!(attributes & 0x100 != 0, temporary, "attributes {attributes:#x}");
            }
            let path = shm.host_path().to_path_buf();
            drop((shm, other));
            let _ = std::fs::remove_file(path);
        }
    }

    /// **The measurement behind `OMNI_SHM_TEMP`**: a 5.6 MB graphics region (1575x890x4) rewritten
    /// at 60 Hz for `OMNI_SHM_BENCH_SECS` (20) and then held 10 s more, its file temporary or not
    /// (`OMNI_SHM_BENCH=plain`). The disk's writes are read from outside meanwhile (the host's disk
    /// counters); this only makes the load. `cargo test --release -p omni-linux --lib --
    /// --ignored --nocapture shm_rewrite_load`.
    #[test]
    #[ignore = "a load to measure from outside; run by hand"]
    fn shm_rewrite_load() {
        const FRAME: usize = 1575 * 890 * 4;
        let temporary = std::env::var("OMNI_SHM_BENCH").as_deref() != Ok("plain");
        let secs: u64 = std::env::var("OMNI_SHM_BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        // `OMNI_SHM_BENCH_REGIONS=<n>` (1): several buffers, as a swapchain and the composer have.
        let regions: usize = std::env::var("OMNI_SHM_BENCH_REGIONS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
        let shms: Vec<_> = (0..regions)
            .map(|_| {
                let shm = Shm::create_in("bench", &std::env::temp_dir(), temporary).expect("made");
                shm.set_len(FRAME as u64).unwrap();
                shm.as_graphics_buffer();
                // Kept, as a crossed graphics buffer's file is.
                let path = shm.host_path_crossing().to_path_buf();
                (shm, path)
            })
            .collect();
        let mut frame = vec![0u8; FRAME];
        let start = std::time::Instant::now();
        let mut n = 0u32;
        while start.elapsed() < std::time::Duration::from_secs(secs) {
            frame.iter_mut().step_by(4096).for_each(|b| *b = b.wrapping_add(1));
            for (shm, _) in &shms {
                shm.write_at(&frame, 0).unwrap();
            }
            n += 1;
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        eprintln!("[shm] {n} frames of {regions} x {FRAME} bytes in {:.1} s, temporary {temporary}, views {}", start.elapsed().as_secs_f64(), views_on());
        std::thread::sleep(std::time::Duration::from_secs(10));
        for (shm, path) in shms {
            drop(shm);
            let _ = std::fs::remove_file(path);
        }
    }

    /// A graphics buffer's bytes lent are the bytes `read_at` copies, cut at the region's end; a
    /// region that is not a graphics buffer lends none.
    #[test]
    fn a_graphics_buffers_bytes_are_lent_as_read() {
        let shm = Shm::create("bytes").expect("region");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        shm.write_at(&data, 0).expect("write");
        assert!(shm.bytes(0, 16).is_none(), "not a graphics buffer");
        shm.as_graphics_buffer();
        let lent = shm.bytes(100, 500).expect("a view");
        let mut read = vec![0u8; 500];
        shm.read_at(&mut read, 100).expect("read");
        assert_eq!(&*lent, &read[..]);
        // Two guards at once (two layers of one buffer), and one past the end, cut.
        let tail = shm.bytes(9_990, 64).expect("a view");
        assert_eq!(&*tail, &data[9_990..]);
        drop((lent, tail));
        assert!(shm.bytes(20_000, 4).is_none(), "past the end");
    }
}
