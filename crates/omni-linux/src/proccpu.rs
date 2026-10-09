//! `OMNI_PROC_CPU=<seconds>`: **where a whole host process's processor time goes**, every thread of
//! it -- guest tasks and the runtime's own host threads alike -- named and grouped, with the
//! hottest few sampled and their host code named from the PDB.
//!
//! `OMNI_THREAD_CPU` follows guest tasks only and reports those above 5% of a core. The system's
//! host process in a world burns 1.5-1.8 cores while it reported ~0.2 "in busy threads": the rest
//! is spread over hundreds of guest threads each below the threshold (wake-up herds: ~700 waiting
//! threads, 24k wake-ups a second) and over host threads it does not see (the binder host pool,
//! the composer, presentation, the window, the release and watcher threads...). This lists them
//! all, once a period:
//!
//! ```text
//! [proc-cpu] pid 1234 10s: 1.62 cores; guest threads 0.90 (712), host threads 0.70 (96)
//! [proc-cpu]   guest: binder:1020_3 4.1%, RenderThread 3.0%, ...
//! [proc-cpu]   host: omni-binder-host 0.30 (14), start nvwgf2umx.dll!OpenAdapter12 0.08 (6), ...
//! [proc-cpu]   sampled 5678 "omni-binder-host-3" 31%: omni_linux::poll::Watch::sleep 12%, jit 0%, ...
//! ```
//!
//! A thread's share is its cycle count's share of the period's, scaled to the process's processor
//! time (`GetProcessTimes`): the cycle counter is exact per thread, the process time exact in sum.
//! Host threads group by name with a trailing number dropped (`omni-binder-host-3` ->
//! `omni-binder-host`); an unnamed one by what it started at (`start ntdll.dll!TppWorkerThread`).
//! Guest threads are known by `cpuprof::started` registering each task's host thread.
//!
//! The `OMNI_PROC_CPU_SAMPLE` (default 4) hottest threads of a period are sampled at 100 Hz through
//! the next (a suspend and resume each, as `OMNI_THREAD_CPU` does), each sample named by function.
//! Everything else is one thread snapshot a period (a few ms for 1,000 threads) on its own thread;
//! nothing is added to any other thread's path but a registry insert when a task starts.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use omni_platform::sampler::{self, HostThread, MemoryKind, Module, CODE_AFTER, CODE_BEFORE};
use parking_lot::Mutex;

/// The report period, if `OMNI_PROC_CPU` asks for one.
fn period() -> Option<u64> {
    static ON: OnceLock<Option<u64>> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_PROC_CPU").ok().and_then(|v| v.trim().parse().ok()).filter(|&s| s > 0))
}

/// How many of a period's hottest threads are sampled through the next (`OMNI_PROC_CPU_SAMPLE`).
fn sample_count() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("OMNI_PROC_CPU_SAMPLE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(4))
}

/// Whether host code is named from debug information (`OMNI_HOST_SYMBOLS=0`: not; the first lookup
/// loads the executable's PDB into dbghelp, some tens of MB, once).
pub fn host_symbols() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_HOST_SYMBOLS").map_or(true, |v| v.trim() != "0"))
}

/// Guest tasks by the host thread that runs them: os id -> (tid, name).
type GuestThreads = HashMap<u32, (i32, Vec<u8>)>;
static GUEST: Mutex<Option<GuestThreads>> = Mutex::new(None);

/// Start the reporter, if `OMNI_PROC_CPU` asks for it. Called once, at start-up.
pub fn start() {
    let Some(every) = period() else { return };
    let _ = std::thread::Builder::new().name("omni-proc-cpu".into()).spawn(move || {
        let mut p = Profiler::default();
        let _ = p.period(); // the baseline
        let ticks = every * 100;
        loop {
            for _ in 0..ticks {
                std::thread::sleep(Duration::from_millis(10));
                p.tick();
            }
            if let Some(report) = p.period() {
                eprint!("{report}");
            }
        }
    });
}

/// The calling host thread runs guest task `tid` from now on.
pub fn task_started(tid: i32, name: &[u8]) {
    if period().is_some() {
        GUEST.lock().get_or_insert_with(HashMap::new).insert(sampler::current_thread_id(), (tid, name.to_vec()));
    }
}

/// The calling host thread no longer runs a guest task.
pub fn task_stopped() {
    if period().is_some() {
        if let Some(m) = GUEST.lock().as_mut() {
            m.remove(&sampler::current_thread_id());
        }
    }
}

