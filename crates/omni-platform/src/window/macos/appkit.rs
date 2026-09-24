//! Everything that runs on the AppKit thread: the application, the event pump, the one view class
//! and the operations [`super::Window`] proxies here.
//!
//! **Nothing in this module is called from any other thread.** Every function takes a
//! [`MainThreadMarker`], which only [`super::main_thread`] hands out -- from the main thread's own
//! loop or from inside a `dispatch_sync_f` to the main queue.
//!
//! # One object is the view, the window's delegate and its text-input client
//!
//! `OmniView` is the window's content view (it owns the `CAMetalLayer`, receives the mouse and the
//! keyboard), its `NSWindowDelegate` (close, resize, minimise, focus, backing scale) and its
//! `NSTextInputClient` (typed text, through the input method). Every event source therefore
//! reaches the same instance variables -- the queue, the last reported size, the modifier and
//! capture state -- without any object having to find another.

use core::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSCursor, NSEvent,
    NSEventMask, NSEventModifierFlags, NSEventType, NSResponder, NSScreen, NSTextInputClient,
    NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSDate, NSDefaultRunLoopMode,
    NSNotification, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString, NSNotFound,
};
use objc2_quartz_core::{CALayer, CAMetalLayer};

use super::{keys, main_thread, Shared};
use crate::window::{PointerButton, RawWindow, WindowError, WindowEvent, WindowResult};

// ------------------------------------------------------------------------------ the application

/// Set once `NSApplication` exists; read by the main thread's loop to choose its pump.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Whether [`start_application`] has run.
pub(super) fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// Create the shared `NSApplication` as an ordinary (Dock-visible, activatable) application and
/// switch the main thread's loop to its event pump. Idempotent.
///
/// `Regular` rather than `Accessory` because a window that cannot become key cannot receive
/// keystrokes, and the window this seam makes is the application's whole interface.
pub(crate) fn start_application(mtm: MainThreadMarker) {
    if started() {
        return;
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    STARTED.store(true, Ordering::Release);
    main_thread::stop_run_loop();
}

/// One event: wait for it (serving the main dispatch queue meanwhile) and dispatch it.
///
/// **One correction to `-[NSApplication sendEvent:]`:** it does not deliver a key-up to the key
/// window while Command is held (a long-standing AppKit behaviour; winit and SDL work around it
/// the same way), which would leave the guest believing the key was never released. Those are sent
/// to their window directly.
pub(super) fn pump_one(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: `NSDefaultRunLoopMode` is a constant AppKit defines.
    let event = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantFuture()),
            NSDefaultRunLoopMode,
            true,
        )
    };
    let Some(event) = event else { return };
    if event.r#type() == NSEventType::KeyUp && event.modifierFlags().contains(NSEventModifierFlags::Command) {
        if let Some(window) = event.window(mtm) {
            window.sendEvent(&event);
            return;
        }
    }
    app.sendEvent(&event);
}

// ------------------------------------------------------------------------------ the registry

/// The objects a window is made of, owned here, on the thread allowed to touch them.
struct Native {
    window: Retained<NSWindow>,
    view: Retained<OmniView>,
}

thread_local! {
    /// Live windows by id. Only ever populated on the main thread.
    static WINDOWS: RefCell<HashMap<u64, Native>> = RefCell::new(HashMap::new());
    /// The next id.
    static NEXT: Cell<u64> = const { Cell::new(1) };
}

/// Run `work` with window `id`. The id is live for as long as its [`super::Window`] is, and that
/// is the only holder of it, so a missing id would be a bug in this file.
fn with<R>(id: u64, work: impl FnOnce(&Native) -> R) -> R {
    WINDOWS.with_borrow(|windows| {
        let native = windows.get(&id).expect("a window id is removed only by its own Window's drop");
        work(native)
    })
}

// ---------------------------------------------------------------------------------- the view

/// State the view's handlers share, all on the main thread.
struct ViewState {
    /// The last size reported as a [`WindowEvent::Resized`]; `(u32::MAX, u32::MAX)` until the
    /// first, so that the first always is.
    last_size: (u32, u32),
    /// Device-dependent modifier bits ([`keys::modifier_bit`]) reported down and not yet up.
    modifiers_down: u64,
    /// Sub-count remainders of captured relative motion, so that slow motion is not rounded away.
    motion_residual: (f64, f64),
    /// Sub-unit remainders of the wheel, for the same reason: a trackpad reports fractions.
    wheel_residual: (f64, f64),
    /// The IME's uncommitted text, if it is composing.
    marked: Option<String>,
    /// The installed tracking area, replaced on every `updateTrackingAreas`.
    tracking: Option<Retained<NSTrackingArea>>,
}

