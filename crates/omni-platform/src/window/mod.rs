//! Desktop windowing and input: the platform seam.
//!
//! # What this module is
//!
//! A resizable desktop window, the handle a graphics backend needs to put a surface on it, and a
//! **non-blocking** drain of the input and lifecycle events that arrived since the last drain.
//! Nothing else in the workspace may call an OS windowing API (Global Constraint 4), so this is
//! the whole of it: `omni-gfx` gets a [`RawWindow`] and `omni-android`'s GameActivity layer gets
//! [`WindowEvent`]s.
//!
//! It is the same shape as [`vm`](crate::vm) and [`fs`](crate::fs) — a portable API, a Windows
//! backend, and a structural unix one — and the backend list is fixed by the compiler rather than
//! by convention: `mod.rs` calls exactly
//!
//! ```text
//! Window::create(&WindowDesc) -> WindowResult<Window>
//! Window::show(&self)
//! Window::poll(&mut self, sink: &mut Vec<WindowEvent>)
//! Window::client_size(&self) -> WindowResult<(u32, u32)>
//! Window::set_client_size(&self, width, height, operation) -> WindowResult<()>
//! Window::set_minimized(&self, minimized: bool) -> WindowResult<()>
//! Window::request_close(&self) -> WindowResult<()>
//! Window::set_pointer_capture(&mut self, captured: bool) -> WindowResult<bool>
//! Window::has_pointer_capture(&self) -> bool
//! Window::wait(&self, timeout: Duration) -> bool
//! Window::raw(&self) -> RawWindow
//! ```
//!
//! so a backend that is missing one, or whose signature has drifted, does not build for that
//! target.
//!
//! # Why polling, and why it must not block
//!
//! The runtime owns the frame loop. `libroblox.so` is driven by Omnidroid's own thread, which
//! must service the guest, present a frame, and *then* ask what the user did — a windowing API
//! that owns the loop and calls back (`winit`'s `ApplicationHandler`, which the graphics spike
//! used, or a bare `GetMessageW` pump) would invert that. [`Window::poll_events`] therefore
//! returns whatever is queued and returns immediately when nothing is, and the caller decides how
//! often to ask.
//!
//! **One thing this shape cannot do, stated rather than hidden.** While the user is dragging the
//! window's border, Win32 runs its own modal message loop inside `DefWindowProcW`, and
//! [`Window::poll_events`] does not get control until the drag ends. The events are not lost —
//! `WM_SIZE` is *sent* straight to the window procedure, so it queues normally — but no frame is
//! presented during the drag, and the whole burst arrives at once afterwards. That burst is why
//! consecutive [`WindowEvent::Resized`] events coalesce (see this module's `push_event`): a drag produces
//! hundreds of them, and the spike measured that swapchain recreation is the single most
//! dangerous operation in this stack (`docs/research/graphics-spike.md` §1 — an incorrect one
//! crashed the NVIDIA driver on *every* live resize), so doing it once for the final size rather
//! than hundreds of times for sizes nobody will see is worth the four lines. The standard fix for
//! the freeze itself — `WM_ENTERSIZEMOVE` plus a `SetTimer` and rendering from inside the window
//! procedure — is deliberately **not** done here, because it would mean this seam calling back
//! into the renderer, which is the inversion the paragraph above rejects.
//!
//! # Pixels, not points
//!
//! Every size and position in this module is in **physical pixels**, because that is the only
//! unit a swapchain can be sized in. On a display with a scale factor, a process that has not
//! declared DPI awareness is lied to by Win32: `GetClientRect` reports a *virtualised* size and
//! the desktop compositor stretches the result, so a swapchain built from that number is blurry
//! and every pixel-exact assertion about it is wrong. The Windows backend therefore declares
//! per-monitor DPI awareness once, before the first window exists; see
//! [`windows`](self) for why that failing is not fatal.
//!
//! # Keycodes are raw on purpose
//!
//! [`WindowEvent::KeyDown`] carries the host's own key number — the Win32 virtual-key code on
//! Windows — and no translation. The guest's scale is Android's `AKEYCODE_*`, and the mapping
//! between the two is a property of the *guest* contract, not of the host window: it belongs in
//! `omni-android`'s GameActivity input layer, where it can be checked against what the engine
//! actually reads. A mapping invented here would be a table this crate cannot test against
//! anything.
//!
//! It also carries the host's number for the **physical** key, raw for the same reason: the guest
//! is handed a Linux input code for the key's position (`KeyEvent.getScanCode()`), and a layout's
//! virtual-key code cannot be turned back into a position — the key that types `A` on AZERTY is
//! the one QWERTY calls `Q`.
//!
//! The one place the host's translation **is** wanted is typed text, and it arrives separately,
//! as [`WindowEvent::Text`]: what a key *types* depends on the layout, on dead keys and on an IME,
//! all of which the host has already resolved and none of which a table of key numbers could
//! reproduce.
//!
//! # The mouse, whole: hover, every button, the wheel, and a captured pointer
//!
//! The pointer is reported as a mouse reports it, not as a finger: every move whether or not a
//! button is held ([`WindowEvent::PointerMoved`]), all five buttons, and the wheel
//! ([`WindowEvent::Wheel`], in the host's own units). What a consumer makes of that -- a finger on
//! a touch screen, or a mouse -- is its decision, not this seam's.
//!
//! **Pointer capture** is the other half of a mouse, and the one a game needs for mouse-look: the
//! cursor disappears and stays where it is, and what arrives instead is the device's own motion,
//! **relative and unaccelerated** ([`WindowEvent::PointerMotion`]), which keeps coming at a screen
//! edge where a cursor would stop. [`Window::set_pointer_capture`] asks for it and gives it back;
//! it is only granted to a window with the keyboard focus, and **losing the focus ends it**, which
//! is reported ([`WindowEvent::PointerCaptureLost`]) so a consumer does not go on believing it
//! holds a capture nobody gave it. While it is held the absolute [`WindowEvent::PointerMoved`] is
//! not reported at all: the cursor is not moving, and a position that does not move is not motion.
//! This is the contract Android's `View.requestPointerCapture` offers an app, which is why it has
//! this shape.

