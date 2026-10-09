//! Binder across host processes (docs/superpowers/specs/2026-09-27-c5-app-launch-design.md). An
//! app runs in a host process of its own (two ART processes cannot share one), and its
//! `/dev/binder` reaches the system's broker over a local socket: each `ioctl` runs in the system
//! host process on a **stand-in** for the app -- a process with the app's pid and uid whose memory
//! and descriptors are the app's, reached back over the same connection -- so the driver is the
//! same driver. The app's host process answers those reads, writes and descriptor requests while
//! its thread waits for the ioctl's result.
//!
//! One connection per app thread (an ioctl can wait for work as long as the thread lives), and one
//! per open of the device, whose end is the open's close.
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use crate::binder::{BinderFile, Context};
use crate::errno::{Errno, SysResult, EBADF, EFAULT, EIO};
use crate::fd::{FileKind, OpenFile};
use crate::process::{Process, Task};

// Frames: u32 length (of what follows), u8 kind, payload.
const OPEN: u8 = 1;
const OPENED: u8 = 2;
const ATTACH: u8 = 3;
const IOCTL: u8 = 4;
const DONE: u8 = 5;
const MMAP: u8 = 6;
const MEM_READ: u8 = 10;
const MEM_DATA: u8 = 11;
const MEM_WRITE: u8 = 12;
const FD_INSERT: u8 = 13;
const FD_GET: u8 = 14;
const FD_DESC: u8 = 15;
const PROPS: u8 = 16;
const PROPS_DATA: u8 = 17;

/// `OMNI_REMOTE_TRACE=1`: every frame, sent and received, in both host processes.
fn trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_REMOTE_TRACE").as_deref() == Ok("1"))
}

/// The longest frame taken: what crosses is guest memory for a binder transaction (its buffers
/// are about a MiB at most) and a properties dump; 64 MiB is generous for both and small enough
/// that a lying length field cannot make the system host allocate its way to an abort.
const MAX_FRAME: usize = 64 << 20;

fn send(stream: &mut TcpStream, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    if trace() {
        eprintln!("[remote {}] send {kind} ({} bytes)", std::process::id(), payload.len());
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&((1 + payload.len()) as u32).to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn receive(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    // Bounded before anything is allocated: a frame is read before its sender is known to hold a
    // credential, so its length is a stranger's word.
    if len == 0 || len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame length out of bounds"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    let kind = body.first().copied().unwrap_or(0);
    body.remove(0);
    if trace() {
        eprintln!("[remote {}] got {kind} ({} bytes)", std::process::id(), body.len());
    }
    Ok((kind, body))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |x| u64::from_le_bytes(x.try_into().expect("8")))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |x| u32::from_le_bytes(x.try_into().expect("4")))
}

fn context_byte(c: Context) -> u8 {
    match c {
        Context::Binder => 0,
        Context::HwBinder => 1,
        Context::VndBinder => 2,
    }
}

fn context_of(b: u8) -> Context {
    match b {
        1 => Context::HwBinder,
        2 => Context::VndBinder,
        _ => Context::Binder,
    }
}

// ---------------------------------------------------------------------------------------------
// Descriptors across host processes: shared memory by its host file, a sync file as signalled.

/// A descriptor as it crosses: `[kind]` then, for shared memory, its name, host path, length and
/// ashmem protection mask (a read-only region is read-only on the other side too);
/// for a socket pair's end or a pipe's end, the port its relay waits on (`crate::relay`) and the
/// end's identity ([`crossing_id`]).
fn describe(file: &Arc<OpenFile>) -> Vec<u8> {
    let port = if crate::relay::relayable(file) { crate::relay::offer(Arc::clone(file)).unwrap_or(0) } else { 0 };
    let id = || [&std::process::id().to_le_bytes()[..], &crossing_id(file).to_le_bytes()].concat();
    match &*file.kind.lock() {
        FileKind::Shared(m) => {
            let mut d = vec![1u8];
            for s in [m.name.as_bytes(), m.host_path_crossing().to_string_lossy().as_bytes()] {
                d.extend_from_slice(&(s.len() as u32).to_le_bytes());
                d.extend_from_slice(s);
            }
            d.extend_from_slice(&m.len().to_le_bytes());
            d.extend_from_slice(&m.prot_mask.load(std::sync::atomic::Ordering::SeqCst).to_le_bytes());
            d
        }
        FileKind::SyncFile(_) => vec![2u8],
        // vold's `/dev/fuse`, handed to MediaProvider: a new open there (`crate::fuse`: nothing
        // is shared but the handshake, which the receiver answers).
        FileKind::Fuse(_) => vec![6u8],
        // The receiver makes a connected end of its own and relays its other end to this one.
        FileKind::Socket(s) => [&[3u8, s.ty as u8][..], &port.to_le_bytes(), &id()].concat(),
        FileKind::Pipe(end) => [&[4u8, u8::from(end.is_write())][..], &port.to_le_bytes(), &id()].concat(),
        // A file (an app's `ParcelFileDescriptor` of its own data, a sysroot file): the receiver
        // opens the same host file, with the same access, at the same offset. The offset is then
        // each side's own (a Linux descriptor passed on shares one); the files that cross are read
        // or written by one side at a time.
        FileKind::Host { file: host, guest, sysroot } if omni_platform::fs::path_of(host).is_ok() => {
            use std::io::Seek;
            let path = omni_platform::fs::path_of(host).expect("checked");
            let offset = (&*host).stream_position().unwrap_or(0);
            let mut d = vec![5u8, u8::from(*sysroot)];
            d.extend_from_slice(&file.flags.lock().to_le_bytes());
            d.extend_from_slice(&offset.to_le_bytes());
            for s in [guest.as_slice(), path.to_string_lossy().as_bytes()] {
                d.extend_from_slice(&(s.len() as u32).to_le_bytes());
                d.extend_from_slice(s);
            }
            d
        }
        other => {
            let kind = match other {
                FileKind::Host { guest, .. } => format!("file {}", String::from_utf8_lossy(guest)),
                FileKind::Dir { .. } => "directory".into(),
                FileKind::Dev(_) => "device".into(),
                FileKind::Synth { guest, .. } => format!("generated {}", String::from_utf8_lossy(guest)),
                FileKind::EventFd(_) => "eventfd".into(),
                FileKind::TimerFd(_) => "timerfd".into(),
                FileKind::Epoll(_) => "epoll".into(),
                FileKind::Binder(_) | FileKind::RemoteBinder(_) => "binder".into(),
                FileKind::Gpu(_) => "gpu".into(),
                _ => "other".into(),
            };
            eprintln!("[remote] a descriptor cannot cross host processes: {kind}");
            vec![0u8]
        }
    }
}

