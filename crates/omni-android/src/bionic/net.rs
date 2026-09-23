//! The network symbols: sockets, name resolution, and the two calls that watch a descriptor set.
//!
//! `socket`, `connect`, `bind`, `shutdown`, `setsockopt`, `getsockopt`, `getsockname`, `sendto`, `recvfrom`,
//! `__sendto_chk`, `read`/`write` **over a socket**, `poll`, `select`, `eventfd`, `getaddrinfo`,
//! `freeaddrinfo`, `gai_strerror`, `inet_ntop`, `inet_pton`.
//!
//! # What this module used to say, and the one decision that overturned it
//!
//! Its heading read "the eight network symbols: two answered from `omni-bionic`, two implemented
//! here, four refused by name", and the argument under it was that **no socket seam existed and
//! none was wanted**: Global Constraint 8 said this runtime makes no network access at run time,
//! so `socket`, `getaddrinfo` and `freeaddrinfo` were refused by name and `poll` and `select`
//! answered over a descriptor space in which nothing could ever come from outside this process.
//! That reasoning is **corrected in place rather than deleted**, the way the `AF_INET6` and
//! `mallinfo` entries in this area already are, because what changed is a decision and not a
//! mistake.
//!
//! **D30 withdrew Global Constraint 8.** The project owner's instruction is quoted in that record;
//! the operative half is that playable Roblox needs login, settings and a game server, and that a
//! measured failure path may be used as a diagnostic and never as the finished behaviour. So the
//! refusal is gone and what replaced it is not an open socket — it is
//! [`Bionic::set_network_policy`](super::Bionic::set_network_policy), which is the sentence
//! `set_filesystem_root` already makes about directories: **which network may this instance reach
//! is a question the embedding answers.** D6's threat is unchanged. An instance whose embedding
//! has not answered it creates no socket at all, and says so by name.
//!
//! | symbol | where its answer comes from |
//! |---|---|
//! | `inet_ntop`, `inet_pton`, `gai_strerror` | [`omni_bionic::net`] — formatting, parsing, a constant table. No state and no OS |
//! | `socket`, `connect`, `bind`, `shutdown`, `setsockopt`, `getsockopt`, `getsockname`, `sendto`, `recvfrom`, `__sendto_chk` | [`omni_platform::net`] — **the one place in the workspace a socket call is made** |
//! | `getaddrinfo` | [`omni_platform::net::resolve`], with the list marshalled into the guest's own memory by [`addrinfo`](super::addrinfo) |
//! | `freeaddrinfo` | the slab's free list, matching the head pointer this layer handed out |
//! | `read`, `write`, `__write_chk` | **dispatched** here: a socket goes to `recv`/`send`, everything else to `files` |
//! | `poll`, `select` | here, over `omni-platform`'s one descriptor table — which now holds sockets too |
//! | `eventfd` | `omni_platform::fs` |
//!
//! # Why `read` and `write` are bound here rather than in `files`
//!
//! **Because `libroblox.so` imports no `recv` and no `send` at all.** MEASURED, from the APK's own
//! undefined-symbol table: the stream data path is `read` and `write` on the socket descriptor —
//! which is what OpenSSL's `readsocket`/`writesocket` expand to on every non-Windows target, and
//! the engine carries its own OpenSSL. `recvfrom`, `sendto` and `__sendto_chk` are imported and
//! are the datagram path; `recvmsg`, `sendmsg` and the `mmsg` forms are imported and **nothing has
//! reached them**, so they stay `Unbound` (D17).
//!
//! So a socket has to be reachable through the two symbols `files` already owned, and the
//! dispatch is one `is_socket` test at the top of each. It is here rather than in `files` for two
//! reasons that are both about *not* flattening a failure:
//!
//! * a socket fails with `ECONNRESET`, `ECONNREFUSED`, `ETIMEDOUT` and `ENOTCONN`, none of which
//!   `omni_platform::fs::FsErrorKind` can express, and a socket read routed through the
//!   filesystem seam would have to arrive as one of the file errnos or as a refusal;
//! * a *blocking* file or pipe is waited out on `Filesystem::wait_for_readiness`, and that gate
//!   **never rises for a socket** — nothing in this process changes a socket's state. A blocking
//!   socket read on that path would wait its whole budget on a descriptor that was ready the
//!   moment it started.
//!
//! `Filesystem::read` and `Filesystem::write` refuse a socket by name and say so, so the dispatch
//! failing open is a loud failure rather than a silent one.
//!
//! # Why `poll` and `select` need an operating system now, which they did not before
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
//! has to grow a real readiness source with it.* M5 replaced the argument's first half — `pipe`
//! made a descriptor whose readiness is state — and **M6 is the day D25 actually named.** A socket
//! is the first descriptor here whose readiness is a question for the kernel, so `poll` and
//! `select` now make an OS call, and `omni_platform::net::poll` is where it is made.
//!
//! `omni-platform`'s [`Filesystem::readiness`] is still a `match` over the descriptor kinds with
//! **no default arm**, which is what forced the socket variant to decide its own answer rather
//! than inherit one — including what an infallible readiness reports when the host refuses to poll
//! at all, which is `error` and is written up beside that arm.
//!
//! [`Filesystem::readiness`]: omni_platform::fs::Filesystem::readiness
//! [`Readiness::ALWAYS`]: omni_platform::fs::Readiness::ALWAYS
//!
//! # Waiting on a mixed set: two sides, alternating slices, and the wakeup that would be lost
//!
//! A guest `poll` names pipes, eventfds **and** sockets. There is no single call that waits on
//! both halves: the in-process half is a condition variable and the socket half is a kernel
//! object. So the loop is *test everything, wait a slice on whichever side can wait, test again*,
//! and [`omni_platform::fs::ReadinessSource`] is what says which side each descriptor is on.
//!
//! When the set is **sockets only**, the whole remaining budget goes into one
//! `omni_platform::net::poll`, which is the cheapest and most responsive shape available. When it
//! is **mixed**, the socket wait is capped at [`MIXED_WAIT_SLICE`] so the in-process half is
//! re-tested that often; when it is **in-process only**, the readiness gate is waited on exactly
//! as before.
//!
//! **The gate generation is read before the descriptors are tested**, in every one of those
//! branches. A write that lands between the test and the wait raises it and the wait returns at
//! once; reading it afterwards is the lost wakeup this project has already measured once, at
//! 1.0104 s (`sem_post`, `VERIFICATION.md` entry 11). The hazard is worse in a mixed set than it
//! was in a pure one, because the thread may be sleeping in `select` on the *other* side when the
//! pipe write lands — which is exactly why the socket slice is bounded rather than open-ended.
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
//! **That refusal has now outlived two of its own reasons and is kept by a third.** It rested
//! first on "none of the descriptors it named can ever become ready", which a pipe made false, and
//! then on "nothing outside this process can make one ready", which a socket makes false. What is
//! left is the step-budget argument alone, which is the half that was always load-bearing: a host
//! thread parked for ever on a socket nobody writes to is exactly as unrecoverable as one parked
//! on a regular file. A blocking transfer is bounded the same way, by the same number.
//!
//! # The `-1`/`errno` versus refusal split, as the rest of the adapter draws it
//!
//! `EINVAL` for an `nfds` past the cap and for a malformed `struct timeval`; `EBADF` from
//! `select` for a descriptor that is not open. Those are Linux's own answers and guest code has a
//! branch for each. `poll` reports the same bad descriptor as `POLLNVAL` in that entry's
//! `revents` rather than as `-1`, because that is what `poll` does and the two calls genuinely
//! differ here.
//!
//! For the socket calls the split is [`settled`]: a classified host failure becomes the guest's
//! errno, and **everything else refuses by name**. In particular
//! [`ResolveFailure::Unclassified`](omni_platform::net::ResolveFailure::Unclassified) is refused
//! rather than given an `EAI_*` code, and an option outside
//! [`SocketOption`](omni_platform::net::SocketOption) is refused through
//! [`NetError::unimplemented_option`](omni_platform::net::NetError::unimplemented_option) carrying
//! the level and name the guest passed. A `setsockopt` that is silently accepted is precisely the
//! defect Global Constraint 1 is about: the caller believes the option took effect and behaves as
//! though it had.
//!
//! **And on any of those failures the guest's own objects are left alone.** POSIX says a failed
//! `select` does not modify the sets, and `poll` answers its whole array or none of it. Both are
//! the direction review finding M1 says to err in, and `select`'s ordering — validate the
//! timeout, then rewrite the sets — was wrong in the first version of this module.

use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_bionic::net;
use omni_mem::GuestAddr;
use omni_platform::fs::{
    EpollMember, EpollOp, Filesystem, FsErrorKind, Readiness, ReadinessSource, EPOLL_CLOEXEC,
    TFD_TIMER_ABSTIME,
};
// **`platnet` rather than `net`**, because `net` in this module is already `omni_bionic::net` —
// the pure-computation half — and the two are deliberately different crates (D19). A single
// import name for both would make it impossible to see, at a call site, whether an OS call is
// being made.
use omni_platform::net as platnet;
use omni_platform::net::{
    ConnectOutcome, ConnectProgress, Interest, IpFamily, NetError, NetErrorKind, NetPolicy,
    OptionValue, PathMtu, PollEntry, ResolveFailure, Shutdown, Socket, SocketAddress, SocketKind,
    SocketOption, SocketQuery,
};

use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::addrinfo;
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

