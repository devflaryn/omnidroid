//! [`VulkanHost`]: the seam between the guest's `libvulkan.so` and a real host driver.
//!
//! # Why this is a trait here and not a dependency on `omni-gfx`
//!
//! `omni-gfx` already owns a real Vulkan: an `ash::Entry` loaded at run time, an instance, a
//! device and a swapchain presenting frames to a Win32 window. The obvious thing to do with it is
//! to `use omni_gfx::...` from this module, and the reason not to is recorded in `omni-gfx`'s own
//! manifest: `ash`'s `loaded` feature pulls `libloading`, which calls `LoadLibraryExW`/`dlopen`,
//! and that manifest signs it off as a **documented exception to Global Constraint 4 inside that
//! crate**. An exception that is scoped to one crate stops being scoped the moment a crate that
//! must build `cfg`-free for five targets depends on it — `cargo tree -p omni-android -e normal`
//! would then contain `ash` and `libloading`, and the `--no-default-features` build that proves no
//! C++ toolchain and no Vulkan SDK are needed would be proving it about a different graph.
//!
//! So this file is the shape [`AssetSource`](crate::ndk::AssetSource),
//! [`WindowSource`](crate::ndk::WindowSource), [`ThreadHost`](crate::bionic::ThreadHost) and
//! `HwcapPolicy` all already have, for the same reason each of them has it: the answer belongs to
//! something this crate must not be able to reach, so this crate asks. `omni-gfx` stays a
//! **dev**-dependency of `omni-android`, and the embedding is what hands an implementation over
//! through [`Vulkan::set_host`](super::Vulkan::set_host).
//!
//! # The one invariant the signatures are built to make unrepresentable
//!
//! **No host function pointer and no host Vulkan handle appears anywhere in this file.** The
//! handoff's stage 2 list puts it fifth and it is the one that is a memory-safety property rather
//! than a correctness one: the guest branches to whatever `vkGetInstanceProcAddr` returns, and a
//! host code address handed to translated ARM64 is a jump into x86-64 machine code with an AAPCS64
//! frame. It cannot happen here because there is no type in this file that can carry one:
//!
//! * [`VulkanHost::has_instance_proc`] answers **`bool`**, not a pointer. An implementation calls
//!   the driver's own `vkGetInstanceProcAddr` and reports only whether it answered — which is
//!   exactly the fact this layer needs in order to decide between a guest thunk and the NULL a
//!   conforming loader owes ([`ProcAnswer::NullFromDriver`](super::ProcAnswer::NullFromDriver)).
//! * [`HostInstance`] is a **token the implementation assigns**, not a `VkInstance`. The
//!   dispatchable pointer the driver returned stays inside the implementation, which is why this
//!   crate cannot leak one even by mistake, and why [`HostInstance`]'s `Debug` prints `#3` rather
//!   than something that could be read as an address.
//!
//! What would falsify the claim is a method here whose return type is `u64` and whose
//! documentation calls it a handle. There is none, and adding one is the thing to refuse.
//!
//! # Why a driver failure is a value and not an `Err`
//!
//! [`DriverAnswer`] separates "this seam could not ask" from "the driver answered, and the answer
//! was a failure". They travel to completely different places: the first is an
//! [`AbiError::Refused`](crate::AbiError::Refused) that stops the guest and names what the host
//! did wrong, and the second is a `VkResult` the guest is **entitled** to receive, because
//! `VK_ERROR_INCOMPATIBLE_DRIVER` from a real driver is a real answer to a real question and the
//! engine has a branch for it. Collapsing them into one `Result` would force a choice between
//! refusing a call the driver legitimately declined — which would make this layer lie about a
//! conforming driver — and inventing a `VkResult` for a host mistake, which is Global
//! Constraint 1's failure exactly. D22's argument, one layer up: values that must not be confused
//! are distinguishable only if the type can tell them apart.

use omni_platform::window::RawWindow;

use crate::error::{AbiError, AbiResult};

/// What a host driver said, when it was asked and answered.
///
/// See this module's documentation for why this is not folded into the surrounding
/// [`AbiResult`]. The short form: `Err` means *this seam* failed, `Failed` means the **driver**
/// failed and the guest gets its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverAnswer<T> {
    /// The driver succeeded, and this is what it produced.
    Ok(T),
    /// The driver returned a failing `VkResult`, carried here as the `i32` it is.
    ///
    /// **Forwarded to the guest verbatim.** Not translated, not clamped to a code this layer
    /// recognises, and not replaced by a refusal: a driver that answers
    /// `VK_ERROR_INCOMPATIBLE_DRIVER` (-9) or `VK_ERROR_EXTENSION_NOT_PRESENT` (-7) is telling the
    /// engine something the engine has a branch for, and substituting a different negative number
    /// would send it down the wrong one.
    Failed(i32),
}

/// One instance extension the host driver reports, as the driver spells it.
///
/// Both fields come from `VkExtensionProperties` and neither is invented here. `spec_version` in
/// particular is **carried through rather than recomputed** when a name is substituted — see
/// [`rewrite`](super::rewrite), which records the version it carried so that a reader can check
/// whether the number the guest saw belongs to the name the guest saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostExtension {
    /// `VkExtensionProperties::extensionName`, as a Rust string.
    pub name: String,
    /// `VkExtensionProperties::specVersion`.
    pub spec_version: u32,
}

/// `VkApplicationInfo`, decoded out of guest memory.
///
/// Every field is a fixed-width integer or an owned copy of a guest string: **nothing here is a
/// pointer**, which is what makes the struct safe to hand to a driver on another thread. The
/// decode is in [`instance`](super::instance) and every guest pointer it followed went through
/// `admit` first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplicationInfo {
    /// `pApplicationName`, or `None` when the guest passed NULL.
    pub application_name: Option<String>,
    /// `applicationVersion`.
    pub application_version: u32,
    /// `pEngineName`, or `None` when the guest passed NULL.
    pub engine_name: Option<String>,
    /// `engineVersion`.
    pub engine_version: u32,
    /// `apiVersion`, as `VK_MAKE_API_VERSION` packs it. Passed through unchanged: a loader that
    /// lowered it would be choosing the engine's Vulkan version for it.
    pub api_version: u32,
}

/// `VkInstanceCreateInfo`, decoded out of guest memory **and already rewritten**.
///
/// # The rewrite has happened by the time an implementation sees this
///
/// [`extensions`](InstanceRequest::extensions) holds **host** extension names. The guest asked for
/// `VK_KHR_android_surface`; what is in this list is whatever the host said stands in for it
/// ([`VulkanHost::platform_surface_extension`]). An implementation must therefore **not** try to
/// map names again — the substitution is applied exactly once, in
/// [`rewrite::apply_enabled`](super::rewrite::apply_enabled), and it is recorded there so that
/// [`Vulkan::rewrites`](super::Vulkan::rewrites) can be asserted on.
///
/// # And it is an owned list, not the guest's array
///
/// `ppEnabledExtensionNames` is a guest `const char *const *`. Under D4 identity mapping a
/// validated guest pointer *is* a host pointer, so passing the array through is possible and it is
/// still the wrong thing to do here, for two independent reasons. The list has to change — the
/// name the guest wrote is not the name the driver has — and the only ways to change it in place
/// are to write into the guest's own `.rodata` (which `admit` refuses, correctly) or to write into
/// memory the guest can concurrently read and write. Both are worse than allocating. So the shim
/// builds this list host-side, **guest memory is never mutated to achieve the substitution**, and
/// the guest's own array is left exactly as the engine wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceRequest {
    /// `VkInstanceCreateInfo::flags`, passed through. Unknown bits are the driver's to reject.
    pub flags: u32,
    /// `pApplicationInfo`, or `None` when the guest passed NULL — which is legal and is **not**
    /// the same as a zeroed structure, because a NULL `pApplicationInfo` means "Vulkan 1.0" by
    /// specification while a zeroed one means `apiVersion = 0`, which is also 1.0 but arrives at
    /// it by a different route the driver is allowed to distinguish.
    pub application: Option<ApplicationInfo>,
    /// `ppEnabledLayerNames`, owned. Not rewritten: no layer substitution exists.
    pub layers: Vec<String>,
    /// `ppEnabledExtensionNames`, owned and **already substituted** to host spelling.
    pub extensions: Vec<String>,
}

/// One instance a [`VulkanHost`] has created, named by a token the host itself assigns.
///
/// **Not a `VkInstance`.** The driver's dispatchable pointer never leaves the implementation; this
/// is an index into whatever the implementation keeps them in. Two indirections separate the guest
/// from the driver's pointer — this token, and the arena address
/// [`instance`](super::instance)'s registry hands the guest — and each exists for its own reason:
/// this one keeps a host pointer out of `omni-android` entirely, and the registry keeps a wild
/// guest value from reaching the driver (Global Constraint 11).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostInstance(u64);

