//! A root layer over a tiny manifest: layer files shadow and add to the image, `.replace`
//! hides the image's children, and `list` merges the two.
use std::path::PathBuf;
use std::sync::Arc;

use omni_linux::manifest;
use omni_linux::root::module::Catalog;
use omni_linux::root::{Layer, Profile};
use omni_linux::vfs::{Node, Sysroot, Vfs};

const MANIFEST: &str = "# omnidroid sysroot v1 test\n\
d\t755\t/\n\
d\t755\t/system\n\
d\t755\t/system/etc\n\
d\t755\t/system/fonts\n\
d\t755\t/system/bin\n\
f\t644\t4\t00\t/system/etc/keep.txt\n\
f\t644\t4\t00\t/system/etc/other.txt\n\
f\t644\t4\t00\t/system/fonts/Roboto.ttf\n\
f\t755\t7\t00\t/system/bin/toybox\n";

fn instance() -> PathBuf {
    let d = std::env::temp_dir().join(format!("omni-vfs-root-layer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let m = d.join("data/adb/modules/m");
    std::fs::create_dir_all(m.join("system/etc")).unwrap();
    std::fs::create_dir_all(m.join("system/fonts")).unwrap();
    std::fs::write(m.join("module.prop"), "id=m\nname=m\nversion=1\nversionCode=1\nauthor=t\ndescription=t\n").unwrap();
    std::fs::write(m.join("system/etc/added.txt"), b"added").unwrap();
    std::fs::write(m.join("system/etc/keep.txt"), b"layer").unwrap();
    std::fs::write(m.join("system/fonts/.replace"), b"").unwrap();
    std::fs::write(m.join("system/fonts/only.ttf"), b"F").unwrap();
    d
}

fn vfs(layer: Option<Arc<Layer>>) -> Vfs {
    let m = manifest::parse(MANIFEST).expect("manifest");
    let root = Sysroot::from_manifest(&PathBuf::from("unused"), m);
    Vfs::new(root, vec![], b"/system/bin/toybox".to_vec()).with_root_layer(layer)
}

fn names(v: &Vfs, dir: &str) -> Vec<String> {
    let d = v.resolve(b"/", dir.as_bytes(), true).unwrap();
    let mut n: Vec<String> = v.list(&d).unwrap().into_iter().map(|e| String::from_utf8(e.name).unwrap()).collect();
    n.sort();
    n
}

#[test]
fn vfs_sees_layer_over_the_image() {
    let inst = instance();
    let profile = Profile::parse("root=1\nmodule=m\n");
    let cat = Catalog::discover(&inst.join("data/adb/modules"), None).unwrap();
    let v = vfs(Some(Arc::new(Layer::build(&profile, &cat, &inst))));

    assert!(matches!(v.resolve(b"/", b"/system/etc/added.txt", true).unwrap().node, Node::HostFile { .. }));
    assert!(matches!(v.resolve(b"/", b"/system/etc/keep.txt", true).unwrap().node, Node::HostFile { .. }));
    assert!(matches!(v.resolve(b"/", b"/system/etc/other.txt", true).unwrap().node, Node::SysFile { .. }));
    assert!(matches!(v.resolve(b"/", b"/system/bin/su", true).unwrap().node, Node::HostFile { .. }));
    assert!(matches!(v.resolve(b"/", b"/debug_ramdisk/su", true).unwrap().node, Node::HostFile { .. }));
    assert!(matches!(v.resolve(b"/", b"/system/etc", true).unwrap().node, Node::Dir));

    assert_eq!(names(&v, "/system/etc"), ["added.txt", "keep.txt", "other.txt"]);
    // .replace: the image's fonts are gone, the module's stay.
    assert_eq!(names(&v, "/system/fonts"), ["only.ttf"]);
    assert!(matches!(v.resolve(b"/", b"/system/fonts/Roboto.ttf", true).unwrap().node, Node::Missing { .. }));
    assert!(names(&v, "/system/bin").contains(&"su".to_string()));
}

#[test]
fn no_layer_changes_nothing() {
    let v = vfs(None);
    assert!(matches!(v.resolve(b"/", b"/system/bin/su", true).unwrap().node, Node::Missing { .. }));
    assert!(matches!(v.resolve(b"/", b"/system/fonts/Roboto.ttf", true).unwrap().node, Node::SysFile { .. }));
    assert_eq!(names(&v, "/system/etc"), ["keep.txt", "other.txt"]);
}
