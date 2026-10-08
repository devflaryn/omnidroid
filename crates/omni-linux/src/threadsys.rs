//! `[thread-sys]` (on with `OMNI_THREAD_CPU=<seconds>`): **where a hot thread's system-call time
//! goes** -- per guest thread, the wall time and processor time spent inside each call over the
//! period, futex waits apart from wakes and `ioctl` by device (GPU, binder), and for its futex waits
//! the guest code that waited (the return addresses at the call, named as `guestprof` names code).
//!
//! `[thread-cpu]` says a thread is `kern` 37% of its samples; that is processor time in a handler,
//! and says nothing of the time the thread spent *blocked* in one, which is what holds a frame back
//! when the render thread waits on a job. Here both are measured around `Process::syscall`.
//!
//! Measured (`tests/thread_sys.rs`, a thread alternating 2 ms of work with a 2 ms condvar wait):
//! `[thread-cpu]` said `kern 45%` while `[thread-sys]` said `futex(wait) 611 ms/s (cpu 5)` -- a
//! `kern` sample is any tick in which the thread ran at all and was found in a handler, so a thread
//! that wakes often and blocks shows its *blocked* time as `kern`. The `cpu` column is the
//! processor time actually spent in the calls.
//!
//! Cost, per system call of a thread being profiled: two `Instant::now` and two reads of the thread's
//! cycle counter (`QueryThreadCycleTime` / the thread CPU clock) plus three relaxed atomic adds, and
//! for a futex wait a walk of up to eight guest frames and an uncontended lock: ~0.7 us a call.
//! Nothing when `OMNI_THREAD_CPU` is unset or `OMNI_THREAD_SYS=0`: the thread has no counters and
//! `begin` is one thread-local read (~7 ns).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use omni_platform::sampler::HostThread;

use crate::syscall::nr;

const SLOTS: usize = 512;
const IOCTL_GPU: usize = 500;
const IOCTL_BINDER: usize = 501;
const FUTEX_WAIT: usize = 502;
const FUTEX_WAKE: usize = 503;
/// Return addresses kept per futex wait: the caller of the call (LR), then the frame-pointer chain.
pub const FRAMES: usize = 8;
/// Distinct waiting stacks kept per thread per period; beyond, a wait counts in its slot only.
const MAX_WAITERS: usize = 4096;

/// One thread's counters: written by the thread itself, read and reset by the report thread.
pub struct Counters {
    calls: [AtomicU64; SLOTS],
    wall_ns: [AtomicU64; SLOTS],
    cycles: [AtomicU64; SLOTS],
    /// Futex waits by the stack that waited: (calls, wall ns).
    waits: Mutex<HashMap<[u64; FRAMES], (u64, u64)>>,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            calls: [const { AtomicU64::new(0) }; SLOTS],
            wall_ns: [const { AtomicU64::new(0) }; SLOTS],
            cycles: [const { AtomicU64::new(0) }; SLOTS],
            waits: Mutex::new(HashMap::new()),
        }
    }
}

/// One slot's figures over a period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub name: String,
    pub calls: u64,
    pub wall_ns: u64,
    pub cycles: u64,
}

/// What [`Counters::take`] hands the report: the slots that saw calls, and the futex waits.
pub struct Period {
    pub slots: Vec<Slot>,
    pub waits: Vec<([u64; FRAMES], u64, u64)>,
}