/// The open file a description names, in this host process.
fn open_described(d: &[u8]) -> Result<Arc<OpenFile>, Errno> {
    // An end this host process already holds (the same end, crossing again): the same end, as a
    // dup of one socket is one socket -- not a second relay taking from its queue.
    if matches!(d.first(), Some(3 | 4)) {
        if let Some(known) = crossed(d) {
            return Ok(known);
        }
    }
    match d.first() {
        Some(1) => {
            let mut at = 1;
            let mut field = || {
                let len = u32_at(d, at) as usize;
                let s = String::from_utf8_lossy(d.get(at + 4..at + 4 + len).unwrap_or_default()).into_owned();
                at += 4 + len;
                s
            };
            let (name, path) = (field(), field());
            let len = u64_at(d, at);
            let shm = crate::shm::Shm::open_path(&name, std::path::Path::new(&path), len)?;
            if d.len() >= at + 16 {
                shm.prot_mask.store(u64_at(d, at + 8), std::sync::atomic::Ordering::SeqCst);
            }
            Ok(Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Shared(shm)), flags: parking_lot::Mutex::new(2) }))
        }
        Some(3) => {
            let ty = u64::from(d.get(1).copied().unwrap_or(5));
            let (mine, other) = crate::socket::pair(ty, [0; 3], [0; 3]);
            let socket = crate::socket::Socket { domain: 1, ty, peer: Some(mine), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: 0, passcred: false };
            let other = Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(other)), flags: parking_lot::Mutex::new(2) });
            relay_or_keep(other, port_at(d));
            Ok(remember_crossed(d, Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(socket)), flags: parking_lot::Mutex::new(2) })))
        }
        Some(4) => {
            let (read, write) = crate::pipe::pair();
            let (mine, other) = if d.get(1) == Some(&1) { (write, read) } else { (read, write) };
            relay_or_keep(other, port_at(d));
            Ok(remember_crossed(d, mine))
        }
        Some(5) => {
            use std::io::Seek;
            let sysroot = d.get(1) == Some(&1);
            let flags = u32_at(d, 2);
            let offset = u64_at(d, 6);
            let mut at = 14;
            let mut field = || {
                let len = u32_at(d, at) as usize;
                let s = d.get(at + 4..at + 4 + len).unwrap_or_default().to_vec();
                at += 4 + len;
                s
            };
            let (guest, path) = (field(), field());
            let access = flags & 3;
            let mut host = std::fs::OpenOptions::new()
                .read(access != 1)
                .write(access != 0)
                .append(flags & 0o2000 != 0)
                .open(String::from_utf8_lossy(&path).as_ref())
                .map_err(|_| EBADF)?;
            host.seek(std::io::SeekFrom::Start(offset)).map_err(|_| EIO)?;
            Ok(Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Host { file: host, guest, sysroot }), flags: parking_lot::Mutex::new(flags) }))
        }
        Some(6) => Ok(Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Fuse(crate::fuse::Fuse::open())), flags: parking_lot::Mutex::new(2) })),
        Some(2) => {
            let now = crate::sys::monotonic().as_nanos() as u64;
            Ok(Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::SyncFile(Arc::new(crate::sync_file::SyncFile { signalled_ns: now }))), flags: parking_lot::Mutex::new(0) }))
        }
        _ => Err(EBADF),
    }
}

/// A described socket's or pipe's relay port (0: none).
fn port_at(d: &[u8]) -> u16 {
    d.get(2..4).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

/// **An end's identity as it crosses**: the same number each time the same end (the same open
/// file) is handed to another host process, a new one for another end. A descriptor Android hands
/// out again and again -- the input method's channel to an app, dup'd for every `startInput` --
/// is then one end on the other side, as it is one socket on Linux. When each crossing made an end
/// and a relay of its own, the relays took turns at the one queue: replies went to ends the app
/// had let go, and every key waited 2.5 s for the input method (`Timeout waiting for IME`, run
/// 2026-09-28), clicks behind it.
fn crossing_id(file: &Arc<OpenFile>) -> u64 {
    static IDS: Mutex<Vec<(Weak<OpenFile>, u64)>> = Mutex::new(Vec::new());
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut ids = IDS.lock();
    ids.retain(|(w, _)| w.strong_count() > 0);
    if let Some((_, id)) = ids.iter().find(|(w, _)| w.as_ptr() == Arc::as_ptr(file)) {
        return *id;
    }
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ids.push((Arc::downgrade(file), id));
    id
}

/// The ends that crossed into this host process, by the sender's host process and the end's
/// identity there.
fn crossed_ends() -> &'static Mutex<HashMap<(u32, u64), Weak<OpenFile>>> {
    static CROSSED: OnceLock<Mutex<HashMap<(u32, u64), Weak<OpenFile>>>> = OnceLock::new();
    CROSSED.get_or_init(Mutex::default)
}

/// The identity a described end carries: (sender's host process, end), if it carries one.
fn crossing_of(d: &[u8]) -> Option<(u32, u64)> {
    Some((u32::from_le_bytes(d.get(4..8)?.try_into().ok()?), u64::from_le_bytes(d.get(8..16)?.try_into().ok()?)))
}

/// The end this host process already holds for described `d`, if it is still open.
fn crossed(d: &[u8]) -> Option<Arc<OpenFile>> {
    let key = crossing_of(d)?;
    crossed_ends().lock().get(&key).and_then(Weak::upgrade)
}

/// Remember `mine` as the end described `d` crossed to.
fn remember_crossed(d: &[u8], mine: Arc<OpenFile>) -> Arc<OpenFile> {
    if let Some(key) = crossing_of(d) {
        let mut ends = crossed_ends().lock();
        ends.retain(|_, w| w.strong_count() > 0);
        ends.insert(key, Arc::downgrade(&mine));
    }
    mine
}

/// Relay the far end of a stand-in pair or pipe to the sender's end, or (no relay) keep it open.
fn relay_or_keep(other: Arc<OpenFile>, port: u16) {
    if port != 0 {
        let held = Arc::clone(&other);
        if crate::relay::attach(held, port).is_ok() {
            return;
        }
    }
    keep(Box::new(other));
}

/// The far ends of the stand-in pairs and pipes above, kept open.
fn keep(end: Box<dyn std::any::Any + Send>) {
    static KEPT: Mutex<Vec<Box<dyn std::any::Any + Send>>> = Mutex::new(Vec::new());
    KEPT.lock().push(end);
}

// ---------------------------------------------------------------------------------------------
// The system's side: stand-ins, run on the real broker.

thread_local! {
    /// The connection of the app thread whose ioctl this host thread runs.
    static CURRENT: RefCell<Option<TcpStream>> = const { RefCell::new(None) };
}

