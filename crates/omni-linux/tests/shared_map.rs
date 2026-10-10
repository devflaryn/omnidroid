//! A file mapped `MAP_SHARED` read-write -- SQLite's WAL index, a database's `-shm` -- is the file:
//! writes through one mapping are seen through another, by `pread`, and after `munmap`.
mod common;

use omni_linux::ExitStatus;

#[test]
fn a_file_mapped_shared_is_the_file() {
    let Some((status, out, err)) = common::run_fixture("sharedmap", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 11, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