impl HostInstance {
    /// Mint a token. Called by the implementation, and by nothing else.
    #[must_use]
    pub const fn from_token(token: u64) -> HostInstance {
        HostInstance(token)
    }

    /// The token back, for the implementation to look up.
    #[must_use]
    pub const fn token(self) -> u64 {
        self.0
    }
}

// Written by hand so that it cannot be read as an address. `#[derive(Debug)]` would print
// `HostInstance(3)`, which is fine, but a token that ever became a pointer-sized value would
// print as a plausible pointer and a reader would go looking for it in a map. The `#` says it is
// an ordinal.
impl core::fmt::Debug for HostInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "HostInstance(#{})", self.0)
    }
}

/// Mint one of stage 3's opaque host tokens.
///
/// **A macro rather than four hand-written copies**, and the reason is the thing the copies would
/// have got wrong: [`HostInstance`]'s `Debug` is written by hand so that a token cannot be read as
/// an address, and four more of those written out longhand is four more chances for one of them to
/// say `HostQueue(140699...)`. The macro makes the ordinal spelling structural. Every other line
/// of each type is the same as [`HostInstance`]'s, which is deliberate: they are the same idea —
/// *the driver's pointer stays in the implementation, and this crate holds an ordinal* — applied
/// to the four handle families a renderer needs next.
macro_rules! host_token {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        ///
        /// **Not a Vulkan handle.** The driver's own value never leaves the [`VulkanHost`]
        /// implementation; this is an index into whatever that implementation keeps them in, and
        /// its `Debug` prints `#3` so that nothing in a log can be mistaken for a pointer. See
        /// [`HostInstance`] for the full argument, which is the same one.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Mint a token. Called by the implementation, and by nothing else.
            #[must_use]
            pub const fn from_token(token: u64) -> $name {
                $name(token)
            }

            /// The token back, for the implementation to look up.
            #[must_use]
            pub const fn token(self) -> u64 {
                self.0
            }
        }

        impl core::fmt::Debug for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, concat!(stringify!($name), "(#{})"), self.0)
            }
        }
    };
}

host_token! {
    /// One `VkPhysicalDevice` a host enumerated.
    ///
    /// A physical device belongs to the instance it was enumerated from, and the token is what
    /// carries that association — this crate cannot see inside it and does not try to. What this
    /// crate guarantees is the other half: the guest never holds one of these either, because
    /// `VkPhysicalDevice` is **dispatchable** and the driver dereferences it.
    HostPhysicalDevice
}

host_token! {
    /// One `VkSurfaceKHR` a host created, for a window this runtime's `ndk` layer owns.
    ///
    /// `VkSurfaceKHR` is *non*-dispatchable — it is a 64-bit value the driver looks up rather than
    /// dereferences — and it is behind a token and a registry anyway. The reason is not memory
    /// safety this time but Global Constraint 1: a surface handle the guest invented and this
    /// layer passed through would reach the driver as a *plausible* surface, and the first thing
    /// that would notice is a swapchain created against something else.
    HostSurface
}

host_token! {
    /// One `VkDevice` a host created.
    HostDevice
}

host_token! {
    /// One `VkQueue` a host retrieved with `vkGetDeviceQueue`.
    ///
    /// A queue is **owned by its device and is never created or destroyed**: asking twice for the
    /// same family and index must produce the same `VkQueue`, so the registry deduplicates by
    /// token rather than issuing a second handle. A renderer that compares two queue handles for
    /// equality — which is how it decides whether the graphics and present queues are one queue,
    /// and therefore whether the swapchain is `EXCLUSIVE` or `CONCURRENT` — gets the right answer
    /// only because of that.
    HostQueue
}

// ------------------------------------------------------------- stage 4's seven handle families

host_token! {
    /// One `VkSwapchainKHR` a host created over a surface this layer issued.
    ///
    /// **Non-dispatchable**, and it is the family where that distinction is least comforting. A
    /// `VkSwapchainKHR` is a 64-bit value the driver looks up, so a forged one does not fault —
    /// it names *some* swapchain, and `vkQueuePresentKHR` would then put the guest's frame into
    /// whichever window that swapchain belongs to. On this host that is a real possibility rather
    /// than a theoretical one, because `omni_gfx::Renderer` creates swapchains of its own on the
    /// very window the guest is presenting to.
    HostSwapchain
}

host_token! {
    /// One `VkImage` a host got out of `vkGetSwapchainImagesKHR`.
    ///
    /// **Non-dispatchable**, and **not created and not destroyed**: a swapchain image is owned by
    /// its swapchain, `vkGetSwapchainImagesKHR` must answer with the same handles every time it is
    /// asked, and the images go away when the swapchain does. So the registry deduplicates on the
    /// token, exactly as [`HostQueue`]'s does, and `vkDestroySwapchainKHR` is what releases the
    /// handles — there is no `vkDestroyImage` for these and a guest calling one would be
    /// destroying an object it does not own.
    ///
    /// Stage 4 creates no other kind of image. An image the guest made with `vkCreateImage` is
    /// stage 5 and reaches a refusal naming the function.
    HostImage
}

host_token! {
    /// One `VkImageView` a host created.
    ///
    /// **Non-dispatchable.** Unlike [`HostImage`] this one *is* created and destroyed by the
    /// guest, one per swapchain image, which is why its registry supports removal and the image
    /// registry's removal is driven by the swapchain instead.
    HostImageView
}

host_token! {
    /// One `VkSemaphore` a host created. **Non-dispatchable.**
    ///
    /// A semaphore is a GPU-side ordering primitive with no host-visible state at all, which
    /// makes a forged one particularly quiet: the driver would wait on, or signal, some other
    /// frame's semaphore, and the symptom is a frame that tears or a queue that deadlocks several
    /// frames later.
    HostSemaphore
}

host_token! {
    /// One `VkFence` a host created. **Non-dispatchable.**
    ///
    /// The one synchronisation object the guest can *observe*: `vkWaitForFences` and
    /// `vkGetFenceStatus` answer about it. That makes a wrong fence handle the difference between
    /// a command buffer that is safe to re-record and one the GPU is still reading.
    HostFence
}

host_token! {
    /// One `VkCommandBuffer` a host allocated. **Dispatchable** — the third of the four
    /// dispatchable families, after `VkInstance`/`VkPhysicalDevice`, `VkDevice` and `VkQueue`.
    ///
    /// Its first word is a loader dispatch table the driver dereferences, so a `VkCommandBuffer`
    /// the guest computed reaching a driver is a host access violation from guest data — Global
    /// Constraint 11, Critical — rather than the quieter wrong-object failure the non-dispatchable
    /// families produce. It is also the handle a renderer touches most often, once per `vkCmd*`
    /// and there are hundreds of those per frame.
    ///
    /// The pool owns every buffer allocated from it, which is why
    /// [`VulkanHost::destroy_command_pool`] takes no list of buffers: destroying the pool frees
    /// them, and the registry has to drop their handles at the same moment or the guest would hold
    /// `VkCommandBuffer` handles naming freed host objects.
    HostCommandBuffer
}

host_token! {
    /// One `VkCommandPool` a host created. **Non-dispatchable.**
    HostCommandPool
}

/// `VkSwapchainCreateInfoKHR`, decoded out of guest memory.
///
/// # Nothing here is a pointer, including the one field that was one
///
/// `pQueueFamilyIndices` arrives as an owned `Vec<u32>`, for [`InstanceRequest`]'s reason: a
/// driver reading a guest array is a driver reading memory other guest threads may be writing, and
/// the copy costs one allocation per swapchain rather than one per frame.
///
/// # `old_swapchain` is a token and that is the whole of the recreation story
///
/// `VkSwapchainCreateInfoKHR::oldSwapchain` is how a renderer replaces a swapchain whose surface
/// has changed size. It is **not** optional bookkeeping: the driver is entitled to reuse the
/// outgoing swapchain's images and presentation resources, and the graphics spike measured what
/// happens when it is got wrong — destroying the outgoing swapchain *before* passing it here
/// crashed the NVIDIA driver on the first live resize, every time, with no validation error
/// anywhere (`docs/research/graphics-spike.md` §1, and `crate::vulkan`'s own module header).
///
/// So the guest's handle is resolved through the swapchain registry like any other, and a
/// `VkSwapchainKHR` the guest invented is a typed refusal rather than a value the driver retires.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SwapchainRequest {
    /// `VkSwapchainCreateInfoKHR::flags`, passed through.
    pub flags: u32,
    /// The surface, as a token. Never the guest's handle and never the driver's value.
    pub surface: Option<HostSurface>,
    /// `minImageCount`.
    pub min_image_count: u32,
    /// `imageFormat`, as the `VkFormat` `u32` it is.
    pub format: u32,
    /// `imageColorSpace`.
    pub colour_space: u32,
    /// `imageExtent.width`.
    pub width: u32,
    /// `imageExtent.height`.
    pub height: u32,
    /// `imageArrayLayers`.
    pub array_layers: u32,
    /// `imageUsage`.
    pub usage: u32,
    /// `imageSharingMode`.
    pub sharing_mode: u32,
    /// `pQueueFamilyIndices`, owned. Its length **is** `queueFamilyIndexCount`.
    pub queue_families: Vec<u32>,
    /// `preTransform`.
    pub pre_transform: u32,
    /// `compositeAlpha`.
    pub composite_alpha: u32,
    /// `presentMode`.
    pub present_mode: u32,
    /// `clipped`, as the `VkBool32` it is — passed through as a number rather than as a `bool`,
    /// because the specification permits any non-zero value and a driver may distinguish them.
    pub clipped: u32,
    /// `oldSwapchain`, resolved through the registry, or `None` for `VK_NULL_HANDLE`.
    pub old_swapchain: Option<HostSwapchain>,
}

