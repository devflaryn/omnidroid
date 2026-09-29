//! The 4 KiB guest overlay (spec 2026-09-29-4k-guest-pages): on a host whose page is larger and
//! that can alias, a space that asks for 4 KiB pages gets them; everywhere else nothing changes.
//! Every test that needs the overlay returns early where it is not active, so the file runs on
//! every host.
use omni_mem::{GuestSpace, GuestSpaceConfig, GUEST_PAGE};
use omni_platform::vm;

fn space(guest_page: Option<usize>) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, guest_page, ..GuestSpaceConfig::default() })
        .expect("a space")
}

fn overlay_expected() -> bool {
    vm::page_size() > GUEST_PAGE && vm::supports_alias()
}

#[test]
fn a_space_that_asks_gets_4_kib_pages_where_the_host_can_give_them() {
    let s = space(Some(GUEST_PAGE));
    assert_eq!(s.subpages_active(), overlay_expected());
    assert_eq!(s.guest_page_size(), if overlay_expected() { GUEST_PAGE } else { s.page_size() });
    assert!(!s.is_trapping(s.base()), "nothing traps in a fresh space");
}

#[test]
fn a_space_that_does_not_ask_is_as_before() {
    let s = space(None);
    assert!(!s.subpages_active());
    assert_eq!(s.guest_page_size(), s.page_size());
}

#[test]
fn only_4_kib_may_be_asked_for() {
    let e = GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, guest_page: Some(8192), ..GuestSpaceConfig::default() });
    assert!(e.is_err());
}

// ---- Task 4: mapping, protecting and unmapping 4 KiB parts --------------------------------------

use omni_mem::{AccessPtr, CommitPolicy, Placement, Protection};

fn host_page() -> usize {
    vm::page_size()
}

/// An active space and one committed read-write host page in it.
fn one_page() -> (GuestSpace, usize) {
    let s = space(Some(GUEST_PAGE));
    let at = s
        .map_anonymous(Placement::Anywhere { align: host_page() }, host_page(), Protection::ReadWrite, CommitPolicy::Eager)
        .unwrap();
    (s, at)
}

#[test]
fn two_parts_of_one_host_page_keep_two_protections() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert_eq!(s.region_at(at).unwrap().protection, Protection::Read);
    assert_eq!(s.region_at(at).unwrap().len, GUEST_PAGE);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().protection, Protection::ReadWrite);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().start, at + GUEST_PAGE);
    assert!(s.is_trapping(at), "a read-write part under a read-only host page traps");
}

#[test]
fn a_page_made_uniform_again_leaves_the_overlay() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    s.protect(at, GUEST_PAGE, Protection::ReadWrite).unwrap();
    assert!(!s.is_trapping(at));
    assert_eq!(s.region_at(at).unwrap().len, host_page(), "one region again");
    assert_eq!(s.split_stats().tracked, 0);
}

#[test]
fn a_4_kib_mapping_is_its_4_kib_and_the_rest_is_a_hole() {
    if !overlay_expected() {
        return;
    }
    let s = space(Some(GUEST_PAGE));
    let at = s.map_anonymous(Placement::Anywhere { align: host_page() }, GUEST_PAGE, Protection::ReadWrite, CommitPolicy::Lazy).unwrap();
    assert_eq!(at % host_page(), 0, "unhinted: host-page-aligned");
    assert_eq!(s.region_at(at).unwrap().len, GUEST_PAGE);
    assert!(s.region_at(at + GUEST_PAGE).is_none(), "the tail is a hole");
    assert!(!s.is_trapping(at), "a lenient hole costs nothing");
    let maps: Vec<_> = s.mapped_regions().into_iter().filter(|r| r.start >= at && r.start < at + host_page()).collect();
    assert_eq!(maps.len(), 1, "{maps:?}");
    assert_eq!(maps[0].len, GUEST_PAGE);
}

#[test]
fn unmapping_every_part_frees_the_host_page() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.unmap(at, GUEST_PAGE).unwrap();
    assert!(s.region_at(at).is_none());
    s.unmap(at + GUEST_PAGE, host_page() - GUEST_PAGE).unwrap();
    assert!(s.regions().iter().any(|r| r.is_free() && r.start <= at && r.end() >= at + host_page()), "free again");
    assert_eq!(s.split_stats().tracked, 0);
}

