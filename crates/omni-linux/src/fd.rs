//! Descriptors and the file syscalls.
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::errno::*;
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};
use crate::vfs::{ino_of, DevNode, DirEnt, Node, Resolved, Vfs};

pub const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_EMPTY_PATH: u64 = 0x1000;
const O_ACCMODE: u32 = 3;
const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_DIRECTORY: u32 = 0o40000;
const O_NOFOLLOW: u32 = 0o100000;
const O_CLOEXEC: u32 = 0o2000000;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFMT: u32 = 0o170000;
/// The name an ashmem region's `Shm` is made with (a memfd's is its own).
const ASHMEM: &str = "/dev/ashmem";
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const PATH_MAX: usize = 4096;

#[derive(Clone)]
pub enum Output {
    Host,
    Capture(Arc<Mutex<Vec<u8>>>),
}

pub enum FileKind {
    /// A sysroot file (read-only) or a writable-mount file.
    Host { file: std::fs::File, guest: Vec<u8>, sysroot: bool },
    Dir { dir: Resolved, entries: Option<Vec<DirEnt>>, next: usize },
    Dev(DevNode),
    Stdin,
    Stdout(Output),
    Stderr(Output),
    /// A generated file (`/proc`, `/sys`): its bytes, taken when it was opened.
    Synth { data: Vec<u8>, guest: Vec<u8>, pos: usize, sized: bool },
    Socket(crate::socket::Socket),
    Pipe(crate::pipe::End),
    EventFd(Arc<crate::poll::EventFd>),
    TimerFd(Arc<crate::poll::TimerFd>),
    Epoll(Arc<crate::poll::Epoll>),
    Binder(Arc<crate::binder::BinderFile>),
    /// An open of `/dev/omni-gpu`.
    Gpu(Arc<crate::gpu::Gpu>),
    /// A fence (`crate::sync_file`).
    SyncFile(Arc<crate::sync_file::SyncFile>),
    /// A shared-memory region (`memfd_create`, `/dev/ashmem`).
    Shared(Arc<crate::shm::Shm>),
    /// An inotify instance: watches are accepted, no event is ever reported (nothing here changes
    /// a watched path behind the app's back). The `AtomicI32` is the next watch descriptor.
    Inotify(Arc<std::sync::atomic::AtomicI32>),
    /// A BPF map, program or link (`crate::bpf`).
    Bpf(crate::bpf::Object),
    /// `/dev/binder` whose driver is in the system's host process (`crate::remote`).
    RemoteBinder(Arc<crate::remote::RemoteBinder>),
}

impl FileKind {
    /// Where a standard output or error descriptor writes.
    #[must_use]
    pub fn output(&self) -> Option<Output> {
        match self {
            Self::Stdout(o) | Self::Stderr(o) => Some(o.clone()),
            _ => None,
        }
    }
}

pub struct OpenFile {
    pub kind: Mutex<FileKind>,
    pub flags: Mutex<u32>,
}

impl Drop for OpenFile {
    /// Closed for the last time: its open-file-description locks go.
    fn drop(&mut self) {
        crate::locks::released(self);
    }
}

pub struct FdTable {
    fds: Mutex<std::collections::BTreeMap<i32, (Arc<OpenFile>, bool)>>,
    /// Set for a stand-in: its descriptors are another host process's (`crate::remote`).
    remote: std::sync::OnceLock<Arc<dyn RemoteFds>>,
}

/// Descriptors that are another host process's.
pub trait RemoteFds: Send + Sync {
    fn insert(&self, file: Arc<OpenFile>) -> Result<i32, Errno>;
    fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno>;
}

impl FdTable {
    /// A fork child's table: the same open files at the same numbers, close-on-exec kept.
    #[must_use]
    pub fn for_fork(&self) -> Self {
        Self { fds: Mutex::new(self.fds.lock().clone()), remote: std::sync::OnceLock::new() }
    }

    /// The table a program `execve` loads starts with: every descriptor not close-on-exec.
    #[must_use]
    pub fn for_exec(&self) -> Self {
        Self { fds: Mutex::new(self.fds.lock().iter().filter(|(_, (_, cloexec))| !cloexec).map(|(n, e)| (*n, e.clone())).collect()), remote: std::sync::OnceLock::new() }
    }

    #[must_use]
    pub fn standard(stdout: Output, stderr: Output) -> Self {
        let file = |kind| Arc::new(OpenFile { kind: Mutex::new(kind), flags: Mutex::new(0) });
        let mut fds = std::collections::BTreeMap::new();
        fds.insert(0, (file(FileKind::Stdin), false));
        fds.insert(1, (file(FileKind::Stdout(stdout)), false));
        fds.insert(2, (file(FileKind::Stderr(stderr)), false));
        Self { fds: Mutex::new(fds), remote: std::sync::OnceLock::new() }
    }

    /// Every open descriptor, lowest first.
    #[must_use]
    pub fn list(&self) -> Vec<(i32, Arc<OpenFile>)> {
        self.fds.lock().iter().map(|(fd, (f, _))| (*fd, Arc::clone(f))).collect()
    }

    /// Make this a stand-in's table: descriptors are got from and put in `remote`.
    pub fn set_remote(&self, remote: Arc<dyn RemoteFds>) {
        let _ = self.remote.set(remote);
    }

    pub fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        if let Some(r) = self.remote.get() {
            return r.get(fd);
        }
        self.fds.lock().get(&fd).map(|(f, _)| Arc::clone(f)).ok_or(EBADF)
    }

    /// The lowest free descriptor at or above `min`.
    pub fn insert(&self, file: Arc<OpenFile>, cloexec: bool, min: i32) -> Result<i32, Errno> {
        if let Some(r) = self.remote.get() {
            return r.insert(file);
        }
        let mut fds = self.fds.lock();
        let mut fd = min;
        while fds.contains_key(&fd) {
            fd += 1;
        }
        if fd >= 32768 {
            return Err(EMFILE);
        }
        fds.insert(fd, (file, cloexec));
        Ok(fd)
    }

    pub fn place(&self, fd: i32, file: Arc<OpenFile>, cloexec: bool) {
        self.fds.lock().insert(fd, (file, cloexec));
    }

    pub fn remove(&self, fd: i32) -> Result<(), Errno> {
        self.fds.lock().remove(&fd).map(|_| ()).ok_or(EBADF)
    }

    /// Close every descriptor, as a process's exit does (a pipe's reader then sees its end).
    pub fn close_all(&self) {
        let all = std::mem::take(&mut *self.fds.lock());
        drop(all);
    }

    pub fn cloexec(&self, fd: i32) -> Result<bool, Errno> {
        self.fds.lock().get(&fd).map(|(_, c)| *c).ok_or(EBADF)
    }

    pub fn set_cloexec(&self, fd: i32, on: bool) -> Result<(), Errno> {
        self.fds.lock().get_mut(&fd).map(|e| e.1 = on).ok_or(EBADF)
    }
}

pub fn open(vfs: &Vfs, cwd: &[u8], path: &[u8], flags: u32) -> Result<OpenFile, Errno> {
    open_by(None, vfs, cwd, path, flags)
}

