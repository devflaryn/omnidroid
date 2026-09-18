//! Why a [`run`](crate::GuestCpu::run) stopped, and how long it was allowed to go on.

use omni_mem::GuestAddr;

/// What kind of access faulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessKind {
    /// A load.
    Read,
    /// A store.
    Write,
    /// An instruction fetch — the guest branched to an address with no executable mapping.
    Execute,
}

impl core::fmt::Display for AccessKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            AccessKind::Read => "read",
            AccessKind::Write => "write",
            AccessKind::Execute => "instruction fetch",
        })
    }
}

/// Why guest execution stopped.
///
/// Every variant carries the guest `PC` it stopped at, because the first question about a stop is
/// always "where", and a caller that has to read it back out of the register file afterwards will
/// eventually read it after something else has moved it.
///
/// This enum is the runtime's entire vocabulary for guest control leaving the CPU, so it is
/// deliberately closed: a backend that meets a condition not listed here must return
/// [`CpuError`](crate::CpuError) rather than invent a stop. That is the difference between an exit
/// the runtime can act on and a silent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitReason {
    /// The guest returned through the sentinel return address the runtime planted in `X30`.
    ///
    /// This is how a *call into* guest code finishes: the runtime sets `LR` to an address it owns,
    /// and the guest's own `RET` lands there. `pc` is that sentinel, so a caller can tell this apart
    /// from a guest that happened to branch to a similar address.
    Returned {
        /// The sentinel return address that was reached.
        pc: GuestAddr,
    },

    /// The guest reached an address registered with
    /// [`add_thunk`](crate::GuestCpu::add_thunk): a call out of the guest world.
    ///
    /// This is the boundary M3 builds the imported-symbol layer on. It is expressed as *reaching an
    /// address*, not as a translator refusing to translate one, because on an ARM64 host there is no
    /// translator: the backend plants a veneer at the address that hands control back. Both shapes
    /// produce this exit.
    Thunk {
        /// The registered address that was reached.
        pc: GuestAddr,
    },

    /// The guest executed an instruction the backend cannot run.
    ///
    /// Names the instruction as well as the address (Global Constraint 7). D5 measured **231 of
    /// dynarmic's 874 decoder entries unimplemented** — LSE atomics, FP16 arithmetic, BF16, i8mm,
    /// `FJCVTZS`, `CNTVCT_EL0`, the `ID_AA64*` system registers — so this is a routine outcome to be
    /// reported precisely, not an assertion failure.
    UnsupportedInstruction {
        /// Where the instruction is.
        pc: GuestAddr,
        /// The 32-bit A64 encoding, exactly as fetched. Fixed width, so there is nothing to
        /// truncate and a report can be decoded by hand.
        encoding: u32,
    },

    /// A guest memory access could not be satisfied.
    ///
    /// The guest is untrusted code (Global Constraint 11) and *will* branch to unmapped addresses
    /// and dereference garbage. This is the typed exit that has to happen instead of a host crash.
    MemoryFault {
        /// The instruction that faulted. Equal to `address` for an
        /// [`AccessKind::Execute`] fault.
        pc: GuestAddr,
        /// The guest address that could not be accessed.
        address: GuestAddr,
        /// What the guest was trying to do to it.
        access: AccessKind,
    },

    /// Execution reached an address registered with
    /// [`add_breakpoint`](crate::GuestCpu::add_breakpoint).
    ///
    /// The instruction at `pc` has **not** been executed, so resuming with
    /// [`run`](crate::GuestCpu::run) from `pc` runs it.
    Breakpoint {
        /// The breakpoint address.
        pc: GuestAddr,
    },

    /// The step budget given to [`run`](crate::GuestCpu::run) ran out.
    ///
    /// Resumable: nothing about the guest state is different from any other instruction boundary.
    StepLimitReached {
        /// Where execution stopped.
        pc: GuestAddr,
        /// How many guest instructions were executed. May exceed the budget by less than one
        /// translated block: a counted budget is checked at block boundaries, not between every pair
        /// of instructions, and a backend that pretended otherwise would be lying about a number.
        executed: u64,
    },

    /// Another thread asked this context to stop, through a [`HaltHandle`](crate::HaltHandle).
    ///
    /// Resumable. This is the only stop that does not come from the guest, and it is the reason the
    /// handle exists: a step budget bounds guest code that a backend can *count*, and Global
    /// Constraint 11 requires bounding guest code that recurses or loops without end on a backend
    /// that cannot.
    Halted {
        /// Where execution stopped.
        pc: GuestAddr,
    },
}

impl ExitReason {
    /// The guest `PC` execution stopped at.
    #[must_use]
    pub fn pc(&self) -> GuestAddr {
        match *self {
            ExitReason::Returned { pc }
            | ExitReason::Thunk { pc }
            | ExitReason::UnsupportedInstruction { pc, .. }
            | ExitReason::MemoryFault { pc, .. }
            | ExitReason::Breakpoint { pc }
            | ExitReason::StepLimitReached { pc, .. }
            | ExitReason::Halted { pc } => pc,
        }
    }

