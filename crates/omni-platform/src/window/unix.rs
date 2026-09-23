//! Shared unix body of the window seam, used by the [`linux`](super::linux) and
//! [`macos`](super::macos) backends.
//!
//! # Status: structural, not implemented
//!
//! **Nothing in this module has ever been run.** [`Window::create`] returns
//! [`WindowError::Unsupported`] naming the platform API it intends to reach for, so a Linux or
//! macOS build fails at the first window rather than appearing to work.
//!
//! # `Window` here is an *uninhabited* type, and that is the whole trick
//!
//! [`crate::vm::unix`] and [`crate::process::unix`] have to write a fabricated `Unsupported`
//! return for every operation, because their seams are free functions and a free function has to
//! have a body. This seam is a *handle*, so there is a better answer available: `enum Window {}`
//! has no values, `create` is the only way to ask for one, and it refuses — therefore none of the
//! other six operations can ever be reached, and each of their bodies is `match *self {}`, which
//! the compiler accepts **because it has proved the argument cannot exist**.
//!
//! That is strictly better than a hand-written refusal in each one, and the difference is not
//! stylistic. A refusal body is a claim ("this was called and could not be served") that this
//! project's own VERIFICATION entry 12 is about: a branch no input can take is not a check, and
//! writing five of them here would be five statements that read as careful and that no test could
//! ever exercise. `match *self {}` makes the same statement as a *proof*, checked at every build
//! of every unix target, and it cannot rot — adding a sixth operation to the seam costs one more
//! line that also cannot be wrong.
//!
//! # What implementing this involves
//!
//! Not one backend but two, and they share almost nothing:
//!
//! * **Linux** is two windowing systems, not one. X11 (`xcb_create_window` +
//!   `VK_KHR_xcb_surface`) and Wayland (`wl_compositor_create_surface` + `xdg_toplevel` +
//!   `VK_KHR_wayland_surface`) are both live on real desktops, and the choice has to be made at
//!   *runtime* from `$WAYLAND_DISPLAY`/`$DISPLAY`, not at compile time. [`super::RawWindow`] grows
//!   an `Xlib`/`Xcb` variant and a `Wayland` variant, and it is `#[non_exhaustive]` so that
//!   adding them does not break `omni-gfx`'s match — it only obliges it to keep refusing what it
//!   has not implemented, by name.
//!   Resize on Wayland is also a different shape from Win32's: the compositor *asks* via
//!   `xdg_toplevel.configure` and the client acknowledges, so a "resize already happened" event
//!   is the wrong model there and this seam's [`WindowEvent::Resized`](super::WindowEvent::Resized)
//!   will need to be produced after the ack rather than before it.
//! * **macOS** is `NSWindow` plus a `CAMetalLayer`-backed view and `VK_EXT_metal_surface` through
//!   MoltenVK, and it carries a constraint this seam has no notion of yet: **AppKit requires the
//!   main thread**. `NSApplication` must be created and pumped on thread 0, which collides with
//!   this seam's "create it on the thread that will poll it" rule the moment the runtime wants to
//!   poll from a worker. Whoever implements macOS has to resolve that, and the resolution is a
//!   change to the *seam*, not to the backend — which is why it is written down here rather than
//!   discovered later.
//!
//! Both also have to answer a question Win32 answers for free: physical pixels. macOS reports
//! points and a `backingScaleFactor`; Wayland reports a surface scale. This module's contract is
//! physical pixels everywhere (see [`super`]), so both backends multiply, and both have to handle
//! the factor *changing* when a window moves between displays.

use super::{RawWindow, WindowDesc, WindowError, WindowResult};

/// The platform this backend was compiled for, for error messages.
fn platform() -> &'static str {
    std::env::consts::OS
}

/// The structural unix window: **a type with no values**.
///
/// See this module's header for why that is the implementation rather than a stand-in for one.
pub(super) enum Window {}

impl Window {
    /// The one reachable operation, and it refuses.
    ///
    /// The `intended` text names both Linux windowing systems rather than picking one, because
    /// picking one here would be this seam pre-deciding a question that belongs to the host it is
    /// running on — see the module header.
    pub(super) fn create(desc: &WindowDesc<'_>) -> WindowResult<Self> {
        // The description has already been validated by `super::validate`, so it is a *usable*
        // request that this target cannot serve, which is exactly what `Unsupported` means.
        let _ = desc;
        Err(WindowError::Unsupported {
            operation: "create",
            intended: "xcb_create_window(3) or wl_compositor_create_surface(3) on Linux, \
                       -[NSWindow initWithContentRect:styleMask:backing:defer:] on macOS",
            platform: platform(),
        })
    }

    /// Unreachable: `create` above never produces a `Window`, so the compiler discharges this.
    pub(super) fn show(&self) {
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn poll(&mut self, sink: &mut Vec<super::WindowEvent>) {
        let _ = sink;
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn client_size(&self) -> WindowResult<(u32, u32)> {
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn dpi(&self) -> WindowResult<u32> {
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn set_client_size(
        &self,
        width: u32,
        height: u32,
        operation: &'static str,
    ) -> WindowResult<()> {
        let _ = (width, height, operation);
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn set_minimized(&self, minimized: bool) -> WindowResult<()> {
        let _ = minimized;
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn request_close(&self) -> WindowResult<()> {
        match *self {}
    }

    /// Unreachable, as [`Window::show`].
    pub(super) fn raw(&self) -> RawWindow {
        match *self {}
    }
}
