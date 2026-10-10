//! What init does to an instance before its services run, read from the image's own `.rc` files
//! rather than written here: every `mkdir` on a writable mount (`/data`, `/metadata`, ...), so a
//! daemon finds the directories its `init.rc` stanza expects (apexd's `/data/apex/sessions`).
use std::path::Path;

use crate::vfs::Sysroot;

/// The mounts an instance keeps on the host, by guest root: what an `.rc` `mkdir` may create.
pub const WRITABLE: [&str; 6] = ["/data", "/metadata", "/tmp", "/linkerconfig", "/mnt", "/storage"];

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
    // Its bind mounts and symbolic links between writable places: /storage shows /mnt/user/0,
    // where vold mounts the emulated volume (`crate::mount`); /mnt/sdcard is /storage/self/primary.
    let binds = crate::vfs::Binds::of(instance);
    let on_writable = |path: &str| {
        let root = WRITABLE.iter().find(|r| path == **r || path.starts_with(&format!("{r}/")))?;
        let rest = &path[root.len()..];
        (!rest.split('/').any(|c| c == "..")).then(|| instance.join(&root[1..]).join(rest.trim_start_matches('/')))
    };
    let script = sysroot.read(b"/system/etc/init/hw/init.rc").unwrap_or_default();
    for line in String::from_utf8_lossy(&script).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            // Not one inside the other: `/linkerconfig/bootstrap` on `/linkerconfig` is init's own
            // switch of linker configurations, which `crate::init` makes itself.
            ["mount", "none", source, target, "bind", ..] if !source.starts_with(&format!("{target}/")) && !target.starts_with(&format!("{source}/")) => {
                if let (Some(host), Some(over)) = (on_writable(source), on_writable(target)) {
                    let _ = std::fs::create_dir_all(&host);
                    let _ = std::fs::create_dir_all(&over);
                    binds.bind(target.as_bytes().to_vec(), host, Some(over));
                }
            }
            ["symlink", target, link] => {
                if let Some(host) = on_writable(link) {
                    if !target.contains('$') && std::fs::write(&host, target.as_bytes()).is_ok() {
                        owners.set(&host, crate::owners::Owner { uid: 0, gid: 0, mode: 0o120_777 });
                    }
                }
            }
            _ => {}
        }
    }
    let _ = std::fs::create_dir_all(instance.join("data"));
    let _ = std::fs::write(marker, b"");
}
