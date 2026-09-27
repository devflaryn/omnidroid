//! A guest process driving `/dev/binder` from the host as libbinder drives it, for the driver's
//! tests: one open, its receive area mapped, and as many threads as a test asks for (each a task
//! of its own; a test drives them one at a time, so they share the scratch buffers).
#![allow(dead_code)]
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};

pub const BINDER_WRITE_READ: u64 = 0xc030_6201;
pub const BINDER_SET_CONTEXT_MGR: u64 = 0x4004_6207;
pub const BINDER_THREAD_EXIT: u64 = 0x4004_6208;
pub const BC_TRANSACTION: u32 = 0x4040_6300;
pub const BC_REPLY: u32 = 0x4040_6301;
pub const BC_FREE_BUFFER: u32 = 0x4008_6303;
pub const BC_ACQUIRE_DONE: u32 = 0x4010_6309;
pub const BC_ENTER_LOOPER: u32 = 0x0000_630c;
pub const BC_EXIT_LOOPER: u32 = 0x0000_630d;
pub const BR_TRANSACTION: u32 = 0x8040_7202;
pub const BR_REPLY: u32 = 0x8040_7203;
pub const BR_DEAD_REPLY: u32 = 0x0000_7205;
pub const BR_TRANSACTION_COMPLETE: u32 = 0x0000_7206;
pub const BR_INCREFS: u32 = 0x8010_7207;
pub const BR_ACQUIRE: u32 = 0x8010_7208;
pub const BR_RELEASE: u32 = 0x8010_7209;
pub const BR_DECREFS: u32 = 0x8010_720a;
pub const BR_NOOP: u32 = 0x0000_720c;
pub const BR_SPAWN_LOOPER: u32 = 0x0000_720d;
pub const BR_FAILED_REPLY: u32 = 0x0000_7211;
pub const TYPE_BINDER: u32 = 0x7362_2a85;
pub const TYPE_HANDLE: u32 = 0x7368_2a85;
pub const TF_ONE_WAY: u32 = 1;
const O_RDWR_NONBLOCK: u64 = 2 | 0o4000;
/// The receive area's size.
pub const AREA: u64 = 1 << 18;

pub struct Guest {
    pub p: Arc<Process>,
    pub fd: u64,
    pub s: u64,
    /// The receive area's address.
    pub area: u64,
    /// The first thread's tid; a test's further threads count up from it.
    pub pid: i32,
    next_tid: i32,
}

/// A return the driver handed out: its command, and for a transaction or reply its fields.
#[derive(Debug, Clone)]
pub struct Br {
    pub cmd: u32,
    pub code: u32,
    pub flags: u32,
    pub data: Vec<u8>,
    /// The buffer in the receive area (what `BC_FREE_BUFFER` frees), or the words of another
    /// return (`BR_RELEASE`: its ptr).
    pub buffer: u64,
}

