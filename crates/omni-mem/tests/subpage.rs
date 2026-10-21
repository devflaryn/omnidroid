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

/// Code beside data in one host page (a JIT's): the read-execute part does not forbid writes on the
/// host, so the data part's writes do not trap; the guest still sees each part's protection.
#[test]
fn code_beside_data_does_not_trap() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.protect(at, GUEST_PAGE, Protection::ReadExecute).unwrap();
    assert_eq!(s.region_at(at).unwrap().protection, Protection::ReadExecute);
    assert_eq!(s.region_at(at + GUEST_PAGE).unwrap().protection, Protection::ReadWrite);
    assert!(!s.is_trapping(at + GUEST_PAGE), "the data part is written without a trap");
}

/// A library's RELRO sealed read-only beside its `.data`: once said sealed, its host page does not
/// trap; an unsealed read-only part does (above); and an unmap forgets the seal.
#[test]
fn sealed_relro_beside_data_does_not_trap_and_an_unmap_forgets_it() {
    if !overlay_expected() {
        return;
    }
    let (s, at) = one_page();
    s.set_sealed_read_only(at, GUEST_PAGE);
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert_eq!(s.region_at(at).unwrap().protection, Protection::Read, "the guest sees it read-only");
    assert!(!s.is_trapping(at + GUEST_PAGE), "the data part is written without a trap");
    s.unmap(at, host_page()).unwrap();
    let again = s.map_anonymous(Placement::Fixed(at), host_page(), Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    assert_eq!(again, at);
    s.protect(at, GUEST_PAGE, Protection::Read).unwrap();
    assert!(s.is_trapping(at), "a new mapping there is not sealed");
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

/// `MADV_DONTNEED` on a part of a shared view's tracked host page leaves the file as it is, as
/// Linux's does: zeros written through the page's alias were written into the file. And on a part
/// of a host page wholly past the file's end it touches nothing -- the alias's store there was the
/// host's `SIGBUS`, outside the guest space -- where an access is the guest's `SIGBUS`, but not in
/// the page's hole (the guest's `SIGSEGV`, as on Linux).
#[test]
fn a_discard_in_a_shared_views_tracked_page_leaves_the_file() {
    if !overlay_expected() {
        return;
    }
    let page = host_page();
    let path = |len: usize| std::env::temp_dir().join(format!("omni-subpage-discard-shared-{len}-{}", std::process::id()));
    let shared = |len: usize| {
        std::fs::write(path(len), vec![0x68u8; len]).unwrap();
        omni_mem::Backing::share(std::fs::OpenOptions::new().read(true).write(true).open(path(len)).unwrap(), "/data/shared.bin").unwrap()
    };
    let s = space(Some(GUEST_PAGE));
    let len = page + 2 * GUEST_PAGE;
    let at = s.map_file(&shared(len), 0, Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite).unwrap();
    s.unmap(at + page + 2 * GUEST_PAGE, page - 2 * GUEST_PAGE).unwrap();
    s.discard(at + page, GUEST_PAGE).unwrap();
    assert_eq!(std::fs::read(path(len)).unwrap()[page], 0x68, "the file's byte stays");
    let _ = std::fs::remove_file(path(len));
    if omni_platform::vm::shared_view_may_pass_end_of_file() {
        let at = s.map_file(&shared(GUEST_PAGE), 0, Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite).unwrap();
        s.unmap(at + page + GUEST_PAGE, page - GUEST_PAGE).unwrap();
        s.discard(at + page, GUEST_PAGE).unwrap();
        assert!(s.past_end_of_file(at + page, omni_mem::FaultAccess::Read), "mapped, past the end: SIGBUS");
        assert!(!s.past_end_of_file(at + page + 2 * GUEST_PAGE, omni_mem::FaultAccess::Read), "a hole: SIGSEGV");
        assert_eq!(std::fs::read(path(GUEST_PAGE)).unwrap(), vec![0x68u8; GUEST_PAGE], "the file as it was");
        let _ = std::fs::remove_file(path(GUEST_PAGE));
    }
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

/// A space may be terabytes (the boot's first 4 KiB-page run had a 78 TiB one): a page's alias
/// takes one page of address space, not the space's size -- a space-sized alias reservation failed
/// with ENOMEM there, and every fixed map that split a page in it with it.
#[test]
fn a_split_in_a_terabyte_space_takes_one_page_of_alias() {
    if !overlay_expected() {
        return;
    }
    let s = GuestSpace::with_config(GuestSpaceConfig { size: 1 << 44, guest_page: Some(GUEST_PAGE), ..GuestSpaceConfig::default() })
        .expect("a 16 TiB space");
    let page = host_page();
    let mut pages = Vec::new();
    for _ in 0..8 {
        let at = s.map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager).unwrap();
        s.protect(at, GUEST_PAGE, Protection::Read).expect("split");
        assert!(s.is_trapping(at));
        pages.push(at);
    }
    for at in pages {
        let AccessPtr::Alias(p) = s.access_ptr(at + GUEST_PAGE, 8) else { panic!("an alias") };
        // SAFETY: the alias is read-write.
        unsafe { (p as *mut u64).write(at as u64) };
        // SAFETY: the host page is readable.
        assert_eq!(unsafe { (s.host_addr(at + GUEST_PAGE) as *const u64).read() }, at as u64, "each page its own alias");
    }
}

/// `usage()` is `mapped_regions()` added up -- `mapped` exactly; `committed` exactly without the
/// overlay and never more with it (a split page's pieces of a partly committed region no longer
/// each count up to the region's whole commit) -- across a seeded mix of maps, partial commits,
/// protects and unmaps at the space's guest page, and it is remembered only while the map is
/// unchanged.
#[test]
fn usage_adds_up_to_the_mapped_regions() {
    for guest_page in [None, Some(GUEST_PAGE)] {
        let s = space(guest_page);
        let unit = s.guest_page_size();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut maps: Vec<(usize, usize)> = Vec::new();
        let mut most_tracked = 0;
        for step in 0..600 {
            match next(5) {
                0 | 1 if maps.len() < 64 => {
                    let len = (1 + next(24)) * unit;
                    let commit = if next(2) == 0 { CommitPolicy::Eager } else { CommitPolicy::Lazy };
                    if let Ok(at) = s.map_anonymous(Placement::Anywhere { align: host_page() }, len, Protection::ReadWrite, commit) {
                        if commit == CommitPolicy::Lazy && next(2) == 0 {
                            let _ = s.ensure_committed(at + next(len / unit) * unit, unit);
                        }
                        maps.push((at, len));
                    }
                }
                2 if !maps.is_empty() => {
                    let (at, len) = maps[next(maps.len())];
                    let from = next(len / unit) * unit;
                    let n = (1 + next((len - from) / unit)) * unit;
                    let protection = [Protection::Read, Protection::ReadWrite, Protection::None, Protection::ReadExecute][next(4)];
                    let _ = s.protect(at + from, n, protection);
                }
                3 if !maps.is_empty() => {
                    let i = next(maps.len());
                    let (at, len) = maps[i];
                    let from = next(len / unit) * unit;
                    let n = (1 + next((len - from) / unit)) * unit;
                    if s.unmap(at + from, n).is_ok() && from == 0 && n == len {
                        maps.swap_remove(i);
                    }
                }
                _ => {}
            }
            let regions = s.mapped_regions();
            let usage = s.usage();
            let mapped: usize = regions.iter().map(|r| r.len).sum();
            let committed: usize = regions.iter().map(|r| r.committed).sum();
            assert_eq!(usage.mapped, mapped, "step {step}, guest page {guest_page:?}");
            if s.subpages_active() {
                assert!(usage.committed <= committed, "step {step}: {} > {committed}", usage.committed);
            } else {
                assert_eq!(usage.committed, committed, "step {step}, guest page {guest_page:?}");
            }
            assert_eq!(s.usage(), usage, "remembered while nothing changed");
            most_tracked = most_tracked.max(s.split_stats().tracked);
        }
        assert!(s.usage().mapped > 0, "the mix left something mapped");
        assert_eq!(most_tracked > 0, s.subpages_active(), "split pages taken in step where the overlay is");
    }
}

/// **What `/proc/<pid>/stat|status|statm` cost the space** (`--ignored --nocapture`), over 3,500
/// regions (the Roblox app host's at Home): two walks of `mapped_regions` (an open before), one
/// walk, `usage()` after a change (a no-op protect bumps the generation; its own cost taken off),
/// and `usage()` remembered.
#[test]
#[ignore = "a measurement; run by hand"]
fn what_a_usage_figure_costs() {
    for guest_page in [None, Some(GUEST_PAGE)] {
        let s = space(guest_page);
        let page = host_page();
        let mut first = 0;
        for i in 0..3500 {
            let commit = if i % 2 == 0 { CommitPolicy::Eager } else { CommitPolicy::Lazy };
            let at = s.map_anonymous(Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite, commit).unwrap();
            if i % 3 == 0 {
                s.protect(at, s.guest_page_size(), Protection::Read).unwrap();
            }
            if i == 0 {
                first = at;
            }
        }
        let regions = s.mapped_regions().len();
        let time = |n: u32, f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            for _ in 0..n {
                f();
            }
            t.elapsed().as_nanos() as f64 / f64::from(n) / 1000.0
        };
        let two = time(200, &mut || {
            let a: usize = s.mapped_regions().iter().map(|r| r.len).sum();
            let b: usize = s.mapped_regions().iter().map(|r| r.committed).sum();
            assert!(a > b);
        });
        let one = time(200, &mut || {
            let (a, b) = s.mapped_regions().iter().fold((0, 0), |(m, c), r| (m + r.len, c + r.committed));
            assert!(a > b);
        });
        let protect = time(2000, &mut || s.protect(first, page, Protection::ReadWrite).unwrap());
        let changed = time(2000, &mut || {
            s.protect(first, page, Protection::ReadWrite).unwrap();
            assert!(s.usage().mapped > 0);
        }) - protect;
        let kept = time(200_000, &mut || assert!(s.usage().mapped > 0));
        eprintln!(
            "[usage] guest page {guest_page:?}, {regions} regions, {} tracked pages: two walks {two:.1} us, one walk {one:.1} us, usage after a change {changed:.1} us, remembered {:.3} us",
            s.split_stats().tracked,
            kept
        );
    }
}

// ---- A file view under edge host pages it shares (`map_file_exact`) -----------------------------

/// A file of `pages` 4 KiB pages, page `i` all `i + 1`, and a private backing of it.
fn paged_file(name: &str, pages: usize) -> (std::path::PathBuf, Vec<u8>, std::sync::Arc<omni_mem::Backing>) {
    let path = std::env::temp_dir().join(format!("omni-subpage-{name}-{}", std::process::id()));
    let bytes: Vec<u8> = (0..pages * GUEST_PAGE).map(|i| (i / GUEST_PAGE) as u8 + 1).collect();
    std::fs::write(&path, &bytes).unwrap();
    let backing = omni_mem::Backing::open_named(&path, omni_mem::MapExecutability::Executable, name).unwrap();
    (path, bytes, backing)
}

/// The dynamic linker's reservation for a library: `pages` host pages, `PROT_NONE`, never touched.
fn reservation(s: &GuestSpace, pages: usize) -> usize {
    s.map_anonymous(Placement::Anywhere { align: host_page() }, pages * host_page(), Protection::None, CommitPolicy::Lazy).unwrap()
}

/// The byte at `at`, read where an admitted access goes.
fn byte(s: &GuestSpace, at: usize) -> u8 {
    let (AccessPtr::Direct(p) | AccessPtr::Alias(p)) = s.access_ptr(at, 1) else { panic!("one page") };
    // SAFETY: the tests read only parts the guest's view allows, through their host page or alias.
    unsafe { p.read_volatile() }
}

/// Write `v` at `at` where an admitted access goes.
fn poke(s: &GuestSpace, at: usize, v: u8) {
    let (AccessPtr::Direct(p) | AccessPtr::Alias(p)) = s.access_ptr(at, 1) else { panic!("one page") };
    // SAFETY: the tests write only parts the guest's view lets them write.
    unsafe { p.write_volatile(v) };
}

/// A segment from the last 4 KiB of a reservation's host page 0 to the first of its host page 2,
/// mapped as the linker maps one (MAP_FIXED: unmapped first): every host page is the view, the
/// edge pages' other parts stay the reservation's, and nothing is a private copy.
#[test]
fn a_view_sits_under_a_reservations_edge_pages() {
    if !overlay_expected() {
        return;
    }
    let (page, gp) = (host_page(), GUEST_PAGE);
    let (path, bytes, backing) = paged_file("reserve", 4 * page / gp);
    let s = space(Some(GUEST_PAGE));
    let r = reservation(&s, 4);
    let (start, end) = (r + page - gp, r + 2 * page + gp);
    s.unmap(start, end - start).unwrap();
    let off = (start - r) as u64;
    assert_eq!(s.map_file_exact(&backing, off, start, end - start, Protection::Read).unwrap(), (start, end - start), "every page a view");
    for at in [start, r + page, end - gp] {
        let reg = s.region_at(at).unwrap();
        let omni_mem::RegionKind::File { file_offset, .. } = reg.kind else { panic!("{at:#x} is {:?}", reg.kind) };
        assert_eq!((file_offset, reg.protection), (off + (at - start) as u64, Protection::Read), "{at:#x}");
        assert_eq!(byte(&s, at), bytes[off as usize + (at - start)], "{at:#x}");
    }
    for at in [r, end] {
        let reg = s.region_at(at).unwrap();
        assert_eq!((reg.kind, reg.protection, reg.committed), (omni_mem::RegionKind::Anonymous, Protection::None, 0), "{at:#x}: the reservation's");
    }
    assert_eq!(s.held_ranges(r, page), vec![(start, gp)], "a reservation's part holds nothing");
    let st = s.split_stats();
    assert_eq!((st.tracked, st.trapping), (2, 0), "two edge pages, nothing traps");
    assert_eq!(s.stats().committed, 0, "no private copy");
    let _ = std::fs::remove_file(path);
}

/// A reservation's part over the view reads zero once it is made accessible (mprotect) or mapped
/// over (MAP_FIXED anonymous), and its writes reach no file; the segment's parts stay the file's.
#[test]
fn a_reservation_part_reads_zero_once_made_accessible_or_mapped_over() {
    if !overlay_expected() {
        return;
    }
    let (page, gp) = (host_page(), GUEST_PAGE);
    let (path, bytes, backing) = paged_file("zero", 4 * page / gp);
    let s = space(Some(GUEST_PAGE));
    let r = reservation(&s, 4);
    let (start, end) = (r + page - gp, r + 2 * page + gp);
    s.unmap(start, end - start).unwrap();
    s.map_file_exact(&backing, (start - r) as u64, start, end - start, Protection::Read).unwrap();
    s.protect(r, gp, Protection::ReadWrite).unwrap();
    assert_eq!(byte(&s, r), 0, "mprotected: zero");
    poke(&s, r, 0xee);
    s.protect(r + gp, gp, Protection::Read).unwrap();
    assert_eq!(byte(&s, r + gp), 0, "a reservation part beside it, its page a copy now: zero too");
    assert_eq!(byte(&s, start), bytes[start - r], "the segment's part is the file's still");
    s.unmap(end, gp).unwrap();
    s.map_anonymous(Placement::Fixed(end), gp, Protection::ReadWrite, CommitPolicy::Lazy).unwrap();
    assert_eq!(byte(&s, end), 0, "mapped over: zero");
    assert_eq!(byte(&s, end - gp), bytes[end - gp - r]);
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "the file is not written");
    let _ = std::fs::remove_file(path);
}