/// `open`, by process `opener` (what a device that knows its opener needs: remote binder).
pub fn open_by(opener: Option<&Process>, vfs: &Vfs, cwd: &[u8], path: &[u8], flags: u32) -> Result<OpenFile, Errno> {
    let r = vfs.resolve(cwd, path, flags & O_NOFOLLOW == 0)?;
    let write = flags & O_ACCMODE != 0;
    let kind = match r.node.clone() {
        Node::Missing { .. } if flags & O_CREAT == 0 => return Err(ENOENT),
        Node::Missing { host: Some(host), parent_is_dir: true } => {
            let file = match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&host) {
                Ok(file) => file,
                // Created meanwhile by another task (two threads opening one new database): with
                // O_EXCL that is EEXIST; a plain O_CREAT opens the file that is there now.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if flags & O_EXCL != 0 {
                        return Err(EEXIST);
                    }
                    return open_by(opener, vfs, cwd, path, flags & !O_CREAT);
                }
                Err(_) => return Err(EACCES),
            };
            FileKind::Host { file, guest: r.path.clone(), sysroot: false }
        }
        Node::Missing { host: None, .. } => return Err(if flags & O_CREAT != 0 { EROFS } else { ENOENT }),
        Node::Missing { .. } => return Err(ENOENT),
        _ if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
        Node::Dir | Node::HostDir { .. } => {
            if write {
                return Err(EISDIR);
            }
            FileKind::Dir { dir: r.clone(), entries: None, next: 0 }
        }
        _ if flags & O_DIRECTORY != 0 => return Err(ENOTDIR),
        Node::SysFile { .. } => {
            if write || flags & O_TRUNC != 0 {
                return Err(EROFS);
            }
            let host = vfs.sysroot().host_path(&r.path).ok_or(EIO)?;
            let file = std::fs::File::open(host).map_err(|_| EIO)?;
            FileKind::Host { file, guest: r.path.clone(), sysroot: true }
        }
        Node::HostFile { host } => {
            let file = std::fs::OpenOptions::new()
                .read(flags & O_ACCMODE != 1)
                .write(write)
                .append(flags & O_APPEND != 0)
                .truncate(flags & O_TRUNC != 0 && write)
                .open(&host)
                .map_err(|_| EACCES)?;
            FileKind::Host { file, guest: r.path.clone(), sysroot: false }
        }
        Node::Dev(DevNode::Ashmem) => FileKind::Shared(crate::shm::Shm::create(ASHMEM)?),
        Node::Dev(d @ (DevNode::Binder | DevNode::HwBinder | DevNode::VndBinder)) => {
            let context = match d {
                DevNode::HwBinder => crate::binder::Context::HwBinder,
                DevNode::VndBinder => crate::binder::Context::VndBinder,
                _ => crate::binder::Context::Binder,
            };
            match opener {
                // The driver is the system's, in its host process.
                Some(p) if crate::remote::is_remote() => FileKind::RemoteBinder(crate::remote::RemoteBinder::open(p, context)?),
                _ => FileKind::Binder(crate::binder::BinderFile::open(context)),
            }
        }
        Node::Dev(DevNode::OmniGpu) => FileKind::Gpu(crate::gpu::Gpu::open()),
        Node::Dev(d) => FileKind::Dev(d),
        Node::Generated | Node::Blob { .. } => {
            if write && !crate::procfs::is_settable_attr(&r.path) {
                return Err(EACCES);
            }
            let data = vfs.read_generated(&r.path).ok_or(ENOENT)?;
            let sized = matches!(r.node, Node::Blob { .. });
            FileKind::Synth { data, guest: r.path.clone(), pos: 0, sized }
        }
        Node::Symlink { .. } => return Err(ELOOP), // O_NOFOLLOW on a link
    };
    Ok(OpenFile { kind: Mutex::new(kind), flags: Mutex::new(flags & !O_CLOEXEC) })
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stat {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub rdev: u64,
    pub size: i64,
    pub blocks: i64,
    pub mtime: i64,
}

impl Stat {
    /// arm64 `struct stat` (asm-generic, 128 bytes).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..8].copy_from_slice(&0x803u64.to_le_bytes()); // st_dev
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..20].copy_from_slice(&self.mode.to_le_bytes());
        b[20..24].copy_from_slice(&self.nlink.to_le_bytes());
        b[24..28].copy_from_slice(&self.uid.to_le_bytes());
        b[28..32].copy_from_slice(&self.gid.to_le_bytes());
        b[32..40].copy_from_slice(&self.rdev.to_le_bytes());
        b[48..56].copy_from_slice(&self.size.to_le_bytes());
        b[56..60].copy_from_slice(&4096i32.to_le_bytes()); // st_blksize
        b[64..72].copy_from_slice(&self.blocks.to_le_bytes());
        for at in [72, 88, 104] {
            b[at..at + 8].copy_from_slice(&self.mtime.to_le_bytes());
        }
        b
    }
}

/// A writable-mount file's permission bits: the host keeps one permission for us, read-only, and
/// `chmod` without write bits sets it (Android refuses to load a writable dex file).
fn host_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() { 0o444 } else { 0o600 }
}

/// A writable-mount file's owner and mode, where one is recorded (`crate::owners`).
fn owned(vfs: &Vfs, r: &Resolved, st: Stat) -> Stat {
    let (Node::HostFile { host } | Node::HostDir { host }) = &r.node else { return st };
    match vfs.owners().get(host) {
        Some(o) => Stat { mode: (st.mode & S_IFMT) | (o.mode & 0o7777), uid: o.uid, gid: o.gid, ..st },
        None => st,
    }
}

fn stat_resolved(vfs: &Vfs, r: &Resolved) -> Result<Stat, Errno> {
    let st = stat_node(r)?;
    // The BPF filesystem keeps its own modes and owners.
    if let Some((mode, uid, gid)) = crate::bpf::owner(&r.path).filter(|_| crate::bpf::on_bpffs(&r.path)) {
        return Ok(Stat { mode: (st.mode & S_IFMT) | mode, uid, gid, ..st });
    }
    // An image file or directory: the image's owner and mode.
    if matches!(r.node, Node::Dir | Node::SysFile { .. }) {
        if let Some(m) = vfs.sysroot().image_meta(&r.path) {
            return Ok(Stat { mode: (st.mode & S_IFMT) | m.mode, uid: m.uid, gid: m.gid, ..st });
        }
    }
    Ok(owned(vfs, r, st))
}

fn stat_node(r: &Resolved) -> Result<Stat, Errno> {
    let ino = ino_of(&r.path);
    let s = |mode: u32, size: i64| Stat { ino, mode, nlink: 1, size, blocks: (size + 511) / 512, ..Stat::default() };
    Ok(match &r.node {
        Node::Dir | Node::HostDir { .. } => Stat { nlink: 2, ..s(S_IFDIR | 0o755, 4096) },
        Node::SysFile { size, mode } => s(S_IFREG | mode, *size as i64),
        Node::HostFile { host } => {
            let meta = std::fs::metadata(host).map_err(|_| EIO)?;
            s(S_IFREG | host_mode(&meta), meta.len() as i64)
        }
        Node::Symlink { target } => s(S_IFLNK | 0o777, target.len() as i64),
        Node::Generated => s(S_IFREG | 0o444, 0),
        Node::Blob { size } => s(S_IFREG | 0o444, *size as i64),
        Node::Dev(d) => Stat { rdev: match d { DevNode::Null => 0x103, DevNode::Zero => 0x105, DevNode::Random => 0x108, DevNode::Urandom => 0x109, DevNode::Binder => 0xa3_00, DevNode::HwBinder => 0xa3_01, DevNode::VndBinder => 0xa3_02, DevNode::Kmsg => 0x10b, DevNode::Ashmem => 0x1_0b, DevNode::OmniGpu => 0xe2_00 }, ..s(S_IFCHR | 0o666, 0) },
        Node::Missing { .. } => return Err(ENOENT),
    })
}

pub fn stat_path(vfs: &Vfs, cwd: &[u8], path: &[u8], follow: bool) -> Result<Stat, Errno> {
    stat_resolved(vfs, &vfs.resolve(cwd, path, follow)?)
}

