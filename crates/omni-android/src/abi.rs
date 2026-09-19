//! AAPCS64, both directions: reading a guest call's arguments, and writing its return value.
//!
//! The rules implemented here are IHI0055's parameter passing, stage C, restricted to the argument
//! shapes the reachable set of 188 imports actually uses — and *refusing* the rest rather than
//! guessing, because a marshaller that guesses produces a plausible number and Global Constraint 1
//! is about exactly that.
//!
//! # The fixed-argument rules
//!
//! Three counters, and their names are the ABI's own:
//!
//! | Counter | What it allocates | Exhausted at |
//! |---|---|---|
//! | **NGRN** | integers and pointers, `X0`-`X7` | 8 |
//! | **NSRN** | floating point, `V0`-`V7` | 8 |
//! | **NSAA** | everything after either runs out, upward from `SP` | never |
//!
//! Two properties of those rules that are easy to get wrong and that this module states as tests:
//!
//! * **The two banks are independent.** Nine integers and one `double` put the `double` in `V0`, not
//!   on the stack: running out of `X` registers says nothing about `V` registers.
//! * **There is no back-filling.** Once an argument goes to the stack, AAPCS64 sets that bank's
//!   counter to 8, so a later, smaller argument may **not** go back into the register that was
//!   skipped. Getting this wrong shifts every subsequent argument by one and is silent.
//!
//! # The variadic rules, which are *not* the fixed rules
//!
//! This is the hard case the brief names, and the trap is that the answer depends on the platform,
//! not only on the architecture:
//!
//! | Platform | Where variadic arguments go |
//! |---|---|
//! | **AAPCS64 proper — Linux, Android, and therefore Omnidroid's guest** | exactly like named ones: integers in `X0`-`X7`, floating point in **`V0`-`V7`**, then the stack |
//! | Apple's arm64 | **all** variadic arguments on the stack, however few |
//! | Windows on ARM64 | variadic floating point in the **general-purpose** registers |
//!
//! So an implementation written from the Apple rule reads every `printf` argument off the stack and
//! finds the caller's locals; one written from the Microsoft rule reads a `double` out of `X`. The
//! evidence that Android is the first row is in bionic's own `va_list`, which has `__vr_top` and
//! `__vr_offs` fields — a structure with nowhere to record a floating-point save area would be
//! describing a platform that does not have one. See [`crate::varargs`].
//!
//! What *does* differ from the named case, and is implemented in [`crate::varargs`] rather than here:
//!
//! * **Default argument promotions**, which come from C rather than from the ABI: a `float` argument
//!   is passed as a `double`, and anything narrower than `int` is passed as an `int`. `printf("%f")`
//!   therefore reads a `double` out of a `V` register even when the source wrote `1.0f`.
//! * **Composites get no HFA treatment.** An argument in the variadic part is never passed as a
//!   homogeneous float aggregate in up to four `V` registers, because the callee's save area has no
//!   way to describe that. None of the nine reachable variadic imports passes a composite, and one
//!   that did would be [`AbiError::UnsupportedShape`].
//! * **Slot sizes in the save area**, which are 8 bytes per integer and a full **16** per
//!   floating-point value even though a `double` is 8 — the `V` save area is `Q` registers.
//!
//! # The indirect result, which is a fourth register
//!
//! `mallinfo` is in the reachable set and returns a 10-field struct — 80 bytes, far over the 16-byte
//! limit — so AAPCS64 returns it *indirectly*: the caller allocates the space and passes its address
//! in **`X8`**, and the callee returns nothing. `X8` is not an argument register and a marshaller
//! that only knew about `X0`-`X7` would have no way to reach it, so [`Args::indirect_result`] exists.

use omni_cpu::ThunkCall;
use omni_mem::GuestAddr;

use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

/// How many integer and how many floating-point argument registers AAPCS64 has.
pub const ARG_REGISTERS: u32 = 8;

/// Where a call's argument registers can be read from.
///
/// Read-only and by `&self`, which is the point: the *inline* path reads them out of the backend's
/// live register file through a [`ThunkCall`], and the *exit* path reads them out of a snapshot taken
/// before anything could clobber them (see `ArgRegs`). One marshaller over both, rather than two that
/// drift — and the marshaller cannot write a register it is still reading arguments out of, because it
/// has no way to.
pub trait ArgSource {
    /// `X{index}`, `0..=8` — `X8` being the indirect result register.
    fn x(&self, index: u32) -> u64;
    /// `V{index}` as its full 128 bits.
    fn v(&self, index: u32) -> u128;
    /// `SP`, where the ninth and later arguments start.
    fn sp(&self) -> GuestAddr;
}

