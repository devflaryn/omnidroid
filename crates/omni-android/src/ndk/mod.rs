//! The NDK surface: `ALooper` now, `AAssetManager`, `AConfiguration` and `ANativeWindow` next.
//!
//! The third thing in this crate that the guest reaches, after the bionic adapter and the JNI
//! tables, and the first that is neither. `libroblox.so` imports **32** `libandroid.so` /
//! `libnativewindow.so` symbols (`apk-analysis.md` §4.4) in four families, and **none of them is
//! among the 188 the static initializers reach** — every one is reached from
//! `Java_com_google_androidgamesdk_GameActivity_initializeNativeCode` or from the game thread it
//! spawns, which is §8 step 13 and everything after it.
//!
//! # Why this is its own instance with its own activation
//!
//! Same shape as [`Bionic`](crate::bionic::Bionic) and [`Jni`](crate::jni::Jni), and for the same
//! reason: an [`ImportFn`](crate::ImportFn) is a bare `fn` pointer with no user data, so per-instance state has to
//! be published to the calling thread by a guard held across
//! [`Boundary::run`](crate::Boundary::run). A process-wide `static` would give this runtime's
//! three concurrent guest instances one shared looper registry, and two guests whose game threads
//! shared a looper is the failure no later test can see.
//!
//! It is a **third** activation rather than a field of one of the other two because it is a third
//! thing. What it borrows at call time it borrows explicitly: `ALooper_pollOnce` asks the *bionic*
//! activation for the descriptor table, because a looper polls descriptors and descriptors belong
//! to the filesystem seam. A host that activates the looper registry and not bionic gets a
//! refusal naming both.
//!
//! # §8.1's fourth failure mode, made into a precondition
//!
//! > `ALooper_forThread()` returning `NULL` makes `initializeNativeCode` return `0` and Java-side
//! > startup fails **silently**.
//!
//! `jni-surface.md` §5.2 decodes the site: the constructor logs `"Unable to retrieve native
//! ALooper"` and returns zero, and a `jlong` of zero is indistinguishable from a handle the Java
//! side would then pass to all 23 other natives. So the host must prepare a looper on the thread
//! it calls step 13 from, **before** the call — [`Ndk::prepare_looper`] is that, and it is a host
//! API precisely so a gate can assert a looper exists rather than diagnose a zero afterwards.
//!
//! # The instrumentation, built before it is needed
//!
//! §8.1's fifth failure mode is that a deadlock in step 14's `pthread_cond_wait` is
//! indistinguishable from a hang. Every looper operation is recorded here — what was asked, by
//! which guest thread, and what was decided — so that "the gate hung" becomes "thread 0 last
//! called `ALooper_pollOnce` with a 100 ms timeout and thread 1 has added no descriptor". The log
//! is bounded like [`MAX_CALL_RECORDS`](crate::jni::MAX_CALL_RECORDS) with a dropped count, because
//! a `pollOnce` per frame would otherwise turn the record into a sample of its own beginning.

pub mod looper;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};
use parking_lot::Mutex;

use crate::boundary::BoundaryBuilder;
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

pub use looper::{
    FdRegistration, Looper, ALOOPER_EVENT_ERROR, ALOOPER_EVENT_HANGUP, ALOOPER_EVENT_INPUT,
    ALOOPER_EVENT_INVALID, ALOOPER_EVENT_OUTPUT, ALOOPER_POLL_CALLBACK, ALOOPER_POLL_ERROR,
    ALOOPER_POLL_TIMEOUT, ALOOPER_POLL_WAKE, ALOOPER_PREPARE_ALLOW_NON_CALLBACKS,
};

/// How many loopers one instance can hold.
///
/// **A policy number, and stated as one.** §5.2 needs exactly two — one on the thread that calls
/// `initializeNativeCode` and one on the game thread `GameActivity_onCreate` spawns — and the
/// engine's own worker pools are M8's problem. Each costs [`LOOPER_SLOT_BYTES`] of the arena. A
/// thread past this gets a refusal naming the symbol rather than a second thread sharing the
/// first one's looper, which is the shape that makes two threads drain one pipe.
pub const MAX_LOOPERS: usize = 16;

