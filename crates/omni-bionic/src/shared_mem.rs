//! A thread-shared [`MockMemory`] handle for concurrency tests.
//!
//! ## Why this exists
//!
//! The blocking primitives (`lock`, `cond_wait`, `sem_wait`…) take
//! `&mut impl GuestMemory` for the whole call, and while blocked on the futex they
//! still hold that `&mut`. If a test wraps one `MockMemory` in a host
//! `Mutex<MockMemory>` and each thread locks that host mutex around the whole
//! primitive call, a waiter holds the host memory lock while it sleeps — and the
//! releasing thread can never write the release. Every cross-thread handover test
//! would deadlock on the *harness*, not the primitive.
//!
//! The real adapter never has this problem: guest memory is true shared memory,
//! and holding "a `&mut` handle" is a borrow-checker fiction, not a hardware lock.
//! This wrapper restores the real semantics in the mock: the host lock is taken
//! only for the duration of each individual byte read/write, never across a futex
//! wait. Threads each own a lightweight clone of the shared handle.
//!
//! `GuestMemory::write` requires `&mut self`; here `&mut self` is satisfied by the
//! thread's own handle and does NOT imply exclusive access to the underlying
//! memory — that is the entire point, and it matches how the adapter's memory will
//! behave (shared hardware under the crate's protocol-level atomicity).

use crate::atomics::GuestAtomic;
use crate::memory::{Fault, GuestMemory};
use crate::mock::MockMemory;
use std::sync::{Arc, Mutex};

/// The shared inner memory.
struct Inner {
    mem: Mutex<MockMemory>,
}

/// A cloneable handle to one shared mock address space.
///
/// Every thread in a concurrency test holds its own `SharedMockMemory` (an
/// `Arc` clone). Reads and writes take the inner host mutex only for the single
/// access; blocking waits release it.
#[derive(Clone)]
pub struct SharedMockMemory {
    inner: Arc<Inner>,
}

impl SharedMockMemory {
    /// Wrap a `MockMemory` into a shared handle. `mem` must be fully mapped
    /// before sharing (all tests map their regions up front).
    pub fn new(mem: MockMemory) -> Self {
        SharedMockMemory { inner: Arc::new(Inner { mem: Mutex::new(mem) }) }
    }

    /// Run a closure with exclusive access to the whole memory — for *setup and
    /// assertions only* (mapping regions, reading final state). Never call this
    /// while another thread is inside a blocking primitive.
    pub fn with_exclusive<R>(&self, f: impl FnOnce(&mut MockMemory) -> R) -> R {
        let mut guard = self.inner.mem.lock().unwrap();
        f(&mut guard)
    }
}

impl GuestMemory for SharedMockMemory {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.inner.mem.lock().unwrap().read(addr, buf)
    }

    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        self.inner.mem.lock().unwrap().write(addr, buf)
    }
}

