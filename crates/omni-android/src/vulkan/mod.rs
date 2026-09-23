//! **The Vulkan loader the engine bootstraps itself through — opened, and measured.**
//!
//! This module implements *no graphics*. It implements the two calls that stand between
//! `libroblox.so` and a renderer, and it turns the question "which renderer does Roblox pick?"
//! into a list this layer can print.
//!
//! # The measurement this exists for
//!
//! `libEGL.so` and `libGLESv2.so` are `DT_NEEDED` of `libroblox.so`, so the GLES path is *linked*.
//! Vulkan is not linked at all: **there are zero `vk*` symbols in the binary's import table.** The
//! engine bootstraps Vulkan itself, and the sequence is decoded at guest `0x02595160`:
//!
//! ```text
//! 0x2595170: adrp/add x0, "libvulkan.so.1" ; mov w1, #2 (RTLD_NOW) ; bl dlopen
//! 0x2595180: cbnz x0, got_it               ; if that failed,
//! 0x2595184: adrp/add x0, "libvulkan.so"   ; mov w1, #2            ; bl dlopen
//! 0x2595194: cbz  x0, give_up
//! 0x2595198: adrp/add x1, "vkGetInstanceProcAddr" ; bl dlsym   -> stored at global 0x6d3ca8
//! 0x25951b8: mov x0, xzr ; adrp/add x1, "vkCreateInstance" ; blr x8
//! 0x25951c8: ...          adrp/add x1, "vkEnumerateInstanceExtensionProperties"
//! ```
//!
//! Three facts follow from those instructions, and every decision here rests on one of them.
//!
//! 1. **`dlopen` of either `soname` returning NULL is a branch the engine already has.** `cbz x0,
//!    give_up` at `0x2595194` is the compiler's own null test. So a runtime that refuses to open
//!    the loader does not produce a diagnostic — it produces a silent fall-back to whichever other
//!    renderer the engine has, and nothing anywhere records that a choice was made. That is the
//!    shape D22 refused to let a `Default` make, and it is why opening the loader comes first.
//! 2. **Every Vulkan entry point arrives through one indirect call.** `blr x8`, where `x8` is what
//!    `dlsym` returned. There is no import table to census and no relocation to watch: the *only*
//!    place the engine can name a Vulkan function is the `const char *` it passes to
//!    `vkGetInstanceProcAddr`. So that argument is the census, and this module records it.
//! 3. **The engine stores what it gets back** (`0x6d3ca8`) and calls it later. A returned pointer
//!    is therefore not a value that gets tested and dropped; it is one that gets branched to. It
//!    has to be an address the guest can `BLR`, and what happens there has to be a refusal that
//!    names the function — not a zero, and not a plausible `VK_SUCCESS`.
//!
//! # What is implemented, and what refuses
//!
//! | call | answer |
//! |---|---|
//! | `dlopen("libvulkan.so")`, `dlopen("libvulkan.so.1")` | a handle, when this module has been bound into the boundary — see [`bionic::dl`](crate::bionic) |
//! | `dlsym(handle, "vkGetInstanceProcAddr")` | that symbol's thunk address |
//! | `dlsym(handle, "vk...")`, anything else | **refused**, naming the symbol — a real `libvulkan.so` exports it and this one cannot, so NULL would be a lie |
//! | `vkGetInstanceProcAddr(NULL, name)`, `name` in [`NULL_INSTANCE_COMMANDS`] | a distinct thunk per name |
//! | `vkGetInstanceProcAddr(NULL, name)`, any other name | **NULL**, which is what the specification requires — see below |
//! | `vkGetInstanceProcAddr(instance, name)`, `instance` issued by this layer | a thunk when the **driver** has that entry point, NULL when it does not, refused when no host is attached |
//! | `vkGetInstanceProcAddr(instance, name)`, any other `instance` | **refused**: that handle was never issued here, and a host driver dereferences a `VkInstance` |
//! | calling the `vkEnumerateInstanceExtensionProperties` thunk | **forwarded** to the host driver, with the surface extension substituted and the substitution recorded |
//! | calling the `vkCreateInstance` thunk | **forwarded**: a real `VkInstance` from a real driver, or the driver's own `VkResult` |
//! | either of those with a non-null `pAllocator` | **refused**, and counted — see [`Vulkan::allocator_non_null`] |
//! | calling any other returned thunk | **refused**, naming the Vulkan function and the eight argument registers |
//!
//! # Stage 2a: what forwarding added, and the two things it is careful about
//!
//! Stage 1 implemented no Vulkan command at all. Stage 2a replaces exactly two of those refusals
//! with real forwarding, through the [`VulkanHost`] seam — `omni-gfx` implements it, this crate
//! does not depend on `omni-gfx`, and [`host`] carries the argument for why that boundary is where
//! it is. Two properties of the result are worth stating here because they are what a reviewer
//! should check first.
//!
//! **The extension list is rewritten, and every rewrite is a value.** The guest looks for
//! `VK_KHR_android_surface` and the host has something else; the engine will not proceed unless
//! the Android name is advertised, so this layer says it. That is a rename, and a rename nobody
//! recorded is the defect class Global Constraint 1 exists for — so [`Vulkan::rewrites`] is the
//! log, [`Vulkan::report`] prints it, and [`rewrite`] holds the reasoning. The substitution's
//! *host* half is supplied by the host ([`VulkanHost::platform_surface_extension`]) rather than
//! written here, because `"VK_KHR_win32_surface"` is an OS name and this crate names no OS.
//!
//! **No host pointer crosses into this crate, and no guest pointer crosses into the driver.**
//! [`VulkanHost::has_instance_proc`] answers a `bool` rather than a function pointer, and
//! [`HostInstance`] is a token rather than a `VkInstance`, so a host code address physically
//! cannot reach a guest thunk. In the other direction the `VkInstanceCreateInfo` is **decoded**
//! through [`GuestMem`](crate::GuestMem) — which is `admit`, the one copy of "may guest code touch
//! this range" — and rebuilt host-side, so the driver never dereferences a number the guest chose.
//! Global Constraint 11 is about the second of those and it is the one that is a crash rather than
//! a wrong answer.
//!
//! # The one place a NULL is returned, and why it is not a guess
//!
//! A NULL from `vkGetInstanceProcAddr` is how a caller detects an absent extension, so inventing
//! one silently disables a feature. It is returned here in exactly one case, and that case is
//! **specified rather than inferred**: the Vulkan specification's "Command Function Pointers"
//! section fixes the behaviour of `vkGetInstanceProcAddr` when `instance` is `VK_NULL_HANDLE` — it
//! returns a valid pointer for `vkEnumerateInstanceVersion`,
//! `vkEnumerateInstanceExtensionProperties`, `vkEnumerateInstanceLayerProperties`,
//! `vkCreateInstance` and `vkGetInstanceProcAddr` itself, and **NULL for every other `pName`**.
//! Those five are [`NULL_INSTANCE_COMMANDS`].
//!
//! So a null-instance lookup of `vkCreateDevice` answering NULL is not this layer declining to
//! implement something; it is the only answer a conforming loader may give, and a real
//! `libvulkan.so` on a device gives it too. What would falsify the list is a run in which the
//! engine asks for a sixth name with a null instance and then treats the NULL as a failure — the
//! census records every name asked *and the answer given*, so that run would say so in its own
//! output rather than presenting as "Vulkan initialisation failed".
//!
//! Every other refusal in this module is a refusal precisely because this layer **cannot**
//! establish that NULL is right. A non-null `VkInstance` is the clearest case: the specification
//! says a valid instance returns pointers for core and enabled-extension commands, and this layer
//! has issued no instance at all, so neither answer is available and the handle itself is the
//! thing that is wrong.
//!
//! # Why the thunks come from a pool rather than from a table of Vulkan names
//!
//! There is no list of Vulkan function names in this file, and that is deliberate: a header
//! scraped into a `static` would be this project implementing an API because its names exist.
//! What is here instead is [`MAX_PROC_SLOTS`] **anonymous** thunk slots, handed out in the order
//! the engine asks for names and bound to the name it asked for. The bound is an allocation
//! bound, not a claim about Vulkan — a request past it is a refusal naming the function, because
//! "this pool is full" must not be spelled the same way as "this Vulkan implementation does not
//! have that function".
//!
//! # The census is unconditional, and that is the point
//!
//! `VERIFICATION.md` entry 15 is about a diagnostic that could be switched off and was read as a
//! system that had stopped. [`Boundary::start_census`](crate::Boundary::start_census) is gated
//! because it sits on a ≈33 ns path taken tens of millions of times. This one is not gated,
//! because it is taken *tens of times in a process*: the engine looks up a fixed set of entry
//! points once during renderer bring-up. A gate here would buy nothing measurable and would make
//! an empty census ambiguous, which is the whole of entry 15's cost.
//!
//! It is bounded, like [`MAX_CALL_RECORDS`](crate::jni::MAX_CALL_RECORDS), and it carries its own
//! dropped count ([`Vulkan::requests_dropped`]) so that a truncated list says it is truncated.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use omni_mem::GuestAddr;
use parking_lot::Mutex;

use crate::abi::ARG_REGISTERS;
use crate::boundary::{BoundaryBuilder, ImportCall, ImportFn};
use crate::error::{AbiError, AbiResult};

pub mod chain;
mod command;
mod counted;
pub mod descriptor;
pub mod device;
mod draw;
mod handles;
pub mod host;
pub mod instance;
pub mod memory;
pub mod physical;
mod queue;
pub mod resource;
pub mod rewrite;
pub mod shader;
pub mod surface;
pub mod swapchain;
pub mod query;
mod sync;
mod view;

pub use chain::{
    flat_structure, FlatStructure, CHAIN_HEADER_BYTES, FLAT_STRUCTURES, MAX_CHAIN_LINKS,
    IMAGE_FORMAT_PROPERTIES_2_BYTES, PHYSICAL_DEVICE_FEATURES_2_BYTES,
    PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2_BYTES, STYPE_IMAGE_FORMAT_PROPERTIES_2,
    STYPE_PHYSICAL_DEVICE_FEATURES_2, STYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
};
pub use command::{
    COMMAND_BUFFER_ALLOCATE_INFO_BYTES, COMMAND_BUFFER_BEGIN_INFO_BYTES,
    COMMAND_POOL_CREATE_INFO_BYTES, IMAGE_MEMORY_BARRIER_BYTES, MAX_BARRIERS,
    MAX_COMMAND_BUFFERS_PER_CALL, MAX_CLEAR_RANGES, MEMORY_BARRIER_BYTES,
};
pub use device::{
    DEVICE_CREATE_INFO_BYTES, DEVICE_QUEUE_CREATE_INFO_BYTES, MAX_QUEUE_PRIORITIES,
    MAX_QUEUE_REQUESTS,
};
pub use handles::HANDLE_SLOT_BYTES;
pub use host::{
    Acquired, ApplicationInfo, BufferRequest, ChainLink, ColorBlendState, ComputePipelineRequest,
    DescriptorBinding, DescriptorCopy,
    DescriptorPoolRequest, DescriptorSetLayoutRequest, DescriptorWrite, DescriptorWrites,
    DeviceRequest, DriverAnswer, FramebufferRequest, GraphicsPipelineRequest, HostBuffer,
    HostCommandBuffer, HostCommandPool, HostCreatedImage, HostDescriptorPool, HostDescriptorSet,
    HostDescriptorSetLayout, HostDevice, HostDeviceMemory, HostExtension, HostFence,
    HostFramebuffer, HostImage, HostImageRef, HostImageView, HostInstance, HostPhysicalDevice,
    HostPipeline, HostPipelineCache, HostPipelineLayout, HostQueryPool, HostQueue, HostRenderPass,
    HostSampler,
    HostSemaphore, HostShaderModule, HostSurface, HostSwapchain, ImageBarrier, ImageFormatQuery,
    ImageRequest,
    ImageViewRequest, InstanceRequest, MemoryAllocation, MemoryPlan, MultisampleState,
    PipelineBarrier, PipelineLayoutRequest, PipelinesCreated, PresentRequest, Presented,
    QueryPoolRequest, QueueRequest, RenderPassBegin, RenderPassRequest, ShaderStage, Specialization, SubmitRequest,
    SubpassRequest, SurfaceCreated, SwapchainRequest, VertexInputState, ViewportState, VulkanHost,
};
pub use descriptor::{
    DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO_BYTES, DESCRIPTOR_UPDATE_TEMPLATE_ENTRY_BYTES,
    MAX_TEMPLATE_ENTRIES, STYPE_DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO, COPY_DESCRIPTOR_SET_BYTES, DESCRIPTOR_BUFFER_INFO_BYTES, DESCRIPTOR_IMAGE_INFO_BYTES,
    DESCRIPTOR_POOL_CREATE_INFO_BYTES, DESCRIPTOR_POOL_SIZE_BYTES,
    DESCRIPTOR_SET_ALLOCATE_INFO_BYTES, DESCRIPTOR_SET_LAYOUT_BINDING_BYTES,
    DESCRIPTOR_SET_LAYOUT_CREATE_INFO_BYTES, MAX_DESCRIPTOR_BINDINGS, MAX_DESCRIPTOR_COPIES,
    MAX_DESCRIPTOR_WRITES, MAX_DESCRIPTORS_PER_WRITE, MAX_IMMUTABLE_SAMPLERS, MAX_POOL_SIZES,
    MAX_SETS_PER_CALL, MAX_SETS_PER_FREE, WRITE_DESCRIPTOR_SET_BYTES,
};
pub use draw::{
    BUFFER_COPY_BYTES, BUFFER_IMAGE_COPY_BYTES, CLEAR_VALUE_BYTES,
    IMAGE_BLIT_BYTES, IMAGE_COPY_BYTES, IMAGE_SUBRESOURCE_LAYERS_BYTES, MAX_BOUND_DESCRIPTOR_SETS, MAX_CLEAR_VALUES, MAX_COPY_REGIONS,
    MAX_DYNAMIC_OFFSETS, MAX_PUSH_CONSTANT_BYTES, MAX_VERTEX_BUFFER_BINDINGS,
    RENDER_PASS_BEGIN_INFO_BYTES,
};
pub use memory::{
    HOST_PROPERTY_BITS, MAPPED_MEMORY_RANGE_BYTES, MAX_MAPPED_RANGES, MEMORY_ALLOCATE_INFO_BYTES,
    MEMORY_REQUIREMENTS_BYTES, VK_WHOLE_SIZE,
};
pub use resource::{
    BUFFER_CREATE_INFO_BYTES, IMAGE_CREATE_INFO_BYTES, MAX_RESOURCE_QUEUE_FAMILIES,
    SAMPLER_CREATE_INFO_BODY_BYTES, SAMPLER_CREATE_INFO_BYTES,
};
pub use query::{MAX_QUERY_RESULT_BYTES, QUERY_POOL_CREATE_INFO_BYTES, STYPE_QUERY_POOL_CREATE_INFO};
pub use shader::{
    ATTACHMENT_DESCRIPTION_BYTES, ATTACHMENT_REFERENCE_BYTES, COLOR_BLEND_ATTACHMENT_BYTES,
    COLOR_BLEND_STATE_BYTES, COMPUTE_PIPELINE_CREATE_INFO_BYTES, DEPTH_STENCIL_STATE_BODY_BYTES,
    DEPTH_STENCIL_STATE_BYTES, DYNAMIC_STATE_CREATE_INFO_BYTES, FRAMEBUFFER_CREATE_INFO_BYTES,
    GRAPHICS_PIPELINE_CREATE_INFO_BYTES, INPUT_ASSEMBLY_STATE_BYTES, MAX_BLEND_ATTACHMENTS,
    MAX_DYNAMIC_STATES, MAX_ENTRY_POINT_BYTES, MAX_FRAMEBUFFER_ATTACHMENTS,
    MAX_PIPELINE_CACHE_BYTES, MAX_PIPELINE_STAGES, MAX_PIPELINES_PER_CALL, MAX_PUSH_CONSTANT_RANGES,
    MAX_RENDER_PASS_ATTACHMENTS, MAX_SAMPLE_MASK_WORDS, MAX_SET_LAYOUTS, MAX_SHADER_CODE_BYTES,
    MAX_SPECIALIZATION_BYTES, MAX_SPECIALIZATION_ENTRIES, MAX_SUBPASS_DEPENDENCIES,
    MAX_SUBPASS_REFERENCES, MAX_SUBPASSES, MAX_VERTEX_ATTRIBUTES, MAX_VERTEX_BINDINGS,
    MAX_VIEWPORTS, MULTISAMPLE_STATE_BYTES, PIPELINE_CACHE_CREATE_INFO_BYTES,
    PIPELINE_LAYOUT_CREATE_INFO_BYTES, PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES,
    PUSH_CONSTANT_RANGE_BYTES, RASTERIZATION_STATE_BODY_BYTES, RASTERIZATION_STATE_BYTES,
    RECT_2D_BYTES, RENDER_PASS_CREATE_INFO_BYTES, SHADER_MODULE_CREATE_INFO_BYTES,
    SPECIALIZATION_INFO_BYTES, SPECIALIZATION_MAP_ENTRY_BYTES, SUBPASS_DEPENDENCY_BYTES,
    SUBPASS_DESCRIPTION_BYTES, TESSELLATION_STATE_BYTES, VERTEX_INPUT_ATTRIBUTE_BYTES,
    VERTEX_INPUT_BINDING_BYTES, VERTEX_INPUT_STATE_BYTES, VIEWPORT_BYTES, VIEWPORT_STATE_BYTES,
};
pub use queue::{MAX_PRESENT_SWAPCHAINS, MAX_SUBMITS, PRESENT_INFO_BYTES, SUBMIT_INFO_BYTES};
pub use swapchain::{
    MAX_SWAPCHAIN_QUEUE_FAMILIES, STYPE_SWAPCHAIN_CREATE_INFO_KHR, SWAPCHAIN_CREATE_INFO_BYTES,
    VK_ERROR_OUT_OF_DATE_KHR, VK_NOT_READY, VK_SUBOPTIMAL_KHR, VK_TIMEOUT,
};
pub use sync::{FENCE_CREATE_INFO_BYTES, MAX_FENCES_PER_CALL, SEMAPHORE_CREATE_INFO_BYTES};
pub use view::{
    COMPONENT_MAPPING_BYTES, IMAGE_SUBRESOURCE_RANGE_BYTES, IMAGE_VIEW_CREATE_INFO_BYTES,
};
pub use instance::{
    EXTENSION_PROPERTIES_BYTES, INSTANCE_REGISTRY_SYMBOL, MAX_ENABLED_NAMES, MAX_INSTANCES,
    VK_INCOMPLETE, VK_SUCCESS,
};
pub use physical::{
    FORMAT_PROPERTIES_BYTES, IMAGE_FORMAT_PROPERTIES_BYTES, PHYSICAL_DEVICE_FEATURES_BYTES, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES,
    PHYSICAL_DEVICE_PROPERTIES_BYTES, PRESENT_MODE_BYTES, QUEUE_FAMILY_PROPERTIES_BYTES,
    SURFACE_CAPABILITIES_BYTES, SURFACE_FORMAT_BYTES,
};
pub use rewrite::{Rewrite, RewriteSite, Substitution};
pub use surface::{ANDROID_SURFACE_CREATE_INFO_BYTES, STYPE_ANDROID_SURFACE_CREATE_INFO_KHR};

use handles::{Handles, HANDLE_SLOT_BYTES as SLOT};
use instance::{Instances, INSTANCE_SLOT_BYTES};

/// The extension the guest looks for, and the **guest** half of the one substitution this layer
/// makes.
///
/// A fact about the guest rather than about the host, which is why it is a constant here while
/// its partner is asked for at run time ([`VulkanHost::platform_surface_extension`]):
/// `libroblox.so` is an Android binary, Android's window-system integration extension is this one,
/// and no `cfg` or target can change that. Its partner is an OS name and this crate has none.
pub const GUEST_SURFACE_EXTENSION: &str = "VK_KHR_android_surface";

/// The **entry point** the guest looks for, and the guest half of the second substitution.
///
/// [`GUEST_SURFACE_EXTENSION`]'s partner and its standing: this crate is the Android
/// compatibility layer, Android's surface creation call is this one, and no `cfg` or target can
/// change that. Its host counterpart is an OS name and is asked for at run time
/// ([`VulkanHost::platform_surface_entry_point`]).
///
/// # Why this name needs a rule of its own, discovered by running it
///
/// Stage 3's first live run failed here, and the failure was the design telling the truth:
/// `vkGetInstanceProcAddr(instance, "vkCreateAndroidSurfaceKHR")` asks the **driver** whether it
/// has that command, and an NVIDIA driver on Windows has never heard of it —
/// [`VulkanHost::has_instance_proc`] answered `false`, this layer answered the driver's NULL, and
/// the engine would have stopped there with no Vulkan surface and no diagnostic. Advertising
/// `VK_KHR_android_surface` in the extension list and then answering NULL for the one command that
/// extension exists to provide is a worse state than not advertising it at all.
///
/// So this name is answered on **this layer's** authority rather than the driver's, conditional on
/// a fact the driver does supply: whether it has the platform call this layer would satisfy it
/// with. That keeps the answer a measurement — a host with no window-system integration still
/// answers NULL — and the substitution is recorded as [`RewriteSite::Resolved`], because a
/// function pointer handed out for a command the driver does not have is exactly the silent
/// rename Global Constraint 1 is about.
pub const GUEST_SURFACE_ENTRY_POINT: &str = "vkCreateAndroidSurfaceKHR";

/// The two `soname`s the engine tries, **in the order it tries them**.
///
/// From the disassembly in this module's documentation: `libvulkan.so.1` first at `0x2595170`,
/// then `libvulkan.so` at `0x2595184` if the first returned zero. Both are answered, because
/// answering only the second would make every run take the fall-back arm and nothing would record
/// that the first had been refused.
pub const LOADER_SONAMES: [&str; 2] = ["libvulkan.so.1", "libvulkan.so"];

/// The one symbol the engine fetches from the loader with `dlsym`.
///
/// Measured, not assumed: `0x2595198` is the only `dlsym` in that sequence, and the pointer it
/// returns is stored at the global `0x6d3ca8` and used for every later lookup.
pub const LOADER_ENTRY_POINT: &str = "vkGetInstanceProcAddr";

/// The commands `vkGetInstanceProcAddr` must return a pointer for when `instance` is
/// `VK_NULL_HANDLE`, and the complete set of them.
///
/// From the Vulkan specification's "Command Function Pointers" section, which tabulates
/// `vkGetInstanceProcAddr`'s behaviour by `instance` and `pName`: with a null instance the only
/// valid names are the four global commands plus `vkGetInstanceProcAddr` itself (the last became
/// valid in Vulkan 1.2; a 1.0-only loader answers NULL for it). **Every other name is NULL**, and
/// that is what makes the NULL this module returns a specified answer rather than a guess.
///
/// `vkEnumerateInstanceVersion` is a 1.1 command: on a 1.0 loader it is absent and NULL is
/// correct there too, which is exactly how a caller discovers the loader is 1.0. This layer
/// hands out a thunk for it, so a caller that uses it to detect the version will be told by the
/// *call* rather than by the lookup — see this module's table.
pub const NULL_INSTANCE_COMMANDS: [&str; 5] = [
    "vkEnumerateInstanceVersion",
    "vkEnumerateInstanceExtensionProperties",
    "vkEnumerateInstanceLayerProperties",
    "vkCreateInstance",
    LOADER_ENTRY_POINT,
];

