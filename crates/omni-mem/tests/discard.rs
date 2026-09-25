//! `GuestSpace::discard` -- the guest's `MADV_DONTNEED` -- at 4 KiB granularity on a space whose
//! page is larger: the range reads zero afterwards and **no byte outside it changes**.
//!
//! Found on the Mac (16 KiB host pages, 2026-09-25): `madvise(map + 4096, 4096, MADV_DONTNEED)`
//! was `EINVAL`, because the handler checked the range against the host page. A part of a host
//! page cannot be decommitted without the rest of that page, which is still the guest's, so it is
//! zeroed in place instead; only whole host pages are decommitted.
//!
//! Portable, and not only because every host runs it: each scenario runs at the host's own page
//! **and** at 16 KiB and 64 KiB pages through `GuestSpace::with_page_size`, so a 4 KiB host
//! (Windows, x86-64 Linux) checks the split a 16 KiB host makes, and the Mac checks it against its
//! real pages.

mod common;

use common::{KIB, MIB};
use omni_mem::{
    split_at_pages, CommitPolicy, Discarded, GuestSpace, GuestSpaceConfig, MemError, PageSplit,
    Placement, Protection, SMALL_PAGE,
};

/// The host's page, then every larger page size worth checking that this host can emulate.
fn page_sizes() -> Vec<usize> {
    let host = GuestSpace::with_config(GuestSpaceConfig { size: 64 * MIB, ..Default::default() })
        .expect("reserve")
        .page_size();
    let mut sizes = vec![host];
    for page in [16 * KIB, 64 * KIB] {
        if page > host && page % host == 0 {
            sizes.push(page);
        }
    }
    sizes
}

fn space_with_page(page: usize) -> GuestSpace {
    let config = GuestSpaceConfig { size: 64 * MIB, ..GuestSpaceConfig::default() };
    let space = GuestSpace::with_page_size(config, page).expect("reserve a guest address space");
    assert_eq!(space.page_size(), page);
    space
}

/// The byte a filled mapping holds at `offset`: which 4 KiB page it is in, plus one, so no page
/// of the pattern is zero and a zeroed page cannot pass for an intact one.
fn pattern(offset: usize) -> u8 {
    (offset / SMALL_PAGE + 1) as u8
}

