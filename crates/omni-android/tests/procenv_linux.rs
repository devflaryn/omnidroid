//! **`setpriority` on a Linux host that withholds the privilege**, driven by translated ARM64.
//!
//! FMOD starts every one of its threads with `setpriority(PRIO_PROCESS, 0, -16)`
//! (`THREAD_PRIORITY_AUDIO`). An Android device answers `0` because `init.rc` gives every app
//! `RLIMIT_NICE` 40. A Linux desktop user usually has `RLIMIT_NICE` 0 and no `CAP_SYS_NICE`
//! (MEASURED on the port's host: `ulimit -e` is 0), so the kernel answers `EACCES` -- which is
//! what a device's kernel would answer the same request under the same limit. Before this was
//! handled the adapter turned that answer into a refusal, and a refusal kills the guest thread
//! (VERIFICATION entry 16): every FMOD thread would have died on the port and nowhere else.
//!
//! The test sets its OWN limit to 0 first, so that it asserts the same thing on every host --
//! including one whose owner has granted the limit (see `docs/ports/linux.md`), where the ordinary
//! `bionic.rs` test exercises the success path instead. Lowering one's own limit needs no
//! privilege, and this file is its own test process, so nothing else sees the change.
//!
//! ```text
//! cargo test -p omni-android --release --test procenv_linux
//! ```

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::Boundary;
use omni_cpu::ExitReason;

/// `EACCES` in bionic's (and Linux's) numbering.
const EACCES: u64 = 13;

/// Take this process's `RLIMIT_NICE` to 0, soft and hard, with util-linux's `prlimit(1)` -- the
/// test crate has no OS dependency to make the call itself, by the rule `ARCHITECTURE.md` §2
/// states. A missing `prlimit` is a failure, not a skip (VERIFICATION entry 4).
fn drop_own_nice_limit() {
    let status = std::process::Command::new("prlimit")
        .args(["--pid", &std::process::id().to_string(), "--nice=0:0"])
        .status()
        .expect("prlimit(1) (util-linux) must be runnable for this test");
    assert!(status.success(), "prlimit --nice=0:0 on this test process failed: {status}");
}

#[test]
fn a_nice_the_host_withholds_is_minus_one_with_eacces_and_the_thread_lives() {
    let _guard = serialized();
    drop_own_nice_limit();
    // The premise, checked on the host side first: with the limit at 0 the seam itself is refused
    // with a permission error -- otherwise this test would be asserting nothing about the adapter.
    let host = omni_platform::process::set_current_thread_nice(-16)
        .expect_err("with RLIMIT_NICE 0 the host must refuse nice -16");
    assert!(host.is_permission_denied(), "the host's refusal is a permission error: {host}");

    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    bionic.set_log_to_stderr(false);
    let boundary: Arc<Boundary> = builder.finish();
    let thunk = |symbol: &str| boundary.slot_named(symbol).expect("bound").address;

    // setpriority(PRIO_PROCESS, 0, -16); store X0; __errno(); store *errno.
    let out = guest.data + 0x300;
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 0);
    asm.mov(1, 0);
    asm.mov(2, (-16i64) as u64);
    asm.bl(thunk("setpriority"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let exit = {
        let _active = bionic.activate().expect("a thread block");
        boundary.run(&mut cpu, entry, BUDGET)
    };
    let exit = exit.expect("setpriority must answer the guest, not refuse and end its thread");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    assert_eq!(guest.read_u64(out) as i32, -1, "setpriority answers -1");
    assert_eq!(guest.read_u64(out + 8), EACCES, "with EACCES in errno, as the kernel answered");
}
