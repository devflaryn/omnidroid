//! The Vulkan renderer: instance, device, surface, swapchain, and one frame on the screen.
//!
//! # What this is and what it deliberately is not
//!
//! D8 rules that Omnidroid's primary graphics path gives the guest a `libvulkan.so` that
//! **forwards** to the host driver — `libroblox.so` reaches Vulkan through `dlopen` only (593
//! `vk*` name strings, zero `vk*` imports), so Omnidroid controls it by supplying the library the
//! engine opens. That forwarding layer is not this file. This file is the thing underneath it:
//! the renderer that owns the window's surface and its swapchain, so that there is a real device,
//! a real present queue and a real frame loop for the forwarding layer to be built against.
//!
//! **There are no shaders here, and that is a decision rather than an omission.** The two present
//! paths are `vkCmdClearColorImage` and `vkCmdBlitImage`, which need no `VkPipeline`, no
//! `VkRenderPass`, no `VkShaderModule` and therefore no SPIR-V — so this crate needs no shader
//! compiler in its build, which the graphics spike §2 found is a genuine cost to take on
//! (`shaderc` needs a working CMake + MSVC + Ninja chain; `naga` works but is another dependency
//! and another SPIR-V producer). D8 records that Roblox ships **1,364 SPIR-V modules** of its own
//! in a 14.7 MB STORED pack, so the shaders this project will run are the guest's, arriving
//! already compiled. A triangle of our own would have been a third source of SPIR-V and would
//! have proved nothing this does not.
//!
//! # Assume nothing about the driver's opinion of your mistakes
//!
//! This host has **no validation layers**: `vkEnumerateInstanceLayerProperties` reports five, and
//! `VK_LAYER_KHRONOS_validation` is not one of them (`docs/research/graphics-spike.md` §6).
//! [`Renderer::new`] enables it when it *is* present and never requires it — but the code here is
//! written as if nothing will ever tell it that it is wrong, because on this machine nothing will.
//! The spike's evidence for what that costs is direct: its first `recreate_swapchain` destroyed
//! the outgoing `VkSwapchainKHR` before passing it as `oldSwapchain`, and instead of a validation
//! error it **crashed the NVIDIA driver on the very first live resize, every time**, diagnosable
//! only from the Windows Event Log (`nvoglv64.dll`, `0xc0000409`). `Renderer::recreate_swapchain`
//! is written to that measurement.
//!
//! # The frame loop's shape
//!
//! Two frames in flight ([`MAX_FRAMES_IN_FLIGHT`]), a fence per frame, an acquire semaphore per
//! frame, and a render-finished semaphore **per swapchain image** rather than per frame. That last
//! asymmetry is the one people get wrong: `vkQueuePresentKHR` waits on a semaphore that the
//! presentation engine holds until it is done with *that image*, so a semaphore indexed by frame
//! can be re-signalled while the presentation engine is still waiting on it.

use std::ffi::{CStr, CString};

use ash::khr;
use ash::vk;
use omni_platform::window::RawWindow;

use crate::claim;
use crate::portability;
use crate::error::{GfxError, GfxResult};
use crate::image::Rgba8Image;
use crate::select::{self, PresentMode};

/// How many frames may be recorded and submitted before the oldest must have completed.
///
/// Two: one being presented while one is being built. Three would add a frame of latency for a
/// throughput this renderer does not need — the spike measured submission overhead at **0.20 ms
/// mean per frame in MAILBOX (n = 300)**, so nothing here is submission-bound.
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// The layer enabled when the host has it. Absent on this host; see the module header.
const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";

/// The format the guest's frames are staged in before being blitted to the swapchain.
///
/// `R8G8B8A8_UNORM` because that is byte-for-byte what `omni_texture` decodes ETC1 into (D27), so
/// the upload is a `memcpy` and not a swizzle. The specification requires every implementation to
/// support it as a blit source in optimal tiling with linear filtering, which is what makes
/// [`select::blit_filter`]'s fallback arm unreachable on any conformant driver and still worth
/// having.
const STAGING_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// What a call to [`Renderer::present_clear`] or [`Renderer::present_rgba8`] actually did.
///
/// Presenting is not something that either succeeds or fails: a minimised window has nowhere to
/// put a frame and a resized one needs its swapchain rebuilt first, and neither is an error. A
/// `Result<(), _>` would have to encode both as failures, and a caller that treated them as such
/// would tear the renderer down the first time the user clicked minimise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameOutcome {
    /// A frame was submitted and handed to the presentation engine.
    Presented,
    /// The swapchain was out of date and has been rebuilt. **No frame was presented**; call again.
    ///
    /// Separate from [`FrameOutcome::Presented`] because rebuilding is the expensive, dangerous
    /// operation in this file and a caller counting frames must not count one that did not happen.
    /// [`Renderer::swapchain_generations`] counts these.
    SwapchainRecreated,
    /// There is nothing to present to: the window has a zero-pixel client area, i.e. it is
    /// minimised. Not an error, and not a state to spin on — the caller should keep polling its
    /// window and try again when it reports a size.
    Skipped,
}

/// How the renderer should be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RendererConfig {
    /// Which presentation mode to ask the surface for. Degrades to FIFO when unavailable; see
    /// [`select::present_mode`].
    pub present_mode: PresentMode,
}

/// What the renderer chose, and what it was choosing from.
///
/// Public and carried on the renderer because Global Constraint 6 requires that a test be able to
/// **assert** what was chosen rather than assume it, and because D8's most awkward constraint is
/// about this data: `libroblox.so` refuses a device whose name matches its emulated/blacklisted
/// patterns, so what got picked is a fact the runtime above this has to be able to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceReport {
    /// The device's own reported name, e.g. `NVIDIA GeForce RTX 4060`.
    pub name: String,
    /// Its `VkPhysicalDeviceType`, as reported. Genuine, because this forwards to a real driver.
    pub device_type: vk::PhysicalDeviceType,
    /// The queue family used for rendering.
    pub graphics_family: u32,
    /// The queue family used for presenting. Equal to `graphics_family` on this host, which
    /// reports six families of which family 0 does graphics, compute and transfer (spike §3).
    pub present_family: u32,
    /// Every instance layer the loader reported, whether or not it was enabled. Kept so that
    /// [`Renderer::validation_enabled`] can be checked *against* it rather than believed.
    pub available_layers: Vec<String>,
    /// Whether `VK_LAYER_KHRONOS_validation` was found and enabled.
    pub validation_enabled: bool,
    /// **What a portability implementation leaves out**, by the specification's feature names:
    /// `None` for a device that is not one (every native driver), otherwise the
    /// `VkPhysicalDevicePortabilitySubsetFeaturesKHR` members it reports false. Everything it
    /// reports true is enabled on the device, and nothing else is.
    pub portability_gaps: Option<Vec<&'static str>>,
}

/// One in-flight frame's own objects.
struct Frame {
    command_buffer: vk::CommandBuffer,
    /// Signalled by `vkAcquireNextImageKHR`, waited on by the submission. Per **frame**, because
    /// the acquire happens before the image index is known.
    image_available: vk::Semaphore,
    /// Signalled when this frame's submission completes, so the next use of this frame's command
    /// buffer can know it is free.
    in_flight: vk::Fence,
}

/// The swapchain and everything whose lifetime is tied to it.
struct Swapchain {
    handle: vk::SwapchainKHR,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    /// Waited on by `vkQueuePresentKHR`. Per **image**; see the module header for why.
    render_finished: Vec<vk::Semaphore>,
    /// The fence of the frame that last submitted work touching each image, or null. Borrowed
    /// from [`Frame::in_flight`] and never destroyed through this vector.
    image_in_flight: Vec<vk::Fence>,
}

/// The staging pair the guest's pixels travel through: a mapped buffer and a device-local image.
///
/// # Why two objects and not one
///
/// The obvious shortcut is `vkCmdCopyBufferToImage` straight into the swapchain image, which works
/// only while the guest's frame is exactly the window's size. The moment it is not — and it will
/// not be, since the guest picks its own surface size — the copy has to become a **blit**, and
/// `vkCmdBlitImage`'s source must be a `VkImage`. One path that always works beats two paths where
/// the rarely-taken one is the one nobody tests (VERIFICATION entry 12).
///
/// The image is device-local rather than host-visible for the reason the spike measured: only
/// **~214 MiB** of this host's memory is both host-visible and device-local (§3), against a
/// 7.77 GiB device-local heap, so an upload path that assumed it could map VRAM would work until
/// a frame did not fit.
struct Staging {
    width: u32,
    height: u32,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    /// The persistent mapping of `buffer_memory`. Mapped once rather than per frame: the mapping
    /// is host-coherent, so there is nothing to flush, and `vkMapMemory` per frame is a driver
    /// call per frame for an address that never changes.
    mapped: *mut u8,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
}

