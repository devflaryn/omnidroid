//! **The import census, read back against calls whose number and order are known.**
//!
//! The census is a report: `Boundary::census` (calls per symbol), `Boundary::last_call` and
//! `Boundary::last_caller` (the latest crossing by any thread), and `Boundary::threads` (each
//! thread's). Every count is kept in the crossing thread's own record so that a crossing writes
//! nothing another core writes (`Boundary::start_census` has the measurement), and the reports
//! gather the records when asked. What these tests pin is that the gathering loses nothing:
//! calls on several threads -- finished ones included -- sum to the total, the latest crossing is
//! found whichever thread made it and whenever that thread first crossed, and a thread that
//! crosses two boundaries is counted by each for its own calls only.
//!
//! ```text
//! cargo test -p omni-android --release --test census
//! ```

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::collections::BTreeMap;
use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, x, Asm, Guest, BUDGET};
use omni_android::{AbiResult, Boundary, ImportCall, ReentrantCall};
use omni_cpu::dynarmic::DynarmicCpu;
use omni_cpu::{ExitReason, GuestAddr, GuestCpu};

fn noop(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    call.ret().u64(0);
    Ok(())
}

/// The same on the exit path, which charges the census from `service_exit` rather than from the
/// inline dispatch.
fn noop_on_the_exit_path(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    call.ret(|mut ret| ret.u64(0));
    Ok(())
}

/// A program that calls `thunk` `X1` times, and the address each of those calls returns to --
/// which is what `last_caller` reports.
fn looping(guest: &Guest, thunk: GuestAddr) -> (GuestAddr, GuestAddr) {
    let at = guest.next_entry();
    let mut asm = Asm::at(at);
    asm.push(mov_reg(20, 30));
    let top = asm.pc();
    asm.bl(thunk);
    let site = asm.pc();
    asm.push(subs_imm(1, 1, 1));
    let here = asm.pc();
    asm.push(b_cond(1, (top as i64 - here as i64) as i32 / 4));
    asm.push(mov_reg(30, 20));
    asm.push(ret(30));
    let entry = guest.load(asm.words());
    assert_eq!(entry, at);
    (entry, site)
}

/// Run `entry` on `cpu`, `times` calls, to its return.
fn run(boundary: &Arc<Boundary>, stack: GuestAddr, cpu: &mut DynarmicCpu, entry: GuestAddr, times: u64) {
    cpu.set_sp(stack);
    cpu.set_x(x(30), boundary.sentinel() as u64);
    cpu.set_x(x(1), times);
    let exit = boundary.run(cpu, entry, BUDGET).expect("the loop runs");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
}

#[test]
fn the_census_sums_every_threads_calls_per_symbol_and_keeps_a_finished_threads() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let a = builder.bind_inline("a", noop).expect("bind");
    let b = builder.bind_inline("b", noop).expect("bind");
    let r = builder.bind_reentrant("r", noop_on_the_exit_path).expect("bind");
    let boundary = builder.finish();
    let (loop_a, _) = looping(&guest, a);
    let (loop_b, _) = looping(&guest, b);
    let (loop_r, _) = looping(&guest, r);
    let stack = guest.stack_top;

    assert_eq!(boundary.census(), None, "nobody was counting, which is not the same as no calls");
    // Crossings before the census starts are not counted.
    let mut main = guest.thread(&boundary);
    run(&boundary, stack, &mut main, loop_a, 11);
    assert_eq!(boundary.census(), None, "a crossing with the census off was counted");

    boundary.start_census();
    // Four threads on one symbol, each a different number of times, all finished before the
    // census is read: their records have to outlive them.
    let counts = [1_000u64, 2_000, 3_000, 4_000];
    let cpus: Vec<_> = counts.iter().map(|_| guest.thread(&boundary)).collect();
    std::thread::scope(|scope| {
        for (mut cpu, &times) in cpus.into_iter().zip(&counts) {
            let boundary = &boundary;
            scope.spawn(move || run(boundary, stack, &mut cpu, loop_a, times));
        }
    });
    run(&boundary, stack, &mut main, loop_b, 7);
    run(&boundary, stack, &mut main, loop_r, 3);

    let expected = BTreeMap::from([("a", 10_000u64), ("b", 7), ("r", 3)]);
    assert_eq!(boundary.census(), Some(expected.clone()), "calls per symbol, summed over threads");

    let reports = boundary.threads();
    assert_eq!(reports.len(), 5, "one record per thread that crossed under the census: {reports:?}");
    assert_eq!(reports.iter().map(|t| t.crossings).sum::<u64>(), 10_010, "{reports:?}");
    let mut per_thread: Vec<u64> = reports.iter().map(|t| t.crossings).collect();
    per_thread.sort_unstable();
    assert_eq!(per_thread, [10, 1_000, 2_000, 3_000, 4_000], "each thread's own total: {reports:?}");
    for report in &reports {
        assert_eq!(report.crossings, report.exits, "every handler returned: {report:?}");
    }

    // Stopped: the counts stay readable and nothing more is added to them.
    boundary.stop_census();
    run(&boundary, stack, &mut main, loop_a, 5);
    assert_eq!(boundary.census(), Some(expected), "a stopped census kept counting, or lost its counts");
}

