//! Functional tests for the guest address space: region tracking, fixed placement, protection, and
//! the emulated partial unmap.
//!
//! These assert behaviour, not cost. The tests that assert what memory operations *cost* are in
//! `commit_charge.rs`, in their own binary, because commit charge is per process and `cargo test`
//! runs one binary's tests as parallel threads.
//!
//! Windows-only, because `omni-platform`'s Linux and macOS backends are structural and every
//! operation on them returns a typed `Unsupported` error. A test here would assert nothing about
//! those targets that `config.rs` does not already assert.
#![cfg(target_os = "windows")]

mod common;

use std::sync::Arc;

use common::{TempFile, KIB, MIB};
use omni_mem::{
    Backing, CommitPolicy, GuestSpace, GuestSpaceConfig, MapExecutability, MemError, Placement,
    Protection, RegionKind,
};

fn space(size: usize) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size, ..GuestSpaceConfig::default() })
        .expect("reserve a guest address space")
}

/// Every entry tiles the space exactly: sorted, non-overlapping, gapless. The region map is the only
/// thing that knows where the OS's placeholder boundaries are, so a map that has drifted would show
/// up later as an unexplainable access violation rather than as a failure here.
fn assert_tiles_the_space(space: &GuestSpace) {
    let regions = space.regions();
    assert!(!regions.is_empty(), "the region map is empty");
    let mut expected = space.base();
    for region in &regions {
        assert_eq!(region.start, expected, "gap or overlap at {:#x}", region.start);
        assert_ne!(region.len, 0, "zero-length region at {:#x}", region.start);
        expected = region.end();
    }
    assert_eq!(expected, space.end(), "the region map does not reach the end of the space");

    let stats = space.stats();
    assert_eq!(
        stats.mapped + stats.free,
        space.len(),
        "mapped {} plus free {} is not the whole space {}",
        stats.mapped,
        stats.free,
        space.len()
    );
}

unsafe fn fill(address: usize, len: usize, value: u8) {
    std::ptr::write_bytes(address as *mut u8, value, len);
}

unsafe fn read(address: usize) -> u8 {
    std::ptr::read_volatile(address as *const u8)
}

#[test]
fn a_fresh_space_is_one_free_region_covering_everything() {
    let space = space(64 * MIB);
    assert_tiles_the_space(&space);
    let regions = space.regions();
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].kind, RegionKind::Free);
    assert_eq!(regions[0].len, 64 * MIB);
    assert!(space.mapped_regions().is_empty());
    assert_eq!(space.stats().free, 64 * MIB);
    assert_eq!(space.stats().committed, 0);
}