/// `int inet_pton(int af, const char *src, void *dst)`
///
/// The third symbol answered out of [`omni_bionic::net`] and the first of the three that is not
/// one of the 188: the initializers never reach it, and what found it was a guest **worker
/// thread** dying on it — `thread 7`, start routine at image offset `0x2217f04`, calling through
/// its thunk at `0x2c6fd3e3270`. That is the same start routine `strcspn` was found on, which is
/// the engine's URL and address handling.
///
/// The marshalling is all of what is here, and the three outcomes are kept apart because the
/// guest's branches are:
///
/// * **1** — `src` was converted, and `dst` holds four or sixteen bytes in network order;
/// * **0** — `src` is not a valid address for `af`. **Not an error**, so errno is untouched:
///   a caller that tries `AF_INET` and then `AF_INET6` would otherwise find an errno from the
///   attempt that was *meant* to fail;
/// * **-1** with `EAFNOSUPPORT` — `af` is neither family, which is the only one of the three
///   that sets errno, exactly as bionic's `switch (af)` default arm does.
///
/// There is no `socklen_t` here and so no `W3`/`X3` question: `inet_pton` takes three arguments
/// and the destination's size is implied by the family.
pub(super) fn inet_pton(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (af, src, dst) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        match net::inet_pton(&mut view, af, src, dst) {
            Ok(Ok(true)) => 1,
            Ok(Ok(false)) => 0,
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
    // The second half of the pair is **whether anything this call named can become ready**,
    // which is what decides whether the wait is a sleep or a wait for an event. See
    // [`bounded_wait`].
    let (first, can_change) = {
        let mut view = enter(c, &state);
        if nfds > MAX_POLL_FDS {
            // Linux's own answer for an `nfds` past the process's descriptor limit.
            view.set_errno(consts::EINVAL);
            (Some(-1), false)
        } else {
            // `nfds` is bounded above, so this cannot overflow.
            let bytes = nfds as usize * POLLFD_BYTES;
            let (ready, watch) = if bytes == 0 {
                // A zero-length array is legal and `fds` may be anything, null included — POSIX
                // says so, and it is the idiom for "sleep for `timeout` milliseconds". Nothing is
                // read, and in particular the descriptor table is not consulted, so a `poll` used
                // as a sleep works on an instance that has no filesystem.
                (0, Watch::default())
            } else {
                poll_entries(&view, fds, bytes)?
            };
            (if ready > 0 { Some(ready) } else { None }, watch.can_change())
        }
    };
    let value = match first {
        Some(value) => value,
        None => {
            // Nothing is ready yet. The wait is bounded here, before any of it happens, so a
            // refusal arrives instead of a sleep rather than after one.
            let duration =
                if timeout < 0 { None } else { Some(Duration::from_millis(timeout as u64)) };
            let budget = bounded_wait(c, duration, can_change)?;
            let bytes = nfds as usize * POLLFD_BYTES;
            wait_until_ready(c, &state, budget, |view| {
                if bytes == 0 {
                    Ok((0, Watch::default()))
                } else {
                    poll_entries(view, fds, bytes)
                }
            })?
        }
    };
    c.ret().i32(value);
    Ok(())
}

/// Which side of a mixed wait the descriptors a call named are on.
///
/// **The reason this exists is that there is no single call that waits on both sides.** The
/// in-process half — a pipe, an eventfd — is a condition variable this process owns, and the
/// socket half is a kernel object; `omni_platform::fs::ReadinessSource` is what says which a given
/// descriptor is. A descriptor that is [`ReadinessSource::Immediate`] is recorded nowhere, because
/// it is already ready and a caller that waited on one would wait for ever.
///
/// The interests are **unioned per descriptor**, not appended, so a `poll` array that names one
/// socket twice — once for `POLLIN` and once for `POLLOUT` — produces one entry asking about
/// both. That matters for more than tidiness: the same socket appearing twice would mean locking
/// its mutex twice in one thread, which deadlocks.
#[derive(Debug, Default)]
struct Watch {
    /// Every distinct socket named, with the union of what was asked about it.
    sockets: Vec<(i32, Interest)>,
    /// Whether anything named waits on this instance's readiness gate.
    gate: bool,
    /// The earliest timerfd deadline named, on the monotonic clock: a time at which readiness
    /// changes with **nothing** announcing it, so no wait may run past it. See
    /// [`ReadinessSource::Timer`].
    deadline: Option<Duration>,
}

impl Watch {
    /// Whether anything in this set has a readiness that can **change**.
    ///
    /// **The predicate the wait cap turns on**, and it is the one [`bounded_wait`]'s refusal has
    /// always claimed to be about: "nothing that can become ready". A socket's readiness is the
    /// network's and an entry on the gate is another descriptor's writer, so either one makes
    /// this call a wait for an event rather than a sleep with a timer. A set of files,
    /// directories and standard streams makes it false -- every one of those is
    /// `Readiness::ALWAYS`, so a wait on them has already returned.
    fn can_change(&self) -> bool {
        !self.sockets.is_empty() || self.gate
    }

    /// Record what one descriptor was asked about.
    fn note(&mut self, fs: &Filesystem, fd: i32, interest: Interest) {
        match fs.readiness_source(fd) {
            Some(ReadinessSource::Host) => {
                match self.sockets.iter_mut().find(|(seen, _)| *seen == fd) {
                    Some((_, held)) => {
                        held.readable |= interest.readable;
                        held.writable |= interest.writable;
                    }
                    None => self.sockets.push((fd, interest)),
                }
            }
            Some(ReadinessSource::Gate) => self.gate = true,
            // Re-arming raises the gate; expiring raises nothing, so the deadline is kept too.
            Some(ReadinessSource::Timer) => {
                self.gate = true;
                if let Some(at) = fs.timer_deadline(fd) {
                    self.deadline = Some(self.deadline.map_or(at, |held| held.min(at)));
                }
            }
            // `Immediate` is already ready, so the caller will have counted it and never reached
            // a wait; `None` is a descriptor that is not open, which `poll` has already answered
            // `POLLNVAL` and `select` `EBADF`. Neither is something to wait on.
            Some(ReadinessSource::Immediate) | None => {}
        }
    }
}

/// Wait one step on whichever side of the set can wait, without losing a wakeup from the other.
///
/// The three cases, and the middle one is the whole of the mixed-set problem:
///
/// * **No socket.** The readiness gate is waited on for the whole remaining budget, exactly as
///   before sockets existed. `seen` was read *before* the descriptors were tested, so a pipe write
///   that landed in between raises the generation past it and the wait returns at once.
/// * **Sockets and something on the gate.** No call waits on both, so the socket side is given
///   [`MIXED_WAIT_SLICE`] and the loop re-tests everything after it. Nothing is lost — the gate
///   generation is re-read on the next pass and a change that landed during the slice is still
///   there — but an in-process wakeup can be **late** by up to one slice, which is why the slice
///   is small and why it is a constant with a reason attached rather than a number.
/// * **Sockets only.** The whole remaining budget goes into one `omni_platform::net::poll`, which
///   wakes exactly when the kernel says so and costs nothing while it waits.
///
/// The socket handles are locked in **ascending descriptor order**, so two guest threads waiting
/// on overlapping sets cannot take two locks in opposite orders. They are all held for the
/// duration of the one readiness call and released before the next test — which is why the table
/// lock is not held here: the order is table-then-socket everywhere, and this takes the socket
/// locks with no table lock in hand.
fn wait_a_slice(
    c: &ImportCall<'_, '_>,
    fs: &Filesystem,
    watch: &Watch,
    seen: u64,
    remaining: Duration,
) -> AbiResult<()> {
    // **Never past a timer's deadline**: its expiry changes readiness and raises nothing, so a
    // wait that ran past it would report the timer late by whatever was left of the slice.
    let remaining = match watch.deadline {
        Some(at) => remaining.min(at.saturating_sub(omni_platform::clock::monotonic_now())),
        None => remaining,
    };
    if watch.sockets.is_empty() {
        fs.wait_for_readiness(seen, remaining);
        return Ok(());
    }
    let mut named = watch.sockets.clone();
    named.sort_unstable_by_key(|(fd, _)| *fd);
    let held: Vec<(Arc<std::sync::Mutex<Socket>>, Interest)> = named
        .into_iter()
        // A descriptor another guest thread closed between the test and here is simply not
        // waited on; the next pass of the loop answers `POLLNVAL` for it.
        .filter_map(|(fd, interest)| fs.socket_at(fd).ok().map(|handle| (handle, interest)))
        .collect();
    if held.is_empty() {
        fs.wait_for_readiness(seen, remaining);
        return Ok(());
    }
    // **Every individual host park stays under the cap**, which is what lets `bounded_wait`
    // honour a longer total: a mixed set is re-tested every `MIXED_WAIT_SLICE` so the in-process
    // half is not late, and a sockets-only set parks for at most `MAX_SLEEP_SECONDS` at a time,
    // so the sum the guest asked for is made of parks no longer than the ones `nanosleep` allows.
    let slice = if watch.gate {
        remaining.min(MIXED_WAIT_SLICE)
    } else {
        remaining.min(Duration::from_secs(MAX_SLEEP_SECONDS))
    };
    let guards: Vec<std::sync::MutexGuard<'_, Socket>> =
        held.iter().map(|(handle, _)| locked(handle)).collect();
    let mut entries: Vec<PollEntry<'_>> = guards
        .iter()
        .zip(held.iter())
        .map(|(guard, (_, interest))| PollEntry::new(guard, *interest))
        .collect();
    // The set cannot be larger than `omni_platform::net::MAX_POLL_SOCKETS`: this instance holds
    // at most `omni_platform::fs::MAX_OPEN_FILES` descriptors and the compile-time assertion
    // beside that constant is what says the one bound is inside the other. A failure here is the
    // host's readiness call failing, which is refused by name rather than reported as a timeout.
    platnet::poll(&mut entries, slice).map_err(|error| refuse(c, error.to_string()))?;
    Ok(())
}

/// Re-test readiness until something is ready or the budget runs out, waiting on whichever side
/// of the descriptor set can wait.
///
/// `test` is whatever the caller counts as ready — the `pollfd` array for `poll`, the three
/// `fd_set`s for `select` — and it writes the guest's own objects back each time it runs, because
/// the last run is the one the guest sees and the caller cannot know in advance which that is. It
/// also reports a [`Watch`]: which of the descriptors it looked at are sockets and which wait on
/// the gate, which is what the next line needs to know how to sleep.
///
/// **The generation is read before `test` runs.** A write that lands between the test and the wait
/// raises it, so the wait returns immediately rather than sleeping through the event. The other
/// order is the lost wakeup `VERIFICATION.md` entry 11 measured at 1.0104 s — and the hazard is
/// sharper now than it was, because with a socket in the set the thread may be asleep in the
/// host's `select` when the pipe write lands. [`wait_a_slice`] is where that is answered.
///
/// An instance with **no filesystem** has no descriptors at all, so nothing can ever become ready
/// and the wait degenerates to a sleep. That is a real branch, not a fallback: `poll(NULL, 0, 50)`
/// as a sleep is legal on an instance that has no filesystem root.
fn wait_until_ready(
    c: &ImportCall<'_, '_>,
    state: &Active,
    budget: Duration,
    mut test: impl FnMut(&GuestView<'_>) -> AbiResult<(i32, Watch)>,
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
        let (ready, watch) = {
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
        stop_requested(c, state)?;
        wait_a_slice(c, fs, &watch, seen, deadline - now)?;
    }
}

/// Read the guest's `pollfd` array, answer every entry, write it back, and count the ready ones.
///
/// **Read whole, decide, write whole.** One access in each direction rather than one per entry,
/// so an array that is only partly mapped leaves the guest's `revents` untouched rather than half
/// updated — the same all-or-nothing shape `clocks::write_pair` and `files::write_struct` use,
/// and the direction review finding M1 says to err in.
/// It also reports the [`Watch`] a wait needs: which of the descriptors it looked at are sockets,
/// with the union of what each was asked about, and whether any of them waits on the gate.
fn poll_entries(view: &GuestView<'_>, fds: u64, bytes: usize) -> AbiResult<(i32, Watch)> {
    let at = guest_address(view, fds)?;
    let blame = Blame::new(view.symbol(), view.address(), 0);
    let mut entries = view.mem().read_bytes(at, bytes, blame)?;
    let mut watch = Watch::default();
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
                // **Only `EBADF` is `POLLNVAL`.** The comment on the arm below said no other seam
                // failure could reach here; an epoll descriptor's readiness, which the seam
                // refuses by name, made that false, and a refusal read as `POLLNVAL` would tell the
                // guest a live descriptor was closed.
                Some(Err(error)) if error.kind() != Some(FsErrorKind::BadDescriptor) => {
                    return Err(view.refusal(error.to_string()));
                }
                Some(Ok(readiness)) => {
                    // What this entry asks about, for the wait that may follow. `POLLPRI` is
                    // deliberately not folded into `readable`: nothing in this runtime produces
                    // out-of-band data, and treating a request for it as a request for ordinary
                    // data would wake a caller for something it did not ask about.
                    if let Some(fs) = fs {
                        watch.note(
                            fs,
                            fd,
                            Interest {
                                readable: events & READABLE_MASK != 0,
                                writable: events & WRITABLE_MASK != 0,
                            },
                        );
                    }
                    revents_for(readiness, events)
                }
                // Reported whether or not it was requested, which is what `POLLNVAL` is for. A
                // seam failure that is not `EBADF` cannot reach here: `readiness` answers from
                // the table alone and, for a socket, from a host call that reports its own
                // failure as `error` rather than as a refusal.
                _ => POLLNVAL,
            }
        };
        entry[6..8].copy_from_slice(&revents.to_le_bytes());
        if revents != 0 {
            ready += 1;
        }
    }
    view.mem().write_bytes(at, &entries, blame)?;
    Ok((ready, watch))
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
        // `answer_sets` reads a refused readiness as "not ready", so an epoll descriptor -- whose
        // readiness the seam does not answer -- is refused here, by name, before it can be.
        if let Some(epfd) = named.iter().find(|fd| fs.is_epoll(**fd)) {
            return Err(view.refusal(format!(
                "`select` was asked about fd {epfd}, an epoll descriptor. Its readiness is its \
                 members' and is not answered here; Linux allows it and no run has reached it"
            )));
        }
    }
    // **What the guest asked about, kept**, because a wait re-tests the same question and the
    // sets are about to be overwritten with the answer.
    let asked: [Set; 3] = [sets[0].copy(), sets[1].copy(), sets[2].copy()];
    // The watch is kept rather than discarded: it says whether the descriptors this call named
    // can become ready, which is what `bounded_wait` below turns the cap on. See there.
    let first_watch = answer_sets(view, &asked, &mut sets, nfds);
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
    let wait = bounded_wait(c, duration, first_watch.can_change())?;
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
        let watch = answer_sets(view, &asked, &mut sets, nfds);
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
        stop_requested(c, view.active)?;
        match (seen, view.active.bionic.filesystem()) {
            // **The same alternating wait `poll` makes**, and through the same function, so the
            // two calls cannot drift apart about which side of a mixed set they sleep on.
            (Some(seen), Some(fs)) => {
                wait_a_slice(c, fs, &watch, seen, deadline - now)?;
            }
            // No filesystem means no descriptors at all, so nothing can change and the wait is
            // the sleep it always was. A `select` used purely as a sleep does not need a
            // filesystem root.
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

/// It also reports the [`Watch`] a wait needs, built from what the guest **asked** rather than
/// from what is ready — a descriptor that is ready is not waited on.
fn answer_sets(
    view: &GuestView<'_>,
    asked: &[Set; 3],
    sets: &mut [Set; 3],
    nfds: i32,
) -> Watch {
    let readiness = |fd: i32| {
        view.active.bionic.filesystem().and_then(|fs| fs.readiness(fd).ok())
    };
    sets[0].restore(&asked[0]);
    sets[1].restore(&asked[1]);
    // **An errored descriptor counts as both readable and writable**, and that clause is what
    // makes `select` work over a socket at all. Linux reports a failed non-blocking connect by
    // making the socket *writable* — the write fails immediately, which is what "ready" means to
    // `select` — and `getsockopt(SO_ERROR)` is then how the caller learns why. Windows' `select`,
    // which is `omni_platform::net`'s backend, reports the same condition in `exceptfds`, so it
    // arrives here as `Readiness::error` with `writable` clear. Without this clause a guest
    // waiting for writability after a refused connect would never wake, and `exceptfds` is not
    // where Linux would have told it either.
    //
    // The same clause covers a pipe write end whose readers have all gone, which reports `error`
    // and not `writable`: a `write` on it returns `EPIPE` immediately, which is ready.
    sets[0].retain(nfds, |fd| readiness(fd).is_some_and(|r| r.readable || r.error));
    sets[1].retain(nfds, |fd| readiness(fd).is_some_and(|r| r.writable || r.error));
    // **`exceptfds` is emptied.** Linux sets it for out-of-band socket data and for a few `ioctl`
    // conditions on character devices, and this runtime produces neither: the seam's backend
    // never reports urgent data, and a failed connect is reported above where Linux reports it.
    sets[2].clear();

    let mut watch = Watch::default();
    if let Some(fs) = view.active.bionic.filesystem() {
        for fd in asked[0].members(nfds) {
            watch.note(fs, fd, Interest::READABLE);
        }
        for fd in asked[1].members(nfds) {
            watch.note(fs, fd, Interest::WRITABLE);
        }
        // A descriptor named only in `exceptfds` is still watched — with neither interest, which
        // the readiness backend answers as "tell me if something is wrong with it". Dropping it
        // would leave a `select` that named a socket only there waiting on nothing.
        for fd in asked[2].members(nfds) {
            watch.note(fs, fd, Interest { readable: false, writable: false });
        }
    }
    watch
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
/// `None` is "wait indefinitely" and is refused.
///
/// # The 60-second cap applies to a wait nothing can end, and to nothing else
///
/// **This is `VERIFICATION.md` entry 13's shape, found in this module's own refusal text.** The
/// message that cap produces has always said *"with nothing that can become ready"*, and that
/// sentence was true when it was written -- every descriptor in this runtime was a file, a
/// directory or a standard stream, all `Readiness::ALWAYS`, so a `poll` that was not already
/// satisfied was a sleep with a timer on it. A pipe made it half-false and a socket made it
/// false: a `poll` over a socket is a wait for the network, and it ends when the peer speaks.
///
/// MEASURED, and this is what made the sentence worth re-reading: the engine's HTTP stack asks
/// `poll` for **69.001 s** over the client-settings socket. The cap refused it, the refusal
/// killed the guest thread carrying that connection, and the fetch came back on another thread
/// as `fetch flag exception: HttpError: Unknown` -- a network failure this layer had caused and
/// then attributed to the network.
///
/// So the cap now turns on [`Watch::can_change`]:
///
/// * **Nothing in the set can become ready.** The wait is a sleep, `nanosleep`'s argument applies
///   unchanged -- a sleeping thread executes no guest instructions, so no step budget can end one
///   (D16) -- and a finite wait past [`MAX_SLEEP_SECONDS`](super::MAX_SLEEP_SECONDS) is refused
///   rather than clamped, because clamping would return 0 from a call that waited a minute when
///   it was asked to wait longer.
/// * **Something in the set can become ready.** The guest's own timeout is honoured. What makes
///   that safe is not a judgement about how long is reasonable: it is that [`wait_until_ready`]
///   is a **loop of bounded slices** that re-tests the whole set on every pass, and
///   [`wait_a_slice`] caps each host call at [`MAX_SLEEP_SECONDS`] as well -- so the cap still
///   governs every individual park, and what has been lifted is only the bound on the *sum* of
///   parks, which the guest asked for and which ends the moment the descriptor is ready.
///
/// **What would falsify this**: a run in which a guest thread sits in `poll` past the gate's
/// watchdog over a set that can never become ready. That would mean a descriptor whose
/// `readiness_source` says its readiness can change when it cannot, which is a defect there
/// rather than in this cap.
fn bounded_wait(
    c: &ImportCall<'_, '_>,
    duration: Option<Duration>,
    can_change: bool,
) -> AbiResult<Duration> {
    let Some(duration) = duration else {
        return Err(refuse(
            c,
            format!(
                "the guest asked `{}` to wait indefinitely, and this layer has no unbounded \n                 wait. The reason this refusal used to give -- that none of the descriptors \n                 it named can ever become ready, because every descriptor here was a file, \n                 a directory or a standard stream and `socket` and `eventfd` were refused \n                 by name -- has now outlived itself twice: a pipe made a descriptor whose \n                 readiness is state, and M6 made a socket, whose readiness is the network. \n                 What is left is the half that was always load-bearing: a host thread \n                 parked for ever on a socket nobody writes to is exactly as unrecoverable \n                 as one parked on a regular file, and D16's runaway-guest defence is built \n                 from step budgets that a sleeping thread does not consume. Returning 0 \n                 instead would report a timeout to a call that was given none",
                c.symbol()
            ),
        ));
    };
    if !can_change && duration.as_secs() > MAX_SLEEP_SECONDS {
        return Err(refuse(
            c,
            format!(
                "the guest asked `{}` to wait {duration:?} with nothing that can become ready, \
                 and this layer caps a guest-chosen wait at {MAX_SLEEP_SECONDS} seconds -- the \
                 same cap `nanosleep` and `usleep` name, and for the same reason: a sleeping \
                 thread executes no guest instructions, so no step budget can end one. Clamping \
                 to the cap was rejected, because it would return 0 from a call that waited a \
                 minute when it was asked to wait {duration:?}. Note what this refusal is \
                 NOT about: a set naming a socket or a pipe is a wait for an event, and its \
                 timeout is honoured in full because the wait re-tests every slice and ends the \
                 moment the descriptor is ready",
                c.symbol()
            ),
        ));
    }
    Ok(duration)
}

// ================================================================== epoll

/// `EPOLLIN`, `EPOLLOUT`, `EPOLLERR` and `EPOLLHUP`: Linux `uapi/linux/eventpoll.h`.
const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;

/// The event bits this layer delivers as asked: **level-triggered** input and output, and the two
/// conditions reported whether or not they are asked for.
///
/// Decided from the engine, not from the header. Its transport (`libroblox.so` link
/// `0x23cc814`-`0x23cc848`) builds `events` from two bits of its own and nothing else --
/// `EPOLLIN`, `EPOLLOUT`, or both -- so `EPOLLET`, `EPOLLONESHOT`, `EPOLLRDHUP`, `EPOLLPRI`,
/// `EPOLLEXCLUSIVE` and `EPOLLWAKEUP` are refused by name, each a semantics this layer would
/// otherwise claim and not keep: an edge-triggered caller woken level-triggered spins on a
/// writable socket, and a one-shot caller re-reported loses its own bookkeeping.
const EPOLL_REQUESTABLE: u32 = EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP;

/// Bytes of one `struct epoll_event` **on aarch64**: `uint32_t events`, four bytes of padding,
/// then the 64-bit `epoll_data_t` at offset 8.
///
/// Not x86-64's 12: the kernel header packs the structure only `#ifdef __x86_64__`, and the
/// engine's own code agrees -- it builds one with `stp xzr, x20, [sp]` then `str w8, [sp]`
/// (`0x23cc840`-`0x23cc844`), data at 8, events at 0.
const EPOLL_EVENT_BYTES: usize = 16;

/// `EP_MAX_EVENTS`: `INT_MAX / sizeof(struct epoll_event)`, the kernel's own bound on
/// `maxevents`.
const EP_MAX_EVENTS: i32 = i32::MAX / EPOLL_EVENT_BYTES as i32;

/// How long one pass of an **indefinite** `epoll_wait` may wait before it is renewed.
///
/// Not a timeout the guest sees: [`epoll_wait`] loops until something is ready. It exists because
/// [`wait_until_ready`] takes a deadline, and every individual park inside it is already capped
/// (see [`wait_a_slice`]) and re-checks the stop switch, so the length only decides how often the
/// deadline is renewed.
const INDEFINITE_EPOLL_PASS: Duration = Duration::from_secs(3600);

/// `int epoll_create1(int flags)`
///
/// A new, empty interest list, as a descriptor from the one table (D30). `EPOLL_CLOEXEC` is
/// recorded for `fcntl(F_GETFD)` and otherwise inert -- there is no `exec` for it to act on -- and
/// any other flag is `EINVAL`, which is the kernel's answer.
///
/// MEASURED reader: the engine's transport, `RbxTransport I/O backend chosen: sys`, at
/// `libroblox.so` link `0x23cc788` with flags 0, on the client-settings success path. The guest
/// thread died on the `Unbound` this replaces.
pub(super) fn epoll_create1(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let flags = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if flags & !EPOLL_CLOEXEC != 0 {
            view.set_errno(consts::EINVAL);
            -1
        } else {
            let fs = filesystem(&view)?;
            match settle(&view, fs.epoll_create())? {
                Settled::Done(fd) => {
                    super::files::record_close_on_exec(&view, fs, fd, flags & EPOLL_CLOEXEC != 0)?
                }
                Settled::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int epoll_ctl(int epfd, int op, int fd, struct epoll_event *event)`
///
/// The list's rules are the seam's (`omni_platform::fs::epoll`); what is decided here is the
/// guest's structure and which event bits can be honoured -- see [`EPOLL_REQUESTABLE`].
/// `EPOLL_CTL_DEL` ignores `event`, which may be null, as it may on Linux since 2.6.9.
pub(super) fn epoll_ctl(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (epfd, op, fd, event) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let op = match op {
            1 => Some(EpollOp::Add),
            2 => Some(EpollOp::Delete),
            3 => Some(EpollOp::Modify),
            _ => None,
        };
        match op {
            None => {
                view.set_errno(consts::EINVAL);
                -1
            }
            Some(op) => {
                let member = if op == EpollOp::Delete {
                    EpollMember { events: 0, data: 0 }
                } else {
                    let at = guest_address(&view, event)?;
                    if at == 0 {
                        // Linux: EFAULT. Refused, for the reason `pipe` refuses a null `pipefd`.
                        return Err(view.refusal(
                            "`epoll_ctl` ADD/MOD was given a null `struct epoll_event *`",
                        ));
                    }
                    let bytes = view.mem().read_bytes(
                        at,
                        EPOLL_EVENT_BYTES,
                        Blame::new(view.symbol(), view.address(), 3),
                    )?;
                    let events = u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes"));
                    let data = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
                    let unhonoured = events & !EPOLL_REQUESTABLE;
                    if unhonoured != 0 {
                        return Err(view.refusal(format!(
                            "`epoll_ctl` was asked to watch fd {fd} for {events:#x}, of which \
                             {unhonoured:#x} is a semantics this layer does not implement \
                             (EPOLLET, EPOLLONESHOT, EPOLLRDHUP, EPOLLPRI, EPOLLEXCLUSIVE or \
                             EPOLLWAKEUP). Delivering level-triggered IN/OUT events in their \
                             place would claim a behaviour the caller then builds on"
                        )));
                    }
                    EpollMember { events, data }
                };
                let fs = filesystem(&view)?;
                match settle(&view, fs.epoll_ctl(epfd, op, fd, member))? {
                    Settled::Done(()) => 0,
                    Settled::Failed(errno) => {
                        view.set_errno(errno);
                        -1
                    }
                }
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int epoll_wait(int epfd, struct epoll_event *events, int maxevents, int timeout)`
///
/// **Level-triggered**, over each member's own readiness, on the same wait `poll` uses: the
/// gate for pipes and eventfds, the host's readiness call for sockets, and both in slices when
/// the list mixes them. `EPOLLERR` and `EPOLLHUP` are reported whether or not they were asked
/// for, as the kernel does.
///
/// **`timeout == -1` waits for as long as it takes**, in passes of [`INDEFINITE_EPOLL_PASS`]
/// whose every park is capped and re-checks the stop switch -- which is what makes it different
/// from the unbounded `poll` [`bounded_wait`] refuses: the engine's I/O thread idles here by
/// design (`0x28738a0` passes -1), and it can still be stopped. A list with **nothing that can
/// become ready** is refused instead, because on a device that thread would never wake.
///
/// The output array is admitted whole before anything is waited for, and written once.
pub(super) fn epoll_wait(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (epfd, events, maxevents, timeout) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_i32()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let first = {
        let mut view = enter(c, &state);
        if maxevents <= 0 || maxevents > EP_MAX_EVENTS {
            view.set_errno(consts::EINVAL);
            Some(-1)
        } else {
            let fs = filesystem(&view)?;
            match settle(&view, fs.epoll_members(epfd))? {
                Settled::Failed(errno) => {
                    view.set_errno(errno);
                    Some(-1)
                }
                Settled::Done(_) => {
                    // The whole array, before a byte of it is written or a wait begins.
                    let at = guest_address(&view, events)?;
                    view.mem().checked_ptr(
                        at,
                        maxevents as usize * EPOLL_EVENT_BYTES,
                        true,
                        Blame::new(view.symbol(), view.address(), 1),
                    )?;
                    None
                }
            }
        }
    };
    if let Some(value) = first {
        c.ret().i32(value);
        return Ok(());
    }
    let test = |view: &GuestView<'_>| epoll_events(view, epfd, events, maxevents as usize);
    let (ready, watch) = {
        let view = enter(c, &state);
        test(&view)?
    };
    let value = if ready > 0 || timeout == 0 {
        ready
    } else if timeout < 0 {
        if !watch.can_change() {
            return Err(refuse(
                c,
                format!(
                    "`epoll_wait` on fd {epfd} with no timeout, over an interest list in which \
                     nothing can become ready. On a device this thread would never wake; this \
                     layer has no unbounded wait for a thread that cannot be woken"
                ),
            ));
        }
        loop {
            let found = wait_until_ready(c, &state, INDEFINITE_EPOLL_PASS, test)?;
            if found > 0 {
                break found;
            }
        }
    } else {
        let budget =
            bounded_wait(c, Some(Duration::from_millis(timeout as u64)), watch.can_change())?;
        wait_until_ready(c, &state, budget, test)?
    };
    c.ret().i32(value);
    Ok(())
}

/// One pass over an interest list: every member's readiness, the ready ones written to the
/// guest's array (at most `max`, in one write), and the [`Watch`] a wait needs.
///
/// The list is read **fresh on every pass**: another guest thread may `epoll_ctl` while this one
/// waits, and the kernel reports against the list as it is when the event is collected.
fn epoll_events(
    view: &GuestView<'_>,
    epfd: i32,
    out: u64,
    max: usize,
) -> AbiResult<(i32, Watch)> {
    let fs = filesystem(view)?;
    let members = match fs.epoll_members(epfd) {
        Ok(members) => members,
        // Closed by another thread mid-wait. There is no errno for an `epoll_wait` whose epoll
        // descriptor went away under it, and pretending it timed out would be the invented
        // answer; refused, naming what happened.
        Err(error) => return Err(view.refusal(format!("the epoll descriptor went away: {error}"))),
    };
    let mut watch = Watch::default();
    let mut collected = Vec::with_capacity(max.min(members.len()) * EPOLL_EVENT_BYTES);
    let mut ready = 0i32;
    for (fd, member) in members {
        let readiness = match fs.readiness(fd) {
            Ok(readiness) => readiness,
            // `close` removes a descriptor from every list under the table lock, so a member
            // that is not open can only be one closed between the snapshot and this read.
            Err(error) if error.kind() == Some(FsErrorKind::BadDescriptor) => continue,
            Err(error) => return Err(view.refusal(error.to_string())),
        };
        watch.note(
            fs,
            fd,
            Interest {
                readable: member.events & EPOLLIN != 0,
                writable: member.events & EPOLLOUT != 0,
            },
        );
        let revents = epoll_revents(readiness, member.events);
        if revents != 0 && (ready as usize) < max {
            collected.extend_from_slice(&revents.to_le_bytes());
            collected.extend_from_slice(&[0u8; 4]);
            collected.extend_from_slice(&member.data.to_le_bytes());
            ready += 1;
        }
    }
    if ready > 0 {
        let at = guest_address(view, out)?;
        view.mem().write_bytes(at, &collected, Blame::new(view.symbol(), view.address(), 1))?;
    }
    Ok((ready, watch))
}

/// One member's `events` word for what it asked about, and the two conditions it did not have to.
fn epoll_revents(readiness: Readiness, asked: u32) -> u32 {
    let mut revents = 0u32;
    if readiness.readable {
        revents |= asked & EPOLLIN;
    }
    if readiness.writable {
        revents |= asked & EPOLLOUT;
    }
    if readiness.error {
        revents |= EPOLLERR;
    }
    if readiness.hangup {
        revents |= EPOLLHUP;
    }
    revents
}

// ================================================================== timerfd

/// `CLOCK_MONOTONIC`, the one clock a timerfd here can be on -- see `omni_platform::fs::timerfd`.
const TIMERFD_CLOCK_MONOTONIC: i32 = 1;

/// Bytes of an aarch64 `struct itimerspec`: `it_interval` then `it_value`, each a 16-byte
/// `timespec`. The engine's own layout agrees: it zeroes `[sp, #8]` and stores the value at
/// `[sp, #0x18]`, passing `sp + 8` (`libroblox.so` link `0x23cde60`-`0x23cde9c`).
const ITIMERSPEC_BYTES: usize = 32;

/// `int timerfd_create(int clockid, int flags)`
///
/// **`CLOCK_MONOTONIC` only**, the clock the guest's own `clock_gettime` answers, so a deadline
/// it computes from that clock fires when it meant. `CLOCK_REALTIME` and `CLOCK_BOOTTIME` refuse
/// by name: a wall-clock timer has to follow the wall clock's jumps, and `clocks` already refuses
/// the boot clock. Unknown flags are `EINVAL`, from the seam.
///
/// MEASURED reader: the engine's transport, link `0x23cc718`, `timerfd_create(CLOCK_MONOTONIC,
/// TFD_NONBLOCK)`, one call after `epoll_create1` on the client-settings success path.
pub(super) fn timerfd_create(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (clockid, flags) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if clockid != TIMERFD_CLOCK_MONOTONIC {
            return Err(view.refusal(format!(
                "`timerfd_create` on clock {clockid}. Only CLOCK_MONOTONIC (1) is implemented: it \
                 is the clock the guest's own clock_gettime reads, and a timer on CLOCK_REALTIME \
                 must follow the wall clock's jumps, which nothing here models"
            )));
        }
        let fs = filesystem(&view)?;
        match settle(&view, fs.timerfd_create(flags))? {
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

/// `int timerfd_settime(int fd, int flags, const struct itimerspec *new_value,
/// struct itimerspec *old_value)`
///
/// Relative (`flags` 0) or absolute (`TFD_TIMER_ABSTIME`), both measured: the transport arms its
/// one-shot timer each way (`0x23cdea0`, `0x23cdf5c`). `TFD_TIMER_CANCEL_ON_SET` refuses by name
/// -- it is about wall-clock changes, and there is no wall-clock timer here. A `tv_nsec` outside
/// `[0, 1e9)` or a negative `tv_sec` is `EINVAL`, the kernel's answer; `old_value`, when given, is
/// written with what the timer had left, after the new arming is known to be valid.
pub(super) fn timerfd_settime(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, flags, new_value, old_value) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if flags & !TFD_TIMER_ABSTIME != 0 {
            if flags & 2 != 0 {
                return Err(view.refusal(
                    "`timerfd_settime` with TFD_TIMER_CANCEL_ON_SET, which is about wall-clock \
                     changes; there is no wall-clock timer in this runtime",
                ));
            }
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        let at = guest_address(&view, new_value)?;
        if at == 0 {
            // Linux: EFAULT. Refused, for the reason `pipe` refuses a null `pipefd`.
            return Err(view.refusal("`timerfd_settime` was given a null `new_value`"));
        }
        let raw = view.mem().read_bytes(
            at,
            ITIMERSPEC_BYTES,
            Blame::new(view.symbol(), view.address(), 2),
        )?;
        let field = |offset: usize| {
            i64::from_le_bytes(raw[offset..offset + 8].try_into().expect("eight bytes"))
        };
        let timespec = |seconds: i64, nanos: i64| {
            if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
                None
            } else {
                Some(Duration::new(seconds as u64, nanos as u32))
            }
        };
        match (timespec(field(0), field(8)), timespec(field(16), field(24))) {
            (Some(interval), Some(value)) => {
                let fs = filesystem(&view)?;
                let absolute = flags & TFD_TIMER_ABSTIME != 0;
                match settle(&view, fs.timerfd_settime(fd, absolute, value, interval))? {
                    Settled::Done((remaining, old_interval)) => {
                        if old_value != 0 {
                            let out = guest_address(&view, old_value)?;
                            let mut bytes = [0u8; ITIMERSPEC_BYTES];
                            for (offset, part) in [
                                (0, old_interval.as_secs()),
                                (8, u64::from(old_interval.subsec_nanos())),
                                (16, remaining.as_secs()),
                                (24, u64::from(remaining.subsec_nanos())),
                            ] {
                                bytes[offset..offset + 8].copy_from_slice(&part.to_le_bytes());
                            }
                            view.mem().write_bytes(
                                out,
                                &bytes,
                                Blame::new(view.symbol(), view.address(), 3),
                            )?;
                        }
                        0
                    }
                    Settled::Failed(errno) => {
                        view.set_errno(errno);
                        -1
                    }
                }
            }
            _ => {
                view.set_errno(consts::EINVAL);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== eventfd

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

// ================================================================== the socket surface
//
// Everything from here down is D30's half of this module: the guest's socket symbols over
// `omni_platform::net`, which is the only place in the workspace a socket call is made.

/// `SOCK_STREAM`: TCP. Linux UAPI, and the number the guest passes.
const SOCK_STREAM: i32 = 1;
/// `SOCK_DGRAM`: UDP.
const SOCK_DGRAM: i32 = 2;
/// `SOCK_NONBLOCK`: `socket(2)`'s in-line `O_NONBLOCK`, `0o4000` on arm64 as on x86.
const SOCK_NONBLOCK: i32 = 0o4000;
/// `SOCK_CLOEXEC`: `socket(2)`'s in-line close-on-exec, `0o2000000`.
const SOCK_CLOEXEC: i32 = 0o2_000_000;
/// The two `type` bits `socket(2)` accepts beside the socket kind itself.
const SOCK_FLAGS: i32 = SOCK_NONBLOCK | SOCK_CLOEXEC;

/// `IPPROTO_IP`, which is the zero a caller passes for "the default protocol for this type".
const IPPROTO_IP: i32 = 0;
/// `IPPROTO_TCP`, and `SOL_TCP`: the level `TCP_NODELAY` is set at.
const IPPROTO_TCP: i32 = 6;
/// `IPPROTO_UDP`.
const IPPROTO_UDP: i32 = 17;
/// `IPPROTO_IPV6`, and `SOL_IPV6`: the level `IPV6_V6ONLY` is set at.
const IPPROTO_IPV6: i32 = 41;

/// `SOL_SOCKET`. **Linux's value is 1**; several other systems spell it `0xFFFF`, and this is one
/// of the constants where taking the development host's number would be silently wrong.
const SOL_SOCKET: i32 = 1;

/// `SO_REUSEADDR`.
const SO_REUSEADDR: i32 = 2;
/// Linux's `SO_KEEPALIVE`, which is the guest's numbering and not the host's.
///
/// **MEASURED**: Roblox's own HTTP stack sets it on the settings socket, and until the seam had
/// the option the refusal killed the fetch thread -- the last thing between the run and graphics.
const SO_KEEPALIVE: i32 = 9;
/// `SO_ERROR`: the pending socket error, which is how a non-blocking `connect` reports itself.
const SO_ERROR: i32 = 4;
/// `SO_SNDBUF`.
const SO_SNDBUF: i32 = 7;
/// `SO_RCVBUF`.
const SO_RCVBUF: i32 = 8;
/// `SO_BROADCAST`.
const SO_BROADCAST: i32 = 6;
/// `SO_LINGER`, which takes a `struct linger { int l_onoff; int l_linger; }` -- [`LINGER_BYTES`].
const SO_LINGER: i32 = 13;
/// `sizeof(struct linger)` on arm64: two `int`s. Winsock's is two `u_short`s, which is why the
/// guest's struct is read here and never handed to the host.
const LINGER_BYTES: usize = 8;
/// `SO_RCVTIMEO`. Linux arm64 numbers the `_OLD` form 20, which is what a 64-bit userspace uses.
const SO_RCVTIMEO: i32 = 20;
/// `SO_SNDTIMEO`.
const SO_SNDTIMEO: i32 = 21;
/// `TCP_NODELAY`, at level `IPPROTO_TCP`.
const TCP_NODELAY: i32 = 1;
/// Linux's `TCP_KEEPIDLE`: seconds a connection may be idle before the first keep-alive probe.
///
/// **MEASURED, and it is the number that named this work.** With `SO_KEEPALIVE` implemented, the
/// engine's HTTP stack went straight on to configure the timing and this layer refused:
/// `` `setsockopt` refused: setsockopt(fd 13, option 4 at IPPROTO_TCP) with a 4-byte value ``,
/// which killed guest thread 8 -- the thread that had just logged
/// `settingsUrl: https://clientsettingscdn.roblox.com/v2/settings/application/android`.
///
/// **These three numbers are the guest's and they are not the host's.** `omni_platform::net`
/// takes a named [`SocketOption`] variant, never an option number, precisely so that this
/// disagreement has one place to live:
///
/// | quantity | here (Linux/bionic) | Windows (`windows-sys` 0.61.2) |
/// |---|---|---|
/// | idle before first probe | `TCP_KEEPIDLE` = 4 | `TCP_KEEPALIVE` = 3 |
/// | interval between probes | `TCP_KEEPINTVL` = 5 | `TCP_KEEPINTVL` = 17 |
/// | probes before giving up | `TCP_KEEPCNT` = 6 | `TCP_KEEPCNT` = 16 |
///
/// The overlap is what makes it dangerous rather than merely different: Windows defines
/// `TCP_MAXRT` = 5 at the same level, so handing the guest's `TCP_KEEPINTVL` straight to the host
/// would set the maximum retransmit time, succeed, and leave the probe interval whatever it was.
const TCP_KEEPIDLE: i32 = 4;
/// Linux's `TCP_KEEPINTVL`: seconds between keep-alive probes. See [`TCP_KEEPIDLE`].
const TCP_KEEPINTVL: i32 = 5;
/// Linux's `TCP_KEEPCNT`: unanswered probes before the connection is declared dead. See
/// [`TCP_KEEPIDLE`].
const TCP_KEEPCNT: i32 = 6;
/// `IPV6_V6ONLY`, at level `IPPROTO_IPV6`.
const IPV6_V6ONLY: i32 = 26;
/// Linux's `IP_MTU_DISCOVER`, at level `IPPROTO_IP` (`linux/in.h`).
const IP_MTU_DISCOVER: i32 = 10;
/// Linux's `IPV6_MTU_DISCOVER`, at level `IPPROTO_IPV6` (`linux/in6.h`).
const IPV6_MTU_DISCOVER: i32 = 23;
/// `IP_PMTUDISC_DONT`: never set don't-fragment. The same values serve `IPV6_MTU_DISCOVER`.
const IP_PMTUDISC_DONT: i32 = 0;
/// Linux's `UDP_GRO`, at level `IPPROTO_UDP` (`linux/udp.h`): receive offload. `UDP_SEGMENT` (103),
/// its send-side twin, is **not** accepted as a socket option -- see the arm that answers this
/// one -- but is honoured as `sendmsg`'s control message, see [`sendmsg`].
const UDP_GRO: i32 = 104;
/// Linux's `UDP_SEGMENT` (`linux/udp.h`): a send's GSO segment size, a `__u16`.
const UDP_SEGMENT: i32 = 103;
/// `IP_PMTUDISC_DO`: always set don't-fragment. `WANT` (1), `INTERFACE` (4) and `OMIT` (5) have no
/// Windows mode and are refused -- see `SocketOption::PathMtuDiscovery`.
const IP_PMTUDISC_DO: i32 = 2;
/// `IP_PMTUDISC_PROBE`: set don't-fragment and ignore the path MTU.
const IP_PMTUDISC_PROBE: i32 = 3;

/// `SHUT_RD`, `SHUT_WR`, `SHUT_RDWR`.
const SHUT_RD: i32 = 0;
const SHUT_WR: i32 = 1;
const SHUT_RDWR: i32 = 2;

/// `MSG_DONTWAIT`: this one call does not block, whatever the descriptor's flag says.
const MSG_DONTWAIT: i32 = 0x40;
/// `MSG_NOSIGNAL`: do not raise `SIGPIPE` on a write to a closed connection.
const MSG_NOSIGNAL: i32 = 0x4000;

/// `AI_PASSIVE`: the caller means to `bind` the result rather than `connect` it.
const AI_PASSIVE: i32 = 0x0001;
/// `AI_CANONNAME`: fill `ai_canonname` on the first node.
const AI_CANONNAME: i32 = 0x0002;
/// `AI_NUMERICHOST`: `node` is an address literal and must not be looked up.
const AI_NUMERICHOST: i32 = 0x0004;
/// `AI_NUMERICSERV`: `service` is a port number and must not be looked up.
const AI_NUMERICSERV: i32 = 0x0008;
/// `AI_ALL`, with `AI_V4MAPPED`: return IPv4 addresses as IPv6-mapped ones too.
const AI_ALL: i32 = 0x0100;
/// `AI_V4MAPPED_CFG`: the same, if the host has an IPv6 address configured.
const AI_V4MAPPED_CFG: i32 = 0x0200;
/// `AI_ADDRCONFIG`: return a family only if this host has an address of it configured.
const AI_ADDRCONFIG: i32 = 0x0400;
/// `AI_V4MAPPED`.
const AI_V4MAPPED: i32 = 0x0800;
/// Every `ai_flags` bit bionic's `netdb.h` defines.
const AI_MASK: i32 = AI_PASSIVE
    | AI_CANONNAME
    | AI_NUMERICHOST
    | AI_NUMERICSERV
    | AI_ALL
    | AI_V4MAPPED_CFG
    | AI_ADDRCONFIG
    | AI_V4MAPPED;

/// `EAI_ADDRFAMILY`: the name has no address of the family that was asked for.
const EAI_ADDRFAMILY: i32 = 1;
/// `EAI_AGAIN`: the resolver did not answer. **The one an `EAI_*` caller may retry.**
const EAI_AGAIN: i32 = 2;
/// `EAI_BADFLAGS`: `ai_flags` contains something `netdb.h` does not define.
const EAI_BADFLAGS: i32 = 3;
/// `EAI_FAIL`: a permanent resolver failure that is not "no such name".
const EAI_FAIL: i32 = 4;
/// `EAI_FAMILY`: `ai_family` is a family this layer has no socket for.
const EAI_FAMILY: i32 = 5;
/// `EAI_SOCKTYPE`: `ai_socktype` is one this layer has no socket for.
const EAI_SOCKTYPE: i32 = 10;
/// `EAI_SERVICE`: the service is not one this socket type can use -- "Servname not supported for
/// ai_socktype", row 9 of bionic's `ai_errlist` (`omni_bionic::net::gai_strerror_message`).
const EAI_SERVICE: i32 = 9;

// ---------------------------------------------------------------- the socket errno numbers
//
// **Linux's `asm-generic/errno.h`, which is what arm64 uses, written as literals here for the
// reason `omni_bionic::errno` gives for its own table**: these numbers reach the guest, the
// development host is Windows — where `WSAECONNREFUSED` is 10061 and not 111 — and a wrong one
// makes the guest take the wrong branch rather than making the build fail. They are here rather
// than in `omni_bionic::errno` because that crate's table is "constants reachable by this crate's
// functions", and no function in it can produce a socket failure.

/// `EMSGSIZE`: a datagram larger than the path will carry.
const EMSGSIZE: i32 = 90;
/// `ENOPROTOOPT`: the option is not defined at this level on this socket.
const ENOPROTOOPT: i32 = 92;
/// `ENOTSOCK`: the descriptor is open and is not a socket.
const ENOTSOCK: i32 = 88;
/// `EOPNOTSUPP`: `listen` or `accept` on a socket type that has no connections -- a datagram
/// socket. 95 in the asm-generic numbering, where it is the same number as `ENOTSUP`.
const EOPNOTSUPP: i32 = 95;
/// `EADDRINUSE`.
const EADDRINUSE: i32 = 98;
/// `EADDRNOTAVAIL`.
const EADDRNOTAVAIL: i32 = 99;
/// `ENETUNREACH`.
const ENETUNREACH: i32 = 101;
/// `ECONNABORTED`.
const ECONNABORTED: i32 = 103;
/// `ECONNRESET`: the peer reset the connection. **The one an HTTPS client meets in normal use.**
const ECONNRESET: i32 = 104;
/// `ENOBUFS`.
const ENOBUFS: i32 = 105;
/// `EISCONN`.
const EISCONN: i32 = 106;
/// `ENOTCONN`.
const ENOTCONN: i32 = 107;
/// `ECONNREFUSED`: nothing is listening on the far end.
const ECONNREFUSED: i32 = 111;
/// `EHOSTUNREACH`.
const EHOSTUNREACH: i32 = 113;
/// `EINPROGRESS`: a non-blocking `connect` has started. **The normal answer, not an edge case.**
const EINPROGRESS: i32 = 115;

/// How long one socket wait may sleep before the in-process half of a mixed set is re-tested.
///
/// **Only used when the set is mixed.** A `poll` naming only sockets puts its whole remaining
/// budget into one `omni_platform::net::poll`, which wakes exactly when the kernel says so; a set
/// that also names a pipe or an eventfd cannot, because no single call waits on both a condition
/// variable and a kernel object. So the socket side is given a slice and the loop re-tests the
/// in-process side that often.
///
/// Twenty milliseconds is the bound on how late an in-process wakeup can be delivered in that
/// case, and it is a bound on latency rather than on correctness: the gate generation is still
/// read before the test, so nothing is *lost*, only deferred by at most one slice. Smaller would
/// cost wakeups on a guest that polls a mixed set continuously; larger would show up as input
/// latency, since the glue's command pipe is one of the descriptors in that set.
const MIXED_WAIT_SLICE: Duration = Duration::from_millis(20);

/// The most bytes one socket transfer moves in a single call.
///
/// **A bound on a host allocation the guest chooses the size of**, which is why it exists: `count`
/// is a `size_t` the guest supplies and a `read(fd, buf, SIZE_MAX)` would otherwise ask this layer
/// for a buffer of that size. A short transfer is `read`'s and `write`'s own contract on a socket
/// — TCP delivers what has arrived and takes what fits — so capping one call is conforming rather
/// than a truncation, and every correct caller already loops.
///
/// 64 KiB rather than [`IO_BLOCK`](omni_platform::fs::IO_BLOCK)'s 4 KiB because a TLS record is up
/// to 16 KiB plus framing and the engine carries its own OpenSSL: a cap below one record would
/// turn every record into four calls for nothing.
const SOCKET_IO_BLOCK: usize = 64 * 1024;

/// The cap is at least one TLS record, which is what the reasoning above rests on.
///
/// A **compile-time** assertion rather than a line in a test, because both sides are constants
/// and clippy is right that a test would fold it to `assert!(true)` -- the same lint, on the same
/// ground, that moved [`MAX_GUEST_FILES`](super::MAX_GUEST_FILES)'s ceiling check up beside its
/// constant. What the *test* beside `transfer_length` asserts instead is the function: that a
/// guest-chosen length is capped at this number and not at some smaller one.
const _: () = assert!(SOCKET_IO_BLOCK >= 16 * 1024);

/// Every descriptor this instance can hold fits in one host readiness call.
///
/// A **compile-time** assertion rather than a test, because both sides are constants. It is what
/// makes the mixed wait's socket set unconditionally expressible: `omni_platform::net::poll`
/// refuses a set larger than `MAX_POLL_SOCKETS` — rather than truncating it, which would answer
/// "not ready" about sockets it never looked at — and this says that a guest cannot build one,
/// because it cannot hold more descriptors than that in the first place.
const _: () =
    assert!(omni_platform::fs::MAX_OPEN_FILES <= omni_platform::net::MAX_POLL_SOCKETS);

/// What one socket call produced: a value, or an `errno` the guest is to be told.
///
/// [`super::files::Settled`]'s counterpart for the network seam, and a separate type for the
/// reason the two seams have separate error kinds at all: a socket's failures are not a file's.
enum Netted<T> {
    /// The call succeeded.
    Done(T),
    /// The call failed the way a real device fails, and this is the `errno` to report.
    Failed(i32),
}

/// The guest `errno` for a classified host network failure, or `None` when there is not one.
///
/// **[`NetErrorKind::Other`] deliberately has no errno**, exactly as `FsErrorKind::Other` does not:
/// it is the kind `std::io::ErrorKind` could not classify, and giving it `EIO` would hand guest
/// code a specific, actionable failure for something nobody identified. A refusal naming the
/// symbol and the host's own message is what a reader can act on.
fn net_errno_for(kind: NetErrorKind) -> Option<i32> {
    Some(match kind {
        NetErrorKind::WouldBlock => consts::EAGAIN,
        NetErrorKind::InProgress => EINPROGRESS,
        NetErrorKind::AlreadyConnected => EISCONN,
        NetErrorKind::NotConnected => ENOTCONN,
        NetErrorKind::ConnectionRefused => ECONNREFUSED,
        NetErrorKind::ConnectionReset => ECONNRESET,
        NetErrorKind::ConnectionAborted => ECONNABORTED,
        NetErrorKind::AddressInUse => EADDRINUSE,
        NetErrorKind::AddressNotAvailable => EADDRNOTAVAIL,
        NetErrorKind::NetworkUnreachable => ENETUNREACH,
        NetErrorKind::HostUnreachable => EHOSTUNREACH,
        NetErrorKind::TimedOut => consts::ETIMEDOUT,
        NetErrorKind::BrokenPipe => consts::EPIPE,
        NetErrorKind::PermissionDenied => consts::EACCES,
        NetErrorKind::InvalidInput => consts::EINVAL,
        NetErrorKind::AddressFamilyNotSupported => consts::EAFNOSUPPORT,
        NetErrorKind::MessageSize => EMSGSIZE,
        NetErrorKind::Interrupted => consts::EINTR,
        NetErrorKind::NoBufferSpace => ENOBUFS,
        // `NetErrorKind::Other` and nothing else. Spelled as a wildcard because the enum is
        // `#[non_exhaustive]`, and a kind added upstream without a decision here must refuse by
        // name rather than acquire a plausible errno.
        _ => return None,
    })
}

/// Turn a network seam result into either a value or an `errno`, refusing what cannot be either.
///
/// The one place the `-1`/refusal split is made for the socket calls, so it is a function rather
/// than a rule repeated fifteen times. Every variant but [`NetError::Io`] is a refusal:
///
/// * [`NetError::Policy`] is a **configuration** fact — the embedding did not open this
///   destination — and reporting it as `ENETUNREACH` would hide it in the ordinary noise of a
///   client failing over, which is the argument D30 itself makes;
/// * [`NetError::Unsupported`] means this target's backend was never built;
/// * [`NetError::UnimplementedOption`] and [`NetError::Refused`] are Global Constraint 1 in
///   person: a `setsockopt` answered `0` without taking effect is the defect "no plausible stubs"
///   exists for;
/// * [`NetError::Resolve`] never reaches here — `getaddrinfo` reports `EAI_*`, which is a
///   different numbering with a different `gai_strerror`, and it is handled where it arises.
fn settled<T>(view: &GuestView<'_>, result: platnet::NetResult<T>) -> AbiResult<Netted<T>> {
    match result {
        Ok(value) => Ok(Netted::Done(value)),
        Err(error) => match error.kind().and_then(net_errno_for) {
            Some(errno) => Ok(Netted::Failed(errno)),
            None => Err(view.refusal(error.to_string())),
        },
    }
}

/// The instance's network policy, or a refusal naming the method that would supply one.
///
/// The counterpart of `files::filesystem`, and the same sentence about a different resource. **A
/// default was rejected rather than omitted**: `NetPolicy::closed()` would be a silent version of
/// this refusal, so an embedding that simply forgot would get a guest whose networking failed as
/// though the network were down — which is D30's own trap, one layer up from `EAI_NONAME`.
fn policy(view: &GuestView<'_>) -> AbiResult<Arc<NetPolicy>> {
    view.active.bionic.network_policy().map(Arc::clone).ok_or_else(|| {
        view.refusal(
            "this guest instance has no network policy. Which destinations a guest may reach is \
             set by the embedding with `Bionic::set_network_policy`, the way which host directory \
             it may read is set with `Bionic::set_filesystem_root`, and none has been supplied. \
             There is deliberately no default: `NetPolicy::closed()` would make an embedding that \
             forgot indistinguishable from one that decided, and the guest would report a network \
             outage that this layer had invented (D30 withdrew Global Constraint 8 in favour of a \
             policy, not in favour of an open socket; D6: the APK under test is cheat-injected \
             and the executor is treated as hostile)",
        )
    })
}

/// The socket `fd` names, or the guest's own answer for a descriptor that is not one.
///
/// `EBADF` and `ENOTSOCK` are kept apart because guest code branches on the difference: the first
/// says the descriptor was closed under it, the second says it is holding the wrong kind of
/// object.
fn socket_of(
    view: &GuestView<'_>,
    fs: &Filesystem,
    fd: i32,
) -> AbiResult<Netted<Arc<std::sync::Mutex<Socket>>>> {
    match fs.socket_at(fd) {
        Ok(handle) => Ok(Netted::Done(handle)),
        Err(error) => match error.kind() {
            Some(omni_platform::fs::FsErrorKind::BadDescriptor) => {
                Ok(Netted::Failed(consts::EBADF))
            }
            Some(omni_platform::fs::FsErrorKind::NotASocket) => Ok(Netted::Failed(ENOTSOCK)),
            _ => Err(view.refusal(error.to_string())),
        },
    }
}

/// Lock a socket, taking a poisoned lock over rather than panicking on it.
///
/// A panic in a handler is reachable from guest code (Global Constraint 11), and a poisoned mutex
/// here means an earlier call panicked while holding one socket — which is a defect to report,
/// not a reason to abort the host.
fn locked(handle: &std::sync::Mutex<Socket>) -> std::sync::MutexGuard<'_, Socket> {
    handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether this call is to behave as a non-blocking one.
///
/// `MSG_DONTWAIT` makes a single call non-blocking on a socket that is otherwise blocking. This
/// seam has no per-call flag — `omni_platform::net` takes the socket's own mode — so the flag is
/// honoured only where it asks for something already true, and **refused by name otherwise**
/// rather than ignored: a caller that passed it and was told the call succeeded would believe the
/// call could not have blocked.
fn wants_nonblocking(nonblocking: bool, flags: i32) -> bool {
    nonblocking || flags & MSG_DONTWAIT != 0
}

/// Wait until a blocking socket is ready for `interest`, or say that the cap ran out.
///
/// **The bound is the same one `nanosleep`, `poll` and a blocking pipe transfer name**, and for
/// the same reason: a sleeping thread executes no guest instructions, so D16's step budgets cannot
/// end one. A device would wait for ever here; this layer waits [`MAX_SLEEP_SECONDS`] and then
/// refuses by name, which is what `files::BlockingWait` does for a pipe.
///
/// The socket lock is held for one slice at a time and released between them, so a `poll` on
/// another guest thread — which takes the descriptor table's lock and then this one — is delayed
/// by at most a slice rather than by the whole wait.
fn await_socket(
    handle: &std::sync::Mutex<Socket>,
    interest: Interest,
    deadline: Instant,
) -> AbiResult<bool> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let slice = (deadline - now).min(MIXED_WAIT_SLICE);
        let readiness = {
            let socket = locked(handle);
            let mut entries = [PollEntry::new(&socket, interest)];
            match platnet::poll(&mut entries, slice) {
                Ok(_) => entries[0].readiness(),
                // The host's readiness call failed. Reported as ready so that the transfer below
                // runs and produces the *real* failure with the host's own message, rather than
                // this layer inventing one from a poll it could not make.
                Err(_) => return Ok(true),
            }
        };
        if (interest.readable && readiness.readable)
            || (interest.writable && readiness.writable)
            || readiness.error
            || readiness.hangup
        {
            return Ok(true);
        }
    }
}

/// Whether a socket is ready for `interest` at this instant: one zero-length poll, and a failed
/// poll reported as ready for [`await_socket`]'s reason -- the transfer then produces the host's
/// own failure.
fn ready_now(handle: &std::sync::Mutex<Socket>, interest: Interest) -> bool {
    let socket = locked(handle);
    let mut entries = [PollEntry::new(&socket, interest)];
    match platnet::poll(&mut entries, Duration::ZERO) {
        Ok(_) => {
            let readiness = entries[0].readiness();
            (interest.readable && readiness.readable)
                || (interest.writable && readiness.writable)
                || readiness.error
                || readiness.hangup
        }
        Err(_) => true,
    }
}

/// End a wait that this runtime is tearing down, by name.
///
/// **The other half of lifting the cap on a wait that can end early.** `bounded_wait` now lets
/// `poll` and `select` wait as long as the guest asked when the set contains a socket or a pipe,
/// and a 69-second wait that nothing can interrupt holds teardown for 69 seconds --
/// `Bionic::stop_guest_threads` is read *between run windows*, and a thread asleep in this loop
/// never ends one. MEASURED, on the run that lifted the cap: no guest thread was killed and
/// `join_guest_threads` then timed out with one still running, which is the same gap
/// `AddressFutex::stop` and `Conds::stop` were each added to close, arriving a third time in a
/// third waiting primitive.
///
/// **Checked between slices, so it costs one relaxed load per slice and nothing while waiting.**
/// It is the same shape `ALooper_pollOnce` already uses and is refused for the same reason it
/// gives: the wait did not expire and nothing became ready, so `0` would report a timeout that
/// did not happen and `-1`/`EINTR` would name a signal this runtime has no delivery for.
fn stop_requested(c: &ImportCall<'_, '_>, state: &Active) -> AbiResult<()> {
    if !state.bionic.guest_threads_stopping() {
        return Ok(());
    }
    Err(refuse(
        c,
        format!(
            "`{}` was waiting on a descriptor set when this runtime asked its guest threads to \
             stop. The wait did not expire and nothing became ready, so there is no value to \
             return that would be true: 0 would report a timeout that did not happen, and \
             -1/EINTR would name a signal that was never delivered because this runtime has no \
             signal delivery",
            c.symbol()
        ),
    ))
}

/// The refusal a blocking socket call produces when it reaches the cap.
fn waited_out(view: &GuestView<'_>, fd: i32) -> AbiError {
    view.refusal(format!(
        "a blocking `{}` on socket fd {fd} waited {MAX_SLEEP_SECONDS} seconds and the socket \
         never became ready. This layer caps a guest-chosen wait at that -- the same cap \
         `nanosleep`, `poll` and a blocking pipe transfer name, and for the same reason: a \
         sleeping thread executes no guest instructions, so no step budget can end one. Returning \
         EAGAIN or a short count instead would report to a blocking socket something only a \
         non-blocking one can be told",
        view.symbol()
    ))
}

/// Read a guest `sockaddr` argument into the seam's structured form.
///
/// The `socklen_t` is the guest's own and is checked against the family **the guest named**, not
/// against what this layer would like: a `connect` with `sizeof(struct sockaddr_in)` on an
/// `AF_INET6` address is `EINVAL` on a device, and reading the sixteen IPv6 bytes anyway would
/// connect somewhere the caller never described.
fn read_sockaddr(
    view: &GuestView<'_>,
    pointer: u64,
    len: i32,
    argument: usize,
) -> AbiResult<Netted<SocketAddress>> {
    if pointer == 0 {
        // POSIX: `EFAULT`. A refusal rather than `-1`, for `files::path_for`'s stated reason --
        // guest code that ignored the return would carry an unconnected socket forward with
        // nothing to say what happened.
        return Err(view.refusal(format!("argument {argument} is a null `sockaddr` pointer")));
    }
    let Ok(given) = usize::try_from(len) else {
        return Ok(Netted::Failed(consts::EINVAL));
    };
    if given < addrinfo::SOCKADDR_IN_BYTES {
        // Shorter than the smallest address this layer can read, so the family cannot even be
        // established. `EINVAL` is what a device answers.
        return Ok(Netted::Failed(consts::EINVAL));
    }
    let at = guest_address(view, pointer)?;
    // Read the whole slot rather than the guest's length: the guest may legitimately pass a
    // `sockaddr_storage` with a shorter `socklen_t`, and reading a fixed size keeps this to one
    // access. It is bounded above by `SOCKADDR_SLOT_BYTES`, which is 32 bytes.
    let want = given.min(addrinfo::SOCKADDR_SLOT_BYTES);
    let mut bytes = [0u8; addrinfo::SOCKADDR_SLOT_BYTES];
    let read =
        view.mem().read_bytes(at, want, Blame::new(view.symbol(), view.address(), argument))?;
    bytes[..read.len()].copy_from_slice(&read);
    match addrinfo::decode_sockaddr(&bytes, given) {
        Ok(address) => Ok(Netted::Done(address)),
        Err(addrinfo::SockaddrError::TooShort { .. }) => Ok(Netted::Failed(consts::EINVAL)),
        Err(addrinfo::SockaddrError::Family(family)) => {
            // A family this layer has no socket for. `EAFNOSUPPORT` is a device's own answer and
            // every caller that tries more than one family has a branch for it.
            let _ = family;
            Ok(Netted::Failed(consts::EAFNOSUPPORT))
        }
    }
}

// ================================================================== socket, connect, bind

/// `int socket(int domain, int type, int protocol)`
///
/// **Answered from M6, where it had been refused by name since M3**, and the refusal's own
/// reasoning is corrected here rather than deleted. It said: "Omnidroid gives the guest no
/// network: `omni-platform` has no socket seam, and an embedding has no way to say which network
/// a guest may reach, the way `Bionic::set_filesystem_root` says which directory it may reach."
/// Both halves were true and both have been built. D30 withdrew Global Constraint 8 because
/// playable Roblox is on the far side of a settings fetch, and what replaced the refusal is the
/// policy that argument asked for — [`super::Bionic::set_network_policy`], which an instance
/// **must** have before a socket exists at all.
///
/// The part of the old reasoning that survives unchanged is D6's threat: the APK under test is
/// cheat-injected and carries a Luau executor, so a descriptor handed out here is held by code
/// treated as hostile. That is why the policy is consulted for every destination rather than once
/// at creation, and why the default is no socket rather than a closed one.
///
/// # The `AF_INET6` exception is now the ordinary case
///
/// The refusal carried one narrow exception, decoded at guest `0x021ed7bc`: a `socket(AF_INET6,
/// SOCK_DGRAM, 0)` whose result is never used to send anything — the engine creates the descriptor
/// **to ask whether the family exists**, clears a capability bit, and sets it again only on
/// success. It was answered `-1`/`EAFNOSUPPORT` because that told the engine the truth (this
/// runtime had no IPv6) while granting nothing.
///
/// That is no longer the truth, so it is no longer the answer. An `AF_INET6` socket is created for
/// real; if this host has no IPv6, the host's own `socket(2)` fails and the guest gets
/// `EAFNOSUPPORT` from the machine rather than from this layer's opinion of it. The capability
/// probe still works, and it now reports a fact.
///
/// # What is refused, and why each is not `-1`
///
/// `AF_UNIX` and `AF_NETLINK` have no primitive in `omni_platform::net` and nothing has reached
/// them; `SOCK_RAW` and `SOCK_SEQPACKET` likewise. Each is refused **by name** rather than
/// answered `-1`/`EAFNOSUPPORT`, because a networked program branches on that quietly: it would
/// switch off the feature that needed the socket and nothing anywhere would record that this
/// layer, rather than the device, had decided.
pub(super) fn socket(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (domain, kind, protocol) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let family = match domain {
            addrinfo::AF_INET => IpFamily::V4,
            addrinfo::AF_INET6 => IpFamily::V6,
            other => {
                let named = match other {
                    1 => "AF_UNIX",
                    16 => "AF_NETLINK",
                    _ => "an address family this layer has no name for",
                };
                return Err(refuse(
                    c,
                    format!(
                        "the guest called socket({domain}, {kind}, {protocol}) -- {named}. \
                         `omni_platform::net` is an IP client: it makes AF_INET and AF_INET6 \
                         stream and datagram sockets and nothing else, because that is what a \
                         measured run has reached (D17: importing is not calling). Answering -1 \
                         with EAFNOSUPPORT was rejected -- it is a legitimate POSIX answer a \
                         networked program branches on quietly, so the engine would switch off \
                         whatever needed this socket and nothing would record that this layer, \
                         rather than the device, had decided"
                    ),
                ));
            }
        };
        let flags = kind & SOCK_FLAGS;
        let socket_kind = match kind & !SOCK_FLAGS {
            SOCK_STREAM => SocketKind::Stream,
            SOCK_DGRAM => SocketKind::Datagram,
            other => {
                let named = match other {
                    3 => "SOCK_RAW",
                    5 => "SOCK_SEQPACKET",
                    _ => "a socket type this layer has no name for",
                };
                return Err(refuse(
                    c,
                    format!(
                        "the guest called socket({domain}, {kind}, {protocol}) -- type {other} is \
                         {named}. This seam implements SOCK_STREAM and SOCK_DGRAM, which is TCP \
                         for HTTPS and UDP for the game protocol; nothing has reached anything \
                         else and `omni_platform::net::SocketKind` has no variant for one"
                    ),
                ));
            }
        };
        // A protocol that contradicts the type is the guest asking for something that does not
        // exist. Linux answers `EPROTONOSUPPORT`; this refuses by name, because the combination
        // is a *programming* error rather than a device's capability and a quiet -1 would hide it.
        let protocol_matches = matches!(
            (socket_kind, protocol),
            (_, IPPROTO_IP)
                | (SocketKind::Stream, IPPROTO_TCP)
                | (SocketKind::Datagram, IPPROTO_UDP)
        );
        if !protocol_matches {
            return Err(refuse(
                c,
                format!(
                    "the guest called socket({domain}, {kind}, {protocol}): protocol {protocol} \
                     is not the protocol of {}. This seam takes 0 (the type's default), \
                     IPPROTO_TCP ({IPPROTO_TCP}) on a stream socket and IPPROTO_UDP \
                     ({IPPROTO_UDP}) on a datagram one",
                    socket_kind.as_str()
                ),
            ));
        }

        let policy = policy(&view)?;
        let fs = filesystem(&view)?;
        let mut socket = match settled(&view, Socket::new(socket_kind, family, policy))? {
            Netted::Done(socket) => socket,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        // `SOCK_NONBLOCK` is honoured before the descriptor exists, which is the whole point of
        // its being a `socket(2)` flag rather than a later `fcntl`: there is no window in which
        // the socket is blocking. `SOCK_CLOEXEC` is recorded on the descriptor below and is
        // otherwise inert -- there is no `exec` in this runtime -- and `fcntl(F_GETFD)` reports it.
        if flags & SOCK_NONBLOCK != 0 {
            if let Netted::Failed(errno) = settled(&view, socket.set_nonblocking(true))? {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        }
        match settle(&view, fs.attach_socket(socket))? {
            Settled::Done(fd) => {
                super::files::record_close_on_exec(&view, fs, fd, flags & SOCK_CLOEXEC != 0)?
            }
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int connect(int sockfd, const struct sockaddr *addr, socklen_t addrlen)`
///
/// **`EINPROGRESS` is the normal answer on a non-blocking socket and not an edge case**, and it is
/// the one this whole call is shaped around: the engine will not park a thread on a connect, so it
/// sets `O_NONBLOCK`, calls this, gets `-1`/`EINPROGRESS`, waits for writability with `poll`, and
/// reads `SO_ERROR`. Every step of that is implemented here and in `getsockopt`.
///
/// On a **blocking** socket the wait is done here instead, bounded by [`MAX_SLEEP_SECONDS`] for
/// D16's reason, and the answer is `0` or the connect's own errno — which is what a device gives.
///
/// The policy is consulted **before any packet leaves the machine**, inside
/// `omni_platform::net::Socket::connect`, and a destination outside it is a refusal naming the
/// rule rather than `ENETUNREACH`: which network an instance may reach is a configuration fact,
/// and reporting one as a routing failure would hide it in the ordinary noise of a client failing
/// over.
pub(super) fn connect(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, addr, addrlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let address = match read_sockaddr(&view, addr, addrlen, 1)? {
            Netted::Done(address) => address,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let (progress, nonblocking) = {
            let mut socket = locked(&handle);
            let nonblocking = socket.nonblocking();
            match settled(&view, socket.connect(&address))? {
                Netted::Done(progress) => (progress, nonblocking),
                Netted::Failed(errno) => {
                    view.set_errno(errno);
                    c.ret().i32(-1);
                    return Ok(());
                }
            }
        };
        match progress {
            ConnectProgress::Connected => 0,
            ConnectProgress::InProgress if nonblocking => {
                view.set_errno(EINPROGRESS);
                -1
            }
            ConnectProgress::InProgress => {
                // A blocking connect finishes here rather than in the guest. The socket is
                // writable-or-in-error when the handshake settles, which is the only correct
                // thing to wait for: `SO_ERROR` reads zero while it is still in flight, so
                // reading it alone cannot tell success from *not yet*.
                let deadline = Instant::now() + Duration::from_secs(MAX_SLEEP_SECONDS);
                if !await_socket(&handle, Interest::WRITABLE, deadline)? {
                    return Err(waited_out(&view, fd));
                }
                match settled(&view, locked(&handle).connect_result())? {
                    Netted::Done(ConnectOutcome::Connected) => 0,
                    Netted::Done(ConnectOutcome::Failed(kind)) => {
                        match net_errno_for(kind) {
                            Some(errno) => {
                                view.set_errno(errno);
                                -1
                            }
                            None => {
                                return Err(view.refusal(format!(
                                    "a blocking connect to {address} failed with a host error \
                                     this layer has no errno for ({kind}). Reporting a specific \
                                     errno for a failure nobody classified would hand guest code \
                                     an actionable branch for something nobody identified"
                                )))
                            }
                        }
                    }
                    // The wait said the socket had settled and `SO_ERROR` says otherwise. That is
                    // not a state this layer can describe, so it says so rather than reporting a
                    // connection it has no evidence for.
                    Netted::Done(other) => {
                        return Err(view.refusal(format!(
                            "a blocking connect to {address} was reported ready by the host's \
                             readiness call and then answered {other:?}, which is a socket that \
                             is neither connected nor failed. Returning 0 here would be a guess"
                        )))
                    }
                    Netted::Failed(errno) => {
                        view.set_errno(errno);
                        -1
                    }
                }
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int bind(int sockfd, const struct sockaddr *addr, socklen_t addrlen)`
///
/// **Not policy-checked, and that is the seam's decision rather than an omission here.**
/// `NetPolicy` is a *destination* policy and a local address is not a destination; what a bind
/// does open is an inbound path, which is a different question -- asked at [`listen`], where a
/// stream socket's inbound path actually opens.
pub(super) fn bind(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, addr, addrlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let address = match read_sockaddr(&view, addr, addrlen, 1)? {
            Netted::Done(address) => address,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let outcome = settled(&view, locked(&handle).bind(&address))?;
        match outcome {
            Netted::Done(()) => 0,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int getsockname(int sockfd, struct sockaddr *addr, socklen_t *addrlen)`
///
/// **MEASURED, and it is the call after the keep-alive options.** With `TCP_KEEPIDLE`,
/// `TCP_KEEPINTVL` and `TCP_KEEPCNT` implemented, the settings-fetch thread went straight on to
/// `getsockname` and died on it as an `Unbound`: `GuestThreadFailure { thread: 8, why: "the guest
/// called the imported symbol `getsockname` through its thunk at 0x12d070352d0, and nothing in
/// the compatibility layer implements it" }`. A connected client asks it for the local end it was
/// given -- which is the ephemeral port the host chose -- and OpenSSL's BIO layer and every HTTP
/// stack that logs a connection want it.
///
/// **The whole of this is [`write_peer`]**, which `recvfrom` already needed and which already
/// implements the one rule that is easy to get wrong: a short `addrlen` **truncates the address
/// and reports the full length**, so the caller learns it was not given enough room by comparing
/// what it passed with what came back. Getting that backwards -- writing the truncated length --
/// would tell a caller its buffer had been big enough.
///
/// `getpeername` is **deliberately not bound beside it**, although
/// [`Socket::peer_address`](omni_platform::net::Socket::peer_address) is implemented and one line
/// away. D17's rule is the whole of the reason: `libroblox.so` imports it and no run has called
/// it, so it stays `Binding::Unbound` and the first run that reaches it will say so by name --
/// which is how this symbol was found.
pub(super) fn getsockname(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, addr, addrlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        if addr == 0 || addrlen == 0 {
            // The same answer `getsockopt` in this file gives a null out-parameter, and for the
            // same reason: there is nowhere to put the result. A device answers `EFAULT` here,
            // `omni-bionic`'s errno table carries no `EFAULT`, and inventing the number would be
            // a constant this layer had not derived from anything -- so the call fails, by a
            // number that is also a failure, rather than succeeding while writing nothing.
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        let outcome = settled(&view, locked(&handle).local_address())?;
        match outcome {
            Netted::Done(address) => {
                write_peer(&view, addr, addrlen, &address)?;
                0
            }
            Netted::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int listen(int sockfd, int backlog)`
///
/// **MEASURED (the owner's session, 2026-09-23):** the engine's MicroProfiler web server -- a
/// thread of its own, start routine link `0x61f1580` -- makes a blocking `socket(AF_INET,
/// SOCK_STREAM, IPPROTO_TCP)`, binds `0.0.0.0` on the first free port of 1338-1358, logs "Web
/// server started on port N", calls `listen(fd, 8)` without reading its result (`0x61f0eec`), and
/// then loops on `accept(fd, NULL, NULL)` (`0x61f0efc`, `0x61f12ac`). It died on this symbol
/// unbound, a TaskScheduler worker died beside it, and the game froze.
///
/// The policy is asked here, with the address the socket is bound to
/// ([`NetPolicy::check_listen`]): `listen` is what turns a bound stream socket into an inbound
/// path. A refusal names the rule, for [`settled`]'s reason. On a datagram socket the answer is
/// Linux's `EOPNOTSUPP`.
pub(super) fn listen(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, backlog) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let mut socket = locked(&handle);
        if socket.kind() == SocketKind::Stream {
            match settled(&view, socket.listen(backlog))? {
                Netted::Done(()) => 0,
                Netted::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            }
        } else {
            view.set_errno(EOPNOTSUPP);
            -1
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int accept(int sockfd, struct sockaddr *addr, socklen_t *addrlen)`
///
/// The connection comes back as a new descriptor that is **blocking and not close-on-exec**,
/// whatever the listener is (accept(2): those flags are not inherited; `accept4` is the call that
/// sets them, and it is not bound -- the MicroProfiler, the one reader, calls this). The peer is
/// written through `addr`/`addrlen` by [`write_peer`]'s rule; a null `addr` asks for nothing.
///
/// # A blocking accept waits for a connection, however long that is
///
/// That is what `accept` is for: the MicroProfiler's thread sits here until a browser connects,
/// which on a device may be never. So this is **not** capped at [`MAX_SLEEP_SECONDS`] as a
/// guest-chosen sleep is -- a refusal after a minute would kill a thread that is behaving
/// correctly. What the cap exists to prevent, a host thread nothing can end, is prevented as
/// `poll` and `select` prevent it for a socket: the wait re-tests every [`MIXED_WAIT_SLICE`], ends
/// the moment a connection is pending, and ends by name when the runtime stops its guest threads
/// ([`stop_requested`]). A receive timeout (`SO_RCVTIMEO`) bounds it as Linux's `accept` honours
/// one: `EAGAIN` when it runs out.
///
/// The host's own `accept` is made only when a connection is pending, **under the socket's lock
/// with a zero-length poll just before it**, so a second guest thread that took the connection
/// first leaves this one waiting rather than blocked inside the host call holding the lock.
pub(super) fn accept(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, addr, addrlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let (kind, timeout) = {
            let mut socket = locked(&handle);
            let timeout = match socket.get_option(SocketQuery::ReceiveTimeout) {
                Ok(OptionValue::Timeout(timeout)) => timeout,
                _ => None,
            };
            (socket.kind(), timeout)
        };
        if kind != SocketKind::Stream {
            view.set_errno(EOPNOTSUPP);
            c.ret().i32(-1);
            return Ok(());
        }
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let accepted = loop {
            {
                let socket = locked(&handle);
                // Not listening: the host's `accept` answers `EINVAL` at once, as Linux's does,
                // and waiting for a readability that cannot come would hang on the mistake.
                if socket.nonblocking() || !socket.listening() || readable_now(&socket) {
                    break Some(socket.accept());
                }
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break None;
            }
            stop_requested(c, &state)?;
            let slice = deadline.map_or(MIXED_WAIT_SLICE, |deadline| {
                deadline.saturating_duration_since(Instant::now()).min(MIXED_WAIT_SLICE)
            });
            wait_readable_for(&handle, slice);
        };
        let Some(accepted) = accepted else {
            // `SO_RCVTIMEO` ran out with nothing pending: Linux's accept answers `EAGAIN`.
            view.set_errno(consts::EAGAIN);
            c.ret().i32(-1);
            return Ok(());
        };
        match settled(&view, accepted)? {
            Netted::Done((socket, peer)) => match settle(&view, fs.attach_socket(socket))? {
                Settled::Done(new_fd) => {
                    super::files::record_close_on_exec(&view, fs, new_fd, false)?;
                    write_peer(&view, addr, addrlen, &peer)?;
                    new_fd
                }
                Settled::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            },
            Netted::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// Whether a locked socket is readable this instant: a zero-length poll, and a failed poll read
/// as readable for [`await_socket`]'s reason -- the call that follows produces the host's own
/// failure.
fn readable_now(socket: &Socket) -> bool {
    let mut entries = [PollEntry::new(socket, Interest::READABLE)];
    match platnet::poll(&mut entries, Duration::ZERO) {
        Ok(_) => {
            let readiness = entries[0].readiness();
            readiness.readable || readiness.error || readiness.hangup
        }
        Err(_) => true,
    }
}

/// Sleep until the socket is readable or `slice` has passed, holding its lock for the one poll.
fn wait_readable_for(handle: &std::sync::Mutex<Socket>, slice: Duration) {
    let socket = locked(handle);
    let mut entries = [PollEntry::new(&socket, Interest::READABLE)];
    let _ = platnet::poll(&mut entries, slice);
}

/// `int shutdown(int sockfd, int how)`
pub(super) fn shutdown(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, how) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let direction = match how {
            SHUT_RD => Shutdown::Read,
            SHUT_WR => Shutdown::Write,
            SHUT_RDWR => Shutdown::Both,
            // Linux's own answer for a `how` outside the three.
            _ => {
                view.set_errno(consts::EINVAL);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        let outcome = settled(&view, locked(&handle).shutdown(direction))?;
        match outcome {
            Netted::Done(()) => 0,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== the socket options

/// The guest's `(level, optname)` as a name, for a refusal that has to say what was asked for.
fn option_name(level: i32, name: i32) -> String {
    let level_name = match level {
        SOL_SOCKET => "SOL_SOCKET".to_owned(),
        IPPROTO_TCP => "IPPROTO_TCP".to_owned(),
        IPPROTO_IPV6 => "IPPROTO_IPV6".to_owned(),
        IPPROTO_IP => "IPPROTO_IP".to_owned(),
        IPPROTO_UDP => "IPPROTO_UDP".to_owned(),
        other => format!("level {other}"),
    };
    let option = match (level, name) {
        (SOL_SOCKET, SO_REUSEADDR) => "SO_REUSEADDR".to_owned(),
        (SOL_SOCKET, SO_KEEPALIVE) => "SO_KEEPALIVE".to_owned(),
        (SOL_SOCKET, SO_ERROR) => "SO_ERROR".to_owned(),
        (SOL_SOCKET, SO_SNDBUF) => "SO_SNDBUF".to_owned(),
        (SOL_SOCKET, SO_RCVBUF) => "SO_RCVBUF".to_owned(),
        (SOL_SOCKET, SO_RCVTIMEO) => "SO_RCVTIMEO".to_owned(),
        (SOL_SOCKET, SO_SNDTIMEO) => "SO_SNDTIMEO".to_owned(),
        (SOL_SOCKET, SO_BROADCAST) => "SO_BROADCAST".to_owned(),
        (SOL_SOCKET, SO_LINGER) => "SO_LINGER".to_owned(),
        (IPPROTO_TCP, TCP_NODELAY) => "TCP_NODELAY".to_owned(),
        (IPPROTO_TCP, TCP_KEEPIDLE) => "TCP_KEEPIDLE".to_owned(),
        (IPPROTO_TCP, TCP_KEEPINTVL) => "TCP_KEEPINTVL".to_owned(),
        (IPPROTO_TCP, TCP_KEEPCNT) => "TCP_KEEPCNT".to_owned(),
        (IPPROTO_IPV6, IPV6_V6ONLY) => "IPV6_V6ONLY".to_owned(),
        (IPPROTO_IP, IP_MTU_DISCOVER) => "IP_MTU_DISCOVER".to_owned(),
        (IPPROTO_UDP, UDP_GRO) => "UDP_GRO".to_owned(),
        (IPPROTO_UDP, UDP_SEGMENT) => "UDP_SEGMENT".to_owned(),
        (IPPROTO_IPV6, IPV6_MTU_DISCOVER) => "IPV6_MTU_DISCOVER".to_owned(),
        (_, other) => format!("option {other}"),
    };
    format!("{option} at {level_name}")
}

/// A guest `int` of seconds as the [`Duration`] `omni_platform::net` takes, or `None` for a value
/// no keep-alive option accepts.
///
/// `None` is what the caller turns into `EINVAL`, and the two ways to reach it are the two a
/// device refuses: a negative number, which is not a count of seconds at all, and zero, which
/// Linux's `do_tcp_setsockopt` rejects for every one of `TCP_KEEPIDLE`, `TCP_KEEPINTVL` and
/// `TCP_KEEPCNT`. **Zero is the one worth the check.** It is a perfectly valid `u32`, it survives
/// every conversion between here and the host, and it would arrive at `setsockopt` meaning
/// "probe with no idle time" -- a socket that works, configured to give up on itself.
///
/// # The zero check is **not** what produces `EINVAL` on either supported host, and that is
/// measured rather than assumed
///
/// `sockcfg-A3` in `tools/mutate.py` removes the `> 0` and is **NOT CAUGHT** -- deliberately
/// left in the table as a miss rather than retargeted at something that would pass. The reason
/// is the honest one: **Winsock refuses a zero keep-alive figure itself**, with an error this
/// seam maps to `EINVAL`, so on this host the guest gets 22 either way and no test that runs
/// here can tell the two apart. Linux's `do_tcp_setsockopt` range-checks all three the same way,
/// so the same is true of the other supported target once its backend exists.
///
/// So what is this check for? **A host whose stack accepts zero.** There is no such host in the
/// five this project targets, which is exactly why the branch reads as redundant and why
/// `VERIFICATION.md` entry 12 is worth holding it against: *a branch no input can take is not a
/// check*. It survives that test -- every input can take it, and it fires before the host is
/// asked -- but it is honest to say that on the only target anybody runs, the host is what
/// produces the answer. **What would make it load-bearing**: a backend on a stack that took a
/// zero idle time literally, at which point this is the line that stops a socket being
/// configured to probe with no idle time at all.
///
/// A separate function rather than an inline `filter` so that the row above has somewhere to
/// anchor, and so that this paragraph has somewhere to live.
fn positive_seconds(value: i32) -> Option<Duration> {
    u32::try_from(value).ok().filter(|seconds| *seconds > 0).map(|seconds| Duration::from_secs(u64::from(seconds)))
}

/// Read a guest `struct timeval` as a duration, or `None` for the zero that means "no timeout".
fn read_timeval(view: &GuestView<'_>, at: GuestAddr) -> AbiResult<Option<Duration>> {
    let raw = view.mem().read_bytes(at, TIMEVAL_BYTES, Blame::new(view.symbol(), view.address(), 3))?;
    let seconds = i64::from_le_bytes(raw[..8].try_into().expect("eight bytes"));
    let micros = i64::from_le_bytes(raw[8..].try_into().expect("eight bytes"));
    if seconds <= 0 && micros <= 0 {
        // A zero `timeval` clears the timeout, which is what a device does and what
        // `omni_platform::net` spells `None`. A negative one is not a duration; Linux answers
        // `EINVAL`, and `None` here means the same thing to the seam, so it is reported as the
        // clear rather than as a refusal -- see `setsockopt`, which validates it before this.
        return Ok(None);
    }
    Ok(Some(
        Duration::from_secs(seconds.max(0) as u64) + Duration::from_micros(micros.max(0) as u64),
    ))
}

/// Write a duration back as a guest `struct timeval`.
fn timeval_bytes(timeout: Option<Duration>) -> [u8; TIMEVAL_BYTES] {
    let mut out = [0u8; TIMEVAL_BYTES];
    if let Some(duration) = timeout {
        out[..8].copy_from_slice(&(duration.as_secs() as i64).to_le_bytes());
        out[8..].copy_from_slice(&i64::from(duration.subsec_micros()).to_le_bytes());
    }
    out
}

/// `int setsockopt(int sockfd, int level, int optname, const void *optval, socklen_t optlen)`
///
/// **The option set is closed and an option outside it is refused by name**, carrying the level
/// and the number the guest passed. That is Global Constraint 1 in person: a `setsockopt` that
/// returns `0` without taking effect leaves the caller believing the option is in force and
/// behaving as though it were, and the failure surfaces somewhere else entirely.
/// `omni_platform::net::NetError::unimplemented_option` exists for exactly this and is what words
/// the refusal, so the message also says which options *are* available — which is what the next
/// person needs at the moment they discover the one they wanted is not.
///
/// An option that exists and is not defined on **this** socket — `TCP_NODELAY` on a datagram one,
/// `IPV6_V6ONLY` on an IPv4 one — is a different answer: `ENOPROTOOPT`, which is what a device
/// gives and which the seam reports as a refusal this layer turns into that errno.
pub(super) fn setsockopt(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, level, name, optval, optlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_i32()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        if optval == 0 {
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        let at = guest_address(&view, optval)?;
        let given = usize::try_from(optlen).unwrap_or(0);
        // Every option here is either an `int` or a `struct timeval`, and a buffer shorter than
        // the one the option needs is `EINVAL` on a device. Checked against the option rather
        // than accepted and padded: a caller that passed two bytes for an `int` has a bug this
        // layer must not paper over.
        let int_option = |needed: usize| -> AbiResult<Option<i32>> {
            if given < needed {
                return Ok(None);
            }
            Ok(Some(view.mem().read_i32(at, Blame::new(view.symbol(), view.address(), 3))?))
        };
        let option = match (level, name) {
            (SOL_SOCKET, SO_REUSEADDR) => int_option(4)?.map(|on| SocketOption::ReuseAddress(on != 0)),
            // MEASURED reader of both: the engine's game-socket setup (`0x502aa70`), which on its
            // UDP socket sets FD_CLOEXEC, SO_RCVBUF, SO_LINGER off, SO_SNDBUF, IPV6_V6ONLY on an
            // IPv6 one, then SO_BROADCAST on; its worker died on the SO_LINGER refusal
            // (2026-09-23). Each is accepted on every socket kind, as Linux accepts it; see
            // `SocketOption::Linger`/`Broadcast` for the kind the host refuses it on.
            (SOL_SOCKET, SO_BROADCAST) => int_option(4)?.map(|on| SocketOption::Broadcast(on != 0)),
            (SOL_SOCKET, SO_LINGER) => {
                if given < LINGER_BYTES {
                    None
                } else {
                    let raw = view.mem().read_bytes(at, LINGER_BYTES, Blame::new(view.symbol(), view.address(), 3))?;
                    let on = i32::from_le_bytes(raw[..4].try_into().expect("four bytes")) != 0;
                    let seconds = i32::from_le_bytes(raw[4..].try_into().expect("four bytes"));
                    if !on {
                        // Off: Linux ignores `l_linger` then.
                        Some(SocketOption::Linger(None))
                    } else if seconds != 0
                        && locked(&handle).kind() == omni_platform::net::SocketKind::Stream
                    {
                        return Err(view.refusal(format!(
                            "the guest called setsockopt(fd {fd}, SO_LINGER at SOL_SOCKET) on a \
                             stream socket with l_onoff {on} and l_linger {seconds}: a close that \
                             blocks until the unsent data is gone or {seconds} s are up, even on a \
                             non-blocking socket. Winsock's closesocket on a non-blocking socket \
                             fails WSAEWOULDBLOCK instead and leaves the socket open, and this \
                             layer's close cannot retry it. Off, and on with l_linger 0 (the \
                             abortive close), are implemented",
                            on = i32::from(on)
                        )));
                    } else {
                        // A negative `l_linger` is Linux's "forever": it widens the `int` to an
                        // `unsigned long` and caps it at the scheduler's largest timeout.
                        Some(SocketOption::Linger(Some(
                            u64::try_from(seconds).map_or(Duration::MAX, Duration::from_secs),
                        )))
                    }
                }
            }
            // The boolean only. `omni_platform::net::SocketOption::KeepAlive` documents what is
            // deliberately not carried with it -- the idle interval, which has no portable
            // spelling and which this layer therefore does not pretend to set.
            (SOL_SOCKET, SO_KEEPALIVE) => int_option(4)?.map(|on| SocketOption::KeepAlive(on != 0)),
            (IPPROTO_TCP, TCP_NODELAY) => int_option(4)?.map(|on| SocketOption::NoDelay(on != 0)),
            // **The keep-alive timing, which is where the guest's numbering and the host's stop
            // agreeing.** See [`TCP_KEEPIDLE`] for the table; the point of naming a variant here
            // rather than forwarding `(level, name)` is that `omni_platform::net` has no way to
            // accept an option number, so the disagreement cannot be forwarded by accident.
            //
            // **A value below one is `EINVAL`, and it is not a formality.** Linux range-checks
            // all three in `do_tcp_setsockopt` and refuses zero; a zero accepted here would reach
            // the host as "probe immediately" or "give up after no probes", which is a working
            // socket configured to tear itself down -- the failure would be a dropped connection
            // minutes later and nothing would point back to this call.
            (IPPROTO_TCP, TCP_KEEPIDLE) => int_option(4)?.and_then(|seconds| {
                positive_seconds(seconds).map(SocketOption::KeepAliveIdle)
            }),
            (IPPROTO_TCP, TCP_KEEPINTVL) => int_option(4)?.and_then(|seconds| {
                positive_seconds(seconds).map(SocketOption::KeepAliveInterval)
            }),
            (IPPROTO_TCP, TCP_KEEPCNT) => int_option(4)?.and_then(|count| {
                u32::try_from(count).ok().filter(|c| *c > 0).map(SocketOption::KeepAliveCount)
            }),
            (IPPROTO_IPV6, IPV6_V6ONLY) => int_option(4)?.map(|on| SocketOption::V6Only(on != 0)),
            // **Path-MTU discovery, by mode: `DONT`, `DO` and `PROBE`**, which the host has under
            // the same names (`omni_platform::net::PathMtu`). MEASURED readers: ngtcp2, the
            // engine's QUIC transport, setting `DO` by the socket's family; and RakNet's game
            // join, `PROBE` for its MTU probes and then `DONT` on the same socket (`0x5019618`,
            // `0x5019888`), whose worker died on the `PROBE` refusal (2026-09-23). The level must
            // be the socket's own family's: the host option is per family, and an IPv4-level
            // option on an IPv6 socket (which Linux applies to mapped traffic) has no spelling
            // there.
            // **`UDP_GRO` is accepted and never coalesces anything, which is Linux's own
            // behaviour whenever no aggregation happens.** GRO is opportunistic: with it on, a
            // Linux socket still delivers plain datagrams -- and no `UDP_GRO` control message --
            // when nothing was coalesced, and every GRO reader (ngtcp2 here, the MEASURED caller)
            // handles that as the ordinary case. So "on" is true of this socket in the only sense
            // a caller can observe. Not so its send-side twin `UDP_SEGMENT` as a socket option,
            // which promises to split every later send into several datagrams: accepting it
            // without splitting would put one oversized datagram on the wire, so it stays
            // unimplemented and refuses. Its control-message form, per `sendmsg`, is split there.
            // `ENOPROTOOPT` on a stream socket, as Linux answers a UDP option there.
            (IPPROTO_UDP, UDP_GRO) => {
                if locked(&handle).kind() != omni_platform::net::SocketKind::Datagram {
                    view.set_errno(ENOPROTOOPT);
                    c.ret().i32(-1);
                    return Ok(());
                }
                match int_option(4)? {
                    None => None,
                    Some(_) => {
                        c.ret().i32(0);
                        return Ok(());
                    }
                }
            }
            (IPPROTO_IP, IP_MTU_DISCOVER) | (IPPROTO_IPV6, IPV6_MTU_DISCOVER) => {
                let family_matches = match locked(&handle).family() {
                    omni_platform::net::IpFamily::V4 => level == IPPROTO_IP,
                    omni_platform::net::IpFamily::V6 => level == IPPROTO_IPV6,
                };
                match int_option(4)? {
                    None => None,
                    Some(mode) if !family_matches => {
                        return Err(view.refusal(format!(
                            "the guest called setsockopt(fd {fd}, {}) with mode {mode} on a socket \
                             of the other family. The host's option is per family, and an \
                             IPv4-level option on an IPv6 socket -- which Linux applies to mapped \
                             traffic -- has no spelling there",
                            option_name(level, name)
                        )))
                    }
                    Some(IP_PMTUDISC_DONT) => Some(SocketOption::PathMtuDiscovery(PathMtu::Dont)),
                    Some(IP_PMTUDISC_DO) => Some(SocketOption::PathMtuDiscovery(PathMtu::Do)),
                    Some(IP_PMTUDISC_PROBE) => Some(SocketOption::PathMtuDiscovery(PathMtu::Probe)),
                    Some(mode) => {
                        return Err(view.refusal(format!(
                            "the guest called setsockopt(fd {fd}, {}) with mode {mode}. \
                             IP_PMTUDISC_DONT (0), _DO (2) and _PROBE (3) are implemented -- the \
                             host has the same three -- and WANT (1), INTERFACE (4) and OMIT (5) \
                             have no host mode; accepting one would report a path-MTU policy \
                             nothing applies",
                            option_name(level, name)
                        )))
                    }
                }
            }
            (SOL_SOCKET, SO_RCVBUF) => int_option(4)?.and_then(|bytes| {
                usize::try_from(bytes).ok().map(SocketOption::ReceiveBuffer)
            }),
            (SOL_SOCKET, SO_SNDBUF) => int_option(4)?
                .and_then(|bytes| usize::try_from(bytes).ok().map(SocketOption::SendBuffer)),
            (SOL_SOCKET, SO_RCVTIMEO) | (SOL_SOCKET, SO_SNDTIMEO) => {
                if given < TIMEVAL_BYTES {
                    None
                } else {
                    let timeout = read_timeval(&view, at)?;
                    Some(if name == SO_RCVTIMEO {
                        SocketOption::ReceiveTimeout(timeout)
                    } else {
                        SocketOption::SendTimeout(timeout)
                    })
                }
            }
            _ => {
                return Err(view.refusal(format!(
                    "the guest called setsockopt(fd {fd}, {}) with a {optlen}-byte value. {}",
                    option_name(level, name),
                    NetError::unimplemented_option("setsockopt", level, name)
                )))
            }
        };
        let Some(option) = option else {
            // The option is implemented and the guest's argument is not one this option accepts:
            // an `optlen` too short for the value, a buffer size that is not a size, or a
            // keep-alive figure below one. All three are `EINVAL` on a device, which is why they
            // share an arm -- and each is decided beside its own option above rather than here,
            // so that the reason is written next to the rule it comes from.
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        };
        let outcome = locked(&handle).set_option(option);
        match outcome {
            Ok(()) => 0,
            // An option that exists and is not defined on this socket kind or family. The seam
            // refuses it by name and a device answers `ENOPROTOOPT`, which is the answer every
            // caller that probes an option already branches on.
            Err(NetError::Refused { .. }) => {
                view.set_errno(ENOPROTOOPT);
                -1
            }
            Err(error) => match settled::<()>(&view, Err(error))? {
                Netted::Done(()) => 0,
                Netted::Failed(errno) => {
                    view.set_errno(errno);
                    -1
                }
            },
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `int getsockopt(int sockfd, int level, int optname, void *optval, socklen_t *optlen)`
///
/// **`SO_ERROR` is the one that matters and the one that is consumed by reading it.** It is how a
/// non-blocking `connect` reports itself, and `omni_platform::net` is built so that a host-side
/// caller cannot take it first: the error is moved into the socket's own pending slot and stays
/// readable exactly once, by whoever asks first, which is what a device does.
///
/// **Both guest objects are validated before either is written.** A `getsockopt` that filled
/// `optval` and then failed to update `*optlen` would leave the guest reading a value against a
/// stale length — the all-or-nothing shape `files::write_struct` exists for, and the direction
/// review finding M1 says to err in.
pub(super) fn getsockopt(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, level, name, optval, optlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_i32()?, a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let handle = match socket_of(&view, fs, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                c.ret().i32(-1);
                return Ok(());
            }
        };
        if optval == 0 || optlen == 0 {
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        let value_at = guest_address(&view, optval)?;
        let length_at = guest_address(&view, optlen)?;
        let blame = Blame::new(view.symbol(), view.address(), 3);
        let given = view.mem().read_u32(length_at, blame)? as usize;

        let query = match (level, name) {
            (SOL_SOCKET, SO_ERROR) => SocketQuery::Error,
            (SOL_SOCKET, SO_REUSEADDR) => SocketQuery::ReuseAddress,
            (SOL_SOCKET, SO_KEEPALIVE) => SocketQuery::KeepAlive,
            (SOL_SOCKET, SO_RCVBUF) => SocketQuery::ReceiveBuffer,
            (SOL_SOCKET, SO_SNDBUF) => SocketQuery::SendBuffer,
            (SOL_SOCKET, SO_RCVTIMEO) => SocketQuery::ReceiveTimeout,
            (SOL_SOCKET, SO_SNDTIMEO) => SocketQuery::SendTimeout,
            (IPPROTO_TCP, TCP_NODELAY) => SocketQuery::NoDelay,
            // The read half of the keep-alive timing. **The run reached the write half only**
            // -- the engine sets these and has not been observed reading them back -- and it is
            // here because the seam's own evidence that the numbers are mapped right is a round
            // trip: an option written to the wrong host constant succeeds, so only reading the
            // three back and finding the three values can tell the two apart. Refusing the read
            // while implementing the write would have meant the mapping could not be verified at
            // the one place a guest could ever check it.
            (IPPROTO_TCP, TCP_KEEPIDLE) => SocketQuery::KeepAliveIdle,
            (IPPROTO_TCP, TCP_KEEPINTVL) => SocketQuery::KeepAliveInterval,
            (IPPROTO_TCP, TCP_KEEPCNT) => SocketQuery::KeepAliveCount,
            (IPPROTO_IPV6, IPV6_V6ONLY) => SocketQuery::V6Only,
            (SOL_SOCKET, SO_BROADCAST) => SocketQuery::Broadcast,
            _ => {
                return Err(view.refusal(format!(
                    "the guest called getsockopt(fd {fd}, {}) into a {given}-byte buffer. {}",
                    option_name(level, name),
                    NetError::unimplemented_option("getsockopt", level, name)
                )))
            }
        };
        let answer = locked(&handle).get_option(query);
        let value = match answer {
            Ok(value) => value,
            Err(NetError::Refused { .. }) => {
                view.set_errno(ENOPROTOOPT);
                c.ret().i32(-1);
                return Ok(());
            }
            Err(error) => match settled::<OptionValue>(&view, Err(error))? {
                Netted::Done(value) => value,
                Netted::Failed(errno) => {
                    view.set_errno(errno);
                    c.ret().i32(-1);
                    return Ok(());
                }
            },
        };
        // **`SO_ERROR` reports the errno, not the host's classification.** It is the one option
        // whose *value* is a guest errno, and a caller that read anything else out of it would
        // branch on a number from the wrong table.
        let bytes: Vec<u8> = match value {
            OptionValue::Error(None) => 0i32.to_le_bytes().to_vec(),
            OptionValue::Error(Some(kind)) => match net_errno_for(kind) {
                Some(errno) => errno.to_le_bytes().to_vec(),
                None => {
                    return Err(view.refusal(format!(
                        "getsockopt(SO_ERROR) on fd {fd} found a host failure this layer has no \
                         errno for ({kind}). Reporting 0 would tell the guest the connection \
                         succeeded, and reporting a plausible errno would hand it an actionable \
                         branch for something nobody identified"
                    )))
                }
            },
            OptionValue::Flag(on) => i32::from(on).to_le_bytes().to_vec(),
            OptionValue::Bytes(bytes) => (bytes.min(i32::MAX as usize) as i32).to_le_bytes().to_vec(),
            OptionValue::Timeout(timeout) => timeval_bytes(timeout).to_vec(),
            // **An `int` of seconds, not a `struct timeval`**, and the difference is twelve bytes
            // and a wrong answer: `TCP_KEEPIDLE` and `TCP_KEEPINTVL` are plain integers on Linux
            // where `SO_RCVTIMEO` is a `timeval`, so writing one as the other would fill the
            // guest's four-byte buffer with the low half of a seconds field and then fail the
            // length check -- or, for a caller that passed sixteen bytes, succeed and be wrong.
            OptionValue::Interval(interval) => {
                (interval.as_secs().min(i32::MAX as u64) as i32).to_le_bytes().to_vec()
            }
            OptionValue::Count(count) => (count.min(i32::MAX as u32) as i32).to_le_bytes().to_vec(),
            // `OptionValue` is `#[non_exhaustive]`: a variant added upstream without a decision
            // here must refuse by name rather than be written into guest memory as some shape.
            other => {
                return Err(view.refusal(format!(
                    "getsockopt(fd {fd}, {}) produced {other:?}, which this layer has no guest \
                     representation for",
                    option_name(level, name)
                )))
            }
        };
        if given < bytes.len() {
            // Linux answers `EINVAL` for a buffer too small to hold the option. Truncating would
            // hand the guest a value it would read as a whole one.
            view.set_errno(consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        // Both destinations admitted before either is written.
        view.mem().checked_ptr(value_at, bytes.len(), true, blame)?;
        view.mem().checked_ptr(length_at, 4, true, blame)?;
        view.mem().write_bytes(value_at, &bytes, blame)?;
        view.mem().write_u32(length_at, bytes.len() as u32, blame)?;
        0
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== the data path

/// Reject a `flags` word this layer cannot honour, naming the bits.
///
/// Two are accepted and the rest refuse by name.
///
/// * **`MSG_NOSIGNAL`** asks for no `SIGPIPE` on a write to a closed connection. This runtime
///   delivers no signal to the guest at all (D24), so its *contract* is already satisfied —
///   accepting it is conforming rather than convenient, exactly as `O_NOCTTY` is in `files`.
/// * **`MSG_DONTWAIT`** asks for this one call not to block. `omni_platform::net` has no per-call
///   flag — it takes the socket's own mode — so it is honoured by making the call non-blocking,
///   which is what [`wants_nonblocking`] does.
///
/// Everything else — `MSG_PEEK`, `MSG_OOB`, `MSG_WAITALL`, `MSG_MORE` — asks for behaviour this
/// seam does not implement, and ignoring one would tell the guest it got something it did not:
/// a `MSG_PEEK` that consumed the datagram is a message the caller can never read again.
fn check_message_flags(view: &GuestView<'_>, flags: i32) -> AbiResult<()> {
    let unknown = flags & !(MSG_NOSIGNAL | MSG_DONTWAIT);
    if unknown == 0 {
        return Ok(());
    }
    Err(view.refusal(format!(
        "the guest called `{}` with message flags {flags:#x}, of which {unknown:#x} is outside \
         MSG_NOSIGNAL and MSG_DONTWAIT. This seam implements those two -- the first because this \
         runtime delivers no signals at all, so its contract is already met, and the second \
         because it asks for the socket's own non-blocking mode for one call. MSG_PEEK, MSG_OOB, \
         MSG_WAITALL and MSG_MORE each ask for behaviour `omni_platform::net` does not implement, \
         and ignoring one would tell the guest it got a behaviour it did not: a MSG_PEEK that \
         consumed the datagram is a message the caller can never read again",
        view.symbol()
    )))
}

/// The most a single socket transfer may move, from a guest-chosen `count`.
fn transfer_length(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX).min(SOCKET_IO_BLOCK)
}

/// Receive into guest memory, optionally reporting where the datagram came from.
///
/// # The whole destination is admitted before the socket is touched
///
/// Adapter review finding **M1**, carried across from `files::read_into_guest` and load-bearing
/// for the same reason one step further: **a received datagram is gone.** There is no offset to
/// seek back to and no second copy in the kernel, so a receive that took the bytes and then found
/// the guest's buffer unwritable would have lost a message the peer will not send again. The
/// buffer is therefore admitted first, in full, before `recv` is called.
///
/// What it does *not* promise is that the destination is still writable when the bytes arrive:
/// another guest thread may `munmap` the range between the check and the write, and no check on
/// this side of the boundary can close that window, because the window *is* the transfer.
fn socket_recv(
    view: &GuestView<'_>,
    handle: &std::sync::Mutex<Socket>,
    fd: i32,
    buffer: u64,
    count: u64,
    flags: i32,
    from: Option<(u64, u64)>,
) -> AbiResult<Netted<i64>> {
    check_message_flags(view, flags)?;
    let want = transfer_length(count);
    if want == 0 {
        // A zero-length receive on a stream socket returns 0 without consuming anything, and the
        // seam is not called at all -- so a zero-length receive at a null pointer, which is legal
        // C, does not fault.
        return Ok(Netted::Done(0));
    }
    let at = guest_address(view, buffer)?;
    let blame = Blame::new(view.symbol(), view.address(), 1);
    view.mem().checked_ptr(at, want, true, blame)?;

    let nonblocking = locked(handle).nonblocking();
    if !wants_nonblocking(nonblocking, flags) {
        let deadline = Instant::now() + Duration::from_secs(MAX_SLEEP_SECONDS);
        if !await_socket(handle, Interest::READABLE, deadline)? {
            return Err(waited_out(view, fd));
        }
    }
    let mut host = vec![0u8; want];
    let outcome = {
        let socket = locked(handle);
        match from {
            None => socket.recv(&mut host).map(|read| (read, None)),
            Some(_) => socket.recv_from(&mut host).map(|(read, peer)| (read, Some(peer))),
        }
    };
    let (read, peer) = match settled(view, outcome)? {
        Netted::Done(pair) => pair,
        Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
    };
    view.mem().write_bytes(at, &host[..read], blame)?;
    if let (Some((address_at, length_at)), Some(peer)) = (from, peer) {
        write_peer(view, address_at, length_at, &peer)?;
    }
    Ok(Netted::Done(read as i64))
}

/// Write a `recvfrom` source address back into the guest's `sockaddr` and `socklen_t`.
///
/// **A short `addrlen` truncates the address and reports the full length**, which is `recvfrom`'s
/// own contract on a device — the caller learns that what it was given was not big enough by
/// comparing the two. Nothing is written at all when either pointer is null, which is how a
/// caller says it does not want the address.
fn write_peer(
    view: &GuestView<'_>,
    address_at: u64,
    length_at: u64,
    peer: &SocketAddress,
) -> AbiResult<()> {
    if address_at == 0 || length_at == 0 {
        return Ok(());
    }
    let address_to = guest_address(view, address_at)?;
    let length_to = guest_address(view, length_at)?;
    let blame = Blame::new(view.symbol(), view.address(), 4);
    let room = view.mem().read_u32(length_to, blame)? as usize;
    let (bytes, len) = addrinfo::encode_sockaddr(peer);
    let copied = room.min(len);
    view.mem().checked_ptr(address_to, copied, true, blame)?;
    view.mem().checked_ptr(length_to, 4, true, blame)?;
    if copied > 0 {
        view.mem().write_bytes(address_to, &bytes[..copied], blame)?;
    }
    // The *full* length, not what fitted: that is how the caller learns it was truncated.
    view.mem().write_u32(length_to, len as u32, blame)
}

/// Send from guest memory, optionally to an address the caller named.
///
/// The mirror of [`socket_recv`]'s rule and the reason finding **M1** named the write direction
/// too: the whole source is read out of guest memory before anything reaches the socket, because
/// a byte that has left this machine cannot be taken back and half a request is a request.
fn socket_send(
    view: &GuestView<'_>,
    handle: &std::sync::Mutex<Socket>,
    fd: i32,
    buffer: u64,
    count: u64,
    flags: i32,
    to: Option<SocketAddress>,
) -> AbiResult<Netted<i64>> {
    check_message_flags(view, flags)?;
    let want = transfer_length(count);
    if want == 0 {
        // A zero-length send is legal and this layer makes it a no-op on a stream socket. On a
        // datagram socket a zero-length datagram is a real message, so it goes through.
        if to.is_none() {
            return Ok(Netted::Done(0));
        }
    }
    let at = guest_address(view, buffer)?;
    let blame = Blame::new(view.symbol(), view.address(), 1);
    let bytes = view.mem().read_bytes(at, want, blame)?;
    send_bytes(view, handle, fd, &bytes, flags, to)
}

/// The sending half of [`socket_send`], over bytes already read -- what `sendmsg` hands its
/// gathered `iovec`s to, so every send waits, times out and reports exactly as `sendto` does.
fn send_bytes(
    view: &GuestView<'_>,
    handle: &std::sync::Mutex<Socket>,
    fd: i32,
    bytes: &[u8],
    flags: i32,
    to: Option<SocketAddress>,
) -> AbiResult<Netted<i64>> {
    let nonblocking = locked(handle).nonblocking();
    if !wants_nonblocking(nonblocking, flags) {
        let deadline = Instant::now() + Duration::from_secs(MAX_SLEEP_SECONDS);
        if !await_socket(handle, Interest::WRITABLE, deadline)? {
            return Err(waited_out(view, fd));
        }
    }
    let outcome = {
        let socket = locked(handle);
        match &to {
            None => socket.send(bytes),
            Some(address) => socket.send_to(bytes, address),
        }
    };
    match settled(view, outcome)? {
        Netted::Done(sent) => Ok(Netted::Done(sent as i64)),
        Netted::Failed(errno) => Ok(Netted::Failed(errno)),
    }
}

/// Run a socket transfer and write the guest's `ssize_t` result.
fn transfer_result(
    c: &mut ImportCall<'_, '_>,
    state: &Active,
    run: impl FnOnce(&mut GuestView<'_>) -> AbiResult<Netted<i64>>,
) -> AbiResult<()> {
    let value = {
        let mut view = enter(c, state);
        match run(&mut view)? {
            Netted::Done(count) => count,
            Netted::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().u64(value as u64);
    Ok(())
}

/// The socket behind `fd`, or the guest's `-1` answer written and the call finished.
///
/// A macro-free early return is not expressible here — the descriptor lookup has to happen inside
/// the guest view and the return value is written after it is dropped — so each caller does the
/// two-step itself. This helper is the first step.
fn socket_for_transfer(
    view: &GuestView<'_>,
    fd: i32,
) -> AbiResult<Netted<Arc<std::sync::Mutex<Socket>>>> {
    let fs = filesystem(view)?;
    socket_of(view, fs, fd)
}

/// `ssize_t sendto(int sockfd, const void *buf, size_t len, int flags, const struct sockaddr *dest_addr, socklen_t addrlen)`
///
/// A null `dest_addr` is `send`, which is what a connected socket uses and is how the guest's own
/// `send` is compiled on bionic — there is no separate `send` import in `libroblox.so`.
pub(super) fn sendto(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, len, flags, dest, addrlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_i32()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        let to = if dest == 0 {
            None
        } else {
            match read_sockaddr(view, dest, addrlen, 4)? {
                Netted::Done(address) => Some(address),
                Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
            }
        };
        socket_send(view, &handle, fd, buf, len, flags, to)
    })
}

/// `struct msghdr` on LP64: `msg_name` at 0, `msg_namelen` (`socklen_t`) at 8, `msg_iov` at 16,
/// `msg_iovlen` (`size_t`) at 24, `msg_control` at 32, `msg_controllen` (`size_t`) at 40,
/// `msg_flags` at 48; 56 bytes. An `iovec` is `{ void *iov_base; size_t iov_len; }`, 16 bytes.
const MSGHDR_BYTES: usize = 56;
/// Linux's `UIO_MAXIOV`: more `iovec`s than this is `EMSGSIZE`.
const UIO_MAXIOV: u64 = 1024;

/// `ssize_t sendmsg(int sockfd, const struct msghdr *msg, int flags)`
///
/// The `iovec`s gathered, in order, into one message and sent exactly as `sendto` sends -- to
/// `msg_name` when there is one, on the connection when there is not. MEASURED reader: the
/// engine's QUIC transport (ngtcp2's `sendmsg`), once the engine was on Vulkan.
///
/// # `UDP_SEGMENT` is honoured by doing what the kernel's software GSO does
///
/// MEASURED: the transport sends a batch of QUIC packets as one `sendmsg` carrying a
/// `UDP_SEGMENT` control message (`cmsg_len` 18, the `__u16` segment size). Linux cuts the
/// payload into `gso_size`-byte datagrams, the last one shorter, all to the same destination, and
/// answers the whole length; when the NIC cannot segment, the kernel's own software GSO does the
/// cutting, so the datagrams on the wire do not depend on the device. That is what happens here:
/// one host send per segment. The kernel's own checks come first, from `udp_cmsg_send` and
/// `udp_send_skb` at the kernels Android 13 ships (android13-5.10 and -5.15): a malformed header
/// (`CMSG_OK`), a `SOL_UDP` message other than `UDP_SEGMENT`, a `cmsg_len` other than
/// `CMSG_LEN(2)`, or more than [`UDP_MAX_SEGMENTS`] segments is `EINVAL`, and a payload no longer
/// than one segment is sent as the one datagram it is. Two differences, neither reachable by a
/// correct caller: a segment larger than the path MTU is the host's `EMSGSIZE` from the first
/// send rather than Linux's up-front `EINVAL`, and a host send failing **after** the first
/// segment answers that failure with the earlier segments already sent -- Linux has no such point,
/// its segments leave as one buffer -- which a QUIC sender's retransmission turns into duplicate
/// packets its peer discards.
///
/// **Every other control message refuses by name, and each one is listed.** An ECN mark
/// (`IP_TOS`/`IPV6_TCLASS`) is a marking the peer's ECN validation reads and cannot be dropped
/// quietly; which ones this engine sends is for a run to say. A stream socket's control messages
/// refuse too: none has been measured there.
pub(super) fn sendmsg(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, msg, flags) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        let at = guest_address(view, msg)?;
        let blame = Blame::new(view.symbol(), view.address(), 1);
        let header = view.mem().read_bytes(at, MSGHDR_BYTES, blame)?;
        let word = |offset: usize| {
            u64::from_le_bytes(header[offset..offset + 8].try_into().expect("eight bytes"))
        };
        let (name, namelen) = (word(0), word(8) as u32);
        let (iov, iovlen) = (word(16), word(24));
        let (control, controllen) = (word(32), word(40));
        let datagram = locked(&handle).kind() == omni_platform::net::SocketKind::Datagram;
        let mut segment_size = 0usize;
        if control != 0 && controllen != 0 {
            let controls = match control_messages(view, control, controllen)? {
                Netted::Done(controls) => controls,
                Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
            };
            let mut refused = Vec::new();
            for message in &controls {
                if datagram && message.level == IPPROTO_UDP {
                    // `__udp_cmsg_send`: the one `SOL_UDP` message a send takes, at exactly its
                    // own length; anything else at that level is `EINVAL`, not ignored.
                    if message.kind != UDP_SEGMENT || message.len != UDP_SEGMENT_CMSG_LEN {
                        return Ok(Netted::Failed(consts::EINVAL));
                    }
                    segment_size = usize::from(message.segment_size);
                } else {
                    refused.push(format!(
                        "{} (cmsg_len {})",
                        option_name(message.level, message.kind),
                        message.len
                    ));
                }
            }
            if !refused.is_empty() {
                return Err(view.refusal(format!(
                    "the guest called sendmsg(fd {fd}, a {} socket) with {controllen} bytes of \
                     control messages, of which these are not implemented: {}. An ECN mark -- \
                     what a QUIC stack sends besides UDP_SEGMENT -- cannot be dropped quietly: \
                     the peer's ECN validation reads it",
                    if datagram { "datagram" } else { "stream" },
                    refused.join(", ")
                )));
            }
        }
        if iovlen > UIO_MAXIOV {
            return Ok(Netted::Failed(EMSGSIZE));
        }
        let mut bytes = Vec::new();
        let mut asked = 0u64;
        for k in 0..iovlen {
            let entry = view.mem().read_bytes(
                guest_address(view, iov + 16 * k)?,
                16,
                Blame::new(view.symbol(), view.address(), 1),
            )?;
            let base = u64::from_le_bytes(entry[0..8].try_into().expect("eight bytes"));
            let len = u64::from_le_bytes(entry[8..16].try_into().expect("eight bytes"));
            asked = asked.saturating_add(len);
            // `udp_sendmsg`'s first check: a UDP payload is at most 0xFFFF bytes, however many
            // segments it is to become.
            if datagram && asked > UDP_MAX_PAYLOAD {
                return Ok(Netted::Failed(EMSGSIZE));
            }
            let want = transfer_length(len);
            if want == 0 {
                continue;
            }
            let from = guest_address(view, base)?;
            bytes.extend(view.mem().read_bytes(from, want, Blame::new(view.symbol(), view.address(), 1))?);
        }
        let to = if name == 0 {
            None
        } else {
            match read_sockaddr(view, name, namelen as i32, 1)? {
                Netted::Done(address) => Some(address),
                Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
            }
        };
        check_message_flags(view, flags)?;
        if segment_size == 0 || bytes.len() <= segment_size {
            return send_bytes(view, &handle, fd, &bytes, flags, to);
        }
        if bytes.len() > segment_size * UDP_MAX_SEGMENTS {
            return Ok(Netted::Failed(consts::EINVAL));
        }
        let mut sent = 0i64;
        for segment in bytes.chunks(segment_size) {
            match send_bytes(view, &handle, fd, segment, flags, to)? {
                Netted::Done(count) => sent += count,
                Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
            }
        }
        Ok(Netted::Done(sent))
    })
}

/// `CMSG_LEN(sizeof(__u16))`: the 16-byte `cmsghdr` and `UDP_SEGMENT`'s segment size.
const UDP_SEGMENT_CMSG_LEN: u64 = 18;
/// Linux's `UDP_MAX_SEGMENTS` at android13-5.10 and -5.15: `1 << 6`.
const UDP_MAX_SEGMENTS: usize = 64;
/// The most one UDP send carries, segmented or not (`udp_sendmsg`: `len > 0xFFFF` is `EMSGSIZE`).
const UDP_MAX_PAYLOAD: u64 = 0xFFFF;
/// `net.core.optmem_max`'s default: a control buffer larger than this is `ENOBUFS`, because
/// `sock_kmalloc` will not allocate it.
const OPTMEM_MAX: u64 = 20480;

/// One control message's header, and `UDP_SEGMENT`'s value when it is one.
struct ControlMessage {
    len: u64,
    level: i32,
    kind: i32,
    /// The `__u16` after the header, read only when `len` is `UDP_SEGMENT`'s.
    segment_size: u16,
}

/// Walk `msg_control` as the kernel's `for_each_cmsghdr` does: `CMSG_FIRSTHDR` is nothing below
/// one header, each header must pass `CMSG_OK` (`EINVAL` otherwise), and `CMSG_NXTHDR` steps by
/// `CMSG_ALIGN(cmsg_len)` and stops when the next header would not fit.
fn control_messages(
    view: &GuestView<'_>,
    control: u64,
    controllen: u64,
) -> AbiResult<Netted<Vec<ControlMessage>>> {
    if controllen > OPTMEM_MAX {
        return Ok(Netted::Failed(ENOBUFS));
    }
    let mut out = Vec::new();
    if controllen < 16 {
        return Ok(Netted::Done(out));
    }
    let raw = view.mem().read_bytes(
        guest_address(view, control)?,
        controllen as usize,
        Blame::new(view.symbol(), view.address(), 1),
    )?;
    let mut offset = 0usize;
    loop {
        let header = &raw[offset..offset + 16];
        let len = u64::from_le_bytes(header[0..8].try_into().expect("eight bytes"));
        if len < 16 || len > controllen - offset as u64 {
            return Ok(Netted::Failed(consts::EINVAL));
        }
        let segment_size = if len == UDP_SEGMENT_CMSG_LEN {
            u16::from_le_bytes(raw[offset + 16..offset + 18].try_into().expect("two bytes"))
        } else {
            0
        };
        out.push(ControlMessage {
            len,
            level: i32::from_le_bytes(header[8..12].try_into().expect("four bytes")),
            kind: i32::from_le_bytes(header[12..16].try_into().expect("four bytes")),
            segment_size,
        });
        let next = offset + (len as usize).next_multiple_of(8);
        if next + 16 > controllen as usize {
            return Ok(Netted::Done(out));
        }
        offset = next;
    }
}

/// `struct mmsghdr` on LP64: a `msghdr` (56 bytes), then `unsigned int msg_len` at 56, padded to
/// the `msghdr`'s alignment of 8.
const MMSGHDR_BYTES: u64 = 64;
/// Linux's `MSG_WAITFORONE`: once one message has arrived, the rest of the call is `MSG_DONTWAIT`.
const MSG_WAITFORONE: i32 = 0x1_0000;
/// `sizeof(struct sockaddr_in6)`, the longest address a datagram socket here reports.
const SOCKADDR_IN6_BYTES: usize = 28;

/// `int recvmmsg(int sockfd, struct mmsghdr *msgvec, unsigned int vlen, int flags, struct timespec *timeout)`
///
/// Up to `vlen` datagrams, each received as `recvmsg` receives one ([`receive_message`]), with its
/// length in `msg_len`. MEASURED reader: the engine's QUIC transport,
/// `recvmmsg(fd, msgvec, 16, 0, NULL)`, once it had sent with `UDP_SEGMENT`.
///
/// Linux's `__sys_recvmmsg`, in its order: `vlen` above `UIO_MAXIOV` is clamped to it; each message
/// waits as `recv` does -- by the socket's own blocking mode and `MSG_DONTWAIT` -- and after the
/// first, `MSG_WAITFORONE` makes the rest `MSG_DONTWAIT`. A failure on the first message is the
/// call's; a failure after it ends the batch and the count so far is the answer, which is how a
/// non-blocking socket with fewer than `vlen` datagrams waiting answers. One difference: Linux keeps
/// a non-`EAGAIN` failure after the first message for the socket's next call (`sk_err`), and this
/// layer does not -- reachable only by a host failure between two datagrams. A `timeout` refuses by
/// name: its semantics (checked only between datagrams) are for a run to show.
pub(super) fn recvmmsg(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, msgvec, vlen, flags, timeout) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()? as u32, a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        if timeout != 0 {
            return Err(view.refusal(format!(
                "the guest called recvmmsg(fd {fd}, vlen {vlen}) with a timeout at {timeout:#x}. \
                 Linux checks it only between datagrams, so it neither bounds the first wait nor \
                 interrupts one; no run has passed one, and answering it any other way would be a \
                 guess at a contract this layer has not measured"
            )));
        }
        check_message_flags(view, flags & !MSG_WAITFORONE)?;
        let nonblocking = locked(&handle).nonblocking();
        let mut dontwait = flags & MSG_DONTWAIT != 0;
        let mut received = 0i64;
        for index in 0..u64::from(vlen.min(UIO_MAXIOV as u32)) {
            let entry = msgvec.wrapping_add(index * MMSGHDR_BYTES);
            match receive_message(view, &handle, fd, entry, !nonblocking, dontwait)? {
                Netted::Done(length) => {
                    let blame = Blame::new(view.symbol(), view.address(), 1);
                    view.mem().write_u32(guest_address(view, entry + 56)?, length as u32, blame)?;
                    received += 1;
                    if flags & MSG_WAITFORONE != 0 {
                        dontwait = true;
                    }
                }
                Netted::Failed(errno) if received == 0 => return Ok(Netted::Failed(errno)),
                Netted::Failed(_) => break,
            }
        }
        Ok(Netted::Done(received))
    })
}

/// `ssize_t recvmsg(int sockfd, struct msghdr *msg, int flags)`
///
/// One message, received exactly as one entry of `recvmmsg` is ([`receive_message`], which has
/// what is and is not reported): the same helper, so the two cannot disagree about a `msghdr`.
/// MEASURED reader: once signed in, a QUIC receiver thread (`0x5b19ccc`: one `iovec`, a
/// 128-byte `msg_name` and a 1,064-byte control buffer it then parses for IPv4/IPv6 ancillary
/// data) died on the unbound symbol. It gets no control message, which is true: every option that
/// would make Linux add one is refused by `setsockopt`.
pub(super) fn recvmsg(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, msg, flags) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        check_message_flags(view, flags)?;
        let nonblocking = locked(&handle).nonblocking();
        receive_message(view, &handle, fd, msg, !nonblocking, flags & MSG_DONTWAIT != 0)
    })
}

/// Receive one datagram into the `msghdr` at `msg`, as `recvmsg` does: scattered over its
/// `iovec`s in order, its source written to `msg_name` (truncated to `msg_namelen`, which is then
/// set to the full length -- `move_addr_to_user`), `msg_controllen` set to 0 and `msg_flags` to 0.
///
/// **No control message is produced**, and that is true rather than chosen: the options that make
/// Linux add one on receive (`IP_RECVTOS`, `IP_PKTINFO` and their IPv6 twins) are all refused by
/// `setsockopt`, and `UDP_GRO`, which is accepted, adds its message only when it coalesced -- which
/// it never does here. `MSG_TRUNC` in `msg_flags` is not reported: the seam cannot see a
/// truncation (see `Socket::recv_from`).
///
/// The whole scatter list is admitted for writing before the socket is touched, for
/// [`socket_recv`]'s reason: a received datagram is gone.
///
/// **`dontwait` on a blocking socket is a readiness check, not a skipped wait.** The host socket's
/// mode is the guest's, so a host receive on a blocking socket with nothing queued would block
/// for ever -- MEASURED, by this function's own test, the first time it skipped the wait instead.
/// A zero-length poll answers `EAGAIN` when nothing is queued, which is what the flag asks.
fn receive_message(
    view: &GuestView<'_>,
    handle: &std::sync::Mutex<Socket>,
    fd: i32,
    msg: u64,
    blocking: bool,
    dontwait: bool,
) -> AbiResult<Netted<i64>> {
    let at = guest_address(view, msg)?;
    let blame = Blame::new(view.symbol(), view.address(), 1);
    let header = view.mem().read_bytes(at, MSGHDR_BYTES, blame)?;
    let word = |offset: usize| {
        u64::from_le_bytes(header[offset..offset + 8].try_into().expect("eight bytes"))
    };
    let (name, room) = (word(0), word(8) as u32 as usize);
    let (iov, iovlen) = (word(16), word(24));
    if iovlen > UIO_MAXIOV {
        return Ok(Netted::Failed(EMSGSIZE));
    }
    let mut spans: Vec<(GuestAddr, usize)> = Vec::new();
    let mut total = 0usize;
    for k in 0..iovlen {
        let entry = view.mem().read_bytes(guest_address(view, iov + 16 * k)?, 16, blame)?;
        let base = u64::from_le_bytes(entry[0..8].try_into().expect("eight bytes"));
        let len = transfer_length(u64::from_le_bytes(entry[8..16].try_into().expect("eight bytes")))
            .min(SOCKET_IO_BLOCK - total);
        if len == 0 {
            continue;
        }
        let to = guest_address(view, base)?;
        view.mem().checked_ptr(to, len, true, blame)?;
        spans.push((to, len));
        total += len;
    }
    // The header's written fields and the name, admitted with the scatter list: none of them can
    // be found unwritable after the datagram has been taken.
    view.mem().checked_ptr(at, MSGHDR_BYTES, true, blame)?;
    if name != 0 && room > 0 {
        view.mem().checked_ptr(guest_address(view, name)?, room.min(SOCKADDR_IN6_BYTES), true, blame)?;
    }
    if blocking && dontwait {
        if !ready_now(handle, Interest::READABLE) {
            return Ok(Netted::Failed(consts::EAGAIN));
        }
    } else if blocking {
        let deadline = Instant::now() + Duration::from_secs(MAX_SLEEP_SECONDS);
        if !await_socket(handle, Interest::READABLE, deadline)? {
            return Err(waited_out(view, fd));
        }
    }
    let mut host = vec![0u8; total];
    let outcome = {
        let socket = locked(handle);
        if socket.kind() == omni_platform::net::SocketKind::Datagram {
            socket.recv_from(&mut host).map(|(read, peer)| (read, Some(peer)))
        } else {
            socket.recv(&mut host).map(|read| (read, None))
        }
    };
    let (read, peer) = match settled(view, outcome)? {
        Netted::Done(pair) => pair,
        Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
    };
    let mut from = 0usize;
    for (to, len) in spans {
        if from == read {
            break;
        }
        let take = len.min(read - from);
        view.mem().write_bytes(to, &host[from..from + take], blame)?;
        from += take;
    }
    if let (true, Some(peer)) = (name != 0, peer) {
        let (bytes, len) = addrinfo::encode_sockaddr(&peer);
        let copied = room.min(len);
        if copied > 0 {
            view.mem().write_bytes(guest_address(view, name)?, &bytes[..copied], blame)?;
        }
        view.mem().write_u32(at + 8, len as u32, blame)?;
    }
    view.mem().write_u64(at + 40, 0, blame)?;
    view.mem().write_u32(at + 48, 0, blame)?;
    Ok(Netted::Done(read as i64))
}

/// `ssize_t __sendto_chk(int fd, const void *buf, size_t len, size_t buflen, int flags, const struct sockaddr *dest, socklen_t addrlen)`
///
/// The `_FORTIFY_SOURCE` form of `sendto`, and **the check is the whole of what it adds**: bionic
/// compares the length being sent against the compiler's knowledge of the buffer's size and calls
/// `__fortify_fatal` when the first exceeds the second. That is a guest defect being caught, so it
/// is reported by name with both numbers rather than sent — a `sendto` that went ahead would put
/// whatever follows the buffer on the network.
pub(super) fn sendto_chk(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, len, buflen, flags, dest, addrlen) = {
        let mut a = c.args();
        (
            a.next_i32()?,
            a.next_u64()?,
            a.next_u64()?,
            a.next_u64()?,
            a.next_i32()?,
            a.next_u64()?,
            a.next_i32()?,
        )
    };
    if len > buflen {
        return Err(refuse(
            c,
            format!(
                "the guest called __sendto_chk(fd {fd}, buf={buf:#x}, len={len}, buflen={buflen}) \
                 -- it asked to send {len} bytes out of a buffer the compiler knows is {buflen}. \
                 That is what _FORTIFY_SOURCE exists to catch and bionic answers it with \
                 __fortify_fatal, which terminates the process. Sending would put whatever \
                 follows the buffer on the network"
            ),
        ));
    }
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        let to = if dest == 0 {
            None
        } else {
            match read_sockaddr(view, dest, addrlen, 5)? {
                Netted::Done(address) => Some(address),
                Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
            }
        };
        socket_send(view, &handle, fd, buf, len, flags, to)
    })
}

/// `ssize_t recvfrom(int sockfd, void *buf, size_t len, int flags, struct sockaddr *src_addr, socklen_t *addrlen)`
///
/// A null `src_addr` is `recv`, which is how a connected socket receives and how the guest's own
/// `recv` is compiled — `libroblox.so` imports no `recv`.
pub(super) fn recvfrom(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, len, flags, src, addrlen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        let from = if src == 0 { None } else { Some((src, addrlen)) };
        socket_recv(view, &handle, fd, buf, len, flags, from)
    })
}

// ================================================================== read and write, dispatched

/// `ssize_t read(int fd, void *buf, size_t count)` — a socket, or whatever `files` makes of it.
///
/// **Bound here rather than in `files` because `libroblox.so` imports no `recv`.** MEASURED, from
/// the APK's own undefined-symbol table: the stream data path is `read`/`write` on the socket
/// descriptor, which is what OpenSSL's `readsocket`/`writesocket` expand to on every non-Windows
/// target, and the engine carries its own OpenSSL. This module's header has the two reasons the
/// socket case cannot be served by `Filesystem::read`.
///
/// Everything that is not a socket goes to `files::read` unchanged, which is the whole of what
/// this function adds: one `is_socket` test and a delegation.
pub(super) fn read(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    if !is_socket(c, fd)? {
        return super::files::read(c);
    }
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        socket_recv(view, &handle, fd, buf, count, 0, None)
    })
}

/// `ssize_t write(int fd, const void *buf, size_t count)` — a socket, or `files`.
pub(super) fn write(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?)
    };
    if !is_socket(c, fd)? {
        return super::files::write(c);
    }
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        socket_send(view, &handle, fd, buf, count, 0, None)
    })
}

/// `ssize_t __write_chk(int fd, const void *buf, size_t count, size_t buflen)` — a socket, or
/// `files`.
///
/// The FORTIFY check itself stays in `files::write_chk`, which is where it was written and
/// tested; this adds the socket dispatch in front of it and nothing else.
pub(super) fn write_chk(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, buf, count, buflen) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    if !is_socket(c, fd)? {
        return super::files::write_chk(c);
    }
    if count > buflen {
        return Err(refuse(
            c,
            format!(
                "the guest called __write_chk(fd {fd}, buf={buf:#x}, count={count}, \
                 buflen={buflen}) on a socket -- it asked to write {count} bytes out of a buffer \
                 the compiler knows is {buflen}. bionic answers that with __fortify_fatal"
            ),
        ));
    }
    let state = active(c.symbol(), c.address())?;
    transfer_result(c, &state, |view| {
        let handle = match socket_for_transfer(view, fd)? {
            Netted::Done(handle) => handle,
            Netted::Failed(errno) => return Ok(Netted::Failed(errno)),
        };
        socket_send(view, &handle, fd, buf, count, 0, None)
    })
}

