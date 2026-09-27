//! What init does to an instance before its services run, read from the image's own `.rc` files
//! rather than written here: every `mkdir` on a writable mount (`/data`, `/metadata`, ...), so a
//! daemon finds the directories its `init.rc` stanza expects (apexd's `/data/apex/sessions`).
use std::path::Path;

use crate::vfs::Sysroot;

/// The mounts an instance keeps on the host, by guest root: what an `.rc` `mkdir` may create.
const WRITABLE: [&str; 4] = ["/data", "/metadata", "/tmp", "/linkerconfig"];

/// Every `mkdir` in the image's init scripts whose path lies on a writable mount, made under
/// `instance`. Once per instance: a marker records it.
pub fn make_init_dirs(sysroot: &Sysroot, instance: &Path) {
    let marker = instance.join("data").join(".omni-init-dirs");
    if marker.exists() {
        return;
    }
    let owners = crate::owners::Owners::of(instance);
    let mut scripts: Vec<Vec<u8>> = vec![b"/system/etc/init/hw/init.rc".to_vec()];
    for dir in [&b"/system/etc/init"[..], b"/system_ext/etc/init", b"/product/etc/init", b"/vendor/etc/init"] {
        for name in sysroot.children(dir) {
            if name.ends_with(b".rc") {
                let mut path = dir.to_vec();
                path.push(b'/');
                path.extend_from_slice(&name);
                scripts.push(path);
            }
        }
    }
    for script in scripts {
        let Some(text) = sysroot.read(&script) else { continue };
        for line in String::from_utf8_lossy(&text).lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("mkdir") {
                continue;
            }
            let Some(path) = words.next() else { continue };
            // Properties (`${...}`) are not expanded: such a directory is made by what reads it.
            if path.contains('$') {
                continue;
            }
            let Some(root) = WRITABLE.iter().find(|r| path == **r || path.starts_with(&format!("{r}/"))) else { continue };
            let rest = &path[root.len()..];
            if rest.split('/').any(|c| c == ".." ) {
                continue;
            }
            let host = instance.join(&root[1..]).join(rest.trim_start_matches('/'));
            let _ = std::fs::create_dir_all(&host);
            // `mkdir <path> [mode] [owner] [group]`: init makes it so (root, 0755 by default).
            let mode = words.next().and_then(|m| u32::from_str_radix(m, 8).ok()).unwrap_or(0o755);
            let uid = words.next().map_or(0, crate::init::uid_of);
            let gid = words.next().filter(|g| !g.contains('=')).map_or(uid, crate::init::uid_of);
            owners.set(&host, crate::owners::Owner { uid, gid, mode });
        }
    }
    let _ = std::fs::create_dir_all(instance.join("data"));
    let _ = std::fs::write(marker, b"");
}
