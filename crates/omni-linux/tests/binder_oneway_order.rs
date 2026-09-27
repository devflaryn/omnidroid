//! Oneway transactions to one node are handed out one at a time, as the binder driver orders them:
//! the next waits until the receiver frees the buffer of the one before. ActivityManager sends an
//! app its Activity's launch and then its top-resumed change, both oneway; an app whose two binder
//! threads ran them at once lost the launch to the change. One test per file: the broker is per
//! host process.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const BINDER_WRITE_READ: u64 = 0xc030_6201;
const BINDER_SET_CONTEXT_MGR: u64 = 0x4004_6207;
const BC_TRANSACTION: u32 = 0x4040_6300;
const BC_FREE_BUFFER: u32 = 0x4008_6303;
const BR_TRANSACTION: u32 = 0x8040_7202;
const TF_ONE_WAY: u32 = 1;
const O_RDWR_NONBLOCK: u64 = 2 | 0o4000;

struct Guest {
    p: Arc<Process>,
    t: omni_linux::Task,
    fd: u64,
    s: u64,
}

impl Guest {
    fn new(flags: u64) -> Self {
        let m = manifest::parse("d\t755\t/\n").unwrap();
        let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
        let p = Process::for_tests(vfs, Output::Capture(Default::default()));
        let mut t = p.test_task();
        let s = p.scratch();
        p.mem.write(s, b"/dev/binder\0").unwrap();
        let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, flags, 0, 0, 0]);
        assert!((fd as i64) >= 0, "open /dev/binder: {}", fd as i64);
        let area = p.syscall(&mut t, nr::MMAP, [0, 1 << 18, 1, 2, fd, 0]);
        assert!((area as i64) > 0, "mmap: {}", area as i64);
        // A thread of the binder thread pool (BC_ENTER_LOOPER): only a looper takes its process's work.
        let (bwr, wbuf) = (s + 0x100, s + 0x1000);
        p.mem.write(wbuf, &0x630cu32.to_le_bytes()).unwrap();
        let b: Vec<u8> = [4u64, 0, wbuf, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        p.mem.write(bwr, &b).unwrap();
        assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd, BINDER_WRITE_READ, bwr, 0, 0, 0]), 0);
        Self { p, t, fd, s }
    }

    fn write_read(&mut self, cmds: &[u8], read: bool) -> Vec<u8> {
        let (bwr, wbuf, rbuf) = (self.s + 0x100, self.s + 0x1000, self.s + 0x4000);
        self.p.mem.write(wbuf, cmds).unwrap();
        let mut b = Vec::new();
        for v in [cmds.len() as u64, 0, wbuf, if read { 0x1000 } else { 0 }, 0, rbuf] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.p.mem.write(bwr, &b).unwrap();
        let fd = self.fd;
        let r = self.p.syscall(&mut self.t, nr::IOCTL, [fd, BINDER_WRITE_READ, bwr, 0, 0, 0]) as i64;
        if r != 0 {
            return Vec::new(); // EAGAIN: nothing for this thread
        }
        let got = self.p.mem.read_u64(bwr + 32).unwrap() as usize;
        self.p.mem.read(rbuf, got).unwrap()
    }

    /// The transactions a read hands out now: (code, buffer).
    fn transactions(&mut self, cmds: &[u8]) -> Vec<(u32, u64)> {
        let out = self.write_read(cmds, true);
        let mut found = Vec::new();
        let mut at = 0;
        while at + 4 <= out.len() {
            let cmd = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
            let size = ((cmd >> 16) & 0x3fff) as usize;
            if cmd == BR_TRANSACTION {
                let arg = &out[at + 4..at + 4 + size];
                found.push((u32::from_le_bytes(arg[16..20].try_into().unwrap()), u64::from_le_bytes(arg[48..56].try_into().unwrap())));
            }
            at += 4 + size;
        }
        found
    }

    fn oneway(&mut self, handle: u32, code: u32) {
        let dbuf = self.s + 0x8000;
        self.p.mem.write(dbuf, b"x").unwrap();
        let mut cmds = Vec::new();
        cmds.extend_from_slice(&BC_TRANSACTION.to_le_bytes());
        cmds.extend_from_slice(&u64::from(handle).to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes());
        cmds.extend_from_slice(&code.to_le_bytes());
        cmds.extend_from_slice(&TF_ONE_WAY.to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes());
        cmds.extend_from_slice(&1u64.to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes());
        cmds.extend_from_slice(&dbuf.to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes());
        self.write_read(&cmds, false);
    }
}

#[test]
fn a_nodes_oneway_transactions_are_handed_out_one_at_a_time() {
    let mut receiver = Guest::new(O_RDWR_NONBLOCK);
    let fd = receiver.fd;
    let s = receiver.s;
    assert_eq!(receiver.p.syscall(&mut receiver.t, nr::IOCTL, [fd, BINDER_SET_CONTEXT_MGR, s, 0, 0, 0]), 0);
    let mut sender = Guest::new(2);
    sender.oneway(0, 1);
    sender.oneway(0, 2);

    let first = receiver.transactions(&[]);
    assert_eq!(first.iter().map(|t| t.0).collect::<Vec<_>>(), [1], "the first oneway alone");
    assert!(receiver.transactions(&[]).is_empty(), "the second waits while the first's buffer is held");
    let mut free = BC_FREE_BUFFER.to_le_bytes().to_vec();
    free.extend_from_slice(&first[0].1.to_le_bytes());
    let second = receiver.transactions(&free);
    assert_eq!(second.iter().map(|t| t.0).collect::<Vec<_>>(), [2], "the second once the first is freed");
}
