//! What a guest thread is created *from*, and what it costs.

use omni_mem::{GuestAddr, GuestSpace};

use crate::error::{CpuError, CpuResult};

/// A half-open range of guest addresses.
///
/// Validated on construction, so no backend receives a range that wraps. The arithmetic matters:
/// the ranges reaching [`invalidate_code`](crate::GuestCpu::invalidate_code) come from guest
/// `mprotect`, guest `munmap` and the guest's own `IC IVAU` sequences, all of which are untrusted
/// (Global Constraint 11), and `start + len` is where an untrusted length turns into a wrapped range
/// that looks small and covers everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestRange {
    start: GuestAddr,
    len: usize,
}

impl GuestRange {
    /// Build a range.
    ///
    /// # Errors
    ///
    /// [`CpuError::InvalidRange`] for a zero length, or for a length that would carry the end past
    /// the top of the address space.
    pub fn new(start: GuestAddr, len: usize) -> CpuResult<Self> {
        if len == 0 {
            return Err(CpuError::InvalidRange {
                start,
                len,
                reason: "a zero-length range names no instruction",
            });
        }
        if start.checked_add(len).is_none() {
            return Err(CpuError::InvalidRange {
                start,
                len,
                reason: "start + len overflows the address space, so the range wraps",
            });
        }
        Ok(Self { start, len })
    }

    /// First address in the range.
    #[must_use]
    pub const fn start(self) -> GuestAddr {
        self.start
    }

    /// Length in bytes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Whether the range is empty. Always false: a zero-length range is refused.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// One past the last address in the range. Cannot overflow: checked on construction.
    #[must_use]
    pub const fn end(self) -> GuestAddr {
        self.start + self.len
    }

    /// Whether `address` is inside the range.
    #[must_use]
    pub const fn contains(self, address: GuestAddr) -> bool {
        address >= self.start && address < self.end()
    }
}

/// The extent of the guest address space a CPU context runs in.
///
/// # Why this is an extent and not a borrow of [`GuestSpace`]
///
/// Because guest VA == host VA (D4, measured: `fastmem_pointer = 0` with
/// `fastmem_address_space_bits = 64` emits `mov reg, [r13 + vaddr]` with `r13 = 0`, folding the base
/// into the SIB byte at **zero** cost, and measured **30-49x** faster than routing memory through
/// callbacks). There is no translation on the memory path, so a CPU context needs nothing from the
/// address space in order to *resolve* an address — only to know which addresses are guest
/// addresses at all, for diagnostics and for configuration checks.
///
/// That is a design constraint as much as a convenience: a context that held `&GuestSpace` would tie
/// every guest thread's lifetime to the space and would put a lock on the guest's load/store path,
/// which is precisely what identity mapping exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestAddressSpace {
    base: GuestAddr,
    len: usize,
}

impl GuestAddressSpace {
    /// Describe a space by its extent.
    ///
    /// # Errors
    ///
    /// [`CpuError::InvalidAddressSpace`] for a zero length or an extent that wraps.
    pub fn new(base: GuestAddr, len: usize) -> CpuResult<Self> {
        if len == 0 {
            return Err(CpuError::InvalidAddressSpace {
                base,
                len,
                reason: "a zero-length address space holds no guest memory",
            });
        }
        if base.checked_add(len).is_none() {
            return Err(CpuError::InvalidAddressSpace {
                base,
                len,
                reason: "base + len overflows the address space, so the extent wraps",
            });
        }
        Ok(Self { base, len })
    }

    /// Describe the extent of a live [`GuestSpace`].
    ///
    /// # Errors
    ///
    /// [`CpuError::InvalidAddressSpace`], as [`new`](GuestAddressSpace::new).
    pub fn of(space: &GuestSpace) -> CpuResult<Self> {
        Self::new(space.base(), space.len())
    }

    /// Base of the space.
    #[must_use]
    pub const fn base(self) -> GuestAddr {
        self.base
    }

    /// Length of the space in bytes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Whether the space is empty. Always false: a zero-length space is refused.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// One past the highest guest address. Cannot overflow: checked on construction.
    #[must_use]
    pub const fn end(self) -> GuestAddr {
        self.base + self.len
    }

