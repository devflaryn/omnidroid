//! **The ten queries a renderer makes between an instance and a device, forwarded.**
//!
//! # What this module is for
//!
//! By the time a renderer has an instance and a surface, everything it does next is a *question*:
//! which devices are there, what are they called, which of their queue families can render, which
//! can present to this surface, what formats does the surface offer, what does the device let a
//! swapchain do. Ten calls, no objects created, and the answer to every one of them belongs to a
//! driver. This module asks, and writes the answer into the guest's buffer.
//!
//! # Structures travel as bytes, and here is why that is the careful choice rather than the lazy
//! one
//!
//! `VkPhysicalDeviceProperties` is [`PHYSICAL_DEVICE_PROPERTIES_BYTES`] bytes of which 504 are
//! `VkPhysicalDeviceLimits` — around 110 members, most of them limits a renderer divides by. Every
//! member of it, and of the other five structures here, is a fixed-width integer, an enum, a
//! `VkDeviceSize`, or a fixed array of those. **There is no pointer and no `size_t` anywhere in
//! them**, so the guest's aarch64 LP64 layout and this host's x86-64 LLP64 layout are the same
//! layout, byte for byte, and the bytes the driver wrote are precisely the bytes the guest is
//! owed.
//!
//! A `#[repr(C)]` mirror in this crate would be 110 field declarations laid out by *this host's*
//! compiler for *this host's* target while making a claim about the guest's, and one transposed
//! pair inside `VkPhysicalDeviceLimits` is a renderer that silently believes it may allocate a
//! larger image than the device supports — a defect whose first symptom is a driver error three
//! hundred milliseconds into the first frame. [`instance`](super::instance) makes the same
//! argument for `VkInstanceCreateInfo` and answers it with named offsets, because that structure
//! has to be *decoded* — its members are guest pointers this layer must follow. None of the
//! structures here has a member to follow, so there is nothing to decode and the bytes are the
//! whole content.
//!
//! What keeps that honest is that the size is stated **twice, in two crates, from two sources**:
//! here, as a constant a reader can check against `vulkan_core.h`, and in `omni-gfx`, as an
//! assertion against `core::mem::size_of` of `ash`'s generated structure. A host that answers a
//! blob of any other length is refused by name rather than having its bytes written into a guest
//! buffer that is a different size.
//!
//! # The two-call protocol
//!
//! Five of the ten use it, and every one of them goes through [`counted::enumerate`], which is
//! where the argument about `VK_INCOMPLETE` and about writing past the end of a guest buffer
//! lives. The one that is easy to get wrong here is
//! `vkGetPhysicalDeviceQueueFamilyProperties`, which returns **`void`**: there is no
//! `VK_INCOMPLETE` to answer with, so the count written back is the only thing that tells the
//! caller it was truncated, and this module deliberately does not invent a result for it.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::counted;
use super::host::DriverAnswer;
use super::instance::{extension_properties, guest_pointer, guest_string, EXTENSION_PROPERTIES_BYTES};
use super::{Site, Vulkan, VK_SUCCESS};

// ------------------------------------------------------------------ the specification's sizes

/// `sizeof(VkPhysicalDeviceProperties)`.
///
/// ```text
/// uint32_t                          apiVersion;             //   0
/// uint32_t                          driverVersion;          //   4
/// uint32_t                          vendorID;               //   8
/// uint32_t                          deviceID;               //  12
/// VkPhysicalDeviceType              deviceType;             //  16
/// char                              deviceName[256];        //  20
/// uint8_t                           pipelineCacheUUID[16];  // 276
///                                                           // 292: four bytes of padding,
///                                                           //      because limits is 8-aligned
/// VkPhysicalDeviceLimits            limits;                 // 296 (504 bytes)
/// VkPhysicalDeviceSparseProperties  sparseProperties;       // 800 (five VkBool32 = 20 bytes)
/// ```
///
/// 820 rounded up to the structure's own alignment of 8. `omni-gfx` asserts the same number
/// against `ash`'s `size_of`, which is generated from `vk.xml`.
pub const PHYSICAL_DEVICE_PROPERTIES_BYTES: usize = 824;

/// `sizeof(VkPhysicalDeviceFeatures)`: 55 `VkBool32`s, no padding, alignment 4.
pub const PHYSICAL_DEVICE_FEATURES_BYTES: usize = 220;

