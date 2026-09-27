//! omnidroid's device overlay: the files this runtime adds over the pinned AOSP image as a device's
//! vendor adds them -- the declarations of the HALs it serves from the host, and the in-process
//! halves of those HALs (a gralloc mapper). They live in `crates/omni-linux/device/` at their guest
//! paths and are compiled in; [`crate::vfs::Sysroot::open`] shows them as sysroot files. The image
//! is never edited: its manifest and its pin are unchanged.
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::manifest::Entry;

/// Every overlay file: its guest path and its bytes.
pub const FILES: &[(&str, &[u8])] = &[
    ("/vendor/etc/vintf/manifest/omni-graphics.xml", include_bytes!("../device/vendor/etc/vintf/manifest/omni-graphics.xml")),
    // gralloc 5's in-process mapper (`device/src/mapper.c`; `device/src/build.txt`, `device/SHA256SUMS`).
    ("/vendor/lib64/hw/mapper.omni.so", include_bytes!("../device/vendor/lib64/hw/mapper.omni.so")),
    // The boot ramdisk's global environment (AOSP's init.environ.rc), which init.rc imports.
    ("/init.environ.rc", include_bytes!("../device/init.environ.rc")),
    // The Vulkan driver, forwarding to the host's GPU (`device/src/vk/`; `crate::gpu`).
    ("/vendor/lib64/hw/vulkan.omni.so", include_bytes!("../device/vendor/lib64/hw/vulkan.omni.so")),
    // This device has no sensors: the AOSP sensors multihal loads no sub-HAL and serves an empty
    // list (the image's lists the emulator's, which needs QEMU's sensors transport).
    ("/vendor/etc/sensors/hals.conf", include_bytes!("../device/vendor/etc/sensors/hals.conf")),
];

/// The image's vendor files this device replaces with its own: device configuration, which a
/// vendor partition holds for its hardware. Any other overlay path already in the image is an
/// error.
pub const REPLACES: &[&str] = &["/vendor/etc/sensors/hals.conf"];

/// An overlay file as the sysroot holds it: its guest path, its manifest entry, and the host file
/// with its bytes.
pub struct Materialized {
    pub guest: Vec<u8>,
    pub entry: Entry,
    pub host: PathBuf,
}

/// Write each overlay file once to a content-addressed host file (mapping a library needs a real
/// file), and describe it.
///
/// # Errors
/// A host file that cannot be written.
pub fn materialize() -> Result<Vec<Materialized>, String> {
    let dir = std::env::temp_dir().join("omni-device");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    FILES
        .iter()
        .map(|(guest, bytes)| {
            let sha256 = format!("{:x}", Sha256::digest(bytes));
            let host = dir.join(&sha256);
            if std::fs::read(&host).ok().as_deref() != Some(*bytes) {
                // Written beside and renamed, so a process mapping the file never sees it partial.
                let tmp = dir.join(format!("{sha256}.{}", std::process::id()));
                std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
                if std::fs::rename(&tmp, &host).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    if std::fs::read(&host).ok().as_deref() != Some(*bytes) {
                        return Err(format!("{}: could not be written", host.display()));
                    }
                }
            }
            // Libraries too: the image's own are 0644 (the linker maps them; nothing executes them).
            Ok(Materialized { guest: guest.as_bytes().to_vec(), entry: Entry::File { mode: 0o644, size: bytes.len() as u64, sha256 }, host })
        })
        .collect()
}
