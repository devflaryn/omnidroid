//! A guest's `IC IVAU` reaches every thread of its address space, as the architecture broadcasts it
//! to the inner-shareable domain.
//!
//! On arm64 each guest thread has its own translation cache (the shared cache, D38, is x64's), and
//! the instruction-cache callback invalidated only the calling thread's. A JIT that rewrites code
//! another thread has run -- ART reusing its code cache -- left that thread running the old
//! translation: `system_server` on the real-AOSP path then died on pointers with garbage upper
//! halves, intermittently, once ART's JIT had a writable-and-executable cache on the M1.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};
use omni_mem::Protection;

/// `IC IVAU, Xt`, `DSB ISH`, `ISB`.
const fn ic_ivau(rt: u32) -> u32 {
    0xD50B_7520 | rt
}
const DSB_ISH: u32 = 0xD503_3B9F;
const ISB: u32 = 0xD503_3FDF;

#[test]
fn code_one_thread_rewrites_is_run_new_by_another_that_ran_it_before() {
    let guest = Guest::new();
    // The function both threads know: `movz x0, #1; ret`, at the start of the code region.
    let function = guest.load(&[movz(0, 1, 0), ret(30)]);
    // The rewriter, further in: store `w1` over the function's first word, then the cache
    // maintenance a JIT does.
    let rewriter = guest.load_at(0x100, &[str_w(1, 2), ic_ivau(2), DSB_ISH, ISB, ret(30)]);
    guest.space.protect(guest.code, harness::CODE_BYTES, Protection::ReadWriteExecute).expect("the JIT's code is writable");

    let (mut a, sentinel_a) = guest.thread();
    let (mut b, sentinel_b) = guest.thread();
    assert_eq!(a.run(function, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel_a });
    assert_eq!(a.x(x(0)), 1, "thread A translated and ran the old function");

    b.set_x(x(1), u64::from(movz(0, 2, 0)));
    b.set_x(x(2), function as u64);
    assert_eq!(b.run(rewriter, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel_b });

    a.set_x(x(30), sentinel_a as u64);
    assert_eq!(a.run(function, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel_a });
    assert_eq!(a.x(x(0)), 2, "thread A runs the code thread B wrote, not its old translation");
}

/// `STR Wt, [Xn]`.
const fn str_w(rt: u32, rn: u32) -> u32 {
    0xB900_0000 | (rn << 5) | rt
}
