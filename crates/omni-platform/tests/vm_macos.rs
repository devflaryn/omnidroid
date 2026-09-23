//! macOS behaviour of the virtual-memory seam: the same contract `vm_windows.rs` asserts on
//! Windows, at this host's 16 KiB page, against a backend built on `mmap(MAP_FIXED)` and a record
//! of its own (see `src/vm/macos.rs`).
//!
//! These assert **behaviour**: the bytes at a mapped address, the page-granular refusals, and that
//! a refused call left the mapping untouched. What memory the operations cost is in
//! `vm_footprint_macos.rs`, a separate binary, because `phys_footprint` is per process and this
//! binary's tests run in parallel.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use omni_platform::vm::{self, MapExecutability, Protection, ReservationKind, VmError};

/// The host page, measured once here so a wrong assumption in the backend cannot hide behind it.
fn page() -> usize {
    vm::page_size()
}

/// The exact bytes of page `index` of a fixture file: a tag naming the index, then a fill byte
/// derived from it, so an off-by-one-page mapping is caught, not just a wholly wrong address.
fn expected_page(index: usize) -> Vec<u8> {
    let tag = format!("OMNIDROID FIXTURE PAGE {index:06} ");
    let mut bytes = vec![(index as u8).wrapping_mul(31).wrapping_add(7); page()];
    bytes[..tag.len()].copy_from_slice(tag.as_bytes());
    bytes
}

fn fixture(pages: usize) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    fs::create_dir_all(&dir).expect("create fixture directory");
    let path = dir.join(format!(
        "fixture-{}-{}.bin",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut data = Vec::with_capacity(pages * page());
    for index in 0..pages {
        data.extend_from_slice(&expected_page(index));
    }
    fs::write(&path, &data).expect("write fixture file");
    path
}

/// # Safety
///
/// `[ptr, ptr + len)` must be readable mapped memory.
unsafe fn read_mapped(ptr: *const u8, len: usize) -> Vec<u8> {
    std::slice::from_raw_parts(ptr, len).to_vec()
}

#[test]
fn the_page_is_16_kib_and_is_also_the_allocation_granularity() {
    // Apple silicon's page. Everything page-granular on the seam is checked against this, and the
    // guest is told it through AT_PAGESZ, so it is asserted rather than assumed.
    assert_eq!(vm::page_size(), 16384);
    assert_eq!(vm::allocation_granularity(), 16384);
    assert!(vm::placeholder_api_available());
    assert!(vm::placeholder_api_symbols().is_empty(), "nothing is resolved at run time here");
}

#[test]
fn a_sixteen_gibibyte_reservation_succeeds_and_honours_a_large_alignment() {
    let reservation = vm::reserve(16 << 30, page()).expect("16 GiB of address space");
    assert_eq!(reservation.base() % page(), 0);
    vm::release(reservation).expect("release");

    let align = 1 << 21;
    let aligned = vm::reserve(3 * page(), align).expect("reserve at 2 MiB alignment");
    assert_eq!(aligned.base() % align, 0, "{:#x} is not 2 MiB aligned", aligned.base());
    vm::release(aligned).expect("release");
}

#[test]
fn a_reserved_page_faults_until_committed_and_decommit_gives_back_zero() {
    let reservation = vm::reserve(8 * page(), page()).expect("reserve");
    let at = reservation.base() + 2 * page();
    // SAFETY: inside a live reservation this test owns; nothing else refers to it.
    unsafe {
        vm::commit(at as *mut u8, page(), Protection::ReadWrite).expect("commit one page");
        *(at as *mut u64) = 0xDEAD_BEEF;
        vm::protect(at as *mut u8, page(), Protection::Read).expect("read only");
        assert_eq!(*(at as *const u64), 0xDEAD_BEEF, "protect must not discard contents");
        vm::decommit(at as *mut u8, page()).expect("decommit");
        vm::commit(at as *mut u8, page(), Protection::ReadWrite).expect("commit again");
        assert_eq!(*(at as *const u64), 0, "a re-committed page reads zero (D10)");
    }
    vm::release(reservation).expect("release");
}

/// Reserved and decommitted pages must really be inaccessible: a read of one kills the process.
/// Run in a child, because the read is not survivable -- that is the property.
#[test]
fn a_reserved_or_decommitted_page_is_not_readable() {
    const CHILD: &str = "OMNI_VM_MACOS_FAULT_CHILD";
    if let Ok(which) = std::env::var(CHILD) {
        let reservation = vm::reserve(2 * page(), page()).expect("reserve");
        let at = reservation.base();
        if which == "decommitted" {
            // SAFETY: inside the reservation.
            unsafe {
                vm::commit(at as *mut u8, page(), Protection::ReadWrite).expect("commit");
                *(at as *mut u8) = 1;
                vm::decommit(at as *mut u8, page()).expect("decommit");
            }
        }
        // SAFETY: none -- the read is expected to kill this process.
        let value = unsafe { std::ptr::read_volatile(at as *const u8) };
        println!("read {value} from a {which} page");
        std::process::exit(7);
    }
    for which in ["reserved", "decommitted"] {
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["a_reserved_or_decommitted_page_is_not_readable", "--exact", "--nocapture"])
            .env(CHILD, which)
            .status()
            .expect("run the child");
        use std::os::unix::process::ExitStatusExt;
        assert_ne!(status.code(), Some(7), "a {which} page was readable");
        assert!(
            matches!(status.signal(), Some(10 | 11)),
            "a read of a {which} page should die of SIGBUS or SIGSEGV, got {status:?}"
        );
    }
}

