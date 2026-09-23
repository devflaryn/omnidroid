//! **What a jit keeps for each translated block** (patch 0010), and the two lookups that read it.
//!
//! One jit per guest thread, each translating the same engine: MEASURED in the macOS gate at the
//! landing screen, ~600,000 blocks over 39 jits and about 2.1 KB of dynarmic bookkeeping per block
//! (the whole `EmittedBlockInfo` inline in robin_map buckets, a `std::map` for the reverse lookup,
//! a robin_map of robin_sets for the references between blocks, and a robin_map of fastmem patch
//! sites per block) -- about 1.2 GB. Patch 0010 keeps what is read after emission in flat records.
//!
//! Measured with the allocator's own count of bytes in use (`malloc_zone_statistics` over every
//! zone), which is where that bookkeeping lives; the translated code is in the code cache and is
//! not counted. Every test here takes [`LOCK`], so nothing else in this binary allocates while
//! one measures.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod harness;

use std::sync::Mutex;

use harness::{a64, Vm, VmOptions, CODE_BASE, HALT_DONE};

static LOCK: Mutex<()> = Mutex::new(());

/// `malloc_statistics_t` (`<malloc/malloc.h>`).
#[repr(C)]
#[derive(Default)]
struct MallocStatistics {
    blocks_in_use: u32,
    size_in_use: usize,
    max_size_in_use: usize,
    size_allocated: usize,
}

extern "C" {
    fn malloc_zone_statistics(zone: *mut core::ffi::c_void, stats: *mut MallocStatistics);
}

