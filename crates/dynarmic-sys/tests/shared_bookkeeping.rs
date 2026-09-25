//! **What a shared code cache keeps for each translated block** (patch 0022's emitter, x86-64),
//! measured with the C heap's own count of bytes allocated.
//!
//! MEASURED in the world (M1/M4, `OMNI_MEM_REPORT`, 2026-09-25): 968 MiB of the process's C heaps
//! committed, 905 MiB allocated, of which only 55 MiB was the runtime's Rust -- and among it single
//! allocations of 272, 224, 80 and 64 MiB. Those are exactly the bucket arrays of the emitter's
//! four robin_maps at a load factor of at most 0.5 for the ~700,000 blocks the shared cache held:
//! `patch_information` (2^21 buckets of 136 bytes -- five `std::vector`s inline, of which a shared
//! cache uses one), `fastmem_patch_info` (2^22 of 56), `outgoing_slots` (2^21 of 40) and
//! `block_descriptors` (2^21 of 32). The rest of the heap is their per-entry vectors and
//! `block_ranges`' boost::icl nodes.
//!
//! Every test here takes [`LOCK`], so nothing else in this binary allocates while one measures.
#![cfg(target_arch = "x86_64")]

mod harness;

use std::ffi::c_void;
use std::sync::Mutex;

use dynarmic_sys::*;
use harness::{a64, Vm, VmOptions, CODE_BASE, HALT_DONE, MEM_GUARD, MEM_SIZE};

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
fn heap_in_use() -> usize {
    /// `HEAP_SUMMARY` (`<heapapi.h>`).
    #[repr(C)]
    #[derive(Default)]
    struct HeapSummaryT {
        cb: u32,
        allocated: usize,
        committed: usize,
        reserved: usize,
        max_reserve: usize,
    }
    extern "system" {
        fn GetProcessHeap() -> *mut c_void;
        fn HeapSummary(heap: *mut c_void, flags: u32, summary: *mut HeapSummaryT) -> i32;
    }
    let mut s = HeapSummaryT { cb: std::mem::size_of::<HeapSummaryT>() as u32, ..Default::default() };
    // SAFETY: the process heap is live for the process; `s` is writable and states its size. The
    // C runtime's `malloc`/`operator new` and Rust's `System` allocator both allocate from it.
    let ok = unsafe { HeapSummary(GetProcessHeap(), 0, &mut s) };
    assert_ne!(ok, 0, "HeapSummary");
    s.allocated
}

#[cfg(target_os = "linux")]
fn heap_in_use() -> usize {
    /// glibc's `struct mallinfo2`.
    #[repr(C)]
    struct Mallinfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }
    extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: no arguments; returns plain data.
    let m = unsafe { mallinfo2() };
    m.uordblks + m.hblkhd
}

#[cfg(not(any(windows, target_os = "linux")))]
fn heap_in_use() -> usize {
    0
}

/// The instrument first: an allocation must show up in it, or a small number below means nothing.
#[test]
fn the_heap_reading_sees_an_allocation() {
    let _g = lock();
    let before = heap_in_use();
    let block = vec![1u8; 16 << 20];
    std::hint::black_box(&block);
    let grew = heap_in_use() as f64 - before as f64;
    assert!(grew >= (15 << 20) as f64, "a 16 MiB allocation moved the reading by only {grew} bytes");
}

/// `units` units of four instructions, then `SVC #0`. A unit is two blocks, as the engine's code
/// is: `LDR X3, [X2] ; STR X3, [X2, #8] ; B.EQ +2` (two fastmem patch sites, two links: the
/// branch not taken and the branch taken) and `B +1` (one link) -- so per unit two blocks, three
/// link slots, two link targets and two fastmem sites. Z is clear, so `B.EQ` falls through and
/// both blocks run.
fn units(units: usize) -> Vec<u32> {
    let mut code = Vec::with_capacity(units * 4 + 1);
    for _ in 0..units {
        code.push(a64::ldr_imm(3, 2, 0));
        code.push(a64::str_imm(3, 2, 8));
        code.push(a64::b_cond(a64::cond::EQ, 2));
        code.push(a64::b(1));
    }
    code.push(a64::svc(0));
    code
}

