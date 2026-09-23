//! Windows backend for the sampler seam.
//!
//! `SuspendThread` / `GetThreadContext` / `ResumeThread` on a handle to one of this process's own
//! threads -- the documented way to read another thread's registers -- plus `VirtualQuery`, the
//! PSAPI module list, `GetProcessMemoryInfo` and `GetSystemCpuSetInformation`.
//!
//! `SuspendThread` is asynchronous: it can return before the target has actually stopped.
//! `GetThreadContext` on a suspended thread waits for the suspension to complete, which is why the
//! context is read before anything else and why a failed read still resumes.

use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE,
    HMODULE,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    GetThreadContext, ReadProcessMemory, CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64,
};
use windows_sys::Win32::System::Memory::{
    VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, MEM_PRIVATE,
    PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32EnumProcessModules, K32GetModuleBaseNameW, K32GetModuleInformation,
    K32GetProcessMemoryInfo, MODULEINFO, PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemCpuSetInformation, CpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetThreadTimes, ResumeThread,
    SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use super::{
    MemoryKind, Module, ProcessCounters, SamplerError, SamplerResult, ThreadSample, CODE_AFTER,
    CODE_BEFORE,
};

// `QueryThreadCycleTime` is behind windows-sys's `Win32_System_WindowsProgramming` feature, which
// this crate does not enable; one declaration here, with the documented signature, rather than a
// feature that pulls in the rest of that module.
#[link(name = "kernel32")]
extern "system" {
    fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> i32;
}

fn last_error(operation: &'static str, api: &'static str) -> SamplerError {
    // SAFETY: reads the calling thread's last-error value; no arguments.
    SamplerError::LastError { operation, api, code: unsafe { GetLastError() } }
}

/// An owned thread handle with suspend, context and query rights.
#[derive(Debug)]
pub(super) struct Thread {
    handle: HANDLE,
    id: u32,
}

// SAFETY: a thread handle is an opaque kernel reference. Every use is a system call that takes it by
// value, and the kernel serialises what those calls do to the thread; nothing is shared through the
// pointer value itself.
unsafe impl Send for Thread {}
// SAFETY: as above -- concurrent calls through one handle are concurrent system calls.
unsafe impl Sync for Thread {}

impl Drop for Thread {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `DuplicateHandle` in `current` and is closed exactly once.
        unsafe { CloseHandle(self.handle) };
    }
}

impl Thread {
    pub(super) fn current() -> SamplerResult<Self> {
        let mut handle: HANDLE = core::ptr::null_mut();
        // SAFETY: both pseudo-handles are always valid for the calling process and thread; `handle`
        // is a writable out-parameter. The access asked for is exactly what sampling uses.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                GetCurrentThread(),
                GetCurrentProcess(),
                &mut handle,
                THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                0,
                0,
            )
        };
        if ok == 0 {
            return Err(last_error("HostThread::current", "DuplicateHandle"));
        }
        // SAFETY: no arguments.
        let id = unsafe { GetCurrentThreadId() };
        Ok(Self { handle, id })
    }

    pub(super) fn os_id(&self) -> u32 {
        self.id
    }

    pub(super) fn cpu_time(&self) -> SamplerResult<Duration> {
        let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
        let [creation, exit, kernel, user] = &mut times;
        // SAFETY: `handle` has THREAD_QUERY_INFORMATION; the four out-parameters are writable.
        let ok = unsafe { GetThreadTimes(self.handle, creation, exit, kernel, user) };
        if ok == 0 {
            return Err(last_error("HostThread::cpu_time", "GetThreadTimes"));
        }
        let ticks = |t: &FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Ok(Duration::from_nanos((ticks(&times[2]) + ticks(&times[3])).saturating_mul(100)))
    }

    pub(super) fn cycles(&self) -> SamplerResult<u64> {
        let mut cycles = 0u64;
        // SAFETY: `handle` has THREAD_QUERY_INFORMATION; `cycles` is writable.
        let ok = unsafe { QueryThreadCycleTime(self.handle, &mut cycles) };
        if ok == 0 {
            return Err(last_error("HostThread::cycles", "QueryThreadCycleTime"));
        }
        Ok(cycles)
    }

    /// Suspend, read, resume. **Allocates nothing and takes no lock between the suspend and the
    /// resume**, and resumes on every path that suspended.
    pub(super) fn sample(&self, code: &mut [u8; CODE_BEFORE + CODE_AFTER]) -> SamplerResult<ThreadSample> {
        // SAFETY: no arguments.
        if self.id == unsafe { GetCurrentThreadId() } {
            return Err(SamplerError::SampledItself);
        }
        // SAFETY: `handle` has THREAD_SUSPEND_RESUME.
        let previous = unsafe { SuspendThread(self.handle) };
        if previous == u32::MAX {
            return Err(last_error("HostThread::sample", "SuspendThread"));
        }
        // `GetThreadContext` requires a 16-byte-aligned `CONTEXT` and fails with
        // `ERROR_NOACCESS` (998) otherwise -- MEASURED here, because windows-sys does not declare
        // the alignment on the type, so a plain local is 8-byte aligned by chance.
        #[repr(C, align(16))]
        struct Aligned(CONTEXT);
        // SAFETY: `CONTEXT` is plain data; all-zero is a valid value.
        let mut aligned = Aligned(unsafe { core::mem::zeroed() });
        let context = &mut aligned.0;
        context.ContextFlags = CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64;
        // SAFETY: `handle` has THREAD_GET_CONTEXT and the thread is suspended; `context` is
        // writable and 16-byte aligned.
        let ok = unsafe { GetThreadContext(self.handle, context) };
        let result = if ok == 0 {
            Err(last_error("HostThread::sample", "GetThreadContext"))
        } else {
            let ip = context.Rip as usize;
            let (code_start, code_end) = read_code(ip, code);
            Ok(ThreadSample {
                ip,
                sp: context.Rsp as usize,
                r15: context.R15 as usize,
                code_start,
                code_end,
            })
        };
        // SAFETY: balances the `SuspendThread` above, on every path.
        unsafe { ResumeThread(self.handle) };
        result
    }
}

