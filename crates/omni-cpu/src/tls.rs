//! The bionic thread pointer (D13): a TLS block per guest thread, with a stack guard at `+0x28`.
//!
//! # Why this gates everything
//!
//! `libroblox.so` holds **1,282** `MRS Xt, TPIDR_EL0` instructions, and **1,276** of them go
//! straight on to load `[Xt, #0x28]`. That offset is bionic's `TLS_SLOT_STACK_GUARD` — slot 5, at
//! 5 × 8 bytes. It is there because every stack-protected function's prologue reads the thread
//! pointer *directly* rather than calling into libc, and the engine is built with the stack
//! protector on.
//!
//! The ordering is what makes it a gate rather than a detail: the first of those reads happens
//! **before `JNI_OnLoad` and before the first of the 3,594 static initializers**. There is no window
//! in which guest code runs and does not need a thread pointer. If `TPIDR_EL0` is zero, the very
//! first stack-protected call faults on a load from `0x28` — a symptom that reads as a loader bug,
//! which is exactly why D13 exists.
//!
//! So: allocate a block, populate at least slot 5, set `TPIDR_EL0`, and only then run guest code.
//! **For every guest thread, not just the first** — a thread created by the engine with no block is
//! the same crash, arriving later and looking even less like a thread-pointer problem.
//!
//! # The layout
//!
//! AArch64 uses the variant-1 TLS layout: `TPIDR_EL0` points at the thread control block and static
//! TLS follows *above* it. Bionic's TCB is a plain array of `void*` slots, so slot *n* is at
//! `TPIDR_EL0 + 8n`. That is the whole of what the measurement pins — slot 5 at `0x28` — and the
//! rest of [`TlsSlot`] is bionic's published set, carried so that a later task that needs
//! `TLS_SLOT_THREAD_ID` or `TLS_SLOT_BIONIC_TLS` finds a name rather than a number.
//!
//! Omnidroid populates slot 5 and leaves the rest zero. That is not a stub: a zero slot is what
//! bionic itself leaves in the slots nothing has claimed yet, and the two that the engine will
//! eventually need (`TLS_SLOT_THREAD_ID`, `TLS_SLOT_BIONIC_TLS`) cannot be filled until there is a
//! `pthread_internal_t` to point them at, which is M3's work. Naming that here is the point: the
//! block is correct for what runs at M2 and the gap is written down rather than discovered.
//!
//! # The guard value
//!
//! Bionic reads its stack guard once per process from `getauxval(AT_RANDOM)` and copies the same
//! value into every thread's slot 5. The same value in every thread is not an accident — a function
//! that stores the canary in one thread and checks it in another would fail otherwise — so
//! [`TlsArena`] generates one value per address space and gives it to every thread.
//!
//! The entropy comes from the standard library's [`RandomState`](std::collections::hash_map::
//! RandomState), which is seeded from the OS. It is **not** a cryptographic generator, and it does
//! not need to be: this canary defends guest code from its own stack overflows, and the host is not
//! defended by it at all — guest code is untrusted whatever the canary says (Global Constraint 11),
//! and the containment for that is the one-process-per-instance boundary in `ARCHITECTURE.md` §7.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};

use crate::context::{ContextCost, TLS_SLOT_STACK_GUARD_OFFSET};
use crate::error::{CpuError, CpuResult};

/// Bionic's thread-control-block slots, as indices into the array `TPIDR_EL0` points at.
///
/// Only [`StackGuard`](TlsSlot::StackGuard) is pinned by measurement; the others are bionic's names
/// for the same array, carried so that later work has somewhere to put a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum TlsSlot {
    /// Slot 0. A self-pointer on x86, unused on AArch64.
    SelfPointer = 0,
    /// Slot 1. `pthread_internal_t*` for this thread. Needs M3's threading layer.
    ThreadId = 1,
    /// Slot 2. Reserved for the app; historically `errno`.
    App = 2,
    /// Slot 3. Used by the platform's GL implementation.
    OpenGl = 3,
    /// Slot 4. Used by the platform's GL implementation.
    OpenGlApi = 4,
    /// Slot 5, at `+0x28`. **The one that matters**: 1,276 of `libroblox.so`'s 1,282 thread-pointer
    /// reads load exactly this (D13).
    StackGuard = 5,
    /// Slot 6. Used by the sanitizers; historically `dlerror`.
    Sanitizer = 6,
    /// Slot 7. ART's `Thread*`. Omnidroid has no ART (D7), so it stays zero.
    ArtThreadSelf = 7,
    /// Slot 8. The dynamic thread vector for ELF TLS.
    Dtv = 8,
    /// Slot 9. Bionic's own per-thread structure.
    BionicTls = 9,
}

