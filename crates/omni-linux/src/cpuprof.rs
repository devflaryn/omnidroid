//! `OMNI_THREAD_CPU=<seconds>`: per guest thread of this host process, the processor time it used
//! over each period and where its host instruction pointer was, sampled 100 times a second --
//! which thread holds a process's frame rate down, and whether it is running translated code, in
//! dynarmic (translating, looking up a block, a memory callback), in a kernel handler, or in a
//! host library (the GPU driver).
//!
//! Classes, from [`omni_platform::sampler`]: `jit` -- in a code cache; `kern` -- the task is in a
//! system call (`IN_KERNEL`); `dyn` -- in this executable while the task runs guest code; a DLL's
//! name -- in that module while the task runs guest code; `other` -- anywhere else. A thread that
//! did not run since the previous tick is not touched. Costs a suspend / resume per running thread
//! per tick (tens of microseconds); off, one relaxed load when a task starts running.
//!
//! `OMNI_GUEST_PROF=1` as well: each `jit` sample's host address is kept, and every report is
//! followed by `[guestprof]` lines naming the guest functions those samples ran ([`crate::guestprof`]).
//!
//! Each report is also followed by `[thread-sys]` lines: for the same threads, the wall and processor
//! time inside each system call, and which guest code their futex waits came from
//! ([`crate::threadsys`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use omni_platform::sampler::{self, HostThread, MemoryKind, CODE_AFTER, CODE_BEFORE};
use parking_lot::Mutex;

/// The report period, if `OMNI_THREAD_CPU` asks for one.
fn period() -> Option<u64> {
    static ON: OnceLock<Option<u64>> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_THREAD_CPU").ok().and_then(|v| v.parse().ok()))
}

struct Entry {
    host: HostThread,
    state: Arc<AtomicU8>,
    name: Vec<u8>,
    cycles: u64,
    cpu: Duration,
    /// Samples this period by class.
    classes: HashMap<String, u32>,
    /// `dyn` samples by offset in this executable (a 256-byte bucket), for symbolization.
    exe: HashMap<usize, u32>,
    /// `kern` samples likewise: where in the kernel's handlers (or a module: its name and offset).
    kern: HashMap<String, u32>,
    /// `OMNI_GUEST_PROF`: the host address of each `jit` sample this period.
    jit: Vec<u64>,
    /// The guest process the task belongs to, for its memory map.
    process: Option<Weak<crate::process::Process>>,
    /// The task's system-call counters (`[thread-sys]`), which its own thread writes.
    sys: Arc<crate::threadsys::Counters>,
    /// `cycles` at the previous report: the cycle counter's rate against `cpu` over the period.
    report_cycles: u64,
}

static THREADS: Mutex<Option<HashMap<i32, Entry>>> = Mutex::new(None);

/// The calling host thread runs task `tid` from now on (`Process::run_task`).
pub fn started(tid: i32, name: &[u8], state: &Arc<AtomicU8>, process: Option<Weak<crate::process::Process>>) {
    let Some(every) = period() else { return };
    static REPORTER: OnceLock<()> = OnceLock::new();
    REPORTER.get_or_init(|| {
        std::thread::spawn(move || sample_loop(every.max(1)));
    });
    let Ok(host) = HostThread::current() else { return };
    let cpu = host.cpu_time().unwrap_or_default();
    let report_cycles = host.cycles().unwrap_or(0);
    let sys = Arc::new(crate::threadsys::Counters::default());
    crate::threadsys::attach(Arc::clone(&sys));
    THREADS.lock().get_or_insert_with(HashMap::new).insert(
        tid,
        Entry {
            host,
            state: Arc::clone(state),
            name: name.to_vec(),
            cycles: 0,
            cpu,
            classes: HashMap::new(),
            exe: HashMap::new(),
            kern: HashMap::new(),
            jit: Vec::new(),
            process,
            sys,
            report_cycles,
        },
    );
}

/// Task `tid` is no longer run by its host thread.
pub fn stopped(tid: i32) {
    if period().is_some() {
        crate::threadsys::detach();
        if let Some(m) = THREADS.lock().as_mut() {
            m.remove(&tid);
        }
    }
}

/// Task `tid` renamed itself (`PR_SET_NAME`).
pub fn renamed(tid: i32, name: &[u8]) {
    if period().is_some() {
        if let Some(e) = THREADS.lock().as_mut().and_then(|m| m.get_mut(&tid)) {
            e.name = name.to_vec();
        }
    }
}

