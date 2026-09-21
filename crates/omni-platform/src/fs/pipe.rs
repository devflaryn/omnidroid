//! `pipe(2)`: an in-process byte queue with two ends, and the readiness rules `poll` needs.
//!
//! # Why a pipe is in the filesystem seam, and why it has no backend
//!
//! It is here because [`poll`] and [`select`] observe **one** descriptor table. A pipe that lived
//! in its own namespace would need its own descriptor allocator, and two allocators can hand out
//! the same number — which is not a wrong answer until the day a guest closes the wrong one.
//!
//! It has no backend because there is **no operating system call to make**. Both ends of every
//! pipe this runtime creates belong to the same guest, so the bytes never leave this process: a
//! pipe here is a `VecDeque<u8>`, a reader count, a writer count and a condition variable. Per
//! D22's other half, it therefore gets **no fabricated `Unsupported` arm** for Linux or macOS.
//! Writing one would assert that a queue this process can hold cannot be held, which is a false
//! claim in the other direction and makes the non-Windows bring-up harder rather than easier.
//!
//! That is the fourth phase running whose OS-surface prediction was too high — files needed
//! fifteen of seventeen operations to be one `std` call (D23), threads needed nothing at all
//! (D24), sockets needed nothing at all (D25), and a pipe needs nothing at all.
//!
//! # Nothing here ever waits
//!
//! **This module has no unbounded wait and no bounded one either.** A read from an empty blocking
//! pipe reports [`FsErrorKind::WouldBlock`] exactly as a non-blocking one does, and the *caller*
//! decides what to do about it. That is deliberate: D16's runaway-guest defence is built from step
//! budgets that a sleeping thread does not consume, so "how long may a guest block" is a policy
//! the adapter owns — it already owns it for `poll`, `select` and `nanosleep`, each capped by
//! `MAX_SLEEP_SECONDS` with an unbounded wait refused by name.
//!
//! What this module provides instead is [`ReadyGate`], a generation counter the caller can wait on
//! with a deadline of its own choosing. There is no API here that can block forever.
//!
//! # The four rules that decide readiness, and the one that is easy to get backwards
//!
//! | end | readable | writable | condition |
//! |---|---|---|---|
//! | read | the queue is non-empty **or every write end is closed** | never | `POLLHUP` once empty and all writers gone |
//! | write | never | a read end is open **and** the queue has room | `POLLERR` once every read end is closed |
//!
//! The load-bearing clause is *or every write end is closed*. A reader whose writers have all gone
//! must see **end of file**, and end of file is a read that returns `0` immediately — so the
//! descriptor has to report itself readable for that to be reachable through `poll`. A readiness
//! rule that said "readable iff the queue is non-empty" would leave such a reader blocked forever
//! on a pipe that can never produce another byte, and every count-based assertion about it would
//! still pass. `VERIFICATION.md` entry 11 is that shape exactly.
//!
//! [`poll`]: https://docs.rs/omni-android
//! [`select`]: https://docs.rs/omni-android

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{FsError, FsErrorKind, FsResult};

/// How many bytes a pipe holds before a write to it blocks.
///
/// **A policy number with a provenance, not a derivation.** 65,536 is Linux's default pipe
/// capacity — `/proc/sys/fs/pipe-max-size`'s starting point and what `fcntl(F_GETPIPE_SZ)` reports
/// on a fresh pipe — so a guest sized against a device sees the number it expects.
///
/// Nothing on the startup path can reach it: the `android_native_app_glue` writes **one byte** per
/// `APP_CMD_*` message and the game thread drains them (`jni-surface.md` §5.2). A capacity that
/// differs from a device's is therefore not a wrong *answer* until something fills it, and the day
/// something does, it is the guest's own back-pressure behaviour that changes rather than a value
/// it reads.
pub const PIPE_CAPACITY: usize = 65_536;

/// How many bytes a single write to a pipe is atomic up to.
///
/// POSIX fixes this at 512 minimum and Linux uses 4,096. It is not a capacity: it is the bound
/// **below which** a write either happens entirely or not at all when several writers share a
/// pipe. Above it a write may be split, which is why a pipe write reports how many bytes it
/// took rather than promising to take them all.
pub const PIPE_BUF: usize = 4_096;

/// Which end of a pipe a descriptor holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeEnd {
    /// The read end: `pipefd[0]`.
    Read,
    /// The write end: `pipefd[1]`.
    Write,
}

impl PipeEnd {
    /// A short name, for a message that has to say which end refused.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            PipeEnd::Read => "the read end",
            PipeEnd::Write => "the write end",
        }
    }
}

/// What a descriptor would do right now, as `poll` and `select` ask it.
///
/// Every descriptor kind in this seam answers one of these, which is what makes the readiness
/// rules a **total function over the table** rather than a rule about pipes with an
/// everything-else default. A fifth `Entry` kind added later cannot compile without deciding its
/// answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    /// A read would return without blocking — including a read that returns end of file.
    pub readable: bool,
    /// A write would take at least one byte without blocking.
    pub writable: bool,
    /// The peer has gone: `POLLHUP` on a read end whose writers have all closed.
    pub hangup: bool,
    /// The call cannot succeed at all: `POLLERR` on a write end whose readers have all closed.
    pub error: bool,
}

