//! What loading `libroblox.so` actually *costs*, measured rather than assumed.
//!
//! # Why this is a separate test binary
//!
//! Commit charge is a **per-process** quantity and `cargo test` runs one binary's tests as parallel
//! threads, so a mapping made by one test appears in another's delta. `omni-mem` solved this the
//! same way: its own binary, plus a [`SERIAL`] mutex held for the whole of each test. Run with
//! `cargo test -p omni-elf --release --test loader_commit -- --nocapture` to read the numbers.
//!
//! # What these tests are for
//!
//! D11 predicts that `libroblox.so`'s text and rodata stay file-backed and shared between instances,
//! and that only `PT_GNU_RELRO`, `.data` and `.bss` become private. That prediction is why the
//! multi-instance requirement is achievable at all, so it is asserted against a measured number, and
//! every number is printed for the record.
#![cfg(target_os = "windows")]

mod common;

use std::sync::Mutex;

use common::fixture::{fixture, load, measuring, mib, GRAND_TOTAL, IMPORTS, INIT_ARRAY_ENTRIES};
use omni_elf::loader::LoaderConfig;
use omni_mem::CommitPolicy;
use omni_platform::vm;

/// Held for the whole of each test, so only one test is mapping at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// The window size the loader uses unless told otherwise.
const DEFAULT_WINDOW: usize = omni_elf::DEFAULT_RELOCATION_WINDOW;

#[test]
fn peak_and_steady_commit_charge_are_measured_and_reported() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = fixture() else { return };
    let before = vm::process_commit_charge().expect("commit charge") as i64;
    let object = load(&f, &measuring());
    let after = vm::process_commit_charge().expect("commit charge") as i64;
    let s = &object.stats;

    let peak = s.peak_commit_delta().expect("measured");
    let steady = after - before;
    let space = f.space.stats();

    eprintln!("\n=== libroblox.so commit charge, window {} KiB ===", DEFAULT_WINDOW / 1024);
    eprintln!("  file-backed mapped   {:>10.3} MiB", mib(s.file_backed_bytes as i64));
    eprintln!("  private anonymous    {:>10.3} MiB", mib(s.anonymous_bytes as i64));
    eprintln!(
        "  after map + decode   {:>10.3} MiB  (mapping plus the unpacked relocation tables)",
        mib(s.decode_commit_delta().expect("measured"))
    );
    eprintln!("  peak commit delta    {:>10.3} MiB", mib(peak));
    eprintln!("  steady commit delta  {:>10.3} MiB", mib(steady));
    eprintln!("  guest space private  {:>10.3} MiB", mib(space.committed as i64));
    eprintln!("  guest space file     {:>10.3} MiB", mib(space.file_backed as i64));
    eprintln!("  largest window       {:>10} bytes", s.relocations.largest_window);
    eprintln!("  windows              {:>10}", s.relocations.windows);
    eprintln!("  windowed bytes       {:>10.3} MiB", mib(s.relocations.windowed_bytes as i64));
    eprintln!("  wall time            {:>10.1?}", s.wall_time);
    eprintln!(
        "    map {:.1?}  bind {:.1?}  decode {:.1?}  relocate {:.1?}  protect {:.1?}",
        s.map_time, s.bind_time, s.decode_time, s.relocate_time, s.protect_time
    );

    // D11 predicts text and rodata stay file-backed and only relro, .data and .bss become private.
    // 103.6 MB of text and rodata against 5.2 MB relro + 0.3 MB .data + 11.6 MB .bss.
    assert!(
        s.file_backed_bytes > 100 * 1024 * 1024,
        "the text and rodata segment must stay file-backed: {} bytes",
        s.file_backed_bytes
    );
    assert!(
        steady < 30 * 1024 * 1024,
        "steady-state commit charge for a 109 MB library must be far below its size, was {:.3} MiB",
        mib(steady)
    );
    // The relocation windows never make more than one window writable at a time.
    assert!(
        s.relocations.largest_window <= DEFAULT_WINDOW,
        "a window of {} bytes exceeds the configured {DEFAULT_WINDOW}",
        s.relocations.largest_window
    );
    // And the *transient* charge the sweep adds is bounded by the writable image, not by the
    // library. Measured from the point where the relocation tables are already decoded, so the
    // loader's own 13 MB of host `Elf64_Rela` records are excluded and what is left is the guest's
    // copy-on-write charge. A loader that protected the whole 109 MB library at once would show
    // about 104 MiB here while passing every correctness test in `loader_m1.rs`.
    let transient = s.commit_peak.expect("measured") as i64
        - s.commit_after_decode.expect("measured") as i64;
    eprintln!("  transient (post-decode) {:>7.3} MiB", mib(transient));
    assert!(
        transient < 8 * 1024 * 1024,
        "the relocation sweep transiently charged {:.3} MiB of commit; windows are not bounding it",
        mib(transient)
    );

    object.unload(&f.space).expect("unload");
    let released = vm::process_commit_charge().expect("commit charge") as i64;
    eprintln!("  after unload         {:>10.3} MiB", mib(released - before));
}