/// `VkImageViewCreateInfo`, decoded out of guest memory.
///
/// `components` and `subresourceRange` travel as their **bytes**, for
/// [`DeviceRequest::features`]' reason: `VkComponentMapping` is four enums and
/// `VkImageSubresourceRange` is five `uint32_t`s, neither has a pointer or a hole, and decoding
/// nine integers into nine named fields and building them back up is eighteen chances to transpose
/// two of them — a swizzle with red and blue exchanged being exactly the kind of defect that looks
/// like a driver bug for a day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageViewRequest {
    /// `VkImageViewCreateInfo::flags`.
    pub flags: u32,
    /// The image, as a token.
    pub image: HostImageRef,
    /// `viewType`.
    pub view_type: u32,
    /// `format`.
    pub format: u32,
    /// `components`, as its [`COMPONENT_MAPPING_BYTES`](super::COMPONENT_MAPPING_BYTES) bytes.
    pub components: Vec<u8>,
    /// `subresourceRange`, as its
    /// [`IMAGE_SUBRESOURCE_RANGE_BYTES`](super::IMAGE_SUBRESOURCE_RANGE_BYTES) bytes.
    pub subresource_range: Vec<u8>,
}

/// Which image an [`ImageViewRequest`] is over.
///
/// A one-variant enum today, and it is here rather than a bare [`HostImage`] because the variant
/// that will be added is the one that matters: stage 5's `vkCreateImage` produces images that are
/// **not** a swapchain's, and a host that had been matching on `HostImage` alone would silently
/// treat one as the other. A reader asking "which images can this layer make a view of?" gets the
/// answer from the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostImageRef {
    /// An image `vkGetSwapchainImagesKHR` produced, which is the only kind stage 4 has.
    Swapchain(HostImage),
    /// No image at all — `VkImageViewCreateInfo::image` was `VK_NULL_HANDLE`, which the
    /// specification does not permit and which the shim refuses before a host sees it. Present so
    /// that the type has a `Default` for the structures that derive one.
    #[default]
    None,
}

/// One `VkImageMemoryBarrier`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageBarrier {
    /// `srcAccessMask`.
    pub src_access: u32,
    /// `dstAccessMask`.
    pub dst_access: u32,
    /// `oldLayout`.
    pub old_layout: u32,
    /// `newLayout`.
    pub new_layout: u32,
    /// `srcQueueFamilyIndex`, as the guest wrote it — including `VK_QUEUE_FAMILY_IGNORED`
    /// (`0xFFFF_FFFF`), which is what every barrier that is not an ownership transfer uses.
    pub src_queue_family: u32,
    /// `dstQueueFamilyIndex`.
    pub dst_queue_family: u32,
    /// The image, as a token.
    pub image: HostImageRef,
    /// `subresourceRange`, as its bytes. See [`ImageViewRequest::subresource_range`].
    pub subresource_range: Vec<u8>,
}

/// One `vkCmdPipelineBarrier`, decoded out of guest memory.
///
/// **Buffer memory barriers are absent and that is a refusal one layer up, not a silent drop.**
/// A `VkBufferMemoryBarrier` names a `VkBuffer`, stage 4 has no `VkBuffer` registry, and a barrier
/// forwarded with a guest-chosen buffer handle would be exactly the wild non-dispatchable value
/// Global Constraint 1 is about. The shim refuses a non-zero `bufferMemoryBarrierCount` naming the
/// count, so nothing reaches here to be dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineBarrier {
    /// `srcStageMask`.
    pub src_stage: u32,
    /// `dstStageMask`.
    pub dst_stage: u32,
    /// `dependencyFlags`.
    pub dependency_flags: u32,
    /// `pMemoryBarriers`, as `(srcAccessMask, dstAccessMask)` pairs. There is no handle in a
    /// `VkMemoryBarrier`, so there is nothing to resolve and nothing to keep the layout of.
    pub memory_barriers: Vec<(u32, u32)>,
    /// `pImageMemoryBarriers`, decoded.
    pub image_barriers: Vec<ImageBarrier>,
}

/// One `VkSubmitInfo`, decoded out of guest memory.
///
/// Every handle is a token and every array is owned, for [`InstanceRequest`]'s reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmitRequest {
    /// `pWaitSemaphores` with `pWaitDstStageMask`, **zipped** — because the specification requires
    /// the two arrays to have the same length and a pair cannot come apart the way two `Vec`s can.
    pub waits: Vec<(HostSemaphore, u32)>,
    /// `pCommandBuffers`, in the guest's order, which is the order they execute in.
    pub command_buffers: Vec<HostCommandBuffer>,
    /// `pSignalSemaphores`.
    pub signals: Vec<HostSemaphore>,
}

/// One `VkPresentInfoKHR`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresentRequest {
    /// `pWaitSemaphores`.
    pub waits: Vec<HostSemaphore>,
    /// `pSwapchains` with `pImageIndices`, **zipped** for [`SubmitRequest::waits`]' reason.
    pub swapchains: Vec<(HostSwapchain, u32)>,
    /// Whether the guest passed a non-NULL `pResults` and therefore wants a per-swapchain code.
    ///
    /// A `bool` rather than the pointer: the shim owns the write, so what the host has to answer
    /// is whether it should produce the list at all.
    pub wants_per_swapchain_results: bool,
}

/// What `vkAcquireNextImageKHR` did.
///
/// # Why this is not a [`DriverAnswer`]
///
/// [`DriverAnswer`] splits "the driver answered, and the answer was a failure" from "this seam
/// could not ask", and that split is still right — but acquire has a **third** shape it cannot
/// express: `VK_SUBOPTIMAL_KHR` is a *success* code that also produces a valid image index, and
/// `VK_TIMEOUT` and `VK_NOT_READY` are success codes that produce **no** index. A
/// `DriverAnswer<u32>` would have to call `VK_SUBOPTIMAL_KHR` a failure, which would make this
/// layer swallow precisely the code the engine's resize branch is looking for.
///
/// So the result travels as the `i32` it is and the index travels beside it as an `Option`, and
/// the shim writes `pImageIndex` when and only when there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acquired {
    /// The driver's own `VkResult`, **verbatim** — `VK_SUCCESS`, `VK_TIMEOUT`, `VK_NOT_READY`,
    /// `VK_SUBOPTIMAL_KHR`, `VK_ERROR_OUT_OF_DATE_KHR` or any other code it chose.
    pub result: i32,
    /// The image index the driver wrote, or `None` when it wrote none.
    ///
    /// `Some` for `VK_SUCCESS` and `VK_SUBOPTIMAL_KHR`; `None` for everything else, including
    /// `VK_TIMEOUT`, where the specification explicitly leaves `pImageIndex` untouched.
    pub image_index: Option<u32>,
}

/// What `vkQueuePresentKHR` did.
///
/// [`Acquired`]'s argument, one call along: `VK_SUBOPTIMAL_KHR` from present means the frame
/// *was* presented and the swapchain no longer matches the surface, so it is neither a success to
/// be flattened nor a failure to be reported as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presented {
    /// The driver's own `VkResult`, verbatim.
    pub result: i32,
    /// One `VkResult` per swapchain, in the order they were presented, when the guest asked for
    /// them by passing a non-NULL `pResults` — otherwise empty.
    ///
    /// **Not synthesised from [`Presented::result`].** The whole reason `pResults` exists is that
    /// a multi-swapchain present can succeed for one window and be out of date for another, and a
    /// list filled in by copying the aggregate would be a plausible answer that is wrong exactly
    /// when it matters.
    pub per_swapchain: Vec<i32>,
}

