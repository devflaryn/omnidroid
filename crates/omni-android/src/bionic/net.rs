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
//! # Why `poll` and `select` need no operating system, stated as a closed argument
//!
//! Not "they are easy", and not "nothing polls during static initialisation". The argument is
//! that **the descriptor space they observe is entirely this runtime's own, and POSIX fixes the
//! answer for every kind in it**:
//!
//! 1. The only bound symbols that produce a descriptor are `open`, `__open_2` and `opendir`, plus
//!    `fileno` handing back one of those or one of the three standard streams. `socket` and
//!    `eventfd` — the two symbols in the reachable 188 that would introduce a *different* kind of
//!    descriptor — refuse. `pipe`, `socketpair`, `epoll_create`, `timerfd_create`, `signalfd`,
//!    `inotify_init` and `dup` are not in the 188 at all.
//! 2. So every descriptor that exists is a regular file, a directory, or one of stdin, stdout and
//!    stderr, and **none of them can block**: `omni-platform`'s `read` on a standard stream is an
//!    immediate end of file, its `write` to one is an immediate host write, and a regular file is
//!    always ready by definition.
//! 3. Linux reports exactly `POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM` for a regular file — its
//!    `DEFAULT_POLLMASK` — regardless of the descriptor's access mode, which is why a read-only
//!    file still answers `POLLOUT` there and here.
//!
//! The consistency criterion is the one that matters: **`poll`'s answer predicts what `read` and
//! `write` on that descriptor will actually do in this runtime**, not what they would do on a
//! device. `the_descriptor_space_poll_answers_over_is_closed` asserts fact 1 mechanically, so the
//! paragraph to invalidate is a test rather than a sentence — the day a phase binds `socket` for
//! real, that test fails and this module has to grow a real readiness source with it.
//!
//! # What they do when nothing is ready, and the one thing they refuse
//!
//! A descriptor that can never become ready makes an *infinite* wait a permanent hang of a host
//! thread, and D16's runaway-guest defence is built from step budgets that a sleeping thread does
//! not consume. So `poll(fds, n, -1)` and `select(.., NULL)` with nothing ready are **refused by
//! name**, with the same argument [`MAX_SLEEP_SECONDS`](super::MAX_SLEEP_SECONDS) makes for
//! `nanosleep` — and a finite timeout past that cap is refused rather than clamped, because a
//! clamp returns `0` from a call that waited a minute when it was asked to wait a year.
//!
//! A finite timeout with nothing ready is a real sleep and a real `0`, which is what `poll`
//! promises.
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

use std::time::Duration;

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_bionic::net;
use omni_mem::GuestAddr;

use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::files::filesystem;
use super::view::GuestView;
use super::{active, enter, MAX_SLEEP_SECONDS};

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

/// What an always-ready descriptor answers, masked by what was asked for.
///
/// Linux's `DEFAULT_POLLMASK`, which is what its `poll` returns for a regular file. `POLLPRI` is
/// deliberately absent: out-of-band data is a socket concept and nothing here has any.
const READY_MASK: i16 = POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM;

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

/// What a `poll` or `select` call decided to do, computed while the guest view is alive and
/// carried out after it is dropped.
///
/// A type rather than an early `return`, because [`ImportCall::args`] borrows the call shared and
/// [`ImportCall::ret`] borrows it uniquely, so a handler cannot write its return value while a
/// [`GuestView`] is alive. It also means a handler cannot decide to wait and then forget to.
enum Outcome {
    /// Return this value now.
    Value(i32),
    /// Sleep, then return zero: the timeout expired with nothing ready.
    Sleep(Duration),
}

impl Outcome {
    fn perform(self) -> i32 {
        match self {
            Outcome::Value(value) => value,
            Outcome::Sleep(duration) => {
                omni_platform::clock::sleep(duration);
                0
            }
        }
    }
}

/// `int poll(struct pollfd *fds, nfds_t nfds, int timeout)`
pub(super) fn poll(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fds, nfds, timeout) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let outcome = {
        let mut view = enter(c, &state);
        if nfds > MAX_POLL_FDS {
            // Linux's own answer for an `nfds` past the process's descriptor limit.
            view.set_errno(consts::EINVAL);
            Outcome::Value(-1)
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
                Outcome::Value(ready)
            } else {
                let duration =
                    if timeout < 0 { None } else { Some(Duration::from_millis(timeout as u64)) };
                Outcome::Sleep(bounded_wait(c, duration)?)
            }
        }
    };
    let value = outcome.perform();
    c.ret().i32(value);
    Ok(())
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
        } else if fs.is_some_and(|fs| fs.is_open(fd)) {
            events & READY_MASK
        } else {
            // Reported whether or not it was requested, which is what `POLLNVAL` is for.
            POLLNVAL
        };
        entry[6..8].copy_from_slice(&revents.to_le_bytes());
        if revents != 0 {
            ready += 1;
        }
    }
    view.mem().write_bytes(at, &entries, blame)?;
    Ok(ready)
}

