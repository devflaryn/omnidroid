//! `sem_*`: POSIX unnamed semaphores over a 4-byte guest `sem_t`.
//!
//! ## Convention difference (the classic trap)
//!
//! The **`sem_*` family sets `errno` and returns -1**, unlike the `pthread_*`
//! family which returns the error code directly. Every function here follows the
//! sem convention: `Ok(-1)` with `errno` set on failure, `Ok(0)` on success
//! (`sem_getvalue` returns the value instead).
//!
//! ## Guest representation (bionic LP64)
//!
//! `sem_t` is ONE 32-bit word (`+0`), the counter, with bionic's futex convention
//!: the sign bit (`0x8000_0000`) is the "waiters present" flag, used by
//! `sem_post` to decide whether a futex wake is needed. `sem_init` with
//! `pshared != 0`: bionic supports process-shared semaphores only for shared
//! mappings; this crate accepts the value (it does not affect single-process
//! semantics) — VERIFIED against bionic's `sem_init` documented behaviour.
//!
//! * value 0 = "no tokens"; wait blocks until a post.
//! * value N = N tokens available.
//! * the waiter flag rides the high bit and is never exposed to `sem_getvalue`
//!   (bionic masks it the same way).

use crate::atomics::GuestAtomic;
use crate::errno::consts;
use crate::layouts::sizes;
use crate::memory::GuestMemory;
use crate::threads::{Futex, WaitResult};
use core::time::Duration;

/// How long a blocked waiter sleeps before re-checking the semaphore word itself.
///
/// This is a safety net for the one race a real Linux futex closes and a
/// non-atomic mock cannot: a waiter that has published the WAITERS flag but has
/// not yet registered with the futex. With `post` preserving the flag, the normal
/// path is a direct wake and this timer never fires.
const SELF_HEAL_SLICE: Duration = Duration::from_millis(50);

/// The waiter-present flag in the high bit.
mod sem_bits {
    /// Waiter flag (bionic: bit 31 of the sem word).
    pub const WAITERS: u32 = 0x8000_0000;
    /// Value mask (the low 31 bits are the count).
    pub const VALUE_MASK: u32 = 0x7FFF_FFFF;
}

// ---------------------------------------------------------------------------
// init / destroy
// ---------------------------------------------------------------------------

/// `sem_init(sem, pshared, value)`. Returns -1 with EINVAL on a value above
/// `SEM_VALUE_MAX` (2^31-1, bionic's documented limit: the value field is 31
/// bits wide under the waiter-flag scheme).
pub fn init(
    mem: &mut (impl GuestMemory + GuestAtomic),
    sem_addr: u64,
    pshared: i32,
    value: u32,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    let _ = pshared; // accepted; single-process semantics unaffected
    if value > sem_bits::VALUE_MASK {
        return errno_result(mem, consts::EINVAL);
    }
    mem.write(sem_addr, &value.to_le_bytes())?;
    Ok(0)
}

