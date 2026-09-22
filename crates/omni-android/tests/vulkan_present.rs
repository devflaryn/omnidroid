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
    GUEST_SURFACE_EXTENSION, IMAGE_MEMORY_BARRIER_BYTES,
    IMAGE_SUBRESOURCE_RANGE_BYTES, IMAGE_VIEW_CREATE_INFO_BYTES, LOADER_ENTRY_POINT,
    LOADER_SONAMES, PHYSICAL_DEVICE_FEATURES_BYTES, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES,
    PHYSICAL_DEVICE_PROPERTIES_BYTES, PRESENT_INFO_BYTES, QUEUE_FAMILY_PROPERTIES_BYTES,
    HANDLE_SLOT_BYTES, SEMAPHORE_CREATE_INFO_BYTES, STYPE_ANDROID_SURFACE_CREATE_INFO_KHR,
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

    let _ = entry_point;
    UpToADevice { f, host, surface, device, get_proc, queue }
}

// ========================================================================= vkCreateSwapchainKHR

/// **Every field of `VkSwapchainCreateInfoKHR` arrives at the host as the guest wrote it, and both
/// of its handles arrive as tokens.**
///
/// The decode test, and the one that would catch the single most likely layout mistake in this
/// stage: `surface` is a `uint64_t` at **24**, not at 20, because a non-dispatchable handle is
/// eight bytes and eight-aligned. A layout that packed it at 20 would read the two halves of the
/// handle as `minImageCount` and `imageFormat`, and every field after it would be wrong by four
/// bytes — so the values here are deliberately all different from one another.
#[test]
fn the_swapchain_create_info_arrives_field_for_field_and_its_handles_are_tokens() {
    let _serial = serialized();
    let up = up_to_a_device("swapchain-decode");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateSwapchainKHR");

    let info = up.f.swapchain_info(
        up.surface,
        3,
        FORMAT_B8G8R8A8_UNORM,
        1280,
        720,
        SWAPCHAIN_USAGE,
        1,
        0,
    );
    let out = up.f.alloc(8);
    assert_eq!(up.f.call(create, [up.device, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let swapchain = up.f.guest.read_u64(out as GuestAddr);
    assert_ne!(swapchain, 0);

    let log = up.host.log();
    assert_eq!(log.swapchains.len(), 1);
    let request = &log.swapchains[0];
    assert_eq!(request.min_image_count, 3);
    assert_eq!(request.format, FORMAT_B8G8R8A8_UNORM);
    assert_eq!(request.colour_space, 0);
    assert_eq!(request.width, 1280, "imageExtent.width is at 44");
    assert_eq!(request.height, 720, "and .height at 48");
    assert_eq!(request.array_layers, 1);
    assert_eq!(request.usage, SWAPCHAIN_USAGE);
    assert_eq!(request.sharing_mode, SHARING_EXCLUSIVE);
    assert_eq!(request.pre_transform, 1);
    assert_eq!(request.composite_alpha, COMPOSITE_ALPHA_OPAQUE);
    assert_eq!(request.present_mode, PRESENT_MODE_FIFO);
    assert_eq!(request.clipped, 1);
    assert!(request.queue_families.is_empty(), "EXCLUSIVE names none");
    assert_eq!(request.old_swapchain, None);
    // **The surface arrived as a token and not as the guest's number.** That is the whole of the
    // no-wild-handles claim for this call: what the host received is the token it issued, and what
    // the guest holds is an address in the boundary's data area.
    assert!(request.surface.is_some(), "the surface is a token: {request:?}");
    drop(log);

    let issued = up.f.vulkan().swapchain_handles();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].0 as u64, swapchain);
    // **On a slot boundary of its family's own range**, which is what makes a handle one byte off
    // name nothing and a handle from another family land somewhere else entirely.
    assert_eq!(swapchain as usize % HANDLE_SLOT_BYTES, 0);
    assert!(
        up.f.vulkan().report().contains(&format!("{swapchain:#x}")),
        "and the report names it, so a reader can correlate it with a dump"
    );
}

/// **A `pNext` chain, a wrong `sType`, a wild surface and a non-null `pAllocator` each refuse by
/// name**, and the allocator is counted.
#[test]
fn the_swapchain_create_info_is_checked_field_by_field() {
    let _serial = serialized();
    let up = up_to_a_device("swapchain-checks");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateSwapchainKHR");
    let good = up.f.swapchain_info(
        up.surface,
        2,
        FORMAT_B8G8R8A8_UNORM,
        800,
        600,
        SWAPCHAIN_USAGE,
        1,
        0,
    );
    let out = up.f.alloc(8);

    // A non-null `pAllocator`: counted first, then refused.
    let before = up.f.vulkan().allocator_calls();
    let text = up.f.refusal(create, &[up.device, good, 0xDEAD, out]).to_string();
    assert!(text.contains("pAllocator = 0xdead"), "{text}");
    assert_eq!(up.f.vulkan().allocator_calls(), before + 1, "counted before it refused");
    assert_eq!(up.f.vulkan().allocator_non_null(), 1);
    assert_eq!(up.f.vulkan().first_allocator_call().as_deref(), Some("vkCreateSwapchainKHR"));

    // A NULL `pCreateInfo` and a NULL `pSwapchain`.
    for (index, name) in [(1usize, "pCreateInfo"), (3, "pSwapchain")] {
        let mut args = [up.device, good, 0, out];
        args[index] = 0;
        let text = up.f.refusal(create, &args).to_string();
        assert!(text.contains(&format!("`{name} = NULL`")), "{text}");
    }

    // A wrong `sType`.
    let mut bytes = up.f.read_bytes(good, SWAPCHAIN_CREATE_INFO_BYTES);
    bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
    let wrong = up.f.bytes(&bytes);
    let text = up.f.refusal(create, &[up.device, wrong, 0, out]).to_string();
    assert!(text.contains("VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR"), "{text}");
    assert!(text.contains("byte 24"), "it names the field that would be misread: {text}");

    // A `pNext` chain.
    let mut bytes = up.f.read_bytes(good, SWAPCHAIN_CREATE_INFO_BYTES);
    bytes[8..16].copy_from_slice(&0x4000u64.to_le_bytes());
    let chained = up.f.bytes(&bytes);
    let text = up.f.refusal(create, &[up.device, chained, 0, out]).to_string();
    assert!(text.contains("pNext = 0x4000"), "{text}");
    assert!(text.contains("VkImageFormatListCreateInfo"), "it says what would be lost: {text}");

    // A `VkSurfaceKHR` the guest invented: one slot past the real one, which is inside the
    // surface registry's range and is still not a handle this layer issued.
    let mut bytes = up.f.read_bytes(good, SWAPCHAIN_CREATE_INFO_BYTES);
    bytes[24..32].copy_from_slice(&(up.surface + 16).to_le_bytes());
    let wild = up.f.bytes(&bytes);
    let text = up.f.refusal(create, &[up.device, wild, 0, out]).to_string();
    assert!(text.contains("`VkSurfaceKHR`"), "{text}");
    assert!(text.contains("dispatchable"), "{text}");

    // And a `VkQueue` where the `VkDevice` belongs.
    let text = up.f.refusal(create, &[up.queue, good, 0, out]).to_string();
    assert!(text.contains("`VkDevice`"), "{text}");
}

// ====================================================================== vkGetSwapchainImagesKHR

/// **The two-call protocol, the stable handles, and what a destroyed swapchain does to them.**
///
/// Three properties in one test because they are one property: the `VkImage` handles a guest
/// receives are the same in both halves of the protocol, are the same on a second pair of calls,
/// and **stop being handles at all** when the swapchain that owns them is destroyed.
#[test]
fn swapchain_images_are_stable_across_calls_and_die_with_their_swapchain() {
    let _serial = serialized();
    let up = up_to_a_device("swapchain-images");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateSwapchainKHR");
    let get_images = up.f.resolve_device(up.get_proc, up.device, "vkGetSwapchainImagesKHR");
    let destroy = up.f.resolve_device(up.get_proc, up.device, "vkDestroySwapchainKHR");

    let info = up
        .f
        .swapchain_info(up.surface, 2, FORMAT_B8G8R8A8_UNORM, 1024, 576, SWAPCHAIN_USAGE, 1, 0);
    let out = up.f.alloc(8);
    assert_eq!(up.f.call(create, [up.device, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let swapchain = up.f.guest.read_u64(out as GuestAddr);

    // The count-only half.
    let count_at = up.f.alloc(8);
    assert_eq!(
        up.f.call(get_images, [up.device, swapchain, count_at, 0]).expect("count") as i32,
        VK_SUCCESS
    );
    assert_eq!(up.f.read_u32(count_at), 3, "this driver reports three images");

    // The filling half.
    let array_at = up.f.alloc(3 * 8);
    assert_eq!(
        up.f.call(get_images, [up.device, swapchain, count_at, array_at]).expect("array") as i32,
        VK_SUCCESS
    );
    let first: Vec<u64> =
        (0..3).map(|i| up.f.guest.read_u64(array_at as GuestAddr + i * 8)).collect();
    assert!(first.iter().all(|handle| *handle != 0));
    assert_eq!(
        first.iter().collect::<std::collections::BTreeSet<_>>().len(),
        3,
        "three distinct handles"
    );

    // **Again**, into a different buffer: the same handles.
    up.f.guest.write_u32(count_at as GuestAddr, 3);
    let second_at = up.f.alloc(3 * 8);
    assert_eq!(
        up.f.call(get_images, [up.device, swapchain, count_at, second_at]).expect("again") as i32,
        VK_SUCCESS
    );
    let second: Vec<u64> =
        (0..3).map(|i| up.f.guest.read_u64(second_at as GuestAddr + i * 8)).collect();
    assert_eq!(first, second, "the same swapchain answers with the same VkImage handles");
    assert_eq!(up.f.vulkan().image_handles().len(), 3, "and three slots, not six");

    // A short buffer is `VK_INCOMPLETE` and writes nothing past the capacity.
    up.f.guest.write_u32(count_at as GuestAddr, 1);
    let short_at = up.f.poisoned(3 * 8, 0x5A);
    assert_eq!(
        up.f.call(get_images, [up.device, swapchain, count_at, short_at]).expect("short") as i32,
        VK_INCOMPLETE
    );
    assert_eq!(up.f.read_u32(count_at), 1, "the count written back is what fitted");
    assert_eq!(up.f.guest.read_u64(short_at as GuestAddr), first[0]);
    assert_eq!(
        up.f.guest.read_u64(short_at as GuestAddr + 8),
        u64::from_le_bytes([0x5A; 8]),
        "nothing was written past the guest's capacity"
    );

    // **Destroying the swapchain takes the image handles with it.**
    up.f.call(destroy, [up.device, swapchain, 0, 0]).expect("destroy");
    assert!(up.f.vulkan().swapchain_handles().is_empty());
    assert!(up.f.vulkan().image_handles().is_empty(), "a swapchain image's lifetime is its own");
    assert!(up.host.log().destroyed.contains(&"vkDestroySwapchainKHR".to_string()));

    // And a `VkImage` handle the guest kept now names nothing, which is the point.
    let view = up.f.resolve_device(up.get_proc, up.device, "vkCreateImageView");
    let view_info = up.f.image_view_info(first[0], FORMAT_B8G8R8A8_UNORM);
    let view_out = up.f.alloc(8);
    let text = up.f.refusal(view, &[up.device, view_info, 0, view_out]).to_string();
    assert!(text.contains("`VkImage`"), "{text}");
    assert!(text.contains("swapchain is destroyed"), "it says why it stopped being valid: {text}");

    // `VK_NULL_HANDLE` is the specified no-op and must not refuse.
    up.f.call(destroy, [up.device, 0, 0, 0]).expect("a null destroy is a no-op");
}

// ======================================================================= vkAcquireNextImageKHR

/// **The four answers `vkAcquireNextImageKHR` can give, and which two write `pImageIndex`.**
///
/// This is the resize test, and it is here rather than in the live one because no real driver can
/// be made to produce `VK_TIMEOUT`, `VK_SUBOPTIMAL_KHR` and `VK_ERROR_OUT_OF_DATE_KHR` on demand.
/// What it establishes:
///
/// * all four codes reach the guest's `X0` **verbatim** — not clamped, not normalised to
///   `VK_SUCCESS`, and not turned into a swapchain recreation this layer decided on;
/// * `pImageIndex` is written for `VK_SUCCESS` and `VK_SUBOPTIMAL_KHR`, because both produce an
///   image, and is **left untouched** for `VK_TIMEOUT` and `VK_ERROR_OUT_OF_DATE_KHR` — which the
///   poisoned buffer is what proves. Writing a zero there would hand the guest image 0, a real
///   image that may still be on the screen.
#[test]
fn the_acquire_codes_reach_the_guest_verbatim_and_only_two_write_an_index() {
    let _serial = serialized();
    let up = up_to_a_device("acquire-codes");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateSwapchainKHR");
    let acquire = up.f.resolve_device(up.get_proc, up.device, "vkAcquireNextImageKHR");
    let create_semaphore = up.f.resolve_device(up.get_proc, up.device, "vkCreateSemaphore");

    let info = up
        .f
        .swapchain_info(up.surface, 2, FORMAT_B8G8R8A8_UNORM, 1024, 576, SWAPCHAIN_USAGE, 1, 0);
    let out = up.f.alloc(8);
    assert_eq!(up.f.call(create, [up.device, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let swapchain = up.f.guest.read_u64(out as GuestAddr);

    let sem_info = up.f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    let sem_out = up.f.alloc(8);
    assert_eq!(
        up.f.call(create_semaphore, [up.device, sem_info, 0, sem_out]).expect("semaphore") as i32,
        VK_SUCCESS
    );
    let semaphore = up.f.guest.read_u64(sem_out as GuestAddr);

    {
        let mut scripted = up.host.acquires.lock().expect("no panic holds this");
        scripted.push_back(Acquired { result: VK_SUCCESS, image_index: Some(2) });
        scripted.push_back(Acquired { result: VK_SUBOPTIMAL_KHR, image_index: Some(1) });
        scripted.push_back(Acquired { result: VK_TIMEOUT, image_index: None });
        scripted.push_back(Acquired { result: VK_ERROR_OUT_OF_DATE_KHR, image_index: None });
    }

    const POISON: u8 = 0xC3;
    let poison_word = u64::from_le_bytes([POISON; 8]);
    for (expected, index) in [
        (VK_SUCCESS, Some(2u32)),
        (VK_SUBOPTIMAL_KHR, Some(1)),
        (VK_TIMEOUT, None),
        (VK_ERROR_OUT_OF_DATE_KHR, None),
    ] {
        let index_at = up.f.poisoned(8, POISON);
        let result = up
            .f
            .call_n(acquire, &[up.device, swapchain, u64::MAX, semaphore, 0, index_at])
            .expect("the call completes whatever the driver said");
        assert_eq!(
            result as i32, expected,
            "the driver's {expected} must reach the guest unchanged, and {result} did"
        );
        match index {
            Some(index) => assert_eq!(up.f.read_u32(index_at), index),
            None => assert_eq!(
                up.f.guest.read_u64(index_at as GuestAddr),
                poison_word,
                "{expected} writes no image index, so the guest's own variable is untouched"
            ),
        }
    }

    // The two resize codes are in the driver-result log, because a run that "worked but looked
    // wrong" is exactly what that log is for.
    let failures = up.f.vulkan().driver_failures();
    assert!(failures
        .iter()
        .any(|(call, r)| call == "vkAcquireNextImageKHR" && *r == VK_SUBOPTIMAL_KHR));
    assert!(failures
        .iter()
        .any(|(call, r)| call == "vkAcquireNextImageKHR" && *r == VK_ERROR_OUT_OF_DATE_KHR));

    // A `VkFence` where the `VkSemaphore` belongs: two bare 64-bit values, told apart only by
    // which registry range they are in.
    let create_fence = up.f.resolve_device(up.get_proc, up.device, "vkCreateFence");
    let fence_info = up.f.flags_only_info(STYPE_FENCE_CREATE_INFO, 0);
    let fence_out = up.f.alloc(8);
    assert_eq!(
        up.f.call(create_fence, [up.device, fence_info, 0, fence_out]).expect("fence") as i32,
        VK_SUCCESS
    );
    let fence = up.f.guest.read_u64(fence_out as GuestAddr);
    let index_at = up.f.alloc(8);
    let text =
        up.f.refusal(acquire, &[up.device, swapchain, u64::MAX, fence, 0, index_at]).to_string();
    assert!(text.contains("`VkSemaphore`"), "the fence was passed as the semaphore: {text}");
    assert!(text.contains("nothing in them to tell one from the other"), "{text}");
}

// ==================================================================== vkCmdPipelineBarrier

/// **`vkCmdPipelineBarrier`'s ninth and tenth arguments are on the stack, and they are read.**
///
/// The test this stage most needed. AAPCS64 puts the first eight integer arguments in `X0`-`X7`
/// and spills the rest, and `imageMemoryBarrierCount`/`pImageMemoryBarriers` are the ninth and
/// tenth — the two the whole call is about. A handler that stopped at `X7` would record a barrier
/// with an empty image list, every call would answer, and the frame would be cleared in the wrong
/// layout with no diagnostic anywhere on this machine.
///
/// It also establishes that a **buffer** memory barrier is a refusal naming the count rather than
/// a barrier silently dropped.
#[test]
fn the_barriers_stacked_arguments_are_read_and_a_buffer_barrier_refuses() {
    let _serial = serialized();
    let up = up_to_a_device("barrier");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateSwapchainKHR");
    let get_images = up.f.resolve_device(up.get_proc, up.device, "vkGetSwapchainImagesKHR");
    let create_pool = up.f.resolve_device(up.get_proc, up.device, "vkCreateCommandPool");
    let allocate = up.f.resolve_device(up.get_proc, up.device, "vkAllocateCommandBuffers");
    let barrier = up.f.resolve_device(up.get_proc, up.device, "vkCmdPipelineBarrier");

    let info = up
        .f
        .swapchain_info(up.surface, 2, FORMAT_B8G8R8A8_UNORM, 1024, 576, SWAPCHAIN_USAGE, 1, 0);
    let out = up.f.alloc(8);
    up.f.call(create, [up.device, info, 0, out]).expect("create");
    let swapchain = up.f.guest.read_u64(out as GuestAddr);
    let count_at = up.f.alloc(8);
    up.f.call(get_images, [up.device, swapchain, count_at, 0]).expect("count");
    let array_at = up.f.alloc(3 * 8);
    up.f.call(get_images, [up.device, swapchain, count_at, array_at]).expect("array");
    let image = up.f.guest.read_u64(array_at as GuestAddr);

    let pool_info = up.f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
    let pool_out = up.f.alloc(8);
    assert_eq!(
        up.f.call(create_pool, [up.device, pool_info, 0, pool_out]).expect("pool") as i32,
        VK_SUCCESS
    );
    let pool = up.f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = up.f.command_buffer_allocate_info(pool, 1);
    let buffers_at = up.f.alloc(8);
    assert_eq!(
        up.f.call(allocate, [up.device, allocate_info, buffers_at, 0]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let command = up.f.guest.read_u64(buffers_at as GuestAddr);
    assert_ne!(command, 0);

    let image_barrier = up.f.image_barrier(
        STYPE_IMAGE_MEMORY_BARRIER,
        0,
        ACCESS_TRANSFER_WRITE,
        LAYOUT_UNDEFINED,
        LAYOUT_TRANSFER_DST,
        image,
    );
    let barriers_at = up.f.bytes(&image_barrier);
    up.f.call_n(
        barrier,
        &[
            command,
            u64::from(STAGE_TOP_OF_PIPE),
            u64::from(STAGE_TRANSFER),
            0,           // dependencyFlags
            0,           // memoryBarrierCount
            0,           // pMemoryBarriers
            0,           // bufferMemoryBarrierCount
            0,           // pBufferMemoryBarriers
            1,           // imageMemoryBarrierCount  <- the ninth, on the stack
            barriers_at, // pImageMemoryBarriers     <- the tenth, on the stack
        ],
    )
    .expect("the barrier records");

    let log = up.host.log();
    assert_eq!(log.barriers.len(), 1, "the barrier reached the host");
    let recorded = &log.barriers[0];
    assert_eq!(recorded.src_stage, STAGE_TOP_OF_PIPE);
    assert_eq!(recorded.dst_stage, STAGE_TRANSFER);
    assert_eq!(
        recorded.image_barriers.len(),
        1,
        "the ninth and tenth arguments came off the stack: {recorded:?}"
    );
    let entry = &recorded.image_barriers[0];
    assert_eq!(entry.dst_access, ACCESS_TRANSFER_WRITE);
    assert_eq!(entry.old_layout, LAYOUT_UNDEFINED);
    assert_eq!(entry.new_layout, LAYOUT_TRANSFER_DST);
    assert_eq!(entry.src_queue_family, QUEUE_FAMILY_IGNORED);
    assert!(matches!(entry.image, HostImageRef::Swapchain(_)), "the image is a token");
    assert_eq!(entry.subresource_range.len(), IMAGE_SUBRESOURCE_RANGE_BYTES);
    drop(log);

    // **A buffer memory barrier refuses**, because stage 4 has no `VkBuffer` registry.
    let text = up.f.refusal(barrier, &[command, 1, 1, 0, 0, 0, 1, 0x9000, 0, 0]).to_string();
    assert!(text.contains("bufferMemoryBarrierCount = 1"), "{text}");
    assert!(text.contains("no `VkBuffer` registry"), "{text}");
    assert!(text.contains("stage 5"), "it says where buffers arrive: {text}");

    // A `VkImage` the guest invented, inside the barrier the stack argument points at.
    let mut wild = image_barrier.clone();
    wild[40..48].copy_from_slice(&(image + 8).to_le_bytes());
    let wild_at = up.f.bytes(&wild);
    let text = up.f.refusal(barrier, &[command, 1, 1, 0, 0, 0, 0, 0, 1, wild_at]).to_string();
    assert!(text.contains("`VkImage`"), "{text}");

    // And a barrier whose `sType` is the *buffer* one — the mistake most likely to be made,
    // because the two structures are adjacent in the header and `image` at 40 is a buffer's
    // offset field.
    let mut mistyped = image_barrier.clone();
    mistyped[0..4].copy_from_slice(&STYPE_BUFFER_MEMORY_BARRIER.to_le_bytes());
    let mistyped_at = up.f.bytes(&mistyped);
    let text = up.f.refusal(barrier, &[command, 1, 1, 0, 0, 0, 0, 0, 1, mistyped_at]).to_string();
    assert!(text.contains("VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER"), "{text}");
    assert!(text.contains("the buffer's offset"), "it names the confusion: {text}");
}

// =================================================== vkCmdClearColorImage and vkQueuePresentKHR

/// **The clear colour's sixteen bytes travel verbatim, and `pResults` is filled per swapchain.**
///
/// Two properties that are the same property twice: this layer must not interpret a value whose
/// meaning belongs to somebody else. Which member of `VkClearColorValue` is live is decided by the
/// image's format, and which `VkResult` belongs to which swapchain is decided by the driver.
#[test]
fn the_clear_colour_travels_as_bytes_and_present_results_are_per_swapchain() {
    let _serial = serialized();
    let up = up_to_a_device("clear-present");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateSwapchainKHR");
    let get_images = up.f.resolve_device(up.get_proc, up.device, "vkGetSwapchainImagesKHR");
    let create_pool = up.f.resolve_device(up.get_proc, up.device, "vkCreateCommandPool");
    let allocate = up.f.resolve_device(up.get_proc, up.device, "vkAllocateCommandBuffers");
    let clear = up.f.resolve_device(up.get_proc, up.device, "vkCmdClearColorImage");
    let present = up.f.resolve_device(up.get_proc, up.device, "vkQueuePresentKHR");
    let create_semaphore = up.f.resolve_device(up.get_proc, up.device, "vkCreateSemaphore");

    let info = up
        .f
        .swapchain_info(up.surface, 2, FORMAT_B8G8R8A8_UNORM, 1024, 576, SWAPCHAIN_USAGE, 1, 0);
    let out = up.f.alloc(8);
    up.f.call(create, [up.device, info, 0, out]).expect("create");
    let swapchain = up.f.guest.read_u64(out as GuestAddr);
    let count_at = up.f.alloc(8);
    up.f.call(get_images, [up.device, swapchain, count_at, 0]).expect("count");
    let array_at = up.f.alloc(3 * 8);
    up.f.call(get_images, [up.device, swapchain, count_at, array_at]).expect("array");
    let image = up.f.guest.read_u64(array_at as GuestAddr);

    let pool_info = up.f.command_pool_info(0, 0);
    let pool_out = up.f.alloc(8);
    up.f.call(create_pool, [up.device, pool_info, 0, pool_out]).expect("pool");
    let pool = up.f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = up.f.command_buffer_allocate_info(pool, 1);
    let buffers_at = up.f.alloc(8);
    up.f.call(allocate, [up.device, allocate_info, buffers_at, 0]).expect("allocate");
    let command = up.f.guest.read_u64(buffers_at as GuestAddr);

    // **A colour whose bytes are not a plausible float**, so that a layer which reinterpreted the
    // union would produce something different. These sixteen bytes are `uint32[4]` as far as
    // anybody here is concerned, and the point is that nothing in the path decides that.
    let colour_bytes: [u8; 16] = [
        0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 0xFF, 0x00, 0xFF, 0x00, 0x7F, 0x80, 0x81,
        0x82,
    ];
    let colour_at = up.f.bytes(&colour_bytes);
    let range_at = up.f.bytes(&up.f.whole_colour_range());
    up.f.call_n(clear, &[command, image, u64::from(LAYOUT_TRANSFER_DST), colour_at, 1, range_at])
        .expect("the clear records");

    let log = up.host.log();
    assert_eq!(log.clears.len(), 1);
    let (layout, colour, ranges) = &log.clears[0];
    assert_eq!(*layout, LAYOUT_TRANSFER_DST);
    assert_eq!(*colour, colour_bytes, "the union's sixteen bytes arrived untouched");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].len(), IMAGE_SUBRESOURCE_RANGE_BYTES);
    assert_eq!(ranges[0][0..4], ASPECT_COLOR.to_le_bytes());
    drop(log);

    // A `rangeCount` of zero refuses: it records successfully and clears nothing, which is the
    // plausible success rule 1 exists for.
    let text = up
        .f
        .refusal(clear, &[command, image, u64::from(LAYOUT_TRANSFER_DST), colour_at, 0, range_at])
        .to_string();
    assert!(text.contains("rangeCount = 0"), "{text}");

    // Now present, with a `pResults` the driver fills.
    let sem_info = up.f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    let sem_out = up.f.alloc(8);
    up.f.call(create_semaphore, [up.device, sem_info, 0, sem_out]).expect("semaphore");
    let semaphore = up.f.guest.read_u64(sem_out as GuestAddr);

    up.host
        .presents
        .lock()
        .expect("no panic")
        .push_back(Presented { result: VK_SUBOPTIMAL_KHR, per_swapchain: vec![VK_SUBOPTIMAL_KHR] });
    let index_at = up.f.u32_array(&[0]);
    let results_at = up.f.poisoned(8, 0x11);
    let present_info = up.f.present_info(semaphore, swapchain, index_at, results_at);
    let result = up.f.call(present, [up.queue, present_info, 0, 0]).expect("present");
    assert_eq!(
        result as i32, VK_SUBOPTIMAL_KHR,
        "the driver's VK_SUBOPTIMAL_KHR reaches the guest verbatim -- it is a success code and \
         the frame was presented"
    );
    assert_eq!(up.f.read_u32(results_at) as i32, VK_SUBOPTIMAL_KHR, "and pResults was filled");

    let log = up.host.log();
    assert_eq!(log.presents.len(), 1);
    assert!(
        log.presents[0].wants_per_swapchain_results,
        "the host was told pResults was asked for"
    );
    assert_eq!(log.presents[0].swapchains.len(), 1);
    assert_eq!(log.presents[0].waits.len(), 1, "the wait semaphore arrived as a token");
    drop(log);

    // A present naming no swapchain refuses: it would succeed and put nothing on any screen.
    let mut bytes = up.f.read_bytes(present_info, PRESENT_INFO_BYTES);
    bytes[32..36].copy_from_slice(&0u32.to_le_bytes());
    let empty = up.f.bytes(&bytes);
    let text = up.f.refusal(present, &[up.queue, empty, 0, 0]).to_string();
    assert!(text.contains("swapchainCount = 0"), "{text}");
    assert!(text.contains("rule 1"), "{text}");
}

// ======================================================= the frame loop's objects and teardown

/// **Every object stage 4 creates is destroyed, and the registries return to empty.**
///
/// The leak test, and the one that says stage 4's registries are an *inventory* rather than a
/// counter: stage 3's five only ever grew, because nothing it implemented destroyed anything.
/// These fall, and a renderer that recreates its swapchain on every resize depends on it — with no
/// removal, `MAX_SWAPCHAINS` would be reached in a second of dragging a window.
#[test]
fn every_stage_four_object_is_destroyed_and_the_registries_return_to_empty() {
    let _serial = serialized();
    let up = up_to_a_device("teardown");
    let name = |n: &str| up.f.resolve_device(up.get_proc, up.device, n);
    let create = name("vkCreateSwapchainKHR");
    let get_images = name("vkGetSwapchainImagesKHR");
    let destroy_swapchain = name("vkDestroySwapchainKHR");
    let create_view = name("vkCreateImageView");
    let destroy_view = name("vkDestroyImageView");
    let create_sem = name("vkCreateSemaphore");
    let destroy_sem = name("vkDestroySemaphore");
    let create_fence = name("vkCreateFence");
    let destroy_fence = name("vkDestroyFence");
    let create_pool = name("vkCreateCommandPool");
    let destroy_pool = name("vkDestroyCommandPool");
    let allocate = name("vkAllocateCommandBuffers");
    let free = name("vkFreeCommandBuffers");

    let info = up
        .f
        .swapchain_info(up.surface, 2, FORMAT_B8G8R8A8_UNORM, 1024, 576, SWAPCHAIN_USAGE, 1, 0);
    let out = up.f.alloc(8);
    up.f.call(create, [up.device, info, 0, out]).expect("create");
    let swapchain = up.f.guest.read_u64(out as GuestAddr);
    let count_at = up.f.alloc(8);
    up.f.call(get_images, [up.device, swapchain, count_at, 0]).expect("count");
    let array_at = up.f.alloc(3 * 8);
    up.f.call(get_images, [up.device, swapchain, count_at, array_at]).expect("array");
    let images: Vec<u64> =
        (0..3).map(|i| up.f.guest.read_u64(array_at as GuestAddr + i * 8)).collect();

    // One view per image, as every renderer does immediately after enumerating them.
    let mut views = Vec::new();
    for image in &images {
        let view_info = up.f.image_view_info(*image, FORMAT_B8G8R8A8_UNORM);
        let view_out = up.f.alloc(8);
        assert_eq!(
            up.f.call(create_view, [up.device, view_info, 0, view_out]).expect("view") as i32,
            VK_SUCCESS
        );
        views.push(up.f.guest.read_u64(view_out as GuestAddr));
    }
    assert_eq!(up.f.vulkan().image_view_handles().len(), 3);
    assert_eq!(up.host.log().views.len(), 3);
    assert!(matches!(up.host.log().views[0].image, HostImageRef::Swapchain(_)));
    assert_eq!(up.host.log().views[0].view_type, VIEW_TYPE_2D);
    assert_eq!(
        up.host.log().views[0].subresource_range.len(),
        IMAGE_SUBRESOURCE_RANGE_BYTES,
        "the range travelled as its twenty bytes"
    );

    let sem_info = up.f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    let mut semaphores = Vec::new();
    for _ in 0..2 {
        let sem_out = up.f.alloc(8);
        up.f.call(create_sem, [up.device, sem_info, 0, sem_out]).expect("semaphore");
        semaphores.push(up.f.guest.read_u64(sem_out as GuestAddr));
    }
    let fence_info = up.f.flags_only_info(STYPE_FENCE_CREATE_INFO, 1); // SIGNALED
    let fence_out = up.f.alloc(8);
    up.f.call(create_fence, [up.device, fence_info, 0, fence_out]).expect("fence");
    let fence = up.f.guest.read_u64(fence_out as GuestAddr);

    let pool_info = up.f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
    let pool_out = up.f.alloc(8);
    up.f.call(create_pool, [up.device, pool_info, 0, pool_out]).expect("pool");
    let pool = up.f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = up.f.command_buffer_allocate_info(pool, 1);
    let buffers_at = up.f.alloc(8);
    up.f.call(allocate, [up.device, allocate_info, buffers_at, 0]).expect("allocate");

    assert_eq!(up.f.vulkan().swapchain_handles().len(), 1);
    assert_eq!(up.f.vulkan().image_handles().len(), 3);
    assert_eq!(up.f.vulkan().semaphore_handles().len(), 2);
    assert_eq!(up.f.vulkan().fence_handles().len(), 1);
    assert_eq!(up.f.vulkan().command_pool_handles().len(), 1);
    assert_eq!(up.f.vulkan().command_buffer_handles().len(), 1);

    // Tear it down the way a renderer does.
    up.f.call_n(free, &[up.device, pool, 1, buffers_at]).expect("free");
    assert!(up.f.vulkan().command_buffer_handles().is_empty());
    for view in &views {
        up.f.call(destroy_view, [up.device, *view, 0, 0]).expect("destroy view");
    }
    for semaphore in &semaphores {
        up.f.call(destroy_sem, [up.device, *semaphore, 0, 0]).expect("destroy semaphore");
    }
    up.f.call(destroy_fence, [up.device, fence, 0, 0]).expect("destroy fence");
    up.f.call(destroy_pool, [up.device, pool, 0, 0]).expect("destroy pool");
    up.f.call(destroy_swapchain, [up.device, swapchain, 0, 0]).expect("destroy swapchain");

    assert!(up.f.vulkan().swapchain_handles().is_empty(), "a swapchain");
    assert!(up.f.vulkan().image_handles().is_empty(), "its images went with it");
    assert!(up.f.vulkan().image_view_handles().is_empty(), "views");
    assert!(up.f.vulkan().semaphore_handles().is_empty(), "semaphores");
    assert!(up.f.vulkan().fence_handles().is_empty(), "fences");
    assert!(up.f.vulkan().command_pool_handles().is_empty(), "pools");
    assert!(up.f.vulkan().command_buffer_handles().is_empty(), "buffers");

    // Every destroyed handle now names nothing, which is what a stale handle must do.
    let text = up.f.refusal(destroy_view, &[up.device, views[0], 0, 0]).to_string();
    assert!(text.contains("`VkImageView`"), "{text}");

    // And the report says the registries are empty rather than printing nothing.
    let report = up.f.vulkan().report();
    assert!(report.contains("stage 4 handles live now"), "{report}");
    assert!(report.contains("VkSwapchainKHR 0/"), "{report}");
    assert!(report.contains("VkCommandBuffer 0/"), "{report}");
}

/// **A destroyed command pool takes its command buffers' handles with it**, separately from the
/// test above, because this one is the *dispatchable* cascade.
///
/// A `VkCommandBuffer` left in the registry after its pool is destroyed resolves to a token whose
/// host object is freed memory — and a driver dereferences a `VkCommandBuffer`'s first word as a
/// dispatch table. Global Constraint 11 calls that Critical, and it is reachable with a handle
/// this layer itself issued.
#[test]
fn destroying_a_command_pool_takes_its_dispatchable_buffer_handles_with_it() {
    let _serial = serialized();
    let up = up_to_a_device("pool-cascade");
    let name = |n: &str| up.f.resolve_device(up.get_proc, up.device, n);
    let create_pool = name("vkCreateCommandPool");
    let allocate = name("vkAllocateCommandBuffers");
    let destroy_pool = name("vkDestroyCommandPool");
    let begin = name("vkBeginCommandBuffer");

    let pool_info = up.f.command_pool_info(0, 0);
    let pool_out = up.f.alloc(8);
    up.f.call(create_pool, [up.device, pool_info, 0, pool_out]).expect("pool");
    let pool = up.f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = up.f.command_buffer_allocate_info(pool, 1);
    let buffers_at = up.f.alloc(8);
    up.f.call(allocate, [up.device, allocate_info, buffers_at, 0]).expect("allocate");
    let command = up.f.guest.read_u64(buffers_at as GuestAddr);
    assert_eq!(up.f.vulkan().command_buffer_handles().len(), 1);

    // It works before.
    let begin_info = up.f.begin_info(ONE_TIME_SUBMIT, 0);
    assert_eq!(up.f.call(begin, [command, begin_info, 0, 0]).expect("begin") as i32, VK_SUCCESS);

    up.f.call(destroy_pool, [up.device, pool, 0, 0]).expect("destroy pool");
    assert!(up.f.vulkan().command_buffer_handles().is_empty(), "the pool took them");

    let text = up.f.refusal(begin, &[command, begin_info, 0, 0]).to_string();
    assert!(text.contains("`VkCommandBuffer`"), "{text}");
    assert!(text.contains("dispatchable"), "{text}");
    assert!(text.contains("Global Constraint 11"), "it names why this one is Critical: {text}");

    // A `pInheritanceInfo` is refused rather than ignored, because stage 4 has no render pass to
    // resolve the three handles it names against.
    let pool_out = up.f.alloc(8);
    up.f.call(create_pool, [up.device, pool_info, 0, pool_out]).expect("pool");
    let pool = up.f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = up.f.command_buffer_allocate_info(pool, 1);
    let buffers_at = up.f.alloc(8);
    up.f.call(allocate, [up.device, allocate_info, buffers_at, 0]).expect("allocate");
    let command = up.f.guest.read_u64(buffers_at as GuestAddr);
    let inherited = up.f.begin_info(0, 0x7000);
    let text = up.f.refusal(begin, &[command, inherited, 0, 0]).to_string();
    assert!(text.contains("pInheritanceInfo = 0x7000"), "{text}");
    assert!(text.contains("stage 4 has none of those"), "{text}");
}

/// **`vkWaitForFences` forwards the driver's code and does not clamp the timeout.**
#[test]
fn wait_for_fences_forwards_the_timeout_and_the_boolean_unchanged() {
    let _serial = serialized();
    let up = up_to_a_device("fences");
    let create_fence = up.f.resolve_device(up.get_proc, up.device, "vkCreateFence");
    let wait = up.f.resolve_device(up.get_proc, up.device, "vkWaitForFences");

    let fence_info = up.f.flags_only_info(STYPE_FENCE_CREATE_INFO, 0);
    let mut fences = Vec::new();
    for _ in 0..2 {
        let out = up.f.alloc(8);
        up.f.call(create_fence, [up.device, fence_info, 0, out]).expect("fence");
        fences.push(up.f.guest.read_u64(out as GuestAddr));
    }
    let array = up.f.u64_array(&fences);

    // `UINT64_MAX` means "wait forever" and must arrive as that number, not as a clamp.
    assert_eq!(
        up.f.call_n(wait, &[up.device, 2, array, 1, u64::MAX]).expect("wait") as i32,
        VK_SUCCESS
    );
    // A `VkBool32` is true for **any** non-zero value, which is what a C `!x` idiom produces.
    assert_eq!(
        up.f.call_n(wait, &[up.device, 2, array, 0xFFFF_FFFF, 1_000_000]).expect("wait") as i32,
        VK_SUCCESS
    );
    assert_eq!(up.f.call_n(wait, &[up.device, 2, array, 0, 0]).expect("wait") as i32, VK_SUCCESS);

    let log = up.host.log();
    assert_eq!(log.waits.len(), 3);
    assert!(log.waits[0].1, "waitAll = 1");
    assert_eq!(log.waits[0].2, u64::MAX, "the timeout is not clamped");
    assert!(log.waits[1].1, "waitAll = 0xFFFFFFFF is still true");
    assert_eq!(log.waits[1].2, 1_000_000);
    assert!(!log.waits[2].1, "and zero is false");
    assert_eq!(log.waits[0].0.len(), 2, "both fences arrived as tokens");
    drop(log);

    // A list with one wild handle is refused whole: waiting for fewer fences than the guest named
    // is, with `waitAll`, the difference between "every frame has finished" and "some have".
    let wild = up.f.u64_array(&[fences[0], fences[1] + 8]);
    let text = up.f.refusal(wait, &[up.device, 2, wild, 1, 0]).to_string();
    assert!(text.contains("`VkFence`"), "{text}");

    // And a count of zero refuses, because `VK_SUCCESS` for a wait on nothing tells a frame loop
    // that work it never submitted has finished.
    let text = up.f.refusal(wait, &[up.device, 0, array, 1, 0]).to_string();
    assert!(text.contains("`fenceCount` was zero"), "{text}");
    assert!(text.contains("never submitted"), "{text}");
}

/// **`vkQueueSubmit` resolves every handle in a `VkSubmitInfo` through this layer's registries.**
///
/// The structure names three arrays of handles and one of stage masks, and
/// `pWaitDstStageMask` is the member a reader forgets is an *array*: it is one
/// `VkPipelineStageFlags` per wait semaphore, not a scalar, so a shim that read it as one would
/// hand the driver a pointer value as a stage mask.
#[test]
fn a_submit_info_arrives_with_every_handle_resolved_and_the_stage_masks_paired() {
    let _serial = serialized();
    let up = up_to_a_device("submit");
    let name = |n: &str| up.f.resolve_device(up.get_proc, up.device, n);
    let create_pool = name("vkCreateCommandPool");
    let allocate = name("vkAllocateCommandBuffers");
    let create_sem = name("vkCreateSemaphore");
    let create_fence = name("vkCreateFence");
    let submit = name("vkQueueSubmit");

    let pool_info = up.f.command_pool_info(0, 0);
    let pool_out = up.f.alloc(8);
    up.f.call(create_pool, [up.device, pool_info, 0, pool_out]).expect("pool");
    let pool = up.f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = up.f.command_buffer_allocate_info(pool, 1);
    let buffers_at = up.f.alloc(8);
    up.f.call(allocate, [up.device, allocate_info, buffers_at, 0]).expect("allocate");
    let command = up.f.guest.read_u64(buffers_at as GuestAddr);

    let sem_info = up.f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    let mut semaphores = Vec::new();
    for _ in 0..2 {
        let out = up.f.alloc(8);
        up.f.call(create_sem, [up.device, sem_info, 0, out]).expect("semaphore");
        semaphores.push(up.f.guest.read_u64(out as GuestAddr));
    }
    let fence_info = up.f.flags_only_info(STYPE_FENCE_CREATE_INFO, 0);
    let fence_out = up.f.alloc(8);
    up.f.call(create_fence, [up.device, fence_info, 0, fence_out]).expect("fence");
    let fence = up.f.guest.read_u64(fence_out as GuestAddr);

    let info = up.f.submit_info(semaphores[0], STAGE_TRANSFER, command, semaphores[1]);
    assert_eq!(
        up.f.call(submit, [up.queue, 1, info, fence]).expect("submit") as i32,
        VK_SUCCESS
    );

    let log = up.host.log();
    assert_eq!(log.submits.len(), 1);
    let (_, submits, submitted_fence) = &log.submits[0];
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0].waits.len(), 1);
    assert_eq!(submits[0].waits[0].1, STAGE_TRANSFER, "the stage mask came from its own array");
    assert_eq!(submits[0].command_buffers.len(), 1);
    assert_eq!(submits[0].signals.len(), 1);
    assert_ne!(
        submits[0].waits[0].0, submits[0].signals[0],
        "the wait and the signal are different semaphores, so neither array was read twice"
    );
    assert!(submitted_fence.is_some(), "the fence arrived as a token");
    drop(log);

    // A `VkSemaphore` the guest invented, inside the array the structure points at.
    let wild_waits = up.f.u64_array(&[semaphores[0] + 8]);
    let mut bytes = up.f.read_bytes(info, SUBMIT_INFO_BYTES);
    bytes[24..32].copy_from_slice(&wild_waits.to_le_bytes());
    let wild = up.f.bytes(&bytes);
    let text = up.f.refusal(submit, &[up.queue, 1, wild, fence]).to_string();
    assert!(text.contains("`VkSemaphore`"), "{text}");

    // And a `VkDevice` where the `VkQueue` belongs -- both dispatchable, and this is the only
    // thing that tells them apart.
    let text = up.f.refusal(submit, &[up.device, 1, info, fence]).to_string();
    assert!(text.contains("`VkQueue`"), "{text}");
    assert!(text.contains("vkGetDeviceQueue"), "it names what issues one: {text}");
}

// ============================================================================== the live test

/// **A frame on the real screen, from assembled ARM64 — and the presented pixels read back.**
///
/// This is the evidence stage 4 owes, and the reason it is not "`VkResult == 0`" is
/// `VERIFICATION.md` entry 11: every call in this sequence could answer zero without a single
/// pixel changing. So the last thing it does is read the swapchain image back off the GPU and
/// assert the colour, byte for byte, in R G B A order.
///
/// The chain, all of it driven from translated ARM64 through guest thunks:
///
/// ```text
/// dlopen -> dlsym -> vkGetInstanceProcAddr -> vkCreateInstance
///   -> vkCreateAndroidSurfaceKHR (satisfied by vkCreateWin32SurfaceKHR)
///   -> vkEnumeratePhysicalDevices, the surface queries, vkCreateDevice, vkGetDeviceQueue
///   -> vkGetDeviceProcAddr for every name below
///   -> vkCreateSwapchainKHR, vkGetSwapchainImagesKHR, vkCreateImageView
///   -> vkCreateSemaphore x2, vkCreateFence, vkCreateCommandPool, vkAllocateCommandBuffers
///   -> vkAcquireNextImageKHR
///   -> vkBeginCommandBuffer, vkCmdPipelineBarrier, vkCmdClearColorImage,
///      vkCmdPipelineBarrier, vkEndCommandBuffer
///   -> vkQueueSubmit, vkWaitForFences, vkQueuePresentKHR, vkQueueWaitIdle, vkDeviceWaitIdle
///   -> the pixels, read back and asserted
/// ```
#[test]
#[ignore = "opens a window and the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_frame_is_presented_from_guest_code_and_the_pixels_read_back_are_the_colour_it_cleared_to() {
    require_gate();
    let _serial = serialized();

    let mut window = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
        "Omnidroid — Vulkan stage 4: a guest frame",
        1024,
        576,
    ))
    .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    let _ = window.poll_events().count();
    let source = HostWindowSource::watching(&window).expect("a source watching the window");
    let client = source.geometry().expect("the window has a client area");

    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture("live-stage4", Some(host.clone()));
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let surface = f.a_surface(entry_point, instance);

    // ---------------------------------------------------------------- choose a device and family
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let device_count = f.read_u32(count_at) as usize;
    assert!(device_count > 0, "a machine with a Vulkan loader and no GPU cannot present");
    let devices_at = f.alloc(device_count * 8);
    assert_eq!(
        f.call(enumerate, [instance, count_at, devices_at, 0]).expect("array") as i32,
        VK_SUCCESS
    );
    let physical_devices: Vec<u64> =
        (0..device_count).map(|i| f.guest.read_u64(devices_at as GuestAddr + i * 8)).collect();

    let properties = f.resolve(entry_point, instance, "vkGetPhysicalDeviceProperties");
    let families = f.resolve(entry_point, instance, "vkGetPhysicalDeviceQueueFamilyProperties");
    let support = f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceSupportKHR");
    let capabilities =
        f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceCapabilitiesKHR");
    let formats = f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceFormatsKHR");

    let scratch = f.alloc(SCRATCH_BYTES);
    let supported_at = f.alloc(8);
    let properties_at = f.alloc(PHYSICAL_DEVICE_PROPERTIES_BYTES);
    let mut chosen: Option<(u64, String, u32)> = None;
    for physical in &physical_devices {
        f.call(properties, [*physical, properties_at, 0, 0]).expect("properties");
        let bytes = f.read_bytes(properties_at, PHYSICAL_DEVICE_PROPERTIES_BYTES);
        let end = bytes[20..276].iter().position(|b| *b == 0).expect("a NUL-terminated deviceName");
        let name = String::from_utf8_lossy(&bytes[20..20 + end]).into_owned();

        f.call(families, [*physical, count_at, 0, 0]).expect("family count");
        let family_count = f.read_u32(count_at) as usize;
        assert!(family_count * QUEUE_FAMILY_PROPERTIES_BYTES <= SCRATCH_BYTES);
        f.guest.write_u32(count_at as GuestAddr, family_count as u32);
        f.call(families, [*physical, count_at, scratch, 0]).expect("families");
        let family_bytes = f.read_bytes(scratch, family_count * QUEUE_FAMILY_PROPERTIES_BYTES);
        for index in 0..family_count {
            let entry = &family_bytes[index * QUEUE_FAMILY_PROPERTIES_BYTES..];
            let flags = u32::from_le_bytes(entry[0..4].try_into().expect("four"));
            let queues = u32::from_le_bytes(entry[4..8].try_into().expect("four"));
            if flags & 0x1 == 0 || queues == 0 {
                continue; // not VK_QUEUE_GRAPHICS_BIT
            }
            let result = f
                .call(support, [*physical, index as u64, surface, supported_at])
                .expect("surface support");
            assert_eq!(result as i32, VK_SUCCESS);
            if f.read_u32(supported_at) == 1 {
                chosen = Some((*physical, name.clone(), index as u32));
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
    }
    let (physical, device_name, family) = chosen.expect(
        "this machine has a Vulkan driver and a window, so some queue family must be able to \
         render and present to a surface over that window",
    );

    // ------------------------------------------------------------ the surface's own terms
    let capabilities_at = f.alloc(SURFACE_CAPABILITIES_BYTES);
    assert_eq!(
        f.call(capabilities, [physical, surface, capabilities_at, 0]).expect("capabilities") as i32,
        VK_SUCCESS
    );
    let caps = f.read_bytes(capabilities_at, SURFACE_CAPABILITIES_BYTES);
    let read_u32 = |bytes: &[u8], at: usize| {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"))
    };
    let min_images = read_u32(&caps, 0);
    let extent = (read_u32(&caps, 8), read_u32(&caps, 12));
    let current_transform = read_u32(&caps, 40);
    let supported_usage = read_u32(&caps, 48);
    assert_eq!(
        supported_usage & SWAPCHAIN_USAGE,
        SWAPCHAIN_USAGE,
        "this surface does not support TRANSFER_SRC on its swapchain images, so the presented \
         frame cannot be read back at all -- which is a fact about this driver and not a reason \
         to pretend the test passed. supportedUsageFlags = {supported_usage:#x}"
    );

    assert_eq!(
        f.call(formats, [physical, surface, count_at, 0]).expect("format count") as i32,
        VK_SUCCESS
    );
    let format_count = f.read_u32(count_at) as usize;
    assert!(format_count > 0);
    assert!(format_count * SURFACE_FORMAT_BYTES <= SCRATCH_BYTES);
    f.guest.write_u32(count_at as GuestAddr, format_count as u32);
    assert_eq!(
        f.call(formats, [physical, surface, count_at, scratch]).expect("formats") as i32,
        VK_SUCCESS
    );
    let format_bytes = f.read_bytes(scratch, format_count * SURFACE_FORMAT_BYTES);
    let offered: Vec<u32> =
        (0..format_count).map(|i| read_u32(&format_bytes, i * SURFACE_FORMAT_BYTES)).collect();
    // **A `UNORM` format, deliberately.** `vkCmdClearColorImage` on an `_SRGB` image raises the
    // question of whether the clear value is encoded on the way in, which implementations have not
    // always agreed on -- and this test is about whether the frame reached the screen, not about
    // colour-space semantics. A `UNORM` swapchain stores `round(value * 255)` and the assertion is
    // exact.
    let format = *offered
        .iter()
        .find(|f| **f == FORMAT_B8G8R8A8_UNORM || **f == FORMAT_R8G8B8A8_UNORM)
        .unwrap_or_else(|| {
            panic!(
                "this surface offers no eight-bit UNORM format ({offered:?}), so the read-back \
                 could not assert an exact colour"
            )
        });

    // ------------------------------------------------------------------ the device and its queue
    let logical = f.a_device(entry_point, instance, physical, family);
    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [logical, u64::from(family), 0, queue_at]).expect("queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);
    assert_ne!(queue, 0);

    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");
    let name = |n: &str| f.resolve_device(get_proc, logical, n);
    let create_swapchain = name("vkCreateSwapchainKHR");
    let get_images = name("vkGetSwapchainImagesKHR");
    let destroy_swapchain = name("vkDestroySwapchainKHR");
    let create_view = name("vkCreateImageView");
    let destroy_view = name("vkDestroyImageView");
    let create_semaphore = name("vkCreateSemaphore");
    let destroy_semaphore = name("vkDestroySemaphore");
    let create_fence = name("vkCreateFence");
    let destroy_fence = name("vkDestroyFence");
    let wait_fences = name("vkWaitForFences");
    let create_pool = name("vkCreateCommandPool");
    let destroy_pool = name("vkDestroyCommandPool");
    let allocate = name("vkAllocateCommandBuffers");
    let begin = name("vkBeginCommandBuffer");
    let end = name("vkEndCommandBuffer");
    let barrier = name("vkCmdPipelineBarrier");
    let clear = name("vkCmdClearColorImage");
    let acquire = name("vkAcquireNextImageKHR");
    let submit = name("vkQueueSubmit");
    let present = name("vkQueuePresentKHR");
    let queue_wait_idle = name("vkQueueWaitIdle");
    let device_wait_idle = name("vkDeviceWaitIdle");
    for thunk in [create_swapchain, acquire, submit, present] {
        assert!(
            f.boundary.symbol_at(thunk as GuestAddr).is_some(),
            "what the guest got is a thunk in this boundary, never the driver's pointer"
        );
    }

    // -------------------------------------------------------------------------- the swapchain
    let info = f.swapchain_info(
        surface,
        min_images,
        format,
        extent.0,
        extent.1,
        SWAPCHAIN_USAGE,
        current_transform,
        0,
    );
    let out = f.alloc(8);
    let result = f.call(create_swapchain, [logical, info, 0, out]).expect("the call completes");
    assert_eq!(result as i32, VK_SUCCESS, "the driver answered VkResult {}", result as i32);
    let swapchain = f.guest.read_u64(out as GuestAddr);
    assert_ne!(swapchain, 0);

    assert_eq!(
        f.call(get_images, [logical, swapchain, count_at, 0]).expect("image count") as i32,
        VK_SUCCESS
    );
    let image_count = f.read_u32(count_at) as usize;
    assert!(image_count >= 2, "a presentable swapchain has at least two images");
    let images_at = f.alloc(image_count * 8);
    assert_eq!(
        f.call(get_images, [logical, swapchain, count_at, images_at]).expect("images") as i32,
        VK_SUCCESS
    );
    let images: Vec<u64> =
        (0..image_count).map(|i| f.guest.read_u64(images_at as GuestAddr + i * 8)).collect();

    // One view per image, as every renderer does. Nothing in this frame samples one; the call is
    // here because it is the call that sits between the images and the acquire in every engine.
    let views: Vec<u64> = images
        .iter()
        .map(|image| {
            let view_info = f.image_view_info(*image, format);
            let view_out = f.alloc(8);
            assert_eq!(
                f.call(create_view, [logical, view_info, 0, view_out]).expect("view") as i32,
                VK_SUCCESS
            );
            f.guest.read_u64(view_out as GuestAddr)
        })
        .collect();

    // -------------------------------------------------------------- the frame's own objects
    let sem_info = f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    let acquired_out = f.alloc(8);
    assert_eq!(
        f.call(create_semaphore, [logical, sem_info, 0, acquired_out]).expect("semaphore") as i32,
        VK_SUCCESS
    );
    let image_available = f.guest.read_u64(acquired_out as GuestAddr);
    let rendered_out = f.alloc(8);
    assert_eq!(
        f.call(create_semaphore, [logical, sem_info, 0, rendered_out]).expect("semaphore") as i32,
        VK_SUCCESS
    );
    let render_finished = f.guest.read_u64(rendered_out as GuestAddr);

    let fence_info = f.flags_only_info(STYPE_FENCE_CREATE_INFO, 0);
    let fence_out = f.alloc(8);
    assert_eq!(
        f.call(create_fence, [logical, fence_info, 0, fence_out]).expect("fence") as i32,
        VK_SUCCESS
    );
    let fence = f.guest.read_u64(fence_out as GuestAddr);

    let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, family);
    let pool_out = f.alloc(8);
    assert_eq!(
        f.call(create_pool, [logical, pool_info, 0, pool_out]).expect("pool") as i32,
        VK_SUCCESS
    );
    let pool = f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = f.command_buffer_allocate_info(pool, 1);
    let buffers_at = f.alloc(8);
    assert_eq!(
        f.call(allocate, [logical, allocate_info, buffers_at, 0]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let command = f.guest.read_u64(buffers_at as GuestAddr);
    assert_ne!(command, 0);

    // ------------------------------------------------------------------------------- the frame
    let index_at = f.alloc(8);
    let acquired = f
        .call_n(acquire, &[logical, swapchain, u64::MAX, image_available, 0, index_at])
        .expect("the acquire completes");
    assert_eq!(
        acquired as i32, VK_SUCCESS,
        "the first acquire on a fresh swapchain over an unchanged window must succeed; the \
         driver answered VkResult {}",
        acquired as i32
    );
    let image_index = f.read_u32(index_at);
    assert!((image_index as usize) < image_count);
    let image = images[image_index as usize];

    // **The image the guest is about to clear is the one the driver acquired.** Nothing in either
    // call establishes that on its own -- acquire answers an index and the guest looks up a handle
    // -- so it is asserted through the host, which is the only participant that knows both.
    let host_images: Vec<_> = f.vulkan().image_handles();
    let host_image = host_images
        .iter()
        .find(|(handle, _)| *handle as u64 == image)
        .map(|(_, token)| *token)
        .expect("the handle the guest holds is one this layer issued");
    let (owning_swapchain, index_in_swapchain) =
        host.image_location(host_image).expect("the host knows where this image lives");
    assert_eq!(
        index_in_swapchain, image_index,
        "the VkImage the guest looked up is the one vkAcquireNextImageKHR named"
    );
    let host_swapchain = f
        .vulkan()
        .swapchain_handles()
        .iter()
        .find(|(handle, _)| *handle as u64 == swapchain)
        .map(|(_, token)| *token)
        .expect("the swapchain handle is one this layer issued");
    assert_eq!(owning_swapchain, host_swapchain, "and it belongs to this guest's swapchain");

    let begin_info = f.begin_info(ONE_TIME_SUBMIT, 0);
    assert_eq!(
        f.call(begin, [command, begin_info, 0, 0]).expect("begin") as i32,
        VK_SUCCESS
    );

    let to_transfer = f.bytes(&f.image_barrier(
        STYPE_IMAGE_MEMORY_BARRIER,
        0,
        ACCESS_TRANSFER_WRITE,
        LAYOUT_UNDEFINED,
        LAYOUT_TRANSFER_DST,
        image,
    ));
    f.call_n(
        barrier,
        &[
            command,
            u64::from(STAGE_TOP_OF_PIPE),
            u64::from(STAGE_TRANSFER),
            0,
            0,
            0,
            0,
            0,
            1,
            to_transfer,
        ],
    )
    .expect("the first barrier records");

    let colour_at = f.bytes(&CLEAR_COLOUR.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>());
    let range_at = f.bytes(&f.whole_colour_range());
    f.call_n(clear, &[command, image, u64::from(LAYOUT_TRANSFER_DST), colour_at, 1, range_at])
        .expect("the clear records");

    let to_present = f.bytes(&f.image_barrier(
        STYPE_IMAGE_MEMORY_BARRIER,
        ACCESS_TRANSFER_WRITE,
        ACCESS_MEMORY_READ,
        LAYOUT_TRANSFER_DST,
        LAYOUT_PRESENT_SRC,
        image,
    ));
    f.call_n(
        barrier,
        &[
            command,
            u64::from(STAGE_TRANSFER),
            u64::from(STAGE_BOTTOM_OF_PIPE),
            0,
            0,
            0,
            0,
            0,
            1,
            to_present,
        ],
    )
    .expect("the second barrier records");

    assert_eq!(f.call(end, [command, 0, 0, 0]).expect("end") as i32, VK_SUCCESS);

    let submit_info = f.submit_info(image_available, STAGE_TRANSFER, command, render_finished);
    assert_eq!(
        f.call(submit, [queue, 1, submit_info, fence]).expect("submit") as i32,
        VK_SUCCESS
    );
    let fences_at = f.u64_array(&[fence]);
    assert_eq!(
        f.call_n(wait_fences, &[logical, 1, fences_at, 1, u64::MAX]).expect("wait") as i32,
        VK_SUCCESS,
        "the fence the submission signals must signal"
    );

    let index_array = f.u32_array(&[image_index]);
    let results_at = f.poisoned(8, 0x33);
    let present_info = f.present_info(render_finished, swapchain, index_array, results_at);
    let presented = f.call(present, [queue, present_info, 0, 0]).expect("the present completes");
    assert_eq!(
        presented as i32, VK_SUCCESS,
        "the window has not been touched since the swapchain was made, so the present must \
         succeed outright; the driver answered VkResult {}",
        presented as i32
    );
    assert_eq!(f.read_u32(results_at) as i32, VK_SUCCESS, "and pResults says the same");

    assert_eq!(f.call(queue_wait_idle, [queue, 0, 0, 0]).expect("queue idle") as i32, VK_SUCCESS);
    assert_eq!(
        f.call(device_wait_idle, [logical, 0, 0, 0]).expect("device idle") as i32,
        VK_SUCCESS
    );

    // ------------------------------------------------------- THE EVIDENCE: the presented pixels
    let presented_image = host
        .read_presented_image(host_swapchain, image_index)
        .expect("the presented image must be readable: the swapchain has TRANSFER_SRC usage");
    assert_eq!(presented_image.width, extent.0);
    assert_eq!(presented_image.height, extent.1);
    assert_eq!(
        presented_image.rgba.len(),
        extent.0 as usize * extent.1 as usize * 4,
        "the whole image came back"
    );

    let centre = presented_image.centre().expect("a non-empty image has a centre");
    assert_eq!(
        centre, CLEAR_BYTES,
        "the pixel at the centre of the frame that was presented must be the colour the guest \
         cleared to. Expected {CLEAR_BYTES:?} (R, G, B, A) from a clear of {CLEAR_COLOUR:?} into \
         a UNORM swapchain, and read back {centre:?}. This is the assertion the whole stage \
         exists for: every VkResult above could be zero with nothing on the screen"
    );
    // The corners too, because a clear of one region and a clear of the whole image are different
    // things and only the second is what `VK_IMAGE_ASPECT_COLOR_BIT` over the whole range asked
    // for.
    for (x, y) in [(0, 0), (extent.0 - 1, 0), (0, extent.1 - 1), (extent.0 - 1, extent.1 - 1)] {
        assert_eq!(
            presented_image.pixel(x, y),
            Some(CLEAR_BYTES),
            "the clear covered the whole image, so ({x}, {y}) is the same colour as the centre"
        );
    }
    // And it is not a uniform buffer of zeros or of 0xFF that would pass a weaker assertion.
    assert!(CLEAR_BYTES.iter().any(|b| *b != 0 && *b != 0xFF));

    // ------------------------------------------------------------------------------- teardown
    for view in &views {
        f.call(destroy_view, [logical, *view, 0, 0]).expect("destroy view");
    }
    f.call(destroy_semaphore, [logical, image_available, 0, 0]).expect("destroy semaphore");
    f.call(destroy_semaphore, [logical, render_finished, 0, 0]).expect("destroy semaphore");
    f.call(destroy_fence, [logical, fence, 0, 0]).expect("destroy fence");
    f.call(destroy_pool, [logical, pool, 0, 0]).expect("destroy pool");
    f.call(destroy_swapchain, [logical, swapchain, 0, 0]).expect("destroy swapchain");
    assert!(f.vulkan().swapchain_handles().is_empty());
    assert!(f.vulkan().image_handles().is_empty());
    assert!(f.vulkan().command_buffer_handles().is_empty());
    let left = host.stage_four_objects();
    assert_eq!(left.swapchains, 0, "the host let the window go: {left:?}");
    assert_eq!(left.command_buffers, 0);
    assert_eq!(left.image_views, 0);

    eprintln!("\n=== stage 4 live evidence ===");
    eprintln!("host: {host:?}");
    eprintln!("the window's client area is {}x{}", client.width, client.height);
    eprintln!("chosen: \"{device_name}\", queue family {family}");
    eprintln!("    surface: currentExtent {}x{}, minImageCount {min_images}", extent.0, extent.1);
    eprintln!("    {format_count} format(s); chose VkFormat {format} (a UNORM, for an exact assertion)");
    eprintln!("    supportedUsageFlags {supported_usage:#x}, asked for {SWAPCHAIN_USAGE:#x}");
    eprintln!("vkCreateSwapchainKHR   -> VkResult 0; the guest's VkSwapchainKHR is {swapchain:#x}");
    eprintln!("vkGetSwapchainImagesKHR -> {image_count} image(s), first {:#x}", images[0]);
    eprintln!("vkAcquireNextImageKHR  -> VkResult 0, imageIndex {image_index}");
    eprintln!("vkQueueSubmit          -> VkResult 0");
    eprintln!("vkWaitForFences        -> VkResult 0");
    eprintln!("vkQueuePresentKHR      -> VkResult {}", presented as i32);
    eprintln!(
        "read back {}x{} from the presented image (VkFormat {}):",
        presented_image.width, presented_image.height, presented_image.format
    );
    eprintln!("    cleared to {CLEAR_COLOUR:?}, which a UNORM stores as {CLEAR_BYTES:?}");
    eprintln!("    centre pixel  = {centre:?}   <-- R, G, B, A");
    eprintln!(
        "    corner pixels = {:?}, {:?}",
        presented_image.pixel(0, 0).expect("a pixel"),
        presented_image.pixel(extent.0 - 1, extent.1 - 1).expect("a pixel")
    );
    eprintln!("\n{}", f.vulkan().report());
}

/// **A resized window makes the driver say so, and the code reaches the guest unchanged.**
///
/// The live half of the resize story; the deterministic half is
/// [`the_acquire_codes_reach_the_guest_verbatim_and_only_two_write_an_index`]. What this adds is
/// that the codes are real on **this** machine rather than only representable: the window is
/// resized through `omni_platform`, the surface's `currentExtent` is read back through the guest
/// to confirm the driver noticed, and then whole frames are presented through the stale swapchain
/// until the driver says so.
///
/// # Why it takes a frame loop rather than one acquire
///
/// Measured here, and it is the reason this test is shaped the way it is: on this host's NVIDIA
/// driver the **first `vkAcquireNextImageKHR` after a resize still answers `VK_SUCCESS`**. The
/// swapchain's images are still valid and still acquirable; what has changed is the surface, and
/// the driver reports that at `vkQueuePresentKHR` — or at a later acquire, once the presentation
/// engine has cycled through the images it already had. Both are conforming: the specification
/// says an implementation *may* return `VK_SUBOPTIMAL_KHR` or `VK_ERROR_OUT_OF_DATE_KHR`, not
/// when it must.
///
/// So this drives real frames, exactly as an engine would, and asserts that within a small number
/// of them one of the two codes arrives. A test that asserted on a single acquire would have
/// failed on a conforming driver — which is how this one first failed.
///
/// **Nothing recreates the swapchain**, which is the point: the engine owns it, and a layer that
/// rebuilt one here would hand the guest a swapchain its image views were not made over. The
/// assertion at the end is that the guest still holds exactly the one it made.
#[test]
#[ignore = "opens a window and the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_resized_window_reaches_the_guest_as_the_drivers_own_out_of_date_code() {
    require_gate();
    let _serial = serialized();

    let mut window = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
        "Omnidroid — Vulkan stage 4: resize",
        1024,
        576,
    ))
    .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    let _ = window.poll_events().count();
    let source = HostWindowSource::watching(&window).expect("a source watching the window");

    let host = omni_gfx::GfxVulkanHost::load().expect("this machine must have a Vulkan loader");
    let f = fixture("live-resize", Some(host.clone()));
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let surface = f.a_surface(entry_point, instance);

    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    f.call(enumerate, [instance, count_at, 0, 0]).expect("count");
    let devices_at = f.alloc(8);
    f.call(enumerate, [instance, count_at, devices_at, 0]).expect("array");
    let physical = f.guest.read_u64(devices_at as GuestAddr);

    let capabilities =
        f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceCapabilitiesKHR");
    let formats = f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceFormatsKHR");
    let capabilities_at = f.alloc(SURFACE_CAPABILITIES_BYTES);
    let read_u32 =
        |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
    f.call(capabilities, [physical, surface, capabilities_at, 0]).expect("capabilities");
    let caps = f.read_bytes(capabilities_at, SURFACE_CAPABILITIES_BYTES);
    let before = (read_u32(&caps, 8), read_u32(&caps, 12));
    let min_images = read_u32(&caps, 0);
    let transform = read_u32(&caps, 40);

    let scratch = f.alloc(SCRATCH_BYTES);
    f.call(formats, [physical, surface, count_at, 0]).expect("format count");
    let format_count = f.read_u32(count_at) as usize;
    f.guest.write_u32(count_at as GuestAddr, format_count as u32);
    f.call(formats, [physical, surface, count_at, scratch]).expect("formats");
    let format_bytes = f.read_bytes(scratch, format_count * SURFACE_FORMAT_BYTES);
    let format = read_u32(&format_bytes, 0);

    let logical = f.a_device(entry_point, instance, physical, 0);
    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [logical, 0, 0, queue_at]).expect("queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);

    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");
    let name = |n: &str| f.resolve_device(get_proc, logical, n);
    let create_swapchain = name("vkCreateSwapchainKHR");
    let get_images = name("vkGetSwapchainImagesKHR");
    let destroy_swapchain = name("vkDestroySwapchainKHR");
    let create_semaphore = name("vkCreateSemaphore");
    let destroy_semaphore = name("vkDestroySemaphore");
    let create_fence = name("vkCreateFence");
    let destroy_fence = name("vkDestroyFence");
    let wait_fences = name("vkWaitForFences");
    let reset_fences = name("vkResetFences");
    let create_pool = name("vkCreateCommandPool");
    let destroy_pool = name("vkDestroyCommandPool");
    let allocate = name("vkAllocateCommandBuffers");
    let begin = name("vkBeginCommandBuffer");
    let end = name("vkEndCommandBuffer");
    let barrier = name("vkCmdPipelineBarrier");
    let clear = name("vkCmdClearColorImage");
    let acquire = name("vkAcquireNextImageKHR");
    let submit = name("vkQueueSubmit");
    let present = name("vkQueuePresentKHR");

    let info = f.swapchain_info(
        surface,
        min_images,
        format,
        before.0,
        before.1,
        0x10 | 0x2, // COLOR_ATTACHMENT | TRANSFER_DST -- no read-back in this test
        transform,
        0,
    );
    let out = f.alloc(8);
    assert_eq!(
        f.call(create_swapchain, [logical, info, 0, out]).expect("create") as i32,
        VK_SUCCESS
    );
    let swapchain = f.guest.read_u64(out as GuestAddr);

    f.call(get_images, [logical, swapchain, count_at, 0]).expect("image count");
    let image_count = f.read_u32(count_at) as usize;
    let images_at = f.alloc(image_count * 8);
    f.call(get_images, [logical, swapchain, count_at, images_at]).expect("images");
    let images: Vec<u64> =
        (0..image_count).map(|i| f.guest.read_u64(images_at as GuestAddr + i * 8)).collect();

    let sem_info = f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    let acquired_out = f.alloc(8);
    f.call(create_semaphore, [logical, sem_info, 0, acquired_out]).expect("semaphore");
    let image_available = f.guest.read_u64(acquired_out as GuestAddr);
    let rendered_out = f.alloc(8);
    f.call(create_semaphore, [logical, sem_info, 0, rendered_out]).expect("semaphore");
    let render_finished = f.guest.read_u64(rendered_out as GuestAddr);
    let fence_info = f.flags_only_info(STYPE_FENCE_CREATE_INFO, 0);
    let fence_out = f.alloc(8);
    f.call(create_fence, [logical, fence_info, 0, fence_out]).expect("fence");
    let fence = f.guest.read_u64(fence_out as GuestAddr);
    let fences_at = f.u64_array(&[fence]);

    let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
    let pool_out = f.alloc(8);
    f.call(create_pool, [logical, pool_info, 0, pool_out]).expect("pool");
    let pool = f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = f.command_buffer_allocate_info(pool, 1);
    let buffers_at = f.alloc(8);
    f.call(allocate, [logical, allocate_info, buffers_at, 0]).expect("allocate");
    let command = f.guest.read_u64(buffers_at as GuestAddr);

    let index_at = f.alloc(8);
    let colour_at =
        f.bytes(&CLEAR_COLOUR.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>());
    let range_at = f.bytes(&f.whole_colour_range());
    let begin_info = f.begin_info(ONE_TIME_SUBMIT, 0);
    let submit_info = f.submit_info(image_available, STAGE_TRANSFER, command, render_finished);

    // One whole frame through the swapchain the guest holds, answering `(acquire, present)`.
    // Written as a closure rather than a helper on `Fixture` because every address it needs is a
    // local of this test, and a helper would have had to take fourteen of them.
    let frame = || -> (i32, Option<i32>) {
        let acquired = f
            .call_n(acquire, &[logical, swapchain, u64::MAX, image_available, 0, index_at])
            .expect("the acquire completes whatever the driver said")
            as i32;
        if acquired != VK_SUCCESS && acquired != VK_SUBOPTIMAL_KHR {
            return (acquired, None);
        }
        let image_index = f.read_u32(index_at);
        let image = images[image_index as usize];

        assert_eq!(
            f.call_n(reset_fences, &[logical, 1, fences_at, 0]).expect("reset") as i32,
            VK_SUCCESS
        );
        assert_eq!(f.call(begin, [command, begin_info, 0, 0]).expect("begin") as i32, VK_SUCCESS);
        let to_transfer = f.bytes(&f.image_barrier(
            STYPE_IMAGE_MEMORY_BARRIER,
            0,
            ACCESS_TRANSFER_WRITE,
            LAYOUT_UNDEFINED,
            LAYOUT_TRANSFER_DST,
            image,
        ));
        f.call_n(
            barrier,
            &[
                command,
                u64::from(STAGE_TOP_OF_PIPE),
                u64::from(STAGE_TRANSFER),
                0,
                0,
                0,
                0,
                0,
                1,
                to_transfer,
            ],
        )
        .expect("the first barrier records");
        f.call_n(clear, &[command, image, u64::from(LAYOUT_TRANSFER_DST), colour_at, 1, range_at])
            .expect("the clear records");
        let to_present = f.bytes(&f.image_barrier(
            STYPE_IMAGE_MEMORY_BARRIER,
            ACCESS_TRANSFER_WRITE,
            ACCESS_MEMORY_READ,
            LAYOUT_TRANSFER_DST,
            LAYOUT_PRESENT_SRC,
            image,
        ));
        f.call_n(
            barrier,
            &[
                command,
                u64::from(STAGE_TRANSFER),
                u64::from(STAGE_BOTTOM_OF_PIPE),
                0,
                0,
                0,
                0,
                0,
                1,
                to_present,
            ],
        )
        .expect("the second barrier records");
        assert_eq!(f.call(end, [command, 0, 0, 0]).expect("end") as i32, VK_SUCCESS);
        assert_eq!(
            f.call(submit, [queue, 1, submit_info, fence]).expect("submit") as i32,
            VK_SUCCESS
        );
        assert_eq!(
            f.call_n(wait_fences, &[logical, 1, fences_at, 1, u64::MAX]).expect("wait") as i32,
            VK_SUCCESS
        );
        let index_array = f.u32_array(&[image_index]);
        let present_info = f.present_info(render_finished, swapchain, index_array, 0);
        let presented =
            f.call(present, [queue, present_info, 0, 0]).expect("the present completes") as i32;
        (acquired, Some(presented))
    };

    // A frame before the resize, so that "the codes change" is a comparison rather than a claim.
    let (acquired, presented) = frame();
    assert_eq!(acquired, VK_SUCCESS, "an untouched window acquires cleanly");
    assert_eq!(presented, Some(VK_SUCCESS), "and presents cleanly");

    // **Resize the window by a large amount**, and let the message loop see it.
    let (wide, tall) = (before.0 * 3 / 4, before.1 * 3 / 4);
    window
        .set_client_size(wide, tall)
        .unwrap_or_else(|err| panic!("could not resize the window: {err}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut after = before;
    while std::time::Instant::now() < deadline {
        let _ = window.poll_events().count();
        f.call(capabilities, [physical, surface, capabilities_at, 0]).expect("capabilities");
        let caps = f.read_bytes(capabilities_at, SURFACE_CAPABILITIES_BYTES);
        after = (read_u32(&caps, 8), read_u32(&caps, 12));
        if after != before {
            break;
        }
    }
    assert_ne!(
        after, before,
        "the window was resized from {before:?} to about ({wide}, {tall}) and the surface still \
         reports {before:?}, so nothing downstream could have noticed either"
    );

    // Present through the now-stale swapchain until the driver says something about it. Bounded,
    // because an unbounded loop on a driver that never reported it would hang rather than fail.
    const FRAMES: usize = 8;
    let mut seen: Option<(usize, &'static str, i32)> = None;
    let mut codes = Vec::new();
    for attempt in 0..FRAMES {
        let (acquired, presented) = frame();
        codes.push((acquired, presented));
        if acquired == VK_SUBOPTIMAL_KHR || acquired == VK_ERROR_OUT_OF_DATE_KHR {
            seen = Some((attempt, "vkAcquireNextImageKHR", acquired));
            break;
        }
        match presented {
            Some(code) if code == VK_SUBOPTIMAL_KHR || code == VK_ERROR_OUT_OF_DATE_KHR => {
                seen = Some((attempt, "vkQueuePresentKHR", code));
                break;
            }
            _ => {}
        }
    }
    let (attempt, call, code) = seen.unwrap_or_else(|| {
        panic!(
            "the surface went from {before:?} to {after:?} and {FRAMES} whole frames were \
             presented through the stale swapchain without this driver answering \
             VK_SUBOPTIMAL_KHR ({VK_SUBOPTIMAL_KHR}) or VK_ERROR_OUT_OF_DATE_KHR \
             ({VK_ERROR_OUT_OF_DATE_KHR}) from either call. The codes were {codes:?}. That is a \
             legal thing for a driver to do and it means this machine cannot demonstrate the \
             forwarding live -- the deterministic half of the claim is \
             `the_acquire_codes_reach_the_guest_verbatim_and_only_two_write_an_index`"
        )
    });
    assert!(
        code == VK_SUBOPTIMAL_KHR || code == VK_ERROR_OUT_OF_DATE_KHR,
        "the code is one of the two the specification defines for this: {code}"
    );

    // The layer **recorded** it rather than swallowing it, and it did not recreate the swapchain:
    // the guest still holds exactly the one it made.
    assert!(f.vulkan().driver_failures().iter().any(|(logged, r)| logged == call && *r == code));
    assert_eq!(
        f.vulkan().swapchain_handles().len(),
        1,
        "nothing recreated the swapchain on the guest's behalf -- the engine owns it"
    );
    assert_eq!(host.stage_four_objects().swapchains, 1, "and the host holds one too");

    eprintln!("\n=== stage 4 live resize evidence ===");
    eprintln!("the surface went from {before:?} to {after:?} after set_client_size({wide}, {tall})");
    eprintln!("frame {attempt} of {FRAMES}: {call} -> VkResult {code} ({})", match code {
        VK_ERROR_OUT_OF_DATE_KHR => "VK_ERROR_OUT_OF_DATE_KHR",
        VK_SUBOPTIMAL_KHR => "VK_SUBOPTIMAL_KHR",
        _ => "neither, which the assertion above has already refused",
    });
    eprintln!("the codes each frame answered, (acquire, present): {codes:?}");
    eprintln!("and the guest still holds its own swapchain: nothing was recreated for it");

    f.call(destroy_semaphore, [logical, image_available, 0, 0]).expect("destroy semaphore");
    f.call(destroy_semaphore, [logical, render_finished, 0, 0]).expect("destroy semaphore");
    f.call(destroy_fence, [logical, fence, 0, 0]).expect("destroy fence");
    f.call(destroy_pool, [logical, pool, 0, 0]).expect("destroy pool");
    f.call(destroy_swapchain, [logical, swapchain, 0, 0]).expect("destroy swapchain");
}

/// **`omni_gfx::Renderer` and the guest cannot both have a swapchain on one window, and the
/// refusal says which of them has it.**
///
/// The ownership test. It is live because the conflict only exists when both stacks are real: a
/// `Renderer` takes the claim when it is constructed, and the guest's `vkCreateSwapchainKHR` then
/// refuses **naming `omni_gfx::Renderer`** rather than reaching a driver that this host has no
/// validation layer to hear from.
#[test]
#[ignore = "opens a window and the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn the_guest_cannot_take_a_window_omni_gfxs_renderer_already_owns() {
    require_gate();
    let _serial = serialized();

    let mut window = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
        "Omnidroid — Vulkan stage 4: who owns the window",
        800,
        450,
    ))
    .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    let _ = window.poll_events().count();
    let source = HostWindowSource::watching(&window).expect("a source watching the window");
    let size = window.client_size().expect("the window has a client area");

    // The host-side renderer takes the window first.
    let renderer = omni_gfx::Renderer::new(
        window.raw(),
        size,
        omni_gfx::vulkan::RendererConfig::default(),
    )
    .unwrap_or_else(|err| panic!("could not create the host renderer: {err}"));

    let host = omni_gfx::GfxVulkanHost::load().expect("this machine must have a Vulkan loader");
    let f = fixture("live-conflict", Some(host.clone()));
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    // **The surface is fine.** Two `VkSurfaceKHR` over one window is legal, and this is the line
    // that says so: the guest gets a real surface over the same `HWND` the renderer is using.
    let surface = f.a_surface(entry_point, instance);
    assert_ne!(surface, 0, "two surfaces over one window is legal Vulkan");

    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    f.call(enumerate, [instance, count_at, 0, 0]).expect("count");
    let devices_at = f.alloc(8);
    f.call(enumerate, [instance, count_at, devices_at, 0]).expect("array");
    let physical = f.guest.read_u64(devices_at as GuestAddr);
    let logical = f.a_device(entry_point, instance, physical, 0);
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");
    let create_swapchain = f.resolve_device(get_proc, logical, "vkCreateSwapchainKHR");

    let info = f.swapchain_info(surface, 2, FORMAT_B8G8R8A8_UNORM, size.0, size.1, 0x10, 1, 0);
    let out = f.alloc(8);
    let text = f.refusal(create_swapchain, &[logical, info, 0, out]).to_string();
    assert!(
        text.contains("omni_gfx::Renderer"),
        "the refusal must name the other owner rather than saying \"in use\": {text}"
    );
    assert!(text.contains("at most one swapchain at a time"), "{text}");
    assert!(text.contains("oldSwapchain"), "it says how a guest replaces its own: {text}");
    assert!(f.vulkan().swapchain_handles().is_empty(), "nothing was issued");

    eprintln!("\n=== stage 4 window-ownership evidence ===");
    eprintln!("omni_gfx::Renderer holds the window: {renderer:?}");
    eprintln!("the guest's own VkSurfaceKHR over the same HWND: {surface:#x} (legal)");
    eprintln!("the guest's vkCreateSwapchainKHR was refused by name:\n  {text}");

    // And once the renderer lets the window go, the guest can have it -- which is the case an
    // embedding actually wants and which a flag rather than a claim would have got wrong.
    drop(renderer);
    let result = f.call(create_swapchain, [logical, info, 0, out]).expect("the call completes");
    assert_eq!(
        result as i32, VK_SUCCESS,
        "with the renderer gone the window is free and the guest's swapchain is created; the \
         driver answered VkResult {}",
        result as i32
    );
    let swapchain = f.guest.read_u64(out as GuestAddr);
    assert_ne!(swapchain, 0);
    eprintln!("after dropping the renderer the guest's swapchain was created: {swapchain:#x}");

    // **A second swapchain on the same window, with no `oldSwapchain`: refused, naming the guest
    // itself.** The rule is not "the renderer wins"; it is "one swapchain per window", and the
    // guest is as bound by it as anything else.
    let second = f.swapchain_info(surface, 2, FORMAT_B8G8R8A8_UNORM, size.0, size.1, 0x10, 1, 0);
    let text = f.refusal(create_swapchain, &[logical, second, 0, out]).to_string();
    assert!(
        text.contains("the guest's vkCreateSwapchainKHR"),
        "the owner named is the guest's own first swapchain: {text}"
    );

    // **And with `oldSwapchain`: allowed, because the claim is transferred rather than taken.**
    // This is how a renderer follows a resize, and it is the one case in which two swapchains
    // legitimately exist over one window at the same instant -- the outgoing one is *retired* by
    // the call rather than destroyed, which is why the guest must still destroy it.
    let replacing =
        f.swapchain_info(surface, 2, FORMAT_B8G8R8A8_UNORM, size.0, size.1, 0x10, 1, swapchain);
    let out2 = f.alloc(8);
    let result =
        f.call(create_swapchain, [logical, replacing, 0, out2]).expect("the call completes");
    assert_eq!(
        result as i32, VK_SUCCESS,
        "recreating over the outgoing swapchain must work -- it is how every renderer follows a \
         resize; the driver answered VkResult {}",
        result as i32
    );
    let replacement = f.guest.read_u64(out2 as GuestAddr);
    assert_ne!(replacement, swapchain, "a new handle, and the old one is retired rather than gone");
    assert_eq!(f.vulkan().swapchain_handles().len(), 2, "both are live until the guest destroys");
    eprintln!("recreated with oldSwapchain = {swapchain:#x} -> {replacement:#x} (claim transferred)");

    // The retired one cannot be used as `oldSwapchain` again: chaining two recreations off one
    // outgoing swapchain would leave two live swapchains believing they own the window.
    let again =
        f.swapchain_info(surface, 2, FORMAT_B8G8R8A8_UNORM, size.0, size.1, 0x10, 1, swapchain);
    let out3 = f.alloc(8);
    let text = f.refusal(create_swapchain, &[logical, again, 0, out3]).to_string();
    assert!(text.contains("already been retired"), "{text}");
    eprintln!("and a second recreation off the same retired swapchain was refused by name");

    let destroy = f.resolve_device(get_proc, logical, "vkDestroySwapchainKHR");
    f.call(destroy, [logical, replacement, 0, 0]).expect("destroy the replacement");
    f.call(destroy, [logical, swapchain, 0, 0]).expect("destroy the retired one");
    assert!(f.vulkan().swapchain_handles().is_empty());
    assert_eq!(host.stage_four_objects().swapchains, 0, "and the window is free again");
}