/// What `OMNI_REMOTE_STATS=<seconds>` counts in the system's host process: apps' binder ioctls
/// served, and the round trips back to the app's host process they made (its memory read or
/// written, a descriptor got or put -- each a request and an answer over the loopback, a thread
/// of each host process woken), with the bytes those carried.
static IOCTLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ASKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ASK_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `OMNI_REMOTE_STATS=<seconds>`: a `[remote]` line that often (see [`IOCTLS`]).
fn start_stats() {
    let Some(every) = std::env::var("OMNI_REMOTE_STATS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|&s| s > 0) else { return };
    let _ = std::thread::Builder::new().name("omni-remote-stats".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(every));
        let ioctls = IOCTLS.swap(0, std::sync::atomic::Ordering::Relaxed);
        let asks = ASKS.swap(0, std::sync::atomic::Ordering::Relaxed);
        let bytes = ASK_BYTES.swap(0, std::sync::atomic::Ordering::Relaxed);
        let done = DIRECT_DONE.swap(0, std::sync::atomic::Ordering::Relaxed);
        let fell_back = DIRECT_FELL_BACK.swap(0, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[remote] pid {}: {} app binder ioctls/s, {} round trips/s back to the apps ({:.1} per ioctl, {} KB/s); direct {} /s, {} fell back /s (remote_direct={})",
            std::process::id(),
            ioctls / every,
            asks / every,
            asks as f64 / ioctls.max(1) as f64,
            bytes / every / 1024,
            done / every,
            fell_back / every,
            u8::from(DIRECT.load(std::sync::atomic::Ordering::Relaxed)),
        );
    });
}

/// A request to the app's host process on the current connection, and its answer.
fn ask(kind: u8, payload: &[u8]) -> Result<(u8, Vec<u8>), Errno> {
    ASKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ASK_BYTES.fetch_add(payload.len() as u64, std::sync::atomic::Ordering::Relaxed);
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let stream = c.as_mut().ok_or(EFAULT)?;
        send(stream, kind, payload).map_err(|_| EIO)?;
        let answer = receive(stream).map_err(|_| EIO)?;
        ASK_BYTES.fetch_add(answer.1.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(answer)
    })
}

// ---------------------------------------------------------------------------------------------
// Direct access to an app's memory (`remote_direct`).

/// **`remote_direct=1`** (lever; off by default): the system's host process reads and writes an
/// app's guest memory itself (`omni_platform::peer`: `ReadProcessMemory` / `NtWriteVirtualMemory`,
/// `process_vm_readv/writev`) instead of asking the app's host process for it over the thread's
/// connection, a request and an answer each. MEASURED (Windows, i7-13700F E-cores,
/// `omni-platform`'s `peer_memory` test, two host processes): a 64 B read 55-56 us over the
/// loopback (31-38 us of the asking process's CPU alone, plus the answering one's) against 1.3 us
/// direct; 4 KiB 60-61 us against 2.2-2.3 us. A binder ioctl of an app makes ~5-8 of them.
///
/// Guest memory is at the same host addresses in the app's host process (identity mapping, D4;
/// the low window's guest addresses at their based host ones, D41), so a guest address is a host
/// address there -- **bounded** here to the app's guest space, which the app's host process
/// declares when a thread attaches, and only for a host process this one launched
/// (`crate::zygote::host_pid`). Anything the direct path cannot do whole -- a page not mapped, not
/// committed (a lazy mapping, a swept zero page), not readable or writable *on the host*, the low
/// window's seam, a space with the 4 KiB overlay -- goes the old way, whose checks then answer.
///
/// What the owner's side does beyond the copy is kept by **the gate**, a page both processes map
/// (`crate::shm::SharedPage`): word 0 counts the app's views that journal the kernel's writes (a
/// fork child running in the memory) or are one side of a live fork pair -- states in which the
/// copy must go through the owner (`crate::guest::GuestMem`: the journal's `note`, a shelved side's
/// wait) -- and word 1 counts direct accesses in flight. A direct access takes a lease (word 1 up)
/// and then looks at word 0; the owner shuts the gate (word 0 up) and then waits for word 1 to
/// drain, both sequentially consistent: one of the two sees the other.
///
/// Not the owner's: the guest's own protection beyond the host's (a page the guest maps read-only
/// that the host keeps writable would take the write), and the owner's layout lock (an `munmap`
/// racing the copy: the copy fails and falls back, or lands before it -- as Linux's would).
pub static DIRECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static DIRECT_DONE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DIRECT_FELL_BACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const GATE_PREFIX: &str = "omni-remote-gate";

/// One app host process's memory, as the system's host process reaches it directly.
struct Direct {
    peer: omni_platform::peer::PeerMemory,
    gate: crate::shm::SharedPage,
    /// The guest space, `[base, end)`, and its low window (`end`, `delta`) if it has one.
    base: u64,
    end: u64,
    window: Option<(u64, u64)>,
}

impl Direct {
    /// The host address of guest range `[addr, addr + len)` in the app's host process, if the
    /// range is the guest space's and has one host range.
    fn host_range(&self, addr: u64, len: usize) -> Option<usize> {
        let end = addr.checked_add(len as u64)?;
        if addr < self.base || end > self.end {
            return None;
        }
        match self.window {
            Some((window_end, delta)) if addr < window_end => (end <= window_end).then(|| addr.wrapping_add(delta)).and_then(|h| usize::try_from(h).ok()),
            _ => usize::try_from(addr).ok(),
        }
    }

    /// `f` under a lease of the gate, if the gate is open.
    fn leased<T>(&self, f: impl FnOnce() -> Option<T>) -> Option<T> {
        use std::sync::atomic::Ordering::SeqCst;
        let (shut, leases) = (self.gate.word(0), self.gate.word(1));
        leases.fetch_add(1, SeqCst);
        let r = if shut.load(SeqCst) == 0 { f() } else { None };
        leases.fetch_sub(1, SeqCst);
        r
    }

    fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let host = self.host_range(addr, len)?;
        self.leased(|| {
            let mut out = vec![0u8; len];
            self.peer.read(host, &mut out).ok().map(|()| out)
        })
    }

    fn write(&self, addr: u64, bytes: &[u8]) -> Option<()> {
        let host = self.host_range(addr, bytes.len())?;
        self.leased(|| self.peer.write(host, bytes).ok())
    }
}

thread_local! {
    /// The direct access to the memory of the app whose thread this host thread serves.
    static CURRENT_DIRECT: RefCell<Option<Arc<Direct>>> = const { RefCell::new(None) };
}

/// `f` on the current app's direct access, when `remote_direct` is on and there is one; `None`
/// (take the connection) otherwise, or when `f` could not.
fn with_direct<T>(f: impl FnOnce(&Direct) -> Option<T>) -> Option<T> {
    if !DIRECT.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let direct = CURRENT_DIRECT.with(|d| d.borrow().clone())?;
    let r = f(&direct);
    let counter = if r.is_some() { &DIRECT_DONE } else { &DIRECT_FELL_BACK };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    r
}

/// What an app's host process declares when a thread attaches, after the credential: its host
/// pid, its guest space and low window, and its gate's path.
fn declaration(p: &Process) -> Vec<u8> {
    let space = p.mem.space();
    let mut d = std::process::id().to_le_bytes().to_vec();
    d.extend_from_slice(&(space.base() as u64).to_le_bytes());
    d.extend_from_slice(&(space.end() as u64).to_le_bytes());
    let (window_end, delta) = space.low_window().map_or((0, 0), |w| (w.end as u64, w.delta as u64));
    d.extend_from_slice(&window_end.to_le_bytes());
    d.extend_from_slice(&delta.to_le_bytes());
    d.push(u8::from(space.subpages_active()));
    if let Some(gate) = own_gate() {
        d.extend_from_slice(gate.path().to_string_lossy().as_bytes());
    }
    d
}