use core::fmt;
use core::time::Duration;
use core::marker::PhantomData;

mod error;

pub use error::{WindowError, WindowResult};

// The backend modules are **private**, for the reason `vm::mod` records: a `pub mod windows` is a
// public surface no other crate can name without writing `#[cfg(target_os = "windows")]` itself,
// which Global Constraint 4 forbids everywhere but here.
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

// Linux's structural body. macOS has its own backend and does not compile this one.
#[cfg(all(unix, not(target_os = "macos")))]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;
/// The AppKit thread, for the web view seam's macOS backend (`webview::macos`).
#[cfg(target_os = "macos")]
pub(crate) use macos::appkit_thread;

/// The macOS backend's physical-key table, re-exported **only** so that
/// `tests/window_keys_macos.rs` can check it key by key against its one consumer,
/// `omni_android::jni::keys::evdev_code` (which this crate cannot depend on). Not API: nothing
/// outside this crate's tests may name it, and only this crate may write `cfg(target_os)` to.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub use macos::keys as macos_keys;

/// **Where the Vulkan loader may be found on this host, in the order to try it** -- for
/// `omni-gfx`, which loads Vulkan at run time and may not write `cfg(target_os)` to know where.
///
/// **Empty means "the platform default"**: `ash::Entry::load()`'s own search, unchanged. That is
/// the answer on Windows (`vulkan-1.dll`, which the Vulkan runtime installs where the loader's
/// search finds it) and on Linux (`libvulkan.so.1`). macOS is the host where the default is not
/// enough: MEASURED, `dlopen("libvulkan.dylib")` does not search Homebrew's `/opt/homebrew/lib`,
/// so a machine with the loader installed fails to find it. See the macOS backend's
/// `VULKAN_LOADER_CANDIDATES` for the list and the measurements behind each entry.
///
/// These are **candidates**, not claims: the caller tries them in order and reports every one it
/// tried when none loads.
#[must_use]
pub fn vulkan_loader_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        macos::VULKAN_LOADER_CANDIDATES
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[]
    }
}

/// The largest client extent this seam will accept in either axis.
///
/// `WM_SIZE` carries the new client size as the two 16-bit halves of its `LPARAM`, so a window
/// wider or taller than this could exist and could never report its own size. The limit is also
/// comfortably past anything presentable: this host's GPU measured `maxImageDimension2D = 32768`
/// (`docs/research/graphics-spike.md` §3).
pub const MAX_EXTENT: u32 = u16::MAX as u32;

/// A pointer button, named by role rather than by side.
///
/// **Not `Left`/`Right`.** Windows delivers `WM_LBUTTONDOWN` for whichever physical button the
/// user has configured as primary — a left-handed mouse swaps them at the OS level and the
/// message numbers do not change — so `Left` would be a name that is wrong for some users and
/// right for most, which is the worst kind. The guest never sees this enum anyway: Android
/// reports `AMOTION_EVENT_BUTTON_PRIMARY`, which is the same idea under the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
    /// The primary button, `WM_LBUTTONDOWN`/`WM_LBUTTONUP`. A tap, on a touch screen.
    Primary,
    /// The secondary button, `WM_RBUTTONDOWN`/`WM_RBUTTONUP`.
    Secondary,
    /// The middle button, `WM_MBUTTONDOWN`/`WM_MBUTTONUP`.
    Middle,
    /// The first extended button, `WM_XBUTTONDOWN` with `XBUTTON1`. "Back" on most mice.
    Back,
    /// The second extended button, `WM_XBUTTONDOWN` with `XBUTTON2`. "Forward" on most mice.
    Forward,
}

impl PointerButton {
    /// Every variant, in order. Exists so that invariants can be asserted over all of them.
    pub const ALL: [PointerButton; 5] = [
        PointerButton::Primary,
        PointerButton::Secondary,
        PointerButton::Middle,
        PointerButton::Back,
        PointerButton::Forward,
    ];
}

impl fmt::Display for PointerButton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            PointerButton::Primary => "primary",
            PointerButton::Secondary => "secondary",
            PointerButton::Middle => "middle",
            PointerButton::Back => "back",
            PointerButton::Forward => "forward",
        };
        f.write_str(name)
    }
}

