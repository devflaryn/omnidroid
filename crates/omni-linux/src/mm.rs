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
    /// The guest's page: 4 KiB on every host that can give it (D42) -- the host's own on Windows and
    /// x86-64 Linux, the sub-page overlay's on Apple silicon (`omni_mem::subpage`). The guest is told
    /// it (`AT_PAGESZ`), and every `mmap`, `mprotect` and `munmap` is exact at it.
    page: u64,
    /// The host's page: what a file view and a fresh placement are whole multiples of.
    host_page: u64,
    /// The layout lock, held exclusively for every change: `MAP_FIXED`'s unmap-then-map must not
    /// interleave with another thread's mapping, nor any change with a syscall's copy.
    lock: crate::guest::Layout,
    files: Mutex<std::collections::BTreeMap<u64, FileMapping>>,
}

/// The layout lock, held either way (see [`Mm::discard`]) -- for its drop.
#[allow(dead_code)]
enum Lock<'a> {
    Shared(parking_lot::RwLockReadGuard<'a, ()>),
    Exclusive(parking_lot::RwLockWriteGuard<'a, ()>),
}

/// `OMNI_DISCARD_SHARED=1`: a guest discard under the layout lock shared ([`Mm::discard`]).
fn discard_shared() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_DISCARD_SHARED").as_deref() == Ok("1"))
}

