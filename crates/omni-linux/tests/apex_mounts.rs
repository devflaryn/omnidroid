//! The image's APEXes are mounted as apexd would have mounted them at boot: `/proc/mounts` lists
//! each at `/apex/<name>@<version>` on a loop device, and `/sys/block/loopN/loop/backing_file`
//! names its APEX file (a compressed APEX: the decompressed file apexd writes). apexd reads exactly
//! this at start (`MountedApexDatabase::PopulateFromMounts`), and so reports them active -- which
//! PackageManagerService needs to find the packages inside them (the permission controller).
mod common;

use omni_linux::ExitStatus;

#[test]
fn the_apexes_are_mounted_on_loop_devices_backed_by_their_files() {
    let (status, mounts, err) = common::run(&["/system/bin/cat", "/proc/mounts"]).expect("sysroot");
    assert_eq!(status, ExitStatus::Exited(0), "{err}");
    // com.android.permission (the permission controller's APEX) is compressed in this image.
    let line = mounts.lines().find(|l| l.split(' ').nth(1).is_some_and(|m| m.starts_with("/apex/com.android.permission@"))).unwrap_or_else(|| panic!("{mounts}"));
    let device = line.split(' ').next().unwrap();
    let loop_name = device.strip_prefix("/dev/block/").expect("a block device");
    assert!(loop_name.starts_with("loop"), "{line}");
    let version = line.split(' ').nth(1).unwrap().rsplit('@').next().unwrap();
    let backing = format!("/sys/block/{loop_name}/loop/backing_file");
    let (status, file, err) = common::run(&["/system/bin/cat", &backing]).expect("sysroot");
    assert_eq!(status, ExitStatus::Exited(0), "{err}");
    assert_eq!(file.trim(), format!("/data/apex/decompressed/com.android.permission@{version}.decompressed.apex"));
    // An uncompressed one is backed by its own file.
    let line = mounts.lines().find(|l| l.split(' ').nth(1).is_some_and(|m| m.starts_with("/apex/com.android.i18n@"))).unwrap_or_else(|| panic!("{mounts}"));
    let loop_name = line.split(' ').next().unwrap().strip_prefix("/dev/block/").unwrap();
    let (_, file, _) = common::run(&["/system/bin/cat", &format!("/sys/block/{loop_name}/loop/backing_file")]).expect("sysroot");
    assert_eq!(file.trim(), "/system/apex/com.android.i18n.apex");
}
