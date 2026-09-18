//! Guest faults: a typed exit instead of a host crash, and Omnidroid owning guest paging (D10).

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{AccessKind, ExitReason, GuestCpu, RunLimit};
use omni_platform::fault;

/// **The one the brief asks for.** A guest load from an address nothing is mapped at must produce a
/// clean typed exit naming the address, not a host access violation.
#[test]
fn a_guest_read_of_an_unmapped_address_is_a_typed_exit() {
    let guest = Guest::new();
    let mut program = mov64(0, guest.unmapped as u64);
    program.push(ldr_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load(&program);

    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit, not an error");

    match exit {
        ExitReason::MemoryFault { pc, address, access } => {
            assert_eq!(address, guest.unmapped, "the exit must name the address that faulted");
            assert_eq!(access, AccessKind::Read);
            // The PC is the faulting instruction: the `LDR` is the last word of the program before
            // the `RET`, and `check_halt_on_memory_access` makes the emitter store exactly it.
            let ldr = entry + (program.len() - 2) * 4;
            assert_eq!(pc, ldr, "the exit must name the faulting instruction, not the block entry");
        }
        other => panic!("expected a typed memory fault, got {other}"),
    }
    assert!(!exit.is_resumable(), "resuming would reproduce the fault forever");
    assert!(exit.to_string().contains("read"), "{exit}");
}

/// The same for a store, which is a different emitted path in dynarmic.
#[test]
fn a_guest_write_to_an_unmapped_address_is_a_typed_exit() {
    let guest = Guest::new();
    let mut program = mov64(0, guest.unmapped as u64);
    program.push(movz(1, 0x1234, 0));
    program.push(str_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load(&program);

    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit");
    assert!(
        matches!(
            exit,
            ExitReason::MemoryFault { address, access: AccessKind::Write, .. }
                if address == guest.unmapped
        ),
        "expected a typed write fault at {:#x}, got {exit}",
        guest.unmapped
    );
}

/// A guest write to a page it only has read access to must be refused, not committed our way out of.
///
/// This is the case the demand pager could silently get wrong: it *is* a mapped guest page, and
/// committing it would hand the guest a permission it does not have.
#[test]
fn a_guest_write_to_a_read_only_page_is_refused_rather_than_committed() {
    let guest = Guest::new();
    let mut program = mov64(0, guest.readonly as u64);
    program.push(movz(1, 0xBEEF, 0));
    program.push(str_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load(&program);

    let before = guest.read_u64(guest.readonly);
    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit");
    assert!(
        matches!(exit, ExitReason::MemoryFault { access: AccessKind::Write, .. }),
        "a write to a read-only guest page must fault, got {exit}"
    );
    assert_eq!(guest.read_u64(guest.readonly), before, "and nothing may have been written");
}

/// A guest branch into memory with no executable mapping.
///
/// This one never reaches the host at all: `read_code` refuses the fetch, dynarmic raises
/// `NoExecuteFault`, and it comes out as an `Execute` fault naming the address.
#[test]
fn a_guest_branch_into_unmapped_memory_is_a_typed_exit() {
    let guest = Guest::new();
    let mut program = mov64(0, guest.unmapped as u64);
    program.push(br(0));
    let entry = guest.load(&program);

    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit");
    assert_eq!(
        exit,
        ExitReason::MemoryFault {
            pc: guest.unmapped,
            address: guest.unmapped,
            access: AccessKind::Execute
        },
        "{exit}"
    );
}

/// A guest branch into memory that is mapped but **not executable** — the data region.
#[test]
fn a_guest_branch_into_non_executable_memory_is_a_typed_exit() {
    let guest = Guest::new();
    guest.write_u64(guest.data, u64::from(NOP) | (u64::from(ret(30)) << 32));
    let mut program = mov64(0, guest.data as u64);
    program.push(br(0));
    let entry = guest.load(&program);

    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit");
    assert!(
        matches!(
            exit,
            ExitReason::MemoryFault { address, access: AccessKind::Execute, .. }
                if address == guest.data
        ),
        "a branch into readable-but-not-executable memory must fault, got {exit}"
    );
}

/// **D10's half: Omnidroid takes the fault first.**
///
/// The guest stores into a region that is mapped but lazily committed. Under identity mapping that
/// is a host access violation inside JIT-generated code. Omnidroid's vectored handler runs before
/// dynarmic's frame-based SEH, commits the page and resumes — so the guest never notices, the store
/// lands, and **dynarmic's slow path is never entered**, which is what `slow_path_total == 0` proves
/// here and what the D4 spike reported as `veh_hits = 1`.
///
/// If Omnidroid did *not* take it, the run would still produce the right answer: dynarmic would
/// recompile the block with fastmem off and route the access through a callback. That is the whole
/// hazard — correct results, 13.2x slower — which is why the assertion is on the counter and not on
/// the value.
#[test]
fn the_vectored_handler_takes_a_guest_fault_before_dynarmic_does() {
    if !fault::available() {
        println!(
            "SKIPPED: omni-platform has no vectored-handler implementation on this target, so \
             Omnidroid cannot own guest paging here. See omni-platform::fault."
        );
        return;
    }
    let guest = Guest::new();
    assert!(guest.backend.owns_guest_paging(), "the backend must have installed a pager");

    // Four pages of a lazily-committed region, each touched once.
    const PAGES: usize = 4;
    let page = guest.space.page_size();
    let mut program = mov64(1, 0xFEED_FACE_0000_0001);
    for i in 0..PAGES {
        program.extend(mov64(0, (guest.lazy + i * page) as u64));
        program.push(str_imm(1, 0, 0));
    }
    program.push(ret(30));
    let entry = guest.load(&program);

    let before = guest.backend.pager_stats().expect("pager stats");
    let host_before = fault::stats();

    let (mut cpu, sentinel) = guest.thread();
    cpu.reset_stats();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the stores run");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");

    for i in 0..PAGES {
        assert_eq!(
            guest.read_u64(guest.lazy + i * page),
            0xFEED_FACE_0000_0001,
            "the store to page {i} must have landed"
        );
    }

    let after = guest.backend.pager_stats().expect("pager stats");
    let host_after = fault::stats();
    let resolved = after.resolved - before.resolved;
    assert!(
        resolved >= 1,
        "Omnidroid's pager must have resolved at least one of these faults; it resolved \
         {resolved} and declined {}",
        after.declined - before.declined
    );
    assert!(host_after.resolved > host_before.resolved, "and the vectored handler must have run");
    assert_eq!(
        cpu.stats().slow_path_total,
        0,
        "dynarmic's slow path must never have been entered: if it was, the block was recompiled \
         with fastmem off and every later access to it pays 13.2x"
    );
    println!(
        "demand paging over {PAGES} untouched guest pages (1 run, deterministic counters): \
         {resolved} faults resolved by Omnidroid's VEH, {} bytes committed, {} dynarmic slow-path \
         entries",
        after.bytes_committed - before.bytes_committed,
        cpu.stats().slow_path_total
    );
}

/// The pager must decline a fault that is not its business, or it would be claiming faults from the
/// rest of the process. An address outside the guest space is the clearest case.
#[test]
fn the_pager_declines_faults_outside_its_own_address_space() {
    if !fault::available() {
        println!("SKIPPED: no vectored-handler implementation on this target.");
        return;
    }
    let guest = Guest::new();
    let before = guest.backend.pager_stats().expect("pager stats");

    // A guest access to an address outside the guest space. Identity mapping means this reaches the
    // host address directly, so the fault is real; the pager must not count it as one of its own.
    let outside = 0x10u64; // never mapped in any Windows process
    let mut program = mov64(0, outside);
    program.push(ldr_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load(&program);

    let (mut cpu, _) = guest.thread();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("a fault is an exit");
    assert!(
        matches!(exit, ExitReason::MemoryFault { address, .. } if address as u64 == outside),
        "{exit}"
    );

    let after = guest.backend.pager_stats().expect("pager stats");
    assert_eq!(
        after.examined, before.examined,
        "an address outside the guest space must be declined before the pager even examines it"
    );
    assert_eq!(after.resolved, before.resolved);
}

/// Faults are not one-shot: a context that faulted must still be usable for another run.
#[test]
fn a_context_survives_a_fault_and_runs_again() {
    let guest = Guest::new();
    let mut faulting = mov64(0, guest.unmapped as u64);
    faulting.push(ldr_imm(1, 0, 0));
    faulting.push(ret(30));
    let fault_entry = guest.load(&faulting);

    let (mut cpu, sentinel) = guest.thread();
    for round in 0..8 {
        let exit = cpu.run(fault_entry, RunLimit::Unlimited).expect("round {round}");
        assert!(matches!(exit, ExitReason::MemoryFault { .. }), "round {round}: {exit}");
    }

    // And then a program that does not fault still works, in the same context.
    let good = guest.load_at(1024, &[movz(0, 0x2A, 0), ret(30)]);
    let exit = cpu.run(good, RunLimit::Unlimited).expect("the good program runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), 0x2A);
}
