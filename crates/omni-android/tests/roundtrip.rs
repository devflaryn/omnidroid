//! **Every argument shape, round-tripped through real translated ARM64 code.**
//!
//! Each test here writes A64 instructions, lets the translating backend run them, and asserts on the
//! *value* that came back — never on the absence of an error. A test that only checked that `run`
//! returned `Ok` would pass with a marshaller that read every argument out of `X0`.
//!
//! What is covered, in the order the brief lists it: integers, pointers, floating point, mixed, more
//! than eight arguments, a variadic call, a `va_list` a guest built, and a callback in each
//! direction. The hostile cases are in `hostile.rs`.
//!
//! ```text
//! cargo test -p omni-android --release
//! ```

#![cfg(target_arch = "x86_64")]

mod harness;

use std::sync::Mutex;

use harness::a64::*;
use harness::{serialized, v, x, Asm, Guest, BUDGET};
use omni_android::{AbiResult, GuestArg, GuestVaList, ImportCall, ReentrantCall};
use omni_cpu::{ExitReason, GuestCpu};

/// What a handler saw, so a test can assert on it.
///
/// A `static` because [`omni_android::ImportFn`] is a bare `fn` and captures nothing — which is
/// deliberate (see `omni_cpu::thunk`), and which is why every test in this binary takes
/// [`serialized`] first.
#[derive(Debug, Default)]
struct Seen {
    ints: Vec<u64>,
    floats: Vec<f64>,
    strings: Vec<String>,
    /// Whatever a reentrant handler got back from the guest.
    returned: Vec<u64>,
    returned_f: Vec<f64>,
    /// An argument read *after* a callback ran, to prove the snapshot works.
    after_callback: Option<u64>,
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

fn reset() {
    // Poison-tolerant, so one failing test's panic does not turn every other test in the binary into
    // a cascade of `PoisonError`s that hide the one real failure.
    *SEEN.lock().unwrap_or_else(|p| p.into_inner()) = Some(Seen::default());
}

fn with_seen<R>(f: impl FnOnce(&mut Seen) -> R) -> R {
    let mut guard = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.as_mut().expect("reset() was called"))
}

// ------------------------------------------------------------------------------------- integers

/// `long eight_ints(long, long, long, long, long, long, long, long)`.
fn eight_ints(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let values = {
        let mut args = call.args();
        let mut values = Vec::new();
        for _ in 0..8 {
            values.push(args.next_u64()?);
        }
        values
    };
    let sum = values.iter().copied().fold(0u64, u64::wrapping_add);
    with_seen(|seen| seen.ints = values);
    call.ret().u64(sum);
    Ok(())
}

#[test]
fn eight_integer_arguments_arrive_in_x0_to_x7_and_the_result_comes_back_in_x0() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("eight_ints", eight_ints).expect("bind");
    let boundary = builder.finish();

    // Values chosen so that no two are equal and none is a small integer: a marshaller that read the
    // wrong register, or the same one twice, cannot produce this multiset by accident.
    let expected: Vec<u64> =
        (0..8u64).map(|i| 0x1111_2222_3333_0000u64 + (i << 8) + i + 1).collect();

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for (index, value) in expected.iter().enumerate() {
        asm.mov(index as u32, *value);
    }
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    let program = guest.load(asm.words());
    assert_eq!(program, entry);

    let mut cpu = guest.thread(&boundary);
    let exit = boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");

    with_seen(|seen| assert_eq!(seen.ints, expected, "the handler must see X0-X7 in order"));
    let sum = expected.iter().copied().fold(0u64, u64::wrapping_add);
    assert_eq!(guest.read_u64(guest.data), sum, "the guest must see the value the handler returned");
    // The dispatch really was inline, which is the whole of D17. Without this the test would pass
    // identically if every call had gone out through `ExitReason::Thunk` three times more slowly.
    assert_eq!(cpu.inline_thunk_calls().serviced, 1);
    assert_eq!(cpu.inline_thunk_calls().deferred, 0);
    assert_eq!(boundary.crossings().exits, 0);
}

