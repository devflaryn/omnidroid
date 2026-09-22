//! Metadata functions: thread identity, attribute objects, naming, and yield.
//!
//! These functions carry no synchronization state — they move and validate
//! bytes between the guest struct and the registry, so most are pure memory
//! traffic over [`GuestMemory`]. Scope notes:
//!
//! * `pthread_sigmask` is **excluded**: it requires the guest's real signal
//!   mask, which lives behind the CPU/adapter seam (how a blocked signal
//!   interacts with the emulated CPU's fault delivery is an M4+ decision). A
//!   plausible stub would silently swallow signal-mask errors the engine's
//!   watchdog paths depend on. Recorded in the report §1 as excluded-with-
//!   reason, not forgotten.
//! * `sched_yield` is a registry-level no-op on a real scheduler: the adapter
//!   calls `std::thread::yield_now()` on the host thread. The trait hook
//!   [`Yield::yield_now`] keeps it mockable.
//! * `pthread_self`/`pthread_equal` read the 64-bit [`GuestThreadId`]; the
//!   guest `pthread_t` width is LP64 `unsigned long` (8 bytes) — the host's
//!   `usize`/pointer width must never leak.

use crate::errno::consts;
use crate::layouts::sizes;
use crate::memory::GuestMemory;
use crate::threads::{GuestThreadId, ThreadRegistry};

/// bionic detach-state numbers (attr values).
pub mod detach_state {
    /// `PTHREAD_CREATE_JOINABLE` (default).
    pub const JOINABLE: i32 = 0;
    /// `PTHREAD_CREATE_DETACHED`.
    pub const DETACHED: i32 = 1;
}

// ---------------------------------------------------------------------------
// identity
// ---------------------------------------------------------------------------

/// `pthread_self()`: the calling thread's 64-bit guest identity.
pub fn self_id(threads: &impl ThreadRegistry) -> GuestThreadId {
    threads.current()
}

/// `pthread_equal(a, b)`: nonzero when equal. The guest receives an `int`.
pub fn equal(a: GuestThreadId, b: GuestThreadId) -> i32 {
    if a == b {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// attr objects
// ---------------------------------------------------------------------------

/// `pthread_attr_init`: 56 zero bytes = JOINABLE, default stack (0 = "system
/// default" in the guest-visible attr), zero guard size.
pub fn attr_init(mem: &mut impl GuestMemory, attr_addr: u64) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    mem.write(attr_addr, &[0u8; sizes::PTHREAD_ATTR_T as usize])?;
    Ok(0)
}

/// `pthread_attr_destroy`: validates only (bionic: nothing to free).
pub fn attr_destroy(_mem: &mut impl GuestMemory, attr_addr: u64) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    Ok(0)
}

/// `pthread_attr_setdetachstate`: EINVAL for anything but JOINABLE(0) /
/// DETACHED(1). Returned, not errno (pthread convention).
pub fn attr_setdetachstate(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
    state: i32,
) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    match state {
        detach_state::JOINABLE | detach_state::DETACHED => {
            mem.write(attr_addr, &state.to_le_bytes())?;
            Ok(0)
        }
        _ => Ok(consts::EINVAL),
    }
}

/// `pthread_attr_getdetachstate`: `Ok(Ok(state))`.
pub fn attr_getdetachstate(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<Result<i32, i32>, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    let mut b = [0u8; 4];
    mem.read(attr_addr, &mut b)?;
    Ok(Ok(i32::from_le_bytes(b)))
}

/// `pthread_attr_setstacksize`: EINVAL for 0 (bionic rejects a zero stack);
/// any positive size is stored.
pub fn attr_setstacksize(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
    size: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    if size == 0 {
        return Ok(consts::EINVAL);
    }
    // Field offset 8 in bionic's attr: flags (4+4 pad), stack_size, ...
    mem.write(attr_addr + 8, &size.to_le_bytes())?;
    Ok(0)
}

/// `pthread_attr_getstacksize`: `Ok(Ok(size))`.
pub fn attr_getstacksize(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<Result<u64, i32>, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    let mut b = [0u8; 8];
    mem.read(attr_addr + 8, &mut b)?;
    Ok(Ok(u64::from_le_bytes(b)))
}

