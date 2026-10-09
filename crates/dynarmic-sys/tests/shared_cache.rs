//! Vendored patch 0022: one code cache shared by every jit of a guest address space
//! (`docs/research/shared-jit-cache.md`, D38).
//!
//! What these establish, each from guest code: that jits on one cache run code another jit
//! translated, and translate each block once between them; that every per-thread value the code
//! reads -- the callbacks' `this`, the budget, the thread pointer, the exclusive monitor slot, the
//! fast-dispatch table -- is the running thread's and not the translating thread's; that an
//! invalidation made through one jit reaches every jit by its next run, including through the
//! return-stack buffer and the fast-dispatch table; that code keeps being right while another
//! thread rewrites and invalidates it; that full regions are retired and given back while
//! threads run -- even while one of them is parked inside a callback; and (patch 0028) that a full
//! region is not a flush: it stays live, and past the live limit the oldest region alone is
//! retired, so only what it held is translated again.
//!
//! x86-64 only: the arm64 backend has no shared cache (`od_code_cache_new` returns null there).
#![cfg(target_arch = "x86_64")]

mod harness;

use dynarmic_sys::*;
use harness::a64;
use harness::{config_for, Vm, VmOptions, CODE_BASE, HALT_DONE, MEM_GUARD, MEM_SIZE};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// `LDAXR Xt, [Xn]`.
const fn ldaxr_x(rt: u32, rn: u32) -> u32 {
    0xC85F_FC00 | (rn << 5) | rt
}
/// `STLXR Ws, Xt, [Xn]`.
const fn stlxr_x(rs: u32, rt: u32, rn: u32) -> u32 {
    0xC800_FC00 | (rs << 16) | (rn << 5) | rt
}
/// `CBNZ Wt, <offset>`.
const fn cbnz_w(rt: u32, offset_insns: i32) -> u32 {
    0x3500_0000 | (((offset_insns as u32) & 0x7_FFFF) << 5) | rt
}

/// One guest address space, as several `Vm`s share it: a data arena, an exclusive monitor, a
/// code cache, and optionally a program other threads may rewrite.
struct Space {
    arena: usize,
    monitor: usize,
    cache: usize,
    code: Arc<Vec<AtomicU32>>,
    opts: VmOptions,
}

// SAFETY: every handle in it is shared by design (the monitor and the cache are built for several
// threads), and it is dropped only after every thread using it has been joined.
unsafe impl Send for Space {}
// SAFETY: as above.
unsafe impl Sync for Space {}

impl Space {
    /// A space whose cache keeps one region live: every region that fills is retired (patch 0028's
    /// oldest-first eviction with nothing older), the cadence most tests here were written for.
    fn new(opts: VmOptions, processors: u64, cache_bytes: u64, region_bytes: u64, code: &[u32]) -> Arc<Self> {
        Self::with_live(opts, processors, cache_bytes, region_bytes, region_bytes, code)
    }

    /// A space whose cache keeps `live_bytes` of regions live (0: all but one).
    fn with_live(opts: VmOptions, processors: u64, cache_bytes: u64, region_bytes: u64, live_bytes: u64, code: &[u32]) -> Arc<Self> {
        let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
        let arena = arena.as_mut_ptr();
        // SAFETY: freed in `Drop`, after every jit using it.
        let monitor = unsafe { od_monitor_new(processors) };
        assert!(!monitor.is_null());
        let opts = VmOptions { shared_arena: arena as usize, shared_monitor: monitor as usize, ..opts };
        let cache = Vm::new_code_cache(&opts, monitor, arena, cache_bytes, region_bytes, live_bytes);
        assert!(!cache.is_null(), "od_code_cache_new refused the configuration");
        let code = Arc::new(code.iter().map(|&w| AtomicU32::new(w)).collect::<Vec<_>>());
        Arc::new(Self { arena: arena as usize, monitor: monitor as usize, cache: cache as usize, code, opts })
    }

    /// A `Vm` of this space, processor `id`, on its shared cache (or, `shared == false`, on a cache
    /// of its own -- the per-thread arrangement, for comparison).
    fn vm(&self, id: u32, shared: bool) -> Vm {
        let vm = Vm::new(
            Vec::new(),
            VmOptions {
                processor_id: id,
                shared_cache: if shared { self.cache } else { 0 },
                ..self.opts
            },
        );
        vm.with_ctx(|c| {
            c.shared_code = self.code.as_ptr() as usize;
            c.shared_code_len = self.code.len();
        });
        vm
    }

    fn stats(&self) -> OdCodeCacheStats {
        let mut s = OdCodeCacheStats::default();
        // SAFETY: the cache is live until `Drop`.
        unsafe { od_code_cache_stats_of(self.cache as *mut c_void, &mut s) };
        s
    }

    fn tables(&self) -> OdCodeCacheTables {
        let mut t = OdCodeCacheTables::default();
        // SAFETY: the cache is live until `Drop`.
        unsafe { od_code_cache_tables_of(self.cache as *mut c_void, &mut t) };
        t
    }

    fn rewrite(&self, index: usize, word: u32) {
        self.code[index].store(word, Ordering::SeqCst);
    }

    fn invalidate(&self, index: usize) {
        // SAFETY: the cache is live; this thread runs no jit of it.
        unsafe { od_code_cache_invalidate_range(self.cache as *mut c_void, CODE_BASE + 4 * index as u64, 4) };
    }

    fn word(&self, addr: u64) -> u64 {
        // SAFETY: `arena` is live for the process (leaked) and `addr` is inside it.
        unsafe { *((self.arena + addr as usize) as *const u64) }
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        // SAFETY: every jit on the cache and the monitor is gone by now (the `Vm`s are dropped with
        // their threads, all joined before the last `Arc` goes).
        unsafe {
            od_code_cache_free(self.cache as *mut c_void);
            od_monitor_free(self.monitor as *mut c_void);
        }
    }
}

const CACHE: u64 = 64 << 20;
const REGION: u64 = 16 << 20;

/// `X0 = sum(1..=X1)` by a loop, then `SVC #0`.
fn sum_loop(n: u16) -> Vec<u32> {
    vec![
        a64::movz(0, 0, 0),
        a64::movz(1, n, 0),
        a64::add_shifted(0, 0, 1, 0, 0), // loop: ADD X0, X0, X1
        a64::subs_imm(1, 1, 1),
        a64::b_cond(a64::cond::NE, -2),
        a64::svc(0),
    ]
}

/// Run `threads` jits of `space` concurrently on the program at `CODE_BASE`; each returns its X0
/// and the instruction words its own `read_code` fetched (i.e. that it translated).
fn run_all(space: &Arc<Space>, threads: u32, shared: bool) -> Vec<(u64, u64, Vec<u32>)> {
    let barrier = Arc::new(Barrier::new(threads as usize));
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let space = Arc::clone(space);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let vm = space.vm(i, shared);
                vm.start(u64::MAX >> 2);
                barrier.wait();
                assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
                (vm.reg(0), vm.stats().read_code, vm.with_ctx(|c| c.svc.clone()))
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().expect("guest thread")).collect()
}

/// **The point of the cache**: eight jits run a program, and between them it is translated once.
/// Each still gets its own answer and its own callbacks (its own `SVC` record). The per-thread
/// arrangement, measured the same way, fetches every instruction eight times.
#[test]
fn eight_jits_on_one_cache_translate_a_program_once_between_them() {
    let program = sum_loop(1000);
    // Baseline: one jit alone, on a fresh cache.
    let alone = Space::new(VmOptions::default(), 8, CACHE, REGION, &program);
    let one = run_all(&alone, 1, true);
    let alone_blocks = alone.stats().blocks_emitted;
    let alone_fetches = one[0].1;
    assert!(alone_blocks >= 2 && alone_fetches >= program.len() as u64, "{alone_blocks} {alone_fetches}");

    let space = Space::new(VmOptions::default(), 8, CACHE, REGION, &program);
    let shared = run_all(&space, 8, true);
    for (x0, _, svc) in &shared {
        assert_eq!(*x0, 500_500, "every jit computes the sum");
        assert_eq!(svc, &vec![0], "every jit's SVC reached its own callbacks, once");
    }
    let stats = space.stats();
    let fetched: u64 = shared.iter().map(|r| r.1).sum();
    assert_eq!(stats.blocks_emitted, alone_blocks, "eight jits emitted what one does: {stats:?}");
    assert_eq!(fetched, alone_fetches, "and fetched the program once between them");
    assert_eq!(stats.attached, 0, "every jit detached when it was freed");

    // Per thread: every jit translates the program itself.
    let per_thread = Space::new(VmOptions::default(), 8, CACHE, REGION, &program);
    let separate = run_all(&per_thread, 8, false);
    let fetched_separately: u64 = separate.iter().map(|r| r.1).sum();
    assert_eq!(fetched_separately, 8 * alone_fetches, "per-thread caches fetch it eight times");
    assert_eq!(per_thread.stats().blocks_emitted, 0, "and emit nothing into the shared cache");
}