/// Whether `fd` is a socket in this instance, for the three dispatching symbols.
///
/// **An instance with no filesystem has no descriptors at all**, so it has no sockets either and
/// the call goes to `files`, which refuses by name with the message that says a root is missing.
/// Answering the dispatch question here rather than letting `files` do it keeps that refusal in
/// one place.
fn is_socket(c: &ImportCall<'_, '_>, fd: i32) -> AbiResult<bool> {
    let state = active(c.symbol(), c.address())?;
    Ok(state.bionic.filesystem().is_some_and(|fs| fs.is_socket(fd)))
}

// ================================================================== name resolution

/// Map a resolver failure onto the `EAI_*` numbering `gai_strerror` already carries.
///
/// **[`ResolveFailure::Unclassified`] deliberately has no code**, and that is the one decision in
/// this function. D30 names it as the trap in as many words: a measured failure path may be used
/// as a diagnostic and must never become the finished behaviour, and a default `EAI_NONAME` for
/// anything unrecognised is exactly that — the guest would be told the name does not exist, would
/// stop asking for ever, and nothing anywhere would record that this layer had invented the
/// answer. `omni_platform::net::resolve` documents the case it arises in: the unix targets, where
/// `std` reports a resolver failure with no error number at all.
///
/// The numbering is bionic's `netdb.h`, which counts **up** from 1 where glibc counts down from
/// -1 — so taking the development host's values would be silently wrong. It is corroborated inside
/// `omni_bionic::net`: `gai_strerror_message`'s table is bionic's own `ai_errlist`, and row 8 is
/// "Name or service not known", which is the string `EAI_NONAME` carries.
fn eai_for(failure: ResolveFailure) -> Option<i32> {
    Some(match failure {
        ResolveFailure::NoSuchHost => net::EAI_NONAME,
        ResolveFailure::NoAddressOfFamily => EAI_ADDRFAMILY,
        ResolveFailure::Transient => EAI_AGAIN,
        ResolveFailure::NonRecoverable => EAI_FAIL,
        // `ResolveFailure::Unclassified`, and nothing else. A wildcard because the enum is
        // `#[non_exhaustive]`: a class added upstream without a decision here must refuse by name
        // rather than acquire a plausible code.
        _ => return None,
    })
}

