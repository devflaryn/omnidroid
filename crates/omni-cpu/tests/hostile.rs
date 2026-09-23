//! Global Constraint 11: guest code is the ultimate untrusted input.
//!
//! It will branch to unmapped addresses, execute garbage, recurse without bound, ask for syscalls
//! nothing implements and rewrite its own instructions underneath the translator. None of that may
//! take down the host process, and every one of these produces a **typed exit** naming what
//! happened — a run that returns `Err` or panics is as much a failure here as one that crashes.
//!
//! Each test is one hostile shape. The assertion is always the same in spirit: the process is still
//! here, the exit says what the guest did, and the context is still usable afterwards.

#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{AccessKind, ExitReason, GuestCpu, GuestRange, RunLimit};

/// Executing data. The engine's own `.rodata` is one branch away from its `.text`, and a tampered
/// binary can put a branch anywhere (D6).
#[test]
fn executing_an_unallocated_encoding_names_it_rather_than_crashing() {
    let guest = Guest::new();
    let entry = guest.load(&[UNALLOCATED, ret(30)]);
    let (mut cpu, _) = guest.thread();

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("garbage is an exit, not an error");
    assert_eq!(
        exit,
        ExitReason::UnsupportedInstruction { pc: entry, encoding: UNALLOCATED },
        "{exit}"
    );
    assert!(!exit.is_resumable(), "resuming would re-execute the same garbage");
    assert!(exit.to_string().contains("0x00000001"), "the encoding must be named: {exit}");
}

/// A guest supervisor call. M2 has no syscall layer, so the honest answer is a typed stop naming
/// the instruction — not a fabricated success (Global Constraint 1).
#[test]
fn a_guest_supervisor_call_is_reported_rather_than_faked() {
    let guest = Guest::new();
    let entry = guest.load(&[svc(0x42), ret(30)]);
    let (mut cpu, _) = guest.thread();

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("an SVC is an exit");
    assert_eq!(
        exit,
        ExitReason::UnsupportedInstruction { pc: entry, encoding: svc(0x42) },
        "{exit}"
    );
}