/// `sizeof(VkQueueFamilyProperties)`.
///
/// ```text
/// VkQueueFlags  queueFlags;                    //  0
/// uint32_t      queueCount;                    //  4
/// uint32_t      timestampValidBits;            //  8
/// VkExtent3D    minImageTransferGranularity;   // 12 (three uint32_t)
/// ```
pub const QUEUE_FAMILY_PROPERTIES_BYTES: usize = 24;

/// `sizeof(VkPhysicalDeviceMemoryProperties)`.
///
/// ```text
/// uint32_t       memoryTypeCount;     //   0
/// VkMemoryType   memoryTypes[32];     //   4  (32 * 8)
/// uint32_t       memoryHeapCount;     // 260
/// VkMemoryHeap   memoryHeaps[16];     // 264  (16 * 16; VkDeviceSize then flags, 8-aligned)
/// ```
///
/// **The table the `vkMapMemory` question is really about.** A renderer picks the memory type it
/// uploads textures through out of exactly this array, by matching
/// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT`.
pub const PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES: usize = 520;

/// `sizeof(VkSurfaceCapabilitiesKHR)`: thirteen `uint32_t`-sized members, no padding.
pub const SURFACE_CAPABILITIES_BYTES: usize = 52;

/// `sizeof(VkSurfaceFormatKHR)`: a `VkFormat` and a `VkColorSpaceKHR`.
pub const SURFACE_FORMAT_BYTES: usize = 8;

/// `sizeof(VkPresentModeKHR)`: it is an enum, which is a `uint32_t`.
pub const PRESENT_MODE_BYTES: usize = 4;

/// `sizeof(VkPhysicalDevice)`: a dispatchable handle, which is pointer-sized on every LP64 and
/// LLP64 target — and what the guest receives is an arena address of that width, never the
/// driver's pointer.
pub const HANDLE_BYTES: usize = 8;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkEnumeratePhysicalDevices(VkInstance instance, uint32_t *pPhysicalDeviceCount,
/// VkPhysicalDevice *pPhysicalDevices)`
///
/// # Every device is registered on the first call, including the count-only one
///
/// The specification lets a caller ask for the count, allocate, and ask again, and it requires the
/// second call to produce **the same handles**. So the registry is filled the first time the
/// driver is asked — even when `pPhysicalDevices` is NULL and nothing will be written — and
/// [`Handles::insert_or_get`](super::handles::Handles::insert_or_get) is what makes the second
/// call recover the same addresses rather than consume four more slots.
///
/// A host with more devices than [`MAX_PHYSICAL_DEVICES`](super::MAX_PHYSICAL_DEVICES) is a
/// refusal naming the constant, not a truncated list: a truncated list of GPUs is a plausible list
/// of GPUs, and the one that was dropped might be the discrete one.
pub(super) fn enumerate_physical_devices(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkEnumeratePhysicalDevices";
    let host = vulkan.require_host(at)?;
    let instance = vulkan.instance_token(at, CALL, args[0])?;

    let tokens = match host.physical_devices(instance)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            return Ok(());
        }
        DriverAnswer::Ok(tokens) => tokens,
    };

    let mut handles = Vec::with_capacity(tokens.len() * HANDLE_BYTES);
    for token in &tokens {
        let registered = vulkan.register_physical_device(at, *token)?;
        handles.extend_from_slice(&(registered.at as u64).to_le_bytes());
        if registered.fresh {
            c.mem().write_bytes(registered.at, &registered.image, c.blame(2))?;
        }
    }

    let filled = counted::enumerate(
        c,
        at,
        &counted::Array {
            call: CALL,
            element: "VkPhysicalDevice",
            element_bytes: HANDLE_BYTES,
            count_pointer: args[1],
            array_pointer: args[2],
            count_argument: 1,
            array_argument: 2,
        },
        &handles,
    )?;
    c.ret().i32(filled.result());
    Ok(())
}

