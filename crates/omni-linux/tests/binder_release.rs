//! A process's references die with it: an object only a dead process referred to is let go by its
//! owner (`BR_RELEASE`, `BR_DECREFS`), as the kernel drops a dead process's refs. SurfaceFlinger
//! removes a client's layers when their handles are released so; without it the boot animation's
//! layer outlived its process and covered every app. One test per file: the broker is per host
//! process.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const BINDER_WRITE_READ: u64 = 0xc030_6201;
const BINDER_SET_CONTEXT_MGR: u64 = 0x4004_6207;
const BC_TRANSACTION: u32 = 0x4040_6300;
const BR_INCREFS: u32 = 0x8010_7207;
const BR_ACQUIRE: u32 = 0x8010_7208;
const BR_RELEASE: u32 = 0x8010_7209;
const BR_DECREFS: u32 = 0x8010_720a;
const BINDER_TYPE_BINDER: u32 = 0x7362_2a85;
const TF_ONE_WAY: u32 = 1;
const O_RDWR_NONBLOCK: u64 = 2 | 0o4000;

struct Guest {
    p: Arc<Process>,
    t: omni_linux::Task,
    fd: u64,
    s: u64,
}

impl Guest {
    fn new() -> Self {
        let m = manifest::parse("d\t755\t/\n").unwrap();
        let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
        let p = Process::for_tests(vfs, Output::Capture(Default::default()));
        let mut t = p.test_task();
        let s = p.scratch();
        p.mem.write(s, b"/dev/binder\0").unwrap();
        let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, O_RDWR_NONBLOCK, 0, 0, 0]);
        assert!((fd as i64) >= 0, "open /dev/binder: {}", fd as i64);
        let area = p.syscall(&mut t, nr::MMAP, [0, 1 << 18, 1, 2, fd, 0]);
        assert!((area as i64) > 0, "mmap: {}", area as i64);
        Self { p, t, fd, s }
    }

    /// Write `cmds`, then read what there is: the returned commands' codes.
    fn write_read(&mut self, cmds: &[u8]) -> Vec<u32> {
        let (bwr, wbuf, rbuf) = (self.s + 0x100, self.s + 0x1000, self.s + 0x4000);
        self.p.mem.write(wbuf, cmds).unwrap();
        let mut b = Vec::new();
        for v in [cmds.len() as u64, 0, wbuf, 0x1000, 0, rbuf] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.p.mem.write(bwr, &b).unwrap();
        let fd = self.fd;
        let r = self.p.syscall(&mut self.t, nr::IOCTL, [fd, BINDER_WRITE_READ, bwr, 0, 0, 0]) as i64;
        if r != 0 {
            return Vec::new();
        }
        let got = self.p.mem.read_u64(bwr + 32).unwrap() as usize;
        let bytes = self.p.mem.read(rbuf, got).unwrap();
        let mut codes = Vec::new();
        let mut at = 0;
        while at + 4 <= bytes.len() {
            let code = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            codes.push(code);
            at += 4 + ((code >> 16) & 0x3fff) as usize;
        }
        codes
    }

    /// Everything there is to read now, over as many reads as it takes.
    fn drain(&mut self) -> Vec<u32> {
        let mut all = Vec::new();
        for _ in 0..8 {
            let got = self.write_read(&[]);
            if got.iter().all(|c| *c == 0x720c) {
                break;
            }
            all.extend(got);
        }
        all
    }
}

#[test]
fn an_object_only_a_dead_process_held_is_released_by_its_owner() {
    let mut manager = Guest::new();
    let fd = manager.fd;
    assert_eq!(manager.p.syscall(&mut manager.t, nr::IOCTL, [fd, BINDER_SET_CONTEXT_MGR, 0, 0, 0, 0]), 0);
    let mut owner = Guest::new();

    // The owner sends the manager one of its objects (a oneway call carrying a flat binder).
    let (data, offsets) = (owner.s + 0x2000, owner.s + 0x2100);
    let mut obj = Vec::new();
    obj.extend_from_slice(&BINDER_TYPE_BINDER.to_le_bytes());
    obj.extend_from_slice(&0u32.to_le_bytes());
    obj.extend_from_slice(&0x5000u64.to_le_bytes()); // binder (its weak refs)
    obj.extend_from_slice(&0x6000u64.to_le_bytes()); // cookie (the object)
    owner.p.mem.write(data, &obj).unwrap();
    owner.p.mem.write(offsets, &0u64.to_le_bytes()).unwrap();
    let mut cmd = BC_TRANSACTION.to_le_bytes().to_vec();
    for v in [0u64, 0] {
        cmd.extend_from_slice(&v.to_le_bytes()); // target handle 0, cookie
    }
    cmd.extend_from_slice(&1u32.to_le_bytes()); // code
    cmd.extend_from_slice(&TF_ONE_WAY.to_le_bytes());
    cmd.extend_from_slice(&[0u8; 8]); // sender pid, euid
    for v in [obj.len() as u64, 8, data, offsets] {
        cmd.extend_from_slice(&v.to_le_bytes());
    }
    let mut first = owner.write_read(&cmd);
    first.extend(owner.drain());
    assert!(first.contains(&BR_INCREFS) && first.contains(&BR_ACQUIRE), "the owner holds the object for the manager: {first:x?}");

    // The manager dies (its descriptor on the driver closed, as its exit closes it): the owner
    // hears that the object is no longer referred to.
    let fd = manager.fd;
    assert_eq!(manager.p.syscall(&mut manager.t, nr::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);
    drop(manager);
    let later = owner.drain();
    assert!(later.contains(&BR_RELEASE) && later.contains(&BR_DECREFS), "released when its only holder died: {later:x?}");
}
