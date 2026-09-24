//! macOS (Apple silicon) backend for the sampler seam.
//!
//! The Windows mechanism has a direct counterpart here: a thread's Mach port (`mach_thread_self()`
//! at registration, one send right held until drop) is suspended with `thread_suspend`, its
//! registers read with `thread_get_state(ARM_THREAD_STATE64)`, the code around `pc` copied with
//! `mach_vm_read_overwrite` on this task (a kernel copy, so an unmapped page is
//! `KERN_INVALID_ADDRESS` rather than a fault), and it is resumed with `thread_resume` on every
//! path that suspended it. Between the suspend and the resume only Mach traps are made -- no
//! allocation, no lock -- which is the rule `sampler/mod.rs` states. Measured on the M1 (macOS
//! 26.5): 3 us per sample of a thread on a performance core, 18 us on an efficiency core.
//!
//! # Did it run
//!
//! `thread_info(THREAD_BASIC_INFO)` gives user and system time, in microseconds -- but for a thread
//! that is running on another core, **the kernel only charges it at context switches and timer
//! ticks**: MEASURED on the M1, a thread spinning on a performance core showed its time change in
//! 152 of 1000 reads 1.5 ms apart, so "the time did not move" does not mean "it did not run". The
//! same call's `run_state` says whether the thread is running (or runnable) right now, which is
//! the missing half. [`cycles`](Thread::cycles) is therefore the charged time in nanoseconds plus a
//! per-handle count of the reads that found the thread running: monotonic, and it moves between
//! two reads if the thread was charged time between them or is running at the second. A runnable
//! thread waiting for a core counts as running -- on a saturated machine the sampler then reads it
//! at the point it was preempted, which is where it is spending wall time.
//!
//! # What an address is
//!
//! [`modules`] is dyld's image list; an image's extent is its `__TEXT` segment, not the span of
//! all its segments, because images in the dyld shared cache have their segments far apart and
//! interleaved with other images' (a whole-image span would overlap its neighbours). Code is in
//! `__TEXT`, which is what a sampled `pc` is classified against. [`memory_kind`] says `Image` for
//! any address `dladdr` places in an image (data included, as Windows' `MEM_IMAGE` does), and
//! `PrivateWritableExecutable` for a private region whose protection includes execute:
//! dynarmic's arm64 code cache is an anonymous `mmap(MAP_JIT)` that `mach_vm_region` reports
//! `rwx`, not shared (MEASURED).
//!
//! # Memory and processors
//!
//! [`process_counters`]: page faults are `task_info(TASK_EVENTS_INFO)`'s `faults`; private bytes
//! and the working set are the vm seam's `phys_footprint` and `resident_size` from
//! `task_info(TASK_VM_INFO)`. [`efficiency_classes`]: `hw.nperflevels` performance levels, level 0
//! the fastest, and the most efficient level's processors numbered first -- MEASURED on the M1: the
//! device tree's `cpu0`..`cpu3` have `cluster-type` `E` and `cpu4`..`cpu7` `P`, and the test
//! checks that on the machine it runs on.
//!
//! # Not supported
//!
//! `ThreadSample::r15` is 0: it is an x86-64 register. `crate::perf`'s `mon` class recognises the
//! exclusive monitor from x86-64 `mov r64, imm64` encodings, so on this backend `mon` samples are
//! counted as `jit`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::{
    MemoryKind, Module, ProcessCounters, SamplerError, SamplerResult, ThreadSample, CODE_AFTER,
    CODE_BEFORE,
};

const CODE_LEN: usize = CODE_BEFORE + CODE_AFTER;

type KernReturn = libc::c_int;
type MachPort = libc::mach_port_t;

