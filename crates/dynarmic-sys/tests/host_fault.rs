//! **A real host fault inside translated code**, through dynarmic's own fastmem handler.
//!
//! Under identity fastmem (D4: base 0, 64 address bits -- `omni-cpu`'s configuration) a guest address
//! is dereferenced as the host address. An unmapped one is a host `EXC_BAD_ACCESS` (macOS) / access
//! violation (Windows) at the patched load or store; dynarmic's handler looks the host PC up in the
//! block's `fastmem_patch_info`, redirects to the fallback, which serves the access through the
//! callbacks, and (with `recompile_on_fastmem_failure`) rebuilds the block without fastmem. Nothing
//! else in `dynarmic-sys`'s tests takes that path -- the harness arena masks every address -- so on
//! the arm64 host it had never run. Here it runs with *no* fault handler of Omnidroid's installed,
//! so what it measures is dynarmic alone.
//!
//! Guest address `0x2000` is unmapped on every host this runs on: macOS reserves the low 4 GiB as
//! `__PAGEZERO`, Windows never maps the low 64 KiB. The callbacks mask it into the arena, so the
//! values the guest sees are the arena's and can be asserted.

mod harness;

use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};

/// Unmapped on the host; `0x2000` in the arena once the callbacks mask it.
const HOLE: u64 = 0x2000;

fn identity() -> VmOptions {
    VmOptions { identity: true, check_halt_on_memory_access: true, fastmem_exclusive: true, ..VmOptions::default() }
}

fn run(code: Vec<u32>, regs: &[(u32, u64)]) -> Vm {
    let vm = Vm::new(code, identity());
    vm.with_ctx(|c| {
        c.write_u64(HOLE, 0x1122_3344_5566_7788);
        c.write_u64(HOLE + 8, 0x99AA_BBCC_DDEE_FF00);
    });
    vm.set_reg(0, HOLE);
    for (r, v) in regs {
        vm.set_reg(*r, *v);
    }
    vm.start(1_000_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "the guest did not reach its SVC: halt {hr:#010X}");
    vm
}

#[test]
fn a_load_that_faults_on_the_host_is_served_by_the_callbacks() {
    // LDR X1, [X0] ; LDR X2, [X0, #8] ; SVC -- the first fault rebuilds the block without fastmem.
    let vm = run(vec![a64::ldr_imm(1, 0, 0), a64::ldr_imm(2, 0, 8), a64::svc(0)], &[]);
    assert_eq!(vm.reg(1), 0x1122_3344_5566_7788);
    assert_eq!(vm.reg(2), 0x99AA_BBCC_DDEE_FF00);
    assert!(vm.stats().slow_path_reads >= 2, "{:?}", vm.stats());
    // Callee-saved state the fallback depends on survived the fault: running again works.
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE, "a second run after the fault");
}

#[test]
fn a_store_that_faults_on_the_host_is_served_by_the_callbacks() {
    // MOVZ X1, #0xBEEF ; STR X1, [X0] ; SVC
    let vm = run(vec![a64::movz(1, 0xBEEF, 0), a64::str_imm(1, 0, 0), a64::svc(0)], &[]);
    assert_eq!(vm.with_ctx(|c| c.read_u64(HOLE)), 0xBEEF);
    assert!(vm.stats().slow_path_writes >= 1, "{:?}", vm.stats());
}

#[test]
fn an_exclusive_pair_that_faults_on_the_host_is_served_through_the_monitor() {
    // LDXR X1, [X0] ; STXR W2, X5, [X0] ; SVC -- inline exclusives (patch 0007), host fault at the
    // load-acquire of each, fallback through the monitor.
    const LDXR: u32 = 0xC85F_7C01; // LDXR X1, [X0]
    const STXR: u32 = 0xC802_7C05; // STXR W2, X5, [X0]
    let vm = run(vec![LDXR, STXR, a64::svc(0)], &[(5, 0x5555)]);
    assert_eq!(vm.reg(1), 0x1122_3344_5566_7788);
    assert_eq!(vm.reg(2), 0, "the store-exclusive succeeded through the fallback");
    assert_eq!(vm.with_ctx(|c| c.read_u64(HOLE)), 0x5555);
}

#[test]
fn an_exclusive_doubleword_pair_that_faults_on_the_host_gives_its_borrowed_registers_back() {
    // LDXP X1, X3, [X0] ; STXP W2, X5, X6, [X0] ; SVC. The 128-bit store borrows four registers on
    // the stack around its compare-and-swap; the fault entry must restore them before the fallback.
    // X7..X15 carry sentinels that must survive whichever four are borrowed.
    const LDXP: u32 = 0xC87F_0C01; // LDXP X1, X3, [X0]
    const STXP: u32 = 0xC822_1805; // STXP W2, X5, X6, [X0]
    let sentinels: Vec<(u32, u64)> = (7..16).map(|r| (r, 0xA000_0000_0000_0000 | u64::from(r))).collect();
    let mut regs = vec![(5, 0x5151), (6, 0x6262)];
    regs.extend(&sentinels);
    let vm = run(vec![LDXP, STXP, a64::svc(0)], &regs);
    assert_eq!((vm.reg(1), vm.reg(3)), (0x1122_3344_5566_7788, 0x99AA_BBCC_DDEE_FF00));
    assert_eq!(vm.reg(2), 0, "STXP succeeded through the fallback");
    assert_eq!(vm.with_ctx(|c| (c.read_u64(HOLE), c.read_u64(HOLE + 8))), (0x5151, 0x6262));
    for (r, v) in sentinels {
        assert_eq!(vm.reg(r), v, "X{r} changed across the faulting STXP");
    }
}