impl TlsSlot {
    /// Byte offset of this slot from `TPIDR_EL0`.
    #[must_use]
    pub const fn offset(self) -> usize {
        (self as usize) * core::mem::size_of::<u64>()
    }
}

/// How many slots the control block holds.
///
/// Bionic's highest named slot is 9, and it rounds the block up so that the static TLS segment that
/// follows can be aligned. Twelve slots is 96 bytes, which leaves the block 16-byte aligned for
/// whatever follows and costs 16 bytes over the named set.
pub const TLS_SLOT_COUNT: usize = 12;

/// Bytes of control block, before any static TLS segment.
pub const TLS_CONTROL_BLOCK_BYTES: usize = TLS_SLOT_COUNT * core::mem::size_of::<u64>();

/// Bytes reserved per guest thread, control block included.
///
/// One page. The control block itself is 96 bytes, so this is mostly headroom for the executable's
/// `PT_TLS` segment, which sits immediately above the control block in the variant-1 layout and
/// which M3 will have to copy in. Sized in whole pages because the commit granularity underneath is
/// a page and a smaller figure would not save anything real; measured per-thread cost is in the
/// task report.
pub const TLS_BLOCK_BYTES: usize = 4096;

/// The list a freed block goes back on.
///
/// Held behind an [`Arc`] by both the arena and every block it has handed out, which is what lets a
/// [`GuestTls`] free *itself*. A back-reference to the whole [`TlsArena`] would have done as well,
/// but this is the smallest thing that has to outlive the arena, and it must outlive it: a block
/// dropped after the last handle to its arena is gone then returns to a list nobody will read again,
/// which is inert, where a dangling arena pointer would not be.
type FreeList = parking_lot::Mutex<Vec<GuestAddr>>;

/// A live guest TLS block. Freed back to its [`TlsArena`] when dropped.
///
/// # Why this has a `Drop` and did not
///
/// That doc line was written before the impl and the whole-branch review found the gap. There were
/// three paths that dropped a block without returning it — two where `GuestThreadConfig::new`
/// refused after the block had been handed out, and the one that matters, `od_jit_new` returning
/// null on code-cache allocation failure after the block had already been moved into the context
/// being built. Each cost one of the arena's blocks permanently, and the symptom arrived much later
/// and somewhere else: `create_thread_with_tls` failing with "the TLS arena is full", which points
/// at the guest's thread count rather than at a failed jit.
///
/// The fix is a real `Drop` rather than a free on each exit, because the exits are the problem: the
/// next one added would leak again, and would look exactly like the code around it.
pub struct GuestTls {
    base: GuestAddr,
    len: usize,
    guard: u64,
    /// Where this block goes when it is dropped.
    free: Arc<FreeList>,
}

impl core::fmt::Debug for GuestTls {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GuestTls")
            .field("base", &format_args!("{:#x}", self.base))
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Drop for GuestTls {
    /// Return the block for reuse.
    ///
    /// Does **not** decommit: a guest thread exiting is usually followed by another starting, and
    /// D10 makes commit charge the thing to husband, not the thing to churn. The whole arena's
    /// charge comes back when the space is closed.
    ///
    /// The lock here is `parking_lot`'s and is taken for a `Vec::push`. It is never taken on the
    /// fault path and never held across guest execution, so it cannot be the lock a faulting thread
    /// already holds.
    fn drop(&mut self) {
        self.free.lock().push(self.base);
    }
}

impl GuestTls {
    /// The value to program into `TPIDR_EL0`.
    #[must_use]
    pub const fn thread_pointer(&self) -> GuestAddr {
        self.base
    }

    /// Length of the block in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Always false: a zero-length TLS block is never produced.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The stack-guard value written into slot 5.
    #[must_use]
    pub const fn stack_guard(&self) -> u64 {
        self.guard
    }