pub fn stat_of(vfs: &Vfs, file: &OpenFile) -> Result<Stat, Errno> {
    match &*file.kind.lock() {
        FileKind::Host { file, guest, sysroot } => {
            let meta = file.metadata().map_err(|_| EIO)?;
            let len = meta.len() as i64;
            let mode = if *sysroot { 0o644 } else { host_mode(&meta) };
            let st = Stat { ino: ino_of(guest), mode: S_IFREG | mode, nlink: 1, size: len, blocks: (len + 511) / 512, ..Stat::default() };
            if *sysroot {
                return Ok(match vfs.sysroot().image_meta(guest) {
                    Some(m) => Stat { mode: S_IFREG | m.mode, uid: m.uid, gid: m.gid, ..st },
                    None => st,
                });
            }
            // The file's owner, by where it is now (a renamed file is found by its new name).
            Ok(vfs.resolve(b"/", guest, true).map_or(st, |r| owned(vfs, &r, st)))
        }
        FileKind::Dir { dir, .. } => stat_resolved(vfs, dir),
        FileKind::Dev(d) => stat_node(&Resolved { path: b"/dev/null".to_vec(), node: Node::Dev(*d) }),
        FileKind::Stdin | FileKind::Stdout(_) | FileKind::Stderr(_) => {
            Ok(Stat { ino: 1, mode: S_IFCHR | 0o620, nlink: 1, rdev: 0x8800, ..Stat::default() })
        }
        FileKind::Synth { guest, data, sized: true, .. } => {
            stat_node(&Resolved { path: guest.clone(), node: Node::Blob { size: data.len() as u64 } })
        }
        FileKind::Synth { guest, .. } => stat_node(&Resolved { path: guest.clone(), node: Node::Generated }),
        FileKind::Socket(_) => Ok(Stat { ino: 2, mode: 0o140000 | 0o777, nlink: 1, ..Stat::default() }),
        FileKind::Pipe(_) => Ok(Stat { ino: 3, mode: 0o010000 | 0o600, nlink: 1, ..Stat::default() }),
        // anon_inode descriptors: a 0600 inode, as the kernel reports them.
        FileKind::EventFd(_) | FileKind::TimerFd(_) | FileKind::Epoll(_) => Ok(Stat { ino: 4, mode: 0o600, nlink: 1, ..Stat::default() }),
        FileKind::Binder(_) => stat_node(&Resolved { path: b"/dev/binder".to_vec(), node: Node::Dev(DevNode::Binder) }),
        FileKind::Gpu(_) => stat_node(&Resolved { path: b"/dev/omni-gpu".to_vec(), node: Node::Dev(DevNode::OmniGpu) }),
        FileKind::SyncFile(_) => Ok(Stat { ino: 7, mode: 0o600, nlink: 1, ..Stat::default() }),
        // An ashmem region is the ashmem device's descriptor: a character device, with the device's
        // number (libcutils tells ashmem from anything else by it). A memfd is a regular file.
        FileKind::Shared(m) if m.name == ASHMEM => stat_node(&Resolved { path: ASHMEM.as_bytes().to_vec(), node: Node::Dev(DevNode::Ashmem) }),
        FileKind::Shared(m) => Ok(Stat { ino: 5, mode: S_IFREG | 0o600, nlink: 1, size: m.len() as i64, blocks: (m.len() as i64 + 511) / 512, ..Stat::default() }),
        FileKind::Inotify(_) => Ok(Stat { ino: 6, mode: 0o600, nlink: 1, ..Stat::default() }),
        FileKind::Bpf(_) => Ok(Stat { ino: 8, mode: 0o600, nlink: 1, ..Stat::default() }),
        FileKind::RemoteBinder(_) => stat_node(&Resolved { path: b"/dev/binder".to_vec(), node: Node::Dev(DevNode::Binder) }),
    }
}

fn read_file(file: &OpenFile, buf: &mut [u8], at: Option<u64>) -> Result<usize, Errno> {
    match &mut *file.kind.lock() {
        FileKind::Host { file, .. } => match at {
            Some(off) => {
                let keep = file.stream_position().map_err(|_| EIO)?;
                file.seek(SeekFrom::Start(off)).map_err(|_| EIO)?;
                let n = file.read(buf).map_err(|_| EIO);
                file.seek(SeekFrom::Start(keep)).map_err(|_| EIO)?;
                n
            }
            None => file.read(buf).map_err(|_| EIO),
        },
        FileKind::Dev(DevNode::Null) | FileKind::Stdin => Ok(0),
        FileKind::Dev(DevNode::Zero) => {
            buf.fill(0);
            Ok(buf.len())
        }
        FileKind::Dev(DevNode::Random | DevNode::Urandom) => {
            omni_platform::process::random_bytes(buf).map_err(|_| EIO)?;
            Ok(buf.len())
        }
        FileKind::Dir { .. } => Err(EISDIR),
        FileKind::Stdout(_) | FileKind::Stderr(_) => Err(EBADF),
        FileKind::Socket(s) => crate::socket::receive(s, buf),
        // Pipes are read by `sys_read`/`sys_readv` without this lock held (they may wait).
        FileKind::Pipe(_) | FileKind::EventFd(_) | FileKind::TimerFd(_) | FileKind::Epoll(_) => Err(ESPIPE),
        FileKind::Dev(DevNode::Binder | DevNode::HwBinder | DevNode::VndBinder | DevNode::OmniGpu) | FileKind::Binder(_) | FileKind::Gpu(_) | FileKind::SyncFile(_) => Err(EINVAL),
        FileKind::Dev(DevNode::Kmsg | DevNode::Ashmem) => Err(EAGAIN),
        FileKind::Shared(m) => match at {
            Some(off) => m.read_at(buf, off),
            None => m.read_seq(buf),
        },
        FileKind::Inotify(_) => Err(EAGAIN), // no event is ever ready
        FileKind::Bpf(_) | FileKind::RemoteBinder(_) => Err(EINVAL),

        FileKind::Synth { data, pos, .. } => {
            let from = at.map_or(*pos, |o| usize::try_from(o).unwrap_or(usize::MAX)).min(data.len());
            let n = buf.len().min(data.len() - from);
            buf[..n].copy_from_slice(&data[from..from + n]);
            if at.is_none() {
                *pos = from + n;
            }
            Ok(n)
        }
    }
}

