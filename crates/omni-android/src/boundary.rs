//! The boundary: which guest address is which symbol, who services it, and how guest code gets
//! called back.
//!
//! # Two dispatch paths, and the type system keeps them apart
//!
//! D17 measured them at **≈33 ns** and **80-105 ns** per call (n = 31 per cell per process, 15
//! processes × 3 placements), a factor of 3, and decided: dispatch inside the run loop per symbol,
//! keeping the exit for unresolved imports and for anything that must call back into guest code.
//!
//! | Path | Handler | May call guest code | Cost |
//! |---|---|---|---|
//! | inside the run loop | [`ImportFn`] over [`ImportCall`] | **no — structurally** | ≈33 ns |
//! | out through [`ExitReason::Thunk`] | [`ReentrantFn`] over [`ReentrantCall`] | yes | 80-105 ns |
//!
//! "Structurally" is the load-bearing word. [`ImportCall`] holds the register file and guest memory
//! and **no CPU**, so there is no expression an inline handler can write that re-enters the guest.
//! That is not a style rule: an inline handler runs inside one of the translating backend's own
//! callbacks, where a `&mut CpuCtx` is live, and re-entering `od_jit_run` from there would form a
//! second one — undefined behaviour, not untidiness. See `omni_cpu::thunk`.
//!
//! An inline handler that finds it cannot finish — a bad pointer, an unterminated string, a `va_list`
//! that does not describe a save area — has no return channel either, so it calls
//! [`ThunkCall::defer_to_caller`](omni_cpu::ThunkCall::defer_to_caller) and the error is picked up on
//! the exit path. One mechanism for both escalations.
//!
//! # Three re-entrancy hazards, and what closes each
//!
//! 1. **Nesting a run inside a callback.** Closed by the type split above.
//! 2. **Unbounded depth.** Guest calls `qsort`, whose comparator calls `qsort`. Every level is
//!    legitimate and the limit is the *host's* stack, which is an abort reachable from guest data and
//!    therefore Critical (Global Constraint 11). Closed by [`MAX_GUEST_DEPTH`] and
//!    [`AbiError::TooDeep`].
//! 3. **State the outer call still needs.** A guest callback clobbers `X0`-`X18`, `V0`-`V7`, the
//!    condition flags and the sentinel, exactly as any guest function may. Closed twice: the outer
//!    call's arguments are snapshotted into [`ArgRegs`] before the handler runs at all, and
//!    [`call_guest`](ReentrantCall::call_guest) saves and restores the whole architectural state
//!    around the nested run.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use omni_cpu::{
    AccessKind, ExitReason, GuestCpu, Nzcv, RunLimit, ThunkCall, ThunkContext, ThunkRegs, VReg,
    XReg,
};
use omni_elf::loader::{SymbolKind, SymbolProvider, SymbolRequest, SymbolValue};
use omni_mem::{GuestAddr, GuestSpace};
use parking_lot::Mutex;

use crate::abi::{ArgRegs, Args, Ret, RetSink, ARG_REGISTERS};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};
use crate::region::ThunkRegion;
use crate::varargs::VarArgs;

/// How many levels of guest → host → guest the boundary allows.
///
/// **A policy number.** Real nesting in this engine is one or two levels — a `pthread_once`
/// initialiser, an `atexit` handler, a `qsort` comparator — and nothing legitimate approaches eight.
/// What eight buys is that a guest which recurses through the boundary hits a typed error naming the
/// symbol instead of the host's stack guard page, and an abort cannot be contained by any caller.
pub const MAX_GUEST_DEPTH: usize = 8;

/// How many times the boundary will service an exit-path thunk in one
/// [`run`](Boundary::run) before giving up.
///
/// The *inline* path needs no such cap: a guest looping through an inline thunk spends guest
/// instructions and the backend's own budget stops it, which `omni-cpu`'s
/// `a_counted_budget_still_stops_a_guest_looping_through_an_inline_thunk` establishes. The exit path
/// is different — each crossing returns to Rust, so a two-instruction guest loop through an exit
/// thunk makes progress the backend's budget never sees. The halt flag is checked every crossing as
/// well; this is the bound for a caller that armed no halt.
///
/// Generous on purpose: the 3,594 initializers register an `__cxa_atexit` handler each, and an
/// initializer run is expected to cross millions of times. Well under `i64::MAX` — D16's footgun is
/// about a budget handed to the backend and this is a Rust counter, but a number that reads as
/// negative anywhere is a number to keep away from.
pub const MAX_EXIT_CROSSINGS: u64 = 1 << 32;

/// A host implementation that runs **inside** the run loop.
///
/// Returns a result: `Err` becomes a typed error the caller sees, through
/// [`ThunkCall::defer_to_caller`](omni_cpu::ThunkCall::defer_to_caller). It is never a fabricated
/// return value (Global Constraint 1).
pub type ImportFn = fn(&mut ImportCall<'_, '_>) -> AbiResult<()>;

/// A host implementation that runs **outside** the run loop, and may call guest code.
pub type ReentrantFn = fn(&mut ReentrantCall<'_>) -> AbiResult<()>;

/// What a thunk slot is for.
#[derive(Clone, Copy)]
pub enum Binding {
    /// A pure host function, serviced inside the run loop.
    Inline(ImportFn),
    /// A host function that may call back into guest code, serviced on the exit path.
    Reentrant(ReentrantFn),
    /// A slot with an address and a name and no implementation.
    ///
    /// **Not a gap in the design — the design.** Every import gets a slot, including the 377 outside
    /// the reachable 188, so that a call to one that was never predicted arrives as
    /// [`AbiError::Unbound`] naming the symbol rather than as a branch to address zero.
    Unbound,
    /// An `STT_OBJECT` data address that guest code is loading from, not calling.
    Data,
}

impl core::fmt::Debug for Binding {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Binding::Inline(_) => "Inline",
            Binding::Reentrant(_) => "Reentrant",
            Binding::Unbound => "Unbound",
            Binding::Data => "Data",
        })
    }
}

/// One symbol's slot.
#[derive(Debug)]
pub struct Slot {
    /// The symbol name, exactly as `.dynstr` spells it.
    pub symbol: String,
    /// Its guest address.
    pub address: GuestAddr,
    /// Who services it.
    pub binding: Binding,
    /// The library the **guest's own `DT_VERNEED`** attributes this import to, when it records
    /// one.
    ///
    /// Not a list this layer keeps: it arrives in [`SymbolRequest::library`] while the loader
    /// relocates, so it is the importing binary's own statement about where the symbol comes from.
    /// `libroblox.so` attributes 345 of its 565 imports to `libc.so`, 56 to `libm.so` and 6 to
    /// `libdl.so`, and leaves 158 unversioned — see `omni_elf::version` for why unversioned is
    /// honest rather than missing.
    ///
    /// It is what makes `dlopen`/`dlsym` answerable: a guest that asks `libc.so` for `getauxval`
    /// is asking for a symbol its own file says lives there.
    ///
    /// [`SymbolRequest::library`]: omni_elf::loader::SymbolRequest::library
    pub library: Option<String>,
    /// How many times the guest has branched here **while the census was on**. See
    /// [`Boundary::start_census`].
    calls: AtomicU64,
}

impl Slot {
    /// How many times the guest has called this symbol since the census was started.
    ///
    /// Zero when the census has never been on, which is not the same statement as "never called" —
    /// [`Boundary::census`] is what tells the two apart, because it refuses to report at all
    /// unless the census was running.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

// The per-thread error channel between an inline handler and `Boundary::run`.
//
// A thread local rather than a lock on the `Boundary`, for two reasons. It is on the path a guest
// takes for every failed imported call, and a lock there would serialise guest threads on the
// boundary's error slot. And it is *correct* where a shared slot is not: two guest threads failing at
// once would otherwise each be able to read the other's error, and an error naming the wrong symbol
// 3,000 initializers deep is worse than no error at all.
thread_local! {
    // The per-thread error channel. See the comment above; it is on a `thread_local!` invocation, so
    // rustdoc cannot carry it as documentation.
    static PENDING: RefCell<Option<AbiError>> = const { RefCell::new(None) };
}

fn record_pending(error: AbiError) {
    PENDING.with(|slot| {
        let mut slot = slot.borrow_mut();
        // First one wins, matching `omni-cpu`'s `stop`: a second failure in the same crossing would
        // overwrite the reason the call actually failed for.
        if slot.is_none() {
            *slot = Some(error);
        }
    });
}

fn take_pending() -> Option<AbiError> {
    PENDING.with(|slot| slot.borrow_mut().take())
}

// ---------------------------------------------------------------------------------- the builder

struct BuilderInner {
    region: ThunkRegion,
    slots: BTreeMap<GuestAddr, Slot>,
    by_name: BTreeMap<String, GuestAddr>,
    /// Symbols a **weak** reference to must resolve to nothing. See
    /// [`BoundaryBuilder::declare_absent`].
    absent: BTreeSet<String>,
    sentinel: GuestAddr,
    exit_crossings: u64,
}

/// The boundary under construction: slots are allocated here, and bound here.
///
/// Two phases rather than one because the *loader* is what assigns addresses — it asks a
/// [`SymbolProvider`] for each of the 565 imports from `&self`,
/// while relocating — and dispatch afterwards must take no lock at all. So allocation is behind a
/// mutex and [`finish`](BoundaryBuilder::finish) hands out an immutable [`Boundary`].
pub struct BoundaryBuilder {
    inner: Mutex<BuilderInner>,
    space: Arc<GuestSpace>,
}

impl BoundaryBuilder {
    /// Reserve a thunk region and start with nothing bound.
    ///
    /// `slots` sizes the function area: 565 covers every import of `libroblox.so`, which is what the
    /// loader will ask about, and is the right number even though only 188 are predicted reachable —
    /// see [`Binding::Unbound`].
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the region could not be reserved.
    pub fn new(space: Arc<GuestSpace>, slots: usize, data_bytes: usize) -> AbiResult<Self> {
        // One extra slot, for the sentinel a host-to-guest call returns through. It lives in the
        // function area on purpose: the area is not executable, so a guest that branches there of its
        // own accord takes a typed fault rather than executing something.
        let mut region = ThunkRegion::reserve(Arc::clone(&space), slots + 1, data_bytes)?;
        let sentinel = region.allocate_function()?;
        Ok(Self {
            inner: Mutex::new(BuilderInner {
                region,
                slots: BTreeMap::new(),
                by_name: BTreeMap::new(),
                absent: BTreeSet::new(),
                sentinel,
                exit_crossings: MAX_EXIT_CROSSINGS,
            }),
            space,
        })
    }

    /// Give `symbol` a function slot, or return the one it already has.
    ///
    /// Idempotent, because the loader may ask about the same name more than once — `.dynsym` has one
    /// entry per symbol but `.rela.plt` and `.rela.dyn` can both reference it.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`].
    pub fn declare_function(&self, symbol: &str) -> AbiResult<GuestAddr> {
        let mut inner = self.inner.lock();
        if let Some(&address) = inner.by_name.get(symbol) {
            return Ok(address);
        }
        let address = inner.region.allocate_function()?;
        inner.by_name.insert(symbol.to_string(), address);
        inner.slots.insert(
            address,
            Slot {
                symbol: symbol.to_string(),
                address,
                binding: Binding::Unbound,
                library: None,
                calls: AtomicU64::new(0),
            },
        );
        Ok(address)
    }

