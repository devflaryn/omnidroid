//! Commit-charge measurements on Linux: the mirror of `vm_commit_charge.rs`.
//!
//! # What "commit charge" is on Linux
//!
//! [`vm::process_commit_charge`] is the sum of this process's VMAs that carry `VM_ACCOUNT` (`ac` in
//! `/proc/self/smaps`), which is exactly what the kernel adds to `Committed_AS` for this process's
//! own mappings -- the per-process quantity that `vm.overcommit_memory = 2` enforces against
//! `CommitLimit`, and the counterpart of Windows' `PrivateUsage`. See `src/vm/linux.rs`.
//!
//! # Why every measurement here runs inside one `#[test]`
//!
//! The Windows file serialises its tests with a mutex and that is enough there. It is not enough
//! here: libtest starts each test on a thread of its own, and a Linux thread stack is a writable
//! private mapping, so **a test thread starting is 2 MiB of `VM_ACCOUNT`** -- twice the tolerance --
//! landing in whichever measurement happens to be in progress. A mutex serialises the measurements
//! but not the spawns. So the measurements are functions, run in sequence by one test, in a binary
//! whose only other thread is libtest's main thread.
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
//! * On Windows page tables are charged too; on Linux they are not (`VM_ACCOUNT` is per page of
//!   mapping), so the Linux deltas are exact multiples of the page and the tolerance is slack.
//! * The test harness itself allocates a little around each test.
//!
//! 1 MiB is therefore about eight times the largest expected genuine overhead in these tests and
//! far below the effect being measured (0 against 4 GiB, and 64 MiB against 0). A tolerance that
//! also admitted, say, a whole extra committed region would not be testing anything.
#![cfg(target_os = "linux")]

use omni_platform::vm::{self, Protection};

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

fn the_process_reports_a_plausible_commit_charge_and_working_set() {
    let commit = charge();
    let ws = working_set();
    eprintln!("baseline commit charge = {}, working set = {}", mib(commit as i64), mib(ws as i64));
    // A live process always has both. Zero would mean the query silently failed.
    assert!(commit > 0, "commit charge reported as 0");
    assert!(ws > 0, "working set reported as 0");
    assert!(
        commit < 64 * 1024 * MIB,
        "commit charge of {} is implausible for a test process",
        mib(commit as i64)
    );
}

