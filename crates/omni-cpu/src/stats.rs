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