    /// Give `symbol` `len` bytes of the data area, aligned to `align`, or return what it already has.
    ///
    /// The size has to be stated, and there is no default: `__sF` is an array of three `FILE`
    /// structures that the guest reaches as `__sF + addend`, so a pointer-sized cell would be
    /// silently too small and the guest's `stdout` would be somebody else's data object. An import
    /// this is never called for stays unresolved, which the loader already reports by name.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`].
    pub fn declare_data(&self, symbol: &str, len: usize, align: usize) -> AbiResult<GuestAddr> {
        let mut inner = self.inner.lock();
        if let Some(&address) = inner.by_name.get(symbol) {
            return Ok(address);
        }
        let address = inner.region.allocate_data(len, align)?;
        inner.by_name.insert(symbol.to_string(), address);
        inner
            .slots
            .insert(
                address,
                Slot {
                    symbol: symbol.to_string(),
                    address,
                    binding: Binding::Data,
                    library: None,
                    calls: AtomicU64::new(0),
                },
            );
        Ok(address)
    }

    /// State that a **weak** reference to `symbol` must resolve to **nothing**, as a real device's
    /// dynamic linker would resolve it.
    ///
    /// # This is the one place a thunk address is the wrong answer, and it is a narrow one
    ///
    /// Every other import gets a slot whose call names it ([`Binding::Unbound`]), because a named
    /// refusal beats a branch to address zero. **A weak undefined symbol inverts that argument**,
    /// and the inversion is a property of what `STB_WEAK` *means*: the reference compiles to a
    /// null test, the guest is required to make it, and zero is the specified answer for "nothing
    /// supplies this". Handing out an address turns a branch the guest was going to skip into a
    /// branch it takes.
    ///
    /// It is therefore not a blanket rule about weak symbols — a weak import a real Android libc
    /// *does* supply should still get a slot, because the guest would have called it on a device.
    /// It is a per-symbol declaration by the compatibility layer, saying **this symbol does not
    /// exist on the platform we are modelling**.
    ///
    /// A **strong** reference to a symbol declared absent still gets a slot. Zero there is a
    /// branch to address zero with no symbol attached to it, which is the failure shape the whole
    /// region exists to replace, and a strong reference means the guest never tests for null.
    ///
    /// `omni_android::bionic::ABSENT_SYMBOLS` is the list, with the guest instructions that
    /// justify it.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if `symbol` already has a slot. A symbol cannot be both bound and
    /// absent: the loader asks once, and two answers means one of them was written by someone who
    /// had not read the other.
    pub fn declare_absent(&self, symbol: &str) -> AbiResult<()> {
        let mut inner = self.inner.lock();
        if let Some(&address) = inner.by_name.get(symbol) {
            return Err(AbiError::Refused {
                symbol: symbol.to_string(),
                address,
                why: format!(
                    "`{symbol}` already has a thunk slot at {address:#x} and cannot also be \
                     declared absent: the loader asks about a symbol once, so this layer would be \
                     giving it two different answers"
                ),
            });
        }
        inner.absent.insert(symbol.to_string());
        Ok(())
    }

    /// Whether a weak reference to `symbol` will be left unresolved.
    #[must_use]
    pub fn is_absent(&self, symbol: &str) -> bool {
        self.inner.lock().absent.contains(symbol)
    }

    /// Bind `symbol` to a handler that runs inside the run loop.
    ///
    /// Declares the symbol if it is not declared yet, so a compatibility layer can be assembled
    /// before anything is loaded.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`].
    pub fn bind_inline(&self, symbol: &str, handler: ImportFn) -> AbiResult<GuestAddr> {
        let address = self.declare_function(symbol)?;
        self.inner.lock().slots.entry(address).and_modify(|slot| {
            slot.binding = Binding::Inline(handler);
        });
        Ok(address)
    }

    /// Bind `symbol` to a handler that runs on the exit path and may call guest code.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`].
    pub fn bind_reentrant(&self, symbol: &str, handler: ReentrantFn) -> AbiResult<GuestAddr> {
        let address = self.declare_function(symbol)?;
        self.inner.lock().slots.entry(address).and_modify(|slot| {
            slot.binding = Binding::Reentrant(handler);
        });
        Ok(address)
    }

    /// Lower the exit-path crossing cap from [`MAX_EXIT_CROSSINGS`].
    ///
    /// **Exists so the cap can be tested.** 2^32 crossings is far too many to reach in a test, and a
    /// limit no test reaches is a limit nobody knows works — Global Constraint 13's distinction
    /// between code that is exercised and a bug that is detected. Zero is clamped to one, since a cap
    /// of zero would refuse the first legitimate call.
    pub fn with_exit_crossing_limit(&self, limit: u64) -> &Self {
        self.inner.lock().exit_crossings = limit.clamp(1, MAX_EXIT_CROSSINGS);
        self
    }

    /// Record which library the importing binary attributes a slot to.
    ///
    /// Idempotent and last-writer-wins, which cannot differ: `.gnu.version_r` gives one symbol one
    /// version record, and the loader asks about a symbol once.
    fn attribute(&self, address: GuestAddr, library: Option<&str>) {
        let Some(library) = library else { return };
        if let Some(slot) = self.inner.lock().slots.get_mut(&address) {
            slot.library = Some(library.to_string());
        }
    }

    /// The address a symbol was given, if it has one.
    #[must_use]
    pub fn address_of(&self, symbol: &str) -> Option<GuestAddr> {
        self.inner.lock().by_name.get(symbol).copied()
    }

    /// Freeze into a [`Boundary`].
    #[must_use]
    pub fn finish(self) -> Arc<Boundary> {
        let inner = self.inner.into_inner();
        Arc::new(Boundary {
            mem: GuestMem::new(self.space),
            region: inner.region,
            slots: inner.slots,
            by_name: inner.by_name,
            sentinel: inner.sentinel,
            exit_crossings: inner.exit_crossings,
            crossings: Mutex::new(Crossings::default()),
            code_watch: CodeWatch::default(),
            census: AtomicBool::new(false),
            last_call: AtomicUsize::new(0),
        })
    }
}

/// **How the loader binds imports to thunk addresses.**
///
/// The loader asks about each of `libroblox.so`'s 565 undefined symbols while it relocates, from
/// `&self`, which is why allocation is behind a mutex.
///
/// Two deliberate asymmetries between functions and data:
///
/// * **Every function gets a slot, whether or not anything implements it.** A function nothing
///   implements is bound to a real address whose call produces [`AbiError::Unbound`] naming it — which
///   is strictly better than leaving it unresolved and bound to null, where the guest's call becomes a
///   branch to address zero with no symbol attached to it. **The one exception is a *weak* reference
///   to a symbol [`declare_absent`](BoundaryBuilder::declare_absent) named**, where zero is not a
///   missing answer but the specified one; that method's documentation has the argument and
///   `omni_android::bionic::ABSENT_SYMBOLS` has the list.
/// * **A data symbol gets one only if it was declared with a size.** There is no size in a
///   [`SymbolRequest`], and `__sF` is an array of three `FILE`s that the guest reaches as
///   `__sF + addend`, so a default pointer-sized cell would be silently too small and the guest's
///   `stderr` would be some other object's bytes. An undeclared data import therefore stays
///   unresolved, which the loader already reports by name in [`Imports::unresolved`].
///
/// [`SymbolRequest`]: omni_elf::loader::SymbolRequest
/// [`Imports::unresolved`]: omni_elf::loader::Imports
impl SymbolProvider for BoundaryBuilder {
    fn name(&self) -> &str {
        "omnidroid-thunks"
    }

    fn resolve(&self, request: &SymbolRequest<'_>) -> Option<SymbolValue> {
        match request.kind {
            SymbolKind::Function | SymbolKind::Unspecified => {
                // A **weak** reference to a symbol this layer has declared absent resolves to
                // nothing, which the loader writes as a null and the guest's own null test then
                // skips. See `declare_absent`. The weakness is checked as well as the name: a
                // strong reference still gets a named slot, because a strong reference has no
                // null test in front of it.
                if request.weak && self.is_absent(request.name) {
                    return None;
                }
                // A failure here is the region running out, which the loader has no channel for. It
                // is reported as "nothing supplied this symbol" rather than swallowed, and the
                // loader's unresolved list then names every symbol that missed out.
                let address = self.declare_function(request.name).ok()?;
                self.attribute(address, request.library);
                Some(SymbolValue { address: address as u64, kind: SymbolKind::Function })
            }
            SymbolKind::Object => self.address_of(request.name).map(|address| {
                self.attribute(address, request.library);
                SymbolValue { address: address as u64, kind: SymbolKind::Object }
            }),
        }
    }
}

/// How many times each path has been taken, so a test can tell them apart (Global Constraint 13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crossings {
    /// Calls serviced on the exit path.
    pub exits: u64,
    /// Calls into guest code the boundary made.
    pub guest_calls: u64,
    /// The deepest nesting reached.
    pub deepest: usize,
}

/// How much cross-context code invalidation has happened — and, in its documentation, the whole
/// of what that mechanism is and is not.
///
/// # Why there is a registry of live contexts at all
///
/// [`GuestCpu::invalidate_code`] is a **per context** operation, which the guest-memory phase
/// recorded as "a narrowing of the window, not a closing of it" when `munmap` and `mprotect`
/// arrived: a second guest thread that had already translated a range kept its own translation of
/// bytes that were no longer there. Closing it needs a registry of live contexts, and that
/// belongs with thread lifecycle — because until there is a `pthread_create` there is no second
/// guest thread to keep a stale translation.
///
/// So: every [`Boundary::run`] registers a queue for the context it is driving (or reuses a
/// [`ContextRegistration`] the caller already holds), and
/// [`ReentrantCall::invalidate_code`] pushes the range onto every **other** registered queue.
/// Each context drains its own queue into its own CPU at the top of every run segment — which is
/// the only place a `&mut dyn GuestCpu` for that context exists. A queue that fills collapses to
/// the whole address space ([`MAX_PENDING_INVALIDATIONS`]), which is *more* invalidation rather
/// than less: over-invalidating costs translation, and dropping a range leaves a context
/// executing bytes that are not there.
///
/// # What remains open, stated exactly
///
/// A context drains at a run-segment boundary, so a guest thread that neither crosses the exit
/// path nor returns from `cpu.run` keeps a stale translation until it does. With
/// [`RunLimit::Unlimited`] and a guest loop that never leaves generated code, that is for ever; a
/// guest thread created through `pthread_create` runs in counted windows, so for one of those it
/// is bounded by a window. Closing it completely needs either a cross-context invalidation the
/// backend does not offer or an asynchronous halt honoured under a counted budget, which
/// `crates/dynarmic-sys/patches/README.md` item 2b measures as not being the case on this pin.
///
/// The window is therefore **narrower** than it was and is not closed, and the difference is
/// written down rather than implied.
///
/// # A detector, not a watch
///
/// Global Constraint 13's distinction: with the broadcast removed, `queued` and `applied` stay at
/// zero under exactly the workload that makes them rise, because nothing else raises them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeInvalidations {
    /// Ranges handed to another context's queue.
    pub queued: u64,
    /// Ranges another context has taken out of its queue and applied to its own CPU.
    pub applied: u64,
    /// Times a queue filled and collapsed to "the whole address space".
    pub overflows: u64,
}

