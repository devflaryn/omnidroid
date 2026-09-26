//! **Which render targets are the frame's own**: the classifier headless mode drops by.
//!
//! A target (a Vulkan framebuffer, a GL framebuffer object) is **per-frame** when what is drawn
//! into it is drawn again for the next frame: the swapchain's own images, and anything rendered in
//! at least [`PER_FRAME_HITS`] of the last [`WINDOW`] frames -- the scene, its depth, shadow maps,
//! post-processing targets, double-buffered ones included. Dropping the draws into those costs
//! nothing that is not redrawn the frame after headless is turned off.
//!
//! Everything else is **kept real**: a texture rendered once or now and then (a composited avatar,
//! a cached GUI) is drawn into only when its content changes, and a draw dropped there would be a
//! stale texture for as long as nothing changes it -- after headless is off, too.

use std::collections::HashMap;

/// How many recent frames the rule looks at.
pub const WINDOW: u32 = 8;
/// In how many of them a target must have been rendered to be per-frame.
pub const PER_FRAME_HITS: u32 = 4;

#[derive(Debug, Clone, Copy, Default)]
struct Target {
    /// Always per-frame: a swapchain image is presented and drawn again every frame.
    always: bool,
    /// The last frame this target was rendered in.
    last: u64,
    /// Bit `i`: rendered in frame `last - i`.
    bits: u64,
}

/// Every target's recent history.
#[derive(Debug, Default)]
pub struct FrameHistory {
    targets: HashMap<u64, Target>,
}

impl FrameHistory {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `key` is a swapchain target: per-frame whatever its history.
    pub fn mark_always(&mut self, key: u64) {
        self.targets.entry(key).or_default().always = true;
    }

    /// `key` is gone (destroyed); a later target with its number starts with no history.
    pub fn forget(&mut self, key: u64) {
        self.targets.remove(&key);
    }

    /// `key` was rendered into during `frame`.
    pub fn rendered(&mut self, key: u64, frame: u64) {
        let target = self.targets.entry(key).or_default();
        if target.bits == 0 || frame >= target.last + 64 {
            target.bits = 1;
        } else if frame > target.last {
            target.bits = (target.bits << (frame - target.last)) | 1;
        } else {
            // A frame at or before the last one (a recording that began before the present that
            // ended its frame): the bit it would have set.
            let back = target.last - frame;
            if back < 64 {
                target.bits |= 1 << back;
            }
            return;
        }
        target.last = frame;
    }

    /// Whether `key` is per-frame as `frame` is being recorded: a swapchain target, or one
    /// rendered in at least [`PER_FRAME_HITS`] of the [`WINDOW`] frames before `frame`.
    #[must_use]
    pub fn per_frame(&self, key: u64, frame: u64) -> bool {
        let Some(target) = self.targets.get(&key) else {
            return false;
        };
        if target.always {
            return true;
        }
        if target.bits == 0 || frame > target.last + u64::from(WINDOW) {
            return false;
        }
        // Frames `frame - WINDOW ..= frame - 1`, as bits of `bits` (bit i is frame `last - i`).
        let mut hits = 0;
        for back in 1..=u64::from(WINDOW) {
            let Some(f) = frame.checked_sub(back) else { break };
            if f <= target.last && target.last - f < 64 && target.bits & (1 << (target.last - f)) != 0 {
                hits += 1;
            }
        }
        hits >= PER_FRAME_HITS
    }

    /// How many targets are known, and how many of those are per-frame at `frame`.
    #[must_use]
    pub fn census(&self, frame: u64) -> (usize, usize) {
        let per_frame = self.targets.keys().filter(|&&key| self.per_frame(key, frame)).count();
        (self.targets.len(), per_frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_drawn_every_frame_is_per_frame_and_one_drawn_once_is_not() {
        let mut history = FrameHistory::new();
        for frame in 0..20 {
            history.rendered(1, frame);
        }
        history.rendered(2, 5);
        assert!(history.per_frame(1, 20), "every frame");
        assert!(!history.per_frame(2, 20), "once, long ago");
        assert!(!history.per_frame(2, 6), "once, just now");
        assert!(!history.per_frame(3, 20), "never seen");
    }

    #[test]
    fn a_double_buffered_target_is_per_frame_and_a_rare_one_is_not() {
        let mut history = FrameHistory::new();
        for frame in (0..20).step_by(2) {
            history.rendered(1, frame); // every other frame
        }
        for frame in (0..20).step_by(5) {
            history.rendered(2, frame); // one frame in five
        }
        assert!(history.per_frame(1, 20), "asked in a frame it is rendered in");
        assert!(!history.per_frame(2, 20));
    }

    #[test]
    fn a_burst_ends_and_the_target_stops_being_per_frame() {
        let mut history = FrameHistory::new();
        for frame in 10..16 {
            history.rendered(1, frame);
        }
        assert!(history.per_frame(1, 16), "just after the burst");
        assert!(!history.per_frame(1, 16 + u64::from(WINDOW)), "a window later");
        assert!(!history.per_frame(1, 1000), "much later");
    }

    #[test]
    fn a_swapchain_target_is_always_per_frame_and_forgetting_clears_it() {
        let mut history = FrameHistory::new();
        history.mark_always(7);
        assert!(history.per_frame(7, 0));
        assert!(history.per_frame(7, 1_000_000));
        history.forget(7);
        assert!(!history.per_frame(7, 0));
    }

    #[test]
    fn a_late_recording_sets_the_bit_of_its_own_frame() {
        let mut history = FrameHistory::new();
        for frame in [10, 12, 11, 13] {
            history.rendered(1, frame);
        }
        assert!(history.per_frame(1, 14), "frames 10-13, one of them recorded late");
        assert_eq!(history.census(14), (1, 1));
    }
}
