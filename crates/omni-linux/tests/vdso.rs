//! The vDSO (`crate::vdso`), from a guest through the real bionic: one is named by
//! `AT_SYSINFO_EHDR`; bionic's `clock_gettime` takes it and is faster than the system call; a clock
//! read through it and through the system call, interleaved, never goes back (MONOTONIC, REALTIME,
//! BOOTTIME, MONOTONIC_COARSE); `gettimeofday` agrees with `CLOCK_REALTIME`; the resolution is 1 ns.
//! The fixture is `tests/fixtures/vdso.c`.
mod common;

#[test]
fn bionic_reads_the_clock_through_the_vdso_and_it_agrees_with_the_system_call() {
    let Some((status, out, err)) = common::run_fixture("vdso", &[]) else { return };
    eprintln!("{out}");
    assert!(matches!(status, omni_linux::ExitStatus::Exited(0)), "{status:?}\n{out}\n{err}");
    assert!(out.lines().next().is_some_and(|l| l.starts_with("vdso at 0x")), "AT_SYSINFO_EHDR names the vDSO: {out}");
    let line = out.lines().find(|l| l.starts_with("clock_gettime:")).expect("the timing line");
    let figure = |after: &str| -> u64 { line.split(after).nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok()).expect("a figure") };
    let (libc, syscall) = (figure("libc "), figure("system call "));
    // A libc call that made the system call could not be faster than the bare call, so one clearly
    // below it is the vDSO. How far below is the host's: Windows 53 against 153 ns, the Linux host
    // (i5-4460) 122 against 205 -- under three quarters leaves room for noise on both.
    assert!(libc * 4 < syscall * 3, "bionic's clock_gettime ({libc} ns) should be well under the system call's ({syscall} ns): it is not taking the vDSO");
    assert!(out.contains("resolution 1 ns"), "{out}");
}