#[test]
fn a_fixed_mapping_lands_exactly_where_it_was_demanded() {
    let space = space(64 * MIB);
    let target = space.base() + 16 * MIB;
    let address = space
        .map_anonymous(Placement::Fixed(target), 128 * KIB, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("fixed mapping");
    assert_eq!(address, target, "a fixed mapping must land exactly where it was demanded");

    let region = space.region_at(target).expect("the range is mapped");
    assert_eq!(region.start, target);
    assert_eq!(region.len, 128 * KIB);
    assert_eq!(region.protection, Protection::ReadWrite);
    assert_eq!(region.kind, RegionKind::Anonymous);
    assert_tiles_the_space(&space);

    // The pages really are there and really are writable.
    // SAFETY: the range was just mapped ReadWrite and committed eagerly.
    unsafe {
        fill(address, 128 * KIB, 0x5A);
        assert_eq!(read(address), 0x5A);
        assert_eq!(read(address + 128 * KIB - 1), 0x5A);
    }
}

#[test]
fn a_fixed_mapping_over_an_occupied_range_fails_with_the_conflict() {
    let space = space(64 * MIB);
    let first = space.base() + MIB;
    space
        .map_anonymous(Placement::Fixed(first), 256 * KIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("first mapping");

    // Overlapping the tail of the existing mapping by one page must fail, and must say what is in
    // the way rather than just that something is.
    let error = space
        .map_anonymous(
            Placement::Fixed(first + 128 * KIB),
            256 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect_err("a fixed mapping over an occupied range must fail");
    match error {
        MemError::AddressTaken { requested, conflict_start, conflict_end, .. } => {
            assert_eq!(requested, first + 128 * KIB);
            assert_eq!(conflict_start, first);
            assert_eq!(conflict_end, first + 256 * KIB);
        }
        other => panic!("expected AddressTaken, got {other}"),
    }

    // The failure changed nothing: the original mapping is intact and the rest is still free.
    assert_tiles_the_space(&space);
    assert_eq!(space.stats().mapped, 256 * KIB);
    assert_eq!(space.mapped_regions().len(), 1);

    // Outside the space is a different error, and names the space.
    let error = space
        .map_anonymous(
            Placement::Fixed(space.end() - 4 * KIB),
            64 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect_err("a mapping running past the end of the space must fail");
    assert!(matches!(error, MemError::OutsideSpace { .. }), "got {error}");
}

#[test]
fn placement_honours_an_arbitrary_alignment_and_does_not_assume_a_page() {
    let space = space(64 * MIB);
    // 16 KiB is the alignment every PT_LOAD in libroblox.so actually has (p_align = 0x4000), and
    // 4 KiB is the page size, so a correct implementation must be able to tell them apart. 1 MiB is
    // here to show nothing is special about either.
    for align in [4 * KIB, 16 * KIB, 64 * KIB, MIB] {
        let address = space
            .map_anonymous(
                Placement::Anywhere { align },
                48 * KIB,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .unwrap_or_else(|error| panic!("mapping at alignment {align:#x}: {error}"));
        assert_eq!(address % align, 0, "{address:#x} is not aligned to {align:#x}");
    }
    assert_tiles_the_space(&space);

    let error = space
        .map_anonymous(
            Placement::Anywhere { align: 3 * KIB },
            KIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect_err("an alignment that is not a power of two must be rejected");
    assert!(matches!(error, MemError::Misaligned { .. }), "got {error}");
}

#[test]
fn a_hint_is_honoured_when_free_and_ignored_when_not() {
    let space = space(64 * MIB);
    let wanted = space.base() + 8 * MIB;
    let first = space
        .map_anonymous(
            Placement::Hint { address: wanted, align: 64 * KIB },
            64 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("hinted mapping");
    assert_eq!(first, wanted, "a free hint should be taken");

    let second = space
        .map_anonymous(
            Placement::Hint { address: wanted, align: 64 * KIB },
            64 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("hinted mapping over an occupied hint must still succeed somewhere");
    assert_ne!(second, wanted);
    assert_eq!(second % (64 * KIB), 0);
    assert_tiles_the_space(&space);
}

#[test]
fn unmapping_the_middle_of_a_mapping_splits_it_and_keeps_the_survivors_intact() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let len = 4 * granule;
    let address = space
        .map_anonymous(Placement::Anywhere { align: granule }, len, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("mapping");

    // SAFETY: the whole range is mapped ReadWrite and committed.
    unsafe {
        for index in 0..4 {
            fill(address + index * granule, granule, 0xB0 + index as u8);
        }
    }

    space.unmap(address + granule, granule).expect("unmap the middle");
    assert_tiles_the_space(&space);

    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 2, "the mapping should have been split in two: {mapped:#?}");
    assert_eq!(mapped[0].start, address);
    assert_eq!(mapped[0].len, granule);
    assert_eq!(mapped[1].start, address + 2 * granule);
    assert_eq!(mapped[1].len, 2 * granule);
    assert_eq!(mapped[0].mapping, mapped[1].mapping, "both halves are still the same mapping");
    assert!(space.region_at(address + granule).is_none(), "the hole must read as unmapped");

    // The survivors kept their contents. This is what distinguishes a partial decommit from
    // re-committing the range, which would have zeroed it.
    // SAFETY: both surviving ranges are still mapped and committed.
    unsafe {
        assert_eq!(read(address), 0xB0);
        assert_eq!(read(address + granule - 1), 0xB0);
        assert_eq!(read(address + 2 * granule), 0xB2);
        assert_eq!(read(address + 4 * granule - 1), 0xB3);
    }

    // And the head and the tail can be unmapped separately afterwards, which they could not be if
    // the partial release had left the OS's idea of the allocations out of step with ours.
    space.unmap(address, granule).expect("unmap the head");
    space.unmap(address + 2 * granule, 2 * granule).expect("unmap the tail");
    assert_eq!(space.stats().mapped, 0);
    assert_eq!(space.stats().committed, 0);
    assert_tiles_the_space(&space);
}

#[test]
fn unmapping_a_range_that_is_already_free_is_not_an_error() {
    let space = space(16 * MIB);
    let address = space
        .map_anonymous(Placement::Anywhere { align: 64 * KIB }, 64 * KIB, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("mapping");
    space.unmap(address, 64 * KIB).expect("first unmap");
    // munmap of an unmapped range succeeds on Linux, and a guest that double-frees must not take
    // the runtime down.
    space.unmap(address, 64 * KIB).expect("second unmap");
    space.unmap(space.base(), 4 * MIB).expect("unmap a range that was never mapped");
    assert_tiles_the_space(&space);
}

#[test]
fn a_mapping_can_span_two_ranges_that_were_freed_separately() {
    // This is the placeholder-coalescing test. Splitting is one-way at the OS level: after two
    // adjacent ranges have been mapped and unmapped independently, they are two separate
    // placeholders, and a split spanning both fails with ERROR_INVALID_PARAMETER (87). Without
    // merging them the guest could never reuse the combined range.
    let space = space(16 * MIB);
    let base = space.base() + MIB;
    for index in 0..4 {
        space
            .map_anonymous(
                Placement::Fixed(base + index * 64 * KIB),
                64 * KIB,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("mapping");
    }
    for index in 0..4 {
        space.unmap(base + index * 64 * KIB, 64 * KIB).expect("unmap");
    }
    assert!(space.stats().entries > 1, "the space should be fragmented at this point");

    let address = space
        .map_anonymous(Placement::Fixed(base), 256 * KIB, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a mapping spanning all four freed ranges");
    assert_eq!(address, base);
    // SAFETY: the whole range is mapped ReadWrite and committed.
    unsafe {
        fill(address, 256 * KIB, 0x11);
        assert_eq!(read(address + 256 * KIB - 1), 0x11);
    }
    assert_tiles_the_space(&space);
}

#[test]
fn reclaim_idle_defragments_the_placeholder_set() {
    let space = space(16 * MIB);
    let base = space.base();
    for index in 0..8 {
        space
            .map_anonymous(
                Placement::Fixed(base + index * 64 * KIB),
                64 * KIB,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("mapping");
    }
    for index in 0..8 {
        space.unmap(base + index * 64 * KIB, 64 * KIB).expect("unmap");
    }
    let before = space.stats().entries;
    assert!(before > 1, "expected a fragmented placeholder set, got {before} entries");

    let reclaimed = space.reclaim_idle().expect("reclaim");
    let after = space.stats().entries;
    assert_eq!(after, 1, "the whole space should be one placeholder again, not {after} entries");
    assert!(reclaimed.coalesced >= before - 1, "{reclaimed:?} did not merge {before} entries");
    assert_tiles_the_space(&space);
}

#[test]
fn a_protection_none_mapping_costs_no_commit_and_becomes_committable_when_raised() {
    let space = space(16 * MIB);
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            MIB,
            Protection::None,
            CommitPolicy::Eager,
        )
        .expect("PROT_NONE mapping");
    // An inaccessible page is exactly what an unreplaced placeholder already is, so paying commit
    // charge for one would be paying for nothing.
    assert_eq!(space.stats().committed, 0, "a PROT_NONE mapping must not commit anything");
    assert_eq!(space.ensure_committed(address, MIB).expect("commit"), 0);

    space.protect(address, MIB, Protection::ReadWrite).expect("raise to ReadWrite");
    assert_eq!(space.stats().committed, 0, "protect must not commit by itself");
    // The mapping is eager, and eager means the caller has said the whole range will be used, so
    // the first commit covers all of it in one call rather than granule by granule.
    let committed = space.ensure_committed(address, 1).expect("commit");
    assert_eq!(committed, MIB, "an eager mapping commits in one piece once it is accessible");
    // SAFETY: the range is now committed and writable.
    unsafe {
        fill(address, MIB, 0x7E);
        assert_eq!(read(address), 0x7E);
        assert_eq!(read(address + MIB - 1), 0x7E);
    }

    // A *lazy* PROT_NONE mapping behaves the same way about commit, and commits one granule at a
    // time once it is raised.
    let lazy = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            MIB,
            Protection::None,
            CommitPolicy::Lazy,
        )
        .expect("lazy PROT_NONE mapping");
    assert_eq!(space.ensure_committed(lazy, MIB).expect("commit"), 0);
    space.protect(lazy, MIB, Protection::ReadWrite).expect("raise to ReadWrite");
    assert_eq!(
        space.ensure_committed(lazy, 1).expect("commit one byte"),
        space.commit_granule(),
        "one byte of a lazy mapping must commit exactly one granule"
    );
    assert_tiles_the_space(&space);
}

#[test]
fn ensure_committed_is_idempotent_and_clipped_to_the_mapping() {
    let space = space(16 * MIB);
    let granule = space.commit_granule();
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            4 * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("lazy mapping");
    assert_eq!(space.stats().committed, 0, "a lazy mapping commits nothing up front");

    assert_eq!(space.ensure_committed(address, 1).expect("commit"), granule);
    assert_eq!(space.ensure_committed(address, 1).expect("commit again"), 0, "not idempotent");
    assert_eq!(space.stats().committed, granule);

    // A request that straddles two granules commits both, and only both.
    assert_eq!(
        space.ensure_committed(address + granule - 1, 2).expect("commit across a boundary"),
        granule
    );
    assert_eq!(space.stats().committed, 2 * granule);

    // The last granule of the mapping is committed without spilling past the mapping's end.
    assert_eq!(space.ensure_committed(address + 4 * granule - 1, 1).expect("commit"), granule);
    assert_eq!(space.stats().committed, 3 * granule);
    let next = space.region_at(address + 4 * granule);
    assert!(next.is_none(), "committing the last granule must not spill into the next region");
    assert_tiles_the_space(&space);
}

#[test]
fn the_space_reports_what_it_cannot_place() {
    let space = space(MIB);
    space
        .map_anonymous(Placement::Fixed(space.base()), MIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("fill the space");
    let error = space
        .map_anonymous(Placement::Anywhere { align: 4 * KIB }, 4 * KIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect_err("a full space cannot place anything");
    match error {
        MemError::NoSpace { len, free, largest, .. } => {
            assert_eq!(len, 4 * KIB);
            assert_eq!(free, 0);
            assert_eq!(largest, 0);
        }
        other => panic!("expected NoSpace, got {other}"),
    }
}

// -------------------------------------------------------------------------------------------
// File-backed mappings, and the emulated partial unmap
// -------------------------------------------------------------------------------------------

fn backing(name: &str, len: usize, page: usize, exec: MapExecutability) -> (TempFile, Arc<Backing>) {
    let file = TempFile::new(name, len, page);
    let backing = Backing::open(file.path(), exec).expect("open the backing file");
    (file, backing)
}

#[test]
fn a_file_maps_at_a_fixed_address_and_reads_back_its_own_contents() {
    let space = space(16 * MIB);
    let page = space.page_size();
    let (file, backing) = backing("segment.bin", MIB, page, MapExecutability::NonExecutable);

    let target = space.base() + 2 * MIB;
    let address = space
        .map_file(&backing, 64 * KIB as u64, Placement::Fixed(target), 256 * KIB, Protection::Read)
        .expect("map the file");
    assert_eq!(address, target);

    let region = space.region_at(target).expect("mapped");
    match &region.kind {
        RegionKind::File { backing: id, file_offset, name, shared } => {
            assert!(!shared, "an opened file is a private mapping");
            assert_eq!(*id, backing.id());
            assert_eq!(*file_offset, 64 * KIB as u64);
            assert!(name.ends_with("segment.bin"), "{name}");
        }
        other => panic!("expected a file region, got {other:?}"),
    }
    // A read-only view costs no commit charge: it is the file's pages, shared with every other
    // instance that maps the same file (D11).
    assert_eq!(space.stats().committed, 0);
    assert_eq!(space.stats().file_backed, 256 * KIB);

    // SAFETY: the range is a live read-only view of the file.
    unsafe {
        for offset in [0usize, page, 255 * KIB] {
            assert_eq!(
                read(address + offset),
                file.byte_at(64 * KIB as u64 + offset as u64),
                "wrong file contents at offset {offset:#x}"
            );
        }
    }
    assert_tiles_the_space(&space);
}

#[test]
fn a_non_executable_backing_cannot_be_mapped_executable() {
    let space = space(16 * MIB);
    let page = space.page_size();
    let (_file, backing) = backing("data.bin", 256 * KIB, page, MapExecutability::NonExecutable);
    let error = space
        .map_file(
            &backing,
            0,
            Placement::Anywhere { align: 64 * KIB },
            64 * KIB,
            Protection::ReadExecute,
        )
        .expect_err("an executable view of a non-executable backing must fail");
    let platform = error.platform_error().expect("a platform error");
    assert!(
        matches!(platform, omni_platform::vm::VmError::FileNotOpenedExecutable { .. }),
        "got {platform}"
    );
    // And the failed attempt left nothing behind.
    assert_eq!(space.stats().mapped, 0);
    assert_tiles_the_space(&space);
}

#[test]
fn the_relocation_sequence_from_d11_works_on_a_file_view() {
    // Map ReadExecute, drop to ReadWrite, write, raise back to ReadExecute. Mapping read-only first
    // and promoting later does *not* work (D11 correction), and this is the sequence the ELF loader
    // has to use to relocate file-backed .text.
    let space = space(16 * MIB);
    let page = space.page_size();
    let (file, backing) = backing("text.bin", 256 * KIB, page, MapExecutability::Executable);
    let address = space
        .map_file(
            &backing,
            0,
            Placement::Anywhere { align: 64 * KIB },
            64 * KIB,
            Protection::ReadExecute,
        )
        .expect("map executable");

    space.protect(address, page, Protection::ReadWrite).expect("drop one page to ReadWrite");
    // One page of the view is writable and the rest is not, so the guest would see two lines.
    let during = space.mapped_regions();
    assert_eq!(during.len(), 2, "{during:#?}");
    assert_eq!((during[0].len, during[0].protection), (page, Protection::ReadWrite));
    assert_eq!((during[1].len, during[1].protection), (64 * KIB - page, Protection::ReadExecute));
    // SAFETY: the first page is a copy-on-write view and is writable.
    unsafe {
        fill(address, 8, 0xCC);
        assert_eq!(read(address), 0xCC);
    }
    space.protect(address, page, Protection::ReadExecute).expect("raise back to ReadExecute");

    // The write privatised only the page it touched: the next page still reads the file.
    // SAFETY: both pages are live views.
    unsafe {
        assert_eq!(read(address), 0xCC);
        assert_eq!(read(address + page), file.byte_at(page as u64));
    }
    // The file itself was never modified — copy-on-write means the writes never reach it.
    let on_disk = std::fs::read(file.path()).expect("read the file back");
    assert_eq!(on_disk[0], file.byte_at(0), "the write reached the file, which it must not");

    // With the protection restored the two halves are indistinguishable to the guest again, so the
    // enumeration reports one region — the privatised page is still privatised, but that is
    // Omnidroid's bookkeeping and not something the guest can observe.
    let regions = space.mapped_regions();
    assert_eq!(regions.len(), 1, "{regions:#?}");
    assert_eq!(regions[0].len, 64 * KIB);
    assert_eq!(regions[0].protection, Protection::ReadExecute);
    assert_tiles_the_space(&space);
}

/// The head, the tail, and a hole through the middle: the three shapes of partial `munmap`.
///
/// Windows cannot partially unmap a view — `UnmapViewOfFile2` takes no length — so `omni-mem`
/// unmaps the whole view and maps the survivors again. What makes this test worth writing is the
/// content check: each survivor is verified to hold the *file bytes belonging to its own address*,
/// which is the thing that breaks if the re-mapped pieces are given the wrong file offsets, and
/// which no amount of "the call returned Ok" would catch.
#[test]
fn a_partial_unmap_of_a_view_keeps_the_surviving_pieces_at_the_right_file_offsets() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let (file, backing) = backing("segments.bin", 4 * MIB, page, MapExecutability::NonExecutable);
    let view_len = 256 * KIB;

    let check = |address: usize, offsets: &[usize], file_offset: u64| {
        for &offset in offsets {
            // SAFETY: the caller only passes offsets inside a live view.
            let seen = unsafe { read(address + offset) };
            assert_eq!(
                seen,
                file.byte_at(file_offset + offset as u64),
                "the piece at {address:#x} holds the wrong file bytes at offset {offset:#x}"
            );
        }
    };

    // --- the head
    let address = space
        .map_file(&backing, MIB as u64, Placement::Anywhere { align: 64 * KIB }, view_len, Protection::Read)
        .expect("map");
    space.unmap(address, 64 * KIB).expect("unmap the head");
    assert_tiles_the_space(&space);
    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 1, "{mapped:#?}");
    assert_eq!(mapped[0].start, address + 64 * KIB);
    assert_eq!(mapped[0].len, view_len - 64 * KIB);
    check(address + 64 * KIB, &[0, page, 64 * KIB], MIB as u64 + 64 * KIB as u64);
    space.unmap(address, view_len).expect("unmap the rest");
    assert_eq!(space.stats().mapped, 0);

    // --- the tail
    let address = space
        .map_file(&backing, MIB as u64, Placement::Anywhere { align: 64 * KIB }, view_len, Protection::Read)
        .expect("map");
    space.unmap(address + view_len - 64 * KIB, 64 * KIB).expect("unmap the tail");
    assert_tiles_the_space(&space);
    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 1, "{mapped:#?}");
    assert_eq!(mapped[0].start, address);
    assert_eq!(mapped[0].len, view_len - 64 * KIB);
    check(address, &[0, page, 64 * KIB], MIB as u64);
    space.unmap(address, view_len).expect("unmap the rest");
    assert_eq!(space.stats().mapped, 0);

    // --- a hole through the middle, which leaves two views where there was one
    let address = space
        .map_file(&backing, MIB as u64, Placement::Anywhere { align: 64 * KIB }, view_len, Protection::Read)
        .expect("map");
    space.unmap(address + 64 * KIB, 128 * KIB).expect("punch a hole");
    assert_tiles_the_space(&space);
    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 2, "{mapped:#?}");
    assert_eq!((mapped[0].start, mapped[0].len), (address, 64 * KIB));
    assert_eq!((mapped[1].start, mapped[1].len), (address + 192 * KIB, 64 * KIB));
    check(address, &[0, page, 64 * KIB - 1], MIB as u64);
    check(address + 192 * KIB, &[0, page], MIB as u64 + 192 * KIB as u64);
    assert!(space.region_at(address + 64 * KIB).is_none(), "the hole must read as unmapped");
    assert_eq!(space.stats().file_backed, 128 * KIB);

    // The survivors are independent views now, so each can be unmapped on its own.
    space.unmap(address, 64 * KIB).expect("unmap the first survivor");
    space.unmap(address + 192 * KIB, 64 * KIB).expect("unmap the second survivor");
    assert_eq!(space.stats().mapped, 0);
    assert_tiles_the_space(&space);
}

#[test]
fn a_partially_unmapped_view_keeps_each_survivors_own_protection() {
    // A view whose pages were protected differently is several entries of one view. When a hole is
    // punched through it, each surviving piece becomes a view of its own — which is the only way it
    // can keep its own protection, because a view is created with one.
    let space = space(64 * MIB);
    let page = space.page_size();
    let (_file, backing) = backing("mixed.bin", MIB, page, MapExecutability::Executable);
    let address = space
        .map_file(
            &backing,
            0,
            Placement::Anywhere { align: 64 * KIB },
            256 * KIB,
            Protection::ReadExecute,
        )
        .expect("map");
    space.protect(address, 64 * KIB, Protection::Read).expect("protect the first 64 KiB");
    space
        .protect(address + 192 * KIB, 64 * KIB, Protection::None)
        .expect("protect the last 64 KiB to None");

    space.unmap(address + 96 * KIB, 64 * KIB).expect("punch a hole through the middle");
    assert_tiles_the_space(&space);

    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 4, "{mapped:#?}");
    assert_eq!((mapped[0].start, mapped[0].len, mapped[0].protection), (address, 64 * KIB, Protection::Read));
    assert_eq!(
        (mapped[1].start, mapped[1].len, mapped[1].protection),
        (address + 64 * KIB, 32 * KIB, Protection::ReadExecute)
    );
    assert_eq!(
        (mapped[2].start, mapped[2].len, mapped[2].protection),
        (address + 160 * KIB, 32 * KIB, Protection::ReadExecute)
    );
    assert_eq!(
        (mapped[3].start, mapped[3].len, mapped[3].protection),
        (address + 192 * KIB, 64 * KIB, Protection::None)
    );
    space.unmap(address, 256 * KIB).expect("unmap the rest");
    assert_eq!(space.stats().mapped, 0);
    assert_tiles_the_space(&space);
}

#[test]
fn adjacent_mappings_are_reported_separately_and_split_regions_are_merged() {
    let space = space(16 * MIB);
    let page = space.page_size();
    let base = space.base() + MIB;

    // Two adjacent anonymous mappings with the same protection are still two mappings: merging them
    // into one region would tell the guest something untrue about its own address space.
    let first = space
        .map_anonymous(Placement::Fixed(base), 64 * KIB, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("first");
    let second = space
        .map_anonymous(
            Placement::Fixed(base + 64 * KIB),
            64 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("second");
    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 2, "{mapped:#?}");
    assert_ne!(mapped[0].mapping, mapped[1].mapping);

    // Within one mapping, a protection change splits it and changing it back merges it again: the
    // enumeration reports what the guest can observe, not how the commit happens to be carved up.
    space.protect(first, page, Protection::Read).expect("protect one page");
    assert_eq!(space.mapped_regions().len(), 3);
    space.protect(first, page, Protection::ReadWrite).expect("protect it back");
    let mapped = space.mapped_regions();
    assert_eq!(mapped.len(), 2, "{mapped:#?}");
    assert_eq!(mapped[0].start, first);
    assert_eq!(mapped[0].len, 64 * KIB);
    assert_eq!(mapped[1].start, second);
    assert_tiles_the_space(&space);
}

#[test]
fn protecting_a_range_that_is_not_mapped_is_refused() {
    let space = space(16 * MIB);
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            64 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("mapping");
    let error = space
        .protect(address, 128 * KIB, Protection::Read)
        .expect_err("protecting past the end of the mapping must fail");
    assert!(matches!(error, MemError::NotMapped { .. }), "got {error}");
    // And the protection of the part that *is* mapped was not changed.
    assert_eq!(space.region_at(address).expect("mapped").protection, Protection::ReadWrite);
}

#[test]
fn a_space_survives_a_long_scripted_sequence_of_guest_operations() {
    // Nothing here asserts a specific outcome; what it asserts is that the region map still tiles
    // the space exactly after every step, which is the invariant that keeps the map and the OS in
    // step. The sequence deliberately mixes anonymous and file mappings, partial unmaps, protection
    // changes and reclamation.
    let space = space(64 * MIB);
    let page = space.page_size();
    let granule = space.commit_granule();
    let (_file, backing) = backing("mixed-sequence.bin", 2 * MIB, page, MapExecutability::Executable);
    let mut addresses = Vec::new();

    for step in 0..24 {
        match step % 6 {
            0 => {
                let address = space
                    .map_anonymous(
                        Placement::Anywhere { align: 16 * KIB },
                        (step + 1) * granule,
                        Protection::ReadWrite,
                        CommitPolicy::Lazy,
                    )
                    .expect("anonymous mapping");
                space.ensure_committed(address, granule + 1).expect("commit");
                addresses.push((address, (step + 1) * granule));
            }
            1 => {
                let address = space
                    .map_file(
                        &backing,
                        (step * page) as u64,
                        Placement::Anywhere { align: 64 * KIB },
                        128 * KIB,
                        Protection::ReadExecute,
                    )
                    .expect("file mapping");
                addresses.push((address, 128 * KIB));
            }
            2 => {
                if let Some(&(address, len)) = addresses.first() {
                    if len > 2 * page {
                        space.protect(address + page, page, Protection::Read).expect("protect");
                    }
                }
            }
            3 => {
                if let Some((address, len)) = addresses.pop() {
                    if len > 2 * page {
                        space.unmap(address + page, page).expect("punch a page-sized hole");
                    }
                    space.unmap(address, len).expect("unmap the rest");
                }
            }
            4 => {
                space.advise_idle(space.base(), space.len()).expect("advise");
                space.reclaim_idle().expect("reclaim");
            }
            _ => {
                // Unmap a mapping's head and keep tracking what is left of it, so that every
                // address still in the list names a range that is mapped end to end.
                if let Some((address, len)) = addresses.pop() {
                    let head = len.min(granule);
                    space.unmap(address, head).expect("unmap the head");
                    if len > head {
                        addresses.push((address + head, len - head));
                    }
                }
            }
        }
        assert_tiles_the_space(&space);
    }

    for (address, len) in addresses {
        space.unmap(address, len).expect("unmap");
    }
    assert_eq!(space.stats().mapped, 0);
    assert_eq!(space.stats().committed, 0);
    space.reclaim_idle().expect("reclaim");
    assert_eq!(space.stats().entries, 1, "the space should be one placeholder again");
    space.close().expect("close");
}

#[test]
fn a_space_can_be_closed_explicitly_and_reports_success() {
    let space = space(16 * MIB);
    let (_file, backing) = backing("closing.bin", 256 * KIB, space.page_size(), MapExecutability::NonExecutable);
    space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            MIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("anonymous");
    space
        .map_file(&backing, 0, Placement::Anywhere { align: 64 * KIB }, 128 * KIB, Protection::Read)
        .expect("file");
    // Teardown has to unmap views, decommit private memory, merge the placeholders back together
    // and release the reservation — and say so rather than logging a failure from a Drop impl.
    space.close().expect("close a space that still holds mappings");
}

/// What the partial-unmap emulation costs.
///
/// Reported as a number because the emulation is not free and Task 5 and the bionic `munmap` shim
/// both need to know roughly what they are paying: unmapping a whole view is one kernel call, while
/// punching a hole through one is an unmap, a split per boundary and a `MapViewOfFile3` per
/// surviving piece — and the number of surviving pieces depends on how many protection sub-ranges
/// the view has been carved into, not on its size.
#[test]
fn the_cost_of_emulating_a_partial_unmap_is_measured() {
    use std::time::Instant;

    let space = space(256 * MIB);
    let page = space.page_size();
    let (_file, backing) = backing("cost.bin", 8 * MIB, page, MapExecutability::NonExecutable);
    let len = 4 * MIB;
    let map = || {
        space
            .map_file(&backing, 0, Placement::Anywhere { align: 64 * KIB }, len, Protection::Read)
            .expect("map")
    };

    let address = map();
    let started = Instant::now();
    space.unmap(address, len).expect("unmap the whole view");
    let whole = started.elapsed();

    let address = map();
    let started = Instant::now();
    space.unmap(address + MIB, MIB).expect("punch a hole");
    let hole = started.elapsed();
    space.unmap(address, len).expect("clean up");

    // The same again, on a view that protection changes have carved into 16 pieces, because that is
    // what decides the cost.
    let address = map();
    for index in 0..16 {
        let protection = if index % 2 == 0 { Protection::Read } else { Protection::None };
        space
            .protect(address + index * 256 * KIB, 256 * KIB, protection)
            .expect("protect a slice");
    }
    let started = Instant::now();
    space.unmap(address + MIB, MIB).expect("punch a hole through a carved-up view");
    let carved = started.elapsed();
    space.unmap(address, len).expect("clean up");

    // Now the same hole on views that have been writable, where the copy-on-write comparison pass
    // runs. Two cases, because the gate is per *entry* and not per view: making one page writable
    // makes one page comparable, while making the whole view writable makes all of it comparable.
    let address = map();
    space.protect(address, page, Protection::ReadWrite).expect("make one page writable");
    // SAFETY: the first page is a copy-on-write view and is writable.
    unsafe { *(address as *mut u8) = 0xD1 };
    space.protect(address, page, Protection::Read).expect("restore");
    let started = Instant::now();
    space.unmap(address + MIB, MIB).expect("punch a hole through a partly writable view");
    let one_page_writable = started.elapsed();
    // SAFETY: the head survivor is a live view.
    unsafe { assert_eq!(*(address as *const u8), 0xD1, "the written byte was lost") };
    space.unmap(address, len).expect("clean up");

    let address = map();
    space.protect(address, len, Protection::ReadWrite).expect("make the whole view writable");
    // SAFETY: the whole view is copy-on-write and writable.
    unsafe { *(address as *mut u8) = 0xD2 };
    space.protect(address, len, Protection::Read).expect("restore");
    let started = Instant::now();
    space.unmap(address + MIB, MIB).expect("punch a hole through a wholly writable view");
    let all_writable = started.elapsed();
    // SAFETY: the head survivor is a live view.
    unsafe { assert_eq!(*(address as *const u8), 0xD2, "the written byte was lost") };
    space.unmap(address, len).expect("clean up");

    eprintln!(
        "a {} MiB view: whole unmap {whole:?}; hole punched through one piece {hole:?}; \
         hole punched through a view carved into 16 pieces {carved:?}; \
         hole punched after one page was made writable {one_page_writable:?}; \
         hole punched after all of it was made writable {all_writable:?}",
        len / MIB
    );
    // The gate is the point, and it is worth two assertions. A view that has never been writable is
    // not compared against the file at all; and because the flag lives on the *entry*, making one
    // page writable does not make the whole view expensive to unmap. If the first ever fails, the
    // comparison has started running on clean views, which would mean reading a 109 MB library's
    // text twice on every guest `munmap`.
    assert!(
        hole * 4 < all_writable,
        "a clean hole punch took {hole:?} and a fully compared one {all_writable:?}: the \
         ever_writable gate is not doing anything"
    );
    assert!(
        one_page_writable * 4 < all_writable,
        "comparing one page took {one_page_writable:?} and comparing the whole view took \
         {all_writable:?}: the gate is not per entry"
    );
    // Loose on purpose: this is a cost report, not a performance gate, and a gate tuned to this
    // machine would fail on a slower one for no useful reason. What it does rule out is the
    // emulation being pathological — quadratic in the view's size, say, or re-mapping every piece
    // when only two survive.
    assert!(hole < std::time::Duration::from_millis(20), "punching a hole took {hole:?}");
    assert!(carved < std::time::Duration::from_millis(50), "punching a hole took {carved:?}");
    assert_tiles_the_space(&space);
}


/// **C1 regression.** A partial unmap must not lose copy-on-write content in the pieces that survive.
///
/// The shape is the D11 relocation sequence — map `ReadExecute`, drop a page to `ReadWrite`, write,
/// restore `ReadExecute` — followed by unmapping a range *elsewhere in the same view*. Windows cannot
/// partially unmap a view, so the survivors are mapped again from the file, and before this was fixed
/// the relocated bytes came back as the file's own contents while `unmap` returned `Ok(())`. Silent
/// data loss in exactly Task 5's RELRO path and in the guest's `munmap`.
///
/// Note what the earlier tests could not see: they check that a survivor holds the right *file* bytes
/// and they never write to a survivor first, which is the one case that cannot detect this.
#[test]
fn a_partial_unmap_preserves_copy_on_write_content_in_the_survivors() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let (file, backing) = backing("relro.bin", MIB, page, MapExecutability::Executable);
    let len = 256 * KIB;
    let address = space
        .map_file(
            &backing,
            0,
            Placement::Anywhere { align: 64 * KIB },
            len,
            Protection::ReadExecute,
        )
        .expect("map executable");

    // Relocate one page near the start, the D11 way.
    space.protect(address, page, Protection::ReadWrite).expect("drop to ReadWrite");
    // SAFETY: the first page is a copy-on-write view and is writable.
    unsafe {
        fill(address, 16, 0xEE);
        *(address as *mut u8).add(page - 1) = 0xED;
    }
    space.protect(address, page, Protection::ReadExecute).expect("restore ReadExecute");

    // And one page at the very end, so that a survivor on the far side of the hole is covered too.
    let tail = address + len - page;
    space.protect(tail, page, Protection::ReadWrite).expect("drop to ReadWrite");
    // SAFETY: the last page is a copy-on-write view and is writable.
    unsafe { fill(tail, 8, 0xAB) };
    space.protect(tail, page, Protection::ReadExecute).expect("restore ReadExecute");

    // Now unmap 64 KiB from the middle: nowhere near either written page, but it destroys the whole
    // view because that is the only thing Windows can do.
    space.unmap(address + 96 * KIB, 64 * KIB).expect("punch a hole");
    assert_tiles_the_space(&space);

    // SAFETY: both survivors are live views again.
    unsafe {
        assert_eq!(read(address), 0xEE, "the relocated byte was lost");
        assert_eq!(read(address + 15), 0xEE, "the relocated bytes were lost");
        assert_eq!(read(address + page - 1), 0xED, "the last byte of the relocated page was lost");
        assert_eq!(read(tail), 0xAB, "the relocated byte in the tail survivor was lost");
        // A page that was never written still reads the file, so the preservation did not smear the
        // written page over its neighbours.
        assert_eq!(read(address + page), file.byte_at(page as u64));
        assert_eq!(read(address + 160 * KIB), file.byte_at(160 * KIB as u64));
    }
    // And copy-on-write still means what it says: the file on disk was never modified.
    let on_disk = std::fs::read(file.path()).expect("read the file back");
    assert_eq!(on_disk[0], file.byte_at(0), "the write reached the file");

    // Once more, to show the preserved content survives repeated partial unmaps rather than only the
    // first one.
    space.unmap(address + 32 * KIB, 16 * KIB).expect("punch a second hole");
    // SAFETY: the head survivor is still a live view.
    unsafe {
        assert_eq!(read(address), 0xEE, "the relocated byte was lost by the second unmap");
        assert_eq!(read(tail), 0xAB);
    }
    assert_tiles_the_space(&space);
}

/// The same guarantee for a view that was mapped `ReadWrite` in the first place, where the pages are
/// copy-on-write from the moment they are mapped rather than from a later `protect`.
#[test]
fn a_partial_unmap_preserves_writes_to_a_read_write_view() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let (file, backing) = backing("data.bin", MIB, page, MapExecutability::NonExecutable);
    let len = 128 * KIB;
    let address = space
        .map_file(
            &backing,
            0,
            Placement::Anywhere { align: 64 * KIB },
            len,
            Protection::ReadWrite,
        )
        .expect("map writable");

    // SAFETY: the whole view is copy-on-write and writable.
    unsafe {
        fill(address, page, 0x5C);
        fill(address + 64 * KIB, page, 0x6D);
    }
    // Unmap the last 32 KiB. The written pages are both in the survivor.
    space.unmap(address + 96 * KIB, 32 * KIB).expect("unmap the tail");
    // SAFETY: the survivor is a live view.
    unsafe {
        assert_eq!(read(address), 0x5C, "a write to a ReadWrite view was lost");
        assert_eq!(read(address + page - 1), 0x5C);
        assert_eq!(read(address + 64 * KIB), 0x6D);
        assert_eq!(read(address + page), file.byte_at(page as u64), "a clean page was disturbed");
    }
    assert_eq!(
        space.region_at(address).expect("mapped").protection,
        Protection::ReadWrite,
        "the survivor kept its protection"
    );
    assert_tiles_the_space(&space);
}

/// Content written into the part that is being unmapped is *meant* to disappear, and the survivor
/// must come back clean from the file rather than inheriting it.
#[test]
fn a_partial_unmap_does_not_preserve_content_in_the_part_it_unmaps() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let (file, backing) = backing("discard.bin", MIB, page, MapExecutability::NonExecutable);
    let len = 128 * KIB;
    let address = space
        .map_file(&backing, 0, Placement::Anywhere { align: 64 * KIB }, len, Protection::ReadWrite)
        .expect("map writable");
    // SAFETY: the whole view is copy-on-write and writable.
    unsafe { fill(address + 64 * KIB, page, 0x77) };

    space.unmap(address + 64 * KIB, 64 * KIB).expect("unmap the written half");
    // Map the same file range again over the freed address space.
    let again = space
        .map_file(
            &backing,
            64 * KIB as u64,
            Placement::Fixed(address + 64 * KIB),
            64 * KIB,
            Protection::Read,
        )
        .expect("map the same range again");
    // SAFETY: a live read-only view.
    unsafe {
        assert_eq!(
            read(again),
            file.byte_at(64 * KIB as u64),
            "a discarded copy-on-write page came back from somewhere"
        );
    }
    assert_tiles_the_space(&space);
}

// -------------------------------------------------------------------------------------------
// The commit ceilings. Commit charge is the scarce resource (D10, Global Constraint 6), and every
// quantity that decides how much of it to spend arrives, somewhere up the stack, from a file.
// -------------------------------------------------------------------------------------------

/// An eager mapping larger than the per-request ceiling is refused, and nothing is left behind.
///
/// This is the shape of the Critical defect it exists to stop: an eight-byte edit to a `PT_LOAD`'s
/// `p_memsz` in `libroblox.so` became a `.bss` mapping of 1 GiB and then of 3.3 GiB, committed
/// eagerly, measured at **+1026.004 MiB** and **+3406.664 MiB** of commit charge — with the load
/// returning success. `omni-elf` bounded the image *span*, but D10 measured address space as free, so
/// that check guarded the abundant resource and left the scarce one open.
#[test]
fn an_eager_mapping_past_the_per_request_ceiling_is_refused() {
    let space = GuestSpace::with_config(GuestSpaceConfig {
        size: 64 * MIB,
        max_committed: 32 * MIB,
        max_commit_request: 4 * MIB,
        ..GuestSpaceConfig::default()
    })
    .expect("reserve");

    let error = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            8 * MIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect_err("an eager mapping twice the per-request ceiling must be refused");
    match error {
        MemError::CommitRequestTooLarge { requested, limit, .. } => {
            assert_eq!(requested, 8 * MIB, "the error names what was asked for");
            assert_eq!(limit, 4 * MIB, "and what is permitted");
        }
        other => panic!("expected CommitRequestTooLarge, got {other}"),
    }
    // Refusing must not itself leak: a loader that reserved 109 MB per rejected library would be its
    // own denial of service, so the failed mapping is rolled back rather than left claimed.
    let stats = space.stats();
    assert_eq!(stats.mapped, 0, "the refused mapping is not left claimed");
    assert_eq!(stats.committed, 0, "and nothing is committed");
    assert_tiles_the_space(&space);

    // The same size lazily is fine, because a lazy mapping commits one granule per call. Address
    // space is free; commit charge is not.
    space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            8 * MIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("the same mapping lazily costs nothing and is allowed");
    assert_eq!(space.stats().committed, 0);
    space.close().expect("close");
}

/// The total ceiling bounds what many mappings accumulate, which is what the per-request ceiling
/// cannot see: sixteen mappings each just under the per-request limit would otherwise add up.
#[test]
fn the_total_ceiling_bounds_what_many_mappings_accumulate() {
    let space = GuestSpace::with_config(GuestSpaceConfig {
        size: 64 * MIB,
        max_committed: 8 * MIB,
        max_commit_request: 4 * MIB,
        ..GuestSpaceConfig::default()
    })
    .expect("reserve");

    for round in 0..2 {
        space
            .map_anonymous(
                Placement::Anywhere { align: 64 * KIB },
                4 * MIB,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .unwrap_or_else(|e| panic!("round {round} is within both ceilings: {e}"));
    }
    assert_eq!(space.stats().committed, 8 * MIB, "the ceiling is reached exactly");

    let error = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            4 * MIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect_err("the third mapping passes the total ceiling");
    match error {
        MemError::CommitCeiling { requested, committed, would_total, limit, .. } => {
            assert_eq!(requested, 4 * MIB);
            assert_eq!(committed, 8 * MIB, "the error names what is already committed");
            assert_eq!(would_total, 12 * MIB, "and what the total would have become");
            assert_eq!(limit, 8 * MIB, "and the limit");
        }
        other => panic!("expected CommitCeiling, got {other}"),
    }
    assert_eq!(space.stats().committed, 8 * MIB, "the refusal changed nothing");
    assert_tiles_the_space(&space);
    space.close().expect("close");
}

/// The total ceiling is enforced against a running total that comes *back down* when memory is
/// released, so it bounds what is held rather than what has ever been committed.
#[test]
fn releasing_memory_gives_the_commit_ceiling_back() {
    let space = GuestSpace::with_config(GuestSpaceConfig {
        size: 64 * MIB,
        max_committed: 4 * MIB,
        max_commit_request: 4 * MIB,
        ..GuestSpaceConfig::default()
    })
    .expect("reserve");

    for round in 0..4 {
        let address = space
            .map_anonymous(
                Placement::Anywhere { align: 64 * KIB },
                4 * MIB,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert_eq!(space.stats().committed, 4 * MIB, "round {round}");
        space.unmap(address, 4 * MIB).expect("unmap");
        assert_eq!(space.stats().committed, 0, "round {round}: the total came back down");
    }

    // And `advise_idle` + `reclaim_idle`, the other path that returns commit charge.
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            4 * MIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("map");
    space.advise_idle(address, 4 * MIB).expect("advise");
    let reclaimed = space.reclaim_idle().expect("reclaim");
    assert_eq!(reclaimed.bytes, 4 * MIB);
    assert_eq!(space.stats().committed, 0, "reclaim_idle returns the ceiling too");
    space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            4 * MIB,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("the ceiling is available again");
    space.close().expect("close");
}

/// Lazy commit accumulates against the total ceiling one granule at a time, and is refused when it
/// arrives — not before, because a lazy mapping that is never touched costs nothing.
#[test]
fn lazy_commit_is_refused_when_it_reaches_the_total_ceiling() {
    let granule = 64 * KIB;
    let space = GuestSpace::with_config(GuestSpaceConfig {
        size: 64 * MIB,
        commit_granule: granule,
        max_committed: 4 * granule,
        max_commit_request: 4 * granule,
        ..GuestSpaceConfig::default()
    })
    .expect("reserve");
    let address = space
        .map_anonymous(
            Placement::Anywhere { align: 64 * KIB },
            MIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a 1 MiB lazy mapping costs nothing, whatever the ceiling is");

    for index in 0..4 {
        let committed = space
            .ensure_committed(address + index * granule, 1)
            .unwrap_or_else(|e| panic!("granule {index}: {e}"));
        assert_eq!(committed, granule, "granule {index}");
    }
    let error = space
        .ensure_committed(address + 4 * granule, 1)
        .expect_err("the fifth granule passes the ceiling");
    assert!(
        matches!(error, MemError::CommitCeiling { .. }),
        "expected CommitCeiling, got {error}"
    );
    assert_eq!(space.stats().committed, 4 * granule);
    assert_tiles_the_space(&space);
    space.close().expect("close");
}

/// An alignment larger than the space is refused instead of overflowing the free-range search.
///
/// `check_align_argument` accepted any power of two, and the search then computed
/// `(from + align - 1) & !(align - 1)`: at `1 << 63` that panics in a debug build and wraps to a
/// spurious `NoSpace` in release, which is a wrong answer rather than a refusal.
#[test]
fn an_absurd_alignment_is_refused_rather_than_wrapping() {
    let space = space(4 * MIB);
    for align in [1usize << 62, 1usize << 63, 8 * MIB] {
        let error = space
            .map_anonymous(
                Placement::Anywhere { align },
                4 * KIB,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect_err("an alignment larger than the space cannot be satisfied");
        match error {
            MemError::AlignmentTooLarge { align: reported, space_len, .. } => {
                assert_eq!(reported, align);
                assert_eq!(space_len, 4 * MIB);
            }
            other => panic!("expected AlignmentTooLarge for {align:#x}, got {other}"),
        }
        // A `Hint` placement routes through the same check.
        assert!(matches!(
            space
                .map_anonymous(
                    Placement::Hint { address: space.base(), align },
                    4 * KIB,
                    Protection::ReadWrite,
                    CommitPolicy::Lazy,
                )
                .expect_err("as does a hint"),
            MemError::AlignmentTooLarge { .. }
        ));
    }
    // The largest alignment that *is* satisfiable still works.
    space
        .map_anonymous(
            Placement::Anywhere { align: 4 * MIB },
            4 * KIB,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("an alignment equal to the space is satisfiable at its base");
    space.close().expect("close");
}

/// **An access may straddle a commit-granule boundary inside one mapping.**
///
/// A commit carves the map into entries that are each exactly one OS placeholder — `commit_range`
/// requires that, because a commit may not cross a placeholder — and adjacent committed granules are
/// never coalesced. So a lazily-committed mapping becomes a run of entries, and an access crossing a
/// granule boundary lands in two of them while being, to the guest, one ordinary access inside one
/// `mmap`.
///
/// Checking only the first entry refused every such access as `NotMapped` even with both granules
/// committed. MEASURED before the fix: an 8-byte read inside the first granule succeeded, a 16-byte
/// read straddling the boundary was refused.
///
/// Invisible until now because every other fixture is `CommitPolicy::Eager`, which is one entry that
/// is never split.
#[test]
fn an_access_may_straddle_a_commit_granule_boundary_within_one_mapping() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            4 * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazily-committed mapping");
    let boundary = base + granule;

    // Commit the two granules either side of the boundary, in separate calls, so the entries stay
    // split exactly as the demand pager would leave them.
    space.ensure_committed(base, 1).expect("commit the first granule");
    space.ensure_committed(boundary, 1).expect("commit the second granule");

    // The entries really are still separate: this is the precondition the test is about.
    let first = space.region_at(boundary - 8).expect("mapped");
    assert_eq!(
        first.end(),
        boundary,
        "adjacent committed granules are expected to stay separate entries",
    );

    // Wholly inside one entry: allowed before and after.
    omni_mem::admit(&space, boundary - 16, 8, omni_mem::FaultAccess::Read)
        .expect("an access inside one granule");

    // Straddling: this is the one that was refused.
    let admitted = omni_mem::admit(&space, boundary - 8, 16, omni_mem::FaultAccess::Read)
        .expect("an access straddling the boundary, inside one mapping");
    assert!(
        admitted.end >= boundary + 8,
        "the admitted extent must cover the whole access, got {:#x}",
        admitted.end,
    );
    assert_tiles_the_space(&space);
}

/// **A scan reaches across committed granules of one mapping**, which is the question `admit`
/// cannot be asked.
///
/// `admit` walks as far as the length it is given needs, so `admit(address, 1, ..)` reports the end
/// of the *first entry*. A caller with no length -- a C-string walk looking for a NUL -- that used
/// that as its bound would be bounded by a granule boundary, which is not a fact about the guest's
/// address space at all.
///
/// MEASURED, and it killed a thread: a real Roblox worker died during M6 startup on
/// "`__android_log_print` ... a string at 0x277dca62fa0 ... with no NUL in the first 96 bytes".
/// 96 was the distance to the next entry; the string was ordinary and NUL-terminated.
#[test]
fn a_scan_reaches_across_the_committed_granules_of_one_mapping() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            4 * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazily-committed mapping");
    let boundary = base + granule;
    space.ensure_committed(base, 1).expect("commit the first granule");
    space.ensure_committed(boundary, 1).expect("commit the second granule");

    // The precondition this test is about: the two granules are still separate entries.
    assert_eq!(
        space.region_at(boundary - 8).expect("mapped").end(),
        boundary,
        "adjacent committed granules are expected to stay separate entries",
    );
    // ...and that is exactly what a one-byte `admit` reports, which is the defect.
    assert_eq!(
        omni_mem::admit(&space, boundary - 96, 1, omni_mem::FaultAccess::Read)
            .expect("mapped")
            .end,
        boundary,
        "a one-byte admit reports the entry end -- the reading that bounded the walk at 96 bytes",
    );

    let reach = omni_mem::scan_reach(
        &space,
        boundary - 96,
        omni_mem::FaultAccess::Read,
        64 * 1024,
    )
    .expect("the address is readable");
    assert!(
        reach >= boundary + granule.min(64 * 1024 - 96),
        "the scan must cross into the second granule, got {reach:#x} against a boundary at \
         {boundary:#x}",
    );
    assert_tiles_the_space(&space);
}

/// **A scan stops at an uncommitted granule**, and does not commit one to look past it.
///
/// `admit` may commit under rule 4 because it was told a length the guest is about to touch. A scan
/// has no such length: committing a whole 64 KiB run to hunt for a NUL would charge D15's ceiling
/// for memory the guest never asked for. A string that really does continue into the next granule
/// continues into one the guest wrote, and that one is committed.
#[test]
fn a_scan_stops_at_an_uncommitted_granule_rather_than_committing_it() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            4 * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazily-committed mapping");
    space.ensure_committed(base, 1).expect("commit only the first granule");
    let committed_before = space.stats().committed;

    // **The limit has to exceed a granule or this test proves nothing.** MEASURED: it was
    // `64 * 1024`, which *is* the commit granule, so the ceiling landed exactly on the first
    // entry's end and the loop this test is about never executed. The mutation harness found it —
    // `access-B3`, which deletes the commit check, changed nothing and was reported NOT CAUGHT.
    // `VERIFICATION.md` entry 11: exercising is not detecting.
    let reach = omni_mem::scan_reach(&space, base, omni_mem::FaultAccess::Read, 4 * granule)
        .expect("the address is readable");
    assert_eq!(reach, base + granule, "the scan stops where the committed run does");
    assert_eq!(
        space.stats().committed,
        committed_before,
        "a scan must not commit the granule it declined to look into",
    );
    assert_tiles_the_space(&space);
}

/// **A scan does not leave its mapping**, however the neighbour is placed.
///
/// The over-correction the fix invites: two mappings butted together are still two mappings, and a
/// string that appears to run from one into the other is a guest bug, not a long string.
#[test]
fn a_scan_stops_at_the_end_of_its_own_mapping() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let first = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("the first mapping");
    let second = space
        .map_anonymous(
            Placement::Fixed(first + granule),
            granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a second mapping butted against the first");
    assert_eq!(second, first + granule, "the two mappings must be adjacent");
    space.ensure_committed(first, 1).expect("commit the first");
    space.ensure_committed(second, 1).expect("commit the second");

    // Four granules of limit against a one-granule mapping, for the reason the test above gives:
    // a limit that stops where the mapping stops cannot tell a scan that respects the boundary
    // from one that has never looked at it.
    let reach = omni_mem::scan_reach(&space, first, omni_mem::FaultAccess::Read, 4 * granule)
        .expect("the address is readable");
    assert_eq!(
        reach,
        first + granule,
        "the scan stops at the mapping boundary, with {second:#x} mapped and readable beyond it",
    );
    assert_tiles_the_space(&space);
}

/// **A scan stops where the protection stops**, inside one mapping it is otherwise entitled to.
///
/// The fourth of the loop's four conditions, and the only one a mapping's own extent cannot
/// express: `mprotect` can drop a range of a mapping to `PROT_NONE` under a reader, and the bytes
/// on the far side are still mapped, still committed and still part of the same mapping. A scan
/// that only asked "am I still in my mapping" would walk straight into them.
#[test]
fn a_scan_stops_where_the_protection_of_its_own_mapping_stops() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            4 * granule,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("an eagerly-committed mapping");
    // Drop the second granule to no access, leaving the first and the third readable. The scan
    // starts in the first, so the run it could walk is interrupted rather than ended.
    space
        .protect(base + granule, granule, Protection::None)
        .expect("drop the middle to no access");

    let reach = omni_mem::scan_reach(&space, base, omni_mem::FaultAccess::Read, 4 * granule)
        .expect("the address is readable");
    assert_eq!(
        reach,
        base + granule,
        "the scan stops at the protection boundary, not at the mapping's end",
    );
    // The precondition, so this cannot pass for the wrong reason: the range beyond really is still
    // part of the same mapping and really is still committed.
    let beyond = space.region_at(base + 2 * granule).expect("mapped");
    assert_eq!(
        beyond.mapping,
        space.region_at(base).expect("mapped").mapping,
        "the far side must be the same mapping, or this test is the mapping check again",
    );
    assert!(beyond.is_committed(), "the far side must be committed, or it is the commit check");
    assert_tiles_the_space(&space);
}

/// A scan that asks for less than the run is given less: the cap is the caller's, and it is kept.
#[test]
fn a_scan_is_bounded_by_the_limit_it_was_given() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            2 * granule,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("an eagerly-committed mapping");
    assert_eq!(
        omni_mem::scan_reach(&space, base, omni_mem::FaultAccess::Read, 32)
            .expect("the address is readable"),
        base + 32,
    );
}

/// An address that is not readable at all is the caller's error, and the rule that refused it is
/// the one `admit` would have named.
#[test]
fn a_scan_of_an_unreadable_address_refuses_with_the_rule_that_said_no() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            granule,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("a mapping");
    space.protect(base, granule, Protection::None).expect("drop to no access");
    assert_eq!(
        omni_mem::scan_reach(&space, base, omni_mem::FaultAccess::Read, 64),
        Err(omni_mem::Refusal::Protection),
    );
}

/// **...and it may straddle into an adjacent mapping, as it may on Linux.**
///
/// This test used to assert the opposite -- "two mappings placed adjacently are still two mappings,
/// and an access that crosses from one into the other is a refusal". Linux does not check accesses
/// per mapping, and the real engine proved it: Roblox 2.739.691's `init_array[188]` `memset`s 256
/// bytes across the seam between its last file-backed page and the `.bss` mapped after it, and the
/// old rule stopped the engine there. What still refuses is free space, and a neighbour whose
/// protection forbids the access. A lazily-committed neighbour is committed, and the extent reported
/// stays inside the first mapping.
#[test]
fn an_access_may_straddle_from_one_mapping_into_an_adjacent_one() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let first = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("the first mapping");
    // Place a second mapping immediately after the first, so the two are contiguous addresses but
    // distinct mappings.
    let second = space
        .map_anonymous(
            Placement::Fixed(first + granule),
            granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a second mapping butted against the first");
    assert_eq!(second, first + granule, "the two mappings must be adjacent");

    space.ensure_committed(first, 1).expect("commit the first");

    // Inside the first: fine.
    omni_mem::admit(&space, first, 8, omni_mem::FaultAccess::Write).expect("inside the first");

    // Across the seam, into a neighbour that is mapped but not yet committed: admitted, and the
    // neighbour's granule is committed by the same call.
    let admitted = omni_mem::admit(&space, second - 8, 16, omni_mem::FaultAccess::Write)
        .expect("an access crossing into an adjacent, writable mapping is an ordinary access");
    assert!(admitted.committed >= granule, "the neighbour's granule was committed ({})", admitted.committed);
    assert!(space.region_at(second).expect("the second mapping").is_committed());
    assert_eq!(admitted.end, second, "the extent reported stops where the first mapping stops");
    // SAFETY: both granules are committed read-write and belong to this test's space.
    unsafe { core::ptr::write_bytes(space.ptr(second - 8, 16).expect("host pointer"), 0xA5, 16) };

    // A neighbour whose protection forbids the access still refuses it.
    space.protect(second, granule, Protection::Read).expect("drop the second to read-only");
    let refusal = omni_mem::admit(&space, second - 8, 16, omni_mem::FaultAccess::Write)
        .expect_err("a write reaching a read-only neighbour");
    assert_eq!(refusal, omni_mem::Refusal::Protection);

    // And free space past the second still ends any access.
    let refusal = omni_mem::admit(&space, second + granule - 8, 16, omni_mem::FaultAccess::Read)
        .expect_err("an access running into free space");
    assert_eq!(refusal, omni_mem::Refusal::NotMapped);
    assert_tiles_the_space(&space);
}

// -------------------------------------------------------------------------------------------
// Shared file views: the guest's writable MAP_SHARED
// -------------------------------------------------------------------------------------------

/// Open a test file for reading and writing, as the guest's own descriptor would be.
fn read_write(file: &TempFile) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file.path())
        .expect("open the test file for reading and writing")
}

/// **A shared view writes its file, from the first store, through every protection it passes
/// through, and through the emulated partial unmap** -- and the file keeps its own length.
///
/// Mapped `Read` first and raised to `ReadWrite` on purpose: that is the order in which a view
/// created with the requested protection would silently become copy-on-write (its flavour is fixed
/// at creation), so this is the detector for "created `PAGE_READWRITE` and protected down". The
/// file's length is not a page multiple, which is the engine's case: it `posix_fallocate`s exactly
/// the bytes it is about to write and maps that many.
#[test]
fn a_shared_view_writes_its_file_through_a_protect_and_a_partial_unmap() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let len = 3 * page + 100;
    let file = TempFile::new("shared.bin", len, page);
    let backing = Backing::share(read_write(&file), "/data/shared.bin").expect("share the file");
    assert!(backing.is_shared());
    let map_len = 4 * page;
    let address = space
        .map_file(&backing, 0, Placement::Anywhere { align: page }, map_len, Protection::Read)
        .expect("a view ending inside the file's last page");
    assert_eq!(space.stats().committed, 0, "a shared view is the file's pages, not commit");
    space.protect(address, map_len, Protection::ReadWrite).expect("raise it");

    // SAFETY: the whole view is mapped and now writable; `len` is inside its last page.
    unsafe {
        fill(address + 10, 1, 0xA1);
        fill(address + len - 1, 1, 0xA2);
        fill(address + len, 1, 0xA3);
    }
    let disk = std::fs::read(file.path()).expect("read the file back");
    assert_eq!(disk.len(), len, "the view must not change the file's length");
    assert_eq!(disk[10], 0xA1, "a store through a shared view must reach the file");
    assert_eq!(disk[len - 1], 0xA2, "the file's last byte, from the view's last page");
    assert_eq!(disk[11], file.byte_at(11), "an untouched byte is the file's own");
    // SAFETY: the view's last page is mapped; past the file's end it is memory, not file.
    unsafe {
        assert_eq!(read(address + len), 0xA3, "the store past the end is held in the page");
        assert_eq!(read(address + len + 1), 0, "and the rest of that page reads as zero");
    }

    assert_eq!(space.sync(address, map_len).expect("sync"), map_len, "the whole view is shared");

    // Partial unmap: the head page goes, the survivors are mapped again from the same section.
    space.unmap(address, page).expect("unmap the head page");
    // SAFETY: the survivors are live and still writable.
    unsafe {
        assert_eq!(read(address + len - 1), 0xA2, "a survivor is still the file");
        fill(address + page + 20, 1, 0xB1);
    }
    let disk = std::fs::read(file.path()).expect("read the file back");
    assert_eq!(disk[page + 20], 0xB1, "a survivor of a partial unmap is still a shared view");
    assert_eq!(disk[10], 0xA1, "the unmapped head's write stays in the file, as on Linux");
    assert_tiles_the_space(&space);

    // With every view gone and the last `Arc` dropped, the section is closed: the host allows the
    // file to be shortened again, which it refuses while a section exists (1224).
    space.unmap(address + page, map_len - page).expect("unmap the rest");
    drop(backing);
    read_write(&file).set_len(1).expect("shorten the file once nothing maps it");
}

