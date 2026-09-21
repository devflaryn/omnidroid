//! Pinned guest buffers: what `GetStringUTFChars` and `Get<Type>ArrayElements` hand back.
//!
//! # Why these need real guest memory and a table of their own
//!
//! Most JNI calls hand the guest an opaque handle it never dereferences. Four families do not:
//! `GetStringUTFChars` (27 sites), `GetStringChars` (1), `GetByteArrayElements` /
//! `GetIntArrayElements` / `GetFloatArrayElements` (7 between them) each return a **pointer the
//! guest reads through** — and, for the array forms, may write through before releasing. So the
//! bytes have to be in mapped, readable, writable guest memory, and the release call has to be
//! able to find them again.
//!
//! Every one of these is returned with `isCopy = JNI_TRUE`. That is not a simplification: this
//! layer's Java objects are host-side values with no guest representation, so a copy is the only
//! honest answer, and JNI's contract is written so that a caller which respects `isCopy` is
//! correct either way.
//!
//! # `Release` is what makes a write visible, and the mode says whether it happens
//!
//! `Release<Type>ArrayElements(env, array, elems, mode)`:
//!
//! | mode | what it means | what this does |
//! |---|---|---|
//! | `0` | copy back and free | reads the buffer into the object, frees the buffer |
//! | [`JNI_COMMIT`](super::slots::JNI_COMMIT) | copy back, keep the pointer valid | reads it back, keeps the buffer |
//! | [`JNI_ABORT`](super::slots::JNI_ABORT) | discard, free | frees without reading |
//!
//! Getting `JNI_ABORT` wrong by copying anyway is the silent kind: the guest asked for its
//! changes to be **discarded**, and a copy-back would apply them.
//!
//! # A pointer the guest hands back is untrusted, like every other
//!
//! `Release…` takes the pointer the guest was given. A value that is not the *start* of a live
//! allocation is a typed error naming the function — not a no-op, because a `Release` on the
//! wrong pointer means the guest lost track of the buffer and the next read through it is
//! whatever else the pool put there.

use std::collections::BTreeMap;

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};

use crate::error::{AbiError, AbiResult};

use super::refs::ObjectId;

/// Bytes of guest address space the pinned pool reserves.
///
/// **A policy number, and a reservation rather than a commitment.** D10 measures a reservation at
/// **0.000 MiB** of commit charge, so the size costs nothing until something is pinned; the pages
/// that are touched are charged and the rest are not, which is why this is generous rather than
/// tight. What bounds the *charge* is [`MAX_PINNED_BYTES`].
pub const POOL_BYTES: usize = 4 * 1024 * 1024;

/// How many bytes may be pinned at once before a pin is refused by name.
///
/// A guest that takes `GetStringUTFChars` in a loop and never releases would otherwise commit the
/// whole pool. The refusal names the function and the total, which is a fact about the guest;
/// silently reusing a live buffer would be a fact about nothing.
pub const MAX_PINNED_BYTES: usize = POOL_BYTES;

/// Alignment every pinned buffer gets. Eight bytes, so a `jlong` or a `jdouble` array is aligned
/// for the guest's own loads.
pub const PIN_ALIGN: usize = 8;

/// What a pinned buffer is a copy of, so `Release` knows how to put it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinKind {
    /// Modified UTF-8 bytes of a string, NUL-terminated. Never copied back: JNI strings are
    /// immutable and `ReleaseStringUTFChars` takes no mode.
    StringUtf8,
    /// UTF-16 code units of a string. Also never copied back.
    StringChars,
    /// `jbyte[]`.
    ByteArray,
    /// `jint[]`.
    IntArray,
    /// `jlong[]`.
    LongArray,
    /// `jfloat[]`.
    FloatArray,
}

impl PinKind {
    /// Whether `Release` copies the buffer back into the object.
    #[must_use]
    pub fn writes_back(self) -> bool {
        !matches!(self, PinKind::StringUtf8 | PinKind::StringChars)
    }

    /// What an error message calls it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PinKind::StringUtf8 => "a modified-UTF-8 string",
            PinKind::StringChars => "a UTF-16 string",
            PinKind::ByteArray => "a byte[]",
            PinKind::IntArray => "an int[]",
            PinKind::LongArray => "a long[]",
            PinKind::FloatArray => "a float[]",
        }
    }
}

