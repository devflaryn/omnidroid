//! **Patch 0061's compact code is the same behaviour in fewer bytes.** With `od_set_compact_code`'s
//! fault-stub bit a fastmem site's slow path is `call fallback` (only where a bounds check jumps to
//! it) and one `call` to a shared abort-check thunk with the guest PC as 8 bytes of data, instead of
//! the call and an inline 38-byte abort check; with its link-tail bit a shared-cache link leaves a
//! spent budget through its slot's own tail instead of a second copy of it. These tests run the
//! paths that changed -- a host fault served and resumed, a memory abort raised inside the fallback
//! (the guest PC stored, `Run` left, the access re-executed), budget-spent links on a shared cache
//! -- with the switch off and on (both bits), and check the bytes went down.
//!
//! The switch is process-wide and read at emit time, so the tests take a lock.

mod harness;

use std::ffi::c_void;
use std::sync::Mutex;

use dynarmic_sys::{od_code_cache_free, od_codegen_census, od_codegen_census_reset, od_jit_clear_halt, od_monitor_free, od_monitor_new, od_set_compact_code, CODEGEN_PARTS, OD_COMPACT_FAULT_STUBS, OD_COMPACT_LINK_TAILS, OD_HALT_MEMORY_ABORT};
use harness::a64::{self, cond};
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE, TEST_SHARED_CACHE_BYTES, TEST_SHARED_REGION_BYTES};

static SWITCH: Mutex<()> = Mutex::new(());

/// Unmapped on every host (Windows never maps the low 64 KiB, macOS has `__PAGEZERO`): a host fault
/// at the fastmem site; the callbacks mask it into the arena.
const HOLE: u64 = 0x2000;

fn set_compact(on: bool) {
    // SAFETY: stores one process-wide atomic.
    unsafe { od_set_compact_code(if on { OD_COMPACT_FAULT_STUBS | OD_COMPACT_LINK_TAILS } else { 0 }) };
}

fn identity(cycle_counting: bool) -> VmOptions {
    VmOptions { identity: true, check_halt_on_memory_access: true, fastmem_exclusive: true, cycle_counting, ..VmOptions::default() }
}

/// A shared cache and its monitor, or neither.
type Shared = Option<(*mut c_void, *mut c_void)>;

/// A `Vm` on `opts`, on a shared cache of its own when `shared` (as `host_fault.rs` builds one);
/// the cache is freed by [`free_cache`] after the `Vm` is dropped.
fn vm(code: Vec<u32>, opts: VmOptions, shared: bool) -> (Vm, Shared) {
    if !shared {
        return (Vm::new(code, opts), None);
    }
    // SAFETY: freed in `free_cache`, after the jit.
    let monitor = unsafe { od_monitor_new(1) };
    assert!(!monitor.is_null());
    let opts = VmOptions { shared_monitor: monitor as usize, ..opts };
    let cache = Vm::new_code_cache(&opts, monitor, std::ptr::null_mut(), TEST_SHARED_CACHE_BYTES, TEST_SHARED_REGION_BYTES, 0);
    assert!(!cache.is_null(), "od_code_cache_new refused the configuration");
    let vm = Vm::new(code, VmOptions { shared_cache: cache as usize, ..opts });
    assert_eq!(vm.code_cache(), cache);
    (vm, Some((cache, monitor)))
}

fn free_cache(shared: Shared) {
    if let Some((cache, monitor)) = shared {
        // SAFETY: the only jit on them is gone.
        unsafe {
            od_code_cache_free(cache);
            od_monitor_free(monitor);
        }
    }
}

fn shapes() -> Vec<(bool, bool)> {
    let mut v = vec![(false, false), (true, false)];
    if cfg!(target_arch = "x86_64") {
        v.extend([(false, true), (true, true)]);
    }
    v
}