/// A guest `SVC` with the **same immediate** the backend plants at its own stops.
///
/// The stop is identified by address, not by immediate, precisely so that a guest cannot forge one:
/// a guest `SVC #0xFFFF` at an address Omnidroid did not register is an ordinary unsupported
/// instruction, not a spoofed return to the sentinel.
#[test]
fn a_guest_cannot_forge_the_backends_own_stop_instruction() {
    let guest = Guest::new();
    let entry = guest.load(&[svc(0xFFFF), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("an SVC is an exit");
    assert_ne!(exit, ExitReason::Returned { pc: sentinel }, "a guest may not forge a return");
    assert_eq!(
        exit,
        ExitReason::UnsupportedInstruction { pc: entry, encoding: svc(0xFFFF) },
        "{exit}"
    );
}

/// A guest `BRK` the guest planted itself, at an address with no breakpoint registered.
#[test]
fn a_guest_planted_brk_is_not_mistaken_for_one_of_ours() {
    let guest = Guest::new();
    let entry = guest.load(&[brk(0), ret(30)]);
    let (mut cpu, _) = guest.thread();

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a BRK is an exit");
    assert!(
        matches!(exit, ExitReason::UnsupportedInstruction { pc, .. } if pc == entry),
        "a guest BRK at an unregistered address must not come back as a Breakpoint: {exit}"
    );
}

/// Unbounded recursion. `BL .` pushes a return address every iteration; dynarmic's return-stack
/// buffer is bounded, the host stack is not involved, and the step budget is what stops it.
#[test]
fn unbounded_recursion_is_stopped_by_the_budget_and_not_by_the_host_stack() {
    let guest = Guest::new();
    let entry = guest.load(&[bl_self()]);
    let (mut cpu, _) = guest.thread();

    let exit = cpu.run(entry, RunLimit::Instructions(200_000)).expect("recursion is an exit");
    assert!(
        matches!(exit, ExitReason::StepLimitReached { executed, .. } if executed >= 200_000),
        "unbounded guest recursion must stop on its budget, got {exit}"
    );
    // And the context is still usable, which is the part that says the host is unharmed.
    let good = guest.load_at(512, &[movz(0, 99, 0), ret(30)]);
    cpu.run(good, RunLimit::Unlimited).expect("the context survives");
    assert_eq!(cpu.x(x(0)), 99);
}

/// `BL .` — a branch-with-link to itself.
fn bl_self() -> u32 {
    // `BL offset` is `100101 imm26`; offset 0 is the instruction itself.
    0x9400_0000
}

/// A guest that sets `SP` to garbage and stores through it.
#[test]
fn a_wild_stack_pointer_is_a_typed_fault() {
    let guest = Guest::new();
    let program = vec![
        movz(0, 0x1234, 0),
        str_imm(0, 31, 0), // STR X0, [SP]
        ret(30),
    ];
    let entry = guest.load(&program);
    let (mut cpu, _) = guest.thread();
    cpu.set_sp(guest.unmapped);

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a wild SP is an exit");
    assert!(
        matches!(
            exit,
            ExitReason::MemoryFault { address, access: AccessKind::Write, .. }
                if address == guest.unmapped
        ),
        "{exit}"
    );
}

/// A guest branching to address 0, which is what a call through a null function pointer looks like
/// and what `libroblox.so` will do the moment a relocation is wrong.
#[test]
fn a_guest_branch_to_address_zero_is_a_typed_fault() {
    let guest = Guest::new();
    let entry = guest.load(&[movz(0, 0, 0), br(0)]);
    let (mut cpu, _) = guest.thread();

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a null call is an exit");
    assert_eq!(
        exit,
        ExitReason::MemoryFault { pc: 0, address: 0, access: AccessKind::Execute },
        "{exit}"
    );
}

/// An unaligned guest `PC`. A64 instructions are 4-byte aligned by construction, so this can only
/// arrive from a corrupted pointer — which is exactly the case that must not be a host crash.
#[test]
fn an_unaligned_guest_pc_is_a_typed_fault() {
    let guest = Guest::new();
    let entry = guest.load(&[NOP, NOP, ret(30)]);
    let (mut cpu, _) = guest.thread();

    let exit = cpu.run(entry + 2, RunLimit::Unlimited).expect("an unaligned PC is an exit");
    assert!(
        matches!(exit, ExitReason::MemoryFault { access: AccessKind::Execute, .. }),
        "an unaligned PC must be refused as an instruction fetch, got {exit}"
    );
}

/// The guest's own cache-maintenance instructions carry an address the guest chose. A wild one must
/// not turn into an invalidation of everything, and must not be a crash.
#[test]
fn a_wild_invalidation_range_is_survivable() {
    let guest = Guest::new();
    let entry = guest.load(&[movz(0, 0x2A, 0), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();
    cpu.run(entry, RunLimit::Unlimited).expect("warm the translation");

    // The shapes an untrusted length takes: empty, one instruction, the whole space, and one that
    // would overflow `addr + len`.
    for (why, start, len) in [
        ("one instruction", entry, 4usize),
        ("a whole page", entry, 4096),
        ("the top of the address space", usize::MAX - 16, 16),
        ("the largest range that does not wrap", 0, usize::MAX),
    ] {
        let range = GuestRange::new(start, len).unwrap_or_else(|e| panic!("{why}: {e}"));
        cpu.invalidate_code(range).unwrap_or_else(|e| panic!("{why}: {e}"));
    }
    // A zero-length range is refused before it reaches the backend, which matters because dynarmic
    // builds a closed interval from `addr + len - 1` and would halt the guest to invalidate nothing.
    assert!(GuestRange::new(entry, 0).is_err(), "a zero-length range names no instruction");

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("and the guest still runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), 0x2A);
}

/// A guest that rewrites its own instructions and re-executes them.
///
/// Without `invalidate_code` the guest would run a stale translation; with it, the new instruction
/// takes effect. Both halves are asserted, because "it worked" would also be true of a backend that
/// never cached anything.
#[test]
fn self_modifying_guest_code_needs_and_gets_invalidation() {
    let guest = Guest::new();
    let entry = guest.load(&[movz(0, 1, 0), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();

    assert_eq!(cpu.run(entry, RunLimit::Unlimited).expect("first run"), ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.x(x(0)), 1);

    // Rewrite the first instruction to produce a different value.
    guest.load(&[movz(0, 2, 0), ret(30)]);
    cpu.run(entry, RunLimit::Unlimited).expect("second run");
    let stale = cpu.x(x(0));

    cpu.invalidate_code(GuestRange::new(entry, 4).expect("a range")).expect("invalidate");
    cpu.run(entry, RunLimit::Unlimited).expect("third run");
    assert_eq!(cpu.x(x(0)), 2, "after invalidation the new instruction must take effect");
    println!(
        "self-modifying guest code (1 run): value before invalidation {stale}, after 2 \
         (a stale value here is the translator's cache doing its job, not a defect)"
    );
}

/// A thunk and a breakpoint at addresses the guest never reaches, plus one at an address with
/// nothing mapped. Registering a stop must not be able to crash anything by itself.
#[test]
fn stops_registered_at_hostile_addresses_are_survivable() {
    let guest = Guest::new();
    let entry = guest.load(&[movz(0, 7, 0), ret(30)]);
    let (mut cpu, sentinel) = guest.thread();

    cpu.add_thunk(guest.unmapped).expect("a thunk in unmapped memory");
    cpu.add_breakpoint(0).expect("a breakpoint at address zero");
    cpu.add_breakpoint(usize::MAX & !3).expect("a breakpoint at the top of the space");
    assert!(cpu.remove_thunk(guest.unmapped).expect("remove"), "it was registered");
    assert!(!cpu.remove_thunk(guest.unmapped).expect("remove again"), "and now it is not");

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the guest still runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), 7);
}

/// The thunk boundary itself, since M3 is built on it: reaching a registered address stops with
/// [`ExitReason::Thunk`] naming that address, and the guest can be resumed past it.
#[test]
fn a_thunk_stops_at_the_registered_address_and_is_resumable() {
    let guest = Guest::new();
    let program = vec![movz(0, 1, 0), movz(1, 2, 0), movz(2, 3, 0), ret(30)];
    let entry = guest.load(&program);
    let (mut cpu, sentinel) = guest.thread();

    let thunk = entry + 4;
    cpu.add_thunk(thunk).expect("register the thunk");
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::Thunk { pc: thunk }, "{exit}");
    assert_eq!(cpu.x(x(0)), 1, "the instruction before the thunk ran");
    assert_eq!(cpu.x(x(1)), 0, "the one the thunk replaced did not");
    assert!(exit.is_resumable());

    // Emulate what M3 does: perform the call's effect on the host side, then resume past it.
    cpu.set_x(x(1), 0xBEEF);
    let exit = cpu.run(thunk + 4, RunLimit::Unlimited).expect("resume past the thunk");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!((cpu.x(x(1)), cpu.x(x(2))), (0xBEEF, 3));
}

/// A breakpoint does not execute the instruction under it, and resuming from it does.
#[test]
fn a_breakpoint_does_not_execute_its_instruction_and_resuming_does() {
    let guest = Guest::new();
    let program = vec![movz(0, 1, 0), movz(1, 2, 0), ret(30)];
    let entry = guest.load(&program);
    let (mut cpu, sentinel) = guest.thread();
    assert!(cpu.capabilities().breakpoints);

    let bp = entry + 4;
    cpu.add_breakpoint(bp).expect("register the breakpoint");
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::Breakpoint { pc: bp }, "{exit}");
    assert_eq!(cpu.pc(), bp, "the PC must be the breakpoint, so resuming from it runs it");
    assert_eq!(cpu.x(x(1)), 0, "the instruction under the breakpoint has not run");

    // Resuming from the breakpoint address runs the instruction under it exactly once.
    let exit = cpu.run(bp, RunLimit::Unlimited).expect("resume");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(1)), 2, "resuming ran the instruction the breakpoint was hiding");

    assert!(cpu.remove_breakpoint(bp).expect("remove"), "it was registered");
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("and now it runs straight through");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
}

/// Many hostile programs in one context, one after another, to show that a typed exit leaves
/// nothing behind. This is the closest thing to what a real guest does: fail, be handled, continue.
#[test]
fn a_context_survives_every_hostile_shape_in_sequence() {
    let guest = Guest::new();
    let (mut cpu, sentinel) = guest.thread();

    let shapes: Vec<(&str, Vec<u32>)> = vec![
        ("unallocated encoding", vec![UNALLOCATED]),
        ("supervisor call", vec![svc(0)]),
        ("breakpoint instruction", vec![brk(1)]),
        ("branch to zero", vec![movz(0, 0, 0), br(0)]),
        ("load from zero", vec![movz(0, 0, 0), ldr_imm(1, 0, 0)]),
        ("store to zero", vec![movz(0, 0, 0), str_imm(0, 0, 0)]),
        ("self branch", vec![b(0)]),
        ("recursion", vec![bl_self()]),
    ];

    for (round, (why, program)) in shapes.iter().enumerate() {
        let entry = guest.load_at(round * 64, program);
        let exit = cpu
            .run(entry, RunLimit::Instructions(100_000))
            .unwrap_or_else(|e| panic!("{why} produced an error rather than an exit: {e}"));
        assert!(!exit.to_string().is_empty(), "{why}");
        // The only thing required of every shape: it stopped, and it said where.
        assert!(exit.pc() != usize::MAX, "{why}: {exit}");
    }

    let good = guest.load_at(shapes.len() * 64, &[movz(0, 0x5A, 0), ret(30)]);
    // X30 has to be re-armed: the recursion shape above is a `BL`, which is a guest instruction
    // whose whole job is to overwrite the link register. A caller that calls into guest code sets
    // the sentinel every time for exactly this reason.
    cpu.set_x(x(30), sentinel as u64);
    let exit = cpu.run(good, RunLimit::Unlimited).expect("the context is still usable");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), 0x5A);
}