/// The instance variables of [`OmniView`].
struct ViewIvars {
    shared: Arc<Shared>,
    state: RefCell<ViewState>,
}

define_class!(
    // SAFETY: `NSView` has no subclassing requirements beyond overriding with matching
    // signatures, and `OmniView` does not implement `Drop`.
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ViewIvars]
    struct OmniView;

    impl OmniView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// The click that activates the window is delivered too, as Win32 delivers it.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// The layer's contents are the swapchain's; AppKit is never asked to draw them.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        /// A `CAMetalLayer` backs the view: what `VK_EXT_metal_surface` presents to.
        #[unsafe(method_id(makeBackingLayer))]
        fn make_backing_layer(&self) -> Retained<CALayer> {
            let layer = CAMetalLayer::new();
            Retained::into_super(layer)
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            self.sync_layer_scale();
            self.report_size();
        }

        /// A tracking area over the whole view, so that a move without a button held -- hover
        /// -- is reported while the pointer is over the client area, as `WM_MOUSEMOVE` is.
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(old) = state.tracking.take() {
                self.removeTrackingArea(&old);
            }
            let options = NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect;
            // SAFETY: `self` owns the area and outlives it; no user info.
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.bounds(),
                    options,
                    Some(self),
                    None,
                )
            };
            self.addTrackingArea(&area);
            state.tracking = Some(area);
            drop(state);
            // SAFETY: `NSView` implements it; the signature is `- (void)updateTrackingAreas`.
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.button(event, PointerButton::Primary, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.button(event, PointerButton::Primary, false);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.button(event, PointerButton::Secondary, true);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.button(event, PointerButton::Secondary, false);
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            if let Some(button) = other_button(event.buttonNumber()) {
                self.button(event, button, true);
            }
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            if let Some(button) = other_button(event.buttonNumber()) {
                self.button(event, button, false);
            }
        }
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.motion(event);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.motion(event);
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.motion(event);
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.motion(event);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.wheel(event);
        }

        /// The raw half first, then the translated half: `interpretKeyEvents:` hands the event
        /// to the input method, which answers through `insertText:replacementRange:`.
        ///
        /// No guard for Command here, and that is measured rather than forgotten: AppKit's input
        /// context inserts no text for a Command+key event (Command+A produces a `KeyDown` and no
        /// `Text`, `tests/window_macos.rs`), which is the analogue of Windows' `WM_SYSKEYDOWN`
        /// producing no `WM_CHAR`. A guard here was tried, and removing it changed nothing any
        /// test could see (VERIFICATION entry 12).
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let kvk = event.keyCode();
            self.push(WindowEvent::KeyDown {
                keycode: u32::from(kvk),
                scancode: keys::scancode_of(kvk),
                repeat: event.isARepeat(),
            });
            self.interpretKeyEvents(&NSArray::from_slice(&[event]));
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            let kvk = event.keyCode();
            self.push(WindowEvent::KeyUp { keycode: u32::from(kvk), scancode: keys::scancode_of(kvk) });
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            self.modifier(event.keyCode(), event.modifierFlags().0 as u64);
        }
    }

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for OmniView {}

    // SAFETY: each method has the signature the protocol declares.
    unsafe impl NSWindowDelegate for OmniView {
        /// The close button becomes a **request**; the window stays open (the seam's contract).
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.push(WindowEvent::CloseRequested);
            false
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            self.report_size();
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _notification: &NSNotification) {
            self.report_size();
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _notification: &NSNotification) {
            self.report_size();
        }

        #[unsafe(method(windowDidChangeBackingProperties:))]
        fn window_did_change_backing_properties(&self, _notification: &NSNotification) {
            self.sync_layer_scale();
            self.report_size();
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            self.push(WindowEvent::FocusChanged { focused: true });
        }

        /// Losing the focus ends a pointer capture, and says so, before the focus change -- the
        /// order the Windows backend reports `WM_KILLFOCUS` in.
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            if self.ivars().shared.captured() {
                end_capture(&self.ivars().shared);
                self.push(WindowEvent::PointerCaptureLost);
            }
            self.push(WindowEvent::FocusChanged { focused: false });
        }
    }

    // SAFETY: each method has the signature the protocol declares. The client is minimal and
    // honest about it: there is no document, so no ranges into one, and only the committed text
    // leaves it.
    unsafe impl NSTextInputClient for OmniView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _range: NSRange) {
            self.ivars().state.borrow_mut().marked = None;
            let text = plain_string(string);
            for character in text.chars().filter(|c| is_text(*c)) {
                self.push(WindowEvent::Text { text: character.to_string() });
            }
        }

        /// Swallowed: the key was already reported as a `KeyDown`, and letting it reach
        /// `NSResponder` would only beep.
        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, _selector: Sel) {}

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
            let text = plain_string(string);
            self.ivars().state.borrow_mut().marked = (!text.is_empty()).then_some(text);
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.ivars().state.borrow_mut().marked = None;
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            NSRange::new(NSNotFound as usize, 0)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            match &self.ivars().state.borrow().marked {
                Some(text) => NSRange::new(0, text.encode_utf16().count()),
                None => NSRange::new(NSNotFound as usize, 0),
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars().state.borrow().marked.is_some()
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(&self, _range: NSRange, _actual: *mut NSRange) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        /// Where the input method puts its candidate window: the client area's top-left corner,
        /// in screen points, since there is no caret to point at.
        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _range: NSRange, _actual: *mut NSRange) -> NSRect {
            let Some(window) = self.window() else { return NSRect::ZERO };
            let bounds = self.bounds();
            let top_left = NSRect::new(NSPoint::new(0.0, bounds.size.height), NSSize::new(0.0, 0.0));
            window.convertRectToScreen(self.convertRect_toView(top_left, None))
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> usize {
            NSNotFound as usize
        }
    }
);

