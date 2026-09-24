"""macOS rows: the window workstream (window + audio seams). Pure data; see `__init__.py`.

Run on the macOS host with `python3 tools/mutate.py --only mac-win-`. The commands run the gated
live tests, so they need a desktop session and an audio device; they set their own gate variables.
`--release` rather than the harness's usual debug build: these rows are about AppKit/Core Audio
behaviour, not arithmetic that only one profile can see (VERIFICATION entry 3), and the live tests
were measured in release.
"""

_BACKEND = "crates/omni-platform/src/window/macos.rs"
_APPKIT = "crates/omni-platform/src/window/macos/appkit.rs"
_KEYS = "crates/omni-platform/src/window/macos/keys.rs"
_MAIN = "crates/omni-platform/src/window/macos/main_thread.rs"
_AUDIO = "crates/omni-platform/src/audio/macos.rs"

# The window seam, live: this backend's own tests, the key table against its consumer, and the
# seam's contract tests (all but the one that asserts a Win32 handle).
_WIN = ["env", "OMNI_GFX_WINDOW_TESTS=1", "cargo", "test", "-p", "omni-platform", "--release",
        "--no-fail-fast", "--test", "window_macos", "--test", "window_keys_macos",
        "--test", "window_live", "--", "--include-ignored", "--test-threads=1",
        "--skip", "the_raw_handle_is_a_live_win32_handle"]

# The audio seam, live, plus the library's unit tests (the key table's own checks among them).
# `process::` and `fs::` are the orchestrator's modules, structural on this base.
_AUDIO_CMD = ["env", "OMNI_AUDIO_LIVE_TESTS=1", "cargo", "test", "-p", "omni-platform", "--release",
              "--no-fail-fast", "--lib", "--test", "audio_live", "--", "--include-ignored",
              "--test-threads=1", "--skip", "process::", "--skip", "fs::"]

