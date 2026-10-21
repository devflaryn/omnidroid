//! **The duplication census** (arm64, `OMNI_JIT_CENSUS=1`; vendored patch 0106): two threads that
//! run the same code each translate it into their own cache, and the census says so -- every block
//! held twice (`D_mem` 2) and emitted twice (`D_emit` 2) -- while each thread is read at its own run
//! edge, not stopped.
#![cfg(all(target_arch = "aarch64", feature = "dynarmic"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::x;
use omni_cpu::dynarmic::{census, DynarmicBackend, DynarmicOptions};
use omni_cpu::{ExitReason, GuestCpu, GuestRange, RunLimit};
use omni_mem::{CommitPolicy, Placement, Protection};

const CHAIN: usize = 2_000;

#[test]
fn two_threads_running_the_same_code_hold_and_emit_it_twice() {
    assert!(census::enable(), "arm64 has a census");
    let space = Arc::new(harness::high_guest_space());
    let mut p = Vec::new();
    for _ in 0..CHAIN {
        p.push(cbnz_w(1, 1)); // a block each
    }
    p.push(add_imm(3, 3, 1));
    p.push(ret(30));
    let code_bytes = ((p.len() + 4) * 4).next_multiple_of(space.page_size());
    let code = space
        .map_anonymous(Placement::Anywhere { align: space.page_size() }, code_bytes, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("a code region");
    let ptr = space.ptr(code, p.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `p.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), ptr.cast::<u32>(), p.len()) };
    space.protect(code, code_bytes, Protection::ReadExecute).expect("executable");
    let tail = code + CHAIN * 4;

    let options = DynarmicOptions { code_cache_size: 16 << 20, ..DynarmicOptions::default() };
    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    backend.invalidate_code(GuestRange::new(code, p.len() * 4).expect("a range"));
    let sentinel = code + code_bytes - 4;
    let mut threads: Vec<_> = (0..2).map(|_| backend.create_thread_with_tls().expect("a thread")).collect();
    for cpu in &mut threads {
        cpu.set_return_sentinel(sentinel).expect("the sentinel");
        cpu.set_x(x(30), sentinel as u64);
        assert_eq!(cpu.run(code, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    }

    // Asked while neither runs nor is parked: each reads its own at the end of its next run.
    assert!(backend.census_begin());
    for cpu in &mut threads {
        cpu.set_x(x(30), sentinel as u64);
        assert_eq!(cpu.run(tail, RunLimit::Unlimited).expect("runs"), ExitReason::Returned { pc: sentinel });
    }
    let c = backend.census_end().expect("a census");
    eprintln!("{c:?}");
    assert_eq!(c.threads, 2);
    assert!(c.distinct_blocks as usize > CHAIN, "{c:?}");
    assert_eq!(c.blocks, 2 * c.distinct_blocks, "each thread holds every block: {c:?}");
    assert_eq!(c.bytes, 2 * c.distinct_bytes, "{c:?}");
    assert!((c.d_mem() - 2.0).abs() < 1e-9);
    assert_eq!(c.emitted_blocks, 2 * c.emitted_distinct_blocks, "{c:?}");
    assert!((c.d_emit() - 2.0).abs() < 1e-9, "{c:?}");
    assert!(c.own_bytes > 0);

    // A look with no run since reads nothing live; what was emitted stays counted.
    assert!(backend.census_begin());
    let quiet = backend.census_end().expect("a census");
    assert_eq!((quiet.threads, quiet.blocks), (0, 0));
    assert_eq!(quiet.emitted_bytes, c.emitted_bytes);
}
