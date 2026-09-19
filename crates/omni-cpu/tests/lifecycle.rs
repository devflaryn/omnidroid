//! **Teardown is a correctness problem.** The three defects this file pins all live in the moment a
//! guest thread stops existing, and none of them has a symptom at the place it happens.
//!
//! * **I1** — a `GuestTls` that is dropped without being returned costs the arena one of its fixed
//!   number of blocks. The symptom arrives later and elsewhere: `create_thread_with_tls` failing
//!   with "the TLS arena is full", which names the guest's thread count and not the failed
//!   construction that actually consumed the block.
//! * **I2** — a processor id handed back *before* `od_jit_free` lets another thread build a jit
//!   against the same entry of the shared `ExclusiveMonitor` as a jit that is still alive. Two guest
//!   threads on one entry makes `STXR` succeed where the architecture requires it to fail: a silent
//!   wrong answer in the subsystem D5 lists as risk 3 of 4.
//! * **M2** — four early returns out of `run` skip its `od_jit_clear_halt`, and the whole-branch
//!   review read that as poisoning the context: the next `run` would return having executed nothing
//!   and be reported as "the jit halted with reason 0x… which this backend does not raise and
//!   cannot classify", blaming dynarmic for a bit this backend left behind. **It does not**, and the
//!   test below is what says so rather than an argument. The emitted dispatcher reads *and clears*
//!   `halt_reason` on every return (`block_of_code.cpp:403-405`), so a context always re-enters
//!   `Jit::Run` clean. The property is real and nothing in `omni-cpu` provides it, which is the
//!   reason it is pinned here and in `dynarmic-sys`'s
//!   `a_halt_reason_is_read_and_cleared_by_the_dispatcher`.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::{DynarmicOptions, MemoryPathOverrides};
use omni_cpu::{CpuError, GuestCpu, RunLimit, TLS_BLOCK_BYTES};

/// A loop that loads and stores `iterations` times through the data region.
///
/// The same shape `identity.rs` uses to make the per-slice invariant fire: every access takes the
/// callback path once the fastmem window has been shrunk to 36 bits, so the callback delta for the
/// slice is large and the exit is "the guest returned".
fn memory_loop(data: usize, iterations: u64) -> Vec<u32> {
    let mut program = mov64(0, data as u64);
    program.extend(mov64(1, iterations));
    program.push(movz(2, 0, 0));
    let loop_start = program.len();
    program.push(ldr_imm(3, 0, 0));
    program.push(add_reg(2, 2, 3));
    program.push(str_imm(2, 0, 8));
    program.push(subs_imm(1, 1, 1));
    let here = program.len();
    program.push(b_cond(1, loop_start as i32 - here as i32)); // B.NE loop
    program.push(ret(30));
    program
}

/// **I2.** Over ordinary churn, and over the construction failures that release an id without ever
/// having built a jit, no id is ever recycled while a jit still holds its monitor entry.
///
/// The witness is inside `Shared::release_processor_id`, because the ordering has no other symptom:
/// the wrong order produces correct-looking results and a monitor that is quietly shared. Nothing
/// here can observe a `STXR` going wrong, so what is observed is the ordering itself.
#[test]
fn a_processor_id_is_never_handed_back_while_a_jit_still_holds_its_monitor_entry() {
    const ROUNDS: usize = 64;

    let guest = Guest::new();

    // Ordinary create-and-drop churn, which is what a runtime does per guest thread.
    for _ in 0..ROUNDS {
        let (cpu, _sentinel) = guest.thread();
        drop(cpu);
    }
    assert_eq!(
        guest.backend.processor_ids_released_early(),
        0,
        "n = {ROUNDS} ordinary create/drop rounds"
    );

    // And the two construction-failure exits, which give an id back without a jit ever existing.
    // `create_misconfigured_thread` with a conforming override is refused *after* the id is taken,
    // which is exactly that path.
    for _ in 0..ROUNDS {
        let refused = guest.backend.create_misconfigured_thread(MemoryPathOverrides::default());
        assert!(
            refused.is_err(),
            "a conforming override has nothing for the startup assertion to refuse"
        );
    }
    assert_eq!(
        guest.backend.processor_ids_released_early(),
        0,
        "n = {ROUNDS} refused constructions"
    );

    // The ids really are coming back: a backend sized for 32 threads has served 128 of them.
    let (cpu, _sentinel) = guest.thread();
    assert_eq!(guest.backend.processor_ids_released_early(), 0);
    drop(cpu);
}