/// `true` for a character a text box should receive: not a C0 control, not DEL (the seam's rule,
/// `WindowEvent::Text`), and not in Apple's function-key range.
///
/// **Which input reaches this.** From the keyboard through AppKit's own input context, none of
/// these arrive: keys that carry a control code are turned into commands (`doCommandBySelector:`)
/// or dropped before `insertText:` (MEASURED: Return, Escape, Tab, Control+Q and F13 type nothing,
/// `tests/window_macos.rs`). The filter is for the other callers of `insertText:replacementRange:`
/// -- an input method or the Character Viewer hands over whatever string it has, and the test that
/// pins this calls the method as they do.
///
/// Details of the rule: not a C0 control, not DEL (the seam's rule,
/// `WindowEvent::Text`), and not in `U+F700..=U+F8FF`, which `NSEvent.h` reserves for the function
/// keys ("Unicodes we reserve for function keys on the keyboard") -- keys pressed for their
/// effect, already reported as `KeyDown`.
fn is_text(character: char) -> bool {
    !(character < ' ' || character == '\u{7F}' || ('\u{F700}'..='\u{F8FF}').contains(&character))
}

/// The plain text of what `insertText:`/`setMarkedText:` was handed: an `NSString` or an
/// `NSAttributedString`, as `NSTextInputClient` documents.
fn plain_string(object: &AnyObject) -> String {
    if let Some(attributed) = object.downcast_ref::<NSAttributedString>() {
        return attributed.string().to_string();
    }
    if let Some(string) = object.downcast_ref::<NSString>() {
        return string.to_string();
    }
    String::new()
}

/// `otherMouseDown:`'s `buttonNumber`: 2 is the middle button, 3 and 4 the two side buttons
/// (`XBUTTON1`/`XBUTTON2` on Windows: Back, Forward). Anything past that has no seam button.
fn other_button(number: isize) -> Option<PointerButton> {
    match number {
        2 => Some(PointerButton::Middle),
        3 => Some(PointerButton::Back),
        4 => Some(PointerButton::Forward),
        _ => None,
    }
}

/// Take the whole-count part of `residual + delta`, keep the rest in `residual`.
fn take_whole(residual: &mut f64, delta: f64) -> i32 {
    let total = *residual + delta;
    let whole = total.trunc();
    *residual = total - whole;
    // `as` saturates; a delta past `i32` is a host bug, and the largest distance is the honest
    // answer to it (VERIFICATION entry 3: never a wrapped value).
    whole as i32
}

