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
/// and the system's composer presents from them when it can. **On by default** since the in-world
/// A/B of 2026-10-09 (PS99 session s6: the system host -1.0 ms of CPU a frame, 5/6 pairs; the
/// window checked by eye against the CPU path). `OMNI_PRESENT_ZERO=0` makes devices that cannot
/// export and keeps it off; `=ready` makes them able to export with the lever off.
pub static ZERO: AtomicBool = AtomicBool::new(false);

/// Whether devices are made able to export share images (`OMNI_PRESENT_ZERO=ready|1`, or the lever
/// already on); `=1` also turns [`ZERO`] on.
pub(crate) fn wanted() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let env = *ENV.get_or_init(|| match std::env::var("OMNI_PRESENT_ZERO").as_deref() {
        Ok("0") => false,
        Ok("ready") => true,
        // Unset or `1`: ready and on.
        _ => {
            ZERO.store(true, Ordering::Relaxed);
            true
        }
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

// ------------------------------------------------------------------------------- region_lazy
//
// **The region copied only when someone needs it** (`region_lazy=0|1`, `OMNI_REGION_LAZY=1`; **off
// by default**). Under `present_zero` the window shows the share image, and the release's other copy
// -- GPU image -> staging -> the worker's CPU copy into the gralloc region -- feeds only readers that
// may never come. So a buffer whose last frame the composer took from its share image ([`TAKEN_AT`]
// = that frame's generation) skips the region copy at its next release and says so ([`STALE_AT`] =
// the generation the region lacks); a buffer anything else consumed keeps being copied. Every host
// reader of a region's pixels -- the composer's CPU paths and the screenshot's on-demand
// composition, SurfaceFlinger's client target, the AHB mirrors (SurfaceFlinger's RenderEngine,
// `screencap`), the GL backend's EGLImages -- first calls [`ensure_region`], which fills a stale
// region from its share image on the GPU (a read-back, on demand) and clears the mark; whatever
// else writes a region whole clears it too ([`region_written`]).
//
// MEASURED (`window_present::zero_tests::zero_region_lazy`, RTX 4060, 1575x890 at stride 1600,
// E-cores beside a live game, two runs): the release worker's thread per frame 0.93-0.98 ms of CPU
// and 1.47-1.53 ms from submit to landed with the region copy; 0.006 ms and 0.09 ms without. A
// fill on demand: 2.3-4 ms (18 ms seen under load), the first in a process 50-110 ms (its Vulkan
// device made).

/// The generation a region's pixels lack (0: they are current): written by the release that
/// skipped the region copy, cleared by whoever fills it ([`ensure_region`]).
pub const STALE_AT: u64 = SHARE_AT + 200;
/// The generation the composer last showed from this buffer's share image.
pub const TAKEN_AT: u64 = SHARE_AT + 208;

/// The lever (`region_lazy`), read by the app's release.
pub static LAZY: AtomicBool = AtomicBool::new(false);

/// Whether releases may skip the region copy (the lever, or `OMNI_REGION_LAZY=1` from the start).
#[must_use]
pub fn lazy_on() -> bool {
    static ENV: std::sync::Once = std::sync::Once::new();
    ENV.call_once(|| {
        if std::env::var("OMNI_REGION_LAZY").as_deref() == Ok("1") {
            LAZY.store(true, Ordering::Relaxed);
        }
    });
    LAZY.load(Ordering::Relaxed)
}

fn read_u64(shm: &Shm, at: u64) -> u64 {
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, at);
    u64::from_le_bytes(g)
}

/// The composer showed this buffer's frame of `generation` from its share image.
pub fn mark_taken(shm: &Shm, generation: u64) {
    let _ = shm.write_at(&generation.to_le_bytes(), TAKEN_AT);
}

/// See [`mark_taken`].
#[must_use]
pub fn taken(shm: &Shm) -> u64 {
    read_u64(shm, TAKEN_AT)
}

/// The region lacks the frame of `generation` (0: it holds its frame).
pub fn set_stale(shm: &Shm, generation: u64) {
    let _ = shm.write_at(&generation.to_le_bytes(), STALE_AT);
}

/// See [`set_stale`].
#[must_use]
pub fn stale(shm: &Shm) -> u64 {
    read_u64(shm, STALE_AT)
}

/// Something other than a release wrote the region's pixels whole (an AHB mirror's download, the GL
/// backend's frame): they are current, and must not be filled over from an older share image.
pub fn region_written(shm: &Shm) {
    if stale(shm) != 0 {
        set_stale(shm, 0);
    }
}

/// Regions filled on demand in this process, for the log.
static FILLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// **Make a region's pixels current before reading them** (`region_lazy`): a region whose last
/// release skipped the copy is filled from its share image (imported here and read back on the GPU,
/// rows `stride_bytes` apart as the region lays them out). False when it could not be (no share
/// image holding that frame, no GPU here): the region then holds an older frame of the buffer.
pub fn ensure_region(shm: &Shm, stride_bytes: u64) -> bool {
    if stale(shm) == 0 {
        return true;
    }
    // The release's copy into the share image lands first.
    super::native::wait_written(shm, std::time::Duration::from_millis(50));
    let wanted = stale(shm);
    if wanted == 0 {
        return true;
    }
    // Whatever happens below, a region that could not be filled is copied again from the next
    // release on (the composer's mark is the release's only reason to skip it).
    let give_up = || {
        mark_taken(shm, 0);
        false
    };
    let Some(desc) = ShareDesc::read(shm) else { return give_up() };
    if read_generation(shm) != wanted {
        return give_up();
    }
    static FILLER: parking_lot::Mutex<Option<Result<super::window_present::WindowPresenter, String>>> = parking_lot::Mutex::new(None);
    let mut filler = FILLER.lock();
    let filler = filler.get_or_insert_with(super::window_present::WindowPresenter::new_headless);
    let Ok(filler) = filler.as_mut() else { return give_up() };
    let started = std::time::Instant::now();
    match filler.read_share(&desc, wanted, stride_bytes) {
        Ok(pixels) => {
            let _ = shm.write_at(&pixels, crate::hal::gralloc::PIXELS_AT);
            // Clear the mark only if no newer release set it meanwhile.
            shm.cas_u64(STALE_AT, wanted, 0);
            let n = FILLS.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 100 == 0 {
                eprintln!("[gpu] region_lazy: a region filled from its share image on demand ({:.2} ms; {n} so far in this process)", started.elapsed().as_secs_f64() * 1000.0);
            }
            true
        }
        Err(e) => {
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                eprintln!("[gpu] region_lazy: a stale region could not be filled from its share image ({e})");
            }
            give_up()
        }
    }
}

