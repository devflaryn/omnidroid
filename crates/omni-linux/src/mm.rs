//! Linux `mmap` semantics on `omni-mem`'s guest space.
use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};
use parking_lot::Mutex;

use crate::errno::*;
use crate::fd::FileKind;
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const PAGE: u64 = 4096;
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

pub struct Mm {
    space: Arc<GuestSpace>,
    /// `MAP_FIXED`'s unmap-then-map must not interleave with another thread's mapping.
    lock: Mutex<()>,
}

const fn round_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
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
    pub fn new(space: Arc<GuestSpace>) -> Self {
        Self { space, lock: Mutex::new(()) }
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
        Ok(())
    }

    pub fn unmap(&self, addr: u64, len: u64) -> Result<(), Errno> {
        if addr % PAGE != 0 || len == 0 {
            return Err(EINVAL);
        }
        let _g = self.lock.lock();
        self.unmap_locked(addr, round_up(len))
    }

    pub fn protect(&self, addr: u64, len: u64, prot: u32) -> Result<(), Errno> {
        if addr % PAGE != 0 {
            return Err(EINVAL);
        }
        let prot = protection(prot)?;
        self.space.protect(addr as usize, round_up(len) as usize, prot).map_err(|_| ENOMEM)
    }

    pub fn map(&self, p: &Process, t: &Task, req: MapRequest) -> Result<u64, Errno> {
        if req.len == 0 || req.offset % PAGE != 0 {
            return Err(EINVAL);
        }
        let prot = protection(req.prot).inspect_err(|_| {
            p.refusals.record("mmap: PROT_WRITE|PROT_EXEC".into(), t.pc, t.lr);
        })?;
        let len = round_up(req.len);
        let fixed = req.flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
        if fixed && req.addr % PAGE != 0 {
            return Err(EINVAL);
        }
        let page = self.space.page_size();
        let placement = if fixed {
            Placement::Fixed(req.addr as usize)
        } else if req.addr != 0 {
            Placement::Hint { address: (req.addr & !(PAGE - 1)) as usize, align: page }
        } else {
            Placement::Anywhere { align: page }
        };
        let _g = self.lock.lock();
        if req.flags & MAP_FIXED != 0 {
            self.unmap_locked(req.addr, len)?;
        }
        let refused_fixed = |e| if fixed { EEXIST } else { e };
        if req.flags & MAP_ANONYMOUS != 0 {
            return self
                .space
                .map_anonymous(placement, len as usize, prot, CommitPolicy::Lazy)
                .map(|a| a as u64)
                .map_err(|_| refused_fixed(ENOMEM));
        }
        let file = p.fds.get(req.fd)?;
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
        let in_file = round_up(file_len.saturating_sub(req.offset)).min(len);
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
                p.mem.write(a, &buf[..n])?;
                if prot != Protection::ReadWrite {
                    self.space.protect(a as usize, len as usize, prot).map_err(|_| ENOMEM)?;
                }
                a
            }
        };
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
        p.mem.space().discard(a[0] as usize, round_up(a[1]) as usize).map_err(|_| EINVAL)?;
    }
    Ok(0)
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
    table.set(nr::BRK, sys_brk);
}
