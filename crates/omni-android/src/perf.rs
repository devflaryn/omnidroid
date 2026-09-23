//! **`OMNI_PERF`: where the time goes, every few seconds, while it is going there.**
//!
//! The instruments that existed before this one (`OMNI_PROFILE`, `OMNI_WAIT_TRACE`, the code-fetch
//! counters) print once, at the end of a session, for the whole session -- menus, a world's load and
//! the world itself mixed into one number. A game world is a sequence of very different phases, so
//! this reports **intervals**, beside the gate's `FRAMES` lines, and it starts itself: the first
//! [`Boundary`] built in a process whose environment asks for it starts a reporter thread. No
//! embedding has to call anything.
//!
//! # The switches, and what each costs
//!
//! | switch | what it adds | cost when on |
//! |---|---|---|
//! | `OMNI_PERF=<seconds>` | one `PERF` block per interval: presents, cores, guest instructions run and translated, crossings, the pager, the process's memory, and a line per busy guest thread | a relaxed store or two per run segment (a million-instruction window or an exit-path crossing) on each guest thread |
//! | `OMNI_PERF_SAMPLE=<hz>` (default 100; `0` off) | per thread, what share of wall time its instruction pointer was in translated code, the exclusive monitor, dynarmic's own code (translation, block lookup), a handler (by symbol), the kernel or the graphics driver | one `QueryThreadCycleTime` per thread per tick, and for a thread that ran since the last tick a suspend / read / resume (tens of microseconds). The block reports the sampler's own CPU |
//! | `OMNI_PERF_DUMP=<file>` | every sampled address inside this executable, per interval and thread, for offline symbolization (dynarmic's translation versus its block lookup, and which handler functions) | a file write per interval |
//! | `OMNI_PERF_WAITS=1` | turns on [`crate::waits`] and prints, per busy thread, where its handler time went and who woke it | two clock reads and a table update per crossing -- **a real perturbation**; do not take a frame-rate baseline with it on |
//!
//! Every block says which of them are on (`docs/VERIFICATION.md` entry 15: a diagnostic that can be
//! off must say so in its own output).
//!
//! # What the sampled shares mean, and what they cannot see
//!
//! A tick samples each registered host thread whose cycle count moved since the previous tick; a
//! thread that did not run is counted `idle` without being touched. Shares are of **ticks**, so
//! they are shares of wall time. The classes:
//!
//! * `jit` -- the instruction pointer is in a code cache (private memory that is writable and
//!   executable at once, which on this runtime is only dynarmic's): translated guest code, **and**
//!   dynarmic's emitted dispatcher prologue, which lives in the same cache;
//! * `mon` -- in a code cache, within `CODE_BEFORE` bytes of a `mov r64, imm64` whose immediate is
//!   one of the exclusive monitor's own addresses ([`omni_cpu::stats::monitor_part`]): the spin
//!   lock, the reservation scan, the reserved-value compare-and-swap. A heuristic that can only
//!   err towards `jit` for code a few bytes past an exclusive access;
//! * `dyn` -- in this executable while the boundary says the thread is **not** in a handler:
//!   dynarmic translating, looking up a block, or in one of `omni-cpu`'s callbacks. The dump splits
//!   these offline;
//! * `hnd` -- the boundary says the thread is inside a handler (needs the census, which the gate
//!   keeps on; the block says when it is off), split by where the handler was: in this executable,
//!   the kernel, or the driver;
//! * `os`, `drv`, `oth` -- the kernel (`ntdll`, `kernelbase`, `kernel32`), the graphics driver and
//!   loader (`nv*`, `vulkan-1`), anything else, while not in a handler.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use omni_cpu::{GuestCpu, GuestAddr};
use omni_platform::sampler::{self, MemoryKind, Module, CODE_AFTER, CODE_BEFORE};
use parking_lot::Mutex;

use crate::bionic::Bionic;
use crate::boundary::{Boundary, ThreadCrossing, ThreadPerf};
use crate::vulkan::Vulkan;

/// What the environment asked for. See the module documentation.
#[derive(Debug, Clone)]
struct Config {
    interval: Duration,
    sample_hz: u32,
    dump: Option<PathBuf>,
    waits: bool,
}