const KERN_SUCCESS: KernReturn = 0;
const THREAD_BASIC_INFO: i32 = 3;
const THREAD_BASIC_INFO_COUNT: u32 = 10;
const TH_STATE_RUNNING: i32 = 1;
const ARM_THREAD_STATE64: i32 = 6;
const TASK_EVENTS_INFO: libc::c_int = 2;
const VM_REGION_BASIC_INFO_64: i32 = 9;
const VM_PROT_EXECUTE: i32 = 4;
const LC_SEGMENT_64: u32 = 0x19;
const MH_MAGIC_64: u32 = 0xFEED_FACF;

/// `thread_basic_info` from `<mach/thread_info.h>`: ten 32-bit words.
#[repr(C)]
#[derive(Default)]
struct ThreadBasicInfo {
    user_seconds: i32,
    user_microseconds: i32,
    system_seconds: i32,
    system_microseconds: i32,
    cpu_usage: i32,
    policy: i32,
    run_state: i32,
    flags: i32,
    suspend_count: i32,
    sleep_time: i32,
}

/// `arm_thread_state64_t` (non-arm64e layout): 68 32-bit words.
#[repr(C)]
#[derive(Default)]
struct ArmThreadState64 {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    pad: u32,
}

/// `vm_region_basic_info_data_64_t`, `#pragma pack(4)`: nine 32-bit words.
#[repr(C, packed(4))]
#[derive(Default, Clone, Copy)]
struct RegionBasicInfo64 {
    protection: i32,
    max_protection: i32,
    inheritance: u32,
    shared: u32,
    reserved: u32,
    offset: u64,
    behavior: i32,
    user_wired_count: u16,
}

/// `task_events_info` from `<mach/task_info.h>`: eight 32-bit words.
#[repr(C)]
#[derive(Default)]
struct TaskEventsInfo {
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    csw: i32,
}

#[repr(C)]
struct MachHeader64 {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
struct SegmentCommand64 {
    cmd: u32,
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

extern "C" {
    static mach_task_self_: MachPort;
    fn mach_thread_self() -> MachPort;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn thread_suspend(thread: MachPort) -> KernReturn;
    fn thread_resume(thread: MachPort) -> KernReturn;
    fn thread_get_state(thread: MachPort, flavor: i32, state: *mut u32, count: *mut u32) -> KernReturn;
    fn thread_info(thread: MachPort, flavor: i32, info: *mut u32, count: *mut u32) -> KernReturn;
    fn mach_vm_read_overwrite(task: MachPort, address: u64, size: u64, data: u64, out: *mut u64) -> KernReturn;
    fn mach_vm_region(
        task: MachPort,
        address: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut u32,
        count: *mut u32,
        object_name: *mut MachPort,
    ) -> KernReturn;
    fn pthread_threadid_np(thread: libc::pthread_t, id: *mut u64) -> libc::c_int;
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_header(image_index: u32) -> *const libc::c_void;
    fn _dyld_get_image_name(image_index: u32) -> *const libc::c_char;
}

fn task_self() -> MachPort {
    // SAFETY: set by libSystem before any Rust code runs and never written afterwards.
    unsafe { mach_task_self_ }
}

fn kern(operation: &'static str, api: &'static str, code: KernReturn) -> SamplerError {
    SamplerError::Kern { operation, api, code }
}

/// The calling thread's 64-bit kernel id.
fn current_thread_id() -> u64 {
    let mut id = 0u64;
    // SAFETY: a null (0) pthread_t names the calling thread; `id` is writable.
    unsafe { pthread_threadid_np(0 as libc::pthread_t, &mut id) };
    id
}

/// A thread of this task, held by a send right to its Mach port.
#[derive(Debug)]
pub(super) struct Thread {
    port: MachPort,
    id: u64,
    /// Reads of [`Thread::cycles`] that found the thread running; see the module documentation.
    running_reads: AtomicU64,
}

impl Drop for Thread {
    fn drop(&mut self) {
        // SAFETY: the send right `mach_thread_self` gave in `current`, released exactly once.
        unsafe { mach_port_deallocate(task_self(), self.port) };
    }
}

impl Thread {
    pub(super) fn current() -> SamplerResult<Self> {
        // SAFETY: no arguments; returns a new send right to the calling thread, owned by `Self`.
        let port = unsafe { mach_thread_self() };
        if port == 0 {
            return Err(kern("HostThread::current", "mach_thread_self", 0));
        }
        Ok(Self { port, id: current_thread_id(), running_reads: AtomicU64::new(0) })
    }

