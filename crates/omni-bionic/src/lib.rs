//! Host-side implementations of the **pure** subset of bionic (Android libc/libm) functions
//! that `libroblox.so` imports: functions that need no operating-system access and are pure
//! computation over guest memory.
//!
//! Everything here is written against the [`memory::GuestMemory`] trait, never against a
//! concrete emulator: a thin adapter written later connects these functions to the thunk
//! boundary in `omni-android`. If that boundary changes under review, this crate does not.
//!
//! Design rules (see `docs/research/bionic-pure-report.md` for the full argument):
//!
//! * **Guest pointers are untrusted.** Every access goes through [`memory::GuestMemory`] and
//!   can fail. A failure is a returned error — never a host panic, never a crash.
//! * **Address arithmetic is checked everywhere.** A guest range that overflows `u64` is a
//!   fault, not a wraparound.
//! * **The guest ABI is Android arm64 (LP64)**, not the host's: `long` is 64-bit, `wchar_t`
//!   is 32-bit, and `errno` numbers are Linux values. The host C library is never an oracle
//!   for anything those types touch.
//! * **No plausible stubs.** A function that cannot be implemented correctly returns an error
//!   naming itself, never a believable wrong answer.
//! * **No OS access.** The crate has zero dependencies, no `#[cfg(target_os)]`, and compiles
//!   unchanged on every host.
//!
//! Scanning loops are designed so that each iteration reads at least one byte through the
//! memory trait; every scan therefore terminates at a terminator or a fault, and no test run
//! can hang on an unterminated guest string.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod atomics;
pub mod atexit;
pub mod cond;
pub mod context;
pub mod ctype;
pub mod errno;
pub mod error;
pub mod guestcmp;
pub mod guard;
pub mod layouts;
pub mod libm;
pub mod locale;
pub mod mem;
pub mod memory;
pub mod metadata;
pub mod mock;
pub mod mock_threads;
pub mod mutex;
pub mod numerics;
pub mod once;
pub mod shared_mem;
pub mod printf;
pub mod rwlock;
pub mod sem;
pub mod sort;
pub mod string;
pub mod threads;
pub mod tls;
pub mod wide;

pub use context::GuestContext;
pub use error::BionicError;
pub use memory::{Fault, GuestMemory};
