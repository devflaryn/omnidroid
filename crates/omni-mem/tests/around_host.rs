//! `GuestSpaceConfig::around_host`: a guest address space reserved over a range the host already
//! holds parts of, stepping around them.
//!
//! Why it exists: ART needs its boot image near 0x7000_0000 and its heap below 4 GiB, and on Windows
//! every process has `KUSER_SHARED_DATA` at 0x7FFE_0000, so a low reservation that covers it fails
//! outright (`ERROR_INVALID_ADDRESS`). With `around_host` the space covers the whole range, and each
//! part the host holds is an entry of the region map that is never free, never mapped over, never
//! unmapped and never released.
//!
//! The portable tests make their own hole: they find a free range (reserve it by the host's choice,
//! give it back), put a host allocation in the middle, and reserve the space over the lot.

use std::sync::Mutex;

use omni_mem::{
    CommitPolicy, GuestSpace, GuestSpaceConfig, MemError, Placement, Protection, RegionKind,
};
use omni_platform::vm;

const MIB: usize = 1 << 20;
/// The probe range. A multiple of every host's allocation granularity and page.
const SIZE: usize = 32 * MIB;
/// Where the hole goes, from the range's base.
const HOLE_AT: usize = 16 * MIB;

/// The tests here find a free range and then use it by address, so two of them racing each other
/// for address space would make each other's ranges occupied. One at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The size of the hole: one allocation granule, or a page where a page is larger (Apple silicon's
/// 16 KiB page is the granularity there).
fn granule() -> usize {
    vm::allocation_granularity().max(vm::page_size())
}

/// A range of `len` bytes nothing holds right now: the middle third of a probe reserved by the
/// host's choice and given back. The middle, because whatever the process allocates next (a
/// `malloc` large enough for its own `mmap`, say) lands at one end of a gap that has just opened:
/// MEASURED on Linux (top-down) and macOS (bottom-up), where a probe used whole intermittently
/// had a stranger inside it by the time the space was reserved.
fn free_range(len: usize) -> usize {
    let probe = GuestSpace::with_config(GuestSpaceConfig { size: 3 * len, ..GuestSpaceConfig::default() })
        .expect("a probe space");
    let base = probe.base() + len;
    probe.close().expect("the probe released");
    base
}

/// Memory the host holds at an address of the test's choosing: reserved, committed, and with a
/// known byte at its start, so that it can be shown to survive the space's teardown.
struct HostAllocation {
    at: usize,
    len: usize,
    reservation: Option<vm::Reservation>,
}

const MARK: u8 = 0xA5;

impl HostAllocation {
    fn new(at: usize, len: usize) -> Self {
        let reservation = vm::reserve_placeholder_at(at, len).expect("a host allocation at the address");
        // SAFETY: `[at, at + len)` is exactly the placeholder just reserved; once committed it is
        // this test's memory and nothing else refers to it.
        unsafe {
            vm::commit_placeholder(at as *mut u8, len, Protection::ReadWrite).expect("committed");
            (at as *mut u8).write_volatile(MARK);
        }
        Self { at, len, reservation: Some(reservation) }
    }

    fn mark(&self) -> u8 {
        // SAFETY: committed read-write memory this test owns, for as long as `self` lives.
        unsafe { (self.at as *const u8).read_volatile() }
    }
}

impl Drop for HostAllocation {
    fn drop(&mut self) {
        // SAFETY: the private memory committed in `new`, which nothing refers to any more.
        let _ = unsafe { vm::decommit_to_placeholder(self.at as *mut u8, self.len) };
        if let Some(reservation) = self.reservation.take() {
            let _ = vm::release(reservation);
        }
    }
}

fn around(base: usize, size: usize) -> GuestSpaceConfig {
    GuestSpaceConfig { base: Some(base), size, around_host: true, ..GuestSpaceConfig::default() }
}

fn overlaps(at: usize, len: usize, other: usize, other_len: usize) -> bool {
    at < other + other_len && other < at + len
}

fn write_read(at: usize, value: u8) -> u8 {
    // SAFETY: the caller has mapped and committed `at` read-write in a live space.
    unsafe {
        (at as *mut u8).write_volatile(value);
        (at as *const u8).read_volatile()
    }
}

