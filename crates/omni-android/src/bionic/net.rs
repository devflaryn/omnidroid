//! The eight network symbols: two answered from `omni-bionic`, two implemented here, four refused
//! by name.
//!
//! `socket`, `poll`, `select`, `eventfd`, `getaddrinfo`, `freeaddrinfo`, `gai_strerror`,
//! `inet_ntop`.
//!
//! # The prediction this group was dispatched with, and what it actually needed
//!
//! The plan's phase-3 table lists "**Sockets and polling** — socket, poll/select, getaddrinfo"
//! among the things `omni-platform` must grow for. **It did not have to**, and that is now the
//! third phase running whose five-target prediction over-estimated the OS surface (files: fifteen
//! of seventeen primitives were one portable `std` call, D23; threads: none at all, D24).
//!
//! The sharper test D23 proposed is *is there one `std` call that serves all five targets?*, and
//! for this group the answer is a third thing: **there is no OS call to make at all.**
//!
//! | symbol | where its answer comes from |
//! |---|---|
//! | `inet_ntop` | [`omni_bionic::net`] — formatting, no state |
//! | `gai_strerror` | [`omni_bionic::net`] — a constant table, interned in the instance's pool |
//! | `poll`, `select` | **here**, over `omni-platform`'s existing descriptor table. No OS call |
//! | `socket`, `eventfd`, `getaddrinfo`, `freeaddrinfo` | **refused by name** |
//!
//! So no socket seam was added to `omni-platform`, and therefore no `unsupported` arm was
//! fabricated for Linux or macOS either — which is D22's other half: a primitive that calls no OS
//! API must not be given one, because that is a false claim in the other direction.
//!
//! # Why `poll` and `select` need no operating system — the argument, and the day it changed
//!
//! Until M5 the argument was that **the descriptor space they observe is entirely this runtime's
//! own, and none of the kinds in it can block**: every descriptor was a regular file, a directory
//! or one of the three standard streams, `socket` and `eventfd` refused by name, and `pipe` was
//! not among the 188 at all. So `poll` reported every open descriptor as ready, which is also what
//! Linux does for a regular file — its `DEFAULT_POLLMASK` is exactly
//! `POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM`, regardless of the access mode the descriptor was
//! opened with, which is why a read-only file still answers `POLLOUT` there and here.
//!
//! `the_descriptor_space_poll_answers_over_is_closed` asserted that mechanically, and D25 wrote
//! down what it was for: *the day a phase binds `socket` for real, that test fails and this module
//! has to grow a real readiness source with it.* **M5 is that day, and the symbol was `pipe`
//! rather than `socket`** — §8 row 13a needs two of them before `initializeNativeCode` can return.
//!
//! What replaced the argument is not a weaker version of it. `omni-platform`'s
//! [`Filesystem::readiness`] is a `match` over the descriptor kinds with **no default arm**: a
//! file, a directory, a device and a standard stream answer [`Readiness::ALWAYS`] for the reason
//! above, and a pipe answers from its own queue and reference counts. So the space is still
//! closed — closed under *kinds that have decided what they answer* rather than under *kinds that
//! cannot block* — and a sixth kind cannot be added without deciding.
//!
//! Still no operating system. A pipe here is an in-process byte queue; there is no OS call on
//! either side of it.
//!
//! The consistency criterion is unchanged and is the one that matters: **`poll`'s answer predicts
//! what `read` and `write` on that descriptor will actually do in this runtime**, not what they
//! would do on a device.
//!
//! [`Filesystem::readiness`]: omni_platform::fs::Filesystem::readiness
//! [`Readiness::ALWAYS`]: omni_platform::fs::Readiness::ALWAYS
//!
//! # What they do when nothing is ready, and the one thing they refuse
//!
//! An *infinite* wait is a permanent hang of a host thread whenever nothing arrives, and D16's
//! runaway-guest defence is built from step budgets that a sleeping thread does not consume. So
//! `poll(fds, n, -1)` and `select(.., NULL)` with nothing ready are **refused by name**, with the
//! same argument [`MAX_SLEEP_SECONDS`](super::MAX_SLEEP_SECONDS) makes for `nanosleep` — and a
//! finite timeout past that cap is refused rather than clamped, because a clamp returns `0` from a
//! call that waited a minute when it was asked to wait a year.
//!
//! **That refusal survived a pipe existing, and its reason changed.** It used to rest on "none of
//! the descriptors it named can ever become ready", which a pipe makes false. What is left is the
//! step-budget argument alone, which is the half that was load-bearing: a host thread parked on a
//! pipe nobody writes to is exactly as unrecoverable as one parked on a regular file.
//!
//! A finite timeout is a real wait on `omni-platform`'s readiness gate, re-testing the
//! descriptors each time it rises, and a real `0` when it expires — which is what `poll` promises.
//! **The generation is read before the descriptors are tested**, so a write landing between the
//! test and the wait raises it and the wait returns at once. Reading it afterwards is the lost
//! wakeup this project has already measured once, at 1.0104 s (`sem_post`, `VERIFICATION.md`
//! entry 11).
//!
//! # The `-1`/`errno` versus refusal split, as the rest of the adapter draws it
//!
//! `EINVAL` for an `nfds` past the cap and for a malformed `struct timeval`; `EBADF` from
//! `select` for a descriptor that is not open. Those are Linux's own answers and guest code has a
//! branch for each. `poll` reports the same bad descriptor as `POLLNVAL` in that entry's
//! `revents` rather than as `-1`, because that is what `poll` does and the two calls genuinely
//! differ here.
//!
//! **And on any of those failures the guest's own objects are left alone.** POSIX says a failed
//! `select` does not modify the sets, and `poll` answers its whole array or none of it. Both are
//! the direction review finding M1 says to err in, and `select`'s ordering — validate the
//! timeout, then rewrite the sets — was wrong in the first version of this module.