impl Counters {
    /// The figures since the last take, and zero.
    pub fn take(&self) -> Period {
        let mut slots = Vec::new();
        for n in 0..SLOTS {
            let calls = self.calls[n].swap(0, Ordering::Relaxed);
            let wall_ns = self.wall_ns[n].swap(0, Ordering::Relaxed);
            let cycles = self.cycles[n].swap(0, Ordering::Relaxed);
            if calls > 0 {
                slots.push(Slot { name: slot_name(n), calls, wall_ns, cycles });
            }
        }
        let waits = std::mem::take(&mut *self.waits.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        Period { slots, waits: waits.into_iter().map(|(k, (c, ns))| (k, c, ns)).collect() }
    }

    fn add(&self, slot: usize, wall_ns: u64, cycles: u64) {
        self.calls[slot].fetch_add(1, Ordering::Relaxed);
        self.wall_ns[slot].fetch_add(wall_ns, Ordering::Relaxed);
        self.cycles[slot].fetch_add(cycles, Ordering::Relaxed);
    }
}

/// The slot a call is counted in: its number, except futex waits and wakes, and `ioctl` on the GPU
/// (`'G'`, one per Vulkan command) and binder (`'b'`).
#[must_use]
pub fn slot_of(number: u64, args: &[u64; 6]) -> usize {
    match number {
        nr::FUTEX => match args[1] & 0x7f {
            // WAIT, LOCK_PI, WAIT_BITSET, WAIT_REQUEUE_PI, LOCK_PI2: the calls that block.
            0 | 6 | 9 | 11 | 13 => FUTEX_WAIT,
            // WAKE, REQUEUE, CMP_REQUEUE, WAKE_OP, UNLOCK_PI, WAKE_BITSET.
            1 | 3 | 4 | 5 | 7 | 10 => FUTEX_WAKE,
            _ => nr::FUTEX as usize,
        },
        nr::IOCTL => match (args[1] >> 8) & 0xff {
            0x47 => IOCTL_GPU,
            0x62 => IOCTL_BINDER,
            _ => nr::IOCTL as usize,
        },
        n => (n as usize).min(SLOTS - 1),
    }
}

fn slot_name(n: usize) -> String {
    match n {
        IOCTL_GPU => "ioctl:gpu".into(),
        IOCTL_BINDER => "ioctl:binder".into(),
        FUTEX_WAIT => "futex(wait)".into(),
        FUTEX_WAKE => "futex(wake)".into(),
        n if n == nr::FUTEX as usize => "futex(other)".into(),
        n if n == nr::IOCTL as usize => "ioctl:other".into(),
        n => crate::syscall::name_of(n as u64).into_owned(),
    }
}

thread_local! {
    /// The counters of the task this host thread runs, and the thread itself for its cycle count.
    static ME: RefCell<Option<(HostThread, Arc<Counters>)>> = const { RefCell::new(None) };
}

/// `OMNI_THREAD_SYS=0` keeps `[thread-sys]` off while `OMNI_THREAD_CPU` is on: timing a call costs
/// its thread ~0.7 us (measured on this host's E-cores, `bench_the_cost_of_timing_a_call`; mostly
/// the two cycle-counter reads), ~1% of a core at 15,000 GPU ioctls a second.
fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_THREAD_SYS").map_or(true, |v| v.trim() != "0"))
}

/// From now on this host thread's calls are counted in `counters` (`cpuprof::started`).
pub fn attach(counters: Arc<Counters>) {
    if !enabled() {
        return;
    }
    if let Ok(host) = HostThread::current() {
        ME.with(|me| *me.borrow_mut() = Some((host, counters)));
    }
}

/// This host thread's calls are no longer counted.
pub fn detach() {
    let _ = ME.try_with(|me| me.borrow_mut().take());
}

/// A call being timed.
pub struct Call {
    started: Instant,
    cycles: u64,
}

/// Start timing a call, if this thread is counted.
#[inline]
#[must_use]
pub fn begin() -> Option<Call> {
    ME.with(|me| {
        let me = me.borrow();
        let (host, _) = me.as_ref()?;
        Some(Call { cycles: host.cycles().unwrap_or(0), started: Instant::now() })
    })
}

/// The call `call` timed has returned: count it. `frames` is asked for (only for a futex wait) the
/// return addresses of the guest code that made it.
pub fn end(call: Call, number: u64, args: &[u64; 6], frames: impl FnOnce() -> [u64; FRAMES]) {
    let wall = call.started.elapsed().as_nanos() as u64;
    ME.with(|me| {
        let me = me.borrow();
        let Some((host, counters)) = me.as_ref() else { return };
        let cycles = host.cycles().unwrap_or(call.cycles).saturating_sub(call.cycles);
        let slot = slot_of(number, args);
        counters.add(slot, wall, cycles);
        if slot == FUTEX_WAIT {
            let key = frames();
            let mut waits = counters.waits.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let full = waits.len() >= MAX_WAITERS;
            if let Some(w) = waits.get_mut(&key) {
                w.0 += 1;
                w.1 += wall;
            } else if !full {
                waits.insert(key, (1, wall));
            }
        }
    });
}

