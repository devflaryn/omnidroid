//! **Device memory, and the one call in Vulkan that hands the guest an address it will store
//! through.**
//!
//! # The decision this module implements, which was measured before it was written
//!
//! `vkMapMemory` returns a pointer. The guest does not check it, does not validate it and does not
//! pass it back — it *writes vertices and texels through it* for the rest of the frame. Under D4
//! amendment 1 [`admit`](omni_mem::admit) governs this layer's own shims and **not** the guest's
//! own loads and stores, so a driver pointer handed over here would work, silently, for as long as
//! nothing re-validated it — and then fail somewhere with no relationship to the cause. That is
//! the worst shape a defect can have in this project and it is why the answer was measured rather
//! than argued about.
//!
//! The answer, recorded in `docs/HANDOFF.md` under "the `vkMapMemory` answer", is
//! `VK_EXT_external_memory_host`: **make the memory not foreign.** This layer asks
//! [`GuestSpace`](omni_mem::GuestSpace) for an anonymous mapping, hands its address to the driver
//! in a `VkImportMemoryHostPointerInfoEXT`, and `vkMapMemory` then gives back the address that was
//! imported. Measured on this machine:
//!
//! ```text
//! VK_EXT_external_memory_host is present (of 263 device extensions)
//! minImportedHostPointerAlignment = 4096            (= GuestSpace::page_size())
//! vkGetMemoryHostPointerPropertiesEXT(ordinary committed host memory) -> SUCCESS
//!   memoryTypeBits = 0xc  -> types 2 and 3, both HOST_VISIBLE | HOST_COHERENT
//! vkAllocateMemory(VkImportMemoryHostPointerInfoEXT) -> OK
//! vkMapMemory -> 0x1cdb4f89000, and the pointer imported was 0x1cdb4f89000 -- SAME
//! ```
//!
//! So the guest receives an address **inside its own address space**, `admit` admits it with zero
//! changes to `omni-mem`, and `HOST_COHERENT` is genuinely coherent because there is exactly one
//! copy of the bytes.
//!
//! **Bouncing was not merely worse, it was impossible.** Every `HOST_VISIBLE` memory type on this
//! GPU is also `HOST_COHERENT`, so a conforming engine is never *required* to call
//! `vkFlushMappedMemoryRanges` — and a bounce buffer with no flush point has nowhere to copy at.
//!
//! # The split, and where it is decided
//!
//! At [`allocate_memory`], from the `memoryTypeIndex` the guest already supplies, through
//! [`VulkanHost::memory_plan`]:
//!
//! | the type is | what happens | what `vkMapMemory` answers |
//! |---|---|---|
//! | not `HOST_VISIBLE` | an ordinary forward; no guest mapping exists | **refused** — and the specification refuses it too |
//! | `HOST_VISIBLE` and importable | a `GuestSpace` mapping, imported | that mapping's address plus `offset` |
//! | `HOST_VISIBLE` and **not** importable | **refused**, naming the type | — |
//!
//! The third row is what [`physical`](super::physical)'s rewrite exists to make unreachable from a
//! conforming guest: the importable set is a *subset* of the host-visible set on this driver, so
//! the types this layer cannot back have their host-visible bits masked out of the list the guest
//! chooses from. A guest that ignores the list and names one anyway still gets a refusal, because
//! the rewrite is a courtesy to a conforming engine and not a security boundary.
//!
//! # What this costs, stated because D15 asks for it
//!
//! Every host-visible allocation is now **guest commit charge**, eagerly committed at
//! `vkAllocateMemory` rather than on first touch. That makes
//! [`GuestSpaceConfig::max_committed`](omni_mem::GuestSpaceConfig::max_committed) a **streaming
//! ceiling**: it is no longer only the guest's heap, it is the guest's heap plus every staging
//! buffer and texture upload in flight. Reaching it is
//! [`MemError::CommitCeiling`](omni_mem::MemError::CommitCeiling), and it arrives here as a
//! refusal that names the ceiling and the allocation that hit it rather than as
//! `VK_ERROR_OUT_OF_DEVICE_MEMORY` — because the device is not out of memory, this runtime's
//! ceiling is, and an engine told the former would go looking in the wrong place.
//!
//! # `pNext` is refused, here and everywhere else in this stage
//!
//! Deliberately, and this module is the one that shows why it is not laziness: the **one**
//! `pNext` structure stage 5 needs is `VkImportMemoryHostPointerInfoEXT`, and this layer
//! *constructs* it rather than forwarding one. A guest chain arriving at `vkAllocateMemory` would
//! be `VkMemoryDedicatedAllocateInfo` (which names a buffer or an image and changes what the
//! allocation is), `VkMemoryAllocateFlagsInfo` (device address), or an import of the guest's own —
//! and the last of those would be a host pointer the guest chose. Walking a chain means knowing
//! every structure in it; dropping the ones this layer does not know produces an allocation that
//! is not the one that was asked for. So the chain is refused and the refusal names the address,
//! which turns "what does this engine actually send?" into a measurement rather than a guess.