/// `void vkGetPhysicalDeviceProperties(VkPhysicalDevice physicalDevice,
/// VkPhysicalDeviceProperties *pProperties)`
///
/// **The call that produces the evidence this whole stage rests on.** `deviceName` is a driver
/// string; a stub cannot produce `NVIDIA GeForce RTX 4060` and the live test prints what came out
/// of the guest's own buffer.
pub(super) fn physical_device_properties(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceProperties";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let bytes = host.physical_device_properties(device)?;
    write_structure(
        c,
        at,
        &Structure {
            call: CALL,
            field: "pProperties",
            name: "VkPhysicalDeviceProperties",
            expected: PHYSICAL_DEVICE_PROPERTIES_BYTES,
            pointer: args[1],
            argument: 1,
        },
        &bytes,
    )?;
    c.ret().void();
    Ok(())
}

/// `void vkGetPhysicalDeviceFeatures(VkPhysicalDevice physicalDevice,
/// VkPhysicalDeviceFeatures *pFeatures)`
pub(super) fn physical_device_features(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceFeatures";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let bytes = host.physical_device_features(device)?;
    write_structure(
        c,
        at,
        &Structure {
            call: CALL,
            field: "pFeatures",
            name: "VkPhysicalDeviceFeatures",
            expected: PHYSICAL_DEVICE_FEATURES_BYTES,
            pointer: args[1],
            argument: 1,
        },
        &bytes,
    )?;
    c.ret().void();
    Ok(())
}

/// `void vkGetPhysicalDeviceMemoryProperties(VkPhysicalDevice physicalDevice,
/// VkPhysicalDeviceMemoryProperties *pMemoryProperties)`
pub(super) fn physical_device_memory_properties(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceMemoryProperties";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let driver = host.physical_device_memory_properties(device)?;
    if driver.len() != PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES {
        // Checked here rather than left to `write_structure` because the masking below indexes
        // into these bytes, and indexing a short blob would be a host read out of bounds long
        // before the guest's buffer was reached.
        return Err(at.refuse(format!(
            "the host answered `{CALL}` with {} byte(s) and a `VkPhysicalDeviceMemoryProperties` \
             is {PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES}. This layer has to *read* that table in \
             order to mask the memory types it cannot back, so a blob of another length is \
             refused before it is indexed",
            driver.len()
        )));
    }

    // **The Global Constraint 1 rewrite.** See `mask_memory_types` for what is edited and
    // `memory` for why it has to be.
    let importable = host.importable_memory_types(device)?;
    let (shown, masked) = mask_memory_types(&driver, importable);
    if !masked.is_empty() {
        vulkan.note_memory_type_rewrites(device, &masked, at.caller);
    }

    write_structure(
        c,
        at,
        &Structure {
            call: CALL,
            field: "pMemoryProperties",
            name: "VkPhysicalDeviceMemoryProperties",
            expected: PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES,
            pointer: args[1],
            argument: 1,
        },
        &shown,
    )?;
    c.ret().void();
    Ok(())
}

