//! **`vkCreateDevice`, `vkGetDeviceQueue`, `vkGetDeviceProcAddr`: the end of stage 3.**
//!
//! # Where this sits
//!
//! [`physical`](super::physical) answers the questions a renderer asks in order to *choose*. This
//! module is what it does with the answer: it creates the logical device, takes the queues out of
//! it, and hands back the thunks for the device-level half of the API. After this the engine has
//! everything it needs to build a swapchain — which is stage 4, and deliberately not here.
//!
//! # `vkGetDeviceProcAddr` is where the no-host-pointers rule is tested twice
//!
//! Stage 2a's argument was about `vkGetInstanceProcAddr`: the guest stores what it is given at
//! `0x6d3ca8` and reaches it with `blr x8`, so a host code address handed over is a jump from
//! translated ARM64 into x86-64 with an AAPCS64 frame. `vkGetDeviceProcAddr` is the same hazard
//! with a larger surface — it is the function that resolves every `vkCmd*`, which is the bulk of
//! what a renderer calls — and it is answered the same way:
//! [`VulkanHost::has_device_proc`](super::VulkanHost::has_device_proc) returns a **`bool`**, the
//! driver's pointer is dropped inside `omni-gfx`, and what the guest receives is a slot from this
//! loader's own thunk pool.
//!
//! **One name, one address, whichever function resolved it.** A name looked up through
//! `vkGetInstanceProcAddr` and then again through `vkGetDeviceProcAddr` gets the *same* thunk
//! back, because the pool is keyed by name. A real loader is entitled to answer differently — its
//! device-level pointer skips the loader trampoline — but the engine stores these and compares
//! them, and two addresses for one function would make `a == b` false for two pointers to the same
//! thing. The census records which call did each lookup ([`ProcVia`](super::ProcVia)), so the
//! sharing is visible rather than implied.
//!
//! # Why `vkGetDeviceProcAddr` is not in `NULL_INSTANCE_COMMANDS`
//!
//! Because the specification's table does not put it there: with a **null instance**
//! `vkGetInstanceProcAddr` answers NULL for it, exactly as it does for `vkCreateDevice`. It
//! becomes resolvable once there is an instance, which is when a caller could use it. That is not
//! this layer declining anything — a real `libvulkan.so` answers the same NULL — and the census
//! records it as [`ProcAnswer::NullPerSpecification`](super::ProcAnswer::NullPerSpecification) so
//! that a reader can tell it from the driver's own NULL.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DeviceRequest, DriverAnswer, QueueRequest};
use super::instance::{decode_names, guest_pointer};
use super::physical::PHYSICAL_DEVICE_FEATURES_BYTES;
use super::{Site, Vulkan, VK_SUCCESS};

/// `VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO`.
const STYPE_DEVICE_QUEUE_CREATE_INFO: u32 = 2;
/// `VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO`.
const STYPE_DEVICE_CREATE_INFO: u32 = 3;

/// `sizeof(VkDeviceCreateInfo)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType                    sType;                      //  0  (then 4 of padding)
/// const void                        *pNext;                      //  8
/// VkDeviceCreateFlags                flags;                      // 16  (then 4 of padding)
/// uint32_t                           queueCreateInfoCount;       // 20
/// const VkDeviceQueueCreateInfo     *pQueueCreateInfos;          // 24
/// uint32_t                           enabledLayerCount;          // 32  (then 4 of padding)
/// const char *const                 *ppEnabledLayerNames;        // 40
/// uint32_t                           enabledExtensionCount;      // 48  (then 4 of padding)
/// const char *const                 *ppEnabledExtensionNames;    // 56
/// const VkPhysicalDeviceFeatures    *pEnabledFeatures;           // 64
/// ```
///
/// `flags` and `queueCreateInfoCount` share the 16..24 pair, which is why the queue array is at 24
/// and not at 32 — the one offset in this structure a reader is likely to get wrong by eye.
pub const DEVICE_CREATE_INFO_BYTES: usize = 72;

