//! Patch 0040's live switch (`omni_cpu::dynarmic::set_tbi_unmasked`, `omni-linux`'s `jit_tbi=0`)
//! and patch 0041's learning: a context configured with Top Byte Ignore's mask emits its accesses
//! unmasked once the switch is on; a tagged access is served by the slow path, and its guest
//! instruction is noted, so that translated again it is masked -- it pays the fault once (per run
//! slice), not every time. Switched back, the mask is everywhere again. Its own test binary, one
//! test: the switch and the noted sites are process-wide.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

const TAG: u64 = 0x02 << 56;

#[test]
fn the_mask_comes_off_a_site_learns_it_back_and_the_switch_puts_it_everywhere() {
    let guest = Guest::with_options(DynarmicOptions { top_byte_ignore: true, ..DynarmicOptions::default() });
    let mut p = mov64(0, guest.data as u64 | TAG);
    p.extend([
        str_imm(1, 0, 0),
        ldr_imm(2, 0, 0),
        str_q(1, 0, 16),
        ldr_q(2, 0, 16),
        add_imm(4, 0, 8),
        ldaxr(5, 4),
        stlxr(6, 3, 4),
        ret(30),
    ]);
    let entry = guest.load(&p);
    let (mut cpu, sentinel) = guest.thread();
    let run = |cpu: &mut omni_cpu::dynarmic::DynarmicCpu, entry| {
        cpu.set_x(x(1), 0x1122_3344_5566_7788);
        cpu.set_x(x(3), 0xAABB);
        cpu.set_x(x(30), sentinel as u64);
        let served = cpu.tagged_served();
        let exit = cpu.run(entry, RunLimit::Unlimited).expect("tagged accesses are not a degraded path");
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
        cpu.tagged_served() - served
    };
    let check = |cpu: &omni_cpu::dynarmic::DynarmicCpu| {
        assert_eq!((cpu.x(x(2)), cpu.x(x(6))), (0x1122_3344_5566_7788, 0));
        assert_eq!(guest.read_u64(guest.data + 8), 0xAABB);
    };

    assert_eq!(run(&mut cpu, entry), 0, "masked: the direct path");
    check(&cpu);
    let on = omni_cpu::dynarmic::set_tbi_unmasked(true);
    assert_eq!(on, cfg!(target_arch = "x86_64"));
    guest.load(&p); // drops the translation, wherever it is cached
    let first = run(&mut cpu, entry);
    check(&cpu);
    if on {
        assert_eq!(first, 6, "unmasked: STR, LDR, STR Q, LDR Q, LDAXR, STLXR through the slow path, once each");
        assert_eq!(run(&mut cpu, entry), 0, "patch 0041: each site met a tag, so it is masked now");
        check(&cpu);
    }

    // A loop: the first iteration runs in the entry block, which faults once and notes the load's
    // instruction; the loop's own block is translated after that, already masked. (A block that
    // loops on itself would keep faulting until the run returns and its translation is dropped.)
    const N: u64 = 500;
    let mut l = mov64(0, guest.data as u64 | TAG);
    l.extend(mov64(9, N));
    let top = l.len();
    l.extend([ldr_imm(10, 0, 32), add_imm(10, 10, 1), subs_imm(9, 9, 1)]);
    let here = l.len();
    l.push(b_cond(1, top as i32 - here as i32));
    l.push(ret(30));
    let looped = guest.load_at(0x1000, &l);
    let first = run(&mut cpu, looped);
    if on {
        assert_eq!(first, 1, "one fault, in the entry block; the loop's block was translated masked");
        assert_eq!(cpu.x(x(9)), 0, "and the loop ran to the end");
        assert_eq!(run(&mut cpu, looped), 0, "the next: masked");
        // Untagged accesses at the same kind of site in a fresh place stay unmasked and direct.
        let before = cpu.slow_path_entries();
        let mut u = mov64(0, guest.data as u64);
        u.extend([ldr_imm(11, 0, 32), ret(30)]);
        let untagged = guest.load_at(0x2000, &u);
        assert_eq!(run(&mut cpu, untagged), 0);
        assert_eq!(cpu.slow_path_entries() - before, 0, "an untagged access is direct");
    }

    omni_cpu::dynarmic::set_tbi_unmasked(false);
    guest.load(&p);
    assert_eq!(run(&mut cpu, entry), 0, "masked again");
    check(&cpu);
}