    pub(super) fn os_id(&self) -> u32 {
        self.id as u32
    }

    fn basic_info(&self, operation: &'static str) -> SamplerResult<ThreadBasicInfo> {
        let mut info = ThreadBasicInfo::default();
        let mut count = THREAD_BASIC_INFO_COUNT;
        // SAFETY: `info` is THREAD_BASIC_INFO_COUNT words and `count` says so.
        let kr = unsafe {
            thread_info(self.port, THREAD_BASIC_INFO, core::ptr::addr_of_mut!(info).cast(), &mut count)
        };
        if kr != KERN_SUCCESS {
            return Err(kern(operation, "thread_info(THREAD_BASIC_INFO)", kr));
        }
        Ok(info)
    }

    fn charged(info: &ThreadBasicInfo) -> Duration {
        let micros = |s: i32, us: i32| u64::from(s.unsigned_abs()) * 1_000_000 + u64::from(us.unsigned_abs());
        Duration::from_micros(
            micros(info.user_seconds, info.user_microseconds)
                + micros(info.system_seconds, info.system_microseconds),
        )
    }

    pub(super) fn cpu_time(&self) -> SamplerResult<Duration> {
        Ok(Self::charged(&self.basic_info("HostThread::cpu_time")?))
    }

    /// Charged nanoseconds plus the reads that found it running. See the module documentation.
    pub(super) fn cycles(&self) -> SamplerResult<u64> {
        let info = self.basic_info("HostThread::cycles")?;
        let running = if info.run_state == TH_STATE_RUNNING {
            self.running_reads.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.running_reads.load(Ordering::Relaxed)
        };
        Ok((Self::charged(&info).as_nanos() as u64).wrapping_add(running))
    }