/// Guest task `tid` renamed itself.
pub fn task_renamed(tid: i32, name: &[u8]) {
    if period().is_some() {
        if let Some((_, n)) = GUEST.lock().as_mut().and_then(|m| m.values_mut().find(|(t, _)| *t == tid)) {
            *n = name.to_vec();
        }
    }
}

/// A host thread's name with a trailing instance number dropped: what threads of one pool share.
#[must_use]
pub fn group_of(name: &str) -> &str {
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() == name.len() {
        return name;
    }
    let trimmed = trimmed.trim_end_matches(['-', '_', ' ', '#', ':', '.']);
    if trimmed.is_empty() { name } else { trimmed }
}

/// Names host code addresses: `function` in this executable, `module!function` elsewhere,
/// `module+0xoffset` with no symbol. Kept, so each address is looked up once.
#[derive(Default)]
pub struct HostNames {
    modules: Vec<Module>,
    exe_base: usize,
    names: HashMap<usize, String>,
    exact: Option<bool>,
}

impl HostNames {
    /// Whether this executable's names are exact: its PDB has private symbols (a build with debug
    /// information, e.g. `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`). A release build's PDB has
    /// public symbols only, and an address in a private function is named after the nearest public
    /// one before it -- a plausible, wrong name. Checked once, on a private function of this crate.
    pub fn exact(&mut self) -> bool {
        *self.exact.get_or_insert_with(|| {
            host_symbols() && sampler::symbolize(private_probe as fn() -> u64 as usize).is_some_and(|(n, _)| demangle(&n).contains("private_probe"))
        })
    }

    /// A note for a report whose names are not exact, empty otherwise.
    pub fn caveat(&mut self) -> &'static str {
        if !host_symbols() || self.exact() {
            ""
        } else {
            " (host names approximate: the PDB has public symbols only; build with CARGO_PROFILE_RELEASE_DEBUG=line-tables-only)"
        }
    }

    /// The name of host address `ip`.
    pub fn name(&mut self, ip: usize) -> String {
        if let Some(n) = self.names.get(&ip) {
            return n.clone();
        }
        let mut module = self.modules.iter().find(|m| ip >= m.base && ip < m.base + m.size);
        if module.is_none() {
            // Loaded since the list was read.
            self.modules = sampler::modules().unwrap_or_default();
            self.exe_base = self.modules.first().map_or(0, |m| m.base);
            module = self.modules.iter().find(|m| ip >= m.base && ip < m.base + m.size);
        }
        let in_exe = module.is_some_and(|m| m.base == self.exe_base);
        // A name far from its symbol's start is most likely the nearest *public* symbol of a PDB
        // without private ones: shown with its distance, so it is not read as the function.
        let symbol = if host_symbols() { sampler::symbolize(ip) } else { None }.map(|(f, at)| {
            let f = demangle(&f);
            (if at > 0x4000 { format!("{f}+{:#x}", at & !0xff) } else { f }, at)
        });
        let name = match (symbol, module) {
            (Some((f, _)), _) if in_exe => f,
            (Some((f, _)), Some(m)) => format!("{}!{f}", m.name),
            (None, Some(m)) => format!("{}+{:#x}", m.name, ip - m.base),
            (Some((f, _)), None) => f,
            (None, None) => format!("{ip:#x}"),
        };
        if self.names.len() > 200_000 {
            self.names.clear();
        }
        self.names.insert(ip, name.clone());
        name
    }
}

/// A private function, for [`HostNames::exact`]: only a PDB with private symbols names it.
#[inline(never)]
fn private_probe() -> u64 {
    std::hint::black_box(0x5ca1_ab1e_0d15_ea5e)
}

/// A Rust legacy-mangled name (`_ZN4core3ptr13drop_in_place$LT$T$GT$17h0123456789abcdefE`, as a
/// PDB's public symbols keep them, leading `_` or not, `.llvm.<n>` suffix or not) as a path:
/// `core::ptr::drop_in_place<T>`. Anything else is returned as it is.
#[must_use]
pub fn demangle(name: &str) -> String {
    let Some(mut rest) = name.strip_prefix("_ZN").or_else(|| name.strip_prefix("ZN")) else { return name.to_string() };
    let mut parts: Vec<String> = Vec::new();
    loop {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        let Ok(len) = rest[..digits].parse::<usize>() else { return name.to_string() };
        let Some(part) = rest.get(digits..digits + len) else { return name.to_string() };
        parts.push(part.to_string());
        rest = &rest[digits + len..];
    }
    if !rest.starts_with('E') || parts.is_empty() {
        return name.to_string();
    }
    // The last part is the hash, `h` and 16 hex digits.
    if parts.last().is_some_and(|h| h.len() == 17 && h.starts_with('h') && h[1..].bytes().all(|b| b.is_ascii_hexdigit())) {
        parts.pop();
    }
    let unescape = |p: &str| {
        let mut p = p.strip_prefix('_').filter(|q| q.starts_with('$')).unwrap_or(p).to_string();
        for (from, to) in [
            ("$LT$", "<"),
            ("$GT$", ">"),
            ("$RF$", "&"),
            ("$BP$", "*"),
            ("$C$", ","),
            ("$SP$", "@"),
            ("$LP$", "("),
            ("$RP$", ")"),
            ("$u20$", " "),
            ("$u27$", "'"),
            ("$u5b$", "["),
            ("$u5d$", "]"),
            ("$u7b$", "{"),
            ("$u7d$", "}"),
            ("$u7e$", "~"),
            ("..", "::"),
        ] {
            p = p.replace(from, to);
        }
        p
    };
    parts.iter().map(|p| unescape(p)).collect::<Vec<_>>().join("::")
}