    /// Whether `address` is a guest address in this space.
    #[must_use]
    pub const fn contains(self, address: GuestAddr) -> bool {
        address >= self.base && address < self.end()
    }

    /// How many address bits a backend must be able to reach to address the top of this space.
    ///
    /// Reported rather than enforced. A translating backend needs it to configure its fast memory
    /// path and to *assert* that configuration at startup — D4's second footgun is that the default
    /// `fastmem_address_space_bits` is **36**, so a high guest address silently degrades to the slow
    /// path while still producing correct results, which is a 30-49x loss no functional test can see.
    /// That assertion is Task 3's, and belongs to the backend that has the setting; this is the
    /// number it checks against.
    #[must_use]
    pub const fn address_bits(self) -> u32 {
        let top = self.end() as u64 - 1;
        u64::BITS - top.leading_zeros()
    }
}

/// Everything needed to bring up one guest thread's CPU context.
///
/// Validated on construction, which is the point: see
/// [`tpidr_el0`](GuestThreadConfig::tpidr_el0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestThreadConfig {
    space: GuestAddressSpace,
    tpidr_el0: GuestAddr,
}

impl GuestThreadConfig {
    /// Configure a guest thread.
    ///
    /// `tpidr_el0` must already point at a populated bionic-layout TLS block inside `space`. It is a
    /// required argument and not a setter for the reason D13 records: the thread pointer is read by
    /// the *first* stack-protected guest function, which runs before `JNI_OnLoad` and before the
    /// first static initializer, so there is no window in which a context legitimately exists
    /// without one. Making it an argument means a backend cannot be handed a thread that has not had
    /// one programmed.
    ///
    /// It can still be changed afterwards through
    /// [`set_tpidr_el0`](crate::GuestCpu::set_tpidr_el0) — a guest thread that calls
    /// `__set_tls` re-points it — but it can never be absent.
    ///
    /// # Errors
    ///
    /// [`CpuError::MissingThreadPointer`] if `tpidr_el0` is zero, is not inside `space`, or leaves
    /// no room for the bionic slots up to and including `TLS_SLOT_STACK_GUARD` at `+0x28`.
    pub fn new(space: GuestAddressSpace, tpidr_el0: GuestAddr) -> CpuResult<Self> {
        let refuse = |reason| CpuError::MissingThreadPointer {
            tpidr_el0,
            base: space.base,
            len: space.len,
            reason,
        };
        if tpidr_el0 == 0 {
            return Err(refuse("it is null"));
        }
        if !space.contains(tpidr_el0) {
            return Err(refuse("it is outside the guest address space"));
        }
        // The thread pointer is not merely a pointer into the space: the very next thing guest code
        // does with it is load `[Xt, #0x28]`. A block that ends before that offset is a fault waiting
        // for the first stack-protected call, so it is refused here instead.
        let fits =
            tpidr_el0.checked_add(MIN_TLS_BLOCK_BYTES).is_some_and(|end| end <= space.end());
        if !fits {
            return Err(refuse(
                "there is not enough room above it for bionic's TLS slots up to \
                 TLS_SLOT_STACK_GUARD at +0x28",
            ));
        }
        Ok(Self { space, tpidr_el0 })
    }

    /// The guest address space this thread runs in.
    #[must_use]
    pub const fn space(self) -> GuestAddressSpace {
        self.space
    }

    /// The bionic thread pointer this thread starts with.
    #[must_use]
    pub const fn tpidr_el0(self) -> GuestAddr {
        self.tpidr_el0
    }
}

/// Offset of bionic's `TLS_SLOT_STACK_GUARD` from the thread pointer: slot 5, at 5 x 8 bytes.
///
/// The one offset in this crate that comes from measurement rather than from the architecture:
/// 1,276 of `libroblox.so`'s 1,282 `MRS TPIDR_EL0` instructions load exactly `[Xt, #0x28]` (D13).
pub const TLS_SLOT_STACK_GUARD_OFFSET: usize = 0x28;

