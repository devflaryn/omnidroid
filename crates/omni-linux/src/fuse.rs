//! `/dev/fuse`, as far as MediaProvider's FUSE daemon needs it to start.
//!
//! vold opens `/dev/fuse`, mounts the emulated volume with it and hands the descriptor to
//! MediaProvider, whose daemon reads requests from it and counts itself started once it has
//! answered the kernel's `FUSE_INIT`. Here the volume's mount is a bind of `/data/media`
//! (`crate::mount`), so no request ever reaches the daemon: a read answers `FUSE_INIT` once, then
//! waits (until a signal) for requests that do not come, and a write (the daemon's replies) is
//! taken whole (`crate::fd`).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::errno::{Errno, EAGAIN, EINVAL};
use crate::fd::{FileKind, OpenFile};
use crate::process::Task;

const FUSE_INIT: u32 = 26;
const O_NONBLOCK: u32 = 0o4000;

/// One open of `/dev/fuse`.
#[derive(Default)]
pub struct Fuse {
    init_sent: AtomicBool,
}

impl Fuse {
    #[must_use]
    pub fn open() -> Arc<Self> {
        Arc::default()
    }
}

/// The `FUSE_INIT` request: `fuse_in_header` then a protocol 7.31 `fuse_init_in` (major, minor,
/// max_readahead, flags -- none of the optional features).
fn init_request() -> Vec<u8> {
    let mut m = Vec::with_capacity(56);
    m.extend_from_slice(&56u32.to_le_bytes()); // len
    m.extend_from_slice(&FUSE_INIT.to_le_bytes()); // opcode
    m.extend_from_slice(&1u64.to_le_bytes()); // unique
    m.extend_from_slice(&0u64.to_le_bytes()); // nodeid
    m.extend_from_slice(&[0u8; 12]); // uid, gid, pid
    m.extend_from_slice(&[0u8; 4]); // total_extlen, padding
    m.extend_from_slice(&7u32.to_le_bytes()); // major
    m.extend_from_slice(&31u32.to_le_bytes()); // minor
    m.extend_from_slice(&(128u32 * 1024).to_le_bytes()); // max_readahead
    m.extend_from_slice(&0u32.to_le_bytes()); // flags
    m
}

/// `read` on `/dev/fuse`; `None` for any other descriptor.
pub fn read(file: &OpenFile, buf: &mut [u8], task: &Task) -> Option<Result<usize, Errno>> {
    let fuse = match &*file.kind.lock() {
        FileKind::Fuse(f) => Arc::clone(f),
        _ => return None,
    };
    if !fuse.init_sent.swap(true, Ordering::SeqCst) {
        let m = init_request();
        if buf.len() < m.len() {
            fuse.init_sent.store(false, Ordering::SeqCst);
            return Some(Err(EINVAL));
        }
        buf[..m.len()].copy_from_slice(&m);
        return Some(Ok(m.len()));
    }
    if *file.flags.lock() & O_NONBLOCK != 0 {
        return Some(Err(EAGAIN));
    }
    // No request ever comes: wait until a signal ends the wait -- on nothing (`poll_keyed`), so
    // the daemon's threads are not woken by every change in the process to find nothing.
    loop {
        let r = if crate::poll::KEYED.load(std::sync::atomic::Ordering::Relaxed) {
            crate::poll::watch(Some(vec![crate::poll::INERT])).wait(None, task)
        } else {
            let seen = crate::poll::generation();
            crate::poll::wait_for_change(seen, None, task)
        };
        if let Err(e) = r {
            return Some(Err(e));
        }
    }
}