ROWS = [
    # ---- keys -----------------------------------------------------------------------------------
    ("mac-win-A1", "A", "kVK_ANSI_Q names the wrong physical key", _KEYS,
     """    (0x0C, 16),  // kVK_ANSI_Q -> KEY_Q""",
     """    (0x0C, 17),  // kVK_ANSI_Q -> KEY_Q""",
     _WIN),

    ("mac-win-A2", "A", "Num Lock inverted to the unextended code (which the consumer reads as Pause)", _KEYS,
     """        69 => Some(0xE045),""",
     """        69 => Some(0x45),""",
     _WIN),

    ("mac-win-A3", "A", "right Shift read from the left Shift's device bit", _KEYS,
     """        0x3C => Some(0x0000_0004), // kVK_RightShift: NX_DEVICERSHIFTKEYMASK""",
     """        0x3C => Some(0x0000_0002), // kVK_RightShift: NX_DEVICERSHIFTKEYMASK""",
     _WIN),

    # No row for Command+key: AppKit's input context inserts no text for it, so the guard that once
    # stood in `keyDown:` was unreachable (its row came back NOT CAUGHT) and was deleted
    # (VERIFICATION entry 12). The host fact stays asserted in the keys test.

    ("mac-win-A22", "A", "the pump leaves a Command key-up to NSApplication, which drops it", _APPKIT,
     """    if event.r#type() == NSEventType::KeyUp && event.modifierFlags().contains(NSEventModifierFlags::Command) {""",
     """    if false && event.r#type() == NSEventType::KeyUp && event.modifierFlags().contains(NSEventModifierFlags::Command) {""",
     _WIN),

    ("mac-win-A5", "A", "control characters passed on as text", _APPKIT,
     """    !(character < ' ' || character == '\\u{7F}' || ('\\u{F700}'..='\\u{F8FF}').contains(&character))""",
     """    !(character == '\\u{7F}' || ('\\u{F700}'..='\\u{F8FF}').contains(&character))""",
     _WIN),

    ("mac-win-A6", "A", "Caps Lock reported as a down with no up", _APPKIT,
     """            (None, keys::KVK_CAPS_LOCK) => {
                self.push(WindowEvent::KeyDown { keycode, scancode, repeat: false });
                self.push(WindowEvent::KeyUp { keycode, scancode });""",
     """            (None, keys::KVK_CAPS_LOCK) => {
                self.push(WindowEvent::KeyDown { keycode, scancode, repeat: false });""",
     _WIN),

    # ---- pointer, wheel ---------------------------------------------------------------------------
    ("mac-win-A7", "A", "pointer y measured from the bottom (AppKit's) instead of the top", _APPKIT,
     """((height - point.y) * scale).floor() as i32)""",
     """(point.y * scale).floor() as i32)""",
     _WIN),

    ("mac-win-A8", "A", "horizontal wheel left at AppKit's sign (positive is left)", _APPKIT,
     """-event.deltaX() * sign * 120.0""",
     """event.deltaX() * sign * 120.0""",
     _WIN),

    ("mac-win-A9", "A", "a wheel line reported as 1 rather than 120", _APPKIT,
     """event.deltaY() * sign * 120.0""",
     """event.deltaY() * sign""",
     _WIN),

    ("mac-win-A10", "A", "losing the focus leaves the pointer captured", _APPKIT,
     """            if self.ivars().shared.captured() {
                end_capture(&self.ivars().shared);
                self.push(WindowEvent::PointerCaptureLost);
            }""",
     """            if false {
                end_capture(&self.ivars().shared);
                self.push(WindowEvent::PointerCaptureLost);
            }""",
     _WIN),

    ("mac-win-B1", "B", "a capture granted to a window without the focus", _APPKIT,
     """        if !app.isActive() || !native.window.isKeyWindow() || native.window.isMiniaturized() {""",
     """        if native.window.isMiniaturized() {""",
     _WIN),

    # ---- window lifecycle -------------------------------------------------------------------------
    ("mac-win-A11", "A", "the close button closes the window instead of asking", _APPKIT,
     """            self.push(WindowEvent::CloseRequested);
            false""",
     """            self.push(WindowEvent::CloseRequested);
            true""",
     _WIN),

    ("mac-win-A12", "A", "a minimise is not reported", _APPKIT,
     """        fn window_did_miniaturize(&self, _notification: &NSNotification) {
            self.report_size();""",
     """        fn window_did_miniaturize(&self, _notification: &NSNotification) {""",
     _WIN),

    ("mac-win-A13", "A", "set_client_size in points (the pixels not divided by the scale)", _APPKIT,
     """        let scale = native.window.backingScaleFactor();
        native.window.setContentSize(NSSize::new(f64::from(width) / scale, f64::from(height) / scale));
    });""",
     """        native.window.setContentSize(NSSize::new(f64::from(width), f64::from(height)));
    });""",
     _WIN),

    ("mac-win-A14", "A", "dpi ignores the backing scale", _BACKEND,
     """        Ok((96.0 * scale).round() as u32)""",
     """        Ok((96.0 * scale.min(1.0)).round() as u32)""",
     _WIN),

    # ---- the main-thread hand-over ------------------------------------------------------------------
    ("mac-win-A15", "A", "initializers after the constructor never run", _MAIN,
     """    for initializer in later {""",
     """    for initializer in later.into_iter().take(0) {""",
     _WIN),

    ("mac-win-A16", "A", "an application bundle's main thread taken anyway", _MAIN,
     """    if in_app_bundle() {""",
     """    if false && in_app_bundle() {""",
     _WIN),

    ("mac-win-A17", "A", "a window attempted with no AppKit thread (would hang, not refuse)", _BACKEND,
     """        if status != Status::Serving {""",
     """        if status != Status::Serving && status != Status::AppBundle {""",
     _WIN),

    # ---- audio ------------------------------------------------------------------------------------
    ("mac-win-A18", "A", "the render callback never advances the read position", _AUDIO,
     """        self.consumed.store(start + available, Ordering::Release);""",
     """        self.consumed.store(start, Ordering::Release);""",
     _AUDIO_CMD),

    ("mac-win-A19", "A", "the render callback never signals a waiter", _AUDIO,
     """    unsafe { dispatch_semaphore_signal(ring.wake) };""",
     """    let _ = ring.wake;""",
     _AUDIO_CMD),

    ("mac-win-A20", "A", "stop leaves the device consuming", _AUDIO,
     """            os("stop", "AudioOutputUnitStop", unsafe { AudioOutputUnitStop(self.unit) })?;""",
     """            os("stop", "AudioOutputUnitStop", 0)?;""",
     _AUDIO_CMD),

    ("mac-win-A21", "A", "the input side at a rate other than the device's (a resampler appears)", _AUDIO,
     """            sample_rate: device_format.sample_rate,""",
     """            sample_rate: device_format.sample_rate / 2.0,""",
     _AUDIO_CMD),
]