/// Attach refuses a jit whose configuration would need different code, and accepts the same one.
#[test]
fn a_jit_that_needs_different_code_is_refused() {
    let space = Space::new(VmOptions::default(), 2, CACHE, REGION, &sum_loop(3));
    let attach = |opts: VmOptions| -> bool {
        let mut tpidr = 0u64;
        let tpidrro = 0u64;
        let cfg = config_for(
            &opts,
            std::ptr::null_mut(),
            &mut tpidr,
            &tpidrro,
            space.monitor as *mut c_void,
            opts.shared_arena as u64,
        );
        // SAFETY: `cfg` is valid; a jit made is freed at once, without running.
        let jit = unsafe { od_jit_new_shared(&cfg, space.cache as *mut c_void) };
        if jit.is_null() {
            return false;
        }
        // SAFETY: as above.
        unsafe { od_jit_free(jit) };
        true
    };
    let base = space.opts;
    assert!(attach(base), "the template's own configuration attaches");
    assert!(attach(VmOptions { processor_id: 1, ..base }), "another processor id attaches");
    assert!(!attach(VmOptions { cycle_counting: !base.cycle_counting, ..base }), "cycle counting");
    assert!(!attach(VmOptions { optimizations: 0x0000_FFF8, ..base }), "optimization flags");
    assert!(!attach(VmOptions { mirror: !base.mirror, ..base }), "fastmem mirroring");
    assert!(!attach(VmOptions { fastmem_exclusive: !base.fastmem_exclusive, ..base }), "inline exclusives");
    assert!(!attach(VmOptions { check_halt_on_memory_access: true, ..base }), "memory-abort checks");
    assert!(!attach(VmOptions { shared_arena: base.shared_arena + 64, ..base }), "a different fastmem base");
    assert!(!attach(VmOptions { processor_id: 2, ..base }), "a processor id the monitor does not have");
}

/// Each thread reads **its own** thread pointer and spends **its own** budget, through code
/// another thread translated: `TPIDR_EL0` and the tick callbacks' `this` come from `JitState`.
#[test]
fn each_jit_reads_its_own_thread_pointer_and_spends_its_own_budget() {
    // MRS X0, TPIDR_EL0 ; loop: ADD X1, X1, #1 ; B loop
    let program = vec![a64::mrs_tpidr_el0(0), a64::add_imm(1, 1, 1), a64::b(-1)];
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 4, CACHE, REGION, &program);
    for round in 0..2 {
        for i in 0..4u32 {
            let mut vm = space.vm(i, true);
            let tp = 0x7000_0000 + u64::from(i) * 0x1000 + round;
            vm.set_tpidr_el0(tp);
            let budget = 10_000 * (u64::from(i) + 1);
            vm.start(budget);
            // SAFETY: `vm.raw()` is live and not executing.
            let hr = unsafe { od_jit_run(vm.raw()) };
            assert_eq!(hr, 0, "the budget ran out, nothing else");
            assert_eq!(vm.reg(0), tp, "jit {i} read its own thread pointer");
            let used = vm.with_ctx(|c| c.ticks_used);
            assert!(
                (budget..budget + 16).contains(&used),
                "jit {i} spent its own budget of {budget}, not another jit's: {used}"
            );
        }
    }
    assert!(space.stats().blocks_emitted <= 3, "translated once: {:?}", space.stats());
}

/// **An invalidation through one jit reaches every jit by its next run** -- and a rewrite that is
/// not invalidated is not seen by any (the translation is shared, so it is stale for all).
///
/// Through the three things that can reach a translation: the block map (a fresh run), a
/// fast-dispatch table entry (`BR`), and the return-stack buffer (`RET`) -- the last two are per
/// thread and must be emptied at the jit's next run, from the cache's generation.
#[test]
fn an_invalidation_through_one_jit_reaches_every_jit_by_its_next_run() {
    // 0: BL 4          (pushes the RSB entry for 1)
    // 1: BR X2         (X2 = 5: a fast-dispatch transfer)
    // 2..3: filler
    // 4: RET           (an RSB hit to 1 on the second pass; X30 = 1)
    // 5: MOVZ X0, #k   <- rewritten
    // 6: SVC #0
    let program = vec![
        a64::bl(4),
        a64::br(2),
        a64::svc(9),
        a64::svc(9),
        a64::ret(30),
        a64::movz(0, 1, 0),
        a64::svc(0),
    ];
    let space = Space::new(VmOptions::default(), 4, CACHE, REGION, &program);
    let vms: Vec<Vm> = (0..4).map(|i| space.vm(i, true)).collect();
    let run = |vm: &Vm| -> u64 {
        vm.start(u64::MAX >> 2);
        vm.set_reg(2, CODE_BASE + 5 * 4);
        vm.with_ctx(|c| c.svc.clear());
        assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
        vm.reg(0)
    };
    // Twice each, so that every jit's RSB and fast-dispatch table hold the old translation.
    for vm in &vms {
        assert_eq!(run(vm), 1);
        assert_eq!(run(vm), 1);
    }
    let generation = space.stats().generation;

    // Rewritten, not invalidated: every jit still runs the shared translation.
    space.rewrite(5, a64::movz(0, 2, 0));
    for vm in &vms {
        assert_eq!(run(vm), 1, "a rewrite nobody invalidated is not seen");
    }

    // Invalidated through jit 0, which is not executing: applied at once, for all.
    // SAFETY: `vms[0]` is live and not executing.
    unsafe { od_jit_invalidate_range(vms[0].raw(), CODE_BASE + 5 * 4, 4) };
    assert!(space.stats().generation > generation, "the invalidation dropped a block");
    for (i, vm) in vms.iter().enumerate() {
        assert_eq!(run(vm), 2, "jit {i} runs the new code at its next run");
        assert_eq!(run(vm), 2, "and keeps doing so");
    }

    // And once more through the cache itself.
    space.rewrite(5, a64::movz(0, 3, 0));
    space.invalidate(5);
    for (i, vm) in vms.iter().enumerate() {
        assert_eq!(run(vm), 3, "jit {i}");
    }
}

/// **Threads keep computing right answers while another thread rewrites and invalidates the code
/// they are running**, and all of them see the final code at their next run.
///
/// Eight jits loop through a block the writer keeps rewriting between two encodings and
/// invalidating (unlinking the loop's link slot under running threads, and dropping blocks they
/// hold in their tables), and the translations that makes fill and retire regions under them;
/// every run must end with one of the two values. Then the writer stops on a third, and every
/// jit's next run returns it.
#[test]
fn threads_stay_right_while_another_rewrites_and_invalidates_their_code() {
    const A: u16 = 0x1111;
    const B: u16 = 0x2222;
    const C: u16 = 0x3333;
    // 0: MOVZ X1, #300 ; 1: MOVZ X0, #A (rewritten) ; 2: SUBS X1, X1, #1 ; 3: B.NE 1 ; 4: SVC #0
    let program = vec![
        a64::movz(1, 300, 0),
        a64::movz(0, A, 0),
        a64::subs_imm(1, 1, 1),
        a64::b_cond(a64::cond::NE, -2),
        a64::svc(0),
    ];
    // Small regions, so that the rewriting also fills and retires them under the runners.
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 8, 40 << 20, 8 << 20, &program);
    let stop = Arc::new(AtomicBool::new(false));
    let runs = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(Barrier::new(9));
    let rewritten = Arc::new(Barrier::new(9));

    let runners: Vec<_> = (0..8u32)
        .map(|i| {
            let space = Arc::clone(&space);
            let stop = Arc::clone(&stop);
            let runs = Arc::clone(&runs);
            let finished = Arc::clone(&finished);
            let rewritten = Arc::clone(&rewritten);
            std::thread::spawn(move || {
                let vm = space.vm(i, true);
                let mut seen = [0u64; 2];
                while !stop.load(Ordering::Relaxed) {
                    vm.start(1_000_000);
                    assert_eq!(vm.run_to_completion(1024) & HALT_DONE, HALT_DONE);
                    match vm.reg(0) as u16 {
                        A => seen[0] += 1,
                        B => seen[1] += 1,
                        other => panic!("jit {i} ended with {other:#x}, neither encoding"),
                    }
                    runs.fetch_add(1, Ordering::Relaxed);
                }
                finished.wait();
                rewritten.wait();
                vm.start(1_000_000);
                assert_eq!(vm.run_to_completion(1024) & HALT_DONE, HALT_DONE);
                assert_eq!(vm.reg(0) as u16, C, "jit {i} runs the final code at its next run");
                seen
            })
        })
        .collect();

    // At least three seconds, and on until a region has been retired under the runners (a slow
    // host emits less per second), for at most sixty.
    let started = Instant::now();
    let mut flips = 0u64;
    while started.elapsed() < Duration::from_secs(3)
        || (space.stats().regions_retired == 0 && started.elapsed() < Duration::from_secs(60))
    {
        space.rewrite(1, a64::movz(0, if flips % 2 == 0 { B } else { A }, 0));
        space.invalidate(1);
        flips += 1;
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    finished.wait();
    space.rewrite(1, a64::movz(0, C, 0));
    space.invalidate(1);
    rewritten.wait();

    let seen: Vec<[u64; 2]> = runners.into_iter().map(|h| h.join().expect("runner")).collect();
    let stats = space.stats();
    println!("flips {flips}, runs {}, seen {seen:?}, {stats:?}", runs.load(Ordering::Relaxed));
    assert!(flips > 100 && runs.load(Ordering::Relaxed) > 100, "the two sides really overlapped");
    assert!(stats.blocks_invalidated > 10, "invalidations really dropped blocks under the runners: {stats:?}");
    assert!(stats.regions_retired >= 1, "and regions were retired under them: {stats:?}");
    assert!(seen.iter().map(|s| s[1]).sum::<u64>() > 0, "the rewritten encoding was really run");
}

