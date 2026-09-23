//! **A frame presented through the guest-facing host, and its pixels read back.**
//!
//! `omni-android/tests/vulkan_present.rs` proves this from assembled ARM64 guest code, but it is
//! built only for an x86-64 host (its harness runs the guest through the translator there), so on
//! an ARM64 macOS host it compiles to nothing. This file drives the same [`GfxVulkanHost`] through
//! the same [`VulkanHost`] calls the guest's shim makes -- instance, platform surface, device,
//! swapchain, acquire, a barrier and a clear, submit, present -- from Rust, against the real
//! driver and a real window, and then **reads the presented image back** with
//! [`GfxVulkanHost::read_presented_image`] and asserts its colour.
//!
//! The requests are shaped the way a guest that knows nothing of the host shapes them: the
//! instance asks for `VK_KHR_surface` and the host's platform surface extension and **not** for
//! portability enumeration, the device for `VK_KHR_swapchain` and **not** for the portability
//! subset. On a portability implementation (MoltenVK) that is exactly the case the host's retry and
//! subset handling exist for; on a native driver it is the ordinary path.
//!
//! # What the read-back can and cannot see (VERIFICATION entry 19)
//!
//! It reads the swapchain image after `vkQueuePresentKHR`: exactly the pixels handed to the
//! presentation engine. It is shown able to see **something known** before it is trusted -- two
//! frames cleared to two different colours must read back as those two colours, so a read-back
//! that returned a default, a stale image or the first frame again would fail. It is not a
//! photograph of the display: the step from "presented" to "composited on the monitor" is the
//! window server's, and is not measured here.
//!
//! ```text
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-gfx --release --test host_present_live -- --ignored --test-threads=1
//! ```

use omni_android::vulkan::{
    ApplicationInfo, DeviceRequest, DriverAnswer, HostImageRef, ImageBarrier, InstanceRequest,
    PipelineBarrier, PresentRequest, QueueRequest, SubmitRequest, SwapchainRequest, VulkanHost,
};
use omni_gfx::GfxVulkanHost;
use omni_platform::window::{Window, WindowDesc};

const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

fn require_gate() {
    assert!(
        std::env::var(GATE).is_ok_and(|v| v == "1"),
        "this test was run with --ignored but {GATE} is not set to 1. It opens a window, loads the \
         Vulkan driver and presents frames; set {GATE}=1 to run it, or drop --ignored to skip it."
    );
}

fn ok<T: std::fmt::Debug>(what: &str, answer: DriverAnswer<T>) -> T {
    match answer {
        DriverAnswer::Ok(value) => value,
        DriverAnswer::Failed(code) => panic!("{what} failed with VkResult {code}"),
    }
}

// The Vulkan numbers this test writes, from `vulkan_core.h`.
const QUEUE_GRAPHICS: u32 = 0x1;
const FORMAT_B8G8R8A8_UNORM: u32 = 44;
const FORMAT_R8G8B8A8_UNORM: u32 = 37;
const USAGE_TRANSFER_SRC: u32 = 0x1;
const USAGE_TRANSFER_DST: u32 = 0x2;
const LAYOUT_UNDEFINED: u32 = 0;
const LAYOUT_TRANSFER_DST: u32 = 7;
const LAYOUT_PRESENT_SRC: u32 = 1_000_001_002;
const STAGE_TOP: u32 = 0x1;
const STAGE_TRANSFER: u32 = 0x1000;
const STAGE_BOTTOM: u32 = 0x2000;
const ACCESS_TRANSFER_WRITE: u32 = 0x1000;
const QUEUE_FAMILY_IGNORED: u32 = !0;