/// A committed read-write mapping of `len` bytes, every byte set to [`pattern`].
fn filled(space: &GuestSpace, len: usize) -> usize {
    let anywhere = Placement::Anywhere { align: space.page_size() };
    let at = space
        .map_anonymous(anywhere, len, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("map");
    for offset in 0..len {
        // SAFETY: the mapping is committed and writable.
        unsafe { std::ptr::write_volatile((at + offset) as *mut u8, pattern(offset)) };
    }
    at
}

fn byte(address: usize) -> u8 {
    // SAFETY: every caller reads a committed, readable page of a mapping it made.
    unsafe { std::ptr::read_volatile(address as *const u8) }
}

/// Every byte of `[at, at + len)` against the pattern, except `[zero, zero + n)`, which is zero.
///
/// Commits the range first, as the demand pager would on the touch: a decommitted page is not
/// readable until then, and these tests run without a pager.
fn assert_only_zeroed(
    space: &GuestSpace,
    at: usize,
    len: usize,
    zero: usize,
    n: usize,
    what: &str,
) {
    space.ensure_committed(at, len).expect("commit the range back in");
    for offset in 0..len {
        let address = at + offset;
        let expected = if (zero..zero + n).contains(&address) { 0 } else { pattern(offset) };
        assert_eq!(
            byte(address),
            expected,
            "{what}: byte at +{offset:#x} (discarded [{:#x}, {:#x}))",
            zero - at,
            zero + n - at
        );
    }
}

/// What `discard` should report for a fully committed range: its whole pages decommitted, the
/// rest zeroed.
fn expected_for(at: usize, len: usize, page: usize) -> Discarded {
    let split = split_at_pages(at, len, page);
    Discarded {
        decommitted: split.whole.map_or(0, |(_, n)| n),
        zeroed: split.partial().map(|(_, n)| n).sum(),
    }
}

// ------------------------------------------------------------------------------ the split itself

/// Every 4 KiB-aligned range of up to 64 KiB starting in the first 64 KiB, split at 16 KiB pages:
/// the parts tile the range in order, the whole pages are pages, and the partial parts each lie
/// inside one page and exist exactly when the range starts or ends off a boundary.
#[test]
fn the_split_at_16k_pages_tiles_every_4k_range() {
    let page = 16 * KIB;
    for address in (0..64 * KIB).step_by(SMALL_PAGE) {
        for len in (SMALL_PAGE..=64 * KIB).step_by(SMALL_PAGE) {
            let end = address + len;
            let split = split_at_pages(address, len, page);
            let parts: Vec<(usize, usize)> =
                split.head.into_iter().chain(split.whole).chain(split.tail).collect();
            let mut cursor = address;
            for &(at, n) in &parts {
                assert_eq!(at, cursor, "{address:#x}+{len:#x}: {split:?} has a gap or overlap");
                assert_ne!(n, 0, "{address:#x}+{len:#x}: an empty part in {split:?}");
                cursor += n;
            }
            assert_eq!(cursor, end, "{address:#x}+{len:#x}: {split:?} does not reach the end");
            if let Some((at, n)) = split.whole {
                assert!(at % page == 0 && n % page == 0, "{split:?}: not whole pages");
            }
            for (at, n) in split.partial() {
                assert_eq!(at / page, (at + n - 1) / page, "{split:?}: a partial part spans pages");
                assert!(n < page, "{split:?}: a partial part is a whole page");
            }
            let case = format!("{address:#x}+{len:#x}: {split:?}");
            assert_eq!(split.head.is_some(), address % page != 0, "{case}");
            // An end off a boundary is the end of a partial part, and a tail only exists there.
            if end % page != 0 {
                assert!(split.partial().any(|(from, n)| from + n == end), "{case}");
            }
            assert!(split.tail.is_none() || end % page != 0, "{case}");
        }
    }
}

#[test]
fn the_split_names_its_parts() {
    let k = KIB;
    let page = 16 * k;
    let none = PageSplit { head: None, whole: None, tail: None };
    // Inside one page, touching neither edge: the pinned case, map + 4096 for 4096.
    let head = |at, n| PageSplit { head: Some((at, n)), ..none };
    assert_eq!(split_at_pages(4 * k, 4 * k, page), head(4 * k, 4 * k));
    // From a boundary into the page: a tail.
    assert_eq!(split_at_pages(0, 4 * k, page), PageSplit { tail: Some((0, 4 * k)), ..none });
    // Into a boundary: a head.
    assert_eq!(split_at_pages(12 * k, 4 * k, page), head(12 * k, 4 * k));
    // Across one boundary.
    assert_eq!(
        split_at_pages(12 * k, 8 * k, page),
        PageSplit { head: Some((12 * k, 4 * k)), tail: Some((16 * k, 4 * k)), ..none }
    );
    // Head, one whole page, tail.
    assert_eq!(
        split_at_pages(12 * k, 24 * k, page),
        PageSplit {
            head: Some((12 * k, 4 * k)),
            whole: Some((16 * k, 16 * k)),
            tail: Some((32 * k, 4 * k)),
        }
    );
    // At the page the range is aligned to, everything is whole.
    let whole = PageSplit { whole: Some((4 * k, 8 * k)), ..none };
    assert_eq!(split_at_pages(4 * k, 8 * k, 4 * k), whole);
}

// ------------------------------------------------------------------------------ discard

/// **The pinned case.** One 4 KiB page in the middle of a host page: it reads zero, and the other
/// bytes of the same host page -- on each side -- keep what was written.
#[test]
fn a_4k_discard_inside_a_larger_page_zeroes_it_and_keeps_its_neighbours() {
    for page in page_sizes() {
        let space = space_with_page(page);
        let len = 4 * page.max(16 * KIB);
        let at = filled(&space, len);
        let zero = at + SMALL_PAGE;
        let discarded = space.discard(zero, SMALL_PAGE).expect("discard");
        assert_eq!(discarded, expected_for(zero, SMALL_PAGE, page), "page {page:#x}");
        assert_only_zeroed(&space, at, len, zero, SMALL_PAGE, &format!("page {page:#x}"));
        // Still the guest's, still writable.
        // SAFETY: the page is mapped read-write and committed (or committed again on the touch).
        unsafe { std::ptr::write_volatile(zero as *mut u8, 0x5A) };
        assert_eq!(byte(zero), 0x5A, "page {page:#x}: the discarded page takes a write");
    }
}

/// A range with a partial head, whole pages and a partial tail: exactly the range reads zero, the
/// whole pages are handed back, and the partial ones stay committed.
#[test]
fn a_discard_across_page_boundaries_decommits_the_whole_pages_and_zeroes_the_ends() {
    for page in page_sizes() {
        let space = space_with_page(page);
        let len = 4 * page.max(16 * KIB);
        let at = filled(&space, len);
        let zero = at + page - SMALL_PAGE;
        let n = page + 2 * SMALL_PAGE;
        let committed = space.stats().committed;
        let discarded = space.discard(zero, n).expect("discard");
        assert_eq!(discarded, expected_for(zero, n, page), "page {page:#x}");
        assert_eq!(
            space.stats().committed,
            committed - discarded.decommitted,
            "page {page:#x}: only the whole pages give their commit charge back"
        );
        assert_only_zeroed(&space, at, len, zero, n, &format!("page {page:#x}"));
    }
}

/// A read-only page is zeroed too -- `MADV_DONTNEED` does not care about protection -- and keeps
/// its protection afterwards.
#[test]
fn a_discard_inside_a_read_only_page_zeroes_it_and_keeps_it_read_only() {
    for page in page_sizes() {
        let space = space_with_page(page);
        let len = 4 * page.max(16 * KIB);
        let at = filled(&space, len);
        space.protect(at, len, Protection::Read).expect("protect");
        let zero = at + 2 * SMALL_PAGE;
        space.discard(zero, SMALL_PAGE).expect("discard");
        assert_only_zeroed(&space, at, len, zero, SMALL_PAGE, &format!("page {page:#x}"));
        let region = space.region_at(zero).expect("still mapped");
        assert_eq!(region.protection, Protection::Read, "page {page:#x}");
    }
}

/// An inaccessible committed page as well: raised for the write, put back, and its other bytes
/// intact when it is opened again.
#[test]
fn a_discard_inside_an_inaccessible_page_zeroes_it_and_keeps_it_inaccessible() {
    for page in page_sizes() {
        let space = space_with_page(page);
        let len = 4 * page.max(16 * KIB);
        let at = filled(&space, len);
        space.protect(at, len, Protection::None).expect("protect none");
        let zero = at + SMALL_PAGE;
        space.discard(zero, SMALL_PAGE).expect("discard");
        assert_eq!(space.region_at(zero).expect("mapped").protection, Protection::None);
        space.protect(at, len, Protection::ReadWrite).expect("protect back");
        assert_only_zeroed(&space, at, len, zero, SMALL_PAGE, &format!("page {page:#x}"));
    }
}

/// Uncommitted memory already reads zero, and zeroing it would commit it: nothing is written.
#[test]
fn a_discard_of_uncommitted_memory_commits_nothing() {
    for page in page_sizes() {
        let space = space_with_page(page);
        let len = 4 * page.max(16 * KIB);
        let anywhere = Placement::Anywhere { align: page };
        let at = space
            .map_anonymous(anywhere, len, Protection::ReadWrite, CommitPolicy::Lazy)
            .expect("map");
        let discarded = space.discard(at + SMALL_PAGE, SMALL_PAGE).expect("discard");
        assert_eq!(discarded, Discarded::default(), "page {page:#x}");
        assert_eq!(space.stats().committed, 0, "page {page:#x}");
    }
}

/// 4 KiB-granular, not host-page-granular: a 4 KiB-aligned address is accepted on every page
/// size, an address off 4 KiB is not, and a length is rounded up to 4 KiB -- not to the page.
#[test]
fn a_discard_is_4k_granular_whatever_the_page() {
    for page in page_sizes() {
        let space = space_with_page(page);
        let len = 4 * page.max(16 * KIB);
        let at = filled(&space, len);
        assert!(matches!(space.discard(at + 1, SMALL_PAGE), Err(MemError::Misaligned { .. })));
        assert!(matches!(space.discard(at, 0), Err(MemError::ZeroSize { .. })));
        let zero = at + 3 * SMALL_PAGE;
        space.discard(zero, 1).expect("one byte is its 4 KiB page");
        assert_only_zeroed(&space, at, len, zero, SMALL_PAGE, &format!("page {page:#x}"));
    }
}

/// A page size the host cannot give is refused, not rounded.
#[test]
fn with_page_size_refuses_a_page_the_host_cannot_give() {
    let host = page_sizes()[0];
    let config = GuestSpaceConfig { size: 64 * MIB, ..GuestSpaceConfig::default() };
    for page in [host / 2, 3 * host, 0] {
        assert!(
            matches!(GuestSpace::with_page_size(config, page), Err(MemError::InvalidConfig { .. })),
            "page {page:#x} on a {host:#x} host"
        );
    }
}