/// `int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds, struct timeval *timeout)`
pub(super) fn select(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (nfds, readfds, writefds, exceptfds, timeout) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let outcome = {
        let mut view = enter(c, &state);
        select_outcome(c, &mut view, nfds, [readfds, writefds, exceptfds], timeout)?
    };
    let value = outcome.perform();
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
) -> AbiResult<Outcome> {
    if !(0..=FD_SETSIZE).contains(&nfds) {
        // Negative is `EINVAL` on Linux. Past `FD_SETSIZE` is `EINVAL` here, and this is the one
        // place the call is stricter than Linux, which clamps to the process's descriptor table
        // instead: a guest `fd_set` is 128 bytes, so honouring a larger `nfds` would read bits
        // out of whatever the guest put after it. Refusing to read past the object is a
        // rejection of the request rather than a truncation of it — the whole call fails and the
        // guest is told, which is the opposite of review finding M5's shape.
        view.set_errno(consts::EINVAL);
        return Ok(Outcome::Value(-1));
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
            return Ok(Outcome::Value(-1));
        }
    }
    // Readable and writable; never an exception. See the module documentation: every descriptor
    // in this runtime is a regular file, a directory or a standard stream, and Linux's answer for
    // all of those is ready for both.
    let ready: i32 = sets[0].count(nfds) + sets[1].count(nfds);
    sets[2].clear();
    if ready > 0 {
        for set in &sets {
            set.write_back(view)?;
        }
        return Ok(Outcome::Value(ready));
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
            return Ok(Outcome::Value(-1));
        }
        // Neither field can overflow the sum: `tv_usec` is bounded by a million and `tv_sec` by
        // the cap `bounded_wait` applies next.
        Some(Duration::from_secs(seconds as u64) + Duration::from_micros(micros as u64))
    };
    // Refused before anything is written, for the same reason.
    let wait = bounded_wait(c, duration)?;
    // Nothing is ready and the wait is going to happen, so on return every set must be empty:
    // POSIX requires the sets to be zeroed when `select` times out, and a guest that read a stale
    // bit would act on a descriptor this call did not report.
    for set in &mut sets {
        set.clear();
    }
    for set in &sets {
        set.write_back(view)?;
    }
    Ok(Outcome::Sleep(wait))
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
/// Refused. An `eventfd` is a **descriptor**, and every descriptor in this runtime belongs to
/// `omni-platform`'s rooted filesystem table — which is what `read`, `__write_chk`, `close`,
/// `fstat` and this module's `poll` are all written against. An eventfd is a 64-bit counter whose
/// `read` blocks until it is non-zero and whose `write` adds to it, and not one of those five
/// could carry that.
///
/// **A descriptor the guest can obtain but cannot use is worse than one it cannot obtain**: the
/// failure moves from this call, where it names the thing that is missing, to whichever of
/// `read`, `write`, `close` or `poll` the guest reaches next — which will report `EBADF` about a
/// descriptor this layer handed out itself.
///
/// `-1`/`ENOSYS` was rejected for the reason `socket`'s `-1` was: it says *this kernel* has no
/// `eventfd`, which is a fact about a kernel rather than about this layer, and a guest that
/// believes it has no eventfd falls back to a pipe — which is not bound either.
pub(super) fn eventfd(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (initval, flags) = {
        let mut a = c.args();
        (a.next_i32()? as u32, a.next_i32()?)
    };
    Err(refuse(
        c,
        format!(
            "the guest called eventfd({initval}, {flags:#x}), which must return a descriptor. \
             Every descriptor in this runtime is one of `omni-platform`'s rooted filesystem \
             table's, and `read`, `__write_chk`, `close`, `fstat` and `poll` are all written \
             against that table; an eventfd is a counter with blocking reads and none of the \
             five could carry one. A descriptor the guest can obtain and then cannot read, \
             write, close or poll is worse than one it cannot obtain, because the failure moves \
             to whichever call it reaches next and reports EBADF about a descriptor this layer \
             issued"
        ),
    ))
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
