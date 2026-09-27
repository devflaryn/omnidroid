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
