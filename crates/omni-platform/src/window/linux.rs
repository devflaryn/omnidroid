//! Linux backend for the window seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, where `Window` is an uninhabited type and `create` returns
//! [`WindowError::Unsupported`](super::WindowError::Unsupported) naming the API it intends to
//! reach for. See that module for what implementing it involves.
//!
//! Linux-specific notes for whoever implements it:
//!
//! * **Two windowing systems, chosen at runtime.** `$WAYLAND_DISPLAY` decides; `$DISPLAY` is the
//!   fallback, and XWayland means both can be set at once, with Wayland being the right answer
//!   when it is. Vulkan needs the matching instance extension — `VK_KHR_wayland_surface` or
//!   `VK_KHR_xcb_surface` — and `vkGetPhysicalDeviceXcbPresentationSupportKHR` /
//!   `…WaylandPresentationSupportKHR` are the per-queue-family presentation checks, the analogue
//!   of the Win32 one the Windows backend already calls.
//! * **Xlib or xcb, and it matters.** `VK_KHR_xlib_surface` and `VK_KHR_xcb_surface` are separate
//!   extensions with separate handle types, and a driver may expose one and not the other. xcb is
//!   the smaller dependency and the one whose event model is already a queue, which is what
//!   [`poll_events`](super::Window::poll_events) wants; Xlib's `XNextEvent` is a blocking call
//!   with a non-blocking sibling (`XPending`) that is easy to get subtly wrong.
//! * **`xcb_intern_atom` for close.** X11 has no `WM_CLOSE`: a window advertises
//!   `WM_DELETE_WINDOW` in `WM_PROTOCOLS` and the window manager sends a `ClientMessage`. A
//!   backend that does not intern that atom gets its connection killed instead of an event, which
//!   is the same failure Win32 would have if `WM_CLOSE` fell through to `DefWindowProcW` — this
//!   seam's contract is that closing is the runtime's decision, so getting this wrong breaks the
//!   contract rather than merely the window.
//! * **Scale.** Wayland's `wl_surface.set_buffer_scale` and X11's RandR DPI are the two places
//!   the physical-pixel contract in [`super`] has to be honoured, and on Wayland the scale can
//!   change while the window is open.

pub(super) use super::unix::Window;
