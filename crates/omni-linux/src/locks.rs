//! POSIX record locks (`fcntl` `F_GETLK`, `F_SETLK`, `F_SETLKW`, and the open-file-description
//! `F_OFD_*`), as SQLite takes them on every database it opens. A classic lock is its process's:
//! any `close` of the file by that process releases it, as does the process's end. An OFD lock is
//! its open file's, released when that is closed for the last time.
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar, Mutex};

use crate::errno::{Errno, SysResult, EAGAIN, EINTR, EINVAL};
use crate::fd::{FileKind, OpenFile};
use crate::process::{Process, Task};
use crate::vfs::Node;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    Process(i32),
    /// An open file description, by its address.
    Ofd(usize),
}

#[derive(Debug, Clone)]
struct Lock {
    file: String,
    /// The open file it was taken through: closing that releases it even when the file has
    /// since been unlinked (and its name no longer finds it).
    via: usize,
    holder: Holder,
    pid: i32,
    start: u64,
    /// Exclusive; `u64::MAX` for "to the end, however far it grows".
    end: u64,
    write: bool,
}

#[derive(Default)]
struct Table {
    locks: Mutex<Vec<Lock>>,
    changed: Condvar,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(Table::default)
}

/// Which file a descriptor's locks are on: a writable-mount file by its host path, an image file
/// by its guest path, a BPF map by its id, anything else by the open file itself.
fn file_key(p: &Process, file: &Arc<OpenFile>) -> String {
    let (guest, sysroot) = match &*file.kind.lock() {
        FileKind::Host { guest, sysroot, .. } => (guest.clone(), *sysroot),
        // A BPF map is one inode however many times it is opened (netd locks each of its maps).
        FileKind::Bpf(crate::bpf::Object::Map(m)) => return format!("bpf-map:{}", m.id),
        _ => return format!("open:{:p}", Arc::as_ptr(file)),
    };
    if sysroot {
        return format!("image:{}", String::from_utf8_lossy(&guest));
    }
    match p.vfs.resolve(b"/", &guest, true).map(|r| r.node) {
        Ok(Node::HostFile { host }) => format!("host:{}", host.display()),
        _ => format!("guest:{}", String::from_utf8_lossy(&guest)),
    }
}

/// The byte range a `struct flock` names: `[start, end)`.
fn range(file: &Arc<OpenFile>, whence: i16, start: i64, len: i64) -> Result<(u64, u64), Errno> {
    let base: i64 = match whence {
        0 => 0,
        1 | 2 => match &mut *file.kind.lock() {
            FileKind::Host { file, .. } => {
                use std::io::Seek;
                if whence == 1 { file.stream_position().map_err(|_| EINVAL)? as i64 } else { file.metadata().map_err(|_| EINVAL)?.len() as i64 }
            }
            _ => 0,
        },
        _ => return Err(EINVAL),
    };
    let from = base.checked_add(start).ok_or(EINVAL)?;
    let (from, to) = match len {
        0 => (from, i64::MAX),
        l if l > 0 => (from, from.checked_add(l).ok_or(EINVAL)?),
        l => (from.checked_add(l).ok_or(EINVAL)?, from),
    };
    if from < 0 {
        return Err(EINVAL);
    }
    Ok((from as u64, if to == i64::MAX { u64::MAX } else { to as u64 }))
}

/// The first lock another holder has that conflicts with taking `[start, end)` (`write`).
fn conflict<'a>(locks: &'a [Lock], file: &str, holder: Holder, start: u64, end: u64, write: bool) -> Option<&'a Lock> {
    locks.iter().find(|l| l.file == file && l.holder != holder && l.start < end && start < l.end && (write || l.write))
}

