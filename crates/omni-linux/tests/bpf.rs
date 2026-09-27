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