#[test]
fn a_fixed_map_over_part_of_a_split_page_keeps_the_neighbours() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    // SAFETY: the page is committed and read-write.
    unsafe { (s.host_addr(at) as *mut u8).write(7) };
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    s.map_anonymous(Placement::Fixed(at + GUEST_PAGE), GUEST_PAGE, Protection::Read, CommitPolicy::Lazy).unwrap();
    // SAFETY: the first part is still mapped read-write, and the host page is readable.
    assert_eq!(unsafe { (s.host_addr(at) as *const u8).read() }, 7, "the neighbour's byte survives");
    assert_eq!(s.region_at(at).unwrap().protection, Protection::ReadWrite);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().protection, Protection::Read);
    assert!(s.map_anonymous(Placement::Fixed(at), GUEST_PAGE, Protection::Read, CommitPolicy::Lazy).is_err(), "occupied");
}

#[test]
fn a_part_mapped_again_reads_zero() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    // SAFETY: committed, read-write.
    unsafe { (s.host_addr(at + GUEST_PAGE) as *mut u8).write(9) };
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    s.map_anonymous(Placement::Fixed(at + GUEST_PAGE), GUEST_PAGE, Protection::ReadWrite, CommitPolicy::Lazy).unwrap();
    // SAFETY: the host page is read-write again (uniform).
    assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u8).read() }, 0);
}

#[test]
fn protect_over_a_hole_is_refused_and_changes_nothing() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert!(s.protect(at, 2 * GUEST_PAGE, Protection::Read).is_err());
    assert_eq!(s.region_at(at).unwrap().protection, Protection::ReadWrite, "unchanged");
}

#[test]
fn execute_is_the_guest_view_not_the_hosts() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::ReadExecute).unwrap();
    assert!(s.any_executable(at, GUEST_PAGE));
    assert!(!s.any_executable(at + GUEST_PAGE, GUEST_PAGE));
}

#[test]
fn a_whole_page_protect_over_a_tracked_page_makes_it_uniform() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert!(s.is_trapping(at));
    s.protect(at, host_page(), Protection::Read).unwrap();
    assert!(!s.is_trapping(at));
    assert_eq!(s.region_at(at).unwrap().len, host_page());
}

// ---- Task 5: reaching a split page ---------------------------------------------------------------

#[test]
fn a_trapping_page_is_reached_through_its_alias_and_the_host_sees_the_write() {
    if !overlay_expected() {
        assert!(matches!(space(Some(GUEST_PAGE)).access_ptr(0, 8), AccessPtr::Direct(_)));
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 8) else { panic!("alias") };
    // SAFETY: the alias is read-write by contract.
    unsafe { p.write(9) };
    // SAFETY: the host page is read-only now, so a read is allowed.
    assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u8).read() }, 9);
}

#[test]
fn a_range_across_a_trapping_and_an_ordinary_page_is_copied_in_chunks() {
    if !overlay_expected() {
        return;
    }
    let s = space(Some(GUEST_PAGE));
    let at = s.map_anonymous(Placement::Anywhere { align: host_page() }, 2 * host_page(), Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert_eq!(s.access_ptr(at + host_page() - 8, 16), AccessPtr::Straddle);
    let mut chunks = Vec::new();
    s.for_each_access_chunk(at + host_page() - 8, 16, |g, _, n| chunks.push((g, n)));
    assert_eq!(chunks, vec![(at + host_page() - 8, 8), (at + host_page(), 8)]);
}

#[test]
fn write_forced_into_a_read_only_part_lands_and_keeps_its_protection() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    s.write_forced(at, &[5]).unwrap();
    // SAFETY: the host page is readable.
    assert_eq!(unsafe { (s.host_addr(at) as *const u8).read() }, 5);
    assert_eq!(s.region_at(at).unwrap().protection, Protection::Read);
    assert!(s.is_trapping(at));
}

#[test]
fn discard_of_a_split_part_zeroes_it_and_keeps_the_alias_live() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 1) else { panic!() };
    // SAFETY: the alias is read-write.
    unsafe { p.write(3) };
    s.discard(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    // SAFETY: as above.
    assert_eq!(unsafe { p.read() }, 0);
    unsafe { p.write(4) };
    // SAFETY: the host page is readable.
    assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u8).read() }, 4, "still one memory");
}

