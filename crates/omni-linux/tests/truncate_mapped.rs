//! A file another mapping holds is shortened as Linux shortens it: `ftruncate` succeeds and the
//! bytes past the new end read as zeros. Windows refuses to shorten a mapped file
//! (ERROR_USER_MAPPED_FILE); the EINVAL that became was SQLite's `SQLITE_IOERR_SHMOPEN` --
//! `ftruncate(<db>-shm, 3)` while another connection maps the `-shm` file -- and the contacts
//! provider died of it on the real-AOSP path.
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

#[test]
fn a_mapped_file_is_shortened_and_reads_zeros_past_its_end() {
    let data = std::env::temp_dir().join(format!("omni-truncate-mapped-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![(b"/data".to_vec(), data.clone())], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, b"/data/x.db-shm\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2 | 0o100, 0o600, 0, 0]) as i64;
    assert!(fd >= 0, "open: {fd}");
    let fd = fd as u64;
    let buf = s + 0x1000;
    p.mem.write(buf, &[0xab; 8192]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::PWRITE64, [fd, buf, 8192, 0, 0, 0]), 8192);
    let map = p.syscall(&mut t, nr::MMAP, [0, 8192, 3, 1, fd, 0]) as i64;
    assert!(map > 0, "mmap: {map}");
    assert_eq!(p.syscall(&mut t, nr::FTRUNCATE, [fd, 3, 0, 0, 0, 0]) as i64, 0, "ftruncate of a mapped file");
    let out = s + 0x4000;
    assert_eq!(p.syscall(&mut t, nr::PREAD64, [fd, out, 16, 3, 0, 0]) as i64 >= 0, true);
    let got = p.mem.read(out, 16).unwrap();
    assert!(got.iter().all(|b| *b == 0), "past the end reads zeros: {got:02x?}");
    assert_eq!(p.mem.read(map as u64, 3).unwrap(), [0xab; 3], "the kept bytes are kept");
    assert!(p.mem.read(map as u64 + 3, 64).unwrap().iter().all(|b| *b == 0), "the mapping reads zeros past the end");
    let _ = std::fs::remove_dir_all(&data);
}
