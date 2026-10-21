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

/// A file mapped shared past its end follows it as another descriptor grows it (AppSearch's icing):
/// a host page wholly past the end is refused (the guest's SIGBUS, `past_end_of_file`), and once
/// `ftruncate` grows the file over it it reads zeros, shows what `pwrite` puts there, and a write
/// through the mapping is in the file. Where a view cannot run past its file's end (Windows, Linux)
/// that part is anonymous, as before: skipped.
#[test]
fn a_shared_mapping_past_the_end_follows_the_file_as_it_grows() {
    if !omni_platform::vm::shared_view_may_pass_end_of_file() {
        return;
    }
    let data = std::env::temp_dir().join(format!("omni-grow-mapped-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![(b"/data".to_vec(), data.clone())], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, b"/data/icing.array\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2 | 0o100, 0o600, 0, 0]) as u64;
    let other = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]) as u64;
    assert!((fd as i64) >= 0 && (other as i64) >= 0);
    let buf = s + 0x1000;
    p.mem.write(buf, &[b'h'; 100]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::PWRITE64, [fd, buf, 100, 0, 0, 0]), 100);
    const KIB: u64 = 1024;
    let map = p.syscall(&mut t, nr::MMAP, [0, 1024 * KIB, 3, 1, fd, 0]) as i64;
    assert!(map > 0, "mmap 1 MiB of a 100-byte file shared: {map}");
    let map = map as u64;
    let space = p.mem.space();
    assert_eq!(p.mem.read(map, 100).unwrap(), [b'h'; 100]);
    assert!(p.mem.read(map + 512 * KIB, 8).is_err(), "past the end: refused");
    assert!(space.past_end_of_file((map + 512 * KIB) as usize, omni_mem::FaultAccess::Read) && !space.past_end_of_file(map as usize, omni_mem::FaultAccess::Read), "the guest's SIGBUS there");
    assert_eq!(p.syscall(&mut t, nr::FTRUNCATE, [other, 768 * KIB, 0, 0, 0, 0]) as i64, 0);
    assert!(!space.past_end_of_file((map + 512 * KIB) as usize, omni_mem::FaultAccess::Read));
    assert_eq!(p.mem.read(map + 512 * KIB, 8).unwrap(), [0; 8], "grown over: zeros");
    p.mem.write(buf, b"pwritten").unwrap();
    assert_eq!(p.syscall(&mut t, nr::PWRITE64, [other, buf, 8, 600 * KIB, 0, 0]), 8);
    assert_eq!(p.mem.read(map + 600 * KIB, 8).unwrap(), b"pwritten", "the mapping shows pwrite");
    p.mem.write(map + 700 * KIB, b"stored").unwrap();
    assert_eq!(p.syscall(&mut t, nr::PREAD64, [other, buf, 6, 700 * KIB, 0, 0]), 6);
    assert_eq!(p.mem.read(buf, 6).unwrap(), b"stored", "a write through the mapping is in the file");
    assert!(p.mem.read(map + 900 * KIB, 8).is_err() && space.past_end_of_file((map + 900 * KIB) as usize, omni_mem::FaultAccess::Read), "past the new end");
    let _ = std::fs::remove_dir_all(&data);
}

/// A process whose `/data` is a scratch directory, and `/data/<name>` holding `bytes`, opened
/// read-write twice: the mapping's descriptor and another.
fn with_file(name: &str, bytes: &[u8]) -> (std::path::PathBuf, std::sync::Arc<Process>, omni_linux::process::Task, u64, u64) {
    let data = std::env::temp_dir().join(format!("omni-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join(name), bytes).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![(b"/data".to_vec(), data.clone())], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, format!("/data/{name}\0").as_bytes()).unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]);
    let other = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]);
    assert!((fd as i64) >= 0 && (other as i64) >= 0);
    (data, p, t, fd, other)
}

/// An empty file mapped shared at its largest follows the file as another descriptor grows it, as
/// a new icing array file does: its whole boot's writes were anonymous memory, and the next boot
/// found the file all zeros ("Array storage bad crc 0 vs 2042943926"). Where a view cannot run
/// past its file's end it is anonymous, as before: skipped.
#[test]
fn an_empty_file_mapped_shared_follows_the_file_as_it_grows() {
    if !omni_platform::vm::shared_view_may_pass_end_of_file() {
        return;
    }
    let (data, p, mut t, fd, other) = with_file("trie.n", b"");
    const KIB: u64 = 1024;
    let map = p.syscall(&mut t, nr::MMAP, [0, 1024 * KIB, 3, 1, fd, 0]) as i64;
    assert!(map > 0, "mmap 1 MiB of an empty file shared: {map}");
    let map = map as u64;
    let space = p.mem.space();
    assert!(p.mem.read(map, 8).is_err() && space.past_end_of_file(map as usize, omni_mem::FaultAccess::Read), "past the end: refused");
    assert_eq!(p.syscall(&mut t, nr::FTRUNCATE, [other, 64 * KIB, 0, 0, 0, 0]) as i64, 0);
    p.mem.write(map + 100, b"trie node").unwrap();
    assert_eq!(std::fs::read(data.join("trie.n")).unwrap()[100..109], *b"trie node", "a write through the mapping is in the file");
    let buf = p.scratch() + 0x1000;
    p.mem.write(buf, b"pwritten").unwrap();
    assert_eq!(p.syscall(&mut t, nr::PWRITE64, [other, buf, 8, 32 * KIB, 0, 0]), 8);
    assert_eq!(p.mem.read(map + 32 * KIB, 8).unwrap(), b"pwritten", "the mapping shows pwrite");
    let _ = std::fs::remove_dir_all(&data);
}

