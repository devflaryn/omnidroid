//! The alias primitive the 4 KiB guest overlay stands on (`omni-mem`'s `subpage`, spec
//! 2026-09-29-4k-guest-pages): two host addresses, one memory. Pinned here before anything
//! depends on it.
use omni_platform::vm::{self, Protection};

fn page() -> usize {
    vm::page_size()
}

/// A committed read-write page, and a reserved page to alias it at.
fn pair() -> (*mut u8, *mut u8, vm::Reservation, vm::Reservation) {
    let src = vm::reserve(page(), page()).expect("reserve src");
    let dst = vm::reserve(page(), page()).expect("reserve dst");
    // SAFETY: a fresh reservation of exactly this size.
    unsafe { vm::commit(src.base() as *mut u8, page(), Protection::ReadWrite).expect("commit src") };
    (src.base() as *mut u8, dst.base() as *mut u8, src, dst)
}

#[test]
fn an_alias_is_the_same_memory_both_ways_whatever_the_source_allows() {
    if !vm::supports_alias() {
        return;
    }
    let (src, dst, _s, _d) = pair();
    // SAFETY: both pages are this test's; the alias is read-write by contract.
    unsafe {
        src.write(0x11);
        vm::alias(src, dst, page()).expect("alias");
        assert_eq!(dst.read(), 0x11, "the source's byte through the alias");
        dst.add(1).write(0x22);
        assert_eq!(src.add(1).read(), 0x22, "the alias's byte through the source");
        for p in [Protection::Read, Protection::None] {
            vm::protect(src, page(), p).expect("protect src");
            dst.add(2).write(0x33); // must not fault: the alias stays read-write
            vm::protect(src, page(), Protection::ReadWrite).expect("restore src");
            assert_eq!(src.add(2).read(), 0x33, "{p:?}");
            src.add(2).write(0);
        }
        vm::unalias(dst, page()).expect("unalias");
        assert_eq!(src.read(), 0x11, "unaliasing leaves the source alone");
    }
}

#[test]
fn a_fresh_mapping_over_the_source_detaches_the_alias() {
    if !vm::supports_alias() {
        return;
    }
    let (src, dst, _s, _d) = pair();
    // SAFETY: as above.
    unsafe {
        src.write(0x44);
        vm::alias(src, dst, page()).expect("alias");
        // omni-mem's decommit on macOS is a fresh MAP_FIXED mapping over the range.
        vm::decommit(src, page()).expect("decommit");
        vm::commit(src, page(), Protection::ReadWrite).expect("recommit");
        assert_eq!(src.read(), 0, "recommitted memory is zero");
        assert_eq!(dst.read(), 0x44, "the alias still holds the old memory: callers must re-alias");
    }
}

#[test]
fn an_alias_of_a_page_not_yet_touched_is_the_page() {
    if !vm::supports_alias() {
        return;
    }
    let (src, dst, _s, _d) = pair();
    // SAFETY: as above. The source was committed but never written: no memory object yet.
    unsafe {
        vm::alias(src, dst, page()).expect("alias");
        dst.write(0x55);
        assert_eq!(src.read(), 0x55, "a write through the alias is the source's");
    }
}

#[test]
fn only_a_host_that_needs_one_offers_an_alias() {
    if !cfg!(target_os = "macos") {
        assert!(!vm::supports_alias(), "no alias off macOS yet");
    }
}