/// What the guest's `struct addrinfo` hints asked for.
#[derive(Debug, Clone, Copy)]
struct Hints {
    flags: i32,
    family: i32,
    socktype: i32,
    protocol: i32,
}

impl Hints {
    /// What a null `hints` means: any family, any socket type, no flags.
    const ANY: Hints =
        Hints { flags: 0, family: addrinfo::AF_UNSPEC, socktype: 0, protocol: 0 };
}

/// `int getaddrinfo(const char *node, const char *service, const struct addrinfo *hints, struct addrinfo **res)`
///
/// **Answered from M6, where it had been refused by name since phase 3d.** The refusal gave two
/// reasons and said either alone was decisive. One is void: D30 withdrew Global Constraint 8 and
/// `omni_platform::net::resolve` is the resolver it said was missing. The other was the real work
/// and is what [`addrinfo`](super::addrinfo) is:
///
/// > There is nowhere to put the answer. `getaddrinfo` allocates a linked list of
/// > `struct addrinfo` *in guest memory* [...] the adapter's pool is a bump allocator that never
/// > frees, so a guest resolving in a loop would exhaust it, and task 2's finding F9 forbids a
/// > handler mapping guest memory at all.
///
/// Both halves of that still hold, which is why the answer is a **bounded slab with a free list**,
/// mapped in `Bionic::new` and carved into [`ADDRINFO_RESULTS`](super::ADDRINFO_RESULTS) slots.
/// `freeaddrinfo` gives a slot back by matching the head pointer this call handed out. **A full
/// slab refuses by name**: never an overwrite of a list the guest is still walking, and never a
/// truncated one, which would show up as a connection to an address the resolver did not return.
///
/// The layout warning the refusal recorded is kept alive rather than retired —
/// `sizeof(struct addrinfo)` is **48 bytes** and **ASSUMED**, there is no NDK on this machine, and
/// **bionic orders `ai_canonname` before `ai_addr` where glibc reverses them**. `addrinfo`'s module
/// documentation carries it, and `the_addrinfo_layout_is_bionics_and_not_glibcs` is the assertion.
///
/// # What is honoured, what is accepted, and what refuses
///
/// * **`AI_NUMERICHOST` is honoured exactly**: a node that is not an address literal answers
///   `EAI_NONAME` without a query leaving the machine, which is the whole point of the flag.
/// * **`AI_NUMERICSERV` is honoured by construction**: this seam takes a numeric service only —
///   `omni_platform::net::service_port` refuses a service *name* by name rather than guessing that
///   `https` is 443, because a built-in table would be a claim about the host's `/etc/services`
///   that nothing here can check.
/// * **`AI_CANONNAME` is accepted and `ai_canonname` is left null.** `std::net::ToSocketAddrs`
///   returns no canonical name, so there is none to report; `omni_platform::net::resolve` records
///   that limit, and a name invented here is the one the guest would log and trust.
/// * **`AI_ADDRCONFIG`, `AI_V4MAPPED`, `AI_V4MAPPED_CFG` and `AI_ALL` are accepted and do not
///   change the answer.** Each of them *narrows or widens by family*, and what they cannot do is
///   make this layer return an address the resolver did not give: the observable difference is a
///   `connect` the guest tries and the host refuses, which is the same outcome the guest already
///   has a branch for. Stated here rather than left silent, because accepting a flag is a claim.
/// * **`AI_PASSIVE` is honoured for a null `node`**, the only case it changes: the wildcard
///   address to bind to, where without it a null `node` is loopback (bionic's `explore_null`).
/// * A bit outside `netdb.h` is `EAI_BADFLAGS`, which is the guest's own answer for it.
pub(super) fn getaddrinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (node, service, hints, res) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let code = {
        let mut view = enter(c, &state);
        resolve_into(c, &mut view, node, service, hints, res)?
    };
    c.ret().i32(code);
    Ok(())
}

