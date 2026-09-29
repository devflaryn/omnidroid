//! Linux `mmap` semantics on `omni-mem`'s guest space.
use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};
use parking_lot::Mutex;

use crate::errno::*;
use crate::fd::FileKind;
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const PROT_EXEC: u32 = 4;
const MAP_SHARED: u32 = 1;
const MAP_FIXED: u32 = 0x10;
const MAP_ANONYMOUS: u32 = 0x20;
const MAP_FIXED_NOREPLACE: u32 = 0x10_0000;
const MADV_DONTNEED: u64 = 4;

#[derive(Debug, Clone, Copy)]
pub struct MapRequest {
    pub addr: u64,
    pub len: u64,
    pub prot: u32,
    pub flags: u32,
    pub fd: i32,
    pub offset: u64,
}

/// A file mapping, as `/proc/self/maps` and fault reports name it.
#[derive(Debug, Clone)]
struct FileMapping {
    len: u64,
    guest: Vec<u8>,
    offset: u64,
}

pub struct Mm {
    space: Arc<GuestSpace>,
    /// The page: the guest space's, which is the host's (4 KiB on Windows and x86-64 Linux, 16 KiB
    /// on Apple silicon). The guest is told it (`AT_PAGESZ`), as a 16 KiB Android 15 device tells
    /// its processes, so every mapping, protection and unmapping it asks for is whole host pages.
    page: u64,
    /// The layout lock, held exclusively for every change: `MAP_FIXED`'s unmap-then-map must not
    /// interleave with another thread's mapping, nor any change with a syscall's copy.
    lock: crate::guest::Layout,
    files: Mutex<std::collections::BTreeMap<u64, FileMapping>>,
}


fn protection(prot: u32) -> Result<Protection, Errno> {
    Ok(match (prot & PROT_READ != 0, prot & PROT_WRITE != 0, prot & PROT_EXEC != 0) {
        // Write and execute together -- ART's JIT code cache when it has no dual view. A device
        // grants it; the guest issues `IC IVAU` after writing code and the CPU backend discards the
        // stale translation there, so the page is genuinely writable-and-executable rather than
        // silently stripped to writable.
        (_, true, true) => Protection::ReadWriteExecute,
        (false, false, false) => Protection::None,
        (_, true, false) => Protection::ReadWrite,
        (_, false, true) => Protection::ReadExecute,
        (true, false, false) => Protection::Read,
    })
}

impl Mm {
    #[must_use]
    pub fn new(space: Arc<GuestSpace>, lock: crate::guest::Layout) -> Self {
        let page = space.page_size() as u64;
        Self { space, page, lock, files: Mutex::default() }
    }

    /// The page size the guest is told and every `mmap`, `mprotect` and `munmap` is exact at.
    #[must_use]
    pub const fn page_size(&self) -> u64 {
        self.page
    }

    const fn round_up(&self, v: u64) -> u64 {
        (v + self.page - 1) & !(self.page - 1)
    }

    /// `len` rounded up to pages, if `[addr, addr + len)` fits in the 56-bit user address range.
    /// Every length from the guest goes through here first: a raw `addr + len` overflowed on
    /// absurd input (A1 review, Important 3).
    pub(crate) fn span(&self, addr: u64, len: u64) -> Option<u64> {
        let len = len.checked_add(self.page - 1)? & !(self.page - 1);
        (addr.checked_add(len)? <= 1 << 56).then_some(len)
    }

    /// Name a range that is not a file (`[stack]`), for `/proc/<pid>/maps`.
    pub fn label(&self, start: u64, len: u64, name: &[u8]) {
        self.forget(start, len);
        self.files.lock().insert(start, FileMapping { len, guest: name.to_vec(), offset: 0 });
    }

    /// The name and file offset at `addr`, if a mapping there is named.
    #[must_use]
    pub fn name_at(&self, addr: u64) -> Option<(Vec<u8>, u64)> {
        let files = self.files.lock();
        let (start, m) = files.range(..=addr).next_back()?;
        (addr < start + m.len).then(|| (m.guest.clone(), m.offset + (addr - start)))
    }

    /// Every named mapping: (start, len, guest path, file offset).
    #[must_use]
    pub fn file_mappings(&self) -> Vec<(u64, u64, Vec<u8>, u64)> {
        self.files.lock().iter().map(|(s, m)| (*s, m.len, m.guest.clone(), m.offset)).collect()
    }

