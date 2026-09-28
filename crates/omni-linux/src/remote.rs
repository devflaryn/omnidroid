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

/// A descriptor as it crosses: `[kind]` then, for shared memory, its name, host path and length;
/// for a socket pair's end or a pipe's end, the port its relay waits on (`crate::relay`).
fn describe(file: &Arc<OpenFile>) -> Vec<u8> {
    let port = if crate::relay::relayable(file) { crate::relay::offer(Arc::clone(file)).unwrap_or(0) } else { 0 };
    match &*file.kind.lock() {
        FileKind::Shared(m) => {
            let mut d = vec![1u8];
            for s in [m.name.as_bytes(), m.host_path_crossing().to_string_lossy().as_bytes()] {
                d.extend_from_slice(&(s.len() as u32).to_le_bytes());
                d.extend_from_slice(s);
            }
            d.extend_from_slice(&m.len().to_le_bytes());
            d
        }
        FileKind::SyncFile(_) => vec![2u8],
        // vold's `/dev/fuse`, handed to MediaProvider: a new open there (`crate::fuse`: nothing
        // is shared but the handshake, which the receiver answers).
        FileKind::Fuse(_) => vec![6u8],
        // The receiver makes a connected end of its own and relays its other end to this one.
        FileKind::Socket(s) => [&[3u8, s.ty as u8][..], &port.to_le_bytes()].concat(),
        FileKind::Pipe(end) => [&[4u8, u8::from(end.is_write())][..], &port.to_le_bytes()].concat(),
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
fn open_described(d: &[u8]) -> Result<OpenFile, Errno> {
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
            Ok(OpenFile { kind: parking_lot::Mutex::new(FileKind::Shared(shm)), flags: parking_lot::Mutex::new(2) })
        }
        Some(3) => {
            let ty = u64::from(d.get(1).copied().unwrap_or(5));
            let (mine, other) = crate::socket::pair(ty, [0; 3], [0; 3]);
            let socket = crate::socket::Socket { domain: 1, ty, peer: Some(mine), inbox: std::collections::VecDeque::new(), name: None, protocol: 0, owner: 0, passcred: false };
            let other = Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(other)), flags: parking_lot::Mutex::new(2) });
            relay_or_keep(other, port_at(d));
            Ok(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(socket)), flags: parking_lot::Mutex::new(2) })
        }
        Some(4) => {
            let (read, write) = crate::pipe::pair();
            let (mine, other) = if d.get(1) == Some(&1) { (write, read) } else { (read, write) };
            relay_or_keep(other, port_at(d));
            Arc::try_unwrap(mine).map_err(|_| EIO)
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
            Ok(OpenFile { kind: parking_lot::Mutex::new(FileKind::Host { file: host, guest, sysroot }), flags: parking_lot::Mutex::new(flags) })
        }
        Some(6) => Ok(OpenFile { kind: parking_lot::Mutex::new(FileKind::Fuse(crate::fuse::Fuse::open())), flags: parking_lot::Mutex::new(2) }),
        Some(2) => {
            let now = crate::sys::monotonic().as_nanos() as u64;
            Ok(OpenFile { kind: parking_lot::Mutex::new(FileKind::SyncFile(Arc::new(crate::sync_file::SyncFile { signalled_ns: now }))), flags: parking_lot::Mutex::new(0) })
        }
        _ => Err(EBADF),
    }
}

/// A described socket's or pipe's relay port (0: none).
fn port_at(d: &[u8]) -> u16 {
    d.get(2..4).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
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

/// A request to the app's host process on the current connection, and its answer.
fn ask(kind: u8, payload: &[u8]) -> Result<(u8, Vec<u8>), Errno> {
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let stream = c.as_mut().ok_or(EFAULT)?;
        send(stream, kind, payload).map_err(|_| EIO)?;
        receive(stream).map_err(|_| EIO)
    })
}

/// The app's memory, reached through the current connection.
pub struct RemoteMemory;

impl crate::guest::Remote for RemoteMemory {
    fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let mut req = addr.to_le_bytes().to_vec();
        req.extend_from_slice(&(len as u32).to_le_bytes());
        match ask(MEM_READ, &req)? {
            (MEM_DATA, data) if data.first() == Some(&1) => Ok(data[1..].to_vec()),
            _ => Err(EFAULT),
        }
    }

    fn write(&self, addr: u64, bytes: &[u8]) -> Result<(), Errno> {
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
            (FD_DESC, d) => open_described(&d).map(Arc::new),
            _ => Err(EBADF),
        }
    }
}

