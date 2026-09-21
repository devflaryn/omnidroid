//! The Android compatibility layer: bionic libc/libm, `libdl`, `liblog`, JNI without a JVM
//! (D7), GameActivity, `ALooper`, `AAssetManager`, `ANativeWindow`.
//!
//! **M3 task 2 built the boundary, not the implementations.** What is here is the crossing: a reserved
//! region of guest address space, one slot per imported symbol, AAPCS64 marshalling in both
//! directions, and the machinery for calling guest code back. The 170 functions and 18 data objects
//! the 3,594 static initializers reach are task 3's.
//!
//! # The shape of the crossing
//!
//! ```text
//!   guest BL ──► thunk slot ──► inline handler  ──► host Rust        ≈33 ns   (D17)
//!                    │
//!                    └────────► ExitReason::Thunk ──► host Rust ──► guest code   80-105 ns
//! ```
//!
//! The upper path is every pure-host import. The lower path is the two things the upper one
//! structurally cannot do: run guest code, and report a typed error. D17 measured the ratio at **3x**
//! and chose the split; [`boundary`] is where the type system enforces it.
//!
//! # What crosses, and what it costs to get wrong
//!
//! | Module | What it owns |
//! |---|---|
//! | [`region`] | the reserved guest addresses, and why a slot is 16 bytes rather than 4 |
//! | [`abi`] | AAPCS64's three counters, and the indirect result register `mallinfo` needs |
//! | [`varargs`] | the variadic rules, which are not the fixed rules, and the guest `va_list` walk |
//! | [`mem`] | every guest pointer, checked, because the guest is hostile by assumption |
//! | [`boundary`] | the symbol table, the two dispatch paths, and host-to-guest re-entry |
//! | [`error`] | one typed refusal per failure, each naming the symbol and the guest address |
//!
//! Three of those exist because of a specific way to be silently wrong, and each is stated where it
//! lives: a variadic `float` read as a `float` rather than as a promoted `double` returns `0.0`; a
//! `va_list` walked in 8-byte steps through the SIMD save area returns `0.0`; and an `int` return
//! zero-extended instead of sign-extended turns every libc `-1` into success.
//!
//! # Portability
//!
//! No `cfg`, no target gate, and no dependency on any CPU backend — `ARCHITECTURE.md` section 2's
//! rule for every crate that is not `omni-platform` or a named backend. That is why the boundary is
//! written against `omni-cpu`'s `ThunkCall`/`GuestCpu` and never against the translating backend's
//! types, and it is what keeps the ARM64-native path (section 6, D5) expressible: there, the slot
//! holds a real veneer, [`GuestCpu::add_inline_thunk`](omni_cpu::GuestCpu::add_inline_thunk) plants it,
//! and nothing in this crate changes. **That path is not tested and is not claimed to work.**

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod abi;
pub mod bionic;
pub mod boundary;
pub mod error;
pub mod jni;
pub mod mem;
pub mod region;
pub mod varargs;

pub use bionic::{Activation, Bionic};
pub use jni::{Jni, JniActivation};
pub use abi::{ArgRegs, ArgSource, Args, Ret, RetSink, ARG_REGISTERS};
pub use boundary::{
    Binding, Boundary, BoundaryBuilder, CodeInvalidations, ContextRegistration, Crossings,
    GuestArg, GuestReturn,
    ImportCall, ImportFn, ReentrantCall, ReentrantFn, Slot, MAX_EXIT_CROSSINGS, MAX_GUEST_DEPTH,
    MAX_PENDING_INVALIDATIONS,
};
pub use error::{AbiError, AbiResult, RefusalText};
pub use mem::{Blame, GuestMem};
pub use region::{ThunkRegion, SLOT_BYTES};
pub use varargs::{GuestVaList, VarArgs, GR_SLOT, VA_LIST_BYTES, VR_SLOT};
