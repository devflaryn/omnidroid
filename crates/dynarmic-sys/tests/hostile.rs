//! Global Constraint 11: guest code is the ultimate untrusted input. It jumps
//! to unmapped addresses, executes garbage, loops without bound and asks for
//! absurd configurations. None of that may take the host process down.
//!
//! The parts that could plausibly abort rather than return run in a **child
//! process**, so an abort is an observable test failure instead of something
//! that kills the test runner and takes the rest of the suite with it.

mod harness;

use dynarmic_sys::*;
use harness::a64::{self};
use harness::{Vm, VmOptions, CODE_BASE};
use std::process::Command;

/// Re-runs this test binary with only `name` selected, in a child process.
/// Returns true if the child exited successfully within `secs`. A child that
/// never exits is a failure, not a wait: guest code that wedges a host thread
/// is exactly the hazard these tests are looking for.
fn in_child(name: &str, secs: u64) -> bool {
    let mut child = spawn_child(name);
    wait_up_to(&mut child, secs).is_some_and(|st| st.success())
}

fn is_child() -> bool {
    std::env::var_os("OD_HOSTILE_CHILD").is_some()
}

#[test]
fn a_branch_into_unmapped_memory_is_a_typed_exit() {
    // B +1024 instructions, which is past the end of the program. `read_code`
    // returns 0 there, so dynarmic raises NoExecuteFault instead of the host
    // taking an access violation.
    //
    // B  +1024                          14000400
    let code = vec![a64::b(1024)];
    assert_eq!(code[0], 0x1400_0400);

    let vm = Vm::new(code, VmOptions::default());
    vm.start(100_000);
    let hr = vm.run_to_completion(16);
    let ex = vm.with_ctx(|c| c.exceptions.clone());
    assert_eq!(ex.len(), 1, "exactly one exception: {ex:?}");
    assert_eq!(
        ex[0],
        (CODE_BASE + 1024 * 4, exception::NO_EXECUTE_FAULT),
        "the faulting guest address is reported, not lost"
    );
    assert_ne!(hr, 0, "execution stopped");
}

#[test]
fn an_unaligned_pc_faults_rather_than_reading_between_instructions() {
    let vm = Vm::new(vec![a64::svc(0)], VmOptions::default());
    vm.set_pc(CODE_BASE + 2);
    vm.with_ctx(|c| c.ticks_remaining = 100_000);
    let _ = vm.run_to_completion(16);
    let ex = vm.with_ctx(|c| c.exceptions.clone());
    assert_eq!(ex.len(), 1, "{ex:?}");
    assert_eq!(ex[0].1, exception::NO_EXECUTE_FAULT);
}

#[test]
fn a_wild_guest_address_stays_inside_the_arena() {
    // The guest computes an address far past the 1 MiB fastmem arena and
    // stores through it. With `silently_mirror_fastmem` the emitted code masks
    // the address to `fastmem_address_space_bits`, so this wraps rather than
    // writing into whatever the host has after the allocation.
    //
    // MOVZ X4, #0xBEEF, LSL #32         D2D7DDE4
    // MOVK X4, #0x2000                  F2840004
    // MOVZ X0, #0x77                    D2800EE0
    // STR  X0, [X4]                     F9000080
    // SVC  #0                           D4000001
    let code = vec![
        a64::movz(4, 0xBEEF, 2),
        a64::movk(4, 0x2000, 0),
        a64::movz(0, 0x77, 0),
        a64::str_imm(0, 4, 0),
        a64::svc(0),
    ];
    assert_eq!(code[0], 0xD2D7_DDE4);
    assert_eq!(code[1], 0xF284_0004);
    assert_eq!(code[2], 0xD280_0EE0);
    assert_eq!(code[3], 0xF900_0080);

    let vm = Vm::new(code, VmOptions::default());
    vm.start(100_000);
    let hr = vm.run_to_completion(16);
    assert_ne!(hr & harness::HALT_DONE, 0);
    vm.with_ctx(|c| {
        assert_eq!(
            c.read_u64(0x2000),
            0x77,
            "the address was masked into the arena"
        );
    });
    assert_eq!(vm.stats().slow_path_total, 0, "it stayed on the fast path");
}