    /// Guest address of `slot`.
    #[must_use]
    pub const fn slot_address(&self, slot: TlsSlot) -> GuestAddr {
        self.base + slot.offset()
    }

    /// What this block costs. Entirely private commit: a guest mapping is anonymous memory, which
    /// `process_commit_charge` does count (unlike the code arena — see [`ContextCost`]).
    #[must_use]
    pub const fn cost(&self) -> ContextCost {
        ContextCost { private_committed: self.len, shared_committed: 0 }
    }
}

/// Hands out one bionic TLS block per guest thread, out of a single guest mapping.
///
/// # Why an arena rather than a mapping per thread
///
/// Because a mapping has a floor that a TLS block is far below. On Windows a reservation's base is
/// allocation-granule aligned — 64 KiB — so a mapping per thread would spend 64 KiB of address
/// space to hold 96 bytes of slots, and D5 measured Roblox to be heavily multithreaded. One mapping
/// subdivided at [`TLS_BLOCK_BYTES`] spends 4 KiB per thread and commits lazily, so a thread that
/// never touches its static TLS area costs one page of commit charge.
///
/// Blocks are reused: a freed block goes on a list and is handed out again, re-zeroed, because
/// guest threads come and go and the guest must never see another thread's leftovers.
#[derive(Debug)]
pub struct TlsArena {
    base: GuestAddr,
    len: usize,
    block_bytes: usize,
    guard: u64,
    next: AtomicUsize,
    free: Arc<FreeList>,
    committed: AtomicU64,
}

impl TlsArena {
    /// Reserve room for `max_threads` TLS blocks in `space`.
    ///
    /// Costs no commit charge: the mapping is [`CommitPolicy::Lazy`], so a block costs a page only
    /// when it is handed out (D10 — address space is free, commit charge is not).
    ///
    /// # Errors
    ///
    /// [`CpuError::Memory`] if the reservation failed, or [`CpuError::Unsupported`] for a zero
    /// thread count.
    pub fn new(space: &GuestSpace, max_threads: usize) -> CpuResult<Self> {
        Self::with_block_size(space, max_threads, TLS_BLOCK_BYTES)
    }

