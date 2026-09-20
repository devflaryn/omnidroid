//! The adapter: `omni-bionic`'s functions bound to the thunk boundary.
//!
//! D19 keeps `omni-bionic` a separate crate with **zero dependencies**, so that "no OS access"
//! is a fact `cargo tree` can check rather than a rule a reviewer has to notice. The price of
//! that is this module: the crate states what it needs as traits — guest memory, a 32-bit
//! compare-and-swap, a thread identity, a futex, a clock, a place to put `errno` — and something
//! has to supply them. D19 says where: "the adapter binds bionic's functions to the thunk
//! boundary and needs both, so it belongs in `omni-android`".
//!
//! # What a handler is, and why it needs a thread-local
//!
//! [`ImportFn`] is a bare `fn` pointer. It has no captured state and [`ImportCall`] carries no
//! user data, so a handler cannot be handed the mutex owner table or the TLS registry through
//! its arguments. A process-wide `static` would be wrong rather than merely ugly: this runtime
//! is designed to host **three concurrent guest instances** (see the memory figures in
//! `STATUS.md`), and one static would give them one shared `pthread_key` table.
//!
//! So the state is per-instance, in [`Bionic`], and a caller holds an [`Activation`] across
//! [`Boundary::run`](crate::Boundary::run) that publishes it to the calling thread. A guest
//! thread runs one instance at a time, which makes a thread-local exactly the right scope. A
//! handler that finds none refuses with [`AbiError::BionicNotActive`] naming the symbol — it
//! does **not** construct a default, because a per-call default state would give two guest
//! threads their own private copy of the same mutex, and two threads that each believe they
//! hold it is the failure no later test can see.
//!
//! # The per-thread block, and why it is mapped before any guest code runs
//!
//! `errno` is per-thread storage the *guest* must be able to read, because `__errno()` returns a
//! pointer to it. `strerror` likewise returns a pointer to a per-thread buffer. Both come out of
//! one small arena this module maps at construction time, one [`view::THREAD_BLOCK_BYTES`] block
//! per guest thread.
//!
//! **Mapped in [`Bionic::new`], deliberately, and this is F9's constraint.** Task 2's review
//! found that `ImportCall::mem()` reaches the whole [`GuestSpace`], so an inline handler *could*
//! map and unmap while generated code is live — which the pager's "the thread running guest code
//! must not hold this space's lock" invariant does not allow. Nothing in this phase maps from a
//! handler. The one mapping this module performs happens before any CPU exists, let alone runs.

mod data;
mod dl;
mod format;
mod guestmem;
mod handlers;
mod runtime;
mod view;

use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use omni_bionic::atexit::AtexitRegistry;
use omni_bionic::cond::CondWaiters;
use omni_bionic::metadata::NameRegistry;
use omni_bionic::mutex::OwnerTable;
use omni_bionic::threads::GuestThreadId;
use omni_bionic::tls::TlsRegistry;
use omni_elf::loader::DlPhdrInfo;
use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};
use parking_lot::Mutex;

use crate::boundary::{BoundaryBuilder, ImportCall, ImportFn, ReentrantFn};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

pub use data::{DataObject, GuestProcess, DATA_OBJECTS, FILE_BYTES};
pub use runtime::{AddressFutex, CallThreads, HostClock, HostYield, ThreadSlot, ThreadTable};
pub use view::{
    GuestView, DL_INFO_OFFSET, DL_INFO_SLOTS, DL_PHDR_INFO_BYTES, ERRNO_OFFSET, SCRATCH_BYTES,
    SCRATCH_OFFSET, THREAD_BLOCK_BYTES,
};

/// How many guest threads one instance can give a block to.
///
/// **A policy number, and stated as one.** This phase binds no thread lifecycle at all —
/// `pthread_create` is a later phase — so the guest has exactly the threads the host started for
/// it, which is a handful. 64 blocks cost [`ARENA_BYTES`] of address space and one commit
/// granule, and a 65th thread is a refusal naming the symbol rather than a second thread
/// writing the first one's `errno`.
pub const MAX_GUEST_THREADS: usize = 64;

