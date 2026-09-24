//! Linux behaviour of the virtual-memory seam: the mirror of `vm_windows.rs`, test for test, with
//! every place the answer differs on Linux changed to the Linux answer and saying why.
//!
//! What stays identical is the seam's contract -- the bytes at a mapped address, the granularity
//! each operation enforces, and which *variant* a wrong call is refused with. What changes is the
//! code a refusal carries: on Windows it is the kernel's (487, 87); on Linux the kernel refuses
//! none of these calls (`munmap` of a released address succeeds, and unmaps whatever is there now),
//! so the refusal is the backend ledger's, and it carries `EINVAL` (see `src/vm/linux.rs`).
//!
//! These tests assert *behaviour*, not the absence of an error: the bytes that appear at a mapped
//! address, the exact granularity each operation enforces, and the specific OS error code that a
//! wrong call produces. A test that only checked `is_ok()` would pass against an implementation
//! that mapped the wrong file offset.
//!
//! Commit-charge measurements live in `vm_commit_charge.rs`, in a separate test binary, because
//! commit charge is a per-process number and the test harness runs tests in one process in
//! parallel.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use omni_platform::vm::{
    self, MapExecutability, Protection, Reservation, ReservationKind, VmError,
};

const PAGE: usize = 4096;
/// A 64 KiB block, which the Windows tests call `GRANULARITY` because it is Windows' allocation
/// granularity. On Linux there is no such granularity (it is the page), so it is only a size here,
/// kept so that every test below reserves and maps what its Windows twin does.
const GRANULARITY: usize = 65536;
/// `EINVAL`: the code the backend ledger refuses with, and `mmap`'s code for a misaligned offset.
const EINVAL: u32 = 22;
/// `EACCES`: Linux's code for `PROT_EXEC` refused, and for a writable shared view of a read-only
/// descriptor.
const EACCES: u32 = 13;

// -------------------------------------------------------------------------------------------
// Fixture: a file whose every 4 KB page is individually identifiable, built by the test.
// -------------------------------------------------------------------------------------------

/// The exact bytes of page `index` of the fixture file.
///
/// Each page begins with a tag naming its own index and is then filled with a byte derived from
/// the index, so that a whole-page comparison catches an off-by-one-page mapping error, not just a
/// wholly wrong address.
fn expected_page(index: usize) -> Vec<u8> {
    let tag = format!("OMNIDROID FIXTURE PAGE {index:06} ");
    let mut page = vec![(index as u8).wrapping_mul(31).wrapping_add(7); PAGE];
    page[..tag.len()].copy_from_slice(tag.as_bytes());
    page
}