impl ArgSource for ThunkCall<'_> {
    fn x(&self, index: u32) -> u64 {
        ThunkCall::x(self, index)
    }
    fn v(&self, index: u32) -> u128 {
        ThunkCall::v(self, index)
    }
    fn sp(&self) -> GuestAddr {
        ThunkCall::sp(self)
    }
}

/// Where a call's return value is written.
///
/// The mirror of [`ArgSource`], and separate from it so that a handler holding a [`Ret`] cannot still
/// be holding an [`Args`].
pub trait RetSink {
    /// Write `X{index}`.
    fn set_x(&mut self, index: u32, value: u64);
    /// Write `V{index}`.
    fn set_v(&mut self, index: u32, value: u128);
}

impl RetSink for ThunkCall<'_> {
    fn set_x(&mut self, index: u32, value: u64) {
        ThunkCall::set_x(self, index, value);
    }
    fn set_v(&mut self, index: u32, value: u128) {
        ThunkCall::set_v(self, index, value);
    }
}

/// The argument registers, copied out before anything can change them.
///
/// **Not an optimisation — a correctness requirement on the exit path.** A handler there may call back
/// into guest code, and a guest callback clobbers `X0`-`X7` and `V0`-`V7` as freely as any other
/// function. A handler that read its third argument *after* invoking a `qsort` comparator would read
/// the comparator's leftovers. Snapshotting at entry makes that impossible rather than forbidden.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArgRegs {
    x: [u64; 9],
    v: [u128; 8],
    sp: GuestAddr,
}

impl ArgRegs {
    /// Capture `X0`-`X8`, `V0`-`V7` and `SP`.
    #[must_use]
    pub fn capture(source: &dyn ArgSource) -> Self {
        let mut out = Self::default();
        for (index, slot) in out.x.iter_mut().enumerate() {
            *slot = source.x(index as u32);
        }
        for (index, slot) in out.v.iter_mut().enumerate() {
            *slot = source.v(index as u32);
        }
        out.sp = source.sp();
        out
    }
}

impl ArgSource for ArgRegs {
    fn x(&self, index: u32) -> u64 {
        self.x.get(index as usize).copied().unwrap_or(0)
    }
    fn v(&self, index: u32) -> u128 {
        self.v.get(index as usize).copied().unwrap_or(0)
    }
    fn sp(&self) -> GuestAddr {
        self.sp
    }
}

/// Reading a guest call's arguments in AAPCS64 order.
///
/// A cursor, not a random-access table, because the position of the *n*th argument depends on the
/// types of all the arguments before it. A handler declares the shape by the order in which it asks:
/// `next_int()`, `next_double()`, `next_pointer()`.
pub struct Args<'a> {
    call: &'a dyn ArgSource,
    mem: &'a GuestMem,
    blame: Blame<'a>,
    /// NGRN: the next general-purpose register, 0-8. 8 means the bank is spent.
    ngrn: u32,
    /// NSRN: the next SIMD/floating-point register, 0-8.
    nsrn: u32,
    /// NSAA: the next stacked argument address, which starts at `SP`.
    nsaa: usize,
    /// How many arguments have been taken, for the blame.
    taken: usize,
}

impl<'a> Args<'a> {
    /// Start reading at the beginning of the argument list.
    #[must_use]
    pub fn new(call: &'a dyn ArgSource, mem: &'a GuestMem, blame: Blame<'a>) -> Self {
        Self { call, mem, blame, ngrn: 0, nsrn: 0, nsaa: call.sp(), taken: 0 }
    }