/// Bytes of the arena set aside for objects that outlive a call and belong to no thread.
///
/// The `AMEDIAFORMAT_KEY_*` strings, `environ`'s empty vector, and one copy of each loaded
/// image's `dlpi_name`. All of it is written **before guest code runs** — by
/// [`Bionic::declare_data_into`] and [`Bionic::register_image`], both of which a host calls while
/// setting up — so nothing here maps or writes from inside a handler. That is F9's constraint,
/// and it is the same reason [`Bionic::new`] maps the arena rather than a handler doing it.
///
/// One page. The ten media keys are 104 bytes with their terminators, `environ`'s vector is eight,
/// and eleven library paths at `PATH_MAX`-ish lengths are the rest. A pool that fills is a
/// refusal naming the symbol, never a silent overwrite.
pub const POOL_BYTES: usize = 4096;

/// Bytes of guest address space the arena occupies: the per-thread blocks, then the pool.
pub const ARENA_BYTES: usize = MAX_GUEST_THREADS * THREAD_BLOCK_BYTES + POOL_BYTES;

/// One guest instance's bionic state.
///
/// Everything here is state a *process* has exactly one of: the mutex owner table, the
/// `pthread_key` table, the atexit list, the `rand` sequence. Sharing any of them between two
/// guest instances would be the bug; per-call defaults would be a different one.
pub struct Bionic {
    /// The address space, kept so the arena can be given back.
    space: Arc<GuestSpace>,
    /// First address of the per-thread arena.
    arena: GuestAddr,
    /// Who is attached, and which block each one has.
    threads: ThreadTable,
    /// Blocking, keyed by guest address.
    futex: AddressFutex,
    /// The monotonic and wall clocks the timed waits measure against.
    clock: HostClock,
    /// `sched_yield`.
    yielder: HostYield,
    /// `pthread_key_create` / `getspecific` / `setspecific`, and `__cxa_thread_atexit_impl`.
    tls: TlsRegistry,
    /// Who owns which mutex. An `Arc` because `omni_bionic::cond` installs it as ambient state
    /// for the duration of a `pthread_cond_wait`, which is how the relock on the way out finds
    /// the same table the lock on the way in used.
    owners: Arc<OwnerTable>,
    /// Registered `pthread_cond_wait` waiters.
    conds: CondWaiters,
    /// `pthread_setname_np`.
    names: NameRegistry,
    /// `__cxa_atexit`.
    atexit: AtexitRegistry,
    /// The `rand` sequence's state. Process-wide, as C says it is.
    rand: AtomicU32,
    /// The bump allocator for [`POOL_BYTES`], and what has been handed out of it.
    pool: Mutex<usize>,
    /// Every loaded image `dl_iterate_phdr` must enumerate, in the order it was registered.
    images: Mutex<Vec<GuestImage>>,
}

/// One loaded image, as `dl_iterate_phdr` reports it.
///
/// The guest-side form of [`DlPhdrInfo`]: the name has been copied into the pool and is a guest
/// address, because a callback receives `dlpi_name` as a `const char *` and dereferences it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestImage {
    /// `dlpi_addr`: the load bias.
    pub addr: GuestAddr,
    /// `dlpi_name`: a guest pointer to a NUL-terminated copy of the name, in the pool.
    pub name: GuestAddr,
    /// `dlpi_phdr`: the guest address of the program header table.
    pub phdr: GuestAddr,
    /// `dlpi_phnum`.
    pub phnum: u16,
}

impl core::fmt::Debug for Bionic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bionic")
            .field("arena", &format_args!("{:#x}", self.arena))
            .field("threads", &self.threads.live())
            .finish_non_exhaustive()
    }
}