/// A chain of `blocks` two-instruction blocks (`ADD X0, X0, #1 ; B +1`), then `SVC #0`: every
/// block a separate translation, so a pass translates `blocks` blocks.
fn chain(blocks: usize) -> Vec<u32> {
    let mut code = Vec::with_capacity(blocks * 2 + 1);
    for _ in 0..blocks {
        code.push(a64::add_imm(0, 0, 1));
        code.push(a64::b(1));
    }
    code.push(a64::svc(0));
    code
}

/// **A full region is retired, and given back, while threads run in it.** Four jits run a chain
/// longer than a region, over and over with the cache cleared between passes, on a cache of four
/// 8 MiB regions; every pass must count the whole chain.
#[test]
fn full_regions_are_retired_and_given_back_while_threads_run() {
    const BLOCKS: usize = 40_000;
    let program = chain(BLOCKS);
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 4, 40 << 20, 8 << 20, &program);
    assert!(space.stats().regions_total >= 3, "{:?}", space.stats());
    for pass in 0..6 {
        let results = run_all(&space, 4, true);
        for (i, (x0, _, _)) in results.iter().enumerate() {
            assert_eq!(*x0, BLOCKS as u64, "pass {pass}, jit {i}");
        }
        // SAFETY: the cache is live and no jit on it is executing.
        unsafe { od_code_cache_clear(space.cache as *mut c_void) };
    }
    let stats = space.stats();
    println!("{stats:?}");
    assert!(stats.regions_retired >= 2, "regions were retired: {stats:?}");
    assert!(stats.regions_reclaimed >= 1, "and given back: {stats:?}");
}

/// **A thread parked inside a callback does not hold a retired region.** Jit P enters an `SVC`
/// that sleeps on the host (an import that waits); meanwhile jit Q fills and retires regions,
/// including the one P will return into. Every retired region must be given back while P is still
/// parked -- the bytes P returns into are kept as a hole -- and P must then finish correctly,
/// leaving through its block's halt test.
///
/// P parks on its *second* pass through the `SVC`, so that its return-stack buffer then holds the
/// translation of the loop body after it -- code in the region Q reuses, reached through a link
/// slot Q's code overwrites. Only the halt raised at retirement keeps P from following that entry
/// when it wakes: its block's tail must leave through the halt test, and its next run must start
/// from an emptied buffer.
#[test]
fn a_thread_parked_in_a_callback_does_not_hold_a_retired_region() {
    const BLOCKS: usize = 40_000;
    const P_WORDS: usize = 7;
    // P at CODE_BASE:
    //   0: MOVZ X0, #5 ; 1: MOVZ X2, #2
    //   2: SVC #1          (the first returns at once, the second sleeps)
    //   3: ADD X0, X0, #1 ; 4: SUBS X2, X2, #1 ; 5: B.NE 2 ; 6: SVC #0
    // Q at CODE_BASE + 4 * P_WORDS: the chain.
    let mut program = vec![
        a64::movz(0, 5, 0),
        a64::movz(2, 2, 0),
        a64::svc(1),
        a64::add_imm(0, 0, 1),
        a64::subs_imm(2, 2, 1),
        a64::b_cond(a64::cond::NE, -3),
        a64::svc(0),
    ];
    assert_eq!(program.len(), P_WORDS);
    program.extend(chain(BLOCKS));
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 2, 40 << 20, 8 << 20, &program);
    let parked = Arc::new(AtomicBool::new(false));

    let p = {
        let space = Arc::clone(&space);
        let parked = Arc::clone(&parked);
        std::thread::spawn(move || {
            let vm = space.vm(0, true);
            vm.with_ctx(|c| {
                c.sleep_on_svc1_us = 4_000_000;
                c.sleep_on_svc1_skip = 1;
                c.halt_on_svc = true;
            });
            vm.start(u64::MAX >> 2);
            parked.store(true, Ordering::SeqCst);
            // `SVC #1` sleeps and returns without halting; `SVC #0` halts. The run that parks must
            // come back -- after the sleep -- through the halt the retirements raised, straight
            // after the `SVC` (the loop's `ADD` not yet run), and not carry on into the retired
            // region's code.
            let first = vm.run();
            let woke = (first, vm.pc(), vm.reg(0));
            // SAFETY: live, not executing.
            unsafe { od_jit_clear_halt(vm.raw(), OD_HALT_CACHE_INVALIDATION) };
            let hr = vm.run_to_completion(64);
            assert_eq!(hr & HALT_DONE, HALT_DONE);
            (vm.reg(0), vm.with_ctx(|c| c.svc.clone()), woke)
        })
    };
    while !parked.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(300));

    let vm = space.vm(1, true);
    let mut pinned_while_parked = Vec::new();
    for _ in 0..4 {
        vm.start(u64::MAX >> 2);
        vm.set_pc(CODE_BASE + 4 * P_WORDS as u64);
        vm.set_reg(0, 0);
        assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
        assert_eq!(vm.reg(0), BLOCKS as u64);
        // SAFETY: the cache is live and this thread is not executing a jit on it.
        unsafe { od_code_cache_clear(space.cache as *mut c_void) };
        pinned_while_parked.push(space.stats().regions_pinned);
    }
    let during = space.stats();
    assert!(!p.is_finished(), "P was still parked through all of Q's passes");
    let (x0, svc, (woke_halt, woke_pc, woke_x0)) = p.join().expect("P");
    assert_ne!(
        woke_halt & OD_HALT_CACHE_INVALIDATION,
        0,
        "P woke and left through its block's halt test (halted {woke_halt:#x})"
    );
    assert_eq!((woke_pc, woke_x0), (CODE_BASE + 3 * 4, 6), "right after the SVC, the loop's ADD not yet run again");
    println!("while parked: {during:?}, pinned after each pass {pinned_while_parked:?}");
    assert!(during.regions_retired >= 2, "Q retired regions while P was parked: {during:?}");
    assert_eq!(
        pinned_while_parked,
        vec![0; pinned_while_parked.len()],
        "every retired region was given back although P was parked in a callback: {during:?}"
    );
    assert_eq!(svc, vec![1, 1, 0]);
    assert_eq!(x0, 7, "P finished its block's tail and the rest of its code correctly");
}

