//! Thread lifecycle: `pthread_create`, `pthread_join`, `pthread_detach`,
//! `pthread_getschedparam`.
//!
//! # Three constraints meet in `pthread_create`, and none of them may be traded against another
//!
//! **1. D13.** A guest thread's `TPIDR_EL0` must point at a populated bionic TLS block with a
//! stack guard at `+0x28` **before the thread executes one instruction**; 1,276 of
//! `libroblox.so`'s 1,282 thread-pointer reads load exactly that offset. Here that is structural
//! rather than remembered: the context comes from
//! [`GuestCpuBackend::create_guest_thread`](omni_cpu::GuestCpuBackend::create_guest_thread),
//! which allocates the block out of the backend's own arena and cannot produce a context without
//! one — `GuestThreadConfig` refuses to be built otherwise. It is the **backend's** arena and not
//! one of ours on purpose: bionic copies a single per-process guard into every thread, and a
//! second arena beside it would put a second guard value in one address space, so a canary stored
//! in one thread and checked in another would fail. That symptom is a *termination*, arriving in
//! a thread that did nothing wrong.
//!
//! **2. Host → guest re-entry is a type property** (D18, task 2's review finding F9). The start
//! routine is guest code. [`ImportCall`](crate::ImportCall) deliberately holds no CPU, so an
//! inline handler *cannot* run guest code; all four symbols here are therefore
//! [`bind_reentrant`](crate::BoundaryBuilder::bind_reentrant). Nothing about that property is
//! weakened to make this work: what `pthread_create` needs is not a second CPU on *this* thread
//! but the boundary itself, so that it can install the thunk table on a **new** context and start
//! a run loop there. [`ReentrantCall::boundary`](crate::ReentrantCall::boundary) is that, and its
//! documentation says why it cannot be used to re-enter the calling thread's guest.
//!
//! `pthread_create` also maps guest memory — the new thread's stack — which is F9's other half
//! and the reason `mmap` is on the exit path too.
//!
//! **3. Every guest thread costs about 24.5 MiB of commit charge**, measured, and this phase is
//! what makes that real: before it, nothing could create one. See
//! [`Bionic::set_thread_host`](super::Bionic::set_thread_host) for the figure, its method and
//! what it means for the multi-instance requirement.
//!
//! # What a guest thread is here
//!
//! One host thread, running one [`GuestCpu`] with the whole thunk table installed on it, with the
//! instance published to it by [`Bionic::activate`](super::Bionic::activate)'s machinery so that
//! its `errno`, its `pthread_self()` and its mutex ownership are its own. It runs in **short
//! budget windows** rather than one unlimited run, which is D16's prescription: a guest thread
//! that never returns is otherwise stoppable by nothing, and a window boundary is a decision
//! point that exists whatever the guest is doing.
//!
//! # What a guest thread deliberately does **not** do
//!
//! **It does not run `pthread_key` destructors when it exits.** Bionic does. Closing it needs a
//! host → guest call from outside a thunk crossing, which is a boundary API that does not exist
//! — every call into guest code today is made from inside a [`ReentrantCall`], and a thread
//! finishing its start routine is not inside one. The cost is recorded rather than hidden: a
//! guest that frees per-thread state from a key destructor leaks it once per thread exit. It does
//! not affect M3's gate, where the 3,594 initializers run on a thread that does not exit, and
//! `pthread_exit` is not in the reachable 188 at all.
//!
//! # The split between an errno-style return and a refusal
//!
//! The same split `guestmem` and `files` draw, and the pthread functions return their error
//! **as the return value** rather than through `errno`:
//!
//! * **Well-formed and legitimately failed** → what POSIX says. `EAGAIN` when a resource ran out,
//!   `ESRCH` for a `pthread_t` no live thread answers to, `EINVAL` for a thread that is not
//!   joinable, `EDEADLK` for a join that would deadlock. Each is in the POSIX text for that
//!   function and guest code has a branch for it.
//! * **Cannot be carried out correctly** → [`AbiError::Refused`] naming the symbol. No thread
//!   host configured, a null start routine, and a join on a thread that stopped without
//!   returning — because there is no value to report for a thread that never produced one, and
//!   `0` with a zero `retval` would be a thread that "returned NULL".

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use omni_bionic::errno::consts;
use omni_bionic::threads::GuestThreadId;
use omni_cpu::{ExitReason, GuestCpu, GuestCpuBackend, RunLimit, XReg};
use omni_mem::{CommitPolicy, GuestAddr, Placement, Protection};

use crate::boundary::{Boundary, ImportCall, ReentrantCall};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use super::runtime::ThreadSlot;
use super::{active, Active, Bionic, MAX_GUEST_THREADS};

// ------------------------------------------------------------------ the guest's constants

/// `SCHED_OTHER`, which Linux spells `SCHED_NORMAL`, and which is **0**.
///
/// Kernel UAPI (`include/uapi/linux/sched.h`), the same source this adapter's `mmap` flags and
/// `clockid_t` numbers come from.
pub const SCHED_OTHER: i32 = 0;

