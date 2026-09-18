//! Guest faults: a typed exit instead of a host crash, and Omnidroid owning guest paging (D10).

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{AccessKind, ExitReason, GuestCpu, RunLimit};
use omni_platform::fault;

/// Serializes the tests that read **process-wide** fault counters.
///
/// `fault::stats()` counts every access violation in the process, so two of these running at once
/// move each other's numbers — which is exactly what happened the first time the concurrency test
/// below was added, and is the same process-global-counter trap Task 1 spent three review rounds on.
/// The per-pager counters in `PagerStats` are per address space and need no lock; only the host-wide
/// ones do.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
/// hazard — correct results, 30-49x slower — which is why the assertion is on the counter and not on
/// the value.
#[test]
fn the_vectored_handler_takes_a_guest_fault_before_dynarmic_does() {
    let _serial = serialized();
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
    // `PagerStats`'s invariant, checked where it is cheapest to check: every guest thread has
    // been joined, so nothing can be mid-handler and the counters are quiescent.
    assert!(after.is_consistent(), "examined == resolved + declined; got {after:?}");
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
        after.bytes_committed - before.bytes_committed,
        guest.space.commit_granule() as u64,
        "exactly one commit granule, and no more. The four pages share one 64 KiB granule, \
         so one fault serves all four -- which is D10's measured granule choice working. \
         A pager that committed the whole mapping instead would also pass every \
         functional assertion here, and would give back D10's whole reason for lazy \
         commit"
    );
    assert_eq!(
        cpu.stats().slow_path_total,
        0,
        "dynarmic's slow path must never have been entered: if it was, the block was recompiled \
         with fastmem off and every later access to it pays 30-49x"
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
    let _serial = serialized();
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
    // `PagerStats`'s invariant, checked where it is cheapest to check: every guest thread has
    // been joined, so nothing can be mid-handler and the counters are quiescent.
    assert!(after.is_consistent(), "examined == resolved + declined; got {after:?}");
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

/// **Several guest threads faulting at once.**
///
/// The vectored handler is process-wide and the pager takes the guest space's lock inside it, so the
/// interesting case is not one fault but many, arriving on different threads at the same instant.
/// Each guest thread here writes to its own page of a lazily-committed region, all starting
/// together, so the faults genuinely overlap rather than queueing behind one another.
///
/// What this pins: no deadlock (the pager's lock is never held by a thread that is about to fault),
/// no lost commit (every store lands), no double-service (the pager's counters add up), and
/// dynarmic's slow path still never entered on any thread.
#[test]
fn several_guest_threads_can_fault_at_the_same_time() {
    let _serial = serialized();
    if !fault::available() {
        println!("SKIPPED: no vectored-handler implementation on this target.");
        return;
    }
    const THREADS: usize = 8;
    const PAGES_EACH: usize = 4;

    let guest = Guest::with_options(omni_cpu::dynarmic::DynarmicOptions {
        max_threads: THREADS as u32,
        ..Default::default()
    });
    let page = guest.space.page_size();
    assert!(
        harness::LAZY_BYTES >= THREADS * PAGES_EACH * page,
        "the lazy region has to be big enough for every thread to get its own pages"
    );

    // One program per thread, each writing its own marker into its own pages.
    let sentinel = guest.code + harness::CODE_BYTES - 4;
    let mut entries = Vec::new();
    for thread in 0..THREADS {
        let marker = 0xC0DE_0000_0000_0000u64 | thread as u64;
        let mut program = mov64(1, marker);
        for slot in 0..PAGES_EACH {
            let address = guest.lazy + (thread * PAGES_EACH + slot) * page;
            program.extend(mov64(0, address as u64));
            program.push(str_imm(1, 0, 0));
        }
        program.push(ret(30));
        // Programs are laid out back to back; 512 bytes each is ample for 4 pages of stores.
        entries.push((guest.load_at(thread * 512, &program), marker));
    }

    let before = guest.backend.pager_stats().expect("pager stats");

    // Contexts are `Send`, so each moves to its own thread. A barrier makes them start together.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let mut running = Vec::new();
    for (entry, marker) in entries {
        let mut cpu = guest.backend.create_thread_with_tls().expect("a guest thread");
        cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
        cpu.set_x(x(30), sentinel as u64);
        let barrier = std::sync::Arc::clone(&barrier);
        running.push(std::thread::spawn(move || {
            barrier.wait();
            let exit = cpu.run(entry, RunLimit::Unlimited).expect("the stores run");
            (exit, cpu.stats().slow_path_total, marker)
        }));
    }

    for (index, handle) in running.into_iter().enumerate() {
        let (exit, slow_path, marker) = handle.join().expect("a guest thread finished");
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "thread {index}: {exit}");
        assert_eq!(slow_path, 0, "thread {index} took dynarmic's slow path {slow_path} times");
        for slot in 0..PAGES_EACH {
            let address = guest.lazy + (index * PAGES_EACH + slot) * page;
            assert_eq!(
                guest.read_u64(address),
                marker,
                "thread {index}'s store to page {slot} did not land"
            );
        }
    }

    let after = guest.backend.pager_stats().expect("pager stats");
    // `PagerStats`'s invariant, checked where it is cheapest to check: every guest thread has
    // been joined, so nothing can be mid-handler and the counters are quiescent.
    assert!(after.is_consistent(), "examined == resolved + declined; got {after:?}");
    let resolved = after.resolved - before.resolved;
    let examined = after.examined - before.examined;
    assert!(resolved >= 1, "the pager must have served these faults; it resolved {resolved}");
    assert_eq!(
        examined,
        resolved + (after.declined - before.declined),
        "every fault the pager examined must be accounted for as resolved or declined"
    );
    println!(
        "concurrent demand paging, n = {THREADS} guest threads x {PAGES_EACH} pages (1 run): \
         {examined} faults examined, {resolved} resolved, {} bytes committed, 0 dynarmic \
         slow-path entries on every thread",
        after.bytes_committed - before.bytes_committed
    );
}