    /// The symbol and address this call is for, with the argument index kept current.
    fn blame(&self) -> Blame<'a> {
        self.blame.argument(self.taken)
    }

    /// Where the stack arguments have reached. Also the start of a variadic call's overflow area.
    #[must_use]
    pub fn overflow(&self) -> usize {
        self.nsaa
    }

    /// How many `X` and `V` registers have been consumed, which is what a variadic cursor needs in
    /// order to start where the named arguments stopped.
    #[must_use]
    pub fn consumed(&self) -> (u32, u32) {
        (self.ngrn, self.nsrn)
    }

    /// Take the next 8-byte-or-narrower integer or pointer argument.
    ///
    /// Returns the full 64 bits of the register. A narrower parameter — `int`, `bool`, an `enum` —
    /// occupies a whole register or a whole 8-byte stack slot, and the **caller is not required to
    /// clear the high half**, so a handler that wants an `int` must take the low 32 bits itself.
    /// [`next_i32`](Args::next_i32) does that and says so.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`] if the argument is a stack argument and the guest's `SP` does not
    /// have it mapped — which is a guest whose stack has overflowed, and which must be a typed error
    /// rather than a host read of whatever is below the stack mapping.
    pub fn next_u64(&mut self) -> AbiResult<u64> {
        let value = if self.ngrn < ARG_REGISTERS {
            let value = self.call.x(self.ngrn);
            self.ngrn += 1;
            value
        } else {
            // Rule C.13/C.14: 8-byte alignment, one 8-byte slot, and **the bank stays spent** — the
            // `ngrn = 8` above is never walked back, because AAPCS64 does not back-fill.
            let at = self.align_nsaa(8);
            let value = self.mem.read_u64(at, self.blame())?;
            self.nsaa = at + 8;
            value
        };
        self.taken += 1;
        Ok(value)
    }

    /// Take the next integer argument as a guest address.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](Args::next_u64).
    pub fn next_pointer(&mut self) -> AbiResult<usize> {
        // A guest pointer is 64 bits and `GuestAddr` is `usize`, which is 64 bits on every host this
        // runs on — but the cast is written as a `try_into` rather than an `as` so that a 32-bit host
        // fails to build instead of silently truncating every pointer the guest passes.
        let value = self.next_u64()?;
        usize::try_from(value).map_err(|_| AbiError::UnsupportedShape {
            symbol: self.blame.symbol.to_string(),
            address: self.blame.address,
            shape: "a 64-bit guest pointer on a host whose usize is narrower",
        })
    }

    /// Take the next `int`-sized argument, sign-extended from its low 32 bits.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](Args::next_u64).
    pub fn next_i32(&mut self) -> AbiResult<i32> {
        Ok(self.next_u64()? as u32 as i32)
    }

    /// Take the next `double`.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](Args::next_u64).
    pub fn next_f64(&mut self) -> AbiResult<f64> {
        let bits = self.next_fp_bits(8)?;
        Ok(f64::from_bits(bits as u64))
    }

    /// Take the next `float`.
    ///
    /// Only correct for a **named** `float` parameter. In the variadic part a `float` has already been
    /// promoted to `double` by the caller, so reading it as a `float` would read the low half of a
    /// `double`'s bit pattern and produce a number that is wrong rather than imprecise. That case goes
    /// through [`crate::varargs`], which promotes.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](Args::next_u64).
    pub fn next_f32(&mut self) -> AbiResult<f32> {
        let bits = self.next_fp_bits(4)?;
        Ok(f32::from_bits(bits as u32))
    }

    /// The shared half of the two floating-point takers: `V0`-`V7`, then the stack.
    fn next_fp_bits(&mut self, size: usize) -> AbiResult<u128> {
        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.v(self.nsrn);
            self.nsrn += 1;
            value
        } else {
            let at = self.align_nsaa(size.max(8));
            let bytes = self.mem.read_bytes(at, size, self.blame())?;
            let mut buf = [0u8; 16];
            buf[..size].copy_from_slice(&bytes);
            self.nsaa = at + size.max(8);
            u128::from_le_bytes(buf)
        };
        self.taken += 1;
        Ok(bits)
    }

    /// The indirect result location, `X8`.
    ///
    /// AAPCS64 reserves it for a return value larger than 16 bytes, which among the reachable imports
    /// means `mallinfo` and nothing else. It is deliberately not part of the cursor: it is allocated
    /// before any argument and consumes no argument register, so threading it through `next_*` would
    /// make every other argument's position depend on whether a handler remembered to ask.
    #[must_use]
    pub fn indirect_result(&self) -> usize {
        self.call.x(8) as usize
    }

    /// Refuse an argument shape this marshaller does not implement.
    ///
    /// # Errors
    ///
    /// Always [`AbiError::UnsupportedShape`]. That is the point: a composite passed by value has to
    /// arrive as an error naming the symbol rather than as eight bytes out of `X0`.
    pub fn unsupported<T>(&self, shape: &'static str) -> AbiResult<T> {
        Err(AbiError::UnsupportedShape {
            symbol: self.blame.symbol.to_string(),
            address: self.blame.address,
            shape,
        })
    }

    fn align_nsaa(&self, align: usize) -> usize {
        // Rule C.12: round the NSAA up to the argument's alignment, minimum 8.
        let align = align.max(8);
        (self.nsaa + align - 1) & !(align - 1)
    }
}

