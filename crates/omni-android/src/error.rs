//! Every way the boundary can refuse, each naming the symbol and the guest address.
//!
//! Global Constraint 7 in its sharpest form. These errors will be read **three thousand
//! initializers deep**, by someone who cannot see the guest's stack and has no debugger attached to
//! it, so "an unimplemented import" is useless and "`pthread_rwlock_init` at 0x1c8_2700_0040" is the
//! whole answer. Constraint 1 is the other half: none of these is recoverable by returning a
//! plausible value, because a plausible value is what makes the failure surface a thousand
//! initializers later somewhere unrelated.

use omni_cpu::{CpuError, ExitReason};
use omni_mem::{GuestAddr, Refusal};

/// Why the boundary could not carry out a call.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AbiError {
    /// The guest called an import the compatibility layer does not implement.
    ///
    /// **The error this whole module exists for.** The alternative — returning zero — is what
    /// Constraint 1 forbids: 3,594 initializers will carry the zero forward and the first visible
    /// symptom will be somewhere else entirely.
    #[error(
        "the guest called the imported symbol `{symbol}` through its thunk at {address:#x}, and \
         nothing in the compatibility layer implements it"
    )]
    Unbound {
        /// The symbol name, exactly as `.dynstr` spells it.
        symbol: String,
        /// The thunk address the guest branched to.
        address: GuestAddr,
    },

    /// The guest branched *into* a thunk slot rather than to its first instruction.
    ///
    /// A hostile case that is also an ordinary bug's signature — a miscomputed `GOT` offset, a
    /// relocation applied at the wrong width — and it must not be indistinguishable from a correct
    /// call. It names the containing symbol, because the offset is what says which.
    #[error(
        "the guest branched to {address:#x}, which is {offset} bytes into the thunk slot for \
         `{symbol}` at {slot:#x} rather than to its first instruction"
    )]
    MidThunk {
        /// The symbol whose slot was branched into.
        symbol: String,
        /// The slot's own address.
        slot: GuestAddr,
        /// Where the guest actually branched.
        address: GuestAddr,
        /// How far into the slot that is.
        offset: usize,
    },

    /// The guest branched into the thunk region, but at an address no symbol was given.
    #[error(
        "the guest branched to {address:#x}, inside the thunk region \
         [{start:#x}, {end:#x}), but no symbol is bound anywhere in that slot"
    )]
    NoSuchThunk {
        /// Where the guest branched.
        address: GuestAddr,
        /// First address of the region.
        start: GuestAddr,
        /// One past its last address.
        end: GuestAddr,
    },

    /// The guest *called* a data symbol.
    ///
    /// The 18 `STT_OBJECT` imports the initializers reach — `environ`, `stdout`, `__sF`,
    /// `AMEDIAFORMAT_KEY_*` — are addresses to load from, not code. Calling one means the guest, or
    /// a relocation, treated data as a function, and executing whatever the object holds is the one
    /// response that must not happen.
    #[error("the guest called `{symbol}` at {address:#x}, which is a data symbol and not a function")]
    DataSymbolCalled {
        /// The symbol name.
        symbol: String,
        /// Its address.
        address: GuestAddr,
    },

    /// An argument pointed somewhere the guest has not mapped for that access.
    ///
    /// The routine case, not the exceptional one: guest code passes null, passes freed pointers, and
    /// passes pointers computed from lengths it got wrong. Every one arrives here instead of at a
    /// host segmentation fault.
    #[error(
        "`{symbol}` at {address:#x} was passed {len} byte(s) at {pointer:#x} for argument \
         {argument}, which the guest has not mapped for {access}: {refusal}"
    )]
    BadPointer {
        /// The symbol whose argument it is.
        symbol: String,
        /// That symbol's thunk address.
        address: GuestAddr,
        /// Which argument, counted from zero as the ABI counts them.
        argument: usize,
        /// The pointer the guest passed.
        pointer: GuestAddr,
        /// How many bytes the boundary needed to be there.
        len: usize,
        /// `"reading"` or `"writing"`.
        access: &'static str,
        /// Which of `admit`'s rules refused it.
        refusal: RefusalText,
    },

    /// A C string argument had no NUL within the cap.
    ///
    /// The other half of a hostile pointer: an address that *is* mapped, pointing at bytes that
    /// never terminate. Without a cap the walk runs to the end of the region and then faults on the
    /// host side; with one it is a typed error naming the symbol.
    #[error(
        "`{symbol}` at {address:#x} was passed a string at {pointer:#x} for argument {argument} \
         with no NUL in the first {limit} bytes"
    )]
    Unterminated {
        /// The symbol.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// Which argument.
        argument: usize,
        /// The pointer.
        pointer: GuestAddr,
        /// How far the walk went.
        limit: usize,
    },

    /// An argument shape the marshaller will not guess at.
    ///
    /// AAPCS64 has rules for composite arguments — HFAs in `V0`-`V7`, small structs in general
    /// registers, large ones passed by an address the caller allocates — and this crate implements
    /// none of them, because no import in the reachable set of 188 passes a composite by value.
    /// Should one appear, it must arrive as this error rather than as eight bytes read out of `X0`
    /// and called a struct.
    #[error(
        "`{symbol}` at {address:#x} asked for an argument shape the AAPCS64 marshaller does not \
         implement: {shape}"
    )]
    UnsupportedShape {
        /// The symbol.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// What was asked for.
        shape: &'static str,
    },

    /// A `va_list` the guest handed over does not describe a register save area.
    ///
    /// AArch64's `va_list` is a five-field record the *callee* filled in, so for `vsnprintf` and its
    /// three siblings in the reachable set the boundary has to walk a structure guest code wrote. It
    /// is untrusted input like any other: `__gr_offs` outside `-64..=0` or `__vr_offs` outside
    /// `-128..=0` would make the next `va_arg` read from an address of the guest's choosing.
    #[error(
        "`{symbol}` at {address:#x} was handed a va_list at {pointer:#x} whose {field} is \
         {value}, which is outside the {low}..={high} an AArch64 register save area can have"
    )]
    BadVaList {
        /// The symbol.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// Where the `va_list` is.
        pointer: GuestAddr,
        /// `"__gr_offs"` or `"__vr_offs"`.
        field: &'static str,
        /// What it held.
        value: i64,
        /// Lowest legal value.
        low: i64,
        /// Highest legal value.
        high: i64,
    },

    /// A variadic call asked for more arguments than it was given.
    ///
    /// There is no way to know how many a `printf`-family call really has except by reading the
    /// format string the guest supplied, which is why running off the end has to be an error rather
    /// than a zero: the overflow area beyond `SP` is the guest's own stack frame, and reading it
    /// would return whatever the caller's locals happen to be.
    #[error(
        "`{symbol}` at {address:#x} asked for variadic argument {index}, past the end of its \
         overflow area at {overflow:#x}"
    )]
    VarArgsExhausted {
        /// The symbol.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// Which variadic argument was asked for, counted from zero.
        index: usize,
        /// Where the overflow area had reached.
        overflow: GuestAddr,
    },

    /// The boundary was re-entered more deeply than it allows.
    ///
    /// Guest calls host calls guest calls host. Each level is a legitimate thing for a `qsort`
    /// comparator or an `atexit` handler to do, and unbounded levels are a host stack overflow
    /// reached from guest-supplied data, which is the abort Constraint 11 calls Critical and which no
    /// caller can contain.
    #[error(
        "servicing `{symbol}` at {address:#x} would re-enter guest code {depth} levels deep, past \
         the limit of {limit}"
    )]
    TooDeep {
        /// The symbol being serviced.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// The depth this call would reach.
        depth: usize,
        /// The cap.
        limit: usize,
    },

    /// A call *into* guest code did not come back through the sentinel.
    ///
    /// A guest callback is untrusted code like any other: it may fault, execute garbage, loop
    /// forever or run out of budget. Every one of those arrives here with the exit that happened,
    /// rather than being reported as the callback's return value.
    #[error("the guest callback at {target:#x}, called for `{symbol}`, stopped: {exit}")]
    GuestCallbackStopped {
        /// The symbol on whose behalf the guest was called.
        symbol: String,
        /// The guest function that was called.
        target: GuestAddr,
        /// How it stopped.
        exit: ExitReason,
    },

    /// A call into guest code was asked for with a stack pointer that cannot be used.
    ///
    /// AArch64 requires `SP` to be 16-byte aligned at a public interface, and every `SP`-relative
    /// access in the callee assumes it. A misaligned `SP` inherited from a hostile guest would make
    /// the callee's own prologue fault at an address nothing here chose.
    #[error(
        "the guest callback at {target:#x}, called for `{symbol}`, cannot be entered with \
         SP = {sp:#x}: {why}"
    )]
    BadCallbackStack {
        /// The symbol.
        symbol: String,
        /// The guest function.
        target: GuestAddr,
        /// The stack pointer as it stood.
        sp: GuestAddr,
        /// What is wrong with it.
        why: &'static str,
    },

    /// One [`run`](crate::Boundary::run) crossed the exit path more times than the boundary allows.
    ///
    /// The containment for a guest that loops through the **exit** path. A guest looping through an
    /// *inline* thunk spends guest instructions and the backend's own budget stops it; each exit-path
    /// crossing returns to Rust, so the budget never expires and the loop would be a hang in host
    /// code rather than in guest code. Reported rather than folded into
    /// [`ExitReason::Halted`], because a caller has to be able to tell
    /// "my watchdog fired" from "the guest is spinning through the boundary".
    #[error(
        "the guest crossed the thunk boundary's exit path {crossings} times in one run, at the          limit of {limit}, most recently at {pc:#x}"
    )]
    CrossingLimit {
        /// How many crossings there were.
        crossings: u64,
        /// The limit.
        limit: u64,
        /// Where the guest was about to resume.
        pc: GuestAddr,
    },

    /// The compatibility layer implements the symbol, and refuses **this** call.
    ///
    /// The difference from [`Unbound`](AbiError::Unbound) is the whole point: `Unbound` means
    /// nothing implements the symbol, and this means something does but will not guess at the
    /// case in front of it — a `%Lf` whose 128-bit quad has no correct 8-byte read, a FORTIFY
    /// `_chk` whose destination is too small, a `sscanf` whose scanning engine does not exist.
    /// Every one of those has a believable wrong answer available (0, -1, the truncated value)
    /// and Global Constraint 1 forbids all of them.
    #[error("`{symbol}` at {address:#x} refused this call: {why}")]
    Refused {
        /// The symbol.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// What it would have had to guess at.
        why: String,
    },

    /// A bionic handler ran on a thread with no compatibility-layer state installed.
    ///
    /// A **host** mistake rather than a guest one: `Bionic::activate` was not held across
    /// `Boundary::run`, so the handler has no errno slot, no thread identity and no futex. It
    /// is an error rather than a default-constructed state because a per-call default would
    /// give every guest thread its own private mutex table, and two guest threads that each
    /// believe they hold the same mutex is precisely the failure no later test can see.
    #[error(
        "`{symbol}` at {address:#x} was serviced on a thread with no bionic state:          `Bionic::activate` must be held across `Boundary::run`"
    )]
    BionicNotActive {
        /// The symbol being serviced.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
    },

    /// The guest deliberately terminated itself: `abort`, or a failed stack-protector check.
    ///
    /// **Not a refusal and not a defect in this layer.** The guest asked to die, and the only
    /// thing that would be wrong here is doing what it asked *literally*: `std::process::abort()`
    /// on the host cannot be caught by any caller, and the runtime is required to host several
    /// isolated guest instances in one process (see the memory figures in `STATUS.md`). One
    /// instance's `abort` must not take the other two with it, or the host, or the test runner.
    ///
    /// So it becomes this: a typed error that propagates out of
    /// [`Boundary::run`](crate::Boundary::run) like any other, which the caller may report, log,
    /// restart the instance after, or turn into its own exit — all of which are decisions that
    /// belong to the embedder and none of which an `abort()` leaves available.
    ///
    /// `message` is whatever the guest passed to `android_set_abort_message`, which is where
    /// bionic's own crash reporter gets the line it prints, and is usually the only human-readable
    /// account of why the process died.
    #[error(
        "the guest terminated itself through `{symbol}` at {address:#x}: {why}{}",
        match message {
            Some(text) => format!(" -- the guest's abort message was: {text}"),
            None => " -- the guest set no abort message".to_string(),
        }
    )]
    GuestAborted {
        /// The symbol the guest called: `abort` or `__stack_chk_fail`.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// What that symbol means, in one clause.
        why: &'static str,
        /// The guest's `android_set_abort_message`, if it set one.
        message: Option<String>,
    },

    /// The guest called `_exit`.
    ///
    /// The same containment argument as [`GuestAborted`](AbiError::GuestAborted): calling the
    /// host's `exit` would end every other guest instance in the process and the host with them.
    /// An exit is reported, not performed.
    ///
    /// It is an `Err` rather than a successful [`ExitReason`] because every caller of
    /// `Boundary::run` today treats `Ok` as "the guest returned and may be resumed", and a guest
    /// that has called `_exit` may not be: resuming it would run code after `exit`. Making that a
    /// type-level difference rather than a flag is the same reasoning D18 applies to re-entrancy.
    #[error("the guest called `{symbol}` at {address:#x} with status {status}")]
    GuestExited {
        /// The symbol the guest called.
        symbol: String,
        /// Its thunk address.
        address: GuestAddr,
        /// The status the guest asked to exit with.
        status: i32,
    },

    /// The thunk region could not be reserved or has run out of slots.
    #[error("the thunk region cannot hold another {what}: {detail}")]
    RegionFull {
        /// `"function slot"` or `"data object"`.
        what: &'static str,
        /// The numbers.
        detail: String,
    },

    /// The guest address space could not give the boundary what it asked for.
    #[error("the thunk boundary could not obtain guest memory: {0}")]
    Memory(#[from] omni_mem::MemError),

    /// The CPU refused, which is distinct from the guest failing.
    #[error("the thunk boundary could not drive the CPU: {0}")]
    Cpu(#[from] CpuError),
}

impl AbiError {
    /// The symbol this error is about, when it is about one.
    #[must_use]
    pub fn symbol(&self) -> Option<&str> {
        match self {
            AbiError::Unbound { symbol, .. }
            | AbiError::MidThunk { symbol, .. }
            | AbiError::DataSymbolCalled { symbol, .. }
            | AbiError::BadPointer { symbol, .. }
            | AbiError::Unterminated { symbol, .. }
            | AbiError::UnsupportedShape { symbol, .. }
            | AbiError::BadVaList { symbol, .. }
            | AbiError::VarArgsExhausted { symbol, .. }
            | AbiError::TooDeep { symbol, .. }
            | AbiError::GuestCallbackStopped { symbol, .. }
            | AbiError::BadCallbackStack { symbol, .. }
            | AbiError::Refused { symbol, .. }
            | AbiError::GuestAborted { symbol, .. }
            | AbiError::GuestExited { symbol, .. }
            | AbiError::BionicNotActive { symbol, .. } => Some(symbol),
            AbiError::NoSuchThunk { .. }
            | AbiError::CrossingLimit { .. }
            | AbiError::RegionFull { .. }
            | AbiError::Memory(_)
            | AbiError::Cpu(_) => None,
        }
    }

    /// The guest address this error is about, when it is about one.
    ///
    /// For most variants that is the thunk address, which is what identifies the symbol; for the two
    /// callback variants it is the guest function that was called, which is the address the reader
    /// needs.
    #[must_use]
    pub fn guest_address(&self) -> Option<GuestAddr> {
        match *self {
            AbiError::Unbound { address, .. }
            | AbiError::DataSymbolCalled { address, .. }
            | AbiError::BadPointer { address, .. }
            | AbiError::Unterminated { address, .. }
            | AbiError::UnsupportedShape { address, .. }
            | AbiError::BadVaList { address, .. }
            | AbiError::VarArgsExhausted { address, .. }
            | AbiError::TooDeep { address, .. }
            | AbiError::Refused { address, .. }
            | AbiError::GuestAborted { address, .. }
            | AbiError::GuestExited { address, .. }
            | AbiError::BionicNotActive { address, .. } => Some(address),
            // The address the *guest* branched to, not the slot it landed in: the whole point of the
            // variant is that those differ.
            AbiError::MidThunk { address, .. } | AbiError::NoSuchThunk { address, .. } => {
                Some(address)
            }
            AbiError::GuestCallbackStopped { target, .. }
            | AbiError::BadCallbackStack { target, .. } => Some(target),
            AbiError::CrossingLimit { pc, .. } => Some(pc),
            AbiError::RegionFull { .. } | AbiError::Memory(_) | AbiError::Cpu(_) => None,
        }
    }
}

/// [`Refusal`] with a `Display`, so an error message can say which rule refused.
///
/// `omni-mem` derives only `Debug` on it — deliberately, since the three refusals are reported
/// differently by each caller — and `{:?}` in a message aimed at somebody 3,000 initializers deep is
/// not good enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalText(pub Refusal);

impl From<Refusal> for RefusalText {
    fn from(refusal: Refusal) -> Self {
        Self(refusal)
    }
}

impl core::fmt::Display for RefusalText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self.0 {
            Refusal::NotMapped => "nothing is mapped there, or the range runs off the end of what is",
            Refusal::Protection => "a region is mapped there but its protection forbids the access",
            Refusal::Commit => "the range could not be committed",
        })
    }
}

