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

#[test]
fn an_address_in_a_file_mapping_is_described_as_file_plus_offset() {
    let (p, mut t, s) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, 2 * 4096, PROT_READ, MAP_PRIVATE, fd, 4096]) as u64;
    assert_eq!(p.mm.describe(at + 0x10).as_deref(), Some("/system/lib64/libx.so+0x1010"));
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, 2 * 4096, 0, 0, 0, 0]), 0);
    assert_eq!(p.mm.describe(at + 0x10), None, "gone with the mapping");
}

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

#[test]
fn mremap_fixed_moves_the_contents_over_an_existing_mapping() {
    let (p, mut t, _) = process();
    let a = mmap(&p, &mut t, [0, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    let b = mmap(&p, &mut t, [0, 4 * 4096, PROT_READ, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(a + 4096, b"moved").unwrap();
    let r = p.syscall(&mut t, nr::MREMAP, [a, 2 * 4096, 2 * 4096, MREMAP_MAYMOVE | MREMAP_FIXED, b + 4096, 0]);
    assert_eq!(r, b + 4096);
    assert_eq!(p.mem.read(b + 2 * 4096, 5).unwrap(), b"moved");
    assert!(p.mem.write(b + 4096, b"w").is_ok(), "the moved pages keep their protection");
    assert!(p.mem.read(a, 1).is_err(), "the old range is gone");
}

#[test]
fn mremap_maymove_grows_a_mapping_keeping_its_contents() {
    let (p, mut t, _) = process();
    let a = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(a, b"grow").unwrap();
    let r = p.syscall(&mut t, nr::MREMAP, [a, 4096, 3 * 4096, MREMAP_MAYMOVE, 0, 0]);
    assert!((r as i64) > 0);
    assert_eq!(p.mem.read(r, 4).unwrap(), b"grow");
    assert_eq!(p.mem.read(r + 2 * 4096, 4).unwrap(), [0; 4], "the new tail is zero");
}

#[test]
fn absurd_lengths_are_errors_not_overflows() {
    let (p, mut t, _) = process();
    let huge = u64::MAX - 100;
    assert_eq!(mmap(&p, &mut t, [0, huge, PROT_READ, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]), -12, "ENOMEM");
    let at = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, huge, 0, 0, 0, 0]) as i64, -22, "EINVAL");
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [at, huge, PROT_READ, 0, 0, 0]) as i64, -22, "EINVAL");
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at, huge, 4, 0, 0, 0]) as i64, -22, "EINVAL");
    assert_eq!(p.syscall(&mut t, nr::MREMAP, [at, 4096, huge, 1, 0, 0]) as i64, -12, "ENOMEM");
    assert!(p.mem.write(at, b"still mapped").is_ok(), "nothing was unmapped by the refused calls");
}