use std::time::{Duration, Instant};

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_bionic::net;
use omni_mem::GuestAddr;
use omni_platform::fs::Readiness;

use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::files::{filesystem, settle, Settled};
use super::view::GuestView;
use super::{active, enter, Active, MAX_SLEEP_SECONDS};

// ================================================================== the guest's constants
//
// Linux UAPI (`asm-generic/poll.h`, `linux/posix_types.h`), which is what `libroblox.so` was
// compiled against, and the same provenance as the `O_*` flags in `files` and the `clockid_t`
// numbers in `clocks`.

/// `POLLIN`: there is data to read.
const POLLIN: i16 = 0x001;
/// `POLLPRI`: there is urgent data to read. Nothing here can ever produce it.
const POLLPRI: i16 = 0x002;
/// `POLLOUT`: writing will not block.
const POLLOUT: i16 = 0x004;
/// `POLLERR`: an error condition. Output only.
const POLLERR: i16 = 0x008;
/// `POLLHUP`: hung up. Output only.
const POLLHUP: i16 = 0x010;
/// `POLLNVAL`: the descriptor is not open. Output only, and reported whether or not it was asked
/// for — which is why it is not in [`READY_MASK`].
const POLLNVAL: i16 = 0x020;
/// `POLLRDNORM`: normal data may be read. The same condition as `POLLIN` for everything here.
const POLLRDNORM: i16 = 0x040;
/// `POLLWRNORM`: normal data may be written.
const POLLWRNORM: i16 = 0x100;

/// What a **readable** descriptor answers, masked by what was asked for.
const READABLE_MASK: i16 = POLLIN | POLLRDNORM;
/// What a **writable** descriptor answers, masked by what was asked for.
const WRITABLE_MASK: i16 = POLLOUT | POLLWRNORM;

/// What an always-ready descriptor answers, masked by what was asked for.
///
/// Linux's `DEFAULT_POLLMASK`, which is what its `poll` returns for a regular file. `POLLPRI` is
/// deliberately absent: out-of-band data is a socket concept and nothing here has any.
///
/// **Since M5 it is the union of the two halves rather than the thing handlers use**, because a
/// pipe is readable or writable and rarely both. It is still the answer for every descriptor kind
/// that cannot block, and the compile-time assertion below is still about it.
const READY_MASK: i16 = READABLE_MASK | WRITABLE_MASK;

/// The ready mask contains no condition this runtime cannot produce, and none that `poll` reports
/// whether or not it was asked for.
///
/// A **compile-time** assertion rather than a test, because both sides are constants — the same
/// reasoning the arena's `MAX_GUEST_FILES` check in [`super`] gives. It is also what keeps
/// `POLLPRI`, `POLLERR` and `POLLHUP` present and pinned although no code path produces one:
/// each names a condition that has no source here (out-of-band data, a device error, a hang-up),
/// and the value of carrying them is exactly this statement that they are *not* answered.
const _: () = assert!(READY_MASK & (POLLPRI | POLLERR | POLLHUP | POLLNVAL) == 0);

/// Bytes of a guest `struct pollfd`: `int fd; short events; short revents;`.
const POLLFD_BYTES: usize = 8;

/// The most `struct pollfd` entries one `poll` call may name.
///
/// **A policy number, and stated as one.** Linux bounds `nfds` by the process's `RLIMIT_NOFILE`
/// and answers `EINVAL` past it; 1,024 is the usual soft limit and is also `FD_SETSIZE`, so it is
/// the number a guest is most likely to have been written against. It matters here because `nfds`
/// is a `nfds_t` — an unsigned 64-bit value the guest chose — and the array it describes is read
/// out of guest memory: without a cap, `poll(p, SIZE_MAX, 0)` asks this layer to read 147
/// exabytes.
///
/// There are at most [`omni_platform::fs::MAX_OPEN_FILES`] descriptors in existence, so a cap of
/// 1,024 cannot refuse a call that names every one of them several times over.
pub const MAX_POLL_FDS: u64 = 1024;

/// `FD_SETSIZE`: how many descriptors a guest `fd_set` can hold.
///
/// 1,024 on bionic as on glibc, and the size of the object is `FD_SETSIZE / 8` = 128 bytes. An
/// `nfds` past it would have `select` read bits out of whatever the guest put after its `fd_set`.
pub const FD_SETSIZE: i32 = 1024;

/// Bits in one word of an `fd_set`. Bionic's `fd_set` is `unsigned long fds_bits[]` on LP64.
const FD_BITS_PER_WORD: i32 = 64;

/// Bytes of a guest `struct timeval`: two `long`-sized fields on LP64.
const TIMEVAL_BYTES: usize = 16;

/// Microseconds in a second.
const MICROS_PER_SECOND: i64 = 1_000_000;

fn refuse(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

/// Narrow a guest pointer to a host address, refusing rather than truncating.
fn guest_address(view: &GuestView<'_>, pointer: u64) -> AbiResult<GuestAddr> {
    GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))
}

// ================================================================== the two from `omni-bionic`