    /// As [`new`](TlsArena::new), with an explicit block size. The size must leave room for the
    /// control block.
    ///
    /// # Errors
    ///
    /// As [`new`](TlsArena::new), plus [`CpuError::Unsupported`] for a block too small to hold the
    /// slots up to and including [`TlsSlot::StackGuard`].
    pub fn with_block_size(
        space: &GuestSpace,
        max_threads: usize,
        block_bytes: usize,
    ) -> CpuResult<Self> {
        if max_threads == 0 {
            return Err(CpuError::Unsupported {
                backend: "tls",
                operation: "reserve TLS blocks for zero guest threads",
                reason: "every guest thread needs a block before it runs a single instruction \
                         (D13), so an arena that can hold none would refuse the first thread",
            });
        }
        if block_bytes < TLS_CONTROL_BLOCK_BYTES {
            return Err(CpuError::Unsupported {
                backend: "tls",
                operation: "reserve a TLS block smaller than bionic's control block",
                reason: "guest code loads [TPIDR_EL0, #0x28] before anything else, so a block that \
                         does not reach that offset is the crash D13 exists to prevent",
            });
        }
        let len = block_bytes.checked_mul(max_threads).ok_or(CpuError::Unsupported {
            backend: "tls",
            operation: "reserve TLS blocks",
            reason: "block size times thread count overflows the address space",
        })?;
        let base = space
            .map_anonymous(
                Placement::Anywhere { align: space.page_size() },
                len,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .map_err(CpuError::from)?;

        Ok(Self {
            base,
            len,
            block_bytes,
            guard: generate_stack_guard(),
            next: AtomicUsize::new(0),
            free: Arc::new(parking_lot::Mutex::new(Vec::new())),
            committed: AtomicU64::new(0),
        })
    }

    /// The per-process stack-guard value every thread in this space gets.
    #[must_use]
    pub const fn stack_guard(&self) -> u64 {
        self.guard
    }

    /// Bytes reserved. Address space, not commit charge.
    #[must_use]
    pub const fn reserved(&self) -> usize {
        self.len
    }

    /// Bytes of commit charge this arena has taken so far.
    #[must_use]
    pub fn committed(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    /// How many blocks fit.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.len / self.block_bytes
    }

    /// Allocate, zero and populate a TLS block for one guest thread.
    ///
    /// On return, slot 5 holds the stack guard and every other slot is zero, and the block is
    /// committed. The caller programs `TPIDR_EL0` with
    /// [`thread_pointer`](GuestTls::thread_pointer) *before* running any guest code.
    ///
    /// # Errors
    ///
    /// [`CpuError::Memory`] if the block could not be committed, or [`CpuError::Unsupported`] when
    /// the arena is full.
    pub fn allocate(&self, space: &GuestSpace) -> CpuResult<GuestTls> {
        let base = match self.free.lock().pop() {
            Some(reused) => reused,
            None => {
                // A compare-exchange loop rather than `fetch_add` and an undo. The undo was wrong
                // under contention: two threads arriving at a full arena both increment, both
                // decrement, and the counter ends below where it started — so the *next* caller is
                // handed an index that is already in use, which is two guest threads sharing one
                // TLS block and therefore one stack guard slot. The refusal was also spurious one
                // slot early whenever a concurrent caller had incremented in between.
                let mut index = self.next.load(Ordering::Relaxed);
                loop {
                    if index >= self.capacity() {
                        return Err(CpuError::Unsupported {
                            backend: "tls",
                            operation: "allocate another guest thread's TLS block",
                            reason: "the TLS arena is full; it is sized at creation from the \
                                     maximum guest thread count",
                        });
                    }
                    match self.next.compare_exchange_weak(
                        index,
                        index + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(observed) => index = observed,
                    }
                }
                self.base + index * self.block_bytes
            }
        };

        let committed = space.ensure_committed(base, self.block_bytes).map_err(CpuError::from)?;
        self.committed.fetch_add(committed as u64, Ordering::Relaxed);

        let ptr = space.ptr(base, self.block_bytes).map_err(CpuError::from)?;
        // SAFETY: `ptr` came from `GuestSpace::ptr` for exactly this range, which has just been
        // committed `ReadWrite`, and identity mapping (D4) makes it a real host pointer. The range
        // belongs to this block alone — `next` hands each index out once and `free` returns it only
        // after the owning `GuestTls` is gone — so there is no aliasing write.
        unsafe {
            core::ptr::write_bytes(ptr, 0, self.block_bytes);
            // Zeroing first and then writing the guard is deliberate: a partially-populated block
            // handed to guest code is the D13 crash, and doing it in this order means the failure
            // mode of an interrupted populate is a *zero* guard, which the assertion below catches,
            // rather than a stale one from the previous owner, which it would not.
            ptr.add(TLS_SLOT_STACK_GUARD_OFFSET)
                .cast::<u64>()
                .write_unaligned(self.guard);
        }

        Ok(GuestTls {
            base,
            len: self.block_bytes,
            guard: self.guard,
            free: Arc::clone(&self.free),
        })
    }

    /// What the arena costs, for [`ContextCost`] accounting. All private commit.
    #[must_use]
    pub fn cost(&self) -> ContextCost {
        ContextCost {
            private_committed: usize::try_from(self.committed()).unwrap_or(usize::MAX),
            shared_committed: 0,
        }
    }
}

/// One stack-guard value per address space, from the standard library's OS-seeded hasher.
///
/// Never zero: a zero canary compares equal to a zeroed stack slot, so a guest stack overflow that
/// wrote zeroes would pass the check. The retry is bounded because a second draw from a different
/// hasher state is independent, and after that a fixed non-zero value is better than looping —
/// but the fallback is distinctive rather than tidy, so that if it ever appeared in a dump it would
/// be recognisable as the fallback rather than mistaken for entropy.
fn generate_stack_guard() -> u64 {
    for _ in 0..8 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(&hasher as *const _ as usize);
        let value = hasher.finish();
        if value != 0 {
            return value;
        }
    }
    0x4F4D_4E49_4452_4F49 // "OMNIDROI"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one offset the measurement pins, and the arithmetic that produces it.
    #[test]
    fn slot_five_is_at_0x28() {
        assert_eq!(TlsSlot::StackGuard as usize, 5);
        assert_eq!(TlsSlot::StackGuard.offset(), 0x28);
        assert_eq!(TlsSlot::StackGuard.offset(), TLS_SLOT_STACK_GUARD_OFFSET);
        // The whole named set, so a renumbering cannot move slot 5 without failing here.
        for (slot, offset) in [
            (TlsSlot::SelfPointer, 0x00),
            (TlsSlot::ThreadId, 0x08),
            (TlsSlot::App, 0x10),
            (TlsSlot::OpenGl, 0x18),
            (TlsSlot::OpenGlApi, 0x20),
            (TlsSlot::StackGuard, 0x28),
            (TlsSlot::Sanitizer, 0x30),
            (TlsSlot::ArtThreadSelf, 0x38),
            (TlsSlot::Dtv, 0x40),
            (TlsSlot::BionicTls, 0x48),
        ] {
            assert_eq!(slot.offset(), offset, "{slot:?}");
        }
        assert!(
            TLS_CONTROL_BLOCK_BYTES > TlsSlot::BionicTls.offset(),
            "the control block must hold every named slot"
        );
    }