/// Something the user or the window system did.
///
/// Positions are in physical pixels relative to the client area's top-left corner, and may be
/// **negative or past the client extent**: a pointer that leaves the window while a button is held
/// keeps reporting, because that is what a drag is, and clamping here would turn a drag that ran
/// off the edge into one that stopped at it.
///
/// **Not `Copy`**, because [`WindowEvent::Text`] owns its string. `Clone` is the spelling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowEvent {
    /// The client area's size in physical pixels changed.
    ///
    /// **Both axes are zero when the window is minimised**, which is a valid state and not an
    /// error. A renderer must skip the frame rather than build a zero-extent swapchain, which
    /// Vulkan rejects.
    Resized {
        /// New client width in physical pixels.
        width: u32,
        /// New client height in physical pixels.
        height: u32,
    },
    /// The user asked for the window to close — the title-bar button, `Alt+F4`, or
    /// [`Window::request_close`].
    ///
    /// **Nothing has closed yet.** The window is still alive and still presentable; this seam
    /// does not act on the request, because the runtime may need to shut the guest down in order
    /// first. Drop the [`Window`] when ready.
    CloseRequested,
    /// The pointer moved.
    PointerMoved {
        /// Client-relative x, in physical pixels.
        x: i32,
        /// Client-relative y, in physical pixels.
        y: i32,
    },
    /// A pointer button went down.
    PointerDown {
        /// Which button.
        button: PointerButton,
        /// Client-relative x, in physical pixels.
        x: i32,
        /// Client-relative y, in physical pixels.
        y: i32,
    },
    /// A pointer button came up.
    PointerUp {
        /// Which button.
        button: PointerButton,
        /// Client-relative x, in physical pixels.
        x: i32,
        /// Client-relative y, in physical pixels.
        y: i32,
    },
    /// A key went down.
    KeyDown {
        /// The host's own key number, untranslated — the Win32 virtual-key code on Windows. See
        /// this module's "Keycodes are raw on purpose".
        keycode: u32,
        /// The host's own number for the **physical** key, untranslated: on Windows the set-1
        /// make code from bits 16-23 of the message's `LPARAM`, with `0xE000` added when bit 24
        /// marks an extended (`E0`-prefixed) key — so left Ctrl is `0x1D` and right Ctrl
        /// `0xE01D`. Zero when the host did not say, which is what input injected with only a
        /// virtual-key code carries.
        scancode: u32,
        /// True when this is an auto-repeat rather than a fresh press. The guest's
        /// `AKEY_EVENT_ACTION_DOWN` carries a repeat count, so dropping this would lose
        /// information the guest has a field for.
        repeat: bool,
    },
    /// A key came up.
    KeyUp {
        /// The host's own key number, untranslated.
        keycode: u32,
        /// The physical key, as [`WindowEvent::KeyDown`] carries it.
        scancode: u32,
    },
    /// The user typed text: the character the host's keyboard layout — and its IME, when one is
    /// composing — made of the keys that were pressed.
    ///
    /// **This is the translated half of typing, and [`WindowEvent::KeyDown`] is the raw half.**
    /// `KeyDown` says which key went down and deliberately not what it means (see this module's
    /// "Keycodes are raw on purpose"). A text box needs the opposite, and it cannot be computed
    /// from key numbers outside the host: it depends on the layout (the key QWERTY calls `Q` types
    /// `a` on AZERTY), on dead keys (`^` then `e` is one `ê`), on AltGr, and on an IME's
    /// composition. On Windows this is `WM_CHAR`, which `TranslateMessage` derives from the key
    /// messages through the thread's active layout, and which `DefWindowProcW`'s default IME
    /// handling produces for a committed composition.
    ///
    /// **One press that types is two events, key first.** `TranslateMessage` posts the `WM_CHAR`
    /// while its key message is being pumped, and Win32 retrieves posted messages ahead of the next
    /// input message, so the `Text` lands after its own `KeyDown` and before the next key. A
    /// consumer feeding a text box takes the `Text`; one feeding key presses takes the `KeyDown`;
    /// none should act on both as the same keystroke.
    ///
    /// **Control characters are not text.** A code unit below `0x20`, or `0x7F`, produces no event:
    /// Backspace (`0x08`), Tab (`0x09`), Enter (`0x0D`), Escape (`0x1B`), Ctrl+letter
    /// (`0x01`-`0x1A`) and Ctrl+Backspace (`0x7F`) are keys pressed for their effect, and each has
    /// already arrived as a [`WindowEvent::KeyDown`]. Passing them on as characters as well would
    /// insert a stray control code into a text box *and* have the consumer act on the key.
    ///
    /// **A character outside the Basic Multilingual Plane is one event.** `WM_CHAR` carries one
    /// UTF-16 code unit, so an emoji arrives as two messages, a high surrogate and then a low one.
    /// The high half is held — per window, in the window's own state — until the low half arrives,
    /// and the pair becomes one `Text`. A surrogate without its partner is dropped: it is not a
    /// character and has no UTF-8 spelling.
    ///
    /// So `text` is exactly one character today. It is a `String` rather than a `char` so that a
    /// backend handed a committed string in one piece need not split it.
    Text {
        /// What was typed: never empty, valid UTF-8, and free of the control codes above.
        text: String,
    },
    /// The window gained or lost keyboard focus.
    ///
    /// GameActivity's `onWindowFocusChanged` is what this feeds, and the engine uses it to pause;
    /// an engine that never learns it lost focus keeps rendering behind another window.
    FocusChanged {
        /// True when the window now has focus.
        focused: bool,
    },
    /// The mouse wheel turned, or tilted: `WM_MOUSEWHEEL` and `WM_MOUSEHWHEEL` on Windows.
    ///
    /// **In the host's own units, untranslated**, for the reason key numbers are: on Windows one
    /// notch of a wheel is `WHEEL_DELTA`, 120, and a high-resolution wheel or a touchpad reports
    /// fractions of it. What a notch *is* to a guest is the guest contract's business.
    ///
    /// Signs are the host's: `dy` is positive when the wheel is rolled **away from the user**, and
    /// `dx` is positive when it is tilted **to the right**. One of the two is zero, because the
    /// host reports the two axes as separate messages.
    ///
    /// Never coalesced: two notches are two events, and summing them would be a decision about the
    /// guest's scroll that this seam has no business making.
    Wheel {
        /// Client-relative x of the pointer, in physical pixels.
        x: i32,
        /// Client-relative y of the pointer, in physical pixels.
        y: i32,
        /// Horizontal wheel movement, positive to the right, in host units (120 a notch).
        dx: i32,
        /// Vertical wheel movement, positive away from the user, in host units (120 a notch).
        dy: i32,
    },
    /// **Relative motion of the pointing device while the pointer is captured** (see
    /// [`Window::set_pointer_capture`]): the device's own counts, **not accelerated**, and not
    /// bounded by any screen edge. On Windows this is raw input (`WM_INPUT`).
    ///
    /// Only reported while the capture is held. A run of them is **summed** into one (see this
    /// module's `push_event`): unlike a position, each one is a distance, and keeping only the
    /// newest would lose all the others.
    PointerMotion {
        /// Rightward motion, in device counts.
        dx: i32,
        /// Downward motion, in device counts.
        dy: i32,
    },
    /// **The host ended a pointer capture this window held** -- on Windows, because the window
    /// lost the keyboard focus. The cursor is visible and free again, and
    /// [`Window::has_pointer_capture`] now answers `false`.
    ///
    /// Not reported for a capture the caller released itself: that caller already knows.
    PointerCaptureLost,
}

