//! Variadic calls, which are the hard case in both of the two forms the reachable set contains.
//!
//! # The two forms, and which imports need each
//!
//! Of the 188 imports the 3,594 static initializers statically reach (D17), **thirteen** do not use
//! the fixed-argument ABI:
//!
//! | Form | Imports | What the host has to do |
//! |---|---|---|
//! | **true variadic**, `f(..., ...)` | `__android_log_print`, `fprintf`, `fscanf`, `open`, `prctl`, `snprintf`, `sscanf`, `syscall`, `syslog` | act as the callee: read the extra arguments straight out of `X`, `V` and the guest's stack — [`VarArgs`] |
//! | **`va_list` consumer**, `f(..., va_list)` | `__vsnprintf_chk`, `vasprintf`, `vfprintf`, `vsnprintf` | walk a five-field record *guest code wrote*, which is untrusted input — [`GuestVaList`] |
//!
//! `printf` itself is **not** in the reachable set, and neither is `vsscanf`; `fprintf` and `sscanf`
//! are. `__open_2` looks like a variadic and is not — it is the `_FORTIFY_SOURCE` helper bionic calls
//! when `open` is used with two arguments, and it takes exactly two.
//!
//! # Why a `va_list` arrives as a pointer
//!
//! AArch64's `va_list` is
//!
//! ```c
//! typedef struct __va_list {
//!   void* __stack;    //  0: the next stacked variadic argument
//!   void* __gr_top;   //  8: one past the end of the general-purpose save area
//!   void* __vr_top;   // 16: one past the end of the SIMD save area
//!   int   __gr_offs;  // 24: NEGATIVE byte offset from __gr_top, counting up to 0
//!   int   __vr_offs;  // 28: NEGATIVE byte offset from __vr_top, counting up to 0
//! } va_list;
//! ```
//!
//! — 32 bytes, so AAPCS64 passes it **indirectly**: an argument larger than 16 bytes is replaced by a
//! pointer to a copy the caller made. So `vsnprintf(char*, size_t, const char*, va_list)` puts a
//! *pointer to* the `va_list` in `X3`, not the record itself, and a marshaller that read 32 bytes out
//! of `X3`-`X6` would be reading the wrong thing entirely.
//!
//! # Why the offsets are negative, and why that is a hostile-input surface
//!
//! `va_arg` on AArch64 is
//!
//! ```text
//!   if (__gr_offs < 0) { value = *(T*)(__gr_top + __gr_offs); __gr_offs += 8; }
//!   else               { value = *(T*)__stack; __stack += 8; }
//! ```
//!
//! so the register save area is walked *upward toward its end*, and the sign of the offset is what
//! says "still in registers". The guest's own callee prologue set `__gr_offs` to
//! `-(8 - named_integer_args) * 8`. **Every one of those five fields is guest-written.** An
//! `__gr_offs` of, say, `-2_000_000_000` would make the next `va_arg` read from `__gr_top` minus two
//! gigabytes — an address of the guest's choosing, inside Omnidroid's own address space under D4's
//! identity mapping. So [`GuestVaList`] range-checks both offsets against what a save area can
//! actually be, *and* every read still goes through [`GuestMem`], which is two independent defences
//! rather than one.

use crate::abi::{ArgSource, ARG_REGISTERS};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};
use omni_mem::{GuestAddr, Refusal};

/// Bytes an integer occupies in a register save area, and in the overflow area.
pub const GR_SLOT: usize = 8;
/// Bytes a floating-point value occupies in a register save area.
///
/// **Sixteen, not eight.** The SIMD save area is `Q` registers, so a `double` variadic argument
/// occupies a full 16-byte slot with the value in its low half. A walker that stepped by 8 would read
/// the second argument out of the first argument's upper lanes — which are zero, so it would produce
/// `0.0` rather than an error.
pub const VR_SLOT: usize = 16;

/// Bytes of general-purpose save area: `X0`-`X7`.
pub const GR_SAVE_BYTES: usize = ARG_REGISTERS as usize * GR_SLOT;
/// Bytes of SIMD save area: `Q0`-`Q7`.
pub const VR_SAVE_BYTES: usize = ARG_REGISTERS as usize * VR_SLOT;

/// Offset of each `va_list` field, so the layout appears once.
mod field {
    pub const STACK: usize = 0;
    pub const GR_TOP: usize = 8;
    pub const VR_TOP: usize = 16;
    pub const GR_OFFS: usize = 24;
    pub const VR_OFFS: usize = 28;
}

/// Bytes of an AArch64 `va_list`. Over 16, which is why it is passed indirectly.
pub const VA_LIST_BYTES: usize = 32;

/// The variadic part of a call the guest made *to* a true variadic import.
///
/// The host thunk is the callee here, so there is no `va_list` yet: the arguments are still in the
/// registers and on the stack where the guest's caller put them. This is `va_start` followed by
/// `va_arg`, done directly.
///
/// # Ordering
///
/// Built from the [`Args`](crate::Args) cursor *after* the named arguments have been read, because
/// where the variadic part starts depends entirely on how many registers the named arguments spent.
/// `fprintf`'s two named arguments spend `X0` and `X1`, so its variadic integers start at `X2`; its
/// variadic `double`s start at `V0`, because the named arguments spent no `V` registers at all.
pub struct VarArgs<'a> {
    call: &'a dyn ArgSource,
    mem: &'a GuestMem,
    blame: Blame<'a>,
    ngrn: u32,
    nsrn: u32,
    overflow: GuestAddr,
    index: usize,
}

