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
/// **Stage 4 wrote this with one variant and said why**: `vkCreateImage` produces images that are
/// *not* a swapchain's, and a host matching on a bare [`HostImage`] would silently treat one as
/// the other. Stage 5 is that second variant arriving, and the prediction held — the two are the
/// same Vulkan type, the same 64-bit non-dispatchable value, and completely different objects. One
/// is owned by its swapchain, already has memory and must never be destroyed by the guest; the
/// other is owned by the guest, has **no memory at all** until `vkBindImageMemory`, and must be.
/// A `vkDestroyImage` of a swapchain image is the mistake this enum makes unrepresentable, and no
/// validation layer on this machine would have caught it (`docs/research/graphics-spike.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostImageRef {
    /// An image `vkGetSwapchainImagesKHR` produced.
    Swapchain(HostImage),
    /// An image the guest made with `vkCreateImage`.
    Created(HostCreatedImage),
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
    /// `pNext`, as the flat structures the guest chained, in its order -- empty for a null
    /// `pNext`. Only [`FLAT_STRUCTURES`](super::FLAT_STRUCTURES) reach here; see
    /// [`ChainLink`].
    pub chain: Vec<ChainLink>,
}

/// The five scalars `vkGetPhysicalDeviceImageFormatProperties` asks about, named.
///
/// A struct rather than five parameters, for [`counted::Array`](super::counted)'s reason: they are
/// all 32-bit and a positional call that transposed `tiling` and `image_type` would compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageFormatQuery {
    /// `VkFormat`.
    pub format: i32,
    /// `VkImageType`.
    pub image_type: i32,
    /// `VkImageTiling`.
    pub tiling: i32,
    /// `VkImageUsageFlags`.
    pub usage: u32,
    /// `VkImageCreateFlags`.
    pub flags: u32,
}

/// One structure of a guest `pNext` chain: its `sType` and the member bytes after `pNext`.
///
/// **No pointer.** The guest's `pNext` values are addresses in guest memory and never reach the
/// driver: a host rebuilds the chain in its own memory, in this order, and links it itself. The
/// members are bytes for [`DeviceRequest::features`]' reason -- every structure that can be here
/// is flat (`VkBool32`s after `pNext`), so the guest's bytes are the driver's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainLink {
    /// The structure's `VkStructureType`.
    pub s_type: u32,
    /// Its members: [`FlatStructure::member_bytes`](super::FlatStructure::member_bytes) bytes,
    /// starting at offset 16 and stopping before the tail padding.
    pub body: Vec<u8>,
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

// ============================================================ stage 5's thirteen handle families
//
// **Thirteen new families, and the split between them is the one thing worth reading first.**
// Stage 4's seven were all objects with a lifetime the guest controls. These are too, with one
// difference that decides the shape of the two that matter: [`HostDeviceMemory`] is the only
// family in this file whose object the guest can **write to directly**, because `vkMapMemory`
// hands back an address the guest stores through without this layer seeing it again. That is why
// [`MemoryAllocation::host_pointer`] exists at all, and why it is a guest address rather than a
// host one.

host_token! {
    /// One `VkDeviceMemory` a host allocated. **Non-dispatchable.**
    ///
    /// # The one allocation that is not the driver's alone
    ///
    /// Two completely different things travel under this token, split at `vkAllocateMemory` on the
    /// memory type index the guest supplied:
    ///
    /// * a **forwarded** allocation, for a type with no `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`. The
    ///   driver allocated it, nothing here can address it, and `vkMapMemory` on one is a refusal
    ///   naming the type — which is what the specification requires of it too.
    /// * an **imported** allocation, for a host-visible type. The bytes are a `GuestSpace` mapping
    ///   this layer made, handed to the driver through `VkImportMemoryHostPointerInfoEXT`, so that
    ///   `vkMapMemory` can answer with an address **inside the guest's own address space**. See
    ///   [`MemoryAllocation::host_pointer`] for the measurement that settled this, and
    ///   [`VulkanHost::map_memory`] for the invariant it is checked against.
    ///
    /// An implementation has to keep the two apart, because freeing them is the same call and
    /// unmapping the guest range is not.
    HostDeviceMemory
}

host_token! {
    /// One `VkBuffer` a host created. **Non-dispatchable.**
    ///
    /// Created with no memory bound: `vkCreateBuffer` makes a buffer, `vkGetBufferMemoryRequirements`
    /// says what it needs, and `vkBindBufferMemory` is a separate call. A buffer used before it is
    /// bound is undefined behaviour the specification does not require a driver to catch, and there
    /// are no validation layers on this machine — so an implementation that can cheaply record
    /// whether a binding happened should.
    HostBuffer
}

host_token! {
    /// One `VkImage` the **guest** created with `vkCreateImage`. **Non-dispatchable.**
    ///
    /// Deliberately **not** [`HostImage`], which is a swapchain's. The two are the same Vulkan type
    /// and completely different objects: a swapchain image is owned by its swapchain, has memory
    /// already, and must not be destroyed; this one is owned by the guest, has no memory until
    /// `vkBindImageMemory`, and must be. [`HostImageRef`] is what keeps a call that accepts either
    /// from confusing them, and it is why that enum existed in stage 4 with one variant.
    HostCreatedImage
}

host_token! {
    /// One `VkSampler` a host created. **Non-dispatchable.**
    HostSampler
}

host_token! {
    /// One `VkShaderModule` a host created. **Non-dispatchable.**
    ///
    /// **The SPIR-V needed no translation.** It is the same bytes on both sides — a stream of
    /// little-endian 32-bit words whose layout the SPIR-V specification fixes independently of any
    /// host — so what crosses this seam is the guest's own bytes, copied out through `GuestMem`
    /// because they are a guest pointer this layer must validate, and not because they had to be
    /// changed.
    HostShaderModule
}

host_token! {
    /// One `VkPipelineLayout` a host created. **Non-dispatchable.**
    HostPipelineLayout
}

host_token! {
    /// One `VkRenderPass` a host created. **Non-dispatchable.**
    HostRenderPass
}

host_token! {
    /// One `VkFramebuffer` a host created. **Non-dispatchable.**
    HostFramebuffer
}

host_token! {
    /// One `VkPipeline` a host created. **Non-dispatchable.**
    HostPipeline
}

host_token! {
    /// One `VkPipelineCache` a host created. **Non-dispatchable.**
    HostPipelineCache
}

host_token! {
    /// One `VkQueryPool` a host created. **Non-dispatchable.**
    HostQueryPool
}

/// `VkQueryPoolCreateInfo`, decoded: every member after `pNext`, which is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryPoolRequest {
    /// `flags`. Reserved; passed through.
    pub flags: u32,
    /// `queryType`: `VK_QUERY_TYPE_TIMESTAMP` (2) for the engine's GPU timer.
    pub query_type: u32,
    /// `queryCount`.
    pub query_count: u32,
    /// `pipelineStatistics`, meaningful only for `VK_QUERY_TYPE_PIPELINE_STATISTICS`.
    pub pipeline_statistics: u32,
}

host_token! {
    /// One `VkDescriptorSetLayout` a host created. **Non-dispatchable.**
    HostDescriptorSetLayout
}

host_token! {
    /// One `VkDescriptorPool` a host created. **Non-dispatchable.**
    ///
    /// The pool owns every set allocated from it, exactly as [`HostCommandPool`] owns its buffers,
    /// and `vkResetDescriptorPool` frees them all at once without naming any of them. An
    /// implementation must drop its record of the sets there, and the shim drops their guest
    /// handles in the same call.
    HostDescriptorPool
}

host_token! {
    /// One `VkDescriptorSet` a host allocated. **Non-dispatchable.**
    ///
    /// Unlike `VkCommandBuffer`, which is dispatchable and which a driver dereferences, this is a
    /// 64-bit value the driver looks up — so a forged one binds *some other* set, and the symptom
    /// is a draw that samples a texture nobody chose. It is behind a registry for exactly the
    /// reason [`HostSurface`] is.
    HostDescriptorSet
}

/// What a host driver can do with one memory type, and whether this layer can back it.
///
/// # Why the shim has to ask before it allocates
///
/// `vkAllocateMemory` arrives with a `memoryTypeIndex` and nothing else. The split this stage
/// rests on — forward a device-local allocation, **import** a host-visible one out of `GuestSpace`
/// — is decided entirely by that index, and the only participant that knows what the index means
/// is the driver. So the shim asks, once per allocation, and the answer is three facts rather than
/// a boolean because the three lead to three different outcomes:
///
/// * not host-visible → an ordinary forward, and `vkMapMemory` on it is a refusal;
/// * host-visible and importable → a `GuestSpace` mapping, imported;
/// * host-visible and **not** importable → a refusal naming the type, because this layer cannot
///   produce an address the guest may store through and inventing one is Global Constraint 11.
///
/// The third is not hypothetical and it is the reason
/// [`VulkanHost::physical_device_memory_properties`]' answer is rewritten: on this machine the
/// importable set is `0xc` while the host-visible set is `0x1c`, so the `DEVICE_LOCAL |
/// HOST_VISIBLE` ReBAR type is host-visible to the driver and unbackable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPlan {
    /// `VkMemoryType::propertyFlags` for this index, **as the driver reports it** — not as the
    /// guest was shown it. The two differ exactly where the rewrite masked a bit, and this is the
    /// one that decides what the allocation actually is.
    pub property_flags: u32,
    /// Whether `VK_EXT_external_memory_host` can import a host pointer into this memory type on
    /// this device, from `vkGetMemoryHostPointerPropertiesEXT`'s `memoryTypeBits`.
    pub importable: bool,
    /// `VkPhysicalDeviceExternalMemoryHostPropertiesEXT::minImportedHostPointerAlignment`, which
    /// both the imported pointer and its length must be a multiple of. Measured 4096 on this
    /// machine, which is `GuestSpace::page_size()` — and a host that reports a larger one is why
    /// this travels rather than being assumed.
    pub import_alignment: u64,
}

