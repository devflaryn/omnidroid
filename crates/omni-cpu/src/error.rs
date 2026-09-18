//! Typed, diagnostic failures (Global Constraint 7).
//!
//! Every variant names the value it failed on. There is no catch-all: the closest thing,
//! [`CpuError::Backend`], still names the backend, the operation and the detail, because the one
//! failure mode this milestone cannot afford is "the CPU stopped and we do not know why".

use omni_mem::{GuestAddr, MemError};

/// Result alias for every operation in this crate.
pub type CpuResult<T> = Result<T, CpuError>;

/// Everything a [`GuestCpu`](crate::GuestCpu) or its factory can refuse.
#[derive(Debug, thiserror::Error)]
pub enum CpuError {
    /// A register index named no register.
    ///
    /// `X31` is the case that matters: AArch64 has no `X31`, and encoding 31 means `XZR` or `SP`
    /// depending on the instruction. See [`XReg`](crate::XReg).
    #[error("there is no {class}{index}: this register file has {count} registers, {class}0 upwards")]
    NoSuchRegister {
        /// Which register file: `"X"` or `"V"`.
        class: &'static str,
        /// The index that was asked for.
        index: u32,
        /// How many registers that file has.
        count: u32,
    },

    /// A guest address range was empty, or ran past the end of the address space.
    #[error("guest range {start:#x}+{len:#x} is not usable: {reason}")]
    InvalidRange {
        /// Start of the range.
        start: GuestAddr,
        /// Length of the range.
        len: usize,
        /// Why it cannot be used.
        reason: &'static str,
    },

    /// A guest address space descriptor could not be built.
    #[error("guest address space {base:#x}+{len:#x} is not usable: {reason}")]
    InvalidAddressSpace {
        /// Base of the space.
        base: GuestAddr,
        /// Length of the space.
        len: usize,
        /// Why it cannot be used.
        reason: &'static str,
    },

