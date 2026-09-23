//! **A jit's code cache costs memory for what is emitted into it, not for its size** (patch 0009).
//!
//! On macOS a page is charged to the process when it is first touched, and cache maintenance
//! touches: `sys_icache_invalidate` over an untouched `MAP_JIT` range faults every page of it in.
//! The pin invalidated the **whole** cache after emitting its prelude, so every jit -- one per guest
//! thread -- was charged its full code cache at creation. Measured by `phys_footprint`, the memory
//! the kernel charges this process for; one test in this binary, so nothing else allocates while
//! it measures.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod harness;

use harness::{a64, Vm, VmOptions, HALT_DONE};

const MIB: f64 = 1024.0 * 1024.0;

/// `task_vm_info_data_t` up to `phys_footprint` (`<mach/task_info.h>`, `#pragma pack(4)`).
#[repr(C, packed(4))]
#[derive(Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    /// device, device_peak, internal, internal_peak, external, external_peak, reusable,
    /// reusable_peak, the three purgeable_volatile counters, compressed, compressed_peak,
    /// compressed_lifetime: fourteen, then phys_footprint.
    counters: [u64; 14],
    phys_footprint: u64,
}

extern "C" {
    static mach_task_self_: u32;
    fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

fn footprint() -> f64 {
    let mut info = TaskVmInfo::default();
    let mut count = (std::mem::size_of::<TaskVmInfo>() / 4) as u32;
    // SAFETY: `info` is writable for `count` words; TASK_VM_INFO is flavor 22.
    let kr = unsafe { task_info(mach_task_self_, 22, std::ptr::addr_of_mut!(info).cast(), &mut count) };
    assert_eq!(kr, 0, "task_info(TASK_VM_INFO)");
    info.phys_footprint as f64 / MIB
}

/// The instrument first: touching memory must show up in it, or a zero below means nothing.
#[test]
fn the_footprint_reading_sees_memory_being_touched() {
    let before = footprint();
    let mut block = vec![0u8; 16 << 20];
    for page in block.chunks_mut(16384) {
        page[0] = 1;
    }
    std::hint::black_box(&block);
    let grew = footprint() - before;
    assert!(grew > 14.0, "touching 16 MiB moved the reading by only {grew:.2} MiB");
}

#[test]
fn a_jit_is_charged_for_what_it_emits_not_for_its_cache() {
    // `add x0, x0, #1; svc #0` -- enough to make the jit translate and run something.
    let code = vec![a64::add_imm(0, 0, 1), a64::svc(0)];
    let options = VmOptions { code_cache_size: 32 << 20, ..VmOptions::default() };
    // One jit first, so the process's own one-time costs (dynarmic's statics, the harness's arena
    // allocator) are paid before the measurement.
    drop(Vm::new(code.clone(), options));
    let before = footprint();
    let vms: Vec<Vm> = (0..4).map(|_| Vm::new(code.clone(), options)).collect();
    let created = footprint();
    for vm in &vms {
        vm.set_reg(0, 41);
        vm.start(1_000_000);
        let halt = vm.run_to_completion(64);
        assert_eq!(halt & HALT_DONE, HALT_DONE, "the guest reached its SVC: {halt:#x}");
        assert_eq!(vm.reg(0), 42, "the translated code ran");
    }
    let ran = footprint();
    let per_jit_created = (created - before) / vms.len() as f64;
    let per_jit_ran = (ran - before) / vms.len() as f64;
    eprintln!(
        "per jit with a 32 MiB cache (n = {}): {per_jit_created:.2} MiB created, {per_jit_ran:.2} \
         MiB after running",
        vms.len()
    );
    assert!(
        per_jit_ran < 4.0,
        "each jit cost {per_jit_ran:.2} MiB with a 32 MiB cache: the cache is being charged for its \
         size rather than for what was emitted into it"
    );
}