/// Append `event` to `queue`, collapsing a run of the events for which only the newest matters.
///
/// Two event kinds arrive far faster than a frame loop can consume them and carry no history:
///
/// * [`WindowEvent::Resized`] — a border drag produces hundreds, and every one but the last names
///   a size that will never be rendered. Acting on each means recreating a swapchain per
///   intermediate size, and swapchain recreation is the operation the graphics spike found
///   crashes drivers when it is wrong (`docs/research/graphics-spike.md` §1).
/// * [`WindowEvent::PointerMoved`] — a high-polling-rate mouse reports up to 1,000 times a second.
///   Win32 already coalesces these *in its own queue*; once drained into ours they would
///   accumulate again between polls, and only the newest position is a position.
///
/// Nothing else coalesces, and the distinction is the point: a button press, a key press, a typed
/// character and a close request are each individually meaningful, and collapsing a run of them
/// would lose input — two [`WindowEvent::Text`]s in a row are the user typing two characters.
/// Coalescing also only ever collapses an event with the one **immediately** before it, so a move
/// that happened before a click still sits before that click in the queue and the order the user
/// produced is preserved.
///
/// **[`WindowEvent::PointerMotion`] is summed, not replaced.** It is a distance, not a position: a
/// raw-input mouse reports up to 8,000 of them a second, and the newest of a run is one sample of
/// the motion while the sum is all of it. Saturating, because the counts are the device's and a run
/// long enough to overflow an `i32` is still better answered with the largest distance than a
/// wrapped one (VERIFICATION entry 3).
fn push_event(queue: &mut Vec<WindowEvent>, event: WindowEvent) {
    if let (Some(WindowEvent::PointerMotion { dx: sum_x, dy: sum_y }), WindowEvent::PointerMotion { dx, dy }) =
        (queue.last_mut(), &event)
    {
        *sum_x = sum_x.saturating_add(*dx);
        *sum_y = sum_y.saturating_add(*dy);
        return;
    }
    let collapses = matches!(
        (queue.last(), &event),
        (Some(WindowEvent::Resized { .. }), WindowEvent::Resized { .. })
            | (Some(WindowEvent::PointerMoved { .. }), WindowEvent::PointerMoved { .. })
    );
    if collapses {
        // `last()` matched, so the queue is non-empty.
        *queue.last_mut().expect("the match arm above observed a last element") = event;
    } else {
        queue.push(event);
    }
}

