//! Linux and macOS backend for the sampler seam: **structural only**. Every entry point refuses,
//! naming the mechanism an implementation would use, so a sampler built here reports that it cannot
//! see rather than reporting threads that do nothing.

use std::time::Duration;

use super::{MemoryKind, Module, ProcessCounters, SamplerError, SamplerResult, ThreadSample};

fn platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "this platform"
    }
}

fn unsupported<T>(operation: &'static str, linux: &'static str, macos: &'static str) -> SamplerResult<T> {
    Err(SamplerError::Unsupported {
        operation,
        intended: if cfg!(target_os = "macos") { macos } else { linux },
        platform: platform(),
    })
}

#[derive(Debug)]
pub(super) struct Thread;

impl Thread {
    pub(super) fn current() -> SamplerResult<Self> {
        unsupported("HostThread::current", "pthread_self(3)", "mach_thread_self()")
    }

    pub(super) fn os_id(&self) -> u32 {
        0
    }

    pub(super) fn cpu_time(&self) -> SamplerResult<Duration> {
        unsupported(
            "HostThread::cpu_time",
            "pthread_getcpuclockid(3) + clock_gettime(2)",
            "thread_info(THREAD_BASIC_INFO)",
        )
    }

    pub(super) fn cycles(&self) -> SamplerResult<u64> {
        unsupported(
            "HostThread::cycles",
            "pthread_getcpuclockid(3) + clock_gettime(2)",
            "thread_info(THREAD_BASIC_INFO)",
        )
    }

    pub(super) fn sample(&self, _code: &mut [u8]) -> SamplerResult<ThreadSample> {
        unsupported(
            "HostThread::sample",
            "pthread_kill(SIGPROF) and the handler's ucontext_t",
            "thread_suspend + thread_get_state + thread_resume",
        )
    }
}

pub(super) fn memory_kind(_address: usize) -> SamplerResult<MemoryKind> {
    unsupported("memory_kind", "/proc/self/maps", "mach_vm_region")
}

pub(super) fn modules() -> SamplerResult<Vec<Module>> {
    unsupported("modules", "dl_iterate_phdr(3)", "_dyld_image_count / _dyld_get_image_header")
}

pub(super) fn process_counters() -> SamplerResult<ProcessCounters> {
    unsupported("process_counters", "getrusage(2) + /proc/self/statm", "task_info(TASK_VM_INFO)")
}

pub(super) fn efficiency_classes() -> SamplerResult<Vec<u8>> {
    unsupported(
        "efficiency_classes",
        "/sys/devices/system/cpu/cpu*/cpu_capacity",
        "sysctl hw.perflevel*",
    )
}
