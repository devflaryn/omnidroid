//! **Descriptor set layouts, pools, sets, and the update that points them at something.**
//!
//! # `vkUpdateDescriptorSets` is where a wrong handle is quietest
//!
//! Every other call in this stage either fails or produces an object. This one returns **`void`**:
//! there is no `VkResult` for a caller to disbelieve, no object to inspect, and no validation
//! layer on this machine (`docs/research/graphics-spike.md` §6). A descriptor pointed at the wrong
//! image view does not fail — the draw samples something else. A descriptor pointed at a buffer
//! that was freed does not fail either; the GPU reads memory it was not given, and what happens
//! next depends on the driver.
//!
//! So every handle in a write goes through its registry, and **which array is live is decided by
//! `descriptorType` rather than by which pointer happens to be non-NULL**. That is the rule the
//! specification states and it is the one a shim is most likely to get wrong: a
//! `COMBINED_IMAGE_SAMPLER` write whose `pBufferInfo` was read instead would produce a descriptor
//! built out of a `VkDescriptorBufferInfo`'s bytes, which are a plausible
//! `VkDescriptorImageInfo`. [`DescriptorWrites`](super::host::DescriptorWrites) makes that
//! unrepresentable past this file: one variant is filled, and a host cannot read the other.
//!
//! # `pTexelBufferView` is refused rather than dropped
//!
//! `VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER` and its storage counterpart point at a
//! `VkBufferView`, and this stage creates none (see [`resource`](super::resource) for why D17 says
//! that is the honest state). A write of that type is therefore a refusal naming the type, not a
//! write with an empty array — which would leave the descriptor holding whatever the pool was
//! allocated with.
//!
//! # A pool owns its sets, and the registry has to agree
//!
//! `vkDestroyDescriptorPool` and `vkResetDescriptorPool` both free **every** set in the pool
//! without naming one, exactly as `vkDestroyCommandPool` frees its buffers. A `VkDescriptorSet`
//! left in the registry afterwards would resolve to a token whose driver object is gone, and
//! binding it is the wild non-dispatchable handle Global Constraint 1 is about — reached with a
//! handle this layer itself issued. [`VulkanHost::descriptor_sets_of`](super::VulkanHost) is what
//! lets the shim take them back at the same moment.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{
    DescriptorBinding, DescriptorCopy, DescriptorPoolRequest, DescriptorSetLayoutRequest,
    DescriptorWrite, DescriptorWrites, DriverAnswer, HostImageView, HostSampler,
};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::resource::check_header;
use super::shader::read_u64_array;
use super::{Site, Vulkan, VK_SUCCESS};

/// One `VkDescriptorImageInfo`, resolved: `(sampler, view, layout)`.
///
/// A named alias because both handles are optional and which of the two is live is decided by the
/// `descriptorType` — see [`DescriptorWrites::Images`](super::host::DescriptorWrites::Images),
/// which is where the same triple is documented.
type ImageDescriptor = (Option<HostSampler>, Option<HostImageView>, u32);

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO`.
const STYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO: u32 = 32;
/// `VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO`.
const STYPE_DESCRIPTOR_POOL_CREATE_INFO: u32 = 33;
/// `VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO`.
const STYPE_DESCRIPTOR_SET_ALLOCATE_INFO: u32 = 34;
/// `VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET`.
const STYPE_WRITE_DESCRIPTOR_SET: u32 = 35;
/// `VK_STRUCTURE_TYPE_COPY_DESCRIPTOR_SET`.
const STYPE_COPY_DESCRIPTOR_SET: u32 = 36;

/// `sizeof(VkDescriptorSetLayoutCreateInfo)`: `flags` 16, `bindingCount` 20, `pBindings` 24.
pub const DESCRIPTOR_SET_LAYOUT_CREATE_INFO_BYTES: usize = 32;

/// `sizeof(VkDescriptorSetLayoutBinding)`.
///
/// ```text
/// uint32_t            binding;               //  0
/// VkDescriptorType    descriptorType;        //  4
/// uint32_t            descriptorCount;       //  8
/// VkShaderStageFlags  stageFlags;            // 12
/// const VkSampler    *pImmutableSamplers;    // 16
/// ```
pub const DESCRIPTOR_SET_LAYOUT_BINDING_BYTES: usize = 24;

/// `sizeof(VkDescriptorPoolCreateInfo)`: `flags` 16, `maxSets` 20, `poolSizeCount` 24 (then 4 of
/// padding), `pPoolSizes` 32.
pub const DESCRIPTOR_POOL_CREATE_INFO_BYTES: usize = 40;

