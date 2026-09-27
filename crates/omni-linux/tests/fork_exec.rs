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
