//! `ALooper`: the seven symbols `jni-surface.md` §5.2 needs before a game thread can exist.
//!
//! `ALooper_prepare`, `_forThread`, `_acquire`, `_release`, `_addFd`, `_removeFd`, `_pollOnce`.
//! §8 rows 13a and 14 name six of them and the seventh follows on the game thread; none is among
//! the 188 the initializers reach.
//!
//! # What a looper is here, and what it deliberately is not
//!
//! On a device an `ALooper` is an epoll set plus a wake pipe plus a message queue with delayed
//! sends. **This is the descriptor half and nothing else**, because the descriptor half is all the
//! GameActivity glue uses: `initializeNativeCode` registers `msgread` with a callback,
//! `android_app_entry` registers the glue's own command pipe with an ident, and
//! `NativeEngine::GameLoop` drives `ALooper_pollOnce`. Nothing on the startup path sends a
//! message, posts a delayed callback or calls `ALooper_wake`, and none of those three symbols is
//! imported at all (`apk-analysis.md` §4.4 lists the seven, exhaustively).
//!
//! So `ALOOPER_POLL_WAKE` is a value this implementation **never returns**, and that is stated
//! rather than left to be inferred: there is no `wake` to produce it. A layer that returned it
//! speculatively would be telling the glue its own message arrived.
//!
//! # Readiness comes from the filesystem seam, and that coupling is deliberate
//!
//! A looper watches descriptors, and every descriptor in this runtime belongs to
//! `omni-platform`'s table, which the **bionic** activation owns. So `ALooper_addFd` and
//! `ALooper_pollOnce` ask [`crate::bionic`] for it. A host that activates the looper registry and
//! not bionic gets a refusal naming both rather than a looper that watches nothing.
//!
//! That also means the wait is the same wait `poll` performs — `omni-platform`'s readiness gate,
//! with the generation read **before** the descriptors are tested — and, for a *bounded* poll,
//! the same bound: [`MAX_SLEEP_SECONDS`](crate::bionic::MAX_SLEEP_SECONDS).
//!
//! # The indefinite `pollOnce`, and the fact it is decided on
//!
//! `ALooper_pollOnce(-1, ..)` is what `NativeEngine::GameLoop` calls, and it was refused here
//! until M6 on the argument that D16's runaway-guest defence is built from step budgets a
//! sleeping thread does not consume, so a host thread parked on a pipe nobody writes to cannot be
//! ended by anything this runtime has.
//!
//! **That argument's premise is conditional, and this call can now test it.** The game thread is
//! not idling — it is waiting for the *main* thread to post `APP_CMD_INIT_WINDOW` down the pipe
//! `initializeNativeCode` created (`jni-surface.md` §8 rows 17-20). An indefinite wait on that
//! pipe is legitimate exactly when the pipe can still be written, and that is a fact rather than
//! a hope: [`Filesystem::pipe_writers`](omni_platform::fs::Filesystem::pipe_writers) counts the
//! descriptors in this instance that still hold the write end. So:
//!
//! * **At least one watched descriptor must be a pipe read end with a live write end.** If none
//!   is, nothing in this runtime can ever make the poll return and the original refusal still
//!   holds — with the descriptors it looked at named, so the refusal says *why* rather than
//!   restating the rule.
//! * **The wait is sliced** (`WAIT_SLICE`) and re-reads the stop switch every pass, so
//!   [`Bionic::stop_guest_threads`](crate::bionic::Bionic::stop_guest_threads) ends it. Without
//!   that, an indefinite poll would be exactly the unstoppable park D16 objects to: the switch is
//!   read between run windows and a parked thread never ends one.
//!
//! The last writer closing does not strand the wait either: a read end with no writers reports
//! **readable** at end of file (`omni-platform`'s pipe table), so the poll returns, the glue
//! drains, and the *next* indefinite poll is the one that gets refused.
//!
//! Capping the wait and returning [`ALOOPER_POLL_TIMEOUT`] remains rejected on this project's own
//! rule: it reports a timeout to a call that was given none, which is the believable wrong answer
//! for this shape.

use std::time::{Duration, Instant};

use omni_cpu::RunLimit;
use omni_mem::GuestAddr;

use crate::abi::Args;
use crate::boundary::{GuestArg, ImportCall, ImportFn, ReentrantCall, ReentrantFn};
use crate::error::{AbiError, AbiResult};

use super::{active, Ndk, NdkState, MAX_LOOPER_FDS};

// ================================================================== the NDK's own constants
//
// `android/looper.h`. The same provenance as the `O_*` flags in `bionic::files` and the `POLL*`
// bits in `bionic::net`: the guest was compiled against these numbers, so they are the guest's
// ABI rather than this layer's choice.

/// `ALOOPER_PREPARE_ALLOW_NON_CALLBACKS`: the looper may return idents from `pollOnce`.
pub const ALOOPER_PREPARE_ALLOW_NON_CALLBACKS: i32 = 1;