fn sample_loop(every: u64) {
    let modules = sampler::modules().unwrap_or_default();
    let exe_base = modules.first().map_or(0, |m| m.base);
    let module_of = |base: usize| modules.iter().find(|m| m.base == base).map_or_else(|| "image".to_string(), |m| m.name.clone());
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    let mut ticks = 0u64;
    let per_report = every * 100;
    let mut kinds: HashMap<usize, MemoryKind> = HashMap::new();
    let guest = crate::guestprof::enabled();
    let mut symbols = crate::guestprof::Symbolizer::default();
    loop {
        std::thread::sleep(Duration::from_millis(10));
        ticks += 1;
        // The samples, taken with the registry locked (no allocation while a thread is suspended:
        // `sample` resumes it before returning, and only then is anything recorded).
        let mut guard = THREADS.lock();
        let Some(map) = guard.as_mut() else { continue };
        for e in map.values_mut() {
            let Ok(c) = e.host.cycles() else { continue };
            if c == e.cycles {
                continue;
            }
            e.cycles = c;
            let kernel = e.state.load(Ordering::Relaxed) == crate::process::IN_KERNEL;
            let Ok(s) = e.host.sample(&mut code) else { continue };
            let page = s.ip & !0xfff;
            let kind = *kinds.entry(page).or_insert_with(|| sampler::memory_kind(s.ip).unwrap_or(MemoryKind::Other));
            if kernel {
                let at = match kind {
                    MemoryKind::Image { base } if base == exe_base => format!("+{:#x}", (s.ip - exe_base) & !0xff),
                    MemoryKind::Image { base } => format!("{}+{:#x}", module_of(base), (s.ip - base) & !0xff),
                    _ => "other".to_string(),
                };
                *e.kern.entry(at).or_default() += 1;
            }
            let class = match kind {
                _ if kernel => "kern".to_string(),
                MemoryKind::PrivateWritableExecutable { .. } => {
                    if guest {
                        e.jit.push(s.ip as u64);
                    }
                    "jit".to_string()
                }
                MemoryKind::Image { base } if base == exe_base => {
                    *e.exe.entry((s.ip - exe_base) & !0xff).or_default() += 1;
                    "dyn".to_string()
                }
                MemoryKind::Image { base } => module_of(base),
                MemoryKind::Other => "other".to_string(),
            };
            *e.classes.entry(class).or_default() += 1;
        }
        if ticks % per_report != 0 {
            continue;
        }
        if kinds.len() > 1 << 16 {
            kinds.clear();
        }
        // Every thread's system-call counters are taken (and so reset) each period; the hot ones
        // are reported.
        let mut sys: Vec<(i32, Arc<crate::threadsys::Counters>, f64)> = Vec::new();
        let mut lines: Vec<(Duration, String, Option<crate::guestprof::HotThread>, i32)> = map
            .iter_mut()
            .filter_map(|(tid, e)| {
                let now = e.host.cpu_time().ok()?;
                let used = now.saturating_sub(e.cpu);
                e.cpu = now;
                let cycles = e.host.cycles().unwrap_or(e.cycles);
                let ns_per_cycle = used.as_nanos() as f64 / (cycles.saturating_sub(e.report_cycles).max(1)) as f64;
                e.report_cycles = cycles;
                sys.push((*tid, Arc::clone(&e.sys), ns_per_cycle));
                let classes = std::mem::take(&mut e.classes);
                let exe = std::mem::take(&mut e.exe);
                let kern = std::mem::take(&mut e.kern);
                let jit = std::mem::take(&mut e.jit);
                if used < Duration::from_millis(every * 50) {
                    return None; // under 5% of a core
                }
                let total: u32 = classes.values().sum::<u32>().max(1);
                let guest_hot = guest.then(|| crate::guestprof::HotThread {
                    tid: *tid,
                    name: e.name.clone(),
                    samples: total,
                    jit,
                    process: e.process.clone(),
                });
                let mut cs: Vec<(String, u32)> = classes.into_iter().collect();
                cs.sort_by(|a, b| b.1.cmp(&a.1));
                let shares: Vec<String> = cs.iter().take(5).map(|(c, n)| format!("{c} {}%", n * 100 / total)).collect();
                let mut hot: Vec<(usize, u32)> = exe.into_iter().collect();
                hot.sort_by(|a, b| b.1.cmp(&a.1));
                let hot: Vec<String> = hot.iter().take(4).map(|(o, n)| format!("+{o:#x}:{n}")).collect();
                let mut kern: Vec<(String, u32)> = kern.into_iter().collect();
                kern.sort_by(|a, b| b.1.cmp(&a.1));
                let kern: Vec<String> = kern.iter().take(6).map(|(o, n)| format!("{o}:{n}")).collect();
                let pct = used.as_secs_f64() * 100.0 / every as f64;
                Some((
                    used,
                    format!(
                        "{tid} {:?} {pct:.0}%: {}{}{}",
                        String::from_utf8_lossy(&e.name),
                        shares.join(", "),
                        if hot.is_empty() { String::new() } else { format!(" (exe {})", hot.join(" ")) },
                        if kern.is_empty() { String::new() } else { format!(" (kern {})", kern.join(" ")) }
                    ),
                    guest_hot,
                    *tid,
                ))
            })
            .collect();
        let names: HashMap<i32, (Vec<u8>, Option<Weak<crate::process::Process>>)> =
            map.iter().map(|(tid, e)| (*tid, (e.name.clone(), e.process.clone()))).collect();
        drop(guard);
        lines.sort_by(|a, b| b.0.cmp(&a.0));
        let total: Duration = lines.iter().map(|l| l.0).sum();
        let mut hot = Vec::new();
        let hot_tids: Vec<i32> = lines.iter().take(12).map(|l| l.3).collect();
        let body: Vec<String> = lines
            .into_iter()
            .take(12)
            .map(|l| {
                hot.extend(l.2);
                l.1
            })
            .collect();
        eprintln!(
            "[thread-cpu] host pid {} {}s, {:.2} cores in busy threads:\n[thread-cpu]   {}",
            std::process::id(),
            every,
            total.as_secs_f64() / every as f64,
            body.join("\n[thread-cpu]   ")
        );
        // After the registry is let go: the lookups below take locks a guest thread may hold.
        if guest {
            let report = crate::guestprof::report(&hot, &mut symbols);
            if !report.is_empty() {
                eprint!("{report}");
            }
        }
        let periods: Vec<crate::threadsys::ThreadPeriod> = sys
            .into_iter()
            .filter_map(|(tid, counters, ns_per_cycle)| {
                let period = counters.take();
                let at = hot_tids.iter().position(|&t| t == tid)?;
                let (name, process) = names.get(&tid).cloned().unwrap_or_default();
                Some((at, crate::threadsys::ThreadPeriod { tid, name, period, ns_per_cycle, process }))
            })
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_values()
            .collect();
        let report = crate::threadsys::report(&periods, every as f64, &mut symbols);
        if !report.is_empty() {
            eprint!("{report}");
        }
    }
}