// ------------------------------------------------------------------------------------- pointers

/// `size_t upper(const char* in, char* out)` — reads a guest string, writes a guest buffer.
fn upper(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (input, output) = {
        let mut args = call.args();
        (args.next_pointer()?, args.next_pointer()?)
    };
    let text = call.mem().cstr(input, call.blame(0))?;
    let upper: Vec<u8> = text.iter().map(u8::to_ascii_uppercase).collect();
    call.mem().write_bytes(output, &upper, call.blame(1))?;
    with_seen(|seen| seen.strings.push(String::from_utf8_lossy(&text).into_owned()));
    call.ret().u64(upper.len() as u64);
    Ok(())
}

#[test]
fn a_pointer_argument_reaches_real_guest_memory_in_both_directions() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("upper", upper).expect("bind");
    let boundary = builder.finish();

    let input = guest.data + 0x100;
    let output = guest.data + 0x200;
    guest.write_bytes(input, b"libroblox.so\0");

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, input as u64);
    asm.mov(1, output as u64);
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    with_seen(|seen| assert_eq!(seen.strings, ["libroblox.so"]));
    assert_eq!(guest.read_u64(guest.data), 12, "the returned length");
    let written = guest.read_u64(output);
    assert_eq!(&written.to_le_bytes()[..8], b"LIBROBLO", "the handler's write landed in the guest");
}

// -------------------------------------------------------------------------------- floating point

/// `double eight_doubles(double, double, double, double, double, double, double, double)`.
fn eight_doubles(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let values = {
        let mut args = call.args();
        let mut values = Vec::new();
        for _ in 0..8 {
            values.push(args.next_f64()?);
        }
        values
    };
    let sum: f64 = values.iter().sum();
    with_seen(|seen| seen.floats = values);
    call.ret().f64(sum);
    Ok(())
}

#[test]
fn eight_double_arguments_arrive_in_v0_to_v7_and_the_result_comes_back_in_v0() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("eight_doubles", eight_doubles).expect("bind");
    let boundary = builder.finish();

    // Powers of two with distinct exponents, so the sum identifies the multiset exactly: a handler
    // that read one register twice would produce a different total, and one that read them in the
    // wrong order would not (which is what the per-value assertion below is for).
    let expected: Vec<f64> = (0..8).map(|i| 2.0f64.powi(i) + 0.5).collect();
    let inputs = guest.data + 0x100;
    for (index, value) in expected.iter().enumerate() {
        guest.write_f64(inputs + index * 8, *value);
    }

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, inputs as u64);
    for index in 0..8u32 {
        asm.push(ldr_d(index, 22, index * 8));
    }
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_d(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    with_seen(|seen| assert_eq!(seen.floats, expected, "the handler must see V0-V7 in order"));
    assert_eq!(guest.read_f64(guest.data), expected.iter().sum::<f64>());
    // The whole 128 bits of `V0` were written, upper lanes zeroed, which is what an AArch64 write to
    // `Dn` does — a guest doing `STR Q0` of the result would otherwise store stale lanes.
    assert_eq!(cpu.v(v(0)) >> 64, 0);
}

// ----------------------------------------------------------------------------------------- mixed

/// `double mixed(long a, double b, void* c, double d, int e)`.
///
/// The interleaving is the point: the two banks advance independently, so `c` is in `X1` and not in
/// `X2`, and `d` is in `V1` and not in `V3`.
fn mixed(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (a, b, c, d, e) = {
        let mut args = call.args();
        (
            args.next_u64()?,
            args.next_f64()?,
            args.next_pointer()?,
            args.next_f64()?,
            args.next_i32()?,
        )
    };
    with_seen(|seen| {
        seen.ints = vec![a, c as u64, e as u64];
        seen.floats = vec![b, d];
    });
    call.ret().f64(b + d + a as f64 + f64::from(e));
    Ok(())
}