/// What the boundary returns.
pub type AbiResult<T> = Result<T, AbiError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant that is about a symbol must name it *and* an address, in its message and
    /// through the accessors, because that is the entire contract with whoever reads this 3,000
    /// initializers deep.
    #[test]
    fn every_symbol_error_names_the_symbol_and_the_address_in_its_message() {
        let cases: Vec<AbiError> = vec![
            AbiError::Unbound { symbol: "pthread_once".into(), address: 0x1234_5000 },
            AbiError::MidThunk {
                symbol: "pthread_once".into(),
                slot: 0x1234_5000,
                address: 0x1234_5004,
                offset: 4,
            },
            AbiError::DataSymbolCalled { symbol: "pthread_once".into(), address: 0x1234_5000 },
            AbiError::BadPointer {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                argument: 0,
                pointer: 0,
                len: 8,
                access: "reading",
                refusal: Refusal::NotMapped.into(),
            },
            AbiError::Unterminated {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                argument: 1,
                pointer: 0x9000,
                limit: 4096,
            },
            AbiError::UnsupportedShape {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                shape: "a composite passed by value",
            },
            AbiError::BadVaList {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                pointer: 0x9000,
                field: "__gr_offs",
                value: -2_000_000,
                low: -64,
                high: 0,
            },
            AbiError::VarArgsExhausted {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                index: 3,
                overflow: 0x7000,
            },
            AbiError::TooDeep {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                depth: 9,
                limit: 8,
            },
            AbiError::GuestAborted {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                why: "the guest called abort()",
                message: None,
            },
            AbiError::GuestExited {
                symbol: "pthread_once".into(),
                address: 0x1234_5000,
                status: 0,
            },
        ];
        for error in &cases {
            let text = error.to_string();
            assert_eq!(error.symbol(), Some("pthread_once"), "{text}");
            assert!(text.contains("pthread_once"), "{text} must name the symbol");
            assert!(
                text.contains("1234500") || text.contains("12345004"),
                "{text} must name the guest address in hex"
            );
            assert!(error.guest_address().is_some(), "{text}");
        }
    }

    /// **The guest's self-termination is reported, never performed**, and the abort message it set
    /// travels with it.
    ///
    /// The thing this test is really pinning is that the two variants exist at all: the failure
    /// they replace is `std::process::abort()`, which no caller can contain and which would take
    /// every other guest instance in the process with it. A test cannot assert "the host did not
    /// abort" — the runner would be gone — so it asserts the shape that makes aborting impossible:
    /// a value, with the reason in it.
    #[test]
    fn a_guest_abort_carries_its_message_and_an_exit_carries_its_status() {
        let silent = AbiError::GuestAborted {
            symbol: "abort".into(),
            address: 0x7000,
            why: "the guest called abort()",
            message: None,
        };
        assert!(silent.to_string().contains("set no abort message"), "{silent}");

        let spoken = AbiError::GuestAborted {
            symbol: "__stack_chk_fail".into(),
            address: 0x7000,
            why: "the stack protector found a corrupted canary",
            message: Some("terminating with uncaught exception".into()),
        };
        let text = spoken.to_string();
        assert!(text.contains("__stack_chk_fail"), "{text}");
        assert!(text.contains("corrupted canary"), "{text}");
        assert!(text.contains("terminating with uncaught exception"), "{text}");

        let exited = AbiError::GuestExited { symbol: "_exit".into(), address: 0x7000, status: 42 };
        let text = exited.to_string();
        assert!(text.contains("status 42"), "{text}");
        // A zero status is still an exit and must not read as a success.
        let zero = AbiError::GuestExited { symbol: "_exit".into(), address: 0x7000, status: 0 };
        assert!(zero.to_string().contains("status 0"), "{zero}");
    }

    /// `MidThunk` must report where the guest went, not where it should have gone — the whole
    /// content of the variant is that the two differ.
    #[test]
    fn a_mid_thunk_error_reports_the_address_branched_to_and_the_slot_separately() {
        let error = AbiError::MidThunk {
            symbol: "memcpy".into(),
            slot: 0x2000,
            address: 0x2008,
            offset: 8,
        };
        assert_eq!(error.guest_address(), Some(0x2008));
        let text = error.to_string();
        assert!(text.contains("0x2008") && text.contains("0x2000") && text.contains("8 bytes"), "{text}");
    }

    #[test]
    fn a_refusal_reads_as_a_sentence_rather_than_as_a_debug_name() {
        for refusal in [Refusal::NotMapped, Refusal::Protection, Refusal::Commit] {
            let text = RefusalText(refusal).to_string();
            assert!(text.len() > 15, "{refusal:?} rendered as {text:?}");
            assert!(!text.contains("NotMapped"), "{text}");
        }
    }

    /// The two variants that are not about a symbol must still be diagnostic, and must not claim one.
    #[test]
    fn the_region_errors_name_the_region_rather_than_a_symbol() {
        let error = AbiError::NoSuchThunk { address: 0x5008, start: 0x5000, end: 0x6000 };
        assert_eq!(error.symbol(), None);
        assert_eq!(error.guest_address(), Some(0x5008));
        let text = error.to_string();
        assert!(text.contains("0x5008") && text.contains("0x5000") && text.contains("0x6000"), "{text}");
    }
}
