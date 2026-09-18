//! Hand-encoded A64 executed through the shim, with the register results
//! checked.
//!
//! Every program is a list of instruction words. Each one is given as a raw
//! `0x…` literal *and* built by the committed encoder in `harness::a64`, and
//! the two are asserted equal, so a transcription error in either is caught by
//! the other. No assembler is involved.
//!
//! `X31` in these listings is the zero register in the data-processing forms.
//! Programs end with `SVC #0`, which the harness turns into a halt.

mod harness;

use dynarmic_sys::optimization;
use harness::a64::{self, cond};
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE};

/// Assemble a listing of `(word, expected_hex)` pairs, checking each.
fn assemble(listing: &[(u32, u32)]) -> Vec<u32> {
    let mut out = Vec::with_capacity(listing.len());
    for (i, (built, stated)) in listing.iter().enumerate() {
        assert_eq!(
            built, stated,
            "instruction {i}: encoder produced {built:#010X}, listing says {stated:#010X}"
        );
        out.push(*built);
    }
    out
}

fn run(code: Vec<u32>, opts: VmOptions) -> Vm {
    let vm = Vm::new(code, opts);
    vm.start(1_000_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(
        hr & HALT_DONE,
        HALT_DONE,
        "guest did not reach SVC #0; halt reason {hr:#010X}, exceptions {:?}",
        vm.with_ctx(|c| c.exceptions.clone())
    );
    vm
}

#[test]
fn integer_alu_with_shifted_operands() {
    // MOVZ  X0, #0x1234                 D2824680
    // MOVZ  X1, #0x0040                 D2800801
    // ADD   X2, X0, X1, LSL #4          8B011002   ; 0x1234 + 0x400  = 0x1634
    // SUB   X3, X0, X1, LSR #2          CB410803   ; 0x1234 - 0x10   = 0x1224
    // MOVN  X4, #7                      928000E4   ; X4 = ~7 = -8
    // ADD   X5, X0, X4, ASR #1          8B840405   ; 0x1234 + (-4)   = 0x1230
    // EOR   X6, X0, X1, LSL #8          CA012006   ; 0x1234 ^ 0x4000 = 0x5234
    // ORR   X7, XZR, X2                 AA0203E7   ; MOV X7, X2
    // SUBS  XZR, X0, X0                 EB00001F   ; CMP X0, X0 -> Z=1, C=1
    // SVC   #0                          D4000001
    let code = assemble(&[
        (a64::movz(0, 0x1234, 0), 0xD282_4680),
        (a64::movz(1, 0x0040, 0), 0xD280_0801),
        (a64::add_shifted(2, 0, 1, 0, 4), 0x8B01_1002),
        (a64::sub_shifted(3, 0, 1, 1, 2), 0xCB41_0803),
        (a64::movn(4, 7, 0), 0x9280_00E4),
        (a64::add_shifted(5, 0, 4, 2, 1), 0x8B84_0405),
        (a64::eor_shifted(6, 0, 1, 8), 0xCA01_2006),
        (a64::mov_reg(7, 2), 0xAA02_03E7),
        (a64::subs_shifted(31, 0, 0), 0xEB00_001F),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = run(code, VmOptions::default());
    assert_eq!(vm.reg(0), 0x1234);
    assert_eq!(vm.reg(1), 0x40);
    assert_eq!(vm.reg(2), 0x1634, "ADD with LSL #4");
    assert_eq!(vm.reg(3), 0x1224, "SUB with LSR #2");
    assert_eq!(vm.reg(4), (-8i64) as u64, "MOVN");
    assert_eq!(vm.reg(5), 0x1230, "ADD with ASR #1 of a negative value");
    assert_eq!(vm.reg(6), 0x5234, "EOR with LSL #8");
    assert_eq!(vm.reg(7), 0x1634, "ORR Xd, XZR, Xm is MOV");
    // NZCV sits in PSTATE bits 31:28. `x - x` is zero with no borrow: Z and C.
    assert_eq!(vm.pstate() & 0xF000_0000, 0x6000_0000, "NZCV after CMP X0, X0");
}

#[test]
fn load_store_pair_and_register_offset() {
    // MOVZ  X4, #0x2000                 D2840004   ; data base
    // MOVZ  X0, #0x1111                 D2822220
    // MOVZ  X1, #0x2222                 D2844441
    // STP   X0, X1, [X4, #16]           A9010480   ; [0x2010]=0x1111 [0x2018]=0x2222
    // LDP   X2, X3, [X4, #16]           A9410C82
    // MOVZ  X5, #3                      D2800065
    // LDR   X6, [X4, X5, LSL #3]        F8657886   ; [0x2018] -> 0x2222
    // MOVZ  X7, #0xABCD                 D29579A7
    // STR   X7, [X4, X5, LSL #3]        F8257887   ; [0x2018] = 0xABCD
    // LDR   X8, [X4, #24]               F9400C88
    // STRB  W0, [X4, #40]               3900A080   ; [0x2028] = 0x11
    // LDRB  W9, [X4, #40]               3940A089
    // SVC   #0                          D4000001
    let code = assemble(&[
        (a64::movz(4, 0x2000, 0), 0xD284_0004),
        (a64::movz(0, 0x1111, 0), 0xD282_2220),
        (a64::movz(1, 0x2222, 0), 0xD284_4441),
        (a64::stp_imm(0, 1, 4, 16), 0xA901_0480),
        (a64::ldp_imm(2, 3, 4, 16), 0xA941_0C82),
        (a64::movz(5, 3, 0), 0xD280_0065),
        (a64::ldr_reg(6, 4, 5), 0xF865_7886),
        (a64::movz(7, 0xABCD, 0), 0xD295_79A7),
        (a64::str_reg(7, 4, 5), 0xF825_7887),
        (a64::ldr_imm(8, 4, 24), 0xF940_0C88),
        (a64::strb_imm(0, 4, 40), 0x3900_A080),
        (a64::ldrb_imm(9, 4, 40), 0x3940_A089),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = run(code, VmOptions::default());
    assert_eq!(vm.reg(2), 0x1111, "LDP low half");
    assert_eq!(vm.reg(3), 0x2222, "LDP high half");
    assert_eq!(vm.reg(6), 0x2222, "LDR with register offset, LSL #3");
    assert_eq!(vm.reg(8), 0xABCD, "STR with register offset, read back");
    assert_eq!(vm.reg(9), 0x11, "LDRB of the low byte of 0x1111");

    vm.with_ctx(|c| {
        assert_eq!(c.read_u64(0x2010), 0x1111, "STP wrote guest memory");
        assert_eq!(c.read_u64(0x2018), 0xABCD, "STR (register offset) wrote it");
    });

    // D4: with identity-style fastmem, not one of those accesses may reach a
    // memory callback. A run that is merely *correct* can still be 30-49x slow,
    // and this counter is the only thing that can tell.
    let stats = vm.stats();
    assert_eq!(stats.slow_path_total, 0, "fastmem was bypassed: {stats:?}");
}

#[test]
fn memory_through_callbacks_when_fastmem_is_off() {
    // The same program with fastmem disabled. Identical results, and the
    // counter proves the other path actually ran -- which is what makes the
    // zero in `load_store_pair_and_register_offset` mean something.
    let code = assemble(&[
        (a64::movz(4, 0x2000, 0), 0xD284_0004),
        (a64::movz(0, 0x1111, 0), 0xD282_2220),
        (a64::movz(1, 0x2222, 0), 0xD284_4441),
        (a64::stp_imm(0, 1, 4, 16), 0xA901_0480),
        (a64::ldp_imm(2, 3, 4, 16), 0xA941_0C82),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = run(
        code,
        VmOptions {
            fastmem: false,
            ..VmOptions::default()
        },
    );
    assert_eq!(vm.reg(2), 0x1111);
    assert_eq!(vm.reg(3), 0x2222);
    let stats = vm.stats();
    assert!(
        stats.slow_path_total >= 2,
        "callback path should have been used: {stats:?}"
    );
}

#[test]
fn a_loop() {
    // MOVZ  X0, #0                      D2800000   ; accumulator
    // MOVZ  X1, #10                     D2800141   ; counter
    // loop:
    // ADD   X0, X0, #3                  91000C00
    // SUBS  X1, X1, #1                  F1000421
    // B.NE  loop  (-2 instructions)      54FFFFC1
    // SVC   #0                          D4000001
    let code = assemble(&[
        (a64::movz(0, 0, 0), 0xD280_0000),
        (a64::movz(1, 10, 0), 0xD280_0141),
        (a64::add_imm(0, 0, 3), 0x9100_0C00),
        (a64::subs_imm(1, 1, 1), 0xF100_0421),
        (a64::b_cond(cond::NE, -2), 0x54FF_FFC1),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = run(code, VmOptions::default());
    assert_eq!(vm.reg(0), 30, "ten iterations of +3");
    assert_eq!(vm.reg(1), 0, "counter ran out");
}

#[test]
fn bl_and_ret() {
    // 0: MOVZ X0, #5                    D28000A0
    // 1: BL   +3 (to index 4)           94000003   ; X30 = CODE_BASE + 8
    // 2: ADD  X0, X0, #100              91019000
    // 3: SVC  #0                        D4000001
    // 4: ADD  X0, X0, #1                91000400   ; the callee
    // 5: RET  X30                       D65F03C0
    let code = assemble(&[
        (a64::movz(0, 5, 0), 0xD280_00A0),
        (a64::bl(3), 0x9400_0003),
        (a64::add_imm(0, 0, 100), 0x9101_9000),
        (a64::svc(0), 0xD400_0001),
        (a64::add_imm(0, 0, 1), 0x9100_0400),
        (a64::ret(30), 0xD65F_03C0),
    ]);

    let vm = run(code, VmOptions::default());
    assert_eq!(vm.reg(30), CODE_BASE + 8, "BL wrote the return address to X30");
    assert_eq!(vm.reg(0), 106, "5, +1 in the callee, +100 after RET");
}

#[test]
fn neon() {
    // ADD   V2.4S, V0.4S, V1.4S         4EA18402
    // ADD   V3.2D, V0.2D, V1.2D         4EE18403
    // MOVZ  X4, #0x3000                 D2860004
    // STR   Q2, [X4]                    3D800082
    // LDR   Q5, [X4]                    3DC00085
    // SVC   #0                          D4000001
    let code = assemble(&[
        (a64::add_vec_4s(2, 0, 1), 0x4EA1_8402),
        (a64::add_vec_2d(3, 0, 1), 0x4EE1_8403),
        (a64::movz(4, 0x3000, 0), 0xD286_0004),
        (a64::str_q_imm(2, 4, 0), 0x3D80_0082),
        (a64::ldr_q_imm(5, 4, 0), 0x3DC0_0085),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = Vm::new(code, VmOptions::default());
    // Lanes, low to high: S0 = 0xFFFFFFFF, S1 = 1, S2 = 4, S3 = 3.
    vm.set_vec(0, [0x0000_0001_FFFF_FFFF, 0x0000_0003_0000_0004]);
    // S0 = 1, S1 = 0, S2 = 0x40, S3 = 0x30.
    vm.set_vec(1, [0x0000_0000_0000_0001, 0x0000_0030_0000_0040]);
    vm.start(1_000_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(hr & HALT_DONE, HALT_DONE);

    // 4S: lane 0 wraps to 0 and carries nothing into lane 1, so D0 is 1<<32.
    assert_eq!(
        vm.vec(2),
        [0x0000_0001_0000_0000, 0x0000_0033_0000_0044],
        "ADD .4S is four independent 32-bit adds"
    );
    // 2D: the same bits added as two 64-bit lanes, so the carry propagates.
    assert_eq!(
        vm.vec(3),
        [0x0000_0002_0000_0000, 0x0000_0033_0000_0044],
        "ADD .2D carries across the 32-bit boundary"
    );
    assert_eq!(vm.vec(5), vm.vec(2), "STR Q / LDR Q round-trip");
    vm.with_ctx(|c| {
        assert_eq!(c.read_u64(0x3000), 0x0000_0001_0000_0000);
        assert_eq!(c.read_u64(0x3008), 0x0000_0033_0000_0044);
    });
    assert_eq!(vm.stats().slow_path_total, 0, "128-bit accesses used fastmem");
}

#[test]
fn floating_point() {
    // MOVZ  X0, #0x3FF8, LSL #48        D2E7FF00   ; bits of 1.5
    // FMOV  D0, X0                      9E670000
    // MOVZ  X1, #0x4002, LSL #48        D2E80041   ; bits of 2.25
    // FMOV  D1, X1                      9E670021
    // FADD  D2, D0, D1                  1E612802   ; 3.75
    // FMUL  D3, D0, D1                  1E610803   ; 3.375
    // FMOV  X2, D2                      9E660042
    // FMOV  X3, D3                      9E660063
    // MOVZ  X4, #7                      D28000E4
    // SCVTF D4, X4                      9E620084   ; 7.0
    // FMUL  D5, D4, D0                  1E600885   ; 10.5
    // FCVTZS X5, D5                     9E7800A5   ; truncates to 10
    // SVC   #0                          D4000001
    let one_five = a64::mov64(0, 1.5f64.to_bits());
    let two_two_five = a64::mov64(1, 2.25f64.to_bits());
    // 1.5 is 0x3FF8_0000_0000_0000 and 2.25 is 0x4002_0000_0000_0000: one
    // non-zero 16-bit field each, so `mov64` emits exactly one MOVZ for each.
    assert_eq!(one_five, vec![0xD2E7_FF00], "MOVZ X0, #0x3FF8, LSL #48");
    assert_eq!(two_two_five, vec![0xD2E8_0041], "MOVZ X1, #0x4002, LSL #48");

    let mut code = one_five;
    code.extend(assemble(&[(a64::fmov_d_from_x(0, 0), 0x9E67_0000)]));
    code.extend(two_two_five);
    code.extend(assemble(&[
        (a64::fmov_d_from_x(1, 1), 0x9E67_0021),
        (a64::fadd_d(2, 0, 1), 0x1E61_2802),
        (a64::fmul_d(3, 0, 1), 0x1E61_0803),
        (a64::fmov_x_from_d(2, 2), 0x9E66_0042),
        (a64::fmov_x_from_d(3, 3), 0x9E66_0063),
        (a64::movz(4, 7, 0), 0xD280_00E4),
        (a64::scvtf_d_from_x(4, 4), 0x9E62_0084),
        (a64::fmul_d(5, 4, 0), 0x1E60_0885),
        (a64::fcvtzs_x_from_d(5, 5), 0x9E78_00A5),
        (a64::svc(0), 0xD400_0001),
    ]));

    let vm = run(code, VmOptions::default());
    assert_eq!(f64::from_bits(vm.reg(2)), 3.75, "FADD 1.5 + 2.25");
    assert_eq!(f64::from_bits(vm.reg(3)), 3.375, "FMUL 1.5 * 2.25");
    assert_eq!(vm.reg(5), 10, "FCVTZS truncates 7.0 * 1.5 toward zero");
}

#[test]
fn atomic_load_exclusive_store_exclusive() {
    // MOVZ  X4, #0x4000                 D2880004
    // MOVZ  X0, #0x55                   D2800AA0
    // STR   X0, [X4]                    F9000080
    // retry:
    // LDXR  X1, [X4]                    C85F7C81
    // ADD   X1, X1, #1                  91000421
    // STXR  W2, X1, [X4]                C8027C81
    // SUBS  XZR, X2, #0                 F100005F   ; CMP X2, #0
    // B.NE  retry  (-4)                  54FFFF81
    // SVC   #0                          D4000001
    let code = assemble(&[
        (a64::movz(4, 0x4000, 0), 0xD288_0004),
        (a64::movz(0, 0x55, 0), 0xD280_0AA0),
        (a64::str_imm(0, 4, 0), 0xF900_0080),
        (a64::ldxr(1, 4), 0xC85F_7C81),
        (a64::add_imm(1, 1, 1), 0x9100_0421),
        (a64::stxr(2, 1, 4), 0xC802_7C81),
        (a64::subs_imm(31, 2, 0), 0xF100_005F),
        (a64::b_cond(cond::NE, -4), 0x54FF_FF81),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = run(code.clone(), VmOptions::default());
    assert_eq!(vm.reg(1), 0x56, "LDXR read 0x55, incremented to 0x56");
    assert_eq!(vm.reg(2), 0, "STXR reported success");
    vm.with_ctx(|c| assert_eq!(c.read_u64(0x4000), 0x56, "STXR wrote guest memory"));
    // Without `fastmem_exclusive_access` the exclusive store goes through the
    // monitor and therefore through a callback. Worth knowing, and worth
    // separating from the ordinary data path.
    let stats = vm.stats();
    assert!(
        stats.slow_path_exclusive >= 1,
        "exclusive store should have used the monitor: {stats:?}"
    );
    assert_eq!(
        stats.slow_path_writes, 0,
        "the ordinary STR still used fastmem: {stats:?}"
    );
    // Note for D5 risk 3: `LDXR` also leaves the fast path, because the global
    // monitor reads through `MemoryRead64` while holding its spin lock. So the
    // exclusive pair costs two callbacks and a lock, not one.
    assert!(
        stats.slow_path_reads >= 1,
        "LDXR is served by the monitor, not fastmem: {stats:?}"
    );

    // The same program with exclusives served by fastmem: same answer, no
    // callback. This is D5 risk 3's mitigation, and it works on our pin.
    let vm = run(
        code,
        VmOptions {
            fastmem_exclusive: true,
            ..VmOptions::default()
        },
    );
    assert_eq!(vm.reg(1), 0x56);
    assert_eq!(vm.reg(2), 0);
    assert_eq!(
        vm.stats().slow_path_total,
        0,
        "fastmem_exclusive_access should keep LDXR/STXR off the callback path"
    );
}

#[test]
fn tpidr_el0_is_readable_with_a_bionic_stack_guard() {
    // D13: every stack-protected function in libroblox.so does exactly this,
    // and it happens before JNI_OnLoad and before the first static initializer.
    //
    // MRS   X0, TPIDR_EL0               D53BD040
    // LDR   X1, [X0, #0x28]             F9401401   ; TLS_SLOT_STACK_GUARD
    // SVC   #0                          D4000001
    let code = assemble(&[
        (a64::mrs_tpidr_el0(0), 0xD53B_D040),
        (a64::ldr_imm(1, 0, 0x28), 0xF940_1401),
        (a64::svc(0), 0xD400_0001),
    ]);

    const TLS_BLOCK: u64 = 0x8000;
    const GUARD: u64 = 0xDEAD_BEEF_CAFE_F00D;

    let mut vm = Vm::new(code, VmOptions::default());
    vm.set_tpidr_el0(TLS_BLOCK);
    vm.with_ctx(|c| c.write_u64(TLS_BLOCK + 0x28, GUARD));
    vm.start(1_000_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(hr & HALT_DONE, HALT_DONE);

    assert_eq!(vm.reg(0), TLS_BLOCK, "MRS read the thread pointer");
    assert_eq!(vm.reg(1), GUARD, "the stack guard is at TPIDR_EL0 + 0x28");
    // The pointer is inlined into generated code, so no callback is involved.
    assert_eq!(vm.stats().slow_path_total, 0);
    assert_eq!(
        vm.effective_config().tpidr_el0_ptr,
        vm.tpidr_el0_ptr(),
        "dynarmic baked in the slot we gave it"
    );
}

#[test]
fn effective_config_reports_what_was_asked_for() {
    // P2: the default `fastmem_address_space_bits` is 36 and a guest address
    // above it silently degrades to the callback path at 30-49x the cost. An
    // assertion that cannot read the setting is not an assertion, so this is
    // the entry point that makes Task 3's startup check possible.
    let vm = Vm::new(vec![a64::svc(0)], VmOptions::default());
    let cfg = vm.effective_config();
    assert_eq!(cfg.fastmem_enabled, 1);
    assert_eq!(
        cfg.fastmem_address_space_bits,
        u64::from(harness::MEM_BITS),
        "the width we asked for, not dynarmic's default of 36"
    );
    assert_ne!(cfg.fastmem_pointer, 0, "the host base of the guest arena");
    assert_eq!(cfg.code_cache_size, 8 << 20);
    assert_eq!(cfg.page_table_present, 0);
    assert_eq!(cfg.enable_cycle_counting, 0);
    assert_eq!(cfg.optimizations, optimization::ALL_SAFE);
    assert_eq!(cfg.unsafe_optimizations, 0);

    // The two flags whose terminal handlers check neither the cycle counter nor
    // the halt flag must be assertable, because with them on a runaway guest
    // cannot be stopped at all (see `tests/hostile.rs`).
    let vm = Vm::new(
        vec![a64::svc(0)],
        VmOptions {
            optimizations: optimization::INTERRUPTIBLE,
            ..VmOptions::default()
        },
    );
    let cfg = vm.effective_config();
    assert_eq!(cfg.optimizations & optimization::RETURN_STACK_BUFFER, 0);
    assert_eq!(cfg.optimizations & optimization::FAST_DISPATCH, 0);
    assert_ne!(cfg.optimizations & optimization::BLOCK_LINKING, 0);

    let vm = Vm::new(
        vec![a64::svc(0)],
        VmOptions {
            fastmem: false,
            cycle_counting: true,
            ..VmOptions::default()
        },
    );
    let cfg = vm.effective_config();
    assert_eq!(cfg.fastmem_enabled, 0, "fastmem off must be visible");
    assert_eq!(cfg.fastmem_pointer, 0);
    assert_eq!(cfg.enable_cycle_counting, 1);
}

#[test]
fn a_step_budget_ends_an_endless_loop() {
    // Guest code is untrusted, and untrusted code loops forever. With cycle
    // counting on, `get_ticks_remaining` is the budget and the run ends when it
    // is spent -- no host thread is lost.
    //
    // MOVZ X0, #0                       D2800000
    // loop: ADD X0, X0, #1              91000400
    //       B loop (-1)                 17FFFFFF
    let code = assemble(&[
        (a64::movz(0, 0, 0), 0xD280_0000),
        (a64::add_imm(0, 0, 1), 0x9100_0400),
        (a64::b(-1), 0x17FF_FFFF),
    ]);

    let vm = Vm::new(
        code,
        VmOptions {
            cycle_counting: true,
            ..VmOptions::default()
        },
    );
    vm.start(10_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(hr & HALT_DONE, 0, "the loop never reaches an SVC");
    let (remaining, used) = vm.with_ctx(|c| (c.ticks_remaining, c.ticks_used));
    assert_eq!(remaining, 0, "the budget was spent");
    assert!(used >= 10_000, "ticks were charged: {used}");
    assert!(vm.reg(0) > 0, "the loop did run");
}

#[test]
fn callbacks_are_never_nested() {
    // The claim the whole re-entrancy argument rests on, measured rather than
    // asserted: run a program that enters the memory callbacks and the SVC
    // callback, and check the deepest nesting ever observed.
    let code = assemble(&[
        (a64::movz(4, 0x2000, 0), 0xD284_0004),
        (a64::movz(0, 0x99, 0), 0xD280_1320),
        (a64::str_imm(0, 4, 0), 0xF900_0080),
        (a64::ldr_imm(1, 4, 0), 0xF940_0081),
        (a64::svc(0), 0xD400_0001),
    ]);
    let vm = run(
        code,
        VmOptions {
            fastmem: false, // force the memory callbacks to be entered
            ..VmOptions::default()
        },
    );
    assert_eq!(vm.reg(1), 0x99);
    let (max_depth, depth) = vm.with_ctx(|c| (c.max_depth, c.depth));
    assert_eq!(max_depth, 1, "a callback was entered from inside a callback");
    assert_eq!(depth, 0, "every callback returned");
    assert!(vm.stats().slow_path_total >= 2);
}

#[test]
fn invalidating_a_range_spares_the_translations_outside_it() {
    // `GuestCpu::invalidate_code` says "the guest changed *these* bytes". A
    // backend that answered it by throwing away every translation would be
    // correct and would pass every functional test, while turning each of
    // libroblox.so's `IC IVAU`s into a full retranslation of the working set --
    // 7-25 s of cold translation, per D5. `read_code` counts instruction
    // fetches, which only happen during translation, so the difference is
    // measurable.
    //
    // 0: MOVZ X0, #1                    D2800020
    // 1: B    +4  (to index 5)          14000004
    // 2: NOP                            D503201F
    // 3: NOP                            D503201F
    // 4: NOP                            D503201F
    // 5: ADD  X0, X0, #1                91000400
    // 6: SVC  #0                        D4000001
    let code = assemble(&[
        (a64::movz(0, 1, 0), 0xD280_0020),
        (a64::b(4), 0x1400_0004),
        (a64::NOP, 0xD503_201F),
        (a64::NOP, 0xD503_201F),
        (a64::NOP, 0xD503_201F),
        (a64::add_imm(0, 0, 1), 0x9100_0400),
        (a64::svc(0), 0xD400_0001),
    ]);

    let vm = Vm::new(code, VmOptions::default());
    vm.start(100_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 2);
    let first = vm.stats().read_code;
    assert!(first >= 4, "the whole program was translated: {first}");

    // Re-run with everything still cached: no fetches at all.
    vm.reset_stats();
    vm.start(100_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.stats().read_code, 0, "nothing needed retranslating");

    // Now invalidate only the second block, at CODE_BASE + 20.
    vm.reset_stats();
    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_invalidate_range(vm.raw(), CODE_BASE + 20, 8) };
    vm.start(100_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 2);

    let after = vm.stats().read_code;
    assert!(after > 0, "the invalidated block was retranslated");
    assert!(
        after < first,
        "invalidating 8 bytes retranslated as much as a cold start          ({after} fetches against {first}): the range was ignored"
    );
}

#[test]
fn the_code_cache_is_writable_and_executable_at_once() {
    // This asserts something Omnidroid does not want, on purpose.
    //
    // D12 rules that Omnidroid never holds a page that is simultaneously
    // writable and executable, and `omni-mem`'s `CodeArena` is built that way.
    // dynarmic's code cache is not: upstream commits it
    // `PAGE_EXECUTE_READWRITE` (`block_of_code.cpp:280`), so the region holding
    // *all* generated guest code is W+X. The upstream option that would change
    // it segfaults on this pin, including in dynarmic's own test suite, so the
    // exception is real and cannot be closed by configuration.
    //
    // What is asserted is the build flag, not a `VirtualQuery` of the pages --
    // Global Constraint 4 keeps OS calls in `omni-platform` -- so this says
    // "W^X was not asked for", and the pages follow from that.
    //
    // Asserting it keeps the contradiction from going quiet. When a re-pin or a
    // carried patch fixes it, this test fails and says so.
    let vm = Vm::new(vec![a64::svc(0)], VmOptions::default());
    assert_eq!(
        vm.effective_config().code_cache_w_xor_x,
        0,
        "dynarmic's code cache is now W^X -- D12's exception can be withdrawn,          and crates/dynarmic-sys/patches/README.md updated"
    );
}