#[test]
fn integers_and_doubles_interleaved_advance_their_own_banks() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("mixed", mixed).expect("bind");
    let boundary = builder.finish();

    let doubles = guest.data + 0x100;
    guest.write_f64(doubles, 1.25);
    guest.write_f64(doubles + 8, -4.5);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(23, doubles as u64);
    asm.push(ldr_d(0, 23, 0)); // b, the first double, in V0
    asm.push(ldr_d(1, 23, 8)); // d, the second, in V1
    asm.mov(0, 7); // a, the first integer, in X0
    asm.mov(1, 0xDEAD_0000); // c, the pointer, in X1
    asm.mov(2, 11); // e, the int, in X2
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_d(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    with_seen(|seen| {
        assert_eq!(seen.ints, [7, 0xDEAD_0000, 11], "X0, X1, X2 — the pointer is not in X2");
        assert_eq!(seen.floats, [1.25, -4.5], "V0, V1 — the second double is not in V3");
    });
    assert_eq!(guest.read_f64(guest.data), 1.25 + -4.5 + 7.0 + 11.0);
}

// ----------------------------------------------------------------------- more than eight arguments

/// `long ten_ints(long × 10)` — two of them on the stack.
fn ten_ints(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (values, overflow) = {
        let mut args = call.args();
        let mut values = Vec::new();
        for _ in 0..10 {
            values.push(args.next_u64()?);
        }
        let overflow = args.overflow();
        (values, overflow)
    };
    with_seen(|seen| {
        seen.ints = values.clone();
        seen.returned = vec![overflow as u64];
    });
    call.ret().u64(values.iter().copied().fold(0u64, u64::wrapping_add));
    Ok(())
}

#[test]
fn the_ninth_and_tenth_arguments_come_off_the_guests_own_stack() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("ten_ints", ten_ints).expect("bind");
    let boundary = builder.finish();

    let expected: Vec<u64> = (1..=10u64).map(|i| 0xAA00_0000_0000_0000 + i).collect();

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for index in 0..8u32 {
        asm.mov(index, expected[index as usize]);
    }
    // The ninth and tenth go in a 16-byte block the caller allocates below `SP`, which is what the
    // guest's own compiler emits: AArch64 has no red zone, so the space has to be made.
    asm.mov(9, expected[8]);
    asm.mov(10, expected[9]);
    asm.push(sub_imm(31, 31, 16));
    asm.push(str_imm(9, 31, 0));
    asm.push(str_imm(10, 31, 8));
    asm.bl(thunk);
    asm.push(add_imm(31, 31, 16));
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    with_seen(|seen| {
        assert_eq!(seen.ints, expected, "eight registers then two stack slots");
        assert_eq!(
            seen.returned[0] as usize,
            guest.stack_top,
            "two 8-byte slots above the SP the guest called with, which was 16 below the top"
        );
    });
    assert_eq!(guest.read_u64(guest.data), expected.iter().copied().fold(0u64, u64::wrapping_add));
    assert_eq!(cpu.sp(), guest.stack_top, "the guest's own SP arithmetic is undisturbed");
}

// -------------------------------------------------------------------------------------- variadic

/// `int log_like(void* stream, const char* fmt, ...)` with `%d`, `%f`, `%s`.
///
/// Shaped exactly like the nine reachable true-variadic imports: two named arguments, then whatever
/// the format string says.
fn log_like(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (stream, fmt, consumed, overflow) = {
        let mut args = call.args();
        let stream = args.next_pointer()?;
        let fmt = args.next_pointer()?;
        (stream, fmt, args.consumed(), args.overflow())
    };
    let format = call.mem().cstr(fmt, call.blame(1))?;
    let (d, f, s) = {
        let mut va = call.varargs(consumed, overflow, 2);
        (va.next_i32()?, va.next_f64()?, va.next_pointer()?)
    };
    let text = call.mem().cstr(s, call.blame(4))?;
    with_seen(|seen| {
        seen.ints = vec![stream as u64, u64::from(d as u32)];
        seen.floats = vec![f];
        seen.strings = vec![
            String::from_utf8_lossy(&format).into_owned(),
            String::from_utf8_lossy(&text).into_owned(),
        ];
    });
    call.ret().i32(format.len() as i32 + text.len() as i32);
    Ok(())
}