/// `ALOOPER_POLL_WAKE`: the poll was woken by `ALooper_wake`.
///
/// **Never returned here.** There is no `wake`: it is not among the seven symbols the engine
/// imports, so nothing can produce the condition. See this module's documentation.
pub const ALOOPER_POLL_WAKE: i32 = -1;
/// `ALOOPER_POLL_CALLBACK`: one or more callbacks were invoked.
pub const ALOOPER_POLL_CALLBACK: i32 = -2;
/// `ALOOPER_POLL_TIMEOUT`: the timeout expired with nothing ready.
pub const ALOOPER_POLL_TIMEOUT: i32 = -3;
/// `ALOOPER_POLL_ERROR`: an error occurred.
pub const ALOOPER_POLL_ERROR: i32 = -4;

/// `ALOOPER_EVENT_INPUT`: the descriptor is readable.
pub const ALOOPER_EVENT_INPUT: i32 = 1;
/// `ALOOPER_EVENT_OUTPUT`: the descriptor is writable.
pub const ALOOPER_EVENT_OUTPUT: i32 = 2;
/// `ALOOPER_EVENT_ERROR`: an error occurred on the descriptor.
pub const ALOOPER_EVENT_ERROR: i32 = 4;
/// `ALOOPER_EVENT_HANGUP`: the descriptor's peer has closed.
pub const ALOOPER_EVENT_HANGUP: i32 = 8;
/// `ALOOPER_EVENT_INVALID`: the descriptor is not valid.
pub const ALOOPER_EVENT_INVALID: i32 = 16;

/// The events a caller may ask to be told about.
///
/// `ERROR`, `HANGUP` and `INVALID` are **output only** and are reported whether or not they were
/// requested, which is `poll`'s own rule and the reason the canonical drain loop learns that its
/// writer has gone. The same statement `bionic::net`'s `READY_MASK` makes, one layer up.
const REQUESTABLE_EVENTS: i32 = ALOOPER_EVENT_INPUT | ALOOPER_EVENT_OUTPUT;

// ================================================================== the looper

/// One descriptor a looper watches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdRegistration {
    /// The descriptor.
    pub fd: i32,
    /// The identifier `pollOnce` returns for it, or [`ALOOPER_POLL_CALLBACK`] when it has a
    /// callback — which is what AOSP stores, because a registration with a callback can never be
    /// reported by ident.
    pub ident: i32,
    /// The events the caller asked about.
    pub events: i32,
    /// The guest callback, or `0`.
    pub callback: GuestAddr,
    /// The opaque `void *` handed back to the callback.
    pub data: u64,
}

/// One looper: the descriptors it watches and how many references are held to it.
#[derive(Debug)]
pub struct Looper {
    /// Which guest thread it belongs to, as this instance numbers them.
    thread: usize,
    /// The `opts` it was prepared with.
    opts: i32,
    /// References, as `ALooper_acquire` and `_release` count them.
    ///
    /// **One at creation**, which is the thread's own: AOSP's `Looper::prepare` stores the looper
    /// in thread-local storage with a strong reference, and `ALooper_acquire` adds the caller's.
    references: i64,
    fds: Vec<FdRegistration>,
}

impl Looper {
    pub(super) fn new(thread: usize, opts: i32) -> Looper {
        Looper { thread, opts, references: 1, fds: Vec::new() }
    }

    /// The descriptors this looper watches.
    #[must_use]
    pub fn registrations(&self) -> &[FdRegistration] {
        &self.fds
    }

    /// How many references are held.
    #[must_use]
    pub fn references(&self) -> i64 {
        self.references
    }

    /// The `opts` it was prepared with.
    #[must_use]
    pub fn opts(&self) -> i32 {
        self.opts
    }

    /// Which guest thread it belongs to.
    #[must_use]
    pub fn thread(&self) -> usize {
        self.thread
    }
}

/// Turn one descriptor's readiness into the events a looper reports for it.
///
/// The `ALOOPER_EVENT_*` spelling of `bionic::net`'s `revents_for`, and it makes the same
/// distinction: what was asked for is masked, and what is a condition rather than a request is
/// not.
fn events_for(readiness: omni_platform::fs::Readiness, asked: i32) -> i32 {
    let mut events = 0;
    if readiness.readable {
        events |= asked & ALOOPER_EVENT_INPUT;
    }
    if readiness.writable {
        events |= asked & ALOOPER_EVENT_OUTPUT;
    }
    if readiness.hangup {
        events |= ALOOPER_EVENT_HANGUP;
    }
    if readiness.error {
        events |= ALOOPER_EVENT_ERROR;
    }
    events
}

// ================================================================== the handlers