/// Read up to `buf.len()` bytes at `offset` without moving the descriptor's position.
pub fn pread_all(file: &OpenFile, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    let mut done = 0;
    while done < buf.len() {
        let n = read_file(file, &mut buf[done..], Some(offset + done as u64))?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

fn write_file(file: &OpenFile, bytes: &[u8]) -> Result<usize, Errno> {
    let sink = |out: &Output, host: &mut dyn Write| match out {
        Output::Host => host.write_all(bytes).and_then(|()| host.flush()).map(|()| bytes.len()).map_err(|_| EIO),
        Output::Capture(buf) => {
            buf.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
    };
    match &mut *file.kind.lock() {
        FileKind::Stdout(out) => sink(out, &mut std::io::stdout()),
        FileKind::Stderr(out) => sink(out, &mut std::io::stderr()),
        FileKind::Host { file, sysroot: false, .. } => file.write(bytes).map_err(|_| EIO),
        FileKind::Host { .. } | FileKind::Stdin => Err(EBADF),
        FileKind::Dev(DevNode::Kmsg) => {
            // One record per write, "<level>tag: message", shown as the kernel log would be.
            eprintln!("K/{}", String::from_utf8_lossy(bytes).trim_end());
            Ok(bytes.len())
        }
        FileKind::Dev(_) => Ok(bytes.len()),
        FileKind::Dir { .. } => Err(EISDIR),
        FileKind::Shared(m) => m.write_seq(bytes),
        FileKind::Inotify(_) => Err(EBADF),
        FileKind::Bpf(_) | FileKind::RemoteBinder(_) => Err(EINVAL),
        FileKind::Synth { guest, data, pos, .. } => {
            let written = crate::procfs::write_generated(guest, bytes, data)?;
            *pos = 0;
            Ok(written)
        }
        FileKind::Socket(s) => crate::socket::send(s, bytes),
        FileKind::Pipe(_) | FileKind::EventFd(_) | FileKind::TimerFd(_) | FileKind::Epoll(_) => Err(ESPIPE),
        FileKind::Binder(_) | FileKind::Gpu(_) | FileKind::SyncFile(_) => Err(EINVAL),
    }
}

fn fd_arg(a: u64) -> i32 {
    a as i64 as i32
}

fn path_arg(p: &Process, a: u64) -> Result<Vec<u8>, Errno> {
    p.mem.read_cstr(a, PATH_MAX)
}

/// The directory a `*at` call's relative path is resolved against.
fn base_dir(p: &Process, dirfd: u64, path: &[u8]) -> Result<Vec<u8>, Errno> {
    // A descriptor is an `int`: the register's upper half is not part of it.
    if path.first() == Some(&b'/') || i64::from(fd_arg(dirfd)) == AT_FDCWD {
        return Ok(p.cwd.lock().clone());
    }
    match &*p.fds.get(fd_arg(dirfd))?.kind.lock() {
        FileKind::Dir { dir, .. } => Ok(dir.path.clone()),
        _ => Err(ENOTDIR),
    }
}

/// `memfd_create(name, flags)`: an anonymous shared-memory region. `MFD_CLOEXEC` (bit 0) is
/// honoured; the sealing and huge-page flags are accepted and have no effect here.
fn sys_inotify_init1(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = Arc::new(OpenFile { kind: Mutex::new(FileKind::Inotify(Arc::default())), flags: Mutex::new(a[0] as u32 & 0o4000) });
    Ok(p.fds.insert(file, a[0] & 0o2000000 != 0, 0)? as u64)
}

fn sys_inotify_add_watch(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    match &*p.fds.get(fd_arg(a[0]))?.kind.lock() {
        FileKind::Inotify(next) => Ok(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed).max(1) as u64),
        _ => Err(EINVAL),
    }
}

fn sys_inotify_rm_watch(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    match &*p.fds.get(fd_arg(a[0]))?.kind.lock() {
        FileKind::Inotify(_) => Ok(0),
        _ => Err(EINVAL),
    }
}

fn sys_memfd_create(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let name = p.mem.read_cstr(a[0], 249)?;
    let m = crate::shm::Shm::create(&String::from_utf8_lossy(&name))?;
    let file = Arc::new(OpenFile { kind: Mutex::new(FileKind::Shared(m)), flags: Mutex::new(2) });
    Ok(p.fds.insert(file, a[1] & 1 != 0, 0)? as u64)
}

/// `/dev/ashmem`'s ioctls (libcutils' `ashmem-dev` when it has no memfd): set the name, the size,
/// and the protection mask; report the size; pin/unpin are no-ops (nothing is purged here).
fn ashmem_ioctl(p: &Process, m: &Arc<crate::shm::Shm>, cmd: u64, arg: u64) -> SysResult {
    const NAME_LEN: u64 = 256;
    match cmd & 0xffff {
        0x7701 => Ok(0),                                          // ASHMEM_SET_NAME (name kept from create)
        0x7702 => p.mem.write(arg, &[0u8; 256]).map(|()| 0),      // ASHMEM_GET_NAME
        0x7703 => m.set_len(arg).map(|()| 0),                     // ASHMEM_SET_SIZE
        0x7704 => Ok(m.len()),                                    // ASHMEM_GET_SIZE
        0x7705 => {                                               // ASHMEM_SET_PROT_MASK
            m.prot_mask.store(arg, std::sync::atomic::Ordering::SeqCst);
            Ok(0)
        }
        0x7706 => Ok(m.prot_mask.load(std::sync::atomic::Ordering::SeqCst)), // ASHMEM_GET_PROT_MASK
        0x7707 | 0x7708 => Ok(0),                                 // ASHMEM_PIN / UNPIN
        0x7709 => Ok(0),                                          // ASHMEM_GET_PIN_STATUS: unpurged
        0x770a => Ok(0),                                          // ASHMEM_PURGE_ALL_CACHES
        _ => {
            let _ = NAME_LEN;
            Err(ENOTTY)
        }
    }
}

fn sys_openat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    let flags = a[2] as u32;
    let creating = flags & O_CREAT != 0 && matches!(p.vfs.resolve(&base, &path, flags & O_NOFOLLOW == 0).map(|r| r.node), Ok(Node::Missing { .. }));
    let file = open_by(Some(p), &p.vfs, &base, &path, flags)?;
    if creating {
        // The new file is its creator's, with the mode it asked for less its umask.
        if let Ok(Node::HostFile { host }) = p.vfs.resolve(&base, &path, true).map(|r| r.node) {
            created(p, &host, a[3] as u32);
        }
    }
    Ok(p.fds.insert(Arc::new(file), flags & O_CLOEXEC != 0, 0)? as u64)
}

fn sys_close(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    p.fds.remove(fd_arg(a[0]))?;
    crate::locks::closed(p, &file);
    Ok(0)
}

fn sys_read(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let mut buf = vec![0u8; (a[2] as usize).min(1 << 24)];
    let n = match crate::pipe::end_of(&file) {
        Some((_, true, _)) => return Err(EBADF),
        Some((pipe, false, nonblocking)) => crate::pipe::read(&pipe, &mut buf, nonblocking, t)?,
        None => match crate::poll::read(&file, &mut buf, t).or_else(|| crate::socket::read(&file, &mut buf, t)) {
            Some(r) => r?,
            None => read_file(&file, &mut buf, None)?,
        },
    };
    p.mem.write(a[1], &buf[..n])?;
    Ok(n as u64)
}

fn sys_pread64(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let mut buf = vec![0u8; (a[2] as usize).min(1 << 24)];
    let n = read_file(&file, &mut buf, Some(a[3]))?;
    p.mem.write(a[1], &buf[..n])?;
    Ok(n as u64)
}

/// A write at `offset`, the file's position left where it was (`pwrite`): a writable-mount file or
/// shared memory; anything without positions is `ESPIPE`.
fn write_file_at(file: &OpenFile, bytes: &[u8], offset: u64) -> Result<usize, Errno> {
    match &mut *file.kind.lock() {
        FileKind::Host { file, sysroot: false, .. } => {
            let keep = file.stream_position().map_err(|_| EIO)?;
            file.seek(SeekFrom::Start(offset)).map_err(|_| EIO)?;
            let n = file.write_all(bytes).map(|()| bytes.len()).map_err(|_| EIO);
            file.seek(SeekFrom::Start(keep)).map_err(|_| EIO)?;
            n
        }
        FileKind::Host { .. } | FileKind::Synth { .. } => Err(EBADF),
        FileKind::Shared(m) => m.write_at(bytes, offset),
        FileKind::Dir { .. } => Err(EISDIR),
        FileKind::Dev(_) => Ok(bytes.len()),
        _ => Err(ESPIPE),
    }
}

fn sys_pwrite64(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    if (a[3] as i64) < 0 {
        return Err(EINVAL);
    }
    let bytes = p.mem.read(a[1], (a[2] as usize).min(1 << 24))?;
    Ok(write_file_at(&file, &bytes, a[3])? as u64)
}

/// `pwritev(fd, iov, iovcnt, offset)`: the buffers written one after another from `offset`.
fn sys_pwritev(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    const CAP: usize = 1 << 24;
    let mut bytes = Vec::new();
    for (base, len) in iovecs(p, a[1], a[2])? {
        let take = len.min(CAP - bytes.len());
        bytes.extend_from_slice(&p.mem.read(base, take)?);
        if bytes.len() == CAP {
            break;
        }
    }
    Ok(write_file_at(&file, &bytes, a[3])? as u64)
}

/// `preadv(fd, iov, iovcnt, offset)`: the buffers filled one after another from `offset`.
fn sys_preadv(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let mut total = 0u64;
    for (base, len) in iovecs(p, a[1], a[2])? {
        let mut buf = vec![0u8; len.min(1 << 24)];
        let n = read_file(&file, &mut buf, Some(a[3] + total))?;
        p.mem.write(base, &buf[..n])?;
        total += n as u64;
        if n < buf.len() {
            break;
        }
    }
    Ok(total)
}

/// `sendfile(out_fd, in_fd, offset, count)`: bytes copied from one descriptor to the other --
/// from `*offset` (updated, the input's position untouched) or the input's position.
fn sys_sendfile(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let (out, input) = (p.fds.get(fd_arg(a[0]))?, p.fds.get(fd_arg(a[1]))?);
    let offset = if a[2] == 0 { None } else { Some(p.mem.read_u64(a[2])?) };
    let mut buf = vec![0u8; (a[3] as usize).min(1 << 24)];
    let n = read_file(&input, &mut buf, offset)?;
    let written = match crate::pipe::end_of(&out) {
        Some((_, false, _)) => return Err(EBADF),
        Some((pipe, true, nonblocking)) => crate::pipe::write(&pipe, &buf[..n], nonblocking, t)?,
        None => write_file(&out, &buf[..n])?,
    };
    if let Some(at) = offset {
        p.mem.write_u64(a[2], at + written as u64)?;
    } else if written < n {
        // The input moved past what was not written: put it back.
        if let FileKind::Host { file, .. } = &mut *input.kind.lock() {
            let _ = file.seek(SeekFrom::Current(-((n - written) as i64)));
        }
    }
    Ok(written as u64)
}

fn sys_write(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let bytes = p.mem.read(a[1], (a[2] as usize).min(1 << 24))?;
    if let Some(r) = crate::poll::write(&file, &bytes, t) {
        return Ok(r? as u64);
    }
    match crate::pipe::end_of(&file) {
        Some((_, false, _)) => Err(EBADF),
        Some((pipe, true, nonblocking)) => Ok(crate::pipe::write(&pipe, &bytes, nonblocking, t)? as u64),
        None => Ok(write_file(&file, &bytes)? as u64),
    }
}

fn iovecs(p: &Process, at: u64, count: u64) -> Result<Vec<(u64, usize)>, Errno> {
    if count > 1024 {
        return Err(EINVAL);
    }
    (0..count).map(|i| Ok((p.mem.read_u64(at + i * 16)?, p.mem.read_u64(at + i * 16 + 8)? as usize))).collect()
}

fn sys_writev(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    // The same cap as `write`, on the total: many iovecs naming one large buffer must not make the
    // host allocate their sum (A2-A5 review, Important 3c).
    const CAP: usize = 1 << 24;
    let mut bytes = Vec::new();
    for (base, len) in iovecs(p, a[1], a[2])? {
        let take = len.min(CAP - bytes.len());
        bytes.extend_from_slice(&p.mem.read(base, take)?);
        if bytes.len() == CAP {
            break;
        }
    }
    match crate::pipe::end_of(&file) {
        Some((_, false, _)) => Err(EBADF),
        Some((pipe, true, nonblocking)) => Ok(crate::pipe::write(&pipe, &bytes, nonblocking, t)? as u64),
        None => Ok(write_file(&file, &bytes)? as u64),
    }
}

fn sys_readv(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let pipe = crate::pipe::end_of(&file);
    let mut total = 0u64;
    for (base, len) in iovecs(p, a[1], a[2])? {
        // The same cap as `read`: the length is the guest's, and an allocation of it can abort
        // the host (A1 review, Important 2).
        let len = len.min(1 << 24);
        let mut buf = vec![0u8; len];
        let n = match &pipe {
            Some((_, true, _)) => return Err(EBADF),
            // A pipe waits only for the first iovec; after that, what is there.
            Some((pipe, false, nonblocking)) => match crate::pipe::read(pipe, &mut buf, *nonblocking || total > 0, t) {
                Err(EAGAIN) if total > 0 => 0,
                r => r?,
            },
            None => read_file(&file, &mut buf, None)?,
        };
        p.mem.write(base, &buf[..n])?;
        total += n as u64;
        if n < len {
            break;
        }
    }
    Ok(total)
}

fn sys_lseek(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let mut kind = file.kind.lock();
    match &mut *kind {
        FileKind::Host { file, .. } => {
            let whence = match a[2] {
                0 => SeekFrom::Start(a[1]),
                1 => SeekFrom::Current(a[1] as i64),
                2 => SeekFrom::End(a[1] as i64),
                _ => return Err(EINVAL),
            };
            file.seek(whence).map_err(|_| EINVAL)
        }
        FileKind::Shared(m) => m.seek(a[2] as u32, a[1] as i64),
        FileKind::Dir { next, .. } if a[1] == 0 && a[2] == 0 => {
            *next = 0;
            Ok(0)
        }
        FileKind::Dev(_) => Ok(0),
        FileKind::Synth { data, pos, .. } => {
            let base = match a[2] {
                0 => 0i64,
                1 => *pos as i64,
                2 => data.len() as i64,
                _ => return Err(EINVAL),
            };
            let to = base.checked_add(a[1] as i64).filter(|t| *t >= 0).ok_or(EINVAL)?;
            *pos = to as usize;
            Ok(to as u64)
        }
        _ => Err(ESPIPE),
    }
}

fn sys_fstat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let st = stat_of(&p.vfs, &*p.fds.get(fd_arg(a[0]))?)?;
    p.mem.write(a[1], &st.to_bytes())?;
    Ok(0)
}

