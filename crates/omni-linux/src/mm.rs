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
        (_, true, true) => return Err(EACCES),
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

    /// `MADV_DONTNEED`: the range reads as zeros again.
    pub fn discard(&self, addr: u64, len: u64) -> Result<(), Errno> {
        let _g = self.lock.write();
        self.space.discard(addr as usize, len as usize).map(|_| ()).map_err(|_| EINVAL)
    }

    pub fn map(&self, p: &Process, t: &Task, req: MapRequest) -> Result<u64, Errno> {
        if req.len == 0 || req.offset % self.page != 0 {
            return Err(EINVAL);
        }
        let prot = protection(req.prot).inspect_err(|_| {
            p.refusals.record("mmap: PROT_WRITE|PROT_EXEC".into(), t.pc, t.lr);
        })?;
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
                .map_err(|_| refused_fixed(ENOMEM));
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
        if let FileKind::Synth { data, guest, .. } = &*file.kind.lock() {
            // An in-memory file (`/dev/__properties__`): a private copy, read-only in effect.
            if req.flags & MAP_SHARED != 0 && req.prot & PROT_WRITE != 0 {
                return Err(EACCES);
            }
            let at = self
                .space
                .map_anonymous(placement, len as usize, Protection::ReadWrite, CommitPolicy::Lazy)
                .map_err(|_| refused_fixed(ENOMEM))? as u64;
            let from = (req.offset as usize).min(data.len());
            let n = (len as usize).min(data.len() - from);
            p.mem.write_holding_layout(at, &data[from..from + n])?;
            if prot != Protection::ReadWrite {
                self.space.protect(at as usize, len as usize, prot).map_err(|_| ENOMEM)?;
            }
            self.files.lock().insert(at, FileMapping { len, guest: guest.clone(), offset: req.offset });
            return Ok(at);
        }
        let (guest, sysroot, file_len) = match &*file.kind.lock() {
            FileKind::Host { file, guest, sysroot } => {
                (guest.clone(), *sysroot, file.metadata().map_err(|_| EIO)?.len())
            }
            _ => return Err(ENODEV),
        };
        if req.flags & MAP_SHARED != 0 && req.prot & PROT_WRITE != 0 && !sysroot {
            p.refusals.record("mmap: MAP_SHARED|PROT_WRITE of a writable file".into(), t.pc, t.lr);
            return Err(ENODEV);
        }
        // The part of the request the file covers, in whole pages; the rest is anonymous zeros.
        let in_file = self.round_up(file_len.saturating_sub(req.offset)).min(len);
        let at = if in_file > 0 && sysroot {
            let backing = p.vfs.sysroot().backing(&guest)?;
            match self.space.map_file(&backing, req.offset, placement, in_file as usize, prot) {
                Ok(a) => Some(a as u64),
                Err(e) => {
                    tracing::debug!(%e, "a file view could not be made; mapping a private copy");
                    None
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

fn sys_mmap(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mm.map(p, t, MapRequest { addr: a[0], len: a[1], prot: a[2] as u32, flags: a[3] as u32, fd: a[4] as i64 as i32, offset: a[5] })
}

fn sys_munmap(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mm.unmap(a[0], a[1]).map(|()| 0)
}

fn sys_mprotect(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    p.mm.protect(a[0], a[1], a[2] as u32).map(|()| 0)
}

fn sys_madvise(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[2] == MADV_DONTNEED {
        let addr = crate::guest::untag(a[0]);
        let len = p.mm.span(addr, a[1]).ok_or(EINVAL)?;
        p.mm.discard(addr, len)?;
    }
    Ok(0)
}

/// `msync`: nothing is ever dirty against a file here (shared writable file mappings are refused),
/// so what it answers is whether the range is mapped -- which is what ART's low-4-GiB allocator
/// asks it. Outside the guest space the host owns the memory, so it is "in use" (0) there: ART
/// then skips it rather than trying to map there.
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
        let region = space.region_at(at as usize).filter(|r| r.mapping.is_some()).ok_or(ENOMEM)?;
        at = (region.start + region.len) as u64;
    }
    Ok(0)
}

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

fn prot_bits(p: Protection) -> u32 {
    match p {
        Protection::None => 0,
        Protection::Read => PROT_READ,
        Protection::ReadWrite => PROT_READ | PROT_WRITE,
        Protection::ReadExecute => PROT_READ | PROT_EXEC,
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
    if old % page != 0 || new_len == 0 || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        p.refusals.record(format!("mremap flags {flags:#x}"), t.pc, t.lr);
        return Err(EINVAL);
    }
    let fixed = flags & MREMAP_FIXED != 0;
    if fixed && (flags & MREMAP_MAYMOVE == 0 || target % page != 0 || (target < old + old_len && old < target + new_len)) {
        return Err(EINVAL);
    }
    if !fixed && new_len <= old_len {
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
    let bytes = p.mem.read(old, keep as usize)?;
    let flags = 0x22 | if fixed { MAP_FIXED } else { 0 }; // MAP_PRIVATE | MAP_ANONYMOUS
    let at = p.mm.map(p, t, MapRequest { addr: if fixed { target } else { 0 }, len: new_len, prot: PROT_READ | PROT_WRITE, flags, fd: -1, offset: 0 })?;
    p.mem.write(at, &bytes)?;
    if prot != Protection::ReadWrite {
        p.mm.protect(at, new_len, prot_bits(prot))?;
    }
    p.mm.unmap(old, old_len)?;
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
    table.set(nr::BRK, sys_brk);
    table.set(nr::MREMAP, sys_mremap);
}
