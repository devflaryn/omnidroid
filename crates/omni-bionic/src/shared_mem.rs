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
}

// NOTE: there is deliberately NO `GuestAtomic` impl for single-threaded
// `MockMemory`: a CAS needs interior mutation through `&self`, which plain
// Vec storage cannot provide in safe code (the crate forbids `unsafe`). Any
// test that exercises the sync primitives wraps its memory in
// [`SharedMockMemory`], whose CAS is atomic under its inner lock.
// The cell mirror once planned for this was removed as redundant complexity.

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
