//! **Who owns a window's swapchain**, and the one refusal that answers it by name.
//!
//! # The problem this exists for
//!
//! There are two Vulkan stacks in an Omnidroid process and they point at the same window.
//!
//! * [`Renderer`](crate::vulkan::Renderer) is the host-side renderer D8 built first: its own
//!   `ash::Entry`, instance, device, surface and swapchain, presenting frames of its own.
//! * The **guest** has an instance, a device and a surface of its own, created through
//!   `omni_android::vulkan` — and its surface is over the *same* `HWND`, because
//!   `ndk::HostWindowSource` publishes one window and `vkCreateAndroidSurfaceKHR` resolves the
//!   guest's `ANativeWindow *` to it.
//!
//! Two `VkSurfaceKHR` objects over one window is legal. Two **swapchains** is not: the Vulkan
//! specification permits a native window to be associated with at most one swapchain at a time,
//! and an implementation that notices is *allowed* to report `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`
//! rather than required to.
//!
//! # Why a registry here rather than letting the driver say no
//!
//! Because on this machine the driver does not reliably say anything. There are **no validation
//! layers installed** (`docs/research/graphics-spike.md` §6), and the spike's direct evidence for
//! what that costs is a swapchain lifetime error that produced **zero** diagnostic output and
//! crashed `nvoglv64.dll` with `0xc0000409`, diagnosable only from the Windows Event Log. A
//! conflict that reaches the driver is therefore not a `VkResult` a caller can branch on; it is a
//! process that dies somewhere else.
//!
//! So the rule is enforced above the driver, by the only participant that can see both stacks, and
//! a conflict is a refusal **naming the other owner**: "`omni_gfx::Renderer` already owns a
//! swapchain on this window". That is a sentence an embedding can act on. A
//! `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR` would send the engine looking at its own surface code, and
//! the real fault is one level up — an embedding started a host renderer on a window it then gave
//! to the guest.
//!
//! # What a claim is and is not
//!
//! It is **not** a lock: nothing waits, and there is no ordering to get wrong. It is a table of
//! `window -> owner`, with a guard that removes its own entry when dropped. The whole of its
//! concurrency story is that the table is behind one mutex and no other lock in this crate is ever
//! taken while it is held.
//!
//! It is also **process-wide**, which is the one thing about it that deserves a second look. A
//! per-`GfxVulkanHost` table would not do, because the conflict this exists to catch is between a
//! `GfxVulkanHost` and a [`Renderer`](crate::vulkan::Renderer) — two objects that share nothing.
//! The window handle is a process-wide identity already, so the table keyed by it is too.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

/// A window, as the identity a swapchain is exclusive over.
///
/// **A number and not a [`RawWindow`](omni_platform::window::RawWindow)**, deliberately: this
/// module would otherwise have to `match` on a `#[non_exhaustive]` enum, and the wildcard arm
/// would be a branch no test reaches (VERIFICATION entry 12). Both callers have already matched
/// the window system in order to *make* a surface for it — `create_surface` and
/// `GfxVulkanHost::create_platform_surface` each refuse a system they have no code for, naming it
/// — so by the time a key is wanted the OS handle is in hand and the constructor for it is
/// unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowKey(u64);

impl WindowKey {
    /// The key for a Win32 window, which is its `HWND`.
    ///
    /// An `HWND` is unique among live windows in a session, which is exactly the property needed:
    /// two swapchains conflict if and only if they are over the same one. It is reused after a
    /// window is destroyed, which is not a problem here because a claim cannot outlive the
    /// swapchain that holds it and a swapchain cannot outlive its window.
    #[must_use]
    pub const fn win32(hwnd: isize) -> WindowKey {
        WindowKey(hwnd as u64)
    }

    /// The key as the number it is, for a diagnostic.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The owner a window is already claimed by.
///
/// Carries the owner's name rather than a bare "in use", because the two possible owners call for
/// completely different actions: `omni_gfx::Renderer` means an embedding started a host renderer
/// it should not have, and the guest's own `vkCreateSwapchainKHR` means the engine created a
/// second swapchain without destroying or retiring the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowClaimed {
    /// The window whose claim was refused.
    pub window: WindowKey,
    /// Who holds it, as [`claim_window`]'s caller named itself.
    pub owner: &'static str,
}

/// An exclusive claim on one window's swapchain, released when dropped.
///
/// Holding one is what entitles its holder to have a swapchain on that window. Dropping it —
/// which happens when a [`Renderer`](crate::vulkan::Renderer) is dropped, or when the guest calls
/// `vkDestroySwapchainKHR` — releases the window for the next claimant.
#[derive(Debug)]
pub struct WindowClaim {
    window: WindowKey,
    owner: &'static str,
}

impl WindowClaim {
    /// Which window this claim is over.
    #[must_use]
    pub const fn window(&self) -> WindowKey {
        self.window
    }

    /// Who this claim names as its owner.
    #[must_use]
    pub const fn owner(&self) -> &'static str {
        self.owner
    }
}

