//! **Headless mode's Vulkan half**: which recordings drop their draws, decided at record time.
//!
//! Recording goes straight to the driver -- `vkCmdDraw` is `host.cmd_draw` the moment the guest
//! calls it (every command buffer is primary, see [`command`](super::command)) -- so a draw is
//! dropped when it is recorded, not when it is submitted:
//!
//! 1. `vkBeginCommandBuffer` **latches** the recording's mode: dropping when headless is on and no
//!    screenshot is waiting for a real frame ([`Headless::drops_now`]). A recording is one mode from
//!    beginning to end, whatever the switch does meanwhile.
//! 2. `vkCmdBeginRenderPass` in a dropping recording, onto a per-frame framebuffer
//!    ([`crate::headless::history`]; a framebuffer with a swapchain image view is always one), opens
//!    a **dropping pass**: the pass itself is forwarded (its clears, stores and layout transitions
//!    happen), and `vkCmdDraw`/`vkCmdDrawIndexed` inside it are not.
//! 3. `vkCmdEndRenderPass` closes it.
//!
//! Everything else -- barriers, copies, blits, compute, timestamps, submits, fences, presents -- is
//! forwarded untouched, so every fence the engine waits on is a real one.
//!
//! **The GPU timer** (`gpuTimeQueryPool`, [`query`](super::query)) would read a GPU that is suddenly
//! idle, and the engine's quality logic reads it. While headless, a two-timestamp read is answered
//! with the driver's first timestamp plus the **last real frame's** difference.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::headless::Headless;

/// The Vulkan layer's headless state.
#[derive(Debug, Default)]
pub struct VulkanHeadless {
    /// The switch, the frame count, the framebuffers' history, the pending screenshot.
    pub switch: Headless,
    maps: Mutex<Maps>,
    /// How many recordings are inside a dropping pass right now: a draw looks nothing up while 0.
    dropping: AtomicUsize,
}

#[derive(Debug, Default)]
struct Maps {
    /// Host tokens of image views over swapchain images.
    swapchain_views: HashSet<u64>,
    /// Latched recordings (host command-buffer token): whether each is inside a dropping pass.
    latched: HashMap<u64, bool>,
    /// The last real frame's GPU time, in timestamp ticks.
    gpu_ticks: Option<u64>,
}

impl VulkanHeadless {
    /// A view over a swapchain image was created.
    pub(super) fn swapchain_view(&self, view: u64) {
        self.maps.lock().swapchain_views.insert(view);
    }

    /// A view was destroyed.
    pub(super) fn view_destroyed(&self, view: u64) {
        self.maps.lock().swapchain_views.remove(&view);
    }

    /// A framebuffer was created over `views`: a swapchain one is always per-frame.
    pub(super) fn framebuffer_created(&self, framebuffer: u64, views: &[u64]) {
        let swapchain = {
            let maps = self.maps.lock();
            views.iter().any(|view| maps.swapchain_views.contains(view))
        };
        if swapchain {
            self.switch.mark_swapchain_target(framebuffer);
        }
    }

    /// A framebuffer was destroyed.
    pub(super) fn framebuffer_destroyed(&self, framebuffer: u64) {
        self.switch.forget_target(framebuffer);
    }

    /// `vkBeginCommandBuffer`: latch the recording's mode.
    pub(super) fn begin_recording(&self, buffer: u64) {
        let drops = self.switch.drops_now();
        let mut maps = self.maps.lock();
        let was_dropping = if drops {
            maps.latched.insert(buffer, false)
        } else {
            maps.latched.remove(&buffer)
        };
        if was_dropping == Some(true) {
            // A recording restarted inside a pass (reset without ending it).
            self.dropping.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// `vkCmdBeginRenderPass` onto `framebuffer`: whether this pass drops its draws.
    pub(super) fn begin_pass(&self, buffer: u64, framebuffer: u64) {
        let per_frame = self.switch.per_frame(framebuffer);
        self.switch.rendered(framebuffer);
        let mut maps = self.maps.lock();
        if let Some(inside) = maps.latched.get_mut(&buffer) {
            if per_frame && !*inside {
                *inside = true;
                self.dropping.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    /// `vkCmdEndRenderPass`.
    pub(super) fn end_pass(&self, buffer: u64) {
        if self.dropping.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut maps = self.maps.lock();
        if let Some(inside) = maps.latched.get_mut(&buffer) {
            if *inside {
                *inside = false;
                self.dropping.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// A draw recorded into `buffer`: whether it is dropped.
    pub(super) fn drops_draw(&self, buffer: u64) -> bool {
        let dropped = self.dropping.load(Ordering::Acquire) != 0
            && self.maps.lock().latched.get(&buffer).copied().unwrap_or(false);
        self.switch.count_draw(dropped);
        dropped
    }

    /// A command buffer was freed.
    pub(super) fn buffer_freed(&self, buffer: u64) {
        let mut maps = self.maps.lock();
        if maps.latched.remove(&buffer) == Some(true) {
            self.dropping.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// `vkGetQueryPoolResults` on a timestamp pool answered two 64-bit timestamps at `stride`
    /// apart in `data`: remember a real frame's difference, or answer a dropped one's.
    pub(super) fn gpu_timer(&self, data: &mut [u8], stride: usize) {
        if stride < 8 || data.len() < stride + 8 {
            return;
        }
        let first = u64::from_le_bytes(data[..8].try_into().expect("eight"));
        let second = u64::from_le_bytes(data[stride..stride + 8].try_into().expect("eight"));
        let mut maps = self.maps.lock();
        if self.switch.is_on() {
            if let Some(ticks) = maps.gpu_ticks {
                data[stride..stride + 8].copy_from_slice(&first.wrapping_add(ticks).to_le_bytes());
            }
        } else if second >= first {
            maps.gpu_ticks = Some(second - first);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropping_recording_drops_draws_only_inside_a_per_frame_pass() {
        let headless = VulkanHeadless::default();
        headless.swapchain_view(10);
        headless.framebuffer_created(100, &[10]); // the swapchain's
        headless.framebuffer_created(200, &[20]); // an offscreen target, rendered once
        headless.switch.set(true);
        headless.begin_recording(1);
        assert!(!headless.drops_draw(1), "outside a pass");
        headless.begin_pass(1, 200);
        assert!(!headless.drops_draw(1), "a target with no history is kept real");
        headless.end_pass(1);
        headless.begin_pass(1, 100);
        assert!(headless.drops_draw(1), "the swapchain's pass");
        headless.end_pass(1);
        assert!(!headless.drops_draw(1));

        headless.switch.set(false);
        headless.begin_pass(1, 100);
        assert!(headless.drops_draw(1), "latched at begin: the recording stays dropping");
        headless.end_pass(1);
        headless.begin_recording(1);
        headless.begin_pass(1, 100);
        assert!(!headless.drops_draw(1), "a recording begun after `off` is real");
    }

    #[test]
    fn the_gpu_timer_reads_the_last_real_frame_while_headless() {
        let headless = VulkanHeadless::default();
        let pair = |a: u64, b: u64| [a.to_le_bytes(), b.to_le_bytes()].concat();
        let mut real = pair(1000, 1700);
        headless.gpu_timer(&mut real, 8);
        assert_eq!(real, pair(1000, 1700), "a real frame is answered as it is");
        headless.switch.set(true);
        let mut idle = pair(5000, 5010);
        headless.gpu_timer(&mut idle, 8);
        assert_eq!(idle, pair(5000, 5700));
    }
}
