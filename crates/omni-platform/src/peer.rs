//! **Another host process's memory, read and written directly**: the platform seam for a host
//! process that serves another's system calls (`omni_linux::remote`'s stand-ins), which otherwise
//! asks the owner for every byte over a socket.
//!
//! | operation | Windows | Linux | macOS |
//! |---|---|---|---|
//! | [`PeerMemory::open`] | `OpenProcess(PROCESS_VM_READ \| PROCESS_VM_WRITE \| PROCESS_VM_OPERATION)` | the pid (`process_vm_*` need no handle) | refused (`task_for_pid` needs an entitlement) |
//! | [`PeerMemory::read`] | `ReadProcessMemory` | `process_vm_readv(2)` | -- |
//! | [`PeerMemory::write`] | `NtWriteVirtualMemory` | `process_vm_writev(2)` | -- |
//!
//! **Neither call changes what the other process allows.** A read or write of a page the owner has
//! not committed, protected against it, or not mapped fails -- whole: a partial copy is a failure
//! too -- and the caller takes its other path. On Windows that is why the write is
//! `NtWriteVirtualMemory` and not `WriteProcessMemory`: the latter makes a read-only or
//! execute-only page writable for the length of the call (what a debugger planting a breakpoint
//! wants) -- a write the owner's protection refuses would go through.
//!
//! What is read or written is the caller's to bound: these calls reach any address of the other
//! process, its runtime's own memory included.

use crate::process::{ProcessError, ProcessResult};

/// A handle on another host process's memory (see the module documentation).
#[derive(Debug)]
pub struct PeerMemory {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    pid: i32,
}

// SAFETY: a process handle (Windows) or a pid (Linux) is usable from any thread; the calls on it
// are thread-safe.
unsafe impl Send for PeerMemory {}
// SAFETY: as `Send`; no method takes `&mut self`.
unsafe impl Sync for PeerMemory {}

#[cfg(windows)]
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtWriteVirtualMemory(process: windows_sys::Win32::Foundation::HANDLE, base: *mut core::ffi::c_void, buffer: *const core::ffi::c_void, size: usize, written: *mut usize) -> i32;
}

impl PeerMemory {
    /// Open host process `pid`'s memory for reading and writing.
    ///
    /// # Errors
    /// The host refuses (no such process, another user's), or this target has no such call.
    pub fn open(pid: u32) -> ProcessResult<Self> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE};
            // SAFETY: plain arguments; the handle is checked below and owned by the value.
            let handle = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_VM_OPERATION, 0, pid) };
            if handle.is_null() {
                // SAFETY: no arguments.
                let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                return Err(ProcessError::LastError { operation: "PeerMemory::open", api: "OpenProcess", code });
            }
            Ok(Self { handle })
        }
        #[cfg(target_os = "linux")]
        {
            Ok(Self { pid: i32::try_from(pid).map_err(|_| ProcessError::Errno { operation: "PeerMemory::open", api: "pid", errno: libc::EINVAL })? })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = pid;
            Err(ProcessError::Unsupported { operation: "PeerMemory::open", intended: "task_for_pid + mach_vm_read_overwrite", platform: "macos" })
        }
    }

    /// Copy `out.len()` bytes at `addr` in the other process into `out`; all or an error.
    ///
    /// # Errors
    /// Any byte of the range not readable there (not mapped, not committed, protected).
    pub fn read(&self, addr: usize, out: &mut [u8]) -> ProcessResult<()> {
        if out.is_empty() {
            return Ok(());
        }
        #[cfg(windows)]
        {
            let mut done = 0usize;
            // SAFETY: `out` is writable for its length; the handle is this value's, open for
            // reading; the call reads the other process and writes only `out` and `done`.
            let ok = unsafe { windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(self.handle, addr as *const _, out.as_mut_ptr().cast(), out.len(), &mut done) };
            if ok == 0 || done != out.len() {
                // SAFETY: no arguments.
                let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                return Err(ProcessError::LastError { operation: "PeerMemory::read", api: "ReadProcessMemory", code });
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            let local = libc::iovec { iov_base: out.as_mut_ptr().cast(), iov_len: out.len() };
            let remote = libc::iovec { iov_base: addr as *mut _, iov_len: out.len() };
            // SAFETY: `local` names `out`, writable for its length; `remote` is only an address in
            // the other process, which the kernel checks there.
            let n = unsafe { libc::process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
            if n < 0 || n as usize != out.len() {
                return Err(ProcessError::Errno { operation: "PeerMemory::read", api: "process_vm_readv", errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EFAULT) });
            }
            Ok(())
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = addr;
            Err(ProcessError::Unsupported { operation: "PeerMemory::read", intended: "mach_vm_read_overwrite", platform: "macos" })
        }
    }

    /// Copy `bytes` to `addr` in the other process; all or an error. A page the other process may
    /// not write is not made writable for it (see the module documentation).
    ///
    /// # Errors
    /// Any byte of the range not writable there (not mapped, not committed, protected).
    pub fn write(&self, addr: usize, bytes: &[u8]) -> ProcessResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        #[cfg(windows)]
        {
            let mut done = 0usize;
            // SAFETY: `bytes` is readable for its length; the handle is this value's, open for
            // writing; the call writes the other process only.
            let status = unsafe { NtWriteVirtualMemory(self.handle, addr as *mut _, bytes.as_ptr().cast(), bytes.len(), &mut done) };
            if status < 0 || done != bytes.len() {
                return Err(ProcessError::Status { operation: "PeerMemory::write", api: "NtWriteVirtualMemory", status });
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            let local = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
            let remote = libc::iovec { iov_base: addr as *mut _, iov_len: bytes.len() };
            // SAFETY: `local` names `bytes`, read only; `remote` is an address in the other
            // process, checked there by the kernel.
            let n = unsafe { libc::process_vm_writev(self.pid, &local, 1, &remote, 1, 0) };
            if n < 0 || n as usize != bytes.len() {
                return Err(ProcessError::Errno { operation: "PeerMemory::write", api: "process_vm_writev", errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EFAULT) });
            }
            Ok(())
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = addr;
            Err(ProcessError::Unsupported { operation: "PeerMemory::write", intended: "mach_vm_write", platform: "macos" })
        }
    }
}

impl Drop for PeerMemory {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: the handle is this value's own, opened by `open`, closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}