/// `MAP_FIXED` of a shared mapping that runs past the file's end into a host page another mapping
/// holds -- a reservation around it -- succeeds, as Linux's does: the view that would follow the
/// file into that host page could not be made, and the call answered EEXIST. The file's bytes are
/// the file's, the rest reads zeros, the reservation's part beside it is still the reservation's.
#[test]
fn a_fixed_shared_mapping_past_the_end_beside_another_mapping_is_made() {
    let (data, p, mut t, fd, _) = with_file("fixed.shm", &[b'h'; 100]);
    let host = p.mem.space().page_size() as u64;
    let r = p.syscall(&mut t, nr::MMAP, [0, 2 * host, 0, 0x22, u64::MAX, 0]); // PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS
    let len = host + 4096;
    assert_eq!(p.syscall(&mut t, nr::MMAP, [r, len, 3, 0x11, fd, 0]), r, "MAP_SHARED | MAP_FIXED");
    assert_eq!(p.mem.read(r, 100).unwrap(), [b'h'; 100]);
    p.mem.write(r + 50, b"x").unwrap();
    assert_eq!(std::fs::read(data.join("fixed.shm")).unwrap()[50], b'x', "shared");
    if host > 4096 {
        let space = p.mem.space();
        assert_eq!(p.mem.read(r + len - 8, 8).unwrap(), [0; 8]);
        assert!(space.region_at((r + len) as usize).is_some_and(|x| x.protection == omni_mem::Protection::None && x.mapping.is_some()), "the reservation's");
    }
    let _ = std::fs::remove_dir_all(&data);
}

/// `mremap` growing a shared mapping that runs past its file's end copies the file's part and
/// succeeds: the host pages past the end hold nothing to copy (a read there was EFAULT, and the new
/// range was left mapped).
#[test]
fn a_shared_mapping_past_the_end_is_moved_by_mremap() {
    let (data, p, mut t, fd, _) = with_file("grow.shm", &[b'h'; 100]);
    const KIB: u64 = 1024;
    let map = p.syscall(&mut t, nr::MMAP, [0, 1024 * KIB, 3, 1, fd, 0]);
    assert!((map as i64) > 0);
    let moved = p.syscall(&mut t, nr::MREMAP, [map, 1024 * KIB, 2048 * KIB, 1, 0, 0]) as i64; // MREMAP_MAYMOVE
    assert!(moved > 0, "mremap: {moved}");
    let moved = moved as u64;
    assert_eq!(p.mem.read(moved, 100).unwrap(), [b'h'; 100]);
    assert_eq!(p.mem.read(moved + 1024 * KIB, 8).unwrap(), [0; 8]);
    let _ = std::fs::remove_dir_all(&data);
}

/// `fallocate(fd, 0, offset, len)` makes the range part of the file, reading zeros; the engine
/// allocates its `memProfStorage` file so before mapping it ("Failed to fallocate ... errno
/// (Function not implemented)" while it was ENOSYS).
#[test]
fn fallocate_makes_the_range_part_of_the_file() {
    let data = std::env::temp_dir().join(format!("omni-fallocate-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![(b"/data".to_vec(), data.clone())], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, b"/data/memProfStorage.json\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2 | 0o100, 0o600, 0, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::FALLOCATE, [fd, 0, 0, 102_400, 0, 0]) as i64, 0);
    assert_eq!(std::fs::metadata(data.join("memProfStorage.json")).unwrap().len(), 102_400);
    // KEEP_SIZE: success, the size unchanged.
    assert_eq!(p.syscall(&mut t, nr::FALLOCATE, [fd, 1, 0, 1 << 20, 0, 0]) as i64, 0);
    assert_eq!(std::fs::metadata(data.join("memProfStorage.json")).unwrap().len(), 102_400);
    let map = p.syscall(&mut t, nr::MMAP, [0, 102_400, 3, 1, fd, 0]) as i64;
    assert!(map > 0, "mmap of the allocated file: {map}");
    let _ = std::fs::remove_dir_all(&data);
}