/// One live pinned buffer.
#[derive(Debug, Clone, Copy)]
pub struct Pinned {
    /// Where it is in guest memory.
    pub address: GuestAddr,
    /// How many bytes it holds, terminator included where there is one.
    pub len: usize,
    /// What it is a copy of.
    pub kind: PinKind,
    /// The object it came from, so `Release` can write it back.
    pub owner: ObjectId,
}

/// The pinned pool: a reserved range plus a first-fit free list.
#[derive(Debug)]
pub struct Pool {
    space: std::sync::Arc<GuestSpace>,
    base: GuestAddr,
    bytes: usize,
    /// Free runs, kept sorted by address and coalesced on release.
    free: Vec<(GuestAddr, usize)>,
    live: BTreeMap<GuestAddr, Pinned>,
    pinned_bytes: usize,
    peak_pinned: usize,
}

impl Pool {
    /// Reserve the pool.
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the address space could not supply it.
    pub fn reserve(space: std::sync::Arc<GuestSpace>) -> AbiResult<Self> {
        let page = space.page_size();
        let bytes = (POOL_BYTES + page - 1) & !(page - 1);
        // **Lazily committed.** Global Constraint 6 and D10: nothing is pinned until the engine
        // asks, so the reservation must cost no commit charge. The pages that are written are
        // charged by the demand pager when they are.
        let base = space.map_anonymous(
            Placement::Anywhere { align: page },
            bytes,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )?;
        Ok(Self {
            space,
            base,
            bytes,
            free: vec![(base, bytes)],
            live: BTreeMap::new(),
            pinned_bytes: 0,
            peak_pinned: 0,
        })
    }

    /// The first address of the pool.
    #[must_use]
    pub fn base(&self) -> GuestAddr {
        self.base
    }

    /// How many bytes it reserves.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many bytes are pinned right now.
    #[must_use]
    pub fn pinned_bytes(&self) -> usize {
        self.pinned_bytes
    }

    /// The most that have been pinned at once.
    #[must_use]
    pub fn peak_pinned_bytes(&self) -> usize {
        self.peak_pinned
    }

    /// How many buffers are pinned right now.
    #[must_use]
    pub fn live_pins(&self) -> usize {
        self.live.len()
    }

    /// Every live pin, in address order — what a leak check reads.
    pub fn pins(&self) -> impl Iterator<Item = &Pinned> {
        self.live.values()
    }

    /// Copy `bytes` into the pool and record what it is.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the pool cannot hold it, naming `function` and the totals;
    /// [`AbiError::BadPointer`] if the write into the pool was refused, which would mean the
    /// address space had changed under it.
    pub fn pin(
        &mut self,
        function: &str,
        address: GuestAddr,
        mem: &crate::mem::GuestMem,
        kind: PinKind,
        owner: ObjectId,
        bytes: &[u8],
    ) -> AbiResult<GuestAddr> {
        let len = bytes.len().max(1);
        let need = (len + PIN_ALIGN - 1) & !(PIN_ALIGN - 1);
        if self.pinned_bytes + need > MAX_PINNED_BYTES {
            return Err(AbiError::JniRefused {
                function: function.to_string(),
                address,
                detail: format!(
                    "{} bytes are already pinned and this call asks for {need} more, past the \
                     {MAX_PINNED_BYTES}-byte cap: a caller that is not releasing what it pins \
                     gets this refusal rather than an unbounded commit charge",
                    self.pinned_bytes
                ),
            });
        }
        let at = self.take(need).ok_or_else(|| AbiError::JniRefused {
            function: function.to_string(),
            address,
            detail: format!(
                "the pinned pool is {} bytes and has no free run of {need}; {} bytes are pinned \
                 across {} buffers",
                self.bytes,
                self.pinned_bytes,
                self.live.len()
            ),
        })?;
        mem.write_bytes(at, bytes, crate::mem::Blame::new(function, address, 0))?;
        self.live.insert(at, Pinned { address: at, len: bytes.len(), kind, owner });
        self.pinned_bytes += need;
        self.peak_pinned = self.peak_pinned.max(self.pinned_bytes);
        Ok(at)
    }

