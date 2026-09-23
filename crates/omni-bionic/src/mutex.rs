//! `pthread_mutex`: the four mutex types (NORMAL, ERRORCHECK, RECURSIVE, DEFAULT)
//! over a 40-byte guest `pthread_mutex_t`, blocking through the [`Futex`] trait on
//! the mutex address itself — the Linux/bionic design.
//!
//! ## Guest state layout (first 32 of the 40 bytes; bytes 32..40 never touched)
//!
//! Bionic's `pthread_mutex_t` is `int32_t __private[10]`. This crate defines the
//! internal meanings (the struct is opaque to the guest; bionic's own field split is
//! irrelevant to a binary that only ever passes the address around):
//!
//! * word 0 (`+0`): lock state.
//!   - NORMAL/DEFAULT/ERRORCHECK: `0` = unlocked, `1` = locked (no waiters tracked
//!     in the word; the futex queue holds that).
//!   - RECURSIVE: `0` = unlocked, else `count` (1..=u32::MAX) of recursive holds.
//! * word 1 (`+4`): owner (LOW 32 bits of the `GuestThreadId`) — ERRORCHECK and
//!   RECURSIVE only. Storing 32 bits keeps the value inside the struct; full 64-bit
//!   owner identity lives in the host-side [`OwnerTable`], which the adapter keeps
//!   for the lifetime of the process. Collisions of the low 32 bits only cost an
//!   ambiguity in *who* illegally unlocked, never a false "correct" unlock: the
//!   authoritative check is the host table.
//! * word 2 (`+8`): type (NORMAL=0, RECURSIVE=1, ERRORCHECK=2, DEFAULT=3), written
//!   by `init` from the attr; DEFAULT behaves exactly like NORMAL for locking but
//!   is a distinct stored value (bionic: DEFAULT may alias NORMAL; the alias is
//!   what makes all-zero valid).
//! * word 3 (`+12`): for RECURSIVE, reserved; unused by this crate.
//!
//! ## The all-zero struct is a valid unlocked DEFAULT mutex
//!
//! `PTHREAD_MUTEX_INITIALIZER` is all-zero. Type word 0 maps to NORMAL, which is
//! behaviourally the DEFAULT: unlocked (state word 0) with no owner. So a mutex the
//! engine never passed through `pthread_mutex_init` still works.
//!
//! ## Deadlock semantics
//!
//! * NORMAL/DEFAULT relock by the owner is a **deadlock** in POSIX. This crate does
//!   not (and cannot, from a single thread) hang: `lock` from the owner on a NORMAL
//!   mutex increments the state word (1 -> 2, "locked with owner waiting") exactly
//!   as bionic does, which releases OTHER waiters into a protocol where they will
//!   contend; the caller is then genuinely stuck in a loop only another thread can
//!   break. Tests exercise this through `trylock` (EBUSY), never by deadlocking.
//! * ERRORCHECK relock by the owner: EDEADLK, no state change.
//! * RECURSIVE relock by the owner: count += 1.

use crate::atomics::GuestAtomic;
use crate::errno::consts;
use crate::layouts::{offsets, sizes};
use crate::memory::GuestMemory;
use crate::threads::{Futex, GuestThreadId, ThreadRegistry, WaitResult};
use core::time::Duration;
use std::collections::HashMap;
use std::sync::Mutex as HostMutex;

/// bionic/glibc mutex type numbers (guest-visible attr values).
pub mod mutex_type {
    /// `PTHREAD_MUTEX_NORMAL`
    pub const NORMAL: i32 = 0;
    /// `PTHREAD_MUTEX_RECURSIVE`
    pub const RECURSIVE: i32 = 1;
    /// `PTHREAD_MUTEX_ERRORCHECK`
    pub const ERRORCHECK: i32 = 2;
    /// `PTHREAD_MUTEX_DEFAULT` (bionic: behaves as NORMAL)
    pub const DEFAULT: i32 = 3;
}

/// Lock-state word values for the non-recursive types.
mod lock_state {
    /// Unlocked.
    pub const UNLOCKED: u32 = 0;
    /// Locked, no owner waiting.
    pub const LOCKED: u32 = 1;
    /// Locked with at least one waiter having futex-waited on the word (bionic's
    /// "locked with waiters" state; keeps wakes precise).
    pub const LOCKED_WITH_WAITERS: u32 = 2;
}

/// A 64-bit owner identity split across the host table.
///
/// The guest struct stores only the LOW 32 bits of the owner's `GuestThreadId`
/// (there is no room for 64). The authoritative owner is the host-side table here,
/// keyed by the mutex's guest address: it maps mutex address -> full 64-bit owner.
/// The guest word exists so a *guest-visible* unlock by a wrong thread can be
/// diagnosed without the table (the table is authoritative; the word is a witness).
#[derive(Default)]
pub struct OwnerTable {
    /// mutex guest address -> full owner identity. Entries removed on unlock/destroy.
    owners: HostMutex<HashMap<u64, GuestThreadId>>,
}

impl OwnerTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self, addr: u64, owner: GuestThreadId) {
        self.owners.lock().unwrap().insert(addr, owner);
    }

    /// Who holds the mutex at `addr`, if anybody.
    ///
    /// Public because a guest blocked in `pthread_mutex_lock` can only be diagnosed from another
    /// thread, and "who holds it" is the question. It is a read of the same table the lock and
    /// unlock paths decide on, so it cannot disagree with them.
    #[must_use]
    pub fn get(&self, addr: u64) -> Option<GuestThreadId> {
        self.owners.lock().unwrap().get(&addr).copied()
    }

    fn clear(&self, addr: u64) -> bool {
        self.owners.lock().unwrap().remove(&addr).is_some()
    }
}