/// `F_GETLK` (`ofd`: `F_OFD_GETLK`), `F_SETLK`, `F_SETLKW` and their OFD forms, on `struct flock`
/// at `at`.
pub fn fcntl(p: &Process, t: &Task, file: &Arc<OpenFile>, cmd: u64, at: u64) -> SysResult {
    let raw = p.mem.read(at, 32)?;
    let kind = i16::from_le_bytes([raw[0], raw[1]]);
    let whence = i16::from_le_bytes([raw[2], raw[3]]);
    let start = i64::from_le_bytes(raw[8..16].try_into().expect("8"));
    let len = i64::from_le_bytes(raw[16..24].try_into().expect("8"));
    let ofd = matches!(cmd, 36..=38);
    if ofd && i32::from_le_bytes(raw[24..28].try_into().expect("4")) != 0 {
        return Err(EINVAL); // l_pid must be 0 for an OFD lock
    }
    let holder = if ofd { Holder::Ofd(Arc::as_ptr(file) as usize) } else { Holder::Process(p.sys.pid) };
    let (from, to) = range(file, whence, start, len)?;
    let key = file_key(p, file);
    let table = table();
    let mut locks = table.locks.lock();
    match cmd {
        5 | 36 => {
            if !matches!(kind, F_RDLCK | F_WRLCK) {
                return Err(EINVAL);
            }
            let mut out = raw.clone();
            match conflict(&locks, &key, holder, from, to, kind == F_WRLCK) {
                Some(l) => {
                    out[0..2].copy_from_slice(&(if l.write { F_WRLCK } else { F_RDLCK }).to_le_bytes());
                    out[2..4].copy_from_slice(&0i16.to_le_bytes());
                    out[8..16].copy_from_slice(&(l.start as i64).to_le_bytes());
                    let len = if l.end == u64::MAX { 0 } else { (l.end - l.start) as i64 };
                    out[16..24].copy_from_slice(&len.to_le_bytes());
                    let pid = if matches!(l.holder, Holder::Ofd(_)) { -1 } else { l.pid };
                    out[24..28].copy_from_slice(&pid.to_le_bytes());
                }
                None => out[0..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
            }
            p.mem.write(at, &out)?;
            Ok(0)
        }
        6 | 7 | 37 | 38 => {
            if !matches!(kind, F_RDLCK | F_WRLCK | F_UNLCK) {
                return Err(EINVAL);
            }
            if kind != F_UNLCK {
                let wait = matches!(cmd, 7 | 38);
                while conflict(&locks, &key, holder, from, to, kind == F_WRLCK).is_some() {
                    if !wait {
                        return Err(EAGAIN);
                    }
                    if t.pending.load(std::sync::atomic::Ordering::SeqCst) & !t.sigmask != 0 {
                        return Err(EINTR);
                    }
                    table.changed.wait_for(&mut locks, std::time::Duration::from_millis(50));
                }
            }
            // The holder's own locks over the range give way (split around it), then the new one.
            let mut kept = Vec::with_capacity(locks.len() + 2);
            for l in locks.drain(..) {
                if l.file != key || l.holder != holder || l.end <= from || to <= l.start {
                    kept.push(l);
                    continue;
                }
                if l.start < from {
                    kept.push(Lock { end: from, ..l.clone() });
                }
                if to < l.end {
                    kept.push(Lock { start: to, ..l });
                }
            }
            if kind != F_UNLCK {
                kept.push(Lock { file: key, via: Arc::as_ptr(file) as usize, holder, pid: p.sys.pid, start: from, end: to, write: kind == F_WRLCK });
            }
            *locks = kept;
            table.changed.notify_all();
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// `close` of a descriptor: the process's classic locks on that file go.
pub fn closed(p: &Process, file: &Arc<OpenFile>) {
    let table = table();
    let mut locks = table.locks.lock();
    if locks.is_empty() {
        return;
    }
    let key = file_key(p, file);
    let via = Arc::as_ptr(file) as usize;
    let before = locks.len();
    locks.retain(|l| !(l.holder == Holder::Process(p.sys.pid) && (l.file == key || l.via == via)));
    if locks.len() != before {
        table.changed.notify_all();
    }
}

/// An open file description closed for the last time: its OFD locks go.
pub fn released(file: &OpenFile) {
    let table = table();
    let mut locks = table.locks.lock();
    if locks.is_empty() {
        return;
    }
    let before = locks.len();
    let me = Holder::Ofd(std::ptr::from_ref(file) as usize);
    locks.retain(|l| l.holder != me);
    if locks.len() != before {
        table.changed.notify_all();
    }
}

/// A process ended: its classic locks go.
pub fn process_ended(pid: i32) {
    let table = table();
    let mut locks = table.locks.lock();
    let before = locks.len();
    locks.retain(|l| l.holder != Holder::Process(pid));
    if locks.len() != before {
        table.changed.notify_all();
    }
}

/// `flock(fd, op)`: a BSD lock on the whole file, held by the open file (`LOCK_SH` 1, `LOCK_EX` 2,
/// `LOCK_UN` 8, with `LOCK_NB` 4 not to wait). Apart from `fcntl` locks, as on Linux.
pub fn flock(p: &Process, t: &Task, file: &Arc<OpenFile>, op: u64) -> SysResult {
    const LOCK_SH: u64 = 1;
    const LOCK_EX: u64 = 2;
    const LOCK_NB: u64 = 4;
    const LOCK_UN: u64 = 8;
    let key = format!("flock:{}", file_key(p, file));
    let holder = Holder::Ofd(Arc::as_ptr(file) as usize);
    let table = table();
    let mut locks = table.locks.lock();
    locks.retain(|l| !(l.file == key && l.holder == holder));
    let write = match op & !LOCK_NB {
        LOCK_UN => {
            table.changed.notify_all();
            return Ok(0);
        }
        LOCK_SH => false,
        LOCK_EX => true,
        _ => return Err(EINVAL),
    };
    while conflict(&locks, &key, holder, 0, u64::MAX, write).is_some() {
        if op & LOCK_NB != 0 {
            return Err(crate::errno::EAGAIN);
        }
        if t.pending.load(std::sync::atomic::Ordering::SeqCst) & !t.sigmask != 0 {
            return Err(EINTR);
        }
        table.changed.wait_for(&mut locks, std::time::Duration::from_millis(50));
    }
    locks.push(Lock { file: key, via: Arc::as_ptr(file) as usize, holder, pid: p.sys.pid, start: 0, end: u64::MAX, write });
    Ok(0)
}
