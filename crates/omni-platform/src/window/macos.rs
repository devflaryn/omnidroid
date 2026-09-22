//! macOS backend for the window seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, where `Window` is an uninhabited type and `create` returns
//! [`WindowError::Unsupported`](super::WindowError::Unsupported) naming the API it intends to
//! reach for. See that module for what implementing it involves.
//!
//! macOS-specific notes for whoever implements it, and the first one is a change to the *seam*
//! rather than to this file:
//!
//! * **AppKit is main-thread-only.** `NSApplication` must be created, activated and pumped on
//!   thread 0. This seam's rule is "create the window on the thread that will poll it" (see
//!   [`super::Window`]'s thread-affinity note), and on macOS that thread is not a choice. If the
//!   runtime ever wants to poll from a worker — and on the ARM64 macOS host, which
//!   `ARCHITECTURE.md` §6 runs guest code *natively* on, it plausibly will — the seam needs a
//!   main-thread-affine variant, not this backend a workaround. Nothing about that is
//!   discoverable from the Windows implementation, which is why it is recorded before anyone
//!   starts.
//! * **Polling is `[NSApp nextEventMatchingMask:… untilDate:[NSDate distantPast] … dequeue:YES]`
//!   in a loop.** `distantPast` is what makes it non-blocking; `untilDate:nil` blocks, and the
//!   difference is one argument.
//! * **Vulkan is MoltenVK, not a driver.** `VK_EXT_metal_surface` over a `CAMetalLayer`-backed
//!   `NSView` (`VK_MVK_macos_surface` is deprecated). Two consequences for `omni-gfx` rather than
//!   for this file: the loader is `libMoltenVK.dylib` or a `libvulkan.1.dylib` ICD rather than
//!   `vulkan-1.dll`, and MoltenVK reports `deviceType` as a real GPU but is a *translation layer*
//!   — D8 records that Roblox refuses emulated Vulkan devices by string match
//!   (`Vulkan: Device %s is emulated, skipping`), and whether MoltenVK trips that check is an
//!   open question nobody has measured.
//! * **Points are not pixels.** `NSView.frame` is in points and `convertRectToBacking:` /
//!   `backingScaleFactor` give physical pixels, which is what [`super`]'s contract requires. The
//!   factor changes when the window is dragged between a Retina and a non-Retina display, and
//!   `viewDidChangeBackingProperties` is the notification that says so.
//! * **Closing.** `windowShouldClose:` returning `NO` is the equivalent of intercepting
//!   `WM_CLOSE`: it turns the red button into a *request*, which is this seam's contract.

pub(super) use super::unix::Window;
