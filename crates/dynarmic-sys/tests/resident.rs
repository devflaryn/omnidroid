//! **What a code cache keeps resident, as opposed to committed** (vendored patch 0036).
//!
//! Windows only: the host process's *private working set* is the figure the owner's RAM target is
//! in, and a page that was committed but never touched is not in it. The pin cleared the 2 MiB
//! constant pool of every code cache with `memset` at creation, so every one of its pages was
//! resident -- zeros -- although a cache stores a few KiB of constants there. In PS99 in-world the
//! system's host process (~65 guest processes, a code cache each) held 84 MiB of resident zero
//! pages in its executable regions (`wsscan.ps1`, 2026-10-08).
//!
//! Measured per page with `QueryWorkingSetEx` over the cache's own reservation, found as the
//! executable allocation that appeared when the cache was made. `OMNI_JIT_POOL_MEMSET=1` brings
//! the old clear back (read once per process): run this binary with it to see the test fail.
#![cfg(all(windows, target_arch = "x86_64"))]

mod harness;

use dynarmic_sys::*;
use harness::{a64, Vm, VmOptions, HALT_DONE, MEM_GUARD, MEM_SIZE};
use std::collections::BTreeMap;
use std::ffi::c_void;

/// `MEMORY_BASIC_INFORMATION` (x64).
#[repr(C)]
#[derive(Default, Clone, Copy)]
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

/// `PSAPI_WORKING_SET_EX_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy)]
struct WorkingSetExInformation {
    virtual_address: usize,
    attributes: usize,
}

extern "system" {
    fn VirtualQuery(address: *const c_void, info: *mut MemoryBasicInformation, len: usize) -> usize;
    fn GetCurrentProcess() -> *mut c_void;
    fn K32QueryWorkingSetEx(process: *mut c_void, info: *mut WorkingSetExInformation, len: u32) -> i32;
}

const MEM_COMMIT: u32 = 0x1000;
const MEM_PRIVATE: u32 = 0x2_0000;
const PAGE: usize = 4096;

/// Every private allocation of this process holding a committed executable page: its base, and its
/// committed ranges.
fn executable_allocations() -> BTreeMap<usize, Vec<(usize, usize)>> {
    let mut by_base: BTreeMap<usize, (bool, Vec<(usize, usize)>)> = BTreeMap::new();
    let mut at = 0usize;
    loop {
        let mut info = MemoryBasicInformation::default();
        // SAFETY: `info` is writable and its size is passed; VirtualQuery reads nothing at `at`.
        let got = unsafe { VirtualQuery(at as *const c_void, &mut info, std::mem::size_of::<MemoryBasicInformation>()) };
        if got == 0 {
            break;
        }
        if info.state == MEM_COMMIT && info.kind == MEM_PRIVATE {
            let e = by_base.entry(info.allocation_base).or_default();
            e.0 |= info.protect & 0xF0 != 0;
            e.1.push((info.base_address, info.region_size));
        }
        let next = info.base_address + info.region_size;
        if next <= at {
            break;
        }
        at = next;
    }
    by_base.into_iter().filter(|(_, (exec, _))| *exec).map(|(b, (_, r))| (b, r)).collect()
}

/// Of the committed pages in `ranges`, how many are resident (in this process's working set).
fn resident_pages(ranges: &[(usize, usize)]) -> (usize, usize) {
    let mut q: Vec<WorkingSetExInformation> = ranges
        .iter()
        .flat_map(|&(b, n)| (0..n / PAGE).map(move |i| WorkingSetExInformation { virtual_address: b + i * PAGE, attributes: 0 }))
        .collect();
    let bytes = u32::try_from(q.len() * std::mem::size_of::<WorkingSetExInformation>()).expect("a small query");
    // SAFETY: `q` is writable and its byte size is passed; the call only reads the addresses.
    let ok = unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), q.as_mut_ptr(), bytes) };
    assert_ne!(ok, 0, "QueryWorkingSetEx");
    (q.iter().filter(|e| e.attributes & 1 != 0).count(), q.len())
}

/// A shared code cache's executable allocation, created now: (committed pages, resident pages).
fn a_fresh_cache(run_something: bool) -> (usize, usize) {
    let before = executable_allocations();
    let arena: &'static mut [u64] = Box::leak(vec![0u64; (MEM_SIZE + MEM_GUARD) / 8].into_boxed_slice());
    let arena = arena.as_mut_ptr();
    // SAFETY: freed below, after the one jit using it.
    let monitor = unsafe { od_monitor_new(2) };
    assert!(!monitor.is_null());
    let opts = VmOptions { shared_arena: arena as usize, shared_monitor: monitor as usize, ..VmOptions::default() };
    let cache = Vm::new_code_cache(&opts, monitor, arena, 64 << 20, 16 << 20, 16 << 20);
    assert!(!cache.is_null());
    if run_something {
        let code = vec![
            a64::movz(0, 0, 0),
            a64::movz(1, 1000, 0),
            a64::add_shifted(0, 0, 1, 0, 0),
            a64::subs_imm(1, 1, 1),
            a64::b_cond(a64::cond::NE, -2),
            a64::svc(0),
        ];
        let vm = Vm::new(code, VmOptions { shared_cache: cache as usize, ..opts });
        vm.start(u64::MAX >> 2);
        assert_eq!(vm.run_to_completion(64) & HALT_DONE, HALT_DONE);
        assert_eq!(vm.reg(0), 500_500);
    }
    let after = executable_allocations();
    let fresh: Vec<_> = after.iter().filter(|(b, _)| !before.contains_key(b)).collect();
    assert_eq!(fresh.len(), 1, "one new executable allocation: the cache's");
    let (resident, committed) = resident_pages(fresh[0].1);
    // SAFETY: no jit uses either any more.
    unsafe {
        od_code_cache_free(cache);
        od_monitor_free(monitor);
    }
    (committed, resident)
}

#[test]
fn a_code_cache_s_constant_pool_is_committed_but_not_resident() {
    let (committed, resident) = a_fresh_cache(false);
    let mib = |pages: usize| (pages * PAGE) as f64 / (1 << 20) as f64;
    eprintln!("a new cache: {:.2} MiB committed, {:.2} MiB resident", mib(committed), mib(resident));
    // The prelude's 2 MiB commit, the pool's 2 MiB, and the first region's headroom.
    assert!(mib(committed) >= 4.0, "{:.2} MiB committed", mib(committed));
    // The prelude's code (0.88 MiB measured) is resident; the pool's zeros must not be. With the
    // pin's memset this reads 2.88 MiB (`OMNI_JIT_POOL_MEMSET=1`).
    assert!(mib(resident) < 2.0, "{:.2} MiB of a new cache resident: its constant pool was written", mib(resident));

    // And a cache that has run code still holds only what it wrote.
    let (_, resident_after_run) = a_fresh_cache(true);
    eprintln!("after a run: {:.2} MiB resident", mib(resident_after_run));
    assert!(mib(resident_after_run) < 2.0, "{:.2} MiB", mib(resident_after_run));
}
