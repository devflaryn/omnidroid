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
    /// The shim's count of what patch 0013's allocator has mapped (`od_dynarmic.h`).
    fn od_page_backed_bytes() -> u64;
}

/// Bytes the allocator has handed out and not had back, over every zone, plus what the backend's
/// bookkeeping holds in pages of its own (patch 0013), which the zones do not see.
fn heap_in_use() -> usize {
    let mut stats = MallocStatistics::default();
    // SAFETY: a null zone asks for the sum over all zones; `stats` is writable.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
    // SAFETY: no arguments; reads an atomic counter.
    stats.size_in_use + unsafe { od_page_backed_bytes() } as usize
}

/// `task_vm_info_data_t` up to `phys_footprint` (`<mach/task_info.h>`, `#pragma pack(4)`), as in
/// `code_cache_charge.rs`.
#[repr(C, packed(4))]
#[derive(Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    counters: [u64; 14],
    phys_footprint: u64,
}

extern "C" {
    static mach_task_self_: u32;
    fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

/// What the kernel charges this process: `phys_footprint`.
fn footprint() -> usize {
    let mut info = TaskVmInfo::default();
    let mut count = (std::mem::size_of::<TaskVmInfo>() / 4) as u32;
    // SAFETY: `info` is writable for `count` words; TASK_VM_INFO is flavor 22.
    let kr = unsafe { task_info(mach_task_self_, 22, std::ptr::addr_of_mut!(info).cast(), &mut count) };
    assert_eq!(kr, 0, "task_info(TASK_VM_INFO)");
    info.phys_footprint as usize
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

/// The second instrument: touching memory must move the footprint.
#[test]
fn the_footprint_reading_sees_memory_being_touched() {
    let _g = lock();
    let before = footprint();
    let mut block = vec![0u8; 16 << 20];
    for page in block.chunks_mut(16384) {
        page[0] = 1;
    }
    std::hint::black_box(&block);
    let grew = footprint() as f64 - before as f64;
    assert!(grew >= (15 << 20) as f64, "touching 16 MiB moved the footprint by only {grew} bytes");
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

const BLOCKS: usize = 32_768;

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
fn clearing_the_cache_gives_its_bookkeeping_back() {
    let _g = lock();
    run_chain(&chain_vm());

    let vm = chain_vm();
    let created = heap_in_use();
    run_chain(&vm);
    let ran = heap_in_use();

    // The clear is performed at the top of the next run; start that run at the final SVC, so one
    // block is translated after it.
    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_clear_cache(vm.raw()) };
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the SVC after the clear");
    let cleared = heap_in_use();

    let held = (ran as f64 - created as f64) / BLOCKS as f64;
    let kept = (cleared as f64 - created as f64) / BLOCKS as f64;
    eprintln!("per block: {held:.0} bytes held after running, {kept:.0} still held after a clear");
    assert!(held > 20.0, "the run was measured holding something: {held:.0} bytes per block");
    assert!(
        kept < 16.0,
        "{kept:.0} bytes per block are still held after the cache was cleared: the bookkeeping \
         was emptied rather than given back"
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

/// `NOP` x `nops`, then `ADD X0, X0, #1 ; SVC`: one block covering `4 * (nops + 1)` bytes.
fn straight_line(nops: usize) -> Vec<u32> {
    let mut code = vec![a64::NOP; nops];
    code.push(a64::add_imm(0, 0, 1));
    code.push(a64::svc(0));
    code
}

/// Translate `straight_line(nops)`, rewrite its `ADD` and invalidate only that word; the next run
/// must run the new `ADD`. Returns how many instructions the retranslation fetched.
fn rewrite_the_last_word(nops: usize) -> u64 {
    let mut code = straight_line(nops);
    let vm = Vm::new(code.clone(), VmOptions { code_cache_size: 32 << 20, ..VmOptions::default() });
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 1);

    code[nops] = a64::add_imm(0, 0, 100);
    vm.with_ctx(|c| c.code = code);
    vm.reset_stats();
    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_invalidate_range(vm.raw(), CODE_BASE + 4 * nops as u64, 4) };
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 1 + 100, "a {nops}-NOP block ran a stale translation of its last word");
    vm.stats().read_code
}

#[test]
fn a_write_to_any_page_a_block_came_from_invalidates_it() {
    let _g = lock();
    // Patch 0011 indexes a block by the 4 KiB guest pages it covers. 1,100 instructions cover two;
    // the rewritten word is on the second.
    let fetched = rewrite_the_last_word(1_100);
    assert!(fetched > 1_024, "the whole two-page block was retranslated: {fetched} fetches");
}

#[test]
fn a_block_wider_than_the_page_index_is_still_found() {
    let _g = lock();
    // More than 64 pages in one block goes to the list checked on every invalidation. That the
    // block really is one block is shown by the retranslation fetching all of it.
    const NOPS: usize = 70_000;
    let fetched = rewrite_the_last_word(NOPS);
    assert!(fetched > 64 * 1024, "the block spans more than 64 pages: {fetched} fetches");
}

#[test]
fn an_invalidation_of_the_whole_address_space_reaches_every_block() {
    let _g = lock();
    // More pages asked about than have anything on them: the index is walked rather than the
    // range. 16 GiB from 0x1000, which covers the harness's code. Every block must go --
    // `omni-android` sends the whole guest space when a context's queue of
    // other threads' invalidations overflows.
    let mut code = vec![a64::add_imm(0, 0, 1), a64::b(4), a64::NOP, a64::NOP, a64::NOP, a64::add_imm(0, 0, 1), a64::svc(0)];
    let vm = Vm::new(code.clone(), VmOptions::default());
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 2);

    code[0] = a64::add_imm(0, 0, 10);
    code[5] = a64::add_imm(0, 0, 100);
    vm.with_ctx(|c| c.code = code);
    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_invalidate_range(vm.raw(), 0x1000, 0x4_0000_0000) };
    vm.set_reg(0, 0);
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 110, "both blocks were retranslated");
}

