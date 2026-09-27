//! ART's JIT code cache: one memfd, mapped shared, and then a second view of it placed with
//! `MAP_FIXED` over part of the first (`MemMap::RemapAtEnd` / `MapFileAtAddress`) -- read-execute
//! over what was read-write. Every ART process (system_server, SystemUI, apps) logged
//! "Failed to create JIT Code Cache: ... map(<addr>, 33554432, 0x5, 0x11, 5, 0) failed: File exists"
//! and ran without its JIT.
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const MB: u64 = 1 << 20;

#[test]
fn a_shared_view_placed_over_part_of_another_view_of_the_same_memfd() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, b"jit-cache\0").unwrap();
    let fd = p.syscall(&mut t, nr::MEMFD_CREATE, [s, 2, 0, 0, 0, 0]) as i64;
    assert!(fd >= 0, "memfd_create: {fd}");
    let fd = fd as u64;
    assert_eq!(p.syscall(&mut t, nr::FTRUNCATE, [fd, 64 * MB, 0, 0, 0, 0]), 0);
    // The whole file, read-write and shared.
    let base = p.syscall(&mut t, nr::MMAP, [0, 64 * MB, 3, 1, fd, 0]) as i64;
    assert!(base > 0, "the first view: {base}");
    let base = base as u64;
    p.mem.write(base, b"code").unwrap();
    // A view of the file's start, read-execute, over the second half of the first view.
    let at = base + 32 * MB;
    let got = p.syscall(&mut t, nr::MMAP, [at, 32 * MB, 5, 0x11, fd, 0]) as i64;
    assert_eq!(got, at as i64, "MAP_FIXED over part of a view of the same file");
    assert_eq!(p.mem.read(at, 4).unwrap(), b"code", "the second view is the same file");
    // A fixed view of the file placed over an anonymous PROT_NONE reservation.
    let reserve = p.syscall(&mut t, nr::MMAP, [0, 32 * MB, 0, 0x22, u64::MAX, 0]) as i64;
    assert!(reserve > 0);
    let got = p.syscall(&mut t, nr::MMAP, [reserve as u64, 32 * MB, 5, 0x11, fd, 0]) as i64;
    assert_eq!(got, reserve, "MAP_FIXED over a reservation");
    assert_eq!(p.mem.read(reserve as u64, 4).unwrap(), b"code");
}