/// The return addresses of the guest code making a call: `lr`, then the frame-pointer chain from
/// `fp`, read from the process's memory (pointer authentication and tag bits cleared).
pub fn guest_frames(p: &crate::process::Process, lr: u64, mut fp: u64) -> [u64; FRAMES] {
    const ADDRESS: u64 = 0x00ff_ffff_ffff_ffff;
    let mut out = [0u64; FRAMES];
    out[0] = lr & ADDRESS;
    for slot in out.iter_mut().skip(1) {
        if fp == 0 || fp & 7 != 0 {
            break;
        }
        let (Ok(next), Ok(ret)) = (p.mem.read_u64(fp), p.mem.read_u64(fp + 8)) else { break };
        if ret == 0 {
            break;
        }
        *slot = ret & ADDRESS;
        if next <= fp {
            break; // the chain goes up the stack, or it is not a chain
        }
        fp = next;
    }
    out
}

/// One thread's figures over a period, for [`report`].
pub struct ThreadPeriod {
    pub tid: i32,
    pub name: Vec<u8>,
    pub period: Period,
    /// Nanoseconds of processor time per cycle-counter unit, over this period (1 on Linux).
    pub ns_per_cycle: f64,
    pub process: Option<std::sync::Weak<crate::process::Process>>,
}

/// Libraries whose frames are the waiting mechanism rather than the code that waits: skipped when
/// naming the waiter (and named after it, `via ...`).
fn is_plumbing(lib: &str) -> bool {
    matches!(lib, "libc.so" | "libc++.so" | "libc++_shared.so" | "libbase.so" | "libutils.so" | "libdl.so")
}

/// Waiting stacks shown per thread.
const TOP_WAITERS: usize = 5;

/// The `[thread-sys]` lines for the period's hot threads, `seconds` long.
pub fn report(threads: &[ThreadPeriod], seconds: f64, symbols: &mut crate::guestprof::Symbolizer) -> String {
    let mut out = String::new();
    let per_s = |ns: u64| ns as f64 / 1e6 / seconds;
    for t in threads {
        let mut slots = t.period.slots.clone();
        if slots.is_empty() {
            continue;
        }
        slots.sort_by(|a, b| b.wall_ns.cmp(&a.wall_ns));
        let wall: u64 = slots.iter().map(|s| s.wall_ns).sum();
        let cycles: u64 = slots.iter().map(|s| s.cycles).sum();
        let cpu = |c: u64| c as f64 * t.ns_per_cycle / 1e6 / seconds;
        let rows: Vec<String> = slots
            .iter()
            .take(8)
            .map(|s| format!("{} {:.0} ms/s {:.0}/s (cpu {:.0})", s.name, per_s(s.wall_ns), s.calls as f64 / seconds, cpu(s.cycles)))
            .collect();
        let _ = writeln!(
            out,
            "[thread-sys] {} {:?}: in calls {:.0} ms/s (cpu {:.0}): {}",
            t.tid,
            String::from_utf8_lossy(&t.name),
            per_s(wall),
            cpu(cycles),
            rows.join(", ")
        );
        let mut waits = t.period.waits.clone();
        waits.sort_by(|a, b| b.2.cmp(&a.2));
        let process = t.process.as_ref().and_then(std::sync::Weak::upgrade);
        for (frames, calls, ns) in waits.iter().take(TOP_WAITERS) {
            let named: Vec<crate::guestprof::Place> = frames
                .iter()
                .take_while(|&&f| f != 0)
                // A return address is the instruction after the call: the call is in the function before it.
                .map(|&f| match &process {
                    Some(p) => symbols.place(p, f.saturating_sub(4)),
                    None => crate::guestprof::Place { lib: "[exited]".into(), offset: f, symbol: None },
                })
                .collect();
            let first = named.iter().position(|p| !is_plumbing(&p.lib)).unwrap_or(0);
            let show = |p: &crate::guestprof::Place| format!("{}+{:#x} {}", p.lib, p.offset, p.symbol.as_deref().unwrap_or("?"));
            let chain: Vec<String> = named.iter().skip(first).take(3).map(show).collect();
            let via = if first > 0 { format!(" (via {})", show(&named[first - 1])) } else { String::new() };
            let _ = writeln!(
                out,
                "[thread-sys]   futex wait {:.0} ms/s, {:.0}/s: {}{via}",
                per_s(*ns),
                *calls as f64 / seconds,
                chain.join(" < ")
            );
        }
    }
    if !out.is_empty() {
        if let Ok(mut recent) = RECENT.lock() {
            if recent.len() == 8 {
                recent.remove(0);
            }
            recent.push(out.clone());
        }
    }
    out
}