/// **Threads parked all over a region do not fragment it** -- the in-world failure of 2026-09-25
/// (D38 amendment 1). A game world keeps dozens of threads parked inside `SVC` callbacks (imports
/// that wait), each with its return address in whichever region was current when it parked. The
/// first version kept a hole of pages around each such address when the region was given back, and
/// reused the region only if its largest hole-free span was at least 4 MiB: with the parked
/// threads' blocks spread through the region, it came back in small pieces or not at all, and a
/// working set larger than the piece it was given filled it, retired it, forgot every block and
/// started again, for minutes (w24: translation at 120-240k instructions a second, memory flat).
///
/// Twelve threads park, one after each twelfth of a translation that fills and retires a region,
/// and a second fill retires the next. While they are all still parked, every retired region must
/// be given back whole (their resume addresses moved to the prelude); on waking each must finish
/// correctly; and a working set run over and over afterwards must reach a steady state.
#[test]
fn threads_parked_all_over_a_region_do_not_fragment_it() {
    const PARKED: usize = 12;
    const SEGMENT: usize = 12_000; // blocks per segment: twelve segments are more than a region
    const WORKING_SET: usize = 20_000;
    // Segment k: SEGMENT x (ADD X0, X0, #1 ; B +1), then SVC #0. After the segments, the working
    // set (the same shape). After that, park stub i: MOVZ X0, #i ; SVC #1 (sleeps) ; SVC #0.
    let mut program = Vec::new();
    let mut segment_at = Vec::new();
    for _ in 0..PARKED {
        segment_at.push(program.len());
        program.extend(chain(SEGMENT));
    }
    let working_set_at = program.len();
    program.extend(chain(WORKING_SET));
    let mut stub_at = Vec::new();
    for i in 0..PARKED {
        stub_at.push(program.len());
        program.extend([a64::movz(0, i as u16, 0), a64::svc(1), a64::svc(0)]);
    }
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, PARKED as u64 + 1, 40 << 20, 8 << 20, &program);
    let q = space.vm(PARKED as u32, true);
    let run_q = |at: usize| {
        q.start(u64::MAX >> 2);
        q.set_pc(CODE_BASE + 4 * at as u64);
        q.set_reg(0, 0);
        assert_eq!(q.run_to_completion(64) & HALT_DONE, HALT_DONE);
    };

    let mut parked = Vec::new();
    for (k, &segment) in segment_at.iter().enumerate() {
        run_q(segment);
        // P_k translates its stub now -- right after segment k's code -- and parks in it.
        let before = space.stats().blocks_emitted;
        let space_k = Arc::clone(&space);
        let stub = stub_at[k];
        parked.push(std::thread::spawn(move || {
            let vm = space_k.vm(k as u32, true);
            vm.with_ctx(|c| {
                c.sleep_on_svc1_us = 8_000_000;
                c.halt_on_svc = true;
            });
            vm.start(u64::MAX >> 2);
            vm.set_pc(CODE_BASE + 4 * stub as u64);
            assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
            (vm.reg(0), vm.with_ctx(|c| c.svc.clone()))
        }));
        while space.stats().blocks_emitted == before {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // A second fill, so the region the later threads parked in is retired too.
    // SAFETY: the cache is live; this thread is not executing a jit on it.
    unsafe { od_code_cache_clear(space.cache as *mut c_void) };
    for &segment in &segment_at {
        run_q(segment);
    }
    let during = space.stats();
    println!("with {PARKED} threads parked: {during:?}");
    assert!(during.regions_retired >= 2, "the fills retired regions: {during:?}");
    assert_eq!(
        during.regions_pinned, 0,
        "every retired region was given back although {PARKED} threads were parked in them: {during:?}"
    );
    assert!(during.parked_redirected >= PARKED as u64 / 2, "{during:?}");
    assert!(parked.iter().all(|p| !p.is_finished()), "the threads were still parked throughout");

    for (i, p) in parked.into_iter().enumerate() {
        let (x0, svc) = p.join().expect("a parked thread");
        assert_eq!((x0, svc), (i as u64, vec![1, 0]), "parked thread {i} finished correctly");
    }

    // And the working set settles: after it has been translated, running it again translates
    // nothing and retires nothing.
    run_q(working_set_at);
    run_q(working_set_at);
    let settled = space.stats();
    for _ in 0..3 {
        run_q(working_set_at);
        assert_eq!(q.reg(0), WORKING_SET as u64);
    }
    let after = space.stats();
    assert_eq!(
        (after.blocks_emitted, after.regions_retired),
        (settled.blocks_emitted, settled.regions_retired),
        "the working set was translated again: {settled:?} -> {after:?}"
    );
}

/// **A link to an invalidated block is undone**, while the block that links to it stays: jit A
/// runs `B` to a block (linking the two through a slot); jit B invalidates only the target, after
/// its code changed; A's next run enters the kept first block, whose slot must now lead to the
/// dispatcher and the new translation, not to the old one.
#[test]
fn a_link_to_an_invalidated_block_is_undone() {
    // 0: B +2 ; 1: SVC #9 ; 2: MOVZ X0, #k (rewritten) ; 3: SVC #0
    let program = vec![a64::b(2), a64::svc(9), a64::movz(0, 1, 0), a64::svc(0)];
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 2, CACHE, REGION, &program);
    let a = space.vm(0, true);
    let b = space.vm(1, true);
    let run = |vm: &Vm| -> u64 {
        vm.start(1_000_000);
        assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
        vm.reg(0)
    };
    assert_eq!(run(&a), 1);
    assert_eq!(run(&a), 1, "the second run goes through the link");
    let emitted = space.stats().blocks_emitted;
    space.rewrite(2, a64::movz(0, 2, 0));
    // SAFETY: `b` is live and not executing.
    unsafe { od_jit_invalidate_range(b.raw(), CODE_BASE + 2 * 4, 4) };
    assert_eq!(run(&a), 2, "the kept block's link now leads to the new translation");
    assert_eq!(
        space.stats().blocks_emitted,
        emitted + 1,
        "only the target was translated again; the block linking to it was kept"
    );
}

/// **Every block linking to a target is unlinked from it when it goes, however the list of its
/// linkers was edited before** (patch 0025: the slots linking to a target are a list threaded
/// through the link records, from the target's head). Four blocks, translated in order, branch to
/// one target; some of them are dropped -- from the middle of the list, its head, its tail -- and
/// translated again, which puts them back at the head; then the target's code changes. Every one
/// of the four must run the new code: a linker the list lost would still jump to the old
/// translation.
#[test]
fn every_block_linking_to_a_target_is_unlinked_from_it_whatever_was_dropped_before() {
    // 0, 2, 4, 6: ADD X1, X1, #1 ; B -> 8.  8: MOVZ X0, #k (rewritten) ; 9: SVC #0
    let mut program = Vec::new();
    for linker in 0..4 {
        program.push(a64::add_imm(1, 1, 1));
        program.push(a64::b(8 - (2 * linker + 1)));
    }
    program.push(a64::movz(0, 1, 0));
    program.push(a64::svc(0));
    let space = Space::new(VmOptions::default(), 1, CACHE, REGION, &program);
    let vm = space.vm(0, true);
    let run_from = |linker: u64| -> u64 {
        vm.set_pc(CODE_BASE + 8 * linker);
        vm.with_ctx(|c| c.ticks_remaining = u64::MAX);
        assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "from linker {linker}");
        vm.reg(0)
    };
    let drop_linker = |linker: u64| {
        // SAFETY: the cache is live and no jit of it is executing.
        unsafe { od_code_cache_invalidate_range(space.cache as *mut c_void, CODE_BASE + 8 * linker, 8) };
    };
    for (round, dropped) in [&[1u64][..], &[3], &[0], &[2, 1], &[3, 0, 2], &[2], &[0, 3, 1, 2]].into_iter().enumerate() {
        let value = round as u64 + 2;
        for linker in 0..4 {
            assert_eq!(run_from(linker), value - 1, "round {round}: linker {linker} before the change");
        }
        let linked = space.stats().blocks_emitted;
        for linker in 0..4 {
            assert_eq!(run_from(linker), value - 1, "round {round}: linker {linker} again");
        }
        assert_eq!(space.stats().blocks_emitted, linked, "round {round}: the second pass ran through the links");
        for &linker in dropped {
            drop_linker(linker);
        }
        // Translated again: back at the head of the target's list.
        for &linker in dropped {
            assert_eq!(run_from(linker), value - 1, "round {round}: dropped linker {linker}, translated again");
        }
        space.rewrite(8, a64::movz(0, value as u16, 0));
        space.invalidate(8);
        for linker in 0..4 {
            assert_eq!(run_from(linker), value, "round {round}: linker {linker} after the target changed");
        }
    }
}