fn sys_newfstatat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[1])?;
    let st = if path.is_empty() {
        if a[3] & AT_EMPTY_PATH == 0 {
            return Err(ENOENT);
        }
        if i64::from(fd_arg(a[0])) == AT_FDCWD {
            stat_path(&p.vfs, &p.cwd.lock(), b".", true)?
        } else {
            stat_of(&p.vfs, &*p.fds.get(fd_arg(a[0]))?)?
        }
    } else {
        let base = base_dir(p, a[0], &path)?;
        stat_path(&p.vfs, &base, &path, a[3] & AT_SYMLINK_NOFOLLOW == 0)?
    };
    p.mem.write(a[2], &st.to_bytes())?;
    Ok(0)
}

/// The guest path an open descriptor names, as `/proc/self/fd/N` reports it.
pub(crate) fn guest_path_of(file: &OpenFile) -> Vec<u8> {
    match &*file.kind.lock() {
        FileKind::Host { guest, .. } | FileKind::Synth { guest, .. } => guest.clone(),
        FileKind::Dir { dir, .. } => dir.path.clone(),
        FileKind::Dev(DevNode::Null) => b"/dev/null".to_vec(),
        FileKind::Dev(DevNode::Zero) => b"/dev/zero".to_vec(),
        FileKind::Dev(DevNode::Random) => b"/dev/random".to_vec(),
        FileKind::Dev(DevNode::Urandom) => b"/dev/urandom".to_vec(),
        FileKind::Stdin | FileKind::Stdout(_) | FileKind::Stderr(_) => b"/dev/pts/0".to_vec(),
        FileKind::Socket(_) => b"socket:[2]".to_vec(),
        FileKind::Pipe(_) => b"pipe:[3]".to_vec(),
        FileKind::EventFd(_) => b"anon_inode:[eventfd]".to_vec(),
        FileKind::TimerFd(_) => b"anon_inode:[timerfd]".to_vec(),
        FileKind::Epoll(_) => b"anon_inode:[eventpoll]".to_vec(),
        FileKind::Dev(DevNode::Binder) | FileKind::Binder(_) => b"/dev/binder".to_vec(),
        FileKind::Dev(DevNode::HwBinder) => b"/dev/hwbinder".to_vec(),
        FileKind::Dev(DevNode::VndBinder) => b"/dev/vndbinder".to_vec(),
        FileKind::Dev(DevNode::Kmsg) => b"/dev/kmsg".to_vec(),
        FileKind::Dev(DevNode::Ashmem) => b"/dev/ashmem".to_vec(),
        FileKind::Dev(DevNode::OmniGpu) | FileKind::Gpu(_) => b"/dev/omni-gpu".to_vec(),
        FileKind::SyncFile(_) => b"anon_inode:sync_file".to_vec(),
        FileKind::Shared(m) if m.name == ASHMEM => ASHMEM.as_bytes().to_vec(),
        FileKind::Shared(m) => format!("/memfd:{} (deleted)", m.name).into_bytes(),
        FileKind::Inotify(_) => b"anon_inode:inotify".to_vec(),
        FileKind::Bpf(o) => o.describe().as_bytes().to_vec(),
        FileKind::RemoteBinder(_) => b"/dev/binder".to_vec(),
    }
}

fn sys_readlinkat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    match p.vfs.resolve(&base, &path, false)?.node {
        Node::Symlink { target } => {
            let n = target.len().min(a[3] as usize);
            p.mem.write(a[2], &target[..n])?;
            Ok(n as u64)
        }
        Node::Missing { .. } => Err(ENOENT),
        _ => Err(EINVAL),
    }
}

/// `faccessat(dirfd, path, mode, flags)`: existence, then the permission bits the caller's ids
/// select -- the owner's when it owns the file, the group's when it is in the file's group, the
/// others' otherwise; root reads and writes anything, and executes what has an execute bit. ART
/// refuses an app's dex file the app could write (it asks here).
fn sys_faccessat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const R: u64 = 4;
    const W: u64 = 2;
    const X: u64 = 1;
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    let r = p.vfs.resolve(&base, &path, true)?;
    let want = a[2] & (R | W | X);
    match r.node {
        Node::Missing { .. } => return Err(ENOENT),
        Node::SysFile { .. } | Node::Dir if want & W != 0 => return Err(EROFS),
        Node::HostFile { ref host } if want & W != 0 => {
            let meta = std::fs::metadata(host).map_err(|_| EIO)?;
            if meta.permissions().readonly() {
                return Err(EACCES);
            }
        }
        _ => {}
    }
    if want == 0 {
        return Ok(0);
    }
    let st = stat_resolved(&p.vfs, &r)?;
    let mode = u64::from(st.mode);
    let uid = p.sys.uid();
    if uid == 0 {
        let executable = mode & 0o111 != 0 || mode & u64::from(S_IFMT) == u64::from(S_IFDIR);
        return if want & X == 0 || executable { Ok(0) } else { Err(EACCES) };
    }
    let bits = if st.uid == uid {
        (mode >> 6) & 7
    } else if p.sys.in_group(st.gid) {
        (mode >> 3) & 7
    } else {
        mode & 7
    };
    if want & !bits == 0 { Ok(0) } else { Err(EACCES) }
}