/// The objects that exist before a device does, with a destructor.
///
/// # Why this is a type and not four fields
///
/// Constructing a renderer creates nine kinds of Vulkan object in sequence, and any of them can
/// fail. Written as a flat function, each failure has to destroy a different prefix of what
/// exists -- nine hand-written teardown paths, of which the live one is whichever failed, i.e.
/// none of them on a working machine. That is VERIFICATION entry 12's shape exactly: code that
/// reads as careful and that no test reaches.
///
/// Splitting the object graph into two `Drop` types instead makes the teardowns *structural*.
/// `?` on any step drops what has been built so far, in reverse order, because that is what Rust
/// does -- and the same code runs in `Renderer::drop`, so the teardown that runs after a
/// successful run is the same one that runs after a failure.
///
/// Field order is destruction order, and it is Vulkan's required order: the surface before the
/// instance that owns it.
struct Base {
    surface: vk::SurfaceKHR,
    surface_fn: khr::surface::Instance,
    instance: ash::Instance,
    /// The loader. Last, because everything above was loaded through it.
    entry: ash::Entry,
}

impl Drop for Base {
    fn drop(&mut self) {
        // SAFETY: a `Base` is only ever dropped once every object created from its instance has
        // been destroyed -- which the field order of `Renderer` and the `?` ordering in
        // `Renderer::new` are what guarantee. Destroying a null surface handle is explicitly
        // permitted, which is what makes the two-step construction in `new` sound.
        unsafe {
            self.surface_fn.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

/// The logical device and the per-frame objects allocated from it, with a destructor.
///
/// See [`Base`] for why these are `Drop` types rather than fields.
struct DeviceOwner {
    frames: Vec<Frame>,
    command_pool: vk::CommandPool,
    device: ash::Device,
}

impl Drop for DeviceOwner {
    fn drop(&mut self) {
        // SAFETY: waiting idle is what makes every destruction below legal -- no submitted work
        // can still reference these objects afterwards. The result is discarded because the only
        // failure is `VK_ERROR_DEVICE_LOST`, in which case the device is already gone and
        // destroying anyway is still the correct thing to do. Destroying the pool frees the
        // command buffers allocated from it, which is why they are not freed individually.
        unsafe {
            let _ = self.device.device_wait_idle();
            for frame in self.frames.drain(..) {
                self.device.destroy_semaphore(frame.image_available, None);
                self.device.destroy_fence(frame.in_flight, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
        }
    }
}

/// A Vulkan renderer presenting to one window.
///
/// Created from a [`RawWindow`] and torn down before it. The renderer is **not** `Send`: it holds
/// a persistent memory mapping, and its surface belongs to a window that Win32 will only deliver
/// messages for on one thread anyway.
pub struct Renderer {
    /// Destroyed explicitly by [`Renderer::drop`], because both need the device and both are
    /// `Option`s that the later teardown steps must find empty.
    swapchain: Option<Swapchain>,
    staging: Option<Staging>,

    frame_index: usize,
    swapchain_fn: khr::swapchain::Device,
    graphics_queue: vk::Queue,
    present_queue: vk::Queue,

    physical_device: vk::PhysicalDevice,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    report: DeviceReport,

    present_mode: vk::PresentModeKHR,
    /// The client size the window last reported. Used only when the surface declines to state its
    /// own extent; see [`select::clamp_extent`].
    target_extent: (u32, u32),
    /// Set when something observed that the swapchain no longer matches the surface -- a resize
    /// event, or a `VK_SUBOPTIMAL_KHR` from acquire or present.
    swapchain_dirty: bool,
    /// Whether a zero-pixel [`notify_resized`](Renderer::notify_resized) means *no swapchain*
    /// even when the surface states an extent of its own, as [`zero_size_is_the_windows`] decides
    /// for this window's system.
    zero_size_is_the_windows: bool,

    frames_presented: u64,
    swapchain_generations: u64,

    /// The two owning halves, **last but one**, so that they are destroyed after everything that
    /// was created from them. Rust drops fields in declaration order and Vulkan requires the
    /// reverse of creation order; putting them here is what reconciles the two.
    dev: DeviceOwner,
    base: Base,

    /// This window's exclusive swapchain claim, held for as long as the renderer is.
    ///
    /// **Last of all**, so that the window is released only after this renderer's swapchain has
    /// actually been destroyed. A claim dropped before `Base` would leave a window that the guest
    /// could claim while a `VkSwapchainKHR` of ours was still on it, which is the exact state
    /// [`claim`](crate::claim) exists to make impossible.
    ///
    /// Never read. Its whole job is to exist and then to be dropped; the leading underscore says
    /// so to the compiler and this paragraph says so to a reader.
    _window: claim::WindowClaim,
}

impl Renderer {
    /// Build a renderer for `window`, whose client area is currently `extent` physical pixels.
    ///
    /// # Errors
    ///
    /// [`GfxError::LoaderMissing`] when the host has no Vulkan runtime at all -- which is a
    /// *message* rather than a link failure only because this crate loads Vulkan at run time; see
    /// `Cargo.toml`. [`GfxError::UnsupportedWindowSystem`] for a window this renderer has no
    /// surface code for, [`GfxError::NoUsableDevice`] when nothing can both render and present,
    /// and [`GfxError::Vulkan`] for anything the driver refused.
    ///
    /// Every one of those leaves nothing behind; see this module's `Base` for how, and for why it is worth a
    /// type rather than nine teardown paths nobody runs.
    pub fn new(window: RawWindow, extent: (u32, u32), config: RendererConfig) -> GfxResult<Self> {
        // `Entry::load()` where the platform names no loader locations, which is the Windows
        // behaviour unchanged; otherwise each location in turn. See `crate::portability`.
        let entry = portability::load_entry().map_err(|detail| GfxError::LoaderMissing { detail })?;

        // **The window is claimed before anything is created on it**, so that a conflict with
        // `omni_gfx`'s other Vulkan stack -- or with the guest's -- is a named refusal rather than
        // a second swapchain a driver may or may not object to. `claim` carries the argument;
        // `window_key` is what says this renderer knows which window system it is on.
        let claim = claim::claim_window(window_key(window)?, "omni_gfx::Renderer").map_err(
            |claimed| GfxError::WindowInUse { owner: claimed.owner, window: claimed.window.raw() },
        )?;

        let (instance, available_layers, validation_enabled, features2) = create_instance(&entry, window)?;
        let surface_fn = khr::surface::Instance::new(&entry, &instance);
        // From here on, every `?` unwinds through `Base::drop`.
        let mut base = Base { surface: vk::SurfaceKHR::null(), surface_fn, instance, entry };
        base.surface = create_surface(&base.entry, &base.instance, window)?;

        let PickedDevice { physical_device, graphics_family, present_family, name, device_type } =
            pick_physical_device(&base.instance, &base.surface_fn, base.surface)?;
        let mut report = DeviceReport {
            name,
            device_type,
            graphics_family,
            present_family,
            available_layers,
            validation_enabled,
            portability_gaps: None,
        };

        let (device, portability_gaps) = create_device(
            &base.entry,
            &base.instance,
            physical_device,
            graphics_family,
            present_family,
            features2,
        )?;
        report.portability_gaps = portability_gaps;
        // A pool per renderer, with `RESET_COMMAND_BUFFER` so that each frame's buffer is
        // re-recorded in place rather than freed and reallocated.
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(graphics_family);
        // SAFETY: the device is live and `pool_info` outlives the call.
        let command_pool = match unsafe { device.create_command_pool(&pool_info, None) } {
            Ok(pool) => pool,
            Err(result) => {
                // The one teardown here that cannot be structural: `DeviceOwner` does not exist
                // yet, because building it is what failed.
                // SAFETY: the device is live and nothing has been created from it.
                unsafe { device.destroy_device(None) };
                return Err(GfxError::vk("create", "vkCreateCommandPool")(result));
            }
        };
        let mut dev = DeviceOwner { frames: Vec::new(), command_pool, device };
        dev.frames = create_frames(&dev.device, command_pool)?;

        // SAFETY: both family indices came from `pick_physical_device`, which read them out of
        // this device's own queue-family list, and `create_device` requested one queue of each.
        let (graphics_queue, present_queue) = unsafe {
            (
                dev.device.get_device_queue(graphics_family, 0),
                dev.device.get_device_queue(present_family, 0),
            )
        };
        let swapchain_fn = khr::swapchain::Device::new(&base.instance, &dev.device);
        // SAFETY: `physical_device` is live for as long as the instance is.
        let memory_properties =
            unsafe { base.instance.get_physical_device_memory_properties(physical_device) };

        // The present mode is chosen once, from what this surface offers, and then kept: changing
        // it later would mean a swapchain recreation for a reason the user cannot see.
        // SAFETY: the physical device and the surface are both live.
        let modes = unsafe {
            base.surface_fn
                .get_physical_device_surface_present_modes(physical_device, base.surface)
        }
        .map_err(GfxError::vk("create", "vkGetPhysicalDeviceSurfacePresentModesKHR"))?;

        let mut renderer = Renderer {
            swapchain: None,
            staging: None,
            frame_index: 0,
            swapchain_fn,
            graphics_queue,
            present_queue,
            physical_device,
            memory_properties,
            report,
            present_mode: select::present_mode(config.present_mode, &modes),
            target_extent: extent,
            swapchain_dirty: false,
            zero_size_is_the_windows: zero_size_is_the_windows(window),
            frames_presented: 0,
            swapchain_generations: 0,
            dev,
            base,
            _window: claim,
        };
        renderer.recreate_swapchain()?;
        Ok(renderer)
    }

    /// Tell the renderer the window's client area changed.
    ///
    /// Does not rebuild anything: the rebuild happens inside the next present, so that a burst of
    /// resize events costs one recreation rather than one each. Call it for every
    /// [`Resized`](omni_platform::window::WindowEvent::Resized) event; it is cheap and idempotent.
    ///
    /// A zero extent — a minimised window — is a legal argument and makes the next present return
    /// [`FrameOutcome::Skipped`].
    pub fn notify_resized(&mut self, width: u32, height: u32) {
        self.target_extent = (width, height);
        self.swapchain_dirty = true;
    }

    /// Clear the whole window to `color` and present it. `color` is RGBA, each channel `0.0..=1.0`.
    ///
    /// # Errors
    ///
    /// [`GfxError::Vulkan`] for anything the driver refused. A minimised window and an out-of-date
    /// swapchain are **not** errors; see [`FrameOutcome`].
    pub fn present_clear(&mut self, color: [f32; 4]) -> GfxResult<FrameOutcome> {
        self.frame(None, color)
    }

    /// Blit `image` across the whole window and present it.
    ///
    /// The image is scaled to the window, with [`select::blit_filter`]'s filter, and its aspect
    /// ratio is **not** preserved — this presents a guest frame, and the guest is told the surface
    /// size, so a mismatch means the guest has not caught up with a resize yet and stretching for
    /// one frame is the right answer.
    ///
    /// # Errors
    ///
    /// [`GfxError::Vulkan`] for a driver refusal, and [`GfxError::NoUsableMemoryType`] if the
    /// staging pair cannot be allocated. Length and extent problems were already refused by
    /// [`Rgba8Image::new`].
    pub fn present_rgba8(&mut self, image: &Rgba8Image<'_>) -> GfxResult<FrameOutcome> {
        self.frame(Some(image), [0.0; 4])
    }

    /// The swapchain's current extent, or `None` when there is no swapchain because the window has
    /// no pixels.
    #[must_use]
    pub fn swapchain_extent(&self) -> Option<(u32, u32)> {
        self.swapchain.as_ref().map(|s| (s.extent.width, s.extent.height))
    }

    /// The format the swapchain images were created with, or `None` when there is no swapchain.
    ///
    /// Exposed because [`select::surface_format`] makes a choice whose consequence is *visible*
    /// and whose wrongness is not obvious — an `_SRGB` swapchain here would double-encode every
    /// pixel the guest hands over — so a test has to be able to assert what was chosen against a
    /// real surface's real list, not only against the synthetic ones `select`'s own tests use.
    #[must_use]
    pub fn swapchain_format(&self) -> Option<vk::Format> {
        self.swapchain.as_ref().map(|s| s.format)
    }

    /// What this renderer chose, and what it chose from.
    #[must_use]
    pub fn device(&self) -> &DeviceReport {
        &self.report
    }

    /// Whether `VK_LAYER_KHRONOS_validation` was available and is enabled.
    ///
    /// False on this host, measured — the loader reports five layers and validation is not among
    /// them (`docs/research/graphics-spike.md` §6). Exposed rather than assumed so that a test can
    /// assert the *relation* (enabled exactly when available) instead of pinning the host fact,
    /// which changes the day someone follows that document's first recommendation and installs it.
    #[must_use]
    pub fn validation_enabled(&self) -> bool {
        self.report.validation_enabled
    }

    /// The present mode the swapchain was actually created with, which may be the FIFO fallback
    /// rather than what [`RendererConfig`] asked for.
    #[must_use]
    pub fn present_mode(&self) -> vk::PresentModeKHR {
        self.present_mode
    }

    /// How many frames have reached `vkQueuePresentKHR`. Diagnostic; Global Constraint 6.
    #[must_use]
    pub fn frames_presented(&self) -> u64 {
        self.frames_presented
    }

    /// How many swapchains have been created, including the first.
    ///
    /// The number that matters for the spike's finding: swapchain recreation is where the driver
    /// crashed, so a test asserting that a resize caused **exactly one** recreation is asserting
    /// that the renderer is not silently rebuilding every frame.
    #[must_use]
    pub fn swapchain_generations(&self) -> u64 {
        self.swapchain_generations
    }

    /// Record and submit one frame.
    ///
    /// `image` present means blit it; absent means clear to `color`.
    fn frame(
        &mut self,
        image: Option<&Rgba8Image<'_>>,
        color: [f32; 4],
    ) -> GfxResult<FrameOutcome> {
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.recreate_swapchain()?;
            if self.swapchain.is_none() {
                return Ok(FrameOutcome::Skipped);
            }
            return Ok(FrameOutcome::SwapchainRecreated);
        }

        let frame = &self.dev.frames[self.frame_index];
        let (command_buffer, image_available, in_flight) =
            (frame.command_buffer, frame.image_available, frame.in_flight);

        // SAFETY: `in_flight` belongs to this renderer's device and was created signalled, so the
        // first wait returns immediately rather than deadlocking on a fence nothing will signal.
        unsafe { self.dev.device.wait_for_fences(&[in_flight], true, u64::MAX) }
            .map_err(GfxError::vk("present", "vkWaitForFences"))?;

        let swapchain = self.swapchain.as_ref().expect("checked non-None at the top of this call");
        // SAFETY: the swapchain and semaphore are live and belong to this device.
        let acquired = unsafe {
            self.swapchain_fn.acquire_next_image(
                swapchain.handle,
                u64::MAX,
                image_available,
                vk::Fence::null(),
            )
        };
        let (index, suboptimal) = match acquired {
            Ok(pair) => pair,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                // The acquire failed, so it signalled nothing: `image_available` is still
                // unsignalled and can be reused by the next attempt. That is why this arm can
                // simply rebuild and return rather than having to drain a stuck semaphore.
                self.swapchain_dirty = true;
                self.recreate_swapchain()?;
                return Ok(FrameOutcome::SwapchainRecreated);
            }
            Err(result) => {
                return Err(GfxError::vk("present", "vkAcquireNextImageKHR")(result));
            }
        };
        let index_usize = index as usize;

        // Another frame may still be using this image. Its fence is borrowed, never owned here.
        let previous = self.swapchain.as_ref().expect("still present").image_in_flight[index_usize];
        if previous != vk::Fence::null() {
            // SAFETY: the fence belongs to one of this renderer's frames and is live.
            unsafe { self.dev.device.wait_for_fences(&[previous], true, u64::MAX) }
                .map_err(GfxError::vk("present", "vkWaitForFences"))?;
        }

        // Everything the recording needs, copied out before `self` is borrowed mutably by the
        // staging upload below.
        let (target_image, target_extent, render_finished) = {
            let swapchain = self.swapchain.as_ref().expect("still present");
            (
                swapchain.images[index_usize],
                swapchain.extent,
                swapchain.render_finished[index_usize],
            )
        };

        let blit = match image {
            Some(image) => Some(self.upload_staging(image)?),
            None => None,
        };

        // SAFETY: `in_flight` is not in use — the wait above returned — so resetting it and
        // re-recording the command buffer it guards is safe.
        unsafe { self.dev.device.reset_fences(&[in_flight]) }
            .map_err(GfxError::vk("present", "vkResetFences"))?;

        self.record(command_buffer, target_image, target_extent, blit, color)?;

        if let Some(swapchain) = self.swapchain.as_mut() {
            swapchain.image_in_flight[index_usize] = in_flight;
        }

        let wait = [image_available];
        let wait_stages = [vk::PipelineStageFlags::TRANSFER];
        let buffers = [command_buffer];
        let signal = [render_finished];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&buffers)
            .signal_semaphores(&signal);
        // SAFETY: every handle in `submit` belongs to this device and outlives the call; the
        // command buffer was just recorded and is not in use, because `in_flight` guards it.
        unsafe { self.dev.device.queue_submit(self.graphics_queue, &[submit], in_flight) }
            .map_err(GfxError::vk("present", "vkQueueSubmit"))?;

        let swapchains = [self.swapchain.as_ref().expect("still present").handle];
        let indices = [index];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal)
            .swapchains(&swapchains)
            .image_indices(&indices);
        // SAFETY: the swapchain, the semaphore and the index all belong to this renderer, and the
        // index came from the acquire above.
        let presented = unsafe { self.swapchain_fn.queue_present(self.present_queue, &present_info) };
        match presented {
            Ok(present_suboptimal) => {
                // `VK_SUBOPTIMAL_KHR` from either call means the swapchain still works and no
                // longer matches the surface. The frame is presented anyway — it is a *correct*
                // frame, merely not an optimal one — and the rebuild happens on the next call.
                // Rebuilding here instead would throw away a frame that already reached the
                // screen.
                self.swapchain_dirty |= suboptimal | present_suboptimal;
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.swapchain_dirty = true,
            Err(result) => return Err(GfxError::vk("present", "vkQueuePresentKHR")(result)),
        }

        self.frames_presented += 1;
        self.frame_index = (self.frame_index + 1) % MAX_FRAMES_IN_FLIGHT;
        Ok(FrameOutcome::Presented)
    }

    /// Record the frame's one command buffer.
    fn record(
        &self,
        command_buffer: vk::CommandBuffer,
        target: vk::Image,
        extent: vk::Extent2D,
        blit: Option<BlitSource>,
        color: [f32; 4],
    ) -> GfxResult<()> {
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the buffer is not in use (its frame's fence has been waited on), the pool was
        // created with `RESET_COMMAND_BUFFER` so beginning implicitly resets it, and every handle
        // recorded below belongs to this device. Grouped into one block because splitting a
        // recording sequence into twelve `unsafe` blocks documents nothing that the sequence as a
        // whole does not.
        unsafe {
            self.dev.device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(GfxError::vk("present", "vkBeginCommandBuffer"))?;

            barrier(
                &self.dev.device,
                command_buffer,
                target,
                Transition::DISCARD_TO_TRANSFER_DST,
            );

            match blit {
                None => {
                    let clear = vk::ClearColorValue { float32: color };
                    self.dev.device.cmd_clear_color_image(
                        command_buffer,
                        target,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &clear,
                        &[whole_colour_range()],
                    );
                }
                Some(source) => {
                    // The staging image's contents from the previous frame are irrelevant,
                    // and the copy below overwrites all of it.
                    barrier(
                        &self.dev.device,
                        command_buffer,
                        source.image,
                        Transition::DISCARD_TO_TRANSFER_DST,
                    );
                    let region = vk::BufferImageCopy::default()
                        .image_subresource(colour_layers())
                        .image_extent(vk::Extent3D {
                            width: source.width,
                            height: source.height,
                            depth: 1,
                        });
                    self.dev.device.cmd_copy_buffer_to_image(
                        command_buffer,
                        source.buffer,
                        source.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    barrier(
                        &self.dev.device,
                        command_buffer,
                        source.image,
                        Transition::TRANSFER_DST_TO_SRC,
                    );
                    let region = vk::ImageBlit::default()
                        .src_subresource(colour_layers())
                        .src_offsets([
                            vk::Offset3D { x: 0, y: 0, z: 0 },
                            vk::Offset3D {
                                x: source.width as i32,
                                y: source.height as i32,
                                z: 1,
                            },
                        ])
                        .dst_subresource(colour_layers())
                        .dst_offsets([
                            vk::Offset3D { x: 0, y: 0, z: 0 },
                            vk::Offset3D {
                                x: extent.width as i32,
                                y: extent.height as i32,
                                z: 1,
                            },
                        ]);
                    self.dev.device.cmd_blit_image(
                        command_buffer,
                        source.image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        target,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                        source.filter,
                    );
                }
            }

            barrier(
                &self.dev.device,
                command_buffer,
                target,
                Transition::TRANSFER_DST_TO_PRESENT,
            );

            self.dev.device
                .end_command_buffer(command_buffer)
                .map_err(GfxError::vk("present", "vkEndCommandBuffer"))?;
        }
        Ok(())
    }

    /// Copy the guest's pixels into the mapped staging buffer, allocating the staging pair if its
    /// dimensions changed.
    fn upload_staging(&mut self, image: &Rgba8Image<'_>) -> GfxResult<BlitSource> {
        let stale = self
            .staging
            .as_ref()
            .is_none_or(|s| s.width != image.width() || s.height != image.height());
        if stale {
            // SAFETY: the device is live. Waiting idle is what makes it safe to destroy the old
            // staging pair: a frame in flight may still be reading it.
            unsafe { self.dev.device.device_wait_idle() }
                .map_err(GfxError::vk("present_rgba8", "vkDeviceWaitIdle"))?;
            self.destroy_staging();
            self.staging = Some(create_staging(
                &self.dev.device,
                &self.memory_properties,
                image.width(),
                image.height(),
            )?);
        }
        let staging = self.staging.as_ref().expect("just created if it was missing");

        // SAFETY: `mapped` is a persistent mapping of `buffer_memory`, which was allocated with at
        // least `pixels.len()` bytes for exactly these dimensions, and the memory is
        // `HOST_COHERENT`, so no flush is needed. The GPU is not reading it: this frame's fence
        // has been waited on and the staging pair is used by no other frame.
        unsafe {
            core::ptr::copy_nonoverlapping(
                image.pixels().as_ptr(),
                staging.mapped,
                image.pixels().len(),
            );
        }

        // SAFETY: the physical device is live.
        let features = unsafe {
            self.base.instance
                .get_physical_device_format_properties(self.physical_device, STAGING_FORMAT)
        }
        .optimal_tiling_features;
        Ok(BlitSource {
            buffer: staging.buffer,
            image: staging.image,
            width: staging.width,
            height: staging.height,
            filter: select::blit_filter(features),
        })
    }

    /// Destroy the swapchain and build a new one for the surface's current extent.
    ///
    /// # The `oldSwapchain` lifetime, which is the one measured bug in this file
    ///
    /// The graphics spike's first version of this function destroyed the outgoing
    /// `VkSwapchainKHR` **before** passing it as `oldSwapchain` to the replacement
    /// `vkCreateSwapchainKHR`. On this host, with no validation layers, that did not produce a
    /// warning: it **crashed the NVIDIA driver on the first live resize, every time**
    /// (`nvoglv64.dll`, `0xc0000409`, from the Windows Event Log — `docs/research/graphics-spike.md`
    /// §1). The order below is the fix that survived six consecutive live resizes from 500x400 to
    /// 1250x900 with zero crash events: **create the new swapchain first, naming the old handle,
    /// and destroy the old handle only afterwards.**
    ///
    /// `vkDeviceWaitIdle` first, rather than tracking which frames still reference the old images:
    /// a resize is rare and a stall of one frame is invisible, while the tracking is the kind of
    /// lifetime bookkeeping that produced the crash above. Cheap correctness beats clever
    /// correctness in the one function this project has already broken once.
    ///
    /// A zero extent leaves the renderer with **no swapchain at all** rather than a zero-sized
    /// one, which Vulkan would refuse. That is the minimised-window state, and it is why
    /// [`Renderer::swapchain_extent`] is an `Option`.
    fn recreate_swapchain(&mut self) -> GfxResult<()> {
        // SAFETY: the device is live; this is what makes destroying the old swapchain's
        // semaphores below safe.
        unsafe { self.dev.device.device_wait_idle() }
            .map_err(GfxError::vk("recreate_swapchain", "vkDeviceWaitIdle"))?;

        // SAFETY: the physical device and surface are both live.
        let caps = unsafe {
            self.base.surface_fn
                .get_physical_device_surface_capabilities(self.physical_device, self.base.surface)
        }
        .map_err(GfxError::vk(
            "recreate_swapchain",
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
        ))?;

        if !caps.supported_usage_flags.contains(vk::ImageUsageFlags::TRANSFER_DST) {
            return Err(GfxError::SurfaceCannotBeTransferDestination);
        }

        let minimised = self.zero_size_is_the_windows
            && (self.target_extent.0 == 0 || self.target_extent.1 == 0);
        let extent = if minimised {
            vk::Extent2D { width: 0, height: 0 }
        } else {
            select::clamp_extent(self.target_extent, &caps)
        };
        if extent.width == 0 || extent.height == 0 {
            self.destroy_swapchain();
            self.swapchain_dirty = false;
            return Ok(());
        }

        // SAFETY: the physical device and surface are both live.
        let formats = unsafe {
            self.base.surface_fn.get_physical_device_surface_formats(self.physical_device, self.base.surface)
        }
        .map_err(GfxError::vk("recreate_swapchain", "vkGetPhysicalDeviceSurfaceFormatsKHR"))?;
        let offered = formats.len();
        // Only formats that can be a blit destination are candidates, because every present path
        // here writes the swapchain image with a transfer command. Filtering before choosing
        // rather than choosing then checking means `select::surface_format` stays pure.
        let candidates: Vec<vk::SurfaceFormatKHR> = formats
            .into_iter()
            .filter(|f| {
                // SAFETY: the physical device is live.
                let props = unsafe {
                    self.base.instance
                        .get_physical_device_format_properties(self.physical_device, f.format)
                };
                props.optimal_tiling_features.contains(vk::FormatFeatureFlags::BLIT_DST)
            })
            .collect();
        let format = select::surface_format(&candidates).ok_or_else(|| {
            GfxError::NoUsableSurfaceFormat { device: self.report.name.clone(), offered }
        })?;

        let families = [self.report.graphics_family, self.report.present_family];
        let concurrent = families[0] != families[1];
        let old = self.swapchain.as_ref().map_or(vk::SwapchainKHR::null(), |s| s.handle);
        let mut info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.base.surface)
            .min_image_count(select::image_count(&caps))
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.present_mode)
            // Discard pixels the compositor covers. The window's contents are redrawn every frame
            // and nothing ever reads them back, which is exactly the condition this permits.
            .clipped(true)
            .old_swapchain(old);
        info = if concurrent {
            info.image_sharing_mode(vk::SharingMode::CONCURRENT).queue_family_indices(&families)
        } else {
            info.image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        };

        // SAFETY: every handle in `info` is live, including `old` — which is the whole point; see
        // this function's doc comment.
        let handle = unsafe { self.swapchain_fn.create_swapchain(&info, None) }
            .map_err(GfxError::vk("recreate_swapchain", "vkCreateSwapchainKHR"))?;

        // Only now is the old one destroyed, and its per-image semaphores with it.
        self.destroy_swapchain();

        // SAFETY: `handle` was created a moment ago and is live.
        let images = unsafe { self.swapchain_fn.get_swapchain_images(handle) }.map_err(|result| {
            // SAFETY: `handle` is live and nothing references it yet.
            unsafe { self.swapchain_fn.destroy_swapchain(handle, None) };
            GfxError::vk("recreate_swapchain", "vkGetSwapchainImagesKHR")(result)
        })?;

        let mut render_finished = Vec::with_capacity(images.len());
        for _ in &images {
            // SAFETY: the device is live.
            match unsafe { self.dev.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
            {
                Ok(semaphore) => render_finished.push(semaphore),
                Err(result) => {
                    // SAFETY: every handle here was created by this function and is live.
                    unsafe {
                        for semaphore in render_finished {
                            self.dev.device.destroy_semaphore(semaphore, None);
                        }
                        self.swapchain_fn.destroy_swapchain(handle, None);
                    }
                    return Err(GfxError::vk("recreate_swapchain", "vkCreateSemaphore")(result));
                }
            }
        }

        self.swapchain = Some(Swapchain {
            handle,
            format: format.format,
            extent,
            image_in_flight: vec![vk::Fence::null(); images.len()],
            images,
            render_finished,
        });
        self.swapchain_dirty = false;
        self.swapchain_generations += 1;
        Ok(())
    }

    /// Destroy the swapchain and its per-image semaphores, if there is one.
    fn destroy_swapchain(&mut self) {
        let Some(swapchain) = self.swapchain.take() else { return };
        // SAFETY: every caller has either waited the device idle or created a replacement
        // swapchain naming this one as its `oldSwapchain`, so nothing is still using these.
        unsafe {
            for semaphore in swapchain.render_finished {
                self.dev.device.destroy_semaphore(semaphore, None);
            }
            self.swapchain_fn.destroy_swapchain(swapchain.handle, None);
        }
    }

    /// Destroy the staging pair, if there is one.
    fn destroy_staging(&mut self) {
        let Some(staging) = self.staging.take() else { return };
        // SAFETY: the caller has waited the device idle, so no frame is reading either object.
        // Unmapping before freeing is required; freeing mapped memory is undefined behaviour.
        unsafe {
            self.dev.device.unmap_memory(staging.buffer_memory);
            self.dev.device.destroy_buffer(staging.buffer, None);
            self.dev.device.free_memory(staging.buffer_memory, None);
            self.dev.device.destroy_image(staging.image, None);
            self.dev.device.free_memory(staging.image_memory, None);
        }
    }

}

impl Drop for Renderer {
    /// Destroy the two objects `Renderer` owns directly, and let the fields do the rest.
    ///
    /// The swapchain and the staging pair are here rather than in a `Drop` of their own because
    /// destroying either needs the device, and a field cannot reach a sibling from its own
    /// destructor. Everything below them -- the frames, the pool, the device, the surface, the
    /// instance -- is destroyed by `DeviceOwner` and `Base` in that order, which is the
    /// reverse of creation, because they are the last two fields of this struct.
    ///
    /// `vkDeviceWaitIdle` first: a swapchain image may still be held by the presentation engine,
    /// and destroying it underneath is the class of mistake this host has no validation layer to
    /// report. It is called again by `DeviceOwner::drop` a moment later, which costs nothing on an
    /// already-idle device and means neither type depends on the other having done it.
    fn drop(&mut self) {
        // SAFETY: the device is live. The result is discarded for the reason `DeviceOwner::drop`
        // gives: the only failure means the device is already lost.
        unsafe {
            let _ = self.dev.device.device_wait_idle();
        }
        self.destroy_swapchain();
        self.destroy_staging();
    }
}

impl core::fmt::Debug for Renderer {
    /// The choices and the counters, not the handles: a `VkImage` printed as a number helps
    /// nobody without a validation layer to correlate it against, and this host has none.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Renderer")
            .field("device", &self.report.name)
            .field("present_mode", &self.present_mode)
            .field("swapchain_extent", &self.swapchain_extent())
            .field("frames_presented", &self.frames_presented)
            .field("swapchain_generations", &self.swapchain_generations)
            .field("validation_enabled", &self.report.validation_enabled)
            .finish()
    }
}

/// What [`Renderer::record`] needs in order to blit a staged frame.
#[derive(Debug, Clone, Copy)]
struct BlitSource {
    buffer: vk::Buffer,
    image: vk::Image,
    width: u32,
    height: u32,
    filter: vk::Filter,
}

/// The whole of a single-mip, single-layer colour image.
fn whole_colour_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// The same, as a `VkImageSubresourceLayers` for the copy and blit commands.
fn colour_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

/// One image layout transition, named by what it is for.
///
/// A struct rather than six arguments because six arguments at a call site are six chances to
/// transpose two of them, and a transposed access mask or stage is exactly the class of mistake
/// this host has no validation layer to report (`docs/research/graphics-spike.md` Section 6). Each
/// of the three the renderer performs is spelled out once, here, where it can be read against the
/// specification.
#[derive(Debug, Clone, Copy)]
struct Transition {
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    src_stage: vk::PipelineStageFlags,
    dst_stage: vk::PipelineStageFlags,
}

impl Transition {
    /// Make an image writable as a transfer destination, **discarding** whatever was in it.
    ///
    /// `UNDEFINED` as the old layout is the load-bearing part: both images this is used on -- the
    /// swapchain image and the staging image -- are completely overwritten by the command that
    /// follows, so telling the driver it may throw the old contents away rather than preserve
    /// them across the transition is both correct and the cheaper of the two.
    const DISCARD_TO_TRANSFER_DST: Transition = Transition {
        old_layout: vk::ImageLayout::UNDEFINED,
        new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        src_access: vk::AccessFlags::empty(),
        dst_access: vk::AccessFlags::TRANSFER_WRITE,
        src_stage: vk::PipelineStageFlags::TOP_OF_PIPE,
        dst_stage: vk::PipelineStageFlags::TRANSFER,
    };

    /// Turn the freshly-uploaded staging image into the source of the blit that follows.
    const TRANSFER_DST_TO_SRC: Transition = Transition {
        old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        src_access: vk::AccessFlags::TRANSFER_WRITE,
        dst_access: vk::AccessFlags::TRANSFER_READ,
        src_stage: vk::PipelineStageFlags::TRANSFER,
        dst_stage: vk::PipelineStageFlags::TRANSFER,
    };

    /// Hand the finished swapchain image to the presentation engine.
    ///
    /// The destination access mask is empty and the destination stage is `BOTTOM_OF_PIPE`, which
    /// looks like a mistake and is not: the presentation engine's read is not a pipeline stage,
    /// and what makes it safe is the semaphore `vkQueuePresentKHR` waits on, not this barrier.
    /// The barrier's job here is only the layout change.
    const TRANSFER_DST_TO_PRESENT: Transition = Transition {
        old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        new_layout: vk::ImageLayout::PRESENT_SRC_KHR,
        src_access: vk::AccessFlags::TRANSFER_WRITE,
        dst_access: vk::AccessFlags::empty(),
        src_stage: vk::PipelineStageFlags::TRANSFER,
        dst_stage: vk::PipelineStageFlags::BOTTOM_OF_PIPE,
    };
}

/// Record one image layout transition.
///
/// # Safety
///
/// `command_buffer` must be in the recording state and `image` must belong to `device`.
unsafe fn barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    transition: Transition,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(transition.old_layout)
        .new_layout(transition.new_layout)
        .src_access_mask(transition.src_access)
        .dst_access_mask(transition.dst_access)
        // No queue-family ownership transfer: this renderer submits every transfer on the
        // graphics queue. `IGNORED` on both sides is how that is spelled, and putting the real
        // family index on both sides instead would request a transfer to the family that already
        // owns it -- legal, and a different thing from what is meant.
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(whole_colour_range());
    // SAFETY: the caller guarantees the recording state and the handle's provenance; `barrier`
    // outlives the call.
    unsafe {
        device.cmd_pipeline_barrier(
            command_buffer,
            transition.src_stage,
            transition.dst_stage,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
}

/// The window-system-integration extension a window needs, with what its absence means.
///
/// **By the window, not by a `cfg`**: the renderer asks for the surface extension of the window it
/// was handed, which is what keeps this crate free of `cfg(target_os)` -- and on Windows it is
/// `VK_KHR_win32_surface`, exactly as before.
fn platform_surface(window: RawWindow) -> GfxResult<(&'static CStr, &'static str, &'static str)> {
    match window {
        RawWindow::Win32 { .. } => Ok((
            khr::win32_surface::NAME,
            "VK_KHR_win32_surface",
            "the Vulkan loader found a driver, but not one that can present to a Win32 window",
        )),
        RawWindow::AppKit { .. } => Ok((
            ash::ext::metal_surface::NAME,
            "VK_EXT_metal_surface",
            "the Vulkan loader found a driver, but not one that can present to a CAMetalLayer \
             (MoltenVK provides it)",
        )),
        RawWindow::Xlib { .. } => Ok((
            khr::xlib_surface::NAME,
            "VK_KHR_xlib_surface",
            "the Vulkan loader found a driver, but not one that can present to an X11 window \
             through Xlib",
        )),
        other => Err(GfxError::UnsupportedWindowSystem { system: other.system_name() }),
    }
}

/// Create the instance, enabling validation only if the host has it, and portability enumeration
/// only when the loader needs it (see `crate::portability`). Answers how the instance can read
/// `VkPhysicalDeviceFeatures2`, which the device's portability subset (if any) needs.
fn create_instance(
    entry: &ash::Entry,
    window: RawWindow,
) -> GfxResult<(ash::Instance, Vec<String>, bool, portability::Features2)> {
    // SAFETY: both enumerations take no handles and write into `ash`-owned vectors.
    let (layer_props, extension_props) = unsafe {
        (
            entry
                .enumerate_instance_layer_properties()
                .map_err(GfxError::vk("create", "vkEnumerateInstanceLayerProperties"))?,
            entry
                .enumerate_instance_extension_properties(None)
                .map_err(GfxError::vk("create", "vkEnumerateInstanceExtensionProperties"))?,
        )
    };

    let available_layers: Vec<String> = layer_props
        .iter()
        .filter_map(|p| p.layer_name_as_c_str().ok())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    let validation_enabled =
        available_layers.iter().any(|name| name.as_str() == VALIDATION_LAYER.to_string_lossy());

    let has_extension = |wanted: &CStr| {
        extension_props.iter().any(|p| p.extension_name_as_c_str().is_ok_and(|n| n == wanted))
    };
    // Each row carries `ash`'s `&CStr` constant for the lookup *and* the plain `&'static str` the
    // error reports, rather than converting one into the other: the two spellings then sit on the
    // same line, where a mismatch is visible, instead of in a match that has to be read twice.
    // Both are refused by name and with what the absence means, because they mean different
    // things -- no driver at all, against a driver that is not a Windows one.
    let platform = platform_surface(window)?;
    for (probe, name, why) in [
        (
            khr::surface::NAME,
            "VK_KHR_surface",
            "the Vulkan loader found no installable client driver that can present at all",
        ),
        platform,
    ] {
        // A `debug_assert!`, not an `if`: no input can make these disagree, because both are
        // literals on the same row. VERIFICATION entry 12 is the rule — a guard that nothing can
        // reach reads as careful and is not a check, and the honest spelling of "this cannot
        // happen" is the one that says so.
        debug_assert_eq!(probe.to_str(), Ok(name), "the probe and the name it reports must match");
        if !has_extension(probe) {
            return Err(GfxError::MissingInstanceExtension { name, why });
        }
    }

    let app_name = CString::new("omnidroid").expect("no interior NUL in a literal");
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .application_version(1)
        .engine_name(&app_name)
        .engine_version(1)
        // 1.0, deliberately: nothing here uses a 1.1+ core feature, and asking for a version the
        // loader does not have is a `VK_ERROR_INCOMPATIBLE_DRIVER` for no benefit. This host
        // reports 1.4.325 (spike §3), which is compatible with a 1.0 request.
        .api_version(vk::make_api_version(0, 1, 0, 0));

    let mut names: Vec<&'static CStr> = vec![khr::surface::NAME, platform.0];
    let offered: Vec<&CStr> =
        extension_props.iter().filter_map(|p| p.extension_name_as_c_str().ok()).collect();
    let layers = [VALIDATION_LAYER.as_ptr()];
    let mut flags = vk::InstanceCreateFlags::empty();
    loop {
        let extensions: Vec<*const std::ffi::c_char> = names.iter().map(|name| name.as_ptr()).collect();
        let mut info = vk::InstanceCreateInfo::default()
            .flags(flags)
            .application_info(&app_info)
            .enabled_extension_names(&extensions);
        if validation_enabled {
            info = info.enabled_layer_names(&layers);
        }
        // SAFETY: every pointer in `info` is a `'static` C string or a local that outlives this
        // call.
        match unsafe { entry.create_instance(&info, None) } {
            Ok(instance) => {
                let features2 = portability::Features2::of(vk::API_VERSION_1_0, &names);
                return Ok((instance, available_layers, validation_enabled, features2));
            }
            // **Once**: the retry asks for the enumeration extension, so a second failure is not
            // retried again (`retry_with_portability` refuses a request that already has it).
            Err(result) if portability::retry_with_portability(result, &offered, &names) => {
                names.extend(portability::portability_additions(&offered, &names, vk::API_VERSION_1_0));
                flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
            }
            Err(result) => return Err(GfxError::vk("create", "vkCreateInstance")(result)),
        }
    }
}

/// Create the `VkSurfaceKHR` for a window.
fn create_surface(
    entry: &ash::Entry,
    instance: &ash::Instance,
    window: RawWindow,
) -> GfxResult<vk::SurfaceKHR> {
    match window {
        RawWindow::Win32 { hwnd, hinstance } => {
            let info = vk::Win32SurfaceCreateInfoKHR::default()
                .hinstance(hinstance)
                .hwnd(hwnd);
            let win32 = khr::win32_surface::Instance::new(entry, instance);
            // SAFETY: `hwnd` and `hinstance` came from a live `omni_platform::window::Window`,
            // whose documentation requires it to outlive any surface made from it.
            unsafe { win32.create_win32_surface(&info, None) }
                .map_err(GfxError::vk("create", "vkCreateWin32SurfaceKHR"))
        }
        RawWindow::AppKit { ca_metal_layer, .. } => {
            let info = vk::MetalSurfaceCreateInfoEXT::default()
                .layer(ca_metal_layer as *const vk::CAMetalLayer);
            let metal = ash::ext::metal_surface::Instance::new(entry, instance);
            // SAFETY: `ca_metal_layer` is the `CAMetalLayer` of a live
            // `omni_platform::window::Window`, whose documentation requires it to outlive any
            // surface made from it.
            unsafe { metal.create_metal_surface(&info, None) }
                .map_err(GfxError::vk("create", "vkCreateMetalSurfaceEXT"))
        }
        RawWindow::Xlib { display, window } => {
            let info = vk::XlibSurfaceCreateInfoKHR::default()
                .dpy(display as *mut vk::Display)
                .window(window as vk::Window);
            let xlib = khr::xlib_surface::Instance::new(entry, instance);
            // SAFETY: `display` and `window` came from a live `omni_platform::window::Window`,
            // whose documentation requires it to outlive any surface made from it, and the
            // instance was created with `VK_KHR_xlib_surface` by `platform_extension`'s same arm.
            unsafe { xlib.create_xlib_surface(&info, None) }
                .map_err(GfxError::vk("create", "vkCreateXlibSurfaceKHR"))
        }
        // `RawWindow` is `#[non_exhaustive]`, so a Wayland or AppKit variant added to the seam
        // later lands here and refuses **naming itself**, rather than turning this `match` into
        // one that silently stopped being exhaustive.
        other => Err(GfxError::UnsupportedWindowSystem { system: other.system_name() }),
    }
}

/// Whether, on `window`'s system, the window's own zero-pixel size is the only sign that it has
/// no pixels -- so that the renderer must hold no swapchain even though the surface still states
/// an extent.
///
/// **Win32: no**, and nothing changes there: a minimised window's surface reports a
/// `currentExtent` of 0x0 itself, which [`select::clamp_extent`] already returns. **Xlib: yes**,
/// MEASURED: an iconified X window keeps its geometry -- the server has no notion of minimised,
/// only the window manager's `WM_STATE` does (see `omni_platform`'s Linux backend) -- and Mesa's
/// X11 surface reports that geometry as `currentExtent` (640x480 for a 640x480 window iconified
/// under xfwm4, with lavapipe). Without this the renderer went on presenting to an unmapped window,
/// 14,863 frames in the ten seconds `renderer_live`'s minimise test waited. **AppKit: yes**,
/// MEASURED on this project's macOS host: MoltenVK kept reporting a miniaturised window's 640x480
/// `currentExtent`, and the renderer went on presenting to a window with no pixels on screen.
const fn zero_size_is_the_windows(window: RawWindow) -> bool {
    matches!(window, RawWindow::Xlib { .. } | RawWindow::AppKit { .. })
}

/// The [`claim::WindowKey`] for a window, or a refusal naming the system it belongs to.
///
/// **The same `match` shape [`create_surface`] has, and for the same reason**: `RawWindow` is
/// `#[non_exhaustive]`, so a Wayland or AppKit variant added to the seam later lands in the
/// wildcard arm and refuses naming itself. It is a separate function rather than a field of the
/// surface result because the claim is taken *before* the instance exists -- a renderer that
/// created an instance and a surface and only then discovered the window was taken would have
/// done real work on a window it is not allowed to present to.
fn window_key(window: RawWindow) -> GfxResult<claim::WindowKey> {
    match window {
        RawWindow::Win32 { hwnd, .. } => Ok(claim::WindowKey::win32(hwnd)),
        RawWindow::AppKit { ns_view, .. } => Ok(claim::WindowKey::appkit(ns_view)),
        RawWindow::Xlib { window, .. } => Ok(claim::WindowKey::xlib(window)),
        other => Err(GfxError::UnsupportedWindowSystem { system: other.system_name() }),
    }
}

/// What [`pick_physical_device`] decided.
struct PickedDevice {
    physical_device: vk::PhysicalDevice,
    graphics_family: u32,
    present_family: u32,
    name: String,
    device_type: vk::PhysicalDeviceType,
}

/// Choose the physical device and its two queue families.
///
/// Every device that can render *and* present *and* has `VK_KHR_swapchain` is a candidate, and the
/// best-ranked one wins ([`select::device_rank`] carries D8's reason for the order). A device that
/// fails is recorded with why, because `NoUsableDevice` with a count and no reasons is the shape
/// of diagnostic that sends the reader to a GPU forum.
fn pick_physical_device(
    instance: &ash::Instance,
    surface_fn: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> GfxResult<PickedDevice> {
    // SAFETY: the instance is live.
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(GfxError::vk("create", "vkEnumeratePhysicalDevices"))?;
    let considered = devices.len();
    let mut rejected: Vec<String> = Vec::new();
    let mut best: Option<(u32, PickedDevice)> = None;

    for physical_device in devices {
        // SAFETY: the handle came from the enumeration above and is live for the instance's
        // lifetime.
        let (props, families, extensions) = unsafe {
            (
                instance.get_physical_device_properties(physical_device),
                instance.get_physical_device_queue_family_properties(physical_device),
                instance.enumerate_device_extension_properties(physical_device),
            )
        };
        let name = props
            .device_name_as_c_str()
            .map_or_else(|_| "<unnamed>".to_owned(), |n| n.to_string_lossy().into_owned());

        let extensions = match extensions {
            Ok(extensions) => extensions,
            Err(result) => {
                rejected.push(format!("{name}: vkEnumerateDeviceExtensionProperties failed ({result})"));
                continue;
            }
        };
        let has_swapchain = extensions
            .iter()
            .any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == khr::swapchain::NAME));
        if !has_swapchain {
            rejected.push(format!("{name}: no VK_KHR_swapchain"));
            continue;
        }

        let mut graphics = None;
        let mut present = None;
        for (index, family) in families.iter().enumerate() {
            let index = index as u32;
            let does_graphics = family.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            // SAFETY: the physical device, the family index and the surface are all live and the
            // index came from this device's own family list.
            let can_present = unsafe {
                surface_fn.get_physical_device_surface_support(physical_device, index, surface)
            }
            .unwrap_or(false);
            // A family that does both is strictly better: it lets the swapchain be `EXCLUSIVE`,
            // which is the faster sharing mode and the one this host takes — its family 0 does
            // graphics, compute and transfer (spike §3).
            if does_graphics && can_present {
                graphics = Some(index);
                present = Some(index);
                break;
            }
            if does_graphics && graphics.is_none() {
                graphics = Some(index);
            }
            if can_present && present.is_none() {
                present = Some(index);
            }
        }

        let (Some(graphics_family), Some(present_family)) = (graphics, present) else {
            rejected.push(format!(
                "{name}: no queue family can {}",
                if graphics.is_none() { "render" } else { "present to this surface" }
            ));
            continue;
        };

        let rank = select::device_rank(props.device_type);
        let candidate = PickedDevice {
            physical_device,
            graphics_family,
            present_family,
            name,
            device_type: props.device_type,
        };
        if best.as_ref().is_none_or(|(best_rank, _)| rank > *best_rank) {
            best = Some((rank, candidate));
        }
    }

    best.map(|(_, picked)| picked).ok_or(GfxError::NoUsableDevice {
        considered,
        detail: if rejected.is_empty() {
            "the host reported no Vulkan physical devices at all".to_owned()
        } else {
            rejected.join("; ")
        },
    })
}

/// Create the logical device with one queue from each family it needs -- and, on a portability
/// implementation, with `VK_KHR_portability_subset` and exactly the subset features it supports
/// (see `crate::portability`). Answers the subset's gaps for the report.
fn create_device(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    graphics_family: u32,
    present_family: u32,
    features2: portability::Features2,
) -> GfxResult<(ash::Device, Option<Vec<&'static str>>)> {
    let priorities = [1.0f32];
    let mut queue_infos = vec![
        vk::DeviceQueueCreateInfo::default()
            .queue_family_index(graphics_family)
            .queue_priorities(&priorities),
    ];
    // **One entry per distinct family.** Listing the same family twice is invalid usage, and on a
    // host with no validation layers the symptom would be a device that works on the machine it
    // was written on and fails on one where the two families differ.
    if present_family != graphics_family {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(present_family)
                .queue_priorities(&priorities),
        );
    }
    // SAFETY: the physical device is live.
    let offered = unsafe { instance.enumerate_device_extension_properties(physical_device) }
        .map_err(GfxError::vk("create", "vkEnumerateDeviceExtensionProperties"))?;
    // SAFETY: live handles; `features2` is what `create_instance` said this instance can do.
    let subset = unsafe { portability::Subset::of(entry, instance, physical_device, &offered, features2) };
    let mut extensions = vec![khr::swapchain::NAME.as_ptr()];
    let mut subset_features = subset.map(|subset| subset.features);
    let mut info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    if let Some(features) = subset_features.as_mut() {
        extensions.push(portability::SUBSET.as_ptr());
        info = info.push_next(features);
    }
    info = info.enabled_extension_names(&extensions);
    // SAFETY: the physical device is live and every pointer in `info` outlives the call. The only
    // features requested are the subset's, read from this device.
    let device = unsafe { instance.create_device(physical_device, &info, None) }
        .map_err(GfxError::vk("create", "vkCreateDevice"))?;
    Ok((device, subset.map(|subset| subset.gaps())))
}

/// Allocate the per-frame command buffers, semaphores and fences.
///
/// The fences are created **signalled**, because the first frame waits on its fence before
/// anything has signalled it. An unsignalled fence there is a deadlock on frame one — a hang with
/// no error and, on this host, nothing to report it.
fn create_frames(device: &ash::Device, pool: vk::CommandPool) -> GfxResult<Vec<Frame>> {
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(MAX_FRAMES_IN_FLIGHT as u32);
    // SAFETY: the pool is live and belongs to this device.
    let buffers = unsafe { device.allocate_command_buffers(&alloc) }
        .map_err(GfxError::vk("create", "vkAllocateCommandBuffers"))?;

    let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
    let mut frames: Vec<Frame> = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
    for command_buffer in buffers {
        // SAFETY: the device is live and both create-infos outlive their calls. The semaphore is
        // destroyed again if the fence fails, because at that point nothing else owns it and the
        // caller's cleanup below only walks `frames`.
        let created = unsafe {
            match device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) {
                Err(result) => Err(result),
                Ok(semaphore) => match device.create_fence(&fence_info, None) {
                    Ok(fence) => Ok((semaphore, fence)),
                    Err(result) => {
                        device.destroy_semaphore(semaphore, None);
                        Err(result)
                    }
                },
            }
        };
        match created {
            Ok((image_available, in_flight)) => {
                frames.push(Frame { command_buffer, image_available, in_flight });
            }
            Err(result) => {
                // SAFETY: every handle here was created by this function and is live.
                unsafe {
                    for frame in frames {
                        device.destroy_semaphore(frame.image_available, None);
                        device.destroy_fence(frame.in_flight, None);
                    }
                }
                return Err(GfxError::vk("create", "vkCreateSemaphore/vkCreateFence")(result));
            }
        }
    }
    Ok(frames)
}