impl OmniView {
    fn new(mtm: MainThreadMarker, frame: NSRect, shared: Arc<Shared>) -> Retained<OmniView> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            shared,
            state: RefCell::new(ViewState {
                last_size: (u32::MAX, u32::MAX),
                modifiers_down: 0,
                motion_residual: (0.0, 0.0),
                wheel_residual: (0.0, 0.0),
                marked: None,
                tracking: None,
            }),
        });
        // SAFETY: `NSView`'s designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn push(&self, event: WindowEvent) {
        self.ivars().shared.push(event);
    }

    /// The window's backing scale factor: physical pixels per point.
    fn scale(&self) -> f64 {
        self.window().map_or(1.0, |window| window.backingScaleFactor())
    }

    /// The client area in physical pixels, `(0, 0)` while minimised (the seam's contract).
    fn pixel_size(&self) -> (u32, u32) {
        if self.window().is_some_and(|window| window.isMiniaturized()) {
            return (0, 0);
        }
        let backing = self.convertRectToBacking(self.bounds());
        (backing.size.width.round().max(0.0) as u32, backing.size.height.round().max(0.0) as u32)
    }

    /// Report the size if it changed since the last report.
    fn report_size(&self) {
        let size = self.pixel_size();
        let mut state = self.ivars().state.borrow_mut();
        if state.last_size != size {
            state.last_size = size;
            drop(state);
            self.push(WindowEvent::Resized { width: size.0, height: size.1 });
        }
    }

    /// Keep the layer's `contentsScale` equal to the window's backing scale: MoltenVK sizes a
    /// surface's `currentExtent` as the layer's bounds times its `contentsScale`, so a stale scale
    /// is a swapchain the wrong size for the window.
    fn sync_layer_scale(&self) {
        if let Some(layer) = self.layer() {
            layer.setContentsScale(self.scale());
        }
    }

    /// An event's position in client pixels, origin top-left. The view is not flipped, so its
    /// own coordinates run up from the bottom and are turned over here.
    fn pixel_position(&self, event: &NSEvent) -> (i32, i32) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let scale = self.scale();
        let height = self.bounds().size.height;
        ((point.x * scale).floor() as i32, ((height - point.y) * scale).floor() as i32)
    }

    fn button(&self, event: &NSEvent, button: PointerButton, down: bool) {
        let (x, y) = self.pixel_position(event);
        self.push(if down {
            WindowEvent::PointerDown { button, x, y }
        } else {
            WindowEvent::PointerUp { button, x, y }
        });
    }

    /// A move: the position, or -- while the pointer is captured -- the motion.
    ///
    /// Captured motion is `-[NSEvent deltaX]`/`deltaY`: points, **after** the system's pointer
    /// acceleration (the same figures `CGEventGetIntegerValueField(kCGMouseEventDeltaX)` gives),
    /// and positive down. That is not what the seam asks for -- Windows raw input is the device's
    /// own counts before acceleration -- and AppKit has no unaccelerated figure; getting one would
    /// mean an `IOHIDManager` reading the mouse itself. Recorded as a gap, not papered over.
    fn motion(&self, event: &NSEvent) {
        if self.ivars().shared.captured() {
            let mut state = self.ivars().state.borrow_mut();
            let dx = take_whole(&mut state.motion_residual.0, event.deltaX());
            let dy = take_whole(&mut state.motion_residual.1, event.deltaY());
            drop(state);
            if dx != 0 || dy != 0 {
                self.push(WindowEvent::PointerMotion { dx, dy });
            }
            return;
        }
        let (x, y) = self.pixel_position(event);
        self.push(WindowEvent::PointerMoved { x, y });
    }

    /// The wheel, in the seam's units: 120 per notch.
    ///
    /// `-[NSEvent deltaY]` is AppKit's **line** delta for every device (the fixed-point line
    /// field of the underlying `CGEvent`): a notch of a wheel turned slowly is 1.0, the system's
    /// scroll acceleration makes a fast one more, and a trackpad reports fractions. One line is
    /// taken as one notch, 120, which is what a Windows wheel notch is. `scrollingDeltaY` is not
    /// used: for a precise device it is in points, a unit the seam does not have.
    ///
    /// **Signs are made physical.** The seam's `dy` is positive when the wheel rolls away from the
    /// user and `dx` when it tilts right. AppKit's deltas follow the user's scrolling-direction
    /// setting ("natural" scrolling inverts them, and says so in
    /// `isDirectionInvertedFromDevice`), and its horizontal axis is positive to the **left**, as
    /// SDL's Cocoa backend also reads it (`x = -deltaX`). Both are undone here.
    ///
    /// Fractions are carried between events rather than rounded away, and the axes are reported
    /// as separate events, vertical first, as Windows reports them.
    fn wheel(&self, event: &NSEvent) {
        let sign = if event.isDirectionInvertedFromDevice() { -1.0 } else { 1.0 };
        let (x, y) = self.pixel_position(event);
        let mut state = self.ivars().state.borrow_mut();
        let dy = take_whole(&mut state.wheel_residual.1, event.deltaY() * sign * 120.0);
        let dx = take_whole(&mut state.wheel_residual.0, -event.deltaX() * sign * 120.0);
        drop(state);
        if dy != 0 {
            self.push(WindowEvent::Wheel { x, y, dx: 0, dy });
        }
        if dx != 0 {
            self.push(WindowEvent::Wheel { x, y, dx, dy: 0 });
        }
    }

    /// `flagsChanged:`: which modifier key changed, and whether it is now down.
    ///
    /// * Shift, Control, Option and Command, each side: the device-dependent bit for that key
    ///   ([`keys::modifier_bit`]) says whether it is down. A change to a key's bit that matches
    ///   what was last reported is not reported again.
    /// * Caps Lock: its flag is the **lock's** state, and macOS sends one `flagsChanged:` per
    ///   press, so a press is reported as a down and an up together -- a keystroke, which is what
    ///   it is -- whichever way the lock went.
    /// * Fn: its flag is also set by the arrow and function keys, so only an event whose key *is*
    ///   Fn is believed.
    fn modifier(&self, kvk: u16, flags: u64) {
        let scancode = keys::scancode_of(kvk);
        let keycode = u32::from(kvk);
        let down = match (keys::modifier_bit(kvk), kvk) {
            (Some(bit), _) => {
                let down = flags & bit != 0;
                let mut state = self.ivars().state.borrow_mut();
                let was = state.modifiers_down & bit != 0;
                if was == down {
                    return;
                }
                state.modifiers_down ^= bit;
                down
            }
            (None, keys::KVK_CAPS_LOCK) => {
                self.push(WindowEvent::KeyDown { keycode, scancode, repeat: false });
                self.push(WindowEvent::KeyUp { keycode, scancode });
                return;
            }
            (None, keys::KVK_FUNCTION) => flags & keys::FLAG_FUNCTION != 0,
            (None, _) => return,
        };
        self.push(if down {
            WindowEvent::KeyDown { keycode, scancode, repeat: false }
        } else {
            WindowEvent::KeyUp { keycode, scancode }
        });
    }
}