/// `pthread_attr_setguardsize`: any size accepted (bionic stores it; 0 means
/// no guard page).
pub fn attr_setguardsize(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
    size: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    // Field offset 16: follows stack_size.
    mem.write(attr_addr + 16, &size.to_le_bytes())?;
    Ok(0)
}

/// `pthread_attr_getguardsize`: `Ok(Ok(size))`.
pub fn attr_getguardsize(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<Result<u64, i32>, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    let mut b = [0u8; 8];
    mem.read(attr_addr + 16, &mut b)?;
    Ok(Ok(u64::from_le_bytes(b)))
}

/// `pthread_attr_getstack`: returns the recorded stack base and size from the
/// attr (`Ok(Ok((base, size)))`). For an attr that never set them, both are 0
/// (the "system default" encoding).
pub fn attr_getstack(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<Result<(u64, u64), i32>, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_ATTR_T)?;
    // stack_base at +0? bionic's layout: flags first, so base lives where
    // this crate put stack_size's neighbour. We define: base at +24, size at
    // +8 — consistent with setstacksize; base is settable only via
    // attr_setstack, which bionic does not implement as writable (attr stores
    // what setstack wrote). We return both recorded fields.
    let mut bsize = [0u8; 8];
    mem.read(attr_addr + 8, &mut bsize)?;
    let mut bbase = [0u8; 8];
    mem.read(attr_addr + 24, &mut bbase)?;
    Ok(Ok((u64::from_le_bytes(bbase), u64::from_le_bytes(bsize))))
}

/// `pthread_getattr_np`'s half: a **live** thread's four attributes, written
/// into the guest's `pthread_attr_t`.
///
/// This is the only writer in this module that fills `stack_base`, and the
/// reason is that nothing else can: the attr setters this crate implements are
/// the ones bionic lets a caller use *before* a thread exists, and a stack base
/// is not among them — `pthread_attr_setstack` is not in the reachable set and
/// is not bound. `attr_getstack` has read the field since phase 3c and nothing
/// has ever written it, so an attr that reached it reported base 0, which is the
/// "system default" encoding rather than an address. A caller asking a running
/// thread where its stack is wants the address.
///
/// **It invents nothing.** Every one of the four values is supplied by the
/// caller, which is the only layer that can have measured them: the adapter
/// mapped the stack, chose the guard and holds the detach state. This function
/// is the encoding and only the encoding — which is why it is here, over
/// `GuestMemory`, with no host or thread state anywhere near it (D19).
///
/// The offsets are the ones `attr_setdetachstate` (0), `attr_setstacksize` (8),
/// `attr_setguardsize` (16) and `attr_getstack` (24) already use. They are
/// restated here rather than shared, because three of the four setters validate
/// a guest's request and this writes a fact — and what holds the two statements
/// together is a test rather than a comment:
/// `a_live_thread_attr_reads_back_through_every_existing_accessor` writes with
/// this function and reads with all four accessors, so a fifth layout invented
/// here fails it (`docs/VERIFICATION.md` entry 1 — the round trip is the
/// membership check a size assertion cannot make).
///
/// Every byte is zeroed first, through [`attr_init`]. bionic fills the whole
/// object, and a caller that reuses one attr for `pthread_attr_init` and then
/// for this would otherwise read its own earlier `setstacksize` back out of a
/// field this call did not reach.
pub fn attr_from_live_thread(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
    detached: bool,
    stack_base: u64,
    stack_size: u64,
    guard_size: u64,
) -> Result<(), crate::memory::Fault> {
    // Range-checks the whole 56 bytes and zeroes them, so the three writes below
    // are inside an area this call has already been allowed to write.
    attr_init(mem, attr_addr)?;
    // A `bool` rather than an `int` because the state has exactly two values and
    // this is the one caller that cannot get one from the guest: the thread is
    // either detached or it is not, and an `i32` parameter would need an EINVAL
    // arm for a number no caller can produce (`docs/VERIFICATION.md` entry 12).
    let state = if detached { detach_state::DETACHED } else { detach_state::JOINABLE };
    mem.write(attr_addr, &state.to_le_bytes())?;
    mem.write(attr_addr + 8, &stack_size.to_le_bytes())?;
    mem.write(attr_addr + 16, &guard_size.to_le_bytes())?;
    mem.write(attr_addr + 24, &stack_base.to_le_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// naming
// ---------------------------------------------------------------------------

/// `pthread_setname_np(thread, name)`: stores up to 15 bytes + NUL of the name
/// in the registry. Bionic's kernel-backed limit is `TASK_COMM_LEN` (16
/// including NUL); a longer name is ENAMETOOLONG... VERIFIED behaviour: bionic
/// returns 0 and truncates? NO — bionic's `pthread_setname_np` validates
/// against the kernel limit and returns ERANGE for names that do not fit.
/// This crate: EINVAL for a null pointer, ERANGE (returned) for a name longer
/// than 15 chars + NUL.
pub fn setname(
    reg: &NameRegistry,
    thread: GuestThreadId,
    name: Option<&str>,
) -> i32 {
    match name {
        None => consts::EINVAL,
        Some(n) if n.len() > 15 => consts::ERANGE,
        Some(n) => {
            reg.set(thread, n);
            0
        }
    }
}

/// `pthread_getname_np(thread, buf, len)`: copies the name (NUL-terminated)
/// into the guest buffer. ERANGE when `len` is too small for name + NUL;
/// EINVAL for a null buffer or zero length.
pub fn getname(
    reg: &NameRegistry,
    thread: GuestThreadId,
    buf: &mut [u8],
) -> Result<i32, i32> {
    if buf.is_empty() {
        return Err(consts::EINVAL);
    }
    match reg.get(thread) {
        Some(name) => {
            if name.len() + 1 > buf.len() {
                return Err(consts::ERANGE);
            }
            buf[..name.len()].copy_from_slice(name.as_bytes());
            buf[name.len()] = 0;
            Ok(0)
        }
        None => {
            // Unnamed: bionic returns an empty string.
            buf[0] = 0;
            Ok(0)
        }
    }
}

/// Host-side per-thread name storage (16 bytes incl. NUL per thread).
#[derive(Default)]
pub struct NameRegistry {
    names: std::sync::Mutex<std::collections::HashMap<GuestThreadId, String>>,
}

impl NameRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self, thread: GuestThreadId, name: &str) {
        self.names
            .lock()
            .unwrap()
            .insert(thread, name.to_string());
    }

    fn get(&self, thread: GuestThreadId) -> Option<String> {
        self.names.lock().unwrap().get(&thread).cloned()
    }
}

