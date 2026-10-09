//! **A gralloc buffer's frame shared as a GPU image between host processes** (`present_zero`,
//! design `docs/superpowers/specs/2026-10-09-gpu-present-design.md`, A1-A3).
//!
//! The app's host process (where its Vulkan device is) gives each image it binds to a gralloc buffer
//! a second, device-local image of the same size -- the **share image** -- whose memory is exported
//! as a **named** Win32 handle (`Local\omni-share-<pid>-<n>`, `VK_KHR_external_memory_win32`,
//! opaque). The system host process, which composes the display, opens it by that name: no handle
//! passed between the processes. At each release the GPU copies the frame into it (beside the copy
//! into the region, which every CPU reader still reads), and the region's metadata page says which
//! content generation the share image holds. What it is, and that generation, are in the metadata
//! page at [`SHARE_AT`] ([`ShareDesc`]).
//!
//! **The system host's side** (`hal::composer::present_shared`): a frame whose every visible layer
//! is a buffer layer holding its share image's generation, at plane alpha 1 and untransformed, is
//! drawn by the display window's GPU (`gpu::window_present::WindowPresenter::present_layers`: the
//! layers as textured quads over black, the composer's three blends as fixed-function blending),
//! and the framebuffer gets it without pixels -- composed on the CPU only when a screenshot or
//! another reader asks. Any other frame, or any failure, is composed on the CPU as before.
//!
//! MEASURED (Windows, RTX 4060, 1575x890, an opaque SurfaceView under a mostly transparent window,
//! 60 frames a second, the process's CPU per frame, E-cores at low priority beside a live game,
//! `window_present::zero_tests::zero_present_cost`): default path 7.20 ms, `compose_zero` +
//! `present_bgra` 3.30 ms, **`present_zero` 0.78 ms**; the copy a release adds is 0.057 ms of the
//! app's GPU. The GPU's frame against the CPU's (`zero_the_gpu_composes_share_images_as_the_cpu_does`):
//! exact where nothing blends, within 2 of 255 where a layer blends (float rounding against the
//! CPU's integer truncation); 2.2% of colour values differ.
//!
//! **Sync without semaphores**: the system host reads a share image only after the region's
//! content generation says the copy landed (`native::wait_written`, the composer's existing wait,
//! which waits for the copy's fence -- the share copy is in the same submit), and its own GPU read
//! is finished before its present returns; the app writes that buffer's share image again only
//! after SurfaceFlinger released the buffer back, which is after a later present.
use std::sync::atomic::{AtomicBool, Ordering};

use ash::vk;

use crate::shm::Shm;

/// Where in a gralloc region's metadata page the share image's description is: 200 bytes in the
/// page's reserved 3080..4079 (`device/src/mapper.c`'s layout; the generations follow at 4080).
pub const SHARE_AT: u64 = 3200;
const MAGIC: u32 = 0x5248_534f; // "OSHR"
const VERSION: u32 = 1;
/// The longest name (UTF-16 units, NUL excluded).
const NAME_UNITS: usize = 63;

/// **The lever** (`present_zero=0|1`): the app's process copies released frames into share images,
/// and the system's composer presents from them when it can. **Off by default.** The app's devices
/// must be made able to export (`OMNI_PRESENT_ZERO=ready`, or `=1`: ready and on).
pub static ZERO: AtomicBool = AtomicBool::new(false);

/// Whether devices are made able to export share images (`OMNI_PRESENT_ZERO=ready|1`, or the lever
/// already on); `=1` also turns [`ZERO`] on.
pub(crate) fn wanted() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let env = *ENV.get_or_init(|| match std::env::var("OMNI_PRESENT_ZERO").as_deref() {
        Ok("1") => {
            ZERO.store(true, Ordering::Relaxed);
            true
        }
        Ok("ready") => true,
        _ => false,
    });
    env || ZERO.load(Ordering::Relaxed)
}

/// Whether the lever is on now.
#[must_use]
pub fn on() -> bool {
    let _ = wanted();
    ZERO.load(Ordering::Relaxed)
}

/// The share image's format for a gralloc image of `format`: the 8-bit four-channel formats, as
/// UNORM (an sRGB image's bytes are copied as they are, and sampled as the bytes the CPU composes).
#[must_use]
pub fn share_format(format: vk::Format) -> Option<vk::Format> {
    match format {
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => Some(vk::Format::R8G8B8A8_UNORM),
        vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => Some(vk::Format::B8G8R8A8_UNORM),
        _ => None,
    }
}

/// The usage a share image is made with (the importer must make it the same).
pub const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(vk::ImageUsageFlags::TRANSFER_DST.as_raw() | vk::ImageUsageFlags::SAMPLED.as_raw());

/// What a share image is: enough for another process to make the same image and import its memory.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareDesc {
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    /// The exported allocation's size.
    pub size: u64,
    /// The GPU and driver it lives on (`VkPhysicalDeviceIDProperties`): the importer's must match.
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    pub name: String,
}