/// How many ranges one context's queue holds before it collapses to the whole address space.
///
/// **A policy number.** A queue that fills is one whose context has not crossed the boundary
/// since 64 other-thread `munmap`/`mprotect` calls, and the collapse is *more* invalidation
/// rather than less — over-invalidating is always safe and only costs translation, where
/// dropping a range silently leaves a context executing bytes that are no longer there.
pub const MAX_PENDING_INVALIDATIONS: usize = 64;

/// One context's pending cross-thread invalidations.
struct CodeQueue {
    /// Fast path: the run loop reads this once per segment and takes the lock only when it is
    /// non-zero or the queue has overflowed.
    pending: AtomicUsize,
    inner: Mutex<QueueInner>,
}

#[derive(Default)]
struct QueueInner {
    ranges: Vec<(GuestAddr, usize)>,
    /// Set when the queue filled: the next drain invalidates the whole guest address space.
    overflowed: bool,
}

/// The registry of live contexts: one queue each, and the counters. See [`CodeInvalidations`].
#[derive(Default)]
struct CodeWatch {
    contexts: Mutex<Vec<(u64, Arc<CodeQueue>)>>,
    next: AtomicU64,
    queued: AtomicU64,
    applied: AtomicU64,
    overflows: AtomicU64,
}

impl CodeWatch {
    fn register(&self) -> (u64, Arc<CodeQueue>) {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let queue = Arc::new(CodeQueue {
            pending: AtomicUsize::new(0),
            inner: Mutex::new(QueueInner::default()),
        });
        self.contexts.lock().push((token, Arc::clone(&queue)));
        (token, queue)
    }

    fn deregister(&self, token: u64) {
        self.contexts.lock().retain(|(held, _)| *held != token);
    }

    /// Hand `range` to every context but `except`.
    fn broadcast(&self, except: u64, range: (GuestAddr, usize)) {
        let contexts = self.contexts.lock();
        for (token, queue) in contexts.iter() {
            if *token == except {
                continue;
            }
            let mut inner = queue.inner.lock();
            if inner.overflowed {
                continue;
            }
            if inner.ranges.len() >= MAX_PENDING_INVALIDATIONS {
                // More invalidation rather than less: the next drain covers the whole space.
                inner.ranges.clear();
                inner.overflowed = true;
                self.overflows.fetch_add(1, Ordering::Relaxed);
            } else {
                inner.ranges.push(range);
                self.queued.fetch_add(1, Ordering::Relaxed);
            }
            queue.pending.store(
                inner.ranges.len() + usize::from(inner.overflowed),
                Ordering::Release,
            );
        }
    }

    fn counts(&self) -> CodeInvalidations {
        CodeInvalidations {
            queued: self.queued.load(Ordering::Relaxed),
            applied: self.applied.load(Ordering::Relaxed),
            overflows: self.overflows.load(Ordering::Relaxed),
        }
    }
}

thread_local! {
    // The queue and token of the context this thread is currently running, if any. A thread-local
    // rather than a parameter because a nested `run_at_depth` -- a guest callback -- drives the
    // SAME CPU, so it must drain the same queue rather than register a second one.
    static CONTEXT: RefCell<Option<(u64, Arc<CodeQueue>)>> = const { RefCell::new(None) };
}

/// Keeps one CPU context in the boundary's registry, so another guest thread's `munmap` or
/// `mprotect` can reach it.
///
/// **Hold one for the life of a long-lived context**, which is what the thread runner does: a
/// guest thread runs in short budget windows and each window is its own [`Boundary::run`], so a
/// registration that lasted one run would be taken off and put back between them — and a range
/// invalidated in that gap would be queued for a context that no longer existed and dropped when
/// the new one registered. See [`CodeInvalidations`].
///
/// A [`Boundary::run`] with no registration already held registers one for the duration of that
/// run. That is correct while it is running, and it is the whole of what is left open here: a
/// range invalidated while a context is **outside** `run` is not applied to it, because there is
/// nothing to apply it to and nothing to remember it with. A context that is not running is not
/// executing stale translations either; it becomes observable only if it starts running again,
/// which is why a context that will do so holds one of these.
pub struct ContextRegistration {
    boundary: Arc<Boundary>,
    token: u64,
    previous: Option<(u64, Arc<CodeQueue>)>,
}

impl core::fmt::Debug for ContextRegistration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ContextRegistration").field("token", &self.token).finish()
    }
}

impl Drop for ContextRegistration {
    fn drop(&mut self) {
        CONTEXT.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
        self.boundary.code_watch.deregister(self.token);
    }
}

// ------------------------------------------------------------------------------- the boundary

/// The frozen boundary: address to symbol to handler, plus the machinery to drive a guest through it.
pub struct Boundary {
    mem: GuestMem,
    region: ThunkRegion,
    slots: BTreeMap<GuestAddr, Slot>,
    by_name: BTreeMap<String, GuestAddr>,
    sentinel: GuestAddr,
    exit_crossings: u64,
    crossings: Mutex<Crossings>,
    /// The live contexts, and what each of them still has to invalidate. See [`CodeWatch`].
    code_watch: CodeWatch,
    /// Whether every crossing counts itself. See [`Boundary::start_census`].
    census: AtomicBool,
    /// The address of the slot the most recent crossing was for. See [`Boundary::last_call`].
    last_call: AtomicUsize,
}

impl Boundary {
    /// Guest memory, checked.
    #[must_use]
    pub fn mem(&self) -> &GuestMem {
        &self.mem
    }

    /// The reserved region.
    #[must_use]
    pub fn region(&self) -> &ThunkRegion {
        &self.region
    }

    /// The address a host-to-guest call returns through.
    #[must_use]
    pub fn sentinel(&self) -> GuestAddr {
        self.sentinel
    }