impl Config {
    fn from_env() -> Option<Self> {
        let interval = std::env::var("OMNI_PERF").ok()?;
        let seconds: f64 = interval
            .trim()
            .parse()
            .ok()
            .filter(|s: &f64| s.is_finite() && *s >= 0.5)
            .unwrap_or_else(|| panic!("OMNI_PERF={interval:?} is not a number of seconds >= 0.5"));
        let sample_hz = match std::env::var("OMNI_PERF_SAMPLE") {
            Ok(text) => text
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("OMNI_PERF_SAMPLE={text:?} is not a whole number of Hz")),
            Err(_) => 100,
        };
        Some(Self {
            interval: Duration::from_secs_f64(seconds),
            sample_hz,
            dump: std::env::var_os("OMNI_PERF_DUMP").map(PathBuf::from),
            waits: std::env::var_os("OMNI_PERF_WAITS").is_some(),
        })
    }
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
static ON: AtomicBool = AtomicBool::new(false);

fn config() -> Option<&'static Config> {
    CONFIG
        .get_or_init(|| {
            let config = Config::from_env();
            // Only ever switched on here: `keep_thread_records` may have switched it on already.
            if config.is_some() {
                ON.store(true, Ordering::Relaxed);
            }
            config
        })
        .as_ref()
}

/// Whether `OMNI_PERF` is on. One relaxed load: this is on the run loop's path.
#[inline]
#[must_use]
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

#[derive(Default)]
struct Registry {
    boundary: Option<Weak<Boundary>>,
    bionic: Option<Weak<Bionic>>,
    vulkan: Vec<Weak<Vulkan>>,
    started: bool,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    boundary: None,
    bionic: None,
    vulkan: Vec::new(),
    started: false,
});

/// Called by [`crate::boundary::BoundaryBuilder::finish`]: the first boundary of a process that
/// asked for `OMNI_PERF` starts the reporter.
pub(crate) fn register_boundary(boundary: &Arc<Boundary>) {
    let Some(config) = config() else { return };
    let mut registry = REGISTRY.lock();
    registry.boundary = Some(Arc::downgrade(boundary));
    if !registry.started {
        registry.started = true;
        let config = config.clone();
        if config.waits {
            crate::waits::enable();
        }
        let spawned = std::thread::Builder::new()
            .name("omnidroid-perf".to_string())
            .spawn(move || reporter(&config));
        if let Err(error) = spawned {
            say(&format!("PERF: the reporter thread could not start: {error}"));
        }
    }
}

/// Called by [`Bionic::new`]: live guest threads come from here.
pub(crate) fn register_bionic(bionic: &Arc<Bionic>) {
    if config().is_some() {
        REGISTRY.lock().bionic = Some(Arc::downgrade(bionic));
    }
}

/// Called by [`Vulkan::new`]: presents and Vulkan call counts come from here.
pub(crate) fn register_vulkan(vulkan: &Arc<Vulkan>) {
    if config().is_some() {
        REGISTRY.lock().vulkan.push(Arc::downgrade(vulkan));
    }
}

/// Keep per-thread records and their sampling handles from now on, as `OMNI_PERF` does, **without**
/// starting the reporter -- for a caller that samples with [`profile`] itself. Irreversible for the
/// process: records already opened keep their handles.
#[doc(hidden)]
pub fn keep_thread_records() {
    ON.store(true, Ordering::Relaxed);
}

/// What [`profile`] saw of one host thread: ticks, and how many landed in each class.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct ThreadProfile {
    /// The guest thread it was running as, `0` for a host-initiated one.
    pub guest_thread: u64,
    /// The OS thread id.
    pub os_id: u32,
    /// Ticks in all, idle ones included.
    pub ticks: u64,
    /// Ticks it had not run since the previous one.
    pub idle: u64,
    /// In a code cache, away from the exclusive monitor.
    pub jit: u64,
    /// In a code cache, at the exclusive monitor.
    pub monitor: u64,
    /// In this executable, outside a handler.
    pub dynarmic: u64,
    /// Inside a handler, wherever the instruction pointer was.
    pub handler: u64,
    /// Everything else.
    pub other: u64,
}

