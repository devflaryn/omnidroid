//! A translation-cache clear in the middle of a run must not leave the return-stack buffer
//! pointing into the cleared cache (vendored patch 0031, arm64).
//!
//! dynarmic's arm64 backend clears the whole cache when a block is emitted with less than 1 MiB
//! left (`AddressSpace::Emit`), from inside a run. The RSB -- eight (return target, host code)
//! pairs in the run's stack frame -- kept its host code pointers, which then pointed at whatever was
//! emitted there next, and a guest `ret` whose target matched jumped into the translation of an
//! unrelated block. `docs/ports/macos.md` "Open" (m11: "a call landed in the translation of another
//! function"); on the real-AOSP path, `system_server`'s `android.bg` thread jumped into its own
//! stack (the first boot on the M1 to reach it).
//!
//! The program calls one function twice from one call site. The first call returns at once, so the
//! return site is translated early -- near the start of the cache, where the first blocks emitted
//! after any clear land. The second call runs a chain of more distinct blocks than the cache holds:
//! its RSB entry holds the return site's translation, and the chain's own emission clears the
//! cache and writes other blocks over it before the `ret`.
#![cfg(all(target_arch = "aarch64", feature = "dynarmic"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::x;
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::{ExitReason, GuestCpu, GuestRange, RunLimit};
use omni_mem::{CommitPolicy, Placement, Protection};

/// Distinct one-instruction blocks in the chain (`cbnz`): more translation than the 8 MiB cache holds (less
/// the 1 MiB it keeps free), so the chain alone forces a clear -- checked below, from how many
/// instructions were translated -- while both passes still fit one 1,000,000-instruction run slice
/// (a new slice is a new run, whose RSB starts empty).
const CHAIN: usize = 400_000;

const fn bl(offset_insns: i32) -> u32 {
    0x9400_0000 | (offset_insns as u32 & 0x03FF_FFFF)
}

#[test]
fn a_ret_after_a_mid_run_cache_clear_returns_where_the_guest_called_from() {
    let space = Arc::new(harness::high_guest_space());
    let code_bytes = (CHAIN + 64) * 4;
    let code_bytes = code_bytes.next_multiple_of(space.page_size());
    let code = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, code_bytes, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a code region");

    // x1: completed calls (2); x3: completed passes through the chain (1).
    let mut p = vec![
        mov_reg(19, 30),  // 0: keep the return address
        movz(1, 0, 0),    // 1
        movz(3, 0, 0),    // 2
    ];
    let lp = p.len() as i32; // 3: loop
    let chain_at = 16i32;
    p.push(bl(chain_at - lp)); // 3: bl chain
    p.push(add_imm(1, 1, 1)); // 4: after: x1 += 1
    p.push(subs_imm(4, 1, 2)); // 5
    p.push(b_cond(1, lp - 6)); // 6: b.ne loop
    p.push(ret(19)); // 7
    p.resize(chain_at as usize, brk(0));
    p.push(cbnz_w(1, 2)); // callee: the first call returns at once
    p.push(ret(30));
    for _ in 0..CHAIN {
        // A conditional branch ends a block (an unconditional one is followed into the next), and
        // both of its ways lead to the next instruction.
        p.push(cbnz_w(1, 1));
    }
    p.push(add_imm(3, 3, 1));
    p.push(ret(30));

    let ptr = space.ptr(code, p.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `p.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), ptr.cast::<u32>(), p.len()) };
    space.protect(code, code_bytes, Protection::ReadExecute).expect("executable");

    // 8 MiB, dynarmic's minimum and the default: the chain is several caches' worth. (Without patch
    // 0031 this ends (2, 2): the `ret` ran the chain again from a block written over the return
    // site's translation.)
    let options = DynarmicOptions { code_cache_size: 8 << 20, ..DynarmicOptions::default() };
    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    backend.invalidate_code(GuestRange::new(code, p.len() * 4).expect("a range"));
    let mut cpu = backend.create_thread_with_tls().expect("a thread");
    let sentinel = code + code_bytes - 4;
    cpu.set_return_sentinel(sentinel).expect("the sentinel");
    cpu.set_x(x(30), sentinel as u64);

    let exit = cpu.run(code, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!((cpu.x(x(1)), cpu.x(x(3))), (2, 1), "two calls, each returning once to its call site");
}