/// **A translation made while its code is invalidated is not published.** A jit translating a
/// block outside the cache's lock reads the old word; before it emits, another thread rewrites the
/// word and invalidates it. The translation must be made again (under the lock) and the run -- and
/// every later run -- must execute the new code.
#[test]
fn a_translation_overtaken_by_an_invalidation_is_made_again() {
    use harness::FetchPause;
    // 0: MOVZ X0, #k (rewritten while it is being translated) ; 1: SVC #0
    let program = vec![a64::movz(0, 1, 0), a64::svc(0)];
    let space = Space::new(VmOptions::default(), 1, CACHE, REGION, &program);
    let pause = Arc::new(FetchPause::default());
    pause.address.store(CODE_BASE, Ordering::SeqCst);
    let runner = {
        let space = Arc::clone(&space);
        let pause = Arc::clone(&pause);
        std::thread::spawn(move || {
            let vm = space.vm(0, true);
            vm.with_ctx(|c| c.pause_fetch = Arc::as_ptr(&pause) as usize);
            let mut seen = Vec::new();
            for _ in 0..2 {
                vm.start(u64::MAX >> 2);
                assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
                seen.push(vm.reg(0));
            }
            seen
        })
    };
    while !pause.paused.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    // The runner has read `MOVZ X0, #1` and is holding it. Change it and invalidate.
    space.rewrite(0, a64::movz(0, 2, 0));
    space.invalidate(0);
    pause.release.store(true, Ordering::SeqCst);
    let seen = runner.join().expect("runner");
    let stats = space.stats();
    assert_eq!(seen, vec![2, 2], "the old word's translation was published: {stats:?}");
    assert!(stats.translations_redone >= 1, "{stats:?}");
}

/// **Exclusives through shared code lose no update**, under both monitors: the monitor slot is the
/// running thread's (`JitState`), and under the global monitor the store's scan -- emitted once
/// for every thread -- clears every matching reservation, the storing processor's own included.
fn contended_on_one_cache(optimizations: u32) {
    const THREADS: u32 = 4;
    const ITERATIONS: u64 = 40_000;
    const ADDR: u64 = 0x4000;
    // retry: LDAXR X1, [X4] ; ADD X1, X1, #1 ; STLXR W2, X1, [X4] ; CBNZ W2, retry ;
    //        SUBS X5, X5, #1 ; B.NE retry ; SVC #0
    let program = vec![
        ldaxr_x(1, 4),
        a64::add_imm(1, 1, 1),
        stlxr_x(2, 1, 4),
        cbnz_w(2, -3),
        a64::subs_imm(5, 5, 1),
        a64::b_cond(a64::cond::NE, -5),
        a64::svc(0),
    ];
    let opts = VmOptions { fastmem_exclusive: true, cycle_counting: false, optimizations, ..VmOptions::default() };
    let space = Space::new(opts, u64::from(THREADS), CACHE, REGION, &program);
    let barrier = Arc::new(Barrier::new(THREADS as usize));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let space = Arc::clone(&space);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let vm = space.vm(i, true);
                vm.set_reg(4, ADDR);
                vm.set_reg(5, ITERATIONS);
                vm.start(u64::MAX >> 1);
                barrier.wait();
                assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
                vm.stats().slow_path_total
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().expect("thread"), 0, "the inline path throughout");
    }
    assert_eq!(space.word(ADDR), ITERATIONS * u64::from(THREADS), "lost updates");
    assert!(space.stats().blocks_emitted <= 8, "one translation for all: {:?}", space.stats());
}

/// **A resume that hits the return-stack buffer at `Run`'s entry sees an invalidation.** When a
/// run stops at an `SVC` and the host resumes after it -- how every exit-path import returns --
/// `Run` starts by checking the RSB, whose top is the `SVC`'s own return address, and jumps
/// straight to the code pointer stored there. After another jit invalidated that code, the entry
/// must have been emptied before that jump.
#[test]
fn a_resume_through_the_return_stack_buffer_sees_an_invalidation() {
    // 0: SVC #5 (halts) ; 1: MOVZ X0, #k (rewritten) ; 2: SVC #0 (halts)
    let program = vec![a64::svc(5), a64::movz(0, 1, 0), a64::svc(0)];
    let space = Space::new(VmOptions::default(), 2, CACHE, REGION, &program);
    let a = space.vm(0, true);
    let b = space.vm(1, true);
    // Start at 0, stop at the SVC; then resume after it, as a host does.
    let stop_then_resume = |vm: &Vm| -> u64 {
        vm.start(u64::MAX >> 2);
        assert_eq!(vm.run() & HALT_DONE, HALT_DONE, "stopped at SVC #5");
        // SAFETY: `vm.raw()` is live and not executing.
        unsafe { od_jit_clear_halt(vm.raw(), HALT_DONE) };
        assert_eq!(vm.pc(), CODE_BASE + 4, "the SVC left the PC after itself");
        assert_eq!(vm.run() & HALT_DONE, HALT_DONE, "stopped at SVC #0");
        // SAFETY: as above.
        unsafe { od_jit_clear_halt(vm.raw(), HALT_DONE) };
        vm.reg(0)
    };
    assert_eq!(stop_then_resume(&a), 1);
    // The second time, the SVC's RSB entry holds the translation of word 1.
    assert_eq!(stop_then_resume(&a), 1);
    a.start(u64::MAX >> 2);
    assert_eq!(a.run() & HALT_DONE, HALT_DONE, "stopped at SVC #5, the RSB entry pushed");
    // SAFETY: `a.raw()` is live and not executing.
    unsafe { od_jit_clear_halt(a.raw(), HALT_DONE) };
    space.rewrite(1, a64::movz(0, 2, 0));
    // SAFETY: `b` is live and not executing.
    unsafe { od_jit_invalidate_range(b.raw(), CODE_BASE + 4, 4) };
    assert_eq!(a.run() & HALT_DONE, HALT_DONE, "stopped at SVC #0");
    assert_eq!(a.reg(0), 2, "the resume ran the new code, not the RSB's stale entry");
}

/// **Under the global monitor, another processor's store-exclusive clears a reservation -- through
/// code translated once for both.** A (processor 0) reserves by single-stepping its `LDXR`; B
/// (processor 1), on the same cache, reserves and stores the same value; A's store-exclusive must
/// fail. The store's reservation scan is emitted once for every thread, so it cannot skip "its own"
/// slot by processor id at emit time; it scans every slot, as dynarmic's `CheckAndClear` does.
#[test]
fn another_processor_s_store_clears_a_reservation_through_shared_code() {
    const ADDR: u64 = 0x4000;
    // LDAXR X1, [X4] ; STLXR W2, X1, [X4] ; SVC #0
    let pair = vec![ldaxr_x(1, 4), stlxr_x(2, 1, 4), a64::svc(0)];
    let opts = VmOptions { fastmem_exclusive: true, optimizations: optimization::ALL_SAFE, ..VmOptions::default() };
    let space = Space::new(opts, 2, CACHE, REGION, &pair);
    let a = space.vm(0, true);
    let b = space.vm(1, true);
    a.with_ctx(|c| c.write_u64(ADDR, 0x42));
    for vm in [&a, &b] {
        vm.set_reg(4, ADDR);
        vm.start(1_000_000);
    }
    a.step();
    assert_eq!((a.pc(), a.reg(1)), (CODE_BASE + 4, 0x42), "A reserved, one step");
    assert_eq!(b.run_to_completion(64) & HALT_DONE, HALT_DONE, "B reserved and stored");
    assert_eq!(b.reg(2), 0, "B's store-exclusive succeeded");
    assert_eq!(a.run_to_completion(64) & HALT_DONE, HALT_DONE, "A tried to store");
    assert_eq!(a.reg(2), 1, "B's store cleared A's reservation, so A's store-exclusive failed");
}

#[test]
fn exclusives_through_shared_code_lose_no_update_under_the_global_monitor() {
    contended_on_one_cache(optimization::ALL_SAFE);
}

#[test]
fn exclusives_through_shared_code_lose_no_update_under_value_compare() {
    contended_on_one_cache(optimization::ALL_SAFE | optimization::UNSAFE_IGNORE_GLOBAL_MONITOR);
}