impl Drop for WindowClaim {
    fn drop(&mut self) {
        let mut table = claims().lock().unwrap_or_else(PoisonError::into_inner);
        // **Conditional, and the condition is not defensive.** A claim is `move`d when a guest
        // recreates its swapchain with `oldSwapchain`, so the entry belongs to whichever
        // `WindowClaim` value currently exists — and `move` in Rust does not run `Drop`, so this
        // runs once per claim. The check is what makes a future refactor that *clones* the owner
        // name fail loudly instead of removing someone else's entry.
        if table.get(&self.window).copied() == Some(self.owner) {
            table.remove(&self.window);
        }
    }
}

/// Take the exclusive swapchain claim on `window`, or say who has it.
///
/// `owner` is a `&'static str` naming the claimant as a reader should see it — this crate passes
/// `"omni_gfx::Renderer"` and `"the guest's vkCreateSwapchainKHR"`. It is not an identity: two
/// claimants with the same name are still two claimants and the second is refused.
///
/// # Errors
///
/// [`WindowClaimed`] naming the current owner, when one is already held.
pub fn claim_window(window: WindowKey, owner: &'static str) -> Result<WindowClaim, WindowClaimed> {
    let mut table = claims().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(&held) = table.get(&window) {
        return Err(WindowClaimed { window, owner: held });
    }
    table.insert(window, owner);
    Ok(WindowClaim { window, owner })
}

/// Who holds the claim on `window`, if anybody. Diagnostic (Global Constraint 6).
#[must_use]
pub fn claimed_by(window: WindowKey) -> Option<&'static str> {
    claims().lock().unwrap_or_else(PoisonError::into_inner).get(&window).copied()
}

/// How many windows are claimed. Diagnostic; a test asserts it returns to zero.
#[must_use]
pub fn claims_held() -> usize {
    claims().lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// The table, created on first use.
///
/// A `OnceLock` rather than a `lazy_static`-shaped dependency, and recovering from a poisoned
/// mutex rather than propagating it: a panic while this lock was held would be a host bug in this
/// file, and the response to it must not be that every later `vkCreateSwapchainKHR` refuses for a
/// reason unrelated to what anybody did. The data behind it is a `HashMap` of `Copy` values with
/// no invariant a panic could break halfway.
fn claims() -> &'static Mutex<HashMap<WindowKey, &'static str>> {
    static CLAIMS: OnceLock<Mutex<HashMap<WindowKey, &'static str>>> = OnceLock::new();
    CLAIMS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key nothing else in the test suite uses, so these tests do not collide with a live
    /// renderer's claim if both are running.
    const A: WindowKey = WindowKey::win32(0x7E57_0001);
    const B: WindowKey = WindowKey::win32(0x7E57_0002);

    /// **The second claimant is refused and is told who holds it**, which is the whole point:
    /// "in use" would leave an embedding unable to say which of its two Vulkan stacks to change.
    #[test]
    fn a_second_claim_on_one_window_is_refused_naming_the_first_owner() {
        let first = claim_window(A, "omni_gfx::Renderer").expect("the window is free");
        assert_eq!(first.window(), A);
        assert_eq!(claimed_by(A), Some("omni_gfx::Renderer"));

        let refused = claim_window(A, "the guest's vkCreateSwapchainKHR")
            .expect_err("one window, one swapchain");
        assert_eq!(refused.owner, "omni_gfx::Renderer");
        assert_eq!(refused.window, A);
        assert_eq!(refused.window.raw(), 0x7E57_0001);

        // A *different* window is unaffected, which is what says the table is keyed rather than a
        // single global flag.
        let other = claim_window(B, "the guest's vkCreateSwapchainKHR").expect("a free window");
        assert_eq!(claimed_by(B), Some("the guest's vkCreateSwapchainKHR"));
        drop(other);
        assert_eq!(claimed_by(B), None);

        drop(first);
        assert_eq!(claimed_by(A), None, "dropping the claim frees the window");

        // And the window can now be claimed by the other stack, which is the case that matters:
        // an embedding that tears its renderer down and then runs the guest must work.
        let guest = claim_window(A, "the guest's vkCreateSwapchainKHR").expect("now free");
        assert_eq!(guest.owner(), "the guest's vkCreateSwapchainKHR");
        drop(guest);
        assert_eq!(claimed_by(A), None);
    }

    /// **A moved claim is released once**, which is what makes `oldSwapchain` recreation work.
    ///
    /// The guest recreates its swapchain by passing the outgoing one as `oldSwapchain`; the claim
    /// moves from the old entry to the new one rather than being released and retaken, because
    /// releasing it for even an instant would let the other stack in. `move` does not run `Drop`,
    /// so the entry survives the move and is removed exactly once — and [`WindowClaim::drop`]'s
    /// owner check is what would make a refactor that broke this fail loudly.
    #[test]
    fn a_claim_that_is_moved_is_still_released_exactly_once() {
        let key = WindowKey::win32(0x7E57_0003);
        let claim = claim_window(key, "the guest's vkCreateSwapchainKHR").expect("free");
        let moved = claim; // The `oldSwapchain` transfer, in miniature.
        assert_eq!(claimed_by(key), Some("the guest's vkCreateSwapchainKHR"), "still held");
        drop(moved);
        assert_eq!(claimed_by(key), None, "and released once");
    }
}