/// `const char *inet_ntop(int af, const void *src, char *dst, socklen_t size)`
///
/// `socklen_t` is an **unsigned 32-bit** value, so it is taken from `W3` rather than `X3`: the
/// high half of `X3` is unspecified by AAPCS64 for a 32-bit parameter, and reading it as a `u64`
/// would turn a `size` of 16 into a number with whatever the guest last left in the top half.
pub(super) fn inet_ntop(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (af, src, dst, size) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_i32()? as u32)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        match net::inet_ntop(&mut view, af, src, dst, size) {
            Ok(Ok(at)) => at,
            Ok(Err(errno)) => {
                view.set_errno(errno);
                // NULL, which is what the guest tests for.
                0
            }
            Err(fault) => return Err(view.fault(fault)),
        }
    };
    c.ret().u64(result);
    Ok(())
}

/// `const char *gai_strerror(int ecode)`
///
/// Returns a pointer into the **instance's pool**, interned once in `Bionic::new`, because C says
/// the returned string is valid indefinitely. Using the per-thread scratch `strerror` uses would
/// have been the believable wrong answer here: the next `strerror` on that thread would overwrite
/// a message the guest had stored a pointer to, and nothing about either call would say so.
pub(super) fn gai_strerror(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let ecode = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    let at = state.bionic.gai_message(ecode);
    c.ret().u64(at as u64);
    Ok(())
}

// ================================================================== polling

// **The wait used to be carried out after the guest view was dropped, and no longer is.**
//
// Before a pipe existed, a `poll` with nothing ready could only sleep, so the handler computed a
// duration while the view was alive and slept after dropping it. A wait that re-tests the
// descriptors has to hold — or re-enter — the view for each test, which both `poll` and `select`
// now do. What survives is the constraint that produced the old shape: [`ImportCall::args`]
// borrows the call shared and [`ImportCall::ret`] borrows it uniquely, so the return value is
// still written once, after every view is gone.

/// `int poll(struct pollfd *fds, nfds_t nfds, int timeout)`
pub(super) fn poll(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fds, nfds, timeout) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let first = {
        let mut view = enter(c, &state);
        if nfds > MAX_POLL_FDS {
            // Linux's own answer for an `nfds` past the process's descriptor limit.
            view.set_errno(consts::EINVAL);
            Some(-1)
        } else {
            // `nfds` is bounded above, so this cannot overflow.
            let bytes = nfds as usize * POLLFD_BYTES;
            let ready = if bytes == 0 {
                // A zero-length array is legal and `fds` may be anything, null included — POSIX
                // says so, and it is the idiom for "sleep for `timeout` milliseconds". Nothing is
                // read, and in particular the descriptor table is not consulted, so a `poll` used
                // as a sleep works on an instance that has no filesystem.
                0
            } else {
                poll_entries(&view, fds, bytes)?
            };
            if ready > 0 {
                Some(ready)
            } else {
                None
            }
        }
    };
    let value = match first {
        Some(value) => value,
        None => {
            // Nothing is ready yet. The wait is bounded here, before any of it happens, so a
            // refusal arrives instead of a sleep rather than after one.
            let duration =
                if timeout < 0 { None } else { Some(Duration::from_millis(timeout as u64)) };
            let budget = bounded_wait(c, duration)?;
            let bytes = nfds as usize * POLLFD_BYTES;
            wait_until_ready(c, &state, budget, |view| {
                if bytes == 0 {
                    Ok(0)
                } else {
                    poll_entries(view, fds, bytes)
                }
            })?
        }
    };
    c.ret().i32(value);
    Ok(())
}

