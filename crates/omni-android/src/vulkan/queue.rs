//! **`vkQueueSubmit`, `vkQueuePresentKHR`, `vkQueueWaitIdle`, `vkDeviceWaitIdle`: the end of the
//! spine.**
//!
//! # `vkQueuePresentKHR` is the call rule 1 was written about
//!
//! The project's first rule forbids plausible stubs, and the example it names is this function: a
//! `vkQueuePresentKHR` that returns `VK_SUCCESS` without presenting is indistinguishable from one
//! that presented — for exactly as long as nobody looks at the screen. Every counter agrees, every
//! fence signals, the frame rate is perfect, and the window is black.
//!
//! So there is no branch anywhere in this file that produces a `VkResult` this layer chose. Every
//! code the guest receives came out of a driver, and the test that establishes it does not check
//! the code at all: it reads the **presented pixels back** and asserts the colour
//! (`tests/vulkan_present.rs`).
//!
//! # The two resize codes, again, and why present's are subtler than acquire's
//!
//! [`swapchain`](super::swapchain)'s header has the table. Present adds one wrinkle: it takes a
//! **list** of swapchains, and the specification says the aggregate `VkResult` is the worst of the
//! per-swapchain ones while `pResults` — when the caller supplies it — carries each one
//! separately. A single-swapchain present, which is every present a game makes, collapses the two;
//! a multi-swapchain present does not, and that is exactly when the difference matters.
//!
//! **`pResults` is therefore filled from the host's per-swapchain list and never from the
//! aggregate.** Copying the aggregate into every slot would be a plausible answer that is wrong
//! precisely when a caller bothered to ask — one window out of date and another fine would be
//! reported as both out of date, and the engine would rebuild a swapchain that did not need it.
//!
//! # `vkQueueWaitIdle` and `vkDeviceWaitIdle` are not bookkeeping
//!
//! They are the two calls that license the guest to destroy things. A `VK_SUCCESS` from either
//! means "no submitted work still references anything", and the guest acts on it immediately by
//! calling `vkDestroySwapchainKHR`, `vkFreeCommandBuffers` and the rest. A fabricated success here
//! is a use-after-free on the GPU — which the graphics spike measured the shape of: its own
//! swapchain use-after-free produced **zero** validation output and crashed the NVIDIA driver
//! instead, diagnosable only from the Windows Event Log.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, PresentRequest, SubmitRequest};
use super::instance::{guest_pointer, require_pointer};
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_SUBMIT_INFO`.
const STYPE_SUBMIT_INFO: u32 = 4;
/// `VK_STRUCTURE_TYPE_PRESENT_INFO_KHR`, which is `VK_KHR_swapchain`'s base plus one.
const STYPE_PRESENT_INFO_KHR: u32 = 1_000_001_001;

/// `sizeof(VkSubmitInfo)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType                sType;                  //  0  (then 4 of padding)
/// const void                    *pNext;                  //  8
/// uint32_t                       waitSemaphoreCount;     // 16  (then 4 of padding)
/// const VkSemaphore             *pWaitSemaphores;        // 24
/// const VkPipelineStageFlags    *pWaitDstStageMask;      // 32
/// uint32_t                       commandBufferCount;     // 40  (then 4 of padding)
/// const VkCommandBuffer         *pCommandBuffers;        // 48
/// uint32_t                       signalSemaphoreCount;   // 56  (then 4 of padding)
/// const VkSemaphore             *pSignalSemaphores;      // 64
/// ```
///
/// Three counts each followed by four bytes of padding, because each is followed by a pointer.
/// `pWaitDstStageMask` at 32 is the member a reader is likely to forget: it is a **second array**
/// the same length as `pWaitSemaphores`, not a scalar, and a shim that read it as one would hand
/// the driver a stage mask that is really a pointer.
pub const SUBMIT_INFO_BYTES: usize = 72;

