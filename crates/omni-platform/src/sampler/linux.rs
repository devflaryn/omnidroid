//! Linux backend for the sampler seam.
//!
//! # Where a thread is: a signal it answers itself
//!
//! Linux has no call that stops one thread of the caller's own process and reads its registers --
//! `ptrace` cannot attach to a thread of the tracer's own thread group -- so the thread is asked:
//! [`sample`](Thread::sample) queues the **sampling signal** to it with `rt_tgsigqueueinfo`, and the
//! handler, running on the target at whatever instruction it was interrupted at, reads the
//! interrupted `RIP`, `RSP` and `R15` from its `ucontext_t`, copies the code bytes around `RIP`
//! with `process_vm_readv` (a system call, so a byte range that crosses into an unmapped page is an
//! `EFAULT` rather than a fault), publishes them, and wakes the sampler through a futex. The target
//! is "stopped" exactly as long as its handler runs, which is what Windows' suspend / read / resume
//! amounts to.
//!
//! **The signal is `SIGRTMIN + 7`** (41 under glibc, whose `SIGRTMIN` is 34 because it keeps 32
//! and 33 for cancellation and `setxid`). Not `SIGPROF`, which `setitimer(ITIMER_PROF)` profilers
//! own; not `SIGUSR1`/`SIGUSR2`, which this crate's own tests install handlers for; not
//! `SIGSEGV`/`SIGBUS`, which are the fault seam's (`fault/linux.rs`) and dynarmic's. Nothing in this
//! runtime, dynarmic, Mesa, X11 or the NVIDIA driver installs a real-time signal handler; and so
//! that "nothing else uses it" is checked rather than assumed, the handler is installed only over a
//! **default** disposition -- if something already holds the signal, [`HostThread::current`]
//! refuses with `EBUSY` naming `sigaction` and the thread goes unsampled, rather than the sampler
//! stealing a handler.
//!
//! The handler is installed `SA_SIGINFO | SA_RESTART` and not `SA_ONSTACK`: its frame is small and
//! goes on the thread's own stack (the kernel skips the 128-byte red zone), and a thread interrupted
//! inside the fault handler is already on its alternate stack, where the kernel places a nested
//! frame below the current one either way. It is async-signal-safe: atomics, `getpid`,
//! `process_vm_readv` and `futex`, and `errno` is saved and restored around the body.
//!
//! **What `SA_RESTART` cannot cover.** A thread that is sampled while it is blocked in `poll`,
//! `epoll_wait`, `select` or `sigtimedwait` gets `EINTR` from that call whatever the flag says
//! (`signal(7)`). Only threads whose CPU time moved since the previous tick are signalled, so a
//! thread parked in a wait is not touched -- but one that ran and then entered such a wait before
//! the signal arrived is. Every such wait in this runtime (the net seam's, libxcb's, the drivers')
//! retries `EINTR`; the guest sees it only where Linux would give it one too.
//!
//! # The request protocol
//!
//! One request is in flight at a time (a process-wide mutex that only samplers take; the handler
//! takes nothing). A request is a sequence number, sent as the signal's `si_value` and stored in
//! `ARMED`; the handler **claims** it with a compare-and-swap of `ARMED` from that number to the
//! number with the top bit set, so a signal that arrives late -- after the sampler gave up, or for
//! an earlier request -- finds a different number there and does nothing. A sampler that times out
//! withdraws the request with the opposite compare-and-swap; if that fails the handler has claimed
//! it and is about to answer, and the sampler waits for the answer, which is microseconds away.
//!
//! # Did it run: the kernel's per-thread CPU clock
//!
//! [`cycles`](Thread::cycles) and [`cpu_time`](Thread::cpu_time) read the thread's CPU-time clock
//! (`pthread_getcpuclockid`, which is `(~tid << 3) | CPUCLOCK_PERTHREAD | CPUCLOCK_SCHED` -- the
//! scheduler's `sum_exec_runtime`), in nanoseconds. For a thread that is running on another
//! processor right now the kernel adds the time since it was last charged
//! (`task_sched_runtime`), so the clock moves between any two reads that straddle execution. The
//! clock id is built from the thread id, not from a `pthread_t`, so reading it after the thread has
//! exited is an `EINVAL`, not undefined behaviour.
//!
//! # What an address is: `/proc/self/maps`
//!
//! [`modules`] are the files that have an executable mapping (and the vDSO), one per path, spanning
//! all of that file's mappings; [`memory_kind`] says `Image` inside one of those, and
//! `PrivateWritableExecutable` for **anonymous private executable** memory: dynarmic's x86-64 code
//! cache is an anonymous `mmap` that Xbyak makes read-write-execute (`rwxp`, no path). The maps are
//! read once and cached: an address the cache places in an image or a code cache is answered from
//! it for a second (so memory unmapped less than a second ago can still be answered as what it
//! was), and any other answer re-reads them, at most every 50 ms for the same page.
//!
//! # Memory
//!
//! [`process_counters`]: page faults are `getrusage(RUSAGE_SELF)`'s minor + major faults; the
//! working set is `/proc/self/statm`'s resident size and "private" is the `VM_ACCOUNT` total from
//! `/proc/self/smaps` -- the vm seam's [`process_working_set`](crate::vm::process_working_set) and
//! [`process_commit_charge`](crate::vm::process_commit_charge), which are the faithful
//! counterparts of Windows' working set and private usage. The `smaps` walk costs in proportion to
//! what is mapped, once per report.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{
    MemoryKind, Module, ProcessCounters, SamplerError, SamplerResult, ThreadSample, CODE_AFTER,
    CODE_BEFORE,
};