/// `sizeof(struct sched_param)`: one `int sched_priority`, and nothing else on Linux.
const SCHED_PARAM_BYTES: usize = 4;

/// Offset of the detach state in a guest `pthread_attr_t`.
///
/// bionic's LP64 attr is `uint32_t flags; ...`, and `omni_bionic::metadata::attr_setdetachstate`
/// writes the state as a four-byte value at offset 0 — so this is the *adapter* agreeing with
/// the crate that owns the attr rather than a second derivation of the layout.
const ATTR_DETACH_STATE: usize = 0;

/// Offset of the stack size in a guest `pthread_attr_t`, matching
/// `omni_bionic::metadata::attr_setstacksize`.
const ATTR_STACK_SIZE: usize = 8;

/// Offset of the guard size in a guest `pthread_attr_t`, matching
/// `omni_bionic::metadata::attr_setguardsize`.
const ATTR_GUARD_SIZE: usize = 16;

/// `PTHREAD_CREATE_JOINABLE`.
const CREATE_JOINABLE: i32 = 0;
/// `PTHREAD_CREATE_DETACHED`.
const CREATE_DETACHED: i32 = 1;

// ------------------------------------------------------------------ this layer's policy numbers

/// The stack a guest thread gets when its attribute object asks for the default.
///
/// **A policy number, and stated as one.** `omni_bionic::metadata::attr_init` writes 56 zero
/// bytes and documents `stack_size == 0` as "system default", so the default is this layer's to
/// choose and is *not* a claim about what bionic's own `PTHREAD_STACK_SIZE_DEFAULT` is — there is
/// no NDK on this machine to read that from, and guessing it would be a number with nothing
/// behind it.
///
/// One mebibyte, and choosing generously is cheap here rather than expensive: the mapping is
/// [`CommitPolicy::Lazy`], so D10's scarce resource — commit charge — is spent one granule at a
/// time as the thread's stack actually grows, and what a large default really costs is address
/// space, which D10 measured at **0.000 MiB** of commit for a 4 GiB reservation.
pub const DEFAULT_GUEST_STACK_BYTES: usize = 1024 * 1024;

/// The smallest stack a guest thread may be given.
///
/// **This layer's number, not bionic's `PTHREAD_STACK_MIN`** — again because there is no header
/// here to read that from. It is a floor under the two things that must fit before the guest's
/// own frame does: the callee's prologue, which
/// [`call_guest`](crate::ReentrantCall::call_guest) already requires 16 writable bytes below `SP`
/// for, and whatever the start routine pushes before it can check anything. A request below it is
/// `EINVAL`, which is what POSIX says `pthread_create` answers for a stacksize under
/// `PTHREAD_STACK_MIN`, rather than a silent round-up — a round-up would leave
/// `pthread_attr_getstacksize` and the real stack disagreeing.
pub const MIN_GUEST_STACK_BYTES: usize = 16 * 1024;

/// The default stack must not be below the floor, or every default-attribute `pthread_create`
/// would answer `EINVAL`.
///
/// A **compile-time** assertion rather than a test, for the reason the arena's two ceiling
/// relations are: both sides are constants, so a build that violated it could not produce a
/// binary to run a test with — and clippy is right that `assert!` on constants is folded away.
const _: () = assert!(DEFAULT_GUEST_STACK_BYTES >= MIN_GUEST_STACK_BYTES);

/// Guest instructions a created thread runs before its run loop gets a decision point.
///
/// **D16's prescription, applied.** A watchdog over a guest that never returns must be built from
/// short budget windows rather than from a cross-thread halt, because the halt flag is checked at
/// terminals that a counted budget makes exclusive (`crates/dynarmic-sys/patches/README.md`, item
/// 2b). So a created thread is run in windows of this size and the loop re-checks the instance's
/// stop switch between them; a thread doing nothing wrong pays one dispatcher return per window.
///
/// A million is the slice size the backend's own callback-free-slice invariant already uses, so
/// it is a size that has been run rather than a size that was picked.
pub const GUEST_THREAD_STEP_WINDOW: u64 = 1_000_000;

// ------------------------------------------------------------------ what the embedding supplies

/// What an embedding must give an instance before its guest may create threads.
///
/// There is **no default**, and that is the same shape as
/// [`set_filesystem_root`](super::Bionic::set_filesystem_root) (D23),
/// [`HwcapPolicy::Undecided`](super::HwcapPolicy) (D22) and `dl_iterate_phdr` with no image
/// registered (D21): an instance that has not been given one refuses `pthread_create` by name,
/// naming the method to call. A default is impossible here rather than merely undesirable — the
/// adapter cannot invent a CPU backend, and a backend is the only thing that can satisfy D13.
pub struct ThreadHost {
    /// Makes the contexts. Held as `Arc<dyn ..>` so the adapter never learns which backend it is
    /// — which is what keeps the ARM64-native path expressible (`ARCHITECTURE.md` §2, §6).
    backend: Arc<dyn GuestCpuBackend>,
    /// How many guest threads this instance will create at once.
    limit: usize,
    /// The stack for a thread whose attribute object asks for the default.
    stack_bytes: usize,
    /// Guest instructions per run window.
    window: u64,
}