/// `sizeof(VkPresentInfoKHR)`.
///
/// ```text
/// VkStructureType        sType;                //  0  (then 4 of padding)
/// const void            *pNext;                //  8
/// uint32_t               waitSemaphoreCount;   // 16  (then 4 of padding)
/// const VkSemaphore     *pWaitSemaphores;      // 24
/// uint32_t               swapchainCount;       // 32  (then 4 of padding)
/// const VkSwapchainKHR  *pSwapchains;          // 40
/// const uint32_t        *pImageIndices;        // 48
/// VkResult              *pResults;             // 56
/// ```
pub const PRESENT_INFO_BYTES: usize = 64;

/// How many `VkSubmitInfo`s one `vkQueueSubmit` may name.
///
/// An allocation bound. A renderer submits one per frame; sixteen covers a frame that batches
/// several passes into one call, which is the reason `submitCount` exists at all.
pub const MAX_SUBMITS: usize = 16;

/// How many semaphores or command buffers one `VkSubmitInfo` may name.
///
/// An allocation bound, and it is above [`MAX_SEMAPHORES`](super::MAX_SEMAPHORES) and
/// [`MAX_COMMAND_BUFFERS_PER_CALL`](super::MAX_COMMAND_BUFFERS_PER_CALL) so that the refusal a
/// guest sees comes from the registry — which can name the handle — rather than from here, where
/// the only thing that could be said is "too many".
const MAX_PER_SUBMIT: usize = 128;

/// How many swapchains one `vkQueuePresentKHR` may name.
///
/// An allocation bound, and equal to [`MAX_SWAPCHAINS`](super::MAX_SWAPCHAINS) because a guest
/// cannot legitimately present to more swapchains than exist.
pub const MAX_PRESENT_SWAPCHAINS: usize = super::MAX_SWAPCHAINS;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkQueueSubmit(VkQueue queue, uint32_t submitCount, const VkSubmitInfo *pSubmits,
/// VkFence fence)`
pub(super) fn queue_submit(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkQueueSubmit";
    let host = vulkan.require_host(at)?;
    let queue = vulkan.queue_token(at, CALL, args[0])?;
    let fence = match args[3] {
        0 => None,
        handle => Some(vulkan.fence_token(at, CALL, handle)?),
    };

    let count = args[1] as u32 as usize;
    if count > MAX_SUBMITS {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `submitCount = {count}`, and this \
             layer reads at most {MAX_SUBMITS}. The count is a guest `uint32_t` indexing an array \
             of {SUBMIT_INFO_BYTES}-byte structures, each of which names three further arrays, so \
             honouring it unbounded would be a guest-controlled host allocation (Global \
             Constraint 11)",
            caller = at.caller
        )));
    }

    let mut submits = Vec::with_capacity(count);
    if count > 0 {
        let array_at = require_pointer(at, CALL, "pSubmits", args[2])?;
        let bytes = c.mem().read_bytes(array_at, count * SUBMIT_INFO_BYTES, c.blame(2))?;
        for index in 0..count {
            let entry = bytes[index * SUBMIT_INFO_BYTES..][..SUBMIT_INFO_BYTES].to_vec();
            submits.push(decode_submit(c, at, vulkan, index, &entry)?);
        }
    }

    match host.queue_submit(queue, &submits, fence)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