/// Clear the host-visible property bits of every memory type this layer cannot back.
///
/// # The edit, exactly
///
/// A `VkPhysicalDeviceMemoryProperties` is a count and a fixed array of 32 `VkMemoryType`s, each
/// an eight-byte `{ VkMemoryPropertyFlags propertyFlags; uint32_t heapIndex; }`. For every type
/// that has `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT` and is **not** in `importable`, the
/// host-visible, host-coherent and host-cached bits are cleared. Nothing else changes: not the
/// count, not the order, not `heapIndex`, not the heaps.
///
/// # Why bits and not entries
///
/// Because `memoryTypeIndex` is how the guest names a type at `vkAllocateMemory`, and it is an
/// index into *this* array. Removing an entry would renumber every type after it, so a guest that
/// read the list, chose index 3 and allocated would be allocating from the driver's type 4. Every
/// index the guest can produce therefore still means to the driver exactly what it meant, and what
/// this layer changed is a *claim* rather than a name — the claim being "you may map this", which
/// is the one thing that would not have been true.
///
/// # Why the whole trio and not `HOST_VISIBLE` alone
///
/// `VK_MEMORY_PROPERTY_HOST_COHERENT_BIT` without `HOST_VISIBLE` is a combination the
/// specification does not produce, and an engine that saw one would be entitled to find it
/// surprising. `VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT` is deliberately **left alone**: it is a fact
/// about where the memory is, it stays true, and clearing it would be this layer lying in the
/// other direction — a ReBAR type is still device-local and an engine that wants device-local
/// memory should still find it.
///
/// Answers the edited bytes and one `(index, before, after)` per type that changed.
#[must_use]
pub fn mask_memory_types(driver: &[u8], importable: u32) -> (Vec<u8>, Vec<(u32, u32, u32)>) {
    /// Where `memoryTypes[0]` starts: immediately after `memoryTypeCount`.
    const TYPES_AT: usize = 4;
    /// `sizeof(VkMemoryType)`: `propertyFlags` and `heapIndex`.
    const TYPE_BYTES: usize = 8;
    /// `VK_MAX_MEMORY_TYPES`.
    const MAX_TYPES: usize = 32;

    let mut shown = driver.to_vec();
    let mut masked = Vec::new();
    if shown.len() < TYPES_AT + MAX_TYPES * TYPE_BYTES {
        // A blob the caller has already refused; answering it unchanged keeps this function total
        // rather than panicking inside an import (`VERIFICATION.md` entry 13).
        return (shown, masked);
    }
    let count = u32::from_le_bytes(shown[0..4].try_into().expect("four")).min(MAX_TYPES as u32);
    for index in 0..count {
        let at = TYPES_AT + index as usize * TYPE_BYTES;
        let flags = u32::from_le_bytes(shown[at..at + 4].try_into().expect("four"));
        let backable = importable & (1u32 << index) != 0;
        if flags & super::memory::HOST_PROPERTY_BITS == 0 || backable {
            continue;
        }
        let edited = flags & !super::memory::HOST_PROPERTY_BITS;
        shown[at..at + 4].copy_from_slice(&edited.to_le_bytes());
        masked.push((index, flags, edited));
    }
    (shown, masked)
}

/// `void vkGetPhysicalDeviceQueueFamilyProperties(VkPhysicalDevice physicalDevice,
/// uint32_t *pQueueFamilyPropertyCount, VkQueueFamilyProperties *pQueueFamilyProperties)`
///
/// # The two-call protocol **without** a result, which is the trap
///
/// This one returns `void`. There is no `VK_INCOMPLETE` to answer with, so the count written back
/// is the only thing that tells a caller its array was too small — and inventing a `VkResult` to
/// carry the fact would be writing a value into a register the guest is not going to read as one.
/// [`Filled::result`](super::counted::Filled::result) exists and is deliberately **not** called
/// here.
///
/// The index into this list *is* the queue family index every later call uses, so the driver's
/// order is kept exactly.
pub(super) fn queue_family_properties(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceQueueFamilyProperties";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let families = host.queue_family_properties(device)?;
    let bytes = flatten(at, CALL, "VkQueueFamilyProperties", QUEUE_FAMILY_PROPERTIES_BYTES, &families)?;
    counted::enumerate(
        c,
        at,
        &counted::Array {
            call: CALL,
            element: "VkQueueFamilyProperties",
            element_bytes: QUEUE_FAMILY_PROPERTIES_BYTES,
            count_pointer: args[1],
            array_pointer: args[2],
            count_argument: 1,
            array_argument: 2,
        },
        &bytes,
    )?;
    // **No result.** See this function's documentation.
    c.ret().void();
    Ok(())
}

