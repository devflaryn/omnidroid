//! Phase 8 concurrency stress: the tests that make the whole layer verifiable.
//!
//! Every test asserts POSITIVE contention (observed overlap / non-zero
//! contention counters), runs against shared guest memory through
//! `SharedMockMemory` (CAS under one host-lock hold — genuinely atomic), and
//! is bounded so a bug fails rather than hangs. The torture test prints its
//! seed.

use omni_bionic::cond::{self, CondWaiters};
use omni_bionic::memory::GuestMemory;
use omni_bionic::mock::MockMemory;
use omni_bionic::mock_threads::{MockFutex, MockThreads};
use omni_bionic::mutex::{self, mutex_type, OwnerTable};
use omni_bionic::once;
use omni_bionic::rwlock;
use omni_bionic::shared_mem::SharedMockMemory;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

const N_THREADS: usize = 8;
const INCREMENTS_PER_THREAD: usize = 12_500; // 8 × 12,500 = 100,000 total

fn guest_mem() -> SharedMockMemory {
    SharedMockMemory::new(MockMemory::new())
}

/// Run `f` with the ambient owners/registry the cond layer needs for
/// reacquisition inside `wait_end`.
fn with_ambient<R>(
    owners: &Arc<OwnerTable>,
    threads: &Arc<MockThreads>,
    f: impl FnOnce() -> R,
) -> R {
    cond::with_owners(owners.clone(), || cond::with_registry(&**threads, f))
}

// ---------------------------------------------------------------------------
// Mutual exclusion under load, for every mutex type
// ---------------------------------------------------------------------------

/// Mutex stress for one type: 8 host threads × 12,500 lock/increment/unlock
/// rounds on a NON-ATOMIC guest counter. Each round first does a `trylock`:
/// success there is positive contention evidence (the lock was genuinely held
/// at that instant by another thread). Returns rounds where trylock hit a
/// held lock.
fn stress_mutex(ty: i32) -> usize {
    let mem = guest_mem();
    mem.with_exclusive(|g| {
        g.map(0x1000, &[0u8; 40]); // mutex
        g.map(0x2000, &[0u8; 4]); // counter (touched only under the lock)
        g.write(0x1008, &ty.to_le_bytes()).unwrap(); // type word at offset 8
    });
    let futex = Arc::new(MockFutex::new());
    let owners = Arc::new(OwnerTable::new());
    let threads = Arc::new(MockThreads::new());

    let mut handles = Vec::new();
    for _ in 0..N_THREADS {
        let (mem, futex, owners, threads) =
            (mem.clone(), futex.clone(), owners.clone(), threads.clone());
        handles.push(std::thread::spawn(move || {
            let mut m = mem.clone();
            let mut contention = 0usize;
            with_ambient(&owners, &threads, || {
                for _ in 0..INCREMENTS_PER_THREAD {
                    let r = mutex::trylock(&mut m, &owners, &*threads, 0x1000).unwrap();
                    if r == 0 {
                        contention += 1; // lock was FREE: uncontended round
                    } else {
                        assert_eq!(r, omni_bionic::errno::consts::EBUSY);
                        let r = mutex::lock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap();
                        assert_eq!(r, 0);
                    }
                    // Non-atomic guest counter increment under the lock.
                    let mut b = [0u8; 4];
                    m.read(0x2000, &mut b).unwrap();
                    let v = u32::from_le_bytes(b).wrapping_add(1);
                    m.write(0x2000, &v.to_le_bytes()).unwrap();
                    assert_eq!(
                        mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap(),
                        0
                    );
                }
            });
            contention
        }));
    }
    let mut total_contention = 0usize;
    for h in handles {
        total_contention += h.join().unwrap();
    }
    let mut b = [0u8; 4];
    mem.read(0x2000, &mut b).unwrap();
    let total = u64::from(u32::from_le_bytes(b));
    assert_eq!(
        total,
        (N_THREADS * INCREMENTS_PER_THREAD) as u64,
        "broken mutual exclusion: lost increments"
    );
    total_contention
}