    /// `path+0xoffset` for an address inside a file mapping.
    #[must_use]
    pub fn describe(&self, addr: u64) -> Option<String> {
        let files = self.files.lock();
        let (start, m) = files.range(..=addr).next_back()?;
        (addr < start + m.len)
            .then(|| format!("{}+{:#x}", String::from_utf8_lossy(&m.guest), m.offset + (addr - start)))
    }

    /// Forget file mappings in `[addr, addr + len)`, splitting any that straddle an edge.
    fn forget(&self, addr: u64, len: u64) {
        let end = addr + len;
        let mut files = self.files.lock();
        let hit: Vec<u64> = files.range(..end).filter(|(s, m)| *s + m.len > addr).map(|(s, _)| *s).collect();
        for start in hit {
            let m = files.remove(&start).expect("present");
            if start < addr {
                files.insert(start, FileMapping { len: addr - start, ..m.clone() });
            }
            if start + m.len > end {
                files.insert(end, FileMapping { len: start + m.len - end, offset: m.offset + (end - start), guest: m.guest });
            }
        }
    }

    fn unmap_locked(&self, addr: u64, len: u64) -> Result<(), Errno> {
        let (start, end) = (addr as usize, (addr + len) as usize);
        for r in self.space.mapped_regions() {
            let (rs, re) = (r.start, r.start + r.len);
            let (s, e) = (rs.max(start), re.min(end));
            if s < e {
                self.space.unmap(s, e - s).map_err(|_| EINVAL)?;
            }
        }
        self.forget(addr, len);
        Ok(())
    }

    pub fn unmap(&self, addr: u64, len: u64) -> Result<(), Errno> {
        let addr = crate::guest::untag(addr);
        if addr % self.page != 0 || len == 0 {
            return Err(EINVAL);
        }
        let _g = self.lock.write();
        let len = self.span(addr, len).ok_or(EINVAL)?;
        self.unmap_locked(addr, len)
    }

    pub fn protect(&self, addr: u64, len: u64, prot: u32) -> Result<(), Errno> {
        let addr = crate::guest::untag(addr);
        if addr % self.page != 0 {
            return Err(EINVAL);
        }
        let prot = protection(prot)?;
        let len = self.span(addr, len).ok_or(EINVAL)?;
        if len == 0 {
            return Ok(());
        }
        let _g = self.lock.write();
        self.space.protect(addr as usize, len as usize, prot).map_err(|_| ENOMEM)
    }

    /// Write into a mapping the guest may only read -- what the kernel does to memory it owns (the
    /// property areas): made writable for the write, and given its protection back.
    pub fn kernel_write(&self, mem: &crate::guest::GuestMem, addr: u64, bytes: &[u8]) -> Result<(), Errno> {
        let _g = self.lock.write();
        let len = self.span(addr, bytes.len() as u64).ok_or(EINVAL)?;
        let prot = self.space.region_at(addr as usize).map(|r| r.protection).ok_or(EFAULT)?;
        self.space.protect(addr as usize, len as usize, Protection::ReadWrite).map_err(|_| EFAULT)?;
        let written = mem.write_holding_layout(addr, bytes);
        self.space.protect(addr as usize, len as usize, prot).map_err(|_| EFAULT)?;
        written
    }

    /// `MADV_DONTNEED`: the range reads as zeros again.
    pub fn discard(&self, addr: u64, len: u64) -> Result<(), Errno> {
        let _g = self.lock.write();
        self.space.discard(addr as usize, len as usize).map(|_| ()).map_err(|_| EINVAL)
    }

