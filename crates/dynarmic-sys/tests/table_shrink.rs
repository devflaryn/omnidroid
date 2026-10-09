//! Vendored patch 0066 (`od_set_shrink_tables`): a shared cache's maps shrink with what they hold.
//!
//! A robin_map never gives its bucket array back, so a cache whose blocks an eviction (code aging,
//! the live limit) or an invalidation forgot kept the block map, the link heads and the guest-range
//! page index of its busiest moment. With the switch on, each is rehashed down after the blocks
//! are forgotten when it holds at most half of what its array could; with it off (the default)
//! nothing changes. Either way the code keeps running right: links, the dispatcher's lookups and
//! invalidations through the rehashed maps.
//!
//! One test, its two halves in turn: the switch is process-wide.
#![cfg(target_arch = "x86_64")]

mod harness;

use dynarmic_sys::*;
use harness::a64;
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE, MEM_GUARD, MEM_SIZE};
use std::ffi::c_void;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

const REGION: u64 = 8 << 20;
const HOT: usize = 3_000;
const COLD: usize = 25_000;
const SEGMENTS: usize = 14;

/// A chain of `blocks` two-instruction blocks (`ADD X0, X0, #1 ; B +1`), then `SVC #0`.
fn chain(blocks: usize) -> Vec<u32> {
    let mut code = Vec::with_capacity(blocks * 2 + 1);
    for _ in 0..blocks {
        code.push(a64::add_imm(0, 0, 1));
        code.push(a64::b(1));
    }
    code.push(a64::svc(0));
    code
}

/// One guest address space on a shared cache with no live limit to speak of.
struct Space {
    monitor: *mut c_void,
    cache: *mut c_void,
    code: Arc<Vec<AtomicU32>>,
    opts: VmOptions,
}

impl Space {
    fn new(program: &[u32]) -> Self {
        let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
        let arena = arena.as_mut_ptr();
        // SAFETY: freed in `Drop`, after the jit.
        let monitor = unsafe { od_monitor_new(1) };
        assert!(!monitor.is_null());
        let opts = VmOptions { shared_arena: arena as usize, shared_monitor: monitor as usize, cycle_counting: true, ..VmOptions::default() };
        let cache = Vm::new_code_cache(&opts, monitor, arena, 96 << 20, REGION, 0);
        assert!(!cache.is_null(), "od_code_cache_new refused the configuration");
        let code = Arc::new(program.iter().map(|&w| AtomicU32::new(w)).collect::<Vec<_>>());
        Self { monitor, cache, code, opts }
    }

    fn vm(&self) -> Vm {
        let vm = Vm::new(Vec::new(), VmOptions { shared_cache: self.cache as usize, ..self.opts });
        vm.with_ctx(|c| {
            c.shared_code = self.code.as_ptr() as usize;
            c.shared_code_len = self.code.len();
        });
        vm
    }

    fn tables(&self) -> OdCodeCacheTables {
        let mut t = OdCodeCacheTables::default();
        // SAFETY: the cache is live until `Drop`.
        unsafe { od_code_cache_tables_of(self.cache, &mut t) };
        t
    }

    fn stats(&self) -> OdCodeCacheStats {
        let mut s = OdCodeCacheStats::default();
        // SAFETY: the cache is live until `Drop`.
        unsafe { od_code_cache_stats_of(self.cache, &mut s) };
        s
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        // SAFETY: every jit on the cache is gone (each test drops its `Vm` first).
        unsafe {
            od_code_cache_free(self.cache);
            od_monitor_free(self.monitor);
        }
    }
}

fn run_chain(vm: &Vm, at: usize, blocks: usize) {
    vm.start(u64::MAX >> 2);
    vm.set_pc(CODE_BASE + 4 * at as u64);
    vm.set_reg(0, 0);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), blocks as u64, "the chain at word {at}");
}

/// The program: W (`HOT` blocks) at `CODE_BASE`, then `SEGMENTS` cold chains of `COLD` blocks.
fn program() -> (Vec<u32>, Vec<usize>) {
    let mut program = chain(HOT);
    let mut at = Vec::new();
    for _ in 0..SEGMENTS {
        at.push(program.len());
        program.extend(chain(COLD));
    }
    (program, at)
}

