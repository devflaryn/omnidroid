//! `__cxa_atexit` / `__cxa_finalize`: the process-exit handler registries that
//! C++ static-object destructors (and `atexit`) ride on.
//!
//! ## Semantics (Itanium C++ ABI, which bionic implements)
//!
//! * `__cxa_atexit(f, arg, dso)`: register handler `f(arg)` with an owning DSO
//!   handle. Handlers run in **reverse registration order** (LIFO) — the C++
//!   rule that a static object's destructor runs before destructors of objects
//!   constructed before it.
//! * `__cxa_finalize(dso)`: run and remove every handler registered against
//!   `dso`, still LIFO among themselves. `dso == 0` runs and removes **all**
//!   handlers (the exit path). Re-registration during finalization: the new
//!   handler is appended and runs within the same finalize sweep (documented;
//!   this is what makes a destructor that registers an atexit handler work).
//! * `atexit(f)` is `__cxa_atexit(f, 0, 0)` in bionic — covered by the same
//!   registry with dso = 0; note that a `dso == 0` *filter* and a handler
//!   registered with dso 0 are distinguished by the finalize call, not the
//!   record: `__cxa_finalize(0)` takes everything.
//!
//! This crate produces the ORDERED list; invoking a guest function pointer is
//! the adapter's job.

use std::collections::HashMap;
use std::sync::Mutex as HostMutex;

/// One registered handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtexitEntry {
    /// Guest function pointer.
    pub func: u64,
    /// Guest argument pointer.
    pub arg: u64,
    /// Owning DSO handle (0 for plain atexit).
    pub dso: u64,
    /// Monotonic registration stamp: the LIFO order authority.
    pub stamp: u64,
}

/// The process-wide registry.
#[derive(Default)]
pub struct AtexitRegistry {
    inner: HostMutex<Inner>,
}

#[derive(Default)]
struct Inner {
    entries: Vec<AtexitEntry>,
    stamp: u64,
    /// Whether a full finalize (dso == 0) already ran: further registrations
    /// still succeed (POSIX: after exit, atexit registration is UB; we accept
    /// and record — the next full finalize runs them) and the flag is only
    /// diagnostic.
    finalized: bool,
}

