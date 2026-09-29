//! `vm::lowest_mappable_address`: the least address this process can map anything at. What decides
//! whether a guest's low 4 GiB can be the host's own (D4) or must be a based window (D41).

use omni_platform::vm;

#[test]
fn the_answer_is_a_page_multiple_and_on_macos_it_is_the_4_gib_page_zero() {
    let lowest = vm::lowest_mappable_address();
    assert!(lowest > 0, "address 0 is never mappable");
    assert_eq!(lowest % vm::page_size(), 0, "{lowest:#x} is not a page multiple");
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        // An arm64 Mach-O must keep a hard page zero of at least 4 GiB (a smaller one is killed at
        // exec); a test binary is linked with the default, 4 GiB -- and it slides with the
        // executable (ASLR), so the floor is 4 GiB plus the slide: the executable's first byte.
        assert!(lowest >= 1 << 32, "{lowest:#x}: the arm64 macOS page zero is at least 4 GiB");
    } else {
        assert!(lowest < 1 << 32, "{lowest:#x}: this host maps below 4 GiB");
    }
}

#[test]
fn nothing_can_be_reserved_below_it() {
    let lowest = vm::lowest_mappable_address();
    let granule = vm::allocation_granularity();
    // The page just under the answer: refused everywhere, whatever else the process holds.
    if let Some(below) = lowest.checked_sub(granule).filter(|&b| b > 0) {
        let attempt = vm::reserve_placeholder_at(below, granule);
        assert!(attempt.is_err(), "reserved {below:#x}, below the lowest mappable {lowest:#x}");
    }
}

#[test]
fn what_lies_below_it_is_reported_occupied() {
    // What a space reserved around the host asks (`GuestSpaceConfig::around_host`): a range that
    // starts below the floor must not be taken for free there -- macOS's region walk never names
    // the page zero, least of all the part the slide moved above 4 GiB.
    let lowest = vm::lowest_mappable_address();
    let page = vm::page_size();
    let from = lowest - page;
    let occupied = vm::occupied_ranges(from, 2 * page).expect("the walk");
    assert!(
        occupied.first().is_some_and(|&(start, end)| start == from && end >= lowest),
        "[{from:#x}, {lowest:#x}) is below the floor, reported {occupied:x?}"
    );
}

#[test]
fn macos_refuses_the_low_range_the_other_hosts_give_the_guest() {
    // `omni_linux::process::GUEST_SPACE_LOW_BASE`: what Windows and Linux reserve for ART.
    const LOW_BASE: usize = 0x1000_0000;
    let lowest = vm::lowest_mappable_address();
    assert_eq!(
        lowest > LOW_BASE,
        cfg!(target_os = "macos"),
        "lowest mappable {lowest:#x} against the guest's low base {LOW_BASE:#x}"
    );
    if lowest > LOW_BASE {
        assert!(vm::reserve_placeholder_at(LOW_BASE, 1 << 20).is_err(), "the host gave out {LOW_BASE:#x}");
    }
}