/// How many distinct Vulkan entry points one instance will hand a thunk address out for.
///
/// **An allocation bound, and the number is measured rather than chosen.** Stage 1 could only ever
/// issue five of these, because a null instance admits exactly [`NULL_INSTANCE_COMMANDS`], so 64
/// was room with nothing behind it. A live `VkInstance` changes that completely: the driver
/// answers for most of the instance-level set, `vkGetDeviceProcAddr` opens the far larger
/// device-level one, and both come out of this one pool because a name must have one address
/// whichever call resolved it.
///
/// So the bound is taken from the only thing that can settle it — **the binary**. Every Vulkan
/// entry point the engine can ever ask for has to exist in `libroblox.so` as a NUL-terminated
/// string, because `vkGetInstanceProcAddr`'s `pName` is the only place the API is named at all
/// (there are zero `vk*` symbols in the import table; this module's header decodes the bootstrap
/// that proves it). MEASURED, on `lib/arm64-v8a/libroblox.so` in `Roblox-2.738.1397.apk`:
///
/// ```text
/// distinct NUL-delimited strings matching vk[A-Z]... : 592
/// ```
///
/// 640 is that number with room, and a multiple of 64 so that it is visibly the same kind of
/// number the pool started as. It is an **upper bound on what the engine can name**, not a claim
/// about Vulkan: a version of Roblox with more strings raises it, and the way that would present
/// is a refusal naming the function rather than a wrong answer.
///
/// The 641st *distinct* name is [`AbiError::Refused`] naming the function. It is deliberately not
/// a NULL: NULL means "this implementation does not have that function", and a pool running out is
/// a fact about this layer.
pub const MAX_PROC_SLOTS: usize = 640;

/// How many **function** slots [`Vulkan::bind_into`] adds to a [`BoundaryBuilder`].
///
/// A host sizing its thunk region adds this to its import count. It is
/// `1 + MAX_PROC_SLOTS`: the loader entry point, and the pool.
///
/// [`Vulkan::bind_into`] also declares five **data** symbols — the `VkInstance` registry and
/// stage 3's four — and those are deliberately **not** counted here. The two areas of a
/// [`ThunkRegion`](crate::ThunkRegion) are sized independently, so a host that added a data symbol
/// to its function-slot count would reserve a slot nothing will ever use and would still not have
/// reserved the bytes. [`REGISTRY_BYTES`] is what the data area needs — well inside the 4 KB every
/// embedding in this workspace already passes.
pub const BOUND_SYMBOLS: usize = 1 + MAX_PROC_SLOTS;

/// How many `vkGetInstanceProcAddr` requests one instance records before it starts dropping.
///
/// Bounded for [`MAX_CALL_RECORDS`](crate::jni::MAX_CALL_RECORDS)' reason: an unbounded log of a
/// call the guest controls is a guest-controlled allocation. 512 is far above the number of
/// distinct entry points any Vulkan renderer resolves, so reaching it means the engine is
/// resolving in a loop — which is itself a finding, and [`Vulkan::requests_dropped`] is how it is
/// visible instead of hidden.
pub const MAX_RECORDS: usize = 512;

/// How many extension-name substitutions one instance records before it starts dropping.
///
/// [`MAX_RECORDS`]' argument, with one difference that makes the bound matter more rather than
/// less: a rewrite log is the *evidence* that a rename happened, so a log that silently stopped
/// recording would be indistinguishable from a layer that had stopped renaming.
/// [`Vulkan::rewrites_dropped`] is what keeps those apart, and it is stated in
/// [`Vulkan::report`] whether it is zero or not.
///
/// The engine enumerates extensions a handful of times and creates one instance, so a run that
/// reaches 256 is a run doing something nobody has seen — which is itself a finding.
pub const MAX_REWRITES: usize = 256;

/// How many `VkPhysicalDevice` handles one [`Vulkan`] will issue.
///
/// **Across every instance**, not per instance, because the registry is one arena and a token
/// carries its own instance. Eight is well above the three a workstation with a discrete GPU, an
/// integrated GPU and a software rasteriser reports, and a driver with more is a refusal naming
/// this constant rather than a truncated list — a truncated list of GPUs is a plausible list of
/// GPUs, and the one dropped might be the one the engine wanted.
pub const MAX_PHYSICAL_DEVICES: usize = 8;

/// How many `VkSurfaceKHR` handles one [`Vulkan`] will hold **at once**.
///
/// A renderer needs one. Four leaves room for a loader that creates and recreates without the
/// registry being what stops it — the shape [`MAX_INSTANCES`] has, for its reason. **At once**
/// since `vkDestroySurfaceKHR` frees a slot: the Android lifecycle tears the surface down at every
/// `onSurfaceDestroyed` and makes a new one at the next `onSurfaceCreated`, so a count of every
/// surface ever made would refuse the fifth time the app came back to the foreground.
pub const MAX_SURFACES: usize = 4;

/// How many `VkDevice` handles one [`Vulkan`] will hold at once. `vkDestroyDevice` frees a slot,
/// for [`MAX_SURFACES`]' lifecycle reason: the engine tears its device down with its window.
pub const MAX_DEVICES: usize = 4;

/// How many `VkQueue` handles one [`Vulkan`] will issue.
///
/// Larger than the others because a queue is not created: it is *taken out of* a device, one per
/// `(family, index)` pair, and a renderer with a graphics queue, a present queue and a transfer
/// queue on a device that also has compute families can reach a handful. Sixteen is above every
/// arrangement this project has seen, and the registry deduplicates — asking twice for the same
/// family and index consumes one slot, not two.
pub const MAX_QUEUES: usize = 16;

/// How many `VkSwapchainKHR` handles one [`Vulkan`] will hold **at once**.
///
/// **At once**, and that is the difference stage 4 makes: this is the first family the guest
/// destroys, so [`Handles::remove`](handles::Handles::remove) frees the slot and a renderer that
/// recreates its swapchain on every resize consumes one slot for as long as each swapchain lives
/// rather than one per resize. Eight leaves room for a guest that recreates before destroying —
/// which is exactly what `oldSwapchain` is for, and which means two live at the same instant — and
/// for a second window if one is ever opened.
pub const MAX_SWAPCHAINS: usize = 8;

/// How many `VkImage` handles one [`Vulkan`] will hold at once.
///
/// A swapchain has between two and about eight images, and [`MAX_SWAPCHAINS`] of them is
/// sixty-four — which does not fit [`REGISTRY_BYTES`]' budget beside everything else. Thirty-two
/// is four full swapchains' worth of images live at once, which is more than a guest that
/// recreates one swapchain at a time can hold, and the thirty-third is a refusal naming this
/// constant rather than a handle that names nothing.
///
/// Stage 4 issues `VkImage` handles from `vkGetSwapchainImagesKHR` and from nothing else.
pub const MAX_IMAGES: usize = 32;

/// How many `VkImageView` handles one [`Vulkan`] will hold at once.
///
/// Stage 4 made one per swapchain image and sized this to match [`MAX_IMAGES`]; the engine's own
/// renderer makes views of the images it creates as well -- MEASURED, five live before its first
/// shader -- so the bound follows [`MAX_CREATED_IMAGES`] instead: every created image may carry
/// a view.
pub const MAX_IMAGE_VIEWS: usize = MAX_CREATED_IMAGES;

/// How many `VkSemaphore` handles one [`Vulkan`] will hold at once.
///
/// A frame loop needs an acquire semaphore per frame in flight and a render-finished semaphore per
/// **swapchain image** — `omni_gfx::vulkan`'s module header records why that asymmetry is the one
/// people get wrong — so a two-frame loop over an eight-image swapchain wants ten. A game engine
/// adds its upload and compute queues' own; 1,024 is room for all of them, at 16 bytes a handle.
pub const MAX_SEMAPHORES: usize = 1024;

/// How many `VkFence` handles one [`Vulkan`] will hold at once. One per frame in flight, plus
/// whatever an upload path uses -- which in a streaming engine is one per transfer in flight.
pub const MAX_FENCES: usize = 1024;

/// How many `VkCommandPool` handles one [`Vulkan`] will hold at once.
///
/// A pool is per thread per queue family, because `VkCommandPool` is externally synchronised and
/// sharing one across threads is the mistake it exists to make visible. A game engine records on
/// every worker thread; 256 is room for its pool of workers several times over.
pub const MAX_COMMAND_POOLS: usize = 256;

/// How many `VkCommandBuffer` handles one [`Vulkan`] will hold at once.
///
/// The largest of stage 4's bounds, because it is the family a renderer has most of: one per frame
/// in flight per pass per recording thread, and secondary command buffers on top.
pub const MAX_COMMAND_BUFFERS: usize = 4096;

// ------------------------------------------------- stage 5's thirteen families, and their bounds
//
// **Every one of these is an allocation bound and none of them is a claim about Vulkan.** Reaching
// one is a refusal naming the constant, because a registry that reused a live slot would silently
// alias two objects and a registry that grew without bound would be a guest-controlled allocation.
//
// **Sized for a game renderer, since the engine's own renderer is what reaches them now.** Stage
// 5 chose them for a textured triangle, against a 4 KiB-then-8 KiB data area, and MEASURED the
// first time the engine loaded its shader pack: the seventeenth live `VkShaderModule` was refused.
// A handle costs 16 bytes of data area, so the bounds are now the specification's own floors
// where it sets one (`maxMemoryAllocationCount` >= 4096, `maxSamplerAllocationCount` >= 4000 --
// an engine written for every Android device cannot count on more) and the engine's content
// otherwise, and `REQUIRED_DATA_BYTES` carries them.

/// How many `VkDeviceMemory` handles one [`Vulkan`] will hold at once.
///
/// A renderer's allocation count is the one number here that scales with *content* rather than
/// with the shape of the renderer: a device-local pool, a staging ring, and one allocation per
/// texture that has not been sub-allocated. 4,096 is the specification's floor for
/// `maxMemoryAllocationCount`, so no conformant device promises an engine fewer and an engine
/// written for all of them cannot count on more.
pub const MAX_DEVICE_MEMORIES: usize = 4096;

/// How many `VkBuffer` handles one [`Vulkan`] will hold at once: vertex, index, uniform and
/// staging buffers for a whole scene.
pub const MAX_BUFFERS: usize = 16384;

/// How many `VkImage` handles the guest **created** one [`Vulkan`] will hold at once.
///
/// Separate from [`MAX_IMAGES`], which is the swapchain's, for the reason
/// [`HostImageRef`](host::HostImageRef) gives: they are the same Vulkan type and different
/// objects, and each family having its own range of the data area is what makes a `vkDestroyImage`
/// of a swapchain image a typed refusal. A scene's textures, render targets and their mips'
/// staging -- thousands in a game.
pub const MAX_CREATED_IMAGES: usize = 8192;

/// How many `VkSampler` handles one [`Vulkan`] will hold at once.
///
/// Samplers are *shared*: a renderer makes a handful — linear-repeat, linear-clamp,
/// nearest-repeat, a shadow comparison one — and binds them to hundreds of textures. 4,000 is the
/// specification's floor for `maxSamplerAllocationCount`, which no engine can exceed anywhere.
pub const MAX_SAMPLERS: usize = 4000;

/// How many `VkShaderModule` handles one [`Vulkan`] will hold at once.
///
/// **Not a bound on how many shaders an engine has.** D8 records that Roblox ships 1,364 SPIR-V
/// modules; what this bounds is how many are *live as modules at one time*, and a module is
/// ordinarily destroyed immediately after the pipelines that use it are created — the
/// specification explicitly permits it -- **and this engine does not**: MEASURED, it creates
/// modules out of `shaders/shaders_vulkan_mobile.pack` and keeps them, and its seventeenth was
/// refused when this bound was sixteen. 2,048 holds every one of the 1,364 at once with room.
pub const MAX_SHADER_MODULES: usize = 2048;

/// How many `VkPipelineLayout` handles one [`Vulkan`] will hold at once.
pub const MAX_PIPELINE_LAYOUTS: usize = 1024;

/// How many `VkRenderPass` handles one [`Vulkan`] will hold at once.
///
/// One per distinct attachment arrangement — a forward pass, a shadow pass, a post pass — not one
/// per frame; an engine that builds them per format and sample count has dozens.
pub const MAX_RENDER_PASSES: usize = 1024;

/// How many `VkFramebuffer` handles one [`Vulkan`] will hold at once.
///
/// **One per swapchain image per render pass**, which is why this is larger than
/// [`MAX_RENDER_PASSES`]: a renderer rebuilds all of them on every resize, and two sets can be
/// live at once while the old swapchain is retired -- and off-screen targets have their own.
pub const MAX_FRAMEBUFFERS: usize = 1024;

/// How many `VkPipeline` handles one [`Vulkan`] will hold at once: one per shader pair per render
/// state an engine has met, which is thousands.
pub const MAX_PIPELINES: usize = 8192;

/// How many `VkPipelineCache` handles one [`Vulkan`] will hold at once. A renderer has one.
pub const MAX_PIPELINE_CACHES: usize = 4;

/// How many `VkDescriptorSetLayout` handles one [`Vulkan`] will hold at once.
///
/// At least [`MAX_SET_LAYOUTS`], which is how many one pipeline layout may be built from; an engine
/// has one per shader interface, so as many as [`MAX_PIPELINE_LAYOUTS`].
pub const MAX_DESCRIPTOR_SET_LAYOUTS: usize = MAX_PIPELINE_LAYOUTS;

/// How many `VkDescriptorPool` handles one [`Vulkan`] will hold at once. An engine grows a pool
/// chain as a scene needs more sets, per frame in flight.
pub const MAX_DESCRIPTOR_POOLS: usize = 1024;

/// How many `VkDescriptorSet` handles one [`Vulkan`] will hold at once.
///
/// The largest of stage 5's bounds for [`MAX_COMMAND_BUFFERS`]' reason: it is the family a
/// renderer has most of, one per material per frame in flight -- a scene's worth.
pub const MAX_DESCRIPTOR_SETS: usize = 16384;

/// How many `VkQueryPool` handles one [`Vulkan`] will hold at once. MEASURED: the engine makes
/// one, its GPU timer (`gpuTimeQueryPool`); four is room for a recreated renderer's.
pub const MAX_QUERY_POOLS: usize = 4;

/// How many `VkDescriptorUpdateTemplate` handles one [`Vulkan`] will hold at once. MEASURED: the
/// engine makes them once its shaders are loaded, one per shader interface it updates that way --
/// so as many as [`MAX_DESCRIPTOR_SET_LAYOUTS`].
pub const MAX_DESCRIPTOR_UPDATE_TEMPLATES: usize = MAX_DESCRIPTOR_SET_LAYOUTS;

/// The symbol [`Vulkan::bind_into`] declares the `VkDeviceMemory` registry under.
pub const DEVICE_MEMORY_REGISTRY_SYMBOL: &str = "vulkan::device_memories";
/// The symbol [`Vulkan::bind_into`] declares the `VkBuffer` registry under.
pub const BUFFER_REGISTRY_SYMBOL: &str = "vulkan::buffers";
/// The symbol [`Vulkan::bind_into`] declares the created-`VkImage` registry under.
pub const CREATED_IMAGE_REGISTRY_SYMBOL: &str = "vulkan::created_images";
/// The symbol [`Vulkan::bind_into`] declares the `VkSampler` registry under.
pub const SAMPLER_REGISTRY_SYMBOL: &str = "vulkan::samplers";
/// The symbol [`Vulkan::bind_into`] declares the `VkShaderModule` registry under.
pub const SHADER_MODULE_REGISTRY_SYMBOL: &str = "vulkan::shader_modules";
/// The symbol [`Vulkan::bind_into`] declares the `VkPipelineLayout` registry under.
pub const PIPELINE_LAYOUT_REGISTRY_SYMBOL: &str = "vulkan::pipeline_layouts";
/// The symbol [`Vulkan::bind_into`] declares the `VkRenderPass` registry under.
pub const RENDER_PASS_REGISTRY_SYMBOL: &str = "vulkan::render_passes";
/// The symbol [`Vulkan::bind_into`] declares the `VkFramebuffer` registry under.
pub const FRAMEBUFFER_REGISTRY_SYMBOL: &str = "vulkan::framebuffers";
/// The symbol [`Vulkan::bind_into`] declares the `VkPipeline` registry under.
pub const PIPELINE_REGISTRY_SYMBOL: &str = "vulkan::pipelines";
/// The symbol [`Vulkan::bind_into`] declares the `VkPipelineCache` registry under.
pub const PIPELINE_CACHE_REGISTRY_SYMBOL: &str = "vulkan::pipeline_caches";
/// The symbol [`Vulkan::bind_into`] declares the `VkDescriptorSetLayout` registry under.
pub const DESCRIPTOR_SET_LAYOUT_REGISTRY_SYMBOL: &str = "vulkan::descriptor_set_layouts";
/// The symbol [`Vulkan::bind_into`] declares the `VkDescriptorPool` registry under.
pub const DESCRIPTOR_POOL_REGISTRY_SYMBOL: &str = "vulkan::descriptor_pools";
/// The symbol [`Vulkan::bind_into`] declares the `VkDescriptorSet` registry under.
pub const DESCRIPTOR_SET_REGISTRY_SYMBOL: &str = "vulkan::descriptor_sets";
/// The symbol [`Vulkan::bind_into`] declares the `VkQueryPool` registry under.
pub const QUERY_POOL_REGISTRY_SYMBOL: &str = "vulkan::query_pools";
/// The symbol [`Vulkan::bind_into`] declares the `VkDescriptorUpdateTemplate` registry under.
pub const UPDATE_TEMPLATE_REGISTRY_SYMBOL: &str = "vulkan::descriptor_update_templates";

/// The symbol [`Vulkan::bind_into`] declares the `VkPhysicalDevice` registry under.
pub const PHYSICAL_DEVICE_REGISTRY_SYMBOL: &str = "vulkan::physical_devices";
/// The symbol [`Vulkan::bind_into`] declares the `VkSurfaceKHR` registry under.
pub const SURFACE_REGISTRY_SYMBOL: &str = "vulkan::surfaces";
/// The symbol [`Vulkan::bind_into`] declares the `VkDevice` registry under.
pub const DEVICE_REGISTRY_SYMBOL: &str = "vulkan::devices";
/// The symbol [`Vulkan::bind_into`] declares the `VkQueue` registry under.
pub const QUEUE_REGISTRY_SYMBOL: &str = "vulkan::queues";
/// The symbol [`Vulkan::bind_into`] declares the `VkSwapchainKHR` registry under.
pub const SWAPCHAIN_REGISTRY_SYMBOL: &str = "vulkan::swapchains";
/// The symbol [`Vulkan::bind_into`] declares the `VkImage` registry under.
pub const IMAGE_REGISTRY_SYMBOL: &str = "vulkan::images";
/// The symbol [`Vulkan::bind_into`] declares the `VkImageView` registry under.
pub const IMAGE_VIEW_REGISTRY_SYMBOL: &str = "vulkan::image_views";
/// The symbol [`Vulkan::bind_into`] declares the `VkSemaphore` registry under.
pub const SEMAPHORE_REGISTRY_SYMBOL: &str = "vulkan::semaphores";
/// The symbol [`Vulkan::bind_into`] declares the `VkFence` registry under.
pub const FENCE_REGISTRY_SYMBOL: &str = "vulkan::fences";
/// The symbol [`Vulkan::bind_into`] declares the `VkCommandPool` registry under.
pub const COMMAND_POOL_REGISTRY_SYMBOL: &str = "vulkan::command_pools";
/// The symbol [`Vulkan::bind_into`] declares the `VkCommandBuffer` registry under.
pub const COMMAND_BUFFER_REGISTRY_SYMBOL: &str = "vulkan::command_buffers";

/// Bytes of the boundary's data area every handle registry needs together.
///
/// Stated as one number because it is the number a host sizing a
/// [`ThunkRegion`](crate::ThunkRegion) has to have room for, and working it out from twelve
/// constants is how an embedding ends up 64 bytes short.
///
/// # This is a budget, and stage 5 is what made it stop fitting in 4 KiB
///
/// Every boundary in this workspace passed **4096** bytes of data area. Stage 3's five registries
/// used 576 of them; stage 4's seven more brought the total to **3,648**, with 448 left for
/// everything else, and its bounds were chosen against that number rather than in isolation.
///
/// **Stage 5 adds thirteen families and does not fit.** Device memory, buffers, created images,
/// samplers, shader modules, pipeline layouts, render passes, framebuffers, pipelines, pipeline
/// caches, descriptor set layouts, descriptor pools and descriptor sets need 232 slots between
/// them even with every bound cut to what a bring-up path can justify — 3,712 bytes against 448
/// available. There is no arrangement of thirteen families that fits in 448 bytes: that is 28
/// slots, fewer than two per family, and a registry with one slot in it refuses the second object
/// of its kind.
///
/// So **the data area every embedding passes to [`BoundaryBuilder::new`] goes from 4096 to 8192**,
/// and that is a deliberate, stated change rather than a quiet bump — the handoff asked for it to
/// be said out loud. It costs 4 KiB of guest address space per boundary and no commit charge worth
/// counting; what it buys is that stage 5 exists. The embeddings that had to change are the ones
/// that bind a `Vulkan` at all, which is the four Vulkan test harnesses, and
/// [`the_registries_fit_the_data_area_every_embedding_passes`] is what fails if a future family
/// pushes past the new number too.
///
/// # And the engine's renderer is what made 8 KiB stop fitting
///
/// MEASURED: once the engine loaded its shader pack it kept its shader modules live, and the
/// seventeenth was refused. Bounds sized for a textured triangle are not bounds for a game, so the
/// families are now sized for one (see each `MAX_*`), about 81,000 slots -- 1.2 MiB -- and the data
/// area is **2 MiB**. Every embedding takes it from [`REQUIRED_DATA_BYTES`], so none had to change;
/// it is committed with the boundary, once, and nothing else grows with it.
///
/// [`BoundaryBuilder::new`]: crate::BoundaryBuilder::new
/// [`the_registries_fit_the_data_area_every_embedding_passes`]:
///     #tests::the_registries_fit_the_data_area_every_embedding_passes
pub const REGISTRY_BYTES: usize = MAX_INSTANCES * INSTANCE_SLOT_BYTES
    + (MAX_PHYSICAL_DEVICES + MAX_SURFACES + MAX_DEVICES + MAX_QUEUES + MAX_SWAPCHAINS
        + MAX_IMAGES
        + MAX_IMAGE_VIEWS
        + MAX_SEMAPHORES
        + MAX_FENCES
        + MAX_COMMAND_POOLS
        + MAX_COMMAND_BUFFERS
        + MAX_DEVICE_MEMORIES
        + MAX_BUFFERS
        + MAX_CREATED_IMAGES
        + MAX_SAMPLERS
        + MAX_SHADER_MODULES
        + MAX_PIPELINE_LAYOUTS
        + MAX_RENDER_PASSES
        + MAX_FRAMEBUFFERS
        + MAX_PIPELINES
        + MAX_PIPELINE_CACHES
        + MAX_DESCRIPTOR_SET_LAYOUTS
        + MAX_DESCRIPTOR_POOLS
        + MAX_DESCRIPTOR_SETS
        + MAX_QUERY_POOLS
        + MAX_DESCRIPTOR_UPDATE_TEMPLATES)
        * SLOT;

/// The bytes of data area an embedding must pass to [`BoundaryBuilder::new`] for a boundary that
/// binds a [`Vulkan`].
///
/// **Stated here so an embedding has one number to copy rather than a subtraction to do.** See
/// [`REGISTRY_BYTES`] for why it went from 4096 to 8192 for stage 5, and then to 2 MiB for the
/// engine's own renderer, and what that costs. The `ndk` and `jni` data symbols come out of the same area, so
/// the margin between the two constants is the whole budget for everything that is not Vulkan.
///
/// [`BoundaryBuilder::new`]: crate::BoundaryBuilder::new
pub const REQUIRED_DATA_BYTES: usize = 2 * 1024 * 1024;