const CODE_LEN: usize = CODE_BEFORE + CODE_AFTER;

/// The sampling signal is `SIGRTMIN() + SIGNAL_OFFSET`. See the module documentation.
const SIGNAL_OFFSET: i32 = 7;

/// How long [`Thread::sample`] waits for the handler's answer. A thread that was running a moment
/// ago answers in microseconds; one that has since been descheduled answers when it next runs, so
/// the wait allows for a busy machine's scheduling latency.
const ANSWER_WAIT: Duration = Duration::from_millis(20);

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn errno_error(operation: &'static str, api: &'static str) -> SamplerError {
    SamplerError::Errno { operation, api, errno: errno() }
}

fn gettid() -> i32 {
    // SAFETY: no arguments; cannot fail.
    unsafe { libc::gettid() }
}

// ------------------------------------------------------------------------------------ the signal

static INSTALLED: OnceLock<Result<i32, SamplerError>> = OnceLock::new();

/// Install the sampling signal's handler, once per process, over a default disposition only.
fn install() -> SamplerResult<i32> {
    INSTALLED
        .get_or_init(|| {
            let signal = libc::SIGRTMIN() + SIGNAL_OFFSET;
            if signal > libc::SIGRTMAX() {
                return Err(SamplerError::Errno {
                    operation: "HostThread::current",
                    api: "SIGRTMIN + 7 is past SIGRTMAX",
                    errno: libc::EINVAL,
                });
            }
            // SAFETY: plain data; all-zero is a valid (default) action.
            let mut current: libc::sigaction = unsafe { core::mem::zeroed() };
            // SAFETY: a query: no action is installed, `current` is written.
            if unsafe { libc::sigaction(signal, core::ptr::null(), &mut current) } != 0 {
                return Err(errno_error("HostThread::current", "sigaction (query)"));
            }
            if current.sa_sigaction != libc::SIG_DFL {
                return Err(SamplerError::Errno {
                    operation: "HostThread::current",
                    api: "sigaction: the sampling signal already has a handler",
                    errno: libc::EBUSY,
                });
            }
            // SAFETY: plain data, filled in below.
            let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
            action.sa_sigaction = on_sample as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            // SAFETY: `action.sa_mask` is a live sigset_t.
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            // SAFETY: `action` names a handler of the three-argument shape `SA_SIGINFO` calls.
            if unsafe { libc::sigaction(signal, &action, core::ptr::null_mut()) } != 0 {
                return Err(errno_error("HostThread::current", "sigaction"));
            }
            Ok(signal)
        })
        .clone()
}

/// The top bit of `ARMED`: the request has been claimed by a handler.
const CLAIMED: u64 = 1 << 63;

/// Next request's sequence number. Never 0, never with the low 32 bits all zero (the futex word
/// holds those bits and starts at 0), never with [`CLAIMED`] set.
static NEXT: AtomicU64 = AtomicU64::new(1);
/// The request a handler may claim (0: none), or it with [`CLAIMED`] set once one has.
static ARMED: AtomicU64 = AtomicU64::new(0);
/// Futex word: the low 32 bits of the last request answered.
static DONE: AtomicU32 = AtomicU32::new(0);
static ANSWER_IP: AtomicUsize = AtomicUsize::new(0);
static ANSWER_SP: AtomicUsize = AtomicUsize::new(0);
static ANSWER_R15: AtomicUsize = AtomicUsize::new(0);
static ANSWER_CODE_START: AtomicUsize = AtomicUsize::new(0);
static ANSWER_CODE_END: AtomicUsize = AtomicUsize::new(0);

/// The code bytes the handler copied. Written only by the handler that claimed the current request
/// and read only by the sampler that made it, after `DONE` says the write is complete (release /
/// acquire), under the one-request-at-a-time mutex.
struct CodeBuffer(UnsafeCell<[u8; CODE_LEN]>);
// SAFETY: see the type's documentation -- writer and reader are ordered by `DONE`.
unsafe impl Sync for CodeBuffer {}
static ANSWER_CODE: CodeBuffer = CodeBuffer(UnsafeCell::new([0; CODE_LEN]));

