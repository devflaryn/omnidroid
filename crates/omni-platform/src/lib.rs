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
//!
//! Threads, clocks, dynamic loading and windowing will arrive as sibling modules in later tasks.
//!
//! # Diagnostics are part of the API
//!
//! [`vm::process_commit_charge`] and [`vm::process_working_set`] are public because commit charge
//! is the resource that limits how many guest instances fit on a machine (D10), and because tests
//! in other crates must be able to *assert* what a memory operation cost rather than assume it
//! (Global Constraint 6).

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod vm;