/// What a `VkPhysicalDevice` slot holds, for a reader of a memory dump. Nothing reads it back.
const PHYSICAL_DEVICE_SLOT_MAGIC: u64 = 0x004F_4D4E_5650_4400; // "\0OMNVPD\0"
/// What a `VkSurfaceKHR` slot holds. Nothing reads it back.
const SURFACE_SLOT_MAGIC: u64 = 0x004F_4D4E_5653_5200; // "\0OMNVSR\0"
/// What a `VkDevice` slot holds. Nothing reads it back.
const DEVICE_SLOT_MAGIC: u64 = 0x004F_4D4E_5644_5600; // "\0OMNVDV\0"
/// What a `VkQueue` slot holds. Nothing reads it back.
const QUEUE_SLOT_MAGIC: u64 = 0x004F_4D4E_5651_5500; // "\0OMNVQU\0"
/// What a `VkSwapchainKHR` slot holds. Nothing reads it back.
const SWAPCHAIN_SLOT_MAGIC: u64 = 0x004F_4D4E_5653_5700; // "\0OMNVSW\0"
/// What a `VkImage` slot holds. Nothing reads it back.
const IMAGE_SLOT_MAGIC: u64 = 0x004F_4D4E_5649_4D00; // "\0OMNVIM\0"
/// What a `VkImageView` slot holds. Nothing reads it back.
const IMAGE_VIEW_SLOT_MAGIC: u64 = 0x004F_4D4E_5649_5600; // "\0OMNVIV\0"
/// What a `VkSemaphore` slot holds. Nothing reads it back.
const SEMAPHORE_SLOT_MAGIC: u64 = 0x004F_4D4E_5653_4D00; // "\0OMNVSM\0"
/// What a `VkFence` slot holds. Nothing reads it back.
const FENCE_SLOT_MAGIC: u64 = 0x004F_4D4E_5646_4E00; // "\0OMNVFN\0"
/// What a `VkCommandPool` slot holds. Nothing reads it back.
const COMMAND_POOL_SLOT_MAGIC: u64 = 0x004F_4D4E_5643_5000; // "\0OMNVCP\0"
/// What a `VkCommandBuffer` slot holds. Nothing reads it back.
const COMMAND_BUFFER_SLOT_MAGIC: u64 = 0x004F_4D4E_5643_4200; // "\0OMNVCB\0"
/// What a `VkDeviceMemory` slot holds. Nothing reads it back.
const DEVICE_MEMORY_SLOT_MAGIC: u64 = 0x004F_4D4E_564D_4D00; // "\0OMNVMM\0"
/// What a `VkBuffer` slot holds. Nothing reads it back.
const BUFFER_SLOT_MAGIC: u64 = 0x004F_4D4E_5642_4600; // "\0OMNVBF\0"
/// What a created `VkImage` slot holds. Nothing reads it back. **Different from
/// [`IMAGE_SLOT_MAGIC`]**, which is a swapchain's, so that a dump says which family an address
/// belonged to without anyone having to work out the ranges.
const CREATED_IMAGE_SLOT_MAGIC: u64 = 0x004F_4D4E_5643_4900; // "\0OMNVCI\0"
/// What a `VkSampler` slot holds. Nothing reads it back.
const SAMPLER_SLOT_MAGIC: u64 = 0x004F_4D4E_5653_4100; // "\0OMNVSA\0"
/// What a `VkShaderModule` slot holds. Nothing reads it back.
const SHADER_MODULE_SLOT_MAGIC: u64 = 0x004F_4D4E_5653_4800; // "\0OMNVSH\0"
/// What a `VkPipelineLayout` slot holds. Nothing reads it back.
const PIPELINE_LAYOUT_SLOT_MAGIC: u64 = 0x004F_4D4E_5650_4C00; // "\0OMNVPL\0"
/// What a `VkRenderPass` slot holds. Nothing reads it back.
const RENDER_PASS_SLOT_MAGIC: u64 = 0x004F_4D4E_5652_5000; // "\0OMNVRP\0"
/// What a `VkFramebuffer` slot holds. Nothing reads it back.
const FRAMEBUFFER_SLOT_MAGIC: u64 = 0x004F_4D4E_5646_4200; // "\0OMNVFB\0"
/// What a `VkPipeline` slot holds. Nothing reads it back.
const PIPELINE_SLOT_MAGIC: u64 = 0x004F_4D4E_5650_4900; // "\0OMNVPI\0"
/// What a `VkPipelineCache` slot holds. Nothing reads it back.
const PIPELINE_CACHE_SLOT_MAGIC: u64 = 0x004F_4D4E_5650_4300; // "\0OMNVPC\0"
/// What a `VkDescriptorSetLayout` slot holds. Nothing reads it back.
const DESCRIPTOR_SET_LAYOUT_SLOT_MAGIC: u64 = 0x004F_4D4E_5644_4C00; // "\0OMNVDL\0"
/// What a `VkDescriptorPool` slot holds. Nothing reads it back.
const DESCRIPTOR_POOL_SLOT_MAGIC: u64 = 0x004F_4D4E_5644_5000; // "\0OMNVDP\0"
/// What a `VkDescriptorSet` slot holds. Nothing reads it back.
const DESCRIPTOR_SET_SLOT_MAGIC: u64 = 0x004F_4D4E_5644_5300; // "\0OMNVDS\0"
/// What a `VkQueryPool` slot holds. Nothing reads it back.
const QUERY_POOL_SLOT_MAGIC: u64 = 0x004F_4D4E_5651_5000; // "\0OMNVQP\0"
/// What a `VkDescriptorUpdateTemplate` slot holds. Nothing reads it back.
const UPDATE_TEMPLATE_SLOT_MAGIC: u64 = 0x004F_4D4E_5655_5400; // "\0OMNVUT\0"

/// What `vkGetInstanceProcAddr` answered for one name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProcAnswer {
    /// A guest address the engine can store and `BLR` to. It forwards, or it refuses by name.
    Thunk(GuestAddr),
    /// NULL, because the specification requires NULL for this `pName` with a **null** instance.
    ///
    /// See [`NULL_INSTANCE_COMMANDS`]. The authority is the specification's table, and this layer
    /// can state it without asking anything.
    NullPerSpecification,
    /// NULL, because the **host driver** has no entry point of that name for this instance.
    ///
    /// A separate variant from [`ProcAnswer::NullPerSpecification`] on purpose, and it is D22's
    /// argument rather than a taste: the two NULLs are produced by different authorities and only
    /// one of them is checkable from a specification. A census that spelled them the same way
    /// would leave a reader unable to say whether a missing `vkCreateAndroidSurfaceKHR` was this
    /// layer's table being short or the driver genuinely not having it — which is exactly the
    /// question stage 3 opens with.
    NullFromDriver,
    /// The call did not return at all: it refused, and the error names the function.
    Refused,
}

impl ProcAnswer {
    /// The value a `vkGet*ProcAddr` returns in `X0` for this answer.
    ///
    /// **The three non-thunk variants all produce zero, and they stay three variants anyway.**
    /// That is the whole of D22's argument in one function: the *value* the guest sees is the
    /// same, and the *fact* is not, so the collapse happens here — at the last possible moment,
    /// on the way into a register — rather than in the census, where a reader would afterwards
    /// have no way to say which authority produced the NULL.
    ///
    /// [`ProcAnswer::Refused`] never reaches a return at all: the resolvers answer `Err` for it
    /// and the boundary stops the guest. It is zero here because there is no other honest number,
    /// and because a `match` that could not be exhaustive would be worse.
    #[must_use]
    pub fn address(self) -> u64 {
        match self {
            ProcAnswer::Thunk(address) => address as u64,
            ProcAnswer::NullPerSpecification
            | ProcAnswer::NullFromDriver
            | ProcAnswer::Refused => 0,
        }
    }
}

// Written by hand rather than derived, because the payload is a **guest address** and
// `#[derive(Debug)]` prints a `usize` in decimal. A census line is read beside a disassembly, and
// `Thunk(1592425192480)` is not a number anybody can find in one.
impl core::fmt::Debug for ProcAnswer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProcAnswer::Thunk(address) => write!(f, "Thunk({address:#x})"),
            ProcAnswer::NullPerSpecification => f.write_str("NULL (per specification)"),
            ProcAnswer::NullFromDriver => f.write_str("NULL (the host driver has no such command)"),
            ProcAnswer::Refused => f.write_str("refused"),
        }
    }
}

/// Which of the two lookup functions a census entry came from.
///
/// Two named variants rather than inferring it from whether [`ProcRequest::instance`] happens to
/// be in the instance registry, for [`RewriteSite`]'s reason: values that must not be confused are
/// distinguishable only if the type can tell them apart. They are also **not** symmetrical — one
/// is asked before there is a device and one cannot be — so a census that spelled them the same
/// way would leave a reader unable to say at which point in bring-up a name was first wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProcVia {
    /// `vkGetInstanceProcAddr`. [`ProcRequest::instance`] is the `VkInstance`, zero for
    /// `VK_NULL_HANDLE`.
    Instance,
    /// `vkGetDeviceProcAddr`. [`ProcRequest::instance`] is the **`VkDevice`**, zero for
    /// `VK_NULL_HANDLE` — which the specification requires to answer NULL.
    Device,
}

/// One `vkGetInstanceProcAddr` or `vkGetDeviceProcAddr` lookup, as it happened.
#[derive(Clone)]
pub struct ProcRequest {
    /// Its position in the whole ordered sequence, counting requests that were dropped.
    ///
    /// Not the index into [`Vulkan::requests`]: once the log is full the two diverge, and an
    /// ordinal that silently renumbered would make a truncated list look complete.
    pub order: usize,
    /// Which lookup function this was.
    pub via: ProcVia,
    /// The handle the guest passed in `X0` — a `VkInstance` or a `VkDevice`, according to
    /// [`via`](ProcRequest::via). Zero is `VK_NULL_HANDLE`.
    pub instance: u64,
    /// The name the guest passed in `X1`, read out of guest memory.
    pub name: String,
    /// What it was answered with.
    pub answer: ProcAnswer,
    /// The guest address the call would have returned to — `X30`, so one instruction past the
    /// `BLR`. A diagnostic, with [`ImportCall::caller`]'s standing: it is a guest value.
    pub caller: GuestAddr,
}

impl core::fmt::Debug for ProcRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (call, handle) = match self.via {
            ProcVia::Instance => ("vkGetInstanceProcAddr", "instance"),
            ProcVia::Device => ("vkGetDeviceProcAddr", "device"),
        };
        write!(
            f,
            "[{order}] {call}({handle} = {instance:#x}, \"{name}\") -> {answer:?} \
             (from {caller:#x})",
            order = self.order,
            instance = self.instance,
            name = self.name,
            answer = self.answer,
            caller = self.caller
        )
    }
}

/// One call *through* a thunk `vkGetInstanceProcAddr` handed out.
///
/// **The other half of the measurement.** Which names the engine asks for says what it intends;
/// which one it calls first says where it actually goes, and those are different questions — a
/// renderer that resolves twelve entry points and calls none of them has failed somewhere
/// between.
#[derive(Clone)]
pub struct ProcCall {
    /// Its position in the ordered sequence of calls, counting any that were dropped.
    pub order: usize,
    /// The Vulkan function the thunk stands for, or `None` for a thunk this instance never handed
    /// out — a guest that computed the address rather than being given it.
    pub name: Option<String>,
    /// The thunk slot the guest branched to.
    pub thunk: GuestAddr,
    /// `X0`-`X7` as the guest left them. Under AAPCS64 that is the first eight integer arguments
    /// of whatever the guest thought it was calling; this layer does not know the signature, so it
    /// records the registers rather than interpreting them.
    pub args: [u64; ARG_REGISTERS as usize],
    /// `X30`, the return address. [`ImportCall::caller`]'s standing.
    pub caller: GuestAddr,
}

impl core::fmt::Debug for ProcCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let registers = self
            .args
            .iter()
            .enumerate()
            .map(|(index, value)| format!("x{index}={value:#x}"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "[{order}] {name}({registers}) through {thunk:#x} (from {caller:#x})",
            order = self.order,
            name = self.name.as_deref().unwrap_or("<a slot never handed out>"),
            thunk = self.thunk,
            caller = self.caller
        )
    }
}

/// The symbol a pool slot is bound under.
///
/// Spelled with `::` and `[]` so it cannot collide with anything in `libroblox.so`'s `.dynstr` —
/// the same argument [`Jni::install_into`](crate::jni::Jni::install_into) makes for its own slot
/// names. It is not a Vulkan name and is never shown to the guest: the refusal a pool thunk
/// produces names the Vulkan function the slot was handed out for.
#[must_use]
pub fn proc_slot_symbol(index: usize) -> String {
    format!("vulkan::proc[{index}]")
}

/// Every **function** symbol [`Vulkan::bind_into`] binds, in the order it binds them.
///
/// [`INSTANCE_REGISTRY_SYMBOL`] is declared by the same call and is not here, for the reason
/// [`BOUND_SYMBOLS`] gives: it is a data symbol, it occupies no function slot, and a host that
/// counted it would size the wrong area.
pub fn bound_symbols() -> impl Iterator<Item = String> {
    std::iter::once(LOADER_ENTRY_POINT.to_string()).chain((0..MAX_PROC_SLOTS).map(proc_slot_symbol))
}

/// Whether `name` is a symbol a `dlsym` on a Vulkan loader handle can be answered with here.
///
/// Used by [`bionic::dl`](crate::bionic)'s `dlsym`. Exactly one name, because exactly one name is
/// what the decoded bootstrap asks for — every other Vulkan function arrives through
/// `vkGetInstanceProcAddr`. A real `libvulkan.so` exports far more than this, which is why a
/// `dlsym` for another `vk*` name is refused there rather than answered NULL.
#[must_use]
pub fn loader_exports(name: &str) -> bool {
    name == LOADER_ENTRY_POINT
}

/// One Vulkan-loader instance: the thunk pool, what has been handed out of it, and the census.
///
/// Its own instance with its own activation, for [`Ndk`](crate::ndk::Ndk)'s reason: an
/// [`ImportFn`] is a bare `fn` with no user data, so per-instance state reaches a handler only
/// through a guard held across [`Boundary::run`](crate::Boundary::run). A process-wide `static`
/// would give this runtime's concurrent guest instances one shared census, and two guests whose
/// renderers were counted together is a measurement nobody can unpick afterwards.
pub struct Vulkan {
    state: Mutex<State>,
}

struct State {
    /// `vkGetInstanceProcAddr`'s own slot, set by [`Vulkan::bind_into`].
    entry: Option<GuestAddr>,
    /// The anonymous pool, in binding order.
    pool: Vec<GuestAddr>,
    /// Which Vulkan name each handed-out slot stands for.
    assigned: BTreeMap<GuestAddr, String>,
    /// The reverse, so a second lookup of one name gets the same address back.
    ///
    /// It must: the engine stores what it is given (`0x6d3ca8`) and compares pointers, and two
    /// addresses for one function would make `a == b` false for two pointers to the same thing.
    by_name: BTreeMap<String, GuestAddr>,
    requests: Vec<ProcRequest>,
    requests_dropped: usize,
    calls: Vec<ProcCall>,
    calls_dropped: usize,
    /// **Every** call through a pool thunk, counted by name -- none dropped, because there are at
    /// most `MAX_PROC_SLOTS` names. `calls` keeps the first `MAX_RECORDS` in order; this is what
    /// answers "did the engine present, and how often" after the render loop has run for minutes.
    call_counts: BTreeMap<String, u64>,
    entry_calls: u64,
    /// The real driver behind this loader, or `None` — in which case `vkCreateInstance` and
    /// `vkEnumerateInstanceExtensionProperties` refuse by name rather than inventing an answer.
    host: Option<Arc<dyn VulkanHost>>,
    /// The `VkInstance` registry, once [`Vulkan::bind_into`] has carved it out of the data area.
    instances: Option<Instances>,
    /// Stage 3's four registries, carved out of the same data area by the same call.
    ///
    /// `Option` for [`instances`](State::instances)' reason and not for a different one: an
    /// instance that was never bound into a boundary has no data area, and the refusal that
    /// produces names `Vulkan::bind_into` rather than panicking inside an import.
    /// Supports removal, driven by `vkDestroyInstance`: an instance's physical devices go with it.
    physical_devices: Option<Handles<HostPhysicalDevice>>,
    /// Supports removal: `vkDestroySurfaceKHR` frees a slot.
    surfaces: Option<Handles<HostSurface>>,
    /// Supports removal: `vkDestroyDevice` frees a slot.
    devices: Option<Handles<HostDevice>>,
    /// Supports removal, driven by `vkDestroyDevice`: a device's queues go with it.
    queues: Option<Handles<HostQueue>>,
    /// Stage 4's seven, carved out of the same data area by the same call.
    ///
    /// **Four of these support removal and three do not**, which is the one thing worth noticing
    /// about the group: `swapchains`, `image_views`, `semaphores`, `fences`, `command_pools` and
    /// `command_buffers` hold objects the guest destroys, and `images` holds objects it does not —
    /// a swapchain image is owned by its swapchain and its handle is released by
    /// `vkDestroySwapchainKHR` on the guest's behalf. [`Handles::remove`](handles::Handles::remove)
    /// says what removal costs.
    swapchains: Option<Handles<HostSwapchain>>,
    images: Option<Handles<HostImage>>,
    image_views: Option<Handles<HostImageView>>,
    semaphores: Option<Handles<HostSemaphore>>,
    fences: Option<Handles<HostFence>>,
    command_pools: Option<Handles<HostCommandPool>>,
    command_buffers: Option<Handles<HostCommandBuffer>>,
    /// Stage 5's thirteen, carved out of the same data area by the same call.
    ///
    /// **Every one of them supports removal**, unlike stage 4's mixed group: these are all objects
    /// the guest both creates and destroys, and two of them — `descriptor_sets` and, indirectly,
    /// everything allocated from a pool — are removed *on the guest's behalf* when their pool goes,
    /// exactly as `command_buffers` are.
    device_memories: Option<Handles<HostDeviceMemory>>,
    buffers: Option<Handles<HostBuffer>>,
    /// Images the **guest** created. Separate from [`images`](State::images), which is a
    /// swapchain's; [`HostImageRef`] carries the argument.
    created_images: Option<Handles<HostCreatedImage>>,
    samplers: Option<Handles<HostSampler>>,
    shader_modules: Option<Handles<HostShaderModule>>,
    pipeline_layouts: Option<Handles<HostPipelineLayout>>,
    render_passes: Option<Handles<HostRenderPass>>,
    framebuffers: Option<Handles<HostFramebuffer>>,
    pipelines: Option<Handles<HostPipeline>>,
    pipeline_caches: Option<Handles<HostPipelineCache>>,
    descriptor_set_layouts: Option<Handles<HostDescriptorSetLayout>>,
    descriptor_pools: Option<Handles<HostDescriptorPool>>,
    descriptor_sets: Option<Handles<HostDescriptorSet>>,
    query_pools: Option<Handles<HostQueryPool>>,
    update_templates: Option<Handles<descriptor::TemplateId>>,
    /// Each live template's entries, by [`descriptor::TemplateId`]: this layer's, not a host's.
    templates: BTreeMap<u64, Vec<descriptor::TemplateEntry>>,
    /// The next [`descriptor::TemplateId`] to hand out.
    next_template: u64,
    /// Each live query pool's create info, by host token: what one of its results is sized by.
    query_shapes: BTreeMap<u64, QueryPoolRequest>,
    /// The `GuestSpace` pages behind every **imported** `VkDeviceMemory`, by token.
    ///
    /// **The one piece of state in this file that owns address space.** A forwarded allocation has
    /// no entry here at all, which is exactly what `vkMapMemory` tests to decide whether it can
    /// answer. See [`memory`] for why the pages are the guest's rather than the driver's.
    imports: BTreeMap<HostDeviceMemory, ImportedMemory>,
    /// How many bytes of `GuestSpace` every live import holds, and the high-water mark. Both are
    /// diagnostics (Global Constraint 6) and the second is the one that says how close a run came
    /// to `GuestSpaceConfig::max_committed` (D15).
    imported_bytes: usize,
    imported_peak: usize,
    /// Bytes of `GuestSpace` an allocation that was never handed to the guest failed to give
    /// back. See `memory::release_guest_pages`: a counter rather than an error, because it
    /// happens on a path already carrying an answer the guest needs.
    leaked_import_bytes: usize,
    /// How many bytes `vkMapMemory` has handed the guest access to, cumulatively. A measure of
    /// streaming rather than of occupancy, which is why it only rises.
    mapped_bytes: u64,
    /// Which `VkPhysicalDevice`s have already had their memory-type rewrite recorded.
    ///
    /// `vkGetPhysicalDeviceMemoryProperties` is called repeatedly — once per allocator, and some
    /// engines call it per allocation — and the *same* masking happens every time. Recording it
    /// once per physical device keeps [`Vulkan::rewrites`] a log of decisions rather than a log of
    /// calls, which is what makes it readable; the **count** of calls is in the census either way.
    memory_rewrites_noted: BTreeSet<HostPhysicalDevice>,
    /// Every extension name this layer changed, oldest first. See [`rewrite`].
    rewrites: Vec<Rewrite>,
    rewrites_dropped: usize,
    /// The next rewrite's ordinal. Held here rather than derived from `rewrites.len()` so that a
    /// dropped rewrite still advances it — an ordinal that silently renumbered would make a
    /// truncated log look complete, which is [`ProcRequest::order`]'s argument.
    next_rewrite: usize,
    /// How many times `vkCreateInstance` was entered at all, and how many of those carried a
    /// non-null `pAllocator`. **Read as a pair**: see [`Vulkan::allocator_non_null`].
    allocator_calls: u64,
    allocator_non_null: u64,
    /// The first non-null `pAllocator` seen, so a run that hit one can point at it.
    first_allocator: Option<u64>,
    /// Which call it was passed to. Held beside the pointer rather than derived, because the
    /// three calls that take a `pAllocator` are counted together and a reader needs to know
    /// which one to go and look at.
    first_allocator_call: Option<String>,
    /// Every failing `VkResult` a driver answered with, in order, with the call that got it.
    driver_results: Vec<(String, i32)>,
}

impl core::fmt::Debug for Vulkan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("Vulkan")
            .field("handed_out", &state.by_name.len())
            .field("requests", &state.requests.len())
            .field("calls", &state.calls.len())
            .finish()
    }
}

/// The `GuestSpace` pages behind one **imported** `VkDeviceMemory`.
///
/// See [`memory`]: an allocation from a host-visible memory type is pages this layer took out of
/// the guest's own address space and handed to the driver through
/// `VkImportMemoryHostPointerInfoEXT`, so that `vkMapMemory` can answer with an address the guest
/// may store through. A forwarded allocation has no record here at all, and that absence is what
/// `vkMapMemory` tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImportedMemory {
    /// Where the pages start. **This is what `vkMapMemory` answers with**, plus the guest's offset.
    at: GuestAddr,
    /// How many bytes were mapped: the guest's `allocationSize` rounded up to
    /// `minImportedHostPointerAlignment`, which is what `vkFreeMemory` must unmap.
    len: usize,
    /// The guest's own `allocationSize`, unrounded. **The bound `vkMapMemory` checks against**,
    /// because the rounding is this layer's and the pages past `size` are not memory the guest was
    /// given.
    size: u64,
}