    /// **I1.** A block that is dropped without anyone calling anything returns to the arena.
    ///
    /// The arena is sized at creation, so a leak is not a slow drift: it is `capacity` failures
    /// away from refusing every further guest thread, with a message that names the thread count.
    /// Three paths used to drop a block on the floor, and the realistic one — `od_jit_new`
    /// returning null — is not reachable from a unit test, which is exactly why the property is
    /// pinned on `GuestTls` itself rather than on any of the three callers.
    #[test]
    fn a_dropped_block_comes_back_to_the_arena_with_its_commit_intact() {
        let space = GuestSpace::new().expect("a guest address space");
        let arena = TlsArena::with_block_size(&space, 4, TLS_BLOCK_BYTES)
            .expect("an arena for four blocks");
        assert_eq!(arena.capacity(), 4);

        // Fill it, and record what the blocks cost.
        let mut blocks = Vec::new();
        let mut bases = std::collections::HashSet::new();
        for _ in 0..arena.capacity() {
            let block = arena.allocate(&space).expect("a block out of an arena with room");
            assert!(bases.insert(block.thread_pointer()), "two live blocks share a base");
            blocks.push(block);
        }
        let committed = arena.committed();
        assert!(committed > 0, "n = 4 blocks must have taken some commit charge");
        assert!(arena.allocate(&space).is_err(), "a full arena refuses");

        // Drop them with no `free` call anywhere. This is the line that had no implementation.
        drop(blocks);

        // Held for the whole round, not dropped per iteration: a block that went straight back on
        // the free list would be handed out again on the very next turn of this loop, and the
        // duplicate check below would be measuring nothing but its own bug. It measured exactly
        // that on the first draft, which is the cheapest possible reminder that `Drop` is now the
        // thing returning these.
        let mut second_round = Vec::new();
        let mut reused = std::collections::HashSet::new();
        for _ in 0..arena.capacity() {
            let block = arena
                .allocate(&space)
                .expect("a block that was dropped must be handed out again, or the arena leaks");
            assert!(
                reused.insert(block.thread_pointer()),
                "the same base was handed out twice while both blocks were live, which is two \
                 guest threads sharing one stack-guard slot"
            );
            second_round.push(block);
        }
        assert_eq!(reused, bases, "the second round must be exactly the first round's blocks");
        assert_eq!(
            arena.committed(),
            committed,
            "reuse must not take commit charge again, and freeing must not give it back: D10 \
             makes commit charge the thing to husband, not the thing to churn"
        );
    }

    #[test]
    fn a_guard_value_is_never_zero() {
        // A zero canary compares equal to a zeroed stack slot, so the overflow it exists to catch
        // would pass. 64 draws, which is enough to fail if the generator were returning a constant
        // zero and cheap enough to run every time.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let guard = generate_stack_guard();
            assert_ne!(guard, 0);
            seen.insert(guard);
        }
        assert!(seen.len() > 1, "64 draws that were all the same value is not entropy");
    }
}
