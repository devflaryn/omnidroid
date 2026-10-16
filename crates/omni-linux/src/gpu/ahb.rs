//! `VK_ANDROID_external_memory_android_hardware_buffer`: a gralloc buffer (D2) imported as the
//! memory of an image -- how ANGLE makes an `EGLImage` of an `AHardwareBuffer`, which is how
//! SurfaceFlinger's RenderEngine samples every layer and renders its output.
//!
//! The host GPU cannot use the buffer's `shm` region itself, so the image is **linear and
//! host-visible**, a mirror of the region's pixels (D3a design, "Gralloc buffers on the host GPU"):
//! before a submit, a region whose content generation moved is copied in; after a submit, the
//! host waits for the queue and copies every GPU-writable mirror out, bumping the generation -- but
//! never an app's frame, which RenderEngine only samples ([`app_frame`]), nor over newer pixels.
use std::ffi::CStr;
use std::sync::Arc;

use ash::vk::{self, Handle};

use super::generated as g;
use super::native::{bump_generation, gralloc_buffer, CONTENT_GENERATION_AT};
use super::{entry_point, CallError, Gpu, Table};
use crate::process::Process;
use crate::shm::Shm;

type R<T> = Result<T, CallError>;

pub(crate) const HANDLE_TYPE_AHB: u32 = 0x400;
pub(crate) const STYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: u32 = 1_000_072_001;
const STYPE_AHB_FORMAT_PROPERTIES: u32 = 1_000_129_002;
const STYPE_MEMORY_DEDICATED_ALLOCATE_INFO: u32 = 1_000_127_001;

/// An image made to be backed by a gralloc buffer, before its memory is imported.
pub(crate) struct AhbImage {
    device: u64,
    format: vk::Format,
    extent: vk::Extent2D,
    writable: bool,
}

/// Imported memory: the mirror of a gralloc buffer.
pub(crate) struct AhbMemory {
    device: u64,
    image: u64,
    mapped: usize,
    row_pitch: u64,
    row_bytes: u64,
    height: u32,
    shm: Arc<Shm>,
    stride_bytes: u64,
    pixels_at: u64,
    /// The region's content generation this mirror holds.
    generation: u64,
    writable: bool,
}

macro_rules! vkfn {
    ($t:expr, $id:ident, $name:literal, $ty:ty) => {{
        const NAMES: &[&CStr] = &[$name];
        // SAFETY: `$ty` is the command's Vulkan signature (ash's PFN type for it).
        #[allow(unused_unsafe)]
        let f: $ty = unsafe { entry_point::<$ty>(&$t, g::$id, NAMES)? };
        f
    }};
}

fn check(r: vk::Result) -> R<()> {
    if r == vk::Result::SUCCESS { Ok(()) } else { Err(CallError::Host(r.as_raw())) }
}

/// The Vulkan format of a gralloc (AIDL `PixelFormat`) format, and its bytes per pixel.
fn format_of(pixel_format: i32) -> Option<(vk::Format, u32)> {
    Some(match pixel_format {
        1 | 2 | 0x22 => (vk::Format::R8G8B8A8_UNORM, 4),
        5 => (vk::Format::B8G8R8A8_UNORM, 4),
        4 => (vk::Format::R5G6B5_UNORM_PACK16, 2),
        0x16 => (vk::Format::R16G16B16A16_SFLOAT, 8),
        0x2b => (vk::Format::A2B10G10R10_UNORM_PACK32, 4),
        0x38 => (vk::Format::R8_UNORM, 1),
        _ => return None,
    })
}

fn rd_u32(p: &Process, at: u64) -> R<u32> {
    Ok(u32::from_le_bytes(p.mem.read(at, 4).map_err(|_| CallError::Args)?.try_into().expect("4")))
}

fn rd_u64(p: &Process, at: u64) -> R<u64> {
    p.mem.read_u64(at).map_err(|_| CallError::Args)
}

fn generation(shm: &Shm) -> u64 {
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, CONTENT_GENERATION_AT);
    u64::from_le_bytes(g)
}