/// Generate the three registry methods a stage 5 handle family needs.
///
/// # Why a macro here and not thirteen hand-written copies
///
/// Stage 3 and stage 4 wrote theirs out, and at four and seven families that was the right call:
/// each one's refusal says something different about *why that family in particular* must not be
/// forwarded, and a macro that flattened those into one sentence would have thrown away the most
/// useful half of every message. Thirteen more copies is a different number. The bodies are
/// identical — look the handle up, refuse through [`wild_handle`] if it is not there, insert,
/// remove — and thirteen copies of an identical body is thirteen places for one of them to look
/// the wrong registry up, which is a defect that produces a *plausible* answer.
///
/// So the macro generates the bodies and **takes the per-family reason as a parameter**, which is
/// the part a reader needs and the part a compiler cannot check. Nothing about the messages is
/// shared except their shape, which is [`wild_handle`]'s job and was already.
///
/// The families are deliberately **not** deduplicating ([`Handles::insert_or_get`]): every one of
/// them is created by a call that makes a new object each time, and
/// [`Handles::remove`](handles::Handles::remove) documents why a family that both deduplicates and
/// has its tokens recycled would be unsound.
macro_rules! stage_five_family {
    (
        $(#[$doc:meta])*
        $token_fn:ident, $register_fn:ident, $forget_fn:ident,
        $field:ident, $token:ty, $kind:literal, $creator:literal, $why:literal
    ) => {
        $(#[$doc])*
        fn $token_fn(&self, at: &Site, call: &str, handle: u64) -> AbiResult<$token> {
            let state = self.state.lock();
            let registry = state.$field.as_ref();
            let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
            found.ok_or_else(|| {
                wild_handle(
                    at,
                    call,
                    $kind,
                    handle,
                    registry.map_or(0, Handles::live),
                    concat!($why, ". `", $creator, "` is what issues one"),
                )
            })
        }

        #[doc = concat!("Put a `", $kind, "` in the registry. Never deduplicated: each `", $creator, "` makes a new object.")]
        fn $register_fn(&self, at: &Site, token: $token) -> AbiResult<Registered> {
            let mut state = self.state.lock();
            register(at, state.$field.as_mut(), $creator, token, false)
        }

        #[doc = concat!("Free the slot a live `", $kind, "` handle names.")]
        fn $forget_fn(&self, handle: GuestAddr) -> bool {
            let mut state = self.state.lock();
            state.$field.as_mut().and_then(|h| h.remove(handle)).is_some()
        }
    };
}

impl Vulkan {
    /// A loader with nothing bound and nothing recorded.
    ///
    /// Takes no address space: unlike [`Ndk::new`](crate::ndk::Ndk::new) this owns no arena,
    /// because every identity it hands the guest is a thunk slot the boundary already reserves.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                entry: None,
                pool: Vec::new(),
                assigned: BTreeMap::new(),
                by_name: BTreeMap::new(),
                requests: Vec::new(),
                requests_dropped: 0,
                calls: Vec::new(),
                calls_dropped: 0,
                call_counts: BTreeMap::new(),
                entry_calls: 0,
                host: None,
                instances: None,
                physical_devices: None,
                surfaces: None,
                devices: None,
                queues: None,
                swapchains: None,
                images: None,
                image_views: None,
                semaphores: None,
                fences: None,
                command_pools: None,
                command_buffers: None,
                device_memories: None,
                buffers: None,
                created_images: None,
                samplers: None,
                shader_modules: None,
                pipeline_layouts: None,
                render_passes: None,
                framebuffers: None,
                pipelines: None,
                pipeline_caches: None,
                descriptor_set_layouts: None,
                descriptor_pools: None,
                descriptor_sets: None,
                query_pools: None,
                update_templates: None,
                templates: BTreeMap::new(),
                next_template: 0,
                query_shapes: BTreeMap::new(),
                imports: BTreeMap::new(),
                imported_bytes: 0,
                imported_peak: 0,
                leaked_import_bytes: 0,
                mapped_bytes: 0,
                memory_rewrites_noted: BTreeSet::new(),
                rewrites: Vec::new(),
                rewrites_dropped: 0,
                next_rewrite: 0,
                allocator_calls: 0,
                allocator_non_null: 0,
                first_allocator: None,
                first_allocator_call: None,
                driver_results: Vec::new(),
            }),
        })
    }

    /// Attach the real driver this loader forwards to.
    ///
    /// The shape [`Ndk::set_window_source`](crate::ndk::Ndk::set_window_source) has, and the same
    /// division of labour: this crate holds the mechanism and the embedding supplies the thing
    /// that can reach the OS. [`host`] carries the argument for why `omni-gfx` is not a
    /// dependency here.
    ///
    /// **A loader with no host still opens.** `dlopen`, `dlsym` and
    /// `vkGetInstanceProcAddr(NULL, ...)` answer exactly as they did in stage 1 — they are a
    /// lookup and not graphics — and it is the forwarded *calls* that refuse, naming this method.
    /// That split is deliberate: an embedding that forgot to attach a host gets a refusal that
    /// says so at the first Vulkan command, rather than a `dlopen` that silently answered NULL and
    /// sent the engine down its own no-Vulkan branch with nothing recorded.
    ///
    /// Last writer wins, and a second call replaces the first. Nothing is migrated: instances
    /// issued by the previous host stay in the registry holding **its** tokens, so a
    /// `vkGetInstanceProcAddr` on one of them afterwards asks the new host about a token it never
    /// issued, and the new host refuses by name. That is the honest outcome — it names the real
    /// mistake, which is that two drivers were attached to one loader.
    pub fn set_host(&self, host: Arc<dyn VulkanHost>) {
        self.state.lock().host = Some(host);
    }

    /// The driver attached to this loader, if one was.
    #[must_use]
    pub fn host(&self) -> Option<Arc<dyn VulkanHost>> {
        self.state.lock().host.clone()
    }

    /// Bind `vkGetInstanceProcAddr` and the thunk pool into `builder`, and return how many
    /// symbols that was ([`BOUND_SYMBOLS`]).
    ///
    /// Must be called **before** the boundary is frozen, like every other `bind_into` here: a
    /// [`Boundary`](crate::Boundary) hands out no new slots after
    /// [`finish`](BoundaryBuilder::finish), which is why the pool is pre-allocated rather than
    /// grown on demand.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the thunk region cannot hold [`BOUND_SYMBOLS`] more slots, and
    /// [`AbiError::Refused`] if this instance has already been bound — a second binding would
    /// leave the instance holding one boundary's addresses while answering for another's.
    pub fn bind_into(&self, builder: &BoundaryBuilder) -> AbiResult<usize> {
        let mut state = self.state.lock();
        if state.entry.is_some() {
            return Err(AbiError::Refused {
                symbol: "Vulkan::bind_into".to_string(),
                address: 0,
                why: "this Vulkan instance is already bound into a boundary. Binding it into a \
                      second one would leave it holding the first boundary's thunk addresses \
                      while answering `vkGetInstanceProcAddr` for the second, so the engine would \
                      be handed pointers into a region its guest does not have"
                    .to_string(),
            });
        }
        state.entry = Some(builder.bind_inline(LOADER_ENTRY_POINT, proc_addr as ImportFn)?);
        for index in 0..MAX_PROC_SLOTS {
            let at = builder.bind_inline(&proc_slot_symbol(index), proc_slot as ImportFn)?;
            state.pool.push(at);
        }
        // The `VkInstance` registry, in the boundary's **data** area rather than in an arena of
        // this module's own. `Vulkan::new` takes no address space and stage 2a does not change
        // that: `declare_data` already owns a mapped, read-write, eagerly-committed region with a
        // guest address, which is precisely what an opaque handle needs to be —
        // [`instance::Instances`] says why a small integer would not have done.
        let registry = builder.declare_data(
            INSTANCE_REGISTRY_SYMBOL,
            MAX_INSTANCES * INSTANCE_SLOT_BYTES,
            INSTANCE_SLOT_BYTES,
        )?;
        state.instances = Some(Instances::new(registry));

        // Stage 3's four, in the same data area and each in its **own** range of it. Separate
        // declarations rather than one block sliced up here, because that is what makes an
        // address in one family's range fail `index_of` for every other family: a `VkQueue`
        // passed where a `VkDevice` belongs is then a typed refusal rather than a lookup that
        // lands, which is `ndk::handles`' argument for its own per-kind ranges.
        state.physical_devices = Some(carve(
            builder,
            PHYSICAL_DEVICE_REGISTRY_SYMBOL,
            MAX_PHYSICAL_DEVICES,
            "VkPhysicalDevice",
            PHYSICAL_DEVICE_SLOT_MAGIC,
        )?);
        state.surfaces = Some(carve(
            builder,
            SURFACE_REGISTRY_SYMBOL,
            MAX_SURFACES,
            "VkSurfaceKHR",
            SURFACE_SLOT_MAGIC,
        )?);
        state.devices = Some(carve(
            builder,
            DEVICE_REGISTRY_SYMBOL,
            MAX_DEVICES,
            "VkDevice",
            DEVICE_SLOT_MAGIC,
        )?);
        state.queues =
            Some(carve(builder, QUEUE_REGISTRY_SYMBOL, MAX_QUEUES, "VkQueue", QUEUE_SLOT_MAGIC)?);

        // Stage 4's seven, each in its own range for stage 3's reason: a `VkFence` passed where a
        // `VkSemaphore` belongs is then a typed refusal rather than a lookup that lands, and the
        // two are *both* non-dispatchable 64-bit values, so there is nothing else that could tell
        // them apart. The guest can reach that mistake with two handles it was legitimately given.
        state.swapchains = Some(carve(
            builder,
            SWAPCHAIN_REGISTRY_SYMBOL,
            MAX_SWAPCHAINS,
            "VkSwapchainKHR",
            SWAPCHAIN_SLOT_MAGIC,
        )?);
        state.images =
            Some(carve(builder, IMAGE_REGISTRY_SYMBOL, MAX_IMAGES, "VkImage", IMAGE_SLOT_MAGIC)?);
        state.image_views = Some(carve(
            builder,
            IMAGE_VIEW_REGISTRY_SYMBOL,
            MAX_IMAGE_VIEWS,
            "VkImageView",
            IMAGE_VIEW_SLOT_MAGIC,
        )?);
        state.semaphores = Some(carve(
            builder,
            SEMAPHORE_REGISTRY_SYMBOL,
            MAX_SEMAPHORES,
            "VkSemaphore",
            SEMAPHORE_SLOT_MAGIC,
        )?);
        state.fences =
            Some(carve(builder, FENCE_REGISTRY_SYMBOL, MAX_FENCES, "VkFence", FENCE_SLOT_MAGIC)?);
        state.command_pools = Some(carve(
            builder,
            COMMAND_POOL_REGISTRY_SYMBOL,
            MAX_COMMAND_POOLS,
            "VkCommandPool",
            COMMAND_POOL_SLOT_MAGIC,
        )?);
        state.command_buffers = Some(carve(
            builder,
            COMMAND_BUFFER_REGISTRY_SYMBOL,
            MAX_COMMAND_BUFFERS,
            "VkCommandBuffer",
            COMMAND_BUFFER_SLOT_MAGIC,
        )?);

        // Stage 5's thirteen, each in its own range for the same reason, and with one pair where
        // it matters more than anywhere else: `created_images` and `images` hold the *same Vulkan
        // type*, so a `vkDestroyImage` of a swapchain image is a lookup that lands in neither
        // registry rather than one that lands in the wrong one.
        state.device_memories = Some(carve(
            builder,
            DEVICE_MEMORY_REGISTRY_SYMBOL,
            MAX_DEVICE_MEMORIES,
            "VkDeviceMemory",
            DEVICE_MEMORY_SLOT_MAGIC,
        )?);
        state.buffers =
            Some(carve(builder, BUFFER_REGISTRY_SYMBOL, MAX_BUFFERS, "VkBuffer", BUFFER_SLOT_MAGIC)?);
        state.created_images = Some(carve(
            builder,
            CREATED_IMAGE_REGISTRY_SYMBOL,
            MAX_CREATED_IMAGES,
            "VkImage (created)",
            CREATED_IMAGE_SLOT_MAGIC,
        )?);
        state.samplers = Some(carve(
            builder,
            SAMPLER_REGISTRY_SYMBOL,
            MAX_SAMPLERS,
            "VkSampler",
            SAMPLER_SLOT_MAGIC,
        )?);
        state.shader_modules = Some(carve(
            builder,
            SHADER_MODULE_REGISTRY_SYMBOL,
            MAX_SHADER_MODULES,
            "VkShaderModule",
            SHADER_MODULE_SLOT_MAGIC,
        )?);
        state.pipeline_layouts = Some(carve(
            builder,
            PIPELINE_LAYOUT_REGISTRY_SYMBOL,
            MAX_PIPELINE_LAYOUTS,
            "VkPipelineLayout",
            PIPELINE_LAYOUT_SLOT_MAGIC,
        )?);
        state.render_passes = Some(carve(
            builder,
            RENDER_PASS_REGISTRY_SYMBOL,
            MAX_RENDER_PASSES,
            "VkRenderPass",
            RENDER_PASS_SLOT_MAGIC,
        )?);
        state.framebuffers = Some(carve(
            builder,
            FRAMEBUFFER_REGISTRY_SYMBOL,
            MAX_FRAMEBUFFERS,
            "VkFramebuffer",
            FRAMEBUFFER_SLOT_MAGIC,
        )?);
        state.pipelines = Some(carve(
            builder,
            PIPELINE_REGISTRY_SYMBOL,
            MAX_PIPELINES,
            "VkPipeline",
            PIPELINE_SLOT_MAGIC,
        )?);
        state.pipeline_caches = Some(carve(
            builder,
            PIPELINE_CACHE_REGISTRY_SYMBOL,
            MAX_PIPELINE_CACHES,
            "VkPipelineCache",
            PIPELINE_CACHE_SLOT_MAGIC,
        )?);
        state.descriptor_set_layouts = Some(carve(
            builder,
            DESCRIPTOR_SET_LAYOUT_REGISTRY_SYMBOL,
            MAX_DESCRIPTOR_SET_LAYOUTS,
            "VkDescriptorSetLayout",
            DESCRIPTOR_SET_LAYOUT_SLOT_MAGIC,
        )?);
        state.descriptor_pools = Some(carve(
            builder,
            DESCRIPTOR_POOL_REGISTRY_SYMBOL,
            MAX_DESCRIPTOR_POOLS,
            "VkDescriptorPool",
            DESCRIPTOR_POOL_SLOT_MAGIC,
        )?);
        state.descriptor_sets = Some(carve(
            builder,
            DESCRIPTOR_SET_REGISTRY_SYMBOL,
            MAX_DESCRIPTOR_SETS,
            "VkDescriptorSet",
            DESCRIPTOR_SET_SLOT_MAGIC,
        )?);
        state.query_pools = Some(carve(
            builder,
            QUERY_POOL_REGISTRY_SYMBOL,
            MAX_QUERY_POOLS,
            "VkQueryPool",
            QUERY_POOL_SLOT_MAGIC,
        )?);
        state.update_templates = Some(carve(
            builder,
            UPDATE_TEMPLATE_REGISTRY_SYMBOL,
            MAX_DESCRIPTOR_UPDATE_TEMPLATES,
            "VkDescriptorUpdateTemplate",
            UPDATE_TEMPLATE_SLOT_MAGIC,
        )?);
        Ok(BOUND_SYMBOLS)
    }

    /// Publish this instance to the calling thread until the guard is dropped.
    #[must_use]
    pub fn activate(self: &Arc<Self>) -> VulkanActivation {
        let previous = ACTIVE
            .with(|cell| cell.borrow_mut().replace(ActiveVulkan { vulkan: Arc::clone(self) }));
        VulkanActivation { previous }
    }

    /// `vkGetInstanceProcAddr`'s thunk address, once [`bind_into`](Vulkan::bind_into) has run.
    ///
    /// What `dlsym(handle, "vkGetInstanceProcAddr")` answers with, and what a test compares
    /// against.
    #[must_use]
    pub fn entry_point(&self) -> Option<GuestAddr> {
        self.state.lock().entry
    }

    /// Every lookup this instance answered, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<ProcRequest> {
        self.state.lock().requests.clone()
    }

    /// How many lookups were not recorded because the log was full ([`MAX_RECORDS`]).
    ///
    /// **Read it beside [`requests`](Vulkan::requests) or the list is a sample.** A non-zero value
    /// means the ordered sequence has a hole in it, and the `order` field is what says where.
    #[must_use]
    pub fn requests_dropped(&self) -> usize {
        self.state.lock().requests_dropped
    }

    /// The names the engine asked for, **in order**, including the ones answered NULL.
    ///
    /// The headline measurement: this is the ordered list of Vulkan entry points the guest wants.
    /// Duplicates are kept, because a second lookup of one name is a fact about the engine.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.state.lock().requests.iter().map(|request| request.name.clone()).collect()
    }

    /// Every call *through* a handed-out thunk, oldest first. Each of them refused.
    #[must_use]
    pub fn calls(&self) -> Vec<ProcCall> {
        self.state.lock().calls.clone()
    }

    /// How many calls were not recorded because the log was full.
    #[must_use]
    pub fn calls_dropped(&self) -> usize {
        self.state.lock().calls_dropped
    }

    /// Every call through a pool thunk, counted by name, none dropped.
    #[must_use]
    pub fn call_counts(&self) -> BTreeMap<String, u64> {
        self.state.lock().call_counts.clone()
    }

    /// The **first** Vulkan function the engine actually called, as opposed to looked up.
    ///
    /// `None` when it has called none, which is a different finding from having looked none up —
    /// [`names`](Vulkan::names) is what tells the two apart.
    #[must_use]
    pub fn first_call(&self) -> Option<ProcCall> {
        self.state.lock().calls.first().cloned()
    }

    /// How many times `vkGetInstanceProcAddr` was entered at all, including calls that refused
    /// before a name could be read.
    ///
    /// **The counter that cannot disagree with itself.** `VERIFICATION.md` entry 15: a log that
    /// can be truncated, and a total charged on the same line of control flow that nothing can
    /// truncate, are a check when read as a pair.
    ///
    /// # The pairing, stated exactly, because stage 3 changed it
    ///
    /// It counts entries into **`vkGetInstanceProcAddr` alone**. The census now also holds
    /// `vkGetDeviceProcAddr` lookups ([`ProcVia::Device`]), which do not charge this, so the
    /// identity is over the instance-level rows rather than over all of them:
    ///
    /// ```text
    /// entry_calls() == requests().iter().filter(|r| r.via == ProcVia::Instance).count()
    ///                  + requests_dropped()
    /// ```
    ///
    /// for every run in which no lookup refused before reading its name, and in which no dropped
    /// row was a device-level one. That last clause is the cost of a shared bounded log and is
    /// stated rather than hidden: [`requests_dropped`](Vulkan::requests_dropped) counts both
    /// kinds, so a run that reached [`MAX_RECORDS`] can no longer close the identity exactly. A
    /// run that has dropped nothing — which is every run anything has seen — can.
    ///
    /// Charging device lookups here too was the other option and is worse: the number would stop
    /// meaning "how many times the guest entered the loader's own entry point", which is the thing
    /// it exists to be able to state.
    #[must_use]
    pub fn entry_calls(&self) -> u64 {
        self.state.lock().entry_calls
    }

    /// The thunk this instance handed out for `name`, if it handed one out.
    #[must_use]
    pub fn thunk_for(&self, name: &str) -> Option<GuestAddr> {
        self.state.lock().by_name.get(name).copied()
    }

    /// How many of the [`MAX_PROC_SLOTS`] have been handed out.
    #[must_use]
    pub fn handed_out(&self) -> usize {
        self.state.lock().by_name.len()
    }

    /// **Every extension name this layer changed, oldest first.**
    ///
    /// The rewrite log Global Constraint 1 requires, exposed exactly the way
    /// [`requests`](Vulkan::requests) exposes the census: a `Vec` a host can read at any time,
    /// without switching anything off. A test asserts on *this* rather than on the list the guest
    /// received, because a list that came out right by coincidence passes the second check and
    /// fails this one.
    ///
    /// **Read it beside [`rewrites_dropped`](Vulkan::rewrites_dropped)** or it is a sample.
    #[must_use]
    pub fn rewrites(&self) -> Vec<Rewrite> {
        self.state.lock().rewrites.clone()
    }

    /// How many rewrites were not recorded because the log was full ([`MAX_REWRITES`]).
    ///
    /// A non-zero value means names were changed that this log does not name, which is a strictly
    /// worse state than the log being empty — so it is stated in [`report`](Vulkan::report)
    /// whether it is zero or not.
    #[must_use]
    pub fn rewrites_dropped(&self) -> usize {
        self.state.lock().rewrites_dropped
    }

    /// How many times `vkCreateInstance` was entered, including the calls that refused.
    ///
    /// Charged on the first line of the handler, before anything can return early — which is what
    /// makes it and [`allocator_non_null`](Vulkan::allocator_non_null) a pair that must agree
    /// rather than two views of one write (`VERIFICATION.md` entry 15).
    #[must_use]
    pub fn allocator_calls(&self) -> u64 {
        self.state.lock().allocator_calls
    }

    /// **How many of those carried a non-null `VkAllocationCallbacks *`.**
    ///
    /// The measurement the handoff asked for before anything is built for it. Zero, read beside a
    /// non-zero [`allocator_calls`](Vulkan::allocator_calls), is the evidence that this engine
    /// passes NULL — and it is evidence rather than an assumption only because the two counters
    /// are read together: zero of zero says nothing at all.
    #[must_use]
    pub fn allocator_non_null(&self) -> u64 {
        self.state.lock().allocator_non_null
    }

    /// The first non-null `pAllocator` this instance saw, so a run that hit one can point at it.
    #[must_use]
    pub fn first_allocator(&self) -> Option<u64> {
        self.state.lock().first_allocator
    }

    /// Which call the first non-null `pAllocator` arrived at.
    ///
    /// Three calls take one now — `vkCreateInstance`, `vkCreateAndroidSurfaceKHR` and
    /// `vkCreateDevice` — and they share one pair of counters because the question is about the
    /// engine rather than about an entry point. This is what keeps the merge from losing the
    /// only thing a reader would then have to go and find by hand.
    #[must_use]
    pub fn first_allocator_call(&self) -> Option<String> {
        self.state.lock().first_allocator_call.clone()
    }

    /// Every `VkPhysicalDevice` this layer has issued: the handle the guest holds, and the host
    /// token behind it.
    #[must_use]
    pub fn physical_device_handles(&self) -> Vec<(GuestAddr, HostPhysicalDevice)> {
        let state = self.state.lock();
        state.physical_devices.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkSurfaceKHR` this layer has issued.
    ///
    /// **Read it beside [`Vulkan::rewrites`]**: a surface in this list with no
    /// [`RewriteSite::SurfaceCall`] beside it would mean one was created without the substitution
    /// being recorded, which is the state Global Constraint 1 exists to make impossible.
    #[must_use]
    pub fn surface_handles(&self) -> Vec<(GuestAddr, HostSurface)> {
        let state = self.state.lock();
        state.surfaces.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkDevice` this layer has issued.
    #[must_use]
    pub fn device_handles(&self) -> Vec<(GuestAddr, HostDevice)> {
        let state = self.state.lock();
        state.devices.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkQueue` this layer has issued.
    ///
    /// Fewer entries than there were `vkGetDeviceQueue` calls is the **expected** state, not a
    /// dropped record: the registry deduplicates, because one `(family, index)` pair is one queue.
    #[must_use]
    pub fn queue_handles(&self) -> Vec<(GuestAddr, HostQueue)> {
        let state = self.state.lock();
        state.queues.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkSwapchainKHR` this layer currently holds.
    ///
    /// **Currently**, unlike stage 3's lists, and the difference is the whole of what stage 4
    /// added to the registries: a destroyed swapchain leaves this list, so a renderer that has
    /// recreated its swapchain forty times over a resize still shows one entry. A count that only
    /// grew would have been a leak counter dressed as an inventory.
    #[must_use]
    pub fn swapchain_handles(&self) -> Vec<(GuestAddr, HostSwapchain)> {
        let state = self.state.lock();
        state.swapchains.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkImage` this layer currently holds. Swapchain images only; stage 4 makes no other.
    #[must_use]
    pub fn image_handles(&self) -> Vec<(GuestAddr, HostImage)> {
        let state = self.state.lock();
        state.images.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkImageView` this layer currently holds.
    #[must_use]
    pub fn image_view_handles(&self) -> Vec<(GuestAddr, HostImageView)> {
        let state = self.state.lock();
        state.image_views.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkSemaphore` this layer currently holds.
    #[must_use]
    pub fn semaphore_handles(&self) -> Vec<(GuestAddr, HostSemaphore)> {
        let state = self.state.lock();
        state.semaphores.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkFence` this layer currently holds.
    #[must_use]
    pub fn fence_handles(&self) -> Vec<(GuestAddr, HostFence)> {
        let state = self.state.lock();
        state.fences.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkCommandPool` this layer currently holds.
    #[must_use]
    pub fn command_pool_handles(&self) -> Vec<(GuestAddr, HostCommandPool)> {
        let state = self.state.lock();
        state.command_pools.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkCommandBuffer` this layer currently holds.
    #[must_use]
    pub fn command_buffer_handles(&self) -> Vec<(GuestAddr, HostCommandBuffer)> {
        let state = self.state.lock();
        state.command_buffers.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkBuffer` this layer currently holds.
    #[must_use]
    pub fn buffer_handles(&self) -> Vec<(GuestAddr, HostBuffer)> {
        let state = self.state.lock();
        state.buffers.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkImage` the **guest created** that this layer currently holds. Not the swapchain's;
    /// [`Vulkan::image_handles`] is that one.
    #[must_use]
    pub fn created_image_handles(&self) -> Vec<(GuestAddr, HostCreatedImage)> {
        let state = self.state.lock();
        state.created_images.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkPipeline` this layer currently holds.
    #[must_use]
    pub fn pipeline_handles(&self) -> Vec<(GuestAddr, HostPipeline)> {
        let state = self.state.lock();
        state.pipelines.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkDescriptorSet` this layer currently holds.
    ///
    /// **Falls without the guest asking**, like `VkCommandBuffer`: destroying or resetting a
    /// descriptor pool frees every set in it, and this layer drops their handles in the same call.
    #[must_use]
    pub fn descriptor_set_handles(&self) -> Vec<(GuestAddr, HostDescriptorSet)> {
        let state = self.state.lock();
        state.descriptor_sets.as_ref().map(|h| h.iter().collect()).unwrap_or_default()
    }

    /// Every `VkInstance` this layer has issued: the handle the guest holds, and the host token
    /// behind it.
    ///
    /// The host token is a [`HostInstance`] and not a driver pointer — [`host`] says why nothing
    /// here can be one.
    #[must_use]
    pub fn instance_handles(&self) -> Vec<(GuestAddr, HostInstance)> {
        let state = self.state.lock();
        state.instances.as_ref().map(|i| i.iter().collect()).unwrap_or_default()
    }

    /// Every failing `VkResult` a driver answered with, in order, with the call that got it.
    ///
    /// **The half of the record that would otherwise be invisible.** A forwarded call that
    /// returned `VK_ERROR_INCOMPATIBLE_DRIVER` produced no refusal, no thunk and no rewrite — the
    /// guest simply got a number — so without this a run in which the driver declined everything
    /// and a run in which it succeeded look identical from the host side.
    #[must_use]
    pub fn driver_failures(&self) -> Vec<(String, i32)> {
        self.state.lock().driver_results.clone()
    }

    /// Everything this instance recorded, as lines a gate can print.
    ///
    /// **Reading it switches nothing off.** `VERIFICATION.md` entry 15 is about
    /// `Boundary::report()` stopping the census in order to print a stable snapshot, after which
    /// every later reading was of a counter that was not running — and read exactly like a system
    /// that had stopped. There is nothing to stop here: this census has no flag, for the reason
    /// the module documentation gives, so a report taken in the middle of a run costs the run
    /// nothing and the next line recorded still lands.
    ///
    /// It always states the two counts and the two dropped counts, including when they are zero,
    /// so that "nothing was asked for" is a sentence in the output rather than an absence of
    /// output.
    #[must_use]
    pub fn report(&self) -> String {
        let state = self.state.lock();
        let mut out = String::new();
        out.push_str(&format!(
            "Vulkan loader: {} lookup(s) recorded ({} dropped, {} entries into \
             vkGetInstanceProcAddr), {} of {MAX_PROC_SLOTS} thunk slots handed out, {} call(s) \
             through them ({} dropped)\n",
            state.requests.len(),
            state.requests_dropped,
            state.entry_calls,
            state.by_name.len(),
            state.calls.len(),
            state.calls_dropped,
        ));
        if state.requests.is_empty() {
            out.push_str("  no Vulkan entry point was asked for in this run\n");
        }
        for request in &state.requests {
            out.push_str(&format!("  {request:?}\n"));
        }
        if state.calls.is_empty() {
            out.push_str("  no returned thunk was called\n");
        }
        for call in &state.calls {
            out.push_str(&format!("  called {call:?}\n"));
        }
        // Every call, by name and count, most frequent first: the part of the census that
        // survives a long run.
        let mut counted: Vec<(&String, &u64)> = state.call_counts.iter().collect();
        counted.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        out.push_str(&format!("  calls by name, all of them: {counted:?}\n"));

        // **The rewrite log, always, including when it is empty.** A rename nobody recorded is the
        // defect Global Constraint 1 names, and a report that printed nothing when nothing was
        // renamed would be indistinguishable from one whose logging had been removed.
        out.push_str(&format!(
            "  extension-name substitutions: {} recorded ({} dropped)\n",
            state.rewrites.len(),
            state.rewrites_dropped,
        ));
        if state.rewrites.is_empty() {
            out.push_str("    no extension name was substituted in this run\n");
        }
        for rewrite in &state.rewrites {
            out.push_str(&format!("    {rewrite}\n"));
        }

        // The `pAllocator` measurement, as a pair. Zero of zero is not evidence and the line says
        // so in words rather than leaving a reader to notice.
        out.push_str(&match (state.allocator_calls, state.allocator_non_null) {
            (0, _) => "  pAllocator: no call that takes one was ever entered, so nothing was \
                       observed\n"
                .to_string(),
            (calls, 0) => format!(
                "  pAllocator: NULL in all {calls} call(s) that take one -- this engine passes \
                 no allocation callbacks\n"
            ),
            (calls, non_null) => format!(
                "  pAllocator: NON-NULL in {non_null} of {calls} call(s) that take one, first at \
                 {first:#x} in `{call}` -- a re-entry trampoline is now a measured requirement\n",
                first = state.first_allocator.unwrap_or(0),
                call = state.first_allocator_call.as_deref().unwrap_or("an unrecorded call")
            ),
        });

        out.push_str(&format!(
            "  VkInstance handles issued: {} of {MAX_INSTANCES}\n",
            state.instances.as_ref().map_or(0, Instances::live)
        ));
        for (handle, token) in state.instances.iter().flat_map(Instances::iter) {
            out.push_str(&format!("    {handle:#x} -> {token:?}\n"));
        }

        // **Stage 3's four registries, always, including when they are empty.** A run that
        // created no surface and a run whose surface reporting was removed must not print the
        // same thing — `VERIFICATION.md` entry 15's shape, applied to a handle count rather than
        // to a census.
        out.push_str(&format!(
            "  VkPhysicalDevice handles issued: {} of {MAX_PHYSICAL_DEVICES}\n",
            state.physical_devices.as_ref().map_or(0, Handles::live)
        ));
        for (handle, token) in state.physical_devices.iter().flat_map(|h| h.iter()) {
            out.push_str(&format!("    {handle:#x} -> {token:?}\n"));
        }
        out.push_str(&format!(
            "  VkSurfaceKHR handles issued: {} of {MAX_SURFACES}\n",
            state.surfaces.as_ref().map_or(0, Handles::live)
        ));
        for (handle, token) in state.surfaces.iter().flat_map(|h| h.iter()) {
            out.push_str(&format!("    {handle:#x} -> {token:?}\n"));
        }
        out.push_str(&format!(
            "  VkDevice handles issued: {} of {MAX_DEVICES}\n",
            state.devices.as_ref().map_or(0, Handles::live)
        ));
        for (handle, token) in state.devices.iter().flat_map(|h| h.iter()) {
            out.push_str(&format!("    {handle:#x} -> {token:?}\n"));
        }
        out.push_str(&format!(
            "  VkQueue handles issued: {} of {MAX_QUEUES} (deduplicated: one per family and \
             index)\n",
            state.queues.as_ref().map_or(0, Handles::live)
        ));
        for (handle, token) in state.queues.iter().flat_map(|h| h.iter()) {
            out.push_str(&format!("    {handle:#x} -> {token:?}\n"));
        }

        // **Stage 4's seven, as "live of bound" rather than "issued of bound".** These are the
        // first families the guest destroys, so the number here falls as well as rises, and a
        // reader comparing it against the number of `vkCreate*` calls in the census is comparing
        // two different things on purpose: the difference between them is how many objects the
        // guest has cleaned up, which is the thing a leak would show in.
        out.push_str(&format!(
            "  stage 4 handles live now (they fall as well as rise; the guest destroys these):\n    \
             VkSwapchainKHR {}/{MAX_SWAPCHAINS}, VkImage {}/{MAX_IMAGES}, \
             VkImageView {}/{MAX_IMAGE_VIEWS}, VkSemaphore {}/{MAX_SEMAPHORES}, \
             VkFence {}/{MAX_FENCES}, VkCommandPool {}/{MAX_COMMAND_POOLS}, \
             VkCommandBuffer {}/{MAX_COMMAND_BUFFERS}\n",
            state.swapchains.as_ref().map_or(0, Handles::live),
            state.images.as_ref().map_or(0, Handles::live),
            state.image_views.as_ref().map_or(0, Handles::live),
            state.semaphores.as_ref().map_or(0, Handles::live),
            state.fences.as_ref().map_or(0, Handles::live),
            state.command_pools.as_ref().map_or(0, Handles::live),
            state.command_buffers.as_ref().map_or(0, Handles::live),
        ));
        for (handle, token) in state.swapchains.iter().flat_map(|h| h.iter()) {
            out.push_str(&format!("    {handle:#x} -> {token:?}\n"));
        }

        // **Stage 5's thirteen, and the two numbers that are not handle counts.** The registries
        // are inventories like stage 4's; the memory figures are what D15 asked for, because
        // host-visible Vulkan memory is guest commit charge now and the peak is how close this run
        // came to `GuestSpaceConfig::max_committed`.
        out.push_str(&format!(
            "  stage 5 handles live now:\n    \
             VkDeviceMemory {}/{MAX_DEVICE_MEMORIES}, VkBuffer {}/{MAX_BUFFERS}, \
             VkImage(created) {}/{MAX_CREATED_IMAGES}, VkSampler {}/{MAX_SAMPLERS}, \
             VkShaderModule {}/{MAX_SHADER_MODULES}, \
             VkPipelineLayout {}/{MAX_PIPELINE_LAYOUTS}, VkRenderPass {}/{MAX_RENDER_PASSES}, \
             VkFramebuffer {}/{MAX_FRAMEBUFFERS}, VkPipeline {}/{MAX_PIPELINES}, \
             VkPipelineCache {}/{MAX_PIPELINE_CACHES}, \
             VkDescriptorSetLayout {}/{MAX_DESCRIPTOR_SET_LAYOUTS}, \
             VkDescriptorPool {}/{MAX_DESCRIPTOR_POOLS}, \
             VkDescriptorSet {}/{MAX_DESCRIPTOR_SETS}, VkQueryPool {}/{MAX_QUERY_POOLS}, \
             VkDescriptorUpdateTemplate {}/{MAX_DESCRIPTOR_UPDATE_TEMPLATES}\n",
            state.device_memories.as_ref().map_or(0, Handles::live),
            state.buffers.as_ref().map_or(0, Handles::live),
            state.created_images.as_ref().map_or(0, Handles::live),
            state.samplers.as_ref().map_or(0, Handles::live),
            state.shader_modules.as_ref().map_or(0, Handles::live),
            state.pipeline_layouts.as_ref().map_or(0, Handles::live),
            state.render_passes.as_ref().map_or(0, Handles::live),
            state.framebuffers.as_ref().map_or(0, Handles::live),
            state.pipelines.as_ref().map_or(0, Handles::live),
            state.pipeline_caches.as_ref().map_or(0, Handles::live),
            state.descriptor_set_layouts.as_ref().map_or(0, Handles::live),
            state.descriptor_pools.as_ref().map_or(0, Handles::live),
            state.descriptor_sets.as_ref().map_or(0, Handles::live),
            state.query_pools.as_ref().map_or(0, Handles::live),
            state.update_templates.as_ref().map_or(0, Handles::live),
        ));
        out.push_str(&format!(
            "  guest memory imported for Vulkan: {} live import(s) holding {} byte(s), peak {} \
             byte(s); {} byte(s) handed to the guest through vkMapMemory\n",
            state.imports.len(),
            state.imported_bytes,
            state.imported_peak,
            state.mapped_bytes,
        ));
        // Stated whether it is zero or not: a leak nobody counted reads exactly like no leak.
        out.push_str(&match state.leaked_import_bytes {
            0 => "  every imported allocation this run unwound gave its guest pages back\n"
                .to_string(),
            bytes => format!(
                "  **{bytes} byte(s) of guest address space were taken and not given back** by an \
                 allocation that was never handed to the guest; see `memory::release_guest_pages`\n"
            ),
        });

        for (call, result) in &state.driver_results {
            out.push_str(&format!("  the driver answered `{call}` with VkResult {result}\n"));
        }
        out
    }

    /// This instance, as something a created guest thread carries.
    ///
    /// **An embedding with a `Vulkan` must pass this to
    /// [`ThreadHost::with_instance`](crate::bionic::ThreadHost::with_instance).** The engine's
    /// renderer bring-up is not on the thread that called `initializeNativeCode`: it is on the
    /// game thread `GameActivity_onCreate` spawns, so a `Vulkan` published only to the calling
    /// thread would be absent exactly where it is needed — which is the failure
    /// [`Ndk::thread_instance`](crate::ndk::Ndk::thread_instance) records having already cost this
    /// project one debugging session.
    #[must_use]
    pub fn thread_instance(self: &Arc<Self>) -> Arc<dyn crate::bionic::ThreadLocalInstance> {
        Arc::new(VulkanThreadInstance(Arc::clone(self)))
    }

    // ------------------------------------------------------------------ internals

    /// The driver behind this loader, or a refusal naming what an embedding must do.
    fn require_host(&self, at: &Site) -> AbiResult<Arc<dyn VulkanHost>> {
        self.state.lock().host.clone().ok_or_else(|| {
            at.refuse(
                "this Vulkan loader has no host driver attached, so there is nothing to forward \
                 to. `Vulkan::set_host` is what an embedding calls with an implementation of \
                 `omni_android::vulkan::VulkanHost` -- `omni_gfx::host::GfxVulkanHost` is the one \
                 this workspace ships. Answering `VK_SUCCESS` here would be reporting an instance \
                 that no driver ever made, and answering `VK_ERROR_INITIALIZATION_FAILED` would \
                 be blaming a driver that was never asked"
                    .to_string(),
            )
        })
    }

    /// Apply the advertised-direction substitution and record every name it changed.
    ///
    /// Under one lock, because the ordinal and the log have to stay consistent with each other:
    /// two threads enumerating at once must not be able to produce two rewrites with the same
    /// `order`.
    fn advertise_extensions(
        &self,
        host: &Arc<dyn VulkanHost>,
        driver: &[HostExtension],
        caller: GuestAddr,
    ) -> AbiResult<Vec<HostExtension>> {
        // Asked outside this instance's lock, because it is a call into the embedding.
        let substitution = self.substitution(host)?;
        let mut state = self.state.lock();
        let mut order = state.next_rewrite;
        let (shown, rewrites) = rewrite::apply_advertised(driver, &substitution, caller, &mut order);
        state.record_rewrites(order, rewrites);
        Ok(shown)
    }

    /// Apply the enabled-direction substitution and record every name it changed.
    fn enable_extensions(
        &self,
        host: &Arc<dyn VulkanHost>,
        requested: &[String],
        caller: GuestAddr,
    ) -> AbiResult<Vec<String>> {
        let substitution = self.substitution(host)?;
        let mut state = self.state.lock();
        let mut order = state.next_rewrite;
        let (sent, rewrites) = rewrite::apply_enabled(requested, &substitution, caller, &mut order);
        state.record_rewrites(order, rewrites);
        Ok(sent)
    }

    /// The one substitution: the guest's name, and whatever the host said stands in for it.
    fn substitution(&self, host: &Arc<dyn VulkanHost>) -> AbiResult<Substitution> {
        Ok(Substitution {
            guest: GUEST_SURFACE_EXTENSION.to_string(),
            host: host.platform_surface_extension()?,
        })
    }

    /// Charge one entry into a call that **takes** a `pAllocator`, and whether it carried one.
    ///
    /// Stage 2a charged this for `vkCreateInstance` alone, because that was the only such call
    /// this layer implemented. Stage 3 adds `vkCreateAndroidSurfaceKHR` and `vkCreateDevice`, and
    /// they are counted in the same pair rather than in three — the question the counter exists
    /// to answer is "does this engine pass allocation callbacks?", which is a fact about the
    /// engine and not about any one entry point.
    /// [`Vulkan::first_allocator_call`] is what says **where** the first non-null one was, so the
    /// merge loses nothing a reader needs.
    fn note_allocator(&self, call: &str, allocator: u64) {
        let mut state = self.state.lock();
        state.allocator_calls += 1;
        if allocator != 0 {
            state.allocator_non_null += 1;
            if state.first_allocator.is_none() {
                state.first_allocator = Some(allocator);
                state.first_allocator_call = Some(call.to_string());
            }
        }
    }

    /// Record that `vkCreateAndroidSurfaceKHR` was satisfied by a different platform's call.
    ///
    /// **The third rewrite, and the only one that changes an object rather than a string.**
    /// [`surface`] carries the argument; what matters here is that it goes in the *same* log as
    /// the two extension renames, with the same ordinal sequence, so that a reader following the
    /// substitution from "the extension was advertised" to "the surface was created" reads three
    /// consecutive lines rather than two lines and an absence.
    fn note_surface_substitution(
        &self,
        host_call: &str,
        window: omni_platform::window::RawWindow,
        caller: GuestAddr,
    ) {
        let mut state = self.state.lock();
        let order = state.next_rewrite;
        state.next_rewrite += 1;
        let rewrite = Rewrite {
            order,
            site: RewriteSite::SurfaceCall { system: window.system_name() },
            from: "vkCreateAndroidSurfaceKHR".to_string(),
            to: host_call.to_string(),
            spec_version: None,
            caller,
        };
        state.record_rewrites(order + 1, vec![rewrite]);
    }

    /// The [`HostInstance`] a guest `VkInstance` names, or a typed refusal naming the call.
    fn instance_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostInstance> {
        let state = self.state.lock();
        let found = GuestAddr::try_from(handle)
            .ok()
            .and_then(|address| state.instances.as_ref().and_then(|i| i.get(address)));
        found.ok_or_else(|| {
            let issued = state.instances.as_ref().map_or(0, Instances::live);
            at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with {handle:#x} as its `VkInstance`, \
                 and that is not a handle this layer issued, or is one `vkDestroyInstance` has \
                 taken back -- it holds {issued} live one(s), each on a \
                 {INSTANCE_SLOT_BYTES}-byte boundary of its registry. A `VkInstance` is \
                 a dispatchable handle a host driver dereferences, so forwarding this one would \
                 be dereferencing a number the guest chose (Global Constraint 11). \
                 `Vulkan::instance_handles()` is the list of the ones that are real",
                caller = at.caller
            ))
        })
    }

    /// The [`HostPhysicalDevice`] a guest `VkPhysicalDevice` names, or a typed refusal.
    fn physical_device_token(
        &self,
        at: &Site,
        call: &str,
        handle: u64,
    ) -> AbiResult<HostPhysicalDevice> {
        let state = self.state.lock();
        let registry = state.physical_devices.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkPhysicalDevice", handle, registry.map_or(0, Handles::live),
                "it is dispatchable, so a host driver dereferences it. \
                 `vkEnumeratePhysicalDevices` is the only thing that issues one, and a guest that \
                 has not called it holds no valid handle at all")
        })
    }

    /// The [`HostSurface`] a guest `VkSurfaceKHR` names, or a typed refusal.
    fn surface_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostSurface> {
        let state = self.state.lock();
        let registry = state.surfaces.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkSurfaceKHR", handle, registry.map_or(0, Handles::live),
                "it is *non*-dispatchable, so passing it through would not crash anything -- it \
                 would reach the driver as a plausible surface, and the first thing to notice \
                 would be a swapchain presenting into a window nobody chose. \
                 `vkCreateAndroidSurfaceKHR` is what issues one")
        })
    }

    /// The [`HostDevice`] a guest `VkDevice` names, or a typed refusal.
    fn device_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostDevice> {
        let state = self.state.lock();
        let registry = state.devices.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkDevice", handle, registry.map_or(0, Handles::live),
                "it is dispatchable, so a host driver dereferences it. `vkCreateDevice` is the \
                 only thing that issues one")
        })
    }

    /// Put a swapchain in the registry. Never deduplicated: each `vkCreateSwapchainKHR` makes one.
    fn register_swapchain(&self, at: &Site, token: HostSwapchain) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.swapchains.as_mut(), "vkCreateSwapchainKHR", token, false)
    }

    /// Put a swapchain image in the registry, or recover the handle it already has.
    ///
    /// **Deduplicated**, and the specification requires it: `vkGetSwapchainImagesKHR` is called
    /// twice — once for the count, once for the array — and a renderer that built one
    /// `VkImageView` per image from the second call and then indexed those views by the
    /// `imageIndex` `vkAcquireNextImageKHR` answers with would be indexing a list built from
    /// handles that must be the same ones. Two handles for one image would still *work* for the
    /// view, and would fail at the barrier, which names a different image from the one the view
    /// covers.
    fn register_image(&self, at: &Site, token: HostImage) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.images.as_mut(), "vkGetSwapchainImagesKHR", token, true)
    }

    /// Put an image view in the registry.
    fn register_image_view(&self, at: &Site, token: HostImageView) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.image_views.as_mut(), "vkCreateImageView", token, false)
    }

    /// Put a semaphore in the registry.
    fn register_semaphore(&self, at: &Site, token: HostSemaphore) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.semaphores.as_mut(), "vkCreateSemaphore", token, false)
    }

    /// Put a fence in the registry.
    fn register_fence(&self, at: &Site, token: HostFence) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.fences.as_mut(), "vkCreateFence", token, false)
    }

    /// Put a command pool in the registry.
    fn register_command_pool(&self, at: &Site, token: HostCommandPool) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.command_pools.as_mut(), "vkCreateCommandPool", token, false)
    }

    /// Put a command buffer in the registry.
    fn register_command_buffer(
        &self,
        at: &Site,
        token: HostCommandBuffer,
    ) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.command_buffers.as_mut(), "vkAllocateCommandBuffers", token, false)
    }

    /// Free the slot a guest handle names in one of stage 4's registries.
    ///
    /// Answers whether a slot was freed, which the destroy handlers use only for a diagnostic: the
    /// handle was validated before the driver was asked, so a `false` here means another thread
    /// destroyed the same object between the two, and that is a guest bug the specification
    /// already calls undefined behaviour rather than something this layer can repair.
    fn forget_swapchain(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.swapchains.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    /// Drop the `VkImage` handles that belonged to a swapchain that has just been destroyed, and
    /// answer how many.
    ///
    /// **Not something the guest asked for, and the alternative is worse.** A swapchain image's
    /// lifetime is its swapchain's: after `vkDestroySwapchainKHR` the driver's `VkImage` values
    /// name nothing, so a guest handle left in the registry would resolve to a token whose host
    /// object is gone. That is the wild non-dispatchable handle Global Constraint 1 is about,
    /// reached with a handle this layer itself issued.
    fn forget_images_of(&self, keep: impl Fn(HostImage) -> bool) -> usize {
        let mut state = self.state.lock();
        state.images.as_mut().map_or(0, |h| h.retain(keep))
    }

    fn forget_image_view(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.image_views.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    fn forget_semaphore(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.semaphores.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    fn forget_fence(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.fences.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    fn forget_command_pool(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.command_pools.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    fn forget_command_buffer(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.command_buffers.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    /// Drop the `VkCommandBuffer` handles allocated from a pool that has just been destroyed.
    ///
    /// [`Vulkan::forget_images_of`]'s argument, one family along: `vkDestroyCommandPool` frees
    /// every buffer allocated from the pool, and a `VkCommandBuffer` is **dispatchable** — so a
    /// handle left behind resolves to a token whose host object is freed memory the driver would
    /// then dereference, which Global Constraint 11 calls Critical rather than merely wrong.
    fn forget_command_buffers_of(&self, keep: impl Fn(HostCommandBuffer) -> bool) -> usize {
        let mut state = self.state.lock();
        state.command_buffers.as_mut().map_or(0, |h| h.retain(keep))
    }

    /// The [`HostQueue`] a guest `VkQueue` names, or a typed refusal naming the call.
    ///
    /// **New in stage 4, because stage 3 issued queue handles and never took one back.**
    /// `vkGetDeviceQueue` put them in the registry so that the *same* handle came back for the
    /// same family and index; nothing then consumed one, because there was no `vkQueueSubmit`.
    /// There is now, and a `VkQueue` is dispatchable.
    fn queue_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostQueue> {
        let state = self.state.lock();
        let registry = state.queues.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkQueue", handle, registry.map_or(0, Handles::live),
                "it is dispatchable, so a host driver dereferences it. `vkGetDeviceQueue` is the \
                 only thing that issues one -- a queue is taken out of a device rather than \
                 created, so a guest that has not called it holds no valid handle at all")
        })
    }

    /// The [`HostSwapchain`] a guest `VkSwapchainKHR` names, or a typed refusal.
    fn swapchain_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostSwapchain> {
        let state = self.state.lock();
        let registry = state.swapchains.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkSwapchainKHR", handle, registry.map_or(0, Handles::live),
                "it is *non*-dispatchable, so it would not crash the driver -- it would name some \
                 other swapchain, and `vkQueuePresentKHR` would put this guest's frame into \
                 whichever window that one belongs to. On this host that is not hypothetical: \
                 `omni_gfx::Renderer` creates swapchains of its own on the same window. \
                 `vkCreateSwapchainKHR` is what issues one, and `vkDestroySwapchainKHR` is what \
                 takes it back -- a handle destroyed and then used again lands here too")
        })
    }

    /// The [`HostImageView`] a guest `VkImageView` names, or a typed refusal.
    fn image_view_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostImageView> {
        let state = self.state.lock();
        let registry = state.image_views.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkImageView", handle, registry.map_or(0, Handles::live),
                "it is *non*-dispatchable. `vkCreateImageView` is what issues one")
        })
    }

    /// The [`HostSemaphore`] a guest `VkSemaphore` names, or a typed refusal.
    fn semaphore_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostSemaphore> {
        let state = self.state.lock();
        let registry = state.semaphores.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkSemaphore", handle, registry.map_or(0, Handles::live),
                "it is *non*-dispatchable, and a `VkSemaphore` and a `VkFence` are both 64-bit \
                 values with nothing in them to tell one from the other -- which is why each \
                 family has its own range of the data area and a handle from the wrong one lands \
                 here. Waiting on another frame's semaphore does not fail; it deadlocks, or it \
                 tears, several frames later")
        })
    }

    /// The [`HostFence`] a guest `VkFence` names, or a typed refusal.
    fn fence_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostFence> {
        let state = self.state.lock();
        let registry = state.fences.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkFence", handle, registry.map_or(0, Handles::live),
                "it is *non*-dispatchable. See the `VkSemaphore` refusal for why the two cannot \
                 be told apart by their values: waiting on the wrong fence is the difference \
                 between a command buffer that is safe to re-record and one the GPU is reading")
        })
    }

    /// The [`HostCommandPool`] a guest `VkCommandPool` names, or a typed refusal.
    fn command_pool_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostCommandPool> {
        let state = self.state.lock();
        let registry = state.command_pools.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkCommandPool", handle, registry.map_or(0, Handles::live),
                "it is *non*-dispatchable. `vkCreateCommandPool` is what issues one, and \
                 destroying it frees every `VkCommandBuffer` allocated from it")
        })
    }

    /// The [`HostCommandBuffer`] a guest `VkCommandBuffer` names, or a typed refusal.
    fn command_buffer_token(
        &self,
        at: &Site,
        call: &str,
        handle: u64,
    ) -> AbiResult<HostCommandBuffer> {
        let state = self.state.lock();
        let registry = state.command_buffers.as_ref();
        let found = GuestAddr::try_from(handle).ok().and_then(|address| registry?.get(address));
        found.ok_or_else(|| {
            wild_handle(at, call, "VkCommandBuffer", handle, registry.map_or(0, Handles::live),
                "it is **dispatchable**, so a host driver dereferences its first word as a loader \
                 dispatch table -- a number the guest chose reaching one is a host access \
                 violation from guest data (Global Constraint 11, Critical). It is also the \
                 handle a renderer touches most, once per `vkCmd*`. \
                 `vkAllocateCommandBuffers` is what issues one")
        })
    }

    // ----------------------------------------------------- stage 5's thirteen, through a macro
    //
    // See `stage_five_family!` for why these three methods per family are generated rather than
    // written out thirteen times, and what the macro is careful to keep hand-written.

    stage_five_family! {
        /// The [`HostDeviceMemory`] a guest `VkDeviceMemory` names, or a typed refusal.
        device_memory_token, register_device_memory, forget_device_memory,
        device_memories, HostDeviceMemory, "VkDeviceMemory", "vkAllocateMemory",
        "it is *non*-dispatchable, and it is the family where a wrong handle is worst: \
         `vkMapMemory` on it hands the guest an address it stores through without checking, and \
         under D4 amendment 1 `admit` does not govern the guest's own stores. A handle naming \
         another allocation would produce a mapping that works, into memory some other resource \
         is bound to"
    }

    stage_five_family! {
        /// The [`HostBuffer`] a guest `VkBuffer` names, or a typed refusal.
        buffer_token, register_buffer, forget_buffer,
        buffers, HostBuffer, "VkBuffer", "vkCreateBuffer",
        "it is *non*-dispatchable, so a forged one names some other buffer -- and a draw that \
         bound it would read vertices from whatever that buffer holds, at full speed, with every \
         `VkResult` zero"
    }

    stage_five_family! {
        /// The [`HostCreatedImage`] a guest `VkImage` names, or a typed refusal.
        ///
        /// **Only images the guest created.** A swapchain image's handle lives in a different
        /// range of the data area and lands here as a refusal, which is what makes
        /// `vkDestroyImage` of one impossible; [`Vulkan::image_ref_token`] is the lookup for the
        /// calls that legitimately accept either.
        created_image_token, register_created_image, forget_created_image,
        created_images, HostCreatedImage, "VkImage (created)", "vkCreateImage",
        "it is *non*-dispatchable. **A swapchain's `VkImage` lands here too**, and that is the \
         point: those come from `vkGetSwapchainImagesKHR`, are owned by their swapchain, and \
         destroying or binding memory to one is undefined behaviour no validation layer on this \
         machine would report"
    }

    stage_five_family! {
        /// The [`HostSampler`] a guest `VkSampler` names, or a typed refusal.
        sampler_token, register_sampler, forget_sampler,
        samplers, HostSampler, "VkSampler", "vkCreateSampler",
        "it is *non*-dispatchable, and a descriptor written with the wrong sampler filters and \
         wraps differently from what the material asked for -- which looks like an art bug"
    }

    stage_five_family! {
        /// The [`HostShaderModule`] a guest `VkShaderModule` names, or a typed refusal.
        shader_module_token, register_shader_module, forget_shader_module,
        shader_modules, HostShaderModule, "VkShaderModule", "vkCreateShaderModule",
        "it is *non*-dispatchable, and a pipeline stage built from the wrong module compiles \
         successfully against a different shader"
    }

    stage_five_family! {
        /// The [`HostPipelineLayout`] a guest `VkPipelineLayout` names, or a typed refusal.
        pipeline_layout_token, register_pipeline_layout, forget_pipeline_layout,
        pipeline_layouts, HostPipelineLayout, "VkPipelineLayout", "vkCreatePipelineLayout",
        "it is *non*-dispatchable, and it is what `vkCmdBindDescriptorSets` and \
         `vkCmdPushConstants` interpret their arguments against -- so the wrong one binds the \
         right sets to the wrong slots"
    }

    stage_five_family! {
        /// The [`HostRenderPass`] a guest `VkRenderPass` names, or a typed refusal.
        render_pass_token, register_render_pass, forget_render_pass,
        render_passes, HostRenderPass, "VkRenderPass", "vkCreateRenderPass",
        "it is *non*-dispatchable, and it decides whether the attachment is cleared, loaded or \
         left alone before the first draw touches it"
    }

    stage_five_family! {
        /// The [`HostFramebuffer`] a guest `VkFramebuffer` names, or a typed refusal.
        framebuffer_token, register_framebuffer, forget_framebuffer,
        framebuffers, HostFramebuffer, "VkFramebuffer", "vkCreateFramebuffer",
        "it is *non*-dispatchable, and it is **which image the frame is drawn into** -- a forged \
         one renders this guest's frame into some other swapchain image, and the one that gets \
         presented is untouched"
    }

    stage_five_family! {
        /// The [`HostPipeline`] a guest `VkPipeline` names, or a typed refusal.
        pipeline_token, register_pipeline, forget_pipeline,
        pipelines, HostPipeline, "VkPipeline", "vkCreateGraphicsPipelines",
        "it is *non*-dispatchable. `VK_NULL_HANDLE` lands here too, which is the case that \
         matters: `vkCreateGraphicsPipelines` may partly fail and writes `VK_NULL_HANDLE` for \
         each pipeline it did not create, so a guest that did not check its `VkResult` reaches \
         this with a handle this layer deliberately wrote"
    }

    stage_five_family! {
        /// The [`HostPipelineCache`] a guest `VkPipelineCache` names, or a typed refusal.
        pipeline_cache_token, register_pipeline_cache, forget_pipeline_cache,
        pipeline_caches, HostPipelineCache, "VkPipelineCache", "vkCreatePipelineCache",
        "it is *non*-dispatchable. `VK_NULL_HANDLE` is legal wherever one is accepted and is \
         handled before this lookup, so reaching it means a value that is neither null nor issued"
    }

    stage_five_family! {
        /// The [`HostDescriptorSetLayout`] a guest `VkDescriptorSetLayout` names, or a typed
        /// refusal.
        descriptor_set_layout_token, register_descriptor_set_layout, forget_descriptor_set_layout,
        descriptor_set_layouts, HostDescriptorSetLayout, "VkDescriptorSetLayout",
        "vkCreateDescriptorSetLayout",
        "it is *non*-dispatchable, and allocating a set from the wrong layout produces a set the \
         shader indexes past"
    }

    stage_five_family! {
        /// The [`HostDescriptorPool`] a guest `VkDescriptorPool` names, or a typed refusal.
        descriptor_pool_token, register_descriptor_pool, forget_descriptor_pool,
        descriptor_pools, HostDescriptorPool, "VkDescriptorPool", "vkCreateDescriptorPool",
        "it is *non*-dispatchable, and resetting or destroying the wrong pool frees every \
         descriptor set another part of the renderer is still binding"
    }

    stage_five_family! {
        /// The [`HostDescriptorSet`] a guest `VkDescriptorSet` names, or a typed refusal.
        descriptor_set_token, register_descriptor_set, forget_descriptor_set,
        descriptor_sets, HostDescriptorSet, "VkDescriptorSet", "vkAllocateDescriptorSets",
        "it is *non*-dispatchable, and it stops being valid when its **pool** is reset or \
         destroyed rather than when anything names it -- which is when this layer takes the \
         handle back, so a set used after its pool went lands here"
    }

    stage_five_family! {
        /// The [`HostQueryPool`] a guest `VkQueryPool` names, or a typed refusal.
        query_pool_token, register_query_pool, forget_query_pool,
        query_pools, HostQueryPool, "VkQueryPool", "vkCreateQueryPool",
        "it is *non*-dispatchable, and reading back another pool's timestamps is a GPU timer \
         measuring frames it did not bracket"
    }

    stage_five_family! {
        /// The [`descriptor::TemplateId`] a guest `VkDescriptorUpdateTemplate` names, or a typed
        /// refusal.
        update_template_token, register_update_template_slot, forget_update_template_slot,
        update_templates, descriptor::TemplateId, "VkDescriptorUpdateTemplate",
        "vkCreateDescriptorUpdateTemplate",
        "it is *non*-dispatchable, and an update through the wrong template reads the guest's \
         data at another template's offsets"
    }

    /// Keep what a query pool holds -- its type, count and statistics -- for reading its results.
    fn remember_query_pool_shape(&self, pool: HostQueryPool, request: QueryPoolRequest) {
        self.state.lock().query_shapes.insert(pool.token(), request);
    }

    /// What a live query pool holds, or `None` for one no create kept.
    fn query_pool_shape(&self, pool: HostQueryPool) -> Option<QueryPoolRequest> {
        self.state.lock().query_shapes.get(&pool.token()).copied()
    }

    /// Forget a destroyed query pool's shape.
    fn forget_query_pool_shape(&self, pool: HostQueryPool) {
        self.state.lock().query_shapes.remove(&pool.token());
    }

    /// Keep a template's entries and register a handle for them.
    fn register_update_template(
        &self,
        at: &Site,
        entries: Vec<descriptor::TemplateEntry>,
    ) -> AbiResult<Registered> {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_template;
            state.next_template += 1;
            id
        };
        let registered = self.register_update_template_slot(at, descriptor::TemplateId(id))?;
        self.state.lock().templates.insert(id, entries);
        Ok(registered)
    }

    /// The entries of the template a guest handle names, or a typed refusal.
    fn update_template_entries(
        &self,
        at: &Site,
        call: &str,
        handle: u64,
    ) -> AbiResult<Vec<descriptor::TemplateEntry>> {
        let id = self.update_template_token(at, call, handle)?;
        Ok(self.state.lock().templates.get(&id.0).cloned().unwrap_or_default())
    }

    /// Forget a template: its handle and its entries.
    fn forget_update_template(&self, handle: GuestAddr) {
        let id = {
            let state = self.state.lock();
            state.update_templates.as_ref().and_then(|h| h.get(handle))
        };
        self.forget_update_template_slot(handle);
        if let Some(id) = id {
            self.state.lock().templates.remove(&id.0);
        }
    }

    /// The image a guest `VkImage` names, **whichever of the two families it belongs to**.
    ///
    /// # Why one lookup and not two calls at every site
    ///
    /// `vkCreateImageView`, `vkCmdPipelineBarrier` and `vkCmdCopyBufferToImage` all take a
    /// `VkImage` that may legitimately be either a swapchain's or one the guest created, and they
    /// arrive as a bare `uint64_t` with nothing in it to say which. A site that checked one
    /// registry and then the other would be three copies of a rule; a site that checked only one
    /// would refuse half the conforming uses.
    ///
    /// What comes back is a [`HostImageRef`] and never a bare token, so the *answer* carries which
    /// family it came from all the way to the host — which is what stops an implementation from
    /// looking a created image up in its swapchain table and finding something.
    fn image_ref_token(&self, at: &Site, call: &str, handle: u64) -> AbiResult<HostImageRef> {
        let state = self.state.lock();
        let address = GuestAddr::try_from(handle).ok();
        if let Some(token) =
            address.and_then(|address| state.images.as_ref().and_then(|h| h.get(address)))
        {
            return Ok(HostImageRef::Swapchain(token));
        }
        if let Some(token) =
            address.and_then(|address| state.created_images.as_ref().and_then(|h| h.get(address)))
        {
            return Ok(HostImageRef::Created(token));
        }
        let swapchain_live = state.images.as_ref().map_or(0, Handles::live);
        let created_live = state.created_images.as_ref().map_or(0, Handles::live);
        Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with {handle:#x} as its `VkImage`, and \
             that is not a handle this layer issued in **either** image family -- it holds \
             {swapchain_live} swapchain image(s) from `vkGetSwapchainImagesKHR` and \
             {created_live} image(s) from `vkCreateImage`, each on a \
             {HANDLE_SLOT_BYTES}-byte boundary of its own range of the boundary's data area. \
             `VkImage` is *non*-dispatchable, so forwarding this would not fault -- it would name \
             some other image, and a copy or a barrier against the wrong one is silent",
            caller = at.caller
        )))
    }

    /// Drop the `VkDescriptorSet` handles a pool has just freed, and answer how many.
    ///
    /// [`Vulkan::forget_command_buffers_of`]'s argument, one family along: a pool reset or destroy
    /// frees every set in it without naming one, and a handle left behind would resolve to a token
    /// whose driver object is gone. The list comes from the host, which is the only participant
    /// that knows which sets a pool holds.
    fn forget_descriptor_sets(&self, tokens: &[HostDescriptorSet]) -> usize {
        if tokens.is_empty() {
            return 0;
        }
        let mut state = self.state.lock();
        state.descriptor_sets.as_mut().map_or(0, |handles| {
            handles.retain(|token| !tokens.contains(&token))
        })
    }

    /// Record the `GuestSpace` pages behind one imported allocation.
    fn note_import(&self, token: HostDeviceMemory, at: GuestAddr, len: usize, size: u64) {
        let mut state = self.state.lock();
        state.imports.insert(token, ImportedMemory { at, len, size });
        state.imported_bytes += len;
        state.imported_peak = state.imported_peak.max(state.imported_bytes);
    }

    /// The pages behind an imported allocation, or `None` for a forwarded one.
    fn import_of(&self, token: HostDeviceMemory) -> Option<ImportedMemory> {
        self.state.lock().imports.get(&token).copied()
    }

    /// Take the record of an imported allocation's pages, so the caller can unmap them.
    fn forget_import(&self, token: HostDeviceMemory) -> Option<ImportedMemory> {
        let mut state = self.state.lock();
        let import = state.imports.remove(&token)?;
        state.imported_bytes = state.imported_bytes.saturating_sub(import.len);
        Some(import)
    }

    /// Charge bytes of guest address space an unwound allocation could not give back.
    fn note_leaked_import(&self, bytes: usize) {
        let mut state = self.state.lock();
        state.leaked_import_bytes = state.leaked_import_bytes.saturating_add(bytes);
    }

    /// **Bytes of guest address space this layer took and could not give back.**
    ///
    /// Zero in every run anything has seen, and stated in [`report`](Vulkan::report) whether it is
    /// zero or not: a leak nobody counted reads exactly like no leak, which is
    /// `VERIFICATION.md` entry 15's shape applied to address space.
    #[must_use]
    pub fn leaked_import_bytes(&self) -> usize {
        self.state.lock().leaked_import_bytes
    }

    /// Charge one `vkMapMemory` of `bytes`.
    fn note_mapped(&self, bytes: u64) {
        let mut state = self.state.lock();
        state.mapped_bytes = state.mapped_bytes.saturating_add(bytes);
    }

    /// **How much `GuestSpace` every live imported allocation holds, and the most it ever held.**
    ///
    /// The measurement D15 asked for: host-visible Vulkan memory is guest commit charge now, so
    /// the peak here is how close a run came to `GuestSpaceConfig::max_committed`. Read as a pair
    /// — the current figure alone says nothing about a run that allocated 300 MB and freed it.
    #[must_use]
    pub fn imported_bytes(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.imported_bytes, state.imported_peak)
    }

    /// How many bytes `vkMapMemory` has handed the guest access to, cumulatively.
    #[must_use]
    pub fn mapped_bytes(&self) -> u64 {
        self.state.lock().mapped_bytes
    }

    /// Every `VkDeviceMemory` this layer currently holds, with whether it was imported.
    #[must_use]
    pub fn device_memory_handles(&self) -> Vec<(GuestAddr, HostDeviceMemory, bool)> {
        let state = self.state.lock();
        state
            .device_memories
            .as_ref()
            .map(|handles| {
                handles
                    .iter()
                    .map(|(at, token)| (at, token, state.imports.contains_key(&token)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record that this layer added a device extension the guest did not ask for.
    ///
    /// See [`RewriteSite::DeviceExtensionAdded`]. Recorded **per `vkCreateDevice`** rather than
    /// once, unlike the memory-type mask: a guest that creates two devices has two devices that
    /// differ from what it described, and a log that mentioned only the first would be a log a
    /// reader could not use to account for the second.
    fn note_device_extension_added(&self, name: &str, caller: GuestAddr) {
        let mut state = self.state.lock();
        let order = state.next_rewrite;
        state.record_rewrites(
            order + 1,
            vec![Rewrite {
                order,
                site: RewriteSite::DeviceExtensionAdded,
                from: "(not requested by the guest)".to_string(),
                to: name.to_string(),
                spec_version: None,
                caller,
            }],
        );
    }

    /// Record the memory types this layer masked out of one physical device's list, **once**.
    ///
    /// Answers whether anything was recorded, so the caller can say in a diagnostic whether this
    /// was the first time. See [`physical`] for the rewrite itself and Global Constraint 1 for why
    /// an unrecorded one is the defect this log exists to prevent.
    fn note_memory_type_rewrites(
        &self,
        device: HostPhysicalDevice,
        masked: &[(u32, u32, u32)],
        caller: GuestAddr,
    ) -> bool {
        let mut state = self.state.lock();
        if !state.memory_rewrites_noted.insert(device) {
            return false;
        }
        let mut order = state.next_rewrite;
        let mut entries = Vec::with_capacity(masked.len());
        for (index, from, to) in masked {
            entries.push(Rewrite {
                order,
                site: RewriteSite::MemoryType { index: *index },
                from: rewrite::memory_property_flags(*from),
                to: rewrite::memory_property_flags(*to),
                spec_version: None,
                caller,
            });
            order += 1;
        }
        state.record_rewrites(order, entries);
        true
    }

    /// Put a physical device in the registry, or recover the handle it already has.
    fn register_physical_device(
        &self,
        at: &Site,
        token: HostPhysicalDevice,
    ) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.physical_devices.as_mut(), "vkEnumeratePhysicalDevices", token, true)
    }

    /// Put a surface in the registry. **Never deduplicated**: each `vkCreateAndroidSurfaceKHR`
    /// makes a new surface, and two calls over one window are two surfaces.
    fn register_surface(&self, at: &Site, token: HostSurface) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.surfaces.as_mut(), "vkCreateAndroidSurfaceKHR", token, false)
    }

    /// Free the slot a live `VkSurfaceKHR` handle names, once `vkDestroySurfaceKHR` has destroyed
    /// the host's surface.
    ///
    /// **Safe to remove from because the family never deduplicates** — the condition
    /// [`Handles::remove`](handles::Handles::remove) states. Answers whether a slot was freed, for
    /// [`Vulkan::forget_swapchain`]'s reason and with its caveat: `false` means another guest
    /// thread destroyed the same surface in between, which the specification already calls
    /// undefined.
    fn forget_surface(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.surfaces.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    /// Put a device in the registry. Never deduplicated, for [`Vulkan::register_surface`]'s
    /// reason.
    fn register_device(&self, at: &Site, token: HostDevice) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.devices.as_mut(), "vkCreateDevice", token, false)
    }

    /// Free the slot a live `VkInstance` handle names, once `vkDestroyInstance` has destroyed the
    /// host's instance. Safe to remove from because the family never deduplicates.
    fn forget_instance(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.instances.as_mut().and_then(|i| i.remove(handle)).is_some()
    }

    /// Drop the `VkPhysicalDevice` handles of an instance that has just been destroyed.
    ///
    /// [`Vulkan::forget_queues_of`]'s argument, one level up: a physical device is enumerated,
    /// never destroyed, and its lifetime is its instance's. The family **deduplicates**, which
    /// [`Handles::remove`](handles::Handles::remove) permits only for tokens a host never recycles
    /// -- and a destroyed instance's physical-device tokens carry its index, which the host never
    /// hands out again.
    fn forget_physical_devices_of(&self, keep: impl Fn(HostPhysicalDevice) -> bool) -> usize {
        let mut state = self.state.lock();
        state.physical_devices.as_mut().map_or(0, |h| h.retain(keep))
    }

    /// Free the slot a live `VkDevice` handle names, once `vkDestroyDevice` has destroyed the
    /// host's device. Safe to remove from because the family never deduplicates.
    fn forget_device(&self, handle: GuestAddr) -> bool {
        let mut state = self.state.lock();
        state.devices.as_mut().and_then(|h| h.remove(handle)).is_some()
    }

    /// Drop the `VkQueue` handles of a device that has just been destroyed, and answer how many.
    ///
    /// [`Vulkan::forget_images_of`]'s argument: a queue's lifetime is its device's, and the guest
    /// has no call that releases one. The queue family **deduplicates**, which
    /// [`Handles::remove`](handles::Handles::remove) warns is unsound if a host recycles tokens;
    /// a host never reuses a queue token, because a queue token names a queue of one device and a
    /// destroyed device's tokens are never handed out again.
    fn forget_queues_of(&self, keep: impl Fn(HostQueue) -> bool) -> usize {
        let mut state = self.state.lock();
        state.queues.as_mut().map_or(0, |h| h.retain(keep))
    }

    /// Put a queue in the registry, or recover the handle it already has. **Deduplicated**, and
    /// [`HostQueue`] says what rests on that.
    fn register_queue(&self, at: &Site, token: HostQueue) -> AbiResult<Registered> {
        let mut state = self.state.lock();
        register(at, state.queues.as_mut(), "vkGetDeviceQueue", token, true)
    }

    /// Whether this layer can satisfy `vkCreateAndroidSurfaceKHR` on `instance`, and record it if
    /// it can.
    ///
    /// **The authority is still the driver's, one name along.** This layer does not assert that it
    /// can create an Android surface; it asks the host what its own platform surface call is
    /// ([`VulkanHost::platform_surface_entry_point`]) and then asks the driver whether it has
    /// *that*. A host with a Vulkan driver but no window-system integration answers `false`, the
    /// guest receives NULL, and nothing was claimed.
    ///
    /// The rewrite is recorded **only when the answer is yes**, because a NULL is not a
    /// substitution. It is recorded here rather than at the call site so that the log entry and
    /// the thunk cannot come apart: a thunk handed out with no entry beside it would be a function
    /// pointer nobody recorded inventing.
    fn resolve_surface_entry_point(
        &self,
        host: &Arc<dyn VulkanHost>,
        instance: HostInstance,
        caller: GuestAddr,
    ) -> AbiResult<bool> {
        let host_call = host.platform_surface_entry_point()?;
        if !host.has_instance_proc(instance, &host_call)? {
            return Ok(false);
        }
        let mut state = self.state.lock();
        let order = state.next_rewrite;
        state.record_rewrites(
            order + 1,
            vec![Rewrite {
                order,
                site: RewriteSite::Resolved,
                from: GUEST_SURFACE_ENTRY_POINT.to_string(),
                to: host_call,
                spec_version: None,
                caller,
            }],
        );
        Ok(true)
    }

    /// Answer one `vkGetDeviceProcAddr`, recording the request either way.
    ///
    /// [`Vulkan::resolve_on_instance`]'s shape and its lock discipline, one handle family along:
    /// the handle is validated under the lock, the lock is dropped, the driver is asked, and the
    /// answer is recorded under the lock again — because an implementation is allowed to block.
    fn resolve_on_device(&self, at: &Site, device: u64, name: &str) -> AbiResult<ProcAnswer> {
        let via = ProcVia::Device;
        if device == 0 {
            // **Specified, not chosen.** The "Command Function Pointers" table fixes
            // `vkGetDeviceProcAddr(VK_NULL_HANDLE, ...)` as NULL for every `pName` — there is no
            // device-level command a null device can answer for, which is why this list has no
            // counterpart to `NULL_INSTANCE_COMMANDS`.
            let mut state = self.state.lock();
            state.record_request(via, device, name, ProcAnswer::NullPerSpecification, at.caller);
            return Ok(ProcAnswer::NullPerSpecification);
        }
        let (entry, host, token) = {
            let mut state = self.state.lock();
            let Some(entry) = state.entry else {
                state.record_request(via, device, name, ProcAnswer::Refused, at.caller);
                return Err(at.refuse(
                    "the guest called `vkGetDeviceProcAddr` on a Vulkan instance that was never \
                     bound into this boundary, so there is no thunk pool to hand an address out \
                     of. `Vulkan::bind_into` is what allocates it"
                        .to_string(),
                ));
            };
            let registry = state.devices.as_ref();
            let token = GuestAddr::try_from(device).ok().and_then(|address| registry?.get(address));
            let Some(token) = token else {
                let live = registry.map_or(0, Handles::live);
                state.record_request(via, device, name, ProcAnswer::Refused, at.caller);
                return Err(wild_handle(
                    at,
                    "vkGetDeviceProcAddr",
                    "VkDevice",
                    device,
                    live,
                    "it is dispatchable, so a host driver dereferences it. NULL is not the answer \
                     either: NULL means \"that device does not support this command\", which \
                     would be a statement about a device that does not exist",
                ));
            };
            let Some(host) = state.host.clone() else {
                state.record_request(via, device, name, ProcAnswer::Refused, at.caller);
                return Err(at.refuse(format!(
                    "the guest called `vkGetDeviceProcAddr(device = {device:#x}, \
                     pName = \"{name}\")` and this loader has no host driver attached, so there \
                     is nothing that can say whether that command exists"
                )));
            };
            (entry, host, token)
        };
        // Outside the lock; see this method's documentation.
        let present = host.has_device_proc(token, name);
        let mut state = self.state.lock();
        match present {
            Err(error) => {
                state.record_request(via, device, name, ProcAnswer::Refused, at.caller);
                Err(error)
            }
            Ok(false) => {
                state.record_request(via, device, name, ProcAnswer::NullFromDriver, at.caller);
                Ok(ProcAnswer::NullFromDriver)
            }
            Ok(true) => state.hand_out(at, via, entry, device, name),
        }
    }

    /// Record a failing `VkResult` a driver answered with. Bounded, like every other log here.
    fn note_driver_result(&self, call: &str, result: i32) {
        let mut state = self.state.lock();
        if state.driver_results.len() < MAX_RECORDS {
            state.driver_results.push((call.to_string(), result));
        }
    }

    /// Put a created instance in the registry and return its index and the handle the guest gets.
    fn register_instance(&self, at: &Site, token: HostInstance) -> AbiResult<(usize, GuestAddr)> {
        let mut state = self.state.lock();
        let Some(instances) = state.instances.as_mut() else {
            return Err(at.refuse(
                "this Vulkan instance was never bound into a boundary, so it has no `VkInstance` \
                 registry to put the driver's instance in. `Vulkan::bind_into` is what declares \
                 it. The instance the driver just created is leaked by this refusal, which is the \
                 lesser of the two outcomes: the other is handing the guest a handle that names \
                 nothing"
                    .to_string(),
            ));
        };
        instances.insert(token).ok_or_else(|| {
            at.refuse(format!(
                "the guest called `vkCreateInstance` from {caller:#x} and this layer already holds \
                 {MAX_INSTANCES} live `VkInstance` handles, which is `MAX_INSTANCES`. This is a \
                 refusal rather than `VK_ERROR_OUT_OF_HOST_MEMORY` because that code says the host \
                 is out of memory and it is not -- the engine would go looking in the wrong place. \
                 The driver did create an instance and this layer is dropping it on the floor, \
                 which is a real leak and is named here rather than hidden",
                caller = at.caller
            ))
        })
    }

    /// Answer one `vkGetInstanceProcAddr`, recording the request either way.
    ///
    /// # Why a non-null instance takes two trips through the lock
    ///
    /// The driver is asked whether it has the entry point, and that is a call into the embedding —
    /// [`VulkanHost`]'s documentation says an implementation may block there. Holding this
    /// instance's mutex across it would let one guest thread's slow driver stall every other
    /// guest thread's `vkGetInstanceProcAddr`, which is the shape
    /// [`ndk::window`](crate::ndk::window)'s `decided` already clones its `Arc` out to avoid. So
    /// the handle is validated under the lock, the lock is dropped, the driver is asked, and the
    /// answer is recorded under the lock again.
    fn resolve(&self, at: &Site, instance: u64, name: &str) -> AbiResult<ProcAnswer> {
        let via = ProcVia::Instance;
        if instance != 0 {
            return self.resolve_on_instance(at, instance, name);
        }
        let mut state = self.state.lock();
        let Some(entry) = state.entry else {
            // **Reachable, and not a guard against the impossible** (`VERIFICATION.md` entry 12):
            // a host that binds instance A into the boundary and then activates instance B gets
            // here, because `active()` answers with whatever was published to this thread and has
            // no way to know which boundary is running. The alternative was an `expect`, and
            // entry 13 is about exactly that: a host panic unwinding out of an import is the one
            // failure this layer exists to never produce.
            state.record_request(via, instance, name, ProcAnswer::Refused, at.caller);
            return Err(at.refuse(
                "the guest called `vkGetInstanceProcAddr` on a Vulkan instance that was never \
                 bound into this boundary. `Vulkan::bind_into` is what allocates the thunk pool, \
                 and an instance that was activated without it has no address to hand out. A \
                 host that binds one instance and activates another arrives here"
                    .to_string(),
            ));
        };
        if !NULL_INSTANCE_COMMANDS.contains(&name) {
            // **The specified NULL**, and the only one. See `NULL_INSTANCE_COMMANDS`.
            state.record_request(via, instance, name, ProcAnswer::NullPerSpecification, at.caller);
            return Ok(ProcAnswer::NullPerSpecification);
        }
        state.hand_out(at, via, entry, instance, name)
    }

    /// Answer one `vkGetInstanceProcAddr` on an instance **this layer issued**.
    ///
    /// # Three answers, and the authority behind each
    ///
    /// * The handle is not one of ours → **refused**. A `VkInstance` is dispatchable: a host
    ///   driver dereferences it, so a number the guest computed reaching one is a host access
    ///   violation from guest data, which Global Constraint 11 calls Critical. NULL would be worse
    ///   than useless here — it says "that instance does not support this command", which is a
    ///   statement about an instance that does not exist.
    /// * The driver has no entry point of that name → **NULL**, on the driver's authority, as
    ///   [`ProcAnswer::NullFromDriver`]. This is what a real loader answers and it is how the
    ///   engine discovers an extension it did not enable.
    /// * The driver has it → **a guest thunk**, never the driver's pointer.
    ///   [`VulkanHost::has_instance_proc`] is shaped so that the second is not expressible.
    fn resolve_on_instance(&self, at: &Site, instance: u64, name: &str) -> AbiResult<ProcAnswer> {
        let via = ProcVia::Instance;
        let (entry, host, token) = {
            let mut state = self.state.lock();
            let Some(entry) = state.entry else {
                state.record_request(via, instance, name, ProcAnswer::Refused, at.caller);
                return Err(at.refuse(
                    "the guest called `vkGetInstanceProcAddr` on a Vulkan instance that was never \
                     bound into this boundary. `Vulkan::bind_into` is what allocates the thunk \
                     pool, and an instance that was activated without it has no address to hand \
                     out"
                        .to_string(),
                ));
            };
            let handle = GuestAddr::try_from(instance).ok();
            let token = handle.and_then(|at| state.instances.as_ref().and_then(|i| i.get(at)));
            let Some(token) = token else {
                state.record_request(via, instance, name, ProcAnswer::Refused, at.caller);
                let issued = state.instances.as_ref().map_or(0, Instances::live);
                return Err(at.refuse(format!(
                    "the guest called `vkGetInstanceProcAddr(instance = {instance:#x}, \
                     pName = \"{name}\")` from {caller:#x}, and {instance:#x} is not a \
                     `VkInstance` this layer issued, or is one `vkDestroyInstance` has taken \
                     back -- it holds {issued} live handle(s), each on a \
                     {INSTANCE_SLOT_BYTES}-byte boundary of its registry. A `VkInstance` is a \
                     dispatchable handle a host driver dereferences, so forwarding this one would \
                     be dereferencing a number the guest chose. NULL is not the answer either: \
                     NULL means \"that instance does not support {name}\", which would be a \
                     statement about an instance that does not exist. \
                     `Vulkan::instance_handles()` is the list of the ones that do",
                    caller = at.caller
                )));
            };
            let Some(host) = state.host.clone() else {
                state.record_request(via, instance, name, ProcAnswer::Refused, at.caller);
                return Err(at.refuse(format!(
                    "the guest called `vkGetInstanceProcAddr(instance = {instance:#x}, \
                     pName = \"{name}\")` and this loader has no host driver attached, so there is \
                     nothing that can say whether that command exists. The instance itself was \
                     issued by a host that is no longer here -- `Vulkan::set_host` replaced it, or \
                     was never called"
                )));
            };
            (entry, host, token)
        };
        // Outside the lock; this method's documentation says why.
        //
        // **The one name this layer answers for rather than the driver.** See
        // `GUEST_SURFACE_ENTRY_POINT`: `vkCreateAndroidSurfaceKHR` is a command no host driver
        // has, so asking `has_instance_proc` about it answers `false` and the engine would be told
        // the command does not exist -- one call after being told the extension does. The
        // condition is still a measurement rather than an assertion: the driver is asked about the
        // **host's** call, and a host with no window-system integration still produces NULL here.
        let present = if name == GUEST_SURFACE_ENTRY_POINT {
            self.resolve_surface_entry_point(&host, token, at.caller)
        } else {
            host.has_instance_proc(token, name)
        };
        let mut state = self.state.lock();
        match present {
            Err(error) => {
                state.record_request(via, instance, name, ProcAnswer::Refused, at.caller);
                Err(error)
            }
            Ok(false) => {
                state.record_request(via, instance, name, ProcAnswer::NullFromDriver, at.caller);
                Ok(ProcAnswer::NullFromDriver)
            }
            Ok(true) => state.hand_out(at, via, entry, instance, name),
        }
    }

    /// Record a call through a pool thunk, and answer which Vulkan function it stands for.
    fn record_call(
        &self,
        thunk: GuestAddr,
        args: [u64; ARG_REGISTERS as usize],
        caller: GuestAddr,
    ) -> Option<String> {
        let mut state = self.state.lock();
        let name = state.assigned.get(&thunk).cloned();
        if let Some(name) = &name {
            *state.call_counts.entry(name.clone()).or_insert(0) += 1;
        }
        let order = state.calls.len() + state.calls_dropped;
        if state.calls.len() >= MAX_RECORDS {
            state.calls_dropped += 1;
        } else {
            state.calls.push(ProcCall { order, name: name.clone(), thunk, args, caller });
        }
        name
    }

    fn note_entry_call(&self) {
        self.state.lock().entry_calls += 1;
    }
}

