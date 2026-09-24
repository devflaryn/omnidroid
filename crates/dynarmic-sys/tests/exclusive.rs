//! **Load/store-exclusive with `fastmem_exclusive_access`**: every width, the failure cases, the
//! fallback, and many threads on one monitor.
//!
//! Root cause of `a64_exec::atomic_load_exclusive_store_exclusive` failing on arm64: the pin's arm64
//! backend accepted `fastmem_exclusive_access` and **ignored** it (`EmitExclusiveReadMemory` and
//! `EmitExclusiveWriteMemory` in `emit_arm64_memory.cpp` called the callback-only versions
//! unconditionally, and the `EmitConfig` never even carried the flag), so every `LDXR`/`STXR` pair
//! cost a slow-path read and an exclusive-write callback -- 2 callback entries where the x64 backend
//! takes 0, and exactly what `omni-cpu`'s per-slice invariant refuses as a degraded memory path.
//! Patch 0007 gives arm64 the x64 inline protocol (see `patches/README.md`).
//!
//! Encodings checked against the LLVM assembler. **Oracle: the ARM ARM** for what the guest must
//! observe -- `LDXR` returns memory; `STXR` writes and reports 0 only while this PE holds a
//! reservation for the address, reports 1 and writes nothing otherwise; `CLREX` and a completed
//! `STXR` both clear the local monitor -- and arithmetic for the counters.

mod harness;

use dynarmic_sys::*;
use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE, MEM_GUARD, MEM_SIZE};

/// `LDXR{B,H,,} Rt, [Xn]` by `size` (0 = byte ... 3 = doubleword).
const fn ldxr(size: u32, rt: u32, rn: u32) -> u32 {
    (size << 30) | 0x085F_7C00 | (rn << 5) | rt
}
/// `STXR{B,H,,} Ws, Rt, [Xn]`.
const fn stxr(size: u32, rs: u32, rt: u32, rn: u32) -> u32 {
    (size << 30) | 0x0800_7C00 | (rs << 16) | (rn << 5) | rt
}
/// `LDAXR Xt, [Xn]`.
const fn ldaxr_x(rt: u32, rn: u32) -> u32 {
    0xC85F_FC00 | (rn << 5) | rt
}
/// `STLXR Ws, Xt, [Xn]`.
const fn stlxr_x(rs: u32, rt: u32, rn: u32) -> u32 {
    0xC800_FC00 | (rs << 16) | (rn << 5) | rt
}
/// `LDXP Xt1, Xt2, [Xn]`.
const fn ldxp(rt1: u32, rt2: u32, rn: u32) -> u32 {
    0xC87F_0000 | (rt2 << 10) | (rn << 5) | rt1
}
/// `STXP Ws, Xt1, Xt2, [Xn]`.
const fn stxp(rs: u32, rt1: u32, rt2: u32, rn: u32) -> u32 {
    0xC820_0000 | (rs << 16) | (rt2 << 10) | (rn << 5) | rt1
}
/// `CBNZ Wt, label`.
const fn cbnz_w(rt: u32, offset_insns: i32) -> u32 {
    0x3500_0000 | (((offset_insns as u32) & 0x7_FFFF) << 5) | rt
}
const CLREX: u32 = 0xD503_3F5F;

const ADDR: u64 = 0x4000;

fn inline() -> VmOptions {
    VmOptions { fastmem_exclusive: true, ..VmOptions::default() }
}

/// Runs `code` (ending in `SVC #0`) with `X4 = ADDR` and `seed` at `ADDR`.
fn run(code: Vec<u32>, opts: VmOptions, seed: [u64; 2], regs: &[(u32, u64)]) -> Vm {
    let vm = Vm::new(code, opts);
    vm.with_ctx(|c| {
        c.write_u64(ADDR, seed[0]);
        c.write_u64(ADDR + 8, seed[1]);
    });
    vm.set_reg(4, ADDR);
    for (r, v) in regs {
        vm.set_reg(*r, *v);
    }
    vm.start(1_000_000);
    let hr = vm.run_to_completion(64);
    assert_eq!(hr & HALT_DONE, HALT_DONE, "the guest did not reach its SVC: halt {hr:#010X}");
    vm
}

