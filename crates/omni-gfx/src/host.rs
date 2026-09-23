//! [`GfxVulkanHost`]: the real driver behind the guest's `libvulkan.so`.
//!
//! # Which side of the seam this is
//!
//! `omni_android::vulkan` owns the guest-facing half — the `dlopen`, the thunk pool, the census,
//! the five handle registries and the substitution log — and it owns none of the driver. This file
//! is the other half: an `ash::Entry` loaded at run time, and every question the [`VulkanHost`]
//! trait asks, answered by a real Vulkan implementation.
//!
//! The dependency runs **this way round on purpose**, and the trait's own documentation carries
//! the argument: `ash`'s `loaded` feature pulls `libloading`, which this crate's manifest signs
//! off as a documented Global Constraint 4 exception *inside this crate*. `omni-android` must
//! build `cfg`-free for five targets and must keep `cargo tree -p omni-android -e normal` free of
//! `ash`, so it defines the trait and this crate implements it — the shape `AssetSource`,
//! `WindowSource`, `ThreadHost` and `HwcapPolicy` already have.
//!
//! # Nothing here destroys anything on the guest's behalf, and that is what keeps [`Drop`] right
//!
//! A destructor is in the trait **only once the guest was measured calling it**: the decoded
//! bootstrap at guest `0x02595160` resolves two names, and stage 3 implements the set a renderer
//! needs in order to *reach* a device. A guest that calls a destructor this host lacks gets a
//! refusal naming the function from the thunk, which is the honest answer and is also what makes
//! this file's tables exactly what this host made — nothing can have gone away behind their back.
//!
//! `vkDestroySurfaceKHR`, `vkDestroyDevice` and `vkDestroyInstance` were measured. The engine's
//! render thread answers `APP_CMD_TERM_WINDOW` — the app closed the way a device closes it — by
//! destroying its swapchain and its surface, saving its pipeline cache, destroying its device and
//! then its instance. The guest's own destroys go through [`GfxVulkanHost::destroy_surface`],
//! [`GfxVulkanHost::destroy_device`] and [`GfxVulkanHost::destroy_instance`], which empty the
//! table entries they destroy.
//!
//! So [`Drop`] destroys whatever is left, and it runs the whole tree in Vulkan's required order:
//! devices (each waited on first), then surfaces, then instances. Queues are not destroyed and
//! never could be — a `VkQueue` is owned by its device and goes away with it.
//!
//! # Validation layers
//!
//! None are installed on this host (`docs/research/graphics-spike.md` §6), and this file **does
//! not add any**. That is a deliberate difference from [`Renderer::new`](crate::vulkan::Renderer),
//! which enables `VK_LAYER_KHRONOS_validation` when it finds it: the instance created here is the
//! *guest's*, made from the guest's own `VkInstanceCreateInfo`, and enabling a layer the engine
//! did not ask for would change the instance it gets. If the guest asks for a layer, it is
//! forwarded; if the driver does not have it, the driver's `VK_ERROR_LAYER_NOT_PRESENT` is what
//! the guest receives.

use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard};

use ash::khr;
use ash::vk;
use omni_android::vulkan::{
    flat_structure, Acquired, BufferRequest, ChainLink, ComputePipelineRequest, DescriptorCopy,
    ImageFormatQuery, DescriptorPoolRequest, DescriptorSetLayoutRequest,
    DescriptorWrite, DescriptorWrites, DeviceRequest, DriverAnswer, FramebufferRequest,
    GraphicsPipelineRequest, HostBuffer, HostCommandBuffer, HostCommandPool, HostCreatedImage,
    HostDescriptorPool, HostDescriptorSet, HostDescriptorSetLayout, HostDevice, HostDeviceMemory,
    HostExtension, HostFence, HostFramebuffer, HostImage, HostImageRef, HostImageView,
    HostInstance, HostPhysicalDevice, HostPipeline, HostPipelineCache, HostPipelineLayout,
    HostQueryPool, QueryPoolRequest,
    HostQueue, HostRenderPass, HostSampler, HostSemaphore, HostShaderModule, HostSurface,
    HostSwapchain, ImageRequest, ImageViewRequest, InstanceRequest, MemoryAllocation, MemoryPlan,
    PipelineBarrier, PipelineLayoutRequest, PipelinesCreated, PresentRequest, Presented,
    RenderPassBegin, RenderPassRequest, ShaderStage, SubmitRequest, SurfaceCreated,
    SwapchainRequest, VulkanHost,
};
use omni_android::{AbiError, AbiResult};
use omni_platform::window::RawWindow;

use crate::error::{GfxError, GfxResult};
use crate::select;

/// The window-system-integration extensions Vulkan defines, in the order this host prefers them.
///
/// **A probe list, not a claim about this machine.** [`GfxVulkanHost::platform_surface_extension`]
/// answers with the first of these the *driver actually reports*, so the answer is a measurement
/// of the host's loader rather than a `cfg` or a guess — which matters, because that answer is one
/// half of the name substitution `omni_android::vulkan::rewrite` records, and a substitution built
/// on an assumption would be a rename nobody could check.
///
/// `VK_KHR_android_surface` is last and is in the list on purpose: on a genuine Android host it is
/// what would be found, the guest's name and the host's name are then the same, and the rewrite
/// layer records **no** substitution because none happened.
pub const PLATFORM_SURFACE_EXTENSIONS: [&str; 6] = [
    "VK_KHR_win32_surface",
    "VK_KHR_xlib_surface",
    "VK_KHR_xcb_surface",
    "VK_KHR_wayland_surface",
    "VK_EXT_metal_surface",
    "VK_KHR_android_surface",
];

/// The surface-creation entry point each of [`PLATFORM_SURFACE_EXTENSIONS`] provides, **in the
/// same order**.
///
/// # Why this is a second array and not a lookup
///
/// [`GfxVulkanHost::platform_surface_extension`] and
/// [`GfxVulkanHost::platform_surface_entry_point`] have to answer about the *same* platform, and
/// index correspondence is the cheapest way to make that structural: both methods run the same
/// probe and index the same position, so there is no arrangement in which one says
/// `VK_KHR_win32_surface` and the other says `vkCreateXlibSurfaceKHR`. A test asserts the pairing
/// entry by entry, because the failure it prevents is one this host could not otherwise notice —
/// `omni-android` would hand the guest a thunk on the strength of a command the surface it
/// eventually creates is not made by.
///
/// `VK_KHR_android_surface` maps to `vkCreateAndroidSurfaceKHR`, which is the guest's own name: on
/// a genuine Android host there is nothing to substitute and
/// `omni_android::vulkan::rewrite` records none, exactly as it does for the extension.
pub const PLATFORM_SURFACE_ENTRY_POINTS: [&str; 6] = [
    "vkCreateWin32SurfaceKHR",
    "vkCreateXlibSurfaceKHR",
    "vkCreateXcbSurfaceKHR",
    "vkCreateWaylandSurfaceKHR",
    "vkCreateMetalSurfaceEXT",
    "vkCreateAndroidSurfaceKHR",
];

/// How many **live** `VkInstance`s this host will hold before it refuses a new one.
///
/// An allocation bound rather than a Vulkan limit. `omni_android::vulkan::MAX_INSTANCES` bounds
/// the guest-visible registry at four; this is deliberately a little larger, so that the refusal a
/// guest sees comes from the registry — which can name the handle and the count — rather than from
/// here, where the only thing that could be said is "the host is full".
///
/// **Live, not ever created**, since `vkDestroyInstance`: the engine destroys its instance with
/// its window and creates a new one when the window comes back, so a count of every instance ever
/// made would refuse the ninth return to the foreground.
pub const MAX_INSTANCES: usize = 8;

/// A real Vulkan driver, as [`VulkanHost`].
///
/// Give one to a loader with `Vulkan::set_host`, and keep it alive for at least as long as the
/// guest: [`Drop`] destroys every instance it created, so dropping it while the guest still holds
/// a `VkInstance` handle leaves that handle naming a token this host no longer has — which is a
/// refusal by name at the next call, not a crash, because the handle the guest holds is an arena
/// address and never the driver's pointer.
pub struct GfxVulkanHost {
    entry: ash::Entry,
    /// Every instance this host created, indexed by [`HostInstance`] token, and `None` once the
    /// guest has destroyed it.
    ///
    /// **Emptied, never removed from and never refilled**, for [`GfxVulkanHost::devices`]'
    /// reason: a token is an index, so removing an entry would make a later token name an earlier
    /// instance, and refilling a slot would make a destroyed instance's token -- and every
    /// surface, device and physical-device token that carries its index -- name a new one.
    instances: Mutex<Vec<Option<ash::Instance>>>,
    /// How each instance can read `VkPhysicalDeviceFeatures2`, indexed by instance token and
    /// pushed with it: what a device's portability subset needs (`crate::portability`). A leaf
    /// lock, taken alone and never while holding another.
    features2: Mutex<Vec<crate::portability::Features2>>,
    /// Each instance's physical devices, **cached on first enumeration**, indexed by instance
    /// token.
    ///
    /// # Why this is a cache and not a call
    ///
    /// `VulkanHost::physical_devices` is asked twice by every renderer — once for the count and
    /// once for the array — and the guest is required to receive the same `VkPhysicalDevice`
    /// handles both times. The token this host mints is `(instance << 32) | index`, so "the same
    /// handle" means "the same index in the same list", and re-enumerating would only be safe if
    /// `vkEnumeratePhysicalDevices` promised a stable order. **It does not.** So the driver is
    /// asked once and the order is frozen here, which makes the promise this host's own rather
    /// than one borrowed from a driver that never made it.
    physical: Mutex<Vec<Vec<vk::PhysicalDevice>>>,
    /// Every surface this host created and the guest has not destroyed.
    ///
    /// **A [`Slab`], unlike the other stage 3 tables**, because it is the one stage 3 family the
    /// guest destroys: `vkDestroySurfaceKHR` removes the entry, and the generation tag is what
    /// makes the destroyed surface's token a refusal rather than a name for the next surface the
    /// Android lifecycle creates in that slot.
    surfaces: Mutex<Slab<SurfaceEntry>>,
    /// Every logical device this host created, indexed by [`HostDevice`] token, and `None` once
    /// the guest has destroyed it.
    ///
    /// **Emptied, never removed from and never refilled**: `vkDestroyDevice` leaves the slot
    /// `None`, and `vkCreateDevice` only ever appends. So a token is an index that names one device
    /// for the life of this host, and a destroyed device's token -- and every stage 4 and 5
    /// entry's `device` index -- is a refusal rather than a name for a later device. Not a
    /// [`Slab`]: every child table stores this index, and a slot that is never reused needs no
    /// generation to tell its occupants apart. A device is made once per window, so the growth is
    /// one slot per foreground.
    devices: Mutex<Vec<Option<DeviceEntry>>>,
    /// Every queue this host has handed out, indexed by [`HostQueue`] token, and `None` once its
    /// device is destroyed. Emptied and never refilled, for [`GfxVulkanHost::devices`]' reason.
    queues: Mutex<Vec<Option<QueueEntry>>>,

    // ------------------------------------------------------------------------ stage 4
    //
    // **Seven [`Slab`]s rather than seven `Vec`s**, because these are the first objects the guest
    // destroys and a table that only grows is not the right shape for something a resize recreates
    // once per frame. `Slab`'s documentation carries the argument for the generation tag.
    /// Every swapchain this host created.
    swapchains: Mutex<Slab<SwapchainEntry>>,
    /// Every swapchain image this host has handed out a token for.
    ///
    /// **Not created and not destroyed here**: a swapchain image belongs to its swapchain, and
    /// these entries are removed when the swapchain is.
    images: Mutex<Slab<ImageEntry>>,
    /// Every image view this host created.
    image_views: Mutex<Slab<ObjectEntry<vk::ImageView>>>,
    /// Every semaphore this host created.
    semaphores: Mutex<Slab<ObjectEntry<vk::Semaphore>>>,
    /// Every fence this host created.
    fences: Mutex<Slab<ObjectEntry<vk::Fence>>>,
    /// Every command pool this host created.
    command_pools: Mutex<Slab<ObjectEntry<vk::CommandPool>>>,
    /// Every command buffer this host allocated.
    command_buffers: Mutex<Slab<CommandBufferEntry>>,

    // ------------------------------------------------------------------------ stage 5
    //
    // Thirteen more [`Slab`]s, for stage 4's reason. The only one that is not an
    // [`ObjectEntry`] is `device_memories`, because an allocation carries one extra fact nothing
    // else does: whether its pages came out of `GuestSpace` and were **imported**, which is what
    // `vkMapMemory` rests on and what `omni-android` needs told back to it.
    /// Every `VkDeviceMemory` this host allocated.
    device_memories: Mutex<Slab<MemoryEntry>>,
    /// Every `VkBuffer` this host created.
    buffers: Mutex<Slab<ObjectEntry<vk::Buffer>>>,
    /// Every `VkImage` the **guest** created. Not the swapchain's; see [`GfxVulkanHost::images`].
    created_images: Mutex<Slab<ObjectEntry<vk::Image>>>,
    /// Every `VkSampler` this host created.
    samplers: Mutex<Slab<ObjectEntry<vk::Sampler>>>,
    /// Every `VkShaderModule` this host created.
    shader_modules: Mutex<Slab<ObjectEntry<vk::ShaderModule>>>,
    /// Every `VkPipelineLayout` this host created.
    pipeline_layouts: Mutex<Slab<ObjectEntry<vk::PipelineLayout>>>,
    /// Every `VkRenderPass` this host created.
    render_passes: Mutex<Slab<ObjectEntry<vk::RenderPass>>>,
    /// Every `VkFramebuffer` this host created.
    framebuffers: Mutex<Slab<ObjectEntry<vk::Framebuffer>>>,
    /// Every `VkPipeline` this host created.
    pipelines: Mutex<Slab<ObjectEntry<vk::Pipeline>>>,
    /// Every `VkPipelineCache` this host created.
    pipeline_caches: Mutex<Slab<ObjectEntry<vk::PipelineCache>>>,
    /// **What keeps a pipeline cache the size the driver just reported**, while
    /// `vkGetPipelineCacheData` reads it.
    ///
    /// Building a pipeline through a cache grows it, and a cache is internally synchronized, so
    /// another guest thread may do that between the size query and the fill. The fill's buffer is
    /// then short, and a short buffer is the path on which this machine's NVIDIA driver was
    /// measured writing past the end: 2,733 bytes past a 2,766-byte buffer, and
    /// `STATUS_HEAP_CORRUPTION`. Every pipeline creation that names a cache holds the shared side;
    /// [`GfxVulkanHost::pipeline_cache_data`] holds the exclusive side across both calls. A call
    /// added later that grows a cache -- `vkMergePipelineCaches` -- must hold the shared side too.
    cache_gate: RwLock<()>,
    /// Every `VkQueryPool` this host created.
    query_pools: Mutex<Slab<ObjectEntry<vk::QueryPool>>>,
    /// Every `VkDescriptorSetLayout` this host created.
    descriptor_set_layouts: Mutex<Slab<ObjectEntry<vk::DescriptorSetLayout>>>,
    /// Every `VkDescriptorPool` this host created.
    descriptor_pools: Mutex<Slab<ObjectEntry<vk::DescriptorPool>>>,
    /// Every `VkDescriptorSet` this host allocated, each remembering the pool it came from — for
    /// [`CommandBufferEntry::pool`]'s reason, one family along.
    descriptor_sets: Mutex<Slab<DescriptorSetEntry>>,
    /// What `VK_EXT_external_memory_host` answered about each physical device, cached.
    ///
    /// See [`GfxVulkanHost::probe_importable`]: the probe costs a throwaway `VkDevice`, the answer
    /// cannot change for the life of a physical device, and `vkGetPhysicalDeviceMemoryProperties`
    /// is a call an engine may make once per allocation.
    importable: Mutex<Vec<ImportableProbe>>,
}

/// One `VkDeviceMemory`, and **whether its bytes are the guest's**.
///
/// `imported` is the whole of stage 5's memory decision made visible in one field: `true` means
/// the pages were mapped out of `GuestSpace` by `omni-android` and handed here through
/// [`MemoryAllocation::host_pointer`], so `vkMapMemory` gives back an address the guest may store
/// through; `false` means an ordinary driver allocation the guest may not map at all.
struct MemoryEntry {
    device: usize,
    memory: vk::DeviceMemory,
    imported: bool,
}

/// One `VkDescriptorSet`, the pool it came from, and the device that pool belongs to.
struct DescriptorSetEntry {
    device: usize,
    /// The [`HostDescriptorPool`] token, so that `vkDestroyDescriptorPool` and
    /// `vkResetDescriptorPool` can find every set they are about to free — the specification frees
    /// them without naming one, exactly as a command pool frees its buffers.
    pool: u64,
    set: vk::DescriptorSet,
}

/// What one physical device answered about importing host pointers, measured once.
struct ImportableProbe {
    /// The physical device, as its raw handle — which is what identifies it across the instance
    /// and device tables without needing an index into either.
    physical: u64,
    /// `vkGetMemoryHostPointerPropertiesEXT`'s `memoryTypeBits` for ordinary host memory, or 0
    /// when this device has no `VK_EXT_external_memory_host`.
    memory_type_bits: u32,
    /// `minImportedHostPointerAlignment`, or [`CONSERVATIVE_IMPORT_ALIGNMENT`] when the instance
    /// could not be asked.
    alignment: u64,
}

/// One device-owned object whose only interesting property is which device it came from.
///
/// Four of stage 4's seven families are exactly this — `VkImageView`, `VkSemaphore`, `VkFence` and
/// `VkCommandPool` — and writing four structurally identical structs would have been four places
/// for the device index to be stored wrongly. What they are **not** is interchangeable: each has
/// its own `Slab`, so a `VkFence` token and a `VkSemaphore` token with the same numeric value name
/// entries in different tables and neither can be found in the other's.
struct ObjectEntry<T> {
    device: usize,
    object: T,
}

/// One swapchain, everything needed to use it, and the window claim it holds.
struct SwapchainEntry {
    device: usize,
    /// Which [`SurfaceEntry`] this was created over, as its [`HostSurface`] token. Checked against
    /// `oldSwapchain`'s, and by `vkDestroySurfaceKHR`, which must not destroy a surface a
    /// swapchain — retired or not — was created over.
    surface: u64,
    handle: vk::SwapchainKHR,
    /// What the guest asked for, kept because the read-back path needs them and there is no
    /// `vkGetSwapchainCreateInfoKHR` to ask.
    format: vk::Format,
    extent: vk::Extent2D,
    /// The driver's images, **cached on first enumeration**, for
    /// [`GfxVulkanHost::physical`]'s reason: the guest is required to receive the same `VkImage`
    /// handles from both halves of the two-call protocol, and `vkGetSwapchainImagesKHR` makes no
    /// promise about order across calls.
    images: Vec<vk::Image>,
    /// The [`HostImage`] token of each of those images, in the same order, minted once.
    image_tokens: Vec<u64>,
    /// The exclusive claim on this swapchain's window, or `None` for a swapchain that has been
    /// **retired** by being passed as `oldSwapchain` — which transferred the claim to its
    /// replacement. A retired swapchain still exists and must still be destroyed by the guest; it
    /// simply no longer owns the window.
    claim: Option<crate::claim::WindowClaim>,
}

/// One swapchain image, named by the swapchain it belongs to and its index in it.
///
/// The index is what makes this more than a handle: it is the `imageIndex`
/// `vkAcquireNextImageKHR` answers with, and it is what [`GfxVulkanHost::read_presented_image`]
/// uses to find the image whose pixels are on the screen.
struct ImageEntry {
    swapchain: u64,
    index: u32,
    image: vk::Image,
}

/// One command buffer, the pool it came from, and the device that pool belongs to.
struct CommandBufferEntry {
    device: usize,
    /// The [`HostCommandPool`] token, so that `vkDestroyCommandPool` can find every buffer it is
    /// about to free and `vkFreeCommandBuffers` can check that a buffer belongs to the pool it was
    /// named with — which the specification makes undefined behaviour and which no validation
    /// layer on this machine would catch.
    pool: u64,
    buffer: vk::CommandBuffer,
}

/// One surface, the instance it belongs to, and the window it is over.
///
/// The instance matters for two reasons and both are checked: destroying it needs that instance's
/// `VK_KHR_surface` function table, and pairing it with a `VkPhysicalDevice` from a *different*
/// instance is undefined behaviour the specification does not require a driver to catch.
///
/// The **window** matters for a third, which stage 4 added: a native window may be associated with
/// at most one swapchain at a time, and this is where `vkCreateSwapchainKHR` finds out which
/// window it is about to take. See [`crate::claim`].
struct SurfaceEntry {
    instance: usize,
    surface: vk::SurfaceKHR,
    window: crate::claim::WindowKey,
}

/// A table of objects addressed by a **generation-tagged** token.
///
/// # Why stage 4 needed this and stage 3 did not
///
/// Stage 3's tables are plain `Vec`s that are never removed from, because nothing it implemented
/// destroyed anything. A token was an index, and an index into a vector that only grows is stable
/// forever. The surface table became one of these when `vkDestroySurfaceKHR` was measured; the
/// device and queue tables, when `vkDestroyDevice` was, instead empty a slot and never refill it,
/// because every child table stores a device's index ([`GfxVulkanHost::devices`]).
///
/// Stage 4 is the first stage whose objects the guest genuinely destroys, once per frame in the
/// case of a swapchain that follows a resize. Two ways of handling that are wrong and the
/// difference between them is worth stating:
///
/// * **Never reuse a slot.** Tokens stay stable and the vector grows without bound — a window
///   dragged for a minute is thousands of dead entries, and it never shrinks.
/// * **Reuse a slot with a bare index as the token.** The vector stays small and a *stale* token
///   silently names the new occupant. That is the aliasing failure the guest-side registry's
///   `Handles::remove` documentation warns about, moved to the host side where nothing would
///   catch it.
///
/// So the token is `(generation << 32) | index`: slots are reused, and reusing one **bumps its
/// generation**, so a token minted for the old occupant no longer matches. A stale token becomes a
/// refusal naming it rather than a different object. The shape is the one
/// [`GfxVulkanHost::split`] already uses for physical devices, where the high half carries the
/// instance instead.
///
/// `omni-android` never looks inside a token — [`HostSwapchain`]'s `Debug` prints it as an
/// ordinal, and its documentation says it is not a Vulkan handle.
struct Slab<T> {
    entries: Vec<Option<(u32, T)>>,
    /// The generation to stamp the next occupant of each slot with. Held per slot rather than
    /// globally so that a long-lived object's neighbours churning does not invalidate it.
    generations: Vec<u32>,
}

impl<T> Slab<T> {
    fn new() -> Slab<T> {
        Slab { entries: Vec::new(), generations: Vec::new() }
    }

    /// Split a token into its generation and its index.
    fn split(token: u64) -> (u32, usize) {
        ((token >> 32) as u32, (token & 0xFFFF_FFFF) as usize)
    }

    /// Put `value` in the lowest free slot and answer its token.
    fn insert(&mut self, value: T) -> u64 {
        let index = match self.entries.iter().position(Option::is_none) {
            Some(index) => index,
            None => {
                self.entries.push(None);
                // A fresh slot starts at generation 1 rather than 0, so that a token is never
                // numerically equal to a bare index. That is not a correctness requirement; it
                // makes a token that *was* being used as an index somewhere visibly wrong.
                self.generations.push(1);
                self.entries.len() - 1
            }
        };
        let generation = self.generations[index];
        self.entries[index] = Some((generation, value));
        (u64::from(generation) << 32) | index as u64
    }

    fn get(&self, token: u64) -> Option<&T> {
        let (generation, index) = Self::split(token);
        match self.entries.get(index) {
            Some(Some((held, value))) if *held == generation => Some(value),
            _ => None,
        }
    }

    fn get_mut(&mut self, token: u64) -> Option<&mut T> {
        let (generation, index) = Self::split(token);
        match self.entries.get_mut(index) {
            Some(Some((held, value))) if *held == generation => Some(value),
            _ => None,
        }
    }

    /// Take the value a token names, and bump the slot's generation so the token names nothing.
    fn remove(&mut self, token: u64) -> Option<T> {
        let (generation, index) = Self::split(token);
        let slot = self.entries.get_mut(index)?;
        match slot {
            Some((held, _)) if *held == generation => {
                let (_, value) = slot.take()?;
                // Saturating rather than wrapping: a slot reused four billion times would
                // otherwise wrap back onto a generation some very old token still carries. Nothing
                // will reach it, and "nothing will reach it" is why the cheap arithmetic is the
                // one to get right rather than the one to argue about.
                self.generations[index] = self.generations[index].saturating_add(1);
                Some(value)
            }
            _ => None,
        }
    }

    /// Every live entry, with its token.
    fn iter(&self) -> impl Iterator<Item = (u64, &T)> + '_ {
        self.entries.iter().enumerate().filter_map(|(index, slot)| {
            slot.as_ref().map(|(generation, value)| {
                ((u64::from(*generation) << 32) | index as u64, value)
            })
        })
    }

    /// Take every live entry out, leaving the slab empty. What [`Drop`] uses.
    fn drain(&mut self) -> Vec<T> {
        self.entries.iter_mut().filter_map(|slot| slot.take().map(|(_, value)| value)).collect()
    }

    /// How many entries are live.
    fn len(&self) -> usize {
        self.entries.iter().filter(|slot| slot.is_some()).count()
    }
}

/// One logical device, what it was made from, and what queues were asked for.
struct DeviceEntry {
    /// Which [`HostInstance`] token's instance this came from — needed for
    /// `vkGetDeviceProcAddr`, which is an *instance*-level entry point.
    instance: usize,
    /// The physical device it was created from.
    ///
    /// **Stage 4 added this**, and the reason is the read-back path: choosing a host-visible
    /// memory type for a staging buffer means asking `vkGetPhysicalDeviceMemoryProperties`, and a
    /// logical device cannot be asked which physical device it came from. Keeping it here is the
    /// only place that fact can live without re-enumerating and hoping the order held.
    physical: vk::PhysicalDevice,
    device: ash::Device,
    /// `(queueFamilyIndex, queueCount)` for every family `vkCreateDevice` was asked for.
    ///
    /// **The only thing that can make `vkGetDeviceQueue` safe.** That call returns `void`, so
    /// there is no `VkResult` to report a family the device does not have — the specification
    /// makes it undefined behaviour, and on this machine there are no validation layers
    /// (`docs/research/graphics-spike.md` §6) so nothing would report it at all. The request is
    /// kept here and checked, and a queue that was not created is a refusal naming the families
    /// that were.
    requested: Vec<(u32, u32)>,
}

/// One queue, identified the way the specification identifies it.
struct QueueEntry {
    device: usize,
    family: u32,
    index: u32,
    queue: vk::Queue,
}

impl GfxVulkanHost {
    /// Load the host's Vulkan loader and take it as a driver the guest can be forwarded to.
    ///
    /// # Errors
    ///
    /// [`GfxError::LoaderMissing`] when there is no Vulkan on this machine at all. **Loud, and
    /// never silent**: `ash` with `default-features = false, loaded` means this is a run-time
    /// `dlopen` rather than a link-time dependency, so a machine with no driver builds the
    /// workspace and fails here, naming what is missing — which is the whole point of that feature
    /// choice and is recorded in this crate's manifest.
    pub fn load() -> GfxResult<Arc<GfxVulkanHost>> {
        // The same loader `Renderer::new` finds: `Entry::load()` where the platform names no
        // locations (Windows, unchanged), otherwise each in turn. See `crate::portability`.
        let entry = crate::portability::load_entry().map_err(|detail| GfxError::LoaderMissing { detail })?;
        Ok(Arc::new(GfxVulkanHost {
            entry,
            instances: Mutex::new(Vec::new()),
            features2: Mutex::new(Vec::new()),
            physical: Mutex::new(Vec::new()),
            surfaces: Mutex::new(Slab::new()),
            devices: Mutex::new(Vec::new()),
            queues: Mutex::new(Vec::new()),
            swapchains: Mutex::new(Slab::new()),
            images: Mutex::new(Slab::new()),
            image_views: Mutex::new(Slab::new()),
            semaphores: Mutex::new(Slab::new()),
            fences: Mutex::new(Slab::new()),
            command_pools: Mutex::new(Slab::new()),
            command_buffers: Mutex::new(Slab::new()),
            device_memories: Mutex::new(Slab::new()),
            buffers: Mutex::new(Slab::new()),
            created_images: Mutex::new(Slab::new()),
            samplers: Mutex::new(Slab::new()),
            shader_modules: Mutex::new(Slab::new()),
            pipeline_layouts: Mutex::new(Slab::new()),
            render_passes: Mutex::new(Slab::new()),
            framebuffers: Mutex::new(Slab::new()),
            pipelines: Mutex::new(Slab::new()),
            pipeline_caches: Mutex::new(Slab::new()),
            cache_gate: RwLock::new(()),
            query_pools: Mutex::new(Slab::new()),
            descriptor_set_layouts: Mutex::new(Slab::new()),
            descriptor_pools: Mutex::new(Slab::new()),
            descriptor_sets: Mutex::new(Slab::new()),
            importable: Mutex::new(Vec::new()),
        }))
    }

    /// How many instances this host has created, destroyed ones included. Diagnostic (Global
    /// Constraint 6).
    #[must_use]
    pub fn instances_created(&self) -> usize {
        self.locked().len()
    }

    /// How many instances this host holds live: created and not destroyed by the guest.
    #[must_use]
    pub fn instances_live(&self) -> usize {
        self.locked().iter().flatten().count()
    }