use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestAddr, MemError, Placement, Protection};

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, MemoryAllocation, MemoryPlan};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO`.
const STYPE_MEMORY_ALLOCATE_INFO: u32 = 5;
/// `VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE`.
const STYPE_MAPPED_MEMORY_RANGE: u32 = 6;

/// `sizeof(VkMemoryAllocateInfo)`.
///
/// ```text
/// VkStructureType   sType;             //  0  (then 4 of padding)
/// const void       *pNext;             //  8
/// VkDeviceSize      allocationSize;    // 16
/// uint32_t          memoryTypeIndex;   // 24  (then 4 of padding)
/// ```
pub const MEMORY_ALLOCATE_INFO_BYTES: usize = 32;

/// `sizeof(VkMemoryRequirements)`.
///
/// ```text
/// VkDeviceSize  size;             //  0
/// VkDeviceSize  alignment;        //  8
/// uint32_t      memoryTypeBits;   // 16  (then 4 of padding, alignment 8)
/// ```
pub const MEMORY_REQUIREMENTS_BYTES: usize = 24;

/// `sizeof(VkMappedMemoryRange)`.
///
/// ```text
/// VkStructureType   sType;    //  0  (then 4 of padding)
/// const void       *pNext;    //  8
/// VkDeviceMemory    memory;   // 16  (a uint64_t)
/// VkDeviceSize      offset;   // 24
/// VkDeviceSize      size;     // 32
/// ```
pub const MAPPED_MEMORY_RANGE_BYTES: usize = 40;

/// `VK_WHOLE_SIZE`.
pub const VK_WHOLE_SIZE: u64 = u64::MAX;

/// How many `VkMappedMemoryRange`s one flush or invalidate will read.
///
/// [`MAX_BARRIERS`](super::MAX_BARRIERS)' argument: the count is a guest `uint32_t` indexing an
/// array of 40-byte structures, so honouring it unbounded would be a guest-controlled host
/// allocation. Thirty-two is far above what a frame's uploads need on a coherent host, where the
/// specified number is zero.
pub const MAX_MAPPED_RANGES: usize = 32;

/// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`, `_HOST_COHERENT_BIT` and `_HOST_CACHED_BIT` together.
///
/// **The bits the memory-type rewrite clears**, named here rather than inside the rewrite because
/// this module is where the reason lives: they are exactly the properties that promise the guest
/// it may call `vkMapMemory`, and a type this layer cannot import is a type for which that promise
/// cannot be kept.
pub const HOST_PROPERTY_BITS: u32 =
    MemoryPlan::HOST_VISIBLE | MemoryPlan::HOST_COHERENT | MemoryPlan::HOST_CACHED;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkAllocateMemory(VkDevice device, const VkMemoryAllocateInfo *pAllocateInfo,
/// const VkAllocationCallbacks *pAllocator, VkDeviceMemory *pMemory)`
pub(super) fn allocate_memory(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkAllocateMemory";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pAllocateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pMemory", args[3])?;

    let info = c.mem().read_bytes(info_at, MEMORY_ALLOCATE_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != STYPE_MEMORY_ALLOCATE_INFO {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with a `pAllocateInfo` whose `sType` is \
             {stype}, and `VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO` is \
             {STYPE_MEMORY_ALLOCATE_INFO}. `allocationSize` and `memoryTypeIndex` would be read \
             at offsets belonging to a different structure, and this layer would then map that \
             many bytes of guest address space",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pAllocateInfo->pNext = {next:#x}`. \
             This layer **constructs** the one `pNext` an allocation here carries -- a \
             `VkImportMemoryHostPointerInfoEXT` naming a `GuestSpace` mapping -- and it does not \
             walk the guest's. A chain here is `VkMemoryDedicatedAllocateInfo`, which makes the \
             allocation belong to one buffer or image rather than being general; \
             `VkMemoryAllocateFlagsInfo`, which asks for a device address; or an import of the \
             guest's own, which would be a host pointer the guest chose. Dropping any of them \
             would produce an allocation that is not the one that was asked for, and chaining \
             this layer's import behind one it does not understand would be two imports of one \
             allocation. The address is named so that a run says which structure the engine \
             actually sends",
            caller = at.caller
        )));
    }
    let size = u64::from_le_bytes(info[16..24].try_into().expect("eight"));
    let type_index = u32::from_le_bytes(info[24..28].try_into().expect("four"));

    if size == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `allocationSize = 0`, which the \
             specification forbids. A zero-byte allocation has no address to hand back, and \
             `vkMapMemory` on one would return a pointer to a mapping that covers nothing",
            caller = at.caller
        )));
    }

    // **The split.** Only the driver knows what the index means, so it is asked -- once, here,
    // before anything is mapped or allocated.
    let plan = host.memory_plan(device, type_index)?;
    let allocation = if plan.host_visible() {
        if !plan.importable {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} asking for {size} byte(s) of memory \
                 type {type_index}, whose driver property flags are {flags:#x} -- host-visible, \
                 and **not importable** on this device. This layer can only make host-visible \
                 memory the guest may map by allocating the pages out of `GuestSpace` and \
                 importing them with `VK_EXT_external_memory_host`, so for this type there is no \
                 address it could return from `vkMapMemory` that is inside the guest's own \
                 address space. Handing back the driver's pointer instead is the one thing that \
                 must not happen: under D4 amendment 1 the guest would store through it \
                 successfully, and the failure would surface far from here. \
                 `vkGetPhysicalDeviceMemoryProperties` already masks this type's host-visible \
                 bits out of the list the guest is shown -- see `Vulkan::rewrites()` -- so a \
                 conforming engine cannot reach this refusal by choosing from that list",
                caller = at.caller,
                flags = plan.property_flags
            )));
        }
        let (address, length) = map_guest_pages(c, at, CALL, size, plan.import_alignment)?;
        MemoryAllocation {
            size,
            memory_type_index: type_index,
            host_pointer: Some(address as u64),
            import_length: length as u64,
        }
    } else {
        MemoryAllocation { size, memory_type_index: type_index, host_pointer: None, import_length: 0 }
    };

    let answered = host.allocate_memory(device, &allocation);
    // **The mapping is released on every path that does not produce a handle.** An allocation the
    // driver declined, or one the registry cannot hold, would otherwise leave guest commit charge
    // behind with nothing referring to it -- a leak that is invisible until `max_committed` binds.
    let answered = match answered {
        Ok(answer) => answer,
        Err(error) => {
            release_guest_pages(c, vulkan, &allocation);
            return Err(error);
        }
    };
    match answered {
        DriverAnswer::Failed(result) => {
            release_guest_pages(c, vulkan, &allocation);
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = match vulkan.register_device_memory(at, token) {
                Ok(registered) => registered,
                Err(error) => {
                    release_guest_pages(c, vulkan, &allocation);
                    return Err(error);
                }
            };
            if let Some(address) = allocation.host_pointer {
                vulkan.note_import(
                    token,
                    address as GuestAddr,
                    allocation.import_length as usize,
                    allocation.size,
                );
            }
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkFreeMemory(VkDevice device, VkDeviceMemory memory,
/// const VkAllocationCallbacks *pAllocator)`
///
/// `VK_NULL_HANDLE` is the specified no-op, and it is the common case: a renderer that frees its
/// resources in a loop hits it for every slot it never filled.
pub(super) fn free_memory(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkFreeMemory";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "memory", args[1])?;
    let token = vulkan.device_memory_token(at, CALL, args[1])?;

    // **The driver first, the guest's pages second, and the order is the whole of the argument.**
    // The driver has imported those pages; unmapping them while it still holds the allocation
    // would decommit memory a device allocation refers to, and on this host nothing would report
    // it -- the next GPU access would fault inside the driver.
    host.free_memory(token)?;
    vulkan.forget_device_memory(handle);
    if let Some(import) = vulkan.forget_import(token) {
        if let Err(error) = c.mem().space().unmap(import.at, import.len) {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x}, the driver freed the allocation, and \
                 this layer could not release the {len} byte(s) of guest address space at \
                 {address:#x} that had been imported into it: {error}. The commit charge is still \
                 held, so this is a leak that `GuestSpaceConfig::max_committed` will eventually \
                 refuse an allocation over, and it is named here rather than swallowed",
                caller = at.caller,
                len = import.len,
                address = import.at
            )));
        }
    }
    c.ret().void();
    Ok(())
}