/// `sizeof(VkDeviceQueueCreateInfo)`.
///
/// ```text
/// VkStructureType              sType;              //  0  (then 4 of padding)
/// const void                  *pNext;              //  8
/// VkDeviceQueueCreateFlags     flags;              // 16
/// uint32_t                     queueFamilyIndex;   // 20
/// uint32_t                     queueCount;         // 24  (then 4 of padding)
/// const float                 *pQueuePriorities;   // 32
/// ```
pub const DEVICE_QUEUE_CREATE_INFO_BYTES: usize = 40;

/// How many `VkDeviceQueueCreateInfo`s one `vkCreateDevice` may name.
///
/// An allocation bound, for [`MAX_ENABLED_NAMES`](super::MAX_ENABLED_NAMES)' reason:
/// `queueCreateInfoCount` is a guest `uint32_t`, and multiplying it by 40 and reading that many
/// bytes is a guest-controlled host allocation of up to 160 GB. Sixteen is far above what any
/// renderer asks for — one family, or two when graphics and present differ — so reaching it means
/// something nobody has seen, which the refusal names.
pub const MAX_QUEUE_REQUESTS: usize = 16;

/// How many queue priorities one `VkDeviceQueueCreateInfo` may name.
///
/// [`MAX_QUEUE_REQUESTS`]' argument at the next level down, and the number is larger because the
/// thing it bounds is real: a device with 32 compute queues in one family is a device an engine
/// may legitimately ask for all of. 256 is above every family size any current driver reports.
pub const MAX_QUEUE_PRIORITIES: usize = 256;

