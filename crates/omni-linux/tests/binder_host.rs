//! The host binder endpoint at the ioctl level: a guest thread driving `/dev/binder` as libbinder
//! does, and host services on the same broker. One test per file: the broker is per host process.
use std::sync::Arc;
use std::time::Duration;

use omni_linux::binder::{broker, Context};
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
const TYPE_HANDLE: u32 = 0x7368_2a85;

/// A guest process with one thread and `/dev/binder` open and mapped.
struct Guest {
    p: Arc<Process>,
    t: omni_linux::Task,
    fd: u64,
    s: u64,
}

/// A `BR_TRANSACTION`/`BR_REPLY` as the guest read it, or another return.
#[derive(Debug)]
enum Br {
    Txn { reply: bool, code: u32, data: Vec<u8>, buffer: u64 },
    Other,
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

    fn ioctl(&mut self, cmd: u64, arg: u64) -> i64 {
        self.p.syscall(&mut self.t, nr::IOCTL, [self.fd, cmd, arg, 0, 0, 0]) as i64
    }

    /// One `BINDER_WRITE_READ`: `cmds` written, then what the driver returns (waiting for some).
    fn write_read(&mut self, cmds: &[u8]) -> Vec<Br> {
        let (bwr, wbuf, rbuf) = (self.s + 0x100, self.s + 0x1000, self.s + 0x4000);
        self.p.mem.write(wbuf, cmds).unwrap();
        let mut b = Vec::new();
        for v in [cmds.len() as u64, 0, wbuf, 0x1000, 0, rbuf] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.p.mem.write(bwr, &b).unwrap();
        assert_eq!(self.ioctl(BINDER_WRITE_READ, bwr), 0);
        let got = self.p.mem.read_u64(bwr + 32).unwrap() as usize;
        let out = self.p.mem.read(rbuf, got).unwrap();
        let mut brs = Vec::new();
        let mut at = 0;
        while at + 4 <= out.len() {
            let cmd = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
            let size = ((cmd >> 16) & 0x3fff) as usize;
            let arg = &out[at + 4..at + 4 + size];
            if cmd == BR_TRANSACTION || cmd == BR_REPLY {
                let code = u32::from_le_bytes(arg[16..20].try_into().unwrap());
                let len = u64::from_le_bytes(arg[32..40].try_into().unwrap()) as usize;
                let buffer = u64::from_le_bytes(arg[48..56].try_into().unwrap());
                let data = self.p.mem.read(buffer, len).unwrap();
                brs.push(Br::Txn { reply: cmd == BR_REPLY, code, data, buffer });
            } else {
                brs.push(Br::Other);
            }
            at += 4 + size;
        }
        brs
    }

    /// Read until a transaction or reply comes.
    fn next_txn(&mut self) -> (bool, u32, Vec<u8>, u64) {
        loop {
            for br in self.write_read(&[]) {
                if let Br::Txn { reply, code, data, buffer } = br {
                    return (reply, code, data, buffer);
                }
            }
        }
    }

    /// `BC_TRANSACTION`/`BC_REPLY` of `data` (no objects), preceded by freeing `free`.
    fn send(&mut self, cmd: u32, handle: u32, code: u32, data: &[u8], free: Option<u64>) {
        let dbuf = self.s + 0x8000;
        self.p.mem.write(dbuf, data).unwrap();
        let mut cmds = Vec::new();
        if let Some(buffer) = free {
            cmds.extend_from_slice(&BC_FREE_BUFFER.to_le_bytes());
            cmds.extend_from_slice(&buffer.to_le_bytes());
        }
        cmds.extend_from_slice(&cmd.to_le_bytes());
        cmds.extend_from_slice(&u64::from(handle).to_le_bytes()); // target
        cmds.extend_from_slice(&0u64.to_le_bytes()); // cookie
        cmds.extend_from_slice(&code.to_le_bytes());
        cmds.extend_from_slice(&0u32.to_le_bytes()); // flags
        cmds.extend_from_slice(&0u64.to_le_bytes()); // pid, euid
        cmds.extend_from_slice(&(data.len() as u64).to_le_bytes());
        cmds.extend_from_slice(&0u64.to_le_bytes()); // offsets size
        cmds.extend_from_slice(&dbuf.to_le_bytes());
        cmds.extend_from_slice(&(dbuf + 0x800).to_le_bytes());
        // Written alone; what it returns is read by the next read.
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

/// A host service that calls back into its caller's process, synchronously, while serving it: the
/// call-back reaches the very thread that is waiting (as a real kernel routes a nested
/// transaction), so a single-threaded caller serves it and then gets its reply.
#[test]
fn a_host_service_calls_back_into_its_waiting_caller() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut g = Guest::new();
        assert_eq!(g.ioctl(BINDER_SET_CONTEXT_MGR, g.s), 0);

        let binder = broker(Context::Binder);
        let service = binder.create_host_service(|code, _| {
            if code != 7 {
                return Vec::new();
            }
            // Call the context manager: the guest that is calling this.
            let back = broker(Context::Binder).host_transact(0, 8, b"ping".to_vec(), &[]);
            let mut r = b"handled:".to_vec();
            r.extend_from_slice(&back.unwrap_or_else(|e| format!("errno {}", e.0).into_bytes()));
            r
        });

        // The host hands the guest its service (as `addService` would): the guest gets a handle.
        let host = std::thread::spawn(move || {
            let mut parcel = Vec::new();
            parcel.extend_from_slice(&TYPE_BINDER.to_le_bytes());
            parcel.extend_from_slice(&0u32.to_le_bytes());
            parcel.extend_from_slice(&service.to_le_bytes());
            parcel.extend_from_slice(&service.to_le_bytes());
            broker(Context::Binder).host_transact(0, 1, parcel, &[0])
        });
        let (reply, code, data, buffer) = g.next_txn();
        assert!(!reply && code == 1, "the host's transaction: {code}");
        assert_eq!(u32::from_le_bytes(data[0..4].try_into().unwrap()), TYPE_HANDLE);
        let handle = u32::from_le_bytes(data[8..12].try_into().unwrap());
        g.send(BC_REPLY, 0, 0, &[], Some(buffer));
        assert_eq!(host.join().unwrap(), Ok(Vec::new()));

        // The guest calls the service, which calls back.
        g.send(BC_TRANSACTION, handle, 7, b"x", None);
        let (reply, code, data, buffer) = g.next_txn();
        assert!(!reply && code == 8 && data == b"ping", "expected the call-back, got reply={reply} code={code} {:?}", String::from_utf8_lossy(&data));
        g.send(BC_REPLY, 0, 0, b"pong", Some(buffer));
        let (reply, _, data, _) = g.next_txn();
        assert!(reply, "expected the reply");
        tx.send(data).unwrap();
    });
    let data = rx.recv_timeout(Duration::from_secs(15)).expect("the guest's call did not come back: the call-back never reached its waiting thread");
    assert_eq!(String::from_utf8_lossy(&data), "handled:pong");
}
