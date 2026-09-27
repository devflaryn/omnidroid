//! omnidroid's device overlay: its vendor files appear over the pinned image, to the VFS and to a
//! real program listing the directory.
mod common;

use omni_linux::vfs::Sysroot;
use omni_linux::ExitStatus;

const MANIFEST_DIR: &[u8] = b"/vendor/etc/vintf/manifest";
const OMNI_GRAPHICS: &[u8] = b"/vendor/etc/vintf/manifest/omni-graphics.xml";

#[test]
fn the_overlay_files_are_sysroot_files_beside_the_image() {
    let Some(dir) = common::sysroot() else { panic!("no sysroot (tools/make_sysroot.py)") };
    let root = Sysroot::open(&dir).expect("the sysroot");
    let want = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/device/vendor/etc/vintf/manifest/omni-graphics.xml")).unwrap();
    assert_eq!(root.read(OMNI_GRAPHICS).as_deref(), Some(want.as_slice()), "the overlay file's bytes");
    let names = root.children(MANIFEST_DIR);
    assert!(names.iter().any(|n| n == b"omni-graphics.xml"), "listed in its directory");
    assert!(names.iter().any(|n| n == b"hwc3.xml"), "beside the image's own");
    // The image itself is untouched: an image file reads as before.
    assert!(root.read(b"/vendor/etc/vintf/manifest/hwc3.xml").is_some_and(|b| b.starts_with(b"<!--")));
}

#[test]
fn a_real_ls_lists_the_overlay_file() {
    let (status, out, err) = common::run(&["/system/bin/ls", "-l", "/vendor/etc/vintf/manifest"]).expect("the sysroot");
    assert_eq!(status, ExitStatus::Exited(0), "ls: {err}");
    assert!(out.contains("omni-graphics.xml"), "ls: {out}");
    assert!(out.contains("hwc3.xml"), "ls: {out}");
}