// -------------------------------------------------------------------------- pointer capture

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// `CGAssociateMouseAndMouseCursorPosition(3)`: 0 freezes the cursor while mouse events keep
    /// coming. Returns a `CGError`, 0 on success.
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    /// `CGWarpMouseCursorPosition(3)`, in global display coordinates (origin top-left of the main
    /// display).
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
}

/// Give the cursor back: reconnect it to the mouse and show it. Clears the flag first, so that no
/// handler running after this reports motion.
fn end_capture(shared: &Shared) {
    if shared.set_captured(false) {
        // SAFETY: plain CoreGraphics call.
        unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
        NSCursor::unhide();
    }
}

// ----------------------------------------------------------------- the proxied operations

/// `-[NSWindow initWithContentRect:…]` and everything the seam needs of a new window. Returns the
/// id it is registered under and its raw handle.
///
/// The size is **measured, then corrected**, as the Windows backend does it: the content rect is
/// given in points from the main screen's scale, the window lands on whatever screen it lands on,
/// and its backing size is then read and fixed once if it differs.
pub(super) fn create(
    mtm: MainThreadMarker,
    title: &str,
    width: u32,
    height: u32,
    shared: Arc<Shared>,
) -> WindowResult<(u64, RawWindow)> {
    start_application(mtm);
    let scale = NSScreen::mainScreen(mtm).map_or(1.0, |screen| screen.backingScaleFactor());
    let content = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(f64::from(width) / scale, f64::from(height) / scale),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: the designated initializer, with a valid style and backing type.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the window is owned by `Native` and closed explicitly in `destroy`; it must not
    // also release itself on close.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(title));

    let view = OmniView::new(mtm, content, shared);
    view.setWantsLayer(true);
    let Some(layer) = view.layer() else {
        window.close();
        return Err(WindowError::AppKit {
            operation: "create",
            api: "-[NSView setWantsLayer:]",
            detail: "the view has no layer after asking for one, so there is no CAMetalLayer to \
                     present to"
                .to_owned(),
        });
    };
    window.setContentView(Some(&view));
    window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    window.makeFirstResponder(Some(&view));

    let (have_w, have_h) = view.pixel_size();
    if (have_w, have_h) != (width, height) {
        let scale = window.backingScaleFactor();
        window.setContentSize(NSSize::new(f64::from(width) / scale, f64::from(height) / scale));
    }
    window.center();
    view.sync_layer_scale();
    view.report_size();

    let raw = RawWindow::AppKit {
        ns_window: Retained::as_ptr(&window) as isize,
        ns_view: Retained::as_ptr(&view) as isize,
        ca_metal_layer: Retained::as_ptr(&layer) as isize,
    };
    let id = NEXT.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    WINDOWS.with_borrow_mut(|windows| windows.insert(id, Native { window, view }));
    Ok((id, raw))
}