/// Read the bytes around `ip` into `code` (byte `CODE_BEFORE` is the one at `ip`), returning the
/// range that was read. `ReadProcessMemory` on our own process rather than a pointer read: an
/// address near `ip` can be on an unmapped page, and this reports that instead of faulting.
fn read_code(ip: usize, code: &mut [u8; CODE_BEFORE + CODE_AFTER]) -> (usize, usize) {
    let page = ip & !0xFFF;
    let whole = (ip.saturating_sub(CODE_BEFORE), ip.saturating_add(CODE_AFTER));
    // The executing page is readable by definition; the fallback stays inside it.
    let inside = (whole.0.max(page), whole.1.min(page + 0x1000));
    for (lo, hi) in [whole, inside] {
        if hi <= lo {
            continue;
        }
        let offset = CODE_BEFORE - (ip - lo);
        let mut read = 0usize;
        // SAFETY: the destination is `hi - lo` bytes starting `offset` into `code`, and
        // `offset + (hi - lo) <= CODE_BEFORE + CODE_AFTER` because `lo >= ip - CODE_BEFORE` and
        // `hi <= ip + CODE_AFTER`. The source is an address in this process, which
        // `ReadProcessMemory` validates rather than dereferences.
        let ok = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                lo as *const core::ffi::c_void,
                code.as_mut_ptr().add(offset).cast(),
                hi - lo,
                &mut read,
            )
        };
        if ok != 0 && read == hi - lo {
            return (offset, offset + read);
        }
    }
    (CODE_BEFORE, CODE_BEFORE)
}

pub(super) fn memory_kind(address: usize) -> SamplerResult<MemoryKind> {
    // SAFETY: `MEMORY_BASIC_INFORMATION` is plain data.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: `VirtualQuery` accepts any address and writes at most `dwlength` bytes to `info`.
    let written = unsafe {
        VirtualQuery(address as *const core::ffi::c_void, &mut info, core::mem::size_of_val(&info))
    };
    if written == 0 || info.State != MEM_COMMIT {
        return Ok(MemoryKind::Other);
    }
    let base = info.AllocationBase as usize;
    if info.Type == MEM_IMAGE {
        return Ok(MemoryKind::Image { base });
    }
    if info.Type == MEM_PRIVATE && info.Protect & (PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY) != 0 {
        return Ok(MemoryKind::PrivateWritableExecutable { base });
    }
    Ok(MemoryKind::Other)
}