impl MemoryPlan {
    /// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`.
    pub const HOST_VISIBLE: u32 = 0x2;
    /// `VK_MEMORY_PROPERTY_HOST_COHERENT_BIT`.
    pub const HOST_COHERENT: u32 = 0x4;
    /// `VK_MEMORY_PROPERTY_HOST_CACHED_BIT`.
    pub const HOST_CACHED: u32 = 0x8;

    /// Whether the guest may map memory of this type at all.
    #[must_use]
    pub const fn host_visible(&self) -> bool {
        self.property_flags & Self::HOST_VISIBLE != 0
    }
}

/// One `vkAllocateMemory`, decoded, with this layer's half of the decision already made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAllocation {
    /// `VkMemoryAllocateInfo::allocationSize`, the guest's, unrounded.
    pub size: u64,
    /// `memoryTypeIndex`, the guest's. **The driver's numbering**, because the rewrite this layer
    /// applies to `vkGetPhysicalDeviceMemoryProperties` changes property *bits* and never the
    /// order or the count — so an index the guest chose from the edited list is still an index
    /// into the driver's own list.
    pub memory_type_index: u32,
    /// The **guest** address of the `GuestSpace` mapping to import, or `None` for an ordinary
    /// forwarded allocation.
    ///
    /// # Why this is a guest address handed to a driver, and why that is not the thing this crate
    /// forbids
    ///
    /// The rule this project holds is "never hand the **guest** a host pointer". This is the other
    /// direction, and under D4's identity mapping a guest address *is* a host address — so what
    /// the driver receives is an ordinary, committed, readable-writable page of this process,
    /// which is precisely what `VkImportMemoryHostPointerInfoEXT` asks for.
    ///
    /// The alternative was letting the driver allocate and handing its pointer back through
    /// `vkMapMemory`. That would **work**, silently, because D4 amendment 1 says `admit` governs
    /// this layer's own shims and not the guest's loads and stores — so the guest would happily
    /// store through a driver pointer outside `GuestSpace` until some later shim re-validated it,
    /// a long way from the cause. Measured on this machine and recorded in `docs/HANDOFF.md`:
    /// `VK_EXT_external_memory_host` is present, `minImportedHostPointerAlignment` is 4096 which
    /// is `GuestSpace::page_size()`, and `vkMapMemory` returned **the same pointer that was
    /// imported**.
    ///
    /// The range is `[host_pointer, host_pointer + import_length)`, both aligned to
    /// [`MemoryPlan::import_alignment`], and it stays mapped until `vkFreeMemory`.
    pub host_pointer: Option<u64>,
    /// The length of that mapping, which is [`size`](MemoryAllocation::size) rounded **up** to
    /// [`MemoryPlan::import_alignment`]. Zero when there is no mapping.
    ///
    /// Carried rather than recomputed because the driver is given this length, not `size`: the
    /// specification requires an imported host pointer's length to be a multiple of the alignment,
    /// and an implementation that passed `size` would be passing a number the driver rejects for
    /// every allocation whose size is not already a multiple of a page.
    pub import_length: u64,
}

/// One `VkBufferCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BufferRequest {
    /// `flags`.
    pub flags: u32,
    /// `size`, in bytes.
    pub size: u64,
    /// `usage`.
    pub usage: u32,
    /// `sharingMode`.
    pub sharing_mode: u32,
    /// `pQueueFamilyIndices`, owned. Empty for `VK_SHARING_MODE_EXCLUSIVE`.
    pub queue_families: Vec<u32>,
}

/// One `VkImageCreateInfo`, decoded out of guest memory.
///
/// `extent` travels as its three `uint32_t`s rather than as a named type, for
/// [`DeviceRequest::features`]' reason: `VkExtent3D` has no pointer and no hole, and three named
/// fields in this crate would be three chances to write `depth` where `height` belongs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRequest {
    /// `flags`.
    pub flags: u32,
    /// `imageType`.
    pub image_type: u32,
    /// `format`.
    pub format: u32,
    /// `extent`, as `[width, height, depth]`.
    pub extent: [u32; 3],
    /// `mipLevels`.
    pub mip_levels: u32,
    /// `arrayLayers`.
    pub array_layers: u32,
    /// `samples`.
    pub samples: u32,
    /// `tiling`.
    pub tiling: u32,
    /// `usage`.
    pub usage: u32,
    /// `sharingMode`.
    pub sharing_mode: u32,
    /// `pQueueFamilyIndices`, owned.
    pub queue_families: Vec<u32>,
    /// `initialLayout`.
    pub initial_layout: u32,
}

/// One `VkPipelineLayoutCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineLayoutRequest {
    /// `flags`.
    pub flags: u32,
    /// `pSetLayouts`, resolved to tokens, in the guest's order — **which is the set number**, so
    /// reordering would rebind every descriptor set the pipeline has.
    pub set_layouts: Vec<HostDescriptorSetLayout>,
    /// `pPushConstantRanges`, as their
    /// [`PUSH_CONSTANT_RANGE_BYTES`](super::PUSH_CONSTANT_RANGE_BYTES) bytes each. Three
    /// `uint32_t`s with no handle in them.
    pub push_constant_ranges: Vec<Vec<u8>>,
}

/// One `VkDescriptorSetLayoutBinding`, decoded.
///
/// The one member that is a handle is `pImmutableSamplers`, which is why this is a struct rather
/// than bytes: an immutable sampler is baked into the layout and cannot be replaced by a later
/// `vkUpdateDescriptorSets`, so a guest-chosen handle reaching a driver here would be a sampler
/// nothing could subsequently correct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorBinding {
    /// `binding`.
    pub binding: u32,
    /// `descriptorType`.
    pub descriptor_type: u32,
    /// `descriptorCount`.
    pub descriptor_count: u32,
    /// `stageFlags`.
    pub stage_flags: u32,
    /// `pImmutableSamplers`, resolved. Empty when the guest passed NULL, which is the usual case.
    pub immutable_samplers: Vec<HostSampler>,
}

/// One `VkDescriptorSetLayoutCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorSetLayoutRequest {
    /// `flags`.
    pub flags: u32,
    /// `pBindings`, in the guest's order.
    pub bindings: Vec<DescriptorBinding>,
}

/// One `VkDescriptorPoolCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DescriptorPoolRequest {
    /// `flags`.
    pub flags: u32,
    /// `maxSets`.
    pub max_sets: u32,
    /// `pPoolSizes`, as `(type, descriptorCount)` pairs. No handle in a `VkDescriptorPoolSize`.
    pub sizes: Vec<(u32, u32)>,
}

/// What one `VkWriteDescriptorSet` points at.
///
/// **Three variants rather than three optional vectors**, because the specification makes exactly
/// one of the three arrays live and which one is decided by `descriptorType`. A structure with all
/// three present would let a shim fill the wrong one and a host read it without noticing: a
/// `COMBINED_IMAGE_SAMPLER` write whose `pBufferInfo` was filled instead would produce a
/// descriptor pointing at nothing, and the draw that used it would sample black.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorWrites {
    /// `pImageInfo`: `(sampler, view, layout)` per descriptor. The sampler is `None` for the types
    /// that do not take one (`SAMPLED_IMAGE`, `STORAGE_IMAGE`, the input attachment), and the view
    /// is `None` for a bare `SAMPLER`.
    Images(Vec<(Option<HostSampler>, Option<HostImageView>, u32)>),
    /// `pBufferInfo`: `(buffer, offset, range)` per descriptor. `range` is the guest's, including
    /// `VK_WHOLE_SIZE`.
    Buffers(Vec<(HostBuffer, u64, u64)>),
}

/// One `VkWriteDescriptorSet`, decoded out of guest memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorWrite {
    /// `dstSet`.
    pub set: HostDescriptorSet,
    /// `dstBinding`.
    pub binding: u32,
    /// `dstArrayElement`.
    pub array_element: u32,
    /// `descriptorType`.
    pub descriptor_type: u32,
    /// What it points at. Its length **is** `descriptorCount`, which is the point of decoding it:
    /// the two cannot disagree once the array has been copied.
    pub writes: DescriptorWrites,
}

/// One `VkCopyDescriptorSet`, decoded out of guest memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorCopy {
    /// `srcSet`.
    pub source: HostDescriptorSet,
    /// `srcBinding`.
    pub source_binding: u32,
    /// `srcArrayElement`.
    pub source_element: u32,
    /// `dstSet`.
    pub destination: HostDescriptorSet,
    /// `dstBinding`.
    pub destination_binding: u32,
    /// `dstArrayElement`.
    pub destination_element: u32,
    /// `descriptorCount`.
    pub count: u32,
}

/// One `VkSubpassDescription`, decoded out of guest memory.
///
/// Every attachment list travels as its `VkAttachmentReference` **bytes** — two `uint32_t`s, an
/// index and a layout, with no handle — for [`DeviceRequest::features`]' reason. What is *not*
/// bytes is the distinction between "no resolve attachments" and "a resolve attachment per colour
/// attachment", and between a `pDepthStencilAttachment` that is NULL and one that is not: those
/// are the two places a dropped pointer would produce a render pass that compiles and renders
/// differently, so they are `Vec` emptiness and `Option` respectively rather than a count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubpassRequest {
    /// `flags`.
    pub flags: u32,
    /// `pipelineBindPoint`.
    pub bind_point: u32,
    /// `pInputAttachments`, flat.
    pub input_attachments: Vec<u8>,
    /// `pColorAttachments`, flat.
    pub color_attachments: Vec<u8>,
    /// `pResolveAttachments`, flat, or empty when the guest passed NULL.
    pub resolve_attachments: Vec<u8>,
    /// `pDepthStencilAttachment`, or `None` when the guest passed NULL.
    pub depth_stencil_attachment: Option<Vec<u8>>,
    /// `pPreserveAttachments`.
    pub preserve_attachments: Vec<u32>,
}