/// Sample every thread `boundary` has a record for, at `hz`, for `duration`, and return what the
/// sampler saw -- the same classification the reporter prints, as numbers. For showing the sampler
/// seeing a known case (`docs/VERIFICATION.md` entry 19); needs [`keep_thread_records`] (or
/// `OMNI_PERF`) before the threads start.
#[doc(hidden)]
#[must_use]
pub fn profile(boundary: &Boundary, duration: Duration, hz: u32) -> Vec<ThreadProfile> {
    let modules = Modules::load();
    let monitors = omni_cpu::stats::monitors();
    let classes = sampler::efficiency_classes().unwrap_or_default();
    let fastest = classes.iter().copied().max().unwrap_or(0);
    let tick = Duration::from_secs_f64(1.0 / f64::from(hz.max(1)));
    let mut acc: HashMap<usize, ThreadAcc> = HashMap::new();
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    let until = Instant::now() + duration;
    let mut records = Vec::new();
    while Instant::now() < until {
        records = boundary.perf_records();
        let census = boundary.census_on();
        for record in &records {
            sample_one(record, census, &modules, &monitors, &classes, fastest, &mut code, &mut acc);
        }
        std::thread::sleep(tick);
    }
    records
        .iter()
        .filter_map(|record| {
            let entry = acc.get(&(Arc::as_ptr(record) as usize))?;
            let c = &entry.classes;
            Some(ThreadProfile {
                guest_thread: record.state().guest_thread,
                os_id: record.perf.host.get().map_or(0, sampler::HostThread::os_id),
                ticks: entry.ticks,
                idle: entry.idle,
                jit: c[Class::Jit.index()],
                monitor: c[Class::Monitor.index()],
                dynarmic: c[Class::Dynarmic.index()],
                handler: c[Class::HandlerHere.index()]
                    + c[Class::HandlerOs.index()]
                    + c[Class::HandlerDriver.index()]
                    + c[Class::HandlerOther.index()],
                other: c[Class::Os.index()] + c[Class::Driver.index()] + c[Class::Other.index()],
            })
        })
        .collect()
}

/// Publish one run segment's worth of this thread's counters. Called by the boundary's run loop
/// after every `cpu.run`, only while [`enabled`].
pub(crate) fn publish_segment(perf: &ThreadPerf, cpu: &dyn GuestCpu) {
    perf.instructions.fetch_add(cpu.last_run_instructions(), Ordering::Relaxed);
    let jit = cpu.jit_counters();
    for (slot, value) in
        perf.jit.iter().zip([jit.fetched, jit.blocks, jit.retranslated, jit.icache_ops, jit.invalidations])
    {
        slot.store(value, Ordering::Relaxed);
    }
    if let Ok(number) = omni_platform::process::current_cpu() {
        perf.cpu.store(number, Ordering::Relaxed);
    }
}

/// The engine's own thread numbers, as its log lines carry them, against the guest thread that
/// wrote each: `2026-09-23T17:08:22.392Z,183.392609,0003,6,...` is engine thread `0003`. The engine
/// names its threads only this way, and the `[SlowBenchmark]` lines that measure its Lua work come
/// from one of them, so this is how a `PERF` line's `g45` becomes "the Lua thread".
static ENGINE_THREADS: Mutex<BTreeMap<u16, u64>> = Mutex::new(BTreeMap::new());

/// Note which guest thread logged `message`, if it carries an engine thread number. Called by the
/// log path only while [`enabled`].
pub(crate) fn note_log_line(message: &str) {
    let Some(thread) = crate::bionic::current_guest_thread() else { return };
    let mut fields = message.splitn(4, ',');
    let (Some(stamp), Some(_seconds), Some(index)) = (fields.next(), fields.next(), fields.next())
    else {
        return;
    };
    if !stamp.ends_with('Z') || index.len() != 4 {
        return;
    }
    let Ok(index) = u16::from_str_radix(index, 16) else { return };
    let mut all = ENGINE_THREADS.lock();
    if all.get(&index) != Some(&thread.0) {
        all.insert(index, thread.0);
    }
}

fn say(text: &str) {
    let _ = writeln!(std::io::stderr(), "{text}");
}

// --------------------------------------------------------------------------------- classification

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Jit,
    Monitor,
    Dynarmic,
    HandlerHere,
    HandlerOs,
    HandlerDriver,
    HandlerOther,
    Os,
    Driver,
    Other,
}

const CLASSES: usize = 10;

impl Class {
    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleKind {
    Exe,
    Os,
    Driver,
    Other,
}

struct Modules {
    all: Vec<(Module, ModuleKind)>,
    exe_base: usize,
}

impl Modules {
    fn load() -> Self {
        let here = reporter as usize;
        let mut all: Vec<(Module, ModuleKind)> = sampler::modules()
            .unwrap_or_default()
            .into_iter()
            .map(|m| {
                let name = m.name.to_ascii_lowercase();
                let kind = if here >= m.base && here < m.base + m.size {
                    ModuleKind::Exe
                } else if name == "ntdll.dll" || name == "kernelbase.dll" || name == "kernel32.dll" {
                    ModuleKind::Os
                } else if name.starts_with("nv") || name.starts_with("vulkan") {
                    ModuleKind::Driver
                } else {
                    ModuleKind::Other
                };
                (m, kind)
            })
            .collect();
        all.sort_by_key(|(m, _)| m.base);
        let exe_base = all.iter().find(|(_, k)| *k == ModuleKind::Exe).map_or(0, |(m, _)| m.base);
        Self { all, exe_base }
    }