#[test]
fn loading_and_unloading_repeatedly_returns_all_commit_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = fixture() else { return };
    // One warm-up load, so that the allocator's own high-water mark is not counted as a leak.
    load(&f, &LoaderConfig::default()).unload(&f.space).expect("warm-up unload");

    let baseline = vm::process_commit_charge().expect("commit charge") as i64;
    let mut bases = Vec::new();
    for round in 0..4 {
        let object = load(&f, &LoaderConfig::default());
        assert_eq!(object.stats.relocations.applied, GRAND_TOTAL, "round {round}");
        assert_eq!(object.init_array.len(), INIT_ARRAY_ENTRIES, "round {round}");
        assert_eq!(object.imports.unresolved.len(), IMPORTS, "round {round}");
        bases.push(object.base);
        object.unload(&f.space).expect("unload");

        let stats = f.space.stats();
        assert_eq!(stats.mapped, 0, "round {round}: mappings survived the unload");
        assert_eq!(stats.committed, 0, "round {round}: commit survived the unload");
        assert_eq!(stats.file_backed, 0, "round {round}: views survived the unload");

        let now = vm::process_commit_charge().expect("commit charge") as i64;
        assert!(
            (now - baseline).abs() < 2 * 1024 * 1024,
            "round {round}: commit charge did not return to baseline: {:+.3} MiB",
            mib(now - baseline)
        );
    }
    eprintln!(
        "4 load/unload cycles, commit back to baseline each time; bases {:#x?}",
        bases.iter().map(|b| *b as u64).collect::<Vec<_>>()
    );
    f.space.close().expect("close the guest space");
}

#[test]
fn the_relocation_window_bounds_the_transient_charge_and_costs_time() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = fixture() else { return };
    eprintln!("\n=== relocation window trade-off (release build recommended) ===");
    eprintln!("  window     windows   relocate     peak delta   steady delta   largest window");
    for window in [4 * 1024usize, 16 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024, 8 * 1024 * 1024] {
        let before = vm::process_commit_charge().expect("commit charge") as i64;
        let config = LoaderConfig {
            relocation_window: window,
            measure_commit: true,
            ..LoaderConfig::default()
        };
        let object = load(&f, &config);
        let after = vm::process_commit_charge().expect("commit charge") as i64;
        let s = &object.stats.relocations;
        assert_eq!(s.applied, GRAND_TOTAL);
        assert!(
            s.largest_window <= window,
            "window {window}: a {} byte window slipped through",
            s.largest_window
        );
        eprintln!(
            "  {:>7} {:>10} {:>10.1?} {:>12.3} MiB {:>10.3} MiB {:>12}",
            window,
            s.windows,
            object.stats.relocate_time,
            mib(object.stats.peak_commit_delta().unwrap_or(0)),
            mib(after - before),
            s.largest_window
        );
        object.unload(&f.space).expect("unload");
    }
}

#[test]
fn lazy_bss_costs_nothing_until_something_touches_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = fixture() else { return };
    // 11.6 MB of `.bss`. Eager commit charges it at load; lazy charges only what relocations reach,
    // which for this library is nothing at all — no relocation targets `.bss`.
    let mut deltas = Vec::new();
    for policy in [CommitPolicy::Eager, CommitPolicy::Lazy] {
        let before = vm::process_commit_charge().expect("commit charge") as i64;
        let object = load(
            &f,
            &LoaderConfig { bss_commit: policy, measure_commit: true, ..LoaderConfig::default() },
        );
        let after = vm::process_commit_charge().expect("commit charge") as i64;
        assert_eq!(object.stats.relocations.applied, GRAND_TOTAL);
        assert_eq!(
            object.stats.relocations.committed_for_relocation, 0,
            "no relocation of libroblox.so targets .bss"
        );
        deltas.push(after - before);
        eprintln!("bss {policy:?}: steady commit {:+.3} MiB", mib(after - before));
        object.unload(&f.space).expect("unload");
    }
    assert!(
        deltas[0] - deltas[1] > 10 * 1024 * 1024,
        "eager .bss must cost about 11.6 MB more than lazy: {:+.3} vs {:+.3} MiB",
        mib(deltas[0]),
        mib(deltas[1])
    );
}
