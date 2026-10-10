//! `IC IVAU` batched until `ISB` (dynarmic patch 0098, `omni_cpu::dynarmic::set_batch_ic_ivau`): a
//! guest that rewrites code and does the architecture's cache maintenance runs the new code -- after
//! its `ISB` in the same run, on another thread, and when it left the jit without an `ISB` -- and
//! a `__clear_cache`-style loop over many lines invalidates every one of them.
//!
//! The switch is process-wide, so these tests live in a binary of their own, with it on throughout.
#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};
use omni_mem::Protection;

/// `IC IVAU, Xt`, `DSB ISH`, `ISB`.
const fn ic_ivau(rt: u32) -> u32 {
    0xD50B_7520 | rt
}
const DSB_ISH: u32 = 0xD503_3B9F;
const ISB: u32 = 0xD503_3FDF;
const NE: u32 = 1;

/// `STR Wt, [Xn]`.
const fn str_w(rt: u32, rn: u32) -> u32 {
    0xB900_0000 | (rn << 5) | rt
}

fn batched() -> bool {
    std::env::var("OMNI_TEST_IC_BATCH").as_deref() != Ok("0") && omni_cpu::dynarmic::set_batch_ic_ivau(true)
}

/// A two-instruction function `movz x0, #value; ret` as one little-endian word pair.
fn function(value: u16) -> u64 {
    (u64::from(ret(30)) << 32) | u64::from(movz(0, value, 0))
}

#[test]
fn code_rewritten_then_synchronised_runs_new_in_the_same_run() {
    assert!(batched());
    let guest = Guest::new();
    let f = guest.load(&[movz(0, 1, 0), ret(30)]);
    // Rewrite `f`, the cache maintenance, then jump to it -- all in one run.
    let rewriter = guest.load_at(0x100, &[str_w(1, 2), ic_ivau(2), DSB_ISH, ISB, br(2)]);
    guest.space.protect(guest.code, harness::CODE_BYTES, Protection::ReadWriteExecute).expect("the code is writable");

    let (mut a, sentinel) = guest.thread();
    assert_eq!(a.run(f, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    assert_eq!(a.x(x(0)), 1, "the old function, translated");

    a.set_x(x(1), u64::from(movz(0, 2, 0)));
    a.set_x(x(2), f as u64);
    a.set_x(x(30), sentinel as u64);
    assert_eq!(a.run(rewriter, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    assert_eq!(a.x(x(0)), 2, "after its ISB the thread runs the code it wrote, not the old translation");
}

#[test]
fn code_rewritten_by_one_thread_is_run_new_by_another() {
    assert!(batched());
    let guest = Guest::new();
    let f = guest.load(&[movz(0, 1, 0), ret(30)]);
    let rewriter = guest.load_at(0x100, &[str_w(1, 2), ic_ivau(2), DSB_ISH, ISB, ret(30)]);
    guest.space.protect(guest.code, harness::CODE_BYTES, Protection::ReadWriteExecute).expect("the code is writable");

    let (mut a, sentinel_a) = guest.thread();
    let (mut b, sentinel_b) = guest.thread();
    assert_eq!(a.run(f, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel_a });
    assert_eq!(a.x(x(0)), 1);
    b.set_x(x(1), u64::from(movz(0, 2, 0)));
    b.set_x(x(2), f as u64);
    assert_eq!(b.run(rewriter, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel_b });
    a.set_x(x(30), sentinel_a as u64);
    assert_eq!(a.run(f, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel_a });
    assert_eq!(a.x(x(0)), 2, "thread A runs the code thread B wrote");
}

#[test]
fn a_line_named_without_an_isb_is_invalidated_when_the_thread_leaves_the_jit() {
    assert!(batched());
    let guest = Guest::new();
    let f = guest.load(&[movz(0, 1, 0), ret(30)]);
    let rewriter = guest.load_at(0x100, &[str_w(1, 2), ic_ivau(2), ret(30)]);
    guest.space.protect(guest.code, harness::CODE_BYTES, Protection::ReadWriteExecute).expect("the code is writable");

    let (mut a, sentinel) = guest.thread();
    assert_eq!(a.run(f, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    a.set_x(x(1), u64::from(movz(0, 2, 0)));
    a.set_x(x(2), f as u64);
    a.set_x(x(30), sentinel as u64);
    assert_eq!(a.run(rewriter, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    a.set_x(x(30), sentinel as u64);
    assert_eq!(a.run(f, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    assert_eq!(a.x(x(0)), 2, "the pending line was invalidated at the exit");
}

/// `x9` = first line, `x11` = lines: `IC IVAU` over each 64-byte line, `DSB ISH`, `ISB`, return --
/// what `__clear_cache` does after `DC CVAU`.
fn clear_cache_loop() -> [u32; 7] {
    [ic_ivau(9), add_imm(9, 9, 64), subs_imm(11, 11, 1), b_cond(NE, -3), DSB_ISH, ISB, ret(30)]
}

/// Functions at every 64-byte line of the first `lines` lines of the code region.
const FUNCTIONS_AT: usize = 0x1000;

#[test]
fn a_clear_cache_loop_over_many_lines_invalidates_every_one() {
    assert!(batched());
    let guest = Guest::new();
    let clear = guest.load(&clear_cache_loop());
    guest.space.protect(guest.code, harness::CODE_BYTES, Protection::ReadWriteExecute).expect("the code is writable");
    let lines = 200usize;
    let at = |i: usize| guest.code + FUNCTIONS_AT + i * 64;
    for i in 0..lines {
        guest.write_u64(at(i), function(i as u16));
    }
    let (mut t, sentinel) = guest.thread();
    for i in 0..lines {
        t.set_x(x(30), sentinel as u64);
        assert_eq!(t.run(at(i), RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
        assert_eq!(t.x(x(0)), i as u64);
    }
    // Every function rewritten (as another agent would), then the guest's maintenance over them all.
    for i in 0..lines {
        guest.write_u64(at(i), function(1000 + i as u16));
    }
    t.set_x(x(9), at(0) as u64);
    t.set_x(x(11), lines as u64);
    t.set_x(x(30), sentinel as u64);
    assert_eq!(t.run(clear, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    for i in 0..lines {
        t.set_x(x(30), sentinel as u64);
        assert_eq!(t.run(at(i), RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
        assert_eq!(t.x(x(0)), 1000 + i as u64, "line {i} runs its new code");
    }
}

/// **What a clear-cache loop costs** a line: `OMNI_TEST_IC_BATCH=0` for the unbatched path.
///
/// ```text
/// cargo test -p omni-cpu --release --features dynarmic --test icache_batch -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_a_clear_cache_loop() {
    let on = batched();
    let guest = Guest::new();
    let clear = guest.load(&clear_cache_loop());
    guest.space.protect(guest.code, harness::CODE_BYTES, Protection::ReadWriteExecute).expect("the code is writable");
    let lines = (harness::CODE_BYTES - FUNCTIONS_AT) / 64;
    let (mut t, sentinel) = guest.thread();
    let mut samples = Vec::new();
    for _ in 0..21 {
        t.set_x(x(9), (guest.code + FUNCTIONS_AT) as u64);
        t.set_x(x(11), lines as u64);
        t.set_x(x(30), sentinel as u64);
        let started = std::time::Instant::now();
        assert_eq!(t.run(clear, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
        samples.push(started.elapsed().as_nanos() as f64 / lines as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("IC IVAU over {lines} lines, batched {on}: {:.1} ns a line (median of 21)", samples[10]);
}
