//! Top Byte Ignore: on arm64 Linux the kernel enables TBI for user space, so a load or store
//! through a pointer carrying a tag in bits 56-63 reaches the untagged address. Android's scudo
//! tags every heap pointer with 0x02 there (`orr x9, x0, #0x200000000000000` in `allocate`), so the
//! Linux personality needs it; the Roblox path keeps D4's full 64-bit identity mapping.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

const TAG: u64 = 0x02 << 56;

/// `x0` = a tagged pointer into the data region. Store `x1` through it, load it back into `x2`,
/// then an exclusive pair through the same tagged pointer stores `x3`.
fn program(tagged: u64) -> Vec<u32> {
    let mut p = mov64(0, tagged);
    p.extend([
        str_imm(1, 0, 0),
        ldr_imm(2, 0, 0),
        add_imm(4, 0, 8),
        ldaxr(5, 4),
        stlxr(6, 3, 4),
        ret(30),
    ]);
    p
}

#[test]
fn with_tbi_a_tagged_pointer_reaches_the_untagged_address() {
    let guest = Guest::with_options(DynarmicOptions { top_byte_ignore: true, ..DynarmicOptions::default() });
    let entry = guest.load(&program(guest.data as u64 | TAG));
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(1), 0x1122_3344_5566_7788);
    cpu.set_x(x(3), 0xAABB);

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");

    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(2)), 0x1122_3344_5566_7788, "the load through the tagged pointer");
    assert_eq!(cpu.x(x(6)), 0, "the exclusive store through the tagged pointer succeeded");
    let stored = guest.space.ptr(guest.data, 16).expect("data");
    // SAFETY: the harness's data region is mapped, committed and 16 bytes long at least.
    let words = unsafe { [stored.cast::<u64>().read_unaligned(), stored.add(8).cast::<u64>().read_unaligned()] };
    assert_eq!(words, [0x1122_3344_5566_7788, 0xAABB], "both stores landed at the untagged address");
}

#[test]
fn without_tbi_a_tagged_pointer_still_faults() {
    let guest = Guest::new();
    let entry = guest.load(&program(guest.data as u64 | TAG));
    let (mut cpu, _sentinel) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    assert!(matches!(exit, ExitReason::MemoryFault { .. }), "D4's configuration is unchanged: {exit}");
}