/// Write a fixture file of `pages` identifiable 4 KB pages and return its path.
///
/// The name is unique per call: the files are opened `FILE_SHARE_READ` only, so two tests running
/// in parallel must not touch the same one.
fn fixture(pages: usize) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    fs::create_dir_all(&dir).expect("create fixture directory");
    let path = dir.join(format!(
        "fixture-{}-{}.bin",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut data = Vec::with_capacity(pages * PAGE);
    for index in 0..pages {
        data.extend_from_slice(&expected_page(index));
    }
    fs::write(&path, &data).expect("write fixture file");
    path
}

/// Read `len` bytes from a mapped address.
///
/// # Safety
///
/// `[ptr, ptr + len)` must be readable mapped memory.
unsafe fn read_mapped(ptr: *const u8, len: usize) -> Vec<u8> {
    std::slice::from_raw_parts(ptr, len).to_vec()
}

// -------------------------------------------------------------------------------------------
// Granularity and symbol availability
// -------------------------------------------------------------------------------------------

#[test]
fn page_size_is_4096_and_allocation_granularity_is_the_page() {
    // Linux has no 64 KB reservation granularity: an mmap base is page-aligned.
    assert_eq!(vm::page_size(), PAGE);
    assert_eq!(vm::allocation_granularity(), PAGE);
}

#[test]
fn the_placeholder_path_is_available_and_resolves_nothing_at_runtime() {
    // Windows asks kernelbase.dll for three symbols. Linux's placeholder path is mmap(MAP_FIXED),
    // linked at build time, so there is nothing to resolve -- and the answer must be the truth,
    // which is yes: every test below that maps at a chosen address is the evidence.
    assert!(vm::placeholder_api_available());
    assert!(vm::placeholder_api_symbols().is_empty());
}

// -------------------------------------------------------------------------------------------
// Reservation
// -------------------------------------------------------------------------------------------

#[test]
fn a_four_gibibyte_reservation_succeeds_and_is_granularity_aligned() {
    let size = 4 * 1024 * 1024 * 1024usize;
    let reservation = vm::reserve(size, GRANULARITY).expect("reserve 4 GiB");
    assert_eq!(reservation.len(), size);
    assert_eq!(reservation.kind(), ReservationKind::Plain);
    assert_eq!(
        reservation.base() % GRANULARITY,
        0,
        "reservation base {:#x} is not 64 KB-aligned",
        reservation.base()
    );
    assert_eq!(reservation.end(), reservation.base() + size);
    vm::release(reservation).expect("release 4 GiB");
}

#[test]
fn reserve_honours_alignments_above_the_allocation_granularity() {
    // Over-reserved and trimmed with munmap, which Linux allows and Windows does not.
    for align in [1 << 20, 2 << 20, 16 << 20usize] {
        let reservation = vm::reserve(align * 2, align)
            .unwrap_or_else(|e| panic!("reserve {align} bytes aligned: {e}"));
        assert_eq!(
            reservation.base() % align,
            0,
            "base {:#x} is not aligned to {align}",
            reservation.base()
        );
        vm::release(reservation).expect("release");
    }
}

#[test]
fn releasing_a_reservation_twice_fails_with_invalid_address() {
    // Windows refuses this with ERROR_INVALID_ADDRESS (487). Linux would not refuse it at all:
    // munmap of an address released a moment ago succeeds, and unmaps whatever the kernel has put
    // there since. The ledger refuses it, with EINVAL.
    let reservation = vm::reserve(GRANULARITY, GRANULARITY).expect("reserve");
    let stale = reservation;
    vm::release(reservation).expect("first release");
    let err = vm::release(stale).expect_err("second release must fail");
    assert_eq!(
        err.os_error().map(|e| e.code()),
        Some(EINVAL),
        "expected the ledger's EINVAL, got {err}"
    );
}

#[test]
fn a_range_outside_a_reservation_is_rejected_with_both_bounds() {
    let reservation = vm::reserve(GRANULARITY, GRANULARITY).expect("reserve");
    let err = reservation.offset_ptr(GRANULARITY, PAGE).expect_err("past the end");
    assert!(matches!(err, VmError::OutsideReservation { .. }), "{err}");
    let text = err.to_string();
    assert!(text.contains(&format!("{:x}", reservation.base())), "{text}");
    assert!(text.contains(&format!("{:x}", reservation.end())), "{text}");

    assert!(reservation.contains(reservation.as_ptr(), GRANULARITY));
    assert!(!reservation.contains(reservation.as_ptr(), GRANULARITY + 1));
    vm::release(reservation).expect("release");
}

// -------------------------------------------------------------------------------------------
// Commit, protect, decommit at 4 KB granularity
// -------------------------------------------------------------------------------------------

#[test]
fn a_single_page_inside_a_large_reservation_commits_protects_and_decommits() {
    let reservation = vm::reserve(256 * 1024 * 1024, GRANULARITY).expect("reserve 256 MiB");

    // Deliberately at a 64 KB-*misaligned* page boundary (+0x3000), which is the whole point of
    // D10's finding that only reservation base addresses are 64 KB-constrained.
    let target = reservation.offset_ptr(3 * PAGE, PAGE).expect("in range");
    let neighbour = reservation.offset_ptr(4 * PAGE, PAGE).expect("in range");
    assert_eq!(target as usize % GRANULARITY, 3 * PAGE);

    // SAFETY: both pages are inside a live reservation this test owns and nothing else refers to
    // them, which is the contract of every vm call below.
    unsafe {
        vm::commit(target, PAGE, Protection::ReadWrite).expect("commit one page");
        vm::commit(neighbour, PAGE, Protection::ReadWrite).expect("commit the neighbour");

        std::ptr::write_bytes(target, 0x07, PAGE);
        std::ptr::write_bytes(neighbour, 0x5a, PAGE);
        assert_eq!(read_mapped(target, PAGE), vec![0x07; PAGE]);

        // Protection is 4 KB-granular: the target drops to read-only while the neighbour stays
        // writable, and the data survives the change.
        vm::protect(target, PAGE, Protection::Read).expect("protect one page read-only");
        assert_eq!(read_mapped(target, PAGE), vec![0x07; PAGE]);
        std::ptr::write_bytes(neighbour, 0x5b, PAGE);
        assert_eq!(read_mapped(neighbour, PAGE), vec![0x5b; PAGE]);

        vm::protect(target, PAGE, Protection::ReadWrite).expect("protect back");

        // Decommit is 4 KB-granular too, and it does not disturb the neighbour.
        vm::decommit(target, PAGE).expect("decommit one page");
        assert_eq!(read_mapped(neighbour, PAGE), vec![0x5b; PAGE]);

        // Re-committing a decommitted page yields zeroes, matching anonymous mmap and
        // MADV_DONTNEED — which is what the guest expects.
        vm::commit(target, PAGE, Protection::ReadWrite).expect("recommit");
        assert_eq!(
            read_mapped(target, PAGE),
            vec![0x00; PAGE],
            "a recommitted page must read back zero-filled"
        );
    }

    vm::release(reservation).expect("release");
}

#[test]
fn commit_rejects_a_misaligned_address_or_size() {
    let reservation = vm::reserve(GRANULARITY, GRANULARITY).expect("reserve");
    // SAFETY: the pointer is inside a live reservation; both calls are rejected by the seam's
    // argument validation before any OS call happens.
    unsafe {
        let ptr = reservation.as_ptr().add(1);
        let err = vm::commit(ptr, PAGE, Protection::ReadWrite).expect_err("misaligned address");
        assert!(matches!(err, VmError::Misaligned { what: "address", .. }), "{err}");

        let err = vm::commit(reservation.as_ptr(), 100, Protection::ReadWrite)
            .expect_err("misaligned size");
        assert!(matches!(err, VmError::Misaligned { what: "size", value: 100, .. }), "{err}");
    }
    vm::release(reservation).expect("release");
}

// -------------------------------------------------------------------------------------------
// Placeholders
// -------------------------------------------------------------------------------------------

/// Split a placeholder into `[head, target, tail]` so that every piece has a descriptor and the
/// whole reservation can be given back at the end of a test.
fn split_three(
    reservation: &Reservation,
    target_offset: usize,
    target_size: usize,
) -> (Option<Reservation>, Reservation, Option<Reservation>) {
    let target = vm::split_placeholder(reservation, target_offset, target_size)
        .expect("split the target piece");
    let head = if target_offset > 0 {
        Some(
            reservation
                .subrange(0, target_offset, ReservationKind::Placeholder)
                .expect("head descriptor"),
        )
    } else {
        None
    };
    let tail_offset = target_offset + target_size;
    let tail = if tail_offset < reservation.len() {
        Some(
            reservation
                .subrange(tail_offset, reservation.len() - tail_offset, ReservationKind::Placeholder)
                .expect("tail descriptor"),
        )
    } else {
        None
    };
    (head, target, tail)
}

#[test]
fn a_placeholder_splits_at_4kb_and_a_file_view_lands_at_a_4kb_file_offset() {
    // The load-bearing measurement of D11: when a view replaces a placeholder, *both* the base
    // address and the file offset are 4 KB-granular, not 64 KB.
    let path = fixture(64);
    let file = vm::open_file_for_mapping(&path, MapExecutability::Executable)
        .expect("open the fixture for mapping");
    assert_eq!(file.len(), 64 * PAGE as u64);
    assert_eq!(file.executability(), MapExecutability::Executable);
    assert_eq!(file.path(), path.as_path());

    let reservation = vm::reserve_placeholder(1024 * 1024, GRANULARITY).expect("reserve 1 MiB");
    assert_eq!(reservation.kind(), ReservationKind::Placeholder);

    // A single page, five pages in: a 4 KB-aligned, 64 KB-misaligned base.
    let (head, target, tail) = split_three(&reservation, 5 * PAGE, PAGE);
    assert_eq!(target.base(), reservation.base() + 5 * PAGE);
    assert_eq!(target.base() % GRANULARITY, 5 * PAGE);
    assert_eq!(target.len(), PAGE);

    // SAFETY: `target` is exactly one unreplaced placeholder piece, produced immediately above.
    unsafe {
        // File offset 9 pages in: 4 KB-aligned and 64 KB-misaligned as well.
        vm::map_file(&file, 9 * PAGE as u64, PAGE, target.as_ptr(), Protection::Read)
            .expect("map one page at a 4 KB file offset");
        assert_eq!(
            read_mapped(target.as_ptr(), PAGE),
            expected_page(9),
            "the mapped page is not the file's page 9"
        );

        // Unmapping preserves the placeholder, so the same address takes another view.
        vm::unmap(target.as_ptr(), PAGE).expect("unmap, preserving the placeholder");
        vm::map_file(&file, 10 * PAGE as u64, PAGE, target.as_ptr(), Protection::Read)
            .expect("remap into the preserved placeholder");
        assert_eq!(read_mapped(target.as_ptr(), PAGE), expected_page(10));

        vm::unmap_and_release(target.as_ptr(), PAGE).expect("unmap and release");
    }

    for piece in [head, tail].into_iter().flatten() {
        vm::release(piece).expect("release a placeholder piece");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn a_multi_page_view_maps_every_page_in_order() {
    let path = fixture(32);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let span = 6 * PAGE;
    let reservation = vm::reserve_placeholder(span, GRANULARITY).expect("reserve placeholder");
    // The reservation is exactly the view size, so no split is needed: it is already an
    // exact-size placeholder.
    // SAFETY: the reservation is one unreplaced placeholder of exactly `span` bytes.
    unsafe {
        vm::map_file(&file, 12 * PAGE as u64, span, reservation.as_ptr(), Protection::Read)
            .expect("map six pages");
        for page in 0..6 {
            let ptr = reservation.as_ptr().add(page * PAGE);
            assert_eq!(
                read_mapped(ptr, PAGE),
                expected_page(12 + page),
                "page {page} of the view should be file page {}",
                12 + page
            );
        }
        vm::unmap_and_release(reservation.as_ptr(), span).expect("unmap and release");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn private_commit_replaces_a_placeholder_and_can_be_returned_to_one() {
    let path = fixture(GRANULARITY / PAGE);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(GRANULARITY, GRANULARITY).expect("reserve");

    // SAFETY: `reservation` is one unreplaced placeholder of exactly GRANULARITY bytes, and every
    // call below operates on exactly that range.
    unsafe {
        vm::commit_placeholder(reservation.as_ptr(), GRANULARITY, Protection::ReadWrite)
            .expect("private commit into a placeholder");
        std::ptr::write_bytes(reservation.as_ptr(), 0x33, GRANULARITY);
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), vec![0x33; PAGE]);

        // Returning it to a placeholder is what lets the same guest address later hold a file
        // mapping instead of private memory.
        vm::decommit_to_placeholder(reservation.as_ptr(), GRANULARITY)
            .expect("decommit back to a placeholder");
        vm::map_file(&file, 0, GRANULARITY, reservation.as_ptr(), Protection::Read)
            .expect("the range must be a placeholder again");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(0));
        vm::unmap_and_release(reservation.as_ptr(), GRANULARITY).expect("unmap and release");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn replacing_a_placeholder_requires_an_exact_size_placeholder() {
    let path = fixture(16);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(1024 * 1024, GRANULARITY).expect("reserve 1 MiB");

    // SAFETY: the reservation is a live 1 MiB placeholder; both calls are expected to fail and
    // nothing is dereferenced.
    unsafe {
        let err = vm::map_file(&file, 0, PAGE, reservation.as_ptr(), Protection::Read)
            .expect_err("a 4 KB view into a 1 MiB placeholder must fail");
        assert!(matches!(err, VmError::PlaceholderNotExactSize { size: PAGE, .. }), "{err}");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
        assert!(err.to_string().contains("split_placeholder"), "{err}");

        let err = vm::commit_placeholder(reservation.as_ptr(), PAGE, Protection::ReadWrite)
            .expect_err("a 4 KB private commit into a 1 MiB placeholder must fail");
        assert!(matches!(err, VmError::PlaceholderNotExactSize { .. }), "{err}");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
    }

    vm::release(reservation).expect("release");
    let _ = fs::remove_file(&path);
}

// -------------------------------------------------------------------------------------------
// Alignment and executability rules for file mapping
// -------------------------------------------------------------------------------------------

#[test]
fn a_misaligned_file_offset_is_rejected_with_the_offending_value() {
    let path = fixture(16);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");

    for offset in [1u64, 512, 1024, 2048, 4095, 4097, 0x1234] {
        // SAFETY: the reservation is a live exact-size placeholder; the call is expected to fail
        // in the seam's own validation and nothing is dereferenced.
        let err = unsafe {
            vm::map_file(&file, offset, PAGE, reservation.as_ptr(), Protection::Read)
                .expect_err("a sub-page file offset can never be mapped")
        };
        match err {
            VmError::Misaligned { what: "file offset", value, required, os_equivalent, .. } => {
                assert_eq!(value, offset);
                assert_eq!(required, PAGE as u64);
                // The condition mmap(2) reports as EINVAL.
                assert_eq!(os_equivalent.code(), EINVAL);
            }
            other => panic!("expected a Misaligned error for offset {offset}, got {other}"),
        }
        assert!(err.to_string().contains(&offset.to_string()), "{err}");
    }

    // A 4 KB-aligned but 64 KB-misaligned offset, by contrast, is accepted — the finding that
    // decides how guest ELF segments are loaded.
    // SAFETY: the reservation is a live exact-size placeholder.
    unsafe {
        vm::map_file(&file, PAGE as u64, PAGE, reservation.as_ptr(), Protection::Read)
            .expect("a 4 KB-aligned file offset must be accepted");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(1));
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn a_view_past_the_end_of_the_file_is_rejected_with_both_lengths() {
    let path = fixture(4);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(2 * PAGE, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is live; the call is rejected before reaching the OS.
    let err = unsafe {
        vm::map_file(&file, 3 * PAGE as u64, 2 * PAGE, reservation.as_ptr(), Protection::Read)
            .expect_err("a view past the end of the file must fail")
    };
    assert!(matches!(err, VmError::ViewPastEndOfFile { len: 16384, .. }), "{err}");
    assert!(err.to_string().contains("16384"), "{err}");
    vm::release(reservation).expect("release");
    let _ = fs::remove_file(&path);
}

#[test]
fn an_executable_view_requires_a_file_opened_executable() {
    // This is the failure D11 warns appears "much later and far from its cause": the decision is
    // made when the file is opened, so the seam refuses the mapping at the point the mistake is
    // visible.
    let path = fixture(8);
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");

    let plain = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    // SAFETY: the reservation is a live exact-size placeholder; this call fails in the seam's own
    // validation without reaching the OS.
    let err = unsafe {
        vm::map_file(&plain, 0, PAGE, reservation.as_ptr(), Protection::ReadExecute)
            .expect_err("an executable view of a non-executable file must fail")
    };
    assert!(matches!(err, VmError::FileNotOpenedExecutable { .. }), "{err}");
    assert!(err.to_string().contains("Executable"), "{err}");
    drop(plain);

    let executable = vm::open_file_for_mapping(&path, MapExecutability::Executable)
        .expect("open for execution: the kernel accepted a PROT_EXEC probe mapping");
    // SAFETY: as above; this call is expected to succeed and the view is read back and unmapped.
    unsafe {
        vm::map_file(&executable, 2 * PAGE as u64, PAGE, reservation.as_ptr(), Protection::ReadExecute)
            .expect("an executable view of an executable file must succeed");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(2));
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn raising_a_view_to_executable_depends_on_how_the_file_was_opened() {
    // D11's rule, kept on Linux: executability is decided when the file is opened. A view of a
    // NonExecutable file is refused r-x -- with EACCES, Linux's code for PROT_EXEC refused.
    //
    // **One Windows refusal is deliberately not reproduced.** Windows also refuses to raise a view
    // *created* read-only to r-x even from an executable section (87, measured there). Linux allows
    // it, nothing in the runtime relies on the refusal, and a backend that invented it would be
    // reporting a failure this host does not have. So the second block asserts that it works.
    let path = fixture(8);

    let plain = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is a live exact-size placeholder and the view is unmapped below.
    unsafe {
        vm::map_file(&plain, 0, PAGE, reservation.as_ptr(), Protection::Read).expect("map");
        let err = vm::protect(reservation.as_ptr(), PAGE, Protection::ReadExecute)
            .expect_err("a view of a non-executable file must not reach r-x");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EACCES), "expected EACCES, got {err}");
        // And the refusal changed nothing: the page is still readable and still the file's.
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(0));
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }
    drop(plain);

    // A read-only view of an executable file: allowed on Linux (see above).
    let executable =
        vm::open_file_for_mapping(&path, MapExecutability::Executable).expect("open executable");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: as above.
    unsafe {
        vm::map_file(&executable, 0, PAGE, reservation.as_ptr(), Protection::Read).expect("map");
        vm::protect(reservation.as_ptr(), PAGE, Protection::ReadExecute)
            .expect("Linux raises a read-only view of an executable file to r-x");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(0));
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }

    // Mapped r-x, the whole cycle the ELF loader needs works: drop to writable to apply
    // relocations, then back up to executable.
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: as above.
    unsafe {
        vm::map_file(&executable, 0, PAGE, reservation.as_ptr(), Protection::ReadExecute)
            .expect("map an executable view");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(0));

        vm::protect(reservation.as_ptr(), PAGE, Protection::ReadWrite)
            .expect("an executable view can drop to copy-on-write");
        std::ptr::write_bytes(reservation.as_ptr(), 0xc3, 16);

        vm::protect(reservation.as_ptr(), PAGE, Protection::ReadExecute)
            .expect("and come back up to r-x");
        assert_eq!(read_mapped(reservation.as_ptr(), 16), vec![0xc3; 16]);
        assert_eq!(
            read_mapped(reservation.as_ptr().add(16), PAGE - 16),
            expected_page(0)[16..].to_vec(),
            "only the written bytes should have changed"
        );

        vm::protect(reservation.as_ptr(), PAGE, Protection::None)
            .expect("and down to no access");
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }
    // The file itself was never written: the write above privatised its page.
    assert_eq!(fs::read(&path).expect("read back")[..PAGE], expected_page(0)[..]);
    let _ = fs::remove_file(&path);
}

#[test]
fn a_writable_file_view_is_copy_on_write_and_does_not_touch_the_file() {
    // Protection::ReadWrite on a file view is MAP_PRIVATE, the extraction cache being shared between
    // instances and immutable. Writes must privatise the page, not reach the disk.
    let path = fixture(8);
    let original = fs::read(&path).expect("read the fixture back");
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is a live exact-size placeholder and the view is unmapped below.
    unsafe {
        vm::map_file(&file, 3 * PAGE as u64, PAGE, reservation.as_ptr(), Protection::ReadWrite)
            .expect("a copy-on-write view");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(3));
        std::ptr::write_bytes(reservation.as_ptr(), 0xee, PAGE);
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), vec![0xee; PAGE]);
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }
    drop(file);
    assert_eq!(
        fs::read(&path).expect("read the fixture again"),
        original,
        "a copy-on-write view must not modify the file"
    );
    let _ = fs::remove_file(&path);
}