/// Re-test readiness every time `omni-platform`'s gate rises, until something is ready or the
/// budget runs out.
///
/// `test` is whatever the caller counts as ready — the `pollfd` array for `poll`, the three
/// `fd_set`s for `select` — and it writes the guest's own objects back each time it runs, because
/// the last run is the one the guest sees and the caller cannot know in advance which that is.
///
/// **The generation is read before `test` runs.** A write that lands between the test and the wait
/// raises it, so the wait returns immediately rather than sleeping through the event. The other
/// order is the lost wakeup `VERIFICATION.md` entry 11 measured at 1.0104 s.
///
/// An instance with **no filesystem** cannot have a pipe, so nothing can ever raise the gate and
/// the wait degenerates to the sleep this function replaced. That is a real branch, not a
/// fallback: `poll(NULL, 0, 50)` as a sleep is legal on an instance that has no filesystem root.
fn wait_until_ready(
    c: &ImportCall<'_, '_>,
    state: &Active,
    budget: Duration,
    mut test: impl FnMut(&GuestView<'_>) -> AbiResult<i32>,
) -> AbiResult<i32> {
    let Some(deadline) = Instant::now().checked_add(budget) else {
        // `bounded_wait` caps the budget well below anything that could do this, so this is a
        // refusal for something that cannot happen rather than a clamp that hides it.
        return Err(refuse(c, format!("a wait of {budget:?} is past this host's clock")));
    };
    let Some(fs) = state.bionic.filesystem() else {
        omni_platform::clock::sleep(budget);
        return Ok(0);
    };
    loop {
        let seen = fs.ready_generation();
        let ready = {
            let view = enter(c, state);
            test(&view)?
        };
        if ready > 0 {
            return Ok(ready);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(0);
        }
        fs.wait_for_readiness(seen, deadline - now);
    }
}

/// Read the guest's `pollfd` array, answer every entry, write it back, and count the ready ones.
///
/// **Read whole, decide, write whole.** One access in each direction rather than one per entry,
/// so an array that is only partly mapped leaves the guest's `revents` untouched rather than half
/// updated — the same all-or-nothing shape `clocks::write_pair` and `files::write_struct` use,
/// and the direction review finding M1 says to err in.
fn poll_entries(view: &GuestView<'_>, fds: u64, bytes: usize) -> AbiResult<i32> {
    let at = guest_address(view, fds)?;
    let blame = Blame::new(view.symbol(), view.address(), 0);
    let mut entries = view.mem().read_bytes(at, bytes, blame)?;
    // The descriptor table is consulted only if some entry actually names a descriptor, so an
    // instance with no filesystem still answers a `poll` over an array of ignored entries.
    let names_a_descriptor = entries
        .chunks_exact(POLLFD_BYTES)
        .any(|entry| i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]) >= 0);
    let fs = if names_a_descriptor { Some(filesystem(view)?) } else { None };
    let mut ready = 0i32;
    for entry in entries.chunks_exact_mut(POLLFD_BYTES) {
        let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
        let events = i16::from_le_bytes([entry[4], entry[5]]);
        let revents = if fd < 0 {
            // POSIX: a negative descriptor is ignored and its `revents` is zeroed. It is the
            // idiom for a slot a program has stopped using, so answering `POLLNVAL` for it would
            // make every such program see an error it has no cause for.
            0
        } else {
            match fs.map(|fs| fs.readiness(fd)) {
                Some(Ok(readiness)) => revents_for(readiness, events),
                // Reported whether or not it was requested, which is what `POLLNVAL` is for. A
                // seam failure that is not `EBADF` cannot reach here: `readiness` answers from
                // the table alone and makes no host call.
                _ => POLLNVAL,
            }
        };
        entry[6..8].copy_from_slice(&revents.to_le_bytes());
        if revents != 0 {
            ready += 1;
        }
    }
    view.mem().write_bytes(at, &entries, blame)?;
    Ok(ready)
}

/// Turn one descriptor's readiness into the `revents` bits for the `events` that were asked for.
///
/// **`POLLERR` and `POLLHUP` are reported whether or not they were requested**, which is POSIX's
/// own rule and is why they are not in [`READY_MASK`]. A guest that polled only for `POLLIN` on a
/// pipe whose writers have all gone still learns that they have — without it, the canonical drain
/// loop never sees end of file and spins.
fn revents_for(readiness: Readiness, events: i16) -> i16 {
    let mut revents = 0i16;
    if readiness.readable {
        revents |= events & READABLE_MASK;
    }
    if readiness.writable {
        revents |= events & WRITABLE_MASK;
    }
    if readiness.hangup {
        revents |= POLLHUP;
    }
    if readiness.error {
        revents |= POLLERR;
    }
    revents
}

/// `int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds, struct timeval *timeout)`
pub(super) fn select(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (nfds, readfds, writefds, exceptfds, timeout) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let value = {
        let mut view = enter(c, &state);
        select_outcome(c, &mut view, nfds, [readfds, writefds, exceptfds], timeout)?
    };
    c.ret().i32(value);
    Ok(())
}