#[test]
fn absurd_configurations_are_refused_rather_than_asserted() {
    let cb = harness::CALLBACKS;
    let base = OdConfig {
        abi_version: OD_DYNARMIC_ABI_VERSION,
        callbacks: &cb,
        ctx: std::ptr::null_mut(),
        tpidr_el0: std::ptr::null_mut(),
        tpidrro_el0: std::ptr::null(),
        fastmem_enabled: 1,
        fastmem_pointer: 0x1000,
        fastmem_address_space_bits: 20,
        silently_mirror_fastmem: 1,
        recompile_on_fastmem_failure: 1,
        fastmem_exclusive_access: 0,
        monitor: std::ptr::null_mut(),
        processor_id: 0,
        code_cache_size: 8 << 20,
        cntfrq_el0: 0,
        ctr_el0: 0,
        dczid_el0: 4,
        enable_cycle_counting: 0,
        wall_clock_cntpct: 0,
        hook_hint_instructions: 0,
        define_unpredictable_behaviour: 0,
        check_halt_on_memory_access: 0,
        unsafe_optimizations: 0,
        optimizations: optimization::ALL_SAFE,
    };

    let mut rejected: Vec<&str> = Vec::new();
    let mut check = |name: &'static str, cfg: OdConfig| {
        // SAFETY: `cfg` is a fully initialised `OdConfig`. Any jit it does
        // produce is freed immediately.
        let p = unsafe { od_jit_new(&cfg) };
        if p.is_null() {
            rejected.push(name);
        } else {
            // SAFETY: `p` came from `od_jit_new` and is not executing.
            unsafe { od_jit_free(p) };
        }
    };

    check(
        "abi_version",
        OdConfig {
            abi_version: OD_DYNARMIC_ABI_VERSION + 1,
            ..base
        },
    );
    check(
        "null callbacks",
        OdConfig {
            callbacks: std::ptr::null(),
            ..base
        },
    );
    let mut holed = cb;
    holed.read64 = None;
    check(
        "callback table with a hole",
        OdConfig {
            callbacks: &holed,
            ..base
        },
    );
    check(
        "fastmem bits 0",
        OdConfig {
            fastmem_address_space_bits: 0,
            ..base
        },
    );
    check(
        "fastmem bits 11",
        OdConfig {
            fastmem_address_space_bits: 11,
            ..base
        },
    );
    check(
        "fastmem bits 65",
        OdConfig {
            fastmem_address_space_bits: 65,
            ..base
        },
    );
    check(
        "fastmem bits u32::MAX",
        OdConfig {
            fastmem_address_space_bits: u32::MAX,
            ..base
        },
    );
    check(
        "code cache 7 MiB",
        OdConfig {
            // Just under dynarmic's documented minimum. Unlike 1 byte and
            // u64::MAX, this is a size dynarmic will happily *construct* and
            // then misbehave on later, so it is the case the shim's own range
            // check has to carry.
            code_cache_size: 7 << 20,
            ..base
        },
    );
    check(
        "code cache 1 byte",
        OdConfig {
            code_cache_size: 1,
            ..base
        },
    );
    check(
        "code cache u64::MAX",
        OdConfig {
            code_cache_size: u64::MAX,
            ..base
        },
    );

    let expected = [
        "abi_version",
        "null callbacks",
        "callback table with a hole",
        "fastmem bits 0",
        "fastmem bits 11",
        "fastmem bits 65",
        "fastmem bits u32::MAX",
        "code cache 7 MiB",
        "code cache 1 byte",
        "code cache u64::MAX",
    ];
    assert_eq!(rejected, expected, "one of these was accepted");
}