#[test]
fn the_encoders_match_the_llvm_assembler() {
    assert_eq!(ldxr(0, 1, 4), 0x085F_7C81);
    assert_eq!(ldxr(1, 1, 4), 0x485F_7C81);
    assert_eq!(ldxr(2, 1, 4), 0x885F_7C81);
    assert_eq!(ldxr(3, 1, 4), 0xC85F_7C81);
    assert_eq!(stxr(0, 2, 1, 4), 0x0802_7C81);
    assert_eq!(stxr(3, 2, 1, 4), 0xC802_7C81);
    assert_eq!(ldaxr_x(1, 4), 0xC85F_FC81);
    assert_eq!(stlxr_x(2, 1, 4), 0xC802_FC81);
    assert_eq!(ldxp(1, 3, 4), 0xC87F_0C81);
    assert_eq!(stxp(2, 5, 6, 4), 0xC822_1885);
    assert_eq!(cbnz_w(2, 0), 0x3500_0002);
}

#[test]
fn every_width_loads_stores_and_stays_off_the_callback_path() {
    // LDXR Rt1,[X4] ; STXR W2, X5, [X4] ; SVC -- memory holds the seed, X5 the new value.
    for (size, mask) in [(0u32, 0xFFu64), (1, 0xFFFF), (2, 0xFFFF_FFFF), (3, u64::MAX)] {
        let seed = 0x8877_6655_4433_2211u64;
        let new = 0x0123_4567_89AB_CDEFu64;
        let vm = run(vec![ldxr(size, 1, 4), stxr(size, 2, 5, 4), a64::svc(0)], inline(), [seed, 0], &[(5, new)]);
        assert_eq!(vm.reg(1), seed & mask, "size {size}: LDXR read the low bytes, zero-extended");
        assert_eq!(vm.reg(2), 0, "size {size}: STXR succeeded");
        let expect = (seed & !mask) | (new & mask);
        assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR)), expect, "size {size}: STXR wrote only its bytes");
        assert_eq!(vm.stats().slow_path_total, 0, "size {size}: {:?}", vm.stats());
    }
}

#[test]
fn a_pair_of_doublewords_is_exclusive_too() {
    // LDXP X1, X3, [X4] ; STXP W2, X5, X6, [X4] ; SVC
    let vm = run(
        vec![ldxp(1, 3, 4), stxp(2, 5, 6, 4), a64::svc(0)],
        inline(),
        [0x1111, 0x2222],
        &[(5, 0xAAAA), (6, 0xBBBB)],
    );
    assert_eq!((vm.reg(1), vm.reg(3)), (0x1111, 0x2222), "LDXP read both halves");
    assert_eq!(vm.reg(2), 0, "STXP succeeded");
    assert_eq!(vm.with_ctx(|c| (c.read_u64(ADDR), c.read_u64(ADDR + 8))), (0xAAAA, 0xBBBB));
    assert_eq!(vm.stats().slow_path_total, 0, "{:?}", vm.stats());
}

#[test]
fn a_store_exclusive_without_a_reservation_fails_and_writes_nothing() {
    for opts in [inline(), VmOptions::default()] {
        let label = if opts.fastmem_exclusive { "inline" } else { "callbacks" };
        // No LDXR at all.
        let vm = run(vec![stxr(3, 2, 5, 4), a64::svc(0)], opts, [7, 0], &[(5, 9)]);
        assert_eq!(vm.reg(2), 1, "{label}: no reservation, so STXR fails");
        assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR)), 7, "{label}: and does not write");
        // LDXR ; CLREX ; STXR.
        let vm = run(vec![ldxr(3, 1, 4), CLREX, stxr(3, 2, 5, 4), a64::svc(0)], opts, [7, 0], &[(5, 9)]);
        assert_eq!(vm.reg(2), 1, "{label}: CLREX dropped the reservation");
        assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR)), 7);
        // LDXR ; STXR ; STXR -- the first store spends the reservation.
        let vm = run(
            vec![ldxr(3, 1, 4), stxr(3, 2, 5, 4), stxr(3, 3, 6, 4), a64::svc(0)],
            opts,
            [7, 0],
            &[(5, 9), (6, 11)],
        );
        assert_eq!((vm.reg(2), vm.reg(3)), (0, 1), "{label}: one success, then a failure");
        assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR)), 9);
        // LDXR from ADDR ; STXR to ADDR + 8: a different address is not reserved.
        let vm = run(
            vec![ldxr(3, 1, 4), a64::add_imm(7, 4, 8), stxr(3, 2, 5, 7), a64::svc(0)],
            opts,
            [7, 8],
            &[(5, 9)],
        );
        assert_eq!(vm.reg(2), 1, "{label}: the reservation is for ADDR, not ADDR + 8");
        assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR + 8)), 8);
    }
}

