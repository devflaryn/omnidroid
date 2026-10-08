//! Sync files: the kernel's fences (`linux/sync_file.h`, `drivers/dma-buf/sync_file.c`).
//!
//! Everything this runtime composes or renders on the host is finished when the call that did it
//! returns (the host GPU's work is waited on before a fence is handed out), so every sync file here
//! is **signalled when it is made**. It is still a real descriptor, because what receives it treats
//! it as one: libsync and ANGLE `poll` it (a fence of -1 polled with no timeout waits forever),
//! SurfaceFlinger reads its signal time (`SYNC_IOC_FILE_INFO`) and merges fences
//! (`SYNC_IOC_MERGE`).
use std::sync::Arc;

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EINVAL, ENOTTY};
use crate::fd::{FileKind, OpenFile};
use crate::process::Process;

const SYNC_IOC_MERGE: u64 = 0xc030_3e03;
const SYNC_IOC_FILE_INFO: u64 = 0xc038_3e04;
const SYNC_IOC_SET_DEADLINE: u64 = 0x4010_3e05;

/// One fence: when it signalled (CLOCK_MONOTONIC, ns).
pub struct SyncFile {
    pub signalled_ns: u64,
}

fn file(signalled_ns: u64) -> Arc<OpenFile> {
    Arc::new(OpenFile { kind: Mutex::new(FileKind::SyncFile(Arc::new(SyncFile { signalled_ns }))), flags: Mutex::new(0) })
}

/// A sync file signalled at `signalled_ns` (CLOCK_MONOTONIC), not yet in any process: for a host
/// service to hand out (the composer's present and release fences).
#[must_use]
pub fn signalled_at(signalled_ns: u64) -> Arc<OpenFile> {
    file(signalled_ns)
}

/// A new sync file in `p`, signalled now: its descriptor.
///
/// # Errors
/// No descriptor free.
pub fn signalled(p: &Process) -> Result<i32, Errno> {
    let now = crate::sys::monotonic().as_nanos() as u64;
    p.fds.insert(file(now), true, 0)
}

/// `ioctl` on a sync file.
pub fn ioctl(p: &Process, fence: &SyncFile, cmd: u64, arg: u64) -> SysResult {
    match cmd {
        SYNC_IOC_MERGE => {
            // struct sync_merge_data { char name[32]; s32 fd2; s32 fence; u32 flags; u32 pad; }
            let fd2 = p.mem.read_u32(arg + 32)? as i32;
            let other = p.fds.get(fd2)?;
            let other_ns = match &*other.kind.lock() {
                FileKind::SyncFile(f) => f.signalled_ns,
                _ => return Err(EINVAL),
            };
            let merged = p.fds.insert(file(fence.signalled_ns.max(other_ns)), true, 0)?;
            p.mem.write(arg + 36, &merged.to_le_bytes())?;
            Ok(0)
        }
        SYNC_IOC_FILE_INFO => {
            // struct sync_file_info { char name[32]; s32 status; u32 flags; u32 num_fences;
            // u32 pad; u64 sync_fence_info; } -- the fences are written when there is room.
            let room = p.mem.read_u32(arg + 40)?;
            let fences = p.mem.read_u64(arg + 48)?;
            let mut name = [0u8; 32];
            name[..9].copy_from_slice(b"omnidroid");
            p.mem.write(arg, &name)?;
            p.mem.write(arg + 32, &1i32.to_le_bytes())?; // signalled
            p.mem.write(arg + 40, &1u32.to_le_bytes())?;
            if room >= 1 && fences != 0 {
                // struct sync_fence_info { char obj_name[32]; char driver_name[32]; s32 status;
                // u32 flags; u64 timestamp_ns; }
                let mut f = [0u8; 80];
                f[..9].copy_from_slice(b"omnidroid");
                f[32..36].copy_from_slice(b"host");
                f[64..68].copy_from_slice(&1i32.to_le_bytes());
                f[72..80].copy_from_slice(&fence.signalled_ns.to_le_bytes());
                p.mem.write(fences, &f)?;
            }
            Ok(0)
        }
        // A deadline for a fence already signalled changes nothing.
        SYNC_IOC_SET_DEADLINE => Ok(0),
        _ => Err(ENOTTY),
    }
}
