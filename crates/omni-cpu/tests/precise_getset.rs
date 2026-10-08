//! **Patch 0037: `GetSetElimination` under `check_halt_on_memory_access`, precise at every access.**
//!
//! dynarmic skipped the pass whenever the memory-abort check was on, because the pass erases a
//! guest-register write that a later write to the same register overwrites -- and a guest fault
//! between the two then returns to the dispatcher with the *older* value (or none) in `JitState`.
//! The patched pass keeps every write that comes before an instruction that can stop the block,
//! and still forwards the value to later reads. These tests say that a fault in the middle of a
//! block sees exactly the state a non-eliminating translation sees, with the switch on and off.

#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::set_precise_get_set;
use omni_cpu::{AccessKind, ExitReason, GuestCpu, Nzcv, RunLimit, VReg};

/// The switch is process-wide and read when a block is translated, so the tests that flip it run
/// one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn v(n: u8) -> VReg {
    VReg::new(n).expect("a vector register")
}

/// `ADDS Xd, Xn, Xm`.
const fn adds_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xAB00_0000 | (rm << 16) | (rn << 5) | rd
}
/// `SUB Xd, Xn, Xm`.
const fn sub_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xCB00_0000 | (rm << 16) | (rn << 5) | rd
}
/// `EOR Xd, Xn, Xm`.
const fn eor_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xCA00_0000 | (rm << 16) | (rn << 5) | rd
}
/// `ADD Wd, Wn, Wm`: a 32-bit write, which the pass tracks apart from a 64-bit one.
const fn add_w(rd: u32, rn: u32, rm: u32) -> u32 {
    0x0B00_0000 | (rm << 16) | (rn << 5) | rd
}
/// `ADC Xd, Xn, Xm`: reads the carry flag alone (`GetCFlag`).
const fn adc(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9A00_0000 | (rm << 16) | (rn << 5) | rd
}
/// `CSEL Xd, Xn, Xm, cond`.
const fn csel(rd: u32, rn: u32, rm: u32, cond: u32) -> u32 {
    0x9A80_0000 | (rm << 16) | (cond << 12) | (rn << 5) | rd
}
/// `MRS Xt, NZCV` / `MSR NZCV, Xt`: the raw flag word (`GetNZCVRaw`/`SetNZCVRaw`).
const fn mrs_nzcv(rt: u32) -> u32 {
    mrs(rt, 3, 3, 4, 2, 0)
}
const fn msr_nzcv(rt: u32) -> u32 {
    msr(rt, 3, 3, 4, 2, 0)
}

/// Every kind of guest access the pass must stay precise at, through `X1` (an unmapped address):
/// an ordinary load and store, ordered ones, an exclusive load (also of a pair), and 64- and
/// 128-bit vector ones.
fn faulting_accesses() -> Vec<(&'static str, u32, AccessKind)> {
    vec![
        ("LDR X5", ldr_imm(5, 1, 0), AccessKind::Read),
        ("STR X5", str_imm(5, 1, 0), AccessKind::Write),
        ("LDAR X5", ldar(5, 1), AccessKind::Read),
        ("STLR X5", stlr(5, 1), AccessKind::Write),
        ("LDXR X5", ldxr(5, 1), AccessKind::Read),
        ("LDAXP X5, X6", ldaxp(5, 6, 1), AccessKind::Read),
        ("LDR Q5", ldr_q(5, 1, 0), AccessKind::Read),
        ("STR Q5", str_q(5, 1, 0), AccessKind::Write),
        ("LDR D5", ldr_d(5, 1, 0), AccessKind::Read),
    ]
}

/// **The one the brief asks for**: `MOV X0,#1; MOV X0,#2; <fault>; MOV X0,#3` stops at the fault
/// with `X0 == 2`, and the flags, a vector register and a second general register are what the
/// instructions before the fault left -- none of what follows it.
#[test]
fn a_fault_in_the_middle_of_a_block_sees_every_write_before_it() {
    let _serial = serialized();
    for precise in [true, false] {
        set_precise_get_set(precise);
        let guest = Guest::new();
        let pattern: [u64; 4] = [0x1111_2222_3333_4444, 0x5555_6666_7777_8888, 0x9999, 0xAAAA];
        for (i, word) in pattern.iter().enumerate() {
            guest.write_u64(guest.data + i * 8, *word);
        }
        for (name, access, kind) in faulting_accesses() {
            let mut program = mov64(1, guest.unmapped as u64);
            program.extend(mov64(2, guest.data as u64));
            program.push(movz(3, 0, 0));
            program.push(ldr_q(7, 2, 0)); // Q7 = pattern[0..2]
            program.push(movz(0, 1, 0));
            program.push(subs_imm(4, 3, 1)); // X4 = -1, NZCV = N
            program.push(movz(0, 2, 0));
            let fault_at = program.len();
            program.push(access);
            program.push(movz(0, 3, 0));
            program.push(subs_imm(4, 3, 0)); // X4 = 0, NZCV = ZC
            program.push(ldr_q(7, 2, 16)); // Q7 = pattern[2..4]
            program.push(ret(30));
            let entry = guest.load(&program);

            let (mut cpu, _) = guest.thread();
            cpu.set_x(x(0), 0xDEAD);
            cpu.set_x(x(4), 0xBEEF);
            cpu.set_v(v(7), 0);
            cpu.set_nzcv(Nzcv { n: false, z: false, c: false, v: true });
            let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit");

            let want_pc = entry + fault_at * 4;
            match exit {
                ExitReason::MemoryFault { pc, address, access } => {
                    assert_eq!(pc, want_pc, "{name} (precise {precise}): the faulting instruction");
                    assert_eq!(address, guest.unmapped, "{name} (precise {precise})");
                    assert_eq!(access, kind, "{name} (precise {precise})");
                }
                other => panic!("{name} (precise {precise}): expected a memory fault, got {other}"),
            }
            assert_eq!(cpu.x(x(0)), 2, "{name} (precise {precise}): X0 is the write before the fault");
            assert_eq!(cpu.x(x(4)), u64::MAX, "{name} (precise {precise}): X4");
            assert_eq!(
                cpu.nzcv(),
                Nzcv { n: true, z: false, c: false, v: false },
                "{name} (precise {precise}): the flags of the SUBS before the fault"
            );
            assert_eq!(
                cpu.v(v(7)),
                u128::from(pattern[0]) | (u128::from(pattern[1]) << 64),
                "{name} (precise {precise}): Q7"
            );
        }
    }
    set_precise_get_set(true);
}