/// **An indirect call whose fast-dispatch entry collides with another location's reaches its own
/// target.** A thread's fast-dispatch table (4,096 entries) is also what its dispatcher consults
/// before the cache's lock; the emitted handler, on a miss, writes the location into the entry
/// *before* it looks the block up, so an entry can hold this location with another location's
/// code pointer while the lookup runs. `BLR` to 8,192 distinct functions, each adding its own
/// index, overfills the table: every collision is such a miss, and the sum says whether every call
/// reached its own function. Run twice, on one jit and then on another, both on a shared cache.
#[test]
fn indirect_calls_that_collide_in_the_fast_dispatch_table_reach_their_own_targets() {
    const N: u64 = 8192;
    const TABLE: u64 = 0x8000;
    // 0: MOVZ X0,#0 ; 1: MOVZ X5,#0 ; 2: MOVZ X3,#TABLE ; 3: MOVZ X7,#N
    // 4 (loop): ADD X6, X3, X5, LSL #3 ; 5: LDR X2, [X6] ; 6: BLR X2 ; 7: ADD X5, X5, #1
    // 8: SUBS X7, X7, #1 ; 9: B.NE loop ; 10: SVC #0
    // 11 + 3i: MOVZ X1, #i ; ADD X0, X0, X1 ; RET
    let mut program = vec![
        a64::movz(0, 0, 0),
        a64::movz(5, 0, 0),
        a64::movz(3, TABLE as u16, 0),
        a64::movz(7, N as u16, 0),
        a64::add_shifted(6, 3, 5, 0, 3),
        a64::ldr_imm(2, 6, 0),
        a64::blr(2),
        a64::add_imm(5, 5, 1),
        a64::subs_imm(7, 7, 1),
        a64::b_cond(a64::cond::NE, -5),
        a64::svc(0),
    ];
    let first = program.len() as u64;
    for i in 0..N {
        program.extend([a64::movz(1, i as u16, 0), a64::add_shifted(0, 0, 1, 0, 0), a64::ret(30)]);
    }
    let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 2, CACHE, REGION, &program);
    for i in 0..N {
        let target = CODE_BASE + 4 * (first + 3 * i);
        // SAFETY: the arena is live (leaked) and the table fits inside it.
        unsafe { *((space.arena + (TABLE + 8 * i) as usize) as *mut u64) = target };
    }
    let expected = N * (N - 1) / 2;
    for (id, shared) in [(0u32, false), (0, true), (1, true)] {
        let vm = space.vm(id, shared);
        for pass in 0..2 {
            // A bounded budget (a pass is ~60k instructions), with cycle counting on: a call that
            // reaches another location's code may loop, and must fail here rather than wedge.
            vm.start(10_000_000);
            let hr = vm.run_to_completion(1024);
            assert_eq!(
                hr & HALT_DONE,
                HALT_DONE,
                "jit {id} (shared {shared}), pass {pass}: halted {hr:#x}, pc {:#x}, exceptions {:?}",
                vm.pc(),
                vm.with_ctx(|c| c.exceptions.clone())
            );
            assert_eq!(vm.reg(0), expected, "jit {id} (shared {shared}), pass {pass}: a call reached another location's code");
        }
    }
}

/// **A thread finds what it has already looked up without the cache's lock**: every run starts
/// with a dispatcher lookup, and eight threads taking one lock for each would contend on it (the
/// benchmark in `omni-cpu`'s `roblox.rs` measured 0.2 -> 0.56 us per call before the dispatcher
/// consulted the thread's own fast-dispatch table). After a first run, a thousand more take none.
#[test]
fn a_thread_finds_what_it_has_looked_up_without_the_lock() {
    let program = vec![a64::movz(0, 1, 0), a64::svc(0)];
    let space = Space::new(VmOptions::default(), 1, CACHE, REGION, &program);
    let vm = space.vm(0, true);
    let run = || {
        vm.set_pc(CODE_BASE);
        assert_eq!(vm.run() & HALT_DONE, HALT_DONE);
        // SAFETY: live, not executing.
        unsafe { od_jit_clear_halt(vm.raw(), HALT_DONE) };
    };
    run();
    let before = space.stats().locked_lookups;
    for _ in 0..1000 {
        run();
    }
    assert_eq!(space.stats().locked_lookups, before, "{:?}", space.stats());
}

/// Measurement: the cost of entering and leaving a run, eight threads at once, per-thread caches
/// against one shared cache. Every run is `MOVZ X0, #1 ; SVC #0` (halts), i.e. nothing but the
/// run boundary and one dispatcher lookup.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_a_run_on_eight_threads() {
    const RUNS: usize = 20_000;
    let program = vec![a64::movz(0, 1, 0), a64::svc(0)];
    for shared in [false, true, false, true] {
        let space = Space::new(VmOptions { cycle_counting: true, ..VmOptions::default() }, 8, CACHE, REGION, &program);
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8u32)
            .map(|i| {
                let space = Arc::clone(&space);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let vm = space.vm(i, shared);
                    vm.start(u64::MAX >> 2);
                    vm.run();
                    barrier.wait();
                    let t = Instant::now();
                    for _ in 0..RUNS {
                        vm.set_pc(CODE_BASE);
                        let hr = vm.run();
                        assert_eq!(hr & HALT_DONE, HALT_DONE);
                        // SAFETY: live, not executing.
                        unsafe { od_jit_clear_halt(vm.raw(), HALT_DONE) };
                    }
                    t.elapsed().as_secs_f64() * 1e9 / RUNS as f64
                })
            })
            .collect();
        let ns: Vec<f64> = handles.into_iter().map(|h| h.join().expect("t")).collect();
        println!("shared {shared}: ns per run per thread {ns:.0?}");
    }
}

/// The program of the eviction tests: a working set `W` (a chain of `hot` blocks) at `CODE_BASE`,
/// then `segments` cold chains of `cold` blocks each, every one translated once. Returns the program
/// and the word index of each cold segment.
fn working_set_and_cold_code(hot: usize, segments: usize, cold: usize) -> (Vec<u32>, Vec<usize>) {
    let mut program = chain(hot);
    let mut at = Vec::with_capacity(segments);
    for _ in 0..segments {
        at.push(program.len());
        program.extend(chain(cold));
    }
    (program, at)
}

/// Run the chain at word `at` on `vm` from the start, and check it counted `blocks`.
fn run_chain(vm: &Vm, at: usize, blocks: usize) {
    vm.start(u64::MAX >> 2);
    vm.set_pc(CODE_BASE + 4 * at as u64);
    vm.set_reg(0, 0);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), blocks as u64, "the chain at word {at}");
}