#[test]
fn a_variadic_call_reads_its_extra_arguments_from_the_registers_aapcs64_puts_them_in() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("log_like", log_like).expect("bind");
    let boundary = builder.finish();

    let fmt = guest.data + 0x100;
    let text = guest.data + 0x140;
    let float_at = guest.data + 0x180;
    guest.write_bytes(fmt, b"%d %f %s\0");
    guest.write_bytes(text, b"omnidroid\0");
    // A `float` in the source, which the variadic rules promote to `double` at the call site.
    guest.write_u32(float_at, 2.5f32.to_bits());

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(23, float_at as u64);
    asm.push(ldr_s(0, 23, 0));
    // **The instruction that makes this a variadic call rather than a fixed one.** The C default
    // argument promotions turn a `float` in the variadic part into a `double`, and this is the
    // conversion the guest's own compiler emits to do it.
    asm.push(fcvt_d_s(0, 0));
    asm.mov(0, 0xF11E); // the named FILE*
    asm.mov(1, fmt as u64); // the named format
    asm.mov(2, 42); // %d — the first variadic integer, in X2
    asm.mov(3, text as u64); // %s — the second, in X3
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    with_seen(|seen| {
        assert_eq!(seen.ints, [0xF11E, 42]);
        assert_eq!(
            seen.floats,
            [2.5],
            "the promoted double is in V0, which is where AAPCS64 puts a variadic floating-point \
             argument — Apple's arm64 would have put it on the stack and Windows on ARM64 in X4"
        );
        assert_eq!(seen.strings, ["%d %f %s", "omnidroid"]);
    });
    assert_eq!(guest.read_u64(guest.data), 8 + 9, "the int return, sign-extended into X0");
}

// ---------------------------------------------------------------------------- a guest-built va_list

/// `int v_like(char* buf, size_t n, const char* fmt, va_list ap)`.
///
/// The shape `vsnprintf`, `vfprintf`, `vasprintf` and `__vsnprintf_chk` have. `ap` is over 16 bytes,
/// so AAPCS64 passes it indirectly: `X3` holds a *pointer* to the record, not the record.
fn v_like(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (_buf, _n, _fmt, ap) = {
        let mut args = call.args();
        (args.next_pointer()?, args.next_u64()?, args.next_pointer()?, args.next_pointer()?)
    };
    let mut va = GuestVaList::read(call.mem(), ap, call.blame(3))?;
    let first = va.next_u64()?;
    let double = va.next_f64()?;
    let second = va.next_u64()?;
    with_seen(|seen| {
        seen.ints = vec![first, second];
        seen.floats = vec![double];
    });
    call.ret().i32(3);
    Ok(())
}

