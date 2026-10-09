//! The whole process on Windows: every thread's cycles, name and start address, the process's
//! processor time, opening one of its threads by id, and naming a code address from its module's
//! PDB (dbghelp). For a report every few seconds; nothing here is for a hot path.
//!
//! The threads are walked with `NtGetNextThread`, which hands out a handle to each thread of one
//! process in turn. A Tool Help snapshot (`CreateToolhelp32Snapshot`) lists every thread of the
//! *system* to find this process's: measured 95-98 ms a call on this host with a game running,
//! against the walk's few ms.

use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use super::{last_error, QueryThreadCycleTime, Thread};
use crate::sampler::{SamplerError, SamplerResult, ThreadTimes};

/// `ThreadQuerySetWin32StartAddress`: the address the thread was started at.
const THREAD_QUERY_SET_WIN32_START_ADDRESS: u32 = 9;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationThread(thread: HANDLE, class: u32, info: *mut core::ffi::c_void, len: u32, returned: *mut u32) -> i32;
    /// The thread after `thread` (null: the first) of `process`, opened with `access`; a non-zero
    /// status (`STATUS_NO_MORE_ENTRIES`) after the last. Exported by ntdll since Windows Vista.
    fn NtGetNextThread(process: HANDLE, thread: HANDLE, access: u32, attributes: u32, flags: u32, next: *mut HANDLE) -> i32;
}

/// A thread's description (`SetThreadDescription`, which `std::thread::Builder::name` sets).
fn description(handle: HANDLE) -> Option<String> {
    use windows_sys::Win32::System::Threading::GetThreadDescription;
    let mut text: *mut u16 = core::ptr::null_mut();
    // SAFETY: `handle` has THREAD_QUERY_INFORMATION (which includes the limited right the call
    // needs); `text` is an out-parameter the call allocates with LocalAlloc.
    if unsafe { GetThreadDescription(handle, &mut text) } < 0 || text.is_null() {
        return None;
    }
    let mut len = 0;
    // SAFETY: a NUL-terminated UTF-16 string from the call.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units are readable.
    let s = String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(text, len) });
    // SAFETY: allocated by the call with LocalAlloc; freed once.
    unsafe { windows_sys::Win32::Foundation::LocalFree(text.cast()) };
    (!s.is_empty()).then_some(s)
}

pub(in crate::sampler) fn threads(want_name: &mut dyn FnMut(u32) -> bool) -> SamplerResult<Vec<ThreadTimes>> {
    use windows_sys::Win32::System::Threading::GetThreadId;
    let mut out = Vec::new();
    let mut handle: HANDLE = core::ptr::null_mut();
    loop {
        let mut next: HANDLE = core::ptr::null_mut();
        // SAFETY: the process pseudo-handle, the previous thread's handle (or null) and a writable
        // out-parameter.
        let status = unsafe { NtGetNextThread(GetCurrentProcess(), handle, THREAD_QUERY_INFORMATION, 0, 0, &mut next) };
        if !handle.is_null() {
            // SAFETY: from the previous call, closed once, after the walk has moved past it.
            unsafe { CloseHandle(handle) };
        }
        if status != 0 {
            if out.is_empty() {
                return Err(SamplerError::LastError { operation: "threads", api: "NtGetNextThread", code: status as u32 });
            }
            break;
        }
        handle = next;
        // SAFETY: a live thread handle.
        let id = unsafe { GetThreadId(handle) };
        let mut cycles = 0u64;
        // SAFETY: `handle` has THREAD_QUERY_INFORMATION; `cycles` is writable.
        unsafe { QueryThreadCycleTime(handle, &mut cycles) };
        let (mut name, mut start) = (None, 0usize);
        if want_name(id) {
            name = description(handle);
            // SAFETY: `start` is a writable pointer-sized out-parameter, as this class requires.
            unsafe {
                NtQueryInformationThread(
                    handle,
                    THREAD_QUERY_SET_WIN32_START_ADDRESS,
                    (&mut start as *mut usize).cast(),
                    core::mem::size_of::<usize>() as u32,
                    core::ptr::null_mut(),
                )
            };
        }
        out.push(ThreadTimes { os_id: id, cycles, name, start });
    }
    Ok(out)
}

pub(in crate::sampler) fn process_cpu_time() -> SamplerResult<Duration> {
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: the process pseudo-handle; the four out-parameters are writable.
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) } == 0 {
        return Err(last_error("process_cpu_time", "GetProcessTimes"));
    }
    let ticks = |t: &FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Ok(Duration::from_nanos((ticks(&times[2]) + ticks(&times[3])).saturating_mul(100)))
}

pub(in crate::sampler) fn current_thread_id() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetCurrentThreadId() }
}

pub(in crate::sampler) fn open_thread(id: u32) -> SamplerResult<Thread> {
    use windows_sys::Win32::System::Threading::OpenThread;
    // SAFETY: any id; null comes back for one that is not a thread we may open.
    let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, id) };
    if handle.is_null() {
        return Err(last_error("HostThread::open", "OpenThread"));
    }
    Ok(Thread { handle, id })
}

/// dbghelp, initialised once for this process (`SymInitializeW` over the modules loaded then, with
/// the executable's directory -- where `cargo` and the portable bundle put its PDB -- and its
/// `deps` on the search path). dbghelp is not thread safe, so every use is under this lock. The
/// value: whether the initialisation succeeded.
static DBGHELP: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

pub(in crate::sampler) fn symbolize(address: usize) -> Option<(String, usize)> {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SymFromAddrW, SymInitializeW, SymSetOptions, SYMBOL_INFOW, SYMOPT_DEFERRED_LOADS, SYMOPT_UNDNAME,
    };
    let mut ready = DBGHELP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let ok = *ready.get_or_insert_with(|| {
        let dir = std::env::current_exe().ok().and_then(|e| e.parent().map(std::path::Path::to_path_buf));
        let search = dir.map_or_else(String::new, |d| format!("{};{}", d.display(), d.join("deps").display()));
        let wide: Vec<u16> = search.encode_utf16().chain(Some(0)).collect();
        // SAFETY: plain flags.
        unsafe { SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS) };
        // SAFETY: the process pseudo-handle; `wide` is NUL-terminated and outlives the call.
        unsafe { SymInitializeW(GetCurrentProcess(), wide.as_ptr(), 1) != 0 }
    });
    if !ok {
        return None;
    }
    const NAME_UNITS: usize = 512;
    // SYMBOL_INFOW is followed by its name; u64 words keep the buffer aligned for the struct.
    let mut buffer = vec![0u64; (core::mem::size_of::<SYMBOL_INFOW>() + NAME_UNITS * 2).div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<SYMBOL_INFOW>();
    // SAFETY: `info` points at a zeroed, aligned buffer large enough for the struct and the name.
    unsafe {
        (*info).SizeOfStruct = core::mem::size_of::<SYMBOL_INFOW>() as u32;
        (*info).MaxNameLen = NAME_UNITS as u32;
    }
    let mut displacement = 0u64;
    // SAFETY: dbghelp is initialised and serialised by the lock; `info` is as above.
    if unsafe { SymFromAddrW(GetCurrentProcess(), address as u64, &mut displacement, info) } == 0 {
        return None;
    }
    // SAFETY: the call wrote `NameLen` units of name (at most `MaxNameLen`) from `Name` on, inside
    // the buffer.
    let name = unsafe {
        let len = ((*info).NameLen as usize).min(NAME_UNITS - 1);
        core::slice::from_raw_parts(core::ptr::addr_of!((*info).Name).cast::<u16>(), len)
    };
    Some((String::from_utf16_lossy(name), displacement as usize))
}