/// A native window handle, as plain integers.
///
/// # Why integers and not a handle type
///
/// This enum crosses the seam into `omni-gfx`, which may not name a Win32 type (Global Constraint
/// 4). Carrying `isize`s makes it ordinary data — `Send`, `Sync`, `Copy`, printable in a
/// diagnostic — and leaves the cast to the one crate that is allowed to know what it is casting
/// to. `ash`'s `vk::HWND` is itself `isize`, so on the Vulkan side the conversion is the identity.
///
/// **`raw-window-handle` was considered and rejected.** It is the crate the graphics spike used,
/// and adding it would mean a workspace dependency whose only job is to name pointers, whose major
/// version must then agree with whatever every graphics crate in the tree wants — a coordination
/// cost this seam does not need, since it has exactly one consumer and five variants to describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RawWindow {
    /// A Win32 window. `hwnd` is the `HWND` and `hinstance` the `HINSTANCE` it was created with —
    /// `VkWin32SurfaceCreateInfoKHR` wants both.
    Win32 {
        /// The `HWND`, as an integer.
        hwnd: isize,
        /// The `HINSTANCE` the window class was registered with, as an integer.
        hinstance: isize,
    },
    /// An AppKit window. `VkMetalSurfaceCreateInfoEXT` wants only the layer; the window and the
    /// view are carried for identity and diagnostics. All three are live for as long as the
    /// `Window` is, and belong to the AppKit (main) thread: `CAMetalLayer` is safe to present to
    /// from any thread, and nothing else here should be touched off it.
    AppKit {
        /// The `NSWindow *`, as an integer.
        ns_window: isize,
        /// The `NSView *` that is the window's content view, as an integer.
        ns_view: isize,
        /// The `CAMetalLayer *` backing that view, as an integer.
        ca_metal_layer: isize,
    },
}

impl RawWindow {
    /// A short name for the windowing system this handle belongs to, for diagnostics.
    ///
    /// Exists so that a renderer which does not know this variant can refuse **naming it** rather
    /// than printing a debug dump of two integers.
    #[must_use]
    pub const fn system_name(self) -> &'static str {
        match self {
            RawWindow::Win32 { .. } => "win32",
            RawWindow::AppKit { .. } => "appkit",
        }
    }
}

/// What a window should be when it is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowDesc<'a> {
    /// The title bar text.
    pub title: &'a str,
    /// Initial client width in physical pixels. This is the *drawable* area, not the outer window:
    /// the frame and title bar are added on top, because a caller sizing a swapchain cares about
    /// the former and nothing cares about the latter.
    pub width: u32,
    /// Initial client height in physical pixels.
    pub height: u32,
}

impl<'a> WindowDesc<'a> {
    /// A description with the given title and initial client size in physical pixels.
    #[must_use]
    pub const fn new(title: &'a str, width: u32, height: u32) -> Self {
        WindowDesc { title, width, height }
    }
}

/// Reject a description no backend could honour, before any backend is asked.
///
/// Argument validation is target-independent, so it happens **first**: a 0x0 request is refused
/// identically on all five targets, and only a request that could have worked reaches the
/// structural backends' [`WindowError::Unsupported`]. The alternative — letting each backend
/// validate — is how "not implemented on linux" comes to be the answer to "you asked for a
/// zero-pixel window", which is true and useless.
fn validate(desc: &WindowDesc<'_>, operation: &'static str) -> WindowResult<()> {
    if let Some(at) = desc.title.chars().position(|c| c == '\0') {
        return Err(WindowError::TitleHasInteriorNul { operation, at });
    }
    validate_extent(desc.width, desc.height, operation)
}

/// The size half of [`validate`], shared with [`Window::set_client_size`].
fn validate_extent(width: u32, height: u32, operation: &'static str) -> WindowResult<()> {
    let bad = |v: u32| v == 0 || v > MAX_EXTENT;
    if bad(width) || bad(height) {
        return Err(WindowError::SizeOutOfRange { operation, width, height, max: MAX_EXTENT });
    }
    Ok(())
}

/// A resizable desktop window.
///
/// # Thread affinity
///
/// A `Window` is **not** `Send` and not `Sync`, and that is a property of Win32 rather than a
/// conservative choice: window messages are delivered to the thread that created the window, so a
/// `Window` polled from another thread would simply never see an event. The compiler refusing the
/// move is better than an empty event queue nobody can explain. Create it on the thread that will
/// poll it.
///
/// # Lifetime
///
/// Dropping the window destroys it. [`WindowEvent::CloseRequested`] does **not** — see that
/// variant for why the runtime has to be the one that decides.
pub struct Window {
    inner: backend::Window,
    /// The buffer [`Window::poll_events`] hands out a [`Drain`](std::vec::Drain) over. Kept here
    /// rather than allocated per poll so that a 60 Hz loop does not allocate 60 times a second;
    /// after the first few frames it is at its high-water mark and never grows again.
    drained: Vec<WindowEvent>,
    /// Makes the type `!Send` and `!Sync`. See "Thread affinity".
    _affinity: PhantomData<*const ()>,
}

impl Window {
    /// Create a window. It is **not** visible yet; call [`Window::show`].
    ///
    /// Creation and showing are separate because a window that appears before its swapchain
    /// exists shows one frame of whatever was behind it. The renderer wants the handle first.
    ///
    /// # Errors
    ///
    /// [`WindowError::SizeOutOfRange`] or [`WindowError::TitleHasInteriorNul`] if the description
    /// is not one any backend could honour; [`WindowError::LastError`] if Win32 refused;
    /// [`WindowError::Unsupported`] on Linux and macOS, whose backends are structural.
    pub fn new(desc: &WindowDesc<'_>) -> WindowResult<Self> {
        validate(desc, "create")?;
        Ok(Window {
            inner: backend::Window::create(desc)?,
            drained: Vec::new(),
            _affinity: PhantomData,
        })
    }