/// `sizeof(VkDescriptorPoolSize)`: `type` and `descriptorCount`.
pub const DESCRIPTOR_POOL_SIZE_BYTES: usize = 8;

/// `sizeof(VkDescriptorSetAllocateInfo)`: `descriptorPool` 16, `descriptorSetCount` 24 (then 4),
/// `pSetLayouts` 32.
pub const DESCRIPTOR_SET_ALLOCATE_INFO_BYTES: usize = 40;

/// `sizeof(VkWriteDescriptorSet)`.
///
/// ```text
/// VkStructureType                sType;              //  0  (then 4 of padding)
/// const void                    *pNext;              //  8
/// VkDescriptorSet                dstSet;             // 16  (a uint64_t)
/// uint32_t                       dstBinding;         // 24
/// uint32_t                       dstArrayElement;    // 28
/// uint32_t                       descriptorCount;    // 32
/// VkDescriptorType               descriptorType;     // 36
/// const VkDescriptorImageInfo   *pImageInfo;         // 40
/// const VkDescriptorBufferInfo  *pBufferInfo;        // 48
/// const VkBufferView            *pTexelBufferView;   // 56
/// ```
pub const WRITE_DESCRIPTOR_SET_BYTES: usize = 64;

/// `sizeof(VkCopyDescriptorSet)`: `srcSet` 16, `srcBinding` 24, `srcArrayElement` 28, `dstSet` 32,
/// `dstBinding` 40, `dstArrayElement` 44, `descriptorCount` 48 (then 4 of padding).
pub const COPY_DESCRIPTOR_SET_BYTES: usize = 56;

/// `sizeof(VkDescriptorImageInfo)`: `sampler` 0, `imageView` 8, `imageLayout` 16 (then 4).
pub const DESCRIPTOR_IMAGE_INFO_BYTES: usize = 24;

/// `sizeof(VkDescriptorBufferInfo)`: `buffer` 0, `offset` 8, `range` 16.
pub const DESCRIPTOR_BUFFER_INFO_BYTES: usize = 24;

// ------------------------------------------------------------------- the descriptor type codes
//
// Only the ones this module has to *branch* on are named. The rest travel as the guest's
// `uint32_t` to the driver, which is what knows them.

/// `VK_DESCRIPTOR_TYPE_SAMPLER`.
const TYPE_SAMPLER: u32 = 0;
/// `VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER`.
const TYPE_COMBINED_IMAGE_SAMPLER: u32 = 1;
/// `VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE`.
const TYPE_SAMPLED_IMAGE: u32 = 2;
/// `VK_DESCRIPTOR_TYPE_STORAGE_IMAGE`.
const TYPE_STORAGE_IMAGE: u32 = 3;
/// `VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER`.
const TYPE_UNIFORM_TEXEL_BUFFER: u32 = 4;
/// `VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER`.
const TYPE_STORAGE_TEXEL_BUFFER: u32 = 5;
/// `VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER`.
const TYPE_UNIFORM_BUFFER: u32 = 6;
/// `VK_DESCRIPTOR_TYPE_STORAGE_BUFFER_DYNAMIC`, the last of the four buffer types.
const TYPE_STORAGE_BUFFER_DYNAMIC: u32 = 9;
/// `VK_DESCRIPTOR_TYPE_INPUT_ATTACHMENT`.
const TYPE_INPUT_ATTACHMENT: u32 = 10;

// ------------------------------------------------------------------------------ the bounds

/// How many bindings one descriptor set layout declares.
pub const MAX_DESCRIPTOR_BINDINGS: usize = 32;

/// How many immutable samplers one binding carries.
pub const MAX_IMMUTABLE_SAMPLERS: usize = 16;

/// How many `VkDescriptorPoolSize` entries one pool is created with. Vulkan 1.0 has eleven
/// descriptor types, so sixteen is every type with room.
pub const MAX_POOL_SIZES: usize = 16;

/// How many descriptor sets one `vkAllocateDescriptorSets` produces.
pub const MAX_SETS_PER_CALL: usize = 16;

/// How many sets one `vkFreeDescriptorSets` frees.
pub const MAX_SETS_PER_FREE: usize = 64;

/// How many `VkWriteDescriptorSet`s one `vkUpdateDescriptorSets` applies.
pub const MAX_DESCRIPTOR_WRITES: usize = 128;

/// How many `VkCopyDescriptorSet`s one `vkUpdateDescriptorSets` applies.
pub const MAX_DESCRIPTOR_COPIES: usize = 64;