static RECENT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The last few `[thread-sys]` reports this process made, oldest first (for a test).
pub fn recent_reports() -> Vec<String> {
    RECENT.lock().map(|r| r.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn futex_waits_and_wakes_and_device_ioctls_have_slots_of_their_own() {
        let with = |a1: u64| [0, a1, 0, 0, 0, 0];
        // FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME
        assert_eq!(slot_of(nr::FUTEX, &with(9 | 128 | 256)), FUTEX_WAIT);
        assert_eq!(slot_of(nr::FUTEX, &with(128)), FUTEX_WAIT);
        assert_eq!(slot_of(nr::FUTEX, &with(1 | 128)), FUTEX_WAKE);
        assert_eq!(slot_of(nr::FUTEX, &with(10)), FUTEX_WAKE);
        assert_eq!(slot_name(slot_of(nr::FUTEX, &with(2))), "futex(other)");
        assert_eq!(slot_of(nr::IOCTL, &with(0xc018_4701)), IOCTL_GPU);
        assert_eq!(slot_of(nr::IOCTL, &with(0xc030_6201)), IOCTL_BINDER);
        assert_eq!(slot_name(slot_of(nr::IOCTL, &with(0x5401))), "ioctl:other");
        assert_eq!(slot_name(slot_of(nr::READ, &with(0))), "read");
    }

    /// What timing one call costs its thread (`cargo test --release -- --ignored --nocapture`).
    #[test]
    #[ignore = "a measurement"]
    fn bench_the_cost_of_timing_a_call() {
        let n = 200_000u32;
        let c = Arc::new(Counters::default());
        attach(c);
        let t0 = Instant::now();
        for _ in 0..n {
            let call = begin().expect("counted");
            end(call, nr::GETPID, &[0; 6], || [0; FRAMES]);
        }
        let on = t0.elapsed();
        detach();
        let t0 = Instant::now();
        for _ in 0..n {
            assert!(std::hint::black_box(begin()).is_none());
        }
        let off = t0.elapsed();
        eprintln!("timing a call: {:.0} ns counted, {:.1} ns not", on.as_nanos() as f64 / f64::from(n), off.as_nanos() as f64 / f64::from(n));
    }

    #[test]
    fn a_counted_thread_s_calls_and_waits_are_taken_once() {
        let c = Arc::new(Counters::default());
        attach(Arc::clone(&c));
        let wait = [0, 128, 0, 0, 0, 0];
        for _ in 0..3 {
            let call = begin().expect("this thread is counted");
            std::thread::sleep(std::time::Duration::from_millis(2));
            end(call, nr::FUTEX, &wait, || [0x1000, 0x2000, 0, 0, 0, 0, 0, 0]);
        }
        let call = begin().expect("counted");
        end(call, nr::GETPID, &[0; 6], || unreachable!("frames are for futex waits only"));
        detach();
        assert!(begin().is_none(), "detached");
        let p = c.take();
        let wait = p.slots.iter().find(|s| s.name == "futex(wait)").expect("the waits");
        assert_eq!(wait.calls, 3);
        assert!(wait.wall_ns >= 6_000_000, "{wait:?}");
        assert!(p.slots.iter().any(|s| s.name == "getpid" && s.calls == 1));
        assert_eq!(p.waits.len(), 1);
        assert_eq!(p.waits[0].1, 3);
        assert!(c.take().slots.is_empty(), "taken once");
    }
}