    /// The driver's own name for a created instance's first physical device, if it has one.
    ///
    /// **The evidence that `vkCreateInstance` created something.** A `VK_SUCCESS` proves nothing
    /// on its own — Global Constraint 1 is about exactly that shape — so the live test asks the
    /// instance the guest was handed for a device name, and an NVIDIA string coming back out is a
    /// fact a fabricated success cannot produce.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if `instance` is not a token this host issued, or if the driver
    /// refused to enumerate.
    pub fn first_device_name(&self, instance: HostInstance) -> AbiResult<Option<String>> {
        let instances = self.locked();
        let instance = Self::lookup(&instances, instance)?;
        // SAFETY: `instance` is a live `ash::Instance` this host created and has not destroyed;
        // `enumerate_physical_devices` takes no other handle and writes into an `ash`-owned vector.
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|err| refused("vkEnumeratePhysicalDevices", &err.to_string()))?;
        let Some(&device) = devices.first() else { return Ok(None) };
        // SAFETY: `device` came from this instance's own enumeration one statement ago.
        let properties = unsafe { instance.get_physical_device_properties(device) };
        Ok(properties.device_name_as_c_str().ok().map(|name| name.to_string_lossy().into_owned()))
    }

    /// The instance table, recovering rather than propagating a poisoned lock.
    ///
    /// A poisoned mutex here would mean a panic inside this file while it was held, which is a
    /// host bug and not a guest one — and the response to it must not be that every later Vulkan
    /// call refuses for a reason unrelated to what the guest did. The data behind it is a `Vec` of
    /// handles with no invariant a panic could have broken halfway.
    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<Option<ash::Instance>>> {
        self.instances.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live `ash::Instance` a token names, or a refusal naming the token.
    fn lookup(
        instances: &[Option<ash::Instance>],
        instance: HostInstance,
    ) -> AbiResult<&ash::Instance> {
        usize::try_from(instance.token())
            .ok()
            .and_then(|index| instances.get(index))
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                refused(
                    "VulkanHost::lookup",
                    &format!(
                        "{instance:?} is not a live instance of this host -- it has issued {} -- \
                         so there is no `VkInstance` to forward to. An instance the guest has \
                         destroyed lands here, and so does a token this host did not mint, which \
                         means the loader's registry and this host disagree: that happens when \
                         `Vulkan::set_host` replaced one host with another while the guest still \
                         held a handle",
                        instances.len()
                    ),
                )
            })
    }

    /// Every instance extension the driver reports, as this seam's own type.
    fn enumerate(&self, layer: Option<&CStr>) -> Result<Vec<HostExtension>, vk::Result> {
        // SAFETY: the enumeration takes no handle and writes into an `ash`-owned vector; `layer`
        // is a `&CStr` that outlives the call.
        let properties = unsafe { self.entry.enumerate_instance_extension_properties(layer) }?;
        Ok(properties
            .iter()
            .filter_map(|property| {
                property.extension_name_as_c_str().ok().map(|name| HostExtension {
                    name: name.to_string_lossy().into_owned(),
                    spec_version: property.spec_version,
                })
            })
            .collect())
    }

    /// Which of [`PLATFORM_SURFACE_EXTENSIONS`] this host's loader actually reports, as an index.
    ///
    /// **One probe behind both halves of the pairing.** `platform_surface_extension` and
    /// `platform_surface_entry_point` must answer about the same platform, and both indexing the
    /// same result of the same function is what makes that true by construction rather than by
    /// two lists staying in step.
    ///
    /// Asked of the driver on every call rather than cached, and that is affordable for the reason
    /// the loader's census is ungated: this is entered tens of times in a process, not millions.
    /// Caching it would also make the answer a fact about the first call rather than about the
    /// loader, and a loader whose layers change under a running process is a real thing on Windows
    /// (an overlay being installed) even if nothing here has seen it happen.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the driver reports **no** window-system-integration extension at
    /// all. Loud rather than an `Option`, for the reason `VulkanHost::platform_surface_extension`
    /// gives: answering "the Android extension is simply absent" would send the engine down its
    /// own silent no-Vulkan fall-back with nothing anywhere recording that a choice was made.
    fn platform_index(&self) -> AbiResult<usize> {
        let available = self
            .enumerate(None)
            .map_err(|err| refused("vkEnumerateInstanceExtensionProperties", &err.to_string()))?;
        for (index, candidate) in PLATFORM_SURFACE_EXTENSIONS.iter().enumerate() {
            if available.iter().any(|extension| extension.name == *candidate) {
                return Ok(index);
            }
        }
        Err(refused(
            "VulkanHost::platform_surface_extension",
            &format!(
                "this host's Vulkan loader reports {} instance extension(s) and not one of them \
                 is a window-system-integration extension ({}). The guest's \
                 `VK_KHR_android_surface` has nothing to be substituted for, so there is no \
                 honest list to advertise -- and answering \"the Android extension is simply \
                 absent\" would send the engine down its own silent no-Vulkan fall-back with \
                 nothing anywhere recording that a choice was made. What the driver did report \
                 is: {}",
                available.len(),
                PLATFORM_SURFACE_EXTENSIONS.join(", "),
                available
                    .iter()
                    .map(|extension| extension.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
    }

    // ------------------------------------------------------------------- stage 3 internals

    /// How many surfaces, devices and queues this host holds **live**. Diagnostic (Global
    /// Constraint 6).
    #[must_use]
    pub fn objects(&self) -> (usize, usize, usize) {
        (
            self.surfaces.lock().unwrap_or_else(PoisonError::into_inner).len(),
            self.locked_devices().iter().flatten().count(),
            self.locked_queues().iter().flatten().count(),
        )
    }

    /// **The lock order, stated once because it is the only thing keeping four mutexes safe.**
    ///
    /// `instances` → `physical` → `surfaces` → `devices` → `queues`, always, and never upward.
    /// Every `with_*` helper below takes them in that order and releases them before returning, so
    /// no call path here can hold two in the opposite order. A fifth table added later goes at the
    /// end of the list or it does not go in.
    ///
    /// The alternative was one `Mutex<Tables>`, which is simpler and was rejected for a reason
    /// that is about `Drop` rather than about contention: destruction has to run devices before
    /// surfaces before instances, and a single guard held across all three would have to be
    /// released and retaken between them anyway.
    fn locked_physical(&self) -> std::sync::MutexGuard<'_, Vec<Vec<vk::PhysicalDevice>>> {
        self.physical.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_surfaces(&self) -> std::sync::MutexGuard<'_, Slab<SurfaceEntry>> {
        self.surfaces.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_devices(&self) -> std::sync::MutexGuard<'_, Vec<Option<DeviceEntry>>> {
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_queues(&self) -> std::sync::MutexGuard<'_, Vec<Option<QueueEntry>>> {
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live device a token or an index names. `None` for one never made **or destroyed**.
    fn live_device(devices: &[Option<DeviceEntry>], index: usize) -> Option<&DeviceEntry> {
        devices.get(index).and_then(Option::as_ref)
    }

    /// The live queue a token names. `None` for one never handed out or whose device is gone.
    fn live_queue(queues: &[Option<QueueEntry>], token: HostQueue) -> Option<&QueueEntry> {
        queues.get(usize::try_from(token.token()).unwrap_or(usize::MAX)).and_then(Option::as_ref)
    }

    /// A physical-device token as `(instance index, device index)`.
    ///
    /// The token is `(instance << 32) | index`, which is transparent on purpose: it means the
    /// association between a device and its instance travels *in the token* and cannot be lost by
    /// a table going out of step with another. `omni-android` never looks inside it — it is an
    /// ordinal there, and [`HostPhysicalDevice`]'s `Debug` prints it as one.
    fn split(token: HostPhysicalDevice) -> (usize, usize) {
        let raw = token.token();
        ((raw >> 32) as usize, (raw & 0xFFFF_FFFF) as usize)
    }

    /// Run `f` with the instance and physical device a token names.
    fn with_physical<R>(
        &self,
        token: HostPhysicalDevice,
        f: impl FnOnce(&ash::Instance, vk::PhysicalDevice) -> R,
    ) -> AbiResult<R> {
        let (instance_index, device_index) = Self::split(token);
        let instances = self.locked();
        let instance = Self::lookup(&instances, HostInstance::from_token(instance_index as u64))?;
        let physical = self.locked_physical();
        let device = physical
            .get(instance_index)
            .and_then(|list| list.get(device_index))
            .copied()
            .ok_or_else(|| {
                refused(
                    "VulkanHost::with_physical",
                    &format!(
                        "{token:?} names physical device {device_index} of instance \
                         {instance_index}, and this host has enumerated {have} device(s) for that \
                         instance. A token this host did not mint means the loader's registry and \
                         this host disagree",
                        have = physical.get(instance_index).map_or(0, Vec::len)
                    ),
                )
            })?;
        Ok(f(instance, device))
    }

    /// Run `f` with a `VK_KHR_surface` function table, a physical device and a surface — after
    /// checking that the two handles belong to the **same instance**.
    ///
    /// That check is this host's and nobody else's. Pairing a `VkPhysicalDevice` from instance A
    /// with a `VkSurfaceKHR` from instance B is undefined behaviour the specification does not
    /// require a driver to detect, this machine has no validation layers to detect it either
    /// (`docs/research/graphics-spike.md` §6), and the guest can reach it with two perfectly valid
    /// handles it was given — so it is the one cross-family mistake a per-family registry cannot
    /// catch on its own.
    fn with_surface<R>(
        &self,
        token: HostPhysicalDevice,
        surface_token: HostSurface,
        f: impl FnOnce(&khr::surface::Instance, vk::PhysicalDevice, vk::SurfaceKHR) -> R,
    ) -> AbiResult<R> {
        let (instance_index, _) = Self::split(token);
        self.with_physical(token, |instance, device| {
            let surfaces = self.locked_surfaces();
            let entry = surfaces.get(surface_token.token()).ok_or_else(|| {
                refused(
                    "VulkanHost::with_surface",
                    &format!(
                        "{surface_token:?} is not a surface this host holds -- it holds {}, and \
                         a surface the guest has destroyed lands here too",
                        surfaces.len()
                    ),
                )
            })?;
            if entry.instance != instance_index {
                return Err(refused(
                    "VulkanHost::with_surface",
                    &format!(
                        "{surface_token:?} belongs to instance #{}, and {token:?} was enumerated \
                         from instance #{instance_index}. Every Vulkan query that takes both \
                         requires them to come from one instance, and a driver is not required to \
                         notice that they do not -- on this machine nothing would, because there \
                         are no validation layers installed. Both handles are real; the pairing is \
                         what is wrong",
                        entry.instance
                    ),
                ));
            }
            let surface_fn = khr::surface::Instance::new(&self.entry, instance);
            Ok(f(&surface_fn, device, entry.surface))
        })?
    }

    /// Run `f` with the instance and logical device a token names, and the queues it was created
    /// with.
    fn with_device<R>(
        &self,
        token: HostDevice,
        f: impl FnOnce(&ash::Instance, &DeviceEntry) -> R,
    ) -> AbiResult<R> {
        let instances = self.locked();
        let devices = self.locked_devices();
        let index = usize::try_from(token.token()).unwrap_or(usize::MAX);
        let entry = Self::live_device(&devices, index)
            .ok_or_else(|| {
                refused(
                    "VulkanHost::with_device",
                    &format!(
                        "{token:?} is not a device this host holds -- it has created {}, and a \
                         device the guest has destroyed lands here too",
                        devices.len()
                    ),
                )
            })?;
        let instance = Self::lookup(&instances, HostInstance::from_token(entry.instance as u64))?;
        Ok(f(instance, entry))
    }

    // ------------------------------------------------------------------- stage 4 internals

    /// **The lock order, extended.** `instances` → `physical` → `surfaces` → `devices` →
    /// `queues` → `swapchains` → `images` → `image_views` → `semaphores` → `fences` →
    /// `command_pools` → `command_buffers`, always, and never upward.
    ///
    /// Stage 4's methods mostly sidestep the question rather than rely on it: each one locks a
    /// table, **copies out what it needs and releases it** before locking the next. Vulkan handles
    /// are `Copy` and `ash::Device` and `ash::Instance` are `Clone`, so there is almost never a
    /// reason to hold two at once — and a driver call made while a table is locked would block
    /// every other guest thread's Vulkan for as long as the driver took.
    ///
    /// The exceptions are named where they occur, and each holds two tables in the order above
    /// for the length of a lookup and nothing else. The one that skips tables in between is
    /// `vkDestroySurfaceKHR`'s, `surfaces` across `swapchains`, still downward.
    fn locked_swapchains(&self) -> std::sync::MutexGuard<'_, Slab<SwapchainEntry>> {
        self.swapchains.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_images(&self) -> std::sync::MutexGuard<'_, Slab<ImageEntry>> {
        self.images.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_views(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::ImageView>>> {
        self.image_views.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_semaphores(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Semaphore>>> {
        self.semaphores.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_fences(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Fence>>> {
        self.fences.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_pools(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::CommandPool>>> {
        self.command_pools.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_buffers(&self) -> std::sync::MutexGuard<'_, Slab<CommandBufferEntry>> {
        self.command_buffers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Everything a stage 4 call needs about a logical device, **copied out with no lock held**.
    ///
    /// See [`GfxVulkanHost::locked_swapchains`] for why copying beats holding: every driver call
    /// below is made outside every lock this file has.
    fn device_parts(&self, token: HostDevice) -> AbiResult<DeviceParts> {
        let (index, instance_index, physical, device, requested) = {
            let devices = self.locked_devices();
            let index = usize::try_from(token.token()).unwrap_or(usize::MAX);
            let entry = Self::live_device(&devices, index).ok_or_else(|| {
                refused(
                    "VulkanHost::device_parts",
                    &format!(
                        "{token:?} is not a device this host holds -- it has created {}, and a \
                         device the guest has destroyed lands here too",
                        devices.len()
                    ),
                )
            })?;
            (index, entry.instance, entry.physical, entry.device.clone(), entry.requested.clone())
        };
        let instance = {
            let instances = self.locked();
            Self::lookup(&instances, HostInstance::from_token(instance_index as u64))?.clone()
        };
        Ok(DeviceParts { index, instance, physical, device, requested })
    }

    /// The logical device one of the four plain object families belongs to.
    fn device_of<T: Copy>(
        &self,
        table: &Slab<ObjectEntry<T>>,
        token: u64,
        family: &str,
        method: &'static str,
    ) -> AbiResult<(usize, T)> {
        table.get(token).map(|entry| (entry.device, entry.object)).ok_or_else(|| {
            refused(
                method,
                &format!(
                    "token #{token} is not a `{family}` this host created -- it holds {} of them. \
                     A token this host did not mint, or one whose object has been destroyed, \
                     means the loader's registry and this host disagree",
                    table.len()
                ),
            )
        })
    }

    /// Every object still alive on device `index`, by kind, as tokens: what `vkDestroyDevice`
    /// refuses over.
    ///
    /// Every table of driver objects this host makes **from a device**. Three families are left
    /// out on purpose, because their parent frees them and their parent is in the list: command
    /// buffers (their pool), descriptor sets (their pool), swapchain images (their swapchain).
    ///
    /// **One table at a time**: each guard is a temporary that ends with its statement. The caller
    /// holds `devices`, which precedes every one of these in the lock order, and no path here
    /// takes `devices` while holding any of them.
    fn children_of(&self, index: usize) -> Vec<(&'static str, Vec<u64>)> {
        fn owned<T>(table: &Slab<T>, index: usize, device: impl Fn(&T) -> usize) -> Vec<u64> {
            table
                .iter()
                .filter(|(_, entry)| device(entry) == index)
                .map(|(token, _)| token)
                .collect()
        }
        let mut live = Vec::new();
        let mut note = |kind: &'static str, tokens: Vec<u64>| {
            if !tokens.is_empty() {
                live.push((kind, tokens));
            }
        };
        note("VkSwapchainKHR", owned(&self.locked_swapchains(), index, |e| e.device));
        note("VkImageView", owned(&self.locked_views(), index, |e| e.device));
        note("VkSemaphore", owned(&self.locked_semaphores(), index, |e| e.device));
        note("VkFence", owned(&self.locked_fences(), index, |e| e.device));
        note("VkCommandPool", owned(&self.locked_pools(), index, |e| e.device));
        note("VkDeviceMemory", owned(&self.locked_memories(), index, |e| e.device));
        note("VkBuffer", owned(&self.locked_vk_buffers(), index, |e| e.device));
        note("VkImage", owned(&self.locked_created_images(), index, |e| e.device));
        note("VkSampler", owned(&self.locked_samplers(), index, |e| e.device));
        note("VkShaderModule", owned(&self.locked_modules(), index, |e| e.device));
        note("VkPipelineLayout", owned(&self.locked_layouts(), index, |e| e.device));
        note("VkRenderPass", owned(&self.locked_passes(), index, |e| e.device));
        note("VkFramebuffer", owned(&self.locked_framebuffers(), index, |e| e.device));
        note("VkPipeline", owned(&self.locked_pipelines(), index, |e| e.device));
        note("VkPipelineCache", owned(&self.locked_caches(), index, |e| e.device));
        note("VkQueryPool", owned(&self.locked_query_pools(), index, |e| e.device));
        note("VkDescriptorSetLayout", owned(&self.locked_set_layouts(), index, |e| e.device));
        note("VkDescriptorPool", owned(&self.locked_descriptor_pools(), index, |e| e.device));
        live
    }

    /// The `ash::Device` a device index names, cloned, with no lock held afterwards.
    fn device_at(&self, index: usize, method: &'static str) -> AbiResult<ash::Device> {
        let devices = self.locked_devices();
        Self::live_device(&devices, index).map(|entry| entry.device.clone()).ok_or_else(|| {
            refused(
                method,
                &format!(
                    "device #{index} is not one this host holds -- it has created {}, and a \
                     device the guest has destroyed lands here too",
                    devices.len()
                ),
            )
        })
    }

    /// `VK_KHR_swapchain`'s function table for one device.
    ///
    /// Built per call rather than cached, for [`GfxVulkanHost::platform_index`]'s reason: these
    /// are entered tens or hundreds of times in a frame rather than millions, and a cached table
    /// is one more thing whose lifetime has to be reasoned about against a device that the guest
    /// can destroy.
    fn swapchain_fn(instance: &ash::Instance, device: &ash::Device) -> khr::swapchain::Device {
        khr::swapchain::Device::new(instance, device)
    }

    /// The swapchain a token names, as the facts a call needs, with the lock released.
    fn swapchain_parts(&self, token: HostSwapchain) -> AbiResult<SwapchainParts> {
        let swapchains = self.locked_swapchains();
        let entry = swapchains.get(token.token()).ok_or_else(|| {
            refused(
                "VulkanHost::swapchain_parts",
                &format!(
                    "{token:?} is not a swapchain this host created -- it holds {}. A token whose \
                     swapchain has been destroyed lands here too, which is the case that matters: \
                     a retired or destroyed swapchain cannot present",
                    swapchains.len()
                ),
            )
        })?;
        Ok(SwapchainParts {
            device: entry.device,
            handle: entry.handle,
            format: entry.format,
            extent: entry.extent,
            images: entry.images.clone(),
            image_tokens: entry.image_tokens.clone(),
        })
    }

    /// **Read the pixels of a presented swapchain image back off the GPU.**
    ///
    /// # Why this exists, and why it is the only honest evidence available on this host
    ///
    /// `VERIFICATION.md` entry 11 says a test that checks `VkResult == 0` proves nothing, and
    /// `vkQueuePresentKHR` is the call that rule was written about: a present that answers
    /// `VK_SUCCESS` without presenting is indistinguishable from one that presented, for as long
    /// as nobody looks at the screen. So something has to look.
    ///
    /// The obvious thing — capture the window — **does not work here and the measurement is the
    /// graphics spike's §1**: `PrintWindow(PW_RENDERFULLCONTENT)` captures the title bar and
    /// returns solid black for the client area of a flip-model swapchain on this host.
    /// `tests/renderer_live.rs`'s header records that as the reason it makes no claim about what
    /// is on the screen.
    ///
    /// What *is* available is the image itself. After `vkQueuePresentKHR` the swapchain image
    /// holds exactly the pixels that were handed to the presentation engine, so copying it to a
    /// host-visible buffer and reading it is a direct measurement of the frame that was presented
    /// — one step short of a photograph of the monitor, and the step it is short of is the
    /// compositor's, not this runtime's. That limit is stated rather than papered over.
    ///
    /// # What it does, and the one requirement it places on the caller
    ///
    /// The device is waited idle, the image is transitioned `PRESENT_SRC_KHR` →
    /// `TRANSFER_SRC_OPTIMAL`, copied into a freshly allocated host-visible buffer, transitioned
    /// **back** to `PRESENT_SRC_KHR`, and the copy is read and converted to RGBA8. Every object it
    /// makes is destroyed before it returns.
    ///
    /// The requirement: the swapchain must have been created with
    /// `VK_IMAGE_USAGE_TRANSFER_SRC_BIT` in its `imageUsage`. That is the guest's choice, not this
    /// host's — and it is deliberately not added behind the guest's back, because a swapchain
    /// created with usage the engine did not ask for is a different swapchain from the one it
    /// asked for. A guest that omits it gets a driver error from the copy rather than a silent
    /// success.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the token is not one this host issued, when `image_index` is out
    /// of range, when the swapchain's format is not one this function can convert, or when any
    /// driver call in the sequence fails — each naming the call.
    pub fn read_presented_image(
        &self,
        swapchain: HostSwapchain,
        image_index: u32,
    ) -> AbiResult<PresentedImage> {
        const METHOD: &str = "GfxVulkanHost::read_presented_image";
        let parts = self.swapchain_parts(swapchain)?;
        let image = *parts.images.get(image_index as usize).ok_or_else(|| {
            refused(
                METHOD,
                &format!(
                    "image index {image_index} is out of range for a swapchain with {} image(s)",
                    parts.images.len()
                ),
            )
        })?;
        let channels = Channels::of(parts.format).ok_or_else(|| {
            refused(
                METHOD,
                &format!(
                    "this swapchain's format is {:?} ({}), and this read-back converts only the \
                     four-channel 8-bit formats -- `R8G8B8A8_UNORM`/`_SRGB` and \
                     `B8G8R8A8_UNORM`/`_SRGB`. A packed or wider format would need its own \
                     unpacking, and guessing at one would produce a colour that is plausible and \
                     wrong, which is the failure this whole read-back exists to detect",
                    parts.format,
                    parts.format.as_raw()
                ),
            )
        })?;

        let DeviceParts { device, physical, instance, requested, .. } =
            self.device_parts(HostDevice::from_token(parts.device as u64))?;
        let (family, _) = *requested.first().ok_or_else(|| {
            refused(
                METHOD,
                "this device was created with no queues at all, so there is no queue to submit \
                 the read-back copy on",
            )
        })?;
        // SAFETY: the device is live and was created with at least one queue in `family`, which
        // is what `requested` records.
        let queue = unsafe { device.get_device_queue(family, 0) };
        // SAFETY: `physical` is the device this logical device was created from and the instance
        // that enumerated it is still live.
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };

        let bytes = u64::from(parts.extent.width) * u64::from(parts.extent.height) * 4;
        // SAFETY: every handle below is one this function created a statement earlier and
        // destroys before it returns; `read_back` is the whole of the unsafe sequence and its own
        // documentation says what each step requires.
        let pixels = unsafe {
            read_back(&device, &memory, queue, family, image, parts.extent, bytes)
        }?;

        Ok(PresentedImage {
            width: parts.extent.width,
            height: parts.extent.height,
            format: parts.format.as_raw(),
            rgba: channels.to_rgba(&pixels),
        })
    }

    /// Which swapchain a `VkImage` belongs to, and its index in that swapchain.
    ///
    /// **The check that ties the two halves of a frame together.** `vkAcquireNextImageKHR` answers
    /// an *index*, and the guest clears a `VkImage` it looked up in the array
    /// `vkGetSwapchainImagesKHR` filled. Nothing in either call proves those are the same image —
    /// a registry that issued two handles for one image, or a guest that indexed the wrong array,
    /// would produce a run in which every `VkResult` is zero and the frame is whatever was in some
    /// other image. This is what lets a test assert it, and it is the reason
    /// [`ImageEntry`] records an index at all.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the token is not one this host issued, or names an image whose
    /// swapchain has been destroyed.
    pub fn image_location(&self, image: HostImage) -> AbiResult<(HostSwapchain, u32)> {
        let table = self.locked_images();
        table
            .get(image.token())
            .map(|entry| (HostSwapchain::from_token(entry.swapchain), entry.index))
            .ok_or_else(|| {
                refused(
                    "GfxVulkanHost::image_location",
                    &format!(
                        "{image:?} is not a swapchain image this host holds -- it holds {}",
                        table.len()
                    ),
                )
            })
    }

    /// The extent a swapchain was created with. Diagnostic (Global Constraint 6).
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the token is not one this host issued.
    pub fn swapchain_extent(&self, swapchain: HostSwapchain) -> AbiResult<(u32, u32)> {
        let parts = self.swapchain_parts(swapchain)?;
        Ok((parts.extent.width, parts.extent.height))
    }

    /// How many swapchains, images, views, semaphores, fences, pools and command buffers this host
    /// holds. Diagnostic (Global Constraint 6).
    ///
    /// **These fall as well as rise**, unlike [`GfxVulkanHost::objects`]' three: stage 4 is the
    /// first stage whose objects the guest destroys, so a number here that only grew would be a
    /// leak counter rather than an inventory.
    #[must_use]
    pub fn stage_four_objects(&self) -> StageFourObjects {
        StageFourObjects {
            swapchains: self.locked_swapchains().len(),
            images: self.locked_images().len(),
            image_views: self.locked_views().len(),
            semaphores: self.locked_semaphores().len(),
            fences: self.locked_fences().len(),
            command_pools: self.locked_pools().len(),
            command_buffers: self.locked_buffers().len(),
        }
    }
}

/// What a stage 4 call needs to know about a logical device, copied out from under the lock.
struct DeviceParts {
    index: usize,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    /// `(queueFamilyIndex, queueCount)`, as `vkCreateDevice` was asked for them.
    requested: Vec<(u32, u32)>,
}

/// The same for a swapchain.
struct SwapchainParts {
    device: usize,
    handle: vk::SwapchainKHR,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    image_tokens: Vec<u64>,
}

/// How many of each stage 4 object a [`GfxVulkanHost`] currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageFourObjects {
    /// Live `VkSwapchainKHR`s, including any that have been retired by an `oldSwapchain` and not
    /// yet destroyed.
    pub swapchains: usize,
    /// Swapchain images this host has minted a token for.
    pub images: usize,
    /// Live `VkImageView`s.
    pub image_views: usize,
    /// Live `VkSemaphore`s.
    pub semaphores: usize,
    /// Live `VkFence`s.
    pub fences: usize,
    /// Live `VkCommandPool`s.
    pub command_pools: usize,
    /// Live `VkCommandBuffer`s.
    pub command_buffers: usize,
}

/// The pixels of one presented frame, read back off the GPU.
///
/// See [`GfxVulkanHost::read_presented_image`] for what it measures and what it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedImage {
    /// The swapchain's width in pixels.
    pub width: u32,
    /// Its height.
    pub height: u32,
    /// The swapchain's `VkFormat`, as the raw `i32` it is — **kept even though the pixels have
    /// been normalised**, because "the colour came out right" and "the colour came out right for
    /// a `B8G8R8A8` swapchain" are different claims and only the second one says the channel
    /// order was handled.
    pub format: i32,
    /// The pixels, tightly packed, `width * height * 4` bytes, **always in R, G, B, A order**
    /// whatever the swapchain's own channel order was. Top row first.
    pub rgba: Vec<u8>,
}

impl PresentedImage {
    /// The pixel at `(x, y)` as `[r, g, b, a]`, or `None` if it is outside the image.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = (y as usize * self.width as usize + x as usize) * 4;
        self.rgba.get(at..at + 4).map(|p| [p[0], p[1], p[2], p[3]])
    }

    /// The pixel at the centre of the image, which is the one a clear test should read.
    ///
    /// **The centre and not the corner**, deliberately: a window's corner pixel can be covered by
    /// a rounded frame or a resize grip on Windows 11, and a clear that failed on the edges and
    /// worked in the middle is a different finding from one that failed everywhere.
    #[must_use]
    pub fn centre(&self) -> Option<[u8; 4]> {
        self.pixel(self.width / 2, self.height / 2)
    }
}

/// Which channel order a swapchain format stores its texels in.
///
/// Only the two orders a four-channel 8-bit format can have. A packed format
/// (`A2B10G10R10_UNORM_PACK32`, which this host's surface also offers) is deliberately **not**
/// handled: unpacking ten-bit channels into eight-bit ones is a conversion with a choice in it,
/// and a read-back that made that choice quietly would report a colour that is close to the one
/// asked for rather than equal to it — which is the difference this measurement exists to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channels {
    /// `R8G8B8A8_*`: the bytes are already in the order [`PresentedImage::rgba`] promises.
    Rgba,
    /// `B8G8R8A8_*`: red and blue are exchanged. The overwhelmingly common swapchain format on
    /// Windows, and the one the live test asks for.
    Bgra,
}

impl Channels {
    fn of(format: vk::Format) -> Option<Channels> {
        match format {
            vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => Some(Channels::Rgba),
            vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => Some(Channels::Bgra),
            _ => None,
        }
    }

    fn to_rgba(self, pixels: &[u8]) -> Vec<u8> {
        match self {
            Channels::Rgba => pixels.to_vec(),
            Channels::Bgra => pixels
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                .collect(),
        }
    }
}

/// The bytes of a Vulkan output structure the driver filled, with its padding initialised.
///
/// # Why the structure is zeroed before the driver is asked
///
/// `VkPhysicalDeviceProperties` has four bytes of padding before `limits`, and
/// `VkPhysicalDeviceMemoryProperties` has padding inside every `VkMemoryHeap` **and** a tail of
/// `memoryHeaps` entries past `memoryHeapCount` that no driver writes. `ash`'s own wrappers hand
/// back a `MaybeUninit::assume_init()`, so reading those bytes as `u8` would be reading
/// uninitialised memory — undefined behaviour, and in practice a few bytes of whatever was on the
/// stack travelling into a guest buffer. Zeroing first makes every byte initialised before the
/// driver is asked, so the slice below is entirely defined and the guest receives zeros where the
/// specification says nothing.
///
/// # Safety
///
/// `T` must be a `#[repr(C)]` Vulkan **output** structure: every member an integer, an enum, a
/// `VkDeviceSize` or a fixed array of those, and **no pointer** — so that all-zero is a valid
/// value of it and so that its bytes mean the same thing on the guest's aarch64 LP64 target as on
/// this host's. `fill` must be a driver entry point that writes into the pointer it is given and
/// does not retain it.
unsafe fn driver_bytes<T>(fill: impl FnOnce(*mut T)) -> Vec<u8> {
    // SAFETY: the caller's contract says `T` holds only integers and fixed arrays of them, so the
    // all-zero bit pattern is a valid value and every byte -- padding included -- is initialised.
    let mut value: T = unsafe { std::mem::zeroed() };
    fill(std::ptr::addr_of_mut!(value));
    // SAFETY: `value` is a live, fully initialised `T` that is not aliased here, and
    // `size_of::<T>()` bytes from its address are exactly its own storage.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!(value).cast::<u8>(),
            std::mem::size_of::<T>(),
        )
    };
    bytes.to_vec()
}

/// The bytes of a Vulkan structure that has **no padding**, as `ash` returned it.
///
/// Used only for the structures whose members leave no holes — `VkPhysicalDeviceFeatures` (55
/// `VkBool32`), `VkQueueFamilyProperties` (four `uint32_t` and a `VkExtent3D`),
/// `VkSurfaceCapabilitiesKHR` (thirteen `uint32_t`-sized members) and `VkSurfaceFormatKHR` (two
/// enums). Every byte of those is a member the driver wrote, so there is nothing uninitialised to
/// read and [`driver_bytes`]' zeroing dance buys nothing.
///
/// # Safety
///
/// `T` must be a `#[repr(C)]` Vulkan structure with no padding bytes and no pointer members, and
/// `value` must have been filled by a driver.
unsafe fn pod_bytes<T>(value: &T) -> Vec<u8> {
    // SAFETY: the caller's contract says `T` has no padding, so every byte of `*value` was written
    // by the driver; `size_of::<T>()` bytes from its address are its own storage.
    let bytes = unsafe {
        std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
    };
    bytes.to_vec()
}

/// A guest `pNext` chain rebuilt in this host's memory.
///
/// Each structure gets its own zeroed buffer of `FlatStructure::size()` bytes: its `sType`
/// written, the guest's member bytes copied in at 16, and its `pNext` pointing at the next buffer
/// in the guest's order. The buffers are `Vec<u64>`s, so each is 8-aligned as `pNext` requires,
/// and each is a heap block that does not move when the outer vector does -- the `pNext` values
/// stay valid for exactly as long as the `HostChain` lives, which is the whole of the call it is
/// built for. No guest address is ever written into one.
struct HostChain {
    buffers: Vec<Vec<u64>>,
}

impl HostChain {
    /// Build the chain, refusing a structure the adapter should never have admitted.
    fn new(call: &str, links: &[ChainLink]) -> AbiResult<Self> {
        let mut buffers: Vec<Vec<u64>> = Vec::with_capacity(links.len());
        for (index, link) in links.iter().enumerate() {
            let known = flat_structure(link.s_type)
                .filter(|known| known.member_bytes == link.body.len())
                .ok_or_else(|| {
                    refused(
                        call,
                        &format!(
                            "chain structure {index} has `sType` {s_type} and {got} member \
                             bytes, which is not a flat structure the adapter admits at that \
                             length. The adapter and this host disagree about a layout, and \
                             building it anyway would hand the driver a structure of the wrong \
                             size",
                            s_type = link.s_type,
                            got = link.body.len()
                        ),
                    )
                })?;
            let mut bytes = vec![0u8; known.size()];
            bytes[0..4].copy_from_slice(&link.s_type.to_le_bytes());
            bytes[16..16 + link.body.len()].copy_from_slice(&link.body);
            buffers.push(
                bytes
                    .chunks_exact(8)
                    .map(|word| u64::from_le_bytes(word.try_into().expect("eight bytes")))
                    .collect(),
            );
        }
        for index in 1..buffers.len() {
            let next = buffers[index].as_ptr() as u64;
            buffers[index - 1][1] = next;
        }
        Ok(Self { buffers })
    }

    /// The first structure, for a `pNext`; null for an empty chain.
    fn head(&mut self) -> *mut std::ffi::c_void {
        self.buffers.first_mut().map_or(std::ptr::null_mut(), |first| first.as_mut_ptr().cast())
    }

    /// Copy each structure's members, as the driver left them, back into `links`.
    fn answer_into(&self, links: &mut [ChainLink]) {
        for (buffer, link) in self.buffers.iter().zip(links) {
            let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_le_bytes()).collect();
            let length = link.body.len();
            link.body.copy_from_slice(&bytes[16..16 + length]);
        }
    }
}

/// A `VkPhysicalDeviceFeatures` from the bytes the guest wrote.
///
/// The reverse of [`pod_bytes`], and the only direction in which guest-authored bytes become a
/// host structure. It is sound for the same reason the forward direction is: the structure is 55
/// `VkBool32`s with no padding and no pointer, so **every** byte pattern is a valid value of it
/// and there is nothing a hostile guest could put here that is not simply a feature flag set to a
/// number other than 0 or 1 — which is the driver's to reject, and which is what a native Android
/// application could equally have written.
fn features_from_bytes(bytes: &[u8]) -> AbiResult<vk::PhysicalDeviceFeatures> {
    if bytes.len() != std::mem::size_of::<vk::PhysicalDeviceFeatures>() {
        return Err(refused(
            "vkCreateDevice",
            &format!(
                "the guest's `pEnabledFeatures` arrived as {} bytes and \
                 `sizeof(VkPhysicalDeviceFeatures)` is {} here. `omni_android::vulkan` and this \
                 crate disagree about a structure the specification fixes",
                bytes.len(),
                std::mem::size_of::<vk::PhysicalDeviceFeatures>()
            ),
        ));
    }
    let mut features = vk::PhysicalDeviceFeatures::default();
    // SAFETY: the lengths are equal, checked one statement ago; `VkPhysicalDeviceFeatures` is 55
    // `VkBool32`s with no padding and no pointer, so any byte pattern is a valid value of it and
    // this is a field-for-field assignment written as one `memcpy`. The source is a `&[u8]` and
    // the destination is a distinct local, so they cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            std::ptr::addr_of_mut!(features).cast::<u8>(),
            bytes.len(),
        );
    }
    Ok(features)
}

/// The name [`crate::claim`] records the guest's swapchain under.
///
/// A `&'static str` the refusal quotes, so that an embedding reading "the guest's
/// vkCreateSwapchainKHR already owns this window" knows which of its two Vulkan stacks to change.
const GUEST_SWAPCHAIN_OWNER: &str = "the guest's vkCreateSwapchainKHR";

impl VulkanHost for GfxVulkanHost {
    /// The first of [`PLATFORM_SURFACE_EXTENSIONS`] the driver reports.
    ///
    /// Asked of the driver on every call rather than cached, and that is affordable for the reason
    /// the loader's census is ungated: this is entered tens of times in a process, not millions.
    /// Caching it would also make the answer a fact about the first call rather than about the
    /// loader, and a loader whose layers change under a running process is a real thing on Windows
    /// (an overlay being installed) even if nothing here has seen it happen.
    fn platform_surface_extension(&self) -> AbiResult<String> {
        let index = self.platform_index()?;
        Ok(PLATFORM_SURFACE_EXTENSIONS[index].to_string())
    }

    /// The entry point that goes with whatever [`GfxVulkanHost::platform_surface_extension`]
    /// chose, **by construction**: both run [`GfxVulkanHost::platform_index`] and index the same
    /// position of two arrays a test checks entry by entry.
    fn platform_surface_entry_point(&self) -> AbiResult<String> {
        let index = self.platform_index()?;
        Ok(PLATFORM_SURFACE_ENTRY_POINTS[index].to_string())
    }