/// `r--`, `r-x` and `rw-` segments of one file, each starting in the host page the one before ends
/// in: each shared page is one view, each part keeps its protection, a write to the `rw-` part
/// never reaches the file -- and a page once writable is not shared again.
#[test]
fn neighbouring_segments_of_one_file_share_their_edge_page_view() {
    if !overlay_expected() {
        return;
    }
    let (page, gp) = (host_page(), GUEST_PAGE);
    let (path, bytes, backing) = paged_file("segments", 4 * page / gp);
    let s = space(Some(GUEST_PAGE));
    let r = reservation(&s, 4);
    let segs = [
        (r + gp, r + page + 2 * gp, Protection::Read),
        (r + page + 2 * gp, r + 2 * page + 2 * gp, Protection::ReadExecute),
        (r + 2 * page + 2 * gp, r + 3 * page, Protection::ReadWrite),
    ];
    for (a, b, prot) in segs {
        s.unmap(a, b - a).unwrap();
        assert_eq!(s.map_file_exact(&backing, (a - r) as u64, a, b - a, prot).unwrap(), (a, b - a), "{prot}");
    }
    for (a, b, prot) in segs {
        for at in (a..b).step_by(gp) {
            let reg = s.region_at(at).unwrap();
            let omni_mem::RegionKind::File { file_offset, .. } = reg.kind else { panic!("{at:#x} is {:?}", reg.kind) };
            assert!(reg.start <= at && at < reg.end(), "{at:#x}");
            assert_eq!((file_offset, reg.protection), ((reg.start - r) as u64, prot), "{at:#x}");
            assert_eq!(byte(&s, at), bytes[at - r], "{at:#x}");
        }
    }
    assert_eq!(s.stats().committed, 0, "every edge page a view, none copied");
    assert_eq!(s.split_stats().trapping, 0);
    let (c, c_end, prot) = segs[2];
    poke(&s, c, 0xaa);
    assert_eq!(byte(&s, c), 0xaa);
    assert_eq!(byte(&s, r + 2 * page), bytes[2 * page], "the r-x part beside it is the file's");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "copy-on-write: never the file");
    s.unmap(c, c_end - c).unwrap();
    assert_eq!(s.map_file_exact(&backing, (c - r) as u64, c, c_end - c, prot).unwrap().1, 0, "a page that has been writable is left to copy");
    let _ = std::fs::remove_file(path);
}

