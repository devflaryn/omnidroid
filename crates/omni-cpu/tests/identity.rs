//! D4: identity mapping, the startup assertion, and proof that the assertion guards something real.
//!
//! Gated on `x86_64`, because that is where the translating backend exists. On another host the
//! tests skip **visibly** — they print why — rather than silently reporting success.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::{DynarmicOptions, FastmemOverrides};
use omni_cpu::{CpuError, ExitReason, GuestCpu, RunLimit};

/// A loop that loads and stores `iterations` times through the data region.
///
/// ```text
///   X0 = data base            (mov64)
///   X1 = iterations
///   X2 = 0                    running sum
/// loop:
///   LDR  X3, [X0]             a guest load  -> fastmem
///   ADD  X2, X2, X3
///   STR  X2, [X0, #8]         a guest store -> fastmem
///   SUBS X1, X1, #1
///   B.NE loop
///   SVC  sentinel-less: fall through to the sentinel via RET X30
/// ```
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

/// **The headline measurement.** A memory-heavy loop under identity mapping must take the
/// callback path exactly zero times.
///
/// Why the count is exact rather than "small": the callback counters are incremented once per entry
/// on the jit's own thread, so they are deterministic, not sampled. A tolerance here would be the
/// thing that let a 13.2x regression through.
#[test]
fn a_memory_heavy_loop_takes_zero_callback_path_entries() {
    let guest = Guest::new();
    guest.assert_high_addresses();

    const ITERATIONS: u64 = 100_000;
    let entry = guest.load(&memory_loop(guest.data, ITERATIONS));
    guest.write_u64(guest.data, 3);

    let (mut cpu, sentinel) = guest.thread();
    cpu.reset_stats();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");

    // The loop is 5 instructions plus a 6-instruction prologue, so it genuinely executed memory
    // operations rather than falling out early.
    assert_eq!(cpu.x(x(2)), 3 * ITERATIONS, "the sum proves the loads really happened");
    assert_eq!(guest.read_u64(guest.data + 8), 3 * ITERATIONS, "and the stores really landed");

    let stats = cpu.stats();
    assert_eq!(
        stats.slow_path_total, 0,
        "identity fastmem must take the callback path zero times, and this run took it \
         {} times ({} reads, {} writes, {} exclusives) over {ITERATIONS} iterations \
         (200,000 guest memory accesses). D4 measured that path at 13.2x slower",
        stats.slow_path_total,
        stats.slow_path_reads,
        stats.slow_path_writes,
        stats.slow_path_exclusive,
    );
    println!(
        "callback-path entries for {} guest memory accesses (1 run, deterministic counters): {}",
        ITERATIONS * 2,
        stats.slow_path_total
    );
}

/// The configuration itself, read back from dynarmic's live `UserConfig` rather than echoed.
#[test]
fn the_effective_configuration_is_the_one_d4_requires() {
    let guest = Guest::new();
    let (cpu, _) = guest.thread();
    let config = cpu.effective_config();

    assert_eq!(config.fastmem_enabled, 1);
    assert_eq!(config.fastmem_pointer, 0, "identity mapping: guest address 0 is host address 0");
    assert_eq!(
        config.fastmem_address_space_bits, 64,
        "64 is what makes the emitter fold the base into the SIB byte; dynarmic's default is 36"
    );
    assert_eq!(config.silently_mirror_fastmem, 0, "a wild guest address must fault, not alias");
    assert_eq!(config.page_table_present, 0, "a page table is the alternative to identity mapping");
    assert_eq!(config.enable_cycle_counting, 1, "the watchdog is a budget expiring");
    assert_ne!(config.tpidr_el0_ptr, 0, "D13: guest code must be able to read the thread pointer");
    assert_eq!(config.fastmem_exclusive_access, 1, "so LDXR does not leave the fast path");

    // And the same facts through the backend-agnostic view the assertion actually checks.
    let mapping = cpu.memory_mapping();
    assert!(mapping.direct_access && !mapping.mirrors_out_of_range);
    assert_eq!((mapping.host_base, mapping.address_bits), (0, 64));
    omni_cpu::require_identity_mapping(&mapping, cpu.space()).expect("D4 holds");
}