#[test]
fn a_processor_id_past_the_end_of_the_monitor_is_refused() {
    // dynarmic indexes `exclusive_addresses[processor_id]` with no bounds
    // check, so an out-of-range id is an out-of-bounds write from the first
    // LDXR the guest executes.
    // SAFETY: freed below.
    let monitor = unsafe { od_monitor_new(2) };
    assert!(!monitor.is_null());

    let cb = harness::CALLBACKS;
    let cfg = OdConfig {
        abi_version: OD_DYNARMIC_ABI_VERSION,
        callbacks: &cb,
        ctx: std::ptr::null_mut(),
        tpidr_el0: std::ptr::null_mut(),
        tpidrro_el0: std::ptr::null(),
        fastmem_enabled: 0,
        fastmem_pointer: 0,
        fastmem_address_space_bits: 0,
        silently_mirror_fastmem: 0,
        recompile_on_fastmem_failure: 0,
        fastmem_exclusive_access: 0,
        monitor,
        processor_id: 2,
        code_cache_size: 8 << 20,
        cntfrq_el0: 0,
        ctr_el0: 0,
        dczid_el0: 4,
        enable_cycle_counting: 0,
        wall_clock_cntpct: 0,
        hook_hint_instructions: 0,
        define_unpredictable_behaviour: 0,
        check_halt_on_memory_access: 0,
        unsafe_optimizations: 0,
        optimizations: optimization::ALL_SAFE,
    };
    // SAFETY: `cfg` is fully initialised.
    let p = unsafe { od_jit_new(&cfg) };
    assert!(p.is_null(), "processor_id 2 of 2 must be refused");

    // SAFETY: nothing holds the monitor.
    unsafe { od_monitor_free(monitor) };
}

#[test]
fn monitor_sizes_are_bounded() {
    // SAFETY: both calls either return null or a monitor freed immediately.
    unsafe {
        assert!(od_monitor_new(0).is_null(), "zero processors");
        assert!(od_monitor_new(u64::MAX).is_null(), "u64::MAX processors");
        let m = od_monitor_new(4);
        assert!(!m.is_null());
        od_monitor_free(m);
        od_monitor_free(std::ptr::null_mut());
    }
}

#[test]
fn invalidate_range_survives_degenerate_arguments() {
    // dynarmic computes `start + length - 1` and builds a closed interval from
    // it. A zero length underflows that into an interval covering the whole
    // address space, and an overflowing length does the same by another route.
    let vm = Vm::new(vec![a64::movz(0, 7, 0), a64::svc(0)], VmOptions::default());
    let jit = vm.raw();

    // Warm the cache, then measure with `read_code`: instruction fetches only
    // happen during translation, so "was this range actually invalidated?" is
    // an observable question rather than a matter of trust.
    vm.start(10_000);
    assert_ne!(vm.run_to_completion(16) & harness::HALT_DONE, 0);
    vm.reset_stats();
    vm.start(10_000);
    assert_ne!(vm.run_to_completion(16) & harness::HALT_DONE, 0);
    assert_eq!(vm.stats().read_code, 0, "the block is cached");

    // A zero length must invalidate *nothing*. dynarmic would compute
    // `addr + 0 - 1`, underflow, and throw away every translation in the
    // process -- correct, and 0.15-0.31 Mguest-insn/s to rebuild.
    vm.reset_stats();
    // SAFETY: `jit` is live and not executing.
    unsafe { od_jit_invalidate_range(jit, CODE_BASE, 0) };
    vm.start(10_000);
    // Not `run_to_completion`: that one absorbs a cache-invalidation halt and
    // runs again, which is precisely what must not be needed here.
    let hr = vm.run();
    assert_eq!(
        hr & OD_HALT_CACHE_INVALIDATION,
        0,
        "a zero-length invalidation interrupted the guest for nothing"
    );
    assert_ne!(hr & harness::HALT_DONE, 0, "and it ran to the SVC");
    assert_eq!(
        vm.stats().read_code,
        0,
        "a zero-length invalidation threw translations away"
    );
    // SAFETY: `jit` is live and execution has returned.
    unsafe { od_jit_clear_halt(jit, harness::HALT_DONE) };

    // A length that overflows the address space must still invalidate what it
    // covers. Passing it through unclamped makes `addr + len - 1` wrap below
    // `addr`, and an inverted interval invalidates nothing at all -- the guest
    // then executes stale code after telling us it changed.
    vm.reset_stats();
    // SAFETY: as above.
    unsafe { od_jit_invalidate_range(jit, CODE_BASE, u64::MAX) };
    vm.start(10_000);
    assert_ne!(vm.run_to_completion(16) & harness::HALT_DONE, 0);
    assert!(
        vm.stats().read_code > 0,
        "an overflowing length silently invalidated nothing"
    );

    // The rest must simply not blow up.
    // SAFETY: as above.
    unsafe {
        od_jit_invalidate_range(jit, 0, 0);
        od_jit_invalidate_range(jit, u64::MAX, 1);
        od_jit_invalidate_range(jit, u64::MAX, u64::MAX);
        od_jit_invalidate_range(jit, 0, u64::MAX);
        od_jit_invalidate_range(jit, CODE_BASE, 4);
        od_jit_clear_cache(jit);
        od_jit_clear_exclusive(jit);
    }
    // Still usable afterwards.
    vm.start(10_000);
    let hr = vm.run_to_completion(16);
    assert_ne!(hr & harness::HALT_DONE, 0);
    assert_eq!(vm.reg(0), 7);
}

