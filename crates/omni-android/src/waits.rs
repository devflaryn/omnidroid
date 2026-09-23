//! **The wait trace**: where each guest thread's time inside handlers goes, by call site.
//!
//! A diagnostic, **off unless an embedding turns it on** ([`enable`]), and an embedding that does
//! must say so in its own output (the gate prints `WAIT TRACE: ON`). While it is off the whole
//! cost is one relaxed load per crossing.
//!
//! While it is on, every handler the boundary services is timed, and the time is charged to
//! `(guest thread, symbol, call site, object)`: the call site is the guest's `X30` (one past the
//! `BL`), and the object is the synchronisation object the call names -- `X0` for `pthread_*` and
//! `sem_*`, the futex word (`X1`) for `syscall` -- so a thread's waits can be matched against the
//! calls that wake the same object on another thread. Everything else is charged with object `0`.
//!
//! It answers "what does each frame wait on" the way [`Boundary::threads`](crate::Boundary::threads)
//! cannot: that one samples *which* handler a thread is in; this one says for how long, how often,
//! from where, and on what.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Every thread's table, so a report can read them all.
static TABLES: Mutex<Vec<Arc<Mutex<Table>>>> = Mutex::new(Vec::new());

thread_local! {
    static LOCAL: OnceCell<Arc<Mutex<Table>>> = const { OnceCell::new() };
    /// A guest stack the running handler noted ([`note_stack`]), kept with its entry's longest wait.
    static NOTED: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}

type Table = HashMap<Key, Stat>;

/// What one entry is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    thread: u64,
    slot: u64,
    caller: u64,
    object: u64,
    /// `syscall`'s futex operation (`X2`), so a wait and a wake on one word stay apart.
    op: u64,
}

/// One entry's totals.
#[derive(Debug, Clone, Default)]
struct Stat {
    count: u64,
    total_ns: u64,
    max_ns: u64,
    over_1ms: u64,
    over_10ms: u64,
    /// The guest stack noted with the longest call, if its handler noted one.
    stack: Option<Vec<u64>>,
}

/// Turn the trace on, with every earlier total discarded.
pub fn enable() {
    reset();
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether the trace is on.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Discard every total, keeping the trace on or off as it is.
pub fn reset() {
    for table in TABLES.lock().iter() {
        table.lock().clear();
    }
    TIMED.lock().clear();
    REFUSED.store(0, Ordering::Relaxed);
}

/// A handler being timed. See [`begin`].
pub(crate) struct Timed {
    key: Key,
    started: Instant,
}

/// Start timing one handler, or `None` while the trace is off.
#[inline]
pub(crate) fn begin(symbol: &str, slot: u64, caller: u64, x0: u64, x1: u64, x2: u64) -> Option<Timed> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let (object, op) = if symbol.starts_with("pthread_") || symbol.starts_with("sem_") {
        (x0, 0)
    } else if symbol == "syscall" {
        (x1, x2)
    } else {
        (0, 0)
    };
    let thread = crate::bionic::current_guest_thread().map_or(u64::MAX, |thread| thread.0);
    NOTED.with(|noted| noted.borrow_mut().take());
    Some(Timed { key: Key { thread, slot, caller, object, op }, started: Instant::now() })
}