const UNITS: usize = 32_768;
const BLOCKS: usize = 2 * UNITS;
/// What a block of [`units`]' shape may cost in the shared cache's tables.
const BYTES_PER_BLOCK_BOUND: f64 = 260.0;

/// One guest address space on a shared cache big enough that nothing is retired.
struct Shared {
    vm: Option<Vm>,
    cache: *mut c_void,
    monitor: *mut c_void,
}

impl Shared {
    fn new(code: Vec<u32>) -> Self {
        let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
        let arena = arena.as_mut_ptr();
        // SAFETY: freed in `Drop`, after the jit.
        let monitor = unsafe { od_monitor_new(1) };
        assert!(!monitor.is_null());
        let opts = VmOptions {
            shared_arena: arena as usize,
            shared_monitor: monitor as usize,
            ..VmOptions::default()
        };
        let cache = Vm::new_code_cache(&opts, monitor, arena, 256 << 20, 64 << 20);
        assert!(!cache.is_null(), "od_code_cache_new refused the configuration");
        let vm = Vm::new(code, VmOptions { shared_cache: cache as usize, ..opts });
        Self { vm: Some(vm), cache, monitor }
    }

    fn vm(&self) -> &Vm {
        self.vm.as_ref().expect("live")
    }

    fn run(&self) {
        let vm = self.vm();
        vm.with_ctx(|c| c.write_u64(0x100, 0x5EED));
        vm.set_reg(2, 0x100);
        vm.start(u64::MAX);
        assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the chain reached its SVC");
        assert_eq!(vm.reg(3), 0x5EED, "the translated loads ran");
        assert_eq!(vm.with_ctx(|c| c.read_u64(0x108)), 0x5EED, "the translated stores ran");
    }

    fn tables(&self) -> OdCodeCacheTables {
        let mut t = OdCodeCacheTables::default();
        // SAFETY: the cache is live.
        unsafe { od_code_cache_tables_of(self.cache, &mut t) };
        t
    }

    fn stats(&self) -> OdCodeCacheStats {
        let mut s = OdCodeCacheStats::default();
        // SAFETY: the cache is live.
        unsafe { od_code_cache_stats_of(self.cache, &mut s) };
        s
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        drop(self.vm.take());
        // SAFETY: the only jit on the cache is gone; each handle is freed once.
        unsafe {
            od_code_cache_free(self.cache);
            od_monitor_free(self.monitor);
        }
    }
}