impl State {
    /// Give `name` its stable thunk — the same one every time — and record the lookup.
    ///
    /// Shared by the null-instance and real-instance paths on purpose: the engine stores what it
    /// is given (`0x6d3ca8`) and compares pointers, so one function must have one address whether
    /// it was asked for before or after `vkCreateInstance`.
    fn hand_out(
        &mut self,
        at: &Site,
        via: ProcVia,
        entry: GuestAddr,
        instance: u64,
        name: &str,
    ) -> AbiResult<ProcAnswer> {
        if name == LOADER_ENTRY_POINT {
            // Its own slot, not a pool slot: the loader really does return a pointer to itself,
            // and handing back the address the guest already has is what makes that true here.
            self.record_request(via, instance, name, ProcAnswer::Thunk(entry), at.caller);
            return Ok(ProcAnswer::Thunk(entry));
        }
        if let Some(&already) = self.by_name.get(name) {
            self.record_request(via, instance, name, ProcAnswer::Thunk(already), at.caller);
            return Ok(ProcAnswer::Thunk(already));
        }
        let taken = self.by_name.len();
        let Some(&slot) = self.pool.get(taken) else {
            self.record_request(via, instance, name, ProcAnswer::Refused, at.caller);
            return Err(at.refuse(format!(
                "the guest asked for `{name}`, which would be the {ordinal}th distinct Vulkan \
                 entry point this instance has been asked for, and the thunk pool holds \
                 {MAX_PROC_SLOTS}. This is a refusal rather than a NULL because NULL means \"this \
                 Vulkan implementation does not have {name}\" and a pool running out is a fact \
                 about this layer. Raise `omni_android::vulkan::MAX_PROC_SLOTS`",
                ordinal = taken + 1
            )));
        };
        self.by_name.insert(name.to_string(), slot);
        self.assigned.insert(slot, name.to_string());
        self.record_request(via, instance, name, ProcAnswer::Thunk(slot), at.caller);
        Ok(ProcAnswer::Thunk(slot))
    }

