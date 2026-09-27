//! eBPF (`crate::bpf`): maps hold what is written to them, by `bpf(2)` and through a pin on the
//! BPF filesystem (the image's loaders: tests/bpf_loaders.rs).
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::vfs::{Sysroot, Vfs};
use omni_linux::manifest;

fn call(p: &Process, t: &mut omni_linux::Task, cmd: u64, attr: &[u8], s: u64) -> i64 {
    p.mem.write(s, attr).unwrap();
    p.syscall(t, nr::BPF, [cmd, s, attr.len() as u64, 0, 0, 0]) as i64
}

fn words(w: &[u64]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn a_map_holds_what_is_written_and_is_found_again_by_its_pin() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Arc::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    // BPF_MAP_CREATE: a hash map, u32 -> u64, 2 entries, named "counts".
    let mut create = Vec::new();
    for v in [1u32, 4, 8, 2, 0, 0, 0] {
        create.extend_from_slice(&v.to_le_bytes());
    }
    create.extend_from_slice(b"counts\0\0\0\0\0\0\0\0\0\0");
    let fd = call(&p, &mut t, 0, &create, s);
    assert!(fd >= 0, "{fd}");
    let (key, value) = (s + 512, s + 520);
    let elem = |v: u64| words(&[fd as u64, key, value, v]);
    p.mem.write_u32(key, 7).unwrap();
    p.mem.write_u64(value, 42).unwrap();
    assert_eq!(call(&p, &mut t, 2, &elem(0), s), 0, "update");
    assert_eq!(call(&p, &mut t, 2, &elem(1), s), -17, "BPF_NOEXIST over a key: EEXIST");
    p.mem.write_u64(value, 0).unwrap();
    assert_eq!(call(&p, &mut t, 1, &elem(0), s), 0, "lookup");
    assert_eq!(p.mem.read_u64(value).unwrap(), 42);
    p.mem.write_u32(key, 8).unwrap();
    assert_eq!(call(&p, &mut t, 2, &elem(0), s), 0);
    p.mem.write_u32(key, 9).unwrap();
    assert_eq!(call(&p, &mut t, 2, &elem(0), s), -7, "full: E2BIG");
    // GET_NEXT_KEY from no key: 7, then 8, then the end.
    let next = |from: u64| words(&[fd as u64, from, s + 600, 0]);
    assert_eq!(call(&p, &mut t, 4, &next(0), s), 0);
    assert_eq!(p.mem.read_u32(s + 600).unwrap(), 7);
    p.mem.write_u32(key, 7).unwrap();
    assert_eq!(call(&p, &mut t, 4, &next(key), s), 0);
    assert_eq!(p.mem.read_u32(s + 600).unwrap(), 8);
    p.mem.write_u32(key, 8).unwrap();
    assert_eq!(call(&p, &mut t, 4, &next(key), s), -2, "the end: ENOENT");
    // Pinned, and found again by the path.
    p.mem.write(s + 700, b"/sys/fs/bpf/test_counts\0").unwrap();
    assert_eq!(call(&p, &mut t, 6, &words(&[s + 700, fd as u64]), s), 0, "BPF_OBJ_PIN");
    let again = call(&p, &mut t, 7, &words(&[s + 700, 0]), s);
    assert!(again >= 0);
    p.mem.write_u32(key, 7).unwrap();
    assert_eq!(call(&p, &mut t, 1, &words(&[again as u64, key, value, 0]), s), 0);
    assert_eq!(p.mem.read_u64(value).unwrap(), 42, "the same map");
    // BPF_OBJ_GET_INFO_BY_FD: the definition, and the length written back.
    assert_eq!(call(&p, &mut t, 15, &words(&[(again as u64) | (88 << 32), s + 800]), s), 0);
    let info = p.mem.read(s + 800, 24).unwrap();
    assert_eq!(&info[0..4], &1u32.to_le_bytes(), "hash");
    assert_eq!(&info[8..20], &words(&[4 | (8 << 32), 2])[..12], "key 4, value 8, 2 entries");
    assert_eq!(p.mem.read_u32(s + 4).unwrap(), 88, "info_len written back");
}

/// A program attached to a cgroup is what `BPF_PROG_QUERY` answers there (netd attaches its
/// cgroup programs and checks them back).
#[test]
fn an_attached_program_is_queried_back() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Arc::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s + 900, b"/dev\0").unwrap();
    let cgroup = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s + 900, 0o200000, 0, 0, 0]);
    assert!((cgroup as i64) >= 0);
    // A program: type CGROUP_SKB (8), one instruction, a license.
    p.mem.write(s + 1000, &[0u8; 8]).unwrap();
    p.mem.write(s + 1100, b"GPL\0").unwrap();
    let prog = call(&p, &mut t, 5, &words(&[8 | (1 << 32), s + 1000, s + 1100]), s);
    assert!(prog >= 0);
    let egress = 1u64; // BPF_CGROUP_INET_EGRESS
    assert_eq!(call(&p, &mut t, 8, &words(&[cgroup | ((prog as u64) << 32), egress]), s), 0, "attach");
    let ids_at = s + 1200;
    let query = words(&[cgroup | (egress << 32), 0, ids_at, 4]);
    assert_eq!(call(&p, &mut t, 16, &query, s), 0);
    assert_eq!(p.mem.read_u32(s + 24).unwrap(), 1, "one program attached");
    assert_eq!(call(&p, &mut t, 15, &words(&[(prog as u64) | (228 << 32), s + 1300]), s), 0);
    assert_eq!(p.mem.read_u32(ids_at).unwrap(), p.mem.read_u32(s + 1300 + 4).unwrap(), "its id");
    assert_eq!(call(&p, &mut t, 9, &words(&[cgroup | ((prog as u64) << 32), egress]), s), 0, "detach");
    assert_eq!(call(&p, &mut t, 16, &query, s), 0);
    assert_eq!(p.mem.read_u32(s + 24).unwrap(), 0);
}

/// Two opens of one pinned map are one file to record locks: an exclusive lock on one refuses the
/// other (netd's map-lock self test).
#[test]
fn a_maps_lock_holds_across_its_opens() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Arc::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    let mut create = Vec::new();
    for v in [2u32, 4, 4, 1, 0, 0, 0] {
        create.extend_from_slice(&v.to_le_bytes());
    }
    create.extend_from_slice(&[0u8; 16]);
    let fd = call(&p, &mut t, 0, &create, s);
    p.mem.write(s + 700, b"/sys/fs/bpf/lock_test\0").unwrap();
    assert_eq!(call(&p, &mut t, 6, &words(&[s + 700, fd as u64]), s), 0);
    let a = call(&p, &mut t, 7, &words(&[s + 700, 0]), s) as u64;
    let b = call(&p, &mut t, 7, &words(&[s + 700, 0]), s) as u64;
    let mut flock = [0u8; 32];
    flock[0..2].copy_from_slice(&1i16.to_le_bytes()); // F_WRLCK
    flock[8..16].copy_from_slice(&5i64.to_le_bytes());
    flock[16..24].copy_from_slice(&1i64.to_le_bytes());
    p.mem.write(s + 800, &flock).unwrap();
    assert_eq!(p.syscall(&mut t, nr::FCNTL, [a, 37, s + 800, 0, 0, 0]), 0, "F_OFD_SETLK on one open");
    assert_eq!(p.syscall(&mut t, nr::FCNTL, [b, 37, s + 800, 0, 0, 0]) as i64, -11, "EAGAIN on the other");
}