#[test]
fn a_va_list_the_guest_built_is_walked_through_both_of_its_save_areas() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("v_like", v_like).expect("bind");
    let boundary = builder.finish();

    let gr_save = guest.data + 0x100; // 64 bytes: X0-X7 as the guest's prologue spilled them
    let vr_save = guest.data + 0x200; // 128 bytes: Q0-Q7
    let overflow = guest.data + 0x400;
    let va_list = guest.data + 0x300;
    let double_at = guest.data + 0x80;
    guest.write_f64(double_at, -0.75);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, guest.data as u64);

    // **The guest writes its own register save area and va_list**, with ordinary stores, exactly as a
    // variadic callee's prologue does. Written by guest instructions rather than by the test, because
    // the point of `GuestVaList` is that every field of it is guest-controlled.
    asm.mov(9, 0x5151_0000_0000_0001);
    asm.push(str_imm(9, 22, 0x100)); // the first variadic integer, X0's slot
    asm.mov(9, 0x5151_0000_0000_0002);
    asm.push(str_imm(9, 22, 0x108)); // the second, X1's slot
    asm.push(ldr_d(3, 22, 0x80));
    asm.push(str_d(3, 22, 0x200)); // the variadic double, Q0's slot

    asm.mov(9, overflow as u64);
    asm.push(str_imm(9, 22, 0x300)); // __stack
    asm.mov(9, (gr_save + 64) as u64);
    asm.push(str_imm(9, 22, 0x308)); // __gr_top
    asm.mov(9, (vr_save + 128) as u64);
    asm.push(str_imm(9, 22, 0x310)); // __vr_top
    asm.mov(9, u64::from((-64i32) as u32));
    asm.push(str_w(9, 22, 0x318)); // __gr_offs: no named arguments consumed a register
    asm.mov(9, u64::from((-128i32) as u32));
    asm.push(str_w(9, 22, 0x31C)); // __vr_offs

    asm.mov(0, (guest.data + 0x500) as u64); // buf
    asm.mov(1, 64); // n
    asm.mov(2, (guest.data + 0x580) as u64); // fmt
    asm.mov(3, va_list as u64); // ap, indirectly
    asm.bl(thunk);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    with_seen(|seen| {
        assert_eq!(seen.ints, [0x5151_0000_0000_0001, 0x5151_0000_0000_0002]);
        assert_eq!(seen.floats, [-0.75], "read from Q0's 16-byte slot, not from an 8-byte step");
    });
    assert_eq!(guest.read_u64(guest.data), 3);
}

// ------------------------------------------------------------------------------ the PLT stub shape

#[test]
fn the_boundary_works_through_a_plt_stub_which_is_the_shape_the_loader_produces() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("eight_ints", eight_ints).expect("bind");
    let boundary = builder.finish();

    // A GOT slot the loader would have relocated to the thunk address, and the four-instruction stub
    // that reads it. The stub leaves through `BR` — an *indirect* terminal, which is a dispatcher
    // round trip rather than a linked jump, and therefore a different path through the backend from a
    // direct `BL`.
    let got = guest.data + 0x100;
    guest.write_u64(got, thunk as u64);
    let stub_at = guest.next_entry();
    guest.load(&plt_stub(stub_at, got));

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for index in 0..8u32 {
        asm.mov(index, u64::from(index) + 1);
    }
    asm.bl(stub_at);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");
    assert_eq!(guest.read_u64(guest.data), (1..=8u64).sum::<u64>());
    with_seen(|seen| assert_eq!(seen.ints, (1..=8u64).collect::<Vec<_>>()));
    assert_eq!(cpu.inline_thunk_calls().serviced, 1, "serviced inline, through the stub");
}

// ------------------------------------------------------------------- host to guest: the callbacks

/// `int sort_like(int (*cmp)(const void*, const void*), const void* a, const void* b)`.
///
/// A `qsort` comparator, called from Rust. The reason this handler is on the exit path and not the
/// inline one: it re-enters the guest, and an inline handler structurally cannot.
fn sort_like(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (cmp, a, b) = {
        let mut args = call.args();
        (args.next_pointer()?, args.next_pointer()?, args.next_pointer()?)
    };
    let result = call.call_guest(cmp, &[GuestArg::Pointer(a), GuestArg::Pointer(b)], BUDGET)?;
    // Re-read the whole argument list *after* the callback ran. It comes out of the snapshot taken
    // before the handler started, which is the only reason `X0` is still the comparator's address —
    // the CPU's own `X0` now holds the comparator's return value.
    let again = {
        let mut args = call.args();
        (args.next_pointer()?, args.next_pointer()?, args.next_pointer()?)
    };
    with_seen(|seen| {
        seen.returned = vec![u64::from(result.as_i32() as u32)];
        seen.ints = vec![again.0 as u64, again.1 as u64, again.2 as u64];
        seen.after_callback = Some(again.2 as u64);
    });
    call.ret(|mut ret| ret.i32(result.as_i32()));
    Ok(())
}