/// A sampled thread: its handle, the last cycle count seen, and the period's samples.
struct Sampled {
    host: HostThread,
    label: String,
    cycles: u64,
    ips: Vec<usize>,
    jit: u32,
    other: u32,
}

/// The reporter's state across periods.
#[derive(Default)]
pub struct Profiler {
    /// The previous snapshot's cycle count per thread.
    previous: HashMap<u32, u64>,
    process_cpu: Option<Duration>,
    /// When the previous period ended, and the cycle clock then (`sampler::cycle_clock`).
    clock: Option<(std::time::Instant, u64)>,
    /// Each host thread's name, or the symbol it started at.
    labels: HashMap<u32, String>,
    /// The threads whose label is their name, which is not read again. An unnamed thread is asked
    /// again each period: `std` names a new thread from the thread itself, after it has started, so
    /// a snapshot can see it before its name.
    named: std::collections::HashSet<u32>,
    sampled: Vec<Sampled>,
    names: HostNames,
    code: Vec<u8>,
}

impl Profiler {
    /// One sampling tick: each sampled thread that ran since the last tick is sampled once.
    pub fn tick(&mut self) {
        if self.code.is_empty() {
            self.code = vec![0; CODE_BEFORE + CODE_AFTER];
        }
        let code: &mut [u8; CODE_BEFORE + CODE_AFTER] = (&mut self.code[..]).try_into().expect("the code buffer's size");
        for s in &mut self.sampled {
            let Ok(c) = s.host.cycles() else { continue };
            if c == s.cycles {
                continue;
            }
            s.cycles = c;
            let Ok(sample) = s.host.sample(code) else { continue };
            match sampler::memory_kind(sample.ip) {
                Ok(MemoryKind::PrivateWritableExecutable { .. }) => s.jit += 1,
                Ok(MemoryKind::Image { .. }) => s.ips.push(sample.ip),
                _ => s.other += 1,
            }
        }
    }