#[test]
fn an_address_outside_fastmem_falls_back_to_the_callbacks_and_still_works() {
    // With mirroring off, a guest address past MEM_BITS misses fastmem: the inline sequence's
    // fallback releases the monitor lock and does the access through the monitor and the callbacks
    // (which mask it into the arena). Same answers, and the callbacks are what served it.
    //
    // LDXR X1, [X4] ; STXR W2, X5, [X4] ; LDXP X7, X8, [X4] ; STXP W9, X5, X5, [X4] ; SVC
    let vm = Vm::new(
        vec![ldxr(3, 1, 4), stxr(3, 2, 5, 4), ldxp(7, 8, 4), stxp(9, 5, 5, 4), a64::svc(0)],
        VmOptions { mirror: false, ..inline() },
    );
    vm.with_ctx(|c| c.write_u64(ADDR, 0x77));
    vm.set_reg(4, ADDR | (1 << 40));
    vm.set_reg(5, 0x99);
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(1), 0x77, "LDXR through the fallback read memory");
    assert_eq!(vm.reg(2), 0, "STXR through the fallback succeeded");
    assert_eq!((vm.reg(7), vm.reg(8)), (0x99, 0), "LDXP through the fallback");
    assert_eq!(vm.reg(9), 0, "STXP through the fallback");
    assert_eq!(vm.with_ctx(|c| (c.read_u64(ADDR), c.read_u64(ADDR + 8))), (0x99, 0x99));
    let stats = vm.stats();
    assert!(stats.slow_path_exclusive >= 2, "the fallback is the callback path: {stats:?}");

    // The same program in range, mirroring still off, stays inline.
    let vm = run(
        vec![ldxr(3, 1, 4), stxr(3, 2, 5, 4), ldxp(7, 8, 4), stxp(9, 5, 5, 4), a64::svc(0)],
        VmOptions { mirror: false, ..inline() },
        [0x77, 0],
        &[(5, 0x99)],
    );
    assert_eq!((vm.reg(1), vm.reg(2), vm.reg(9)), (0x77, 0, 0));
    assert_eq!(vm.stats().slow_path_total, 0, "{:?}", vm.stats());
}

#[test]
fn another_observer_s_store_between_the_pair_makes_the_store_exclusive_fail() {
    // ARM ARM: a store by another observer to the reserved location clears this PE's global monitor,
    // so the store-exclusive must fail and write nothing. The "other observer" is the host, storing
    // from inside `SVC #3` between the LDXR and the STXR.
    // LDXR X1, [X4] ; SVC #3 ; STXR W2, X5, [X4] ; SVC #0
    for opts in [inline(), VmOptions::default()] {
        let label = if opts.fastmem_exclusive { "inline" } else { "callbacks" };
        let vm = Vm::new(vec![ldxr(3, 1, 4), a64::svc(3), stxr(3, 2, 5, 4), a64::svc(0)], opts);
        vm.with_ctx(|c| {
            c.write_u64(ADDR, 7);
            c.poke_on_svc3 = Some((ADDR, 8));
        });
        vm.set_reg(4, ADDR);
        vm.set_reg(5, 9);
        vm.start(1_000_000);
        assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
        assert_eq!(vm.reg(1), 7);
        assert_eq!(vm.reg(2), 1, "{label}: the store-exclusive must fail after another observer stored");
        assert_eq!(vm.with_ctx(|c| c.read_u64(ADDR)), 8, "{label}: and must not overwrite that store");
    }
}