/// Bytes the allocator has handed out and not had back, over every zone.
fn heap_in_use() -> usize {
    let mut stats = MallocStatistics::default();
    // SAFETY: a null zone asks for the sum over all zones; `stats` is writable.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
    stats.size_in_use
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The instrument first: an allocation must show up in it, or a small number below means nothing.
#[test]
fn the_heap_reading_sees_an_allocation() {
    let _g = lock();
    let before = heap_in_use();
    let block = vec![1u8; 16 << 20];
    std::hint::black_box(&block);
    let grew = heap_in_use() as f64 - before as f64;
    // Not exactly 16 MiB: the test harness's own threads allocate and free around this one.
    assert!(grew >= (15 << 20) as f64, "a 16 MiB allocation moved the reading by only {grew} bytes");
}

/// `blocks` blocks of `LDR X3, [X2] ; B .+4` -- one fastmem patch site and one link each -- and an
/// `SVC` at the end.
fn chain(blocks: usize) -> Vec<u32> {
    let mut code = Vec::with_capacity(blocks * 2 + 1);
    for _ in 0..blocks {
        code.push(a64::ldr_imm(3, 2, 0));
        code.push(a64::b(1));
    }
    code.push(a64::svc(0));
    code
}

const BLOCKS: usize = 8192;

fn chain_vm() -> Vm {
    // 64 MiB: the chain's code must fit without dynarmic clearing the cache part-way.
    Vm::new(chain(BLOCKS), VmOptions { code_cache_size: 64 << 20, ..VmOptions::default() })
}

fn run_chain(vm: &Vm) {
    vm.with_ctx(|c| c.write_u64(0x100, 0x5EED));
    vm.set_reg(2, 0x100);
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the chain reached its SVC");
    assert_eq!(vm.reg(3), 0x5EED, "the translated loads ran");
}

#[test]
fn a_translated_block_costs_bytes_of_bookkeeping_not_kilobytes() {
    let _g = lock();
    // One jit first, so one-time costs (dynarmic's statics, the decoder tables) are paid before
    // the measurement.
    run_chain(&chain_vm());

    let vm = chain_vm();
    let created = heap_in_use();
    run_chain(&vm);
    let ran = heap_in_use();
    let fetched = vm.stats().read_code;
    assert!(fetched >= 2 * BLOCKS as u64, "every block was translated: {fetched} instruction fetches");

    let per_block = (ran as f64 - created as f64) / BLOCKS as f64;
    eprintln!("dynarmic bookkeeping per translated block: {per_block:.0} bytes (n = {BLOCKS} blocks)");
    // MEASURED on the pin: 1,785 bytes per block of this shape (2,100 in the gate, where blocks
    // have more patch sites). The bound is well above what the records cost and well below what
    // the maps did.
    assert!(
        per_block < 700.0,
        "{per_block:.0} bytes of bookkeeping per block: the jit is keeping each block's emission \
         products (EmittedBlockInfo in map buckets) rather than the records read after emission"
    );
}

#[test]
fn every_block_that_links_to_an_invalidated_block_is_unlinked_from_it() {
    let _g = lock();
    // Three blocks branch to one: 0, 2 and 4 each `ADD X1, X1, #1 ; B` to 10, which is
    // `ADD X0, X0, #1 ; SVC`. Block linking patches each of the three to branch straight to 10's
    // code, so each is recorded as a reference to it.
    let mut code = vec![
        a64::add_imm(1, 1, 1),
        a64::b(9),
        a64::add_imm(1, 1, 1),
        a64::b(7),
        a64::add_imm(1, 1, 1),
        a64::b(5),
        a64::NOP,
        a64::NOP,
        a64::NOP,
        a64::NOP,
        a64::add_imm(0, 0, 1),
        a64::svc(0),
    ];
    let vm = Vm::new(code.clone(), VmOptions::default());
    let run_from = |index: u64| {
        vm.set_pc(CODE_BASE + 4 * index);
        assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    };
    // Twice each: the first run translates and links, the second runs the linked code.
    for _ in 0..2 {
        for entry in [0, 2, 4] {
            run_from(entry);
        }
    }
    assert_eq!(vm.reg(0), 6);

    // The guest rewrites 10 and says so. Every block linked to the old translation must stop
    // branching to it -- one that still does runs the old `ADD #1`.
    code[10] = a64::add_imm(0, 0, 100);
    vm.with_ctx(|c| c.code = code.clone());
    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_invalidate_range(vm.raw(), CODE_BASE + 40, 4) };
    for entry in [0, 2, 4] {
        let before = vm.reg(0);
        run_from(entry);
        assert_eq!(vm.reg(0) - before, 100, "the block at index {entry} ran a stale translation of 10");
    }
    // And each is relinked to the new translation, which is still the new code.
    for entry in [4, 2, 0] {
        let before = vm.reg(0);
        run_from(entry);
        assert_eq!(vm.reg(0) - before, 100, "the block at index {entry}, relinked");
    }
}

#[test]
fn a_host_fault_finds_its_own_patch_site_among_a_blocks_several() {
    let _g = lock();
    // Identity fastmem, as `omni-cpu` configures it: the guest address is the host address. Four
    // loads in one block; only the third faults (0x2000 is in `__PAGEZERO`), so dynarmic's handler
    // must find the third of the block's four patch sites by the faulting host PC. The callbacks
    // mask 0x2000 into the arena.
    const HOLE: u64 = 0x2000;
    let code = vec![
        a64::ldr_imm(1, 5, 0),
        a64::ldr_imm(2, 5, 8),
        a64::ldr_imm(3, 0, 0),
        a64::ldr_imm(4, 5, 16),
        a64::svc(0),
    ];
    let options = VmOptions { identity: true, check_halt_on_memory_access: true, ..VmOptions::default() };
    let vm = Vm::new(code, options);
    let arena = vm.with_ctx(|c| {
        c.write_u64(0, 0x1111);
        c.write_u64(8, 0x2222);
        c.write_u64(16, 0x4444);
        c.write_u64(HOLE, 0x3333);
        c.arena as u64
    });
    vm.set_reg(5, arena);
    vm.set_reg(0, HOLE);
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
    assert_eq!([vm.reg(1), vm.reg(2), vm.reg(3), vm.reg(4)], [0x1111, 0x2222, 0x3333, 0x4444]);
    assert!(vm.stats().slow_path_reads >= 1, "the third load was served by the callbacks: {:?}", vm.stats());
}
