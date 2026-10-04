//! fork, execve and wait (docs/superpowers/specs/2026-09-27-fork-exec-design.md): a child runs a
//! program with a pipe for its stdout, the parent reads it and reaps the child, and nothing the
//! child did in its parent's memory before executing shows in the parent.
mod common;

use omni_linux::ExitStatus;

#[test]
fn a_forked_child_executes_a_program_and_is_reaped() {
    let Some((status, out, err)) = common::run_fixture("forkexec", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 12, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}

/// A fork child that never executes a program and waits on its parent (an anti-tamper watchdog's
/// shape, Clash of Clans'): parent and child take turns in the one memory, each with its own view
/// of it, so the parent runs on while the child waits -- not a deadlock.
#[test]
fn a_forked_child_that_waits_on_its_parent_lives_beside_it() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(common::run_fixture("forkwatch", &[]));
    });
    let Some((status, out, err)) = rx.recv_timeout(std::time::Duration::from_secs(120)).expect("forkwatch hung: the parent never ran on after the fork") else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 11, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}

/// `execve` in a process that did not fork (a shell's `exec`, netbpfload handing over to the
/// platform's bpfloader): the program replaces it under the same pid, and the process ends as the
/// program does.
#[test]
fn a_process_replaces_itself_with_a_program() {
    let Some((status, out, err)) = common::run(&["/system/bin/sh", "-c", "echo $$; exec /system/bin/sh -c 'echo $$; exit 7'"]) else { return };
    assert_eq!(status, ExitStatus::Exited(7), "{out}\n{err}");
    let pids: Vec<&str> = out.lines().collect();
    assert_eq!(pids.len(), 2, "{out}");
    assert_eq!(pids[0], pids[1], "the same process: {out}");
}

/// The shell's own forks: a pipeline (two children at once, told apart by pid -- a pid just freed
/// is not given again at once) and command substitution (a child that runs a builtin and exits
/// without executing anything: the parent's memory, `.data` included, is its own again).
#[test]
fn the_shell_runs_pipelines_and_command_substitution() {
    let Some((status, out, err)) = common::run(&["/system/bin/sh", "-c", "echo pipe | cat; echo $(echo sub); x=$(getprop ro.build.version.sdk); echo sdk $x"]) else { return };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}\n{err}");
    assert_eq!(out, "pipe\nsub\nsdk 35\n", "{err}");
}