/// How many descriptors one write covers — `descriptorCount`, which indexes `pImageInfo` or
/// `pBufferInfo`.
pub const MAX_DESCRIPTORS_PER_WRITE: usize = 256;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkCreateDescriptorSetLayout(VkDevice device,
/// const VkDescriptorSetLayoutCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkDescriptorSetLayout *pSetLayout)`
pub(super) fn create_descriptor_set_layout(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateDescriptorSetLayout";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pSetLayout", args[3])?;

    let info = c.mem().read_bytes(info_at, DESCRIPTOR_SET_LAYOUT_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        "VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO",
        "`pBindings` would be read at an offset belonging to a different structure, and a layout \
         built from the wrong bindings makes every pipeline and every set built from it disagree \
         with the shader",
        "a set-layout `pNext` chain carries `VkDescriptorSetLayoutBindingFlagsCreateInfo`, which \
         is what makes a binding partially bound or variable-sized -- dropping it would produce a \
         layout whose descriptors the shader may not index the way it was compiled to",
    )?;
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four"));
    let count = u32::from_le_bytes(info[20..24].try_into().expect("four")) as usize;
    let pointer = u64::from_le_bytes(info[24..32].try_into().expect("eight"));

    let bindings = decode_bindings(c, at, vulkan, CALL, count, pointer)?;
    let request = DescriptorSetLayoutRequest { flags, bindings };

    match host.create_descriptor_set_layout(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_descriptor_set_layout(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyDescriptorSetLayout(VkDevice device, VkDescriptorSetLayout descriptorSetLayout,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_descriptor_set_layout(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyDescriptorSetLayout";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "descriptorSetLayout", args[1])?;
    let token = vulkan.descriptor_set_layout_token(at, CALL, args[1])?;
    host.destroy_descriptor_set_layout(token)?;
    vulkan.forget_descriptor_set_layout(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateDescriptorPool(VkDevice device,
/// const VkDescriptorPoolCreateInfo *pCreateInfo, const VkAllocationCallbacks *pAllocator,
/// VkDescriptorPool *pDescriptorPool)`
pub(super) fn create_descriptor_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateDescriptorPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pDescriptorPool", args[3])?;

    let info = c.mem().read_bytes(info_at, DESCRIPTOR_POOL_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_DESCRIPTOR_POOL_CREATE_INFO,
        "VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO",
        "`maxSets` and `pPoolSizes` would be read at offsets belonging to a different structure, \
         and a pool sized from them runs out at an allocation the engine had budgeted for",
        "a descriptor-pool `pNext` chain carries `VkDescriptorPoolInlineUniformBlockCreateInfo`",
    )?;
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four"));
    let max_sets = u32::from_le_bytes(info[20..24].try_into().expect("four"));
    let count = u32::from_le_bytes(info[24..28].try_into().expect("four")) as usize;
    let pointer = u64::from_le_bytes(info[32..40].try_into().expect("eight"));

    let sizes = if count == 0 || pointer == 0 {
        Vec::new()
    } else {
        if count > MAX_POOL_SIZES {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with `poolSizeCount = {count}`, and \
                 this layer reads at most {MAX_POOL_SIZES}. Vulkan 1.0 has eleven descriptor \
                 types, so a larger count is either repetition or a structure read at the wrong \
                 offset",
                caller = at.caller
            )));
        }
        let array_at = guest_pointer(at, "pPoolSizes", pointer)?;
        let bytes = c.mem().read_bytes(array_at, count * DESCRIPTOR_POOL_SIZE_BYTES, c.blame(1))?;
        (0..count)
            .map(|index| {
                let entry = &bytes[index * DESCRIPTOR_POOL_SIZE_BYTES..];
                (
                    u32::from_le_bytes(entry[0..4].try_into().expect("four")),
                    u32::from_le_bytes(entry[4..8].try_into().expect("four")),
                )
            })
            .collect()
    };

    let request = DescriptorPoolRequest { flags, max_sets, sizes };
    match host.create_descriptor_pool(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_descriptor_pool(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyDescriptorPool(VkDevice device, VkDescriptorPool descriptorPool,
/// const VkAllocationCallbacks *pAllocator)`
///
/// **Every set allocated from the pool goes with it.** See this module's header: a
/// `VkDescriptorSet` handle outliving its pool is a non-dispatchable value the driver would still
/// look up, and it is reachable with a handle this layer itself issued.
pub(super) fn destroy_descriptor_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyDescriptorPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "descriptorPool", args[1])?;
    let token = vulkan.descriptor_pool_token(at, CALL, args[1])?;
    // Asked **before** the pool is destroyed, because afterwards the host has nothing to answer
    // from -- the same order `vkDestroyCommandPool` uses for its buffers.
    let sets = host.descriptor_sets_of(token)?;
    host.destroy_descriptor_pool(token)?;
    vulkan.forget_descriptor_pool(handle);
    vulkan.forget_descriptor_sets(&sets);
    c.ret().void();
    Ok(())
}

/// `VkResult vkResetDescriptorPool(VkDevice device, VkDescriptorPool descriptorPool,
/// VkDescriptorPoolResetFlags flags)`
pub(super) fn reset_descriptor_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkResetDescriptorPool";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let token = vulkan.descriptor_pool_token(at, CALL, args[1])?;
    let sets = host.descriptor_sets_of(token)?;
    match host.reset_descriptor_pool(token, args[2] as u32)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(()) => {
            // Only after the driver said it did it. A reset that failed leaves the sets alive,
            // and dropping their handles first would take them away from a guest that still has
            // them.
            vulkan.forget_descriptor_sets(&sets);
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `VkResult vkAllocateDescriptorSets(VkDevice device,
/// const VkDescriptorSetAllocateInfo *pAllocateInfo, VkDescriptorSet *pDescriptorSets)`
pub(super) fn allocate_descriptor_sets(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkAllocateDescriptorSets";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pAllocateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pDescriptorSets", args[2])?;

    let info = c.mem().read_bytes(info_at, DESCRIPTOR_SET_ALLOCATE_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != STYPE_DESCRIPTOR_SET_ALLOCATE_INFO {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with a `pAllocateInfo` whose `sType` is \
             {stype}, and `VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO` is \
             {STYPE_DESCRIPTOR_SET_ALLOCATE_INFO}. `descriptorPool` would be read at an offset \
             belonging to a different structure and looked up in this layer's registry",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pAllocateInfo->pNext = {next:#x}`. \
             A descriptor-set allocation chain carries \
             `VkDescriptorSetVariableDescriptorCountAllocateInfo`, which sets the size of a \
             variable-count binding -- dropping it would allocate a set whose last binding has \
             the wrong number of descriptors, and the shader would index past it",
            caller = at.caller
        )));
    }
    let pool_handle = u64::from_le_bytes(info[16..24].try_into().expect("eight"));
    let pool = vulkan.descriptor_pool_token(at, CALL, pool_handle)?;
    let count = u32::from_le_bytes(info[24..28].try_into().expect("four")) as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `descriptorSetCount = 0`. Nothing \
             would be written into `pDescriptorSets`, and the guest would read whatever was in \
             its own array as a `VkDescriptorSet` and bind it",
            caller = at.caller
        )));
    }
    let layout_handles = read_u64_array(
        c,
        at,
        CALL,
        "pSetLayouts",
        count,
        u64::from_le_bytes(info[32..40].try_into().expect("eight")),
        MAX_SETS_PER_CALL,
        1,
    )?;
    if layout_handles.len() != count {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `descriptorSetCount = {count}` and a \
             `pSetLayouts` this layer read {read} entries from -- which happens when the pointer \
             is NULL. There is one layout per set and the specification requires the array",
            caller = at.caller,
            read = layout_handles.len()
        )));
    }
    let mut layouts = Vec::with_capacity(count);
    for handle in &layout_handles {
        layouts.push(vulkan.descriptor_set_layout_token(at, CALL, *handle)?);
    }

    match host.allocate_descriptor_sets(pool, &layouts)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(tokens) => {
            if tokens.len() != count {
                return Err(at.refuse(format!(
                    "the host answered `{CALL}` with {answered} set(s) for a \
                     `descriptorSetCount` of {count}. Writing fewer leaves the guest reading its \
                     own uninitialised memory as a handle; writing more is a host write past the \
                     end of a guest buffer",
                    answered = tokens.len()
                )));
            }
            let mut handles = Vec::with_capacity(count * 8);
            for token in &tokens {
                let registered = vulkan.register_descriptor_set(at, *token)?;
                c.mem().write_bytes(registered.at, &registered.image, c.blame(2))?;
                handles.extend_from_slice(&(registered.at as u64).to_le_bytes());
            }
            c.mem().write_bytes(out_at, &handles, c.blame(2))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `VkResult vkFreeDescriptorSets(VkDevice device, VkDescriptorPool descriptorPool,
/// uint32_t descriptorSetCount, const VkDescriptorSet *pDescriptorSets)`
pub(super) fn free_descriptor_sets(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkFreeDescriptorSets";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let pool = vulkan.descriptor_pool_token(at, CALL, args[1])?;
    let count = args[2] as u32 as usize;
    if count == 0 {
        c.ret().i32(VK_SUCCESS);
        return Ok(());
    }
    let handles =
        read_u64_array(c, at, CALL, "pDescriptorSets", count, args[3], MAX_SETS_PER_FREE, 3)?;
    let mut sets = Vec::with_capacity(handles.len());
    let mut addresses = Vec::with_capacity(handles.len());
    for handle in &handles {
        // `VK_NULL_HANDLE` entries are explicitly permitted here and are ignored, which is what
        // lets a guest free a partly-filled array without compacting it first.
        if *handle == 0 {
            continue;
        }
        sets.push(vulkan.descriptor_set_token(at, CALL, *handle)?);
        addresses.push(guest_pointer(at, "pDescriptorSets[..]", *handle)?);
    }

    match host.free_descriptor_sets(pool, &sets)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(()) => {
            for address in addresses {
                vulkan.forget_descriptor_set(address);
            }
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkUpdateDescriptorSets(VkDevice device, uint32_t descriptorWriteCount,
/// const VkWriteDescriptorSet *pDescriptorWrites, uint32_t descriptorCopyCount,
/// const VkCopyDescriptorSet *pDescriptorCopies)`
pub(super) fn update_descriptor_sets(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkUpdateDescriptorSets";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let writes = decode_writes(c, at, vulkan, CALL, args[1] as u32 as usize, args[2])?;
    let copies = decode_copies(c, at, vulkan, CALL, args[3] as u32 as usize, args[4])?;
    host.update_descriptor_sets(device, &writes, &copies)?;
    c.ret().void();
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// Decode `pBindings`, whose one pointer is `pImmutableSamplers`.
fn decode_bindings(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    count: usize,
    pointer: u64,
) -> AbiResult<Vec<DescriptorBinding>> {
    if count == 0 || pointer == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_DESCRIPTOR_BINDINGS {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `bindingCount = {count}`, and this \
             layer reads at most {MAX_DESCRIPTOR_BINDINGS}. The count is a guest `uint32_t` \
             indexing an array of {DESCRIPTOR_SET_LAYOUT_BINDING_BYTES}-byte structures",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pBindings", pointer)?;
    let bytes =
        c.mem().read_bytes(array_at, count * DESCRIPTOR_SET_LAYOUT_BINDING_BYTES, c.blame(1))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry =
            &bytes[index * DESCRIPTOR_SET_LAYOUT_BINDING_BYTES..][..DESCRIPTOR_SET_LAYOUT_BINDING_BYTES];
        let u32_at =
            |offset: usize| u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four"));
        let descriptor_count = u32_at(8);
        let samplers_pointer = u64::from_le_bytes(entry[16..24].try_into().expect("eight"));
        let descriptor_type = u32_at(4);
        // **`pImmutableSamplers` is only read for the two types that have one.** The
        // specification says the member is ignored otherwise, and a guest that left a stale
        // pointer there for a `UNIFORM_BUFFER` binding is conforming -- following it would be
        // this layer dereferencing a pointer the program does not claim is valid.
        let immutable_samplers = if samplers_pointer == 0
            || descriptor_count == 0
            || !matches!(descriptor_type, TYPE_SAMPLER | TYPE_COMBINED_IMAGE_SAMPLER)
        {
            Vec::new()
        } else {
            let handles = read_u64_array(
                c,
                at,
                call,
                "pImmutableSamplers",
                descriptor_count as usize,
                samplers_pointer,
                MAX_IMMUTABLE_SAMPLERS,
                1,
            )?;
            let mut samplers = Vec::with_capacity(handles.len());
            for handle in &handles {
                samplers.push(vulkan.sampler_token(at, call, *handle)?);
            }
            samplers
        };
        out.push(DescriptorBinding {
            binding: u32_at(0),
            descriptor_type,
            descriptor_count,
            stage_flags: u32_at(12),
            immutable_samplers,
        });
    }
    Ok(out)
}

/// Decode `pDescriptorWrites`, resolving every handle and choosing the live array by
/// `descriptorType`.
fn decode_writes(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    count: usize,
    pointer: u64,
) -> AbiResult<Vec<DescriptorWrite>> {
    if count == 0 || pointer == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_DESCRIPTOR_WRITES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `descriptorWriteCount = {count}`, \
             and this layer reads at most {MAX_DESCRIPTOR_WRITES}",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pDescriptorWrites", pointer)?;
    let bytes = c.mem().read_bytes(array_at, count * WRITE_DESCRIPTOR_SET_BYTES, c.blame(2))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * WRITE_DESCRIPTOR_SET_BYTES..][..WRITE_DESCRIPTOR_SET_BYTES];
        let u32_at =
            |offset: usize| u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four"));
        let u64_at = |offset: usize| {
            u64::from_le_bytes(entry[offset..offset + 8].try_into().expect("eight"))
        };
        let stype = u32_at(0);
        if stype != STYPE_WRITE_DESCRIPTOR_SET {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} and `pDescriptorWrites[{index}].sType` \
                 is {stype}, where `VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET` is \
                 {STYPE_WRITE_DESCRIPTOR_SET}. `dstSet`, `descriptorType` and all three array \
                 pointers would be read at offsets belonging to a different structure",
                caller = at.caller
            )));
        }
        if u64_at(8) != 0 {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorWrites[{index}].pNext = {next:#x}`. A write chain carries \
                 `VkWriteDescriptorSetInlineUniformBlock` and \
                 `VkWriteDescriptorSetAccelerationStructureKHR`, each of which *is* the data the \
                 write points at -- dropping one leaves a write with `descriptorCount` \
                 descriptors and nothing to fill them from. This layer does not walk chains",
                caller = at.caller,
                next = u64_at(8)
            )));
        }
        let set = vulkan.descriptor_set_token(at, call, u64_at(16))?;
        let descriptor_count = u32_at(32) as usize;
        let descriptor_type = u32_at(36);
        if descriptor_count == 0 {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorWrites[{index}].descriptorCount = 0`, which the specification \
                 forbids. A write of no descriptors is a call that succeeds and changes nothing, \
                 and the set would keep whatever the pool was allocated with",
                caller = at.caller
            )));
        }
        if descriptor_count > MAX_DESCRIPTORS_PER_WRITE {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorWrites[{index}].descriptorCount = {descriptor_count}`, and this \
                 layer reads at most {MAX_DESCRIPTORS_PER_WRITE}",
                caller = at.caller
            )));
        }

        // **The live array is chosen by `descriptorType`**, which is the specification's rule and
        // this file's header says what reading the other one would produce.
        let writes = match descriptor_type {
            TYPE_SAMPLER
            | TYPE_COMBINED_IMAGE_SAMPLER
            | TYPE_SAMPLED_IMAGE
            | TYPE_STORAGE_IMAGE
            | TYPE_INPUT_ATTACHMENT => {
                let images = decode_image_infos(
                    c,
                    at,
                    vulkan,
                    call,
                    index,
                    descriptor_type,
                    descriptor_count,
                    u64_at(40),
                )?;
                DescriptorWrites::Images(images)
            }
            TYPE_UNIFORM_BUFFER..=TYPE_STORAGE_BUFFER_DYNAMIC => {
                let buffers =
                    decode_buffer_infos(c, at, vulkan, call, index, descriptor_count, u64_at(48))?;
                DescriptorWrites::Buffers(buffers)
            }
            TYPE_UNIFORM_TEXEL_BUFFER | TYPE_STORAGE_TEXEL_BUFFER => {
                return Err(at.refuse(format!(
                    "the guest called `{call}` from {caller:#x} with \
                     `pDescriptorWrites[{index}].descriptorType = {descriptor_type}`, which is a \
                     texel-buffer descriptor and points at a `VkBufferView` through \
                     `pTexelBufferView`. **This stage creates no `VkBufferView`** -- \
                     `vkCreateBufferView` is refused by name, so the guest can hold no valid one \
                     -- and a write with an empty array would leave the descriptor holding \
                     whatever the pool was allocated with, which a draw would then read. \
                     `Vulkan::names()` reaching this is what says the view family is needed",
                    caller = at.caller
                )));
            }
            other => {
                return Err(at.refuse(format!(
                    "the guest called `{call}` from {caller:#x} with \
                     `pDescriptorWrites[{index}].descriptorType = {other}`, which is not one of \
                     Vulkan 1.0's eleven. Which of the three arrays is live is decided by this \
                     value alone, so a type this layer does not know is a write it cannot read \
                     the data of -- and guessing would mean reading a \
                     `VkDescriptorBufferInfo` as a `VkDescriptorImageInfo`, whose bytes are a \
                     plausible one",
                    caller = at.caller
                )));
            }
        };

        out.push(DescriptorWrite {
            set,
            binding: u32_at(24),
            array_element: u32_at(28),
            descriptor_type,
            writes,
        });
    }
    Ok(out)
}