#[test]
fn a_space_steps_around_a_host_allocation_and_leaves_it_alone() {
    let _serial = serial();
    let g = granule();
    let base = free_range(SIZE);
    let hole = base + HOLE_AT;
    let host = HostAllocation::new(hole, g);

    let space = GuestSpace::with_config(around(base, SIZE)).expect("a space around the host allocation");
    assert_eq!(space.base(), base);
    assert_eq!(space.end(), base + SIZE, "the space spans the whole range, hole included");
    let page = space.page_size();

    // The hole is reported, and reported as the host's.
    for probe in [hole, hole + g - 1] {
        let region = space.region_at(probe).expect("the hole is in use");
        assert_eq!(region.kind, RegionKind::Host, "{probe:#x}");
        assert_eq!((region.start, region.len), (hole, g));
        assert_eq!(region.mapping, None, "no guest mapping owns it");
        assert_eq!(region.protection, Protection::None);
    }
    let hosts: Vec<_> =
        space.mapped_regions().into_iter().filter(|r| r.kind == RegionKind::Host).collect();
    assert_eq!(hosts.len(), 1, "{hosts:?}");
    let stats = space.stats();
    assert_eq!(stats.reserved, SIZE);
    assert_eq!(stats.mapped, 0, "the host's bytes are not the guest's mappings");
    assert_eq!(stats.free, SIZE - g);
    assert_eq!(stats.largest_free, HOLE_AT, "no free range spans the hole");

    // Nothing may be placed over it.
    let taken = space.map_anonymous(Placement::Fixed(hole), page, Protection::ReadWrite, CommitPolicy::Lazy);
    assert!(matches!(taken, Err(MemError::AddressTaken { .. })), "{taken:?}");
    let straddle =
        space.map_anonymous(Placement::Fixed(hole - page), 2 * page, Protection::ReadWrite, CommitPolicy::Lazy);
    assert!(matches!(straddle, Err(MemError::AddressTaken { .. })), "{straddle:?}");
    let hinted = space
        .map_anonymous(Placement::Hint { address: hole, align: page }, page, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("a hint is taken elsewhere");
    assert!(!overlaps(hinted, page, hole, g), "a hint into the hole landed at {hinted:#x}");
    space.unmap(hinted, page).expect("unmapped");

    // Right up to both edges works, and the memory there is real.
    let below = space
        .map_anonymous(Placement::Fixed(hole - page), page, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("the page just below the hole");
    let above = space
        .map_anonymous(Placement::Fixed(hole + g), page, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("the page just above the hole");
    space.ensure_committed(above, page).expect("committed");
    assert_eq!(write_read(below + page - 1, 0x11), 0x11);
    assert_eq!(write_read(above, 0x22), 0x22);

    // An unmap, protect or discard that reaches into the hole is refused, and changes nothing: the
    // mappings on either side keep their contents.
    let across = space.unmap(hole - page, g + 2 * page);
    assert!(matches!(across, Err(MemError::HostOwned { .. })), "{across:?}");
    let protect = space.protect(hole - page, g + 2 * page, Protection::Read);
    assert!(matches!(protect, Err(MemError::HostOwned { .. })), "{protect:?}");
    let discard = space.discard(hole, page);
    assert!(matches!(discard, Err(MemError::HostOwned { .. })), "{discard:?}");
    assert_eq!(space.region_at(hole).map(|r| r.kind), Some(RegionKind::Host));
    assert_eq!(space.region_at(below).map(|r| r.protection), Some(Protection::ReadWrite));
    // SAFETY: both pages are still mapped and committed read-write.
    unsafe {
        assert_eq!(((below + page - 1) as *const u8).read_volatile(), 0x11);
        assert_eq!((above as *const u8).read_volatile(), 0x22);
    }
    assert_eq!(host.mark(), MARK, "the host's memory was not touched");

    // Unmapping the two neighbours on their own is fine, and reclaiming coalesces nothing across
    // the hole.
    space.unmap(below, page).expect("the page below");
    space.unmap(above, page).expect("the page above");
    space.reclaim_idle().expect("reclaimed");
    assert_eq!(space.stats().largest_free, HOLE_AT);
    assert_eq!(space.region_at(hole).map(|r| r.kind), Some(RegionKind::Host));

    // Teardown releases the space's own pieces and leaves the host's allocation where it was.
    space.close().expect("closed");
    assert_eq!(host.mark(), MARK, "the host's memory survives the space");
    assert_eq!(vm::occupied_ranges(hole, g).expect("the walk"), vec![(hole, hole + g)]);

    // And the space's pieces really were given back: the same range can be reserved around the
    // host again.
    let again = GuestSpace::with_config(around(base, SIZE)).expect("the range is free again");
    again.close().expect("closed again");
}

#[test]
fn anywhere_placement_never_crosses_the_hole() {
    let _serial = serial();
    let g = granule();
    let base = free_range(SIZE);
    let hole = base + HOLE_AT;
    let _host = HostAllocation::new(hole, g);
    let space = GuestSpace::with_config(around(base, SIZE)).expect("a space around the host allocation");
    let page = space.page_size();

    // 12 MiB at the base leaves 4 MiB below the hole, so 8 MiB only fits above it. Placed by
    // ignoring the hole it would run straight through it.
    let low = space
        .map_anonymous(Placement::Fixed(base), 12 * MIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("12 MiB at the base");
    let high = space
        .map_anonymous(Placement::Anywhere { align: page }, 8 * MIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("8 MiB somewhere");
    assert!(!overlaps(high, 8 * MIB, hole, g), "8 MiB at {high:#x} crosses the hole at {hole:#x}");
    assert!(high >= hole + g, "it can only fit above the hole");

    // Bigger than either side of the hole, smaller than both together: no room.
    let too_big =
        space.map_anonymous(Placement::Anywhere { align: page }, 20 * MIB, Protection::ReadWrite, CommitPolicy::Lazy);
    assert!(matches!(too_big, Err(MemError::NoSpace { .. })), "{too_big:?}");

    // Everything back, coalesced, and then each side filled exactly.
    space.unmap(low, 12 * MIB).expect("unmapped");
    space.unmap(high, 8 * MIB).expect("unmapped");
    space.reclaim_idle().expect("reclaimed");
    let below = space
        .map_anonymous(Placement::Anywhere { align: page }, HOLE_AT, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("all of the side below");
    assert_eq!(below, base);
    let above = space
        .map_anonymous(Placement::Anywhere { align: page }, SIZE - HOLE_AT - g, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("all of the side above");
    assert_eq!(above, hole + g);
    // Commit the pages that touch the hole, through the lazy path.
    space.ensure_committed(hole - 1, 1).expect("the last page below");
    space.ensure_committed(hole + g, 1).expect("the first page above");
    assert_eq!(write_read(hole - 1, 0x33), 0x33);
    assert_eq!(write_read(hole + g, 0x44), 0x44);
    space.close().expect("closed");
}

#[test]
fn host_memory_at_either_end_of_the_range_is_stepped_around_too() {
    let _serial = serial();
    let g = granule();
    let base = free_range(SIZE);
    let first = HostAllocation::new(base, g);
    let last = HostAllocation::new(base + SIZE - g, g);

    let space = GuestSpace::with_config(around(base, SIZE)).expect("a space with host memory at both ends");
    assert_eq!((space.base(), space.end()), (base, base + SIZE));
    assert_eq!(space.region_at(base).map(|r| r.kind), Some(RegionKind::Host));
    assert_eq!(space.region_at(base + SIZE - 1).map(|r| r.kind), Some(RegionKind::Host));
    assert_eq!(space.stats().free, SIZE - 2 * g);
    let page = space.page_size();
    let at = space
        .map_anonymous(Placement::Fixed(base + g), SIZE - 2 * g, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("everything between the two");
    space.ensure_committed(at, 1).expect("first page");
    space.ensure_committed(at + SIZE - 2 * g - page, page).expect("last page");
    assert_eq!(write_read(at, 1), 1);
    assert_eq!(write_read(at + SIZE - 2 * g - 1, 2), 2);
    space.close().expect("closed");
    assert_eq!((first.mark(), last.mark()), (MARK, MARK));
}

#[test]
fn without_around_host_an_occupied_range_is_still_refused() {
    let _serial = serial();
    let g = granule();
    let base = free_range(SIZE);
    let _host = HostAllocation::new(base + HOLE_AT, g);
    let refused = GuestSpace::with_config(GuestSpaceConfig {
        base: Some(base),
        size: SIZE,
        ..GuestSpaceConfig::default()
    });
    assert!(refused.is_err(), "a reservation over host memory must fail without around_host");
}

#[test]
fn around_host_on_a_range_with_nothing_in_it_is_an_ordinary_space() {
    let _serial = serial();
    let base = free_range(SIZE);
    let space = GuestSpace::with_config(around(base, SIZE)).expect("a space");
    assert!(space.mapped_regions().is_empty());
    assert_eq!(space.stats().largest_free, SIZE);
    let at = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, SIZE, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("the whole space in one mapping");
    assert_eq!(at, base);
    space.close().expect("closed");
}

/// The case this exists for: 64 GiB from 256 MiB covers `KUSER_SHARED_DATA` at 0x7FFE_0000, which
/// every Windows process has and which makes a plain reservation of the range fail.
#[cfg(windows)]
#[test]
fn a_low_space_on_windows_steps_around_kuser_shared_data() {
    let _serial = serial();
    const BASE: usize = 0x1000_0000;
    const KUSER_SHARED_DATA: usize = 0x7FFE_0000;
    let size = 64 << 30;
    let plain = GuestSpace::with_config(GuestSpaceConfig {
        base: Some(BASE),
        size,
        ..GuestSpaceConfig::default()
    });
    assert!(plain.is_err(), "without around_host, KUSER_SHARED_DATA is in the way");
    drop(plain);

    let space = GuestSpace::with_config(around(BASE, size)).expect("a low space around the host");
    assert_eq!((space.base(), space.end()), (BASE, BASE + size));
    let region = space.region_at(KUSER_SHARED_DATA).expect("KUSER_SHARED_DATA is in use");
    assert_eq!(region.kind, RegionKind::Host);
    assert!(region.start <= KUSER_SHARED_DATA && region.end() >= KUSER_SHARED_DATA + 0x1_0000,
        "the whole 64 KiB granule is the host's: {region:?}");
    let page = space.page_size();
    let taken = space.map_anonymous(Placement::Fixed(KUSER_SHARED_DATA), page, Protection::ReadWrite, CommitPolicy::Lazy);
    assert!(matches!(taken, Err(MemError::AddressTaken { .. })), "{taken:?}");
    // Something below 4 GiB and something above it, as ART's heap and a later allocation would.
    let low = space
        .map_anonymous(Placement::Hint { address: 0x7000_0000, align: page }, 16 * MIB, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("a mapping near the boot image's address");
    assert!(low + 16 * MIB <= 1 << 32);
    space.ensure_committed(low, 1).expect("committed");
    assert_eq!(write_read(low, 7), 7);
    space.close().expect("closed");
}
