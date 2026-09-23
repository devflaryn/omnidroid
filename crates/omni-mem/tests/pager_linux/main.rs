//! The demand pager on Linux, where it runs inside a `SIGSEGV` handler: does it fit the alternate
//! stack it will be given, and does it serve faults on threads that have one.
//!
//! `omni-platform`'s Linux fault handler is installed `SA_ONSTACK`, so on a thread with an alternate
//! signal stack the whole pager path -- the dispatch, `DemandPager::handle_fault`, `admit`, the
//! space's lock, `ensure_committed`, the vm ledger, and the `mmap` -- runs on that stack. Rust gives
//! every `std::thread` one of `max(SIGSTKSZ, AT_MINSIGSTKSZ)` bytes. If the path needed more, the
//! first demand fault on such a thread would run off the end into the guard page, fault again with
//! `SIGSEGV` blocked, and the process would die. So the need is **measured**: an alternate stack is
//! painted with a pattern, a real fault is served on it, and the untouched part is counted.
//!
//! A directory target so that `tests/windows_only.rs`, which accounts for every `tests/*.rs` by
//! name, does not see it.
#![cfg(target_os = "linux")]

use std::sync::Arc;

use omni_mem::{CommitPolicy, DemandPager, GuestSpace, GuestSpaceConfig, Placement, Protection};

const PAINT: u8 = 0xA5;
const MIB: usize = 1024 * 1024;

/// The current thread's alternate signal stack: `(size, enabled)`.
fn current_altstack() -> (usize, bool) {
    // SAFETY: plain data, filled in by a query-only sigaltstack.
    unsafe {
        let mut old: libc::stack_t = core::mem::zeroed();
        assert_eq!(libc::sigaltstack(core::ptr::null(), &mut old), 0, "sigaltstack query");
        (old.ss_size, old.ss_flags & libc::SS_DISABLE == 0)
    }
}

/// Serve one real demand fault per address on a painted alternate stack of `size` bytes, on a fresh
/// thread, and return the most bytes of it any of them used (kernel signal frame included).
fn stack_used_by_faults_at(addresses: Vec<usize>, size: usize) -> usize {
    std::thread::spawn(move || {
        let mut stack = vec![PAINT; size];
        // SAFETY: plain data; `stack` outlives every use below, and the previous alternate stack is
        // put back before it is dropped.
        let previous = unsafe {
            let new = libc::stack_t {
                ss_sp: stack.as_mut_ptr().cast(),
                ss_flags: 0,
                ss_size: size,
            };
            let mut previous: libc::stack_t = core::mem::zeroed();
            assert_eq!(libc::sigaltstack(&new, &mut previous), 0, "install the painted stack");
            previous
        };
        for &address in &addresses {
            // SAFETY: inside a live lazily-committed guest mapping; the demand pager commits it.
            unsafe { core::ptr::read_volatile(address as *const u8) };
        }
        // SAFETY: restores what this thread had, before `stack` is freed.
        unsafe {
            assert_eq!(libc::sigaltstack(&previous, core::ptr::null_mut()), 0, "restore");
        }
        // The stack grows down from the top, so the used part is everything above the first byte
        // that is no longer the paint.
        let untouched = stack.iter().take_while(|&&byte| byte == PAINT).count();
        size - untouched
    })
    .join()
    .expect("the painted-stack thread")
}

#[test]
fn the_demand_pager_path_fits_the_alternate_stack_rust_gives_a_thread() {
    let space = Arc::new(
        GuestSpace::with_config(GuestSpaceConfig { size: 256 * MIB, ..GuestSpaceConfig::default() })
            .expect("a guest space"),
    );
    let pager = DemandPager::install(Arc::clone(&space)).expect("a demand pager");
    let granule = space.commit_granule();
    let lazy = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            64 * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazy mapping");
    // Fragment the map first, so the region lookups and placeholder splits the faults take are not
    // the cheapest possible ones: every other granule is already committed.
    for index in (0..64).step_by(2) {
        space.ensure_committed(lazy + index * granule, 1).expect("commit");
    }

    // What Rust gave this (libtest) thread and gives every std thread.
    let (std_size, std_enabled) = std::thread::spawn(current_altstack).join().expect("query");
    assert!(std_enabled, "a std thread must have an alternate signal stack in a Rust binary");

    const PAINTED: usize = 256 * 1024;
    let addresses: Vec<usize> = (1..64).step_by(2).map(|index| lazy + index * granule + 8).collect();
    let before = pager.stats();
    let used = stack_used_by_faults_at(addresses.clone(), PAINTED);
    let after = pager.stats();
    assert_eq!(
        after.resolved - before.resolved,
        addresses.len() as u64,
        "every fault must have been served by the pager, on the painted stack"
    );
    println!(
        "alternate stack used by the demand pager's SIGSEGV path: {used} bytes (the most any of \
         n = {} faults used, kernel signal frame included; painted-stack method); a std thread's \
         alternate stack is {std_size} bytes",
        addresses.len()
    );
    assert!(used > 0, "the painted stack was never used: the fault did not run on it");
    // A margin of a quarter of the std stack, for paths this run did not take (an error being built,
    // a panic being caught). The measured figure is the one to read; this is what makes it binding.
    assert!(
        used + std_size / 4 <= std_size,
        "the pager path used {used} bytes of alternate stack, too close to the {std_size} bytes a \
         std thread has: a demand fault on such a thread would overrun it"
    );
}

/// And the real thing: a std thread, with the alternate stack std gave it, serves demand faults.
/// A path that overran it would kill this process (a fault inside the handler, with `SIGSEGV`
/// blocked, is fatal), so a pass is the evidence.
#[test]
fn std_threads_serve_demand_faults_on_their_own_alternate_stacks() {
    let space = Arc::new(
        GuestSpace::with_config(GuestSpaceConfig { size: 256 * MIB, ..GuestSpaceConfig::default() })
            .expect("a guest space"),
    );
    let pager = DemandPager::install(Arc::clone(&space)).expect("a demand pager");
    let granule = space.commit_granule();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 8;
    let lazy = space
        .map_anonymous(
            Placement::Anywhere { align: granule },
            THREADS * PER_THREAD * granule,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazy mapping");
    let before = pager.stats();
    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            std::thread::spawn(move || {
                assert!(current_altstack().1, "a std thread has an alternate stack");
                for index in 0..PER_THREAD {
                    let address = lazy + (thread * PER_THREAD + index) * granule;
                    // SAFETY: inside the lazy mapping; the pager commits it.
                    unsafe { core::ptr::write_volatile(address as *mut u8, thread as u8) };
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("a worker");
    }
    let after = pager.stats();
    assert_eq!(after.resolved - before.resolved, (THREADS * PER_THREAD) as u64);
    assert!(after.is_consistent(), "{after:?}");
    assert_eq!(after.reentered, 0, "the pager must never fault on itself");
}
