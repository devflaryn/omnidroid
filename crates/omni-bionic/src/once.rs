//! `pthread_once`: run an initialisation routine exactly once, process-wide, under
//! arbitrary concurrency — with the **ordering guarantee C++ static initialisers
//! depend on**: every other caller does not return until the routine has completed.
//!
//! Guest representation: `pthread_once_t` is 4 bytes ([`crate::layouts::sizes`]),
//! value 0 = never run, 1 = in progress, 2 = done. The all-zero static initializer
//! (`PTHREAD_ONCE_INIT`) is the valid never-run state.
//!
//! Blocking uses the [`Futex`] trait on the `pthread_once_t` address itself, the
//! same protocol bionic uses (futex wait on the control word while the winner runs
//! the routine).

use crate::atomics::GuestAtomic;
use crate::layouts::{offsets, sizes};
use crate::memory::GuestMemory;
use crate::threads::{Futex, WaitResult};
use core::time::Duration;

/// Control-word values inside a guest `pthread_once_t`.
mod state {
    /// Never run (the all-zero static initializer).
    pub const NEVER: u32 = 0;
    /// Initialization in progress.
    pub const IN_PROGRESS: u32 = 1;
    /// Initialization complete.
    pub const DONE: u32 = 2;
}

/// Outcome of a [`once`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnceOutcome {
    /// The calling thread ran the init routine.
    Ran,
    /// Another thread ran (or was running) it; the caller waited for completion.
    AlreadyDone,
}

/// `pthread_once(once_control, init_routine)`.
///
/// `run_init` is invoked by this thread only if it won the race; the adapter turns
/// it into the actual guest call. Every loser blocks on the futex word until the
/// winner stores DONE — *not* merely until it observes a non-NEVER value — so the
/// ordering guarantee holds: no caller returns before the routine has completed.
///
/// The race is resolved by a TRUE atomic CAS (`GuestAtomic::cas_u32`) on the
/// control word: exactly one caller transitions NEVER -> IN_PROGRESS and only
/// that caller runs the routine. (An earlier read-then-write "CAS" here was
/// detection-after-the-fact, not atomicity: two racing threads could both write
/// IN_PROGRESS, both re-read IN_PROGRESS, and both run the init — a real
/// double-run the concurrent stress suite caught.)
///
/// Errors: a guest-memory fault on the 4-byte control word (nothing else can fail;
/// `init_routine` faults are the adapter's to report).
pub fn once<F: FnMut()>(
    mem: &mut (impl GuestMemory + GuestAtomic),
    futex: &impl Futex,
    once_addr: u64,
    mut run_init: F,
) -> Result<OnceOutcome, crate::memory::Fault> {
    check_addr(once_addr)?;

    loop {
        // Read the control word.
        let mut word = [0u8; 4];
        mem.read(once_addr, &mut word)?;
        let current = u32::from_le_bytes(word);

        match current {
            state::DONE => return Ok(OnceOutcome::AlreadyDone),
            state::IN_PROGRESS => {
                // Someone else is running it: block until the word is DONE.
                // The wait is in bounded slices with a word re-check per wake:
                // a wake delivered between our read and the futex registration
                // is then reaped by the NEXT slice's re-check instead of being
                // lost forever (an unbounded sleep here would hang on exactly
                // that lost wake). Correctness is unaffected: we only return
                // when the word IS DONE, never on a mere timeout.
                loop {
                    let r = futex.wait(once_addr, state::IN_PROGRESS, Some(Duration::from_secs(1)));
                    match r {
                        WaitResult::Woken => {}
                        WaitResult::TimedOut => {}
                        WaitResult::WouldBlock => {}
                    }
                    let mut word = [0u8; 4];
                    mem.read(once_addr, &mut word)?;
                    match u32::from_le_bytes(word) {
                        state::DONE => return Ok(OnceOutcome::AlreadyDone),
                        // Still in progress (or raced back through a re-init):
                        // sleep again.
                        _ => continue,
                    }
                }
            }
            state::NEVER => {
                // Win the race with a TRUE atomic CAS: NEVER -> IN_PROGRESS.
                // Exactly one caller can succeed; the loser re-loops and observes
                // IN_PROGRESS (or DONE).
                if mem.cas_u32(once_addr, state::NEVER, state::IN_PROGRESS)? {
                    // We won: run the routine.
                    run_init();
                    // Publish DONE, then wake every waiter on the word.
                    mem.write(once_addr, &state::DONE.to_le_bytes())?;
                    futex.wake(once_addr, u32::MAX);
                    return Ok(OnceOutcome::Ran);
                }
                // Lost the race: loop and observe IN_PROGRESS (or DONE).
                continue;
            }
            // Any other value is a corrupted control word: not a plausible state.
            _ => return Err(crate::memory::Fault(once_addr)),
        }
    }
}



/// Validate the `pthread_once_t` address (4 bytes, no wraparound).
fn check_addr(addr: u64) -> Result<(), crate::memory::Fault> {
    if addr == 0 {
        return Err(crate::memory::Fault(0));
    }
    match addr.checked_add(sizes::PTHREAD_ONCE_T - 1) {
        Some(_) => Ok(()),
        None => Err(crate::memory::Fault(addr)),
    }
}

