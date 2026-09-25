//! **Process-wide facts about the translating backend, for diagnostics that hold no context.**
//!
//! A sampler running on its own thread has to recognise the host code a backend emits -- the
//! exclusive monitor's spin lock and reservation scan in particular, whose addresses the emitter
//! bakes into generated code as 64-bit immediates -- and it has none of the contexts involved.
//! Backends register what such a reader needs here, once, when they are created.
//!
//! Not behind the `dynarmic` feature, on purpose: the reader (`omni-android`'s `perf`) depends on
//! this crate without that feature, and a backend that does not translate simply registers nothing.
//! Nothing here is on a hot path.

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

/// Where one exclusive monitor keeps its state, in host addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MonitorLayout {
    /// The spin lock every exclusive access takes under the global monitor.
    pub lock: usize,
    /// Processor 0's reservation-address slot.
    pub addresses: usize,
    /// Bytes between two processors' address slots.
    pub address_stride: usize,
    /// Processor 0's reserved-value slot.
    pub values: usize,
    /// Bytes between two processors' value slots.
    pub value_stride: usize,
    /// Slots the monitor was sized for.
    pub slots: usize,
    /// Whether exclusive accesses take the global lock and scan every slot
    /// ([`ExclusiveMonitor::Global`](crate::dynarmic::ExclusiveMonitor)), rather than compare
    /// values.
    pub global: bool,
}

/// Which part of a monitor a host address belongs to. See [`monitor_part`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorPart {
    /// The spin lock word.
    Lock,
    /// A reservation-address slot, as the exclusive-store scan and the exclusive load touch it.
    Address,
    /// A reserved-value slot.
    Value,
}

static MONITORS: Mutex<Vec<MonitorLayout>> = Mutex::new(Vec::new());

/// Record a monitor. Called by a backend when it creates one.
pub fn register_monitor(layout: MonitorLayout) {
    MONITORS.lock().push(layout);
}

/// Forget a monitor. Called by a backend when it frees one, so a stale layout cannot classify
/// whatever the allocator puts there next.
pub fn unregister_monitor(lock: usize) {
    MONITORS.lock().retain(|m| m.lock != lock);
}

/// Every monitor currently registered.
#[must_use]
pub fn monitors() -> Vec<MonitorLayout> {
    MONITORS.lock().clone()
}

/// Which part of which registered monitor `address` names, if any.
///
/// Takes the registry as a slice so a caller classifying many addresses reads it once.
#[must_use]
pub fn monitor_part(monitors: &[MonitorLayout], address: usize) -> Option<MonitorPart> {
    monitors.iter().find_map(|m| {
        if address == m.lock {
            return Some(MonitorPart::Lock);
        }
        let in_slots = |base: usize, stride: usize| {
            address >= base
                && address < base.saturating_add(stride.saturating_mul(m.slots.max(1)))
        };
        if in_slots(m.addresses, m.address_stride.max(8)) {
            Some(MonitorPart::Address)
        } else if in_slots(m.values, m.value_stride.max(16)) {
            Some(MonitorPart::Value)
        } else {
            None
        }
    })
}

/// With a shared code cache (D38) the code does not carry the running thread's monitor slots as
/// immediates: it loads them from its `JitState`, at these two offsets from `r15`. Registered by a
/// backend with a shared cache; `None` otherwise.
static JIT_STATE_MONITOR_OFFSETS: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Record where `JitState` keeps the monitor slot pointers (reservation address, reserved value).
pub fn register_jit_state_monitor_offsets(address: u32, value: u32) {
    *JIT_STATE_MONITOR_OFFSETS.lock() = Some((address, value));
}

/// See [`register_jit_state_monitor_offsets`].
#[must_use]
pub fn jit_state_monitor_offsets() -> Option<(u32, u32)> {
    *JIT_STATE_MONITOR_OFFSETS.lock()
}

