//! **`vkCreateAndroidSurfaceKHR`: the call with no host counterpart, and the second half of the
//! substitution stage 2a began.**
//!
//! # What makes this one different from every other forwarded call
//!
//! Everything else in stages 2a and 3 is *trampolining*: read the AAPCS64 arguments, validate the
//! pointers through `admit`, hand the driver the same call the guest made. This one has no same
//! call to hand over. `vkCreateAndroidSurfaceKHR` does not exist on this host — its driver has
//! never heard of it and `vkGetInstanceProcAddr` answers NULL for it — so the guest's call is
//! satisfied by a **different entry point on a different platform's extension**, and the only
//! thing that connects the two is this file.
//!
//! Stage 2a made the guest *believe* the extension is there:
//! `vkEnumerateInstanceExtensionProperties` advertises `VK_KHR_android_surface` where the driver
//! said the platform one, and `vkCreateInstance` sends the platform name back the other way. Both
//! directions are in [`Vulkan::rewrites`](super::Vulkan::rewrites). This is the third rewrite and
//! it is the one that actually *does* something: the earlier two changed strings, and this one
//! creates an object through a code path the engine did not choose. A surface silently created
//! from another platform's call is exactly the defect Global Constraint 1 names, so it is recorded
//! in the same log, with the host's own spelling of the call it made
//! ([`SurfaceCreated::host_call`](super::SurfaceCreated::host_call)) — because
//! `"vkCreateWin32SurfaceKHR"` is an OS name and this crate names no OS.
//!
//! # Resolving an `ANativeWindow *` to a window the host can make a surface for
//!
//! `VkAndroidSurfaceCreateInfoKHR::window` is a `struct ANativeWindow *`, and in this runtime that
//! is an address in [`ndk`](crate::ndk)'s own arena — five real symbols over a slot registry, with
//! a reference count `ANativeWindow_fromSurface`/`_acquire`/`_release` maintain. So the chain is:
//!
//! 1. the guest's pointer is checked against **that** registry
//!    ([`Ndk::window_references`](crate::ndk::Ndk::window_references) answers `None` for anything
//!    else), because a `ALooper *`, a stale window or a value the engine computed must be a typed
//!    refusal rather than a lookup that happens to land;
//! 2. the instance's [`WindowSource`](crate::ndk::WindowSource) is asked for
//!    [`raw_window`](crate::ndk::WindowSource::raw_window), which is the OS handle of the window
//!    the *same* source reports the geometry of — so `ANativeWindow_getWidth` and this surface
//!    cannot describe two different windows;
//! 3. the host turns that handle into a surface, and names the call it used.
//!
//! **Both of the ways that chain can be incomplete refuse by name, and they are different
//! refusals**, because they have different fixes: no source at all is an embedding that never
//! called [`Ndk::set_window_source`](crate::ndk::Ndk::set_window_source), and a source with no
//! handle is one built by [`HostWindowSource::unpublished`](crate::ndk::HostWindowSource) or fed
//! by hand — a host compositing into something that is not an OS window, which genuinely has no
//! `HWND` to give. Neither is a `VkResult`.
//!
//! # One `ANativeWindow`, and what is *not* checked here
//!
//! This layer does **not** check that a second surface is not created over the same window. Two
//! `VkSurfaceKHR`s over one `HWND` is legal Vulkan and a driver will say so if it is not; a rule
//! invented here would be a second copy of the driver's own validation, in the place least able
//! to be right about it. What is checked is everything this layer is the only holder of: the
//! window handle, the instance handle, the structure's `sType` and `pNext`, and the allocator.

use std::sync::Arc;

use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::DriverAnswer;
use super::instance::guest_pointer;
use super::{Site, Vulkan};

/// `VK_STRUCTURE_TYPE_ANDROID_SURFACE_CREATE_INFO_KHR`.
///
/// The extension's own base, `1000008000` — `VK_KHR_android_surface` is extension number 9, so its
/// structure types start at `1000000000 + (9 - 1) * 1000`. Stated here rather than derived,
/// because a reader checking it against `vulkan_core.h` wants the number and not the arithmetic.
pub const STYPE_ANDROID_SURFACE_CREATE_INFO_KHR: u32 = 1_000_008_000;

