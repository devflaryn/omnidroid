//! **The host's current keyboard layout, read as a table**: for each physical key, the characters
//! the host types with it under each combination of Shift, Caps Lock and the third-level key
//! (Option on macOS, AltGr on Windows), and which of them are dead keys.
//!
//! # Why
//!
//! The guest's keyboard is an evdev device (`omni-linux`'s `evdev`), so Android sees **physical
//! keys** (`KEY_*`) and turns them into characters itself, through a key character map (`.kcm`).
//! Its own maps are a US one (`Generic.kcm`) and the overlays Android 14+'s KeyboardLayoutManager
//! picks from the IME's subtype locale -- neither is what the person at the host is typing with.
//! `omni-linux`'s `keymap` writes the device a map of its own from this table, so a key types in
//! the guest what it types on the host, whatever the host's layout is (US, Turkish F, German, ...).
//!
//! ```text
//! current_layout() -> KeyboardResult<HostLayout>
//! ```
//!
//! # Keyed by the Linux input code, as the window seam reports keys
//!
//! A key here is the Linux code (`KEY_*`) of the physical key, derived with **the window seam's own
//! tables**, so that the key a [`crate::window::WindowEvent::KeyDown`] names and the key this table
//! describes are one key by construction: macOS's `kVK` table (`window::macos::keys`), the set-1
//! scancodes elsewhere ([`crate::window::evdev_code`]). Only keys the window seam can deliver are
//! listed (a macOS key whose `scancode` is 0 never reaches the guest).
//!
//! # Backends
//!
//! * **macOS**: Text Input Sources' current keyboard layout (`TISCopyCurrentKeyboardLayoutInputSource`,
//!   then the current input source, then the current ASCII-capable layout -- an input method such
//!   as Japanese Kana has no layout data of its own), its `'uchr'` data, and `UCKeyTranslate` with
//!   `LMGetKbdType()` for each key and modifier set. TIS is main-thread API (macOS 14+ is reported to
//!   trap a call off the main queue in `dispatch_assert_queue`; not tried here), so the work runs on
//!   the window seam's AppKit thread.
//! * **Windows**: the foreground thread's layout (`GetKeyboardLayout`; the window thread's own
//!   layout is not known to a thread that has no window yet), `MapVirtualKeyExW` from scancode to
//!   virtual key and `ToUnicodeEx` with a key-state array per modifier set, with flag `0x4` (Windows
//!   10 1607+: "do not change the keyboard state") so a dead key does not linger, and the state
//!   flushed anyway for an older Windows that ignores the flag.
//! * **Linux and other unix**: none, [`KeyboardError::Unsupported`]. Android's `Generic.kcm` (US)
//!   stays. An X11 backend would read XKB's map for the session (`XkbKeycodeToKeysym` and
//!   `xkb_keysym_to_utf32`); not written, because no Linux host here runs the window path with a
//!   non-US layout to check it against.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(all(unix, not(target_os = "macos")))]
mod unix;
#[cfg(all(unix, not(target_os = "macos")))]
use unix as backend;

/// Result alias for the keyboard seam.
pub type KeyboardResult<T> = Result<T, KeyboardError>;

/// Why the host's layout could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyboardError {
    /// This host has no backend: the guest keeps its own layout.
    #[error("no keyboard-layout backend on this host ({0})")]
    Unsupported(&'static str),
    /// A host call failed or answered nothing usable.
    #[error("`{api}`: {detail}")]
    Os {
        /// The host entry point.
        api: &'static str,
        /// What it answered.
        detail: String,
    },
}

/// What one key types under one modifier set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutput {
    /// Nothing (a key with no character on that level).
    None,
    /// These characters (one, almost always; a Windows layout may type a ligature of several).
    Text(String),
    /// A **dead key**: it types nothing itself and accents the next key. The character is what the
    /// host types for the dead key followed by Space -- the accent's spacing form (`´`, `¨`, `^`,
    /// `` ` ``, `~`, ...), which is how both hosts name it.
    Dead(char),
}

/// The modifier sets a key is read under, as bits of the index into [`HostKey::out`].
pub mod level {
    /// Shift.
    pub const SHIFT: usize = 1;
    /// Caps Lock (locked on).
    pub const CAPS: usize = 2;
    /// The third-level key: Option on macOS, AltGr on Windows (see [`super::ThirdLevel`]).
    pub const ALT: usize = 4;
    /// How many sets there are: every combination of the three.
    pub const COUNT: usize = 8;
}

/// One physical key of the host's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    /// The key's Linux input code (`KEY_*`).
    pub code: u16,
    /// What it types under each modifier set, indexed by [`level`] bits: `out[0]` with none,
    /// `out[SHIFT | ALT]` with Shift and the third-level key, and so on.
    pub out: [KeyOutput; level::COUNT],
}

/// Which physical keys select the third level, as the guest sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThirdLevel {
    /// **Either** Option key (macOS): the guest sees `KEY_LEFTALT` or `KEY_RIGHTALT`.
    EitherAlt,
    /// **AltGr** (Windows): the guest sees `KEY_RIGHTALT` -- with the left Ctrl Windows reports
    /// beside it, because Windows' AltGr *is* Ctrl+Alt (a layout's third level answers either).
    AltGr,
}

/// The host's current keyboard layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLayout {
    /// The layout's name, for a log line: `Turkish Q (com.apple.keylayout.Turkish)` on macOS, the
    /// keyboard layout handle (`HKL 041f041f`) on Windows.
    pub name: String,
    /// Which keys select the third level.
    pub third_level: ThirdLevel,
    /// Every key the window seam delivers, in Linux-code order.
    pub keys: Vec<HostKey>,
}

/// The host's keyboard layout **as it is now**. A layout switched later is not followed.
///
/// # Errors
/// [`KeyboardError::Unsupported`] on a host without a backend (Linux); [`KeyboardError::Os`] when
/// the host has no current layout with character data, or a call fails.
pub fn current_layout() -> KeyboardResult<HostLayout> {
    let mut layout = backend::current_layout()?;
    layout.keys.sort_by_key(|k| k.code);
    layout.keys.dedup_by_key(|k| k.code);
    Ok(layout)
}