    /// Look a live pin up by the pointer the guest hands back.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] naming the function, for a pointer that is not the start of a
    /// live pin.
    pub fn pinned(&self, function: &str, address: GuestAddr, at: GuestAddr) -> AbiResult<Pinned> {
        self.live.get(&at).copied().ok_or_else(|| AbiError::JniRefused {
            function: function.to_string(),
            address,
            detail: format!(
                "{at:#x} is not the start of a buffer this instance pinned; a release must be \
                 given exactly the pointer the matching get returned"
            ),
        })
    }

    /// Read a live pin's bytes back out of guest memory — what a copy-back does.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] for a pointer that is not a live pin; [`AbiError::BadPointer`] if
    /// the read was refused.
    pub fn read_back(
        &self,
        function: &str,
        address: GuestAddr,
        mem: &crate::mem::GuestMem,
        at: GuestAddr,
    ) -> AbiResult<(Pinned, Vec<u8>)> {
        let pin = self.pinned(function, address, at)?;
        let bytes =
            mem.read_bytes(pin.address, pin.len, crate::mem::Blame::new(function, address, 2))?;
        Ok((pin, bytes))
    }

    /// Release a live pin.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] for a pointer that is not a live pin.
    pub fn release(&mut self, function: &str, address: GuestAddr, at: GuestAddr) -> AbiResult<Pinned> {
        let pin = self.pinned(function, address, at)?;
        self.live.remove(&at);
        let need = (pin.len.max(1) + PIN_ALIGN - 1) & !(PIN_ALIGN - 1);
        self.pinned_bytes -= need;
        self.give_back(at, need);
        Ok(pin)
    }

    /// First fit.
    fn take(&mut self, need: usize) -> Option<GuestAddr> {
        let index = self.free.iter().position(|(_, len)| *len >= need)?;
        let (start, len) = self.free[index];
        if len == need {
            self.free.remove(index);
        } else {
            self.free[index] = (start + need, len - need);
        }
        Some(start)
    }

    /// Put a run back and coalesce with its neighbours, so a pin/release cycle does not fragment
    /// the pool into unusable pieces.
    fn give_back(&mut self, start: GuestAddr, len: usize) {
        let index = self.free.partition_point(|(at, _)| *at < start);
        self.free.insert(index, (start, len));
        // Merge forward, then backward. Two passes rather than one because merging with the
        // previous run changes which run is "this" one.
        if index + 1 < self.free.len() && self.free[index].0 + self.free[index].1 == self.free[index + 1].0
        {
            let (_, next_len) = self.free.remove(index + 1);
            self.free[index].1 += next_len;
        }
        if index > 0 && self.free[index - 1].0 + self.free[index - 1].1 == self.free[index].0 {
            let (_, this_len) = self.free.remove(index);
            self.free[index - 1].1 += this_len;
        }
    }

