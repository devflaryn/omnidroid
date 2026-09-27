//! File modes on a writable mount: `chmod` without write bits makes a file read-only -- and it
//! then stats and answers `access(W_OK)` as read-only -- because Android refuses to load a
//! writable dex file (apps targeting 34+, and `dalvikvm`).
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const AT_FDCWD: u64 = (-100i64) as u64;
const EACCES: i64 = -13;
const W_OK: u64 = 2;

fn with_data() -> (Arc<Process>, omni_linux::Task, u64) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("omni-linux-modes-{}-{n}", std::process::id()));
    let data = root.join("instance").join("data");
    std::fs::create_dir_all(&data).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&root, m), vec![(b"/data".to_vec(), data)], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

fn mode_of(p: &Process, t: &mut omni_linux::Task, s: u64) -> u32 {
    assert_eq!(p.syscall(t, nr::NEWFSTATAT, [AT_FDCWD, s, s + 512, 0, 0, 0]), 0);
    u32::from_le_bytes(p.mem.read(s + 512 + 16, 4).unwrap().try_into().unwrap()) & 0o777
}

#[test]
fn chmod_without_write_bits_makes_a_file_read_only_and_back() {
    let (p, mut t, s) = with_data();
    p.mem.write(s, b"/data/app.dex\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [AT_FDCWD, s, 0o102, 0o600, 0, 0]);
    assert!((fd as i64) >= 0);
    assert_ne!(mode_of(&p, &mut t, s) & 0o200, 0, "a new file is writable");
    assert_eq!(p.syscall(&mut t, nr::FACCESSAT, [AT_FDCWD, s, W_OK, 0, 0, 0]), 0);

    assert_eq!(p.syscall(&mut t, nr::FCHMOD, [fd, 0o444, 0, 0, 0, 0]), 0);
    assert_eq!(mode_of(&p, &mut t, s) & 0o222, 0, "read-only after chmod 0444");
    assert_eq!(p.syscall(&mut t, nr::FACCESSAT, [AT_FDCWD, s, W_OK, 0, 0, 0]) as i64, EACCES);
    assert_eq!(p.syscall(&mut t, nr::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);

    assert_eq!(p.syscall(&mut t, nr::FCHMODAT, [AT_FDCWD, s, 0o644, 0, 0, 0]), 0);
    assert_ne!(mode_of(&p, &mut t, s) & 0o200, 0, "writable again after chmod 0644");
    // chown to its own owner changes nothing and succeeds (to another is EPERM: tests/owners.rs).
    let uid = p.syscall(&mut t, nr::GETUID, [0; 6]);
    assert_eq!(p.syscall(&mut t, nr::FCHOWNAT, [AT_FDCWD, s, uid, uid, 0, 0]), 0);
    assert_ne!(mode_of(&p, &mut t, s) & 0o200, 0);
}