#[test]
fn commit_refuses_a_misaligned_address_or_size_before_any_os_call() {
    let reservation = vm::reserve(4 * page(), page()).expect("reserve");
    // SAFETY: refused before anything is touched.
    unsafe {
        for (address, size) in [(reservation.base() + 4096, page()), (reservation.base(), 4096)] {
            match vm::commit(address as *mut u8, size, Protection::ReadWrite) {
                Err(VmError::Misaligned { required, .. }) => assert_eq!(required, 16384),
                other => panic!("expected Misaligned for {address:#x}+{size}, got {other:?}"),
            }
        }
    }
    vm::release(reservation).expect("release");
}

#[test]
fn a_placeholder_splits_and_a_file_view_lands_at_a_page_offset_with_the_right_bytes() {
    let path = fixture(8);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let parent = vm::reserve_placeholder(6 * page(), page()).expect("reserve");
    let left = vm::split_placeholder(&parent, 0, 2 * page()).expect("left");
    let middle = vm::split_placeholder(&parent, 2 * page(), 3 * page()).expect("middle");
    let right = vm::split_placeholder(&parent, 5 * page(), page()).expect("right");
    // SAFETY: `middle` is an exact-size placeholder piece this test owns.
    unsafe {
        vm::map_file(&file, 3 * page() as u64, 3 * page(), middle.as_ptr(), Protection::Read)
            .expect("map three pages from file page 3");
        for index in 0..3 {
            assert_eq!(
                read_mapped(middle.as_ptr().add(index * page()), page()),
                expected_page(3 + index),
                "view page {index}"
            );
        }
        vm::unmap(middle.as_ptr(), 3 * page()).expect("unmap back to a placeholder");
        // Back to a placeholder: it can be replaced again, by private memory this time.
        vm::commit_placeholder(middle.as_ptr(), 3 * page(), Protection::ReadWrite)
            .expect("replace the placeholder again");
        assert_eq!(*middle.as_ptr(), 0, "private memory replacing a placeholder reads zero");
        vm::decommit_to_placeholder(middle.as_ptr(), 3 * page()).expect("back to placeholder");
        vm::coalesce_placeholders(parent.as_ptr(), 6 * page()).expect("coalesce all three");
    }
    // The pieces are gone; the parent is one placeholder again, and releases whole.
    let _ = (left, right);
    vm::release(parent).expect("release the coalesced parent");
    let _ = fs::remove_file(&path);
}

#[test]
fn replacing_a_placeholder_requires_an_exact_size_placeholder() {
    let path = fixture(4);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(4 * page(), page()).expect("reserve");
    // SAFETY: refused calls; nothing is mapped.
    unsafe {
        assert!(matches!(
            vm::map_file(&file, 0, page(), reservation.as_ptr(), Protection::Read),
            Err(VmError::PlaceholderNotExactSize { .. })
        ));
        assert!(matches!(
            vm::commit_placeholder(reservation.as_ptr(), page(), Protection::ReadWrite),
            Err(VmError::PlaceholderNotExactSize { .. })
        ));
    }
    vm::release(reservation).expect("the refused calls left the placeholder whole");
    let _ = fs::remove_file(&path);
}