fn a_four_gibibyte_reservation_adds_no_commit_charge() {
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

fn committing_64_mib_raises_commit_charge_and_decommitting_returns_it() {
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

fn a_read_only_file_view_costs_almost_no_commit_charge() {
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

// ================================================================== `process_memory`
//
// The snapshot `/proc/self/statm` is answered from. Each field is shown to be **the quantity it is
// named for** by making that quantity -- and only that one -- move: a reservation moves the
// address space and nothing else, touching committed memory moves the private resident set,
// reading a file view moves the shareable one. A field wired to the wrong counter moves on the
// wrong line.

/// Address space moves in allocation granules, and the Rust heap and the test harness reserve
/// their own segments while a test runs; 16 MiB is far above that and far below the 4 GiB
/// measured against it.
const ADDRESS_TOLERANCE: u64 = 16 * MIB;

fn memory() -> vm::ProcessMemory {
    vm::process_memory().expect("read this process's memory")
}

/// **The snapshot's counters are the ones the single calls report**, and they sit inside each
/// other the way a resident set sits inside an address space.
fn process_memory_reports_the_counters_the_single_calls_do() {
    let ws_before = working_set();
    let charge_before = charge();
    let snapshot = memory();
    let ws_after = working_set();
    let charge_after = charge();
    eprintln!(
        "process memory: address space {}, resident {} (shareable {}), commit {}, \
         code {:#x}..{:#x}",
        mib(snapshot.address_space as i64),
        mib(snapshot.resident as i64),
        mib(snapshot.resident_shared as i64),
        mib(snapshot.commit_charge as i64),
        snapshot.executable_code.start,
        snapshot.executable_code.end,
    );
    // The same counters as `process_working_set` and `process_commit_charge`, taken an instant
    // apart with nothing allocating in between.
    assert_close(delta(ws_before, snapshot.resident), 0, "resident against process_working_set");
    assert_close(delta(ws_after, snapshot.resident), 0, "resident against process_working_set");
    let against_charge = "commit against process_commit_charge";
    assert_close(delta(charge_before, snapshot.commit_charge), 0, against_charge);
    assert_close(delta(charge_after, snapshot.commit_charge), 0, against_charge);
    // Parts of wholes.
    assert!(snapshot.resident_shared <= snapshot.resident, "{snapshot:?}");
    assert!(snapshot.resident <= snapshot.address_space, "{snapshot:?}");
    assert!(snapshot.commit_charge <= snapshot.address_space, "{snapshot:?}");
    // Every process has file pages resident -- its own executable's and libc's at the least -- and
    // a file page nothing has written is shareable. Zero would be a counter that was never read.
    assert!(snapshot.resident_shared > 0, "no shareable page resident: {snapshot:?}");
}

/// **A reservation is address space and nothing else; touched private memory is resident and
/// private; a read file view is resident and shareable.**
fn process_memory_moves_each_field_with_the_quantity_it_names() {
    let region = 64 * MIB as usize;

    // A reservation: address space, and no residency and no commit.
    let before = memory();
    let reservation =
        vm::reserve(4 * 1024 * MIB as usize, vm::allocation_granularity()).expect("reserve 4 GiB");
    let reserved = memory();
    let space_delta = delta(before.address_space, reserved.address_space);
    eprintln!("reserve 4 GiB: address space delta {}", mib(space_delta));
    assert!(
        (space_delta - (4 * 1024 * MIB) as i64).unsigned_abs() <= ADDRESS_TOLERANCE,
        "a 4 GiB reservation moved the address space by {}",
        mib(space_delta)
    );
    let reservation_commit = delta(before.commit_charge, reserved.commit_charge);
    assert_close(reservation_commit, 0, "commit for a reservation");
    assert!(
        delta(before.resident, reserved.resident) < 8 * MIB as i64,
        "a reservation made {} resident",
        mib(delta(before.resident, reserved.resident))
    );

    // Committed and touched: resident, and private -- the shareable part does not move.
    let ptr = reservation.offset_ptr(0, region).expect("in range");
    // SAFETY: `ptr` covers the first 64 MiB of a live reservation this test owns.
    unsafe { vm::commit(ptr, region, Protection::ReadWrite).expect("commit 64 MiB") };
    // SAFETY: the whole range is committed read-write by the call above.
    unsafe { std::ptr::write_bytes(ptr, 0xa5, region) };
    let touched = memory();
    eprintln!(
        "commit and touch 64 MiB: resident delta {}, shareable delta {}, commit delta {}",
        mib(delta(reserved.resident, touched.resident)),
        mib(delta(reserved.resident_shared, touched.resident_shared)),
        mib(delta(reserved.commit_charge, touched.commit_charge)),
    );
    assert!(
        delta(reserved.resident, touched.resident) > 48 * MIB as i64,
        "touching 64 MiB moved the resident set by only {}",
        mib(delta(reserved.resident, touched.resident))
    );
    assert!(
        delta(reserved.resident_shared, touched.resident_shared).unsigned_abs() < 8 * MIB,
        "touching 64 MiB of private memory moved the shareable resident set by {}",
        mib(delta(reserved.resident_shared, touched.resident_shared))
    );
    assert_close(
        delta(reserved.commit_charge, touched.commit_charge),
        64 * MIB as i64,
        "commit for 64 MiB committed",
    );
    vm::release(reservation).expect("release");

    // A file view, read: resident and shareable, and no commit.
    let page = vm::page_size();
    let span = 4096 * page; // 16 MiB
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    std::fs::create_dir_all(&dir).expect("create fixture directory");
    let path = dir.join(format!("process-memory-view-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x5au8; span]).expect("write a 16 MiB fixture");
    let file = vm::open_file_for_mapping(&path, omni_platform::vm::MapExecutability::NonExecutable)
        .expect("open for mapping");
    let view = vm::reserve_placeholder(span, vm::allocation_granularity()).expect("a placeholder");
    // SAFETY: the reservation is one unreplaced placeholder of exactly `span` bytes.
    unsafe { vm::map_file(&file, 0, span, view.as_ptr(), Protection::Read).expect("map it") };
    let mapped = memory();
    // SAFETY: the whole span is a live read-only view.
    let sum: u64 = unsafe {
        std::slice::from_raw_parts(view.as_ptr(), span).iter().map(|b| u64::from(*b)).sum()
    };
    assert_eq!(sum, 0x5a * span as u64, "the view did not contain the file's bytes");
    let read = memory();
    eprintln!(
        "read a 16 MiB file view: resident delta {}, shareable delta {}, commit delta {}",
        mib(delta(mapped.resident, read.resident)),
        mib(delta(mapped.resident_shared, read.resident_shared)),
        mib(delta(mapped.commit_charge, read.commit_charge)),
    );
    assert!(
        delta(mapped.resident_shared, read.resident_shared) > 12 * MIB as i64,
        "reading a 16 MiB file view moved the shareable resident set by only {}",
        mib(delta(mapped.resident_shared, read.resident_shared))
    );
    assert_close(delta(mapped.commit_charge, read.commit_charge), 0, "commit for a read file view");
    // SAFETY: the whole view is unmapped exactly once and nothing refers to it any more.
    unsafe { vm::unmap_and_release(view.as_ptr(), span).expect("unmap and release") };
    drop(file);
    let _ = std::fs::remove_file(&path);
}

/// A function of this test binary, whose address has to be inside its executable's code.
#[inline(never)]
fn a_function_of_this_executable() -> u32 {
    std::hint::black_box(7)
}

/// A writable static of this test binary, whose address has to be outside its code.
static A_STATIC_OF_THIS_EXECUTABLE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// **The code span is this executable's code: its functions are in it and its data is not.**
///
/// The second half is the over-correction's detector: a span taken from the whole image (every
/// `PT_LOAD`) rather than its `PF_X` segments would contain the static too.
fn the_executable_code_span_holds_this_executables_code_and_not_its_data() {
    let code = memory().executable_code;
    let function = a_function_of_this_executable as *const () as usize;
    let data = std::ptr::addr_of!(A_STATIC_OF_THIS_EXECUTABLE) as usize;
    assert_eq!(a_function_of_this_executable(), 7);
    A_STATIC_OF_THIS_EXECUTABLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "code {:#x}..{:#x}, a function at {function:#x}, a static at {data:#x}",
        code.start, code.end
    );
    assert!(code.start < code.end, "an empty code span: {code:?}");
    assert!(code.contains(&function), "{function:#x} is not in the code span {code:x?}");
    assert!(!code.contains(&data), "{data:#x}, a static, is in the code span {code:x?}");
    // This process's own seam function is in it too: omni-platform is linked into the executable.
    assert!(code.contains(&(vm::process_memory as *const () as usize)), "{code:x?}");
}


/// **The placeholder path is charged and returned exactly as the plain path is**, and decommit gives
/// back the resident pages as well as the charge -- the two halves `MEM_DECOMMIT` has, and the half
/// `MADV_DONTNEED` lacks (see `src/vm/linux.rs` for that measurement).
///
/// Also the copy-on-write file view: charged its full size the moment it is mapped writable, before
/// a byte is written, which is `PAGE_WRITECOPY`'s measured behaviour on Windows.
fn the_placeholder_path_and_a_writable_view_are_charged_as_on_windows() {
    let region = 64 * MIB as usize;
    let placeholder =
        vm::reserve_placeholder(region, vm::allocation_granularity()).expect("reserve 64 MiB");

    let before = charge();
    let before_ws = working_set();
    // SAFETY: the reservation is exactly one unreplaced placeholder of `region` bytes.
    unsafe { vm::commit_placeholder(placeholder.as_ptr(), region, Protection::ReadWrite) }
        .expect("commit the placeholder");
    let committed = charge();
    eprintln!("commit_placeholder 64 MiB: commit delta {}", mib(delta(before, committed)));
    assert_close(delta(before, committed), 64 * MIB as i64, "commit_placeholder of 64 MiB");

    // SAFETY: committed read-write just above.
    unsafe { std::ptr::write_bytes(placeholder.as_ptr(), 0x3c, region) };
    let touched_ws = working_set();
    assert!(
        delta(before_ws, touched_ws) > 48 * MIB as i64,
        "touching 64 MiB moved the working set by only {}",
        mib(delta(before_ws, touched_ws))
    );

    // SAFETY: the range is exactly the region commit_placeholder produced, and nothing refers to it.
    unsafe { vm::decommit_to_placeholder(placeholder.as_ptr(), region) }
        .expect("decommit to a placeholder");
    let returned = charge();
    let returned_ws = working_set();
    eprintln!(
        "decommit_to_placeholder 64 MiB: commit delta {}, working-set delta {}",
        mib(delta(committed, returned)),
        mib(delta(touched_ws, returned_ws))
    );
    assert_close(delta(before, returned), 0, "commit then decommit_to_placeholder");
    assert!(
        delta(touched_ws, returned_ws) < -(48 * MIB as i64),
        "decommit must give the resident pages back as well as the charge; the working set moved \
         by only {}",
        mib(delta(touched_ws, returned_ws))
    );
    vm::release(placeholder).expect("release");

    // A writable, copy-on-write file view: charged in full at map time.
    let span = 4 * MIB as usize;
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    std::fs::create_dir_all(&dir).expect("create fixture directory");
    let path = dir.join(format!("commit-cow-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x11u8; span]).expect("write a 4 MiB fixture");
    let file = vm::open_file_for_mapping(&path, omni_platform::vm::MapExecutability::NonExecutable)
        .expect("open for mapping");
    let view = vm::reserve_placeholder(span, vm::allocation_granularity()).expect("reserve");
    let before = charge();
    // SAFETY: one unreplaced placeholder of exactly `span` bytes.
    unsafe { vm::map_file(&file, 0, span, view.as_ptr(), Protection::ReadWrite) }.expect("map rw");
    let mapped = charge();
    eprintln!("map a 4 MiB copy-on-write view: commit delta {}", mib(delta(before, mapped)));
    assert_close(delta(before, mapped), 4 * MIB as i64, "a 4 MiB copy-on-write view, untouched");
    // SAFETY: the whole view is unmapped exactly once and nothing refers to it any more.
    unsafe { vm::unmap_and_release(view.as_ptr(), span) }.expect("unmap and release");
    assert_close(delta(before, charge()), 0, "the view's charge comes back with the view");
    drop(file);
    let _ = std::fs::remove_file(&path);
}

/// Every measurement above, in sequence, on one test thread. See the module docs for why.
#[test]
fn commit_charge_measurements_with_no_other_test_thread_alive() {
    eprintln!("--- the_process_reports_a_plausible_commit_charge_and_working_set");
    the_process_reports_a_plausible_commit_charge_and_working_set();
    eprintln!("--- a_four_gibibyte_reservation_adds_no_commit_charge");
    a_four_gibibyte_reservation_adds_no_commit_charge();
    eprintln!("--- committing_64_mib_raises_commit_charge_and_decommitting_returns_it");
    committing_64_mib_raises_commit_charge_and_decommitting_returns_it();
    eprintln!("--- a_read_only_file_view_costs_almost_no_commit_charge");
    a_read_only_file_view_costs_almost_no_commit_charge();
    eprintln!("--- process_memory_reports_the_counters_the_single_calls_do");
    process_memory_reports_the_counters_the_single_calls_do();
    eprintln!("--- process_memory_moves_each_field_with_the_quantity_it_names");
    process_memory_moves_each_field_with_the_quantity_it_names();
    eprintln!("--- the_executable_code_span_holds_this_executables_code_and_not_its_data");
    the_executable_code_span_holds_this_executables_code_and_not_its_data();
    eprintln!("--- the_placeholder_path_and_a_writable_view_are_charged_as_on_windows");
    the_placeholder_path_and_a_writable_view_are_charged_as_on_windows();
}