/// The system's side: the direct access a declaration offers, for app process `pid` -- only for a
/// host process this one launched as that app, with a gate, and no 4 KiB overlay.
fn direct_for(server: &Server, pid: i32, d: &[u8]) -> Option<Arc<Direct>> {
    if d.len() < 37 || d[36] != 0 {
        return None;
    }
    let host_pid = u32_at(d, 0);
    if crate::zygote::host_pid(pid) != Some(host_pid) {
        return None;
    }
    if let Some(known) = server.directs.lock().get(&host_pid).and_then(std::sync::Weak::upgrade) {
        return Some(known);
    }
    let path = std::path::PathBuf::from(String::from_utf8(d[37..].to_vec()).ok()?);
    let gate = crate::shm::SharedPage::open(&path, GATE_PREFIX)?;
    let peer = omni_platform::peer::PeerMemory::open(host_pid).ok()?;
    let (window_end, delta) = (u64_at(d, 20), u64_at(d, 28));
    let direct = Arc::new(Direct { peer, gate, base: u64_at(d, 4), end: u64_at(d, 12), window: (window_end != 0).then_some((window_end, delta)) });
    server.directs.lock().insert(host_pid, Arc::downgrade(&direct));
    Some(direct)
}

/// The app's side: how many of this host process's views hold the gate shut, and the gate.
static EXPOSED: Mutex<u32> = Mutex::new(0);
static GATE: OnceLock<Option<crate::shm::SharedPage>> = OnceLock::new();

/// This app host process's gate, made at its first use: shut as many times as views hold it.
fn own_gate() -> Option<&'static crate::shm::SharedPage> {
    GATE.get_or_init(|| {
        let exposed = EXPOSED.lock();
        let page = crate::shm::SharedPage::create(GATE_PREFIX)?;
        page.word(0).store(*exposed, std::sync::atomic::Ordering::SeqCst);
        Some(page)
    })
    .as_ref()
}

/// A view of this host process starts (`exposed`) or stops holding the gate shut
/// (`crate::guest::GuestMem`). Shutting it waits until no direct access is in flight.
pub(crate) fn gate(exposed: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    let mut n = EXPOSED.lock();
    if exposed {
        *n += 1;
    } else {
        *n = n.saturating_sub(1);
    }
    let Some(Some(page)) = GATE.get() else { return };
    if exposed {
        page.word(0).fetch_add(1, SeqCst);
        while page.word(1).load(SeqCst) != 0 {
            std::thread::yield_now();
        }
    } else {
        page.word(0).fetch_sub(1, SeqCst);
    }
}

/// The app's memory, reached through the current connection.
pub struct RemoteMemory;

impl crate::guest::Remote for RemoteMemory {
    fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno> {
        if let Some(bytes) = with_direct(|d| d.read(addr, len)) {
            return Ok(bytes);
        }
        let mut req = addr.to_le_bytes().to_vec();
        req.extend_from_slice(&(len as u32).to_le_bytes());
        match ask(MEM_READ, &req)? {
            (MEM_DATA, data) if data.first() == Some(&1) => Ok(data[1..].to_vec()),
            _ => Err(EFAULT),
        }
    }

    fn write(&self, addr: u64, bytes: &[u8]) -> Result<(), Errno> {
        if with_direct(|d| d.write(addr, bytes)).is_some() {
            return Ok(());
        }
        let mut req = addr.to_le_bytes().to_vec();
        req.extend_from_slice(bytes);
        match ask(MEM_WRITE, &req)? {
            (DONE, r) if u64_at(&r, 0) == 0 => Ok(()),
            _ => Err(EFAULT),
        }
    }
}

/// The app's descriptors, reached through the current connection.
pub struct RemoteFds;

impl crate::fd::RemoteFds for RemoteFds {
    fn insert(&self, file: Arc<OpenFile>) -> Result<i32, Errno> {
        match ask(FD_INSERT, &describe(&file))? {
            (DONE, r) => {
                let v = u64_at(&r, 0) as i64;
                if v < 0 { Err(Errno(-v as i32)) } else { Ok(v as i32) }
            }
            _ => Err(EIO),
        }
    }

    fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        match ask(FD_GET, &fd.to_le_bytes())? {
            (FD_DESC, d) => open_described(&d),
            _ => Err(EBADF),
        }
    }
}

/// A process's credential with the system's binder listener: 16 random bytes the zygote hands an
/// app's host process at launch (on its stdin), which that host process presents on every
/// connection. The listener takes the stand-in's pid and uid from it, never from a frame: before
/// it, any local program -- or a guest's socket to the host's loopback -- could claim uid 1000 or
/// another instance's uid.
pub type Credential = [u8; 16];

/// The credentials issued in this host process: credential -> (pid, uid).
fn issued() -> &'static Mutex<HashMap<Credential, (i32, u32)>> {
    static ISSUED: OnceLock<Mutex<HashMap<Credential, (i32, u32)>>> = OnceLock::new();
    ISSUED.get_or_init(Mutex::default)
}

/// Issue the credential of the process `pid` running as `uid`.
///
/// # Panics
/// When the host has no entropy to give (`omni_platform::process::random_bytes`).
#[must_use]
pub fn issue_credential(pid: i32, uid: u32) -> Credential {
    let mut c = [0u8; 16];
    omni_platform::process::random_bytes(&mut c).expect("entropy for a binder credential");
    // One credential per pid: stand-ins are found by pid alone, so a second uid for the same pid
    // must not leave the first one's credential working.
    let mut issued = issued().lock();
    issued.retain(|_, (p, _)| *p != pid);
    issued.insert(c, (pid, uid));
    c
}

/// Withdraw process `pid`'s credential (its host process ended).
pub fn revoke_credential(pid: i32) {
    issued().lock().retain(|_, (p, _)| *p != pid);
}

fn identity(c: &[u8]) -> Option<(i32, u32)> {
    let c: Credential = c.try_into().ok()?;
    issued().lock().get(&c).copied()
}

#[must_use]
pub fn credential_hex(c: &Credential) -> String {
    c.iter().map(|b| format!("{b:02x}")).collect()
}

#[must_use]
pub fn credential_from_hex(s: &str) -> Option<Credential> {
    let s = s.trim();
    if s.len() != 32 {
        return None;
    }
    let mut c = [0u8; 16];
    for (i, b) in c.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(c)
}

fn random_token() -> u64 {
    let mut b = [0u8; 8];
    omni_platform::process::random_bytes(&mut b).expect("entropy for an open's token");
    u64::from_le_bytes(b)
}