    /// End a period: the report (`None` for the first, which only takes the baseline), and the next
    /// period's threads to sample chosen.
    pub fn period(&mut self) -> Option<String> {
        let snapshot_started = std::time::Instant::now();
        let named = &self.named;
        let threads = sampler::threads(&mut |id| !named.contains(&id)).ok()?;
        let cpu = sampler::process_cpu_time().ok()?;
        let guests = GUEST.lock().clone().unwrap_or_default();
        let snapshot_ms = snapshot_started.elapsed().as_secs_f64() * 1e3;
        for t in &threads {
            if self.named.contains(&t.os_id) {
                continue;
            }
            let label = match (&t.name, t.start) {
                (Some(n), _) => {
                    self.named.insert(t.os_id);
                    n.clone()
                }
                (None, 0) => "(unnamed)".to_string(),
                // Without its distance from the symbol: threads started at one place group together.
                (None, start) => format!("start {}", self.names.name(start).split("+0x").next().unwrap_or_default()),
            };
            self.labels.insert(t.os_id, label);
        }
        let live: std::collections::HashSet<u32> = threads.iter().map(|t| t.os_id).collect();
        self.labels.retain(|id, _| live.contains(id));
        self.named.retain(|id| live.contains(id));

        let first = self.process_cpu.is_none();
        let cpu_used = cpu.saturating_sub(self.process_cpu.unwrap_or_default());
        self.process_cpu = Some(cpu);
        let now = (std::time::Instant::now(), sampler::cycle_clock());
        let (then, then_clock) = self.clock.replace(now).unwrap_or(now);
        let seconds = now.0.duration_since(then).as_secs_f64().max(1e-3);
        // The cycle counter's rate over the period: a thread's cycles are its processor time.
        let per_second = now.1.saturating_sub(then_clock) as f64 / seconds;
        let deltas: Vec<(u32, u64)> = threads.iter().map(|t| (t.os_id, t.cycles.saturating_sub(*self.previous.get(&t.os_id).unwrap_or(&0)))).collect();
        self.previous = threads.iter().map(|t| (t.os_id, t.cycles)).collect();
        // Each thread's processor time, in cores.
        let cores = |cycles: u64| cycles as f64 / per_second.max(1.0) / seconds;

        let sampled = std::mem::take(&mut self.sampled);
        // The next period's sampled threads: this period's hottest (not this one).
        let mut hottest: Vec<(u32, u64)> = deltas.iter().copied().filter(|&(id, _)| id != sampler::current_thread_id()).collect();
        hottest.sort_by(|a, b| b.1.cmp(&a.1));
        for &(id, _) in hottest.iter().take(sample_count()) {
            if let Ok(host) = HostThread::open(id) {
                let label = match guests.get(&id) {
                    Some((tid, name)) => format!("{tid} {:?} (guest)", String::from_utf8_lossy(name)),
                    None => format!("{id} {:?}", self.labels.get(&id).map_or("", String::as_str)),
                };
                let cycles = host.cycles().unwrap_or(0);
                self.sampled.push(Sampled { host, label, cycles, ips: Vec::new(), jit: 0, other: 0 });
            }
        }
        if first {
            return None;
        }

        // Guest threads by name, host threads by group.
        let (mut guest_total, mut guest_count, mut host_total, mut host_count) = (0u64, 0usize, 0u64, 0usize);
        let mut guest_names: HashMap<String, (u64, usize)> = HashMap::new();
        let mut host_groups: HashMap<String, (u64, usize)> = HashMap::new();
        for &(id, cycles) in &deltas {
            if let Some((_, name)) = guests.get(&id) {
                guest_total += cycles;
                guest_count += 1;
                let name = if name.is_empty() { "(unnamed)".into() } else { String::from_utf8_lossy(name).into_owned() };
                let g = guest_names.entry(name).or_default();
                g.0 += cycles;
                g.1 += 1;
            } else {
                host_total += cycles;
                host_count += 1;
                let label = self.labels.get(&id).map_or("(unnamed)", String::as_str);
                let g = host_groups.entry(group_of(label).to_string()).or_default();
                g.0 += cycles;
                g.1 += 1;
            }
        }
        let top = |m: HashMap<String, (u64, usize)>, n: usize, as_pct: bool| -> String {
            let mut v: Vec<(String, (u64, usize))> = m.into_iter().collect();
            v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
            v.iter()
                .take(n)
                .map(|(name, (c, k))| {
                    let count = if *k > 1 { format!(" ({k})") } else { String::new() };
                    if as_pct {
                        format!("{name} {:.1}%{count}", cores(*c) * 100.0)
                    } else {
                        format!("{name} {:.2}{count}", cores(*c))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let caveat = self.names.caveat();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "[proc-cpu] pid {} {seconds:.0}s: {:.2} cores; guest threads {:.2} ({guest_count}), host threads {:.2} ({host_count}); snapshot {:.1} ms{}",
            std::process::id(),
            cpu_used.as_secs_f64() / seconds,
            cores(guest_total),
            cores(host_total),
            snapshot_ms,
            caveat,
        );
        if guest_count > 0 {
            let _ = writeln!(out, "[proc-cpu]   guest: {}", top(guest_names, 12, true));
        }
        let _ = writeln!(out, "[proc-cpu]   host: {}", top(host_groups, 12, false));
        for s in sampled {
            let named = s.ips.len() as u32;
            if named + s.jit + s.other == 0 {
                continue;
            }
            let total = named + s.jit + s.other;
            let mut by: HashMap<String, u32> = HashMap::new();
            for ip in s.ips {
                *by.entry(self.names.name(ip)).or_default() += 1;
            }
            if s.jit > 0 {
                by.insert("jit".into(), s.jit);
            }
            if s.other > 0 {
                by.insert("other".into(), s.other);
            }
            let mut by: Vec<(String, u32)> = by.into_iter().collect();
            by.sort_by(|a, b| b.1.cmp(&a.1));
            let rows: Vec<String> = by.iter().take(8).map(|(n, k)| format!("{n} {}%", k * 100 / total)).collect();
            let _ = writeln!(out, "[proc-cpu]   sampled {} {total} samples: {}", s.label, rows.join(", "));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(windows)]
    use std::sync::Arc;

    /// The two tests that measure spinning threads, one at a time: each one's spinners would be the
    /// other's hottest threads.
    #[cfg(windows)]
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn a_pool_s_instance_numbers_are_dropped_from_its_group() {
        assert_eq!(group_of("omni-binder-host-3"), "omni-binder-host");
        assert_eq!(group_of("binder:1020_3"), "binder:1020");
        assert_eq!(group_of("omni-display-present"), "omni-display-present");
        assert_eq!(group_of("1234"), "1234");
    }

    #[test]
    fn a_legacy_mangled_rust_name_reads_as_its_path() {
        assert_eq!(
            demangle("ZN4core3ptr52drop_in_place$LT$omni_platform..vm..MappableFile$GT$17hd6d7f0854852cfa6E.llvm.9793523885739779271"),
            "core::ptr::drop_in_place<omni_platform::vm::MappableFile>"
        );
        assert_eq!(demangle("_ZN10omni_linux4poll5Watch5sleep17h0123456789abcdefE"), "omni_linux::poll::Watch::sleep");
        assert_eq!(demangle("NtWaitForAlertByThreadId"), "NtWaitForAlertByThreadId");
        assert_eq!(demangle("omni_linux::poll::Watch::sleep"), "omni_linux::poll::Watch::sleep");
    }

    #[cfg(windows)]
    #[inline(never)]
    fn proccpu_test_spin_body(stop: &AtomicBool) -> u64 {
        let mut x = 1u64;
        while !stop.load(Ordering::Relaxed) {
            for _ in 0..1000 {
                x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
            }
        }
        x
    }

    /// Two named spinning threads show as one group of two with about two cores, and the sampled
    /// one's samples are named after the function it spins in (from this test binary's PDB).
    #[test]
    #[cfg(windows)]
    fn every_thread_is_counted_by_its_group_and_a_hot_one_is_named_by_function() {
        let _one = SERIAL.lock();
        let stop = Arc::new(AtomicBool::new(false));
        let spinners: Vec<_> = (1..=2)
            .map(|i| {
                let stop = Arc::clone(&stop);
                std::thread::Builder::new().name(format!("proccpu-test-spin-{i}")).spawn(move || proccpu_test_spin_body(&stop)).unwrap()
            })
            .collect();
        std::thread::sleep(Duration::from_millis(100));
        let mut p = Profiler::default();
        assert!(p.period().is_none(), "the first period is the baseline");
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(10));
            p.tick();
        }
        let report = p.period().expect("a report");
        stop.store(true, Ordering::Relaxed);
        for s in spinners {
            s.join().unwrap();
        }
        eprint!("{report}");
        let host = report.lines().find(|l| l.contains("  host: ")).expect("the host line");
        let group = host.split(", ").find(|g| g.contains("proccpu-test-spin ")).expect("the spinners' group");
        let cores: f64 = group.split("proccpu-test-spin ").nth(1).and_then(|c| c.split(' ').next()).and_then(|c| c.parse().ok()).unwrap_or(0.0);
        assert!(group.ends_with("(2)"), "two threads in one group: {group}");
        assert!((1.7..=2.1).contains(&cores), "two spinning threads use two cores: {group}");
    }

    /// The sampled threads (chosen at a period's end) are named by function through the next.
    #[test]
    #[cfg(windows)]
    fn a_sampled_thread_s_samples_are_named_after_its_function() {
        let _one = SERIAL.lock();
        let stop = Arc::new(AtomicBool::new(false));
        let spinner = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new().name("proccpu-test-named".into()).spawn(move || proccpu_test_spin_body(&stop)).unwrap()
        };
        let mut p = Profiler::default();
        let _ = p.period();
        std::thread::sleep(Duration::from_millis(150));
        // This period chooses the spinner (the hottest thread) to be sampled through the next.
        let _ = p.period();
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(10));
            p.tick();
        }
        let report = p.period().expect("a report");
        stop.store(true, Ordering::Relaxed);
        spinner.join().unwrap();
        eprint!("{report}");
        let line = report.lines().find(|l| l.contains("\"proccpu-test-named\"")).expect("the spinner was sampled");
        // A release build without debug information has a PDB of public symbols only, which do not
        // name a private function: then the name is checked only for being the same as a direct
        // lookup of the function's own address gives.
        let direct = HostNames::default().name(proccpu_test_spin_body as fn(&AtomicBool) -> u64 as usize);
        eprintln!("the function's own address names as {direct:?}");
        if direct.contains("proccpu_test_spin_body") {
            assert!(line.contains("proccpu_test_spin_body"), "named from the PDB: {line}");
        } else {
            eprintln!("SKIPPED the name check: this build's PDB has no private symbols (CARGO_PROFILE_RELEASE_DEBUG=line-tables-only)");
        }
    }
}
