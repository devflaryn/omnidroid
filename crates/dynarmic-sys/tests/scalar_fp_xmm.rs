//! **Patch 0039: element 0 of a vector stays in its register.** Every scalar floating-point
//! operand of the A64 frontend is `VectorGetElement(GetQ(vec), 0)`, which upstream copied to a
//! general register (and the SSE consumer copied straight back). With the switch on, the value is
//! the vector's own register, whose bits above the element are the rest of the vector -- so the
//! property to hold is that nothing ever reads them as part of the element. These tests run random
//! scalar floating-point, conversion and vector code with the switch off and on and compare every
//! register, the flags and `FPSR`, under several `FPCR` settings, with the upper lanes of every
//! vector register full of other values.
//!
//! The switch is process-wide and read when a block is emitted; every test here takes [`SERIAL`]
//! and builds a fresh `Vm` (its own code cache) per run.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn set_switch(on: bool) {
    // SAFETY: stores one process-wide atomic; the tests here are serialized.
    let kept = unsafe { dynarmic_sys::od_set_scalar_fp_in_xmm(u32::from(on)) };
    assert_eq!(kept != 0, on && cfg!(target_arch = "x86_64"));
}

const S: u32 = 0;
const D: u32 = 1;

/// Floating-point data processing, two sources: `FMUL FDIV FADD FSUB FMAX FMIN FMAXNM FMINNM FNMUL`.
const fn fp2(ftype: u32, opc: u32, rd: u32, rn: u32, rm: u32) -> u32 {
    0x1E20_0800 | (ftype << 22) | (rm << 16) | (opc << 12) | (rn << 5) | rd
}
/// One source: `FMOV FABS FNEG FSQRT FCVT(S) FCVT(D) FRINTN FRINTP FRINTM FRINTZ FRINTA FRINTX FRINTI`.
const fn fp1(ftype: u32, opc: u32, rd: u32, rn: u32) -> u32 {
    0x1E20_4000 | (ftype << 22) | (opc << 15) | (rn << 5) | rd
}
const FP1_OPS: [u32; 12] = [0b000000, 0b000001, 0b000010, 0b000011, 0b000100, 0b000101, 0b001000, 0b001001, 0b001010, 0b001011, 0b001100, 0b001111];
/// `FMADD/FMSUB/FNMADD/FNMSUB`.
const fn fp3(ftype: u32, o1: u32, o0: u32, rd: u32, rn: u32, rm: u32, ra: u32) -> u32 {
    0x1F00_0000 | (ftype << 22) | (o1 << 21) | (rm << 16) | (o0 << 15) | (ra << 10) | (rn << 5) | rd
}
/// `FCMP`/`FCMPE Vn, Vm`.
const fn fcmp(ftype: u32, e: u32, rn: u32, rm: u32) -> u32 {
    0x1E20_2000 | (ftype << 22) | (rm << 16) | (rn << 5) | (e << 4)
}
/// `FCSEL`.
const fn fcsel(ftype: u32, rd: u32, rn: u32, rm: u32, cond: u32) -> u32 {
    0x1E20_0C00 | (ftype << 22) | (rm << 16) | (cond << 12) | (rn << 5) | rd
}
/// Conversions between floating point and integer: `SCVTF UCVTF FCVTZS FCVTZU FCVTNS FCVTAS FMOV`.
const fn cvt(sf: u32, ftype: u32, rmode: u32, opc: u32, rd: u32, rn: u32) -> u32 {
    0x1E20_0000 | (sf << 31) | (ftype << 22) | (rmode << 19) | (opc << 16) | (rn << 5) | rd
}
/// `FMOV Vd, #imm8`.
const fn fmov_imm(ftype: u32, rd: u32, imm8: u32) -> u32 {
    0x1E20_1000 | (ftype << 22) | (imm8 << 13) | rd
}
/// `EOR Vd.16B, Vn.16B, Vm.16B`.
const fn eor16b(rd: u32, rn: u32, rm: u32) -> u32 {
    0x6E20_1C00 | (rm << 16) | (rn << 5) | rd
}
/// `FADD Vd.4S/2D`.
const fn fadd_vec(sz: u32, rd: u32, rn: u32, rm: u32) -> u32 {
    0x4E20_D400 | (sz << 22) | (rm << 16) | (rn << 5) | rd
}
/// `INS Vd.S[1], Vn.S[0]`.
const fn ins_s1(rd: u32, rn: u32) -> u32 {
    0x6E0C_0400 | (rn << 5) | rd
}
/// `DUP Vd.4S, Vn.S[0]`.
const fn dup_4s(rd: u32, rn: u32) -> u32 {
    0x4E04_0400 | (rn << 5) | rd
}
/// `UMOV Wd, Vn.S[0]` / `MOV Xd, Vn.D[0]`.
const fn umov_s(rd: u32, rn: u32) -> u32 {
    0x0E04_3C00 | (rn << 5) | rd
}
const fn umov_d(rd: u32, rn: u32) -> u32 {
    0x4E08_3C00 | (rn << 5) | rd
}
/// `ADD Dd, Dn, Dm` (scalar integer).
const fn add_d(rd: u32, rn: u32, rm: u32) -> u32 {
    0x5EE0_8400 | (rm << 16) | (rn << 5) | rd
}
/// `LDR/STR St|Dt, [Xn, #off]`.
const fn ldr_s(rt: u32, rn: u32, off: u32) -> u32 {
    0xBD40_0000 | ((off / 4) << 10) | (rn << 5) | rt
}
const fn str_s(rt: u32, rn: u32, off: u32) -> u32 {
    0xBD00_0000 | ((off / 4) << 10) | (rn << 5) | rt
}
const fn ldr_d(rt: u32, rn: u32, off: u32) -> u32 {
    0xFD40_0000 | ((off / 8) << 10) | (rn << 5) | rt
}
const fn str_d(rt: u32, rn: u32, off: u32) -> u32 {
    0xFD00_0000 | ((off / 8) << 10) | (rn << 5) | rt
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u32 {
        (self.next() % n) as u32
    }
}

