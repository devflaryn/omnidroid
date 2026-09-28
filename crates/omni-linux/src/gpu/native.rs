//! `VK_ANDROID_native_buffer`: the images the Android loader's swapchain makes on gralloc buffers
//! (D2), whose pixels must end up in the buffer's `shm` region for the buffer's consumer
//! (SurfaceFlinger, the composer, a screenshot) to see them.
//!
//! Such an image is an ordinary image of the host's GPU. When the loader releases it to the window
//! (`vkQueueSignalReleaseImageANDROID`, the image then in `PRESENT_SRC_KHR`), it is copied through a
//! staging buffer into the region, row for row at the buffer's stride, and the region's content
//! generation is bumped (D3a design, "Gralloc buffers on the host GPU").
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Arc;

use ash::vk::{self, Handle};

use super::generated as g;
use super::{entry_point, CallError, Gpu, Table};
use crate::fd::FileKind;
use crate::process::Process;
use crate::shm::Shm;

type R<T> = Result<T, CallError>;

/// The gralloc handle's ints (`hal::gralloc`): magic, then ... the stride at 8 and the pixels'
/// offset at 13.
const HANDLE_MAGIC: u32 = 0x4247_4d4f;
const HANDLE_INTS: u32 = 14;
/// Where a region's content generation is (`hal::gralloc`, `mapper.c`).
pub(crate) const CONTENT_GENERATION_AT: u64 = 4088;

/// What the host keeps of a device for the Android extensions: its physical device's memory
/// types, the queues it handed out, and a copier per queue family.
pub(crate) struct DeviceInfo {
    pub memory_types: Vec<vk::MemoryType>,
    /// Each queue the guest got, and its family.
    pub queues: HashMap<u64, u32>,
    copiers: HashMap<u32, Copier>,
}

impl DeviceInfo {
    pub(crate) fn new(memory: &vk::PhysicalDeviceMemoryProperties) -> Self {
        Self { memory_types: memory.memory_types[..memory.memory_type_count as usize].to_vec(), queues: HashMap::new(), copiers: HashMap::new() }
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> R<u32> {
        (0..self.memory_types.len() as u32)
            .find(|&i| bits & (1 << i) != 0 && self.memory_types[i as usize].property_flags.contains(want))
            .ok_or(CallError::Missing("a memory type"))
    }
}

/// A command buffer and fence for the host's own copies on one queue family.
struct Copier {
    cb: vk::CommandBuffer,
    fence: vk::Fence,
}

/// An image on a gralloc buffer, and the staging buffer its pixels leave through.
pub(crate) struct NativeImage {
    device: u64,
    memory: vk::DeviceMemory,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    mapped: usize,
    /// The staging memory is not host-coherent: invalidated before the CPU reads it.
    invalidate: bool,
    bytes: usize,
    width: u32,
    height: u32,
    stride: u32,
    shm: Arc<Shm>,
    pixels_at: u64,
}

impl NativeImage {
    pub(crate) fn belongs_to(&self, device: u64) -> bool {
        self.device == device
    }
}

/// Bytes per pixel of the formats gralloc buffers have.
fn bytes_per_pixel(format: vk::Format) -> Option<u32> {
    match format {
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB | vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB | vk::Format::A2B10G10R10_UNORM_PACK32 => Some(4),
        vk::Format::R5G6B5_UNORM_PACK16 => Some(2),
        vk::Format::R16G16B16A16_SFLOAT => Some(8),
        vk::Format::R8_UNORM => Some(1),
        _ => None,
    }
}

fn rd_u32(p: &Process, at: u64) -> R<u32> {
    Ok(u32::from_le_bytes(p.mem.read(at, 4).map_err(|_| CallError::Args)?.try_into().expect("4")))
}

/// The gralloc buffer a guest `native_handle_t*` names: its region, stride and pixel offset.
pub(crate) fn gralloc_buffer(p: &Process, handle: u64) -> R<(Arc<Shm>, u32, u64)> {
    let (num_fds, num_ints) = (rd_u32(p, handle + 4)?, rd_u32(p, handle + 8)?);
    if num_fds != 1 || num_ints != HANDLE_INTS {
        return Err(CallError::Args);
    }
    let int = |i: u64| rd_u32(p, handle + 16 + i * 4);
    if int(0)? != HANDLE_MAGIC {
        return Err(CallError::Args);
    }
    let fd = rd_u32(p, handle + 12)? as i32;
    let file = p.fds.get(fd).map_err(|_| CallError::Args)?;
    let shm = match &*file.kind.lock() {
        FileKind::Shared(m) => Arc::clone(m),
        _ => return Err(CallError::Args),
    };
    shm.as_graphics_buffer();
    Ok((shm, int(8)?, u64::from(int(13)?)))
}

/// Bump a region's content generation: its pixels changed.
pub(crate) fn bump_generation(shm: &Shm) {
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, CONTENT_GENERATION_AT);
    let _ = shm.write_at(&(u64::from_le_bytes(g).wrapping_add(1)).to_le_bytes(), CONTENT_GENERATION_AT);
}