#[test]
fn a_successful_store_exclusive_clears_another_processor_s_reservation_of_the_address() {
    // Processor A reserves ADDR ; processor B reserves it and stores to it -- the *same* value, so
    // only the monitor, not the memory, can tell A its reservation is gone ; A's store-exclusive
    // must then fail (ARM ARM: B's store clears A's global monitor).
    // A: LDXR X1, [X4] ; SVC #0 ; STXR W2, X1, [X4] ; SVC #0
    // B: LDXR X1, [X4] ; STXR W2, X1, [X4] ; SVC #0
    for inline_a in [true, false] {
        // SAFETY: freed below, after both jits are dropped.
        let monitor = unsafe { od_monitor_new(2) } as usize;
        let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
        let arena = arena.as_mut_ptr() as usize;
        let shared = |pid: u32, fastmem_exclusive: bool| VmOptions {
            fastmem_exclusive,
            shared_monitor: monitor,
            processor_id: pid,
            shared_arena: arena,
            ..VmOptions::default()
        };
        {
            let a = Vm::new(vec![ldxr(3, 1, 4), a64::svc(0), stxr(3, 2, 1, 4), a64::svc(0)], shared(0, inline_a));
            let b = Vm::new(vec![ldxr(3, 1, 4), stxr(3, 2, 1, 4), a64::svc(0)], shared(1, true));
            a.with_ctx(|c| c.write_u64(ADDR, 0x42));
            for vm in [&a, &b] {
                vm.set_reg(4, ADDR);
                vm.start(1_000_000);
            }
            assert_eq!(a.run_to_completion(64) & HALT_DONE, HALT_DONE, "A reserved");
            assert_eq!(b.run_to_completion(64) & HALT_DONE, HALT_DONE, "B reserved and stored");
            assert_eq!(b.reg(2), 0, "B's store-exclusive succeeded");
            assert_eq!(a.run_to_completion(64) & HALT_DONE, HALT_DONE, "A tried to store");
            assert_eq!(
                a.reg(2),
                1,
                "A's reservation must have been cleared by B's store (A inline: {inline_a})"
            );
        }
        // SAFETY: both jits using it are dropped.
        unsafe { od_monitor_free(monitor as *mut std::ffi::c_void) };
    }
}

/// `THREADS` guest threads on one monitor and one arena, each adding 1 to one doubleword
/// `ITERATIONS` times with the canonical retry loop. Any lost update -- two threads' `STXR` both
/// succeeding against the same old value -- shows as a short total.
fn contended_counter(modes: &[bool]) {
    const ITERATIONS: u64 = 50_000;
    // retry: LDAXR X1, [X4] ; ADD X1, X1, #1 ; STLXR W2, X1, [X4] ; CBNZ W2, retry ;
    //        SUBS X5, X5, #1 ; B.NE retry ; SVC #0
    let code = vec![
        ldaxr_x(1, 4),
        a64::add_imm(1, 1, 1),
        stlxr_x(2, 1, 4),
        cbnz_w(2, -3),
        a64::subs_imm(5, 5, 1),
        a64::b_cond(a64::cond::NE, -5),
        a64::svc(0),
    ];
    // SAFETY: freed below, after every thread using it has been joined.
    let monitor = unsafe { od_monitor_new(modes.len() as u64) } as usize;
    assert_ne!(monitor, 0);
    let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
    let arena = arena.as_mut_ptr() as usize;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(modes.len()));

    let threads: Vec<_> = modes
        .iter()
        .enumerate()
        .map(|(i, &inline)| {
            let code = code.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let vm = Vm::new(
                    code,
                    VmOptions {
                        fastmem_exclusive: inline,
                        shared_monitor: monitor,
                        processor_id: i as u32,
                        shared_arena: arena,
                        cycle_counting: false,
                        ..VmOptions::default()
                    },
                );
                vm.set_reg(4, ADDR);
                vm.set_reg(5, ITERATIONS);
                vm.start(u64::MAX >> 1);
                barrier.wait();
                assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
                vm.stats()
            })
        })
        .collect();
    let stats: Vec<OdStats> = threads.into_iter().map(|t| t.join().expect("guest thread")).collect();
    // SAFETY: `arena` is the leaked buffer above, and every thread is joined.
    let total = unsafe { *((arena + ADDR as usize) as *const u64) };
    // SAFETY: every jit using it is gone (each `Vm` dropped with its thread).
    unsafe { od_monitor_free(monitor as *mut std::ffi::c_void) };

    assert_eq!(total, ITERATIONS * modes.len() as u64, "lost updates, modes {modes:?}: {total}");
    for (inline, s) in modes.iter().zip(&stats) {
        if *inline {
            // A contended STXR can fail and retry, but it never leaves the inline path.
            assert_eq!(s.slow_path_total, 0, "an inline thread took callbacks: {s:?}");
        } else {
            assert!(s.slow_path_exclusive >= ITERATIONS, "a callback thread must use callbacks: {s:?}");
        }
    }
}

#[test]
fn four_inline_threads_never_lose_an_update() {
    contended_counter(&[true, true, true, true]);
}

#[test]
fn inline_and_callback_threads_share_one_monitor_without_losing_an_update() {
    contended_counter(&[true, false, true, false]);
}
