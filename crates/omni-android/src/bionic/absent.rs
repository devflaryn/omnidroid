//! The two symbols that are deliberately left **unresolved**, and the guest instructions that say
//! they must be.
//!
//! `__gcov_dump` and `__gcov_flush`. They are the last two of the reachable 188 and they are the
//! only ones this layer answers by supplying *nothing* — not a handler, not a refusal, not even a
//! thunk address whose call names them.
//!
//! # Why a thunk address is the wrong answer here, and only here
//!
//! [`Binding::Unbound`](crate::Binding::Unbound) is the design for every other import: a symbol
//! nothing implements gets a real address whose call produces a typed error naming it, which beats
//! a branch to address zero with no symbol attached. That argument depends on the guest *calling*
//! the symbol either way. **For a weak undefined symbol it does not**, because the reference the
//! compiler emits is a null test, and the address is what the test reads.
//!
//! Both of these are `WEAK NOTYPE` in `libroblox.so`'s `.dynsym`, and both carry **two**
//! relocations — an `R_AARCH64_JUMP_SLOT` for the PLT stub and an `R_AARCH64_GLOB_DAT` for the GOT
//! slot the code loads to test. **VERIFIED by decoding the one site that references them**, at
//! `0x6194be8` in the library's text:
//!
//! ```text
//! 0x6194be8: 900031e8  ADRP X8, ..
//! 0x6194bec: f943c508  LDR  X8, [X8, #0x788]   ; the __gcov_dump GOT slot
//! 0x6194bf0: b4000068  CBZ  X8, 0x6194bfc      ; if it is null, skip
//! 0x6194bf4: 94050c23  BL   0x62d7c80          ; __gcov_dump's PLT stub
//! 0x6194bf8: 940506da  BL   0x62d6760          ; abort's PLT stub
//! 0x6194bfc: 900031e8  ADRP X8, ..
//! 0x6194c00: f943c908  LDR  X8, [X8, #0x790]   ; the __gcov_flush GOT slot
//! 0x6194c04: b4000048  CBZ  X8, 0x6194c10      ; if it is null, skip
//! 0x6194c08: 94050c22  BL   0x62d7c90          ; __gcov_flush's PLT stub
//! ```
//!
//! The guest tests both for null before calling either. On a real Android device the test
//! succeeds and the calls are skipped, because **no Android libc supplies `__gcov_*`** — they come
//! from `libgcov`, which is linked only into a coverage-instrumented binary, and this one is not
//! instrumented (it merely kept the guarded reference). So a guest whose GOT slot is null is a
//! guest behaving exactly as it does on the device it was built for.
//!
//! Give it an address instead and the `CBZ` falls through:
//!
//! * with the symbol left [`Unbound`](crate::Binding::Unbound), the run **fails** at
//!   `__gcov_dump` — on a path a real device never takes;
//! * with the symbol bound to a no-op that "flushed" coverage data nothing ever collected, the
//!   guest goes on to `BL abort` — the next instruction. A plausible stub here does not merely
//!   lie, it **terminates the process**, and the terminating instruction is four bytes past the
//!   call the stub answered.
//!
//! That second bullet is the whole case. It is the most expensive plausible stub this phase could
//! have written, and nothing short of decoding the call site would have shown it.
//!
//! # What is *not* being claimed
//!
//! This is not "weak symbols resolve to nothing". `libroblox.so` has five weak undefined symbols —
//! `__cxa_thread_atexit_impl`, `gettid`, `getentropy` and these two — and the first three are
//! supplied by a real bionic, so a guest on a device calls them. Only these two are absent *on the
//! platform being modelled*, which is why the declaration is per symbol and the boundary requires
//! both the name and the weakness before it resolves to nothing
//! ([`BoundaryBuilder::declare_absent`](crate::BoundaryBuilder::declare_absent)).

/// One import the compatibility layer deliberately does not supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsentSymbol {
    /// The symbol name, exactly as `.dynstr` spells it.
    pub symbol: &'static str,
    /// Why a real Android device does not supply it either.
    pub why: &'static str,
}

/// The imports a **weak** reference to resolves to nothing.
///
/// Two, and the module documentation has the decoded guest instructions that justify both.
pub static ABSENT_SYMBOLS: &[AbsentSymbol] = &[
    AbsentSymbol {
        symbol: "__gcov_dump",
        why: "a libgcov entry point. No Android libc exports it, `libroblox.so` is not a \
              coverage-instrumented build, and its own code tests the GOT slot for null before \
              calling — so a null is what the device produces and what the guest expects. An \
              address makes the guest call it and then `abort`, which is the next instruction \
              after the guarded call",
    },
    AbsentSymbol {
        symbol: "__gcov_flush",
        why: "the same libgcov pair, guarded by its own null test eight instructions after \
              `__gcov_dump`'s",
    },
];