/// A refusal naming this symbol and its guest address.
fn refuse_inline(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

/// The same, on the exit path.
fn refuse_reentrant(c: &ReentrantCall<'_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

/// Count one call, on either path.
fn count(ndk: &Ndk, symbol: &'static str) {
    *ndk.census.lock().entry(symbol).or_insert(0) += 1;
}

/// `ALooper *ALooper_forThread(void)`
///
/// **Returns null when the calling thread has none, and that is the answer rather than a
/// refusal.** §8.1's fourth failure mode is what the *caller* does with the null — `jni-surface.md`
/// §5.2 decodes `initializeNativeCode` logging `"Unable to retrieve native ALooper"` and returning
/// zero — and a layer that refused here instead would replace a measurable engine behaviour with
/// this layer's opinion. The host's job is to have prepared one; [`Ndk::prepare_looper`] is how,
/// and a gate asserts it before the call rather than reading a zero back afterwards.
fn for_thread(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_forThread");
    let found = ndk.looper_for_current_thread();
    {
        let mut state = ndk.state.lock();
        let thread = Ndk::thread_index(&mut state);
        state.record(
            found.unwrap_or(0),
            thread,
            "forThread",
            match found {
                Some(at) => format!("{at:#x}"),
                None => "NULL -- this thread has no looper".to_string(),
            },
        );
    }
    c.ret().u64(found.unwrap_or(0) as u64);
    Ok(())
}

/// `ALooper *ALooper_prepare(int opts)`
///
/// Creates the calling thread's looper if it has none and returns it. **It does not take a
/// reference for the caller**: AOSP's `Looper::prepare` stores the looper in thread-local storage
/// with the one strong reference that creation makes, and `ALooper_acquire` is what adds a
/// caller's. A `prepare` that incremented would leave the count one too high for ever, which
/// nothing would notice until something released the last one.
fn prepare(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let opts = c.args().next_i32()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_prepare");
    if opts & !ALOOPER_PREPARE_ALLOW_NON_CALLBACKS != 0 {
        return Err(refuse_inline(
            c,
            format!(
                "the guest called `ALooper_prepare({opts:#x})`, and the only option \
                 `android/looper.h` defines is ALOOPER_PREPARE_ALLOW_NON_CALLBACKS \
                 ({ALOOPER_PREPARE_ALLOW_NON_CALLBACKS}). Ignoring an option nobody has defined \
                 would be this layer accepting a request it cannot honour"
            ),
        ));
    }
    let at = ndk.prepare_for("ALooper_prepare", opts)?;
    c.ret().u64(at as u64);
    Ok(())
}

/// The guest `ALooper*` as a checked arena address, or a refusal naming the pointer.
///
/// **Checked rather than trusted.** The arena is divided into a range per handle kind, and
/// `Slots::index_of` refuses a pointer that is inside a range but off a slot boundary — so an
/// `AAsset *` passed where an `ALooper *` belongs, and a `looper + 4` the engine computed, are
/// both refusals rather than lookups that happen to succeed.
fn looper_at(ndk: &Ndk, c: &ImportCall<'_, '_>, looper: u64) -> AbiResult<GuestAddr> {
    let state = ndk.state.lock();
    looper_in(&state, c, looper)
}

/// As [`looper_at`], but against a state whose lock the caller **already holds**.
///
/// # Why this exists, and why the lock must not be released in between
///
/// A liveness check justifies a later `expect` only if nothing could have changed the table in
/// between. `looper_at` takes the lock, checks, and *drops* it before returning; a caller that
/// then re-locks and writes `.expect("the slot was checked live")` is asserting something that
/// stopped being true the moment the guard fell. `ALooper_release` on another guest thread, taking
/// the last reference, frees the slot in exactly that window -- and `ALooper_pollOnce` holds the
/// window open for as long as it sleeps, which makes ordinary shutdown the case that hits it.
///
/// What the panic would cost is the point: a release the guest got wrong is a *guest* defect, and
/// this layer's contract is that such a thing becomes a typed refusal naming the symbol and the
/// guest address, never a host panic unwinding out of an import.
fn looper_in(state: &NdkState, c: &ImportCall<'_, '_>, looper: u64) -> AbiResult<GuestAddr> {
    let at = GuestAddr::try_from(looper).unwrap_or(0);
    if state.loopers.index_of(at).is_none() {
        return Err(refuse_inline(
            c,
            format!(
                "the guest passed {looper:#x} as an `ALooper *`, and this instance's loopers live \
                 in their own range of its arena. An ALooper is opaque, so a pointer this layer \
                 did not hand out is a handle of another kind, a looper from another instance, or \
                 a value the engine computed -- and there is nothing here to operate on in any of \
                 those cases"
            ),
        ));
    }
    if state.loopers.get(at).is_none() {
        return Err(refuse_inline(
            c,
            format!("the guest passed {looper:#x} as an `ALooper *`, and that slot is not live"),
        ));
    }
    Ok(at)
}

/// `void ALooper_acquire(ALooper *looper)`
fn acquire(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let looper = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_acquire");
    // **One lock across the check and the increment.** See [`looper_in`].
    let mut state = ndk.state.lock();
    let at = looper_in(&state, c, looper)?;
    let thread = Ndk::thread_index(&mut state);
    let references = {
        let entry = state.loopers.get_mut(at).expect("checked live under this same lock");
        entry.references += 1;
        entry.references
    };
    state.record(at, thread, "acquire", format!("references now {references}"));
    Ok(())
}

/// `void ALooper_release(ALooper *looper)`
///
/// **A release past the last reference is a refusal**, and it arrives as "that slot is not live"
/// from [`slot_for`] rather than as a negative count.
///
/// That is worth stating because the first version of this function guarded on `references < 0`
/// and **the guard was unreachable**: the count starts at one, the slot is freed the moment it
/// reaches zero, so nothing can observe it below. The test written for the guard is what found
/// that, and the guard was deleted rather than kept as an assertion nothing can reach — a branch
/// no input can take is not a check, it is a comment that looks like one.
///
/// What is refused is the same mistake either way: guest code releasing a reference it does not
/// hold. Saturating instead would keep a looper alive that guest code believes it has destroyed,
/// and the next `pollOnce` on it would answer for descriptors nobody owns.
fn release(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let looper = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_release");
    // **One lock across the check and the decrement.** This is the call that frees the slot, so
    // two of these racing is the case [`looper_in`] describes.
    let mut state = ndk.state.lock();
    let at = looper_in(&state, c, looper)?;
    let thread = Ndk::thread_index(&mut state);
    let references = {
        let entry = state.loopers.get_mut(at).expect("checked live under this same lock");
        entry.references -= 1;
        entry.references
    };
    debug_assert!(references >= 0, "the slot is freed at zero, so a live looper cannot be below it");
    if references == 0 {
        // The last reference, including the one creation made. The thread's own binding goes
        // with it, so a later `ALooper_forThread` on that thread answers NULL -- which is what a
        // device does and is the condition §8.1's fourth failure mode is about.
        state.loopers.remove(at);
        state.by_thread.retain(|_, held| *held != at);
        state.record(at, thread, "release", "destroyed".to_string());
    } else {
        state.record(at, thread, "release", format!("references now {references}"));
    }
    Ok(())
}

/// `int ALooper_addFd(ALooper *looper, int fd, int ident, int events, ALooper_callbackFunc
/// callback, void *data)`
///
/// Returns `1` on success and `-1` on failure, which is the NDK's own contract.
///
/// **A registration with a callback stores `ALOOPER_POLL_CALLBACK` as its ident**, because AOSP
/// does: an entry with a callback can never be reported by ident, so keeping the caller's number
/// would be storing a value that can never be returned. §5.2's constructor passes `ident = 0`
/// *and* a callback, and a layer that kept the zero would report ident 0 from `pollOnce` — a
/// legal-looking answer the glue has no branch for.
fn add_fd(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (looper, fd, ident, events, callback, data) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_i32()?, a.next_i32()?, a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_addFd");
    // **The handle first, so a forged one is reported as a forged one** rather than as
    // whatever the fd below turns out to be. The binding is discarded on purpose: this
    // check orders the diagnosis and nothing may rest on it, because the descriptor work
    // that follows drops this lock. See [`looper_in`].
    looper_at(&ndk, c, looper)?;

    if fd < 0 {
        return Err(refuse_inline(c, format!("`ALooper_addFd` was given fd {fd}")));
    }
    if events & !REQUESTABLE_EVENTS != 0 {
        return Err(refuse_inline(
            c,
            format!(
                "the guest asked `ALooper_addFd` to watch fd {fd} for {events:#x}. Only \
                 ALOOPER_EVENT_INPUT and ALOOPER_EVENT_OUTPUT can be *requested*: ERROR, HANGUP \
                 and INVALID are conditions this layer reports whether or not they were asked \
                 for, exactly as `poll` reports POLLERR and POLLHUP, and accepting them as a \
                 request would say this layer had subscribed to something"
            ),
        ));
    }
    if callback == 0 && ident < 0 {
        return Err(refuse_inline(
            c,
            format!(
                "`ALooper_addFd` was given fd {fd} with no callback and ident {ident}. A \
                 registration with neither can never be reported: `pollOnce` returns an ident \
                 only when it is non-negative, and a negative one collides with the \
                 ALOOPER_POLL_* values"
            ),
        ));
    }

    // **The descriptor must be one this runtime has.** The looper's whole answer about it comes
    // from the filesystem seam, so a descriptor that is not in the table is one this layer would
    // have to invent readiness for.
    {
        let bionic = crate::bionic::active(c.symbol(), c.address())?;
        // An epoll descriptor's readiness is its members' and the seam does not answer it; the
        // poll below would read that refusal as ALOOPER_EVENT_INVALID, so it is refused here.
        // A timerfd is refused for a neighbouring reason: its readiness changes at a deadline
        // that raises nothing, and the looper's wait does not cap itself at one, so a registered
        // timer would be reported late by up to a whole wait.
        if bionic.bionic.filesystem().is_some_and(|fs| {
            fs.readiness_source(fd) == Some(omni_platform::fs::ReadinessSource::Timer)
        }) {
            return Err(refuse_inline(
                c,
                format!(
                    "`ALooper_addFd` was given fd {fd}, a timerfd, and the looper's wait does not \
                     wake at a timer's deadline. No run has reached it"
                ),
            ));
        }
        if bionic.bionic.filesystem().is_some_and(|fs| fs.is_epoll(fd)) {
            return Err(refuse_inline(
                c,
                format!(
                    "`ALooper_addFd` was given fd {fd}, an epoll descriptor. Its readiness is its \
                     members' and is not answered here; Linux allows it and no run has reached it"
                ),
            ));
        }
        let fs = bionic.bionic.filesystem().ok_or_else(|| {
            refuse_inline(
                c,
                "this guest instance has no filesystem root, so it has no descriptor table and a \
                 looper has nothing to watch. `Bionic::set_filesystem_root` is what supplies one"
                    .to_string(),
            )
        })?;
        if !fs.is_open(fd) {
            return Err(refuse_inline(
                c,
                format!(
                    "`ALooper_addFd` was given fd {fd}, which this instance does not hold. A \
                     looper's answer about a descriptor is the filesystem seam's answer, so there \
                     is no readiness to report for one that is not in the table"
                ),
            ));
        }
    }

    let mut state = ndk.state.lock();
    // **Re-checked under the lock that is about to be written through.** The check at the top of
    // this function ordered the diagnosis; it cannot be what this rests on, because the
    // descriptor work in between drops the lock and takes the filesystem's.
    let at = looper_in(&state, c, looper)?;
    let thread = Ndk::thread_index(&mut state);
    let entry = state.loopers.get_mut(at).expect("checked live under this same lock");
    if entry.opts & ALOOPER_PREPARE_ALLOW_NON_CALLBACKS == 0 && callback == 0 {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address: c.address(),
            why: format!(
                "fd {fd} was registered with no callback on a looper prepared without \
                 ALOOPER_PREPARE_ALLOW_NON_CALLBACKS, so `pollOnce` could never report it"
            ),
        });
    }
    // A second registration for the same descriptor **replaces** the first, which is what AOSP's
    // `Looper::addFd` does. Keeping both would report one descriptor twice.
    entry.fds.retain(|held| held.fd != fd);
    if entry.fds.len() >= MAX_LOOPER_FDS {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address: c.address(),
            why: format!(
                "this looper already watches {MAX_LOOPER_FDS} descriptors, which is the cap"
            ),
        });
    }
    let stored = FdRegistration {
        fd,
        ident: if callback == 0 { ident } else { ALOOPER_POLL_CALLBACK },
        events,
        callback: callback as GuestAddr,
        data,
    };
    entry.fds.push(stored);
    state.record(
        at,
        thread,
        "addFd",
        format!("fd {fd} ident {ident} events {events:#x} callback {callback:#x} data {data:#x}"),
    );
    drop(state);
    c.ret().i32(1);
    Ok(())
}