impl Guest {
    /// A process with `/dev/binder` open (O_NONBLOCK: a read with nothing to hand out is EAGAIN)
    /// and its receive area mapped.
    pub fn new() -> Self {
        let m = manifest::parse("d\t755\t/\n").unwrap();
        let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
        let p = Process::for_tests(vfs, Output::Capture(Default::default()));
        let mut t = p.test_task();
        let s = p.scratch();
        p.mem.write(s, b"/dev/binder\0").unwrap();
        let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, O_RDWR_NONBLOCK, 0, 0, 0]);
        assert!((fd as i64) >= 0, "open /dev/binder: {}", fd as i64);
        let area = p.syscall(&mut t, nr::MMAP, [0, AREA, 1, 2, fd, 0]);
        assert!((area as i64) > 0, "mmap: {}", area as i64);
        Self { p, fd, s, area, pid: t.tid, next_tid: t.tid }
    }

    /// A thread of this process (the first is the process's own tid).
    pub fn thread(&mut self) -> Task {
        let tid = self.next_tid;
        self.next_tid += 1;
        Task::new(tid, Arc::clone(&self.p))
    }

    /// A thread that has entered the looper (`BC_ENTER_LOOPER`), as a binder thread pool's.
    pub fn looper(&mut self) -> Task {
        let mut t = self.thread();
        let (r, _) = self.write_read(&mut t, &BC_ENTER_LOOPER.to_le_bytes(), false);
        assert_eq!(r, 0);
        t
    }

    pub fn set_context_mgr(&self, t: &mut Task) {
        assert_eq!(self.p.syscall(t, nr::IOCTL, [self.fd, BINDER_SET_CONTEXT_MGR, 0, 0, 0, 0]), 0);
    }

    /// Close the process's descriptor on the driver, as its exit does.
    pub fn close(&self, t: &mut Task) {
        assert_eq!(self.p.syscall(t, nr::CLOSE, [self.fd, 0, 0, 0, 0, 0]), 0);
    }

    /// One `BINDER_WRITE_READ` of thread `t`: `cmds` written, then (when `read`) what there is to
    /// read. The ioctl's result, and the returns read (`BR_NOOP` left out).
    pub fn write_read(&self, t: &mut Task, cmds: &[u8], read: bool) -> (i64, Vec<Br>) {
        let (bwr, wbuf, rbuf) = (self.s + 0x100, self.s + 0x1000, self.s + 0x4000);
        self.p.mem.write(wbuf, cmds).unwrap();
        let mut b = Vec::new();
        for v in [cmds.len() as u64, 0, wbuf, if read { 0x1000 } else { 0 }, 0, rbuf] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.p.mem.write(bwr, &b).unwrap();
        let r = self.p.syscall(t, nr::IOCTL, [self.fd, BINDER_WRITE_READ, bwr, 0, 0, 0]) as i64;
        let got = self.p.mem.read_u64(bwr + 32).unwrap() as usize;
        let out = self.p.mem.read(rbuf, got).unwrap();
        let mut found = Vec::new();
        let mut at = 0;
        while at + 4 <= out.len() {
            let cmd = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
            let size = ((cmd >> 16) & 0x3fff) as usize;
            let arg = &out[at + 4..(at + 4 + size).min(out.len())];
            let word = |i: usize| arg.get(i..i + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
            let mut br = Br { cmd, code: 0, flags: 0, data: Vec::new(), buffer: word(0) };
            if matches!(cmd, BR_TRANSACTION | BR_REPLY) {
                br.code = u32::from_le_bytes(arg[16..20].try_into().unwrap());
                br.flags = u32::from_le_bytes(arg[20..24].try_into().unwrap());
                br.buffer = word(48);
                br.data = self.p.mem.read(br.buffer, word(32) as usize).unwrap_or_default();
            }
            if cmd != BR_NOOP {
                found.push(br);
            }
            at += 4 + size;
        }
        (r, found)
    }

    /// What thread `t` reads now, after writing `cmds`.
    pub fn read(&self, t: &mut Task, cmds: &[u8]) -> Vec<Br> {
        self.write_read(t, cmds, true).1
    }

    /// `BC_TRANSACTION` (or `BC_REPLY`) of `data`, its objects at `offsets`, to `handle`.
    pub fn transaction(&self, cmd: u32, handle: u32, code: u32, flags: u32, data: &[u8], offsets: &[u64]) -> Vec<u8> {
        let (dbuf, obuf) = (self.s + 0x8000, self.s + 0x9000);
        self.p.mem.write(dbuf, data).unwrap();
        let obytes: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        self.p.mem.write(obuf, &obytes).unwrap();
        let mut c = cmd.to_le_bytes().to_vec();
        c.extend_from_slice(&u64::from(handle).to_le_bytes());
        c.extend_from_slice(&0u64.to_le_bytes()); // cookie
        c.extend_from_slice(&code.to_le_bytes());
        c.extend_from_slice(&flags.to_le_bytes());
        c.extend_from_slice(&[0u8; 8]); // sender pid, euid
        for v in [data.len() as u64, obytes.len() as u64, dbuf, obuf] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c
    }
}

/// `BC_FREE_BUFFER` of `buffer`.
pub fn free(buffer: u64) -> Vec<u8> {
    let mut c = BC_FREE_BUFFER.to_le_bytes().to_vec();
    c.extend_from_slice(&buffer.to_le_bytes());
    c
}

/// A flat binder object: (type, binder or handle, cookie).
pub fn object(kind: u32, value: u64, cookie: u64) -> [u8; 24] {
    let mut o = [0u8; 24];
    o[0..4].copy_from_slice(&kind.to_le_bytes());
    o[8..16].copy_from_slice(&value.to_le_bytes());
    o[16..24].copy_from_slice(&cookie.to_le_bytes());
    o
}

/// The codes of the transactions among `brs`.
pub fn codes(brs: &[Br]) -> Vec<u32> {
    brs.iter().filter(|b| b.cmd == BR_TRANSACTION).map(|b| b.code).collect()
}

/// The commands of `brs`.
pub fn cmds(brs: &[Br]) -> Vec<u32> {
    brs.iter().map(|b| b.cmd).collect()
}
