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
    assert!(out.lines().any(|l| l.contains(" 1000 ") && l.ends_with("toybox")), "{out}");
}