/// Bring the application forward and the window to the front as the key window.
pub(super) fn show(mtm: MainThreadMarker, id: u64) {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    with(id, |native| native.window.makeKeyAndOrderFront(None));
}

/// The backing size, asked of AppKit now.
pub(super) fn client_size(id: u64) -> (u32, u32) {
    with(id, |native| native.view.pixel_size())
}

/// 96 × the backing scale factor: see `super::Window::dpi`.
pub(super) fn backing_scale(id: u64) -> f64 {
    with(id, |native| native.window.backingScaleFactor())
}

/// `-[NSWindow setContentSize:]` in points, from pixels at the window's own scale. A window that
/// already has the size is left alone, so that no resize is reported.
pub(super) fn set_client_size(id: u64, width: u32, height: u32) {
    with(id, |native| {
        if native.view.pixel_size() == (width, height) {
            return;
        }
        let scale = native.window.backingScaleFactor();
        native.window.setContentSize(NSSize::new(f64::from(width) / scale, f64::from(height) / scale));
    });
}

/// `-[NSWindow miniaturize:]` / `deminiaturize:`.
pub(super) fn set_minimized(id: u64, minimized: bool) {
    with(id, |native| {
        if minimized {
            native.window.miniaturize(None);
        } else {
            native.window.deminiaturize(None);
        }
    });
}

/// `-[NSWindow performClose:]`: exactly what the close button does, so the request arrives through
/// `windowShouldClose:`, the same path a user's click takes.
pub(super) fn request_close(id: u64) {
    with(id, |native| native.window.performClose(None));
}

/// See `super::Window::set_pointer_capture`.
pub(super) fn set_pointer_capture(mtm: MainThreadMarker, id: u64, captured: bool) -> WindowResult<bool> {
    with(id, |native| {
        let shared = &native.view.ivars().shared;
        if !captured {
            end_capture(shared);
            return Ok(false);
        }
        if shared.captured() {
            return Ok(true);
        }
        let app = NSApplication::sharedApplication(mtm);
        if !app.isActive() || !native.window.isKeyWindow() || native.window.isMiniaturized() {
            return Ok(false);
        }
        // **Where the cursor is held: where it is, inside the client area** -- moved there first
        // when it is outside, so that a hidden cursor is not frozen over another window.
        let client = native.window.convertRectToScreen(native.view.convertRect_toView(native.view.bounds(), None));
        let at = NSEvent::mouseLocation();
        let held = NSPoint::new(
            at.x.clamp(client.origin.x, client.origin.x + client.size.width - 1.0),
            at.y.clamp(client.origin.y, client.origin.y + client.size.height - 1.0),
        );
        if held != at {
            // AppKit's screen space runs up from the bottom of the primary display; CoreGraphics'
            // runs down from its top.
            let primary = NSScreen::screens(mtm).firstObject().map_or(0.0, |screen| screen.frame().size.height);
            // SAFETY: plain CoreGraphics call.
            unsafe { CGWarpMouseCursorPosition(NSPoint::new(held.x, primary - held.y)) };
        }
        // SAFETY: plain CoreGraphics call.
        let error = unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
        if error != 0 {
            return Err(WindowError::AppKit {
                operation: "set_pointer_capture",
                api: "CGAssociateMouseAndMouseCursorPosition",
                detail: format!("CGError {error}"),
            });
        }
        NSCursor::hide();
        native.view.ivars().state.borrow_mut().motion_residual = (0.0, 0.0);
        shared.set_captured(true);
        Ok(true)
    })
}

/// Close and forget the window. A held capture is given back first: the cursor's association is
/// process-wide and would outlive the window.
pub(super) fn destroy(id: u64) {
    let native = WINDOWS
        .with_borrow_mut(|windows| windows.remove(&id))
        .expect("a window id is removed only here, by its own Window's drop, once");
    end_capture(&native.view.ivars().shared);
    native.window.setDelegate(None);
    native.window.orderOut(None);
    native.window.close();
}