/// `sync` writes back shared views and **only** shared views, and skips free space rather than
/// refusing it: Linux's `msync` does both.
#[test]
fn sync_counts_only_shared_views_and_skips_holes() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let anonymous = space
        .map_anonymous(
            Placement::Anywhere { align: page },
            2 * page,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("anonymous memory");
    assert_eq!(space.sync(anonymous, 2 * page).expect("sync anonymous"), 0);

    let (_private_file, private) = backing("private.bin", 2 * page, page, MapExecutability::NonExecutable);
    let private_view = space
        .map_file(&private, 0, Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite)
        .expect("a private view");
    assert_eq!(space.sync(private_view, 2 * page).expect("sync private"), 0);

    let shared_file = TempFile::new("shared-sync.bin", 2 * page, page);
    let shared = Backing::share(read_write(&shared_file), "/data/s.bin").expect("share");
    let target = space.base() + 32 * MIB;
    let shared_view = space
        .map_file(&shared, 0, Placement::Fixed(target), page, Protection::ReadWrite)
        .expect("a shared view");
    // A range that starts on the shared page and runs into free space after it.
    assert_eq!(space.sync(shared_view, 4 * page).expect("a hole is skipped"), page);
}

/// A handle without write access cannot back a shared view: the section is refused, which is
/// Linux's `EACCES` for a `MAP_SHARED` writable mapping of a descriptor not open for writing.
/// An empty file cannot have one at all.
#[test]
fn a_shared_backing_needs_write_access_and_a_non_empty_file() {
    let page = omni_platform_page();
    let file = TempFile::new("read-only.bin", page, page);
    let read_only = std::fs::File::open(file.path()).expect("open read-only");
    let error = Backing::share(read_only, "/data/ro.bin").expect_err("must refuse");
    assert_eq!(
        error.platform_error().and_then(|e| e.os_error()).map(|e| e.code()),
        Some(5),
        "ERROR_ACCESS_DENIED: {error}"
    );

    let empty = TempFile::new("empty.bin", 0, page);
    let error = Backing::share(read_write(&empty), "/data/empty.bin").expect_err("must refuse");
    assert!(error.to_string().contains("0 bytes long"), "{error}");
}

fn omni_platform_page() -> usize {
    space(MIB).page_size()
}