/// The stand-ins, by pid; an open's binder file, by token.
struct Server {
    sysroot: Arc<crate::vfs::Sysroot>,
    stand_ins: Mutex<HashMap<i32, Weak<Process>>>,
    opens: Mutex<HashMap<u64, (Arc<Process>, Arc<BinderFile>)>>,
    next: std::sync::atomic::AtomicU64,
}

/// Serve apps' binder on a local port: what `--binder-server` points an app's host process at.
///
/// # Errors
/// The port cannot be opened.
pub fn serve(sysroot: Arc<crate::vfs::Sysroot>) -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = Arc::new(Server { sysroot, stand_ins: Mutex::default(), opens: Mutex::default(), next: 1.into() });
    std::thread::Builder::new().name("binder-remote".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_nodelay(true);
            let server = Arc::clone(&server);
            let _ = std::thread::Builder::new().name("binder-remote-conn".into()).spawn(move || server.connection(stream));
        }
    })?;
    Ok(addr)
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
                let (context, pid, uid) = (context_of(body[0]), u32_at(&body, 1) as i32, u32_at(&body, 5));
                let p = self.stand_in(pid, uid);
                let file = BinderFile::open(context);
                let token = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.opens.lock().insert(token, (p, file));
                if send(&mut stream, OPENED, &token.to_le_bytes()).is_err() {
                    self.opens.lock().remove(&token);
                    return;
                }
                // The open lasts as long as this connection: its end is the close.
                let mut sink = [0u8; 1];
                let _ = stream.read(&mut sink);
                if let Some((_, file)) = self.opens.lock().remove(&token) {
                    file.release();
                }
            }
            PROPS => {
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
                let Some((p, file)) = self.opens.lock().get(&token).cloned() else { return };
                let mut task = Task::new(tid, Arc::clone(&p));
                CURRENT.with(|c| *c.borrow_mut() = stream.try_clone().ok());
                loop {
                    let Ok((kind, body)) = receive(&mut stream) else { break };
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
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The app's side.

static SERVER: OnceLock<String> = OnceLock::new();

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
    send(&mut s, PROPS, &[]).map_err(|_| EIO)?;
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
    pub fn open(p: &Process, context: Context) -> Result<Arc<Self>, Errno> {
        let addr = SERVER.get().ok_or(EIO)?;
        let mut control = TcpStream::connect(addr).map_err(|_| EIO)?;
        let _ = control.set_nodelay(true);
        let mut req = vec![context_byte(context)];
        req.extend_from_slice(&(p.sys.pid as u32).to_le_bytes());
        req.extend_from_slice(&p.sys.uid().to_le_bytes());
        send(&mut control, OPEN, &req).map_err(|_| EIO)?;
        let (kind, body) = receive(&mut control).map_err(|_| EIO)?;
        if kind != OPENED {
            return Err(EIO);
        }
        Ok(Arc::new(Self { token: u64_at(&body, 0), _control: control, threads: Mutex::default() }))
    }

    fn thread(&self, tid: i32) -> Result<Arc<Mutex<TcpStream>>, Errno> {
        if let Some(s) = self.threads.lock().get(&tid) {
            return Ok(Arc::clone(s));
        }
        let mut s = TcpStream::connect(SERVER.get().ok_or(EIO)?).map_err(|e| lost("connect", &e))?;
        let _ = s.set_nodelay(true);
        let mut req = self.token.to_le_bytes().to_vec();
        req.extend_from_slice(&(tid as u32).to_le_bytes());
        send(&mut s, ATTACH, &req).map_err(|e| lost("attach", &e))?;
        let s = Arc::new(Mutex::new(s));
        self.threads.lock().insert(tid, Arc::clone(&s));
        Ok(s)
    }

    /// Send a request and answer the system's reads, writes and descriptor requests until its
    /// result comes.
    fn call(&self, p: &Process, t: &Task, kind: u8, payload: &[u8]) -> SysResult {
        let conn = self.thread(t.tid)?;
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
                    let r = open_described(&body).and_then(|f| p.fds.insert(Arc::new(f), false, 0));
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

    /// A file's descriptor crosses to another host process as the same host file, with its access
    /// and offset: the WebView hands its service a `ParcelFileDescriptor` of the app's own data
    /// (`app_webview/variations_seed_new`), and a refused descriptor failed the whole transaction.
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
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789!", "writable, and the same file");
        let _ = std::fs::remove_file(&path);
    }
}
