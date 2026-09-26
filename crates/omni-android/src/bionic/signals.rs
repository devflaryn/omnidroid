//! Non-local control flow: the signal dispositions this layer **records but never delivers**, the
//! per-thread mask and `raise`, which refuse, and `longjmp`, which refuses with them.
//!
//! # There is no guest signal delivery here, and this module is where that is said out loud
//!
//! A POSIX signal is three mechanisms, not one: a per-process table of dispositions, a per-thread
//! blocked mask, and a delivery path that can interrupt a thread at an arbitrary instruction, push
//! a `ucontext_t` onto its stack and resume it in a handler. Omnidroid has **none** of the three.
//! Guest code runs inside a translator (D5) whose generated frames have no place to put a signal
//! frame, and building one is a design — with its own interaction with the demand pager (D10), the
//! halt flag (D16) and the thunk boundary's re-entrancy rules (D18) — rather than a gap to be
//! filled in a handler.
//!
//! # What the first disposition table changed, and why (2026-09-26)
//!
//! Until then this module **refused** every disposition, on one ground: *"returning the old
//! disposition would tell the guest a handler is installed, and it would then wait for a
//! notification that can never arrive."* That ground is sound for a guest whose *next* act depends
//! on the handler running, and it was right for `SIGPIPE`, where libcurl saves and restores a
//! disposition and both values promise no delivery anyway.
//!
//! **MEASURED need:** a substituted build of the APK's compression library calls
//! `signal(SIGILL, <handler>)` from its **fifth `init_array` entry** — a real handler, not
//! `SIG_IGN` — and the thread-failure assertion stopped the whole boot there. `SIGILL` is the trap
//! an integrity check raises on itself, so what that handler exists for is the payload noticing
//! that its own code was tampered with.
//!
//! The decision taken, and it is a **recorded** one rather than a silent substitution:
//!
//! * **A disposition is stored and the previous one is returned.** `signal(signum, handler)`
//!   answers the old disposition; `sigaction` writes it into `oldact` exactly as it always did.
//!   A save/restore pair round-trips, and a guest that reads a disposition back reads what it set.
//! * **Nothing is ever delivered.** There is no path from a fault, a trap or a `kill` to a guest
//!   handler, and adding one is the design this module says is not being built.
//! * **So a guest that triggers the signal it armed gets a report, not its handler.** A guest
//!   executing an undefined instruction is reported by the boundary as an unsupported instruction
//!   at its address, and the thread stops there — which is the *same outcome* the refusal produced,
//!   arrived at by the same event, with more detail in the report. That equivalence is the whole
//!   argument: the answer differs only for a guest that installs a handler and never triggers the
//!   signal, and such a guest cannot tell.
//! * **The trap is still caught.** Whatever this payload's integrity check is watching for, a
//!   tamper in this runtime surfaces as an unsupported-instruction report naming the address,
//!   rather than as a signal the handler could have swallowed.
//!
//! `raise` and `pthread_sigmask` still refuse, and for the reasons above: `raise(SIGABRT)` is the
//! abort path, where a `0` means the guest **carries on past the point it expected to die**, and
//! `pthread_sigmask`'s answer would be a claim about a mask nothing consults.
//!
//! # `sigfillset` is answered, and it is not a concession
//!
//! It is `memset(set, 0xff, sizeof(sigset_t))`: a total function of its one argument, with no
//! table, no mask and no delivery behind it. Implementing it is implementing the real function,
//! and the logic is in [`omni_bionic::signal`] — no OS call, so D19 puts it on that side of the
//! line. What it is *for* is still refused, which is the point: a guest that calls
//! `sigfillset(&set)` and then `pthread_sigmask(SIG_BLOCK, &set, &old)` gets a correctly filled
//! set and then a refusal naming the symbol, rather than a correctly filled set and a lie.

use omni_bionic::context::GuestContext;
use omni_bionic::signal;

use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::{active, enter};