impl core::fmt::Debug for ThreadHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThreadHost")
            .field("backend", &self.backend.name())
            .field("limit", &self.limit)
            .field("stack_bytes", &self.stack_bytes)
            .field("window", &self.window)
            .finish()
    }
}

impl ThreadHost {
    /// A thread host over `backend`, with this layer's default policy.
    ///
    /// The default limit is [`MAX_GUEST_THREADS`], the adapter's arena capacity, so that this
    /// type adds **no second invented ceiling**: a `pthread_create` past it is `EAGAIN` because
    /// something real ran out. An embedding that wants a tighter bound — and the per-thread
    /// memory figure is a reason to want one — calls [`with_limit`](ThreadHost::with_limit).
    #[must_use]
    pub fn new(backend: Arc<dyn GuestCpuBackend>) -> Self {
        Self {
            backend,
            limit: MAX_GUEST_THREADS,
            stack_bytes: DEFAULT_GUEST_STACK_BYTES,
            window: GUEST_THREAD_STEP_WINDOW,
        }
    }

    /// Cap how many guest threads this instance will have running at once.
    ///
    /// Clamped to [`MAX_GUEST_THREADS`], above which the arena has no `errno` block to give, and
    /// to at least one, since a limit of zero would refuse every legitimate call.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.clamp(1, MAX_GUEST_THREADS);
        self
    }

    /// The stack size for a thread whose attribute object asks for the default.
    ///
    /// Clamped up to [`MIN_GUEST_STACK_BYTES`]: a default below the floor would make every
    /// default-attribute `pthread_create` fail with `EINVAL`, which is a configuration mistake
    /// that would read as a guest defect.
    #[must_use]
    pub fn with_default_stack(mut self, bytes: usize) -> Self {
        self.stack_bytes = bytes.max(MIN_GUEST_STACK_BYTES);
        self
    }

    /// Guest instructions per run window. At least one, so the loop always makes progress.
    #[must_use]
    pub fn with_step_window(mut self, instructions: u64) -> Self {
        self.window = instructions.max(1);
        self
    }

    /// How many guest threads this instance will run at once.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The default stack size.
    #[must_use]
    pub fn default_stack(&self) -> usize {
        self.stack_bytes
    }

    /// Guest instructions per run window.
    #[must_use]
    pub fn step_window(&self) -> u64 {
        self.window
    }

    /// The backend that makes the contexts.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn GuestCpuBackend> {
        &self.backend
    }
}

// ------------------------------------------------------------------ the live-thread registry

/// How a guest thread ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestThreadState {
    /// Still running, or not started yet.
    Running,
    /// The start routine returned, and this is its `void *`.
    Returned(u64),
    /// The instance asked its guest threads to stop and this one did.
    Stopped,
    /// The thread stopped for a reason that is not a return: a fault, an unbound symbol, a
    /// refusal from a handler, or a panic in the runner.
    Failed(String),
}

impl GuestThreadState {
    /// Whether the thread has stopped, whatever the reason.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !matches!(self, GuestThreadState::Running)
    }
}

/// One guest thread this instance created.
pub(super) struct ThreadRecord {
    /// How it ended, if it has.
    pub(super) state: GuestThreadState,
    /// Whether `pthread_detach` has been called on it, or its attribute object asked for it.
    pub(super) detached: bool,
    /// The host thread, so a join can reap it.
    pub(super) handle: Option<std::thread::JoinHandle<()>>,
    /// The start routine, for a diagnostic that has to say which thread.
    pub(super) start_routine: GuestAddr,
}

/// A guest thread that did not finish by returning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestThreadFailure {
    /// The `pthread_t` it was given.
    pub thread: u64,
    /// The guest function it was asked to run.
    pub start_routine: GuestAddr,
    /// What stopped it.
    pub why: String,
}

impl core::fmt::Display for GuestThreadFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "guest thread {:#x} (start routine {:#x}): {}",
            self.thread, self.start_routine, self.why
        )
    }
}

// ------------------------------------------------------------------ one call's state

/// The symbol, the address and the instance, resolved once per call.
struct Call {
    symbol: String,
    address: GuestAddr,
    state: Active,
    mem: GuestMem,
}

impl Call {
    fn begin(c: &ReentrantCall<'_>) -> AbiResult<Self> {
        Self::at(c.symbol(), c.address(), c.mem())
    }

    /// The same, for the one symbol in this module that is serviced inside the run loop.
    fn inline(c: &ImportCall<'_, '_>) -> AbiResult<Self> {
        Self::at(c.symbol(), c.address(), c.mem())
    }