    /// Every slot, in address order.
    pub fn slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots.values()
    }

    /// The slot a symbol was given.
    #[must_use]
    pub fn slot_named(&self, symbol: &str) -> Option<&Slot> {
        self.by_name.get(symbol).and_then(|address| self.slots.get(address))
    }

    /// Every library the importing binary attributes at least one of its imports to.
    ///
    /// What `dlopen` will and will not issue a handle for. Derived from the guest's own
    /// `DT_VERNEED` while the loader relocated — see [`Slot::library`].
    #[must_use]
    pub fn libraries(&self) -> BTreeSet<&str> {
        self.slots.values().filter_map(|slot| slot.library.as_deref()).collect()
    }

    /// The slot `dlsym(handle, symbol)` should answer with.
    ///
    /// `library` is the scope: `None` is the global one, which searches everything this layer
    /// supplies, and `Some(name)` searches only the imports the guest's own file attributes to
    /// that library. A symbol nothing here supplies answers `None`, which is `dlsym`'s ordinary
    /// "not found" and which the caller is required to test for.
    ///
    /// **A [`Binding::Unbound`] slot is deliberately *not* a hit.** Its address exists so that a
    /// direct call names the symbol; handing it back through `dlsym` would turn a lookup the guest
    /// is prepared to see fail into a pointer it will call later, and the failure would arrive
    /// somewhere unrelated. A data object is a hit: its address is a real object.
    #[must_use]
    pub fn lookup(&self, library: Option<&str>, symbol: &str) -> Option<&Slot> {
        let slot = self.slot_named(symbol)?;
        if matches!(slot.binding, Binding::Unbound) {
            return None;
        }
        match library {
            None => Some(slot),
            Some(name) if slot.library.as_deref() == Some(name) => Some(slot),
            Some(_) => None,
        }
    }

    /// How many times each path has been taken.
    #[must_use]
    pub fn crossings(&self) -> Crossings {
        *self.crossings.lock()
    }

    /// How much cross-context code invalidation this boundary has done. See
    /// [`CodeInvalidations`], whose documentation is where the mechanism is described.
    #[must_use]
    pub fn code_invalidations(&self) -> CodeInvalidations {
        self.code_watch.counts()
    }

    /// Count every crossing **per symbol**, from now until [`stop_census`](Boundary::stop_census).
    ///
    /// # Why this is switched on rather than always on
    ///
    /// The question it answers is the most valuable output M3 has: *which* imports the guest
    /// actually calls, against the 188 a static closure predicted. D17 is explicit that 188 is a
    /// **lower bound** — 17,698 indirect call sites could not be followed — so the only way to
    /// know what the engine really reaches is to watch it reach.
    ///
    /// But the counter sits on the hot path. D17 measured an inline crossing at **≈33 ns**, and an
    /// unconditional `lock xadd` there is a real fraction of that on every one of the imported
    /// calls all 3,594 initializers make. So the fast path pays a relaxed load of one `bool` and a
    /// branch that predicts perfectly, and the increment happens only for a host that asked for
    /// it. A timing run and a census run are then two different runs, which is the honest
    /// arrangement: a figure measured with the census on is a figure about the census.
    ///
    /// Counts are **not** reset — a host that wants a delta takes a [`census`](Boundary::census)
    /// before and after — so starting it twice resumes rather than restarts.
    pub fn start_census(&self) {
        self.census.store(true, Ordering::Relaxed);
    }

    /// Stop counting. Whatever was counted stays readable.
    pub fn stop_census(&self) {
        self.census.store(false, Ordering::Relaxed);
    }

    /// Every symbol the guest has called since the census was started, with its count.
    ///
    /// `None` when the census has never been started, because "no symbol was called" and "nobody
    /// was counting" are different statements and a caller that could not tell them apart would
    /// report an empty census as a finding.
    #[must_use]
    pub fn census(&self) -> Option<BTreeMap<&str, u64>> {
        if !self.census.load(Ordering::Relaxed) {
            // Started at least once leaves a count behind; never started leaves every slot at
            // zero. Distinguished by the flag being *currently* off with nothing counted, which
            // is why `stop_census` does not clear the counts.
            if self.slots.values().all(|slot| slot.calls() == 0) {
                return None;
            }
        }
        Some(
            self.slots
                .values()
                .filter(|slot| slot.calls() > 0)
                .map(|slot| (slot.symbol.as_str(), slot.calls()))
                .collect(),
        )
    }

    /// The symbol the guest most recently branched into, while the census was on.
    ///
    /// **The one thing that identifies a guest parked inside a handler.** A guest blocked on a
    /// futex, a condition variable or a join executes no guest instructions, so no budget
    /// expires and nothing on its own thread will report again; another thread reading this is
    /// what says which import it went into. Recorded only under the census, for the reason
    /// [`start_census`](Boundary::start_census) gives about the 33 ns path.
    #[must_use]
    pub fn last_call(&self) -> Option<&Slot> {
        let address = self.last_call.load(Ordering::Relaxed);
        if address == 0 {
            return None;
        }
        self.slots.get(&address)
    }

    /// Charge one crossing to a slot, if a host asked for the census.
    #[inline]
    fn count(&self, slot: &Slot) {
        if self.census.load(Ordering::Relaxed) {
            slot.calls.fetch_add(1, Ordering::Relaxed);
            self.last_call.store(slot.address, Ordering::Relaxed);
        }
    }

    /// The token an inline thunk is registered with: this boundary's own address.
    ///
    /// # Panics
    ///
    /// Never. The cast cannot fail: `Arc::as_ptr` is a valid pointer and `usize` holds a pointer on
    /// every target this crate builds for.
    #[must_use]
    pub fn context(self: &Arc<Self>) -> ThunkContext {
        ThunkContext(Arc::as_ptr(self) as usize)
    }

    /// Register every slot with a CPU context, and arm the callback sentinel.
    ///
    /// Inline where the backend offers it and on the exit path where it does not — which is how the
    /// ARM64-native path stays expressible without this crate knowing anything about either backend.
    /// A backend answering [`Capabilities::inline_thunks`](omni_cpu::Capabilities) `false` gets a
    /// boundary that is three times slower per call (D17) and identical in behaviour.
    ///
    /// # Errors
    ///
    /// [`AbiError::Cpu`] if the backend could not install a thunk.
    pub fn install(self: &Arc<Self>, cpu: &mut dyn GuestCpu) -> AbiResult<()> {
        let inline_ok = cpu.capabilities().inline_thunks;
        let context = self.context();
        for slot in self.slots.values() {
            match slot.binding {
                Binding::Inline(_) if inline_ok => {
                    cpu.add_inline_thunk(slot.address, inline_trampoline, context)?;
                }
                // Everything else exits: a handler that may call guest code, a slot nothing
                // implements, a data address somebody called, and — on a backend with no in-loop
                // dispatch — the inline handlers too.
                _ => cpu.add_thunk(slot.address)?,
            }
        }
        cpu.set_return_sentinel(self.sentinel)?;
        Ok(())
    }

    /// Run guest code from `from`, servicing every thunk it reaches, until it stops for another
    /// reason.
    ///
    /// Returns the [`ExitReason`] that was not a thunk. [`ExitReason::Thunk`] never escapes: it is
    /// either serviced or turned into a typed [`AbiError`].
    ///
    /// # Errors
    ///
    /// Any [`AbiError`]. [`AbiError::Unbound`] is the one a caller running initializers should expect
    /// most, and it names the symbol and the guest address.
    pub fn run(
        self: &Arc<Self>,
        cpu: &mut dyn GuestCpu,
        from: GuestAddr,
        limit: RunLimit,
    ) -> AbiResult<ExitReason> {
        // Anything left in the channel is from an earlier run on this thread that did not consume it.
        // Dropped rather than reported here, because reporting it would blame this run for a failure
        // that happened in another one.
        let _ = take_pending();
        // Register this run's context for the length of the run, so that another guest thread's
        // `munmap` or `mprotect` can reach it — unless the caller already holds a registration
        // for this context, which a long-lived one does. Registering a second would give this
        // thread two queues and drain only the inner one.
        let _context = if CONTEXT.with(|cell| cell.borrow().is_some()) {
            None
        } else {
            Some(self.watch_context())
        };
        self.run_at_depth(cpu, from, limit, 0)
    }

    /// Call a guest function **from the host**, AAPCS64, and come back with its return value.
    ///
    /// # Why this exists, and why it is not [`run`](Boundary::run)
    ///
    /// [`run`](Boundary::run) enters guest code at an address and reports why it stopped. That is
    /// enough to *start* a guest function and not enough to *call* one: the caller still has to
    /// arm the return sentinel in `X30`, place AAPCS64 arguments, tell a return through the
    /// sentinel apart from a return somewhere else, and put the context back afterwards. Every
    /// caller that wants a call rather than a run would write those four things again, and
    /// [`ReentrantCall::call_guest`] already had them — but only from *inside* a thunk crossing.
    ///
    /// Two callers want the host-initiated direction and neither is inside one:
    ///
    /// * **The M3 gate**, which runs 3,594 `init_array` entries in order with nothing having
    ///   crossed the boundary yet.
    /// * **`pthread_key` destructors at thread exit** (D24), which run after the guest thread's
    ///   entry point has returned, so there is no crossing left to be inside of. That is still
    ///   not wired up; this is the API it was missing.
    ///
    /// `caller` names whoever is asking, and it is what the errors carry — `init_array[1729]`
    /// tells a reader which of 3,594 stopped, where a bare guest address does not.
    ///
    /// # It does not weaken the re-entrancy property, and that is checkable rather than asserted
    ///
    /// D18 makes "a handler cannot re-enter its own thread's guest" a **type** property, and task
    /// 2's review verified it by trying to obtain two live mutable CPU references. This method
    /// takes a `&mut dyn GuestCpu` **the caller already owns**, exactly as
    /// [`run`](Boundary::run) does, so it can only be reached by something holding one:
    ///
    /// * [`ImportCall`] has no CPU and no boundary at all, so nothing on the inline path can
    ///   reach this.
    /// * [`ReentrantCall`] holds the only `&mut dyn GuestCpu` for the calling thread, and the
    ///   borrow checker will not produce a second — which is the same argument
    ///   [`ReentrantCall::boundary`] rests on, and the reason `pthread_create` could be given the
    ///   whole boundary without widening anything.
    ///
    /// The body it shares with [`ReentrantCall::call_guest`] is a private function, so this adds
    /// no way to enter guest code that did not already exist; it adds a *caller* that is the host.
    ///
    /// # What is saved and restored
    ///
    /// As [`ReentrantCall::call_guest`]: all of `X0`-`X30`, `SP`, `PC`, the condition flags,
    /// `V0`-`V31`, and whatever sentinel was armed. A caller making a sequence of these — the
    /// gate makes 3,594 — therefore gets each one starting from the register state it set up,
    /// rather than from the leftovers of the previous initializer.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadCallbackStack`] if `SP` is not usable or more than eight arguments of one
    /// bank were passed; [`AbiError::GuestCallbackStopped`] if the guest did not return through
    /// the sentinel — a fault, an unsupported instruction, a budget running out; and anything the
    /// guest's own imported calls raise, which propagates with the *import's* symbol named rather
    /// than with `caller`.
    pub fn call_guest(
        self: &Arc<Self>,
        cpu: &mut dyn GuestCpu,
        caller: &str,
        target: GuestAddr,
        args: &[GuestArg],
        limit: RunLimit,
    ) -> AbiResult<GuestReturn> {
        // As `run`: anything left in the channel is from an earlier run on this thread that did
        // not consume it, and reporting it here would blame this call for another one's failure.
        let _ = take_pending();
        let _context = if CONTEXT.with(|cell| cell.borrow().is_some()) {
            None
        } else {
            Some(self.watch_context())
        };
        {
            let mut crossings = self.crossings.lock();
            crossings.guest_calls += 1;
        }
        let saved = SavedState::capture(cpu);
        // Depth zero, the same depth `run` enters at: this call is not nested inside a thunk
        // crossing, so a handler it reaches gets depth 1 exactly as one reached from `run` does.
        // Giving the host entry a depth of its own would spend one of `MAX_GUEST_DEPTH`'s levels
        // on the outermost frame and make the two entry points disagree about how deep a guest is.
        let result = enter_guest(self, cpu, caller, target, args, limit, 0);
        saved.restore(cpu);
        result
    }

    /// Keep this thread's CPU context in the registry until the returned guard is dropped.
    ///
    /// See [`ContextRegistration`]: a context that runs in repeated short windows holds one
    /// across all of them, so that a range another guest thread invalidates between two windows
    /// is still applied.
    #[must_use]
    pub fn watch_context(self: &Arc<Self>) -> ContextRegistration {
        let (token, queue) = self.code_watch.register();
        let previous = CONTEXT.with(|cell| cell.borrow_mut().replace((token, queue)));
        ContextRegistration { boundary: Arc::clone(self), token, previous }
    }

    /// Apply whatever another context queued for this one, before it runs again.
    ///
    /// Called at the top of every run segment, which is the only place a `&mut dyn GuestCpu` for
    /// this context exists. The common case is one relaxed load.
    fn drain_invalidations(&self, cpu: &mut dyn GuestCpu) -> AbiResult<()> {
        let queue = CONTEXT.with(|cell| cell.borrow().as_ref().map(|(_, q)| Arc::clone(q)));
        let Some(queue) = queue else { return Ok(()) };
        if queue.pending.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let (ranges, overflowed) = {
            let mut inner = queue.inner.lock();
            let overflowed = core::mem::take(&mut inner.overflowed);
            let ranges = core::mem::take(&mut inner.ranges);
            queue.pending.store(0, Ordering::Release);
            (ranges, overflowed)
        };
        if overflowed {
            // The whole space, which is more invalidation rather than less. A backend that has
            // translated nothing in most of it does nothing for most of it.
            let space = self.mem.space();
            let range = omni_cpu::GuestRange::new(space.base(), space.len())?;
            cpu.invalidate_code(range)?;
            self.code_watch.applied.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        for (address, len) in ranges {
            let range = omni_cpu::GuestRange::new(address, len)?;
            cpu.invalidate_code(range)?;
            self.code_watch.applied.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn run_at_depth(
        self: &Arc<Self>,
        cpu: &mut dyn GuestCpu,
        from: GuestAddr,
        limit: RunLimit,
        depth: usize,
    ) -> AbiResult<ExitReason> {
        let halt = cpu.halt_handle();
        let mut pc = from;
        let mut crossings = 0u64;
        // **The caller's budget, spent down across crossings rather than handed out afresh to each
        // one.** Every exit-path crossing returns to Rust and the next `cpu.run` starts a new counted
        // run, so passing `limit` unchanged would give a guest that crosses N times N times the
        // allowance the caller asked for — and Global Constraint 11's point is that a bound is only as
        // trustworthy as its least-validated input. Found by the mutation harness *hanging* rather
        // than failing: with `Binding::Unbound` mutated to return quietly, a two-instruction guest loop
        // through the boundary ran for ever under a counted budget.
        //
        // Only ever shrinks, so it cannot grow past what the caller passed and cannot reach the value
        // D16 warns about — `GuestCpu::run` clamps its own slices below `i64::MAX` in any case.
        let mut remaining = limit;
        // What the whole run has executed, across every segment. The backend's own
        // `StepLimitReached` carries one segment's count, and a caller that asked for a budget over
        // the run wants it over the run.
        let mut spent = 0u64;
        loop {
            // The containment for a guest that loops through the *exit* path: each crossing returns
            // to Rust, so the backend's own budget never expires. Checked before the run rather than
            // after, so a halt requested while the boundary was servicing a call is honoured before
            // the guest is let go again.
            if halt.is_requested() {
                return Ok(ExitReason::Halted { pc });
            }
            if crossings >= self.exit_crossings {
                return Err(AbiError::CrossingLimit {
                    crossings,
                    limit: self.exit_crossings,
                    pc,
                });
            }
            // Another guest thread may have unmapped or reprotected a range this context has
            // translated. This is the only place a `&mut dyn GuestCpu` for it exists, so it is
            // the only place that can be put right; see [`CodeWatch`] for what that does and does
            // not close.
            self.drain_invalidations(cpu)?;
            let exit = cpu.run(pc, remaining)?;
            // Saturating, because a backend counts at the end of a unit it handles as a whole and may
            // overshoot its slice. A counter that wrapped here would turn a bounded run into an
            // unbounded one.
            spent = spent.saturating_add(cpu.last_run_instructions());
            let site = match exit {
                ExitReason::Thunk { pc: site } => site,
                // **A branch into the region that was not a call to a slot's first instruction.**
                //
                // The function area is not executable (see `crate::region`), so the backend refuses
                // the fetch and this is what the guest gets: a typed fault naming the address. It is
                // the *right* stop and the wrong *message* — "a guest read fault at 0x…" tells a
                // reader nothing about which symbol's slot it was four bytes into. So it is
                // re-described here, where the table can say.
                //
                // Doing it this way rather than by registering all four words of every slot is what
                // makes the answer exact for an address that is not even word-aligned, and it costs
                // no registrations at all.
                ExitReason::MemoryFault { address, access: AccessKind::Execute, .. }
                    if self.region.holds_function(address) || self.region.holds_data(address) =>
                {
                    return Err(self.slot_at(address).err().unwrap_or_else(|| {
                        AbiError::NoSuchThunk {
                            address,
                            start: self.region.functions_start(),
                            end: self.region.functions_end(),
                        }
                    }));
                }
                // **Charged only on the exits that end the run**, so that a terminal exit landing on
                // the budget's last instruction is reported as what it is. The first version charged
                // immediately after `cpu.run` and pre-empted whenever the allowance had run out, which
                // turned a `Returned` into a `StepLimitReached` — and, worse, a `MemoryFault` into one
                // too, since a fault is *not* resumable and a step limit is, so a caller would have
                // resumed a faulting guest.
                ExitReason::StepLimitReached { pc: at, .. } => {
                    return Ok(ExitReason::StepLimitReached { pc: at, executed: spent })
                }
                other => return Ok(other),
            };
            // An inline handler that failed left its reason here and deferred.
            //
            // Checked BEFORE the budget, and that order is load-bearing. A handler that has already
            // run and failed is not "unserviced and resumable": if the allowance happened to run out
            // on the same crossing, the budget arm below would return `StepLimitReached` and this
            // error would be left in the thread-local for the next `Boundary::run` to drop on the
            // floor. The caller would be told "budget expired, resumable" when an imported call had
            // actually failed — and resuming would enter the handler a SECOND time, which is exactly
            // the once-only property `boundary-A6` exists to protect.
            //
            // This is the same class as the defect the late budget fix was itself for: a
            // non-resumable condition reported as a resumable one.
            //
            // It is also checked before the slot lookup in `service_exit`, which would otherwise
            // report the symbol as merely unbound.
            if let Some(error) = take_pending() {
                return Err(error);
            }
            // Only now, having decided to go round again, is the allowance spent down. A budget that
            // has run out stops the guest *at the thunk*, unserviced and resumable, which is the
            // honest stop: servicing the call and then refusing to resume would leave the caller
            // unable to say what happened.
            if let RunLimit::Instructions(allowance) = remaining {
                let left = allowance.saturating_sub(cpu.last_run_instructions());
                if left == 0 {
                    return Ok(ExitReason::StepLimitReached { pc: site, executed: spent });
                }
                remaining = RunLimit::Instructions(left);
            }
            crossings += 1;
            pc = self.service_exit(cpu, site, depth)?;
        }
    }

    /// Service one exit-path crossing and return where the guest resumes.
    fn service_exit(
        self: &Arc<Self>,
        cpu: &mut dyn GuestCpu,
        site: GuestAddr,
        depth: usize,
    ) -> AbiResult<GuestAddr> {
        let slot = self.slot_at(site)?;
        let resume = cpu.x(XReg::new(30).expect("X30 exists")) as GuestAddr;
        self.crossings.lock().exits += 1;
        // Charged before the binding is looked at, so that an `Unbound` symbol the guest really
        // reached appears in the census. That one is the whole point: an import nobody predicted,
        // named by the guest having branched to it.
        self.count(slot);
        match slot.binding {
            Binding::Unbound => Err(AbiError::Unbound {
                symbol: slot.symbol.clone(),
                address: slot.address,
            }),
            Binding::Data => Err(AbiError::DataSymbolCalled {
                symbol: slot.symbol.clone(),
                address: slot.address,
            }),
            Binding::Inline(handler) => {
                // Reached either because the backend has no in-loop dispatch, or because a handler
                // deferred for a reason that has since been consumed. Serviced with the same handler
                // over the CPU's own register file, so the two paths cannot disagree about a value.
                let mut regs = CpuRegs { cpu };
                let mut call = ThunkCall::new(&mut regs, site, ThunkContext::default());
                let mut import = ImportCall { symbol: &slot.symbol, call: &mut call, mem: &self.mem };
                handler(&mut import)?;
                Ok(resume)
            }
            Binding::Reentrant(handler) => {
                let args = {
                    let mut regs = CpuRegs { cpu };
                    let call = ThunkCall::new(&mut regs, site, ThunkContext::default());
                    // Snapshotted before the handler runs, because the handler may call guest code
                    // and a guest callback clobbers the argument registers.
                    ArgRegs::capture(&call)
                };
                let mut reentrant = ReentrantCall {
                    symbol: &slot.symbol,
                    address: slot.address,
                    boundary: self,
                    cpu,
                    args,
                    depth,
                };
                handler(&mut reentrant)?;
                Ok(resume)
            }
        }
    }

    /// The slot at a guest address, or the typed error that says why there is not one.
    ///
    /// This is where the two hostile branch shapes are told apart, and they must be: an address four
    /// bytes into a slot is a relocation applied at the wrong width or a guest jumping into the middle
    /// of a thunk, and reporting it as "unbound symbol" would send a reader looking for a missing
    /// implementation that is not missing.
    fn slot_at(&self, address: GuestAddr) -> AbiResult<&Slot> {
        if let Some(slot) = self.slots.get(&address) {
            return Ok(slot);
        }
        // An address inside a data object rather than at its start: the guest branched into
        // `__sF + 8`, or a relocation went in at the wrong width. Named against the object it is
        // inside, which is the only thing that identifies it.
        if self.region.holds_data(address) {
            if let Some(slot) = self
                .slots
                .range(..=address)
                .next_back()
                .map(|(_, slot)| slot)
                .filter(|slot| matches!(slot.binding, Binding::Data))
            {
                return Err(AbiError::DataSymbolCalled {
                    symbol: slot.symbol.clone(),
                    address,
                });
            }
        }
        match self.region.slot_of(address) {
            Some((slot, offset)) if offset != 0 => match self.slots.get(&slot) {
                Some(found) => Err(AbiError::MidThunk {
                    symbol: found.symbol.clone(),
                    slot,
                    address,
                    offset,
                }),
                None => Err(AbiError::NoSuchThunk {
                    address,
                    start: self.region.functions_start(),
                    end: self.region.functions_end(),
                }),
            },
            _ => Err(AbiError::NoSuchThunk {
                address,
                start: self.region.functions_start(),
                end: self.region.functions_end(),
            }),
        }
    }

    /// Service one inline crossing, from inside the run loop.
    fn service_inline(&self, call: &mut ThunkCall<'_>) {
        let Some(slot) = self.slots.get(&call.address()) else {
            // Cannot normally happen — the address was registered from this very table — but the
            // honest answer to "I do not know what this is" is to hand it to the caller, which turns
            // it into `NoSuchThunk` with the region's bounds in it.
            call.defer_to_caller();
            return;
        };
        self.count(slot);
        let Binding::Inline(handler) = slot.binding else {
            call.defer_to_caller();
            return;
        };
        let mut import = ImportCall { symbol: &slot.symbol, call, mem: &self.mem };
        if let Err(error) = handler(&mut import) {
            // No return channel from inside the run loop, so the error goes in the thread's channel
            // and the call becomes an ordinary exit at the same address. Nothing is written to the
            // guest's return register, which is the point: Global Constraint 1's failure shape is a
            // plausible value, and there is none here.
            record_pending(error);
            import.call.defer_to_caller();
        }
    }
}

/// The one handler every inline thunk is registered with.
///
/// A single `fn` for all 170 symbols, because [`ThunkCall::address`](omni_cpu::ThunkCall::address) is
/// what identifies the symbol and [`ThunkContext`] is what reaches the table.
fn inline_trampoline(call: &mut ThunkCall<'_>) {
    let context = call.context().0;
    if context == 0 {
        call.defer_to_caller();
        return;
    }
    // SAFETY: `context` is `Arc::as_ptr` of the very `Boundary` that registered this thunk, taken in
    // `Boundary::context` and passed to `add_inline_thunk`. The `Arc` is alive for as long as the
    // registration is: `install` takes `&Arc<Self>`, and a caller that dropped the last `Arc` while a
    // CPU context still had its thunks registered would have dropped the table this reads. That is the
    // invariant this boundary's public API is shaped around — `install` and `run` both take
    // `&Arc<Self>`, so a caller holds one across every call — and it is the reason `ThunkContext` is an
    // opaque `usize` in `omni-cpu` rather than a reference: the crate that can make this argument is
    // this one. No `&mut` to the `Boundary` exists anywhere, so forming a shared reference is sound
    // even with guest threads running.
    let boundary: &Boundary = unsafe { &*(context as *const Boundary) };
    boundary.service_inline(call);
}

// --------------------------------------------------------------------------- the two call shapes

/// A guest call being serviced **inside** the run loop.
///
/// Holds the register file and guest memory. It deliberately holds **no CPU**: see the module docs.
pub struct ImportCall<'a, 'c> {
    symbol: &'a str,
    call: &'a mut ThunkCall<'c>,
    mem: &'a GuestMem,
}

impl ImportCall<'_, '_> {
    /// The symbol being serviced.
    #[must_use]
    pub fn symbol(&self) -> &str {
        self.symbol
    }

    /// Its thunk address.
    #[must_use]
    pub fn address(&self) -> GuestAddr {
        self.call.address()
    }

    /// Checked guest memory.
    #[must_use]
    pub fn mem(&self) -> &GuestMem {
        self.mem
    }

    /// The guest address this call will return to, which is `X30`.
    ///
    /// **Not the same thing as [`address`](ImportCall::address)**, which is the thunk's own slot:
    /// every call to one symbol reports the same thunk, and this reports the *call site*. A guest
    /// reached by `BL` leaves its return address in `X30`, so this is one instruction past the
    /// call -- which is what a reader with the binary wants, because it lands inside the calling
    /// function rather than at its entry.
    ///
    /// It is a **guest** value and nothing here can vouch for it: a callee that has already
    /// clobbered `X30`, or a `BR` rather than a `BL`, gives something that is not a return
    /// address. Every handler that uses it reads it first, before anything can run, and treats it
    /// as a diagnostic rather than as control flow.
    #[must_use]
    pub fn caller(&self) -> GuestAddr {
        self.call.x(30) as GuestAddr
    }

    /// Name this call and an argument, for an error message.
    #[must_use]
    pub fn blame(&self, argument: usize) -> Blame<'_> {
        Blame::new(self.symbol, self.call.address(), argument)
    }

    /// Start reading arguments.
    #[must_use]
    pub fn args(&self) -> Args<'_> {
        Args::new(&*self.call, self.mem, self.blame(0))
    }

    /// Continue into the variadic part, where the named arguments stopped.
    ///
    /// `consumed` and `overflow` come from [`Args::consumed`] and [`Args::overflow`] on the cursor
    /// that read the named arguments, so a handler cannot get the split point wrong by counting its
    /// own parameters. `first_variadic` is the argument index the errors should blame.
    #[must_use]
    pub fn varargs(
        &self,
        consumed: (u32, u32),
        overflow: GuestAddr,
        first_variadic: usize,
    ) -> VarArgs<'_> {
        VarArgs::new(&*self.call, self.mem, self.blame(first_variadic), consumed, overflow)
    }

    /// Start writing the return value. Take this *after* the arguments have been read.
    #[must_use]
    pub fn ret(&mut self) -> Ret<'_> {
        Ret::new(&mut *self.call)
    }
}

