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

/// **A forking background subshell beside a forking parent** -- the device setup's shape
/// (`r_scripts::lean`: `( for a in ...; do r=$(cmd ...); done ) & lean_pid=$!`, then the parent's
/// own commands): the subshell never executes a program, so it lives beside the shell in the one
/// memory, and each side forks children of its own that run in it until they execute theirs. A
/// side's fork child runs in that side's view; the pair must not hand the memory to the other side
/// meanwhile (the child would run on in the other side's view, and the fork's restore then write the
/// first side's pages over it): the shell crashed on a pointer of 0x80 bytes and the subshell on a
/// smashed stack canary, run r-2664, 2026-10-09.
#[test]
fn a_forking_background_subshell_and_its_forking_parent_keep_their_own_memory() {
    let script = "( i=0; while [ $i -lt 10 ]; do r=$(getprop ro.build.version.sdk); i=$((i+1)); done; echo child $i $r ) & \
                  lean=$!; j=0; while [ $j -lt 10 ]; do s=$(getprop ro.build.version.sdk); j=$((j+1)); done; \
                  wait $lean; echo parent $j $s $?";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(common::run(&["/system/bin/sh", "-c", script]));
    });
    let Some((status, out, err)) = rx.recv_timeout(std::time::Duration::from_secs(300)).expect("the shell hung") else { return };
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["child 10 35", "parent 10 35 0"], "{err}");
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
