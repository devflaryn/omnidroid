//! **Stage 4: a frame on the screen, driven from translated ARM64, with the presented pixels read
//! back.**
//!
//! ```text
//! cargo test -p omni-android --release --test vulkan_present
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-android --release --test vulkan_present -- --ignored --test-threads=1
//! ```
//!
//! # The two halves, and what each is the only way to see
//!
//! `tests/vulkan_device.rs` makes the argument in full and this file inherits it. The **ordinary**
//! tests run on every machine against [`StageFourHost`], a double for the *host* side of
//! [`VulkanHost`] and never for anything the guest can see. Its job is to be a driver whose
//! answers the test chose, because these properties cannot be asserted against a real one:
//!
//! * that `VK_SUBOPTIMAL_KHR` reaches the guest **and** `pImageIndex` is written, while
//!   `VK_ERROR_OUT_OF_DATE_KHR` and `VK_TIMEOUT` reach the guest and `pImageIndex` is **not** —
//!   no real driver can be made to produce those three on demand;
//! * that `vkQueuePresentKHR`'s `pResults` is filled per swapchain rather than from the aggregate;
//! * that `vkCmdPipelineBarrier`'s ninth and tenth arguments, which AAPCS64 puts on the **stack**,
//!   are read at all;
//! * that destroying a swapchain takes its `VkImage` handles with it, and destroying a command
//!   pool takes its `VkCommandBuffer` handles;
//! * that every handle family refuses a value from another family by name.
//!
//! The **live** test is the other half and it is the one that answers "does a frame reach the
//! screen": a real window, a real NVIDIA driver, a real swapchain over that window's `HWND`, a
//! barrier and a clear recorded into a real command buffer, a real submit and a real present — and
//! then **the presented pixels read back and the colour asserted**. It is `#[ignore]`d and gated
//! on `OMNI_GFX_WINDOW_TESTS`, and under `--ignored` without the gate it **panics** naming the
//! variable (`VERIFICATION.md` entry 4).
//!
//! # What the read-back can and cannot see
//!
//! It reads the swapchain image after `vkQueuePresentKHR`, which holds exactly the pixels handed
//! to the presentation engine. It is **not** a capture of the monitor, and the reason is measured
//! rather than assumed: the graphics spike found that `PrintWindow(PW_RENDERFULLCONTENT)` returns
//! solid black for the client area of a flip-model swapchain on this host (§1), which is why
//! `omni-gfx`'s own `tests/renderer_live.rs` makes no claim about what is on the screen. The step
//! between "these pixels were presented" and "these pixels are on the monitor" is the compositor's
//! and is not one this runtime can measure. That limit is stated rather than papered over — and it
//! is a great deal more than `VkResult == 0`, which is what `VERIFICATION.md` entry 11 is about.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::jni::Jni;
use omni_android::ndk::{HostWindowSource, Ndk, WindowSource, SURFACE_CLASS};
use omni_android::vulkan::{
    Acquired, DeviceRequest, DriverAnswer, HostCommandBuffer, HostCommandPool, HostDevice,
    HostExtension, HostFence, HostImage, HostImageRef, HostImageView, HostInstance,
    HostPhysicalDevice, HostQueue, HostSemaphore, HostSurface, HostSwapchain, ImageViewRequest,
    InstanceRequest, PipelineBarrier, PresentRequest, Presented, SubmitRequest, SurfaceCreated,
    SwapchainRequest, Vulkan, VulkanHost, ANDROID_SURFACE_CREATE_INFO_BYTES,
    COMMAND_BUFFER_ALLOCATE_INFO_BYTES, COMMAND_BUFFER_BEGIN_INFO_BYTES,
    COMMAND_POOL_CREATE_INFO_BYTES, DEVICE_CREATE_INFO_BYTES, DEVICE_QUEUE_CREATE_INFO_BYTES,
    FENCE_CREATE_INFO_BYTES, GUEST_SURFACE_EXTENSION, IMAGE_MEMORY_BARRIER_BYTES,
    IMAGE_SUBRESOURCE_RANGE_BYTES, IMAGE_VIEW_CREATE_INFO_BYTES, LOADER_ENTRY_POINT,
    LOADER_SONAMES, PHYSICAL_DEVICE_FEATURES_BYTES, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES,
    PHYSICAL_DEVICE_PROPERTIES_BYTES, PRESENT_INFO_BYTES, QUEUE_FAMILY_PROPERTIES_BYTES,
    SEMAPHORE_CREATE_INFO_BYTES, STYPE_ANDROID_SURFACE_CREATE_INFO_KHR,
    STYPE_SWAPCHAIN_CREATE_INFO_KHR, SUBMIT_INFO_BYTES, SURFACE_CAPABILITIES_BYTES,
    SURFACE_FORMAT_BYTES, SWAPCHAIN_CREATE_INFO_BYTES, VK_ERROR_OUT_OF_DATE_KHR, VK_INCOMPLETE,
    VK_SUBOPTIMAL_KHR, VK_SUCCESS, VK_TIMEOUT,
};
use omni_android::{AbiError, AbiResult, Boundary};
use omni_cpu::{ExitReason, GuestAddr};
use omni_platform::window::RawWindow;

/// `RTLD_NOW`.
const RTLD_NOW: u64 = 2;

/// The gate, shared with every other live graphics test in this workspace.
const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// A window handle the double is given. Not a real `HWND`; the double calls no OS function.
const FAKE_HWND: isize = 0x1234_5678;
/// Its `HINSTANCE`, a different number so a shim that passed one field twice is caught.
const FAKE_HINSTANCE: isize = 0x0BAD_F00D;

// ----------------------------------------------------------- the Vulkan numbers the guest writes