/// `MOVZ X5,#1 ; LDR X1,[X0] ; LDR X2,[X0,#8] ; MOVZ X5,#2 ; SVC`, X0 the hole: both loads fault
/// on the host and are served by the callbacks, execution resumes after each.
#[test]
fn a_host_fault_resumes_after_the_load() {
    let _g = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    for (compact, shared) in shapes() {
        set_compact(compact);
        let code = vec![a64::movz(5, 1, 0), a64::ldr_imm(1, 0, 0), a64::ldr_imm(2, 0, 8), a64::movz(5, 2, 0), a64::svc(0)];
        let (vm, cache) = vm(code, identity(false), shared);
        vm.with_ctx(|c| {
            c.write_u64(HOLE, 0x1122_3344_5566_7788);
            c.write_u64(HOLE + 8, 0x99AA_BBCC_DDEE_FF00);
        });
        for pass in 0..2 {
            vm.set_reg(0, HOLE);
            vm.start(1_000_000);
            let hr = vm.run_to_completion(64);
            assert_eq!(hr & HALT_DONE, HALT_DONE, "compact {compact} shared {shared} pass {pass}: halt {hr:#x}");
            assert_eq!(vm.reg(1), 0x1122_3344_5566_7788, "compact {compact} shared {shared} pass {pass}");
            assert_eq!(vm.reg(2), 0x99AA_BBCC_DDEE_FF00, "compact {compact} shared {shared} pass {pass}");
            assert_eq!(vm.reg(5), 2, "compact {compact} shared {shared} pass {pass}");
        }
        drop(vm);
        free_cache(cache);
    }
    set_compact(false);
}

/// The fallback of a faulting load raises a memory abort (as omni-cpu does for a guest SIGSEGV):
/// `Run` returns `MemoryAbort` with the PC at the load and the instructions after it not run;
/// cleared and run again, the load is re-executed and served.
#[test]
fn a_memory_abort_in_the_fallback_stops_at_the_load() {
    let _g = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    for (compact, shared) in shapes() {
        set_compact(compact);
        let code = vec![a64::movz(5, 1, 0), a64::add_imm(6, 6, 1), a64::ldr_imm(1, 0, 0), a64::movz(5, 2, 0), a64::svc(0)];
        let (vm, cache) = vm(code, identity(false), shared);
        let what = format!("compact {compact} shared {shared}");
        vm.with_ctx(|c| {
            c.write_u64(HOLE, 0x1122_3344_5566_7788);
            c.abort_on_read = HOLE;
        });
        vm.set_reg(0, HOLE);
        vm.start(1_000_000);
        let hr = vm.run();
        assert_eq!(hr & OD_HALT_MEMORY_ABORT, OD_HALT_MEMORY_ABORT, "{what}: halt {hr:#x}");
        assert_eq!(vm.pc(), CODE_BASE + 8, "{what}: the PC is the faulting load's");
        assert_eq!(vm.reg(5), 1, "{what}: nothing after the load ran");
        assert_eq!(vm.reg(6), 1, "{what}");
        // SAFETY: the jit is live and not executing.
        unsafe { od_jit_clear_halt(vm.raw(), OD_HALT_MEMORY_ABORT) };
        let hr = vm.run_to_completion(64);
        assert_eq!(hr & HALT_DONE, HALT_DONE, "{what}: halt {hr:#x} after the abort");
        assert_eq!(vm.reg(1), 0x1122_3344_5566_7788, "{what}: the load re-executed");
        assert_eq!(vm.reg(5), 2, "{what}");
        assert_eq!(vm.reg(6), 1, "{what}: only the load re-executed");
        drop(vm);
        free_cache(cache);
    }
    set_compact(false);
}