/// A name for the next share image of this process: its pid, a number this process drew when it
/// started, and a count. The pid alone named a dead process's images again in a new process that
/// Windows gave the same pid -- while the system host still held the old ones open by those names
/// (it keeps an image it is not drawing for `KEEP_IMPORTED` composed frames, and composes none
/// while no app draws), and it looks an image up by name: the new app's frames would have been
/// shown from the old app's memory, or the new export refused.
#[must_use]
pub fn next_name() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("Local\\omni-share-{}-{:08x}-{}", std::process::id(), process_nonce(), N.fetch_add(1, Ordering::Relaxed))
}

/// A number drawn once per process (the clock's nanoseconds, mixed with a stack address).
fn process_nonce() -> u32 {
    static NONCE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *NONCE.get_or_init(|| {
        let local = 0u8;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let mixed = (nanos ^ (std::ptr::addr_of!(local) as u64).rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> 32) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A share image's name says which process *instance* made it, not only its pid (which Windows
    /// gives again to a later process), and fits a description's 63 units at the longest.
    #[test]
    fn a_name_carries_the_process_s_own_number() {
        let (a, b) = (next_name(), next_name());
        assert_ne!(a, b);
        let parts = |n: &str| n.rsplitn(3, '-').map(str::to_string).collect::<Vec<_>>();
        let (pa, pb) = (parts(&a), parts(&b));
        assert_eq!(pa[1], pb[1], "one number per process");
        assert_eq!(pa[1].len(), 8, "{a}");
        assert!(a.starts_with(&format!("Local\\omni-share-{}-", std::process::id())), "{a}");
        let longest = format!("Local\\omni-share-{}-{:08x}-{}", u32::MAX, u32::MAX, u64::MAX);
        assert!(longest.encode_utf16().count() <= NAME_UNITS, "{longest}");
    }

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
