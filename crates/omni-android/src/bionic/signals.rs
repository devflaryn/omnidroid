//! `sigfillset`, and the three signal symbols that are **refused by name**: `sigaction`,
//! `raise`, `pthread_sigmask`.
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
//! So three of the four refuse. **The important thing about them is what they do not do**:
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

/// `int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact)`
///
/// Refused. See this module's documentation: there is no disposition table to install into and
/// no delivery path to reach a handler through, and returning 0 would tell the guest it is going
/// to be notified about a signal it will never hear about.
///
/// The **query** form, `act == NULL`, is refused too. It looks answerable — nothing can have
/// installed a handler, so `SIG_DFL` for everything is arithmetically true — but answering it
/// means writing a `struct sigaction` whose layout has never been checked against a header on
/// this machine, in order to describe a table this runtime does not have. A guest that reads
/// back `SIG_DFL` learns nothing it did not already know, and a guest that *saves and restores*
/// a disposition around a call would be handed a restore that silently does nothing.
pub(super) fn sigaction(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (signum, act, oldact) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
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