impl Readiness {
    /// What a descriptor that can never block answers: a regular file, a directory, a character
    /// device or a standard stream.
    ///
    /// **Not a default.** Linux reports exactly `POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM` for a
    /// regular file — its `DEFAULT_POLLMASK` — regardless of the access mode the descriptor was
    /// opened with, which is why a read-only file still answers writable there and here.
    pub const ALWAYS: Readiness =
        Readiness { readable: true, writable: true, hangup: false, error: false };
}

/// A generation counter that rises whenever any pipe in one filesystem changes state.
///
/// **The whole of how a caller waits for readiness**, and the reason nothing in this module can
/// block forever: [`ReadyGate::wait`] takes a deadline from its caller, and there is no overload
/// that does not.
///
/// One per [`Filesystem`](super::Filesystem) rather than one per pipe, so that a caller watching
/// several descriptors — which is exactly what `ALooper_pollOnce` and `poll` do — waits on one
/// condition variable rather than on a set of them. The cost is a wakeup for a pipe the caller was
/// not watching; the alternative is a per-descriptor condvar and a thread per descriptor, which is
/// how a poll loop becomes a thread pool.
#[derive(Debug, Default)]
pub struct ReadyGate {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl ReadyGate {
    /// The current generation. A caller reads this **before** testing readiness, so that a change
    /// arriving between the test and the wait cannot be missed.
    #[must_use]
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record that something changed, and wake everyone waiting.
    fn bump(&self) {
        {
            let mut generation =
                self.generation.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *generation = generation.wrapping_add(1);
        }
        self.changed.notify_all();
    }

    /// Wait until the generation differs from `seen`, or until `timeout` elapses.
    ///
    /// Returns whether the generation changed. A `timeout` of zero returns immediately, which is
    /// what a caller polling with a zero timeout wants.
    ///
    /// **The caller supplies the bound.** This module has no opinion about how long a guest may
    /// block, because that is D16's question and the adapter above already answers it for `poll`,
    /// `select` and `nanosleep`.
    pub fn wait(&self, seen: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut generation =
            self.generation.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *generation == seen {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self
                .changed
                .wait_timeout(generation, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            generation = next;
        }
        true
    }
}

/// The shared state of one pipe: the bytes in flight and how many descriptors hold each end.
#[derive(Debug)]
struct PipeState {
    queue: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

/// One pipe, as both of its ends see it.
#[derive(Debug)]
pub struct Pipe {
    state: Mutex<PipeState>,
    gate: Arc<ReadyGate>,
}

impl Pipe {
    /// Create a pipe with one read end and one write end already accounted for.
    fn new(gate: Arc<ReadyGate>) -> Pipe {
        Pipe {
            state: Mutex::new(PipeState {
                queue: VecDeque::new(),
                readers: 1,
                writers: 1,
            }),
            gate,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PipeState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many bytes are waiting to be read. Diagnostic; the guest cannot ask.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.state().queue.len()
    }

    /// Readiness of one end of this pipe, by the table in this module's documentation.
    fn readiness(&self, end: PipeEnd) -> Readiness {
        let state = self.state();
        match end {
            PipeEnd::Read => {
                let empty = state.queue.is_empty();
                let hangup = empty && state.writers == 0;
                Readiness {
                    // The clause that must not be dropped: end of file is a read that returns
                    // immediately, so a reader with no writers left is readable.
                    readable: !empty || state.writers == 0,
                    writable: false,
                    hangup,
                    error: false,
                }
            }
            PipeEnd::Write => Readiness {
                readable: false,
                writable: state.readers > 0 && state.queue.len() < PIPE_CAPACITY,
                hangup: false,
                error: state.readers == 0,
            },
        }
    }

    /// Take up to `buf.len()` bytes out of the queue.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::WouldBlock`] when the queue is empty and a write end is still open. That is
    /// the answer for a blocking descriptor as much as for a non-blocking one: see this module's
    /// documentation for why the wait belongs to the caller.
    fn read(&self, buf: &mut [u8]) -> FsResult<usize> {
        let mut state = self.state();
        if state.queue.is_empty() {
            if state.writers == 0 {
                // End of file, and it stays end of file: every later read answers zero too.
                return Ok(0);
            }
            return Err(FsError::kinded(
                "read",
                "a pipe",
                FsErrorKind::WouldBlock,
                "the pipe is empty and a write end is still open",
            ));
        }
        let taken = buf.len().min(state.queue.len());
        for slot in buf.iter_mut().take(taken) {
            *slot = state.queue.pop_front().expect("the queue holds at least `taken` bytes");
        }
        drop(state);
        // A reader making room is a writability change, so the gate rises for it too.
        self.gate.bump();
        Ok(taken)
    }

    /// Put as many of `buf`'s bytes into the queue as fit, and report how many that was.
    ///
    /// **A short write is the contract, not a failure.** POSIX guarantees atomicity only up to
    /// [`PIPE_BUF`]; past it a write may be split, and a caller that ignores the returned count
    /// is wrong on a device too. Returning `WouldBlock` because the *whole* buffer does not fit,
    /// when some of it does, is the believable wrong answer this shape invites — it would make a
    /// guest writing a large buffer spin forever against a pipe that was draining.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::WouldBlock`] when the queue is full and a read end is open, and
    /// [`FsErrorKind::BrokenPipe`] when every read end has closed.
    fn write(&self, buf: &[u8]) -> FsResult<usize> {
        let mut state = self.state();
        if state.readers == 0 {
            // `EPIPE`, and on a device `SIGPIPE` as well. There is no signal delivery here (D24),
            // so the errno is the whole of what the guest gets and the message says so.
            return Err(FsError::kinded(
                "write",
                "a pipe",
                FsErrorKind::BrokenPipe,
                "every read end of the pipe is closed. A device would also raise SIGPIPE, which \
                 this runtime does not deliver",
            ));
        }
        let room = PIPE_CAPACITY - state.queue.len();
        if room == 0 {
            return Err(FsError::kinded(
                "write",
                "a pipe",
                FsErrorKind::WouldBlock,
                format!("the pipe already holds its whole capacity of {PIPE_CAPACITY} bytes"),
            ));
        }
        if buf.is_empty() {
            // POSIX: a zero-byte write to a pipe has no effect and returns zero. It must not
            // bump the gate, because nothing changed.
            return Ok(0);
        }
        let taken = buf.len().min(room);
        state.queue.extend(&buf[..taken]);
        drop(state);
        self.gate.bump();
        Ok(taken)
    }
}

/// One descriptor's hold on one end of a pipe.
///
/// **The reference counts are maintained by `Drop`, not by `close`.** A count that `close`
/// decremented would be wrong for every other way an entry can go away — the table being dropped
/// with the instance, a future `dup2` replacing an entry — and "every write end is closed" is the
/// clause end-of-file depends on. Making it a property of the type means there is no path that
/// forgets.
#[derive(Debug)]
pub struct PipeHandle {
    pipe: Arc<Pipe>,
    end: PipeEnd,
    nonblocking: bool,
}

impl PipeHandle {
    /// Which end this descriptor holds.
    #[must_use]
    pub fn end(&self) -> PipeEnd {
        self.end
    }