/// `siginfo_t` as `rt_tgsigqueueinfo` reads it and an `SI_QUEUE` handler receives it: signo,
/// errno, code, then (8-aligned) the sender's pid and uid and the `sigval`, in 128 bytes.
#[repr(C)]
struct Queued {
    signo: i32,
    errno: i32,
    code: i32,
    _pad: i32,
    pid: i32,
    uid: u32,
    value: usize,
    _rest: [u64; 12],
}
const _: () = assert!(core::mem::size_of::<Queued>() == 128);
const _: () = assert!(core::mem::size_of::<libc::siginfo_t>() == 128);

/// One request at a time. Only samplers take it; the handler takes nothing.
static SAMPLING: Mutex<()> = Mutex::new(());

fn next_sequence() -> u64 {
    loop {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed) & !CLAIMED;
        if seq & 0xFFFF_FFFF != 0 {
            return seq;
        }
    }
}

/// The sampling signal's handler. Async-signal-safe: see the module documentation.
extern "C" fn on_sample(_signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: the calling thread's errno location is always valid.
    let saved = unsafe { *libc::__errno_location() };
    // SAFETY: the kernel passes a valid siginfo_t for an SA_SIGINFO handler.
    let info = unsafe { &*info };
    if info.si_code == libc::SI_QUEUE {
        // SAFETY: an SI_QUEUE siginfo has `Queued`'s layout (the kernel's `_rt` member).
        let seq = unsafe { (*core::ptr::from_ref(info).cast::<Queued>()).value } as u64;
        if seq != 0 && ARMED.compare_exchange(seq, seq | CLAIMED, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            // SAFETY: the kernel passes the interrupted context as a ucontext_t for SA_SIGINFO.
            let gregs = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext.gregs };
            let ip = gregs[libc::REG_RIP as usize] as usize;
            // SAFETY: this handler claimed the request, so it is the buffer's only writer until it
            // publishes `DONE`; see `CodeBuffer`.
            let code = unsafe { &mut *ANSWER_CODE.0.get() };
            let (start, end) = read_code(ip, code);
            ANSWER_IP.store(ip, Ordering::Relaxed);
            ANSWER_SP.store(gregs[libc::REG_RSP as usize] as usize, Ordering::Relaxed);
            ANSWER_R15.store(gregs[libc::REG_R15 as usize] as usize, Ordering::Relaxed);
            ANSWER_CODE_START.store(start, Ordering::Relaxed);
            ANSWER_CODE_END.store(end, Ordering::Relaxed);
            DONE.store(seq as u32, Ordering::Release);
            // SAFETY: a futex wake on a live static word; no memory is written.
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    DONE.as_ptr(),
                    libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                    1,
                );
            }
        }
    }
    // SAFETY: as above.
    unsafe { *libc::__errno_location() = saved };
}

/// Read the bytes around `ip` into `code` (byte `CODE_BEFORE` is the one at `ip`), returning the
/// range that was read -- `process_vm_readv` on this process, so an unmapped page is an error
/// rather than a fault. The same two attempts as the Windows backend: the whole window, then the
/// part of it inside `ip`'s own page. Async-signal-safe.
fn read_code(ip: usize, code: &mut [u8; CODE_LEN]) -> (usize, usize) {
    let page = ip & !0xFFF;
    let whole = (ip.saturating_sub(CODE_BEFORE), ip.saturating_add(CODE_AFTER));
    let inside = (whole.0.max(page), whole.1.min(page + 0x1000));
    for (lo, hi) in [whole, inside] {
        if hi <= lo {
            continue;
        }
        let offset = CODE_BEFORE - (ip - lo);
        let local = libc::iovec {
            // SAFETY: `offset + (hi - lo) <= CODE_LEN` because `lo >= ip - CODE_BEFORE` and
            // `hi <= ip + CODE_AFTER`.
            iov_base: unsafe { code.as_mut_ptr().add(offset) }.cast(),
            iov_len: hi - lo,
        };
        let remote = libc::iovec { iov_base: lo as *mut libc::c_void, iov_len: hi - lo };
        // SAFETY: `local` is writable for its length; `remote` is an address range in this process,
        // which the kernel validates rather than dereferences.
        let read = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
        if read == (hi - lo) as isize {
            return (offset, offset + (hi - lo));
        }
    }
    (CODE_BEFORE, CODE_BEFORE)
}