/// Writing a guest call's return value.
///
/// A separate type from [`Args`] because it needs `&mut ThunkCall` and the arguments only need
/// `&ThunkCall` — and because that separation is what stops a handler writing `X0` while it still has
/// arguments left to read out of it.
pub struct Ret<'a> {
    call: &'a mut dyn RetSink,
}

impl<'a> Ret<'a> {
    /// Prepare to write a return value.
    #[must_use]
    pub fn new(call: &'a mut dyn RetSink) -> Self {
        Self { call }
    }

    /// Return an integer or a pointer in `X0`.
    ///
    /// A narrower return — `int`, `char` — is written zero- or sign-extended by the *caller's*
    /// convention: AAPCS64 leaves the high bits unspecified, and the guest's own code will have a
    /// `SXTW`/`UXTB` if it cares. Writing the full 64 bits of the value the handler computed is
    /// therefore both correct and the only thing that can be done.
    pub fn u64(&mut self, value: u64) {
        self.call.set_x(0, value);
    }

    /// Return an `int`, sign-extended into `X0`.
    ///
    /// Sign-extended rather than zero-extended because almost every `int`-returning libc function
    /// returns `-1` for failure, and a guest that compares `W0` against `#-1` after a zero-extending
    /// thunk would see `0xFFFFFFFF` and take the success path.
    pub fn i32(&mut self, value: i32) {
        self.call.set_x(0, i64::from(value) as u64);
    }

    /// Return a 128-bit value, or a two-register composite, in `X0` and `X1`.
    pub fn u128(&mut self, value: u128) {
        self.call.set_x(0, value as u64);
        self.call.set_x(1, (value >> 64) as u64);
    }

    /// Return a `double` in `V0`.
    ///
    /// The whole 128-bit register is written, with the upper bits zeroed, which is what an AArch64
    /// `FMOV D0, …` does — the architecture zeroes the rest of the vector on any write to `Dn`. A
    /// handler that wrote only the low 64 bits would leave whatever the guest last had in `V0.D[1]`,
    /// and a guest doing a 128-bit `STR Q0` of the result would store it.
    pub fn f64(&mut self, value: f64) {
        self.call.set_v(0, u128::from(value.to_bits()));
    }

    /// Return a `float` in `V0`, with the rest of the register zeroed.
    pub fn f32(&mut self, value: f32) {
        self.call.set_v(0, u128::from(value.to_bits()));
    }

    /// Return nothing, and do not disturb `X0`.
    ///
    /// Named rather than left implicit so that "this function is `void`" is a decision in the code
    /// rather than the absence of one. AAPCS64 says `X0` is *unspecified* after a `void` call, so
    /// leaving it alone is legal — and it is also what the guest's own compiler assumes, which is
    /// what makes clobbering it a source of bugs that appear far away.
    pub fn void(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_cpu::{ThunkContext, ThunkRegs};
    use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};
    use std::sync::Arc;

