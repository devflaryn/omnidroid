//! The keyboard seam on Windows: `ToUnicodeEx` over the foreground thread's keyboard layout.
//!
//! **Which layout.** A keyboard layout is per thread on Windows, and the thread this runs on has
//! no window: `GetKeyboardLayout(0)` would answer the layout it was started with, not the one the
//! person is typing with now. The foreground window's thread -- the console or terminal the session
//! was started from, when this runs at a session's start -- has that one; its own thread's is the
//! fallback when there is no foreground window.
//!
//! **Each key.** The set-1 scancodes the window seam reports ([`evdev_code`]: 0x01-0x58 and the
//! [`EXTENDED_SCANCODES`]), each turned into the layout's virtual key with `MapVirtualKeyExW(...,
//! MAPVK_VSC_TO_VK_EX)` and translated by `ToUnicodeEx` with a key-state array per modifier set:
//! Shift (`VK_SHIFT`, `VK_LSHIFT`), Caps Lock toggled (`VK_CAPITAL` bit 0), AltGr as Windows
//! defines it (`VK_CONTROL` + `VK_MENU`, the left Ctrl and right Alt). A return of -1 is a dead key,
//! whose spacing form is in the buffer.
//!
//! **Dead-key state.** `ToUnicodeEx` keeps a dead key pending in the thread's keyboard state, where
//! it would accent the next key translated. Flag `0x4` (Windows 10 1607+) leaves the state alone;
//! an older Windows ignores it, so after a dead key Space is translated (flags 0) until the state
//! is clear, which on a newer one is a no-op that answers `" "`.
//!
//! NOT RUN: written and type-checked on macOS (`cargo check --target x86_64-pc-windows-msvc`), not
//! executed on a Windows host.
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, MapVirtualKeyExW, ToUnicodeEx, MAPVK_VSC_TO_VK_EX, VK_CAPITAL, VK_CONTROL, VK_LCONTROL, VK_LSHIFT, VK_MENU, VK_RMENU, VK_SHIFT,
    VK_SPACE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::{level, HostKey, HostLayout, KeyOutput, KeyboardError, KeyboardResult, ThirdLevel};
use crate::window::{evdev_code, EXTENDED_SCANCODES};

/// `ToUnicodeEx`'s "do not change the keyboard state" flag (Windows 10 1607+).
const NO_STATE_CHANGE: u32 = 0x4;
/// Space's set-1 make code.
const SPACE_SCANCODE: u32 = 0x39;

pub(super) fn current_layout() -> KeyboardResult<HostLayout> {
    // SAFETY: plain Win32 calls; a null foreground window answers thread 0, the caller's own.
    let hkl = unsafe {
        let foreground = GetForegroundWindow();
        let thread = if foreground.is_null() { 0 } else { GetWindowThreadProcessId(foreground, core::ptr::null_mut()) };
        GetKeyboardLayout(thread)
    };
    if hkl.is_null() {
        return Err(KeyboardError::Os { api: "GetKeyboardLayout", detail: "no keyboard layout".into() });
    }
    let scancodes = (0x01..=0x58u32).chain(EXTENDED_SCANCODES.iter().map(|&(make, _)| 0xE000 | make));
    let mut keys = Vec::new();
    for scancode in scancodes {
        let Some(code) = evdev_code(scancode) else { continue };
        // SAFETY: a plain query on a live layout handle.
        let vk = unsafe { MapVirtualKeyExW(scancode, MAPVK_VSC_TO_VK_EX, hkl) };
        if vk == 0 {
            continue;
        }
        let out: [KeyOutput; level::COUNT] = core::array::from_fn(|set| translate(hkl, vk, scancode, set));
        keys.push(HostKey { code, out });
    }
    Ok(HostLayout { name: format!("HKL {:08x}", hkl as usize), third_level: ThirdLevel::AltGr, keys })
}

/// What virtual key `vk` (scancode `scancode`) types under modifier set `set`.
fn translate(hkl: windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL, vk: u32, scancode: u32, set: usize) -> KeyOutput {
    let mut state = [0u8; 256];
    if set & level::SHIFT != 0 {
        state[usize::from(VK_SHIFT)] = 0x80;
        state[usize::from(VK_LSHIFT)] = 0x80;
    }
    if set & level::CAPS != 0 {
        state[usize::from(VK_CAPITAL)] = 0x01;
    }
    if set & level::ALT != 0 {
        for vk in [VK_CONTROL, VK_LCONTROL, VK_MENU, VK_RMENU] {
            state[usize::from(vk)] = 0x80;
        }
    }
    let mut buf = [0u16; 8];
    // SAFETY: the state array is 256 bytes as documented, the buffer's length is passed.
    let n = unsafe { ToUnicodeEx(vk, scancode & 0xFF, state.as_ptr(), buf.as_mut_ptr(), buf.len() as i32, NO_STATE_CHANGE, hkl) };
    match n {
        n if n > 0 => match String::from_utf16(&buf[..(n as usize).min(buf.len())]) {
            Ok(s) => KeyOutput::Text(s),
            Err(_) => KeyOutput::None,
        },
        -1 => {
            let spacing = char::decode_utf16([buf[0]]).next().and_then(Result::ok);
            clear_dead_key(hkl);
            spacing.map_or(KeyOutput::None, KeyOutput::Dead)
        }
        _ => KeyOutput::None,
    }
}

/// Translate Space until no dead key is pending (see the module header).
fn clear_dead_key(hkl: windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL) {
    let state = [0u8; 256];
    let mut buf = [0u16; 8];
    for _ in 0..4 {
        // SAFETY: as in `translate`.
        let n = unsafe { ToUnicodeEx(u32::from(VK_SPACE), SPACE_SCANCODE, state.as_ptr(), buf.as_mut_ptr(), buf.len() as i32, 0, hkl) };
        if n >= 0 {
            return;
        }
    }
}
