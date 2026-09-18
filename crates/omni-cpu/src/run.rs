//! The run loop's budget policy: why a watchdog is a **short budget expiring**, and never an
//! external halt.
//!
//! # The measurement this is built on
//!
//! Task 2 ran an 18-cell matrix — each cell a separate child process, one escape armed at a time —
//! against guest loops that never terminate. The result that shapes everything here:
//!
//! | Guest shape | step budget alone | cross-thread halt alone | both armed |
//! |---|---|---|---|
//! | direct branch (`B .`, `ADD; B -1`) | stops it | stops it | **neither stops it** |
//! | indirect branch (`BR X30` → self), default flags | no | no | no |
//! | indirect branch, `INTERRUPTIBLE` | stops it | stops it | stops it |
//!
//! The direct-branch row is the one that matters, and the cause is visible in the emitted code.
//! `A64EmitX64::EmitTerminalImpl(IR::Term::LinkBlock)` emits **one** check, chosen by
//! `enable_cycle_counting`: with counting on it compares `cycles_remaining` against zero and jumps
//! to the next block while it is positive, and with counting off it compares `halt_reason` against
//! zero instead. It never emits both. So the cell a real runtime would want — a budget for bounding
//! work *and* a halt for stopping on demand — is not reachable: arming the halt as well does not add
//! a check, it just leaves the block-linked loop running until the budget it is not checking runs
//! out.
//!
//! Two consequences, and they are the whole design:
//!
//! 1. **The watchdog is the budget.** Not `HaltHandle`, which is the escape that does not work on
//!    the shape most likely to occur. So `run` never asks the backend to run unbounded: it runs in
//!    slices of [`SLICE_INSTRUCTIONS`], and between slices — in Rust, on the same thread, where no
//!    generated code is involved — it checks the halt flag, the elapsed count and whatever else a
//!    caller needs. A halt still works; it just works *between* slices rather than inside one.
//! 2. **Nothing may depend on stopping a spinning guest thread from outside.** The containment for a
//!    guest that ignores its slice budget entirely is the process boundary
//!    (`ARCHITECTURE.md` §7), not anything in this crate.
//!
//! # The `u64::MAX` footgun
//!
//! The emitted check is `cmp qword[…cycles_remaining], 0` followed by `jg` — a **signed** compare.
//! A caller passing `u64::MAX` to mean "no limit" gets `-1`, `jg` fails at every block boundary, and
//! the guest returns to the dispatcher after *every single block*: the value that reads as
//! "unlimited" produces the worst throughput available. [`slice_budget`] is the only place a budget
//! is computed, it is clamped to [`MAX_SLICE_INSTRUCTIONS`], and it can never return 0 or a value
//! that reads as negative.

use crate::exit::RunLimit;

/// Guest instructions per slice of an otherwise-unlimited run.
///
/// The number is a trade between two costs that were both measured, and it is stated as a choice
/// rather than a derivation:
///
/// * **Too small** and the cost is the return to the dispatcher. D5 measured the fastest workload at
///   5,207 Mguest-insn/s, so a slice of one million instructions is on the order of 200 µs of guest
///   time, against a dispatcher round trip of well under a microsecond — under a half-percent
///   overhead.
/// * **Too large** and it is the watchdog's resolution: a slice is the longest a guest can ignore a
///   halt request, and 200 µs is far below anything a caller would notice.
///
/// A fitted constant, not a derived one, and deliberately round.
pub const SLICE_INSTRUCTIONS: u64 = 1_000_000;

/// The largest budget that may ever be handed to a backend.
///
/// `i64::MAX`, because the emitted comparison is signed: anything with bit 63 set reads as negative
/// and makes the backend return after every block. This is the guard rail around the footgun in the
/// module docs, and it is a *clamp* rather than a rejection — a caller asking for a budget larger
/// than 9.2 × 10^18 instructions is asking for "effectively unlimited" and should get it, not an
/// error.
pub const MAX_SLICE_INSTRUCTIONS: u64 = i64::MAX as u64;

/// How much of a counted budget is left, and what to hand the backend for the next slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    limit: RunLimit,
    executed: u64,
}

impl Budget {
    /// Start a run under `limit`.
    #[must_use]
    pub const fn new(limit: RunLimit) -> Self {
        Self { limit, executed: 0 }
    }

    /// Guest instructions executed so far in this run.
    #[must_use]
    pub const fn executed(&self) -> u64 {
        self.executed
    }

    /// The limit this run was started under.
    #[must_use]
    pub const fn limit(&self) -> RunLimit {
        self.limit
    }

    /// Record a slice's worth of progress. Saturating: a backend that over-runs its slice — and they
    /// do, because a counted budget is checked at block boundaries and not between instructions —
    /// must not be able to wrap this counter and make a bounded run look unbounded.
    pub fn charge(&mut self, instructions: u64) {
        self.executed = self.executed.saturating_add(instructions);
    }

