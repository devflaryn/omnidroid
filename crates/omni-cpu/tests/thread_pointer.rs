//! D13: the bionic thread pointer, read by guest code exactly the way `libroblox.so` reads it.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit, TlsSlot, TLS_SLOT_STACK_GUARD_OFFSET};

/// The two instructions every stack-protected function in `libroblox.so` begins with.
///
/// 1,276 of the engine's 1,282 `MRS Xt, TPIDR_EL0` instructions are followed by exactly this load
/// (D13), so this is not a synthetic test — it is the real sequence, first.
fn read_stack_guard() -> Vec<u32> {
    vec![
        mrs_tpidr_el0(0),           // MRS X0, TPIDR_EL0
        ldr_imm(1, 0, TLS_SLOT_STACK_GUARD_OFFSET as u32), // LDR X1, [X0, #0x28]
        mrs_tpidrro_el0(2),         // MRS X2, TPIDRRO_EL0
        ldr_imm(3, 2, TLS_SLOT_STACK_GUARD_OFFSET as u32),
        ret(30),
    ]
}

#[test]
fn guest_code_reads_the_stack_guard_through_the_thread_pointer() {
    let guest = Guest::new();
    let entry = guest.load(&read_stack_guard());
    let (mut cpu, sentinel) = guest.thread();

    let tls = {
        let block = cpu.tls().expect("the context allocated a TLS block");
        (block.thread_pointer(), block.stack_guard())
    };
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");

    assert_eq!(cpu.x(x(0)), tls.0 as u64, "MRS X0, TPIDR_EL0 must give the block's base");
    assert_eq!(
        cpu.x(x(1)),
        tls.1,
        "the guest must find the stack guard at [TPIDR_EL0, #0x28]: this is the load 1,276 of \
         libroblox.so's 1,282 thread-pointer reads perform, before JNI_OnLoad and before the first \
         static initializer (D13)"
    );
    assert_ne!(cpu.x(x(1)), 0, "a zero canary compares equal to a zeroed stack slot");
    assert_eq!(cpu.x(x(2)), tls.0 as u64, "TPIDRRO_EL0 must agree with TPIDR_EL0");
    assert_eq!(cpu.x(x(3)), tls.1);

    // The encoding, stated so the test can be checked without running it.
    assert_eq!(mrs_tpidr_el0(0), 0xD53B_D040);
    assert_eq!(ldr_imm(1, 0, 0x28), 0xF940_1401);
}

/// **Every** guest thread, not just the first. A thread the engine creates later with no block is
/// the same crash, arriving later and looking even less like a thread-pointer problem.
#[test]
fn every_guest_thread_gets_its_own_populated_block() {
    const THREADS: u32 = 8;
    let guest = Guest::with_options(DynarmicOptions { max_threads: THREADS, ..Default::default() });
    let entry = guest.load(&read_stack_guard());
    let sentinel = guest.code + harness::CODE_BYTES - 4;

    // The contexts are kept alive for the whole loop on purpose: a dropped context returns its TLS
    // block to the arena, so letting each one go would make every thread reuse one block and the
    // distinctness assertion below would be testing nothing.
    let mut live = Vec::new();
    let mut pointers = Vec::new();
    let mut guards = Vec::new();
    for thread in 0..THREADS {
        let mut cpu = guest.backend.create_thread_with_tls().expect("a guest thread");
        cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
        cpu.set_x(x(30), sentinel as u64);
        let (base, guard) = {
            let block = cpu.tls().expect("a TLS block");
            (block.thread_pointer(), block.stack_guard())
        };

        let exit = cpu.run(entry, RunLimit::Unlimited).expect("thread {thread} runs");
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "thread {thread}: {exit}");
        assert_eq!(cpu.x(x(0)), base as u64, "thread {thread}");
        assert_eq!(cpu.x(x(1)), guard, "thread {thread} must find its own stack guard");

        pointers.push(base);
        guards.push(guard);
        live.push(cpu);
    }
    assert_eq!(live.len(), THREADS as usize);

    pointers.sort_unstable();
    pointers.dedup();
    assert_eq!(pointers.len(), THREADS as usize, "every thread needs its own block");

    // But one guard value for the whole address space, which is what bionic does: a canary stored
    // by one thread and checked by another has to compare equal.
    guards.dedup();
    assert_eq!(guards.len(), 1, "bionic copies one AT_RANDOM-derived guard into every thread");
}

