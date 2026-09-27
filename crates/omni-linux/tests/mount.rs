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

/// A bind mount is the instance's, not one host process's: made in one (vold binds /data/data
/// onto /data/user/0 in the system's), it is there in another (an app's, which finds its data
/// through it).
#[test]
fn a_bind_mount_is_seen_by_another_host_process() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-binds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    std::fs::create_dir_all(instance.join("data/source")).unwrap();
    std::fs::write(instance.join("data/source/marker"), b"here").unwrap();
    std::fs::create_dir_all(instance.join("data/target")).unwrap();
    let run = |uid: &str, argv: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_omni-linux-run"))
            .args(["--sysroot", &sysroot.to_string_lossy(), "--instance", &instance.to_string_lossy(), "--uid", uid, "--"])
            .args(argv)
            .output()
            .expect("omni-linux-run")
    };
    let bound = run("0", &["/system/bin/mount", "--bind", "/data/source", "/data/target"]);
    assert!(bound.status.success(), "{}", String::from_utf8_lossy(&bound.stderr));
    let seen = run("0", &["/system/bin/cat", "/data/target/marker"]);
    assert_eq!(String::from_utf8_lossy(&seen.stdout), "here", "{}", String::from_utf8_lossy(&seen.stderr));
    let _ = std::fs::remove_dir_all(&instance);
}