fn sys_ioctl(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let remote = match &*file.kind.lock() {
        FileKind::RemoteBinder(b) => Some(Arc::clone(b)),
        _ => None,
    };
    if let Some(b) = remote {
        return b.ioctl(p, t, a[1], a[2]);
    }
    let binder = match &*file.kind.lock() {
        FileKind::Binder(b) => Some(Arc::clone(b)),
        _ => None,
    };
    if let Some(b) = binder {
        return crate::binder::ioctl(p, t, &b, a[1], a[2]);
    }
    let gpu = match &*file.kind.lock() {
        FileKind::Gpu(g) => Some(Arc::clone(g)),
        _ => None,
    };
    if let Some(g) = gpu {
        return crate::gpu::ioctl(p, t, &g, a[1], a[2]);
    }
    let fence = match &*file.kind.lock() {
        FileKind::SyncFile(f) => Some(Arc::clone(f)),
        _ => None,
    };
    if let Some(f) = fence {
        return crate::sync_file::ioctl(p, &f, a[1], a[2]);
    }
    let shared = match &*file.kind.lock() {
        FileKind::Shared(m) => Some(Arc::clone(m)),
        _ => None,
    };
    if let Some(m) = shared {
        return ashmem_ioctl(p, &m, a[1], a[2]);
    }
    match a[1] {
        // TCGETS, TIOCGWINSZ, TIOCGPGRP: nothing here is a terminal.
        0x5401 | 0x5413 | 0x540F => Err(ENOTTY),
        // FIONREAD: the bytes ready to read.
        0x541b => {
            let n = match crate::pipe::end_of(&file) {
                Some((pipe, _, _)) => crate::pipe::queued(&pipe),
                None => match &mut *file.kind.lock() {
                    FileKind::Socket(s) => crate::socket::available(s),
                    FileKind::Host { file, .. } => {
                        let at = file.stream_position().map_err(|_| EIO)?;
                        file.metadata().map_err(|_| EIO)?.len().saturating_sub(at) as usize
                    }
                    FileKind::Synth { data, pos, .. } => data.len().saturating_sub(*pos),
                    FileKind::Dir { .. } => return Err(EISDIR),
                    _ => 0,
                },
            };
            p.mem.write_u32(a[2], n as u32)?;
            Ok(0)
        }
        other => {
            p.refusals.record(format!("ioctl {other:#x}"), t.pc, t.lr);
            Err(ENOTTY)
        }
    }
}

fn sys_fcntl(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let fd = fd_arg(a[0]);
    let file = p.fds.get(fd)?;
    match a[1] {
        0 => Ok(p.fds.insert(file, false, a[2] as i32)? as u64),
        1030 => Ok(p.fds.insert(file, true, a[2] as i32)? as u64),
        1 => Ok(u64::from(p.fds.cloexec(fd)?)),
        2 => p.fds.set_cloexec(fd, a[2] & 1 != 0).map(|()| 0),
        3 => Ok(u64::from(*file.flags.lock())),
        // F_SETPIPE_SZ / F_GETPIPE_SZ: a pipe's capacity is fixed at 64 KiB here; a request is
        // answered with what it would round to, as a successful resize is.
        1031 | 1032 if crate::pipe::end_of(&file).is_some() => {
            Ok(if a[1] == 1031 { (a[2].clamp(4096, 1 << 20) + 4095) & !4095 } else { 64 << 10 })
        }
        // Record locks: F_GETLK, F_SETLK, F_SETLKW and the open-file-description forms.
        5..=7 | 36..=38 => crate::locks::fcntl(p, t, &file, a[1], a[2]),
        4 => {
            let mut f = file.flags.lock();
            *f = (*f & O_ACCMODE) | (a[2] as u32 & (O_APPEND | 0o4000));
            Ok(0)
        }
        other => {
            p.refusals.record(format!("fcntl cmd {other}"), t.pc, t.lr);
            Err(EINVAL)
        }
    }
}

fn sys_flock(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    crate::locks::flock(p, t, &file, a[1])
}

fn sys_dup(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    Ok(p.fds.insert(file, false, 0)? as u64)
}

fn sys_dup3(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let (old, new) = (fd_arg(a[0]), fd_arg(a[1]));
    if old == new {
        return Err(EINVAL);
    }
    let file = p.fds.get(old)?;
    p.fds.place(new, file, a[2] as u32 & O_CLOEXEC != 0);
    Ok(new as u64)
}

fn sys_getdents64(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let mut kind = file.kind.lock();
    let FileKind::Dir { dir, entries, next } = &mut *kind else { return Err(ENOTDIR) };
    if entries.is_none() {
        *entries = Some(p.vfs.list(dir)?);
    }
    let list = entries.as_ref().expect("listed");
    let mut out = Vec::new();
    while *next < list.len() {
        let e = &list[*next];
        let reclen = (19 + e.name.len() + 1 + 7) & !7;
        if out.len() + reclen > a[2] as usize {
            if out.is_empty() {
                return Err(EINVAL);
            }
            break;
        }
        let mut rec = vec![0u8; reclen];
        rec[0..8].copy_from_slice(&e.ino.to_le_bytes());
        rec[8..16].copy_from_slice(&((*next + 1) as i64).to_le_bytes());
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = e.kind;
        rec[19..19 + e.name.len()].copy_from_slice(&e.name);
        out.extend_from_slice(&rec);
        *next += 1;
    }
    p.mem.write(a[1], &out)?;
    Ok(out.len() as u64)
}

/// The host path a mutating call acts on, on a writable mount; the sysroot and the generated trees
/// are read-only.
fn writable_host(p: &Process, dirfd: u64, path: &[u8], follow: bool) -> Result<(Node, std::path::PathBuf), Errno> {
    let base = base_dir(p, dirfd, path)?;
    let r = p.vfs.resolve(&base, path, follow)?;
    let host = match &r.node {
        Node::HostFile { host } | Node::HostDir { host } | Node::Missing { host: Some(host), .. } => host.clone(),
        Node::Missing { host: None, parent_is_dir: true } => return Err(EROFS),
        Node::Missing { .. } => return Err(ENOENT),
        _ => return Err(EROFS),
    };
    Ok((r.node, host))
}

/// A file or directory `p` just made: its, with `mode` less the umask.
fn created(p: &Process, host: &std::path::Path, mode: u32) {
    let mode = mode & 0o7777 & !p.sys.umask();
    p.vfs.owners().set(host, crate::owners::Owner { uid: p.sys.uid(), gid: p.sys.gid(), mode });
}

fn sys_mkdirat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    let r = p.vfs.resolve(&base, &path, false)?;
    if crate::bpf::on_bpffs(&r.path) {
        return crate::bpf::mkdir(&r.path, a[2] as u32 & !p.sys.umask(), p.sys.uid(), p.sys.gid()).map(|()| 0);
    }
    match r.node {
        Node::Missing { host: Some(host), parent_is_dir: true } => {
            std::fs::create_dir(&host).map_err(|_| EACCES)?;
            created(p, &host, a[2] as u32);
            Ok(0)
        }
        Node::Missing { host: None, parent_is_dir: true } => Err(EROFS),
        Node::Missing { .. } => Err(ENOENT),
        _ => Err(EEXIST),
    }
}

/// `OMNI_FS_TRACE=1`: every file or directory removed or renamed, by which process -- what a
/// directory that vanished is traced back with.
fn fs_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_FS_TRACE").as_deref() == Ok("1"))
}