#[test]
fn a_zero_length_invalidation_does_not_interrupt_the_guest() {
    // `InvalidateCacheRange` raises `HaltReason::CacheInvalidation`
    // unconditionally, before it looks at the range. Outside execution that is
    // invisible -- `Jit::Run` services the pending invalidation on the way in.
    // From *inside a callback* it is not: the guest stops at the next block
    // boundary so a dispatcher can service an invalidation of nothing.
    //
    // A guest reaches this by executing `IC IVAU` over an empty range, which is
    // legal and which a memcpy-style routine can emit at the tail of a copy.
    //
    // 0: MOVZ X0, #1                    D2800020
    // 1: SVC  #0                        D4000001   ; callback invalidates [pc, pc)
    // 2: ADD  X0, X0, #1                91000400
    // 3: SVC  #0                        D4000001
    let code = vec![
        a64::movz(0, 1, 0),
        a64::svc(0),
        a64::add_imm(0, 0, 1),
        a64::svc(0),
    ];

    let vm = Vm::new(code, VmOptions::default());
    vm.with_ctx(|c| {
        c.halt_on_svc = false;
        c.zero_invalidate_on_svc = true;
    });
    vm.start(100_000);

    // One run. It should reach the end of the program and fault there, not
    // come back early asking to be resumed.
    let hr = vm.run();
    assert_eq!(
        hr & OD_HALT_CACHE_INVALIDATION,
        0,
        "a zero-length invalidation interrupted the guest ({hr:#010X})"
    );
    assert_eq!(vm.reg(0), 2, "both halves of the program ran, in one go");
    assert_eq!(vm.with_ctx(|c| c.svc.len()), 2);
}

#[test]
fn register_indices_are_bounds_checked() {
    // dynarmic indexes its register array unchecked; the shim does not.
    let vm = Vm::new(vec![a64::svc(0)], VmOptions::default());
    let jit = vm.raw();
    // A distinctive value in the last real register, so that an out-of-range
    // index clamped to 30 -- which reads as a tidy fix -- is distinguishable
    // from one refused.
    vm.set_reg(30, 0x1234_5678_9ABC_DEF0);
    // SAFETY: `jit` is live; the point of the test is that out-of-range
    // indices are handled by the shim rather than reaching dynarmic.
    unsafe {
        assert_eq!(od_jit_get_reg(jit, 31), 0, "X31 does not exist");
        assert_eq!(od_jit_get_reg(jit, u32::MAX), 0);
        od_jit_set_reg(jit, 31, 0xDEAD);
        od_jit_set_reg(jit, u32::MAX, 0xDEAD);
        let mut v = [0xAAu64; 2];
        od_jit_get_vec(jit, 32, v.as_mut_ptr());
        assert_eq!(v, [0, 0], "V32 does not exist");
        od_jit_get_vec(jit, u32::MAX, v.as_mut_ptr());
        assert_eq!(v, [0, 0]);
        od_jit_set_vec(jit, 32, v.as_ptr());
        od_jit_set_vec(jit, u32::MAX, v.as_ptr());
    }
    assert_eq!(vm.reg(0), 0, "no neighbouring register was clobbered");
    assert_eq!(
        vm.reg(30),
        0x1234_5678_9ABC_DEF0,
        "an out-of-range write must be dropped, not folded onto X30"
    );
    assert_eq!(vm.sp(), 0);
}

#[test]
fn null_handles_are_accepted_where_documented() {
    // SAFETY: both functions document null as a no-op.
    unsafe {
        od_jit_free(std::ptr::null_mut());
        od_monitor_free(std::ptr::null_mut());
        assert!(od_jit_new(std::ptr::null()).is_null());
    }
}

