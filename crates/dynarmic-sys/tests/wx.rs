//! **W^X of the code cache on an arm64 host, measured by writing to it.**
//!
//! On x64 the cache is committed `PAGE_EXECUTE_READWRITE` and `a64_exec.rs` pins that as the D12
//! exception. The arm64 backend is different code: oaknut's `CodeBlock` maps the cache
//! `PROT_READ | PROT_WRITE | PROT_EXEC` with `MAP_JIT` on macOS, and `AddressSpace` brackets every
//! write (prelude, each emitted block, every relink and invalidation) with
//! `pthread_jit_write_protect_np(0)` / `(1)`, which switches **the calling thread's** view of every
//! `MAP_JIT` page between RW- and R-X in hardware. So the question has two halves, and the tests
//! answer the first by measurement:
//!
//! * **Can a thread that is running guest code, or resting between runs, write the cache?** It must
//!   not. Each test takes an address inside this jit's cache (the return address the last `SVC`
//!   callback was entered with, `od_jit_last_svc_return_address`), rewrites the byte already there,
//!   and must **die** -- in a child process, so the death is the result. A child that lives means
//!   the page was writable, and the test fails.
//! * **Can another thread write it at the same time?** Yes, by design of `MAP_JIT`: a thread with
//!   its own write window open (dynarmic emitting for *another* jit, or dynarmic's Mach handler
//!   thread relinking after a fastmem fault) can write every `MAP_JIT` page in the process while
//!   this thread executes them. That is the platform's contract rather than something a test here
//!   can change; it is recorded, with a one-off measurement, in `docs/ports/macos-cpu.md`.
//!
//! `od_jit_effective_config` reports the arm64 case as `code_cache::W_XOR_X_PER_THREAD`, and the
//! first test asserts that the report and the measurement agree.

#![cfg(target_arch = "aarch64")]

mod harness;

use dynarmic_sys::*;
use harness::a64;
use harness::{Vm, VmOptions, HALT_DONE};
use std::process::Command;

fn is_child() -> Option<String> {
    std::env::var("OD_WX_CHILD").ok()
}

/// Runs this binary's `name` in a child; returns its exit status.
fn child(name: &str) -> std::process::ExitStatus {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env("OD_WX_CHILD", name)
        .status()
        .expect("spawn child")
}

/// A jit that has run `SVC #0` once, and the host address inside its cache that `SVC` returned to.
fn jit_and_cache_address() -> (Vm, u64) {
    let vm = Vm::new(vec![a64::svc(2), a64::svc(0)], VmOptions::default());
    vm.start(1_000_000);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
    // SAFETY: the jit is live and this is its thread.
    let at = unsafe { od_jit_last_svc_return_address(vm.raw()) };
    assert_ne!(at, 0, "an SVC was taken, so a return address was recorded");
    (vm, at)
}

#[test]
fn the_cache_is_not_writable_from_a_thread_resting_between_runs() {
    const NAME: &str = "the_cache_is_not_writable_from_a_thread_resting_between_runs";
    if is_child().is_some() {
        let (vm, at) = jit_and_cache_address();
        assert_eq!(
            vm.effective_config().code_cache_w_xor_x,
            code_cache::W_XOR_X_PER_THREAD,
            "the report must say what the write below measures"
        );
        // Readable: it is mapped, and it is code this jit emitted (a nonzero instruction word).
        // SAFETY: `at` is inside the live jit's cache; reading it is the positive control.
        let word = unsafe { core::ptr::read_volatile((at & !3) as *const u32) };
        assert_ne!(word, 0, "the cache is mapped and holds code");
        println!("readable at {at:#x}: {word:#010x}; now writing it");
        // SAFETY: the write the host must refuse. The byte written is the byte already there.
        unsafe { core::ptr::write_volatile(at as *mut u8, core::ptr::read_volatile(at as *const u8)) };
        println!("WRITE SUCCEEDED");
        return; // exit 0: the cache was writable
    }
    let st = child(NAME);
    assert!(
        !st.success() && st.code().is_none(),
        "the write to the code cache from the jit's own thread, between runs, did not fault ({st}) \
         -- the cache is writable there, and W^X does not hold"
    );
}

#[test]
fn the_cache_is_not_writable_from_the_guest_thread_while_guest_code_runs() {
    const NAME: &str = "the_cache_is_not_writable_from_the_guest_thread_while_guest_code_runs";
    if is_child().is_some() {
        let (vm, at) = jit_and_cache_address();
        // Run again; this time `SVC #2`'s callback rewrites a byte of the cache from inside guest
        // execution, on the guest's own thread.
        vm.with_ctx(|c| c.rewrite_byte_on_svc2 = at);
        vm.start(1_000_000);
        let _ = vm.run_to_completion(16);
        println!("WRITE SUCCEEDED");
        return;
    }
    let st = child(NAME);
    assert!(
        !st.success() && st.code().is_none(),
        "a write to the code cache from inside guest execution did not fault ({st})"
    );
}
