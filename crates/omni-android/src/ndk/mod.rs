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

pub mod assets;
pub mod config;
mod handles;
pub mod looper;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};
use parking_lot::Mutex;

use crate::boundary::BoundaryBuilder;
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use handles::Slots;

pub use assets::{AssetSource, OpenAsset};
pub use config::{DeviceConfiguration, ScreenSize, ACONFIGURATION_NAVHIDDEN_NO,
    ACONFIGURATION_NAVHIDDEN_YES};
pub use handles::{SLOT_BYTES, SLOT_MAGIC};
pub use looper::{
    FdRegistration, Looper, ALOOPER_EVENT_ERROR, ALOOPER_EVENT_HANGUP, ALOOPER_EVENT_INPUT,
    ALOOPER_EVENT_INVALID, ALOOPER_EVENT_OUTPUT, ALOOPER_POLL_CALLBACK, ALOOPER_POLL_ERROR,
    ALOOPER_POLL_TIMEOUT, ALOOPER_POLL_WAKE, ALOOPER_PREPARE_ALLOW_NON_CALLBACKS,
};

/// How many loopers one instance can hold.
///
/// **A policy number, and stated as one.** §5.2 needs exactly two — one on the thread that calls
/// `initializeNativeCode` and one on the game thread `GameActivity_onCreate` spawns — and the
/// engine's own worker pools are M8's problem. Each costs [`SLOT_BYTES`] of the arena. A
/// thread past this gets a refusal naming the symbol rather than a second thread sharing the
/// first one's looper, which is the shape that makes two threads drain one pipe.
pub const MAX_LOOPERS: usize = 16;

/// How many `AAssetManager`s one instance can hold.
///
/// **A policy number.** §5.2 needs exactly one — `initializeNativeCode` calls
/// `AAssetManager_fromJava` once with the `AssetManager` the Java side passed it — and asking
/// twice with the same `jobject` gets the same manager back, so four is room for a host that
/// hands the engine more than one without the bound being what stops it.
pub const MAX_ASSET_MANAGERS: usize = 4;

/// How many `AAsset`s one instance can hold open at once.
///
/// Each costs its bytes in guest memory for as long as it is open, because `AAsset_getBuffer`
/// hands the guest a pointer it dereferences. A thirty-third `AAssetManager_open` is a **null**,
/// which is what a device answers when it cannot open an asset and what every caller branches on.
pub const MAX_OPEN_ASSETS: usize = 32;

/// How many `AConfiguration`s one instance can hold.
///
/// §5.2 needs one, on the game thread. `AConfiguration_new` past this returns **null**, which is
/// what a device answers on allocation failure.
pub const MAX_CONFIGURATIONS: usize = 8;

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
    /// The live loopers, addressed by arena slot.
    loopers: Slots<Looper>,
    /// The live `AAssetManager`s.
    managers: Slots<assets::AssetManager>,
    /// The live `AAsset`s.
    assets: Slots<OpenAsset>,
    /// The live `AConfiguration`s.
    configs: Slots<config::LiveConfiguration>,
    /// Which looper address a host thread's guest thread has been given.
    by_thread: HashMap<std::thread::ThreadId, GuestAddr>,
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

