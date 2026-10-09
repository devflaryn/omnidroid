//! **Translation snapshots (patch 0070) restore the same behaviour, and only for the same code.**
//!
//! A program runs on a shared code cache with snapshots enabled and is saved; a second cache --
//! at another address, with another monitor -- loads the snapshot and runs the program again: the
//! same results, with nothing translated. Guest code changed between the two drops exactly the
//! blocks it touches (translated again, the new code's results); a host fault in a restored
//! block is served through its restored fastmem site; another key or another code shape (a
//! compact-code switch) is refused before anything is installed.
//!
//! The snapshot switches are process-wide, so the tests take a lock.
#![cfg(target_arch = "x86_64")]

mod harness;

use std::ffi::{c_void, CString};
use std::sync::Mutex;

use dynarmic_sys::{
    od_code_cache_enable_snapshots, od_code_cache_free, od_code_cache_load_snapshot, od_code_cache_save_snapshot, od_monitor_free, od_monitor_new,
    od_set_compact_code, OdCodeCacheStats,
};
use harness::a64::{self, cond};
use harness::{Vm, VmOptions, HALT_DONE, TEST_SHARED_CACHE_BYTES, TEST_SHARED_REGION_BYTES};

static SERIAL: Mutex<()> = Mutex::new(());

/// Unmapped on the host; served through the callbacks (the arena's bytes there).
const HOLE: u64 = 0x2000;

fn identity() -> VmOptions {
    VmOptions { identity: true, check_halt_on_memory_access: true, fastmem_exclusive: true, cycle_counting: true, ..VmOptions::default() }
}

/// A shared cache with its own monitor, freed on drop (after the `Vm`s on it).
struct Cache {
    cache: *mut c_void,
    monitor: *mut c_void,
    opts: VmOptions,
}

impl Cache {
    fn new() -> Self {
        // SAFETY: freed in `Drop`.
        let monitor = unsafe { od_monitor_new(1) };
        assert!(!monitor.is_null());
        let opts = VmOptions { shared_monitor: monitor as usize, ..identity() };
        let cache = Vm::new_code_cache(&opts, monitor, std::ptr::null_mut(), TEST_SHARED_CACHE_BYTES, TEST_SHARED_REGION_BYTES, 0);
        assert!(!cache.is_null());
        Cache { cache, monitor, opts: VmOptions { shared_cache: cache as usize, ..opts } }
    }
    fn vm(&self, code: Vec<u32>) -> Vm {
        Vm::new(code, self.opts.clone())
    }
    fn stats(&self) -> OdCodeCacheStats {
        let mut s = OdCodeCacheStats::default();
        // SAFETY: live cache, writable out.
        unsafe { dynarmic_sys::od_code_cache_stats_of(self.cache, &mut s) };
        s
    }
    fn save(&self, path: &std::path::Path, key: &str) -> i64 {
        self.save_with(path, key, 0)
    }
    fn save_with(&self, path: &std::path::Path, key: &str, flags: u32) -> i64 {
        let p = CString::new(path.to_str().unwrap()).unwrap();
        let k = CString::new(key).unwrap();
        // SAFETY: live cache; NUL-terminated strings; no jit is running.
        unsafe { od_code_cache_save_snapshot(self.cache, p.as_ptr(), k.as_ptr(), u64::MAX, flags) }
    }
    fn load(&self, path: &std::path::Path, key: &str) -> i64 {
        let p = CString::new(path.to_str().unwrap()).unwrap();
        let k = CString::new(key).unwrap();
        // SAFETY: as `save`.
        unsafe { od_code_cache_load_snapshot(self.cache, p.as_ptr(), k.as_ptr()) }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        // SAFETY: every Vm on it is gone (each test drops its Vms first).
        unsafe {
            od_code_cache_free(self.cache);
            od_monitor_free(self.monitor);
        }
    }
}

/// `X0` = a host buffer, `X1` = HOLE. 200 times: load, add, store through X0; a call to a leaf that
/// adds `leaf_add` to X3; a load through the hole into X4 (a host fault, served). Then `SVC #0`.
fn program(leaf_add: u32) -> Vec<u32> {
    let mut code = a64::mov64(2, 200);
    let leaf_at = code.len() + 1;
    code.push(a64::b(3)); // over the leaf
    code.extend([a64::add_imm(3, 3, leaf_add), a64::ret(30)]);
    let start = code.len();
    assert_eq!(start, leaf_at + 2);
    code.push(a64::ldr_imm(5, 0, 0));
    code.push(a64::add_imm(5, 5, 3));
    code.push(a64::str_imm(5, 0, 0));
    let here = code.len();
    code.push(a64::bl(leaf_at as i32 - here as i32));
    code.push(a64::ldr_imm(4, 1, 0));
    code.push(a64::subs_imm(2, 2, 1));
    let here = code.len();
    code.push(a64::b_cond(cond::NE, start as i32 - here as i32));
    code.push(a64::svc(0));
    code
}