    /// Make the window visible and bring it forward.
    ///
    /// Idempotent: showing an already-visible window does nothing.
    pub fn show(&self) {
        self.inner.show();
    }

    /// Drain everything that has happened since the last call. **Never blocks.**
    ///
    /// Returns immediately with an empty iterator when nothing has happened, which is the common
    /// case in a frame loop and must therefore be cheap: on Windows it costs one `PeekMessageW`
    /// that reports no message.
    ///
    /// The events come out in the order they occurred, except that consecutive
    /// [`Resized`](WindowEvent::Resized) and consecutive [`PointerMoved`](WindowEvent::PointerMoved)
    /// runs are collapsed to their newest member — see this module's `push_event` for why, and for
    /// why nothing else is.
    pub fn poll_events(&mut self) -> std::vec::Drain<'_, WindowEvent> {
        // Belt and braces: `Drain`'s own `Drop` clears the range even when the iterator is
        // abandoned part-way, but `mem::forget` on it would leave stale events to be handed out
        // twice. One `clear` per frame costs nothing and removes the question.
        self.drained.clear();
        self.inner.poll(&mut self.drained);
        self.drained.drain(..)
    }

    /// The current client-area size in physical pixels, asked of the OS rather than remembered.
    ///
    /// Returns `(0, 0)` for a minimised window. A caller sizing a swapchain must treat that as
    /// "skip this frame", not as an error — Vulkan rejects a zero extent, and the window will
    /// report a real size again when it is restored.
    ///
    /// This asks the OS every time instead of caching what the last
    /// [`Resized`](WindowEvent::Resized) said, and the graphics spike is why: it measured the
    /// surface extent on this host drifting **41 times across 5 seconds with the window
    /// untouched** (`docs/research/graphics-spike.md` §4, a Parsec virtual-display adapter
    /// renegotiating the desktop), so a remembered size is a size that can silently stop being
    /// true.
    ///
    /// # Errors
    ///
    /// [`WindowError::LastError`] if `GetClientRect` failed; [`WindowError::Unsupported`] on the
    /// structural backends.
    pub fn client_size(&self) -> WindowResult<(u32, u32)> {
        self.inner.client_size()
    }

    /// The dots per inch the host's display scaling gives this window: 96 at 100%, 144 at 150%.
    ///
    /// **The host fact a guest's display density comes from.** It is the scale the user chose for
    /// the monitor the window is on -- logical rather than physical, which is also what Android's
    /// `densityDpi` is -- and it follows the window between monitors, because the process is
    /// per-monitor DPI aware. MEASURED why it matters: with a display density of 0 the engine
    /// divides by it and sizes a render target from the infinity.
    ///
    /// # Errors
    ///
    /// [`WindowError::LastError`] if `GetDpiForWindow` answered 0; [`WindowError::Unsupported`] on
    /// the structural backends.
    pub fn dpi(&self) -> WindowResult<u32> {
        self.inner.dpi()
    }

    /// Resize the window so that its **client area** becomes `width` x `height` physical pixels.
    ///
    /// # Why this exists, given that the user resizes the window by dragging it
    ///
    /// Two reasons, and the second is the load-bearing one.
    ///
    /// An Android application asks for a surface of a particular size, and a runtime that wants to
    /// honour that has to be able to say so — GameActivity's `ANativeWindow_setBuffersGeometry` is
    /// the guest-side spelling of the same request.
    ///
    /// And it is the **only** way the resize path gets an automated test. A border drag cannot be
    /// simulated from a test, so without this method the one operation the graphics spike measured
    /// as dangerous — swapchain recreation, which crashed the NVIDIA driver on *every* live resize
    /// until the `oldSwapchain` lifetime was fixed (`docs/research/graphics-spike.md` §1) — would
    /// be covered only by a human dragging a window, which VERIFICATION entry 4 is the record of
    /// this project refusing to accept as evidence.
    ///
    /// The size is a **request**. Windows enforces a minimum tracking size of roughly 130 physical
    /// pixels of width, and a window larger than the display is clamped, so
    /// [`Window::client_size`] afterwards is what to believe rather than the arguments passed
    /// here. A [`WindowEvent::Resized`] naming what actually happened arrives from a later
    /// [`Window::poll_events`] — and none arrives at all if the size did not change.
    ///
    /// # Errors
    ///
    /// [`WindowError::SizeOutOfRange`] for an extent outside `1..=`[`MAX_EXTENT`];
    /// [`WindowError::LastError`] if Win32 refused; [`WindowError::Unsupported`] on the structural
    /// backends.
    pub fn set_client_size(&self, width: u32, height: u32) -> WindowResult<()> {
        validate_extent(width, height, "set_client_size")?;
        self.inner.set_client_size(width, height, "set_client_size")
    }

    /// Minimise the window, or restore it.
    ///
    /// # Why the seam has this at all
    ///
    /// A minimised window has a **zero-pixel** client area, and that is a state the whole graphics
    /// stack has to handle: Vulkan refuses a zero-extent swapchain, so the renderer must hold no
    /// swapchain at all while it lasts and rebuild when the window comes back. That is a real
    /// production path — a user clicks minimise — and without this method it is a path no test can
    /// enter, because minimising is otherwise something only a person can do. VERIFICATION entry
    /// 12 is this project's record of what untestable branches are worth.
    ///
    /// It is also what GameActivity's `onPause`/`onResume` pair will be driven from, so it is not
    /// a test hook that happens to be public.
    ///
    /// A [`WindowEvent::Resized`] with both axes zero arrives from a later
    /// [`Window::poll_events`], and a restore produces another naming the size it came back to.
    ///
    /// # Errors
    ///
    /// [`WindowError::Unsupported`] on the structural backends. The Windows backend cannot fail:
    /// `ShowWindow` reports the previous visibility rather than an error.
    pub fn set_minimized(&self, minimized: bool) -> WindowResult<()> {
        self.inner.set_minimized(minimized)
    }

    /// Ask the window to close, exactly as the title-bar button does.
    ///
    /// **Asks.** Nothing closes; a [`WindowEvent::CloseRequested`] arrives from a later
    /// [`Window::poll_events`], and the runtime decides. That makes a programmatic shutdown and a
    /// user-initiated one the same code path, which means the one that only ever runs in a test
    /// is the same one that runs in production.
    ///
    /// # Errors
    ///
    /// [`WindowError::LastError`] if the message could not be posted;
    /// [`WindowError::Unsupported`] on the structural backends.
    pub fn request_close(&self) -> WindowResult<()> {
        self.inner.request_close()
    }

    /// **Capture the pointer, or give it back** -- Android's `View.requestPointerCapture` and
    /// `releasePointerCapture`, which is the contract this has.
    ///
    /// While the capture is held the cursor is hidden and held where it was, and the device's own
    /// relative motion arrives as [`WindowEvent::PointerMotion`] in place of
    /// [`WindowEvent::PointerMoved`]; buttons and the wheel are reported as before. Giving it back
    /// shows the cursor where it was when the capture began.
    ///
    /// **Only a window with the keyboard focus is granted one**, and the answer says which
    /// happened: `Ok(true)` when the capture is now held, `Ok(false)` when a request was declined
    /// because the window does not have the focus -- which is not an error, because Android ignores
    /// such a request the same way and the caller asks again later. Asking for the state already in
    /// force does nothing. **Losing the focus ends a capture** and reports
    /// [`WindowEvent::PointerCaptureLost`].
    ///
    /// # Errors
    ///
    /// [`WindowError::LastError`] if the host refused a step of it (registering for raw input,
    /// confining the cursor); nothing is left half-captured. [`WindowError::Unsupported`] on the
    /// structural backends.
    pub fn set_pointer_capture(&mut self, captured: bool) -> WindowResult<bool> {
        self.inner.set_pointer_capture(captured)
    }

    /// Whether this window holds the pointer capture now.
    #[must_use]
    pub fn has_pointer_capture(&self) -> bool {
        self.inner.has_pointer_capture()
    }

    /// **Wait until something happens, or until `timeout` has passed**: `true` when there is input
    /// or another message for this window to drain, `false` on the timeout.
    ///
    /// The frame loop's alternative to a fixed sleep: a loop that sleeps 100 ms between polls
    /// hands every key and click to the guest up to 100 ms late, and one that polls without
    /// sleeping spins a core. Nothing is drained here -- [`Window::poll_events`] still does that --
    /// so a `true` is followed by a poll, and a message that turns out to produce no event (a
    /// repaint) costs one empty poll.
    ///
    /// It returns at once when events are already queued, including ones a message sent during
    /// the last poll produced.
    pub fn wait(&self, timeout: Duration) -> bool {
        self.inner.wait(timeout)
    }

    /// The native handle, for a graphics backend to build a surface on.
    ///
    /// Valid for as long as this `Window` is. A surface outliving its window is undefined
    /// behaviour in Vulkan, so the renderer must be torn down first.
    #[must_use]
    pub fn raw(&self) -> RawWindow {
        self.inner.raw()
    }
}