// ---------------------------------------------------------------------------
// yield
// ---------------------------------------------------------------------------

/// The scheduler-yield hook the adapter implements with
/// `std::thread::yield_now()` (or a host sched_yield).
pub trait Yield {
    /// Yield the calling host thread.
    fn yield_now(&self);
}

/// `sched_yield()` through the [`Yield`] hook. Always 0.
pub fn sched_yield(y: &impl Yield) -> i32 {
    y.yield_now();
    0
}

// ---------------------------------------------------------------------------
// scheduling policy
// ---------------------------------------------------------------------------

/// The `SCHED_*` policy numbers, as Linux's `sched.h` fixes them for every architecture.
///
/// They are the **guest's** ABI: `libroblox.so` was compiled against these, which is the same
/// provenance the `O_*` flags and the `POLL*` bits carry.
pub mod sched_policy {
    /// `SCHED_OTHER`, the ordinary time-sharing policy.
    pub const OTHER: i32 = 0;
    /// `SCHED_FIFO`, real-time, run-until-you-yield.
    pub const FIFO: i32 = 1;
    /// `SCHED_RR`, real-time, round-robin.
    pub const RR: i32 = 2;
    /// `SCHED_BATCH`.
    pub const BATCH: i32 = 3;
    /// `SCHED_IDLE`.
    pub const IDLE: i32 = 5;
}