/// `vkGetAndroidHardwareBufferPropertiesANDROID(device, handle, props)`; the guest passes the
/// buffer's `native_handle_t*` where the `AHardwareBuffer*` was.
pub(crate) fn properties(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (device, _) = gpu.dispatchable(p, a[0])?;
    let handle = a[1];
    let (shm, _, pixels_at) = gralloc_buffer(p, handle)?;
    let format = rd_u32(p, handle + 16 + 5 * 4)? as i32;
    let Some((vk_format, _)) = format_of(format) else { return Ok(u64::from(vk::Result::ERROR_INVALID_EXTERNAL_HANDLE.as_raw() as u32)) };
    let host_visible = {
        let devices = gpu.devices.lock();
        let info = devices.get(&device).ok_or(CallError::Handle(device))?;
        info.memory_types
            .iter()
            .enumerate()
            .filter(|(_, m)| m.property_flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT))
            .fold(0u32, |bits, (i, _)| bits | 1 << i)
    };
    // VkAndroidHardwareBufferPropertiesANDROID: allocationSize at 16, memoryTypeBits at 24. The
    // size is the region's pixels; an import sizes its memory by the image, whatever it is told.
    p.mem.write(a[2] + 16, &(shm.len().saturating_sub(pixels_at)).to_le_bytes()).map_err(|_| CallError::Args)?;
    p.mem.write(a[2] + 24, &host_visible.to_le_bytes()).map_err(|_| CallError::Args)?;
    let mut node = rd_u64(p, a[2] + 8)?;
    for _ in 0..16 {
        if node == 0 {
            break;
        }
        if rd_u32(p, node)? == STYPE_AHB_FORMAT_PROPERTIES {
            // format 16, externalFormat 24, formatFeatures 32, components 36..52, model 52,
            // range 56, x/y chroma offsets 60, 64.
            let features = vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::TRANSFER_SRC
                | vk::FormatFeatureFlags::TRANSFER_DST;
            let mut b = [0u8; 52];
            b[0..4].copy_from_slice(&vk_format.as_raw().to_le_bytes());
            b[16..20].copy_from_slice(&features.as_raw().to_le_bytes());
            p.mem.write(node + 16, &b).map_err(|_| CallError::Args)?;
        }
        node = rd_u64(p, node + 8)?;
    }
    Ok(0)
}

/// `vkCreateImage` of an image for a gralloc buffer (its `VkExternalMemoryImageCreateInfo` names
/// the AHB handle type, already out of the chain): made linear, to be a host-visible mirror.
pub(crate) fn create_image(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, info: &[u8], out: u64) -> R<u64> {
    // SAFETY: `info` is a whole VkImageCreateInfo (the caller read 88 bytes of one).
    let mut ci: vk::ImageCreateInfo<'static> = unsafe { std::ptr::read_unaligned(info.as_ptr().cast()) };
    ci.tiling = vk::ImageTiling::LINEAR;
    if std::env::var("OMNI_GPU_TRACE").as_deref() == Ok("1") {
        eprintln!(
            "[gpu] gralloc image: flags {:#x} format {} extent {}x{} mips {} layers {} samples {:#x} usage {:#x} layout {}",
            ci.flags.as_raw(), ci.format.as_raw(), ci.extent.width, ci.extent.height, ci.mip_levels, ci.array_layers, ci.samples.as_raw(), ci.usage.as_raw(), ci.initial_layout.as_raw()
        );
    }
    let writable = ci.usage.intersects(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::STORAGE);
    let create = vkfn!(t, ID_VK_CREATE_IMAGE, c"vkCreateImage", vk::PFN_vkCreateImage);
    let mut image = vk::Image::null();
    let mut r = unsafe { create(vk::Device::from_raw(device), &ci, std::ptr::null(), &mut image) };
    if r == vk::Result::ERROR_FORMAT_NOT_SUPPORTED {
        // A GPU's linear images are narrower than its optimal ones (NVIDIA's take neither input
        // attachments nor other-format views): what an EGLImage needs -- sampling, rendering,
        // copies -- is kept. Known gap: a shader reading the image as an input attachment, or a
        // view of it in another format (sRGB), is not served until mirrors are optimal images
        // synchronised at ANGLE's foreign-queue transfers.
        ci.usage &= !(vk::ImageUsageFlags::INPUT_ATTACHMENT | vk::ImageUsageFlags::STORAGE);
        ci.flags &= !(vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE);
        ci.p_next = std::ptr::null();
        r = unsafe { create(vk::Device::from_raw(device), &ci, std::ptr::null(), &mut image) };
    }
    if r == vk::Result::SUCCESS {
        gpu.ahb_images.lock().insert(image.as_raw(), AhbImage { device, format: ci.format, extent: vk::Extent2D { width: ci.extent.width, height: ci.extent.height }, writable });
        p.mem.write(out, &image.as_raw().to_le_bytes()).map_err(|_| CallError::Args)?;
    }
    Ok(u64::from(r.as_raw() as u32))
}