    fn at(symbol: &str, address: GuestAddr, mem: &GuestMem) -> AbiResult<Self> {
        let symbol = symbol.to_string();
        let state = active(&symbol, address)?;
        Ok(Self { symbol, address, state, mem: mem.clone() })
    }

    fn bionic(&self) -> &Arc<Bionic> {
        &self.state.bionic
    }

    fn blame(&self, argument: usize) -> Blame<'_> {
        Blame::new(&self.symbol, self.address, argument)
    }

    fn refuse<T>(&self, why: impl Into<String>) -> AbiResult<T> {
        Err(AbiError::Refused {
            symbol: self.symbol.clone(),
            address: self.address,
            why: why.into(),
        })
    }
}

/// Narrow a guest pointer to a host address, refusing rather than truncating.
fn guest_address(call: &Call, pointer: u64, argument: usize) -> AbiResult<GuestAddr> {
    let _ = argument;
    GuestAddr::try_from(pointer)
        .map_err(|_| AbiError::Refused {
            symbol: call.symbol.clone(),
            address: call.address,
            why: format!("the guest pointer {pointer:#x} is wider than the host's usize"),
        })
}

// ================================================================== pthread_create

/// Everything the new host thread needs, moved into it.
struct Spawn {
    bionic: Arc<Bionic>,
    boundary: Arc<Boundary>,
    cpu: Box<dyn GuestCpu>,
    slot: ThreadSlot,
    entry: GuestAddr,
    argument: u64,
    /// The whole stack mapping, including its guard, so the thread can give it back.
    stack: (GuestAddr, usize),
    /// `SP` at entry: the top of the stack, 16-byte aligned as AAPCS64 requires.
    stack_top: GuestAddr,
    window: u64,
}