/// Decode one write's `pImageInfo`.
#[allow(clippy::too_many_arguments)]
fn decode_image_infos(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    index: usize,
    descriptor_type: u32,
    count: usize,
    pointer: u64,
) -> AbiResult<Vec<ImageDescriptor>> {
    let array_at = require_pointer(at, call, "pDescriptorWrites[..].pImageInfo", pointer)?;
    let bytes = c.mem().read_bytes(array_at, count * DESCRIPTOR_IMAGE_INFO_BYTES, c.blame(2))?;
    // Which of the two handles is live is the type's business again: a bare `SAMPLER` descriptor
    // ignores `imageView`, and `SAMPLED_IMAGE`, `STORAGE_IMAGE` and `INPUT_ATTACHMENT` ignore
    // `sampler`. Resolving an ignored member would refuse a conforming guest that left it zero --
    // or worse, resolve a stale value it was entitled to leave there.
    let wants_sampler = matches!(descriptor_type, TYPE_SAMPLER | TYPE_COMBINED_IMAGE_SAMPLER);
    let wants_view = matches!(
        descriptor_type,
        TYPE_COMBINED_IMAGE_SAMPLER | TYPE_SAMPLED_IMAGE | TYPE_STORAGE_IMAGE | TYPE_INPUT_ATTACHMENT
    );
    let mut out = Vec::with_capacity(count);
    for entry_index in 0..count {
        let entry = &bytes[entry_index * DESCRIPTOR_IMAGE_INFO_BYTES..][..DESCRIPTOR_IMAGE_INFO_BYTES];
        let sampler_handle = u64::from_le_bytes(entry[0..8].try_into().expect("eight"));
        let view_handle = u64::from_le_bytes(entry[8..16].try_into().expect("eight"));
        let layout = u32::from_le_bytes(entry[16..20].try_into().expect("four"));
        let sampler = if wants_sampler && sampler_handle != 0 {
            Some(vulkan.sampler_token(at, call, sampler_handle)?)
        } else if wants_sampler {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorWrites[{index}].pImageInfo[{entry_index}].sampler = VK_NULL_HANDLE` \
                 for a descriptor of type {descriptor_type}, which the specification requires to \
                 have one unless the binding was created with an immutable sampler. This layer \
                 cannot tell those apart without the set's layout, so it refuses rather than \
                 writing a descriptor with no sampler -- which samples black",
                caller = at.caller
            )));
        } else {
            None
        };
        let view = if wants_view && view_handle != 0 {
            Some(vulkan.image_view_token(at, call, view_handle)?)
        } else if wants_view {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorWrites[{index}].pImageInfo[{entry_index}].imageView = \
                 VK_NULL_HANDLE` for a descriptor of type {descriptor_type}, which the \
                 specification requires to name a view. A descriptor with no view is one a draw \
                 reads and gets nothing from",
                caller = at.caller
            )));
        } else {
            None
        };
        out.push((sampler, view, layout));
    }
    Ok(out)
}

