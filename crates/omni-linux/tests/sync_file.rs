//! Sync files: the kernel's fences (`linux/sync_file.h`). Composition here is synchronous, so every
//! sync file this kernel makes is signalled when it is made -- and it must still be a real
//! descriptor: readable at once, and answering `SYNC_IOC_FILE_INFO` (status, signal time) and
//! `SYNC_IOC_MERGE`, since libsync, ANGLE and SurfaceFlinger wait on and inspect fences by fd.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const SYNC_IOC_MERGE: u64 = 0xc030_3e03;
const SYNC_IOC_FILE_INFO: u64 = 0xc038_3e04;

#[test]
fn a_signalled_sync_file_is_readable_and_reports_itself_signalled() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    let fd = omni_linux::sync_file::signalled(&p).expect("a sync file") as u64;

    // poll: readable at once.
    let mut pfd = (fd as i32).to_le_bytes().to_vec();
    pfd.extend_from_slice(&1u16.to_le_bytes());
    pfd.extend_from_slice(&0u16.to_le_bytes());
    p.mem.write(s, &pfd).unwrap();
    p.mem.write(s + 0x20, &[0u8; 16]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::PPOLL, [s, 1, s + 0x20, 0, 0, 0]), 1, "one ready");
    assert_eq!(u16::from_le_bytes(p.mem.read(s + 6, 2).unwrap().try_into().unwrap()) & 1, 1, "POLLIN");

    // SYNC_IOC_FILE_INFO, first for the count, then with room for the fence.
    let info = s + 0x100;
    p.mem.write(info, &[0u8; 56]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd, SYNC_IOC_FILE_INFO, info, 0, 0, 0]), 0);
    let b = p.mem.read(info, 56).unwrap();
    assert_eq!(i32::from_le_bytes(b[32..36].try_into().unwrap()), 1, "status: signalled");
    assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 1, "one fence");
    let fences = s + 0x200;
    let mut req = vec![0u8; 56];
    req[40..44].copy_from_slice(&1u32.to_le_bytes());
    req[48..56].copy_from_slice(&fences.to_le_bytes());
    p.mem.write(info, &req).unwrap();
    assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd, SYNC_IOC_FILE_INFO, info, 0, 0, 0]), 0);
    let f = p.mem.read(fences, 80).unwrap();
    assert_eq!(i32::from_le_bytes(f[64..68].try_into().unwrap()), 1, "the fence: signalled");
    assert_ne!(u64::from_le_bytes(f[72..80].try_into().unwrap()), 0, "with its signal time");

    // SYNC_IOC_MERGE: a new, signalled sync file.
    let merge = s + 0x300;
    let mut md = vec![0u8; 48];
    md[32..36].copy_from_slice(&(fd as i32).to_le_bytes());
    p.mem.write(merge, &md).unwrap();
    assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd, SYNC_IOC_MERGE, merge, 0, 0, 0]), 0);
    let merged = i32::from_le_bytes(p.mem.read(merge + 36, 4).unwrap().try_into().unwrap());
    assert!(merged > fd as i32, "a new descriptor: {merged}");
    let _ = Arc::clone(&p);
}