/// **I1.** A construction that fails after the TLS block has been handed out must not consume it.
///
/// The arena is sized at backend creation, so a leak is not a slow drift — it is `max_threads`
/// failures away from refusing every further guest thread. This drives the failure `max_threads`
/// times *and then* asks for a real thread, which is the assertion that would have failed.
#[test]
fn a_failed_thread_construction_does_not_consume_a_tls_block() {
    let options = DynarmicOptions { max_threads: 4, ..Default::default() };
    let guest = Guest::with_options(options);
    let capacity = guest.backend.tls().capacity();
    assert_eq!(capacity, 4, "the arena is sized from max_threads");

    // Far more failures than the arena has blocks. Every one of these takes a block and a processor
    // id and then refuses.
    for round in 0..capacity * 8 {
        let refused = guest.backend.create_misconfigured_thread(MemoryPathOverrides::default());
        assert!(refused.is_err(), "round {round} was supposed to be refused");
    }

    // The arena must still be able to fill itself completely.
    let mut threads = Vec::new();
    for i in 0..capacity {
        threads.push(guest.backend.create_thread_with_tls().unwrap_or_else(|e| {
            panic!(
                "guest thread {i} of {capacity} was refused after {} failed constructions: {e}. \
                 Each failure used to keep its TLS block, so the arena ran out and said the \
                 thread count was the problem",
                capacity * 8
            )
        }));
    }
    assert_eq!(threads.len(), capacity);

    // Commit charge is per block and is not taken twice for a reused one (D10).
    assert_eq!(
        guest.backend.tls().committed(),
        (capacity * TLS_BLOCK_BYTES) as u64,
        "n = {capacity} blocks, one page each, and reuse must not charge for them again"
    );
    drop(threads);
}

/// **M2.** An early return out of `run` must not poison the context for the next `run`.
///
/// `DegradedMemoryPath` is the reachable one of the four exits — the two shim failures declare the
/// jit uncharacterised and a callback panic needs a defect to provoke — and it is representative,
/// because all four skip the same `od_jit_clear_halt`.
///
/// **This passes today and passed before the fix round, which is the finding.** The review's M2 said
/// the halt bit survives the early return; the emitted dispatcher clears it on the way out of every
/// `Run`, so it does not. An entry clear was written, measured against exactly this test, found to
/// change nothing, and removed — it is a lock-prefixed RMW on the per-guest-call path, and the
/// boundary M3 budgets against is about 33 ns in total for an inline thunk (`tests/thunk.rs`, which
/// replaced D5 amendment 2's "under 53 ns" with a measured round trip), and a lock-prefixed RMW is a
/// material fraction of that.
///
/// So this is a *characterisation* test, not a regression test for a fix: it pins a property of the
/// pin that `omni-cpu`'s error handling silently depends on. If dynarmic ever stops reading and
/// clearing, the second run here returns "halted with reason … which this backend does not raise
/// and cannot classify" and the review's M2 becomes true.
#[test]
fn an_early_return_does_not_poison_the_context_for_the_next_run() {
    let guest = Guest::new();
    guest.assert_high_addresses();

    const ITERATIONS: u64 = 1_000;
    let entry = guest.load(&memory_loop(guest.data, ITERATIONS));
    guest.write_u64(guest.data, 3);
    let sentinel = guest.code + harness::CODE_BYTES - 4;

    let (mut cpu, _refusal) = guest
        .backend
        .create_misconfigured_thread(MemoryPathOverrides {
            address_space_bits: Some(36),
            ..Default::default()
        })
        .expect("a deliberately misconfigured context");
    cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
    cpu.set_x(x(30), sentinel as u64);

    // The first run ends in an early return, before `od_jit_clear_halt`.
    match cpu.run(entry, RunLimit::Unlimited) {
        Err(CpuError::DegradedMemoryPath { .. }) => {}
        other => panic!("expected the per-slice invariant to fire, got {other:?}"),
    }

    // The second run must reach guest code. Under the defect it returned immediately with the halt
    // bit the first run left set, and the run loop classified that as dynarmic's fault.
    guest.write_u64(guest.data, 3);
    cpu.set_x(x(30), sentinel as u64);
    let second = cpu.run(entry, RunLimit::Unlimited);
    match second {
        // The same violation again is the correct answer: the context is still misconfigured.
        Err(CpuError::DegradedMemoryPath { callbacks, .. }) => {
            assert!(
                callbacks > 0,
                "the second run must have executed guest code, not returned on a stale halt bit"
            );
        }
        Err(CpuError::Backend { detail, .. }) if detail.contains("cannot classify") => panic!(
            "the second run was refused with a halt reason the FIRST run left behind: {detail}. \
             The context was poisoned by this backend and the message blames dynarmic"
        ),
        other => panic!("expected the invariant to fire again, got {other:?}"),
    }
    assert_eq!(
        cpu.degraded_slices(),
        2,
        "both runs must have reached guest code and both must have been caught"
    );
}
