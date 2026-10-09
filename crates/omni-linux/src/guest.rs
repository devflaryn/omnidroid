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
    /// This side forks: block until its view is the one in the address space and keep it there
    /// until [`release_fork`](Self::release_fork) -- the fork child runs in it until it executes a
    /// program. False when the pair is over (nothing to keep).
    fn claim_fork(&self) -> bool;
    /// The fork claimed with [`claim_fork`](Self::claim_fork) is done with the memory.
    fn release_fork(&self);
    /// Which pair this is a side of (an identity to compare, nothing more).
    fn pair_key(&self) -> usize;
}

/// A fork's hold on its side's view of a time-shared memory ([`GuestMem::hold_for_fork`]), let go
/// when dropped.
pub struct ForkHold(Option<Arc<dyn Resident>>);

impl Drop for ForkHold {
    fn drop(&mut self) {
        if let Some(side) = self.0.take() {
            side.release_fork();
        }
    }
}

/// Whether the kernel's reads of guest memory leave a lazy mapping's uncommitted pages
/// uncommitted (reading them as zeros) -- [`GuestMem`]'s `checked_read`. On by default: the bytes
/// read are the same either way. `OMNI_READ_NO_COMMIT=0`, or the lever `read_no_commit=0`,
/// commits them first as before.
pub static READ_NO_COMMIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `OMNI_READ_NO_COMMIT=0`: [`READ_NO_COMMIT`] off from the start.
pub fn read_no_commit_from_env() {
    if std::env::var("OMNI_READ_NO_COMMIT").as_deref() == Ok("0") {
        READ_NO_COMMIT.store(false, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[lever] OMNI_READ_NO_COMMIT: read_no_commit=0");
    }
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
    /// Whether this view holds the remote direct-access gate shut (`crate::remote::gate`): while it
    /// journals or is one side of a fork pair, the system's host process must not read or write
    /// this memory itself -- the kernel's writes must be journaled, and a shelved side's must wait.
    gated: parking_lot::Mutex<bool>,
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
            gated: parking_lot::Mutex::new(false),
        }
    }

    /// Hold the remote direct-access gate shut (`exposed`) or let it go. Shut **before** a journal
    /// or a pair starts -- it waits out any direct access in flight -- and let go after both end.
    fn gate_to(&self, exposed: bool) {
        let mut gated = self.gated.lock();
        if *gated != exposed {
            *gated = exposed;
            crate::remote::gate(exposed);
        }
    }

    /// This view is one side of a live fork pair: every copy waits for its turn in the memory.
    pub(crate) fn share_with(&self, side: Arc<dyn Resident>) {
        self.gate_to(true);
        *self.resident.lock() = Some(side);
        self.paired.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Pair `key` is over: if this view is its side (and not another pair's -- a process that forks
    /// and whose child executes a program never shared its memory with that child, and must not
    /// lose the pair it is a side of), copies are unconditional again.
    pub(crate) fn unshare_from(&self, key: usize) {
        let ours = self.resident.lock().as_ref().is_some_and(|s| s.pair_key() == key);
        if ours {
            self.unshare();
        }
    }

    /// The pair is over (the child executed a program or ended): copies are unconditional again.
    pub(crate) fn unshare(&self) {
        self.paired.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.resident.lock() = None;
        self.gate_to(self.journaling.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Wait until this side's view is the one in the address space. Called before a copy takes the
    /// layout lock -- never while holding it, which is what a switch takes exclusively.
    /// A fork of this process is starting: when it is one side of a live fork pair, wait until its
    /// view is in the address space and keep it there while the fork's child runs in it (the
    /// pair's switches wait), until the hold is dropped.
    pub(crate) fn hold_for_fork(&self) -> ForkHold {
        if !self.paired.load(std::sync::atomic::Ordering::Relaxed) {
            return ForkHold(None);
        }
        let side = self.resident.lock().clone();
        ForkHold(side.filter(|s| s.claim_fork()))
    }

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
        self.gate_to(true);
        self.journal.lock().clear();
        self.journaling.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stop recording; the ranges written since `start_journal`.
    pub fn take_journal(&self) -> Vec<(u64, usize)> {
        let journal = self.take_journal_gated();
        self.journal_done();
        journal
    }

    /// [`take_journal`](Self::take_journal) with the remote direct-access gate **kept shut**, for
    /// a caller that goes on writing this memory back (a fork's restore of the parent): a direct
    /// write from the system's host process (a binder reply) landing between the gate opening and
    /// the write-back was overwritten by it -- unjournaled, so not kept. [`journal_done`] lets it go.
    ///
    /// [`journal_done`]: Self::journal_done
    pub(crate) fn take_journal_gated(&self) -> Vec<(u64, usize)> {
        self.journaling.store(false, std::sync::atomic::Ordering::SeqCst);
        std::mem::take(&mut *self.journal.lock())
    }

    /// The gate as the view's state says now (shut while it journals or is one side of a pair).
    pub(crate) fn journal_done(&self) {
        let shut = self.paired.load(std::sync::atomic::Ordering::SeqCst) || self.journaling.load(std::sync::atomic::Ordering::SeqCst);
        self.gate_to(shut);
    }

    fn note(&self, addr: u64, len: usize) {
        if self.journaling.load(std::sync::atomic::Ordering::Relaxed) {
            self.journal.lock().push((untag(addr), len));
        }
    }

    /// `read`, for a caller that holds the layout lock exclusively.
    pub(crate) fn read_holding_layout(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let mut out = vec![0u8; len];
        self.checked_read(addr, &mut out)?;
        Ok(out)
    }

    /// Check `[addr, addr + out.len())` readable and copy it out. Memory a lazy mapping has not
    /// committed yet is read as the zeros it holds, **without committing it** ([`READ_NO_COMMIT`]):
    /// committing it and copying from it made every such page a resident private page of zeros
    /// in the host process (Windows), where Linux reads it from the shared zero page.
    fn checked_read(&self, addr: u64, out: &mut [u8]) -> Result<(), Errno> {
        let (start, lazy) = self.scan(addr, out.len(), false)?;
        if out.is_empty() {
            return Ok(());
        }
        if lazy && READ_NO_COMMIT.load(std::sync::atomic::Ordering::Relaxed) {
            match self.space.read_uncommitted_as_zero(start, out) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(_) => return Err(EFAULT),
            }
        }
        if lazy {
            self.space.ensure_committed(start, out.len()).map_err(|_| EFAULT)?;
        }
        self.space.ptr(start, out.len()).map_err(|_| EFAULT)?;
        self.copy_out(start, out);
        Ok(())
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
        let (start, commit) = self.scan(addr, len, write)?;
        if len == 0 {
            return Ok(0);
        }
        if commit {
            self.space.ensure_committed(start, len).map_err(|_| EFAULT)?;
        }
        self.space.ptr(start, len).map_err(|_| EFAULT)?;
        Ok(start)
    }

    /// `check` without the commit: the start, and whether any of it is a lazy mapping's
    /// uncommitted placeholder.
    fn scan(&self, addr: u64, len: usize, write: bool) -> Result<(usize, bool), Errno> {
        if len == 0 {
            return Ok((0, false));
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
        Ok((start, commit))
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
            // Nothing to read is nothing to ask the app's host process for (a transaction's empty
            // offsets array, every one): the local path answers it so too, unchecked.
            if len == 0 {
                return Ok(Vec::new());
            }
            return r.read(untag(addr), len);
        }
        // Before the layout lock, never while holding it: a switch takes it exclusively.
        self.ensure_resident();
        let _layout = self.layout.read();
        let mut out = vec![0u8; len];
        self.checked_read(addr, &mut out)?;
        Ok(out)
    }

    /// [`read`](Self::read) into the caller's buffer: the same checks, no allocation (a forwarded
    /// GPU command reads its request and arguments this way, thousands of times a frame).
    pub fn read_into(&self, addr: u64, out: &mut [u8]) -> Result<(), Errno> {
        if let Some(r) = self.remote.get() {
            if out.is_empty() {
                return Ok(());
            }
            let bytes = r.read(untag(addr), out.len())?;
            if bytes.len() != out.len() {
                return Err(EFAULT);
            }
            out.copy_from_slice(&bytes);
            return Ok(());
        }
        self.ensure_resident();
        let _layout = self.layout.read();
        self.checked_read(addr, out)
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
            if bytes.is_empty() {
                return Ok(());
            }
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

    /// `N` bytes at `addr`, on the stack: [`read_into`](Self::read_into) without the caller's
    /// buffer. What a fixed-size argument (a word, a timespec, a msghdr) is read with -- a `read`'s
    /// allocation is over half of a small checked copy (`timing_probe`).
    pub fn read_array<const N: usize>(&self, addr: u64) -> Result<[u8; N], Errno> {
        let mut out = [0u8; N];
        self.read_into(addr, &mut out)?;
        Ok(out)
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, Errno> {
        self.read_array(addr).map(u64::from_le_bytes)
    }

    pub fn write_u64(&self, addr: u64, value: u64) -> Result<(), Errno> {
        self.write(addr, &value.to_le_bytes())
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, Errno> {
        self.read_array(addr).map(u32::from_le_bytes)
    }

    pub fn write_u32(&self, addr: u64, value: u32) -> Result<(), Errno> {
        self.write(addr, &value.to_le_bytes())
    }

    /// A NUL-terminated string of at most `max` bytes (excluding the NUL); longer is `ENAMETOOLONG`.
    pub fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let mut out = Vec::new();
        let mut at = addr;
        let mut buf = [0u8; 256];
        loop {
            // Never past the end of the page, so a string ending just before an unmapped page
            // works; 256 bytes at a time into the stack (a whole page was copied out before).
            let page_left = 4096 - (at as usize & 4095);
            let chunk = &mut buf[..page_left.min(256)];
            self.read_into(at, chunk)?;
            if let Some(nul) = chunk.iter().position(|&b| b == 0) {
                out.extend_from_slice(&chunk[..nul]);
                return Ok(out);
            }
            out.extend_from_slice(chunk);
            if out.len() > max {
                return Err(crate::errno::ENAMETOOLONG);
            }
            at += chunk.len() as u64;
        }
    }
}

/// The largest buffer [`with_scratch`] keeps per host thread.
pub const SCRATCH_MAX: usize = 4096;

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `f` on a buffer of `len` bytes: this host thread's own when `len` is at most [`SCRATCH_MAX`]
/// (kept from call to call, so a small `read`/`write`/`sendmsg`/`recvmsg` allocates nothing), a new
/// zeroed one otherwise. Its contents are what its last user left: callers fill what they use (a
/// copy in) or use only what was filled (a read's count). At most 4 KiB a thread (the system host
/// process's ~950 threads: ~4 MB at worst).
pub fn with_scratch<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    if len <= SCRATCH_MAX {
        // Taken out while `f` runs: a nested use (none today) gets an empty one and allocates.
        if let Some(mut v) = SCRATCH.with(|s| s.try_borrow_mut().ok().map(|mut v| std::mem::take(&mut *v))) {
            if v.len() < len {
                v.resize(len, 0);
            }
            let r = f(&mut v[..len]);
            SCRATCH.with(|s| {
                if let Ok(mut slot) = s.try_borrow_mut() {
                    if slot.capacity() < v.capacity() {
                        *slot = v;
                    }
                }
            });
            return r;
        }
    }
    // A large one: fresh pages that become resident only where they are written
    // (`crate::zbuf::ZeroBuf`), so a long wait in a 64 KiB `recvmsg` keeps no resident zeros.
    f(&mut crate::zbuf::ZeroBuf::new(len))
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    /// A fork's restore of the parent ends the journal and then writes the parent's memory back:
    /// the remote direct-access gate must stay shut until that is done (`crate::fork`'s
    /// `Snapshot::restore`), or a binder reply the system host writes directly in between is
    /// overwritten -- unjournaled, so not kept.
    #[test]
    fn the_gate_stays_shut_until_the_parents_memory_is_back() {
        let space = Arc::new(GuestSpace::with_config(omni_mem::GuestSpaceConfig { guest_page: Some(omni_mem::GUEST_PAGE), ..Default::default() }).unwrap());
        let mem = GuestMem::new(space, Layout::default());
        mem.start_journal();
        assert!(*mem.gated.lock(), "shut while the child runs in the memory");
        let _journal = mem.take_journal_gated();
        assert!(*mem.gated.lock(), "still shut while the parent's memory is written back");
        mem.journal_done();
        assert!(!*mem.gated.lock(), "open once it is back");
        // `take_journal` (no write-back after it) opens it at once, as before.
        mem.start_journal();
        let _ = mem.take_journal();
        assert!(!*mem.gated.lock());
    }
}

#[cfg(test)]
mod timing_probe {
    use super::*;

    /// Where a 32-byte checked copy's time goes (`--ignored --nocapture`). MEASURED on an E-core
    /// (i7-13700F, Windows): `read` 123 ns, `read_into` 56 ns -- the Vec's allocation and free are
    /// over half of `read` -- of which the layout lock 15, `check` 36 (`region_at` 14), the copy 6.
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