/// `VK_STRUCTURE_TYPE_SUBMIT_INFO`.
const STYPE_SUBMIT_INFO: u32 = 4;
/// `VK_STRUCTURE_TYPE_FENCE_CREATE_INFO`.
const STYPE_FENCE_CREATE_INFO: u32 = 8;
/// `VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO`.
const STYPE_SEMAPHORE_CREATE_INFO: u32 = 9;
/// `VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO`.
const STYPE_IMAGE_VIEW_CREATE_INFO: u32 = 15;
/// `VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO`.
const STYPE_COMMAND_POOL_CREATE_INFO: u32 = 39;
/// `VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO`.
const STYPE_COMMAND_BUFFER_ALLOCATE_INFO: u32 = 40;
/// `VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO`.
const STYPE_COMMAND_BUFFER_BEGIN_INFO: u32 = 42;
/// `VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER`.
const STYPE_BUFFER_MEMORY_BARRIER: u32 = 44;
/// `VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER`.
const STYPE_IMAGE_MEMORY_BARRIER: u32 = 45;
/// `VK_STRUCTURE_TYPE_PRESENT_INFO_KHR`.
const STYPE_PRESENT_INFO_KHR: u32 = 1_000_001_001;

/// `VK_IMAGE_LAYOUT_UNDEFINED`.
const LAYOUT_UNDEFINED: u32 = 0;
/// `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`.
const LAYOUT_TRANSFER_DST: u32 = 7;
/// `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR`.
const LAYOUT_PRESENT_SRC: u32 = 1_000_001_002;

/// `VK_IMAGE_ASPECT_COLOR_BIT`.
const ASPECT_COLOR: u32 = 1;
/// `VK_QUEUE_FAMILY_IGNORED`.
const QUEUE_FAMILY_IGNORED: u32 = 0xFFFF_FFFF;
/// `VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT`.
const STAGE_TOP_OF_PIPE: u32 = 0x0000_0001;
/// `VK_PIPELINE_STAGE_TRANSFER_BIT`.
const STAGE_TRANSFER: u32 = 0x0000_1000;
/// `VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT`.
const STAGE_BOTTOM_OF_PIPE: u32 = 0x0000_2000;
/// `VK_ACCESS_TRANSFER_WRITE_BIT`.
const ACCESS_TRANSFER_WRITE: u32 = 0x0000_1000;
/// `VK_ACCESS_MEMORY_READ_BIT`.
const ACCESS_MEMORY_READ: u32 = 0x0000_8000;
/// `VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT`.
const ONE_TIME_SUBMIT: u32 = 1;
/// `VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT`.
const POOL_RESET_COMMAND_BUFFER: u32 = 2;
/// `VK_COMMAND_BUFFER_LEVEL_PRIMARY`.
const LEVEL_PRIMARY: u32 = 0;
/// `VK_IMAGE_USAGE_TRANSFER_SRC_BIT | _TRANSFER_DST_BIT | _COLOR_ATTACHMENT_BIT`.
///
/// `TRANSFER_DST` is what `vkCmdClearColorImage` needs. `TRANSFER_SRC` is what the **read-back**
/// needs, and asking for it here rather than adding it inside the host is deliberate: a swapchain
/// created with usage the engine did not ask for is a different swapchain from the one it asked
/// for, and a host that quietly widened the usage would be making the measurement possible by
/// changing the thing being measured.
const SWAPCHAIN_USAGE: u32 = 0x1 | 0x2 | 0x10;
/// `VK_PRESENT_MODE_FIFO_KHR`, which every implementation supports.
const PRESENT_MODE_FIFO: u32 = 2;
/// `VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR`.
const COMPOSITE_ALPHA_OPAQUE: u32 = 1;
/// `VK_SHARING_MODE_EXCLUSIVE`.
const SHARING_EXCLUSIVE: u32 = 0;
/// `VK_IMAGE_VIEW_TYPE_2D`.
const VIEW_TYPE_2D: u32 = 1;
/// `VK_FORMAT_B8G8R8A8_UNORM`, the format this host's surfaces offer first.
const FORMAT_B8G8R8A8_UNORM: u32 = 44;
/// `VK_FORMAT_R8G8B8A8_UNORM`.
const FORMAT_R8G8B8A8_UNORM: u32 = 37;

/// **The colour the live test clears to, and the bytes it must read back.**
///
/// Chosen so that every channel is exact in eight bits: a `UNORM` format stores
/// `round(value * 255)`, and `0.2 * 255 = 51`, `0.6 * 255 = 153`, `0.8 * 255 = 204` are all whole
/// numbers. A colour whose components rounded would make the assertion depend on the driver's
/// rounding mode rather than on whether the frame was presented.
///
/// The four are also **all different from each other and none is 0 or 255** in the colour
/// channels, which is what makes a channel-order mistake visible: a test that cleared to opaque
/// white would pass whether or not red and blue were exchanged.
const CLEAR_COLOUR: [f32; 4] = [0.2, 0.6, 0.8, 1.0];
/// [`CLEAR_COLOUR`] as the eight-bit values a `UNORM` swapchain stores, in R, G, B, A order.
const CLEAR_BYTES: [u8; 4] = [51, 153, 204, 255];

/// Fail, naming the variable, if a live test was run without the opt-in.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It opens a window, loads \
         the host's Vulkan driver, creates a real swapchain over that window, presents a frame on \
         this machine's GPU and reads the presented pixels back; it will not pretend to have \
         passed on a machine that cannot do that. Set {GATE}=1 to run it, or drop --ignored to \
         skip it visibly."
    );
}

// =================================================================== the host test double