impl<'a> VarArgs<'a> {
    /// Start the variadic part where the named arguments stopped.
    ///
    /// `consumed` and `overflow` come from [`Args::consumed`](crate::Args::consumed) and
    /// [`Args::overflow`](crate::Args::overflow), so a handler cannot get the split point wrong by
    /// counting its own parameters.
    #[must_use]
    pub fn new(
        call: &'a dyn ArgSource,
        mem: &'a GuestMem,
        blame: Blame<'a>,
        consumed: (u32, u32),
        overflow: GuestAddr,
    ) -> Self {
        Self { call, mem, blame, ngrn: consumed.0, nsrn: consumed.1, overflow, index: 0 }
    }

    /// How many variadic arguments have been taken.
    #[must_use]
    pub fn taken(&self) -> usize {
        self.index
    }

    fn blame(&self) -> Blame<'a> {
        // The argument index a reader cares about is the one in the source: named arguments first,
        // then the variadic ones.
        self.blame.argument(self.blame.argument + self.index)
    }

    /// The next variadic integer or pointer, as the full 64 bits of wherever it is.
    ///
    /// Anything narrower than `int` has already been promoted to `int` by the caller (a C default
    /// argument promotion, not an ABI rule), and an `int` occupies a whole 8-byte slot, so there is
    /// exactly one integer shape to read.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`] if the overflow area is not mapped where the argument would be, which
    /// is what running off the end of a hostile `printf` format string looks like.
    pub fn next_u64(&mut self) -> AbiResult<u64> {
        let value = if self.ngrn < ARG_REGISTERS {
            let value = self.call.x(self.ngrn);
            self.ngrn += 1;
            value
        } else {
            let at = self.aligned_overflow(GR_SLOT)?;
            let value = self.mem.read_u64(at, self.blame())?;
            self.overflow = self.advance_overflow(at, GR_SLOT)?;
            value
        };
        self.index += 1;
        Ok(value)
    }

    /// The next variadic integer, as an `int`.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](VarArgs::next_u64).
    pub fn next_i32(&mut self) -> AbiResult<i32> {
        Ok(self.next_u64()? as u32 as i32)
    }

    /// The next variadic pointer.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](VarArgs::next_u64).
    pub fn next_pointer(&mut self) -> AbiResult<GuestAddr> {
        let value = self.next_u64()?;
        Ok(value as GuestAddr)
    }

    /// The next variadic floating-point argument, which is **always a `double`**.
    ///
    /// `%f` in a format string reads a `double` even when the source wrote `printf("%f", 1.0f)`: the
    /// C default argument promotions convert every `float` in the variadic part to `double` before
    /// the call. A handler that read a `float` here would read the low 32 bits of a `double`'s bit
    /// pattern — which for `1.0` is `0x00000000`, so it would produce `0.0` and no error.
    ///
    /// In a register the value is in the low 64 bits of `Vn`. In the overflow area it takes an 8-byte
    /// slot like an integer; the 16-byte slot is the *save area*'s, not the stack's.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](VarArgs::next_u64).
    pub fn next_f64(&mut self) -> AbiResult<f64> {
        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.v(self.nsrn) as u64;
            self.nsrn += 1;
            value
        } else {
            let at = self.aligned_overflow(GR_SLOT)?;
            let value = self.mem.read_u64(at, self.blame())?;
            self.overflow = self.advance_overflow(at, GR_SLOT)?;
            value
        };
        self.index += 1;
        Ok(f64::from_bits(bits))
    }

    /// `self.overflow` rounded up to `align`, refusing a round-up that leaves the address space.
    ///
    /// The overflow area starts from the guest's `SP`, so this is arithmetic on a guest-chosen
    /// value. See [`GuestVaList::aligned_stack`] for why a wrap is the worse of the two outcomes.
    fn aligned_overflow(&self, align: usize) -> AbiResult<GuestAddr> {
        self.overflow
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.overflow_out_of_space(self.overflow, align))
    }

    /// `at + step`, refusing a sum that leaves the address space.
    fn advance_overflow(&self, at: GuestAddr, step: usize) -> AbiResult<GuestAddr> {
        at.checked_add(step).ok_or_else(|| self.overflow_out_of_space(at, step))
    }

    fn overflow_out_of_space(&self, pointer: GuestAddr, len: usize) -> AbiError {
        AbiError::BadPointer {
            symbol: self.blame.symbol.to_string(),
            address: self.blame.address,
            argument: self.index,
            pointer,
            len,
            access: "reading",
            refusal: Refusal::NotMapped.into(),
        }
    }
}

/// A `va_list` the guest built, walked from the host.
///
/// The mirror of [`VarArgs`]: here the guest has already been the callee once — it ran its own
/// prologue, spilled `X0`-`X7` and `Q0`-`Q7` into a save area on its stack, and filled in the record
/// — and has handed the record's address to `vsnprintf`. So the host walks guest-written state.
///
/// **Every field is untrusted.** The two offsets are bounds-checked at construction, which catches a
/// wild offset before it is ever added to a pointer, and each read still goes through [`GuestMem`],
/// which catches a wild `__gr_top`. Neither check is redundant: the first makes the error message say
/// *which field* was wrong, and the second is what stands between a hostile `va_list` and a host
/// read.
///
/// The walk mutates its own copy of the offsets and writes nothing back to guest memory. That is
/// correct C: `vsnprintf` takes `va_list` **by value** — which on AArch64 means by an address the
/// caller allocated for a *copy* — and is not permitted to advance the caller's list.
pub struct GuestVaList<'a> {
    mem: &'a GuestMem,
    blame: Blame<'a>,
    at: GuestAddr,
    stack: GuestAddr,
    gr_top: GuestAddr,
    vr_top: GuestAddr,
    gr_offs: i64,
    vr_offs: i64,
    index: usize,
}