/// Another file's view on the edge page is not shared: only the rest is made a view.
#[test]
fn another_files_edge_page_is_left_to_copy() {
    if !overlay_expected() {
        return;
    }
    let (page, gp) = (host_page(), GUEST_PAGE);
    let (path1, _, one) = paged_file("one", 4 * page / gp);
    let (path2, _, two) = paged_file("two", 4 * page / gp);
    let s = space(Some(GUEST_PAGE));
    let r = reservation(&s, 4);
    let (a, b) = (r + gp, r + page + 2 * gp);
    s.unmap(a, b - a).unwrap();
    s.map_file_exact(&one, (a - r) as u64, a, b - a, Protection::Read).unwrap();
    let (c, d) = (b, r + 2 * page + 2 * gp);
    s.unmap(c, d - c).unwrap();
    assert_eq!(s.map_file_exact(&two, (c - r) as u64, c, d - c, Protection::Read).unwrap(), (r + 2 * page, d - r - 2 * page));
    let _ = (std::fs::remove_file(path1), std::fs::remove_file(path2));
}

/// Edge pages in a strict range would trap (their `PROT_NONE` parts count): they are left to copy,
/// and the whole host page between is the view.
#[test]
fn an_edge_page_whose_parts_would_trap_is_left_to_copy() {
    if !overlay_expected() {
        return;
    }
    let (page, gp) = (host_page(), GUEST_PAGE);
    let (path, _, backing) = paged_file("strict", 4 * page / gp);
    let s = space(Some(GUEST_PAGE));
    let r = reservation(&s, 4);
    s.set_strict_gaps(r, 4 * page, true);
    let (start, end) = (r + page - gp, r + 2 * page + gp);
    s.unmap(start, end - start).unwrap();
    assert_eq!(s.map_file_exact(&backing, (start - r) as u64, start, end - start, Protection::Read).unwrap(), (r + page, page));
    let _ = std::fs::remove_file(path);
}