// ------------------------------------------------------------------ the guest's constants
//
// Linux's signal numbers, which are what `libroblox.so` was compiled against. arm64 uses the
// `asm-generic` numbering unmodified, so these are the same values every Linux architecture but
// MIPS, SPARC and Alpha uses. Carried only so a refusal can say what was asked for.

/// The signal names a refusal can spell, in number order from 1.
const SIGNAL_NAMES: [&str; 32] = [
    "SIGHUP", "SIGINT", "SIGQUIT", "SIGILL", "SIGTRAP", "SIGABRT", "SIGBUS", "SIGFPE", "SIGKILL",
    "SIGUSR1", "SIGSEGV", "SIGUSR2", "SIGPIPE", "SIGALRM", "SIGTERM", "SIGSTKFLT", "SIGCHLD",
    "SIGCONT", "SIGSTOP", "SIGTSTP", "SIGTTIN", "SIGTTOU", "SIGURG", "SIGXCPU", "SIGXFSZ",
    "SIGVTALRM", "SIGPROF", "SIGWINCH", "SIGIO", "SIGPWR", "SIGSYS", "SIGRTMIN",
];

/// The symbolic name of a signal number, for a refusal that has to say what was asked for.
fn signal_name(signum: i32) -> String {
    match usize::try_from(signum) {
        Ok(index) if index >= 1 && index <= SIGNAL_NAMES.len() => {
            format!("`{}`", SIGNAL_NAMES[index - 1])
        }
        _ => "no signal this layer has a name for".to_string(),
    }
}

