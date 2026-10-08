//! `madvise(MADV_FREE)` under the `madv_free` lever, and the zero-page sweep (`zero_reclaim`),
//! through the guest's own system calls.
//!
//! Linux's contract for `MADV_FREE` on private anonymous memory: until the range is written again
//! it reads either its old contents or zeros, and a write after the call is kept. With the lever
//! off it is a hint (the old contents); on, it is carried out at once (zeros) -- both inside the
//! contract. The sweep must change no byte the guest reads.
use std::sync::atomic::Ordering;
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};

const MADV_FREE: u64 = 8;

fn process() -> Arc<Process> {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    Process::for_tests(vfs, Output::Capture(Default::default()))
}

#[test]
fn madv_free_reads_old_contents_or_zeros_and_keeps_a_later_write() {
    let p = process();
    let mut t = Task::new(3001, Arc::clone(&p));
    let page = p.mm.page_size();
    let len = 4 * page;
    let at = p.syscall(&mut t, nr::MMAP, [0, len, 3, 0x22, u64::MAX, 0]);
    assert!((at as i64) > 0);
    p.mem.write(at, &vec![0xA5; len as usize]).unwrap();

    // The lever off (the default): a hint, the contents stay.
    omni_linux::mm::MADV_FREE_DISCARDS.store(false, Ordering::Relaxed);
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at, len, MADV_FREE, 0, 0, 0]), 0);
    assert!(p.mem.read(at, len as usize).unwrap().iter().all(|&b| b == 0xA5), "a hint changes nothing");

    // On: carried out at once, as MADV_DONTNEED -- the range reads zeros, and only the range.
    omni_linux::lever::apply("madv_free=1").expect("the lever");
    assert_eq!(p.syscall(&mut t, nr::MADVISE, [at + page, 2 * page, MADV_FREE, 0, 0, 0]), 0);
    let now = p.mem.read(at, len as usize).unwrap();
    let page = page as usize;
    assert!(now[..page].iter().all(|&b| b == 0xA5), "before the range");
    assert!(now[page..3 * page].iter().all(|&b| b == 0), "the range reads zeros");
    assert!(now[3 * page..].iter().all(|&b| b == 0xA5), "after the range");
    // A write after the call is kept.
    p.mem.write(at + page as u64 + 5, &[7]).unwrap();
    assert_eq!(p.mem.read(at + page as u64 + 5, 1).unwrap(), vec![7]);
    omni_linux::lever::apply("madv_free=0").expect("the lever");
}

/// A sweep takes the guest's zero pages out of the working set (on Windows) and changes no byte.
#[test]
fn the_zero_sweep_changes_nothing_the_guest_reads() {
    let p = process();
    let mut t = Task::new(3002, Arc::clone(&p));
    let page = p.mm.page_size() as usize;
    let len = 32 * page;
    let at = p.syscall(&mut t, nr::MMAP, [0, len as u64, 3, 0x22, u64::MAX, 0]);
    assert!((at as i64) > 0);
    // Every page touched: zeros (a memset), but every fourth page holds a byte.
    p.mem.write(at, &vec![0; len]).unwrap();
    for i in (0..32).step_by(4) {
        p.mem.write(at + (i * page) as u64 + 9, &[i as u8 + 1]).unwrap();
    }
    let (found, spaces) = omni_linux::zero_reclaim::sweep();
    assert!(spaces >= 1);
    if cfg!(windows) {
        assert!(found.reset >= 24, "the 24 zero pages of this mapping at least: {found:?}");
    }
    let now = p.mem.read(at, len).unwrap();
    for i in 0..32 {
        let want = if i % 4 == 0 { i as u8 + 1 } else { 0 };
        assert_eq!(now[i * page + 9], want, "page {i}");
        assert!(now[i * page..(i + 1) * page].iter().enumerate().all(|(j, &b)| j == 9 || b == 0), "page {i}");
    }
}

/// The kernel's reads of memory the guest never touched read zeros and **commit nothing**
/// (`read_no_commit`, on by default); the touched part reads as written.
#[test]
fn a_kernel_read_of_untouched_memory_commits_nothing() {
    let p = process();
    let mut t = Task::new(3003, Arc::clone(&p));
    let len = 1usize << 20;
    let at = p.syscall(&mut t, nr::MMAP, [0, len as u64, 3, 0x22, u64::MAX, 0]);
    assert!((at as i64) > 0);
    let committed = |p: &Process| {
        let space = p.mem.space();
        let mut sum = 0;
        let mut a = at as usize;
        while a < at as usize + len {
            let r = space.region_at(a).expect("mapped");
            sum += r.committed;
            a = r.start + r.len;
        }
        sum
    };
    assert_eq!(committed(&p), 0);
    // A byte in the middle: one granule committed by the write.
    p.mem.write(at + (len as u64) / 2 + 3, &[0x5C]).unwrap();
    let after_write = committed(&p);
    assert!(after_write > 0 && after_write < len, "{after_write}");
    let all = p.mem.read(at, len).unwrap();
    assert_eq!(all.iter().filter(|&&b| b != 0).count(), 1);
    assert_eq!(all[len / 2 + 3], 0x5C);
    let mut word = [0u8; 8];
    p.mem.read_into(at + 4096, &mut word).unwrap();
    assert_eq!(word, [0; 8]);
    assert_eq!(p.mem.read_cstr(at + 8192, 64).unwrap(), Vec::<u8>::new());
    assert_eq!(committed(&p), after_write, "the reads committed nothing");

    // The lever off: the read commits, as before.
    omni_linux::lever::apply("read_no_commit=0").expect("the lever");
    assert_eq!(p.mem.read(at, len).unwrap().iter().filter(|&&b| b != 0).count(), 1);
    assert_eq!(committed(&p), len, "committed whole by the read");
    omni_linux::lever::apply("read_no_commit=1").expect("the lever");
}
