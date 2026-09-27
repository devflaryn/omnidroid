//! init's global environment: what `export` sets in the boot phases -- the ramdisk's
//! `/init.environ.rc` (`ANDROID_DATA`, `ANDROID_STORAGE`, ...) and `init.rc`'s own -- is every
//! service's environment, as it is on a device. installd, for one, dereferences a variable it does
//! not find.
mod common;

use omni_linux::init::Init;

#[test]
fn services_get_the_boot_phases_exports() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let instance = std::env::temp_dir().join(format!("omni-linux-init-env-{}", std::process::id()));
    let init = Init::start(sysroot, instance, vec![b"PATH=/system/bin".to_vec()]).expect("init");
    let env: Vec<String> = init.environment().iter().map(|e| String::from_utf8_lossy(e).into_owned()).collect();
    for want in ["PATH=/system/bin", "ANDROID_DATA=/data", "ANDROID_STORAGE=/storage", "EXTERNAL_STORAGE=/sdcard", "DOWNLOAD_CACHE=/data/cache"] {
        assert!(env.iter().any(|e| e == want), "{want} in {env:?}");
    }
}

/// post-fs-data starts apexd afresh (`setprop apexd.status ""`, `restart apexd`) and later waits
/// for it (`wait_for_prop apexd.status activated`) before anything that needs the APEXes runs.
#[test]
fn boot_starts_apexd_and_waits_for_it_to_activate() {
    use omni_linux::init::Command;
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let instance = std::env::temp_dir().join(format!("omni-linux-init-apexd-{}", std::process::id()));
    let init = Init::start(sysroot, instance, Vec::new()).expect("init");
    let c = init.boot_commands();
    let at = |want: &Command| c.iter().position(|x| x == want).unwrap_or_else(|| panic!("{want:?} in {c:?}"));
    let reset = at(&Command::SetProp("apexd.status".into(), "\"\"".into()));
    let restart = at(&Command::Restart("apexd".into()));
    let wait = at(&Command::WaitForProp("apexd.status".into(), "activated".into()));
    let classpath = at(&Command::ExecStart("derive_classpath".into()));
    assert!(reset < restart && restart < wait && wait < classpath, "{c:?}");
}

/// A service line continued with `\` keeps its arguments: vold needs its `--blkid_context`, and
/// aborts without it. `init_user0` (vold preparing user 0's storage) is a boot command.
#[test]
fn a_continued_service_line_keeps_its_arguments() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let instance = std::env::temp_dir().join(format!("omni-linux-init-vold-{}", std::process::id()));
    let init = Init::start(sysroot, instance, Vec::new()).expect("init");
    let vold = &init.services()["vold"];
    assert!(vold.argv.iter().any(|a| a == "--blkid_context=u:r:blkid:s0"), "{:?}", vold.argv);
    assert!(vold.classes.iter().any(|c| c == "core"), "{:?}", vold.classes);
    assert!(init.boot_commands().contains(&omni_linux::init::Command::InitUser0));
}