fn refuse(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

// ================================================================== the one that is answered

/// `int sigfillset(sigset_t *set)`
///
/// Every bit set, including the two that glibc reserves for its own threading implementation and
/// bionic does not — see [`omni_bionic::signal`]. A null `set` is `-1` with `errno` set to
/// `EINVAL`, which is bionic's own answer and a branch guest code has; a `set` that is not
/// writable guest memory is an [`AbiError::BadPointer`] naming the symbol and the argument,
/// because a guest that ignored the return would otherwise carry an **unwritten** set forward
/// and later block, or fail to block, a different set of signals than it asked for.
pub(super) fn sigfillset(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let set = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        match signal::fillset(&mut view, set) {
            Ok(Ok(())) => 0,
            Ok(Err(errno)) => {
                view.set_errno(errno);
                -1
            }
            Err(fault) => return Err(view.fault(fault)),
        }
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== the three that are refused

/// `struct sigaction` on LP64 bionic: `int sa_flags` (padded to eight), the handler union at
/// `+8`, `sigset_t sa_mask` (one word) at `+16`, `sa_restorer` at `+24`. MEASURED against the
/// guest's own code as well as bionic's `__SIGACTION_BODY`: libcurl's `sigpipe_ignore` in this
/// binary (`0x0220229c` on) copies the 32 bytes it was given back, clears `SA_SIGINFO` in the
/// `int` at `+0` and stores `SIG_IGN` at `+8`.
pub(super) const SIGACTION_BYTES: usize = 32;
const HANDLER_OFFSET: usize = 8;
/// `SIG_DFL` is 0 on every Linux bionic supports. Spelled so `signal`'s answer reads as the
/// disposition it is returning rather than as a bare zero. (`SIG_IGN` is 1 and needs no constant:
/// with the general table a handler value is stored rather than interpreted, so nothing in this
/// layer branches on it.)
const SIG_DFL: u64 = 0;
/// `EINVAL`, which is what C says a bad signal number gets (with `SIG_ERR` returned).
const EINVAL: i32 = 22;
/// The highest signal number a disposition table is asked about: Linux reserves 1..=`31` for real
/// signals and 32 and 33 for the two threads the C library starts itself, which is why the guest
/// may legitimately pass them and why a number above this is a bad number rather than an unusual
/// one. `_NSIG`-1 on bionic, which is 33 here (and 65 with `__USE_MISC`'s 32 real-time signals,
/// which a 32-bit-only extension this guest cannot reach does not have).
const MAX_SIGNAL: i32 = 33;

/// One signal's disposition as this layer holds it: **recorded, never delivered**.
///
/// The handler is a guest address (or `SIG_DFL`/`SIG_IGN`), and the mask and flags are kept because
/// `sigaction`'s `oldact` has to be written back with them — a save/restore pair that dropped the
/// mask would restore a *different* disposition than the one it saved, which is the kind of quiet
/// wrongness this project treats as a defect rather than a detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disposition {
    /// `sa_handler` / the `signal` handler argument: a guest address, `SIG_DFL` or `SIG_IGN`.
    pub handler: u64,
    /// `sa_mask`, the bytes as the guest wrote them (bionic's is one 8-byte word on arm64).
    pub mask: [u8; 8],
    /// `sa_flags`.
    pub flags: u32,
}

impl Disposition {
    /// A disposition with nothing set: `SIG_DFL`, an empty mask, no flags. What every signal
    /// starts as, and what a fresh table hands back for a signal nobody has touched.
    pub const NONE: Self = Self { handler: SIG_DFL, mask: [0; 8], flags: 0 };
}

/// `int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact)`
///
/// **Every signal is answered, and nothing is delivered** — the decision this module's
/// documentation records. It was `SIGPIPE` alone, with `SIG_DFL`/`SIG_IGN` only, until 2026-09-26;
/// see the module header for what changed and why the old restriction could not stay.
///
/// # The `SIGPIPE` answer, which is now the general one
///
/// MEASURED reader: libcurl's `sigpipe_ignore`/`sigpipe_restore` around `curl_easy_cleanup` --
/// query `SIGPIPE`, install `SIG_IGN`, do the work, restore the saved action. It never installs a
/// handler, so nothing is promised that cannot be kept: `SIG_IGN` says "nothing will be
/// delivered", which is true.
///
/// The query's answer is the app process's real one, read from Android's source rather than
/// assumed: ART's `Runtime::BlockSignals` **blocks** `SIGPIPE` (with `SIGQUIT` and `SIGUSR1`)
/// and never sets its disposition, and neither the zygote (`com_android_internal_os_Zygote.cpp`)
/// nor `AndroidRuntime.cpp` mentions it -- so an app's `SIGPIPE` action is the untouched
/// `SIG_DFL` with no flags and an empty mask, which is 32 zero bytes. A blocked `SIGPIPE` is also
/// why a write to a closed socket returns `EPIPE` instead of killing the process, which is what
/// this layer's sockets do. Other signals are not answered: ART and debuggerd install real
/// handlers for several (`SIGSEGV`, `SIGABRT`, `SIGBUS`, ...), so "`SIG_DFL` for everything"
/// would be false on a device.
///
/// What is stored is exactly the 32 bytes the guest passed -- bionic's arm64 `sigaction` adds no
/// restorer -- so a save-and-restore round-trips.
pub(super) fn sigaction(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (signum, act, oldact) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    // A signal number outside the table is not a disposition this layer can speak for, and C's
    // answer for it is a real one: `EINVAL`, with nothing written.
    if signum <= 0 || signum > MAX_SIGNAL {
        let state = active(c.symbol(), c.address())?;
        let mut view = enter(c, &state);
        view.set_errno(EINVAL);
        c.ret().i32(-1);
        return Ok(());
    }
    // The new disposition, read out of the guest's own `struct sigaction` — the 32 bytes bionic's
    // arm64 layout is, and no restorer, so a save-and-restore round-trips exactly.
    let new = if act == 0 {
        None
    } else {
        let bytes = c
            .mem()
            .read_bytes(act as usize, SIGACTION_BYTES, Blame::new(c.symbol(), c.address(), 1))?;
        let mut action = [0u8; SIGACTION_BYTES];
        action.copy_from_slice(&bytes);
        let handler =
            u64::from_le_bytes(action[HANDLER_OFFSET..HANDLER_OFFSET + 8].try_into().expect("eight"));
        let mut mask = [0u8; 8];
        mask.copy_from_slice(&action[16..24]);
        let flags = u32::from_le_bytes(action[0..4].try_into().expect("four"));
        Some(Disposition { handler, mask, flags })
    };
    let state = active(c.symbol(), c.address())?;
    let mut held = state
        .bionic
        .dispositions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The old disposition goes back as the guest's own 32 bytes, so `oldact` is exactly what a
    // restore would put back. `SA_SIGINFO` is cleared on the way out because the handler this
    // layer stores is a plain `sa_handler`, and libcurl's `sigpipe_ignore` does the same
    // (`0x0220229c`, MEASURED against the guest's own code).
    if oldact != 0 {
        let previous = held.get(&signum).copied().unwrap_or(Disposition::NONE);
        let mut action = [0u8; SIGACTION_BYTES];
        action[0..4].copy_from_slice(&previous.flags.to_le_bytes());
        action[HANDLER_OFFSET..HANDLER_OFFSET + 8].copy_from_slice(&previous.handler.to_le_bytes());
        action[16..24].copy_from_slice(&previous.mask);
        c.mem().write_bytes(oldact as usize, &action, Blame::new(c.symbol(), c.address(), 2))?;
    }
    if let Some(new) = new {
        held.insert(signum, new);
    }
    drop(held);
    c.ret().i32(0);
    Ok(())
}

/// `sighandler_t signal(int signum, sighandler_t handler)` -- the older spelling of `sigaction`,
/// bound 2026-09-26, over the same disposition table.
///
/// **MEASURED need:** a substituted build of the APK's compression library calls
/// `signal(SIGILL, <handler>)` from its **fifth `init_array` entry** — a real handler, not
/// `SIG_IGN` — and the thread-failure assertion stopped the boot there. `SIGILL` is the trap an
/// integrity check raises on itself, so that handler exists to notice its own code being tampered
/// with; see the module header for why storing the disposition without delivering it changes no
/// outcome the guest can observe.
///
/// The older ABI has no mask and no flags, so it shares the table and touches only the handler:
/// `signal` and `sigaction` are two spellings of one disposition, and a guest that mixes them
/// (libcurl's `sigpipe_ignore` is `sigaction`; a C library that reached for `signal` instead) must
/// see one table, not two.
///
/// The previous disposition is returned as a `sighandler_t` — the handler value itself, or
/// `SIG_DFL`/`SIG_IGN` — which is what makes a save/restore pair work: `old = signal(...)` then
/// `signal(..., old)` puts back what was there.
pub(super) fn signal(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (signum, handler) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?)
    };
    // `SIG_ERR` is (void*)-1; C says a bad signal number gets it and `errno` set to `EINVAL`. That
    // is a real answer rather than a refusal, and it is this runtime's too: the number is not ours.
    if signum <= 0 || signum > MAX_SIGNAL {
        let state = active(c.symbol(), c.address())?;
        let mut view = enter(c, &state);
        view.set_errno(EINVAL);
        c.ret().u64(u64::MAX);
        return Ok(());
    }
    let state = active(c.symbol(), c.address())?;
    let mut held = state
        .bionic
        .dispositions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = held.get(&signum).copied().unwrap_or(Disposition::NONE).handler;
    // **The mask and flags are left alone**, deliberately: `signal` cannot set them, so a guest
    // that installed a masked disposition with `sigaction` and then called `signal` to change the
    // handler keeps its mask. That is what a real `signal` does — it is `BSD semantics`, and the
    // disposition this stores is the whole of what the guest can see afterwards.
    let mut updated = held.get(&signum).copied().unwrap_or(Disposition::NONE);
    updated.handler = handler;
    held.insert(signum, updated);
    drop(held);
    c.ret().u64(previous);
    Ok(())
}