impl<'a> GuestVaList<'a> {
    /// Read and validate the record at `at`.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`] if the 32 bytes are not readable guest memory, or
    /// [`AbiError::BadVaList`] if either offset is outside the range a save area can have.
    pub fn read(mem: &'a GuestMem, at: GuestAddr, blame: Blame<'a>) -> AbiResult<Self> {
        let stack = mem.read_u64(at + field::STACK, blame)? as GuestAddr;
        let gr_top = mem.read_u64(at + field::GR_TOP, blame)? as GuestAddr;
        let vr_top = mem.read_u64(at + field::VR_TOP, blame)? as GuestAddr;
        let gr_offs = i64::from(mem.read_i32(at + field::GR_OFFS, blame)?);
        let vr_offs = i64::from(mem.read_i32(at + field::VR_OFFS, blame)?);

        // **The bound.** A general-purpose save area is 64 bytes and is walked from `-64` up to `0`,
        // so anything outside that is not an offset into one. A positive value is legal and means
        // "the registers are spent"; it is normalised to 0 rather than refused, because that is what
        // the guest's own `va_arg` does with it. Anything *below* the area is the hostile case.
        Self::check_offset(gr_offs, GR_SAVE_BYTES, "__gr_offs", at, blame)?;
        Self::check_offset(vr_offs, VR_SAVE_BYTES, "__vr_offs", at, blame)?;

        Ok(Self {
            mem,
            blame,
            at,
            stack,
            gr_top,
            vr_top,
            gr_offs: gr_offs.min(0),
            vr_offs: vr_offs.min(0),
            index: 0,
        })
    }

    fn check_offset(
        value: i64,
        save_bytes: usize,
        field: &'static str,
        at: GuestAddr,
        blame: Blame<'_>,
    ) -> AbiResult<()> {
        let low = -(save_bytes as i64);
        // The high bound is generous on purpose: a guest whose registers are spent leaves a small
        // positive value here, and the amount is an implementation detail of its compiler. What is
        // refused is a value that would make the *subtraction* reach outside the area.
        let high = save_bytes as i64;
        if value < low || value > high {
            return Err(AbiError::BadVaList {
                symbol: blame.symbol.to_string(),
                address: blame.address,
                pointer: at,
                field,
                value,
                low,
                high,
            });
        }
        Ok(())
    }

    /// Where the record lives, for a diagnostic.
    #[must_use]
    pub fn address(&self) -> GuestAddr {
        self.at
    }

    /// How many arguments have been taken.
    #[must_use]
    pub fn taken(&self) -> usize {
        self.index
    }