impl Bionic {
    /// Map the per-thread arena and start with nothing attached.
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the arena could not be mapped.
    pub fn new(space: Arc<GuestSpace>) -> AbiResult<Arc<Self>> {
        // Eager rather than lazy, and the exception to D10's "never commit speculatively" is
        // stated rather than assumed: the arena is 17 KiB, which is a fraction of one 64 KiB
        // commit granule, so lazy and eager cost exactly the same commit charge here. Eager
        // buys that the first `errno` write on a new thread cannot fail for a commit reason
        // inside a handler, where there is no good way to retry.
        let arena = space.map_anonymous(
            Placement::Anywhere { align: space.page_size() },
            ARENA_BYTES,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        Ok(Arc::new(Self {
            space,
            arena,
            threads: ThreadTable::new(),
            futex: AddressFutex::new(),
            clock: HostClock::new(),
            yielder: HostYield,
            tls: TlsRegistry::new(),
            owners: Arc::new(OwnerTable::new()),
            conds: CondWaiters::new(),
            names: NameRegistry::new(),
            atexit: AtexitRegistry::new(),
            // Seeded as C's `rand` is before any `srand`: the standard says the sequence is as
            // if `srand(1)` had been called.
            rand: AtomicU32::new(1),
            pool: Mutex::new(0),
            images: Mutex::new(Vec::new()),
        }))
    }

    /// First address of the static pool.
    #[must_use]
    pub fn pool(&self) -> GuestAddr {
        self.arena + MAX_GUEST_THREADS * THREAD_BLOCK_BYTES
    }

    /// Take `len` zeroed, 8-byte-aligned bytes out of the pool.
    ///
    /// Called from host setup, never from a handler: see [`POOL_BYTES`].
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the pool is full, naming `symbol` — a refusal rather than a
    /// wrapped bump pointer overwriting somebody else's object.
    pub fn reserve(&self, symbol: &str, len: usize) -> AbiResult<GuestAddr> {
        let mut used = self.pool.lock();
        // Eight-byte aligned, because the pool holds `environ`'s vector of pointers as well as
        // strings, and a misaligned pointer array is not something to hand a guest.
        let start = (*used + 7) & !7;
        if len == 0 || start.saturating_add(len) > POOL_BYTES {
            return Err(AbiError::Refused {
                symbol: symbol.to_string(),
                address: self.pool(),
                why: format!(
                    "the adapter's {POOL_BYTES}-byte static pool has {} bytes left and this needs \
                     {len}",
                    POOL_BYTES.saturating_sub(start)
                ),
            });
        }
        let at = self.pool() + start;
        let mem = GuestMem::new(Arc::clone(&self.space));
        mem.write_bytes(at, &vec![0u8; len], Blame::new(symbol, self.pool(), 0))?;
        *used = start + len;
        Ok(at)
    }

    /// Copy `bytes` plus a NUL into the pool and return its guest address.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the pool is full.
    pub fn intern(&self, symbol: &str, bytes: &[u8]) -> AbiResult<GuestAddr> {
        let at = self.reserve(symbol, bytes.len() + 1)?;
        if !bytes.is_empty() {
            let mem = GuestMem::new(Arc::clone(&self.space));
            mem.write_bytes(at, bytes, Blame::new(symbol, self.pool(), 0))?;
        }
        Ok(at)
    }

    /// How many bytes of [`POOL_BYTES`] have been handed out.
    #[must_use]
    pub fn pool_used(&self) -> usize {
        *self.pool.lock()
    }

    /// Tell the adapter about a loaded image, so `dl_iterate_phdr` enumerates it.
    ///
    /// **`dl_iterate_phdr` is not a stub and cannot become one**: the C++ runtime in
    /// `libroblox.so` is statically linked, so the in-guest unwinder walks 11.5 MB of `.eh_frame`
    /// through this call and C++ exceptions break without it. An adapter with no image registered
    /// therefore *refuses* the call rather than reporting an empty process.
    ///
    /// Registration order is iteration order, which is what a dynamic linker reports: the main
    /// object first, then its dependencies in link order. Nothing here sorts.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if the name does not fit the pool.
    pub fn register_image(&self, info: &DlPhdrInfo) -> AbiResult<()> {
        let name = self.intern("dl_iterate_phdr", info.name.as_bytes())?;
        self.images.lock().push(GuestImage {
            addr: info.addr,
            name,
            phdr: info.phdr,
            phnum: info.phnum,
        });
        Ok(())
    }

    /// Every registered image, in registration order.
    #[must_use]
    pub fn images(&self) -> Vec<GuestImage> {
        self.images.lock().clone()
    }

    /// Publish this instance to the calling thread, and attach the thread if it is new.
    ///
    /// Hold the returned guard across [`Boundary::run`](crate::Boundary::run). Dropping it
    /// restores whatever was published before, so a caller that drives two instances from one
    /// host thread cannot leave the wrong one installed.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the arena has no block left, which is a refusal rather than a
    /// second thread sharing the first one's `errno`.
    pub fn activate(self: &Arc<Self>) -> AbiResult<Activation> {
        let base = self.arena;
        let slot = self
            .threads
            .attach_current(MAX_GUEST_THREADS, |index| base + index * THREAD_BLOCK_BYTES)
            .ok_or_else(|| AbiError::Refused {
                symbol: "pthread_self".to_string(),
                address: self.arena,
                why: format!(
                    "this guest instance already has {MAX_GUEST_THREADS} attached threads, and \
                     a {n}th would have to share another thread's errno slot",
                    n = MAX_GUEST_THREADS + 1
                ),
            })?;
        let active = Active { bionic: Arc::clone(self), thread: slot.id, block: slot.block };
        let previous = ACTIVE.with(|cell| cell.borrow_mut().replace(active));
        Ok(Activation { previous })
    }

    /// This thread's `pthread_t`, if it is attached.
    #[must_use]
    pub fn current_thread(&self) -> Option<GuestThreadId> {
        ACTIVE.with(|cell| cell.borrow().as_ref().map(|a| a.thread))
    }

    /// The per-thread arena's first address, for a test that wants to look at it.
    #[must_use]
    pub fn arena(&self) -> GuestAddr {
        self.arena
    }

    /// How many host threads have attached to this instance.
    #[must_use]
    pub fn attached(&self) -> usize {
        self.threads.live()
    }

    /// The futex, for a test that wants its activity counters.
    #[must_use]
    pub fn futex(&self) -> &AddressFutex {
        &self.futex
    }

    /// The monotonic and wall clocks the sync layer's timed waits measure against.
    ///
    /// **Nothing this phase binds reads it yet.** `pthread_cond_timedwait`, `sem_timedwait` and
    /// the rwlock's timed forms are the callers, and none of them is in the reachable set's first
    /// six sections — `pthread_cond_timedwait` is Tier C. It is wired and exercised here rather
    /// than left out, because the trait exists, the implementation is six lines of portable
    /// `std::time`, and an unwired capability is how the next phase discovers that the shape was
    /// wrong.
    #[must_use]
    pub fn clock(&self) -> &HostClock {
        &self.clock
    }

    /// The atexit list, so a caller can run the registered handlers at shutdown.
    #[must_use]
    pub fn atexit(&self) -> &AtexitRegistry {
        &self.atexit
    }

    /// The `rand` state. Process-wide, as C specifies.
    #[must_use]
    pub fn rand_state(&self) -> u32 {
        self.rand.load(Ordering::Relaxed)
    }

    /// Store the `rand` state.
    pub fn set_rand_state(&self, state: u32) {
        self.rand.store(state, Ordering::Relaxed);
    }

    /// Bind every symbol this phase implements into `builder`, and return how many.
    ///
    /// Symbols this phase does **not** implement are left exactly as the boundary left them:
    /// `Unbound`, with a real address whose call names the symbol. That is the design (see
    /// [`Binding::Unbound`](crate::Binding::Unbound)) and not a gap — a `fopen` bound to a stub
    /// that returned a plausible `FILE*` would surface three thousand initializers later.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the thunk region cannot hold another slot.
    pub fn bind_into(&self, builder: &BoundaryBuilder) -> AbiResult<usize> {
        for (symbol, handler) in INLINE {
            builder.bind_inline(symbol, *handler)?;
        }
        for (symbol, handler) in REENTRANT {
            builder.bind_reentrant(symbol, *handler)?;
        }
        Ok(INLINE.len() + REENTRANT.len())
    }

    /// Every symbol this phase binds, in table order.
    pub fn bound_symbols() -> impl Iterator<Item = &'static str> {
        INLINE.iter().map(|(s, _)| *s).chain(REENTRANT.iter().map(|(s, _)| *s))
    }