/// Bytes of arena reserved for each looper.
///
/// An `ALooper*` is **opaque** — AOSP declares `struct ALooper;` and never defines it, and the
/// GameActivity glue only stores the pointer and hands it back. So the slot exists to make the
/// identity a *real guest address* the guest can compare and store, not to hold fields. It is
/// written with [`LOOPER_MAGIC`] so that a guest which does dereference it reads something
/// recognisable in a dump rather than whatever the arena happened to contain.
pub const LOOPER_SLOT_BYTES: usize = 16;

/// What a looper slot holds, for a reader of a memory dump. Nothing reads it back.
pub const LOOPER_MAGIC: u64 = 0x004F_4D4E_4C4F_4F50; // "\0OMNLOOP"

/// How many descriptors one looper can watch.
///
/// §5.2 registers one per looper: `msgread` in the constructor, and the glue's own command pipe
/// on the game thread. Sixteen is room for the engine to be doing something this layer has not
/// seen without the bound being the thing that stops it.
pub const MAX_LOOPER_FDS: usize = 16;

/// How many looper operations one instance records before it stops recording.
pub const MAX_LOOPER_EVENTS: usize = 4096;

/// How many guest threads can be given a looper identity.
///
/// Matched to [`MAX_JNI_THREADS`](crate::jni::MAX_JNI_THREADS) so the two ceilings cannot
/// disagree about how many threads one guest instance has.
pub const MAX_NDK_THREADS: usize = crate::jni::MAX_JNI_THREADS;

/// One looper operation, as it happened.
///
/// **The measurement §8.1's fifth failure mode asks for.** A hang inside step 13 is a hang in one
/// of two threads, and which of them last did what is the whole of the difference between "the
/// glue is waiting for the game thread" and "the game thread is waiting for a descriptor nobody
/// writes to".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooperEvent {
    /// The looper it was about, or `0` for a call that found none.
    pub looper: GuestAddr,
    /// Which guest thread made the call, as this instance numbers them.
    pub thread: usize,
    /// The operation: `"prepare"`, `"forThread"`, `"acquire"`, `"release"`, `"addFd"`,
    /// `"removeFd"`, `"pollOnce"`, `"callback"`.
    pub what: &'static str,
    /// What it was asked and what it decided.
    pub detail: String,
}

/// Everything one NDK instance owns, behind one lock.
#[derive(Debug)]
struct NdkState {
    /// One entry per live looper, indexed by its arena slot.
    loopers: Vec<Option<Looper>>,
    /// Which slot a host thread's guest thread has been given a looper in.
    by_thread: HashMap<std::thread::ThreadId, usize>,
    /// Which index this instance has given each host thread, for the event log.
    thread_ids: HashMap<std::thread::ThreadId, usize>,
    events: Vec<LooperEvent>,
    events_dropped: usize,
}

impl NdkState {
    /// Record one operation, or count it as dropped.
    fn record(&mut self, looper: GuestAddr, thread: usize, what: &'static str, detail: String) {
        if self.events.len() >= MAX_LOOPER_EVENTS {
            self.events_dropped += 1;
            return;
        }
        self.events.push(LooperEvent { looper, thread, what, detail });
    }
}

/// One NDK instance: the looper registry and the arena its identities come out of.
pub struct Ndk {
    mem: GuestMem,
    arena: GuestAddr,
    arena_bytes: usize,
    state: Mutex<NdkState>,
    /// How many times each named NDK function has been serviced.
    ///
    /// Its own lock, for the reason [`Jni`](crate::jni::Jni)'s census gives: a handler counts
    /// itself and then takes the state lock, and folding the two together would make the state
    /// lock re-entrant.
    census: Mutex<BTreeMap<&'static str, u64>>,
}

impl core::fmt::Debug for Ndk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ndk").field("arena", &format_args!("{:#x}", self.arena)).finish()
    }
}

