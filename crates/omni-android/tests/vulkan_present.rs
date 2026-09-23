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
    Acquired, BufferRequest, ComputePipelineRequest, DescriptorCopy, DescriptorSetLayoutRequest,
    DescriptorWrite, COMPUTE_PIPELINE_CREATE_INFO_BYTES,
    DescriptorPoolRequest, DeviceRequest, DriverAnswer, GraphicsPipelineRequest, HostBuffer, HostCommandBuffer,
    HostCommandPool, HostCreatedImage, HostDescriptorPool, HostDescriptorSet,
    HostDescriptorSetLayout, HostDevice, HostDeviceMemory, HostExtension, HostFence, HostImage,
    HostImageRef, HostImageView, HostInstance, HostPhysicalDevice, HostPipeline, HostPipelineCache,
    HostPipelineLayout, HostQueryPool, HostQueue, HostRenderPass, HostSampler, HostSemaphore,
    HostShaderModule, QueryPoolRequest, QUERY_POOL_CREATE_INFO_BYTES, IMAGE_COPY_BYTES,
    IMAGE_BLIT_BYTES, MAX_BARRIERS,
    DescriptorWrites, DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO_BYTES,
    DESCRIPTOR_UPDATE_TEMPLATE_ENTRY_BYTES, STYPE_DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO,
    HostSurface, HostSwapchain, ImageRequest, ImageViewRequest, InstanceRequest, MemoryAllocation,
    MemoryPlan, PipelineBarrier, PipelineLayoutRequest, PipelinesCreated, PresentRequest,
    PIPELINE_CACHE_CREATE_INFO_BYTES,
    Presented, RenderPassRequest, RewriteSite, SubmitRequest, SurfaceCreated, SwapchainRequest,
    Vulkan, VulkanHost, ANDROID_SURFACE_CREATE_INFO_BYTES,
    ATTACHMENT_DESCRIPTION_BYTES, ATTACHMENT_REFERENCE_BYTES, BUFFER_CREATE_INFO_BYTES,
    BUFFER_IMAGE_COPY_BYTES, COLOR_BLEND_ATTACHMENT_BYTES, COLOR_BLEND_STATE_BYTES,
    COMMAND_BUFFER_ALLOCATE_INFO_BYTES, COMMAND_BUFFER_BEGIN_INFO_BYTES,
    COMMAND_POOL_CREATE_INFO_BYTES, DESCRIPTOR_IMAGE_INFO_BYTES, DESCRIPTOR_POOL_CREATE_INFO_BYTES,
    DESCRIPTOR_POOL_SIZE_BYTES, DESCRIPTOR_SET_ALLOCATE_INFO_BYTES,
    DESCRIPTOR_SET_LAYOUT_BINDING_BYTES, DESCRIPTOR_SET_LAYOUT_CREATE_INFO_BYTES,
    DEVICE_CREATE_INFO_BYTES, DEVICE_QUEUE_CREATE_INFO_BYTES, DYNAMIC_STATE_CREATE_INFO_BYTES,
    FRAMEBUFFER_CREATE_INFO_BYTES, GRAPHICS_PIPELINE_CREATE_INFO_BYTES, GUEST_SURFACE_EXTENSION,
    IMAGE_CREATE_INFO_BYTES, IMAGE_MEMORY_BARRIER_BYTES, IMAGE_SUBRESOURCE_RANGE_BYTES,
    IMAGE_VIEW_CREATE_INFO_BYTES, INPUT_ASSEMBLY_STATE_BYTES, LOADER_ENTRY_POINT, LOADER_SONAMES,
    MEMORY_ALLOCATE_INFO_BYTES, MEMORY_REQUIREMENTS_BYTES, MULTISAMPLE_STATE_BYTES,
    PHYSICAL_DEVICE_FEATURES_BYTES, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES,
    PHYSICAL_DEVICE_PROPERTIES_BYTES, PIPELINE_LAYOUT_CREATE_INFO_BYTES,
    PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES, PRESENT_INFO_BYTES, QUEUE_FAMILY_PROPERTIES_BYTES,
    RASTERIZATION_STATE_BODY_BYTES, RASTERIZATION_STATE_BYTES, RECT_2D_BYTES, RENDER_PASS_BEGIN_INFO_BYTES,
    RENDER_PASS_CREATE_INFO_BYTES, HANDLE_SLOT_BYTES, SAMPLER_CREATE_INFO_BYTES,
    SEMAPHORE_CREATE_INFO_BYTES, SHADER_MODULE_CREATE_INFO_BYTES,
    STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, STYPE_SWAPCHAIN_CREATE_INFO_KHR,
    SUBMIT_INFO_BYTES, SUBPASS_DEPENDENCY_BYTES, SUBPASS_DESCRIPTION_BYTES,
    SURFACE_CAPABILITIES_BYTES, SURFACE_FORMAT_BYTES, SWAPCHAIN_CREATE_INFO_BYTES,
    VERTEX_INPUT_ATTRIBUTE_BYTES, VERTEX_INPUT_BINDING_BYTES, VERTEX_INPUT_STATE_BYTES,
    VIEWPORT_STATE_BYTES, VK_ERROR_OUT_OF_DATE_KHR, VK_INCOMPLETE, VK_SUBOPTIMAL_KHR, VK_SUCCESS,
    VK_NOT_READY, VK_TIMEOUT, VK_WHOLE_SIZE, WRITE_DESCRIPTOR_SET_BYTES,
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


// ======================================================= the SPIR-V the stage 5 live test draws
//
// **Assembled by hand, once, and embedded — because there is no shader compiler on this machine
// and adding one is exactly what this project does not do.** `omni-gfx`'s own `lib.rs` records
// that decision: no shaders, no `VkPipeline`, no SPIR-V, "and therefore no shader compiler in the
// build". The shaders this project runs are the *guest's*; these two exist only so that a test can
// prove the guest's would work, and they are checked by the thing that matters — an NVIDIA driver
// compiling them, and the pixels that come out.
//
// The GLSL they correspond to, so a reader can check the modules against something:
//
// ```glsl
// // vertex
// #version 450
// layout(location = 0) in vec2 inPos;
// layout(location = 1) in vec2 inUV;
// layout(location = 0) out vec2 outUV;
// void main() { gl_Position = vec4(inPos, 0.0, 1.0); outUV = inUV; }
//
// // fragment
// #version 450
// layout(location = 0) in vec2 inUV;
// layout(location = 0) out vec4 outColour;
// layout(set = 0, binding = 0) uniform sampler2D tex;
// void main() { outColour = texture(tex, inUV); }
// ```
//
// SPIR-V 1.0, which every Vulkan 1.0 driver accepts, and **the same bytes on both sides of the
// boundary**: the format is little-endian 32-bit words whose meaning the SPIR-V specification
// fixes with no reference to a host, so nothing translates them and `vulkan::shader` says so.

/// **An empty compute shader**, assembled by hand for the same reason as the two above:
///
/// ```glsl
/// #version 450
/// layout(local_size_x = 1, local_size_y = 1, local_size_z = 1) in;
/// void main() {}
/// ```
///
/// `OpCapability Shader`, `OpMemoryModel Logical GLSL450`, `OpEntryPoint GLCompute %4 "main"`,
/// `OpExecutionMode %4 LocalSize 1 1 1`, `%2 = OpTypeVoid`, `%3 = OpTypeFunction %2`, and
/// `%4 = OpFunction %2 None %3` with one block that returns. Checked by a driver compiling it.
const COMPUTE_SPIRV: [u32; 35] = [
    0x07230203, 0x00010000, 0x00000000, 0x00000006, 0x00000000, // header, bound 6
    0x00020011, 0x00000001, // OpCapability Shader
    0x0003000e, 0x00000000, 0x00000001, // OpMemoryModel Logical GLSL450
    0x0005000f, 0x00000005, 0x00000004, 0x6e69616d, 0x00000000, // OpEntryPoint GLCompute %4 "main"
    0x00060010, 0x00000004, 0x00000011, 0x00000001, 0x00000001, 0x00000001, // LocalSize 1 1 1
    0x00020013, 0x00000002, // %2 = OpTypeVoid
    0x00030021, 0x00000003, 0x00000002, // %3 = OpTypeFunction %2
    0x00050036, 0x00000002, 0x00000004, 0x00000000, 0x00000003, // %4 = OpFunction %2 None %3
    0x000200f8, 0x00000005, // %5 = OpLabel
    0x000100fd, // OpReturn
    0x00010038, // OpFunctionEnd
];

const TRIANGLE_VERT_SPIRV: [u32; 151] = [
    0x07230203, 0x00010000, 0x00000000, 0x0000001b, 0x00000000, 0x00020011,
    0x00000001, 0x0003000e, 0x00000000, 0x00000001, 0x0009000f, 0x00000000,
    0x00000013, 0x6e69616d, 0x00000000, 0x00000008, 0x0000000c, 0x0000000d,
    0x0000000f, 0x00050048, 0x00000006, 0x00000000, 0x0000000b, 0x00000000,
    0x00030047, 0x00000006, 0x00000002, 0x00040047, 0x0000000c, 0x0000001e,
    0x00000000, 0x00040047, 0x0000000d, 0x0000001e, 0x00000001, 0x00040047,
    0x0000000f, 0x0000001e, 0x00000000, 0x00020013, 0x00000001, 0x00030021,
    0x00000002, 0x00000001, 0x00030016, 0x00000003, 0x00000020, 0x00040017,
    0x00000004, 0x00000003, 0x00000004, 0x00040017, 0x00000005, 0x00000003,
    0x00000002, 0x0003001e, 0x00000006, 0x00000004, 0x00040020, 0x00000007,
    0x00000003, 0x00000006, 0x0004003b, 0x00000007, 0x00000008, 0x00000003,
    0x00040015, 0x00000009, 0x00000020, 0x00000001, 0x0004002b, 0x00000009,
    0x0000000a, 0x00000000, 0x00040020, 0x0000000b, 0x00000001, 0x00000005,
    0x0004003b, 0x0000000b, 0x0000000c, 0x00000001, 0x0004003b, 0x0000000b,
    0x0000000d, 0x00000001, 0x00040020, 0x0000000e, 0x00000003, 0x00000005,
    0x0004003b, 0x0000000e, 0x0000000f, 0x00000003, 0x0004002b, 0x00000003,
    0x00000010, 0x00000000, 0x0004002b, 0x00000003, 0x00000011, 0x3f800000,
    0x00040020, 0x00000012, 0x00000003, 0x00000004, 0x00050036, 0x00000001,
    0x00000013, 0x00000000, 0x00000002, 0x000200f8, 0x00000014, 0x0004003d,
    0x00000005, 0x00000015, 0x0000000c, 0x00050051, 0x00000003, 0x00000016,
    0x00000015, 0x00000000, 0x00050051, 0x00000003, 0x00000017, 0x00000015,
    0x00000001, 0x00070050, 0x00000004, 0x00000018, 0x00000016, 0x00000017,
    0x00000010, 0x00000011, 0x00050041, 0x00000012, 0x00000019, 0x00000008,
    0x0000000a, 0x0003003e, 0x00000019, 0x00000018, 0x0004003d, 0x00000005,
    0x0000001a, 0x0000000d, 0x0003003e, 0x0000000f, 0x0000001a, 0x000100fd,
    0x00010038,
];

const TRIANGLE_FRAG_SPIRV: [u32; 113] = [
    0x07230203, 0x00010000, 0x00000000, 0x00000013, 0x00000000, 0x00020011,
    0x00000001, 0x0003000e, 0x00000000, 0x00000001, 0x0007000f, 0x00000004,
    0x0000000e, 0x6e69616d, 0x00000000, 0x00000007, 0x0000000d, 0x00030010,
    0x0000000e, 0x00000007, 0x00040047, 0x00000007, 0x0000001e, 0x00000000,
    0x00040047, 0x0000000d, 0x0000001e, 0x00000000, 0x00040047, 0x0000000b,
    0x00000022, 0x00000000, 0x00040047, 0x0000000b, 0x00000021, 0x00000000,
    0x00020013, 0x00000001, 0x00030021, 0x00000002, 0x00000001, 0x00030016,
    0x00000003, 0x00000020, 0x00040017, 0x00000004, 0x00000003, 0x00000004,
    0x00040017, 0x00000005, 0x00000003, 0x00000002, 0x00040020, 0x00000006,
    0x00000003, 0x00000004, 0x0004003b, 0x00000006, 0x00000007, 0x00000003,
    0x00090019, 0x00000008, 0x00000003, 0x00000001, 0x00000000, 0x00000000,
    0x00000000, 0x00000001, 0x00000000, 0x0003001b, 0x00000009, 0x00000008,
    0x00040020, 0x0000000a, 0x00000000, 0x00000009, 0x0004003b, 0x0000000a,
    0x0000000b, 0x00000000, 0x00040020, 0x0000000c, 0x00000001, 0x00000005,
    0x0004003b, 0x0000000c, 0x0000000d, 0x00000001, 0x00050036, 0x00000001,
    0x0000000e, 0x00000000, 0x00000002, 0x000200f8, 0x0000000f, 0x0004003d,
    0x00000009, 0x00000010, 0x0000000b, 0x0004003d, 0x00000005, 0x00000011,
    0x0000000d, 0x00050057, 0x00000004, 0x00000012, 0x00000010, 0x00000011,
    0x0003003e, 0x00000007, 0x00000012, 0x000100fd, 0x00010038,
];


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
    /// Every `vkDestroySurfaceKHR` that reached the host: the instance it was destroyed through
    /// and the surface, as the tokens the shim resolved them to.
    surfaces_destroyed: Vec<(HostInstance, HostSurface)>,
    /// Every live pipeline cache and the blob it holds: the guest's `pInitialData` when it gave
    /// one, [`fake_cache_blob`] when it did not.
    caches: Vec<(HostPipelineCache, Vec<u8>)>,
    /// Every `pInitialData` a `vkCreatePipelineCache` carried, byte for byte.
    cache_initial_data: Vec<Vec<u8>>,
    /// Every `vkGetPipelineCacheData` that reached the host.
    cache_reads: Vec<(HostDevice, HostPipelineCache)>,
    /// Every semaphore created and not yet destroyed: the child `destroy_device` refuses over.
    live_semaphores: Vec<HostSemaphore>,
    /// Every queue token `vkGetDeviceQueue` handed out, once each.
    queues_handed: Vec<HostQueue>,
    /// Every device a `vkDestroyDevice` actually destroyed.
    devices_destroyed: Vec<HostDevice>,

    // ---------------------------------------------------------------------------- stage 5
    /// Every `vkAllocateMemory`, as the shim decoded it, with the token it was answered with —
    /// **including whether it was imported**, which is the one fact the guest cannot see and the
    /// whole of the memory decision.
    allocations: Vec<(HostDeviceMemory, MemoryAllocation)>,
    /// Every `vkCreateBuffer` request.
    buffers: Vec<BufferRequest>,
    /// Every `vkCreateImage` request.
    images: Vec<ImageRequest>,
    /// Every `vkCreateShaderModule`'s SPIR-V, byte for byte.
    shader_code: Vec<Vec<u8>>,
    /// Every `vkCreateRenderPass` request.
    render_passes: Vec<RenderPassRequest>,
    /// Every `vkCreateGraphicsPipelines` batch.
    pipelines: Vec<Vec<GraphicsPipelineRequest>>,
    /// Every `vkCreateComputePipelines` batch.
    compute_pipelines: Vec<Vec<ComputePipelineRequest>>,
    /// Every `vkCreateDescriptorSetLayout` request.
    set_layouts: Vec<DescriptorSetLayoutRequest>,
    /// Which pool each allocated descriptor set came from.
    sets: Vec<(HostDescriptorPool, HostDescriptorSet)>,
    /// Every `vkUpdateDescriptorSets` write.
    writes: Vec<DescriptorWrite>,
    /// Every `vkCreateSampler`'s body bytes.
    sampler_bodies: Vec<Vec<u8>>,
    /// Every `vkCreateQueryPool` request.
    query_pools: Vec<QueryPoolRequest>,
    /// Every query command: `("reset", pool, first, count)` or `("timestamp", pool, stage, query)`.
    query_commands: Vec<(&'static str, HostQueryPool, u32, u32)>,
    /// Every `vkCmdDispatch`'s three group counts.
    dispatches: Vec<[u32; 3]>,
    /// Every `vkCmdCopyImage`: source and its layout, destination and its layout, the regions.
    image_copies: Vec<(HostImageRef, u32, HostImageRef, u32, Vec<u8>)>,
    /// Every `vkCmdBlitImage`: as `image_copies`, then the filter.
    image_blits: Vec<(HostImageRef, u32, HostImageRef, u32, Vec<u8>, u32)>,
}

/// The measured memory table of this machine, which the double reports so that the rewrite is
/// asserted against the arrangement it was designed for.
///
/// `docs/HANDOFF.md`: five types, of which `0xc` — types 2 and 3 — are importable, and type 4 is
/// the `DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT` ReBAR one that is host-visible and **not**.
const MEASURED_MEMORY_TYPES: [u32; 5] = [0x0, 0x1, 0x6, 0xe, 0x7];
/// `vkGetMemoryHostPointerPropertiesEXT`'s measured answer on this machine.
const MEASURED_IMPORTABLE: u32 = 0xc;

/// A `VkMemoryRequirements` blob, for a double that has no driver to ask.
fn requirements(size: u64, alignment: u64, type_bits: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; MEMORY_REQUIREMENTS_BYTES];
    bytes[0..8].copy_from_slice(&size.to_le_bytes());
    bytes[8..16].copy_from_slice(&alignment.to_le_bytes());
    bytes[16..20].copy_from_slice(&type_bits.to_le_bytes());
    bytes
}

/// `sizeof(VkPipelineCacheHeaderVersionOne)`: the part of every cache blob the specification
/// defines -- `headerSize`, `headerVersion`, `vendorID`, `deviceID`, `pipelineCacheUUID[16]`.
const CACHE_HEADER_BYTES: usize = 32;
/// One entry of [`fake_cache_blob`]'s private body: a format the shim knows nothing about, which
/// is why it may not cut one.
const FAKE_CACHE_ENTRY_BYTES: usize = 16;