impl GuestAtomic for SharedMockMemory {
    /// Atomic because the whole read-compare-write happens under one hold of
    /// the inner host lock — exactly the guarantee the sync primitives need.
    fn cas_u32(&self, addr: u64, expect: u32, new: u32) -> Result<bool, Fault> {
        let mut mem = self.inner.mem.lock().unwrap();
        let mut b = [0u8; 4];
        mem.read(addr, &mut b)?;
        if u32::from_le_bytes(b) == expect {
            mem.write(addr, &new.to_le_bytes())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Ordered because the inner host lock is: a load under it sees every store
    /// made under it before, which is an acquire and more.
    fn load_u32_acquire(&self, addr: u64) -> Result<u32, Fault> {
        let mem = self.inner.mem.lock().unwrap();
        let mut b = [0u8; 4];
        mem.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// Ordered and untorn for the same reason: one hold of the inner lock.
    fn store_u32_release(&self, addr: u64, value: u32) -> Result<(), Fault> {
        self.inner.mem.lock().unwrap().write(addr, &value.to_le_bytes())
    }
}

// NOTE: there is deliberately NO `GuestAtomic` impl for single-threaded
// `MockMemory`: a CAS needs interior mutation through `&self`, which plain
// Vec storage cannot provide in safe code (the crate forbids `unsafe`). Any
// test that exercises the sync primitives wraps its memory in
// [`SharedMockMemory`], whose CAS is atomic under its inner lock.
// The cell mirror once planned for this was removed as redundant complexity.
// The same holds for `store_u32_release`, which writes through `&self` as the
// CAS does.

/// How one access touched a watched guest word. See [`RecordingMemory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordAccess {
    /// A plain [`GuestMemory::read`] that covered a byte of the word: no ordering.
    Read,
    /// A plain [`GuestMemory::write`] that covered a byte of the word, with the
    /// value the word held after it: no ordering, and not even one store.
    Write(u32),
    /// [`GuestAtomic::load_u32_acquire`], with the value it returned.
    LoadAcquire(u32),
    /// [`GuestAtomic::store_u32_release`], with the value it stored.
    StoreRelease(u32),
    /// [`GuestAtomic::cas_u32`] to `new`; `swapped` says whether it wrote.
    Cas {
        /// The value the CAS would write.
        new: u32,
        /// Whether it did.
        swapped: bool,
    },
}

/// A [`SharedMockMemory`] that records **which primitive** touched one watched
/// 32-bit word, in order.
///
/// # Why a behavioural test cannot do this job
///
/// The mock's inner host lock orders everything, so a primitive that publishes its
/// word with a plain `write` behaves exactly like one that uses a release store,
/// here and on every x86-64 host. The difference exists only on a weakly ordered
/// host, in a window no unit test can make appear on demand. What a test *can* pin
/// is the choice of primitive: a regression back to `write` shows up in this log as
/// a [`WordAccess::Write`], and fails.
#[derive(Clone)]
pub struct RecordingMemory {
    mem: SharedMockMemory,
    word: u64,
    log: Arc<Mutex<Vec<WordAccess>>>,
}

impl RecordingMemory {
    /// Record every access to the four bytes at `word`, over `mem`.
    pub fn new(mem: SharedMockMemory, word: u64) -> Self {
        Self { mem, word, log: Arc::new(Mutex::new(Vec::new())) }
    }

    /// The accesses so far, oldest first.
    pub fn log(&self) -> Vec<WordAccess> {
        self.log.lock().unwrap().clone()
    }

    /// Forget the accesses so far (for a test that only wants one call's).
    pub fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    /// The underlying shared memory.
    pub fn shared(&self) -> &SharedMockMemory {
        &self.mem
    }

    fn covers(&self, addr: u64, len: usize) -> bool {
        len != 0 && addr < self.word + 4 && self.word < addr.saturating_add(len as u64)
    }

    fn push(&self, access: WordAccess) {
        self.log.lock().unwrap().push(access);
    }

    fn word_now(&self) -> u32 {
        self.mem.with_exclusive(|m| {
            let mut b = [0u8; 4];
            m.read(self.word, &mut b).map_or(0, |()| u32::from_le_bytes(b))
        })
    }
}

impl GuestMemory for RecordingMemory {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        if self.covers(addr, buf.len()) {
            self.push(WordAccess::Read);
        }
        self.mem.read(addr, buf)
    }

    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        let result = self.mem.write(addr, buf);
        if self.covers(addr, buf.len()) {
            let now = self.word_now();
            self.push(WordAccess::Write(now));
        }
        result
    }
}

impl GuestAtomic for RecordingMemory {
    fn cas_u32(&self, addr: u64, expect: u32, new: u32) -> Result<bool, Fault> {
        let swapped = self.mem.cas_u32(addr, expect, new)?;
        if self.covers(addr, 4) {
            self.push(WordAccess::Cas { new, swapped });
        }
        Ok(swapped)
    }

    fn load_u32_acquire(&self, addr: u64) -> Result<u32, Fault> {
        let value = self.mem.load_u32_acquire(addr)?;
        if self.covers(addr, 4) {
            self.push(WordAccess::LoadAcquire(value));
        }
        Ok(value)
    }

    fn store_u32_release(&self, addr: u64, value: u32) -> Result<(), Fault> {
        self.mem.store_u32_release(addr, value)?;
        if self.covers(addr, 4) {
            self.push(WordAccess::StoreRelease(value));
        }
        Ok(())
    }
}

