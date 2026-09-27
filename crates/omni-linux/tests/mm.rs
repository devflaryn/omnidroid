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

/// The process, a task, a scratch buffer, and the page size the guest is told (`AT_PAGESZ`): the
/// guest space's page, 4 KiB on Windows and x86-64 Linux, 16 KiB on Apple silicon.
fn process() -> (Arc<Process>, omni_linux::Task, u64, u64) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // One directory per test: tests run in parallel, and rewriting a file another test has open
    // races on Windows.
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("omni-linux-mm-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("objects/bb")).unwrap();
    // 6000 bytes: more than a 4 KiB page, less than a 16 KiB one.
    let body: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8 + 1).collect();
    std::fs::write(dir.join("objects/bb/bb01"), &body).unwrap();
    let m = manifest::parse("d\t755\t/\nd\t755\t/system\nd\t755\t/system/lib64\nf\t644\t6000\tbb01\t/system/lib64/libx.so\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    let page = p.mm.page_size();
    (p, t, s, page)
}

fn mmap(p: &Process, t: &mut omni_linux::Task, a: [u64; 6]) -> i64 {
    p.syscall(t, nr::MMAP, a) as i64
}

#[test]
fn the_page_is_the_guest_spaces_page_and_mappings_are_exact_at_it() {
    let (p, mut t, _, pg) = process();
    assert_eq!(pg, p.mem.space().page_size() as u64);
    assert!(pg >= 4096 && pg.is_power_of_two());
    let at = mmap(&p, &mut t, [0, 3 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(at % pg, 0);
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [at + pg, pg, PROT_READ, 0, 0, 0]), 0);
    assert!(p.mem.write(at + pg, b"x").is_err(), "the middle page is read-only");
    assert!(p.mem.write(at + 2 * pg, b"x").is_ok(), "its neighbours are not");
    assert!(p.mem.write(at + pg - 1, b"x").is_ok());
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at + pg, pg, 0, 0, 0, 0]), 0);
    assert!(p.mem.read(at + pg, 1).is_err(), "the middle page is gone");
    if pg > 4096 {
        // A Linux kernel with 16 KiB pages refuses a 4 KiB-aligned address, and so do we.
        assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at + 4096, 4096, 0, 0, 0, 0]), (-(EINVAL.0 as i64)) as u64);
    }
}

#[test]
fn anonymous_memory_is_zeroed_and_writable() {
    let (p, mut t, _, _) = process();
    let at = mmap(&p, &mut t, [0, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]);
    assert!(at > 0);
    assert_eq!(p.mem.read(at as u64 + 5000, 4).unwrap(), [0; 4]);
    p.mem.write(at as u64 + 5000, b"abcd").unwrap();
}

#[test]
fn map_fixed_replaces_part_of_an_existing_mapping() {
    let (p, mut t, _, pg) = process();
    let at = mmap(&p, &mut t, [0, 4 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(at + pg, b"old!").unwrap();
    let again = mmap(&p, &mut t, [at + pg, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_FIXED, u64::MAX, 0]);
    assert_eq!(again as u64, at + pg);
    assert_eq!(p.mem.read(at + pg, 4).unwrap(), [0; 4], "the replaced page is fresh");
    let noreplace = mmap(&p, &mut t, [at, pg, PROT_READ, MAP_PRIVATE | MAP_ANON | MAP_FIXED_NOREPLACE, u64::MAX, 0]);
    assert_eq!(noreplace, -(EEXIST.0 as i64));
}

#[test]
fn a_file_mapping_past_end_of_file_reads_zeros_in_the_tail() {
    let (p, mut t, s, pg) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, 3 * pg, PROT_READ, MAP_PRIVATE, fd, 0]) as u64;
    assert_eq!(p.mem.read(at, 3).unwrap(), [1, 2, 3]);
    assert_eq!(p.mem.read(at + 5999, 1).unwrap(), [(5999 % 251) as u8 + 1], "last file byte");
    assert_eq!(p.mem.read(at + 6000, 8).unwrap(), [0; 8], "tail of the last file page");
    assert_eq!(p.mem.read(at + 2 * pg, 8).unwrap(), [0; 8], "a page wholly past end of file");
}

#[test]
fn a_private_writable_file_mapping_does_not_change_the_file() {
    let (p, mut t, s, pg) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0]) as u64;
    p.mem.write(at, b"ZZ").unwrap();
    let again = mmap(&p, &mut t, [0, pg, PROT_READ, MAP_PRIVATE, fd, 0]) as u64;
    assert_eq!(p.mem.read(again, 2).unwrap(), [1, 2]);
}