/// `int ALooper_removeFd(ALooper *looper, int fd)`
///
/// `1` if it was removed, `0` if the looper was not watching it, `-1` on error — the NDK's own
/// three answers, and the middle one is not a failure.
fn remove_fd(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (looper, fd) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_removeFd");
    // **One lock across the check and the removal.** See [`looper_in`].
    let mut state = ndk.state.lock();
    let at = looper_in(&state, c, looper)?;
    let thread = Ndk::thread_index(&mut state);
    let entry = state.loopers.get_mut(at).expect("checked live under this same lock");
    let before = entry.fds.len();
    entry.fds.retain(|held| held.fd != fd);
    let removed = before != entry.fds.len();
    state.record(
        at,
        thread,
        "removeFd",
        format!("fd {fd}: {}", if removed { "removed" } else { "was not watched" }),
    );
    drop(state);
    c.ret().i32(i32::from(removed));
    Ok(())
}

/// What one pass over a looper's descriptors found.
enum Pass {
    /// A registration with an ident is ready: report it and stop.
    Ident { ident: i32, fd: i32, events: i32, data: u64 },
    /// Registrations with callbacks are ready: call each, in order.
    Callbacks(Vec<(GuestAddr, i32, i32, u64)>),
    /// Nothing is ready.
    Idle,
}