/// `VkResult vkGetPhysicalDeviceSurfaceSupportKHR(VkPhysicalDevice physicalDevice,
/// uint32_t queueFamilyIndex, VkSurfaceKHR surface, VkBool32 *pSupported)`
///
/// **Two handles of two different families in one call**, which is exactly the case a registry per
/// family catches and a single opaque-pointer check would not: a guest that passed its
/// `VkPhysicalDevice` where the `VkSurfaceKHR` belongs gets a typed refusal naming which argument
/// was of the wrong kind.
pub(super) fn surface_support(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceSurfaceSupportKHR";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let queue_family = args[1] as u32;
    let surface = vulkan.surface_token(at, CALL, args[2])?;

    let supported_at = guest_pointer(at, "pSupported", args[3])?;
    if supported_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pSupported = NULL`, which the \
             specification requires to be a valid pointer to a `VkBool32`. It is the only output \
             this call has, so `VK_SUCCESS` would say an answer had been written into memory \
             nothing wrote to -- and the guest would then read whatever was already there as the \
             answer to \"can this queue family present?\"",
            caller = at.caller
        )));
    }

    match host.surface_support(device, queue_family, surface)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            // **Nothing is written on a failure.** The specification leaves `pSupported`
            // untouched when the call fails, and writing `VK_FALSE` would turn a lost surface into
            // "this queue family cannot present", which is a different thing the engine acts on
            // differently.
            c.ret().i32(result);
        }
        DriverAnswer::Ok(supported) => {
            c.mem().write_u32(supported_at, u32::from(supported), c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
        }
    }
    Ok(())
}

/// `VkResult vkGetPhysicalDeviceSurfaceCapabilitiesKHR(VkPhysicalDevice physicalDevice,
/// VkSurfaceKHR surface, VkSurfaceCapabilitiesKHR *pSurfaceCapabilities)`
pub(super) fn surface_capabilities(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceSurfaceCapabilitiesKHR";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let surface = vulkan.surface_token(at, CALL, args[1])?;
    match host.surface_capabilities(device, surface)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(bytes) => {
            write_structure(
                c,
                at,
                &Structure {
                    call: CALL,
                    field: "pSurfaceCapabilities",
                    name: "VkSurfaceCapabilitiesKHR",
                    expected: SURFACE_CAPABILITIES_BYTES,
                    pointer: args[2],
                    argument: 2,
                },
                &bytes,
            )?;
            c.ret().i32(VK_SUCCESS);
        }
    }
    Ok(())
}

/// `VkResult vkGetPhysicalDeviceSurfaceFormatsKHR(VkPhysicalDevice physicalDevice,
/// VkSurfaceKHR surface, uint32_t *pSurfaceFormatCount, VkSurfaceFormatKHR *pSurfaceFormats)`
pub(super) fn surface_formats(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceSurfaceFormatsKHR";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let surface = vulkan.surface_token(at, CALL, args[1])?;
    let formats = match host.surface_formats(device, surface)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            return Ok(());
        }
        DriverAnswer::Ok(formats) => formats,
    };
    let bytes = flatten(at, CALL, "VkSurfaceFormatKHR", SURFACE_FORMAT_BYTES, &formats)?;
    let filled = counted::enumerate(
        c,
        at,
        &counted::Array {
            call: CALL,
            element: "VkSurfaceFormatKHR",
            element_bytes: SURFACE_FORMAT_BYTES,
            count_pointer: args[2],
            array_pointer: args[3],
            count_argument: 2,
            array_argument: 3,
        },
        &bytes,
    )?;
    c.ret().i32(filled.result());
    Ok(())
}

/// `VkResult vkGetPhysicalDeviceSurfacePresentModesKHR(VkPhysicalDevice physicalDevice,
/// VkSurfaceKHR surface, uint32_t *pPresentModeCount, VkPresentModeKHR *pPresentModes)`
pub(super) fn surface_present_modes(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetPhysicalDeviceSurfacePresentModesKHR";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;
    let surface = vulkan.surface_token(at, CALL, args[1])?;
    let modes = match host.surface_present_modes(device, surface)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            return Ok(());
        }
        DriverAnswer::Ok(modes) => modes,
    };
    let bytes: Vec<u8> = modes.iter().flat_map(|mode| mode.to_le_bytes()).collect();
    let filled = counted::enumerate(
        c,
        at,
        &counted::Array {
            call: CALL,
            element: "VkPresentModeKHR",
            element_bytes: PRESENT_MODE_BYTES,
            count_pointer: args[2],
            array_pointer: args[3],
            count_argument: 2,
            array_argument: 3,
        },
        &bytes,
    )?;
    c.ret().i32(filled.result());
    Ok(())
}