#[test]
fn write_and_execute_together_is_refused_by_name() {
    let (p, mut t, _, _) = process();
    let r = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]);
    assert_eq!(r, -(EACCES.0 as i64));
    assert!(p.refusals.report().contains("PROT_WRITE|PROT_EXEC"));
    assert_eq!(mmap(&p, &mut t, [0, 0, PROT_READ, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]), -(EINVAL.0 as i64));
}

#[test]
fn munmap_over_holes_succeeds_and_mprotect_takes_effect() {
    let (p, mut t, _, pg) = process();
    let at = mmap(&p, &mut t, [0, 4 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at + pg, pg, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, 4 * pg, 0, 0, 0, 0]), 0, "a range with a hole in it");
    let again = mmap(&p, &mut t, [0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [again, pg, PROT_READ, 0, 0, 0]), 0);
    assert!(p.mem.write(again, b"x").is_err(), "read-only now");
}

#[test]
fn an_address_in_a_file_mapping_is_described_as_file_plus_offset() {
    let (p, mut t, s, pg) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = mmap(&p, &mut t, [0, 2 * pg, PROT_READ, MAP_PRIVATE, fd, pg]) as u64;
    assert_eq!(p.mm.describe(at + 0x10).as_deref(), Some(format!("/system/lib64/libx.so+{:#x}", pg + 0x10).as_str()));
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, 2 * pg, 0, 0, 0, 0]), 0);
    assert_eq!(p.mm.describe(at + 0x10), None, "gone with the mapping");
}

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

