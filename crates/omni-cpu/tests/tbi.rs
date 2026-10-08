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

    let before = cpu.slow_path_entries();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");

    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(
        cpu.slow_path_entries() - before,
        0,
        "the tagged accesses stayed on the direct path: TBI is a mask, not a detour through callbacks"
    );
    assert_eq!(cpu.x(x(2)), 0x1122_3344_5566_7788, "the load through the tagged pointer");
    assert_eq!(cpu.x(x(6)), 0, "the exclusive store through the tagged pointer succeeded");
    let stored = guest.space.ptr(guest.data, 16).expect("data");
    // SAFETY: the harness's data region is mapped, committed and 16 bytes long at least.
    let words = unsafe { [stored.cast::<u64>().read_unaligned(), stored.add(8).cast::<u64>().read_unaligned()] };
    assert_eq!(words, [0x1122_3344_5566_7788, 0xAABB], "both stores landed at the untagged address");
}

/// Patch 0040: the mask as one `and` against a pool constant reaches the same addresses, on the
/// direct path. (The switch is process-wide; other tests here may run under it, which is the
/// point: it changes no address.)
#[test]
fn with_tbi_masked_by_one_and_a_tagged_pointer_reaches_the_untagged_address() {
    omni_cpu::dynarmic::set_fastmem_mask_by_and(true);
    let guest = Guest::with_options(DynarmicOptions { top_byte_ignore: true, ..DynarmicOptions::default() });
    let entry = guest.load(&program(guest.data as u64 | TAG));
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(1), 0x1122_3344_5566_7788);
    cpu.set_x(x(3), 0xAABB);
    let before = cpu.slow_path_entries();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    omni_cpu::dynarmic::set_fastmem_mask_by_and(false);
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.slow_path_entries() - before, 0, "the direct path");
    assert_eq!((cpu.x(x(2)), cpu.x(x(6))), (0x1122_3344_5566_7788, 0));
    assert_eq!((guest.read_u64(guest.data), guest.read_u64(guest.data + 8)), (0x1122_3344_5566_7788, 0xAABB));
}

/// Without the option the translator adds no mask (D4's configuration is unchanged), so what a
/// tagged pointer does is the host MMU's answer: a fault on x86-64, the untagged address where the
/// host kernel itself enables TBI (arm64 Linux, macOS on Apple silicon).
#[test]
fn without_tbi_a_tagged_pointer_is_the_host_mmus_business() {
    let guest = Guest::new();
    let entry = guest.load(&program(guest.data as u64 | TAG));
    let (mut cpu, sentinel) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    if omni_platform::vm::host_ignores_top_byte() {
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "the host ignored the tag: {exit}");
    } else {
        assert!(matches!(exit, ExitReason::MemoryFault { .. }), "D4's configuration is unchanged: {exit}");
    }
}

/// bionic's `malloc` starts with `ldar x8, [__libc_globals + 0x48]`, a load-acquire from a page
/// libc has write-protected. It must stay on the direct path under TBI as it does without.
fn load_acquire_from_read_only(options: DynarmicOptions) -> u64 {
    let guest = Guest::with_options(options);
    let mut program = mov64(0, guest.readonly as u64);
    program.extend([ldar(1, 0), ret(30)]);
    let entry = guest.load(&program);
    let (mut cpu, sentinel) = guest.thread();
    let before = cpu.slow_path_entries();
    let exit = cpu
        .run(entry, RunLimit::Unlimited)
        .unwrap_or_else(|e| panic!("top_byte_ignore = {}: {e}", options.top_byte_ignore));
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    cpu.slow_path_entries() - before
}

/// TBI with the direct path left at D4's 64-bit identity (`tbi_direct_mask: false`,
/// `OMNI_JIT_TBI=0`): untagged accesses are direct, a tagged one faults on the host (x86-64:
/// non-canonical) and is served by the slow path, counted, and not taken for a degraded block.
fn unmasked() -> DynarmicOptions {
    DynarmicOptions { top_byte_ignore: true, tbi_direct_mask: false, ..DynarmicOptions::default() }
}

/// Whether this host runs `tbi_direct_mask: false` as asked (x64), rather than masking anyway.
const UNMASKED_HERE: bool = cfg!(target_arch = "x86_64");

#[test]
fn unmasked_a_tagged_pointer_still_reaches_the_untagged_address_through_the_slow_path() {
    let guest = Guest::with_options(unmasked());
    let entry = guest.load(&program(guest.data as u64 | TAG));
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(1), 0x1122_3344_5566_7788);
    cpu.set_x(x(3), 0xAABB);
    let before = (cpu.slow_path_entries(), omni_cpu::dynarmic::tagged_accesses());

    // The per-slice invariant is armed (the default): a tagged access served by the slow path must
    // not read as a block that stopped reaching memory directly.
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("tagged accesses are not a degraded memory path");

    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(2)), 0x1122_3344_5566_7788, "the load through the tagged pointer");
    assert_eq!(cpu.x(x(6)), 0, "the exclusive store through the tagged pointer succeeded");
    let stored = guest.space.ptr(guest.data, 16).expect("data");
    // SAFETY: the harness's data region is mapped, committed and 16 bytes long at least.
    let words = unsafe { [stored.cast::<u64>().read_unaligned(), stored.add(8).cast::<u64>().read_unaligned()] };
    assert_eq!(words, [0x1122_3344_5566_7788, 0xAABB], "both stores landed at the untagged address");
    if UNMASKED_HERE {
        // STR, LDR, LDAXR, STLXR: four tagged accesses, each through the slow path.
        assert_eq!(cpu.tagged_served(), 4, "each tagged access is counted once");
        assert!(omni_cpu::dynarmic::tagged_accesses() - before.1 >= 4, "and process-wide");
        assert_eq!(cpu.slow_path_entries() - before.0, 4, "and nothing else left the direct path");
    }
}