/// `vkAllocateMemory` importing a gralloc buffer (`VkImportAndroidHardwareBufferInfoANDROID`,
/// already out of the chain) for a dedicated image; the guest passes the buffer's
/// `native_handle_t*` in the allocator's place.
pub(crate) fn import(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, info: u64, handle: u64, out: u64) -> R<u64> {
    let (shm, stride, pixels_at) = gralloc_buffer(p, handle)?;
    let format = rd_u32(p, handle + 16 + 5 * 4)? as i32;
    let (_, bpp) = format_of(format).ok_or(CallError::Args)?;
    // The dedicated image this memory is for.
    let mut node = rd_u64(p, info + 8)?;
    let mut image = 0;
    for _ in 0..16 {
        if node == 0 {
            break;
        }
        if rd_u32(p, node)? == STYPE_MEMORY_DEDICATED_ALLOCATE_INFO {
            image = rd_u64(p, node + 16)?;
        }
        node = rd_u64(p, node + 8)?;
    }
    let (height, writable) = {
        let images = gpu.ahb_images.lock();
        let img = images.get(&image).ok_or(CallError::Args)?;
        if img.device != device || img.extent.width > stride {
            return Err(CallError::Args);
        }
        let _ = img.format;
        (img.extent.height, img.writable && !app_frame(&shm))
    };
    let d = vk::Device::from_raw(device);
    let mut req = vk::MemoryRequirements::default();
    unsafe { vkfn!(t, ID_VK_GET_IMAGE_MEMORY_REQUIREMENTS, c"vkGetImageMemoryRequirements", vk::PFN_vkGetImageMemoryRequirements)(d, vk::Image::from_raw(image), &mut req) };
    let memory_type_index = {
        let devices = gpu.devices.lock();
        let info = devices.get(&device).ok_or(CallError::Handle(device))?;
        let has = |i: u32, want: vk::MemoryPropertyFlags| req.memory_type_bits & (1 << i) != 0 && info.memory_types[i as usize].property_flags.contains(want);
        let coherent = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        // A mirror the GPU writes is read back by the CPU after each submit: host-cached memory,
        // which uncached (write-combined) memory reads at a fraction of the speed (`native`).
        let first = if writable { coherent | vk::MemoryPropertyFlags::HOST_CACHED } else { coherent };
        (0..info.memory_types.len() as u32)
            .find(|&i| has(i, first))
            .or_else(|| (0..info.memory_types.len() as u32).find(|&i| has(i, coherent)))
            .ok_or(CallError::Missing("host-visible memory for a linear image"))?
    };
    let dedicated = vk::MemoryDedicatedAllocateInfo { image: vk::Image::from_raw(image), ..Default::default() };
    let ai = vk::MemoryAllocateInfo { allocation_size: req.size, memory_type_index, p_next: std::ptr::from_ref(&dedicated).cast(), ..Default::default() };
    let mut memory = vk::DeviceMemory::null();
    check(unsafe { vkfn!(t, ID_VK_ALLOCATE_MEMORY, c"vkAllocateMemory", vk::PFN_vkAllocateMemory)(d, &ai, std::ptr::null(), &mut memory) })?;
    let mut mapped = std::ptr::null_mut();
    if let Err(e) = check(unsafe { vkfn!(t, ID_VK_MAP_MEMORY, c"vkMapMemory", vk::PFN_vkMapMemory)(d, memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty(), &mut mapped) }) {
        unsafe { vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, memory, std::ptr::null()) };
        return Err(e);
    }
    let sub = vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, array_layer: 0 };
    let mut layout = vk::SubresourceLayout::default();
    unsafe { vkfn!(t, ID_VK_GET_IMAGE_SUBRESOURCE_LAYOUT, c"vkGetImageSubresourceLayout", vk::PFN_vkGetImageSubresourceLayout)(d, vk::Image::from_raw(image), &sub, &mut layout) };
    let width = gpu.ahb_images.lock().get(&image).map_or(0, |i| i.extent.width);
    let mut m = AhbMemory {
        device,
        image,
        mapped: mapped as usize + layout.offset as usize,
        row_pitch: layout.row_pitch,
        row_bytes: u64::from(width) * u64::from(bpp),
        height,
        shm,
        stride_bytes: u64::from(stride) * u64::from(bpp),
        pixels_at,
        generation: u64::MAX,
        writable,
    };
    upload(&mut m);
    gpu.ahb_memory.lock().insert(memory.as_raw(), m);
    p.mem.write(out, &memory.as_raw().to_le_bytes()).map_err(|_| CallError::Args)?;
    Ok(0)
}

