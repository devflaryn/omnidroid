//! macOS backend for the process seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, where every operation returns
//! [`ProcessError::Unsupported`](super::ProcessError::Unsupported) naming the POSIX call it
//! intends to make.
//!
//! macOS-specific notes for whoever implements it, and a reason not to assume this is symmetric
//! with Linux:
//!
//! * `arc4random_buf(3)` is the whole of [`random_bytes`](super::random_bytes) here. It cannot
//!   fail, cannot return short and needs no file descriptor, so the macOS body is three lines
//!   while the Linux one is a loop — which is exactly why they do not share an implementation.
//! * `cpu_time` is the one entry here that is **not** asymmetric with Linux: macOS has had
//!   `CLOCK_PROCESS_CPUTIME_ID` since 10.12, so the same `clock_gettime` call serves both and
//!   the shared `unix` body names it once. `getrusage(RUSAGE_SELF)` is the portable fallback and
//!   is microsecond-resolution, which is enough for the guest's `clock()` and not enough for
//!   `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`; the shared module says so.
//! * **There is no `sched_getcpu` on macOS and no supported replacement.** The guest symbol is
//!   expected to stay a refusal on this target rather than gain an answer. A caller that needs a
//!   shard index has to derive one from the thread identity instead, and that is a decision for
//!   whoever first needs it, not something to guess at here.

pub(super) use super::unix::{
    cpu_time, current_cpu, current_thread_host_priority, host_manufacturer, random_bytes,
    set_current_thread_nice,
};