/// What a [`StageFourHost`] was asked, in order.
#[derive(Debug, Default)]
struct HostLog {
    /// Every `vkCreateSwapchainKHR` request, exactly as the shim decoded it.
    swapchains: Vec<SwapchainRequest>,
    /// Every `vkCreateImageView` request.
    views: Vec<ImageViewRequest>,
    /// Every `vkCmdPipelineBarrier`, as the shim decoded it — including the two arguments AAPCS64
    /// puts on the stack.
    barriers: Vec<PipelineBarrier>,
    /// Every `vkCmdClearColorImage`: the layout, the sixteen raw bytes of the union, and the
    /// ranges.
    clears: Vec<(u32, [u8; 16], Vec<Vec<u8>>)>,
    /// Every `vkQueueSubmit`.
    submits: Vec<(HostQueue, Vec<SubmitRequest>, Option<HostFence>)>,
    /// Every `vkQueuePresentKHR`.
    presents: Vec<PresentRequest>,
    /// Every `vkWaitForFences`: the fences, `waitAll`, and the timeout **unclamped**.
    waits: Vec<(Vec<HostFence>, bool, u64)>,
    /// Destroy calls, by name, so a test can say what was and was not torn down.
    destroyed: Vec<String>,
}

/// A [`VulkanHost`] whose answers the test chooses. **Not a driver, and not guest-facing.**
#[derive(Debug)]
struct StageFourHost {
    /// How many images each created swapchain reports.
    images_per_swapchain: usize,
    /// Scripted `vkAcquireNextImageKHR` answers, oldest first. Empty means "success, image 0".
    acquires: Mutex<VecDeque<Acquired>>,
    /// Scripted `vkQueuePresentKHR` answers. Empty means "success".
    presents: Mutex<VecDeque<Presented>>,
    /// How many of each object have been handed out, so tokens are distinct.
    next: Mutex<u64>,
    log: Mutex<HostLog>,
}

impl StageFourHost {
    fn new() -> Arc<StageFourHost> {
        Arc::new(StageFourHost {
            images_per_swapchain: 3,
            acquires: Mutex::new(VecDeque::new()),
            presents: Mutex::new(VecDeque::new()),
            next: Mutex::new(1),
            log: Mutex::new(HostLog::default()),
        })
    }

    fn log(&self) -> std::sync::MutexGuard<'_, HostLog> {
        self.log.lock().expect("the log is never held across a panic")
    }

    /// A fresh token, distinct from every other this host has handed out — **across families**.
    ///
    /// Deliberately one counter rather than one per family: it means a `VkFence` and a
    /// `VkSemaphore` never share a token value, so a test that found the right object after
    /// passing the wrong handle would be finding it for a real reason rather than because two
    /// tables happened to start at zero.
    fn token(&self) -> u64 {
        let mut next = self.next.lock().expect("no panic holds this");
        *next += 1;
        *next
    }

    fn note(&self, what: &str) {
        self.log().destroyed.push(what.to_string());
    }
}

impl VulkanHost for StageFourHost {
    fn platform_surface_extension(&self) -> AbiResult<String> {
        Ok("VK_KHR_win32_surface".to_string())
    }

    fn platform_surface_entry_point(&self) -> AbiResult<String> {
        Ok("vkCreateWin32SurfaceKHR".to_string())
    }

    fn instance_extensions(
        &self,
        _layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        Ok(DriverAnswer::Ok(vec![
            HostExtension { name: "VK_KHR_surface".to_string(), spec_version: 25 },
            HostExtension { name: "VK_KHR_win32_surface".to_string(), spec_version: 6 },
        ]))
    }

    fn create_instance(&self, _request: &InstanceRequest) -> AbiResult<DriverAnswer<HostInstance>> {
        Ok(DriverAnswer::Ok(HostInstance::from_token(0)))
    }

    fn has_instance_proc(&self, _instance: HostInstance, name: &str) -> AbiResult<bool> {
        Ok(name.starts_with("vkGet")
            || name.starts_with("vkEnumerate")
            || name == "vkCreateWin32SurfaceKHR"
            || name == "vkCreateDevice")
    }

    fn create_platform_surface(
        &self,
        _instance: HostInstance,
        _window: RawWindow,
    ) -> AbiResult<DriverAnswer<SurfaceCreated>> {
        Ok(DriverAnswer::Ok(SurfaceCreated {
            surface: HostSurface::from_token(self.token()),
            host_call: "vkCreateWin32SurfaceKHR".to_string(),
        }))
    }

    fn physical_devices(
        &self,
        _instance: HostInstance,
    ) -> AbiResult<DriverAnswer<Vec<HostPhysicalDevice>>> {
        Ok(DriverAnswer::Ok(vec![HostPhysicalDevice::from_token(0)]))
    }

    fn physical_device_properties(&self, _device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        let mut bytes = vec![0u8; PHYSICAL_DEVICE_PROPERTIES_BYTES];
        let name = b"OMNI Reference GPU";
        bytes[20..20 + name.len()].copy_from_slice(name);
        Ok(bytes)
    }

    fn physical_device_features(&self, _device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        Ok(vec![0u8; PHYSICAL_DEVICE_FEATURES_BYTES])
    }

    fn queue_family_properties(&self, _device: HostPhysicalDevice) -> AbiResult<Vec<Vec<u8>>> {
        let mut family = vec![0u8; QUEUE_FAMILY_PROPERTIES_BYTES];
        family[0..4].copy_from_slice(&0x0000_0007u32.to_le_bytes()); // GRAPHICS|COMPUTE|TRANSFER
        family[4..8].copy_from_slice(&1u32.to_le_bytes());
        Ok(vec![family])
    }

    fn physical_device_memory_properties(&self, _d: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        Ok(vec![0u8; PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES])
    }