    /// A register file in host memory, which is what an ARM64-native backend's veneer frame is.
    ///
    /// The marshaller is tested against this rather than against a live guest because the rules it
    /// implements are about *where a value is*, and a synthetic frame states that directly. The live
    /// guest is in `tests/roundtrip.rs`, where real translated ARM64 code puts the values there.
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
            if let Some(s) = self.x.get_mut(index as usize) {
                *s = value;
            }
        }
        fn v(&self, index: u32) -> u128 {
            self.v.get(index as usize).copied().unwrap_or(0)
        }
        fn set_v(&mut self, index: u32, value: u128) {
            if let Some(s) = self.v.get_mut(index as usize) {
                *s = value;
            }
        }
        fn sp(&self) -> GuestAddr {
            self.sp
        }
        fn set_sp(&mut self, value: GuestAddr) {
            self.sp = value;
        }
    }

    struct Fixture {
        mem: GuestMem,
        stack: GuestAddr,
    }

    fn fixture() -> Fixture {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let page = space.page_size();
        let stack = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a stack page");
        Fixture { mem: GuestMem::new(space), stack }
    }

    fn blame() -> Blame<'static> {
        Blame::new("test", 0x1000, 0)
    }

    #[test]
    fn the_first_eight_integers_come_out_of_x0_to_x7_in_order() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_x(i, 0x1000 + u64::from(i));
        }
        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        for i in 0..8u64 {
            assert_eq!(args.next_u64().expect("an integer argument"), 0x1000 + i);
        }
        assert_eq!(args.consumed(), (8, 0), "eight X registers spent, no V registers");
    }

    /// The ninth argument onward is on the stack, and each one takes a whole 8-byte slot.
    #[test]
    fn arguments_past_the_eighth_come_off_the_stack_in_eight_byte_slots() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_x(i, u64::from(i));
        }
        for slot in 0..4u64 {
            f.mem
                .write_u64(f.stack + slot as usize * 8, 0xAA00 + slot, blame())
                .expect("the stack slot must be writable");
        }
        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        for i in 0..8u64 {
            assert_eq!(args.next_u64().expect("register argument"), i);
        }
        for slot in 0..4u64 {
            assert_eq!(args.next_u64().expect("stack argument"), 0xAA00 + slot);
        }
        assert_eq!(args.overflow(), f.stack + 32, "four slots consumed");
    }

    /// **The rule that is easy to get wrong.** The integer and floating-point banks are independent,
    /// so nine integers and one `double` put the `double` in `V0` — not on the stack behind them.
    #[test]
    fn running_out_of_integer_registers_does_not_push_floating_point_onto_the_stack() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_x(i, u64::from(i) + 1);
        }
        frame.set_v(0, u128::from(2.5f64.to_bits()));
        f.mem.write_u64(f.stack, 9, blame()).expect("write the ninth integer");

        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        for i in 1..=8u64 {
            assert_eq!(args.next_u64().expect("integer"), i);
        }
        assert_eq!(args.next_u64().expect("the ninth integer, on the stack"), 9);
        assert_eq!(
            args.next_f64().expect("the double"),
            2.5,
            "the double belongs in V0: the two banks are allocated independently"
        );
    }

    /// The mirror of the rule above, in the other bank.
    #[test]
    fn running_out_of_floating_point_registers_does_not_push_integers_onto_the_stack() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_v(i, u128::from(f64::from(i).to_bits()));
        }
        frame.set_x(0, 0xBEEF);
        f.mem.write_u64(f.stack, 9.5f64.to_bits(), blame()).expect("the ninth double");

        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        for i in 0..8 {
            assert_eq!(args.next_f64().expect("double"), f64::from(i));
        }
        assert_eq!(args.next_f64().expect("the ninth double, on the stack"), 9.5);
        assert_eq!(args.next_u64().expect("the integer"), 0xBEEF, "X0 was never spent");
    }

    /// **No back-filling.** Once the integer bank has spilled to the stack it stays spilled: a later
    /// argument may not go back into a register the spill skipped. Getting this wrong shifts every
    /// argument after it and is invisible.
    #[test]
    fn an_integer_bank_that_has_spilled_to_the_stack_never_goes_back_to_a_register() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_x(i, 100 + u64::from(i));
        }
        f.mem.write_u64(f.stack, 900, blame()).expect("write");
        f.mem.write_u64(f.stack + 8, 901, blame()).expect("write");

        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        for _ in 0..8 {
            args.next_u64().expect("register");
        }
        assert_eq!(args.next_u64().expect("stack"), 900);
        assert_eq!(args.next_u64().expect("stack"), 901, "and not back into X0");
        assert_eq!(args.consumed().0, 8);
    }

    /// A narrower parameter occupies a whole register and the caller need not clear the high half, so
    /// a handler asking for an `int` must get the low 32 bits sign-extended and nothing else.
    #[test]
    fn a_narrow_integer_argument_ignores_the_high_half_of_its_register() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        // What a guest really leaves in X0 when it passes `(int)-1`: `MOV W0, #-1` writes the low 32
        // bits and zeroes the top, but a register holding a previous 64-bit value and then a 32-bit
        // store keeps nothing — either way the high half is not the caller's promise.
        frame.set_x(0, 0xDEAD_BEEF_FFFF_FFFF);
        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        assert_eq!(args.next_i32().expect("an int"), -1);
    }

    #[test]
    fn a_float_and_a_double_are_read_from_the_low_bits_of_their_own_register() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        // Garbage in the high lanes, which is what a guest that last used V0 for a vector leaves.
        frame.set_v(0, (u128::from(0xFFFF_FFFF_FFFF_FFFFu64) << 64) | u128::from(1.5f32.to_bits()));
        frame.set_v(1, (u128::from(0xAAAAu64) << 64) | u128::from((-2.25f64).to_bits()));
        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        assert_eq!(args.next_f32().expect("float"), 1.5);
        assert_eq!(args.next_f64().expect("double"), -2.25);
    }

    /// A stack argument whose slot is not mapped is a guest whose stack overflowed. It must be a
    /// typed error naming the symbol, not a host read below the stack mapping.
    #[test]
    fn a_stack_argument_below_an_unmapped_sp_is_a_typed_error() {
        let f = fixture();
        let mut frame = Frame { sp: 0, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_x(i, 0);
        }
        let call = ThunkCall::new(&mut frame, 0x2000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, Blame::new("fprintf", 0x2000, 0));
        for _ in 0..8 {
            args.next_u64().expect("register arguments are fine");
        }
        let error = args.next_u64().expect_err("a stack argument at SP = 0 must be refused");
        assert!(matches!(error, AbiError::BadPointer { pointer: 0, argument: 8, .. }), "{error:?}");
        assert_eq!(error.symbol(), Some("fprintf"));
    }

    #[test]
    fn the_indirect_result_register_is_x8_and_costs_no_argument_register() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        frame.set_x(8, 0x7FFF_0000);
        frame.set_x(0, 42);
        let call = ThunkCall::new(&mut frame, 0x1000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, blame());
        assert_eq!(args.indirect_result(), 0x7FFF_0000, "mallinfo's 80-byte result buffer");
        assert_eq!(args.next_u64().expect("X0 is still the first argument"), 42);
    }

    #[test]
    fn an_unsupported_shape_is_refused_by_name_rather_than_guessed_at() {
        let f = fixture();
        let mut frame = Frame { sp: f.stack, ..Frame::default() };
        let call = ThunkCall::new(&mut frame, 0x3000, ThunkContext::default());
        let args = Args::new(&call, &f.mem, Blame::new("getaddrinfo", 0x3000, 0));
        let error = args
            .unsupported::<()>("a composite passed by value")
            .expect_err("it must refuse, not guess");
        assert!(matches!(error, AbiError::UnsupportedShape { .. }), "{error:?}");
        assert!(error.to_string().contains("getaddrinfo"), "{error}");
    }

    #[test]
    fn a_return_value_lands_in_the_register_the_abi_names() {
        let mut frame = Frame::default();
        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            Ret::new(&mut call).u64(0x1122_3344_5566_7788);
        }
        assert_eq!(frame.x(0), 0x1122_3344_5566_7788);

        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            Ret::new(&mut call).u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
        }
        assert_eq!(frame.x(0), 0x5555_6666_7777_8888, "the low half in X0");
        assert_eq!(frame.x(1), 0x1111_2222_3333_4444, "the high half in X1");

        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            Ret::new(&mut call).f64(2.5);
        }
        assert_eq!(frame.v(0), u128::from(2.5f64.to_bits()));
    }

    /// `-1` is what almost every `int`-returning libc function means by failure, and a guest
    /// comparing `W0` with `#-1` after a zero-extending thunk would take the success path.
    #[test]
    fn an_int_return_of_minus_one_is_sign_extended() {
        let mut frame = Frame::default();
        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            Ret::new(&mut call).i32(-1);
        }
        assert_eq!(frame.x(0), u64::MAX, "sign-extended, so a 64-bit compare with -1 also matches");
        assert_eq!(frame.x(0) as u32 as i32, -1);
    }

    /// AArch64 zeroes the upper lanes on any write to `Dn`/`Sn`, and a guest doing `STR Q0` of the
    /// result would store whatever a partial write left behind.
    #[test]
    fn a_floating_point_return_zeroes_the_upper_lanes_of_v0() {
        let mut frame = Frame::default();
        frame.set_v(0, u128::MAX);
        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            Ret::new(&mut call).f32(1.0);
        }
        assert_eq!(frame.v(0), u128::from(1.0f32.to_bits()));
        assert_eq!(frame.v(0) >> 32, 0, "every bit above the float is clear");
    }

    /// `void` must leave `X0` alone. AAPCS64 leaves it unspecified, and so does the guest's compiler —
    /// which is exactly why clobbering it produces bugs that appear somewhere else.
    #[test]
    fn a_void_return_does_not_touch_x0() {
        let mut frame = Frame::default();
        frame.set_x(0, 0xC0FFEE);
        {
            let mut call = ThunkCall::new(&mut frame, 0, ThunkContext::default());
            Ret::new(&mut call).void();
        }
        assert_eq!(frame.x(0), 0xC0FFEE);
    }
}