    fn blame(&self) -> Blame<'a> {
        self.blame
    }

    /// `va_arg(ap, long)` / `va_arg(ap, void*)`.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`] if the save area or the overflow area is not readable where the next
    /// argument would be — which is the hostile `__gr_top` case, and the honest answer to a format
    /// string claiming more arguments than the caller passed.
    pub fn next_u64(&mut self) -> AbiResult<u64> {
        let value = if self.gr_offs < 0 {
            // `__gr_top + __gr_offs` with `__gr_offs` negative. Done in signed arithmetic and then
            // range-checked back into a `usize`, so a `__gr_top` small enough that the sum is
            // negative is a refusal rather than a wrap into the top of the address space.
            let at = self.offset_from(self.gr_top, self.gr_offs, SaveBank::General)?;
            let value = self.mem.read_u64(at, self.blame())?;
            self.gr_offs += GR_SLOT as i64;
            value
        } else {
            let at = self.aligned_stack(GR_SLOT)?;
            let value = self.mem.read_u64(at, self.blame())?;
            self.stack = self.advance_stack(at, GR_SLOT)?;
            value
        };
        self.index += 1;
        Ok(value)
    }

    /// `va_arg(ap, int)`.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](GuestVaList::next_u64).
    pub fn next_i32(&mut self) -> AbiResult<i32> {
        Ok(self.next_u64()? as u32 as i32)
    }

    /// `va_arg(ap, void*)`.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](GuestVaList::next_u64).
    pub fn next_pointer(&mut self) -> AbiResult<GuestAddr> {
        Ok(self.next_u64()? as GuestAddr)
    }

    /// `va_arg(ap, double)` — and there is no `va_arg(ap, float)`, by promotion.
    ///
    /// The save-area step is [`VR_SLOT`] and not [`GR_SLOT`]: a `double` occupies a whole `Q`
    /// register's worth of the area.
    ///
    /// # Errors
    ///
    /// As [`next_u64`](GuestVaList::next_u64).
    pub fn next_f64(&mut self) -> AbiResult<f64> {
        let bits = if self.vr_offs < 0 {
            let at = self.offset_from(self.vr_top, self.vr_offs, SaveBank::Simd)?;
            // The low 8 bytes of the 16-byte slot: little-endian, so the `double` is at the bottom.
            let value = self.mem.read_u64(at, self.blame())?;
            self.vr_offs += VR_SLOT as i64;
            value
        } else {
            let at = self.aligned_stack(GR_SLOT)?;
            let value = self.mem.read_u64(at, self.blame())?;
            self.stack = self.advance_stack(at, GR_SLOT)?;
            value
        };
        self.index += 1;
        Ok(f64::from_bits(bits))
    }

    /// `top + offs` with `offs` negative, refusing a sum that leaves the address space.
    ///
    /// `bank` names the field for the error message and carries its own lower bound. It is passed in
    /// rather than derived: the previous form asked `core::ptr::eq(&self.gr_top, &top)`, and because
    /// `top` arrives **by value** that compares the address of a stack local against a field of
    /// `self` and is therefore always false. Every `__gr_top` refusal was reported as `__vr_top`,
    /// with the VR bounds — and the only test on this path asserts just
    /// `matches!(error, AbiError::BadVaList { .. })`, so it could not fail on it.
    fn offset_from(&self, top: GuestAddr, offs: i64, bank: SaveBank) -> AbiResult<GuestAddr> {
        let sum = i128::from(top as u64) + i128::from(offs);
        u64::try_from(sum)
            .ok()
            .and_then(|value| GuestAddr::try_from(value).ok())
            .ok_or_else(|| AbiError::BadVaList {
                symbol: self.blame.symbol.to_string(),
                address: self.blame.address,
                pointer: self.at,
                field: bank.field(),
                value: offs,
                low: -(bank.save_bytes() as i64),
                high: 0,
            })
    }

    /// `self.stack` rounded up to `align`, refusing a round-up that leaves the address space.
    ///
    /// `__stack` is read verbatim out of guest memory and gets no range check — only the two `int`
    /// offsets do — so this arithmetic is on a value the guest chose. `(x + align - 1)` on
    /// `GuestAddr::MAX` is an overflow: a panic in a build with overflow checks on, and a wrap to a
    /// small address in one without. Both are wrong; a typed refusal is the answer.
    fn aligned_stack(&self, align: usize) -> AbiResult<GuestAddr> {
        self.stack
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.stack_out_of_space(self.stack, align))
    }

    /// `at + step`, refusing a sum that leaves the address space.
    fn advance_stack(&self, at: GuestAddr, step: usize) -> AbiResult<GuestAddr> {
        at.checked_add(step).ok_or_else(|| self.stack_out_of_space(at, step))
    }

    fn stack_out_of_space(&self, pointer: GuestAddr, len: usize) -> AbiError {
        AbiError::BadPointer {
            symbol: self.blame.symbol.to_string(),
            address: self.blame.address,
            argument: self.index,
            pointer,
            len,
            access: "reading",
            refusal: Refusal::NotMapped.into(),
        }
    }
}

/// Which register save area an offset belongs to.
///
/// Exists so the field name and its lower bound travel together: they are two halves of one fact and
/// were previously computed independently, which is how the name came to be wrong while the bound
/// stayed right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveBank {
    /// `X0`-`X7`, stepped by [`GR_SLOT`].
    General,
    /// `Q0`-`Q7`, stepped by [`VR_SLOT`].
    Simd,
}

impl SaveBank {
    /// The `va_list` field this bank's top pointer lives in.
    fn field(self) -> &'static str {
        match self {
            SaveBank::General => "__gr_top",
            SaveBank::Simd => "__vr_top",
        }
    }

    /// How far below the top this bank's offsets may legally reach.
    fn save_bytes(self) -> usize {
        match self {
            SaveBank::General => GR_SAVE_BYTES,
            SaveBank::Simd => VR_SAVE_BYTES,
        }
    }
}