/// One `VkDeviceQueueCreateInfo`, decoded out of guest memory.
///
/// Nothing here is a pointer: `pQueuePriorities` is an owned `Vec<f32>` by the time it arrives,
/// for [`InstanceRequest`]'s reason — a driver reading a guest array is a driver reading memory
/// other guest threads may be writing, and the copy costs one allocation per queue family.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueRequest {
    /// `VkDeviceQueueCreateInfo::flags`. Passed through; unknown bits are the driver's to reject.
    pub flags: u32,
    /// `queueFamilyIndex`, as the guest wrote it. **Not validated against the device's family
    /// count here**: the driver owns that check and answers `VK_ERROR_INITIALIZATION_FAILED` or
    /// triggers validation, and a second copy of the rule in this layer would be a second place
    /// for it to be wrong.
    pub family_index: u32,
    /// `pQueuePriorities`, owned. Its length **is** `queueCount` — the two cannot disagree once
    /// the array has been copied, which is the point of copying it.
    pub priorities: Vec<f32>,
}

/// `VkDeviceCreateInfo`, decoded out of guest memory.
///
/// # Why the extension list is **not** rewritten on the way in
///
/// [`InstanceRequest::extensions`] arrives already substituted, because `VK_KHR_android_surface`
/// is an *instance* extension and the guest's spelling of it is not the host's. There is no
/// device-level counterpart: `VK_KHR_swapchain` is spelled the same way on every platform, and
/// Android's device extensions that have no host analogue
/// (`VK_ANDROID_external_memory_android_hardware_buffer`) have no substitute to offer either — a
/// rename would be inventing one. So this list is the guest's, verbatim, and a name the driver
/// does not have comes back as the driver's own `VK_ERROR_EXTENSION_NOT_PRESENT`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceRequest {
    /// `VkDeviceCreateInfo::flags`. Reserved by the specification; passed through anyway, because
    /// a bit this layer dropped would be a device created differently from the one asked for.
    pub flags: u32,
    /// `pQueueCreateInfos`, owned, in the guest's order.
    pub queues: Vec<QueueRequest>,
    /// `ppEnabledLayerNames`, owned. Deprecated and ignored by modern loaders; forwarded anyway.
    pub layers: Vec<String>,
    /// `ppEnabledExtensionNames`, owned and **not** substituted. See this type's documentation.
    pub extensions: Vec<String>,
    /// `pEnabledFeatures`, as the [`PHYSICAL_DEVICE_FEATURES_BYTES`] bytes the guest wrote, or
    /// `None` when the guest passed NULL — which is legal and means "no optional feature".
    ///
    /// **Bytes rather than a decoded structure**, and that is the whole argument this seam makes
    /// about Vulkan's fixed-layout structures: `VkPhysicalDeviceFeatures` is 55 `VkBool32`s with
    /// no pointer and no padding, identical on aarch64 LP64 and x86-64 LLP64, so the bytes the
    /// guest wrote **are** the bytes the driver reads. Decoding 55 booleans into 55 named fields
    /// and building them back up would be 110 opportunities to transpose two of them, and a
    /// transposed feature bit is a device that quietly lacks something the engine enabled.
    ///
    /// [`PHYSICAL_DEVICE_FEATURES_BYTES`]: super::PHYSICAL_DEVICE_FEATURES_BYTES
    pub features: Option<Vec<u8>>,
}

/// What a host's platform surface call produced.
///
/// # Why the host names the call and this crate does not
///
/// [`VulkanHost::platform_surface_extension`]'s argument, one call along: `omni-android` contains
/// no `cfg(target_os)` and names no OS, and `"vkCreateWin32SurfaceKHR"` is an OS name. The guest
/// called `vkCreateAndroidSurfaceKHR` and something else happened; **what** else happened is the
/// substitution's other half, it goes in the rewrite log beside the extension renames, and the
/// only participant that can spell it is the one that made the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceCreated {
    /// The surface itself, as a token.
    pub surface: HostSurface,
    /// The entry point the host actually called, spelled as the host spells it — for example
    /// `vkCreateWin32SurfaceKHR`. **Recorded in [`Vulkan::rewrites`](super::Vulkan::rewrites)**,
    /// because a surface silently created through another platform's call is exactly the defect
    /// Global Constraint 1 names.
    pub host_call: String,
}

/// The refusal a [`VulkanHost`] method's default body produces.
///
/// # Why the stage 3 methods have default bodies at all
///
/// Every method added after stage 2a defaults to this, and the alternative — making them required
/// — was rejected for a reason that is about honesty rather than convenience. A required method
/// forces every implementation to write *something*, and the something an implementation writes
/// when it has nothing to say is the plausible stub rule 1 forbids. A default that refuses **by
/// name** gives a partial host exactly one behaviour: the guest's call fails, the failure says
/// which method of which trait was not implemented, and nothing anywhere returns `VK_SUCCESS`.
///
/// `GfxVulkanHost` overrides all of them, so nothing in this workspace relies on the default; what
/// relies on it is the test double in `tests/vulkan_instance.rs`, which is a stage 2a driver and
/// should stay one.
fn host_has_no(method: &'static str, what: &str) -> AbiError {
    AbiError::Refused {
        symbol: method.to_string(),
        address: 0,
        why: format!(
            "the guest reached `{method}` and the `VulkanHost` implementation behind this loader \
             does not implement it, so {what}. This is the trait's own default body: it refuses \
             by name rather than answering, because a host that has not been written yet and a \
             driver that declined are different facts and only one of them is the guest's \
             business. `omni_gfx::GfxVulkanHost` implements every method of this trait"
        ),
    }
}

/// A real Vulkan driver, as much of one as stages 2a and 3 need and **not one function more**.
///
/// # The scope is measured, not chosen
///
/// `libroblox.so` imports zero `vk*` symbols; the whole API arrives through
/// `vkGetInstanceProcAddr`, and the decoded bootstrap at guest `0x02595160` asks for exactly two
/// names before it does anything else. Those two were stage 2a. Stage 3 adds the set a renderer
/// must call between an instance and a swapchain — a surface, an enumeration, the queries that
/// choose a device, and the device itself — and stops there. The batch after it is a
/// **measurement** away rather than a guess: [`Vulkan::names`](super::Vulkan::names) is the
/// ordered list of what the engine actually asked for, and a name with no method behind it
/// produces a refusal naming the function rather than a plausible `VK_SUCCESS`.
///
/// # Why so many methods answer `Vec<u8>`
///
/// `vkGetPhysicalDeviceProperties` fills 824 bytes of which 504 are `VkPhysicalDeviceLimits`
/// alone. Every member of every one of these structures is a fixed-width integer, an enum, a
/// fixed array of those, or a `VkDeviceSize` — **no pointers**, and `size_t` never appears — so
/// the guest's aarch64 LP64 layout and this host's x86-64 layout are the same layout, and the
/// bytes the driver wrote are exactly the bytes the guest is owed. Transcribing them into a
/// hand-written mirror would put 130 field offsets in this crate for a compiler to lay out for
/// the *wrong* target, and one transposed pair in `VkPhysicalDeviceLimits` is a renderer that
/// silently believes it may allocate a larger image than the device supports.
///
/// So the structure travels as its bytes, and the two checks that make that safe are stated in
/// two places that must agree: this crate names the size
/// ([`PHYSICAL_DEVICE_PROPERTIES_BYTES`](super::PHYSICAL_DEVICE_PROPERTIES_BYTES) and its
/// siblings) and refuses a blob of any other length, and `omni-gfx` asserts the same number
/// against `core::mem::size_of` of `ash`'s generated structure. A disagreement is a failing test
/// on one side and a named refusal on the other, never a short write into a guest buffer.
///
/// # `Send + Sync`, and where the calls come from
///
/// An implementation is called **on a guest thread, inside an import**, with none of
/// [`Vulkan`](super::Vulkan)'s own locks held — the shim clones the `Arc` out before asking, the
/// way [`ndk::window`](crate::ndk::window)'s `decided` does. In practice that thread is the game
/// thread `GameActivity_onCreate` spawns, not the thread that called `initializeNativeCode`, which
/// is why [`Vulkan::thread_instance`](super::Vulkan::thread_instance) exists at all. An
/// implementation that blocks here blocks the guest inside a Vulkan call.
pub trait VulkanHost: Send + Sync + core::fmt::Debug {
    /// The host extension that stands in for the guest's `VK_KHR_android_surface`.
    ///
    /// # Why the host names it and this crate does not
    ///
    /// `omni-android` contains no `cfg(target_os)` and names no OS (Global Constraint 4 and
    /// `ARCHITECTURE.md` section 2). `"VK_KHR_win32_surface"` is an OS name, so a constant here
    /// would be this crate deciding which platform it is on — and it would be wrong on four of
    /// the five targets. What *is* a fact this crate owns is the other half of the pair:
    /// [`GUEST_SURFACE_EXTENSION`](super::GUEST_SURFACE_EXTENSION), because this crate is the
    /// Android compatibility layer and the guest is an Android binary. So the pairing is
    /// completed here, by the only participant that knows both ends.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`](crate::AbiError::Refused) when the host has **no** platform surface
    /// extension at all. That is deliberately loud rather than an `Option`: a host with a Vulkan
    /// driver that cannot present to any window is a misconfigured embedding, and answering
    /// "`VK_KHR_android_surface` is simply absent" would send the engine down its own silent
    /// no-Vulkan fall-back — the branch at `0x2595194` that this module's header opens by saying
    /// produces no diagnostic anywhere.
    fn platform_surface_extension(&self) -> AbiResult<String>;