    fn surface_support(
        &self,
        _device: HostPhysicalDevice,
        _queue_family: u32,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<bool>> {
        Ok(DriverAnswer::Ok(true))
    }

    fn surface_capabilities(
        &self,
        _device: HostPhysicalDevice,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let mut bytes = vec![0u8; SURFACE_CAPABILITIES_BYTES];
        bytes[0..4].copy_from_slice(&2u32.to_le_bytes()); // minImageCount
        bytes[8..12].copy_from_slice(&1024u32.to_le_bytes()); // currentExtent.width
        bytes[12..16].copy_from_slice(&576u32.to_le_bytes()); // currentExtent.height
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes()); // currentTransform = IDENTITY
        bytes[48..52].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // supportedUsageFlags
        Ok(DriverAnswer::Ok(bytes))
    }

    fn surface_formats(
        &self,
        _device: HostPhysicalDevice,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<Vec<u8>>>> {
        let mut format = vec![0u8; SURFACE_FORMAT_BYTES];
        format[0..4].copy_from_slice(&FORMAT_B8G8R8A8_UNORM.to_le_bytes());
        Ok(DriverAnswer::Ok(vec![format]))
    }

    fn surface_present_modes(
        &self,
        _device: HostPhysicalDevice,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u32>>> {
        Ok(DriverAnswer::Ok(vec![PRESENT_MODE_FIFO]))
    }

    fn device_extensions(
        &self,
        _device: HostPhysicalDevice,
        _layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        Ok(DriverAnswer::Ok(vec![HostExtension {
            name: "VK_KHR_swapchain".to_string(),
            spec_version: 70,
        }]))
    }

    fn create_device(
        &self,
        _device: HostPhysicalDevice,
        _request: &DeviceRequest,
    ) -> AbiResult<DriverAnswer<HostDevice>> {
        Ok(DriverAnswer::Ok(HostDevice::from_token(0)))
    }

    fn device_queue(&self, _d: HostDevice, family: u32, index: u32) -> AbiResult<HostQueue> {
        Ok(HostQueue::from_token((u64::from(family) << 32) | u64::from(index)))
    }

    fn has_device_proc(&self, _device: HostDevice, name: &str) -> AbiResult<bool> {
        // Every stage 4 name, and nothing else -- so a test that resolves one this stage does not
        // implement gets the driver's NULL rather than a thunk.
        Ok(matches!(
            name,
            "vkCreateSwapchainKHR"
                | "vkGetSwapchainImagesKHR"
                | "vkDestroySwapchainKHR"
                | "vkAcquireNextImageKHR"
                | "vkCreateImageView"
                | "vkDestroyImageView"
                | "vkCreateSemaphore"
                | "vkDestroySemaphore"
                | "vkCreateFence"
                | "vkDestroyFence"
                | "vkWaitForFences"
                | "vkResetFences"
                | "vkCreateCommandPool"
                | "vkDestroyCommandPool"
                | "vkResetCommandPool"
                | "vkAllocateCommandBuffers"
                | "vkFreeCommandBuffers"
                | "vkBeginCommandBuffer"
                | "vkEndCommandBuffer"
                | "vkResetCommandBuffer"
                | "vkCmdPipelineBarrier"
                | "vkCmdClearColorImage"
                | "vkQueueSubmit"
                | "vkQueuePresentKHR"
                | "vkQueueWaitIdle"
                | "vkDeviceWaitIdle"
        ))
    }

    // ------------------------------------------------------------------------ stage 4

    fn create_swapchain(
        &self,
        _device: HostDevice,
        request: &SwapchainRequest,
    ) -> AbiResult<DriverAnswer<HostSwapchain>> {
        let mut log = self.log();
        log.swapchains.push(request.clone());
        drop(log);
        Ok(DriverAnswer::Ok(HostSwapchain::from_token(self.token())))
    }

    fn swapchain_images(
        &self,
        swapchain: HostSwapchain,
    ) -> AbiResult<DriverAnswer<Vec<HostImage>>> {
        // **Derived from the swapchain's own token**, so the same swapchain always answers with
        // the same images -- which is what the guest-side registry's deduplication rests on, and
        // what a double that minted fresh tokens per call would have hidden.
        Ok(DriverAnswer::Ok(
            (0..self.images_per_swapchain as u64)
                .map(|index| HostImage::from_token((swapchain.token() << 8) | index))
                .collect(),
        ))
    }

    fn destroy_swapchain(&self, _swapchain: HostSwapchain) -> AbiResult<()> {
        self.note("vkDestroySwapchainKHR");
        Ok(())
    }

    fn create_image_view(
        &self,
        _device: HostDevice,
        request: &ImageViewRequest,
    ) -> AbiResult<DriverAnswer<HostImageView>> {
        self.log().views.push(request.clone());
        Ok(DriverAnswer::Ok(HostImageView::from_token(self.token())))
    }

    fn destroy_image_view(&self, _view: HostImageView) -> AbiResult<()> {
        self.note("vkDestroyImageView");
        Ok(())
    }

    fn create_semaphore(
        &self,
        _device: HostDevice,
        _flags: u32,
    ) -> AbiResult<DriverAnswer<HostSemaphore>> {
        Ok(DriverAnswer::Ok(HostSemaphore::from_token(self.token())))
    }

    fn destroy_semaphore(&self, _semaphore: HostSemaphore) -> AbiResult<()> {
        self.note("vkDestroySemaphore");
        Ok(())
    }

    fn create_fence(&self, _device: HostDevice, _flags: u32) -> AbiResult<DriverAnswer<HostFence>> {
        Ok(DriverAnswer::Ok(HostFence::from_token(self.token())))
    }

    fn destroy_fence(&self, _fence: HostFence) -> AbiResult<()> {
        self.note("vkDestroyFence");
        Ok(())
    }

    fn wait_for_fences(
        &self,
        _device: HostDevice,
        fences: &[HostFence],
        wait_all: bool,
        timeout: u64,
    ) -> AbiResult<i32> {
        self.log().waits.push((fences.to_vec(), wait_all, timeout));
        Ok(VK_SUCCESS)
    }

    fn reset_fences(
        &self,
        _device: HostDevice,
        _fences: &[HostFence],
    ) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn create_command_pool(
        &self,
        _device: HostDevice,
        _flags: u32,
        _queue_family: u32,
    ) -> AbiResult<DriverAnswer<HostCommandPool>> {
        Ok(DriverAnswer::Ok(HostCommandPool::from_token(self.token())))
    }

    fn allocate_command_buffers(
        &self,
        pool: HostCommandPool,
        _level: u32,
        count: u32,
    ) -> AbiResult<DriverAnswer<Vec<HostCommandBuffer>>> {
        Ok(DriverAnswer::Ok(
            (0..u64::from(count))
                .map(|index| HostCommandBuffer::from_token((pool.token() << 8) | index))
                .collect(),
        ))
    }

    fn command_buffers_of(&self, pool: HostCommandPool) -> AbiResult<Vec<HostCommandBuffer>> {
        Ok((0..1u64).map(|i| HostCommandBuffer::from_token((pool.token() << 8) | i)).collect())
    }

    fn begin_command_buffer(
        &self,
        _buffer: HostCommandBuffer,
        _flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn end_command_buffer(&self, _buffer: HostCommandBuffer) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn reset_command_buffer(
        &self,
        _buffer: HostCommandBuffer,
        _flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn reset_command_pool(
        &self,
        _pool: HostCommandPool,
        _flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn free_command_buffers(
        &self,
        _pool: HostCommandPool,
        _buffers: &[HostCommandBuffer],
    ) -> AbiResult<()> {
        self.note("vkFreeCommandBuffers");
        Ok(())
    }

    fn destroy_command_pool(&self, _pool: HostCommandPool) -> AbiResult<()> {
        self.note("vkDestroyCommandPool");
        Ok(())
    }

    fn cmd_pipeline_barrier(
        &self,
        _buffer: HostCommandBuffer,
        barrier: &PipelineBarrier,
    ) -> AbiResult<()> {
        self.log().barriers.push(barrier.clone());
        Ok(())
    }

    fn cmd_clear_color_image(
        &self,
        _buffer: HostCommandBuffer,
        _image: HostImageRef,
        layout: u32,
        colour: [u8; 16],
        ranges: &[Vec<u8>],
    ) -> AbiResult<()> {
        self.log().clears.push((layout, colour, ranges.to_vec()));
        Ok(())
    }

    fn acquire_next_image(
        &self,
        _swapchain: HostSwapchain,
        _timeout: u64,
        _semaphore: Option<HostSemaphore>,
        _fence: Option<HostFence>,
    ) -> AbiResult<Acquired> {
        let scripted = self.acquires.lock().expect("no panic holds this").pop_front();
        Ok(scripted.unwrap_or(Acquired { result: VK_SUCCESS, image_index: Some(0) }))
    }

    fn queue_submit(
        &self,
        queue: HostQueue,
        submits: &[SubmitRequest],
        fence: Option<HostFence>,
    ) -> AbiResult<DriverAnswer<()>> {
        self.log().submits.push((queue, submits.to_vec(), fence));
        Ok(DriverAnswer::Ok(()))
    }

    fn queue_present(&self, _queue: HostQueue, present: &PresentRequest) -> AbiResult<Presented> {
        self.log().presents.push(present.clone());
        let scripted = self.presents.lock().expect("no panic holds this").pop_front();
        Ok(scripted.unwrap_or(Presented {
            result: VK_SUCCESS,
            per_swapchain: vec![VK_SUCCESS; present.swapchains.len()],
        }))
    }

    fn queue_wait_idle(&self, _queue: HostQueue) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn device_wait_idle(&self, _device: HostDevice) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }
}

// ================================================================================ the fixture

/// A host directory that removes itself, so the bionic instance has a filesystem root.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-vkpresent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
    ndk: Arc<Ndk>,
    vulkan: Arc<Vulkan>,
    boundary: Arc<Boundary>,
    next: Cell<usize>,
    _root: Scratch,
}

/// Where the bump allocator starts, leaving the low bytes for results.
const ARENA_AT: usize = 0x800;

/// One reusable buffer for enumerations, sized from what a real driver reports.
const SCRATCH_BYTES: usize = 48 * 1024;

/// Enough thunk slots for bionic, the NDK, the JNI tables and the Vulkan pool.
const SLOTS: usize = 2048;

fn fixture(tag: &str, host: Option<Arc<dyn VulkanHost>>) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let ndk = Ndk::new(Arc::clone(&guest.space)).expect("an NDK instance");
    let jni = Jni::new(Arc::clone(&guest.space)).expect("a JNI instance");
    let builder = guest.boundary(SLOTS);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    ndk.bind_into(&builder).expect("bind every NDK handler");
    jni.install_into(&builder).expect("install the JNI tables");
    bionic.set_log_to_stderr(false);
    let vulkan = Vulkan::new();
    vulkan.bind_into(&builder).expect("bind the Vulkan loader");
    if let Some(host) = host {
        vulkan.set_host(host);
    }
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let boundary = builder.finish();
    Fixture { guest, bionic, jni, ndk, vulkan, boundary, next: Cell::new(ARENA_AT), _root: root }
}

impl Fixture {
    fn vulkan(&self) -> &Arc<Vulkan> {
        &self.vulkan
    }

    fn thunk(&self, symbol: &str) -> GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    fn alloc(&self, len: usize) -> u64 {
        let offset = (self.next.get() + 7) & !7;
        assert!(offset + len < harness::DATA_BYTES, "the guest arena is full");
        self.next.set(offset + len);
        (self.guest.data + offset) as u64
    }

    fn bytes(&self, image: &[u8]) -> u64 {
        let at = self.alloc(image.len().max(8));
        self.guest.write_bytes(at as GuestAddr, image);
        at
    }

    fn cstr(&self, text: &str) -> u64 {
        let bytes: Vec<u8> = text.bytes().chain(std::iter::once(0)).collect();
        self.bytes(&bytes)
    }

    fn u64_array(&self, values: &[u64]) -> u64 {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.bytes(&bytes)
    }

    fn u32_array(&self, values: &[u32]) -> u64 {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.bytes(&bytes)
    }

    /// `len` bytes filled with `fill`, so that "nothing was written here" is checkable.
    fn poisoned(&self, len: usize, fill: u8) -> u64 {
        self.bytes(&vec![fill; len])
    }

    fn run(&self, entry: GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _jni = self.jni.activate().expect("publish the JNI instance");
        let _ndk = self.ndk.activate();
        let _vulkan = self.vulkan.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    fn program_branching(&self, body: impl FnOnce(&mut Asm)) -> GuestAddr {
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        body(&mut asm);
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        entry
    }

    /// Call `target` indirectly with up to twelve arguments, **the ninth onwards on the stack**.
    ///
    /// # Why this exists and `vulkan_device.rs`'s four-argument `call` did not need to
    ///
    /// Stage 3's widest call takes four arguments. Stage 4's `vkCmdPipelineBarrier` takes **ten**,
    /// and AAPCS64 puts the first eight in `X0`-`X7` and the rest in the caller's overflow area at
    /// `SP`. Those last two are `imageMemoryBarrierCount` and `pImageMemoryBarriers` — the two the
    /// whole barrier is *about* — so a harness that could only fill registers would test the
    /// handler with an empty barrier list and would pass.
    ///
    /// The frame is rounded up to sixteen bytes because AAPCS64 requires `SP` to be
    /// sixteen-aligned at a public interface, and a `blr` with a misaligned `SP` is a fault on
    /// aarch64 rather than a slow path.
    fn call_n(&self, target: u64, args: &[u64]) -> Result<u64, AbiError> {
        assert!(args.len() <= 12, "this harness spills at most four arguments");
        let registers: Vec<u64> = args.iter().take(8).copied().collect();
        let stacked: Vec<u64> = args.iter().skip(8).copied().collect();
        let frame = ((stacked.len() * 8) + 15) & !15;
        let program = self.program_branching(|asm| {
            asm.mov(9, target);
            if frame > 0 {
                asm.push(sub_imm(31, 31, frame as u32));
                for (index, value) in stacked.iter().enumerate() {
                    asm.mov(10, *value);
                    asm.push(str_imm(10, 31, (index * 8) as u32));
                }
            }
            for (index, value) in registers.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
            asm.push(blr(9));
            if frame > 0 {
                asm.push(add_imm(31, 31, frame as u32));
            }
        });
        let exit = self.run(program)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data))
    }

    /// The four-argument form, which is most of them.
    fn call(&self, target: u64, args: [u64; 4]) -> Result<u64, AbiError> {
        self.call_n(target, &args)
    }

    fn call_symbol(&self, symbol: &str, args: [u64; 4]) -> Result<u64, AbiError> {
        self.call(self.thunk(symbol) as u64, args)
    }

    fn refusal(&self, target: u64, args: &[u64]) -> AbiError {
        match self.call_n(target, args) {
            Err(error) => error,
            Ok(value) => {
                panic!("a call to {target:#x} returned {value:#x} where a refusal was required")
            }
        }
    }

    /// `dlopen` + `dlsym` + `vkGetInstanceProcAddr`, as the engine does it.
    fn entry_point(&self) -> u64 {
        let soname = self.cstr(LOADER_SONAMES[0]);
        let symbol = self.cstr(LOADER_ENTRY_POINT);
        let dlopen = self.thunk("dlopen");
        let dlsym = self.thunk("dlsym");
        let program = self.program_branching(|asm| {
            asm.mov(0, soname);
            asm.mov(1, RTLD_NOW);
            asm.bl(dlopen);
            asm.mov(1, symbol);
            asm.bl(dlsym);
        });
        let exit = self.run(program).expect("the bootstrap must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        let at = self.guest.read_u64(self.guest.data);
        assert_eq!(at as GuestAddr, self.thunk(LOADER_ENTRY_POINT));
        at
    }

    fn resolve(&self, entry_point: u64, instance: u64, name: &str) -> u64 {
        let at = self.cstr(name);
        let found = self.call(entry_point, [instance, at, 0, 0]).expect("the lookup completes");
        assert_ne!(found, 0, "`{name}` must resolve to a thunk on instance {instance:#x}");
        found
    }

    /// The **device-level** resolution, which is how a renderer gets every name in this file.
    fn resolve_device(&self, get_proc: u64, device: u64, name: &str) -> u64 {
        let at = self.cstr(name);
        let found = self.call(get_proc, [device, at, 0, 0]).expect("the lookup completes");
        assert_ne!(found, 0, "`{name}` must resolve to a thunk on device {device:#x}");
        found
    }

    fn an_instance(&self, entry_point: u64) -> u64 {
        let create = self.resolve(entry_point, 0, "vkCreateInstance");
        let names = ["VK_KHR_surface", GUEST_SURFACE_EXTENSION];
        let pointers: Vec<u64> = names.iter().map(|name| self.cstr(name)).collect();
        let array = self.u64_array(&pointers);
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&(names.len() as u32).to_le_bytes());
        bytes[56..64].copy_from_slice(&array.to_le_bytes());
        let info = self.bytes(&bytes);
        let out = self.alloc(8);
        assert_eq!(self.call(create, [info, 0, out, 0]).expect("create") as i32, VK_SUCCESS);
        let handle = self.guest.read_u64(out as GuestAddr);
        assert_ne!(handle, 0);
        handle
    }

    fn a_native_window(&self) -> u64 {
        let surface = self.jni.new_object(SURFACE_CLASS).expect("a Java Surface");
        let window = self
            .call_symbol("ANativeWindow_fromSurface", [0, surface, 0, 0])
            .expect("fromSurface must complete");
        assert_ne!(window, 0);
        window
    }

    fn a_surface(&self, entry_point: u64, instance: u64) -> u64 {
        let create = self.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");
        let window = self.a_native_window();
        let mut bytes = vec![0u8; ANDROID_SURFACE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_ANDROID_SURFACE_CREATE_INFO_KHR.to_le_bytes());
        bytes[24..32].copy_from_slice(&window.to_le_bytes());
        let info = self.bytes(&bytes);
        let out = self.alloc(8);
        assert_eq!(
            self.call(create, [instance, info, 0, out]).expect("create") as i32,
            VK_SUCCESS
        );
        self.guest.read_u64(out as GuestAddr)
    }

    fn a_device(&self, entry_point: u64, instance: u64, physical: u64, family: u32) -> u64 {
        let create = self.resolve(entry_point, instance, "vkCreateDevice");
        let priorities = self.bytes(&1.0f32.to_le_bytes());
        let mut queue = vec![0u8; DEVICE_QUEUE_CREATE_INFO_BYTES];
        queue[0..4].copy_from_slice(&2u32.to_le_bytes());
        queue[20..24].copy_from_slice(&family.to_le_bytes());
        queue[24..28].copy_from_slice(&1u32.to_le_bytes());
        queue[32..40].copy_from_slice(&priorities.to_le_bytes());
        let queues = self.bytes(&queue);
        let extension = self.cstr("VK_KHR_swapchain");
        let extensions = self.u64_array(&[extension]);
        let mut bytes = vec![0u8; DEVICE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&3u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&queues.to_le_bytes());
        bytes[48..52].copy_from_slice(&1u32.to_le_bytes());
        bytes[56..64].copy_from_slice(&extensions.to_le_bytes());
        let info = self.bytes(&bytes);
        let out = self.alloc(8);
        assert_eq!(
            self.call(create, [physical, info, 0, out]).expect("create") as i32,
            VK_SUCCESS
        );
        self.guest.read_u64(out as GuestAddr)
    }

    fn read_u32(&self, at: u64) -> u32 {
        self.guest.read_u64(at as GuestAddr) as u32
    }

    fn read_bytes(&self, at: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        let mut offset = 0;
        while out.len() < len {
            out.extend_from_slice(&self.guest.read_u64(at as GuestAddr + offset).to_le_bytes());
            offset += 8;
        }
        out.truncate(len);
        out
    }

    // -------------------------------------------------------- the stage 4 structures, in memory

    /// A `VkSwapchainCreateInfoKHR`, with every field a test might want to vary.
    #[allow(clippy::too_many_arguments)]
    fn swapchain_info(
        &self,
        surface: u64,
        min_images: u32,
        format: u32,
        width: u32,
        height: u32,
        usage: u32,
        transform: u32,
        old: u64,
    ) -> u64 {
        let mut bytes = vec![0u8; SWAPCHAIN_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_SWAPCHAIN_CREATE_INFO_KHR.to_le_bytes());
        bytes[24..32].copy_from_slice(&surface.to_le_bytes());
        bytes[32..36].copy_from_slice(&min_images.to_le_bytes());
        bytes[36..40].copy_from_slice(&format.to_le_bytes());
        bytes[40..44].copy_from_slice(&0u32.to_le_bytes()); // SRGB_NONLINEAR
        bytes[44..48].copy_from_slice(&width.to_le_bytes());
        bytes[48..52].copy_from_slice(&height.to_le_bytes());
        bytes[52..56].copy_from_slice(&1u32.to_le_bytes()); // imageArrayLayers
        bytes[56..60].copy_from_slice(&usage.to_le_bytes());
        bytes[60..64].copy_from_slice(&SHARING_EXCLUSIVE.to_le_bytes());
        bytes[80..84].copy_from_slice(&transform.to_le_bytes());
        bytes[84..88].copy_from_slice(&COMPOSITE_ALPHA_OPAQUE.to_le_bytes());
        bytes[88..92].copy_from_slice(&PRESENT_MODE_FIFO.to_le_bytes());
        bytes[92..96].copy_from_slice(&1u32.to_le_bytes()); // clipped = VK_TRUE
        bytes[96..104].copy_from_slice(&old.to_le_bytes());
        self.bytes(&bytes)
    }

    /// The whole of a single-mip, single-layer colour image, as a `VkImageSubresourceRange`.
    fn whole_colour_range(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; IMAGE_SUBRESOURCE_RANGE_BYTES];
        bytes[0..4].copy_from_slice(&ASPECT_COLOR.to_le_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes()); // levelCount
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes()); // layerCount
        bytes
    }

    fn image_view_info(&self, image: u64, format: u32) -> u64 {
        let mut bytes = vec![0u8; IMAGE_VIEW_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_IMAGE_VIEW_CREATE_INFO.to_le_bytes());
        bytes[24..32].copy_from_slice(&image.to_le_bytes());
        bytes[32..36].copy_from_slice(&VIEW_TYPE_2D.to_le_bytes());
        bytes[36..40].copy_from_slice(&format.to_le_bytes());
        // `components` stays all-zero, which is `VK_COMPONENT_SWIZZLE_IDENTITY` four times.
        bytes[56..76].copy_from_slice(&self.whole_colour_range());
        self.bytes(&bytes)
    }

    /// A `VkSemaphoreCreateInfo` or a `VkFenceCreateInfo` — the same twenty-four bytes, told apart
    /// only by `sType`, which is exactly the property `sync.rs` is written around.
    fn flags_only_info(&self, stype: u32, flags: u32) -> u64 {
        let mut bytes = vec![0u8; SEMAPHORE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&stype.to_le_bytes());
        bytes[16..20].copy_from_slice(&flags.to_le_bytes());
        self.bytes(&bytes)
    }

    fn command_pool_info(&self, flags: u32, family: u32) -> u64 {
        let mut bytes = vec![0u8; COMMAND_POOL_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_COMMAND_POOL_CREATE_INFO.to_le_bytes());
        bytes[16..20].copy_from_slice(&flags.to_le_bytes());
        bytes[20..24].copy_from_slice(&family.to_le_bytes());
        self.bytes(&bytes)
    }

    fn command_buffer_allocate_info(&self, pool: u64, count: u32) -> u64 {
        let mut bytes = vec![0u8; COMMAND_BUFFER_ALLOCATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_COMMAND_BUFFER_ALLOCATE_INFO.to_le_bytes());
        bytes[16..24].copy_from_slice(&pool.to_le_bytes());
        bytes[24..28].copy_from_slice(&LEVEL_PRIMARY.to_le_bytes());
        bytes[28..32].copy_from_slice(&count.to_le_bytes());
        self.bytes(&bytes)
    }

    fn begin_info(&self, flags: u32, inheritance: u64) -> u64 {
        let mut bytes = vec![0u8; COMMAND_BUFFER_BEGIN_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_COMMAND_BUFFER_BEGIN_INFO.to_le_bytes());
        bytes[16..20].copy_from_slice(&flags.to_le_bytes());
        bytes[24..32].copy_from_slice(&inheritance.to_le_bytes());
        self.bytes(&bytes)
    }

    #[allow(clippy::too_many_arguments)]
    fn image_barrier(
        &self,
        stype: u32,
        src_access: u32,
        dst_access: u32,
        old_layout: u32,
        new_layout: u32,
        image: u64,
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; IMAGE_MEMORY_BARRIER_BYTES];
        bytes[0..4].copy_from_slice(&stype.to_le_bytes());
        bytes[16..20].copy_from_slice(&src_access.to_le_bytes());
        bytes[20..24].copy_from_slice(&dst_access.to_le_bytes());
        bytes[24..28].copy_from_slice(&old_layout.to_le_bytes());
        bytes[28..32].copy_from_slice(&new_layout.to_le_bytes());
        bytes[32..36].copy_from_slice(&QUEUE_FAMILY_IGNORED.to_le_bytes());
        bytes[36..40].copy_from_slice(&QUEUE_FAMILY_IGNORED.to_le_bytes());
        bytes[40..48].copy_from_slice(&image.to_le_bytes());
        bytes[48..68].copy_from_slice(&self.whole_colour_range());
        bytes
    }

    /// A `VkSubmitInfo` with one command buffer, one wait and one signal.
    fn submit_info(&self, wait: u64, stage: u32, command: u64, signal: u64) -> u64 {
        let waits = self.u64_array(&[wait]);
        let stages = self.u32_array(&[stage]);
        let commands = self.u64_array(&[command]);
        let signals = self.u64_array(&[signal]);
        let mut bytes = vec![0u8; SUBMIT_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_SUBMIT_INFO.to_le_bytes());
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&waits.to_le_bytes());
        bytes[32..40].copy_from_slice(&stages.to_le_bytes());
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes());
        bytes[48..56].copy_from_slice(&commands.to_le_bytes());
        bytes[56..60].copy_from_slice(&1u32.to_le_bytes());
        bytes[64..72].copy_from_slice(&signals.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkPresentInfoKHR` for one swapchain, with `pResults` when `results` is non-zero.
    fn present_info(&self, wait: u64, swapchain: u64, index_at: u64, results: u64) -> u64 {
        let waits = self.u64_array(&[wait]);
        let swapchains = self.u64_array(&[swapchain]);
        let mut bytes = vec![0u8; PRESENT_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_PRESENT_INFO_KHR.to_le_bytes());
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&waits.to_le_bytes());
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&swapchains.to_le_bytes());
        bytes[48..56].copy_from_slice(&index_at.to_le_bytes());
        bytes[56..64].copy_from_slice(&results.to_le_bytes());
        self.bytes(&bytes)
    }
}

/// A window source with a handle, which is what `vkCreateAndroidSurfaceKHR` needs.
fn a_source_with_a_handle() -> Arc<HostWindowSource> {
    let source = HostWindowSource::unpublished();
    source.publish(1024, 576).expect("a publishable size");
    source.set_raw_window(RawWindow::Win32 { hwnd: FAKE_HWND, hinstance: FAKE_HINSTANCE });
    source
}

/// Everything a stage 4 test needs: a fixture with a double, and the handles up to a device.
struct UpToADevice {
    f: Fixture,
    host: Arc<StageFourHost>,
    entry_point: u64,
    surface: u64,
    device: u64,
    get_proc: u64,
    queue: u64,
}

fn up_to_a_device(tag: &str) -> UpToADevice {
    let host = StageFourHost::new();
    let f = fixture(tag, Some(host.clone()));
    f.ndk.set_window_source(a_source_with_a_handle() as Arc<dyn WindowSource>);
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let surface = f.a_surface(entry_point, instance);

    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let array_at = f.alloc(8);
    assert_eq!(
        f.call(enumerate, [instance, count_at, array_at, 0]).expect("array") as i32,
        VK_SUCCESS
    );
    let physical = f.guest.read_u64(array_at as GuestAddr);

    let device = f.a_device(entry_point, instance, physical, 0);
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");

    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [device, 0, 0, queue_at]).expect("a queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);
    assert_ne!(queue, 0);

    UpToADevice { f, host, entry_point, surface, device, get_proc, queue }
}
