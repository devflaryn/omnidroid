//! Commit-charge measurements.
//!
//! Commit charge is the resource that decides how many guest instances fit on a machine (D10), so
//! these tests assert what memory operations actually cost rather than that they returned `Ok`.
//! Every observed number is printed; run with `cargo test -- --nocapture` to read them.
//!
//! # Why this is a separate test binary
//!
//! Commit charge is a *per-process* quantity, and `cargo test` runs the tests of one binary as
//! parallel threads in a single process. A reservation made by one test would then appear in
//! another test's delta. These tests therefore live in their own binary and additionally serialise
//! against each other with [`SERIAL`], so that exactly one of them is allocating at a time.
//!
//! # Tolerance
//!
//! Deltas are compared with a tolerance of 1 MiB. Reasons, not superstition:
//!
//! * Page tables are charged too, at 8 bytes per committed 4 KB page — 128 KiB for the 64 MiB
//!   case. The measurement in D10 shows this directly: 256 MiB committed cost 256.50 MiB.
//! * The test harness itself allocates a little around each test.
//!
//! 1 MiB is therefore about eight times the largest expected genuine overhead in these tests and
//! far below the effect being measured (0 against 4 GiB, and 64 MiB against 0). A tolerance that
//! also admitted, say, a whole extra committed region would not be testing anything.
#![cfg(target_os = "windows")]

use std::sync::Mutex;

use omni_platform::vm::{self, Protection};

/// Held for the whole of each test, so only one test is allocating at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const MIB: u64 = 1024 * 1024;
const TOLERANCE: u64 = MIB;

fn charge() -> u64 {
    vm::process_commit_charge().expect("read the process commit charge")
}

fn working_set() -> u64 {
    vm::process_working_set().expect("read the process working set")
}

fn mib(bytes: i64) -> String {
    format!("{:.3} MiB", bytes as f64 / MIB as f64)
}

fn delta(before: u64, after: u64) -> i64 {
    after as i64 - before as i64
}

fn assert_close(actual: i64, expected: i64, what: &str) {
    let difference = (actual - expected).unsigned_abs();
    assert!(
        difference <= TOLERANCE,
        "{what}: expected {} +/- {}, observed {}",
        mib(expected),
        mib(TOLERANCE as i64),
        mib(actual)
    );
}

#[test]
fn the_process_reports_a_plausible_commit_charge_and_working_set() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let commit = charge();
    let ws = working_set();
    eprintln!("baseline commit charge = {}, working set = {}", mib(commit as i64), mib(ws as i64));
    // A live Windows process always has both. Zero would mean the query silently failed.
    assert!(commit > 0, "commit charge reported as 0");
    assert!(ws > 0, "working set reported as 0");
    assert!(
        commit < 64 * 1024 * MIB,
        "commit charge of {} is implausible for a test process",
        mib(commit as i64)
    );
}

#[test]
fn a_four_gibibyte_reservation_adds_no_commit_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let size = 4 * 1024 * 1024 * 1024usize;

    let before = charge();
    let before_ws = working_set();
    let reservation = vm::reserve(size, vm::allocation_granularity()).expect("reserve 4 GiB");
    let after = charge();
    let after_ws = working_set();

    let commit_delta = delta(before, after);
    eprintln!(
        "reserve 4 GiB: commit {} -> {} (delta {}), working set delta {}",
        mib(before as i64),
        mib(after as i64),
        mib(commit_delta),
        mib(delta(before_ws, after_ws))
    );

    // Address space is free; commit charge tracks MEM_COMMIT, not MEM_RESERVE. The whole memory
    // design depends on this being true, so it is asserted rather than assumed.
    assert_close(commit_delta, 0, "4 GiB reservation");

    // Same again for a placeholder reservation: placeholders are not a cheaper or more expensive
    // kind of reservation, they are a differently *replaceable* one.
    let before = charge();
    let placeholder =
        vm::reserve_placeholder(size, vm::allocation_granularity()).expect("reserve 4 GiB");
    let placeholder_delta = delta(before, charge());
    eprintln!("reserve 4 GiB placeholder: commit delta {}", mib(placeholder_delta));
    assert_close(placeholder_delta, 0, "4 GiB placeholder reservation");

    let before = charge();
    vm::release(reservation).expect("release");
    vm::release(placeholder).expect("release placeholder");
    eprintln!("release both: commit delta {}", mib(delta(before, charge())));
}