    /// Append what [`rewrite`] produced, dropping past [`MAX_REWRITES`] but still advancing the
    /// ordinal — so a truncated log cannot be mistaken for a complete one.
    fn record_rewrites(&mut self, next_order: usize, rewrites: Vec<Rewrite>) {
        self.next_rewrite = next_order;
        for entry in rewrites {
            if self.rewrites.len() >= MAX_REWRITES {
                self.rewrites_dropped += 1;
            } else {
                self.rewrites.push(entry);
            }
        }
    }

    fn record_request(
        &mut self,
        via: ProcVia,
        instance: u64,
        name: &str,
        answer: ProcAnswer,
        caller: GuestAddr,
    ) {
        let order = self.requests.len() + self.requests_dropped;
        if self.requests.len() >= MAX_RECORDS {
            self.requests_dropped += 1;
            return;
        }
        self.requests.push(ProcRequest {
            order,
            via,
            instance,
            name: name.to_string(),
            answer,
            caller,
        });
    }
}

/// Carve one handle registry out of the boundary's data area.
///
/// A free function rather than a closure in [`Vulkan::bind_into`] because it is **generic** over
/// the token type and a closure cannot be: each family stores a different token, which is the
/// whole point — a `Handles<HostQueue>` cannot be handed a `HostDevice` by mistake, and the
/// type system rather than a runtime tag is what says so.
fn carve<T: Copy + PartialEq>(
    builder: &BoundaryBuilder,
    symbol: &str,
    count: usize,
    kind: &'static str,
    magic: u64,
) -> AbiResult<Handles<T>> {
    builder
        .declare_data(symbol, count * SLOT, SLOT)
        .map(|base| Handles::new(base, count, kind, magic))
}

