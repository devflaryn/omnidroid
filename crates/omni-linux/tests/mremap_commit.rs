//! `mremap` moves what the old range holds, not its every byte: ART's concurrent-mark-compact GC
//! moves its 512 MiB space with `MREMAP_MAYMOVE | MREMAP_DONTUNMAP`, and a byte-for-byte copy read
//! the never-touched pages as zeros and wrote them -- 504 MiB committed in every idle Java process.
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const MB: u64 = 1 << 20;

#[test]
fn a_moved_range_commits_only_what_it_held() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let old = p.syscall(&mut t, nr::MMAP, [0, 64 * MB, 3, 0x22, u64::MAX, 0]) as i64;
    assert!(old > 0);
    let old = old as u64;
    // One page written, at the end.
    p.mem.write(old + 64 * MB - 4096, b"held").unwrap();
    let before = p.mem.space().stats().committed;
    // MREMAP_MAYMOVE | MREMAP_DONTUNMAP.
    let new = p.syscall(&mut t, nr::MREMAP, [old, 64 * MB, 64 * MB, 1 | 4, 0, 0]) as i64;
    assert!(new > 0, "mremap: {new}");
    let new = new as u64;
    assert_ne!(new, old, "DONTUNMAP moves");
    assert_eq!(p.mem.read(new + 64 * MB - 4096, 4).unwrap(), b"held", "the content moved");
    assert_eq!(p.mem.read(new, 4).unwrap(), [0; 4], "untouched pages read zeros");
    assert_eq!(p.mem.read(old + 64 * MB - 4096, 4).unwrap(), [0; 4], "DONTUNMAP: the old range reads zeros");
    let after = p.mem.space().stats().committed;
    assert!(
        after <= before + 2 * MB as usize,
        "moving a 64 MiB range holding one page committed {} MiB more",
        (after - before) >> 20
    );
}

#[test]
fn a_fixed_move_commits_only_what_it_held() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let old = p.syscall(&mut t, nr::MMAP, [0, 64 * MB, 3, 0x22, u64::MAX, 0]) as u64;
    let target = p.syscall(&mut t, nr::MMAP, [0, 64 * MB, 0, 0x22, u64::MAX, 0]) as u64;
    p.mem.write(old + 4096, b"held").unwrap();
    let before = p.mem.space().stats().committed;
    // MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP onto a reserved range, as a moving GC does.
    let new = p.syscall(&mut t, nr::MREMAP, [old, 64 * MB, 64 * MB, 1 | 2 | 4, target, 0]) as i64;
    assert_eq!(new, target as i64);
    assert_eq!(p.mem.read(target + 4096, 4).unwrap(), b"held");
    let after = p.mem.space().stats().committed;
    assert!(after <= before + 2 * MB as usize, "committed {} MiB more", (after - before) >> 20);
}