    /// Suspend, read, resume. **Allocates nothing and takes no lock between the suspend and the
    /// resume**, and resumes on every path that suspended.
    pub(super) fn sample(&self, code: &mut [u8; CODE_LEN]) -> SamplerResult<ThreadSample> {
        if self.id == current_thread_id() {
            return Err(SamplerError::SampledItself);
        }
        // SAFETY: no memory.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
        // SAFETY: a send right this handle owns.
        let kr = unsafe { thread_suspend(self.port) };
        if kr != KERN_SUCCESS {
            return Err(kern("HostThread::sample", "thread_suspend", kr));
        }
        let mut state = ArmThreadState64::default();
        let mut count = (core::mem::size_of::<ArmThreadState64>() / 4) as u32;
        // SAFETY: `state` is `count` words; the thread is suspended.
        let kr = unsafe {
            thread_get_state(self.port, ARM_THREAD_STATE64, core::ptr::addr_of_mut!(state).cast(), &mut count)
        };
        let result = if kr == KERN_SUCCESS {
            let ip = state.pc as usize;
            let (code_start, code_end) = read_code(ip, page, code);
            Ok(ThreadSample { ip, sp: state.sp as usize, r15: 0, code_start, code_end })
        } else {
            Err(kern("HostThread::sample", "thread_get_state(ARM_THREAD_STATE64)", kr))
        };
        // SAFETY: balances the `thread_suspend` above, on every path.
        unsafe { thread_resume(self.port) };
        result
    }
}

/// Read the bytes around `ip` into `code` (byte `CODE_BEFORE` is the one at `ip`), returning the
/// range read: the whole window, else the part inside `ip`'s own page. A Mach trap; no allocation.
fn read_code(ip: usize, page: usize, code: &mut [u8; CODE_LEN]) -> (usize, usize) {
    let page_start = ip & !(page - 1);
    let whole = (ip.saturating_sub(CODE_BEFORE), ip.saturating_add(CODE_AFTER));
    let inside = (whole.0.max(page_start), whole.1.min(page_start + page));
    for (lo, hi) in [whole, inside] {
        if hi <= lo {
            continue;
        }
        let offset = CODE_BEFORE - (ip - lo);
        let mut read = 0u64;
        // SAFETY: the destination is `hi - lo` bytes at `offset` into `code`, inside it because
        // `lo >= ip - CODE_BEFORE` and `hi <= ip + CODE_AFTER`; the source is validated by the
        // kernel, not dereferenced.
        let kr = unsafe {
            mach_vm_read_overwrite(
                task_self(),
                lo as u64,
                (hi - lo) as u64,
                code.as_mut_ptr().add(offset) as u64,
                &mut read,
            )
        };
        if kr == KERN_SUCCESS && read == (hi - lo) as u64 {
            return (offset, offset + (hi - lo));
        }
    }
    (CODE_BEFORE, CODE_BEFORE)
}

pub(super) fn memory_kind(address: usize) -> SamplerResult<MemoryKind> {
    // SAFETY: plain data.
    let mut info: libc::Dl_info = unsafe { core::mem::zeroed() };
    // SAFETY: `dladdr` accepts any address and writes `info`.
    if unsafe { libc::dladdr(address as *const libc::c_void, &mut info) } != 0 && !info.dli_fbase.is_null() {
        return Ok(MemoryKind::Image { base: info.dli_fbase as usize });
    }
    let mut start = address as u64;
    let mut size = 0u64;
    let mut region = RegionBasicInfo64::default();
    let mut count = (core::mem::size_of::<RegionBasicInfo64>() / 4) as u32;
    let mut object: MachPort = 0;
    // SAFETY: every out-parameter is writable; `count` is the region info's size in words.
    let kr = unsafe {
        mach_vm_region(
            task_self(),
            &mut start,
            &mut size,
            VM_REGION_BASIC_INFO_64,
            core::ptr::addr_of_mut!(region).cast(),
            &mut count,
            &mut object,
        )
    };
    // `mach_vm_region` answers for the first region at or *above* the address.
    if kr != KERN_SUCCESS || start > address as u64 {
        return Ok(MemoryKind::Other);
    }
    let (protection, shared) = (region.protection, region.shared);
    if protection & VM_PROT_EXECUTE != 0 && shared == 0 {
        return Ok(MemoryKind::PrivateWritableExecutable { base: start as usize });
    }
    Ok(MemoryKind::Other)
}

/// An image's `__TEXT` extent from its load commands; `None` if the header is not a 64-bit one.
///
/// # Safety
///
/// `header` must be a loaded image's Mach-O header.
unsafe fn text_size(header: *const MachHeader64) -> Option<usize> {
    // SAFETY: the caller's contract.
    let h = unsafe { &*header };
    if h.magic != MH_MAGIC_64 {
        return None;
    }
    let mut at = header.cast::<u8>().wrapping_add(core::mem::size_of::<MachHeader64>());
    for _ in 0..h.ncmds {
        // SAFETY: load commands follow the header, each `cmdsize` long, `ncmds` of them.
        let (cmd, cmdsize) = unsafe { (*at.cast::<u32>(), *at.cast::<u32>().add(1)) };
        if cmd == LC_SEGMENT_64 {
            // SAFETY: an LC_SEGMENT_64 command is a segment_command_64.
            let segment = unsafe { &*at.cast::<SegmentCommand64>() };
            if segment.segname.starts_with(b"__TEXT\0") {
                return Some(segment.vmsize as usize);
            }
        }
        if cmdsize == 0 {
            break;
        }
        at = at.wrapping_add(cmdsize as usize);
    }
    None
}

pub(super) fn modules() -> SamplerResult<Vec<Module>> {
    let mut out = Vec::new();
    // SAFETY: no arguments.
    let count = unsafe { _dyld_image_count() };
    for index in 0..count {
        // SAFETY: an index below the count; null if the image was unloaded since.
        let header = unsafe { _dyld_get_image_header(index) }.cast::<MachHeader64>();
        // SAFETY: as above.
        let name = unsafe { _dyld_get_image_name(index) };
        if header.is_null() || name.is_null() {
            continue;
        }
        // SAFETY: a loaded image's header.
        let Some(size) = (unsafe { text_size(header) }) else { continue };
        // SAFETY: dyld's NUL-terminated path for a loaded image.
        let path = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        out.push(Module { name, base: header as usize, size });
    }
    out.sort_by_key(|m| m.base);
    Ok(out)
}

fn vm_code(error: &crate::vm::VmError) -> i32 {
    match error {
        crate::vm::VmError::Os { source, .. } => source.code() as i32,
        _ => -1,
    }
}

pub(super) fn process_counters() -> SamplerResult<ProcessCounters> {
    let mut events = TaskEventsInfo::default();
    let mut count = (core::mem::size_of::<TaskEventsInfo>() / 4) as libc::mach_msg_type_number_t;
    // SAFETY: `events` is `count` words.
    let kr = unsafe {
        libc::task_info(
            task_self(),
            TASK_EVENTS_INFO as libc::task_flavor_t,
            core::ptr::addr_of_mut!(events).cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(kern("process_counters", "task_info(TASK_EVENTS_INFO)", kr));
    }
    let working_set = crate::vm::process_working_set()
        .map_err(|e| kern("process_counters", "task_info(TASK_VM_INFO)", vm_code(&e)))?;
    let private_bytes = crate::vm::process_commit_charge()
        .map_err(|e| kern("process_counters", "task_info(TASK_VM_INFO)", vm_code(&e)))?;
    Ok(ProcessCounters { page_faults: u64::from(events.faults.unsigned_abs()), working_set, private_bytes })
}

fn sysctl_int(name: &str) -> Option<i32> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value = 0i32;
    let mut size = core::mem::size_of::<i32>();
    // SAFETY: `value` is writable for `size` bytes; no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr(), core::ptr::addr_of_mut!(value).cast(), &mut size, core::ptr::null_mut(), 0)
    };
    (rc == 0 && size == core::mem::size_of::<i32>()).then_some(value)
}

/// Classes from per-level processor counts, level 0 the fastest; the most efficient level's
/// processors are numbered first. See the module documentation.
fn classes_from_levels(counts: &[usize]) -> Vec<u8> {
    let levels = counts.len();
    let mut out = Vec::new();
    for (level, &n) in counts.iter().enumerate().rev() {
        out.extend(std::iter::repeat((levels - 1 - level) as u8).take(n));
    }
    out
}

pub(super) fn efficiency_classes() -> SamplerResult<Vec<u8>> {
    let levels = sysctl_int("hw.nperflevels").unwrap_or(1).max(1) as usize;
    let counts: Option<Vec<usize>> = (0..levels)
        .map(|l| sysctl_int(&format!("hw.perflevel{l}.logicalcpu")).map(|n| n.max(0) as usize))
        .collect();
    match counts {
        Some(counts) if levels > 1 => Ok(classes_from_levels(&counts)),
        _ => {
            let all = sysctl_int("hw.logicalcpu").ok_or(SamplerError::Errno {
                operation: "efficiency_classes",
                api: "sysctlbyname(hw.logicalcpu)",
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            })?;
            Ok(vec![0; all.max(0) as usize])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_levels_become_classes_with_the_efficient_processors_first() {
        // M1: level 0 performance (4), level 1 efficiency (4); processors 0-3 are efficiency.
        assert_eq!(classes_from_levels(&[4, 4]), vec![0, 0, 0, 0, 1, 1, 1, 1]);
        // M1 Pro: 8 performance, 2 efficiency.
        assert_eq!(classes_from_levels(&[8, 2]), vec![0, 0, 1, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(classes_from_levels(&[6]), vec![0; 6]);
    }
}