/// `OMNI_STRICT_GAPS=<prefix>,...`: named ranges (`PR_SET_VMA_ANON_NAME`, or a file's path) whose
/// unmapped 4 KiB must fault even inside a host page that holds other mappings (D42's escape
/// hatch; by default such a gap is lenient). Empty by default.
fn strict_gap_prefixes() -> &'static [Vec<u8>] {
    static LIST: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("OMNI_STRICT_GAPS")
            .unwrap_or_default()
            .split(',')
            .filter(|p| !p.is_empty())
            .map(|p| p.as_bytes().to_vec())
            .collect()
    })
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
        let page = space.guest_page_size() as u64;
        let host_page = space.page_size() as u64;
        Self { space, page, host_page, lock, files: Mutex::default() }
    }

    /// The page size the guest is told and every `mmap`, `mprotect` and `munmap` is exact at.
    #[must_use]
    pub const fn page_size(&self) -> u64 {
        self.page
    }

    const fn round_up(&self, v: u64) -> u64 {
        (v + self.page - 1) & !(self.page - 1)
    }

    const fn round_up_host(&self, v: u64) -> u64 {
        (v + self.host_page - 1) & !(self.host_page - 1)
    }

    /// Whether a file at `offset` can be a view here: a view is whole host pages at a host-page
    /// file offset, so the offset -- and, for a fixed address, the address -- must be host-page
    /// aligned. A 4 KiB-aligned one on a larger host page (a library linked for 4 KiB pages) is a
    /// private copy instead (D42).
    fn viewable(&self, offset: u64, addr: u64, fixed: bool) -> bool {
        offset % self.host_page == 0 && (!fixed || addr % self.host_page == 0)
    }

    /// A shared mapping a host view cannot honour: a *fixed* address that does not agree with its
    /// file offset modulo the host page (a placement we choose is made to agree,
    /// `map_shared_view`). Refused, as sharing a copy would silently not be sharing (D42).
    fn refuse_unshareable(&self, p: &Process, t: &Task, req: &MapRequest, fixed: bool) -> Result<(), Errno> {
        if !fixed || req.addr % self.host_page == req.offset % self.host_page {
            return Ok(());
        }
        p.refusals.record("mmap: MAP_SHARED at a fixed address incongruent with its offset".into(), t.pc, t.lr);
        Err(EINVAL)
    }

    /// Map `backed` bytes of `backing` from `req.offset` for a shared mapping of `len` bytes. A view
    /// is whole host pages at a host-page file offset, so the view starts at the host page below the
    /// offset and is placed so the guest's address agrees with the offset modulo the host page (an
    /// FMQ ring, a plane of a graphics buffer, at a 4 KiB offset: D42). The view's parts before the
    /// address and past `len` are given back as 4 KiB holes; anything of `len` past the view is
    /// anonymous.
    fn map_shared_view(&self, backing: &Arc<omni_mem::Backing>, req: &MapRequest, placement: Placement, backed: u64, len: u64, prot: Protection) -> Result<u64, omni_mem::MemError> {
        let host = self.host_page;
        let head = req.offset % host;
        let viewed = self.round_up_host(head + backed);
        let placement = match placement {
            Placement::Fixed(a) => Placement::Fixed(a - head as usize),
            Placement::Hint { address, align } => Placement::Hint { address: address & !(host as usize - 1), align },
            anywhere @ Placement::Anywhere { .. } => anywhere,
        };
        let base = self.space.map_file(backing, req.offset - head, placement, viewed as usize, prot)? as u64;
        let at = base + head;
        if head > 0 {
            self.space.unmap(base as usize, head as usize)?;
        }
        let from_at = viewed - head;
        if from_at > len {
            self.space.unmap((at + len) as usize, (from_at - len) as usize)?;
        } else if len > from_at {
            self.space.map_anonymous(Placement::Fixed((at + from_at) as usize), (len - from_at) as usize, prot, CommitPolicy::Lazy)?;
        }
        Ok(at)
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
        self.mark_strict(start, len, name);
    }

    /// A range named on `OMNI_STRICT_GAPS` gets strict gaps (`GuestSpace::set_strict_gaps`).
    fn mark_strict(&self, start: u64, len: u64, name: &[u8]) {
        let list = strict_gap_prefixes();
        if list.is_empty() {
            return;
        }
        let bare = name.strip_prefix(b"[anon:".as_slice()).and_then(|n| n.strip_suffix(b"]".as_slice())).unwrap_or(name);
        if list.iter().any(|p| bare.starts_with(p)) {
            self.space.set_strict_gaps(start as usize, len as usize, true);
        }
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
    ///
    /// File mappings never overlap (each is inserted over a range just mapped, after `forget`), so
    /// the ones in the range are the one before `addr` if it reaches past it and those starting
    /// inside: two lookups, not a walk of every mapping below `end` -- which an ART start's
    /// thousands of `munmap`s each paid.
    fn forget(&self, addr: u64, len: u64) {
        let end = addr + len;
        let mut files = self.files.lock();
        let mut hit: Vec<u64> = Vec::new();
        if let Some((s, m)) = files.range(..addr).next_back() {
            if *s + m.len > addr {
                hit.push(*s);
            }
        }
        hit.extend(files.range(addr..end).map(|(s, _)| *s));
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
        // The regions overlapping the range, from the space's index: not every region of the
        // space built (and merged, and allocated) for every munmap. Clipped below, as before.
        for r in self.space.mapped_regions_overlapping(start, end - start) {
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
        // `OMNI_DISCARD_SHARED=1`: under the layout lock shared, not exclusive. A discard changes no
        // mapping -- its pages stay mapped, lazily committed -- so a copy that touches one it just
        // decommitted is served by the demand pager (committed again, zeros), unlike a page
        // `munmap` took away, which is what the exclusive lock is for. Exclusive, each of the
        // game's MADV_DONTNEEDs (53,883 in its first minute, s30) waited for every copy in flight
        // and held every other thread's copies and mappings off while it ran.
        let _g = if discard_shared() { Lock::Shared(self.lock.read()) } else { Lock::Exclusive(self.lock.write()) };
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
        // A placement the guest leaves to us is whole host pages of its own, so separate mappings
        // never share a host page (the 4 KiB overlay then has nothing to split for them).
        let page = self.host_page as usize;
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
            self.refuse_unshareable(p, t, &req, fixed)?;
            let host = m.dup_file().map_err(|_| ENODEV)?;
            let backing = omni_mem::Backing::share(host, &String::from_utf8_lossy(&name)).map_err(|_| ENODEV)?;
            let backed = self.round_up((m.len()).saturating_sub(req.offset)).min(len);
            let at = if backed > 0 {
                self.map_shared_view(&backing, &req, placement, backed, len, prot).map_err(|e| {
                    if fixed {
                        eprintln!("[mm] {} at {:#x}+{backed:#x} refused: {e}", String::from_utf8_lossy(&name), req.addr);
                    }
                    refused_fixed(ENOMEM)
                })?
            } else {
                self.space
                    .map_anonymous(placement, len as usize, prot, CommitPolicy::Lazy)
                    .map_err(|_| refused_fixed(ENOMEM))? as u64
            };
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
            // Only up to the last non-zero byte: the mapping is fresh anonymous memory, zero already,
            // and a prop area is mostly room to grow (~61 KiB used of ~1.1 MiB) -- writing its zero
            // tail committed every page of it in every guest process (~80 copies in the system's
            // host process, docs/NIGHT-2026-10-02.md). The property service's later writes commit
            // what they touch.
            // (`OMNI_PROP_FULL_COPY=1`: the whole area written, as before -- the A/B's other arm.)
            let copy = |bytes: &[u8]| {
                let from = (req.offset as usize).min(bytes.len());
                let n = (len as usize).min(bytes.len() - from);
                let bytes = &bytes[from..from + n];
                p.mem.write_holding_layout(at, if full_prop_copy() { bytes } else { nonzero_prefix(bytes) })
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
            self.refuse_unshareable(p, t, &req, fixed)?;
            let in_file = self.round_up(file_len.saturating_sub(req.offset)).min(len);
            // Past the file's end the mapping is anonymous memory, private commit, and stays so when
            // the file grows (a pool mapped first and grown with ftruncate) -- said, for a large one.
            if len - in_file >= 16 << 20 {
                eprintln!(
                    "[mm] shared {} +{} MiB at offset {:#x}: {} MiB of it past the file's end ({} bytes), anonymous",
                    String::from_utf8_lossy(&guest),
                    len >> 20,
                    req.offset,
                    (len - in_file) >> 20,
                    file_len
                );
            }
            let at = if in_file > 0 {
                let backing = omni_mem::Backing::share(host, &String::from_utf8_lossy(&guest)).map_err(|_| {
                    p.refusals.record("mmap: MAP_SHARED of a file the host cannot share".into(), t.pc, t.lr);
                    ENODEV
                })?;
                self.map_shared_view(&backing, &req, placement, in_file, len, prot).map_err(|_| refused_fixed(ENOMEM))?
            } else {
                self.space.map_anonymous(placement, len as usize, prot, CommitPolicy::Lazy).map_err(|_| refused_fixed(ENOMEM))? as u64
            };
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
        if !sysroot && installed.is_none() && len >= 64 << 20 {
            // A large private mapping of an app's own file: a copy, private commit -- said.
            eprintln!(
                "[mm] private {} +{} MiB at offset {:#x} (file {} bytes, {}shared, prot {:#x}): a copy",
                String::from_utf8_lossy(&guest),
                len >> 20,
                req.offset,
                file_len,
                if req.flags & MAP_SHARED != 0 { "" } else { "not " },
                req.prot
            );
        }
        let backing = if in_file == 0 || !self.viewable(req.offset, req.addr, fixed) {
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
        // Whole *host* pages: what a view can be (D42).
        let whole = (file_len.saturating_sub(req.offset) / self.host_page * self.host_page).min(len / self.host_page * self.host_page);
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
        if crate::boot_image::enabled() && crate::boot_image::is_boot_art(&guest) {
            report_boot_image(p, &file, &guest, at.is_some(), at.unwrap_or(0), len, req.offset);
        }
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

/// `[bootimage]`: how a boot image file was mapped (`OMNI_BOOT_IMAGE_UNCOMPRESSED`) -- a view
/// (shared, copy-on-write per page) or a private copy, and whether at the address it was compiled
/// for (`image_begin` in its header: not relocated) or elsewhere.
fn report_boot_image(p: &Process, file: &crate::fd::OpenFile, guest: &[u8], view: bool, at: u64, len: u64, offset: u64) {
    let mut header = vec![0u8; crate::boot_image::HEADER_SIZE];
    let begin = crate::fd::pread_all(file, &mut header, 0).ok().and_then(|_| crate::boot_image::image_begin(&header));
    let name = String::from_utf8_lossy(guest);
    let place = match begin {
        Some(b) if offset == 0 && view && at == u64::from(b) => "at its compiled address (not relocated)".to_string(),
        Some(b) if offset == 0 && view => format!("relocated by {:+#x}", at as i64 - i64::from(b)),
        _ => String::new(),
    };
    if view {
        eprintln!("[bootimage] pid {} {name}: a view at {at:#x} (+{len:#x}, offset {offset:#x}) {place}", p.sys.pid);
    } else {
        eprintln!("[bootimage] pid {} {name}: a private copy (+{len:#x}, offset {offset:#x}) -- not shared", p.sys.pid);
    }
}

/// `OMNI_PROP_FULL_COPY=1`: a mapped prop area is written whole, zero tail and all (the old way) --
/// and every process keeps its `/dev/__properties__` areas whole (`crate::props::PropBlob`) --
/// here and in the property service's writes of a change (`crate::props`).
pub(crate) fn full_prop_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_PROP_FULL_COPY").as_deref() == Ok("1"))
}

/// `bytes` up to and including its last non-zero byte: what has to be written into fresh (zero)
/// memory to hold them, touching no page the zero tail would.
fn nonzero_prefix(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1)]
}

#[cfg(test)]
mod nonzero_prefix_tests {
    use super::nonzero_prefix;

    #[test]
    fn the_zero_tail_is_left_out() {
        assert_eq!(nonzero_prefix(&[1, 0, 2, 0, 0, 0]), &[1, 0, 2]);
        assert_eq!(nonzero_prefix(&[0, 0, 0]), &[] as &[u8]);
        assert_eq!(nonzero_prefix(&[]), &[] as &[u8]);
        assert_eq!(nonzero_prefix(&[0, 7]), &[0, 7]);
    }

    #[test]
    fn a_prop_area_is_mostly_zero_tail() {
        // What `attach` hands the mapping: the area holding every property, at its capacity.
        let area = crate::props::serial_area_bytes();
        assert!(nonzero_prefix(&area).len() < area.len() / 4, "{} of {}", nonzero_prefix(&area).len(), area.len());
    }
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
    // `OMNI_JIT_SNAPSHOT_LIB_ZONE`: the dynamic linker's library reservations placed by the library.
    let mut a = a;
    let asked = a[0];
    crate::jit_snapshot::linker_mmap(p, t.tid, t.pc, &mut a);
    let r = p.mm.map(p, t, MapRequest { addr: a[0], len: a[1], prot: a[2] as u32, flags: a[3] as u32, fd: a[4] as i64 as i32, offset: a[5] });
    if a[0] != asked {
        crate::jit_snapshot::linker_mmap_placed(p, a[0], &r);
    }
    mmap_watch(p, t, a, &r);
    crate::mmap_log::mmap(p, t, a, &r);
    r
}

/// **Diagnostic** (`OMNI_MMAP_WATCH=<length>`, off by default): every anonymous `mmap` of exactly
/// that length is logged -- thread, where it was called from, what it answered, and when.
fn mmap_watch(p: &Process, t: &Task, a: [u64; 6], r: &Result<u64, Errno>) {
    static WATCH: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let Some(len) = *WATCH.get_or_init(|| std::env::var("OMNI_MMAP_WATCH").ok().and_then(|v| v.parse().ok())) else { return };
    if a[1] != len || a[3] & u64::from(MAP_ANONYMOUS) == 0 {
        return;
    }
    let ms = START.get_or_init(std::time::Instant::now).elapsed().as_millis();
    eprintln!(
        "[mmap-watch] +{ms}ms pid {} tid {} ({}) pc {:#x} {} lr {:#x} {} prot {} flags {:#x} -> {:?}",
        p.sys.pid, t.tid, String::from_utf8_lossy(&t.name), t.pc, p.mm.describe(t.pc).unwrap_or_default(), t.lr, p.mm.describe(t.lr).unwrap_or_default(), a[2], a[3], r
    );
}

fn sys_munmap(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    crate::mmap_log::munmap(p, t, a);
    let probed = smc_probe(p, a[0], a[1], None);
    p.mm.unmap(a[0], a[1])?;
    if probed {
        p.invalidate_code(crate::guest::untag(a[0]), a[1]);
    }
    Ok(0)
}

fn sys_mprotect(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let probed = smc_probe(p, a[0], a[1], Some(a[2] as u32));
    p.mm.protect(a[0], a[1], a[2] as u32)?;
    if probed {
        p.invalidate_code(crate::guest::untag(a[0]), a[1]);
    }
    Ok(0)
}

/// **Diagnostic probe** (`OMNI_SMC_PROBE=<name part>`, off by default): an `mprotect` or `munmap` of
/// a mapping whose name contains it drops that range's translations on every thread, and is logged
/// with its protection -- whether a library that rewrites its own code runs stale translations.
fn smc_probe(p: &Process, addr: u64, len: u64, prot: Option<u32>) -> bool {
    static WANT: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    let Some(want) = WANT.get_or_init(|| std::env::var("OMNI_SMC_PROBE").ok().filter(|w| !w.is_empty()).map(String::into_bytes)) else {
        return false;
    };
    let addr = crate::guest::untag(addr);
    let Some((name, offset)) = p.mm.name_at(addr) else { return false };
    if !name.windows(want.len()).any(|w| w == want.as_slice()) {
        return false;
    }
    let tail = name.rsplit(|&b| b == b'/').next().unwrap_or(&name);
    match prot {
        Some(prot) => eprintln!("[smc] {} mprotect {}+{offset:#x} len {len:#x} prot {prot}", p.sys.pid, String::from_utf8_lossy(tail)),
        None => eprintln!("[smc] {} munmap {}+{offset:#x} len {len:#x}", p.sys.pid, String::from_utf8_lossy(tail)),
    }
    true
}

/// `madvise`. The advice that changes what memory reads -- `MADV_DONTNEED` and `MADV_REMOVE` --
/// leaves zeros; the rest are hints and are accepted. A success must mean what it means on Linux:
/// ART zeroes released arena memory with `MADV_REMOVE` and trusts the answer, and a no-op here
/// once left stale bytes that became garbage `DexCache` entries.
///
/// `MADV_FREE` is a hint here unless [`MADV_FREE_DISCARDS`] is on; then it is carried out as
/// `MADV_DONTNEED` -- one of the outcomes Linux allows ("the old contents or zeros", the kernel
/// free to drop the pages the moment the call returns), and what the previous engine
/// (`omni-android`'s `madvise`) did: the range is decommitted and its next touch reads zeros. An
/// allocator that purges with it (mimalloc's reset mode, `MADV_FREE` "when purging by reset")
/// then gives the host its RAM and commit back instead of keeping them for good.
fn sys_madvise(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const MADV_FREE: u64 = 8;
    const MADV_REMOVE: u64 = 9;
    let addr = crate::guest::untag(a[0]);
    if addr % p.mm.page != 0 {
        return Err(EINVAL);
    }
    madvise_stats::count(a[2], a[1]);
    let discards = matches!(a[2], MADV_DONTNEED | MADV_REMOVE)
        || (a[2] == MADV_FREE && MADV_FREE_DISCARDS.load(std::sync::atomic::Ordering::Relaxed));
    if discards {
        let len = p.mm.span(addr, a[1]).ok_or(EINVAL)?;
        let t = madvise_stats::start();
        p.mm.discard(addr, len)?;
        madvise_stats::timed(a[2], t);
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
    let r = mremap(p, t, a);
    crate::mmap_log::mremap(p, t, a, &r);
    r
}

fn mremap(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
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

/// Whether a guest `MADV_FREE` discards its range (as `MADV_DONTNEED`) rather than being a hint:
/// the `madv_free=` lever (`crate::lever`), or `OMNI_MADV_FREE=1` from the start. Off by default.
pub static MADV_FREE_DISCARDS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `OMNI_MADV_FREE=1`: [`MADV_FREE_DISCARDS`] on from the start (read once, by the lever reader).
pub fn madv_free_from_env() {
    if std::env::var("OMNI_MADV_FREE").as_deref() == Ok("1") {
        MADV_FREE_DISCARDS.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[lever] OMNI_MADV_FREE: madv_free=1");
    }
}

/// `OMNI_MADVISE_STATS=<seconds>`: this host process's `madvise` calls by advice, with the bytes
/// they named, every so often (`[madvise]`) -- which advice an allocator gives memory back with.
mod madvise_stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::OnceLock;

    const ADVICE: usize = 32;
    static CALLS: [AtomicU64; ADVICE] = [const { AtomicU64::new(0) }; ADVICE];
    static BYTES: [AtomicU64; ADVICE] = [const { AtomicU64::new(0) }; ADVICE];
    /// Nanoseconds spent carrying the advice out (a discard: the layout lock held exclusively).
    static NANOS: [AtomicU64; ADVICE] = [const { AtomicU64::new(0) }; ADVICE];

    fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            let Some(every) = std::env::var("OMNI_MADVISE_STATS").ok().and_then(|v| v.parse::<u64>().ok()) else { return false };
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
                let rows: Vec<String> = (0..ADVICE)
                    .filter_map(|i| {
                        let (c, b, ns) = (CALLS[i].swap(0, Relaxed), BYTES[i].swap(0, Relaxed), NANOS[i].swap(0, Relaxed));
                        (c > 0).then(|| format!("advice {i}: {c}x {} MiB in {} ms", b >> 20, ns / 1_000_000))
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

    /// When counting: the time from `since` on, for `advice`.
    pub(super) fn timed(advice: u64, since: Option<std::time::Instant>) {
        if let Some(t) = since {
            NANOS[(advice as usize).min(ADVICE - 1)].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        }
    }

    /// An instant to time a call from, when counting.
    pub(super) fn start() -> Option<std::time::Instant> {
        on().then(std::time::Instant::now)
    }
}
