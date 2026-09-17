//! Windows behaviour of the virtual-memory seam, asserted against the measurements in
//! `docs/research/windows-memory-model.md` (D10, D11).
//!
//! These tests assert *behaviour*, not the absence of an error: the bytes that appear at a mapped
//! address, the exact granularity each operation enforces, and the specific OS error code that a
//! wrong call produces. A test that only checked `is_ok()` would pass against an implementation
//! that mapped the wrong file offset.
//!
//! Commit-charge measurements live in `vm_commit_charge.rs`, in a separate test binary, because
//! commit charge is a per-process number and the test harness runs tests in one process in
//! parallel.
#![cfg(target_os = "windows")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use omni_platform::vm::{
    self, MapExecutability, Protection, Reservation, ReservationKind, VmError,
};

const PAGE: usize = 4096;
const GRANULARITY: usize = 65536;

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
fn page_size_is_4096_and_allocation_granularity_is_65536() {
    assert_eq!(vm::page_size(), PAGE);
    assert_eq!(vm::allocation_granularity(), GRANULARITY);
}

#[test]
fn the_three_kernelbase_symbols_resolve() {
    // D11: VirtualAlloc2, MapViewOfFile3 and UnmapViewOfFile2 are not exported from kernel32.dll.
    // Everything placeholder-related depends on resolving them from kernelbase.dll, so the seam
    // reports the result rather than leaving it to be inferred from a later failure.
    for (symbol, resolved) in vm::windows::placeholder_api_symbols() {
        assert!(resolved, "{symbol} did not resolve from kernelbase.dll");
    }
    assert!(vm::windows::placeholder_api_available());
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
    // Requested through MEM_ADDRESS_REQUIREMENTS, because an ordinary reservation cannot be
    // partially released and so cannot be over-reserved and trimmed.
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
    // Measured here, not documented in D10/D11: a release of an address that is not the base of a
    // live reservation reports ERROR_INVALID_ADDRESS (487), not ERROR_INVALID_PARAMETER (87) as a
    // *partial* release of a live reservation does. Both are refusals, but only one of them is the
    // code to look for after a double free.
    let reservation = vm::reserve(GRANULARITY, GRANULARITY).expect("reserve");
    let stale = reservation;
    vm::release(reservation).expect("first release");
    let err = vm::release(stale).expect_err("second release must fail");
    assert_eq!(
        err.os_error().map(|e| e.code()),
        Some(487),
        "expected ERROR_INVALID_ADDRESS, got {err}"
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
        assert_eq!(err.os_error().map(|e| e.code()), Some(487), "{err}");
        assert!(err.to_string().contains("split_placeholder"), "{err}");

        let err = vm::commit_placeholder(reservation.as_ptr(), PAGE, Protection::ReadWrite)
            .expect_err("a 4 KB private commit into a 1 MiB placeholder must fail");
        assert!(matches!(err, VmError::PlaceholderNotExactSize { .. }), "{err}");
        assert_eq!(err.os_error().map(|e| e.code()), Some(487), "{err}");
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
                // The condition the kernel reports as ERROR_MAPPED_ALIGNMENT.
                assert_eq!(os_equivalent.code(), 1132);
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
        .expect("open GENERIC_READ | GENERIC_EXECUTE with a PAGE_EXECUTE_READ section");
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
fn raising_a_view_to_executable_depends_on_both_the_section_and_the_view() {
    // D11 (5.F) records that the section protection caps the maximum protection of every view.
    // Measured here: the *view* protection caps it as well. A view created PAGE_READONLY cannot be
    // raised to PAGE_EXECUTE_READ even when the section is PAGE_EXECUTE_READ — VirtualProtect
    // fails with ERROR_INVALID_PARAMETER (87) in that case too. So executability has to be decided
    // twice: once when the file is opened, and again when the view is mapped.
    let path = fixture(8);

    // A read-only view of a non-executable file: refused, as D11 says.
    let plain = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: the reservation is a live exact-size placeholder and the view is unmapped below.
    unsafe {
        vm::map_file(&plain, 0, PAGE, reservation.as_ptr(), Protection::Read).expect("map");
        let err = vm::protect(reservation.as_ptr(), PAGE, Protection::ReadExecute)
            .expect_err("a view of a PAGE_READONLY section must not reach r-x");
        assert_eq!(
            err.os_error().map(|e| e.code()),
            Some(87),
            "expected ERROR_INVALID_PARAMETER, got {err}"
        );
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }
    drop(plain);

    // A read-only view of an executable file: also refused. This is the part D11 does not say.
    let executable =
        vm::open_file_for_mapping(&path, MapExecutability::Executable).expect("open executable");
    let reservation = vm::reserve_placeholder(PAGE, GRANULARITY).expect("reserve");
    // SAFETY: as above.
    unsafe {
        vm::map_file(&executable, 0, PAGE, reservation.as_ptr(), Protection::Read).expect("map");
        let err = vm::protect(reservation.as_ptr(), PAGE, Protection::ReadExecute).expect_err(
            "a PAGE_READONLY view cannot be raised to r-x even from an executable section",
        );
        assert_eq!(
            err.os_error().map(|e| e.code()),
            Some(87),
            "expected ERROR_INVALID_PARAMETER, got {err}"
        );
        vm::unmap_and_release(reservation.as_ptr(), PAGE).expect("unmap and release");
    }

    // Mapped PAGE_EXECUTE_READ from a PAGE_EXECUTE_READ section, the whole cycle the ELF loader
    // needs works: drop to writable to apply relocations, then back up to executable.
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
    let _ = fs::remove_file(&path);
}

#[test]
fn a_writable_file_view_is_copy_on_write_and_does_not_touch_the_file() {
    // Protection::ReadWrite on a file view becomes PAGE_WRITECOPY, because the file is opened
    // without GENERIC_WRITE and because the extraction cache is shared between instances and must
    // stay immutable. Writes must privatise the page, not reach the disk.
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
    // Windows unmaps a whole view from its base; there is no partial unmap. Silently unmapping
    // more than was asked for would corrupt whatever the caller put next to it.
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
    // The dangerous direction, and the one a guest partial munmap of a mapping's *head* produces.
    // UnmapViewOfFile2 takes no length, so honouring this literally would tear down the whole view
    // and report success. Both unmap flavours must refuse it.
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
    // `protect` on part of a view splits it into several MEMORY_BASIC_INFORMATION regions that all
    // keep the view's AllocationBase, so a single RegionSize understates the view. If the extent
    // check used one query, the short-size refusal above would stop working the moment anything
    // had called protect — which, for a loaded ELF segment, is always.
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
    assert_eq!(err.os_error().map(|e| e.code()), Some(2), "{err}");
    let text = err.to_string();
    assert!(text.contains("ERROR_FILE_NOT_FOUND"), "{text}");
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