#[test]
fn a_guest_comparator_is_callable_from_rust_and_its_sign_survives() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("sort_like", sort_like).expect("bind");
    let boundary = builder.finish();

    // The comparator: `return *(long*)a - *(long*)b;`
    let cmp_at = guest.next_entry();
    let mut cmp = Asm::at(cmp_at);
    cmp.push(ldr_imm(3, 0, 0));
    cmp.push(ldr_imm(4, 1, 0));
    cmp.push(sub_reg(0, 3, 4));
    cmp.push(ret(30));
    guest.load(cmp.words());

    let a = guest.data + 0x100;
    let b = guest.data + 0x108;

    for (left, right, expected) in [(10i64, 4i64, 6i32), (4, 10, -6), (7, 7, 0)] {
        reset();
        guest.write_u64(a, left as u64);
        guest.write_u64(b, right as u64);

        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, cmp_at as u64);
        asm.mov(1, a as u64);
        asm.mov(2, b as u64);
        asm.bl(thunk);
        asm.mov(22, guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        guest.load(asm.words());

        let mut cpu = guest.thread(&boundary);
        boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

        assert_eq!(
            guest.read_u64(guest.data) as u32 as i32,
            expected,
            "comparing {left} with {right}"
        );
        with_seen(|seen| {
            assert_eq!(seen.returned, [u64::from(expected as u32)]);
            assert_eq!(
                seen.ints,
                [cmp_at as u64, a as u64, b as u64],
                "every argument, re-read after the callback ran, must still be itself — the                  snapshot is what makes that true, and the CPU's own X0 now holds the                  comparator's answer"
            );
            assert_eq!(seen.after_callback, Some(b as u64));
        });
    }
    assert!(boundary.crossings().exits >= 3);
    assert_eq!(boundary.crossings().guest_calls, 3);
    assert_eq!(boundary.crossings().deepest, 1);
}

/// `void atexit_like(void (*handler)(void))` — zero arguments, no return value.
fn atexit_like(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let handler = call.args().next_pointer()?;
    call.call_guest(handler, &[], BUDGET)?;
    Ok(())
}

#[test]
fn a_zero_argument_guest_handler_runs_and_its_side_effect_is_visible() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("atexit_like", atexit_like).expect("bind");
    let boundary = builder.finish();

    let marker = guest.data + 0x100;
    guest.write_u64(marker, 0);

    // The handler: `*(long*)marker = 0xA7E317;`
    let handler_at = guest.next_entry();
    let mut handler = Asm::at(handler_at);
    handler.mov(9, marker as u64);
    handler.mov(10, 0x00A7_E317);
    handler.push(str_imm(10, 9, 0));
    handler.push(ret(30));
    guest.load(handler.words());

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, handler_at as u64);
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");
    assert_eq!(guest.read_u64(marker), 0x00A7_E317, "the atexit handler really ran");
    assert_eq!(boundary.crossings().guest_calls, 1);
}

/// `double scale_like(double (*f)(double), double x)` — a callback taking and returning a `double`.
fn scale_like(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (f, x) = {
        let mut args = call.args();
        (args.next_pointer()?, args.next_f64()?)
    };
    let result = call.call_guest(f, &[GuestArg::Double(x)], BUDGET)?;
    with_seen(|seen| seen.returned_f = vec![result.as_f64()]);
    call.ret(|mut ret| ret.f64(result.as_f64()));
    Ok(())
}