/// The blob a [`StageFourHost`] cache holds when the guest gave it no `pInitialData`: the header
/// the specification defines, then three entries of a private format, each a distinct byte.
fn fake_cache_blob() -> Vec<u8> {
    let mut blob = Vec::with_capacity(CACHE_HEADER_BYTES + 3 * FAKE_CACHE_ENTRY_BYTES);
    blob.extend_from_slice(&(CACHE_HEADER_BYTES as u32).to_le_bytes());
    blob.extend_from_slice(&1u32.to_le_bytes()); // VK_PIPELINE_CACHE_HEADER_VERSION_ONE
    blob.extend_from_slice(&0x10DEu32.to_le_bytes()); // vendorID
    blob.extend_from_slice(&0x2882u32.to_le_bytes()); // deviceID
    blob.extend((0xA0u8..0xB0).collect::<Vec<u8>>()); // pipelineCacheUUID
    for entry in 0..3u8 {
        blob.extend_from_slice(&[0xC0 + entry; FAKE_CACHE_ENTRY_BYTES]);
    }
    blob
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
    /// Scripted `vkCreateGraphicsPipelines` outcomes: one `bool` per create info, oldest batch
    /// first. Empty means "every pipeline was created".
    ///
    /// **The only way to see a partial success.** No real driver can be asked to decline the
    /// second of two pipelines on demand, and that case is the entire reason
    /// `PipelinesCreated` is not a `DriverAnswer`.
    pipelines: Mutex<VecDeque<Vec<bool>>>,
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
            pipelines: Mutex::new(VecDeque::new()),
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
            || name == "vkDestroySurfaceKHR"
            || name == "vkDestroyDevice"
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

    /// **Records and nothing else.** The two rules only a host can see -- the instance pairing
    /// and a live swapchain over the surface -- are the real host's to check, and the live test
    /// checks them against it; what this double lets a test count is how many destroys reached a
    /// host at all, which is the guest-side registry's property.
    fn destroy_surface(&self, instance: HostInstance, surface: HostSurface) -> AbiResult<()> {
        self.log().surfaces_destroyed.push((instance, surface));
        self.note("vkDestroySurfaceKHR");
        Ok(())
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

    /// **This machine's measured memory table**, so that the rewrite a test asserts on is the one
    /// this stage was designed against rather than an invented arrangement.
    fn physical_device_memory_properties(&self, _d: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        let mut bytes = vec![0u8; PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES];
        bytes[0..4].copy_from_slice(&(MEASURED_MEMORY_TYPES.len() as u32).to_le_bytes());
        for (index, flags) in MEASURED_MEMORY_TYPES.iter().enumerate() {
            let at = 4 + index * 8;
            bytes[at..at + 4].copy_from_slice(&flags.to_le_bytes());
        }
        bytes[260..264].copy_from_slice(&1u32.to_le_bytes());
        bytes[264..272].copy_from_slice(&(8u64 << 30).to_le_bytes());
        Ok(bytes)
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
        let queue = HostQueue::from_token((u64::from(family) << 32) | u64::from(index));
        let mut log = self.log();
        if !log.queues_handed.contains(&queue) {
            log.queues_handed.push(queue);
        }
        Ok(queue)
    }

    /// **Refuses while a semaphore lives**, standing in for the real host's check of every child
    /// family -- which is the live test's to exercise, against `GfxVulkanHost`. What this lets a
    /// test see is the guest side of a refusal (nothing forgotten, nothing destroyed) and of a
    /// success (once, and the queues this double handed out go with the device).
    fn destroy_device(&self, device: HostDevice) -> AbiResult<Vec<HostQueue>> {
        let mut log = self.log();
        if !log.live_semaphores.is_empty() {
            return Err(AbiError::Refused {
                symbol: "vkDestroyDevice".to_string(),
                address: 0,
                why: format!(
                    "{device:?} still has objects created from it: {} `VkSemaphore`",
                    log.live_semaphores.len()
                ),
            });
        }
        log.devices_destroyed.push(device);
        log.destroyed.push("vkDestroyDevice".to_string());
        Ok(std::mem::take(&mut log.queues_handed))
    }

    fn has_device_proc(&self, _device: HostDevice, name: &str) -> AbiResult<bool> {
        // Every stage 4 and stage 5 name, and nothing else -- so a test that resolves one neither
        // stage implements gets the driver's NULL rather than a thunk.
        Ok(matches!(
            name,
            "vkAllocateMemory"
                | "vkFreeMemory"
                | "vkMapMemory"
                | "vkUnmapMemory"
                | "vkGetBufferMemoryRequirements"
                | "vkGetImageMemoryRequirements"
                | "vkBindBufferMemory"
                | "vkBindImageMemory"
                | "vkFlushMappedMemoryRanges"
                | "vkInvalidateMappedMemoryRanges"
                | "vkCreateBuffer"
                | "vkDestroyBuffer"
                | "vkCreateImage"
                | "vkDestroyImage"
                | "vkCreateSampler"
                | "vkDestroySampler"
                | "vkCreateShaderModule"
                | "vkDestroyShaderModule"
                | "vkCreatePipelineCache"
                | "vkDestroyPipelineCache"
                | "vkGetPipelineCacheData"
                | "vkCreatePipelineLayout"
                | "vkDestroyPipelineLayout"
                | "vkCreateRenderPass"
                | "vkDestroyRenderPass"
                | "vkCreateFramebuffer"
                | "vkDestroyFramebuffer"
                | "vkCreateGraphicsPipelines"
                | "vkCreateComputePipelines"
                | "vkCmdDispatch"
                | "vkCmdCopyImage"
                | "vkCmdBlitImage"
                | "vkDestroyPipeline"
                | "vkCreateDescriptorSetLayout"
                | "vkDestroyDescriptorSetLayout"
                | "vkCreateDescriptorPool"
                | "vkDestroyDescriptorPool"
                | "vkResetDescriptorPool"
                | "vkAllocateDescriptorSets"
                | "vkFreeDescriptorSets"
                | "vkUpdateDescriptorSets"
                | "vkCmdBeginRenderPass"
                | "vkCmdEndRenderPass"
                | "vkCmdBindPipeline"
                | "vkCmdBindVertexBuffers"
                | "vkCmdBindIndexBuffer"
                | "vkCmdBindDescriptorSets"
                | "vkCmdSetViewport"
                | "vkCmdSetScissor"
                | "vkCmdDraw"
                | "vkCmdDrawIndexed"
                | "vkCmdCopyBuffer"
                | "vkCmdCopyBufferToImage"
                | "vkCmdPushConstants"
                | "vkCreateSwapchainKHR"
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
                | "vkCreateQueryPool"
                | "vkDestroyQueryPool"
                | "vkCmdResetQueryPool"
                | "vkCmdWriteTimestamp"
                | "vkGetQueryPoolResults"
                | "vkCreateDescriptorUpdateTemplate"
                | "vkUpdateDescriptorSetWithTemplate"
                | "vkDestroyDescriptorUpdateTemplate"
        ))
    }

    fn create_query_pool(
        &self,
        _device: HostDevice,
        request: &QueryPoolRequest,
    ) -> AbiResult<DriverAnswer<HostQueryPool>> {
        let mut log = self.log();
        log.query_pools.push(*request);
        Ok(DriverAnswer::Ok(HostQueryPool::from_token(log.query_pools.len() as u64 - 1)))
    }

    /// **A driver's answer, from what was recorded**: a query is available when a
    /// `vkCmdWriteTimestamp` wrote it, and its value is `0x1000 + query`. The unavailable ones are
    /// left as they arrived and the answer is `VK_NOT_READY`, as a driver's is without `WAIT`.
    fn get_query_pool_results(
        &self,
        _device: HostDevice,
        pool: HostQueryPool,
        first: u32,
        count: u32,
        stride: u64,
        flags: u32,
        data: &mut [u8],
    ) -> AbiResult<i32> {
        let written: Vec<u32> = self
            .log()
            .query_commands
            .iter()
            .filter(|(what, at, _, _)| *what == "timestamp" && *at == pool)
            .map(|(_, _, _, query)| *query)
            .collect();
        let mut all = true;
        for index in 0..count {
            let query = first + index;
            if !written.contains(&query) {
                all = false;
                continue;
            }
            let at = (u64::from(index) * stride) as usize;
            let value = 0x1000 + u64::from(query);
            if flags & 1 != 0 {
                data[at..at + 8].copy_from_slice(&value.to_le_bytes());
            } else {
                data[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
            }
        }
        Ok(if all { VK_SUCCESS } else { VK_NOT_READY })
    }

    fn destroy_query_pool(&self, _pool: HostQueryPool) -> AbiResult<()> {
        self.note("vkDestroyQueryPool");
        Ok(())
    }

    /// Keeps the guest's `pInitialData` as the cache's blob, as a driver that accepted it would,
    /// so that a blob read back out is a blob that went in.
    fn create_pipeline_cache(
        &self,
        _device: HostDevice,
        _flags: u32,
        initial_data: &[u8],
    ) -> AbiResult<DriverAnswer<HostPipelineCache>> {
        let token = HostPipelineCache::from_token(self.token());
        let mut log = self.log();
        log.cache_initial_data.push(initial_data.to_vec());
        let blob = if initial_data.is_empty() { fake_cache_blob() } else { initial_data.to_vec() };
        log.caches.push((token, blob));
        Ok(DriverAnswer::Ok(token))
    }

    fn destroy_pipeline_cache(&self, cache: HostPipelineCache) -> AbiResult<()> {
        self.log().caches.retain(|(token, _)| *token != cache);
        self.note("vkDestroyPipelineCache");
        Ok(())
    }

    /// The whole blob, as the trait requires: the guest's two-call idiom is the shim's to run.
    fn pipeline_cache_data(
        &self,
        device: HostDevice,
        cache: HostPipelineCache,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let mut log = self.log();
        log.cache_reads.push((device, cache));
        let Some((_, blob)) = log.caches.iter().find(|(token, _)| *token == cache) else {
            return Err(AbiError::Refused {
                symbol: "StageFourHost::pipeline_cache_data".to_string(),
                address: 0,
                why: format!("{cache:?} is not a cache this double holds"),
            });
        };
        Ok(DriverAnswer::Ok(blob.clone()))
    }

    fn cmd_reset_query_pool(
        &self,
        _buffer: HostCommandBuffer,
        pool: HostQueryPool,
        first: u32,
        count: u32,
    ) -> AbiResult<()> {
        self.log().query_commands.push(("reset", pool, first, count));
        Ok(())
    }

    fn cmd_write_timestamp(
        &self,
        _buffer: HostCommandBuffer,
        stage: u32,
        pool: HostQueryPool,
        query: u32,
    ) -> AbiResult<()> {
        self.log().query_commands.push(("timestamp", pool, stage, query));
        Ok(())
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
        let semaphore = HostSemaphore::from_token(self.token());
        self.log().live_semaphores.push(semaphore);
        Ok(DriverAnswer::Ok(semaphore))
    }

    fn destroy_semaphore(&self, semaphore: HostSemaphore) -> AbiResult<()> {
        self.log().live_semaphores.retain(|live| *live != semaphore);
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


    // ================================================================= stage 5 on the double
    //
    // Enough of stage 5 for the four properties **no real driver can be made to produce**: a
    // host-visible memory type that cannot be imported into, a `vkMapMemory` on a forwarded
    // allocation, a `vkCreateGraphicsPipelines` that partly fails, and a descriptor pool that
    // takes its sets with it. Everything else about stage 5 is asserted live, against a real
    // NVIDIA driver, further down this file.

    /// The measured table of this machine, so the rewrite is asserted against the arrangement it
    /// was designed for.
    ///
    /// `docs/HANDOFF.md`: five types, of which `0xc` — types 2 and 3 — are importable, and type 4
    /// is the `DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT` ReBAR one that is host-visible and
    /// **not** importable. That fifth type is the whole reason the memory-type rewrite exists.
    fn importable_memory_types(&self, _device: HostPhysicalDevice) -> AbiResult<u32> {
        Ok(MEASURED_IMPORTABLE)
    }

    fn memory_plan(&self, _device: HostDevice, memory_type_index: u32) -> AbiResult<MemoryPlan> {
        let flags = *MEASURED_MEMORY_TYPES.get(memory_type_index as usize).ok_or_else(|| {
            AbiError::Refused {
                symbol: "StageFourHost::memory_plan".to_string(),
                address: 0,
                why: format!("memory type {memory_type_index} is past this double's five"),
            }
        })?;
        Ok(MemoryPlan {
            property_flags: flags,
            importable: MEASURED_IMPORTABLE & (1 << memory_type_index) != 0,
            import_alignment: 4096,
        })
    }

    fn allocate_memory(
        &self,
        _device: HostDevice,
        allocation: &MemoryAllocation,
    ) -> AbiResult<DriverAnswer<HostDeviceMemory>> {
        let token = self.token();
        self.log().allocations.push((HostDeviceMemory::from_token(token), *allocation));
        Ok(DriverAnswer::Ok(HostDeviceMemory::from_token(token)))
    }

    fn free_memory(&self, _memory: HostDeviceMemory) -> AbiResult<()> {
        self.note("vkFreeMemory");
        Ok(())
    }

    /// **The pointer that was imported, plus the offset** — which is what a real driver does with
    /// `VK_EXT_external_memory_host` and what the shim checks its answer against.
    ///
    /// A forwarded allocation never reaches here: the shim refuses `vkMapMemory` on one before
    /// asking, because there is no `GuestSpace` mapping behind it to answer with.
    fn map_memory(
        &self,
        memory: HostDeviceMemory,
        offset: u64,
        _size: u64,
        _flags: u32,
    ) -> AbiResult<DriverAnswer<u64>> {
        let imported = self
            .log()
            .allocations
            .iter()
            .find(|(token, _)| *token == memory)
            .and_then(|(_, allocation)| allocation.host_pointer);
        match imported {
            Some(pointer) => Ok(DriverAnswer::Ok(pointer + offset)),
            // Unreachable from a conforming shim: it refuses `vkMapMemory` on a forwarded
            // allocation before asking. A code rather than a panic, so a shim that stopped
            // refusing would show up as a failing assertion rather than as a crash in a double.
            None => Ok(DriverAnswer::Failed(-1)),
        }
    }

    fn unmap_memory(&self, _memory: HostDeviceMemory) -> AbiResult<()> {
        Ok(())
    }

    fn create_buffer(
        &self,
        _device: HostDevice,
        request: &BufferRequest,
    ) -> AbiResult<DriverAnswer<HostBuffer>> {
        self.log().buffers.push(request.clone());
        Ok(DriverAnswer::Ok(HostBuffer::from_token(self.token())))
    }

    fn destroy_buffer(&self, _buffer: HostBuffer) -> AbiResult<()> {
        self.note("vkDestroyBuffer");
        Ok(())
    }

    fn create_image(
        &self,
        _device: HostDevice,
        request: &ImageRequest,
    ) -> AbiResult<DriverAnswer<HostCreatedImage>> {
        self.log().images.push(request.clone());
        Ok(DriverAnswer::Ok(HostCreatedImage::from_token(self.token())))
    }

    fn destroy_image(&self, _image: HostCreatedImage) -> AbiResult<()> {
        self.note("vkDestroyImage");
        Ok(())
    }

    fn buffer_memory_requirements(&self, _buffer: HostBuffer) -> AbiResult<Vec<u8>> {
        Ok(requirements(256, 16, 0b1_1100))
    }

    fn image_memory_requirements(&self, _image: HostCreatedImage) -> AbiResult<Vec<u8>> {
        Ok(requirements(4096, 256, 0b1_1111))
    }

    /// Different numbers from a created image's, so the family that answered is visible.
    fn swapchain_image_memory_requirements(&self, _image: HostImage) -> AbiResult<Vec<u8>> {
        Ok(requirements(8192, 1024, 0b10))
    }

    fn bind_buffer_memory(
        &self,
        _buffer: HostBuffer,
        _memory: HostDeviceMemory,
        _offset: u64,
    ) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn bind_image_memory(
        &self,
        _image: HostCreatedImage,
        _memory: HostDeviceMemory,
        _offset: u64,
    ) -> AbiResult<DriverAnswer<()>> {
        Ok(DriverAnswer::Ok(()))
    }

    fn create_shader_module(
        &self,
        _device: HostDevice,
        _flags: u32,
        code: &[u8],
    ) -> AbiResult<DriverAnswer<HostShaderModule>> {
        self.log().shader_code.push(code.to_vec());
        Ok(DriverAnswer::Ok(HostShaderModule::from_token(self.token())))
    }

    fn destroy_shader_module(&self, _module: HostShaderModule) -> AbiResult<()> {
        self.note("vkDestroyShaderModule");
        Ok(())
    }

    fn create_pipeline_layout(
        &self,
        _device: HostDevice,
        _request: &PipelineLayoutRequest,
    ) -> AbiResult<DriverAnswer<HostPipelineLayout>> {
        Ok(DriverAnswer::Ok(HostPipelineLayout::from_token(self.token())))
    }

    fn create_render_pass(
        &self,
        _device: HostDevice,
        request: &RenderPassRequest,
    ) -> AbiResult<DriverAnswer<HostRenderPass>> {
        self.log().render_passes.push(request.clone());
        Ok(DriverAnswer::Ok(HostRenderPass::from_token(self.token())))
    }

    /// **The partial success**, scripted. See [`StageFourHost::pipelines`].
    fn create_graphics_pipelines(
        &self,
        _device: HostDevice,
        _cache: Option<HostPipelineCache>,
        requests: &[GraphicsPipelineRequest],
    ) -> AbiResult<PipelinesCreated> {
        self.log().pipelines.push(requests.to_vec());
        let scripted = self.pipelines.lock().expect("no panic holds this").pop_front();
        match scripted {
            Some(outcomes) => Ok(PipelinesCreated {
                result: if outcomes.iter().all(|made| *made) { VK_SUCCESS } else { -2 },
                pipelines: outcomes
                    .iter()
                    .map(|made| made.then(|| HostPipeline::from_token(self.token())))
                    .collect(),
            }),
            None => Ok(PipelinesCreated {
                result: VK_SUCCESS,
                pipelines: requests
                    .iter()
                    .map(|_| Some(HostPipeline::from_token(self.token())))
                    .collect(),
            }),
        }
    }

    fn cmd_dispatch(&self, _buffer: HostCommandBuffer, x: u32, y: u32, z: u32) -> AbiResult<()> {
        self.log().dispatches.push([x, y, z]);
        Ok(())
    }

    fn cmd_copy_image(
        &self,
        _buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        self.log().image_copies.push((
            source,
            source_layout,
            destination,
            destination_layout,
            regions.to_vec(),
        ));
        Ok(())
    }

    fn cmd_blit_image(
        &self,
        _buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
        filter: u32,
    ) -> AbiResult<()> {
        self.log().image_blits.push((
            source,
            source_layout,
            destination,
            destination_layout,
            regions.to_vec(),
            filter,
        ));
        Ok(())
    }

    /// The same scripted partial success as [`StageFourHost::create_graphics_pipelines`].
    fn create_compute_pipelines(
        &self,
        _device: HostDevice,
        _cache: Option<HostPipelineCache>,
        requests: &[ComputePipelineRequest],
    ) -> AbiResult<PipelinesCreated> {
        self.log().compute_pipelines.push(requests.to_vec());
        let outcomes = self
            .pipelines
            .lock()
            .expect("no panic holds this")
            .pop_front()
            .unwrap_or_else(|| vec![true; requests.len()]);
        Ok(PipelinesCreated {
            result: if outcomes.iter().all(|made| *made) { VK_SUCCESS } else { -2 },
            pipelines: outcomes
                .iter()
                .map(|made| made.then(|| HostPipeline::from_token(self.token())))
                .collect(),
        })
    }

    fn destroy_pipeline(&self, _pipeline: HostPipeline) -> AbiResult<()> {
        self.note("vkDestroyPipeline");
        Ok(())
    }

    fn create_descriptor_set_layout(
        &self,
        _device: HostDevice,
        request: &DescriptorSetLayoutRequest,
    ) -> AbiResult<DriverAnswer<HostDescriptorSetLayout>> {
        self.log().set_layouts.push(request.clone());
        Ok(DriverAnswer::Ok(HostDescriptorSetLayout::from_token(self.token())))
    }

    fn create_descriptor_pool(
        &self,
        _device: HostDevice,
        _request: &DescriptorPoolRequest,
    ) -> AbiResult<DriverAnswer<HostDescriptorPool>> {
        Ok(DriverAnswer::Ok(HostDescriptorPool::from_token(self.token())))
    }

    fn destroy_descriptor_pool(&self, pool: HostDescriptorPool) -> AbiResult<()> {
        self.note("vkDestroyDescriptorPool");
        self.log().sets.retain(|(owner, _)| *owner != pool);
        Ok(())
    }

    fn descriptor_sets_of(&self, pool: HostDescriptorPool) -> AbiResult<Vec<HostDescriptorSet>> {
        Ok(self
            .log()
            .sets
            .iter()
            .filter(|(owner, _)| *owner == pool)
            .map(|(_, set)| *set)
            .collect())
    }

    fn allocate_descriptor_sets(
        &self,
        pool: HostDescriptorPool,
        layouts: &[HostDescriptorSetLayout],
    ) -> AbiResult<DriverAnswer<Vec<HostDescriptorSet>>> {
        let sets: Vec<HostDescriptorSet> =
            layouts.iter().map(|_| HostDescriptorSet::from_token(self.token())).collect();
        for set in &sets {
            self.log().sets.push((pool, *set));
        }
        Ok(DriverAnswer::Ok(sets))
    }

    fn update_descriptor_sets(
        &self,
        _device: HostDevice,
        writes: &[DescriptorWrite],
        _copies: &[DescriptorCopy],
    ) -> AbiResult<()> {
        self.log().writes.extend_from_slice(writes);
        Ok(())
    }

    fn create_sampler(
        &self,
        _device: HostDevice,
        body: &[u8],
    ) -> AbiResult<DriverAnswer<HostSampler>> {
        self.log().sampler_bodies.push(body.to_vec());
        Ok(DriverAnswer::Ok(HostSampler::from_token(self.token())))
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
    /// `vkGetInstanceProcAddr`, for the instance-level names -- `vkDestroySurfaceKHR` among them.
    entry_point: u64,
    instance: u64,
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

    UpToADevice { f, host, entry_point, instance, surface, device, get_proc, queue }
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

/// **A descriptor update template is the writes its entries describe**: an update reads each
/// descriptor out of the guest's data at `offset + i * stride`, resolves its handles, and reaches
/// the host as ordinary writes -- two uniform-buffer descriptors and a sampler here. A destroyed
/// template is no longer a handle, a push-descriptor template refuses, and so does a texel-buffer
/// entry.
#[test]
fn a_descriptor_update_template_becomes_the_writes_it_describes() {
    let _serial = serialized();
    let m = up_to_memory("update-template");
    let f = &m.up.f;
    let device = m.up.device;
    let name = |call: &str| f.resolve_device(m.up.get_proc, device, call);
    let out = f.alloc(8);
    f.call(name("vkCreateDescriptorSetLayout"), [device, f.descriptor_set_layout_info(), 0, out])
        .expect("layout");
    let set_layout = f.guest.read_u64(out as GuestAddr);
    f.call(name("vkCreateDescriptorPool"), [device, f.descriptor_pool_info(), 0, out]).expect("pool");
    let pool = f.guest.read_u64(out as GuestAddr);
    let set_out = f.alloc(8);
    f.call(
        name("vkAllocateDescriptorSets"),
        [device, f.descriptor_set_allocate_info(pool, set_layout), set_out, 0],
    )
    .expect("a set");
    let set = f.guest.read_u64(set_out as GuestAddr);
    f.call(name("vkCreateBuffer"), [device, f.buffer_info(256, 0x10), 0, out]).expect("a buffer");
    let buffer = f.guest.read_u64(out as GuestAddr);
    f.call(name("vkCreateSampler"), [device, f.sampler_info(), 0, out]).expect("a sampler");
    let sampler = f.guest.read_u64(out as GuestAddr);

    // Two entries: binding 0, two UNIFORM_BUFFER (6) descriptors at 8 and 48; binding 1, one
    // SAMPLER (0) at 96.
    let entry = |binding: u32, count: u32, kind: u32, offset: u64, stride: u64| -> Vec<u8> {
        let mut bytes = vec![0u8; DESCRIPTOR_UPDATE_TEMPLATE_ENTRY_BYTES];
        bytes[0..4].copy_from_slice(&binding.to_le_bytes());
        bytes[8..12].copy_from_slice(&count.to_le_bytes());
        bytes[12..16].copy_from_slice(&kind.to_le_bytes());
        bytes[16..24].copy_from_slice(&offset.to_le_bytes());
        bytes[24..32].copy_from_slice(&stride.to_le_bytes());
        bytes
    };
    let template_info = |entries: &[Vec<u8>], template_type: u32| -> u64 {
        let array = f.bytes(&entries.concat());
        let mut bytes = vec![0u8; DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&array.to_le_bytes());
        bytes[32..36].copy_from_slice(&template_type.to_le_bytes());
        bytes[40..48].copy_from_slice(&set_layout.to_le_bytes());
        f.bytes(&bytes)
    };
    let info = template_info(&[entry(0, 2, 6, 8, 40), entry(1, 1, 0, 96, 24)], 0);
    assert_eq!(
        f.call(name("vkCreateDescriptorUpdateTemplate"), [device, info, 0, out]).expect("create")
            as i32,
        VK_SUCCESS
    );
    let template = f.guest.read_u64(out as GuestAddr);

    let mut data = vec![0u8; 128];
    for (at, offset, range) in [(8usize, 16u64, 64u64), (48, 128, 32)] {
        data[at..at + 8].copy_from_slice(&buffer.to_le_bytes());
        data[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
        data[at + 16..at + 24].copy_from_slice(&range.to_le_bytes());
    }
    data[96..104].copy_from_slice(&sampler.to_le_bytes());
    let data_at = f.bytes(&data);
    let before = m.up.host.log().writes.len();
    f.call(name("vkUpdateDescriptorSetWithTemplate"), [device, set, template, data_at])
        .expect("the update");
    let writes = m.up.host.log().writes[before..].to_vec();
    assert_eq!(writes.len(), 2, "one write per entry");
    assert_eq!((writes[0].binding, writes[0].descriptor_type), (0, 6));
    match &writes[0].writes {
        DescriptorWrites::Buffers(buffers) => {
            let ranges: Vec<(u64, u64)> = buffers.iter().map(|(_, o, r)| (*o, *r)).collect();
            assert_eq!(ranges, vec![(16, 64), (128, 32)], "each at offset + i * stride");
        }
        other => panic!("buffer descriptors, not {other:?}"),
    }
    assert_eq!((writes[1].binding, writes[1].descriptor_type), (1, 0));
    match &writes[1].writes {
        DescriptorWrites::Images(images) => {
            assert_eq!(images.len(), 1);
            assert!(images[0].0.is_some() && images[0].1.is_none(), "a bare sampler: {images:?}");
        }
        other => panic!("an image descriptor, not {other:?}"),
    }

    f.call(name("vkDestroyDescriptorUpdateTemplate"), [device, template, 0, 0]).expect("destroy");
    let text = f
        .refusal(name("vkUpdateDescriptorSetWithTemplate"), &[device, set, template, data_at])
        .to_string();
    assert!(text.contains("VkDescriptorUpdateTemplate"), "{text}");

    let push = template_info(&[entry(0, 1, 6, 0, 24)], 1);
    let text = f.refusal(name("vkCreateDescriptorUpdateTemplate"), &[device, push, 0, out]).to_string();
    assert!(text.contains("templateType = 1"), "{text}");
    let texel = template_info(&[entry(0, 1, 4, 0, 8)], 0);
    let text = f.refusal(name("vkCreateDescriptorUpdateTemplate"), &[device, texel, 0, out]).to_string();
    assert!(text.contains("descriptorType = 4"), "{text}");
}

/// **The engine's GPU timer: a timestamp query pool, created member by member and destroyed
/// once** -- `gpuTimeQueryPool` (`0x2592d68`..`0x2592da0`). A second destroy of the same handle
/// refuses, naming the family, and a `pNext` refuses by name.
#[test]
fn a_query_pool_is_created_member_by_member_and_destroyed_once() {
    let _serial = serialized();
    let up = up_to_a_device("query-pool");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreateQueryPool");
    let destroy = up.f.resolve_device(up.get_proc, up.device, "vkDestroyQueryPool");

    let info = up.f.alloc(QUERY_POOL_CREATE_INFO_BYTES);
    up.f.guest.write_u64(info as GuestAddr, 11);
    up.f.guest.write_u64(info as GuestAddr + 8, 0);
    // flags 0, VK_QUERY_TYPE_TIMESTAMP (2), 8 queries, no statistics.
    up.f.guest.write_u64(info as GuestAddr + 16, 2 << 32);
    up.f.guest.write_u64(info as GuestAddr + 24, 8);
    let out = up.f.alloc(8);
    assert_eq!(up.f.call(create, [up.device, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let pool = up.f.guest.read_u64(out as GuestAddr);
    assert_ne!(pool, 0);
    assert_eq!(
        up.host.log().query_pools,
        vec![QueryPoolRequest { flags: 0, query_type: 2, query_count: 8, pipeline_statistics: 0 }]
    );

    up.f.call(destroy, [up.device, pool, 0, 0]).expect("destroy");
    assert!(up.host.log().destroyed.iter().any(|name| name == "vkDestroyQueryPool"));
    let text = up.f.refusal(destroy, &[up.device, pool, 0, 0]).to_string();
    assert!(text.contains("VkQueryPool"), "a destroyed pool is no longer a handle: {text}");

    up.f.guest.write_u64(info as GuestAddr + 8, 0x1234);
    let text = up.f.refusal(create, &[up.device, info, 0, out]).to_string();
    assert!(text.contains("pNext"), "{text}");
}

/// **The GPU timer's commands reach the host with their pool and their numbers**: the engine's
/// first recorded command is `vkCmdResetQueryPool(pool, 0, 2)`, and a timestamp pool is written by
/// `vkCmdWriteTimestamp` and nothing else. A handle of another family where the pool goes refuses.
#[test]
fn the_gpu_timers_commands_carry_their_pool_and_numbers() {
    let _serial = serialized();
    let up = up_to_a_device("query-commands");
    let f = &up.f;
    let name = |call: &str| f.resolve_device(up.get_proc, up.device, call);
    let info = f.alloc(QUERY_POOL_CREATE_INFO_BYTES);
    f.guest.write_u64(info as GuestAddr, 11);
    f.guest.write_u64(info as GuestAddr + 16, 2 << 32);
    f.guest.write_u64(info as GuestAddr + 24, 2);
    let out = f.alloc(8);
    f.call(name("vkCreateQueryPool"), [up.device, info, 0, out]).expect("a pool");
    let pool = f.guest.read_u64(out as GuestAddr);
    let command = {
        let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
        f.call(name("vkCreateCommandPool"), [up.device, pool_info, 0, out]).expect("command pool");
        let command_pool = f.guest.read_u64(out as GuestAddr);
        let buffers_at = f.alloc(8);
        f.call(
            name("vkAllocateCommandBuffers"),
            [up.device, f.command_buffer_allocate_info(command_pool, 1), buffers_at, 0],
        )
        .expect("a command buffer");
        f.guest.read_u64(buffers_at as GuestAddr)
    };
    f.call(name("vkCmdResetQueryPool"), [command, pool, 0, 2]).expect("reset");
    // VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT (0x2000), query 1.
    f.call(name("vkCmdWriteTimestamp"), [command, 0x2000, pool, 1]).expect("timestamp");
    let commands = up.host.log().query_commands.clone();
    let host_pool = HostQueryPool::from_token(0);
    assert_eq!(
        commands,
        vec![("reset", host_pool, 0, 2), ("timestamp", host_pool, 0x2000, 1)],
        "each argument where it belongs"
    );
    let text = f.refusal(name("vkCmdResetQueryPool"), &[command, command, 0, 2]).to_string();
    assert!(text.contains("VkQueryPool"), "a command buffer is not a pool: {text}");

    // **The read-back, as the engine makes it**: `(0, 2, 16, pData, 8, VK_QUERY_RESULT_64_BIT)`,
    // no `WAIT`. Only query 1 has been written, so the driver's answer is `VK_NOT_READY`, query 1
    // is its value and **query 0 is still the guest's own bytes** -- not a zero the host made up.
    let get = name("vkGetQueryPoolResults");
    let results = f.poisoned(16, 0x5A);
    let answer = f.call_n(get, &[up.device, pool, 0, 2, 16, results, 8, 1]).expect("a read-back");
    assert_eq!(answer as i32, VK_NOT_READY, "the driver's own code");
    assert_eq!(f.guest.read_u64(results as GuestAddr), 0x5A5A_5A5A_5A5A_5A5A, "left as it was");
    assert_eq!(f.guest.read_u64(results as GuestAddr + 8), 0x1001, "query 1's timestamp");
    // Query 0 written too, and the same read-back is whole.
    f.call(name("vkCmdWriteTimestamp"), [command, 0x2000, pool, 0]).expect("timestamp");
    let answer = f.call_n(get, &[up.device, pool, 0, 2, 16, results, 8, 1]).expect("a read-back");
    assert_eq!(answer as i32, VK_SUCCESS);
    assert_eq!(f.guest.read_u64(results as GuestAddr), 0x1000, "query 0's timestamp");

    // A buffer too small for what the driver would write is refused by name, and so is a range
    // past the pool's two queries.
    let text = f.refusal(get, &[up.device, pool, 0, 2, 8, results, 8, 1]).to_string();
    assert!(text.contains("dataSize = 8"), "{text}");
    let text = f.refusal(get, &[up.device, pool, 1, 2, 16, results, 8, 1]).to_string();
    assert!(text.contains("pool of 2"), "{text}");
}

/// **`vkCmdDispatch` carries its three group counts in order**, and a handle that is not a
/// command buffer is refused rather than recorded into.
#[test]
fn a_dispatch_carries_its_three_group_counts_in_order() {
    let _serial = serialized();
    let up = up_to_a_device("dispatch");
    let f = &up.f;
    let name = |call: &str| f.resolve_device(up.get_proc, up.device, call);
    let out = f.alloc(8);
    let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
    f.call(name("vkCreateCommandPool"), [up.device, pool_info, 0, out]).expect("command pool");
    let command_pool = f.guest.read_u64(out as GuestAddr);
    let buffers_at = f.alloc(8);
    f.call(
        name("vkAllocateCommandBuffers"),
        [up.device, f.command_buffer_allocate_info(command_pool, 1), buffers_at, 0],
    )
    .expect("a command buffer");
    let command = f.guest.read_u64(buffers_at as GuestAddr);
    // The engine's own first dispatch, then one whose three counts all differ from it.
    f.call(name("vkCmdDispatch"), [command, 5, 3, 1]).expect("dispatch");
    f.call(name("vkCmdDispatch"), [command, 7, 11, 13]).expect("dispatch");
    assert_eq!(up.host.log().dispatches, vec![[5, 3, 1], [7, 11, 13]]);
    let text = f.refusal(name("vkCmdDispatch"), &[command_pool, 1, 1, 1]).to_string();
    assert!(text.contains("vkCmdDispatch"), "a command pool is not a command buffer: {text}");
    assert_eq!(up.host.log().dispatches.len(), 2, "the refused one recorded nothing");
}

/// **`vkCmdCopyImage` carries each image with its own layout, in order, and the regions whole.**
///
/// The source is an image the guest created and the destination a swapchain image, so the two
/// arrive as different families: a shim that swapped the images, or the layouts, is caught.
#[test]
fn an_image_copy_carries_each_image_with_its_own_layout() {
    let _serial = serialized();
    let up = up_to_a_device("copy-image");
    let f = &up.f;
    let name = |call: &str| f.resolve_device(up.get_proc, up.device, call);
    let out = f.alloc(8);
    let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
    f.call(name("vkCreateCommandPool"), [up.device, pool_info, 0, out]).expect("command pool");
    let command_pool = f.guest.read_u64(out as GuestAddr);
    let buffers_at = f.alloc(8);
    f.call(
        name("vkAllocateCommandBuffers"),
        [up.device, f.command_buffer_allocate_info(command_pool, 1), buffers_at, 0],
    )
    .expect("a command buffer");
    let command = f.guest.read_u64(buffers_at as GuestAddr);

    let info = f.image_info(8, 8, FORMAT_B8G8R8A8_UNORM, IMAGE_USAGE_TEXTURE);
    f.call(name("vkCreateImage"), [up.device, info, 0, out]).expect("an image");
    let created = f.guest.read_u64(out as GuestAddr);
    let info = f.swapchain_info(up.surface, 2, FORMAT_B8G8R8A8_UNORM, 8, 8, SWAPCHAIN_USAGE, 1, 0);
    f.call(name("vkCreateSwapchainKHR"), [up.device, info, 0, out]).expect("swapchain");
    let swapchain = f.guest.read_u64(out as GuestAddr);
    let count_at = f.alloc(8);
    f.call(name("vkGetSwapchainImagesKHR"), [up.device, swapchain, count_at, 0]).expect("count");
    let images_at = f.alloc(f.read_u32(count_at) as usize * 8);
    f.call(name("vkGetSwapchainImagesKHR"), [up.device, swapchain, count_at, images_at])
        .expect("images");
    let presented = f.guest.read_u64(images_at as GuestAddr);

    // One region, every one of its seventeen words different.
    let region: Vec<u8> = (1u32..=17).flat_map(|word| (word * 0x0101).to_le_bytes()).collect();
    assert_eq!(region.len(), IMAGE_COPY_BYTES);
    let regions = f.bytes(&region);
    // TRANSFER_SRC_OPTIMAL (6) and TRANSFER_DST_OPTIMAL (7), as the engine passes them.
    f.call_n(name("vkCmdCopyImage"), &[command, created, 6, presented, 7, 1, regions])
        .expect("the copy is recorded");
    let copies = up.host.log().image_copies.clone();
    assert_eq!(copies.len(), 1);
    let (source, source_layout, destination, destination_layout, bytes) = &copies[0];
    assert!(matches!(source, HostImageRef::Created(_)), "the source is the created one: {source:?}");
    assert!(
        matches!(destination, HostImageRef::Swapchain(_)),
        "the destination is the swapchain's: {destination:?}"
    );
    assert_eq!((*source_layout, *destination_layout), (6, 7), "each layout with its own image");
    assert_eq!(bytes, &region, "the region, whole");

    let text = f.refusal(name("vkCmdCopyImage"), &[command, created, 6, presented, 7, 0, regions]);
    assert!(text.to_string().contains("regionCount = 0"), "{text}");
    assert_eq!(up.host.log().image_copies.len(), 1, "the refused one recorded nothing");

    // **`vkCmdBlitImage`: the engine's shape, an image into itself with LINEAR**, then a
    // cross-family blit with NEAREST, so the filter in `x7` and each image's place both show.
    let blit: Vec<u8> = (1u32..=20).flat_map(|word| (word * 0x0203).to_le_bytes()).collect();
    assert_eq!(blit.len(), IMAGE_BLIT_BYTES);
    let blits = f.bytes(&blit);
    f.call_n(name("vkCmdBlitImage"), &[command, created, 6, created, 7, 1, blits, 1])
        .expect("the mip blit is recorded");
    f.call_n(name("vkCmdBlitImage"), &[command, created, 6, presented, 7, 1, blits, 0])
        .expect("the cross-family blit is recorded");
    let recorded = up.host.log().image_blits.clone();
    assert_eq!(recorded.len(), 2);
    let (source, source_layout, destination, destination_layout, bytes, filter) = &recorded[0];
    assert_eq!(source, destination, "one image, source and destination: {source:?}");
    assert!(matches!(source, HostImageRef::Created(_)), "{source:?}");
    assert_eq!((*source_layout, *destination_layout, *filter), (6, 7, 1), "LINEAR, from x7");
    assert_eq!(bytes, &blit, "the region, whole");
    let (source, _, destination, _, _, filter) = &recorded[1];
    assert!(matches!(source, HostImageRef::Created(_)), "{source:?}");
    assert!(matches!(destination, HostImageRef::Swapchain(_)), "{destination:?}");
    assert_eq!(*filter, 0, "NEAREST");
}

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
    // **Stage 5 changed which refusal this is**, and the new one is the stronger statement: the
    // handle is not in *either* image registry, because there are now two — a swapchain's and the
    // guest's own `vkCreateImage` images — and a stale swapchain handle is in neither.
    assert!(
        text.contains("either** image family"),
        "it says the handle is in neither registry: {text}"
    );
    assert!(text.contains("0 swapchain image(s)"), "and how many of each are real: {text}");

    // `VK_NULL_HANDLE` is the specified no-op and must not refuse.
    up.f.call(destroy, [up.device, 0, 0, 0]).expect("a null destroy is a no-op");
}

// ========================================================================= vkDestroySurfaceKHR

/// **A surface is destroyed once: the host is asked exactly once, and afterwards the guest's
/// handle names nothing.**
///
/// The call the engine was measured making on its way out of `APP_CMD_TERM_WINDOW`, right after
/// `vkDestroySwapchainKHR`, reached through `vkGetInstanceProcAddr`. This test owns the guest-side
/// half: the handle is looked up rather than forwarded, the slot is freed after the host destroyed
/// the surface, and a second destroy of the same handle is a refusal naming it rather than a
/// second host destroy -- a double free no driver is required to notice. `VK_NULL_HANDLE` is the
/// specified no-op; a wild handle, a wild `VkInstance` and a guest allocator refuse and reach no
/// host. The two rules only a host can see -- the instance pairing and a swapchain still live over
/// the surface -- are the live test's, against the real one.
#[test]
fn a_surface_is_destroyed_once_and_its_handle_names_nothing_afterwards() {
    let _serial = serialized();
    let up = up_to_a_device("destroy-surface");
    let destroy = up.f.resolve(up.entry_point, up.instance, "vkDestroySurfaceKHR");
    let issued = up.f.vulkan().surface_handles();
    assert_eq!(issued.len(), 1);
    let (handle, token) = issued[0];
    assert_eq!(handle as u64, up.surface);

    // `VK_NULL_HANDLE`: the specified no-op. Nothing reaches the host and nothing is freed.
    up.f.call(destroy, [up.instance, 0, 0, 0]).expect("a null destroy is a no-op");
    assert!(up.host.log().surfaces_destroyed.is_empty(), "a null destroy reached the host");
    assert_eq!(up.f.vulkan().surface_handles().len(), 1);

    // A handle this layer did not issue -- the next slot of the surface registry, in range and on
    // a slot boundary, and empty -- refuses naming the family and the value.
    let wild = up.surface + HANDLE_SLOT_BYTES as u64;
    let text = up.f.refusal(destroy, &[up.instance, wild, 0, 0]).to_string();
    assert!(text.contains("`VkSurfaceKHR`"), "it names the family: {text}");
    assert!(text.contains(&format!("{wild:#x}")), "it names the handle: {text}");
    // A `VkInstance` that is not one, even beside a real surface.
    let text = up.f.refusal(destroy, &[up.instance + 8, up.surface, 0, 0]).to_string();
    assert!(text.contains("`VkInstance`"), "{text}");
    // A guest allocator is refused and counted, as on every other destroy.
    let before = up.f.vulkan().allocator_non_null();
    let text = up.f.refusal(destroy, &[up.instance, up.surface, 0x1000, 0]).to_string();
    assert!(text.contains("pAllocator"), "{text}");
    assert_eq!(up.f.vulkan().allocator_non_null(), before + 1);
    assert!(up.host.log().surfaces_destroyed.is_empty(), "none of the three reached the host");
    assert_eq!(up.f.vulkan().surface_handles().len(), 1, "and the surface is still live");

    // **The destroy**: once, with the instance and the surface as the tokens the shim resolved.
    up.f.call(destroy, [up.instance, up.surface, 0, 0]).expect("destroy the surface");
    assert_eq!(
        up.host.log().surfaces_destroyed,
        vec![(HostInstance::from_token(0), token)],
        "the host was asked once, about this surface, through this instance"
    );
    assert!(up.f.vulkan().surface_handles().is_empty(), "the slot is freed");

    // **The second destroy refuses naming the handle, and reaches no host.**
    let text = up.f.refusal(destroy, &[up.instance, up.surface, 0, 0]).to_string();
    assert!(text.contains("`VkSurfaceKHR`"), "{text}");
    assert!(text.contains(&format!("{:#x}", up.surface)), "it names the handle: {text}");
    assert!(text.contains("0 live one(s)"), "and says none is live: {text}");
    assert_eq!(up.host.log().surfaces_destroyed.len(), 1, "the host was asked once, not twice");

    // Nor does anything else that takes a surface accept the stale handle.
    let support = up.f.resolve(up.entry_point, up.instance, "vkGetPhysicalDeviceSurfaceSupportKHR");
    let physical = up.f.vulkan().physical_device_handles()[0].0 as u64;
    let supported_at = up.f.alloc(8);
    let text = up.f.refusal(support, &[physical, 0, up.surface, supported_at]).to_string();
    assert!(text.contains("`VkSurfaceKHR`"), "{text}");

    // The freed slot is reused rather than leaked: the next `onSurfaceCreated` gets a surface.
    let again = up.f.a_surface(up.entry_point, up.instance);
    assert_eq!(again, up.surface, "the lowest free slot, which is the one just freed");
    assert_eq!(up.f.vulkan().surface_handles().len(), 1);
}

// ====================================================================== vkGetPipelineCacheData

/// **`vkGetPipelineCacheData`'s two-call idiom, and a short buffer that gets nothing.**
///
/// Measured: the engine's render thread saves its pipeline cache on `APP_CMD_TERM_WINDOW`, through
/// the thunk `vkGetInstanceProcAddr` handed out, so that the next launch can pass it back to
/// `vkCreatePipelineCache`. What this test owns:
///
/// * `pData = NULL` writes the size and nothing else, `VK_SUCCESS`;
/// * a buffer big enough gets the whole blob, `VK_SUCCESS`, and nothing past it;
/// * a buffer one byte short, or with room for the header alone, gets **nothing**: `*pDataSize = 0`
///   and `VK_INCOMPLETE`. A prefix of a driver's private format is not valid `pInitialData`, and
///   the real driver cannot be asked to cut one -- the live test's header says what it does;
/// * a terabyte of claimed room is only a bound on a write into the guest's own buffer;
/// * a wild cache, a NULL `pDataSize` and an unmapped `pData` refuse by name;
/// * the bytes read out go back into `vkCreatePipelineCache` unchanged.
#[test]
fn a_pipeline_cache_is_read_by_the_two_call_idiom_and_a_short_buffer_gets_nothing() {
    let _serial = serialized();
    let up = up_to_a_device("cache-data");
    let create = up.f.resolve_device(up.get_proc, up.device, "vkCreatePipelineCache");
    let destroy = up.f.resolve_device(up.get_proc, up.device, "vkDestroyPipelineCache");
    // Through `vkGetInstanceProcAddr`, as the engine was measured reaching it.
    let get_data = up.f.resolve(up.entry_point, up.instance, "vkGetPipelineCacheData");
    let device_token = up.f.vulkan().device_handles()[0].1;

    let out = up.f.alloc(8);
    let info = up.f.pipeline_cache_info(&[]);
    assert_eq!(up.f.call(create, [up.device, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let cache = up.f.guest.read_u64(out as GuestAddr);
    let token = up.host.log().caches[0].0;
    let blob = fake_cache_blob();
    let whole = blob.len() as u64;

    // 1. `pData = NULL`: the size, and nothing else.
    let size_at = up.f.poisoned(8, 0x5A);
    let result = up.f.call(get_data, [up.device, cache, size_at, 0]).expect("the size");
    assert_eq!(result as i32, VK_SUCCESS);
    assert_eq!(up.f.guest.read_u64(size_at as GuestAddr), whole, "the whole blob's size");

    // 2. A buffer big enough: the whole blob, and nothing past it.
    let data_at = up.f.poisoned(blob.len() + 8, 0x5A);
    let result = up.f.call(get_data, [up.device, cache, size_at, data_at]).expect("the blob");
    assert_eq!(result as i32, VK_SUCCESS);
    assert_eq!(up.f.read_bytes(data_at, blob.len()), blob, "the driver's bytes, unchanged");
    assert_eq!(up.f.guest.read_u64(size_at as GuestAddr), whole);
    assert_eq!(
        up.f.guest.read_u64(data_at as GuestAddr + whole as usize),
        u64::from_le_bytes([0x5A; 8]),
        "nothing past the blob"
    );

    // 3. **A short buffer gets nothing, and `VK_INCOMPLETE`.** One byte short, and then room for
    // the header and a few bytes of the body -- a prefix a driver would read as a blob whose
    // entries end mid-way. Neither is written: not a byte, and `*pDataSize` says so.
    for room in [whole - 1, CACHE_HEADER_BYTES as u64 + 8] {
        let short_at = up.f.poisoned(blob.len(), 0x5A);
        up.f.guest.write_u64(size_at as GuestAddr, room);
        let result = up.f.call(get_data, [up.device, cache, size_at, short_at]).expect("short");
        assert_eq!(result as i32, VK_INCOMPLETE, "room for {room}: VK_INCOMPLETE, not VK_SUCCESS");
        assert_eq!(up.f.guest.read_u64(size_at as GuestAddr), 0, "room for {room}: none written");
        assert_eq!(
            up.f.read_bytes(short_at, blob.len()),
            vec![0x5A; blob.len()],
            "room for {room}: not one byte of a blob that would not load back"
        );
    }

    // 4. **The guest's `size_t` bounds only a write into its own buffer.** A terabyte of claimed
    // room gets the blob -- the host was asked for the blob and nothing else, every time.
    up.f.guest.write_u64(size_at as GuestAddr, 1 << 40);
    let result = up.f.call(get_data, [up.device, cache, size_at, data_at]).expect("huge");
    assert_eq!(result as i32, VK_SUCCESS);
    assert_eq!(up.f.guest.read_u64(size_at as GuestAddr), whole);
    let reads = up.host.log().cache_reads.clone();
    assert_eq!(reads.len(), 5, "one host read per guest call");
    assert!(reads.iter().all(|read| *read == (device_token, token)), "{reads:?}");

    // 5. Refusals by name, none of which writes anything.
    let before = up.host.log().cache_reads.len();
    let wild = cache + HANDLE_SLOT_BYTES as u64;
    let text = up.f.refusal(get_data, &[up.device, wild, size_at, 0]).to_string();
    assert!(text.contains("`VkPipelineCache`"), "it names the family: {text}");
    assert!(text.contains(&format!("{wild:#x}")), "and the handle: {text}");
    let text = up.f.refusal(get_data, &[up.device, cache, 0, data_at]).to_string();
    assert!(text.contains("pDataSize = NULL"), "{text}");
    assert_eq!(up.host.log().cache_reads.len(), before, "none of those reached the host");
    up.f.guest.write_u64(size_at as GuestAddr, whole);
    let error = up.f.refusal(get_data, &[up.device, cache, size_at, up.f.guest.unmapped as u64]);
    assert!(
        matches!(error, AbiError::BadPointer { argument: 3, .. }),
        "an unmapped `pData` is a refusal naming argument 3, not a host write: {error:?}"
    );
    assert_eq!(up.f.guest.read_u64(size_at as GuestAddr), whole, "and `*pDataSize` is untouched");

    // 6. **The round trip**: the bytes read out are the bytes the next `vkCreatePipelineCache`
    // hands the host, and reading that cache back gives them again.
    let saved = up.f.read_bytes(data_at, blob.len());
    let info = up.f.pipeline_cache_info(&saved);
    assert_eq!(up.f.call(create, [up.device, info, 0, out]).expect("reload") as i32, VK_SUCCESS);
    let reloaded = up.f.guest.read_u64(out as GuestAddr);
    assert_eq!(up.host.log().cache_initial_data.last(), Some(&blob), "pInitialData crossed whole");
    let again_at = up.f.poisoned(blob.len(), 0x5A);
    up.f.guest.write_u64(size_at as GuestAddr, whole);
    let result = up.f.call(get_data, [up.device, reloaded, size_at, again_at]).expect("again");
    assert_eq!(result as i32, VK_SUCCESS);
    assert_eq!(up.f.read_bytes(again_at, blob.len()), blob);

    // A destroyed cache's handle names nothing.
    up.f.call(destroy, [up.device, cache, 0, 0]).expect("destroy");
    let text = up.f.refusal(get_data, &[up.device, cache, size_at, 0]).to_string();
    assert!(text.contains("`VkPipelineCache`"), "{text}");
    up.f.call(destroy, [up.device, reloaded, 0, 0]).expect("destroy the reloaded one");
}

// ============================================================================= vkDestroyDevice

/// **A device is destroyed once, only after its children, and its handles -- the device's and its
/// queues' -- go with it.**
///
/// Measured: the engine's render thread tears its device down on `APP_CMD_TERM_WINDOW`, after
/// saving its pipeline cache, through the thunk `vkGetInstanceProcAddr` handed out. This test owns
/// the guest side:
///
/// * `VK_NULL_HANDLE` is the specified no-op, and reaches no host;
/// * a wild `VkDevice` and a guest allocator refuse, and reach no host;
/// * a device the host refuses -- here, over a live semaphore -- keeps its handle and its queue's,
///   and the refusal names the child;
/// * once the child is gone the host destroys the device **once**, and the guest's `VkDevice` and
///   `VkQueue` handles stop being handles: each is then a refusal naming it.
///
/// The host's own check of every child family is the live test's, against `GfxVulkanHost`.
#[test]
fn a_device_is_destroyed_once_after_its_children_and_its_queues_go_with_it() {
    let _serial = serialized();
    let up = up_to_a_device("destroy-device");
    // Through `vkGetInstanceProcAddr`, as the engine was measured reaching it.
    let destroy = up.f.resolve(up.entry_point, up.instance, "vkDestroyDevice");
    let create_semaphore = up.f.resolve_device(up.get_proc, up.device, "vkCreateSemaphore");
    let destroy_semaphore = up.f.resolve_device(up.get_proc, up.device, "vkDestroySemaphore");
    let wait_idle = up.f.resolve_device(up.get_proc, up.device, "vkQueueWaitIdle");
    let get_queue = up.f.resolve(up.entry_point, up.instance, "vkGetDeviceQueue");
    assert_eq!(up.f.vulkan().device_handles().len(), 1);
    assert_eq!(up.f.vulkan().queue_handles().len(), 1);

    // `VK_NULL_HANDLE`: the specified no-op.
    up.f.call(destroy, [0, 0, 0, 0]).expect("a null destroy is a no-op");
    // A handle this layer did not issue, and a guest allocator.
    let wild = up.device + HANDLE_SLOT_BYTES as u64;
    let text = up.f.refusal(destroy, &[wild, 0]).to_string();
    assert!(text.contains("`VkDevice`"), "it names the family: {text}");
    assert!(text.contains(&format!("{wild:#x}")), "and the handle: {text}");
    let text = up.f.refusal(destroy, &[up.device, 0x1000]).to_string();
    assert!(text.contains("pAllocator"), "{text}");
    assert!(up.host.log().devices_destroyed.is_empty(), "none of the three reached a host destroy");

    // **A live child: refused, naming it, and nothing is taken back.**
    let out = up.f.alloc(8);
    let info = up.f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
    up.f.call(create_semaphore, [up.device, info, 0, out]).expect("a semaphore");
    let semaphore = up.f.guest.read_u64(out as GuestAddr);
    let text = up.f.refusal(destroy, &[up.device, 0]).to_string();
    assert!(text.contains("VkSemaphore"), "the refusal names the live child: {text}");
    assert!(up.host.log().devices_destroyed.is_empty(), "the device was not destroyed");
    assert_eq!(up.f.vulkan().device_handles().len(), 1, "and its handle still names it");
    assert_eq!(up.f.vulkan().queue_handles().len(), 1, "and so does its queue's");
    up.f.call(wait_idle, [up.queue, 0, 0, 0]).expect("the queue is still usable");

    // **The child first, then the device: destroyed once, and its handles go with it.**
    up.f.call(destroy_semaphore, [up.device, semaphore, 0, 0]).expect("destroy the semaphore");
    up.f.call(destroy, [up.device, 0, 0, 0]).expect("destroy the device");
    assert_eq!(up.host.log().devices_destroyed, vec![HostDevice::from_token(0)], "once");
    assert!(up.f.vulkan().device_handles().is_empty(), "the device's slot is free");
    assert!(up.f.vulkan().queue_handles().is_empty(), "and its queue's handle went with it");

    // **Stale handles name nothing.** A second destroy, a queue lookup and a queue wait.
    let text = up.f.refusal(destroy, &[up.device, 0]).to_string();
    assert!(text.contains("`VkDevice`"), "{text}");
    assert!(text.contains("0 live one(s)"), "{text}");
    assert_eq!(up.host.log().devices_destroyed.len(), 1, "the host was asked once, not twice");
    let queue_at = up.f.alloc(8);
    let text = up.f.refusal(get_queue, &[up.device, 0, 0, queue_at]).to_string();
    assert!(text.contains("`VkDevice`"), "{text}");
    let text = up.f.refusal(wait_idle, &[up.queue, 0, 0, 0]).to_string();
    assert!(text.contains("`VkQueue`"), "a destroyed device's queue names nothing: {text}");
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

    // **The engine's own batch**, MEASURED: 135 image barriers in one call, every one of which
    // reaches the host -- and one past the bound is refused by its count, not truncated to it.
    let batch: Vec<u8> = (0..135).flat_map(|_| image_barrier.clone()).collect();
    let batch_at = up.f.bytes(&batch);
    let stages = [u64::from(STAGE_TOP_OF_PIPE), u64::from(STAGE_TRANSFER)];
    up.f.call_n(barrier, &[command, stages[0], stages[1], 0, 0, 0, 0, 0, 135, batch_at])
        .expect("the engine's batch records");
    assert_eq!(up.host.log().barriers[1].image_barriers.len(), 135, "all of it");
    let past = (MAX_BARRIERS + 1) as u64;
    let text = up
        .f
        .refusal(barrier, &[command, stages[0], stages[1], 0, 0, 0, 0, 0, past, batch_at])
        .to_string();
    assert!(text.contains(&format!("imageMemoryBarrierCount = {past}")), "{text}");
    assert_eq!(up.host.log().barriers.len(), 2, "the refused one recorded nothing");

    // **A buffer memory barrier still refuses, and the reason changed in stage 5.** Stage 4
    // refused because there was no `VkBuffer` registry to resolve the handle through; there is
    // one now, and what holds instead is D17 — nothing in this stage's path records one. The
    // assertion is kept and rewritten rather than deleted, because a refusal whose *reason* has
    // gone stale is the kind that stops meaning anything.
    let text = up.f.refusal(barrier, &[command, 1, 1, 0, 0, 0, 1, 0x9000, 0, 0]).to_string();
    assert!(text.contains("bufferMemoryBarrierCount = 1"), "{text}");
    assert!(text.contains("`VkBuffer` registry now"), "the old reason is gone: {text}");
    assert!(text.contains("D17"), "and the one that holds is named: {text}");
    assert!(
        text.contains("Dropping the barrier is not the alternative"),
        "and dropping it is still refused: {text}"
    );

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

/// **The engine's measured teardown against the real driver: `vkDestroySurfaceKHR` refuses by
/// name while any swapchain over the surface lives -- a retired one included -- and through
/// another instance, and then the driver's surface is gone from the host.**
///
/// A gate run measured the engine's render thread answering `APP_CMD_TERM_WINDOW` with
/// `vkDestroySwapchainKHR` and then `vkDestroySurfaceKHR(instance, surface, NULL)`. This test
/// makes that call through the guest path. It is live because both ordering rules are the
/// **host's** to check -- the guest-side registries record neither which instance a surface came
/// from nor which surface a swapchain was made over -- and because "the surface is gone" is a
/// statement about `GfxVulkanHost`'s own table, which a double does not have.
#[test]
#[ignore = "opens a window and the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_surface_is_destroyed_through_the_guest_only_after_its_swapchains_and_leaves_the_host() {
    require_gate();
    let _serial = serialized();

    let mut window = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
        "Omnidroid — Vulkan: the guest destroys its surface",
        800,
        450,
    ))
    .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    let _ = window.poll_events().count();
    let source = HostWindowSource::watching(&window).expect("a source watching the window");
    let size = window.client_size().expect("the window has a client area");

    let host = omni_gfx::GfxVulkanHost::load().expect("this machine must have a Vulkan loader");
    let f = fixture("live-destroy-surface", Some(host.clone()));
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let surface = f.a_surface(entry_point, instance);
    let token = f.vulkan().surface_handles()[0].1;
    assert_eq!(host.objects().0, 1, "the driver made one surface");
    // Through `vkGetInstanceProcAddr`, as the engine was measured reaching it: the real driver
    // has the command, so this is a guest thunk and not the driver's NULL.
    let destroy_surface = f.resolve(entry_point, instance, "vkDestroySurfaceKHR");

    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    f.call(enumerate, [instance, count_at, 0, 0]).expect("count");
    let devices_at = f.alloc(8);
    f.call(enumerate, [instance, count_at, devices_at, 0]).expect("array");
    let physical = f.guest.read_u64(devices_at as GuestAddr);
    let logical = f.a_device(entry_point, instance, physical, 0);
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");
    let create_swapchain = f.resolve_device(get_proc, logical, "vkCreateSwapchainKHR");
    let destroy_swapchain = f.resolve_device(get_proc, logical, "vkDestroySwapchainKHR");

    // A swapchain over the surface, then its replacement: the first is **retired**, not destroyed.
    let info = f.swapchain_info(surface, 2, FORMAT_B8G8R8A8_UNORM, size.0, size.1, 0x10, 1, 0);
    let out = f.alloc(8);
    let result = f.call(create_swapchain, [logical, info, 0, out]).expect("the call completes");
    assert_eq!(result as i32, VK_SUCCESS, "the driver answered VkResult {}", result as i32);
    let retired = f.guest.read_u64(out as GuestAddr);
    let replacing =
        f.swapchain_info(surface, 2, FORMAT_B8G8R8A8_UNORM, size.0, size.1, 0x10, 1, retired);
    let out = f.alloc(8);
    let result =
        f.call(create_swapchain, [logical, replacing, 0, out]).expect("the call completes");
    assert_eq!(result as i32, VK_SUCCESS, "the driver answered VkResult {}", result as i32);
    let replacement = f.guest.read_u64(out as GuestAddr);

    // **Refused while both live**, naming the surface and the swapchains over it.
    let text = f.refusal(destroy_surface, &[instance, surface, 0, 0]).to_string();
    assert!(text.contains(&format!("{token:?}")), "it names the surface: {text}");
    assert!(text.contains("2 swapchain(s)"), "{text}");
    assert!(text.contains("HostSwapchain(#"), "it names the swapchains: {text}");
    assert_eq!(host.objects().0, 1, "the surface is untouched");
    assert_eq!(f.vulkan().surface_handles().len(), 1, "and the guest's handle still names it");
    eprintln!("\n=== vkDestroySurfaceKHR evidence ===");
    eprintln!("with two swapchains over the surface, refused by name:\n  {text}");

    // **The retired one still counts**: it no longer owns the window, but it was created over this
    // surface and the guest still owes its destroy.
    f.call(destroy_swapchain, [logical, replacement, 0, 0]).expect("destroy the replacement");
    let text = f.refusal(destroy_surface, &[instance, surface, 0, 0]).to_string();
    assert!(text.contains("1 swapchain(s)"), "{text}");
    assert!(text.contains("retired one included"), "{text}");
    assert_eq!(host.objects().0, 1);
    f.call(destroy_swapchain, [logical, retired, 0, 0]).expect("destroy the retired one");
    assert_eq!(host.stage_four_objects().swapchains, 0);

    // **Through another instance: refused naming both.** Both handles are real; the pairing is
    // not, and there is no validation layer on this machine to say so.
    let other = f.an_instance(entry_point);
    let text = f.refusal(destroy_surface, &[other, surface, 0, 0]).to_string();
    assert!(text.contains("created from HostInstance(#0)"), "{text}");
    assert!(text.contains("through HostInstance(#1)"), "{text}");
    assert_eq!(host.objects().0, 1);
    eprintln!("through another instance, refused by name:\n  {text}");

    // `VK_NULL_HANDLE` is the specified no-op against a real host too.
    f.call(destroy_surface, [instance, 0, 0, 0]).expect("a null destroy is a no-op");
    assert_eq!(host.objects().0, 1);

    // **The destroy, and the host's surface is gone.**
    f.call(destroy_surface, [instance, surface, 0, 0]).expect("destroy the surface");
    assert_eq!(host.objects().0, 0, "the driver's surface has left the host's table");
    assert!(f.vulkan().surface_handles().is_empty(), "and the guest's slot is free");
    eprintln!(
        "vkDestroySurfaceKHR({surface:#x}) -> the host holds {} surface(s)",
        host.objects().0
    );

    // A second destroy is refused by the guest registry, and the host would refuse the stale
    // token itself -- its slot's generation moved on -- rather than destroy anything twice.
    let text = f.refusal(destroy_surface, &[instance, surface, 0, 0]).to_string();
    assert!(text.contains("`VkSurfaceKHR`"), "{text}");
    let stale = host.destroy_surface(HostInstance::from_token(0), token).expect_err("stale");
    assert!(stale.to_string().contains("already destroyed"), "{stale}");

    // The next `onSurfaceCreated`: a new surface, in the same host slot under a new token, so the
    // stale one still names nothing.
    let again = f.a_surface(entry_point, instance);
    let fresh = f.vulkan().surface_handles()[0].1;
    assert_ne!(fresh, token, "a reused slot answers to a new token");
    assert!(host.destroy_surface(HostInstance::from_token(0), token).is_err(), "still stale");
    assert_eq!(host.objects().0, 1);
    f.call(destroy_surface, [instance, again, 0, 0]).expect("destroy the second surface");
    assert_eq!(host.objects().0, 0);
    eprintln!("a second surface ({fresh:?}, was {token:?}) was created and destroyed the same way");
}

// ============================================== stage 5: the structures a textured draw needs

/// `VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO`.
const STYPE_SHADER_MODULE_CREATE_INFO: u32 = 16;
/// `VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO`.
const STYPE_PIPELINE_CACHE_CREATE_INFO: u32 = 17;
/// `VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO`.
const STYPE_GRAPHICS_PIPELINE_CREATE_INFO: u32 = 28;
/// `VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO`.
const STYPE_PIPELINE_LAYOUT_CREATE_INFO: u32 = 30;
/// `VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO`.
const STYPE_SAMPLER_CREATE_INFO: u32 = 31;
/// `VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO`.
const STYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO: u32 = 32;
/// `VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO`.
const STYPE_DESCRIPTOR_POOL_CREATE_INFO: u32 = 33;
/// `VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO`.
const STYPE_DESCRIPTOR_SET_ALLOCATE_INFO: u32 = 34;
/// `VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET`.
const STYPE_WRITE_DESCRIPTOR_SET: u32 = 35;
/// `VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO`.
const STYPE_FRAMEBUFFER_CREATE_INFO: u32 = 37;
/// `VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO`.
const STYPE_RENDER_PASS_CREATE_INFO: u32 = 38;
/// `VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO`.
const STYPE_RENDER_PASS_BEGIN_INFO: u32 = 43;
/// `VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO`.
const STYPE_MEMORY_ALLOCATE_INFO: u32 = 5;
/// `VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO`.
const STYPE_BUFFER_CREATE_INFO: u32 = 12;
/// `VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO`.
const STYPE_IMAGE_CREATE_INFO: u32 = 14;

/// `VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT`.
const MEMORY_DEVICE_LOCAL: u32 = 0x1;
/// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`.
const MEMORY_HOST_VISIBLE: u32 = 0x2;
/// `VK_MEMORY_PROPERTY_HOST_COHERENT_BIT`.
const MEMORY_HOST_COHERENT: u32 = 0x4;

/// `VK_BUFFER_USAGE_TRANSFER_SRC_BIT`.
const BUFFER_USAGE_TRANSFER_SRC: u32 = 0x1;
/// `VK_BUFFER_USAGE_VERTEX_BUFFER_BIT`.
const BUFFER_USAGE_VERTEX: u32 = 0x80;
/// `VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT`.
const IMAGE_USAGE_TEXTURE: u32 = 0x2 | 0x4;
/// `VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL`.
const LAYOUT_SHADER_READ_ONLY: u32 = 5;
/// `VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL`.
const LAYOUT_COLOR_ATTACHMENT: u32 = 2;
/// `VK_FORMAT_R8G8B8A8_UNORM` for the texture, and `VK_FORMAT_R32G32_SFLOAT` for a `vec2`.
const FORMAT_R32G32_SFLOAT: u32 = 103;
/// `VK_ACCESS_SHADER_READ_BIT`.
const ACCESS_SHADER_READ: u32 = 0x20;
/// `VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT`.
const ACCESS_COLOR_ATTACHMENT_WRITE: u32 = 0x100;
/// `VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT`.
const STAGE_FRAGMENT_SHADER: u32 = 0x80;
/// `VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT`.
const STAGE_COLOR_ATTACHMENT_OUTPUT: u32 = 0x400;
/// `VK_SHADER_STAGE_VERTEX_BIT` and `VK_SHADER_STAGE_FRAGMENT_BIT`.
const SHADER_STAGE_VERTEX: u32 = 0x1;
/// `VK_SHADER_STAGE_FRAGMENT_BIT`.
const SHADER_STAGE_FRAGMENT: u32 = 0x10;
/// `VK_SHADER_STAGE_COMPUTE_BIT`.
const SHADER_STAGE_COMPUTE: u32 = 0x20;
/// `VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER`.
const DESCRIPTOR_COMBINED_IMAGE_SAMPLER: u32 = 1;
/// `VK_PIPELINE_BIND_POINT_GRAPHICS`.
const BIND_POINT_GRAPHICS: u32 = 0;
/// `VK_DYNAMIC_STATE_VIEWPORT` and `VK_DYNAMIC_STATE_SCISSOR`.
const DYNAMIC_STATE_VIEWPORT: u32 = 0;
/// `VK_DYNAMIC_STATE_SCISSOR`.
const DYNAMIC_STATE_SCISSOR: u32 = 1;
/// `VK_ATTACHMENT_LOAD_OP_CLEAR`, `VK_ATTACHMENT_STORE_OP_STORE`,
/// `VK_ATTACHMENT_LOAD_OP_DONT_CARE`, `VK_ATTACHMENT_STORE_OP_DONT_CARE`.
const LOAD_OP_CLEAR: u32 = 1;
/// `VK_ATTACHMENT_STORE_OP_STORE`.
const STORE_OP_STORE: u32 = 0;
/// `VK_ATTACHMENT_LOAD_OP_DONT_CARE`.
const LOAD_OP_DONT_CARE: u32 = 2;
/// `VK_ATTACHMENT_STORE_OP_DONT_CARE`.
const STORE_OP_DONT_CARE: u32 = 1;
/// `VK_SUBPASS_EXTERNAL`.
const SUBPASS_EXTERNAL: u32 = 0xFFFF_FFFF;
/// `VK_SAMPLE_COUNT_1_BIT`.
const SAMPLE_COUNT_1: u32 = 1;
/// `VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST`.
const TOPOLOGY_TRIANGLE_LIST: u32 = 3;
/// `VK_COLOR_COMPONENT_R|G|B|A_BIT`.
const COLOR_COMPONENT_RGBA: u32 = 0xF;
/// `VK_SUBPASS_CONTENTS_INLINE`.
const SUBPASS_CONTENTS_INLINE: u32 = 0;
/// `VK_FILTER_NEAREST` and `VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE`.
const FILTER_NEAREST: u32 = 0;
/// `VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE`.
const ADDRESS_MODE_CLAMP_TO_EDGE: u32 = 2;

/// **The four texels of the 2×2 texture the triangle samples, and the whole of what the live
/// test asserts.**
///
/// Four colours, all different from each other, none equal to the render pass's clear colour, and
/// none of them 0 or 0xFF in more than one channel. That combination is what makes the assertion
/// unforgeable in three separate ways at once:
///
/// * a frame that was **not drawn** shows the clear colour, which is none of these;
/// * a texture that was **not uploaded** shows whatever the driver left in the image, which would
///   have to be these four exact colours in these four exact places by coincidence;
/// * a **UV mapping that is flipped or transposed** shows these four colours in the wrong
///   quadrants, which a single-colour texture could never catch.
///
/// Row-major, so index 0 is the top-left texel and index 3 the bottom-right — and because the
/// fullscreen triangle maps `u` and `v` across the whole viewport with `OriginUpperLeft`, that is
/// also the order the four screen quadrants must show them in.
const TEXELS: [[u8; 4]; 4] = [
    [32, 96, 160, 255],   // top-left
    [16, 176, 64, 255],   // top-right
    [200, 48, 16, 255],   // bottom-left
    [240, 224, 80, 255],  // bottom-right
];

/// What the render pass clears to before the draw. **Deliberately none of [`TEXELS`]**, so that a
/// frame that cleared and did not draw fails the assertion rather than passing it.
const DRAW_CLEAR_COLOUR: [f32; 4] = [0.4, 0.0, 0.4, 1.0];
/// [`DRAW_CLEAR_COLOUR`] as the eight-bit values a `UNORM` swapchain stores.
const DRAW_CLEAR_BYTES: [u8; 4] = [102, 0, 102, 255];

/// **The fullscreen triangle, as the sixteen bytes per vertex the pipeline declares.**
///
/// `(-1,-1), (3,-1), (-1,3)` with `(0,0), (2,0), (0,2)`: the standard oversized triangle, whose
/// intersection with the `[-1,1]²` clip rectangle is the whole viewport and across which `u` and
/// `v` interpolate to exactly `0..1` over the visible part — at `x = 1`, the right edge, `t` is
/// `(1 − −1)/(3 − −1) = 0.5` and `u` is `0 + 0.5 × 2 = 1`.
///
/// One triangle rather than two, because the evidence this stage owes is *a triangle*, and a
/// fullscreen one is the shape that lets four quadrants be asserted from three vertices.
fn triangle_vertices() -> Vec<u8> {
    let vertices: [[f32; 4]; 3] =
        [[-1.0, -1.0, 0.0, 0.0], [3.0, -1.0, 2.0, 0.0], [-1.0, 3.0, 0.0, 2.0]];
    vertices.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect()
}

impl Fixture {
    // ------------------------------------------------------- stage 5's structures, in memory

    /// A `VkMemoryAllocateInfo`.
    fn memory_allocate_info(&self, size: u64, type_index: u32) -> u64 {
        let mut bytes = vec![0u8; MEMORY_ALLOCATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_MEMORY_ALLOCATE_INFO.to_le_bytes());
        bytes[16..24].copy_from_slice(&size.to_le_bytes());
        bytes[24..28].copy_from_slice(&type_index.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkBufferCreateInfo` with `VK_SHARING_MODE_EXCLUSIVE`.
    fn buffer_info(&self, size: u64, usage: u32) -> u64 {
        let mut bytes = vec![0u8; BUFFER_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_BUFFER_CREATE_INFO.to_le_bytes());
        bytes[24..32].copy_from_slice(&size.to_le_bytes());
        bytes[32..36].copy_from_slice(&usage.to_le_bytes());
        bytes[36..40].copy_from_slice(&SHARING_EXCLUSIVE.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkImageCreateInfo` for a 2D, single-mip, single-layer, optimally tiled texture.
    fn image_info(&self, width: u32, height: u32, format: u32, usage: u32) -> u64 {
        let mut bytes = vec![0u8; IMAGE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_IMAGE_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // VK_IMAGE_TYPE_2D
        bytes[24..28].copy_from_slice(&format.to_le_bytes());
        bytes[28..32].copy_from_slice(&width.to_le_bytes());
        bytes[32..36].copy_from_slice(&height.to_le_bytes());
        bytes[36..40].copy_from_slice(&1u32.to_le_bytes()); // depth
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes()); // mipLevels
        bytes[44..48].copy_from_slice(&1u32.to_le_bytes()); // arrayLayers
        bytes[48..52].copy_from_slice(&SAMPLE_COUNT_1.to_le_bytes());
        bytes[52..56].copy_from_slice(&0u32.to_le_bytes()); // VK_IMAGE_TILING_OPTIMAL
        bytes[56..60].copy_from_slice(&usage.to_le_bytes());
        bytes[60..64].copy_from_slice(&SHARING_EXCLUSIVE.to_le_bytes());
        bytes[80..84].copy_from_slice(&LAYOUT_UNDEFINED.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkPipelineCacheCreateInfo`, with `initial` as `pInitialData` when it is not empty.
    fn pipeline_cache_info(&self, initial: &[u8]) -> u64 {
        let mut bytes = vec![0u8; PIPELINE_CACHE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_PIPELINE_CACHE_CREATE_INFO.to_le_bytes());
        if !initial.is_empty() {
            let data = self.bytes(initial);
            bytes[24..32].copy_from_slice(&(initial.len() as u64).to_le_bytes());
            bytes[32..40].copy_from_slice(&data.to_le_bytes());
        }
        self.bytes(&bytes)
    }

    /// A `VkSamplerCreateInfo`: nearest, clamped, no anisotropy, no comparison.
    ///
    /// **Nearest and clamped on purpose.** Linear filtering between two of [`TEXELS`] would make
    /// the asserted pixel a blend whose exact value depends on the driver's rounding, and the
    /// whole point of the assertion is that it is exact.
    fn sampler_info(&self) -> u64 {
        let mut bytes = vec![0u8; SAMPLER_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_SAMPLER_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&FILTER_NEAREST.to_le_bytes()); // magFilter
        bytes[24..28].copy_from_slice(&FILTER_NEAREST.to_le_bytes()); // minFilter
        bytes[32..36].copy_from_slice(&ADDRESS_MODE_CLAMP_TO_EDGE.to_le_bytes()); // U
        bytes[36..40].copy_from_slice(&ADDRESS_MODE_CLAMP_TO_EDGE.to_le_bytes()); // V
        bytes[40..44].copy_from_slice(&ADDRESS_MODE_CLAMP_TO_EDGE.to_le_bytes()); // W
        self.bytes(&bytes)
    }

    /// A `VkShaderModuleCreateInfo` over SPIR-V words already written into guest memory.
    fn shader_module_info(&self, code: &[u32]) -> u64 {
        let words: Vec<u8> = code.iter().flat_map(|word| word.to_le_bytes()).collect();
        let code_at = self.bytes(&words);
        let mut bytes = vec![0u8; SHADER_MODULE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_SHADER_MODULE_CREATE_INFO.to_le_bytes());
        bytes[24..32].copy_from_slice(&(words.len() as u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&code_at.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkDescriptorSetLayoutCreateInfo` with one combined image sampler at binding 0.
    fn descriptor_set_layout_info(&self) -> u64 {
        let mut binding = vec![0u8; DESCRIPTOR_SET_LAYOUT_BINDING_BYTES];
        binding[0..4].copy_from_slice(&0u32.to_le_bytes()); // binding
        binding[4..8].copy_from_slice(&DESCRIPTOR_COMBINED_IMAGE_SAMPLER.to_le_bytes());
        binding[8..12].copy_from_slice(&1u32.to_le_bytes()); // descriptorCount
        binding[12..16].copy_from_slice(&SHADER_STAGE_FRAGMENT.to_le_bytes());
        let bindings = self.bytes(&binding);

        let mut bytes = vec![0u8; DESCRIPTOR_SET_LAYOUT_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&bindings.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkDescriptorPoolCreateInfo` for one set with one combined image sampler.
    fn descriptor_pool_info(&self) -> u64 {
        let mut size = vec![0u8; DESCRIPTOR_POOL_SIZE_BYTES];
        size[0..4].copy_from_slice(&DESCRIPTOR_COMBINED_IMAGE_SAMPLER.to_le_bytes());
        size[4..8].copy_from_slice(&1u32.to_le_bytes());
        let sizes = self.bytes(&size);

        let mut bytes = vec![0u8; DESCRIPTOR_POOL_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_DESCRIPTOR_POOL_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // maxSets
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes()); // poolSizeCount
        bytes[32..40].copy_from_slice(&sizes.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkDescriptorSetAllocateInfo` for one set of one layout.
    fn descriptor_set_allocate_info(&self, pool: u64, layout: u64) -> u64 {
        let layouts = self.u64_array(&[layout]);
        let mut bytes = vec![0u8; DESCRIPTOR_SET_ALLOCATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_DESCRIPTOR_SET_ALLOCATE_INFO.to_le_bytes());
        bytes[16..24].copy_from_slice(&pool.to_le_bytes());
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&layouts.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkWriteDescriptorSet` for one combined image sampler.
    fn write_descriptor_set(&self, set: u64, sampler: u64, view: u64) -> u64 {
        let mut image_info = vec![0u8; DESCRIPTOR_IMAGE_INFO_BYTES];
        image_info[0..8].copy_from_slice(&sampler.to_le_bytes());
        image_info[8..16].copy_from_slice(&view.to_le_bytes());
        image_info[16..20].copy_from_slice(&LAYOUT_SHADER_READ_ONLY.to_le_bytes());
        let images = self.bytes(&image_info);

        let mut bytes = vec![0u8; WRITE_DESCRIPTOR_SET_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_WRITE_DESCRIPTOR_SET.to_le_bytes());
        bytes[16..24].copy_from_slice(&set.to_le_bytes());
        bytes[24..28].copy_from_slice(&0u32.to_le_bytes()); // dstBinding
        bytes[28..32].copy_from_slice(&0u32.to_le_bytes()); // dstArrayElement
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes()); // descriptorCount
        bytes[36..40].copy_from_slice(&DESCRIPTOR_COMBINED_IMAGE_SAMPLER.to_le_bytes());
        bytes[40..48].copy_from_slice(&images.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkComputePipelineCreateInfo` as the engine writes one: the stage **embedded** at 24 --
    /// `sType` 24, `stage` 44, `module` 48, `pName` 56 -- `layout` 72, no base pipeline, and a
    /// `basePipelineIndex` of -1 at 88.
    fn compute_pipeline_info(&self, layout: u64, module: u64, stage: u32, flags: u32) -> u64 {
        let entry = self.cstr("main");
        let mut bytes = vec![0u8; COMPUTE_PIPELINE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&29u32.to_le_bytes()); // COMPUTE_PIPELINE_CREATE_INFO
        bytes[16..20].copy_from_slice(&flags.to_le_bytes());
        bytes[24..28].copy_from_slice(&18u32.to_le_bytes()); // PIPELINE_SHADER_STAGE_CREATE_INFO
        bytes[44..48].copy_from_slice(&stage.to_le_bytes());
        bytes[48..56].copy_from_slice(&module.to_le_bytes());
        bytes[56..64].copy_from_slice(&entry.to_le_bytes());
        bytes[72..80].copy_from_slice(&layout.to_le_bytes());
        bytes[88..92].copy_from_slice(&(-1i32).to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkPipelineLayoutCreateInfo` with one set layout and no push constants.
    fn pipeline_layout_info(&self, set_layout: u64) -> u64 {
        let layouts = self.u64_array(&[set_layout]);
        let mut bytes = vec![0u8; PIPELINE_LAYOUT_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_PIPELINE_LAYOUT_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&layouts.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkRenderPassCreateInfo` with one colour attachment that is cleared and then presented.
    fn render_pass_info(&self, format: u32) -> u64 {
        let mut attachment = vec![0u8; ATTACHMENT_DESCRIPTION_BYTES];
        attachment[4..8].copy_from_slice(&format.to_le_bytes());
        attachment[8..12].copy_from_slice(&SAMPLE_COUNT_1.to_le_bytes());
        attachment[12..16].copy_from_slice(&LOAD_OP_CLEAR.to_le_bytes());
        attachment[16..20].copy_from_slice(&STORE_OP_STORE.to_le_bytes());
        attachment[20..24].copy_from_slice(&LOAD_OP_DONT_CARE.to_le_bytes()); // stencilLoadOp
        attachment[24..28].copy_from_slice(&STORE_OP_DONT_CARE.to_le_bytes()); // stencilStoreOp
        attachment[28..32].copy_from_slice(&LAYOUT_UNDEFINED.to_le_bytes());
        attachment[32..36].copy_from_slice(&LAYOUT_PRESENT_SRC.to_le_bytes());
        let attachments = self.bytes(&attachment);

        let mut reference = vec![0u8; ATTACHMENT_REFERENCE_BYTES];
        reference[0..4].copy_from_slice(&0u32.to_le_bytes()); // attachment 0
        reference[4..8].copy_from_slice(&LAYOUT_COLOR_ATTACHMENT.to_le_bytes());
        let references = self.bytes(&reference);

        let mut subpass = vec![0u8; SUBPASS_DESCRIPTION_BYTES];
        subpass[4..8].copy_from_slice(&BIND_POINT_GRAPHICS.to_le_bytes());
        subpass[24..28].copy_from_slice(&1u32.to_le_bytes()); // colorAttachmentCount
        subpass[32..40].copy_from_slice(&references.to_le_bytes());
        let subpasses = self.bytes(&subpass);

        // One external dependency, which is what orders the colour write against the acquire.
        let mut dependency = vec![0u8; SUBPASS_DEPENDENCY_BYTES];
        dependency[0..4].copy_from_slice(&SUBPASS_EXTERNAL.to_le_bytes());
        dependency[4..8].copy_from_slice(&0u32.to_le_bytes());
        dependency[8..12].copy_from_slice(&STAGE_COLOR_ATTACHMENT_OUTPUT.to_le_bytes());
        dependency[12..16].copy_from_slice(&STAGE_COLOR_ATTACHMENT_OUTPUT.to_le_bytes());
        dependency[16..20].copy_from_slice(&0u32.to_le_bytes());
        dependency[20..24].copy_from_slice(&ACCESS_COLOR_ATTACHMENT_WRITE.to_le_bytes());
        let dependencies = self.bytes(&dependency);

        let mut bytes = vec![0u8; RENDER_PASS_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_RENDER_PASS_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&attachments.to_le_bytes());
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&subpasses.to_le_bytes());
        bytes[48..52].copy_from_slice(&1u32.to_le_bytes());
        bytes[56..64].copy_from_slice(&dependencies.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkFramebufferCreateInfo` over one view.
    fn framebuffer_info(&self, pass: u64, view: u64, width: u32, height: u32) -> u64 {
        let attachments = self.u64_array(&[view]);
        let mut bytes = vec![0u8; FRAMEBUFFER_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_FRAMEBUFFER_CREATE_INFO.to_le_bytes());
        bytes[24..32].copy_from_slice(&pass.to_le_bytes());
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&attachments.to_le_bytes());
        bytes[48..52].copy_from_slice(&width.to_le_bytes());
        bytes[52..56].copy_from_slice(&height.to_le_bytes());
        bytes[56..60].copy_from_slice(&1u32.to_le_bytes()); // layers
        self.bytes(&bytes)
    }

    /// A `VkRenderPassBeginInfo` covering the whole framebuffer, with one clear value.
    fn render_pass_begin_info(
        &self,
        pass: u64,
        framebuffer: u64,
        width: u32,
        height: u32,
        clear: [f32; 4],
    ) -> u64 {
        let clear_bytes: Vec<u8> = clear.iter().flat_map(|c| c.to_le_bytes()).collect();
        let clears = self.bytes(&clear_bytes);
        let mut bytes = vec![0u8; RENDER_PASS_BEGIN_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_RENDER_PASS_BEGIN_INFO.to_le_bytes());
        bytes[16..24].copy_from_slice(&pass.to_le_bytes());
        bytes[24..32].copy_from_slice(&framebuffer.to_le_bytes());
        // renderArea: offset (0,0), extent (width, height)
        bytes[40..44].copy_from_slice(&width.to_le_bytes());
        bytes[44..48].copy_from_slice(&height.to_le_bytes());
        bytes[48..52].copy_from_slice(&1u32.to_le_bytes()); // clearValueCount
        bytes[56..64].copy_from_slice(&clears.to_le_bytes());
        self.bytes(&bytes)
    }

    /// A `VkViewport` covering the whole framebuffer, and a `VkRect2D` that does the same.
    fn viewport_and_scissor(&self, width: u32, height: u32) -> (u64, u64) {
        let viewport: Vec<u8> = [0.0f32, 0.0, width as f32, height as f32, 0.0, 1.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let mut scissor = vec![0u8; RECT_2D_BYTES];
        scissor[8..12].copy_from_slice(&width.to_le_bytes());
        scissor[12..16].copy_from_slice(&height.to_le_bytes());
        (self.bytes(&viewport), self.bytes(&scissor))
    }

    /// A `VkBufferImageCopy` for the whole of a 2D, single-mip, single-layer image.
    fn buffer_image_copy(&self, width: u32, height: u32) -> u64 {
        let mut bytes = vec![0u8; BUFFER_IMAGE_COPY_BYTES];
        // bufferOffset 0, bufferRowLength 0 and bufferImageHeight 0 mean "tightly packed".
        bytes[16..20].copy_from_slice(&ASPECT_COLOR.to_le_bytes());
        bytes[20..24].copy_from_slice(&0u32.to_le_bytes()); // mipLevel
        bytes[24..28].copy_from_slice(&0u32.to_le_bytes()); // baseArrayLayer
        bytes[28..32].copy_from_slice(&1u32.to_le_bytes()); // layerCount
        bytes[44..48].copy_from_slice(&width.to_le_bytes());
        bytes[48..52].copy_from_slice(&height.to_le_bytes());
        bytes[52..56].copy_from_slice(&1u32.to_le_bytes()); // depth
        self.bytes(&bytes)
    }

    /// **The whole `VkGraphicsPipelineCreateInfo`, with all nine sub-states present.**
    ///
    /// The one that is deliberately *not* filled is `pViewports`/`pScissors`: the pipeline names
    /// `VK_DYNAMIC_STATE_VIEWPORT` and `_SCISSOR`, which is the case where the counts and the
    /// arrays legitimately disagree — `viewportCount` must still be 1 while the array is NULL.
    /// That is exactly the property `ViewportState` keeps the two apart for, and a shim that
    /// derived the count from the array would build a pipeline with no viewport at all.
    fn graphics_pipeline_info(
        &self,
        layout: u64,
        pass: u64,
        vertex_module: u64,
        fragment_module: u64,
    ) -> u64 {
        let entry = self.cstr("main");
        let mut stages = Vec::new();
        for (stage, module) in
            [(SHADER_STAGE_VERTEX, vertex_module), (SHADER_STAGE_FRAGMENT, fragment_module)]
        {
            let mut bytes = vec![0u8; PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES];
            bytes[0..4].copy_from_slice(&18u32.to_le_bytes()); // PIPELINE_SHADER_STAGE_CREATE_INFO
            bytes[20..24].copy_from_slice(&stage.to_le_bytes());
            bytes[24..32].copy_from_slice(&module.to_le_bytes());
            bytes[32..40].copy_from_slice(&entry.to_le_bytes());
            stages.extend_from_slice(&bytes);
        }
        let stages_at = self.bytes(&stages);

        // One binding of sixteen bytes: a `vec2` position and a `vec2` texture coordinate.
        let mut binding = vec![0u8; VERTEX_INPUT_BINDING_BYTES];
        binding[0..4].copy_from_slice(&0u32.to_le_bytes()); // binding
        binding[4..8].copy_from_slice(&16u32.to_le_bytes()); // stride
        binding[8..12].copy_from_slice(&0u32.to_le_bytes()); // VK_VERTEX_INPUT_RATE_VERTEX
        let bindings_at = self.bytes(&binding);

        let mut attributes = Vec::new();
        for (location, offset) in [(0u32, 0u32), (1, 8)] {
            let mut bytes = vec![0u8; VERTEX_INPUT_ATTRIBUTE_BYTES];
            bytes[0..4].copy_from_slice(&location.to_le_bytes());
            bytes[4..8].copy_from_slice(&0u32.to_le_bytes()); // binding
            bytes[8..12].copy_from_slice(&FORMAT_R32G32_SFLOAT.to_le_bytes());
            bytes[12..16].copy_from_slice(&offset.to_le_bytes());
            attributes.extend_from_slice(&bytes);
        }
        let attributes_at = self.bytes(&attributes);

        let mut vertex_input = vec![0u8; VERTEX_INPUT_STATE_BYTES];
        vertex_input[0..4].copy_from_slice(&19u32.to_le_bytes());
        vertex_input[20..24].copy_from_slice(&1u32.to_le_bytes());
        vertex_input[24..32].copy_from_slice(&bindings_at.to_le_bytes());
        vertex_input[32..36].copy_from_slice(&2u32.to_le_bytes());
        vertex_input[40..48].copy_from_slice(&attributes_at.to_le_bytes());
        let vertex_input_at = self.bytes(&vertex_input);

        let mut assembly = vec![0u8; INPUT_ASSEMBLY_STATE_BYTES];
        assembly[0..4].copy_from_slice(&20u32.to_le_bytes());
        assembly[20..24].copy_from_slice(&TOPOLOGY_TRIANGLE_LIST.to_le_bytes());
        let assembly_at = self.bytes(&assembly);

        // **Counts without arrays**, which is what the dynamic states below make legal.
        let mut viewport = vec![0u8; VIEWPORT_STATE_BYTES];
        viewport[0..4].copy_from_slice(&22u32.to_le_bytes());
        viewport[20..24].copy_from_slice(&1u32.to_le_bytes()); // viewportCount
        viewport[32..36].copy_from_slice(&1u32.to_le_bytes()); // scissorCount
        let viewport_at = self.bytes(&viewport);

        let mut rasterization = vec![0u8; RASTERIZATION_STATE_BYTES];
        rasterization[0..4].copy_from_slice(&23u32.to_le_bytes());
        rasterization[28..32].copy_from_slice(&0u32.to_le_bytes()); // VK_POLYGON_MODE_FILL
        rasterization[32..36].copy_from_slice(&0u32.to_le_bytes()); // VK_CULL_MODE_NONE
        rasterization[36..40].copy_from_slice(&0u32.to_le_bytes()); // COUNTER_CLOCKWISE
        rasterization[56..60].copy_from_slice(&1.0f32.to_le_bytes()); // lineWidth
        let rasterization_at = self.bytes(&rasterization);

        let mut multisample = vec![0u8; MULTISAMPLE_STATE_BYTES];
        multisample[0..4].copy_from_slice(&24u32.to_le_bytes());
        multisample[20..24].copy_from_slice(&SAMPLE_COUNT_1.to_le_bytes());
        let multisample_at = self.bytes(&multisample);

        let mut blend_attachment = vec![0u8; COLOR_BLEND_ATTACHMENT_BYTES];
        blend_attachment[0..4].copy_from_slice(&0u32.to_le_bytes()); // blendEnable = VK_FALSE
        blend_attachment[28..32].copy_from_slice(&COLOR_COMPONENT_RGBA.to_le_bytes());
        let blend_attachments_at = self.bytes(&blend_attachment);

        let mut blend = vec![0u8; COLOR_BLEND_STATE_BYTES];
        blend[0..4].copy_from_slice(&26u32.to_le_bytes());
        blend[28..32].copy_from_slice(&1u32.to_le_bytes()); // attachmentCount
        blend[32..40].copy_from_slice(&blend_attachments_at.to_le_bytes());
        let blend_at = self.bytes(&blend);

        let states = self.u32_array(&[DYNAMIC_STATE_VIEWPORT, DYNAMIC_STATE_SCISSOR]);
        let mut dynamic = vec![0u8; DYNAMIC_STATE_CREATE_INFO_BYTES];
        dynamic[0..4].copy_from_slice(&27u32.to_le_bytes());
        dynamic[20..24].copy_from_slice(&2u32.to_le_bytes());
        dynamic[24..32].copy_from_slice(&states.to_le_bytes());
        let dynamic_at = self.bytes(&dynamic);

        let mut bytes = vec![0u8; GRAPHICS_PIPELINE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&STYPE_GRAPHICS_PIPELINE_CREATE_INFO.to_le_bytes());
        bytes[20..24].copy_from_slice(&2u32.to_le_bytes()); // stageCount
        bytes[24..32].copy_from_slice(&stages_at.to_le_bytes());
        bytes[32..40].copy_from_slice(&vertex_input_at.to_le_bytes());
        bytes[40..48].copy_from_slice(&assembly_at.to_le_bytes());
        // pTessellationState stays NULL, which is legal with no tessellation stages.
        bytes[56..64].copy_from_slice(&viewport_at.to_le_bytes());
        bytes[64..72].copy_from_slice(&rasterization_at.to_le_bytes());
        bytes[72..80].copy_from_slice(&multisample_at.to_le_bytes());
        // pDepthStencilState stays NULL: the render pass has no depth attachment.
        bytes[88..96].copy_from_slice(&blend_at.to_le_bytes());
        bytes[96..104].copy_from_slice(&dynamic_at.to_le_bytes());
        bytes[104..112].copy_from_slice(&layout.to_le_bytes());
        bytes[112..120].copy_from_slice(&pass.to_le_bytes());
        bytes[120..124].copy_from_slice(&0u32.to_le_bytes()); // subpass
        bytes[136..140].copy_from_slice(&(-1i32).to_le_bytes()); // basePipelineIndex
        self.bytes(&bytes)
    }

    /// The `memoryTypeIndex` of the first type that is in `type_bits` and has every bit of
    /// `required`, **read out of the list the guest was shown**.
    ///
    /// That last clause is the point: the list has been through
    /// `physical::mask_memory_types`, so a type whose host-visible bits this layer cleared is one
    /// this search will not find — which is how a conforming engine is steered away from a type
    /// `vkMapMemory` would have to refuse.
    fn memory_type_index(&self, properties: &[u8], type_bits: u32, required: u32) -> Option<u32> {
        let count = u32::from_le_bytes(properties[0..4].try_into().expect("four"));
        (0..count).find(|index| {
            let at = 4 + *index as usize * 8;
            let flags = u32::from_le_bytes(properties[at..at + 4].try_into().expect("four"));
            type_bits & (1 << index) != 0 && flags & required == required
        })
    }
}


// ===================================== stage 5 against the double: the four unmakeable cases
//
// The live test further up is the evidence that a textured triangle reaches the screen. These are
// the properties it **cannot** show, because no real driver can be made to produce them on demand.

/// Everything from a device to a mapped allocation, for the tests below.
struct UpToMemory {
    up: UpToADevice,
    allocate: u64,
    map: u64,
    create_buffer: u64,
}

fn up_to_memory(tag: &str) -> UpToMemory {
    let up = up_to_a_device(tag);
    let allocate = up.f.resolve_device(up.get_proc, up.device, "vkAllocateMemory");
    let map = up.f.resolve_device(up.get_proc, up.device, "vkMapMemory");
    let create_buffer = up.f.resolve_device(up.get_proc, up.device, "vkCreateBuffer");
    UpToMemory { up, allocate, map, create_buffer }
}

/// **The split: an importable type is backed by guest pages, a device-local one is not.**
///
/// The one decision the whole stage rests on, asserted from both sides in one test — because the
/// two are told apart by nothing the guest can see, and a shim that imported everything would
/// still pass every other test in this file.
///
/// The imported allocation's pages are then checked to be **inside `GuestSpace` and admissible**,
/// which is the property `docs/HANDOFF.md`'s `VK_EXT_external_memory_host` route was chosen for:
/// `admit` admits the address with zero changes to `omni-mem`.
#[test]
fn a_host_visible_allocation_is_imported_from_guest_pages_and_a_device_local_one_is_forwarded() {
    let _serial = serialized();
    let m = up_to_memory("memsplit");

    // Type 2 on this machine's table is `HOST_VISIBLE | HOST_COHERENT` and importable.
    let out = m.up.f.alloc(8);
    let info = m.up.f.memory_allocate_info(4096, 2);
    assert_eq!(
        m.up.f.call(m.allocate, [m.up.device, info, 0, out]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let imported_handle = m.up.f.guest.read_u64(out as GuestAddr);

    // Type 1 is `DEVICE_LOCAL` alone: nothing to import, nothing to map.
    let info = m.up.f.memory_allocate_info(8192, 1);
    assert_eq!(
        m.up.f.call(m.allocate, [m.up.device, info, 0, out]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let forwarded_handle = m.up.f.guest.read_u64(out as GuestAddr);
    assert_ne!(imported_handle, forwarded_handle);

    let allocations = m.up.host.log().allocations.clone();
    assert_eq!(allocations.len(), 2, "{allocations:?}");
    let imported = allocations[0].1;
    let forwarded = allocations[1].1;

    assert_eq!(imported.size, 4096, "the guest's own allocationSize travels unrounded");
    assert_eq!(imported.memory_type_index, 2);
    let pointer = imported.host_pointer.expect("a host-visible type is imported");
    assert_eq!(
        imported.import_length, 4096,
        "and the length the driver is given is the size rounded up to the alignment"
    );

    // **Inside `GuestSpace`, and `admit` admits it.** The whole of why this route was chosen.
    let space = &m.up.f.guest.space;
    assert!(
        pointer >= space.base() as u64 && pointer < space.end() as u64,
        "the imported pages must be the guest's own: {pointer:#x} against \
         [{:#x}, {:#x})",
        space.base(),
        space.end()
    );
    omni_mem::admit(space, pointer as GuestAddr, 4096, omni_mem::FaultAccess::Write)
        .expect("`admit` admits the imported range for writing, with no change to `omni-mem`");

    assert_eq!(forwarded.size, 8192);
    assert_eq!(forwarded.memory_type_index, 1);
    assert_eq!(
        forwarded.host_pointer, None,
        "a device-local allocation is an ordinary forward -- importing it would take guest commit \
         charge for memory the guest can never touch"
    );
    assert_eq!(forwarded.import_length, 0);

    // The commit charge is exactly the imported allocation's, which is what D15's ceiling now
    // covers.
    let (live, peak) = m.up.f.vulkan().imported_bytes();
    assert_eq!(live, 4096, "only the imported one is guest commit charge");
    assert_eq!(peak, 4096);

    // ---------------------------------------------------- `vkMapMemory` answers for one and not
    // the other.
    let mapped_at = m.up.f.alloc(8);
    assert_eq!(
        m.up
            .f
            .call_n(m.map, &[m.up.device, imported_handle, 0, VK_WHOLE_SIZE, 0, mapped_at])
            .expect("map") as i32,
        VK_SUCCESS
    );
    assert_eq!(
        m.up.f.guest.read_u64(mapped_at as GuestAddr),
        pointer,
        "`vkMapMemory` answers with the address that was imported"
    );
    assert_eq!(m.up.f.vulkan().mapped_bytes(), 4096);

    // An offset shifts the answer, and is bounded by the guest's own `allocationSize`.
    assert_eq!(
        m.up
            .f
            .call_n(m.map, &[m.up.device, imported_handle, 256, VK_WHOLE_SIZE, 0, mapped_at])
            .expect("map") as i32,
        VK_SUCCESS
    );
    assert_eq!(m.up.f.guest.read_u64(mapped_at as GuestAddr), pointer + 256);
    let text = m
        .up
        .f
        .refusal(m.map, &[m.up.device, imported_handle, 4097, VK_WHOLE_SIZE, 0, mapped_at])
        .to_string();
    assert!(text.contains("offset = 4097"), "{text}");
    assert!(text.contains("stores through without checking it"), "{text}");

    // **And the forwarded one refuses by name.**
    let text = m
        .up
        .f
        .refusal(m.map, &[m.up.device, forwarded_handle, 0, VK_WHOLE_SIZE, 0, mapped_at])
        .to_string();
    assert!(text.contains("**forwarded** rather than imported"), "{text}");
    assert!(text.contains("HOST_VISIBLE"), "it names why: {text}");
    assert!(
        text.contains("inside `GuestSpace`"),
        "and what it could not have produced instead: {text}"
    );

    // Freeing the imported one gives the guest pages back.
    let free = m.up.f.resolve_device(m.up.get_proc, m.up.device, "vkFreeMemory");
    m.up.f.call(free, [m.up.device, imported_handle, 0, 0]).expect("free");
    assert_eq!(m.up.f.vulkan().imported_bytes(), (0, 4096), "live falls, the peak does not");
    assert_eq!(m.up.f.vulkan().leaked_import_bytes(), 0);
}

/// **A host-visible type this layer cannot import into is refused by name, and the guest is never
/// shown it as host-visible in the first place.**
///
/// Both halves matter and they are different claims. The mask is a courtesy to a conforming
/// engine — it steers one away from a type whose `vkMapMemory` would have to be refused one call
/// later. The refusal is the boundary: a guest that names the type anyway, from a list it did not
/// read or from a number it computed, still cannot reach a `vkMapMemory` that has no address to
/// answer with.
#[test]
fn the_rebar_memory_type_is_masked_out_of_the_guests_list_and_refused_if_named_anyway() {
    let _serial = serialized();
    let m = up_to_memory("rebar");
    let entry_point = m.up.f.entry_point();

    // The one physical device this double has, through the guest's own query.
    let instance = m.up.f.vulkan().instance_handles()[0].0 as u64;
    let properties = m.up.f.resolve(entry_point, instance, "vkGetPhysicalDeviceMemoryProperties");
    let physical = m.up.f.vulkan().physical_device_handles()[0].0 as u64;
    let at = m.up.f.alloc(PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);
    m.up.f.call(properties, [physical, at, 0, 0]).expect("memory properties");
    let shown = m.up.f.read_bytes(at, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);

    let flags = |index: usize| {
        u32::from_le_bytes(shown[4 + index * 8..8 + index * 8].try_into().expect("four"))
    };
    assert_eq!(u32::from_le_bytes(shown[0..4].try_into().expect("four")), 5, "the count is kept");
    assert_eq!(flags(2), 0x6, "an importable type keeps its promise");
    assert_eq!(flags(3), 0xe, "and so does the cached one");
    assert_eq!(
        flags(4),
        0x1,
        "**the ReBAR type is shown as device-local only**: it is host-visible to the driver and \
         this layer cannot import into it, so the promise that the guest may map it is the one \
         thing that is removed. That it is device-local is still true and is still said"
    );

    // The rewrite is in the log, with both spellings, so nobody has to diff two tables to see it.
    let masked: Vec<_> = m
        .up
        .f
        .vulkan()
        .rewrites()
        .into_iter()
        .filter(|rewrite| matches!(rewrite.site, RewriteSite::MemoryType { .. }))
        .collect();
    assert_eq!(masked.len(), 1, "one type changed, one record: {masked:?}");
    assert_eq!(masked[0].site, RewriteSite::MemoryType { index: 4 });
    assert_eq!(masked[0].from, "DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT");
    assert_eq!(masked[0].to, "DEVICE_LOCAL");
    assert!(masked[0].spec_version.is_none(), "a memory type has no version");

    // **Recorded once per physical device, not once per call.** The log is a log of decisions;
    // an engine that asks per allocation would otherwise fill it with the same line.
    m.up.f.call(properties, [physical, at, 0, 0]).expect("again");
    m.up.f.call(properties, [physical, at, 0, 0]).expect("and again");
    let again = m
        .up
        .f
        .vulkan()
        .rewrites()
        .into_iter()
        .filter(|rewrite| matches!(rewrite.site, RewriteSite::MemoryType { .. }))
        .count();
    assert_eq!(again, 1, "three calls, one record");

    // And naming it anyway is a refusal that says what could not be produced.
    let out = m.up.f.alloc(8);
    let info = m.up.f.memory_allocate_info(4096, 4);
    let text = m.up.f.refusal(m.allocate, &[m.up.device, info, 0, out]).to_string();
    assert!(text.contains("memory type 4"), "{text}");
    assert!(text.contains("not importable**"), "{text}");
    assert!(text.contains("VK_EXT_external_memory_host"), "it names the route: {text}");
    assert!(text.contains("D4 amendment 1"), "and why a driver pointer is not the answer: {text}");
    assert!(
        m.up.f.vulkan().imported_bytes().0 == 0,
        "a refused allocation takes no guest commit charge"
    );
}

/// **A `pNext` chain is refused and the address is named, which turns "what does the engine send?"
/// into a measurement.**
///
/// The deliberate choice of this stage, stated where it is checked: chains are refused rather than
/// walked, because walking means knowing, and a structure this layer did not recognise would have
/// to be either dropped — producing an object that is not the one that was asked for — or
/// forwarded blind. `vkAllocateMemory` is the sharpest case, because this layer **constructs** the
/// one chain an allocation carries.
#[test]
fn a_guest_pnext_chain_is_refused_by_name_and_the_address_is_recorded() {
    let _serial = serialized();
    let m = up_to_memory("pnext");
    let out = m.up.f.alloc(8);

    let mut bytes = vec![0u8; MEMORY_ALLOCATE_INFO_BYTES];
    bytes[0..4].copy_from_slice(&5u32.to_le_bytes()); // MEMORY_ALLOCATE_INFO
    bytes[8..16].copy_from_slice(&0xDEAD_0000u64.to_le_bytes()); // pNext
    bytes[16..24].copy_from_slice(&4096u64.to_le_bytes());
    bytes[24..28].copy_from_slice(&2u32.to_le_bytes());
    let info = m.up.f.bytes(&bytes);

    let text = m.up.f.refusal(m.allocate, &[m.up.device, info, 0, out]).to_string();
    assert!(text.contains("pNext = 0xdead0000"), "the address is named: {text}");
    assert!(text.contains("VkImportMemoryHostPointerInfoEXT"), "{text}");
    assert!(text.contains("VkMemoryDedicatedAllocateInfo"), "and what it would have been: {text}");
    assert!(
        text.contains("two imports of one allocation"),
        "and why chaining behind it is not an option: {text}"
    );

    // The same refusal, at a `vkCreate*` that does not construct one -- with a `pNext` on a
    // structure of the right `sType`, so that it is the chain being refused and not the header.
    let mut bytes = vec![0u8; BUFFER_CREATE_INFO_BYTES];
    bytes[0..4].copy_from_slice(&12u32.to_le_bytes()); // BUFFER_CREATE_INFO
    bytes[8..16].copy_from_slice(&0xBEEF_0000u64.to_le_bytes());
    bytes[24..32].copy_from_slice(&256u64.to_le_bytes());
    bytes[32..36].copy_from_slice(&BUFFER_USAGE_VERTEX.to_le_bytes());
    let buffer_info = m.up.f.bytes(&bytes);
    let text = m
        .up
        .f
        .refusal(m.create_buffer, &[m.up.device, buffer_info, 0, out])
        .to_string();
    assert!(text.contains("refuses every `pNext` chain"), "{text}");
    assert!(text.contains("walking means knowing"), "{text}");
}

/// **A compute pipeline is its embedded stage and its layout, and a batch may partly fail.**
///
/// The engine's call (MEASURED): one create info, on its stack, whose stage is a
/// `VK_SHADER_STAGE_COMPUTE_BIT` stage embedded at 24 rather than behind a pointer. What this
/// catches: a stage read through a pointer that is not there, a layout read at the base
/// pipeline's offset, a base index read from the padding, and a failed slot left as whatever the
/// guest's array held.
#[test]
fn a_compute_pipeline_is_its_embedded_stage_and_its_layout() {
    let _serial = serialized();
    let m = up_to_memory("compute");
    let f = &m.up.f;
    let device = m.up.device;
    let create_set_layout = f.resolve_device(m.up.get_proc, device, "vkCreateDescriptorSetLayout");
    let create_layout = f.resolve_device(m.up.get_proc, device, "vkCreatePipelineLayout");
    let create_shader = f.resolve_device(m.up.get_proc, device, "vkCreateShaderModule");
    let create_compute = f.resolve_device(m.up.get_proc, device, "vkCreateComputePipelines");

    let out = f.alloc(8);
    assert_eq!(
        f.call(create_set_layout, [device, f.descriptor_set_layout_info(), 0, out])
            .expect("set layout") as i32,
        VK_SUCCESS
    );
    let set_layout = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_layout, [device, f.pipeline_layout_info(set_layout), 0, out])
            .expect("layout") as i32,
        VK_SUCCESS
    );
    let layout = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_shader, [device, f.shader_module_info(&COMPUTE_SPIRV), 0, out])
            .expect("module") as i32,
        VK_SUCCESS
    );
    let module = f.guest.read_u64(out as GuestAddr);

    // Two create infos with different flags, of which the driver will decline the second.
    let first = f.compute_pipeline_info(layout, module, SHADER_STAGE_COMPUTE, 0x1);
    let second = f.compute_pipeline_info(layout, module, SHADER_STAGE_COMPUTE, 0x2);
    let both = f.bytes(
        &[
            f.read_bytes(first, COMPUTE_PIPELINE_CREATE_INFO_BYTES),
            f.read_bytes(second, COMPUTE_PIPELINE_CREATE_INFO_BYTES),
        ]
        .concat(),
    );
    m.up.host.pipelines.lock().expect("no panic holds this").push_back(vec![true, false]);
    let pipelines_at = f.poisoned(16, 0x5A);
    let result = f
        .call_n(create_compute, &[device, 0, 2, both, 0, pipelines_at])
        .expect("the call completes");
    assert_eq!(result as i32, -2, "the driver's own code reaches the guest");
    let written = f.read_bytes(pipelines_at, 16);
    let created = u64::from_le_bytes(written[0..8].try_into().expect("eight"));
    assert_ne!(created, 0, "a real handle for the one that was created");
    assert_ne!(created, 0x5A5A_5A5A_5A5A_5A5A, "and written, not left as the poison");
    assert_eq!(
        u64::from_le_bytes(written[8..16].try_into().expect("eight")),
        0,
        "VK_NULL_HANDLE for the one that failed"
    );
    assert_eq!(f.vulkan().pipeline_handles().len(), 1, "one handle, for the one pipeline");

    let batches = m.up.host.log().compute_pipelines.clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 2);
    for (index, (request, flags)) in batches[0].iter().zip([0x1u32, 0x2]).enumerate() {
        assert_eq!(request.flags, flags, "flags {index}");
        assert_eq!(request.stage.stage, SHADER_STAGE_COMPUTE, "the embedded stage {index}");
        assert_eq!(request.stage.name, "main", "its entry point {index}");
        assert!(request.stage.module.is_some(), "its module, as a token {index}");
        assert_eq!(request.stage.specialization, None, "no specialization {index}");
        assert!(request.layout.is_some(), "the layout, as a token {index}");
        assert_eq!(request.base_pipeline, None, "no base pipeline {index}");
        assert_eq!(request.base_pipeline_index, -1, "basePipelineIndex {index}");
    }

    // **A stage that is not the compute stage is refused by name**, and reaches no host.
    let vertex = f.compute_pipeline_info(layout, module, SHADER_STAGE_VERTEX, 0);
    let text = f.refusal(create_compute, &[device, 0, 1, vertex, 0, pipelines_at]).to_string();
    assert!(text.contains("VK_SHADER_STAGE_COMPUTE_BIT"), "{text}");
    assert_eq!(m.up.host.log().compute_pipelines.len(), 1, "the refused call reached no host");
}

/// **`vkCreateGraphicsPipelines` may partly succeed, and the guest gets both halves.**
///
/// The only creation call in Vulkan that can, and the whole reason
/// [`PipelinesCreated`](omni_android::vulkan::PipelinesCreated) is not a `DriverAnswer`: the
/// specification requires `VK_NULL_HANDLE` in the slot of every pipeline that failed, a real
/// handle in every slot that did not, **and** an error code for the call. A shim that treated any
/// failure as total would drop handles the driver created, which leaks the most expensive object
/// a renderer makes.
///
/// No real driver can be asked to fail the second of two pipelines on demand, which is why this
/// one is against the double.
#[test]
fn a_partly_failed_pipeline_batch_writes_null_for_the_failures_and_keeps_the_successes() {
    let _serial = serialized();
    let m = up_to_memory("pipelines");
    let f = &m.up.f;
    let device = m.up.device;

    let create_pass = f.resolve_device(m.up.get_proc, device, "vkCreateRenderPass");
    let create_set_layout = f.resolve_device(m.up.get_proc, device, "vkCreateDescriptorSetLayout");
    let create_layout = f.resolve_device(m.up.get_proc, device, "vkCreatePipelineLayout");
    let create_shader = f.resolve_device(m.up.get_proc, device, "vkCreateShaderModule");
    let create_pipelines = f.resolve_device(m.up.get_proc, device, "vkCreateGraphicsPipelines");

    let out = f.alloc(8);
    assert_eq!(
        f.call(create_pass, [device, f.render_pass_info(FORMAT_B8G8R8A8_UNORM), 0, out])
            .expect("render pass") as i32,
        VK_SUCCESS
    );
    let pass = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_set_layout, [device, f.descriptor_set_layout_info(), 0, out])
            .expect("set layout") as i32,
        VK_SUCCESS
    );
    let set_layout = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_layout, [device, f.pipeline_layout_info(set_layout), 0, out])
            .expect("layout") as i32,
        VK_SUCCESS
    );
    let layout = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_shader, [device, f.shader_module_info(&TRIANGLE_VERT_SPIRV), 0, out])
            .expect("module") as i32,
        VK_SUCCESS
    );
    let vertex_module = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_shader, [device, f.shader_module_info(&TRIANGLE_FRAG_SPIRV), 0, out])
            .expect("module") as i32,
        VK_SUCCESS
    );
    let fragment_module = f.guest.read_u64(out as GuestAddr);

    // **The SPIR-V arrived unchanged.** Nothing translates it; `vulkan::shader` says why there is
    // nothing here to translate.
    let code = m.up.host.log().shader_code.clone();
    assert_eq!(code.len(), 2);
    let expected: Vec<u8> = TRIANGLE_VERT_SPIRV.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(code[0], expected, "the guest's SPIR-V bytes, verbatim");
    assert_eq!(code[0][0..4], 0x0723_0203u32.to_le_bytes(), "including the magic number");

    // Two create infos, of which the driver will decline the first.
    let first = f.graphics_pipeline_info(layout, pass, vertex_module, fragment_module);
    let second = f.graphics_pipeline_info(layout, pass, vertex_module, fragment_module);
    let both = f.bytes(
        &[
            f.read_bytes(first, GRAPHICS_PIPELINE_CREATE_INFO_BYTES),
            f.read_bytes(second, GRAPHICS_PIPELINE_CREATE_INFO_BYTES),
        ]
        .concat(),
    );
    m.up.host.pipelines.lock().expect("no panic holds this").push_back(vec![false, true]);

    let pipelines_at = f.poisoned(16, 0x5A);
    let result = f
        .call_n(create_pipelines, &[device, 0, 2, both, 0, pipelines_at])
        .expect("the call completes");
    assert_eq!(result as i32, -2, "the driver's own code reaches the guest");

    let written = f.read_bytes(pipelines_at, 16);
    assert_eq!(
        u64::from_le_bytes(written[0..8].try_into().expect("eight")),
        0,
        "**VK_NULL_HANDLE for the one that failed**, which is what the guest's own clean-up loop \
         reads -- and what the poison pattern would otherwise still be"
    );
    let survivor = u64::from_le_bytes(written[8..16].try_into().expect("eight"));
    assert_ne!(survivor, 0, "and a real handle for the one that was created");
    assert_eq!(
        f.vulkan().pipeline_handles().len(),
        1,
        "exactly one handle was issued, so the failed slot consumed nothing"
    );

    // The decoded request reached the host whole, including the members that are easiest to lose.
    let requests = m.up.host.log().pipelines.clone();
    assert_eq!(requests.len(), 1, "one call");
    let request = &requests[0][0];
    assert_eq!(request.stages.len(), 2);
    assert_eq!(request.stages[0].name, "main", "the entry point is the guest's, not assumed");
    assert_eq!(
        request.dynamic_states.as_deref(),
        Some(&[0u32, 1][..]),
        "VK_DYNAMIC_STATE_VIEWPORT and _SCISSOR"
    );
    let viewport = request.viewport.as_ref().expect("a viewport state");
    assert_eq!(viewport.viewport_count, 1);
    assert!(
        viewport.viewports.is_empty(),
        "**the count without the array**: with a dynamic viewport, `pViewports` is NULL and the \
         count is still required to be right. A shim that derived one from the other would build \
         a pipeline with no viewport at all"
    );
    assert!(request.depth_stencil.is_none(), "NULL stays NULL rather than becoming a zeroed one");
    assert!(request.tessellation.is_none());
    assert_eq!(
        request.rasterization.as_ref().map(Vec::len),
        Some(RASTERIZATION_STATE_BODY_BYTES),
        "the rasterization body travels as its members without the tail padding"
    );
}

/// **A descriptor pool takes its sets' handles with it, and a swapchain image cannot be
/// destroyed.**
///
/// Two ownership rules that are invisible from a handle. A `VkDescriptorSet` outliving its pool is
/// the wild non-dispatchable value Global Constraint 1 is about, reached with a handle this layer
/// itself issued; and `vkDestroyImage` on a swapchain image is undefined behaviour that would take
/// the presentation engine's own resource with it. Neither would be reported by anything on this
/// machine, which has no validation layers.
#[test]
fn a_descriptor_pool_takes_its_sets_and_a_swapchain_image_cannot_be_destroyed() {
    let _serial = serialized();
    let m = up_to_memory("ownership");
    let f = &m.up.f;
    let device = m.up.device;

    let create_set_layout = f.resolve_device(m.up.get_proc, device, "vkCreateDescriptorSetLayout");
    let create_pool = f.resolve_device(m.up.get_proc, device, "vkCreateDescriptorPool");
    let allocate_sets = f.resolve_device(m.up.get_proc, device, "vkAllocateDescriptorSets");
    let destroy_pool = f.resolve_device(m.up.get_proc, device, "vkDestroyDescriptorPool");
    let bind_sets = f.resolve_device(m.up.get_proc, device, "vkCmdBindDescriptorSets");

    let out = f.alloc(8);
    f.call(create_set_layout, [device, f.descriptor_set_layout_info(), 0, out]).expect("layout");
    let set_layout = f.guest.read_u64(out as GuestAddr);
    f.call(create_pool, [device, f.descriptor_pool_info(), 0, out]).expect("pool");
    let pool = f.guest.read_u64(out as GuestAddr);
    let set_out = f.alloc(8);
    assert_eq!(
        f.call(allocate_sets, [device, f.descriptor_set_allocate_info(pool, set_layout), set_out, 0])
            .expect("sets") as i32,
        VK_SUCCESS
    );
    let set = f.guest.read_u64(set_out as GuestAddr);
    assert_eq!(f.vulkan().descriptor_set_handles().len(), 1);

    f.call(destroy_pool, [device, pool, 0, 0]).expect("destroy pool");
    assert!(
        f.vulkan().descriptor_set_handles().is_empty(),
        "the set's handle went with its pool, which the guest never asked for and which the \
         specification makes true"
    );

    // And using it afterwards is a typed refusal rather than a bind of a freed object.
    let command = {
        let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, 0);
        f.call(f.resolve_device(m.up.get_proc, device, "vkCreateCommandPool"),
               [device, pool_info, 0, out]).expect("command pool");
        let command_pool = f.guest.read_u64(out as GuestAddr);
        let allocate_info = f.command_buffer_allocate_info(command_pool, 1);
        let buffers_at = f.alloc(8);
        f.call(f.resolve_device(m.up.get_proc, device, "vkAllocateCommandBuffers"),
               [device, allocate_info, buffers_at, 0]).expect("allocate");
        f.guest.read_u64(buffers_at as GuestAddr)
    };
    let sets = f.u64_array(&[set]);
    let layout = {
        let create_layout = f.resolve_device(m.up.get_proc, device, "vkCreatePipelineLayout");
        f.call(create_layout, [device, f.pipeline_layout_info(set_layout), 0, out])
            .expect("pipeline layout");
        f.guest.read_u64(out as GuestAddr)
    };
    let text = f.refusal(bind_sets, &[command, 0, layout, 0, 1, sets, 0, 0]).to_string();
    assert!(text.contains("`VkDescriptorSet`"), "{text}");
    assert!(text.contains("its **pool** is reset or destroyed"), "it says why: {text}");

    // ------------------------------------------------------- and the two image families
    let create_swapchain = f.resolve_device(m.up.get_proc, device, "vkCreateSwapchainKHR");
    let get_images = f.resolve_device(m.up.get_proc, device, "vkGetSwapchainImagesKHR");
    let destroy_image = f.resolve_device(m.up.get_proc, device, "vkDestroyImage");
    let create_image = f.resolve_device(m.up.get_proc, device, "vkCreateImage");

    let info = f.swapchain_info(m.up.surface, 2, FORMAT_B8G8R8A8_UNORM, 8, 8, SWAPCHAIN_USAGE, 1, 0);
    f.call(create_swapchain, [device, info, 0, out]).expect("swapchain");
    let swapchain = f.guest.read_u64(out as GuestAddr);
    let count_at = f.alloc(8);
    f.call(get_images, [device, swapchain, count_at, 0]).expect("count");
    let count = f.read_u32(count_at) as usize;
    let images_at = f.alloc(count * 8);
    f.call(get_images, [device, swapchain, count_at, images_at]).expect("images");
    let swapchain_image = f.guest.read_u64(images_at as GuestAddr);

    // Asking what a swapchain image needs is allowed -- MEASURED, the engine does -- and it is
    // the swapchain image's own answer, not a created image's.
    let requirements_of = f.resolve_device(m.up.get_proc, device, "vkGetImageMemoryRequirements");
    let needs_at = f.poisoned(24, 0x77);
    f.call(requirements_of, [device, swapchain_image, needs_at, 0]).expect("its requirements");
    assert_eq!(f.guest.read_u64(needs_at as GuestAddr), 8192, "the swapchain image's size");
    assert_eq!(f.guest.read_u64(needs_at as GuestAddr + 8), 1024, "and alignment");
    assert_eq!(f.read_u32(needs_at + 16), 0b10, "and memory types");

    let text = f.refusal(destroy_image, &[device, swapchain_image, 0, 0]).to_string();
    assert!(text.contains("VkImage (created)"), "the family it is not: {text}");
    assert!(
        text.contains("owned by their swapchain"),
        "and why destroying it is the mistake the split exists to prevent: {text}"
    );

    // A created image destroys fine, which is what says the refusal above is about the *family*
    // and not about `vkDestroyImage` being broken.
    let image_info = f.image_info(2, 2, FORMAT_R8G8B8A8_UNORM, IMAGE_USAGE_TEXTURE);
    assert_eq!(
        f.call(create_image, [device, image_info, 0, out]).expect("image") as i32,
        VK_SUCCESS
    );
    let created = f.guest.read_u64(out as GuestAddr);
    assert_eq!(f.vulkan().created_image_handles().len(), 1);
    f.call(destroy_image, [device, created, 0, 0]).expect("destroy");
    assert!(f.vulkan().created_image_handles().is_empty());
    assert_eq!(f.vulkan().image_handles().len(), count, "the swapchain's images are untouched");
}

/// **A real driver builds the compute pipeline the guest describes, and runs it.**
///
/// The guest's create info, with its stage embedded, crosses the boundary, is rebuilt in the host's
/// memory and handed to the machine's driver, which compiles the module's `GLCompute` entry point
/// against the layout. A stage flag, an entry-point name or a layout lost on the way is a driver
/// that fails the call -- or, with no validation layer, one that does something undefined -- so
/// `VK_SUCCESS` and a handle are the claim, and the handle is destroyed through the guest's path.
#[test]
#[ignore = "opens the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_real_driver_builds_the_compute_pipeline_the_guest_describes() {
    require_gate();
    let _serial = serialized();
    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture("live-compute", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let array_at = f.alloc(8);
    let result = f.call(enumerate, [instance, count_at, array_at, 0]).expect("array") as i32;
    assert!(result == VK_SUCCESS || result == VK_INCOMPLETE, "{result}");
    let physical = f.guest.read_u64(array_at as GuestAddr);
    let device = f.a_device(entry_point, instance, physical, 0);
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");

    let create_set_layout = f.resolve_device(get_proc, device, "vkCreateDescriptorSetLayout");
    let create_layout = f.resolve_device(get_proc, device, "vkCreatePipelineLayout");
    let create_shader = f.resolve_device(get_proc, device, "vkCreateShaderModule");
    let create_compute = f.resolve_device(get_proc, device, "vkCreateComputePipelines");
    let destroy = f.resolve_device(get_proc, device, "vkDestroyPipeline");

    let out = f.alloc(8);
    assert_eq!(
        f.call(create_set_layout, [device, f.descriptor_set_layout_info(), 0, out])
            .expect("set layout") as i32,
        VK_SUCCESS
    );
    let set_layout = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_layout, [device, f.pipeline_layout_info(set_layout), 0, out])
            .expect("layout") as i32,
        VK_SUCCESS
    );
    let layout = f.guest.read_u64(out as GuestAddr);
    assert_eq!(
        f.call(create_shader, [device, f.shader_module_info(&COMPUTE_SPIRV), 0, out])
            .expect("module") as i32,
        VK_SUCCESS
    );
    let module = f.guest.read_u64(out as GuestAddr);

    let info = f.compute_pipeline_info(layout, module, SHADER_STAGE_COMPUTE, 0);
    let pipeline_at = f.poisoned(8, 0x5A);
    let result = f
        .call_n(create_compute, &[device, 0, 1, info, 0, pipeline_at])
        .expect("the call completes");
    assert_eq!(result as i32, VK_SUCCESS, "the driver compiled the guest's compute pipeline");
    let pipeline = f.guest.read_u64(pipeline_at as GuestAddr);
    assert_ne!(pipeline, 0);
    assert_ne!(pipeline, 0x5A5A_5A5A_5A5A_5A5A, "a handle was written");
    assert_eq!(f.vulkan().pipeline_handles().len(), 1);

    // **And dispatched on a real queue**, bound at the compute bind point with the engine's own
    // first group counts. A lost device answers the idle wait -4.
    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [device, 0, 0, queue_at]).expect("a queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);
    let name = |call: &str| f.resolve_device(get_proc, device, call);
    f.call(name("vkCreateCommandPool"), [device, f.command_pool_info(0, 0), 0, out])
        .expect("a command pool");
    let command_pool = f.guest.read_u64(out as GuestAddr);
    let buffers_at = f.alloc(8);
    f.call(
        name("vkAllocateCommandBuffers"),
        [device, f.command_buffer_allocate_info(command_pool, 1), buffers_at, 0],
    )
    .expect("a command buffer");
    let command = f.guest.read_u64(buffers_at as GuestAddr);
    assert_eq!(
        f.call(name("vkBeginCommandBuffer"), [command, f.begin_info(0, 0), 0, 0]).expect("begin")
            as i32,
        VK_SUCCESS
    );
    // **Timed as the engine times its frame**: a two-query timestamp pool, reset, written either
    // side of the dispatch, and read back after the queue drains.
    let mut pool_info = vec![0u8; QUERY_POOL_CREATE_INFO_BYTES];
    pool_info[0..4].copy_from_slice(&11u32.to_le_bytes()); // QUERY_POOL_CREATE_INFO
    pool_info[20..24].copy_from_slice(&2u32.to_le_bytes()); // VK_QUERY_TYPE_TIMESTAMP
    pool_info[24..28].copy_from_slice(&2u32.to_le_bytes()); // queryCount
    let pool_info = f.bytes(&pool_info);
    assert_eq!(
        f.call(name("vkCreateQueryPool"), [device, pool_info, 0, out]).expect("pool") as i32,
        VK_SUCCESS
    );
    let timer = f.guest.read_u64(out as GuestAddr);
    f.call(name("vkCmdResetQueryPool"), [command, timer, 0, 2]).expect("reset");
    // VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, query 0.
    f.call(name("vkCmdWriteTimestamp"), [command, 0x1, timer, 0]).expect("timestamp");
    // VK_PIPELINE_BIND_POINT_COMPUTE.
    f.call(name("vkCmdBindPipeline"), [command, 1, pipeline, 0]).expect("bind");
    f.call(name("vkCmdDispatch"), [command, 5, 3, 1]).expect("dispatch");
    // VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, query 1.
    f.call(name("vkCmdWriteTimestamp"), [command, 0x2000, timer, 1]).expect("timestamp");
    assert_eq!(
        f.call(name("vkEndCommandBuffer"), [command, 0, 0, 0]).expect("end") as i32,
        VK_SUCCESS
    );
    // One command buffer, no semaphores.
    let commands = f.u64_array(&[command]);
    let mut submit = vec![0u8; SUBMIT_INFO_BYTES];
    submit[0..4].copy_from_slice(&STYPE_SUBMIT_INFO.to_le_bytes());
    submit[40..44].copy_from_slice(&1u32.to_le_bytes());
    submit[48..56].copy_from_slice(&commands.to_le_bytes());
    let submit = f.bytes(&submit);
    assert_eq!(
        f.call(name("vkQueueSubmit"), [queue, 1, submit, 0]).expect("submit") as i32,
        VK_SUCCESS
    );
    assert_eq!(
        f.call(name("vkQueueWaitIdle"), [queue, 0, 0, 0]).expect("idle") as i32,
        VK_SUCCESS,
        "the dispatch ran to completion on the real queue"
    );
    // The engine's read-back, plus `WAIT`: 64-bit, two queries, a stride of eight.
    let results = f.poisoned(16, 0x5A);
    let answer = f
        .call_n(name("vkGetQueryPoolResults"), &[device, timer, 0, 2, 16, results, 8, 0x1 | 0x2])
        .expect("the read-back completes");
    assert_eq!(answer as i32, VK_SUCCESS, "both timestamps are available after the idle wait");
    let (start, end) =
        (f.guest.read_u64(results as GuestAddr), f.guest.read_u64(results as GuestAddr + 8));
    assert_ne!(start, 0x5A5A_5A5A_5A5A_5A5A, "the driver wrote the first");
    assert!(end >= start, "and the GPU's clock ran forwards across the dispatch: {start} -> {end}");
    f.call(name("vkDestroyQueryPool"), [device, timer, 0, 0]).expect("the pool is destroyed");

    f.call(destroy, [device, pipeline, 0, 0]).expect("the pipeline is destroyed");
    assert!(f.vulkan().pipeline_handles().is_empty(), "and its handle is forgotten");
}

/// **The real driver's pipeline cache, saved through the guest path and loaded back.**
///
/// The engine was measured calling `vkGetPipelineCacheData` on `APP_CMD_TERM_WINDOW`, to write the
/// cache to disk for the next launch's `vkCreatePipelineCache`. Against the real driver this test
/// checks three things:
///
/// * the blob's header is this machine's: `VK_PIPELINE_CACHE_HEADER_VERSION_ONE`, and the
///   vendor, device and `pipelineCacheUUID` that `vkGetPhysicalDeviceProperties` reports;
/// * a short buffer is `VK_INCOMPLETE` with nothing written, **and the process survives it**;
/// * a cache loaded from the saved blob holds exactly what was saved.
///
/// # The short buffer is this test's detector for a measured driver defect
///
/// The first version of this layer handed the guest's short capacity to the driver, so the
/// driver would make the cut. Given room for 2,766 bytes of a 5,499-byte cache, this machine's
/// driver wrote all 5,499 bytes, 2,733 past the end of the host's buffer, then answered
/// `VK_INCOMPLETE` with a count of 36. This test died of it, `STATUS_HEAP_CORRUPTION`. A layer
/// that ever gives the driver a short buffer again brings that back.
#[test]
#[ignore = "opens the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn the_real_drivers_pipeline_cache_is_saved_through_the_guest_and_loads_back() {
    require_gate();
    let _serial = serialized();
    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture("live-cache", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let array_at = f.alloc(8);
    let result = f.call(enumerate, [instance, count_at, array_at, 0]).expect("array") as i32;
    assert!(result == VK_SUCCESS || result == VK_INCOMPLETE, "{result}");
    let physical = f.guest.read_u64(array_at as GuestAddr);
    let properties = f.resolve(entry_point, instance, "vkGetPhysicalDeviceProperties");
    let properties_at = f.alloc(PHYSICAL_DEVICE_PROPERTIES_BYTES);
    f.call(properties, [physical, properties_at, 0, 0]).expect("properties");
    let properties = f.read_bytes(properties_at, PHYSICAL_DEVICE_PROPERTIES_BYTES);
    let device = f.a_device(entry_point, instance, physical, 0);
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");
    let name = |call: &str| f.resolve_device(get_proc, device, call);
    let create_cache = name("vkCreatePipelineCache");
    let destroy_cache = name("vkDestroyPipelineCache");
    // Through `vkGetInstanceProcAddr`, as the engine was measured reaching it.
    let get_data = f.resolve(entry_point, instance, "vkGetPipelineCacheData");

    let out = f.alloc(8);
    let size_at = f.alloc(8);
    let new_cache = |initial: &[u8]| {
        let info = f.pipeline_cache_info(initial);
        let result = f.call(create_cache, [device, info, 0, out]).expect("the call completes");
        assert_eq!(result as i32, VK_SUCCESS, "the driver answered VkResult {}", result as i32);
        f.guest.read_u64(out as GuestAddr)
    };
    let size_of = |cache: u64| {
        let result = f.call(get_data, [device, cache, size_at, 0]).expect("the size");
        assert_eq!(result as i32, VK_SUCCESS);
        f.guest.read_u64(size_at as GuestAddr) as usize
    };
    // Room for `capacity` bytes and 8 more, all poisoned: the result, what `*pDataSize` says was
    // written, and the whole buffer afterwards.
    let read = |cache: u64, capacity: usize| {
        let data_at = f.poisoned(capacity + 8, 0x5A);
        f.guest.write_u64(size_at as GuestAddr, capacity as u64);
        let result = f.call(get_data, [device, cache, size_at, data_at]).expect("the data") as i32;
        let written = f.guest.read_u64(size_at as GuestAddr) as usize;
        assert!(written <= capacity, "{written} bytes written into room for {capacity}");
        (result, written, f.read_bytes(data_at, capacity + 8))
    };

    let empty = new_cache(&[]);
    let empty_size = size_of(empty);

    // A pipeline built **through** a cache, which is what gives the cache something to save.
    let cache = new_cache(&[]);
    f.call(name("vkCreateDescriptorSetLayout"), [device, f.descriptor_set_layout_info(), 0, out])
        .expect("set layout");
    let set_layout = f.guest.read_u64(out as GuestAddr);
    f.call(name("vkCreatePipelineLayout"), [device, f.pipeline_layout_info(set_layout), 0, out])
        .expect("layout");
    let layout = f.guest.read_u64(out as GuestAddr);
    f.call(name("vkCreateShaderModule"), [device, f.shader_module_info(&COMPUTE_SPIRV), 0, out])
        .expect("module");
    let module = f.guest.read_u64(out as GuestAddr);
    let info = f.compute_pipeline_info(layout, module, SHADER_STAGE_COMPUTE, 0);
    let pipeline_at = f.alloc(8);
    let result = f
        .call_n(name("vkCreateComputePipelines"), &[device, cache, 1, info, 0, pipeline_at])
        .expect("the call completes");
    assert_eq!(result as i32, VK_SUCCESS, "the driver compiled the pipeline through the cache");

    // **The whole blob**, and its header is this machine's.
    let size = size_of(cache);
    assert!(size > empty_size, "building a pipeline through the cache gave it something to save");
    let (result, written, buffer) = read(cache, size);
    assert_eq!(result, VK_SUCCESS);
    assert_eq!(written, size);
    assert_eq!(buffer[size..], [0x5A; 8], "nothing past the blob");
    let blob = buffer[..size].to_vec();
    let word =
        |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
    assert_eq!(word(&blob, 0) as usize, CACHE_HEADER_BYTES, "headerSize");
    assert_eq!(word(&blob, 4), 1, "VK_PIPELINE_CACHE_HEADER_VERSION_ONE");
    assert_eq!(word(&blob, 8), word(&properties, 8), "vendorID is the device's");
    assert_eq!(word(&blob, 12), word(&properties, 12), "deviceID is the device's");
    assert_eq!(blob[16..32], properties[276..292], "pipelineCacheUUID is the device's");

    // **A short buffer: nothing, `VK_INCOMPLETE`, and a process still standing.** Room for the
    // header and one byte more than half the body: the length the defect was measured at.
    let short = CACHE_HEADER_BYTES + (size - CACHE_HEADER_BYTES) / 2 + 1;
    let (result, written, buffer) = read(cache, short);
    assert_eq!(result, VK_INCOMPLETE, "a truncated save is VK_INCOMPLETE, not VK_SUCCESS");
    assert_eq!(written, 0, "and nothing was written");
    assert_eq!(buffer, vec![0x5A; short + 8], "not one byte, in the buffer or past it");

    // **The round trip**: a cache created from the saved blob holds exactly it.
    let reloaded = new_cache(&blob);
    let reloaded_size = size_of(reloaded);
    let (result, written, buffer) = read(reloaded, reloaded_size);
    assert_eq!(result, VK_SUCCESS);
    let reloaded_blob = &buffer[..written];

    eprintln!("\n=== vkGetPipelineCacheData evidence ===");
    eprintln!(
        "vendor {:#x}, device {:#x}, pipelineCacheUUID {:02x?}",
        word(&blob, 8),
        word(&blob, 12),
        &blob[16..32]
    );
    eprintln!("an empty cache: {empty_size} bytes; after one compute pipeline: {size} bytes");
    eprintln!("room for {short}: VkResult {VK_INCOMPLETE}, nothing written, process intact");
    eprintln!(
        "loaded back from the saved blob: {reloaded_size} bytes, identical: {}",
        reloaded_blob == blob.as_slice()
    );
    assert_eq!(
        reloaded_blob,
        blob.as_slice(),
        "a cache loaded from the saved blob holds exactly what was saved -- the driver took it"
    );

    for handle in [empty, cache, reloaded] {
        f.call(destroy_cache, [device, handle, 0, 0]).expect("destroy a cache");
    }
    let pipeline = f.guest.read_u64(pipeline_at as GuestAddr);
    f.call(name("vkDestroyPipeline"), [device, pipeline, 0, 0]).expect("destroy the pipeline");
}

/// **The real driver's device is destroyed through the guest path only once nothing made from it
/// is alive, and then it is gone from the host.**
///
/// Measured: the engine's render thread tears its device down on `APP_CMD_TERM_WINDOW`. The
/// children check is `GfxVulkanHost`'s -- the guest-side registries do not know which device an
/// object came from -- so it is exercised here, against the real host:
///
/// * with two semaphores, a fence and a command pool holding a command buffer alive, the refusal
///   names each kind with its count, and leaves out the command buffer, which its pool frees;
/// * with only the pool left, it names only the pool;
/// * with nothing left, the device is destroyed, its queue goes with it, and the host holds no
///   device and no queue; the stale handle and the stale token are both refusals;
/// * a second device -- the one the engine makes at its next window -- is made and destroyed the
///   same way.
#[test]
#[ignore = "opens the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn the_real_device_is_destroyed_through_the_guest_only_after_its_children() {
    require_gate();
    let _serial = serialized();
    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture("live-destroy-device", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let array_at = f.alloc(8);
    let result = f.call(enumerate, [instance, count_at, array_at, 0]).expect("array") as i32;
    assert!(result == VK_SUCCESS || result == VK_INCOMPLETE, "{result}");
    let physical = f.guest.read_u64(array_at as GuestAddr);
    // Through `vkGetInstanceProcAddr`, as the engine was measured reaching it.
    let destroy_device = f.resolve(entry_point, instance, "vkDestroyDevice");
    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");

    let device = f.a_device(entry_point, instance, physical, 0);
    let token = f.vulkan().device_handles()[0].1;
    let queue_at = f.alloc(8);
    f.call(get_queue, [device, 0, 0, queue_at]).expect("a queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);
    assert_eq!((host.objects().1, host.objects().2), (1, 1), "one device and one queue");

    // Children of three kinds, and a command buffer its pool will free.
    let name = |call: &str| f.resolve_device(get_proc, device, call);
    let wait_idle = name("vkQueueWaitIdle");
    let out = f.alloc(8);
    let mut semaphores = Vec::new();
    for _ in 0..2 {
        let info = f.flags_only_info(STYPE_SEMAPHORE_CREATE_INFO, 0);
        f.call(name("vkCreateSemaphore"), [device, info, 0, out]).expect("a semaphore");
        semaphores.push(f.guest.read_u64(out as GuestAddr));
    }
    let info = f.flags_only_info(STYPE_FENCE_CREATE_INFO, 0);
    f.call(name("vkCreateFence"), [device, info, 0, out]).expect("a fence");
    let fence = f.guest.read_u64(out as GuestAddr);
    f.call(name("vkCreateCommandPool"), [device, f.command_pool_info(0, 0), 0, out])
        .expect("a command pool");
    let pool = f.guest.read_u64(out as GuestAddr);
    let buffers_at = f.alloc(8);
    f.call(
        name("vkAllocateCommandBuffers"),
        [device, f.command_buffer_allocate_info(pool, 1), buffers_at, 0],
    )
    .expect("a command buffer");

    // **Refused, naming every kind that lives, and the device is untouched.**
    let text = f.refusal(destroy_device, &[device, 0]).to_string();
    assert!(text.contains("2 `VkSemaphore`"), "{text}");
    assert!(text.contains("1 `VkFence`"), "{text}");
    assert!(text.contains("1 `VkCommandPool`"), "{text}");
    assert!(!text.contains("VkCommandBuffer"), "a pool frees its own buffers: {text}");
    assert_eq!((host.objects().1, host.objects().2), (1, 1), "nothing was destroyed");
    assert_eq!(f.vulkan().device_handles().len(), 1, "the guest's handle still names it");
    let idle = f.call(wait_idle, [queue, 0, 0, 0]).expect("the queue still answers") as i32;
    assert_eq!(idle, VK_SUCCESS, "and the device still works");
    eprintln!("\n=== vkDestroyDevice evidence ===");
    eprintln!("with children alive, refused by name:\n  {text}");

    // Only the pool left: only the pool named.
    for semaphore in &semaphores {
        f.call(name("vkDestroySemaphore"), [device, *semaphore, 0, 0]).expect("destroy");
    }
    f.call(name("vkDestroyFence"), [device, fence, 0, 0]).expect("destroy the fence");
    let text = f.refusal(destroy_device, &[device, 0]).to_string();
    assert!(text.contains("1 `VkCommandPool`"), "{text}");
    assert!(!text.contains("VkSemaphore") && !text.contains("VkFence"), "{text}");
    f.call(name("vkDestroyCommandPool"), [device, pool, 0, 0]).expect("destroy the pool");

    // **Nothing left: destroyed, once, and gone from the host with its queue.**
    f.call(destroy_device, [device, 0, 0, 0]).expect("destroy the device");
    assert_eq!((host.objects().1, host.objects().2), (0, 0), "no device and no queue on the host");
    assert!(f.vulkan().device_handles().is_empty());
    assert!(f.vulkan().queue_handles().is_empty(), "the queue's handle went with the device");
    let text = f.refusal(destroy_device, &[device, 0]).to_string();
    assert!(text.contains("`VkDevice`"), "the stale handle refuses by name: {text}");
    let stale = host.destroy_device(token).expect_err("the host refuses the stale token too");
    assert!(stale.to_string().contains("already destroyed"), "{stale}");
    eprintln!(
        "vkDestroyDevice({device:#x}) -> the host holds {:?} (surfaces, devices, queues)",
        host.objects()
    );

    // **The next window's device**: a new token, never the old one, destroyed the same way.
    let again = f.a_device(entry_point, instance, physical, 0);
    let fresh = f.vulkan().device_handles()[0].1;
    assert_ne!(fresh, token, "a destroyed device's token is never handed out again");
    f.call(get_queue, [again, 0, 0, queue_at]).expect("a queue of the new device");
    assert_eq!((host.objects().1, host.objects().2), (1, 1));
    f.call(destroy_device, [again, 0, 0, 0]).expect("destroy the second device");
    assert_eq!((host.objects().1, host.objects().2), (0, 0));
    eprintln!("a second device ({fresh:?}, the first {token:?}) was made and destroyed alike");
}

/// **Stage 5's evidence: a textured triangle, drawn by guest code, presented, and its pixels
/// asserted.**
///
/// # Why this one test and not twenty
///
/// One path exercises the whole stage: device memory allocated through both branches of the split,
/// `vkMapMemory` answering an address the guest stores through, a vertex buffer, a staging buffer,
/// an image, a sampler, an image view, two shader modules of real SPIR-V, a pipeline layout, a
/// render pass, a framebuffer, a graphics pipeline, a descriptor set layout, a pool, a set, an
/// update, and eleven `vkCmd*` calls ending in `vkCmdDraw`. Any one of them answering
/// `VK_SUCCESS` without doing its job changes the pixels that come back, and the pixels are what
/// is asserted.
///
/// # What the four quadrants prove that one colour would not
///
/// The texture is 2×2 with four distinct colours ([`TEXELS`]) and the triangle is the fullscreen
/// one, so `u` and `v` span the viewport exactly. Nearest filtering means each screen quadrant is
/// one texel, exactly, with no blend to round. So the assertion catches, separately:
///
/// * **no draw at all** — the frame is [`DRAW_CLEAR_BYTES`], which is none of the four;
/// * **no texture upload** — the image holds whatever the driver left in it, which would have to
///   be these four colours in these four places;
/// * **a flipped or transposed UV mapping** — the four colours appear in the wrong quadrants,
///   which is the failure a single-colour texture cannot see at all;
/// * **a descriptor pointing somewhere else** — the sampled colour is not the texture's.
///
/// # What the read-back can and cannot see
///
/// The same limit stage 4's test states, and it is stated again rather than inherited: these are
/// the pixels handed to the presentation engine, read back out of the swapchain image. They are
/// not a photograph of the monitor, because `PrintWindow(PW_RENDERFULLCONTENT)` returns solid
/// black for a flip-model swapchain's client area on this host
/// (`docs/research/graphics-spike.md` §1). The step between "these pixels were presented" and
/// "these pixels are on the screen" is the compositor's.
#[test]
#[ignore = "opens a window and the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_textured_triangle_is_drawn_by_guest_code_and_the_presented_pixels_are_its_texels() {
    require_gate();
    let _serial = serialized();

    let mut window = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
        "Omnidroid — Vulkan stage 5: a guest textured triangle",
        512,
        512,
    ))
    .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    let _ = window.poll_events().count();
    let source = HostWindowSource::watching(&window).expect("a source watching the window");

    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture("live-stage5", Some(host.clone()));
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let surface = f.a_surface(entry_point, instance);

    // ---------------------------------------------------------------- choose a device and family
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(8);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let device_count = f.read_u32(count_at) as usize;
    assert!(device_count > 0, "a machine with a Vulkan loader and no GPU cannot draw");
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
    let memory_properties =
        f.resolve(entry_point, instance, "vkGetPhysicalDeviceMemoryProperties");

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
                continue;
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
    let (physical, device_name, family) = chosen.expect("a graphics-and-present queue family");

    // ----------------------------------------------- the memory table, **as the guest is shown it**
    let memory_at = f.alloc(PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);
    f.call(memory_properties, [physical, memory_at, 0, 0]).expect("memory properties");
    let shown = f.read_bytes(memory_at, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);
    let shown_count = u32::from_le_bytes(shown[0..4].try_into().expect("four"));
    assert!(shown_count > 0, "a device with no memory types cannot be drawn with");

    // **The rewrite, asserted where it is visible.** Whatever this host masked is in the log, and
    // every type that still advertises HOST_VISIBLE is one this layer can back — which is the
    // property the whole memory design rests on, and the one that makes `vkMapMemory` below able
    // to answer at all.
    let masked: Vec<_> = f
        .vulkan()
        .rewrites()
        .into_iter()
        .filter(|rewrite| matches!(rewrite.site, RewriteSite::MemoryType { .. }))
        .collect();
    let importable = host.importable_memory_types(
        f.vulkan()
            .physical_device_handles()
            .iter()
            .find(|(handle, _)| *handle as u64 == physical)
            .map(|(_, token)| *token)
            .expect("the physical device handle is one this layer issued"),
    )
    .expect("this host can say what it can import");
    for index in 0..shown_count {
        let at = 4 + index as usize * 8;
        let flags = u32::from_le_bytes(shown[at..at + 4].try_into().expect("four"));
        if flags & MEMORY_HOST_VISIBLE != 0 {
            assert_ne!(
                importable & (1 << index),
                0,
                "memory type {index} is advertised to the guest as host-visible, so this layer \
                 must be able to import into it -- otherwise `vkMapMemory` on it would have to \
                 refuse one call after the guest was told it could map"
            );
        }
    }

    // ------------------------------------------------------------------ the surface's own terms
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
        "this surface's swapchain images cannot be both a colour attachment and a transfer \
         source, so the frame could be drawn or read back but not both. \
         supportedUsageFlags = {supported_usage:#x}"
    );

    assert_eq!(
        f.call(formats, [physical, surface, count_at, 0]).expect("format count") as i32,
        VK_SUCCESS
    );
    let format_count = f.read_u32(count_at) as usize;
    f.guest.write_u32(count_at as GuestAddr, format_count as u32);
    assert_eq!(
        f.call(formats, [physical, surface, count_at, scratch]).expect("formats") as i32,
        VK_SUCCESS
    );
    let format_bytes = f.read_bytes(scratch, format_count * SURFACE_FORMAT_BYTES);
    let offered: Vec<u32> =
        (0..format_count).map(|i| read_u32(&format_bytes, i * SURFACE_FORMAT_BYTES)).collect();
    let format = *offered
        .iter()
        .find(|f| **f == FORMAT_B8G8R8A8_UNORM || **f == FORMAT_R8G8B8A8_UNORM)
        .unwrap_or_else(|| {
            panic!("this surface offers no eight-bit UNORM format ({offered:?})")
        });

    // ------------------------------------------------------------------ the device and its queue
    let logical = f.a_device(entry_point, instance, physical, family);

    // **The device this layer created is not the device the guest described**, and the log says
    // so. `VK_EXT_external_memory_host` is what `vkMapMemory` needs, the engine asked only for
    // `VK_KHR_swapchain`, and an addition nobody recorded is what Global Constraint 1 forbids.
    let added: Vec<_> = f
        .vulkan()
        .rewrites()
        .into_iter()
        .filter(|rewrite| rewrite.site == RewriteSite::DeviceExtensionAdded)
        .collect();
    assert_eq!(added.len(), 1, "exactly one extension was added: {added:?}");
    assert_eq!(added[0].to, "VK_EXT_external_memory_host");
    assert!(added[0].from.contains("not requested"), "{:?}", added[0]);

    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [logical, u64::from(family), 0, queue_at]).expect("queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);

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
    let allocate_buffers = name("vkAllocateCommandBuffers");
    let begin = name("vkBeginCommandBuffer");
    let end = name("vkEndCommandBuffer");
    let barrier = name("vkCmdPipelineBarrier");
    let acquire = name("vkAcquireNextImageKHR");
    let submit = name("vkQueueSubmit");
    let present = name("vkQueuePresentKHR");
    let queue_wait_idle = name("vkQueueWaitIdle");
    let device_wait_idle = name("vkDeviceWaitIdle");
    // Stage 5's own.
    let allocate_memory = name("vkAllocateMemory");
    let free_memory = name("vkFreeMemory");
    let map_memory = name("vkMapMemory");
    let unmap_memory = name("vkUnmapMemory");
    let buffer_requirements = name("vkGetBufferMemoryRequirements");
    let image_requirements = name("vkGetImageMemoryRequirements");
    let bind_buffer = name("vkBindBufferMemory");
    let bind_image = name("vkBindImageMemory");
    let create_buffer = name("vkCreateBuffer");
    let destroy_buffer = name("vkDestroyBuffer");
    let create_image = name("vkCreateImage");
    let destroy_image = name("vkDestroyImage");
    let create_sampler = name("vkCreateSampler");
    let destroy_sampler = name("vkDestroySampler");
    let create_shader = name("vkCreateShaderModule");
    let destroy_shader = name("vkDestroyShaderModule");
    let create_set_layout = name("vkCreateDescriptorSetLayout");
    let destroy_set_layout = name("vkDestroyDescriptorSetLayout");
    let create_descriptor_pool = name("vkCreateDescriptorPool");
    let destroy_descriptor_pool = name("vkDestroyDescriptorPool");
    let allocate_sets = name("vkAllocateDescriptorSets");
    let update_sets = name("vkUpdateDescriptorSets");
    let create_pipeline_layout = name("vkCreatePipelineLayout");
    let destroy_pipeline_layout = name("vkDestroyPipelineLayout");
    let create_render_pass = name("vkCreateRenderPass");
    let destroy_render_pass = name("vkDestroyRenderPass");
    let create_framebuffer = name("vkCreateFramebuffer");
    let destroy_framebuffer = name("vkDestroyFramebuffer");
    let create_pipelines = name("vkCreateGraphicsPipelines");
    let destroy_pipeline = name("vkDestroyPipeline");
    let cmd_begin_pass = name("vkCmdBeginRenderPass");
    let cmd_end_pass = name("vkCmdEndRenderPass");
    let cmd_bind_pipeline = name("vkCmdBindPipeline");
    let cmd_bind_vertex = name("vkCmdBindVertexBuffers");
    let cmd_bind_sets = name("vkCmdBindDescriptorSets");
    let cmd_set_viewport = name("vkCmdSetViewport");
    let cmd_set_scissor = name("vkCmdSetScissor");
    let cmd_draw = name("vkCmdDraw");
    let cmd_copy_to_image = name("vkCmdCopyBufferToImage");

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
    assert_eq!(
        f.call(create_swapchain, [logical, info, 0, out]).expect("the call completes") as i32,
        VK_SUCCESS
    );
    let swapchain = f.guest.read_u64(out as GuestAddr);

    assert_eq!(
        f.call(get_images, [logical, swapchain, count_at, 0]).expect("image count") as i32,
        VK_SUCCESS
    );
    let image_count = f.read_u32(count_at) as usize;
    let images_at = f.alloc(image_count * 8);
    assert_eq!(
        f.call(get_images, [logical, swapchain, count_at, images_at]).expect("images") as i32,
        VK_SUCCESS
    );
    let images: Vec<u64> =
        (0..image_count).map(|i| f.guest.read_u64(images_at as GuestAddr + i * 8)).collect();
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

    // ----------------------------------------------------------- the render pass and its targets
    let pass_info = f.render_pass_info(format);
    let pass_out = f.alloc(8);
    assert_eq!(
        f.call(create_render_pass, [logical, pass_info, 0, pass_out]).expect("render pass") as i32,
        VK_SUCCESS
    );
    let render_pass = f.guest.read_u64(pass_out as GuestAddr);
    let framebuffers: Vec<u64> = views
        .iter()
        .map(|view| {
            let fb_info = f.framebuffer_info(render_pass, *view, extent.0, extent.1);
            let fb_out = f.alloc(8);
            assert_eq!(
                f.call(create_framebuffer, [logical, fb_info, 0, fb_out]).expect("framebuffer")
                    as i32,
                VK_SUCCESS
            );
            f.guest.read_u64(fb_out as GuestAddr)
        })
        .collect();

    // ----------------------------------------------------- the vertex buffer, through `vkMapMemory`
    let vertices = triangle_vertices();
    let vertex_buffer_info = f.buffer_info(vertices.len() as u64, BUFFER_USAGE_VERTEX);
    let vertex_out = f.alloc(8);
    assert_eq!(
        f.call(create_buffer, [logical, vertex_buffer_info, 0, vertex_out]).expect("buffer") as i32,
        VK_SUCCESS
    );
    let vertex_buffer = f.guest.read_u64(vertex_out as GuestAddr);

    let requirements_at = f.alloc(MEMORY_REQUIREMENTS_BYTES);
    f.call(buffer_requirements, [logical, vertex_buffer, requirements_at, 0]).expect("needs");
    let requirements = f.read_bytes(requirements_at, MEMORY_REQUIREMENTS_BYTES);
    let vertex_bytes = u64::from_le_bytes(requirements[0..8].try_into().expect("eight"));
    let vertex_type_bits = u32::from_le_bytes(requirements[16..20].try_into().expect("four"));
    let host_visible_type = f
        .memory_type_index(&shown, vertex_type_bits, MEMORY_HOST_VISIBLE | MEMORY_HOST_COHERENT)
        .expect(
            "the list the guest was shown must still contain a host-visible, host-coherent type \
             the vertex buffer can be bound to -- if the mask left none, nothing can be uploaded",
        );

    let vertex_alloc = f.memory_allocate_info(vertex_bytes, host_visible_type);
    let memory_out = f.alloc(8);
    assert_eq!(
        f.call(allocate_memory, [logical, vertex_alloc, 0, memory_out]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let vertex_memory = f.guest.read_u64(memory_out as GuestAddr);
    assert_eq!(
        f.call(bind_buffer, [logical, vertex_buffer, vertex_memory, 0]).expect("bind") as i32,
        VK_SUCCESS
    );

    // **The measurement this whole stage rests on.**
    let mapped_at = f.alloc(8);
    assert_eq!(
        f.call_n(map_memory, &[logical, vertex_memory, 0, VK_WHOLE_SIZE, 0, mapped_at])
            .expect("map") as i32,
        VK_SUCCESS
    );
    let mapped = f.guest.read_u64(mapped_at as GuestAddr);
    let space = &f.guest.space;
    assert!(
        mapped >= space.base() as u64 && mapped < space.end() as u64,
        "**`vkMapMemory` must answer with an address inside `GuestSpace`**: it answered \
         {mapped:#x}, and the guest's space is [{base:#x}, {end:#x}). A driver pointer here would \
         be stored through successfully by translated ARM64 -- D4 amendment 1 means `admit` does \
         not govern the guest's own stores -- and the failure would arrive far from the cause",
        base = space.base(),
        end = space.end()
    );
    // And `admit` admits it, with **zero changes to `omni-mem`**, which is the claim
    // `docs/HANDOFF.md` makes for the `VK_EXT_external_memory_host` route.
    let admitted = omni_mem::admit(
        space,
        mapped as GuestAddr,
        vertices.len(),
        omni_mem::FaultAccess::Write,
    )
    .expect("the mapping `vkMapMemory` answered with is one `admit` admits for writing");
    assert!(admitted.end >= mapped as GuestAddr + vertices.len());

    // Written through that very pointer. `Guest::write_bytes` goes through `GuestSpace::ptr`,
    // which refuses an address outside the space -- so this line is itself an assertion.
    f.guest.write_bytes(mapped as GuestAddr, &vertices);
    f.call(unmap_memory, [logical, vertex_memory, 0, 0]).expect("unmap");

    // ------------------------------------------------- the texture, and the other half of the split
    let texel_bytes: Vec<u8> = TEXELS.iter().flat_map(|texel| texel.iter().copied()).collect();
    let staging_info = f.buffer_info(texel_bytes.len() as u64, BUFFER_USAGE_TRANSFER_SRC);
    let staging_out = f.alloc(8);
    assert_eq!(
        f.call(create_buffer, [logical, staging_info, 0, staging_out]).expect("staging") as i32,
        VK_SUCCESS
    );
    let staging = f.guest.read_u64(staging_out as GuestAddr);
    f.call(buffer_requirements, [logical, staging, requirements_at, 0]).expect("needs");
    let staging_requirements = f.read_bytes(requirements_at, MEMORY_REQUIREMENTS_BYTES);
    let staging_size = u64::from_le_bytes(staging_requirements[0..8].try_into().expect("eight"));
    let staging_type = f
        .memory_type_index(
            &shown,
            u32::from_le_bytes(staging_requirements[16..20].try_into().expect("four")),
            MEMORY_HOST_VISIBLE | MEMORY_HOST_COHERENT,
        )
        .expect("a host-visible type for the staging buffer");
    let staging_alloc = f.memory_allocate_info(staging_size, staging_type);
    assert_eq!(
        f.call(allocate_memory, [logical, staging_alloc, 0, memory_out]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let staging_memory = f.guest.read_u64(memory_out as GuestAddr);
    assert_eq!(
        f.call(bind_buffer, [logical, staging, staging_memory, 0]).expect("bind") as i32,
        VK_SUCCESS
    );
    assert_eq!(
        f.call_n(map_memory, &[logical, staging_memory, 0, VK_WHOLE_SIZE, 0, mapped_at])
            .expect("map") as i32,
        VK_SUCCESS
    );
    let staging_pointer = f.guest.read_u64(mapped_at as GuestAddr);
    f.guest.write_bytes(staging_pointer as GuestAddr, &texel_bytes);
    f.call(unmap_memory, [logical, staging_memory, 0, 0]).expect("unmap");

    let texture_info = f.image_info(2, 2, FORMAT_R8G8B8A8_UNORM, IMAGE_USAGE_TEXTURE);
    let texture_out = f.alloc(8);
    assert_eq!(
        f.call(create_image, [logical, texture_info, 0, texture_out]).expect("image") as i32,
        VK_SUCCESS
    );
    let texture = f.guest.read_u64(texture_out as GuestAddr);
    f.call(image_requirements, [logical, texture, requirements_at, 0]).expect("needs");
    let texture_requirements = f.read_bytes(requirements_at, MEMORY_REQUIREMENTS_BYTES);
    let texture_size = u64::from_le_bytes(texture_requirements[0..8].try_into().expect("eight"));
    let texture_type_bits =
        u32::from_le_bytes(texture_requirements[16..20].try_into().expect("four"));

    // **The forwarded half of the split**, chosen deliberately: a device-local type with no
    // `HOST_VISIBLE` bit is one this layer allocates without importing anything, and
    // `vkMapMemory` on it refuses. Falling back to any type at all keeps the test running on a
    // device whose only types are host-visible.
    let device_local_type = f
        .memory_type_index(&shown, texture_type_bits, MEMORY_DEVICE_LOCAL)
        .filter(|index| {
            let at = 4 + *index as usize * 8;
            u32::from_le_bytes(shown[at..at + 4].try_into().expect("four")) & MEMORY_HOST_VISIBLE
                == 0
        })
        .or_else(|| f.memory_type_index(&shown, texture_type_bits, 0))
        .expect("some memory type the texture can be bound to");
    let texture_alloc = f.memory_allocate_info(texture_size, device_local_type);
    assert_eq!(
        f.call(allocate_memory, [logical, texture_alloc, 0, memory_out]).expect("allocate") as i32,
        VK_SUCCESS
    );
    let texture_memory = f.guest.read_u64(memory_out as GuestAddr);
    assert_eq!(
        f.call(bind_image, [logical, texture, texture_memory, 0]).expect("bind") as i32,
        VK_SUCCESS
    );

    let texture_view_info = f.image_view_info(texture, FORMAT_R8G8B8A8_UNORM);
    let texture_view_out = f.alloc(8);
    assert_eq!(
        f.call(create_view, [logical, texture_view_info, 0, texture_view_out]).expect("view")
            as i32,
        VK_SUCCESS
    );
    let texture_view = f.guest.read_u64(texture_view_out as GuestAddr);

    let sampler_out = f.alloc(8);
    assert_eq!(
        f.call(create_sampler, [logical, f.sampler_info(), 0, sampler_out]).expect("sampler")
            as i32,
        VK_SUCCESS
    );
    let sampler = f.guest.read_u64(sampler_out as GuestAddr);

    // ------------------------------------------------------------------- descriptors and pipeline
    let set_layout_out = f.alloc(8);
    assert_eq!(
        f.call(create_set_layout, [logical, f.descriptor_set_layout_info(), 0, set_layout_out])
            .expect("set layout") as i32,
        VK_SUCCESS
    );
    let set_layout = f.guest.read_u64(set_layout_out as GuestAddr);

    let descriptor_pool_out = f.alloc(8);
    assert_eq!(
        f.call(create_descriptor_pool, [logical, f.descriptor_pool_info(), 0, descriptor_pool_out])
            .expect("descriptor pool") as i32,
        VK_SUCCESS
    );
    let descriptor_pool = f.guest.read_u64(descriptor_pool_out as GuestAddr);

    let set_out = f.alloc(8);
    let set_alloc = f.descriptor_set_allocate_info(descriptor_pool, set_layout);
    assert_eq!(
        f.call(allocate_sets, [logical, set_alloc, set_out, 0]).expect("sets") as i32,
        VK_SUCCESS
    );
    let descriptor_set = f.guest.read_u64(set_out as GuestAddr);
    assert_ne!(descriptor_set, 0);

    let write = f.write_descriptor_set(descriptor_set, sampler, texture_view);
    f.call_n(update_sets, &[logical, 1, write, 0, 0]).expect("update");

    let pipeline_layout_out = f.alloc(8);
    assert_eq!(
        f.call(
            create_pipeline_layout,
            [logical, f.pipeline_layout_info(set_layout), 0, pipeline_layout_out]
        )
        .expect("pipeline layout") as i32,
        VK_SUCCESS
    );
    let pipeline_layout = f.guest.read_u64(pipeline_layout_out as GuestAddr);

    let vertex_module_out = f.alloc(8);
    let vertex_module_info = f.shader_module_info(&TRIANGLE_VERT_SPIRV);
    assert_eq!(
        f.call(create_shader, [logical, vertex_module_info, 0, vertex_module_out])
            .expect("vertex module") as i32,
        VK_SUCCESS,
        "the driver rejected the vertex SPIR-V"
    );
    let vertex_module = f.guest.read_u64(vertex_module_out as GuestAddr);
    let fragment_module_out = f.alloc(8);
    let fragment_module_info = f.shader_module_info(&TRIANGLE_FRAG_SPIRV);
    assert_eq!(
        f.call(create_shader, [logical, fragment_module_info, 0, fragment_module_out])
            .expect("fragment module") as i32,
        VK_SUCCESS,
        "the driver rejected the fragment SPIR-V"
    );
    let fragment_module = f.guest.read_u64(fragment_module_out as GuestAddr);

    let pipeline_info =
        f.graphics_pipeline_info(pipeline_layout, render_pass, vertex_module, fragment_module);
    let pipeline_out = f.poisoned(8, 0x5A);
    let created = f
        .call_n(create_pipelines, &[logical, 0, 1, pipeline_info, 0, pipeline_out])
        .expect("the call completes");
    assert_eq!(
        created as i32, VK_SUCCESS,
        "the driver answered VkResult {} for the graphics pipeline",
        created as i32
    );
    let pipeline = f.guest.read_u64(pipeline_out as GuestAddr);
    assert_ne!(pipeline, 0, "a VK_SUCCESS must come with a pipeline, not VK_NULL_HANDLE");

    // ------------------------------------------------------------------- the frame's own objects
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

    let pool_info = f.command_pool_info(POOL_RESET_COMMAND_BUFFER, family);
    let pool_out = f.alloc(8);
    f.call(create_pool, [logical, pool_info, 0, pool_out]).expect("pool");
    let pool = f.guest.read_u64(pool_out as GuestAddr);
    let allocate_info = f.command_buffer_allocate_info(pool, 1);
    let buffers_at = f.alloc(8);
    f.call(allocate_buffers, [logical, allocate_info, buffers_at, 0]).expect("allocate");
    let command = f.guest.read_u64(buffers_at as GuestAddr);

    // --------------------------------------------------------------------------------- the frame
    let index_at = f.alloc(8);
    let acquired = f
        .call_n(acquire, &[logical, swapchain, u64::MAX, image_available, 0, index_at])
        .expect("the acquire completes");
    assert_eq!(acquired as i32, VK_SUCCESS, "the first acquire must succeed");
    let image_index = f.read_u32(index_at);
    let framebuffer = framebuffers[image_index as usize];

    let begin_info = f.begin_info(ONE_TIME_SUBMIT, 0);
    assert_eq!(f.call(begin, [command, begin_info, 0, 0]).expect("begin") as i32, VK_SUCCESS);

    // The texture upload: undefined -> transfer destination, copy, -> shader read only.
    let to_transfer = f.bytes(&f.image_barrier(
        STYPE_IMAGE_MEMORY_BARRIER,
        0,
        ACCESS_TRANSFER_WRITE,
        LAYOUT_UNDEFINED,
        LAYOUT_TRANSFER_DST,
        texture,
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
    .expect("the upload barrier records");

    let region = f.buffer_image_copy(2, 2);
    f.call_n(
        cmd_copy_to_image,
        &[command, staging, texture, u64::from(LAYOUT_TRANSFER_DST), 1, region],
    )
    .expect("the texture upload records");

    let to_sampled = f.bytes(&f.image_barrier(
        STYPE_IMAGE_MEMORY_BARRIER,
        ACCESS_TRANSFER_WRITE,
        ACCESS_SHADER_READ,
        LAYOUT_TRANSFER_DST,
        LAYOUT_SHADER_READ_ONLY,
        texture,
    ));
    f.call_n(
        barrier,
        &[
            command,
            u64::from(STAGE_TRANSFER),
            u64::from(STAGE_FRAGMENT_SHADER),
            0,
            0,
            0,
            0,
            0,
            1,
            to_sampled,
        ],
    )
    .expect("the sampling barrier records");

    // The draw.
    let begin_pass =
        f.render_pass_begin_info(render_pass, framebuffer, extent.0, extent.1, DRAW_CLEAR_COLOUR);
    f.call(cmd_begin_pass, [command, begin_pass, u64::from(SUBPASS_CONTENTS_INLINE), 0])
        .expect("the render pass begins");
    f.call(cmd_bind_pipeline, [command, u64::from(BIND_POINT_GRAPHICS), pipeline, 0])
        .expect("bind pipeline");
    let (viewport_at, scissor_at) = f.viewport_and_scissor(extent.0, extent.1);
    f.call(cmd_set_viewport, [command, 0, 1, viewport_at]).expect("viewport");
    f.call(cmd_set_scissor, [command, 0, 1, scissor_at]).expect("scissor");
    let vertex_handles = f.u64_array(&[vertex_buffer]);
    let vertex_offsets = f.u64_array(&[0]);
    f.call_n(cmd_bind_vertex, &[command, 0, 1, vertex_handles, vertex_offsets])
        .expect("bind vertex buffers");
    let sets = f.u64_array(&[descriptor_set]);
    f.call_n(
        cmd_bind_sets,
        &[command, u64::from(BIND_POINT_GRAPHICS), pipeline_layout, 0, 1, sets, 0, 0],
    )
    .expect("bind descriptor sets");
    f.call_n(cmd_draw, &[command, 3, 1, 0, 0]).expect("the draw records");
    f.call(cmd_end_pass, [command, 0, 0, 0]).expect("the render pass ends");

    assert_eq!(f.call(end, [command, 0, 0, 0]).expect("end") as i32, VK_SUCCESS);

    let submit_info =
        f.submit_info(image_available, STAGE_COLOR_ATTACHMENT_OUTPUT, command, render_finished);
    assert_eq!(f.call(submit, [queue, 1, submit_info, fence]).expect("submit") as i32, VK_SUCCESS);
    let fences_at = f.u64_array(&[fence]);
    assert_eq!(
        f.call_n(wait_fences, &[logical, 1, fences_at, 1, u64::MAX]).expect("wait") as i32,
        VK_SUCCESS
    );

    let index_array = f.u32_array(&[image_index]);
    let results_at = f.poisoned(8, 0x33);
    let present_info = f.present_info(render_finished, swapchain, index_array, results_at);
    let presented = f.call(present, [queue, present_info, 0, 0]).expect("the present completes");
    assert_eq!(presented as i32, VK_SUCCESS, "the present must succeed");

    f.call(queue_wait_idle, [queue, 0, 0, 0]).expect("queue idle");
    f.call(device_wait_idle, [logical, 0, 0, 0]).expect("device idle");

    // ------------------------------------------------------- THE EVIDENCE: the presented pixels
    let host_swapchain = f
        .vulkan()
        .swapchain_handles()
        .iter()
        .find(|(handle, _)| *handle as u64 == swapchain)
        .map(|(_, token)| *token)
        .expect("the swapchain handle is one this layer issued");
    let presented_image = host
        .read_presented_image(host_swapchain, image_index)
        .expect("the presented image must be readable");
    assert_eq!(presented_image.width, extent.0);
    assert_eq!(presented_image.height, extent.1);

    let quadrants = [
        ("top-left", extent.0 / 4, extent.1 / 4, TEXELS[0]),
        ("top-right", extent.0 * 3 / 4, extent.1 / 4, TEXELS[1]),
        ("bottom-left", extent.0 / 4, extent.1 * 3 / 4, TEXELS[2]),
        ("bottom-right", extent.0 * 3 / 4, extent.1 * 3 / 4, TEXELS[3]),
    ];
    for (which, x, y, expected) in quadrants {
        let pixel = presented_image.pixel(x, y).expect("a pixel inside the image");
        assert_eq!(
            pixel, expected,
            "the {which} quadrant of the presented frame must be the {which} texel of the 2x2 \
             texture the guest uploaded. Expected {expected:?} (R, G, B, A) at ({x}, {y}) and \
             read back {pixel:?}. This is the assertion the whole stage exists for: every \
             VkResult above could be zero with an empty frame, an un-uploaded texture or a \
             flipped UV mapping"
        );
        assert_ne!(
            pixel, DRAW_CLEAR_BYTES,
            "and it is not the colour the render pass cleared to, which is what a frame that was \
             never drawn into would be"
        );
    }

    // -------------------------------------------------------------------------------- teardown
    f.call(destroy_pipeline, [logical, pipeline, 0, 0]).expect("destroy pipeline");
    f.call(destroy_shader, [logical, vertex_module, 0, 0]).expect("destroy module");
    f.call(destroy_shader, [logical, fragment_module, 0, 0]).expect("destroy module");
    f.call(destroy_pipeline_layout, [logical, pipeline_layout, 0, 0]).expect("destroy layout");
    f.call(destroy_descriptor_pool, [logical, descriptor_pool, 0, 0]).expect("destroy pool");
    assert!(
        f.vulkan().descriptor_set_handles().is_empty(),
        "destroying the pool takes its sets' handles with it, exactly as a command pool does"
    );
    f.call(destroy_set_layout, [logical, set_layout, 0, 0]).expect("destroy set layout");
    f.call(destroy_sampler, [logical, sampler, 0, 0]).expect("destroy sampler");
    f.call(destroy_view, [logical, texture_view, 0, 0]).expect("destroy view");
    f.call(destroy_image, [logical, texture, 0, 0]).expect("destroy image");
    f.call(destroy_buffer, [logical, vertex_buffer, 0, 0]).expect("destroy buffer");
    f.call(destroy_buffer, [logical, staging, 0, 0]).expect("destroy buffer");

    let (before, peak) = f.vulkan().imported_bytes();
    for memory in [vertex_memory, staging_memory, texture_memory] {
        f.call(free_memory, [logical, memory, 0, 0]).expect("free");
    }
    let (after, _) = f.vulkan().imported_bytes();
    assert_eq!(after, 0, "every imported allocation gave its guest pages back");
    assert_eq!(f.vulkan().leaked_import_bytes(), 0, "and none were lost on the way");

    for framebuffer in &framebuffers {
        f.call(destroy_framebuffer, [logical, *framebuffer, 0, 0]).expect("destroy framebuffer");
    }
    f.call(destroy_render_pass, [logical, render_pass, 0, 0]).expect("destroy render pass");
    for view in &views {
        f.call(destroy_view, [logical, *view, 0, 0]).expect("destroy view");
    }
    f.call(destroy_semaphore, [logical, image_available, 0, 0]).expect("destroy semaphore");
    f.call(destroy_semaphore, [logical, render_finished, 0, 0]).expect("destroy semaphore");
    f.call(destroy_fence, [logical, fence, 0, 0]).expect("destroy fence");
    f.call(destroy_pool, [logical, pool, 0, 0]).expect("destroy pool");
    f.call(destroy_swapchain, [logical, swapchain, 0, 0]).expect("destroy swapchain");

    let left = host.stage_five_objects();
    assert_eq!(left.device_memories, 0, "the host let every allocation go: {left:?}");
    assert_eq!(left.buffers, 0);
    assert_eq!(left.images, 0);
    assert_eq!(left.pipelines, 0);
    assert_eq!(left.descriptor_sets, 0);

    eprintln!("\n=== stage 5 live evidence ===");
    eprintln!("host: {host:?}");
    eprintln!("chosen: \"{device_name}\", queue family {family}");
    eprintln!("    swapchain {}x{}, VkFormat {format}", extent.0, extent.1);
    eprintln!("memory types the driver reports: {shown_count}");
    eprintln!("    importable set (vkGetMemoryHostPointerPropertiesEXT): {importable:#x}");
    for rewrite in &masked {
        eprintln!("    masked: {rewrite}");
    }
    if masked.is_empty() {
        eprintln!("    masked: nothing -- every host-visible type on this device is importable");
    }
    eprintln!("    added to the device: {}", added[0]);
    eprintln!(
        "vkAllocateMemory       -> vertex {vertex_bytes} B from type {host_visible_type} \
         (imported), texture {texture_size} B from type {device_local_type} (forwarded)"
    );
    eprintln!("vkMapMemory            -> {mapped:#x}, inside GuestSpace [{:#x}, {:#x})", space.base(), space.end());
    eprintln!(
        "    guest commit charge for Vulkan: {before} B live before the frees, peak {peak} B          (each import is rounded up to minImportedHostPointerAlignment)"
    );
    eprintln!("    vkMapMemory handed the guest {} B in total", f.vulkan().mapped_bytes());
    eprintln!("vkCreateShaderModule   -> two real SPIR-V modules, {} and {} words",
        TRIANGLE_VERT_SPIRV.len(), TRIANGLE_FRAG_SPIRV.len());
    eprintln!("vkCreateGraphicsPipelines -> VkResult 0, VkPipeline {pipeline:#x}");
    eprintln!("vkCmdDraw(3, 1, 0, 0)  -> recorded; vkQueuePresentKHR -> VkResult {}", presented as i32);
    eprintln!(
        "read back {}x{} from the presented image (VkFormat {}):",
        presented_image.width, presented_image.height, presented_image.format
    );
    eprintln!("    the render pass cleared to {DRAW_CLEAR_COLOUR:?} = {DRAW_CLEAR_BYTES:?}");
    for (which, x, y, expected) in quadrants {
        eprintln!(
            "    {which:<13} ({x:>4}, {y:>4}) = {:?}   expected the texel {expected:?}",
            presented_image.pixel(x, y).expect("a pixel")
        );
    }
    eprintln!("\n{}", f.vulkan().report());
}