#[test]
fn a_callback_panic_becomes_a_halt_not_an_abort() {
    // A Rust `extern "C"` function that unwinds aborts the process, and guest
    // code decides when callbacks fire. The harness catches the panic, halts
    // the jit and re-raises after `run` has returned -- so the panic is
    // observable to the test but never crosses a JIT frame.
    //
    // MOVZ X4, #0x2000                  D2840004
    // LDRB W0, [X4]                     39400080
    // SVC  #0                           D4000001
    let code = vec![a64::movz(4, 0x2000, 0), a64::ldrb_imm(0, 4, 0), a64::svc(0)];
    assert_eq!(code[1], 0x3940_0080);

    let vm = Vm::new(
        code,
        VmOptions {
            fastmem: false, // force the read through the callback
            ..VmOptions::default()
        },
    );
    vm.with_ctx(|c| c.panic_on_read8 = true);
    vm.start(10_000);

    // Called directly rather than through `Vm::run`, because `Vm::run`
    // re-raises the panic and the halt reason is what this test is about.
    // dynarmic consumes the halt reason as it returns it, so it can only be
    // observed here.
    // SAFETY: `vm.raw()` is live and not executing; the callbacks cannot
    // unwind past `harness::with`.
    let hr = unsafe { od_jit_run(vm.raw()) };

    // 1. The process is still running, which is the whole point: an
    //    `extern "C"` Rust function that unwinds aborts, and guest code picks
    //    when callbacks fire.
    // 2. The halt is load-bearing, not decoration -- a callback that failed
    //    must stop the guest rather than let it run on with a bogus value.
    assert_eq!(
        hr & harness::HALT_PANIC,
        harness::HALT_PANIC,
        "the panicking callback did not halt the jit (halt reason {hr:#010X})"
    );
    // 3. The payload survived, so the caller can re-raise it.
    let msg = vm
        .with_ctx(|c| c.panic_msg.take())
        .expect("the panic payload was not recorded");
    assert!(msg.contains("on purpose"), "unexpected panic: {msg}");

    // And the jit is still usable.
    // SAFETY: `vm.raw()` is live and execution has returned.
    assert_eq!(unsafe { od_jit_is_executing(vm.raw()) }, 0);
    assert_eq!(vm.reg(4), 0x2000, "registers still readable");
}

#[test]
fn vm_run_re_raises_a_caught_callback_panic() {
    // The other half of the discipline: the panic is not swallowed. It is
    // deferred until the JIT frames have been left, then resumed.
    let code = vec![a64::movz(4, 0x2000, 0), a64::ldrb_imm(0, 4, 0), a64::svc(0)];
    let vm = Vm::new(
        code,
        VmOptions {
            fastmem: false,
            ..VmOptions::default()
        },
    );
    vm.with_ctx(|c| c.panic_on_read8 = true);
    vm.start(10_000);

    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run()))
        .expect_err("Vm::run should have re-raised the callback's panic");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| "?".into());
    assert!(msg.contains("on purpose"), "unexpected panic: {msg}");
}

#[test]
fn garbage_instruction_words_do_not_take_down_the_process() {
    if is_child() {
        fuzz_random_words(true);
        return;
    }
    assert!(
        in_child("garbage_instruction_words_do_not_take_down_the_process", 120),
        "the child process died running random A64 words"
    );
}

#[test]
fn garbage_instruction_words_through_the_callback_path() {
    if is_child() {
        fuzz_random_words(false);
        return;
    }
    assert!(
        in_child("garbage_instruction_words_through_the_callback_path", 120),
        "the child process died running random A64 words without fastmem"
    );
}