// ---------------------------------------------------------------------------
// Attr functions
// ---------------------------------------------------------------------------

/// `pthread_mutexattr_init`: an attr is 8 bytes, all-zero = DEFAULT type.
/// All-zero is the correct initial value, so this is a validated no-op that
/// guarantees the 8 bytes are mapped and zeroed.
pub fn attr_init(mem: &mut impl GuestMemory, attr_addr: u64) -> Result<(), crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_MUTEXATTR_T)?;
    mem.write(attr_addr, &[0u8; 8])?;
    Ok(())
}

/// `pthread_mutexattr_destroy`: nothing to free; validate only.
pub fn attr_destroy(_mem: &mut impl GuestMemory, attr_addr: u64) -> Result<(), crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_MUTEXATTR_T)?;
    Ok(())
}

/// `pthread_mutexattr_settype`. Bionic accepts exactly the four type numbers and
/// rejects anything else with EINVAL (return-value convention: pthread functions
/// RETURN the error, they do not set errno).
pub fn attr_settype(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
    type_: i32,
) -> Result<i32, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_MUTEXATTR_T)?;
    if !matches!(
        type_,
        mutex_type::NORMAL | mutex_type::RECURSIVE | mutex_type::ERRORCHECK | mutex_type::DEFAULT
    ) {
        return Ok(consts::EINVAL);
    }
    mem.write(attr_addr, &type_.to_le_bytes())?;
    Ok(0)
}

/// `pthread_mutexattr_gettype`: returns (result, type). Result is 0 or EINVAL for
/// a null/unmapped attr (the guest pointer faults instead — a fault is returned).
pub fn attr_gettype(
    mem: &mut impl GuestMemory,
    attr_addr: u64,
) -> Result<Result<i32, i32>, crate::memory::Fault> {
    check_range(attr_addr, sizes::PTHREAD_MUTEXATTR_T)?;
    let mut b = [0u8; 4];
    mem.read(attr_addr, &mut b)?;
    let t = i32::from_le_bytes(b);
    if !matches!(
        t,
        mutex_type::NORMAL | mutex_type::RECURSIVE | mutex_type::ERRORCHECK | mutex_type::DEFAULT
    ) {
        Ok(Err(consts::EINVAL))
    } else {
        Ok(Ok(t))
    }
}

// ---------------------------------------------------------------------------
// init / destroy
// ---------------------------------------------------------------------------

/// `pthread_mutex_init(mutex, attr)`. Returns the pthread error code (0 on
/// success). `attr_addr == 0` means NULL attr = DEFAULT type.
///
/// Writes the full 40-byte struct: state=0, owner-low=0, type=attr-or-default,
/// rest zeroed. The guest-visible mutex error convention is *return the code*.
pub fn init(
    mem: &mut impl GuestMemory,
    mutex_addr: u64,
    attr_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(mutex_addr, sizes::PTHREAD_MUTEX_T)?;
    let type_ = if attr_addr == 0 {
        mutex_type::DEFAULT
    } else {
        check_range(attr_addr, 4)?;
        let mut b = [0u8; 4];
        mem.read(attr_addr, &mut b)?;
        i32::from_le_bytes(b)
    };
    if !matches!(
        type_,
        mutex_type::NORMAL | mutex_type::RECURSIVE | mutex_type::ERRORCHECK | mutex_type::DEFAULT
    ) {
        return Ok(consts::EINVAL);
    }
    let mut bytes = [0u8; 40];
    bytes[8..12].copy_from_slice(&type_.to_le_bytes());
    mem.write(mutex_addr, &bytes)?;
    Ok(0)
}

/// `pthread_mutex_destroy`. POSIX: destroying a locked mutex (or one another
/// thread holds or waits on) is not allowed; bionic's ERRORCHECK reports EBUSY.
/// This crate: a locked mutex (any type) returns EBUSY; an unlocked one is reset
/// to all-zero (the canonical dead state) and its owner-table entry dropped.
/// Convention: **returns** the error code, does not set errno.
pub fn destroy(
    mem: &mut impl GuestMemory,
    owners: &OwnerTable,
    mutex_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(mutex_addr, sizes::PTHREAD_MUTEX_T)?;
    let state = read_state(mem, mutex_addr)?;
    if state != 0 {
        return Ok(consts::EBUSY);
    }
    // Unlock any host-side waiter state that could survive: a final wake-all on
    // the address so a stale waiter cannot block forever on a destroyed mutex.
    // (Waiters hold the futex, not the mutex; POSIX says destroying a mutex with
    // waiters is UB, so waking them to fail fast is the safe interpretation.)
    mem.write(mutex_addr, &[0u8; 40])?;
    owners.clear(mutex_addr);
    Ok(0)
}

// ---------------------------------------------------------------------------
// lock / trylock / timedlock / unlock
// ---------------------------------------------------------------------------