/// A guest call being serviced **outside** the run loop, where guest code may be re-entered.
pub struct ReentrantCall<'a> {
    symbol: &'a str,
    address: GuestAddr,
    boundary: &'a Arc<Boundary>,
    cpu: &'a mut dyn GuestCpu,
    args: ArgRegs,
    depth: usize,
}

impl ReentrantCall<'_> {
    /// The symbol being serviced.
    #[must_use]
    pub fn symbol(&self) -> &str {
        self.symbol
    }

    /// Its thunk address.
    #[must_use]
    pub fn address(&self) -> GuestAddr {
        self.address
    }

    /// Checked guest memory.
    #[must_use]
    pub fn mem(&self) -> &GuestMem {
        &self.boundary.mem
    }

    /// Name this call and an argument.
    #[must_use]
    pub fn blame(&self, argument: usize) -> Blame<'_> {
        Blame::new(self.symbol, self.address, argument)
    }

    /// Start reading arguments, **from the snapshot taken before the handler ran**.
    ///
    /// Which is the only thing that makes this path safe to read arguments on: a handler that has
    /// already called [`call_guest`](ReentrantCall::call_guest) would otherwise read the callback's
    /// leftovers out of `X0`.
    #[must_use]
    pub fn args(&self) -> Args<'_> {
        Args::new(&self.args, &self.boundary.mem, self.blame(0))
    }

    /// Continue into the variadic part. As [`ImportCall::varargs`], over the argument snapshot.
    #[must_use]
    pub fn varargs(
        &self,
        consumed: (u32, u32),
        overflow: GuestAddr,
        first_variadic: usize,
    ) -> VarArgs<'_> {
        VarArgs::new(
            &self.args,
            &self.boundary.mem,
            self.blame(first_variadic),
            consumed,
            overflow,
        )
    }

    /// Write the return value.
    ///
    /// A closure rather than a returned [`Ret`] because the sink over a `&mut dyn GuestCpu` has to
    /// live somewhere for the duration, and the alternative — handing out a `&mut dyn RetSink` to a
    /// boxed wrapper — leaks one box per imported call. The *same* [`Ret`] is used as the inline path
    /// uses, so the two paths cannot disagree about which register a `double` goes in.
    pub fn ret(&mut self, write: impl FnOnce(Ret<'_>)) {
        let mut sink = CpuSink { cpu: &mut *self.cpu };
        write(Ret::new(&mut sink));
    }

    /// How deep in the boundary this call is. Zero at the outermost.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// The boundary itself, so a handler can drive a **different** guest thread through it.
    ///
    /// **Only `pthread_create` needs this, and it is on the exit path only.** A new guest thread
    /// is a new [`GuestCpu`] with the whole thunk table installed on it and its own run loop, and
    /// [`install`](Boundary::install) and [`run`](Boundary::run) both take `&Arc<Boundary>` — so
    /// without this a handler could not start one.
    ///
    /// It does **not** weaken the re-entrancy property D18 makes a type property.
    /// [`ImportCall`] has no boundary and no CPU, so nothing on the inline path can reach this.
    /// And what it hands back cannot re-enter *this* thread's guest either: `Boundary::run` needs
    /// a `&mut dyn GuestCpu`, this call already holds the only one for the calling thread, and the
    /// borrow checker will not produce a second. The only CPU a handler can drive through it is
    /// one it has just created, which is precisely the capability `pthread_create` is.
    #[must_use]
    pub fn boundary(&self) -> &Arc<Boundary> {
        self.boundary
    }

    /// Discard any translated code covering `[address, address + len)`.
    ///
    /// **Only reachable from the exit path, and that is the point.** A handler that changes what
    /// is mapped at a guest address — `munmap`, `mprotect`, an `mmap` reusing addresses a previous
    /// mapping held — leaves the backend holding translations of bytes that are no longer there.
    /// [`ImportCall`] has no CPU at all, deliberately (see the module docs), so an inline handler
    /// could not do this even if it were safe for one to change the address space, which task 2's
    /// review finding F9 establishes it is not.
    ///
    /// A zero length is a no-op rather than an error: a caller that has just rounded a guest's
    /// length to pages may legitimately have nothing to invalidate, and
    /// [`GuestRange`](omni_cpu::GuestRange) refuses an empty range.
    ///
    /// **Per context, and what closes the rest of it is a registry.**
    /// [`GuestCpu::invalidate_code`] is documented as a per-context operation, so this call
    /// reaches only the context the calling guest thread is on. Phase 2 recorded that as "a
    /// narrowing of the window, not a closing of it" and said a registry of live contexts
    /// belonged with thread lifecycle. It is here now: the range is also **queued for every other
    /// live context**, each of which applies it at the top of its next run segment.
    /// [`CodeInvalidations`] has what that closes and the one case it does not.
    ///
    /// # Errors
    ///
    /// [`AbiError::Cpu`] if the range is unusable or the backend refused.
    pub fn invalidate_code(&mut self, address: GuestAddr, len: usize) -> AbiResult<()> {
        if len == 0 {
            return Ok(());
        }
        let range = omni_cpu::GuestRange::new(address, len)?;
        // This context first, synchronously, because the caller is about to return into it.
        self.cpu.invalidate_code(range)?;
        // Then every other live context, which applies it at the top of its next run segment.
        let me = CONTEXT.with(|cell| cell.borrow().as_ref().map(|(token, _)| *token));
        self.boundary.code_watch.broadcast(me.unwrap_or(u64::MAX), (address, len));
        Ok(())
    }

    /// Call a guest function, AAPCS64, and come back with its return value.
    ///
    /// The mirror direction: a `qsort` comparator, an `__cxa_atexit` handler, a `pthread_once`
    /// initialiser, a `pthread_key_create` destructor, a `dl_iterate_phdr` callback, a `pthread_create`
    /// entry point.
    ///
    /// # What is saved and restored
    ///
    /// All of `X0`-`X30`, `SP`, `PC`, the condition flags, `V0`-`V31`, and whatever sentinel was
    /// armed. A guest callback may clobber every one of those, and the outer guest frame this call is
    /// suspended inside expects to find them as it left them.
    ///
    /// # Errors
    ///
    /// [`AbiError::TooDeep`] past [`MAX_GUEST_DEPTH`]; [`AbiError::BadCallbackStack`] if `SP` is not
    /// usable; [`AbiError::GuestCallbackStopped`] if the guest did not return through the sentinel —
    /// a fault, an unsupported instruction, a budget running out; and anything the callback's own
    /// imported calls raise, which propagates with the *inner* symbol named.
    pub fn call_guest(
        &mut self,
        target: GuestAddr,
        args: &[GuestArg],
        limit: RunLimit,
    ) -> AbiResult<GuestReturn> {
        let depth = self.depth + 1;
        if depth > MAX_GUEST_DEPTH {
            return Err(AbiError::TooDeep {
                symbol: self.symbol.to_string(),
                address: self.address,
                depth,
                limit: MAX_GUEST_DEPTH,
            });
        }
        {
            let mut crossings = self.boundary.crossings.lock();
            crossings.guest_calls += 1;
            crossings.deepest = crossings.deepest.max(depth);
        }

        let saved = SavedState::capture(self.cpu);
        let result =
            enter_guest(self.boundary, self.cpu, self.symbol, target, args, limit, depth);
        // Restored on every path, including the error ones: a handler that reports a failed callback
        // still leaves the outer guest frame runnable, and a caller that decides to carry on must not
        // be carrying on with the callback's registers.
        saved.restore(self.cpu);
        result
    }

}