/// `int sched_get_priority_max(int policy)`
///
/// Linux's answer, which is a **constant per policy** and not a property of the machine: 99 for
/// the two real-time policies and 0 for every other. That is why this is pure computation in this
/// crate rather than a question for the host — a host that answered from its own scheduler would
/// be describing Windows' priority classes to a guest that reasons in Linux's numbers.
///
/// `-1` for a policy Linux does not define; the caller is expected to set `EINVAL` alongside it.
///
/// # What the guest does with it, MEASURED
///
/// Both call sites in `libroblox.so` pass `SCHED_FIFO`. At guest `0x054e0260`/`0x054e026c` it
/// takes the min and the max, rejects `-1` from either, and then requires
/// **`max - min >= 3`** (`sub w8, w0, w20; cmp w8, #3; b.lt`) — it is sizing a band of real-time
/// priorities. Linux's 1..99 satisfies that; a pair this layer invented would silently decide
/// whether a whole scheduling strategy in the engine turns itself on.
#[must_use]
pub fn sched_get_priority_max(policy: i32) -> i32 {
    match policy {
        sched_policy::FIFO | sched_policy::RR => 99,
        sched_policy::OTHER | sched_policy::BATCH | sched_policy::IDLE => 0,
        _ => -1,
    }
}

/// `int sched_get_priority_min(int policy)`
///
/// The mirror of [`sched_get_priority_max`]: 1 for the real-time policies, 0 for the rest.
#[must_use]
pub fn sched_get_priority_min(policy: i32) -> i32 {
    match policy {
        sched_policy::FIFO | sched_policy::RR => 1,
        sched_policy::OTHER | sched_policy::BATCH | sched_policy::IDLE => 0,
        _ => -1,
    }
}

