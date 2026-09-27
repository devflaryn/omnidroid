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
