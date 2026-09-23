//! Linux backend for the process seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, where every operation returns
//! [`ProcessError::Unsupported`](super::ProcessError::Unsupported) naming the POSIX call it
//! intends to make. See that module for what implementing each one involves.
//!
//! Linux-specific notes for whoever implements it:
//!
//! * `getrandom(2)` needs Linux 3.17. On older kernels the fallback is reading `/dev/urandom`,
//!   which needs a file descriptor held open from before any `chroot` — a lifetime question this
//!   seam has no opinion about yet.
//! * `sched_getcpu(3)` is a glibc/bionic wrapper over the `getcpu` vDSO entry. It is present in
//!   `libc` on Linux and is the one call in this module that maps one-to-one onto the guest symbol
//!   it serves, because the guest symbol *is* this call.
//! * `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` is the whole of `cpu_time` here and needs no
//!   `/proc` read. It is one call and cannot return short, so this body is three lines — unlike
//!   `getrandom`. `times(2)` is the older spelling and reports in clock ticks (`_SC_CLK_TCK`,
//!   100 Hz on most configurations), which is *coarser than the guest symbol it serves*: the
//!   guest's `clock()` wants microseconds. Reaching for it would lose two decimal digits
//!   silently.
//! * The guest is an Android ARM64 binary and this host would be a Linux ARM64 one, which is the
//!   configuration `ARCHITECTURE.md` section 6 runs guest code **natively** on. That makes Linux
//!   ARM64 the target where these two answers are most nearly the guest's own — and the one where
//!   a wrong cpu id would be hardest to notice, because the guest's `sched_getcpu` and the host's
//!   would be describing the same physical core.

pub(super) use super::unix::{
    cpu_time, current_cpu, current_thread_host_priority, random_bytes, set_current_thread_nice,
};