#[test]
fn last_call_and_last_caller_name_the_latest_crossing_by_any_thread() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let a = builder.bind_inline("a", noop).expect("bind");
    let b = builder.bind_inline("b", noop).expect("bind");
    let boundary = builder.finish();
    let (loop_a, site_a) = looping(&guest, a);
    let (loop_b, site_b) = looping(&guest, b);
    let stack = guest.stack_top;
    let latest = |boundary: &Boundary| {
        (boundary.last_call().map(|slot| slot.symbol.clone()), boundary.last_caller())
    };

    assert_eq!(latest(&boundary), (None, 0), "nothing has crossed under the census");
    boundary.start_census();
    let mut main = guest.thread(&boundary);
    let on_another_thread = |cpu: DynarmicCpu| {
        let boundary = &boundary;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut cpu = cpu;
                run(boundary, stack, &mut cpu, loop_a, 3);
            });
        });
    };

    // The order of the records is the order threads first crossed; the latest crossing is none
    // of first, last, or the thread asking -- each step below makes a different one of those the
    // wrong answer.
    on_another_thread(guest.thread(&boundary));
    assert_eq!(latest(&boundary), (Some("a".to_string()), site_a), "the only thread that crossed");
    run(&boundary, stack, &mut main, loop_b, 2);
    assert_eq!(latest(&boundary), (Some("b".to_string()), site_b), "a later thread's crossing");
    on_another_thread(guest.thread(&boundary));
    assert_eq!(latest(&boundary), (Some("a".to_string()), site_a), "a third thread, after both");
    run(&boundary, stack, &mut main, loop_b, 1);
    assert_eq!(
        latest(&boundary),
        (Some("b".to_string()), site_b),
        "the second thread to have crossed crossed last, and it is neither the first record nor \
         the newest"
    );
}

#[test]
fn a_thread_that_crosses_two_boundaries_is_counted_by_each_for_its_own_calls() {
    let _guard = serialized();
    let guest = Guest::new();
    let first = guest.boundary(8);
    let a = first.bind_inline("a", noop).expect("bind");
    let first = first.finish();
    // A different table: `x` sits where `a` does in the first one, so a crossing charged to the
    // wrong boundary's record would land on `a`.
    let second = guest.boundary(8);
    let x_thunk = second.bind_inline("x", noop).expect("bind");
    second.bind_inline("y", noop).expect("bind");
    let second = second.finish();
    let (loop_a, _) = looping(&guest, a);
    let (loop_x, _) = looping(&guest, x_thunk);
    let stack = guest.stack_top;

    first.start_census();
    second.start_census();
    let mut on_first = guest.thread(&first);
    let mut on_second = guest.thread(&second);
    run(&first, stack, &mut on_first, loop_a, 5);
    run(&second, stack, &mut on_second, loop_x, 3);
    run(&first, stack, &mut on_first, loop_a, 2);

    assert_eq!(first.census(), Some(BTreeMap::from([("a", 7u64)])));
    assert_eq!(second.census(), Some(BTreeMap::from([("x", 3u64)])));
    for (boundary, calls) in [(&first, 7u64), (&second, 3)] {
        let reports = boundary.threads();
        assert_eq!(reports.len(), 1, "one thread, one record per boundary: {reports:?}");
        assert_eq!((reports[0].crossings, reports[0].exits), (calls, calls), "{reports:?}");
    }
}