/// Silence the unused-import warning for `offsets` while keeping the module shape
/// parallel with the other primitive modules (the once state is the whole struct).
#[allow(unused)]
fn _offsets_used() -> u64 {
    offsets::ONCE_WORD
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;
    use crate::mock_threads::MockFutex;
    use crate::shared_mem::SharedMockMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn place(mem: &mut MockMemory, addr: u64) {
        mem.map(addr, &[0u8; 4]); // PTHREAD_ONCE_INIT = all zero
    }

    /// A shared-CAS memory with a mapped 4-byte once control at `addr`.
    fn smem(addr: u64) -> SharedMockMemory {
        SharedMockMemory::new({
            let mut m = MockMemory::new();
            place(&mut m, addr);
            m
        })
    }

    /// The winner runs the routine exactly once; a second caller observes DONE
    /// without running it.
    #[test]
    fn runs_exactly_once_sequential() {
        let mem = smem(0x1000);
        let futex = MockFutex::new();
        let count = AtomicUsize::new(0);
        let mut run = || {
            count.fetch_add(1, Ordering::SeqCst);
        };
        assert_eq!(once(&mut { mem.clone() }, &futex, 0x1000, &mut run).unwrap(), OnceOutcome::Ran);
        assert_eq!(once(&mut { mem.clone() }, &futex, 0x1000, &mut run).unwrap(), OnceOutcome::AlreadyDone);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// Losers must not return before the routine completed: the winner's routine
    /// writes a flag LAST, and losers assert they observe it set when they return.
    #[test]
    fn losers_return_after_completion() {
        let mem = Arc::new(smem(0x1000));
        let futex = Arc::new(MockFutex::new());
        let routine_running = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let mem = mem.clone();
            let futex = futex.clone();
            let running = routine_running.clone();
            let completed = completed.clone();
            handles.push(std::thread::spawn(move || {
                let mut m = (*mem).clone();
                let mut run = || {
                    running.fetch_add(1, Ordering::SeqCst);
                    // Simulate slow init: the winner sleeps INSIDE the routine.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    completed.fetch_add(1, Ordering::SeqCst);
                };
                let r = once(&mut m, &*futex, 0x1000, &mut run).unwrap();
                // By the time ANY caller returns, the routine must have completed.
                assert_eq!(completed.load(Ordering::SeqCst), 1, "ordering guarantee");
                r
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| **r == OnceOutcome::Ran).count(), 1);
        assert_eq!(results.iter().filter(|r| **r == OnceOutcome::AlreadyDone).count(), 7);
        assert_eq!(routine_running.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    /// Under real concurrency (32 threads hammering at once), the routine still
    /// runs exactly once.
    #[test]
    fn concurrent_hammer_runs_once() {
        let mem = Arc::new(smem(0x2000));
        let futex = Arc::new(MockFutex::new());
        let count = Arc::new(AtomicUsize::new(0));

        let barrier = Arc::new(std::sync::Barrier::new(32));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let mem = mem.clone();
            let futex = futex.clone();
            let count = count.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait(); // maximise contention
                let mut m = (*mem).clone();
                let mut run = || {
                    count.fetch_add(1, Ordering::SeqCst);
                };
                once(&mut m, &*futex, 0x2000, &mut run).unwrap()
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(results.iter().filter(|r| **r == OnceOutcome::Ran).count(), 1);
    }

    /// A done-once word is 4 bytes and leaves neighbours alone (guard check).
    #[test]
    fn control_word_stays_in_bounds() {
        let mem = SharedMockMemory::new({
            let mut m = MockMemory::new();
            // Guards around the 4-byte once control.
            m.map(0x0FF0, &[0xA5; 16]);
            m.map(0x1000, &[0u8; 4]);
            m.map(0x1004, &[0xA5; 16]);
            m
        });
        let futex = MockFutex::new();
        once(&mut { mem.clone() }, &futex, 0x1000, || {}).unwrap();
        let mut lo = [0u8; 16];
        mem.read(0x0FF0, &mut lo).unwrap();
        let mut hi = [0u8; 16];
        mem.read(0x1004, &mut hi).unwrap();
        assert!(lo.iter().all(|&b| b == 0xA5) && hi.iter().all(|&b| b == 0xA5));
        let mut word = [0u8; 4];
        mem.read(0x1000, &mut word).unwrap();
        assert_eq!(u32::from_le_bytes(word), state::DONE);
    }

    /// Hostile: a null control address faults at 0; an address that wraps u64
    /// faults at the address.
    #[test]
    fn hostile_addresses() {
        let mem = MockMemory::new();
        let futex = MockFutex::new();
        // Null/wraparound faults are detected before any memory access, so the
        // single-threaded MockMemory (no CAS) is fine here.
        assert_eq!(
            once(&mut SharedMockMemory::new(mem.clone()), &futex, 0, || {})
                .unwrap_err()
                .addr(),
            0
        );
        assert_eq!(
            once(&mut SharedMockMemory::new(mem), &futex, u64::MAX - 2, || {})
                .unwrap_err()
                .addr(),
            u64::MAX - 2
        );
    }

    /// Hostile: a garbage control word (not 0/1/2) is rejected, not treated as a
    /// state — and nothing is run.
    #[test]
    fn garbage_control_word_rejected() {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &7u32.to_le_bytes());
        let futex = MockFutex::new();
        let ran = AtomicUsize::new(0);
        let mut run = || {
            ran.fetch_add(1, Ordering::SeqCst);
        };
        assert!(once(&mut SharedMockMemory::new(mem), &futex, 0x1000, &mut run).is_err());
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    /// An unmapped control word faults at the address.
    #[test]
    fn unmapped_control_word_faults() {
        let mem = MockMemory::new();
        let futex = MockFutex::new();
        let err = once(&mut SharedMockMemory::new(mem), &futex, 0x9999_0000, || {}).unwrap_err();
        assert_eq!(err.addr(), 0x9999_0000);
    }
}