/// `int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
///                     void *(*start_routine)(void *), void *arg)`
///
/// Re-entrant, and this is the one symbol in the reachable 188 that creates a guest thread.
/// Everything it does before spawning is validated first, so a failure leaves nothing behind: the
/// out-pointer is checked for writability, the attribute object is read, the stack is mapped, the
/// arena block is reserved and the context is created — and every one of those failure paths
/// gives back whatever the earlier ones took.
pub(super) fn pthread_create(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (thread_out, attr, start_routine, argument) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let call = Call::begin(c)?;
    let bionic = Arc::clone(call.bionic());

    // 1. A thread host, or a refusal naming the method that supplies one. No default is possible:
    //    the adapter cannot invent a CPU backend, and a backend is the only thing that can give a
    //    new thread the TLS block D13 requires before it runs an instruction.
    let Some(host) = bionic.thread_host() else {
        return call.refuse(
            "this guest instance has no thread host, so `pthread_create` cannot create a guest \
             thread. A guest thread is a host thread running a new CPU context whose TPIDR_EL0 \
             points at a populated bionic TLS block (D13), and only a `GuestCpuBackend` can \
             produce one — the embedding supplies it with `Bionic::set_thread_host`. Returning \
             EAGAIN instead would say the runtime ran out of resources when it was never \
             configured, and the guest would retry for ever",
        );
    };

    // 2. A null start routine. There is no errno for it -- POSIX defines none, and bionic simply
    //    branches to it -- so inventing EINVAL would be inventing a contract. It is a detected
    //    defect in guest code, reported as one, which is the same treatment `__write_chk` gives a
    //    detected buffer overrun.
    if start_routine == 0 {
        return call.refuse(
            "the guest called pthread_create with a NULL start routine. POSIX defines no error \
             for it and bionic would branch to address zero, so there is no correct value to \
             return: EAGAIN claims a resource shortage that did not happen and 0 claims a thread \
             was started. This is a detected defect in guest code and is reported as one",
        );
    }
    let entry = guest_address(&call, start_routine, 2)?;

    // 3. The out-pointer, checked BEFORE anything is created. A `pthread_create` that spawned a
    //    thread and then failed to report its id would leave a thread nothing can ever join.
    let out = guest_address(&call, thread_out, 0)?;
    call.mem.checked_ptr(out, 8, true, call.blame(0))?;

    // 4. The attribute object. A null `attr` is the defaults, which is what POSIX says.
    let (detached, stack_request, guard_request) = if attr == 0 {
        (false, 0u64, None)
    } else {
        let at = guest_address(&call, attr, 1)?;
        // **`checked_add` on every offset, because `at` is an address the guest chose.**
        // `at + ATTR_GUARD_SIZE` wraps for an `attr` near the top of the address space, and a
        // debug build *panics* on it -- which Global Constraint 11 calls Critical. Today the
        // first read short-circuits for `attr == usize::MAX`, so the overflow is not reachable;
        // that is an accident of ordering and not a defence.
        let field = |offset: usize| {
            at.checked_add(offset).ok_or_else(|| AbiError::Refused {
                symbol: call.symbol.clone(),
                address: call.address,
                why: format!(
                    "the guest's pthread_attr_t at {at:#x} is close enough to the top of the                      address space that its own fields do not fit below it"
                ),
            })
        };
        let state = call.mem.read_i32(field(ATTR_DETACH_STATE)?, call.blame(1))?;
        let size = call.mem.read_u64(field(ATTR_STACK_SIZE)?, call.blame(1))?;
        let guard = call.mem.read_u64(field(ATTR_GUARD_SIZE)?, call.blame(1))?;
        match state {
            CREATE_JOINABLE => (false, size, Some(guard)),
            CREATE_DETACHED => (true, size, Some(guard)),
            // The attribute object is guest memory and the guest may have written anything into
            // it. `pthread_attr_setdetachstate` refuses anything but 0 and 1 (`omni-bionic`), so
            // a third value means the bytes were written directly.
            _ => {
                c.ret(|mut r| r.i32(consts::EINVAL));
                return Ok(());
            }
        }
    };

    let page = call.mem.space().page_size();
    // 5. The stack. A request below the floor is EINVAL, which is POSIX's own answer for a
    //    stacksize under PTHREAD_STACK_MIN, rather than a silent round-up that would leave
    //    `pthread_attr_getstacksize` and the real stack disagreeing.
    let requested = if stack_request == 0 {
        host.default_stack()
    } else {
        match usize::try_from(stack_request) {
            Ok(size) => size,
            // Wider than the host's address space: no mapping of it can exist, which is EAGAIN's
            // "insufficient resources" rather than a malformed request.
            Err(_) => {
                c.ret(|mut r| r.i32(consts::EAGAIN));
                return Ok(());
            }
        }
    };
    if requested < MIN_GUEST_STACK_BYTES {
        c.ret(|mut r| r.i32(consts::EINVAL));
        return Ok(());
    }
    // A guard page below the stack, which is what bionic gives a thread: a stack that overflows
    // faults on a PROT_NONE page instead of running into whatever mapping happens to be next.
    // The attribute's own guard size is honoured when it is set, rounded up to a page.
    let guard = match guard_request {
        Some(0) => 0,
        Some(bytes) => match usize::try_from(bytes).ok().and_then(|n| round_up(n, page)) {
            Some(rounded) => rounded,
            None => {
                c.ret(|mut r| r.i32(consts::EAGAIN));
                return Ok(());
            }
        },
        None => page,
    };
    let Some(stack_bytes) = round_up(requested, page) else {
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    };
    let Some(total) = stack_bytes.checked_add(guard) else {
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    };

    // 6. The live-thread limit, before anything is mapped. A count rather than a guess: the two
    //    real ceilings under it are the arena's `errno` blocks and the backend's own thread count.
    if bionic.live_guest_threads() >= host.limit() {
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    }

    // 7. The stack mapping. Lazily committed, so a thread that uses a page of its stack costs a
    //    page (D10), and the guard dropped to PROT_NONE afterwards.
    let space = call.mem.space();
    let Ok(stack_base) = space.map_anonymous(
        Placement::Anywhere { align: page },
        total,
        Protection::ReadWrite,
        CommitPolicy::Lazy,
    ) else {
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    };
    if guard > 0 && space.protect(stack_base, guard, Protection::None).is_err() {
        let _ = space.unmap(stack_base, total);
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    }
    // AAPCS64 requires `SP` 16-byte aligned at a public interface, and the whole mapping is
    // page-aligned, so this is the top of the stack exactly.
    let stack_top = (stack_base + total) & !0xF;

    // 8. The arena block and the `pthread_t`, reserved here rather than in the new thread —
    //    because the guest may compare the value this call writes with what the new thread's
    //    `pthread_self()` returns, and allocating it over there would make the two differ until
    //    the child got to it.
    let arena = bionic.arena();
    let Some(slot) = bionic.threads_table().reserve(MAX_GUEST_THREADS, |index| {
        arena + index * super::THREAD_BLOCK_BYTES
    }) else {
        let _ = space.unmap(stack_base, total);
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    };

    // 9. The context. D13 is satisfied by construction: `create_guest_thread` allocates the
    //    bionic TLS block out of the backend's own arena — the same arena, and therefore the same
    //    stack-guard value, as every other thread of this address space — and builds a
    //    `GuestThreadConfig`, which cannot exist without a usable thread pointer.
    let mut cpu = match host.backend().create_guest_thread() {
        Ok(cpu) => cpu,
        Err(error) => {
            bionic.record_thread_failure(GuestThreadFailure {
                thread: slot.id.0,
                start_routine: entry,
                why: format!("the CPU context could not be created: {error}"),
            });
            bionic.threads_table().release(slot);
            let _ = space.unmap(stack_base, total);
            c.ret(|mut r| r.i32(consts::EAGAIN));
            return Ok(());
        }
    };

    // 10. The thunk table on the new context, so the new thread's imported calls reach the same
    //     handlers. Every failure from here on gives back the block, the stack and the context.
    let boundary = Arc::clone(c.boundary());
    if let Err(error) = boundary.install(&mut *cpu) {
        drop(cpu);
        bionic.record_thread_failure(GuestThreadFailure {
            thread: slot.id.0,
            start_routine: entry,
            why: format!("the thunk boundary could not be installed on the new context: {error}"),
        });
        bionic.threads_table().release(slot);
        let _ = space.unmap(stack_base, total);
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
    }

    // 11. The `pthread_t` into the guest's own out-pointer, before the thread starts, so that a
    //     start routine which reads it through `arg` cannot see an unwritten value.
    call.mem.write_u64(out, slot.id.0, call.blame(0))?;

    // 12. The record, then the thread. The record goes in first because the new thread may finish
    //     before this call gets back here, and a thread that finished with no record would have
    //     nothing to report its result to.
    bionic.register_guest_thread(slot.id, detached, entry);
    let spawn = Spawn {
        bionic: Arc::clone(&bionic),
        boundary,
        cpu,
        slot,
        entry,
        argument,
        stack: (stack_base, total),
        stack_top,
        window: host.step_window(),
    };
    match std::thread::Builder::new()
        .name(format!("omnidroid-guest-{:#x}", slot.id.0))
        .spawn(move || run_guest_thread(spawn))
    {
        Ok(handle) => bionic.attach_guest_thread_handle(slot.id, handle),
        Err(error) => {
            // The host refused a thread. Everything this call took has to go back, including the
            // record — otherwise a `pthread_join` on the id this call never reported would block
            // for ever on a thread that does not exist.
            bionic.record_thread_failure(GuestThreadFailure {
                thread: slot.id.0,
                start_routine: entry,
                why: format!("the host refused another thread: {error}"),
            });
            bionic.forget_guest_thread(slot.id);
            bionic.threads_table().release(slot);
            let _ = space.unmap(stack_base, total);
            c.ret(|mut r| r.i32(consts::EAGAIN));
            return Ok(());
        }
    }

    c.ret(|mut r| r.i32(0));
    Ok(())
}