/// Queue `signal` to thread `tid` of this process with `value` as its `sigval`
/// (`rt_tgsigqueueinfo`, `SI_QUEUE`). The system call's return value.
fn queue(tid: i32, signal: i32, value: u64) -> libc::c_long {
    let info = Queued {
        signo: signal,
        errno: 0,
        code: libc::SI_QUEUE,
        _pad: 0,
        // SAFETY: no arguments.
        pid: unsafe { libc::getpid() },
        // SAFETY: no arguments.
        uid: unsafe { libc::getuid() },
        value: value as usize,
        _rest: [0; 12],
    };
    // SAFETY: `info` is a 128-byte siginfo the kernel copies in; the kernel checks that `tid` is a
    // thread of this process.
    unsafe {
        libc::syscall(libc::SYS_rt_tgsigqueueinfo, libc::getpid(), tid, signal, &info as *const Queued)
    }
}

/// Wait until `DONE` holds `seq`'s low bits or `deadline` passes.
fn wait_for_answer(seq: u64, deadline: Instant) -> bool {
    let want = seq as u32;
    loop {
        let seen = DONE.load(Ordering::Acquire);
        if seen == want {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let left = deadline - now;
        let timeout = libc::timespec {
            tv_sec: left.as_secs() as libc::time_t,
            tv_nsec: left.subsec_nanos() as libc::c_long,
        };
        // SAFETY: a futex wait on a live static word with a relative timeout; it returns at once
        // if the word no longer holds `seen`.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                DONE.as_ptr(),
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                seen,
                &timeout as *const libc::timespec,
            );
        }
    }
}

/// A thread of this process: its kernel id and its CPU-time clock.
#[derive(Debug)]
pub(super) struct Thread {
    tid: i32,
    clock: libc::clockid_t,
    signal: i32,
}

impl Thread {
    pub(super) fn current() -> SamplerResult<Self> {
        let signal = install()?;
        let mut clock: libc::clockid_t = 0;
        // SAFETY: the calling thread's own pthread_t; `clock` is writable.
        let rc = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock) };
        if rc != 0 {
            return Err(SamplerError::Errno {
                operation: "HostThread::current",
                api: "pthread_getcpuclockid",
                errno: rc,
            });
        }
        Ok(Self { tid: gettid(), clock, signal })
    }

    pub(super) fn os_id(&self) -> u32 {
        self.tid.unsigned_abs()
    }

    fn clock_ns(&self, operation: &'static str) -> SamplerResult<u64> {
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `now` is writable; the clock id names a thread of this process or fails.
        if unsafe { libc::clock_gettime(self.clock, &mut now) } != 0 {
            return Err(errno_error(operation, "clock_gettime(thread CPU clock)"));
        }
        Ok((now.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(now.tv_nsec as u64))
    }

    pub(super) fn cpu_time(&self) -> SamplerResult<Duration> {
        Ok(Duration::from_nanos(self.clock_ns("HostThread::cpu_time")?))
    }

    /// Nanoseconds of CPU time, not cycles: the contract is "a monotonic count that moves whenever
    /// the thread runs", which the scheduler's per-thread runtime is, at nanosecond resolution.
    pub(super) fn cycles(&self) -> SamplerResult<u64> {
        self.clock_ns("HostThread::cycles")
    }

    /// Signal, wait for the handler's answer, copy it out. Allocates nothing.
    pub(super) fn sample(&self, code: &mut [u8; CODE_LEN]) -> SamplerResult<ThreadSample> {
        if self.tid == gettid() {
            return Err(SamplerError::SampledItself);
        }
        let _one = SAMPLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = next_sequence();
        ARMED.store(seq, Ordering::SeqCst);
        let started = Instant::now();
        if queue(self.tid, self.signal, seq) != 0 {
            let error = errno_error("HostThread::sample", "rt_tgsigqueueinfo");
            ARMED.store(0, Ordering::SeqCst);
            return Err(error);
        }
        if !wait_for_answer(seq, started + ANSWER_WAIT) {
            if ARMED.compare_exchange(seq, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Err(SamplerError::NotAnswered {
                    operation: "HostThread::sample",
                    signal: self.signal,
                    waited_us: started.elapsed().as_micros() as u64,
                });
            }
            // Claimed just now: the handler is running and answers in microseconds.
            while DONE.load(Ordering::Acquire) != seq as u32 {
                std::hint::spin_loop();
            }
        }
        // SAFETY: `DONE` (acquire) says the claiming handler finished writing; see `CodeBuffer`.
        let answer = unsafe { &*ANSWER_CODE.0.get() };
        let (code_start, code_end) =
            (ANSWER_CODE_START.load(Ordering::Relaxed), ANSWER_CODE_END.load(Ordering::Relaxed));
        code[code_start..code_end].copy_from_slice(&answer[code_start..code_end]);
        Ok(ThreadSample {
            ip: ANSWER_IP.load(Ordering::Relaxed),
            sp: ANSWER_SP.load(Ordering::Relaxed),
            r15: ANSWER_R15.load(Ordering::Relaxed),
            code_start,
            code_end,
        })
    }
}