macro_rules! vkfn {
    ($t:expr, $id:ident, $name:literal, $ty:ty) => {{
        const NAMES: &[&CStr] = &[$name];
        // SAFETY: `$ty` is the command's Vulkan signature (ash's PFN type for it). (A call site
        // may already be in an `unsafe` block.)
        #[allow(unused_unsafe)]
        let f: $ty = unsafe { entry_point::<$ty>(&$t, g::$id, NAMES)? };
        f
    }};
}

fn check(r: vk::Result) -> R<()> {
    if r == vk::Result::SUCCESS { Ok(()) } else { Err(CallError::Host(r.as_raw())) }
}

/// `vkCreateImage` with a `VkNativeBufferANDROID` (at `native`, already out of the guest's chain):
/// the image, its memory, and its staging buffer. The `VkImageCreateInfo` (88 bytes) is `info`,
/// its chain what the host may see.
pub(crate) fn create_image(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, info: &[u8], native: u64) -> R<u64> {
    let (image, format, width, height) = create_for_swapchain(device, t, info)?;
    match attach(gpu, p, device, t, image, format, width, height, native) {
        Ok(n) => {
            gpu.native.lock().insert(image.as_raw(), n);
            Ok(image.as_raw())
        }
        Err(e) => {
            unsafe { vkfn!(t, ID_VK_DESTROY_IMAGE, c"vkDestroyImage", vk::PFN_vkDestroyImage)(vk::Device::from_raw(device), image, std::ptr::null()) };
            Err(e)
        }
    }
}

/// The image of a swapchain (its `VkImageSwapchainCreateInfoKHR` names the loader's swapchain, out
/// of the chain): made now, bound to its gralloc buffer when the loader binds it
/// (`vkBindImageMemory2` with a `VkNativeBufferANDROID`, spec version 8).
pub(crate) fn create_swapchain_image(gpu: &Gpu, device: u64, t: &Arc<Table>, info: &[u8]) -> R<u64> {
    let (image, format, width, height) = create_for_swapchain(device, t, info)?;
    gpu.swapchain_images.lock().insert(image.as_raw(), (format, width, height));
    Ok(image.as_raw())
}

/// `vkBindImageMemory2`'s `VkBindImageMemoryInfo` of a swapchain image with a
/// `VkNativeBufferANDROID` at `native`: the image gets its memory and staging buffer.
pub(crate) fn bind_swapchain_image(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, image: u64, native: u64) -> R<()> {
    let (format, width, height) = gpu.swapchain_images.lock().remove(&image).ok_or(CallError::Args)?;
    let n = attach(gpu, p, device, t, vk::Image::from_raw(image), format, width, height, native)?;
    gpu.native.lock().insert(image, n);
    Ok(())
}