// ------------------------------------------------- one crossing into guest code, from either side

/// The body of a host-to-guest call, shared by [`ReentrantCall::call_guest`] and
/// [`Boundary::call_guest`].
///
/// **One copy, because the two entry points differ only in who is asking.** A second
/// implementation for the host-initiated path is how the two would come to disagree about the
/// sentinel, the stack check or which register a `double` goes in — and the whole reason the exit
/// path and the inline path share [`Ret`] is that a boundary with two marshallers has two answers.
///
/// `caller` and `caller_address` are what the errors name: a thunk's symbol and slot for the
/// re-entrant path, and whatever the host called itself for the other one.
fn enter_guest(
    boundary: &Arc<Boundary>,
    cpu: &mut dyn GuestCpu,
    caller: &str,
    target: GuestAddr,
    args: &[GuestArg],
    limit: RunLimit,
    depth: usize,
) -> AbiResult<GuestReturn> {
    let sp = cpu.sp();
    if sp % 16 != 0 {
        return Err(AbiError::BadCallbackStack {
            symbol: caller.to_string(),
            target,
            sp,
            why: "AArch64 requires SP to be 16-byte aligned at a public interface, and every \
                  SP-relative access in the callee's prologue assumes it",
        });
    }
    // The callee's prologue will push below `SP`, so the stack has to be there. Checked rather
    // than assumed: a guest whose stack has overflowed would otherwise have its callback fault at
    // an address nothing here chose, and the failure would be reported as the callback's.
    boundary
        .mem
        .checked_ptr(sp.saturating_sub(16), 16, true, Blame::new(caller, target, 0))
        .map_err(|_| AbiError::BadCallbackStack {
            symbol: caller.to_string(),
            target,
            sp,
            why: "the 16 bytes below SP are not writable guest memory, so the callee's own \
                  prologue would fault",
        })?;

    place_arguments(cpu, caller, target, args)?;
    let sentinel = boundary.sentinel;
    let previous = cpu.return_sentinel();
    cpu.set_return_sentinel(sentinel)?;
    cpu.set_x(XReg::new(30).expect("X30 exists"), sentinel as u64);

    let outcome = boundary.run_at_depth(cpu, target, limit, depth);

    // Put the outer sentinel back before anything else can go wrong, so an error path cannot
    // leave the context armed on the callback's sentinel.
    if let Some(previous) = previous {
        cpu.set_return_sentinel(previous)?;
    }
    match outcome? {
        ExitReason::Returned { pc } if pc == sentinel => Ok(GuestReturn {
            x0: cpu.x(XReg::new(0).expect("X0 exists")),
            x1: cpu.x(XReg::new(1).expect("X1 exists")),
            v0: cpu.v(VReg::new(0).expect("V0 exists")),
        }),
        exit => Err(AbiError::GuestCallbackStopped {
            symbol: caller.to_string(),
            target,
            exit,
        }),
    }
}

