//! Path resolution over a small hand-written manifest.
use std::path::PathBuf;

use omni_linux::errno::{ELOOP, ENOENT, ENOTDIR};
use omni_linux::manifest;
use omni_linux::vfs::{DevNode, Node, Sysroot, Vfs};

const MANIFEST: &str = "# omnidroid sysroot v1 test\n\
d\t755\t/\n\
d\t755\t/system\n\
d\t755\t/system/bin\n\
d\t755\t/system/lib64\n\
f\t644\t5\t00\t/system/lib64/libc.so\n\
f\t755\t7\t00\t/system/bin/toybox\n\
l\ttoybox\t/system/bin/echo\n\
l\t../lib64/libc.so\t/system/bin/libc-link\n\
l\t/system/bin/loop-b\t/system/bin/loop-a\n\
l\t/system/bin/loop-a\t/system/bin/loop-b\n\
d\t755\t/apex\n\
d\t755\t/apex/com.android.runtime\n\
d\t755\t/apex/com.android.runtime/bin\n\
f\t755\t3\t00\t/apex/com.android.runtime/bin/linker64\n\
l\t/apex/com.android.runtime/bin/linker64\t/system/bin/linker64\n";

fn vfs() -> Vfs {
    let m = manifest::parse(MANIFEST).expect("manifest");
    let root = Sysroot::from_manifest(&PathBuf::from("unused"), m);
    Vfs::new(root, vec![(b"/data".to_vec(), std::env::temp_dir())], b"/system/bin/toybox".to_vec())
}

#[test]
fn an_absolute_symlink_is_followed_to_the_apex() {
    let r = vfs().resolve(b"/", b"/system/bin/linker64", true).expect("resolves");
    assert_eq!(r.path, b"/apex/com.android.runtime/bin/linker64");
    assert!(matches!(r.node, Node::SysFile { size: 3, .. }));
}

#[test]
fn a_relative_link_target_resolves_against_the_links_directory() {
    let r = vfs().resolve(b"/", b"/system/bin/libc-link", true).expect("resolves");
    assert_eq!(r.path, b"/system/lib64/libc.so");
}

#[test]
fn the_final_link_is_kept_when_not_following() {
    let r = vfs().resolve(b"/", b"/system/bin/echo", false).expect("resolves");
    assert!(matches!(r.node, Node::Symlink { ref target } if target == b"toybox"));
}

#[test]
fn a_loop_is_eloop_not_a_hang() {
    assert_eq!(vfs().resolve(b"/", b"/system/bin/loop-a", true).map(|_| ()), Err(ELOOP));
}

#[test]
fn dot_dot_cwd_and_missing_paths_follow_linux() {
    let v = vfs();
    assert_eq!(v.resolve(b"/system/bin", b"../lib64/./libc.so", true).expect("relative").path, b"/system/lib64/libc.so");
    assert_eq!(v.resolve(b"/", b"/../../system", true).expect("above root").path, b"/system");
    assert!(matches!(v.resolve(b"/", b"/system/nope", true).expect("missing").node, Node::Missing { parent_is_dir: true, .. }));
    assert_eq!(v.resolve(b"/", b"/nope/deeper", true).map(|_| ()), Err(ENOENT));
    assert_eq!(v.resolve(b"/", b"/system/bin/toybox/x", true).map(|_| ()), Err(ENOTDIR));
}

#[test]
fn dev_nodes_and_proc_self_exe_are_synthesized() {
    let v = vfs();
    assert!(matches!(v.resolve(b"/", b"/dev/null", true).expect("null").node, Node::Dev(DevNode::Null)));
    let exe = v.resolve(b"/", b"/proc/self/exe", false).expect("exe");
    assert!(matches!(exe.node, Node::Symlink { ref target } if target == b"/system/bin/toybox"));
}

#[test]
fn a_writable_mount_resolves_to_its_host_directory() {
    let r = vfs().resolve(b"/", b"/data/some-file", true).expect("resolves");
    assert!(matches!(r.node, Node::Missing { host: Some(_), .. } | Node::HostFile { .. }));
}

#[test]
fn directory_listings_come_from_the_manifest() {
    let v = vfs();
    let dir = v.resolve(b"/", b"/system/bin", true).expect("dir");
    let mut names: Vec<_> = v.list(&dir).expect("list").into_iter().map(|e| e.name).collect();
    names.sort();
    assert_eq!(names, [&b"echo"[..], b"libc-link", b"linker64", b"loop-a", b"loop-b", b"toybox"]);
}

#[test]
fn the_installed_sysroot_opens_and_is_the_pinned_one() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35");
    if !dir.join("sysroot.manifest").exists() {
        eprintln!("SKIPPED: no sysroot at {} (tools/make_sysroot.py, Task 1)", dir.display());
        return;
    }
    let root = Sysroot::open(&dir).expect("the pinned sysroot");
    let v = Vfs::new(root, vec![], b"/system/bin/toybox".to_vec());
    let r = v.resolve(b"/", b"/system/bin/linker64", true).expect("linker64");
    assert_eq!(r.path, b"/apex/com.android.runtime/bin/linker64");
}

#[test]
fn a_files_host_path_is_its_content_address() {
    let m = manifest::parse("d	755	/
f	644	3	abcdef	/x
").expect("manifest");
    let root = Sysroot::from_manifest(&PathBuf::from("root"), m);
    assert_eq!(root.host_path(b"/x"), Some(PathBuf::from("root").join("objects").join("ab").join("abcdef")));
    assert_eq!(root.host_path(b"/"), None, "a directory has no content");
}
