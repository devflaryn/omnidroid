//! An image file's owner, mode, SELinux label and file capability are the image's own
//! (`sysroot.meta`, read from the image's filesystems by tools/make_sysroot.py --meta), as a
//! device's mounted image reports them: system_server's ClatCoordinator refuses to start unless
//! the tethering module's clatd is setuid clat, labelled clatd_exec, in a directory only system
//! can enter.
mod common;

use omni_linux::ExitStatus;

/// `ls -lnZ`'s line for `path`, split into its columns.
fn columns<'a>(out: &'a str, path: &str) -> Vec<&'a str> {
    out.lines().find(|l| l.ends_with(path)).map(|l| l.split_whitespace().collect()).unwrap_or_default()
}

#[test]
fn clatd_is_the_images_setuid_clat_binary() {
    let dir = "/apex/com.android.tethering/bin/for-system";
    let clatd = "/apex/com.android.tethering/bin/for-system/clatd";
    let Some((status, out, err)) = common::run(&["/system/bin/ls", "-lnZd", dir, clatd]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
    assert_eq!(columns(&out, dir)[..5], ["drwxr-x---", "2", "0", "1000", "u:object_r:system_file:s0"], "{out}");
    assert_eq!(columns(&out, clatd)[..5], ["-rwsr-sr-x", "1", "1029", "1029", "u:object_r:clatd_exec:s0"], "{out}");
}

#[test]
fn run_as_carries_its_file_capability() {
    let Some((status, out, err)) = common::run(&["/system/bin/getfattr", "-n", "security.capability", "/system/bin/run-as"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
    // toybox names an attribute whose value is not text without printing the value.
    assert!(out.lines().any(|l| l.starts_with("security.capability")), "{out}\n{err}");
}