/// An image to hold a gralloc buffer's pixels: as the guest described it, and a copy source.
fn create_for_swapchain(device: u64, t: &Arc<Table>, info: &[u8]) -> R<(vk::Image, vk::Format, u32, u32)> {
    // SAFETY: `info` is a whole VkImageCreateInfo (the caller read 88 bytes of one).
    let mut ci: vk::ImageCreateInfo<'static> = unsafe { std::ptr::read_unaligned(info.as_ptr().cast()) };
    ci.usage |= vk::ImageUsageFlags::TRANSFER_SRC;
    bytes_per_pixel(ci.format).ok_or(CallError::Args)?;
    let mut image = vk::Image::null();
    check(unsafe { vkfn!(t, ID_VK_CREATE_IMAGE, c"vkCreateImage", vk::PFN_vkCreateImage)(vk::Device::from_raw(device), &ci, std::ptr::null(), &mut image) })?;
    Ok((image, ci.format, ci.extent.width, ci.extent.height))
}

/// Give `image` memory of its own and a staging buffer its pixels leave through into the gralloc
/// buffer a `VkNativeBufferANDROID` (at `native`) names.
#[allow(clippy::too_many_arguments)]
fn attach(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, image: vk::Image, format: vk::Format, width: u32, height: u32, native: u64) -> R<NativeImage> {
    let handle = p.mem.read_u64(native + 16).map_err(|_| CallError::Args)?;
    let (shm, stride, pixels_at) = gralloc_buffer(p, handle)?;
    let bpp = bytes_per_pixel(format).ok_or(CallError::Args)?;
    if stride < width || u64::from(stride) * u64::from(height) * u64::from(bpp) > shm.len().saturating_sub(pixels_at) {
        return Err(CallError::Args);
    }
    let d = vk::Device::from_raw(device);
    {
        let mut req = vk::MemoryRequirements::default();
        unsafe { vkfn!(t, ID_VK_GET_IMAGE_MEMORY_REQUIREMENTS, c"vkGetImageMemoryRequirements", vk::PFN_vkGetImageMemoryRequirements)(d, image, &mut req) };
        let devices = gpu.devices.lock();
        let info = devices.get(&device).ok_or(CallError::Handle(device))?;
        let memory = allocate(t, d, req.size, info.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).or_else(|_| info.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty()))?)?;
        check(unsafe { vkfn!(t, ID_VK_BIND_IMAGE_MEMORY, c"vkBindImageMemory", vk::PFN_vkBindImageMemory)(d, image, memory, 0) })?;
        let bytes = stride as usize * height as usize * bpp as usize;
        let bci = vk::BufferCreateInfo { size: bytes as u64, usage: vk::BufferUsageFlags::TRANSFER_DST, ..Default::default() };
        let mut staging = vk::Buffer::null();
        check(unsafe { vkfn!(t, ID_VK_CREATE_BUFFER, c"vkCreateBuffer", vk::PFN_vkCreateBuffer)(d, &bci, std::ptr::null(), &mut staging) })?;
        unsafe { vkfn!(t, ID_VK_GET_BUFFER_MEMORY_REQUIREMENTS, c"vkGetBufferMemoryRequirements", vk::PFN_vkGetBufferMemoryRequirements)(d, staging, &mut req) };
        // The CPU reads every frame out of it: host-cached memory. Uncached (write-combined)
        // host-visible memory -- what `HOST_VISIBLE | HOST_COHERENT` alone finds first on NVIDIA --
        // reads at a fraction of memory speed: MEASURED (Windows, RTX 4060, 2026-09-28), a 1280x720
        // frame took 13-15 ms to copy out of it into a mapped region, against ~1 ms for the copy.
        let (visible, coherent, cached) = (vk::MemoryPropertyFlags::HOST_VISIBLE, vk::MemoryPropertyFlags::HOST_COHERENT, vk::MemoryPropertyFlags::HOST_CACHED);
        let (staging_type, invalidate) = match info.memory_type(req.memory_type_bits, visible | coherent | cached) {
            Ok(i) => (i, false),
            Err(_) => match info.memory_type(req.memory_type_bits, visible | cached) {
                Ok(i) => (i, true),
                Err(_) => (info.memory_type(req.memory_type_bits, visible | coherent)?, false),
            },
        };
        let staging_memory = allocate(t, d, req.size, staging_type)?;
        drop(devices);
        check(unsafe { vkfn!(t, ID_VK_BIND_BUFFER_MEMORY, c"vkBindBufferMemory", vk::PFN_vkBindBufferMemory)(d, staging, staging_memory, 0) })?;
        let mut mapped = std::ptr::null_mut();
        check(unsafe { vkfn!(t, ID_VK_MAP_MEMORY, c"vkMapMemory", vk::PFN_vkMapMemory)(d, staging_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty(), &mut mapped) })?;
        Ok(NativeImage { device, memory, staging, staging_memory, mapped: mapped as usize, invalidate, bytes, width, height, stride, shm, pixels_at })
    }
}