/// `VkResult vkEnumerateDeviceExtensionProperties(VkPhysicalDevice physicalDevice,
/// const char *pLayerName, uint32_t *pPropertyCount, VkExtensionProperties *pProperties)`
///
/// **Not rewritten.** [`DeviceRequest`](super::DeviceRequest) carries the argument: the one
/// substitution this layer makes is an *instance* extension, and there is no device-level name
/// whose host spelling differs. The rewrite log staying empty across this call is therefore a
/// claim in its own right, and the report prints the count whether it is zero or not.
pub(super) fn device_extension_properties(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkEnumerateDeviceExtensionProperties";
    let host = vulkan.require_host(at)?;
    let device = vulkan.physical_device_token(at, CALL, args[0])?;

    let layer = if args[1] == 0 {
        None
    } else {
        let layer_at = guest_pointer(at, "pLayerName", args[1])?;
        Some(guest_string(c.mem(), at, "pLayerName", layer_at, c.blame(1))?)
    };

    let driver = match host.device_extensions(device, layer.as_deref())? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            return Ok(());
        }
        DriverAnswer::Ok(list) => list,
    };
    let mut bytes = Vec::with_capacity(driver.len() * EXTENSION_PROPERTIES_BYTES);
    for extension in &driver {
        bytes.extend_from_slice(&extension_properties(at, extension)?);
    }
    let filled = counted::enumerate(
        c,
        at,
        &counted::Array {
            call: CALL,
            element: "VkExtensionProperties",
            element_bytes: EXTENSION_PROPERTIES_BYTES,
            count_pointer: args[2],
            array_pointer: args[3],
            count_argument: 2,
            array_argument: 3,
        },
        &bytes,
    )?;
    c.ret().i32(filled.result());
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// One fixed-layout structure a query writes, as the facts it takes to write it.
///
/// A struct rather than six parameters, for [`counted::Array`]'s reason: `expected` and
/// `argument` are both `usize` and a positional call that transposed them would compile.
#[derive(Clone, Copy)]
pub(super) struct Structure<'a> {
    /// The Vulkan function, for every refusal this produces.
    pub(super) call: &'a str,
    /// The parameter the guest passed the buffer in, as the specification names it.
    pub(super) field: &'a str,
    /// The structure's own name, for the refusal that says the host's blob is the wrong size.
    pub(super) name: &'a str,
    /// `sizeof` that structure, from this module's constants.
    pub(super) expected: usize,
    /// The guest's buffer, as it arrived.
    pub(super) pointer: u64,
    /// Which AAPCS64 argument it was, so a refusal from `admit` names the register.
    pub(super) argument: usize,
}

/// Write one fixed-layout structure into a guest buffer, or refuse naming what was wrong.
///
/// Two checks, and they catch different mistakes. A NULL pointer is the **guest's** and refuses
/// because there is nowhere to put the answer; a blob of the wrong length is the **host's** and
/// refuses because writing it would either leave the tail of the guest's structure as whatever was
/// there before or run past the end of it — and Global Constraint 11 calls the second one
/// Critical.
pub(super) fn write_structure(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    structure: &Structure<'_>,
    bytes: &[u8],
) -> AbiResult<()> {
    let Structure { call, field, name, expected, .. } = *structure;
    let out = guest_pointer(at, field, structure.pointer)?;
    if out == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `{field} = NULL`. The specification              requires it to be a valid pointer to a `{name}`, and it is this call's only              output -- so there is nothing to answer into and returning at all would leave the              guest reading whatever was already in its own buffer as the driver's answer",
            caller = at.caller
        )));
    }
    if bytes.len() != expected {
        return Err(at.refuse(format!(
            "the host answered `{call}` with {got} bytes and `sizeof({name})` is {expected}.              Writing the shorter of the two would either leave the tail of the guest's structure              as whatever was there before, or write past the end of it -- and this layer cannot              tell which, because the length it was handed is the only thing that described the              blob. `omni_android::vulkan` and the `VulkanHost` implementation disagree about a              structure the Vulkan specification fixes; `omni-gfx` asserts this number against              `core::mem::size_of` and that assertion is where the disagreement is visible",
            got = bytes.len()
        )));
    }
    c.mem().write_bytes(out, bytes, c.blame(structure.argument))
}

