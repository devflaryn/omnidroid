//! **The `Interpret` terminal**: what a backend does with an instruction the A64 frontend cannot
//! translate.
//!
//! The pin's A64 decoder leaves 231 of its 874 entries commented out (D5), among them every ARMv8.1
//! LSE atomic, and `Translate` turns a word that decodes to nothing -- or that a visitor declines --
//! into `InterpretThisInstruction()`: the block ends *before* that instruction with
//! `IR::Term::Interpret{pc, num_instructions}`. What the backend emits for that terminal decides
//! whether an unknown instruction is a callback or the end of the process:
//!
//! * the x64 backend (`A64EmitX64::EmitTerminalImpl(IR::Term::Interpret)`) switches `MXCSR` to the
//!   host's, stores the PC, calls `UserCallbacks::InterpreterFallback(pc, num_instructions)`, and
//!   returns to the dispatcher loop (`ReturnFromRunCode(true)`, which reloads the guest's `MXCSR`);
//! * the arm64 backend **on the pin** has `ASSERT_FALSE("Interpret should never be emitted.")`
//!   (`emit_arm64_a64.cpp:36`) -- `std::terminate` on the first undecodable word, measured by
//!   `hostile.rs`'s fuzzer dying with exactly that message. Carried patch 0002 gives it the x64
//!   behaviour; see `patches/README.md`.
//!
//! The guest semantics asserted here are the callback contract, which is the same on both
//! backends: the instructions before the unknown one are committed, the PC handed over is the
//! unknown instruction's own, `num_instructions` counts the merged run of them, and when the
//! callback does not halt, execution continues from whatever PC the callback left.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE};

/// `MOVZ X0, #5 ; MOVZ X4, #0x4000 ; LDADD X1, X2, [X4] ; SVC #0`.
fn one_unknown_instruction() -> Vec<u32> {
    let code = vec![a64::movz(0, 5, 0), a64::movz(4, 0x4000, 0), a64::ldadd_x(1, 2, 4), a64::svc(0)];
    assert_eq!(code, [0xD280_00A0, 0xD288_0004, 0xF821_0082, 0xD400_0001]);
    code
}

#[test]
fn an_undecodable_instruction_reaches_the_interpreter_fallback_and_the_process_survives() {
    let vm = Vm::new(one_unknown_instruction(), VmOptions::default());
    vm.with_ctx(|c| c.write_u64(0x4000, 0x1111));
    vm.set_reg(1, 0x22);
    vm.start(1_000_000);
    let hr = vm.run();

    // The harness's fallback refuses (it cannot interpret A64) by halting with USER7.
    assert_eq!(hr & dynarmic_sys::OD_HALT_USER7, dynarmic_sys::OD_HALT_USER7, "halt {hr:#010X}");
    assert_eq!(
        vm.with_ctx(|c| c.fallbacks.clone()),
        [(CODE_BASE + 8, 1)],
        "exactly one fallback, for the LDADD, asking for one instruction"
    );
    assert_eq!(vm.pc(), CODE_BASE + 8, "the guest PC names the instruction to interpret");
    assert_eq!(vm.reg(0), 5, "the instructions before it were committed");
    assert_eq!(vm.reg(4), 0x4000);
    assert_eq!(vm.reg(2), 0, "the LDADD itself was not executed by the translator");
    assert_eq!(vm.with_ctx(|c| c.read_u64(0x4000)), 0x1111, "and memory was not touched");
}

#[test]
fn a_fallback_that_does_not_halt_resumes_at_the_pc_it_left() {
    // Two unknown words back to back, then a marker. With `MiscIROpt` on (it is, in `ALL_SAFE`),
    // `A64MergeInterpretBlocksPass` merges the two into one fallback of two instructions.
    //
    // MOVZ  X0, #5                      D28000A0
    // LDADD X1, X2, [X4]                F8210082
    // LDADD X1, X3, [X4]                F8210083
    // MOVZ  X9, #0x77                   D2800EE9
    // SVC   #0                          D4000001
    let code = vec![
        a64::movz(0, 5, 0),
        a64::ldadd_x(1, 2, 4),
        a64::ldadd_x(1, 3, 4),
        a64::movz(9, 0x77, 0),
        a64::svc(0),
    ];
    assert_eq!(code, [0xD280_00A0, 0xF821_0082, 0xF821_0083, 0xD280_0EE9, 0xD400_0001]);

    let vm = Vm::new(code, VmOptions::default());
    vm.with_ctx(|c| c.fallback_skips = true);
    vm.start(1_000_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "the guest reached its SVC: halt {hr:#010X}");
    assert_eq!(
        vm.with_ctx(|c| c.fallbacks.clone()),
        [(CODE_BASE + 4, 2)],
        "one fallback covering both unknown words"
    );
    assert_eq!(vm.reg(9), 0x77, "execution continued after the interpreted pair");
    assert_eq!(vm.reg(0), 5);
}