/// A futex that **compares its word**, as Linux's `FUTEX_WAIT` does and as the embedding's
/// does: a word that differs from `expected` is `WouldBlock`, and each refusal is counted.
///
/// Over [`MockFutex`](crate::mock_threads::MockFutex)'s queue, so the comparison is not atomic
/// with the park. What it is for is the count: a primitive that hands its wait the wrong word
/// spins against a futex that compares, and shows up here as refusals by the hundred thousand.
pub struct ComparingFutex {
    mem: SharedMockMemory,
    inner: crate::mock_threads::MockFutex,
    refused: std::sync::atomic::AtomicU64,
}

impl ComparingFutex {
    /// A comparing futex over `mem`'s words.
    pub fn new(mem: SharedMockMemory) -> Self {
        Self {
            mem,
            inner: crate::mock_threads::MockFutex::new(),
            refused: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// How many waits found the word no longer holding `expected`.
    pub fn refused(&self) -> u64 {
        self.refused.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl crate::threads::Futex for ComparingFutex {
    fn wait(
        &self,
        addr: u64,
        expected: u32,
        timeout: Option<core::time::Duration>,
    ) -> crate::threads::WaitResult {
        let mut word = [0u8; 4];
        let held = self.mem.read(addr, &mut word).is_ok() && u32::from_le_bytes(word) == expected;
        if !held {
            self.refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return crate::threads::WaitResult::WouldBlock;
        }
        self.inner.wait(addr, expected, timeout)
    }

    fn wake(&self, addr: u64, count: u32) -> u32 {
        self.inner.wake(addr, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two threads can write the shared memory concurrently without the harness
    /// serialising whole primitive calls: one thread sleeps inside its *test
    /// logic* while another writes — impossible with a whole-call host lock.
    #[test]
    fn concurrent_access_is_possible() {
        let mut base = MockMemory::new();
        base.map(0x1000, &[0u8; 64]);
        let mem = SharedMockMemory::new(base);

        let m2 = mem.clone();
        let writer = std::thread::spawn(move || {
            let mut m = m2.clone();
            m.write(0x1010, &[1, 2, 3]).unwrap();
        });
        // Main thread writes a disjoint range while the writer runs.
        let mut m = mem.clone();
        m.write(0x1020, &[9]).unwrap();
        writer.join().unwrap();

        mem.with_exclusive(|g| {
            let mut a = [0u8; 3];
            g.read(0x1010, &mut a).unwrap();
            assert_eq!(a, [1, 2, 3]);
            let mut b = [0u8; 1];
            g.read(0x1020, &mut b).unwrap();
            assert_eq!(b, [9]);
        });
    }

    /// The ordered load and store are the same word the CAS and plain reads see, and fault
    /// where a plain access would.
    #[test]
    fn ordered_load_and_store_round_trip() {
        let mut base = MockMemory::new();
        base.map(0x3000, &[0u8; 8]);
        let mem = SharedMockMemory::new(base);
        mem.store_u32_release(0x3004, 0x0102_0304).unwrap();
        assert_eq!(mem.load_u32_acquire(0x3004).unwrap(), 0x0102_0304);
        assert!(mem.cas_u32(0x3004, 0x0102_0304, 9).unwrap());
        assert_eq!(crate::atomics::read_u32(&mem, 0x3004).unwrap(), 9);
        assert_eq!(mem.load_u32_acquire(0x3006), Err(Fault(0x3008)), "straddles the end");
        assert_eq!(mem.store_u32_release(0x9000, 1), Err(Fault(0x9000)));
    }

    /// Clones see each other's writes (true sharing, not copy-on-write).
    #[test]
    fn clones_share_state() {
        let mut base = MockMemory::new();
        base.map(0x2000, &[0u8; 8]);
        let mem = SharedMockMemory::new(base);
        let mut m2 = mem.clone();
        m2.write(0x2000, &[7]).unwrap();
        let mut local = [0u8; 1];
        mem.read(0x2000, &mut local).unwrap();
        assert_eq!(local, [7]);
    }
}