impl core::fmt::Debug for GuestVaList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GuestVaList")
            .field("at", &format_args!("{:#x}", self.at))
            .field("__stack", &format_args!("{:#x}", self.stack))
            .field("__gr_top", &format_args!("{:#x}", self.gr_top))
            .field("__vr_top", &format_args!("{:#x}", self.vr_top))
            .field("__gr_offs", &self.gr_offs)
            .field("__vr_offs", &self.vr_offs)
            .field("taken", &self.index)
            .finish()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::Args;
    use omni_cpu::{ThunkCall, ThunkContext, ThunkRegs};
    use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};
    use std::sync::Arc;

    #[derive(Default)]
    struct Frame {
        x: [u64; 31],
        v: [u128; 32],
        sp: GuestAddr,
    }

    impl ThunkRegs for Frame {
        fn x(&self, i: u32) -> u64 {
            self.x.get(i as usize).copied().unwrap_or(0)
        }
        fn set_x(&mut self, i: u32, value: u64) {
            if let Some(s) = self.x.get_mut(i as usize) {
                *s = value;
            }
        }
        fn v(&self, i: u32) -> u128 {
            self.v.get(i as usize).copied().unwrap_or(0)
        }
        fn set_v(&mut self, i: u32, value: u128) {
            if let Some(s) = self.v.get_mut(i as usize) {
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
        scratch: GuestAddr,
        len: usize,
        unmapped: GuestAddr,
    }

    fn fixture() -> Fixture {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let page = space.page_size();
        let scratch = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page * 4,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("scratch guest memory");
        let unmapped = space
            .regions()
            .into_iter()
            .find(|r| r.is_free() && r.len >= page)
            .map(|r| r.start + r.len / 2)
            .expect("free address space");
        Fixture { mem: GuestMem::new(space), scratch, len: page * 4, unmapped }
    }

    fn blame() -> Blame<'static> {
        Blame::new("fprintf", 0x4000, 2)
    }

    // ------------------------------------------------------------------ the true-variadic direction

    /// `fprintf(f, "%d %f %s", 7, 2.5, p)`: the shape nine reachable imports have.
    ///
    /// Two named arguments spend `X0` and `X1`, so the variadic integers start at `X2` — and the
    /// variadic `double` starts at **`V0`**, because the named arguments spent no `V` registers. That
    /// is the AAPCS64 rule, and the rule an implementation written from Apple's arm64 (all varargs on
    /// the stack) or Microsoft's (varargs floating point in `X`) would get wrong.
    #[test]
    fn a_variadic_call_reads_its_extra_arguments_from_x2_onward_and_v0_onward() {
        let f = fixture();
        let mut frame = Frame { sp: f.scratch, ..Frame::default() };
        frame.set_x(0, 0xF11E); // FILE*
        frame.set_x(1, 0xFEED); // const char* format
        frame.set_x(2, 7); // %d
        frame.set_x(3, 0xDA7A); // %s
        frame.set_v(0, u128::from(2.5f64.to_bits())); // %f

        let call = ThunkCall::new(&mut frame, 0x4000, ThunkContext::default());
        let mut args = Args::new(&call, &f.mem, Blame::new("fprintf", 0x4000, 0));
        assert_eq!(args.next_pointer().expect("FILE*"), 0xF11E);
        assert_eq!(args.next_pointer().expect("format"), 0xFEED);
        let (consumed, overflow) = (args.consumed(), args.overflow());
        assert_eq!(consumed, (2, 0), "two named integers, no named floats");

        let mut va = VarArgs::new(&call, &f.mem, blame(), consumed, overflow);
        assert_eq!(va.next_i32().expect("%d"), 7);
        assert_eq!(va.next_f64().expect("%f"), 2.5, "a variadic double is in V0, not on the stack");
        assert_eq!(va.next_pointer().expect("%s"), 0xDA7A);
        assert_eq!(va.taken(), 3);
    }

    /// A `float` in the variadic part has been promoted to `double` by the caller. Reading it as a
    /// `float` would read the low 32 bits of the `double` — `0x00000000` for `1.0` — and return `0.0`
    /// with no error anywhere, which is precisely the silent-wrong-number class.
    #[test]
    fn a_variadic_float_has_been_promoted_and_is_read_as_a_double() {
        let f = fixture();
        let mut frame = Frame { sp: f.scratch, ..Frame::default() };
        // What the guest's compiler emits for `printf("%f", 1.0f)`: `FCVT D0, S0`.
        frame.set_v(0, u128::from(1.0f64.to_bits()));
        let call = ThunkCall::new(&mut frame, 0x4000, ThunkContext::default());
        let mut va = VarArgs::new(&call, &f.mem, blame(), (1, 0), f.scratch);
        assert_eq!(va.next_f64().expect("%f"), 1.0);
        // And the trap, stated: the low half of that pattern on its own is not 1.0f.
        assert_ne!(f32::from_bits(1.0f64.to_bits() as u32), 1.0f32);
        assert_eq!(f32::from_bits(1.0f64.to_bits() as u32), 0.0f32, "it is exactly zero");
    }

    /// Past the eighth register the variadic part goes to the overflow area, in 8-byte slots — and a
    /// `double` there takes 8 bytes, not the 16 it takes in a *save area*.
    #[test]
    fn variadic_arguments_past_the_registers_take_eight_byte_overflow_slots() {
        let f = fixture();
        let mut frame = Frame { sp: f.scratch, ..Frame::default() };
        for i in 0..8u32 {
            frame.set_x(i, u64::from(i));
            frame.set_v(i, u128::from(f64::from(i).to_bits()));
        }
        f.mem.write_u64(f.scratch, 99, blame()).expect("write");
        f.mem.write_u64(f.scratch + 8, 8.5f64.to_bits(), blame()).expect("write");

        let call = ThunkCall::new(&mut frame, 0x4000, ThunkContext::default());
        let mut va = VarArgs::new(&call, &f.mem, blame(), (8, 8), f.scratch);
        assert_eq!(va.next_u64().expect("overflow integer"), 99);
        assert_eq!(
            va.next_f64().expect("overflow double"),
            8.5,
            "the overflow slot is 8 bytes; VR_SLOT's 16 is the save area's"
        );
    }

    /// A format string that claims more arguments than the caller passed walks off the end of the
    /// guest's own frame. It must be a typed error naming the symbol, not a read of the caller's
    /// locals presented as an argument.
    #[test]
    fn a_variadic_call_that_runs_past_its_overflow_area_is_a_typed_error() {
        let f = fixture();
        let mut frame = Frame { sp: f.unmapped, ..Frame::default() };
        let call = ThunkCall::new(&mut frame, 0x4000, ThunkContext::default());
        let mut va = VarArgs::new(&call, &f.mem, blame(), (8, 8), f.unmapped);
        let error = va.next_u64().expect_err("an unmapped overflow area must be refused");
        assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
        assert_eq!(error.symbol(), Some("fprintf"));
    }

    // ------------------------------------------------------------------- the va_list direction

    /// Build the `va_list` a guest's own `vsnprintf` caller would have built.
    ///
    /// `named_gr` named integer arguments means `__gr_offs` starts at `-(8 - named_gr) * 8`, which is
    /// the callee prologue's own arithmetic.
    fn write_va_list(
        f: &Fixture,
        at: GuestAddr,
        gr_save: GuestAddr,
        vr_save: GuestAddr,
        overflow: GuestAddr,
        named_gr: usize,
        named_vr: usize,
    ) {
        let b = blame();
        f.mem.write_u64(at + field::STACK, overflow as u64, b).expect("write __stack");
        f.mem
            .write_u64(at + field::GR_TOP, (gr_save + GR_SAVE_BYTES) as u64, b)
            .expect("write __gr_top");
        f.mem
            .write_u64(at + field::VR_TOP, (vr_save + VR_SAVE_BYTES) as u64, b)
            .expect("write __vr_top");
        let gr_offs = -((ARG_REGISTERS as i32 - named_gr as i32) * GR_SLOT as i32);
        let vr_offs = -((ARG_REGISTERS as i32 - named_vr as i32) * VR_SLOT as i32);
        f.mem.write_u32(at + field::GR_OFFS, gr_offs as u32, b).expect("write __gr_offs");
        f.mem.write_u32(at + field::VR_OFFS, vr_offs as u32, b).expect("write __vr_offs");
    }

    /// `vsnprintf(buf, n, "%d %f", ap)`: three named arguments, so the walk starts at the fourth
    /// integer slot of the save area and the first floating-point one.
    #[test]
    fn a_guest_va_list_is_walked_from_the_slot_the_named_arguments_stopped_at() {
        let f = fixture();
        let va_at = f.scratch;
        let gr_save = f.scratch + 64;
        let vr_save = f.scratch + 256;
        let overflow = f.scratch + 1024;
        // Three named integer arguments (buf, n, format) and none floating point.
        write_va_list(&f, va_at, gr_save, vr_save, overflow, 3, 0);
        // The guest's prologue spilled X0-X7 here; the variadic ones start at index 3.
        for i in 0..8u64 {
            f.mem.write_u64(gr_save + i as usize * GR_SLOT, 700 + i, blame()).expect("write");
        }
        for i in 0..8u64 {
            f.mem
                .write_u64(vr_save + i as usize * VR_SLOT, (f64::from(i as u32) + 0.5).to_bits(), blame())
                .expect("write");
        }

        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("a well-formed va_list");
        assert_eq!(va.address(), va_at);
        assert_eq!(va.next_u64().expect("%d"), 703, "the fourth integer slot, X3's");
        assert_eq!(va.next_f64().expect("%f"), 0.5, "the first floating-point slot, Q0's");
        assert_eq!(va.next_u64().expect("the next"), 704);
        assert_eq!(va.next_f64().expect("the next"), 1.5, "stepped by 16, not by 8");
        assert_eq!(va.taken(), 4);
    }

    /// The step in the SIMD save area is 16. A walker that stepped by 8 would land in the first
    /// argument's upper lanes, which are zero — so it would return `0.0` and no error.
    #[test]
    fn the_simd_save_area_is_stepped_by_sixteen_bytes_and_not_by_eight() {
        assert_eq!(VR_SLOT, 16);
        assert_eq!(GR_SLOT, 8);
        let f = fixture();
        let (va_at, gr_save, vr_save) = (f.scratch, f.scratch + 64, f.scratch + 256);
        write_va_list(&f, va_at, gr_save, vr_save, f.scratch + 1024, 0, 0);
        // Only the 16-byte-aligned slots hold values; the 8-byte offsets between them are left zero,
        // which is exactly what a `Q` register spill leaves behind.
        f.mem.write_u64(vr_save, 1.25f64.to_bits(), blame()).expect("write");
        f.mem.write_u64(vr_save + 16, 2.25f64.to_bits(), blame()).expect("write");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("va_list");
        assert_eq!(va.next_f64().expect("first"), 1.25);
        assert_eq!(va.next_f64().expect("second"), 2.25, "a step of 8 would have read 0.0 here");
    }

    /// Once the save area is exhausted the walk moves to `__stack`, and the two areas must join up
    /// without repeating or skipping an argument.
    #[test]
    fn a_va_list_walk_continues_into_the_overflow_area_when_the_save_area_runs_out() {
        let f = fixture();
        let (va_at, gr_save, vr_save) = (f.scratch, f.scratch + 64, f.scratch + 256);
        let overflow = f.scratch + 1024;
        // Eight named integers, so the general-purpose save area is already spent.
        write_va_list(&f, va_at, gr_save, vr_save, overflow, 8, 8);
        f.mem.write_u64(overflow, 4242, blame()).expect("write");
        f.mem.write_u64(overflow + 8, 4243, blame()).expect("write");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("va_list");
        assert_eq!(va.next_u64().expect("first"), 4242);
        assert_eq!(va.next_u64().expect("second"), 4243);
    }

    /// **The hostile `va_list`.** Guest code wrote all five fields, so an offset outside what a save
    /// area can have must be refused by name before it is ever added to a pointer.
    #[test]
    fn an_offset_outside_the_save_area_is_refused_and_names_the_field() {
        let f = fixture();
        let (va_at, gr_save, vr_save) = (f.scratch, f.scratch + 64, f.scratch + 256);
        write_va_list(&f, va_at, gr_save, vr_save, f.scratch + 1024, 0, 0);

        for (offset, field, bad) in [
            (field::GR_OFFS, "__gr_offs", -2_000_000_000i32),
            (field::GR_OFFS, "__gr_offs", -65),
            (field::VR_OFFS, "__vr_offs", -129),
            (field::VR_OFFS, "__vr_offs", i32::MIN),
        ] {
            f.mem.write_u32(va_at + offset, bad as u32, blame()).expect("write");
            let error = GuestVaList::read(&f.mem, va_at, blame())
                .expect_err("an out-of-range offset must be refused");
            match error {
                AbiError::BadVaList { field: named, value, .. } => {
                    assert_eq!(named, field, "{bad}");
                    assert_eq!(value, i64::from(bad));
                }
                other => panic!("{bad}: {other:?}"),
            }
            // Put it back, so the next iteration tests one field at a time.
            write_va_list(&f, va_at, gr_save, vr_save, f.scratch + 1024, 0, 0);
        }
    }

    /// A positive offset is legal and means "the registers are spent". It is normalised, not refused:
    /// refusing it would refuse a correct guest whose compiler leaves a small positive value there.
    #[test]
    fn a_positive_offset_means_the_registers_are_spent_and_is_not_an_error() {
        let f = fixture();
        let (va_at, gr_save, vr_save) = (f.scratch, f.scratch + 64, f.scratch + 256);
        let overflow = f.scratch + 1024;
        write_va_list(&f, va_at, gr_save, vr_save, overflow, 0, 0);
        f.mem.write_u32(va_at + field::GR_OFFS, 8u32, blame()).expect("write a positive offset");
        f.mem.write_u64(overflow, 777, blame()).expect("write");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("accepted");
        assert_eq!(va.next_u64().expect("from the overflow area"), 777);
    }

    /// A `__gr_top` that is garbage passes the offset check — the offsets are fine — so the second
    /// defence has to be the one that catches it.
    #[test]
    fn a_wild_save_area_pointer_is_caught_by_the_memory_check_and_not_by_the_offset_check() {
        let f = fixture();
        let va_at = f.scratch;
        write_va_list(&f, va_at, f.scratch + 64, f.scratch + 256, f.scratch + 1024, 0, 0);
        f.mem
            .write_u64(va_at + field::GR_TOP, f.unmapped as u64, blame())
            .expect("point __gr_top at nothing");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("the offsets are still fine");
        let error = va.next_u64().expect_err("the read must be refused");
        assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    }

    /// A `__gr_top` so small that `__gr_top + __gr_offs` goes negative must not wrap into the top of
    /// the address space, where something of the host's might be mapped.
    #[test]
    fn a_save_area_pointer_small_enough_to_underflow_is_refused_rather_than_wrapping() {
        let f = fixture();
        let va_at = f.scratch;
        write_va_list(&f, va_at, f.scratch + 64, f.scratch + 256, f.scratch + 1024, 0, 0);
        f.mem.write_u64(va_at + field::GR_TOP, 8, blame()).expect("__gr_top = 8");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("offsets fine");
        let error = va.next_u64().expect_err("8 - 64 must not become 0xFFFF_FFFF_FFFF_FFC8");
        assert!(matches!(error, AbiError::BadVaList { .. }), "{error:?}");
    }

    /// The `VarArgs` overflow area starts from the guest's `SP` too, and had the same unchecked
    /// round-up as the `va_list` walk.
    #[test]
    fn a_varargs_overflow_area_at_the_top_of_the_address_space_is_refused() {
        let f = fixture();
        let mut frame = Frame { sp: f.scratch, ..Frame::default() };
        let call = ThunkCall::new(&mut frame, 0x4000, ThunkContext::default());
        // (8, 8): every argument register is spent, so the next one must come from the overflow area.
        let mut va = VarArgs::new(&call, &f.mem, blame(), (8, 8), GuestAddr::MAX);
        let error = va.next_u64().expect_err("aligning MAX up to 8 must be refused");
        match error {
            AbiError::BadPointer { pointer, .. } => assert_eq!(
                pointer,
                GuestAddr::MAX,
                "the refusal must name the address the guest gave, not a wrapped one",
            ),
            other => panic!("expected BadPointer, got {other:?}"),
        }
    }

    /// And through the floating-point taker, which reaches the overflow area by its own route.
    #[test]
    fn a_varargs_overflow_area_at_the_top_is_refused_for_doubles_too() {
        let f = fixture();
        let mut frame = Frame { sp: f.scratch, ..Frame::default() };
        let call = ThunkCall::new(&mut frame, 0x4000, ThunkContext::default());
        let mut va = VarArgs::new(&call, &f.mem, blame(), (8, 8), GuestAddr::MAX);
        let error = va.next_f64().expect_err("aligning MAX up to 8 must be refused");
        match error {
            AbiError::BadPointer { pointer, .. } => assert_eq!(pointer, GuestAddr::MAX),
            other => panic!("expected BadPointer, got {other:?}"),
        }
    }

    /// A `__stack` at the very top of the address space must be refused, not aligned into a wrap.
    ///
    /// `__stack` is read verbatim out of guest memory and, unlike the two `int` offsets, gets no
    /// range check — so `align_up(__stack, 8)` was arithmetic on a guest-chosen value.
    /// `usize::MAX + 7` panics where overflow checks are on and wraps to `6` where they are not, and
    /// the wrap is the worse half: it turns a variadic argument read into a read near address zero.
    #[test]
    fn a_stack_pointer_at_the_top_of_the_address_space_is_refused_rather_than_overflowing() {
        let f = fixture();
        let va_at = f.scratch;
        // named_gr/named_vr = 8: every argument register is spent, so the walk must use `__stack`.
        write_va_list(&f, va_at, f.scratch + 64, f.scratch + 256, f.scratch + 1024, 8, 8);
        f.mem.write_u64(va_at + field::STACK, u64::MAX, blame()).expect("__stack = MAX");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("the offsets are still fine");
        let error = va.next_u64().expect_err("aligning MAX up to 8 must be refused");
        // The POINTER is asserted, not just the variant. Unchecked, `MAX + 7` wraps to 6 and the
        // subsequent `read_u64(6)` fails with `BadPointer` too — so a test that checked only the
        // variant would pass against the defect in any profile with overflow checks off, which is
        // this one's release profile. The wrapped address is small; the refused one is `MAX`.
        match error {
            AbiError::BadPointer { pointer, .. } => assert_eq!(
                pointer,
                GuestAddr::MAX,
                "the refusal must name the address the guest gave, not a wrapped one",
            ),
            other => panic!("expected BadPointer, got {other:?}"),
        }
    }

    /// The same, on the floating-point taker, which reaches `__stack` by its own route.
    #[test]
    fn a_stack_pointer_at_the_top_of_the_address_space_is_refused_for_doubles_too() {
        let f = fixture();
        let va_at = f.scratch;
        write_va_list(&f, va_at, f.scratch + 64, f.scratch + 256, f.scratch + 1024, 8, 8);
        f.mem.write_u64(va_at + field::STACK, u64::MAX, blame()).expect("__stack = MAX");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("the offsets are still fine");
        let error = va.next_f64().expect_err("aligning MAX up to 8 must be refused");
        match error {
            AbiError::BadPointer { pointer, .. } => assert_eq!(pointer, GuestAddr::MAX),
            other => panic!("expected BadPointer, got {other:?}"),
        }
    }

    /// An underflowing `__gr_top` must NAME `__gr_top` and carry the GENERAL bank's bound.
    ///
    /// The refusal used to select its field name with `core::ptr::eq(&self.gr_top, &top)`, and
    /// because `top` arrives by value that compares a stack local's address against a field of
    /// `self` — always false. Every general-bank refusal was reported as `__vr_top`, with the VR
    /// bounds. The existing underflow test asserts only `matches!(error, BadVaList { .. })`, so it
    /// could not fail on it; this one inspects the field.
    #[test]
    fn an_underflowing_gr_top_names_the_general_bank_and_not_the_simd_one() {
        let f = fixture();
        let va_at = f.scratch;
        write_va_list(&f, va_at, f.scratch + 64, f.scratch + 256, f.scratch + 1024, 0, 0);
        f.mem.write_u64(va_at + field::GR_TOP, 8, blame()).expect("__gr_top = 8");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("offsets fine");
        let error = va.next_u64().expect_err("8 - 64 underflows");
        match error {
            AbiError::BadVaList { field, low, .. } => {
                assert_eq!(field, "__gr_top", "the general bank must name itself");
                assert_eq!(
                    low,
                    -(GR_SAVE_BYTES as i64),
                    "and must carry the general bank's bound, not the SIMD one",
                );
            }
            other => panic!("expected BadVaList, got {other:?}"),
        }
    }

    /// And the SIMD bank still names itself — so the fix did not simply swap one wrong answer for
    /// another.
    #[test]
    fn an_underflowing_vr_top_names_the_simd_bank() {
        let f = fixture();
        let va_at = f.scratch;
        write_va_list(&f, va_at, f.scratch + 64, f.scratch + 256, f.scratch + 1024, 0, 0);
        f.mem.write_u64(va_at + field::VR_TOP, 8, blame()).expect("__vr_top = 8");
        let mut va = GuestVaList::read(&f.mem, va_at, blame()).expect("offsets fine");
        let error = va.next_f64().expect_err("8 - 128 underflows");
        match error {
            AbiError::BadVaList { field, low, .. } => {
                assert_eq!(field, "__vr_top");
                assert_eq!(low, -(VR_SAVE_BYTES as i64));
            }
            other => panic!("expected BadVaList, got {other:?}"),
        }
    }

    #[test]
    fn a_va_list_at_an_unmapped_address_is_a_bad_pointer() {
        let f = fixture();
        let error = GuestVaList::read(&f.mem, f.unmapped, blame()).expect_err("refused");
        assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    }

    /// The record is 32 bytes, which is what makes AAPCS64 pass it indirectly. A marshaller that
    /// thought it was 16 or fewer would read it out of the argument registers.
    #[test]
    fn the_va_list_is_larger_than_sixteen_bytes_which_is_why_it_arrives_as_a_pointer() {
        // Over sixteen, which is the threshold AAPCS64 passes a composite indirectly above.
        assert_eq!(VA_LIST_BYTES, 32);
        assert_eq!(field::VR_OFFS + 4, VA_LIST_BYTES, "the fields account for the whole record");
        assert_eq!(GR_SAVE_BYTES, 64);
        assert_eq!(VR_SAVE_BYTES, 128);
    }

    /// A walk that straddles the end of the mapping holding the record is refused whole. `f.len` is
    /// used so the test does not depend on a page size.
    #[test]
    fn a_va_list_straddling_the_end_of_its_mapping_is_refused() {
        let f = fixture();
        let error = GuestVaList::read(&f.mem, f.scratch + f.len - 16, blame())
            .expect_err("32 bytes will not fit in the last 16");
        assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    }
}