fn sys_unlinkat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const AT_REMOVEDIR: u64 = 0x200;
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    if let Ok(r) = p.vfs.resolve(&base, &path, false) {
        if crate::bpf::on_bpffs(&r.path) {
            return crate::bpf::remove(&r.path, a[2] & AT_REMOVEDIR != 0).map(|()| 0);
        }
    }
    let (node, host) = writable_host(p, a[0], &path, false)?;
    if fs_trace() {
        let comm = String::from_utf8_lossy(&p.comm.lock()).into_owned();
        eprintln!("[fs] {} ({comm}) {} {} -> {}", p.sys.pid, if a[2] & AT_REMOVEDIR != 0 { "rmdir" } else { "unlink" }, String::from_utf8_lossy(&path), host.display());
    }
    match (node, a[2] & AT_REMOVEDIR != 0) {
        (Node::Missing { .. }, _) => Err(ENOENT),
        (Node::HostDir { .. }, false) => Err(EISDIR),
        (Node::HostDir { .. }, true) => {
            std::fs::remove_dir(&host).map_err(|e| {
                if std::fs::read_dir(&host).is_ok_and(|mut d| d.next().is_some()) { ENOTEMPTY } else { let _ = e; EACCES }
            })?;
            p.vfs.owners().forget(&host);
            Ok(0)
        }
        (_, true) => Err(ENOTDIR),
        (_, false) => {
            std::fs::remove_file(&host).map_err(|_| EACCES)?;
            p.vfs.owners().forget(&host);
            Ok(0)
        }
    }
}

/// `renameat`/`renameat2` (flags 0 only) within the writable mounts.
fn sys_renameat2(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    const RENAME_NOREPLACE: u64 = 1;
    if a[4] & !RENAME_NOREPLACE != 0 {
        p.refusals.record(format!("renameat2 flags {:#x}", a[4]), t.pc, t.lr);
        return Err(EINVAL);
    }
    let noreplace = a[4] & RENAME_NOREPLACE != 0;
    let (from_path, to_path) = (path_arg(p, a[1])?, path_arg(p, a[3])?);
    // On the BPF filesystem: its own names (a loader pins under a temporary name, then renames).
    let from_r = p.vfs.resolve(&base_dir(p, a[0], &from_path)?, &from_path, false)?;
    if crate::bpf::on_bpffs(&from_r.path) {
        let to_r = p.vfs.resolve(&base_dir(p, a[2], &to_path)?, &to_path, false)?;
        return crate::bpf::rename(&from_r.path, &to_r.path, noreplace).map(|()| 0);
    }
    let (from_node, from) = writable_host(p, a[0], &from_path, false)?;
    if matches!(from_node, Node::Missing { .. }) {
        return Err(ENOENT);
    }
    let (to_node, to) = writable_host(p, a[2], &to_path, false)?;
    if noreplace && !matches!(to_node, Node::Missing { .. }) {
        return Err(EEXIST);
    }
    if fs_trace() {
        let comm = String::from_utf8_lossy(&p.comm.lock()).into_owned();
        eprintln!("[fs] {} ({comm}) rename {} -> {}", p.sys.pid, from.display(), to.display());
    }
    if std::fs::rename(&from, &to).is_err() {
        // A directory holding an open file: the host (Windows) will not rename it, where Linux
        // does. Its entries can each be renamed -- every host file here is opened with delete
        // sharing -- so the tree is moved entry by entry, open descriptors staying valid.
        if !from.is_dir() {
            return Err(EACCES);
        }
        move_tree(&from, &to).map_err(|_| EACCES)?;
    }
    p.vfs.owners().rename(&from, &to);
    Ok(0)
}

/// Move directory `from` to `to` (absent, or an empty directory it replaces): make `to`, rename
/// each entry into it (a directory by the same means when the host refuses), remove `from`.
fn move_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if to.is_dir() {
        std::fs::remove_dir(to)?;
    }
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if std::fs::rename(&src, &dst).is_err() {
            if entry.file_type()?.is_dir() {
                move_tree(&src, &dst)?;
            } else {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
        }
    }
    std::fs::remove_dir(from)
}

fn sys_renameat(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    sys_renameat2(p, t, [a[0], a[1], a[2], a[3], 0, 0])
}

fn sys_ftruncate(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    match &*p.fds.get(fd_arg(a[0]))?.kind.lock() {
        FileKind::Host { file, sysroot: false, .. } => file.set_len(a[1]).map(|()| 0).map_err(|_| EINVAL),
        FileKind::Shared(m) => m.set_len(a[1]).map(|()| 0),
        FileKind::Host { .. } | FileKind::Synth { .. } => Err(EROFS),
        _ => Err(EINVAL),
    }
}

fn sys_fsync(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if let FileKind::Host { file, sysroot: false, .. } = &*p.fds.get(fd_arg(a[0]))?.kind.lock() {
        file.sync_data().map_err(|_| EIO)?;
    }
    Ok(0)
}

/// `utimensat`: timestamps are accepted and not kept (nothing here reads them back as set).
/// `utimensat(dirfd, path, times, flags)`: a file's access and modification times. The
/// modification time of a host file named by path is set (the host keeps it); a missing file is
/// `ENOENT` --
/// `touch` creates the file only when told so.
fn sys_utimensat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    let host = if a[1] == 0 {
        // futimens: an open descriptor; its times are not kept.
        p.fds.get(fd_arg(a[0]))?;
        None
    } else {
        let path = path_arg(p, a[1])?;
        let base = base_dir(p, a[0], &path)?;
        match p.vfs.resolve(&base, &path, a[3] & AT_SYMLINK_NOFOLLOW == 0)?.node {
            Node::Missing { .. } => return Err(ENOENT),
            Node::HostFile { host } => Some(host),
            _ => None,
        }
    };
    let Some(host) = host else { return Ok(0) };
    let modified = if a[2] == 0 {
        Some(std::time::SystemTime::now())
    } else {
        let t = p.mem.read(a[2] + 16, 16)?;
        let sec = i64::from_le_bytes(t[..8].try_into().expect("8 bytes"));
        let nsec = i64::from_le_bytes(t[8..].try_into().expect("8 bytes"));
        match nsec {
            UTIME_OMIT => None,
            UTIME_NOW => Some(std::time::SystemTime::now()),
            _ if sec >= 0 => Some(std::time::UNIX_EPOCH + std::time::Duration::new(sec as u64, nsec as u32)),
            _ => None,
        }
    };
    if let Some(m) = modified {
        let file = std::fs::OpenOptions::new().write(true).open(&host).map_err(|_| EACCES)?;
        file.set_modified(m).map_err(|_| EIO)?;
    }
    Ok(0)
}

/// `fchmod`/`fchown`: a mode or owner the host cannot hold (Windows has neither), so a writable
/// file accepts it and keeps nothing; the sysroot is read-only.
/// Set a host file read-only exactly when `mode` grants no write permission.
fn set_host_mode(host: &std::path::Path, mode: u64) -> Result<(), Errno> {
    let meta = std::fs::metadata(host).map_err(|_| EIO)?;
    if !meta.is_file() {
        return Ok(()); // a directory keeps its permissions; nothing here checks them
    }
    let mut perms = meta.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(mode & 0o222 == 0);
    std::fs::set_permissions(host, perms).map_err(|_| EIO)
}

/// What an open descriptor names, resolved again by its path (for `fchmod`, `fchown`).
fn node_of(p: &Process, file: &OpenFile) -> Result<Node, Errno> {
    let guest = match &*file.kind.lock() {
        FileKind::Host { sysroot: true, .. } | FileKind::Synth { .. } => return Err(EROFS),
        FileKind::Host { guest, .. } => guest.clone(),
        FileKind::Dir { dir, .. } => dir.path.clone(),
        _ => return Ok(Node::Generated),
    };
    Ok(p.vfs.resolve(b"/", &guest, true)?.node)
}

/// `chmod` of a node: a writable-mount file's mode is recorded (and a file without write bits is
/// read-only on the host too: Android refuses to load a writable dex file); a device's is
/// accepted; the image is read-only.
fn chmod_node(p: &Process, node: Node, mode: u64) -> SysResult {
    let host = match node {
        Node::Missing { .. } => return Err(ENOENT),
        Node::HostFile { host } => {
            set_host_mode(&host, mode)?;
            host
        }
        Node::HostDir { host } => host,
        Node::Dev(_) | Node::Generated => return Ok(0),
        _ => return Err(EROFS),
    };
    let was = p.vfs.owners().get(&host).unwrap_or(crate::owners::Owner { uid: 0, gid: 0, mode: 0 });
    p.vfs.owners().set(&host, crate::owners::Owner { mode: mode as u32 & 0o7777, ..was });
    Ok(0)
}