/// `sizeof(VkAndroidSurfaceCreateInfoKHR)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType                   sType;    //  0  (u32, then 4 bytes of padding)
/// const void                       *pNext;    //  8
/// VkAndroidSurfaceCreateFlagsKHR    flags;    // 16  (u32, then 4 bytes of padding)
/// struct ANativeWindow             *window;   // 24
/// ```
pub const ANDROID_SURFACE_CREATE_INFO_BYTES: usize = 32;

/// `VkResult vkCreateAndroidSurfaceKHR(VkInstance instance,
/// const VkAndroidSurfaceCreateInfoKHR *pCreateInfo, const VkAllocationCallbacks *pAllocator,
/// VkSurfaceKHR *pSurface)`
///
/// See this module's documentation for the whole of the argument. The order of the checks is the
/// same one [`instance`](super::instance) uses and for the same reason: the allocator is observed
/// first, because it is a measurement a later refusal would lose.
pub(super) fn create_android_surface(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; crate::abi::ARG_REGISTERS as usize],
) -> AbiResult<()> {
    let (instance_handle, create_info_pointer, allocator_pointer, surface_pointer) =
        (args[0], args[1], args[2], args[3]);

    // First, and before anything can return early. `Vulkan::allocator_calls` counts every call
    // that *takes* a `pAllocator`, so this one is in the denominator whatever happens next.
    vulkan.note_allocator("vkCreateAndroidSurfaceKHR", allocator_pointer);
    if allocator_pointer != 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} with \
             `pAllocator = {allocator_pointer:#x}`. See the same refusal on `vkCreateInstance`: a \
             `VkAllocationCallbacks` holds **guest** function pointers, and a host driver cannot \
             branch into translated ARM64 -- nor is there a guest CPU context on the driver's own \
             worker threads, where the specification explicitly permits it to call them. Passing \
             NULL to the driver instead would silently discard an allocator the engine asked to \
             be used",
            caller = at.caller
        )));
    }

    let host = vulkan.require_host(at)?;
    let instance = vulkan.instance_token(at, "vkCreateAndroidSurfaceKHR", instance_handle)?;

    let create_info_at = guest_pointer(at, "pCreateInfo", create_info_pointer)?;
    let surface_at = guest_pointer(at, "pSurface", surface_pointer)?;
    for (name, pointer) in [("pCreateInfo", create_info_at), ("pSurface", surface_at)] {
        if pointer == 0 {
            return Err(at.refuse(format!(
                "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} with \
                 `{name} = NULL`, which the specification requires to be a valid pointer. There is \
                 no window to describe or nowhere to put the surface that was made",
                caller = at.caller
            )));
        }
    }

    // **Decoded, not forwarded.** The structure this layer builds for the host is a different
    // structure for a different extension, so there is nothing that could be passed through even
    // if it were safe to.
    let info = c.mem().read_bytes(create_info_at, ANDROID_SURFACE_CREATE_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four bytes"));
    if stype != STYPE_ANDROID_SURFACE_CREATE_INFO_KHR {
        return Err(at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} with a `pCreateInfo` \
             whose `sType` is {stype}, and \
             `VK_STRUCTURE_TYPE_ANDROID_SURFACE_CREATE_INFO_KHR` is \
             {STYPE_ANDROID_SURFACE_CREATE_INFO_KHR}. The `window` member would be read at an \
             offset that belongs to a different structure, so the `ANativeWindow *` this layer \
             resolved would be whatever happens to sit at byte 24 of something else",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight bytes"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} with \
             `pCreateInfo->pNext = {next:#x}`. See the same refusal on `vkCreateInstance`: a chain \
             is a list of structures this layer would have to know the layout of to validate and \
             to rebuild, and dropping it would create a different surface from the one that was \
             asked for. The address is named so the next run can decode it",
            caller = at.caller
        )));
    }
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four bytes"));
    if flags != 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} with \
             `pCreateInfo->flags = {flags:#x}`. `VkAndroidSurfaceCreateFlagsKHR` is reserved for \
             future use and the specification requires it to be zero, so this layer cannot know \
             what was asked for -- and it cannot be carried across the substitution either, \
             because the structure it becomes has a different flags field for a different \
             extension. Dropping it would be creating a surface the engine did not ask for",
            caller = at.caller
        )));
    }
    let window = u64::from_le_bytes(info[24..32].try_into().expect("eight bytes"));

    let raw = resolve_window(at, window)?;
    match host.create_platform_surface(instance, raw)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result("vkCreateAndroidSurfaceKHR", result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(created) => {
            // **The rewrite, recorded before the guest is told anything.** A handle written into
            // guest memory and a log entry that never happened would leave a surface in existence
            // with nothing saying which call made it.
            vulkan.note_surface_substitution(&created.host_call, raw, at.caller);
            let registered = vulkan.register_surface(at, created.surface)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(surface_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(super::VK_SUCCESS);
            Ok(())
        }
    }
}