    /// `vkEnumerateInstanceExtensionProperties`, forwarded.
    ///
    /// `layer` is `pLayerName`: `None` for the implicit-layer set, `Some` for one named layer's
    /// extensions. A layer the driver does not have is **not** an error here — it is
    /// `VK_ERROR_LAYER_NOT_PRESENT` from the driver, so it comes back as
    /// [`DriverAnswer::Failed`] and reaches the guest as that code.
    ///
    /// The list comes back in the **driver's own order**, and the shim keeps it: a guest that
    /// indexes the array it was given twice must see the same thing in the same place both times.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`](crate::AbiError::Refused) if the host could not ask at all — no
    /// loader, or a driver that answered something this seam cannot render as a list.
    fn instance_extensions(&self, layer: Option<&str>) -> AbiResult<DriverAnswer<Vec<HostExtension>>>;

    /// `vkCreateInstance`, forwarded, with the extension list already in host spelling.
    ///
    /// **A real instance or a real failure code, never a fabricated success.** An implementation
    /// that returned [`DriverAnswer::Ok`] without calling the driver is the defect Global
    /// Constraint 1 names, and the test that catches it is the live one: it asserts a driver
    /// device name came back through the same host afterwards.
    ///
    /// There is no `pAllocator` parameter, and its absence is a decision rather than an omission
    /// — see [`instance`](super::instance), which refuses a non-null one by name and counts it,
    /// because a `VkAllocationCallbacks` is a **guest** function pointer that a host driver may
    /// call on its own worker thread where no guest CPU context exists.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`](crate::AbiError::Refused) if the host could not ask — no loader, or
    /// a request it cannot render (a name with an interior NUL, or more instances than it keeps
    /// room for).
    fn create_instance(&self, request: &InstanceRequest) -> AbiResult<DriverAnswer<HostInstance>>;

    /// Whether the driver has an entry point called `name` for `instance` — and **only** whether.
    ///
    /// This is the method the no-host-pointers invariant is built around; this module's header
    /// says why a pointer here would be a branch from translated ARM64 into x86-64. An
    /// implementation calls the driver's own `vkGetInstanceProcAddr` and reports `is_some()`.
    ///
    /// `false` is what lets this layer answer the NULL a conforming loader owes for a command the
    /// instance does not support, on the driver's authority rather than on a list written here —
    /// and [`ProcAnswer::NullFromDriver`](super::ProcAnswer::NullFromDriver) is a different value
    /// from the NULL the specification fixes for a null instance, so the census says which
    /// authority produced each one.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`](crate::AbiError::Refused) if `instance` is not a token this host
    /// issued, which is a bug in the caller's registry rather than anything the guest did, and if
    /// `name` cannot be rendered as a C string.
    fn has_instance_proc(&self, instance: HostInstance, name: &str) -> AbiResult<bool>;

    // ------------------------------------------------------------------------ stage 3

    /// **The call the guest has no host counterpart for.** `vkCreateAndroidSurfaceKHR`, satisfied
    /// by whatever this host's window system calls the same thing.
    ///
    /// `window` is the OS handle behind the guest's `ANativeWindow *`, resolved through
    /// [`ndk::WindowSource::raw_window`](crate::ndk::WindowSource::raw_window) — so the geometry
    /// `ANativeWindow_getWidth` reports and the window this surface is created over come from the
    /// **same** source and cannot describe two different windows.
    ///
    /// [`SurfaceCreated::host_call`] is not optional bookkeeping: it is the other half of the
    /// substitution stage 2a began, it lands in the same rewrite log, and a host that returned an
    /// empty string there would be making a rename nobody recorded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `instance` is not a token this host issued, or when `window` is
    /// a windowing system this host has no surface call for — which refuses **naming the system**
    /// ([`RawWindow::system_name`]) rather than answering a `VkResult`, because "this build of the
    /// host cannot make a surface for Wayland" is not something the Vulkan specification has a
    /// code for and `VK_ERROR_INITIALIZATION_FAILED` would send the engine looking at its driver.
    fn create_platform_surface(
        &self,
        instance: HostInstance,
        window: RawWindow,
    ) -> AbiResult<DriverAnswer<SurfaceCreated>> {
        let _ = (instance, window);
        Err(host_has_no(
            "VulkanHost::create_platform_surface",
            "the guest's `vkCreateAndroidSurfaceKHR` has nothing to be turned into and there is \
             no surface",
        ))
    }

    /// The host entry point that stands in for the guest's `vkCreateAndroidSurfaceKHR`.
    ///
    /// [`VulkanHost::platform_surface_extension`]'s partner, and it **must name the call that goes
    /// with the extension that method returns** — the two are one choice, and a host that answered
    /// `VK_KHR_win32_surface` here and `vkCreateXlibSurfaceKHR` there would make this layer hand
    /// the guest a thunk on the strength of a command the surface it eventually creates is not
    /// made by.
    ///
    /// It exists because of what the first live run found:
    /// [`GUEST_SURFACE_ENTRY_POINT`](super::GUEST_SURFACE_ENTRY_POINT) carries the whole account.
    /// The short form is that `vkGetInstanceProcAddr` asks
    /// [`VulkanHost::has_instance_proc`] whether the driver has a command, this one is a command no
    /// host driver has, and answering the driver's NULL for it would strand the engine one call
    /// after it had been told the extension exists.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`](crate::AbiError::Refused) when the host has no platform surface call
    /// at all — the same condition [`VulkanHost::platform_surface_extension`] refuses for, and for
    /// the same reason.
    fn platform_surface_entry_point(&self) -> AbiResult<String> {
        Err(host_has_no(
            "VulkanHost::platform_surface_entry_point",
            "there is no host call to satisfy the guest's `vkCreateAndroidSurfaceKHR` with, so \
             `vkGetInstanceProcAddr` cannot hand out a thunk for it and the engine is told the \
             command does not exist",
        ))
    }

    /// `vkEnumeratePhysicalDevices`, forwarded, as tokens.
    ///
    /// **The order is the driver's and must not change between calls.** The guest enumerates
    /// twice — once for the count, once for the array — and a renderer indexes the array it was
    /// given. An implementation that re-enumerated and got a different order the second time would
    /// hand the guest handles that name different devices in the two halves of one protocol, so an
    /// implementation caches the driver's list per instance and answers from the cache.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `instance` is not a token this host issued.
    fn physical_devices(
        &self,
        instance: HostInstance,
    ) -> AbiResult<DriverAnswer<Vec<HostPhysicalDevice>>> {
        let _ = instance;
        Err(host_has_no(
            "VulkanHost::physical_devices",
            "there is no list of devices to answer with and a count of zero would tell the engine \
             this machine has no GPU",
        ))
    }