/// Fill a cache with W and the cold code, then evict it down to one region: the tables before and
/// after, and the cache (with its jit) for more.
fn fill_and_evict() -> (Space, Vm, OdCodeCacheTables, OdCodeCacheTables) {
    let (program, cold_at) = program();
    let space = Space::new(&program);
    let vm = space.vm();
    run_chain(&vm, 0, HOT);
    for &at in &cold_at {
        run_chain(&vm, at, COLD);
    }
    let full = space.tables();
    assert!(space.stats().regions_live >= 3, "cold code over several regions: {:?}", space.stats());
    // SAFETY: the cache is live and its one jit is not executing.
    let evicted = unsafe { od_code_cache_evict_to(space.cache, REGION) };
    assert!(evicted >= 2, "{:?}", space.stats());
    let after = space.tables();
    (space, vm, full, after)
}

#[test]
fn the_maps_shrink_after_an_eviction_only_when_asked_and_the_code_runs_on() {
    let bucket_bytes = |t: &OdCodeCacheTables| t.blocks.bytes + t.link_targets.bytes;

    // Off (the default): the block map keeps its array through the eviction.
    // SAFETY: stores one process-wide atomic.
    assert_eq!(unsafe { od_set_shrink_tables(0) }, 0);
    let (space, vm, full, after) = fill_and_evict();
    assert!(after.blocks.entries * 2 < full.blocks.entries, "most blocks forgotten: {} -> {}", full.blocks.entries, after.blocks.entries);
    assert_eq!(after.blocks.bytes, full.blocks.bytes, "off: the block map's array stays");
    eprintln!(
        "off: {} -> {} blocks, block map {} KiB, link heads {} -> {} KiB, page index {} -> {} KiB",
        full.blocks.entries,
        after.blocks.entries,
        after.blocks.bytes >> 10,
        full.link_targets.bytes >> 10,
        after.link_targets.bytes >> 10,
        full.guest_ranges.bytes >> 10,
        after.guest_ranges.bytes >> 10
    );
    drop(vm);
    drop(space);

    // On: the same eviction gives the arrays back.
    // SAFETY: as above.
    assert_eq!(unsafe { od_set_shrink_tables(1) }, 1);
    let (space, vm, full, after) = fill_and_evict();
    eprintln!(
        "on: {} -> {} blocks, block map {} -> {} KiB, link heads {} -> {} KiB, page index {} -> {} KiB",
        full.blocks.entries,
        after.blocks.entries,
        full.blocks.bytes >> 10,
        after.blocks.bytes >> 10,
        full.link_targets.bytes >> 10,
        after.link_targets.bytes >> 10,
        full.guest_ranges.bytes >> 10,
        after.guest_ranges.bytes >> 10
    );
    assert!(after.blocks.bytes * 2 <= full.blocks.bytes, "on: the block map shrank ({} -> {} bytes)", full.blocks.bytes, after.blocks.bytes);
    assert!(bucket_bytes(&after) < bucket_bytes(&full));
    // Not below what holds the entries at the load factor: the next block finds room.
    assert!(after.blocks.bytes >= after.blocks.entries * 16, "{after:?}");

    // The code runs on through the rehashed maps: W translated again and run (twice: the second
    // pass through the links and the block map), the cold code again, each counting right.
    let (_, cold_at) = program();
    run_chain(&vm, 0, HOT);
    run_chain(&vm, 0, HOT);
    run_chain(&vm, cold_at[0], COLD);
    run_chain(&vm, cold_at[0], COLD);

    // An invalidation of the cold chain forgets its blocks and shrinks the maps again; the chain
    // is translated anew and runs right.
    let before = space.tables();
    // SAFETY: the cache is live; its one jit is not executing.
    unsafe { od_code_cache_invalidate_range(space.cache, CODE_BASE + 4 * cold_at[0] as u64, 8 * COLD as u64) };
    let invalidated = space.tables();
    assert!(invalidated.blocks.entries + COLD as u64 / 2 <= before.blocks.entries, "{before:?} -> {invalidated:?}");
    assert!(invalidated.blocks.bytes <= before.blocks.bytes);
    run_chain(&vm, cold_at[0], COLD);
    run_chain(&vm, 0, HOT);
    // SAFETY: as above.
    unsafe { od_set_shrink_tables(0) };
    drop(vm);
    drop(space);
}