    /// Whether a counted budget has been reached. Always false for [`RunLimit::Unlimited`], which is
    /// bounded by the caller's halt flag and by the process boundary, not by a count.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        match self.limit {
            RunLimit::Unlimited => false,
            RunLimit::Instructions(limit) => self.executed >= limit,
        }
    }

    /// What to hand the backend for the next slice.
    ///
    /// Never 0 — a zero budget means "return immediately", which would spin — and never above
    /// [`MAX_SLICE_INSTRUCTIONS`], which is where the signed-comparison footgun lives. Returns
    /// `None` when a counted budget is already spent, which is the caller's signal to stop rather
    /// than to run a slice of zero.
    #[must_use]
    pub fn slice(&self) -> Option<u64> {
        match self.limit {
            RunLimit::Unlimited => Some(SLICE_INSTRUCTIONS),
            RunLimit::Instructions(limit) => {
                let remaining = limit.saturating_sub(self.executed);
                if remaining == 0 {
                    None
                } else {
                    // Sliced even when the budget is counted. A caller that asks for 10^12
                    // instructions is asking for a bound on *work*, not for the halt flag to go
                    // unchecked for an hour, and the emitted terminal checks the cycle counter or
                    // the halt flag but never both.
                    Some(slice_budget(remaining.min(SLICE_INSTRUCTIONS)))
                }
            }
        }
    }
}

/// Clamp a wanted budget into the range a backend can be given.
///
/// The only place a budget is computed. `0` becomes 1 rather than "run forever", which is the other
/// half of the same footgun: a zero passed where "unlimited" was meant is as easy a mistake as
/// `u64::MAX`, and it has the opposite and equally wrong effect.
#[must_use]
pub const fn slice_budget(wanted: u64) -> u64 {
    if wanted == 0 {
        1
    } else if wanted > MAX_SLICE_INSTRUCTIONS {
        MAX_SLICE_INSTRUCTIONS
    } else {
        wanted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The footgun, pinned from both ends. `u64::MAX` is what a caller writes for "no limit"; 0 is
    /// what a caller writes when a subtraction went wrong. Neither may reach a backend.
    #[test]
    fn no_budget_ever_reaches_a_backend_negative_or_zero() {
        assert_eq!(slice_budget(u64::MAX), MAX_SLICE_INSTRUCTIONS);
        assert_eq!(slice_budget(0), 1);
        assert_eq!(slice_budget(1), 1);
        assert_eq!(slice_budget(SLICE_INSTRUCTIONS), SLICE_INSTRUCTIONS);
        assert_eq!(slice_budget(MAX_SLICE_INSTRUCTIONS), MAX_SLICE_INSTRUCTIONS);
        assert_eq!(slice_budget(MAX_SLICE_INSTRUCTIONS + 1), MAX_SLICE_INSTRUCTIONS);

        // The property that matters is not the clamp value but that the result never has bit 63
        // set, because the emitted compare is signed and a negative budget returns after every
        // block. Checked over the whole interesting range, including every power of two.
        for shift in 0..64 {
            let wanted = 1u64 << shift;
            for wanted in [wanted, wanted - 1, wanted + 1, wanted.wrapping_neg()] {
                let budget = slice_budget(wanted);
                assert!(budget > 0, "{wanted:#x} produced a zero budget");
                assert!(
                    (budget as i64) > 0,
                    "{wanted:#x} produced {budget:#x}, which reads as {} to the emitted signed \
                     compare",
                    budget as i64
                );
            }
        }
    }

    /// An unlimited run is sliced, not unbounded, because the halt flag can only be checked between
    /// slices — the emitted block-linking terminal checks the cycle counter *or* the halt flag and
    /// never both.
    #[test]
    fn an_unlimited_run_is_sliced_rather_than_unbounded() {
        let mut budget = Budget::new(RunLimit::Unlimited);
        for round in 0..5u64 {
            assert_eq!(budget.slice(), Some(SLICE_INSTRUCTIONS), "round {round}");
            assert!(!budget.is_exhausted(), "an unlimited run is never exhausted");
            budget.charge(SLICE_INSTRUCTIONS);
        }
        assert_eq!(budget.executed(), 5 * SLICE_INSTRUCTIONS);
        assert_ne!(
            budget.slice(),
            Some(u64::MAX),
            "an unlimited run must never hand the backend a budget that reads as negative"
        );
    }

    #[test]
    fn a_counted_run_spends_exactly_its_budget_and_then_stops() {
        let mut budget = Budget::new(RunLimit::Instructions(2_500_000));
        assert_eq!(budget.slice(), Some(1_000_000));
        budget.charge(1_000_000);
        assert_eq!(budget.slice(), Some(1_000_000));
        budget.charge(1_000_000);
        // The last slice is the remainder, not a full slice: a counted run must not overshoot by up
        // to a million instructions because the arithmetic was lazy.
        assert_eq!(budget.slice(), Some(500_000));
        budget.charge(500_000);
        assert!(budget.is_exhausted());
        assert_eq!(budget.slice(), None, "an exhausted budget asks for no more slices");
    }

    /// A backend over-runs its slice, because a counted budget is checked at block boundaries.
    /// `executed` must report what really happened and must not wrap.
    #[test]
    fn an_overshooting_backend_cannot_make_a_bounded_run_look_unbounded() {
        let mut budget = Budget::new(RunLimit::Instructions(1_000));
        assert_eq!(budget.slice(), Some(1_000));
        budget.charge(1_337); // the block ran past the boundary
        assert!(budget.is_exhausted());
        assert_eq!(budget.executed(), 1_337, "the real count is reported, not the budget");
        assert_eq!(budget.slice(), None);

        let mut budget = Budget::new(RunLimit::Instructions(10));
        budget.charge(u64::MAX);
        budget.charge(u64::MAX);
        assert_eq!(budget.executed(), u64::MAX, "saturating, not wrapping");
        assert!(budget.is_exhausted());
    }
}
