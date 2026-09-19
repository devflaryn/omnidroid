//! Guest process state the implemented functions need beyond raw memory.
//!
//! A function that only reads or writes memory takes `&impl GuestMemory`. A function that
//! must *set the guest's errno*, keep PRNG state, or place a returned string somewhere the
//! guest can read takes a [`GuestContext`], which *is* a `GuestMemory` (supertrait) plus
//! those state hooks. The later adapter implements this once, over the emulator.

use crate::memory::GuestMemory;

/// Guest process state: memory + errno + rand state + scratch storage.
///
/// Why these hooks (and nothing more):
/// * `errno` — POSIX says the conversion and libm functions set `*errno` on error; the
///   storage belongs to the guest (thread-local on a real device), so the adapter owns it.
/// * rand state — `rand`/`srand` are stateful; libroblox.so imports no allocator, so the
///   state cannot be malloc'd like real bionic does. Storing it behind this trait keeps the
///   sequence policy in one replaceable place.
/// * scratch buffers — `strerror` and `localeconv` return pointers to storage the guest
///   reads later. This crate never allocates and never picks guest addresses: the adapter
///   supplies an address and capacity through [`scratch`], and this crate writes into it.
///
/// [`scratch`]: Self::scratch
pub trait GuestContext: GuestMemory {
    /// Read the guest's current `errno` value.
    fn errno(&self) -> i32;
    /// Set the guest's `errno`.
    fn set_errno(&mut self, value: i32);

    /// Read the 32-bit state of the guest's `rand` engine and the current seed policy.
    ///
    /// The adapter may store any deterministic sequence it likes; the crate treats this as
    /// an opaque state word plus a monotonically usable seed. See [`crate::numerics`] for
    /// how `rand` uses it.
    fn rand_state(&self) -> u32;
    /// Store the 32-bit state of the guest's `rand` engine.
    fn set_rand_state(&mut self, state: u32);

    /// Acquire a scratch buffer for a returned string/struct: `write(addr, bytes)` through
    /// this same context must succeed afterwards, and `capacity` bytes at the returned
    /// address must stay reserved for this purpose by the adapter until the next `scratch`
    /// call.
    ///
    /// Returns `Some((guest_address, capacity))`, or `None` when the backend has no scratch
    /// space; functions needing scratch then fail with a named error instead of guessing a
    /// plausible address.
    fn scratch(&mut self) -> Option<(u64, usize)>;
}

// Note: no blanket `GuestMemory` impl is needed or wanted here. `GuestContext` has
// `GuestMemory` as a *supertrait*, so every context already implements `GuestMemory` and
// can be passed directly to functions taking `&impl GuestMemory`. A blanket impl that
// delegates would recurse into itself.
