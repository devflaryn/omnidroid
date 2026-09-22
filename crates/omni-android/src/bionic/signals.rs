//! Non-local control flow: `sigfillset`, the three signal symbols that are **refused by name** --
//! `sigaction`, `raise`, `pthread_sigmask` -- and `longjmp`, which is refused with them.
//!
//! The four refusals here are one family rather than a bucket. Each is a way of transferring
//! control somewhere the ordinary call and return does not reach: a signal handler, or a stack
//! frame that has already been left. Omnidroid has a mechanism for neither, and in both cases the
//! believable wrong answer is the one that lets the guest **carry on past a point it expected not
//! to reach**.
//!
//! # There is no guest signal delivery here, and this module is where that is said out loud
//!
//! A POSIX signal is three mechanisms, not one: a per-process table of dispositions, a
//! per-thread blocked mask, and a delivery path that can interrupt a thread at an arbitrary
//! instruction, push a `ucontext_t` onto its stack and resume it in a handler. Omnidroid has
//! none of the three. Guest code runs inside a translator (D5) whose generated frames have no
//! place to put a signal frame, and building one is a design — with its own interaction with
//! the demand pager (D10), the halt flag (D16) and the thunk boundary's re-entrancy rules
//! (D18) — rather than a gap to be filled in a handler.
//!
//! So three of the four refuse -- `sigaction` with one measured exception: `SIGPIPE`'s `SIG_DFL`
//! and `SIG_IGN`, which promise no delivery and are what libcurl saves and restores (see
//! [`sigaction`]). **The important thing about the refusals is what they do not do**:
//! each has a believable wrong answer sitting right next to it, and each of those answers is
//! *the* failure Global Constraint 1 exists for, because it is not observable until much later
//! and somewhere else.
//!
//! | symbol | the believable wrong answer | what it would cost |
//! |---|---|---|
//! | `sigaction` | return 0, "handler installed" | the guest believes it will be told about `SIGSEGV`, `SIGPIPE` or `SIGABRT`. It never will, and the code that would have recovered is simply never reached |
//! | `raise` | return 0, "signal delivered" | `raise(SIGABRT)` is the abort path of assert failures and of bionic's own fatal checks. A 0 means the guest **carries on past the point it expected to die**, with whatever invariant it had just found broken |
//! | `pthread_sigmask` | return 0 and write an empty old mask | the guest believes signals are blocked across a critical section. Nothing is blocked, and nothing is delivered either, so the lie is invisible until something depends on the unblock |
//!
//! `pthread_sigmask` was already excluded from `omni-bionic` for exactly this reason (D19, and
//! that crate's `metadata` module says so). The other two are the same family and get the same
//! answer.
//!
//! **`raise` and `abort` are not the same call, and the refusal says so.** `abort` *is* bound —
//! it reports a termination rather than performing one (`procenv`), because the runtime hosts
//! several guest instances in one process and a host `abort()` would take all of them. It would
//! be easy to route `raise(SIGABRT)` onto that path and call the job done. It is not done: that
//! would be this layer deciding that `SIGABRT`'s disposition is `SIG_DFL`, which is a fact about
//! a signal table that does not exist, and it would answer only for one of 64 signal numbers
//! while every other number still needed a decision. The refusal names the signal and points at
//! `abort`, which is the reader's next question.
//!
//! # `sigfillset` is the exception, and it is not a concession
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

/// `SIG_BLOCK`, `SIG_UNBLOCK`, `SIG_SETMASK` — bionic's `how` values, in that order from 0.
const SIGPROCMASK_HOW: [&str; 3] = ["SIG_BLOCK", "SIG_UNBLOCK", "SIG_SETMASK"];

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
const SIGPIPE: i32 = 13;
/// `SIG_DFL` and `SIG_IGN`: the two dispositions that are not a function to deliver to.
const SIG_IGN: u64 = 1;

/// `int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact)`
///
/// **`SIGPIPE` alone is answered, and only with `SIG_DFL` or `SIG_IGN`.** Everything else is
/// refused, for the reason this module's documentation gives: there is no delivery path to reach
/// a handler through, and returning 0 for one would tell the guest it will be notified about a
/// signal it will never hear about.
///
/// # Why `SIGPIPE` can be answered
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
    if signum == SIGPIPE {
        let new = if act == 0 {
            None
        } else {
            let bytes =
                c.mem().read_bytes(act as usize, SIGACTION_BYTES, Blame::new(c.symbol(), c.address(), 1))?;
            let mut action = [0u8; SIGACTION_BYTES];
            action.copy_from_slice(&bytes);
            let handler = u64::from_le_bytes(
                action[HANDLER_OFFSET..HANDLER_OFFSET + 8].try_into().expect("eight bytes"),
            );
            (handler <= SIG_IGN).then_some(action)
        };
        if act == 0 || new.is_some() {
            let state = active(c.symbol(), c.address())?;
            let mut held = state
                .bionic
                .sigpipe_action
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if oldact != 0 {
                c.mem().write_bytes(
                    oldact as usize,
                    &*held,
                    Blame::new(c.symbol(), c.address(), 2),
                )?;
            }
            if let Some(action) = new {
                *held = action;
            }
            drop(held);
            c.ret().i32(0);
            return Ok(());
        }
    }
    let shape = if act == 0 {
        "querying the current disposition"
    } else {
        "installing a handler"
    };
    Err(refuse(
        c,
        format!(
            "the guest called sigaction({signum}, {act:#x}, {oldact:#x}) — {} for signal {signum} \
             ({}). Omnidroid has no guest signal delivery: no disposition table, no per-thread \
             mask, and no way to interrupt translated guest code at an arbitrary instruction and \
             resume it in a handler. Returning 0 would report that a handler is installed, and \
             the guest would then wait for a notification that can never arrive — which is not \
             observable until the fault it was registered for happens. `abort` and \
             `__stack_chk_fail` ARE bound and report a guest termination through the boundary; \
             that is the one death path this layer models",
            shape,
            signal_name(signum)
        ),
    ))
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

/// `int pthread_sigmask(int how, const sigset_t *set, sigset_t *oldset)`
///
/// Refused, and it is the symbol `omni-bionic` **excluded by name** for this exact reason (D19):
/// it needs the guest's real signal state, which is the thing that does not exist.
pub(super) fn pthread_sigmask(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (how, set, oldset) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let named = usize::try_from(how)
        .ok()
        .and_then(|index| SIGPROCMASK_HOW.get(index))
        .map_or_else(|| "no `how` this layer has a name for".to_string(), |name| format!("`{name}`"));
    Err(refuse(
        c,
        format!(
            "the guest called pthread_sigmask({how}, {set:#x}, {oldset:#x}) — {named}. There is no \
             per-thread signal mask here because there is no signal delivery to mask: see \
             `sigaction`, refused for the same reason. This is the one symbol `omni-bionic` \
             excluded by name rather than left out (D19), because a mask that reports success \
             without blocking anything is invisible for exactly as long as nothing depends on it"
        ),
    ))
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