/// The whole of `select`'s decision, with the guest view alive.
fn select_outcome(
    c: &ImportCall<'_, '_>,
    view: &mut GuestView<'_>,
    nfds: i32,
    pointers: [u64; 3],
    timeout: u64,
) -> AbiResult<i32> {
    if !(0..=FD_SETSIZE).contains(&nfds) {
        // Negative is `EINVAL` on Linux. Past `FD_SETSIZE` is `EINVAL` here, and this is the one
        // place the call is stricter than Linux, which clamps to the process's descriptor table
        // instead: a guest `fd_set` is 128 bytes, so honouring a larger `nfds` would read bits
        // out of whatever the guest put after it. Refusing to read past the object is a
        // rejection of the request rather than a truncation of it — the whole call fails and the
        // guest is told, which is the opposite of review finding M5's shape.
        view.set_errno(consts::EINVAL);
        return Ok(-1);
    }
    // Whole words, as the kernel's own `FDS_BYTES` computes them: an `fd_set` is an array of
    // 64-bit words, so an `nfds` of 65 covers two of them. `nfds` is bounded by `FD_SETSIZE`, so
    // this arithmetic cannot overflow.
    let words = ((nfds + FD_BITS_PER_WORD - 1) / FD_BITS_PER_WORD) as usize;
    let bytes = words * 8;

    let mut sets = [
        Set::read(view, pointers[0], bytes, 1)?,
        Set::read(view, pointers[1], bytes, 2)?,
        Set::read(view, pointers[2], bytes, 3)?,
    ];
    // **Every descriptor named in any set is checked before any set is answered.** `select`
    // reports `EBADF` for the *call*, not for one bit, so a bad descriptor in the third set must
    // not leave the first two already rewritten.
    let named: Vec<i32> = sets.iter().flat_map(|set| set.members(nfds)).collect();
    if !named.is_empty() {
        let fs = filesystem(view)?;
        if named.iter().any(|fd| !fs.is_open(*fd)) {
            view.set_errno(consts::EBADF);
            return Ok(-1);
        }
    }
    // **What the guest asked about, kept**, because a wait re-tests the same question and the
    // sets are about to be overwritten with the answer.
    let asked: [Set; 3] = [sets[0].copy(), sets[1].copy(), sets[2].copy()];
    answer_sets(view, &asked, &mut sets, nfds);
    let ready = ready_bits(&sets, nfds);
    if ready > 0 {
        for set in &sets {
            set.write_back(view)?;
        }
        return Ok(ready);
    }
    // **The timeout is read and validated BEFORE any set is modified**, and that ordering is the
    // contract rather than tidiness: POSIX says that on failure "the objects pointed to by the
    // readfds, writefds, and errorfds arguments are not modified". Zeroing them and then
    // answering `-1`/`EINVAL` for a malformed `timeval` would leave a guest that retried the call
    // with sets it had already lost — the shape of review findings M1 and M5, one call along.
    //
    // A first version of this function did exactly that, and it was found by re-reading the code
    // with the question "what does a guest-chosen number do here" rather than by a failing test.
    let duration = if timeout == 0 {
        // A null `timeout` is "wait indefinitely".
        None
    } else {
        let at = guest_address(view, timeout)?;
        let raw =
            view.mem().read_bytes(at, TIMEVAL_BYTES, Blame::new(view.symbol(), view.address(), 4))?;
        let seconds = i64::from_le_bytes(raw[..8].try_into().expect("eight bytes"));
        let micros = i64::from_le_bytes(raw[8..].try_into().expect("eight bytes"));
        if !(0..MICROS_PER_SECOND).contains(&micros) || seconds < 0 {
            // Bionic converts the `timeval` to a `timespec` before the syscall and reports a
            // `tv_usec` outside `[0, 1e6)` as `EINVAL` itself; a negative `tv_sec` is the
            // kernel's own `EINVAL`.
            view.set_errno(consts::EINVAL);
            return Ok(-1);
        }
        // Neither field can overflow the sum: `tv_usec` is bounded by a million and `tv_sec` by
        // the cap `bounded_wait` applies next.
        Some(Duration::from_secs(seconds as u64) + Duration::from_micros(micros as u64))
    };
    // Refused before anything is written, for the same reason.
    let wait = bounded_wait(c, duration)?;
    // **The wait re-asks the question every time the readiness gate rises**, against `asked`
    // rather than against the sets in guest memory, which are about to be overwritten. Before a
    // pipe existed this was a plain sleep, because nothing could change during it.
    let Some(deadline) = Instant::now().checked_add(wait) else {
        return Err(refuse(c, format!("a wait of {wait:?} is past this host's clock")));
    };
    loop {
        // Read before the descriptors are tested. The other order loses a wakeup that lands in
        // between — `VERIFICATION.md` entry 11, measured at 1.0104 s.
        let seen = view.active.bionic.filesystem().map(omni_platform::fs::Filesystem::ready_generation);
        answer_sets(view, &asked, &mut sets, nfds);
        let ready = ready_bits(&sets, nfds);
        if ready > 0 {
            for set in &sets {
                set.write_back(view)?;
            }
            return Ok(ready);
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match (seen, view.active.bionic.filesystem()) {
            (Some(seen), Some(fs)) => {
                fs.wait_for_readiness(seen, deadline - now);
            }
            // No filesystem means no pipe means nothing can change, so the wait is the sleep it
            // always was. A `select` used purely as a sleep does not need a filesystem root.
            _ => {
                omni_platform::clock::sleep(deadline - now);
                break;
            }
        }
    }
    // The wait expired, so on return every set must be empty: POSIX requires the sets to be
    // zeroed when `select` times out, and a guest that read a stale bit would act on a descriptor
    // this call did not report.
    for set in &mut sets {
        set.clear();
    }
    for set in &sets {
        set.write_back(view)?;
    }
    Ok(0)
}

/// Answer each set from `asked`: keep the descriptors that are ready for what that set asks, and
/// empty `exceptfds`.
///
/// A file, a directory, a device and a standard stream are ready for both, which is Linux's answer
/// for them; a pipe is ready for one, the other or neither.
///
/// **`exceptfds` is emptied.** Linux sets it for out-of-band socket data and for a few `ioctl`
/// conditions on character devices, and this runtime produces neither. A pipe with no readers
/// reports `POLLERR` to `poll`, and `select`'s `exceptfds` is deliberately **not** where Linux
/// reports that either.
/// What `select` returns: **the number of bits set across all the masks**, not the number of
/// distinct descriptors.
///
/// POSIX is explicit, and the difference is invisible until one descriptor is ready in two sets:
/// counting descriptors then returns one where the call must return two — a smaller and entirely
/// reasonable-looking number. A function rather than an expression because both the first
/// evaluation and every pass of the wait need it, and a second copy is a second thing to get
/// wrong.
fn ready_bits(sets: &[Set; 3], nfds: i32) -> i32 {
    sets[0].count(nfds) + sets[1].count(nfds)
}

fn answer_sets(view: &GuestView<'_>, asked: &[Set; 3], sets: &mut [Set; 3], nfds: i32) {
    let readiness = |fd: i32| {
        view.active.bionic.filesystem().and_then(|fs| fs.readiness(fd).ok())
    };
    sets[0].restore(&asked[0]);
    sets[1].restore(&asked[1]);
    sets[0].retain(nfds, |fd| readiness(fd).is_some_and(|r| r.readable));
    sets[1].retain(nfds, |fd| readiness(fd).is_some_and(|r| r.writable));
    sets[2].clear();
}

/// One guest `fd_set`, read out of guest memory and written back to the same place.
///
/// A null pointer is a set with no members, which is what passing `NULL` for a `select` set
/// means; it is kept as a distinct state rather than as an empty buffer so that nothing is
/// written back to address zero.
struct Set {
    at: Option<GuestAddr>,
    bits: Vec<u8>,
    argument: usize,
}

impl Set {
    fn read(view: &GuestView<'_>, pointer: u64, bytes: usize, argument: usize) -> AbiResult<Self> {
        if pointer == 0 || bytes == 0 {
            return Ok(Self { at: None, bits: Vec::new(), argument });
        }
        let at = guest_address(view, pointer)?;
        let bits =
            view.mem().read_bytes(at, bytes, Blame::new(view.symbol(), view.address(), argument))?;
        Ok(Self { at: Some(at), bits, argument })
    }

    /// Every descriptor this set names, below `nfds`.
    fn members(&self, nfds: i32) -> impl Iterator<Item = i32> + '_ {
        (0..nfds).filter(move |fd| self.contains(*fd))
    }

    fn contains(&self, fd: i32) -> bool {
        let byte = (fd / 8) as usize;
        self.bits.get(byte).is_some_and(|bits| bits & (1 << (fd % 8)) != 0)
    }

    fn count(&self, nfds: i32) -> i32 {
        // Bounded by `nfds`, which is bounded by `FD_SETSIZE`, so this cannot overflow.
        self.members(nfds).count() as i32
    }

    fn clear(&mut self) {
        self.bits.fill(0);
    }

    /// A copy of the bits, kept so a wait can re-ask the same question after the set in guest
    /// memory has been overwritten with an answer.
    fn copy(&self) -> Set {
        Set { at: self.at, bits: self.bits.clone(), argument: self.argument }
    }

    /// Put `other`'s bits back, so the next answer starts from what the guest asked.
    fn restore(&mut self, other: &Set) {
        self.bits.copy_from_slice(&other.bits);
    }

    /// Keep only the members `keep` accepts, clearing every other bit.
    ///
    /// Bits at or above `nfds` are cleared too: POSIX says `select` examines only the first
    /// `nfds` descriptors, and a bit the call did not examine must not be reported as ready.
    fn retain(&mut self, nfds: i32, mut keep: impl FnMut(i32) -> bool) {
        let members: Vec<i32> = self.members(nfds).filter(|fd| keep(*fd)).collect();
        self.clear();
        for fd in members {
            let byte = (fd / 8) as usize;
            if let Some(bits) = self.bits.get_mut(byte) {
                *bits |= 1 << (fd % 8);
            }
        }
    }

    fn write_back(&self, view: &GuestView<'_>) -> AbiResult<()> {
        let Some(at) = self.at else { return Ok(()) };
        view.mem().write_bytes(
            at,
            &self.bits,
            Blame::new(view.symbol(), view.address(), self.argument),
        )
    }
}