#[test]
fn a_fallback_runs_under_the_host_s_fpcr_and_the_guest_s_is_back_afterwards() {
    // The x64 terminal switches `MXCSR` to the host's around the callback and reloads the guest's
    // on the way back into the dispatcher. The arm64 twin is `FPCR`, and both halves are asserted,
    // in two directions, from guest code: a subnormal times 1.0 is flushed to +0 exactly when the
    // guest's `FPCR.FZ` (bit 24) is in force.
    //
    // MOVZ  X0, #0x0100, LSL #16        D2A02000   (FZ) -- or MOVZ X0, #0 (D2800000)
    // MSR   FPCR, X0                    D51B4400
    // MOVZ  X5, #1                      D2800025   (the smallest double subnormal)
    // MOVZ  X6, #0x3FF0, LSL #48        D2E7FE06   (1.0)
    // LDADD X1, X2, [X4]                F8210082   (unknown: the fallback)
    // FMOV  D0, X5                      9E6700A0
    // FMOV  D1, X6                      9E6700C1
    // FMUL  D2, D0, D1                  1E610802
    // FMOV  X7, D2                      9E660047
    // SVC   #0                          D4000001
    for fz in [true, false] {
        let code = vec![
            if fz { a64::movz(0, 0x0100, 1) } else { a64::movz(0, 0, 0) },
            a64::msr_fpcr(0),
            a64::movz(5, 1, 0),
            a64::movz(6, 0x3FF0, 3),
            a64::ldadd_x(1, 2, 4),
            a64::fmov_d_from_x(0, 5),
            a64::fmov_d_from_x(1, 6),
            a64::fmul_d(2, 0, 1),
            a64::fmov_x_from_d(7, 2),
            a64::svc(0),
        ];
        assert_eq!(
            code,
            [
                if fz { 0xD2A0_2000 } else { 0xD280_0000 },
                0xD51B_4400,
                0xD280_0025,
                0xD2E7_FE06,
                0xF821_0082,
                0x9E67_00A0,
                0x9E67_00C1,
                0x1E61_0802,
                0x9E66_0047,
                0xD400_0001,
            ]
        );
        let vm = Vm::new(code, VmOptions::default());
        vm.with_ctx(|c| c.fallback_skips = true);
        vm.start(1_000_000);
        let hr = vm.run_to_completion(64);
        assert_eq!(hr & HALT_DONE, HALT_DONE, "fz={fz}: halt {hr:#010X}");
        assert_eq!(vm.with_ctx(|c| c.fallbacks.len()), 1, "fz={fz}");

        #[cfg(target_arch = "aarch64")]
        {
            let host: u64;
            // SAFETY: `FPCR` is readable at EL0 and `mrs` touches no memory.
            unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) host, options(nomem, nostack)) };
            assert_eq!(host & (1 << 24), 0, "the test thread itself runs with FZ clear");
            let seen = vm.with_ctx(|c| c.fallback_host_fpcr);
            assert_eq!(
                seen,
                host as u32,
                "fz={fz}: the fallback saw FPCR {seen:#x}, the host's is {host:#x} -- the guest's \
                 control word was live in a host callback"
            );
        }

        let product = vm.reg(7);
        let expected = if fz { 0 } else { 1 };
        assert_eq!(
            product, expected,
            "fz={fz}: after the fallback the guest's subnormal * 1.0 was {product:#x}; the guest's \
             FPCR must be back in force"
        );
    }
}