    pub fn map(&self, p: &Process, t: &Task, req: MapRequest) -> Result<u64, Errno> {
        if req.len == 0 || req.offset % self.page != 0 {
            return Err(EINVAL);
        }
        let prot = protection(req.prot)?;
        let len = self.span(crate::guest::untag(req.addr), req.len).ok_or(ENOMEM)?;
        let fixed = req.flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
        if fixed && req.addr % self.page != 0 {
            return Err(EINVAL);
        }
        let page = self.page as usize;
        let placement = if fixed {
            Placement::Fixed(req.addr as usize)
        } else if req.addr != 0 {
            let hint = crate::guest::untag(req.addr);
            if hint < 1 << 32 && !self.space.contains(hint as usize, len as usize) {
                // A low hint is a request for low memory (ART's heap, anything that needs 32-bit
                // pointers), and below 4 GiB outside the guest space is the host's. Linux would
                // map elsewhere, and the caller would unmap and try the next page: answering
                // "not there" at once is the same answer without the mapping.
                return Err(ENOMEM);
            }
            Placement::Hint { address: (hint & !(self.page - 1)) as usize, align: page }
        } else {
            // No hint: above 4 GiB when the space reaches there, as Linux's top-down `mmap_base`
            // keeps ordinary mappings out of the low 4 GiB -- which ART needs for its heap and
            // boot image, and asks for by address.
            let low_end = 1usize << 32;
            if self.space.base() < low_end && self.space.end() > low_end + len as usize {
                Placement::Hint { address: low_end, align: page }
            } else {
                Placement::Anywhere { align: page }
            }
        };
        let _g = self.lock.write();
        if req.flags & MAP_FIXED != 0 {
            self.unmap_locked(req.addr, len)?;
        }
        let refused_fixed = |e| if fixed { EEXIST } else { e };
        if req.flags & MAP_ANONYMOUS != 0 {
            let at = self
                .space
                .map_anonymous(placement, len as usize, prot, CommitPolicy::Lazy)
                .map(|a| a as u64)
                .map_err(|e| {
                    // A large mapping refused is rare and decisive (a process that cannot map its
                    // stack does not start): say why.
                    if len >= 1 << 20 && !fixed {
                        eprintln!("[mm] {len:#x} anonymous bytes refused: {e}");
                        // The first time: what holds the space, by owner and call site.
                        static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                        if !SHOWN.swap(true, std::sync::atomic::Ordering::SeqCst) {
                            let mut by: std::collections::HashMap<(&'static str, u64), (usize, usize)> = std::collections::HashMap::new();
                            for (r, label) in p.mem.space().labelled_regions() {
                                let e = by.entry((label.owner, label.site)).or_default();
                                e.0 += r.len;
                                e.1 += 1;
                            }
                            let mut top: Vec<_> = by.into_iter().collect();
                            top.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
                            for ((owner, site), (bytes, n)) in top.into_iter().take(12) {
                                eprintln!("[mm]   {:>8} MiB in {n} regions: {owner:?} site {site:#x}", bytes >> 20);
                            }
                        }
                    }
                    refused_fixed(ENOMEM)
                });
            if p.trace {
                if let (Placement::Hint { address, .. }, Ok(got)) = (placement, &at) {
                    if *got != address as u64 {
                        let r = self.space.region_at(address);
                        eprintln!("[mm] hint {address:#x}+{len:#x} not free: {r:?}");
                    }
                }
            }
            return at;
        }
        let file = p.fds.get(req.fd)?;
        // A shared-memory region (memfd, ashmem): MAP_SHARED maps the same host file, so another
        // process that received this descriptor over binder and maps it sees the same memory; a
        // private mapping is a copy-on-write view of it.
        let shm = match &*file.kind.lock() {
            FileKind::Shared(m) => Some(std::sync::Arc::clone(m)),
            _ => None,
        };
        if let Some(m) = shm {
            let name = format!("/memfd:{}", m.name).into_bytes();
            let host = m.dup_file().map_err(|_| ENODEV)?;
            let backing = omni_mem::Backing::share(host, &String::from_utf8_lossy(&name)).map_err(|_| ENODEV)?;
            let backed = self.round_up((m.len()).saturating_sub(req.offset)).min(len);
            let at = if backed > 0 {
                self.space
                    .map_file(&backing, req.offset, placement, backed as usize, prot)
                    .map_err(|e| {
                        if fixed {
                            eprintln!("[mm] {} at {:#x}+{backed:#x} refused: {e}", String::from_utf8_lossy(&name), req.addr);
                        }
                        refused_fixed(ENOMEM)
                    })? as u64
            } else {
                self.space
                    .map_anonymous(placement, len as usize, prot, CommitPolicy::Lazy)
                    .map_err(|_| refused_fixed(ENOMEM))? as u64
            };
            if len > backed && backed > 0 {
                self.space
                    .map_anonymous(Placement::Fixed((at + backed) as usize), (len - backed) as usize, prot, CommitPolicy::Lazy)
                    .map_err(|_| ENOMEM)?;
            }
            self.forget(at, len);
            self.files.lock().insert(at, FileMapping { len, guest: name, offset: req.offset });
            return Ok(at);
        }
        if let FileKind::Synth { data, guest, .. } = &*file.kind.lock() {
            // An in-memory file (`/dev/__properties__`): a private copy, read-only in effect.
            if req.flags & MAP_SHARED != 0 && req.prot & PROT_WRITE != 0 {
                return Err(EACCES);
            }
            let at = self
                .space
                .map_anonymous(placement, len as usize, Protection::ReadWrite, CommitPolicy::Lazy)
                .map_err(|_| refused_fixed(ENOMEM))? as u64;
            let copy = |bytes: &[u8]| {
                let from = (req.offset as usize).min(bytes.len());
                let n = (len as usize).min(bytes.len() - from);
                p.mem.write_holding_layout(at, &bytes[from..from + n])
            };
            // The property areas: their bytes as they are now (not when the file was opened), and
            // every later change written into them by the property service.
            let area = guest.strip_prefix(b"/dev/__properties__/".as_slice()).filter(|name| *name != b"property_info");
            match (area, p.me.get()) {
                (Some(name), Some(me)) => {
                    let service = crate::props::PropertyService::global(p.vfs.sysroot());
                    service.attach(me.clone(), at, name == b"properties_serial", copy)?;
                }
                _ => copy(data)?,
            }
            if prot != Protection::ReadWrite {
                self.space.protect(at as usize, len as usize, prot).map_err(|_| ENOMEM)?;
            }
            self.files.lock().insert(at, FileMapping { len, guest: guest.clone(), offset: req.offset });
            return Ok(at);
        }
        // An instance file mapped MAP_SHARED to be written (SQLite's WAL index): the host file
        // itself, so every mapping of it, and its reads and writes, are the same bytes. (A read-only
        // shared mapping -- an idmap, an APK -- is a copy: its descriptor may not be writable, and
        // the host shares only a writable one.)
        let shared = match &*file.kind.lock() {
            FileKind::Host { file: host, guest, sysroot: false } if req.flags & MAP_SHARED != 0 && req.prot & PROT_WRITE != 0 => {
                Some((host.try_clone().map_err(|_| EIO)?, guest.clone(), host.metadata().map_err(|_| EIO)?.len()))
            }
            _ => None,
        };
        if let Some((host, guest, file_len)) = shared {
            let in_file = self.round_up(file_len.saturating_sub(req.offset)).min(len);
            let at = if in_file > 0 {
                let backing = omni_mem::Backing::share(host, &String::from_utf8_lossy(&guest)).map_err(|_| {
                    p.refusals.record("mmap: MAP_SHARED of a file the host cannot share".into(), t.pc, t.lr);
                    ENODEV
                })?;
                self.space.map_file(&backing, req.offset, placement, in_file as usize, prot).map_err(|_| refused_fixed(ENOMEM))? as u64
            } else {
                self.space.map_anonymous(placement, len as usize, prot, CommitPolicy::Lazy).map_err(|_| refused_fixed(ENOMEM))? as u64
            };
            if len > in_file && in_file > 0 {
                self.space
                    .map_anonymous(Placement::Fixed((at + in_file) as usize), (len - in_file) as usize, prot, CommitPolicy::Lazy)
                    .map_err(|_| ENOMEM)?;
            }
            self.forget(at, len);
            self.files.lock().insert(at, FileMapping { len, guest, offset: req.offset });
            return Ok(at);
        }
        let (guest, sysroot, file_len, installed) = match &*file.kind.lock() {
            FileKind::Host { file, guest, sysroot } => {
                let meta = file.metadata().map_err(|_| EIO)?;
                // An installed package's file: the host file behind it, for a view like the image's.
                let installed = (!sysroot && is_installed(guest)).then(|| omni_platform::fs::path_of(file).ok().map(|path| (path, meta.len(), meta.modified().ok()))).flatten();
                (guest.clone(), *sysroot, meta.len(), installed)
            }
            _ => return Err(ENODEV),
        };
        // The part of the request the file covers, in whole pages; the rest is anonymous zeros.
        let in_file = self.round_up(file_len.saturating_sub(req.offset)).min(len);
        let backing = if in_file == 0 {
            None
        } else if sysroot {
            Some(p.vfs.sysroot().backing(&guest)?)
        } else {
            installed.and_then(|(path, len, modified)| installed_backing(&path, len, modified, &guest))
        };
        // **The view covers the file's whole pages only.** A read-only view may not run past the
        // file's last byte, and a file's size is rarely a whole number of pages: the rounded-up
        // request was refused, and every such mapping -- ICU's data, fonts, APKs, vdex files, in
        // every process -- fell back to a private copy of all of it (26 MiB of ICU data twice in
        // each app process, run 2026-09-29). Now the whole pages are the view and only the last,
        // partial page is a private copy (below).
        let whole = (file_len.saturating_sub(req.offset) / self.page_size() * self.page_size()).min(len);
        let at = if let (Some(backing), true) = (backing, whole > 0) {
            if whole == in_file {
                match self.space.map_file(&backing, req.offset, placement, in_file as usize, prot) {
                    Ok(a) => Some(a as u64),
                    Err(e) => {
                        tracing::debug!(%e, "a file view could not be made; mapping a private copy");
                        None
                    }
                }
            } else {
                // Room for all of it, the tail filled from the file, then the view over the head.
                let a = self.space.map_anonymous(placement, len as usize, Protection::ReadWrite, CommitPolicy::Lazy).map_err(|_| refused_fixed(ENOMEM))? as u64;
                let tail = in_file.min(file_len.saturating_sub(req.offset)).saturating_sub(whole);
                let mut buf = vec![0u8; tail as usize];
                let n = crate::fd::pread_all(&file, &mut buf, req.offset + whole)?;
                p.mem.write_holding_layout(a + whole, &buf[..n])?;
                // `Fixed` does not replace: the head's anonymous pages go first (the layout lock is
                // held, so nothing else takes the hole).
                self.space.unmap(a as usize, whole as usize).map_err(|_| ENOMEM)?;
                match self.space.map_file(&backing, req.offset, Placement::Fixed(a as usize), whole as usize, prot) {
                    Ok(_) => {
                        if prot != Protection::ReadWrite {
                            self.space.protect((a + whole) as usize, (len - whole) as usize, prot).map_err(|_| ENOMEM)?;
                        }
                        self.forget(a, len);
                        self.files.lock().insert(a, FileMapping { len, guest: guest.clone(), offset: req.offset });
                        return Ok(a);
                    }
                    Err(e) => {
                        tracing::debug!(%e, "a file view could not be made; mapping a private copy");
                        let _ = self.space.unmap(a as usize, len as usize);
                        None
                    }
                }
            }
        } else {
            None
        };
        let label = |at: u64| {
            self.forget(at, len);
            self.files.lock().insert(at, FileMapping { len, guest: guest.clone(), offset: req.offset });
        };
        let at = match at {
            Some(a) => {
                if len > in_file {
                    self.space
                        .map_anonymous(Placement::Fixed((a + in_file) as usize), (len - in_file) as usize, prot, CommitPolicy::Lazy)
                        .map_err(|_| ENOMEM)?;
                }
                a
            }
            None => {
                // A private copy: anonymous read-write, filled from the file, then protected.
                let a = self
                    .space
                    .map_anonymous(placement, len as usize, Protection::ReadWrite, CommitPolicy::Lazy)
                    .map_err(|_| refused_fixed(ENOMEM))? as u64;
                let mut buf = vec![0u8; in_file.min(file_len - req.offset.min(file_len)) as usize];
                let n = crate::fd::pread_all(&file, &mut buf, req.offset)?;
                p.mem.write_holding_layout(a, &buf[..n])?;
                if prot != Protection::ReadWrite {
                    self.space.protect(a as usize, len as usize, prot).map_err(|_| ENOMEM)?;
                }
                a
            }
        };
        label(at);
        Ok(at)
    }
}

/// **An installed app's native library** (`/data/app/<package>/lib/<abi>/*.so`, extracted at
/// install and never written after) -- mapped privately as a view of the host file, as the image's
/// libraries are, instead of as a private copy: the pages no one writes are the host's file cache,
/// shared, and cost the process no commit (`libroblox.so` alone was 98 MiB of private copy in the
/// app's host process, run 2026-09-28). A write to such a mapping is copy-on-write, as a private
/// mapping's is.
///
/// Only those: a host file with a live view cannot be renamed or deleted on Windows, and installd
/// renames an install's staging directory (`vmdl*.tmp`) after PackageManager has parsed the APK in
/// it (`INSTALL_FAILED_INSUFFICIENT_STORAGE: Failed to rename`, run 2026-09-29, when every
/// `/data/app` file was a view). A library is mapped by its app alone, which is gone before its
/// package is.
fn is_installed(guest: &[u8]) -> bool {
    let path = String::from_utf8_lossy(guest);
    path.starts_with("/data/app/") && path.contains("/lib/") && path.ends_with(".so") && !path.contains(".tmp/")
}

/// The view backing of an installed library while something maps it: one per host file while it is
/// unchanged (the same size and time), and nothing kept once no mapping holds it.
fn installed_backing(path: &std::path::Path, len: u64, modified: Option<std::time::SystemTime>, guest: &[u8]) -> Option<Arc<omni_mem::Backing>> {
    type Cache = std::collections::HashMap<std::path::PathBuf, (u64, Option<std::time::SystemTime>, std::sync::Weak<omni_mem::Backing>)>;
    static CACHE: std::sync::OnceLock<Mutex<Cache>> = std::sync::OnceLock::new();
    let mut cache = CACHE.get_or_init(Mutex::default).lock();
    cache.retain(|_, (_, _, w)| w.strong_count() > 0);
    if let Some((l, m, w)) = cache.get(path) {
        if *l == len && *m == modified {
            if let Some(b) = w.upgrade() {
                return Some(b);
            }
        }
    }
    let backing = omni_mem::Backing::open_named(path, omni_mem::MapExecutability::Executable, &String::from_utf8_lossy(guest)).ok()?;
    cache.insert(path.to_path_buf(), (len, modified, Arc::downgrade(&backing)));
    Some(backing)
}

fn sys_mmap(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    // `/dev/binder`'s receive area is the driver's to fill.
    if a[3] & u64::from(MAP_ANONYMOUS) == 0 {
        if let Ok(file) = p.fds.get(a[4] as i64 as i32) {
            let binder = match &*file.kind.lock() {
                FileKind::Binder(b) => Some(std::sync::Arc::clone(b)),
                _ => None,
            };
            if let Some(b) = binder {
                return crate::binder::mmap(p, t, &b, a[1]);
            }
            let remote = match &*file.kind.lock() {
                FileKind::RemoteBinder(b) => Some(std::sync::Arc::clone(b)),
                _ => None,
            };
            if let Some(b) = remote {
                return b.mmap(p, t, a[1]);
            }
        }
    }
    p.mm.map(p, t, MapRequest { addr: a[0], len: a[1], prot: a[2] as u32, flags: a[3] as u32, fd: a[4] as i64 as i32, offset: a[5] })
}

fn sys_munmap(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mm.unmap(a[0], a[1]).map(|()| 0)
}

fn sys_mprotect(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mm.protect(a[0], a[1], a[2] as u32).map(|()| 0)
}

/// `madvise`. The advice that changes what memory reads -- `MADV_DONTNEED` and `MADV_REMOVE` --
/// leaves zeros; the rest are hints and are accepted. A success must mean what it means on Linux:
/// ART zeroes released arena memory with `MADV_REMOVE` and trusts the answer, and a no-op here
/// once left stale bytes that became garbage `DexCache` entries.
fn sys_madvise(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const MADV_REMOVE: u64 = 9;
    let addr = crate::guest::untag(a[0]);
    if addr % p.mm.page != 0 {
        return Err(EINVAL);
    }
    madvise_stats::count(a[2], a[1]);
    if matches!(a[2], MADV_DONTNEED | MADV_REMOVE) {
        let len = p.mm.span(addr, a[1]).ok_or(EINVAL)?;
        p.mm.discard(addr, len)?;
    }
    Ok(0)
}

/// `msync`: nothing is ever dirty against a file here (shared writable file mappings are refused),
/// so what it answers is whether the range is mapped -- which is what ART's low-4-GiB allocator
/// asks it. Outside the guest space the host owns the memory, so it is "in use" (0) there: ART
/// then skips it rather than trying to map there.
/// `mlock(addr, len)` and `munlock`: nothing here is ever paged out (there is no swap), so a
/// mapped range is resident already; a range with a hole is ENOMEM, as the kernel answers.
fn sys_mlock(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let addr = crate::guest::untag(a[0]);
    let start = addr - addr % p.mm.page;
    let len = p.mm.span(start, a[1] + (addr - start)).ok_or(ENOMEM)?;
    let space = p.mem.space();
    let mut at = start;
    while at < start + len {
        let region = space.region_at(at as usize).filter(|r| r.mapping.is_some()).ok_or(ENOMEM)?;
        at = (region.start + region.len) as u64;
    }
    Ok(0)
}

/// `mlock2(addr, len, flags)`: `MLOCK_ONFAULT` (1) or none.
fn sys_mlock2(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[2] & !1 != 0 {
        return Err(EINVAL);
    }
    sys_mlock(p, t, a)
}

/// `mlockall(flags)`: `MCL_CURRENT`, `MCL_FUTURE`, `MCL_ONFAULT` (with one of the others);
/// satisfied, nothing being paged out.
fn sys_mlockall(_p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const CURRENT: u64 = 1;
    const FUTURE: u64 = 2;
    const ONFAULT: u64 = 4;
    if a[0] == 0 || a[0] & !(CURRENT | FUTURE | ONFAULT) != 0 || a[0] == ONFAULT {
        return Err(EINVAL);
    }
    Ok(0)
}

fn sys_munlockall(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult {
    Ok(0)
}

fn sys_msync(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let addr = crate::guest::untag(a[0]);
    if addr % p.mm.page != 0 {
        return Err(EINVAL);
    }
    let len = p.mm.span(addr, a[1]).ok_or(ENOMEM)?;
    let space = p.mem.space();
    let (base, end) = (space.base() as u64, space.end() as u64);
    let mut at = addr;
    while at < addr + len {
        if at < base || at >= end {
            at = if at < base { base.min(addr + len) } else { addr + len };
            continue;
        }
        // A host-owned hole inside the space (`around_host`) is in use, like the host outside it.
        let region = space
            .region_at(at as usize)
            .filter(|r| r.mapping.is_some() || matches!(r.kind, omni_mem::RegionKind::Host))
            .ok_or(ENOMEM)?;
        at = (region.start + region.len) as u64;
    }
    Ok(0)
}

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;
const MREMAP_DONTUNMAP: u64 = 4;

fn prot_bits(p: Protection) -> u32 {
    match p {
        Protection::None => 0,
        Protection::Read => PROT_READ,
        Protection::ReadWrite => PROT_READ | PROT_WRITE,
        Protection::ReadExecute => PROT_READ | PROT_EXEC,
        Protection::ReadWriteExecute => PROT_READ | PROT_WRITE | PROT_EXEC,
    }
}

/// `mremap` by copy: shrink in place, or (with `MREMAP_MAYMOVE`) move to a new or `MREMAP_FIXED`
/// address, keeping contents and protection. bionic's CFI shadow uses the `FIXED` form to replace
/// a range atomically; scudo's secondary grows with `MAYMOVE`.
fn sys_mremap(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let page = p.mm.page;
    let (old, flags, target) = (crate::guest::untag(a[0]), a[3], crate::guest::untag(a[4]));
    let old_len = p.mm.span(old, a[1]).ok_or(EINVAL)?;
    let new_len = p.mm.span(if flags & MREMAP_FIXED != 0 { target } else { 0 }, a[2]).ok_or(ENOMEM)?;
    let dontunmap = flags & MREMAP_DONTUNMAP != 0;
    if dontunmap && (flags & MREMAP_MAYMOVE == 0 || new_len != old_len) {
        return Err(EINVAL);
    }
    if old % page != 0 || new_len == 0 || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 {
        p.refusals.record(format!("mremap flags {flags:#x}"), t.pc, t.lr);
        return Err(EINVAL);
    }
    let fixed = flags & MREMAP_FIXED != 0;
    if fixed && (flags & MREMAP_MAYMOVE == 0 || target % page != 0 || (target < old + old_len && old < target + new_len)) {
        return Err(EINVAL);
    }
    // Shrinking in place (DONTUNMAP always moves: its point is a second range).
    if !fixed && !dontunmap && new_len <= old_len {
        if new_len < old_len {
            p.mm.unmap(old + new_len, old_len - new_len)?;
        }
        return Ok(old);
    }
    if !fixed && flags & MREMAP_MAYMOVE == 0 {
        return Err(ENOMEM); // growing in place is not offered
    }
    let space = p.mem.space();
    let region = space.region_at(old as usize).filter(|r| r.mapping.is_some()).ok_or(EFAULT)?;
    let prot = region.protection;
    let keep = old_len.min(new_len);
    if prot == Protection::None || prot == Protection::ReadExecute {
        p.mm.protect(old, keep, PROT_READ).map_err(|_| EFAULT)?;
    }
    let flags = 0x22 | if fixed { MAP_FIXED } else { 0 }; // MAP_PRIVATE | MAP_ANONYMOUS
    let at = p.mm.map(p, t, MapRequest { addr: if fixed { target } else { 0 }, len: new_len, prot: PROT_READ | PROT_WRITE, flags, fd: -1, offset: 0 })?;
    // Only what the old range holds: its committed pages and its file's. Linux moves page-table
    // entries and touches no page; a copy of every byte read the never-touched pages as zeros and
    // wrote them, committing the whole range -- ART's concurrent-mark-compact GC moves its 512 MiB
    // space with MREMAP_DONTUNMAP, and every Java process then held 504 MiB it never used (35 of
    // them after boot exhausted the host). The new range is fresh anonymous memory: zeros already.
    // In pieces: a committed run may be a GC space of hundreds of MiB.
    const PIECE: u64 = 16 << 20;
    let held: Vec<(u64, u64)> = space.held_ranges(old as usize, keep as usize).into_iter().map(|(s, n)| (s as u64, (s + n) as u64)).collect();
    for (start, end) in held {
        let mut done = start;
        while done < end {
            let n = (end - done).min(PIECE);
            let bytes = p.mem.read(done, n as usize)?;
            p.mem.write(at + (done - old), &bytes)?;
            done += n;
        }
    }
    if prot != Protection::ReadWrite {
        p.mm.protect(at, new_len, prot_bits(prot))?;
    }
    if dontunmap {
        // The old range stays mapped, its pages gone: it reads zeros, with its protection back.
        p.mm.discard(old, old_len)?;
        if prot == Protection::None || prot == Protection::ReadExecute {
            p.mm.protect(old, keep, prot_bits(prot))?;
        }
    } else {
        p.mm.unmap(old, old_len)?;
    }
    Ok(at)
}

/// Always the same break: bionic's allocator uses `mmap`, and `sbrk` callers see "no growth".
fn sys_brk(_p: &Process, _t: &mut Task, _a: [u64; 6]) -> SysResult {
    Ok(0)
}

pub fn install(table: &mut Table) {
    table.set(nr::MMAP, sys_mmap);
    table.set(nr::MUNMAP, sys_munmap);
    table.set(nr::MPROTECT, sys_mprotect);
    table.set(nr::MADVISE, sys_madvise);
    table.set(nr::MSYNC, sys_msync);
    table.set(nr::MLOCK, sys_mlock);
    table.set(nr::MUNLOCK, sys_mlock);
    table.set(nr::MLOCK2, sys_mlock2);
    table.set(nr::MLOCKALL, sys_mlockall);
    table.set(nr::MUNLOCKALL, sys_munlockall);
    table.set(nr::BRK, sys_brk);
    table.set(nr::MREMAP, sys_mremap);
}

/// `OMNI_MADVISE_STATS=<seconds>`: this host process's `madvise` calls by advice, with the bytes
/// they named, every so often (`[madvise]`) -- which advice an allocator gives memory back with.
mod madvise_stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::OnceLock;

    const ADVICE: usize = 32;
    static CALLS: [AtomicU64; ADVICE] = [const { AtomicU64::new(0) }; ADVICE];
    static BYTES: [AtomicU64; ADVICE] = [const { AtomicU64::new(0) }; ADVICE];

    fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            let Some(every) = std::env::var("OMNI_MADVISE_STATS").ok().and_then(|v| v.parse::<u64>().ok()) else { return false };
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
                let rows: Vec<String> = (0..ADVICE)
                    .filter_map(|i| {
                        let (c, b) = (CALLS[i].swap(0, Relaxed), BYTES[i].swap(0, Relaxed));
                        (c > 0).then(|| format!("advice {i}: {c}x {} MiB", b >> 20))
                    })
                    .collect();
                if !rows.is_empty() {
                    eprintln!("[madvise] host pid {} {every}s: {}", std::process::id(), rows.join(", "));
                }
            });
            true
        })
    }

    pub(super) fn count(advice: u64, len: u64) {
        if on() {
            let i = (advice as usize).min(ADVICE - 1);
            CALLS[i].fetch_add(1, Relaxed);
            BYTES[i].fetch_add(len, Relaxed);
        }
    }
}