/// A random 64-bit floating-point bit pattern: ordinary values most of the time, then zeros,
/// infinities, NaNs (quiet and signalling) and denormals.
fn f64_bits(rng: &mut Rng) -> u64 {
    match rng.below(10) {
        0 => [0, 1 << 63, 0x7FF0 << 48, 0xFFF0 << 48, 0x7FF8 << 48, 0x7FF4 << 48 | 1, 1, 0x000F_FFFF_FFFF_FFFF][rng.below(8) as usize],
        1 => rng.next(),
        _ => ((rng.below(2000) as f64 - 1000.0) / (1.0 + rng.below(97) as f64)).to_bits(),
    }
}
fn f32_bits(rng: &mut Rng) -> u64 {
    let v: u32 = match rng.below(10) {
        0 => [0, 1 << 31, 0x7F80_0000, 0xFF80_0000, 0x7FC0_0000, 0x7FA0_0001, 1, 0x007F_FFFF][rng.below(8) as usize],
        1 => rng.next() as u32,
        _ => ((rng.below(2000) as f32 - 1000.0) / (1.0 + rng.below(97) as f32)).to_bits(),
    };
    u64::from(v)
}

/// `X1` is the data base; `X2..X9` take integers; `V0..V7` are worked; `X10` loads `FPCR`.
fn random_program(rng: &mut Rng, fpcr: u64) -> Vec<u32> {
    let mut code = a64::mov64(10, fpcr);
    code.push(a64::msr_fpcr(10));
    code.extend(a64::mov64(1, 0x2000));
    let v = |rng: &mut Rng| rng.below(8);
    let x = |rng: &mut Rng| 2 + rng.below(8);
    for _ in 0..(8 + rng.below(40)) {
        let t = rng.below(2);
        let w = match rng.below(24) {
            0..=4 => fp2(t, rng.below(9), v(rng), v(rng), v(rng)),
            5 | 6 => {
                // `FCVT` to its own size is unallocated: convert to the other one.
                let op = match FP1_OPS[rng.below(12) as usize] {
                    0b000100 | 0b000101 => 0b000100 | (1 - t),
                    op => op,
                };
                fp1(t, op, v(rng), v(rng))
            }
            7 => fp3(t, rng.below(2), rng.below(2), v(rng), v(rng), v(rng), v(rng)),
            8 => fcmp(t, rng.below(2), v(rng), v(rng)),
            9 => fcsel(t, v(rng), v(rng), v(rng), rng.below(14)),
            10 => cvt(rng.below(2), t, 0, 2 + rng.below(2), v(rng), x(rng)),          // SCVTF/UCVTF
            11 => cvt(rng.below(2), t, 3, rng.below(2), x(rng), v(rng)),              // FCVTZS/FCVTZU
            12 => cvt(rng.below(2), t, 0, [0, 4][rng.below(2) as usize], x(rng), v(rng)), // FCVTNS/FCVTAS
            13 => cvt(t, t, 0, 7, v(rng), x(rng)),                                     // FMOV S<-W, D<-X
            14 => cvt(t, t, 0, 6, x(rng), v(rng)),                                     // FMOV W<-S, X<-D
            15 => fmov_imm(t, v(rng), rng.below(256)),
            16 => eor16b(v(rng), v(rng), v(rng)),
            17 => fadd_vec(rng.below(2), v(rng), v(rng), v(rng)),
            18 => [ins_s1(v(rng), v(rng)), dup_4s(v(rng), v(rng))][rng.below(2) as usize],
            19 => [umov_s(x(rng), v(rng)), umov_d(x(rng), v(rng))][rng.below(2) as usize],
            20 => add_d(v(rng), v(rng), v(rng)),
            21 => [ldr_s(v(rng), 1, 4 * rng.below(16)), ldr_d(v(rng), 1, 8 * rng.below(16))][rng.below(2) as usize],
            22 => [str_s(v(rng), 1, 4 * rng.below(16)), str_d(v(rng), 1, 8 * rng.below(16))][rng.below(2) as usize],
            _ => a64::add_imm(x(rng), x(rng), rng.below(4096)),
        };
        code.push(w);
    }
    code.push(a64::mrs_fpsr(11));
    code.push(a64::svc(0));
    code
}