/// AAPCS64 in the outgoing direction: integers in `X0`-`X7`, floating point in `V0`-`V7`.
///
/// **Nothing goes on the stack.** Every callback shape in the reachable set takes at most three
/// arguments — a comparator takes two pointers, a `dl_iterate_phdr` callback takes three, a
/// thread entry point takes one, an `init_array` entry takes `(argc, argv, envp)` — so the stack
/// path would be code with no caller, and pushing arguments below a guest `SP` the boundary does
/// not own raises a question about the guest's red zone that AArch64 does not have but that would
/// need answering anyway. A call that needs more is refused by name.
fn place_arguments(
    cpu: &mut dyn GuestCpu,
    caller: &str,
    target: GuestAddr,
    args: &[GuestArg],
) -> AbiResult<()> {
    let mut ngrn = 0u32;
    let mut nsrn = 0u32;
    for arg in args {
        match *arg {
            GuestArg::Int(value) => {
                if ngrn >= ARG_REGISTERS {
                    return too_many(cpu, caller, target, Bank::Integer);
                }
                cpu.set_x(XReg::new(ngrn as u8).expect("X0-X7 exist"), value);
                ngrn += 1;
            }
            GuestArg::Pointer(value) => {
                if ngrn >= ARG_REGISTERS {
                    return too_many(cpu, caller, target, Bank::Integer);
                }
                cpu.set_x(XReg::new(ngrn as u8).expect("X0-X7 exist"), value as u64);
                ngrn += 1;
            }
            GuestArg::Double(value) => {
                if nsrn >= ARG_REGISTERS {
                    return too_many(cpu, caller, target, Bank::FloatingPoint);
                }
                cpu.set_v(VReg::new(nsrn as u8).expect("V0-V7 exist"), u128::from(value.to_bits()));
                nsrn += 1;
            }
            GuestArg::Float(value) => {
                if nsrn >= ARG_REGISTERS {
                    return too_many(cpu, caller, target, Bank::FloatingPoint);
                }
                cpu.set_v(VReg::new(nsrn as u8).expect("V0-V7 exist"), u128::from(value.to_bits()));
                nsrn += 1;
            }
        }
    }
    Ok(())
}

fn too_many(
    cpu: &dyn GuestCpu,
    caller: &str,
    target: GuestAddr,
    bank: Bank,
) -> AbiResult<()> {
    Err(AbiError::BadCallbackStack {
        symbol: caller.to_string(),
        target,
        sp: cpu.sp(),
        why: match bank {
            Bank::Integer => {
                "more than eight integer arguments would have to go on the guest's stack, which \
                 the host-to-guest direction does not write to"
            }
            Bank::FloatingPoint => {
                "more than eight floating-point arguments would have to go on the guest's \
                 stack, which the host-to-guest direction does not write to"
            }
        },
    })
}

/// Which of AAPCS64's two argument register banks ran out.
///
/// An enum rather than a `&'static str`, because the string form was written once and then discarded
/// with a `let _ = bank;`, which made both messages the generic one and made the test asserting on
/// them pass against the wrong text. A type cannot be ignored that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bank {
    Integer,
    FloatingPoint,
}

/// One argument to a guest callback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GuestArg {
    /// An integer, in the next `X` register.
    Int(u64),
    /// A guest pointer, in the next `X` register.
    Pointer(GuestAddr),
    /// A `double`, in the next `V` register.
    Double(f64),
    /// A `float`, in the next `V` register.
    ///
    /// Distinct from [`Double`](GuestArg::Double) because a *named* `float` parameter really is a
    /// `float` — the promotion to `double` is a variadic rule, and a guest callback's parameters are
    /// named. Writing a `double`'s bit pattern where the callee will read `S0` produces a number that
    /// is wrong rather than imprecise.
    Float(f32),
}

/// What a guest callback returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestReturn {
    /// `X0`.
    pub x0: u64,
    /// `X1`, for a 128-bit or two-register return.
    pub x1: u64,
    /// `V0`, all 128 bits.
    pub v0: u128,
}

impl GuestReturn {
    /// As an `int`, sign-extended from the low 32 bits of `X0`.
    ///
    /// Which is what a `qsort` comparator returns, and the sign is the whole content of it.
    #[must_use]
    pub fn as_i32(self) -> i32 {
        self.x0 as u32 as i32
    }

    /// As a pointer, which is what a `pthread` entry point returns.
    #[must_use]
    pub fn as_pointer(self) -> GuestAddr {
        self.x0 as GuestAddr
    }

    /// As a `double`.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from_bits(self.v0 as u64)
    }

    /// As a `float`.
    #[must_use]
    pub fn as_f32(self) -> f32 {
        f32::from_bits(self.v0 as u32)
    }
}

// ------------------------------------------------------------------------------- state plumbing

/// [`ThunkRegs`] over a `&mut dyn GuestCpu`, so the exit path can use the same marshaller.
struct CpuRegs<'a> {
    cpu: &'a mut dyn GuestCpu,
}

impl ThunkRegs for CpuRegs<'_> {
    fn x(&self, index: u32) -> u64 {
        u8::try_from(index).ok().and_then(|n| XReg::new(n).ok()).map_or(0, |reg| self.cpu.x(reg))
    }
    fn set_x(&mut self, index: u32, value: u64) {
        if let Some(reg) = u8::try_from(index).ok().and_then(|n| XReg::new(n).ok()) {
            self.cpu.set_x(reg, value);
        }
    }
    fn v(&self, index: u32) -> u128 {
        u8::try_from(index).ok().and_then(|n| VReg::new(n).ok()).map_or(0, |reg| self.cpu.v(reg))
    }
    fn set_v(&mut self, index: u32, value: u128) {
        if let Some(reg) = u8::try_from(index).ok().and_then(|n| VReg::new(n).ok()) {
            self.cpu.set_v(reg, value);
        }
    }
    fn sp(&self) -> GuestAddr {
        self.cpu.sp()
    }
    fn set_sp(&mut self, value: GuestAddr) {
        self.cpu.set_sp(value);
    }
}

/// [`RetSink`] over a `&mut dyn GuestCpu`.
struct CpuSink<'a> {
    cpu: &'a mut dyn GuestCpu,
}

impl RetSink for CpuSink<'_> {
    fn set_x(&mut self, index: u32, value: u64) {
        if let Some(reg) = u8::try_from(index).ok().and_then(|n| XReg::new(n).ok()) {
            self.cpu.set_x(reg, value);
        }
    }
    fn set_v(&mut self, index: u32, value: u128) {
        if let Some(reg) = u8::try_from(index).ok().and_then(|n| VReg::new(n).ok()) {
            self.cpu.set_v(reg, value);
        }
    }
}

/// The whole architectural state, saved across a call into guest code.
///
/// **All of it, not the callee-saved half.** AAPCS64 says a callee may clobber `X0`-`X18` and
/// `V0`-`V7` freely, so saving only the callee-saved registers would be correct *if the guest
/// callback obeyed the ABI*. It is untrusted code: it may clobber anything, and the outer guest frame
/// this call is suspended inside is entitled to find its registers as it left them. Saving everything
/// costs 64 register reads against a guest call that costs at least a translation.
struct SavedState {
    x: [u64; 31],
    v: [u128; 32],
    sp: GuestAddr,
    pc: GuestAddr,
    nzcv: Nzcv,
}

impl SavedState {
    fn capture(cpu: &dyn GuestCpu) -> Self {
        let mut x = [0u64; 31];
        for (index, slot) in x.iter_mut().enumerate() {
            *slot = cpu.x(XReg::new(index as u8).expect("X0-X30 exist"));
        }
        let mut v = [0u128; 32];
        for (index, slot) in v.iter_mut().enumerate() {
            *slot = cpu.v(VReg::new(index as u8).expect("V0-V31 exist"));
        }
        Self { x, v, sp: cpu.sp(), pc: cpu.pc(), nzcv: cpu.nzcv() }
    }