/// `VkResult vkMapMemory(VkDevice device, VkDeviceMemory memory, VkDeviceSize offset,
/// VkDeviceSize size, VkMemoryMapFlags flags, void **ppData)`
///
/// **The call this whole module exists for.** What it writes into `ppData` is an address inside
/// [`GuestSpace`](omni_mem::GuestSpace), and the check that keeps it one is not a comment: the
/// driver's own answer is compared against the pointer that was imported, and a driver that
/// answered anything else is a refusal naming both rather than a host pointer handed to translated
/// ARM64.
pub(super) fn map_memory(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkMapMemory";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let token = vulkan.device_memory_token(at, CALL, args[1])?;
    let offset = args[2];
    let size = args[3];
    let flags = args[4] as u32;
    let out_at = require_pointer(at, CALL, "ppData", args[5])?;

    if flags != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `flags = {flags:#x}`. \
             `VkMemoryMapFlags` is reserved by the core specification and every bit in it belongs \
             to an extension this layer has not enabled, so forwarding it would ask the driver \
             for behaviour nothing here can describe and dropping it would map with behaviour the \
             guest did not ask for",
            caller = at.caller
        )));
    }

    let Some(import) = vulkan.import_of(token) else {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} on a `VkDeviceMemory` this layer \
             **forwarded** rather than imported, which means it was allocated from a memory type \
             with no `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`. The specification forbids mapping such \
             an allocation, and this layer could not satisfy it in any case: the only addresses it \
             can hand the guest are ones inside `GuestSpace`, and a device-local allocation has \
             none. `vkGetPhysicalDeviceMemoryProperties` is where a renderer finds out which types \
             it may map",
            caller = at.caller
        )));
    };

    // Bounds first, against the **guest's** allocationSize rather than against the rounded-up
    // import length: the pages past the end are this layer's rounding and are not memory the guest
    // was given.
    if offset > import.size {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `offset = {offset}` into an \
             allocation of {size} byte(s). The pointer this call answers with is one the guest \
             stores through without checking it, so an offset past the end is refused here rather \
             than producing an address a page beyond what was allocated",
            caller = at.caller,
            size = import.size
        )));
    }
    let mapped = if size == VK_WHOLE_SIZE {
        import.size - offset
    } else {
        if size > import.size - offset {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} for {size} byte(s) at offset \
                 {offset} of an allocation of {total} byte(s), which runs {over} byte(s) past its \
                 end",
                caller = at.caller,
                total = import.size,
                over = size - (import.size - offset)
            )));
        }
        size
    };

    let expected = import.at as u64 + offset;
    match host.map_memory(token, offset, size, flags)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(answered) => {
            if answered != expected {
                return Err(at.refuse(format!(
                    "the guest called `{CALL}` from {caller:#x} on imported memory whose pages \
                     this layer mapped at {expected:#x}, and the driver answered {answered:#x}. \
                     The whole of `VK_EXT_external_memory_host`'s usefulness here is that a \
                     mapping of imported host memory **is** the imported pointer -- measured on \
                     this machine, and recorded in `docs/HANDOFF.md` -- so a different address is \
                     a driver that copied rather than imported. Writing {answered:#x} into \
                     `ppData` would hand translated ARM64 a host pointer outside `GuestSpace`, \
                     which it would then store through successfully (D4 amendment 1) until \
                     something far from here re-validated it. Refusing is the only answer that \
                     names the cause",
                    caller = at.caller
                )));
            }
            // `expected` rather than `answered`, although they are now provably the same number:
            // what the guest is owed is a guest address, and writing the variable that *means*
            // that is what keeps the two from drifting if this code is ever changed.
            c.mem().write_u64(out_at, expected, c.blame(5))?;
            vulkan.note_mapped(mapped);
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkUnmapMemory(VkDevice device, VkDeviceMemory memory)`
///
/// The guest's pages stay mapped in `GuestSpace`: they are the allocation, not the mapping of it,
/// and they go away at `vkFreeMemory`. What this releases is the driver's own record that the
/// allocation is mapped, which is what `vkMapMemory` may not be called twice without.
pub(super) fn unmap_memory(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkUnmapMemory";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let token = vulkan.device_memory_token(at, CALL, args[1])?;
    host.unmap_memory(token)?;
    c.ret().void();
    Ok(())
}

/// `void vkGetBufferMemoryRequirements(VkDevice device, VkBuffer buffer,
/// VkMemoryRequirements *pMemoryRequirements)`
pub(super) fn buffer_memory_requirements(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetBufferMemoryRequirements";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let buffer = vulkan.buffer_token(at, CALL, args[1])?;
    let out_at = require_pointer(at, CALL, "pMemoryRequirements", args[2])?;
    let bytes = host.buffer_memory_requirements(buffer)?;
    write_requirements(c, at, CALL, &bytes, out_at, 2)?;
    c.ret().void();
    Ok(())
}

/// `void vkGetImageMemoryRequirements(VkDevice device, VkImage image,
/// VkMemoryRequirements *pMemoryRequirements)`
pub(super) fn image_memory_requirements(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetImageMemoryRequirements";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let image = vulkan.created_image_token(at, CALL, args[1])?;
    let out_at = require_pointer(at, CALL, "pMemoryRequirements", args[2])?;
    let bytes = host.image_memory_requirements(image)?;
    write_requirements(c, at, CALL, &bytes, out_at, 2)?;
    c.ret().void();
    Ok(())
}

/// `VkResult vkBindBufferMemory(VkDevice device, VkBuffer buffer, VkDeviceMemory memory,
/// VkDeviceSize memoryOffset)`
pub(super) fn bind_buffer_memory(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkBindBufferMemory";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let buffer = vulkan.buffer_token(at, CALL, args[1])?;
    let memory = vulkan.device_memory_token(at, CALL, args[2])?;
    let result = host.bind_buffer_memory(buffer, memory, args[3])?;
    answer(c, vulkan, CALL, result);
    Ok(())
}

/// `VkResult vkBindImageMemory(VkDevice device, VkImage image, VkDeviceMemory memory,
/// VkDeviceSize memoryOffset)`
pub(super) fn bind_image_memory(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkBindImageMemory";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let image = vulkan.created_image_token(at, CALL, args[1])?;
    let memory = vulkan.device_memory_token(at, CALL, args[2])?;
    let result = host.bind_image_memory(image, memory, args[3])?;
    answer(c, vulkan, CALL, result);
    Ok(())
}

/// `VkResult vkFlushMappedMemoryRanges(VkDevice device, uint32_t memoryRangeCount,
/// const VkMappedMemoryRange *pMemoryRanges)`
pub(super) fn flush_mapped_memory_ranges(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkFlushMappedMemoryRanges";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let ranges = decode_ranges(c, at, vulkan, CALL, args[1] as u32, args[2])?;
    let result = host.flush_mapped_memory_ranges(device, &ranges)?;
    answer(c, vulkan, CALL, result);
    Ok(())
}

/// `VkResult vkInvalidateMappedMemoryRanges(VkDevice device, uint32_t memoryRangeCount,
/// const VkMappedMemoryRange *pMemoryRanges)`
pub(super) fn invalidate_mapped_memory_ranges(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkInvalidateMappedMemoryRanges";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let ranges = decode_ranges(c, at, vulkan, CALL, args[1] as u32, args[2])?;
    let result = host.invalidate_mapped_memory_ranges(device, &ranges)?;
    answer(c, vulkan, CALL, result);
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// Ask [`GuestSpace`](omni_mem::GuestSpace) for the pages an imported allocation is made of.
///
/// Eagerly committed, and that is not a performance choice: the driver is about to be handed this
/// address and will read and write it from the GPU without ever touching it through a page fault
/// this runtime can see. A lazily-committed range would be a placeholder the driver imports and
/// then faults on, in a thread with no guest context.
fn map_guest_pages(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    size: u64,
    import_alignment: u64,
) -> AbiResult<(GuestAddr, usize)> {
    let space = c.mem().space();
    let page = space.page_size();
    let align = usize::try_from(import_alignment).unwrap_or(page).max(page);
    if !align.is_power_of_two() {
        return Err(at.refuse(format!(
            "the guest called `{call}` and this host reports \
             `minImportedHostPointerAlignment = {import_alignment}`, which is not a power of two. \
             Every alignment in `GuestSpace` is, so there is no mapping this layer could make that \
             satisfies it"
        )));
    }
    let length = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_next_multiple_of(align))
        .ok_or_else(|| {
            at.refuse(format!(
                "the guest called `{call}` from {caller:#x} asking for {size} byte(s), which does \
                 not fit this host's address space once rounded up to the {align}-byte alignment \
                 an imported host pointer needs",
                caller = at.caller
            ))
        })?;

    space.map_anonymous(Placement::Anywhere { align }, length, Protection::ReadWrite, CommitPolicy::Eager)
        .map(|address| (address, length))
        .map_err(|error| {
            let ceiling = matches!(
                error,
                MemError::CommitCeiling { .. } | MemError::CommitRequestTooLarge { .. }
            );
            at.refuse(format!(
                "the guest called `{call}` from {caller:#x} asking for {size} byte(s) of \
                 host-visible memory, and this layer could not map the {length} byte(s) of guest \
                 address space it has to import in order to satisfy it: {error}.{note} This is a \
                 refusal rather than `VK_ERROR_OUT_OF_DEVICE_MEMORY` because the device is not \
                 out of memory -- an engine told that code would free textures and try again, \
                 which changes nothing here",
                caller = at.caller,
                note = if ceiling {
                    " **This is D15's ceiling binding.** Every host-visible Vulkan allocation is \
                     now guest commit charge, so `GuestSpaceConfig::max_committed` and \
                     `max_commit_request` bound the engine's streaming as well as its heap, and \
                     this run has reached one of them."
                } else {
                    ""
                }
            ))
        })
}

/// Give back the pages [`map_guest_pages`] took, on a path that produced no handle.
///
/// # Why a failure here is counted rather than returned
///
/// This runs on paths that are **already** returning something the guest needs — a driver's
/// `VkResult`, or a refusal naming a full registry. Replacing that answer with "and also the
/// unmap failed" would hide the thing the guest is actually being told, and the guest can do
/// nothing about either. So the bytes are charged to
/// [`Vulkan::leaked_import_bytes`](super::Vulkan::leaked_import_bytes), which is a counter a gate
/// prints and which `Vulkan::report` states whether it is zero or not — because a leak nobody
/// counted is the shape `VERIFICATION.md` entry 15 is about.
fn release_guest_pages(
    c: &mut ImportCall<'_, '_>,
    vulkan: &Arc<Vulkan>,
    allocation: &MemoryAllocation,
) {
    let Some(address) = allocation.host_pointer else { return };
    let Ok(address) = GuestAddr::try_from(address) else { return };
    let Ok(length) = usize::try_from(allocation.import_length) else { return };
    if c.mem().space().unmap(address, length).is_err() {
        vulkan.note_leaked_import(length);
    }
}

/// Write a `VkMemoryRequirements` the host answered, refusing a blob of the wrong length.
fn write_requirements(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    bytes: &[u8],
    out_at: GuestAddr,
    argument: usize,
) -> AbiResult<()> {
    if bytes.len() != MEMORY_REQUIREMENTS_BYTES {
        return Err(at.refuse(format!(
            "the host answered `{call}` with {} byte(s) and a `VkMemoryRequirements` is \
             {MEMORY_REQUIREMENTS_BYTES}. Writing them into the guest's buffer would either \
             overrun it or leave `memoryTypeBits` holding whatever was there, and a guest that \
             allocated from a fabricated `memoryTypeBits` would bind memory the resource cannot \
             use",
            bytes.len()
        )));
    }
    c.mem().write_bytes(out_at, bytes, c.blame(argument))
}

/// Decode `pMemoryRanges` for a flush or an invalidate.
fn decode_ranges(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    count: u32,
    array: u64,
) -> AbiResult<Vec<(super::HostDeviceMemory, u64, u64)>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count = count as usize;
    if count > MAX_MAPPED_RANGES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `memoryRangeCount = {count}`, and \
             this layer reads at most {MAX_MAPPED_RANGES}. The count is a guest `uint32_t` \
             indexing an array of {MAPPED_MEMORY_RANGE_BYTES}-byte structures, so honouring it \
             unbounded would be a guest-controlled host allocation (Global Constraint 11)",
            caller = at.caller
        )));
    }
    let array_at = require_pointer(at, call, "pMemoryRanges", array)?;
    let bytes = c.mem().read_bytes(array_at, count * MAPPED_MEMORY_RANGE_BYTES, c.blame(2))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * MAPPED_MEMORY_RANGE_BYTES..][..MAPPED_MEMORY_RANGE_BYTES];
        let stype = u32::from_le_bytes(entry[0..4].try_into().expect("four"));
        if stype != STYPE_MAPPED_MEMORY_RANGE {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} and `pMemoryRanges[{index}].sType` is \
                 {stype}, where `VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE` is \
                 {STYPE_MAPPED_MEMORY_RANGE}. The `memory` member would be read at an offset \
                 belonging to a different structure and looked up in this layer's registry",
                caller = at.caller
            )));
        }
        let next = u64::from_le_bytes(entry[8..16].try_into().expect("eight"));
        if next != 0 {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pMemoryRanges[{index}].pNext = {next:#x}`. This layer does not walk `pNext` \
                 chains; see `vkAllocateMemory`'s refusal for the argument",
                caller = at.caller
            )));
        }
        let memory = u64::from_le_bytes(entry[16..24].try_into().expect("eight"));
        let token = vulkan.device_memory_token(at, call, memory)?;
        let offset = u64::from_le_bytes(entry[24..32].try_into().expect("eight"));
        let size = u64::from_le_bytes(entry[32..40].try_into().expect("eight"));
        out.push((token, offset, size));
    }
    Ok(out)
}