/// Round `value` up to a multiple of `to`, or `None` if that would wrap.
///
/// `checked_add` rather than `(n + to - 1) & !(to - 1)`: the masked form wraps to a *small*
/// number for a length near `usize::MAX`, and a release build does it silently — which is the
/// `gmtime(i64::MIN)` shape (D22). A guest that asks for a stack of `SIZE_MAX` must get EAGAIN,
/// not a 4 KiB stack.
fn round_up(value: usize, to: usize) -> Option<usize> {
    if to == 0 {
        return Some(value);
    }
    let remainder = value % to;
    if remainder == 0 {
        return Some(value);
    }
    value.checked_add(to - remainder)
}

/// The body of one guest thread.
///
/// Wrapped in `catch_unwind` **because a panic here would otherwise be a hang, not a crash**: the
/// record would stay `Running` and every `pthread_join` on it would block for ever. Global
/// Constraint 11 makes a panic reachable from guest input Critical; this turns one into a
/// reported thread failure, which a caller can see and act on.
fn run_guest_thread(spawn: Spawn) {
    let Spawn { bionic, boundary, mut cpu, slot, entry, argument, stack, stack_top, window } =
        spawn;
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // D13 again, from the other side: the context already has its thread pointer, which is
        // what makes it legal to run guest code at all on this thread.
        if !bionic.threads_table().adopt(slot) {
            return GuestThreadState::Failed(
                "this host thread already held an arena block, which the runner is the only \
                 caller of and does exactly once"
                    .to_string(),
            );
        }
        let _activation = bionic.activate_slot(slot);
        // Keep this context in the boundary's registry for the whole of its life rather than for
        // one run window. A guest thread runs in many short windows, and a range another thread
        // unmapped between two of them has to reach this one — see `Boundary::watch_context`.
        let _watch = boundary.watch_context();
        cpu.set_sp(stack_top);
        cpu.set_x(XReg::new(0).expect("X0 exists"), argument);
        cpu.set_x(XReg::new(30).expect("X30 exists"), boundary.sentinel() as u64);
        drive(&bionic, &boundary, &mut *cpu, entry, window)
    }));
    let state = match outcome {
        Ok(state) => state,
        Err(payload) => GuestThreadState::Failed(format!(
            "the runner panicked: {}",
            payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a payload of an unknown type".to_string())
        )),
    };

    // Give everything back before anyone is told the thread is over, so a `pthread_join` that
    // returns and immediately creates another thread finds the resources free rather than
    // transiently needing two of them. The 16 MiB-plus context is the one that matters.
    let (base, len) = stack;
    // **Recorded rather than swallowed.** A stack that cannot be given back leaks its address
    // space and its commit charge for the life of the instance, and the thread that leaked it is
    // the only thing that knows. It does not change how the thread ended -- it returned or it did
    // not -- so it is reported beside the outcome rather than instead of it.
    if let Err(error) = bionic.space_ref().unmap(base, len) {
        bionic.record_thread_failure(GuestThreadFailure {
            thread: slot.id.0,
            start_routine: entry,
            why: format!(
                "the guest thread's stack at {base:#x} ({len} bytes) could not be unmapped, so                  its address space and commit charge are leaked for the life of this instance:                  {error}"
            ),
        });
    }
    let _ = bionic.threads_table().detach_current();
    drop(cpu);

    if let GuestThreadState::Failed(ref why) = state {
        bionic.record_thread_failure(GuestThreadFailure {
            thread: slot.id.0,
            start_routine: entry,
            why: why.clone(),
        });
    }
    bionic.finish_guest_thread(slot.id, state);
}