/// The bound both calls apply to a wait with nothing that can end it.
///
/// `None` is "wait indefinitely" and is refused; a finite wait past
/// [`MAX_SLEEP_SECONDS`](super::MAX_SLEEP_SECONDS) is refused rather than clamped.
fn bounded_wait(c: &ImportCall<'_, '_>, duration: Option<Duration>) -> AbiResult<Duration> {
    let Some(duration) = duration else {
        return Err(refuse(
            c,
            format!(
                "the guest asked `{}` to wait indefinitely, and none of the descriptors it named \
                 can ever become ready: every descriptor in this runtime is a regular file, a \
                 directory or a standard stream, all of which are ready the moment they are \
                 polled, and the two symbols that would introduce a descriptor which blocks -- \
                 `socket` and `eventfd` -- are refused by name. So this call would block the \
                 host thread for ever, and D16's runaway-guest defence is built from step \
                 budgets that a sleeping thread does not consume. Returning 0 instead would \
                 report a timeout to a call that was given none",
                c.symbol()
            ),
        ));
    };
    if duration.as_secs() > MAX_SLEEP_SECONDS {
        return Err(refuse(
            c,
            format!(
                "the guest asked `{}` to wait {duration:?} with nothing that can become ready, \
                 and this layer caps a guest-chosen wait at {MAX_SLEEP_SECONDS} seconds -- the \
                 same cap `nanosleep` and `usleep` name, and for the same reason: a sleeping \
                 thread executes no guest instructions, so no step budget can end one. Clamping \
                 to the cap was rejected, because it would return 0 from a call that waited a \
                 minute when it was asked to wait {duration:?}",
                c.symbol()
            ),
        ));
    }
    Ok(duration)
}

// ================================================================== the four that are refused