    /// A guest thread was configured without a usable `TPIDR_EL0`.
    ///
    /// D13, and the reason this is a refusal rather than a warning: `libroblox.so` holds **1,282**
    /// `MRS Xt, TPIDR_EL0` instructions and **1,276** of them immediately load `[Xt, #0x28]` —
    /// bionic's `TLS_SLOT_STACK_GUARD`. Every stack-protected function in the engine reads the
    /// thread pointer directly, and the first one runs *before* `JNI_OnLoad` and before the first of
    /// the 3,594 static initializers. A context created with a null or out-of-space thread pointer
    /// would fault on its first stack-protected call, with a symptom that looks like a loader bug.
    /// So a thread cannot be configured without one.
    #[error(
        "TPIDR_EL0 is {tpidr_el0:#x}, which is not a bionic TLS block inside the guest address \
         space {base:#x}+{len:#x}: {reason}. Every stack-protected guest function reads the thread \
         pointer directly and dereferences [TPIDR_EL0, #0x28] (D13), so this must be programmed \
         before any guest code runs"
    )]
    MissingThreadPointer {
        /// The thread pointer that was offered.
        tpidr_el0: GuestAddr,
        /// Base of the guest address space it must lie in.
        base: GuestAddr,
        /// Length of that space.
        len: usize,
        /// Why it is not usable.
        reason: &'static str,
    },

    /// The guest memory path is not D4's identity mapping, and every one of these failures
    /// produces **correct results**.
    ///
    /// That is what makes it a refusal instead of a warning. The setting that decides it,
    /// `fastmem_address_space_bits`, defaults to 36 and silently degrades a high guest address onto
    /// the callback path, measured at **30-49x** slower through this runtime's own callbacks
    /// (n = 31, two loop shapes, both degraded configurations). There is no functional test that
    /// can see a 30x slowdown that produces the right answer, so this check, run once per context
    /// before any guest code, is the entire defence. See
    /// [`require_identity_mapping`](crate::require_identity_mapping).
    ///
    /// `consequence` is carried because the other four fields do not explain why anyone should
    /// care: "36 instead of 64" is not actionable, and "every guest address above the limit
    /// silently takes a path measured 30-49x slower" is.
    #[error(
        "the guest memory path is misconfigured: {setting} is {actual} but D4 requires \
         {expected}. {consequence}"
    )]
    MisconfiguredMemoryPath {
        /// Which setting is wrong, in Omnidroid's vocabulary rather than a backend's.
        setting: &'static str,
        /// What D4 requires.
        expected: u64,
        /// What the backend reported it is actually configured with.
        actual: u64,
        /// What running anyway would cost. This is the field that makes the error act on anyone.
        consequence: &'static str,
    },

    /// The guest memory path was D4's at startup and **stopped being** D4's while running.
    ///
    /// [`MisconfiguredMemoryPath`](CpuError::MisconfiguredMemoryPath) defends the *configuration*,
    /// once, before any guest code runs. Task 3 found two paths that degrade **afterwards**, so
    /// that check passes and the runtime is quietly 30-49x slower with correct results: two guest
    /// threads faulting on one commit granule made the second decline, handing the block to
    /// dynarmic's own handler, which recompiled it with fastmem off **permanently**; and a panic
    /// inside the fault handler declined a resolvable fault while incrementing no counter at all.
    ///
    /// Both are invisible to every functional test, and both are instances of one class: guest
    /// memory that should have reached memory directly went through a host callback instead. So
    /// the check is on the class, not on the two paths — **per run slice, the callback-path
    /// counter's delta must be zero unless that slice ended in a memory-fault exit**. It cannot be
    /// "the counter stays at zero", because a legitimate [`ExitReason::MemoryFault`] increments it
    /// too, and a check that fires on the normal case gets disabled.
    ///
    /// [`ExitReason::MemoryFault`]: crate::ExitReason::MemoryFault
    #[error(
        "the guest memory path degraded while running: the slice ending at {pc:#x} took \
         {callbacks} callback-path entries and stopped for a reason that is not a memory fault \
         ({exit}). Under D4's identity mapping a guest access reaches memory with no callback at \
         all, so a non-zero delta here means blocks have been recompiled onto the callback path -- \
         measured 30-49x slower (n = 31, two loop shapes), with correct results throughout"
    )]
    DegradedMemoryPath {
        /// Where the slice stopped.
        pc: GuestAddr,
        /// How many callback-path entries the slice took.
        callbacks: u64,
        /// How the slice ended, so the exemption that did not apply is visible.
        exit: &'static str,
    },

    /// This backend cannot do what was asked, and says so rather than pretending.
    ///
    /// The shape that forced this variant: [`RunLimit::Instructions`](crate::RunLimit) is a
    /// *counted* budget, and a backend that executes guest code natively on an ARM64 host has no
    /// instruction counter to spend — there is no translator in the loop to insert one
    /// (`ARCHITECTURE.md` section 6). Such a backend refuses the limit here. Silently running
    /// unbounded instead would be exactly the placeholder success Global Constraint 1 forbids.
    #[error("the {backend} backend cannot {operation}: {reason}")]
    Unsupported {
        /// Which backend refused.
        backend: &'static str,
        /// What was asked of it.
        operation: &'static str,
        /// Why it cannot.
        reason: &'static str,
    },

    /// The backend failed inside itself.
    ///
    /// Not a catch-all in the sense `omni-mem` avoids: it names the backend and the operation, and
    /// carries the backend's own message. It exists because a backend is allowed to be foreign code
    /// with failures this crate cannot enumerate — dynarmic's is C++ — and swallowing those would be
    /// worse than carrying them untyped.
    #[error("the {backend} backend failed in {operation}: {detail}")]
    Backend {
        /// Which backend failed.
        backend: &'static str,
        /// What it was doing.
        operation: &'static str,
        /// What it said.
        detail: String,
    },

    /// Memory for a CPU context could not be obtained.
    ///
    /// Carried rather than flattened because D5 measured **20-35 MiB of committed code cache per
    /// guest thread**, which makes per-thread context creation a real consumer of the scarce
    /// resource (Global Constraint 6) and a real failure path rather than a formality.
    #[error("allocating the CPU context for a guest thread failed: {source}")]
    Memory {
        /// The underlying allocation failure.
        #[from]
        source: MemError,
    },
}