/// What a shared code cache (D38) has done, summed over the caches alive in the process: what an
/// `OMNI_PERF` line prints, so a cache that keeps translating the same code shows why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodeCacheCounters {
    /// Shared caches alive.
    pub caches: u64,
    /// Blocks translated into them.
    pub blocks_emitted: u64,
    /// Host code bytes those blocks took.
    pub code_bytes_emitted: u64,
    /// Invalidation requests applied.
    pub invalidations: u64,
    /// Blocks those requests dropped.
    pub blocks_invalidated: u64,
    /// Regions retired: each forgets every block, which the threads then translate again.
    pub regions_retired: u64,
    /// Retired regions given back.
    pub regions_reclaimed: u64,
    /// Retired regions still held by a thread running code it entered before the retirement.
    pub regions_pinned: u64,
    /// Parked threads moved out of a retiring region.
    pub parked_redirected: u64,
    /// Dispatcher lookups a thread's own table could not answer.
    pub locked_lookups: u64,
    /// Bytes committed now.
    pub committed_bytes: u64,
}

type CodeCacheReader = Box<dyn Fn() -> Option<CodeCacheCounters> + Send + Sync>;
static CODE_CACHES: Mutex<Vec<CodeCacheReader>> = Mutex::new(Vec::new());

/// Record a shared code cache's counters; `read` answers `None` once the cache is gone, and is
/// dropped then.
pub fn register_code_cache(read: CodeCacheReader) {
    CODE_CACHES.lock().push(read);
}

/// Every live shared code cache's counters, summed (`caches` 0 when there is none).
#[must_use]
pub fn code_caches() -> CodeCacheCounters {
    let mut all = CODE_CACHES.lock();
    let mut sum = CodeCacheCounters::default();
    all.retain(|read| match read() {
        Some(c) => {
            sum.caches += 1;
            sum.blocks_emitted += c.blocks_emitted;
            sum.code_bytes_emitted += c.code_bytes_emitted;
            sum.invalidations += c.invalidations;
            sum.blocks_invalidated += c.blocks_invalidated;
            sum.regions_retired += c.regions_retired;
            sum.regions_reclaimed += c.regions_reclaimed;
            sum.regions_pinned += c.regions_pinned;
            sum.parked_redirected += c.parked_redirected;
            sum.locked_lookups += c.locked_lookups;
            sum.committed_bytes += c.committed_bytes;
            true
        }
        None => false,
    });
    sum
}

static TRACK_RETRANSLATION: AtomicBool = AtomicBool::new(false);

/// Start counting, per context, translations of block starts that context had translated before
/// ([`JitCounters::retranslated`](crate::JitCounters::retranslated)).
///
/// **Off by default and costly when on**: every context then keeps the set of block starts it has
/// translated, a few megabytes for a thread that runs a lot of code. It exists to tell a code cache
/// being thrown away and refilled apart from new code being reached, which a fetch count alone
/// cannot do.
pub fn track_retranslation(on: bool) {
    TRACK_RETRANSLATION.store(on, Ordering::Relaxed);
}

/// Whether [`track_retranslation`] is on.
#[must_use]
pub fn tracking_retranslation() -> bool {
    TRACK_RETRANSLATION.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_classified_against_the_slot_it_falls_in_and_nothing_else() {
        let m = MonitorLayout {
            lock: 0x1000,
            addresses: 0x2000,
            address_stride: 64,
            values: 0x8000,
            value_stride: 128,
            slots: 4,
            global: false,
        };
        let all = [m];
        assert_eq!(monitor_part(&all, 0x1000), Some(MonitorPart::Lock));
        assert_eq!(monitor_part(&all, 0x2000), Some(MonitorPart::Address));
        assert_eq!(monitor_part(&all, 0x2000 + 3 * 64), Some(MonitorPart::Address));
        assert_eq!(monitor_part(&all, 0x2000 + 4 * 64), None, "one past the last slot");
        assert_eq!(monitor_part(&all, 0x8000 + 3 * 128), Some(MonitorPart::Value));
        assert_eq!(monitor_part(&all, 0x8000 + 4 * 128), None);
        assert_eq!(monitor_part(&all, 0x1001), None);
        assert_eq!(monitor_part(&[], 0x1000), None);
    }
}