/// The smallest TLS block a guest thread can be given: enough for the stack-guard slot itself.
///
/// A floor, not the real size — the full bionic block is larger and is Task 3's to lay out. It is
/// here so that the *obviously* broken case is refused at configuration time rather than at the
/// first stack-protected call.
const MIN_TLS_BLOCK_BYTES: usize = TLS_SLOT_STACK_GUARD_OFFSET + 8;

/// What one CPU context costs, split by whether the OS counter can see it.
///
/// # Why the split
///
/// D15: a pagefile-backed section does **not** appear in `PrivateUsage`, which is what
/// [`omni_mem::process_commit_charge`] returns, yet it is charged against the system commit limit in
/// full the moment it is created. The JIT code arena is exactly such a section (D12), and D5
/// measured **20-35 MiB of code cache per guest thread, unshared**, with Roblox heavily
/// multithreaded — so the fastest-growing consumer in the runtime is the one the process counter
/// shows as a flat line. A single `bytes` figure would have hidden that; two fields cannot.
///
/// Both figures are per *context*. Whatever a backend shares between threads is reported by
/// [`GuestCpuBackend::shared_cost`](crate::GuestCpuBackend::shared_cost) instead, so that summing
/// contexts does not count it once per thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextCost {
    /// Bytes of private commit: register state, guest stacks the backend owns, host heap. Counted
    /// by [`omni_mem::process_commit_charge`].
    pub private_committed: usize,
    /// Bytes of shared commit: pagefile-backed sections, above all the code arena. Charged against
    /// the system commit limit and **invisible** to [`omni_mem::process_commit_charge`].
    pub shared_committed: usize,
}

impl ContextCost {
    /// Everything this context charges against the system commit limit.
    ///
    /// Saturating rather than checked: these are two of our own measurements added together, and a
    /// budget report that panics is worse than one that reports `usize::MAX`.
    #[must_use]
    pub const fn total(self) -> usize {
        self.private_committed.saturating_add(self.shared_committed)
    }

    /// Add two costs, for summing across the guest threads of one instance.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            private_committed: self.private_committed.saturating_add(other.private_committed),
            shared_committed: self.shared_committed.saturating_add(other.shared_committed),
        }
    }
}

