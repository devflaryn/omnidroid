//! Which test binaries in this crate do not run off Windows, and saying so out loud.
//!
//! `loader_commit.rs` begins with `#![cfg(target_os = "windows")]`, because `omni-platform`'s Linux
//! and macOS backends are structural: every operation returns a typed `Unsupported` error, so a test
//! that maps anything cannot pass there. That gating is the right call — the alternative is a suite
//! that fails everywhere for a reason already documented — but it has an honesty hazard attached,
//! and the hazard is not a style one.
//!
//! A `cfg`-gated file compiles to **nothing**. On Linux that binary reports `0 tests` and passes,
//! so the headline memory result of the whole branch — 16.7 MiB of commit charge per loaded
//! instance — is silently not measured, in a run that looks green.
//!
//! So the fact is a named test rather than an absence. On a non-Windows target it announces itself;
//! on Windows it checks that the list below is still true, so the announcement cannot go stale while
//! claiming to be accurate.
//!
//! The production code in this crate is genuinely `cfg`-free (Global Constraint 4). This is about
//! the tests. The parsing suites — the golden values, the APS2 decoder, the hostile-input sweep —
//! are portable and do run everywhere; it is the loader's *cost* measurements that cannot.

use std::path::{Path, PathBuf};

/// Every test file in this crate that is gated to Windows, and therefore runs nowhere else.
const WINDOWS_ONLY: [&str; 1] = ["loader_commit.rs"];

/// Test files in this crate that are *not* gated, and so really do run everywhere.
const PORTABLE: [&str; 6] = [
    "all_libraries.rs",
    "aps2_errors.rs",
    "libroblox_golden.rs",
    "loader_hostile.rs",
    "loader_m1.rs",
    "windows_only.rs",
];

const GATE: &str = r#"#![cfg(target_os = "windows")]"#;

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Whether a file carries the gate as an attribute of its own, rather than merely mentioning the
/// text — which this file does, twice, and which a `contains` check cannot tell apart.
fn is_gated(text: &str) -> bool {
    text.lines().any(|line| line.trim() == GATE)
}

/// The list above is accurate: each named file carries the gate, and each portable one does not.
#[test]
fn the_windows_only_list_is_accurate() {
    for name in WINDOWS_ONLY {
        let path = tests_dir().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            is_gated(&text),
            "{name} is listed as Windows-only but does not carry `{GATE}`; either the gate was \
             removed, in which case delete it from WINDOWS_ONLY, or this list is lying"
        );
    }
    for name in PORTABLE {
        let path = tests_dir().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!is_gated(&text), "{name} is listed as portable but carries `{GATE}`");
    }

    // And the lists together cover every test file, so a new gated file cannot be added silently.
    let mut found: Vec<String> = std::fs::read_dir(tests_dir())
        .expect("read the tests directory")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            name.ends_with(".rs").then_some(name)
        })
        .collect();
    found.sort();
    let mut expected: Vec<String> =
        WINDOWS_ONLY.iter().chain(PORTABLE.iter()).map(|s| (*s).to_string()).collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "a test file was added or removed without updating this list; every test file in the crate \
         has to be accounted for as gated or portable, or the gating stops being visible"
    );
}

/// Announce, on a target where those three binaries run nothing at all.
#[cfg(not(target_os = "windows"))]
#[test]
fn the_windows_only_suites_did_not_run_on_this_target() {
    use std::io::Write;
    // Straight to the process's stderr: `eprintln!` is captured by libtest and then discarded for a
    // passing test, which is exactly the outcome this exists to prevent.
    let notice = format!(
        "\n\
         ============================================================================\n\
         omni-elf: {} of {} test files did not run on {}.\n\
         Skipped entirely: {}\n\
         Gated to Windows because omni-platform's unix backend returns\n\
         `Unsupported` from every virtual-memory operation. The per-instance\n\
         commit-charge result this branch is about was NOT measured here.\n\
         ============================================================================\n",
        WINDOWS_ONLY.len(),
        WINDOWS_ONLY.len() + PORTABLE.len(),
        std::env::consts::OS,
        WINDOWS_ONLY.join(", "),
    );
    let _ = std::io::stderr().write_all(notice.as_bytes());
    let _ = std::io::stderr().flush();
}