#[test]
fn a_strict_gap_makes_its_host_page_trap_and_a_lenient_one_does_not() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.set_strict_gaps(at, host_page(), true);
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert!(s.is_trapping(at), "read-write parts beside a strict gap trap");
    let (l, at2) = one_page();
    l.unmap(at2 + GUEST_PAGE, GUEST_PAGE).unwrap();
    assert!(!l.is_trapping(at2));
}

#[test]
fn splitting_a_file_view_page_keeps_its_bytes() {
    if !overlay_expected() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("omni-subpage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    let bytes: Vec<u8> = (0..host_page()).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let s = space(Some(GUEST_PAGE));
    let backing = omni_mem::Backing::open_named(&path, omni_mem::MapExecutability::NonExecutable, "f").unwrap();
    let at = s.map_file(&backing, 0, Placement::Anywhere { align: host_page() }, host_page(), Protection::Read).unwrap();
    s.protect(at + GUEST_PAGE, GUEST_PAGE, Protection::ReadWrite).unwrap();
    assert!(s.is_trapping(at));
    // SAFETY: the host page is readable (the intersection of Read and ReadWrite).
    let got = unsafe { std::slice::from_raw_parts(s.host_addr(at) as *const u8, host_page()) };
    assert_eq!(got, &bytes[..]);
}

#[test]
fn stats_count_what_is_tracked_and_served() {
    if !overlay_expected() {
        assert_eq!(space(Some(GUEST_PAGE)).split_stats(), omni_mem::SplitStats::default());
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    s.note_split_served(at + GUEST_PAGE);
    let st = s.split_stats();
    assert_eq!((st.tracked, st.trapping, st.served_total), (1, 1, 1));
    assert_eq!(st.top, vec![(at, 1)]);
}

// ---- Checkpoint A findings -----------------------------------------------------------------------

fn space_with(cfg: GuestSpaceConfig) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, guest_page: Some(GUEST_PAGE), ..cfg }).expect("a space")
}

/// A served write through the alias while another part of the same, still trapping, page is
/// re-protected: the alias must stay writable throughout (it was re-made, briefly read-only, on
/// every change: SIGBUS).
#[test]
fn a_served_write_survives_a_concurrent_protect_of_another_part() {
    if !overlay_expected() {
        return;
    }
    let s = std::sync::Arc::new(space(Some(GUEST_PAGE)));
    let page = host_page();
    let at = s.map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (s, stop) = (std::sync::Arc::clone(&s), std::sync::Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 8) else { panic!("an alias") };
                // SAFETY: the alias is read-write for as long as the page traps.
                unsafe { (p as *mut u64).write_volatile(n) };
                n += 1;
            }
        })
    };
    for i in 0..20_000 {
        s.protect(at + 2 * GUEST_PAGE, GUEST_PAGE, if i % 2 == 0 { Protection::Read } else { Protection::ReadWrite }).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}

/// Two views of one shared backing (a memfd, ashmem, a MAP_SHARED file): splitting one keeps it
/// the same memory as the other.
#[test]
fn splitting_a_shared_view_keeps_it_shared() {
    if !overlay_expected() {
        return;
    }
    let page = host_page();
    let path = std::env::temp_dir().join(format!("omni-subpage-shared-{}", std::process::id()));
    std::fs::write(&path, vec![0u8; page]).unwrap();
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let s = space(Some(GUEST_PAGE));
    let backing = omni_mem::Backing::share(file, "shm").unwrap();
    let a = s.map_file(&backing, 0, Placement::Anywhere { align: page }, page, Protection::ReadWrite).unwrap();
    let b = s.map_file(&backing, 0, Placement::Anywhere { align: page }, page, Protection::ReadWrite).unwrap();
    s.protect(a, GUEST_PAGE, Protection::Read).unwrap();
    // SAFETY: b is an ordinary read-write view.
    unsafe { (s.host_addr(b + GUEST_PAGE) as *mut u64).write_volatile(2) };
    let AccessPtr::Alias(p) = s.access_ptr(a + GUEST_PAGE, 8) else { panic!("an alias") };
    // SAFETY: the alias is read-write.
    assert_eq!(unsafe { (p as *const u64).read_volatile() }, 2, "a still sees b's write");
    assert!(matches!(s.region_at(a + GUEST_PAGE).unwrap().kind, omni_mem::RegionKind::File { .. }));
    let _ = std::fs::remove_file(path);
}

