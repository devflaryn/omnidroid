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

/// A file mapped shared larger than it is and grown through another descriptor, as AppSearch's
/// icing maps its arrays at their largest and grows them: SIGBUS (`BUS_ADRERR`) past the end, the
/// file's bytes once it grows over them, a store through the mapping in the file -- from an empty
/// file too. Where a view cannot run past its file's end (Windows, Linux) that part is anonymous,
/// as before: skipped.
#[test]
fn a_file_mapped_shared_past_its_end_follows_it_as_it_grows() {
    if !omni_platform::vm::shared_view_may_pass_end_of_file() {
        return;
    }
    let Some((status, out, err)) = common::run_fixture("sharedgrow", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 16, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