fn allocate(t: &Arc<Table>, d: vk::Device, size: u64, memory_type_index: u32) -> R<vk::DeviceMemory> {
    let ai = vk::MemoryAllocateInfo { allocation_size: size, memory_type_index, ..Default::default() };
    let mut m = vk::DeviceMemory::null();
    check(unsafe { vkfn!(t, ID_VK_ALLOCATE_MEMORY, c"vkAllocateMemory", vk::PFN_vkAllocateMemory)(d, &ai, std::ptr::null(), &mut m) })?;
    Ok(m)
}

/// `vkDestroyImage` of a native image: it, its memory and its staging buffer. False when `image`
/// is not one.
pub(crate) fn destroy_image(gpu: &Gpu, t: &Arc<Table>, image: u64) -> R<bool> {
    let Some(n) = gpu.native.lock().remove(&image) else { return Ok(false) };
    let d = vk::Device::from_raw(n.device);
    unsafe {
        vkfn!(t, ID_VK_DESTROY_IMAGE, c"vkDestroyImage", vk::PFN_vkDestroyImage)(d, vk::Image::from_raw(image), std::ptr::null());
        vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, n.memory, std::ptr::null());
        vkfn!(t, ID_VK_DESTROY_BUFFER, c"vkDestroyBuffer", vk::PFN_vkDestroyBuffer)(d, n.staging, std::ptr::null());
        vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, n.staging_memory, std::ptr::null());
    }
    Ok(true)
}

/// A queue of `device` the guest has, with its family.
fn any_queue(gpu: &Gpu, device: u64) -> R<(u64, u32)> {
    gpu.devices.lock().get(&device).and_then(|i| i.queues.iter().next().map(|(q, f)| (*q, *f))).ok_or(CallError::Missing("a queue"))
}

/// Submit nothing on `queue`, waiting for `waits` and signalling `signal`/`fence`.
fn submit_empty(t: &Arc<Table>, queue: u64, waits: &[vk::Semaphore], signal: vk::Semaphore, fence: vk::Fence) -> R<()> {
    let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
    let si = vk::SubmitInfo {
        wait_semaphore_count: waits.len() as u32,
        p_wait_semaphores: waits.as_ptr(),
        p_wait_dst_stage_mask: stages.as_ptr(),
        signal_semaphore_count: u32::from(signal != vk::Semaphore::null()),
        p_signal_semaphores: &signal,
        ..Default::default()
    };
    check(unsafe { vkfn!(t, ID_VK_QUEUE_SUBMIT, c"vkQueueSubmit", vk::PFN_vkQueueSubmit)(vk::Queue::from_raw(queue), 1, &si, fence) })
}