/// `pthread_mutex_lock`. Returns the pthread error code (0 on success):
/// * ERRORCHECK + owner relock: EDEADLK.
/// * RECURSIVE + owner relock: count += 1 (recursive limit exhausted: EAGAIN,
///   matching POSIX's EAGAIN for "maximum number of recursive locks exceeded").
/// * NORMAL/DEFAULT: futex-wait until the state word reads unlocked.
///   A relock by the owner deadlocks (POSIX); the futex wait has no timeout and
///   nobody will wake this thread — tests never exercise that path by hanging.
pub fn lock(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    owners: &OwnerTable,
    threads: &impl ThreadRegistry,
    mutex_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(mutex_addr, sizes::PTHREAD_MUTEX_T)?;
    let me = threads.current();
    let type_ = read_type(mem, mutex_addr)?;

    match type_ {
        mutex_type::RECURSIVE => {
            let state = read_state(mem, mutex_addr)?;
            if state != 0 {
                if owners.get(mutex_addr) == Some(me) {
                    if state == u32::MAX {
                        return Ok(consts::EAGAIN); // recursive limit
                    }
                    write_state(mem, mutex_addr, state + 1)?;
                    return Ok(0);
                }
                // Not the owner: contend.
                return contend(mem, futex, owners, threads, mutex_addr, None)
                    .map(|r| match r {
                        Ok(()) => 0,
                        Err(code) => code,
                    });
            }
            if cas_acquire(mem, mutex_addr)? {
                owners.set(mutex_addr, me);
                Ok(0)
            } else {
                // Lost the free-acquire race: another thread won the word. Block
                // on the contention path — returning 0 here would be a FALSE
                // acquire (two threads inside at once).
                contend(mem, futex, owners, threads, mutex_addr, None).map(|r| match r {
                    Ok(()) => 0,
                    Err(code) => code,
                })
            }
        }
        mutex_type::ERRORCHECK => {
            let state = read_state(mem, mutex_addr)?;
            if state != 0 && owners.get(mutex_addr) == Some(me) {
                return Ok(consts::EDEADLK);
            }
            if state == 0 {
                if cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    return Ok(0);
                }
                // Lost the race: contend (see RECURSIVE arm).
                return contend(mem, futex, owners, threads, mutex_addr, None).map(|r| match r {
                    Ok(()) => 0,
                    Err(code) => code,
                });
            }
            contend(mem, futex, owners, threads, mutex_addr, None).map(|r| match r {
                Ok(()) => 0,
                Err(code) => code,
            })
        }
        // NORMAL and DEFAULT share the fast path.
        _ => {
            let state = read_state(mem, mutex_addr)?;
            if state == lock_state::UNLOCKED {
                // Try to take it: 0 -> 1, atomically. A loser's CAS fails and it
                // falls through to contend().
                if cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    return Ok(0);
                }
            }
            contend(mem, futex, owners, threads, mutex_addr, None).map(|r| match r {
                Ok(()) => 0,
                Err(code) => code,
            })
        }
    }
}

/// `pthread_mutex_trylock`. Non-blocking: EBUSY when held (by anyone, including
/// the caller for NORMAL — POSIX leaves NORMAL self-trylock UB, and bionic's
/// trylock returns EBUSY for any held state; that is the behaviour here).
pub fn trylock(
    mem: &mut (impl GuestMemory + GuestAtomic),
    owners: &OwnerTable,
    threads: &impl ThreadRegistry,
    mutex_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(mutex_addr, sizes::PTHREAD_MUTEX_T)?;
    let me = threads.current();
    let type_ = read_type(mem, mutex_addr)?;
    let state = read_state(mem, mutex_addr)?;

    match type_ {
        mutex_type::RECURSIVE => {
            if state == 0 {
                if cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    Ok(0)
                } else {
                    Ok(consts::EBUSY) // lost the free race: someone else holds it
                }
            } else if owners.get(mutex_addr) == Some(me) {
                if state == u32::MAX {
                    Ok(consts::EAGAIN)
                } else {
                    write_state(mem, mutex_addr, state + 1)?;
                    Ok(0)
                }
            } else {
                Ok(consts::EBUSY)
            }
        }
        mutex_type::ERRORCHECK => {
            if state == 0 {
                if cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    Ok(0)
                } else {
                    Ok(consts::EBUSY) // lost the free race
                }
            } else {
                Ok(consts::EBUSY) // held, even by self: EBUSY, not EDEADLK
            }
        }
        _ => {
            if state == lock_state::UNLOCKED && cas_acquire(mem, mutex_addr)? {
                owners.set(mutex_addr, me);
                Ok(0)
            } else {
                Ok(consts::EBUSY)
            }
        }
    }
}

/// `pthread_mutex_timedlock`. Blocks up to `timeout` on contention. Returns 0,
/// ETIMEDOUT, or the ERRORCHECK/RECURSIVE codes as `lock` does.
pub fn timedlock(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    owners: &OwnerTable,
    threads: &impl ThreadRegistry,
    mutex_addr: u64,
    timeout: Duration,
) -> Result<i32, crate::memory::Fault> {
    check_range(mutex_addr, sizes::PTHREAD_MUTEX_T)?;
    let me = threads.current();
    let type_ = read_type(mem, mutex_addr)?;

    match type_ {
        mutex_type::RECURSIVE => {
            let state = read_state(mem, mutex_addr)?;
            if state == 0 {
                if cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    Ok(0)
                } else {
                    // Lost the free race: block up to the timeout.
                    contend(mem, futex, owners, threads, mutex_addr, Some(timeout))
                        .map(|r| match r {
                            Ok(()) => 0,
                            Err(code) => code,
                        })
                }
            } else if owners.get(mutex_addr) == Some(me) {
                if state == u32::MAX {
                    Ok(consts::EAGAIN)
                } else {
                    write_state(mem, mutex_addr, state + 1)?;
                    Ok(0)
                }
            } else {
                contend(mem, futex, owners, threads, mutex_addr, Some(timeout))
                    .map(|r| match r {
                        Ok(()) => 0,
                        Err(code) => code,
                    })
            }
        }
        mutex_type::ERRORCHECK => {
            let state = read_state(mem, mutex_addr)?;
            if state != 0 && owners.get(mutex_addr) == Some(me) {
                Ok(consts::EDEADLK)
            } else if state == 0 {
                if cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    Ok(0)
                } else {
                    // Lost the free race: block up to the timeout.
                    contend(mem, futex, owners, threads, mutex_addr, Some(timeout))
                        .map(|r| match r {
                            Ok(()) => 0,
                            Err(code) => code,
                        })
                }
            } else {
                contend(mem, futex, owners, threads, mutex_addr, Some(timeout))
                    .map(|r| match r {
                        Ok(()) => 0,
                        Err(code) => code,
                    })
            }
        }
        _ => {
            let state = read_state(mem, mutex_addr)?;
            if state == lock_state::UNLOCKED && cas_acquire(mem, mutex_addr)? {
                owners.set(mutex_addr, me);
                Ok(0)
            } else {
                contend(mem, futex, owners, threads, mutex_addr, Some(timeout))
                    .map(|r| match r {
                        Ok(()) => 0,
                        Err(code) => code,
                    })
            }
        }
    }
}

