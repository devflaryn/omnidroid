//! `GuestSpaceConfig::low_window` (D41): the part of a guest space below 4 GiB is backed somewhere
//! else in the host and addressed as `W + g`; above 4 GiB a guest address stays a host address.
//!
//! macOS needs it -- nothing can be mapped below its 4 GiB `__PAGEZERO`, and ART's heap must be
//! there -- but the window is nothing macOS-specific, so these run on every host.

use std::sync::{Arc, Mutex};

use omni_mem::{CommitPolicy, DemandPager, GuestSpace, GuestSpaceConfig, MemError, Placement, Protection, LOW_WINDOW_END};

const MIB: usize = 1 << 20;
const GIB: usize = 1 << 30;
/// `omni_linux::process::GUEST_SPACE_LOW_BASE`.
const BASE: usize = 0x1000_0000;
/// Where ART puts its boot image (`ART_BASE_ADDRESS`).
const ART_BASE: usize = 0x7000_0000;

/// One space at `BASE` at a time: each reserves the same high range.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn windowed(size: usize) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig {
        base: Some(BASE),
        size,
        around_host: true,
        low_window: true,
        ..GuestSpaceConfig::default()
    })
    .expect("a space with a low window")
}

#[test]
fn guest_addresses_below_4_gib_are_based_and_above_are_the_hosts() {
    let _serial = serial();
    let space = windowed(4 * GIB);
    assert_eq!(space.base(), BASE, "the guest's layout is the other hosts'");
    let window = space.low_window().expect("the window");
    assert_eq!(window.end, LOW_WINDOW_END);
    assert_ne!(window.delta, 0);
    assert_eq!(window.delta % vm_page(), 0, "the window's base is a page multiple");

    assert_eq!(space.host_addr(ART_BASE), ART_BASE + window.delta);
    assert_eq!(space.host_to_guest(ART_BASE + window.delta), Some(ART_BASE));
    let high = LOW_WINDOW_END + GIB / 8;
    assert_eq!(space.host_addr(high), high, "identity above 4 GiB");
    assert_eq!(space.host_to_guest(high), Some(high));
    // An address the space does not cover, either way round.
    assert_eq!(space.host_to_guest(space.end() + window.delta + GIB), None);
    if window.delta > GIB {
        assert_eq!(space.host_to_guest(ART_BASE), None, "the unbased low address is not the space's");
    }
    space.close().expect("closed");
}

#[test]
fn a_mapping_in_the_window_is_memory_at_the_based_address() {
    let _serial = serial();
    let space = windowed(4 * GIB);
    let at = space
        .map_anonymous(Placement::Fixed(ART_BASE), MIB, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("ART's boot image address");
    assert_eq!(at, ART_BASE);
    let ptr = space.ptr(ART_BASE + 16, 8).expect("a pointer");
    assert_eq!(ptr as usize, space.host_addr(ART_BASE + 16), "`ptr` is the based address");
    // SAFETY: eight bytes of a committed read-write mapping this test owns.
    unsafe { ptr.cast::<u64>().write_unaligned(0x0123_4567_89ab_cdef) };
    space.write_forced(ART_BASE + 32, b"window").expect("a forced write");
    // SAFETY: as above, 32 bytes on.
    let back = unsafe { std::slice::from_raw_parts(space.host_addr(ART_BASE + 32) as *const u8, 6) };
    assert_eq!(back, b"window");
    assert_eq!(space.region_at(ART_BASE).expect("the region").start, ART_BASE, "the map is the guest's");

    space.unmap(ART_BASE, MIB).expect("unmapped");
    let again = space
        .map_anonymous(Placement::Fixed(ART_BASE), MIB, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("mapped again");
    // SAFETY: committed read-write memory this test owns.
    let zero = unsafe { (space.host_addr(again + 16) as *const u64).read_unaligned() };
    assert_eq!(zero, 0, "a fresh mapping reads zero");
    space.close().expect("closed");
}

#[test]
fn a_lazy_page_in_the_window_is_committed_by_the_pager() {
    let _serial = serial();
    let space = Arc::new(windowed(4 * GIB));
    let pager = DemandPager::install(Arc::clone(&space)).expect("the demand pager");
    DemandPager::prepare_thread().expect("room to take a fault");
    let at = space
        .map_anonymous(Placement::Fixed(ART_BASE), 4 * MIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("a lazy mapping");
    let host = space.host_addr(at + 2 * MIB + 8) as *mut u64;
    // SAFETY: the pager commits the granule on the first touch; the mapping is read-write.
    unsafe { host.write_volatile(42) };
    // SAFETY: as above, and committed now.
    assert_eq!(unsafe { host.read_volatile() }, 42);
    assert!(pager.stats().resolved >= 1, "the pager resolved the fault: {:?}", pager.stats());
    drop(pager);
    Arc::try_unwrap(space).ok().expect("the only reference").close().expect("closed");
}

#[test]
fn a_mapping_without_an_address_stays_out_of_the_window() {
    let _serial = serial();
    let space = windowed(4 * GIB);
    for _ in 0..4 {
        let at = space
            .map_anonymous(Placement::Anywhere { align: vm_page() }, MIB, Protection::ReadWrite, CommitPolicy::Lazy)
            .expect("mapped");
        assert!(at >= LOW_WINDOW_END, "{at:#x} was placed in the window");
        assert_eq!(space.host_addr(at), at);
    }
    space.close().expect("closed");
}

#[test]
fn a_mapping_across_the_seam_is_refused() {
    let _serial = serial();
    let space = windowed(4 * GIB);
    let page = vm_page();
    let across = space.map_anonymous(
        Placement::Fixed(LOW_WINDOW_END - page),
        2 * page,
        Protection::ReadWrite,
        CommitPolicy::Lazy,
    );
    assert!(
        matches!(across, Err(MemError::AddressTaken { .. } | MemError::HostOwned { .. })),
        "a mapping across 4 GiB: {across:?}"
    );
    // Right up to the seam from below is fine.
    let below = space
        .map_anonymous(Placement::Fixed(LOW_WINDOW_END - 2 * page), page, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("the window's last usable page");
    assert_eq!(space.host_addr(below), below + space.low_window().unwrap().delta);
    space.close().expect("closed");
}

#[test]
fn a_space_without_the_window_is_the_identity() {
    let space = GuestSpace::with_config(GuestSpaceConfig { size: 64 * MIB, ..GuestSpaceConfig::default() })
        .expect("a plain space");
    assert!(space.low_window().is_none());
    let at = space.base() + MIB;
    assert_eq!(space.host_addr(at), at);
    assert_eq!(space.host_to_guest(at), Some(at));
    space.close().expect("closed");
}

fn vm_page() -> usize {
    omni_platform::vm::page_size()
}
