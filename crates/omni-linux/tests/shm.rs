//! Shared memory: `memfd_create` grows, reads back what was written, and a `MAP_SHARED` mapping is
//! live -- a write through the mapping is read back from the descriptor, which is what a graphics
//! buffer or `hidl_memory` passed between processes relies on.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 1;

fn process() -> (Arc<Process>, omni_linux::Task, u64, u64) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    let page = p.mm.page_size();
    (p, t, s, page)
}

#[test]
fn a_memfd_grows_and_reads_back() {
    let (p, mut t, s, _) = process();
    p.mem.write(s, b"buffer\0").unwrap();
    let fd = p.syscall(&mut t, nr::MEMFD_CREATE, [s, 1, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    assert_eq!(p.syscall(&mut t, nr::FTRUNCATE, [fd, 4096, 0, 0, 0, 0]), 0);
    p.mem.write(s + 64, b"hello shared memory").unwrap();
    assert_eq!(p.syscall(&mut t, nr::WRITE, [fd, s + 64, 19, 0, 0, 0]), 19);
    p.syscall(&mut t, nr::LSEEK, [fd, 0, 0, 0, 0, 0]);
    assert_eq!(p.syscall(&mut t, nr::READ, [fd, s + 256, 19, 0, 0, 0]), 19);
    assert_eq!(p.mem.read(s + 256, 19).unwrap(), b"hello shared memory");
}

#[test]
fn a_shared_mapping_of_a_memfd_is_live() {
    let (p, mut t, s, page) = process();
    p.mem.write(s, b"buf\0").unwrap();
    let fd = p.syscall(&mut t, nr::MEMFD_CREATE, [s, 0, 0, 0, 0, 0]);
    assert_eq!(p.syscall(&mut t, nr::FTRUNCATE, [fd, page, 0, 0, 0, 0]), 0);
    let at = p.syscall(&mut t, nr::MMAP, [0, page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0]);
    assert!((at as i64) > 0, "{at}");
    // A write through the mapping is read back through the descriptor.
    p.mem.write(at + 100, b"through the map").unwrap();
    p.syscall(&mut t, nr::LSEEK, [fd, 100, 0, 0, 0, 0]);
    assert_eq!(p.syscall(&mut t, nr::READ, [fd, s + 512, 15, 0, 0, 0]), 15);
    assert_eq!(p.mem.read(s + 512, 15).unwrap(), b"through the map");
    // And a second shared mapping of the same descriptor sees it.
    let at2 = p.syscall(&mut t, nr::MMAP, [0, page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0]);
    assert!((at2 as i64) > 0 && at2 != at);
    assert_eq!(p.mem.read(at2 + 100, 15).unwrap(), b"through the map");
    p.mem.write(at2 + 200, b"and back again").unwrap();
    assert_eq!(p.mem.read(at + 200, 14).unwrap(), b"and back again");
}

/// A region handed to another host process stays openable by its path after the process that made
/// it lets it go (a gralloc buffer the allocator made, which the app later sends to
/// SurfaceFlinger); one never handed over is removed with it.
#[test]
fn a_crossed_region_outlives_its_maker() {
    let made = omni_linux::shm::Shm::create("buffer").unwrap();
    made.set_len(4096).unwrap();
    let path = made.host_path_crossing().to_path_buf();
    drop(made);
    let reopened = omni_linux::shm::Shm::open_path("buffer", &path, 4096).expect("still there");
    assert_eq!(reopened.len(), 4096);
    drop(reopened);
    let _ = std::fs::remove_file(&path);

    let kept = omni_linux::shm::Shm::create("private").unwrap();
    let path = kept.host_path().to_path_buf();
    drop(kept);
    assert!(!path.exists(), "a region never handed over is removed with its maker");
}