/// Decode one `VkSubmitInfo`, resolving every handle in it through this layer's registries.
fn decode_submit(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    index: usize,
    entry: &[u8],
) -> AbiResult<SubmitRequest> {
    const CALL: &str = "vkQueueSubmit";
    let u32_at =
        |offset: usize| u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four"));
    let u64_at =
        |offset: usize| u64::from_le_bytes(entry[offset..offset + 8].try_into().expect("eight"));

    let stype = u32_at(0);
    if stype != STYPE_SUBMIT_INFO {
        return Err(at.refuse(format!(
            "the guest's `pSubmits[{index}]` has `sType` {stype}, and \
             `VK_STRUCTURE_TYPE_SUBMIT_INFO` is {STYPE_SUBMIT_INFO}. Every pointer after it would \
             be read at an offset belonging to a different structure, and this layer would then \
             follow three of them"
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest's `pSubmits[{index}]->pNext` is {next:#x}. A submit `pNext` chain is where \
             `VkTimelineSemaphoreSubmitInfo` goes -- the values a timeline semaphore is waited on \
             and signalled with -- and where `VkDeviceGroupSubmitInfo` and the external-semaphore \
             structures go. Dropping it would submit work that waits on nothing where the engine \
             asked it to wait on a value, which is a race rather than a failure"
        )));
    }

    let wait_count = u32_at(16) as usize;
    let waits = if wait_count == 0 {
        Vec::new()
    } else {
        bound(at, CALL, "waitSemaphoreCount", wait_count)?;
        let semaphores_at = require_pointer(at, CALL, "pWaitSemaphores", u64_at(24))?;
        // **The second array, which is the one a reader forgets.** `pWaitDstStageMask` is
        // `waitSemaphoreCount` `VkPipelineStageFlags` and not a scalar; reading it as one would
        // hand the driver a pointer value as a stage mask.
        let stages_at = require_pointer(at, CALL, "pWaitDstStageMask", u64_at(32))?;
        let semaphore_bytes = c.mem().read_bytes(semaphores_at, wait_count * 8, c.blame(2))?;
        let stage_bytes = c.mem().read_bytes(stages_at, wait_count * 4, c.blame(2))?;
        let mut waits = Vec::with_capacity(wait_count);
        for slot in 0..wait_count {
            let handle =
                u64::from_le_bytes(semaphore_bytes[slot * 8..slot * 8 + 8].try_into().expect("8"));
            let stage =
                u32::from_le_bytes(stage_bytes[slot * 4..slot * 4 + 4].try_into().expect("4"));
            waits.push((vulkan.semaphore_token(at, CALL, handle)?, stage));
        }
        waits
    };

    let buffer_count = u32_at(40) as usize;
    let command_buffers = if buffer_count == 0 {
        Vec::new()
    } else {
        bound(at, CALL, "commandBufferCount", buffer_count)?;
        let array_at = require_pointer(at, CALL, "pCommandBuffers", u64_at(48))?;
        let raw = c.mem().read_bytes(array_at, buffer_count * 8, c.blame(2))?;
        (0..buffer_count)
            .map(|slot| {
                let handle =
                    u64::from_le_bytes(raw[slot * 8..slot * 8 + 8].try_into().expect("eight"));
                vulkan.command_buffer_token(at, CALL, handle)
            })
            .collect::<AbiResult<Vec<_>>>()?
    };

    let signal_count = u32_at(56) as usize;
    let signals = if signal_count == 0 {
        Vec::new()
    } else {
        bound(at, CALL, "signalSemaphoreCount", signal_count)?;
        let array_at = require_pointer(at, CALL, "pSignalSemaphores", u64_at(64))?;
        let raw = c.mem().read_bytes(array_at, signal_count * 8, c.blame(2))?;
        (0..signal_count)
            .map(|slot| {
                let handle =
                    u64::from_le_bytes(raw[slot * 8..slot * 8 + 8].try_into().expect("eight"));
                vulkan.semaphore_token(at, CALL, handle)
            })
            .collect::<AbiResult<Vec<_>>>()?
    };

    Ok(SubmitRequest { waits, command_buffers, signals })
}

