//! **A thread's fast-dispatch table at the size its process asks for** (patch 0035, x64 with the
//! shared cache): `DynarmicOptions::fast_dispatch_entries`. A table far smaller than the working set
//! -- 64 entries for 200 call targets, so every probe collides -- still runs every indirect call to
//! the right code, and a thread's fixed cost is the table it has.
#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicOptions;
use omni_cpu::{ExitReason, GuestCpu, RunLimit};

/// `BLR Xn`.
const fn blr(rn: u32) -> u32 {
    0xD63F_0000 | (rn << 5)
}
/// `B.NE`.
const NE: u32 = 1;

/// The tests measure the process's heap: one at a time.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TARGETS: u64 = 200;
const ROUNDS: u64 = 50;

/// Run `ROUNDS` passes over a table of `TARGETS` functions, calling each through `BLR`; function
/// `j` adds `j + 1` to x0. Returns x0 and the thread's fixed cost.
fn calls(options: DynarmicOptions) -> (u64, usize) {
    let guest = Guest::with_options(options);
    let main = guest.load(&[
        mov_reg(9, 30),
        movz(5, 0, 0),
        movz(8, TARGETS as u16, 0),
        ldr_reg(2, 1, 5),
        blr(2),
        add_imm(5, 5, 8),
        subs_imm(8, 8, 1),
        b_cond(NE, -4),
        subs_imm(4, 4, 1),
        b_cond(NE, -8),
        mov_reg(30, 9),
        ret(30),
    ]);
    for j in 0..TARGETS {
        let f = guest.load_at(0x1000 + j as usize * 16, &[add_imm(0, 0, j as u32 + 1), ret(30)]);
        guest.write_u64(guest.data + j as usize * 8, f as u64);
    }
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(0), 0);
    cpu.set_x(x(1), guest.data as u64);
    cpu.set_x(x(4), ROUNDS);
    assert_eq!(cpu.run(main, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    (cpu.x(x(0)), cpu.cost().private_committed)
}

/// Bytes the C heap holds (the C runtime's `malloc`/`operator new` -- where dynarmic allocates a
/// thread's table -- and Rust's `System` both allocate from it).
#[cfg(windows)]
fn heap_in_use() -> usize {
    #[repr(C)]
    #[derive(Default)]
    struct HeapSummaryT {
        cb: u32,
        allocated: usize,
        committed: usize,
        reserved: usize,
        max_reserve: usize,
    }
    extern "system" {
        fn GetProcessHeap() -> *mut core::ffi::c_void;
        fn HeapSummary(heap: *mut core::ffi::c_void, flags: u32, summary: *mut HeapSummaryT) -> i32;
    }
    let mut s = HeapSummaryT { cb: std::mem::size_of::<HeapSummaryT>() as u32, ..Default::default() };
    // SAFETY: the process heap is live for the process; `s` is writable and states its size.
    assert_ne!(unsafe { HeapSummary(GetProcessHeap(), 0, &mut s) }, 0, "HeapSummary");
    s.allocated
}

#[cfg(target_os = "linux")]
fn heap_in_use() -> usize {
    #[repr(C)]
    struct Mallinfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }
    extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: no arguments; returns plain data.
    let m = unsafe { mallinfo2() };
    m.uordblks + m.hblkhd
}

/// What the C heap grows by for `n` guest threads of one guest under `options`.
fn heap_for_threads(options: DynarmicOptions, n: usize) -> usize {
    let guest = Guest::with_options(options);
    let before = heap_in_use();
    let threads: Vec<_> = (0..n).map(|_| guest.thread()).collect();
    let grew = heap_in_use().saturating_sub(before);
    drop(threads);
    grew
}

/// The allocation itself, measured: 32 threads at 64 entries hold ~2 MiB less than at the pin's
/// 4,096 -- the table dynarmic allocates is the size asked for, not just the size accounted.
#[test]
fn the_table_dynarmic_allocates_is_the_size_asked_for() {
    let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !harness::shared_code_cache_asked() {
        return;
    }
    let n = 32;
    let pin = heap_for_threads(DynarmicOptions::default(), n);
    let small = heap_for_threads(DynarmicOptions { fast_dispatch_entries: 64, ..DynarmicOptions::default() }, n);
    let saved = pin.saturating_sub(small);
    let expected = n * (0x1000 - 64) * 16;
    assert!(saved >= expected * 9 / 10 && saved <= expected * 11 / 10, "saved {saved} bytes for {n} threads, expected ~{expected} (pin {pin}, small {small})");
}

#[test]
fn a_small_table_runs_every_call_to_its_own_code_and_costs_less() {
    let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let want = ROUNDS * TARGETS * (TARGETS + 1) / 2;
    let (pin, pin_cost) = calls(DynarmicOptions::default());
    let (small, small_cost) = calls(DynarmicOptions { fast_dispatch_entries: 64, ..DynarmicOptions::default() });
    assert_eq!(pin, want, "the pin's 4,096 entries");
    assert_eq!(small, want, "64 entries for 200 targets: every call still reaches its own function");
    if harness::shared_code_cache_asked() {
        assert_eq!(pin_cost - small_cost, (0x1000 - 64) * 16, "a thread's fixed cost is the table it has");
    }
    // Not a power of two, or out of range: the pin's size.
    let (odd, odd_cost) = calls(DynarmicOptions { fast_dispatch_entries: 1000, ..DynarmicOptions::default() });
    assert_eq!((odd, odd_cost), (want, pin_cost));
}
