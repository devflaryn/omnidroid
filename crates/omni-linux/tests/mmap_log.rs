//! `OMNI_MMAP_LOG_MB`: with it set, a big `mmap`, its naming, `mremap` and `munmap` are logged and
//! answered exactly as without it (the log reads the caller's stack, which must never fail a call).
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};

#[test]
fn big_maps_are_logged_and_answered_as_ever() {
    // Before anything reads it (one test in this binary).
    std::env::set_var("OMNI_MMAP_LOG_MB", "2");
    assert!(omni_linux::mmap_log::wanted(2 << 20));
    assert!(!omni_linux::mmap_log::wanted(1 << 20));
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = Task::new(5001, Arc::clone(&p));
    // A frame pointer that points nowhere mapped: the walk stops, the call is unaffected.
    t.fp = 0x10;
    let len = 4u64 << 20;
    let at = p.syscall(&mut t, nr::MMAP, [0, len, 3, 0x22, u64::MAX, 0]);
    assert!((at as i64) > 0, "{}", at as i64);
    let name = p.scratch();
    p.mem.write(name, b"test-arena\0").unwrap();
    assert_eq!(p.syscall(&mut t, nr::PRCTL, [0x5356_4d41, 0, at, len, name, 0]), 0);
    assert_eq!(p.mm.name_at(at).map(|(n, _)| n), Some(b"[anon:test-arena]".to_vec()));
    let moved = p.syscall(&mut t, nr::MREMAP, [at, len, 2 << 20, 0, 0, 0]);
    assert_eq!(moved, at, "a shrink in place");
    assert_eq!(p.syscall(&mut t, nr::MUNMAP, [at, 2 << 20, 0, 0, 0, 0]), 0);
}