/// One NDK instance: the four opaque-handle registries and the arena their identities come out of.
pub struct Ndk {
    /// Kept so `AAsset_getBuffer` can map an asset's bytes where the guest can read them.
    space: Arc<GuestSpace>,
    mem: GuestMem,
    arena: GuestAddr,
    arena_bytes: usize,
    /// Where the guest's assets come from.
    ///
    /// **`None` until the embedding supplies one, and no default is possible** — the same shape
    /// as the filesystem root (D23) and the thread host (D24). This crate cannot read an APK: it
    /// does not depend on `omni-apk` and must not, so the source is a trait the embedding
    /// implements. An instance without one refuses `AAssetManager_fromJava` by name.
    ///
    /// A `OnceLock` for the reason the filesystem root is one: a source that changed under an
    /// open `AAsset` would leave that asset's bytes orphaned from the thing that produced them.
    asset_source: OnceLock<Arc<dyn AssetSource>>,
    /// What `AConfiguration_fromAssetManager` fills a configuration with.
    ///
    /// **`None` by default and it refuses**, the same shape as `HwcapPolicy` (D26): the device's
    /// language, country, screen size and density are the *embedding's* facts, and a number this
    /// layer chose would be a number with nothing behind it.
    configuration: Mutex<Option<DeviceConfiguration>>,
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
        // Four tables, laid out end to end. **Each kind gets its own range**, which is what makes
        // an `AAsset *` passed where an `ALooper *` belongs a refusal rather than a table lookup
        // that happens to succeed.
        let slots = MAX_LOOPERS + MAX_ASSET_MANAGERS + MAX_OPEN_ASSETS + MAX_CONFIGURATIONS;
        let arena_bytes = (slots * SLOT_BYTES + page - 1) & !(page - 1);
        // Eagerly committed: it is one page, and a handle is given to the guest on the first
        // `ALooper_prepare` there is, so nothing is saved by faulting it in.
        let arena = space.map_anonymous(
            Placement::Anywhere { align: page },
            arena_bytes,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        let mem = GuestMem::new(Arc::clone(&space));
        let blame = Blame::new("Ndk::new", arena, 0);
        for index in 0..slots {
            mem.write_u64(arena + index * SLOT_BYTES, SLOT_MAGIC, blame)?;
            mem.write_u64(arena + index * SLOT_BYTES + 8, index as u64, blame)?;
        }
        let loopers = Slots::new(arena, MAX_LOOPERS);
        let managers = Slots::new(loopers.end(), MAX_ASSET_MANAGERS);
        let assets = Slots::new(managers.end(), MAX_OPEN_ASSETS);
        let configs = Slots::new(assets.end(), MAX_CONFIGURATIONS);
        Ok(Arc::new(Self {
            space,
            mem,
            arena,
            arena_bytes,
            asset_source: OnceLock::new(),
            configuration: Mutex::new(None),
            state: Mutex::new(NdkState {
                loopers,
                managers,
                assets,
                configs,
                by_thread: HashMap::new(),
                thread_ids: HashMap::new(),
                events: Vec::new(),
                events_dropped: 0,
            }),
            census: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Supply where the guest's assets come from.
    ///
    /// **Once, before any guest code runs.** There is deliberately no default: this crate cannot
    /// read an APK — it does not depend on `omni-apk` and must not — so the source is the
    /// embedding's. Until one is supplied, `AAssetManager_fromJava` refuses by name.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if a source has already been supplied. Changing it under an open
    /// `AAsset` would leave that asset's bytes orphaned from what produced them.
    pub fn set_asset_source(&self, source: Arc<dyn AssetSource>) -> AbiResult<()> {
        self.asset_source.set(source).map_err(|_| AbiError::Refused {
            symbol: "Ndk::set_asset_source".to_string(),
            address: self.arena,
            why: "this instance already has an asset source. It may be set once: a source that \
                  changed under an open AAsset would leave that asset's bytes orphaned from what \
                  produced them"
                .to_string(),
        })
    }

    /// Whether an asset source has been supplied.
    #[must_use]
    pub fn has_asset_source(&self) -> bool {
        self.asset_source.get().is_some()
    }

    /// Decide what `AConfiguration` reports.
    ///
    /// The same shape as `Bionic::set_hwcap_policy` (D26): the type refuses until a call site
    /// chooses, because the device's language, country, screen and density are the embedding's
    /// facts and a number this layer picked would be a number with nothing behind it.
    ///
    /// May be called again — a device really does change configuration, and `onConfigurationChanged`
    /// is a GameActivity callback — but it does not reach an `AConfiguration` the guest already
    /// holds, exactly as it does not on a device: the guest re-reads by calling
    /// `AConfiguration_fromAssetManager` again.
    pub fn set_configuration(&self, configuration: DeviceConfiguration) {
        *self.configuration.lock() = Some(configuration);
    }

    /// What this instance reports, if the embedding has decided.
    #[must_use]
    pub fn configuration(&self) -> Option<DeviceConfiguration> {
        *self.configuration.lock()
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
        for (symbol, handler) in looper::INLINE.iter().chain(assets::INLINE).chain(config::INLINE)
        {
            builder.bind_inline(symbol, *handler)?;
        }
        for (symbol, handler) in looper::REENTRANT.iter().chain(assets::REENTRANT) {
            builder.bind_reentrant(symbol, *handler)?;
        }
        Ok(Self::bound_symbols().count())
    }

    /// Every symbol this module binds, in table order.
    pub fn bound_symbols() -> impl Iterator<Item = &'static str> {
        Self::inline_symbols().chain(Self::reentrant_symbols())
    }

    /// Every symbol serviced **inside** the run loop.
    pub fn inline_symbols() -> impl Iterator<Item = &'static str> {
        looper::INLINE
            .iter()
            .chain(assets::INLINE)
            .chain(config::INLINE)
            .map(|(s, _)| *s)
    }

    /// Every symbol serviced on the **exit** path, because it calls guest code or maps memory.
    pub fn reentrant_symbols() -> impl Iterator<Item = &'static str> {
        looper::REENTRANT.iter().map(|(s, _)| *s).chain(assets::REENTRANT.iter().map(|(s, _)| *s))
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
        self.state.lock().by_thread.get(&std::thread::current().id()).copied()
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
        self.state.lock().loopers.live()
    }

    /// How many `AAssetManager`s are live.
    #[must_use]
    pub fn live_asset_managers(&self) -> usize {
        self.state.lock().managers.live()
    }

    /// How many `AAsset`s are open.
    #[must_use]
    pub fn live_assets(&self) -> usize {
        self.state.lock().assets.live()
    }

    /// How many `AConfiguration`s are live.
    #[must_use]
    pub fn live_configurations(&self) -> usize {
        self.state.lock().configs.live()
    }

    /// The descriptors a looper is watching, for a test or a diagnostic.
    #[must_use]
    pub fn registrations(&self, looper: GuestAddr) -> Vec<FdRegistration> {
        self.state
            .lock()
            .loopers
            .get(looper)
            .map_or_else(Vec::new, |looper| looper.registrations().to_vec())
    }

    // ------------------------------------------------------------------ internals

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
        if let Some(at) = state.by_thread.get(&id).copied() {
            state.record(at, thread, "prepare", format!("opts {opts}: already prepared"));
            return Ok(at);
        }
        let Some(at) = state.loopers.insert(Looper::new(thread, opts)) else {
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
        state.by_thread.insert(id, at);
        state.record(at, thread, "prepare", format!("opts {opts}: created at {at:#x}"));
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