// ----------------------------------------------------------------------------- /proc/self/maps

/// One line of `/proc/self/maps`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mapping {
    start: usize,
    end: usize,
    exec: bool,
    shared: bool,
    /// Empty for an anonymous mapping; `[heap]`, `[stack]`, `[vdso]` ... for the kernel's names.
    path: String,
}

/// Parse one line: `start-end perms offset dev inode [path]`. The path is everything after the
/// inode's padding and may contain spaces.
fn parse_line(line: &str) -> Option<Mapping> {
    let mut rest = line;
    let mut field = || -> Option<&str> {
        rest = rest.trim_start_matches(' ');
        let end = rest.find(' ').unwrap_or(rest.len());
        let (f, r) = rest.split_at(end);
        rest = r;
        (!f.is_empty()).then_some(f)
    };
    let range = field()?;
    let perms = field()?.as_bytes();
    let (_offset, _dev, _inode) = (field()?, field()?, field()?);
    let (start, end) = range.split_once('-')?;
    let start = usize::from_str_radix(start, 16).ok()?;
    let end = usize::from_str_radix(end, 16).ok()?;
    if perms.len() != 4 || end < start {
        return None;
    }
    Some(Mapping {
        start,
        end,
        exec: perms[2] == b'x',
        shared: perms[3] == b's',
        path: rest.trim_start_matches(' ').trim_end().to_string(),
    })
}

fn parse_maps(text: &str) -> Vec<Mapping> {
    text.lines().filter_map(parse_line).collect()
}

/// Whether mappings of `path` make up a loaded image: a file (not a memfd), or the vDSO.
fn is_image_path(path: &str) -> bool {
    (path.starts_with('/') && !path.starts_with("/memfd:")) || path == "[vdso]"
}

/// The images: every file with an executable mapping, and the vDSO, each spanning all of its
/// mappings, in address order.
fn images(maps: &[Mapping]) -> Vec<Module> {
    use std::collections::HashMap;
    let with_code: std::collections::HashSet<&str> = maps
        .iter()
        .filter(|m| m.exec && is_image_path(&m.path))
        .map(|m| m.path.as_str())
        .collect();
    let mut spans: HashMap<&str, (usize, usize)> = HashMap::new();
    for m in maps.iter().filter(|m| with_code.contains(m.path.as_str())) {
        let span = spans.entry(m.path.as_str()).or_insert((m.start, m.end));
        span.0 = span.0.min(m.start);
        span.1 = span.1.max(m.end);
    }
    let mut modules: Vec<Module> = spans
        .into_iter()
        .map(|(path, (base, end))| {
            let file = path.strip_suffix(" (deleted)").unwrap_or(path);
            let name = file.rsplit('/').next().unwrap_or(file).to_string();
            Module { name, base, size: end - base }
        })
        .collect();
    modules.sort_by_key(|m| m.base);
    modules
}

/// What `address` is, from parsed maps and their images; `None` if no mapping holds it.
fn kind_in(maps: &[Mapping], modules: &[Module], address: usize) -> Option<MemoryKind> {
    let at = maps.partition_point(|m| m.start <= address);
    let m = maps.get(at.checked_sub(1)?)?;
    if address >= m.end {
        return None;
    }
    if is_image_path(&m.path) {
        if let Some(image) = modules.iter().find(|i| address >= i.base && address < i.base + i.size) {
            return Some(MemoryKind::Image { base: image.base });
        }
    }
    if m.exec && !m.shared && m.path.is_empty() {
        return Some(MemoryKind::PrivateWritableExecutable { base: m.start });
    }
    Some(MemoryKind::Other)
}

struct MapsCache {
    at: Instant,
    maps: Vec<Mapping>,
    modules: Vec<Module>,
    /// The last page no mapping held, and when the maps were re-read for it.
    missed: Option<(usize, Instant)>,
}

static CACHE: Mutex<Option<MapsCache>> = Mutex::new(None);

/// How long an answer from a mapping the cache knows stays good.
pub(super) const CACHE_FRESH: Duration = Duration::from_secs(1);
/// How often the same page, answered `Other`, may re-read the maps. Only an image or a code cache
/// is answered from the cache: a sampled instruction pointer is in executable memory, so an address
/// the cache places in data or in no mapping at all is a mapping newer than the cache (a new code
/// cache, a library just loaded, over addresses something else held a moment ago) and re-reads at
/// once; this only bounds a caller that keeps asking about one address that really is data.
const MISS_RETRY: Duration = Duration::from_millis(50);

