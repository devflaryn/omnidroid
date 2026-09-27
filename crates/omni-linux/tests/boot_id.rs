//! The boot id, and the ashmem device libcutils names after it.
mod common;

/// `/proc/sys/kernel/random/boot_id` is one UUID for the boot, and libcutils' ashmem device is
/// named after it (`/dev/ashmem<boot_id>`).
#[test]
fn the_boot_id_names_the_ashmem_device() {
    let Some((status, out, err)) = common::run(&["/system/bin/cat", "/proc/sys/kernel/random/boot_id"]) else { return };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{err}");
    let id = out.trim();
    assert_eq!(id.len(), 36, "{out:?}");
    assert_eq!(id.as_bytes()[14], b'4', "a version-4 UUID: {id}");
    let (status, out, err) = common::run(&["/system/bin/ls", "-l", &format!("/dev/ashmem{id}")]).unwrap();
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}{err}");
    assert!(out.starts_with('c'), "a character device: {out}");
}

/// An ashmem region's descriptor is the ashmem device's: `fstat` answers a character device with
/// the device's number, as libcutils checks before it trusts the region.
#[test]
fn an_ashmem_descriptor_is_the_ashmem_device() {
    use omni_linux::syscall::nr;
    let m = omni_linux::manifest::parse("d\t755\t/\n").unwrap();
    let vfs = omni_linux::vfs::Vfs::new(omni_linux::vfs::Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = omni_linux::process::Process::for_tests(vfs, omni_linux::fd::Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, b"/dev/ashmem\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    assert_eq!(p.syscall(&mut t, nr::FSTAT, [fd, s + 256, 0, 0, 0, 0]), 0);
    let fd_mode = p.mem.read_u32(s + 256 + 16).unwrap();
    let fd_rdev = p.mem.read_u64(s + 256 + 32).unwrap();
    assert_eq!(p.syscall(&mut t, nr::NEWFSTATAT, [(-100i64) as u64, s, s + 512, 0, 0, 0]), 0);
    assert_eq!(fd_mode & 0o170000, 0o020000, "a character device");
    assert_eq!(fd_rdev, p.mem.read_u64(s + 512 + 32).unwrap(), "the device's own number");
    assert_ne!(fd_rdev, 0);
}