/// One `VkRenderPassCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderPassRequest {
    /// `flags`.
    pub flags: u32,
    /// `pAttachments`, as `VkAttachmentDescription` bytes, nine `uint32_t`s each and no handle.
    pub attachments: Vec<Vec<u8>>,
    /// `pSubpasses`, decoded.
    pub subpasses: Vec<SubpassRequest>,
    /// `pDependencies`, as `VkSubpassDependency` bytes, seven `uint32_t`s each and no handle.
    pub dependencies: Vec<Vec<u8>>,
}

/// One `VkFramebufferCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FramebufferRequest {
    /// `flags`.
    pub flags: u32,
    /// `renderPass`, as a token. A framebuffer is only compatible with render passes that match
    /// the one it was made with, and the driver checks that — against the render pass this names.
    pub render_pass: Option<HostRenderPass>,
    /// `pAttachments`, resolved, **in the guest's order**, which is the attachment index every
    /// `VkAttachmentReference` in the render pass refers to.
    pub attachments: Vec<HostImageView>,
    /// `width`.
    pub width: u32,
    /// `height`.
    pub height: u32,
    /// `layers`.
    pub layers: u32,
}

/// One `VkSpecializationInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Specialization {
    /// `pMapEntries`, as `(constantID, offset, size)`. `size` is a `size_t`, which is eight bytes
    /// on the guest's aarch64 LP64 and on this host alike.
    pub entries: Vec<(u32, u32, u64)>,
    /// `pData`, copied. Its length is `dataSize`.
    pub data: Vec<u8>,
}

/// One `VkPipelineShaderStageCreateInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShaderStage {
    /// `flags`.
    pub flags: u32,
    /// `stage`, a single bit of `VkShaderStageFlagBits`.
    pub stage: u32,
    /// `module`, as a token.
    pub module: Option<HostShaderModule>,
    /// `pName`, the entry point inside the module. **Not assumed to be `"main"`**: a module
    /// compiled from HLSL or with several entry points names one here, and substituting `"main"`
    /// would link a different shader.
    pub name: String,
    /// `pSpecializationInfo`, or `None`.
    pub specialization: Option<Specialization>,
}

/// `VkPipelineVertexInputStateCreateInfo`, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VertexInputState {
    /// `flags`.
    pub flags: u32,
    /// `pVertexBindingDescriptions`, as their
    /// [`VERTEX_INPUT_BINDING_BYTES`](super::VERTEX_INPUT_BINDING_BYTES) bytes each.
    pub bindings: Vec<Vec<u8>>,
    /// `pVertexAttributeDescriptions`, as their
    /// [`VERTEX_INPUT_ATTRIBUTE_BYTES`](super::VERTEX_INPUT_ATTRIBUTE_BYTES) bytes each.
    pub attributes: Vec<Vec<u8>>,
}

/// `VkPipelineViewportStateCreateInfo`, decoded.
///
/// # Why the counts are separate from the arrays
///
/// Because they are allowed to disagree, and the case where they do is the one this stage uses:
/// with `VK_DYNAMIC_STATE_VIEWPORT` in `pDynamicStates`, `pViewports` is **ignored and may be
/// NULL** while `viewportCount` is still required to be correct. A structure that stored only the
/// arrays would lose the count, and the pipeline would be created with zero viewports and fail at
/// the draw rather than here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewportState {
    /// `flags`.
    pub flags: u32,
    /// `viewportCount`.
    pub viewport_count: u32,
    /// `pViewports`, flat, or empty when it was NULL.
    pub viewports: Vec<u8>,
    /// `scissorCount`.
    pub scissor_count: u32,
    /// `pScissors`, flat, or empty when it was NULL.
    pub scissors: Vec<u8>,
}

/// `VkPipelineMultisampleStateCreateInfo`, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultisampleState {
    /// `flags`.
    pub flags: u32,
    /// `rasterizationSamples`.
    pub samples: u32,
    /// `sampleShadingEnable`.
    pub sample_shading: u32,
    /// `minSampleShading`, as its four raw bytes — a `float`, carried unmodified rather than
    /// through an `f32` so that a signalling NaN the guest wrote is the one the driver sees.
    pub min_sample_shading: [u8; 4],
    /// `pSampleMask`, which is `ceil(rasterizationSamples / 32)` words, or empty when NULL.
    pub sample_mask: Vec<u32>,
    /// `alphaToCoverageEnable`.
    pub alpha_to_coverage: u32,
    /// `alphaToOneEnable`.
    pub alpha_to_one: u32,
}

/// `VkPipelineColorBlendStateCreateInfo`, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColorBlendState {
    /// `flags`.
    pub flags: u32,
    /// `logicOpEnable`.
    pub logic_op_enable: u32,
    /// `logicOp`.
    pub logic_op: u32,
    /// `pAttachments`, as their
    /// [`COLOR_BLEND_ATTACHMENT_BYTES`](super::COLOR_BLEND_ATTACHMENT_BYTES) bytes each.
    pub attachments: Vec<Vec<u8>>,
    /// `blendConstants`, as its sixteen raw bytes. Four floats, carried for
    /// [`MultisampleState::min_sample_shading`]'s reason.
    pub blend_constants: [u8; 16],
}

/// One `VkGraphicsPipelineCreateInfo`, decoded out of guest memory.
///
/// # The largest structure this layer decodes, and the reason it is decoded at all
///
/// Every other fixed-layout Vulkan structure in this crate travels as its bytes, because it has no
/// pointer to follow. This one is nothing *but* pointers: nine sub-state pointers, two handles, an
/// array of shader stages each of which holds a handle and two more pointers, and a `pDynamicState`
/// whose presence changes what the other members mean. There is no byte image to pass through.
///
/// What is decoded is therefore only what has to be. Each sub-state's own **body** travels as bytes
/// wherever that body is flat — [`RasterizationState`](GraphicsPipelineRequest::rasterization) and
/// [`depth_stencil`](GraphicsPipelineRequest::depth_stencil) are the two that are — and is decoded
/// wherever it holds a pointer of its own.
///
/// # `None` is not `Default` here
///
/// Five of these members are legitimately NULL for a pipeline that does not use them: a pipeline
/// with `rasterizerDiscardEnable` set needs no viewport, multisample, depth-stencil or colour-blend
/// state at all, and one with no tessellation stages needs no tessellation state. A `Default`
/// substituted for a NULL would be a pipeline that rasterizes where the engine asked for one that
/// does not, so each is an `Option` and the host passes NULL back through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphicsPipelineRequest {
    /// `flags`.
    pub flags: u32,
    /// `pStages`, in the guest's order.
    pub stages: Vec<ShaderStage>,
    /// `pVertexInputState`.
    pub vertex_input: Option<VertexInputState>,
    /// `pInputAssemblyState`, as `(flags, topology, primitiveRestartEnable)`.
    pub input_assembly: Option<(u32, u32, u32)>,
    /// `pTessellationState`, as `(flags, patchControlPoints)`.
    pub tessellation: Option<(u32, u32)>,
    /// `pViewportState`.
    pub viewport: Option<ViewportState>,
    /// `pRasterizationState`, as the
    /// [`RASTERIZATION_STATE_BODY_BYTES`](super::RASTERIZATION_STATE_BODY_BYTES) bytes that follow
    /// its `pNext`. Eleven scalars, no pointer.
    pub rasterization: Option<Vec<u8>>,
    /// `pMultisampleState`.
    pub multisample: Option<MultisampleState>,
    /// `pDepthStencilState`, as the
    /// [`DEPTH_STENCIL_STATE_BODY_BYTES`](super::DEPTH_STENCIL_STATE_BODY_BYTES) bytes that follow
    /// its `pNext`. Two `VkStencilOpState`s and nine scalars, no pointer.
    pub depth_stencil: Option<Vec<u8>>,
    /// `pColorBlendState`.
    pub color_blend: Option<ColorBlendState>,
    /// `pDynamicState`'s `pDynamicStates`, or `None` when `pDynamicState` itself was NULL —
    /// which is **not** the same as an empty list, though the driver treats them alike.
    pub dynamic_states: Option<Vec<u32>>,
    /// `layout`, as a token.
    pub layout: Option<HostPipelineLayout>,
    /// `renderPass`, as a token.
    pub render_pass: Option<HostRenderPass>,
    /// `subpass`.
    pub subpass: u32,
    /// `basePipelineHandle`, as a token, or `None` for `VK_NULL_HANDLE`.
    pub base_pipeline: Option<HostPipeline>,
    /// `basePipelineIndex`, the guest's, including `-1`.
    pub base_pipeline_index: i32,
}

/// One `VkComputePipelineCreateInfo`, decoded out of guest memory.
///
/// A compute pipeline is one shader stage and a layout. The stage is **embedded** in the create
/// info rather than pointed at, which is the one layout difference from the graphics call that
/// matters, and it is the same [`ShaderStage`] the graphics call decodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComputePipelineRequest {
    /// `flags`.
    pub flags: u32,
    /// `stage`, which the shim has checked is `VK_SHADER_STAGE_COMPUTE_BIT`.
    pub stage: ShaderStage,
    /// `layout`, as a token.
    pub layout: Option<HostPipelineLayout>,
    /// `basePipelineHandle`, as a token, or `None` for `VK_NULL_HANDLE`.
    pub base_pipeline: Option<HostPipeline>,
    /// `basePipelineIndex`, the guest's, including `-1`.
    pub base_pipeline_index: i32,
}

/// What one `vkCreateGraphicsPipelines` or `vkCreateComputePipelines` did.
///
/// # Why this is not a [`DriverAnswer`], for a reason [`Acquired`]'s is not
///
/// The pipeline creation calls are the only ones in Vulkan that **partly succeed**. The
/// specification is explicit: when a pipeline fails to be created, `pPipelines` receives
/// `VK_NULL_HANDLE` in that slot, *the pipelines that succeeded are still valid and still the
/// application's to destroy*, and an error code is returned for the call as a whole. A
/// `DriverAnswer<Vec<HostPipeline>>` cannot say that. It would force the failure to be total, and
/// an implementation obeying it would drop handles the driver had created — which is a leak of the
/// most expensive object a renderer makes.
///
/// So the result is the `i32` the driver produced and the list travels beside it with a hole in it
/// where a pipeline failed. The shim writes `VK_NULL_HANDLE` for each `None`, which is what the
/// guest is owed, and registers a handle only for each `Some`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelinesCreated {
    /// The driver's own `VkResult`, verbatim. `VK_SUCCESS` when every pipeline was created.
    pub result: i32,
    /// One entry per request, in the guest's order. `None` is a pipeline the driver declined to
    /// create, and the guest receives `VK_NULL_HANDLE` for it.
    pub pipelines: Vec<Option<HostPipeline>>,
}

