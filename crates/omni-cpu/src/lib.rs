//! The [`GuestCpu`] trait: one guest thread's ARM64 CPU, behind an interface the rest of the
//! runtime can hold without knowing which backend it has.
//!
//! No backend yet. This crate is the seam, and the seam is the load-bearing part.
//!
//! # What the seam has to survive
//!
//! `ARCHITECTURE.md` section 6 gives it two implementations that could hardly be less alike:
//!
//! * **On an ARM64 host** (Linux ARM64, macOS ARM64) guest code runs **natively**. There is no
//!   translation at all: the loader maps the code executable and calls it, and the thunk boundary
//!   reduces to an ABI-compatible call. This is the reason the abstraction exists.
//! * **On an x86-64 host** the guest is binary-translated (D5: dynarmic, pinned as a fork,
//!   `yuzu-mirror/dynarmic@9d45823`).
//!
//! So the trait must not have a translator's shape. Three places it would have acquired one, and
//! what each became instead:
//!
//! | The translating shape | What it is here |
//! |---|---|
//! | the context is handed a `CodeArena` to emit into | the arena belongs to [`GuestCpuBackend`]'s implementation and never appears in this crate |
//! | `run(pc, max_instructions)` — a counter the translator inserts | [`RunLimit`] is *offered* and may be refused, and [`HaltHandle`] is the mechanism both backends have |
//! | `invalidate_translated_code(range) -> blocks_flushed` | [`GuestCpu::invalidate_code`], stated as *the guest changed these bytes*, which a native backend answers with `IC IVAU` |
//!
//! # What the seam has to guarantee
//!
//! * **`TPIDR_EL0` is mandatory** (D13). `libroblox.so` holds 1,282 `MRS Xt, TPIDR_EL0`
//!   instructions, 1,276 of which immediately load `[Xt, #0x28]` — bionic's `TLS_SLOT_STACK_GUARD`
//!   — and the first of them runs before `JNI_OnLoad` and before the first of the 3,594 static
//!   initializers. A [`GuestThreadConfig`] cannot be built without one, so a backend cannot be
//!   handed a thread that has not had one programmed.
//! * **Guest code is untrusted** (Global Constraint 11). It will branch to unmapped addresses,
//!   execute garbage and recurse without end. [`ExitReason`] gives each of those a typed stop
//!   rather than a host crash, and [`HaltHandle`] bounds the ones that never stop on their own.
//! * **Per-thread cost is measured, not assumed** (D5: 20-35 MiB of unshared code cache per guest
//!   thread; D15: a pagefile-backed section is invisible to `process_commit_charge`).
//!   [`ContextCost`] keeps those two halves apart and [`GuestCpuBackend::shared_cost`] keeps shared
//!   memory from being counted once per thread.
//!
//! # What is deliberately not here
//!
//! No build script, and no C or C++ dependency (preflight ruling P1). The dynarmic FFI lives in
//! `dynarmic-sys`. This crate compiles on a host with no C++ toolchain at all — which is not tidiness
//! but the same portability argument as everything above: the ARM64-native path needs no translator,
//! so it must not need a translator's build.

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod clock;
mod context;
mod cpu;
mod error;
mod exit;
#[cfg(all(feature = "dynarmic", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub mod dynarmic;
mod fastmem;
mod regs;
pub mod run;
mod thunk;
mod tls;

pub use clock::{cntpct, CNTFRQ_HZ};
pub use context::{
    ContextCost, GuestAddressSpace, GuestRange, GuestThreadConfig, TLS_SLOT_STACK_GUARD_OFFSET,
};
pub use cpu::{Capabilities, GuestCpu, GuestCpuBackend, HaltHandle, InlineThunkCounts};
pub use error::{CpuError, CpuResult};
pub use fastmem::{
    identity_mapping, pc_is_representable, require_identity_mapping, truncate_pc, MemoryMapping,
    GUEST_PC_BITS,
};
pub use tls::{
    GuestTls, TlsArena, TlsSlot, TLS_BLOCK_BYTES, TLS_CONTROL_BLOCK_BYTES, TLS_SLOT_COUNT,
};
pub use exit::{AccessKind, ExitReason, RunLimit};
pub use thunk::{ThunkCall, ThunkContext, ThunkFn, ThunkRegs};
pub use regs::{Nzcv, VReg, XReg};

/// Re-exported so that naming a guest address does not require depending on `omni-mem` directly.
pub use omni_mem::GuestAddr;