/// Copy the region's pixels into the mirror, if they changed since it last held them.
fn upload(m: &mut AhbMemory) {
    // A swapchain copy on its way into the region lands first (`native::wait_written`).
    super::native::wait_written(&m.shm, std::time::Duration::from_millis(50));
    // A region its release left stale (`region_lazy`) is filled from its share image first.
    super::share::ensure_region(&m.shm, m.stride_bytes);
    let now = generation(&m.shm);
    if now == m.generation {
        return;
    }
    let mut row = vec![0u8; m.row_bytes as usize];
    for y in 0..u64::from(m.height) {
        if m.shm.read_at(&mut row, m.pixels_at + y * m.stride_bytes).is_err() {
            return;
        }
        // SAFETY: the mirror's mapping holds `height` rows of `row_pitch` bytes (its subresource
        // layout), each at least `row_bytes` long.
        unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), (m.mapped + (y * m.row_pitch) as usize) as *mut u8, row.len()) };
    }
    m.generation = now;
}

/// Whether the buffer is an app's frame: one of a BLAST BufferQueue's buffers, which gralloc names
/// for its window (`VRI[<title>]#<n>(BLAST Consumer)<n>`, `SurfaceView[...](BLAST Consumer)<n>`).
/// Its producer writes it (through its swapchain's releases, `native::release`); SurfaceFlinger's
/// RenderEngine, which imports it here, only samples it -- although ANGLE makes every imported
/// image a colour attachment, so its usage alone says writable.
///
/// Such a mirror is never copied back. When it was, every RenderEngine submit wrote each app
/// frame's mirror -- what the buffer held when RenderEngine last sampled it -- over the region,
/// after the app's newer frame had landed or while it landed, and bumped the generation past it:
/// the composer, and RenderEngine's next upload, then took a frame from a turn of the buffer queue
/// before. Seen 2026-10-16 on macOS as frames going back and forth: Talking Tom Camp (an RGB_565
/// layer, so composed by SurfaceFlinger every frame) and, beside it, Super Mechs, whose buffers
/// RenderEngine had sampled for a few frames once (their generations rose by 4 a turn, not 1).
fn app_frame(shm: &Shm) -> bool {
    let mut name = [0u8; 128];
    if shm.read_at(&mut name, crate::hal::gralloc::NAME_AT).is_err() {
        return false;
    }
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    name.windows(16).any(|w| w == b"(BLAST Consumer)")
}

/// Copy the mirror's pixels out into the region, and bump its generation -- unless the region moved
/// on since the mirror last matched it (another writer's newer frame, which the mirror's older
/// pixels must not cover), or a copy into it is on its way.
fn download(m: &mut AhbMemory) {
    let pending = {
        let mut g = [0u8; 8];
        let _ = m.shm.read_at(&mut g, super::native::PENDING_GENERATION_AT);
        u64::from_le_bytes(g)
    };
    let now = generation(&m.shm);
    if now != m.generation || pending > now {
        return;
    }
    for y in 0..u64::from(m.height) {
        // SAFETY: as in `upload`.
        let row = unsafe { std::slice::from_raw_parts((m.mapped + (y * m.row_pitch) as usize) as *const u8, m.row_bytes as usize) };
        if m.shm.write_at(row, m.pixels_at + y * m.stride_bytes).is_err() {
            return;
        }
    }
    super::share::region_written(&m.shm);
    bump_generation(&m.shm);
    m.generation = generation(&m.shm);
}

/// Before a submit on `device`: every mirror current.
pub(crate) fn before_submit(gpu: &Gpu, device: u64) {
    for m in gpu.ahb_memory.lock().values_mut().filter(|m| m.device == device) {
        upload(m);
    }
}