/// `int socket(int domain, int type, int protocol)`
///
/// Refused. **Omnidroid gives the guest no network at all**, and there is not a seam here that
/// happens to be empty: there is no socket module in `omni-platform` and no way for an embedding
/// to express a network policy, which is the shape `Bionic::set_filesystem_root` gives the
/// filesystem. A socket opened here would be an unrestricted host socket in the hands of
/// untrusted guest code — D6 records that the APK under test is cheat-injected and carries a Luau
/// executor — and Global Constraint 8 says this runtime makes no network access at run time.
///
/// **`-1` with `EAFNOSUPPORT` or `EACCES` was considered and rejected**, and it is the most
/// believable wrong answer this phase had available. Each is a legitimate POSIX outcome that a
/// networked program has a quiet branch for, so the engine would disable its own networking
/// during initialisation, the run would complete, and nothing anywhere would record that
/// *Omnidroid* rather than the device had made that choice. The same argument D21 makes for
/// refusing `mlock` rather than answering `-1`/`ENOMEM`.
pub(super) fn socket(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (domain, kind, protocol) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_i32()?)
    };
    let family = match domain {
        1 => "AF_UNIX",
        2 => "AF_INET",
        10 => "AF_INET6",
        16 => "AF_NETLINK",
        _ => "an address family this layer has no name for",
    };
    Err(refuse(
        c,
        format!(
            "the guest called socket({domain}, {kind}, {protocol}) -- {family}. Omnidroid gives \
             the guest no network: `omni-platform` has no socket seam, and an embedding has no \
             way to say which network a guest may reach, the way `Bionic::set_filesystem_root` \
             says which directory it may reach. A descriptor returned here would be an \
             unrestricted host socket held by untrusted guest code (D6), and Global Constraint 8 \
             forbids network access at run time. Returning -1 with EAFNOSUPPORT or EACCES was \
             rejected: both are legitimate POSIX answers a networked program branches on \
             quietly, so the engine would switch its networking off during initialisation and \
             nothing would record that this layer, rather than the device, had decided that"
        ),
    ))
}

