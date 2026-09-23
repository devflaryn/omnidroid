//! **D4 / D10 on Linux: a guest fault in translated code reaches Omnidroid's pager before it
//! reaches dynarmic.**
//!
//! On Windows this ordering is structural (a vectored handler precedes every frame-based one). On
//! Linux it is not: dynarmic's POSIX build installs its own `SIGSEGV` handler lazily, when the
//! **first jit in the process** is constructed -- after the backend has installed the demand pager,
//! and in front of it -- and for a fault at a RIP inside its code cache that handler takes the
//! fastmem fallback without passing the fault on. That is the 30-49x callback path, taken silently,
//! with correct results. `omni-cpu` re-asserts first place after building each jit
//! (`omni_platform::fault::reassert_precedence`); this binary is the evidence that it works.
//!
//! It must be its **own binary with one test** in it: the moment under test is the first jit of the
//! process, and any earlier test would have taken it.
#![cfg(all(target_os = "linux", target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{AccessKind, ExitReason, GuestCpu, RunLimit};
use omni_platform::fault;

#[test]
fn guest_loads_in_translated_code_are_served_by_the_pager_with_no_slow_path_entry() {
    // The pager is installed here; no jit exists yet in this process.
    let guest = Guest::new();
    assert!(guest.backend.owns_guest_paging(), "the backend must have installed a pager");
    assert!(
        guest.backend.slice_invariant_armed(),
        "D4 amendment 2's per-slice invariant must be armed, or a degraded slice would go unseen"
    );
    let page = guest.space.page_size();
    let granule = guest.space.commit_granule();

    // Loads from three untouched granules of the lazily-committed region: three demand faults, each
    // at a guest `LDR` inside dynarmic's code cache.
    let targets = [guest.lazy + 8, guest.lazy + granule + 16, guest.lazy + 2 * granule + 24];
    let mut program = Vec::new();
    for (index, &target) in targets.iter().enumerate() {
        program.extend(mov64(0, target as u64));
        program.push(ldr_imm(index as u32 + 1, 0, 0));
    }
    program.extend(mov64(5, 0x0D15_EA5E));
    program.push(ret(30));
    let entry = guest.load(&program);

    let pager_before = guest.backend.pager_stats().expect("pager stats");
    let host_before = fault::stats();

    // **The first jit of the process.** Building it is what installs dynarmic's handler.
    let (mut cpu, sentinel) = guest.thread();
    cpu.reset_stats();
    for register in 1..=3 {
        cpu.set_x(x(register), 0xFFFF_FFFF_FFFF_FFFF);
    }
    let exit = cpu
        .run(entry, RunLimit::Unlimited)
        .expect("the loads run; a DegradedMemoryPath error here is the ordering defect");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    for register in 1..=3 {
        assert_eq!(cpu.x(x(register)), 0, "X{register}: a demand-committed page reads zero");
    }
    assert_eq!(cpu.x(x(5)), 0x0D15_EA5E, "the program ran to its end");

    let pager_after = guest.backend.pager_stats().expect("pager stats");
    let host_after = fault::stats();
    let resolved = pager_after.resolved - pager_before.resolved;
    assert!(pager_after.is_consistent(), "{pager_after:?}");
    assert_eq!(
        resolved,
        targets.len() as u64,
        "Omnidroid's pager must have resolved every one of the {} faults; it resolved {resolved} \
         and declined {}",
        targets.len(),
        pager_after.declined - pager_before.declined
    );
    assert_eq!(
        pager_after.bytes_committed - pager_before.bytes_committed,
        (targets.len() * granule) as u64,
        "one commit granule per fault, and no more"
    );
    assert!(host_after.resolved - host_before.resolved >= targets.len() as u64);
    assert_eq!(
        cpu.stats().slow_path_total,
        0,
        "dynarmic's slow path was entered: a fault in translated code reached dynarmic's own \
         SIGSEGV handler before Omnidroid's, and the block was recompiled onto the callback path"
    );
    assert_eq!(cpu.degraded_slices(), 0, "the per-slice invariant fired");

    // A second jit changes nothing: the re-assertion after it finds Omnidroid still on top.
    let (mut second, sentinel) = guest.thread();
    second.reset_stats();
    let mut program = mov64(0, (guest.lazy + 3 * granule) as u64);
    program.push(ldr_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load_at(4 * page, &program);
    let before = guest.backend.pager_stats().expect("pager stats").resolved;
    let exit = second.run(entry, RunLimit::Unlimited).expect("the load runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(guest.backend.pager_stats().expect("pager stats").resolved - before, 1);
    assert_eq!(second.stats().slow_path_total, 0, "the second jit's fault reached dynarmic first");

    // And the chain still works the other way: a fault the pager declines -- an address with nothing
    // mapped -- is passed on to dynarmic, whose fastmem fallback turns it into a typed exit. On
    // Windows that is frame-based dispatch after the VEH declines; here it is the forward.
    let mut program = mov64(0, guest.unmapped as u64);
    program.push(ldr_imm(1, 0, 0));
    program.push(ret(30));
    let entry = guest.load_at(8 * page, &program);
    let declined_before = guest.backend.pager_stats().expect("pager stats").declined;
    let exit = second.run(entry, RunLimit::Unlimited).expect("a fault is an exit, not an error");
    match exit {
        ExitReason::MemoryFault { address, access, .. } => {
            assert_eq!(address, guest.unmapped, "the exit must name the address that faulted");
            assert_eq!(access, AccessKind::Read);
        }
        other => panic!("expected a typed memory fault, got {other}"),
    }
    assert_eq!(
        guest.backend.pager_stats().expect("pager stats").declined - declined_before,
        1,
        "the pager saw the fault first, and declined it"
    );

    println!(
        "Linux pager precedence (n = 1 run, deterministic counters): {resolved} guest faults in \
         translated code resolved by Omnidroid's pager, {} bytes committed, {} dynarmic slow-path \
         entries, {} degraded slices; an unmapped access still became a typed exit",
        pager_after.bytes_committed - pager_before.bytes_committed,
        cpu.stats().slow_path_total,
        cpu.degraded_slices()
    );
}