    /// `vkGetPhysicalDeviceProperties`, as its
    /// [`PHYSICAL_DEVICE_PROPERTIES_BYTES`](super::PHYSICAL_DEVICE_PROPERTIES_BYTES) bytes.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn physical_device_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        let _ = device;
        Err(host_has_no(
            "VulkanHost::physical_device_properties",
            "there is no `VkPhysicalDeviceProperties` to write and a zeroed one would name a \
             device with no name, vendor or limits",
        ))
    }

    /// `vkGetPhysicalDeviceFeatures`, as its
    /// [`PHYSICAL_DEVICE_FEATURES_BYTES`](super::PHYSICAL_DEVICE_FEATURES_BYTES) bytes.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn physical_device_features(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        let _ = device;
        Err(host_has_no(
            "VulkanHost::physical_device_features",
            "there is no `VkPhysicalDeviceFeatures` to write, and an all-zero one is a device that \
             supports no optional feature -- which is a believable answer and therefore the worst \
             one",
        ))
    }

    /// `vkGetPhysicalDeviceQueueFamilyProperties`, one
    /// [`QUEUE_FAMILY_PROPERTIES_BYTES`](super::QUEUE_FAMILY_PROPERTIES_BYTES)-byte entry per
    /// family, **in the driver's order** — the index into this list *is* the queue family index
    /// every later call uses.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn queue_family_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<Vec<u8>>> {
        let _ = device;
        Err(host_has_no(
            "VulkanHost::queue_family_properties",
            "there are no queue families to report and an empty list is a device that can do no \
             work at all",
        ))
    }

    /// `vkGetPhysicalDeviceMemoryProperties`, as its
    /// [`PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES`](super::PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES)
    /// bytes.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn physical_device_memory_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        let _ = device;
        Err(host_has_no(
            "VulkanHost::physical_device_memory_properties",
            "there are no memory types or heaps to report, and the engine chooses where every \
             texture it uploads lives from exactly this table",
        ))
    }

    /// `vkGetPhysicalDeviceSurfaceSupportKHR`, as the `VkBool32` it writes.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when either token is not one this host issued.
    fn surface_support(
        &self,
        device: HostPhysicalDevice,
        queue_family: u32,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<bool>> {
        let _ = (device, queue_family, surface);
        Err(host_has_no(
            "VulkanHost::surface_support",
            "there is no way to say whether that queue family can present to that surface, and \
             both answers are ones a renderer acts on",
        ))
    }

    /// `vkGetPhysicalDeviceSurfaceCapabilitiesKHR`, as its
    /// [`SURFACE_CAPABILITIES_BYTES`](super::SURFACE_CAPABILITIES_BYTES) bytes.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when either token is not one this host issued.
    fn surface_capabilities(
        &self,
        device: HostPhysicalDevice,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let _ = (device, surface);
        Err(host_has_no(
            "VulkanHost::surface_capabilities",
            "there is no `VkSurfaceCapabilitiesKHR` to write, and the swapchain's extent and image \
             count are read straight out of it",
        ))
    }

    /// `vkGetPhysicalDeviceSurfaceFormatsKHR`, one
    /// [`SURFACE_FORMAT_BYTES`](super::SURFACE_FORMAT_BYTES)-byte entry per format, in the
    /// driver's order.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when either token is not one this host issued.
    fn surface_formats(
        &self,
        device: HostPhysicalDevice,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<Vec<u8>>>> {
        let _ = (device, surface);
        Err(host_has_no(
            "VulkanHost::surface_formats",
            "there are no surface formats to report and an empty list ends the renderer's \
             selection with nothing naming why",
        ))
    }

    /// `vkGetPhysicalDeviceSurfacePresentModesKHR`, as the `VkPresentModeKHR` values they are.
    ///
    /// `u32` rather than bytes because a `VkPresentModeKHR` **is** a `u32` — there is no structure
    /// to keep the layout of, and a list of integers is something this layer can print in a
    /// diagnostic.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when either token is not one this host issued.
    fn surface_present_modes(
        &self,
        device: HostPhysicalDevice,
        surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u32>>> {
        let _ = (device, surface);
        Err(host_has_no(
            "VulkanHost::surface_present_modes",
            "there are no present modes to report -- not even `VK_PRESENT_MODE_FIFO_KHR`, which \
             every implementation supports and which this layer must not assert on a driver's \
             behalf",
        ))
    }

    /// `vkEnumerateDeviceExtensionProperties`, forwarded.
    ///
    /// The list is **not** rewritten; [`DeviceRequest`] says why there is nothing to rewrite.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when `layer` cannot
    /// be rendered as a C string.
    fn device_extensions(
        &self,
        device: HostPhysicalDevice,
        layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        let _ = (device, layer);
        Err(host_has_no(
            "VulkanHost::device_extensions",
            "there is no extension list, and an empty one says this device has no \
             `VK_KHR_swapchain` -- which would end renderer bring-up on a device that has it",
        ))
    }

    /// `vkCreateDevice`, forwarded.
    ///
    /// There is no `pAllocator` parameter for [`VulkanHost::create_instance`]'s reason, and the
    /// shim refuses a non-null one by name one layer up.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when a name in the
    /// request cannot be rendered as a C string.
    fn create_device(
        &self,
        device: HostPhysicalDevice,
        request: &DeviceRequest,
    ) -> AbiResult<DriverAnswer<HostDevice>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_device",
            "no `VkDevice` is created, and a `VK_SUCCESS` with a handle naming nothing is the \
             defect Global Constraint 1 exists for",
        ))
    }

    /// `vkGetDeviceQueue`, forwarded.
    ///
    /// **The same family and index must answer with the same queue every time.** See
    /// [`HostQueue`]: a renderer decides whether its graphics and present queues are one queue by
    /// comparing the two handles.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn device_queue(
        &self,
        device: HostDevice,
        family: u32,
        index: u32,
    ) -> AbiResult<HostQueue> {
        let _ = (device, family, index);
        Err(host_has_no(
            "VulkanHost::device_queue",
            "there is no `VkQueue` to answer with, and `vkGetDeviceQueue` returns `void` -- so a \
             fabricated handle would have no status code beside it to be disbelieved",
        ))
    }

    /// Whether the driver has a **device-level** entry point called `name` for `device`.
    ///
    /// [`VulkanHost::has_instance_proc`]'s shape and its invariant, one handle family along: it
    /// answers a `bool` and never a pointer, so `vkGetDeviceProcAddr` cannot return a host code
    /// address to translated ARM64 even by mistake.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when `name` cannot
    /// be rendered as a C string.
    fn has_device_proc(&self, device: HostDevice, name: &str) -> AbiResult<bool> {
        let _ = (device, name);
        Err(host_has_no(
            "VulkanHost::has_device_proc",
            "there is nothing that can say whether that command exists on this device -- and both \
             a thunk and a NULL would be this layer answering on a driver's behalf",
        ))
    }

    // ------------------------------------------------------------------------ stage 4

    /// `vkCreateSwapchainKHR`, forwarded.
    ///
    /// # This is where the surface stops being shareable
    ///
    /// A `VkSurfaceKHR` may be queried by anything; a **swapchain** takes the window. The
    /// specification says a native window may be associated with at most one swapchain at a time
    /// and reports the violation as `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR` *if the driver notices* —
    /// and this host has no validation layers (`docs/research/graphics-spike.md` §6), while
    /// `omni_gfx::Renderer` creates a swapchain on exactly the window the guest presents to. So
    /// the conflict is the implementation's to refuse **by name**, before a driver is asked, and
    /// `omni_gfx::claim` is what makes that structural.
    ///
    /// [`SwapchainRequest::old_swapchain`] is part of the same question rather than a separate
    /// one: recreating over the outgoing swapchain is the one case in which the window is already
    /// taken and the call must still succeed, because it is the *same* owner replacing itself.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, when the surface's window
    /// is already owned by another swapchain, or when the old swapchain is not on the surface
    /// being created over.
    fn create_swapchain(
        &self,
        device: HostDevice,
        request: &SwapchainRequest,
    ) -> AbiResult<DriverAnswer<HostSwapchain>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_swapchain",
            "there is no swapchain, and a `VK_SUCCESS` with a handle naming nothing would be \
             followed by a `vkQueuePresentKHR` that presents nowhere and says it presented",
        ))
    }

    /// `vkGetSwapchainImagesKHR`, forwarded, as tokens.
    ///
    /// **The order is the driver's and must not change between calls**, for
    /// [`VulkanHost::physical_devices`]' reason and more sharply: the index into this list is the
    /// `imageIndex` `vkAcquireNextImageKHR` answers with, so a list that reordered would make the
    /// guest clear one image and present another.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `swapchain` is not a token this host issued.
    fn swapchain_images(
        &self,
        swapchain: HostSwapchain,
    ) -> AbiResult<DriverAnswer<Vec<HostImage>>> {
        let _ = swapchain;
        Err(host_has_no(
            "VulkanHost::swapchain_images",
            "there are no images to report, and an empty list is a swapchain that can present \
             nothing",
        ))
    }

    /// `vkDestroySwapchainKHR`, forwarded.
    ///
    /// Returns `AbiResult<()>` and not a `VkResult`, because `vkDestroySwapchainKHR` returns
    /// `void`: there is no code a guest could disbelieve, so an implementation that could not
    /// destroy has nothing to say except a refusal.
    ///
    /// **The images go with it.** An implementation drops its record of the swapchain's images
    /// here, and the shim drops their guest handles in the same call — a `VkImage` handle
    /// outliving its swapchain is a non-dispatchable value the driver would still look up.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `swapchain` is not a token this host issued.
    fn destroy_swapchain(&self, swapchain: HostSwapchain) -> AbiResult<()> {
        let _ = swapchain;
        Err(host_has_no(
            "VulkanHost::destroy_swapchain",
            "the swapchain cannot be destroyed, and returning quietly would leave the window \
             owned by a swapchain the guest believes is gone",
        ))
    }

    /// `vkCreateImageView`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when the bytes of
    /// `components` or `subresourceRange` are not the length the specification fixes.
    fn create_image_view(
        &self,
        device: HostDevice,
        request: &ImageViewRequest,
    ) -> AbiResult<DriverAnswer<HostImageView>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_image_view",
            "there is no `VkImageView`, and every attachment a render pass could ever bind is one",
        ))
    }

    /// `vkDestroyImageView`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `view` is not a token this host issued.
    fn destroy_image_view(&self, view: HostImageView) -> AbiResult<()> {
        let _ = view;
        Err(host_has_no(
            "VulkanHost::destroy_image_view",
            "the view cannot be destroyed, and returning quietly would leak one per frame for as \
             long as the guest runs",
        ))
    }

    /// `vkCreateSemaphore`, forwarded. `flags` is `VkSemaphoreCreateInfo::flags`, reserved and
    /// passed through.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_semaphore(
        &self,
        device: HostDevice,
        flags: u32,
    ) -> AbiResult<DriverAnswer<HostSemaphore>> {
        let _ = (device, flags);
        Err(host_has_no(
            "VulkanHost::create_semaphore",
            "there is no `VkSemaphore`, and a submission that waits on a handle naming nothing \
             either never runs or runs before the image it was supposed to wait for",
        ))
    }

    /// `vkDestroySemaphore`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `semaphore` is not a token this host issued.
    fn destroy_semaphore(&self, semaphore: HostSemaphore) -> AbiResult<()> {
        let _ = semaphore;
        Err(host_has_no("VulkanHost::destroy_semaphore", "the semaphore cannot be destroyed"))
    }

    /// `vkCreateFence`, forwarded. `flags` carries `VK_FENCE_CREATE_SIGNALED_BIT`, which is the
    /// bit every frame loop sets so that its first `vkWaitForFences` does not block forever.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_fence(&self, device: HostDevice, flags: u32) -> AbiResult<DriverAnswer<HostFence>> {
        let _ = (device, flags);
        Err(host_has_no(
            "VulkanHost::create_fence",
            "there is no `VkFence`, and a frame loop with no fence re-records a command buffer \
             the GPU is still reading",
        ))
    }

    /// `vkDestroyFence`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `fence` is not a token this host issued.
    fn destroy_fence(&self, fence: HostFence) -> AbiResult<()> {
        let _ = fence;
        Err(host_has_no("VulkanHost::destroy_fence", "the fence cannot be destroyed"))
    }

    /// `vkWaitForFences`, forwarded.
    ///
    /// The result is the `i32` the driver produced, **verbatim**, because `VK_TIMEOUT` is a
    /// *success* code the caller is required to handle: a frame loop that asked with a timeout and
    /// was told `VK_SUCCESS` when the fence had not signalled would re-record a command buffer the
    /// GPU is still executing.
    ///
    /// `timeout` is the guest's `uint64_t` nanoseconds, passed through unchanged including
    /// `UINT64_MAX`, which means "wait forever" and which an implementation must not clamp —
    /// a clamped wait returns `VK_TIMEOUT` for a frame that was merely slow.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when the fences do not
    /// all belong to `device`.
    fn wait_for_fences(
        &self,
        device: HostDevice,
        fences: &[HostFence],
        wait_all: bool,
        timeout: u64,
    ) -> AbiResult<i32> {
        let _ = (device, fences, wait_all, timeout);
        Err(host_has_no(
            "VulkanHost::wait_for_fences",
            "there is nothing to wait on -- and `VK_SUCCESS` here is the plausible answer Global \
             Constraint 1 exists for, because it says work finished that was never submitted",
        ))
    }

    /// `vkResetFences`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn reset_fences(
        &self,
        device: HostDevice,
        fences: &[HostFence],
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (device, fences);
        Err(host_has_no(
            "VulkanHost::reset_fences",
            "the fences cannot be reset, and a frame loop whose fence stays signalled never waits \
             again",
        ))
    }

    /// `vkCreateCommandPool`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_command_pool(
        &self,
        device: HostDevice,
        flags: u32,
        queue_family: u32,
    ) -> AbiResult<DriverAnswer<HostCommandPool>> {
        let _ = (device, flags, queue_family);
        Err(host_has_no(
            "VulkanHost::create_command_pool",
            "there is no `VkCommandPool` and therefore nowhere for a command buffer to come from",
        ))
    }

    /// `vkAllocateCommandBuffers`, forwarded.
    ///
    /// The list is in allocation order and its length is `commandBufferCount`; the shim writes
    /// them into the guest's array in that order, which is what a caller that indexes it expects.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn allocate_command_buffers(
        &self,
        pool: HostCommandPool,
        level: u32,
        count: u32,
    ) -> AbiResult<DriverAnswer<Vec<HostCommandBuffer>>> {
        let _ = (pool, level, count);
        Err(host_has_no(
            "VulkanHost::allocate_command_buffers",
            "there are no command buffers, and handles naming nothing would be *dispatchable* \
             handles naming nothing -- a driver dereferences one",
        ))
    }

    /// `vkBeginCommandBuffer`, forwarded. `flags` is `VkCommandBufferBeginInfo::flags`.
    ///
    /// There is no `pInheritanceInfo`: it is meaningful only for a **secondary** command buffer
    /// inside a render pass, and stage 4 has no render pass. The shim refuses a non-NULL one by
    /// name rather than dropping it.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn begin_command_buffer(
        &self,
        buffer: HostCommandBuffer,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (buffer, flags);
        Err(host_has_no(
            "VulkanHost::begin_command_buffer",
            "recording cannot start, and a `VK_SUCCESS` here would be followed by `vkCmd*` calls \
             recorded into nothing and a submission of an empty buffer that presents a blank frame",
        ))
    }

    /// `vkEndCommandBuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn end_command_buffer(&self, buffer: HostCommandBuffer) -> AbiResult<DriverAnswer<()>> {
        let _ = buffer;
        Err(host_has_no(
            "VulkanHost::end_command_buffer",
            "recording cannot be ended, so nothing can be submitted",
        ))
    }

    /// `vkResetCommandBuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn reset_command_buffer(
        &self,
        buffer: HostCommandBuffer,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (buffer, flags);
        Err(host_has_no(
            "VulkanHost::reset_command_buffer",
            "the buffer cannot be reset, and re-recording over commands that are still there \
             would submit last frame's work as well as this frame's",
        ))
    }

    /// `vkResetCommandPool`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn reset_command_pool(
        &self,
        pool: HostCommandPool,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (pool, flags);
        Err(host_has_no("VulkanHost::reset_command_pool", "the pool cannot be reset"))
    }

    /// `vkFreeCommandBuffers`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when a buffer does not
    /// belong to `pool` — which the specification makes undefined behaviour and which no
    /// validation layer on this machine would catch.
    fn free_command_buffers(
        &self,
        pool: HostCommandPool,
        buffers: &[HostCommandBuffer],
    ) -> AbiResult<()> {
        let _ = (pool, buffers);
        Err(host_has_no(
            "VulkanHost::free_command_buffers",
            "the buffers cannot be freed, and returning quietly would leak one per frame",
        ))
    }

    /// Every command buffer still allocated from `pool`, as tokens.
    ///
    /// **Not a Vulkan command**, and it is here for the same reason
    /// [`VulkanHost::swapchain_images`] is asked again inside `vkDestroySwapchainKHR`: destroying
    /// a pool frees every buffer allocated from it, and the shim has to drop those guest handles
    /// in the same call. A `VkCommandBuffer` is **dispatchable**, so a handle left behind resolves
    /// to a token whose host object is freed memory the driver would dereference — which Global
    /// Constraint 11 calls Critical. The only participant that knows which buffers a pool owns is
    /// the one that allocated them.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn command_buffers_of(&self, pool: HostCommandPool) -> AbiResult<Vec<HostCommandBuffer>> {
        let _ = pool;
        Err(host_has_no(
            "VulkanHost::command_buffers_of",
            "there is no way to say which command buffers a pool owns, so destroying it would \
             leave the guest holding dispatchable handles to freed memory",
        ))
    }

    /// `vkDestroyCommandPool`, forwarded. Frees every buffer allocated from it.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn destroy_command_pool(&self, pool: HostCommandPool) -> AbiResult<()> {
        let _ = pool;
        Err(host_has_no("VulkanHost::destroy_command_pool", "the pool cannot be destroyed"))
    }

    /// `vkCmdPipelineBarrier`, forwarded.
    ///
    /// Returns `AbiResult<()>`: every `vkCmd*` returns `void` and records into a command buffer
    /// rather than executing, so there is no `VkResult` at all — the driver reports a bad
    /// recording at `vkEndCommandBuffer`, or does not report it.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_pipeline_barrier(
        &self,
        buffer: HostCommandBuffer,
        barrier: &PipelineBarrier,
    ) -> AbiResult<()> {
        let _ = (buffer, barrier);
        Err(host_has_no(
            "VulkanHost::cmd_pipeline_barrier",
            "the barrier is not recorded -- and a layout transition that silently did not happen \
             is a swapchain image cleared in the wrong layout, which on this machine is undefined \
             behaviour with no validation layer to report it",
        ))
    }

    /// `vkCmdClearColorImage`, forwarded.
    ///
    /// `colour` is the sixteen bytes of the `VkClearColorValue` **union**, carried as bytes for
    /// the reason the union exists: which of `float32[4]`, `int32[4]` and `uint32[4]` those bytes
    /// mean is decided by the image's format, not by the caller, so interpreting them here would
    /// be this layer choosing — and choosing wrong for an integer-format image, silently.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when `ranges` holds an
    /// entry that is not [`IMAGE_SUBRESOURCE_RANGE_BYTES`](super::IMAGE_SUBRESOURCE_RANGE_BYTES)
    /// bytes.
    fn cmd_clear_color_image(
        &self,
        buffer: HostCommandBuffer,
        image: HostImageRef,
        layout: u32,
        colour: [u8; 16],
        ranges: &[Vec<u8>],
    ) -> AbiResult<()> {
        let _ = (buffer, image, layout, colour, ranges);
        Err(host_has_no(
            "VulkanHost::cmd_clear_color_image",
            "nothing is recorded, so the frame that is presented is whatever the presentation \
             engine last had in that image",
        ))
    }

    /// `vkAcquireNextImageKHR`, forwarded. See [`Acquired`] for why the answer is not a
    /// [`DriverAnswer`].
    ///
    /// **`VK_ERROR_OUT_OF_DATE_KHR` and `VK_SUBOPTIMAL_KHR` are answers, not failures.** An
    /// implementation returns them as [`Acquired::result`] and the shim puts them in the guest's
    /// `X0` unchanged. Recreating the swapchain here would be this layer taking a decision that
    /// belongs to whoever owns the swapchain, which is the guest.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn acquire_next_image(
        &self,
        swapchain: HostSwapchain,
        timeout: u64,
        semaphore: Option<HostSemaphore>,
        fence: Option<HostFence>,
    ) -> AbiResult<Acquired> {
        let _ = (swapchain, timeout, semaphore, fence);
        Err(host_has_no(
            "VulkanHost::acquire_next_image",
            "there is no image to acquire, and an index this layer chose would be an index into a \
             swapchain nothing created",
        ))
    }

    /// `vkQueueSubmit`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when a command buffer
    /// or semaphore does not belong to the queue's device.
    fn queue_submit(
        &self,
        queue: HostQueue,
        submits: &[SubmitRequest],
        fence: Option<HostFence>,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (queue, submits, fence);
        Err(host_has_no(
            "VulkanHost::queue_submit",
            "nothing is submitted -- and `VK_SUCCESS` here is the exact defect rule 1 exists for, \
             because the fence the guest then waits on would never signal",
        ))
    }

    /// `vkQueuePresentKHR`, forwarded. See [`Presented`].
    ///
    /// **This is the call rule 1 was written about.** A `vkQueuePresentKHR` that answers
    /// `VK_SUCCESS` without presenting is indistinguishable from one that presented, for exactly
    /// as long as nobody looks at the screen.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn queue_present(&self, queue: HostQueue, present: &PresentRequest) -> AbiResult<Presented> {
        let _ = (queue, present);
        Err(host_has_no(
            "VulkanHost::queue_present",
            "nothing is presented, and this is the one call where a fabricated `VK_SUCCESS` is \
             invisible until somebody looks at the screen",
        ))
    }

    /// `vkQueueWaitIdle`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `queue` is not a token this host issued.
    fn queue_wait_idle(&self, queue: HostQueue) -> AbiResult<DriverAnswer<()>> {
        let _ = queue;
        Err(host_has_no(
            "VulkanHost::queue_wait_idle",
            "there is nothing to wait for, and a `VK_SUCCESS` would tell the guest the GPU had \
             finished work it never started -- after which it is free to destroy what that work \
             was reading",
        ))
    }

    /// `vkDeviceWaitIdle`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn device_wait_idle(&self, device: HostDevice) -> AbiResult<DriverAnswer<()>> {
        let _ = device;
        Err(host_has_no(
            "VulkanHost::device_wait_idle",
            "there is nothing to wait for; see `queue_wait_idle` for what a fabricated success \
             licenses the guest to do next",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token prints as an ordinal, so nobody reads it as an address.
    #[test]
    fn a_host_instance_prints_as_an_ordinal_and_not_as_a_pointer() {
        let token = HostInstance::from_token(3);
        assert_eq!(format!("{token:?}"), "HostInstance(#3)");
        assert_eq!(token.token(), 3);
        assert_ne!(HostInstance::from_token(0), token);
    }

    /// **Every stage 3 token prints as an ordinal too**, which is the one thing the macro exists
    /// to make structural.
    ///
    /// A `HostQueue` whose `Debug` printed a bare `u64` would be indistinguishable in a log from
    /// a driver pointer, and the whole claim of this file is that no driver pointer is here.
    #[test]
    fn every_stage_three_token_prints_as_an_ordinal_and_not_as_a_pointer() {
        assert_eq!(format!("{:?}", HostPhysicalDevice::from_token(0)), "HostPhysicalDevice(#0)");
        assert_eq!(format!("{:?}", HostSurface::from_token(1)), "HostSurface(#1)");
        assert_eq!(format!("{:?}", HostDevice::from_token(2)), "HostDevice(#2)");
        assert_eq!(format!("{:?}", HostQueue::from_token(3)), "HostQueue(#3)");
        assert_eq!(HostQueue::from_token(3).token(), 3);
        assert_ne!(HostDevice::from_token(2), HostDevice::from_token(3));
    }

    /// **A host that has not implemented a stage 3 method refuses naming it**, rather than
    /// answering.
    ///
    /// The default bodies are the one place in this file where "not implemented" is expressible at
    /// all, so this is the test that says what they do. A `VulkanHost` with none of stage 3 is a
    /// valid stage 2a driver and every stage 3 call on it fails by name.
    #[test]
    fn an_unimplemented_stage_three_method_refuses_naming_the_trait_method() {
        #[derive(Debug)]
        struct StageTwoOnly;
        impl VulkanHost for StageTwoOnly {
            fn platform_surface_extension(&self) -> AbiResult<String> {
                Ok("VK_KHR_win32_surface".to_string())
            }
            fn instance_extensions(
                &self,
                _layer: Option<&str>,
            ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
                Ok(DriverAnswer::Ok(Vec::new()))
            }
            fn create_instance(
                &self,
                _request: &InstanceRequest,
            ) -> AbiResult<DriverAnswer<HostInstance>> {
                Ok(DriverAnswer::Ok(HostInstance::from_token(0)))
            }
            fn has_instance_proc(&self, _i: HostInstance, _n: &str) -> AbiResult<bool> {
                Ok(true)
            }
        }

        let host = StageTwoOnly;
        let error = host
            .physical_devices(HostInstance::from_token(0))
            .expect_err("a stage 2a host has no stage 3");
        assert_eq!(error.symbol(), Some("VulkanHost::physical_devices"));
        let text = error.to_string();
        assert!(text.contains("count of zero"), "it says what the wrong answer would be: {text}");
        assert!(text.contains("GfxVulkanHost"), "and which implementation has it: {text}");

        let error = host
            .device_queue(HostDevice::from_token(0), 0, 0)
            .expect_err("nor a queue");
        assert_eq!(error.symbol(), Some("VulkanHost::device_queue"));
    }

    /// The two arms of [`DriverAnswer`] are not the same value, which is the whole of D22's point
    /// here: a driver's `VK_ERROR_INCOMPATIBLE_DRIVER` and a seam that could not ask must not be
    /// spelled the same way.
    #[test]
    fn a_driver_failure_is_not_equal_to_a_success() {
        let ok: DriverAnswer<u32> = DriverAnswer::Ok(0);
        let failed: DriverAnswer<u32> = DriverAnswer::Failed(0);
        assert_ne!(ok, failed, "a zero result and a zero VkResult are different facts");
    }
}