fn read_maps() -> SamplerResult<MapsCache> {
    let text = std::fs::read_to_string("/proc/self/maps").map_err(|error| SamplerError::Errno {
        operation: "memory_kind",
        api: "read /proc/self/maps",
        errno: error.raw_os_error().unwrap_or(libc::EIO),
    })?;
    let mut maps = parse_maps(&text);
    maps.sort_by_key(|m| m.start);
    let modules = images(&maps);
    Ok(MapsCache { at: Instant::now(), maps, modules, missed: None })
}

pub(super) fn memory_kind(address: usize) -> SamplerResult<MemoryKind> {
    let page = address & !0xFFF;
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(c) = cache.as_ref() {
        let found = kind_in(&c.maps, &c.modules, address);
        match found {
            Some(kind @ (MemoryKind::Image { .. } | MemoryKind::PrivateWritableExecutable { .. }))
                if c.at.elapsed() < CACHE_FRESH =>
            {
                return Ok(kind)
            }
            _ if c.missed.is_some_and(|(p, at)| p == page && at.elapsed() < MISS_RETRY) => {
                return Ok(found.unwrap_or(MemoryKind::Other))
            }
            _ => {}
        }
    }
    let mut fresh = read_maps()?;
    let kind = kind_in(&fresh.maps, &fresh.modules, address).unwrap_or(MemoryKind::Other);
    if kind == MemoryKind::Other {
        fresh.missed = Some((page, fresh.at));
    }
    *cache = Some(fresh);
    Ok(kind)
}

pub(super) fn modules() -> SamplerResult<Vec<Module>> {
    let fresh = read_maps()?;
    let modules = fresh.modules.clone();
    *CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fresh);
    Ok(modules)
}

// ---------------------------------------------------------------------------------------- memory

fn vm_errno(error: &crate::vm::VmError) -> i32 {
    match error {
        crate::vm::VmError::Os { source, .. } => source.code() as i32,
        _ => libc::EPROTO,
    }
}

pub(super) fn process_counters() -> SamplerResult<ProcessCounters> {
    // SAFETY: plain data.
    let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `usage` is writable.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return Err(errno_error("process_counters", "getrusage"));
    }
    let working_set = crate::vm::process_working_set().map_err(|e| SamplerError::Errno {
        operation: "process_counters",
        api: "/proc/self/statm",
        errno: vm_errno(&e),
    })?;
    let private_bytes = crate::vm::process_commit_charge().map_err(|e| SamplerError::Errno {
        operation: "process_counters",
        api: "/proc/self/smaps",
        errno: vm_errno(&e),
    })?;
    Ok(ProcessCounters {
        page_faults: (usage.ru_minflt as u64).saturating_add(usage.ru_majflt as u64),
        working_set,
        private_bytes,
    })
}

// ------------------------------------------------------------------------------ efficiency classes

/// A kernel CPU list (`0-3,8,10-11`) as the CPU numbers it names.
fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if b < a {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.trim().parse().ok()?),
        }
    }
    Some(out)
}

/// Classes from per-CPU capacities (`cpu_capacity`, ARM): each distinct capacity is a class, the
/// smallest capacity class 0.
fn classes_from_capacities(capacities: &[u32]) -> Vec<u8> {
    let mut distinct: Vec<u32> = capacities.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    capacities
        .iter()
        .map(|c| distinct.iter().position(|d| d == c).unwrap_or(0) as u8)
        .collect()
}

