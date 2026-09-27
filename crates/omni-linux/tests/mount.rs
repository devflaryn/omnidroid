//! Bind mounts, as vold makes /data/user/0 show /data/data: what is in the source is seen, and
//! made, through the target, by every later process of the instance, until it is unmounted.
//! Unmounting what is not a mount point is `EINVAL` (vold's `UnmountTree` goes on past it).
mod common;

use omni_linux::ExitStatus;

const OK: ExitStatus = ExitStatus::Exited(0);

#[test]
fn a_bind_mount_shows_its_source_until_unmounted() {
    let Some(runs) = common::run_each(&[
        &["/system/bin/mkdir", "-p", "/data/data/probe", "/data/user/0"],
        &["/system/bin/touch", "/data/data/probe/f"],
        &["fixture:bindmount", "bind", "/data/data", "/data/user/0"],
        &["/system/bin/ls", "/data/user/0/probe"],
        &["/system/bin/touch", "/data/user/0/g"],
        &["/system/bin/ls", "/data/data"],
        &["/system/bin/grep", " /data/user/0 ", "/proc/mounts"],
        &["fixture:bindmount", "umount", "/data/user/0"],
        &["/system/bin/ls", "/data/user/0"],
        &["fixture:bindmount", "umount", "/data/user/0"],
    ]) else {
        return;
    };
    for (i, (status, out, err)) in runs[..9].iter().enumerate() {
        assert_eq!(*status, OK, "step {i}: {out}
{err}");
    }
    assert_eq!(runs[3].1.trim(), "f");
    assert!(runs[5].1.split_whitespace().any(|n| n == "g"), "{:?}", runs[5]);
    assert_eq!(runs[8].1.trim(), "", "unmounted, the target is its own empty directory again");
    assert_eq!(runs[9].1.trim(), "umount2 Invalid argument", "not a mount point any more: {:?}", runs[9]);
}
