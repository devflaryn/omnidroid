//! The mechanism half of the thunk boundary: what a host handler sees of the guest when the guest
//! calls out of its world, and how it says it cannot finish the call here.
//!
//! The *policy* half — AAPCS64 argument marshalling, the symbol table, guest memory validation, the
//! host-to-guest direction — is `omni-android`'s (`ARCHITECTURE.md` section 5). This module is only
//! what a backend must be able to offer, and it is in `omni-cpu` for one reason: the two backends
//! offer it in completely different ways.
//!
//! # The shape that nearly assumed translation
//!
//! M3 task 1's measurement probe took `fn(&mut InlineThunkCall)`, and `InlineThunkCall` read guest
//! registers by calling into dynarmic's `JitState`. That type lives in `omni-cpu::dynarmic`, which
//! **does not exist on an ARM64 host** — the crate is `cfg(all(feature = "dynarmic",
//! target_arch = "x86_64"))`. A compatibility layer written against it would have compiled on
//! exactly one of the two hosts the `GuestCpu` abstraction exists for, and the ARM64-native path
//! (`ARCHITECTURE.md` section 6, D5) would have had to be bolted on afterwards by rewriting every
//! handler's signature.
//!
//! So the register file a handler sees is [`ThunkRegs`], a trait, and the handler takes
//! [`ThunkCall`], which is backend-neutral:
//!
//! * the **translating** backend implements [`ThunkRegs`] over `JitState`, which is coherent at every
//!   callback because the A64 emitter stores each guest register write straight to memory;
//! * an **ARM64-native** backend implements it over the register frame its veneer saved — guest code
//!   ran on the real registers, so the veneer spills them and hands the frame over. That is what
//!   makes the thunk "close to a direct call" on that host: the ABI already matches, so the marshal
//!   is a load from the frame rather than a translation.
//!
//! The cost of the trait object is one indirect call per register access, replacing what was a direct
//! call into the FFI shim. It is measured rather than assumed; see `tests/thunk.rs`.
//!
//! # Why a handler cannot reach the CPU
//!
//! [`ThunkCall`] gives a handler the register file and nothing else. In particular it does **not**
//! carry a `&mut dyn GuestCpu`, and that absence is load-bearing: an inline handler runs *inside*
//! generated guest code, so calling back into the guest from one would re-enter the backend's run
//! loop from inside its own callback. On the translating backend that means two live `&mut` to the
//! same callback context, which is undefined behaviour and not merely untidy.
//!
//! A handler that needs guest code run for it therefore calls
//! [`defer_to_caller`](ThunkCall::defer_to_caller), which turns the call into an ordinary
//! [`ExitReason::Thunk`](crate::ExitReason::Thunk) — and the caller, standing outside the run loop
//! with `&mut dyn GuestCpu` in hand, can re-enter the guest safely. That is D17's "keep the exit path
//! for unresolved imports and for anything that must call back into guest code", expressed as a type
//! rather than as a convention.

use omni_mem::GuestAddr;

/// The guest register file, as a thunk handler sees it while the guest is suspended at the thunk.
///
/// Indices are architectural: `x(30)` is the link register, and `sp` is **not** `x(31)` — see
/// [`XReg`](crate::XReg) for why. An index that names no register reads zero and ignores a write,
/// because the index comes from marshalling code and a panic here would be a panic inside generated
/// guest code.
pub trait ThunkRegs {
    /// Read `X{index}`, `index` in `0..=30`.
    fn x(&self, index: u32) -> u64;
    /// Write `X{index}`, `index` in `0..=30`.
    fn set_x(&mut self, index: u32, value: u64);
    /// Read `V{index}` as its full 128 bits, `index` in `0..=31`.
    ///
    /// Needed exactly as much as [`x`](ThunkRegs::x): AAPCS64 passes floating-point arguments in
    /// `V0`-`V7` and returns in `V0`, so a boundary that could only reach the general-purpose
    /// registers would silently drop every `double`.
    fn v(&self, index: u32) -> u128;
    /// Write `V{index}`.
    fn set_v(&mut self, index: u32, value: u128);
    /// Read `SP`. The eighth-and-beyond arguments live above it, so this is not optional either.
    fn sp(&self) -> GuestAddr;
    /// Write `SP`.
    fn set_sp(&mut self, value: GuestAddr);
}