#[derive(Debug, PartialEq)]
struct State {
    x: Vec<u64>,
    v: Vec<[u64; 2]>,
    nzcv: u32,
    memory: Vec<u64>,
}

fn run(code: &[u32], seed: u64) -> State {
    let vm = Vm::new(code.to_vec(), VmOptions { check_halt_on_memory_access: true, ..VmOptions::default() });
    let mut rng = Rng(seed | 1);
    for i in 0..8 {
        // Both lanes hold something: the upper lane is what the switch must never let leak.
        let pick = |rng: &mut Rng| if rng.below(2) == 0 { f64_bits(rng) } else { f32_bits(rng) | (f32_bits(rng) << 32) };
        let lo = pick(&mut rng);
        vm.set_vec(i, [lo, pick(&mut rng)]);
    }
    for i in 2..10 {
        vm.set_reg(i, if rng.below(3) == 0 { rng.below(1000) as u64 } else { rng.next() });
    }
    vm.with_ctx(|c| {
        for i in 0..16 {
            c.write_u64(0x2000 + 8 * i, f64_bits(&mut rng));
        }
    });
    vm.start(u64::MAX);
    let hr = vm.run_to_completion(16);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "halt {hr:#x}");
    State {
        x: (0..31).map(|i| vm.reg(i)).collect(),
        v: (0..32).map(|i| vm.vec(i)).collect(),
        nzcv: vm.pstate() & 0xF000_0000,
        memory: vm.with_ctx(|c| (0..16).map(|i| c.read_u64(0x2000 + 8 * i)).collect()),
    }
}

#[test]
fn random_scalar_floating_point_is_the_same_with_element_zero_in_its_register() {
    let _serial = serialized();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    // Default, flush-to-zero, default-NaN, both, and each rounding mode.
    let fpcrs = [0u64, 1 << 24, 1 << 25, 3 << 24, 1 << 22, 2 << 22, 3 << 22];
    const TRIALS: usize = 3000;
    for trial in 0..TRIALS {
        let seed = rng.next();
        let fpcr = fpcrs[trial % fpcrs.len()];
        let code = random_program(&mut Rng(seed), fpcr);
        set_switch(false);
        let off = run(&code, seed);
        set_switch(true);
        let on = run(&code, seed);
        assert_eq!(on, off, "trial {trial} (seed {seed:#x}, FPCR {fpcr:#x}) diverged: {code:08x?}");
    }
    set_switch(false);
    let _ = CODE_BASE;
}

/// The case the switch exists for, checked by value: a dependent chain of scalar adds, in both
/// widths, whose registers' upper lanes are non-zero and must come out zeroed (a scalar write
/// clears the rest of the register).
#[test]
fn a_scalar_chain_computes_the_same_and_clears_the_upper_lanes() {
    let _serial = serialized();
    for on in [false, true] {
        set_switch(on);
        let mut code = vec![];
        for _ in 0..4 {
            code.push(fp2(D, 0b0010, 1, 1, 2)); // FADD D1, D1, D2
            code.push(fp2(S, 0b0010, 3, 3, 4)); // FADD S3, S3, S4
        }
        code.push(fp3(D, 0, 0, 5, 1, 2, 1)); // FMADD D5, D1, D2, D1
        code.push(a64::svc(0));
        let vm = Vm::new(code, VmOptions::default());
        vm.set_vec(1, [1.5f64.to_bits(), 0xDEAD_BEEF]);
        vm.set_vec(2, [0.25f64.to_bits(), 0xFFFF_FFFF_FFFF_FFFF]);
        vm.set_vec(3, [u64::from(2.0f32.to_bits()) | (0x1234_5678 << 32), 7]);
        vm.set_vec(4, [u64::from(0.5f32.to_bits()) | (0x9ABC_DEF0 << 32), 9]);
        vm.set_vec(5, [3, 3]);
        vm.start(u64::MAX);
        assert_eq!(vm.run_to_completion(4) & HALT_DONE, HALT_DONE);
        assert_eq!(vm.vec(1), [2.5f64.to_bits(), 0], "switch {on}");
        assert_eq!(vm.vec(3), [u64::from(4.0f32.to_bits()), 0], "switch {on}");
        assert_eq!(vm.vec(5), [(2.5f64 * 0.25 + 2.5).to_bits(), 0], "switch {on}");
    }
    set_switch(false);
}
