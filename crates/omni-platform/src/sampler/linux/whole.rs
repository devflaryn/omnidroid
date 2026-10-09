//! The whole process on Linux: every thread from `/proc/self/task` (its scheduler time in
//! nanoseconds from `schedstat`, its name from `comm`), the process's CPU clock, and a thread opened
//! by id. Code addresses are not named here (no PDB; `None`): a report shows them as module offsets.

use std::time::Duration;

use super::{errno_error, install, Thread};
use crate::sampler::{SamplerResult, ThreadTimes};

pub(in crate::sampler) fn threads(want_name: &mut dyn FnMut(u32) -> bool) -> SamplerResult<Vec<ThreadTimes>> {
    let dir = std::fs::read_dir("/proc/self/task").map_err(|_| errno_error("threads", "opendir(/proc/self/task)"))?;
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(id) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let path = entry.path();
        // The first field of `schedstat` is the time on a processor, in nanoseconds.
        let Some(cycles) = std::fs::read_to_string(path.join("schedstat"))
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()))
        else {
            continue; // exited since the listing
        };
        let name = if want_name(id) {
            std::fs::read_to_string(path.join("comm")).ok().map(|s| s.trim_end().to_string()).filter(|s| !s.is_empty())
        } else {
            None
        };
        out.push(ThreadTimes { os_id: id, cycles, name, start: 0 });
    }
    Ok(out)
}

pub(in crate::sampler) fn process_cpu_time() -> SamplerResult<Duration> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is writable.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now) } != 0 {
        return Err(errno_error("process_cpu_time", "clock_gettime(CLOCK_PROCESS_CPUTIME_ID)"));
    }
    Ok(Duration::new(now.tv_sec as u64, now.tv_nsec as u32))
}

pub(in crate::sampler) fn current_thread_id() -> u32 {
    super::gettid().unsigned_abs()
}

pub(in crate::sampler) fn open_thread(id: u32) -> SamplerResult<Thread> {
    let signal = install()?;
    let tid = id as i32;
    // The kernel's MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED): what `pthread_getcpuclockid`
    // returns for that thread (see the module documentation of `linux.rs`).
    let clock = ((!tid) << 3) | 6;
    Ok(Thread { tid, clock, signal })
}

pub(in crate::sampler) fn symbolize(_address: usize) -> Option<(String, usize)> {
    None
}