    fn instance_extensions(
        &self,
        layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        let layer = match layer {
            None => None,
            Some(name) => Some(CString::new(name).map_err(|err| {
                refused(
                    "vkEnumerateInstanceExtensionProperties",
                    &format!(
                        "the guest's `pLayerName` \"{name}\" has an interior NUL ({err}), so it \
                         cannot be handed to the driver as a C string. The driver would see a \
                         shorter name than the guest wrote and would answer about a different \
                         layer"
                    ),
                )
            })?),
        };
        match self.enumerate(layer.as_deref()) {
            Ok(extensions) => Ok(DriverAnswer::Ok(extensions)),
            // The driver's code, carried as itself. `VK_ERROR_LAYER_NOT_PRESENT` for a layer this
            // host does not have is a real answer the engine has a branch for.
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn create_instance(&self, request: &InstanceRequest) -> AbiResult<DriverAnswer<HostInstance>> {
        let mut instances = self.locked();
        if instances.iter().flatten().count() >= MAX_INSTANCES {
            return Err(refused(
                "vkCreateInstance",
                &format!(
                    "this host already holds {MAX_INSTANCES} live instances, which is \
                     `omni_gfx::host::MAX_INSTANCES`. The guest's registry holds fewer, so an \
                     instance reaching here past that bound was created some other way than \
                     through this loader, or `vkDestroyInstance` was never called for the ones \
                     before it"
                ),
            ));
        }

        // Owned C strings for everything the driver will read, and **they outlive the call by
        // construction**: `ash`'s builders take pointers, so the `CString`s have to stay alive
        // until `create_instance` returns and these locals do. Nothing here points into guest
        // memory -- `InstanceRequest` is owned `String`s by the time it arrives, which is what
        // makes the driver's reads safe without any statement about what guest threads are doing.
        let layers = c_strings("ppEnabledLayerNames", &request.layers)?;
        let extensions = c_strings("ppEnabledExtensionNames", &request.extensions)?;
        let layer_pointers: Vec<*const std::ffi::c_char> =
            layers.iter().map(|name| name.as_ptr()).collect();

        let application_name = optional_c_string("pApplicationName", request, |a| &a.application_name)?;
        let engine_name = optional_c_string("pEngineName", request, |a| &a.engine_name)?;
        let application_info = request.application.as_ref().map(|application| {
            let mut info = vk::ApplicationInfo::default()
                .application_version(application.application_version)
                .engine_version(application.engine_version)
                .api_version(application.api_version);
            if let Some(name) = application_name.as_deref() {
                info = info.application_name(name);
            }
            if let Some(name) = engine_name.as_deref() {
                info = info.engine_name(name);
            }
            info
        });

        // **Created as the guest asked, first.** Only when the loader answers
        // `VK_ERROR_INCOMPATIBLE_DRIVER` -- its answer when every driver it found is a portability
        // implementation (MoltenVK) and the request did not opt in -- and offers
        // `VK_KHR_portability_enumeration` is it asked again with that extension and flag added
        // (see `crate::portability`). A request a native driver accepts is never altered.
        let api_version = request.application.as_ref().map_or(vk::API_VERSION_1_0, |a| a.api_version);
        let mut names: Vec<&std::ffi::CStr> = extensions.iter().map(CString::as_c_str).collect();
        let mut flags = vk::InstanceCreateFlags::from_raw(request.flags);
        let mut retried = false;
        let created = loop {
            let extension_pointers: Vec<*const std::ffi::c_char> =
                names.iter().map(|name| name.as_ptr()).collect();
            let mut info = vk::InstanceCreateInfo::default()
                .flags(flags)
                .enabled_layer_names(&layer_pointers)
                .enabled_extension_names(&extension_pointers);
            if let Some(application_info) = application_info.as_ref() {
                info = info.application_info(application_info);
            }
            // SAFETY: every pointer reachable from `info` is into a local `CString`, a `'static`
            // name or a local `Vec` that outlives this call, `pNext` is null because
            // `InstanceRequest` cannot carry a chain (the shim refuses one by name), and
            // `pAllocator` is `None` because a guest allocator is refused by name one layer up.
            // Nothing here is a guest address.
            match unsafe { self.entry.create_instance(&info, None) } {
                Err(result) if !retried => {
                    let offered = self.enumerate(None).unwrap_or_default();
                    let offered: Vec<CString> =
                        offered.into_iter().filter_map(|e| CString::new(e.name).ok()).collect();
                    let offered: Vec<&std::ffi::CStr> = offered.iter().map(CString::as_c_str).collect();
                    if !crate::portability::retry_with_portability(result, &offered, &names) {
                        break Err(result);
                    }
                    names.extend(crate::portability::portability_additions(&offered, &names, api_version));
                    flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
                    retried = true;
                }
                other => break other,
            }
        };
        match created {
            Ok(instance) => {
                let token = instances.len() as u64;
                instances.push(Some(instance));
                self.features2
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(crate::portability::Features2::of(api_version, &names));
                Ok(DriverAnswer::Ok(HostInstance::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn has_instance_proc(&self, instance: HostInstance, name: &str) -> AbiResult<bool> {
        let instances = self.locked();
        let handle = Self::lookup(&instances, instance)?.handle();
        let name = CString::new(name).map_err(|err| {
            refused(
                "vkGetInstanceProcAddr",
                &format!(
                    "the guest's `pName` \"{name}\" has an interior NUL ({err}), so the driver \
                     would be asked about a shorter name than the guest wrote"
                ),
            )
        })?;
        // SAFETY: `handle` is a live `VkInstance` this host created, and `name` is a C string that
        // outlives the call. **The pointer the driver returns is dropped here and goes no
        // further** -- `VulkanHost::has_instance_proc` answers a `bool`, so a host code address
        // cannot reach a guest thunk even by mistake. That is the invariant the trait is shaped
        // around, and this is the one place in the workspace where the pointer exists at all.
        let found = unsafe { self.entry.get_instance_proc_addr(handle, name.as_ptr()) };
        Ok(found.is_some())
    }

    /// `vkDestroyInstance`, after the check the specification requires: **no `VkDevice` and no
    /// `VkSurfaceKHR` made from the instance may still be alive**.
    ///
    /// [`GfxVulkanHost::destroy_device`]'s argument, one level up: no validation layers here, a
    /// driver measured failing silently when misused, so live children are refused **naming each
    /// kind with a count and a few tokens**, and the instance is left exactly as it was. This host
    /// makes no debug messenger -- the shim refuses a `pNext` chain on `vkCreateInstance`, and
    /// `vkCreateDebugUtilsMessengerEXT` is not forwarded -- so there is no third kind to check.
    ///
    /// # Held across the check and the removal
    ///
    /// `instances` is held from the check until the slot is emptied. Creating a device and
    /// creating a surface both look the instance up in that table first, so neither can happen in
    /// between. `surfaces`, `devices` and `physical` follow `instances` in the lock order and are
    /// each taken on their own. The driver is called with no lock held.
    ///
    /// # What goes with it
    ///
    /// Its physical devices: their tokens are the answer, and the enumeration cached for the
    /// instance is emptied. Also the import probes cached for those physical devices, which are
    /// keyed by the driver's raw handle, a value a later instance's physical device may reuse.
    fn destroy_instance(&self, instance: HostInstance) -> AbiResult<Vec<HostPhysicalDevice>> {
        const CALL: &str = "vkDestroyInstance";
        let index = usize::try_from(instance.token()).unwrap_or(usize::MAX);
        let (doomed, gone) = {
            let mut instances = self.locked();
            if instances.get(index).and_then(Option::as_ref).is_none() {
                return Err(refused(
                    CALL,
                    &format!(
                        "{instance:?} is not an instance this host holds -- it has created {}. An \
                         instance the guest has already destroyed lands here too, and a second \
                         `vkDestroyInstance` of one instance is a double free the driver is not \
                         required to notice",
                        instances.len()
                    ),
                ));
            }
            let mut live = Vec::new();
            let surfaces: Vec<u64> = self
                .locked_surfaces()
                .iter()
                .filter(|(_, surface)| surface.instance == index)
                .map(|(token, _)| token)
                .collect();
            if !surfaces.is_empty() {
                live.push(("VkSurfaceKHR", surfaces));
            }
            let devices: Vec<u64> = self
                .locked_devices()
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.as_ref().is_some_and(|device| device.instance == index))
                .map(|(token, _)| token as u64)
                .collect();
            if !devices.is_empty() {
                live.push(("VkDevice", devices));
            }
            if !live.is_empty() {
                return Err(refused(
                    CALL,
                    &format!(
                        "{instance:?} still has objects created from it: {}. The specification \
                         requires every device and surface made from an instance to be destroyed \
                         before it is, and there is no validation layer on this machine to report \
                         the violation -- its NVIDIA driver has been measured failing silently or \
                         corrupting the heap when misused instead. So the instance is left \
                         exactly as it was",
                        describe_children(&live)
                    ),
                ));
            }
            let Some(doomed) = instances.get_mut(index).and_then(Option::take) else {
                // Checked live above, under this same lock.
                return Err(refused(CALL, &format!("{instance:?} vanished under its own lock")));
            };
            let enumerated =
                self.locked_physical().get_mut(index).map(std::mem::take).unwrap_or_default();
            let raw: Vec<u64> =
                enumerated.iter().map(|device| vk::Handle::as_raw(*device)).collect();
            self.importable
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|probe| !raw.contains(&probe.physical));
            // The tokens `physical_devices` minted for this instance: `(instance << 32) | index`.
            let gone: Vec<HostPhysicalDevice> = (0..enumerated.len() as u64)
                .map(|device| HostPhysicalDevice::from_token(((index as u64) << 32) | device))
                .collect();
            (doomed, gone)
        };
        // SAFETY: the instance is live and this host created it; every device and surface made
        // from it is gone -- checked above, under the lock that then emptied its slot, so no new
        // one can have been made through this host -- and `pAllocator` was `None` at creation.
        unsafe { doomed.destroy_instance(None) };
        Ok(gone)
    }

    // ------------------------------------------------------------------------ stage 3

    /// `vkCreateAndroidSurfaceKHR`, satisfied by this host's own window system.
    ///
    /// The `match` is over `RawWindow`, which is `#[non_exhaustive]`: a Wayland or AppKit variant
    /// added to the seam later lands in the wildcard arm and refuses **naming itself**, rather
    /// than turning this into a `match` that silently stopped being exhaustive.
    /// [`crate::vulkan`]'s own `create_surface` has the same shape for the same reason.
    fn create_platform_surface(
        &self,
        instance: HostInstance,
        window: RawWindow,
    ) -> AbiResult<DriverAnswer<SurfaceCreated>> {
        let instances = self.locked();
        let index = usize::try_from(instance.token()).unwrap_or(usize::MAX);
        let handle = Self::lookup(&instances, instance)?;
        match window {
            RawWindow::Win32 { hwnd, hinstance } => {
                let key = crate::claim::WindowKey::win32(hwnd);
                let info =
                    vk::Win32SurfaceCreateInfoKHR::default().hinstance(hinstance).hwnd(hwnd);
                let win32 = khr::win32_surface::Instance::new(&self.entry, handle);
                // SAFETY: `hwnd` and `hinstance` came from a live `omni_platform::window::Window`
                // that `ndk::WindowSource::raw_window` published, `info` borrows nothing that does
                // not outlive this call, and `pAllocator` is `None` because a guest allocator is
                // refused by name one layer up.
                match unsafe { win32.create_win32_surface(&info, None) } {
                    Ok(surface) => {
                        let token = self.locked_surfaces().insert(SurfaceEntry {
                            instance: index,
                            surface,
                            window: key,
                        });
                        Ok(DriverAnswer::Ok(SurfaceCreated {
                            surface: HostSurface::from_token(token),
                            // **The name the rewrite log records**, taken from the pairing table
                            // rather than written out here, so that the call this reports and the
                            // call `platform_surface_entry_point` promised are the same string by
                            // construction. This is the only crate allowed to know which platform
                            // it is on.
                            host_call: PLATFORM_SURFACE_ENTRY_POINTS[0].to_string(),
                        }))
                    }
                    Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
                }
            }
            RawWindow::AppKit { ns_view, ca_metal_layer, .. } => {
                let key = crate::claim::WindowKey::appkit(ns_view);
                let info = vk::MetalSurfaceCreateInfoEXT::default()
                    .layer(ca_metal_layer as *const vk::CAMetalLayer);
                let metal = ash::ext::metal_surface::Instance::new(&self.entry, handle);
                // SAFETY: `ca_metal_layer` is the `CAMetalLayer` of a live
                // `omni_platform::window::Window` that `ndk::WindowSource::raw_window` published,
                // `info` borrows nothing that does not outlive this call, and `pAllocator` is
                // `None` because a guest allocator is refused by name one layer up.
                match unsafe { metal.create_metal_surface(&info, None) } {
                    Ok(surface) => {
                        let token = self.locked_surfaces().insert(SurfaceEntry {
                            instance: index,
                            surface,
                            window: key,
                        });
                        Ok(DriverAnswer::Ok(SurfaceCreated {
                            surface: HostSurface::from_token(token),
                            // The pairing table's entry for `VK_EXT_metal_surface`, as the Win32
                            // arm takes its own: the same string by construction.
                            host_call: PLATFORM_SURFACE_ENTRY_POINTS[4].to_string(),
                        }))
                    }
                    Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
                }
            }
            other => Err(refused(
                "vkCreateAndroidSurfaceKHR",
                &format!(
                    "the guest's `ANativeWindow *` resolved to a {system} window, and this host \
                     has no Vulkan surface call for that window system -- only \
                     `VK_KHR_win32_surface` and `VK_EXT_metal_surface`. This is a refusal rather than a `VkResult` because \
                     the specification has no code for \"this build of the host cannot make a \
                     surface here\", and `VK_ERROR_INITIALIZATION_FAILED` would send the engine \
                     looking at its driver. `crate::vulkan::create_surface` is the other place \
                     that grows when `RawWindow` gains a variant",
                    system = other.system_name()
                ),
            )),
        }
    }

    /// `vkDestroySurfaceKHR`: the end of a surface [`GfxVulkanHost::create_platform_surface`]
    /// made, after the two checks the specification requires and nothing on this machine would
    /// otherwise make.
    ///
    /// # Why this host checks rather than trusting the driver to
    ///
    /// Destroying a surface a swapchain was created over is undefined behaviour, and so is
    /// destroying it through an instance it did not come from. There are no validation layers here
    /// (`docs/research/graphics-spike.md` §6), and the spike measured what the NVIDIA driver does
    /// with swapchain misuse it is not required to notice: it crashed, with no diagnostic anywhere.
    /// So both are refused **naming both objects**, before the driver is asked.
    ///
    /// **A retired swapchain counts.** `oldSwapchain` retires the outgoing swapchain without
    /// destroying it; it no longer owns the window, but it was created over this surface and the
    /// guest still owes its `vkDestroySwapchainKHR`.
    ///
    /// # Lock order
    ///
    /// The instance is cloned out first and released, because `instances` precedes `surfaces`.
    /// `surfaces` is then held across `swapchains` — downward in the order
    /// [`GfxVulkanHost::locked_swapchains`] states, and one of its named exceptions to "copy out
    /// and release" — so that the check and the removal are one step: no swapchain can be recorded
    /// over this surface between them. The driver is called with neither held.
    fn destroy_surface(&self, instance: HostInstance, surface: HostSurface) -> AbiResult<()> {
        const CALL: &str = "vkDestroySurfaceKHR";
        let handle = {
            let instances = self.locked();
            Self::lookup(&instances, instance)?.clone()
        };
        let doomed = {
            let mut surfaces = self.locked_surfaces();
            let Some(entry) = surfaces.get(surface.token()) else {
                return Err(refused(
                    CALL,
                    &format!(
                        "{surface:?} is not a surface this host holds -- it holds {}. A surface \
                         the guest has already destroyed lands here too, and a second \
                         `vkDestroySurfaceKHR` of one surface is a double free the driver is not \
                         required to notice",
                        surfaces.len()
                    ),
                ));
            };
            let created = HostInstance::from_token(entry.instance as u64);
            let doomed = entry.surface;
            if created != instance {
                return Err(refused(
                    CALL,
                    &format!(
                        "{surface:?} was created from {created:?}, and the guest destroyed it \
                         through {instance:?}. The specification requires a surface to be \
                         destroyed through the instance it was created from -- the call is \
                         dispatched through that instance's `VK_KHR_surface` table -- and a driver \
                         is not required to notice that it was not; on this machine nothing \
                         would, because there are no validation layers installed. Both handles \
                         are real; the pairing is what is wrong"
                    ),
                ));
            }
            let swapchains = self.locked_swapchains();
            let over: Vec<HostSwapchain> = swapchains
                .iter()
                .filter(|(_, entry)| entry.surface == surface.token())
                .map(|(token, _)| HostSwapchain::from_token(token))
                .collect();
            if !over.is_empty() {
                return Err(refused(
                    CALL,
                    &format!(
                        "{surface:?} still has {count} swapchain(s) created over it: {over:?}. \
                         The specification requires every `VkSwapchainKHR` created for a surface \
                         to be destroyed before the surface is -- a retired one included, because \
                         `oldSwapchain` retires a swapchain without destroying it and the guest \
                         still owes `vkDestroySwapchainKHR` on it. Destroying the surface anyway \
                         is undefined behaviour with no validation layer on this machine to \
                         report it, so the surface is left exactly as it was",
                        count = over.len()
                    ),
                ));
            }
            drop(swapchains);
            // The entry goes while the lock that checked it is still held, and its slot's
            // generation moves on: the guest's stale token is a refusal from here, not a name for
            // whichever surface is created in this slot next.
            surfaces.remove(surface.token());
            doomed
        };
        let surface_fn = khr::surface::Instance::new(&self.entry, &handle);
        // SAFETY: `doomed` is a live surface created from `handle` -- both checked above, under
        // the lock that then removed its entry, so nothing else can reach it through this host --
        // no swapchain created over it survives, and `pAllocator` was `None` at creation.
        unsafe { surface_fn.destroy_surface(doomed, None) };
        Ok(())
    }

    fn physical_devices(
        &self,
        instance: HostInstance,
    ) -> AbiResult<DriverAnswer<Vec<HostPhysicalDevice>>> {
        let index = usize::try_from(instance.token()).unwrap_or(usize::MAX);
        let instances = self.locked();
        let handle = Self::lookup(&instances, instance)?;
        let mut physical = self.locked_physical();
        if physical.len() <= index {
            physical.resize_with(index + 1, Vec::new);
        }
        if physical[index].is_empty() {
            // SAFETY: `handle` is a live instance this host created; the enumeration takes no
            // other handle and writes into an `ash`-owned vector.
            match unsafe { handle.enumerate_physical_devices() } {
                Ok(devices) => physical[index] = devices,
                Err(result) => return Ok(DriverAnswer::Failed(result.as_raw())),
            }
        }
        Ok(DriverAnswer::Ok(
            (0..physical[index].len())
                .map(|device| {
                    HostPhysicalDevice::from_token(((index as u64) << 32) | device as u64)
                })
                .collect(),
        ))
    }

    fn physical_device_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        self.with_physical(device, |instance, physical| {
            // The **raw** entry point rather than `ash`'s wrapper, because the wrapper returns a
            // `MaybeUninit::assume_init()` and this structure has padding. `driver_bytes` carries
            // the argument.
            let fp = instance.fp_v1_0().get_physical_device_properties;
            // SAFETY: `VkPhysicalDeviceProperties` holds only integers, enums and fixed arrays of
            // them; `physical` is a live device of `instance`; the driver writes the structure and
            // does not keep the pointer.
            unsafe { driver_bytes::<vk::PhysicalDeviceProperties>(|out| fp(physical, out)) }
        })
    }

    fn physical_device_features(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`.
            let features = unsafe { instance.get_physical_device_features(physical) };
            // SAFETY: `VkPhysicalDeviceFeatures` is 55 `VkBool32`s with no padding, all written.
            unsafe { pod_bytes(&features) }
        })
    }

    fn physical_device_format_properties(
        &self,
        device: HostPhysicalDevice,
        format: i32,
    ) -> AbiResult<Vec<u8>> {
        self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`; any `VkFormat` value is a valid
            // argument, and one the driver does not know is answered with no features.
            let properties = unsafe {
                instance.get_physical_device_format_properties(physical, vk::Format::from_raw(format))
            };
            // SAFETY: `VkFormatProperties` is three `VkFormatFeatureFlags` with no padding, all
            // written.
            unsafe { pod_bytes(&properties) }
        })
    }

    fn physical_device_image_format_properties(
        &self,
        device: HostPhysicalDevice,
        query: ImageFormatQuery,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`; every combination of the five
            // scalars is a valid question, and an unsupported one is the driver's
            // `VK_ERROR_FORMAT_NOT_SUPPORTED`.
            let answer = unsafe {
                instance.get_physical_device_image_format_properties(
                    physical,
                    vk::Format::from_raw(query.format),
                    vk::ImageType::from_raw(query.image_type),
                    vk::ImageTiling::from_raw(query.tiling),
                    vk::ImageUsageFlags::from_raw(query.usage),
                    vk::ImageCreateFlags::from_raw(query.flags),
                )
            };
            match answer {
                // SAFETY: `VkImageFormatProperties` has no padding (`maxResourceSize` is at 24)
                // and the driver wrote every member.
                Ok(properties) => DriverAnswer::Ok(unsafe { pod_bytes(&properties) }),
                Err(result) => DriverAnswer::Failed(result.as_raw()),
            }
        })
    }

    fn physical_device_image_format_properties2(
        &self,
        device: HostPhysicalDevice,
        entry: &str,
        query: ImageFormatQuery,
        question: &[ChainLink],
        answers: &mut [ChainLink],
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let name = CString::new(entry).map_err(|err| {
            refused(entry, &format!("the entry point's name has an interior NUL ({err})"))
        })?;
        let mut asked = HostChain::new(entry, question)?;
        let mut links = HostChain::new(entry, answers)?;
        let answer = self.with_physical(device, |instance, physical| {
            // SAFETY: `instance` is a live `VkInstance` this host created and `name` is a C string
            // that outlives the call.
            let found = unsafe { self.entry.get_instance_proc_addr(instance.handle(), name.as_ptr()) };
            let Some(found) = found else {
                return Err(refused(
                    entry,
                    "the driver answers NULL for it on this instance -- the `KHR` spelling needs \
                     `VK_KHR_get_physical_device_properties2` enabled, the core one a 1.1 \
                     instance -- so there is nothing to forward the guest's call to",
                ));
            };
            // SAFETY: both spellings share `PFN_vkGetPhysicalDeviceImageFormatProperties2`'s
            // signature, and `found` is the driver's pointer for exactly the name asked.
            let fp: vk::PFN_vkGetPhysicalDeviceImageFormatProperties2 =
                unsafe { std::mem::transmute(found) };
            let mut info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(vk::Format::from_raw(query.format))
                .ty(vk::ImageType::from_raw(query.image_type))
                .tiling(vk::ImageTiling::from_raw(query.tiling))
                .usage(vk::ImageUsageFlags::from_raw(query.usage))
                .flags(vk::ImageCreateFlags::from_raw(query.flags));
            info.p_next = asked.head().cast_const();
            let mut properties =
                vk::ImageFormatProperties2 { p_next: links.head(), ..Default::default() };
            // SAFETY: `physical` is a live device of `instance`; `info`, `properties` and every
            // buffer of both chains outlive the call; each chained structure is flat and sized
            // from `ash`'s own definition. The driver writes `properties` and the answer chain's
            // members, reads the question chain, and keeps no pointer.
            let result = unsafe { fp(physical, &info, &mut properties) };
            if result != vk::Result::SUCCESS {
                return Ok(DriverAnswer::Failed(result.as_raw()));
            }
            // SAFETY: `VkImageFormatProperties` has no padding and the driver wrote every member.
            Ok(DriverAnswer::Ok(unsafe { pod_bytes(&properties.image_format_properties) }))
        })??;
        if matches!(answer, DriverAnswer::Ok(_)) {
            links.answer_into(answers);
        }
        Ok(answer)
    }

    fn physical_device_features2(
        &self,
        device: HostPhysicalDevice,
        entry: &str,
        chain: &mut [ChainLink],
    ) -> AbiResult<Vec<u8>> {
        let name = CString::new(entry).map_err(|err| {
            refused(entry, &format!("the entry point's name has an interior NUL ({err})"))
        })?;
        let mut links = HostChain::new(entry, chain)?;
        let features = self.with_physical(device, |instance, physical| {
            // SAFETY: `instance` is a live `VkInstance` this host created and `name` is a C string
            // that outlives the call.
            let found = unsafe { self.entry.get_instance_proc_addr(instance.handle(), name.as_ptr()) };
            let Some(found) = found else {
                return Err(refused(
                    entry,
                    "the driver answers NULL for it on this instance -- the `KHR` spelling needs \
                     `VK_KHR_get_physical_device_properties2` enabled, the core one a 1.1 \
                     instance -- so there is nothing to forward the guest's call to",
                ));
            };
            // SAFETY: `vkGetPhysicalDeviceFeatures2` and `vkGetPhysicalDeviceFeatures2KHR` share
            // `PFN_vkGetPhysicalDeviceFeatures2`'s signature, and `found` is the driver's pointer
            // for exactly the name asked.
            let fp: vk::PFN_vkGetPhysicalDeviceFeatures2 = unsafe { std::mem::transmute(found) };
            let mut features2 =
                vk::PhysicalDeviceFeatures2 { p_next: links.head(), ..Default::default() };
            // SAFETY: `physical` is a live device of `instance`; `features2` and every buffer of
            // `links` outlive the call; each chained structure is one the physical device's
            // extension list named (the guest chains it only then) and is sized from `ash`'s own
            // definition (asserted in this file's tests). The driver writes the members and keeps
            // no pointer.
            unsafe { fp(physical, &mut features2) };
            // SAFETY: `VkPhysicalDeviceFeatures` is 55 `VkBool32`s with no padding, all written.
            Ok(unsafe { pod_bytes(&features2.features) })
        })??;
        links.answer_into(chain);
        Ok(features)
    }

    fn queue_family_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<Vec<u8>>> {
        self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`.
            let families =
                unsafe { instance.get_physical_device_queue_family_properties(physical) };
            families
                .iter()
                // SAFETY: `VkQueueFamilyProperties` has no padding and the driver filled every
                // entry of the vector it sized.
                .map(|family| unsafe { pod_bytes(family) })
                .collect()
        })
    }

