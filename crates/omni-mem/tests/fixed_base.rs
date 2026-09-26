//! `GuestSpaceConfig::base`: a guest address space reserved at a chosen host address, for guests
//! that need particular addresses (ART's heap and boot image below 4 GiB).
use omni_mem::{CommitPolicy, GuestSpace, GuestSpaceConfig, Placement, Protection};

const SIZE: usize = 256 << 20;

#[test]
fn a_space_reserved_at_a_free_address_starts_there_and_maps_there() {
    // Find a free range by letting the host choose, then give it back and ask for it by address.
    let probe = GuestSpace::with_config(GuestSpaceConfig { size: SIZE, ..GuestSpaceConfig::default() }).expect("a space");
    let base = probe.base();
    probe.close().expect("released");
    let space = GuestSpace::with_config(GuestSpaceConfig { base: Some(base), size: SIZE, ..GuestSpaceConfig::default() })
        .expect("the same range, by address");
    assert_eq!(space.base(), base);
    let page = space.page_size();
    let at = space
        .map_anonymous(Placement::Fixed(base + 16 * page), page, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a page inside it");
    assert_eq!(at, base + 16 * page);
}

#[test]
fn a_base_that_is_in_use_is_refused() {
    let first = GuestSpace::with_config(GuestSpaceConfig { size: SIZE, ..GuestSpaceConfig::default() }).expect("a space");
    let clash = GuestSpace::with_config(GuestSpaceConfig { base: Some(first.base()), size: SIZE, ..GuestSpaceConfig::default() });
    assert!(clash.is_err(), "a reservation over a live one must fail, not overlap it");
}