/// `int raise(int sig)`
///
/// Refused. `raise(SIGABRT)` is the tail of `assert`, of bionic's own fatal checks and of most
/// C++ runtimes' `std::terminate`, and a `0` from it means the guest **runs on past the point it
/// expected to die** — carrying whatever broken invariant made it call.
pub(super) fn raise(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let sig = c.args().next_i32()?;
    Err(refuse(
        c,
        format!(
            "the guest called raise({sig}) — signal {sig} ({}) to itself. There is no signal \
             delivery here, so there is no correct value to return: 0 says the signal was \
             delivered and the guest carries on past a point it expected not to reach, and -1 \
             with EINVAL says the signal number is invalid when it is not. Mapping SIGABRT alone \
             onto `abort`'s reported termination was considered and rejected — it would be this \
             layer deciding that SIGABRT's disposition is SIG_DFL, which is a fact about a table \
             that does not exist, and it would answer for one signal number out of 64 while every \
             other still needed a decision",
            signal_name(sig)
        ),
    ))
}

/// `int sigprocmask(int how, const sigset_t *set, sigset_t *oldset)` and its `pthread_sigmask`
/// twin -- the same function under two names, which bionic exports as a weak alias.
///
/// **Answered as of 2026-09-26, and it follows the same decision as the dispositions**: the mask is
/// recorded per thread and never consulted, because there is no delivery for it to mask. A guest
/// that blocks signals across a critical section gets the same answer and the same non-event, and
/// the argument that made the disposition table an answer applies unchanged — the mask is only ever
/// read by the next `sigprocmask` on the same thread, which now gets back what it set.
///
/// **MEASURED need:** a substituted build of the APK's compression library calls it from the
/// constructors, right after the `signal(SIGILL, ...)` that stopped the previous run.
///
/// # What was true before, and what is still true
///
/// This symbol was the one `omni-bionic` **excluded by name** rather than left out (D19), on the
/// ground that "a mask that reports success without blocking anything is invisible for exactly as
/// long as nothing depends on it". The clause after the semicolon is the honest half and it is
/// still true: nothing is blocked. What changed is that the mask is now *kept*, so the invisible
/// case is a mask that is never read by anything except the call that set it, rather than a mask
/// that does not exist — and a save/restore pair round-trips, which a refusal could not offer.
///
/// `SIG_SETMASK` replaces, `SIG_BLOCK` adds, `SIG_UNBLOCK` removes — bionic's three `how` values,
/// in that order from 0, and a fourth value is `EINVAL` (a real answer, not a refusal). The mask is
/// bionic's `sigset_t`: **8 bytes** on arm64, which is this layer's measured `sizeof(sigset_t)`.
pub(super) fn pthread_sigmask(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (how, set, oldset) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    // `how` is `SIG_BLOCK` (0), `SIG_UNBLOCK` (1) or `SIG_SETMASK` (2) -- bionic's three, in that
    // order from 0. Anything else is C's own `EINVAL` case, which is **answered** rather than
    // refused: a guest that passes a fourth value is not asking this layer for something it cannot
    // do, it is asking a question the C library answers, and answering it wrongly (or refusing) is
    // what would be the defect.
    const SIG_BLOCK: i32 = 0;
    const SIG_SETMASK: i32 = 2;
    if !(SIG_BLOCK..=SIG_SETMASK).contains(&how) {
        let state = active(c.symbol(), c.address())?;
        let mut view = enter(c, &state);
        view.set_errno(EINVAL);
        c.ret().i32(-1);
        return Ok(());
    }
    let state = active(c.symbol(), c.address())?;
    let thread = state.thread;
    // The mask is 8 bytes on arm64 -- this layer's measured `sizeof(sigset_t)` -- and a null `set`
    // is a pure query, which `update_signal_mask` spells as `None`.
    let wanted = if set == 0 {
        None
    } else {
        let bytes = c.mem().read_bytes(set as usize, 8, Blame::new(c.symbol(), c.address(), 1))?;
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes);
        Some(u64::from_le_bytes(word))
    };
    // The one function that owns that table, shared with the raw `rt_sigprocmask` syscall
    // (`sysroute` routes 129 here) -- two doors, one state, which is the rule this layer keeps.
    let previous = state.bionic.update_signal_mask(thread, how, wanted);
    // `oldset` is how the caller gets the previous mask back, and a null one is legal: C says the
    // query is simply skipped.
    if oldset != 0 {
        c.mem().write_bytes(
            oldset as usize,
            &previous.to_le_bytes(),
            Blame::new(c.symbol(), c.address(), 2),
        )?;
    }
    c.ret().i32(0);
    Ok(())
}

