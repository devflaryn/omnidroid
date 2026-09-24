//! **Looking at this process's own threads from the outside**: where a thread is executing, how
//! much processor time it has used, and what kind of memory an address is.
//!
//! The seam a sampling profiler needs, and nothing else calls it. It exists because the questions a
//! performance investigation asks of a guest thread -- is it executing translated code, translating,
//! looking up a block, spinning on the exclusive monitor, inside a handler, or waiting in the
//! kernel? -- are answered by *where its instruction pointer is*, and nothing inside the thread can
//! report that without changing what it is doing. Kernel-level sampling (ETW) needs an elevated
//! session this runtime does not have, so the sampler pauses its own process's threads.
//!
//! # The one rule a caller must keep
//!
//! [`HostThread::sample`] **suspends the target**. Between the suspend and the resume the sampling
//! thread must not take any lock the target could hold -- the process heap's included -- or it
//! waits for a thread that cannot run. So `sample` allocates nothing, calls nothing that
//! allocates, writes only into the buffer it is handed, and resumes before it returns on every
//! path. Everything else in this module ([`memory_kind`], [`modules`], ...) may allocate and must
//! be called with no thread suspended, which is automatic because no suspension outlives `sample`.
//!
//! On Linux nothing is suspended: the target is sent a signal and answers from its own handler
//! (`linux.rs`), so it is "stopped" only while that handler runs, and the rule holds trivially.
//!
//! # Five targets
//!
//! * **Windows** (`windows.rs`): `SuspendThread` / `GetThreadContext` / `ResumeThread`,
//!   `QueryThreadCycleTime`, `VirtualQuery`, the PSAPI module list.
//! * **Linux x86-64** (`linux.rs`): a real-time signal (`SIGRTMIN + 7`) queued to the thread with
//!   `rt_tgsigqueueinfo`, whose handler reads the interrupted `ucontext_t`; the thread's CPU-time
//!   clock; `/proc/self/maps`.
//! * **macOS arm64** (`macos.rs`): `thread_suspend` / `thread_get_state` / `thread_resume` on the
//!   thread's Mach port; `thread_info` time and run state; dyld's image list, `dladdr` and
//!   `mach_vm_region`.
//!
//! Any other target returns [`SamplerError::Unsupported`] naming the intended mechanism, because a
//! sampler that reported nothing would read as a thread doing nothing.

use std::time::Duration;

mod error;

pub use error::{SamplerError, SamplerResult};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use linux as backend;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use macos as backend;

#[cfg(not(any(
    target_os = "windows",
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
mod unsupported;
#[cfg(not(any(
    target_os = "windows",
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
use unsupported as backend;

/// How many code bytes [`HostThread::sample`] reads **before** the instruction pointer.
///
/// Enough to reach back over the `mov r64, imm64` that dynarmic emits in front of a spin-lock loop
/// or a reservation compare, which is how a caller recognises monitor code from its bytes alone.
pub const CODE_BEFORE: usize = 48;
/// How many code bytes [`HostThread::sample`] reads from the instruction pointer onwards.
pub const CODE_AFTER: usize = 16;

/// A thread of this process, held open for sampling.
///
/// Holds an OS handle with the rights sampling needs and closes it on drop. `Send` and `Sync`: the
/// handle is an opaque kernel reference and every call through it is a system call.
#[derive(Debug)]
pub struct HostThread {
    inner: backend::Thread,
}

/// Where a thread was when [`HostThread::sample`] looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThreadSample {
    /// The user-mode instruction pointer. For a thread blocked in the kernel, the address in the
    /// system-call stub it entered from.
    pub ip: usize,
    /// The stack pointer.
    pub sp: usize,
    /// `R15` on x86-64 -- the register dynarmic keeps its `JitState` pointer in while translated
    /// code runs, so it is meaningful exactly when `ip` is in a code cache. `0` elsewhere.
    pub r15: usize,
    /// How many of the buffer's `CODE_BEFORE + CODE_AFTER` bytes were read. The buffer's byte
    /// `CODE_BEFORE` is the one at `ip`; a read that could not reach back across an unmapped page
    /// starts later and says so here.
    pub code_start: usize,
    /// One past the last byte read.
    pub code_end: usize,
}

/// What kind of memory an address is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Part of a loaded image (the executable or a DLL); `base` is the image's load address.
    Image {
        /// The image's allocation base.
        base: usize,
    },
    /// Private memory that is executable and writable at once -- on this runtime, only a JIT code
    /// cache (dynarmic commits its cache `PAGE_EXECUTE_READWRITE`; D12's recorded exception). On
    /// Linux and macOS: anonymous private memory that is executable (dynarmic's cache is `rwxp` on
    /// Linux and a `MAP_JIT` `rwx` region on macOS), writable or not.
    PrivateWritableExecutable {
        /// The reservation's allocation base, one per code cache.
        base: usize,
    },
    /// Anything else: data, a file view, free, or an executable mapping that is not writable.
    Other,
}