/// `sem_destroy`. Bionic returns -1/EINVAL for an invalid sem; destroying one
/// with waiters is UB per POSIX — this crate refuses EBUSY (documented, safer).
pub fn destroy(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    sem_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    let word = read_word(mem, sem_addr)?;
    if word & sem_bits::WAITERS != 0 {
        // The flag is conservative -- it means "a waiter MAY be blocked" -- so it
        // cannot by itself justify EBUSY. Probe: a broadcast wake reports how many
        // were actually queued. Waking threads we are about to refuse is harmless;
        // they re-check their predicate and block again.
        if futex.wake(sem_addr, u32::MAX) > 0 {
            return errno_result(mem, consts::EBUSY);
        }
        // Nobody was queued: the flag was stale, so drop it and destroy.
    }
    mem.write(sem_addr, &0u32.to_le_bytes())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// wait / trywait / timedwait / post / getvalue
// ---------------------------------------------------------------------------

/// `sem_wait`: decrement or block. Returns -1/EINVAL for an unmapped/invalid
/// sem, -1/EINTR if interrupted (never in this mock), 0 on success.
pub fn wait(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    sem_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    loop {
        let word = read_word(mem, sem_addr)?;
        let value = word & sem_bits::VALUE_MASK;
        if value > 0 {
            // `value > 0` means no borrow into bit 31, so `word - 1` decrements the
            // count and PRESERVES the waiter flag. This thread cannot know whether
            // other threads are still blocked, so it must not clear it.
            if mem.cas_u32(sem_addr, word, word - 1)? {
                return Ok(0);
            }
            continue; // raced another waiter: re-read
        }
        // Zero: register interest (waiter flag), then sleep. The flag makes a
        // concurrent post perform the futex wake (and it also re-checks).
        if word & sem_bits::WAITERS == 0 {
            let _ = mem.cas_u32(sem_addr, word, word | sem_bits::WAITERS);
        }
        // Bounded slices so a lost wake self-heals (spurious re-sleeps legal). This
        // is a SAFETY NET, not the wake path: with the waiter flag preserved by
        // `post`, a blocked waiter is woken directly. The slice is short because a
        // real futex checks the value atomically with the block and this mock cannot,
        // so the residual registration race must cost milliseconds, not a second.
        match futex.wait(sem_addr, 0, Some(SELF_HEAL_SLICE)) {
            WaitResult::Woken => continue,
            WaitResult::TimedOut => continue,
            WaitResult::WouldBlock => continue,
        }
    }
}

/// `sem_trywait`: decrement or -1/EAGAIN when the value is zero.
pub fn trywait(
    mem: &mut (impl GuestMemory + GuestAtomic),
    sem_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    loop {
        let word = read_word(mem, sem_addr)?;
        let value = word & sem_bits::VALUE_MASK;
        if value == 0 {
            return errno_result(mem, consts::EAGAIN);
        }
        // Preserve the waiter flag: see `wait`. A successful trywait says nothing
        // about whether other threads are blocked.
        if mem.cas_u32(sem_addr, word, word - 1)? {
            return Ok(0);
        }
        // raced: re-read
    }
}

/// `sem_timedwait`: decrement or block up to `timeout`; -1/ETIMEDOUT on expiry.
pub fn timedwait(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    sem_addr: u64,
    timeout: Duration,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let word = read_word(mem, sem_addr)?;
        let value = word & sem_bits::VALUE_MASK;
        if value > 0 {
            if mem.cas_u32(sem_addr, word, (word - 1) & !sem_bits::WAITERS)? {
                return Ok(0);
            }
            continue;
        }
        if word & sem_bits::WAITERS == 0 {
            let _ = mem.cas_u32(sem_addr, word, word | sem_bits::WAITERS);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining == Duration::ZERO {
            // Clear our waiter flag before reporting the timeout: a later post
            // must not wake a phantom.
            let w = read_word(mem, sem_addr)?;
            let _ = mem.cas_u32(sem_addr, w, w & !sem_bits::WAITERS);
            return errno_result(mem, consts::ETIMEDOUT);
        }
        match futex.wait(sem_addr, 0, Some(remaining)) {
            WaitResult::Woken => continue,
            WaitResult::TimedOut => {
                let w = read_word(mem, sem_addr)?;
                let _ = mem.cas_u32(sem_addr, w, w & !sem_bits::WAITERS);
                return errno_result(mem, consts::ETIMEDOUT);
            }
            WaitResult::WouldBlock => continue,
        }
    }
}

/// `sem_post`: increment and wake one waiter if any are registered. Returns
/// -1/EINVAL if the increment would overflow `SEM_VALUE_MAX` (POSIX: EINVAL).
pub fn post(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    sem_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    loop {
        let word = read_word(mem, sem_addr)?;
        let value = word & sem_bits::VALUE_MASK;
        if value == sem_bits::VALUE_MASK {
            return errno_result(mem, consts::EINVAL); // overflow past SEM_VALUE_MAX
        }
        let waiters = word & sem_bits::WAITERS != 0;
        // The flag is PRESERVED here. `value < VALUE_MASK` was checked above, so
        // `word + 1` cannot carry into bit 31: the count increments and the flag
        // survives. Clearing it here is what caused a posted token to take a full
        // second to reach a second blocked waiter -- the wake was skipped because
        // the flag had already been consumed by the first post.
        let next = word + 1;
        if mem.cas_u32(sem_addr, word, next)? {
            if waiters {
                // A wake that reports zero woken proves the queue was empty at that
                // instant, and only then is it safe to drop the flag. Anything else
                // leaves it set: an extra wake is free, a missed one is a stall.
                if futex.wake(sem_addr, 1) == 0 {
                    clear_waiters_flag(mem, sem_addr)?;
                }
            }
            return Ok(0);
        }
        // raced: re-read
    }
}

/// `sem_getvalue`: returns the current value (the waiter flag is masked off,
/// matching bionic). A negative return from POSIX's sval convention (negative =
/// waiters) is NOT used: bionic reports the plain count.
pub fn getvalue(
    mem: &mut (impl GuestMemory + GuestAtomic),
    sem_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_addr(sem_addr)?;
    let word = read_word(mem, sem_addr)?;
    Ok((word & sem_bits::VALUE_MASK) as i32)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Drop the waiter flag, leaving the count alone. Used only where a wake has just
/// proved the futex queue empty.
fn clear_waiters_flag(
    mem: &mut (impl GuestMemory + GuestAtomic),
    addr: u64,
) -> Result<(), crate::memory::Fault> {
    loop {
        let word = read_word(mem, addr)?;
        if word & sem_bits::WAITERS == 0 {
            return Ok(());
        }
        if mem.cas_u32(addr, word, word & !sem_bits::WAITERS)? {
            return Ok(());
        }
    }
}

fn read_word(mem: &impl GuestMemory, addr: u64) -> Result<u32, crate::memory::Fault> {
    let mut b = [0u8; 4];
    mem.read(addr, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// The sem-family error convention: return -1 with `errno` set.
///
/// The guest's errno lives behind the adapter's context
/// ([`crate::context::GuestContext::set_errno`]), which will wrap this memory.
/// To keep the sem functions testable without a full context, the errno code is
/// returned in the `Err`-carrying position of the *value*: these functions
/// return `Ok(-1)` and the adapter reads the pending code from
/// [`crate::sem::last_errno`]. This is the one piece of hidden state in the
/// sync layer — documented in the report (§3) and keyed per-thread so
/// concurrent guest threads never see each other's errno.
fn errno_result(
    mem: &mut impl GuestMemory,
    code: i32,
) -> Result<i32, crate::memory::Fault> {
    let _ = mem;
    LAST_ERRNO.with(|c| c.set(code));
    Ok(-1)
}

thread_local! {
    static LAST_ERRNO: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// The pending errno from the most recent failed sem call **on this thread**.
/// The adapter copies this into the guest's errno slot before returning from
/// the thunk; pthread-family functions never touch it.
pub fn last_errno() -> i32 {
    LAST_ERRNO.with(|c| c.get())
}

fn check_addr(addr: u64) -> Result<(), crate::memory::Fault> {
    if addr == 0 {
        return Err(crate::memory::Fault(0));
    }
    match addr.checked_add(sizes::SEM_T - 1) {
        Some(_) => Ok(()),
        None => Err(crate::memory::Fault(addr)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;
    use crate::mock_threads::MockFutex;
    use crate::shared_mem::SharedMockMemory;

    fn placed() -> SharedMockMemory {
        SharedMockMemory::new({
            let mut m = MockMemory::new();
            m.map(0x1000, &[0u8; 4]);
            m
        })
    }

    /// init -> wait consumes; trywait on zero is EAGAIN (errno convention: -1).
    #[test]
    fn wait_trywait_eagain() {
        let mem = placed();
        let mut m = mem.clone();
        assert_eq!(init(&mut m, 0x1000, 0, 1).unwrap(), 0);
        assert_eq!(trywait(&mut m, 0x1000).unwrap(), 0);
        // Zero now: trywait returns -1 (errno set by the adapter path).
        let r = trywait(&mut m, 0x1000).unwrap();
        assert_eq!(r, -1, "trywait on zero returns -1 (errno convention)");
        assert_eq!(getvalue(&mut m, 0x1000).unwrap(), 0);
    }

    /// post increments; getvalue reports the count.
    #[test]
    fn post_getvalue() {
        let mem = placed();
        let mut m = mem.clone();
        init(&mut m, 0x1000, 0, 0).unwrap();
        assert_eq!(getvalue(&mut m, 0x1000).unwrap(), 0);
        assert_eq!(post(&mut m, &MockFutex::new(), 0x1000).unwrap(), 0);
        assert_eq!(post(&mut m, &MockFutex::new(), 0x1000).unwrap(), 0);
        assert_eq!(getvalue(&mut m, 0x1000).unwrap(), 2);
    }

    /// wait blocks on zero and is released by a post from another host thread
    /// (real blocking handover).
    #[test]
    fn wait_blocks_until_post() {
        let mem = placed();
        let futex = std::sync::Arc::new(MockFutex::new());
        {
            let mut m = mem.clone();
            init(&mut m, 0x1000, 0, 0).unwrap();
        }
        let waker = {
            let (mem, futex) = (mem.clone(), futex.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                let mut m = mem.clone();
                post(&mut m, &*futex, 0x1000).unwrap();
            })
        };
        let start = std::time::Instant::now();
        let r = wait(&mut mem.clone(), &*futex, 0x1000).unwrap();
        let elapsed = start.elapsed();
        waker.join().unwrap();
        assert_eq!(r, 0);
        crate::timing::assert_blocked_for(
            elapsed, Duration::from_millis(120), "sem_wait blocks until the post");
        assert_eq!(getvalue(&mut mem.clone(), 0x1000).unwrap(), 0);
    }

    /// timedwait times out with elapsed >= timeout (and clears the waiter flag).
    #[test]
    fn timedwait_times_out() {
        let mem = placed();
        let futex = MockFutex::new();
        let mut m = mem.clone();
        init(&mut m, 0x1000, 0, 0).unwrap();
        let start = std::time::Instant::now();
        let r = timedwait(&mut m, &futex, 0x1000, Duration::from_millis(140)).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(r, -1, "timedwait expiry: -1 with errno ETIMEDOUT (adapter)");
        crate::timing::assert_blocked_for(
            elapsed, Duration::from_millis(140), "sem_timedwait");
        // Waiter flag cleared: a subsequent post does not need a wake.
        let mut b = [0u8; 4];
        mem.read(0x1000, &mut b).unwrap();
        assert_eq!(u32::from_le_bytes(b) & sem_bits::WAITERS, 0, "flag cleared");
    }

    /// init rejects values above SEM_VALUE_MAX with -1 (EINVAL via adapter).
    #[test]
    fn init_value_overflow() {
        let mem = placed();
        let mut m = mem.clone();
        let r = init(&mut m, 0x1000, 0, sem_bits::VALUE_MASK + 1).unwrap();
        assert_eq!(r, -1);
        // Exactly SEM_VALUE_MAX is fine.
        assert_eq!(init(&mut m, 0x1000, 0, sem_bits::VALUE_MASK).unwrap(), 0);
    }

    /// post overflow past SEM_VALUE_MAX is -1/EINVAL.
    #[test]
    fn post_overflow() {
        let mem = placed();
        let mut m = mem.clone();
        init(&mut m, 0x1000, 0, sem_bits::VALUE_MASK).unwrap();
        assert_eq!(post(&mut m, &MockFutex::new(), 0x1000).unwrap(), -1);
    }

    /// A STALE waiter flag -- set, but with nothing actually blocked -- must NOT
    /// refuse `sem_destroy`. The flag is conservative by design (`post` preserves
    /// it so a second waiter cannot be stranded), so it outlives the waiters that
    /// set it; treating it as proof of a waiter turns every previously-contended
    /// semaphore into one that can never be destroyed.
    #[test]
    fn destroy_succeeds_when_the_waiter_flag_is_stale() {
        let mem = placed();
        let mut m = mem.clone();
        init(&mut m, 0x1000, 0, 1).unwrap();
        let mut b = [0u8; 4];
        m.read(0x1000, &mut b).unwrap();
        let word = u32::from_le_bytes(b) | sem_bits::WAITERS;
        m.write(0x1000, &word.to_le_bytes()).unwrap();
        // Nothing is blocked on this address.
        assert_eq!(destroy(&mut m, &MockFutex::new(), 0x1000).unwrap(), 0);
    }

    /// ...but a REAL blocked waiter still refuses EBUSY. This is the property the
    /// stale-flag fix must not destroy, so it is asserted against a thread that is
    /// genuinely parked on the futex rather than against a hand-set bit.
    #[test]
    fn destroy_refuses_ebusy_with_a_real_blocked_waiter() {
        use std::sync::Arc;
        let mem = placed();
        let futex = Arc::new(MockFutex::new());
        init(&mut mem.clone(), 0x1000, 0, 0).unwrap();

        let (m2, f2) = (mem.clone(), futex.clone());
        let waiter = std::thread::spawn(move || {
            let mut m = m2.clone();
            wait(&mut m, &*f2, 0x1000).unwrap()
        });
        // Let it genuinely park on the futex.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut m = mem.clone();
        assert_eq!(
            destroy(&mut m, &*futex, 0x1000).unwrap(),
            -1,
            "destroy must refuse while a thread is really blocked",
        );

        // Release the waiter so the test cannot hang.
        post(&mut m, &*futex, 0x1000).unwrap();
        assert_eq!(waiter.join().unwrap(), 0);
    }

    /// Many posts/waiters: N tokens satisfy N waits with none lost (no dup).
    #[test]
    fn tokens_are_not_lost_or_duplicated() {
        let mem = placed();
        let futex = std::sync::Arc::new(MockFutex::new());
        init(&mut mem.clone(), 0x1000, 0, 0).unwrap();
        // Post 5 tokens.
        for _ in 0..5 {
            assert_eq!(post(&mut mem.clone(), &*futex, 0x1000).unwrap(), 0);
        }
        // 8 threads race to wait; exactly 5 succeed, 3 see zero (would block —
        // they use trywait so they finish).
        let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let (mem, results, barrier) = (mem.clone(), results.clone(), barrier.clone());
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let r = trywait(&mut mem.clone(), 0x1000).unwrap();
                results.lock().unwrap().push(r);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let rs = results.lock().unwrap();
        let oks = rs.iter().filter(|&&r| r == 0).count();
        let fails = rs.iter().filter(|&&r| r == -1).count();
        assert_eq!(oks, 5, "exactly the posted tokens are consumed");
        assert_eq!(fails, 3, "the rest see EAGAIN");
        assert_eq!(getvalue(&mut mem.clone(), 0x1000).unwrap(), 0);
    }

    /// Guard regions: writes stay inside the 4-byte sem.
    #[test]
    fn writes_stay_in_struct() {
        let mem = SharedMockMemory::new({
            let mut m = MockMemory::new();
            m.map(0x0FF0, &[0xA5; 16]);
            m.map(0x1000, &[0u8; 4]);
            m.map(0x1004, &[0xA5; 16]);
            m
        });
        let f = MockFutex::new();
        init(&mut mem.clone(), 0x1000, 0, 3).unwrap();
        post(&mut mem.clone(), &f, 0x1000).unwrap();
        trywait(&mut mem.clone(), 0x1000).unwrap();
        trywait(&mut mem.clone(), 0x1000).unwrap();
        destroy(&mut mem.clone(), &f, 0x1000).unwrap();
        for (addr, len) in [(0x0FF0u64, 16usize), (0x1004, 16)] {
            let mut buf = vec![0u8; len];
            mem.read(addr, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0xA5), "guard corrupted at {addr:#x}");
        }
    }

    /// Hostile: null/unmapped/wrapping addresses fault.
    #[test]
    fn hostile_inputs() {
        let mem = placed();
        let mut m = mem.clone();
        assert!(wait(&mut m, &MockFutex::new(), 0).is_err());
        assert!(trywait(&mut m, 0).is_err());
        assert!(post(&mut m, &MockFutex::new(), 0).is_err());
        assert!(init(&mut m, 0, 0, 1).is_err());
        assert!(init(&mut m, u64::MAX, 0, 1).is_err());
        assert!(getvalue(&mut m, 0xdead_0000).is_err());
    }
}
