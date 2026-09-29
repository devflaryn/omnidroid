//! Guest code on a host page split into 4 KiB parts (spec 2026-09-29-4k-guest-pages): each part's
//! protection holds for loads, stores, exclusives and instruction fetch, a served access is not a
//! degraded slice, and where the host page is 4 KiB the same programs run on the direct path.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::{DynarmicCpu, DynarmicOptions};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};
use omni_mem::{CommitPolicy, Placement, Protection, GUEST_PAGE};

const NE: u32 = 1;

/// The Linux personality's options: a fault the guest means is served once, not recompiled.
fn linux_options() -> DynarmicOptions {
    DynarmicOptions { recompile_on_declined_fault: false, max_threads: 8, ..DynarmicOptions::default() }
}

/// A guest with 4 KiB pages, and one committed read-write host page in its space.
fn guest_and_page() -> (Guest, usize) {
    let g = Guest::with_space_config(linux_options(), Some(GUEST_PAGE));
    let page = g.space.page_size();
    let at = g
        .space
        .map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a page");
    (g, at)
}

fn run_with(g: &Guest, program: &[u32], setup: impl FnOnce(&mut DynarmicCpu)) -> (ExitReason, DynarmicCpu) {
    let entry = g.load(program);
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(30), sentinel as u64);
    setup(&mut cpu);
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("no degraded slice");
    (exit, cpu)
}

/// `str x1, [x0]; ret`
fn store(g: &Guest, at: usize, value: u64) -> ExitReason {
    run_with(g, &[str_imm(1, 0, 0), ret(30)], |cpu| {
        cpu.set_x(x(0), at as u64);
        cpu.set_x(x(1), value);
    })
    .0
}

/// `ldr x2, [x0]; ret`
fn load(g: &Guest, at: usize) -> (ExitReason, u64) {
    let (exit, cpu) = run_with(g, &[ldr_imm(2, 0, 0), ret(30)], |cpu| cpu.set_x(x(0), at as u64));
    (exit, cpu.x(x(2)))
}

fn returned(exit: &ExitReason) -> bool {
    matches!(exit, ExitReason::Returned { .. })
}

fn faulted_at(exit: &ExitReason, at: usize) -> bool {
    matches!(exit, ExitReason::MemoryFault { address, .. } if *address == at)
}

#[test]
fn two_4_kib_pages_in_one_host_page_keep_their_own_protection() {
    let (g, at) = guest_and_page();
    g.space.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let rw = at + GUEST_PAGE;
    assert!(returned(&store(&g, rw, 0xAB)), "the read-write page takes the store");
    let refused = store(&g, at, 0xCD);
    assert!(faulted_at(&refused, at), "the read-only page refuses it: {refused}");
    let (exit, v) = load(&g, rw);
    assert!(returned(&exit), "{exit}");
    assert_eq!(v, 0xAB);
    let (exit, v) = load(&g, at);
    assert!(returned(&exit), "the read-only page is readable: {exit}");
    assert_eq!(v, 0);
}

#[test]
fn a_prot_none_4_kib_page_traps_and_its_neighbours_do_not() {
    let (g, at) = guest_and_page();
    let page = g.space.page_size();
    let none = at + GUEST_PAGE.min(page - GUEST_PAGE);
    g.space.protect(none, GUEST_PAGE, Protection::None).unwrap();
    let (exit, _) = load(&g, none);
    assert!(faulted_at(&exit, none), "{exit}");
    for n in [at, at + page - 8] {
        if n >= none && n < none + GUEST_PAGE {
            continue;
        }
        let (exit, _) = load(&g, n);
        assert!(returned(&exit), "load {n:#x}: {exit}");
        assert!(returned(&store(&g, n, 1)), "store {n:#x}");
    }
    // A 16-byte access straddling into the PROT_NONE part faults.
    let (exit, _) = run_with(&g, &[ldr_q(0, 0, 0), ret(30)], |cpu| cpu.set_x(x(0), (none - 8) as u64));
    assert!(matches!(exit, ExitReason::MemoryFault { .. }), "{exit}");
}

#[test]
fn a_served_access_is_not_a_degraded_slice() {
    let (g, at) = guest_and_page();
    g.space.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    // str x1, [x0]; subs x3, x3, #1; b.ne -2; ret -- a thousand stores to the read-write part.
    let (exit, cpu) = run_with(&g, &[str_imm(1, 0, 0), subs_imm(3, 3, 1), b_cond(NE, -2), ret(30)], |cpu| {
        cpu.set_x(x(0), (at + GUEST_PAGE) as u64);
        cpu.set_x(x(1), 7);
        cpu.set_x(x(3), 1000);
    });
    assert!(returned(&exit), "{exit}");
    assert_eq!(g.read_u64(at + GUEST_PAGE), 7);
    if g.space.subpages_active() {
        assert!(cpu.split_served() >= 1, "served through the alias");
    } else {
        assert_eq!(cpu.split_served(), 0, "a 4 KiB host never serves");
    }
}

#[test]
fn code_in_an_executable_part_beside_a_prot_none_part_runs() {
    let (g, at) = guest_and_page();
    let page = g.space.page_size();
    for (i, w) in [movz(0, 42, 0), ret(30)].iter().enumerate() {
        g.space.write_forced(at + 4 * i, &w.to_le_bytes()).unwrap();
    }
    g.space.protect(at, GUEST_PAGE, Protection::ReadExecute).unwrap();
    g.space.protect(at + page - GUEST_PAGE, GUEST_PAGE, Protection::None).unwrap();
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(30), sentinel as u64);
    let exit = cpu.run(at, RunLimit::Unlimited).expect("no degraded slice");
    assert!(returned(&exit), "{exit}");
    assert_eq!(cpu.x(x(0)), 42);
}

#[test]
fn code_in_a_non_executable_part_does_not_run() {
    let (g, at) = guest_and_page();
    g.space.write_forced(at + GUEST_PAGE, &ret(30).to_le_bytes()).unwrap();
    g.space.protect(at, GUEST_PAGE, Protection::ReadExecute).unwrap(); // the neighbour is executable
    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(30), sentinel as u64);
    let exit = cpu.run(at + GUEST_PAGE, RunLimit::Unlimited).expect("a typed exit");
    assert!(!returned(&exit), "a read-write part is not executable: {exit}");
}

#[test]
fn four_threads_increment_a_split_page_counter() {
    let (g, at) = guest_and_page();
    g.space.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let counter = at + GUEST_PAGE;
    // loop: ldaxr x2,[x0]; add x2,x2,#1; stlxr w4,x2,[x0]; cbnz w4,loop; subs x3,x3,#1; b.ne loop; ret
    let entry = g.load(&[ldaxr(2, 0), add_imm(2, 2, 1), stlxr(4, 2, 0), cbnz_w(4, -3), subs_imm(3, 3, 1), b_cond(NE, -5), ret(30)]);
    let sentinel = g.code + harness::CODE_BYTES - 4;
    let mut cpus: Vec<DynarmicCpu> = (0..4)
        .map(|_| {
            let (mut cpu, _) = g.thread();
            cpu.set_x(x(0), counter as u64);
            cpu.set_x(x(3), 500);
            cpu.set_x(x(30), sentinel as u64);
            cpu
        })
        .collect();
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for cpu in &mut cpus {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                let exit = cpu.run(entry, RunLimit::Unlimited).expect("no degraded slice");
                assert!(matches!(exit, ExitReason::Returned { .. }), "{exit}");
            });
        }
    });
    assert_eq!(g.read_u64(counter), 2000, "no lost update");
}
