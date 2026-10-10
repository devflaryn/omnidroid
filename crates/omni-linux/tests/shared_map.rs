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

/// A pool mapped `MAP_SHARED` over a file with nothing in it yet and grown with `ftruncate` (Roblox's
/// asset pool): its part past the file's end is a host file's view (`mm::tail_backing`), and holds
/// what is written -- across the growth, a fork, a file offset inside a host page, a fixed address.
#[test]
fn a_pool_mapped_past_its_file_s_end_holds_its_writes() {
    let Some((status, out, err)) = common::run_fixture("sharedtail", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}
{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 17, "{out}
{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}
{err}");
}