impl core::fmt::Display for ContextCost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const MIB: f64 = (1024 * 1024) as f64;
        write!(
            f,
            "{:.3} MiB ({:.3} MiB private + {:.3} MiB shared, which process_commit_charge does not \
             count)",
            self.total() as f64 / MIB,
            self.private_committed as f64 / MIB,
            self.shared_committed as f64 / MIB,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_that_wraps_or_is_empty_is_refused() {
        let range = GuestRange::new(0x1000, 0x40).expect("a range");
        assert_eq!((range.start(), range.len(), range.end()), (0x1000, 0x40, 0x1040));
        assert!(range.contains(0x1000) && range.contains(0x103F));
        assert!(!range.contains(0x0FFF) && !range.contains(0x1040));
        assert!(!range.is_empty());

        assert!(matches!(GuestRange::new(0x1000, 0), Err(CpuError::InvalidRange { len: 0, .. })));
        for (start, len) in [(usize::MAX, 1), (1, usize::MAX), (usize::MAX / 2 + 1, usize::MAX)] {
            assert!(
                matches!(GuestRange::new(start, len), Err(CpuError::InvalidRange { .. })),
                "{start:#x}+{len:#x} wraps and must be refused"
            );
        }
        // A half-open range ending exactly at `usize::MAX` is fine; one byte more wraps. The very
        // last byte of the address space is therefore not addressable by a `GuestRange`, which is
        // the right trade: no real mapping ends there, and the alternative is an `end()` that has to
        // be saturating and is then wrong by one everywhere it is used.
        assert!(GuestRange::new(usize::MAX - 16, 16).is_ok());
        assert_eq!(GuestRange::new(usize::MAX - 16, 16).expect("a range").end(), usize::MAX);
        assert!(GuestRange::new(usize::MAX - 16, 17).is_err());
    }

    #[test]
    fn an_address_space_that_wraps_or_is_empty_is_refused() {
        let space = GuestAddressSpace::new(0x1_0000, 0x10_0000).expect("a space");
        assert_eq!(space.end(), 0x11_0000);
        assert_eq!(space.address_bits(), 21, "0x10FFFF needs 21 bits");
        assert!(matches!(
            GuestAddressSpace::new(0, 0),
            Err(CpuError::InvalidAddressSpace { len: 0, .. })
        ));
        assert!(GuestAddressSpace::new(1, usize::MAX).is_err());

        // D4 ran at host VA 0x7F00_0000_0000, bit 46. A space reaching it must report 47 bits, which
        // is the number a translating backend's fastmem configuration is asserted against — the
        // default of 36 would silently take the 30-49x-slower path.
        let high = GuestAddressSpace::new(0x7F00_0000_0000, 0x1_0000_0000).expect("a high space");
        assert_eq!(high.address_bits(), 47);
    }

    /// D13 in one test: a thread cannot be configured without a usable thread pointer.
    #[test]
    fn a_thread_cannot_be_configured_without_a_usable_thread_pointer() {
        let space = GuestAddressSpace::new(0x1_0000, 0x10_0000).expect("a space");

        let good = GuestThreadConfig::new(space, 0x2_0000).expect("a thread");
        assert_eq!(good.tpidr_el0(), 0x2_0000);
        assert_eq!(good.space(), space);

        // The expected `reason` is asserted, not just the variant. A null thread pointer is not the
        // same failure as one outside the space even though both are outside it: D13's whole point
        // is that the symptom of a missing thread pointer looks like a loader bug, so the message
        // that distinguishes them is the deliverable. Checking only the variant made the null test
        // pass with the null check deleted.
        for (tpidr, why, reason) in [
            (0usize, "null", "it is null"),
            (0x0FFF, "below the space", "outside the guest address space"),
            (0x11_0000, "at the end of the space", "outside the guest address space"),
            (0x20_0000, "above the space", "outside the guest address space"),
            (usize::MAX, "nowhere near the space", "outside the guest address space"),
            // Inside the space, but with fewer than 0x30 bytes above it: the first stack-protected
            // guest function loads [TPIDR_EL0, #0x28] and would fault.
            (0x10_FFF8, "too close to the top", "not enough room above it"),
            (0x11_0000 - MIN_TLS_BLOCK_BYTES + 1, "one byte short", "not enough room above it"),
        ] {
            match GuestThreadConfig::new(space, tpidr) {
                Err(error @ CpuError::MissingThreadPointer { tpidr_el0, base, len, .. }) => {
                    assert_eq!(tpidr_el0, tpidr, "{why}");
                    assert_eq!((base, len), (space.base(), space.len()));
                    assert!(
                        error.to_string().contains(reason),
                        "TPIDR_EL0 {tpidr:#x} ({why}) was refused for the wrong reason: {error}"
                    );
                }
                other => panic!("TPIDR_EL0 {tpidr:#x} ({why}) must be refused, got {other:?}"),
            }
        }
        // And the exact boundary is accepted, so the check is a bound rather than a margin.
        GuestThreadConfig::new(space, 0x11_0000 - MIN_TLS_BLOCK_BYTES)
            .expect("a block that exactly fits the stack-guard slot");
        assert_eq!(TLS_SLOT_STACK_GUARD_OFFSET, 0x28, "bionic TLS_SLOT_STACK_GUARD is slot 5");
    }

    #[test]
    fn a_context_cost_keeps_the_invisible_half_separate() {
        let a = ContextCost { private_committed: 1024, shared_committed: 20 * 1024 * 1024 };
        assert_eq!(a.total(), 20 * 1024 * 1024 + 1024);
        let both = a.saturating_add(a);
        assert_eq!(both.private_committed, 2048);
        assert_eq!(both.shared_committed, 40 * 1024 * 1024);
        // Summing many threads must saturate rather than wrap: 32 threads of 35 MiB is D5's figure,
        // and a runtime that reported a small number there would be reporting the opposite of the
        // truth.
        let huge = ContextCost { private_committed: usize::MAX, shared_committed: usize::MAX };
        assert_eq!(huge.saturating_add(huge).total(), usize::MAX);
        assert!(a.to_string().contains("20.000 MiB shared"));
    }
}