#[test]
fn mremap_fixed_moves_the_contents_over_an_existing_mapping() {
    let (p, mut t, _, pg) = process();
    let a = mmap(&p, &mut t, [0, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    let b = mmap(&p, &mut t, [0, 4 * pg, PROT_READ, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(a + pg, b"moved").unwrap();
    let r = p.syscall(&mut t, nr::MREMAP, [a, 2 * pg, 2 * pg, MREMAP_MAYMOVE | MREMAP_FIXED, b + pg, 0]);
    assert_eq!(r, b + pg);
    assert_eq!(p.mem.read(b + 2 * pg, 5).unwrap(), b"moved");
    assert!(p.mem.write(b + pg, b"w").is_ok(), "the moved pages keep their protection");
    assert!(p.mem.read(a, 1).is_err(), "the old range is gone");
}

#[test]
fn mremap_maymove_grows_a_mapping_keeping_its_contents() {
    let (p, mut t, _, pg) = process();
    let a = mmap(&p, &mut t, [0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(a, b"grow").unwrap();
    let r = p.syscall(&mut t, nr::MREMAP, [a, pg, 3 * pg, MREMAP_MAYMOVE, 0, 0]);
    assert!((r as i64) > 0);
    assert_eq!(p.mem.read(r, 4).unwrap(), b"grow");
    assert_eq!(p.mem.read(r + 2 * pg, 4).unwrap(), [0; 4], "the new tail is zero");
}

#[test]
fn absurd_lengths_are_errors_not_overflows() {
    let (p, mut t, _, _) = process();
    let huge = u64::MAX - 100;
    assert_eq!(mmap(&p, &mut t, [0, huge, PROT_READ, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]), -12, "ENOMEM");
    let at = mmap(&p, &mut t, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, huge, 0, 0, 0, 0]) as i64, -22, "EINVAL");
    assert_eq!(p.syscall(&mut t, nr::MPROTECT, [at, huge, PROT_READ, 0, 0, 0]) as i64, -22, "EINVAL");
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at, huge, 4, 0, 0, 0]) as i64, -22, "EINVAL");
    assert_eq!(p.syscall(&mut t, nr::MREMAP, [at, 4096, huge, 1, 0, 0]) as i64, -12, "ENOMEM");
    assert!(p.mem.write(at, b"still mapped").is_ok(), "nothing was unmapped by the refused calls");
}

/// `msync` is how ART's low-4-GiB allocator asks whether a page is in use: 0 when mapped, `ENOMEM`
/// when free. What lies outside the guest space is the host's, so it answers "in use" there.
#[test]
fn msync_tells_mapped_from_free_pages() {
    let (p, mut t, _, pg) = process();
    let at = mmap(&p, &mut t, [0, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.syscall(&mut t, nr::MSYNC, [at, 2 * pg, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at + pg, pg, 0, 0, 0, 0]), 0);
    assert_eq!(p.syscall(&mut t, nr::MSYNC, [at + pg, pg, 0, 0, 0, 0]) as i64, -12, "a free page is ENOMEM");
    assert_eq!(p.syscall(&mut t, nr::MSYNC, [at, 2 * pg, 0, 0, 0, 0]) as i64, -12, "a range with a hole is ENOMEM");
    assert_eq!(p.syscall(&mut t, nr::MSYNC, [at + 1, pg, 0, 0, 0, 0]) as i64, -(EINVAL.0 as i64));
    let base = p.mem.space().base() as u64;
    if base > pg {
        assert_eq!(p.syscall(&mut t, nr::MSYNC, [base - pg, pg, 0, 0, 0, 0]), 0, "below the space is the host's");
    }
}

/// ART's low-4-GiB allocator asks for each candidate address with a hint and gives back whatever
/// else it gets: below 4 GiB and outside the guest space, where nothing can be mapped, the answer
/// is `ENOMEM` at once rather than a mapping elsewhere made and unmade for every page it tries.
#[test]
fn a_low_hint_outside_the_space_is_enomem_and_a_high_one_is_a_preference() {
    let (p, mut t, _, pg) = process();
    let base = p.mem.space().base() as u64;
    let end = p.mem.space().end() as u64;
    let low = 0x1000_0000u64;
    if base > low + pg {
        assert_eq!(mmap(&p, &mut t, [low, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]), -12);
    }
    let high = mmap(&p, &mut t, [(end + (1 << 30)).max(1 << 40), pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]);
    assert!(high > 0, "a high hint outside the space still maps somewhere: {high}");
}

/// A hint at a free address inside the space is where the mapping goes: ART reserves its boot image
/// at the address it chose (`PROT_NONE`, then maps the image over it).
#[test]
fn a_free_hint_inside_the_space_is_honoured() {
    let (p, mut t, _, pg) = process();
    let hint = (p.mem.space().base() as u64 + (1 << 30)) & !(pg - 1);
    let len = 0x6bc_8000u64;
    assert_eq!(mmap(&p, &mut t, [hint, len, 0, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64, hint);
}

/// `MADV_DONTNEED` on private anonymous memory: the next read is zeros, as ART's arenas rely on
/// when they reuse memory released that way.
#[test]
fn madv_dontneed_reads_back_zeros() {
    let (p, mut t, _, pg) = process();
    let len = 64 * pg;
    let at = mmap(&p, &mut t, [0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    for i in 0..64 {
        p.mem.write(at + i * pg + 8, &[0xAB; 64]).unwrap();
    }
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at + pg, 62 * pg, 4, 0, 0, 0]), 0);
    assert_eq!(p.mem.read(at + 8, 4).unwrap(), vec![0xAB; 4], "outside the range is kept");
    for i in 1..63 {
        assert_eq!(p.mem.read(at + i * pg + 8, 64).unwrap(), vec![0; 64], "page {i} reads zeros");
    }
    assert_eq!(p.mem.read(at + 63 * pg + 8, 4).unwrap(), vec![0xAB; 4]);
}

/// `MADV_REMOVE` that answers success must leave zeros, as Linux's does: ART's arena pool zeroes
/// released arenas with it and trusts the answer (garbage there became garbage `DexCache` entries).
#[test]
fn madv_remove_reads_back_zeros_and_an_unaligned_start_is_einval() {
    let (p, mut t, _, pg) = process();
    let at = mmap(&p, &mut t, [0, 4 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(at + pg, &[0xCD; 64]).unwrap();
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at + pg, 2 * pg, 9, 0, 0, 0]), 0);
    assert_eq!(p.mem.read(at + pg, 64).unwrap(), vec![0; 64]);
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at + 8, pg, 4, 0, 0, 0]) as i64, -(EINVAL.0 as i64));
}

/// `MREMAP_DONTUNMAP` (ART's compacting GC moves its space's pages aside with it): the pages move
/// to the new address, and the old range stays mapped, reading zeros.
#[test]
fn mremap_dontunmap_moves_the_pages_and_leaves_the_old_range_mapped_and_empty() {
    let (p, mut t, _, pg) = process();
    let len = 8 * pg;
    let old = mmap(&p, &mut t, [0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    let target = mmap(&p, &mut t, [0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as u64;
    p.mem.write(old + 3 * pg + 5, b"moved").unwrap();
    // MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP
    assert_eq!(p.syscall(&mut t, nr::MREMAP, [old, len, len, 7, target, 0]), target);
    assert_eq!(p.mem.read(target + 3 * pg + 5, 5).unwrap(), b"moved");
    assert_eq!(p.mem.read(old + 3 * pg + 5, 5).unwrap(), vec![0; 5], "the old range is mapped and empty");
    p.mem.write(old, b"x").unwrap();
    // Different lengths are EINVAL with DONTUNMAP.
    assert_eq!(p.syscall(&mut t, nr::MREMAP, [old, len, 2 * len, 7, target, 0]) as i64, -(EINVAL.0 as i64));
}
