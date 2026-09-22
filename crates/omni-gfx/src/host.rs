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
//! There is **no `vkDestroyInstance`, `vkDestroySurfaceKHR` or `vkDestroyDevice`** in the trait,
//! because the guest has never called one: the decoded bootstrap at guest `0x02595160` resolves
//! two names, and stage 3 implements the set a renderer needs in order to *reach* a device. A
//! guest that calls a destructor gets a refusal naming the function from the thunk, which is the
//! honest answer and is also what makes this file's tables exactly what this host made — nothing
//! can have gone away behind their back.
//!
//! So [`Drop`] is the only destructor, and it runs the whole tree in Vulkan's required order:
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
use std::sync::{Arc, Mutex, PoisonError};

use ash::khr;
use ash::vk;
use omni_android::vulkan::{
    Acquired, DeviceRequest, DriverAnswer, HostCommandBuffer, HostCommandPool, HostDevice,
    HostExtension, HostFence, HostImage, HostImageRef, HostImageView, HostInstance,
    HostPhysicalDevice, HostQueue, HostSemaphore, HostSurface, HostSwapchain, ImageViewRequest,
    InstanceRequest, PipelineBarrier, PresentRequest, Presented, SubmitRequest, SurfaceCreated,
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

/// How many `VkInstance`s this host will create before it refuses.
///
/// An allocation bound rather than a Vulkan limit. `omni_android::vulkan::MAX_INSTANCES` bounds
/// the guest-visible registry at four; this is deliberately a little larger, so that the refusal a
/// guest sees comes from the registry — which can name the handle and the count — rather than from
/// here, where the only thing that could be said is "the host is full".
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
    /// Every instance this host created, indexed by [`HostInstance`] token.
    ///
    /// **Never removed from.** A token is an index, so removing an entry would make a later token
    /// name an earlier instance. Nothing removes today in any case: there is no
    /// `vkDestroyInstance` in the trait, for the reason this module's header gives.
    instances: Mutex<Vec<ash::Instance>>,
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
    /// Every surface this host created, indexed by [`HostSurface`] token.
    surfaces: Mutex<Vec<SurfaceEntry>>,
    /// Every logical device this host created, indexed by [`HostDevice`] token.
    devices: Mutex<Vec<DeviceEntry>>,
    /// Every queue this host has handed out, indexed by [`HostQueue`] token.
    queues: Mutex<Vec<QueueEntry>>,

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
    /// Which [`SurfaceEntry`] this was created over. Checked against `oldSwapchain`'s.
    surface: usize,
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
/// destroys anything — there is no `vkDestroyInstance`, `vkDestroySurfaceKHR` or
/// `vkDestroyDevice` in [`VulkanHost`]. A token was an index, and an index into a vector that only
/// grows is stable forever.
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
        // SAFETY: `Entry::load` dlopen's the Vulkan loader. It is unsafe because the library it
        // finds is chosen by the host's own search path and its entry points are trusted after
        // that; this is the same call `Renderer::new` makes, with the same standing.
        let entry = unsafe { ash::Entry::load() }
            .map_err(|err| GfxError::LoaderMissing { detail: err.to_string() })?;
        Ok(Arc::new(GfxVulkanHost {
            entry,
            instances: Mutex::new(Vec::new()),
            physical: Mutex::new(Vec::new()),
            surfaces: Mutex::new(Vec::new()),
            devices: Mutex::new(Vec::new()),
            queues: Mutex::new(Vec::new()),
            swapchains: Mutex::new(Slab::new()),
            images: Mutex::new(Slab::new()),
            image_views: Mutex::new(Slab::new()),
            semaphores: Mutex::new(Slab::new()),
            fences: Mutex::new(Slab::new()),
            command_pools: Mutex::new(Slab::new()),
            command_buffers: Mutex::new(Slab::new()),
        }))
    }

    /// How many instances this host has created. Diagnostic (Global Constraint 6).
    #[must_use]
    pub fn instances_created(&self) -> usize {
        self.locked().len()
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
    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<ash::Instance>> {
        self.instances.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `ash::Instance` a token names, or a refusal naming the token.
    fn lookup(
        instances: &[ash::Instance],
        instance: HostInstance,
    ) -> AbiResult<&ash::Instance> {
        usize::try_from(instance.token())
            .ok()
            .and_then(|index| instances.get(index))
            .ok_or_else(|| {
                refused(
                    "VulkanHost::lookup",
                    &format!(
                        "{instance:?} is not a token this host issued -- it has issued {} -- so \
                         there is no `VkInstance` to forward to. A token reaching here that this \
                         host did not mint means the loader's registry and this host disagree, \
                         which happens when `Vulkan::set_host` replaced one host with another \
                         while the guest still held a handle",
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

    /// How many surfaces, devices and queues this host holds. Diagnostic (Global Constraint 6).
    #[must_use]
    pub fn objects(&self) -> (usize, usize, usize) {
        (
            self.surfaces.lock().unwrap_or_else(PoisonError::into_inner).len(),
            self.devices.lock().unwrap_or_else(PoisonError::into_inner).len(),
            self.queues.lock().unwrap_or_else(PoisonError::into_inner).len(),
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

    fn locked_surfaces(&self) -> std::sync::MutexGuard<'_, Vec<SurfaceEntry>> {
        self.surfaces.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_devices(&self) -> std::sync::MutexGuard<'_, Vec<DeviceEntry>> {
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_queues(&self) -> std::sync::MutexGuard<'_, Vec<QueueEntry>> {
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
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
            let entry = surfaces
                .get(usize::try_from(surface_token.token()).unwrap_or(usize::MAX))
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::with_surface",
                        &format!(
                            "{surface_token:?} is not a surface this host created -- it has \
                             created {}",
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
        let entry = devices
            .get(usize::try_from(token.token()).unwrap_or(usize::MAX))
            .ok_or_else(|| {
                refused(
                    "VulkanHost::with_device",
                    &format!(
                        "{token:?} is not a device this host created -- it has created {}",
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
    /// The exceptions are named where they occur, and each holds two adjacent tables in the order
    /// above for the length of a `Vec` lookup and nothing else.
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
            let entry = devices.get(index).ok_or_else(|| {
                refused(
                    "VulkanHost::device_parts",
                    &format!(
                        "{token:?} is not a device this host created -- it has created {}",
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

    /// The `ash::Device` a device index names, cloned, with no lock held afterwards.
    fn device_at(&self, index: usize, method: &'static str) -> AbiResult<ash::Device> {
        let devices = self.locked_devices();
        devices.get(index).map(|entry| entry.device.clone()).ok_or_else(|| {
            refused(
                method,
                &format!("device #{index} is not one this host created -- it has {}", devices.len()),
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
        if instances.len() >= MAX_INSTANCES {
            return Err(refused(
                "vkCreateInstance",
                &format!(
                    "this host has already created {MAX_INSTANCES} instances and keeps every one \
                     of them, because a `HostInstance` token is an index into that table and \
                     reusing a slot would make an old token name a new instance. Nothing destroys \
                     an instance today: there is no `vkDestroyInstance` in `VulkanHost`, because \
                     the guest has never called one"
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
        let extension_pointers: Vec<*const std::ffi::c_char> =
            extensions.iter().map(|name| name.as_ptr()).collect();

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

        let mut info = vk::InstanceCreateInfo::default()
            .flags(vk::InstanceCreateFlags::from_raw(request.flags))
            .enabled_layer_names(&layer_pointers)
            .enabled_extension_names(&extension_pointers);
        if let Some(application_info) = application_info.as_ref() {
            info = info.application_info(application_info);
        }

        // SAFETY: every pointer reachable from `info` is into a local `CString` or a local `Vec`
        // that outlives this call, `pNext` is null because `InstanceRequest` cannot carry a chain
        // (the shim refuses one by name), and `pAllocator` is `None` because a guest allocator is
        // refused by name one layer up. Nothing here is a guest address.
        match unsafe { self.entry.create_instance(&info, None) } {
            Ok(instance) => {
                let token = instances.len() as u64;
                instances.push(instance);
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
                        let mut surfaces = self.locked_surfaces();
                        let token = surfaces.len() as u64;
                        surfaces.push(SurfaceEntry { instance: index, surface, window: key });
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
            other => Err(refused(
                "vkCreateAndroidSurfaceKHR",
                &format!(
                    "the guest's `ANativeWindow *` resolved to a {system} window, and this host \
                     has no Vulkan surface call for that window system -- only \
                     `VK_KHR_win32_surface`. This is a refusal rather than a `VkResult` because \
                     the specification has no code for \"this build of the host cannot make a \
                     surface here\", and `VK_ERROR_INITIALIZATION_FAILED` would send the engine \
                     looking at its driver. `crate::vulkan::create_surface` is the other place \
                     that grows when `RawWindow` gains a variant",
                    system = other.system_name()
                ),
            )),
        }
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
        let extension_pointers: Vec<*const std::ffi::c_char> =
            extensions.iter().map(|name| name.as_ptr()).collect();
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

        let created = self.with_physical(device, |instance, physical| {
            // SAFETY: `physical` is a live device of `instance`; every pointer reachable from
            // `info` is into a local that outlives this call; `pNext` is null because
            // `DeviceRequest` cannot carry a chain (the shim refuses one by name); `pAllocator` is
            // `None` because a guest allocator is refused by name one layer up.
            (unsafe { instance.create_device(physical, &info, None) }, physical)
        })?;
        match created {
            (Ok(device), physical) => {
                let mut devices = self.locked_devices();
                let token = devices.len() as u64;
                devices.push(DeviceEntry {
                    instance: instance_index,
                    physical,
                    device,
                    requested,
                });
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
        if let Some(position) = queues.iter().position(|entry| {
            entry.device == device_index && entry.family == family && entry.index == index
        }) {
            // **And the driver is held to its own contract.** `vkGetDeviceQueue` for one family
            // and index must produce the same `VkQueue` every time; if it did not, the handle the
            // guest already holds would name a different queue from the one it would get now, and
            // every later comparison the renderer makes between its graphics and present queues
            // would be answering about the wrong pair. Nothing has ever seen this happen, which
            // is precisely why it is checked rather than assumed.
            if queues[position].queue != queue {
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
        queues.push(QueueEntry { device: device_index, family, index, queue });
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

        let (surface_index, surface, window) = {
            let surfaces = self.locked_surfaces();
            let index = usize::try_from(surface_token.token()).unwrap_or(usize::MAX);
            let entry = surfaces.get(index).ok_or_else(|| {
                refused(
                    "vkCreateSwapchainKHR",
                    &format!(
                        "{surface_token:?} is not a surface this host created -- it has created {}",
                        surfaces.len()
                    ),
                )
            })?;
            (index, entry.surface, entry.window)
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
                if entry.surface != surface_index {
                    return Err(refused(
                        "vkCreateSwapchainKHR",
                        &format!(
                            "{old:?} was passed as `oldSwapchain` and belongs to surface \
                             #{had}, while the swapchain being created is over surface \
                             #{surface_index}. The specification requires them to be the same \
                             surface -- retiring a swapchain on one window in order to create one \
                             on another would release the first window's claim and take the \
                             second's, and neither is what the caller asked for",
                            had = entry.surface
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
            surface: surface_index,
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
            let entry = queues
                .get(usize::try_from(queue.token()).unwrap_or(usize::MAX))
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_submit",
                        &format!(
                            "{queue:?} is not a queue this host handed out -- it has handed out {}",
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
            let entry = queues
                .get(usize::try_from(queue.token()).unwrap_or(usize::MAX))
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_present",
                        &format!(
                            "{queue:?} is not a queue this host handed out -- it has handed out {}",
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
            let entry = queues
                .get(usize::try_from(queue.token()).unwrap_or(usize::MAX))
                .ok_or_else(|| {
                    refused(
                        "VulkanHost::queue_wait_idle",
                        &format!(
                            "{queue:?} is not a queue this host handed out -- it has handed out {}",
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
}

/// The name [`crate::claim`] records the guest's swapchain under.
///
/// A `&'static str` the refusal quotes, so that an embedding reading "the guest's
/// vkCreateSwapchainKHR already owns this window" knows which of its two Vulkan stacks to change.
const GUEST_SWAPCHAIN_OWNER: &str = "the guest's vkCreateSwapchainKHR";

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
}

/// The refusal an object from the wrong device produces.
///
/// **A check this host is the only thing that can make.** Submitting a command buffer or waiting
/// on a fence that belongs to a different `VkDevice` is undefined behaviour the specification does
/// not require a driver to detect, this machine has no validation layers
/// (`docs/research/graphics-spike.md` §6), and the guest can reach it with two handles it was
/// legitimately given — exactly the shape `with_surface`'s instance check catches one stage
/// earlier.
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
            for entry in devices.iter() {
                // SAFETY: the device is live and this host created it.
                let _ = unsafe { entry.device.device_wait_idle() };
            }
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

        let devices = std::mem::take(&mut *self.locked_devices());
        for entry in devices {
            // SAFETY: the device is live and this host created it. The wait is what makes the
            // destroy below legal if the guest ever submitted work through it.
            let _ = unsafe { entry.device.device_wait_idle() };
            // SAFETY: every object made from this device has gone away with it -- nothing in this
            // file creates one -- and `pAllocator` was `None` at creation.
            unsafe { entry.device.destroy_device(None) };
        }

        let instances = std::mem::take(&mut *self.locked());
        let surfaces = std::mem::take(&mut *self.locked_surfaces());
        for surface in surfaces {
            let Some(instance) = instances.get(surface.instance) else { continue };
            let surface_fn = khr::surface::Instance::new(&self.entry, instance);
            // SAFETY: the surface is live, it was created from this instance, every swapchain
            // made from it is gone (this file creates none), and `pAllocator` was `None`.
            unsafe { surface_fn.destroy_surface(surface.surface, None) };
        }

        for instance in instances {
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
        write!(f, "GfxVulkanHost {{ {} instance(s) created }}", self.locked().len())
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
