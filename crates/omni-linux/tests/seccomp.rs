//! seccomp filters (`crate::seccomp`): minijail installs one in the media daemons (mediaextractor,
//! media.swcodec, the codec HAL), which abort when the kernel refuses it. The filter -- a classic
//! BPF program -- answers every system call from then on, in the threads and children too.
mod common;

use omni_linux::ExitStatus;

#[test]
fn a_filter_answers_each_system_call() {
    let Some((status, out, err)) = common::run_fixture("seccomp", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 11, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