impl fmt::Debug for Window {
    /// Prints the handle and nothing else. The event buffer's contents are transient and its
    /// capacity is noise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Window").field("raw", &self.inner.raw()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The coalescing rule, asserted by membership rather than by length: a test that only
    /// checked `queue.len()` would pass for a queue that collapsed the wrong pair (VERIFICATION
    /// entry 1).
    #[test]
    fn consecutive_resizes_collapse_to_the_newest_and_keep_their_place() {
        let mut queue = Vec::new();
        push_event(&mut queue, WindowEvent::PointerDown {
            button: PointerButton::Primary,
            x: 1,
            y: 2,
        });
        for width in [100, 200, 300, 400] {
            push_event(&mut queue, WindowEvent::Resized { width, height: width / 2 });
        }
        push_event(&mut queue, WindowEvent::CloseRequested);

        assert_eq!(
            queue,
            vec![
                WindowEvent::PointerDown { button: PointerButton::Primary, x: 1, y: 2 },
                WindowEvent::Resized { width: 400, height: 200 },
                WindowEvent::CloseRequested,
            ],
            "only the newest of a resize run survives, and it keeps the run's position"
        );
    }

    #[test]
    fn consecutive_pointer_moves_collapse_to_the_newest() {
        let mut queue = Vec::new();
        for x in 0..50 {
            push_event(&mut queue, WindowEvent::PointerMoved { x, y: x * 2 });
        }
        assert_eq!(queue, vec![WindowEvent::PointerMoved { x: 49, y: 98 }]);
    }