    /// Every symbol serviced **inside** the run loop, in table order.
    pub fn inline_symbols() -> impl Iterator<Item = &'static str> {
        INLINE.iter().map(|(s, _)| *s)
    }

    /// Every symbol serviced on the **exit** path, in table order.
    pub fn reentrant_symbols() -> impl Iterator<Item = &'static str> {
        REENTRANT.iter().map(|(s, _)| *s)
    }

    /// Declare the eighteen `STT_OBJECT` imports into `builder` and fill them in.
    ///
    /// **Call this before the loader resolves symbols**, because a data import only gets an
    /// address if it was declared with a size — [`BoundaryBuilder::declare_data`] is explicit that
    /// there is no default — and the loader's answer is what the guest's `GOT` ends up holding.
    ///
    /// Returns how many were declared. `builder` must be over the same [`GuestSpace`] this
    /// instance was built on; one over a different space produces a typed
    /// [`AbiError::BadPointer`] out of the first write rather than a
    /// silently unfilled object.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the data area cannot hold them, [`AbiError::Refused`] for a
    /// zero [`stack_guard`](data::GuestProcess::stack_guard), and
    /// [`AbiError::BadPointer`] if a write into the data area or the
    /// pool is refused.
    pub fn declare_data_into(
        &self,
        builder: &BoundaryBuilder,
        process: &GuestProcess,
    ) -> AbiResult<usize> {
        data::install(self, builder, process)
    }
}

