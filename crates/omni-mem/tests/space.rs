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
        RegionKind::File { backing: id, file_offset, name } => {
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

    eprintln!(
        "a {} MiB view: whole unmap {whole:?}; hole punched through one piece {hole:?}; \
         hole punched through a view carved into 16 pieces {carved:?}",
        len / MIB
    );
    // Loose on purpose: this is a cost report, not a performance gate, and a gate tuned to this
    // machine would fail on a slower one for no useful reason. What it does rule out is the
    // emulation being pathological — quadratic in the view's size, say, or re-mapping every piece
    // when only two survive.
    assert!(hole < std::time::Duration::from_millis(20), "punching a hole took {hole:?}");
    assert!(carved < std::time::Duration::from_millis(50), "punching a hole took {carved:?}");
    assert_tiles_the_space(&space);
}