/// The stand-ins, by pid; an open's binder file, by token.
struct Server {
    sysroot: Arc<crate::vfs::Sysroot>,
    stand_ins: Mutex<HashMap<i32, Weak<Process>>>,
    /// An open's binder file, its stand-in, and the credential that opened it, by random token.
    opens: Mutex<HashMap<u64, (Arc<Process>, Arc<BinderFile>, Credential)>>,
    /// The direct accesses to app host processes' memory, by host pid (`remote_direct`).
    directs: Mutex<HashMap<u32, Weak<Direct>>>,
}

/// Serve apps' binder on a local port: what `--binder-server` points an app's host process at.
///
/// # Errors
/// The port cannot be opened.
pub fn serve(sysroot: Arc<crate::vfs::Sysroot>) -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = Arc::new(Server { sysroot, stand_ins: Mutex::default(), opens: Mutex::default(), directs: Mutex::default() });
    let _ = SERVING.set(Arc::clone(&server));
    start_stats();
    std::thread::Builder::new().name("binder-remote".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_nodelay(true);
            let server = Arc::clone(&server);
            let _ = std::thread::Builder::new().name("binder-remote-conn".into()).spawn(move || server.connection(stream));
        }
    })?;
    Ok(addr)
}

/// This host process's binder listener, once it serves (`serve`).
static SERVING: OnceLock<Arc<Server>> = OnceLock::new();

/// Process `pid` becomes `uid` (and its group): its credential's identity, and its stand-in's
/// ids when it has opened the binder already -- a spare app process (`crate::zygote`), started
/// as root before the app it becomes was known, and given its uid before it makes a call.
pub fn rebind_uid(pid: i32, uid: u32) {
    for (_, (p, u)) in issued().lock().iter_mut() {
        if *p == pid {
            *u = uid;
        }
    }
    if let Some(p) = SERVING.get().and_then(|s| s.stand_ins.lock().get(&pid).and_then(Weak::upgrade)) {
        p.sys.become_user(uid);
    }
}

impl Server {
    fn stand_in(&self, pid: i32, uid: u32) -> Arc<Process> {
        let mut stand_ins = self.stand_ins.lock();
        if let Some(p) = stand_ins.get(&pid).and_then(Weak::upgrade) {
            return p;
        }
        let p = Process::stand_in(Arc::clone(&self.sysroot), pid, uid, Arc::new(RemoteMemory), Arc::new(RemoteFds));
        stand_ins.insert(pid, Arc::downgrade(&p));
        p
    }