/// Run `vm` from the start with the buffer at `buffer`; (memory word, X3, X4).
fn run(vm: &Vm, buffer: *mut u64) -> (u64, u64, u64) {
    vm.with_ctx(|c| c.write_u64(HOLE, 0x1234_5678));
    vm.set_reg(0, buffer as u64);
    vm.set_reg(1, HOLE);
    vm.set_reg(3, 0);
    vm.start(1_000_000_000);
    assert_eq!(vm.run_to_completion(256) & HALT_DONE, HALT_DONE);
    // SAFETY: the buffer is live; the guest is done with it.
    (unsafe { buffer.read_volatile() }, vm.reg(3), vm.reg(4))
}

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("od-snapshot-{}-{name}", std::process::id()))
}

/// Save after a first run; a fresh cache loads and runs the same program with nothing translated.
fn saved_snapshot(path: &std::path::Path, key: &str) -> (u64, (u64, u64, u64)) {
    let cache = Cache::new();
    // SAFETY: live cache, nothing emitted yet.
    unsafe { od_code_cache_enable_snapshots(cache.cache) };
    let vm = cache.vm(program(1));
    let mut buffer = Box::new(0u64);
    let result = run(&vm, &mut *buffer);
    drop(vm);
    let emitted = cache.stats().blocks_emitted;
    assert!(emitted >= 4, "{:?}", cache.stats());
    let saved = cache.save(path, key);
    assert_eq!(saved, emitted as i64, "every block emitted was saved");
    (emitted, result)
}

#[test]
fn a_loaded_snapshot_runs_the_same_with_nothing_translated() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let path = temp("same");
    let (emitted, first) = saved_snapshot(&path, "same");
    assert_eq!(first, (600, 200, 0x1234_5678));

    // Another cache: elsewhere in memory, another monitor -- keep a third alive meanwhile so it
    // cannot land where the first was.
    let _elsewhere = Cache::new();
    let cache = Cache::new();
    assert_eq!(cache.load(&path, "same"), emitted as i64);
    let vm = cache.vm(program(1));
    let mut buffer = Box::new(0u64);
    assert_eq!(run(&vm, &mut *buffer), first, "the same results from the restored blocks");
    drop(vm);
    let s = cache.stats();
    assert_eq!(s.blocks_emitted, 0, "nothing translated: {s:?}");
    assert_eq!(s.snapshot_blocks_restored, emitted, "{s:?}");
    assert_eq!(s.snapshot_blocks_verified, emitted, "every restored block was run, verified: {s:?}");
    assert_eq!(s.snapshot_blocks_rejected, 0, "{s:?}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn changed_guest_code_drops_exactly_the_blocks_it_touches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let path = temp("changed");
    let (emitted, first) = saved_snapshot(&path, "changed");
    assert_eq!(first.1, 200);

    let cache = Cache::new();
    assert_eq!(cache.load(&path, "changed"), emitted as i64);
    // The leaf adds 2 now.
    let vm = cache.vm(program(2));
    let mut buffer = Box::new(0u64);
    assert_eq!(run(&vm, &mut *buffer), (600, 400, 0x1234_5678), "the new code ran, not the restored block");
    drop(vm);
    let s = cache.stats();
    assert_eq!(s.snapshot_blocks_rejected, 1, "the leaf's block, and only it: {s:?}");
    assert_eq!(s.blocks_emitted, 1, "translated again: {s:?}");
    assert_eq!(s.snapshot_blocks_verified, emitted - 1, "{s:?}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn another_key_or_another_code_shape_is_refused_and_the_cache_runs_as_new() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let path = temp("refused");
    let (_, first) = saved_snapshot(&path, "refused");

    let cache = Cache::new();
    assert_eq!(cache.load(&path, "another key"), -9);
    // SAFETY: stores one process-wide atomic.
    unsafe { od_set_compact_code(1) };
    assert_eq!(cache.load(&path, "refused"), -10, "compact code shapes code differently");
    // SAFETY: as above.
    unsafe { od_set_compact_code(0) };
    let vm = cache.vm(program(1));
    let mut buffer = Box::new(0u64);
    assert_eq!(run(&vm, &mut *buffer), first);
    drop(vm);
    let s = cache.stats();
    assert_eq!((s.snapshot_blocks_restored, s.snapshot_blocks_verified), (0, 0), "{s:?}");
    assert!(s.blocks_emitted > 0);
    // And a cache that has emitted is not loaded into.
    assert_eq!(cache.load(&path, "refused"), -7);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_snapshot_of_a_restored_cache_saves_the_unverified_blocks_too() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let path = temp("chain");
    let (emitted, first) = saved_snapshot(&path, "chain");
    // Loaded, nothing run, saved again: the same blocks, from their stored hashes.
    let cache = Cache::new();
    assert_eq!(cache.load(&path, "chain"), emitted as i64);
    let again = temp("chain2");
    // Only what was entered since: nothing.
    assert_eq!(cache.save_with(&temp("chain-entered"), "chain", dynarmic_sys::OD_SNAPSHOT_ENTERED_ONLY), 0);
    let _ = std::fs::remove_file(temp("chain-entered"));
    assert_eq!(cache.save(&again, "chain"), emitted as i64);
    drop(cache);
    let cache = Cache::new();
    assert_eq!(cache.load(&again, "chain"), emitted as i64);
    let vm = cache.vm(program(1));
    let mut buffer = Box::new(0u64);
    assert_eq!(run(&vm, &mut *buffer), first);
    drop(vm);
    assert_eq!(cache.stats().blocks_emitted, 0);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&again);
}