impl Ndk {
    /// Map the arena.
    ///
    /// **Everything this writes into guest memory happens here**, before any CPU exists, which is
    /// task 2's review finding F9: a handler must not change the address space while generated
    /// code is live.
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the address space could not supply the arena.
    pub fn new(space: Arc<GuestSpace>) -> AbiResult<Arc<Self>> {
        let page = space.page_size();
        let arena_bytes = (MAX_LOOPERS * LOOPER_SLOT_BYTES + page - 1) & !(page - 1);
        // Eagerly committed: it is one page, and a looper identity is handed to the guest on the
        // first `ALooper_prepare` there is, so nothing is saved by faulting it in.
        let arena = space.map_anonymous(
            Placement::Anywhere { align: page },
            arena_bytes,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        let mem = GuestMem::new(space);
        let blame = Blame::new("Ndk::new", arena, 0);
        for index in 0..MAX_LOOPERS {
            mem.write_u64(arena + index * LOOPER_SLOT_BYTES, LOOPER_MAGIC, blame)?;
            mem.write_u64(arena + index * LOOPER_SLOT_BYTES + 8, index as u64, blame)?;
        }
        let mut loopers = Vec::with_capacity(MAX_LOOPERS);
        loopers.resize_with(MAX_LOOPERS, || None);
        Ok(Arc::new(Self {
            mem,
            arena,
            arena_bytes,
            state: Mutex::new(NdkState {
                loopers,
                by_thread: HashMap::new(),
                thread_ids: HashMap::new(),
                events: Vec::new(),
                events_dropped: 0,
            }),
            census: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Checked guest memory over this instance's address space.
    #[must_use]
    pub fn mem(&self) -> &GuestMem {
        &self.mem
    }

    /// The arena's base, for a test that reads a looper identity back.
    #[must_use]
    pub fn arena(&self) -> GuestAddr {
        self.arena
    }

    /// Bytes of arena this instance holds.
    #[must_use]
    pub fn arena_bytes(&self) -> usize {
        self.arena_bytes
    }

    /// Bind every NDK symbol this layer implements into `builder`, and return how many.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if a symbol already has a slot.
    pub fn bind_into(&self, builder: &BoundaryBuilder) -> AbiResult<usize> {
        for (symbol, handler) in looper::INLINE {
            builder.bind_inline(symbol, *handler)?;
        }
        for (symbol, handler) in looper::REENTRANT {
            builder.bind_reentrant(symbol, *handler)?;
        }
        Ok(looper::INLINE.len() + looper::REENTRANT.len())
    }

    /// Every symbol this module binds, in table order.
    pub fn bound_symbols() -> impl Iterator<Item = &'static str> {
        looper::INLINE.iter().map(|(s, _)| *s).chain(looper::REENTRANT.iter().map(|(s, _)| *s))
    }

    /// Every symbol serviced **inside** the run loop.
    pub fn inline_symbols() -> impl Iterator<Item = &'static str> {
        looper::INLINE.iter().map(|(s, _)| *s)
    }

    /// Every symbol serviced on the **exit** path, because it calls guest code.
    pub fn reentrant_symbols() -> impl Iterator<Item = &'static str> {
        looper::REENTRANT.iter().map(|(s, _)| *s)
    }

    /// Publish this instance to the calling thread until the guard is dropped.
    #[must_use]
    pub fn activate(self: &Arc<Self>) -> NdkActivation {
        let previous =
            ACTIVE.with(|cell| cell.borrow_mut().replace(ActiveNdk { ndk: Arc::clone(self) }));
        NdkActivation { previous }
    }

    /// **The host's own `ALooper_prepare`**, for the thread it is about to call step 13 from.
    ///
    /// §8.1's fourth failure mode is that `initializeNativeCode` returns `0` when
    /// `ALooper_forThread` finds nothing, and a `jlong` of zero is indistinguishable from a real
    /// handle. So this exists to be called *before* the guest is, and to be **asserted**: a gate
    /// that checks a looper is there is diagnosing nothing, where one that reads a zero back
    /// afterwards is diagnosing everything at once.
    ///
    /// Idempotent, exactly as `ALooper_prepare` is: a thread that already has one gets it back.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if this instance already holds [`MAX_LOOPERS`].
    pub fn prepare_looper(&self) -> AbiResult<GuestAddr> {
        self.prepare_for("Ndk::prepare_looper", ALOOPER_PREPARE_ALLOW_NON_CALLBACKS)
    }

    /// The calling thread's looper, or `None`. The host-side spelling of `ALooper_forThread`.
    #[must_use]
    pub fn looper_for_current_thread(&self) -> Option<GuestAddr> {
        let state = self.state.lock();
        state.by_thread.get(&std::thread::current().id()).map(|slot| self.address_of(*slot))
    }

    /// Every looper operation this instance recorded, oldest first.
    #[must_use]
    pub fn events(&self) -> Vec<LooperEvent> {
        self.state.lock().events.clone()
    }

    /// How many operations were not recorded because the log was full.
    #[must_use]
    pub fn events_dropped(&self) -> usize {
        self.state.lock().events_dropped
    }

    /// How many times each named NDK function has been serviced.
    #[must_use]
    pub fn census(&self) -> BTreeMap<&'static str, u64> {
        self.census.lock().clone()
    }

    /// How many loopers are live.
    #[must_use]
    pub fn live_loopers(&self) -> usize {
        self.state.lock().loopers.iter().filter(|slot| slot.is_some()).count()
    }

    /// The descriptors a looper is watching, for a test or a diagnostic.
    #[must_use]
    pub fn registrations(&self, looper: GuestAddr) -> Vec<FdRegistration> {
        let state = self.state.lock();
        self.slot_of(looper)
            .and_then(|slot| state.loopers[slot].as_ref())
            .map_or_else(Vec::new, |looper| looper.registrations().to_vec())
    }

    // ------------------------------------------------------------------ internals

    /// The guest address of a slot.
    fn address_of(&self, slot: usize) -> GuestAddr {
        self.arena + slot * LOOPER_SLOT_BYTES
    }

    /// The slot a guest `ALooper*` names, if it is one of ours.
    ///
    /// **Checked rather than trusted**, as a `jobject` is: the guest is hostile by assumption, and
    /// an `ALooper*` that is merely inside the arena but not on a slot boundary would otherwise
    /// index whatever the division produced.
    fn slot_of(&self, looper: GuestAddr) -> Option<usize> {
        if looper < self.arena || looper >= self.arena + MAX_LOOPERS * LOOPER_SLOT_BYTES {
            return None;
        }
        let offset = looper - self.arena;
        (offset % LOOPER_SLOT_BYTES == 0).then_some(offset / LOOPER_SLOT_BYTES)
    }

    /// This thread's index for the event log, assigned on first use.
    fn thread_index(state: &mut NdkState) -> usize {
        let id = std::thread::current().id();
        let next = state.thread_ids.len();
        *state.thread_ids.entry(id).or_insert(next)
    }

    /// The body `ALooper_prepare` and [`Ndk::prepare_looper`] share.
    fn prepare_for(&self, caller: &str, opts: i32) -> AbiResult<GuestAddr> {
        let id = std::thread::current().id();
        let mut state = self.state.lock();
        let thread = Self::thread_index(&mut state);
        if let Some(slot) = state.by_thread.get(&id).copied() {
            let at = self.address_of(slot);
            state.record(at, thread, "prepare", format!("opts {opts}: already prepared"));
            return Ok(at);
        }
        let Some(slot) = state.loopers.iter().position(Option::is_none) else {
            return Err(AbiError::Refused {
                symbol: caller.to_string(),
                address: self.arena,
                why: format!(
                    "this guest instance already holds {MAX_LOOPERS} loopers, which is the cap. A \
                     further thread is refused rather than given another thread's looper: two \
                     threads sharing one looper would drain each other's pipe, and jni-surface.md \
                     §5.2 needs exactly two -- the thread that calls initializeNativeCode and the \
                     game thread GameActivity_onCreate spawns"
                ),
            });
        };
        if state.thread_ids.len() > MAX_NDK_THREADS {
            return Err(AbiError::Refused {
                symbol: caller.to_string(),
                address: self.arena,
                why: format!("more than {MAX_NDK_THREADS} threads have reached this instance"),
            });
        }
        state.loopers[slot] = Some(Looper::new(thread, opts));
        state.by_thread.insert(id, slot);
        let at = self.address_of(slot);
        state.record(at, thread, "prepare", format!("opts {opts}: created in slot {slot}"));
        Ok(at)
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<ActiveNdk>> = const { RefCell::new(None) };
}

/// The instance published to one thread.
#[derive(Clone)]
struct ActiveNdk {
    ndk: Arc<Ndk>,
}

/// Restores the previously published instance when dropped.
pub struct NdkActivation {
    previous: Option<ActiveNdk>,
}

impl Drop for NdkActivation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

impl core::fmt::Debug for NdkActivation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("NdkActivation")
    }
}

/// The instance published to this thread, or a typed refusal naming the NDK function.
pub(crate) fn active(symbol: &str, address: GuestAddr) -> AbiResult<Arc<Ndk>> {
    ACTIVE.with(|cell| cell.borrow().clone()).map(|active| active.ndk).ok_or_else(|| {
        AbiError::NdkNotActive { symbol: symbol.to_string(), address }
    })
}
