//! Patch 0040's live switch (`omni_cpu::dynarmic::set_tbi_unmasked`, `omni-linux`'s `jit_tbi=0`):
//! a context configured with Top Byte Ignore's mask emits its accesses unmasked once the switch is
//! on, and a tagged access is served by the slow path; switched back and the code cache cleared,
//! the mask is back. Its own test binary: the switch is process-wide.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

const TAG: u64 = 0x02 << 56;

#[test]
fn the_mask_comes_off_and_back_on_while_the_context_lives() {
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
    let run = |cpu: &mut omni_cpu::dynarmic::DynarmicCpu| {
        cpu.set_x(x(1), 0x1122_3344_5566_7788);
        cpu.set_x(x(3), 0xAABB);
        cpu.set_x(x(30), sentinel as u64);
        let served = cpu.tagged_served();
        let exit = cpu.run(entry, RunLimit::Unlimited).expect("tagged accesses are not a degraded path");
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
        assert_eq!((cpu.x(x(2)), cpu.x(x(6))), (0x1122_3344_5566_7788, 0));
        assert_eq!(guest.read_u64(guest.data + 8), 0xAABB);
        cpu.tagged_served() - served
    };

    assert_eq!(run(&mut cpu), 0, "masked: the direct path");
    let on = omni_cpu::dynarmic::set_tbi_unmasked(true);
    assert_eq!(on, cfg!(target_arch = "x86_64"));
    guest.load(&p); // drops the translation, wherever it is cached
    let served = run(&mut cpu);
    if on {
        assert_eq!(served, 6, "unmasked: STR, LDR, STR Q, LDR Q, LDAXR, STLXR through the slow path");
    }
    omni_cpu::dynarmic::set_tbi_unmasked(false);
    guest.load(&p); // drops the translation, wherever it is cached
    assert_eq!(run(&mut cpu), 0, "masked again");
}