/// Feed dynarmic's A64 decoder pseudo-random 32-bit words and execute them.
/// Deterministic, so a failure can be reproduced.
fn fuzz_random_words(fastmem: bool) {
    let trials: usize = std::env::var("OD_FUZZ_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    const WORDS: usize = 8;

    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    let mut next = move || {
        // SplitMix64.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };

    let vm = Vm::new(
        vec![a64::svc(0)],
        VmOptions {
            fastmem,
            cycle_counting: true,
            // Not `ALL_SAFE`: with the return-stack-buffer and fast-dispatch
            // handlers enabled a guest indirect-branch loop cannot be stopped
            // at all, and a random corpus finds one within a few thousand
            // trials. See `a_runaway_guest_*` below.
            optimizations: optimization::INTERRUPTIBLE,
            ..VmOptions::default()
        },
    );
    let jit = vm.raw();

    // Half the corpus is uniform noise, which mostly lands on unallocated
    // encodings. The other half is single- and double-bit flips of *valid*
    // instructions, which is where the near-miss decoder paths are -- the
    // reserved-field and unpredictable-operand cases that a uniform draw
    // almost never reaches.
    let seeds: Vec<u32> = vec![
        a64::movz(0, 0x1234, 0),
        a64::add_shifted(2, 0, 1, 0, 4),
        a64::ldr_imm(1, 4, 8),
        a64::stp_imm(0, 1, 4, 16),
        a64::ldr_reg(6, 4, 5),
        a64::ldxr(1, 4),
        a64::stxr(2, 1, 4),
        a64::add_vec_4s(2, 0, 1),
        a64::fadd_d(2, 0, 1),
        a64::fcvtzs_x_from_d(5, 5),
        a64::mrs_tpidr_el0(0),
        a64::bl(3),
        a64::ret(30),
        a64::b_cond(1, -2),
        a64::svc(0),
        a64::YIELD,
    ];

    let mut halts = std::collections::BTreeMap::<u32, usize>::new();
    for trial in 0..trials {
        let words: Vec<u32> = if trial % 2 == 0 {
            (0..WORDS).map(|_| next() as u32).collect()
        } else {
            (0..WORDS)
                .map(|_| {
                    let mut w = seeds[(next() as usize) % seeds.len()];
                    let flips = 1 + (next() % 2);
                    for _ in 0..flips {
                        w ^= 1u32 << (next() % 32);
                    }
                    w
                })
                .collect()
        };
        if std::env::var_os("OD_FUZZ_TRACE").is_some() {
            eprintln!("start {trial}: {words:08X?}");
        }
        vm.with_ctx(|c| {
            c.code = words.clone();
            c.exceptions.clear();
            c.svc.clear();
            c.ticks_remaining = 2_000;
            c.ticks_used = 0;
        });
        // SAFETY: `jit` is live and not executing; the code behind it changed,
        // so every translation of it must go.
        unsafe { od_jit_clear_cache(jit) };
        vm.set_pc(CODE_BASE);
        // Registers are left as the previous trial ended, which is free extra
        // entropy for the operands.

        let mut hr = 0;
        for _ in 0..8 {
            hr = vm.run();
            if hr & (OD_HALT_CACHE_INVALIDATION | harness::HALT_DONE) != 0 {
                // SAFETY: `jit` is live and not executing.
                unsafe { od_jit_clear_halt(jit, hr) };
                if hr & harness::HALT_DONE != 0 {
                    break;
                }
                continue;
            }
            if hr != 0 {
                // SAFETY: as above -- clear so the next trial starts clean.
                unsafe { od_jit_clear_halt(jit, hr) };
                break;
            }
            if vm.with_ctx(|c| c.ticks_remaining) == 0 {
                break;
            }
        }
        if std::env::var_os("OD_FUZZ_TRACE").is_some() {
            eprintln!("trial {trial}: {words:08X?} -> {hr:#010X}");
        }
        *halts.entry(hr).or_default() += 1;

        assert_eq!(
            vm.with_ctx(|c| c.max_depth),
            1,
            "trial {trial}: callbacks nested, words {words:02X?}"
        );
    }
    println!("fuzz(fastmem={fastmem}): {trials} trials, halt reasons {halts:02X?}");
}


/// Spawns `name` as a child of this test binary without waiting for it.
fn spawn_child(name: &str) -> std::process::Child {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env("OD_HOSTILE_CHILD", "1")
        .spawn()
        .expect("spawn child")
}

/// Waits up to `secs` for `child`, killing it and returning `None` on timeout.
fn wait_up_to(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(st) => return Some(st),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

/// Exit code the runaway child uses when neither escape worked.
const EXIT_WEDGED: i32 = 9;

/// A one-instruction guest loop: `BR X30` with `X30` pointing at itself.
/// Runs with a 2,000-cycle budget and a watchdog that halts from another
/// thread after 300 ms -- both of the escapes a host has.
///
/// If neither works the main thread never comes back, so a second watchdog
/// ends the process with [`EXIT_WEDGED`]. Without it the wedged child outlives
/// whatever spawned it, and on Windows an orphan that inherited a pipe holds
/// that pipe open for good -- which is how a mutation run stops dead.
fn runaway_guest(optimizations: u32) {
    // BR X30                            D61F03C0
    let code = vec![a64::br(30)];
    assert_eq!(code[0], 0xD61F_03C0);

    let vm = Vm::new(
        code,
        VmOptions {
            cycle_counting: true,
            optimizations,
            ..VmOptions::default()
        },
    );
    vm.set_reg(30, CODE_BASE);
    vm.start(2_000);

    let jit = vm.raw() as usize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        // SAFETY: the jit outlives this thread -- the main thread is blocked
        // inside `od_jit_run` on it, and `od_jit_halt` only sets an atomic
        // flag, which dynarmic documents as safe from any thread.
        unsafe { od_jit_halt(jit as *mut std::ffi::c_void, OD_HALT_USER5) };
    });
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        println!("runaway: neither the cycle budget nor od_jit_halt was honoured");
        std::process::exit(EXIT_WEDGED);
    });

    // SAFETY: `vm.raw()` is live.
    let hr = unsafe { od_jit_run(vm.raw()) };
    let remaining = vm.with_ctx(|c| c.ticks_remaining);
    println!(
        "runaway(optimizations={optimizations:#06X}) returned {hr:#010X},          {remaining} cycles left"
    );
    // Returning at all is the point. Either escape counts: a halt bit means the
    // watchdog's `od_jit_halt` was seen, and an exhausted budget means the
    // cycle counter was.
    assert!(
        hr != 0 || remaining == 0,
        "the run returned without either escape firing"
    );
}