/// One handle this layer has just issued, or recovered.
pub(super) struct Registered {
    /// The address the guest receives as its `VkPhysicalDevice`, `VkSurfaceKHR`, `VkDevice` or
    /// `VkQueue`.
    pub(super) at: GuestAddr,
    /// The sixteen bytes the slot should hold. **Only written when
    /// [`fresh`](Registered::fresh)**: rewriting a live slot would be a guest-visible write for no
    /// reason, and the bytes are the same ones that are already there.
    pub(super) image: [u8; HANDLE_SLOT_BYTES],
    /// Whether a slot was consumed, as opposed to an existing handle being recovered.
    pub(super) fresh: bool,
}

/// Put a token in a registry, or recover the handle it already has.
///
/// `deduplicate` is the difference between "asking again for the same thing gets the same handle"
/// — `vkEnumeratePhysicalDevices`, `vkGetDeviceQueue` — and "each call makes a new object" —
/// `vkCreateAndroidSurfaceKHR`, `vkCreateDevice`. It is a parameter rather than a property of the
/// registry because it is a property of the **call**: the same `VkDevice` registry would
/// deduplicate wrongly if a host ever reused a token for a destroyed device -- which
/// `vkDestroyDevice` now makes possible, and which is why it never deduplicates.
fn register<T: Copy + PartialEq>(
    at: &Site,
    registry: Option<&mut Handles<T>>,
    call: &str,
    token: T,
    deduplicate: bool,
) -> AbiResult<Registered> {
    let Some(registry) = registry else {
        return Err(at.refuse(format!(
            "the guest called `{call}` and this Vulkan instance was never bound into a boundary, \
             so it has no handle registry to put the result in. `Vulkan::bind_into` is what \
             declares them. Whatever the driver just made is leaked by this refusal, which is the \
             lesser of the two outcomes: the other is handing the guest a handle that names \
             nothing"
        )));
    };
    let kind = registry.kind();
    let capacity = registry.capacity();
    let placed = if deduplicate {
        registry.insert_or_get(token)
    } else {
        registry.insert(token).map(|(index, at)| (index, at, true))
    };
    let Some((index, address, fresh)) = placed else {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} and this layer already holds {capacity} \
             live `{kind}` handle(s), which is its bound. This is a refusal rather than \
             `VK_ERROR_OUT_OF_HOST_MEMORY` because that code says the host is out of memory and \
             it is not -- the engine would go looking in the wrong place. The driver's object is \
             dropped on the floor by this refusal, which is a real leak and is named here rather \
             than hidden",
            caller = at.caller
        )));
    };
    Ok(Registered { at: address, image: registry.slot_image(index), fresh })
}

/// The refusal a handle of the wrong family, or one the guest invented, produces.
///
/// One function so that all four families refuse in the same words with the same structure — the
/// call, the value, the family it was supposed to be, how many real ones exist, and *why this
/// family in particular must not be forwarded*. That last clause differs per family and is the
/// caller's to supply, because the reasons genuinely differ: three of the four are dispatchable
/// and the fourth is a Global Constraint 1 problem rather than a Global Constraint 11 one.
fn wild_handle(
    at: &Site,
    call: &str,
    kind: &str,
    handle: u64,
    live: usize,
    why: &str,
) -> AbiError {
    at.refuse(format!(
        "the guest called `{call}` from {caller:#x} with {handle:#x} as its `{kind}`, and that is \
         not a handle this layer issued -- it holds {live} live one(s), each on a \
         {HANDLE_SLOT_BYTES}-byte boundary of that family's own range of the boundary's data \
         area, so a handle from another family and a handle the guest computed both land here. \
         Forwarding it is not an option: {why}",
        caller = at.caller
    ))
}

/// Where a refusal points: the symbol, its thunk, and the guest's return address.
///
/// Taken by value out of an [`ImportCall`] rather than borrowed from one, so that a handler can
/// keep it while it still holds the `&mut ImportCall` it needs for [`ImportCall::mem`] and
/// [`ImportCall::ret`]. That is what lets [`instance`]'s forwarding read the site and the call in
/// the same function without a second borrow.
pub(super) struct Site {
    pub(super) symbol: String,
    pub(super) address: GuestAddr,
    pub(super) caller: GuestAddr,
}

impl Site {
    pub(super) fn of(c: &ImportCall<'_, '_>) -> Self {
        Self { symbol: c.symbol().to_string(), address: c.address(), caller: c.caller() }
    }

    pub(super) fn refuse(&self, why: String) -> AbiError {
        AbiError::Refused { symbol: self.symbol.clone(), address: self.address, why }
    }
}

// ------------------------------------------------------------------------------ the handlers

/// `PFN_vkVoidFunction vkGetInstanceProcAddr(VkInstance instance, const char *pName)`
///
/// **A lookup, not graphics**, which is why it is a real implementation in a module that
/// implements no Vulkan: the engine's whole renderer selection is this function's return value
/// being non-null or null, and nothing downstream of it can be measured until it answers.
///
/// Inline rather than reentrant: it reads one guest string and answers, calls no guest code and
/// changes no mapping, so [`ImportCall`]'s ≈33 ns path is where it belongs (D17, D18).
///
/// The module documentation has the table of answers and the argument for the one NULL.
fn proc_addr(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (instance, name_pointer) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let at = Site::of(c);
    let vulkan = active(&at.symbol, at.address)?;
    // Charged before anything can fail, so that `entry_calls` and the request log are a pair that
    // must agree rather than two views of the same write. `VERIFICATION.md` entry 15.
    vulkan.note_entry_call();

    let Ok(name_at) = GuestAddr::try_from(name_pointer) else {
        return Err(at.refuse(format!(
            "the guest passed {name_pointer:#x} as `pName`, which is not an address in this \
             guest's space"
        )));
    };
    if name_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkGetInstanceProcAddr(instance = {instance:#x}, pName = NULL)` \
             from {caller:#x}. The specification requires `pName` to be a pointer to a \
             null-terminated UTF-8 string, so there is no name to look up -- and NULL is not an \
             answer here, because NULL is how a caller detects an absent function and this call \
             never named one",
            caller = at.caller
        )));
    }
    let name = {
        let bytes = c.mem().cstr(name_at, c.blame(1))?;
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let answer = vulkan.resolve(&at, instance, &name)?;
    let value = match answer {
        ProcAnswer::Thunk(address) => address as u64,
        // The two NULLs — the specification's and the driver's. `ProcAnswer::Refused` never
        // reaches here: `resolve` returns `Err` for it, so these arms are the NULL cases alone.
        // The variants stay distinct all the way into the census; only the *value* is the same.
        ProcAnswer::NullPerSpecification | ProcAnswer::NullFromDriver | ProcAnswer::Refused => 0,
    };
    c.ret().u64(value);
    Ok(())
}

