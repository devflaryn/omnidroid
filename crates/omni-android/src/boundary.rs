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
use std::collections::BTreeMap;
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
#[derive(Debug, Clone)]
pub struct Slot {
    /// The symbol name, exactly as `.dynstr` spells it.
    pub symbol: String,
    /// Its guest address.
    pub address: GuestAddr,
    /// Who services it.
    pub binding: Binding,
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
            Slot { symbol: symbol.to_string(), address, binding: Binding::Unbound },
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
            .insert(address, Slot { symbol: symbol.to_string(), address, binding: Binding::Data });
        Ok(address)
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
///   branch to address zero with no symbol attached to it.
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
                // A failure here is the region running out, which the loader has no channel for. It
                // is reported as "nothing supplied this symbol" rather than swallowed, and the
                // loader's unresolved list then names every symbol that missed out.
                let address = self.declare_function(request.name).ok()?;
                Some(SymbolValue { address: address as u64, kind: SymbolKind::Function })
            }
            SymbolKind::Object => self.address_of(request.name).map(|address| SymbolValue {
                address: address as u64,
                kind: SymbolKind::Object,
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

    /// How many times each path has been taken.
    #[must_use]
    pub fn crossings(&self) -> Crossings {
        *self.crossings.lock()
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
        self.run_at_depth(cpu, from, limit, 0)
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
            // An inline handler that failed left its reason here and deferred. Checked first, because
            // the slot lookup below would otherwise report the symbol as merely unbound.
            if let Some(error) = take_pending() {
                return Err(error);
            }
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
        let result = self.call_guest_inner(target, args, limit, depth);
        // Restored on every path, including the error ones: a handler that reports a failed callback
        // still leaves the outer guest frame runnable, and a caller that decides to carry on must not
        // be carrying on with the callback's registers.
        saved.restore(self.cpu);
        result
    }

    fn call_guest_inner(
        &mut self,
        target: GuestAddr,
        args: &[GuestArg],
        limit: RunLimit,
        depth: usize,
    ) -> AbiResult<GuestReturn> {
        let sp = self.cpu.sp();
        if sp % 16 != 0 {
            return Err(AbiError::BadCallbackStack {
                symbol: self.symbol.to_string(),
                target,
                sp,
                why: "AArch64 requires SP to be 16-byte aligned at a public interface, and every \
                      SP-relative access in the callee's prologue assumes it",
            });
        }
        // The callee's prologue will push below `SP`, so the stack has to be there. Checked rather
        // than assumed: a guest whose stack has overflowed would otherwise have its callback fault at
        // an address nothing here chose, and the failure would be reported as the callback's.
        self.boundary
            .mem
            .checked_ptr(sp.saturating_sub(16), 16, true, self.blame(0))
            .map_err(|_| AbiError::BadCallbackStack {
                symbol: self.symbol.to_string(),
                target,
                sp,
                why: "the 16 bytes below SP are not writable guest memory, so the callee's own \
                      prologue would fault",
            })?;

        self.place_arguments(target, args)?;
        let sentinel = self.boundary.sentinel;
        let previous = self.cpu.return_sentinel();
        self.cpu.set_return_sentinel(sentinel)?;
        self.cpu.set_x(XReg::new(30).expect("X30 exists"), sentinel as u64);

        let outcome = self.boundary.run_at_depth(self.cpu, target, limit, depth);

        // Put the outer sentinel back before anything else can go wrong, so an error path cannot
        // leave the context armed on the callback's sentinel.
        if let Some(previous) = previous {
            self.cpu.set_return_sentinel(previous)?;
        }
        match outcome? {
            ExitReason::Returned { pc } if pc == sentinel => Ok(GuestReturn {
                x0: self.cpu.x(XReg::new(0).expect("X0 exists")),
                x1: self.cpu.x(XReg::new(1).expect("X1 exists")),
                v0: self.cpu.v(VReg::new(0).expect("V0 exists")),
            }),
            exit => Err(AbiError::GuestCallbackStopped {
                symbol: self.symbol.to_string(),
                target,
                exit,
            }),
        }
    }

    /// AAPCS64 in the outgoing direction: integers in `X0`-`X7`, floating point in `V0`-`V7`.
    ///
    /// **Nothing goes on the stack.** Every callback shape in the reachable set takes at most three
    /// arguments — a comparator takes two pointers, a `dl_iterate_phdr` callback takes three, a
    /// thread entry point takes one — so the stack path would be code with no caller, and pushing
    /// arguments below a guest `SP` the boundary does not own raises a question about the guest's red
    /// zone that AArch64 does not have but that would need answering anyway. A call that needs more
    /// is refused by name.
    fn place_arguments(&mut self, target: GuestAddr, args: &[GuestArg]) -> AbiResult<()> {
        let mut ngrn = 0u32;
        let mut nsrn = 0u32;
        for arg in args {
            match *arg {
                GuestArg::Int(value) => {
                    if ngrn >= ARG_REGISTERS {
                        return self.too_many(target, Bank::Integer);
                    }
                    self.cpu.set_x(XReg::new(ngrn as u8).expect("X0-X7 exist"), value);
                    ngrn += 1;
                }
                GuestArg::Pointer(value) => {
                    if ngrn >= ARG_REGISTERS {
                        return self.too_many(target, Bank::Integer);
                    }
                    self.cpu.set_x(XReg::new(ngrn as u8).expect("X0-X7 exist"), value as u64);
                    ngrn += 1;
                }
                GuestArg::Double(value) => {
                    if nsrn >= ARG_REGISTERS {
                        return self.too_many(target, Bank::FloatingPoint);
                    }
                    self.cpu
                        .set_v(VReg::new(nsrn as u8).expect("V0-V7 exist"), u128::from(value.to_bits()));
                    nsrn += 1;
                }
                GuestArg::Float(value) => {
                    if nsrn >= ARG_REGISTERS {
                        return self.too_many(target, Bank::FloatingPoint);
                    }
                    self.cpu
                        .set_v(VReg::new(nsrn as u8).expect("V0-V7 exist"), u128::from(value.to_bits()));
                    nsrn += 1;
                }
            }
        }
        Ok(())
    }

    fn too_many(&self, target: GuestAddr, bank: Bank) -> AbiResult<()> {
        Err(AbiError::BadCallbackStack {
            symbol: self.symbol.to_string(),
            target,
            sp: self.cpu.sp(),
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