#[test]
fn a_view_past_the_end_of_the_file_and_a_view_with_no_access_are_refused() {
    let path = fixture(2);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(3 * page(), page()).expect("reserve");
    // SAFETY: refused before any OS call.
    unsafe {
        assert!(matches!(
            vm::map_file(&file, 0, 3 * page(), reservation.as_ptr(), Protection::Read),
            Err(VmError::ViewPastEndOfFile { .. })
        ));
        assert!(matches!(
            vm::map_file(&file, 0, page(), reservation.as_ptr(), Protection::None),
            Err(VmError::UnsupportedViewProtection { .. })
        ));
    }
    vm::release(reservation).expect("release");
    let _ = fs::remove_file(&path);
}

#[test]
fn an_executable_view_needs_a_file_opened_executable_and_really_is_r_x() {
    let path = fixture(2);
    let plain = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let exec = vm::open_file_for_mapping(&path, MapExecutability::Executable).expect("open exec");
    let reservation = vm::reserve_placeholder(2 * page(), page()).expect("reserve");
    // SAFETY: an exact-size placeholder this test owns; unmapped at the end.
    unsafe {
        assert!(matches!(
            vm::map_file(&plain, 0, 2 * page(), reservation.as_ptr(), Protection::ReadExecute),
            Err(VmError::FileNotOpenedExecutable { .. })
        ));
        // macOS refuses `mmap(PROT_EXEC)` of a file (EPERM, measured) and the backend maps it
        // read-only and raises it; the result must be readable with the file's bytes.
        vm::map_file(&exec, 0, 2 * page(), reservation.as_ptr(), Protection::ReadExecute)
            .expect("an executable view");
        assert_eq!(read_mapped(reservation.as_ptr(), page()), expected_page(0));
        assert_eq!(region_protection(reservation.base()), libc_prot_rx(), "the page is r-x");
        vm::unmap_and_release(reservation.as_ptr(), 2 * page()).expect("unmap");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn a_view_of_a_non_executable_file_cannot_be_raised_to_execute() {
    // Windows caps every view of a PAGE_READONLY section below execute (87, measured); `mprotect`
    // would not refuse here, so the backend enforces the cap from its record.
    let path = fixture(1);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(page(), page()).expect("reserve");
    // SAFETY: an exact-size placeholder this test owns.
    unsafe {
        vm::map_file(&file, 0, page(), reservation.as_ptr(), Protection::Read).expect("map");
        let err = vm::protect(reservation.as_ptr(), page(), Protection::ReadExecute)
            .expect_err("a non-executable file's view cannot become executable");
        assert_eq!(err.os_error().map(|e| e.code()), Some(13), "EACCES: {err}");
        assert_ne!(region_protection(reservation.base()), libc_prot_rx());
        vm::unmap_and_release(reservation.as_ptr(), page()).expect("unmap");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn a_writable_file_view_is_copy_on_write_and_does_not_touch_the_file() {
    let path = fixture(1);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let reservation = vm::reserve_placeholder(page(), page()).expect("reserve");
    // SAFETY: an exact-size placeholder this test owns.
    unsafe {
        vm::map_file(&file, 0, page(), reservation.as_ptr(), Protection::ReadWrite).expect("map");
        *reservation.as_ptr() = b'X';
        assert_eq!(*reservation.as_ptr(), b'X');
        vm::unmap_and_release(reservation.as_ptr(), page()).expect("unmap");
    }
    assert_eq!(fs::read(&path).expect("reread")[..page()], expected_page(0)[..], "file untouched");
    let _ = fs::remove_file(&path);
}

#[test]
fn a_shared_view_writes_the_file_and_is_refused_for_a_read_only_descriptor() {
    let path = fixture(1);
    let read_only = fs::File::open(&path).expect("open read-only");
    match vm::share_file_for_mapping(read_only, &path) {
        Err(VmError::SectionCreate { source, .. }) => assert_eq!(source.code(), 13, "EACCES"),
        other => panic!("a read-only descriptor cannot back a shared writable view: {other:?}"),
    }
    let rw = fs::OpenOptions::new().read(true).write(true).open(&path).expect("open rw");
    let shared = vm::share_file_for_mapping(rw, &path).expect("share");
    assert!(shared.is_shared());
    let reservation = vm::reserve_placeholder(page(), page()).expect("reserve");
    // SAFETY: an exact-size placeholder this test owns.
    unsafe {
        vm::map_file(&shared, 0, page(), reservation.as_ptr(), Protection::ReadWrite).expect("map");
        *reservation.as_ptr().add(100) = b'S';
        vm::sync_view(&shared, reservation.as_ptr(), page()).expect("msync");
        vm::unmap_and_release(reservation.as_ptr(), page()).expect("unmap");
    }
    assert_eq!(fs::read(&path).expect("reread")[100], b'S', "the store reached the file");
    let _ = fs::remove_file(&path);
}

#[test]
fn unmap_refuses_a_partial_view_in_both_directions_and_leaves_it_intact() {
    let path = fixture(8);
    let file = vm::open_file_for_mapping(&path, MapExecutability::NonExecutable).expect("open");
    let span = 4 * page();
    let reservation = vm::reserve_placeholder(span, page()).expect("reserve");
    // SAFETY: an exact-size placeholder this test owns; the refused calls dereference nothing.
    unsafe {
        vm::map_file(&file, 0, span, reservation.as_ptr(), Protection::Read).expect("map");
        // Chop the view into protection regions first: the record, not the kernel's regions,
        // decides the extent, so this must not change the answers below.
        vm::protect(reservation.as_ptr().add(page()), page(), Protection::None).expect("none");
        vm::protect(reservation.as_ptr().add(2 * page()), page(), Protection::ReadWrite)
            .expect("copy-on-write page");
        match vm::unmap(reservation.as_ptr().add(page()), page()) {
            Err(VmError::NotViewBase { view_base, view_len, offset, .. }) => {
                assert_eq!((view_base, view_len, offset), (reservation.base(), span, page()));
            }
            other => panic!("expected NotViewBase, got {other:?}"),
        }
        for operation in ["unmap", "unmap_and_release"] {
            let result = if operation == "unmap" {
                vm::unmap(reservation.as_ptr(), page())
            } else {
                vm::unmap_and_release(reservation.as_ptr(), page())
            };
            match result {
                Err(VmError::ViewSizeMismatch { requested, view_len, surviving, .. }) => {
                    assert_eq!((requested, view_len, surviving), (page(), span, span - page()));
                }
                other => panic!("{operation}: expected ViewSizeMismatch, got {other:?}"),
            }
        }
        assert_eq!(read_mapped(reservation.as_ptr(), page()), expected_page(0), "page 0 intact");
        assert_eq!(read_mapped(reservation.as_ptr().add(3 * page()), page()), expected_page(3));
        vm::unmap_and_release(reservation.as_ptr(), span).expect("the whole view unmaps");
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn releasing_a_split_placeholder_by_its_parent_is_refused_with_both_extents() {
    let parent = vm::reserve_placeholder(4 * page(), page()).expect("reserve");
    let piece = vm::split_placeholder(&parent, 0, page()).expect("split");
    match vm::release(parent) {
        Err(VmError::ReleaseExtentMismatch { requested, actual, .. }) => {
            assert_eq!((requested, actual), (4 * page(), page()));
        }
        other => panic!("expected ReleaseExtentMismatch, got {other:?}"),
    }
    vm::release(piece).expect("the piece releases");
    let rest = parent.subrange(page(), 3 * page(), ReservationKind::Placeholder).expect("rest");
    vm::release(rest).expect("and so does the rest");
    assert!(vm::release(rest).is_err(), "a double release is refused");
}

#[test]
fn a_zero_length_or_missing_file_is_refused_by_name() {
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    fs::create_dir_all(&dir).expect("dir");
    let empty = dir.join(format!("empty-{}.bin", std::process::id()));
    fs::write(&empty, b"").expect("empty file");
    assert!(matches!(
        vm::open_file_for_mapping(&empty, MapExecutability::NonExecutable),
        Err(VmError::EmptyFile { .. })
    ));
    let missing = dir.join("does-not-exist.bin");
    match vm::open_file_for_mapping(&missing, MapExecutability::NonExecutable) {
        Err(VmError::FileOpen { path, source, .. }) => {
            assert!(path.contains("does-not-exist.bin"));
            assert_eq!(source.code(), 2, "ENOENT");
        }
        other => panic!("expected FileOpen, got {other:?}"),
    }
    let _ = fs::remove_file(&empty);
}

#[test]
fn a_dual_mapped_section_shows_a_write_through_the_other_view_and_executes() {
    let size = page() as u64;
    let section = vm::create_shared_section(size).expect("section");
    // SAFETY: the two views are fresh mappings this test owns and unmaps at the end.
    unsafe {
        let rw = vm::map_section(&section, 0, page(), Protection::ReadWrite).expect("rw view");
        let rx = vm::map_section(&section, 0, page(), Protection::ReadExecute).expect("rx view");
        assert_ne!(rw, rx);
        // `mov w0, #42; ret`
        (rw as *mut u32).write(0x5280_0540);
        (rw as *mut u32).add(1).write(0xd65f_03c0);
        clear_icache(rx, 8);
        assert_eq!((rx as *const u32).read(), 0x5280_0540, "the RX view sees the RW view's store");
        let function: extern "C" fn() -> i32 = std::mem::transmute(rx);
        assert_eq!(function(), 42, "and it executes");
        assert_eq!(region_protection(rw as usize), 3, "rw- view");
        assert_eq!(region_protection(rx as usize), libc_prot_rx(), "r-x view");
        vm::unmap_and_release(rw, page()).expect("unmap rw");
        vm::unmap_and_release(rx, page()).expect("unmap rx");
    }
}

#[test]
fn process_memory_is_one_consistent_snapshot_of_true_counters() {
    let memory = vm::process_memory().expect("process memory");
    assert!(memory.resident > 0);
    assert!(memory.resident_shared <= memory.resident);
    assert!(memory.address_space >= memory.resident);
    // The code span must contain this very function.
    let here = process_memory_is_one_consistent_snapshot_of_true_counters as fn() as usize;
    assert!(
        memory.executable_code.contains(&here),
        "{:#x} is not in the executable's code {:x?}",
        here,
        memory.executable_code
    );
    // `commit_charge` is phys_footprint; a second reader of the same ledger must agree with it
    // within what the process allocated between the two reads (two counters, one check).
    let footprint = rusage_footprint();
    let charge = vm::process_commit_charge().expect("footprint");
    let gap = charge.abs_diff(footprint);
    assert!(gap < 8 << 20, "task_info {charge} vs proc_pid_rusage {footprint}");
}

// ---------------------------------------------------------------------------------------------
// Host facts, read independently of the backend under test.
// ---------------------------------------------------------------------------------------------

const VM_REGION_BASIC_INFO_64: i32 = 9;

#[repr(C, packed(4))]
#[derive(Default)]
struct RegionBasicInfo64 {
    protection: i32,
    max_protection: i32,
    inheritance: u32,
    shared: u32,
    reserved: u32,
    offset: u64,
    behavior: i32,
    user_wired_count: u16,
}

extern "C" {
    static mach_task_self_: u32;
    fn mach_vm_region(
        task: u32,
        address: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
        object_name: *mut u32,
    ) -> i32;
    fn sys_icache_invalidate(start: *mut u8, len: usize);
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
}

fn libc_prot_rx() -> i32 {
    1 | 4
}

/// The kernel's own record of the protection at `address`, from `mach_vm_region` -- not from the
/// backend's registry.
fn region_protection(address: usize) -> i32 {
    let mut at = address as u64;
    let mut size = 0u64;
    let mut info = RegionBasicInfo64::default();
    let mut count = (std::mem::size_of::<RegionBasicInfo64>() / 4) as u32;
    let mut object = 0u32;
    // SAFETY: every out-pointer is a live local of the right type and `count` is its size in words.
    let kr = unsafe {
        mach_vm_region(
            mach_task_self_,
            &mut at,
            &mut size,
            VM_REGION_BASIC_INFO_64,
            std::ptr::addr_of_mut!(info).cast(),
            &mut count,
            &mut object,
        )
    };
    assert_eq!(kr, 0, "mach_vm_region");
    assert!(at <= address as u64, "the region query landed after {address:#x}");
    info.protection
}

/// # Safety
///
/// `[start, start + len)` must be mapped.
unsafe fn clear_icache(start: *mut u8, len: usize) {
    sys_icache_invalidate(start, len);
}

/// `ri_phys_footprint` from `proc_pid_rusage(RUSAGE_INFO_V2)`: the same ledger `task_info` reads,
/// through a different call.
fn rusage_footprint() -> u64 {
    // rusage_info_v2 (<sys/resource.h>): a 16-byte uuid (two u64 slots), then user, system,
    // pkg_idle_wkups, interrupt_wkups, pageins, wired_size, resident_size, phys_footprint -- so
    // ri_phys_footprint is u64 slot 9.
    let mut buffer = [0u64; 32];
    // SAFETY: `buffer` is larger than rusage_info_v2 and writable.
    let rc = unsafe { proc_pid_rusage(std::process::id() as i32, 2, buffer.as_mut_ptr()) };
    assert_eq!(rc, 0, "proc_pid_rusage");
    buffer[2 + 7]
}