#[test]
fn a_block_in_a_shared_cache_costs_bytes_of_bookkeeping_not_kilobytes() {
    let _g = lock();
    // One cache first, so one-time costs (dynarmic's statics, the decoder tables) are paid before
    // the measurement.
    Shared::new(units(UNITS)).run();

    let shared = Shared::new(units(UNITS));
    let created = heap_in_use();
    shared.run();
    let ran = heap_in_use();
    let stats = shared.stats();
    assert!(stats.blocks_emitted >= BLOCKS as u64, "every block was translated: {stats:?}");

    let per_block = (ran as f64 - created as f64) / BLOCKS as f64;
    eprintln!(
        "shared-cache bookkeeping per translated block: {per_block:.0} bytes (n = {BLOCKS} blocks, \
         {} bytes of code each)",
        stats.code_bytes_emitted / stats.blocks_emitted
    );
    let tables = shared.tables();
    let mut counted = 0u64;
    for (name, t) in tables.named() {
        counted += t.bytes;
        eprintln!(
            "  {name}: {} entries, {:.0} bytes per block, largest allocation {} KiB",
            t.entries,
            t.bytes as f64 / BLOCKS as f64,
            t.largest_bytes >> 10
        );
    }
    eprintln!("  the census: {:.0} bytes per block", counted as f64 / BLOCKS as f64);
    // MEASURED: 1,144 bytes per block on the pin (patch 0024's census), 401 with patch 0025's flat
    // link and fastmem records, 289 with patch 0026's guest-range index, 225 with patch 0027's
    // fuller block map; the bound sits above that and below each earlier step.
    assert!(
        per_block < BYTES_PER_BLOCK_BOUND,
        "{per_block:.0} bytes of bookkeeping per block: the shared cache is keeping its links, \
         fastmem sites or guest ranges in maps and trees again"
    );

    // The census is what `OMNI_MEM_REPORT` says the heap holds for the cache: it must account for
    // what the heap grew by (less the allocator's own headers, and the few blocks of the jit's own
    // run), or the report's attribution means nothing.
    let grew = (ran - created) as f64;
    assert!(
        (counted as f64 - grew).abs() < 0.15 * grew,
        "the census counts {counted} bytes, the heap grew by {grew}"
    );
    // And the address it names lies inside an allocation at least as large as the one it sizes.
    for (name, t) in tables.named() {
        if t.largest_bytes >= 1 << 20 {
            let span = allocation_span(t.largest_address as usize);
            assert!(span >= t.largest_bytes as usize, "{name}: its address is in an allocation of {span} bytes, not {}", t.largest_bytes);
        }
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// **Measurement, not an assertion**: what a cold block costs a shared cache to translate and emit
/// -- the bookkeeping is written at emission. Seven fresh caches, the median.
///
/// ```text
/// cargo test -p dynarmic-sys --release --test shared_bookkeeping -- --ignored --nocapture --test-threads 1
/// ```
#[test]
#[ignore]
fn bench_cold_translation_into_a_shared_cache() {
    let _g = lock();
    let (mut wall, mut emit, mut translate) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..7 {
        let shared = Shared::new(units(UNITS));
        let t0 = std::time::Instant::now();
        shared.run();
        let ns = t0.elapsed().as_nanos() as f64;
        let s = shared.stats();
        wall.push(ns / s.blocks_emitted as f64);
        emit.push(s.emit_ns as f64 / s.blocks_emitted as f64);
        translate.push(s.translate_ns as f64 / s.blocks_emitted as f64);
    }
    eprintln!(
        "cold, per block (n = {BLOCKS}, median of 7): {:.0} ns in all, {:.0} ns emitting, {:.0} ns translating",
        median(wall),
        median(emit),
        median(translate)
    );
}

/// **Measurement, not an assertion**: what a dispatcher lookup that misses the thread's own
/// fast-dispatch table costs -- the shared cache's lock, taken shared, and the block map. A loop
/// calls, through `BLR`, 100,000 one-instruction functions (`RET`) in a scattered order read from a
/// table, so nearly every call misses the 4,096-entry fast-dispatch table and the block map is
/// large. Warm: every block is translated before the timed passes.
#[test]
#[ignore]
fn bench_dispatcher_lookups_in_a_shared_cache() {
    let _g = lock();
    const TARGETS: usize = 100_000;
    const TABLE: u64 = 0x1_0000;
    // 0: LDR X9, [X13, X10, LSL #3] ; 1: ADD X10, X10, #1 ; 2: BLR X9 ; 3: SUBS X12, X12, #1 ;
    // 4: B.NE 0 ; 5: SVC #0 ; 6..: RET
    let mut code = vec![
        a64::ldr_reg(9, 13, 10),
        a64::add_imm(10, 10, 1),
        a64::blr(9),
        a64::subs_imm(12, 12, 1),
        a64::b_cond(a64::cond::NE, -4),
        a64::svc(0),
    ];
    code.extend(std::iter::repeat(a64::ret(30)).take(TARGETS));
    let shared = Shared::new(code);
    let vm = shared.vm();
    vm.with_ctx(|c| {
        // A scattered order: a stride coprime with the count.
        for i in 0..TARGETS as u64 {
            let target = (i * 7_919) % TARGETS as u64;
            c.write_u64(TABLE + 8 * i, CODE_BASE + 4 * (6 + target));
        }
    });
    let pass = || {
        vm.set_reg(10, 0);
        vm.set_reg(12, TARGETS as u64);
        vm.set_reg(13, TABLE);
        vm.start(u64::MAX);
        assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    };
    pass();
    let blocks = shared.stats().blocks_emitted;
    let mut per_call = Vec::new();
    for _ in 0..9 {
        let before = shared.stats().locked_lookups;
        let t0 = std::time::Instant::now();
        pass();
        let ns = t0.elapsed().as_nanos() as f64;
        let locked = shared.stats().locked_lookups - before;
        assert!(locked > TARGETS as u64 / 2, "the calls missed the fast-dispatch table: {locked}");
        per_call.push(ns / TARGETS as f64);
    }
    assert_eq!(shared.stats().blocks_emitted, blocks, "the timed passes translated nothing");
    let t = shared.tables().blocks;
    eprintln!(
        "a call through the dispatcher's locked lookup: {:.1} ns (median of 9 passes of {TARGETS}); block map {} entries in {} KiB",
        median(per_call),
        t.entries,
        t.largest_bytes >> 10
    );
}

/// `NOP` x `nops`, then `ADD X0, X0, #1 ; SVC`: one block covering `4 * (nops + 1)` bytes.
fn straight_line(nops: usize) -> Vec<u32> {
    let mut code = vec![a64::NOP; nops];
    code.push(a64::add_imm(0, 0, 1));
    code.push(a64::svc(0));
    code
}

/// Translate `straight_line(nops)`, rewrite its `ADD` and invalidate only that word; the next run
/// must run the new `ADD`. Returns how many instructions the retranslation fetched. On the jit's
/// own cache, or -- `OD_TEST_SHARED_CACHE=1` -- a shared one.
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
    unsafe { od_jit_invalidate_range(vm.raw(), CODE_BASE + 4 * nops as u64, 4) };
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 1 + 100, "a {nops}-NOP block ran a stale translation of its last word");
    vm.stats().read_code
}

