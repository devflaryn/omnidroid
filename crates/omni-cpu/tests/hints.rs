//! **The A64 hint instructions execute, and the guest carries on.**
//!
//! `YIELD`, `WFE`, `WFI`, `SEV` and `SEVL` are hints: the architecture permits an implementation
//! to execute every one of them as a `NOP`, and none has an effect a program can observe. A guest
//! is entitled to execute them, so stopping on one is this layer refusing an instruction the
//! architecture defines.
//!
//! # Why they reach a callback at all, which is the whole reason this file exists
//!
//! `DynarmicOptions` asks for `hook_hint_instructions: 0`, and the shim writes it into
//! `A64::UserConfig`. **The x64 A64 backend does not forward it to the translator.**
//! `backend/x64/a64_interface.cpp:273` builds the translator's options from two of its three
//! members, so `TranslationOptions::hook_hint_instructions` keeps its declared default of `true`
//! (`frontend/A64/translate/a64_translate.h:37`); the A32 paths *do* forward it
//! (`backend/x64/a32_interface.cpp:216`). So the configuration this backend asks for is not the
//! configuration it gets, and the hints arrive as raised exceptions.
//!
//! **MEASURED before the handling existed**: `movz x0,#7; yield; ret` exited
//! `UnsupportedInstruction { encoding: 0xd503203f }` with `interpreter_fallbacks: 0` and
//! `exceptions: 1`. It was found by running the real `libroblox.so`: §8 row 21's
//! `nativeInitClientSettings` stopped at guest `0x021eba20`, inside a three-instruction
//! `ldr`/`cbz`/`yield` spin on a guard word another guest thread owns.
//!
//! # What these tests are, and are not
//!
//! They assert the **observable** consequence — the guest reaches the instruction after the hint,
//! with its registers intact — rather than the counter. `OdStats::exceptions` rising is a watch:
//! a pin that started forwarding the flag would take it to zero and the guest would still be
//! correct, so a test asserting on it would fail for the right thing happening.

#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// `NE`, as `B.<cond>` numbers the condition codes.
const COND_NE: u32 = 0b0001;

/// `HINT #imm`, which is how every one of these is encoded: `11010101000000110010 CRm op2 11111`.
fn hint(crm: u32, op2: u32) -> u32 {
    assert!(crm < 16 && op2 < 8);
    0xd503_201f | (crm << 8) | (op2 << 5)
}

/// Every hint this backend continues through, by name and by encoding.
///
/// The encodings are spelled out beside the constructor so that a wrong `CRm`/`op2` is visible
/// rather than being whatever `hint()` happens to produce — the same reason the scheduling band
/// is asserted value by value rather than as a span.
fn hints() -> Vec<(&'static str, u32, u32)> {
    let rows = vec![
        ("YIELD", hint(0, 1), 0xd503_203f),
        ("WFE", hint(0, 2), 0xd503_205f),
        ("WFI", hint(0, 3), 0xd503_207f),
        ("SEV", hint(0, 4), 0xd503_209f),
        ("SEVL", hint(0, 5), 0xd503_20bf),
    ];
    for (name, built, expected) in &rows {
        assert_eq!(built, expected, "{name} is not the encoding this test thinks it is");
    }
    rows
}

/// **Each hint executes and the guest reaches the instruction after it**, with `X0` untouched.
///
/// `X0` is loaded *before* the hint and read after the return, so a handler that resumed at the
/// wrong `PC` — re-executing the `movz`, or skipping past the `ret` — fails on the value or on
/// the exit reason rather than passing quietly.
#[test]
fn every_hint_executes_and_the_guest_continues_past_it() {
    for (name, encoding, _) in hints() {
        let guest = Guest::new();
        // movz x0, #0x1234 ; <hint> ; ret
        let program = vec![movz(0, 0x1234, 0), encoding, ret(30)];
        let entry = guest.load(&program);
        let (mut cpu, _sentinel) = guest.thread();
        let exit = cpu
            .run(entry, RunLimit::Instructions(1_000))
            .unwrap_or_else(|error| panic!("`{name}` ({encoding:#010x}): {error}"));
        assert!(
            matches!(exit, ExitReason::Returned { .. }),
            "`{name}` ({encoding:#010x}) must not stop the guest; it is a hint the architecture \
             lets an implementation execute as a NOP. Got {exit:?}"
        );
        assert_eq!(
            cpu.x(x(0)),
            0x1234,
            "`{name}` resumed somewhere other than the instruction after it"
        );
    }
}

/// **A hint inside a loop does not end the loop**, which is the shape the real guest uses.
///
/// `libroblox.so` at `0x021eba20` spins `ldr`/`cbz`/`yield`/`b` on a guard word. A handler that
/// continued once and stopped the second time would pass the test above and fail here, and so
/// would one that resumed at the top of the block instead of past the hint — that spins for ever
/// and is caught by the budget rather than by an assertion, which is why the budget is small.
#[test]
fn a_hint_inside_a_loop_runs_every_iteration() {
    let guest = Guest::new();
    // movz x0, #8 ; loop: yield ; subs x0, x0, #1 ; b.ne loop ; ret
    let program = vec![
        movz(0, 8, 0),          // movz x0, #8
        hint(0, 1),             // yield  <- the loop body starts here
        subs_imm(0, 0, 1),      // subs x0, x0, #1
        b_cond(COND_NE, -2),    // back to the yield, not to the movz
        ret(30),
    ];
    let entry = guest.load(&program);
    let (mut cpu, _sentinel) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Instructions(1_000)).expect("the loop must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    assert_eq!(cpu.x(x(0)), 0, "the loop ran to completion with a hint in its body");
}

/// **An encoding that really is unallocated still stops the guest**, so the arm above is a
/// decision about hints and not a blanket "continue on any raised exception".
///
/// `0x00000000` is `UDF #0`, permanently undefined. `VERIFICATION.md` entry 12 is the shape this
/// guards against from the other side: a branch that no input can take is not a check, and a
/// check that every input passes is not one either.
#[test]
fn an_unallocated_encoding_is_still_an_unsupported_instruction() {
    let guest = Guest::new();
    let program = vec![movz(0, 8, 0), 0x0000_0000, ret(30)];
    let entry = guest.load(&program);
    let (mut cpu, _sentinel) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Instructions(1_000)).expect("the run itself must complete");
    match exit {
        ExitReason::UnsupportedInstruction { encoding, .. } => {
            assert_eq!(encoding, 0, "the encoding it stopped on is the one it names");
        }
        other => panic!("an unallocated encoding must stop the guest, not continue: {other:?}"),
    }
}