#[test]
fn a_runaway_guest_is_stoppable_without_the_two_unchecked_terminal_handlers() {
    if is_child() {
        runaway_guest(optimization::INTERRUPTIBLE);
        return;
    }
    let mut child =
        spawn_child("a_runaway_guest_is_stoppable_without_the_two_unchecked_terminal_handlers");
    let st = wait_up_to(&mut child, 60).expect("the child never exited at all");
    assert_ne!(
        st.code(),
        Some(EXIT_WEDGED),
        "with ReturnStackBuffer and FastDispatch cleared, a BR-to-self loop \
         must be stopped by the cycle budget or by od_jit_halt"
    );
    assert!(st.success(), "the child failed for some other reason: {st}");
}

#[test]
fn a_runaway_guest_is_unstoppable_with_dynarmics_default_optimizations() {
    // This documents a defect in the pin, not a property we want.
    //
    // `EmitTerminalImpl(IR::Term::PopRSBHint)` and
    // `EmitTerminalImpl(IR::Term::FastDispatchHint)` jump to terminal handlers
    // (`a64_emit_x64.cpp`, `GenTerminalHandlers`) that transfer straight to the
    // next block's entry point. Neither handler reads `cycles_remaining` and
    // neither reads `halt_reason`. So a guest `BR`/`RET` loop whose target
    // stays in the return-stack buffer or the fast-dispatch cache ignores the
    // step budget *and* ignores `od_jit_halt` from another thread, and the host
    // thread is lost for good. Global Constraint 11: guest code is untrusted,
    // and this is a denial of service it can reach in one instruction.
    //
    // If this test ever starts failing, the pin has been fixed or patched and
    // `optimization::INTERRUPTIBLE` can be retired.
    if is_child() {
        runaway_guest(optimization::ALL_SAFE);
        return;
    }
    let mut child =
        spawn_child("a_runaway_guest_is_unstoppable_with_dynarmics_default_optimizations");
    let st = wait_up_to(&mut child, 60).expect("the child never exited at all");
    assert_eq!(
        st.code(),
        Some(EXIT_WEDGED),
        "the runaway guest returned: the pin's unchecked terminal handlers \
         appear to be fixed, so optimization::INTERRUPTIBLE is no longer needed"
    );
}