/// Patch 0026 indexes a block by the 4 KiB guest pages it covers (the arm64 backend's patch 0011,
/// and these three tests, for x64). 1,100 instructions cover two; the rewritten word is on the
/// second.
#[test]
fn a_write_to_any_page_a_block_came_from_invalidates_it() {
    let fetched = rewrite_the_last_word(1_100);
    assert!(fetched > 1_024, "the whole two-page block was retranslated: {fetched} fetches");
}

/// More than 64 pages in one block goes to the list checked on every invalidation. That the block
/// really is one block is shown by the retranslation fetching all of it.
#[test]
fn a_block_wider_than_the_page_index_is_still_found() {
    let fetched = rewrite_the_last_word(70_000);
    assert!(fetched > 64 * 1024, "the block spans more than 64 pages: {fetched} fetches");
}

/// More pages asked about than have anything on them: the index is walked rather than the range.
/// 16 GiB from 0x1000, which covers the harness's code -- `omni-android` sends the whole guest space
/// when a context's queue of other threads' invalidations overflows. Every block must go.
#[test]
fn an_invalidation_of_the_whole_address_space_reaches_every_block() {
    let mut code = vec![a64::add_imm(0, 0, 1), a64::b(4), a64::NOP, a64::NOP, a64::NOP, a64::add_imm(0, 0, 1), a64::svc(0)];
    let vm = Vm::new(code.clone(), VmOptions::default());
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 2);

    code[0] = a64::add_imm(0, 0, 10);
    code[5] = a64::add_imm(0, 0, 100);
    vm.with_ctx(|c| c.code = code);
    // SAFETY: `vm.raw()` is live and not executing.
    unsafe { od_jit_invalidate_range(vm.raw(), 0x1000, 0x4_0000_0000) };
    vm.set_reg(0, 0);
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    assert_eq!(vm.reg(0), 110, "both blocks were retranslated");
}