/// One `VkRenderPassBeginInfo`, decoded out of guest memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderPassBegin {
    /// `renderPass`, as a token.
    pub render_pass: Option<HostRenderPass>,
    /// `framebuffer`, as a token.
    pub framebuffer: Option<HostFramebuffer>,
    /// `renderArea`, as its [`RECT_2D_BYTES`](super::RECT_2D_BYTES) bytes: two `int32_t`s and two
    /// `uint32_t`s.
    pub render_area: Vec<u8>,
    /// `pClearValues`, as their sixteen raw bytes each.
    ///
    /// **The union travels whole**, for [`VulkanHost::cmd_clear_color_image`]'s reason: which
    /// member is live is decided by the attachment's format, which this layer does not know, and
    /// interpreting the bytes would be this layer choosing.
    pub clear_values: Vec<[u8; 16]>,
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

    /// `vkDestroyInstance`, forwarded, **only once every child is gone**.
    ///
    /// Measured: the engine's render thread destroys its instance on `APP_CMD_TERM_WINDOW`, after
    /// its device, and creates a new one when the window comes back.
    ///
    /// # Children first, and the implementation is the one that can see them
    ///
    /// The specification requires every `VkDevice` and `VkSurfaceKHR` made from the instance to be
    /// destroyed first (and every debug messenger, which this layer never creates). The guest-side
    /// registries do not record which instance a device or surface came from; an implementation
    /// does, so it refuses an instance with live children, **naming each kind with a count and a
    /// few tokens**, and leaves the instance exactly as it was.
    ///
    /// # The physical devices go with it
    ///
    /// A `VkPhysicalDevice` is enumerated, never created, and its lifetime is its instance's. The
    /// answer is the physical-device tokens that went with the instance, so the shim can drop the
    /// guest's handles for them, as `vkDestroyDevice` drops its queues'. An implementation must
    /// never hand the instance's token out again. Returns no `VkResult`: the call returns `void`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `instance` is not a token this host holds -- an instance already
    /// destroyed included -- or when a device or surface made from it is still alive.
    fn destroy_instance(&self, instance: HostInstance) -> AbiResult<Vec<HostPhysicalDevice>> {
        let _ = instance;
        Err(host_has_no(
            "VulkanHost::destroy_instance",
            "the instance cannot be destroyed, and returning quietly would leave the driver's \
             instance alive after the engine has let go of it",
        ))
    }

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

    /// `vkDestroySurfaceKHR`, forwarded: the end of the surface
    /// [`VulkanHost::create_platform_surface`] made.
    ///
    /// # Why it exists: measured, not anticipated
    ///
    /// Stage 3 had no destructor in this trait because the guest had never called one. A gate run
    /// that closed the app the way a device does (focus lost, `onPause`, `onSurfaceDestroyed`)
    /// measured that it does: the engine's render thread answers `APP_CMD_TERM_WINDOW` by
    /// destroying its swapchain and then its surface, `vkDestroySurfaceKHR(instance, surface,
    /// NULL)`, through the thunk `vkGetInstanceProcAddr` handed out. The refusal that call used
    /// to reach killed the render thread and hung the close.
    ///
    /// # What only an implementation can check, and must
    ///
    /// Two of the specification's rules for this call are about relationships the guest-side
    /// registry does not record, and this host has no validation layers to catch either
    /// (`docs/research/graphics-spike.md` §6):
    ///
    /// * the surface must have been created from `instance` -- an implementation keeps which
    ///   instance each surface came from, for [`VulkanHost::surface_support`]'s pairing check;
    /// * every `VkSwapchainKHR` created over the surface must already be destroyed -- **retired
    ///   ones included**, because `oldSwapchain` retires a swapchain without destroying it.
    ///
    /// Each is a refusal **naming both objects**, not a destroy the driver may or may not survive.
    /// Returns `AbiResult<()>` for [`VulkanHost::destroy_swapchain`]'s reason: the call returns
    /// `void`, so there is no code a guest could disbelieve.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued -- a surface already
    /// destroyed included -- when the surface belongs to another instance, or when a swapchain
    /// over it is still live.
    fn destroy_surface(&self, instance: HostInstance, surface: HostSurface) -> AbiResult<()> {
        let _ = (instance, surface);
        Err(host_has_no(
            "VulkanHost::destroy_surface",
            "the surface cannot be destroyed, and returning quietly would leave the driver's \
             surface alive over a window the guest believes it has let go of",
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

    /// `vkGetPhysicalDeviceFormatProperties`, as its
    /// [`FORMAT_PROPERTIES_BYTES`](super::FORMAT_PROPERTIES_BYTES) bytes.
    ///
    /// `format` is the guest's `VkFormat`, passed through: every value names the same format on
    /// every platform, and one the driver does not know is answered with no features by the driver
    /// itself.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn physical_device_format_properties(
        &self,
        device: HostPhysicalDevice,
        format: i32,
    ) -> AbiResult<Vec<u8>> {
        let _ = (device, format);
        Err(host_has_no(
            "VulkanHost::physical_device_format_properties",
            "there is no `VkFormatProperties` to write, and zeroed features are a format the \
             device cannot use at all -- the answer that quietly steers a renderer away from it",
        ))
    }

    /// `vkGetPhysicalDeviceImageFormatProperties`: the driver's
    /// [`IMAGE_FORMAT_PROPERTIES_BYTES`](super::IMAGE_FORMAT_PROPERTIES_BYTES) bytes, or its own
    /// failure -- `VK_ERROR_FORMAT_NOT_SUPPORTED` is an answer about the device, not an error of
    /// this layer's.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn physical_device_image_format_properties(
        &self,
        device: HostPhysicalDevice,
        query: ImageFormatQuery,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let _ = (device, query);
        Err(host_has_no(
            "VulkanHost::physical_device_image_format_properties",
            "there is no `VkImageFormatProperties` to write, and inventing \
             `VK_ERROR_FORMAT_NOT_SUPPORTED` would steer a renderer away from a format the \
             device may well support",
        ))
    }

    /// `vkGetPhysicalDeviceImageFormatProperties2` (or its `KHR` alias, as `entry` names it): the
    /// [`IMAGE_FORMAT_PROPERTIES_BYTES`](super::IMAGE_FORMAT_PROPERTIES_BYTES) bytes of the
    /// `VkImageFormatProperties` it heads, with each structure of `answers` answered in place --
    /// or the driver's own failure. `question` is the chain the guest hung from its
    /// `VkPhysicalDeviceImageFormatInfo2`, which the driver reads and does not write.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when the driver
    /// has no such entry point.
    fn physical_device_image_format_properties2(
        &self,
        device: HostPhysicalDevice,
        entry: &str,
        query: ImageFormatQuery,
        question: &[ChainLink],
        answers: &mut [ChainLink],
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let _ = (device, entry, query, question, answers);
        Err(host_has_no(
            "VulkanHost::physical_device_image_format_properties2",
            "there is no `VkImageFormatProperties2` to write, and inventing \
             `VK_ERROR_FORMAT_NOT_SUPPORTED` would steer a renderer away from a format the \
             device may well support",
        ))
    }

    /// `vkGetPhysicalDeviceFeatures2` (or its `KHR` alias, as `entry` names it): the
    /// [`PHYSICAL_DEVICE_FEATURES_BYTES`](super::PHYSICAL_DEVICE_FEATURES_BYTES) bytes of the
    /// `VkPhysicalDeviceFeatures` it heads, with each structure of `chain` answered **in place**.
    ///
    /// `entry` is the name the guest called through. The `KHR` spelling is valid on an instance
    /// that enabled `VK_KHR_get_physical_device_properties2`, the core one on a 1.1 instance, and
    /// forwarding the one the guest chose is what keeps the host inside what the guest asked for.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when the driver
    /// has no such entry point.
    fn physical_device_features2(
        &self,
        device: HostPhysicalDevice,
        entry: &str,
        chain: &mut [ChainLink],
    ) -> AbiResult<Vec<u8>> {
        let _ = (device, entry, chain);
        Err(host_has_no(
            "VulkanHost::physical_device_features2",
            "there is no `VkPhysicalDeviceFeatures2` to write, and zeroed members would be a \
             device that supports none of the features the guest chained -- believable, and so \
             the worst answer",
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

    /// `vkDestroyDevice`, forwarded, **only once every child is gone**.
    ///
    /// Measured: the engine's render thread tears its whole device down on
    /// `APP_CMD_TERM_WINDOW`, right after saving its pipeline cache.
    ///
    /// # Children first, and the implementation is the one that can see them
    ///
    /// The specification requires every object created from the device to have been destroyed
    /// first. The guest-side registries do not record which device an object came from; an
    /// implementation does -- it needs to, to destroy each one -- so it is the implementation that
    /// refuses a device with live children, **naming each kind with a count and a few tokens**,
    /// before the driver is asked. There are no validation layers on this machine, and a misused
    /// NVIDIA driver is measured to fail silently or corrupt the heap rather than report.
    ///
    /// # The queues go with it
    ///
    /// A `VkQueue` is retrieved, never created, and the guest never destroys one. The answer is the
    /// queue tokens that went with the device, so the shim can drop the guest's handles for them,
    /// as `vkDestroySwapchainKHR` drops a swapchain's images. Returns no `VkResult`: the call
    /// returns `void`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host holds -- a device already
    /// destroyed included -- or when anything created from it is still alive.
    fn destroy_device(&self, device: HostDevice) -> AbiResult<Vec<HostQueue>> {
        let _ = device;
        Err(host_has_no(
            "VulkanHost::destroy_device",
            "the device cannot be destroyed, and returning quietly would leave the driver's device \
             and everything on it alive after the engine has let go of them",
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

    // =========================================================== stage 5: memory and resources

    /// Which memory types on `physical` this host can **import a host pointer into**, as a mask of
    /// `1 << memoryTypeIndex`.
    ///
    /// # Why this is asked of a physical device and answered by a device-level call
    ///
    /// The only route the specification gives to this fact is
    /// `vkGetMemoryHostPointerPropertiesEXT`, which takes a `VkDevice` — and the call that needs
    /// the answer is `vkGetPhysicalDeviceMemoryProperties`, which an engine may make **before** it
    /// has created one. So the question is asked of the physical device and it is the
    /// implementation's problem to obtain a device to ask with, cache the answer, and be honest
    /// about it. It is not this crate's problem, and it must not be guessed at here: a rule like
    /// "host-visible and not device-local" reproduces the measured `0xc` on this machine and is a
    /// *derivation*, not a measurement, which is exactly the kind of plausible claim this project
    /// refuses to make about a driver.
    ///
    /// A host with no `VK_EXT_external_memory_host` answers **0**. That is a legitimate answer and
    /// the consequence is stated rather than hidden: every host-visible type is masked out of the
    /// list the guest sees, and any `vkAllocateMemory` from one is a refusal naming the type.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `physical` is not a token this host issued, or when the probe
    /// itself failed — which is a fact about this host and not an answer of zero.
    fn importable_memory_types(&self, physical: HostPhysicalDevice) -> AbiResult<u32> {
        let _ = physical;
        Err(host_has_no(
            "VulkanHost::importable_memory_types",
            "this layer cannot say which memory types it is able to back, so it cannot edit the \
             list the guest chooses from -- and an unedited list is a guest choosing a type whose \
             `vkMapMemory` this layer would then have to refuse, one call after telling it the \
             type was host-visible",
        ))
    }

    /// The device extensions this host **must** have enabled in order to satisfy the guest's
    /// Vulkan, whatever the guest itself asked for.
    ///
    /// # Why the default is an empty list and not a refusal
    ///
    /// Every other method added since stage 2a defaults to refusing by name, because the answer
    /// they would otherwise have to invent is one only a real driver can give. This one is
    /// different: "this host needs no extension the guest did not ask for" is a **complete and
    /// true** answer for a host that needs none, and every test double in this workspace is such a
    /// host — it creates no device and imports no memory. A default that refused would make
    /// `vkCreateDevice` fail for a double that is behaving correctly.
    ///
    /// [`GfxVulkanHost`] overrides it with exactly one name, `VK_EXT_external_memory_host`, and
    /// only when the physical device has it. The shim appends whatever comes back, records each
    /// addition as a [`RewriteSite::DeviceExtensionAdded`](super::RewriteSite), and a physical
    /// device without the extension therefore produces no rewrite, no addition and no failure —
    /// the consequence appears instead as the memory-type mask.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `physical` is not a token this host issued, or when the
    /// extension list cannot be read — which is a fact about this host, and must not be flattened
    /// into an empty list.
    fn device_extensions_required_by_host(
        &self,
        physical: HostPhysicalDevice,
    ) -> AbiResult<Vec<String>> {
        let _ = physical;
        Ok(Vec::new())
    }

    /// What one memory type index means on `device`, and whether this layer can back it.
    ///
    /// See [`MemoryPlan`] for the three outcomes and why the shim has to know before it allocates.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when
    /// `memory_type_index` is past the device's `memoryTypeCount` — which is a guest value and is
    /// refused here rather than reaching a driver, because indexing a driver's fixed array with a
    /// guest `uint32_t` is a host read out of bounds.
    fn memory_plan(&self, device: HostDevice, memory_type_index: u32) -> AbiResult<MemoryPlan> {
        let _ = (device, memory_type_index);
        Err(host_has_no(
            "VulkanHost::memory_plan",
            "there is no way to tell a device-local allocation from a host-visible one, and \
             guessing would either import memory the driver cannot import or forward an \
             allocation the guest is about to map",
        ))
    }

    /// `vkAllocateMemory`, forwarded — **importing the guest's pages when there are any**.
    ///
    /// When [`MemoryAllocation::host_pointer`] is `Some`, an implementation must chain a
    /// `VkImportMemoryHostPointerInfoEXT` with
    /// `handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT` and that pointer, and
    /// must pass [`MemoryAllocation::import_length`] as `allocationSize` rather than
    /// [`MemoryAllocation::size`] — the specification requires the imported length to be a
    /// multiple of `minImportedHostPointerAlignment` and the guest's size is not.
    ///
    /// When it is `None` the allocation is an ordinary forward and `allocationSize` is the guest's.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when an import was
    /// asked for and this host has no `VK_EXT_external_memory_host`.
    fn allocate_memory(
        &self,
        device: HostDevice,
        allocation: &MemoryAllocation,
    ) -> AbiResult<DriverAnswer<HostDeviceMemory>> {
        let _ = (device, allocation);
        Err(host_has_no(
            "VulkanHost::allocate_memory",
            "there is no `VkDeviceMemory`, and a handle naming nothing would be bound to a buffer \
             and then written through a pointer `vkMapMemory` invented",
        ))
    }

    /// `vkFreeMemory`, forwarded.
    ///
    /// **The guest's mapping is not this method's to unmap.** The shim made the `GuestSpace`
    /// mapping and the shim releases it, after this returns — so an implementation frees the
    /// driver's object and drops its own record, and nothing here touches guest address space.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `memory` is not a token this host issued.
    fn free_memory(&self, memory: HostDeviceMemory) -> AbiResult<()> {
        let _ = memory;
        Err(host_has_no(
            "VulkanHost::free_memory",
            "the allocation cannot be freed, and returning quietly would leak a device allocation \
             per texture for as long as the guest runs",
        ))
    }

    /// `vkMapMemory`, forwarded, answering **the address the driver produced**.
    ///
    /// # The invariant the shim checks this against, and why it is checked rather than assumed
    ///
    /// For an imported allocation the address must be the pointer that was imported, plus
    /// `offset`. That is what was measured on this machine — `vkMapMemory -> 0x1cdb4f89000, and
    /// the pointer imported was 0x1cdb4f89000` — and it is what makes the whole route work: the
    /// guest receives an address inside `GuestSpace`, `admit` admits it, and `HOST_COHERENT` is
    /// genuinely coherent because there is only one copy of the bytes.
    ///
    /// It is also a property of a driver rather than of the specification, which says only that
    /// the pointer is to the start of the range. So the shim compares, and a driver that answered
    /// anything else is a **refusal naming both addresses** rather than a host pointer handed to
    /// translated ARM64. The answer travels as a `u64` here precisely so that the comparison is
    /// possible; it is never what reaches the guest unchecked.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `memory` is not a token this host issued.
    fn map_memory(
        &self,
        memory: HostDeviceMemory,
        offset: u64,
        size: u64,
        flags: u32,
    ) -> AbiResult<DriverAnswer<u64>> {
        let _ = (memory, offset, size, flags);
        Err(host_has_no(
            "VulkanHost::map_memory",
            "there is no mapping, and the one thing that must never happen here is a plausible \
             address: the guest stores through whatever it is given, and D4 amendment 1 means \
             `admit` would not stop it",
        ))
    }

    /// `vkUnmapMemory`, forwarded. Returns `void` in Vulkan, so there is no code to carry.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `memory` is not a token this host issued.
    fn unmap_memory(&self, memory: HostDeviceMemory) -> AbiResult<()> {
        let _ = memory;
        Err(host_has_no("VulkanHost::unmap_memory", "the mapping cannot be released"))
    }

    /// `vkFlushMappedMemoryRanges`, forwarded. Each range is `(memory, offset, size)` with the
    /// guest's own `size`, including `VK_WHOLE_SIZE`.
    ///
    /// **On this machine every `HOST_VISIBLE` memory type is also `HOST_COHERENT`**
    /// (`docs/HANDOFF.md`), so a conforming engine is never *required* to call this — and one that
    /// calls it anyway is conforming too, which is why it is implemented rather than refused. What
    /// it must not become is a no-op: a host whose memory is not coherent would then lose every
    /// upload, silently, on the first frame.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn flush_mapped_memory_ranges(
        &self,
        device: HostDevice,
        ranges: &[(HostDeviceMemory, u64, u64)],
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (device, ranges);
        Err(host_has_no(
            "VulkanHost::flush_mapped_memory_ranges",
            "nothing is flushed, and a `VK_SUCCESS` would tell the guest its uploads are visible \
             to the GPU when they may still be in a host cache",
        ))
    }

    /// `vkInvalidateMappedMemoryRanges`, forwarded. See
    /// [`flush_mapped_memory_ranges`](VulkanHost::flush_mapped_memory_ranges).
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn invalidate_mapped_memory_ranges(
        &self,
        device: HostDevice,
        ranges: &[(HostDeviceMemory, u64, u64)],
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (device, ranges);
        Err(host_has_no(
            "VulkanHost::invalidate_mapped_memory_ranges",
            "nothing is invalidated, and the guest would read a stale host cache line where the \
             GPU's result should be",
        ))
    }

    /// `vkGetBufferMemoryRequirements`, forwarded, as the
    /// [`MEMORY_REQUIREMENTS_BYTES`](super::MEMORY_REQUIREMENTS_BYTES) bytes of a
    /// `VkMemoryRequirements`.
    ///
    /// Bytes for [`VulkanHost::physical_device_properties`]' reason: two `VkDeviceSize`s and a
    /// `uint32_t`, no pointer, no hole, identical on both targets.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn buffer_memory_requirements(&self, buffer: HostBuffer) -> AbiResult<Vec<u8>> {
        let _ = buffer;
        Err(host_has_no(
            "VulkanHost::buffer_memory_requirements",
            "there is no size, no alignment and no `memoryTypeBits` -- and a fabricated \
             `memoryTypeBits` is a guest allocating from a type the buffer cannot be bound to",
        ))
    }

    /// `vkGetImageMemoryRequirements`, forwarded. See
    /// [`buffer_memory_requirements`](VulkanHost::buffer_memory_requirements).
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `image` is not a token this host issued.
    fn image_memory_requirements(&self, image: HostCreatedImage) -> AbiResult<Vec<u8>> {
        let _ = image;
        Err(host_has_no(
            "VulkanHost::image_memory_requirements",
            "there is no size, no alignment and no `memoryTypeBits`; an image's requirements \
             differ from its own extent times its format, because tiling is the driver's",
        ))
    }

    /// `vkGetImageMemoryRequirements` on a **swapchain's** image, forwarded.
    ///
    /// The specification forbids binding memory to a swapchain image and destroying one, and
    /// neither is reachable here ([`HostImageRef`] keeps the families apart); it does not forbid
    /// asking what one needs. MEASURED: the engine asks it of its swapchain's images once the
    /// swapchain exists.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `image` is not a swapchain image this host holds.
    fn swapchain_image_memory_requirements(&self, image: HostImage) -> AbiResult<Vec<u8>> {
        let _ = image;
        Err(host_has_no(
            "VulkanHost::swapchain_image_memory_requirements",
            "there is no size, no alignment and no `memoryTypeBits` for the swapchain's image, \
             and the engine budgets its memory from them",
        ))
    }

    /// `vkBindBufferMemory`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when the buffer and the
    /// memory belong to different devices.
    fn bind_buffer_memory(
        &self,
        buffer: HostBuffer,
        memory: HostDeviceMemory,
        offset: u64,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (buffer, memory, offset);
        Err(host_has_no(
            "VulkanHost::bind_buffer_memory",
            "no memory is bound, and a `VK_SUCCESS` would let the guest write vertices into a \
             mapping the buffer has no relationship with",
        ))
    }

    /// `vkBindImageMemory`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when the image and the
    /// memory belong to different devices.
    fn bind_image_memory(
        &self,
        image: HostCreatedImage,
        memory: HostDeviceMemory,
        offset: u64,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (image, memory, offset);
        Err(host_has_no(
            "VulkanHost::bind_image_memory",
            "no memory is bound, and the first thing to notice would be a texture that samples as \
             whatever the driver left in that image",
        ))
    }

    /// `vkCreateBuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_buffer(
        &self,
        device: HostDevice,
        request: &BufferRequest,
    ) -> AbiResult<DriverAnswer<HostBuffer>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_buffer",
            "there is no `VkBuffer`, and every vertex, index and staging upload a renderer makes \
             is one",
        ))
    }

    /// `vkDestroyBuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn destroy_buffer(&self, buffer: HostBuffer) -> AbiResult<()> {
        let _ = buffer;
        Err(host_has_no("VulkanHost::destroy_buffer", "the buffer cannot be destroyed"))
    }

    /// `vkCreateImage`, forwarded. The result is a [`HostCreatedImage`] and **not** a
    /// [`HostImage`]; [`HostImageRef`] says why those are different families.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_image(
        &self,
        device: HostDevice,
        request: &ImageRequest,
    ) -> AbiResult<DriverAnswer<HostCreatedImage>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_image",
            "there is no `VkImage`, so there is nothing for a texture upload to be copied into",
        ))
    }

    /// `vkDestroyImage`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `image` is not a token this host issued.
    fn destroy_image(&self, image: HostCreatedImage) -> AbiResult<()> {
        let _ = image;
        Err(host_has_no("VulkanHost::destroy_image", "the image cannot be destroyed"))
    }

    /// `vkCreateSampler`, forwarded. `body` is the
    /// [`SAMPLER_CREATE_INFO_BODY_BYTES`](super::SAMPLER_CREATE_INFO_BODY_BYTES) bytes of
    /// `VkSamplerCreateInfo` that follow `pNext`.
    ///
    /// Bytes rather than sixteen named fields, for [`DeviceRequest::features`]' reason: they are
    /// sixteen scalars with no pointer and no hole, and `addressModeU` written where `addressModeV`
    /// belongs is a texture that wraps on the wrong axis and looks like an atlas bug.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when `body` is not
    /// the length the specification fixes.
    fn create_sampler(
        &self,
        device: HostDevice,
        body: &[u8],
    ) -> AbiResult<DriverAnswer<HostSampler>> {
        let _ = (device, body);
        Err(host_has_no(
            "VulkanHost::create_sampler",
            "there is no `VkSampler`, and a combined image sampler descriptor needs one",
        ))
    }

    /// `vkDestroySampler`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `sampler` is not a token this host issued.
    fn destroy_sampler(&self, sampler: HostSampler) -> AbiResult<()> {
        let _ = sampler;
        Err(host_has_no("VulkanHost::destroy_sampler", "the sampler cannot be destroyed"))
    }

    /// `vkCreateShaderModule`, forwarded. `code` is the guest's SPIR-V, **verbatim**.
    ///
    /// The bytes are the same on both sides: SPIR-V is a stream of little-endian 32-bit words
    /// whose meaning the SPIR-V specification fixes with no reference to a host, so there is
    /// nothing here to translate and the only thing this layer does is validate the pointer it
    /// came through. An implementation must not rewrite it, and must not accept a length that is
    /// not a multiple of four.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when `code`'s
    /// length is not a multiple of four.
    fn create_shader_module(
        &self,
        device: HostDevice,
        flags: u32,
        code: &[u8],
    ) -> AbiResult<DriverAnswer<HostShaderModule>> {
        let _ = (device, flags, code);
        Err(host_has_no(
            "VulkanHost::create_shader_module",
            "there is no `VkShaderModule`, and a pipeline built from one that names nothing would \
             be a pipeline with no shaders",
        ))
    }

    /// `vkDestroyShaderModule`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `module` is not a token this host issued.
    fn destroy_shader_module(&self, module: HostShaderModule) -> AbiResult<()> {
        let _ = module;
        Err(host_has_no("VulkanHost::destroy_shader_module", "the module cannot be destroyed"))
    }

    /// `vkCreatePipelineLayout`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn create_pipeline_layout(
        &self,
        device: HostDevice,
        request: &PipelineLayoutRequest,
    ) -> AbiResult<DriverAnswer<HostPipelineLayout>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_pipeline_layout",
            "there is no `VkPipelineLayout`, and it is what says which descriptor sets a draw may \
             bind",
        ))
    }

    /// `vkDestroyPipelineLayout`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `layout` is not a token this host issued.
    fn destroy_pipeline_layout(&self, layout: HostPipelineLayout) -> AbiResult<()> {
        let _ = layout;
        Err(host_has_no("VulkanHost::destroy_pipeline_layout", "the layout cannot be destroyed"))
    }

    /// `vkCreateRenderPass`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued, or when an attachment,
    /// reference or dependency blob is not the length the specification fixes.
    fn create_render_pass(
        &self,
        device: HostDevice,
        request: &RenderPassRequest,
    ) -> AbiResult<DriverAnswer<HostRenderPass>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_render_pass",
            "there is no `VkRenderPass`, and it is what decides whether the swapchain image is \
             cleared, loaded or left alone before a draw touches it",
        ))
    }

    /// `vkDestroyRenderPass`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pass` is not a token this host issued.
    fn destroy_render_pass(&self, pass: HostRenderPass) -> AbiResult<()> {
        let _ = pass;
        Err(host_has_no("VulkanHost::destroy_render_pass", "the render pass cannot be destroyed"))
    }

    /// `vkCreateFramebuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when the attachments do
    /// not all belong to `device`.
    fn create_framebuffer(
        &self,
        device: HostDevice,
        request: &FramebufferRequest,
    ) -> AbiResult<DriverAnswer<HostFramebuffer>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_framebuffer",
            "there is no `VkFramebuffer`, so a render pass has nothing to render into",
        ))
    }

    /// `vkDestroyFramebuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `framebuffer` is not a token this host issued.
    fn destroy_framebuffer(&self, framebuffer: HostFramebuffer) -> AbiResult<()> {
        let _ = framebuffer;
        Err(host_has_no("VulkanHost::destroy_framebuffer", "the framebuffer cannot be destroyed"))
    }

    /// `vkCreateQueryPool`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_query_pool(
        &self,
        device: HostDevice,
        request: &QueryPoolRequest,
    ) -> AbiResult<DriverAnswer<HostQueryPool>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_query_pool",
            "there is no `VkQueryPool`, and a GPU timer reading back queries from nothing would \
             report whatever its buffer held as the frame's GPU time",
        ))
    }

    /// `vkCmdResetQueryPool`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_reset_query_pool(
        &self,
        buffer: HostCommandBuffer,
        pool: HostQueryPool,
        first: u32,
        count: u32,
    ) -> AbiResult<()> {
        let _ = (buffer, pool, first, count);
        Err(host_has_no(
            "VulkanHost::cmd_reset_query_pool",
            "the queries are not reset, and a timestamp read back from an unreset query is \
             undefined",
        ))
    }

    /// `vkCmdWriteTimestamp`, forwarded. `stage` is the guest's `VkPipelineStageFlagBits`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_write_timestamp(
        &self,
        buffer: HostCommandBuffer,
        stage: u32,
        pool: HostQueryPool,
        query: u32,
    ) -> AbiResult<()> {
        let _ = (buffer, stage, pool, query);
        Err(host_has_no(
            "VulkanHost::cmd_write_timestamp",
            "no timestamp is written, and the GPU timer would read back whatever the query held",
        ))
    }

    /// `vkGetQueryPoolResults`, forwarded. `data` arrives holding **the guest's own bytes**, sized
    /// to exactly the span the driver writes; the host has the driver write into it and answers
    /// the driver's `VkResult` verbatim -- `VK_NOT_READY` included, with the unavailable queries'
    /// bytes left as they arrived.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    #[allow(clippy::too_many_arguments)]
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
        let _ = (device, pool, first, count, stride, flags, data);
        Err(host_has_no(
            "VulkanHost::get_query_pool_results",
            "no timestamp is read back, and the GPU timer would divide whatever the buffer held",
        ))
    }

    /// `vkDestroyQueryPool`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn destroy_query_pool(&self, pool: HostQueryPool) -> AbiResult<()> {
        let _ = pool;
        Err(host_has_no("VulkanHost::destroy_query_pool", "the query pool cannot be destroyed"))
    }

    /// `vkCreatePipelineCache`, forwarded. `initial_data` is `pInitialData`, copied, or empty.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_pipeline_cache(
        &self,
        device: HostDevice,
        flags: u32,
        initial_data: &[u8],
    ) -> AbiResult<DriverAnswer<HostPipelineCache>> {
        let _ = (device, flags, initial_data);
        Err(host_has_no(
            "VulkanHost::create_pipeline_cache",
            "there is no `VkPipelineCache`; a renderer that passes one to \
             `vkCreateGraphicsPipelines` would be passing a handle naming nothing",
        ))
    }

    /// `vkDestroyPipelineCache`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `cache` is not a token this host issued.
    fn destroy_pipeline_cache(&self, cache: HostPipelineCache) -> AbiResult<()> {
        let _ = cache;
        Err(host_has_no("VulkanHost::destroy_pipeline_cache", "the cache cannot be destroyed"))
    }

    /// `vkGetPipelineCacheData`, forwarded: the driver's **whole** cache blob, whose header names
    /// this machine's vendor, device and `pipelineCacheUUID`.
    ///
    /// Measured: the engine's render thread calls it on `APP_CMD_TERM_WINDOW`, to save the cache
    /// for the next launch's `vkCreatePipelineCache` to take back as `pInitialData`.
    ///
    /// # Whole, and never read through a short buffer
    ///
    /// The guest's two-call idiom is the shim's to run over the answer; the host's job is the
    /// blob. It must be read into a buffer of the size the driver reported, and the size must not
    /// be able to change in between. Measured on this machine's NVIDIA driver: asked to fill 2,766
    /// bytes of a 5,499-byte cache, it wrote all 5,499 -- 2,733 past the end of the buffer --
    /// then answered `VK_INCOMPLETE` with a count of 36, and the process died of
    /// `STATUS_HEAP_CORRUPTION`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when the cache belongs
    /// to another device.
    fn pipeline_cache_data(
        &self,
        device: HostDevice,
        cache: HostPipelineCache,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let _ = (device, cache);
        Err(host_has_no(
            "VulkanHost::pipeline_cache_data",
            "there is no cache blob to hand the engine, and a size of zero would tell it the \
             driver has nothing worth saving",
        ))
    }

    /// `vkCreateGraphicsPipelines`, forwarded. **One call, many pipelines, and it may partly
    /// succeed.**
    ///
    /// See [`PipelinesCreated`]: the specification requires `pPipelines` to be filled with
    /// `VK_NULL_HANDLE` for every pipeline that failed and a valid handle for every one that did
    /// not, *and* an error code to be returned. A signature that answered
    /// `DriverAnswer<Vec<HostPipeline>>` could not express that, and the shape it would force —
    /// treating any failure as total — would throw away pipelines the driver did create, which
    /// leaks them.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn create_graphics_pipelines(
        &self,
        device: HostDevice,
        cache: Option<HostPipelineCache>,
        requests: &[GraphicsPipelineRequest],
    ) -> AbiResult<PipelinesCreated> {
        let _ = (device, cache, requests);
        Err(host_has_no(
            "VulkanHost::create_graphics_pipelines",
            "there are no pipelines. **This is the method rule 1 is written about**: a \
             `VK_SUCCESS` with no pipeline behind it is believed, bound, drawn with, and produces \
             a frame that is empty for a reason nothing records",
        ))
    }

    /// `vkCreateComputePipelines`, forwarded, with the same partial-success answer as
    /// [`VulkanHost::create_graphics_pipelines`].
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn create_compute_pipelines(
        &self,
        device: HostDevice,
        cache: Option<HostPipelineCache>,
        requests: &[ComputePipelineRequest],
    ) -> AbiResult<PipelinesCreated> {
        let _ = (device, cache, requests);
        Err(host_has_no(
            "VulkanHost::create_compute_pipelines",
            "there are no compute pipelines, and a `VK_SUCCESS` without one would be bound and \
             dispatched with nothing behind it",
        ))
    }

    /// `vkDestroyPipeline`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pipeline` is not a token this host issued.
    fn destroy_pipeline(&self, pipeline: HostPipeline) -> AbiResult<()> {
        let _ = pipeline;
        Err(host_has_no("VulkanHost::destroy_pipeline", "the pipeline cannot be destroyed"))
    }

    /// `vkCreateDescriptorSetLayout`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn create_descriptor_set_layout(
        &self,
        device: HostDevice,
        request: &DescriptorSetLayoutRequest,
    ) -> AbiResult<DriverAnswer<HostDescriptorSetLayout>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_descriptor_set_layout",
            "there is no `VkDescriptorSetLayout`, and it is what a pipeline layout and a \
             descriptor set are both built from",
        ))
    }

    /// `vkDestroyDescriptorSetLayout`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `layout` is not a token this host issued.
    fn destroy_descriptor_set_layout(&self, layout: HostDescriptorSetLayout) -> AbiResult<()> {
        let _ = layout;
        Err(host_has_no(
            "VulkanHost::destroy_descriptor_set_layout",
            "the set layout cannot be destroyed",
        ))
    }

    /// `vkCreateDescriptorPool`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `device` is not a token this host issued.
    fn create_descriptor_pool(
        &self,
        device: HostDevice,
        request: &DescriptorPoolRequest,
    ) -> AbiResult<DriverAnswer<HostDescriptorPool>> {
        let _ = (device, request);
        Err(host_has_no(
            "VulkanHost::create_descriptor_pool",
            "there is no `VkDescriptorPool` and therefore nowhere for a descriptor set to come \
             from",
        ))
    }

    /// `vkDestroyDescriptorPool`, forwarded. **Every set allocated from it goes with it**, the way
    /// a command pool takes its buffers.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn destroy_descriptor_pool(&self, pool: HostDescriptorPool) -> AbiResult<()> {
        let _ = pool;
        Err(host_has_no("VulkanHost::destroy_descriptor_pool", "the pool cannot be destroyed"))
    }

    /// Every `VkDescriptorSet` this host has allocated from `pool` and not yet freed.
    ///
    /// [`VulkanHost::command_buffers_of`]'s argument, one family along: `vkDestroyDescriptorPool`
    /// and `vkResetDescriptorPool` both free every set in the pool without naming one, and the
    /// shim has to drop those guest handles in the same call or the guest holds descriptor-set
    /// handles naming freed driver objects.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn descriptor_sets_of(&self, pool: HostDescriptorPool) -> AbiResult<Vec<HostDescriptorSet>> {
        let _ = pool;
        Err(host_has_no(
            "VulkanHost::descriptor_sets_of",
            "this layer cannot find out which sets are about to be freed, so it cannot take their \
             guest handles back",
        ))
    }

    /// `vkAllocateDescriptorSets`, forwarded. The list is in the guest's layout order, which is
    /// the order the shim writes it into `pDescriptorSets`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when a layout does not
    /// belong to the pool's device.
    fn allocate_descriptor_sets(
        &self,
        pool: HostDescriptorPool,
        layouts: &[HostDescriptorSetLayout],
    ) -> AbiResult<DriverAnswer<Vec<HostDescriptorSet>>> {
        let _ = (pool, layouts);
        Err(host_has_no(
            "VulkanHost::allocate_descriptor_sets",
            "there are no descriptor sets, and a draw that bound one naming nothing would sample \
             whatever descriptor the hardware last had",
        ))
    }

    /// `vkFreeDescriptorSets`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued, or when a set was not
    /// allocated from `pool` — which the specification makes undefined behaviour and which no
    /// validation layer on this machine would report.
    fn free_descriptor_sets(
        &self,
        pool: HostDescriptorPool,
        sets: &[HostDescriptorSet],
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (pool, sets);
        Err(host_has_no("VulkanHost::free_descriptor_sets", "the sets cannot be freed"))
    }

    /// `vkResetDescriptorPool`, forwarded. Frees every set in the pool.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `pool` is not a token this host issued.
    fn reset_descriptor_pool(
        &self,
        pool: HostDescriptorPool,
        flags: u32,
    ) -> AbiResult<DriverAnswer<()>> {
        let _ = (pool, flags);
        Err(host_has_no(
            "VulkanHost::reset_descriptor_pool",
            "the pool cannot be reset, and a guest that believed it had been would allocate from \
             a pool that is still full",
        ))
    }

    /// `vkUpdateDescriptorSets`, forwarded. Returns `void` in Vulkan and has no `VkResult`.
    ///
    /// **The call where a wrong handle is quietest.** Every one of these writes points a
    /// descriptor at an image view, a sampler or a buffer, and a descriptor pointing somewhere
    /// else does not fail — it draws the wrong thing, or reads memory the GPU was not given.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn update_descriptor_sets(
        &self,
        device: HostDevice,
        writes: &[DescriptorWrite],
        copies: &[DescriptorCopy],
    ) -> AbiResult<()> {
        let _ = (device, writes, copies);
        Err(host_has_no(
            "VulkanHost::update_descriptor_sets",
            "no descriptor is updated, and this call has no `VkResult` at all -- so a host that \
             returned quietly would leave every set holding whatever the pool was allocated with, \
             with nothing anywhere to say so",
        ))
    }

    /// `vkCmdBeginRenderPass`, forwarded. `contents` is `VkSubpassContents`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_begin_render_pass(
        &self,
        buffer: HostCommandBuffer,
        begin: &RenderPassBegin,
        contents: u32,
    ) -> AbiResult<()> {
        let _ = (buffer, begin, contents);
        Err(host_has_no(
            "VulkanHost::cmd_begin_render_pass",
            "no render pass is begun, and every draw recorded afterwards is invalid -- which the \
             driver reports at `vkEndCommandBuffer`, a long way from here",
        ))
    }

    /// `vkCmdEndRenderPass`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn cmd_end_render_pass(&self, buffer: HostCommandBuffer) -> AbiResult<()> {
        let _ = buffer;
        Err(host_has_no("VulkanHost::cmd_end_render_pass", "the render pass is not ended"))
    }

    /// `vkCmdBindPipeline`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_bind_pipeline(
        &self,
        buffer: HostCommandBuffer,
        bind_point: u32,
        pipeline: HostPipeline,
    ) -> AbiResult<()> {
        let _ = (buffer, bind_point, pipeline);
        Err(host_has_no("VulkanHost::cmd_bind_pipeline", "no pipeline is bound"))
    }

    /// `vkCmdBindVertexBuffers`, forwarded. `buffers` is `(buffer, offset)` **zipped**, for
    /// [`SubmitRequest::waits`]' reason — the specification requires `pBuffers` and `pOffsets` to
    /// have the same length, and a pair cannot come apart the way two `Vec`s can.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_bind_vertex_buffers(
        &self,
        buffer: HostCommandBuffer,
        first_binding: u32,
        buffers: &[(HostBuffer, u64)],
    ) -> AbiResult<()> {
        let _ = (buffer, first_binding, buffers);
        Err(host_has_no(
            "VulkanHost::cmd_bind_vertex_buffers",
            "no vertex buffer is bound, and the draw would read whatever the hardware last had",
        ))
    }

    /// `vkCmdBindIndexBuffer`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_bind_index_buffer(
        &self,
        buffer: HostCommandBuffer,
        index_buffer: HostBuffer,
        offset: u64,
        index_type: u32,
    ) -> AbiResult<()> {
        let _ = (buffer, index_buffer, offset, index_type);
        Err(host_has_no("VulkanHost::cmd_bind_index_buffer", "no index buffer is bound"))
    }

    /// `vkCmdBindDescriptorSets`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_bind_descriptor_sets(
        &self,
        buffer: HostCommandBuffer,
        bind_point: u32,
        layout: HostPipelineLayout,
        first_set: u32,
        sets: &[HostDescriptorSet],
        dynamic_offsets: &[u32],
    ) -> AbiResult<()> {
        let _ = (buffer, bind_point, layout, first_set, sets, dynamic_offsets);
        Err(host_has_no(
            "VulkanHost::cmd_bind_descriptor_sets",
            "no descriptor set is bound, so the draw samples nothing the guest chose",
        ))
    }

    /// `vkCmdSetViewport`, forwarded. `viewports` is the flat bytes of the guest's array,
    /// [`VIEWPORT_BYTES`](super::VIEWPORT_BYTES) per entry — six floats, no pointer.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn cmd_set_viewport(
        &self,
        buffer: HostCommandBuffer,
        first: u32,
        viewports: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, first, viewports);
        Err(host_has_no(
            "VulkanHost::cmd_set_viewport",
            "the viewport is not set, and a pipeline with `VK_DYNAMIC_STATE_VIEWPORT` has none \
             until it is",
        ))
    }

    /// `vkCmdSetScissor`, forwarded. `scissors` is the flat bytes,
    /// [`RECT_2D_BYTES`](super::RECT_2D_BYTES) per entry.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn cmd_set_scissor(
        &self,
        buffer: HostCommandBuffer,
        first: u32,
        scissors: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, first, scissors);
        Err(host_has_no(
            "VulkanHost::cmd_set_scissor",
            "the scissor is not set, and a zero-sized scissor discards every fragment -- a frame \
             that renders nothing with every `VkResult` zero",
        ))
    }

    /// `vkCmdDraw`, forwarded.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn cmd_draw(
        &self,
        buffer: HostCommandBuffer,
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    ) -> AbiResult<()> {
        let _ = (buffer, vertex_count, instance_count, first_vertex, first_instance);
        Err(host_has_no(
            "VulkanHost::cmd_draw",
            "**nothing is drawn**, and every call around it still answers `VK_SUCCESS`",
        ))
    }

    /// `vkCmdDispatch`, forwarded: the three workgroup counts, in order.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn cmd_dispatch(&self, buffer: HostCommandBuffer, x: u32, y: u32, z: u32) -> AbiResult<()> {
        let _ = (buffer, x, y, z);
        Err(host_has_no(
            "VulkanHost::cmd_dispatch",
            "**nothing is dispatched**, and the compute pass's output is whatever the buffer held",
        ))
    }

    /// `vkCmdDrawIndexed`, forwarded. `vertex_offset` is signed, which is the one argument of the
    /// six that is.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `buffer` is not a token this host issued.
    fn cmd_draw_indexed(
        &self,
        buffer: HostCommandBuffer,
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        vertex_offset: i32,
        first_instance: u32,
    ) -> AbiResult<()> {
        let _ = (buffer, index_count, instance_count, first_index, vertex_offset, first_instance);
        Err(host_has_no("VulkanHost::cmd_draw_indexed", "nothing is drawn"))
    }

    /// `vkCmdCopyBuffer`, forwarded. `regions` is the flat bytes,
    /// [`BUFFER_COPY_BYTES`](super::BUFFER_COPY_BYTES) per entry — three `VkDeviceSize`s.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_copy_buffer(
        &self,
        buffer: HostCommandBuffer,
        source: HostBuffer,
        destination: HostBuffer,
        regions: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, source, destination, regions);
        Err(host_has_no(
            "VulkanHost::cmd_copy_buffer",
            "nothing is copied, so a staging upload never reaches device-local memory",
        ))
    }

    /// `vkCmdCopyBufferToImage`, forwarded. `regions` is the flat bytes,
    /// [`BUFFER_IMAGE_COPY_BYTES`](super::BUFFER_IMAGE_COPY_BYTES) per entry.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_copy_buffer_to_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostBuffer,
        image: HostImageRef,
        layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, source, image, layout, regions);
        Err(host_has_no(
            "VulkanHost::cmd_copy_buffer_to_image",
            "the texture is never uploaded, and the draw that samples it samples whatever the \
             driver left in that image",
        ))
    }

    /// `vkCmdCopyImageToBuffer`, forwarded: `vkCmdCopyBufferToImage` turned round. The image
    /// may be of either family, and `regions` is the flat bytes,
    /// [`BUFFER_IMAGE_COPY_BYTES`](super::BUFFER_IMAGE_COPY_BYTES) per entry.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_copy_image_to_buffer(
        &self,
        buffer: HostCommandBuffer,
        image: HostImageRef,
        layout: u32,
        destination: HostBuffer,
        regions: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, image, layout, destination, regions);
        Err(host_has_no(
            "VulkanHost::cmd_copy_image_to_buffer",
            "the pixels are never read back, and whatever reads the buffer reads what was there \
             before",
        ))
    }

    /// `vkCmdCopyImage`, forwarded. Either image may be of either family, and `regions` is the
    /// guest's [`IMAGE_COPY_BYTES`](super::IMAGE_COPY_BYTES)-byte `VkImageCopy`s, whole.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_copy_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, source, source_layout, destination, destination_layout, regions);
        Err(host_has_no(
            "VulkanHost::cmd_copy_image",
            "the destination image keeps whatever it held, and what samples it samples that",
        ))
    }

    /// `vkCmdResolveImage`, forwarded. As [`VulkanHost::cmd_copy_image`], with `regions` the
    /// guest's [`IMAGE_RESOLVE_BYTES`](super::IMAGE_RESOLVE_BYTES)-byte `VkImageResolve`s: a
    /// multisampled source resolved into a single-sample destination.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_resolve_image(
        &self,
        buffer: HostCommandBuffer,
        source: HostImageRef,
        source_layout: u32,
        destination: HostImageRef,
        destination_layout: u32,
        regions: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, source, source_layout, destination, destination_layout, regions);
        Err(host_has_no(
            "VulkanHost::cmd_resolve_image",
            "the destination keeps whatever it held instead of the multisampled image's resolve",
        ))
    }

    /// `vkCmdBlitImage`, forwarded. As [`VulkanHost::cmd_copy_image`], with `regions` the
    /// guest's [`IMAGE_BLIT_BYTES`](super::IMAGE_BLIT_BYTES)-byte `VkImageBlit`s and `filter` its
    /// `VkFilter`. Source and destination may be the same image.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    #[allow(clippy::too_many_arguments)]
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
        let _ = (buffer, source, source_layout, destination, destination_layout, regions, filter);
        Err(host_has_no(
            "VulkanHost::cmd_blit_image",
            "the destination keeps whatever it held -- for a mip chain, every level below the first",
        ))
    }

    /// `vkCmdPushConstants`, forwarded. `values` is the guest's bytes, copied.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when a token is not one this host issued.
    fn cmd_push_constants(
        &self,
        buffer: HostCommandBuffer,
        layout: HostPipelineLayout,
        stage_flags: u32,
        offset: u32,
        values: &[u8],
    ) -> AbiResult<()> {
        let _ = (buffer, layout, stage_flags, offset, values);
        Err(host_has_no(
            "VulkanHost::cmd_push_constants",
            "the constants are not pushed, and the shader reads whatever was there",
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

        // The destructor too: a host that cannot destroy a surface says so, rather than letting
        // the guest believe the window was let go of.
        let error = host
            .destroy_surface(HostInstance::from_token(0), HostSurface::from_token(0))
            .expect_err("nor a surface destroyed");
        assert_eq!(error.symbol(), Some("VulkanHost::destroy_surface"));

        let error = host
            .pipeline_cache_data(HostDevice::from_token(0), HostPipelineCache::from_token(0))
            .expect_err("nor a cache blob");
        assert_eq!(error.symbol(), Some("VulkanHost::pipeline_cache_data"));

        let error = host.destroy_device(HostDevice::from_token(0)).expect_err("nor a device gone");
        assert_eq!(error.symbol(), Some("VulkanHost::destroy_device"));

        let error =
            host.destroy_instance(HostInstance::from_token(0)).expect_err("nor an instance gone");
        assert_eq!(error.symbol(), Some("VulkanHost::destroy_instance"));
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
