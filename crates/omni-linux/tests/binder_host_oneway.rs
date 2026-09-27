//! The host calls a guest's binder oneway -- as a HAL calls back its client (IComposerCallback's
//! onHotplug and onVsync): the guest receives a one-way transaction, and the host does not wait.
//! One test per file: the broker is per host process.
use std::sync::Arc;
use std::time::Duration;

use omni_linux::binder::{broker, Context, HostCall, HostReply};
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const BINDER_WRITE_READ: u64 = 0xc030_6201;
const BINDER_SET_CONTEXT_MGR: u64 = 0x4004_6207;
const BC_TRANSACTION: u32 = 0x4040_6300;
const BC_REPLY: u32 = 0x4040_6301;
const BC_FREE_BUFFER: u32 = 0x4008_6303;
const BR_TRANSACTION: u32 = 0x8040_7202;
const BR_REPLY: u32 = 0x8040_7203;
const TYPE_BINDER: u32 = 0x7362_2a85;

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
        let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]);
        assert!((fd as i64) >= 0, "open /dev/binder: {}", fd as i64);
        let area = p.syscall(&mut t, nr::MMAP, [0, 1 << 18, 1, 2, fd, 0]);
        assert!((area as i64) > 0, "mmap: {}", area as i64);
        Self { p, t, fd, s }
    }

    fn sys(&mut self, n: u64, a: [u64; 6]) -> i64 {
        self.p.syscall(&mut self.t, n, a) as i64
    }

    fn ioctl(&mut self, cmd: u64, arg: u64) -> i64 {
        let fd = self.fd;
        self.sys(nr::IOCTL, [fd, cmd, arg, 0, 0, 0])
    }

    /// Read until a transaction or reply comes: (reply, code, data, offsets, buffer).
    fn next_txn(&mut self) -> (bool, u32, Vec<u8>, Vec<u64>, u64) {
        loop {
            let (bwr, rbuf) = (self.s + 0x100, self.s + 0x4000);
            let mut b = Vec::new();
            for v in [0u64, 0, 0, 0x1000, 0, rbuf] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            self.p.mem.write(bwr, &b).unwrap();
            assert_eq!(self.ioctl(BINDER_WRITE_READ, bwr), 0);
            let got = self.p.mem.read_u64(bwr + 32).unwrap() as usize;
            let out = self.p.mem.read(rbuf, got).unwrap();
            let mut at = 0;
            while at + 4 <= out.len() {
                let cmd = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
                let size = ((cmd >> 16) & 0x3fff) as usize;
                let arg = &out[at + 4..at + 4 + size];
                if cmd == BR_TRANSACTION || cmd == BR_REPLY {
                    let code = u32::from_le_bytes(arg[16..20].try_into().unwrap());
                    let len = u64::from_le_bytes(arg[32..40].try_into().unwrap()) as usize;
                    let olen = u64::from_le_bytes(arg[40..48].try_into().unwrap()) as usize;
                    let buffer = u64::from_le_bytes(arg[48..56].try_into().unwrap());
                    let optr = u64::from_le_bytes(arg[56..64].try_into().unwrap());
                    let data = self.p.mem.read(buffer, len).unwrap();
                    let offsets = self.p.mem.read(optr, olen).unwrap().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
                    return (cmd == BR_REPLY, code, data, offsets, buffer);
                }
                at += 4 + size;
            }
        }
    }

    /// `BC_TRANSACTION`/`BC_REPLY` of `data` with objects at `offsets`, after freeing `free`.
    fn send(&mut self, cmd: u32, handle: u32, code: u32, data: &[u8], offsets: &[u64], free: Option<u64>) {
        let (dbuf, obuf) = (self.s + 0x8000, self.s + 0x8800);
        self.p.mem.write(dbuf, data).unwrap();
        let o: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        self.p.mem.write(obuf, &o).unwrap();
        let mut cmds = Vec::new();
        if let Some(buffer) = free {
            cmds.extend_from_slice(&BC_FREE_BUFFER.to_le_bytes());
            cmds.extend_from_slice(&buffer.to_le_bytes());
        }
        cmds.extend_from_slice(&cmd.to_le_bytes());
        cmds.extend_from_slice(&u64::from(handle).to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes());
        cmds.extend_from_slice(&code.to_le_bytes());
        cmds.extend_from_slice(&0u32.to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes());
        cmds.extend_from_slice(&(data.len() as u64).to_le_bytes());
        cmds.extend_from_slice(&(o.len() as u64).to_le_bytes());
        cmds.extend_from_slice(&dbuf.to_le_bytes());
        cmds.extend_from_slice(&obuf.to_le_bytes());
        let (bwr, wbuf) = (self.s + 0x100, self.s + 0x1000);
        self.p.mem.write(wbuf, &cmds).unwrap();
        let mut b = Vec::new();
        for v in [cmds.len() as u64, 0, wbuf, 0, 0, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.p.mem.write(bwr, &b).unwrap();
        assert_eq!(self.ioctl(BINDER_WRITE_READ, bwr), 0);
    }
}

fn object(kind: u32, value: u64, cookie: u64) -> [u8; 24] {
    let mut o = [0u8; 24];
    o[0..4].copy_from_slice(&kind.to_le_bytes());
    o[8..16].copy_from_slice(&value.to_le_bytes());
    o[16..24].copy_from_slice(&cookie.to_le_bytes());
    o
}

#[test]
fn the_host_calls_a_guest_binder_oneway() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut g = Guest::new();
        assert_eq!(g.ioctl(BINDER_SET_CONTEXT_MGR, g.s), 0);
        // A host service the guest hands its own binder to, as a client registers a callback.
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let service = broker(Context::Binder).create_host_service_objects(move |call: HostCall| {
            handle_tx.send(call.handles.clone()).unwrap();
            HostReply::bytes(Vec::new())
        });
        let host = std::thread::spawn(move || {
            let parcel = object(TYPE_BINDER, service, service).to_vec();
            broker(Context::Binder).host_transact(0, 1, parcel, &[0])
        });
        let (_, _, data, _, buffer) = g.next_txn();
        let handle = u32::from_le_bytes(data[8..12].try_into().unwrap());
        g.send(BC_REPLY, 0, 0, &[], &[], Some(buffer));
        assert_eq!(host.join().unwrap(), Ok(Vec::new()));
        g.send(BC_TRANSACTION, handle, 2, &object(TYPE_BINDER, 0xcb, 0xcc), &[0], None);
        let handles = handle_rx.recv_timeout(Duration::from_secs(10)).expect("the registration");
        let (reply, _, _, _, _) = g.next_txn();
        assert!(reply, "the registration's reply");
        let callback = handles[0];

        // The host calls back, oneway, and does not wait for the guest.
        let started = std::time::Instant::now();
        broker(Context::Binder).host_transact_oneway(callback, 7, b"hotplug".to_vec(), &[]).expect("oneway");
        assert!(started.elapsed() < Duration::from_secs(1), "a oneway call does not wait");
        let (reply, code, data, _, _) = g.next_txn();
        tx.send((reply, code, data)).unwrap();
    });
    let (reply, code, data) = rx.recv_timeout(Duration::from_secs(20)).expect("the guest never got the host's call");
    assert!(!reply && code == 7, "a transaction, code 7");
    assert_eq!(String::from_utf8_lossy(&data), "hotplug");
}
