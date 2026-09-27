use std::sync::Arc;

use omni_linux::errno::EFAULT;
use omni_linux::guest::GuestMem;
use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};

fn mem() -> (GuestMem, u64, u64) {
    let space = Arc::new(GuestSpace::new().expect("a space"));
    let page = space.page_size();
    let rw = space
        .map_anonymous(Placement::Anywhere { align: page }, 2 * page, Protection::ReadWrite, CommitPolicy::Lazy)
        .expect("rw");
    let ro = space
        .map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("ro");
    space.protect(ro, page, Protection::Read).expect("read-only");
    (GuestMem::new(space, Default::default()), rw as u64, ro as u64)
}

#[test]
fn a_round_trip_through_lazily_committed_memory() {
    let (m, rw, _) = mem();
    m.write(rw + 4090, b"hello world").expect("write across a page boundary");
    assert_eq!(m.read(rw + 4090, 11).expect("read"), b"hello world");
}

#[test]
fn a_write_to_read_only_memory_is_efault() {
    let (m, _, ro) = mem();
    assert_eq!(m.write(ro, b"x"), Err(EFAULT));
    assert!(m.read(ro, 1).is_ok());
}

#[test]
fn unmapped_and_absurd_pointers_are_efault_not_a_crash() {
    let (m, _, _) = mem();
    assert_eq!(m.read(0xdead_0000, 5).map(|_| ()), Err(EFAULT));
    assert_eq!(m.read(0, 1).map(|_| ()), Err(EFAULT));
    assert_eq!(m.read(u64::MAX - 2, 8).map(|_| ()), Err(EFAULT));
}

#[test]
fn a_c_string_ending_at_the_end_of_the_mapping_is_read() {
    let (m, rw, _) = mem();
    let end = rw + 2 * 4096;
    m.write(end - 4, b"abc\0").expect("write");
    assert_eq!(m.read_cstr(end - 4, 4096).expect("string"), b"abc");
}

#[test]
fn a_tagged_pointer_reaches_the_untagged_address_as_the_tagged_address_abi_says() {
    let (m, rw, _) = mem();
    let tagged = rw | (0x02 << 56);
    m.write(tagged, b"tag").expect("write through a tagged pointer");
    assert_eq!(m.read(rw, 3).expect("read untagged"), b"tag");
}