/// `VkImageSubresourceRange` for the one colour level and layer, as the bytes the host reads.
fn colour_range() -> Vec<u8> {
    [1u32, 0, 1, 0, 1].iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn barrier(image: HostImageRef, old: u32, new: u32, src_stage: u32, dst_stage: u32, src: u32, dst: u32) -> PipelineBarrier {
    PipelineBarrier {
        src_stage,
        dst_stage,
        dependency_flags: 0,
        memory_barriers: Vec::new(),
        image_barriers: vec![ImageBarrier {
            src_access: src,
            dst_access: dst,
            old_layout: old,
            new_layout: new,
            src_queue_family: QUEUE_FAMILY_IGNORED,
            dst_queue_family: QUEUE_FAMILY_IGNORED,
            image,
            subresource_range: colour_range(),
        }],
    }
}

/// One guest-shaped run at `api_version`: present two colours, read each back.
fn present_and_read_back(api_version: u32) {
    let mut window = Window::new(&WindowDesc::new("omnidroid: host present", 320, 240)).expect("a window");
    window.show();
    let _ = window.poll_events().count();
    let (width, height) = window.client_size().unwrap();

    let host = GfxVulkanHost::load().expect("the Vulkan loader");
    let platform = host.platform_surface_extension().expect("a platform surface extension");
    let instance = ok("vkCreateInstance", host.create_instance(&InstanceRequest {
        flags: 0,
        application: Some(ApplicationInfo {
            application_name: Some("omnidroid host_present_live".to_owned()),
            application_version: 1,
            engine_name: None,
            engine_version: 0,
            api_version,
        }),
        layers: Vec::new(),
        extensions: vec!["VK_KHR_surface".to_owned(), platform.clone()],
    }).unwrap());
    let surface = ok("vkCreateAndroidSurfaceKHR", host.create_platform_surface(instance, window.raw()).unwrap());
    println!("api {api_version:#x}: {platform}, surface made by {}", surface.host_call);
    let surface = surface.surface;

    let physical = ok("vkEnumeratePhysicalDevices", host.physical_devices(instance).unwrap())
        .into_iter()
        .next()
        .expect("a physical device");
    let families = host.queue_family_properties(physical).unwrap();
    let family = (0..families.len() as u32)
        .find(|&index| {
            let flags = u32::from_le_bytes(families[index as usize][0..4].try_into().unwrap());
            flags & QUEUE_GRAPHICS != 0
                && ok("vkGetPhysicalDeviceSurfaceSupportKHR", host.surface_support(physical, index, surface).unwrap())
        })
        .expect("a family that renders and presents");
    let device = ok("vkCreateDevice", host.create_device(physical, &DeviceRequest {
        flags: 0,
        queues: vec![QueueRequest { flags: 0, family_index: family, priorities: vec![1.0] }],
        layers: Vec::new(),
        extensions: vec!["VK_KHR_swapchain".to_owned()],
        features: None,
        chain: Vec::new(),
    }).unwrap());
    let queue = host.device_queue(device, family, 0).unwrap();

    let formats = ok("vkGetPhysicalDeviceSurfaceFormatsKHR", host.surface_formats(physical, surface).unwrap());
    let format = formats
        .iter()
        .map(|bytes| u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
        .find(|&f| f == FORMAT_B8G8R8A8_UNORM || f == FORMAT_R8G8B8A8_UNORM)
        .expect("an 8-bit UNORM surface format");
    let swapchain = ok("vkCreateSwapchainKHR", host.create_swapchain(device, &SwapchainRequest {
        flags: 0,
        surface: Some(surface),
        min_image_count: 3,
        format,
        colour_space: 0,
        width,
        height,
        array_layers: 1,
        usage: USAGE_TRANSFER_DST | USAGE_TRANSFER_SRC,
        sharing_mode: 0,
        queue_families: Vec::new(),
        pre_transform: 1,
        composite_alpha: 1,
        present_mode: 2,
        clipped: 1,
        old_swapchain: None,
    }).unwrap());
    let images = ok("vkGetSwapchainImagesKHR", host.swapchain_images(swapchain).unwrap());

    let pool = ok("vkCreateCommandPool", host.create_command_pool(device, 0x2, family).unwrap());
    let buffer = ok("vkAllocateCommandBuffers", host.allocate_command_buffers(pool, 0, 1).unwrap())[0];
    let acquired = ok("vkCreateSemaphore", host.create_semaphore(device, 0).unwrap());
    let rendered = ok("vkCreateSemaphore", host.create_semaphore(device, 0).unwrap());
    let fence = ok("vkCreateFence", host.create_fence(device, 0).unwrap());

    // Two colours, each exactly representable in 8-bit UNORM (n/255), so the read-back is exact.
    for rgba in [[51u8, 102, 204, 255], [255, 128, 0, 255]] {
        let got = host.acquire_next_image(swapchain, u64::MAX, Some(acquired), None).unwrap();
        assert!(got.result == 0 || got.result == 1_000_001_003, "vkAcquireNextImageKHR: {}", got.result);
        let index = got.image_index.expect("an acquired image");
        let image = HostImageRef::Swapchain(images[index as usize]);

        ok("vkBeginCommandBuffer", host.begin_command_buffer(buffer, 1).unwrap());
        host.cmd_pipeline_barrier(buffer, &barrier(image, LAYOUT_UNDEFINED, LAYOUT_TRANSFER_DST, STAGE_TOP, STAGE_TRANSFER, 0, ACCESS_TRANSFER_WRITE)).unwrap();
        let mut colour = [0u8; 16];
        for (channel, value) in rgba.iter().enumerate() {
            colour[channel * 4..channel * 4 + 4].copy_from_slice(&(f32::from(*value) / 255.0).to_le_bytes());
        }
        host.cmd_clear_color_image(buffer, image, LAYOUT_TRANSFER_DST, colour, &[colour_range()]).unwrap();
        host.cmd_pipeline_barrier(buffer, &barrier(image, LAYOUT_TRANSFER_DST, LAYOUT_PRESENT_SRC, STAGE_TRANSFER, STAGE_BOTTOM, ACCESS_TRANSFER_WRITE, 0)).unwrap();
        ok("vkEndCommandBuffer", host.end_command_buffer(buffer).unwrap());
        ok("vkQueueSubmit", host.queue_submit(queue, &[SubmitRequest {
            waits: vec![(acquired, STAGE_TRANSFER)],
            command_buffers: vec![buffer],
            signals: vec![rendered],
        }], Some(fence)).unwrap());
        let presented = host.queue_present(queue, &PresentRequest {
            waits: vec![rendered],
            swapchains: vec![(swapchain, index)],
            wants_per_swapchain_results: true,
        }).unwrap();
        assert!(presented.result == 0 || presented.result == 1_000_001_003, "vkQueuePresentKHR: {presented:?}");
        assert_eq!(host.wait_for_fences(device, &[fence], true, u64::MAX).unwrap(), 0);
        ok("vkResetFences", host.reset_fences(device, &[fence]).unwrap());

        let frame = host.read_presented_image(swapchain, index).expect("the presented image read back");
        assert_eq!((frame.width, frame.height), (width, height), "the image is the window's size");
        for (x, y) in [(0, 0), (width / 2, height / 2), (width - 1, height - 1)] {
            assert_eq!(frame.pixel(x, y), Some(rgba), "pixel ({x}, {y}) of a frame cleared to {rgba:?}");
        }
        println!("  presented image {index} ({}x{}, VkFormat {}): read back {rgba:?} at three points", frame.width, frame.height, frame.format);
        let _ = window.poll_events().count();
    }

    ok("vkDeviceWaitIdle", host.device_wait_idle(device).unwrap());
    host.destroy_fence(fence).unwrap();
    host.destroy_semaphore(acquired).unwrap();
    host.destroy_semaphore(rendered).unwrap();
    host.destroy_command_pool(pool).unwrap();
    host.destroy_swapchain(swapchain).unwrap();
    host.destroy_device(device).unwrap();
    host.destroy_surface(instance, surface).unwrap();
    host.destroy_instance(instance).unwrap();
    drop(window);
}

/// A 1.0 instance, as a guest that asks for nothing more makes it: the host must add
/// `VK_KHR_get_physical_device_properties2` itself to read a portability subset's features.
#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_frame_presented_through_the_host_on_a_1_0_instance_reads_back_as_cleared() {
    require_gate();
    present_and_read_back(0x0040_0000);
}

/// A 1.1 instance: the subset's features are read through the core entry point.
#[test]
#[ignore = "needs a desktop session and a GPU: OMNI_GFX_WINDOW_TESTS=1 cargo test -- --ignored"]
fn a_frame_presented_through_the_host_on_a_1_1_instance_reads_back_as_cleared() {
    require_gate();
    present_and_read_back(0x0040_1000);
}
