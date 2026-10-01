//! Emulation-layer debugging and dumping for guest AArch64 code.
//!
//! # What this is, and why it lives below the guest
//!
//! Omnidroid runs guest arm64 code on a translating backend ([`omni_cpu`]) inside a host process,
//! with the guest's address space identity-mapped into the host's (`ARCHITECTURE.md` section 1).
//! That gives a debugger a vantage point that a normal one does not have: it sits *below* the code
//! it inspects, so it needs no `ptrace`, sets off no self-check the target can run against its own
//! text, and behaves the same on Windows, Linux and macOS because the thing being debugged is
//! always arm64 no matter what the host CPU is (D5: dynarmic on every host).
//!
//! Everything here is built on primitives that already existed and were proven by other gates:
//!
//! * breakpoints, thunks, the return sentinel and register access on [`omni_cpu::GuestCpu`];
//! * W^X memory, copy-on-write writes into read-only views and the region map on
//!   [`omni_mem::GuestSpace`];
//! * symbol tables and the loader on [`omni_elf`].
//!
//! It adds **no OS access of its own** (Global Constraint 4): there is no `cfg(target_os)` in this
//! crate and no `windows-sys`/`libc`. What differs per host is reached through the lower crates.
//!
//! # The lab session
//!
//! A [`Session`] loads one library into a fresh guest address space on the translating backend and
//! lets a caller drive it: resolve a symbol, read and write guest memory (including forced writes
//! into read-only code), list the memory map, dump a loaded module out of guest memory, and — the
//! point of the whole exercise for reverse engineering — **call a function directly with crafted
//! inputs**, so a library's behaviour can be exercised without the game, the login flow or any
//! anti-cheat path around it. Breakpoints, entry/exit interception, `replace_return`, and call and
//! syscall tracing are all driven from the same run loop.
//!
//! This is the primary, host-agnostic path. A separate live-instance path (attaching to a running
//! omnidroid) is layered on the same vocabulary elsewhere.

pub mod argspec;
pub mod corpus;
pub mod provider;
pub mod session;

pub use argspec::{Arg, CallSpecResult};
pub use provider::ModuleExportsProvider;
pub use session::{
    CallOutcome, Disassembly, HookAction, LoadReport, MapEntry, Registers, Session, Stop,
    SymbolInfo, TraceEvent, TraceKind,
};

/// Re-exported so callers can read a [`LoadReport`]'s unresolved imports without depending on
/// `omni-elf` directly.
pub use omni_elf::loader::UnresolvedImport;

use omni_cpu::CpuError;
use omni_elf::ElfError;
use omni_mem::MemError;

/// Anything that can go wrong driving a debug session.
///
/// The three lower crates each have their own error type; this keeps them distinct rather than
/// flattening to a string, so a caller (and the MCP layer above) can tell *the guest did something*
/// apart from *the request was malformed* apart from *the host failed*.
#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    /// The CPU backend failed, or refused an operation it does not support.
    #[error("cpu backend: {0}")]
    Cpu(#[from] CpuError),

    /// The guest address space refused an access — an address outside it, an uncommitted or
    /// protected range, a failed protection change.
    #[error("guest memory: {0}")]
    Mem(#[from] MemError),

    /// Parsing the ELF failed.
    #[error("elf: {0}")]
    Elf(#[from] ElfError),

    /// Loading the ELF into the guest address space failed (relocation, mapping, a tampered field).
    #[error("load: {0}")]
    Load(#[from] omni_elf::loader::LoadError),

    /// A symbol the caller named is not exported by any loaded module.
    #[error("no symbol named {0:?} in any loaded module")]
    NoSuchSymbol(String),

    /// A module the caller named is not loaded.
    #[error("no module named {0:?} is loaded")]
    NoSuchModule(String),

    /// A call into guest code stopped for a reason other than returning through the sentinel, and
    /// the caller asked for the return value. Carries what actually happened.
    #[error("the guest did not return normally: {0}")]
    DidNotReturn(String),

    /// The request itself was malformed: too many arguments for the AAPCS64 register path, a zero
    /// length where one is required, an address range that wraps.
    #[error("bad request: {0}")]
    BadRequest(String),
}

/// The result type every fallible operation in this crate returns.
pub type Result<T> = std::result::Result<T, DebugError>;
