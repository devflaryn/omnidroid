//! Milestone A2: the real toybox lists the sysroot, reads its own /proc/self/maps and sees itself in ps.
mod common;

use common::run;
use omni_linux::ExitStatus;

#[test]
fn a2_ls_l_of_the_system_libraries() {
    let Some((status, out, err)) = run(&["/system/bin/toybox", "ls", "-l", "/system/lib64"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert!(
        out.lines().any(|l| l.ends_with("libc.so -> /apex/com.android.runtime/lib64/bionic/libc.so")),
        "{out}"
    );
}

#[test]
fn a2_cat_proc_self_maps_names_the_program_its_libraries_and_its_stack() {
    let Some((status, out, err)) = run(&["/system/bin/toybox", "cat", "/proc/self/maps"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    for name in ["/system/bin/toybox", "/apex/com.android.runtime/lib64/bionic/libc.so", "[stack]"] {
        assert!(out.lines().any(|l| l.ends_with(name)), "no {name} in:\n{out}");
    }
}

#[test]
fn a2_ps_shows_this_process() {
    let Some((status, out, err)) = run(&["/system/bin/toybox", "ps", "-A"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    // Its pid: 1000, or the next free thousand when other tests' processes are alive.
    let pid = |l: &str| l.split_whitespace().nth(1).and_then(|p| p.parse::<i32>().ok()).is_some_and(|p| p % 1000 == 0);
    assert!(out.lines().any(|l| pid(l) && l.ends_with("toybox")), "{out}");
}

/// tracefs, mounted with tracing off: its settings read and write, the marker takes a write, and
/// the events directory is there (the tracing HAL aborts without it).
#[test]
fn tracefs_is_mounted_with_tracing_off() {
    let Some((status, out, err)) = common::run(&[
        "/system/bin/sh",
        "-c",
        "cat /sys/kernel/tracing/tracing_on; echo hello > /sys/kernel/tracing/trace_marker && echo marked; ls -d /sys/kernel/tracing/events",
    ]) else {
        return;
    };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}\n{err}");
    assert_eq!(out, "0\nmarked\n/sys/kernel/tracing/events\n", "{err}");
}

/// The kernel's configuration, as `CONFIG_IKCONFIG_PROC` publishes it (`/proc/config.gz`):
/// ActivityManager aborts when VINTF cannot read it.
#[test]
fn the_kernel_configuration_is_published() {
    let Some((status, out, err)) = common::run(&["/system/bin/sh", "-c", "zcat /proc/config.gz | grep -c '^CONFIG_'; zcat /proc/config.gz | grep VMAP_STACK"]) else { return };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}\n{err}");
    let mut lines = out.lines();
    assert!(lines.next().and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| n > 20), "{out}");
    assert_eq!(lines.next(), Some("CONFIG_VMAP_STACK=y"), "{out}");
}