    /// Whether the guest can simply be run again from where it stopped.
    ///
    /// `false` for the two stops that describe guest code the backend could not carry out:
    /// resuming either of those without the runtime doing something first would reproduce it
    /// forever.
    #[must_use]
    pub fn is_resumable(&self) -> bool {
        !matches!(
            self,
            ExitReason::UnsupportedInstruction { .. } | ExitReason::MemoryFault { .. }
        )
    }
}

impl core::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            ExitReason::Returned { pc } => {
                write!(f, "the guest returned to the sentinel address {pc:#x}")
            }
            ExitReason::Thunk { pc } => write!(f, "the guest reached the thunk at {pc:#x}"),
            ExitReason::UnsupportedInstruction { pc, encoding } => write!(
                f,
                "the guest executed an unsupported instruction {encoding:#010x} at {pc:#x}"
            ),
            ExitReason::MemoryFault { pc, address, access } => write!(
                f,
                "the instruction at {pc:#x} faulted on a guest {access} of {address:#x}"
            ),
            ExitReason::Breakpoint { pc } => write!(f, "breakpoint at {pc:#x}"),
            ExitReason::StepLimitReached { pc, executed } => {
                write!(f, "the step budget ran out at {pc:#x} after {executed} instructions")
            }
            ExitReason::Halted { pc } => write!(f, "execution was halted at {pc:#x}"),
        }
    }
}

/// How long one [`run`](crate::GuestCpu::run) may go on for.
///
/// # The shape that nearly assumed translation
///
/// A counted budget is natural for a translating backend, which is already rewriting every block and
/// can add a counter for nothing. A backend that executes guest code **natively** on an ARM64 host
/// has no such place to put one: there is no translator in the loop at all, the loader maps the code
/// executable and calls it (`ARCHITECTURE.md` section 6). So a trait whose only stop control were
/// `run(n_instructions)` would be implementable by exactly one of the two backends the abstraction
/// exists for.
///
/// Hence two mechanisms rather than one. [`Instructions`](RunLimit::Instructions) is *offered*, and
/// a backend that cannot count refuses it with [`CpuError::Unsupported`](crate::CpuError) rather
/// than ignoring it. [`HaltHandle`](crate::HaltHandle) is the mechanism every backend must provide,
/// and it is what actually satisfies Global Constraint 11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLimit {
    /// Run until the guest stops for a reason of its own, or until halted.
    Unlimited,
    /// Run at most this many guest instructions.
    ///
    /// Zero returns [`ExitReason::StepLimitReached`] immediately, having executed nothing. That is a
    /// defined answer rather than an error because the budget will be arithmetic on a caller's
    /// remaining allowance, and an allowance reaching zero is ordinary.
    Instructions(u64),
}

impl RunLimit {
    /// The budget in instructions, if it is counted.
    #[must_use]
    pub fn instructions(self) -> Option<u64> {
        match self {
            RunLimit::Unlimited => None,
            RunLimit::Instructions(n) => Some(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_exit_names_where_it_stopped_and_whether_it_can_be_resumed() {
        let exits = [
            (ExitReason::Returned { pc: 0x1000 }, true),
            (ExitReason::Thunk { pc: 0x2000 }, true),
            (ExitReason::UnsupportedInstruction { pc: 0x3000, encoding: 0xD503_201F }, false),
            (
                ExitReason::MemoryFault {
                    pc: 0x4000,
                    address: 0xDEAD_BEEF,
                    access: AccessKind::Write,
                },
                false,
            ),
            (ExitReason::Breakpoint { pc: 0x5000 }, true),
            (ExitReason::StepLimitReached { pc: 0x6000, executed: 4096 }, true),
            (ExitReason::Halted { pc: 0x7000 }, true),
        ];
        for (exit, resumable) in exits {
            assert_ne!(exit.pc(), 0, "{exit:?} must name a PC");
            assert_eq!(exit.is_resumable(), resumable, "{exit:?}");
            assert!(!exit.to_string().is_empty());
        }

        // The two diagnostic exits must carry the value that explains them, not just the address.
        let unsupported = exits[2].0;
        assert!(
            unsupported.to_string().contains("0xd503201f"),
            "an unsupported instruction must name its encoding: {unsupported}"
        );
        let fault = exits[3].0;
        assert!(
            fault.to_string().contains("0xdeadbeef") && fault.to_string().contains("write"),
            "a fault must name the address and the access: {fault}"
        );
    }

    #[test]
    fn a_zero_step_budget_is_a_budget_and_not_an_absence_of_one() {
        assert_eq!(RunLimit::Unlimited.instructions(), None);
        assert_eq!(RunLimit::Instructions(0).instructions(), Some(0));
        assert_eq!(RunLimit::Instructions(u64::MAX).instructions(), Some(u64::MAX));
    }
}