/// `vkAcquireImageANDROID(device, image, nativeFenceFd, semaphore, fence)`.
pub(crate) fn acquire(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (device, t) = gpu.dispatchable(p, a[0])?;
    let fd = a[2] as i32;
    if fd >= 0 {
        // The fence of whoever released the buffer: every fence here is signalled when it is made
        // (composition is synchronous), so there is nothing to wait for; it is ours to close.
        let _ = p.fds.remove(fd);
    }
    let (semaphore, fence) = (vk::Semaphore::from_raw(a[3]), vk::Fence::from_raw(a[4]));
    if semaphore != vk::Semaphore::null() || fence != vk::Fence::null() {
        let (queue, _) = any_queue(gpu, device)?;
        submit_empty(&t, queue, &[], semaphore, fence)?;
    }
    Ok(0)
}

/// `vkQueueSignalReleaseImageANDROID(queue, waitCount, pWaits, image, pNativeFenceFd)`: the
/// image's pixels, once its rendering is done, into the gralloc buffer.
pub(crate) fn release(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (queue, t) = gpu.dispatchable(p, a[0])?;
    let n = a[1] as u32;
    if n > 64 {
        return Err(CallError::Args);
    }
    let waits: Vec<vk::Semaphore> = (0..u64::from(n)).map(|i| p.mem.read_u64(a[2] + i * 8).map(vk::Semaphore::from_raw).map_err(|_| CallError::Args)).collect::<R<_>>()?;
    let image = a[3];
    let natives = gpu.native.lock();
    let Some(img) = natives.get(&image) else {
        return Err(CallError::Args);
    };
    let (device, width, height, stride, staging) = (img.device, img.width, img.height, img.stride, img.staging);
    let (mapped, bytes, shm, pixels_at) = (img.mapped, img.bytes, Arc::clone(&img.shm), img.pixels_at);
    let (invalidate, staging_memory) = (img.invalidate, img.staging_memory);
    drop(natives);
    let d = vk::Device::from_raw(device);
    let mut devices = gpu.devices.lock();
    let info = devices.get_mut(&device).ok_or(CallError::Handle(device))?;
    let family = *info.queues.get(&queue).ok_or(CallError::Handle(queue))?;
    if let std::collections::hash_map::Entry::Vacant(slot) = info.copiers.entry(family) {
        let pci = vk::CommandPoolCreateInfo { flags: vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER, queue_family_index: family, ..Default::default() };
        let mut pool = vk::CommandPool::null();
        check(unsafe { vkfn!(t, ID_VK_CREATE_COMMAND_POOL, c"vkCreateCommandPool", vk::PFN_vkCreateCommandPool)(d, &pci, std::ptr::null(), &mut pool) })?;
        let cai = vk::CommandBufferAllocateInfo { command_pool: pool, level: vk::CommandBufferLevel::PRIMARY, command_buffer_count: 1, ..Default::default() };
        let mut cb = vk::CommandBuffer::null();
        check(unsafe { vkfn!(t, ID_VK_ALLOCATE_COMMAND_BUFFERS, c"vkAllocateCommandBuffers", vk::PFN_vkAllocateCommandBuffers)(d, &cai, &mut cb) })?;
        // A host command buffer is dispatchable: the host's loader wrote its dispatch into it.
        let mut fence = vk::Fence::null();
        let fci = vk::FenceCreateInfo::default();
        check(unsafe { vkfn!(t, ID_VK_CREATE_FENCE, c"vkCreateFence", vk::PFN_vkCreateFence)(d, &fci, std::ptr::null(), &mut fence) })?;
        slot.insert(Copier { cb, fence });
    }
    let copier = info.copiers.get(&family).expect("made above");
    let (cb, fence) = (copier.cb, copier.fence);
    // The copier is this device's, and the lock is held until its fence has signalled.
    let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
    let to_src = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::MEMORY_WRITE,
        dst_access_mask: vk::AccessFlags::TRANSFER_READ,
        old_layout: vk::ImageLayout::PRESENT_SRC_KHR,
        new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        image: vk::Image::from_raw(image),
        subresource_range: range,
        ..Default::default()
    };
    let back = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::TRANSFER_READ,
        dst_access_mask: vk::AccessFlags::empty(),
        old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        new_layout: vk::ImageLayout::PRESENT_SRC_KHR,
        ..to_src
    };
    let copy = vk::BufferImageCopy {
        buffer_offset: 0,
        buffer_row_length: stride,
        buffer_image_height: height,
        image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 },
        image_offset: vk::Offset3D::default(),
        image_extent: vk::Extent3D { width, height, depth: 1 },
    };
    unsafe {
        check(vkfn!(t, ID_VK_RESET_COMMAND_BUFFER, c"vkResetCommandBuffer", vk::PFN_vkResetCommandBuffer)(cb, vk::CommandBufferResetFlags::empty()))?;
        let bi = vk::CommandBufferBeginInfo { flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT, ..Default::default() };
        check(vkfn!(t, ID_VK_BEGIN_COMMAND_BUFFER, c"vkBeginCommandBuffer", vk::PFN_vkBeginCommandBuffer)(cb, &bi))?;
        let barrier = vkfn!(t, ID_VK_CMD_PIPELINE_BARRIER, c"vkCmdPipelineBarrier", vk::PFN_vkCmdPipelineBarrier);
        barrier(cb, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), 0, std::ptr::null(), 0, std::ptr::null(), 1, &to_src);
        vkfn!(t, ID_VK_CMD_COPY_IMAGE_TO_BUFFER, c"vkCmdCopyImageToBuffer", vk::PFN_vkCmdCopyImageToBuffer)(
            cb,
            vk::Image::from_raw(image),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            staging,
            1,
            &copy,
        );
        barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), 0, std::ptr::null(), 0, std::ptr::null(), 1, &back);
        check(vkfn!(t, ID_VK_END_COMMAND_BUFFER, c"vkEndCommandBuffer", vk::PFN_vkEndCommandBuffer)(cb))?;
        check(vkfn!(t, ID_VK_RESET_FENCES, c"vkResetFences", vk::PFN_vkResetFences)(d, 1, &fence))?;
        let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
        let si = vk::SubmitInfo {
            wait_semaphore_count: waits.len() as u32,
            p_wait_semaphores: waits.as_ptr(),
            p_wait_dst_stage_mask: stages.as_ptr(),
            command_buffer_count: 1,
            p_command_buffers: &cb,
            ..Default::default()
        };
        check(vkfn!(t, ID_VK_QUEUE_SUBMIT, c"vkQueueSubmit", vk::PFN_vkQueueSubmit)(vk::Queue::from_raw(queue), 1, &si, fence))?;
        let waited = std::time::Instant::now();
        check(vkfn!(t, ID_VK_WAIT_FOR_FENCES, c"vkWaitForFences", vk::PFN_vkWaitForFences)(d, 1, &fence, vk::TRUE, u64::MAX))?;
        if super::stats::enabled() {
            super::stats::add(super::special::ID_GRALLOC_USAGE + 6, waited.elapsed());
        }
        if invalidate {
            let range = vk::MappedMemoryRange { memory: staging_memory, offset: 0, size: vk::WHOLE_SIZE, ..Default::default() };
            check(vkfn!(t, ID_VK_INVALIDATE_MAPPED_MEMORY_RANGES, c"vkInvalidateMappedMemoryRanges", vk::PFN_vkInvalidateMappedMemoryRanges)(d, 1, &range))?;
        }
    }
    drop(devices);
    let written = std::time::Instant::now();
    // SAFETY: `mapped` is the staging memory's host mapping, `bytes` long, written by the copy the
    // fence has just seen finish.
    let pixels = unsafe { std::slice::from_raw_parts(mapped as *const u8, bytes) };
    shm.write_at(pixels, pixels_at).map_err(|_| CallError::Args)?;
    bump_generation(&shm);
    if super::stats::enabled() {
        super::stats::add(super::special::ID_GRALLOC_USAGE + 7, written.elapsed());
    }
    if a[4] != 0 {
        p.mem.write(a[4], &(-1i32).to_le_bytes()).map_err(|_| CallError::Args)?;
    }
    Ok(0)
}