    /// The other half of the rule, and the one that matters: input that carries history must not
    /// be collapsed. A double-click is two `PointerDown`s and nothing else distinguishes it from
    /// one.
    #[test]
    fn presses_keys_and_close_requests_never_collapse() {
        let mut queue = Vec::new();
        let repeated = [
            WindowEvent::PointerDown { button: PointerButton::Primary, x: 0, y: 0 },
            WindowEvent::PointerUp { button: PointerButton::Primary, x: 0, y: 0 },
            WindowEvent::KeyDown { keycode: 65, scancode: 0x1E, repeat: false },
            WindowEvent::KeyUp { keycode: 65, scancode: 0x1E },
            WindowEvent::CloseRequested,
            WindowEvent::FocusChanged { focused: true },
            WindowEvent::Text { text: "a".to_owned() },
        ];
        for event in &repeated {
            push_event(&mut queue, event.clone());
            push_event(&mut queue, event.clone());
        }
        assert_eq!(queue.len(), repeated.len() * 2, "queue was {queue:?}");
        for (pair, event) in queue.chunks_exact(2).zip(&repeated) {
            let both = [event.clone(), event.clone()];
            assert_eq!(pair, both, "{event:?} was collapsed and must not be");
        }
    }

    /// Typing is a sequence, and the queue must hand it over as one: every character kept, in the
    /// order typed, each after the key that typed it. Distinct characters, so that a queue which
    /// kept the right *number* of events but reordered or replaced them still fails.
    #[test]
    fn typed_text_is_never_collapsed_and_keeps_its_place_among_keys() {
        let text = |t: &str| WindowEvent::Text { text: t.to_owned() };
        let sequence = [
            WindowEvent::KeyDown { keycode: 0x48, scancode: 0x23, repeat: false },
            text("h"),
            WindowEvent::KeyUp { keycode: 0x48, scancode: 0x23 },
            text("é"),
            text("😀"),
            text("é"),
            WindowEvent::PointerMoved { x: 1, y: 1 },
            text("ç"),
            WindowEvent::PointerMoved { x: 2, y: 2 },
        ];
        let mut queue = Vec::new();
        for event in &sequence {
            push_event(&mut queue, event.clone());
        }
        assert_eq!(queue, sequence, "typed text was collapsed, dropped or moved");
    }

    /// **Relative motion is summed**, because each one is a distance: a run of five becomes their
    /// total, and one separated by anything else starts a new run. A wheel notch is never
    /// coalesced -- two notches are two events -- and neither is a lost capture.
    #[test]
    fn relative_motion_is_summed_and_wheel_notches_are_kept() {
        let mut queue = Vec::new();
        for (dx, dy) in [(3, -1), (4, 0), (-10, 7), (1, 1), (2, 2)] {
            push_event(&mut queue, WindowEvent::PointerMotion { dx, dy });
        }
        push_event(&mut queue, WindowEvent::PointerDown { button: PointerButton::Primary, x: 0, y: 0 });
        push_event(&mut queue, WindowEvent::PointerMotion { dx: 5, dy: 6 });
        let notch = WindowEvent::Wheel { x: 1, y: 2, dx: 0, dy: 120 };
        push_event(&mut queue, notch.clone());
        push_event(&mut queue, notch.clone());
        push_event(&mut queue, WindowEvent::PointerCaptureLost);
        push_event(&mut queue, WindowEvent::PointerCaptureLost);
        assert_eq!(
            queue,
            vec![
                WindowEvent::PointerMotion { dx: 0, dy: 9 },
                WindowEvent::PointerDown { button: PointerButton::Primary, x: 0, y: 0 },
                WindowEvent::PointerMotion { dx: 5, dy: 6 },
                notch.clone(),
                notch,
                WindowEvent::PointerCaptureLost,
                WindowEvent::PointerCaptureLost,
            ]
        );
        // Saturating, not wrapping: a sum that would overflow is the largest distance.
        let mut queue = vec![WindowEvent::PointerMotion { dx: i32::MAX - 1, dy: i32::MIN + 1 }];
        push_event(&mut queue, WindowEvent::PointerMotion { dx: 5, dy: -5 });
        assert_eq!(queue, vec![WindowEvent::PointerMotion { dx: i32::MAX, dy: i32::MIN }]);
    }

    /// A move separated from another move by anything at all is two moves. This is the property
    /// that keeps a drag's shape: press, move, release must not become press, release.
    #[test]
    fn a_move_split_by_another_event_does_not_collapse_across_it() {
        let mut queue = Vec::new();
        push_event(&mut queue, WindowEvent::PointerMoved { x: 1, y: 1 });
        push_event(&mut queue, WindowEvent::PointerDown {
            button: PointerButton::Secondary,
            x: 1,
            y: 1,
        });
        push_event(&mut queue, WindowEvent::PointerMoved { x: 9, y: 9 });
        assert_eq!(
            queue,
            vec![
                WindowEvent::PointerMoved { x: 1, y: 1 },
                WindowEvent::PointerDown { button: PointerButton::Secondary, x: 1, y: 1 },
                WindowEvent::PointerMoved { x: 9, y: 9 },
            ]
        );
    }
}