/// **A full region is not a flush; the oldest region goes first, and only what it held is
/// translated again** (patch 0028). Before it, a region that filled forgot every block of the cache
/// and every thread translated its working set again (a 30-minute session in a world would have
/// done that each time a 256 MiB region filled). A working set `W` is translated first, then cold
/// code fills region after region, `W` run again after each cold segment:
///
/// 1. While regions are free, a fill moves on to the next one and forgets nothing: `W` is never
///    translated again and nothing is retired (the flush translated all of `W` at the first fill).
/// 2. At the live limit the oldest region -- the one `W` was translated into -- is retired, and it
///    alone: at most a region's blocks forgotten, and `W` (all of it was there) translated again,
///    once, into the newest region; the re-translations counted are `W`'s.
/// 3. From then on `W` is translated again only when the region it went into has become the oldest,
///    at most once per `live - 1` fills: cold code, not the working set, is what is evicted.
/// 4. Committed code stays within the live limit and a region.
#[test]
fn a_full_region_is_not_a_flush_and_the_oldest_region_goes_first() {
    const HOT: usize = 3_000;
    const COLD: usize = 25_000;
    const SEGMENTS: usize = 56;
    const REGION: u64 = 8 << 20;
    const LIVE: u64 = 4 * REGION;
    let (program, cold_at) = working_set_and_cold_code(HOT, SEGMENTS, COLD);
    let space = Space::with_live(VmOptions { cycle_counting: true, ..VmOptions::default() }, 1, 96 << 20, REGION, LIVE, &program);
    let vm = space.vm(0, true);
    let start = space.stats();
    assert_eq!(start.regions_live_max, LIVE / REGION, "{start:?}");
    assert!(start.regions_total > start.regions_live_max, "{start:?}");

    run_chain(&vm, 0, HOT);
    let w_blocks = space.stats().blocks_emitted - start.blocks_emitted;
    assert!(w_blocks as usize >= HOT, "{w_blocks}");
    let bytes_per_block = (space.stats().code_bytes_emitted / w_blocks).max(1);
    let blocks_per_region = REGION / bytes_per_block;

    let mut w_translated_again = 0u64; // runs of W that translated anything
    let mut fills_without_eviction = 0u64;
    let mut max_committed = 0u64;
    let mut evictions_seen = 0u64;
    for (k, &at) in cold_at.iter().enumerate() {
        let before = space.stats();
        run_chain(&vm, at, COLD);
        let mid = space.stats();
        run_chain(&vm, 0, HOT);
        let after = space.stats();
        max_committed = max_committed.max(after.committed_bytes);
        let w_emitted = after.blocks_emitted - mid.blocks_emitted;
        let evicted_now = after.regions_evicted - before.regions_evicted;
        if mid.regions_live > before.regions_live && evicted_now == 0 {
            fills_without_eviction += 1;
        }
        if after.regions_evicted == 0 {
            // (1) Regions filled with room to spare: nothing forgotten, nothing retired.
            assert_eq!(w_emitted, 0, "segment {k}: a fill with free regions translated W again: {after:?}");
            assert_eq!(after.regions_retired, 0, "{after:?}");
            continue;
        }
        if evicted_now > 0 {
            evictions_seen += evicted_now;
            // (2) One region's blocks at most.
            let forgot = after.blocks_evicted - before.blocks_evicted;
            assert!(
                forgot <= evicted_now * (blocks_per_region + blocks_per_region / 10),
                "segment {k}: an eviction forgot {forgot} blocks, a region holds about {blocks_per_region}: {after:?}"
            );
        }
        if w_emitted != 0 {
            // W is contiguous, in one region: translated again whole, or not at all.
            assert_eq!(w_emitted, w_blocks, "segment {k}: {after:?}");
            assert_eq!(
                after.blocks_reemitted - mid.blocks_reemitted,
                w_blocks,
                "segment {k}: counted as blocks the eviction forgot: {after:?}"
            );
            w_translated_again += 1;
        }
    }
    let end = space.stats();
    println!(
        "W {w_blocks} blocks ({bytes_per_block} B each, ~{blocks_per_region} a region); translated again {w_translated_again} times; \
         {fills_without_eviction} fills without an eviction; committed at most {} MiB; evict max {} us; {end:?}",
        max_committed >> 20,
        end.evict_max_ns / 1000
    );
    assert!(fills_without_eviction >= LIVE / REGION - 1, "the first fills moved on without evicting: {end:?}");
    assert!(end.regions_evicted >= 2 * (LIVE / REGION), "the test evicted region after region: {end:?}");
    assert_eq!(evictions_seen, end.regions_evicted, "{end:?}");
    // (3) The flush translated W again at every fill; eviction does it once per `live - 1` fills.
    assert!(w_translated_again >= 1, "W's region was evicted and W translated again: {end:?}");
    assert!(
        w_translated_again <= 1 + end.regions_evicted / (LIVE / REGION - 1),
        "W translated again {w_translated_again} times over {} evictions: {end:?}",
        end.regions_evicted
    );
    // (4) The live regions, one retired and not yet given back, and the prelude.
    assert!(max_committed <= LIVE + REGION + (4 << 20), "committed {max_committed}: {end:?}");
    assert_eq!(end.regions_live, LIVE / REGION, "{end:?}");

    // A clear forgets every block: every region, holding none now, is given back at once (no jit
    // is running) -- the one being filled too (patch 0030) -- and only the prelude stays.
    // SAFETY: the cache is live and its one jit is not executing.
    unsafe { od_code_cache_clear(space.cache as *mut c_void) };
    let cleared = space.stats();
    assert_eq!((cleared.regions_live, cleared.regions_pinned), (0, 0), "{cleared:?}");
    assert!(cleared.committed_bytes <= 4 << 20, "{cleared:?}");
    run_chain(&vm, 0, HOT);
}

/// **A clear gives back the region being filled** (patch 0030). A cache whose code fits in one
/// region -- a service that translated its start and then waits -- kept all of it committed
/// through a clear, since only full regions were retired: the idle process's translations were
/// forgotten but not given back. Now the clear retires that region too, and the next block starts
/// a fresh one, committed as it fills.
#[test]
fn a_clear_gives_back_the_region_being_filled() {
    const BLOCKS: usize = 60_000;
    let program = chain(BLOCKS);
    let space = Space::new(VmOptions::default(), 2, CACHE, REGION, &program);
    let vm = space.vm(0, true);
    vm.start(u64::MAX >> 2);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), BLOCKS as u64);
    let filled = space.stats();
    let map = space.tables().blocks;
    assert_eq!(filled.regions_live, 1, "the chain fits in one region: {filled:?}");
    assert!(filled.committed_bytes >= 4 << 20, "and committed several MiB of it: {filled:?}");

    // SAFETY: the cache is live and its one jit is not executing.
    unsafe { od_code_cache_clear(space.cache as *mut c_void) };
    let cleared = space.stats();
    assert_eq!((cleared.regions_live, cleared.regions_pinned), (0, 0), "{cleared:?}");
    assert!(cleared.committed_bytes <= 4 << 20, "given back, the prelude kept: {cleared:?}");
    assert!(cleared.committed_bytes + (4 << 20) <= filled.committed_bytes, "{filled:?} -> {cleared:?}");
    // Patch 0031: and the block map is a new one, not the emptied bucket array of 60,000 blocks.
    let emptied = space.tables().blocks;
    assert!(map.entries >= BLOCKS as u64 && map.bytes >= 1 << 20, "{map:?}");
    assert_eq!(emptied.entries, 0);
    assert!(emptied.bytes <= 64 << 10, "the map given back: {map:?} -> {emptied:?}");

    // The same jit runs on: its next blocks start a fresh region.
    vm.set_reg(0, 0);
    vm.start(u64::MAX >> 2);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), BLOCKS as u64, "the program runs as before");
    let again = space.stats();
    assert_eq!(again.regions_live, 1, "{again:?}");
}

/// **A block translated again elsewhere survives its old region's eviction, and nothing links into
/// an evicted region** (patch 0028). Block `k` of the working set is rewritten and invalidated after
/// its region has filled, so its new translation goes into a newer region -- and links to the next
/// block, still in the old one. Evicting the old region must forget the rest of the working set but
/// not the new block `k`, and must undo the new block's link into the old region (following it would
/// run code that is being given back). A range invalidation after the evictions still finds the
/// working set's blocks through the guest-range index the evictions trimmed.
#[test]
fn a_block_translated_again_elsewhere_survives_its_old_region_s_eviction() {
    const HOT: usize = 2_000;
    const COLD: usize = 25_000;
    const SEGMENTS: usize = 40;
    const REGION: u64 = 8 << 20;
    const LIVE: u64 = 2 * REGION;
    const K: usize = 700;
    let (program, cold_at) = working_set_and_cold_code(HOT, SEGMENTS, COLD);
    let space = Space::with_live(VmOptions { cycle_counting: true, ..VmOptions::default() }, 1, 64 << 20, REGION, LIVE, &program);
    let vm = space.vm(0, true);
    run_chain(&vm, 0, HOT);
    let w_blocks = space.stats().blocks_emitted;

    // Fill W's region with cold code, so the cache moves on to the next one.
    let mut next = 0;
    while space.stats().regions_live < 2 {
        run_chain(&vm, cold_at[next], COLD);
        next += 1;
    }
    assert_eq!(space.stats().regions_evicted, 0);
    // Block K counts 2 now; its new translation goes into the region being filled.
    space.rewrite(2 * K, a64::add_imm(0, 0, 2));
    space.invalidate(2 * K);
    let before = space.stats();
    vm.start(u64::MAX >> 2);
    vm.set_pc(CODE_BASE);
    vm.set_reg(0, 0);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), HOT as u64 + 1);
    assert_eq!(space.stats().blocks_emitted - before.blocks_emitted, 1, "only block K was translated again");

    // Evict W's old region.
    while space.stats().regions_evicted == 0 {
        run_chain(&vm, cold_at[next], COLD);
        next += 1;
    }
    let evicted = space.stats();
    vm.start(u64::MAX >> 2);
    vm.set_pc(CODE_BASE);
    vm.set_reg(0, 0);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    let after = space.stats();
    assert_eq!(vm.reg(0), HOT as u64 + 1, "W ran whole, through block K's link to the old region undone");
    assert_eq!(
        after.blocks_emitted - evicted.blocks_emitted,
        w_blocks - 1,
        "all of W was translated again but block K, whose newer translation stayed: {after:?}"
    );

    // More evictions (each trimming the ranges and link records), W run again after each, then an
    // invalidation of W's last word: the trimmed index still finds that block, and only it.
    let target = after.regions_evicted + 3;
    while space.stats().regions_evicted < target {
        run_chain(&vm, cold_at[next], COLD);
        next += 1;
        vm.start(u64::MAX >> 2);
        vm.set_pc(CODE_BASE);
        vm.set_reg(0, 0);
        assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
        assert_eq!(vm.reg(0), HOT as u64 + 1);
    }
    let before = space.stats();
    space.rewrite(2 * (HOT - 1), a64::add_imm(0, 0, 5));
    space.invalidate(2 * (HOT - 1));
    vm.start(u64::MAX >> 2);
    vm.set_pc(CODE_BASE);
    vm.set_reg(0, 0);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    let last = space.stats();
    assert_eq!(vm.reg(0), HOT as u64 + 1 + 4, "the invalidation reached W's last block: {last:?}");
    assert_eq!(
        (last.blocks_invalidated - before.blocks_invalidated, last.blocks_emitted - before.blocks_emitted),
        (1, 1),
        "that block, and only it, was dropped and translated again: {last:?}"
    );
    println!("{last:?}");
}