/// Every width and kind of access through a tagged pointer, in a loop, so each site is reached
/// again after its first fault (the site is not recompiled: every iteration faults and is served).
#[test]
fn unmasked_every_access_kind_through_a_tagged_pointer_is_served_every_time() {
    /// `STP`/`LDP Xt, Xt2, [Xn, #off]`, `STRB`/`LDRB Wt, [Xn, #off]`.
    const fn stp(rt: u32, rt2: u32, rn: u32, off: u32) -> u32 {
        0xA900_0000 | ((off / 8) << 15) | (rt2 << 10) | (rn << 5) | rt
    }
    const fn ldp(rt: u32, rt2: u32, rn: u32, off: u32) -> u32 {
        0xA940_0000 | ((off / 8) << 15) | (rt2 << 10) | (rn << 5) | rt
    }
    const fn strb(rt: u32, rn: u32, off: u32) -> u32 {
        0x3900_0000 | (off << 10) | (rn << 5) | rt
    }
    const fn ldrb(rt: u32, rn: u32, off: u32) -> u32 {
        0x3940_0000 | (off << 10) | (rn << 5) | rt
    }
    const ITERATIONS: u64 = 100;
    let guest = Guest::with_options(unmasked());
    let mut p = mov64(0, guest.data as u64 | TAG);
    p.extend(mov64(9, ITERATIONS));
    let top = p.len();
    p.extend([
        stp(1, 3, 0, 32),     // [32] = x1, [40] = x3
        ldp(10, 11, 0, 32),   // x10, x11
        str_q(1, 0, 48),      // [48..64) = q1
        ldr_q(2, 0, 48),      // q2
        strb(3, 0, 64),       // [64] = low byte of x3
        ldrb(12, 0, 64),      // x12
        stlr(1, 0),           // [0] = x1 (ordered)
        ldar(13, 0),          // x13
        add_imm(14, 0, 8),
        ldaxr(15, 14),        // exclusive pair on [8]
        add_imm(15, 15, 1),
        stlxr(16, 15, 14),
        subs_imm(9, 9, 1),
    ]);
    let here = p.len();
    p.push(b_cond(1, top as i32 - here as i32));
    p.push(ret(30));
    let entry = guest.load(&p);
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(1), 0x0102_0304_0506_0708);
    cpu.set_x(x(3), 0x99AA);
    cpu.set_v(omni_cpu::VReg::new(1).expect("v1"), 0xFEDC_BA98_7654_3210_0011_2233_4455_6677);
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("tagged accesses are not a degraded memory path");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!((cpu.x(x(10)), cpu.x(x(11))), (0x0102_0304_0506_0708, 0x99AA), "LDP after STP");
    assert_eq!(cpu.v(omni_cpu::VReg::new(2).expect("v2")), 0xFEDC_BA98_7654_3210_0011_2233_4455_6677, "128-bit");
    assert_eq!(cpu.x(x(12)), 0xAA, "a byte");
    assert_eq!(cpu.x(x(13)), 0x0102_0304_0506_0708, "LDAR after STLR");
    assert_eq!(cpu.x(x(16)), 0, "the last exclusive store succeeded");
    assert_eq!(guest.read_u64(guest.data + 8), ITERATIONS, "every exclusive increment landed");
    if UNMASKED_HERE {
        // Ten accesses an iteration (the pair, the 128-bit pair, the bytes, the ordered pair, the
        // exclusive pair; STP/LDP may be split by the translator, so at least).
        assert!(cpu.tagged_served() >= 10 * ITERATIONS, "served {}", cpu.tagged_served());
    }
}

/// An untagged pointer stays on the direct path, and a tagged one into nothing still faults --
/// naming the untagged address, as arm64 Linux reports it.
#[test]
fn unmasked_untagged_accesses_stay_direct_and_a_tagged_wild_pointer_still_faults() {
    let guest = Guest::with_options(unmasked());
    let entry = guest.load(&program(guest.data as u64));
    let (mut cpu, sentinel) = guest.thread();
    let before = cpu.slow_path_entries();
    assert_eq!(cpu.run(entry, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.slow_path_entries() - before, 0, "untagged: the direct path, as D4 has it");
    assert_eq!(cpu.tagged_served(), 0);

    let mut p = mov64(0, guest.unmapped as u64 | TAG);
    p.extend([ldr_imm(1, 0, 0), ret(30)]);
    let entry = guest.load(&p);
    let (mut cpu, _) = guest.thread();
    match cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit") {
        ExitReason::MemoryFault { address, .. } => assert_eq!(address, guest.unmapped),
        other => panic!("expected a fault, got {other}"),
    }
    // And a store through a tagged pointer to a read-only page is refused, not committed.
    let mut p = mov64(0, guest.readonly as u64 | TAG);
    p.extend([str_imm(1, 0, 0), ret(30)]);
    let entry = guest.load(&p);
    let (mut cpu, _) = guest.thread();
    assert!(
        matches!(
            cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit"),
            ExitReason::MemoryFault { access: omni_cpu::AccessKind::Write, .. }
        ),
        "a tagged store to a read-only page"
    );
}

#[test]
fn a_load_acquire_from_a_read_only_page_stays_on_the_direct_path() {
    assert_eq!(load_acquire_from_read_only(DynarmicOptions::default()), 0, "without TBI");
    let tbi = DynarmicOptions { top_byte_ignore: true, ..DynarmicOptions::default() };
    assert_eq!(load_acquire_from_read_only(tbi), 0, "with TBI");
    assert_eq!(load_acquire_from_read_only(unmasked()), 0, "with TBI, the direct path unmasked");
}