    fn connection(&self, mut stream: TcpStream) {
        let Ok((kind, body)) = receive(&mut stream) else { return };
        match kind {
            OPEN => {
                let Some((pid, uid)) = identity(body.get(1..17).unwrap_or_default()) else { return };
                let cred: Credential = body[1..17].try_into().expect("16");
                let context = context_of(body[0]);
                let p = self.stand_in(pid, uid);
                let file = BinderFile::open(context);
                let token = random_token();
                self.opens.lock().insert(token, (p, file, cred));
                let mut reply = token.to_le_bytes().to_vec();
                reply.extend_from_slice(&(pid as u32).to_le_bytes());
                reply.extend_from_slice(&uid.to_le_bytes());
                if send(&mut stream, OPENED, &reply).is_err() {
                    self.opens.lock().remove(&token);
                    return;
                }
                // The open lasts as long as this connection: its end is the close.
                let mut sink = [0u8; 1];
                let _ = stream.read(&mut sink);
                if let Some((_, file, _)) = self.opens.lock().remove(&token) {
                    file.release();
                }
            }
            PROPS => {
                if identity(&body).is_none() {
                    return;
                }
                // The system's properties as they are now, `name value ` each.
                let service = crate::props::PropertyService::global(&self.sysroot);
                let mut out = Vec::new();
                for (k, v) in service.entries() {
                    out.extend_from_slice(k.as_bytes());
                    out.push(0);
                    out.extend_from_slice(v.as_bytes());
                    out.push(0);
                }
                let _ = send(&mut stream, PROPS_DATA, &out);
            }
            ATTACH => {
                let (token, tid) = (u64_at(&body, 0), u32_at(&body, 8) as i32);
                let Some((p, file, owner)) = self.opens.lock().get(&token).cloned() else { return };
                // Only the credential that made the open attaches threads to it.
                if body.get(12..28) != Some(&owner[..]) {
                    return;
                }
                let mut task = Task::new(tid, Arc::clone(&p));
                CURRENT.with(|c| *c.borrow_mut() = stream.try_clone().ok());
                let direct = body.get(28..).and_then(|d| direct_for(self, p.sys.pid, d));
                CURRENT_DIRECT.with(|d| *d.borrow_mut() = direct);
                loop {
                    let Ok((kind, body)) = receive(&mut stream) else { break };
                    if kind == IOCTL {
                        IOCTLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let ret: u64 = match kind {
                        IOCTL => match crate::binder::ioctl(&p, &mut task, &file, u64_at(&body, 0), u64_at(&body, 8), false) {
                            Ok(v) => v,
                            Err(e) => e.as_return(),
                        },
                        MMAP => {
                            file.set_area(u64_at(&body, 0), u64_at(&body, 8));
                            0
                        }
                        other => {
                            eprintln!("[remote] pid {} tid {tid}: unexpected frame {other} on its thread's connection", p.sys.pid);
                            break;
                        }
                    };
                    if send(&mut stream, DONE, &ret.to_le_bytes()).is_err() {
                        break;
                    }
                }
                CURRENT.with(|c| *c.borrow_mut() = None);
                CURRENT_DIRECT.with(|d| *d.borrow_mut() = None);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The app's side.

static SERVER: OnceLock<String> = OnceLock::new();

static CREDENTIAL: OnceLock<Credential> = OnceLock::new();

/// This host process's credential with the system's binder listener (read from stdin by the
/// runner: `--binder-credential-stdin`).
pub fn set_credential(c: Credential) {
    let _ = CREDENTIAL.set(c);
}

fn credential() -> Result<Credential, Errno> {
    CREDENTIAL.get().copied().ok_or(EIO)
}

/// Point this host process's `/dev/binder` at the system's (`serve`'s address).
pub fn set_server(addr: &str) {
    let _ = SERVER.set(addr.to_string());
}

/// The system's properties, as they are now (an app's host process starts from them: what
/// servicemanager and system_server have set -- `servicemanager.ready` -- as well as the build's).
///
/// # Errors
/// The system cannot be reached.
pub fn system_properties() -> Result<Vec<(String, String)>, Errno> {
    let mut s = TcpStream::connect(SERVER.get().ok_or(EIO)?).map_err(|_| EIO)?;
    send(&mut s, PROPS, &credential()?).map_err(|_| EIO)?;
    let (kind, body) = receive(&mut s).map_err(|_| EIO)?;
    if kind != PROPS_DATA {
        return Err(EIO);
    }
    let parts: Vec<&[u8]> = body.split(|b| *b == 0).collect();
    Ok(parts.chunks(2).filter(|c| c.len() == 2 && !c[0].is_empty()).map(|c| (String::from_utf8_lossy(c[0]).into_owned(), String::from_utf8_lossy(c[1]).into_owned())).collect())
}

/// A thread's connection to the system failed: said, and `EIO`.
fn lost(what: &str, e: &std::io::Error) -> Errno {
    eprintln!("[remote] {what}: {e}");
    EIO
}

/// Whether binder is remote here.
#[must_use]
pub fn is_remote() -> bool {
    SERVER.get().is_some()
}

/// An open of `/dev/binder` whose driver is in the system's host process.
pub struct RemoteBinder {
    token: u64,
    /// Held open: closing it is the open's close.
    _control: TcpStream,
    /// One connection per thread that uses it.
    threads: Mutex<HashMap<i32, Arc<Mutex<TcpStream>>>>,
}

impl RemoteBinder {
    /// Open the device in the system's host process for process `p`.
    ///
    /// # Errors
    /// `EIO` when the system cannot be reached.
    pub fn open(_p: &Process, context: Context) -> Result<Arc<Self>, Errno> {
        let addr = SERVER.get().ok_or(EIO)?;
        let mut control = TcpStream::connect(addr).map_err(|_| EIO)?;
        let _ = control.set_nodelay(true);
        let mut req = vec![context_byte(context)];
        req.extend_from_slice(&credential()?);
        send(&mut control, OPEN, &req).map_err(|_| EIO)?;
        let (kind, body) = receive(&mut control).map_err(|_| EIO)?;
        if kind != OPENED {
            return Err(EIO);
        }
        Ok(Arc::new(Self { token: u64_at(&body, 0), _control: control, threads: Mutex::default() }))
    }

    fn thread(&self, p: &Process, tid: i32) -> Result<Arc<Mutex<TcpStream>>, Errno> {
        if let Some(s) = self.threads.lock().get(&tid) {
            return Ok(Arc::clone(s));
        }
        let mut s = TcpStream::connect(SERVER.get().ok_or(EIO)?).map_err(|e| lost("connect", &e))?;
        let _ = s.set_nodelay(true);
        let mut req = self.token.to_le_bytes().to_vec();
        req.extend_from_slice(&(tid as u32).to_le_bytes());
        req.extend_from_slice(&credential()?);
        // What a direct access to this process's memory needs (`remote_direct`), after the
        // credential: the system uses it or not.
        req.extend_from_slice(&declaration(p));
        send(&mut s, ATTACH, &req).map_err(|e| lost("attach", &e))?;
        let s = Arc::new(Mutex::new(s));
        self.threads.lock().insert(tid, Arc::clone(&s));
        Ok(s)
    }

    /// Send a request and answer the system's reads, writes and descriptor requests until its
    /// result comes.
    fn call(&self, p: &Process, t: &Task, kind: u8, payload: &[u8]) -> SysResult {
        let conn = self.thread(p, t.tid)?;
        let mut s = conn.lock();
        send(&mut s, kind, payload).map_err(|e| lost("send", &e))?;
        loop {
            let (kind, body) = receive(&mut s).map_err(|e| lost("receive", &e))?;
            match kind {
                DONE => return Ok(u64_at(&body, 0)),
                MEM_READ => {
                    let reply = match p.mem.read(u64_at(&body, 0), u32_at(&body, 8) as usize) {
                        Ok(bytes) => [&[1u8][..], &bytes].concat(),
                        Err(_) => vec![0u8],
                    };
                    send(&mut s, MEM_DATA, &reply).map_err(|_| EIO)?;
                }
                MEM_WRITE => {
                    let r: u64 = if p.mem.write(u64_at(&body, 0), &body[8..]).is_ok() { 0 } else { 1 };
                    send(&mut s, DONE, &r.to_le_bytes()).map_err(|_| EIO)?;
                }
                FD_INSERT => {
                    let r = open_described(&body).and_then(|f| p.fds.insert(f, false, 0));
                    let v: i64 = match r {
                        Ok(fd) => i64::from(fd),
                        Err(e) => -i64::from(e.0),
                    };
                    send(&mut s, DONE, &(v as u64).to_le_bytes()).map_err(|_| EIO)?;
                }
                FD_GET => {
                    let d = p.fds.get(u32_at(&body, 0) as i32).map_or_else(|_| vec![0u8], |f| describe(&f));
                    send(&mut s, FD_DESC, &d).map_err(|_| EIO)?;
                }
                other => {
                    eprintln!("[remote] pid {} tid {}: unexpected frame {other}", p.sys.pid, t.tid);
                    return Err(EIO);
                }
            }
        }
    }

    /// An ioctl on the device: run in the system's host process.
    pub fn ioctl(&self, p: &Process, t: &Task, cmd: u64, arg: u64) -> SysResult {
        let mut req = cmd.to_le_bytes().to_vec();
        req.extend_from_slice(&arg.to_le_bytes());
        let r = self.call(p, t, IOCTL, &req)?;
        // The driver's answer, as a syscall return value.
        if r > (-4096i64) as u64 {
            let e = Errno(-(r as i64) as i32);
            if e != crate::errno::EINTR {
                eprintln!("[remote] binder ioctl {cmd:#x} of pid {} failed: errno {}", p.sys.pid, e.0);
            }
            Err(e)
        } else {
            Ok(r)
        }
    }

    /// `mmap` of the device: the receive area is mapped here and its place told to the driver.
    pub fn mmap(&self, p: &Process, t: &Task, len: u64) -> Result<u64, Errno> {
        let at = p.mm.map(p, t, crate::mm::MapRequest { addr: 0, len, prot: 3, flags: 0x22, fd: -1, offset: 0 })?;
        p.mm.label(at, (len + p.mm.page_size() - 1) & !(p.mm.page_size() - 1), b"/dev/binderfs/binder");
        let mut req = at.to_le_bytes().to_vec();
        req.extend_from_slice(&len.to_le_bytes());
        self.call(p, t, MMAP, &req)?;
        Ok(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};

    fn frame(stream: &mut TcpStream, kind: u8, payload: &[u8]) {
        send(stream, kind, payload).expect("send");
    }

    fn sysroot() -> Option<Arc<crate::vfs::Sysroot>> {
        // As the integration tests find it (`tests/common/mod.rs`): `OMNI_SYSROOT` (the Linux and
        // macOS hosts keep theirs there), else the in-repo one.
        let dir = std::env::var_os("OMNI_SYSROOT").map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35"),
            std::path::PathBuf::from,
        );
        crate::vfs::Sysroot::open(&dir).ok()
    }

    /// A direct access on this very process (what another host process's is, short of the
    /// process boundary `omni-platform`'s `peer_memory` test crosses): inside the declared space
    /// it reads and writes; outside it, across the low window's seam, or with the gate shut it
    /// declines (the caller then asks over the connection).
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_direct_access_stays_in_the_space_and_behind_the_gate() {
        use std::sync::atomic::Ordering::SeqCst;
        let mut memory = vec![7u8; 8192];
        let base = memory.as_mut_ptr() as u64;
        let made = crate::shm::SharedPage::create("omni-remote-gate-test").expect("a gate");
        let gate = crate::shm::SharedPage::open(made.path(), "omni-remote-gate-test").expect("opened by path");
        let d = Direct { peer: omni_platform::peer::PeerMemory::open(std::process::id()).expect("this process"), gate, base, end: base + 8192, window: None };
        assert_eq!(d.read(base + 10, 4), Some(vec![7; 4]));
        assert_eq!(d.write(base + 100, b"abc"), Some(()));
        assert_eq!(&memory[100..103], b"abc");
        assert_eq!(d.read(base + 8190, 4), None, "past the space's end");
        assert_eq!(d.read(base - 1, 1), None, "before its start");
        // The other process's view of the same page: shutting it there closes it here.
        made.word(0).store(1, SeqCst);
        assert_eq!(d.read(base, 4), None, "the gate shut");
        assert_eq!(d.write(base, b"x"), None);
        assert_eq!(memory[0], 7);
        made.word(0).store(0, SeqCst);
        assert_eq!(made.word(1).load(SeqCst), 0, "no lease left behind");
        // A low window: guest addresses below its end are at `delta` above.
        let w = Direct { window: Some((0x1000, base - 0x10)), base: 0x10, end: base + 8192, ..d };
        assert_eq!(w.host_range(0x10, 4), Some(base as usize));
        assert_eq!(w.host_range(0xffe, 4), None, "across the window's seam");
        std::fs::remove_file(made.path()).ok();
    }

    /// **A stand-in's whole binder ioctl with no round trip**: a `BC_TRANSACTION` and the read of
    /// its answer, run on a stand-in whose memory is reached only directly (this thread has no
    /// connection to the app: any request over one would fail the call as `EFAULT`). The
    /// command, its data, the answer and the consumed counts are all read and written there.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_stand_ins_binder_call_needs_no_round_trip_with_direct_access() {
        let mut memory = vec![0u8; 1 << 16];
        let base = memory.as_mut_ptr() as u64;
        let w64 = |m: &mut Vec<u8>, at: usize, v: u64| m[at..at + 8].copy_from_slice(&v.to_le_bytes());
        // bwr: write 4 + 64 bytes at +0x100, read 0x100 at +0x400.
        w64(&mut memory, 0, 68);
        w64(&mut memory, 16, base + 0x100);
        w64(&mut memory, 24, 0x100);
        w64(&mut memory, 40, base + 0x400);
        memory[0x100..0x104].copy_from_slice(&0x4040_6300u32.to_le_bytes()); // BC_TRANSACTION
        // binder_transaction_data: handle 0, cookie, code 1, flags 0, pid/euid, 16 bytes of data
        // at +0x800, no offsets (at +0x900).
        let tr = 0x104;
        memory[tr + 16..tr + 20].copy_from_slice(&1u32.to_le_bytes());
        w64(&mut memory, tr + 32, 16);
        w64(&mut memory, tr + 40, 0);
        w64(&mut memory, tr + 48, base + 0x800);
        w64(&mut memory, tr + 56, base + 0x900);
        let made = crate::shm::SharedPage::create("omni-remote-gate-call").expect("a gate");
        let gate = crate::shm::SharedPage::open(made.path(), "omni-remote-gate-call").expect("opened");
        let direct = Arc::new(Direct { peer: omni_platform::peer::PeerMemory::open(std::process::id()).unwrap(), gate, base, end: base + (1 << 16), window: None });
        CURRENT_DIRECT.with(|d| *d.borrow_mut() = Some(direct));
        DIRECT.store(true, std::sync::atomic::Ordering::SeqCst);
        let m = crate::manifest::parse("d\t755\t/\n").unwrap();
        let p = Process::stand_in(crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), m), 424_242, 10_000, Arc::new(RemoteMemory), Arc::new(RemoteFds));
        let file = BinderFile::open(Context::VndBinder);
        file.set_area(base + 0x4000, 0x8000);
        let mut task = Task::new(424_242, Arc::clone(&p));
        let r = crate::binder::ioctl(&p, &mut task, &file, 0xc030_6201, base, true);
        DIRECT.store(false, std::sync::atomic::Ordering::SeqCst);
        CURRENT_DIRECT.with(|d| *d.borrow_mut() = None);
        assert_eq!(r, Ok(0), "the ioctl ran on direct access alone");
        let u64_of = |at: usize| u64::from_le_bytes(memory[at..at + 8].try_into().unwrap());
        assert_eq!(u64_of(8), 68, "the command consumed, written back directly");
        assert!(u64_of(32) >= 4, "an answer read, written back directly ({})", u64_of(32));
        file.release();
        std::fs::remove_file(made.path()).ok();
    }

    /// The owner shuts the gate only once no direct access is in flight: a lease taken first is
    /// waited out.
    #[test]
    fn shutting_the_gate_waits_out_a_lease() {
        use std::sync::atomic::Ordering::SeqCst;
        let Some(page) = own_gate() else { return };
        page.word(1).fetch_add(1, SeqCst);
        let shut = std::thread::spawn(|| {
            let t = std::time::Instant::now();
            gate(true);
            t.elapsed()
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        page.word(1).fetch_sub(1, SeqCst);
        let waited = shut.join().unwrap();
        assert!(waited >= std::time::Duration::from_millis(40), "shut after the lease ended ({waited:?})");
        assert!(page.word(0).load(SeqCst) >= 1, "shut");
        gate(false);
    }

    /// A declaration from a host process this one did not launch as that app is not used.
    #[test]
    fn a_direct_access_is_only_for_a_launched_app() {
        let Some(root) = sysroot() else { return };
        let server = Server { sysroot: root, stand_ins: Mutex::default(), opens: Mutex::default(), directs: Mutex::default() };
        let mut d = std::process::id().to_le_bytes().to_vec();
        d.extend_from_slice(&[0u8; 33]);
        assert!(direct_for(&server, 12345, &d).is_none());
    }

    #[test]
    fn an_unknown_credential_is_refused() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&[7u8; 16]); // never issued
        frame(&mut s, OPEN, &req);
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(s.read(&mut b).unwrap_or(0), 0, "closed without an answer");
    }

    #[test]
    fn the_stand_ins_identity_is_the_credentials_whatever_the_process_claims() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let c = issue_credential(4242_000, 10_115);
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&c);
        frame(&mut s, OPEN, &req);
        let (kind, body) = receive(&mut s).expect("answer");
        assert_eq!(kind, OPENED);
        assert_eq!(u32_at(&body, 8) as i32, 4242_000, "pid from the credential");
        assert_eq!(u32_at(&body, 12), 10_115, "uid from the credential");
        revoke_credential(4242_000);
        let mut again = TcpStream::connect(addr).expect("connect");
        frame(&mut again, OPEN, &req);
        again.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(again.read(&mut b).unwrap_or(0), 0, "revoked: refused");
    }

