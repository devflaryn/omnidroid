use std::sync::Arc;

use omni_linux::errno::{EACCES, EEXIST, EINVAL};
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_PRIVATE: u64 = 2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANON: u64 = 0x20;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

fn process() -> (Arc<Process>, omni_linux::Task, u64) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // One directory per test: tests run in parallel, and rewriting a file another test has open
    // races on Windows.
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("omni-linux-mm-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("objects/bb")).unwrap();
    // 6000 bytes: one full page and a partial second page.
    let body: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8 + 1).collect();
    std::fs::write(dir.join("objects/bb/bb01"), &body).unwrap();
    let m = manifest::parse("d\t755\t/\nd\t755\t/system\nd\t755\t/system/lib64\nf\t644\t6000\tbb01\t/system/lib64/libx.so\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

fn mmap(p: &Process, t: &mut omni_linux::Task, a: [u64; 6]) -> i64 {
    p.syscall(t, nr::MMAP, a) as i64
}

#[test]
fn anonymous_memory_is_zeroed_and_writable() {
    let (p, mut t, _) = process();
    let at = mmap(&p, &mut t, [0, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]);
    assert!(at > 0);
    assert_eq!(p.mem.read(at as u64 + 5000, 4).unwrap(), [0; 4]);
    p.mem.write(at as u64 + 5000, b"abcd").unwrap();
}

#[test]
fn map_fixed_replaces_part_of_an_existing_mapping() {
    let (p, mut t, _) = process();
    let at = mmap(&p, &mut t, [0, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(at + 4096, b"old!").unwrap();
    let again = mmap(&p, &mut t, [at + 4096, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_FIXED, u64::MAX, 0]);
    assert_eq!(again as u64, at + 4096);
    assert_eq!(p.mem.read(at + 4096, 4).unwrap(), [0; 4], "the replaced page is fresh");
    let noreplace = mmap(&p, &mut t, [at, 4096, PROT_READ, MAP_PRIVATE | MAP_ANON | MAP_FIXED_NOREPLACE, u64::MAX, 0]);
    assert_eq!(noreplace, -(EEXIST.0 as i64));
}

#[test]
fn a_file_mapping_past_end_of_file_reads_zeros_in_the_tail() {
    let (p, mut t, s) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, 3 * 4096, PROT_READ, MAP_PRIVATE, fd, 0]) as u64;
    assert_eq!(p.mem.read(at, 3).unwrap(), [1, 2, 3]);
    assert_eq!(p.mem.read(at + 5999, 1).unwrap(), [(5999 % 251) as u8 + 1], "last file byte");
    assert_eq!(p.mem.read(at + 6000, 8).unwrap(), [0; 8], "tail of the last file page");
    assert_eq!(p.mem.read(at + 2 * 4096, 8).unwrap(), [0; 8], "a page wholly past end of file");
}

#[test]
fn a_private_writable_file_mapping_does_not_change_the_file() {
    let (p, mut t, s) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0]) as u64;
    p.mem.write(at, b"ZZ").unwrap();
    let again = mmap(&p, &mut t, [0, 4096, PROT_READ, MAP_PRIVATE, fd, 0]) as u64;
    assert_eq!(p.mem.read(again, 2).unwrap(), [1, 2]);
}

#[test]
fn write_and_execute_together_is_refused_by_name() {
    let (p, mut t, _) = process();
    let r = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]);
    assert_eq!(r, -(EACCES.0 as i64));
    assert!(p.refusals.report().contains("PROT_WRITE|PROT_EXEC"));
    assert_eq!(mmap(&p, &mut t, [0, 0, PROT_READ, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]), -(EINVAL.0 as i64));
}

#[test]
fn munmap_over_holes_succeeds_and_mprotect_takes_effect() {
    let (p, mut t, _) = process();
    let at = mmap(&p, &mut t, [0, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at + 4096, 4096, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, 4 * 4096, 0, 0, 0, 0]), 0, "a range with a hole in it");
    let again = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [again, 4096, PROT_READ, 0, 0, 0]), 0);
    assert!(p.mem.write(again, b"x").is_err(), "read-only now");
}
