//! A host binder service receives and sends objects: a guest's file descriptor arrives at the host
//! as the guest's open file, the guest's binder as a handle in the host's table, and a file
//! descriptor in the host's reply arrives in the guest as a new descriptor on the same file (a
//! graphics buffer's memory crossing from the host allocator, D2). One test per file: the broker is
//! per host process.
use std::sync::Arc;
use std::time::Duration;

use omni_linux::binder::{broker, Context, HostCall, HostReply};
use omni_linux::fd::{FileKind, OpenFile, Output};
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
const TYPE_FD: u32 = 0x6664_2a85;

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

/// A shared-memory file holding `bytes`, as the host makes one.
fn shm_file(bytes: &[u8]) -> Arc<OpenFile> {
    let m = omni_linux::shm::Shm::create("host").unwrap();
    m.write_at(bytes, 0).unwrap();
    Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Shared(m)), flags: parking_lot::Mutex::new(2) })
}

fn shm_bytes(f: &OpenFile) -> Vec<u8> {
    match &*f.kind.lock() {
        FileKind::Shared(m) => {
            let mut b = vec![0u8; m.len() as usize];
            let n = m.read_at(&mut b, 0).unwrap();
            b.truncate(n);
            b
        }
        _ => b"(not shared memory)".to_vec(),
    }
}

#[test]
fn a_host_service_receives_an_fd_and_a_binder_and_replies_with_an_fd() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut g = Guest::new();
        assert_eq!(g.ioctl(BINDER_SET_CONTEXT_MGR, g.s), 0);

        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let service = broker(Context::Binder).create_host_service_objects(move |call: HostCall| {
            let fds: Vec<Vec<u8>> = call.fds.iter().map(|f| shm_bytes(f)).collect();
            seen_tx.send((call.code, fds, call.handles.clone())).unwrap();
            let mut data = b"ok:".to_vec();
            data.resize(8, 0);
            let at = data.len();
            data.extend_from_slice(&object(TYPE_FD, u64::MAX, 1));
            HostReply { data, fds: vec![(at, shm_file(b"world"))], binders: vec![] }
        });

        // The host hands the guest its service: the guest gets a handle.
        let host = std::thread::spawn(move || {
            let parcel = object(TYPE_BINDER, service, service).to_vec();
            broker(Context::Binder).host_transact(0, 1, parcel, &[0])
        });
        let (_, _, data, _, buffer) = g.next_txn();
        assert_eq!(u32::from_le_bytes(data[0..4].try_into().unwrap()), TYPE_HANDLE);
        let handle = u32::from_le_bytes(data[8..12].try_into().unwrap());
        g.send(BC_REPLY, 0, 0, &[], &[], Some(buffer));
        assert_eq!(host.join().unwrap(), Ok(Vec::new()));

        // A memfd holding "hello", and a binder of the guest's own.
        let s = g.s;
        g.p.mem.write(s + 0x200, b"buf\0").unwrap();
        let memfd = g.sys(nr::MEMFD_CREATE, [s + 0x200, 0, 0, 0, 0, 0]);
        assert!(memfd >= 0, "memfd_create: {memfd}");
        g.p.mem.write(s + 0x300, b"hello").unwrap();
        assert_eq!(g.sys(nr::WRITE, [memfd as u64, s + 0x300, 5, 0, 0, 0]), 5);
        let mut parcel = object(TYPE_FD, memfd as u64, 0).to_vec();
        parcel.extend_from_slice(&object(TYPE_BINDER, 0xb0b0, 0xc0c0));
        g.send(BC_TRANSACTION, handle, 9, &parcel, &[0, 24], None);

        let (code, fds, handles) = seen_rx.recv_timeout(Duration::from_secs(10)).expect("the host service was not called");
        assert_eq!(code, 9);
        assert_eq!(fds, vec![b"hello".to_vec()], "the host sees the guest's memfd");
        assert_eq!(handles.len(), 1, "the guest's binder reaches the host as one handle: {handles:?}");
        assert_ne!(handles[0], 0, "a handle in the host's table, not the context manager");

        let (reply, _, data, offsets, _) = g.next_txn();
        assert!(reply, "the reply");
        assert_eq!(&data[0..3], b"ok:");
        assert_eq!(offsets, vec![8], "the reply's one object");
        assert_eq!(u32::from_le_bytes(data[8..12].try_into().unwrap()), TYPE_FD);
        let fd = u32::from_le_bytes(data[16..20].try_into().unwrap()) as u64;
        assert!(fd > 2 && fd != memfd as u64, "a new descriptor in the guest: {fd}");
        let n = g.sys(nr::PREAD64, [fd, s + 0x400, 16, 0, 0, 0]);
        tx.send(g.p.mem.read(s + 0x400, n.max(0) as usize).unwrap()).unwrap();
    });
    let got = rx.recv_timeout(Duration::from_secs(20)).expect("the guest never got the host's reply");
    assert_eq!(String::from_utf8_lossy(&got), "world", "the guest reads the host's shared memory through the received fd");
}