    #[test]
    fn an_attach_needs_the_opens_own_credential() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let mine = issue_credential(4243_000, 10_116);
        let theirs = issue_credential(4244_000, 10_117);
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&mine);
        frame(&mut s, OPEN, &req);
        let (_, body) = receive(&mut s).expect("opened");
        let token = u64_at(&body, 0);
        let mut a = TcpStream::connect(addr).expect("connect");
        let mut att = token.to_le_bytes().to_vec();
        att.extend_from_slice(&7u32.to_le_bytes());
        att.extend_from_slice(&theirs);
        frame(&mut a, ATTACH, &att);
        frame(&mut a, MMAP, &[0u8; 16]);
        a.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(a.read(&mut b).unwrap_or(0), 0, "another's credential cannot attach to this open");
        // The owner's own credential attaches and is served.
        let mut ok = TcpStream::connect(addr).expect("connect");
        let mut att = token.to_le_bytes().to_vec();
        att.extend_from_slice(&7u32.to_le_bytes());
        att.extend_from_slice(&mine);
        frame(&mut ok, ATTACH, &att);
        frame(&mut ok, MMAP, &[0u8; 16]);
        ok.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let (kind, _) = receive(&mut ok).expect("served");
        assert_eq!(kind, DONE, "the owner's thread attaches");
        revoke_credential(4243_000);
        revoke_credential(4244_000);
    }

    #[test]
    fn a_pid_holds_one_credential_the_latest() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let first = issue_credential(4246_000, 10_120);
        let second = issue_credential(4246_000, 10_121);
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&first);
        frame(&mut s, OPEN, &req);
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(s.read(&mut b).unwrap_or(0), 0, "the earlier credential is refused");
        let mut t = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&second);
        frame(&mut t, OPEN, &req);
        let (kind, body) = receive(&mut t).expect("answer");
        assert_eq!(kind, OPENED);
        assert_eq!(u32_at(&body, 12), 10_121, "the second uid");
        revoke_credential(4246_000);
    }

    #[test]
    fn a_frame_with_a_lying_length_closes_the_connection_and_the_server_serves_on() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        for len in [0u32, 0xFFFF_FFFF] {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.write_all(&len.to_le_bytes()).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut b = [0u8; 1];
            assert_eq!(s.read(&mut b).unwrap_or(0), 0, "closed on length {len:#x}");
        }
        let c = issue_credential(4247_000, 10_122);
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&c);
        frame(&mut s, OPEN, &req);
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let (kind, _) = receive(&mut s).expect("still serving");
        assert_eq!(kind, OPENED);
        revoke_credential(4247_000);
    }

    #[test]
    fn a_credential_round_trips_as_hex() {
        let c = issue_credential(4245_000, 10_000);
        assert_eq!(credential_from_hex(&credential_hex(&c)), Some(c));
        assert_eq!(credential_from_hex("zz"), None);
        revoke_credential(4245_000);
    }

    /// A file's descriptor crosses to another host process as the same host file, with its access
    /// and offset: the WebView hands its service a `ParcelFileDescriptor` of the app's own data
    /// (`app_webview/variations_seed_new`), and a refused descriptor failed the whole transaction.
        /// A read-only ashmem region stays read-only on the other side: the WebView's renderer (another
    /// host process) checks the mask of every read-only region it is handed
    /// (`platform_shared_memory_region_android.cc`: "Ashmem region has a wrong protection mask"),
    /// and its CHECK took the renderer -- and WebView then the app -- down (Roblox, 2026-09-29).
    #[test]
    fn a_shared_regions_protection_mask_crosses_with_it() {
        let shm = crate::shm::Shm::create("dev/ashmem").expect("a region");
        shm.prot_mask.store(1, std::sync::atomic::Ordering::SeqCst); // PROT_READ
        let file = Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Shared(shm)), flags: parking_lot::Mutex::new(2) });
        let other = open_described(&describe(&file)).expect("opened on the other side");
        let FileKind::Shared(m) = &*other.kind.lock() else { panic!("not a shared region") };
        assert_eq!(m.prot_mask.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

#[test]
    fn a_file_crosses_as_the_same_host_file_at_the_same_offset() {
        let path = std::env::temp_dir().join(format!("omni-remote-file-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let mut host = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        host.seek(std::io::SeekFrom::Start(4)).unwrap();
        let file = Arc::new(OpenFile {
            kind: parking_lot::Mutex::new(FileKind::Host { file: host, guest: b"/data/user/0/x/seed".to_vec(), sysroot: false }),
            flags: parking_lot::Mutex::new(2),
        });
        let d = describe(&file);
        assert_eq!(d.first(), Some(&5), "a file is described, not refused");
        let crossed = open_described(&d).expect("opened on the other side");
        assert_eq!(*crossed.flags.lock(), 2);
        {
            let mut kind = crossed.kind.lock();
            let FileKind::Host { file: other, guest, sysroot } = &mut *kind else { panic!("a host file") };
            assert_eq!(guest, b"/data/user/0/x/seed");
            assert!(!*sysroot);
            let mut rest = String::new();
            other.read_to_string(&mut rest).unwrap();
            assert_eq!(rest, "456789", "read from the sender's offset");
            other.write_all(b"!").unwrap();
        }
        drop(crossed);
        drop(file);
        assert!(std::fs::read(&path).is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789!", "writable, and the same file");
        let _ = std::fs::remove_file(&path);
    }

    /// The same socket pair's end handed over twice is one end on the other side (a dup of one
    /// socket), and what its peer sends is read there, whole, in order -- not split between two.
    #[test]
    fn an_end_that_crosses_twice_is_one_end_and_loses_nothing() {
        let (peer, kept) = crate::socket::pair(5, [0; 3], [0; 3]);
        let handed = Arc::new(OpenFile {
            kind: parking_lot::Mutex::new(FileKind::Socket(crate::socket::Socket { domain: 1, ty: 5, peer: Some(peer), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: 0, passcred: false })),
            flags: parking_lot::Mutex::new(2),
        });
        let first = open_described(&describe(&handed)).expect("first crossing");
        let second = open_described(&describe(&handed)).expect("second crossing");
        assert!(Arc::ptr_eq(&first, &second), "one end");
        let mut kept = kept;
        for i in 0..20u8 {
            crate::socket::send(&mut kept, &[i]).expect("send");
        }
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.len() < 20 && std::time::Instant::now() < deadline {
            let mut buf = [0u8; 8];
            let r = {
                let FileKind::Socket(s) = &mut *second.kind.lock() else { panic!("socket") };
                crate::socket::receive(s, &mut buf)
            };
            match r {
                Ok(n) => got.push(buf[..n][0]),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        assert_eq!(got, (0..20).collect::<Vec<u8>>());
        // Another end is another identity.
        let (other, _) = crate::socket::pair(5, [0; 3], [0; 3]);
        let another = Arc::new(OpenFile {
            kind: parking_lot::Mutex::new(FileKind::Socket(crate::socket::Socket { domain: 1, ty: 5, peer: Some(other), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: 0, passcred: false })),
            flags: parking_lot::Mutex::new(2),
        });
        assert!(!Arc::ptr_eq(&open_described(&describe(&another)).expect("another"), &first));
    }
}