/// The segment's own parts protect and unmap on their own: a write after `mprotect` stays private,
/// an unmap leaves the reservation's parts, and unmapping those too frees the pages.
#[test]
fn an_edge_pages_parts_protect_and_unmap_on_their_own() {
    if !overlay_expected() {
        return;
    }
    let (page, gp) = (host_page(), GUEST_PAGE);
    let (path, bytes, backing) = paged_file("parts", 4 * page / gp);
    let s = space(Some(GUEST_PAGE));
    let r = reservation(&s, 4);
    let (start, end) = (r + page - gp, r + 2 * page + gp);
    s.unmap(start, end - start).unwrap();
    s.map_file_exact(&backing, (start - r) as u64, start, end - start, Protection::Read).unwrap();
    s.protect(start, gp, Protection::ReadWrite).unwrap();
    poke(&s, start, 0x55);
    assert_eq!(byte(&s, start), 0x55);
    s.write_forced(end - gp, &[0x66]).unwrap();
    assert_eq!(byte(&s, end - gp), 0x66, "a forced write into a read-only part lands");
    assert_eq!(s.region_at(end - gp).unwrap().protection, Protection::Read);
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "never the file");
    s.unmap(start, end - start).unwrap();
    assert!(s.region_at(start).is_none() && s.region_at(r + page).is_none());
    assert_eq!(s.region_at(r).unwrap().protection, Protection::None, "the reservation's parts stay");
    assert_eq!(s.region_at(end).unwrap().protection, Protection::None);
    s.unmap(r, 4 * page).unwrap();
    assert_eq!(s.split_stats().tracked, 0);
    assert!(s.region_at(r).is_none() && s.region_at(end).is_none());
    let _ = std::fs::remove_file(path);
}

/// Without the overlay (or at whole host pages) it is `map_file` at the range: on every host.
#[test]
fn map_file_exact_of_whole_host_pages_is_map_file() {
    let page = host_page();
    let (path, bytes, backing) = paged_file("whole", 2 * page / GUEST_PAGE);
    for guest_page in [None, Some(GUEST_PAGE)] {
        let s = space(guest_page);
        let r = reservation(&s, 2);
        s.unmap(r, 2 * page).unwrap();
        assert_eq!(s.map_file_exact(&backing, 0, r, 2 * page, Protection::Read).unwrap(), (r, 2 * page));
        assert!(matches!(s.region_at(r).unwrap().kind, omni_mem::RegionKind::File { .. }));
        assert_eq!(byte(&s, r + page), bytes[page]);
    }
    let _ = std::fs::remove_file(path);
}