/// Every thunk `vkGetInstanceProcAddr` hands out lands here, and the name decides what happens.
///
/// **One dispatch, unchanged from stage 1, with two of its refusals replaced.** The slot is
/// anonymous — the pool has no Vulkan names in it — so the only thing that says which function the
/// guest branched to is the name the slot was handed out *for*, which is what
/// [`Vulkan::record_call`] returns. Stage 2a matches on that string and forwards two of them; the
/// rest reach [`instance::unimplemented`] and refuse exactly as they did before, naming the
/// function and quoting `x0`-`x7`.
///
/// Recording happens **before** the match, so a forwarded call is in
/// [`Vulkan::calls`](Vulkan::calls) whether it succeeded, was refused or made the driver unhappy.
/// A record written after the call is a record written by every call except the ones that matter.
///
/// **The refusal is still the measurement** for everything else. `libroblox.so` stores what it is given at `0x6d3ca8` and
/// reaches it with `blr x8`, so the thing that says which Vulkan function the engine went to
/// *first* is this handler naming it. A slot that returned `VK_SUCCESS` without creating anything
/// would be Global Constraint 1's failure exactly: a believable answer whose consequence arrives
/// in the next frame, or in the frame after the swapchain it never made.
///
/// The eight argument registers are recorded and printed because the signature is unknown to this
/// layer — the pool slot is anonymous and the name arrived as a string — so interpreting `X0` as a
/// `VkInstance *` would be this layer guessing at a prototype. They are what stage 2 will need in
/// order to marshal the call for real, and quoting them in the refusal is what makes the first
/// failing call self-describing instead of a name with no arguments attached.
fn proc_slot(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let mut args = [0u64; ARG_REGISTERS as usize];
    {
        // Exactly `ARG_REGISTERS` reads, which is exactly `X0`-`X7`: a ninth would walk into the
        // stack overflow area, and this handler knows no signature that could say there is one.
        let mut cursor = c.args();
        for slot in &mut args {
            *slot = cursor.next_u64()?;
        }
    }
    let at = Site::of(c);
    let vulkan = active(&at.symbol, at.address)?;
    let name = vulkan.record_call(at.address, args, at.caller);

    let Some(name) = name else {
        let registers = args
            .iter()
            .enumerate()
            .map(|(index, value)| format!("x{index}={value:#x}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(at.refuse(format!(
            "the guest branched to {address:#x} from {caller:#x}, which is a Vulkan procedure \
             slot this instance has never handed out. Every such address comes from \
             `vkGetInstanceProcAddr`, so a branch to one that was never returned is a pointer the \
             guest computed. It was passed {registers}",
            address = at.address,
            caller = at.caller
        )));
    };
    match name.as_str() {
        // Stage 2a.
        "vkEnumerateInstanceExtensionProperties" => {
            instance::enumerate_instance_extension_properties(c, &at, &vulkan, args)
        }
        "vkCreateInstance" => instance::create_instance(c, &at, &vulkan, args),
        // The last call of the engine's `APP_CMD_TERM_WINDOW` teardown, measured.
        "vkDestroyInstance" => instance::destroy_instance(c, &at, &vulkan, args),
        "vkEnumerateInstanceLayerProperties" => {
            instance::enumerate_instance_layer_properties(c, &at, args)
        }
        "vkEnumerateInstanceVersion" => instance::enumerate_instance_version(c, &at, args),
        // Stage 3: the substitution that creates an object rather than renaming a string.
        "vkCreateAndroidSurfaceKHR" => surface::create_android_surface(c, &at, &vulkan, args),
        // And its end, which the engine's `APP_CMD_TERM_WINDOW` handler was measured calling
        // right after `vkDestroySwapchainKHR`.
        "vkDestroySurfaceKHR" => surface::destroy_surface(c, &at, &vulkan, args),
        // Stage 3: the queries a renderer makes in order to choose a device.
        "vkEnumeratePhysicalDevices" => {
            physical::enumerate_physical_devices(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceProperties" => {
            physical::physical_device_properties(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceFeatures" => physical::physical_device_features(c, &at, &vulkan, args),
        "vkGetPhysicalDeviceFormatProperties" => {
            physical::physical_device_format_properties(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceImageFormatProperties" => {
            physical::physical_device_image_format_properties(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceImageFormatProperties2"
        | "vkGetPhysicalDeviceImageFormatProperties2KHR" => {
            physical::physical_device_image_format_properties2(c, &at, &vulkan, name.as_str(), args)
        }
        // The engine's device bring-up: `KHR`, with flat structures chained (`chain`).
        "vkGetPhysicalDeviceFeatures2" | "vkGetPhysicalDeviceFeatures2KHR" => {
            physical::physical_device_features2(c, &at, &vulkan, name.as_str(), args)
        }
        "vkGetPhysicalDeviceQueueFamilyProperties" => {
            physical::queue_family_properties(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceMemoryProperties" => {
            physical::physical_device_memory_properties(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceSurfaceSupportKHR" => physical::surface_support(c, &at, &vulkan, args),
        "vkGetPhysicalDeviceSurfaceCapabilitiesKHR" => {
            physical::surface_capabilities(c, &at, &vulkan, args)
        }
        "vkGetPhysicalDeviceSurfaceFormatsKHR" => physical::surface_formats(c, &at, &vulkan, args),
        "vkGetPhysicalDeviceSurfacePresentModesKHR" => {
            physical::surface_present_modes(c, &at, &vulkan, args)
        }
        "vkEnumerateDeviceExtensionProperties" => {
            physical::device_extension_properties(c, &at, &vulkan, args)
        }
        // Stage 3: the device, its queues, and the device-level half of the lookup.
        "vkCreateDevice" => device::create_device(c, &at, &vulkan, args),
        "vkGetDeviceQueue" => device::get_device_queue(c, &at, &vulkan, args),
        "vkGetDeviceProcAddr" => device::get_device_proc_addr(c, &at, &vulkan, args),
        // Measured on `APP_CMD_TERM_WINDOW`, after the pipeline cache is saved.
        "vkDestroyDevice" => device::destroy_device(c, &at, &vulkan, args),
        // Stage 4: the presentation spine -- from the device the guest now holds to a frame on
        // the screen. Nothing past a clear: there is no render pass, no pipeline and no device
        // memory here, and a guest that asks for one reaches `instance::unimplemented` below,
        // which names it (D17).
        "vkCreateSwapchainKHR" => swapchain::create_swapchain(c, &at, &vulkan, args),
        "vkGetSwapchainImagesKHR" => swapchain::get_swapchain_images(c, &at, &vulkan, args),
        "vkDestroySwapchainKHR" => swapchain::destroy_swapchain(c, &at, &vulkan, args),
        "vkAcquireNextImageKHR" => swapchain::acquire_next_image(c, &at, &vulkan, args),
        "vkCreateImageView" => view::create_image_view(c, &at, &vulkan, args),
        "vkDestroyImageView" => view::destroy_image_view(c, &at, &vulkan, args),
        "vkCreateSemaphore" => sync::create_semaphore(c, &at, &vulkan, args),
        "vkDestroySemaphore" => sync::destroy_semaphore(c, &at, &vulkan, args),
        "vkCreateFence" => sync::create_fence(c, &at, &vulkan, args),
        "vkDestroyFence" => sync::destroy_fence(c, &at, &vulkan, args),
        "vkWaitForFences" => sync::wait_for_fences(c, &at, &vulkan, args),
        "vkResetFences" => sync::reset_fences(c, &at, &vulkan, args),
        "vkCreateCommandPool" => command::create_command_pool(c, &at, &vulkan, args),
        "vkDestroyCommandPool" => command::destroy_command_pool(c, &at, &vulkan, args),
        "vkResetCommandPool" => command::reset_command_pool(c, &at, &vulkan, args),
        "vkAllocateCommandBuffers" => command::allocate_command_buffers(c, &at, &vulkan, args),
        "vkFreeCommandBuffers" => command::free_command_buffers(c, &at, &vulkan, args),
        "vkBeginCommandBuffer" => command::begin_command_buffer(c, &at, &vulkan, args),
        "vkEndCommandBuffer" => command::end_command_buffer(c, &at, &vulkan, args),
        "vkResetCommandBuffer" => command::reset_command_buffer(c, &at, &vulkan, args),
        "vkCmdPipelineBarrier" => command::cmd_pipeline_barrier(c, &at, &vulkan, args),
        "vkCmdClearColorImage" => command::cmd_clear_color_image(c, &at, &vulkan, args),
        "vkQueueSubmit" => queue::queue_submit(c, &at, &vulkan, args),
        "vkQueuePresentKHR" => queue::queue_present(c, &at, &vulkan, args),
        "vkQueueWaitIdle" => queue::queue_wait_idle(c, &at, &vulkan, args),
        "vkDeviceWaitIdle" => queue::device_wait_idle(c, &at, &vulkan, args),
        // Stage 5: memory and resources -- everything between "I have a device" and "I can draw".
        // The order here is the order a renderer reaches them in, which is also the order
        // `docs/HANDOFF.md` lists them in.
        "vkAllocateMemory" => memory::allocate_memory(c, &at, &vulkan, args),
        "vkFreeMemory" => memory::free_memory(c, &at, &vulkan, args),
        "vkMapMemory" => memory::map_memory(c, &at, &vulkan, args),
        "vkUnmapMemory" => memory::unmap_memory(c, &at, &vulkan, args),
        "vkGetBufferMemoryRequirements" => {
            memory::buffer_memory_requirements(c, &at, &vulkan, args)
        }
        "vkGetImageMemoryRequirements" => memory::image_memory_requirements(c, &at, &vulkan, args),
        "vkBindBufferMemory" => memory::bind_buffer_memory(c, &at, &vulkan, args),
        "vkBindImageMemory" => memory::bind_image_memory(c, &at, &vulkan, args),
        "vkFlushMappedMemoryRanges" => memory::flush_mapped_memory_ranges(c, &at, &vulkan, args),
        "vkInvalidateMappedMemoryRanges" => {
            memory::invalidate_mapped_memory_ranges(c, &at, &vulkan, args)
        }
        "vkCreateBuffer" => resource::create_buffer(c, &at, &vulkan, args),
        "vkDestroyBuffer" => resource::destroy_buffer(c, &at, &vulkan, args),
        "vkCreateImage" => resource::create_image(c, &at, &vulkan, args),
        "vkDestroyImage" => resource::destroy_image(c, &at, &vulkan, args),
        "vkCreateSampler" => resource::create_sampler(c, &at, &vulkan, args),
        "vkDestroySampler" => resource::destroy_sampler(c, &at, &vulkan, args),
        "vkCreateShaderModule" => shader::create_shader_module(c, &at, &vulkan, args),
        "vkDestroyShaderModule" => shader::destroy_shader_module(c, &at, &vulkan, args),
        "vkCreatePipelineCache" => shader::create_pipeline_cache(c, &at, &vulkan, args),
        "vkDestroyPipelineCache" => shader::destroy_pipeline_cache(c, &at, &vulkan, args),
        // The engine saves its cache on `APP_CMD_TERM_WINDOW`, measured.
        "vkGetPipelineCacheData" => shader::get_pipeline_cache_data(c, &at, &vulkan, args),
        // The engine's own renderer: its GPU timer.
        "vkCreateQueryPool" => query::create_query_pool(c, &at, &vulkan, args),
        // Its descriptor update templates, kept by this layer (see `descriptor`).
        "vkCreateDescriptorUpdateTemplate" | "vkCreateDescriptorUpdateTemplateKHR" => {
            descriptor::create_descriptor_update_template(c, &at, &vulkan, name.as_str(), args)
        }
        "vkUpdateDescriptorSetWithTemplate" | "vkUpdateDescriptorSetWithTemplateKHR" => {
            descriptor::update_descriptor_set_with_template(c, &at, &vulkan, name.as_str(), args)
        }
        "vkDestroyDescriptorUpdateTemplate" | "vkDestroyDescriptorUpdateTemplateKHR" => {
            descriptor::destroy_descriptor_update_template(c, &at, &vulkan, name.as_str(), args)
        }
        "vkDestroyQueryPool" => query::destroy_query_pool(c, &at, &vulkan, args),
        "vkCmdResetQueryPool" => query::cmd_reset_query_pool(c, &at, &vulkan, args),
        "vkCmdWriteTimestamp" => query::cmd_write_timestamp(c, &at, &vulkan, args),
        "vkGetQueryPoolResults" => query::get_query_pool_results(c, &at, &vulkan, args),
        "vkCreatePipelineLayout" => shader::create_pipeline_layout(c, &at, &vulkan, args),
        "vkDestroyPipelineLayout" => shader::destroy_pipeline_layout(c, &at, &vulkan, args),
        "vkCreateRenderPass" => shader::create_render_pass(c, &at, &vulkan, args),
        "vkDestroyRenderPass" => shader::destroy_render_pass(c, &at, &vulkan, args),
        "vkCreateFramebuffer" => shader::create_framebuffer(c, &at, &vulkan, args),
        "vkDestroyFramebuffer" => shader::destroy_framebuffer(c, &at, &vulkan, args),
        "vkCreateGraphicsPipelines" => shader::create_graphics_pipelines(c, &at, &vulkan, args),
        "vkCreateComputePipelines" => shader::create_compute_pipelines(c, &at, &vulkan, args),
        "vkDestroyPipeline" => shader::destroy_pipeline(c, &at, &vulkan, args),
        "vkCreateDescriptorSetLayout" => {
            descriptor::create_descriptor_set_layout(c, &at, &vulkan, args)
        }
        "vkDestroyDescriptorSetLayout" => {
            descriptor::destroy_descriptor_set_layout(c, &at, &vulkan, args)
        }
        "vkCreateDescriptorPool" => descriptor::create_descriptor_pool(c, &at, &vulkan, args),
        "vkDestroyDescriptorPool" => descriptor::destroy_descriptor_pool(c, &at, &vulkan, args),
        "vkResetDescriptorPool" => descriptor::reset_descriptor_pool(c, &at, &vulkan, args),
        "vkAllocateDescriptorSets" => descriptor::allocate_descriptor_sets(c, &at, &vulkan, args),
        "vkFreeDescriptorSets" => descriptor::free_descriptor_sets(c, &at, &vulkan, args),
        "vkUpdateDescriptorSets" => descriptor::update_descriptor_sets(c, &at, &vulkan, args),
        "vkCmdBeginRenderPass" => draw::cmd_begin_render_pass(c, &at, &vulkan, args),
        "vkCmdEndRenderPass" => draw::cmd_end_render_pass(c, &at, &vulkan, args),
        "vkCmdBindPipeline" => draw::cmd_bind_pipeline(c, &at, &vulkan, args),
        "vkCmdBindVertexBuffers" => draw::cmd_bind_vertex_buffers(c, &at, &vulkan, args),
        "vkCmdBindIndexBuffer" => draw::cmd_bind_index_buffer(c, &at, &vulkan, args),
        "vkCmdBindDescriptorSets" => draw::cmd_bind_descriptor_sets(c, &at, &vulkan, args),
        "vkCmdSetViewport" => draw::cmd_set_viewport(c, &at, &vulkan, args),
        "vkCmdSetScissor" => draw::cmd_set_scissor(c, &at, &vulkan, args),
        "vkCmdDraw" => draw::cmd_draw(c, &at, &vulkan, args),
        "vkCmdDispatch" => draw::cmd_dispatch(c, &at, &vulkan, args),
        "vkCmdCopyImage" => draw::cmd_copy_image(c, &at, &vulkan, args),
        "vkCmdBlitImage" => draw::cmd_blit_image(c, &at, &vulkan, args),
        "vkCmdDrawIndexed" => draw::cmd_draw_indexed(c, &at, &vulkan, args),
        "vkCmdCopyBuffer" => draw::cmd_copy_buffer(c, &at, &vulkan, args),
        "vkCmdCopyBufferToImage" => draw::cmd_copy_buffer_to_image(c, &at, &vulkan, args),
        "vkCmdPushConstants" => draw::cmd_push_constants(c, &at, &vulkan, args),
        // Everything else. **Still the measurement**: the refusal names the Vulkan function and
        // quotes `x0`-`x7`, and `Vulkan::names()` is the ordered list that says what to build
        // next.
        _ => Err(instance::unimplemented(&at, &name, args)),
    }
}

// ----------------------------------------------------------------------------- the activation

thread_local! {
    // A `thread_local!` invocation carries no rustdoc, so the documentation is here: this is the
    // `Vulkan` published to one thread, for the reason `Vulkan`'s own documentation gives -- an
    // `ImportFn` is a bare `fn` and a handler reaches per-instance state no other way.
    static ACTIVE: RefCell<Option<ActiveVulkan>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct ActiveVulkan {
    vulkan: Arc<Vulkan>,
}

/// Restores the previously published instance when dropped.
pub struct VulkanActivation {
    previous: Option<ActiveVulkan>,
}

impl Drop for VulkanActivation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

impl core::fmt::Debug for VulkanActivation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VulkanActivation")
    }
}

/// The instance published to this thread, or a typed refusal naming the Vulkan call.
pub(crate) fn active(symbol: &str, address: GuestAddr) -> AbiResult<Arc<Vulkan>> {
    ACTIVE.with(|cell| cell.borrow().clone()).map(|active| active.vulkan).ok_or_else(|| {
        AbiError::Refused {
            symbol: symbol.to_string(),
            address,
            why: "no Vulkan loader instance is published to this thread. `Vulkan::activate` \
                  publishes one for as long as its guard is held, and a created guest thread \
                  carries one only if `Vulkan::thread_instance` was given to the `ThreadHost` -- \
                  which is the case that matters, because the engine's renderer bring-up runs on \
                  the game thread rather than on the thread that called `initializeNativeCode`"
                .to_string(),
        }
    })
}

/// A [`Vulkan`] as something a **created guest thread** carries.
///
/// See [`ThreadLocalInstance`](crate::bionic::ThreadLocalInstance). A wrapper rather than an impl
/// on `Vulkan` itself for [`NdkThreadInstance`](crate::ndk)'s reason: publishing an instance means
/// cloning the `Arc`, and a `&self` cannot produce one.
struct VulkanThreadInstance(Arc<Vulkan>);

impl core::fmt::Debug for VulkanThreadInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VulkanThreadInstance")
    }
}

impl crate::bionic::ThreadLocalInstance for VulkanThreadInstance {
    fn name(&self) -> &'static str {
        "Vulkan"
    }

    fn publish(&self) -> AbiResult<Box<dyn core::any::Any>> {
        Ok(Box::new(self.0.activate()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five names are the specification's five, spelled exactly as `vkGetInstanceProcAddr`
    /// will be handed them.
    ///
    /// Membership rather than a count (`VERIFICATION.md` entry 1): a list of the right length with
    /// one name misspelled would answer NULL for a command the engine is required to get a
    /// pointer for, and `vkCreateInstance` answering NULL is renderer selection failing with no
    /// diagnostic anywhere.
    #[test]
    fn the_null_instance_commands_are_the_specification_s_five() {
        let mut sorted = NULL_INSTANCE_COMMANDS;
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            [
                "vkCreateInstance",
                "vkEnumerateInstanceExtensionProperties",
                "vkEnumerateInstanceLayerProperties",
                "vkEnumerateInstanceVersion",
                "vkGetInstanceProcAddr",
            ]
        );
    }

    /// `bound_symbols` and `BOUND_SYMBOLS` are derived from the same bound, so a host that sizes
    /// its thunk region from the constant cannot be given a different number of slots.
    #[test]
    fn the_symbol_count_a_host_sizes_with_is_the_number_of_symbols_bound() {
        assert_eq!(bound_symbols().count(), BOUND_SYMBOLS);
        assert_eq!(BOUND_SYMBOLS, 1 + MAX_PROC_SLOTS);
        // Distinct, or two pool slots would share one address and the engine would be handed the
        // same pointer for two different Vulkan functions.
        let names: std::collections::BTreeSet<String> = bound_symbols().collect();
        assert_eq!(names.len(), BOUND_SYMBOLS);
    }

    /// The pool slot names cannot be mistaken for Vulkan names or for anything in `.dynstr`.
    #[test]
    fn a_pool_slot_symbol_is_not_a_c_identifier() {
        let name = proc_slot_symbol(7);
        assert_eq!(name, "vulkan::proc[7]");
        assert!(!name.starts_with("vk"), "a pool slot must not look like a Vulkan function");
    }

    /// `dlsym` on a loader handle answers for exactly one name, which is the one the decoded
    /// bootstrap at `0x2595198` asks for.
    #[test]
    fn the_loader_exports_only_the_entry_point() {
        assert!(loader_exports("vkGetInstanceProcAddr"));
        assert!(!loader_exports("vkCreateInstance"));
        assert!(!loader_exports("vkGetDeviceProcAddr"));
        assert!(!loader_exports("eglGetProcAddress"));
    }

    /// An instance that recorded nothing **says so**, rather than printing an empty list.
    ///
    /// `VERIFICATION.md` entry 15's shape: a report that is blank when nothing happened is
    /// indistinguishable from a report that did not run.
    #[test]
    fn a_report_with_nothing_in_it_is_a_sentence_rather_than_a_blank() {
        let report = Vulkan::new().report();
        assert!(report.contains("0 lookup(s) recorded"), "{report}");
        assert!(report.contains("no Vulkan entry point was asked for"), "{report}");
        assert!(report.contains("no returned thunk was called"), "{report}");
        assert!(report.contains(&MAX_PROC_SLOTS.to_string()), "the bound is stated: {report}");
    }

    /// The two `soname`s are in the order `0x2595170` and `0x2595184` try them.
    #[test]
    fn the_sonames_are_in_the_order_the_engine_tries_them() {
        assert_eq!(LOADER_SONAMES, ["libvulkan.so.1", "libvulkan.so"]);
    }

    /// **Every registry together fits the 4 KB data area every embedding in this workspace
    /// passes**, with room left over.
    ///
    /// The check [`REGISTRY_BYTES`] became a budget for. Stage 4's first draft came to 8,640
    /// bytes and made every existing Vulkan test fail at `bind_into` with `RegionFull` — a true
    /// failure, arriving at the least useful moment. This asserts the arithmetic instead, so that
    /// raising one of the twelve bounds is a failing test here rather than a boundary that will
    /// not bind, and so that a reader can see how much room is left before the next family has to
    /// come with a larger data area.
    ///
    /// The margin is real rather than nominal: `REGISTRY_BYTES` is what *this* module declares,
    /// and an embedding's `ndk` and `jni` data symbols come out of the same 4096.
    #[test]
    fn the_registries_fit_the_data_area_every_embedding_passes() {
        // Stage 3's five: 64 bytes of `VkInstance` registry and 512 of the other four.
        assert_eq!(MAX_INSTANCES * INSTANCE_SLOT_BYTES, 64);
        assert_eq!((MAX_PHYSICAL_DEVICES + MAX_SURFACES + MAX_DEVICES + MAX_QUEUES) * SLOT, 512);
        // Stage 4's seven.
        let stage_four = (MAX_SWAPCHAINS
            + MAX_IMAGES
            + MAX_IMAGE_VIEWS
            + MAX_SEMAPHORES
            + MAX_FENCES
            + MAX_COMMAND_POOLS
            + MAX_COMMAND_BUFFERS)
            * SLOT;
        assert_eq!(stage_four, 14_632 * SLOT);
        // Stage 5's thirteen, which is what pushed the total past the old 4096.
        let stage_five = (MAX_DEVICE_MEMORIES
            + MAX_BUFFERS
            + MAX_CREATED_IMAGES
            + MAX_SAMPLERS
            + MAX_SHADER_MODULES
            + MAX_PIPELINE_LAYOUTS
            + MAX_RENDER_PASSES
            + MAX_FRAMEBUFFERS
            + MAX_PIPELINES
            + MAX_PIPELINE_CACHES
            + MAX_DESCRIPTOR_SET_LAYOUTS
            + MAX_DESCRIPTOR_POOLS
            + MAX_DESCRIPTOR_SETS)
            * SLOT;
        assert_eq!(stage_five, 64_420 * SLOT, "64,420 slots of {SLOT} bytes");
        // The engine's own renderer: its GPU timer's query pools.
        let engine = (MAX_QUERY_POOLS + MAX_DESCRIPTOR_UPDATE_TEMPLATES) * SLOT;
        assert_eq!(engine, 16_448);
        assert_eq!(REGISTRY_BYTES, 64 + 512 + stage_four + stage_five + engine);
        assert_eq!(REGISTRY_BYTES, 1_281_856);

        // **The old area, as the subtraction that says why it had to grow.** 4096 - 3648 left 448
        // bytes after stage 4, which is 28 slots — fewer than two per stage 5 family, and a
        // registry with one slot refuses the second object of its kind. There is no arrangement
        // of thirteen families that fits, which is what makes `REQUIRED_DATA_BYTES` a change to
        // every embedding rather than a bound to squeeze.
        assert!(stage_five > 4096, "stage 5 alone does not fit the first area");

        // **The margin, as a subtraction rather than as `<`.** A comparison between two constants
        // is one the compiler folds away — clippy's `assertions_on_constants` names it — and the
        // number a reader actually wants is how much room is left, not that there is some. The
        // `ndk` and `jni` data symbols come out of the same area, so this is the whole budget for
        // everything else; raising a Vulkan bound past it means raising the data area every
        // embedding passes to `BoundaryBuilder::new` **again**, which is a change to those
        // embeddings and not to this constant.
        assert_eq!(REQUIRED_DATA_BYTES, 2_097_152);
        assert_eq!(
            REQUIRED_DATA_BYTES - REGISTRY_BYTES,
            815_296,
            "what is left for everything else"
        );
    }
}
