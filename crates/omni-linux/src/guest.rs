//! Checked copies between guest memory and the host: the kernel's `copy_{from,to}_user`.
//!
//! A range that is not mapped, or not readable (writable) throughout, is `EFAULT`, never a host
//! fault: identity mapping (D4) makes a guest pointer a host pointer, so an unchecked copy through
//! a hostile one would be the host's crash.
use std::sync::Arc;

use omni_mem::{GuestSpace, Protection};

use crate::errno::{Errno, EFAULT};

/// Clear an address's top byte, as arm64 Linux's tagged-address ABI does for user pointers
/// (Android's scudo tags every heap pointer: see `DynarmicOptions::top_byte_ignore`).
#[must_use]
pub const fn untag(addr: u64) -> u64 {
    addr & 0x00FF_FFFF_FFFF_FFFF
}

/// The lock on the space's layout. A copy holds it shared from its check to its last byte;
/// `mmap`, `munmap`, `mprotect` and `madvise` hold it exclusively. Without it a copy could pass its
/// check and then touch a page another thread had just unmapped: a host access violation (A2-A5
/// review, Important 7).
pub type Layout = Arc<parking_lot::RwLock<()>>;

/// Memory that is another host process's (a stand-in's: `crate::remote`).
pub trait Remote: Send + Sync {
    fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno>;
    fn write(&self, addr: u64, bytes: &[u8]) -> Result<(), Errno>;
}

/// One side of a live fork pair whose two views take turns in the one memory (`crate::fork`).
/// Every copy this view makes waits here first, so a call that completes for the side whose view is
/// *not* in the address space cannot write into the other's memory.
pub trait Resident: Send + Sync {
    /// Block until this side's view is the one in the address space.
    fn ensure(&self);
}

pub struct GuestMem {
    space: Arc<GuestSpace>,
    layout: Layout,
    /// Set for a stand-in: every read and write goes there.
    remote: std::sync::OnceLock<Arc<dyn Remote>>,
    /// While a fork child runs in this process's memory: the ranges the kernel wrote for this
    /// process (a blocked call completing), which the fork's restore keeps.
    journaling: std::sync::atomic::AtomicBool,
    journal: parking_lot::Mutex<Vec<(u64, usize)>>,
    /// Set while this process is one side of a live fork pair. `paired` is the whole cost on the
    /// copy path for every process that is not: one relaxed load.
    paired: std::sync::atomic::AtomicBool,
    resident: parking_lot::Mutex<Option<Arc<dyn Resident>>>,
}

impl GuestMem {
    #[must_use]
    pub fn new(space: Arc<GuestSpace>, layout: Layout) -> Self {
        Self {
            space,
            layout,
            remote: std::sync::OnceLock::new(),
            journaling: std::sync::atomic::AtomicBool::new(false),
            journal: parking_lot::Mutex::default(),
            paired: std::sync::atomic::AtomicBool::new(false),
            resident: parking_lot::Mutex::default(),
        }
    }