    /// How many free runs the pool is in. One, when nothing is pinned.
    #[must_use]
    pub fn free_runs(&self) -> usize {
        self.free.len()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // As `Bionic::drop`: the mapping is this instance's own and the space is about to go with
        // it, so a failure to give it back is not reportable and not worth aborting over.
        let _ = self.space.unmap(self.base, self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::GuestMem;
    use std::sync::Arc;

    fn pool() -> (Pool, GuestMem) {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let mem = GuestMem::new(Arc::clone(&space));
        (Pool::reserve(space).expect("a pool"), mem)
    }

    fn owner() -> ObjectId {
        // An id the pool never dereferences: it only carries it back to the caller.
        let mut handles = super::super::refs::Handles::new(1);
        let handle = handles
            .new_local("test", 0, super::super::refs::Object::ByteArray(vec![0; 4]))
            .expect("a reference");
        handles.resolve_id("test", 0, handle).expect("live")
    }

    #[test]
    fn a_pin_round_trips_through_guest_memory() {
        let (mut pool, mem) = pool();
        let at = pool
            .pin("GetStringUTFChars", 0x100, &mem, PinKind::StringUtf8, owner(), b"2.738.1397\0")
            .expect("pinned");
        let read = mem
            .read_bytes(at, 11, crate::mem::Blame::new("test", 0, 0))
            .expect("the pool is readable guest memory");
        assert_eq!(read, b"2.738.1397\0");
        assert_eq!(pool.live_pins(), 1);
        let pin = pool.release("ReleaseStringUTFChars", 0x100, at).expect("released");
        assert_eq!(pin.kind, PinKind::StringUtf8);
        assert_eq!(pool.live_pins(), 0);
        assert_eq!(pool.pinned_bytes(), 0);
    }

    /// **The hostile case.** A release given a pointer that is not a live pin must name itself
    /// rather than do nothing: a silent no-op leaves the buffer pinned and the guest reading
    /// through a pointer it believes it has given back.
    #[test]
    fn releasing_a_pointer_that_was_never_pinned_is_refused_by_name() {
        let (mut pool, mem) = pool();
        let at = pool
            .pin("GetByteArrayElements", 0x200, &mem, PinKind::ByteArray, owner(), &[1, 2, 3, 4])
            .expect("pinned");
        for wrong in [0, at + 1, at + 4096, u64::MAX as GuestAddr] {
            let error =
                pool.release("ReleaseByteArrayElements", 0x200, wrong).expect_err("not a pin");
            match error {
                AbiError::JniRefused { function, .. } => {
                    assert_eq!(function, "ReleaseByteArrayElements");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(pool.live_pins(), 1, "and the real pin is untouched");
        pool.release("ReleaseByteArrayElements", 0x200, at).expect("the right pointer works");
    }

    /// The design's own claim: a pin/release cycle must not fragment the pool. Ten thousand
    /// rounds through a pool of four megabytes would exhaust it if released runs were not
    /// coalesced.
    #[test]
    fn released_runs_coalesce_so_a_pin_release_cycle_does_not_fragment() {
        let (mut pool, mem) = pool();
        assert_eq!(pool.free_runs(), 1);
        for round in 0..10_000u32 {
            let bytes = round.to_le_bytes();
            let at = pool
                .pin("GetStringUTFChars", 0x300, &mem, PinKind::StringUtf8, owner(), &bytes)
                .expect("pinned");
            pool.release("ReleaseStringUTFChars", 0x300, at).expect("released");
        }
        assert_eq!(pool.free_runs(), 1, "the pool is one free run again");
        assert_eq!(pool.pinned_bytes(), 0);
        assert!(pool.peak_pinned_bytes() > 0, "something really was pinned");
    }

    /// A guest that pins without releasing hits the cap and gets a refusal naming the function,
    /// not an unbounded commit charge.
    #[test]
    fn pinning_past_the_cap_refuses_by_name() {
        let (mut pool, mem) = pool();
        let chunk = vec![0u8; 64 * 1024];
        let mut pinned = 0usize;
        loop {
            match pool.pin("GetStringUTFChars", 0x400, &mem, PinKind::StringUtf8, owner(), &chunk) {
                Ok(_) => pinned += 1,
                Err(AbiError::JniRefused { function, detail, .. }) => {
                    assert_eq!(function, "GetStringUTFChars");
                    assert!(detail.contains(&MAX_PINNED_BYTES.to_string()) || detail.contains("no free run"), "{detail}");
                    break;
                }
                Err(other) => panic!("{other:?}"),
            }
            assert!(pinned < 1024, "the cap must stop this");
        }
        assert!(pinned > 0);
    }

    /// Global Constraint 6: the pool is a reservation, so it costs no commit charge until
    /// something is pinned. Read out of the region list rather than a process counter, which two
    /// tests would measure of each other.
    #[test]
    fn an_empty_pool_costs_no_commit_charge() {
        let (pool, _mem) = pool();
        let info = pool.space.region_at(pool.base()).expect("the pool is mapped");
        assert_eq!(info.committed, 0);
    }

    /// Which kinds copy back. `ReleaseStringUTFChars` has no mode argument and a JNI string is
    /// immutable, so writing one back would apply whatever the guest scribbled over the copy.
    #[test]
    fn only_the_array_kinds_write_back() {
        assert!(!PinKind::StringUtf8.writes_back());
        assert!(!PinKind::StringChars.writes_back());
        for kind in [PinKind::ByteArray, PinKind::IntArray, PinKind::LongArray, PinKind::FloatArray]
        {
            assert!(kind.writes_back(), "{kind:?}");
        }
    }
}
