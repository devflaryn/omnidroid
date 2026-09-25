//! Vendored patch 0022: one code cache shared by every jit of a guest address space
//! (`docs/research/shared-jit-cache.md`, D38).
//!
//! What these establish, each from guest code: that jits on one cache run code another jit
//! translated, and translate each block once between them; that every per-thread value the code
//! reads -- the callbacks' `this`, the budget, the thread pointer, the exclusive monitor slot, the
//! fast-dispatch table -- is the running thread's and not the translating thread's; that an
//! invalidation made through one jit reaches every jit by its next run, including through the
//! return-stack buffer and the fast-dispatch table; that code keeps being right while another
//! thread rewrites and invalidates it; and that full regions are retired and given back while
//! threads run -- even while one of them is parked inside a callback.
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
    fn new(opts: VmOptions, processors: u64, cache_bytes: u64, region_bytes: u64, code: &[u32]) -> Arc<Self> {
        let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
        let arena = arena.as_mut_ptr();
        // SAFETY: freed in `Drop`, after every jit using it.
        let monitor = unsafe { od_monitor_new(processors) };
        assert!(!monitor.is_null());
        let opts = VmOptions { shared_arena: arena as usize, shared_monitor: monitor as usize, ..opts };
        let cache = Vm::new_code_cache(&opts, monitor, arena, cache_bytes, region_bytes);
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

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut flips = 0u64;
    while Instant::now() < deadline {
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
            // `SVC #1` sleeps and returns without halting; `SVC #0` halts.
            let hr = vm.run_to_completion(64);
            assert_eq!(hr & HALT_DONE, HALT_DONE);
            (vm.reg(0), vm.with_ctx(|c| c.svc.clone()))
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
    let (x0, svc) = p.join().expect("P");
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