/// An opaque token a thunk is registered with and handed back at every call.
///
/// A [`ThunkFn`] is a bare `fn` and captures nothing, so this is how one symbol's handler finds the
/// state it shares with the other 169. It is a `usize` rather than a pointer so that nothing in
/// `omni-cpu` has to make a claim about what it points at or how long it lives: the crate that
/// registers the thunk is the crate that knows, and the `unsafe` that turns it back into a reference
/// belongs there with the argument for why it is sound.
///
/// Zero is the default and means "nothing registered".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub struct ThunkContext(pub usize);

/// One guest call out of the guest world, in progress.
///
/// Reads and writes go straight to wherever the backend keeps guest registers at a thunk, so a write
/// here is what the resumed guest sees.
pub struct ThunkCall<'a> {
    regs: &'a mut dyn ThunkRegs,
    address: GuestAddr,
    context: ThunkContext,
    deferred: bool,
}

impl<'a> ThunkCall<'a> {
    /// Build one. Backends only: `address` must be the thunk address the guest actually reached, and
    /// `context` the token that thunk was registered with.
    #[must_use]
    pub fn new(regs: &'a mut dyn ThunkRegs, address: GuestAddr, context: ThunkContext) -> Self {
        Self { regs, address, context, deferred: false }
    }

    /// The thunk address the guest reached — which is what identifies the *symbol*.
    ///
    /// The whole reason a single handler can serve every import: the address is the name.
    #[must_use]
    pub fn address(&self) -> GuestAddr {
        self.address
    }

    /// The token this thunk was registered with.
    #[must_use]
    pub fn context(&self) -> ThunkContext {
        self.context
    }

    /// Read `X{index}`.
    #[must_use]
    pub fn x(&self, index: u32) -> u64 {
        self.regs.x(index)
    }

    /// Write `X{index}`.
    pub fn set_x(&mut self, index: u32, value: u64) {
        self.regs.set_x(index, value);
    }

    /// Read `V{index}` as its full 128 bits.
    #[must_use]
    pub fn v(&self, index: u32) -> u128 {
        self.regs.v(index)
    }

    /// Write `V{index}`.
    pub fn set_v(&mut self, index: u32, value: u128) {
        self.regs.set_v(index, value);
    }

    /// Read `SP`.
    #[must_use]
    pub fn sp(&self) -> GuestAddr {
        self.regs.sp()
    }

    /// Write `SP`.
    pub fn set_sp(&mut self, value: GuestAddr) {
        self.regs.set_sp(value);
    }

    /// `X30`, where the guest will resume when the call completes.
    ///
    /// Not necessarily the address after a `BL`: a guest that reaches a thunk by a plain `B` — a tail
    /// call, or a hostile jump — resumes at whatever `X30` already held, which is what real hardware
    /// would do and therefore what this must do too.
    #[must_use]
    pub fn lr(&self) -> GuestAddr {
        self.regs.x(30) as GuestAddr
    }

    /// Do **not** resume the guest: return [`ExitReason::Thunk`](crate::ExitReason::Thunk) to
    /// whoever called [`run`](crate::GuestCpu::run) instead.
    ///
    /// The escape hatch for the two things an inline handler structurally cannot do — run guest code
    /// (see the module docs) and fail with a typed error, since it has no return channel. The guest
    /// `PC` is left at the thunk, so the exit is resumable and the caller sees the exact address.
    ///
    /// Idempotent.
    pub fn defer_to_caller(&mut self) {
        self.deferred = true;
    }

    /// Whether this call was deferred. Backends check it after the handler returns.
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        self.deferred
    }
}