/// `chown` of a node: root gives any owner; another user may only leave it as it is. `-1` keeps
/// that id.
fn chown_node(p: &Process, node: Node, uid: u64, gid: u64) -> SysResult {
    let host = match node {
        Node::Missing { .. } => return Err(ENOENT),
        Node::HostFile { host } | Node::HostDir { host } => host,
        Node::Dev(_) | Node::Generated => return Ok(0),
        _ => return Err(EROFS),
    };
    let was = p.vfs.owners().get(&host).unwrap_or(crate::owners::Owner { uid: 0, gid: 0, mode: 0o755 });
    let pick = |want: u64, old: u32| if want as u32 == u32::MAX { old } else { want as u32 };
    let now = crate::owners::Owner { uid: pick(uid, was.uid), gid: pick(gid, was.gid), ..was };
    if p.sys.uid() != 0 && (now.uid != was.uid || now.gid != was.gid) && !(was.uid == p.sys.uid() && now.uid == was.uid) {
        return Err(EPERM);
    }
    p.vfs.owners().set(&host, now);
    Ok(0)
}

fn sys_fchmod(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let node = node_of(p, &file)?;
    chmod_node(p, node, a[1])
}

fn sys_fchown(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    let node = node_of(p, &file)?;
    chown_node(p, node, a[1], a[2])
}

/// `fchmodat`/`fchownat`: the same, by path.
fn sys_fchmodat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    let r = p.vfs.resolve(&base, &path, true)?;
    if crate::bpf::on_bpffs(&r.path) && !matches!(r.node, Node::Missing { .. }) {
        crate::bpf::chmod(&r.path, a[2] as u32);
        return Ok(0);
    }
    chmod_node(p, r.node, a[2])
}

fn sys_fchownat(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    let path = path_arg(p, a[1])?;
    let base = base_dir(p, a[0], &path)?;
    let r = p.vfs.resolve(&base, &path, a[4] & AT_SYMLINK_NOFOLLOW == 0)?;
    if crate::bpf::on_bpffs(&r.path) && !matches!(r.node, Node::Missing { .. }) {
        crate::bpf::chown(&r.path, a[2] as u32, a[3] as u32);
        return Ok(0);
    }
    chown_node(p, r.node, a[2], a[3])
}

/// The filesystem magic a path is on: selinuxfs, sysfs and proc where the kernel mounts them,
/// ext4 everywhere else. libselinux knows SELinux is there by selinuxfs's magic.
fn fs_magic(path: &[u8]) -> u64 {
    let under = |root: &[u8]| path == root || (path.starts_with(root) && path.get(root.len()) == Some(&b'/'));
    if under(b"/sys/fs/selinux") {
        0xf97c_ff8c // SELINUX_MAGIC
    } else if under(b"/sys/fs/bpf") {
        0xcafe_4a11 // BPF_FS_MAGIC
    } else if under(b"/sys") {
        0x6265_6572 // SYSFS_MAGIC
    } else if under(b"/proc") {
        0x9fa0 // PROC_SUPER_MAGIC
    } else {
        0xEF53 // EXT4_SUPER_MAGIC
    }
}

/// arm64 `struct statfs` (120 bytes).
fn statfs_bytes(magic: u64) -> [u8; 120] {
    let mut b = [0u8; 120];
    let words: [(usize, u64); 8] = [
        (0, magic),       // f_type
        (8, 4096),        // f_bsize
        (16, 1 << 20),    // f_blocks
        (24, 1 << 19),    // f_bfree
        (32, 1 << 19),    // f_bavail
        (40, 1 << 20),    // f_files
        (48, 1 << 19),    // f_ffree
        (64, 255),        // f_namelen
    ];
    for (at, v) in words {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    b[72..80].copy_from_slice(&4096u64.to_le_bytes()); // f_frsize
    b
}

fn sys_statfs(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[0])?;
    let r = p.vfs.resolve(&p.cwd.lock(), &path, true)?;
    if matches!(r.node, Node::Missing { .. }) {
        return Err(ENOENT);
    }
    p.mem.write(a[1], &statfs_bytes(fs_magic(&r.path)))?;
    Ok(0)
}

fn sys_fstatfs(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let file = p.fds.get(fd_arg(a[0]))?;
    p.mem.write(a[1], &statfs_bytes(fs_magic(&guest_path_of(&file))))?;
    Ok(0)
}

fn sys_getcwd(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let mut cwd = p.cwd.lock().clone();
    cwd.push(0);
    if cwd.len() > a[1] as usize {
        return Err(ERANGE);
    }
    p.mem.write(a[0], &cwd)?;
    Ok(cwd.len() as u64)
}

/// `chdir(path)`: the working directory becomes the directory `path` resolves to.
fn sys_chdir(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = path_arg(p, a[0])?;
    let cwd = p.cwd.lock().clone();
    let r = p.vfs.resolve(&cwd, &path, true)?;
    match r.node {
        Node::Dir | Node::HostDir { .. } => {
            *p.cwd.lock() = r.path;
            Ok(0)
        }
        Node::Missing { .. } => Err(ENOENT),
        _ => Err(ENOTDIR),
    }
}

/// `fchdir(fd)`: the working directory becomes the directory `fd` is open on.
fn sys_fchdir(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let path = match &*p.fds.get(fd_arg(a[0]))?.kind.lock() {
        FileKind::Dir { dir, .. } => dir.path.clone(),
        _ => return Err(ENOTDIR),
    };
    *p.cwd.lock() = path;
    Ok(0)
}

pub fn install(table: &mut Table) {
    table.set(nr::FLOCK, sys_flock);
    table.set(nr::CHDIR, sys_chdir);
    table.set(nr::FCHDIR, sys_fchdir);
    table.set(nr::OPENAT, sys_openat);
    table.set(nr::CLOSE, sys_close);
    table.set(nr::READ, sys_read);
    table.set(nr::WRITE, sys_write);
    table.set(nr::READV, sys_readv);
    table.set(nr::WRITEV, sys_writev);
    table.set(nr::PREAD64, sys_pread64);
    table.set(nr::LSEEK, sys_lseek);
    table.set(nr::FSTAT, sys_fstat);
    table.set(nr::NEWFSTATAT, sys_newfstatat);
    table.set(nr::READLINKAT, sys_readlinkat);
    table.set(nr::FACCESSAT, sys_faccessat);
    table.set(nr::FACCESSAT2, sys_faccessat);
    table.set(nr::IOCTL, sys_ioctl);
    table.set(nr::FCNTL, sys_fcntl);
    table.set(nr::DUP, sys_dup);
    table.set(nr::DUP3, sys_dup3);
    table.set(nr::GETDENTS64, sys_getdents64);
    table.set(nr::GETCWD, sys_getcwd);
    table.set(nr::STATFS, sys_statfs);
    table.set(nr::MKDIRAT, sys_mkdirat);
    table.set(nr::UNLINKAT, sys_unlinkat);
    table.set(nr::RENAMEAT, sys_renameat);
    table.set(nr::RENAMEAT2, sys_renameat2);
    table.set(nr::FTRUNCATE, sys_ftruncate);
    table.set(nr::PWRITE64, sys_pwrite64);
    table.set(nr::SENDFILE, sys_sendfile);
    table.set(nr::PREADV, sys_preadv);
    table.set(nr::PWRITEV, sys_pwritev);
    table.set(nr::MEMFD_CREATE, sys_memfd_create);
    table.set(nr::INOTIFY_INIT1, sys_inotify_init1);
    table.set(nr::INOTIFY_ADD_WATCH, sys_inotify_add_watch);
    table.set(nr::INOTIFY_RM_WATCH, sys_inotify_rm_watch);
    table.set(nr::FSYNC, sys_fsync);
    table.set(nr::FDATASYNC, sys_fsync);
    table.set(nr::UTIMENSAT, sys_utimensat);
    table.set(nr::FCHMOD, sys_fchmod);
    table.set(nr::FCHOWN, sys_fchown);
    table.set(nr::FCHMODAT, sys_fchmodat);
    table.set(nr::FCHOWNAT, sys_fchownat);
    table.set(nr::FSTATFS, sys_fstatfs);
}