/// How long one pass of `ALooper_pollOnce`'s wait sleeps before it looks again.
///
/// **This exists for the stop switch and for nothing else.**
/// [`Bionic::stop_guest_threads`](crate::bionic::Bionic::stop_guest_threads) is read between run
/// windows (D16), and a thread asleep inside this handler never ends one -- so the handler has to
/// read it itself, and it can only do that if the sleep is bounded. It costs one gate acquisition
/// and one atomic load per slice on a thread that is otherwise idle, and it bounds teardown
/// latency at this value rather than at the whole remaining timeout.
///
/// The readiness gate is still what actually wakes the wait: a write to the command pipe bumps
/// the generation and returns the slice early, so this number is a *ceiling* on how late a stop
/// is noticed and not a polling interval for the event itself.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// How long `ALooper_pollOnce` may wait.
#[derive(Debug, Clone, Copy)]
enum Bound {
    /// `pollOnce(timeoutMillis >= 0)`: this instant, and then [`ALOOPER_POLL_TIMEOUT`].
    Until(Instant),
    /// `pollOnce(-1)`, allowed because a live wake source was measured. See this module's
    /// documentation for what is checked and why the check is a fact rather than a policy.
    Indefinite,
}

/// The descriptors a looper watches, rendered for a refusal that has to say *why* nothing can
/// wake it.
///
/// Each entry is `fd:kind`, where a pipe's kind carries the count that decided the refusal -- a
/// read end with zero live writers is the whole reason an indefinite wait cannot end, and a
/// message that named only the descriptor numbers would leave the next reader to measure it
/// again.
fn describe_watched(fs: &omni_platform::fs::Filesystem, watched: &[FdRegistration]) -> String {
    watched
        .iter()
        .map(|held| match fs.pipe_writers(held.fd) {
            Some(writers) => format!("{}:pipe-read-end,{writers} live writer(s)", held.fd),
            None if fs.pipe_end(held.fd).is_some() => format!("{}:pipe-write-end", held.fd),
            None => format!("{}:not a pipe", held.fd),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `int ALooper_pollOnce(int timeoutMillis, int *outFd, int *outEvents, void **outData)`
///
/// **On the exit path, because it calls guest code.** A registration with a callback is a guest
/// function pointer, and F9's rule is that anything which calls guest code or reaches
/// `GuestSpace` is `bind_reentrant` — `ImportCall` has no CPU at all, so an inline handler could
/// not do this even if it were safe.
///
/// # The order, which is AOSP's and matters
///
/// A ready registration **with an ident** is reported and the call returns that ident, filling
/// `outFd`/`outEvents`/`outData`. Only if none is does the call invoke the ready **callbacks** and
/// return [`ALOOPER_POLL_CALLBACK`]. The glue depends on it: `android_app_entry` registers its
/// command pipe with `LOOPER_ID_MAIN` and no callback, and `GameLoop` switches on the return.
///
/// A callback returning `0` **removes** its registration, which is the NDK's documented contract
/// and the mechanism by which the glue detaches its pipe.
fn poll_once(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (timeout_millis, out_fd, out_events, out_data) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "ALooper_pollOnce");

    let Some(looper) = ndk.looper_for_current_thread() else {
        // AOSP's `ALooper_pollOnce` on a thread with no looper returns POLL_ERROR. It is an
        // answer the caller has a branch for, and unlike a null from `forThread` it is not a
        // value anything stores, so reporting it is not a plausible stub.
        let mut state = ndk.state.lock();
        let thread = Ndk::thread_index(&mut state);
        state.record(0, thread, "pollOnce", "this thread has no looper: POLL_ERROR".to_string());
        drop(state);
        c.ret(|mut r| r.i32(ALOOPER_POLL_ERROR));
        return Ok(());
    };

    let bionic = crate::bionic::active(c.symbol(), c.address())?;
    let Some(fs) = bionic.bionic.filesystem() else {
        return Err(refuse_reentrant(
            c,
            "this guest instance has no filesystem root, so it has no descriptor table and a \
             looper has nothing to poll"
                .to_string(),
        ));
    };

    let bound = if timeout_millis < 0 {
        // **The premise of the old refusal, tested rather than assumed.** See this module's
        // documentation: an indefinite wait is legitimate exactly when something in this runtime
        // can still make one of the watched descriptors ready, and for a pipe read end that is a
        // write end still open in this instance's descriptor table.
        let watched = ndk.registrations(looper);
        let sources: Vec<(i32, usize)> = watched
            .iter()
            .filter_map(|held| {
                fs.pipe_writers(held.fd)
                    .filter(|writers| *writers > 0)
                    .map(|writers| (held.fd, writers))
            })
            .collect();
        if sources.is_empty() {
            return Err(refuse_reentrant(
                c,
                format!(
                    "the guest called `ALooper_pollOnce({timeout_millis})`, which is an \
                     indefinite wait, and none of the {} descriptor(s) this looper watches can \
                     still be made ready by anything in this runtime: [{}]. D16's runaway-guest \
                     defence is built from step budgets that a sleeping thread does not consume, \
                     so a host thread parked on a descriptor nobody can write to cannot be ended \
                     by anything this runtime has -- the same argument `poll(fds, n, -1)` is \
                     refused under. Returning ALOOPER_POLL_TIMEOUT instead would report a \
                     timeout to a call that was given none. What changes this is a live write \
                     end on a pipe this looper watches, which is how the main thread posts \
                     APP_CMD_INIT_WINDOW (jni-surface.md §8 rows 17-20)",
                    watched.len(),
                    describe_watched(fs, &watched)
                ),
            ));
        }
        {
            let mut state = ndk.state.lock();
            let thread = Ndk::thread_index(&mut state);
            state.record(
                looper,
                thread,
                "pollOnce",
                format!("indefinite: wake sources (fd, live writers) {sources:?}"),
            );
        }
        Bound::Indefinite
    } else {
        let budget = Duration::from_millis(timeout_millis as u64);
        if budget.as_secs() > crate::bionic::MAX_SLEEP_SECONDS {
            return Err(refuse_reentrant(
                c,
                format!(
                    "the guest asked `ALooper_pollOnce` to wait {budget:?}, and this layer caps \
                     a guest-chosen wait at {} seconds -- the same cap `nanosleep`, `poll` and \
                     `select` name. Clamping to the cap was rejected: it would return a timeout \
                     from a call that waited a minute when it was asked to wait longer",
                    crate::bionic::MAX_SLEEP_SECONDS
                ),
            ));
        }
        let Some(deadline) = Instant::now().checked_add(budget) else {
            return Err(refuse_reentrant(
                c,
                format!("a wait of {budget:?} is past this host's clock"),
            ));
        };
        Bound::Until(deadline)
    };

    let pass = loop {
        // Read **before** the descriptors are tested. A write that lands in between raises it, so
        // the wait returns at once rather than sleeping through the event — `VERIFICATION.md`
        // entry 11, measured at 1.0104 s.
        let seen = fs.ready_generation();
        let pass = {
            let state = ndk.state.lock();
            // **Re-read every pass, because this loop sleeps.** The looper was live when
            // `pollOnce` was entered; another guest thread taking the last reference during a
            // sleep frees it, and a looper that goes away under a poll is a refusal naming the
            // call rather than a panic out of the import. See [`looper_in`].
            let Some(entry) = state.loopers.get(looper) else {
                return Err(refuse_reentrant(
                    c,
                    format!(
                        "the looper at {looper:#x} was released while `ALooper_pollOnce` was \
                         waiting on it. A poll whose looper no longer exists has nothing left to \
                         report, and the descriptors it was watching are now owned by nobody"
                    ),
                ));
            };
            let mut callbacks = Vec::new();
            let mut ident = None;
            for held in &entry.fds {
                let events = match fs.readiness(held.fd) {
                    Ok(readiness) => events_for(readiness, held.events),
                    // The descriptor was closed out from under the looper. `ALOOPER_EVENT_INVALID`
                    // is what that condition is called, and it is reported rather than hidden.
                    Err(_) => ALOOPER_EVENT_INVALID,
                };
                if events == 0 {
                    continue;
                }
                if held.callback == 0 {
                    if ident.is_none() {
                        ident = Some(Pass::Ident {
                            ident: held.ident,
                            fd: held.fd,
                            events,
                            data: held.data,
                        });
                    }
                } else {
                    callbacks.push((held.callback, held.fd, events, held.data));
                }
            }
            // Idents first, as AOSP does: the glue's command pipe is registered with
            // `LOOPER_ID_MAIN` and no callback, and `GameLoop` switches on the return.
            match ident {
                Some(found) => found,
                None if callbacks.is_empty() => Pass::Idle,
                None => Pass::Callbacks(callbacks),
            }
        };
        if !matches!(pass, Pass::Idle) {
            break pass;
        }
        let slice = match bound {
            Bound::Indefinite => WAIT_SLICE,
            Bound::Until(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    break Pass::Idle;
                }
                (deadline - now).min(WAIT_SLICE)
            }
        };
        // **The stop switch, read every slice — and only once this call is going to sleep.**
        // `Bionic::stop_guest_threads` is read between run windows, and a thread asleep in this
        // wait never ends one, so without this an indefinite poll is exactly the unstoppable park
        // D16 objects to and a 60-second bounded one holds teardown for up to a minute.
        //
        // **Below the deadline test, not above it**, because `pollOnce(0)` is a *poll* and not a
        // wait: it asks what is ready now and POLL_TIMEOUT is the true answer when nothing is.
        // MEASURED with the two the other way round: teardown turned every `pollOnce(0)` on the
        // game thread into a refusal, reporting a wait that had been interrupted where no wait
        // had been asked for.
        //
        // Refused rather than turned into POLL_TIMEOUT for a call that *was* waiting: the wait
        // did not expire, the runtime ended it, and a timeout would say otherwise.
        if bionic.bionic.guest_threads_stopping() {
            return Err(refuse_reentrant(
                c,
                format!(
                    "`ALooper_pollOnce({timeout_millis})` was waiting on looper {looper:#x} \n                     when this runtime asked its guest threads to stop. The wait did not \n                     expire and nothing became ready, so there is no value to return that \n                     would be true: ALOOPER_POLL_TIMEOUT would report a timeout that did \n                     not happen"
                ),
            ));
        }
        fs.wait_for_readiness(seen, slice);
    };

    let returned = match pass {
        Pass::Idle => {
            // **A zero-timeout poll that found nothing is not recorded.**
            //
            // It is the game loop's idle tick and it carries no information: `GameLoop` calls
            // `pollOnce(0)` once per frame and again whenever it has nothing to do, so recording
            // it means a `format!`, a `String` allocation and the state lock on the hottest path
            // in the runtime. MEASURED once the engine reached its loop: **242,333,924 calls in
            // one run**, every one of them formatting a line into a ring that had already dropped
            // it. The event log existed to explain a looper that was not progressing; a log that
            // is itself what stops it progressing explains nothing.
            //
            // A poll that was *given* a timeout and expired is still recorded: that one says a
            // wait happened and ended, which is the shape §8.1's fifth failure mode asks about.
            // The census counts every call either way, so nothing is lost that was being counted.
            if timeout_millis != 0 {
                let mut state = ndk.state.lock();
                let thread = Ndk::thread_index(&mut state);
                state.record(
                    looper,
                    thread,
                    "pollOnce",
                    format!("{timeout_millis} ms: nothing ready, POLL_TIMEOUT"),
                );
            }
            ALOOPER_POLL_TIMEOUT
        }
        Pass::Ident { ident, fd, events, data } => {
            // **All three out-parameters or none.** A caller that read `outFd` after a failed
            // `outEvents` write would act on a descriptor whose events it never learned; the same
            // all-or-nothing shape `poll` answers its array with.
            let mem = c.mem();
            if out_fd != 0 {
                mem.write_u32(
                    GuestAddr::try_from(out_fd).map_err(|_| {
                        refuse_reentrant(c, format!("`outFd` {out_fd:#x} is wider than a pointer"))
                    })?,
                    fd as u32,
                    c.blame(1),
                )?;
            }
            if out_events != 0 {
                mem.write_u32(
                    GuestAddr::try_from(out_events).map_err(|_| {
                        refuse_reentrant(
                            c,
                            format!("`outEvents` {out_events:#x} is wider than a pointer"),
                        )
                    })?,
                    events as u32,
                    c.blame(2),
                )?;
            }
            if out_data != 0 {
                mem.write_u64(
                    GuestAddr::try_from(out_data).map_err(|_| {
                        refuse_reentrant(
                            c,
                            format!("`outData` {out_data:#x} is wider than a pointer"),
                        )
                    })?,
                    data,
                    c.blame(3),
                )?;
            }
            let mut state = ndk.state.lock();
            let thread = Ndk::thread_index(&mut state);
            state.record(
                looper,
                thread,
                "pollOnce",
                format!("fd {fd} ready with {events:#x}, reporting ident {ident}"),
            );
            ident
        }
        Pass::Callbacks(ready) => {
            for (callback, fd, events, data) in ready {
                let returned = c
                    .call_guest(
                        callback,
                        // `int (*)(int fd, int events, void *data)`. The two `int`s are
                        // **sign-extended into the X register**, which `i64 as u64` does and a
                        // bare `as u64` on an `i32` would not: `ALOOPER_EVENT_*` are all
                        // positive, but a caller that compared `events` against a negative
                        // constant would see a number four billion away from the one meant.
                        &[
                            GuestArg::Int(i64::from(fd) as u64),
                            GuestArg::Int(i64::from(events) as u64),
                            GuestArg::Pointer(GuestAddr::try_from(data).map_err(|_| {
                                refuse_reentrant(
                                    c,
                                    format!(
                                        "the callback's `data` {data:#x} is wider than a pointer"
                                    ),
                                )
                            })?),
                        ],
                        RunLimit::Unlimited,
                    )?
                    .as_i32();
                let mut state = ndk.state.lock();
                let thread = Ndk::thread_index(&mut state);
                state.record(
                    looper,
                    thread,
                    "callback",
                    format!("{callback:#x}(fd {fd}, events {events:#x}) -> {returned}"),
                );
                if returned == 0 {
                    // The NDK's documented contract: a callback returning 0 asks to be removed.
                    //
                    // **`if let`, not `expect`.** The callback that just returned is *guest code*,
                    // run with no lock held, and `ALooper_release` is one of the things it may
                    // have called. A looper it destroyed has no registration left to remove, which
                    // is the request already satisfied -- not a reason to panic. See [`looper_in`].
                    if let Some(entry) = state.loopers.get_mut(looper) {
                        entry.fds.retain(|held| held.fd != fd);
                    }
                }
            }
            ALOOPER_POLL_CALLBACK
        }
    };
    c.ret(|mut r| r.i32(returned));
    Ok(())
}

/// Every symbol serviced **inside** the run loop: no guest code, no address-space change.
pub(super) static INLINE: &[(&str, ImportFn)] = &[
    ("ALooper_forThread", for_thread),
    ("ALooper_prepare", prepare),
    ("ALooper_acquire", acquire),
    ("ALooper_release", release),
    ("ALooper_addFd", add_fd),
    ("ALooper_removeFd", remove_fd),
];

/// Every symbol serviced on the **exit** path, because it calls guest code (F9).
pub(super) static REENTRANT: &[(&str, ReentrantFn)] = &[("ALooper_pollOnce", poll_once)];