/// **A region given back takes its fastmem records with it** (patch 0025): the sites are records of
/// the region they are in, dropped when it is reclaimed -- not entries of one map that only grows.
/// The chain is longer than a region, so running it again after each clear retires regions, and
/// the cache gives them back between passes.
#[test]
fn a_region_given_back_takes_its_fastmem_records_with_it() {
    let _g = lock();
    let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
    let arena = arena.as_mut_ptr();
    // SAFETY: freed below, after the jit.
    let monitor = unsafe { od_monitor_new(1) };
    let opts = VmOptions { shared_arena: arena as usize, shared_monitor: monitor as usize, ..VmOptions::default() };
    let cache = Vm::new_code_cache(&opts, monitor, arena, 40 << 20, 8 << 20);
    assert!(!cache.is_null());
    let shared = Shared { vm: Some(Vm::new(units(UNITS), VmOptions { shared_cache: cache as usize, ..opts })), cache, monitor };
    for _ in 0..4 {
        shared.run();
        // SAFETY: the cache is live and its one jit is not executing.
        unsafe { od_code_cache_clear(shared.cache) };
    }
    shared.run();
    let stats = shared.stats();
    let sites = shared.tables().fastmem_sites;
    eprintln!("{} fastmem records for {} blocks emitted; {stats:?}", sites.entries, stats.blocks_emitted);
    assert!(stats.regions_reclaimed >= 1, "regions were given back: {stats:?}");
    // One site per block emitted, on average, in this shape: every record kept would be as many.
    assert!(
        sites.entries + 1_000 < stats.blocks_emitted,
        "{} fastmem records for {} blocks emitted: a reclaimed region's records were kept",
        sites.entries,
        stats.blocks_emitted
    );
}

/// The bytes from the start of the allocation holding `address` to its end, per the OS.
#[cfg(windows)]
fn allocation_span(address: usize) -> usize {
    /// `MEMORY_BASIC_INFORMATION` (x64).
    #[repr(C)]
    #[derive(Default)]
    struct MemoryBasicInformation {
        base_address: usize,
        allocation_base: usize,
        allocation_protect: u32,
        partition_id: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        kind: u32,
    }
    extern "system" {
        fn VirtualQuery(address: *const c_void, info: *mut MemoryBasicInformation, len: usize) -> usize;
    }
    let query = |at: usize| {
        let mut info = MemoryBasicInformation::default();
        // SAFETY: `info` is writable and its size is passed; VirtualQuery reads nothing at `at`.
        let got = unsafe { VirtualQuery(at as *const c_void, &mut info, std::mem::size_of::<MemoryBasicInformation>()) };
        assert_ne!(got, 0, "VirtualQuery({at:#x})");
        info
    };
    let base = query(address).allocation_base;
    let mut end = base;
    loop {
        let info = query(end);
        if info.allocation_base != base {
            return end - base;
        }
        end = info.base_address + info.region_size;
    }
}

/// On Linux a large array is its own `mmap`; the test does not read the maps.
#[cfg(not(windows))]
fn allocation_span(_address: usize) -> usize {
    usize::MAX
}

#[test]
fn clearing_a_shared_cache_gives_its_bookkeeping_back() {
    let _g = lock();
    Shared::new(units(UNITS)).run();

    let shared = Shared::new(units(UNITS));
    let created = heap_in_use();
    shared.run();
    let ran = heap_in_use();
    // SAFETY: the cache is live and its one jit is not executing.
    unsafe { od_code_cache_clear(shared.cache) };
    let vm = shared.vm();
    vm.set_pc(CODE_BASE + 4 * (4 * UNITS as u64));
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the SVC after the clear");
    let cleared = heap_in_use();

    let held = (ran as f64 - created as f64) / BLOCKS as f64;
    let kept = (cleared as f64 - created as f64) / BLOCKS as f64;
    eprintln!("per block: {held:.0} bytes held after running, {kept:.0} still held after a clear");
}