/// Decode one write's `pBufferInfo`.
fn decode_buffer_infos(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    index: usize,
    count: usize,
    pointer: u64,
) -> AbiResult<Vec<(super::HostBuffer, u64, u64)>> {
    let array_at = require_pointer(at, call, "pDescriptorWrites[..].pBufferInfo", pointer)?;
    let bytes = c.mem().read_bytes(array_at, count * DESCRIPTOR_BUFFER_INFO_BYTES, c.blame(2))?;
    let mut out = Vec::with_capacity(count);
    for entry_index in 0..count {
        let entry =
            &bytes[entry_index * DESCRIPTOR_BUFFER_INFO_BYTES..][..DESCRIPTOR_BUFFER_INFO_BYTES];
        let handle = u64::from_le_bytes(entry[0..8].try_into().expect("eight"));
        if handle == 0 {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorWrites[{index}].pBufferInfo[{entry_index}].buffer = \
                 VK_NULL_HANDLE`, which the core specification does not permit. A buffer \
                 descriptor with no buffer is one the shader reads from nowhere",
                caller = at.caller
            )));
        }
        out.push((
            vulkan.buffer_token(at, call, handle)?,
            u64::from_le_bytes(entry[8..16].try_into().expect("eight")),
            u64::from_le_bytes(entry[16..24].try_into().expect("eight")),
        ));
    }
    Ok(out)
}

/// Decode `pDescriptorCopies`.
fn decode_copies(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    count: usize,
    pointer: u64,
) -> AbiResult<Vec<DescriptorCopy>> {
    if count == 0 || pointer == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_DESCRIPTOR_COPIES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `descriptorCopyCount = {count}`, and \
             this layer reads at most {MAX_DESCRIPTOR_COPIES}",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pDescriptorCopies", pointer)?;
    let bytes = c.mem().read_bytes(array_at, count * COPY_DESCRIPTOR_SET_BYTES, c.blame(4))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * COPY_DESCRIPTOR_SET_BYTES..][..COPY_DESCRIPTOR_SET_BYTES];
        let u32_at =
            |offset: usize| u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four"));
        let u64_at = |offset: usize| {
            u64::from_le_bytes(entry[offset..offset + 8].try_into().expect("eight"))
        };
        let stype = u32_at(0);
        if stype != STYPE_COPY_DESCRIPTOR_SET {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} and \
                 `pDescriptorCopies[{index}].sType` is {stype}, where \
                 `VK_STRUCTURE_TYPE_COPY_DESCRIPTOR_SET` is {STYPE_COPY_DESCRIPTOR_SET}",
                caller = at.caller
            )));
        }
        if u64_at(8) != 0 {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pDescriptorCopies[{index}].pNext = {next:#x}`. This layer does not walk `pNext` \
                 chains",
                caller = at.caller,
                next = u64_at(8)
            )));
        }
        out.push(DescriptorCopy {
            source: vulkan.descriptor_set_token(at, call, u64_at(16))?,
            source_binding: u32_at(24),
            source_element: u32_at(28),
            destination: vulkan.descriptor_set_token(at, call, u64_at(32))?,
            destination_binding: u32_at(40),
            destination_element: u32_at(44),
            count: u32_at(48),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure sizes are the specification's**, with the arithmetic written out.
    #[test]
    fn the_descriptor_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, 32);
        assert_eq!(STYPE_DESCRIPTOR_POOL_CREATE_INFO, 33);
        assert_eq!(STYPE_DESCRIPTOR_SET_ALLOCATE_INFO, 34);
        assert_eq!(STYPE_WRITE_DESCRIPTOR_SET, 35);
        assert_eq!(STYPE_COPY_DESCRIPTOR_SET, 36);

        assert_eq!(DESCRIPTOR_SET_LAYOUT_CREATE_INFO_BYTES, 24 + 8);
        assert_eq!(DESCRIPTOR_SET_LAYOUT_BINDING_BYTES, 16 + 8);
        assert_eq!(DESCRIPTOR_POOL_CREATE_INFO_BYTES, 32 + 8);
        assert_eq!(DESCRIPTOR_POOL_SIZE_BYTES, 2 * 4);
        assert_eq!(DESCRIPTOR_SET_ALLOCATE_INFO_BYTES, 32 + 8);
        assert_eq!(WRITE_DESCRIPTOR_SET_BYTES, 56 + 8);
        assert_eq!(COPY_DESCRIPTOR_SET_BYTES, 48 + 4 + 4);
        assert_eq!(DESCRIPTOR_IMAGE_INFO_BYTES, 16 + 4 + 4);
        assert_eq!(DESCRIPTOR_BUFFER_INFO_BYTES, 8 + 8 + 8);
    }

    /// **The eleven descriptor types fall into exactly three groups, and the boundaries are where
    /// the specification puts them.**
    ///
    /// This is the partition `decode_writes` branches on, asserted as a partition rather than as
    /// three separate facts: a type that fell into two groups, or into none, would be read out of
    /// the wrong array — and a `VkDescriptorBufferInfo`'s 24 bytes are a perfectly plausible
    /// `VkDescriptorImageInfo`, so nothing downstream would notice.
    #[test]
    fn every_descriptor_type_belongs_to_exactly_one_array() {
        let images = |t: u32| {
            matches!(
                t,
                TYPE_SAMPLER
                    | TYPE_COMBINED_IMAGE_SAMPLER
                    | TYPE_SAMPLED_IMAGE
                    | TYPE_STORAGE_IMAGE
                    | TYPE_INPUT_ATTACHMENT
            )
        };
        let buffers = |t: u32| (TYPE_UNIFORM_BUFFER..=TYPE_STORAGE_BUFFER_DYNAMIC).contains(&t);
        let texels = |t: u32| matches!(t, TYPE_UNIFORM_TEXEL_BUFFER | TYPE_STORAGE_TEXEL_BUFFER);
        for kind in 0..=10u32 {
            let groups = usize::from(images(kind)) + usize::from(buffers(kind)) + usize::from(texels(kind));
            assert_eq!(groups, 1, "descriptor type {kind} belongs to {groups} groups, not one");
        }
        assert_eq!(TYPE_SAMPLER, 0);
        assert_eq!(TYPE_COMBINED_IMAGE_SAMPLER, 1);
        assert_eq!(TYPE_UNIFORM_TEXEL_BUFFER, 4);
        assert_eq!(TYPE_UNIFORM_BUFFER, 6);
        assert_eq!(TYPE_STORAGE_BUFFER_DYNAMIC, 9);
        assert_eq!(TYPE_INPUT_ATTACHMENT, 10);
    }
}
