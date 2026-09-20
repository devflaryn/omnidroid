//! OS primitives for Omnidroid.
//!
//! This is the **only** crate in the workspace permitted to use `#[cfg(target_os = …)]` or to call
//! an operating-system API (Global Constraint 4). Everything else — `omni-mem`, `omni-apk`,
//! `omni-elf`, `omni-cpu`, `omni-android`, `omni-gfx`, `omni-core`, `omni-cli` — compiles for all
//! five targets with no `cfg` at all, and reaches the OS only through the seams defined here.
//!
//! Other crates depending on `omni-platform` is the seam working as intended, not a violation of
//! that constraint; the constraint forbids *external* OS crates (`windows-sys`, `libc`, …) outside
//! this one.
//!
//! # What is here
//!
//! * [`vm`] — virtual memory: reservation, lazy commit, decommit, protection, placeholder
//!   splitting and file-backed mapping. Implemented and measured on Windows; structural on Linux
//!   and macOS, where every operation returns a typed
//!   [`Unsupported`](vm::VmError::Unsupported) error.
//! * [`fault`] — guest memory faults: one process-wide **vectored** exception handler, so that
//!   Omnidroid sees an access violation in JIT-generated guest code before dynarmic's frame-based
//!   SEH does. D4 verified the ordering (`veh_hits = 1`, dynarmic's slow path never entered) and
//!   D10 requires it, because whoever handles the fault owns guest demand paging.
//! * [`clock`] — monotonic time, wall time and sleeping. One process-wide monotonic epoch.
//! * [`process`] — pid, cpu count, entropy and the current processor number. Implemented and run
//!   on Windows; the entropy and cpu-id halves are structural on Linux and macOS, where they
//!   return [`ProcessError::Unsupported`](process::ProcessError::Unsupported) naming the POSIX
//!   call they intend to make.
//! * [`log`] — a sink for a line the guest wrote, with Android's and syslog's priority scales.
//!
//! Files, directories, sockets, threads, dynamic loading and windowing will arrive as sibling
//! modules in later tasks.
//!
//! # Not every primitive needs a `cfg`, and saying which is part of the seam
//!
//! [`vm`] and [`fault`] are OS APIs end to end, so both have a Windows backend and a structural
//! unix one. [`clock`], [`log`], and half of [`process`] are **portable standard library** —
//! `Instant`, `SystemTime`, `thread::sleep`, `stderr`, `process::id`, `available_parallelism` —
//! and they are implemented once, with no backend and no `cfg`.
//!
//! That asymmetry is deliberate and is written out in each module. The five-target rule this
//! project enforces is *never claim a platform works*, and a fabricated `Unsupported` return for
//! something `std` already does correctly on all five targets would be a false claim in the other
//! direction: it would assert that a clock this process can read cannot be read, and it would make
//! the non-Windows bring-up harder rather than easier. What stays unclaimed is what has been
//! **run**: nothing outside Windows x86-64 has been.
//!
//! # Diagnostics are part of the API
//!
//! [`vm::process_commit_charge`] and [`vm::process_working_set`] are public because commit charge
//! is the resource that limits how many guest instances fit on a machine (D10), and because tests
//! in other crates must be able to *assert* what a memory operation cost rather than assume it
//! (Global Constraint 6).

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod clock;
pub mod fault;
pub mod log;
pub mod process;
pub mod vm;