pub(super) fn efficiency_classes() -> SamplerResult<Vec<u8>> {
    // SAFETY: no memory.
    let configured = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    if configured <= 0 {
        return Err(errno_error("efficiency_classes", "sysconf(_SC_NPROCESSORS_CONF)"));
    }
    let cpus = configured as usize;
    // ARM big.LITTLE: a capacity per CPU.
    let capacities: Option<Vec<u32>> = (0..cpus)
        .map(|cpu| {
            std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"))
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .collect();
    if let Some(capacities) = capacities {
        return Ok(classes_from_capacities(&capacities));
    }
    // Intel hybrid: the efficiency cores are the `cpu_atom` PMU's CPUs.
    let mut classes = vec![0u8; cpus];
    if let Some(atoms) =
        std::fs::read_to_string("/sys/devices/cpu_atom/cpus").ok().and_then(|t| parse_cpu_list(&t))
    {
        classes.iter_mut().for_each(|c| *c = 1);
        for cpu in atoms {
            if let Some(c) = classes.get_mut(cpu) {
                *c = 0;
            }
        }
    }
    Ok(classes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
55d0c0a00000-55d0c0a40000 r--p 00000000 08:02 1311 /home/u/target/release/deps/perf-abc\n\
55d0c0a40000-55d0c0c00000 r-xp 00040000 08:02 1311 /home/u/target/release/deps/perf-abc\n\
55d0c0c00000-55d0c0c10000 rw-p 00200000 08:02 1311 /home/u/target/release/deps/perf-abc\n\
55d0c0c10000-55d0c0c20000 rw-p 00000000 00:00 0 \n\
55d0c1000000-55d0c1100000 rw-p 00000000 00:00 0                          [heap]\n\
7f0000000000-7f0000400000 rwxp 00000000 00:00 0 \n\
7f0000400000-7f0000500000 r--s 00000000 00:01 77                         /memfd:guest (deleted)\n\
7f0000500000-7f0000580000 r-xs 00000000 00:01 78                         /memfd:jit (deleted)\n\
7f0000600000-7f0000700000 r--p 00000000 08:02 99                         /usr/share/fonts/a font.ttf\n\
7f1000000000-7f1000028000 r--p 00000000 08:02 5                          /usr/lib/x86_64-linux-gnu/libc.so.6\n\
7f1000028000-7f10001bd000 r-xp 00028000 08:02 5                          /usr/lib/x86_64-linux-gnu/libc.so.6\n\
7f10001bd000-7f1000215000 r--p 001bd000 08:02 5                          /usr/lib/x86_64-linux-gnu/libc.so.6\n\
7ffc00000000-7ffc00002000 r-xp 00000000 00:00 0                          [vdso]\n\
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]\n";

    #[test]
    fn a_maps_line_is_parsed_with_a_path_that_contains_spaces() {
        let m = parse_line("7f0000600000-7f0000700000 r--p 00000000 08:02 99   /usr/share/fonts/a font.ttf")
            .expect("parsed");
        assert_eq!((m.start, m.end, m.exec, m.shared), (0x7f00_0060_0000, 0x7f00_0070_0000, false, false));
        assert_eq!(m.path, "/usr/share/fonts/a font.ttf");
        let anon = parse_line("7f0000000000-7f0000400000 rwxp 00000000 00:00 0 ").expect("parsed");
        assert!(anon.exec && !anon.shared && anon.path.is_empty());
        assert_eq!(parse_line("7f0000000000-7f0000400000 r--s 0 00:01 7 /memfd:x (deleted)").map(|m| m.shared), Some(true));
        assert_eq!(parse_line("garbage"), None);
        assert_eq!(parse_line("10-5 r--p 0 0:0 0"), None, "an end before its start");
        assert_eq!(parse_maps(MAPS).len(), 14);
    }

    #[test]
    fn images_are_files_with_code_and_the_vdso_spanning_all_their_mappings() {
        let maps = parse_maps(MAPS);
        let modules = images(&maps);
        let names: Vec<&str> = modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["perf-abc", "libc.so.6", "[vdso]"], "no font, no memfd, no vsyscall");
        assert_eq!((modules[0].base, modules[0].size), (0x55d0_c0a0_0000, 0x21_0000));
        assert_eq!((modules[1].base, modules[1].size), (0x7f10_0000_0000, 0x21_5000));
    }

    #[test]
    fn an_address_is_told_as_image_code_cache_or_other() {
        let maps = parse_maps(MAPS);
        let modules = images(&maps);
        let kind = |a: usize| kind_in(&maps, &modules, a);
        // The executable's data segment is part of its image, as Windows' MEM_IMAGE is.
        assert_eq!(kind(0x55d0_c0c0_0010), Some(MemoryKind::Image { base: 0x55d0_c0a0_0000 }));
        assert_eq!(kind(0x7f10_0003_0000), Some(MemoryKind::Image { base: 0x7f10_0000_0000 }));
        assert_eq!(kind(0x7ffc_0000_1000), Some(MemoryKind::Image { base: 0x7ffc_0000_0000 }));
        assert_eq!(
            kind(0x7f00_0012_3456),
            Some(MemoryKind::PrivateWritableExecutable { base: 0x7f00_0000_0000 })
        );
        // Anonymous data, the heap, a shared memfd, a data file: other.
        assert_eq!(kind(0x55d0_c0c1_0000), Some(MemoryKind::Other));
        assert_eq!(kind(0x55d0_c100_0000), Some(MemoryKind::Other));
        assert_eq!(kind(0x7f00_0040_0000), Some(MemoryKind::Other));
        assert_eq!(kind(0x7f00_0060_0000), Some(MemoryKind::Other));
        // Executable but shared, and a memfd: not an image and not a private code cache.
        assert_eq!(kind(0x7f00_0050_0000), Some(MemoryKind::Other));
        // In no mapping at all: unknown to these maps (the caller re-reads or says Other).
        assert_eq!(kind(0x7f00_0058_0000), None);
        assert_eq!(kind(0x1000), None);
    }

    /// The claim: a handler answers only the request whose number its own signal carries. A signal
    /// sent for any other number -- an earlier, withdrawn request -- must not answer the one that
    /// is armed now, or a sample would carry another moment's (or another thread's) registers.
    #[test]
    fn a_signal_for_another_request_is_not_answered() {
        let _serial = crate::sampler::serial();
        let _one = SAMPLING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let signal = install().expect("the sampling signal");
        let seq = next_sequence();
        ARMED.store(seq, Ordering::SeqCst);
        let before = DONE.load(Ordering::Acquire);
        // To this thread: delivered before the system call returns.
        assert_eq!(queue(gettid(), signal, seq.wrapping_add(1) & !CLAIMED), 0);
        assert_eq!(DONE.load(Ordering::Acquire), before, "another request's signal answered this one");
        assert_eq!(ARMED.load(Ordering::SeqCst), seq, "another request's signal claimed this one");
        assert_eq!(queue(gettid(), signal, seq), 0);
        assert_eq!(DONE.load(Ordering::Acquire), seq as u32, "the request's own signal was not answered");
        assert_eq!(ARMED.load(Ordering::SeqCst), seq | CLAIMED);
        assert_ne!(ANSWER_IP.load(Ordering::Relaxed), 0);
        ARMED.store(0, Ordering::SeqCst);
    }

    /// A thread that has the signal blocked cannot answer: the sample is `NotAnswered`, the
    /// request is withdrawn, and when the thread unblocks and the signal is finally delivered its
    /// answer is discarded -- then the next request is answered normally.
    #[test]
    fn a_thread_with_the_signal_blocked_is_not_answered_and_its_late_answer_is_discarded() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let _serial = crate::sampler::serial();
        let signal = install().expect("the sampling signal");
        let unblock = Arc::new(AtomicBool::new(false));
        let unblocked = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = {
            let (unblock, unblocked, stop) = (Arc::clone(&unblock), Arc::clone(&unblocked), Arc::clone(&stop));
            std::thread::spawn(move || {
                // SAFETY: plain data, filled by sigemptyset/sigaddset.
                let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
                // SAFETY: `set` is a live sigset_t; this thread's own mask is changed.
                unsafe {
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, signal);
                    assert_eq!(libc::pthread_sigmask(libc::SIG_BLOCK, &set, core::ptr::null_mut()), 0);
                }
                tx.send(Thread::current().expect("a handle")).expect("sent");
                while !unblock.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                // SAFETY: as above; the pending signal is delivered here.
                unsafe { assert_eq!(libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut()), 0) };
                unblocked.store(true, Ordering::Release);
                while !stop.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
            })
        };
        let thread = rx.recv().expect("the thread's handle");
        let mut code = [0u8; CODE_LEN];
        let result = thread.sample(&mut code);
        assert!(
            matches!(result, Err(SamplerError::NotAnswered { signal: s, .. }) if s == signal),
            "{result:?}"
        );
        let before = DONE.load(Ordering::Acquire);
        unblock.store(true, Ordering::Release);
        while !unblocked.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(DONE.load(Ordering::Acquire), before, "a withdrawn request was answered late");
        let sample = thread.sample(&mut code).expect("answered once the signal is unblocked");
        assert_ne!(sample.ip, 0);
        stop.store(true, Ordering::Release);
        handle.join().expect("joined");
    }

    /// Only an image or a code cache is answered from cached maps: memory the cache knew as data,
    /// made executable in place since (a code cache over addresses something else held a moment
    /// ago), is read again rather than answered from the old maps.
    #[test]
    fn memory_the_cache_knew_as_data_and_now_executable_is_read_again() {
        let _serial = crate::sampler::serial();
        const SIZE: usize = 0x10000;
        let anonymous = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        // SAFETY: a fresh anonymous mapping, unmapped below.
        let p = unsafe { libc::mmap(core::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, anonymous, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        assert_eq!(memory_kind(p as usize).expect("a kind"), MemoryKind::Other);
        let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
        // SAFETY: replaces the mapping made above, which this test owns.
        let q = unsafe { libc::mmap(p, SIZE, rwx, anonymous | libc::MAP_FIXED, -1, 0) };
        assert_eq!(q, p);
        std::thread::sleep(MISS_RETRY + Duration::from_millis(10));
        let kind = memory_kind(p as usize + 0x100).expect("a kind");
        // SAFETY: the mapping above, unmapped once.
        unsafe { libc::munmap(p, SIZE) };
        assert!(matches!(kind, MemoryKind::PrivateWritableExecutable { .. }), "{kind:?}");
    }

    #[test]
    fn cpu_lists_and_capacities_become_classes() {
        assert_eq!(parse_cpu_list("0-3,8,10-11\n"), Some(vec![0, 1, 2, 3, 8, 10, 11]));
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
        assert_eq!(classes_from_capacities(&[446, 446, 1024, 1024, 871]), vec![0, 0, 2, 2, 1]);
        assert_eq!(classes_from_capacities(&[1024; 4]), vec![0; 4]);
    }
}