/// **A load whose value nobody reads still faults.** Two ways to write one: into the zero register
/// (`LDR WZR, [Xn]` is how ART's implicit stack-overflow check probes below the stack), and into a
/// register the next instruction overwrites -- which only becomes dead once `GetSetElimination` has
/// erased the first write. `DeadCodeElimination` dropped a memory read with no uses (a read is not a
/// side effect to it), so before patch 0037 the first kind did not fault at all whenever
/// `ConstProp` ran, and the second would not have with the pass on.
#[test]
fn a_load_whose_value_is_never_read_still_faults() {
    let _serial = serialized();
    /// `LDR Wt, [Xn]`: 32-bit, unsigned offset 0.
    const fn ldr_w(rt: u32, rn: u32) -> u32 {
        0xB940_0000 | (rn << 5) | rt
    }
    for precise in [false, true] {
        set_precise_get_set(precise);
        let guest = Guest::new();
        for (name, load) in [
            ("LDR XZR", ldr_imm(31, 1, 0)),
            ("LDR WZR", ldr_w(31, 1)),
            ("LDR X5 then overwritten", ldr_imm(5, 1, 0)),
        ] {
            let mut program = mov64(1, guest.unmapped as u64);
            program.push(movz(0, 1, 0));
            let fault_at = program.len();
            program.push(load);
            program.push(movz(5, 7, 0));
            program.push(movz(0, 3, 0));
            program.push(ret(30));
            let entry = guest.load(&program);
            let (mut cpu, _) = guest.thread();
            let exit = cpu.run(entry, RunLimit::Unlimited).expect("an exit");
            assert!(
                matches!(exit, ExitReason::MemoryFault { pc, address, .. }
                    if pc == entry + fault_at * 4 && address == guest.unmapped),
                "{name} (precise {precise}): expected a fault at the load, got {exit}"
            );
            assert_eq!(cpu.x(x(0)), 1, "{name} (precise {precise})");
        }
    }
    set_precise_get_set(true);
}

/// A value forwarded past an access that did *not* fault is still the right value: the pass keeps
/// what it knows across a memory access rather than reading `JitState` again.
#[test]
fn a_value_is_forwarded_past_an_access_that_does_not_fault() {
    let _serial = serialized();
    set_precise_get_set(true);
    let guest = Guest::new();
    guest.write_u64(guest.data, 40);
    let mut program = mov64(2, guest.data as u64);
    program.push(movz(0, 1, 0));
    program.push(ldr_imm(5, 2, 0)); // X5 = 40
    program.push(add_imm(0, 0, 1)); // X0 = 2, its read forwarded past the load
    program.push(str_imm(0, 2, 8));
    program.push(add_reg(6, 0, 5)); // X6 = 42
    program.push(subs_imm(7, 6, 42)); // Z
    program.push(ldr_imm(8, 2, 8)); // X8 = 2
    program.push(csel(9, 6, 8, 0)); // EQ: X9 = 42
    program.push(ret(30));
    let entry = guest.load(&program);
    let (mut cpu, sentinel) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel });
    assert_eq!((cpu.x(x(0)), cpu.x(x(6)), cpu.x(x(8)), cpu.x(x(9))), (2, 42, 2, 42));
    assert_eq!(guest.read_u64(guest.data + 8), 2);
}