/// After a submit on `queue` of `device`: if it has GPU-writable mirrors, wait for the queue and
/// copy them out.
pub(crate) fn after_submit(gpu: &Gpu, t: &Arc<Table>, device: u64, queue: u64) -> R<()> {
    if !gpu.ahb_memory.lock().values().any(|m| m.device == device && m.writable) {
        return Ok(());
    }
    check(unsafe { vkfn!(t, ID_VK_QUEUE_WAIT_IDLE, c"vkQueueWaitIdle", vk::PFN_vkQueueWaitIdle)(vk::Queue::from_raw(queue)) })?;
    for m in gpu.ahb_memory.lock().values_mut().filter(|m| m.device == device && m.writable) {
        download(m);
    }
    Ok(())
}

/// `vkFreeMemory` of imported memory: forget its mirror (the caller frees the memory).
pub(crate) fn forget_memory(gpu: &Gpu, memory: u64) {
    if let Some(m) = gpu.ahb_memory.lock().remove(&memory) {
        gpu.ahb_images.lock().remove(&m.image);
    }
}

/// A device is gone: its mirrors with it.
pub(crate) fn forget_device(gpu: &Gpu, device: u64) {
    gpu.ahb_memory.lock().retain(|_, m| m.device != device);
    gpu.ahb_images.lock().retain(|_, i| i.device != device);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::native::PENDING_GENERATION_AT;
    use crate::hal::gralloc::{NAME_AT, PIXELS_AT};

    /// A 4x2 RGBA region named `name`, at content generation `generation`, every byte 0x11.
    fn region(name: &str, generation: u64) -> Arc<Shm> {
        let shm = Shm::create("ahb_test").expect("region");
        shm.set_len(PIXELS_AT + 4 * 2 * 4).expect("size");
        let mut n = name.as_bytes().to_vec();
        n.push(0);
        shm.write_at(&n, NAME_AT).expect("name");
        shm.write_at(&generation.to_le_bytes(), CONTENT_GENERATION_AT).expect("generation");
        shm.write_at(&generation.to_le_bytes(), PENDING_GENERATION_AT).expect("generation");
        shm.write_at(&[0x11; 32], PIXELS_AT).expect("pixels");
        shm
    }

    /// A mirror of `shm` holding generation `generation`, its pixels `mirror` (every byte 0x22).
    fn mirror(shm: &Arc<Shm>, mirror: &mut [u8; 32], generation: u64) -> AhbMemory {
        AhbMemory {
            device: 1,
            image: 1,
            mapped: mirror.as_mut_ptr() as usize,
            row_pitch: 16,
            row_bytes: 16,
            height: 2,
            shm: Arc::clone(shm),
            stride_bytes: 16,
            pixels_at: PIXELS_AT,
            generation,
            writable: true,
        }
    }

    fn pixels(shm: &Shm) -> [u8; 32] {
        let mut p = [0u8; 32];
        shm.read_at(&mut p, PIXELS_AT).expect("pixels");
        p
    }

    #[test]
    fn an_apps_buffer_queue_frame_is_one_renderengine_only_samples() {
        for name in ["VRI[MainActivity]#0(BLAST Consumer)0", "SurfaceView[com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity]#1(BLAST Consumer)1"] {
            assert!(app_frame(&region(name, 1)), "{name}");
        }
        for name in ["FramebufferSurface", "ScreenCapture", ""] {
            assert!(!app_frame(&region(name, 1)), "{name:?}");
        }
    }

    #[test]
    fn a_mirror_goes_back_only_over_the_pixels_it_was_made_from() {
        // In step with the region: the GPU's pixels go out, and the generation moves on.
        let shm = region("FramebufferSurface", 5);
        let mut px = [0x22u8; 32];
        let mut m = mirror(&shm, &mut px, 5);
        download(&mut m);
        assert_eq!((pixels(&shm), generation(&shm), m.generation), ([0x22; 32], 6, 6));

        // The region holds a newer frame (another writer's): the mirror's older pixels stay out.
        let shm = region("FramebufferSurface", 7);
        let mut m = mirror(&shm, &mut px, 5);
        download(&mut m);
        assert_eq!((pixels(&shm), generation(&shm)), ([0x11; 32], 7));

        // A copy into the region is on its way: not covered either.
        let shm = region("FramebufferSurface", 5);
        shm.write_at(&6u64.to_le_bytes(), PENDING_GENERATION_AT).expect("pending");
        let mut m = mirror(&shm, &mut px, 5);
        download(&mut m);
        assert_eq!((pixels(&shm), generation(&shm)), ([0x11; 32], 5));
    }
}