/// `VkResult vkQueuePresentKHR(VkQueue queue, const VkPresentInfoKHR *pPresentInfo)`
///
/// This module's header carries the argument for both of this function's careful parts: the
/// aggregate result travels verbatim, and `pResults` is filled from the host's per-swapchain list
/// rather than from that aggregate.
pub(super) fn queue_present(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkQueuePresentKHR";
    let host = vulkan.require_host(at)?;
    let queue = vulkan.queue_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pPresentInfo", args[1])?;

    let info = c.mem().read_bytes(info_at, PRESENT_INFO_BYTES, c.blame(1))?;
    let u32_at =
        |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let u64_at =
        |offset: usize| u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight"));

    let stype = u32_at(0);
    if stype != STYPE_PRESENT_INFO_KHR {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with a `pPresentInfo` whose `sType` is \
             {stype}, and `VK_STRUCTURE_TYPE_PRESENT_INFO_KHR` is {STYPE_PRESENT_INFO_KHR}. \
             `pSwapchains` would be read at byte 40 of a different structure and looked up in \
             this layer's swapchain registry",
            caller = at.caller
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pPresentInfo->pNext = {next:#x}`. A \
             present `pNext` chain carries `VkPresentRegionsKHR` (present only part of the \
             image), `VkPresentTimesInfoGOOGLE` (present at a time) and \
             `VkDisplayPresentInfoKHR`. Dropping it would present the whole image, now, where the \
             engine asked for something else -- and every call would answer `VK_SUCCESS`. This \
             layer does not know those layouts, so it refuses and names the address",
            caller = at.caller
        )));
    }

    let wait_count = u32_at(16) as usize;
    let waits = if wait_count == 0 {
        Vec::new()
    } else {
        bound(at, CALL, "waitSemaphoreCount", wait_count)?;
        let array_at = require_pointer(at, CALL, "pWaitSemaphores", u64_at(24))?;
        let raw = c.mem().read_bytes(array_at, wait_count * 8, c.blame(1))?;
        (0..wait_count)
            .map(|slot| {
                let handle =
                    u64::from_le_bytes(raw[slot * 8..slot * 8 + 8].try_into().expect("eight"));
                vulkan.semaphore_token(at, CALL, handle)
            })
            .collect::<AbiResult<Vec<_>>>()?
    };

    let swapchain_count = u32_at(32) as usize;
    if swapchain_count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `swapchainCount = 0`. The \
             specification requires at least one, and a present of no swapchains is a call that \
             succeeds and puts nothing on any screen -- which is the plausible success rule 1 \
             exists for, arriving through the one call it was written about",
            caller = at.caller
        )));
    }
    if swapchain_count > MAX_PRESENT_SWAPCHAINS {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with \
             `swapchainCount = {swapchain_count}`, and this layer holds at most \
             {MAX_PRESENT_SWAPCHAINS} swapchains -- so some of those handles cannot be ones it \
             issued. The count is a guest `uint32_t` (Global Constraint 11)",
            caller = at.caller
        )));
    }
    let swapchains_at = require_pointer(at, CALL, "pSwapchains", u64_at(40))?;
    let indices_at = require_pointer(at, CALL, "pImageIndices", u64_at(48))?;
    let swapchain_bytes = c.mem().read_bytes(swapchains_at, swapchain_count * 8, c.blame(1))?;
    let index_bytes = c.mem().read_bytes(indices_at, swapchain_count * 4, c.blame(1))?;
    let mut swapchains = Vec::with_capacity(swapchain_count);
    for slot in 0..swapchain_count {
        let handle =
            u64::from_le_bytes(swapchain_bytes[slot * 8..slot * 8 + 8].try_into().expect("8"));
        let index = u32::from_le_bytes(index_bytes[slot * 4..slot * 4 + 4].try_into().expect("4"));
        swapchains.push((vulkan.swapchain_token(at, CALL, handle)?, index));
    }

    // `pResults` is optional. When it is there it is `swapchainCount` `VkResult`s, and it is the
    // one output of this call besides the return value.
    let results_at = match u64_at(56) {
        0 => None,
        pointer => Some(guest_pointer(at, "pResults", pointer)?),
    };

    let presented = host.queue_present(
        queue,
        &PresentRequest {
            waits,
            swapchains,
            wants_per_swapchain_results: results_at.is_some(),
        },
    )?;

    if let Some(results_at) = results_at {
        if presented.per_swapchain.len() != swapchain_count {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` with a non-NULL `pResults` for {swapchain_count} \
                 swapchain(s) and the host answered {got} per-swapchain result(s). Writing the \
                 shorter list would leave the tail of the guest's array as whatever was in it, \
                 and filling it from the aggregate result would report one window's \
                 `VK_ERROR_OUT_OF_DATE_KHR` against another window that is fine -- which is \
                 exactly the case `pResults` exists for",
                got = presented.per_swapchain.len()
            )));
        }
        let bytes: Vec<u8> =
            presented.per_swapchain.iter().flat_map(|result| result.to_le_bytes()).collect();
        c.mem().write_bytes(results_at, &bytes, c.blame(1))?;
    }

    if presented.result != VK_SUCCESS {
        vulkan.note_driver_result(CALL, presented.result);
    }
    // **Verbatim.** `VK_SUBOPTIMAL_KHR` means the frame was presented and the swapchain no longer
    // matches the surface; the engine has a branch for it and this layer does not know which.
    c.ret().i32(presented.result);
    Ok(())
}

/// `VkResult vkQueueWaitIdle(VkQueue queue)`
pub(super) fn queue_wait_idle(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkQueueWaitIdle";
    let host = vulkan.require_host(at)?;
    let queue = vulkan.queue_token(at, CALL, args[0])?;
    match host.queue_wait_idle(queue)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

/// `VkResult vkDeviceWaitIdle(VkDevice device)`
pub(super) fn device_wait_idle(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDeviceWaitIdle";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    match host.device_wait_idle(device)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// A per-submit array length within [`MAX_PER_SUBMIT`], or a refusal naming the field.
fn bound(at: &Site, call: &str, field: &str, count: usize) -> AbiResult<()> {
    if count > MAX_PER_SUBMIT {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `{field} = {count}`, and this layer \
             reads at most {MAX_PER_SUBMIT}. The count is a guest `uint32_t` indexing an array of \
             handles, so honouring it unbounded would be a guest-controlled host allocation \
             (Global Constraint 11). It is also more handles of that family than this layer will \
             ever have issued, so every entry past its own bound would be refused individually in \
             any case",
            caller = at.caller
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure numbers are the ones the specification fixes**, with the padding written
    /// out as arithmetic.
    #[test]
    fn the_queue_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_SUBMIT_INFO, 4);
        assert_eq!(STYPE_PRESENT_INFO_KHR, 1_000_001_001);
        // `VK_KHR_swapchain` is extension 2, so its structure types start at 1000001000 and
        // `VkPresentInfoKHR` is the second of them.
        assert_eq!(STYPE_PRESENT_INFO_KHR, 1_000_000_000 + (2 - 1) * 1000 + 1);

        // `VkSubmitInfo`: three counts, each followed by four bytes of padding because each is
        // followed by a pointer. 16+4+4 = 24, 40+4+4 = 48, 56+4+4 = 64, then one pointer to 72.
        assert_eq!(16 + 4 + 4, 24);
        assert_eq!(40 + 4 + 4, 48);
        assert_eq!(56 + 4 + 4, 64);
        assert_eq!(64 + 8, SUBMIT_INFO_BYTES);
        assert_eq!(SUBMIT_INFO_BYTES, 72);

        // `VkPresentInfoKHR`: two counts with the same padding, then three pointers.
        assert_eq!(32 + 4 + 4, 40);
        assert_eq!(40 + 8 + 8 + 8, PRESENT_INFO_BYTES);
        assert_eq!(PRESENT_INFO_BYTES, 64);
    }

    /// The bounds are above what any renderer asks for, and the present bound is exactly the
    /// number of swapchains that can exist — so a guest cannot name one this layer did not issue
    /// and still be inside it.
    #[test]
    fn the_queue_bounds_are_above_what_any_renderer_asks_for() {
        assert_eq!(MAX_SUBMITS, 16);
        assert_eq!(MAX_PER_SUBMIT, 128);
        assert_eq!(MAX_PRESENT_SWAPCHAINS, super::super::MAX_SWAPCHAINS);
        // The largest read `submitCount` permits stays inside a page.
        assert_eq!(MAX_SUBMITS * SUBMIT_INFO_BYTES, 1152);
    }
}