impl std::fmt::Debug for ShareDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShareDesc({} {}x{} VkFormat {} {} bytes)", self.name, self.width, self.height, self.format.as_raw(), self.size)
    }
}

impl ShareDesc {
    /// The bytes at [`SHARE_AT`] (the share generation, at +64, written apart).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 8 + (NAME_UNITS + 1) * 2];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&(self.format.as_raw() as u32).to_le_bytes());
        b[12..16].copy_from_slice(&self.width.to_le_bytes());
        b[16..20].copy_from_slice(&self.height.to_le_bytes());
        b[24..32].copy_from_slice(&self.size.to_le_bytes());
        b[32..48].copy_from_slice(&self.device_uuid);
        b[48..64].copy_from_slice(&self.driver_uuid);
        for (i, u) in self.name.encode_utf16().take(NAME_UNITS).enumerate() {
            b[72 + i * 2..74 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        b
    }

    /// The description in these bytes, if they hold one.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 72 + (NAME_UNITS + 1) * 2 {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().expect("4"));
        if u32_at(0) != MAGIC || u32_at(4) != VERSION {
            return None;
        }
        let units: Vec<u16> = (0..=NAME_UNITS).map(|i| u16::from_le_bytes([b[72 + i * 2], b[73 + i * 2]])).take_while(|&u| u != 0).collect();
        Some(Self {
            format: vk::Format::from_raw(u32_at(8) as i32),
            width: u32_at(12),
            height: u32_at(16),
            size: u64::from_le_bytes(b[24..32].try_into().expect("8")),
            device_uuid: b[32..48].try_into().expect("16"),
            driver_uuid: b[48..64].try_into().expect("16"),
            name: String::from_utf16(&units).ok()?,
        })
    }

    /// The name as a NUL-terminated UTF-16 string, for Vulkan's Win32 handle names.
    #[must_use]
    pub fn wide_name(&self) -> Vec<u16> {
        self.name.encode_utf16().take(NAME_UNITS).chain(std::iter::once(0)).collect()
    }

    /// Write it into `shm`'s metadata page.
    pub fn write(&self, shm: &Shm) {
        let _ = shm.write_at(&self.to_bytes(), SHARE_AT);
    }

    /// The one `shm`'s metadata page holds, if any.
    #[must_use]
    pub fn read(shm: &Shm) -> Option<Self> {
        let mut b = vec![0u8; 72 + (NAME_UNITS + 1) * 2];
        shm.read_at(&mut b, SHARE_AT).ok()?;
        Self::from_bytes(&b)
    }
}

/// Which content generation the share image holds (0: none, or stale).
pub fn write_generation(shm: &Shm, generation: u64) {
    let _ = shm.write_at(&generation.to_le_bytes(), SHARE_AT + 64);
}

/// See [`write_generation`].
#[must_use]
pub fn read_generation(shm: &Shm) -> u64 {
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, SHARE_AT + 64);
    u64::from_le_bytes(g)
}

/// How a layer's pixels combine with what is under them, as the composer's `Blend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareBlend {
    None,
    Premultiplied,
    Coverage,
}

/// One layer of a frame the window's GPU composes from share images (bottom first).
#[derive(Debug, Clone, PartialEq)]
pub struct ShareLayer {
    pub desc: ShareDesc,
    /// A format without alpha (RGBX): sampled with alpha 1.
    pub opaque: bool,
    /// The source crop, left, top, right, bottom, in the buffer's pixels.
    pub crop: (f32, f32, f32, f32),
    /// Where on the display, left, top, right, bottom.
    pub frame: (i32, i32, i32, i32),
    pub blend: ShareBlend,
    /// The content generation the share image holds: the reader takes the image over from the
    /// app's process (`QUEUE_FAMILY_EXTERNAL`) once per generation, as the app hands each over.
    pub generation: u64,
}

/// A name for the next share image of this process.
#[must_use]
pub fn next_name() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("Local\\omni-share-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_survives_the_metadata_page() {
        let d = ShareDesc { format: vk::Format::B8G8R8A8_UNORM, width: 1575, height: 890, size: 5_701_632, device_uuid: [7; 16], driver_uuid: [9; 16], name: next_name() };
        assert_eq!(ShareDesc::from_bytes(&d.to_bytes()), Some(d.clone()));
        assert!(ShareDesc::from_bytes(&vec![0u8; 400]).is_none(), "an empty page holds none");
        assert_eq!(d.wide_name().last(), Some(&0));
        assert!(share_format(vk::Format::R8G8B8A8_SRGB) == Some(vk::Format::R8G8B8A8_UNORM));
        assert!(share_format(vk::Format::R5G6B5_UNORM_PACK16).is_none());
        // In a region, beside the generation.
        let shm = Shm::create("share-desc").expect("region");
        shm.set_len(8192).expect("size");
        d.write(&shm);
        write_generation(&shm, 42);
        assert_eq!(ShareDesc::read(&shm), Some(d));
        assert_eq!(read_generation(&shm), 42);
    }
}