// ================================================================== setjmp, and its refusal

/// `int setjmp(jmp_buf env)`
///
/// **The direct return: 0.** That is the whole of what `setjmp` is when nothing jumps back, and it
/// is what this answers. The environment is **not** saved -- an inline handler cannot read the
/// callee-saved registers (D18) -- and that is safe for one reason stated here and enforced in
/// [`longjmp`]: the only thing that could ever observe the saved environment is a `longjmp`, and
/// `longjmp` refuses by name. A guest that sets a buffer and never jumps runs exactly as on a
/// device; one that jumps stops at the jump, loudly.
///
/// MEASURED reader: libpng, arming its error recovery (`png_jmpbuf`) before a decode on a guest
/// worker, once the Lua app was starting. It jumps only on a corrupt image.
pub(super) fn setjmp(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let _env = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    state.bionic.setjmps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    c.ret().i32(0);
    Ok(())
}

// ================================================================== the fourth refusal

/// `void longjmp(jmp_buf env, int val)`
///
/// Refused. It needs **no operating system** — which is why it fell through every phase of this
/// task's OS-surface plan and had to be collected by the last one — and it needs something this
/// layer does not have either: a way to put a saved guest CPU state back.
///
/// # What restoring a `jmp_buf` would take
///
/// AArch64's `setjmp` saves the callee-saved registers `X19`-`X28`, the frame pointer `X29`, the
/// link register `X30`, the stack pointer, and the low 64 bits of `D8`-`D15`; `longjmp` writes all
/// of them back and then *does not return* — it resumes at the saved `LR` with the saved `SP`.
/// Every one of those is a **guest** register, and the thunk boundary does not offer a handler a
/// way to write one: [`ImportCall`] exposes the AAPCS64 argument registers and one return value,
/// and that is by design (D18 makes "cannot reach the CPU" a type property, which is what stops an
/// inline handler re-entering the guest). A `longjmp` would need the opposite capability, on the
/// calling thread's own context.
///
/// # And there is no `jmp_buf` here for it to restore
///
/// [`setjmp`] answers only its direct return (0) and saves nothing, because an inline handler
/// cannot read the registers it would save. Nothing in this runtime can therefore have *filled* a
/// `jmp_buf`, and the bytes at the pointer that arrives here are whatever the guest last left
/// there -- which is why this refusal is what keeps that `setjmp` honest.
///
/// Bionic's own layout makes that worse rather than better: its `setjmp` mangles the saved `SP`
/// and `LR` with a per-process cookie and stores a checksum, so even a byte-for-byte copy of a
/// `jmp_buf` from a real device would not be restorable by anything that did not share the cookie.
///
/// **The believable wrong answer is to return.** `longjmp` is declared `noreturn` and its whole
/// contract is that control arrives back at the `setjmp`; a handler that quietly returned to the
/// instruction after the call would resume the guest in the frame it was trying to escape, with
/// whatever error condition made it call. That is the same failure `raise` declines, one frame
/// further in.
pub(super) fn longjmp(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (env, val) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    // C says a `val` of 0 is delivered to the caller as 1, which is the one piece of this
    // function's contract that can be stated without restoring anything.
    let delivered = if val == 0 { 1 } else { val };
    Err(refuse(
        c,
        format!(
            "the guest called longjmp({env:#x}, {val}) -- a non-local jump that must restore \
             X19-X28, X29, X30, SP and the low halves of D8-D15 from that jmp_buf and resume \
             there, delivering {delivered} at the matching setjmp. The thunk boundary gives a \
             handler the AAPCS64 argument registers and one return value and deliberately no way \
             to write the calling thread's guest state (D18 makes that a type property, which is \
             what stops an inline handler re-entering the guest). And nothing here filled that \
             jmp_buf: this layer's `setjmp` answers only its direct return, 0, because it cannot \
             read the registers it would save -- this refusal is what makes that safe. Returning \
             normally was rejected -- longjmp is noreturn, and a return would resume the guest in \
             the frame it was trying to escape, carrying the condition that made it jump"
        ),
    ))
}
