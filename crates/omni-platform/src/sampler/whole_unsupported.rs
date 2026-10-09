//! The whole-process calls where they are not implemented (macOS, and targets with no sampler
//! backend): each refuses, naming the mechanism an implementation would use.

use std::time::Duration;

use super::{backend, SamplerError, SamplerResult, ThreadTimes};

fn unsupported<T>(operation: &'static str, intended: &'static str) -> SamplerResult<T> {
    Err(SamplerError::Unsupported { operation, intended, platform: if cfg!(target_os = "macos") { "macos" } else { "this platform" } })
}

pub(super) fn threads(_want_name: &mut dyn FnMut(u32) -> bool) -> SamplerResult<Vec<ThreadTimes>> {
    unsupported("threads", "task_threads() and thread_info(THREAD_BASIC_INFO)")
}

pub(super) fn process_cpu_time() -> SamplerResult<Duration> {
    unsupported("process_cpu_time", "getrusage(RUSAGE_SELF)")
}

pub(super) fn current_thread_id() -> u32 {
    0
}

pub(super) fn open_thread(_id: u32) -> SamplerResult<backend::Thread> {
    unsupported("HostThread::open", "a Mach thread port from task_threads()")
}

pub(super) fn symbolize(_address: usize) -> Option<(String, usize)> {
    None
}