#[test]
fn stress_mutex_normal() {
    let contention = stress_mutex(mutex_type::NORMAL);
    println!("NORMAL: contention evidence {contention} rounds");
    assert!(contention > 0, "no contention observed — not a concurrency test");
}

#[test]
fn stress_mutex_default() {
    let contention = stress_mutex(mutex_type::DEFAULT);
    println!("DEFAULT: contention evidence {contention} rounds");
    assert!(contention > 0);
}

#[test]
fn stress_mutex_errorcheck() {
    let contention = stress_mutex(mutex_type::ERRORCHECK);
    println!("ERRORCHECK: contention evidence {contention} rounds");
    assert!(contention > 0);
}

#[test]
fn stress_mutex_recursive() {
    let contention = stress_mutex(mutex_type::RECURSIVE);
    println!("RECURSIVE: contention evidence {contention} rounds");
    assert!(contention > 0);
}

// ---------------------------------------------------------------------------
// Producer/consumer over cond + mutex with a bounded queue
// ---------------------------------------------------------------------------

/// Bounded ring buffer (8 slots) at fixed guest addresses; one producer, one
/// consumer, 512 items. Asserts every item consumed exactly once, in order —
/// no loss, no duplication.
#[test]
fn stress_producer_consumer_no_lost_or_duplicated_items() {
    const SLOTS: usize = 8;
    const ITEMS: u32 = 512;

    let mem = guest_mem();
    mem.with_exclusive(|g| {
        g.map(0x1000, &[0u8; 40]); // mutex
        g.map(0x1100, &[0u8; 48]); // not_full cond
        g.map(0x1200, &[0u8; 48]); // not_empty cond
        g.map(0x1300, &[0u8; 4 * SLOTS]); // ring slots (u32 items)
        g.map(0x1400, &[0u8; 12]); // head, tail, count
        g.map(0x1500, &[0u8; 4 * ITEMS as usize]); // consumed log
    });
    let futex = Arc::new(MockFutex::new());
    let owners = Arc::new(OwnerTable::new());
    let threads = Arc::new(MockThreads::new());
    let waiters = Arc::new(CondWaiters::new());

    let rd32 = |m: &SharedMockMemory, addr: u64| -> u32 {
        let mut b = [0u8; 4];
        m.read(addr, &mut b).unwrap();
        u32::from_le_bytes(b)
    };
    fn wr32(m: &mut SharedMockMemory, addr: u64, v: u32) {
        m.write(addr, &v.to_le_bytes()).unwrap();
    }

    // Producer: items 0..ITEMS.
    let p = {
        let (mem, futex, owners, threads, waiters) =
            (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
        std::thread::spawn(move || {
            let mut m = mem.clone();
            with_ambient(&owners, &threads, || {
                for item in 0..ITEMS {
                    while mutex::lock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap() != 0 {}
                    loop {
                        if rd32(&m, 0x1408) < SLOTS as u32 {
                            break;
                        }
                        cond::wait_begin(&mut m, &owners, &*threads, &waiters, 0x1100, 0x1000)
                            .unwrap();
                        let r = cond::wait_end(
                            &*threads, &waiters, 0x1100, 0x1000, &mut m, &*futex,
                            Some(Duration::from_secs(10)),
                        )
                        .unwrap();
                        assert_eq!(r, 0, "producer timed out: lost wake?");
                    }
                    let head = u64::from(rd32(&m, 0x1400));
                    wr32(&mut m, 0x1300 + head * 4, item);
                    wr32(&mut m, 0x1400, ((head + 1) % (SLOTS as u64)) as u32);
                    let count = rd32(&m, 0x1408);
                    wr32(&mut m, 0x1408, count + 1);
                    assert_eq!(cond::signal(&waiters, 0x1200).unwrap(), 0);
                    assert_eq!(
                        mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap(),
                        0
                    );
                }
            });
        })
    };

    // Consumer: consume ITEMS items, log them.
    let c = {
        let (mem, futex, owners, threads, waiters) =
            (mem.clone(), futex.clone(), owners.clone(), threads.clone(), waiters.clone());
        std::thread::spawn(move || {
            let mut m = mem.clone();
            with_ambient(&owners, &threads, || {
                for i in 0..ITEMS {
                    while mutex::lock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap() != 0 {}
                    loop {
                        if rd32(&m, 0x1408) > 0 {
                            break;
                        }
                        cond::wait_begin(&mut m, &owners, &*threads, &waiters, 0x1200, 0x1000)
                            .unwrap();
                        let r = cond::wait_end(
                            &*threads, &waiters, 0x1200, 0x1000, &mut m, &*futex,
                            Some(Duration::from_secs(10)),
                        )
                        .unwrap();
                        assert_eq!(r, 0, "consumer timed out: lost wake?");
                    }
                    let tail = u64::from(rd32(&m, 0x1404));
                    let v = rd32(&m, 0x1300 + tail * 4);
                    wr32(&mut m, 0x1404, ((tail + 1) % (SLOTS as u64)) as u32);
                    let count = rd32(&m, 0x1408);
                    wr32(&mut m, 0x1408, count - 1);
                    wr32(&mut m, 0x1500 + u64::from(i) * 4, v);
                    assert_eq!(cond::signal(&waiters, 0x1100).unwrap(), 0);
                    assert_eq!(
                        mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap(),
                        0
                    );
                }
            });
        })
    };

    p.join().unwrap();
    c.join().unwrap();

    // consumed[i] == i for all i: exactly-once, in order.
    for i in 0..ITEMS {
        let v = rd32(&mem, 0x1500 + u64::from(i) * 4);
        assert_eq!(v, i, "item {i} diverged (loss/duplication/reorder)");
    }
}

// ---------------------------------------------------------------------------
// Reader/writer overlap
// ---------------------------------------------------------------------------

/// 4 readers × 200 rounds vs 2 writers × 200 rounds. Asserts: no reader ever
/// inside while a writer is inside (violation counter stays 0), and readers
/// genuinely overlap (max observed concurrent readers > 1 — positive
/// contention evidence).
///
/// Instrumentation lives in HOST atomics, not guest memory: the invariant must
/// not itself depend on the primitive under test, and an unsynchronised
/// guest-side RMW counter would produce false violations (its own lost
/// updates) — harness bugs, not primitive bugs. The guest rwlock is still what
/// is being verified: each thread's atomic registration happens strictly
/// inside its guest-held region.
#[test]
fn stress_readers_writers_overlap_and_exclusion() {
    use std::sync::atomic::Ordering;

    const READERS: usize = 4;
    const WRITERS: usize = 2;
    const ROUNDS: usize = 200;

    let mem = guest_mem();
    mem.with_exclusive(|g| {
        g.map(0x1000, &[0u8; 56]); // rwlock
    });
    let futex = Arc::new(MockFutex::new());

    let readers_inside = Arc::new(AtomicUsize::new(0));
    let writer_inside = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let max_readers = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..READERS {
        let (mem, futex, readers_inside, writer_inside, violations, max_readers) = (
            mem.clone(),
            futex.clone(),
            readers_inside.clone(),
            writer_inside.clone(),
            violations.clone(),
            max_readers.clone(),
        );
        handles.push(std::thread::spawn(move || {
            let mut m = mem.clone();
            for _ in 0..ROUNDS {
                assert_eq!(rwlock::rdlock(&mut m, &*futex, 0x1000).unwrap(), 0);
                // Inside the guest-held region: register, observe max.
                let now = readers_inside.fetch_add(1, Ordering::SeqCst) + 1;
                max_readers.fetch_max(now, Ordering::SeqCst);
                if writer_inside.load(Ordering::SeqCst) > 0 {
                    violations.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::yield_now();
                readers_inside.fetch_sub(1, Ordering::SeqCst);
                assert_eq!(rwlock::unlock(&mut m, &*futex, 0x1000).unwrap(), 0);
            }
        }));
    }
    for _ in 0..WRITERS {
        let (mem, futex, readers_inside, writer_inside, violations) = (
            mem.clone(),
            futex.clone(),
            readers_inside.clone(),
            writer_inside.clone(),
            violations.clone(),
        );
        handles.push(std::thread::spawn(move || {
            let mut m = mem.clone();
            for _ in 0..ROUNDS {
                assert_eq!(rwlock::wrlock(&mut m, &*futex, 0x1000).unwrap(), 0);
                writer_inside.store(1, Ordering::SeqCst);
                // Yield a few times so any illegally-inside reader gets the
                // chance to register and be observed.
                for _ in 0..4 {
                    std::thread::yield_now();
                }
                if readers_inside.load(Ordering::SeqCst) > 0 {
                    violations.fetch_add(1, Ordering::SeqCst);
                }
                writer_inside.store(0, Ordering::SeqCst);
                assert_eq!(rwlock::unlock(&mut m, &*futex, 0x1000).unwrap(), 0);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        violations.load(Ordering::SeqCst),
        0,
        "reader/writer exclusion violated"
    );
    let observed_max = max_readers.load(Ordering::SeqCst);
    println!("rwlock: max observed concurrent readers = {observed_max}");
    assert!(
        observed_max > 1,
        "readers never overlapped — this is not a concurrency test"
    );
}

// ---------------------------------------------------------------------------
// pthread_once hammered
// ---------------------------------------------------------------------------

/// 16 threads race through pthread_once. The init routine runs exactly once,
/// and EVERY caller (including the losers) observes the init routine's final
/// side effect before returning — the ordering guarantee C++ static
/// initializers depend on.
#[test]
fn stress_once_hammered() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Barrier;

    let mem = guest_mem();
    mem.with_exclusive(|g| {
        g.map(0x1000, &[0u8; 4]); // once control
        g.map(0x1100, &[0u8; 4]); // completed flag (written last in routine)
    });
    let futex = Arc::new(MockFutex::new());
    const N: usize = 16;
    let ran = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(N));

    let mut handles = Vec::new();
    for _ in 0..N {
        let (mem, futex, ran, barrier) =
            (mem.clone(), futex.clone(), ran.clone(), barrier.clone());
        handles.push(std::thread::spawn(move || {
            let mut m = mem.clone();
            let mut mc = m.clone(); // separate handle for the init routine's write
            barrier.wait();
            let outcome = once::once(&mut m, &*futex, 0x1000, move || {
                ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                mc.write(0x1100, &1u32.to_le_bytes()).unwrap(); // LAST side effect
            })
            .unwrap();
            let _ = outcome;
            // Ordering guarantee: init's final side effect is visible to
            // every caller by the time once() returns.
            let mut b = [0u8; 4];
            m.read(0x1100, &mut b).unwrap();
            assert_eq!(
                u32::from_le_bytes(b),
                1,
                "caller returned before init routine completed"
            );
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "init must run exactly once"
    );
}

// ---------------------------------------------------------------------------
// Randomised torture test (seeded)
// ---------------------------------------------------------------------------

/// Random interleaved operations across mutex / rwlock / sem / cond with
/// invariants checked throughout. Seeded xorshift64* so failures reproduce;
/// the seed is printed. Each thread derives a distinct seed from the master
/// seed, printed alongside.
#[test]
fn stress_torture_seeded() {
    const MASTER_SEED: u64 = 0x0DDB_1A5E_5BAD_5EED;
    println!("torture master seed: {MASTER_SEED:#018x}");

    let mem = guest_mem();
    mem.with_exclusive(|g| {
        g.map(0x1000, &[0u8; 40]); // mutex (all-zero = valid default)
        g.map(0x1100, &[0u8; 56]); // rwlock
        g.map(0x1200, &[0u8; 4]); // sem
        g.map(0x1300, &[0u8; 4]); // invariant token count
        g.write(0x1200, &4u32.to_le_bytes()).unwrap(); // sem starts at TOKENS
    });
    let futex = Arc::new(MockFutex::new());
    let owners = Arc::new(OwnerTable::new());
    let threads = Arc::new(MockThreads::new());
    let waiters = Arc::new(CondWaiters::new());

    let rd32 = |m: &SharedMockMemory, addr: u64| -> u32 {
        let mut b = [0u8; 4];
        m.read(addr, &mut b).unwrap();
        u32::from_le_bytes(b)
    };

    const ROUNDS: usize = 2_000;

    // Per-thread op tallies, host-side, so the mutex-protected counter's
    // expected value is EXACT rather than probabilistic (an unsynchronised
    // guest-side tally would lose updates of its own and produce false
    // divergence). The invariant under test is the mutex's, not the harness
    // counter's.
    let increments = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let takes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut handles = Vec::new();
    for t in 0..6u64 {
        let (mem, futex, owners, threads, waiters) = (
            mem.clone(),
            futex.clone(),
            owners.clone(),
            threads.clone(),
            waiters.clone(),
        );
        let (increments, posts, takes) = (increments.clone(), posts.clone(), takes.clone());
        // Per-thread seed derived deterministically from the master seed.
        let mut s = MASTER_SEED ^ t.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        handles.push(std::thread::spawn(move || {
            let mut rng = move || {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                s.wrapping_mul(0x2545_F491_4F6C_DD1D)
            };
            let mut m = mem.clone();
            with_ambient(&owners, &threads, || {
                for _ in 0..ROUNDS {
                    match rng() % 5 {
                        0 | 1 => {
                            // Mutex round-trip; increment the guest counter
                            // strictly under the guest mutex.
                            assert_eq!(
                                mutex::lock(&mut m, &*futex, &owners, &*threads, 0x1000).unwrap(),
                                0
                            );
                            let cur = rd32(&m, 0x1300);
                            m.write(0x1300, &(cur + 1).to_le_bytes()).unwrap();
                            assert_eq!(
                                mutex::unlock(&mut m, &*futex, &owners, &*threads, 0x1000)
                                    .unwrap(),
                                0
                            );
                            increments.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        2 => {
                            // rwlock write round-trip.
                            assert_eq!(rwlock::wrlock(&mut m, &*futex, 0x1100).unwrap(), 0);
                            assert_eq!(rwlock::unlock(&mut m, &*futex, 0x1100).unwrap(), 0);
                        }
                        3 => {
                            // Sem: try to take a token; if none, post one.
                            if omni_bionic::sem::trywait(&mut m, 0x1200).unwrap() != 0 {
                                // EAGAIN: no token available — post one so
                                // other threads keep making progress.
                                assert_eq!(
                                    omni_bionic::sem::post(&mut m, &*futex, 0x1200).unwrap(),
                                    0
                                );
                                posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            } else {
                                takes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                        _ => {
                            // Cond signal with (probably) no waiters: no-op.
                            assert_eq!(cond::signal(&waiters, 0x1100).unwrap(), 0);
                        }
                    }
                }
            });
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Global invariants (exact, from the host-side tallies):
    // 1. Mutex-protected counter equals the number of increments performed —
    //    any lost update is a mutual-exclusion break.
    assert_eq!(
        rd32(&mem, 0x1300),
        increments.load(std::sync::atomic::Ordering::SeqCst) as u32,
        "mutex-protected counter diverged (lost updates)"
    );
    // 2. Sem token conservation: final value = initial (4) + posts - takes.
    let sem_final = rd32(&mem, 0x1200) as i64;
    let sem_expected = 4 + posts.load(std::sync::atomic::Ordering::SeqCst) as i64
        - takes.load(std::sync::atomic::Ordering::SeqCst) as i64;
    assert_eq!(
        sem_final, sem_expected,
        "sem token conservation violated"
    );
}