#[test]
fn a_guest_callback_takes_and_returns_a_double_through_v0() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("scale_like", scale_like).expect("bind");
    let boundary = builder.finish();

    let three = guest.data + 0x100;
    guest.write_f64(three, 3.0);

    // `double f(double x) { return x * 3.0; }`
    let f_at = guest.next_entry();
    let mut f = Asm::at(f_at);
    f.mov(9, three as u64);
    f.push(ldr_d(1, 9, 0));
    f.push(fmul_d(0, 0, 1));
    f.push(ret(30));
    guest.load(f.words());

    let arg = guest.data + 0x108;
    guest.write_f64(arg, 1.5);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(9, arg as u64);
    asm.push(ldr_d(0, 9, 0));
    asm.mov(0, f_at as u64);
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_d(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");
    with_seen(|seen| assert_eq!(seen.returned_f, [4.5]));
    assert_eq!(guest.read_f64(guest.data), 4.5, "1.5 × 3.0, through V0 in both directions");
}

/// The `pthread` entry-point shape: a guest function entered from Rust on a **fresh context**, with
/// its argument in `X0`, returning a pointer.
///
/// Done at the test level rather than from a handler because a new guest thread needs a new
/// `GuestCpu`, which comes from the backend — and a handler, being a bare `fn`, holds no backend. The
/// mechanism is the same one [`ReentrantCall::call_guest`] uses: arguments placed per AAPCS64, `X30`
/// on the boundary's sentinel, and the return read out of `X0`.
#[test]
fn a_pthread_entry_point_is_callable_from_rust_on_a_fresh_context() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("eight_ints", eight_ints).expect("bind");
    let boundary = builder.finish();

    // `void* entry(void* arg) { *(long*)arg = 0x7357; eight_ints(...); return arg + 8; }` — it calls
    // an import, because a real thread entry point does, and the boundary has to service that on a
    // context nothing has run before.
    let entry_at = guest.next_entry();
    let mut body = Asm::at(entry_at);
    body.push(mov_reg(21, 30));
    body.push(mov_reg(23, 0));
    body.mov(9, 0x7357);
    body.push(str_imm(9, 23, 0));
    for index in 0..8u32 {
        body.mov(index, u64::from(index) + 1);
    }
    body.bl(thunk);
    body.push(add_imm(0, 23, 8));
    body.push(ret(21));
    guest.load(body.words());

    let arg = guest.data + 0x100;
    let mut cpu = guest.thread(&boundary);
    cpu.set_x(x(0), arg as u64);
    let exit = boundary.run(&mut cpu, entry_at, BUDGET).expect("the thread body must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    assert_eq!(guest.read_u64(arg), 0x7357, "the thread body ran");
    assert_eq!(cpu.x(x(0)) as usize, arg + 8, "and its return value is in X0");
    with_seen(|seen| assert_eq!(seen.ints.len(), 8, "and its imported call was serviced"));
}

// --------------------------------------------------------------------------------- re-entrancy

/// A handler that calls a guest function which itself calls an **inline** import.
///
/// guest → host → guest → host, which is the nesting a `pthread_once` initialiser or a `qsort`
/// comparator produces in real code.
fn nest_like(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let target = call.args().next_pointer()?;
    let result = call.call_guest(target, &[GuestArg::Int(0x1234)], BUDGET)?;
    with_seen(|seen| seen.returned.push(result.x0));
    call.ret(|mut ret| ret.u64(result.x0));
    Ok(())
}

#[test]
fn a_guest_callback_may_itself_call_an_imported_symbol() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let outer = builder.bind_reentrant("nest_like", nest_like).expect("bind");
    let inner = builder.bind_inline("eight_ints", eight_ints).expect("bind");
    let boundary = builder.finish();

    // The callback: set up eight arguments, call the inline import, return what it returned.
    let callback_at = guest.next_entry();
    let mut callback = Asm::at(callback_at);
    callback.push(mov_reg(21, 30));
    for index in 0..8u32 {
        callback.mov(index, 100 + u64::from(index));
    }
    callback.bl(inner);
    callback.push(ret(21));
    guest.load(callback.words());

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, callback_at as u64);
    asm.bl(outer);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    let expected: u64 = (100..108u64).sum();
    assert_eq!(guest.read_u64(guest.data), expected);
    with_seen(|seen| {
        assert_eq!(seen.ints, (100..108u64).collect::<Vec<_>>(), "the inner import ran");
        assert_eq!(seen.returned, [expected], "and the outer handler got its answer back");
    });
    assert_eq!(boundary.crossings().deepest, 1);
    assert_eq!(cpu.inline_thunk_calls().serviced, 1, "the inner call was still dispatched inline");
}