impl Drop for Bionic {
    fn drop(&mut self) {
        // The arena is this instance's own mapping and nothing else refers to it. A failure to
        // give it back is not reportable from `drop` and is not worth aborting over: the space
        // itself is about to go, and the commit charge goes with it.
        let _ = self.space.unmap(self.arena, ARENA_BYTES);
    }
}

/// The instance published to one thread, plus that thread's identity and block.
#[derive(Clone)]
pub struct Active {
    /// The instance.
    pub bionic: Arc<Bionic>,
    /// This thread's `pthread_t`.
    pub thread: GuestThreadId,
    /// This thread's block in the arena.
    pub block: GuestAddr,
}

thread_local! {
    // The instance this thread's guest code belongs to. A `RefCell<Option<..>>` rather than a
    // raw pointer, because the whole reason this is not a `static` is that the lifetime is a
    // caller's and not the program's.
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// Restores the previously published instance when dropped.
pub struct Activation {
    previous: Option<Active>,
}

impl Drop for Activation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

impl core::fmt::Debug for Activation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Activation")
    }
}

/// The instance published to this thread, or a typed refusal naming the symbol.
///
/// Cloned out rather than borrowed: the borrow would have to be held across a handler that can
/// re-enter guest code and arrive back here, and an `Arc` clone is one relaxed increment.
pub(crate) fn active(symbol: &str, address: GuestAddr) -> AbiResult<Active> {
    ACTIVE.with(|cell| cell.borrow().clone()).ok_or_else(|| AbiError::BionicNotActive {
        symbol: symbol.to_string(),
        address,
    })
}

/// The state and the view one inline handler works through.
pub(crate) fn enter<'a>(
    call: &'a ImportCall<'_, '_>,
    active: &'a Active,
) -> GuestView<'a> {
    GuestView::new(call.mem(), call.symbol(), call.address(), active)
}

/// Every symbol serviced **inside** the run loop: pure host work, no guest code.
static INLINE: &[(&str, ImportFn)] = handlers::INLINE;

/// Every symbol serviced on the **exit** path, because it calls guest code back.
static REENTRANT: &[(&str, ReentrantFn)] = handlers::REENTRANT;