/// `int eventfd(unsigned int initval, int flags)`
///
/// **Answered from M6, where it had been refused by name since M3.** The refusal's argument was
/// that every descriptor in this runtime belongs to `omni-platform`'s table, which `read`,
/// `__write_chk`, `close`, `fstat` and this module's `poll` are all written against, and that an
/// eventfd -- a 64-bit counter with a destructive read -- was not a kind that table had. That was
/// true and it is what changed: `omni_platform::fs::eventfd` is that kind, the table carries it,
/// and all five of those calls have an arm for it because the table's `match` has no default.
///
/// # Why it is implemented now, which is a measurement rather than a plan
///
/// `jni-surface.md` §8 row 21 reaches it. `nativeInitClientSettings` -- the call that loads the
/// client settings the engine refuses to initialise its TaskScheduler without -- asks for
/// `eventfd(0, 0o200_4000)`, which is `EFD_CLOEXEC | EFD_NONBLOCK`. The refusal was the next stop
/// after the spin lock, and it is on the only path to a frame.
///
/// # What it answers, and what it deliberately does not
///
/// The counter, the flags and the `EINVAL` on an unknown flag are `omni-platform`'s; this layer
/// converts and reports. The one decision here is the same one `pipe` makes: **`EFD_NONBLOCK` is
/// honoured and a blocking eventfd still reports `EAGAIN` rather than waiting.** A zero-counter
/// read on a blocking descriptor is the one case where this differs from a device, and it differs
/// in the direction D16 requires -- a guest parked on a counter nobody increments consumes no
/// step budget and cannot be ended. The guest's own caller here sets `EFD_NONBLOCK`, so the case
/// is not on the measured path; when something reaches it, `EAGAIN` is a value every eventfd
/// caller has an arm for, and the refusal it replaces was not.
pub(super) fn eventfd(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (initval, flags) = {
        let mut a = c.args();
        (a.next_i32()? as u32, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        match settle(&view, fs.eventfd(u64::from(initval), flags))? {
            Settled::Done(fd) => fd,
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int getaddrinfo(const char *node, const char *service, const struct addrinfo *hints, struct addrinfo **res)`
///
/// Refused, for two independent reasons and either would be enough.
///
/// **There is nowhere to put the answer.** `getaddrinfo` allocates a linked list of
/// `struct addrinfo` *in guest memory* — each node carrying an `ai_addr` pointer to a
/// `sockaddr` and an optional `ai_canonname` string — and hands back a pointer the guest walks
/// and later frees. This layer has no guest allocator to build one with: the adapter's arena is a
/// fixed set of tables sized at construction, at 65,280 bytes of a 65,536-byte commit granule
/// with 256 bytes spare, its pool is a bump allocator that never frees — so a guest resolving in
/// a loop would exhaust it and never get the memory back — and task 2's finding F9 forbids a
/// handler mapping guest memory at all, because an inline handler runs with generated code live.
/// The guest's own allocator is not reachable from here either: `libroblox.so` imports no
/// allocator (D17), it carries its own and reaches the host through guest `mmap`.
///
/// **And the resolution itself needs a network**, which is the whole of `socket`'s argument.
///
/// One thing is worth recording for whoever implements this later: `sizeof(struct addrinfo)` on
/// LP64 bionic would be **48 bytes** — `int ai_flags, ai_family, ai_socktype, ai_protocol`, a
/// `socklen_t ai_addrlen` with four bytes of padding after it, then `char *ai_canonname`,
/// `struct sockaddr *ai_addr` and `struct addrinfo *ai_next`. **That is ASSUMED, not verified**:
/// there is no NDK on this machine, it is the same gap `FILE_BYTES` and `layouts.rs` record, and
/// bionic orders `ai_canonname` before `ai_addr` where glibc does the reverse — so a
/// glibc-derived layout would put the canonical name where the address belongs.
pub(super) fn getaddrinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (node, service, hints, res) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    Err(refuse(
        c,
        format!(
            "the guest called getaddrinfo(node={node:#x}, service={service:#x}, \
             hints={hints:#x}, res={res:#x}). Two things are missing and either alone is \
             decisive. There is nowhere to build the answer: a `struct addrinfo` list lives in \
             GUEST memory and must be freeable by `freeaddrinfo`, and this layer has no guest \
             allocator -- the arena is fixed at construction and the pool is a bump allocator \
             that never frees, while F9 forbids a handler mapping guest memory. And the \
             resolution needs DNS, which means a network this runtime does not give the guest \
             (see `socket`) and Global Constraint 8 forbids at run time. Returning EAI_NONAME or \
             EAI_FAIL was rejected: a caller retries EAI_AGAIN, reports EAI_FAIL as a real DNS \
             failure, and either way believes it asked a resolver"
        ),
    ))
}

/// `void freeaddrinfo(struct addrinfo *res)`
///
/// Refused, and the **`void` return is exactly why**. There is no value to get wrong, so a stub
/// here would be invisible: it would do nothing, report nothing, and be indistinguishable from a
/// correct implementation until something needed the memory back.
///
/// Nothing in this layer can produce an `addrinfo` list — `getaddrinfo` refuses — so any pointer
/// that arrives here was not made by this layer. Freeing it is impossible (there is nothing to
/// free), and *ignoring* it would be a claim that the list is gone.
pub(super) fn freeaddrinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let res = c.args().next_u64()?;
    Err(refuse(
        c,
        format!(
            "the guest called freeaddrinfo({res:#x}). Nothing in this layer can have produced \
             that list -- `getaddrinfo` refuses by name -- so the pointer came from somewhere \
             else. This function returns `void`, which is what makes doing nothing the \
             dangerous answer here: a silent no-op is indistinguishable from a correct free, and \
             it would still be indistinguishable on the day `getaddrinfo` starts returning real \
             lists and this stub starts leaking them"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The `POLL*` constants are the Linux values**, written as literals.
    ///
    /// The same discipline `omni_bionic::errno` records, and for the same reason: these numbers
    /// reach the guest, the development host's own `poll` constants are different
    /// (`FD_READ`/`FD_WRITE` are not these at all), and a wrong one makes the guest take the
    /// wrong branch rather than making the build fail. Comparing a constant to itself would pass
    /// against any value.
    ///
    /// `POLLPRI`, `POLLERR` and `POLLHUP` are carried and pinned although nothing here can
    /// produce them: each names a condition this runtime has no source for — out-of-band data, a
    /// device error, a hang-up — and the assertion that they are *not* in [`READY_MASK`] is what
    /// says so.
    #[test]
    fn the_poll_constants_are_the_linux_values() {
        assert_eq!(POLLIN, 0x001);
        assert_eq!(POLLPRI, 0x002);
        assert_eq!(POLLOUT, 0x004);
        assert_eq!(POLLERR, 0x008);
        assert_eq!(POLLHUP, 0x010);
        assert_eq!(POLLNVAL, 0x020);
        assert_eq!(POLLRDNORM, 0x040);
        assert_eq!(POLLWRNORM, 0x100);
        assert_eq!(READY_MASK, 0x145, "POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM");
        for never in [POLLPRI, POLLERR, POLLHUP, POLLNVAL] {
            assert_eq!(
                READY_MASK & never,
                0,
                "nothing in this runtime can report {never:#x} as ready"
            );
        }
        assert_eq!(POLLFD_BYTES, 8, "int + short + short on LP64");
        assert_eq!(TIMEVAL_BYTES, 16, "two longs on LP64");
        assert_eq!(FD_SETSIZE as usize / 8, 128, "sizeof(fd_set)");
    }

    /// The `fd_set` bit order is the kernel's: descriptor *n* is bit *n mod 8* of byte *n / 8*.
    ///
    /// Asserted on an asymmetric pattern, because a byte- or word-reversed implementation reads a
    /// different set of descriptors and every set still looks like a set.
    #[test]
    fn a_guest_fd_set_is_indexed_the_way_the_kernel_indexes_it() {
        let mut bits = vec![0u8; 128];
        for fd in [0i32, 1, 7, 8, 63, 64, 1023] {
            bits[(fd / 8) as usize] |= 1 << (fd % 8);
        }
        let set = Set { at: None, bits, argument: 0 };
        let members: Vec<i32> = set.members(FD_SETSIZE).collect();
        assert_eq!(members, vec![0, 1, 7, 8, 63, 64, 1023]);
        assert_eq!(set.count(FD_SETSIZE), 7);
        // `nfds` bounds what is seen, which is the whole of what `nfds` is for.
        assert_eq!(set.count(64), 5, "descriptors 64 and 1023 are above nfds");
        assert_eq!(set.count(0), 0);
        assert!(!set.contains(2));
        // A set shorter than the descriptor asked about answers `false` rather than indexing out
        // of bounds: `select` reads only `FDS_BYTES(nfds)` and a guest may pass a smaller object.
        let short = Set { at: None, bits: vec![0xFF; 1], argument: 0 };
        assert!(short.contains(7));
        assert!(!short.contains(8), "a byte-long set says nothing about descriptor 8");
    }

    /// The two caps, pinned.
    ///
    /// Their *behaviour* — `EINVAL` past `MAX_POLL_FDS`, `EINVAL` past `FD_SETSIZE`, and a wait
    /// refused rather than clamped — needs a real guest call and is asserted in
    /// `crates/omni-android/tests/bionic.rs`, which is where a thunk crossing exists. What is
    /// here is only that the numbers are the ones the documentation names.
    #[test]
    fn the_two_caps_are_the_numbers_the_documentation_names() {
        assert_eq!(MAX_POLL_FDS, 1024, "Linux's usual RLIMIT_NOFILE soft limit");
        assert_eq!(FD_SETSIZE, 1024, "and the size of an fd_set in bits");
    }
}
