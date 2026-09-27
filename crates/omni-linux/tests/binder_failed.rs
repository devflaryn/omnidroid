//! A transaction the driver cannot make is answered as the kernel answers it: the command is
//! consumed, the ioctl succeeds, and the sender reads BR_FAILED_REPLY. An ioctl error instead left
//! libbinder's out-buffer unconsumed ("mOut.dataSize() > 0 after flushCommands()"), so every later
//! call of that thread failed with it: an app's `unbindService` threw IllegalArgumentException and
//! killed its main thread (Roblox on the real-AOSP path, 2026-09-28). One test per file: the
//! broker is per host process.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const BINDER_WRITE_READ: u64 = 0xc030_6201;
const BINDER_SET_CONTEXT_MGR: u64 = 0x4004_6207;
const BC_TRANSACTION: u32 = 0x4040_6300;
const BR_FAILED_REPLY: u32 = 0x7211;
const BR_TRANSACTION_COMPLETE: u32 = 0x7206;
const BR_NOOP: u32 = 0x720c;
const BINDER_TYPE_FD: u32 = 0x6664_2a85;
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

    /// Write `cmds` and read what there is: the ioctl's result, the bytes of `cmds` consumed, and
    /// the codes read.
    fn write_read(&mut self, cmds: &[u8]) -> (i64, u64, Vec<u32>) {
        let (bwr, wbuf, rbuf) = (self.s + 0x100, self.s + 0x1000, self.s + 0x4000);
        self.p.mem.write(wbuf, cmds).unwrap();
        let mut b = Vec::new();
        for v in [cmds.len() as u64, 0, wbuf, 0x1000, 0, rbuf] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.p.mem.write(bwr, &b).unwrap();
        let fd = self.fd;
        let r = self.p.syscall(&mut self.t, nr::IOCTL, [fd, BINDER_WRITE_READ, bwr, 0, 0, 0]) as i64;
        let consumed = self.p.mem.read_u64(bwr + 8).unwrap();
        let got = self.p.mem.read_u64(bwr + 32).unwrap() as usize;
        let bytes = self.p.mem.read(rbuf, got).unwrap();
        let mut codes = Vec::new();
        let mut at = 0;
        while at + 4 <= bytes.len() {
            let code = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            if code != BR_NOOP {
                codes.push(code);
            }
            at += 4 + ((code >> 16) & 0x3fff) as usize;
        }
        (r, consumed, codes)
    }
}

/// BC_TRANSACTION to handle 0 carrying one flat object `obj` in its data.
fn transaction(g: &Guest, flags: u32, obj: &[u8]) -> Vec<u8> {
    let (data, offsets) = (g.s + 0x800, g.s + 0x900);
    g.p.mem.write(data, obj).unwrap();
    g.p.mem.write(offsets, &0u64.to_le_bytes()).unwrap();
    let mut tr = vec![0u8; 64];
    tr[16..20].copy_from_slice(&1u32.to_le_bytes()); // code
    tr[20..24].copy_from_slice(&flags.to_le_bytes());
    tr[32..40].copy_from_slice(&(obj.len() as u64).to_le_bytes());
    tr[40..48].copy_from_slice(&8u64.to_le_bytes());
    tr[48..56].copy_from_slice(&data.to_le_bytes());
    tr[56..64].copy_from_slice(&offsets.to_le_bytes());
    let mut cmd = BC_TRANSACTION.to_le_bytes().to_vec();
    cmd.extend_from_slice(&tr);
    cmd
}

fn fd_object(fd: u32) -> Vec<u8> {
    let mut obj = vec![0u8; 24];
    obj[0..4].copy_from_slice(&BINDER_TYPE_FD.to_le_bytes());
    obj[8..12].copy_from_slice(&fd.to_le_bytes());
    obj
}

#[test]
fn a_transaction_that_cannot_be_made_is_consumed_and_read_as_failed_reply() {
    let mut manager = Guest::new();
    let (m_fd, m_s) = (manager.fd, manager.s);
    assert_eq!(manager.p.syscall(&mut manager.t, nr::IOCTL, [m_fd, BINDER_SET_CONTEXT_MGR, 0, 0, 0, 0]), 0);
    let _ = m_s;

    let mut client = Guest::new();
    // A descriptor the client does not have.
    let bad = transaction(&client, TF_ONE_WAY, &fd_object(999));
    let (r, consumed, codes) = client.write_read(&bad);
    assert_eq!(r, 0, "the kernel's ioctl succeeds; the failure is the thread's to read");
    assert_eq!(consumed, bad.len() as u64, "the failed command is consumed");
    assert_eq!(codes, vec![BR_FAILED_REPLY], "the sender reads BR_FAILED_REPLY");

    // The thread's connection still works: a good oneway call completes.
    let good = transaction(&client, TF_ONE_WAY, &[0u8; 24]);
    let mut good_no_objects = good.clone();
    good_no_objects[4 + 40..4 + 48].copy_from_slice(&0u64.to_le_bytes()); // no offsets
    let (r, consumed, codes) = client.write_read(&good_no_objects);
    assert_eq!(r, 0);
    assert_eq!(consumed, good_no_objects.len() as u64);
    assert_eq!(codes, vec![BR_TRANSACTION_COMPLETE]);
}