/// `VkResult vkCreateDevice(VkPhysicalDevice physicalDevice, const VkDeviceCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkDevice *pDevice)`
///
/// The shape [`instance::create_instance`](super::instance) has, one handle family along: observe
/// the allocator first, validate every guest pointer through `admit`, **decode** the structure
/// rather than forwarding it, and hand the guest a registry address rather than the driver's
/// dispatchable handle.
pub(super) fn create_device(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateDevice";
    let (physical_handle, create_info_pointer, allocator_pointer, device_pointer) =
        (args[0], args[1], args[2], args[3]);

    // First, and before anything can return early.
    vulkan.note_allocator(CALL, allocator_pointer);
    if allocator_pointer != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pAllocator = \
             {allocator_pointer:#x}`. See the same refusal on `vkCreateInstance`: those are \
             **guest** function pointers and a host driver cannot branch into translated ARM64, \
             nor is there a guest CPU context on the driver's own worker threads where the \
             specification permits it to call them. `Vulkan::allocator_non_null()` counts this",
            caller = at.caller
        )));
    }

    let host = vulkan.require_host(at)?;
    let physical = vulkan.physical_device_token(at, CALL, physical_handle)?;

    let create_info_at = guest_pointer(at, "pCreateInfo", create_info_pointer)?;
    let device_at = guest_pointer(at, "pDevice", device_pointer)?;
    for (name, pointer) in [("pCreateInfo", create_info_at), ("pDevice", device_at)] {
        if pointer == 0 {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with `{name} = NULL`, which the \
                 specification requires to be a valid pointer. There is no device to describe or \
                 nowhere to put the one that was made, and a `VK_SUCCESS` here would be the \
                 plausible answer Global Constraint 1 forbids",
                caller = at.caller
            )));
        }
    }

    let mut request = decode_device_create_info(c, at, create_info_at)?;

    // **The second rewrite stage 5 needs, and it is an addition rather than a rename.**
    //
    // `vkMapMemory` can only hand the guest an address inside `GuestSpace` by importing those
    // pages with `VK_EXT_external_memory_host` (see `memory`), and an extension can only be used
    // on a device that **enabled** it. The engine asks for `VK_KHR_swapchain` and nothing else, so
    // this layer adds the one it needs — which means the device the guest receives is not the
    // device it described, and that is precisely the silent divergence Global Constraint 1 exists
    // to forbid going unrecorded. `Vulkan::rewrites()` is where it is recorded, as
    // `RewriteSite::DeviceExtensionAdded`, beside the extension renames and the surface call.
    //
    // The host is asked rather than told: a physical device without the extension answers with an
    // empty list, nothing is added, `vkCreateDevice` still succeeds, and the consequence surfaces
    // where it belongs — `vkGetPhysicalDeviceMemoryProperties` masks every host-visible type out
    // of the list the guest chooses from, and `vkAllocateMemory` from one refuses by name.
    for name in host.device_extensions_required_by_host(physical)? {
        if request.extensions.contains(&name) {
            continue;
        }
        vulkan.note_device_extension_added(&name, at.caller);
        request.extensions.push(name);
    }

    match host.create_device(physical, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_device(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(device_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// Decode `VkDeviceCreateInfo` out of guest memory.
fn decode_device_create_info(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    create_info_at: omni_mem::GuestAddr,
) -> AbiResult<DeviceRequest> {
    let info = c.mem().read_bytes(create_info_at, DEVICE_CREATE_INFO_BYTES, c.blame(1))?;
    let u32_at =
        |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four bytes"));
    let u64_at = |offset: usize| {
        u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight bytes"))
    };

    let stype = u32_at(0);
    if stype != STYPE_DEVICE_CREATE_INFO {
        return Err(at.refuse(format!(
            "the guest called `vkCreateDevice` from {caller:#x} with a `pCreateInfo` whose \
             `sType` is {stype}, and `VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO` is \
             {STYPE_DEVICE_CREATE_INFO}. Every field after it would be read at an offset that \
             belongs to a different structure -- `pQueueCreateInfos` would be whatever sits at \
             byte 24 of something else, and this layer would then follow it",
            caller = at.caller
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateDevice` from {caller:#x} with \
             `pCreateInfo->pNext = {next:#x}`. A device `pNext` chain is where Vulkan 1.1 and \
             later put **enabled features** -- `VkPhysicalDeviceFeatures2`, \
             `VkPhysicalDeviceVulkan12Features` and the rest -- so dropping it would create a \
             device that silently lacks features the engine enabled, and every consequence would \
             arrive later as a validation error on a command that used one. This layer does not \
             know those layouts, so it refuses and names the address for the next run to decode. \
             This is the refusal most likely to be the one stage 4 has to answer",
            caller = at.caller
        )));
    }

    let queues = decode_queue_requests(c, at, u32_at(20), u64_at(24))?;
    let layers = decode_names(c, at, "vkCreateDevice", "ppEnabledLayerNames", u32_at(32), u64_at(40))?;
    let extensions =
        decode_names(c, at, "vkCreateDevice", "ppEnabledExtensionNames", u32_at(48), u64_at(56))?;
    let features = decode_features(c, at, u64_at(64))?;

    Ok(DeviceRequest { flags: u32_at(16), queues, layers, extensions, features })
}

/// Decode `pQueueCreateInfos`, which is an array of structures rather than an array of pointers.
fn decode_queue_requests(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    count: u32,
    array: u64,
) -> AbiResult<Vec<QueueRequest>> {
    if count == 0 {
        // A device with no queues is legal to *ask* for and useless to have; the driver is the one
        // that says so, and a refusal here would be this layer having an opinion the
        // specification does not.
        return Ok(Vec::new());
    }
    let count = count as usize;
    if count > MAX_QUEUE_REQUESTS {
        return Err(at.refuse(format!(
            "the guest called `vkCreateDevice` from {caller:#x} with \
             `queueCreateInfoCount = {count}`, and this layer reads at most \
             {MAX_QUEUE_REQUESTS}. The count is a guest `uint32_t` and the array it indexes is \
             {bytes} bytes long, so honouring it unbounded would be a guest-controlled host \
             allocation (Global Constraint 11). This is a refusal rather than a truncation \
             because a device created with fewer queue families than the engine asked for is a \
             device whose `vkGetDeviceQueue` fails later, somewhere else",
            caller = at.caller,
            bytes = count * DEVICE_QUEUE_CREATE_INFO_BYTES
        )));
    }
    let array_at = guest_pointer(at, "pQueueCreateInfos", array)?;
    if array_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateDevice` from {caller:#x} with \
             `pQueueCreateInfos = NULL` and `queueCreateInfoCount = {count}`. There are no queue \
             families to create, and creating a device with none while the engine believes it \
             asked for {count} is the silent divergence this layer exists to refuse",
            caller = at.caller
        )));
    }

    let bytes =
        c.mem().read_bytes(array_at, count * DEVICE_QUEUE_CREATE_INFO_BYTES, c.blame(1))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * DEVICE_QUEUE_CREATE_INFO_BYTES..][..DEVICE_QUEUE_CREATE_INFO_BYTES];
        let u32_at = |offset: usize| {
            u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four bytes"))
        };
        let u64_at = |offset: usize| {
            u64::from_le_bytes(entry[offset..offset + 8].try_into().expect("eight bytes"))
        };
        let stype = u32_at(0);
        if stype != STYPE_DEVICE_QUEUE_CREATE_INFO {
            return Err(at.refuse(format!(
                "the guest's `pQueueCreateInfos[{index}]` has `sType` {stype}, and \
                 `VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO` is \
                 {STYPE_DEVICE_QUEUE_CREATE_INFO}. `queueFamilyIndex` and `pQueuePriorities` \
                 would be read at offsets belonging to a different structure"
            )));
        }
        let next = u64_at(8);
        if next != 0 {
            return Err(at.refuse(format!(
                "the guest's `pQueueCreateInfos[{index}]->pNext` is {next:#x}. See the refusal \
                 for `pCreateInfo->pNext`: `VkDeviceQueueGlobalPriorityCreateInfoKHR` is what \
                 usually goes there, and a device whose queue was created at a different \
                 priority from the one asked for is a scheduling difference nothing would \
                 record"
            )));
        }
        let queue_count = u32_at(24) as usize;
        if queue_count > MAX_QUEUE_PRIORITIES {
            return Err(at.refuse(format!(
                "the guest's `pQueueCreateInfos[{index}]->queueCount` is {queue_count}, and this \
                 layer reads at most {MAX_QUEUE_PRIORITIES} priorities. The count is a guest \
                 `uint32_t` indexing an array of `float`, so honouring it unbounded would be a \
                 guest-controlled host allocation (Global Constraint 11)"
            )));
        }
        let priorities_at = guest_pointer(at, "pQueuePriorities", u64_at(32))?;
        let priorities = if queue_count == 0 {
            Vec::new()
        } else {
            if priorities_at == 0 {
                return Err(at.refuse(format!(
                    "the guest's `pQueueCreateInfos[{index}]` names {queue_count} queue(s) and \
                     `pQueuePriorities = NULL`. The specification requires an array of \
                     {queue_count} floats, and substituting a priority this layer chose would be \
                     creating queues the engine did not ask for"
                )));
            }
            let raw = c.mem().read_bytes(priorities_at, queue_count * 4, c.blame(1))?;
            (0..queue_count)
                .map(|slot| {
                    f32::from_le_bytes(raw[slot * 4..slot * 4 + 4].try_into().expect("four bytes"))
                })
                .collect()
        };
        out.push(QueueRequest {
            flags: u32_at(16),
            family_index: u32_at(20),
            priorities,
        });
    }
    Ok(out)
}

/// Decode `pEnabledFeatures`, which is allowed to be NULL.
///
/// The bytes travel as bytes; [`DeviceRequest::features`] carries the argument for why 55 booleans
/// are not decoded into 55 fields and built back up again.
fn decode_features(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    pointer: u64,
) -> AbiResult<Option<Vec<u8>>> {
    let features_at = guest_pointer(at, "pEnabledFeatures", pointer)?;
    if features_at == 0 {
        // Legal, and it means "no optional feature" -- which is **not** the same as a zeroed
        // structure to a driver that distinguishes the two, so `Option` is what carries it.
        return Ok(None);
    }
    let bytes = c.mem().read_bytes(features_at, PHYSICAL_DEVICE_FEATURES_BYTES, c.blame(1))?;
    Ok(Some(bytes))
}

/// `void vkGetDeviceQueue(VkDevice device, uint32_t queueFamilyIndex, uint32_t queueIndex,
/// VkQueue *pQueue)`
///
/// # `void`, which is what makes the handle registry matter more here than anywhere else
///
/// There is no `VkResult`. If this layer wrote a handle naming nothing, there would be no status
/// code beside it for the engine to disbelieve, and the first sign would be a `vkQueueSubmit`
/// against a queue that does not exist. So a family or index the device does not have has to
/// become a **refusal** — the driver's own validation is not reachable, because `vkGetDeviceQueue`
/// has no way to report one.
///
/// **The same family and index answer with the same `VkQueue` every time**, because
/// [`Handles::insert_or_get`](super::handles::Handles::insert_or_get) deduplicates on the host's
/// token. A renderer decides whether its graphics and present queues are one queue by comparing
/// the two handles, and that decision is what makes its swapchain `EXCLUSIVE` rather than
/// `CONCURRENT`.
pub(super) fn get_device_queue(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetDeviceQueue";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let (family, index) = (args[1] as u32, args[2] as u32);

    let queue_at = guest_pointer(at, "pQueue", args[3])?;
    if queue_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pQueue = NULL`. It is this call's \
             only output and the call returns `void`, so there is nothing to write and nothing to \
             report -- returning quietly would leave the guest reading whatever was already in \
             its own variable as a `VkQueue`",
            caller = at.caller
        )));
    }

    let token = host.device_queue(device, family, index)?;
    let registered = vulkan.register_queue(at, token)?;
    if registered.fresh {
        c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
    }
    c.mem().write_u64(queue_at, registered.at as u64, c.blame(3))?;
    c.ret().void();
    Ok(())
}

/// `PFN_vkVoidFunction vkGetDeviceProcAddr(VkDevice device, const char *pName)`
///
/// This module's documentation carries the argument. The short form: a **`bool`** from the host
/// decides between a guest thunk and a NULL, so a driver's function pointer physically cannot
/// reach translated ARM64 through this call.
pub(super) fn get_device_proc_addr(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    let (device, name_pointer) = (args[0], args[1]);
    let name_at = guest_pointer(at, "pName", name_pointer)?;
    if name_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkGetDeviceProcAddr(device = {device:#x}, pName = NULL)` from \
             {caller:#x}. The specification requires `pName` to be a null-terminated UTF-8 \
             string, so there is no name to look up -- and NULL is not an answer here, because \
             NULL is how a caller detects an absent function and this call never named one",
            caller = at.caller
        )));
    }
    let name = {
        let bytes = c.mem().cstr(name_at, c.blame(1))?;
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let answer = vulkan.resolve_on_device(at, device, &name)?;
    c.ret().u64(answer.address());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure numbers are the ones the specification fixes**, with the one offset a
    /// reader is likely to get wrong stated as arithmetic rather than as a memory.
    #[test]
    fn the_device_structure_numbers_are_the_specifications() {
        assert_eq!(DEVICE_CREATE_INFO_BYTES, 72);
        assert_eq!(DEVICE_QUEUE_CREATE_INFO_BYTES, 40);
        assert_eq!(STYPE_DEVICE_QUEUE_CREATE_INFO, 2);
        assert_eq!(STYPE_DEVICE_CREATE_INFO, 3);
        // `flags` (16) and `queueCreateInfoCount` (20) share one eight-byte slot, which is why
        // `pQueueCreateInfos` is at 24. A layout that padded after `flags` would put it at 32 and
        // this layer would follow whatever `enabledLayerCount` happened to be.
        assert_eq!(16 + 4 + 4, 24);
    }

    /// The two allocation bounds are far above anything a renderer asks for, which is what makes
    /// reaching one a finding rather than a limit.
    ///
    /// Written as equalities rather than as `assert!(X >= 2)`, because a comparison between two
    /// constants is a comparison the compiler folds away — clippy's `assertions_on_constants`
    /// names it, and it is right: such a line asserts nothing at run time and reads as though it
    /// did. What these do instead is **state the numbers**, so that changing one is a visible
    /// change to a test rather than a silent change to a bound.
    #[test]
    fn the_queue_bounds_are_above_what_any_renderer_asks_for() {
        // Two, because graphics and present may be different families; this is eight times that.
        assert_eq!(MAX_QUEUE_REQUESTS, 16);
        // A compute family with 32 queues is a real device; this is eight times that.
        assert_eq!(MAX_QUEUE_PRIORITIES, 256);
        // And the largest read either bound permits stays well under a page.
        assert_eq!(MAX_QUEUE_REQUESTS * DEVICE_QUEUE_CREATE_INFO_BYTES, 640);
        assert_eq!(MAX_QUEUE_PRIORITIES * 4, 1024);
    }
}
