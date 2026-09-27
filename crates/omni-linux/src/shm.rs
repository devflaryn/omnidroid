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

use parking_lot::Mutex;

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
}

impl Shm {
    /// Create one, `name` for `/proc` and diagnostics.
    pub fn create(name: &str) -> Result<Arc<Self>, Errno> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let host_path = std::env::temp_dir().join(format!("omni-shm-{}-{n}", std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&host_path).map_err(|_| EIO)?;
        Ok(Arc::new(Self {
            file: Mutex::new(file),
            name: name.to_string(),
            len: AtomicU64::new(0),
            prot_mask: AtomicU64::new(0x7), // PROT_READ|WRITE|EXEC until ASHMEM_SET_PROT_MASK narrows it
            host_path,
            owned: true,
        }))
    }

    /// Open another host process's region by its host file (`crate::remote`).
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn open_path(name: &str, host_path: &std::path::Path, len: u64) -> Result<Arc<Self>, Errno> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(host_path).map_err(|_| EIO)?;
        Ok(Arc::new(Self { file: Mutex::new(file), name: name.to_string(), len: AtomicU64::new(len), prot_mask: AtomicU64::new(0x7), host_path: host_path.to_path_buf(), owned: false }))
    }

    /// Its host file.
    #[must_use]
    pub fn host_path(&self) -> &std::path::Path {
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
        self.file.lock().set_len(len).map_err(|_| EINVAL)?;
        self.len.store(len, Ordering::SeqCst);
        Ok(())
    }

    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
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
        if self.owned {
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