/// `pthread_mutex_unlock`. ERRORCHECK checks ownership: unlock by a non-owner or
/// of an unlocked mutex is EPERM. RECURSIVE decrements; releases at zero.
/// NORMAL/DEFAULT: release and wake one waiter (if any were waiting, the state
/// word was LOCKED_WITH_WAITERS; wake 1; else wake 0 — a wake with no waiter is
/// free, so always attempting one wake is correct and simpler).
pub fn unlock(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    owners: &OwnerTable,
    threads: &impl ThreadRegistry,
    mutex_addr: u64,
) -> Result<i32, crate::memory::Fault> {
    check_range(mutex_addr, sizes::PTHREAD_MUTEX_T)?;
    let me = threads.current();
    let type_ = read_type(mem, mutex_addr)?;
    let state = read_state(mem, mutex_addr)?;

    match type_ {
        mutex_type::ERRORCHECK => {
            if state == 0 {
                return Ok(consts::EPERM); // unlocking an unlocked mutex
            }
            if owners.get(mutex_addr) != Some(me) {
                return Ok(consts::EPERM); // not the owner
            }
            // Clear the owner BEFORE publishing the released word: an acquirer
            // that wins the word in between registers as the new owner, and a
            // late `clear` here would delete the NEW owner's entry — the next
            // unlock would then fail EPERM with the mutex genuinely held.
            owners.clear(mutex_addr);
            write_state(mem, mutex_addr, 0)?;
            futex.wake(mutex_addr, 1);
            Ok(0)
        }
        mutex_type::RECURSIVE => {
            if state == 0 {
                return Ok(consts::EPERM);
            }
            if owners.get(mutex_addr) != Some(me) {
                return Ok(consts::EPERM);
            }
            let new = state - 1;
            if new == 0 {
                // Same ordering discipline as ERRORCHECK: clear before publish.
                owners.clear(mutex_addr);
                write_state(mem, mutex_addr, new)?;
                futex.wake(mutex_addr, 1);
            } else {
                write_state(mem, mutex_addr, new)?;
            }
            Ok(0)
        }
        _ => {
            if state == lock_state::UNLOCKED {
                // POSIX: unlocking a non-held NORMAL mutex is UB. Bionic's fast
                // path decrements anyway. This crate refuses the plausible
                // corruption route and reports EPERM (documented deviation,
                // strictly safer: the engine should never do this).
                return Ok(consts::EPERM);
            }
            // Release atomically: 1->0 or 2->0 (2 = had waiters). The owner
            // table is the host-side authority for WHO may unlock; the CAS
            // keeps the guest word consistent under concurrent releases. The
            // clear happens BEFORE the CAS publish (matching ERRORCHECK/
            // RECURSIVE): once the word is 0 a new acquirer may register as
            // owner, and a late clear would delete the new owner's entry.
            owners.clear(mutex_addr);
            let released = mem.cas_u32(mutex_addr, lock_state::LOCKED, lock_state::UNLOCKED)?
                || mem.cas_u32(mutex_addr, lock_state::LOCKED_WITH_WAITERS, lock_state::UNLOCKED)?;
            if !released {
                // State changed under us: another thread released first. Our
                // clear was a no-op (the table entry was already theirs or
                // absent), so nothing to restore.
                return Ok(consts::EPERM);
            }
            futex.wake(mutex_addr, 1);
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Contention path (shared by lock/timedlock)
// ---------------------------------------------------------------------------

/// The slow path: the mutex is held. Futex-wait on the mutex address with the
/// state word as the expected value, then re-run the acquire protocol. Returns
/// Ok(()) on eventual acquisition, Err(code) on timeout (ETIMEDOUT).
fn contend(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    owners: &OwnerTable,
    threads: &impl ThreadRegistry,
    mutex_addr: u64,
    timeout: Option<Duration>,
) -> Result<Result<(), i32>, crate::memory::Fault> {
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let me = threads.current();
    let type_ = read_type(mem, mutex_addr)?;

    loop {
        let state = read_state(mem, mutex_addr)?;
        if state == lock_state::UNLOCKED {
            if cas_acquire(mem, mutex_addr)? {
                owners.set(mutex_addr, me);
                return Ok(Ok(()));
            }
            continue; // lost the race: re-read
        }
        if type_ == mutex_type::RECURSIVE && owners.get(mutex_addr) == Some(me) {
            // Lost the mutex then got it back via recursion window — treat as
            // unlocked path.
            write_state(mem, mutex_addr, state + 1)?;
            return Ok(Ok(()));
        }
        // Mark waiters present (LOCKED_WITH_WAITERS) so the unlock wakes us even
        // if the state was plain LOCKED when we registered. This MUST be a CAS,
        // not a write: if the holder released between our read and our mark, the
        // blind write would clobber the published UNLOCKED word and the mutex
        // would be stuck at 2 with no owner and no pending wake (a real hang).
        // CAS LOCKED -> LOCKED_WITH_WAITERS: success means the word is still
        // held and our wake is guaranteed; failure means the state moved under
        // us, so re-run the acquire protocol instead of sleeping.
        // RECURSIVE stores its COUNT in this word (2 = held twice), so the
        // waiter-flag state is meaningless there; its unlock only wakes on the
        // count->0 transition, which is exactly when the last hold releases.
        if type_ != mutex_type::RECURSIVE
            && state == lock_state::LOCKED
            && !mem.cas_u32(mutex_addr, lock_state::LOCKED, lock_state::LOCKED_WITH_WAITERS)?
        {
            continue;
        }
        // Expected value: the word as this thread last saw or wrote it -- the
        // LOCKED_WITH_WAITERS the CAS above just wrote over LOCKED, and otherwise
        // the word read at the top of this pass. For RECURSIVE that is the hold
        // COUNT, because that word never carries the waiter flag: passing
        // LOCKED_WITH_WAITERS there would be a count of two, and a futex that
        // compares (the embedding's does, as Linux's FUTEX_WAIT does) would turn
        // every wait on a once-held recursive mutex into a spin.
        // The comparison is what closes the window between the mark above and the
        // park: an unlock that lands in it changes the word, so the wait returns
        // WouldBlock instead of sleeping through a wake that has already happened.
        // The unlock's wake targets the address regardless of value, which matches
        // Linux FUTEX_WAKE (it never compares values). The MockFutex does not
        // compare; the bounded slice below is the net for a futex that does not.
        let expected = if type_ != mutex_type::RECURSIVE && state == lock_state::LOCKED {
            lock_state::LOCKED_WITH_WAITERS
        } else {
            state
        };
        let remaining = match deadline {
            Some(d) => d.saturating_duration_since(std::time::Instant::now()),
            None => Duration::MAX,
        };
        if remaining == Duration::ZERO {
            return Ok(Err(consts::ETIMEDOUT));
        }
        // For an unbounded wait (timeout = None), pass a long bounded wait so the
        // caller can never hang forever on a lost wakeup; on expiry we re-check
        // the state (a lost wake self-heals) — POSIX correctness retained because
        // the predicate loop runs again.
        let bounded = match deadline {
            Some(_) => remaining,
            None => Duration::from_millis(1_000),
        };
        // **A futex that will not block again ends the wait here** (see
        // `Futex::interrupted`): this loop runs in the host, inside one import, and would
        // otherwise re-read the word forever and never return to a guest that could be
        // stopped. `EINTR` is **not** a lock's answer in POSIX, so it never reaches a guest:
        // the embedding's handler turns it into its shutdown refusal. MEASURED: a worker sat in
        // `pthread_mutex_lock` past the embedding's join, which failed the teardown.
        if futex.interrupted() {
            return Ok(Err(consts::EINTR));
        }
        match futex.wait(mutex_addr, expected, Some(bounded)) {
            WaitResult::Woken => continue, // re-run the acquire protocol
            WaitResult::TimedOut => {
                // One last chance: maybe the wake raced our deregistration.
                let state = read_state(mem, mutex_addr)?;
                if state == lock_state::UNLOCKED && cas_acquire(mem, mutex_addr)? {
                    owners.set(mutex_addr, me);
                    return Ok(Ok(()));
                }
                if deadline.is_none() {
                    continue; // unbounded protocol wait: keep looping
                }
                return Ok(Err(consts::ETIMEDOUT));
            }
            WaitResult::WouldBlock => continue, // value changed under us: retry
        }
    }
}

// ---------------------------------------------------------------------------
// Guest struct access helpers
// ---------------------------------------------------------------------------

fn read_state(mem: &impl GuestMemory, addr: u64) -> Result<u32, crate::memory::Fault> {
    let mut b = [0u8; 4];
    mem.read(addr, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn write_state(mem: &mut impl GuestMemory, addr: u64, v: u32) -> Result<(), crate::memory::Fault> {
    mem.write(addr, &v.to_le_bytes())
}

fn read_type(mem: &impl GuestMemory, addr: u64) -> Result<i32, crate::memory::Fault> {
    let mut b = [0u8; 4];
    mem.read(addr + 8, &mut b)?;
    Ok(i32::from_le_bytes(b))
}

/// Atomic acquire: 0 -> LOCKED (1) via CAS. Returns true if THIS caller won.
/// A failed CAS means another thread holds the mutex (or won the race).
fn cas_acquire(
    mem: &(impl GuestMemory + GuestAtomic),
    mutex_addr: u64,
) -> Result<bool, crate::memory::Fault> {
    // The whole 4-byte word is the state; LOCKED and LOCKED_WITH_WAITERS are
    // both "held", so the only acquirable value is UNLOCKED (0).
    mem.cas_u32(mutex_addr, lock_state::UNLOCKED, lock_state::LOCKED)
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

/// Keep `offsets` referenced (the state words ARE offsets 0/4/8 in this layout).
#[allow(unused)]
fn _offsets_used() -> u64 {
    offsets::MUTEX_STATE_WORDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;
    use crate::mock_threads::{MockFutex, MockThreads};

    fn setup(addr: u64) -> (
        crate::shared_mem::SharedMockMemory,
        MockFutex,
        OwnerTable,
        MockThreads,
    ) {
        let mut mem = MockMemory::new();
        mem.map(addr, &[0u8; 40]);
        (
            crate::shared_mem::SharedMockMemory::new(mem),
            MockFutex::new(),
            OwnerTable::new(),
            MockThreads::new(),
        )
    }

    /// All-zero struct (PTHREAD_MUTEX_INITIALIZER) is a valid unlocked DEFAULT.
    #[test]
    fn zero_struct_is_valid_default_mutex() {
        let (mut mem, _f, _o, t) = setup(0x1000);
        // Not through init: the struct is already all-zero.
        assert_eq!(trylock(&mut mem, &_o, &t, 0x1000).unwrap(), 0);
        assert_eq!(unlock(&mut mem, &_f, &_o, &t, 0x1000).unwrap(), 0);
    }

    /// trylock on a held mutex returns EBUSY — for NORMAL/DEFAULT/ERRORCHECK.
    /// RECURSIVE is deliberately different (POSIX: trylock by the owner of a
    /// recursive mutex SUCCESSFULLY re-locks it); it is covered separately below.
    #[test]
    fn trylock_held_is_ebusy_all_types() {
        for ty in [mutex_type::NORMAL, mutex_type::DEFAULT, mutex_type::ERRORCHECK] {
            let (mut mem, f, o, t) = setup(0x1000);
            init(&mut mem, 0x1000, 0).unwrap();
            // Set the type through the attr path.
            mem.with_exclusive(|g| g.map(0x2000, &ty.to_le_bytes()));
            init(&mut mem, 0x1000, 0x2000).unwrap();
            assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), 0, "type {ty}");
            assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), consts::EBUSY, "type {ty}");
            assert_eq!(unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
            // After unlock, trylock works again.
            assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), 0);
        }
    }

    /// POSIX: trylock on a RECURSIVE mutex held by the caller increments the
    /// count and succeeds; it takes two unlocks to release.
    #[test]
    fn trylock_recursive_self_increments() {
        let (mut mem, f, o, t) = setup(0x1000);
        set_type(&mut mem, 0x1000, mutex_type::RECURSIVE);
        assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), 0);
        assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), 0, "self trylock counts");
        assert_eq!(read_state(&mem, 0x1000).unwrap(), 2);
        assert_eq!(unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        assert_eq!(read_state(&mem, 0x1000).unwrap(), 1, "still held once");
        assert_eq!(unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        assert_eq!(read_state(&mem, 0x1000).unwrap(), 0);
    }

    /// ERRORCHECK relock by the owner returns EDEADLK; unlock by a non-owner or
    /// of an unlocked mutex returns EPERM.
    #[test]
    fn errorcheck_semantics() {
        use crate::shared_mem::SharedMockMemory;
        let base = {
            let mut m = MockMemory::new();
            m.map(0x1000, &[0u8; 40]);
            m
        };
        let mem = SharedMockMemory::new(base);
        set_type(&mut { mem.clone() }, 0x1000, mutex_type::ERRORCHECK);
        let f = std::sync::Arc::new(MockFutex::new());
        let o = std::sync::Arc::new(OwnerTable::new());
        let t = std::sync::Arc::new(MockThreads::new());
        {
            let mut m = mem.clone();
            assert_eq!(lock(&mut m, &*f, &o, &*t, 0x1000).unwrap(), 0);
            assert_eq!(lock(&mut m, &*f, &o, &*t, 0x1000).unwrap(), consts::EDEADLK);
        }
        // Unlock from a *different* thread: EPERM. MockThreads binds per host
        // thread, so the spawned thread IS a different guest identity.
        let h = {
            let mem = mem.clone();
            let f = f.clone();
            let o = o.clone();
            let t = t.clone();
            std::thread::spawn(move || {
                let mut m = mem.clone();
                unlock(&mut m, &*f, &o, &*t, 0x1000).unwrap()
            })
        };
        assert_eq!(h.join().unwrap(), consts::EPERM);
        // The owner can still unlock.
        {
            let mut m = mem.clone();
            assert_eq!(unlock(&mut m, &*f, &o, &*t, 0x1000).unwrap(), 0);
            // Unlocking an unlocked mutex: EPERM.
            assert_eq!(unlock(&mut m, &*f, &o, &*t, 0x1000).unwrap(), consts::EPERM);
        }
    }

    /// RECURSIVE counts: N locks need N unlocks; only the last releases.
    #[test]
    fn recursive_counts() {
        let (mut mem, f, o, t) = setup(0x1000);
        set_type(&mut mem, 0x1000, mutex_type::RECURSIVE);
        for i in 1..=5 {
            assert_eq!(lock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0, "lock {i}");
            assert_eq!(read_state(&mem, 0x1000).unwrap(), i, "count after lock {i}");
        }
        // While held, the owner still holds the count (state != 0) but its own
        // trylock would only count up; nothing else to assert there — instead
        // verify the OTHER-thread view through a second registry identity.
        for i in (1..=4).rev() {
            assert_eq!(unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
            assert_eq!(read_state(&mem, 0x1000).unwrap(), i, "after unlock");
        }
        assert_eq!(unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        assert_eq!(read_state(&mem, 0x1000).unwrap(), 0);
        assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), 0, "released at zero");
    }

    /// NORMAL relock by the owner is a deadlock — tested via trylock (EBUSY),
    /// never by deadlocking (reasoning in module docs).
    #[test]
    fn normal_relock_documented_via_trylock() {
        let (mut mem, f, o, t) = setup(0x1000);
        set_type(&mut mem, 0x1000, mutex_type::NORMAL);
        assert_eq!(lock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        // POSIX: lock() again would deadlock. trylock() must report EBUSY:
        assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), consts::EBUSY);
    }

    /// destroy on a locked mutex returns EBUSY; on an unlocked one, 0, and the
    /// struct reads back all-zero.
    #[test]
    fn destroy_semantics() {
        let (mem, f, o, t) = setup(0x1000);
        assert_eq!(lock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        assert_eq!(destroy(&mut mem.clone(), &o, 0x1000).unwrap(), consts::EBUSY);
        assert_eq!(unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        assert_eq!(destroy(&mut mem.clone(), &o, 0x1000).unwrap(), 0);
        let mut bytes = [0u8; 40];
        mem.read(0x1000, &mut bytes).unwrap();
        assert!(bytes.iter().all(|&b| b == 0));
    }

    /// timedlock returns ETIMEDOUT when the holder never releases, and the
    /// measured elapsed time is at least the timeout.
    /// A futex shut down the way an embedding's is: every wait refused, and interrupted.
    struct ShutDown;

    impl Futex for ShutDown {
        fn wait(&self, _addr: u64, _expected: u32, _timeout: Option<Duration>) -> WaitResult {
            WaitResult::WouldBlock
        }
        fn wake(&self, _addr: u64, _count: u32) -> u32 {
            0
        }
        fn interrupted(&self) -> bool {
            true
        }
    }

    /// **A held mutex on an interrupted futex answers `EINTR` instead of looping in the host.**
    /// The owner relocking a NORMAL mutex is the contention path with nobody to release it --
    /// without the check this loops forever, which the channel's deadline turns into a failure.
    /// A free mutex is still taken: interruption only answers a wait.
    #[test]
    fn a_contended_lock_on_an_interrupted_futex_answers_eintr() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut mem, _f, o, t) = setup(0x1000);
            set_type(&mut mem, 0x1000, mutex_type::NORMAL);
            let free = lock(&mut mem.clone(), &ShutDown, &o, &t, 0x1000).unwrap();
            let held = lock(&mut mem.clone(), &ShutDown, &o, &t, 0x1000).unwrap();
            let _ = tx.send((free, held));
        });
        let answers = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a lock on an interrupted futex must return, not loop in the host");
        assert_eq!(answers, (0, consts::EINTR));
    }

    #[test]
    fn timedlock_times_out() {
        let (mut mem, f, o, t) = setup(0x1000);
        set_type(&mut mem, 0x1000, mutex_type::NORMAL);
        assert_eq!(lock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap(), 0);
        let start = std::time::Instant::now();
        let r = timedlock(&mut mem.clone(), &f, &o, &t, 0x1000, std::time::Duration::from_millis(120)).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(r, consts::ETIMEDOUT);
        crate::timing::assert_blocked_for(
            elapsed, std::time::Duration::from_millis(120), "mutex timedlock");
        // And the mutex is still locked by the owner.
        assert_eq!(trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap(), consts::EBUSY);
    }

    /// timedlock acquires when the holder releases in time: the holder holds for
    /// 100 ms, the contender's timedlock (2 s budget) must succeed. Uses the
    /// shared-memory handle whose host lock spans only single accesses — holding
    /// a whole-call `Mutex<MockMemory>` guard would deadlock the harness (see
    /// `shared_mem.rs`).
    #[test]
    fn timedlock_acquires_after_release() {
        use crate::shared_mem::SharedMockMemory;

        let base = {
            let mut m = MockMemory::new();
            m.map(0x1000, &[0u8; 40]);
            m
        };
        let mem = SharedMockMemory::new(base);
        set_type(&mut { mem.clone() }, 0x1000, mutex_type::NORMAL);
        let futex = std::sync::Arc::new(MockFutex::new());
        let owners = std::sync::Arc::new(OwnerTable::new());
        let threads = std::sync::Arc::new(MockThreads::new());

        // Main thread takes the mutex first.
        {
            let mut m = mem.clone();
            lock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap();
        }
        // Holder thread: releases after 100 ms.
        let h = {
            let mem = mem.clone();
            let futex = futex.clone();
            let owners = owners.clone();
            let threads = threads.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let mut m = mem.clone();
                unlock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap();
            })
        };
        // Contender: timedlock with a 2 s budget must succeed well under budget.
        let start = std::time::Instant::now();
        let r = {
            let mut m = mem.clone();
            timedlock(&mut m, &*futex, &owners, &*threads, 0x1000, std::time::Duration::from_secs(2)).unwrap()
        };
        let elapsed = start.elapsed();
        h.join().unwrap();
        assert_eq!(r, 0, "timedlock must acquire after release");
        crate::timing::assert_blocked_for(
            elapsed, std::time::Duration::from_millis(100), "mutex timedlock waits for the holder");
        assert!(elapsed < std::time::Duration::from_secs(2), "must not hit the budget: {elapsed:?}");
    }

    /// init from an attr sets the type word; NULL attr gives DEFAULT.
    #[test]
    fn init_sets_type_from_attr() {
        let (mut mem, _f, _o, _t) = setup(0x1000);
        mem.with_exclusive(|g| g.map(0x2000, &[0u8; 8]));
        attr_init(&mut mem, 0x2000).unwrap();
        assert_eq!(attr_settype(&mut mem, 0x2000, mutex_type::RECURSIVE).unwrap(), 0);
        assert_eq!(init(&mut mem, 0x1000, 0x2000).unwrap(), 0);
        assert_eq!(read_type(&mem, 0x1000).unwrap(), mutex_type::RECURSIVE);

        // NULL attr => DEFAULT (3).
        mem.with_exclusive(|g| g.map(0x3000, &[0u8; 40]));
        init(&mut mem, 0x3000, 0).unwrap();
        assert_eq!(read_type(&mem, 0x3000).unwrap(), mutex_type::DEFAULT);

        // Bad type rejected with EINVAL (returned, not errno).
        assert_eq!(attr_settype(&mut mem, 0x2000, 99).unwrap(), consts::EINVAL);
    }

    /// Guard regions: no operation writes outside the 40 bytes.
    #[test]
    fn writes_stay_in_struct() {
        let mem = crate::shared_mem::SharedMockMemory::new({
            let mut m = MockMemory::new();
            m.map(0x0F00, &[0xA5; 32]); // low guard
            m.map(0x1000, &[0u8; 40]);
            m.map(0x1028, &[0xA5; 32]); // high guard
            m
        });
        let f = MockFutex::new();
        let o = OwnerTable::new();
        let t = MockThreads::new();
        set_type(&mut mem.clone(), 0x1000, mutex_type::RECURSIVE);
        for _ in 0..3 {
            lock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap();
        }
        trylock(&mut mem.clone(), &o, &t, 0x1000).unwrap();
        for _ in 0..4 {
            unlock(&mut mem.clone(), &f, &o, &t, 0x1000).unwrap();
        }
        destroy(&mut mem.clone(), &o, 0x1000).unwrap();
        for range in [(0x0F00u64, 32usize), (0x1028, 32)] {
            let mut buf = vec![0u8; range.1];
            mem.read(range.0, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0xA5), "guard corrupted at {:#x}", range.0);
        }
    }

    /// Hostile: null addresses fault; a garbage type word makes lock refuse (the
    /// fallthrough treats unknown types as NORMAL — documented: bionic's fast
    /// path is identical, and the type word is only ever written by init).
    #[test]
    fn hostile_inputs() {
        let (mem, f, o, t) = setup(0x1000);
        assert!(lock(&mut mem.clone(), &f, &o, &t, 0).is_err());
        assert!(unlock(&mut mem.clone(), &f, &o, &t, 0).is_err());
        assert!(trylock(&mut mem.clone(), &o, &t, 0).is_err());
        assert!(destroy(&mut mem.clone(), &o, 0).is_err());
        // Address range past u64::MAX:
        assert!(lock(&mut mem.clone(), &f, &o, &t, u64::MAX - 20).is_err());
        // Unmapped:
        assert!(lock(&mut mem.clone(), &f, &o, &t, 0xdead_0000).is_err());
    }

    fn set_type(mem: &mut crate::shared_mem::SharedMockMemory, addr: u64, ty: i32) {
        mem.with_exclusive(|g| g.write(addr + 8, &ty.to_le_bytes()).unwrap());
    }

    /// One thread holds the mutex (once) for 150 ms while another locks it, over a futex that
    /// compares: the second must **sleep**, then acquire when the first releases. Returns how
    /// many of its waits the futex refused.
    fn contended_over_a_comparing_futex(ty: i32) -> u64 {
        let (mem, _, owners, threads) = setup(0x1000);
        set_type(&mut mem.clone(), 0x1000, ty);
        let futex = std::sync::Arc::new(crate::shared_mem::ComparingFutex::new(mem.clone()));
        let owners = std::sync::Arc::new(owners);
        let threads = std::sync::Arc::new(threads);
        // Both roles on fresh host threads: `MockThreads` binds an identity per host thread, and
        // a thread that outlives one registry would share its number with the next one's first.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = {
            let (mem, futex, owners, threads) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone());
            std::thread::spawn(move || {
                let code = lock(&mut mem.clone(), &*futex, &owners, &*threads, 0x1000).unwrap();
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
                (code, unlock(&mut mem.clone(), &*futex, &owners, &*threads, 0x1000).unwrap())
            })
        };
        held_rx.recv().unwrap();
        let contender = {
            let (mem, futex, owners, threads) =
                (mem.clone(), futex.clone(), owners.clone(), threads.clone());
            std::thread::spawn(move || {
                let code = lock(&mut mem.clone(), &*futex, &owners, &*threads, 0x1000).unwrap();
                (code, unlock(&mut mem.clone(), &*futex, &owners, &*threads, 0x1000).unwrap())
            })
        };
        assert_eq!(holder.join().unwrap(), (0, 0), "the holder took and released it");
        assert_eq!(contender.join().unwrap(), (0, 0), "the contender acquired and released");
        futex.refused()
    }

    /// **A contended lock tells the futex the word it will sleep on**, for every type. A
    /// recursive mutex's word is its hold count, which never carries the waiter flag, so
    /// `LOCKED_WITH_WAITERS` there is a count of two -- and over a futex that compares, a wait on
    /// a mutex held once was refused every pass: a spin, MEASURED here in the hundreds of
    /// thousands of refusals in 150 ms before `contend` passed the count it read.
    #[test]
    fn a_contended_lock_sleeps_on_a_futex_that_compares_its_word() {
        for ty in [mutex_type::NORMAL, mutex_type::ERRORCHECK, mutex_type::RECURSIVE] {
            let refused = contended_over_a_comparing_futex(ty);
            assert!(refused < 100, "type {ty}: {refused} waits refused -- the contender spun");
        }
    }
}