    fn restore(&self, cpu: &mut dyn GuestCpu) {
        for (index, &value) in self.x.iter().enumerate() {
            cpu.set_x(XReg::new(index as u8).expect("X0-X30 exist"), value);
        }
        for (index, &value) in self.v.iter().enumerate() {
            cpu.set_v(VReg::new(index as u8).expect("V0-V31 exist"), value);
        }
        cpu.set_sp(self.sp);
        cpu.set_pc(self.pc);
        cpu.set_nzcv(self.nzcv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_mem::{CommitPolicy, Placement, Protection};

    fn builder() -> BoundaryBuilder {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        BoundaryBuilder::new(space, 32, 4096).expect("a boundary")
    }

    fn noop(_call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
        Ok(())
    }

    fn noop_reentrant(_call: &mut ReentrantCall<'_>) -> AbiResult<()> {
        Ok(())
    }

    #[test]
    fn declaring_a_symbol_twice_gives_it_the_same_address() {
        let b = builder();
        let first = b.declare_function("memcpy").expect("a slot");
        assert_eq!(b.declare_function("memcpy").expect("the same slot"), first);
        assert_eq!(b.address_of("memcpy"), Some(first));
        assert_eq!(b.address_of("memmove"), None);
        let other = b.declare_function("memmove").expect("a slot");
        assert_ne!(other, first);
    }

    /// **`lookup` is what `dlsym` answers with, and an `Unbound` slot is not a hit.**
    ///
    /// Both directions. A symbol this layer supplies is found in the scope the guest's own
    /// `DT_VERNEED` attributes it to and nowhere else; and a symbol that has an address only so
    /// that a *direct call* can name itself is a miss, because handing that address back through
    /// `dlsym` would turn a lookup the guest is prepared to see fail into a pointer it calls
    /// thousands of initializers later.
    #[test]
    fn lookup_answers_in_the_librarys_scope_and_never_with_an_unbound_slot() {
        let b = builder();
        // The loader is what attributes a symbol, so the attribution is made the way the loader
        // makes it: by resolving a request that carries a library.
        let ask = |name: &str, library: Option<&str>| {
            b.resolve(&SymbolRequest {
                name,
                kind: SymbolKind::Function,
                library,
                version: None,
                weak: false,
            })
        };
        ask("memcpy", Some("libc.so")).expect("a slot");
        ask("sinf", Some("libm.so")).expect("a slot");
        ask("setjmp", Some("libc.so")).expect("a slot");
        b.bind_inline("memcpy", noop).expect("bound");
        b.bind_inline("sinf", noop).expect("bound");
        // `setjmp` is deliberately left unbound.
        let boundary = b.finish();

        assert_eq!(
            boundary.libraries().into_iter().collect::<Vec<_>>(),
            vec!["libc.so", "libm.so"],
            "the libraries are the guest's own, not a list here"
        );
        // Found in the global scope and in its own library.
        assert!(boundary.lookup(None, "memcpy").is_some());
        assert!(boundary.lookup(Some("libc.so"), "memcpy").is_some());
        // **And not in another library's**, which is the whole point of the scope.
        assert!(
            boundary.lookup(Some("libm.so"), "memcpy").is_none(),
            "this binary says `memcpy` comes from libc.so, so libm.so must not answer for it"
        );
        assert!(boundary.lookup(Some("libc.so"), "sinf").is_none());
        // An `Unbound` slot has an address and is still a miss, in every scope.
        assert!(boundary.slot_named("setjmp").is_some(), "it does have an address");
        assert!(boundary.lookup(None, "setjmp").is_none());
        assert!(boundary.lookup(Some("libc.so"), "setjmp").is_none());
        // A symbol nothing declared is a miss rather than a panic.
        assert!(boundary.lookup(None, "eglGetProcAddress").is_none());
    }

    /// The design point: an import nothing implements still gets an address, so that calling it is a
    /// named error rather than a branch to zero.
    #[test]
    fn an_undeclared_symbol_becomes_an_unbound_slot_rather_than_nothing() {
        let b = builder();
        b.declare_function("getaddrinfo").expect("a slot");
        let boundary = b.finish();
        let slot = boundary.slot_named("getaddrinfo").expect("the slot exists");
        assert!(matches!(slot.binding, Binding::Unbound));
        assert_ne!(slot.address, 0, "and it has a real guest address");
        assert!(boundary.region().holds_function(slot.address));
    }

    /// **`declare_absent` in both directions**, which is the whole of the mechanism.
    ///
    /// A **weak** reference to a declared-absent symbol resolves to nothing, so the loader writes
    /// a null and the guest's own null test decides. A **strong** reference to the same symbol
    /// still gets a named slot, because a strong reference has no null test in front of it and a
    /// null there is a branch to address zero with no symbol attached. Both halves are asserted:
    /// a mechanism that returned `None` for every weak symbol would pass the first and fail the
    /// second, and it would take `gettid` and `getentropy` — weak imports a real bionic *does*
    /// supply — down with it.
    #[test]
    fn an_absent_symbol_resolves_to_nothing_only_for_a_weak_reference() {
        let b = builder();
        b.declare_absent("__gcov_dump").expect("declared absent");
        assert!(b.is_absent("__gcov_dump"));
        assert!(!b.is_absent("gettid"));

        let request = |name: &'static str, weak: bool| SymbolRequest {
            name,
            kind: SymbolKind::Unspecified,
            library: None,
            version: None,
            weak,
        };
        assert!(
            b.resolve(&request("__gcov_dump", true)).is_none(),
            "a weak reference to an absent symbol must resolve to nothing"
        );
        assert_eq!(b.address_of("__gcov_dump"), None, "and must not have consumed a slot");

        let strong = b.resolve(&request("__gcov_dump", false)).expect("a strong reference binds");
        assert_ne!(strong.address, 0, "a strong reference gets a named slot even so");

        // Another weak symbol, not declared absent, still binds: this is not a rule about
        // weakness.
        let other = b.resolve(&request("gettid", true)).expect("an ordinary weak import binds");
        assert_ne!(other.address, strong.address);
    }

    /// A symbol cannot be both bound and absent, and saying so is refused rather than resolved
    /// silently in one direction.
    #[test]
    fn a_symbol_that_already_has_a_slot_cannot_be_declared_absent() {
        let b = builder();
        b.bind_inline("__gcov_dump", noop).expect("bound");
        let error = b.declare_absent("__gcov_dump").expect_err("a contradiction");
        let text = error.to_string();
        assert!(text.contains("__gcov_dump"), "{text}");
        assert!(text.contains("two different answers"), "{text}");
        assert!(!b.is_absent("__gcov_dump"), "the refusal must not half-apply");
    }

    #[test]
    fn binding_replaces_the_unbound_placeholder_and_keeps_the_address() {
        let b = builder();
        let declared = b.declare_function("memcpy").expect("a slot");
        let bound = b.bind_inline("memcpy", noop).expect("bind");
        assert_eq!(declared, bound, "binding must not move the address the loader already wrote");
        let reentrant = b.bind_reentrant("qsort", noop_reentrant).expect("bind");
        let boundary = b.finish();
        assert!(matches!(boundary.slot_named("memcpy").expect("slot").binding, Binding::Inline(_)));
        assert!(matches!(
            boundary.slot_named("qsort").expect("slot").binding,
            Binding::Reentrant(_)
        ));
        assert_eq!(boundary.slot_named("qsort").expect("slot").address, reentrant);
    }

    #[test]
    fn a_data_symbol_gets_data_memory_and_is_marked_as_data() {
        let b = builder();
        let address = b.declare_data("environ", 8, 8).expect("a data object");
        let boundary = b.finish();
        assert!(boundary.region().holds_data(address));
        assert!(!boundary.region().holds_function(address));
        assert!(matches!(boundary.slot_named("environ").expect("slot").binding, Binding::Data));
    }

    /// The four ways an address can fail to be a thunk, told apart. Reporting a mid-slot branch as
    /// "unbound" would send a reader looking for an implementation that is not missing.
    #[test]
    fn the_slot_lookup_distinguishes_a_mid_thunk_branch_from_an_unknown_address() {
        let b = builder();
        b.bind_inline("memcpy", noop).expect("bind");
        let boundary = b.finish();
        let slot = boundary.slot_named("memcpy").expect("slot").address;

        assert_eq!(boundary.slot_at(slot).expect("the slot itself").symbol, "memcpy");

        let error = boundary.slot_at(slot + 4).expect_err("four bytes in is not a call");
        match error {
            AbiError::MidThunk { symbol, offset, address, slot: reported } => {
                assert_eq!(symbol, "memcpy");
                assert_eq!(offset, 4);
                assert_eq!(address, slot + 4);
                assert_eq!(reported, slot);
            }
            other => panic!("{other:?}"),
        }

        // An unallocated slot inside the region, and an address outside it altogether.
        let past = boundary.region().functions_end() - crate::region::SLOT_BYTES;
        assert!(matches!(
            boundary.slot_at(past).expect_err("nothing there"),
            AbiError::NoSuchThunk { .. }
        ));
        assert!(matches!(
            boundary.slot_at(boundary.region().data_start()).expect_err("not the function area"),
            AbiError::NoSuchThunk { .. }
        ));
    }

    /// The sentinel must be inside the non-executable function area, so that a guest branching there
    /// on its own takes a typed fault. And it must not collide with a symbol's slot.
    #[test]
    fn the_callback_sentinel_is_its_own_slot_in_the_function_area() {
        let b = builder();
        let first = b.declare_function("memcpy").expect("a slot");
        let boundary = b.finish();
        let sentinel = boundary.sentinel();
        assert!(boundary.region().holds_function(sentinel));
        assert_ne!(sentinel, first, "the sentinel is not a symbol's slot");
        assert!(boundary.slots().all(|slot| slot.address != sentinel));
        assert_eq!(boundary.region().slot_of(sentinel).map(|(_, offset)| offset), Some(0));
    }

    /// The token an inline thunk carries has to be this boundary and not a copy of it, or the
    /// trampoline would look up the symbol in a different table.
    #[test]
    fn the_thunk_context_is_this_boundary_s_own_address() {
        let boundary = builder().finish();
        let context = boundary.context();
        assert_eq!(context.0, Arc::as_ptr(&boundary) as usize);
        assert_ne!(context.0, 0);
        let other = builder().finish();
        assert_ne!(other.context().0, context.0);
    }

    #[test]
    fn the_depth_limit_is_a_stated_number_and_the_crossing_cap_is_not_negative() {
        assert_eq!(MAX_GUEST_DEPTH, 8);
        assert!(MAX_EXIT_CROSSINGS < i64::MAX as u64, "D16: nothing that reads as negative");
        // An initializer run registers 3,594 `__cxa_atexit` handlers, so the cap has to be far
        // above that to be a containment rather than a limit on correct programs.
        assert_eq!(MAX_EXIT_CROSSINGS, 4_294_967_296);
    }

    /// The error channel is per thread, so two guest threads failing at once cannot read each other's
    /// error. An error naming the wrong symbol is worse than no error.
    #[test]
    fn the_pending_error_channel_does_not_cross_a_thread_boundary() {
        record_pending(AbiError::Unbound { symbol: "here".into(), address: 1 });
        let seen = std::thread::spawn(|| {
            let other = take_pending();
            record_pending(AbiError::Unbound { symbol: "there".into(), address: 2 });
            (other.is_some(), take_pending().map(|e| e.symbol().unwrap_or_default().to_string()))
        })
        .join()
        .expect("the other thread");
        assert!(!seen.0, "the other thread must not see this thread's error");
        assert_eq!(seen.1.as_deref(), Some("there"));
        assert_eq!(take_pending().and_then(|e| e.symbol().map(str::to_owned)).as_deref(), Some("here"));
        assert!(take_pending().is_none(), "taking it consumes it");
    }

    /// First one wins, matching `omni-cpu`'s `stop`: the first failure is the one that happened.
    #[test]
    fn the_first_pending_error_is_the_one_kept() {
        let _ = take_pending();
        record_pending(AbiError::Unbound { symbol: "first".into(), address: 1 });
        record_pending(AbiError::Unbound { symbol: "second".into(), address: 2 });
        assert_eq!(
            take_pending().and_then(|e| e.symbol().map(str::to_owned)).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn a_guest_return_is_read_out_of_the_registers_the_abi_names() {
        let ret = GuestReturn { x0: 0xFFFF_FFFF_FFFF_FFFF, x1: 7, v0: u128::from(2.5f64.to_bits()) };
        assert_eq!(ret.as_i32(), -1, "a comparator's sign is the whole content of its answer");
        assert_eq!(ret.as_pointer(), usize::MAX);
        assert_eq!(ret.as_f64(), 2.5);
        let float = GuestReturn { x0: 0, x1: 0, v0: u128::from(1.5f32.to_bits()) };
        assert_eq!(float.as_f32(), 1.5);
    }

    /// A `float` callback argument is not a `double`. The promotion is a variadic rule, and a guest
    /// callback's parameters are named — so writing a `double`'s bits where the callee reads `S0`
    /// produces a wrong number rather than an imprecise one.
    #[test]
    fn a_float_callback_argument_is_distinct_from_a_double_one() {
        assert_ne!(GuestArg::Float(1.0), GuestArg::Double(1.0));
        assert_ne!(u128::from(1.0f32.to_bits()), u128::from(1.0f64.to_bits()));
    }

    #[test]
    fn a_region_that_runs_out_of_slots_refuses_by_name() {
        let space = Arc::new(GuestSpace::new().expect("space"));
        let b = BoundaryBuilder::new(space, 1, 64).expect("a boundary with room for almost nothing");
        let mut declared = 0usize;
        let error = loop {
            match b.declare_function(&format!("symbol_{declared}")) {
                Ok(_) => declared += 1,
                Err(error) => break error,
            }
            assert!(declared < 100_000, "the region must run out");
        };
        assert!(matches!(error, AbiError::RegionFull { what: "function slot", .. }), "{error:?}");
        assert!(declared > 0, "some slots must have been handed out first");
    }

    /// The data area has to be usable guest memory, not just an address: the 18 `STT_OBJECT` imports
    /// are loaded from.
    #[test]
    fn the_data_area_is_readable_and_writable_guest_memory() {
        let b = builder();
        let address = b.declare_data("stdout", 8, 8).expect("data");
        let boundary = b.finish();
        let blame = Blame::new("stdout", address, 0);
        boundary.mem().write_u64(address, 0xF11E, blame).expect("the guest writes environ and errno");
        assert_eq!(boundary.mem().read_u64(address, blame).expect("read"), 0xF11E);
    }

    /// A sanity check on the fixture the integration suites use: a mapping placed by the space is at a
    /// high address, which is the premise D4's identity mapping rests on.
    #[test]
    fn the_thunk_region_lives_where_a_guest_mapping_lives() {
        let boundary = builder().finish();
        let space = boundary.region().space();
        let page = space.page_size();
        let elsewhere = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect("another mapping");
        assert!(space.contains(boundary.region().functions_start(), 16));
        assert!(space.contains(elsewhere, page));
    }
}
