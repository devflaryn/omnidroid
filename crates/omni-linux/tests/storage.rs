//! Shared storage without FUSE (`crate::mount`, `crate::fuse`): init binds /mnt/user/0 onto
//! /storage, vold mounts the emulated volume on /mnt/user/0/emulated -- here a bind of
//! /data/media -- and an app's `/storage/emulated/0/...` is a file in /data/media/0. Before:
//! /mnt and /storage did not exist, vold's `mkdir /mnt/runtime/default/emulated` was ENOENT, the
//! volume "unmountable", `getExternalFilesDir` null, and the modified Roblox build's loader
//! aborted on it (r22: "JNI DETECTED ERROR IN APPLICATION: obj == null").
//!
//! Also a symbolic link on a writable mount (vold's /mnt/user/0/self/primary), and `/dev/fuse`'s
//! `FUSE_INIT`.
use std::path::Path;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, owners::Owners, vfs::{Binds, Sysroot, Vfs}};

const AT_FDCWD: u64 = -100i64 as u64;
const MS_BIND: u64 = 0x1000;
const MS_REC: u64 = 0x4000;

fn process(instance: &Path) -> (std::sync::Arc<Process>, omni_linux::Task) {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let writable = ["data", "mnt", "storage"].iter().map(|d| (format!("/{d}").into_bytes(), instance.join(d))).collect();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), writable, b"/x".to_vec())
        .with_binds(Binds::of(instance))
        .with_owners(Owners::of(instance));
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    (p, t)
}

/// `s` as a C string in the process's scratch memory at `slot` (1 KiB apart); its address.
fn cstr(p: &Process, slot: u64, s: &str) -> u64 {
    let at = p.scratch() + slot * 1024;
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    p.mem.write(at, &b).unwrap();
    at
}

fn read_file(p: &Process, t: &mut omni_linux::Task, path: &str) -> Result<Vec<u8>, i64> {
    let fd = p.syscall(t, nr::OPENAT, [AT_FDCWD, cstr(p, 0, path), 0, 0, 0, 0]) as i64;
    if fd < 0 {
        return Err(fd);
    }
    let buf = p.scratch() + 8 * 1024;
    let n = p.syscall(t, nr::READ, [fd as u64, buf, 4096, 0, 0, 0]) as i64;
    p.syscall(t, nr::CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    Ok(p.mem.read(buf, n.max(0) as usize).unwrap())
}

#[test]
fn storage_emulated_is_data_media_through_both_mounts() {
    let instance = std::env::temp_dir().join(format!("omni-storage-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    for d in ["data/media/0", "mnt/user/0/emulated", "storage"] {
        std::fs::create_dir_all(instance.join(d)).unwrap();
    }
    std::fs::write(instance.join("data/media/0/hello.txt"), b"from /data/media").unwrap();
    let (p, mut t) = process(&instance);

    // init: `mount none /mnt/user/0 /storage bind rec`.
    let r = p.syscall(&mut t, nr::MOUNT, [cstr(&p, 1, "/mnt/user/0"), cstr(&p, 2, "/storage"), 0, MS_BIND | MS_REC, 0, 0]);
    assert_eq!(r, 0, "bind /mnt/user/0 on /storage");
    // Before vold: /storage/emulated is the empty mount point.
    assert!(read_file(&p, &mut t, "/storage/emulated/0/hello.txt").is_err());

    // vold: the emulated volume's FUSE mount, after /storage's bind.
    let r = p.syscall(&mut t, nr::MOUNT, [cstr(&p, 1, "/dev/fuse"), cstr(&p, 2, "/mnt/user/0/emulated"), cstr(&p, 3, "fuse"), 0x6, cstr(&p, 4, "fd=3,rootmode=40000"), 0]);
    assert_eq!(r, 0, "the fuse mount of the emulated volume");

    assert_eq!(read_file(&p, &mut t, "/mnt/user/0/emulated/0/hello.txt").unwrap(), b"from /data/media");
    assert_eq!(read_file(&p, &mut t, "/storage/emulated/0/hello.txt").unwrap(), b"from /data/media", "seen through /storage too");

    // A file an app makes under /storage/emulated/0 is in /data/media/0.
    let dir = p.syscall(&mut t, nr::MKDIRAT, [AT_FDCWD, cstr(&p, 1, "/storage/emulated/0/Android"), 0o771, 0, 0, 0]);
    assert_eq!(dir, 0);
    assert!(instance.join("data/media/0/Android").is_dir(), "mkdir through /storage lands in /data/media");

    // vold's /mnt/user/0/self/primary -> /storage/emulated/0.
    std::fs::create_dir_all(instance.join("mnt/user/0/self")).unwrap();
    let r = p.syscall(&mut t, nr::SYMLINKAT, [cstr(&p, 1, "/storage/emulated/0"), AT_FDCWD, cstr(&p, 2, "/mnt/user/0/self/primary"), 0, 0, 0]);
    assert_eq!(r, 0, "symlinkat on a writable mount");
    let buf = p.scratch() + 8 * 1024;
    let n = p.syscall(&mut t, nr::READLINKAT, [AT_FDCWD, cstr(&p, 1, "/mnt/user/0/self/primary"), buf, 256, 0, 0]);
    assert_eq!(p.mem.read(buf, n as usize).unwrap(), b"/storage/emulated/0");
    assert_eq!(read_file(&p, &mut t, "/mnt/user/0/self/primary/hello.txt").unwrap(), b"from /data/media", "followed");
    let r = p.syscall(&mut t, nr::UNLINKAT, [AT_FDCWD, cstr(&p, 1, "/mnt/user/0/self/primary"), 0, 0, 0, 0]);
    assert_eq!(r, 0, "the link is removed, not its target");
    assert!(instance.join("data/media/0/hello.txt").exists());

    // /dev/fuse: the daemon's first read is FUSE_INIT.
    let fd = p.syscall(&mut t, nr::OPENAT, [AT_FDCWD, cstr(&p, 1, "/dev/fuse"), 2, 0, 0, 0]) as i64;
    assert!(fd >= 0, "open /dev/fuse: {fd}");
    let n = p.syscall(&mut t, nr::READ, [fd as u64, buf, 8192, 0, 0, 0]);
    let m = p.mem.read(buf, n as usize).unwrap();
    assert_eq!(n, 56);
    assert_eq!(u32::from_le_bytes(m[4..8].try_into().unwrap()), 26, "FUSE_INIT");
    assert_eq!(u32::from_le_bytes(m[40..44].try_into().unwrap()), 7, "protocol 7");
    assert_eq!(p.syscall(&mut t, nr::WRITE, [fd as u64, buf, 16, 0, 0, 0]), 16, "the reply is taken");

    let _ = std::fs::remove_dir_all(&instance);
}
