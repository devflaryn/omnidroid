//! **Query pools: the engine's GPU timer.**
//!
//! MEASURED: once its swapchain, image views, semaphores, fences and command pools exist, the
//! engine creates `gpuTimeQueryPool` (`0x2592d68`..`0x2592da0` in `libroblox.so`, the string is
//! its own assertion text) -- a pool of timestamp queries it reads back to time the GPU's frame.
//! A `VkQueryPool` is a non-dispatchable handle of its own family ([`MAX_QUERY_POOLS`]); its
//! create info has no pointer beyond `pNext`, which is refused by name as every create info's is
//! here ([`check_header`]).
//!
//! [`MAX_QUERY_POOLS`]: super::MAX_QUERY_POOLS

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, QueryPoolRequest};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::resource::check_header;
use super::{Site, Vulkan, VK_SUCCESS};

/// `VK_QUERY_TYPE_OCCLUSION`: one value per query.
const QUERY_TYPE_OCCLUSION: u32 = 0;
/// `VK_QUERY_TYPE_PIPELINE_STATISTICS`: one value per statistic the pool was created with.
const QUERY_TYPE_PIPELINE_STATISTICS: u32 = 1;
/// `VK_QUERY_TYPE_TIMESTAMP`: one value per query.
const QUERY_TYPE_TIMESTAMP: u32 = 2;

/// `VK_QUERY_RESULT_64_BIT`.
const RESULT_64_BIT: u32 = 0x1;
/// `VK_QUERY_RESULT_WAIT_BIT`.
const RESULT_WAIT_BIT: u32 = 0x2;
/// `VK_QUERY_RESULT_WITH_AVAILABILITY_BIT`: one more value per query.
const RESULT_WITH_AVAILABILITY_BIT: u32 = 0x4;
/// `VK_QUERY_RESULT_PARTIAL_BIT`.
const RESULT_PARTIAL_BIT: u32 = 0x8;

/// How many bytes one `vkGetQueryPoolResults` may have the driver write.
///
/// An allocation bound: the span is `(queryCount - 1) * stride` plus one query's results, and
/// `stride` is a guest `VkDeviceSize`. 4,096 timestamps with availability, 64-bit, packed. MEASURED,
/// the engine's GPU timer reads 16 bytes.
pub const MAX_QUERY_RESULT_BYTES: usize = 4096 * 16;

/// `VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO`.
pub const STYPE_QUERY_POOL_CREATE_INFO: u32 = 11;

/// `sizeof(VkQueryPoolCreateInfo)`.
///
/// ```text
/// VkStructureType                  sType;               //  0  (then 4 of padding)
/// const void                      *pNext;               //  8
/// VkQueryPoolCreateFlags           flags;               // 16
/// VkQueryType                      queryType;           // 20
/// uint32_t                         queryCount;          // 24
/// VkQueryPipelineStatisticFlags    pipelineStatistics;  // 28
/// ```
pub const QUERY_POOL_CREATE_INFO_BYTES: usize = 32;

