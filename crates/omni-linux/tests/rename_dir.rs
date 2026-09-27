//! A directory renamed while a file in it is open -- PackageManager moves an install's staging
//! directory while the APK in it is still open -- as Linux renames it: open descriptors stay valid
//! and the files are found under the new name (Windows refuses such a rename of a directory).
mod common;

use omni_linux::ExitStatus;

#[test]
fn a_directory_with_open_files_is_renamed() {
    let Some((status, out, err)) = common::run_fixture("renamedir", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 8, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
