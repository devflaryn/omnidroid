"""lnx-win- / lnx-gfx-: the X11 window backend and the Vulkan surfaces on it.

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.

**Most of these need the port's two X servers running** -- `Xvfb :92` (no window manager) and
`Xvfb :93` with `xfwm4 --compositor=off` -- because what they defend is behaviour against a real
server: a key typed through XTEST, a grab another client cannot take, a window manager's close.
The pre-flight runs every command on the clean tree first, so a missing server fails the whole
table loudly rather than filing every row as caught (VERIFICATION entry 8).
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import HOME, with_env  # noqa: E402

LINUX_RS = "crates/omni-platform/src/window/linux.rs"
KEYMAP_RS = "crates/omni-platform/src/window/linux/keymap.rs"
DECODE_RS = "crates/omni-platform/src/window/linux/decode.rs"
VULKAN_RS = "crates/omni-gfx/src/vulkan.rs"

LIVE_ENV = {"OMNI_GFX_WINDOW_TESTS": "1", "DISPLAY": ":92"}
WM_ENV = {"OMNI_GFX_WINDOW_TESTS": "1", "DISPLAY": ":93"}
# omni-android builds dynarmic; point it at this worktree's own CMake tree, as `cargo-locked` does.
# The checkout's own dynarmic tree, the one `~/odb/cargo-locked` uses (`dyn-<checkout>`): CMake
# refuses a cache made from another checkout's source directory.
_CHECKOUT = os.path.basename(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
DYNARMIC_ENV = {"OMNIDROID_DYNARMIC_BUILD_DIR": os.path.join(HOME, "odb", f"dyn-{_CHECKOUT}")}

UNIT = ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast", "--lib", "window"]
LIVE = with_env(LIVE_ENV, ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast",
                           "--test", "window_linux", "--", "--ignored", "--test-threads=1"])
WM = with_env(WM_ENV, ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast",
                       "--test", "window_linux_wm", "--", "--ignored", "--test-threads=1"])
# The Windows file, on Linux: everything but its Win32-handle test, which is Windows' by design.
SHARED_LIVE = with_env(LIVE_ENV, ["cargo", "test", "-p", "omni-platform", "--release",
                                  "--no-fail-fast", "--test", "window_live", "--", "--ignored",
                                  "--test-threads=1", "--skip", "the_raw_handle_is_a_live_win32_handle"])
KEYS = with_env(DYNARMIC_ENV, ["cargo", "test", "-p", "omni-android", "--release",
                               "--no-fail-fast", "--test", "keys_linux"])
RENDER_WM = with_env(WM_ENV, ["cargo", "test", "-p", "omni-gfx", "--release", "--no-fail-fast",
                             "--test", "renderer_linux_wm", "--", "--ignored", "--test-threads=1"])
RENDER = with_env(LIVE_ENV, ["cargo", "test", "-p", "omni-gfx", "--release", "--no-fail-fast",
                             "--test", "renderer_linux", "--", "--ignored", "--test-threads=1"])

ROWS = [
    # --- the physical key --------------------------------------------------------------------
    # The scancode handed over as the Linux input code itself (X keycode - 8), unconverted. keys.rs
    # decodes the set-1 code, so right Ctrl (97) would read as 0x61 -- no key -- and Up (103) as
    # 0x67, nothing. The live key test names the set-1 codes from the PC keyboard's specification.
    ("lnx-win-A1", "A", "the scancode is the evdev code, not its set-1 code",
     KEYMAP_RS,
     """        Some(evdev) => scancode_from_evdev(evdev),""",
     """        Some(evdev) => evdev as u32,""",
     LIVE),
    # Num Lock and Pause not crossed: Num Lock handed over as plain 0x45, which keys.rs (following
    # Windows) reads as Pause. The round trip against keys.rs is what notices.
    ("lnx-win-A2", "A", "Num Lock reported as the plain 0x45, which keys.rs decodes as Pause",
     KEYMAP_RS,
     """        69 => 0xE045,""",
     """        69 => 0x45,""",
     KEYS),
    # Over-correct: the "same number on both sides" block widened to all of 1..=88, so 84 (no key)
    # and 85 (KEY_ZENKAKUHANKAKU, set-1 0x76) are claimed as 0x54 and 0x55.
    ("lnx-win-B1", "B", "input codes 84 and 85 claimed as set-1 codes they are not",
     KEYMAP_RS,
     """        1..=83 | 86..=88 => evdev as u32,""",
     """        1..=88 => evdev as u32,""",
     KEYS),
    # The keycode taken at the shifted level, so Shift+a is keysym `A` where Win32 says VK_A.
    ("lnx-win-A3", "A", "the keycode is the modified keysym rather than the key's first level",
     LINUX_RS,
     """            (self.libs.xlib.XkbKeycodeToKeysym)(self.display, key.keycode as u8, group, 0)""",
     """            (self.libs.xlib.XkbKeycodeToKeysym)(self.display, key.keycode as u8, group, (key.state & 1) as c_int)""",
     LIVE),
    # --- repeat, text ------------------------------------------------------------------------
    ("lnx-win-A4", "A", "an auto-repeat is reported as a fresh press",
     LINUX_RS,
     """            let repeat = self.is_key_down(arrived.keycode);""",
     """            let repeat = false;""",
     LIVE),
    # Text passing control characters: Enter, Backspace, Tab, Escape, Ctrl+a and Delete all come
    # back from Xutf8LookupString as control codes.
    ("lnx-win-A5", "A", "Text passes control characters",
     DECODE_RS,
     """    committed.chars().filter(|&c| !(c < ' ' || c == '\\u{7F}')).map(String::from)""",
     """    committed.chars().map(String::from)""",
     LIVE),
    ("lnx-win-B2", "B", "space dropped from Text along with the control codes",
     DECODE_RS,
     """    committed.chars().filter(|&c| !(c < ' ' || c == '\\u{7F}')).map(String::from)""",
     """    committed.chars().filter(|&c| !(c <= ' ' || c == '\\u{7F}')).map(String::from)""",
     UNIT),
    # Text only from events the input method did not filter -- reverted, the dead key's own
    # keysym and the composed character would both be looked up.
    ("lnx-win-A6", "A", "text looked up for key events the input method consumed",
     LINUX_RS,
     """        if filtered {
            return;
        }
        // SAFETY: the caller matched `KeyPress`, so `key` is the member.""",
     """        // SAFETY: the caller matched `KeyPress`, so `key` is the member.""",
     LIVE),
    # The raw key read after the input method, which zeroes the keycode of the key that completes
    # a compose sequence: the `e` of `^ e` would never be reported as pressed.
    ("lnx-win-A23", "A", "the key that completes a compose sequence is not reported as pressed",
     LINUX_RS,
     """            xlib::KeyPress => self.key_press(arrived, event, filtered),""",
     """            xlib::KeyPress => self.key_press(unsafe { event.key }, event, filtered),""",
     LIVE),
    # --- buttons and the wheel ---------------------------------------------------------------
    ("lnx-win-A7", "A", "the wheel's sign flipped (button 4 is towards the user)",
     DECODE_RS,
     """        4 => Button::Wheel(0, WHEEL_NOTCH),
        5 => Button::Wheel(0, -WHEEL_NOTCH),""",
     """        4 => Button::Wheel(0, -WHEEL_NOTCH),
        5 => Button::Wheel(0, WHEEL_NOTCH),""",
     LIVE),
    ("lnx-win-A8", "A", "the wheel's release reported as a second notch",
     LINUX_RS,
     """                    Some(Button::Wheel(dx, dy)) if kind == xlib::ButtonPress => {""",
     """                    Some(Button::Wheel(dx, dy)) => {""",
     LIVE),
    # Middle and secondary swapped: core button 2 is the middle one, 3 the secondary.
    ("lnx-win-A9", "A", "core buttons 2 and 3 swapped",
     DECODE_RS,
     """        2 => Button::Pointer(PointerButton::Middle),
        3 => Button::Pointer(PointerButton::Secondary),""",
     """        2 => Button::Pointer(PointerButton::Secondary),
        3 => Button::Pointer(PointerButton::Middle),""",
     LIVE),
    # --- pointer capture ---------------------------------------------------------------------
    ("lnx-win-A10", "A", "the capture is not released when the focus leaves",
     LINUX_RS,
     """            self.keys_down = [0; 4];
            if self.captured.is_some() {
                self.end_capture(true);
                push_event(&mut self.queue, WindowEvent::PointerCaptureLost);
            }""",
     """            self.keys_down = [0; 4];""",
     LIVE),
    ("lnx-win-A11", "A", "raw motion dropped while captured",
     LINUX_RS,
     """        if cookie.evtype == xinput2::XI_RawMotion && self.captured.is_some() {""",
     """        if cookie.evtype == xinput2::XI_RawMotion && self.captured.is_none() {""",
     LIVE),
    ("lnx-win-A12", "A", "absolute moves still reported while captured",
     LINUX_RS,
     """                if self.captured.is_none() {
                    // SAFETY: the type says `motion` is the member.""",
     """                if true {
                    // SAFETY: the type says `motion` is the member.""",
     LIVE),
    ("lnx-win-B3", "B", "absolute moves never reported, captured or not",
     LINUX_RS,
     """                if self.captured.is_none() {
                    // SAFETY: the type says `motion` is the member.""",
     """                if false {
                    // SAFETY: the type says `motion` is the member.""",
     LIVE),
    ("lnx-win-A13", "A", "a captured click reported where the hidden pointer drifted, not where it is held",
     LINUX_RS,
     """                let (x, y) = self.captured.unwrap_or((press.x, press.y));""",
     """                let (x, y) = (press.x, press.y);""",
     LIVE),
    ("lnx-win-A14", "A", "the pointer not put back where it was held when the capture ends",
     LINUX_RS,
     """        let _ = self.select_raw_motion(false);
        if warp {""",
     """        let _ = self.select_raw_motion(false);
        if false {""",
     LIVE),
    # The re-grab at every poll removed. Another client's failed XGrabPointer lifts the
    # confinement (MEASURED on Xvfb); the live test makes exactly that attempt before it checks.
    ("lnx-win-A22", "A", "a confinement another client lifted is not put back",
     LINUX_RS,
     """        if self.captured.is_some() {
            self.grab();
        }
        sink.append(&mut self.queue);""",
     """        sink.append(&mut self.queue);""",
     LIVE),
    ("lnx-win-A15", "A", "the capture granted without the focus",
     LINUX_RS,
     """        if !self.focused {
            return Ok(false);
        }
        if self.xi_opcode.is_none() {""",
     """        if self.xi_opcode.is_none() {""",
     LIVE),
    # --- focus ---------------------------------------------------------------------------------
    ("lnx-win-A16", "A", "a window manager's keyboard grab counted as a focus change",
     LINUX_RS,
     """    let mode_counts = mode == xlib::NotifyNormal || mode == xlib::NotifyWhileGrabbed;""",
     """    let mode_counts = true;""",
     UNIT),
    ("lnx-win-B4", "B", "a nonlinear focus move (another client's window) not counted",
     LINUX_RS,
     """        xlib::NotifyAncestor | xlib::NotifyVirtual | xlib::NotifyNonlinear | xlib::NotifyNonlinearVirtual
    );""",
     """        xlib::NotifyAncestor | xlib::NotifyVirtual
    );""",
     LIVE),
    ("lnx-win-A17", "A", "show() maps the window but never asks for the focus",
     LINUX_RS,
     """                if self.focus_on_map.take() {
                    self.request_focus();
                }""",
     """                let _ = self.focus_on_map.take();""",
     # Not `SHARED_LIVE`: `window_live.rs` calls `show` twice, and the second call finds the
     # window viewable and asks for the focus directly -- which is `show`'s other path and hides
     # this one (the first run of this row reported NOT CAUGHT for exactly that reason). This
     # file's tests show once and wait for the focus.
     LIVE),
    # --- close, size, dpi ----------------------------------------------------------------------
    # WM_DELETE_WINDOW not advertised: a manager would kill the connection instead of asking.
    ("lnx-win-A18", "A", "WM_DELETE_WINDOW is not advertised in WM_PROTOCOLS",
     LINUX_RS,
     """                (xl.XSetWMProtocols)(display, window, protocols.as_mut_ptr(), 1);""",
     """                let _ = &mut protocols;""",
     LIVE),
    ("lnx-win-B5", "B", "any WM_PROTOCOLS message is a close request",
     LINUX_RS,
     """                    && message.format == 32
                    && message.data.get_long(0) as xlib::Atom == self.atoms.wm_delete_window""",
     """                    && message.format == 32""",
     LIVE),
    ("lnx-win-A19", "A", "the initial size is not in the queue before the first poll",
     LINUX_RS,
     """        window.size = (width, height);
        window.report_size();""",
     """        window.size = (width, height);""",
     SHARED_LIVE),
    ("lnx-win-A20", "A", "an iconified window reported at its full size",
     LINUX_RS,
     """        let now = if self.iconic { (0, 0) } else { self.size };""",
     """        let now = self.size;""",
     WM),
    # client_size's iconic read live from the server, ahead of the event stream: it says 0x0
    # while the renderer, fed from events, has not heard (MEASURED as ndk_host_window's minimise
    # test failing). The WM test reads the server's WM_STATE on its own connection and asserts the
    # two agree, deterministically.
    ("lnx-win-A25", "A", "client_size says minimised before the event stream has",
     LINUX_RS,
     """        Ok(if self.iconic { (0, 0) } else { size })""",
     """        Ok(if self.read_iconic() { (0, 0) } else { size })""",
     WM),
    ("lnx-win-A21", "A", "Xft.dpi ignored; the physical screen DPI always",
     LINUX_RS,
     """        if let Some(dpi) = self.resources().as_deref().and_then(decode::xft_dpi) {""",
     """        if let Some(dpi) = None::<f64> {""",
     LIVE),
    # A minimise that does not cancel the activation `show` has queued for the window's first
    # MapNotify: the manager receives _NET_ACTIVE_WINDOW after WM_CHANGE_STATE and un-minimises
    # (MEASURED: focus lost, focus regained, no iconic state).
    ("lnx-win-A24", "A", "a minimise straight after show is undone by show's own activation",
     LINUX_RS,
     """        // straight after `XMapRaised` leaves `WM_STATE` iconic, 3 of 3 runs.)
        self.focus_on_map.set(false);""",
     """        // straight after `XMapRaised` leaves `WM_STATE` iconic, 3 of 3 runs.)""",
     RENDER_WM),
    # --- the surface ---------------------------------------------------------------------------
    # The instance given VK_KHR_xcb_surface for an Xlib window: vkCreateXlibSurfaceKHR is then a
    # function the instance never loaded.
    ("lnx-gfx-A1", "A", "the renderer's instance is created with the wrong surface extension",
     VULKAN_RS,
     """        RawWindow::Xlib { .. } => Ok((
            khr::xlib_surface::NAME,""",
     """        RawWindow::Xlib { .. } => Ok((
            khr::xcb_surface::NAME,""",
     RENDER),
    # The renderer trusting the X surface's currentExtent over the window's 0x0: an iconified X
    # window keeps its geometry and Mesa reports it, so the renderer presents to an unmapped window.
    ("lnx-gfx-A2", "A", "an iconified X window keeps a swapchain because its surface still states an extent",
     VULKAN_RS,
     """    matches!(window, RawWindow::Xlib { .. })""",
     """    let _ = window;
    false""",
     RENDER_WM),
]