/// Timed waits that ran to their deadline, by kind: (count, requested ns, overslept ns, max
/// overslept ns, overslept 2 ms or more).
static TIMED: Mutex<Vec<(&'static str, [u64; 5])>> = Mutex::new(Vec::new());

/// Record a timed wait of `kind` that was asked for `requested` and returned, timed out, after
/// `actual`: how far past its deadline the host let it sleep. Nothing while the trace is off.
pub(crate) fn record_timeout(kind: &'static str, requested: std::time::Duration, actual: std::time::Duration) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let requested_ns = u64::try_from(requested.as_nanos()).unwrap_or(u64::MAX);
    let over_ns = u64::try_from(actual.saturating_sub(requested).as_nanos()).unwrap_or(u64::MAX);
    let mut timed = TIMED.lock();
    let at = match timed.iter().position(|(k, _)| *k == kind) {
        Some(at) => at,
        None => {
            timed.push((kind, [0; 5]));
            timed.len() - 1
        }
    };
    let row = &mut timed[at].1;
    row[0] += 1;
    row[1] = row[1].saturating_add(requested_ns);
    row[2] = row[2].saturating_add(over_ns);
    row[3] = row[3].max(over_ns);
    row[4] += u64::from(over_ns >= 2_000_000);
}

/// Waits by a bionic primitive (`Futex::wait`) that the futex refused because the word had
/// already changed: each is a wake that would have been lost without the comparison.
static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one [`REFUSED`] wait. Nothing while the trace is off.
pub(crate) fn count_refused() {
    if ENABLED.load(Ordering::Relaxed) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Keep `stack` with the running handler's entry, if this call turns out to be its longest.
/// Nothing while the trace is off.
pub(crate) fn note_stack(stack: &[u64]) {
    if ENABLED.load(Ordering::Relaxed) {
        NOTED.with(|noted| *noted.borrow_mut() = Some(stack.to_vec()));
    }
}

impl Timed {
    /// Charge the time since [`begin`].
    #[inline]
    pub(crate) fn end(self) {
        let ns = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        LOCAL.with(|cell| {
            let table = cell.get_or_init(|| {
                let table = Arc::new(Mutex::new(Table::new()));
                TABLES.lock().push(Arc::clone(&table));
                table
            });
            let noted = NOTED.with(|noted| noted.borrow_mut().take());
            let mut table = table.lock();
            let stat = table.entry(self.key).or_default();
            stat.count += 1;
            stat.total_ns = stat.total_ns.saturating_add(ns);
            if ns >= stat.max_ns && noted.is_some() {
                stat.stack = noted;
            }
            stat.max_ns = stat.max_ns.max(ns);
            stat.over_1ms += u64::from(ns >= 1_000_000);
            stat.over_10ms += u64::from(ns >= 10_000_000);
        });
    }
}

/// One entry of a [`snapshot`]: `(guest thread, slot, call site, object, op)`.
pub(crate) type SnapshotKey = (u64, u64, u64, u64, u64);

/// Every entry's running totals, merged across the per-thread tables: `(count, total ns, max ns)`.
///
/// For a reader that reports **intervals** (`crate::perf`): it keeps the previous snapshot and
/// prints the difference, so this leaves the totals alone and the end-of-session [`report`] still
/// sees everything.
pub(crate) fn snapshot() -> HashMap<SnapshotKey, (u64, u64, u64)> {
    let mut merged: HashMap<SnapshotKey, (u64, u64, u64)> = HashMap::new();
    for table in TABLES.lock().iter() {
        for (key, stat) in table.lock().iter() {
            let into = merged
                .entry((key.thread, key.slot, key.caller, key.object, key.op))
                .or_insert((0, 0, 0));
            into.0 += stat.count;
            into.1 = into.1.saturating_add(stat.total_ns);
            into.2 = into.2.max(stat.max_ns);
        }
    }
    merged
}

/// The report: per guest thread, its handler time in all and its `per_thread` largest entries,
/// over `seconds` of tracing. `name` turns a slot into its symbol and `base` turns a call site
/// into a link address.
#[must_use]
pub fn report(seconds: f64, base: u64, per_thread: usize, name: &dyn Fn(u64) -> String) -> String {
    let mut merged: HashMap<Key, Stat> = HashMap::new();
    for table in TABLES.lock().iter() {
        for (key, stat) in table.lock().iter() {
            let into = merged.entry(*key).or_default();
            if stat.max_ns >= into.max_ns && stat.stack.is_some() {
                into.stack = stat.stack.clone();
            }
            into.count += stat.count;
            into.total_ns = into.total_ns.saturating_add(stat.total_ns);
            into.max_ns = into.max_ns.max(stat.max_ns);
            into.over_1ms += stat.over_1ms;
            into.over_10ms += stat.over_10ms;
        }
    }
    let wakes: Vec<(Key, Stat)> = merged
        .iter()
        .filter(|(key, _)| {
            let symbol = name(key.slot);
            symbol == "pthread_cond_signal"
                || symbol == "pthread_cond_broadcast"
                || symbol == "sem_post"
                || symbol == "pthread_mutex_unlock"
                || (symbol == "syscall" && matches!(key.op & 0x7f, 1 | 10))
        })
        .map(|(key, stat)| (*key, stat.clone()))
        .collect();
    // **Lock waits of a second or more**, wherever they are in a thread's list: a lock's slow
    // path that sleeps out a bounded slice after a wake it missed looks like exactly this.
    let mut long: Vec<(Key, Stat)> = merged
        .iter()
        .filter(|(key, stat)| {
            let symbol = name(key.slot);
            stat.max_ns >= 900_000_000
                && (symbol.starts_with("pthread_mutex_")
                    || symbol.starts_with("pthread_rwlock_")
                    || symbol.starts_with("sem_")
                    || symbol == "pthread_once")
        })
        .map(|(key, stat)| (*key, stat.clone()))
        .collect();
    long.sort_by(|a, b| b.1.max_ns.cmp(&a.1.max_ns));
    let mut threads: HashMap<u64, Vec<(Key, Stat)>> = HashMap::new();
    for (key, stat) in merged {
        threads.entry(key.thread).or_default().push((key, stat));
    }
    let site_of = |caller: u64| if caller >= base { caller - base } else { caller };
    let mut order: Vec<(u64, u64, u64)> = threads
        .iter()
        .map(|(thread, rows)| {
            (*thread, rows.iter().map(|(_, s)| s.total_ns).sum(), rows.iter().map(|(_, s)| s.count).sum())
        })
        .collect();
    order.sort_by(|a, b| b.1.cmp(&a.1));
    let seconds = seconds.max(0.001);
    let mut out = format!("WAIT TRACE: {seconds:.1}s traced; per guest thread, time inside handlers\n");
    for (kind, row) in TIMED.lock().iter() {
        out.push_str(&format!(
            "  timed out, {kind}: {} waits, mean asked {:.2} ms, mean past the deadline {:.3} ms, \
             max {:.2} ms, 2 ms or more past it {}\n",
            row[0],
            row[1] as f64 / row[0].max(1) as f64 / 1e6,
            row[2] as f64 / row[0].max(1) as f64 / 1e6,
            row[3] as f64 / 1e6,
            row[4]
        ));
    }
    out.push_str(&format!(
        "  bionic primitive waits refused because the word had changed before the park: {}\n",
        REFUSED.load(Ordering::Relaxed)
    ));
    out.push_str(&format!("  lock waits of 0.9 s or more: {} entries\n", long.len()));
    for (key, stat) in &long {
        out.push_str(&format!(
            "    thread {} {} from {:#x} on {:#x}: {} calls, max {:.1} ms, >=10ms {}\n",
            key.thread,
            name(key.slot),
            site_of(key.caller),
            key.object,
            stat.count,
            stat.max_ns as f64 / 1e6,
            stat.over_10ms
        ));
    }
    for (thread, total, calls) in order {
        out.push_str(&format!(
            "  thread {thread}: {:.1}% of the time in handlers, {:.0} calls/s\n",
            100.0 * total as f64 / 1e9 / seconds,
            calls as f64 / seconds
        ));
        let mut rows = threads.remove(&thread).unwrap_or_default();
        rows.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
        for (key, stat) in rows.into_iter().take(per_thread) {
            let share = stat.total_ns as f64 / 1e9 / seconds;
            let op = if key.op != 0 { format!(" op {}", key.op) } else { String::new() };
            out.push_str(&format!(
                "    {:5.1}% {:<28} from {:#9x} on {:#x}{op}: {} calls ({:.1}/s), mean {:.1} us, max {:.1} ms, >=1ms {}, >=10ms {}\n",
                100.0 * share,
                name(key.slot),
                site_of(key.caller),
                key.object,
                stat.count,
                stat.count as f64 / seconds,
                stat.total_ns as f64 / stat.count.max(1) as f64 / 1e3,
                stat.max_ns as f64 / 1e6,
                stat.over_1ms,
                stat.over_10ms
            ));
            // **Who wakes what this waited on**, for a wait worth a tenth of the thread's time.
            if share >= 0.1 && key.object != 0 {
                if let Some(stack) = &stat.stack {
                    let frames: Vec<String> =
                        stack.iter().map(|at| format!("{:#x}", site_of(*at))).collect();
                    out.push_str(&format!("        longest wait's stack: {}\n", frames.join(" < ")));
                }
                for (wake, woke) in wakes
                    .iter()
                    .filter(|(wake, _)| wake.object == key.object && wake.thread != key.thread)
                {
                    out.push_str(&format!(
                        "        woken by thread {} {} from {:#x}: {} calls ({:.1}/s)\n",
                        wake.thread,
                        name(wake.slot),
                        site_of(wake.caller),
                        woke.count,
                        woke.count as f64 / seconds
                    ));
                }
            }
        }
    }
    out
}