#[test]
fn committing_64_mib_raises_commit_charge_and_decommitting_returns_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let region = 64 * MIB as usize;
    let reservation =
        vm::reserve(4 * 1024 * MIB as usize, vm::allocation_granularity()).expect("reserve 4 GiB");
    let ptr = reservation.offset_ptr(0, region).expect("in range");

    let before = charge();
    let before_ws = working_set();
    // SAFETY: `ptr` covers the first 64 MiB of a live 4 GiB reservation this test owns, and
    // nothing else refers to it.
    unsafe { vm::commit(ptr, region, Protection::ReadWrite).expect("commit 64 MiB") };
    let committed = charge();
    let committed_ws = working_set();

    let commit_delta = delta(before, committed);
    eprintln!(
        "commit 64 MiB: commit delta {}, working-set delta {} (untouched)",
        mib(commit_delta),
        mib(delta(before_ws, committed_ws))
    );
    assert_close(commit_delta, 64 * MIB as i64, "committing 64 MiB");

    // Commit is charged before anything is touched: the working set has barely moved even though
    // 64 MiB of commit has been spent. This is the asymmetry that makes commit, not working set,
    // the thing to budget.
    assert!(
        delta(before_ws, committed_ws) < 8 * MIB as i64,
        "the working set grew by {} for an untouched 64 MiB commit",
        mib(delta(before_ws, committed_ws))
    );

    // Touching it moves the working set without moving commit charge any further.
    // SAFETY: the whole range is committed read-write by the call above.
    unsafe { std::ptr::write_bytes(ptr, 0xa5, region) };
    let touched = charge();
    let touched_ws = working_set();
    eprintln!(
        "touch 64 MiB: commit delta since commit {}, working-set delta since commit {}",
        mib(delta(committed, touched)),
        mib(delta(committed_ws, touched_ws))
    );
    assert_close(delta(committed, touched), 0, "touching already-committed memory");
    assert!(
        delta(committed_ws, touched_ws) > 48 * MIB as i64,
        "the working set only grew by {} after touching 64 MiB",
        mib(delta(committed_ws, touched_ws))
    );

    // MEM_DECOMMIT is the only primitive that gives the commit charge back (D10).
    // SAFETY: the range is committed memory of a live reservation and nothing refers to it.
    unsafe { vm::decommit(ptr, region).expect("decommit 64 MiB") };
    let decommitted = charge();
    let decommit_delta = delta(touched, decommitted);
    eprintln!(
        "decommit 64 MiB: commit delta {}, net against baseline {}",
        mib(decommit_delta),
        mib(delta(before, decommitted))
    );
    assert_close(decommit_delta, -(64 * MIB as i64), "decommitting 64 MiB");
    assert_close(delta(before, decommitted), 0, "net effect of commit then decommit");

    // The address space is still reserved: the same range commits again, and reads back zeroed.
    // SAFETY: the range is still inside the live reservation.
    unsafe {
        vm::commit(ptr, region, Protection::ReadWrite).expect("recommit after decommit");
        assert_eq!(
            std::slice::from_raw_parts(ptr, 4096),
            &[0u8; 4096],
            "a recommitted page must read back zero-filled"
        );
    }

    vm::release(reservation).expect("release");
}

#[test]
fn a_read_only_file_view_costs_almost_no_commit_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // This is what makes ~109 MB of libroblox.so shareable between instances: a shared,
    // file-backed read-only view is charged for its page tables and nothing else, even after every
    // page has been read (D11, 4.9).
    let page = vm::page_size();
    let pages = 1024; // 4 MiB, the size measured in the research
    let span = pages * page;
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    std::fs::create_dir_all(&dir).expect("create fixture directory");
    let path = dir.join(format!("commit-view-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x5au8; span]).expect("write a 4 MiB fixture");

    let file = vm::open_file_for_mapping(&path, omni_platform::vm::MapExecutability::NonExecutable)
        .expect("open for mapping");
    let reservation =
        vm::reserve_placeholder(span, vm::allocation_granularity()).expect("reserve placeholder");

    let before = charge();
    // SAFETY: the reservation is one unreplaced placeholder of exactly `span` bytes.
    unsafe {
        vm::map_file(&file, 0, span, reservation.as_ptr(), Protection::Read).expect("map 4 MiB");
    }
    let mapped = charge();
    eprintln!("map a 4 MiB read-only file view: commit delta {}", mib(delta(before, mapped)));

    // Read every page, so the pages are genuinely resident and not merely promised.
    // SAFETY: the whole span is a live read-only view.
    let sum: u64 = unsafe {
        std::slice::from_raw_parts(reservation.as_ptr(), span).iter().map(|b| u64::from(*b)).sum()
    };
    assert_eq!(sum, 0x5a * span as u64, "the view did not contain the file's bytes");
    let read = charge();
    eprintln!(
        "after reading all 4 MiB: commit delta since map {}, total {}",
        mib(delta(mapped, read)),
        mib(delta(before, read))
    );

    // Page tables for 1024 pages are 8 KiB; 1 MiB of tolerance is generous and still two orders of
    // magnitude below the 4 MiB a private copy would have cost.
    assert_close(delta(before, read), 0, "a 4 MiB shared read-only view");

    // SAFETY: the whole view is unmapped exactly once and nothing refers to it any more.
    unsafe { vm::unmap_and_release(reservation.as_ptr(), span).expect("unmap and release") };
    drop(file);
    let _ = std::fs::remove_file(&path);
}
