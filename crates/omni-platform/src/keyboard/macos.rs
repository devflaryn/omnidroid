//! The keyboard seam on macOS: Text Input Sources and `UCKeyTranslate`.
//!
//! The current keyboard layout's `'uchr'` resource (`kTISPropertyUnicodeKeyLayoutData`) is the
//! layout itself, the same data the system types with; `UCKeyTranslate` reads it for one virtual
//! key (`kVK_*`) and one modifier state. Each key is read from a zero dead-key state, so a dead key
//! answers no characters and a nonzero state; Space translated from that state answers the accent's
//! spacing form, which is what [`KeyOutput::Dead`] carries.
//!
//! The keys are the window seam's [`KEYS`](crate::window::macos_keys::KEYS) whose `scancode` is not
//! 0 -- the keys a window delivers -- each named by its Linux code from the same table.
//!
//! Everything runs on the AppKit thread (`appkit_thread::on_main`): TIS is main-thread API. A
//! process with no AppKit thread and not on the main thread itself is refused rather than calling
//! TIS where it may trap.
use core::ffi::{c_char, c_void};

use super::{level, HostKey, HostLayout, KeyOutput, KeyboardError, KeyboardResult, ThirdLevel};
use crate::window::appkit_thread::{on_main, status, Status};
use crate::window::macos_keys::{linux_of_kvk, scancode_of, KEYS};

type CFTypeRef = *const c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef;
    fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> CFTypeRef;
    /// Not retained: owned by the source.
    fn TISGetInputSourceProperty(source: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    static kTISPropertyUnicodeKeyLayoutData: CFTypeRef;
    static kTISPropertyInputSourceID: CFTypeRef;
    static kTISPropertyLocalizedName: CFTypeRef;
    fn LMGetKbdType() -> u8;
    #[allow(clippy::too_many_arguments)]
    fn UCKeyTranslate(
        layout: *const c_void,
        virtual_key: u16,
        action: u16,
        modifiers: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_length: usize,
        actual_length: *mut usize,
        out: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFStringGetCString(string: CFTypeRef, buffer: *mut c_char, size: i64, encoding: u32) -> u8;
}

const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
const KEY_DOWN: u16 = 0; // kUCKeyActionDown
const KVK_SPACE: u16 = 0x31;

/// `UCKeyTranslate`'s modifier state is the classic `EventModifiers` shifted right by 8:
/// `shiftKey` (bit 9) is 0x02, `alphaLock` (bit 10) 0x04, `optionKey` (bit 11) 0x08.
const fn modifiers(set: usize) -> u32 {
    let mut m = 0;
    if set & level::SHIFT != 0 {
        m |= 0x02;
    }
    if set & level::CAPS != 0 {
        m |= 0x04;
    }
    if set & level::ALT != 0 {
        m |= 0x08;
    }
    m
}

pub(super) fn current_layout() -> KeyboardResult<HostLayout> {
    if objc2::MainThreadMarker::new().is_none() && status() != Status::Serving {
        return Err(KeyboardError::Os { api: "TISCopyCurrentKeyboardLayoutInputSource", detail: format!("needs the main thread, and {}", status().why()) });
    }
    on_main(|_| read())
}

/// On the main thread.
fn read() -> KeyboardResult<HostLayout> {
    // SAFETY: TIS on the main thread; each source is a +1 reference released below, and the
    // properties read from it are used before it is.
    unsafe {
        let sources: [unsafe extern "C" fn() -> CFTypeRef; 3] =
            [TISCopyCurrentKeyboardLayoutInputSource, TISCopyCurrentKeyboardInputSource, TISCopyCurrentASCIICapableKeyboardLayoutInputSource];
        for copy in sources {
            let source = copy();
            if source.is_null() {
                continue;
            }
            let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
            let out = (!data.is_null()).then(|| translate_all(CFDataGetBytePtr(data).cast(), source));
            CFRelease(source);
            if let Some(layout) = out {
                return layout;
            }
        }
    }
    Err(KeyboardError::Os { api: "TISGetInputSourceProperty", detail: "no current input source has keyboard layout data ('uchr')".into() })
}

/// Every deliverable key of the `'uchr'` layout at `layout`.
///
/// # Safety
/// `layout` is a `UCKeyboardLayout` kept alive by `source`, on the main thread.
unsafe fn translate_all(layout: *const c_void, source: CFTypeRef) -> KeyboardResult<HostLayout> {
    if layout.is_null() {
        return Err(KeyboardError::Os { api: "CFDataGetBytePtr", detail: "the layout data is empty".into() });
    }
    // SAFETY: plain C calls on the main thread with a live source.
    let (kbd_type, name, id) = unsafe { (u32::from(LMGetKbdType()), string(TISGetInputSourceProperty(source, kTISPropertyLocalizedName)), string(TISGetInputSourceProperty(source, kTISPropertyInputSourceID))) };
    let mut keys = Vec::new();
    for &(kvk, _) in KEYS {
        let Some(code) = linux_of_kvk(kvk).filter(|_| scancode_of(kvk) != 0) else { continue };
        // SAFETY: the caller's.
        let out: [KeyOutput; level::COUNT] = core::array::from_fn(|set| unsafe { translate(layout, kvk, modifiers(set), kbd_type) });
        keys.push(HostKey { code, out });
    }
    let name = match (name, id) {
        (Some(n), Some(i)) => format!("{n} ({i})"),
        (n, i) => n.or(i).unwrap_or_else(|| "unnamed".into()),
    };
    Ok(HostLayout { name, third_level: ThirdLevel::EitherAlt, keys })
}

/// What `kvk` types under `modifiers`, from a clean dead-key state.
///
/// # Safety
/// As [`translate_all`].
unsafe fn translate(layout: *const c_void, kvk: u16, modifiers: u32, kbd_type: u32) -> KeyOutput {
    let mut dead = 0u32;
    let mut buf = [0u16; 8];
    let mut len = 0usize;
    // SAFETY: the buffers are this frame's, their sizes passed.
    let status = unsafe { UCKeyTranslate(layout, kvk, KEY_DOWN, modifiers, kbd_type, 0, &mut dead, buf.len(), &mut len, buf.as_mut_ptr()) };
    if status != 0 {
        return KeyOutput::None;
    }
    if len == 0 && dead != 0 {
        // The accent's spacing form: Space typed after it.
        // SAFETY: as above.
        let status = unsafe { UCKeyTranslate(layout, KVK_SPACE, KEY_DOWN, 0, kbd_type, 0, &mut dead, buf.len(), &mut len, buf.as_mut_ptr()) };
        let spacing = String::from_utf16(&buf[..len.min(buf.len())]).ok().filter(|_| status == 0);
        return match spacing.as_deref().map(|s| (s.chars().next(), s.chars().count())) {
            Some((Some(c), 1)) => KeyOutput::Dead(c),
            _ => KeyOutput::None,
        };
    }
    match String::from_utf16(&buf[..len.min(buf.len())]) {
        Ok(s) if !s.is_empty() => KeyOutput::Text(s),
        _ => KeyOutput::None,
    }
}

/// A `CFStringRef`'s UTF-8, or `None` for a null or unconvertible one.
///
/// # Safety
/// `s` is null or a live `CFStringRef`.
unsafe fn string(s: CFTypeRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    // SAFETY: the buffer's size is passed.
    if unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as i64, UTF8) } == 0 {
        return None;
    }
    // SAFETY: `CFStringGetCString` wrote a NUL-terminated string into `buf`.
    Some(unsafe { core::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}