    /// This view is one side of a live fork pair: every copy waits for its turn in the memory.
    pub(crate) fn share_with(&self, side: Arc<dyn Resident>) {
        *self.resident.lock() = Some(side);
        self.paired.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The pair is over (the child executed a program or ended): copies are unconditional again.
    pub(crate) fn unshare(&self) {
        self.paired.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.resident.lock() = None;
    }

    /// Wait until this side's view is the one in the address space. Called before a copy takes the
    /// layout lock -- never while holding it, which is what a switch takes exclusively.
    pub(crate) fn ensure_resident(&self) {
        if !self.paired.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let side = self.resident.lock().clone();
        if let Some(side) = side {
            side.ensure();
        }
    }

    /// Make this a stand-in's memory: every access goes to `remote`.
    pub fn set_remote(&self, remote: Arc<dyn Remote>) {
        let _ = self.remote.set(remote);
    }

    /// Start recording the ranges written through this view.
    pub fn start_journal(&self) {
        self.journal.lock().clear();
        self.journaling.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stop recording; the ranges written since `start_journal`.
    pub fn take_journal(&self) -> Vec<(u64, usize)> {
        self.journaling.store(false, std::sync::atomic::Ordering::SeqCst);
        std::mem::take(&mut *self.journal.lock())
    }

    fn note(&self, addr: u64, len: usize) {
        if self.journaling.load(std::sync::atomic::Ordering::Relaxed) {
            self.journal.lock().push((untag(addr), len));
        }
    }

    /// `read`, for a caller that holds the layout lock exclusively.
    pub(crate) fn read_holding_layout(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let start = self.check(addr, len, false)?;
        let mut out = vec![0u8; len];
        self.copy_out(start, &mut out);
        Ok(out)
    }

    #[must_use]
    pub fn space(&self) -> &Arc<GuestSpace> {
        &self.space
    }

    /// The layout lock (shared by a vfork child with its parent).
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn check(&self, addr: u64, len: usize, write: bool) -> Result<usize, Errno> {
        if len == 0 {
            return Ok(0);
        }
        let start = usize::try_from(untag(addr)).map_err(|_| EFAULT)?;
        let end = start.checked_add(len).ok_or(EFAULT)?;
        if !self.space.contains(start, len) {
            return Err(EFAULT);
        }
        let mut at = start;
        // Whether any of it is still a lazy mapping's uncommitted placeholder. Almost never: an
        // anonymous region is committed whole or not at all, and a file view is mapped whole. So
        // the common access takes no lock -- `region_at` answers from this thread's cache -- and
        // `ensure_committed` (the space's lock) is left for the rare first touch.
        let mut commit = false;
        while at < end {
            let region = self.space.region_at(at).ok_or(EFAULT)?;
            commit |= matches!(region.kind, omni_mem::RegionKind::Anonymous) && region.committed < region.len;
            if region.mapping.is_none() {
                return Err(EFAULT);
            }
            let ok = match region.protection {
                Protection::None => false,
                Protection::ReadWrite | Protection::ReadWriteExecute => true,
                Protection::Read | Protection::ReadExecute => !write,
            };
            if !ok {
                return Err(EFAULT);
            }
            at = region.start + region.len;
        }
        if commit {
            self.space.ensure_committed(start, len).map_err(|_| EFAULT)?;
        }
        self.space.ptr(start, len).map_err(|_| EFAULT)?;
        Ok(start)
    }

    /// Copy the checked range `[start, start + out.len())` out, piece by piece where it crosses a
    /// host page the 4 KiB overlay traps (its alias), in one copy otherwise (D42).
    fn copy_out(&self, start: usize, out: &mut [u8]) {
        self.space.for_each_access_chunk(start, out.len(), |g, p, n| {
            let at = g - start;
            // SAFETY: `check` proved the range mapped, readable and committed; each piece's pointer
            // is its host address or its page's read-write alias.
            unsafe { std::ptr::copy_nonoverlapping(p, out[at..at + n].as_mut_ptr(), n) };
        });
    }

    /// Copy `bytes` into the checked range at `start`, piece by piece as [`copy_out`](Self::copy_out).
    fn copy_in(&self, start: usize, bytes: &[u8]) {
        self.space.for_each_access_chunk(start, bytes.len(), |g, p, n| {
            let at = g - start;
            // SAFETY: `check` proved the range mapped, writable and committed.
            unsafe { std::ptr::copy_nonoverlapping(bytes[at..at + n].as_ptr(), p, n) };
        });
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno> {
        if let Some(r) = self.remote.get() {
            return r.read(untag(addr), len);
        }
        // Before the layout lock, never while holding it: a switch takes it exclusively.
        self.ensure_resident();
        let _layout = self.layout.read();
        let start = self.check(addr, len, false)?;
        let mut out = vec![0u8; len];
        self.copy_out(start, &mut out);
        Ok(out)
    }

    /// [`read`](Self::read) into the caller's buffer: the same checks, no allocation (a forwarded
    /// GPU command reads its request and arguments this way, thousands of times a frame).
    pub fn read_into(&self, addr: u64, out: &mut [u8]) -> Result<(), Errno> {
        if let Some(r) = self.remote.get() {
            let bytes = r.read(untag(addr), out.len())?;
            if bytes.len() != out.len() {
                return Err(EFAULT);
            }
            out.copy_from_slice(&bytes);
            return Ok(());
        }
        self.ensure_resident();
        let _layout = self.layout.read();
        let start = self.check(addr, out.len(), false)?;
        self.copy_out(start, out);
        Ok(())
    }

    pub fn write(&self, addr: u64, bytes: &[u8]) -> Result<(), Errno> {
        // Before the layout lock, as `read`: this is what stops a call that completes for the side
        // whose view is shelved from writing its answer into the other side's memory.
        self.ensure_resident();
        let _layout = self.layout.read();
        self.write_holding_layout(addr, bytes)
    }

    /// `write`, for a caller that already holds the layout lock exclusively (`mmap` filling the
    /// private copy it just made).
    pub(crate) fn write_holding_layout(&self, addr: u64, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(r) = self.remote.get() {
            return r.write(untag(addr), bytes);
        }
        let start = self.check(addr, bytes.len(), true)?;
        if !bytes.is_empty() {
            self.note(addr, bytes.len());
            self.copy_in(start, bytes);
        }
        Ok(())
    }

    /// The aligned, writable guest word at `addr`, for an atomic update the kernel makes
    /// (`FUTEX_WAKE_OP`).
    pub fn atomic_u32(&self, addr: u64) -> Result<&std::sync::atomic::AtomicU32, Errno> {
        if untag(addr) % 4 != 0 {
            return Err(crate::errno::EINVAL);
        }
        self.ensure_resident();
        let start = self.check(addr, 4, true)?;
        // Four aligned bytes are one host page: its address, or its alias if the page traps (D42).
        let ptr = match self.space.access_ptr(start, 4) {
            omni_mem::AccessPtr::Direct(p) | omni_mem::AccessPtr::Alias(p) => p,
            omni_mem::AccessPtr::Straddle => return Err(EFAULT),
        };
        self.note(addr, 4);
        // SAFETY: `check` proved the four bytes mapped, writable and committed; they are aligned;
        // guest memory outlives `self`, and every access to it is atomic or byte-wise.
        Ok(unsafe { &*ptr.cast::<std::sync::atomic::AtomicU32>() })
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, Errno> {
        Ok(u64::from_le_bytes(self.read(addr, 8)?.try_into().expect("eight bytes")))
    }

    pub fn write_u64(&self, addr: u64, value: u64) -> Result<(), Errno> {
        self.write(addr, &value.to_le_bytes())
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, Errno> {
        Ok(u32::from_le_bytes(self.read(addr, 4)?.try_into().expect("four bytes")))
    }

    pub fn write_u32(&self, addr: u64, value: u32) -> Result<(), Errno> {
        self.write(addr, &value.to_le_bytes())
    }

    /// A NUL-terminated string of at most `max` bytes (excluding the NUL); longer is `ENAMETOOLONG`.
    pub fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let mut out = Vec::new();
        let mut at = addr;
        loop {
            // Read up to the end of the page, so a string ending just before an unmapped page works.
            let page_left = 4096 - (at as usize & 4095);
            let chunk = self.read(at, page_left)?;
            if let Some(nul) = chunk.iter().position(|&b| b == 0) {
                out.extend_from_slice(&chunk[..nul]);
                return Ok(out);
            }
            out.extend_from_slice(&chunk);
            if out.len() > max {
                return Err(crate::errno::ENAMETOOLONG);
            }
            at += page_left as u64;
        }
    }
}

#[cfg(test)]
mod timing_probe {
    use super::*;

    #[test]
    #[ignore]
    fn where_a_checked_copy_goes() {
        let space = Arc::new(GuestSpace::with_config(omni_mem::GuestSpaceConfig { guest_page: Some(omni_mem::GUEST_PAGE), ..Default::default() }).unwrap());
        let at = space.map_anonymous(omni_mem::Placement::Anywhere { align: space.page_size() }, 1 << 20, Protection::ReadWrite, omni_mem::CommitPolicy::Lazy).unwrap() as u64;
        let mem = GuestMem::new(Arc::clone(&space), Layout::default());
        mem.write(at, &vec![0u8; 256 * 1024]).unwrap();
        const N: u32 = 1_000_000;
        let time = |name: &str, f: &mut dyn FnMut()| {
            let t0 = std::time::Instant::now();
            for _ in 0..N {
                f();
            }
            eprintln!("[probe] {name}: {:.1} ns", t0.elapsed().as_nanos() as f64 / f64::from(N));
        };
        let mut buf = [0u8; 32];
        time("read_into", &mut || mem.read_into(at, &mut buf).unwrap());
        time("read (Vec)", &mut || drop(std::hint::black_box(mem.read(at, 32).unwrap())));
        time("layout.read", &mut || drop(std::hint::black_box(mem.layout.read())));
        time("check", &mut || {
            std::hint::black_box(mem.check(at, 32, false).unwrap());
        });
        time("region_at", &mut || drop(std::hint::black_box(space.region_at(at as usize))));
        time("ensure_committed", &mut || drop(std::hint::black_box(space.ensure_committed(at as usize, 32))));
        time("space.ptr", &mut || drop(std::hint::black_box(space.ptr(at as usize, 32))));
        time("copy_out", &mut || mem.copy_out(at as usize, &mut buf));
        let r = space.region_at(at as usize).unwrap();
        eprintln!("[probe] region committed {} of {}", r.committed, r.len);
    }
}