/// A private file view's split page keeps its file identity in the guest's view (`/proc/maps`).
#[test]
fn a_split_file_page_is_still_the_file_in_the_guests_view() {
    if !overlay_expected() {
        return;
    }
    let page = host_page();
    let dir = std::env::temp_dir().join(format!("omni-subpage-id-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lib.so");
    std::fs::write(&path, vec![0x5a; 2 * page]).unwrap();
    let s = space(Some(GUEST_PAGE));
    let backing = omni_mem::Backing::open_named(&path, omni_mem::MapExecutability::NonExecutable, "lib.so").unwrap();
    let f = s.map_file(&backing, 0, Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite).unwrap();
    s.protect(f + GUEST_PAGE, GUEST_PAGE, Protection::Read).unwrap();
    for at in [f, f + GUEST_PAGE, f + page] {
        let r = s.region_at(at).unwrap();
        let omni_mem::RegionKind::File { file_offset, .. } = r.kind else { panic!("{at:#x} is {:?}", r.kind) };
        assert_eq!(file_offset, (at - f) as u64, "{at:#x}");
    }
    let files = s.mapped_regions().into_iter().filter(|r| r.start >= f && r.start < f + 2 * page).all(|r| matches!(r.kind, omni_mem::RegionKind::File { .. }));
    assert!(files, "every piece of the mapping is the file");
}

/// A split near the commit ceiling is refused with nothing changed: the file page stays mapped.
#[test]
fn a_split_refused_at_the_commit_ceiling_changes_nothing() {
    if !overlay_expected() {
        return;
    }
    let page = host_page();
    let path = std::env::temp_dir().join(format!("omni-subpage-ceiling-{}", std::process::id()));
    std::fs::write(&path, vec![0x5a; 2 * page]).unwrap();
    let s = space_with(GuestSpaceConfig { max_committed: page, max_commit_request: page, ..GuestSpaceConfig::default() });
    s.map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    let backing = omni_mem::Backing::open(&path, omni_mem::MapExecutability::NonExecutable).unwrap();
    let f = s.map_file(&backing, 0, Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite).unwrap();
    assert!(s.protect(f + GUEST_PAGE, GUEST_PAGE, Protection::Read).is_err(), "no commit left for the private copy");
    let r = s.region_at(f).expect("the file page is still mapped");
    assert_eq!(r.protection, Protection::ReadWrite);
    assert!(!s.is_trapping(f));
    let _ = std::fs::remove_file(path);
}

/// Reclaiming idle memory never takes a tracked page's memory from under its alias.
#[test]
fn reclaim_leaves_a_trapping_page_alone() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 8) else { panic!() };
    // SAFETY: read-write alias.
    unsafe { (p as *mut u64).write_volatile(0x77) };
    s.advise_idle(at, host_page()).unwrap();
    s.reclaim_idle().unwrap();
    assert!(s.is_trapping(at));
    // SAFETY: the host page is readable.
    assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u64).read_volatile() }, 0x77, "kept");
}

/// `held_ranges` (what `mremap` copies) is the guest's view: a hole in a tracked page is not held.
#[test]
fn held_ranges_leave_out_a_split_pages_holes() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.unmap(at + GUEST_PAGE, GUEST_PAGE).unwrap();
    let held = s.held_ranges(at, host_page());
    assert_eq!(held, vec![(at, GUEST_PAGE), (at + 2 * GUEST_PAGE, host_page() - 2 * GUEST_PAGE)]);
}

/// A PROT_NONE 4 KiB beside accessible parts of its host page (a library's reservation filler, a
/// 4 KiB guard): lenient by default -- the live parts do not trap -- and enforced in a strict
/// range. The guest's view says PROT_NONE either way.
#[test]
fn a_prot_none_part_is_lenient_unless_its_range_is_strict() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::None).unwrap();
    assert!(!s.is_trapping(at), "the live parts are reached directly");
    assert_eq!(s.region_at(at).unwrap().protection, Protection::None, "the guest's view");
    let (strict, at2) = one_page();
    strict.set_strict_gaps(at2, host_page(), true);
    strict.protect(at2, GUEST_PAGE, Protection::None).unwrap();
    assert!(strict.is_trapping(at2), "strict: the PROT_NONE part faults, so its neighbours trap");
}