/// **M5: the instruction-fetch cache may not short-circuit a commit that has not happened.**
///
/// `CpuCtx::fetch` caches the region a fetch resolved to, so translating a run of instructions in one
/// function does not take the space's lock once per instruction. Skipping `resolve` also skips
/// `ensure_committed`, and that is only harmless while there is nothing left to commit. For a
/// **lazily-committed anonymous executable** region larger than the commit granule it is not: the
/// fetch reads the instruction from Rust, at an address the region map says is mapped and the OS says
/// is not committed.
///
/// What made it a real hazard rather than a slow path is *who catches that*. It is a fault inside
/// Rust, not inside generated code, so dynarmic's frame-based handler — which only covers its own
/// code cache — is not in the picture at all. The only thing that can resolve it is Omnidroid's
/// vectored handler, and `owns_guest_paging()` is allowed to be `false` (Linux, macOS, or a full
/// handler table). On such a platform this is a crash rather than a commit.
///
/// The third element of the cached tuple was written `true` and never read, which is how it survived
/// two reviews.
///
/// The measurement is the pager's own counter: with the cache gated on commitment, the fetch commits
/// the granule itself and **no fault happens at all**. With the gate missing, the granule is
/// committed by the fault handler instead, and `examined` moves. Both produce the same guest-visible
/// exit, which is exactly why a test that only looked at the exit could not see this.
#[test]
fn an_instruction_fetch_commits_its_own_granule_rather_than_faulting_for_it() {
    use omni_mem::{CommitPolicy, Placement, Protection};

    let guest = Guest::new();
    let granule = guest.space.commit_granule();

    // Four granules, lazily committed, so the region is never "fully committed" and the fetch cache
    // must keep asking.
    let code = guest
        .space
        .map_anonymous(
            Placement::Anywhere { align: guest.space.page_size() },
            4 * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazily-committed region to put code in");

    // The first granule is committed because the program is written into it. The third is not, and
    // that is where the guest branches.
    let target = code + 2 * granule;
    let hop = i32::try_from((target - code) / 4).expect("a branch offset in range");
    guest.space.ensure_committed(code, 4).expect("commit the first granule");
    let ptr = guest.space.ptr(code, 4).expect("a host pointer for the first instruction");
    // SAFETY: the range was just committed `ReadWrite` and identity mapping (D4) makes the guest
    // address a host address. No guest thread is running.
    unsafe { ptr.cast::<u32>().write_unaligned(b(hop)) };
    guest
        .space
        .protect(code, 4 * granule, Protection::ReadExecute)
        .expect("a lazy mapping records a protection for granules it has not committed yet");

    let region = guest.space.region_at(target).expect("the region covers the branch target");
    assert!(
        !region.is_committed(),
        "the branch target's region must still be partly uncommitted, or there is nothing for the \
         fetch to commit and this test measures nothing"
    );

    let before = guest.backend.pager_stats().expect("this backend owns guest paging");
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(30), sentinel as u64);
    let exit = cpu.run(code, RunLimit::Unlimited).expect("a branch into zeroed memory is an exit");
    let after = guest.backend.pager_stats().expect("this backend owns guest paging");

    // Zeroed memory is `UDF #0`, which is the honest outcome: the guest branched somewhere with no
    // code in it.
    match exit {
        ExitReason::UnsupportedInstruction { pc, encoding } => {
            assert_eq!(pc, target, "the exit must name the address the guest branched to");
            assert_eq!(encoding, 0, "a freshly committed granule reads back as zeroes");
        }
        other => panic!("expected the zeroed granule to decode as an unallocated encoding: {other}"),
    }

    assert_eq!(
        after.examined - before.examined,
        0,
        "the fetch took {} page fault(s) to read an instruction from a granule it could have \
         committed itself. That works here only because this backend owns guest paging; where it \
         does not, the same read is an unhandled access violation inside Rust, which dynarmic's \
         frame-based handler does not cover",
        after.examined - before.examined
    );
    assert!(
        after.is_consistent(),
        "PagerStats invariant: examined == resolved + declined. Got {after:?}"
    );
    assert!(
        guest.space.region_at(target).is_some_and(|r| r.committed >= granule),
        "and the granule really was committed, by the fetch"
    );
}