fn check_range(addr: u64, len: u64) -> Result<(), crate::memory::Fault> {
    if addr == 0 {
        return Err(crate::memory::Fault(0));
    }
    match addr.checked_add(len - 1) {
        Some(_) => Ok(()),
        None => Err(crate::memory::Fault(addr)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;

    /// The scheduling band is Linux's, **named value by value** rather than checked as a span.
    ///
    /// A test that only asserted `max - min >= 3` would pass for 1..4, for 0..99, and for a pair
    /// that had swapped -- and the guest at 0x054e0260 asks exactly that question, so a test
    /// shaped like the guest's check cannot see a substitution in either endpoint
    /// (`docs/VERIFICATION.md` entry 1).
    #[test]
    fn the_scheduling_priority_band_is_linux_value_by_value() {
        assert_eq!(sched_get_priority_min(sched_policy::FIFO), 1);
        assert_eq!(sched_get_priority_max(sched_policy::FIFO), 99);
        assert_eq!(sched_get_priority_min(sched_policy::RR), 1);
        assert_eq!(sched_get_priority_max(sched_policy::RR), 99);
        for policy in [sched_policy::OTHER, sched_policy::BATCH, sched_policy::IDLE] {
            assert_eq!(sched_get_priority_min(policy), 0, "policy {policy}");
            assert_eq!(sched_get_priority_max(policy), 0, "policy {policy}");
        }
        // 4 is the one number below IDLE that Linux does not define, so it is the case that
        // separates "anything in range" from the actual set.
        for policy in [-1, 4, 6, 99] {
            assert_eq!(sched_get_priority_min(policy), -1, "policy {policy}");
            assert_eq!(sched_get_priority_max(policy), -1, "policy {policy}");
        }
    }

    /// And the relation the guest actually tests, asserted as a relation.
    ///
    /// `docs/VERIFICATION.md` entry 10: the values above are the evidence, this is the
    /// *conclusion* the engine draws from them, and stating it separately means a future change
    /// to either endpoint has to break one of the two.
    #[test]
    fn the_real_time_band_is_wide_enough_for_the_engine_check_at_0x054e0280() {
        let min = sched_get_priority_min(sched_policy::FIFO);
        let max = sched_get_priority_max(sched_policy::FIFO);
        assert!(max - min >= 3, "the guest requires `max - min >= 3`: got {min}..{max}");
    }

    /// pthread_equal: equal identities -> 1, distinct -> 0.
    #[test]
    fn equal_semantics() {
        assert_eq!(equal(GuestThreadId(5), GuestThreadId(5)), 1);
        assert_eq!(equal(GuestThreadId(5), GuestThreadId(6)), 0);
    }

    /// attr roundtrips: detachstate, stacksize, guardsize; EINVAL on zero
    /// stack size and on a bogus detach state.
    #[test]
    fn attr_roundtrips() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; 56]);
        assert_eq!(attr_init(&mut mem, 0x1000).unwrap(), 0);
        assert_eq!(attr_getdetachstate(&mut mem, 0x1000).unwrap(), Ok(detach_state::JOINABLE));
        assert_eq!(attr_setdetachstate(&mut mem, 0x1000, detach_state::DETACHED).unwrap(), 0);
        assert_eq!(attr_getdetachstate(&mut mem, 0x1000).unwrap(), Ok(detach_state::DETACHED));
        assert_eq!(attr_setdetachstate(&mut mem, 0x1000, 99).unwrap(), consts::EINVAL);

        assert_eq!(attr_setstacksize(&mut mem, 0x1000, 0).unwrap(), consts::EINVAL);
        assert_eq!(attr_setstacksize(&mut mem, 0x1000, 16 * 1024 * 1024).unwrap(), 0);
        assert_eq!(
            attr_getstacksize(&mut mem, 0x1000).unwrap(),
            Ok(16 * 1024 * 1024)
        );

        assert_eq!(attr_setguardsize(&mut mem, 0x1000, 4096).unwrap(), 0);
        assert_eq!(attr_getguardsize(&mut mem, 0x1000).unwrap(), Ok(4096));
        assert_eq!(attr_getstack(&mut mem, 0x1000).unwrap(), Ok((0, 16 * 1024 * 1024)));

        assert_eq!(attr_destroy(&mut mem, 0x1000).unwrap(), 0);
    }

    /// **What `attr_from_live_thread` writes is what all four existing accessors
    /// read**, field by field and value by value.
    ///
    /// This is the test that stops a second `pthread_attr_t` layout existing.
    /// The adapter's `pthread_getattr_np` fills an attr the guest then reads
    /// with `pthread_attr_getstack` and `pthread_attr_getguardsize`, so a writer
    /// with its own idea of where `guard_size` lives produces an attr that is
    /// *self-consistent and wrong* — the guest reads the stack size out of the
    /// guard field and believes its stack is 4 KiB. Nothing about the write
    /// would fail; only reading it back the way the guest does shows it.
    ///
    /// The four values are deliberately **distinct and non-zero**, so a pair of
    /// swapped offsets cannot pass: entry 1 in `docs/VERIFICATION.md` is a count
    /// that stayed right while two members were substituted, and two fields
    /// holding each other's value is the same failure one struct down.
    #[test]
    fn a_live_thread_attr_reads_back_through_every_existing_accessor() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; 56]);
        // A base, a size and a guard that are all different, and none of them a
        // power of two multiple of another.
        let base = 0x7f_1234_5000u64;
        let size = 1024 * 1024u64;
        let guard = 4096u64;

        attr_from_live_thread(&mut mem, 0x1000, true, base, size, guard).unwrap();
        assert_eq!(attr_getdetachstate(&mut mem, 0x1000).unwrap(), Ok(detach_state::DETACHED));
        assert_eq!(attr_getstacksize(&mut mem, 0x1000).unwrap(), Ok(size));
        assert_eq!(attr_getguardsize(&mut mem, 0x1000).unwrap(), Ok(guard));
        assert_eq!(attr_getstack(&mut mem, 0x1000).unwrap(), Ok((base, size)));

        // Joinable is the other state, and it is 0 — which is also what an
        // untouched field reads as, so it is asserted from a DETACHED attr
        // rather than from a fresh one: that is the only way round that proves
        // the write happened.
        attr_from_live_thread(&mut mem, 0x1000, false, base, size, guard).unwrap();
        assert_eq!(attr_getdetachstate(&mut mem, 0x1000).unwrap(), Ok(detach_state::JOINABLE));
    }

    /// **Every byte of a reused attr is the live thread's**, and none of it is
    /// what the caller put there before.
    ///
    /// The reachable guest pattern is one `pthread_attr_t` on the stack used for
    /// a `pthread_attr_init` + `setstacksize` + `pthread_create` and then for a
    /// `pthread_getattr_np`. Without the zeroing, the fields this call does not
    /// reach keep the earlier request — so the answer to "how big is my stack"
    /// would be "as big as you once asked for", which is the plausible wrong
    /// answer Global Constraint 1 is about.
    #[test]
    fn a_live_thread_attr_leaves_nothing_of_what_the_attr_held_before() {
        let mut mem = MockMemory::new();
        mem.map(0x2000, &[0xAAu8; 56]);
        attr_from_live_thread(&mut mem, 0x2000, false, 0x4000, 8192, 0).unwrap();
        let mut whole = [0u8; 56];
        mem.read(0x2000, &mut whole).unwrap();
        // The four fields, then everything else, which must be zero: bionic's
        // `__private` tail carries the scheduling fields and this layer knows
        // nothing about them, so zero is the only value it may write there.
        assert_eq!(&whole[0..8], &0u64.to_le_bytes(), "detach state JOINABLE and its padding");
        assert_eq!(&whole[8..16], &8192u64.to_le_bytes());
        assert_eq!(&whole[16..24], &0u64.to_le_bytes(), "a guard size of zero is a value");
        assert_eq!(&whole[24..32], &0x4000u64.to_le_bytes());
        assert!(whole[32..].iter().all(|b| *b == 0), "{:?}", &whole[32..]);
    }

    /// Hostile: the same null and wrapping addresses the other attr writers
    /// refuse, through the one that writes four fields rather than one.
    #[test]
    fn a_live_thread_attr_refuses_an_address_its_own_fields_do_not_fit_below() {
        let mut mem = MockMemory::new();
        assert!(attr_from_live_thread(&mut mem, 0, false, 0x1000, 4096, 0).is_err());
        assert!(attr_from_live_thread(&mut mem, u64::MAX - 20, false, 0x1000, 4096, 0).is_err());
        // And an address that is neither null nor wrapping but is not mapped:
        // the fault comes from the memory, not from the range check.
        assert!(attr_from_live_thread(&mut mem, 0xdead_0000, false, 0x1000, 4096, 0).is_err());
    }

    /// Names: set/get roundtrip, truncation limit (ERANGE past 15 chars),
    /// unnamed threads read an empty string.
    #[test]
    fn name_semantics() {
        let reg = NameRegistry::new();
        let t = GuestThreadId(2);
        assert_eq!(setname(&reg, t, Some("GameThread")), 0);
        let mut buf = [0u8; 16];
        assert_eq!(getname(&reg, t, &mut buf), Ok(0));
        assert_eq!(&buf[..11], b"GameThread\0");
        // Unnamed thread: empty string.
        let mut buf2 = [0u8; 16];
        assert_eq!(getname(&reg, GuestThreadId(3), &mut buf2), Ok(0));
        assert_eq!(buf2[0], 0);
        // Too long: ERANGE.
        assert_eq!(setname(&reg, t, Some("0123456789abcdef")), consts::ERANGE);
        // 15 chars + NUL fits exactly.
        assert_eq!(setname(&reg, t, Some("012345678901234")), 0);
        let mut small = [0u8; 4];
        assert_eq!(getname(&reg, t, &mut small), Err(consts::ERANGE));
        // Null name: EINVAL.
        assert_eq!(setname(&reg, t, None), consts::EINVAL);
    }

    /// sched_yield goes through the hook and returns 0.
    #[test]
    fn yield_hook() {
        struct Counted(std::sync::atomic::AtomicUsize);
        impl Yield for Counted {
            fn yield_now(&self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let c = Counted(std::sync::atomic::AtomicUsize::new(0));
        assert_eq!(sched_yield(&c), 0);
        assert_eq!(c.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Hostile: null/wrapping attr addresses fault.
    #[test]
    fn hostile_inputs() {
        let mut mem = MockMemory::new();
        assert!(attr_init(&mut mem, 0).is_err());
        assert!(attr_init(&mut mem, u64::MAX - 20).is_err());
        assert!(attr_setstacksize(&mut mem, 0, 4096).is_err());
        assert!(attr_getstack(&mut mem, 0xdead_0000).is_err());
    }
}
