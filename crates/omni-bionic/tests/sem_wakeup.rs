//! Regression: a `sem_post` must reach a blocked `sem_wait` directly, even when
//! more than one waiter is parked on the semaphore.
//!
//! ## The defect these tests pin
//!
//! `sem_t` is one 32-bit word: bit 31 is the "waiters present" flag and the low 31
//! bits are the count. `post` used to compute its next word as
//! `(word & !WAITERS) + 1` -- under a comment reading "increment, keep flag state",
//! which is the opposite of what it did. `wait` and `trywait` cleared the flag the
//! same way on a successful decrement.
//!
//! So the FIRST post consumed the flag. With two threads parked, the second post
//! then saw no flag, skipped its futex wake, and the second waiter slept until the
//! bounded re-check fired. MEASURED before the fix: waiter latency **1.0104 s**
//! against a 50 ms post interval (n=1 run, reproduced on every run of this test).
//! After the fix the same waiter is woken directly.
//!
//! The pre-existing suite could not see this: every `sem_wait` loops on a bounded
//! timed slice, so a lost wake always *eventually* healed and every assertion about
//! tokens and counts still held. It exercised the wake path without detecting that
//! the wake never happened -- which is why these tests assert on LATENCY and on the
//! flag itself, not on the final count.

use omni_bionic::GuestMemory;
use omni_bionic::mock::MockMemory;
use omni_bionic::mock_threads::MockFutex;
use omni_bionic::sem;
use omni_bionic::shared_mem::SharedMockMemory;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn setup() -> (SharedMockMemory, Arc<MockFutex>) {
    let mem = SharedMockMemory::new(MockMemory::new());
    mem.with_exclusive(|g| {
        g.map(0x1000, &[0u8; 4]); // sem_t, value 0
    });
    (mem, Arc::new(MockFutex::new()))
}

/// Two waiters on a zero semaphore, then two posts. Report each waiter's latency.
#[test]
fn two_waiters_two_posts_latency() {
    let (mem, futex) = setup();
    let mut handles = Vec::new();
    for id in 0..2 {
        let (mem, futex) = (mem.clone(), futex.clone());
        handles.push(std::thread::spawn(move || {
            let mut m = mem.clone();
            let start = Instant::now();
            let r = sem::wait(&mut m, &*futex, 0x1000).unwrap();
            (id, r, start.elapsed())
        }));
    }
    // Let both waiters genuinely block and register the WAITERS flag.
    std::thread::sleep(Duration::from_millis(200));

    let mut m = mem.clone();
    assert_eq!(sem::post(&mut m, &*futex, 0x1000).unwrap(), 0);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(sem::post(&mut m, &*futex, 0x1000).unwrap(), 0);

    let mut worst = Duration::ZERO;
    for h in handles {
        let (id, r, el) = h.join().unwrap();
        println!("waiter {id}: rc={r} latency={:?}", el);
        assert_eq!(r, 0);
        if el > worst { worst = el; }
    }
    println!("WORST WAITER LATENCY = {worst:?}");
    assert!(
        worst < Duration::from_millis(500),
        "a posted token took {worst:?} to reach a blocked waiter — \
         lost wakeup healed only by the 1000 ms bounded re-check"
    );
}

/// Does post() preserve the WAITERS flag while a waiter is still blocked?
#[test]
fn post_preserves_waiters_flag_while_a_waiter_remains() {
    let (mem, futex) = setup();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let (mem, futex) = (mem.clone(), futex.clone());
        handles.push(std::thread::spawn(move || {
            let mut m = mem.clone();
            sem::wait(&mut m, &*futex, 0x1000).unwrap()
        }));
    }
    std::thread::sleep(Duration::from_millis(200));

    let mut m = mem.clone();
    let before = { let mut b = [0u8; 4]; m.read(0x1000, &mut b).unwrap(); u32::from_le_bytes(b) };
    println!("word with 2 blocked waiters = {before:#010x}");
    assert_ne!(before & 0x8000_0000, 0, "WAITERS flag should be set with 2 blocked waiters");

    sem::post(&mut m, &*futex, 0x1000).unwrap();
    let after = { let mut b = [0u8; 4]; m.read(0x1000, &mut b).unwrap(); u32::from_le_bytes(b) };
    println!("word after ONE post (one waiter still blocked) = {after:#010x}");

    // Release the second waiter so the test cannot hang.
    sem::post(&mut m, &*futex, 0x1000).unwrap();
    for h in handles { assert_eq!(h.join().unwrap(), 0); }

    assert_ne!(
        after & 0x8000_0000, 0,
        "post() cleared the WAITERS flag while a waiter was still blocked — \
         the next post will skip its futex wake"
    );
}