/// A module loaded in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// The file name, e.g. `ntdll.dll`.
    pub name: String,
    /// The load address.
    pub base: usize,
    /// The image size in bytes.
    pub size: usize,
}

/// Process-wide memory counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessCounters {
    /// Page faults the process has taken, soft and hard together.
    pub page_faults: u64,
    /// Bytes of physical memory in the working set.
    pub working_set: u64,
    /// Bytes of private commit (what the commit limit is charged): on Linux the `VM_ACCOUNT` total,
    /// on macOS `phys_footprint` -- the vm seam's [`process_commit_charge`](crate::vm::process_commit_charge).
    pub private_bytes: u64,
}

impl HostThread {
    /// The calling thread.
    ///
    /// # Errors
    ///
    /// [`SamplerError::LastError`] if the handle could not be opened (Windows);
    /// [`SamplerError::Errno`] on Linux if the sampling signal is already taken;
    /// [`SamplerError::Unsupported`] on a target with no backend.
    pub fn current() -> SamplerResult<Self> {
        Ok(Self { inner: backend::Thread::current()? })
    }

    /// The OS thread id.
    #[must_use]
    pub fn os_id(&self) -> u32 {
        self.inner.os_id()
    }

    /// Processor time the thread has used, kernel and user together.
    ///
    /// Charged in scheduler ticks on Windows (15.625 ms by default, finer while a timer resolution
    /// is raised), so a difference over a few seconds is meaningful and one over a few
    /// milliseconds is not. Nanoseconds on Linux; on macOS charged at context switches and timer
    /// ticks, in microseconds.
    ///
    /// # Errors
    ///
    /// As [`current`](Self::current).
    pub fn cpu_time(&self) -> SamplerResult<Duration> {
        self.inner.cpu_time()
    }

    /// Processor cycles the thread has used: a monotonic count that moves whenever the thread
    /// runs, at the resolution of the time-stamp counter -- which is what makes it the right test
    /// for "did this thread run since I last looked", where [`cpu_time`](Self::cpu_time)'s tick
    /// charging is too coarse. Not a frequency: the counter runs at a constant rate.
    ///
    /// On Linux it is the thread's CPU-time clock in nanoseconds; on macOS the charged time in
    /// nanoseconds plus the number of reads that found the thread running (see `macos.rs`: the
    /// charged time of a running thread lags by up to a scheduler tick). Only its movement means
    /// anything.
    ///
    /// # Errors
    ///
    /// As [`current`](Self::current).
    pub fn cycles(&self) -> SamplerResult<u64> {
        self.inner.cycles()
    }

    /// Suspend the thread, read where it is and the code bytes around its instruction pointer into
    /// `code`, and resume it. See the module documentation for the rule this keeps.
    ///
    /// # Errors
    ///
    /// [`SamplerError::SampledItself`] for the calling thread, which would never resume;
    /// [`SamplerError::LastError`] if the suspend or the context read failed (the thread has
    /// exited, typically) -- [`SamplerError::Errno`] (`ESRCH`) on Linux, [`SamplerError::Kern`] on
    /// macOS; [`SamplerError::NotAnswered`] on Linux for a thread that did not run its handler in
    /// time. The thread is resumed on every path that suspended it.
    pub fn sample(&self, code: &mut [u8; CODE_BEFORE + CODE_AFTER]) -> SamplerResult<ThreadSample> {
        self.inner.sample(code)
    }
}

/// What kind of memory `address` is in. Must not be called while a thread is suspended -- which
/// [`HostThread::sample`] guarantees by never returning with one suspended.
///
/// # Errors
///
/// [`SamplerError::Unsupported`] on a target with no backend; on Linux, [`SamplerError::Errno`] if
/// `/proc/self/maps` cannot be read.
pub fn memory_kind(address: usize) -> SamplerResult<MemoryKind> {
    backend::memory_kind(address)
}

/// Every module loaded in this process.
///
/// # Errors
///
/// [`SamplerError::LastError`] if the list could not be read.
pub fn modules() -> SamplerResult<Vec<Module>> {
    backend::modules()
}

/// Process-wide memory counters.
///
/// # Errors
///
/// [`SamplerError::LastError`] if they could not be read.
pub fn process_counters() -> SamplerResult<ProcessCounters> {
    backend::process_counters()
}

/// The efficiency class of each logical processor in this process's group, indexed by the number
/// [`crate::process::current_cpu`] reports: `0` is the most efficient (slowest) class. A machine
/// whose cores are all alike reports one class for all of them.
///
/// # Errors
///
/// [`SamplerError::LastError`] if the processor sets could not be read.
pub fn efficiency_classes() -> SamplerResult<Vec<u8>> {
    backend::efficiency_classes()
}

/// Every sampler test in this crate's unit-test binary takes this, so that the Linux backend's
/// process-wide request state is observed by one test at a time.
#[cfg(test)]
static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn serial() -> std::sync::MutexGuard<'static, ()> {
    TEST_SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