    /// Whether `O_NONBLOCK` is set on this descriptor.
    #[must_use]
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    /// Set or clear `O_NONBLOCK`.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        self.nonblocking = nonblocking;
    }

    /// The pipe both ends share, for a caller that needs to look at it.
    #[must_use]
    pub fn pipe(&self) -> &Arc<Pipe> {
        &self.pipe
    }

    /// This end's readiness.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.pipe.readiness(self.end)
    }

    /// `read(2)` on this end.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] on the write end, and whatever the pipe's own read reports.
    pub fn read(&self, buf: &mut [u8]) -> FsResult<usize> {
        if self.end != PipeEnd::Read {
            return Err(FsError::kinded(
                "read",
                "a pipe",
                FsErrorKind::BadDescriptor,
                "the write end of a pipe is not open for reading",
            ));
        }
        self.pipe.read(buf)
    }

    /// `write(2)` on this end.
    ///
    /// # Errors
    ///
    /// [`FsErrorKind::BadDescriptor`] on the read end, and whatever the pipe's own write reports.
    pub fn write(&self, buf: &[u8]) -> FsResult<usize> {
        if self.end != PipeEnd::Write {
            return Err(FsError::kinded(
                "write",
                "a pipe",
                FsErrorKind::BadDescriptor,
                "the read end of a pipe is not open for writing",
            ));
        }
        self.pipe.write(buf)
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        {
            let mut state = self.pipe.state();
            match self.end {
                PipeEnd::Read => state.readers = state.readers.saturating_sub(1),
                PipeEnd::Write => state.writers = state.writers.saturating_sub(1),
            }
        }
        // Closing an end is a readiness change for the other one: the last writer going away
        // makes the reader readable at end of file, and the last reader going away makes the
        // writer report `POLLERR`. A close that did not wake the gate would leave a waiter
        // parked on a pipe that can never change again.
        self.pipe.gate.bump();
    }
}

/// Create a pipe and return its two handles, read end first — the order `pipefd[2]` has.
#[must_use]
pub fn create(gate: Arc<ReadyGate>) -> (PipeHandle, PipeHandle) {
    let pipe = Arc::new(Pipe::new(gate));
    (
        PipeHandle { pipe: Arc::clone(&pipe), end: PipeEnd::Read, nonblocking: false },
        PipeHandle { pipe, end: PipeEnd::Write, nonblocking: false },
    )
}
