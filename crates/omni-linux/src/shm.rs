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

impl Shm {
    /// Create one, `name` for `/proc` and diagnostics.
    pub fn create(name: &str) -> Result<Arc<Self>, Errno> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let host_path = host_dir().join(format!("omni-shm-{}-{n}", std::process::id()));
        let file = executable_access(std::fs::OpenOptions::new().read(true).write(true).create_new(true)).open(&host_path).map_err(|_| EIO)?;
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
        }))
    }

    /// Open another host process's region by its host file (`crate::remote`).
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn open_path(name: &str, host_path: &std::path::Path, len: u64) -> Result<Arc<Self>, Errno> {
        let file = executable_access(std::fs::OpenOptions::new().read(true).write(true)).open(host_path).map_err(|_| EIO)?;
        Ok(Arc::new(Self { file: Mutex::new(file), name: name.to_string(), len: AtomicU64::new(len), prot_mask: AtomicU64::new(0x7), host_path: host_path.to_path_buf(), owned: false, crossed: std::sync::atomic::AtomicBool::new(true), graphics: std::sync::atomic::AtomicBool::new(false), view: RwLock::new(None) }))
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