/// Put a `DriverAnswer<()>` into `X0`, recording a failure.
fn answer(c: &mut ImportCall<'_, '_>, vulkan: &Arc<Vulkan>, call: &str, result: DriverAnswer<()>) {
    match result {
        DriverAnswer::Failed(code) => {
            vulkan.note_driver_result(call, code);
            c.ret().i32(code);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure sizes are the ones the specification fixes**, with the arithmetic written
    /// out so a reader can check it against `vulkan_core.h` rather than remember it.
    #[test]
    fn the_memory_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_MEMORY_ALLOCATE_INFO, 5);
        assert_eq!(STYPE_MAPPED_MEMORY_RANGE, 6);
        // `allocationSize` is a `VkDeviceSize` and therefore 8-aligned, so `pNext` at 8 is
        // followed by it at 16 and `memoryTypeIndex` lands at 24 with four bytes of tail padding.
        assert_eq!(MEMORY_ALLOCATE_INFO_BYTES, 32);
        assert_eq!(MEMORY_REQUIREMENTS_BYTES, 8 + 8 + 4 + 4);
        assert_eq!(MAPPED_MEMORY_RANGE_BYTES, 16 + 8 + 8 + 8);
        assert_eq!(VK_WHOLE_SIZE, u64::MAX);
    }

    /// **The bits the rewrite clears are exactly the three that promise a mappable allocation.**
    ///
    /// Clearing `HOST_VISIBLE` alone would leave `HOST_COHERENT` set on a type with no
    /// `HOST_VISIBLE`, which the specification does not permit and which an engine reading the
    /// list would be entitled to find surprising. Clearing `DEVICE_LOCAL` as well would be this
    /// layer lying about where the memory is.
    #[test]
    fn the_masked_bits_are_the_host_visible_trio_and_nothing_else() {
        assert_eq!(MemoryPlan::HOST_VISIBLE, 0x2);
        assert_eq!(MemoryPlan::HOST_COHERENT, 0x4);
        assert_eq!(MemoryPlan::HOST_CACHED, 0x8);
        assert_eq!(HOST_PROPERTY_BITS, 0xe);
        // `VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT` is 0x1 and is deliberately **not** in the mask.
        assert_eq!(HOST_PROPERTY_BITS & 0x1, 0, "DEVICE_LOCAL is a fact, not a promise");
        // The measured ReBAR type on this machine: DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT.
        assert_eq!(0x7 & !HOST_PROPERTY_BITS, 0x1, "what the guest is shown for type 4");
    }

    /// A plan's `host_visible` is the specification's bit and not a second opinion.
    #[test]
    fn a_plan_is_host_visible_exactly_when_the_driver_says_it_is() {
        let plan = |flags| MemoryPlan { property_flags: flags, importable: true, import_alignment: 4096 };
        assert!(!plan(0x1).host_visible(), "DEVICE_LOCAL alone");
        assert!(plan(0x6).host_visible(), "HOST_VISIBLE | HOST_COHERENT");
        assert!(plan(0x7).host_visible(), "the ReBAR type is host-visible to the driver");
        assert!(!plan(0x0).host_visible());
    }
}