    fn of(&self, address: usize) -> Option<ModuleKind> {
        let at = self.all.partition_point(|(m, _)| m.base <= address);
        let (m, kind) = self.all.get(at.checked_sub(1)?)?;
        (address < m.base + m.size).then_some(*kind)
    }
}

/// Whether the code bytes around a sampled instruction pointer contain a 64-bit immediate that is
/// one of the exclusive monitor's own addresses. See the module documentation for `mon`.
fn near_monitor(code: &[u8], start: usize, end: usize, monitors: &[omni_cpu::stats::MonitorLayout]) -> bool {
    if monitors.is_empty() || end < start + 10 {
        return false;
    }
    (start..=end - 10).any(|at| {
        let rex = code[at];
        let op = code[at + 1];
        if (rex == 0x48 || rex == 0x49) && (0xB8..=0xBF).contains(&op) {
            let mut imm = [0u8; 8];
            imm.copy_from_slice(&code[at + 2..at + 10]);
            omni_cpu::stats::monitor_part(monitors, u64::from_le_bytes(imm) as usize).is_some()
        } else {
            false
        }
    })
}

// ----------------------------------------------------------------------------------- per thread

#[derive(Default)]
struct ThreadAcc {
    last_cycles: u64,
    ticks: u64,
    idle: u64,
    failed: u64,
    classes: [u64; CLASSES],
    efficient: u64,
    handler_slots: HashMap<GuestAddr, u64>,
    exe_rips: HashMap<(bool, usize), u64>,
    // Previous interval's totals.
    prev_cpu: Duration,
    prev_instructions: u64,
    prev_jit: [u64; 5],
    prev_crossings: u64,
}

struct Process {
    at: Instant,
    cpu: Duration,
    vk: BTreeMap<String, u64>,
    pager: (u64, u64),
    counters: sampler::ProcessCounters,
    sampler_cpu: Duration,
}

fn vk_counts(vulkans: &[Arc<Vulkan>]) -> BTreeMap<String, u64> {
    let mut all = BTreeMap::new();
    for v in vulkans {
        for (name, n) in v.call_counts() {
            *all.entry(name).or_insert(0) += n;
        }
    }
    all
}

fn snapshot_process(vulkans: &[Arc<Vulkan>], me: Option<&sampler::HostThread>) -> Process {
    Process {
        at: Instant::now(),
        cpu: omni_platform::process::cpu_time().unwrap_or_default(),
        vk: vk_counts(vulkans),
        pager: omni_mem::process_pager_totals(),
        counters: sampler::process_counters().unwrap_or_default(),
        sampler_cpu: me.and_then(|t| t.cpu_time().ok()).unwrap_or_default(),
    }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

/// The reporter thread's body.
fn reporter(config: &Config) {
    let me = sampler::HostThread::current().ok();
    let mut modules = Modules::load();
    let classes = sampler::efficiency_classes().unwrap_or_default();
    let fastest = classes.iter().copied().max().unwrap_or(0);
    let tick = if config.sample_hz > 0 {
        Duration::from_secs_f64(1.0 / f64::from(config.sample_hz))
    } else {
        config.interval
    };
    let mut dump = config.dump.as_ref().and_then(|path| match std::fs::File::create(path) {
        Ok(file) => Some(std::io::BufWriter::new(file)),
        Err(error) => {
            say(&format!("PERF: OMNI_PERF_DUMP={} could not be created: {error}", path.display()));
            None
        }
    });
    if let Some(out) = dump.as_mut() {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let _ = writeln!(out, "exe {exe} base {:#x}", modules.exe_base);
        for (m, _) in &modules.all {
            let _ = writeln!(out, "module {} {:#x} {:#x}", m.name, m.base, m.size);
        }
    }
    let monitors = omni_cpu::stats::monitors();
    say(&format!(
        "PERF: ON (OMNI_PERF): a block every {:.1}s; sampler {} (OMNI_PERF_SAMPLE); dump {} \
         (OMNI_PERF_DUMP); handler timing {} (OMNI_PERF_WAITS); processor efficiency classes {:?}; \
         exclusive monitors {}",
        config.interval.as_secs_f64(),
        if config.sample_hz > 0 { format!("{} Hz", config.sample_hz) } else { "OFF".into() },
        config.dump.as_ref().map_or("OFF".to_string(), |p| p.display().to_string()),
        if config.waits { "ON -- a perturbation" } else { "OFF" },
        classes,
        if monitors.is_empty() {
            "none registered yet".to_string()
        } else {
            monitors
                .iter()
                .map(|m| format!("{} ({} slots)", if m.global { "global" } else { "value-compare" }, m.slots))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));

    let started = Instant::now();
    let mut next_report = started + config.interval;
    let mut acc: HashMap<usize, ThreadAcc> = HashMap::new();
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    let mut prev: Option<Process> = None;
    let mut prev_waits = if config.waits { crate::waits::snapshot() } else { HashMap::new() };
    let mut monitors = monitors;
    loop {
        std::thread::sleep(tick);
        let (boundary, bionic, vulkans) = {
            let registry = REGISTRY.lock();
            (
                registry.boundary.as_ref().and_then(Weak::upgrade),
                registry.bionic.as_ref().and_then(Weak::upgrade),
                registry.vulkan.iter().filter_map(Weak::upgrade).collect::<Vec<_>>(),
            )
        };
        let Some(boundary) = boundary else {
            // The instance is gone; so is anything to report.
            return;
        };
        let records = boundary.perf_records();
        let census = boundary.census_on();
        if config.sample_hz > 0 {
            for record in &records {
                sample_one(record, census, &modules, &monitors, &classes, fastest, &mut code, &mut acc);
            }
        }
        if Instant::now() < next_report {
            continue;
        }
        next_report += config.interval;
        let now = snapshot_process(&vulkans, me.as_ref());
        if let Some(before) = prev.as_ref() {
            report(
                config,
                started,
                before,
                &now,
                &boundary,
                bionic.as_deref(),
                &vulkans,
                &records,
                census,
                &mut acc,
                &mut prev_waits,
                dump.as_mut(),
            );
        } else {
            // The first interval only sets the baselines.
            for record in &records {
                let entry = acc.entry(Arc::as_ptr(record) as usize).or_default();
                reset_baseline(entry, record);
            }
        }
        prev = Some(now);
        // A driver or a guest library can be loaded at any time; the monitor list likewise grows
        // when a backend is created after the reporter started.
        modules = Modules::load();
        monitors = omni_cpu::stats::monitors();
    }
}

fn reset_baseline(entry: &mut ThreadAcc, record: &ThreadCrossing) {
    entry.prev_cpu = record.perf.host.get().and_then(|h| h.cpu_time().ok()).unwrap_or_default();
    entry.prev_instructions = record.perf.instructions.load(Ordering::Relaxed);
    for (slot, value) in entry.prev_jit.iter_mut().zip(&record.perf.jit) {
        *slot = value.load(Ordering::Relaxed);
    }
    entry.prev_crossings = record.state().crossings;
    entry.ticks = 0;
    entry.idle = 0;
    entry.failed = 0;
    entry.classes = [0; CLASSES];
    entry.efficient = 0;
    entry.handler_slots.clear();
    entry.exe_rips.clear();
}

#[allow(clippy::too_many_arguments)]
fn sample_one(
    record: &Arc<ThreadCrossing>,
    census: bool,
    modules: &Modules,
    monitors: &[omni_cpu::stats::MonitorLayout],
    classes: &[u8],
    fastest: u8,
    code: &mut [u8; CODE_BEFORE + CODE_AFTER],
    acc: &mut HashMap<usize, ThreadAcc>,
) {
    let Some(host) = record.perf.host.get() else { return };
    let entry = acc.entry(Arc::as_ptr(record) as usize).or_default();
    entry.ticks += 1;
    let Ok(cycles) = host.cycles() else {
        entry.failed += 1;
        return;
    };
    if cycles == entry.last_cycles {
        entry.idle += 1;
        return;
    }
    entry.last_cycles = cycles;
    // Read before the suspend: the record is the thread's own atomics and the sampler must take
    // nothing the target could hold while it is stopped.
    let state = record.state();
    let Ok(sample) = host.sample(code) else {
        entry.failed += 1;
        return;
    };
    let cpu = record.perf.cpu.load(Ordering::Relaxed) as usize;
    if classes.get(cpu).is_some_and(|&c| c < fastest) {
        entry.efficient += 1;
    }
    let in_handler = census && state.crossings != state.exits;
    let module = modules.of(sample.ip);
    let class = match (in_handler, module) {
        (true, Some(ModuleKind::Exe)) => Class::HandlerHere,
        (true, Some(ModuleKind::Os)) => Class::HandlerOs,
        (true, Some(ModuleKind::Driver)) => Class::HandlerDriver,
        (true, _) => Class::HandlerOther,
        (false, Some(ModuleKind::Exe)) => Class::Dynarmic,
        (false, Some(ModuleKind::Os)) => Class::Os,
        (false, Some(ModuleKind::Driver)) => Class::Driver,
        (false, Some(ModuleKind::Other)) => Class::Other,
        (false, None) => match sampler::memory_kind(sample.ip) {
            Ok(MemoryKind::PrivateWritableExecutable { .. }) => {
                if near_monitor(code, sample.code_start, sample.code_end, monitors) {
                    Class::Monitor
                } else {
                    Class::Jit
                }
            }
            _ => Class::Other,
        },
    };
    entry.classes[class.index()] += 1;
    if in_handler {
        *entry.handler_slots.entry(state.slot).or_insert(0) += 1;
    }
    if module == Some(ModuleKind::Exe) && modules.exe_base != 0 {
        *entry.exe_rips.entry((in_handler, sample.ip - modules.exe_base)).or_insert(0) += 1;
    }
}

fn symbol_of(boundary: &Boundary, vk: &BTreeMap<GuestAddr, String>, slot: GuestAddr) -> String {
    vk.get(&slot)
        .cloned()
        .or_else(|| boundary.symbol_at(slot).map(str::to_string))
        .unwrap_or_else(|| format!("{slot:#x}"))
}

#[allow(clippy::too_many_arguments)]
fn report(
    config: &Config,
    started: Instant,
    before: &Process,
    now: &Process,
    boundary: &Boundary,
    bionic: Option<&Bionic>,
    vulkans: &[Arc<Vulkan>],
    records: &[Arc<ThreadCrossing>],
    census: bool,
    acc: &mut HashMap<usize, ThreadAcc>,
    prev_waits: &mut HashMap<crate::waits::SnapshotKey, (u64, u64, u64)>,
    mut dump: Option<&mut std::io::BufWriter<std::fs::File>>,
) {
    let dt = now.at.duration_since(before.at).as_secs_f64().max(1e-3);
    let t = now.at.duration_since(started).as_secs_f64();
    // Saturating: the counts are summed over the Vulkan instances still registered, so one that
    // was dropped since the previous interval makes a total go *down*.
    let vk_delta = |name: &str| {
        now.vk.get(name).copied().unwrap_or(0).saturating_sub(before.vk.get(name).copied().unwrap_or(0))
    };
    let vk_names: BTreeMap<GuestAddr, String> =
        vulkans.iter().flat_map(|v| v.slot_names()).collect();

    struct Row {
        label: String,
        guest: u64,
        cpu: f64,
        instructions: u64,
        jit: [u64; 5],
        crossings: u64,
        ticks: u64,
        idle: u64,
        classes: [u64; CLASSES],
        efficient: u64,
        handlers: Vec<(GuestAddr, u64)>,
    }
    let mut rows = Vec::new();
    let (mut all_instructions, mut all_fetched, mut all_crossings) = (0u64, 0u64, 0u64);
    for record in records {
        let entry = acc.entry(Arc::as_ptr(record) as usize).or_default();
        let state = record.state();
        let cpu_now = record.perf.host.get().and_then(|h| h.cpu_time().ok()).unwrap_or_default();
        let instructions = record.perf.instructions.load(Ordering::Relaxed);
        let mut jit = [0u64; 5];
        for (slot, value) in jit.iter_mut().zip(&record.perf.jit) {
            *slot = value.load(Ordering::Relaxed);
        }
        let jit_delta: [u64; 5] = std::array::from_fn(|i| jit[i].saturating_sub(entry.prev_jit[i]));
        let row = Row {
            label: match (state.guest_thread, record.perf.host.get()) {
                (0, Some(h)) => format!("host{}", h.os_id()),
                (0, None) => "host?".to_string(),
                (g, _) => format!("g{g}"),
            },
            guest: state.guest_thread,
            cpu: cpu_now.saturating_sub(entry.prev_cpu).as_secs_f64() / dt,
            instructions: instructions.saturating_sub(entry.prev_instructions),
            jit: jit_delta,
            crossings: state.crossings.saturating_sub(entry.prev_crossings),
            ticks: entry.ticks,
            idle: entry.idle,
            classes: entry.classes,
            efficient: entry.efficient,
            handlers: {
                let mut h: Vec<(GuestAddr, u64)> =
                    entry.handler_slots.iter().map(|(s, n)| (*s, *n)).collect();
                h.sort_by(|a, b| b.1.cmp(&a.1));
                h
            },
        };
        all_instructions += row.instructions;
        all_fetched += row.jit[0];
        all_crossings += row.crossings;
        if let Some(out) = dump.as_mut() {
            for ((handler, rva), n) in &entry.exe_rips {
                let _ = writeln!(
                    out,
                    "R {t:.1} {} {} {rva:#x} {n}",
                    row.label,
                    if *handler { 'h' } else { 'd' }
                );
            }
        }
        rows.push(row);
        reset_baseline(entry, record);
    }
    rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));

    let (faults, committed) = (now.pager.0 - before.pager.0, now.pager.1 - before.pager.1);
    let mut out = String::new();
    out.push_str(&format!(
        "PERF +{t:.0}s ({dt:.1}s): presents +{} | cores {:.2} | guest {:.0} Minsn/s, translated {:.1} \
         kinsn/s, crossings {:.2} M/s{} | live guest threads {} | pager +{faults} faults +{} MiB | \
         page faults {:.0}/s, private {:.2} GiB, working set {:.2} GiB | sampler cpu {:.1}%\n",
        vk_delta("vkQueuePresentKHR"),
        now.cpu.saturating_sub(before.cpu).as_secs_f64() / dt,
        all_instructions as f64 / dt / 1e6,
        all_fetched as f64 / dt / 1e3,
        all_crossings as f64 / dt / 1e6,
        if census { "" } else { " (census OFF: no crossings, no handler split)" },
        bionic.map_or_else(|| "?".to_string(), |b| b.live_guest_threads().to_string()),
        committed >> 20,
        now.counters.page_faults.wrapping_sub(before.counters.page_faults) as f64 / dt,
        gib(now.counters.private_bytes),
        gib(now.counters.working_set),
        100.0 * now.sampler_cpu.saturating_sub(before.sampler_cpu).as_secs_f64() / dt,
    ));
    for row in rows.iter().filter(|r| r.cpu >= 0.03).take(14) {
        let pct = |n: u64| 100.0 * n as f64 / row.ticks.max(1) as f64;
        let c = &row.classes;
        let hnd = c[Class::HandlerHere.index()]
            + c[Class::HandlerOs.index()]
            + c[Class::HandlerDriver.index()]
            + c[Class::HandlerOther.index()];
        let sampled = row.ticks.saturating_sub(row.idle).max(1);
        let top: Vec<String> = row
            .handlers
            .iter()
            .take(3)
            .map(|(slot, n)| format!("{} {:.0}%", symbol_of(boundary, &vk_names, *slot), pct(*n)))
            .collect();
        let retranslated =
            if row.jit[2] > 0 { format!(" ({} blocks again)", row.jit[2]) } else { String::new() };
        out.push_str(&format!(
            "PERF   {:<8} cpu {:3.0}% | {:7.1} Minsn/s | xlate {:6.1}k insn/s in {} blocks{retranslated} | \
             {:.2}M cross/s | jit {:.0}% mon {:.0}% dyn {:.0}% hnd {:.0}% (here {:.0} os {:.0} drv {:.0}) \
             os {:.0}% drv {:.0}% oth {:.0}% idle {:.0}% | E-core {:.0}% | {}{}\n",
            row.label,
            100.0 * row.cpu,
            row.instructions as f64 / dt / 1e6,
            row.jit[0] as f64 / dt / 1e3,
            row.jit[1],
            row.crossings as f64 / dt / 1e6,
            pct(c[Class::Jit.index()]),
            pct(c[Class::Monitor.index()]),
            pct(c[Class::Dynarmic.index()]),
            pct(hnd),
            pct(c[Class::HandlerHere.index()]),
            pct(c[Class::HandlerOs.index()]),
            pct(c[Class::HandlerDriver.index()]),
            pct(c[Class::Os.index()]),
            pct(c[Class::Driver.index()]),
            pct(c[Class::Other.index()]),
            pct(row.idle),
            100.0 * row.efficient as f64 / sampled as f64,
            if top.is_empty() { String::new() } else { format!("[{}]", top.join(", ")) },
            if row.jit[3] + row.jit[4] > 0 {
                format!(" icache ops {} invalidations {}", row.jit[3], row.jit[4])
            } else {
                String::new()
            },
        ));
    }
    {
        let engine = ENGINE_THREADS.lock();
        if !engine.is_empty() {
            out.push_str(&format!(
                "PERF   engine threads: {}\n",
                engine.iter().map(|(i, g)| format!("{i:04x}=g{g}")).collect::<Vec<_>>().join(" ")
            ));
        }
    }
    let mut vk: Vec<(String, u64)> = now
        .vk
        .iter()
        .map(|(name, n)| (name.clone(), n.saturating_sub(before.vk.get(name).copied().unwrap_or(0))))
        .filter(|(_, n)| *n > 0)
        .collect();
    vk.sort_by(|a, b| b.1.cmp(&a.1));
    if !vk.is_empty() {
        let total: u64 = vk.iter().map(|(_, n)| n).sum();
        out.push_str(&format!(
            "PERF   vulkan {total} calls: {}\n",
            vk.iter().take(10).map(|(name, n)| format!("{name} {n}")).collect::<Vec<_>>().join(", ")
        ));
    }
    if config.waits {
        let snapshot = crate::waits::snapshot();
        let delta: Vec<(crate::waits::SnapshotKey, (u64, u64, u64))> = snapshot
            .iter()
            .filter_map(|(key, (count, ns, max))| {
                let (c0, n0, _) = prev_waits.get(key).copied().unwrap_or((0, 0, 0));
                (*count > c0).then_some((*key, (count - c0, ns.saturating_sub(n0), *max)))
            })
            .collect();
        for row in rows.iter().filter(|r| r.cpu >= 0.03 || r.guest != 0).take(10) {
            let mut mine: Vec<&(crate::waits::SnapshotKey, (u64, u64, u64))> =
                delta.iter().filter(|(key, _)| key.0 == row.guest).collect();
            if mine.is_empty() {
                continue;
            }
            mine.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
            let total: u64 = mine.iter().map(|(_, v)| v.1).sum();
            let mut line = format!(
                "PERF   waits {:<8} {:.0}% in handlers:",
                row.label,
                100.0 * total as f64 / 1e9 / dt
            );
            for (key, (count, ns, max)) in mine.iter().take(4) {
                let share = *ns as f64 / 1e9 / dt;
                line.push_str(&format!(
                    " {:.0}% {} x{count} (mean {:.0} us, max {:.1} ms)",
                    100.0 * share,
                    symbol_of(boundary, &vk_names, key.1 as GuestAddr),
                    *ns as f64 / (*count).max(1) as f64 / 1e3,
                    *max as f64 / 1e6
                ));
                if share >= 0.1 && key.3 != 0 {
                    let wakers: Vec<String> = delta
                        .iter()
                        .filter(|(k, _)| k.3 == key.3 && k.0 != key.0)
                        .map(|(k, v)| {
                            format!("g{} {} x{}", k.0, symbol_of(boundary, &vk_names, k.1 as GuestAddr), v.0)
                        })
                        .take(4)
                        .collect();
                    if !wakers.is_empty() {
                        line.push_str(&format!(" <- [{}]", wakers.join(", ")));
                    }
                }
                line.push(';');
            }
            out.push_str(&line);
            out.push('\n');
        }
        *prev_waits = snapshot;
    }
    let _ = std::io::stderr().write_all(out.as_bytes());
    if let Some(out) = dump.as_mut() {
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monitor_address_in_a_mov_immediate_is_recognised_and_nothing_else_is() {
        let monitors = [omni_cpu::stats::MonitorLayout {
            lock: 0x1234_5678_9000,
            addresses: 0x1234_5678_a000,
            address_stride: 8,
            values: 0x1234_5678_b000,
            value_stride: 16,
            slots: 4,
            global: true,
        }];
        let mut code = [0x90u8; CODE_BEFORE + CODE_AFTER];
        // `mov rcx, 0x123456789000` (48 b9 imm64), 20 bytes before ip.
        let at = CODE_BEFORE - 20;
        code[at] = 0x48;
        code[at + 1] = 0xB9;
        code[at + 2..at + 10].copy_from_slice(&0x1234_5678_9000u64.to_le_bytes());
        assert!(near_monitor(&code, 0, code.len(), &monitors));
        // The same shape with an immediate that is not the monitor's.
        code[at + 2..at + 10].copy_from_slice(&0x1234_5678_9008u64.to_le_bytes());
        assert!(!near_monitor(&code, 0, code.len(), &monitors));
        // A reservation slot, with r8-r15's REX prefix.
        code[at] = 0x49;
        code[at + 2..at + 10].copy_from_slice(&(0x1234_5678_a000u64 + 3 * 8).to_le_bytes());
        assert!(near_monitor(&code, 0, code.len(), &monitors));
        // Outside the bytes that were actually read, it does not count.
        assert!(!near_monitor(&code, at + 1, code.len(), &monitors));
        assert!(!near_monitor(&code, 0, code.len(), &[]));
    }
}
