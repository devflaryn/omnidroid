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
    let after_all = omni_cpu::dynarmic::tagged_accesses();
    // Patch 0041: a process pays a fault per instruction (and block) that meets a tag, once -- not
    // per access -- so the count does not grow with the work. (Learned sites are by guest PC, and
    // each process maps libc.so somewhere else, so every process learns its own.) The same
    // listing of `/system/etc`, then of all of `/system`: many times the allocations.
    let count = |args: &[&str]| {
        let before = omni_cpu::dynarmic::tagged_accesses();
        let (status, out, err) = run(args).expect("the sysroot is there");
        assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
        (omni_cpu::dynarmic::tagged_accesses() - before, out.lines().count())
    };
    let (small, small_lines) = count(&["/system/bin/toybox", "ls", "-lR", "/system/etc"]);
    let (large, large_lines) = count(&["/system/bin/toybox", "ls", "-lR", "/system"]);
    println!(
        "tagged accesses served by the slow path: {after_echo} after `echo`, {after_all} after `ls -lR /system/etc` and \
         `sha256sum`; `ls -lR /system/etc` ({small_lines} lines) {small}, `ls -lR /system` ({large_lines} lines) {large}; \
         {} guest instructions learned the mask",
        omni_cpu::dynarmic::tbi_sites_noted()
    );
    // With translation snapshots (`OMNI_JIT_SNAPSHOT`) a process starts from code another learned
    // the mask in -- the second listing of `/system/etc` from the first's -- and pays fewer faults
    // than a process with none: the comparison measures per-process learning, so not then.
    if omni_cpu::dynarmic::tbi_sites_noted() > 0 && std::env::var_os("OMNI_JIT_SNAPSHOT").is_none() {
        assert!(large_lines > 4 * small_lines, "the large listing is the larger workload");
        assert!(large < 2 * small, "the faults do not grow with the work: {small} for {small_lines} lines, {large} for {large_lines}");
    }
}
