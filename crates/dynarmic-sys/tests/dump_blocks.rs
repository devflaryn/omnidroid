//! **The x64 dynarmic emits** (measurement, `#[ignore]`d): the x64 dynarmic emits for chosen guest addresses of a
//! library, as omni-cpu configures the jit (shared cache, INTERRUPTIBLE optimisations).
//! OD_LIB=<elf> OD_TEXT=<off>,<vaddr>,<len> (hex) OD_PCS=<hex,...> OD_OUT=<dir>
//!   OD_TEST_SHARED_CACHE=1 cargo test -p dynarmic-sys --release --test dump_blocks -- --ignored --nocapture
mod harness;

use dynarmic_sys::*;
use harness::{Vm, VmOptions, CODE_BASE};
use std::io::Write;

struct Sink {
    out: std::path::PathBuf,
    n: usize,
}

unsafe extern "C" fn observe(ctx: *mut std::ffi::c_void, guest_pc: u64, host: *const std::ffi::c_void, code_bytes: usize, _total: usize) {
    // SAFETY: the test's live sink; `host` is valid for `code_bytes` (the block just emitted).
    let (sink, code) = unsafe { (&mut *(ctx as *mut Sink), std::slice::from_raw_parts(host as *const u8, code_bytes)) };
    let path = sink.out.join(format!("{:02}-{:x}.bin", sink.n, guest_pc));
    sink.n += 1;
    std::fs::File::create(&path).and_then(|mut f| f.write_all(code)).expect("write");
    println!("block {guest_pc:#x}: {code_bytes} bytes -> {}", path.display());
}

#[test]
#[ignore = "measurement"]
fn dump_blocks() {
    let lib = std::env::var("OD_LIB").expect("OD_LIB");
    let text: Vec<u64> = std::env::var("OD_TEXT").expect("OD_TEXT").split(',').map(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap()).collect();
    let (off, vaddr, len) = (text[0], text[1], text[2]);
    let pcs: Vec<u64> = std::env::var("OD_PCS").expect("OD_PCS").split(',').map(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap()).collect();
    let out = std::path::PathBuf::from(std::env::var("OD_OUT").expect("OD_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let elf = std::fs::read(&lib).unwrap();
    let code: Vec<u32> = elf[off as usize..(off + len) as usize].chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
    let vm = Vm::new(
        code,
        VmOptions {
            cycle_counting: true,
            check_halt_on_memory_access: true,
            fastmem_exclusive: true,
            optimizations: optimization::INTERRUPTIBLE | optimization::UNSAFE_IGNORE_GLOBAL_MONITOR,
            code_cache_size: 256 << 20,
            ..VmOptions::default()
        },
    );
    let mut sink = Box::new(Sink { out, n: 0 });
    let ctx: *mut Sink = &mut *sink;
    // SAFETY: the sink outlives the observer, removed below; one thread emits.
    unsafe { od_set_emit_observer(Some(observe), ctx.cast()) };
    for pc in pcs {
        vm.set_pc(CODE_BASE + (pc - vaddr));
        vm.with_ctx(|c| {
            c.ticks_remaining = 1;
            c.ticks_used = 0;
        });
        let hr = vm.run();
        if hr != 0 {
            // SAFETY: the jit is live and not executing.
            unsafe { od_jit_clear_halt(vm.raw(), hr) };
        }
    }
    // SAFETY: removes the observer.
    unsafe { od_set_emit_observer(None, std::ptr::null_mut()) };
}