/// Run the start routine in short windows until it returns or the instance asks it to stop.
fn drive(
    bionic: &Arc<Bionic>,
    boundary: &Arc<Boundary>,
    cpu: &mut dyn GuestCpu,
    entry: GuestAddr,
    window: u64,
) -> GuestThreadState {
    let sentinel = boundary.sentinel();
    let mut pc = entry;
    loop {
        if bionic.guest_threads_stopping() {
            return GuestThreadState::Stopped;
        }
        match boundary.run(cpu, pc, RunLimit::Instructions(window)) {
            // The start routine's own `RET` lands on the sentinel, which is how a call into guest
            // code finishes. `X0` is its `void *`.
            Ok(ExitReason::Returned { pc: at }) if at == sentinel => {
                return GuestThreadState::Returned(cpu.x(XReg::new(0).expect("X0 exists")));
            }
            // The window expired with the guest still running: this is the decision point, and
            // the only thing to decide is whether a stop has been requested. Resumable by
            // construction — `Boundary::run` stops an unserviced guest at an instruction
            // boundary and reports where.
            Ok(ExitReason::StepLimitReached { pc: at, .. }) => pc = at,
            Ok(ExitReason::Halted { .. }) => return GuestThreadState::Stopped,
            Ok(other) => return GuestThreadState::Failed(format!("{other:?}")),
            Err(error) => return GuestThreadState::Failed(error.to_string()),
        }
    }
}

// ================================================================== pthread_join

/// `int pthread_join(pthread_t thread, void **retval)`
///
/// Blocks the calling host thread until the named guest thread stops. The POSIX errors are all
/// here and each is a branch guest code has: `ESRCH` for an id no thread answers to, `EDEADLK`
/// for a join that would deadlock, `EINVAL` for a thread that is not joinable.
///
/// **A thread that stopped without returning is a refusal, not a `0`.** There is no `void *` for
/// a thread that faulted, and `0` with a null `retval` would be indistinguishable from a thread
/// that returned `NULL`.
pub(super) fn pthread_join(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (thread, retval) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let call = Call::begin(c)?;
    let bionic = Arc::clone(call.bionic());
    let me = call.state.thread;

    // The out-pointer is checked before the wait, not after: a join that blocked for a second and
    // then refused because the destination was never writable would have wasted the wait, and the
    // thread would already have been reaped.
    let out = if retval == 0 {
        None
    } else {
        let at = guest_address(&call, retval, 1)?;
        call.mem.checked_ptr(at, 8, true, call.blame(1))?;
        Some(at)
    };

    match bionic.join_guest_thread(me, thread) {
        Ok(JoinOutcome::Returned(value)) => {
            if let Some(at) = out {
                call.mem.write_u64(at, value, call.blame(1))?;
            }
            c.ret(|mut r| r.i32(0));
        }
        Ok(JoinOutcome::Errno(errno)) => c.ret(|mut r| r.i32(errno)),
        Err(why) => return call.refuse(why),
    }
    Ok(())
}

/// What a join produced.
pub(crate) enum JoinOutcome {
    /// The thread returned this `void *`.
    Returned(u64),
    /// POSIX's own answer for this call, as the return value.
    Errno(i32),
}

// ================================================================== pthread_detach

/// `int pthread_detach(pthread_t thread)`
///
/// `ESRCH` for an id no thread answers to, and `EINVAL` for one that is not joinable — which is
/// POSIX's own wording ("the value specified by thread does not refer to a joinable thread") and
/// covers both a second `pthread_detach` and a detach of a thread somebody is joining.
///
/// **A second detach is `EINVAL` rather than `0`.** Succeeding twice would make a double detach
/// indistinguishable from a single one, and a double detach is a guest bug that is worth hearing
/// about: in a real implementation it is a use-after-free of the thread's own descriptor.
pub(super) fn pthread_detach(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let thread = c.args().next_u64()?;
    let call = Call::begin(c)?;
    let errno = call.bionic().detach_guest_thread(thread);
    c.ret(|mut r| r.i32(errno));
    Ok(())
}

// ================================================================== pthread_getschedparam