pub(super) fn modules() -> SamplerResult<Vec<Module>> {
    let mut handles: Vec<HMODULE> = vec![core::ptr::null_mut(); 512];
    loop {
        let bytes = u32::try_from(handles.len() * core::mem::size_of::<HMODULE>()).unwrap_or(u32::MAX);
        let mut needed = 0u32;
        // SAFETY: `handles` is writable for `bytes`; `needed` is an out-parameter.
        let ok = unsafe {
            K32EnumProcessModules(GetCurrentProcess(), handles.as_mut_ptr(), bytes, &mut needed)
        };
        if ok == 0 {
            return Err(last_error("modules", "K32EnumProcessModules"));
        }
        let count = needed as usize / core::mem::size_of::<HMODULE>();
        if count <= handles.len() {
            handles.truncate(count);
            break;
        }
        handles.resize(count, core::ptr::null_mut());
    }
    let mut out = Vec::with_capacity(handles.len());
    for module in handles {
        let mut info = MODULEINFO {
            lpBaseOfDll: core::ptr::null_mut(),
            SizeOfImage: 0,
            EntryPoint: core::ptr::null_mut(),
        };
        // SAFETY: `module` came from the enumeration; `info` is writable for its size.
        let ok = unsafe {
            K32GetModuleInformation(
                GetCurrentProcess(),
                module,
                &mut info,
                core::mem::size_of::<MODULEINFO>() as u32,
            )
        };
        if ok == 0 {
            // Unloaded since the enumeration; not an error of the list.
            continue;
        }
        let mut name = [0u16; 260];
        // SAFETY: `name` is writable for its length in UTF-16 units.
        let len = unsafe {
            K32GetModuleBaseNameW(GetCurrentProcess(), module, name.as_mut_ptr(), name.len() as u32)
        };
        out.push(Module {
            name: String::from_utf16_lossy(&name[..len as usize]),
            base: info.lpBaseOfDll as usize,
            size: info.SizeOfImage as usize,
        });
    }
    Ok(out)
}

pub(super) fn process_counters() -> SamplerResult<ProcessCounters> {
    // SAFETY: plain data.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { core::mem::zeroed() };
    counters.cb = core::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: `counters` is writable for `cb` bytes.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return Err(last_error("process_counters", "K32GetProcessMemoryInfo"));
    }
    Ok(ProcessCounters {
        page_faults: u64::from(counters.PageFaultCount),
        working_set: counters.WorkingSetSize as u64,
        private_bytes: counters.PagefileUsage as u64,
    })
}

pub(super) fn efficiency_classes() -> SamplerResult<Vec<u8>> {
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 asks for the size.
    let ok = unsafe {
        GetSystemCpuSetInformation(core::ptr::null_mut(), 0, &mut needed, GetCurrentProcess(), 0)
    };
    // SAFETY: no arguments.
    if ok == 0 && unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return Err(last_error("efficiency_classes", "GetSystemCpuSetInformation"));
    }
    // `u64` words, so the records are 8-byte aligned as `SYSTEM_CPU_SET_INFORMATION` requires.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` is writable for `needed` bytes.
    let ok = unsafe {
        GetSystemCpuSetInformation(
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
            GetCurrentProcess(),
            0,
        )
    };
    if ok == 0 {
        return Err(last_error("efficiency_classes", "GetSystemCpuSetInformation"));
    }
    let bytes = buffer.as_ptr().cast::<u8>();
    let mut classes = Vec::new();
    let mut at = 0usize;
    while at + core::mem::size_of::<u32>() <= needed as usize {
        // SAFETY: `at` is inside the `needed` bytes the call wrote, and every record starts on an
        // 8-byte boundary (each `Size` is a multiple of 8).
        let record = unsafe { &*(bytes.add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        let size = record.Size as usize;
        if size == 0 {
            break;
        }
        if record.Type == CpuSetInformation {
            // SAFETY: `Type` says the union holds `CpuSet`.
            let set = unsafe { record.Anonymous.CpuSet };
            if set.Group == 0 {
                let index = usize::from(set.LogicalProcessorIndex);
                if classes.len() <= index {
                    classes.resize(index + 1, 0);
                }
                classes[index] = set.EfficiencyClass;
            }
        }
        at += size;
    }
    Ok(classes)
}