impl AtexitRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// `__cxa_atexit(func, arg, dso)`. Always succeeds (0) in practice; a host
    /// OOM aborts. The returned stamp is internal (the ABI's return is 0/–1).
    pub fn register(&self, func: u64, arg: u64, dso: u64) -> i32 {
        let mut inner = self.inner.lock().unwrap();
        inner.stamp += 1;
        let stamp = inner.stamp;
        inner.entries.push(AtexitEntry { func, arg, dso, stamp });
        0
    }

    /// Number of registered handlers not yet run (test/diagnostic).
    pub fn pending(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// `__cxa_finalize(dso)`: run and remove every handler whose `dso` matches
    /// (`dso == 0`: ALL), in reverse registration order, returning the ordered
    /// (func, arg) list. `run_recorded` is the adapter's invoke stand-in: it is
    /// called for each pair as it is popped, so a handler that REGISTERS more
    /// work during the sweep joins the same sweep (the loop drains until no
    /// entry matches). This mirrors `tls::take_exit_work`.
    pub fn finalize_with(
        &self,
        dso: u64,
        run_recorded: &mut dyn FnMut(u64, u64),
    ) -> Vec<(u64, u64)> {
        let mut work: Vec<(u64, u64)> = Vec::new();
        loop {
            let next = {
                let mut inner = self.inner.lock().unwrap();
                inner.finalized = true;
                // LIFO: scan from the newest entry backwards for the first
                // entry matching the filter.
                let pos = inner
                    .entries
                    .iter()
                    .rposition(|e| dso == 0 || e.dso == dso);
                match pos {
                    Some(p) => {
                        let e = inner.entries.remove(p);
                        (e.func, e.arg)
                    }
                    None => break,
                }
            };
            run_recorded(next.0, next.1);
            work.push(next);
        }
        work
    }

    /// Pure-list variant: pops ALL matching entries LIFO without invoking any
    /// callback (for callers that run the list afterwards and accept that
    /// registrations made during that run land in the NEXT finalize).
    pub fn finalize(&self, dso: u64) -> Vec<(u64, u64)> {
        let mut work: Vec<(u64, u64)> = Vec::new();
        loop {
            let next = {
                let mut inner = self.inner.lock().unwrap();
                inner.finalized = true;
                let pos = inner
                    .entries
                    .iter()
                    .rposition(|e| dso == 0 || e.dso == dso);
                match pos {
                    Some(p) => {
                        let e = inner.entries.remove(p);
                        (e.func, e.arg)
                    }
                    None => break,
                }
            };
            work.push(next);
        }
        work
    }

    /// Snapshot of the remaining entries in registration order (diagnostic).
    pub fn snapshot(&self) -> Vec<AtexitEntry> {
        self.inner.lock().unwrap().entries.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handlers run in REVERSE registration order.
    #[test]
    fn reverse_registration_order() {
        let reg = AtexitRegistry::new();
        assert_eq!(reg.register(0xF1, 0xA1, 0), 0);
        assert_eq!(reg.register(0xF2, 0xA2, 0), 0);
        assert_eq!(reg.register(0xF3, 0xA3, 0), 0);
        let work = reg.finalize(0);
        assert_eq!(work, vec![(0xF3, 0xA3), (0xF2, 0xA2), (0xF1, 0xA1)]);
        assert_eq!(reg.pending(), 0, "full finalize removes everything");
    }

    /// Finalize filtered by DSO: only that DSO's handlers run, others stay.
    #[test]
    fn dso_filtered_finalize() {
        let reg = AtexitRegistry::new();
        assert_eq!(reg.register(0xF1, 0xA1, 0xD1), 0);
        assert_eq!(reg.register(0xF2, 0xA2, 0xD2), 0);
        assert_eq!(reg.register(0xF3, 0xA3, 0xD1), 0);
        assert_eq!(reg.register(0xF4, 0xA4, 0xD2), 0);
        // Finalize D1: F3 then F1 (reverse among themselves).
        let work = reg.finalize(0xD1);
        assert_eq!(work, vec![(0xF3, 0xA3), (0xF1, 0xA1)]);
        // D2's handlers remain and run LIFO on its finalize.
        assert_eq!(reg.pending(), 2);
        let work = reg.finalize(0xD2);
        assert_eq!(work, vec![(0xF4, 0xA4), (0xF2, 0xA2)]);
        assert_eq!(reg.pending(), 0);
    }

    /// Repeated finalize of the same DSO runs nothing (entries removed).
    #[test]
    fn finalize_is_idempotent_per_dso() {
        let reg = AtexitRegistry::new();
        reg.register(0xF1, 0xA1, 0xD1);
        assert_eq!(reg.finalize(0xD1).len(), 1);
        assert_eq!(reg.finalize(0xD1).len(), 0);
    }

    /// Full finalize (dso == 0) also removes handlers registered against a
    /// specific DSO — the exit path runs everything.
    #[test]
    fn full_finalize_takes_everything() {
        let reg = AtexitRegistry::new();
        reg.register(0xF1, 0xA1, 0xD1);
        reg.register(0xF2, 0xA2, 0);
        let work = reg.finalize(0);
        assert_eq!(work, vec![(0xF2, 0xA2), (0xF1, 0xA1)]);
    }

    /// Registration DURING finalization (via finalize_with) joins the same
    /// sweep; the pure-list `finalize` leaves late registrations to the next
    /// finalize (both policies defined; the adapter picks one).
    #[test]
    fn registration_during_finalize() {
        let reg = std::sync::Arc::new(AtexitRegistry::new());
        reg.register(0xF1, 0xA1, 0);
        let reg2 = reg.clone();
        let ran_late = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_late2 = ran_late.clone();
        let mut run = move |func: u64, _arg: u64| {
            if func == 0xF1 {
                reg2.register(0xF2, 0xA2, 0); // register during the sweep
            } else {
                ran_late2.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        };
        let work = reg.finalize_with(0, &mut run);
        assert_eq!(work, vec![(0xF1, 0xA1), (0xF2, 0xA2)]);
        assert!(ran_late.load(std::sync::atomic::Ordering::SeqCst), "F2 ran in the same sweep");
        assert_eq!(reg.pending(), 0);
    }

    /// LIFO order among a large mixed set (stamps, not arrival order).
    #[test]
    fn lifo_scale() {
        let reg = AtexitRegistry::new();
        for i in 0..100u64 {
            reg.register(0xF000 + i, i, 0xD0);
        }
        let work = reg.finalize(0);
        let expected: Vec<(u64, u64)> = (0..100u64).rev().map(|i| (0xF000 + i, i)).collect();
        assert_eq!(work, expected);
    }
}
