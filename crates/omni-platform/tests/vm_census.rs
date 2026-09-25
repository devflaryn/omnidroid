//! The memory census (`vm::process_regions`, `vm::resident_set`, `vm::heap_totals`) against
//! memory whose shape the test made itself: a committed block half touched, this thread's own
//! stack, this executable's image, and a large heap allocation. Windows and Linux; macOS refuses
//! the census by name, and that refusal is what is asserted there.

use omni_platform::vm::{self, HostRegionKind, Protection};

const MIB: usize = 1 << 20;

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn a_committed_block_is_one_private_allocation_and_only_its_touched_pages_are_resident() {
    let size = 8 * MIB;
    let reservation = vm::reserve(size, vm::allocation_granularity()).expect("reserve");
    let base = reservation.as_ptr();
    // SAFETY: the reservation is exactly `[base, base + size)` and nothing else uses it.
    unsafe { vm::commit(base, size, Protection::ReadWrite) }.expect("commit");
    let touched = 2 * MIB;
    for offset in (0..touched).step_by(vm::page_size()) {
        // SAFETY: committed read-write above, and `offset < size`.
        unsafe { base.add(offset).write_volatile(1) };
    }
    let start = base as usize;

    let regions = vm::process_regions().expect("the census");
    let ours: Vec<_> = regions.iter().filter(|r| r.start < start + size && r.end() > start).collect();
    assert!(!ours.is_empty(), "no region covers the block at {start:#x}");
    for region in &ours {
        assert_eq!(region.kind, HostRegionKind::Private, "{region:?}");
        assert!(region.writable && !region.executable, "{region:?}");
    }
    let committed: u64 = ours.iter().map(|r| r.committed).sum();
    if cfg!(windows) {
        // One allocation is its own regions, and nothing else is in them.
        assert_eq!(committed, size as u64, "the whole block is committed: {ours:?}");
        assert!(ours.iter().all(|r| !r.stack), "{ours:?}");
    } else {
        // Linux merges adjacent anonymous VMAs with the same flags -- MEASURED: the block came
        // back inside one VMA with a neighbouring thread's stack -- so its VMA can be larger.
        assert!(committed >= size as u64, "the whole block is committed: {ours:?}");
        assert!(ours.iter().all(|r| r.start <= start + size && r.end() >= start), "{ours:?}");
    }

    let resident = vm::resident_set().expect("a resident set").in_range(start, size).expect("in range");
    assert_eq!(resident.resident, touched as u64, "exactly the touched pages are resident");
    assert_eq!(resident.private, resident.resident, "anonymous pages are private");
    assert_eq!(resident.shareable(), 0);
    // The region's own figure agrees with the range query -- exactly where the regions are the
    // block's alone, and as a floor where Linux merged a neighbour into them.
    let by_region: u64 =
        ours.iter().map(|r| r.residency.expect("a residency").resident).sum();
    if cfg!(windows) {
        assert_eq!(by_region, touched as u64, "{ours:?}");
    } else {
        assert!(by_region >= touched as u64, "{ours:?}");
    }

    vm::release(reservation).expect("release");
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn this_threads_stack_is_marked_a_stack_and_the_executable_an_image() {
    let local = 0u64;
    let here = std::ptr::addr_of!(local) as usize;
    let regions = vm::process_regions().expect("the census");
    let stack = regions.iter().find(|r| r.start <= here && here < r.end()).expect("the stack's region");
    assert!(stack.stack, "the region holding a local is not marked a stack: {stack:?}");
    assert_eq!(stack.kind, HostRegionKind::Private);

    let code = this_threads_stack_is_marked_a_stack_and_the_executable_an_image as *const () as usize;
    let image = regions.iter().find(|r| r.start <= code && code < r.end()).expect("the code's region");
    assert_eq!(image.kind, HostRegionKind::Image, "{image:?}");
    assert!(image.executable, "{image:?}");
    let exe = std::env::current_exe().expect("the executable");
    let exe_name = exe.file_name().expect("a file name").to_string_lossy().to_string();
    assert_eq!(image.name.as_deref(), Some(exe_name.as_str()), "{image:?}");

    // Nothing is counted twice: regions are disjoint and in address order.
    for pair in regions.windows(2) {
        assert!(pair[0].end() <= pair[1].start, "{:?} overlaps {:?}", pair[0], pair[1]);
    }
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn a_large_heap_allocation_shows_in_the_heap_totals() {
    let before = vm::heap_totals().expect("heap totals");
    assert!(before.heaps >= 1 && before.committed >= before.allocated / 2, "{before:?}");
    let block = vec![7u8; 64 * MIB];
    let during = vm::heap_totals().expect("heap totals");
    assert!(
        during.allocated >= before.allocated + 60 * MIB as u64,
        "a 64 MiB Vec did not show: {before:?} -> {during:?}"
    );
    drop(std::hint::black_box(block));
}

#[cfg(not(any(windows, target_os = "linux")))]
#[test]
fn the_census_is_refused_by_name_where_it_is_not_built() {
    assert!(vm::process_regions().unwrap_err().is_unsupported());
    assert!(vm::resident_set().unwrap_err().is_unsupported());
    assert!(vm::heap_totals().unwrap_err().is_unsupported());
}