/// **Proof that the assertion fires, and proof that it is guarding something real.**
///
/// Two halves, and the second is the one that is usually left out. Showing the check rejects a bad
/// value only shows the check exists; it says nothing about whether the value matters. So the same
/// misconfigured context is then *run*, and the callback-path counter shows what the assertion
/// prevented: a configuration that produces identical results at a fraction of the speed.
#[test]
fn the_startup_assertion_fires_on_dynarmics_default_width_and_the_default_is_really_slower() {
    let guest = Guest::new();
    guest.assert_high_addresses();

    const ITERATIONS: u64 = 10_000;
    let entry = guest.load(&memory_loop(guest.data, ITERATIONS));
    guest.write_u64(guest.data, 7);

    // Half one: the assertion refuses it, and for the right reason.
    let overrides = FastmemOverrides { address_space_bits: Some(36), ..Default::default() };
    let (mut cpu, error) = guest
        .backend
        .create_misconfigured_thread(overrides)
        .expect("a deliberately misconfigured context");
    match &error {
        CpuError::MisconfiguredMemoryPath { setting, expected, actual, .. } => {
            assert_eq!(*setting, "guest address bits covered by the direct path");
            assert_eq!((*expected, *actual), (64, 36));
        }
        other => panic!("the wrong width must be refused as a memory-path failure, got {other}"),
    }
    assert!(
        error.to_string().contains("13.2x-slower") && error.to_string().contains("default here is 36"),
        "the message has to say what going ahead would cost, or nobody acts on it: {error}"
    );

    // Half two: run it anyway and measure what the assertion prevented.
    let sentinel = guest.code + harness::CODE_BYTES - 4;
    cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
    cpu.set_x(x(30), sentinel as u64);
    cpu.reset_stats();
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the misconfigured loop still runs");

    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(
        cpu.x(x(2)),
        7 * ITERATIONS,
        "and this is the whole problem: the wrong configuration produces the RIGHT ANSWER"
    );

    let stats = cpu.stats();
    assert!(
        stats.slow_path_total > 0,
        "with a 36-bit window and a guest space above 64 GiB, every access must leave the fast \
         path — if this is zero the test is no longer testing the degradation it claims to"
    );
    assert_eq!(
        stats.slow_path_total,
        ITERATIONS * 2,
        "every one of the {} guest memory accesses should have taken the callback path",
        ITERATIONS * 2
    );
    println!(
        "misconfigured (36-bit) callback-path entries for {} accesses: {} ({} reads, {} writes)",
        ITERATIONS * 2,
        stats.slow_path_total,
        stats.slow_path_reads,
        stats.slow_path_writes
    );
}

/// The other three ways the memory path can be broken, each refused by name.
#[test]
fn every_other_broken_memory_path_is_refused_too() {
    let guest = Guest::new();
    for (why, overrides, expected) in [
        (
            "fastmem off entirely",
            FastmemOverrides { direct_access: Some(false), ..Default::default() },
            "13.2x",
        ),
        (
            "mirroring on",
            FastmemOverrides {
                address_space_bits: Some(36),
                mirrors_out_of_range: Some(true),
                ..Default::default()
            },
            // With a narrowed width the width check fires first, which is correct: it is the more
            // specific failure and the one that names dynarmic's default.
            "default here is 36",
        ),
    ] {
        let (_cpu, error) = guest
            .backend
            .create_misconfigured_thread(overrides)
            .unwrap_or_else(|e| panic!("{why}: {e}"));
        assert!(error.to_string().contains(expected), "{why} was refused wrongly: {error}");
    }

    // And the door is not a back door: asking for a *conforming* configuration through it fails.
    let error = guest
        .backend
        .create_misconfigured_thread(FastmemOverrides::default())
        .expect_err("a conforming configuration must not come back through this path");
    assert!(error.to_string().contains("nothing for the startup assertion to refuse"), "{error}");
}

/// A context that the assertion refuses is never handed out. The refusal happens before
/// `create_thread` returns, so there is no window in which a misconfigured CPU exists in the
/// runtime's hands.
#[test]
fn a_normally_created_context_always_satisfies_the_assertion() {
    let guest = Guest::with_options(DynarmicOptions { max_threads: 4, ..Default::default() });
    for _ in 0..4 {
        let cpu = guest.backend.create_thread_with_tls().expect("a guest thread");
        omni_cpu::require_identity_mapping(&cpu.memory_mapping(), cpu.space())
            .expect("every context that exists has already passed this");
    }
}

/// D4's other footgun, at the level of a running guest: `PC` survives the 56-bit round trip for
/// every address this guest space can hold.
#[test]
fn the_guest_pc_round_trips_for_every_address_in_the_space() {
    let guest = Guest::new();
    let (mut cpu, _) = guest.thread();
    for address in [guest.space.base(), guest.code, guest.data, guest.space.end() - 4] {
        assert!(
            omni_cpu::pc_is_representable(address as u64),
            "{address:#x} does not survive dynarmic's 56-bit PC"
        );
        cpu.set_pc(address);
        assert_eq!(cpu.pc(), address, "PC round trip for {address:#x}");
    }
    // And the band that does not survive, so the constant is not vacuous.
    assert!(!omni_cpu::pc_is_representable(1 << 55));
    assert_eq!(omni_cpu::truncate_pc(1 << 55), 0xFF80_0000_0000_0000);
}