/// **Evictions under running threads.** Four threads run the working set over and over while a
/// fifth streams cold code through a cache whose live limit it passes many times: every run of the
/// working set must count right, the working set must be translated again only when its region has
/// been evicted (not at every fill), and every retired region must be given back.
#[test]
fn threads_keep_their_working_set_while_another_streams_cold_code_through_the_cache() {
    const HOT: usize = 2_000;
    const COLD: usize = 25_000;
    const SEGMENTS: usize = 48;
    const REGION: u64 = 8 << 20;
    const LIVE: u64 = 3 * REGION;
    const RUNNERS: u32 = 4;
    let (program, cold_at) = working_set_and_cold_code(HOT, SEGMENTS, COLD);
    let space = Space::with_live(
        VmOptions { cycle_counting: true, ..VmOptions::default() },
        u64::from(RUNNERS) + 1,
        64 << 20,
        REGION,
        LIVE,
        &program,
    );
    let q = space.vm(RUNNERS, true);
    run_chain(&q, 0, HOT);
    let w_blocks = space.stats().blocks_emitted;
    let stop = Arc::new(AtomicBool::new(false));
    let runners: Vec<_> = (0..RUNNERS)
        .map(|i| {
            let space = Arc::clone(&space);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let vm = space.vm(i, true);
                let mut runs = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    run_chain(&vm, 0, HOT);
                    runs += 1;
                }
                runs
            })
        })
        .collect();
    let mut cold_blocks = 0u64;
    for &at in &cold_at {
        // Each segment is translated once, by Q (a chain of `COLD` blocks and its `SVC`'s);
        // whatever else is emitted meanwhile is the working set, translated again.
        run_chain(&q, at, COLD);
        cold_blocks += COLD as u64 + 1;
    }
    stop.store(true, Ordering::Relaxed);
    let runs: Vec<u64> = runners.into_iter().map(|h| h.join().expect("runner")).collect();
    // The last retirement is given back once every runner has left its run: by a thread leaving a
    // run (at most every 10 ms) or the next emission.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        run_chain(&q, 0, HOT);
        if space.stats().regions_pinned == 0 || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let end = space.stats();
    let w_again = end.blocks_emitted - w_blocks - cold_blocks;
    println!("runs {runs:?}, W {w_blocks} blocks translated again {w_again}, {end:?}");
    assert!(runs.iter().all(|&r| r > 10), "the runners ran throughout: {runs:?}");
    assert!(end.regions_evicted >= 6, "the cold code passed the live limit many times: {end:?}");
    // W is translated again when its region is evicted: once per `live - 1` fills at most, plus
    // the pass that finds it evicted when the test ends. The flush did it at every fill.
    let bound = w_blocks * (2 + end.regions_evicted / (LIVE / REGION - 1));
    assert!(w_again <= bound, "W translated again {w_again} blocks, bound {bound}: {end:?}");
    assert_eq!(end.regions_pinned, 0, "every retired region was given back: {end:?}");
}

/// Measurement: what an eviction costs, holding the cache's lock, on blocks of the engine's shape
/// (`LDR ; STR ; B.cond` then `B`, as `shared_bookkeeping.rs` builds them: fastmem sites and link
/// slots in every block). One thread translates a chain far longer than the live limit, once. Rows:
/// 16 MiB regions with 128 MiB live (amendment 3's default; 256 since amendment 4), with 32 MiB live
/// (what depends on the live size), 8 MiB regions with 128 MiB live, and one 128 MiB region live --
/// every block of a full region forgotten at once, as patch 0022's flush did (through the
/// eviction's path, not its own).
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_an_eviction() {
    const UNITS: usize = 600_000;
    let mut program = Vec::with_capacity(UNITS * 4 + 1);
    for _ in 0..UNITS {
        program.push(a64::ldr_imm(3, 2, 0));
        program.push(a64::str_imm(3, 2, 8));
        program.push(a64::b_cond(a64::cond::EQ, 2));
        program.push(a64::b(1));
    }
    program.push(a64::svc(0));
    let rows = [(16u64 << 20, 128u64 << 20), (16 << 20, 32 << 20), (8 << 20, 128 << 20), (128 << 20, 128 << 20)];
    for (region, live) in rows {
        let space = Space::with_live(VmOptions { cycle_counting: true, ..VmOptions::default() }, 1, 512 << 20, region, live, &program);
        let vm = space.vm(0, true);
        vm.start(u64::MAX >> 2);
        vm.set_pc(CODE_BASE);
        let t = Instant::now();
        assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
        let wall = t.elapsed();
        let s = space.stats();
        println!(
            "region {} MiB, live {} MiB: {} blocks ({} B each), {} evictions forgetting {} blocks, {:.2} ms each on \
             average, longest {:.2} ms; emission {:.2} us a block; committed {} MiB; {:.1} s",
            region >> 20,
            live >> 20,
            s.blocks_emitted,
            s.code_bytes_emitted / s.blocks_emitted.max(1),
            s.regions_evicted,
            s.blocks_evicted,
            s.evict_ns as f64 / s.regions_evicted.max(1) as f64 / 1e6,
            s.evict_max_ns as f64 / 1e6,
            s.emit_ns as f64 / s.blocks_emitted.max(1) as f64 / 1e3,
            s.committed_bytes >> 20,
            wall.as_secs_f64()
        );
    }
}

/// **An eviction on demand gives back the oldest regions and nothing else** (patch 0050,
/// `od_code_cache_evict_to`): a cache holding a hot working set W (translated first, so in the
/// oldest region) and cold code after it, evicted down to one region, keeps only the region being
/// filled; W runs right after, translated again once, and the cold code is gone. Prints what the
/// eviction and W's retranslation cost.
#[test]
fn an_eviction_on_demand_keeps_the_newest_region_and_w_runs_on() {
    const HOT: usize = 3_000;
    const COLD: usize = 25_000;
    const SEGMENTS: usize = 14;
    const REGION: u64 = 8 << 20;
    let (program, cold_at) = working_set_and_cold_code(HOT, SEGMENTS, COLD);
    // No live limit to speak of (all but one region): only the eviction on demand retires.
    let space = Space::with_live(VmOptions { cycle_counting: true, ..VmOptions::default() }, 1, 96 << 20, REGION, 0, &program);
    let vm = space.vm(0, true);
    run_chain(&vm, 0, HOT);
    let w_blocks = space.stats().blocks_emitted;
    for &at in &cold_at {
        run_chain(&vm, at, COLD);
    }
    let filled = space.stats();
    assert!(filled.regions_live >= 3, "cold code over several regions: {filled:?}");
    assert_eq!(filled.regions_evicted, 0, "{filled:?}");

    let t = Instant::now();
    // SAFETY: the cache is live and its one jit is not executing.
    let evicted = unsafe { od_code_cache_evict_to(space.cache as *mut c_void, REGION) };
    let took = t.elapsed();
    let after = space.stats();
    assert_eq!(evicted, filled.regions_live - 1, "{filled:?} -> {after:?}");
    assert_eq!(after.regions_live, 1, "the region being filled stays: {after:?}");
    assert!(after.committed_bytes + (evicted - 1) * REGION <= filled.committed_bytes, "given back: {filled:?} -> {after:?}");
    // SAFETY: as above. Asking again changes nothing.
    assert_eq!(unsafe { od_code_cache_evict_to(space.cache as *mut c_void, REGION) }, 0);

    // W runs on: translated again, once.
    let t2 = Instant::now();
    run_chain(&vm, 0, HOT);
    let again = t2.elapsed();
    let rerun = space.stats();
    assert_eq!(rerun.blocks_emitted - after.blocks_emitted, w_blocks, "W translated again whole: {rerun:?}");
    run_chain(&vm, 0, HOT);
    assert_eq!(space.stats().blocks_emitted, rerun.blocks_emitted, "and only once");
    eprintln!(
        "evicted {evicted} regions ({} blocks) in {:.1} ms; W's {w_blocks} blocks translated again in {:.1} ms ({:.1} us a block)",
        after.blocks_evicted - filled.blocks_evicted,
        took.as_secs_f64() * 1e3,
        again.as_secs_f64() * 1e3,
        again.as_secs_f64() * 1e6 / w_blocks as f64
    );
}