#[test]
fn an_invalidation_that_leaves_no_block_standing_gives_their_bookkeeping_back() {
    let _g = lock();
    // Patch 0012. `omni-android` invalidates a context's whole guest space when its queue of other
    // threads' invalidations overflows, which leaves every block of that jit invalidated. With
    // nothing left standing and no generated code on the stack, what the invalidated blocks hold
    // can never be used again.
    run_chain(&chain_vm());

    let vm = chain_vm();
    let created = heap_in_use();
    run_chain(&vm);
    let ran = heap_in_use();

    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_invalidate_range(vm.raw(), 0x1000, 0x4_0000_0000) };
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the SVC after the invalidation");
    let invalidated = heap_in_use();

    let held = (ran as f64 - created as f64) / BLOCKS as f64;
    let kept = (invalidated as f64 - created as f64) / BLOCKS as f64;
    eprintln!("per block: {held:.0} bytes held after running, {kept:.0} after invalidating them all");
    assert!(held > 20.0, "the run was measured holding something: {held:.0} bytes per block");
    assert!(
        kept < 16.0,
        "{kept:.0} bytes per block are still held for blocks that were all invalidated between runs"
    );

    // And the chain still runs, translated afresh.
    vm.reset_stats();
    run_chain(&vm);
    assert!(vm.stats().read_code >= 2 * BLOCKS as u64, "the chain was translated again");
}

#[test]
fn a_cleared_caches_bookkeeping_goes_back_to_the_kernel() {
    let _g = lock();
    // Patch 0013. `ClearCache` gives its arrays back (0010-0012); this is about where they go. Freed
    // through the C++ heap, a large array is the host allocator's to keep, and the kernel goes on
    // charging the process for it. So the measure here is `phys_footprint`, around a clear.
    run_chain(&chain_vm());

    let vm = chain_vm();
    let created = heap_in_use();
    run_chain(&vm);
    let held = heap_in_use() - created;
    let charged = footprint();

    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { dynarmic_sys::od_jit_clear_cache(vm.raw()) };
    vm.set_pc(CODE_BASE + 4 * (2 * BLOCKS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the SVC after the clear");
    let returned = charged as f64 - footprint() as f64;

    eprintln!(
        "bookkeeping held {:.2} MiB for {BLOCKS} blocks; the clear took {:.2} MiB off the footprint",
        held as f64 / 1048576.0,
        returned / 1048576.0
    );
    assert!(held > 4 << 20, "the chain's bookkeeping was measured: {held} bytes");
    assert!(
        returned >= 0.75 * held as f64,
        "the clear gave the kernel back {:.2} MiB of the {:.2} MiB the bookkeeping held: the rest is \
         still charged to the process",
        returned / 1048576.0,
        held as f64 / 1048576.0
    );
}
