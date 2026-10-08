//! `OMNI_JIT_TBI=0`: the real bionic and scudo with Top Byte Ignore off the CPU's direct path.
//! A tagged guest access then takes a host fault and the slow path (served, counted); this runs
//! real programs that way and says how many there were -- the number that decides whether the
//! switch pays (`omni_cpu::dynarmic::tagged_accesses`, `[tbi]` in `omni-linux-run`).
//!
//! Its own test binary: the switch is read once per process.
mod common;

use common::run;
use omni_linux::ExitStatus;

#[test]
fn real_programs_run_with_tbi_off_the_direct_path() {
    // Before any process is made: `tbi_direct_mask` reads it once.
    std::env::set_var("OMNI_JIT_TBI", "0");
    assert!(!omni_linux::process::tbi_direct_mask());
    let Some((status, out, err)) = run(&["/system/bin/toybox", "echo", "hello"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert_eq!(out, "hello\n");
    let after_echo = omni_cpu::dynarmic::tagged_accesses();
    // Something that allocates a good deal: a recursive long listing, sorted.
    let (status, out, err) = run(&["/system/bin/toybox", "ls", "-lR", "/system/etc"]).expect("the sysroot is there");
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert!(out.lines().count() > 50, "a real listing: {out}");
    let (status, out, err) = run(&["/system/bin/toybox", "sha256sum", "/system/bin/toybox"]).expect("the sysroot is there");
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert_eq!(out.len(), 64 + 2 + "/system/bin/toybox\n".len(), "{out}");
    println!(
        "tagged accesses served by the slow path: {after_echo} after `echo`, {} after `ls -lR` and `sha256sum`",
        omni_cpu::dynarmic::tagged_accesses()
    );
}