/// `int pthread_getschedparam(pthread_t thread, int *policy, struct sched_param *param)`
///
/// Answers `SCHED_OTHER` with a priority of **0**, for a thread this instance knows about.
///
/// # Why that is an answer rather than a plausible stub, and the argument is that it is forced
///
/// Three facts, and the conclusion follows from them rather than from a preference:
///
/// 1. **Nothing in the reachable 188 can set a scheduling policy.** The set contains
///    `pthread_getschedparam`, `sched_getcpu` and `sched_yield`, and no `pthread_setschedparam`,
///    `pthread_attr_setschedpolicy`, `pthread_attr_setschedparam`, `sched_setscheduler` or
///    `setpriority`. So every thread in this guest's world has the policy it was created with.
/// 2. **That policy is the default**, because nothing here changes a host thread's priority
///    either. On Linux and Android the default is `SCHED_OTHER`, which is `SCHED_NORMAL` and is 0.
/// 3. **`sched_priority` is not a choice for `SCHED_OTHER`.** Linux's
///    `sched_get_priority_min(SCHED_OTHER)` and `sched_get_priority_max(SCHED_OTHER)` are both 0,
///    so the field has exactly one legal value. The nice value is a different thing and is not
///    reported through `struct sched_param`.
///
/// So this is what a stock Android app thread reports, and it is the *only* thing the pair can be
/// given facts 1-3. The paragraph to invalidate is fact 1: if a later phase binds a setter, this
/// stops being an answer and becomes a lie, and it has to grow a real per-thread policy or become
/// a refusal.
///
/// `ESRCH` for an id this instance never handed out — including `pthread_self()` of a host thread
/// that attached without being created by `pthread_create`, which is the main thread. That is
/// deliberate and it is the honest answer: the main thread is not one of this registry's, and
/// reporting a policy for it would be reporting one for a thread the runtime did not start.
///
/// **Inline, unlike its three siblings.** It runs no guest code and touches no mapping, so the
/// exit path would cost it three times as much per call for nothing (D17). Keeping the four
/// thread symbols together on the slow path was the tidier-looking arrangement and would have
/// been an over-correction of exactly the kind `dispatch_paths_are_what_f9_requires` pins in
/// both directions.
pub(super) fn pthread_getschedparam(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (thread, policy, param) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let call = Call::inline(c)?;
    // **Any thread this instance has an identity for**, which is the threads `pthread_create`
    // made *and* the host threads that attached to the instance -- above all the one the
    // initializers run on. Answering `ESRCH` for `pthread_self()` was the first version and it is
    // the wrong answer: the main thread is a thread of this process with the same default policy,
    // so facts 1-3 above apply to it identically, and a guest asking about itself during
    // initialisation would have taken an error branch for no reason.
    let known = call.bionic().knows_guest_thread(thread)
        || call.bionic().threads_table().knows(GuestThreadId(thread));
    if !known {
        c.ret().i32(consts::ESRCH);
        return Ok(());
    }
    let policy_at = guest_address(&call, policy, 1)?;
    let param_at = guest_address(&call, param, 2)?;
    call.mem.write_u32(policy_at, SCHED_OTHER as u32, call.blame(1))?;
    call.mem.write_bytes(param_at, &[0u8; SCHED_PARAM_BYTES], call.blame(2))?;
    c.ret().i32(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rounding a hostile stack size goes through must not wrap.
    ///
    /// `(n + page - 1) & !(page - 1)` gives **0** for a size near `usize::MAX`, and a release
    /// build produces it silently — so `pthread_attr_setstacksize(attr, SIZE_MAX)` would become a
    /// zero-byte stack rather than an `EAGAIN`. That is the `gmtime(i64::MIN)` shape again.
    #[test]
    fn rounding_a_stack_size_up_to_a_page_refuses_rather_than_wrapping() {
        assert_eq!(round_up(0, 4096), Some(0));
        assert_eq!(round_up(1, 4096), Some(4096));
        assert_eq!(round_up(4096, 4096), Some(4096));
        assert_eq!(round_up(4097, 4096), Some(8192));
        assert_eq!(round_up(usize::MAX, 4096), None, "SIZE_MAX must not round to zero");
        assert_eq!(round_up(usize::MAX - 4094, 4096), None);
        // And the exact boundary still works, so the check is a bound rather than a margin.
        let last = usize::MAX - (usize::MAX % 4096);
        assert_eq!(round_up(last, 4096), Some(last));
    }

    /// The policy numbers are stated, and the relations between them hold.
    #[test]
    fn the_thread_policy_numbers_are_what_this_layer_says_they_are() {
        assert_eq!(SCHED_OTHER, 0, "Linux spells it SCHED_NORMAL and it is 0");
        assert_eq!(SCHED_PARAM_BYTES, 4, "struct sched_param is one int on Linux");
        assert_eq!((ATTR_DETACH_STATE, ATTR_STACK_SIZE, ATTR_GUARD_SIZE), (0, 8, 16));
    }
}
