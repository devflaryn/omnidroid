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