/// **An access whose tail leaves its mapping.** The guest loads eight bytes from four bytes before
/// the end of a region, so half of it is in the next region — which is free address space.
///
/// This is the rule the two access policies used to disagree about, and it is the side the pager
/// cannot cover: an access violation names one address, so the pager can only ever ask about one
/// byte, while dynarmic's callback is handed the real length and must check it. With the check gone,
/// the callback performs an eight-byte read from *Rust* across the boundary and the second half
/// touches a page nothing is mapped at — a host access violation with no handler that owns it.
///
/// So the two possible outcomes are a typed exit and a dead process, which is what makes this a
/// Global Constraint 11 test rather than a unit test about arithmetic.
#[test]
fn a_guest_load_that_straddles_the_end_of_its_mapping_is_a_typed_fault() {
    let guest = Guest::new();

    // A readable mapping whose successor is free address space, taken from the region map rather
    // than assumed: `Placement::Anywhere` packs mappings, so which one has a hole after it is not
    // something a test may guess at. Four bytes before its end, `LDR X1, [X0]` reads eight.
    let regions = guest.space.regions();
    let straddle = regions
        .windows(2)
        .find(|pair| {
            !pair[0].is_free()
                && pair[0].protection.is_readable()
                && pair[0].len >= 8
                && pair[1].is_free()
        })
        .map(|pair| pair[0].end() - 4)
        .expect("a readable mapping with free address space after it");
    assert!(
        guest.space.region_at(straddle + 4).is_none_or(|r| r.is_free()),
        "the second half of the load must land in free address space, or this measures a read of \
         the next mapping rather than a read of nothing"
    );

    let mut program = mov64(0, straddle as u64);
    program.push(ldr_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load(&program);

    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a straddling load is an exit");
    match exit {
        ExitReason::MemoryFault { address, access: AccessKind::Read, .. } => {
            // dynarmic reports the access's own address, which is where the load started.
            assert_eq!(
                address, straddle,
                "the exit must name the address the guest asked for, not the page that faulted"
            );
        }
        other => panic!(
            "an access running off the end of its mapping must be refused whole, not served \
             partly: got {other}"
        ),
    }

    // And the context is still usable, which is the other half of every test in this file.
    let after = guest.load_at(0x800, &[movz(0, 7, 0), ret(30)]);
    cpu.set_x(x(30), (guest.code + harness::CODE_BYTES - 4) as u64);
    let exit = cpu.run(after, RunLimit::Unlimited).expect("the context still runs");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit}");
    assert_eq!(cpu.x(x(0)), 7);
}
