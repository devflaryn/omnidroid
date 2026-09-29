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