impl core::fmt::Debug for ThunkCall<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThunkCall")
            .field("address", &format_args!("{:#x}", self.address))
            .field("context", &self.context)
            .field("deferred", &self.deferred)
            .finish()
    }
}

/// A host function servicing a guest call from inside the run loop.
///
/// A bare `fn` rather than a boxed closure, deliberately, for two reasons. It is what task 1
/// measured, and a boxed closure would add an indirection to the thing the 33 ns figure is about;
/// and it makes [`ThunkContext`] the only way a handler reaches shared state, which is what keeps
/// that state's lifetime an explicit argument somebody had to make rather than an `Arc` clone
/// nobody thought about.
pub type ThunkFn = fn(&mut ThunkCall<'_>);

#[cfg(test)]
mod tests {
    use super::*;

    /// A register file in a `Vec`, which is what an ARM64-native backend's veneer frame would be.
    ///
    /// It exists to prove the trait is implementable without a translator — the failure mode this
    /// module is shaped against.
    #[derive(Default)]
    struct Frame {
        x: [u64; 31],
        v: [u128; 32],
        sp: GuestAddr,
    }

    impl ThunkRegs for Frame {
        fn x(&self, index: u32) -> u64 {
            self.x.get(index as usize).copied().unwrap_or(0)
        }
        fn set_x(&mut self, index: u32, value: u64) {
            if let Some(slot) = self.x.get_mut(index as usize) {
                *slot = value;
            }
        }
        fn v(&self, index: u32) -> u128 {
            self.v.get(index as usize).copied().unwrap_or(0)
        }
        fn set_v(&mut self, index: u32, value: u128) {
            if let Some(slot) = self.v.get_mut(index as usize) {
                *slot = value;
            }
        }
        fn sp(&self) -> GuestAddr {
            self.sp
        }
        fn set_sp(&mut self, value: GuestAddr) {
            self.sp = value;
        }
    }

    #[test]
    fn an_out_of_range_register_index_reads_zero_rather_than_panicking() {
        let mut frame = Frame::default();
        let mut call = ThunkCall::new(&mut frame, 0x1000, ThunkContext(7));
        // 31 is `XZR`/`SP` depending on the instruction and is never a `ThunkRegs` index; 99 names
        // nothing at all. Marshalling code computes these indices, and a panic here would be a
        // panic inside generated guest code.
        assert_eq!(call.x(31), 0);
        assert_eq!(call.x(99), 0);
        assert_eq!(call.v(32), 0);
        call.set_x(31, 0xDEAD);
        call.set_v(99, 1);
        assert_eq!(call.x(31), 0, "the write was ignored, not redirected somewhere");
    }

    #[test]
    fn a_call_carries_its_address_and_its_registration_token() {
        let mut frame = Frame::default();
        frame.set_x(30, 0x4000);
        frame.set_sp(0x8000);
        let mut call = ThunkCall::new(&mut frame, 0x1234, ThunkContext(0xABC));
        assert_eq!(call.address(), 0x1234, "the address is what identifies the symbol");
        assert_eq!(call.context(), ThunkContext(0xABC));
        assert_eq!(call.lr(), 0x4000);
        assert_eq!(call.sp(), 0x8000);
        assert!(!call.is_deferred());
        assert!(!format!("{call:?}").is_empty());
        call.defer_to_caller();
        call.defer_to_caller();
        assert!(call.is_deferred(), "deferring is idempotent");
    }

    #[test]
    fn a_write_through_the_call_is_visible_in_the_frame() {
        let mut frame = Frame::default();
        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            call.set_x(0, 0x1122_3344_5566_7788);
            call.set_v(0, u128::from_le_bytes([0xAA; 16]));
            call.set_sp(0x9000);
        }
        assert_eq!(frame.x(0), 0x1122_3344_5566_7788);
        assert_eq!(frame.v(0), u128::from_le_bytes([0xAA; 16]));
        assert_eq!(frame.sp(), 0x9000);
    }
}