/// `VkResult vkCreateQueryPool(VkDevice device, const VkQueryPoolCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkQueryPool *pQueryPool)`
pub(super) fn create_query_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateQueryPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pQueryPool", args[3])?;

    let info = c.mem().read_bytes(info_at, QUERY_POOL_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_QUERY_POOL_CREATE_INFO,
        "VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO",
        "`queryType` and `queryCount` would be read at offsets belonging to a different structure",
        "a query-pool `pNext` chain carries `VkQueryPoolPerformanceCreateInfoKHR`, which selects \
         performance counters",
    )?;
    let word = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let request = QueryPoolRequest {
        flags: word(16),
        query_type: word(20),
        query_count: word(24),
        pipeline_statistics: word(28),
    };

    match host.create_query_pool(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_query_pool(at, token)?;
            vulkan.remember_query_pool_shape(token, request);
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkCmdResetQueryPool(VkCommandBuffer commandBuffer, VkQueryPool queryPool,
/// uint32_t firstQuery, uint32_t queryCount)`
///
/// MEASURED: the first command the engine records, `(pool, 0, 2)` -- the GPU timer's two
/// timestamps, reset before a frame writes them.
pub(super) fn cmd_reset_query_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdResetQueryPool";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let pool = vulkan.query_pool_token(at, CALL, args[1])?;
    host.cmd_reset_query_pool(buffer, pool, args[2] as u32, args[3] as u32)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdWriteTimestamp(VkCommandBuffer commandBuffer, VkPipelineStageFlagBits
/// pipelineStage, VkQueryPool queryPool, uint32_t query)`
///
/// The other half of a timestamp pool's use, and the only way one is written: the pool the engine
/// created is `VK_QUERY_TYPE_TIMESTAMP`, which nothing but this command fills.
pub(super) fn cmd_write_timestamp(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdWriteTimestamp";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let pool = vulkan.query_pool_token(at, CALL, args[2])?;
    host.cmd_write_timestamp(buffer, args[1] as u32, pool, args[3] as u32)?;
    c.ret().void();
    Ok(())
}

/// `VkResult vkGetQueryPoolResults(VkDevice device, VkQueryPool queryPool, uint32_t firstQuery,
/// uint32_t queryCount, size_t dataSize, void *pData, VkDeviceSize stride,
/// VkQueryResultFlags flags)`
///
/// MEASURED: the GPU timer reads its two timestamps as `(0, 2, 16, pData, 8,
/// VK_QUERY_RESULT_64_BIT)` -- **without `WAIT`**, so the driver may answer `VK_NOT_READY` and
/// write only the queries that are available. The ones it does not write keep what the guest's
/// buffer held: so the host is handed a copy of **the guest's own bytes** to write into, and the
/// copy goes back whole. A zeroed host buffer copied back would be a timestamp of 0 the guest
/// never had.
///
/// With no validation layer, a `dataSize` too small for the results the driver writes is a driver
/// write past the end of the host's buffer; so the span is computed here, from the pool's own
/// query type -- kept since `vkCreateQueryPool` -- and the flags, and a `dataSize` short of it is
/// refused by name.
pub(super) fn get_query_pool_results(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetQueryPoolResults";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let pool = vulkan.query_pool_token(at, CALL, args[1])?;
    let Some(shape) = vulkan.query_pool_shape(pool) else {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} on a query pool this layer registered \
             but kept no shape for, so the size of its results cannot be known",
            caller = at.caller
        )));
    };
    let (first, count) = (args[2] as u32, args[3] as u32);
    let data_size = args[4];
    let data_at = require_pointer(at, CALL, "pData", args[5])?;
    let (stride, flags) = (args[6], args[7] as u32);

    if u64::from(first) + u64::from(count) > u64::from(shape.query_count) {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} for queries {first}..{end} of a pool of \
             {pool_count}, which the specification forbids",
            caller = at.caller,
            end = u64::from(first) + u64::from(count),
            pool_count = shape.query_count
        )));
    }
    let known = RESULT_64_BIT | RESULT_WAIT_BIT | RESULT_WITH_AVAILABILITY_BIT | RESULT_PARTIAL_BIT;
    if flags & !known != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `flags = {flags:#x}`; the bits \
             {unknown:#x} are not core Vulkan's, and one of them (`WITH_STATUS_BIT_KHR`) changes \
             how many values each query writes",
            caller = at.caller,
            unknown = flags & !known
        )));
    }
    let values: u64 = match shape.query_type {
        QUERY_TYPE_OCCLUSION | QUERY_TYPE_TIMESTAMP => 1,
        QUERY_TYPE_PIPELINE_STATISTICS => u64::from(shape.pipeline_statistics.count_ones()),
        other => {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} on a pool of `queryType = {other}`, \
                 whose results this layer does not know the size of",
                caller = at.caller
            )))
        }
    };
    let width: u64 = if flags & RESULT_64_BIT != 0 { 8 } else { 4 };
    let availability = u64::from(flags & RESULT_WITH_AVAILABILITY_BIT != 0);
    let per_query = (values + availability) * width;
    if stride % width != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `stride = {stride}`, which is not a \
             multiple of the {width}-byte values it asked for; the specification forbids it",
            caller = at.caller
        )));
    }
    let span = if count == 0 {
        Some(0)
    } else {
        stride.checked_mul(u64::from(count - 1)).and_then(|lead| lead.checked_add(per_query))
    };
    let span = match span {
        Some(span) if span <= MAX_QUERY_RESULT_BYTES as u64 => span as usize,
        _ => {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} for {count} queries at a stride of \
                 {stride}, a span past the {MAX_QUERY_RESULT_BYTES} bytes this layer has the \
                 driver write in one call (Global Constraint 11)",
                caller = at.caller
            )))
        }
    };
    if data_size < span as u64 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `dataSize = {data_size}`, and \
             {count} queries of {per_query} bytes at a stride of {stride} need {span}. The driver \
             would write past the end of the buffer, which with no validation layer here is a \
             write into whatever follows it",
            caller = at.caller
        )));
    }

    let mut data = c.mem().read_bytes(data_at, span, c.blame(5))?;
    let result = host.get_query_pool_results(device, pool, first, count, stride, flags, &mut data)?;
    if result >= 0 {
        // `VK_SUCCESS` or `VK_NOT_READY`: what the driver wrote, and the guest's own bytes where it
        // wrote nothing.
        c.mem().write_bytes(data_at, &data, c.blame(5))?;
    } else {
        vulkan.note_driver_result(CALL, result);
    }
    c.ret().i32(result);
    Ok(())
}

/// `void vkDestroyQueryPool(VkDevice device, VkQueryPool queryPool,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_query_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyQueryPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "queryPool", args[1])?;
    let token = vulkan.query_pool_token(at, CALL, args[1])?;
    host.destroy_query_pool(token)?;
    vulkan.forget_query_pool(handle);
    vulkan.forget_query_pool_shape(token);
    c.ret().void();
    Ok(())
}
