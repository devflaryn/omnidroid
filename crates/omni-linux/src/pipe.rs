//! Pipes (`pipe2`): an in-process byte queue with a read end and a write end. Every thread of the
//! guest is a thread of this one host process, so a pipe never leaves it.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::errno::{Errno, SysResult, EAGAIN, EINTR, EINVAL, EPIPE};
use crate::fd::{FileKind, OpenFile};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

/// What Linux holds before a writer blocks: 64 KiB.
const CAPACITY: usize = 64 << 10;
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;

#[derive(Default)]
pub struct Pipe {
    bytes: Mutex<VecDeque<u8>>,
    changed: Condvar,
    readers: AtomicUsize,
    writers: AtomicUsize,
}

/// One end. Dropping it (the last descriptor on it closed) is what lets the other end see
/// end-of-file or `EPIPE`.
pub struct End {
    pipe: Arc<Pipe>,
    write: bool,
}

impl End {
    fn new(pipe: &Arc<Pipe>, write: bool) -> Self {
        let count = if write { &pipe.writers } else { &pipe.readers };
        count.fetch_add(1, Ordering::SeqCst);
        Self { pipe: Arc::clone(pipe), write }
    }

    #[must_use]
    pub const fn is_write(&self) -> bool {
        self.write
    }

    /// The pipe itself, to wait on without holding the descriptor's lock.
    #[must_use]
    pub fn pipe(&self) -> Arc<Pipe> {
        Arc::clone(&self.pipe)
    }
}

impl Drop for End {
    fn drop(&mut self) {
        let count = if self.write { &self.pipe.writers } else { &self.pipe.readers };
        count.fetch_sub(1, Ordering::SeqCst);
        let _held = self.pipe.bytes.lock();
        self.pipe.changed.notify_all();
    }
}

/// Wait for the pipe to change, up to a slice at a time, so a signal posted meanwhile is seen.
fn wait(pipe: &Pipe, bytes: &mut parking_lot::MutexGuard<'_, VecDeque<u8>>, task: &Task) -> Result<(), Errno> {
    if task.pending.load(Ordering::SeqCst) & !task.sigmask != 0 {
        return Err(EINTR);
    }
    pipe.changed.wait_for(bytes, Duration::from_millis(50));
    Ok(())
}

/// Read up to `buf.len()` bytes: what is there, or wait for some; 0 once no writer is left.
pub fn read(pipe: &Pipe, buf: &mut [u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    let mut bytes = pipe.bytes.lock();
    loop {
        if !bytes.is_empty() {
            let n = buf.len().min(bytes.len());
            for (slot, b) in buf.iter_mut().zip(bytes.drain(..n)) {
                *slot = b;
            }
            pipe.changed.notify_all();
            return Ok(n);
        }
        if pipe.writers.load(Ordering::SeqCst) == 0 {
            return Ok(0);
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        wait(pipe, &mut bytes, task)?;
    }
}

/// Write all of `data` (waiting for room), or as much as fits when non-blocking; `EPIPE` when no
/// reader is left.
pub fn write(pipe: &Pipe, data: &[u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    let mut bytes = pipe.bytes.lock();
    let mut done = 0;
    while done < data.len() {
        if pipe.readers.load(Ordering::SeqCst) == 0 {
            return if done > 0 { Ok(done) } else { Err(EPIPE) };
        }
        let room = CAPACITY.saturating_sub(bytes.len());
        if room == 0 {
            if nonblocking {
                return if done > 0 { Ok(done) } else { Err(EAGAIN) };
            }
            wait(pipe, &mut bytes, task)?;
            continue;
        }
        let n = room.min(data.len() - done);
        bytes.extend(&data[done..done + n]);
        done += n;
        pipe.changed.notify_all();
    }
    Ok(done)
}

/// Bytes waiting to be read (`FIONREAD`, and `poll`'s readiness).
#[must_use]
pub fn available(pipe: &Pipe) -> usize {
    pipe.bytes.lock().len()
}

/// A descriptor's pipe end, if it is one: (the pipe, is the write end, non-blocking).
#[must_use]
pub fn end_of(file: &OpenFile) -> Option<(Arc<Pipe>, bool, bool)> {
    match &*file.kind.lock() {
        FileKind::Pipe(end) => Some((end.pipe(), end.is_write(), *file.flags.lock() & O_NONBLOCK != 0)),
        _ => None,
    }
}

fn sys_pipe2(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    if a[1] & !(O_CLOEXEC | u64::from(O_NONBLOCK) | 0o40000 /* O_DIRECT */) != 0 {
        return Err(EINVAL);
    }
    let pipe = Arc::new(Pipe::default());
    let flags = (a[1] as u32) & O_NONBLOCK;
    let open = |write: bool| {
        Arc::new(OpenFile {
            kind: Mutex::new(FileKind::Pipe(End::new(&pipe, write))),
            flags: Mutex::new(flags | if write { 1 } else { 0 }),
        })
    };
    let cloexec = a[1] & O_CLOEXEC != 0;
    let r = p.fds.insert(open(false), cloexec, 0)?;
    let w = match p.fds.insert(open(true), cloexec, 0) {
        Ok(w) => w,
        Err(e) => {
            let _ = p.fds.remove(r);
            return Err(e);
        }
    };
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&r.to_le_bytes());
    out[4..8].copy_from_slice(&w.to_le_bytes());
    if let Err(e) = p.mem.write(a[0], &out) {
        let _ = p.fds.remove(r);
        let _ = p.fds.remove(w);
        return Err(e);
    }
    Ok(0)
}

pub fn install(table: &mut Table) {
    table.set(nr::PIPE2, sys_pipe2);
}
