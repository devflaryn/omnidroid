//! init's directories come back when something deleted them: Windows' temporary-file cleanup
//! removes empty directories under `%TEMP%`, where instances and saved devices live, and twice took
//! `/data/apex/sessions` and the rest from every saved device, whose marker still said they were
//! made -- apexd then aborted at every boot (2026-10-03, 2026-10-22).
mod common;

use omni_linux::vfs::Sysroot;

#[test]
fn init_dirs_deleted_after_they_were_made_are_made_again() {
    let Some(dir) = common::sysroot() else { panic!("no sysroot (tools/make_sysroot.py)") };
    let sysroot = Sysroot::open(&dir).expect("the sysroot");
    let instance = std::env::temp_dir().join(format!("omni-init-dirs-healed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    std::fs::create_dir_all(instance.join("data")).unwrap();

    omni_linux::boot::make_init_dirs(&sysroot, &instance);
    let sessions = instance.join("data/apex/sessions");
    let tombstones = instance.join("data/tombstones");
    assert!(sessions.is_dir() && tombstones.is_dir(), "made at the first boot");
    let marker = std::fs::read_to_string(instance.join("data/.omni-init-dirs")).unwrap();
    assert!(marker.lines().any(|l| l == "data/apex/sessions"), "the marker lists what it made");

    // An empty-folder cleanup between two boots.
    std::fs::remove_dir(&sessions).unwrap();
    std::fs::remove_dir(&tombstones).unwrap();
    omni_linux::boot::make_init_dirs(&sysroot, &instance);
    assert!(sessions.is_dir() && tombstones.is_dir(), "made again at the next boot");

    // A marker from before it listed them (empty): everything is made once more, and listed.
    std::fs::remove_dir(&sessions).unwrap();
    std::fs::write(instance.join("data/.omni-init-dirs"), b"").unwrap();
    omni_linux::boot::make_init_dirs(&sysroot, &instance);
    assert!(sessions.is_dir(), "an old marker is not trusted");
    assert!(!std::fs::read_to_string(instance.join("data/.omni-init-dirs")).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&instance);
}