/// Deterministic xorshift64.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A straight-line program of register, flag, vector and memory operations, with at most one
/// faulting access somewhere in it. `X1` is the unmapped address, `X2` the data region; neither is
/// ever written. `X30` is the return.
fn random_program(rng: &mut Rng, unmapped: u64, data: u64) -> Vec<u32> {
    const DST: [u32; 12] = [0, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
    let reg = |rng: &mut Rng| DST[rng.below(DST.len() as u64) as usize];
    let src = |rng: &mut Rng| rng.below(16) as u32;
    let mut program = mov64(1, unmapped);
    program.extend(mov64(2, data));
    let len = 4 + rng.below(40) as usize;
    let fault_at = if rng.below(3) == 0 { usize::MAX } else { rng.below(len as u64) as usize };
    for i in 0..len {
        if i == fault_at {
            let accesses = faulting_accesses();
            program.push(accesses[rng.below(accesses.len() as u64) as usize].1);
            continue;
        }
        let off = (rng.below(16) * 16) as u32;
        let word = match rng.below(22) {
            0 => movz(reg(rng), rng.next() as u16, rng.below(4) as u32),
            1 => movk(reg(rng), rng.next() as u16, rng.below(4) as u32),
            2 => add_imm(reg(rng), src(rng), rng.below(4096) as u32),
            3 => subs_imm(reg(rng), src(rng), rng.below(4096) as u32),
            4 => add_reg(reg(rng), src(rng), src(rng)),
            5 => adds_reg(reg(rng), src(rng), src(rng)),
            6 => sub_reg(reg(rng), src(rng), src(rng)),
            7 => eor_reg(reg(rng), src(rng), src(rng)),
            8 => add_w(reg(rng), src(rng), src(rng)),
            9 => adc(reg(rng), src(rng), src(rng)),
            10 => csel(reg(rng), src(rng), src(rng), rng.below(14) as u32),
            11 => mov_reg(reg(rng), src(rng)),
            12 => ldr_imm(reg(rng), 2, off),
            13 => str_imm(src(rng), 2, off),
            14 => ldr_q(rng.below(8) as u32, 2, off),
            15 => str_q(rng.below(8) as u32, 2, off),
            16 => ldr_d(rng.below(8) as u32, 2, off),
            17 => fmul_d(rng.below(8) as u32, rng.below(8) as u32, rng.below(8) as u32),
            18 => mrs_nzcv(reg(rng)),
            19 => msr_nzcv(src(rng)),
            20 => add_imm(31, 31, 16 * rng.below(4) as u32), // ADD SP, SP, #imm
            _ => add_imm(reg(rng), 31, 0),                    // MOV Xd, SP
        };
        program.push(word);
    }
    program.push(ret(30));
    program
}

/// Everything a run leaves that a guest could observe.
#[derive(Debug, PartialEq)]
struct Observed {
    exit: String,
    x: Vec<u64>,
    v: Vec<u128>,
    sp: usize,
    nzcv: Nzcv,
    memory: Vec<u64>,
}

fn run_once(guest: &Guest, program: &[u32], seed: u64) -> Observed {
    let mut fill = Rng(seed | 1);
    for i in 0..32 {
        guest.write_u64(guest.data + i * 8, fill.next());
    }
    let entry = guest.load(program);
    let (mut cpu, _) = guest.thread();
    for n in 0..30u8 {
        if n != 1 && n != 2 {
            cpu.set_x(x(n), fill.next());
        }
    }
    for n in 0..8u8 {
        cpu.set_v(v(n), u128::from(fill.next()) | (u128::from(fill.next()) << 64));
    }
    cpu.set_sp(guest.data + 0x400);
    cpu.set_nzcv(Nzcv::from_pstate(fill.next()));
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("an exit");
    Observed {
        exit: exit.to_string(),
        x: (0..31u8).map(|n| cpu.x(x(n))).collect(),
        v: (0..8u8).map(|n| cpu.v(v(n))).collect(),
        sp: cpu.sp(),
        nzcv: cpu.nzcv(),
        memory: (0..32).map(|i| guest.read_u64(guest.data + i * 8)).collect(),
    }
}

/// **Differential**: random blocks, each run translated with the precise pass and without any
/// elimination, must leave identical state -- registers, flags, vectors, `SP`, memory and the exit
/// (a fault's PC and address included). One guest for both, so every address is the same; loading
/// the program drops the previous translation, so each run is translated under the switch it ran
/// with.
#[test]
fn random_blocks_leave_the_same_state_with_and_without_the_pass() {
    let _serial = serialized();
    let guest = Guest::new();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut faults = 0;
    const TRIALS: usize = 4000;
    for trial in 0..TRIALS {
        let seed = rng.next();
        let program = random_program(&mut Rng(seed), guest.unmapped as u64, guest.data as u64);
        set_precise_get_set(true);
        let on = run_once(&guest, &program, seed);
        set_precise_get_set(false);
        let off = run_once(&guest, &program, seed);
        if on.exit.contains("faulted") {
            faults += 1;
        }
        assert_eq!(on, off, "trial {trial} (seed {seed:#x}) diverged: {program:08x?}");
    }
    set_precise_get_set(true);
    println!("{TRIALS} random blocks, {faults} of them faulting mid-block: identical state");
    assert!(faults > TRIALS / 3, "too few faulting blocks ({faults}) to say anything about faults");
}