/// The OS window behind a guest `ANativeWindow *`, or a refusal naming which link was missing.
///
/// This module's documentation carries the argument for each of the three refusals; what is worth
/// noticing at the call site is that they are **three** and not one, because each has a different
/// fix and a single "could not resolve the window" would leave a reader unable to say which.
fn resolve_window(at: &Site, window: u64) -> AbiResult<omni_platform::window::RawWindow> {
    if window == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} with \
             `pCreateInfo->window = NULL`. The specification requires it to be a valid \
             `ANativeWindow *`, and on this runtime that is a handle
             `ANativeWindow_fromSurface` issued -- jni-surface.md §8 row 17 is where the engine \
             gets one, out of the `Surface` the Java side passes `onSurfaceCreatedNative`",
            caller = at.caller
        )));
    }
    let window_at = guest_pointer(at, "pCreateInfo->window", window)?;

    // The NDK instance published to **this** thread, which is the guest thread the renderer runs
    // on. A refusal here is `AbiError::NdkNotActive` and names the symbol, exactly as it does for
    // `ANativeWindow_getWidth`.
    let ndk = crate::ndk::active(&at.symbol, at.address)?;
    if ndk.window_references(window_at).is_none() {
        return Err(at.refuse(format!(
            "the guest passed {window:#x} as `VkAndroidSurfaceCreateInfoKHR::window`, and that is \
             not a live `ANativeWindow` of this instance. An `ANativeWindow` is opaque and this \
             layer's live ones are slots in its own arena, so a pointer it did not hand out is a \
             handle of another kind, a window from another instance, a window that was already \
             released, or a value the engine computed. Creating a surface over \"the window\" \
             anyway would put the guest's frames into whichever window this host happens to have"
        )));
    }

    let Some(source) = ndk.window_source() else {
        return Err(at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` from {caller:#x} for the \
             `ANativeWindow` at {window:#x}, and this guest instance has no live window source at \
             all -- so there is no OS window to create a surface over. \
             `Ndk::set_window_source` is what attaches one, and \
             `omni_android::ndk::HostWindowSource::watching` is the implementation this workspace \
             ships over `omni_platform::window::Window`. A constant supplied through \
             `Ndk::set_window_geometry` is **not** enough here and that is not an oversight: a \
             width and a height are a size, and a surface needs a window",
            caller = at.caller
        )));
    };
    source.raw_window().ok_or_else(|| {
        at.refuse(format!(
            "the guest called `vkCreateAndroidSurfaceKHR` for the `ANativeWindow` at \
             {window:#x}, and this instance's window source reports no OS handle: {source:?}. \
             `WindowSource::raw_window` answers `None` by default, which is the honest answer for \
             a source fed by hand through `HostWindowSource::publish` or for a host compositing \
             into something that is not an OS window -- neither has an `HWND` to give. \
             `HostWindowSource::watching` publishes one; `HostWindowSource::set_raw_window` is \
             what a host that feeds its own geometry calls. This is a refusal rather than \
             `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR` because that code says another surface already \
             owns the window, which is a statement about a window this layer never found"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure's numbers are the ones the specification fixes**, stated where a reader
    /// can check them against `vulkan_core.h` by eye — [`instance`](super::super::instance)'s own
    /// argument for why there is no `#[repr(C)]` mirror anywhere in this module.
    #[test]
    fn the_android_surface_structure_numbers_are_the_specifications() {
        assert_eq!(ANDROID_SURFACE_CREATE_INFO_BYTES, 32);
        assert_eq!(STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, 1_000_008_000);
        // Extension 9's structure-type base, which is where that number comes from.
        assert_eq!(STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, 1_000_000_000 + (9 - 1) * 1000);
    }
}