/// Eight two-instruction blocks linked in a loop, run in slices of 5 ticks with cycle counting:
/// nearly every link finds the budget spent and leaves `Run` (through the slot's tail with the
/// switch on), and the loop still counts exactly.
#[test]
fn links_that_find_the_budget_spent_leave_and_resume_exactly() {
    let _g = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    const ITERATIONS: u64 = 300;
    for (compact, shared) in shapes() {
        set_compact(compact);
        let mut code = a64::mov64(0, ITERATIONS);
        let start = code.len();
        for i in 0..8 {
            code.push(a64::add_imm(2 + i % 4, 2 + i % 4, 1));
            code.push(a64::b_cond(cond::NE, 1));
        }
        code.push(a64::subs_imm(0, 0, 1));
        let here = code.len();
        code.push(a64::b_cond(cond::NE, start as i32 - here as i32));
        code.push(a64::svc(0));
        let (vm, cache) = vm(code, identity(true), shared);
        vm.start(5);
        let mut runs = 0;
        loop {
            vm.with_ctx(|c| c.ticks_remaining = 5);
            let hr = vm.run();
            runs += 1;
            if hr & HALT_DONE != 0 {
                break;
            }
            assert_eq!(hr & !dynarmic_sys::OD_HALT_CACHE_INVALIDATION, 0, "compact {compact} shared {shared}: halt {hr:#x}");
            if hr != 0 {
                // SAFETY: the jit is live and not executing.
                unsafe { od_jit_clear_halt(vm.raw(), hr) };
            }
            assert!(runs < 100_000, "compact {compact} shared {shared}: no progress");
        }
        for r in 2..6 {
            assert_eq!(vm.reg(r), 2 * ITERATIONS, "compact {compact} shared {shared}: X{r}");
        }
        assert!(runs > 100, "compact {compact} shared {shared}: the budget was spent at links ({runs} runs)");
        drop(vm);
        free_cache(cache);
    }
    set_compact(false);
}

fn census() -> Vec<u64> {
    let mut out = vec![0u64; CODEGEN_PARTS.len()];
    // SAFETY: `out` holds `CODEGEN_PARTS.len()` writable `u64`s.
    unsafe { od_codegen_census(out.as_mut_ptr(), out.len() as u32) };
    out
}

fn part(c: &[u64], name: &str) -> u64 {
    c[CODEGEN_PARTS.iter().position(|p| *p == name).unwrap_or_else(|| panic!("no census part {name}"))]
}

/// Out-of-line code per memory access and terminal code per block, off and on (x64 only: the
/// census is the x64 emitter's). Measured here: see the printout (`--nocapture`).
#[cfg(target_arch = "x86_64")]
#[test]
fn the_compact_code_is_smaller() {
    let _g = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    // Sixteen blocks of `LDR; ADD; STR; CMP; B.NE`, on a shared cache (where the link changes),
    // over host memory (identity fastmem: no access faults, so no block is rebuilt).
    let mut data = vec![0u64; 16];
    let data_ptr = data.as_mut_ptr();
    let mut code = a64::mov64(1, data_ptr as u64);
    for i in 0..16 {
        code.extend([a64::ldr_imm(5, 1, 8 * i), a64::add_imm(5, 5, 1), a64::str_imm(5, 1, 8 * i), a64::subs_shifted(31, 5, 0), a64::b_cond(cond::NE, 1)]);
    }
    code.push(a64::svc(0));
    let mut figures = Vec::new();
    for compact in [false, true] {
        set_compact(compact);
        // SAFETY: resets process-wide counters; the lock keeps other tests' emission out.
        unsafe { od_codegen_census_reset() };
        let (vm, cache) = vm(code.clone(), identity(true), true);
        vm.start(1_000_000);
        assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
        drop(vm);
        free_cache(cache);
        let c = census();
        let (far, ops, terminal, blocks) = (part(&c, "far"), part(&c, "memory_ops"), part(&c, "terminal"), part(&c, "blocks"));
        assert!(ops >= 32 && blocks >= 16, "compact {compact}: census {c:?}");
        figures.push((far as f64 / ops as f64, terminal as f64 / blocks as f64));
        println!("compact {compact}: far {:.1} B a memory access, terminal {:.1} B a block ({blocks} blocks, {ops} accesses)", figures.last().unwrap().0, figures.last().unwrap().1);
    }
    set_compact(false);
    // SAFETY: `data` is live; the guest wrote it through `data_ptr` and is done.
    assert_eq!(unsafe { data_ptr.read_volatile() }, 2, "both runs incremented the data");
    drop(data);
    assert!(figures[1].0 < figures[0].0 - 10.0, "out-of-line code: {figures:?}");
    assert!(figures[1].1 < figures[0].1 - 10.0, "terminal code: {figures:?}");
}