/// Flatten a host's list of fixed-size structures, refusing an entry of the wrong length.
///
/// The per-entry check is here rather than in [`counted::enumerate`] because it can name **which**
/// entry was wrong, and a host that produced one short structure in a list of five is a different
/// bug from one that produced five short ones.
fn flatten(
    at: &Site,
    call: &str,
    structure: &str,
    expected: usize,
    entries: &[Vec<u8>],
) -> AbiResult<Vec<u8>> {
    let mut out = Vec::with_capacity(entries.len() * expected);
    for (index, entry) in entries.iter().enumerate() {
        if entry.len() != expected {
            return Err(at.refuse(format!(
                "the host answered `{call}` with a `{structure}` at index {index} that is {got} \
                 bytes, and `sizeof({structure})` is {expected}. The list is refused whole rather \
                 than written up to the bad entry, because a guest that received the entries \
                 before it would have no way to know the array stopped early",
                got = entry.len()
            )));
        }
        out.extend_from_slice(entry);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `VkPhysicalDeviceMemoryProperties` blob out of `(propertyFlags, heapIndex)` pairs.
    fn memory_properties(types: &[(u32, u32)]) -> Vec<u8> {
        let mut bytes = vec![0u8; PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES];
        bytes[0..4].copy_from_slice(&(types.len() as u32).to_le_bytes());
        for (index, (flags, heap)) in types.iter().enumerate() {
            let at = 4 + index * 8;
            bytes[at..at + 4].copy_from_slice(&flags.to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&heap.to_le_bytes());
        }
        bytes[260..264].copy_from_slice(&1u32.to_le_bytes());
        bytes[264..272].copy_from_slice(&(8u64 << 30).to_le_bytes());
        bytes
    }

    /// **The rewrite, against the table this machine actually reports.**
    ///
    /// The five types and the importable mask are the measured ones from `docs/HANDOFF.md`:
    ///
    /// ```text
    /// [0] (none)                                        heap 1
    /// [1] DEVICE_LOCAL                                  heap 0
    /// [2] HOST_VISIBLE | HOST_COHERENT                  heap 1
    /// [3] HOST_VISIBLE | HOST_COHERENT | HOST_CACHED    heap 1
    /// [4] DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT   heap 2
    /// memoryTypeBits from vkGetMemoryHostPointerPropertiesEXT = 0xc
    /// ```
    ///
    /// So exactly one type is masked — the ReBAR one — and what it loses is the promise that the
    /// guest may map it, not the fact that it is device-local.
    #[test]
    fn the_rebar_type_loses_its_host_visible_bits_and_keeps_being_device_local() {
        let driver = memory_properties(&[(0x0, 1), (0x1, 0), (0x6, 1), (0xe, 1), (0x7, 2)]);
        let (shown, masked) = mask_memory_types(&driver, 0xc);

        assert_eq!(masked.len(), 1, "one type changed: {masked:?}");
        assert_eq!(masked[0], (4, 0x7, 0x1), "type 4, DEVICE_LOCAL|HOST_VISIBLE|HOST_COHERENT -> DEVICE_LOCAL");

        let flags = |bytes: &[u8], index: usize| {
            u32::from_le_bytes(bytes[4 + index * 8..8 + index * 8].try_into().expect("four"))
        };
        assert_eq!(flags(&shown, 0), 0x0, "a type with no properties is untouched");
        assert_eq!(flags(&shown, 1), 0x1, "device-local and not host-visible: untouched");
        assert_eq!(flags(&shown, 2), 0x6, "importable, so its promise is kept");
        assert_eq!(flags(&shown, 3), 0xe, "importable, so its promise is kept");
        assert_eq!(flags(&shown, 4), 0x1, "the ReBAR type is still device-local");

        // **Nothing else moved.** The count, the heap indices and the heaps are the driver's, and
        // that is what keeps `memoryTypeIndex` meaning the same thing on both sides of the edit.
        assert_eq!(shown[0..4], driver[0..4], "memoryTypeCount");
        assert_eq!(shown[260..272], driver[260..272], "the heap count and the first heap");
        for index in 0..5 {
            let at = 8 + index * 8;
            assert_eq!(shown[at..at + 4], driver[at..at + 4], "heapIndex of type {index}");
        }
        assert_eq!(shown.len(), driver.len());
    }

    /// **A host that can import nothing masks every host-visible type**, which is the honest
    /// answer for a device with no `VK_EXT_external_memory_host` — and the one that keeps a
    /// conforming engine away from a `vkMapMemory` this layer would have to refuse.
    #[test]
    fn an_importable_set_of_zero_masks_every_host_visible_type() {
        let driver = memory_properties(&[(0x1, 0), (0x6, 1), (0xe, 1)]);
        let (shown, masked) = mask_memory_types(&driver, 0);
        assert_eq!(masked.len(), 2, "the two host-visible types: {masked:?}");
        let flags = |index: usize| {
            u32::from_le_bytes(shown[4 + index * 8..8 + index * 8].try_into().expect("four"))
        };
        assert_eq!(flags(0), 0x1, "device-local only: nothing to mask");
        assert_eq!(flags(1), 0x0);
        assert_eq!(flags(2), 0x0);
    }

    /// **Nothing is masked when everything is importable**, and the log records nothing — because
    /// a rewrite that did not happen must not appear in a log Global Constraint 1 relies on.
    #[test]
    fn a_fully_importable_device_is_not_rewritten_at_all() {
        let driver = memory_properties(&[(0x1, 0), (0x6, 1), (0xe, 1)]);
        let (shown, masked) = mask_memory_types(&driver, 0b110);
        assert!(masked.is_empty(), "{masked:?}");
        assert_eq!(shown, driver, "byte for byte");
    }

    /// **`memoryTypeCount` bounds the walk, not the array's 32 slots.**
    ///
    /// The tail of `memoryTypes` is whatever the driver left there, and editing it would be this
    /// layer writing into bytes the specification says are undefined — which a guest that
    /// (wrongly) read them would then see this layer's edit in.
    #[test]
    fn the_unused_tail_of_the_memory_type_array_is_not_touched() {
        let mut driver = memory_properties(&[(0x6, 1)]);
        // Leftovers in slot 1, past `memoryTypeCount`, that look host-visible.
        driver[12..16].copy_from_slice(&0xeu32.to_le_bytes());
        let (shown, masked) = mask_memory_types(&driver, 0);
        assert_eq!(masked.len(), 1, "only the one live type: {masked:?}");
        assert_eq!(
            u32::from_le_bytes(shown[12..16].try_into().expect("four")),
            0xe,
            "the slot past the count is the driver's, untouched"
        );
    }

    /// **The structure sizes are the ones the specification fixes**, stated where a reader can
    /// check them against `vulkan_core.h` by eye.
    ///
    /// These describe the **guest's** aarch64 LP64 layout. `omni-gfx` asserts the same numbers
    /// against `ash`'s `size_of` on the host, and the two agreeing is the claim; a host compiler
    /// reproducing them on its own would be evidence about the host alone.
    #[test]
    fn the_structure_sizes_are_the_ones_the_specification_fixes() {
        assert_eq!(PHYSICAL_DEVICE_PROPERTIES_BYTES, 824);
        assert_eq!(PHYSICAL_DEVICE_FEATURES_BYTES, 220, "55 VkBool32");
        assert_eq!(PHYSICAL_DEVICE_FEATURES_BYTES % 4, 0);
        assert_eq!(PHYSICAL_DEVICE_FEATURES_BYTES / 4, 55);
        assert_eq!(QUEUE_FAMILY_PROPERTIES_BYTES, 24);
        assert_eq!(PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES, 520);
        assert_eq!(SURFACE_CAPABILITIES_BYTES, 52);
        assert_eq!(SURFACE_FORMAT_BYTES, 8);
        assert_eq!(PRESENT_MODE_BYTES, 4);
        assert_eq!(HANDLE_BYTES, 8);
        // The memory-properties arithmetic, so the 520 is checkable rather than remembered.
        assert_eq!(PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES, 4 + 32 * 8 + 4 + 16 * 16);
    }

    /// **A host list with one wrong-sized entry is refused whole**, naming the index.
    #[test]
    fn a_short_entry_refuses_the_whole_list_and_names_its_index() {
        let at = Site { symbol: "test".to_string(), address: 0, caller: 0 };
        let entries = vec![vec![0u8; 24], vec![0u8; 20], vec![0u8; 24]];
        let error = flatten(&at, "vkGetPhysicalDeviceQueueFamilyProperties", "VkQueueFamilyProperties", 24, &entries)
            .expect_err("a 20-byte queue family is not one");
        let text = error.to_string();
        assert!(text.contains("index 1"), "{text}");
        assert!(text.contains("20"), "{text}");
        assert!(text.contains("refused whole"), "{text}");

        let good = vec![vec![7u8; 24], vec![9u8; 24]];
        let flat = flatten(&at, "call", "VkQueueFamilyProperties", 24, &good).expect("two entries");
        assert_eq!(flat.len(), 48);
        assert_eq!(flat[0], 7);
        assert_eq!(flat[24], 9, "and they are in the driver's order");
    }
}