    fn physical_device_memory_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        self.with_physical(device, |instance, physical| {
            // The raw entry point, for `physical_device_properties`' reason and more so: the
            // `memoryHeaps` entries past `memoryHeapCount` are never written by any driver.
            let fp = instance.fp_v1_0().get_physical_device_memory_properties;
            // SAFETY: `VkPhysicalDeviceMemoryProperties` holds only integers and fixed arrays of
            // them; `physical` is a live device of `instance`.
            unsafe { driver_bytes::<vk::PhysicalDeviceMemoryProperties>(|out| fp(physical, out)) }
        })
    }

    fn surface_support(
        &self,
        device: HostPhysicalDevice,
        queue_family: u32,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<bool>> {
        self.with_surface(device, surface, |surface_fn, physical, surface| {
            // SAFETY: the physical device and the surface are both live and belong to the same
            // instance, which `with_surface` checked.
            match unsafe {
                surface_fn.get_physical_device_surface_support(physical, queue_family, surface)
            } {
                Ok(supported) => DriverAnswer::Ok(supported),
                Err(result) => DriverAnswer::Failed(result.as_raw()),
            }
        })
    }

    fn surface_capabilities(
        &self,
        device: HostPhysicalDevice,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        self.with_surface(device, surface, |surface_fn, physical, surface| {
            // SAFETY: as `surface_support`.
            match unsafe { surface_fn.get_physical_device_surface_capabilities(physical, surface) }
            {
                // SAFETY: `VkSurfaceCapabilitiesKHR` is thirteen `uint32_t`-sized members with no
                // padding, all written by the driver.
                Ok(capabilities) => DriverAnswer::Ok(unsafe { pod_bytes(&capabilities) }),
                Err(result) => DriverAnswer::Failed(result.as_raw()),
            }
        })
    }

    fn surface_formats(
        &self,
        device: HostPhysicalDevice,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<Vec<u8>>>> {
        self.with_surface(device, surface, |surface_fn, physical, surface| {
            // SAFETY: as `surface_support`.
            match unsafe { surface_fn.get_physical_device_surface_formats(physical, surface) } {
                Ok(formats) => DriverAnswer::Ok(
                    formats
                        .iter()
                        // SAFETY: `VkSurfaceFormatKHR` is two enums with no padding.
                        .map(|format| unsafe { pod_bytes(format) })
                        .collect(),
                ),
                Err(result) => DriverAnswer::Failed(result.as_raw()),
            }
        })
    }

    fn surface_present_modes(
        &self,
        device: HostPhysicalDevice,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u32>>> {
        self.with_surface(device, surface, |surface_fn, physical, surface| {
            // SAFETY: as `surface_support`.
            match unsafe { surface_fn.get_physical_device_surface_present_modes(physical, surface) }
            {
                Ok(modes) => {
                    DriverAnswer::Ok(modes.iter().map(|mode| mode.as_raw() as u32).collect())
                }
                Err(result) => DriverAnswer::Failed(result.as_raw()),
            }
        })
    }

    fn device_extensions(
        &self,
        device: HostPhysicalDevice,
        layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        let layer = match layer {
            None => None,
            Some(name) => Some(CString::new(name).map_err(|err| {
                refused(
                    "vkEnumerateDeviceExtensionProperties",
                    &format!(
                        "the guest's `pLayerName` \"{name}\" has an interior NUL ({err}), so the \
                         driver would be asked about a different layer from the one the guest \
                         named"
                    ),
                )
            })?),
        };
        self.with_physical(device, |instance, physical| {
            // `ash` 0.38's `enumerate_device_extension_properties` takes no layer, so the layered
            // form goes through the raw entry point. A named layer is what the guest may ask for
            // and answering the implicit set for it would be answering a different question.
            let name = layer.as_deref().map_or(std::ptr::null(), CStr::as_ptr);
            let fp = instance.fp_v1_0().enumerate_device_extension_properties;
            let mut count = 0u32;
            // SAFETY: `physical` is a live device of `instance`; the count-only form writes one
            // `uint32_t` and reads `name` as a C string that outlives the call.
            let result = unsafe { fp(physical, name, &mut count, std::ptr::null_mut()) };
            if result != vk::Result::SUCCESS {
                return DriverAnswer::Failed(result.as_raw());
            }
            let mut properties = vec![vk::ExtensionProperties::default(); count as usize];
            // SAFETY: `properties` has room for exactly `count` entries, which is what the call
            // above reported.
            let result =
                unsafe { fp(physical, name, &mut count, properties.as_mut_ptr()) };
            if result != vk::Result::SUCCESS && result != vk::Result::INCOMPLETE {
                return DriverAnswer::Failed(result.as_raw());
            }
            properties.truncate(count as usize);
            DriverAnswer::Ok(
                properties
                    .iter()
                    .filter_map(|property| {
                        property.extension_name_as_c_str().ok().map(|name| HostExtension {
                            name: name.to_string_lossy().into_owned(),
                            spec_version: property.spec_version,
                        })
                    })
                    .collect(),
            )
        })
    }

    fn create_device(
        &self,
        device: HostPhysicalDevice,
        request: &DeviceRequest,
    ) -> AbiResult<DriverAnswer<HostDevice>> {
        let (instance_index, _) = Self::split(device);
        // Owned host-side for the duration of the call, exactly as `create_instance` does it:
        // `ash`'s builders take pointers and nothing here may point into guest memory.
        let layers = c_strings("ppEnabledLayerNames", &request.layers)?;
        let extensions = c_strings("ppEnabledExtensionNames", &request.extensions)?;
        let layer_pointers: Vec<*const std::ffi::c_char> =
            layers.iter().map(|name| name.as_ptr()).collect();
        let mut extension_pointers: Vec<*const std::ffi::c_char> =
            extensions.iter().map(|name| name.as_ptr()).collect();

        // **A portability implementation's subset, enabled as the specification requires** --
        // a device that exposes `VK_KHR_portability_subset` must have it enabled -- with exactly
        // the subset features it reports, in front of the guest's own `pNext` chain. The guest
        // does not know the extension exists (Android drivers are native), so nothing it asked
        // for is changed; on a native driver nothing is added at all. See `crate::portability`.
        let features2 = self
            .features2
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(instance_index)
            .copied()
            .unwrap_or(crate::portability::Features2::None);
        let subset = self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`.
            let offered = unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
            // SAFETY: live handles; `features2` is what this instance was created able to do.
            unsafe { crate::portability::Subset::of(&self.entry, instance, physical, &offered, features2) }
        })?;
        let already = extensions.iter().any(|name| name.as_c_str() == crate::portability::SUBSET);
        let mut subset_features = subset.filter(|_| !already).map(|subset| subset.features);
        if subset_features.is_some() {
            extension_pointers.push(crate::portability::SUBSET.as_ptr());
        }
        let features = request.features.as_deref().map(features_from_bytes).transpose()?;

        // The priorities have to outlive the `VkDeviceQueueCreateInfo`s that point at them, so
        // they are collected into one owned `Vec<Vec<f32>>` first and borrowed from afterwards.
        // Building both in one `map` would drop each `Vec<f32>` at the end of its iteration and
        // leave `pQueuePriorities` dangling -- the defect this arrangement exists to make
        // impossible rather than to remember.
        let priorities: Vec<Vec<f32>> =
            request.queues.iter().map(|queue| queue.priorities.clone()).collect();
        let queue_infos: Vec<vk::DeviceQueueCreateInfo<'_>> = request
            .queues
            .iter()
            .zip(priorities.iter())
            .map(|(queue, priorities)| {
                vk::DeviceQueueCreateInfo::default()
                    .flags(vk::DeviceQueueCreateFlags::from_raw(queue.flags))
                    .queue_family_index(queue.family_index)
                    .queue_priorities(priorities)
            })
            .collect();
        let requested: Vec<(u32, u32)> = request
            .queues
            .iter()
            .map(|queue| (queue.family_index, queue.priorities.len() as u32))
            .collect();

        // `enabled_layer_names` is deprecated in `ash` because device layers were deprecated in
        // Vulkan 1.0.13 and are ignored by every modern loader. The guest's list is still
        // forwarded, and the `allow` is the record of why: **dropping it here would be this layer
        // silently discarding something the engine asked for**, and the honest place for "device
        // layers do nothing any more" to be discovered is the loader that ignores them, not a
        // `.collect()` in this file that never happened.
        #[allow(deprecated)]
        let mut info = vk::DeviceCreateInfo::default()
            .flags(vk::DeviceCreateFlags::from_raw(request.flags))
            .queue_create_infos(&queue_infos)
            .enabled_layer_names(&layer_pointers)
            .enabled_extension_names(&extension_pointers);
        if let Some(features) = features.as_ref() {
            info = info.enabled_features(features);
        }
        // Built here, owned until `create_device` returns: the guest's `pNext` chain, relinked in
        // this host's memory.
        let mut chain = HostChain::new("vkCreateDevice", &request.chain)?;
        info.p_next = chain.head().cast_const();

        if let Some(features) = subset_features.as_mut() {
            features.p_next = info.p_next.cast_mut();
            info.p_next = (features as *mut vk::PhysicalDevicePortabilitySubsetFeaturesKHR<'_>).cast_const().cast();
        }

        let created = self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`; every pointer reachable from
            // `info` is into a local that outlives this call, `pNext` included -- it is `chain`'s
            // first buffer or null, and each buffer is a flat structure sized from `ash`'s
            // definition; `pAllocator` is `None` because a guest allocator is refused by name one
            // layer up.
            (unsafe { instance.create_device(physical, &info, None) }, physical)
        })?;
        match created {
            (Ok(device), physical) => {
                let mut devices = self.locked_devices();
                let token = devices.len() as u64;
                devices.push(Some(DeviceEntry {
                    instance: instance_index,
                    physical,
                    device,
                    requested,
                }));
                Ok(DriverAnswer::Ok(HostDevice::from_token(token)))
            }
            (Err(result), _) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    /// `vkGetDeviceQueue`, **with the check the specification leaves to validation**.
    ///
    /// The call returns `void`, so a family or index the device does not have is undefined
    /// behaviour with nothing to report it — and there are no validation layers on this machine.
    /// [`DeviceEntry::requested`] is what this host kept in order to be able to say no.
    fn device_queue(&self, device: HostDevice, family: u32, index: u32) -> AbiResult<HostQueue> {
        let queue = self.with_device(device, |_instance, entry| {
            let created = entry
                .requested
                .iter()
                .find(|(created_family, _)| *created_family == family)
                .copied();
            let Some((_, count)) = created else {
                return Err(refused(
                    "vkGetDeviceQueue",
                    &format!(
                        "the guest asked {device:?} for queue {index} of family {family}, and \
                         that device was created without any queue in family {family}. The \
                         families it was created with are {families:?} (family, count). \
                         `vkGetDeviceQueue` returns `void`, so there is no `VkResult` to carry \
                         this and the specification makes it undefined behaviour -- and this \
                         machine has no validation layers to notice, so the first symptom would \
                         be a `vkQueueSubmit` against a queue that was never made",
                        families = entry.requested
                    ),
                ));
            };
            if index >= count {
                return Err(refused(
                    "vkGetDeviceQueue",
                    &format!(
                        "the guest asked {device:?} for queue {index} of family {family}, and \
                         that family was created with {count} queue(s), so the valid indices are \
                         0..{count}. See the refusal above for why this cannot be a `VkResult`"
                    ),
                ));
            }
            // SAFETY: `entry.device` is live, and the family and index were checked against the
            // `VkDeviceQueueCreateInfo`s this host passed to `vkCreateDevice` one call ago.
            Ok(unsafe { entry.device.get_device_queue(family, index) })
        })??;
        let device_index = usize::try_from(device.token()).unwrap_or(usize::MAX);

        let mut queues = self.locked_queues();
        // **Deduplicated**, because one `(family, index)` pair is one queue and a renderer
        // compares two queue handles to decide whether its swapchain is `EXCLUSIVE`.
        if let Some(position) = queues.iter().position(|slot| {
            slot.as_ref().is_some_and(|entry| {
                entry.device == device_index && entry.family == family && entry.index == index
            })
        }) {
            // **And the driver is held to its own contract.** `vkGetDeviceQueue` for one family
            // and index must produce the same `VkQueue` every time; if it did not, the handle the
            // guest already holds would name a different queue from the one it would get now, and
            // every later comparison the renderer makes between its graphics and present queues
            // would be answering about the wrong pair. Nothing has ever seen this happen, which
            // is precisely why it is checked rather than assumed.
            if queues[position].as_ref().is_some_and(|entry| entry.queue != queue) {
                return Err(refused(
                    "vkGetDeviceQueue",
                    &format!(
                        "the driver answered with a different `VkQueue` for family {family}                          index {index} of {device:?} than it did the first time. The                          specification requires the same queue for the same family and index, so                          one of the two handles the guest would then hold names a queue it does                          not think it has"
                    ),
                ));
            }
            return Ok(HostQueue::from_token(position as u64));
        }
        let token = queues.len() as u64;
        queues.push(Some(QueueEntry { device: device_index, family, index, queue }));
        Ok(HostQueue::from_token(token))
    }

    fn has_device_proc(&self, device: HostDevice, name: &str) -> AbiResult<bool> {
        let name = CString::new(name).map_err(|err| {
            refused(
                "vkGetDeviceProcAddr",
                &format!(
                    "the guest's `pName` \"{name}\" has an interior NUL ({err}), so the driver \
                     would be asked about a shorter name than the guest wrote"
                ),
            )
        })?;
        self.with_device(device, |instance, entry| {
            // SAFETY: `entry.device` is a live device of `instance` and `name` outlives the call.
            // **The pointer the driver returns is dropped here and goes no further** -- this
            // method answers a `bool`, so a host code address cannot reach a guest thunk even by
            // mistake. That is the invariant the trait is shaped around.
            let found =
                unsafe { instance.get_device_proc_addr(entry.device.handle(), name.as_ptr()) };
            found.is_some()
        })
    }

    /// `vkDestroyDevice`, after the check the specification requires and nothing on this machine
    /// would otherwise make: **no object created from the device may still be alive**.
    ///
    /// # Why this host refuses rather than trusting the driver
    ///
    /// There are no validation layers here (`docs/research/graphics-spike.md` §6), and this
    /// machine's NVIDIA driver was measured failing without a diagnostic when misused -- the
    /// spike's swapchain misuse crashed it, and a short `vkGetPipelineCacheData` buffer corrupted
    /// the heap.
    /// So a device with live children is refused **naming each kind, with a count and a few
    /// tokens** ([`GfxVulkanHost::children_of`]), and the device is left exactly as it was.
    ///
    /// # Held across the check and the removal
    ///
    /// `devices` is held from the check until the slot is emptied: every creation of a child looks
    /// the device up in that table first, so none can be made in between. The slot is then `None`
    /// for good -- [`GfxVulkanHost::devices`] says why a destroyed device's index is never
    /// reused -- and so is every queue of the device, whose tokens are the answer. The driver is
    /// called with no lock held. Nothing else here is per device: `cache_gate` is one lock for the
    /// whole host, and the import probes are per physical device.
    fn destroy_device(&self, device: HostDevice) -> AbiResult<Vec<HostQueue>> {
        const CALL: &str = "vkDestroyDevice";
        let index = usize::try_from(device.token()).unwrap_or(usize::MAX);
        let (entry, gone) = {
            let mut devices = self.locked_devices();
            if Self::live_device(&devices, index).is_none() {
                return Err(refused(
                    CALL,
                    &format!(
                        "{device:?} is not a device this host holds -- it has created {}. A device \
                         the guest has already destroyed lands here too, and a second \
                         `vkDestroyDevice` of one device is a double free the driver is not \
                         required to notice",
                        devices.len()
                    ),
                ));
            }
            let live = self.children_of(index);
            if !live.is_empty() {
                return Err(refused(
                    CALL,
                    &format!(
                        "{device:?} still has objects created from it: {}. The specification \
                         requires every one to be destroyed before the device is, and there is no \
                         validation layer on this machine to report the violation -- its NVIDIA \
                         driver has been measured failing silently or corrupting the heap when \
                         misused instead. So the device is left exactly as it was",
                        describe_children(&live)
                    ),
                ));
            }
            let Some(entry) = devices.get_mut(index).and_then(Option::take) else {
                // Checked live above, under this same lock.
                return Err(refused(CALL, &format!("{device:?} vanished under its own lock")));
            };
            let mut queues = self.locked_queues();
            let gone: Vec<HostQueue> = queues
                .iter_mut()
                .enumerate()
                .filter(|(_, slot)| slot.as_ref().is_some_and(|queue| queue.device == index))
                .map(|(token, slot)| {
                    *slot = None;
                    HostQueue::from_token(token as u64)
                })
                .collect();
            (entry, gone)
        };
        // SAFETY: the device is live and this host created it; the wait is what makes the destroy
        // legal if the guest left work queued.
        let _ = unsafe { entry.device.device_wait_idle() };
        // SAFETY: no object created from the device survives -- checked above, under the lock that
        // then emptied its slot, so no new one can have been made through this host -- its queues
        // are idle, and `pAllocator` was `None` at creation.
        unsafe { entry.device.destroy_device(None) };
        Ok(gone)
    }

    // ------------------------------------------------------------------------ stage 4

    /// `vkCreateSwapchainKHR`, with the window claim this host is the only thing able to take.
    ///
    /// The order is deliberate and is the whole of the ownership story: the claim is settled
    /// **before** the driver is asked, so a conflict is a refusal naming the other owner rather
    /// than a second swapchain a driver may or may not object to. [`crate::claim`] carries the
    /// argument.
    fn create_swapchain(
        &self,
        device: HostDevice,
        request: &SwapchainRequest,
    ) -> AbiResult<DriverAnswer<HostSwapchain>> {
        let parts = self.device_parts(device)?;
        let surface_token = request.surface.ok_or_else(|| {
            refused(
                "vkCreateSwapchainKHR",
                "the request carries no surface. `VkSwapchainCreateInfoKHR::surface` is required \
                 and the shim resolves it through its own registry, so a `None` here means the \
                 shim and this host disagree about what a decoded request contains",
            )
        })?;

        let (surface, window) = {
            let surfaces = self.locked_surfaces();
            let entry = surfaces.get(surface_token.token()).ok_or_else(|| {
                refused(
                    "vkCreateSwapchainKHR",
                    &format!(
                        "{surface_token:?} is not a surface this host holds -- it holds {}, and a \
                         surface the guest has destroyed lands here too",
                        surfaces.len()
                    ),
                )
            })?;
            (entry.surface, entry.window)
        };

        // **The claim.** With an `oldSwapchain` it is transferred from the outgoing swapchain,
        // which is what makes recreation work without the window being free for an instant; with
        // none it is taken fresh, and a window somebody else owns is refused here by name.
        let mut claim = match request.old_swapchain {
            None => Some(
                crate::claim::claim_window(window, GUEST_SWAPCHAIN_OWNER).map_err(|held| {
                    refused(
                        "vkCreateSwapchainKHR",
                        &format!(
                            "`{owner}` already owns a swapchain on the window {window:#x} that \
                             this surface is over, and a native window may be associated with at \
                             most one swapchain at a time. This is a refusal naming the owner \
                             rather than `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`, because that code \
                             would send the engine looking at its own surface while the real \
                             fault is one level up: an embedding started `omni_gfx::Renderer` on \
                             a window it then handed to the guest, or the guest created a second \
                             swapchain without destroying or retiring the first. A guest \
                             replacing its own swapchain passes the outgoing one as \
                             `oldSwapchain`, which transfers the claim instead of taking it",
                            owner = held.owner,
                            window = held.window.raw()
                        ),
                    )
                })?,
            ),
            Some(old) => {
                let mut swapchains = self.locked_swapchains();
                let entry = swapchains.get_mut(old.token()).ok_or_else(|| {
                    refused(
                        "vkCreateSwapchainKHR",
                        &format!(
                            "{old:?} was passed as `oldSwapchain` and is not a swapchain this \
                             host holds -- a swapchain that has already been destroyed lands here \
                             too. The specification requires `oldSwapchain` to be a live, \
                             non-retired swapchain of this surface: the driver is entitled to \
                             reuse its images, and the graphics spike measured what happens when \
                             it is handed one that is already gone -- the NVIDIA driver crashed \
                             on the first live resize, every time, with no validation error \
                             anywhere"
                        ),
                    )
                })?;
                if entry.surface != surface_token.token() {
                    return Err(refused(
                        "vkCreateSwapchainKHR",
                        &format!(
                            "{old:?} was passed as `oldSwapchain` and belongs to {had:?}, while \
                             the swapchain being created is over {surface_token:?}. The \
                             specification requires them to be the same surface -- retiring a \
                             swapchain on one window in order to create one on another would \
                             release the first window's claim and take the second's, and neither \
                             is what the caller asked for",
                            had = HostSurface::from_token(entry.surface)
                        ),
                    ));
                }
                let taken = entry.claim.take();
                if taken.is_none() {
                    return Err(refused(
                        "vkCreateSwapchainKHR",
                        &format!(
                            "{old:?} was passed as `oldSwapchain` and has already been retired by \
                             an earlier recreation, so it no longer owns its window -- something \
                             else does. A retired swapchain cannot be retired twice: the \
                             specification requires `oldSwapchain` to be non-retired, and \
                             chaining two recreations off one outgoing swapchain would leave two \
                             live swapchains believing they own the window"
                        ),
                    ));
                }
                taken
            }
        };

        let old_handle = match request.old_swapchain {
            None => vk::SwapchainKHR::null(),
            Some(old) => self.locked_swapchains().get(old.token()).map_or_else(
                vk::SwapchainKHR::null,
                |entry| entry.handle,
            ),
        };

        let format = vk::Format::from_raw(request.format as i32);
        let extent = vk::Extent2D { width: request.width, height: request.height };
        let info = vk::SwapchainCreateInfoKHR::default()
            .flags(vk::SwapchainCreateFlagsKHR::from_raw(request.flags))
            .surface(surface)
            .min_image_count(request.min_image_count)
            .image_format(format)
            .image_color_space(vk::ColorSpaceKHR::from_raw(request.colour_space as i32))
            .image_extent(extent)
            .image_array_layers(request.array_layers)
            .image_usage(vk::ImageUsageFlags::from_raw(request.usage))
            .image_sharing_mode(vk::SharingMode::from_raw(request.sharing_mode as i32))
            .queue_family_indices(&request.queue_families)
            .pre_transform(vk::SurfaceTransformFlagsKHR::from_raw(request.pre_transform))
            .composite_alpha(vk::CompositeAlphaFlagsKHR::from_raw(request.composite_alpha))
            .present_mode(vk::PresentModeKHR::from_raw(request.present_mode as i32))
            .clipped(request.clipped != 0)
            .old_swapchain(old_handle);

        let swapchain_fn = Self::swapchain_fn(&parts.instance, &parts.device);
        // SAFETY: the device and the surface are live and belong to this host; every pointer
        // reachable from `info` is into a local or into `request`, both of which outlive the call;
        // `pNext` is null because `SwapchainRequest` cannot carry a chain (the shim refuses one by
        // name); `pAllocator` is `None` because a guest allocator is refused by name one layer up.
        let created = unsafe { swapchain_fn.create_swapchain(&info, None) };
        let handle = match created {
            Ok(handle) => handle,
            Err(result) => {
                // **The claim goes back where it came from.** A failed recreation must leave the
                // outgoing swapchain owning the window, or the next attempt would be refused for
                // a reason that is this host's rather than the driver's.
                if let (Some(old), Some(claim)) = (request.old_swapchain, claim.take()) {
                    if let Some(entry) = self.locked_swapchains().get_mut(old.token()) {
                        entry.claim = Some(claim);
                    }
                }
                return Ok(DriverAnswer::Failed(result.as_raw()));
            }
        };

        let mut swapchains = self.locked_swapchains();
        let token = swapchains.insert(SwapchainEntry {
            device: parts.index,
            surface: surface_token.token(),
            handle,
            format,
            extent,
            images: Vec::new(),
            image_tokens: Vec::new(),
            claim: claim.take(),
        });
        Ok(DriverAnswer::Ok(HostSwapchain::from_token(token)))
    }

    /// `vkGetSwapchainImagesKHR`, **cached on first enumeration**.
    ///
    /// [`GfxVulkanHost::physical`] carries the argument and it is sharper here: the index into
    /// this list is the `imageIndex` `vkAcquireNextImageKHR` answers with, so a list that
    /// re-enumerated and came back in a different order would make the guest clear one image and
    /// present another.
    fn swapchain_images(
        &self,
        swapchain: HostSwapchain,
    ) -> AbiResult<DriverAnswer<Vec<HostImage>>> {
        let parts = self.swapchain_parts(swapchain)?;
        if !parts.image_tokens.is_empty() {
            return Ok(DriverAnswer::Ok(
                parts.image_tokens.iter().copied().map(HostImage::from_token).collect(),
            ));
        }
        let device = self.device_parts(HostDevice::from_token(parts.device as u64))?;
        let swapchain_fn = Self::swapchain_fn(&device.instance, &device.device);
        // SAFETY: the swapchain is live and belongs to this device.
        let images = match unsafe { swapchain_fn.get_swapchain_images(parts.handle) } {
            Ok(images) => images,
            Err(result) => return Ok(DriverAnswer::Failed(result.as_raw())),
        };

        // `images` then `swapchains` would be the wrong order; both are taken in sequence with
        // neither held across the other, which is what the lock-order note describes.
        let tokens: Vec<u64> = {
            let mut table = self.locked_images();
            images
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    table.insert(ImageEntry {
                        swapchain: swapchain.token(),
                        index: index as u32,
                        image: *image,
                    })
                })
                .collect()
        };
        if let Some(entry) = self.locked_swapchains().get_mut(swapchain.token()) {
            entry.images = images;
            entry.image_tokens = tokens.clone();
        }
        Ok(DriverAnswer::Ok(tokens.iter().copied().map(HostImage::from_token).collect()))
    }

    fn destroy_swapchain(&self, swapchain: HostSwapchain) -> AbiResult<()> {
        let parts = self.swapchain_parts(swapchain)?;
        let device = self.device_parts(HostDevice::from_token(parts.device as u64))?;
        // SAFETY: the device is live. `vkDestroySwapchainKHR` requires that no work still
        // references the swapchain's images; the guest is supposed to have waited, and this host
        // does not rely on that -- the same argument `Drop` makes for waiting on every device.
        unsafe {
            let _ = device.device.device_wait_idle();
        }
        let swapchain_fn = Self::swapchain_fn(&device.instance, &device.device);
        // SAFETY: the swapchain is live, was created from this device, every image of it is idle
        // after the wait above, and `pAllocator` was `None` at creation.
        unsafe { swapchain_fn.destroy_swapchain(parts.handle, None) };

        // The entry, and with it the window claim: dropping the `WindowClaim` is what frees the
        // window for the next swapchain. A *retired* swapchain's entry has `None` here, so
        // destroying it releases nothing -- which is right, because its replacement holds it.
        self.locked_swapchains().remove(swapchain.token());
        let mut images = self.locked_images();
        for token in parts.image_tokens {
            images.remove(token);
        }
        Ok(())
    }

    fn create_image_view(
        &self,
        device: HostDevice,
        request: &ImageViewRequest,
    ) -> AbiResult<DriverAnswer<HostImageView>> {
        let parts = self.device_parts(device)?;
        let image = self.image_handle(request.image, "vkCreateImageView")?;
        let components = components_from_bytes("vkCreateImageView", &request.components)?;
        let range = range_from_bytes("vkCreateImageView", &request.subresource_range)?;

        let info = vk::ImageViewCreateInfo::default()
            .flags(vk::ImageViewCreateFlags::from_raw(request.flags))
            .image(image)
            .view_type(vk::ImageViewType::from_raw(request.view_type as i32))
            .format(vk::Format::from_raw(request.format as i32))
            .components(components)
            .subresource_range(range);
        // SAFETY: the device is live, `image` is a live swapchain image of it, `info` outlives the
        // call, `pNext` is null and `pAllocator` is `None` for the reasons `create_swapchain`
        // gives.
        match unsafe { parts.device.create_image_view(&info, None) } {
            Ok(view) => {
                let token = self
                    .locked_views()
                    .insert(ObjectEntry { device: parts.index, object: view });
                Ok(DriverAnswer::Ok(HostImageView::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_image_view(&self, view: HostImageView) -> AbiResult<()> {
        let (device_index, handle) = {
            let table = self.locked_views();
            self.device_of(&table, view.token(), "VkImageView", "VulkanHost::destroy_image_view")?
        };
        let device = self.device_at(device_index, "VulkanHost::destroy_image_view")?;
        // SAFETY: the view is live, was created from this device, and `pAllocator` was `None`.
        unsafe { device.destroy_image_view(handle, None) };
        self.locked_views().remove(view.token());
        Ok(())
    }

    fn create_semaphore(
        &self,
        device: HostDevice,
        flags: u32,
    ) -> AbiResult<DriverAnswer<HostSemaphore>> {
        let parts = self.device_parts(device)?;
        let info = vk::SemaphoreCreateInfo::default()
            .flags(vk::SemaphoreCreateFlags::from_raw(flags));
        // SAFETY: the device is live and `info` outlives the call.
        match unsafe { parts.device.create_semaphore(&info, None) } {
            Ok(semaphore) => {
                let token = self
                    .locked_semaphores()
                    .insert(ObjectEntry { device: parts.index, object: semaphore });
                Ok(DriverAnswer::Ok(HostSemaphore::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_semaphore(&self, semaphore: HostSemaphore) -> AbiResult<()> {
        let (device_index, handle) = {
            let table = self.locked_semaphores();
            self.device_of(
                &table,
                semaphore.token(),
                "VkSemaphore",
                "VulkanHost::destroy_semaphore",
            )?
        };
        let device = self.device_at(device_index, "VulkanHost::destroy_semaphore")?;
        // SAFETY: the semaphore is live and no submission still waits on it -- which the guest is
        // responsible for and the specification makes its responsibility.
        unsafe { device.destroy_semaphore(handle, None) };
        self.locked_semaphores().remove(semaphore.token());
        Ok(())
    }

    fn create_fence(&self, device: HostDevice, flags: u32) -> AbiResult<DriverAnswer<HostFence>> {
        let parts = self.device_parts(device)?;
        let info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::from_raw(flags));
        // SAFETY: the device is live and `info` outlives the call.
        match unsafe { parts.device.create_fence(&info, None) } {
            Ok(fence) => {
                let token =
                    self.locked_fences().insert(ObjectEntry { device: parts.index, object: fence });
                Ok(DriverAnswer::Ok(HostFence::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_fence(&self, fence: HostFence) -> AbiResult<()> {
        let (device_index, handle) = {
            let table = self.locked_fences();
            self.device_of(&table, fence.token(), "VkFence", "VulkanHost::destroy_fence")?
        };
        let device = self.device_at(device_index, "VulkanHost::destroy_fence")?;
        // SAFETY: the fence is live and no submission still references it.
        unsafe { device.destroy_fence(handle, None) };
        self.locked_fences().remove(fence.token());
        Ok(())
    }

    /// `vkWaitForFences`, **with the driver's own result carried out as an `i32`**.
    ///
    /// `ash`'s wrapper answers `VkResult<()>`, which turns `VK_TIMEOUT` into an `Err` and loses
    /// the distinction between it and a real failure. `VK_TIMEOUT` is a *success* code the guest's
    /// frame loop branches on, so this goes through the raw entry point and the code travels
    /// unchanged — the same reason `device_extensions` uses `fp_v1_0()`.
    fn wait_for_fences(
        &self,
        device: HostDevice,
        fences: &[HostFence],
        wait_all: bool,
        timeout: u64,
    ) -> AbiResult<i32> {
        let parts = self.device_parts(device)?;
        let handles = self.fence_handles(fences, parts.index, "VulkanHost::wait_for_fences")?;
        if handles.is_empty() {
            return Err(refused(
                "vkWaitForFences",
                "the guest named no fences. The specification requires `fenceCount` to be greater \
                 than zero, and `VK_SUCCESS` for a wait on nothing would tell a frame loop that \
                 work it never submitted had finished -- after which it re-records a command \
                 buffer the GPU may be reading",
            ));
        }
        let fp = parts.device.fp_v1_0().wait_for_fences;
        // SAFETY: every handle is a live fence of this device, checked above; the slice outlives
        // the call; the driver writes nothing.
        let result = unsafe {
            fp(
                parts.device.handle(),
                handles.len() as u32,
                handles.as_ptr(),
                u32::from(wait_all),
                timeout,
            )
        };
        Ok(result.as_raw())
    }

    fn reset_fences(
        &self,
        device: HostDevice,
        fences: &[HostFence],
    ) -> AbiResult<DriverAnswer<()>> {
        let parts = self.device_parts(device)?;
        let handles = self.fence_handles(fences, parts.index, "VulkanHost::reset_fences")?;
        // SAFETY: every handle is a live fence of this device and no submission still references
        // one -- which the guest is responsible for.
        match unsafe { parts.device.reset_fences(&handles) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn create_command_pool(
        &self,
        device: HostDevice,
        flags: u32,
        queue_family: u32,
    ) -> AbiResult<DriverAnswer<HostCommandPool>> {
        let parts = self.device_parts(device)?;
        let info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::from_raw(flags))
            .queue_family_index(queue_family);
        // SAFETY: the device is live and `info` outlives the call. A queue family the device does
        // not have is the driver's to reject -- it has a `VkResult` for it, unlike
        // `vkGetDeviceQueue`.
        match unsafe { parts.device.create_command_pool(&info, None) } {
            Ok(pool) => {
                let token =
                    self.locked_pools().insert(ObjectEntry { device: parts.index, object: pool });
                Ok(DriverAnswer::Ok(HostCommandPool::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn allocate_command_buffers(
        &self,
        pool: HostCommandPool,
        level: u32,
        count: u32,
    ) -> AbiResult<DriverAnswer<Vec<HostCommandBuffer>>> {
        let (device_index, handle) = {
            let table = self.locked_pools();
            self.device_of(
                &table,
                pool.token(),
                "VkCommandPool",
                "VulkanHost::allocate_command_buffers",
            )?
        };
        let device = self.device_at(device_index, "VulkanHost::allocate_command_buffers")?;
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(handle)
            .level(vk::CommandBufferLevel::from_raw(level as i32))
            .command_buffer_count(count);
        // SAFETY: the pool is live and belongs to this device; `info` outlives the call.
        match unsafe { device.allocate_command_buffers(&info) } {
            Ok(buffers) => {
                let mut table = self.locked_buffers();
                Ok(DriverAnswer::Ok(
                    buffers
                        .into_iter()
                        .map(|buffer| {
                            HostCommandBuffer::from_token(table.insert(CommandBufferEntry {
                                device: device_index,
                                pool: pool.token(),
                                buffer,
                            }))
                        })
                        .collect(),
                ))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn begin_command_buffer(
        &self,
        buffer: HostCommandBuffer,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::begin_command_buffer")?;
        let info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::from_raw(flags));
        // SAFETY: the buffer is live, belongs to this device, and `info` outlives the call. There
        // is no `pInheritanceInfo`: the shim refuses a non-NULL one by name, because a secondary
        // buffer would name a render pass this stage does not have.
        match unsafe { device.begin_command_buffer(handle, &info) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn end_command_buffer(&self, buffer: HostCommandBuffer) -> AbiResult<DriverAnswer<()>> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::end_command_buffer")?;
        // SAFETY: the buffer is live and in the recording state -- which the driver checks and
        // reports, this being the one call in the recording sequence that has a `VkResult`.
        match unsafe { device.end_command_buffer(handle) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn reset_command_buffer(
        &self,
        buffer: HostCommandBuffer,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::reset_command_buffer")?;
        // SAFETY: the buffer is live and no submission still executes it -- the guest's fence is
        // what establishes that, and it is the guest's responsibility.
        match unsafe {
            device.reset_command_buffer(handle, vk::CommandBufferResetFlags::from_raw(flags))
        } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn reset_command_pool(
        &self,
        pool: HostCommandPool,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let (device_index, handle) = {
            let table = self.locked_pools();
            self.device_of(&table, pool.token(), "VkCommandPool", "VulkanHost::reset_command_pool")?
        };
        let device = self.device_at(device_index, "VulkanHost::reset_command_pool")?;
        // SAFETY: the pool is live and belongs to this device.
        match unsafe {
            device.reset_command_pool(handle, vk::CommandPoolResetFlags::from_raw(flags))
        } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn command_buffers_of(&self, pool: HostCommandPool) -> AbiResult<Vec<HostCommandBuffer>> {
        {
            // The pool has to exist, or this would answer "none" for a token that names nothing —
            // and the shim would then destroy no handles and report success.
            let table = self.locked_pools();
            self.device_of(&table, pool.token(), "VkCommandPool", "VulkanHost::command_buffers_of")?
        };
        let table = self.locked_buffers();
        Ok(table
            .iter()
            .filter(|(_, entry)| entry.pool == pool.token())
            .map(|(token, _)| HostCommandBuffer::from_token(token))
            .collect())
    }

    fn free_command_buffers(
        &self,
        pool: HostCommandPool,
        buffers: &[HostCommandBuffer],
    ) -> AbiResult<()> {
        let (device_index, pool_handle) = {
            let table = self.locked_pools();
            self.device_of(
                &table,
                pool.token(),
                "VkCommandPool",
                "VulkanHost::free_command_buffers",
            )?
        };
        let device = self.device_at(device_index, "VulkanHost::free_command_buffers")?;

        let handles: Vec<vk::CommandBuffer> = {
            let table = self.locked_buffers();
            buffers
                .iter()
                .map(|buffer| {
                    let entry = table.get(buffer.token()).ok_or_else(|| {
                        refused(
                            "VulkanHost::free_command_buffers",
                            &format!(
                                "{buffer:?} is not a command buffer this host allocated -- it \
                                 holds {}",
                                table.len()
                            ),
                        )
                    })?;
                    // **The check the specification leaves to validation.** Freeing a buffer from
                    // a pool it does not belong to is undefined behaviour, and this machine has no
                    // validation layer to notice -- the guest can reach it with two handles it was
                    // legitimately given.
                    if entry.pool != pool.token() {
                        return Err(refused(
                            "VulkanHost::free_command_buffers",
                            &format!(
                                "{buffer:?} was allocated from command pool #{had} and is being \
                                 freed against pool #{asked}. Both handles are real; the pairing \
                                 is what is wrong, and no validation layer on this machine would \
                                 report it",
                                had = entry.pool,
                                asked = pool.token()
                            ),
                        ));
                    }
                    Ok(entry.buffer)
                })
                .collect::<AbiResult<Vec<_>>>()?
        };
        if !handles.is_empty() {
            // SAFETY: every handle was allocated from `pool_handle` on this device, checked
            // above, and no submission still executes one -- which the guest is responsible for.
            unsafe { device.free_command_buffers(pool_handle, &handles) };
        }
        let mut table = self.locked_buffers();
        for buffer in buffers {
            table.remove(buffer.token());
        }
        Ok(())
    }

    fn destroy_command_pool(&self, pool: HostCommandPool) -> AbiResult<()> {
        let (device_index, handle) = {
            let table = self.locked_pools();
            self.device_of(
                &table,
                pool.token(),
                "VkCommandPool",
                "VulkanHost::destroy_command_pool",
            )?
        };
        let device = self.device_at(device_index, "VulkanHost::destroy_command_pool")?;
        // SAFETY: the pool is live and no command buffer allocated from it is still executing.
        unsafe { device.destroy_command_pool(handle, None) };
        self.locked_pools().remove(pool.token());
        // Destroying the pool freed its buffers; the entries go with them, or a later token would
        // name a dispatchable handle to memory the driver has reclaimed.
        let mut table = self.locked_buffers();
        let doomed: Vec<u64> = table
            .iter()
            .filter(|(_, entry)| entry.pool == pool.token())
            .map(|(token, _)| token)
            .collect();
        for token in doomed {
            table.remove(token);
        }
        Ok(())
    }

    fn cmd_pipeline_barrier(
        &self,
        buffer: HostCommandBuffer,
        barrier: &PipelineBarrier,
    ) -> AbiResult<()> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::cmd_pipeline_barrier")?;
        let memory: Vec<vk::MemoryBarrier<'_>> = barrier
            .memory_barriers
            .iter()
            .map(|(src, dst)| {
                vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::from_raw(*src))
                    .dst_access_mask(vk::AccessFlags::from_raw(*dst))
            })
            .collect();
        let images: Vec<vk::ImageMemoryBarrier<'_>> = barrier
            .image_barriers
            .iter()
            .map(|entry| {
                let image = self.image_handle(entry.image, "vkCmdPipelineBarrier")?;
                let range = range_from_bytes("vkCmdPipelineBarrier", &entry.subresource_range)?;
                Ok(vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::from_raw(entry.src_access))
                    .dst_access_mask(vk::AccessFlags::from_raw(entry.dst_access))
                    .old_layout(vk::ImageLayout::from_raw(entry.old_layout as i32))
                    .new_layout(vk::ImageLayout::from_raw(entry.new_layout as i32))
                    .src_queue_family_index(entry.src_queue_family)
                    .dst_queue_family_index(entry.dst_queue_family)
                    .image(image)
                    .subresource_range(range))
            })
            .collect::<AbiResult<Vec<_>>>()?;

        // SAFETY: the command buffer is live, belongs to this device and is in the recording
        // state -- which the driver checks and reports at `vkEndCommandBuffer`, this call having
        // no `VkResult` of its own. Every image named is a live swapchain image of this host.
        // There are no buffer barriers: the shim refuses a non-zero count by name.
        unsafe {
            device.cmd_pipeline_barrier(
                handle,
                vk::PipelineStageFlags::from_raw(barrier.src_stage),
                vk::PipelineStageFlags::from_raw(barrier.dst_stage),
                vk::DependencyFlags::from_raw(barrier.dependency_flags),
                &memory,
                &[],
                &images,
            );
        }
        Ok(())
    }

    fn cmd_clear_color_image(
        &self,
        buffer: HostCommandBuffer,
        image: HostImageRef,
        layout: u32,
        colour: [u8; 16],
        ranges: &[Vec<u8>],
    ) -> AbiResult<()> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::cmd_clear_color_image")?;
        let image = self.image_handle(image, "vkCmdClearColorImage")?;
        let ranges: Vec<vk::ImageSubresourceRange> = ranges
            .iter()
            .map(|bytes| range_from_bytes("vkCmdClearColorImage", bytes))
            .collect::<AbiResult<Vec<_>>>()?;

        // **The union, reassembled from the guest's own bytes.** Which member is live is decided
        // by the image's format and not by this code, so the bytes are placed into the `float32`
        // member and the driver reads whichever one the format says -- the three members alias the
        // same sixteen bytes, so this is a `memcpy` and not an interpretation.
        let mut value = vk::ClearColorValue { float32: [0.0; 4] };
        // SAFETY: `VkClearColorValue` is a union of three sixteen-byte arrays of scalars, so every
        // byte pattern is a valid value of it and its size is exactly sixteen. The source is a
        // local array and the destination a distinct local, so they cannot overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                colour.as_ptr(),
                std::ptr::addr_of_mut!(value).cast::<u8>(),
                colour.len(),
            );
        }

        // SAFETY: as `cmd_pipeline_barrier`. The image is a live swapchain image of this host and
        // the layout is the guest's, which the driver validates.
        unsafe {
            device.cmd_clear_color_image(
                handle,
                image,
                vk::ImageLayout::from_raw(layout as i32),
                &value,
                &ranges,
            );
        }
        Ok(())
    }

    /// `vkAcquireNextImageKHR`, through the **raw** entry point.
    ///
    /// `ash`'s wrapper answers `VkResult<(u32, bool)>`, which flattens `VK_SUBOPTIMAL_KHR` into a
    /// `bool` and turns `VK_TIMEOUT` and `VK_NOT_READY` into `Err`s indistinguishable from real
    /// failures. All three are codes the guest's frame loop branches on, so the raw entry point is
    /// what carries them out unchanged.
    fn acquire_next_image(
        &self,
        swapchain: HostSwapchain,
        timeout: u64,
        semaphore: Option<HostSemaphore>,
        fence: Option<HostFence>,
    ) -> AbiResult<Acquired> {
        let parts = self.swapchain_parts(swapchain)?;
        let device = self.device_parts(HostDevice::from_token(parts.device as u64))?;
        let semaphore_handle = match semaphore {
            None => vk::Semaphore::null(),
            Some(token) => {
                let table = self.locked_semaphores();
                self.device_of(
                    &table,
                    token.token(),
                    "VkSemaphore",
                    "VulkanHost::acquire_next_image",
                )?
                .1
            }
        };
        let fence_handle = match fence {
            None => vk::Fence::null(),
            Some(token) => {
                let table = self.locked_fences();
                self.device_of(&table, token.token(), "VkFence", "VulkanHost::acquire_next_image")?
                    .1
            }
        };

        let swapchain_fn = Self::swapchain_fn(&device.instance, &device.device);
        let mut index = 0u32;
        // SAFETY: the swapchain, semaphore and fence are all live objects of this device; the
        // driver writes one `uint32_t` through `&mut index`, which is a live local.
        let result = unsafe {
            (swapchain_fn.fp().acquire_next_image_khr)(
                device.device.handle(),
                parts.handle,
                timeout,
                semaphore_handle,
                fence_handle,
                &mut index,
            )
        };
        // **`Some` for exactly the two codes that write an index.** The specification leaves
        // `pImageIndex` untouched for `VK_TIMEOUT`, `VK_NOT_READY` and every error, and answering
        // `Some(0)` there would hand the guest image 0 -- a real image that may still be on the
        // screen.
        let image_index = matches!(result, vk::Result::SUCCESS | vk::Result::SUBOPTIMAL_KHR)
            .then_some(index);
        Ok(Acquired { result: result.as_raw(), image_index })
    }

    fn queue_submit(
        &self,
        queue: HostQueue,
        submits: &[SubmitRequest],
        fence: Option<HostFence>,
    ) -> AbiResult<DriverAnswer<()>> {
        let (device_index, queue_handle) = {
            let queues = self.locked_queues();
            let entry = Self::live_queue(&queues, queue)
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_submit",
                        &format!(
                            "{queue:?} is not a live queue of this host -- it has handed out {}, \
                             and a queue whose device was destroyed lands here too",
                            queues.len()
                        ),
                    )
                })?;
            (entry.device, entry.queue)
        };
        let device = self.device_at(device_index, "VulkanHost::queue_submit")?;
        let fence_handle = match fence {
            None => vk::Fence::null(),
            Some(token) => {
                let table = self.locked_fences();
                let (owner, handle) =
                    self.device_of(&table, token.token(), "VkFence", "VulkanHost::queue_submit")?;
                if owner != device_index {
                    return Err(cross_device("VkFence", owner, device_index));
                }
                handle
            }
        };

        // The four owned vectors below hold what `VkSubmitInfo`'s pointers point at, and they are
        // built **before** the structures so that they outlive them. Building them inside the
        // `map` would drop each one at the end of its iteration and leave four dangling pointers
        // -- the defect `create_device`'s own priorities arrangement exists to make impossible
        // rather than to remember.
        let mut waits = Vec::with_capacity(submits.len());
        let mut stages = Vec::with_capacity(submits.len());
        let mut commands = Vec::with_capacity(submits.len());
        let mut signals = Vec::with_capacity(submits.len());
        for submit in submits {
            let mut these_waits = Vec::with_capacity(submit.waits.len());
            let mut these_stages = Vec::with_capacity(submit.waits.len());
            {
                let table = self.locked_semaphores();
                for (token, stage) in &submit.waits {
                    let (owner, handle) = self.device_of(
                        &table,
                        token.token(),
                        "VkSemaphore",
                        "VulkanHost::queue_submit",
                    )?;
                    if owner != device_index {
                        return Err(cross_device("VkSemaphore", owner, device_index));
                    }
                    these_waits.push(handle);
                    these_stages.push(vk::PipelineStageFlags::from_raw(*stage));
                }
            }
            let mut these_signals = Vec::with_capacity(submit.signals.len());
            {
                let table = self.locked_semaphores();
                for token in &submit.signals {
                    let (owner, handle) = self.device_of(
                        &table,
                        token.token(),
                        "VkSemaphore",
                        "VulkanHost::queue_submit",
                    )?;
                    if owner != device_index {
                        return Err(cross_device("VkSemaphore", owner, device_index));
                    }
                    these_signals.push(handle);
                }
            }
            let mut these_commands = Vec::with_capacity(submit.command_buffers.len());
            {
                let table = self.locked_buffers();
                for token in &submit.command_buffers {
                    let entry = table.get(token.token()).ok_or_else(|| {
                        refused(
                            "VulkanHost::queue_submit",
                            &format!(
                                "{token:?} is not a command buffer this host allocated -- it \
                                 holds {}",
                                table.len()
                            ),
                        )
                    })?;
                    if entry.device != device_index {
                        return Err(cross_device("VkCommandBuffer", entry.device, device_index));
                    }
                    these_commands.push(entry.buffer);
                }
            }
            waits.push(these_waits);
            stages.push(these_stages);
            commands.push(these_commands);
            signals.push(these_signals);
        }

        let infos: Vec<vk::SubmitInfo<'_>> = (0..submits.len())
            .map(|index| {
                vk::SubmitInfo::default()
                    .wait_semaphores(&waits[index])
                    .wait_dst_stage_mask(&stages[index])
                    .command_buffers(&commands[index])
                    .signal_semaphores(&signals[index])
            })
            .collect();

        // SAFETY: the queue and every object named belong to this device, checked above; every
        // pointer reachable from `infos` is into a local `Vec` that outlives the call.
        match unsafe { device.queue_submit(queue_handle, &infos, fence_handle) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    /// `vkQueuePresentKHR`, through the **raw** entry point, with `pResults` filled by the driver.
    ///
    /// [`GfxVulkanHost::acquire_next_image`]'s argument, one call along: `ash`'s wrapper flattens
    /// `VK_SUBOPTIMAL_KHR` into a `bool` and has no way to ask for the per-swapchain results at
    /// all. Both are things the guest branches on.
    fn queue_present(&self, queue: HostQueue, present: &PresentRequest) -> AbiResult<Presented> {
        let (device_index, queue_handle) = {
            let queues = self.locked_queues();
            let entry = Self::live_queue(&queues, queue)
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_present",
                        &format!(
                            "{queue:?} is not a live queue of this host -- it has handed out {}, \
                             and a queue whose device was destroyed lands here too",
                            queues.len()
                        ),
                    )
                })?;
            (entry.device, entry.queue)
        };
        let device = self.device_parts(HostDevice::from_token(device_index as u64))?;

        let waits: Vec<vk::Semaphore> = {
            let table = self.locked_semaphores();
            present
                .waits
                .iter()
                .map(|token| {
                    let (owner, handle) = self.device_of(
                        &table,
                        token.token(),
                        "VkSemaphore",
                        "VulkanHost::queue_present",
                    )?;
                    if owner != device_index {
                        return Err(cross_device("VkSemaphore", owner, device_index));
                    }
                    Ok(handle)
                })
                .collect::<AbiResult<Vec<_>>>()?
        };
        let (handles, indices): (Vec<vk::SwapchainKHR>, Vec<u32>) = {
            let table = self.locked_swapchains();
            let mut handles = Vec::with_capacity(present.swapchains.len());
            let mut indices = Vec::with_capacity(present.swapchains.len());
            for (token, index) in &present.swapchains {
                let entry = table.get(token.token()).ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_present",
                        &format!(
                            "{token:?} is not a swapchain this host holds -- one that has been \
                             destroyed lands here too, and presenting to a destroyed swapchain is \
                             the use-after-free this machine has no validation layer to report",
                            token = token
                        ),
                    )
                })?;
                handles.push(entry.handle);
                indices.push(*index);
            }
            (handles, indices)
        };

        // `pResults` is `swapchainCount` entries the **driver** writes. Initialised to
        // `VK_SUCCESS` so that a driver which writes fewer than it should leaves something
        // defined rather than uninitialised; nothing depends on the initial value.
        let mut results = if present.wants_per_swapchain_results {
            vec![vk::Result::SUCCESS; handles.len()]
        } else {
            Vec::new()
        };
        let info = vk::PresentInfoKHR {
            s_type: vk::StructureType::PRESENT_INFO_KHR,
            p_next: std::ptr::null(),
            wait_semaphore_count: waits.len() as u32,
            p_wait_semaphores: waits.as_ptr(),
            swapchain_count: handles.len() as u32,
            p_swapchains: handles.as_ptr(),
            p_image_indices: indices.as_ptr(),
            p_results: if results.is_empty() {
                std::ptr::null_mut()
            } else {
                results.as_mut_ptr()
            },
            _marker: std::marker::PhantomData,
        };

        let swapchain_fn = Self::swapchain_fn(&device.instance, &device.device);
        // SAFETY: the queue, every semaphore and every swapchain belong to this device, checked
        // above; every pointer in `info` is into a local `Vec` that outlives the call, and
        // `p_results` addresses exactly `swapchain_count` `VkResult`s when it is not null.
        let result = unsafe { (swapchain_fn.fp().queue_present_khr)(queue_handle, &info) };
        Ok(Presented {
            result: result.as_raw(),
            per_swapchain: results.iter().map(|result| result.as_raw()).collect(),
        })
    }

    fn queue_wait_idle(&self, queue: HostQueue) -> AbiResult<DriverAnswer<()>> {
        let (device_index, queue_handle) = {
            let queues = self.locked_queues();
            let entry = Self::live_queue(&queues, queue)
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_wait_idle",
                        &format!(
                            "{queue:?} is not a live queue of this host -- it has handed out {}, \
                             and a queue whose device was destroyed lands here too",
                            queues.len()
                        ),
                    )
                })?;
            (entry.device, entry.queue)
        };
        let device = self.device_at(device_index, "VulkanHost::queue_wait_idle")?;
        // SAFETY: the queue is live and belongs to this device.
        match unsafe { device.queue_wait_idle(queue_handle) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn device_wait_idle(&self, device: HostDevice) -> AbiResult<DriverAnswer<()>> {
        let parts = self.device_parts(device)?;
        // SAFETY: the device is live.
        match unsafe { parts.device.device_wait_idle() } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    // ============================================================ stage 5: memory and resources

    fn importable_memory_types(&self, physical: HostPhysicalDevice) -> AbiResult<u32> {
        let probed = self.with_physical(physical, |instance, device| {
            self.probe_importable(instance, device)
        })?;
        probed.map(|(bits, _)| bits)
    }

    fn device_extensions_required_by_host(
        &self,
        physical: HostPhysicalDevice,
    ) -> AbiResult<Vec<String>> {
        // **One name, and only when the device has it.** `VK_EXT_external_memory_host` is what
        // makes `vkMapMemory` able to answer with an address inside `GuestSpace`, and it has to be
        // enabled on the device the guest will allocate from. A device without it gets an empty
        // list: nothing is added, nothing is recorded, `vkCreateDevice` behaves exactly as it did
        // in stage 4, and the consequence surfaces as the memory-type mask instead.
        //
        // The probe is what decides, rather than a second reading of the extension list, so that
        // this answer and `importable_memory_types` cannot disagree -- adding the extension to a
        // device whose importable set is empty would be an addition that buys nothing.
        let probed = self.with_physical(physical, |instance, device| {
            self.probe_importable(instance, device)
        })?;
        let (bits, _) = probed?;
        Ok(if bits == 0 {
            Vec::new()
        } else {
            vec![vk::EXT_EXTERNAL_MEMORY_HOST_NAME.to_string_lossy().into_owned()]
        })
    }

    fn memory_plan(&self, device: HostDevice, memory_type_index: u32) -> AbiResult<MemoryPlan> {
        const METHOD: &str = "VulkanHost::memory_plan";
        let parts = self.device_parts(device)?;
        // SAFETY: `physical` is the device this logical device was created from and the instance
        // that enumerated it is still live.
        let properties = unsafe { parts.instance.get_physical_device_memory_properties(parts.physical) };
        if memory_type_index >= properties.memory_type_count {
            return Err(refused(
                METHOD,
                &format!(
                    "the guest asked about memory type {memory_type_index} and this device has \
                     {count}. The index is a guest `uint32_t` and this array is a fixed 32-entry \
                     one, so reading past `memoryTypeCount` would be a host read of whatever the \
                     driver left in the unused tail -- which is a plausible set of property flags",
                    count = properties.memory_type_count
                ),
            ));
        }
        let (bits, alignment) = self.probe_importable(&parts.instance, parts.physical)?;
        Ok(MemoryPlan {
            property_flags: properties.memory_types[memory_type_index as usize]
                .property_flags
                .as_raw(),
            importable: bits & (1u32 << memory_type_index) != 0,
            import_alignment: alignment,
        })
    }

    fn allocate_memory(
        &self,
        device: HostDevice,
        allocation: &MemoryAllocation,
    ) -> AbiResult<DriverAnswer<HostDeviceMemory>> {
        let parts = self.device_parts(device)?;
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(allocation.size)
            .memory_type_index(allocation.memory_type_index);

        // **The one `pNext` this layer constructs.** `omni-android` refuses every chain the guest
        // sends and builds this one itself, because the pointer in it has to be a `GuestSpace`
        // address and no guest is allowed to choose it.
        let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
        if let Some(pointer) = allocation.host_pointer {
            let pointer = usize::try_from(pointer).map_err(|_| {
                refused(
                    "VulkanHost::allocate_memory",
                    &format!("the guest-space pointer {pointer:#x} does not fit this host's address space"),
                )
            })?;
            import.p_host_pointer = pointer as *mut std::ffi::c_void;
            // The **rounded** length, not the guest's `allocationSize`: the specification requires
            // an imported host pointer's length to be a multiple of
            // `minImportedHostPointerAlignment`, and the shim mapped exactly that many bytes.
            info = info.allocation_size(allocation.import_length).push_next(&mut import);
        }

        // SAFETY: the device is live, `info` and anything it chains outlive the call, and
        // `pAllocator` is `None` because a guest allocator is refused by name one layer up. When
        // the import is present, `p_host_pointer` names pages `omni-android` mapped and committed
        // in the guest address space and will keep mapped until `vkFreeMemory`.
        match unsafe { parts.device.allocate_memory(&info, None) } {
            Ok(memory) => {
                let token = self.locked_memories().insert(MemoryEntry {
                    device: parts.index,
                    memory,
                    imported: allocation.host_pointer.is_some(),
                });
                Ok(DriverAnswer::Ok(HostDeviceMemory::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn free_memory(&self, memory: HostDeviceMemory) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::free_memory";
        let (device_index, handle, _) = self.memory_of(memory, METHOD)?;
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the allocation is live, was made from this device, and `pAllocator` was `None`.
        // Nothing bound to it is still in use: that is the guest's responsibility and the
        // specification makes it so.
        unsafe { device.free_memory(handle, None) };
        self.locked_memories().remove(memory.token());
        Ok(())
    }

    fn map_memory(
        &self,
        memory: HostDeviceMemory,
        offset: u64,
        size: u64,
        flags: u32,
    ) -> AbiResult<DriverAnswer<u64>> {
        const METHOD: &str = "VulkanHost::map_memory";
        let (device_index, handle, imported) = self.memory_of(memory, METHOD)?;
        if !imported {
            return Err(refused(
                METHOD,
                "this allocation was forwarded rather than imported, so its bytes are the \
                 driver's and the only pointer available for it is one outside `GuestSpace`. The \
                 shim refuses `vkMapMemory` on such an allocation before reaching here; a call \
                 that got this far means the shim's import record and this host's disagree",
            ));
        }
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the allocation is live, host-visible (it was imported from host pages), and not
        // currently mapped -- which the driver itself enforces and reports.
        match unsafe {
            device.map_memory(handle, offset, size, vk::MemoryMapFlags::from_raw(flags))
        } {
            // The address travels as an integer so that the shim can **compare** it against the
            // pointer it imported. It is never written into guest memory unchecked; see
            // `VulkanHost::map_memory`'s own documentation for the invariant.
            Ok(pointer) => Ok(DriverAnswer::Ok(pointer as usize as u64)),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn unmap_memory(&self, memory: HostDeviceMemory) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::unmap_memory";
        let (device_index, handle, _) = self.memory_of(memory, METHOD)?;
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the allocation is live and currently mapped, which the driver enforces.
        unsafe { device.unmap_memory(handle) };
        Ok(())
    }

    fn flush_mapped_memory_ranges(
        &self,
        device: HostDevice,
        ranges: &[(HostDeviceMemory, u64, u64)],
    ) -> AbiResult<DriverAnswer<()>> {
        const METHOD: &str = "VulkanHost::flush_mapped_memory_ranges";
        let parts = self.device_parts(device)?;
        let built = self.mapped_ranges(ranges, parts.index, METHOD)?;
        // SAFETY: every range names a live allocation of this device, and the structures outlive
        // the call.
        match unsafe { parts.device.flush_mapped_memory_ranges(&built) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn invalidate_mapped_memory_ranges(
        &self,
        device: HostDevice,
        ranges: &[(HostDeviceMemory, u64, u64)],
    ) -> AbiResult<DriverAnswer<()>> {
        const METHOD: &str = "VulkanHost::invalidate_mapped_memory_ranges";
        let parts = self.device_parts(device)?;
        let built = self.mapped_ranges(ranges, parts.index, METHOD)?;
        // SAFETY: as `flush_mapped_memory_ranges`.
        match unsafe { parts.device.invalidate_mapped_memory_ranges(&built) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn buffer_memory_requirements(&self, buffer: HostBuffer) -> AbiResult<Vec<u8>> {
        const METHOD: &str = "VulkanHost::buffer_memory_requirements";
        let (device_index, handle) = {
            let table = self.locked_vk_buffers();
            self.device_of(&table, buffer.token(), "VkBuffer", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the buffer is live and belongs to this device.
        let requirements = unsafe { device.get_buffer_memory_requirements(handle) };
        Ok(requirements_bytes(&requirements))
    }

    fn image_memory_requirements(&self, image: HostCreatedImage) -> AbiResult<Vec<u8>> {
        const METHOD: &str = "VulkanHost::image_memory_requirements";
        let (device_index, handle) = {
            let table = self.locked_created_images();
            self.device_of(&table, image.token(), "VkImage", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the image is live and belongs to this device.
        let requirements = unsafe { device.get_image_memory_requirements(handle) };
        Ok(requirements_bytes(&requirements))
    }

    fn swapchain_image_memory_requirements(&self, image: HostImage) -> AbiResult<Vec<u8>> {
        const METHOD: &str = "VulkanHost::swapchain_image_memory_requirements";
        // The image table, then the swapchain table, each released before the next is taken --
        // the order `swapchain_images` documents.
        let (swapchain, handle) = {
            let table = self.locked_images();
            let entry = table.get(image.token()).ok_or_else(|| {
                refused(
                    METHOD,
                    &format!(
                        "{image:?} is not a swapchain image this host holds -- it holds {}. An \
                         image whose swapchain has been destroyed lands here",
                        table.len()
                    ),
                )
            })?;
            (entry.swapchain, entry.image)
        };
        let parts = self.swapchain_parts(HostSwapchain::from_token(swapchain))?;
        let device = self.device_at(parts.device, METHOD)?;
        // SAFETY: the image is a live image of a live swapchain of this device, and asking its
        // requirements is valid -- only binding memory to it or destroying it is not.
        let requirements = unsafe { device.get_image_memory_requirements(handle) };
        Ok(requirements_bytes(&requirements))
    }

    fn bind_buffer_memory(
        &self,
        buffer: HostBuffer,
        memory: HostDeviceMemory,
        offset: u64,
    ) -> AbiResult<DriverAnswer<()>> {
        const METHOD: &str = "VulkanHost::bind_buffer_memory";
        let (buffer_device, handle) = {
            let table = self.locked_vk_buffers();
            self.device_of(&table, buffer.token(), "VkBuffer", METHOD)?
        };
        let (memory_device, allocation, _) = self.memory_of(memory, METHOD)?;
        if buffer_device != memory_device {
            return Err(cross_device("VkDeviceMemory", memory_device, buffer_device));
        }
        let device = self.device_at(buffer_device, METHOD)?;
        // SAFETY: both objects are live and belong to this device, and nothing is bound to the
        // buffer yet -- which the driver enforces and reports.
        match unsafe { device.bind_buffer_memory(handle, allocation, offset) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn bind_image_memory(
        &self,
        image: HostCreatedImage,
        memory: HostDeviceMemory,
        offset: u64,
    ) -> AbiResult<DriverAnswer<()>> {
        const METHOD: &str = "VulkanHost::bind_image_memory";
        let (image_device, handle) = {
            let table = self.locked_created_images();
            self.device_of(&table, image.token(), "VkImage", METHOD)?
        };
        let (memory_device, allocation, _) = self.memory_of(memory, METHOD)?;
        if image_device != memory_device {
            return Err(cross_device("VkDeviceMemory", memory_device, image_device));
        }
        let device = self.device_at(image_device, METHOD)?;
        // SAFETY: as `bind_buffer_memory`.
        match unsafe { device.bind_image_memory(handle, allocation, offset) } {
            Ok(()) => Ok(DriverAnswer::Ok(())),
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn create_buffer(
        &self,
        device: HostDevice,
        request: &BufferRequest,
    ) -> AbiResult<DriverAnswer<HostBuffer>> {
        let parts = self.device_parts(device)?;
        let info = vk::BufferCreateInfo::default()
            .flags(vk::BufferCreateFlags::from_raw(request.flags))
            .size(request.size)
            .usage(vk::BufferUsageFlags::from_raw(request.usage))
            .sharing_mode(vk::SharingMode::from_raw(request.sharing_mode as i32))
            .queue_family_indices(&request.queue_families);
        // SAFETY: the device is live, `info` and the slice it borrows outlive the call, `pNext`
        // is null because the shim refuses a chain, and `pAllocator` is `None`.
        match unsafe { parts.device.create_buffer(&info, None) } {
            Ok(buffer) => {
                let token = self
                    .locked_vk_buffers()
                    .insert(ObjectEntry { device: parts.index, object: buffer });
                Ok(DriverAnswer::Ok(HostBuffer::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_buffer(&self, buffer: HostBuffer) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_buffer";
        let (device_index, handle) = {
            let table = self.locked_vk_buffers();
            self.device_of(&table, buffer.token(), "VkBuffer", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the buffer is live and nothing the GPU is executing still references it, which
        // the specification makes the guest's responsibility.
        unsafe { device.destroy_buffer(handle, None) };
        self.locked_vk_buffers().remove(buffer.token());
        Ok(())
    }

    fn create_image(
        &self,
        device: HostDevice,
        request: &ImageRequest,
    ) -> AbiResult<DriverAnswer<HostCreatedImage>> {
        let parts = self.device_parts(device)?;
        let info = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::from_raw(request.flags))
            .image_type(vk::ImageType::from_raw(request.image_type as i32))
            .format(vk::Format::from_raw(request.format as i32))
            .extent(vk::Extent3D {
                width: request.extent[0],
                height: request.extent[1],
                depth: request.extent[2],
            })
            .mip_levels(request.mip_levels)
            .array_layers(request.array_layers)
            .samples(vk::SampleCountFlags::from_raw(request.samples))
            .tiling(vk::ImageTiling::from_raw(request.tiling as i32))
            .usage(vk::ImageUsageFlags::from_raw(request.usage))
            .sharing_mode(vk::SharingMode::from_raw(request.sharing_mode as i32))
            .queue_family_indices(&request.queue_families)
            .initial_layout(vk::ImageLayout::from_raw(request.initial_layout as i32));
        // SAFETY: as `create_buffer`.
        match unsafe { parts.device.create_image(&info, None) } {
            Ok(image) => {
                let token = self
                    .locked_created_images()
                    .insert(ObjectEntry { device: parts.index, object: image });
                Ok(DriverAnswer::Ok(HostCreatedImage::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_image(&self, image: HostCreatedImage) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_image";
        let (device_index, handle) = {
            let table = self.locked_created_images();
            self.device_of(&table, image.token(), "VkImage", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the image is live, is one the **guest** created rather than a swapchain's --
        // they are separate tables and a swapchain image cannot reach here -- and nothing the GPU
        // is executing still references it.
        unsafe { device.destroy_image(handle, None) };
        self.locked_created_images().remove(image.token());
        Ok(())
    }

    fn create_sampler(
        &self,
        device: HostDevice,
        body: &[u8],
    ) -> AbiResult<DriverAnswer<HostSampler>> {
        let parts = self.device_parts(device)?;
        let info = sampler_from_body("vkCreateSampler", body)?;
        // SAFETY: the device is live and `info` outlives the call.
        match unsafe { parts.device.create_sampler(&info, None) } {
            Ok(sampler) => {
                let token = self
                    .locked_samplers()
                    .insert(ObjectEntry { device: parts.index, object: sampler });
                Ok(DriverAnswer::Ok(HostSampler::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_sampler(&self, sampler: HostSampler) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_sampler";
        let (device_index, handle) = {
            let table = self.locked_samplers();
            self.device_of(&table, sampler.token(), "VkSampler", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the sampler is live and no descriptor in use still names it.
        unsafe { device.destroy_sampler(handle, None) };
        self.locked_samplers().remove(sampler.token());
        Ok(())
    }

    fn create_shader_module(
        &self,
        device: HostDevice,
        flags: u32,
        code: &[u8],
    ) -> AbiResult<DriverAnswer<HostShaderModule>> {
        const METHOD: &str = "VulkanHost::create_shader_module";
        let parts = self.device_parts(device)?;
        if code.len() % 4 != 0 || code.is_empty() {
            return Err(refused(
                METHOD,
                &format!(
                    "the SPIR-V arrived as {} byte(s), which is not a non-zero multiple of four. \
                     `pCode` is a `const uint32_t *`, so a driver handed this reads past the end \
                     of the buffer while assembling its last word",
                    code.len()
                ),
            ));
        }
        // **The words, not a re-encoding.** SPIR-V is little-endian 32-bit words on both sides;
        // this is the copy that gives them four-byte alignment, which `pCode` requires and a
        // `&[u8]` does not guarantee. Nothing about their content changes.
        let words: Vec<u32> = code
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("four")))
            .collect();
        let info = vk::ShaderModuleCreateInfo::default()
            .flags(vk::ShaderModuleCreateFlags::from_raw(flags))
            .code(&words);
        // SAFETY: the device is live, `info` and `words` outlive the call, and `pAllocator` is
        // `None`. A malformed module is the driver's to reject and it does.
        match unsafe { parts.device.create_shader_module(&info, None) } {
            Ok(module) => {
                let token = self
                    .locked_modules()
                    .insert(ObjectEntry { device: parts.index, object: module });
                Ok(DriverAnswer::Ok(HostShaderModule::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_shader_module(&self, module: HostShaderModule) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_shader_module";
        let (device_index, handle) = {
            let table = self.locked_modules();
            self.device_of(&table, module.token(), "VkShaderModule", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the module is live. Destroying one a pipeline was built from is explicitly
        // permitted -- the pipeline does not reference it after creation.
        unsafe { device.destroy_shader_module(handle, None) };
        self.locked_modules().remove(module.token());
        Ok(())
    }

    fn create_pipeline_layout(
        &self,
        device: HostDevice,
        request: &PipelineLayoutRequest,
    ) -> AbiResult<DriverAnswer<HostPipelineLayout>> {
        const METHOD: &str = "VulkanHost::create_pipeline_layout";
        let parts = self.device_parts(device)?;
        let layouts = {
            let table = self.locked_set_layouts();
            self.objects_of(
                &table,
                request.set_layouts.iter().copied().map(HostDescriptorSetLayout::token),
                "VkDescriptorSetLayout",
                parts.index,
                METHOD,
            )?
        };
        let ranges: Vec<vk::PushConstantRange> = request
            .push_constant_ranges
            .iter()
            .map(|bytes| push_constant_from_bytes(METHOD, bytes))
            .collect::<AbiResult<Vec<_>>>()?;
        let info = vk::PipelineLayoutCreateInfo::default()
            .flags(vk::PipelineLayoutCreateFlags::from_raw(request.flags))
            .set_layouts(&layouts)
            .push_constant_ranges(&ranges);
        // SAFETY: the device is live, every handle is one of its own, and `info` and the slices it
        // borrows outlive the call.
        match unsafe { parts.device.create_pipeline_layout(&info, None) } {
            Ok(layout) => {
                let token = self
                    .locked_layouts()
                    .insert(ObjectEntry { device: parts.index, object: layout });
                Ok(DriverAnswer::Ok(HostPipelineLayout::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_pipeline_layout(&self, layout: HostPipelineLayout) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_pipeline_layout";
        let (device_index, handle) = {
            let table = self.locked_layouts();
            self.device_of(&table, layout.token(), "VkPipelineLayout", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the layout is live and no command buffer being recorded still names it.
        unsafe { device.destroy_pipeline_layout(handle, None) };
        self.locked_layouts().remove(layout.token());
        Ok(())
    }

    fn create_render_pass(
        &self,
        device: HostDevice,
        request: &RenderPassRequest,
    ) -> AbiResult<DriverAnswer<HostRenderPass>> {
        const METHOD: &str = "VulkanHost::create_render_pass";
        let parts = self.device_parts(device)?;
        let attachments: Vec<vk::AttachmentDescription> = request
            .attachments
            .iter()
            .map(|bytes| attachment_from_bytes(METHOD, bytes))
            .collect::<AbiResult<Vec<_>>>()?;
        let dependencies: Vec<vk::SubpassDependency> = request
            .dependencies
            .iter()
            .map(|bytes| dependency_from_bytes(METHOD, bytes))
            .collect::<AbiResult<Vec<_>>>()?;

        // The references have to outlive the `VkSubpassDescription`s that point at them, so they
        // are collected first and borrowed afterwards — `create_device`'s `priorities` makes the
        // same argument, and getting it wrong leaves `pColorAttachments` dangling.
        let references: Vec<SubpassReferences> = request
            .subpasses
            .iter()
            .map(|subpass| {
                Ok(SubpassReferences {
                    input: references_from_bytes(METHOD, &subpass.input_attachments)?,
                    colour: references_from_bytes(METHOD, &subpass.color_attachments)?,
                    resolve: references_from_bytes(METHOD, &subpass.resolve_attachments)?,
                    depth: subpass
                        .depth_stencil_attachment
                        .as_ref()
                        .map(|bytes| reference_from_bytes(METHOD, bytes))
                        .transpose()?,
                    preserve: subpass.preserve_attachments.clone(),
                })
            })
            .collect::<AbiResult<Vec<_>>>()?;
        let subpasses: Vec<vk::SubpassDescription<'_>> = request
            .subpasses
            .iter()
            .zip(references.iter())
            .map(|(subpass, references)| {
                let mut description = vk::SubpassDescription::default()
                    .flags(vk::SubpassDescriptionFlags::from_raw(subpass.flags))
                    .pipeline_bind_point(vk::PipelineBindPoint::from_raw(subpass.bind_point as i32))
                    .input_attachments(&references.input)
                    .color_attachments(&references.colour)
                    .preserve_attachments(&references.preserve);
                // **Only when the guest supplied one.** `pResolveAttachments` NULL and
                // `pResolveAttachments` pointing at `colorAttachmentCount` entries are different
                // subpasses, and `ash` writes the pointer unconditionally once this is called.
                if !references.resolve.is_empty() {
                    description = description.resolve_attachments(&references.resolve);
                }
                if let Some(depth) = references.depth.as_ref() {
                    description = description.depth_stencil_attachment(depth);
                }
                description
            })
            .collect();

        let info = vk::RenderPassCreateInfo::default()
            .flags(vk::RenderPassCreateFlags::from_raw(request.flags))
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);
        // SAFETY: the device is live and every pointer reachable from `info` is into a local that
        // outlives the call.
        match unsafe { parts.device.create_render_pass(&info, None) } {
            Ok(pass) => {
                let token =
                    self.locked_passes().insert(ObjectEntry { device: parts.index, object: pass });
                Ok(DriverAnswer::Ok(HostRenderPass::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_render_pass(&self, pass: HostRenderPass) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_render_pass";
        let (device_index, handle) = {
            let table = self.locked_passes();
            self.device_of(&table, pass.token(), "VkRenderPass", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the render pass is live and nothing in flight still names it.
        unsafe { device.destroy_render_pass(handle, None) };
        self.locked_passes().remove(pass.token());
        Ok(())
    }

    fn create_framebuffer(
        &self,
        device: HostDevice,
        request: &FramebufferRequest,
    ) -> AbiResult<DriverAnswer<HostFramebuffer>> {
        const METHOD: &str = "VulkanHost::create_framebuffer";
        let parts = self.device_parts(device)?;
        let Some(pass_token) = request.render_pass else {
            return Err(refused(METHOD, "the request names no render pass"));
        };
        let pass = {
            let table = self.locked_passes();
            let (owner, handle) =
                self.device_of(&table, pass_token.token(), "VkRenderPass", METHOD)?;
            if owner != parts.index {
                return Err(cross_device("VkRenderPass", owner, parts.index));
            }
            handle
        };
        let attachments = {
            let table = self.locked_views();
            self.objects_of(
                &table,
                request.attachments.iter().copied().map(HostImageView::token),
                "VkImageView",
                parts.index,
                METHOD,
            )?
        };
        let info = vk::FramebufferCreateInfo::default()
            .flags(vk::FramebufferCreateFlags::from_raw(request.flags))
            .render_pass(pass)
            .attachments(&attachments)
            .width(request.width)
            .height(request.height)
            .layers(request.layers);
        // SAFETY: the device is live, every handle is one of its own, and `info` outlives the call.
        match unsafe { parts.device.create_framebuffer(&info, None) } {
            Ok(framebuffer) => {
                let token = self
                    .locked_framebuffers()
                    .insert(ObjectEntry { device: parts.index, object: framebuffer });
                Ok(DriverAnswer::Ok(HostFramebuffer::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_framebuffer(&self, framebuffer: HostFramebuffer) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_framebuffer";
        let (device_index, handle) = {
            let table = self.locked_framebuffers();
            self.device_of(&table, framebuffer.token(), "VkFramebuffer", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the framebuffer is live and no render pass in flight still names it.
        unsafe { device.destroy_framebuffer(handle, None) };
        self.locked_framebuffers().remove(framebuffer.token());
        Ok(())
    }

    fn create_query_pool(
        &self,
        device: HostDevice,
        request: &QueryPoolRequest,
    ) -> AbiResult<DriverAnswer<HostQueryPool>> {
        let parts = self.device_parts(device)?;
        let info = vk::QueryPoolCreateInfo::default()
            .flags(vk::QueryPoolCreateFlags::from_raw(request.flags))
            .query_type(vk::QueryType::from_raw(request.query_type as i32))
            .query_count(request.query_count)
            .pipeline_statistics(vk::QueryPipelineStatisticFlags::from_raw(
                request.pipeline_statistics,
            ));
        // SAFETY: the device is live and `info` has no pointer beyond a null `pNext`.
        match unsafe { parts.device.create_query_pool(&info, None) } {
            Ok(pool) => {
                let token =
                    self.locked_query_pools().insert(ObjectEntry { device: parts.index, object: pool });
                Ok(DriverAnswer::Ok(HostQueryPool::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn cmd_reset_query_pool(
        &self,
        buffer: HostCommandBuffer,
        pool: HostQueryPool,
        first: u32,
        count: u32,
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_reset_query_pool";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let target = {
            let table = self.locked_query_pools();
            self.device_of(&table, pool.token(), "VkQueryPool", METHOD)?.1
        };
        // SAFETY: the command buffer is live and recording, outside a render pass, and the pool
        // is live -- the driver enforces the rest and reports it.
        unsafe { device.cmd_reset_query_pool(handle, target, first, count) };
        Ok(())
    }

    fn cmd_write_timestamp(
        &self,
        buffer: HostCommandBuffer,
        stage: u32,
        pool: HostQueryPool,
        query: u32,
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_write_timestamp";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let target = {
            let table = self.locked_query_pools();
            self.device_of(&table, pool.token(), "VkQueryPool", METHOD)?.1
        };
        // SAFETY: the command buffer is live and recording and the pool is live.
        unsafe {
            device.cmd_write_timestamp(
                handle,
                vk::PipelineStageFlags::from_raw(stage),
                target,
                query,
            );
        }
        Ok(())
    }

    fn get_query_pool_results(
        &self,
        device: HostDevice,
        pool: HostQueryPool,
        first: u32,
        count: u32,
        stride: u64,
        flags: u32,
        data: &mut [u8],
    ) -> AbiResult<i32> {
        const METHOD: &str = "VulkanHost::get_query_pool_results";
        let parts = self.device_parts(device)?;
        let (owner, handle) = {
            let table = self.locked_query_pools();
            self.device_of(&table, pool.token(), "VkQueryPool", METHOD)?
        };
        if owner != parts.index {
            return Err(cross_device("VkQueryPool", owner, parts.index));
        }
        // The raw entry point rather than `ash::Device::get_query_pool_results`, which fixes the
        // stride at `size_of::<T>()` and the count at the slice's length: the guest's stride and
        // count are its own, and so is the availability value interleaved with each result.
        //
        // SAFETY: the device and pool are live and the pool is the device's own; `data` is
        // `data.len()` writable bytes, and the shim sized it to the whole span the driver writes
        // for these queries, flags and stride -- which is also the `dataSize` passed.
        let result = unsafe {
            (parts.device.fp_v1_0().get_query_pool_results)(
                parts.device.handle(),
                handle,
                first,
                count,
                data.len(),
                data.as_mut_ptr().cast(),
                stride,
                vk::QueryResultFlags::from_raw(flags),
            )
        };
        Ok(result.as_raw())
    }

    fn destroy_query_pool(&self, pool: HostQueryPool) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_query_pool";
        let (device_index, handle) = {
            let table = self.locked_query_pools();
            self.device_of(&table, pool.token(), "VkQueryPool", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the pool is live; a pool still referenced by a pending command buffer is the
        // guest's to have waited for, as on a device.
        unsafe { device.destroy_query_pool(handle, None) };
        self.locked_query_pools().remove(pool.token());
        Ok(())
    }

    fn create_pipeline_cache(
        &self,
        device: HostDevice,
        flags: u32,
        initial_data: &[u8],
    ) -> AbiResult<DriverAnswer<HostPipelineCache>> {
        let parts = self.device_parts(device)?;
        let info = vk::PipelineCacheCreateInfo::default()
            .flags(vk::PipelineCacheCreateFlags::from_raw(flags))
            .initial_data(initial_data);
        // SAFETY: the device is live and `info` and the slice it borrows outlive the call. A blob
        // from another driver is rejected by this one's own header check, which is what the
        // header is for.
        match unsafe { parts.device.create_pipeline_cache(&info, None) } {
            Ok(cache) => {
                let token =
                    self.locked_caches().insert(ObjectEntry { device: parts.index, object: cache });
                Ok(DriverAnswer::Ok(HostPipelineCache::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_pipeline_cache(&self, cache: HostPipelineCache) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_pipeline_cache";
        let (device_index, handle) = {
            let table = self.locked_caches();
            self.device_of(&table, cache.token(), "VkPipelineCache", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the cache is live and no pipeline creation is in progress against it.
        unsafe { device.destroy_pipeline_cache(handle, None) };
        self.locked_caches().remove(cache.token());
        Ok(())
    }

    /// `vkGetPipelineCacheData`: the whole blob, read into a buffer of exactly the size the driver
    /// reported, with nothing able to grow the cache in between.
    ///
    /// # MEASURED: this machine's driver writes past a short buffer
    ///
    /// On the RTX 4060, a 5,499-byte cache read with `*pDataSize = 2766` answered `VK_INCOMPLETE`
    /// with a count of 36, and had written all 5,499 bytes: 2,733 past the end of the buffer. The
    /// first run of the live test died of it, `STATUS_HEAP_CORRUPTION`. A buffer of the full size
    /// was written exactly, with not one byte past it. So this host never hands the driver a
    /// buffer shorter than the blob. It asks the size and fills exactly that much, and
    /// [`GfxVulkanHost::cache_gate`] is held exclusively across both calls so that no pipeline
    /// built through this host can grow the cache between them.
    ///
    /// The raw entry point rather than `ash::Device::get_pipeline_cache_data`: if the cache grows
    /// between its two calls, that one hands the driver a short buffer and retries on
    /// `VK_INCOMPLETE` after the damage is done.
    fn pipeline_cache_data(
        &self,
        device: HostDevice,
        cache: HostPipelineCache,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        const METHOD: &str = "VulkanHost::pipeline_cache_data";
        let parts = self.device_parts(device)?;
        let (owner, handle) = {
            let table = self.locked_caches();
            self.device_of(&table, cache.token(), "VkPipelineCache", METHOD)?
        };
        if owner != parts.index {
            return Err(cross_device("VkPipelineCache", owner, parts.index));
        }
        let get = parts.device.fp_v1_0().get_pipeline_cache_data;
        let _still = self.cache_gate.write().unwrap_or_else(PoisonError::into_inner);

        let mut size = 0usize;
        // SAFETY: the device and the cache are live and the cache is the device's own; `pData` is
        // NULL, so the driver writes the size into `size` and nothing else.
        let result = unsafe { get(parts.device.handle(), handle, &mut size, std::ptr::null_mut()) };
        if result.as_raw() < 0 {
            return Ok(DriverAnswer::Failed(result.as_raw()));
        }
        let mut bytes = vec![0u8; size];
        let mut written = size;
        // SAFETY: as above, and `bytes` is `size` writable bytes: the whole blob, which it stays
        // until this returns because the gate holds off every pipeline creation through a cache --
        // so the driver is never on its short-buffer path.
        let result =
            unsafe { get(parts.device.handle(), handle, &mut written, bytes.as_mut_ptr().cast()) };
        if result.as_raw() < 0 {
            return Ok(DriverAnswer::Failed(result.as_raw()));
        }
        if result != vk::Result::SUCCESS || written > size {
            return Err(refused(
                METHOD,
                &format!(
                    "the driver reported {size} bytes of cache and then answered {result:?} with \
                     a count of {written} for a buffer of exactly that size, while every \
                     pipeline creation through a cache was held off. Something grew the cache \
                     anyway, and a short buffer is the path on which this machine's driver was \
                     measured writing past the end of it -- so the bytes are not handed on"
                ),
            ));
        }
        bytes.truncate(written);
        Ok(DriverAnswer::Ok(bytes))
    }

    /// `vkCreateGraphicsPipelines`, rebuilt from the decoded request in four owned layers.
    ///
    /// # Why the layers
    ///
    /// `VkGraphicsPipelineCreateInfo` is a tree of pointers: a create info points at nine
    /// sub-states, the vertex-input state points at two arrays, a shader stage points at a name
    /// and a specialization info, and that points at two more. Every one of those has to be alive
    /// and unmoved when `vkCreateGraphicsPipelines` reads it, and `ash`'s builders take borrows —
    /// so the storage is built **bottom-up in four passes**, each fully populated before the next
    /// borrows from it. `create_device`'s `priorities` makes the same argument two levels
    /// shallower, and getting it wrong there would leave `pQueuePriorities` dangling.
    fn create_graphics_pipelines(
        &self,
        device: HostDevice,
        cache: Option<HostPipelineCache>,
        requests: &[GraphicsPipelineRequest],
    ) -> AbiResult<PipelinesCreated> {
        const METHOD: &str = "VulkanHost::create_graphics_pipelines";
        let parts = self.device_parts(device)?;
        let cache_handle = match cache {
            None => vk::PipelineCache::null(),
            Some(token) => {
                let table = self.locked_caches();
                let (owner, handle) =
                    self.device_of(&table, token.token(), "VkPipelineCache", METHOD)?;
                if owner != parts.index {
                    return Err(cross_device("VkPipelineCache", owner, parts.index));
                }
                handle
            }
        };

        // Layer 0: everything owned, one entry per request.
        let owned: Vec<OwnedPipeline> = requests
            .iter()
            .map(|request| self.own_pipeline(parts.index, request))
            .collect::<AbiResult<Vec<_>>>()?;

        // Layer 1: specialization infos, borrowing layer 0.
        let specializations: Vec<Vec<vk::SpecializationInfo<'_>>> = owned
            .iter()
            .map(|pipeline| {
                pipeline
                    .stages
                    .iter()
                    .map(|stage| {
                        vk::SpecializationInfo::default()
                            .map_entries(&stage.entries)
                            .data(&stage.data)
                    })
                    .collect()
            })
            .collect();

        // Layer 2: shader stages, borrowing layers 0 and 1.
        let stages: Vec<Vec<vk::PipelineShaderStageCreateInfo<'_>>> = owned
            .iter()
            .zip(specializations.iter())
            .map(|(pipeline, specializations)| {
                pipeline
                    .stages
                    .iter()
                    .zip(specializations.iter())
                    .map(|(stage, specialization)| {
                        let mut built = vk::PipelineShaderStageCreateInfo::default()
                            .flags(vk::PipelineShaderStageCreateFlags::from_raw(stage.flags))
                            .stage(vk::ShaderStageFlags::from_raw(stage.stage))
                            .module(stage.module)
                            .name(stage.name.as_c_str());
                        if stage.specialized {
                            built = built.specialization_info(specialization);
                        }
                        built
                    })
                    .collect()
            })
            .collect();

        // Layer 3: the nine sub-states, borrowing layer 0.
        let sub_states: Vec<SubStates<'_>> =
            owned.iter().map(SubStates::of).collect();

        // Layer 4: the create infos themselves.
        let infos: Vec<vk::GraphicsPipelineCreateInfo<'_>> = owned
            .iter()
            .zip(stages.iter())
            .zip(sub_states.iter())
            .map(|((pipeline, stages), sub)| pipeline.info(stages, sub))
            .collect();

        // Building through a cache grows it: held off while `vkGetPipelineCacheData` reads one.
        let _growing = self.cache_growth(cache);
        // SAFETY: the device is live; every handle in `infos` is one of its own, checked above;
        // every pointer reachable from `infos` is into `owned`, `specializations`, `stages` or
        // `sub_states`, all of which outlive this call and none of which are mutated after being
        // borrowed; `pAllocator` is `None`.
        let created = unsafe {
            parts.device.create_graphics_pipelines(cache_handle, &infos, None)
        };
        // **`ash` returns the handles *and* the failure**, which is exactly the partial-success
        // shape the specification requires and which `PipelinesCreated` exists to carry: the
        // pipelines that were created are still real and still have to be destroyed.
        let (handles, result) = match created {
            Ok(handles) => (handles, vk::Result::SUCCESS),
            Err((handles, result)) => (handles, result),
        };
        if handles.len() != requests.len() {
            return Err(refused(
                METHOD,
                &format!(
                    "the driver answered with {} handle slot(s) for {} create info structure(s)",
                    handles.len(),
                    requests.len()
                ),
            ));
        }
        let pipelines = handles
            .into_iter()
            .map(|handle| {
                if handle == vk::Pipeline::null() {
                    None
                } else {
                    let token = self
                        .locked_pipelines()
                        .insert(ObjectEntry { device: parts.index, object: handle });
                    Some(HostPipeline::from_token(token))
                }
            })
            .collect();
        Ok(PipelinesCreated { result: result.as_raw(), pipelines })
    }

    /// `vkCreateComputePipelines`, rebuilt from the decoded request in the graphics call's owned
    /// layers, of which a compute pipeline needs three: the owned stage, its specialization info,
    /// and the create info with the stage embedded in it.
    fn create_compute_pipelines(
        &self,
        device: HostDevice,
        cache: Option<HostPipelineCache>,
        requests: &[ComputePipelineRequest],
    ) -> AbiResult<PipelinesCreated> {
        const METHOD: &str = "VulkanHost::create_compute_pipelines";
        let parts = self.device_parts(device)?;
        let cache_handle = match cache {
            None => vk::PipelineCache::null(),
            Some(token) => {
                let table = self.locked_caches();
                let (owner, handle) =
                    self.device_of(&table, token.token(), "VkPipelineCache", METHOD)?;
                if owner != parts.index {
                    return Err(cross_device("VkPipelineCache", owner, parts.index));
                }
                handle
            }
        };

        // Layer 0: the stage, the layout and the base, owned, one entry per request. Each table is
        // locked on its own, as `own_pipeline` locks them.
        let owned: Vec<(OwnedStage, vk::PipelineLayout, vk::Pipeline)> = requests
            .iter()
            .map(|request| {
                let stage = {
                    let modules = self.locked_modules();
                    self.own_stage(&modules, parts.index, &request.stage, METHOD)?
                };
                let Some(layout_token) = request.layout else {
                    return Err(refused(METHOD, "the request names no pipeline layout"));
                };
                let layout = {
                    let table = self.locked_layouts();
                    let (owner, handle) =
                        self.device_of(&table, layout_token.token(), "VkPipelineLayout", METHOD)?;
                    if owner != parts.index {
                        return Err(cross_device("VkPipelineLayout", owner, parts.index));
                    }
                    handle
                };
                let base = match request.base_pipeline {
                    None => vk::Pipeline::null(),
                    Some(token) => {
                        let table = self.locked_pipelines();
                        let (owner, handle) =
                            self.device_of(&table, token.token(), "VkPipeline", METHOD)?;
                        if owner != parts.index {
                            return Err(cross_device("VkPipeline", owner, parts.index));
                        }
                        handle
                    }
                };
                Ok((stage, layout, base))
            })
            .collect::<AbiResult<Vec<_>>>()?;

        // Layer 1: specialization infos, borrowing layer 0.
        let specializations: Vec<vk::SpecializationInfo<'_>> = owned
            .iter()
            .map(|(stage, _, _)| {
                vk::SpecializationInfo::default().map_entries(&stage.entries).data(&stage.data)
            })
            .collect();

        // Layer 2: the create infos, each with its stage embedded, borrowing layers 0 and 1.
        let infos: Vec<vk::ComputePipelineCreateInfo<'_>> = owned
            .iter()
            .zip(specializations.iter())
            .zip(requests.iter())
            .map(|(((stage, layout, base), specialization), request)| {
                let mut built = vk::PipelineShaderStageCreateInfo::default()
                    .flags(vk::PipelineShaderStageCreateFlags::from_raw(stage.flags))
                    .stage(vk::ShaderStageFlags::from_raw(stage.stage))
                    .module(stage.module)
                    .name(stage.name.as_c_str());
                if stage.specialized {
                    built = built.specialization_info(specialization);
                }
                vk::ComputePipelineCreateInfo::default()
                    .flags(vk::PipelineCreateFlags::from_raw(request.flags))
                    .stage(built)
                    .layout(*layout)
                    .base_pipeline_handle(*base)
                    .base_pipeline_index(request.base_pipeline_index)
            })
            .collect();

        // Building through a cache grows it: held off while `vkGetPipelineCacheData` reads one.
        let _growing = self.cache_growth(cache);
        // SAFETY: the device is live; every handle in `infos` is one of its own, checked above;
        // every pointer reachable from `infos` is into `owned` or `specializations`, both of which
        // outlive this call and neither of which is mutated after being borrowed; `pAllocator` is
        // `None`.
        let created = unsafe { parts.device.create_compute_pipelines(cache_handle, &infos, None) };
        // The handles *and* the failure, as the graphics call's: a pipeline that was created is
        // still real and still has to be destroyed.
        let (handles, result) = match created {
            Ok(handles) => (handles, vk::Result::SUCCESS),
            Err((handles, result)) => (handles, result),
        };
        if handles.len() != requests.len() {
            return Err(refused(
                METHOD,
                &format!(
                    "the driver answered with {} handle slot(s) for {} create info structure(s)",
                    handles.len(),
                    requests.len()
                ),
            ));
        }
        let pipelines = handles
            .into_iter()
            .map(|handle| {
                if handle == vk::Pipeline::null() {
                    None
                } else {
                    let token = self
                        .locked_pipelines()
                        .insert(ObjectEntry { device: parts.index, object: handle });
                    Some(HostPipeline::from_token(token))
                }
            })
            .collect();
        Ok(PipelinesCreated { result: result.as_raw(), pipelines })
    }

    fn destroy_pipeline(&self, pipeline: HostPipeline) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_pipeline";
        let (device_index, handle) = {
            let table = self.locked_pipelines();
            self.device_of(&table, pipeline.token(), "VkPipeline", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the pipeline is live and nothing in flight still binds it.
        unsafe { device.destroy_pipeline(handle, None) };
        self.locked_pipelines().remove(pipeline.token());
        Ok(())
    }

    fn create_descriptor_set_layout(
        &self,
        device: HostDevice,
        request: &DescriptorSetLayoutRequest,
    ) -> AbiResult<DriverAnswer<HostDescriptorSetLayout>> {
        const METHOD: &str = "VulkanHost::create_descriptor_set_layout";
        let parts = self.device_parts(device)?;
        // The immutable samplers have to outlive the bindings that point at them.
        let samplers: Vec<Vec<vk::Sampler>> = {
            let table = self.locked_samplers();
            request
                .bindings
                .iter()
                .map(|binding| {
                    self.objects_of(
                        &table,
                        binding.immutable_samplers.iter().copied().map(HostSampler::token),
                        "VkSampler",
                        parts.index,
                        METHOD,
                    )
                })
                .collect::<AbiResult<Vec<_>>>()?
        };
        let bindings: Vec<vk::DescriptorSetLayoutBinding<'_>> = request
            .bindings
            .iter()
            .zip(samplers.iter())
            .map(|(binding, samplers)| {
                let mut built = vk::DescriptorSetLayoutBinding::default()
                    .binding(binding.binding)
                    .descriptor_type(vk::DescriptorType::from_raw(binding.descriptor_type as i32))
                    .stage_flags(vk::ShaderStageFlags::from_raw(binding.stage_flags));
                if samplers.is_empty() {
                    // `descriptor_count` and `immutable_samplers` are the same field in `ash`'s
                    // builder: the second sets the count from the slice. With no samplers the
                    // count is the guest's own.
                    built = built.descriptor_count(binding.descriptor_count);
                } else {
                    built = built.immutable_samplers(samplers);
                }
                built
            })
            .collect();
        let info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::from_raw(request.flags))
            .bindings(&bindings);
        // SAFETY: the device is live and every pointer reachable from `info` is into a local that
        // outlives the call.
        match unsafe { parts.device.create_descriptor_set_layout(&info, None) } {
            Ok(layout) => {
                let token = self
                    .locked_set_layouts()
                    .insert(ObjectEntry { device: parts.index, object: layout });
                Ok(DriverAnswer::Ok(HostDescriptorSetLayout::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_descriptor_set_layout(&self, layout: HostDescriptorSetLayout) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_descriptor_set_layout";
        let (device_index, handle) = {
            let table = self.locked_set_layouts();
            self.device_of(&table, layout.token(), "VkDescriptorSetLayout", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the layout is live; sets allocated from it stay valid, which the specification
        // states explicitly.
        unsafe { device.destroy_descriptor_set_layout(handle, None) };
        self.locked_set_layouts().remove(layout.token());
        Ok(())
    }

    fn create_descriptor_pool(
        &self,
        device: HostDevice,
        request: &DescriptorPoolRequest,
    ) -> AbiResult<DriverAnswer<HostDescriptorPool>> {
        let parts = self.device_parts(device)?;
        let sizes: Vec<vk::DescriptorPoolSize> = request
            .sizes
            .iter()
            .map(|(kind, count)| vk::DescriptorPoolSize {
                ty: vk::DescriptorType::from_raw(*kind as i32),
                descriptor_count: *count,
            })
            .collect();
        let info = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::from_raw(request.flags))
            .max_sets(request.max_sets)
            .pool_sizes(&sizes);
        // SAFETY: the device is live and `info` and the slice it borrows outlive the call.
        match unsafe { parts.device.create_descriptor_pool(&info, None) } {
            Ok(pool) => {
                let token = self
                    .locked_descriptor_pools()
                    .insert(ObjectEntry { device: parts.index, object: pool });
                Ok(DriverAnswer::Ok(HostDescriptorPool::from_token(token)))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn destroy_descriptor_pool(&self, pool: HostDescriptorPool) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::destroy_descriptor_pool";
        let (device_index, handle) = {
            let table = self.locked_descriptor_pools();
            self.device_of(&table, pool.token(), "VkDescriptorPool", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the pool is live, and destroying it frees every set allocated from it -- which
        // is why those entries are dropped below rather than freed one by one.
        unsafe { device.destroy_descriptor_pool(handle, None) };
        self.locked_descriptor_pools().remove(pool.token());
        self.drop_sets_of(pool.token());
        Ok(())
    }

    fn descriptor_sets_of(&self, pool: HostDescriptorPool) -> AbiResult<Vec<HostDescriptorSet>> {
        let table = self.locked_descriptor_sets();
        Ok(table
            .iter()
            .filter(|(_, entry)| entry.pool == pool.token())
            .map(|(token, _)| HostDescriptorSet::from_token(token))
            .collect())
    }

    fn allocate_descriptor_sets(
        &self,
        pool: HostDescriptorPool,
        layouts: &[HostDescriptorSetLayout],
    ) -> AbiResult<DriverAnswer<Vec<HostDescriptorSet>>> {
        const METHOD: &str = "VulkanHost::allocate_descriptor_sets";
        let (device_index, pool_handle) = {
            let table = self.locked_descriptor_pools();
            self.device_of(&table, pool.token(), "VkDescriptorPool", METHOD)?
        };
        let handles = {
            let table = self.locked_set_layouts();
            self.objects_of(
                &table,
                layouts.iter().copied().map(HostDescriptorSetLayout::token),
                "VkDescriptorSetLayout",
                device_index,
                METHOD,
            )?
        };
        let device = self.device_at(device_index, METHOD)?;
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool_handle)
            .set_layouts(&handles);
        // SAFETY: the device is live, the pool and every layout are its own, and `info` and the
        // slice it borrows outlive the call.
        match unsafe { device.allocate_descriptor_sets(&info) } {
            Ok(sets) => {
                let mut table = self.locked_descriptor_sets();
                let tokens = sets
                    .into_iter()
                    .map(|set| {
                        HostDescriptorSet::from_token(table.insert(DescriptorSetEntry {
                            device: device_index,
                            pool: pool.token(),
                            set,
                        }))
                    })
                    .collect();
                Ok(DriverAnswer::Ok(tokens))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn free_descriptor_sets(
        &self,
        pool: HostDescriptorPool,
        sets: &[HostDescriptorSet],
    ) -> AbiResult<DriverAnswer<()>> {
        const METHOD: &str = "VulkanHost::free_descriptor_sets";
        let (device_index, pool_handle) = {
            let table = self.locked_descriptor_pools();
            self.device_of(&table, pool.token(), "VkDescriptorPool", METHOD)?
        };
        let handles = {
            let table = self.locked_descriptor_sets();
            sets.iter()
                .map(|token| {
                    let entry = table.get(token.token()).ok_or_else(|| {
                        refused(
                            METHOD,
                            &format!(
                                "{token:?} is not a descriptor set this host allocated -- it \
                                 holds {}",
                                table.len()
                            ),
                        )
                    })?;
                    // **The check the specification leaves to validation**: freeing a set with a
                    // pool it was not allocated from is undefined behaviour, and nothing on this
                    // machine would report it.
                    if entry.pool != pool.token() {
                        return Err(refused(
                            METHOD,
                            &format!(
                                "{token:?} was allocated from pool #{} and is being freed with \
                                 pool #{}. Both handles are real; the pairing is what is wrong",
                                entry.pool,
                                pool.token()
                            ),
                        ));
                    }
                    Ok(entry.set)
                })
                .collect::<AbiResult<Vec<_>>>()?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the device and pool are live, every set was allocated from that pool (checked
        // above), and the pool was created with `FREE_DESCRIPTOR_SET` -- which the driver itself
        // enforces and reports.
        let result = unsafe { device.free_descriptor_sets(pool_handle, &handles) };
        match result {
            Ok(()) => {
                let mut table = self.locked_descriptor_sets();
                for token in sets {
                    table.remove(token.token());
                }
                Ok(DriverAnswer::Ok(()))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn reset_descriptor_pool(
        &self,
        pool: HostDescriptorPool,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        const METHOD: &str = "VulkanHost::reset_descriptor_pool";
        let (device_index, handle) = {
            let table = self.locked_descriptor_pools();
            self.device_of(&table, pool.token(), "VkDescriptorPool", METHOD)?
        };
        let device = self.device_at(device_index, METHOD)?;
        // SAFETY: the pool is live and no set allocated from it is still in use by a command
        // buffer the GPU is executing -- the guest's responsibility, which the specification
        // states.
        match unsafe {
            device.reset_descriptor_pool(handle, vk::DescriptorPoolResetFlags::from_raw(flags))
        } {
            Ok(()) => {
                self.drop_sets_of(pool.token());
                Ok(DriverAnswer::Ok(()))
            }
            Err(result) => Ok(DriverAnswer::Failed(result.as_raw())),
        }
    }

    fn update_descriptor_sets(
        &self,
        device: HostDevice,
        writes: &[DescriptorWrite],
        copies: &[DescriptorCopy],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::update_descriptor_sets";
        let parts = self.device_parts(device)?;

        // The `VkDescriptorImageInfo`/`VkDescriptorBufferInfo` arrays have to outlive the writes
        // that point at them, so they are built first.
        let payloads: Vec<WritePayload> = writes
            .iter()
            .map(|write| self.write_payload(parts.index, write, METHOD))
            .collect::<AbiResult<Vec<_>>>()?;
        let built_writes: Vec<vk::WriteDescriptorSet<'_>> = writes
            .iter()
            .zip(payloads.iter())
            .map(|(write, payload)| {
                let built = vk::WriteDescriptorSet::default()
                    .dst_set(payload.set)
                    .dst_binding(write.binding)
                    .dst_array_element(write.array_element)
                    .descriptor_type(vk::DescriptorType::from_raw(write.descriptor_type as i32));
                // **Which builder is called is decided by the decoded variant**, which was
                // decided by `descriptorType` one layer up. `ash` sets `descriptorCount` from the
                // slice, so the count and the array cannot disagree.
                match &payload.data {
                    PayloadData::Images(images) => built.image_info(images),
                    PayloadData::Buffers(buffers) => built.buffer_info(buffers),
                }
            })
            .collect();

        let built_copies: Vec<vk::CopyDescriptorSet<'_>> = copies
            .iter()
            .map(|copy| {
                let table = self.locked_descriptor_sets();
                let source = Self::set_handle(&table, copy.source, parts.index, METHOD)?;
                let destination =
                    Self::set_handle(&table, copy.destination, parts.index, METHOD)?;
                Ok(vk::CopyDescriptorSet::default()
                    .src_set(source)
                    .src_binding(copy.source_binding)
                    .src_array_element(copy.source_element)
                    .dst_set(destination)
                    .dst_binding(copy.destination_binding)
                    .dst_array_element(copy.destination_element)
                    .descriptor_count(copy.count))
            })
            .collect::<AbiResult<Vec<_>>>()?;

        // SAFETY: the device is live, every handle in both lists is one of its own, and every
        // pointer reachable from them is into `payloads` or a local that outlives the call.
        unsafe { parts.device.update_descriptor_sets(&built_writes, &built_copies) };
        Ok(())
    }

    fn cmd_begin_render_pass(
        &self,
        buffer: HostCommandBuffer,
        begin: &RenderPassBegin,
        contents: u32,
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_begin_render_pass";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let Some(pass_token) = begin.render_pass else {
            return Err(refused(METHOD, "the request names no render pass"));
        };
        let Some(framebuffer_token) = begin.framebuffer else {
            return Err(refused(METHOD, "the request names no framebuffer"));
        };
        let pass = {
            let table = self.locked_passes();
            self.device_of(&table, pass_token.token(), "VkRenderPass", METHOD)?.1
        };
        let framebuffer = {
            let table = self.locked_framebuffers();
            self.device_of(&table, framebuffer_token.token(), "VkFramebuffer", METHOD)?.1
        };
        let area = rect_from_bytes(METHOD, &begin.render_area)?;
        // **The union, reassembled from the guest's own bytes**, for `cmd_clear_color_image`'s
        // reason: which member is live is decided by each attachment's format, which this layer
        // does not know, and the three members alias the same sixteen bytes.
        let clears: Vec<vk::ClearValue> =
            begin.clear_values.iter().map(clear_value_from_bytes).collect();

        let info = vk::RenderPassBeginInfo::default()
            .render_pass(pass)
            .framebuffer(framebuffer)
            .render_area(area)
            .clear_values(&clears);
        // SAFETY: the command buffer is live and recording, every handle is one of its device's,
        // and `info` and the slice it borrows outlive the call.
        unsafe {
            device.cmd_begin_render_pass(
                handle,
                &info,
                vk::SubpassContents::from_raw(contents as i32),
            );
        }
        Ok(())
    }

    fn cmd_end_render_pass(&self, buffer: HostCommandBuffer) -> AbiResult<()> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::cmd_end_render_pass")?;
        // SAFETY: the command buffer is live and inside a render pass, which the driver enforces.
        unsafe { device.cmd_end_render_pass(handle) };
        Ok(())
    }

    fn cmd_bind_pipeline(
        &self,
        buffer: HostCommandBuffer,
        bind_point: u32,
        pipeline: HostPipeline,
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_bind_pipeline";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let target = {
            let table = self.locked_pipelines();
            self.device_of(&table, pipeline.token(), "VkPipeline", METHOD)?.1
        };
        // SAFETY: the command buffer is live and recording, and the pipeline is live.
        unsafe {
            device.cmd_bind_pipeline(
                handle,
                vk::PipelineBindPoint::from_raw(bind_point as i32),
                target,
            );
        }
        Ok(())
    }

    fn cmd_bind_vertex_buffers(
        &self,
        buffer: HostCommandBuffer,
        first_binding: u32,
        buffers: &[(HostBuffer, u64)],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_bind_vertex_buffers";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let handles = {
            let table = self.locked_vk_buffers();
            buffers
                .iter()
                .map(|(token, _)| {
                    self.device_of(&table, token.token(), "VkBuffer", METHOD).map(|(_, h)| h)
                })
                .collect::<AbiResult<Vec<_>>>()?
        };
        let offsets: Vec<u64> = buffers.iter().map(|(_, offset)| *offset).collect();
        // SAFETY: the command buffer is live and recording, every buffer is live, and the two
        // slices have the same length by construction -- they were decoded as pairs.
        unsafe { device.cmd_bind_vertex_buffers(handle, first_binding, &handles, &offsets) };
        Ok(())
    }

    fn cmd_bind_index_buffer(
        &self,
        buffer: HostCommandBuffer,
        index_buffer: HostBuffer,
        offset: u64,
        index_type: u32,
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_bind_index_buffer";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let target = {
            let table = self.locked_vk_buffers();
            self.device_of(&table, index_buffer.token(), "VkBuffer", METHOD)?.1
        };
        // SAFETY: the command buffer is live and recording and the buffer is live.
        unsafe {
            device.cmd_bind_index_buffer(
                handle,
                target,
                offset,
                vk::IndexType::from_raw(index_type as i32),
            );
        }
        Ok(())
    }

    fn cmd_bind_descriptor_sets(
        &self,
        buffer: HostCommandBuffer,
        bind_point: u32,
        layout: HostPipelineLayout,
        first_set: u32,
        sets: &[HostDescriptorSet],
        dynamic_offsets: &[u32],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_bind_descriptor_sets";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let layout_handle = {
            let table = self.locked_layouts();
            self.device_of(&table, layout.token(), "VkPipelineLayout", METHOD)?.1
        };
        let set_handles = {
            let table = self.locked_descriptor_sets();
            sets.iter()
                .map(|token| {
                    table.get(token.token()).map(|entry| entry.set).ok_or_else(|| {
                        refused(
                            METHOD,
                            &format!(
                                "{token:?} is not a descriptor set this host holds -- it holds \
                                 {}. A set freed with its pool lands here",
                                table.len()
                            ),
                        )
                    })
                })
                .collect::<AbiResult<Vec<_>>>()?
        };
        // SAFETY: the command buffer is live and recording, and every handle is live.
        unsafe {
            device.cmd_bind_descriptor_sets(
                handle,
                vk::PipelineBindPoint::from_raw(bind_point as i32),
                layout_handle,
                first_set,
                &set_handles,
                dynamic_offsets,
            );
        }
        Ok(())
    }

    fn cmd_set_viewport(
        &self,
        buffer: HostCommandBuffer,
        first: u32,
        viewports: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_set_viewport";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let built = viewports_from_bytes(METHOD, viewports)?;
        // SAFETY: the command buffer is live and recording, and `built` outlives the call.
        unsafe { device.cmd_set_viewport(handle, first, &built) };
        Ok(())
    }

    fn cmd_set_scissor(
        &self,
        buffer: HostCommandBuffer,
        first: u32,
        scissors: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_set_scissor";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let built = rects_from_bytes(METHOD, scissors)?;
        // SAFETY: as `cmd_set_viewport`.
        unsafe { device.cmd_set_scissor(handle, first, &built) };
        Ok(())
    }

    fn cmd_draw(
        &self,
        buffer: HostCommandBuffer,
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    ) -> AbiResult<()> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::cmd_draw")?;
        // SAFETY: the command buffer is live, recording, and inside a render pass with a graphics
        // pipeline bound -- all of which the driver enforces and reports.
        unsafe {
            device.cmd_draw(handle, vertex_count, instance_count, first_vertex, first_instance);
        }
        Ok(())
    }

    fn cmd_dispatch(&self, buffer: HostCommandBuffer, x: u32, y: u32, z: u32) -> AbiResult<()> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::cmd_dispatch")?;
        // SAFETY: the command buffer is live and recording, outside a render pass, with a compute
        // pipeline bound -- which the driver enforces and reports.
        unsafe { device.cmd_dispatch(handle, x, y, z) };
        Ok(())
    }

    fn cmd_draw_indexed(
        &self,
        buffer: HostCommandBuffer,
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        vertex_offset: i32,
        first_instance: u32,
    ) -> AbiResult<()> {
        let (device, handle) = self.command_parts(buffer, "VulkanHost::cmd_draw_indexed")?;
        // SAFETY: as `cmd_draw`, with an index buffer bound.
        unsafe {
            device.cmd_draw_indexed(
                handle,
                index_count,
                instance_count,
                first_index,
                vertex_offset,
                first_instance,
            );
        }
        Ok(())
    }

    fn cmd_copy_buffer(
        &self,
        buffer: HostCommandBuffer,
        source: HostBuffer,
        destination: HostBuffer,
        regions: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_copy_buffer";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let (source_handle, destination_handle) = {
            let table = self.locked_vk_buffers();
            (
                self.device_of(&table, source.token(), "VkBuffer", METHOD)?.1,
                self.device_of(&table, destination.token(), "VkBuffer", METHOD)?.1,
            )
        };
        let built = buffer_copies_from_bytes(METHOD, regions)?;
        // SAFETY: the command buffer is live and recording, both buffers are live, and `built`
        // outlives the call.
        unsafe { device.cmd_copy_buffer(handle, source_handle, destination_handle, &built) };
        Ok(())
    }

    fn cmd_copy_buffer_to_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostBuffer,
        image: HostImageRef,
        layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_copy_buffer_to_image";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let source_handle = {
            let table = self.locked_vk_buffers();
            self.device_of(&table, source.token(), "VkBuffer", METHOD)?.1
        };
        let destination = self.image_handle(image, "vkCmdCopyBufferToImage")?;
        let built = buffer_image_copies_from_bytes(METHOD, regions)?;
        // SAFETY: the command buffer is live and recording, the buffer and image are live, and
        // the image is in the layout the guest named -- which is its responsibility and which the
        // driver reports if the barrier before it was wrong.
        unsafe {
            device.cmd_copy_buffer_to_image(
                handle,
                source_handle,
                destination,
                vk::ImageLayout::from_raw(layout as i32),
                &built,
            );
        }
        Ok(())
    }

    fn cmd_copy_image_to_buffer(
        &self,
        buffer: HostCommandBuffer,
        image: HostImageRef,
        layout: u32,
        destination: HostBuffer,
        regions: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_copy_image_to_buffer";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let source = self.image_handle(image, "vkCmdCopyImageToBuffer")?;
        let destination_handle = {
            let table = self.locked_vk_buffers();
            self.device_of(&table, destination.token(), "VkBuffer", METHOD)?.1
        };
        let built = buffer_image_copies_from_bytes(METHOD, regions)?;
        // SAFETY: the command buffer is live and recording, the image and buffer are live, and
        // the image is in the layout the guest named -- which is its responsibility and which the
        // driver reports if the barrier before it was wrong.
        unsafe {
            device.cmd_copy_image_to_buffer(
                handle,
                source,
                vk::ImageLayout::from_raw(layout as i32),
                destination_handle,
                &built,
            );
        }
        Ok(())
    }

    fn cmd_copy_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_copy_image";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let source_handle = self.image_handle(source, "vkCmdCopyImage")?;
        let destination_handle = self.image_handle(destination, "vkCmdCopyImage")?;
        let built = flat_list(METHOD, regions, "VkImageCopy", image_copy_from_bytes)?;
        // SAFETY: the command buffer is live and recording, both images are live, and each is in
        // the layout the guest named -- its responsibility, which the driver reports if a barrier
        // before it was wrong.
        unsafe {
            device.cmd_copy_image(
                handle,
                source_handle,
                vk::ImageLayout::from_raw(source_layout as i32),
                destination_handle,
                vk::ImageLayout::from_raw(destination_layout as i32),
                &built,
            );
        }
        Ok(())
    }

    fn cmd_resolve_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_resolve_image";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let source_handle = self.image_handle(source, "vkCmdResolveImage")?;
        let destination_handle = self.image_handle(destination, "vkCmdResolveImage")?;
        let built = flat_list(METHOD, regions, "VkImageResolve", image_resolve_from_bytes)?;
        // SAFETY: the command buffer is live and recording, both images are live and in the
        // layouts the guest named, and the regions are the guest's own -- the driver reports a
        // source that is not multisampled or formats that differ.
        unsafe {
            device.cmd_resolve_image(
                handle,
                source_handle,
                vk::ImageLayout::from_raw(source_layout as i32),
                destination_handle,
                vk::ImageLayout::from_raw(destination_layout as i32),
                &built,
            );
        }
        Ok(())
    }

    fn cmd_blit_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
        filter: u32,
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_blit_image";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let source_handle = self.image_handle(source, "vkCmdBlitImage")?;
        let destination_handle = self.image_handle(destination, "vkCmdBlitImage")?;
        let built = flat_list(METHOD, regions, "VkImageBlit", image_blit_from_bytes)?;
        // SAFETY: the command buffer is live and recording, both images are live and in the
        // layouts the guest named, and the regions are the guest's own -- the driver reports a
        // format that cannot be blitted with this filter.
        unsafe {
            device.cmd_blit_image(
                handle,
                source_handle,
                vk::ImageLayout::from_raw(source_layout as i32),
                destination_handle,
                vk::ImageLayout::from_raw(destination_layout as i32),
                &built,
                vk::Filter::from_raw(filter as i32),
            );
        }
        Ok(())
    }

    fn cmd_push_constants(
        &self,
        buffer: HostCommandBuffer,
        layout: HostPipelineLayout,
        stage_flags: u32,
        offset: u32,
        values: &[u8],
    ) -> AbiResult<()> {
        const METHOD: &str = "VulkanHost::cmd_push_constants";
        let (device, handle) = self.command_parts(buffer, METHOD)?;
        let layout_handle = {
            let table = self.locked_layouts();
            self.device_of(&table, layout.token(), "VkPipelineLayout", METHOD)?.1
        };
        // SAFETY: the command buffer is live and recording, the layout is live, and `values`
        // outlives the call.
        unsafe {
            device.cmd_push_constants(
                handle,
                layout_handle,
                vk::ShaderStageFlags::from_raw(stage_flags),
                offset,
                values,
            );
        }
        Ok(())
    }
}

impl GfxVulkanHost {
    /// The `VkImage` behind an [`HostImageRef`], or a refusal naming the call.
    fn image_handle(&self, image: HostImageRef, call: &str) -> AbiResult<vk::Image> {
        match image {
            HostImageRef::Swapchain(token) => {
                let table = self.locked_images();
                table.get(token.token()).map(|entry| entry.image).ok_or_else(|| {
                    refused(
                        call,
                        &format!(
                            "{token:?} is not a swapchain image this host holds -- it holds {}. \
                             An image whose swapchain has been destroyed lands here, which is the \
                             case that matters: a swapchain image's lifetime is its swapchain's",
                            table.len()
                        ),
                    )
                })
            }
            HostImageRef::Created(token) => {
                let table = self.locked_created_images();
                self.device_of(&table, token.token(), "VkImage", "GfxVulkanHost::image_handle")
                    .map(|(_, image)| image)
            }
            HostImageRef::None => Err(refused(
                call,
                "the request names no image at all. `VK_NULL_HANDLE` where a `VkImage` is \
                 required is refused by the shim before a host sees it, so a `None` here means \
                 the shim and this host disagree about what a decoded request contains",
            )),
        }
    }

    /// The device and handle of one command buffer.
    fn command_parts(
        &self,
        buffer: HostCommandBuffer,
        method: &'static str,
    ) -> AbiResult<(ash::Device, vk::CommandBuffer)> {
        let (device_index, handle) = {
            let table = self.locked_buffers();
            let entry = table.get(buffer.token()).ok_or_else(|| {
                refused(
                    method,
                    &format!(
                        "{buffer:?} is not a command buffer this host allocated -- it holds {}. A \
                         buffer freed by `vkFreeCommandBuffers`, or by the destruction of its \
                         pool, lands here",
                        table.len()
                    ),
                )
            })?;
            (entry.device, entry.buffer)
        };
        Ok((self.device_at(device_index, method)?, handle))
    }

    /// The `VkFence` handles a list of tokens names, checked against one device.
    fn fence_handles(
        &self,
        fences: &[HostFence],
        device_index: usize,
        method: &'static str,
    ) -> AbiResult<Vec<vk::Fence>> {
        let table = self.locked_fences();
        fences
            .iter()
            .map(|token| {
                let (owner, handle) = self.device_of(&table, token.token(), "VkFence", method)?;
                if owner != device_index {
                    return Err(cross_device("VkFence", owner, device_index));
                }
                Ok(handle)
            })
            .collect()
    }

    // ------------------------------------------------------------------- stage 5 internals

    fn locked_memories(&self) -> std::sync::MutexGuard<'_, Slab<MemoryEntry>> {
        self.device_memories.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_vk_buffers(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Buffer>>> {
        self.buffers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_created_images(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Image>>> {
        self.created_images.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_samplers(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Sampler>>> {
        self.samplers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_modules(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::ShaderModule>>> {
        self.shader_modules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_layouts(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::PipelineLayout>>> {
        self.pipeline_layouts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_passes(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::RenderPass>>> {
        self.render_passes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_framebuffers(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Framebuffer>>> {
        self.framebuffers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_pipelines(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::Pipeline>>> {
        self.pipelines.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_caches(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::PipelineCache>>> {
        self.pipeline_caches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// [`GfxVulkanHost::cache_gate`]'s shared side, for a pipeline creation that names a cache,
    /// and nothing for one that does not. Held for the length of the driver call.
    fn cache_growth(&self, cache: Option<HostPipelineCache>) -> Option<RwLockReadGuard<'_, ()>> {
        cache.map(|_| self.cache_gate.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn locked_query_pools(&self) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::QueryPool>>> {
        self.query_pools.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_set_layouts(
        &self,
    ) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::DescriptorSetLayout>>> {
        self.descriptor_set_layouts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_descriptor_pools(
        &self,
    ) -> std::sync::MutexGuard<'_, Slab<ObjectEntry<vk::DescriptorPool>>> {
        self.descriptor_pools.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_descriptor_sets(&self) -> std::sync::MutexGuard<'_, Slab<DescriptorSetEntry>> {
        self.descriptor_sets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The device index and `VkDeviceMemory` a token names.
    fn memory_of(
        &self,
        memory: HostDeviceMemory,
        method: &'static str,
    ) -> AbiResult<(usize, vk::DeviceMemory, bool)> {
        let table = self.locked_memories();
        table.get(memory.token()).map(|entry| (entry.device, entry.memory, entry.imported)).ok_or_else(
            || {
                refused(
                    method,
                    &format!(
                        "{memory:?} is not an allocation this host made -- it holds {}. An \
                         allocation already freed by `vkFreeMemory` lands here, which is the case \
                         that matters: the guest's pages were unmapped with it",
                        table.len()
                    ),
                )
            },
        )
    }

    /// Resolve a list of tokens out of one of the plain object tables, checking the device.
    fn objects_of<T: Copy>(
        &self,
        table: &Slab<ObjectEntry<T>>,
        tokens: impl IntoIterator<Item = u64>,
        family: &'static str,
        device_index: usize,
        method: &'static str,
    ) -> AbiResult<Vec<T>> {
        tokens
            .into_iter()
            .map(|token| {
                let (owner, object) = self.device_of(table, token, family, method)?;
                if owner != device_index {
                    return Err(cross_device(family, owner, device_index));
                }
                Ok(object)
            })
            .collect()
    }

    /// **What `VK_EXT_external_memory_host` can do on one physical device, measured once.**
    ///
    /// # Why this costs a throwaway `VkDevice`, and why there is no cheaper honest answer
    ///
    /// The question `omni-android` needs answered at `vkGetPhysicalDeviceMemoryProperties` is
    /// *which memory types a host pointer can be imported into*, because that is what decides
    /// which types it can let the guest map. The only route the specification gives to that fact
    /// is `vkGetMemoryHostPointerPropertiesEXT`, which is a **device**-level call — and
    /// `vkGetPhysicalDeviceMemoryProperties` may be made before the guest has created a device.
    ///
    /// The tempting alternative is a rule: "host-visible and not device-local". On this machine
    /// that reproduces the measured `0xc` exactly, which is precisely why it is dangerous — it is
    /// a *derivation* that happens to agree with one measurement, and the first driver it
    /// disagreed with would silently mis-mask the table an engine chooses its uploads from. This
    /// project does not make plausible claims about drivers.
    ///
    /// So a device is created, asked, and destroyed. It happens **once per physical device** for
    /// the life of the host, it enables nothing but `VK_EXT_external_memory_host`, and it asks for
    /// one queue of family 0 because `vkCreateDevice` requires at least one queue and any family
    /// index is legal for a device that submits nothing.
    ///
    /// A physical device with no `VK_EXT_external_memory_host` answers `memory_type_bits = 0`,
    /// which masks every host-visible type out of the list the guest sees and makes every
    /// `vkMapMemory` a refusal naming the type. That is a real limitation of such a host and is
    /// reported rather than worked around.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the extension is present and the probe itself fails — which is a
    /// fact about this host and must not be flattened into an answer of zero.
    fn probe_importable(
        &self,
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> AbiResult<(u32, u64)> {
        const METHOD: &str = "GfxVulkanHost::probe_importable";
        let raw = vk::Handle::as_raw(physical);
        if let Some(probe) =
            self.importable.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|probe| probe.physical == raw)
        {
            return Ok((probe.memory_type_bits, probe.alignment));
        }

        // SAFETY: `physical` is a live physical device of `instance`, which this host created.
        let available = unsafe { instance.enumerate_device_extension_properties(physical) }
            .map_err(|result| {
                refused(
                    METHOD,
                    &format!(
                        "`vkEnumerateDeviceExtensionProperties` failed with {result:?}, so this \
                         host cannot say whether it is able to back host-visible memory at all"
                    ),
                )
            })?;
        let has_extension = available.iter().any(|entry| {
            entry.extension_name_as_c_str().is_ok_and(|name| name == vk::EXT_EXTERNAL_MEMORY_HOST_NAME)
        });
        if !has_extension {
            let probe = ImportableProbe {
                physical: raw,
                memory_type_bits: 0,
                alignment: CONSERVATIVE_IMPORT_ALIGNMENT,
            };
            let answer = (probe.memory_type_bits, probe.alignment);
            self.importable.lock().unwrap_or_else(PoisonError::into_inner).push(probe);
            return Ok(answer);
        }

        let alignment = self.imported_pointer_alignment(instance, physical);

        // The throwaway device. One queue, one extension, nothing else.
        let priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(0)
            .queue_priorities(&priorities);
        let queue_infos = [queue_info];
        let extension = vk::EXT_EXTERNAL_MEMORY_HOST_NAME.as_ptr();
        // A portability implementation's probe device must enable its subset too (the spec's
        // requirement for every device of one); with none of its optional features, which this
        // throwaway device never uses.
        let has_subset = available.iter().any(|entry| {
            entry.extension_name_as_c_str().is_ok_and(|name| name == crate::portability::SUBSET)
        });
        let mut extensions = vec![extension];
        if has_subset {
            extensions.push(crate::portability::SUBSET.as_ptr());
        }
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&extensions);
        // SAFETY: `physical` is live, every pointer reachable from `info` is into a local that
        // outlives the call, and `pAllocator` is `None`.
        let device = unsafe { instance.create_device(physical, &info, None) }.map_err(|result| {
            refused(
                METHOD,
                &format!(
                    "creating the one-queue probe device that `vkGetMemoryHostPointerPropertiesEXT` \
                     needs failed with {result:?}. `VK_EXT_external_memory_host` is present on \
                     this physical device, so this is a host failure and not an answer of zero -- \
                     reporting zero would mask every host-visible memory type out of the guest's \
                     list for a reason that is not true of the device"
                ),
            )
        })?;

        let bits = self.ask_host_pointer_properties(instance, &device, alignment);
        // SAFETY: the probe device submitted nothing, holds nothing and is about to be dropped.
        unsafe { device.destroy_device(None) };
        let memory_type_bits = bits?;

        let probe = ImportableProbe { physical: raw, memory_type_bits, alignment };
        let answer = (probe.memory_type_bits, probe.alignment);
        self.importable.lock().unwrap_or_else(PoisonError::into_inner).push(probe);
        Ok(answer)
    }

    /// `minImportedHostPointerAlignment`, or a conservative over-alignment when it cannot be asked.
    ///
    /// # Why a fallback rather than a refusal
    ///
    /// The property lives in a `VkPhysicalDeviceExternalMemoryHostPropertiesEXT` chained onto
    /// `vkGetPhysicalDeviceProperties2`, which is a **Vulkan 1.1** entry point — and the instance
    /// this is asked through is the *guest's*, created from the guest's own
    /// `VkInstanceCreateInfo`, which may well name `apiVersion` 1.0. This host does not get to
    /// change that.
    ///
    /// So the entry point is resolved by name, under both its core and its `KHR` spelling, and if
    /// neither is there the answer is [`CONSERVATIVE_IMPORT_ALIGNMENT`]. That is **safe rather
    /// than guessed**: an over-alignment satisfies any smaller requirement, because every
    /// alignment Vulkan reports is a power of two and 64 KiB is a multiple of all of them up to
    /// itself. What it costs is rounding — an allocation of one page becomes sixteen — which is
    /// visible in `Vulkan::imported_bytes()` rather than silent.
    fn imported_pointer_alignment(
        &self,
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
    ) -> u64 {
        let core = c"vkGetPhysicalDeviceProperties2";
        let khr = c"vkGetPhysicalDeviceProperties2KHR";
        // SAFETY: `instance` is live and both names are NUL-terminated literals. A name the
        // loader does not have answers null, which is the case this handles.
        let found = unsafe {
            self.entry
                .get_instance_proc_addr(instance.handle(), core.as_ptr())
                .or_else(|| self.entry.get_instance_proc_addr(instance.handle(), khr.as_ptr()))
        };
        let Some(function) = found else { return CONSERVATIVE_IMPORT_ALIGNMENT };
        // SAFETY: `vkGetPhysicalDeviceProperties2` and its `KHR` alias have this signature by
        // definition; the loader answered non-null for one of those two names and nothing else.
        let get_properties2: vk::PFN_vkGetPhysicalDeviceProperties2 =
            unsafe { std::mem::transmute(function) };

        let mut host = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut host);
        // SAFETY: `physical` is a live device of `instance`, and `properties` is a correctly
        // chained, locally owned structure that outlives the call.
        unsafe { get_properties2(physical, &mut properties) };
        let reported = host.min_imported_host_pointer_alignment;
        // A driver that reported zero, or something that is not a power of two, is one this code
        // cannot align to — so the conservative value stands rather than being multiplied by it.
        if reported == 0 || !reported.is_power_of_two() {
            CONSERVATIVE_IMPORT_ALIGNMENT
        } else {
            reported
        }
    }

    /// Ask one device which memory types an ordinary host allocation can be imported into.
    ///
    /// The pointer is a real, aligned, committed host allocation of exactly `alignment` bytes,
    /// because that is what the call is defined over — `VkMemoryHostPointerPropertiesEXT`'s answer
    /// is about the memory the pointer names, not about the pointer's value.
    fn ask_host_pointer_properties(
        &self,
        instance: &ash::Instance,
        device: &ash::Device,
        alignment: u64,
    ) -> AbiResult<u32> {
        const METHOD: &str = "GfxVulkanHost::probe_importable";
        let name = c"vkGetMemoryHostPointerPropertiesEXT";
        // SAFETY: `device` is live and was created with `VK_EXT_external_memory_host` enabled, and
        // the name is a NUL-terminated literal.
        let found = unsafe { instance.get_device_proc_addr(device.handle(), name.as_ptr()) };
        let Some(function) = found else {
            return Err(refused(
                METHOD,
                "this device enabled `VK_EXT_external_memory_host` and its loader answered NULL \
                 for `vkGetMemoryHostPointerPropertiesEXT`, which is the one command that \
                 extension exists to provide. Answering zero would mask every host-visible memory \
                 type out of the guest's list for a reason that is not true of the device",
            ));
        };
        // SAFETY: the loader answered non-null for exactly this name, whose signature the
        // extension defines.
        let get_properties: vk::PFN_vkGetMemoryHostPointerPropertiesEXT =
            unsafe { std::mem::transmute(function) };

        let size = usize::try_from(alignment).unwrap_or(4096).max(1);
        let layout = std::alloc::Layout::from_size_align(size, size).map_err(|err| {
            refused(METHOD, &format!("a {size}-byte probe allocation is not a valid layout: {err}"))
        })?;
        // SAFETY: `layout` has a non-zero size.
        let probe = unsafe { std::alloc::alloc_zeroed(layout) };
        if probe.is_null() {
            return Err(refused(METHOD, "the probe allocation failed"));
        }

        let mut properties = vk::MemoryHostPointerPropertiesEXT::default();
        // SAFETY: `device` is live, the handle type is the one for an ordinary host allocation,
        // `probe` is a live, committed, `alignment`-aligned allocation of this process, and
        // `properties` is a locally owned structure that outlives the call.
        let result = unsafe {
            get_properties(
                device.handle(),
                vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
                probe.cast(),
                &mut properties,
            )
        };
        // SAFETY: `probe` came from `alloc_zeroed` with this exact layout and nothing else freed
        // it; the driver's call above reads it and keeps no reference.
        unsafe { std::alloc::dealloc(probe, layout) };

        if result != vk::Result::SUCCESS {
            return Err(refused(
                METHOD,
                &format!(
                    "`vkGetMemoryHostPointerPropertiesEXT` on ordinary host memory answered \
                     {result:?}. That is this host failing to answer a question it has the \
                     extension for, not the device declining"
                ),
            ));
        }
        Ok(properties.memory_type_bits)
    }

    /// How many of each stage 5 object this host holds. Diagnostic (Global Constraint 6).
    ///
    /// # The allocation table is read **once**, and that is not a tidiness point
    ///
    /// Two of the fields below come from `device_memories`, and taking the guard twice inside one
    /// struct expression deadlocks: a temporary lives to the end of the *enclosing statement*, so
    /// the first guard is still held when the second is taken, and `std::sync::Mutex` is not
    /// reentrant. That is not hypothetical — it is how this function was first written, and the
    /// symptom was the stage 5 live test hanging in teardown after every one of its assertions had
    /// already passed.
    #[must_use]
    pub fn stage_five_objects(&self) -> StageFiveObjects {
        let (device_memories, imported_memories) = {
            let table = self.locked_memories();
            (table.len(), table.iter().filter(|(_, entry)| entry.imported).count())
        };
        StageFiveObjects {
            device_memories,
            imported_memories,
            buffers: self.locked_vk_buffers().len(),
            images: self.locked_created_images().len(),
            samplers: self.locked_samplers().len(),
            shader_modules: self.locked_modules().len(),
            pipeline_layouts: self.locked_layouts().len(),
            render_passes: self.locked_passes().len(),
            framebuffers: self.locked_framebuffers().len(),
            pipelines: self.locked_pipelines().len(),
            pipeline_caches: self.locked_caches().len(),
            descriptor_set_layouts: self.locked_set_layouts().len(),
            descriptor_pools: self.locked_descriptor_pools().len(),
            descriptor_sets: self.locked_descriptor_sets().len(),
        }
    }

    /// Build one `VkMappedMemoryRange` per `(memory, offset, size)`, checking the device.
    fn mapped_ranges(
        &self,
        ranges: &[(HostDeviceMemory, u64, u64)],
        device_index: usize,
        method: &'static str,
    ) -> AbiResult<Vec<vk::MappedMemoryRange<'static>>> {
        ranges
            .iter()
            .map(|(memory, offset, size)| {
                let (owner, handle, _) = self.memory_of(*memory, method)?;
                if owner != device_index {
                    return Err(cross_device("VkDeviceMemory", owner, device_index));
                }
                Ok(vk::MappedMemoryRange::default().memory(handle).offset(*offset).size(*size))
            })
            .collect()
    }

    /// Drop this host's record of every descriptor set a pool held.
    ///
    /// Called **after** the driver has freed them, by `vkDestroyDescriptorPool` and
    /// `vkResetDescriptorPool`. [`CommandBufferEntry`] makes the same argument one family along: a
    /// set whose pool is gone is a handle naming a freed driver object, and the shim drops the
    /// guest's handles in the same call.
    fn drop_sets_of(&self, pool: u64) {
        let mut table = self.locked_descriptor_sets();
        let doomed: Vec<u64> =
            table.iter().filter(|(_, entry)| entry.pool == pool).map(|(token, _)| token).collect();
        for token in doomed {
            table.remove(token);
        }
    }

    /// The `VkDescriptorSet` a token names, checked against one device.
    fn set_handle(
        table: &Slab<DescriptorSetEntry>,
        token: HostDescriptorSet,
        device_index: usize,
        method: &'static str,
    ) -> AbiResult<vk::DescriptorSet> {
        let entry = table.get(token.token()).ok_or_else(|| {
            refused(
                method,
                &format!(
                    "{token:?} is not a descriptor set this host holds -- it holds {}",
                    table.len()
                ),
            )
        })?;
        if entry.device != device_index {
            return Err(cross_device("VkDescriptorSet", entry.device, device_index));
        }
        Ok(entry.set)
    }

    /// Resolve one `VkWriteDescriptorSet`'s handles into an owned payload.
    fn write_payload(
        &self,
        device_index: usize,
        write: &DescriptorWrite,
        method: &'static str,
    ) -> AbiResult<WritePayload> {
        let set = {
            let table = self.locked_descriptor_sets();
            Self::set_handle(&table, write.set, device_index, method)?
        };
        let data = match &write.writes {
            DescriptorWrites::Images(entries) => {
                let samplers = self.locked_samplers();
                let views = self.locked_views();
                let built = entries
                    .iter()
                    .map(|(sampler, view, layout)| {
                        let mut info = vk::DescriptorImageInfo::default()
                            .image_layout(vk::ImageLayout::from_raw(*layout as i32));
                        if let Some(token) = sampler {
                            let (owner, handle) =
                                self.device_of(&samplers, token.token(), "VkSampler", method)?;
                            if owner != device_index {
                                return Err(cross_device("VkSampler", owner, device_index));
                            }
                            info = info.sampler(handle);
                        }
                        if let Some(token) = view {
                            let (owner, handle) =
                                self.device_of(&views, token.token(), "VkImageView", method)?;
                            if owner != device_index {
                                return Err(cross_device("VkImageView", owner, device_index));
                            }
                            info = info.image_view(handle);
                        }
                        Ok(info)
                    })
                    .collect::<AbiResult<Vec<_>>>()?;
                PayloadData::Images(built)
            }
            DescriptorWrites::Buffers(entries) => {
                let buffers = self.locked_vk_buffers();
                let built = entries
                    .iter()
                    .map(|(token, offset, range)| {
                        let (owner, handle) =
                            self.device_of(&buffers, token.token(), "VkBuffer", method)?;
                        if owner != device_index {
                            return Err(cross_device("VkBuffer", owner, device_index));
                        }
                        Ok(vk::DescriptorBufferInfo::default()
                            .buffer(handle)
                            .offset(*offset)
                            .range(*range))
                    })
                    .collect::<AbiResult<Vec<_>>>()?;
                PayloadData::Buffers(built)
            }
        };
        Ok(WritePayload { set, data })
    }

    /// Resolve and own one shader stage: its module, which must be `device_index`'s, its entry
    /// point as a C string, and its specialization. The graphics and compute calls share it,
    /// because both carry the same `VkPipelineShaderStageCreateInfo`.
    fn own_stage(
        &self,
        modules: &Slab<ObjectEntry<vk::ShaderModule>>,
        device_index: usize,
        stage: &ShaderStage,
        method: &'static str,
    ) -> AbiResult<OwnedStage> {
        let Some(token) = stage.module else {
            return Err(refused(method, "a shader stage names no module"));
        };
        let (owner, module) = self.device_of(modules, token.token(), "VkShaderModule", method)?;
        if owner != device_index {
            return Err(cross_device("VkShaderModule", owner, device_index));
        }
        let name = CString::new(stage.name.as_str()).map_err(|err| {
            refused(
                method,
                &format!(
                    "a shader stage entry point \"{}\" has an interior NUL ({err}). Passing it on \
                     would name a shorter entry point than the guest wrote, and SPIR-V matches it \
                     byte for byte",
                    stage.name
                ),
            )
        })?;
        let (entries, data, specialized) = match stage.specialization.as_ref() {
            None => (Vec::new(), Vec::new(), false),
            Some(specialization) => (
                specialization
                    .entries
                    .iter()
                    .map(|(id, offset, size)| vk::SpecializationMapEntry {
                        constant_id: *id,
                        offset: *offset,
                        size: usize::try_from(*size).unwrap_or(usize::MAX),
                    })
                    .collect(),
                specialization.data.clone(),
                true,
            ),
        };
        Ok(OwnedStage {
            flags: stage.flags,
            stage: stage.stage,
            module,
            name,
            entries,
            data,
            specialized,
        })
    }

    /// Resolve and own everything one `VkGraphicsPipelineCreateInfo` points at.
    ///
    /// Layer 0 of [`GfxVulkanHost::create_graphics_pipelines`]' four. Nothing here borrows from
    /// anything else, which is what makes the three layers above it able to.
    fn own_pipeline(
        &self,
        device_index: usize,
        request: &GraphicsPipelineRequest,
    ) -> AbiResult<OwnedPipeline> {
        const METHOD: &str = "VulkanHost::create_graphics_pipelines";
        let stages = {
            let modules = self.locked_modules();
            request
                .stages
                .iter()
                .map(|stage| self.own_stage(&modules, device_index, stage, METHOD))
                .collect::<AbiResult<Vec<_>>>()?
        };

        let vertex = request
            .vertex_input
            .as_ref()
            .map(|vertex| {
                AbiResult::Ok(OwnedVertexInput {
                    flags: vertex.flags,
                    bindings: vertex
                        .bindings
                        .iter()
                        .map(|bytes| vertex_binding_from_bytes(METHOD, bytes))
                        .collect::<AbiResult<Vec<_>>>()?,
                    attributes: vertex
                        .attributes
                        .iter()
                        .map(|bytes| vertex_attribute_from_bytes(METHOD, bytes))
                        .collect::<AbiResult<Vec<_>>>()?,
                })
            })
            .transpose()?;

        let viewport = request
            .viewport
            .as_ref()
            .map(|viewport| {
                AbiResult::Ok(OwnedViewport {
                    flags: viewport.flags,
                    viewport_count: viewport.viewport_count,
                    viewports: viewports_from_bytes(METHOD, &viewport.viewports)?,
                    scissor_count: viewport.scissor_count,
                    scissors: rects_from_bytes(METHOD, &viewport.scissors)?,
                })
            })
            .transpose()?;

        let multisample = request.multisample.as_ref().map(|multisample| OwnedMultisample {
            flags: multisample.flags,
            samples: multisample.samples,
            sample_shading: multisample.sample_shading != 0,
            min_sample_shading: f32::from_le_bytes(multisample.min_sample_shading),
            sample_mask: multisample.sample_mask.clone(),
            alpha_to_coverage: multisample.alpha_to_coverage != 0,
            alpha_to_one: multisample.alpha_to_one != 0,
        });

        let color_blend = request
            .color_blend
            .as_ref()
            .map(|blend| {
                let mut constants = [0.0f32; 4];
                for (index, value) in constants.iter_mut().enumerate() {
                    *value = f32::from_le_bytes(
                        blend.blend_constants[index * 4..][..4].try_into().expect("four"),
                    );
                }
                AbiResult::Ok(OwnedColorBlend {
                    flags: blend.flags,
                    logic_op_enable: blend.logic_op_enable != 0,
                    logic_op: blend.logic_op,
                    attachments: blend
                        .attachments
                        .iter()
                        .map(|bytes| blend_attachment_from_bytes(METHOD, bytes))
                        .collect::<AbiResult<Vec<_>>>()?,
                    blend_constants: constants,
                })
            })
            .transpose()?;

        let Some(rasterization) = request.rasterization.as_ref() else {
            return Err(refused(
                METHOD,
                "the request has no rasterization state, which the specification requires of \
                 every graphics pipeline. The shim refuses a NULL one by name, so this means the \
                 shim and this host disagree about what a decoded request contains",
            ));
        };
        let rasterization = rasterization_from_body(METHOD, rasterization)?;
        let depth_stencil = request
            .depth_stencil
            .as_ref()
            .map(|body| depth_stencil_from_body(METHOD, body))
            .transpose()?;

        let Some(layout_token) = request.layout else {
            return Err(refused(METHOD, "the request names no pipeline layout"));
        };
        let layout = {
            let table = self.locked_layouts();
            let (owner, handle) =
                self.device_of(&table, layout_token.token(), "VkPipelineLayout", METHOD)?;
            if owner != device_index {
                return Err(cross_device("VkPipelineLayout", owner, device_index));
            }
            handle
        };
        let Some(pass_token) = request.render_pass else {
            return Err(refused(METHOD, "the request names no render pass"));
        };
        let render_pass = {
            let table = self.locked_passes();
            let (owner, handle) =
                self.device_of(&table, pass_token.token(), "VkRenderPass", METHOD)?;
            if owner != device_index {
                return Err(cross_device("VkRenderPass", owner, device_index));
            }
            handle
        };
        let base = match request.base_pipeline {
            None => vk::Pipeline::null(),
            Some(token) => {
                let table = self.locked_pipelines();
                let (owner, handle) = self.device_of(&table, token.token(), "VkPipeline", METHOD)?;
                if owner != device_index {
                    return Err(cross_device("VkPipeline", owner, device_index));
                }
                handle
            }
        };

        Ok(OwnedPipeline {
            flags: request.flags,
            stages,
            vertex,
            input_assembly: request.input_assembly,
            tessellation: request.tessellation,
            viewport,
            rasterization,
            multisample,
            depth_stencil,
            color_blend,
            dynamic_states: request.dynamic_states.as_ref().map(|states| {
                states.iter().map(|state| vk::DynamicState::from_raw(*state as i32)).collect()
            }),
            layout,
            render_pass,
            subpass: request.subpass,
            base,
            base_index: request.base_pipeline_index,
        })
    }
}

/// The refusal an object from the wrong device produces.
///
/// **A check this host is the only thing that can make.** Submitting a command buffer or waiting
/// on a fence that belongs to a different `VkDevice` is undefined behaviour the specification does
/// not require a driver to detect, this machine has no validation layers
/// (`docs/research/graphics-spike.md` §6), and the guest can reach it with two handles it was
/// legitimately given — exactly the shape `with_surface`'s instance check catches one stage
/// earlier.
/// Live children as a refusal can say them: each kind with its count and its first three tokens.
fn describe_children(live: &[(&str, Vec<u64>)]) -> String {
    live.iter()
        .map(|(kind, tokens)| {
            let shown: Vec<String> =
                tokens.iter().take(3).map(|token| format!("#{token}")).collect();
            let more = tokens.len().saturating_sub(3);
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            format!("{} `{kind}` ({}{more})", tokens.len(), shown.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn cross_device(family: &str, had: usize, asked: usize) -> AbiError {
    refused(
        "VulkanHost",
        &format!(
            "a `{family}` belonging to device #{had} was used with device #{asked}. Both handles \
             are real; the pairing is what is wrong, and a driver is not required to notice that \
             it is -- on this machine nothing would, because there are no validation layers \
             installed"
        ),
    )
}

/// A `VkComponentMapping` from the bytes the guest wrote.
///
/// Sound for [`features_from_bytes`]' reason: the structure is four `VkComponentSwizzle` enums
/// with no padding and no pointer, so every byte pattern is a valid value of it and a swizzle the
/// specification does not define is the driver's to reject.
fn components_from_bytes(call: &str, bytes: &[u8]) -> AbiResult<vk::ComponentMapping> {
    let expected = std::mem::size_of::<vk::ComponentMapping>();
    if bytes.len() != expected {
        return Err(refused(
            call,
            &format!(
                "the guest's `components` arrived as {} bytes and `sizeof(VkComponentMapping)` is \
                 {expected} here. `omni_android::vulkan` and this crate disagree about a \
                 structure the specification fixes",
                bytes.len()
            ),
        ));
    }
    let mut value = vk::ComponentMapping::default();
    // SAFETY: the lengths are equal, checked one statement ago; the structure is four `u32`-sized
    // enums with no padding, so any byte pattern is a valid value of it. The source is a `&[u8]`
    // and the destination a distinct local, so they cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            std::ptr::addr_of_mut!(value).cast::<u8>(),
            bytes.len(),
        );
    }
    Ok(value)
}

/// A `VkImageSubresourceRange` from the bytes the guest wrote. As [`components_from_bytes`].
fn range_from_bytes(call: &str, bytes: &[u8]) -> AbiResult<vk::ImageSubresourceRange> {
    let expected = std::mem::size_of::<vk::ImageSubresourceRange>();
    if bytes.len() != expected {
        return Err(refused(
            call,
            &format!(
                "the guest's `subresourceRange` arrived as {} bytes and \
                 `sizeof(VkImageSubresourceRange)` is {expected} here. `omni_android::vulkan` and \
                 this crate disagree about a structure the specification fixes",
                bytes.len()
            ),
        ));
    }
    let mut value = vk::ImageSubresourceRange::default();
    // SAFETY: as `components_from_bytes`: five `uint32_t`s with no padding and no pointer.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            std::ptr::addr_of_mut!(value).cast::<u8>(),
            bytes.len(),
        );
    }
    Ok(value)
}

impl Drop for GfxVulkanHost {
    /// Destroys everything this host created, **in Vulkan's required order**.
    ///
    /// Devices, then surfaces, then instances — children before parents, which is the order the
    /// specification makes a hard requirement rather than a preference. [`crate::vulkan`]'s `Base`
    /// and `Dev` achieve the same thing by field order and say so; here it is a sequence, because
    /// the tables are separate mutexes.
    ///
    /// Each device is waited on first. `vkDestroyDevice` requires every queue to be idle, and
    /// nothing in this crate submits work to the guest's device — but the **guest** may have, at
    /// stage 4, and a host that skipped the wait would be relying on that staying true. There are
    /// no validation layers on this machine to notice if it stops
    /// (`docs/research/graphics-spike.md` §6), so the wait is the check.
    ///
    /// Queues are not destroyed and never could be: a `VkQueue` is owned by its device and goes
    /// away with it. The table is simply dropped.
    fn drop(&mut self) {
        // **Stage 4 first, and in its own order.** Every one of these objects is made *from* a
        // device, so all of them have to go before the devices do — and within the group the order
        // is the specification's: a command pool frees its buffers, so the buffers' entries are
        // simply dropped; a swapchain's images belong to it, so those entries are dropped too.
        //
        // Each device is waited idle **before** anything of its is destroyed, rather than only
        // before `vkDestroyDevice`. The guest may have submitted work and may have exited without
        // waiting, and destroying a semaphore a queue is still waiting on is the class of mistake
        // this host has no validation layer to report (`docs/research/graphics-spike.md` §6).
        {
            let devices = self.locked_devices();
            for entry in devices.iter().flatten() {
                // SAFETY: the device is live and this host created it.
                let _ = unsafe { entry.device.device_wait_idle() };
            }
        }

        // **Stage 5 first, and within it children before parents**, because a framebuffer names
        // image views and a pipeline names a layout and a render pass. Everything here is made
        // *from* a device, so all of it goes before the devices do. The guest's own `GuestSpace`
        // pages behind an imported allocation are **not** released here: they belong to
        // `omni_android::vulkan::Vulkan`, which unmaps them at `vkFreeMemory` and whose guest
        // address space is dropped with the guest rather than with this host.
        let pipelines = self.locked_pipelines().drain();
        for entry in pipelines {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and the pipeline is live.
            unsafe { device.destroy_pipeline(entry.object, None) };
        }
        let caches = self.locked_caches().drain();
        for entry in caches {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and no pipeline creation is in progress.
            unsafe { device.destroy_pipeline_cache(entry.object, None) };
        }
        let framebuffers = self.locked_framebuffers().drain();
        for entry in framebuffers {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle; this runs **before** the image views it names.
            unsafe { device.destroy_framebuffer(entry.object, None) };
        }
        let passes = self.locked_passes().drain();
        for entry in passes {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and every framebuffer that named this pass has gone.
            unsafe { device.destroy_render_pass(entry.object, None) };
        }
        let layouts = self.locked_layouts().drain();
        for entry in layouts {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and every pipeline built from this layout has gone.
            unsafe { device.destroy_pipeline_layout(entry.object, None) };
        }
        let descriptor_pools = self.locked_descriptor_pools().drain();
        for entry in descriptor_pools {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle, and destroying the pool frees every set allocated from
            // it -- which is why the set table is simply dropped below.
            unsafe { device.destroy_descriptor_pool(entry.object, None) };
        }
        let _ = self.locked_descriptor_sets().drain();
        let set_layouts = self.locked_set_layouts().drain();
        for entry in set_layouts {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and every set allocated from this layout has gone.
            unsafe { device.destroy_descriptor_set_layout(entry.object, None) };
        }
        let samplers = self.locked_samplers().drain();
        for entry in samplers {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and no descriptor still names this sampler.
            unsafe { device.destroy_sampler(entry.object, None) };
        }
        let modules = self.locked_modules().drain();
        for entry in modules {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle; a pipeline does not reference its modules after
            // creation, which the specification states.
            unsafe { device.destroy_shader_module(entry.object, None) };
        }

        let pools = self.locked_pools().drain();
        for entry in pools {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle, and destroying the pool frees every command buffer
            // allocated from it -- which is why the buffer table is simply dropped below.
            unsafe { device.destroy_command_pool(entry.object, None) };
        }
        let _ = self.locked_buffers().drain();

        let views = self.locked_views().drain();
        for entry in views {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and the view is live.
            unsafe { device.destroy_image_view(entry.object, None) };
        }
        let semaphores = self.locked_semaphores().drain();
        for entry in semaphores {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle, so nothing is waiting on this semaphore.
            unsafe { device.destroy_semaphore(entry.object, None) };
        }
        let fences = self.locked_fences().drain();
        for entry in fences {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle, so nothing references this fence.
            unsafe { device.destroy_fence(entry.object, None) };
        }

        // **The stage 5 objects that memory is bound to, after the views that might name them.**
        // An image view of a guest-created image has just been destroyed above, so the image is
        // free to go -- and the allocations go after both, because a `VkDeviceMemory` freed while
        // a buffer is still bound to it is a use-after-free the driver is not required to notice.
        let buffers = self.locked_vk_buffers().drain();
        for entry in buffers {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and nothing still reads this buffer.
            unsafe { device.destroy_buffer(entry.object, None) };
        }
        let created_images = self.locked_created_images().drain();
        for entry in created_images {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle, every view of this image has gone, and it is one the
            // guest created rather than a swapchain's -- those are a different table.
            unsafe { device.destroy_image(entry.object, None) };
        }
        let memories = self.locked_memories().drain();
        for entry in memories {
            let Ok(device) = self.device_at(entry.device, "GfxVulkanHost::drop") else { continue };
            // SAFETY: the device is idle and every buffer and image bound to this allocation has
            // just been destroyed. For an imported allocation the guest pages outlive this call,
            // which is correct: freeing the `VkDeviceMemory` is what releases the driver's claim
            // on them, and `omni-android` owns the mapping itself.
            unsafe { device.free_memory(entry.memory, None) };
        }

        // Swapchains last of the stage 4 group, because a swapchain's images may still be named by
        // views that have only just gone. The claim each one holds is released as its entry drops,
        // which is what frees the window for whatever comes next.
        let swapchains = self.locked_swapchains().drain();
        for entry in swapchains {
            let parts = self.device_parts(HostDevice::from_token(entry.device as u64));
            let Ok(parts) = parts else { continue };
            let swapchain_fn = Self::swapchain_fn(&parts.instance, &parts.device);
            // SAFETY: the device is idle and the swapchain is live.
            unsafe { swapchain_fn.destroy_swapchain(entry.handle, None) };
        }
        let _ = self.locked_images().drain();

        // Only the devices the guest did not destroy itself: `vkDestroyDevice` empties the slot it
        // destroys, so nothing here is destroyed twice.
        let devices = std::mem::take(&mut *self.locked_devices());
        for entry in devices.into_iter().flatten() {
            // SAFETY: the device is live and this host created it. The wait is what makes the
            // destroy below legal if the guest ever submitted work through it.
            let _ = unsafe { entry.device.device_wait_idle() };
            // SAFETY: every object made from this device was destroyed above, and `pAllocator`
            // was `None` at creation.
            unsafe { entry.device.destroy_device(None) };
        }

        let instances = std::mem::take(&mut *self.locked());
        // Only the surfaces the guest did not destroy itself: `vkDestroySurfaceKHR` removes the
        // entry it destroys, so nothing here is destroyed twice.
        let surfaces = self.locked_surfaces().drain();
        for surface in surfaces {
            let Some(instance) = instances.get(surface.instance).and_then(Option::as_ref) else {
                continue;
            };
            let surface_fn = khr::surface::Instance::new(&self.entry, instance);
            // SAFETY: the surface is live, it was created from this instance, every swapchain
            // made from it was destroyed above, and `pAllocator` was `None`.
            unsafe { surface_fn.destroy_surface(surface.surface, None) };
        }

        // Only the instances the guest did not destroy itself, for the surfaces' reason.
        for instance in instances.into_iter().flatten() {
            // SAFETY: each is a live instance this host created; every device and surface made
            // from it has just been destroyed above, and `pAllocator` was `None` at creation.
            unsafe { instance.destroy_instance(None) };
        }
    }
}

impl core::fmt::Debug for GfxVulkanHost {
    /// Prints how many instances are live, because a [`VulkanHost`] is interpolated into
    /// `omni_android::vulkan::Vulkan`'s own `Debug` and that is what a reader wants from it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let instances = self.locked();
        let live = instances.iter().flatten().count();
        write!(f, "GfxVulkanHost {{ {live} instance(s) live of {} created }}", instances.len())
    }
}

/// Copy one swapchain image into host memory and answer its bytes.
///
/// # What this does, step by step
///
/// 1. `vkDeviceWaitIdle`, because the presentation engine may still be reading the image and a
///    barrier is not enough on its own to order against a present that has not completed.
/// 2. A transient command pool, one primary command buffer and a fence.
/// 3. A host-visible, host-coherent buffer of `bytes`.
/// 4. A recording: `PRESENT_SRC_KHR` → `TRANSFER_SRC_OPTIMAL`, `vkCmdCopyImageToBuffer`,
///    `TRANSFER_SRC_OPTIMAL` → `PRESENT_SRC_KHR`. **The transition back matters**: the guest still
///    owns this swapchain and will present this image again, and an image left in
///    `TRANSFER_SRC_OPTIMAL` is one `vkQueuePresentKHR` would be undefined behaviour on — with no
///    validation layer on this machine to say so.
/// 5. Submit, wait on the fence with no timeout, map, copy, unmap, destroy everything.
///
/// Every object it creates is destroyed on both the success and the failure paths, which is why
/// the failure path is written as a closure the `?`-free body defers to rather than as nine
/// `inspect_err` arms.
///
/// # Safety
///
/// `device` must be live; `image` must be a swapchain image of that device currently in
/// `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR` whose swapchain was created with
/// `VK_IMAGE_USAGE_TRANSFER_SRC_BIT`; `queue` must belong to `family` on that device; `bytes` must
/// be `extent.width * extent.height * 4`.
unsafe fn read_back(
    device: &ash::Device,
    memory: &vk::PhysicalDeviceMemoryProperties,
    queue: vk::Queue,
    family: u32,
    image: vk::Image,
    extent: vk::Extent2D,
    bytes: u64,
) -> AbiResult<Vec<u8>> {
    const METHOD: &str = "GfxVulkanHost::read_presented_image";
    fn vk_failed(api: &'static str) -> impl Fn(vk::Result) -> AbiError {
        move |result| {
            refused(METHOD, &format!("the read-back's `{api}` failed with {result:?}"))
        }
    }

    // SAFETY: the device is live. Waiting idle is what orders this copy against a present that
    // may still be in flight; the result is propagated rather than discarded, because unlike a
    // teardown wait this one is load-bearing.
    unsafe { device.device_wait_idle() }.map_err(vk_failed("vkDeviceWaitIdle"))?;

    let pool_info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::TRANSIENT)
        .queue_family_index(family);
    // SAFETY: the device is live, `family` is one it was created with, `pool_info` outlives the
    // call.
    let pool = unsafe { device.create_command_pool(&pool_info, None) }
        .map_err(vk_failed("vkCreateCommandPool"))?;

    // From here on every exit destroys `pool`, which also frees the command buffer.
    let result = (|| -> AbiResult<Vec<u8>> {
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: `pool` was created from this device a statement ago.
        let buffers = unsafe { device.allocate_command_buffers(&allocate) }
            .map_err(vk_failed("vkAllocateCommandBuffers"))?;
        let command = buffers[0];

        let buffer_info = vk::BufferCreateInfo::default()
            .size(bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the device is live and `buffer_info` outlives the call.
        let staging = unsafe { device.create_buffer(&buffer_info, None) }
            .map_err(vk_failed("vkCreateBuffer"))?;

        let out = (|| -> AbiResult<Vec<u8>> {
            // SAFETY: `staging` was just created on this device.
            let needs = unsafe { device.get_buffer_memory_requirements(staging) };
            let visible =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let kind = select::memory_type(memory, needs.memory_type_bits, visible).ok_or_else(
                || {
                    refused(
                        METHOD,
                        &format!(
                            "no memory type of this device is both HOST_VISIBLE and \
                             HOST_COHERENT among the {bits:#010x} the staging buffer permits, so \
                             the presented pixels cannot be read back at all",
                            bits = needs.memory_type_bits
                        ),
                    )
                },
            )?;
            let allocate =
                vk::MemoryAllocateInfo::default().allocation_size(needs.size).memory_type_index(kind);
            // SAFETY: the device is live and `allocate` outlives the call.
            let memory_handle = unsafe { device.allocate_memory(&allocate, None) }
                .map_err(vk_failed("vkAllocateMemory"))?;

            let out = (|| -> AbiResult<Vec<u8>> {
                // SAFETY: the allocation came from a type the buffer's requirements permit and is
                // at least as large as they require, so binding at offset 0 is in range and
                // correctly aligned.
                unsafe { device.bind_buffer_memory(staging, memory_handle, 0) }
                    .map_err(vk_failed("vkBindBufferMemory"))?;

                // SAFETY: the device is live.
                let fence = unsafe {
                    device.create_fence(&vk::FenceCreateInfo::default(), None)
                }
                .map_err(vk_failed("vkCreateFence"))?;

                let out = (|| -> AbiResult<Vec<u8>> {
                    let range = vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1);
                    let to_source = vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::MEMORY_READ)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .old_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(range);
                    let back = vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(range);
                    let region = vk::BufferImageCopy::default()
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: extent.width,
                            height: extent.height,
                            depth: 1,
                        });

                    // SAFETY: `command` is a primary buffer of `pool`, in the initial state, and
                    // nothing else records into it. Every handle named in the recording is live
                    // and belongs to this device.
                    unsafe {
                        device
                            .begin_command_buffer(
                                command,
                                &vk::CommandBufferBeginInfo::default()
                                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                            )
                            .map_err(vk_failed("vkBeginCommandBuffer"))?;
                        device.cmd_pipeline_barrier(
                            command,
                            vk::PipelineStageFlags::TOP_OF_PIPE,
                            vk::PipelineStageFlags::TRANSFER,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            &[to_source],
                        );
                        device.cmd_copy_image_to_buffer(
                            command,
                            image,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            staging,
                            &[region],
                        );
                        device.cmd_pipeline_barrier(
                            command,
                            vk::PipelineStageFlags::TRANSFER,
                            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            &[back],
                        );
                        device
                            .end_command_buffer(command)
                            .map_err(vk_failed("vkEndCommandBuffer"))?;
                    }

                    let commands = [command];
                    let submit = vk::SubmitInfo::default().command_buffers(&commands);
                    // SAFETY: `queue` belongs to `family` of this device, `command` has been
                    // recorded and ended, and `fence` is unsignalled.
                    unsafe { device.queue_submit(queue, &[submit], fence) }
                        .map_err(vk_failed("vkQueueSubmit"))?;
                    // SAFETY: the fence was just submitted with. `u64::MAX` is "wait forever",
                    // which is right here: this is a test-time measurement and a timeout would
                    // turn a slow GPU into a failure that reads like a wrong colour.
                    unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }
                        .map_err(vk_failed("vkWaitForFences"))?;

                    // SAFETY: the memory is host-visible, is not mapped, and the submission that
                    // wrote it has completed -- which the fence wait above is what establishes.
                    let mapped = unsafe {
                        device.map_memory(memory_handle, 0, bytes, vk::MemoryMapFlags::empty())
                    }
                    .map_err(vk_failed("vkMapMemory"))?;
                    // SAFETY: `mapped` addresses at least `bytes` bytes of host-coherent memory
                    // the driver has just written, and the destination is a fresh `Vec` that
                    // cannot overlap it.
                    let pixels = unsafe {
                        std::slice::from_raw_parts(mapped.cast::<u8>(), bytes as usize).to_vec()
                    };
                    // SAFETY: the memory is mapped, by the call above.
                    unsafe { device.unmap_memory(memory_handle) };
                    Ok(pixels)
                })();

                // SAFETY: the fence is live and the submission that used it has completed.
                unsafe { device.destroy_fence(fence, None) };
                out
            })();

            // SAFETY: the memory is unmapped and no submission still references the buffer bound
            // to it -- the fence wait above is what says so.
            unsafe { device.free_memory(memory_handle, None) };
            out
        })();

        // SAFETY: the buffer is unbound from any live submission, for the reason above.
        unsafe { device.destroy_buffer(staging, None) };
        out
    })();

    // SAFETY: the pool is live, and destroying it frees the command buffer allocated from it. No
    // submission is still executing: every path above either waited on the fence or never
    // submitted.
    unsafe { device.destroy_command_pool(pool, None) };
    result
}

/// An [`AbiError::Refused`] naming this seam, with no thunk address.
///
/// Address zero because a [`VulkanHost`] has no thunk of its own: the address that matters is the
/// one in the refusal the shim wraps this in, which names the Vulkan function the guest called.
fn refused(symbol: &str, why: &str) -> AbiError {
    AbiError::Refused { symbol: symbol.to_string(), address: 0, why: why.to_string() }
}

/// Owned C strings for a list of names, refusing one with an interior NUL rather than truncating.
///
/// A truncated extension name is a plausible extension name, and the driver would answer
/// `VK_ERROR_EXTENSION_NOT_PRESENT` about a name the guest never wrote.
fn c_strings(field: &str, names: &[String]) -> AbiResult<Vec<CString>> {
    names
        .iter()
        .map(|name| {
            CString::new(name.as_str()).map_err(|err| {
                refused(
                    "vkCreateInstance",
                    &format!(
                        "`{field}` contains \"{name}\", which has an interior NUL ({err}). \
                         Passing it on would ask the driver for a shorter name than the guest \
                         wrote"
                    ),
                )
            })
        })
        .collect()
}

/// One optional name out of the request's [`ApplicationInfo`], as an owned C string.
///
/// [`ApplicationInfo`]: omni_android::vulkan::ApplicationInfo
fn optional_c_string(
    field: &str,
    request: &InstanceRequest,
    pick: impl Fn(&omni_android::vulkan::ApplicationInfo) -> &Option<String>,
) -> AbiResult<Option<CString>> {
    let Some(application) = request.application.as_ref() else { return Ok(None) };
    let Some(name) = pick(application) else { return Ok(None) };
    CString::new(name.as_str())
        .map(Some)
        .map_err(|err| refused("vkCreateInstance", &format!("`{field}` has an interior NUL: {err}")))
}


// ==================================================================== stage 5 support types

/// The alignment an imported host pointer is given when the instance cannot be asked for the real
/// one.
///
/// 64 KiB, which is **safe rather than guessed**: every alignment Vulkan reports is a power of
/// two, `minImportedHostPointerAlignment` measured 4096 on this machine, and an allocation aligned
/// to 64 KiB is aligned to every power of two up to it. Over-aligning costs rounding —
/// `Vulkan::imported_bytes()` is where that shows — and under-aligning would be undefined
/// behaviour with no validation layer to report it.
const CONSERVATIVE_IMPORT_ALIGNMENT: u64 = 64 * 1024;

/// How many of each stage 5 object a [`GfxVulkanHost`] currently holds.
///
/// **`imported_memories` is the one that is not a count of a kind.** It is how many of the
/// allocations are backed by pages out of the guest's own address space rather than the driver's,
/// which is the whole of stage 5's memory decision expressed as a number — and it is what a live
/// test asserts against, because an allocation the guest can map and one it cannot are
/// indistinguishable from their handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageFiveObjects {
    /// Live `VkDeviceMemory` allocations.
    pub device_memories: usize,
    /// How many of those were imported out of `GuestSpace`.
    pub imported_memories: usize,
    /// Live `VkBuffer`s.
    pub buffers: usize,
    /// Live `VkImage`s the guest created. Not the swapchain's.
    pub images: usize,
    /// Live `VkSampler`s.
    pub samplers: usize,
    /// Live `VkShaderModule`s.
    pub shader_modules: usize,
    /// Live `VkPipelineLayout`s.
    pub pipeline_layouts: usize,
    /// Live `VkRenderPass`es.
    pub render_passes: usize,
    /// Live `VkFramebuffer`s.
    pub framebuffers: usize,
    /// Live `VkPipeline`s.
    pub pipelines: usize,
    /// Live `VkPipelineCache`s.
    pub pipeline_caches: usize,
    /// Live `VkDescriptorSetLayout`s.
    pub descriptor_set_layouts: usize,
    /// Live `VkDescriptorPool`s.
    pub descriptor_pools: usize,
    /// Live `VkDescriptorSet`s.
    pub descriptor_sets: usize,
}

/// The attachment references of one subpass, owned so the `VkSubpassDescription` can point at them.
struct SubpassReferences {
    input: Vec<vk::AttachmentReference>,
    colour: Vec<vk::AttachmentReference>,
    resolve: Vec<vk::AttachmentReference>,
    depth: Option<vk::AttachmentReference>,
    preserve: Vec<u32>,
}

/// One shader stage's owned storage: the entry-point name and its specialization data.
struct OwnedStage {
    flags: u32,
    stage: u32,
    module: vk::ShaderModule,
    name: CString,
    entries: Vec<vk::SpecializationMapEntry>,
    data: Vec<u8>,
    /// Whether the guest supplied a `pSpecializationInfo` at all. **Not** `!entries.is_empty()`: a
    /// specialization info with no entries is a different thing from none, and only the guest
    /// knows which it meant.
    specialized: bool,
}

/// The vertex-input arrays, owned.
struct OwnedVertexInput {
    flags: u32,
    bindings: Vec<vk::VertexInputBindingDescription>,
    attributes: Vec<vk::VertexInputAttributeDescription>,
}

/// The viewport arrays and **their counts**, owned.
///
/// The counts are kept beside the arrays because they are allowed to disagree: with
/// `VK_DYNAMIC_STATE_VIEWPORT` set, `pViewports` may be NULL while `viewportCount` still has to be
/// right. See `omni_android::vulkan::ViewportState`.
struct OwnedViewport {
    flags: u32,
    viewport_count: u32,
    viewports: Vec<vk::Viewport>,
    scissor_count: u32,
    scissors: Vec<vk::Rect2D>,
}

/// The multisample state, owned.
struct OwnedMultisample {
    flags: u32,
    samples: u32,
    sample_shading: bool,
    min_sample_shading: f32,
    sample_mask: Vec<u32>,
    alpha_to_coverage: bool,
    alpha_to_one: bool,
}

/// The colour-blend state, owned.
struct OwnedColorBlend {
    flags: u32,
    logic_op_enable: bool,
    logic_op: u32,
    attachments: Vec<vk::PipelineColorBlendAttachmentState>,
    blend_constants: [f32; 4],
}

/// Everything one `VkGraphicsPipelineCreateInfo` needs, owned and resolved.
///
/// See [`GfxVulkanHost::create_graphics_pipelines`] for why the storage is built in layers.
struct OwnedPipeline {
    flags: u32,
    stages: Vec<OwnedStage>,
    vertex: Option<OwnedVertexInput>,
    input_assembly: Option<(u32, u32, u32)>,
    tessellation: Option<(u32, u32)>,
    viewport: Option<OwnedViewport>,
    rasterization: vk::PipelineRasterizationStateCreateInfo<'static>,
    multisample: Option<OwnedMultisample>,
    depth_stencil: Option<vk::PipelineDepthStencilStateCreateInfo<'static>>,
    color_blend: Option<OwnedColorBlend>,
    dynamic_states: Option<Vec<vk::DynamicState>>,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    subpass: u32,
    base: vk::Pipeline,
    base_index: i32,
}

/// The nine sub-states, built from an [`OwnedPipeline`] and borrowing it.
struct SubStates<'a> {
    vertex: Option<vk::PipelineVertexInputStateCreateInfo<'a>>,
    input_assembly: Option<vk::PipelineInputAssemblyStateCreateInfo<'a>>,
    tessellation: Option<vk::PipelineTessellationStateCreateInfo<'a>>,
    viewport: Option<vk::PipelineViewportStateCreateInfo<'a>>,
    multisample: Option<vk::PipelineMultisampleStateCreateInfo<'a>>,
    color_blend: Option<vk::PipelineColorBlendStateCreateInfo<'a>>,
    dynamic: Option<vk::PipelineDynamicStateCreateInfo<'a>>,
}

impl<'a> SubStates<'a> {
    fn of(owned: &'a OwnedPipeline) -> SubStates<'a> {
        SubStates {
            vertex: owned.vertex.as_ref().map(|vertex| {
                vk::PipelineVertexInputStateCreateInfo::default()
                    .flags(vk::PipelineVertexInputStateCreateFlags::from_raw(vertex.flags))
                    .vertex_binding_descriptions(&vertex.bindings)
                    .vertex_attribute_descriptions(&vertex.attributes)
            }),
            input_assembly: owned.input_assembly.map(|(flags, topology, restart)| {
                vk::PipelineInputAssemblyStateCreateInfo::default()
                    .flags(vk::PipelineInputAssemblyStateCreateFlags::from_raw(flags))
                    .topology(vk::PrimitiveTopology::from_raw(topology as i32))
                    .primitive_restart_enable(restart != 0)
            }),
            tessellation: owned.tessellation.map(|(flags, points)| {
                vk::PipelineTessellationStateCreateInfo::default()
                    .flags(vk::PipelineTessellationStateCreateFlags::from_raw(flags))
                    .patch_control_points(points)
            }),
            viewport: owned.viewport.as_ref().map(|viewport| {
                let mut built = vk::PipelineViewportStateCreateInfo::default()
                    .flags(vk::PipelineViewportStateCreateFlags::from_raw(viewport.flags));
                // **The count when there is no array, the array when there is.** `ash`'s
                // `viewports` sets the count from the slice, so calling it with an empty one for
                // a dynamic-viewport pipeline would say `viewportCount = 0` — and a pipeline with
                // no viewports draws nothing.
                built = if viewport.viewports.is_empty() {
                    built.viewport_count(viewport.viewport_count)
                } else {
                    built.viewports(&viewport.viewports)
                };
                if viewport.scissors.is_empty() {
                    built.scissor_count(viewport.scissor_count)
                } else {
                    built.scissors(&viewport.scissors)
                }
            }),
            multisample: owned.multisample.as_ref().map(|multisample| {
                let mut built = vk::PipelineMultisampleStateCreateInfo::default()
                    .flags(vk::PipelineMultisampleStateCreateFlags::from_raw(multisample.flags))
                    .rasterization_samples(vk::SampleCountFlags::from_raw(multisample.samples))
                    .sample_shading_enable(multisample.sample_shading)
                    .min_sample_shading(multisample.min_sample_shading)
                    .alpha_to_coverage_enable(multisample.alpha_to_coverage)
                    .alpha_to_one_enable(multisample.alpha_to_one);
                if !multisample.sample_mask.is_empty() {
                    built = built.sample_mask(&multisample.sample_mask);
                }
                built
            }),
            color_blend: owned.color_blend.as_ref().map(|blend| {
                vk::PipelineColorBlendStateCreateInfo::default()
                    .flags(vk::PipelineColorBlendStateCreateFlags::from_raw(blend.flags))
                    .logic_op_enable(blend.logic_op_enable)
                    .logic_op(vk::LogicOp::from_raw(blend.logic_op as i32))
                    .attachments(&blend.attachments)
                    .blend_constants(blend.blend_constants)
            }),
            dynamic: owned.dynamic_states.as_ref().map(|states| {
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(states)
            }),
        }
    }
}

impl OwnedPipeline {
    /// The create info, borrowing the stages and sub-states built from this pipeline.
    fn info<'a>(
        &'a self,
        stages: &'a [vk::PipelineShaderStageCreateInfo<'a>],
        sub: &'a SubStates<'a>,
    ) -> vk::GraphicsPipelineCreateInfo<'a> {
        let mut info = vk::GraphicsPipelineCreateInfo::default()
            .flags(vk::PipelineCreateFlags::from_raw(self.flags))
            .stages(stages)
            .rasterization_state(&self.rasterization)
            .layout(self.layout)
            .render_pass(self.render_pass)
            .subpass(self.subpass)
            .base_pipeline_handle(self.base)
            .base_pipeline_index(self.base_index);
        // **Each of these is set only when the guest supplied one.** A zeroed structure in place
        // of a NULL is a different pipeline — one that rasterizes where the engine asked for one
        // that does not — so the pointer stays null instead.
        if let Some(vertex) = sub.vertex.as_ref() {
            info = info.vertex_input_state(vertex);
        }
        if let Some(assembly) = sub.input_assembly.as_ref() {
            info = info.input_assembly_state(assembly);
        }
        if let Some(tessellation) = sub.tessellation.as_ref() {
            info = info.tessellation_state(tessellation);
        }
        if let Some(viewport) = sub.viewport.as_ref() {
            info = info.viewport_state(viewport);
        }
        if let Some(multisample) = sub.multisample.as_ref() {
            info = info.multisample_state(multisample);
        }
        if let Some(depth_stencil) = self.depth_stencil.as_ref() {
            info = info.depth_stencil_state(depth_stencil);
        }
        if let Some(blend) = sub.color_blend.as_ref() {
            info = info.color_blend_state(blend);
        }
        if let Some(dynamic) = sub.dynamic.as_ref() {
            info = info.dynamic_state(dynamic);
        }
        info
    }
}

/// One `VkWriteDescriptorSet`'s resolved payload, owned so the write can point at it.
struct WritePayload {
    set: vk::DescriptorSet,
    data: PayloadData,
}

/// Which of the two arrays a write carries. The third — `pTexelBufferView` — is refused one layer
/// up, because this stage creates no `VkBufferView`.
enum PayloadData {
    Images(Vec<vk::DescriptorImageInfo>),
    Buffers(Vec<vk::DescriptorBufferInfo>),
}

// -------------------------------------------------------------------- flat structures, decoded

/// Build one flat Vulkan structure out of the bytes the guest wrote.
///
/// # Why a macro over one `unsafe fn` per type
///
/// [`features_from_bytes`] and [`components_from_bytes`] make the argument for the technique and
/// this is eleven more of it: every structure here is `#[repr(C)]`, is made entirely of
/// fixed-width scalars and enums, has **no pointer and no padding hole**, and is therefore valid
/// for any byte pattern of its own length — so the guest's aarch64 LP64 bytes *are* the bytes this
/// host's structure holds. What makes it safe is the length check, which is against
/// `core::mem::size_of` of `ash`'s generated type rather than against a number written here: a
/// disagreement between the two crates about a structure the specification fixes is then a
/// refusal naming both numbers, not a short read.
///
/// Writing eleven of these by hand would be eleven places for the check to be omitted.
macro_rules! flat_structure {
    ($name:ident, $type:ty, $what:literal) => {
        #[doc = concat!("A `", $what, "` from the bytes the guest wrote.")]
        fn $name(call: &str, bytes: &[u8]) -> AbiResult<$type> {
            let expected = std::mem::size_of::<$type>();
            if bytes.len() != expected {
                return Err(refused(
                    call,
                    &format!(
                        concat!(
                            "a `", $what, "` arrived as {} byte(s) and `sizeof(", $what,
                            ")` is {} here. `omni_android::vulkan` and this crate disagree about \
                             a structure the specification fixes"
                        ),
                        bytes.len(),
                        expected
                    ),
                ));
            }
            let mut value = <$type>::default();
            // SAFETY: the lengths are equal, checked one statement ago; the structure is
            // `#[repr(C)]` and made entirely of fixed-width scalars and enums with no pointer and
            // no padding, so every byte pattern of its length is a valid value of it. The source
            // is a `&[u8]` and the destination a distinct local, so they cannot overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    std::ptr::addr_of_mut!(value).cast::<u8>(),
                    expected,
                );
            }
            Ok(value)
        }
    };
}

flat_structure!(attachment_from_bytes, vk::AttachmentDescription, "VkAttachmentDescription");
flat_structure!(dependency_from_bytes, vk::SubpassDependency, "VkSubpassDependency");
flat_structure!(reference_from_bytes, vk::AttachmentReference, "VkAttachmentReference");
flat_structure!(push_constant_from_bytes, vk::PushConstantRange, "VkPushConstantRange");
flat_structure!(rect_from_bytes, vk::Rect2D, "VkRect2D");

/// A list of `VkAttachmentReference` from the flat bytes of a guest array.
fn references_from_bytes(call: &str, bytes: &[u8]) -> AbiResult<Vec<vk::AttachmentReference>> {
    flat_list(call, bytes, "VkAttachmentReference", reference_from_bytes)
}

/// A list of `VkViewport` from the flat bytes of a guest array.
fn viewports_from_bytes(call: &str, bytes: &[u8]) -> AbiResult<Vec<vk::Viewport>> {
    flat_list(call, bytes, "VkViewport", viewport_from_bytes)
}

/// A list of `VkRect2D` from the flat bytes of a guest array.
fn rects_from_bytes(call: &str, bytes: &[u8]) -> AbiResult<Vec<vk::Rect2D>> {
    flat_list(call, bytes, "VkRect2D", rect_from_bytes)
}

/// A list of `VkBufferCopy` from the flat bytes of a guest array.
fn buffer_copies_from_bytes(call: &str, bytes: &[u8]) -> AbiResult<Vec<vk::BufferCopy>> {
    flat_list(call, bytes, "VkBufferCopy", buffer_copy_from_bytes)
}

/// A list of `VkBufferImageCopy` from the flat bytes of a guest array.
fn buffer_image_copies_from_bytes(
    call: &str,
    bytes: &[u8],
) -> AbiResult<Vec<vk::BufferImageCopy>> {
    flat_list(call, bytes, "VkBufferImageCopy", buffer_image_copy_from_bytes)
}

flat_structure!(viewport_from_bytes, vk::Viewport, "VkViewport");
flat_structure!(buffer_copy_from_bytes, vk::BufferCopy, "VkBufferCopy");
flat_structure!(buffer_image_copy_from_bytes, vk::BufferImageCopy, "VkBufferImageCopy");
flat_structure!(image_copy_from_bytes, vk::ImageCopy, "VkImageCopy");
flat_structure!(image_blit_from_bytes, vk::ImageBlit, "VkImageBlit");
flat_structure!(image_resolve_from_bytes, vk::ImageResolve, "VkImageResolve");
flat_structure!(
    blend_attachment_from_bytes,
    vk::PipelineColorBlendAttachmentState,
    "VkPipelineColorBlendAttachmentState"
);
flat_structure!(
    vertex_binding_from_bytes,
    vk::VertexInputBindingDescription,
    "VkVertexInputBindingDescription"
);
flat_structure!(
    vertex_attribute_from_bytes,
    vk::VertexInputAttributeDescription,
    "VkVertexInputAttributeDescription"
);

/// Split a guest array's flat bytes into elements and decode each one.
///
/// The stride is `sizeof` of **this host's** structure, and a length that does not divide by it is
/// a refusal rather than a truncation: a truncated list of attachment references is a plausible
/// list of attachment references, and the render pass built from it would be a different one.
fn flat_list<T>(
    call: &str,
    bytes: &[u8],
    what: &str,
    decode: impl Fn(&str, &[u8]) -> AbiResult<T>,
) -> AbiResult<Vec<T>> {
    let stride = std::mem::size_of::<T>();
    if stride == 0 || bytes.len() % stride != 0 {
        return Err(refused(
            call,
            &format!(
                "an array of `{what}` arrived as {} byte(s), which does not divide by the \
                 {stride} that `sizeof({what})` is here",
                bytes.len()
            ),
        ));
    }
    bytes.chunks_exact(stride).map(|chunk| decode(call, chunk)).collect()
}

/// A `VkClearValue` from the sixteen bytes the guest wrote.
///
/// **The union travels whole.** Which member is live is decided by the attachment's format, which
/// this layer does not know; the three members alias the same sixteen bytes, so this is a copy and
/// not an interpretation. [`VulkanHost::cmd_clear_color_image`] makes the same argument for
/// `VkClearColorValue`.
fn clear_value_from_bytes(bytes: &[u8; 16]) -> vk::ClearValue {
    let mut value = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0; 4] } };
    // SAFETY: `VkClearValue` is a union of four sixteen-byte aggregates of scalars, so every byte
    // pattern is a valid value of it and its size is exactly sixteen. The source is a fixed-size
    // array and the destination a distinct local, so they cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            std::ptr::addr_of_mut!(value).cast::<u8>(),
            16,
        );
    }
    value
}

/// Build one Vulkan create-info structure from the **body** bytes after its `sType` and `pNext`.
///
/// [`flat_structure`]'s argument, for the three structures whose bodies are flat but whose first
/// sixteen bytes are a header this host must set itself — `sType` because `ash`'s `default()` is
/// what knows it, and `pNext` because it must be null and the shim refuses any chain.
macro_rules! flat_body {
    ($name:ident, $type:ty, $what:literal, $body:expr) => {
        #[doc = concat!("A `", $what, "` from the body bytes after its `pNext`.")]
        fn $name(call: &str, body: &[u8]) -> AbiResult<$type> {
            const HEADER: usize = 16;
            if body.len() != $body {
                return Err(refused(
                    call,
                    &format!(
                        concat!(
                            "the body of a `", $what, "` arrived as {} byte(s) and this stage's \
                             constant says {}. `omni_android::vulkan` and this crate disagree \
                             about a structure the specification fixes"
                        ),
                        body.len(),
                        $body
                    ),
                ));
            }
            let expected = std::mem::size_of::<$type>();
            if expected < HEADER + $body {
                return Err(refused(
                    call,
                    &format!(
                        concat!("`sizeof(", $what, ")` is {} here, which cannot hold a \
                                 sixteen-byte header and a {}-byte body"),
                        expected, $body
                    ),
                ));
            }
            let mut value = <$type>::default();
            // SAFETY: the body is exactly the bytes of the members after `pNext`, all of which
            // are fixed-width scalars, enums and fixed aggregates of those with no pointer and no
            // padding; `default()` has already written the `sType` and a null `pNext` into the
            // first sixteen bytes, which this does not touch. The length fits, checked one
            // statement ago, and the two regions cannot overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    body.as_ptr(),
                    std::ptr::addr_of_mut!(value).cast::<u8>().add(HEADER),
                    $body,
                );
            }
            Ok(value)
        }
    };
}

flat_body!(sampler_from_body, vk::SamplerCreateInfo<'static>, "VkSamplerCreateInfo", 64);
flat_body!(
    rasterization_from_body,
    vk::PipelineRasterizationStateCreateInfo<'static>,
    "VkPipelineRasterizationStateCreateInfo",
    44
);
flat_body!(
    depth_stencil_from_body,
    vk::PipelineDepthStencilStateCreateInfo<'static>,
    "VkPipelineDepthStencilStateCreateInfo",
    88
);

/// A `VkMemoryRequirements` as the bytes the guest is owed.
///
/// The output direction of [`flat_structure`]'s argument: three fixed-width members, no pointer,
/// identical on both targets, so the driver's structure *is* the guest's bytes.
fn requirements_bytes(requirements: &vk::MemoryRequirements) -> Vec<u8> {
    let mut out = vec![0u8; std::mem::size_of::<vk::MemoryRequirements>()];
    // SAFETY: the destination is exactly `sizeof(VkMemoryRequirements)` bytes, the source is one
    // such structure, and a `Vec<u8>` cannot overlap a local.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(requirements).cast::<u8>(),
            out.as_mut_ptr(),
            out.len(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The probe list is the WSI extensions Vulkan defines, and the Android one is last.**
    ///
    /// Order is the assertion, not membership: the first match wins, so a list that put
    /// `VK_KHR_android_surface` first would make a Windows host answer with the guest's own name,
    /// the substitution would collapse to a no-op, and the rewrite log would record nothing while
    /// a rename was still needed.
    #[test]
    fn the_platform_surface_probe_list_prefers_a_real_platform_over_the_guest_s_own() {
        assert_eq!(PLATFORM_SURFACE_EXTENSIONS[0], "VK_KHR_win32_surface");
        assert_eq!(
            PLATFORM_SURFACE_EXTENSIONS[PLATFORM_SURFACE_EXTENSIONS.len() - 1],
            "VK_KHR_android_surface"
        );
        assert!(
            PLATFORM_SURFACE_EXTENSIONS.iter().all(|name| name.ends_with("_surface")),
            "every entry is a window-system-integration extension"
        );
        let unique: std::collections::BTreeSet<&str> =
            PLATFORM_SURFACE_EXTENSIONS.iter().copied().collect();
        assert_eq!(unique.len(), PLATFORM_SURFACE_EXTENSIONS.len(), "no duplicates");
    }

    /// **The extension and the entry point at each index are the same platform's.**
    ///
    /// The pairing `platform_index` relies on, checked entry by entry rather than trusted to two
    /// lists staying in step. A mismatch would make `omni-android` hand the guest a thunk for
    /// `vkCreateAndroidSurfaceKHR` on the strength of a command the surface it later creates is
    /// not made by — and nothing downstream could notice, because both names would be real.
    #[test]
    fn every_platform_surface_extension_is_paired_with_its_own_entry_point() {
        assert_eq!(PLATFORM_SURFACE_ENTRY_POINTS.len(), PLATFORM_SURFACE_EXTENSIONS.len());
        for (extension, entry_point) in
            PLATFORM_SURFACE_EXTENSIONS.iter().zip(PLATFORM_SURFACE_ENTRY_POINTS.iter())
        {
            // `VK_KHR_win32_surface` <-> `vkCreateWin32SurfaceKHR`: the platform word is shared,
            // which is the only mechanical check available and catches a transposed pair.
            let platform = extension
                .trim_start_matches("VK_KHR_")
                .trim_start_matches("VK_EXT_")
                .trim_end_matches("_surface");
            assert!(
                entry_point.to_ascii_lowercase().contains(&platform.to_ascii_lowercase()),
                "`{extension}` is paired with `{entry_point}`, which is another platform's call"
            );
            assert!(entry_point.starts_with("vkCreate"), "{entry_point}");
        }
        // The guest's own name is at the end of both, so a genuine Android host substitutes
        // nothing in either direction.
        assert_eq!(PLATFORM_SURFACE_ENTRY_POINTS[5], "vkCreateAndroidSurfaceKHR");
        assert_eq!(
            PLATFORM_SURFACE_ENTRY_POINTS[0],
            "vkCreateWin32SurfaceKHR",
            "the one `create_platform_surface` names when it takes the Win32 arm"
        );
    }

    /// **A name with an interior NUL is refused, not truncated**, and the refusal quotes it.
    ///
    /// No driver is needed for this, which is the point: it runs on every machine, and it is the
    /// detector for the one silent-corruption path this file has.
    #[test]
    fn a_name_with_an_interior_nul_is_refused_rather_than_truncated() {
        let error = c_strings("ppEnabledExtensionNames", &["VK_KHR\0_surface".to_string()])
            .expect_err("an interior NUL cannot become a C string");
        let text = error.to_string();
        assert!(text.contains("interior NUL"), "{text}");
        assert!(text.contains("ppEnabledExtensionNames"), "{text}");
        assert_eq!(error.symbol(), Some("vkCreateInstance"));

        let clean = c_strings("ppEnabledExtensionNames", &["VK_KHR_surface".to_string()])
            .expect("an ordinary name");
        assert_eq!(clean.len(), 1);
        assert_eq!(clean[0].as_c_str(), c"VK_KHR_surface");
    }

    /// **`omni-android`'s structure sizes are `ash`'s structure sizes.**
    ///
    /// This is the assertion that makes stage 3's "the structure travels as bytes" honest. The
    /// adapter names each size as a constant a reader can check against `vulkan_core.h`, because
    /// it must refuse a blob of any other length before writing it into a guest buffer, and it
    /// has no way to *verify* the number — it cannot depend on `ash`. This crate can, and does.
    ///
    /// `ash`'s structures are generated from `vk.xml`, so a disagreement means one of the two
    /// numbers is wrong about a layout the specification fixes, and this test is where it becomes
    /// visible rather than becoming a short write. No driver is needed, so it runs on every
    /// machine.
    #[test]
    fn the_adapter_s_structure_sizes_are_the_ones_ash_generates() {
        use omni_android::vulkan as guest;
        assert_eq!(
            std::mem::size_of::<vk::PhysicalDeviceProperties>(),
            guest::PHYSICAL_DEVICE_PROPERTIES_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::PhysicalDeviceFeatures>(),
            guest::PHYSICAL_DEVICE_FEATURES_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::QueueFamilyProperties>(),
            guest::QUEUE_FAMILY_PROPERTIES_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::PhysicalDeviceMemoryProperties>(),
            guest::PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::SurfaceCapabilitiesKHR>(),
            guest::SURFACE_CAPABILITIES_BYTES
        );
        assert_eq!(std::mem::size_of::<vk::SurfaceFormatKHR>(), guest::SURFACE_FORMAT_BYTES);
        assert_eq!(std::mem::size_of::<vk::PresentModeKHR>(), guest::PRESENT_MODE_BYTES);
        assert_eq!(std::mem::size_of::<vk::ExtensionProperties>(), guest::EXTENSION_PROPERTIES_BYTES);
        assert_eq!(std::mem::size_of::<vk::FormatProperties>(), guest::FORMAT_PROPERTIES_BYTES);
        assert_eq!(
            std::mem::size_of::<vk::QueryPoolCreateInfo<'_>>(),
            guest::QUERY_POOL_CREATE_INFO_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::DescriptorUpdateTemplateCreateInfo<'_>>(),
            guest::DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::DescriptorUpdateTemplateEntry>(),
            guest::DESCRIPTOR_UPDATE_TEMPLATE_ENTRY_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::ComputePipelineCreateInfo<'_>>(),
            guest::COMPUTE_PIPELINE_CREATE_INFO_BYTES
        );
        assert_eq!(std::mem::size_of::<vk::ImageCopy>(), guest::IMAGE_COPY_BYTES);
        assert_eq!(std::mem::size_of::<vk::ImageBlit>(), guest::IMAGE_BLIT_BYTES);
        assert_eq!(
            vk::StructureType::DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO.as_raw(),
            i32::try_from(guest::STYPE_DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO).expect("an sType")
        );
        assert_eq!(
            vk::StructureType::QUERY_POOL_CREATE_INFO.as_raw(),
            i32::try_from(guest::STYPE_QUERY_POOL_CREATE_INFO).expect("an sType")
        );
        assert_eq!(
            std::mem::size_of::<vk::ImageFormatProperties>(),
            guest::IMAGE_FORMAT_PROPERTIES_BYTES
        );
        // The *input* structures the adapter decodes by hand, for the same reason.
        assert_eq!(std::mem::size_of::<vk::DeviceCreateInfo<'_>>(), guest::DEVICE_CREATE_INFO_BYTES);
        assert_eq!(
            std::mem::size_of::<vk::DeviceQueueCreateInfo<'_>>(),
            guest::DEVICE_QUEUE_CREATE_INFO_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::AndroidSurfaceCreateInfoKHR<'_>>(),
            guest::ANDROID_SURFACE_CREATE_INFO_BYTES
        );
        assert_eq!(
            vk::StructureType::ANDROID_SURFACE_CREATE_INFO_KHR.as_raw(),
            i32::try_from(guest::STYPE_ANDROID_SURFACE_CREATE_INFO_KHR).expect("a VkStructureType")
        );
    }

    /// **Every flat structure a `pNext` chain may carry is the size and `sType` `ash` says**, and
    /// `VkPhysicalDeviceFeatures2` too: the adapter's table is a claim about `vk.xml`, and this is
    /// where it is checked against the headers `ash` is generated from.
    #[test]
    fn the_flat_chain_structures_are_ashs_sizes_and_stypes() {
        use omni_android::vulkan as guest;
        use vk::StructureType as S;
        for known in guest::FLAT_STRUCTURES {
            let s_type = i32::try_from(known.s_type).expect("a VkStructureType");
            let size = match S::from_raw(s_type) {
                S::PHYSICAL_DEVICE_MULTIVIEW_FEATURES => {
                    std::mem::size_of::<vk::PhysicalDeviceMultiviewFeatures<'_>>()
                }
                S::PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES => {
                    std::mem::size_of::<vk::PhysicalDeviceSamplerYcbcrConversionFeatures<'_>>()
                }
                S::PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_FEATURES_EXT => {
                    std::mem::size_of::<vk::PhysicalDeviceExtendedDynamicStateFeaturesEXT<'_>>()
                }
                S::SAMPLER_YCBCR_CONVERSION_IMAGE_FORMAT_PROPERTIES => {
                    std::mem::size_of::<vk::SamplerYcbcrConversionImageFormatProperties<'_>>()
                }
                other => panic!("{} ({other:?}) has no `ash` structure named here", known.name),
            };
            assert_eq!(known.size(), size, "{}", known.name);
        }
        assert_eq!(
            std::mem::size_of::<vk::PhysicalDeviceFeatures2<'_>>(),
            guest::PHYSICAL_DEVICE_FEATURES_2_BYTES
        );
        assert_eq!(
            S::PHYSICAL_DEVICE_FEATURES_2.as_raw(),
            i32::try_from(guest::STYPE_PHYSICAL_DEVICE_FEATURES_2).expect("a VkStructureType")
        );
        assert_eq!(
            std::mem::size_of::<vk::PhysicalDeviceImageFormatInfo2<'_>>(),
            guest::PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::ImageFormatProperties2<'_>>(),
            guest::IMAGE_FORMAT_PROPERTIES_2_BYTES
        );
        assert_eq!(
            S::PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2.as_raw(),
            i32::try_from(guest::STYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2).expect("an sType")
        );
        assert_eq!(
            S::IMAGE_FORMAT_PROPERTIES_2.as_raw(),
            i32::try_from(guest::STYPE_IMAGE_FORMAT_PROPERTIES_2).expect("an sType")
        );
    }

    /// **Stage 4's structure sizes are `ash`'s too**, and so are its two resize codes.
    ///
    /// The same assertion one stage along, and it matters more here than it did for stage 3:
    /// stage 3's structures were mostly *outputs* whose size a short write would corrupt, and
    /// stage 4's are mostly **inputs** the adapter decodes field by field. A size that disagreed
    /// would mean an offset that disagreed, and an offset that disagreed inside
    /// `VkSwapchainCreateInfoKHR` is a `surface` handle read out of the two halves of something
    /// else — which the adapter would then look up in its own registry and, finding nothing,
    /// refuse. That refusal would be true and would name the wrong cause.
    ///
    /// `ash`'s structures and enums are generated from `vk.xml`, so a disagreement means one of
    /// the two numbers is wrong about something the specification fixes. No driver is needed, so
    /// this runs on every machine.
    #[test]
    fn stage_fours_structure_sizes_and_result_codes_are_the_ones_ash_generates() {
        use omni_android::vulkan as guest;

        assert_eq!(
            std::mem::size_of::<vk::SwapchainCreateInfoKHR<'_>>(),
            guest::SWAPCHAIN_CREATE_INFO_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::ImageViewCreateInfo<'_>>(),
            guest::IMAGE_VIEW_CREATE_INFO_BYTES
        );
        assert_eq!(std::mem::size_of::<vk::ComponentMapping>(), guest::COMPONENT_MAPPING_BYTES);
        assert_eq!(
            std::mem::size_of::<vk::ImageSubresourceRange>(),
            guest::IMAGE_SUBRESOURCE_RANGE_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::SemaphoreCreateInfo<'_>>(),
            guest::SEMAPHORE_CREATE_INFO_BYTES
        );
        assert_eq!(std::mem::size_of::<vk::FenceCreateInfo<'_>>(), guest::FENCE_CREATE_INFO_BYTES);
        assert_eq!(
            std::mem::size_of::<vk::CommandPoolCreateInfo<'_>>(),
            guest::COMMAND_POOL_CREATE_INFO_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::CommandBufferAllocateInfo<'_>>(),
            guest::COMMAND_BUFFER_ALLOCATE_INFO_BYTES
        );
        assert_eq!(
            std::mem::size_of::<vk::CommandBufferBeginInfo<'_>>(),
            guest::COMMAND_BUFFER_BEGIN_INFO_BYTES
        );
        assert_eq!(std::mem::size_of::<vk::MemoryBarrier<'_>>(), guest::MEMORY_BARRIER_BYTES);
        assert_eq!(
            std::mem::size_of::<vk::ImageMemoryBarrier<'_>>(),
            guest::IMAGE_MEMORY_BARRIER_BYTES
        );
        assert_eq!(std::mem::size_of::<vk::SubmitInfo<'_>>(), guest::SUBMIT_INFO_BYTES);
        assert_eq!(std::mem::size_of::<vk::PresentInfoKHR<'_>>(), guest::PRESENT_INFO_BYTES);
        assert_eq!(std::mem::size_of::<vk::ClearColorValue>(), 16);

        assert_eq!(
            vk::StructureType::SWAPCHAIN_CREATE_INFO_KHR.as_raw(),
            i32::try_from(guest::STYPE_SWAPCHAIN_CREATE_INFO_KHR).expect("a VkStructureType")
        );

        // **The three codes the guest's resize branch reads**, which is the part of this test
        // that is not about layout at all: a `VK_SUBOPTIMAL_KHR` spelled with the wrong number
        // would be forwarded faithfully and understood as something else.
        assert_eq!(vk::Result::SUBOPTIMAL_KHR.as_raw(), guest::VK_SUBOPTIMAL_KHR);
        assert_eq!(vk::Result::ERROR_OUT_OF_DATE_KHR.as_raw(), guest::VK_ERROR_OUT_OF_DATE_KHR);
        assert_eq!(vk::Result::TIMEOUT.as_raw(), guest::VK_TIMEOUT);
        assert_eq!(vk::Result::NOT_READY.as_raw(), guest::VK_NOT_READY);
        // And `VK_SUBOPTIMAL_KHR` is positive, which is how the specification says it is a
        // success -- asserted against `ash`'s own value rather than against the adapter's copy.
        assert!(vk::Result::SUBOPTIMAL_KHR.as_raw() > 0);
        assert!(vk::Result::ERROR_OUT_OF_DATE_KHR.as_raw() < 0);
    }

    /// **A generation-tagged slot makes a stale token name nothing**, which is the whole reason
    /// [`Slab`] exists rather than a `Vec`.
    ///
    /// The failure it prevents is the quiet one: a guest destroys its swapchain, creates another,
    /// and the driver hands back a table slot that was just freed. With a bare index as the token,
    /// a stale `VkSwapchainKHR` the guest still held would name the **new** swapchain, and
    /// `vkQueuePresentKHR` on it would work — into a window the guest thought it had finished
    /// with. No driver is needed for this, which is the point.
    #[test]
    fn a_reused_slab_slot_does_not_answer_to_the_token_its_last_occupant_had() {
        let mut slab: Slab<&str> = Slab::new();
        let first = slab.insert("the first swapchain");
        let second = slab.insert("another object");
        assert_eq!(slab.get(first), Some(&"the first swapchain"));
        assert_eq!(slab.len(), 2);

        assert_eq!(slab.remove(first), Some("the first swapchain"));
        assert_eq!(slab.get(first), None, "a destroyed object's token names nothing");
        assert_eq!(slab.remove(first), None, "and a second destroy finds nothing");

        let reused = slab.insert("the replacement");
        assert_ne!(reused, first, "the slot was reused and the token is a different number");
        assert_eq!(Slab::<&str>::split(reused).1, Slab::<&str>::split(first).1, "same slot");
        assert_eq!(slab.get(reused), Some(&"the replacement"));
        assert_eq!(slab.get(first), None, "the stale token still names nothing");
        assert_eq!(slab.get(second), Some(&"another object"), "the neighbour is untouched");

        let live: Vec<&str> = slab.iter().map(|(_, value)| *value).collect();
        assert_eq!(live, vec!["the replacement", "another object"]);
        assert_eq!(slab.drain().len(), 2);
        assert_eq!(slab.len(), 0);
    }

    /// **A `B8G8R8A8` read-back comes out in R, G, B, A order**, which is the one conversion
    /// between the driver's bytes and an assertion about a colour.
    ///
    /// The failure it prevents is a test that passes for the wrong reason: a clear to pure red
    /// read back through a swapchain whose channel order was ignored is pure blue, and a test
    /// asserting "some channel is 255" would not notice. This host's surfaces offer `B8G8R8A8`
    /// first, so the swapped path is the one that actually runs.
    #[test]
    fn a_bgra_read_back_is_reordered_and_an_rgba_one_is_not() {
        assert_eq!(Channels::of(vk::Format::B8G8R8A8_UNORM), Some(Channels::Bgra));
        assert_eq!(Channels::of(vk::Format::B8G8R8A8_SRGB), Some(Channels::Bgra));
        assert_eq!(Channels::of(vk::Format::R8G8B8A8_UNORM), Some(Channels::Rgba));
        assert_eq!(Channels::of(vk::Format::R8G8B8A8_SRGB), Some(Channels::Rgba));
        // A packed ten-bit format is refused rather than approximated; the constant is one this
        // host's own surface offers.
        assert_eq!(Channels::of(vk::Format::A2B10G10R10_UNORM_PACK32), None);
        assert_eq!(Channels::of(vk::Format::R5G6B5_UNORM_PACK16), None);

        // One pixel of "pure red, opaque" as a BGRA driver would have written it.
        let bgra = [0x00u8, 0x00, 0xFF, 0xFF];
        assert_eq!(Channels::Bgra.to_rgba(&bgra), vec![0xFF, 0x00, 0x00, 0xFF]);
        assert_eq!(Channels::Rgba.to_rgba(&bgra), bgra.to_vec(), "and RGBA is left alone");

        let image = PresentedImage {
            width: 2,
            height: 2,
            format: vk::Format::B8G8R8A8_UNORM.as_raw(),
            rgba: vec![
                1, 2, 3, 4, /* (0,0) */ 5, 6, 7, 8, /* (1,0) */
                9, 10, 11, 12, /* (0,1) */ 13, 14, 15, 16, /* (1,1) */
            ],
        };
        assert_eq!(image.pixel(0, 0), Some([1, 2, 3, 4]));
        assert_eq!(image.pixel(1, 1), Some([13, 14, 15, 16]));
        assert_eq!(image.pixel(2, 0), None, "outside the image");
        assert_eq!(image.pixel(0, 2), None);
        // `centre` is `(width / 2, height / 2)`, which for a 2x2 is (1, 1).
        assert_eq!(image.centre(), Some([13, 14, 15, 16]));
    }

    /// **A `VkPhysicalDeviceFeatures` round-trips through bytes unchanged.**
    ///
    /// The one place guest-authored bytes become a host structure, so it is the one place a
    /// transposition would silently enable the wrong feature. Two distant fields are set and read
    /// back, which a `memcpy` of the wrong length or a struct of the wrong size cannot survive.
    #[test]
    fn features_survive_the_round_trip_through_bytes() {
        let original = vk::PhysicalDeviceFeatures {
            robust_buffer_access: vk::TRUE, // the first member
            geometry_shader: vk::TRUE,
            inherited_queries: vk::TRUE, // the last member
            ..Default::default()
        };
        // SAFETY: `VkPhysicalDeviceFeatures` is 55 `VkBool32`s with no padding.
        let bytes = unsafe { pod_bytes(&original) };
        assert_eq!(bytes.len(), omni_android::vulkan::PHYSICAL_DEVICE_FEATURES_BYTES);
        let back = features_from_bytes(&bytes).expect("the right length");
        assert_eq!(back.robust_buffer_access, vk::TRUE);
        assert_eq!(back.inherited_queries, vk::TRUE, "the last member survived, so did the size");
        assert_eq!(back.geometry_shader, vk::TRUE);
        assert_eq!(back.tessellation_shader, vk::FALSE, "and nothing else was turned on");

        let error = features_from_bytes(&bytes[..4]).expect_err("a short blob is refused");
        assert_eq!(error.symbol(), Some("vkCreateDevice"));
    }
}