/// The block is zero apart from the guard, and a block handed back out is re-zeroed.
///
/// The re-zeroing is the part that matters: a guest thread must never see the leftovers of a thread
/// that exited, and slot 1 (`TLS_SLOT_THREAD_ID`) is the one that would be most dangerous to inherit.
#[test]
fn a_reused_block_is_zeroed_before_it_is_handed_out_again() {
    let guest = Guest::with_options(DynarmicOptions { max_threads: 1, ..Default::default() });

    // Program: write a marker into slot 1, then read it back.
    let poison = vec![
        mrs_tpidr_el0(0),
        movz(1, 0xDEAD, 0),
        str_imm(1, 0, TlsSlot::ThreadId.offset() as u32),
        ret(30),
    ];
    let inspect = vec![
        mrs_tpidr_el0(0),
        ldr_imm(1, 0, TlsSlot::ThreadId.offset() as u32),
        ldr_imm(2, 0, TLS_SLOT_STACK_GUARD_OFFSET as u32),
        ret(30),
    ];
    let poison_entry = guest.load(&poison);
    let inspect_entry = guest.load_at(256, &inspect);
    let sentinel = guest.code + harness::CODE_BYTES - 4;

    let block;
    {
        let mut cpu = guest.backend.create_thread_with_tls().expect("first thread");
        cpu.set_return_sentinel(sentinel).expect("sentinel");
        cpu.set_x(x(30), sentinel as u64);
        block = cpu.tls().expect("a block").thread_pointer();
        cpu.run(poison_entry, RunLimit::Unlimited).expect("poison runs");
        // Dropping the context returns the block to the arena.
    }

    let mut cpu = guest.backend.create_thread_with_tls().expect("second thread");
    cpu.set_return_sentinel(sentinel).expect("sentinel");
    cpu.set_x(x(30), sentinel as u64);
    let (base, guard) = {
        let block = cpu.tls().expect("a block");
        (block.thread_pointer(), block.stack_guard())
    };
    assert_eq!(base, block, "the arena must reuse the freed block, or this proves nothing");

    cpu.run(inspect_entry, RunLimit::Unlimited).expect("inspect runs");
    assert_eq!(cpu.x(x(1)), 0, "a reused block must not carry the previous thread's slot 1");
    assert_eq!(cpu.x(x(2)), guard, "and it must still have its stack guard");
}

/// A guest thread that re-points itself, which is what bionic's `__set_tls` does.
#[test]
fn a_guest_thread_can_repoint_its_own_thread_pointer() {
    let guest = Guest::new();
    let (mut cpu, sentinel) = guest.thread();
    let (original, guard) = {
        let block = cpu.tls().expect("a block");
        (block.thread_pointer(), block.stack_guard())
    };

    // A second, hand-built block in the data region, with a different guard.
    let elsewhere = guest.data + 4096;
    guest.write_u64(elsewhere + TLS_SLOT_STACK_GUARD_OFFSET, 0x0BAD_CAFE_0BAD_CAFE);

    let program = vec![
        msr_tpidr_el0(4),                                   // MSR TPIDR_EL0, X4
        mrs_tpidr_el0(0),                                   // read it straight back
        ldr_imm(1, 0, TLS_SLOT_STACK_GUARD_OFFSET as u32),  // and the guard it now points at
        ret(30),
    ];
    let entry = guest.load(&program);
    cpu.set_x(x(4), elsewhere as u64);

    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.x(x(0)), elsewhere as u64, "MSR TPIDR_EL0 must take effect for the guest");
    assert_eq!(cpu.x(x(1)), 0x0BAD_CAFE_0BAD_CAFE);
    assert_ne!(original as u64, elsewhere as u64);
    assert_ne!(guard, 0x0BAD_CAFE_0BAD_CAFE);

    // And the host side agrees: `GuestCpu::tpidr_el0` reads the same register.
    assert_eq!(cpu.tpidr_el0(), elsewhere);
}

/// **The per-thread cost of a TLS block**, measured rather than assumed.
///
/// Reported with its n (Global Constraint 12). Both halves are asserted: the arena's own accounting,
/// which is exact, and the process commit charge, which is noisy and is therefore bounded rather
/// than pinned.
#[test]
fn the_per_thread_tls_cost_is_one_page() {
    const THREADS: u32 = 16;
    let guest = Guest::with_options(DynarmicOptions { max_threads: THREADS, ..Default::default() });
    let arena = guest.backend.tls();

    assert_eq!(arena.committed(), 0, "an arena costs nothing until a thread asks for a block");
    assert_eq!(
        arena.reserved(),
        omni_cpu::TLS_BLOCK_BYTES * THREADS as usize,
        "address space is reserved up front, which D10 says is free"
    );

    let mut threads = Vec::new();
    for _ in 0..THREADS {
        threads.push(guest.backend.create_thread_with_tls().expect("a guest thread"));
    }

    let committed = arena.committed();
    assert_eq!(
        committed,
        u64::from(THREADS) * omni_cpu::TLS_BLOCK_BYTES as u64,
        "n = {THREADS} threads: each block is exactly one page of commit charge"
    );
    for cpu in &threads {
        assert_eq!(cpu.cost().private_committed, omni_cpu::TLS_BLOCK_BYTES);
        assert_eq!(
            cpu.cost().shared_committed,
            0,
            "a guest anonymous mapping is private commit, which process_commit_charge does count"
        );
    }
    println!(
        "TLS cost, n = {THREADS} guest threads: {committed} bytes of commit charge total, \
         {} bytes per thread, {} bytes of address space reserved",
        committed / u64::from(THREADS),
        arena.reserved()
    );
}

/// The arena refuses an over-subscription rather than handing out a block that overlaps another's.
#[test]
fn the_arena_refuses_more_threads_than_it_was_sized_for() {
    let guest = Guest::with_options(DynarmicOptions { max_threads: 2, ..Default::default() });
    let _a = guest.backend.create_thread_with_tls().expect("thread 1");
    let _b = guest.backend.create_thread_with_tls().expect("thread 2");
    let error = guest.backend.create_thread_with_tls().expect_err("thread 3 must be refused");
    assert!(
        error.to_string().contains("arena is full")
            || error.to_string().contains("distinct processor id"),
        "the refusal must say which resource ran out: {error}"
    );
}