/// The whole of `getaddrinfo`'s decision, with the guest view alive.
///
/// Returns the `EAI_*` code, or `0` with `*res` written.
fn resolve_into(
    c: &ImportCall<'_, '_>,
    view: &mut GuestView<'_>,
    node: u64,
    service: u64,
    hints: u64,
    res: u64,
) -> AbiResult<i32> {
    if node == 0 && service == 0 {
        // bionic's first check: no host and no service is `EAI_NONAME`.
        return Ok(net::EAI_NONAME);
    }
    let _ = c;
    if res == 0 {
        // The out-parameter is where the whole answer goes. A null one is `EFAULT` on a device;
        // it arrives here as a refusal for `files::path_for`'s stated reason -- a guest that
        // ignored the return would walk an uninitialised pointer.
        return Err(view.refusal("`getaddrinfo` was given a null `res` pointer"));
    }
    let res_at = guest_address(view, res)?;

    let hints = if hints == 0 {
        Hints::ANY
    } else {
        let at = guest_address(view, hints)?;
        let blame = Blame::new(view.symbol(), view.address(), 2);
        let raw = view.mem().read_bytes(at, addrinfo::ADDRINFO_BYTES, blame)?;
        let field = |offset: usize| {
            i32::from_le_bytes(raw[offset..offset + 4].try_into().expect("four bytes"))
        };
        Hints { flags: field(0), family: field(4), socktype: field(8), protocol: field(12) }
    };

    if hints.flags & !AI_MASK != 0 {
        return Ok(EAI_BADFLAGS);
    }
    let want = match hints.family {
        addrinfo::AF_UNSPEC => None,
        addrinfo::AF_INET => Some(IpFamily::V4),
        addrinfo::AF_INET6 => Some(IpFamily::V6),
        _ => return Ok(EAI_FAMILY),
    };
    // **One node per address per socket type**, and a zero `ai_socktype` means both. That is
    // bionic's own shape: its `explore` table has a row per (family, socktype, protocol) and a
    // hints socktype of 0 visits every row, which is why a real `getaddrinfo` returns several
    // nodes for one address. The order is bionic's too -- datagram before stream -- and it is
    // ASSUMED from that table rather than measured, for the same reason the layout is.
    let socktypes: &[(i32, i32)] = match hints.socktype {
        SOCK_STREAM => &[(SOCK_STREAM, IPPROTO_TCP)],
        SOCK_DGRAM => &[(SOCK_DGRAM, IPPROTO_UDP)],
        0 => &[(SOCK_DGRAM, IPPROTO_UDP), (SOCK_STREAM, IPPROTO_TCP)],
        _ => return Ok(EAI_SOCKTYPE),
    };
    if hints.protocol != IPPROTO_IP
        && !socktypes.iter().any(|(_, protocol)| *protocol == hints.protocol)
    {
        // A protocol that no socket type in the request can carry. Bionic answers `EAI_SOCKTYPE`
        // for the mismatch rather than `EAI_PROTOCOL`, which it reserves for a protocol it does
        // not know at all.
        return Ok(EAI_SOCKTYPE);
    }
    let socktypes: Vec<(i32, i32)> = socktypes
        .iter()
        .copied()
        .filter(|(_, protocol)| hints.protocol == IPPROTO_IP || *protocol == hints.protocol)
        .collect();

    let name = if node == 0 {
        String::new()
    } else {
        let at = guest_address(view, node)?;
        let bytes = view.mem().cstr(at, Blame::new(view.symbol(), view.address(), 0))?;
        match String::from_utf8(bytes) {
            Ok(name) => name,
            // A host name is IDNA or ASCII; bytes that are not UTF-8 are not a name any resolver
            // can be asked about, and `EAI_NONAME` is what a device answers for one.
            Err(_) => return Ok(net::EAI_NONAME),
        }
    };
    if node != 0 && hints.flags & AI_NUMERICHOST != 0 && name.parse::<std::net::IpAddr>().is_err() {
        // The flag's entire purpose: no query may leave the machine for a name that is not
        // already an address. `EAI_NONAME` is what `getaddrinfo` answers for it.
        return Ok(net::EAI_NONAME);
    }
    let port = if service == 0 {
        0
    } else {
        let at = guest_address(view, service)?;
        let bytes = view.mem().cstr(at, Blame::new(view.symbol(), view.address(), 1))?;
        if bytes.is_empty() {
            // **An empty service is answered, and needs no services database to answer.** bionic's
            // `str2number` rejects the empty string before `strtoul` (`if (*p == ' ') return
            // -1`), so `get_port` takes the name path: `EAI_NONAME` under `AI_NUMERICSERV`, and
            // otherwise `getservbyname("", proto)`, which no entry of any services table can match
            // -- `EAI_SERVICE`. That is `get_portmatch`, which bionic runs before it resolves the
            // node, so the name is never looked up. MEASURED why: pressing Play on a game page,
            // a TaskScheduler worker called `getaddrinfo(host, "", ...)` and died on this seam's
            // numeric-only refusal (2026-09-23), and the join froze.
            return Ok(if hints.flags & AI_NUMERICSERV != 0 { net::EAI_NONAME } else { EAI_SERVICE });
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        match platnet::service_port(&text) {
            Ok(port) => port,
            Err(error) => return Err(view.refusal(error.to_string())),
        }
    };

    let policy = policy(view)?;
    let addresses = if node == 0 {
        // **A null `node` is answered, as bionic's `explore_null` answers it, and no query leaves
        // the machine.** With `AI_PASSIVE` it is the family's wildcard address (`0.0.0.0`, `::`)
        // -- an address to `bind` a socket to -- and without it the loopback address, each with
        // the service's port. Families in bionic's `explore` table order, IPv6 before IPv4, each
        // kept only if the host can make a socket of it (bionic probes with `socket(af,
        // SOCK_DGRAM)`); this host can make both. It was refused, on the reasoning that a bind
        // address meant `listen`/`accept`; MEASURED otherwise (2026-09-23): pressing Play on a
        // game page, a TaskScheduler worker asked for one and died on the refusal -- a UDP client
        // binding its own port, which is client work (`bind` is implemented).
        let families: &[platnet::IpFamily] = match want {
            Some(family) => match family {
                platnet::IpFamily::V4 => &[platnet::IpFamily::V4],
                platnet::IpFamily::V6 => &[platnet::IpFamily::V6],
            },
            None => &[platnet::IpFamily::V6, platnet::IpFamily::V4],
        };
        families
            .iter()
            .map(|family| {
                match (*family, hints.flags & AI_PASSIVE != 0) {
                    (platnet::IpFamily::V4, true) => platnet::SocketAddress::V4 { address: [0; 4], port },
                    (platnet::IpFamily::V6, true) => {
                        platnet::SocketAddress::V6 { address: [0; 16], port, flowinfo: 0, scope_id: 0 }
                    }
                    (family, false) => platnet::SocketAddress::loopback(family, port),
                }
            })
            .collect::<Vec<_>>()
    } else {
        match platnet::resolve(&name, port, want, &policy) {
        Ok(addresses) => addresses,
        Err(error) => {
            return match error.resolve_failure().and_then(eai_for) {
                Some(code) => Ok(code),
                // Everything else -- a policy refusal, an unclassified resolver failure, an empty
                // name -- refuses by name. D30 names the alternative as the trap: an `EAI_*` for a
                // failure nobody classified tells the guest something definite about a name, and
                // the guest acts on it for ever.
                None => Err(view.refusal(error.to_string())),
            };
            }
        }
    };

    let mut nodes = Vec::new();
    for address in &addresses {
        for (socktype, protocol) in &socktypes {
            nodes.push(addrinfo::ResultNode {
                socktype: *socktype,
                protocol: *protocol,
                address: *address,
            });
        }
    }
    if nodes.len() > addrinfo::ADDRINFO_NODES_PER_RESULT {
        return Err(view.refusal(format!(
            "`{name}` resolved to {} address(es) which, with {} socket type(s), is {} \
             `struct addrinfo` nodes -- and one result slot holds {}. The list is refused rather \
             than truncated: a truncated one is indistinguishable from a complete one to the \
             guest, which would connect to whichever addresses survived while nothing recorded \
             that the resolver had offered others. Raising ADDRINFO_NODES_PER_RESULT is the fix, \
             and it is a decision about the slab's size rather than a line to add",
            addresses.len(),
            socktypes.len(),
            nodes.len(),
            addrinfo::ADDRINFO_NODES_PER_RESULT
        )));
    }

    let slab = view.active.bionic.addrinfo_slab();
    let Some(head) = slab.take() else {
        return Err(view.refusal(format!(
            "the `struct addrinfo` slab is full: all {} result slots are live, which means the \
             guest holds that many lists it has not passed to `freeaddrinfo`. The call is refused \
             rather than reusing a slot, because reusing one overwrites a list the guest may still \
             be walking -- and it would do it silently, since nothing in a walk can tell a node \
             from a node that has been replaced. `Bionic::addrinfo_slab().live()` is what says \
             whether this is a guest that leaks or a slab that is too small",
            super::ADDRINFO_RESULTS
        )));
    };
    // From here on the slot is taken, so every failure path gives it back. A slot leaked by an
    // error return is a slot no `freeaddrinfo` can ever name, and after
    // ADDRINFO_RESULTS of them every later resolution refuses.
    let bytes = addrinfo::encode_result(&nodes, head);
    let blame = Blame::new(view.symbol(), view.address(), 3);
    // **The whole list, then the head pointer, and the guest's own pointer last.** The guest
    // learns the address only after the bytes it points at are there, so a write that fails
    // leaves `*res` untouched and the guest with nothing to walk.
    if let Err(error) = view.mem().write_bytes(head, &bytes, blame) {
        slab.release(head);
        return Err(error);
    }
    if let Err(error) = view.mem().write_u64(res_at, head as u64, blame) {
        slab.release(head);
        return Err(error);
    }
    Ok(0)
}

/// `void freeaddrinfo(struct addrinfo *res)`
///
/// **Answered from M6, and the `void` return is still exactly why it cannot be a stub.** The old
/// refusal's argument was that nothing in this layer could produce a list, so any pointer arriving
/// here came from somewhere else — and that doing nothing would be indistinguishable from a
/// correct free, "on the day `getaddrinfo` starts returning real lists and this stub starts
/// leaking them". This is that day, and the answer is a free list rather than a no-op.
///
/// The slot is matched on the **head pointer this layer handed out**, exactly, and a pointer that
/// matches nothing is a refusal naming it. That refuses three things a range check would accept:
///
/// * `res->ai_next`, which a guest walking and freeing as it went would pass — and which would
///   free a slot the guest is still reading;
/// * a list already freed, which is a double free and a defect worth hearing about;
/// * a pointer from somewhere else entirely, which is the case the old refusal was about.
///
/// A null pointer is a no-op and not a refusal: bionic's own `freeaddrinfo` is a `while (ai)`
/// loop, so `freeaddrinfo(NULL)` does nothing there and callers written against it rely on that.
pub(super) fn freeaddrinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let res = c.args().next_u64()?;
    if res == 0 {
        return Ok(());
    }
    let state = active(c.symbol(), c.address())?;
    let head = guest_address(&enter(c, &state), res)?;
    if state.bionic.addrinfo_slab().release(head) {
        return Ok(());
    }
    Err(refuse(
        c,
        format!(
            "the guest called freeaddrinfo({res:#x}), and that is not the head of any list this \
             layer handed out. `getaddrinfo` answers out of a bounded slab and records the head \
             pointer of every live result; a pointer that matches none of them is one of three \
             things, and none of them is a free: a pointer INTO a live list (`res->ai_next`, \
             which a guest that walks and frees as it goes would pass, and freeing it would \
             release a slot the guest is still reading), a list that has already been freed, or a \
             pointer from somewhere else. This function returns `void`, which is what makes doing \
             nothing the dangerous answer: a silent no-op is indistinguishable from a correct free"
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

    /// **A set naming a socket or the readiness gate can become ready; one naming neither cannot.**
    ///
    /// This predicate is what `bounded_wait` turns the 60-second cap on, so getting it wrong in
    /// the "false" direction refuses the 69.001-second `poll` the engine's HTTP stack asks for
    /// over the settings socket and kills the thread carrying the connection — MEASURED, and it
    /// is why the cap stopped being unconditional. Getting it wrong in the "true" direction lets
    /// a plain sleep run unbounded, which is the D16 hazard the cap exists for.
    ///
    /// It is a function rather than an inline `||` for the reason `clocks::capped` is one: a
    /// mutation of it has to have somewhere to be caught, and the condition it feeds needs an
    /// `ImportCall` to reach. See `sockcfg-B2` in `tools/mutate.py`.
    #[test]
    fn a_set_can_become_ready_exactly_when_it_names_a_socket_or_the_gate() {
        assert!(!Watch::default().can_change(), "a set of files and standard streams cannot");
        let socket_only =
            Watch { sockets: vec![(7, Interest::READABLE)], gate: false, deadline: None };
        assert!(socket_only.can_change(), "a socket's readiness is the network's");
        let gate_only = Watch { sockets: Vec::new(), gate: true, deadline: None };
        assert!(gate_only.can_change(), "a pipe or an eventfd is another descriptor's writer");
        let both = Watch { sockets: vec![(7, Interest::BOTH)], gate: true, deadline: None };
        assert!(both.can_change());
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
    /// **The socket constants are the Linux arm64 values**, written as literals.
    ///
    /// The same discipline `omni_bionic::errno` records and the same reason, one family along:
    /// these numbers reach the guest, the development host's are different — `SOL_SOCKET` is
    /// `0xFFFF` on Winsock and 1 here, `WSAECONNREFUSED` is 10061 and `ECONNREFUSED` is 111 — and
    /// a wrong one makes the guest take the wrong branch rather than making the build fail.
    /// Comparing a constant to itself would pass against any value.
    #[test]
    fn the_socket_constants_are_the_linux_values() {
        // `socket(2)`'s type word.
        assert_eq!(SOCK_STREAM, 1);
        assert_eq!(SOCK_DGRAM, 2);
        assert_eq!(SOCK_NONBLOCK, 0o4000);
        assert_eq!(SOCK_CLOEXEC, 0o2_000_000);
        assert_eq!(SOCK_FLAGS, 0o2_004_000, "the two bits that are not the socket kind");
        // Protocols and levels.
        assert_eq!(IPPROTO_IP, 0);
        assert_eq!(IPPROTO_TCP, 6);
        assert_eq!(IPPROTO_UDP, 17);
        assert_eq!(IPPROTO_IPV6, 41);
        assert_eq!(SOL_SOCKET, 1, "Winsock spells this 0xFFFF; the guest is Linux");
        // Options.
        assert_eq!(SO_REUSEADDR, 2);
        assert_eq!(SO_ERROR, 4);
        assert_eq!(SO_SNDBUF, 7);
        assert_eq!(SO_RCVBUF, 8);
        assert_eq!(SO_RCVTIMEO, 20, "the _OLD form, which is what LP64 userspace uses");
        assert_eq!(SO_SNDTIMEO, 21);
        assert_eq!(SO_KEEPALIVE, 9, "the switch, which M6's network run found the engine setting");
        assert_eq!(TCP_NODELAY, 1);
        assert_eq!(IPV6_V6ONLY, 26);
        // **The three keep-alive TIMING options, and they are the ones where taking the
        // development host's numbers would be silently wrong in the worst way.** Windows spells
        // the idle time `TCP_KEEPALIVE` = 3, the count `TCP_KEEPCNT` = 16 and the interval
        // `TCP_KEEPINTVL` = 17 -- and it puts `TCP_MAXRT` on 5, which is Linux's
        // `TCP_KEEPINTVL`. So a pass-through of the guest's 5 sets a real Windows option with a
        // different meaning, `setsockopt` answers 0, and nothing anywhere reports it. The
        // mapping itself is asserted by round trip in
        // `the_keep_alive_timing_options_reach_the_socket_in_the_guests_numbering`; these three
        // lines are the guest half of it.
        assert_eq!(TCP_KEEPIDLE, 4, "Windows calls the same quantity TCP_KEEPALIVE and numbers it 3");
        assert_eq!(TCP_KEEPINTVL, 5, "Windows numbers this 17, and puts TCP_MAXRT on 5");
        assert_eq!(TCP_KEEPCNT, 6, "Windows numbers this 16");
        // `shutdown(2)`.
        assert_eq!(SHUT_RD, 0);
        assert_eq!(SHUT_WR, 1);
        assert_eq!(SHUT_RDWR, 2);
        // Message flags.
        assert_eq!(MSG_DONTWAIT, 0x40);
        assert_eq!(MSG_NOSIGNAL, 0x4000);
        // **`SO_KEEPALIVE` used to be the exception here and is not any more**, and the line is
        // corrected in place rather than deleted because the history is the point: M6's network
        // run found the engine setting option 9 at `SOL_SOCKET` on the settings socket, this
        // seam had no `SocketOption` for it, and the refusal killed the fetch thread. It is
        // implemented now and is asserted above with the rest.
        //
        // What is left is the shape that assertion had, pointed at something that genuinely is
        // not implemented: **`SO_LINGER` is 13**, nothing here has a variant for it, and if it
        // ever gains one this line is where a reader finds out it used to refuse.
        for implemented in [
            SO_REUSEADDR,
            SO_ERROR,
            SO_SNDBUF,
            SO_RCVBUF,
            SO_RCVTIMEO,
            SO_SNDTIMEO,
            SO_KEEPALIVE,
        ] {
            assert_ne!(implemented, 13, "SO_LINGER is 13 and is not implemented");
        }
    }

    /// **The socket errno numbers are Linux's `asm-generic/errno.h`**, written as literals.
    ///
    /// They are here rather than in `omni_bionic::errno` because that module's table is
    /// "constants reachable by this crate's functions" and no function in it can produce a socket
    /// failure. The values still reach the guest, so they get the same test.
    ///
    /// `EINPROGRESS` is the one worth reading twice: it is the **normal** answer to a
    /// non-blocking `connect`, not an edge case, and a guest that received any other value would
    /// treat a connection that was merely in flight as one that had failed.
    #[test]
    fn the_socket_errno_numbers_are_the_linux_values() {
        assert_eq!(ENOTSOCK, 88);
        assert_eq!(EMSGSIZE, 90);
        assert_eq!(ENOPROTOOPT, 92);
        assert_eq!(EADDRINUSE, 98);
        assert_eq!(EADDRNOTAVAIL, 99);
        assert_eq!(ENETUNREACH, 101);
        assert_eq!(ECONNABORTED, 103);
        assert_eq!(ECONNRESET, 104);
        assert_eq!(ENOBUFS, 105);
        assert_eq!(EISCONN, 106);
        assert_eq!(ENOTCONN, 107);
        assert_eq!(ECONNREFUSED, 111);
        assert_eq!(EHOSTUNREACH, 113);
        assert_eq!(EINPROGRESS, 115);
        // And none of them collides with one `omni_bionic::errno` already owns, which is what
        // would happen if this table had been written from the wrong architecture's headers.
        for socket_errno in [
            ENOTSOCK, EMSGSIZE, ENOPROTOOPT, EADDRINUSE, EADDRNOTAVAIL, ENETUNREACH,
            ECONNABORTED, ECONNRESET, ENOBUFS, EISCONN, ENOTCONN, ECONNREFUSED, EHOSTUNREACH,
            EINPROGRESS,
        ] {
            for owned in [
                consts::EAGAIN,
                consts::EINVAL,
                consts::EACCES,
                consts::EPIPE,
                consts::EINTR,
                consts::EAFNOSUPPORT,
                consts::ETIMEDOUT,
                consts::EBADF,
            ] {
                assert_ne!(socket_errno, owned, "two names for one number");
            }
        }
    }

    /// **Every classified host failure has an errno, and `Other` has none.**
    ///
    /// Membership over the whole enum rather than a count (VERIFICATION entry 1), because the
    /// failure this catches is a kind silently acquiring the *wrong* errno: the mapping is
    /// one-to-one, so two kinds sharing a number is a copy-and-paste error that nothing else can
    /// see. `Other` is the one that must stay unmapped — it is the kind `std::io::ErrorKind`
    /// could not classify, and giving it `EIO` would hand guest code an actionable branch for
    /// something nobody identified.
    #[test]
    fn every_classified_network_failure_maps_to_exactly_one_errno_and_other_maps_to_none() {
        let classified = [
            NetErrorKind::WouldBlock,
            NetErrorKind::InProgress,
            NetErrorKind::AlreadyConnected,
            NetErrorKind::NotConnected,
            NetErrorKind::ConnectionRefused,
            NetErrorKind::ConnectionReset,
            NetErrorKind::ConnectionAborted,
            NetErrorKind::AddressInUse,
            NetErrorKind::AddressNotAvailable,
            NetErrorKind::NetworkUnreachable,
            NetErrorKind::HostUnreachable,
            NetErrorKind::TimedOut,
            NetErrorKind::BrokenPipe,
            NetErrorKind::PermissionDenied,
            NetErrorKind::InvalidInput,
            NetErrorKind::AddressFamilyNotSupported,
            NetErrorKind::MessageSize,
            NetErrorKind::Interrupted,
            NetErrorKind::NoBufferSpace,
        ];
        let mut seen = Vec::new();
        for kind in classified {
            let errno = net_errno_for(kind).unwrap_or_else(|| {
                panic!("{kind} is a classified failure and has no errno")
            });
            assert!(errno > 0, "{kind} mapped to {errno}, and an errno is positive");
            assert!(!seen.contains(&errno), "{kind} shares errno {errno} with another kind");
            seen.push(errno);
        }
        assert_eq!(
            net_errno_for(NetErrorKind::Other),
            None,
            "an unclassified host failure must refuse by name rather than acquire an errno"
        );
    }

    /// **`ResolveFailure::Unclassified` has no `EAI_*` code, and the other four do.**
    ///
    /// D30 names this as the trap in as many words: a default `EAI_NONAME` for a failure nobody
    /// classified tells the guest the name does not exist — which is permanent, so the guest stops
    /// asking for ever — and nothing anywhere records that this layer invented the answer.
    ///
    /// The four that *are* mapped are asserted against bionic's own numbering, which counts **up**
    /// from 1 where glibc counts down from -1. `EAI_NONAME` is corroborated inside `omni-bionic`:
    /// row 8 of `gai_strerror_message`'s table is "Name or service not known".
    #[test]
    fn the_resolver_classes_map_to_bionics_eai_numbers_and_unclassified_maps_to_none() {
        assert_eq!(eai_for(ResolveFailure::NoSuchHost), Some(8));
        assert_eq!(eai_for(ResolveFailure::NoSuchHost), Some(net::EAI_NONAME));
        assert_eq!(eai_for(ResolveFailure::NoAddressOfFamily), Some(1));
        assert_eq!(eai_for(ResolveFailure::Transient), Some(2));
        assert_eq!(eai_for(ResolveFailure::NonRecoverable), Some(4));
        assert_eq!(
            eai_for(ResolveFailure::Unclassified),
            None,
            "D30: a measured failure path may be a diagnostic and must never be the contract"
        );
        // The four codes are distinct and each carries bionic's own message, which is what says
        // the numbering is bionic's rather than a host's.
        assert_eq!(net::gai_strerror_message(8), "Name or service not known");
        assert_eq!(net::gai_strerror_message(1), "Address family for hostname not supported");
        assert_eq!(net::gai_strerror_message(2), "Temporary failure in name resolution");
        assert_eq!(net::gai_strerror_message(4), "Non-recoverable failure in name resolution");
        // And the retryable one is the only retryable one, which is what a client acts on.
        assert!(ResolveFailure::Transient.is_retryable());
        for permanent in [
            ResolveFailure::NoSuchHost,
            ResolveFailure::NoAddressOfFamily,
            ResolveFailure::NonRecoverable,
            ResolveFailure::Unclassified,
        ] {
            assert!(!permanent.is_retryable(), "{permanent} is not worth asking again");
        }
    }

    /// **`MSG_DONTWAIT` is honoured only where it asks for something already true.**
    ///
    /// `omni_platform::net` takes the socket's own blocking mode and has no per-call flag, so the
    /// only honest reading of the flag is "make this call non-blocking" — which
    /// [`wants_nonblocking`] does by returning true. What it must not do is report a blocking call
    /// as non-blocking or the other way round.
    #[test]
    fn the_per_call_nonblocking_flag_is_the_union_of_the_two_sources() {
        assert!(wants_nonblocking(true, 0), "the socket's own flag");
        assert!(wants_nonblocking(false, MSG_DONTWAIT), "the per-call flag");
        assert!(wants_nonblocking(true, MSG_DONTWAIT), "both");
        assert!(!wants_nonblocking(false, 0), "neither");
        assert!(
            !wants_nonblocking(false, MSG_NOSIGNAL),
            "MSG_NOSIGNAL says nothing about blocking"
        );
    }

    /// **A guest-chosen transfer length never becomes a host allocation of that size.**
    ///
    /// The bound this exists for is a `read(fd, buf, SIZE_MAX)`: `count` is a `size_t` the guest
    /// supplies, and without a cap it is a request for that many bytes of host memory. A short
    /// transfer is `read`'s and `write`'s own contract on a socket, so capping one call is
    /// conforming rather than a truncation.
    #[test]
    fn a_guest_chosen_transfer_length_is_capped_rather_than_allocated() {
        assert_eq!(transfer_length(0), 0);
        assert_eq!(transfer_length(1), 1);
        assert_eq!(transfer_length(SOCKET_IO_BLOCK as u64), SOCKET_IO_BLOCK);
        assert_eq!(transfer_length(SOCKET_IO_BLOCK as u64 + 1), SOCKET_IO_BLOCK);
        assert_eq!(transfer_length(u64::MAX), SOCKET_IO_BLOCK);
        // **Against the number rather than against the constant**, because a cap read out of
        // whatever `SOCKET_IO_BLOCK` happens to be would agree with any value. A TLS record is up
        // to 16 KiB plus framing and the engine carries its own OpenSSL, so a cap below one
        // record turns every record into four calls.
        assert!(
            transfer_length(u64::MAX) >= 16 * 1024,
            "a socket transfer is capped at {} bytes, below one TLS record",
            transfer_length(u64::MAX)
        );
    }

    /// **A `struct timeval` round-trips, and a zero one is "no timeout" rather than zero.**
    ///
    /// The distinction is the one `setsockopt(SO_RCVTIMEO)` makes on a device: a zero `timeval`
    /// clears the timeout, and a caller that had it translated into a zero-length one would have
    /// every receive time out immediately.
    #[test]
    fn a_zero_timeval_clears_the_timeout_and_a_real_one_round_trips() {
        assert_eq!(timeval_bytes(None), [0u8; TIMEVAL_BYTES]);
        let bytes = timeval_bytes(Some(Duration::from_millis(2_500)));
        assert_eq!(&bytes[..8], &2i64.to_le_bytes(), "tv_sec");
        assert_eq!(&bytes[8..], &500_000i64.to_le_bytes(), "tv_usec, not milliseconds");
    }

}