/// Allocate the staging buffer/image pair for a `width` x `height` RGBA8 frame.
fn create_staging(
    device: &ash::Device,
    memory: &vk::PhysicalDeviceMemoryProperties,
    width: u32,
    height: u32,
) -> GfxResult<Staging> {
    // `u64` throughout: 32,768 squared — this host's `maxImageDimension2D` (spike §3) — times four
    // bytes is 4 GiB, which overflows a `u32` well inside the range a guest can ask for.
    let size = u64::from(width) * u64::from(height) * 4;

    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: the device is live and `buffer_info` outlives the call.
    let buffer = unsafe { device.create_buffer(&buffer_info, None) }
        .map_err(GfxError::vk("present_rgba8", "vkCreateBuffer"))?;
    // SAFETY: `buffer` was just created on this device.
    let buffer_needs = unsafe { device.get_buffer_memory_requirements(buffer) };
    let host_visible = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let buffer_type = select::memory_type(memory, buffer_needs.memory_type_bits, host_visible)
        .ok_or(GfxError::NoUsableMemoryType {
            required: "HOST_VISIBLE | HOST_COHERENT",
            type_bits: buffer_needs.memory_type_bits,
        })
        .inspect_err(|_| {
            // SAFETY: `buffer` is live and nothing else references it.
            unsafe { device.destroy_buffer(buffer, None) };
        })?;

    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(buffer_needs.size)
        .memory_type_index(buffer_type);
    // SAFETY: the device is live and `alloc` outlives the call.
    let buffer_memory = unsafe { device.allocate_memory(&alloc, None) }
        .map_err(GfxError::vk("present_rgba8", "vkAllocateMemory"))
        .inspect_err(|_| {
            // SAFETY: `buffer` is live and unbound.
            unsafe { device.destroy_buffer(buffer, None) };
        })?;
    // SAFETY: the memory was allocated from a type `buffer`'s requirements permit and is at least
    // as large as they require; binding at offset 0 is therefore in range and correctly aligned.
    // The map that follows is of the whole allocation, which nothing else has mapped.
    let mapped = unsafe {
        device
            .bind_buffer_memory(buffer, buffer_memory, 0)
            .and_then(|()| {
                device.map_memory(buffer_memory, 0, size, vk::MemoryMapFlags::empty())
            })
            .map(|ptr| ptr.cast::<u8>())
    };
    let mapped = match mapped {
        Ok(mapped) => mapped,
        Err(result) => {
            // SAFETY: both handles are live; the memory is not mapped, because mapping is what
            // failed (or the bind before it, in which case it was never mapped either).
            unsafe {
                device.destroy_buffer(buffer, None);
                device.free_memory(buffer_memory, None);
            }
            return Err(GfxError::vk("present_rgba8", "vkBindBufferMemory/vkMapMemory")(result));
        }
    };

    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(STAGING_FORMAT)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);

    let cleanup_buffer = || {
        // SAFETY: the buffer is live and mapped, and nothing references it.
        unsafe {
            device.unmap_memory(buffer_memory);
            device.destroy_buffer(buffer, None);
            device.free_memory(buffer_memory, None);
        }
    };

    // SAFETY: the device is live and `image_info` outlives the call.
    let image = unsafe { device.create_image(&image_info, None) }
        .map_err(GfxError::vk("present_rgba8", "vkCreateImage"))
        .inspect_err(|_| cleanup_buffer())?;
    // SAFETY: `image` was just created on this device.
    let image_needs = unsafe { device.get_image_memory_requirements(image) };
    let image_type =
        select::memory_type(memory, image_needs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .ok_or(GfxError::NoUsableMemoryType {
                required: "DEVICE_LOCAL",
                type_bits: image_needs.memory_type_bits,
            })
            .inspect_err(|_| {
                cleanup_buffer();
                // SAFETY: `image` is live and unbound.
                unsafe { device.destroy_image(image, None) };
            })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(image_needs.size)
        .memory_type_index(image_type);
    // SAFETY: the device is live and `alloc` outlives the call. The bind that follows is of an
    // allocation from a memory type `image`'s own requirements permit and at least as large as
    // they require, so offset 0 is in range and correctly aligned; a failed bind frees it again
    // rather than leaking, which matters because the caller's error path does not know about it.
    let image_memory = unsafe {
        device.allocate_memory(&alloc, None).and_then(|memory| {
            match device.bind_image_memory(image, memory, 0) {
                Ok(()) => Ok(memory),
                Err(result) => {
                    device.free_memory(memory, None);
                    Err(result)
                }
            }
        })
    }
    .map_err(GfxError::vk("present_rgba8", "vkAllocateMemory/vkBindImageMemory"))
        .inspect_err(|_| {
            cleanup_buffer();
            // SAFETY: `image` is live and unbound.
            unsafe { device.destroy_image(image, None) };
        })?;

    Ok(Staging { width, height, buffer, buffer_memory, mapped, image, image_memory })
}