#[test]
fn a_file_view_cannot_be_created_with_no_access() {
    let path = fixture(4);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is live; the call is rejected before reaching the OS.
    let err = unsafe {
        vm::map_file(&file, 0, PAGE, reservation.as_ptr(), Protection::None)
            .expect_err("a no-access view must be refused")
    };
    assert!(matches!(err, VmError::UnsupportedViewProtection { .. }), "{err}");
    assert!(err.to_string().contains("Protection::Read"), "{err} should say what to do instead");
    vm::release(reservation).expect("release");
    let _ = fs::remove_file(&path);
}

#[test]
fn unmap_refuses_an_address_that_is_not_the_base_of_a_view() {
    // Windows unmaps a whole view from its base; there is no partial unmap. Linux could unmap part
    // of one, but the seam's contract is that this call does not -- omni-mem emulates the partial
    // case above the seam on every backend -- so the refusal and its numbers are the same here.
    let path = fixture(8);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let span = 2 * PAGE;
    let reservation = vm::reserve_placeholder(span, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is a live exact-size placeholder; the whole view is unmapped at the
    // end, and the rejected call does not dereference anything.
    unsafe {
        vm::map_file(&file, 0, span, reservation.as_ptr(), Protection::Read).expect("map");
        let interior = reservation.as_ptr().add(PAGE);
        let err = vm::unmap(interior, PAGE).expect_err("a partial unmap must be refused");
        match err {
            VmError::NotViewBase { address, view_base, view_len, offset } => {
                assert_eq!(address, interior as usize);
                assert_eq!(view_base, reservation.base());
                // The error must hand back the numbers needed to emulate a partial unmap.
                assert_eq!(view_len, span);
                assert_eq!(offset, PAGE);
            }
            other => panic!("expected NotViewBase, got {other}"),
        }
        assert!(err.to_string().contains(&span.to_string()), "{err} should state the view length");
        // The view is untouched, so the whole thing still unmaps cleanly.
        vm::unmap_and_release(reservation.as_ptr(), span).expect("unmap and release");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn unmap_refuses_a_view_base_with_a_size_shorter_than_the_view() {
    // The dangerous direction on Windows, where UnmapViewOfFile2 takes no length. The contract is
    // the same on Linux (see above), and both unmap flavours refuse it.
    let path = fixture(16);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let span = 8 * PAGE;

    for (label, short) in [("one page", PAGE), ("all but one page", span - PAGE)] {
        let reservation = vm::reserve_placeholder(span, GRANULARITY).expect("reserve");
        // SAFETY: the reservation is a live exact-size placeholder; the refused calls dereference
        // nothing and the whole view is unmapped at the end of each iteration.
        unsafe {
            vm::map_file(&file, 0, span, reservation.as_ptr(), Protection::Read).expect("map");

            for operation in ["unmap", "unmap_and_release"] {
                let result = if operation == "unmap" {
                    vm::unmap(reservation.as_ptr(), short)
                } else {
                    vm::unmap_and_release(reservation.as_ptr(), short)
                };
                let err = match result {
                    Ok(()) => panic!(
                        "{operation} of {label} out of a {span}-byte view must be refused"
                    ),
                    Err(err) => err,
                };
                match err {
                    VmError::ViewSizeMismatch {
                        operation: reported,
                        address,
                        requested,
                        view_len,
                        surviving,
                    } => {
                        assert_eq!(reported, operation);
                        assert_eq!(address, reservation.base());
                        assert_eq!(requested, short);
                        assert_eq!(view_len, span);
                        assert_eq!(surviving, span - short);
                    }
                    other => panic!("expected ViewSizeMismatch, got {other}"),
                }
            }

            // Nothing was unmapped by the refusals: every page of the view is still readable and
            // still holds the right file bytes. This is what makes the refusal meaningful rather
            // than cosmetic.
            for page in 0..8 {
                assert_eq!(
                    read_mapped(reservation.as_ptr().add(page * PAGE), PAGE),
                    expected_page(page),
                    "page {page} was disturbed by a refused unmap"
                );
            }

            vm::unmap_and_release(reservation.as_ptr(), span).expect("the whole view unmaps");
        }
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn a_views_extent_survives_being_split_into_several_protection_regions() {
    // `protect` on part of a view splits it into several VMAs, as it splits a Windows view into
    // several regions. The ledger's view extent must not be the VMA's, or the short-size refusal
    // would stop working the moment anything had called protect -- which, for a loaded ELF
    // segment, is always.
    let path = fixture(16);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let span = 8 * PAGE;
    let reservation = vm::reserve_placeholder(span, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is a live exact-size placeholder and the whole view is unmapped at
    // the end.
    unsafe {
        vm::map_file(&file, 0, span, reservation.as_ptr(), Protection::Read).expect("map");

        // Chop the view into at least five distinct protection regions.
        vm::protect(reservation.as_ptr().add(PAGE), PAGE, Protection::None).expect("page 1 none");
        vm::protect(reservation.as_ptr().add(3 * PAGE), PAGE, Protection::ReadWrite)
            .expect("page 3 copy-on-write");
        vm::protect(reservation.as_ptr().add(6 * PAGE), PAGE, Protection::None).expect("page 6");

        // A short unmap is still refused, and still knows the view's full length.
        let err = vm::unmap(reservation.as_ptr(), PAGE)
            .expect_err("a short unmap of a split view must still be refused");
        match err {
            VmError::ViewSizeMismatch { view_len, .. } => assert_eq!(
                view_len, span,
                "the extent walk did not cross the protection regions"
            ),
            other => panic!("expected ViewSizeMismatch, got {other}"),
        }

        // And the whole view, split or not, unmaps in one call.
        vm::unmap_and_release(reservation.as_ptr(), span).expect("the whole split view unmaps");
    }
    let _ = fs::remove_file(&path);
}

// -------------------------------------------------------------------------------------------
// Opening files for mapping
// -------------------------------------------------------------------------------------------

#[test]
fn opening_a_missing_file_for_mapping_names_the_path_and_the_code() {
    let path = std::env::temp_dir().join("omnidroid-no-such-file-9f3a1c.bin");
    let _ = fs::remove_file(&path);
    let err = vm::open_file_for_mapping(&path, MapExecutability::Executable)
        .expect_err("a missing file must fail to open");
    assert!(matches!(err, VmError::FileOpen { .. }), "{err}");
    // ENOENT. (`OsError`'s name table is Windows' and prints 2 as ERROR_FILE_NOT_FOUND, which is
    // a coincidence of numbering, so the name is not asserted: see the port notes.)
    assert_eq!(err.os_error().map(|e| e.code()), Some(2), "{err}");
    let text = err.to_string();
    assert!(text.contains(&path.display().to_string()), "{text} should name the path");
    assert!(text.contains("executable"), "{text} should say how it was to be opened");
}

#[test]
fn a_zero_length_file_cannot_be_mapped() {
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    fs::create_dir_all(&dir).expect("create fixture directory");
    let path = dir.join(format!("empty-{}.bin", std::process::id()));
    fs::write(&path, []).expect("write an empty file");
    let err = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable)
        .expect_err("a zero-length file cannot be mapped");
    assert!(matches!(err, VmError::EmptyFile { .. }), "{err}");
    assert!(err.to_string().contains("0 bytes"), "{err}");
    let _ = fs::remove_file(&path);
}

/// Releasing a split placeholder's parent must be refused.
///
/// On Windows the danger is a silent *partial* free. On Linux it is the opposite and worse: `munmap`
/// of the parent's whole range would free every piece, and every view and commit inside them, while
/// the region map still believed they were there. The refusal and its numbers are the same.
#[test]
fn releasing_a_split_placeholder_by_its_parent_is_refused_with_both_extents() {
    let span = 4 * GRANULARITY;
    let reservation = vm::reserve_placeholder(span, GRANULARITY).expect("reserve a placeholder");
    let piece = vm::split_placeholder(&reservation, 0, GRANULARITY).expect("split the first piece");
    assert_eq!(piece.base(), reservation.base());

    let err = vm::release(reservation).expect_err("releasing the split parent must be refused");
    match err {
        VmError::ReleaseExtentMismatch { address, requested, actual } => {
            assert_eq!(address, reservation.base());
            assert_eq!(requested, span, "the caller asked for the whole span");
            assert_eq!(actual, GRANULARITY, "only the first piece starts at that address");
        }
        other => panic!("expected ReleaseExtentMismatch, got {other}"),
    }
    let text = vm::release(reservation).expect_err("still refused").to_string();
    assert!(text.contains(&format!("{span}")), "{text}");
    assert!(text.contains(&format!("{GRANULARITY}")), "{text}");

    // Both legitimate ways still work. First, piece by piece.
    vm::release(piece).expect("release the first piece on its own");
    let rest = reservation
        .subrange(GRANULARITY, span - GRANULARITY, ReservationKind::Placeholder)
        .expect("name the remainder");
    vm::release(rest).expect("release the remainder on its own");

    // Second, merged back together. The merged placeholder's extent is the whole span again, so the
    // extent check passes and one release frees all of it.
    let reservation = vm::reserve_placeholder(span, GRANULARITY).expect("reserve");
    let _piece = vm::split_placeholder(&reservation, GRANULARITY, GRANULARITY).expect("split");
    // SAFETY: the whole span is placeholders this process owns, none of them replaced.
    unsafe { vm::coalesce_placeholders(reservation.as_ptr(), span) }.expect("coalesce");
    vm::release(reservation).expect("one release frees a coalesced placeholder");
}

/// A reservation whose size was not a whole number of pages is still released as a whole.
///
/// The OS rounds a reservation up to a page, so the extent check has to compare against the rounded
/// length or it would refuse every release of an odd-sized reservation.
#[test]
fn releasing_a_reservation_whose_size_was_rounded_up_still_works() {
    let reservation = vm::reserve(PAGE + 1, GRANULARITY).expect("reserve a page and one byte");
    assert_eq!(reservation.len(), PAGE + 1, "the descriptor keeps the requested length");
    vm::release(reservation).expect("release a reservation the OS rounded up");
}

/// The `Reservation` descriptor's edge cases are refused rather than answered wrongly.
///
/// Each of these is a *descriptor* question with no OS call behind it, which is exactly why they
/// went unnoticed: a wrong answer here does not fail, it produces a descriptor that a later
/// `release` or bounds check takes at face value.
#[test]
fn reservation_edges_are_refused_rather_than_answered_wrongly() {
    let reservation = vm::reserve_placeholder(2 * GRANULARITY, GRANULARITY).expect("reserve");

    // A zero-length subrange used to succeed, producing a reservation whose `is_empty()` is true —
    // contradicting that method's own documentation — and which `release` would then hand to
    // `MEM_RELEASE`, whose size argument is 0 anyway: it would free whatever allocation starts at
    // that base. That is the whole reservation when the offset is 0.
    let err = reservation
        .subrange(0, 0, ReservationKind::Placeholder)
        .expect_err("a zero-length subrange must be refused");
    assert!(matches!(err, VmError::ZeroSize { operation: "subrange" }), "{err}");
    assert!(reservation.subrange(GRANULARITY, 0, ReservationKind::Placeholder).is_err());

    // `is_empty()` can therefore keep its promise.
    assert!(!reservation.is_empty(), "no reservation this seam produces is empty");

    // `end()` is exclusive, and `contains` must not admit it. With `len == 0` the arithmetic alone
    // says yes, which would report the first address *past* the reservation as inside it.
    let one_past = reservation.end() as *const u8;
    assert!(!reservation.contains(one_past, 0), "end() is not inside the reservation");
    assert!(!reservation.contains(one_past, 1));
    assert!(!reservation.contains(reservation.as_ptr(), 0), "a zero-length range is nowhere");
    assert!(reservation.contains(reservation.as_ptr(), 1));
    assert!(reservation.contains(reservation.as_ptr(), 2 * GRANULARITY), "the whole extent");
    assert!(!reservation.contains(reservation.as_ptr(), 2 * GRANULARITY + 1), "one byte too many");
    // The last byte is inside; one past it is not.
    assert!(reservation.contains((reservation.end() - 1) as *const u8, 1));

    vm::release(reservation).expect("release");
}

// -------------------------------------------------------------------------------------------
// Linux only: the refusals the ledger exists for, where the kernel would not refuse
// -------------------------------------------------------------------------------------------

/// **A stale release must not unmap what the kernel has handed out since.**
///
/// The Linux-specific half of `releasing_a_reservation_twice_fails_with_invalid_address`, and the
/// reason the ledger exists: the kernel reuses a freed range for the next mapping that fits
/// (measured below), and a `munmap` of the stale descriptor would silently unmap the new owner's
/// memory. Here the new owner is a *larger* reservation over the freed range, committed and
/// written, and it is read back after the refused release.
///
/// **What this cannot defend, on either backend:** a new reservation of exactly the stale one's
/// base *and* length is indistinguishable from it -- the descriptor carries no generation -- so a
/// release through the stale descriptor frees it. Windows' `MEM_RELEASE` has the same property.
/// That case is the caller's to prevent, and `omni-mem` does, by owning every descriptor it makes.
#[test]
fn a_stale_release_does_not_unmap_the_range_the_kernel_handed_out_since() {
    let mut attempts = 0;
    let (stale, fresh) = loop {
        attempts += 1;
        assert!(attempts <= 64, "the kernel never reused a released range in 64 attempts");
        let first = vm::reserve(GRANULARITY, PAGE).expect("reserve");
        let stale = first;
        vm::release(first).expect("release");
        let fresh = vm::reserve(2 * GRANULARITY, PAGE).expect("reserve a larger range");
        if fresh.contains(stale.as_ptr(), GRANULARITY) {
            break (stale, fresh);
        }
        vm::release(fresh).expect("release a range that did not reuse the freed one");
    };
    println!(
        "the kernel placed a new 128 KiB reservation over a freed 64 KiB one on attempt {attempts} \
         (freed base {:#x}, new base {:#x})",
        stale.base(),
        fresh.base()
    );

    // SAFETY: `fresh` is a live reservation this test owns.
    unsafe {
        vm::commit(fresh.as_ptr(), 2 * GRANULARITY, Protection::ReadWrite).expect("commit it all");
        std::ptr::write_bytes(fresh.as_ptr(), 0x6b, 2 * GRANULARITY);
    }
    let err = vm::release(stale).expect_err("the stale descriptor must be refused");
    assert!(
        matches!(err, VmError::ReleaseExtentMismatch { .. })
            || err.os_error().map(|e| e.code()) == Some(EINVAL),
        "{err}"
    );
    // The evidence that the refusal was real rather than cosmetic: had it reached munmap, this read
    // would be a SIGSEGV on the freed half.
    // SAFETY: `fresh` is still committed read-write.
    let back = unsafe { read_mapped(fresh.as_ptr(), 2 * GRANULARITY) };
    assert!(back.iter().all(|&b| b == 0x6b), "the new reservation was disturbed");
    vm::release(fresh).expect("release the live reservation");
}

/// Operations that need a particular kind of range refuse any other kind, before the kernel sees
/// them -- each of these the kernel would otherwise have carried out.
#[test]
fn each_operation_refuses_a_range_of_the_wrong_kind() {
    let path = fixture(4);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let plain = vm::reserve(GRANULARITY, PAGE).expect("reserve");
    let placeholder = vm::reserve_placeholder(GRANULARITY, PAGE).expect("reserve a placeholder");
    // SAFETY: every call is on a live range this test owns; the refused ones reach no kernel call.
    unsafe {
        // `commit` is for plain reservations: into a placeholder it would silently commit a range
        // the placeholder bookkeeping says is empty.
        let err = vm::commit(placeholder.as_ptr(), PAGE, Protection::ReadWrite)
            .expect_err("commit into a placeholder");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
        // `commit_placeholder` and `map_file` are for placeholders, exact size.
        let err = vm::commit_placeholder(plain.as_ptr(), GRANULARITY, Protection::ReadWrite)
            .expect_err("commit_placeholder into a plain reservation");
        assert!(matches!(err, VmError::PlaceholderNotExactSize { .. }), "{err}");
        let err = vm::map_file(&file, 0, PAGE, plain.as_ptr(), Protection::Read)
            .expect_err("map_file into a plain reservation");
        assert!(matches!(err, VmError::PlaceholderNotExactSize { .. }), "{err}");
        // A placeholder has no pages to protect or decommit.
        let err = vm::protect(placeholder.as_ptr(), PAGE, Protection::Read)
            .expect_err("protect a placeholder");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
        let err = vm::decommit_to_placeholder(placeholder.as_ptr(), PAGE)
            .expect_err("decommit a placeholder");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
        // `unmap` is for views.
        let err = vm::unmap(placeholder.as_ptr(), GRANULARITY).expect_err("unmap a placeholder");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
        // A coalesce must be whole placeholders: not a range that starts inside one.
        let err = vm::coalesce_placeholders(placeholder.as_ptr().add(PAGE), PAGE)
            .expect_err("coalesce a sub-range");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
        // And a split cannot cross into something that is not a placeholder.
        let err = vm::split_placeholder(&plain, 0, PAGE).expect_err("split a plain reservation");
        assert_eq!(err.os_error().map(|e| e.code()), Some(EINVAL), "{err}");
    }
    vm::release(plain).expect("release");
    vm::release(placeholder).expect("release");
    let _ = fs::remove_file(&path);
}

/// A view of a shared file writes the file, and a shared view protected down and back up stays
/// shared -- the property Windows has to build by creating every shared view writable first, and
/// Linux has because `MAP_SHARED` is the mapping's and not the protection's.
#[test]
fn a_shared_view_writes_the_file_and_stays_shared_across_protect() {
    let path = fixture(4);
    let file = fs::OpenOptions::new().read(true).write(true).open(&path).expect("open rw");
    let shared = vm::share_file_for_mapping(file, &path).expect("share it");
    assert!(shared.is_shared());
    let reservation = vm::reserve_placeholder(2 * PAGE, PAGE).expect("reserve");
    // SAFETY: the reservation is a live exact-size placeholder; the view is unmapped below.
    unsafe {
        vm::map_file(&shared, PAGE as u64, 2 * PAGE, reservation.as_ptr(), Protection::Read)
            .expect("a read-only shared view");
        assert_eq!(read_mapped(reservation.as_ptr(), PAGE), expected_page(1));
        vm::protect(reservation.as_ptr(), 2 * PAGE, Protection::ReadWrite).expect("raise to rw");
        std::ptr::write_bytes(reservation.as_ptr(), 0x5c, 8);
        vm::sync_view(&shared, reservation.as_ptr(), 2 * PAGE).expect("msync");
        vm::unmap_and_release(reservation.as_ptr(), 2 * PAGE).expect("unmap and release");
    }
    let on_disk = fs::read(&path).expect("read the file back");
    assert_eq!(&on_disk[PAGE..PAGE + 8], &[0x5c; 8], "the store must have reached the file");
    assert_eq!(&on_disk[PAGE + 8..2 * PAGE], &expected_page(1)[8..], "and nothing else changed");

    // A descriptor open only for reading cannot back a writable shared view, and the refusal comes
    // at the same step it does on Windows: when the file is made shareable.
    let read_only = fs::File::open(&path).expect("open read-only");
    let err = vm::share_file_for_mapping(read_only, &path).expect_err("read-only descriptor");
    assert!(matches!(err, VmError::SectionCreate { .. }), "{err}");
    assert_eq!(err.os_error().map(|e| e.code()), Some(EACCES), "{err}");
    let _ = fs::remove_file(&path);
}

/// The D12 section: two views of one memfd, a store through the writable one visible through the
/// executable one, and **no page of either both writable and executable** -- read back from the
/// kernel's own record of the mappings rather than assumed from the flags passed.
#[test]
fn a_shared_section_is_two_views_of_the_same_pages_and_neither_is_writable_and_executable() {
    let section = vm::create_shared_section(GRANULARITY as u64).expect("a memfd section");
    assert_eq!(section.len(), GRANULARITY as u64);
    // SAFETY: both views are fresh kernel-chosen mappings, unmapped at the end.
    unsafe {
        let rw = vm::map_section(&section, 0, GRANULARITY, Protection::ReadWrite).expect("rw view");
        let rx = vm::map_section(&section, 0, GRANULARITY, Protection::ReadExecute).expect("rx view");
        assert_ne!(rw, rx, "two views, two addresses");
        std::ptr::write_bytes(rw, 0x90, 64);
        assert_eq!(read_mapped(rx, 64), vec![0x90; 64], "the executable view sees the store");

        let maps = fs::read_to_string("/proc/self/maps").expect("read /proc/self/maps");
        let perms_at = |address: usize| {
            maps.lines()
                .find(|line| {
                    let range = line.split_whitespace().next().unwrap_or("");
                    let (start, end) = range.split_once('-').unwrap_or(("0", "0"));
                    let start = usize::from_str_radix(start, 16).unwrap_or(0);
                    let end = usize::from_str_radix(end, 16).unwrap_or(0);
                    address >= start && address < end
                })
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(perms_at(rw as usize), "rw-s", "the writable view is shared and not executable");
        assert_eq!(perms_at(rx as usize), "r-xs", "the executable view is shared and not writable");

        vm::unmap_and_release(rw, GRANULARITY).expect("unmap rw");
        vm::unmap_and_release(rx, GRANULARITY).expect("unmap rx");
    }
}