/// Everything the outer guest frame owns has to be where it left it after a callback.
#[test]
fn a_callback_leaves_the_outer_guests_registers_and_stack_exactly_as_it_found_them() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("nest_like", nest_like).expect("bind");
    let boundary = builder.finish();

    // A callback that clobbers everything AAPCS64 says it may, and some it may not: X0-X18 and V0-V7
    // are fair game for a well-behaved callee, and a guest callback is untrusted code that may go
    // further. The outer frame is entitled to find its registers unchanged either way.
    let callback_at = guest.next_entry();
    let mut callback = Asm::at(callback_at);
    for index in 0..=28u32 {
        callback.mov(index, 0xBAD0_0000_0000_0000 + u64::from(index));
    }
    // **And it leaves `SP` 64 bytes lower than it found it**, which is what makes restoring `SP`
    // testable at all. A callback that balanced its own frame would leave `SP` correct however the
    // boundary behaved, and the assertion below would pass against a boundary that never restored it —
    // which is exactly what the mutation harness found.
    callback.push(sub_imm(31, 31, 64));
    callback.mov(0, 0x600D);
    callback.push(ret(30));
    guest.load(callback.words());

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(19, 0x1111_1111);
    asm.mov(20, 0x2222_2222);
    asm.mov(23, 0x3333_3333);
    asm.mov(0, callback_at as u64);
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0)); // what the handler returned
    asm.push(str_imm(19, 22, 8));
    asm.push(str_imm(20, 22, 16));
    asm.push(str_imm(23, 22, 24));
    asm.push(add_imm(9, 31, 0)); // MOV X9, SP — `ADD Xd, SP, #0` is how AArch64 spells it
    asm.push(str_imm(9, 22, 32));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    assert_eq!(guest.read_u64(guest.data), 0x600D, "the callback's return value came back");
    assert_eq!(guest.read_u64(guest.data + 8), 0x1111_1111, "X19 survived the callback");
    assert_eq!(guest.read_u64(guest.data + 16), 0x2222_2222, "X20 survived");
    assert_eq!(guest.read_u64(guest.data + 24), 0x3333_3333, "X23 survived");
    assert_eq!(
        guest.read_u64(guest.data + 32) as usize,
        guest.stack_top,
        "and so did SP, which the callback's own frame moved"
    );
}

/// The two paths are told apart by the counters, not inferred from the answer.
///
/// Global Constraint 13's distinction between exercising code and detecting a difference: both paths
/// produce the same value, so a test asserting only on the value could not tell which ran.
#[test]
fn the_inline_and_exit_paths_are_distinguishable_by_their_counters() {
    let _guard = serialized();
    reset();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let inline = builder.bind_inline("eight_ints", eight_ints).expect("bind");
    let exiting = builder.bind_reentrant("atexit_like", atexit_like).expect("bind");
    let boundary = builder.finish();

    let nothing_at = guest.next_entry();
    guest.load(&[ret(30)]);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for index in 0..8u32 {
        asm.mov(index, 1);
    }
    asm.bl(inline);
    asm.mov(0, nothing_at as u64);
    asm.bl(exiting);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    boundary.run(&mut cpu, entry, BUDGET).expect("the run must complete");

    assert_eq!(cpu.inline_thunk_calls().serviced, 1, "exactly one call stayed in the run loop");
    assert_eq!(cpu.inline_thunk_calls().deferred, 0, "and none of them escalated");
    assert_eq!(boundary.crossings().exits, 1, "exactly one left it");
    assert_eq!(boundary.crossings().guest_calls, 1);
}
