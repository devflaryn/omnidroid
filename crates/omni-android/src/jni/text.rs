//! **Text input**: the Java side's `RbxKeyboard` -- an `EditText` -- and its listener `fi.p0`, run
//! by the host, so that what a person types in the host window reaches the engine's focused
//! `TextBox` the way a device's keyboard reaches it.
//!
//! # The path a device takes, decoded from `classes2.dex`
//!
//! * **The engine asks for the keyboard** with `NativeHelper.gameActivity_showKeyboard` or
//!   `NativeGLJavaInterface.showKeyboard`, both `(JZ[BLNativeTextBoxInfo;)V`: the text box (a
//!   native pointer the Java side keeps as a `long`), whether to lay the field out from the info,
//!   the box's text as UTF-8, and a `NativeTextBoxInfo`. Both decode the bytes
//!   (`new String(bytes, UTF_8)`) and reach `fi.p0.b` on the UI thread (`runOnUiThread`). This
//!   layer hands that to the embedding as a [`KeyboardRequest`]; the embedding is the UI thread.
//! * `fi.p0.b` calls `RbxKeyboard.setCurrentTextBox(textBox)`, then `setText(text)`, then -- when
//!   the flag is set -- `RbxKeyboard.l(info)`, which takes `manualFocusRelease`; then it shows and
//!   focuses the field and puts the cursor at the end (`setSelection(text.length())`).
//! * **Every text change** reaches the `TextWatcher`, `RbxKeyboard$a.onTextChanged(s, start,
//!   before, count)`: `k()` -- which is `syncTextboxTextAndCursorPosition2(text, selectionStart)`
//!   -- and then `nativePassText(textBox, text, false, start + count)`. **Every selection change**
//!   reaches `RbxKeyboard.onSelectionChanged`, which calls `k()` again.
//! * **Enter** reaches `RbxKeyboard$b.onEditorAction`: `k()`,
//!   `nativeReturnPressedFromOnScreenKeyboard(textBox)`, and -- unless the box asked for manual
//!   focus release (`g()`) -- `nativePassText(textBox, text, true, selectionStart)` and `e()`,
//!   which sets the current text box to `0` and hides the field.
//! * **The engine hides it** with `gameActivity_hideKeyboard` / `NativeGLJavaInterface.hideKeyboard`,
//!   reaching `fi.p0.a`: `RbxKeyboard.e()`.
//!
//! All three natives this module calls are static and exported by `libroblox.so` under their
//! short manglings ([`SYNC_SYMBOL`], [`PASS_TEXT_SYMBOL`], [`RETURN_SYMBOL`]).
//!
//! # What the host's keyboard is here
//!
//! The field's text arrives the way an input method commits it: [`WindowEvent::Text`], the host
//! layout's character, inserted at the cursor. Backspace and Enter are keys the field handles
//! itself (`BaseKeyListener.backspace` deletes one character before the cursor; a single-line
//! field turns Enter into its editor action). While the field is open it has the focus, so **it
//! takes the keys**: a device's focused `EditText` consumes them before `MainGameActivity.onKeyDown`
//! would pass them to `nativePassKeyEvent` ([`super::keys`]).
//!
//! # Two things this does not model, said so
//!
//! * `setText` in `fi.p0.b` fires `onTextChanged` before `setSelection` places the cursor, so the
//!   device's first `k()` reads whatever selection the fresh text has. Here that first sync
//!   already carries the end-of-text cursor the `setSelection` one line later gives it; the sync
//!   that follows is identical on both.
//! * No selection range, arrow keys, forward delete or `BACK`: the cursor is one point, moved only
//!   by what is typed or deleted. Each is a key a later run can ask for.
//!
//! **The text is never logged**: this is the path a password takes.

use std::sync::Arc;

use omni_cpu::GuestCpu;
use omni_mem::GuestAddr;
use omni_platform::window::WindowEvent;

use crate::boundary::{Boundary, GuestArg};
use crate::error::{AbiError, AbiResult};

use super::{Jni, KeyboardRequest};

/// The class the text natives are declared on.
pub const TEXT_CLASS: &str = "com/roblox/engine/jni/NativeGLInterface";

/// `syncTextboxTextAndCursorPosition2(Ljava/lang/String;I)V`, exported under its short mangling.
pub const SYNC_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeGLInterface_syncTextboxTextAndCursorPosition2";

/// `nativePassText(JLjava/lang/String;ZI)V`, exported under its short mangling.
pub const PASS_TEXT_SYMBOL: &str = "Java_com_roblox_engine_jni_NativeGLInterface_nativePassText";

/// `nativeReturnPressedFromOnScreenKeyboard(J)V`, exported under its short mangling.
pub const RETURN_SYMBOL: &str =
    "Java_com_roblox_engine_jni_NativeGLInterface_nativeReturnPressedFromOnScreenKeyboard";

/// Backspace's set-1 make code, as [`WindowEvent::KeyDown`] carries it (Linux `KEY_BACKSPACE`,
/// 14, which `Generic.kl` maps to `DEL`).
const SCAN_BACKSPACE: u32 = 0x0E;
/// Enter's (`KEY_ENTER`, 28 -> `ENTER`).
const SCAN_ENTER: u32 = 0x1C;
/// The keypad's Enter, `E0`-extended (`KEY_KPENTER` -> `NUMPAD_ENTER`, which `TextView` handles
/// as it handles `ENTER`).
const SCAN_KEYPAD_ENTER: u32 = 0xE01C;

/// One call the Java side makes into the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextCall {
    /// `syncTextboxTextAndCursorPosition2(text, cursor)` -- `RbxKeyboard.k()`.
    Sync {
        /// The field's whole text.
        text: String,
        /// `getSelectionStart()`, in UTF-16 units, as Java counts.
        cursor: i32,
    },
    /// `nativePassText(textBox, text, enter, cursor)`.
    PassText {
        /// The text box the field is editing for.
        text_box: i64,
        /// The field's whole text.
        text: String,
        /// `true` only from the editor action (Enter).
        enter: bool,
        /// Where the change ended, in UTF-16 units.
        cursor: i32,
    },
    /// `nativeReturnPressedFromOnScreenKeyboard(textBox)`.
    ReturnPressed {
        /// The text box the field is editing for.
        text_box: i64,
    },
}

impl TextCall {
    /// A description with the text's **length** in place of the text -- what a log may carry.
    #[must_use]
    pub fn redacted(&self) -> String {
        let chars = |text: &str| text.chars().count();
        match self {
            TextCall::Sync { text, cursor } => {
                format!("syncTextboxTextAndCursorPosition2(<{} chars>, {cursor})", chars(text))
            }
            TextCall::PassText { text_box, text, enter, cursor } => format!(
                "nativePassText({text_box:#x}, <{} chars>, {enter}, {cursor})",
                chars(text)
            ),
            TextCall::ReturnPressed { text_box } => {
                format!("nativeReturnPressedFromOnScreenKeyboard({text_box:#x})")
            }
        }
    }
}

/// `RbxKeyboard`'s state, and the calls each thing done to it makes.
///
/// Pure: no guest, no JNI. [`TextInput`] makes the calls this returns.
#[derive(Debug, Default, Clone)]
pub struct Keyboard {
    /// `RbxKeyboard.h`, the current text box; `0` when there is none, which is closed.
    text_box: i64,
    /// The field's text, in UTF-16 units, as an `Editable` holds it.
    text: Vec<u16>,
    /// The cursor, in UTF-16 units.
    cursor: usize,
    /// `RbxKeyboard.i`, set by `setManualFocusRelease`.
    manual_focus_release: bool,
}

impl Keyboard {
    /// Whether the field is open for a text box, and so takes the keys.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.text_box != 0
    }

    /// The field's text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf16_lossy(&self.text)
    }

    /// The cursor, in UTF-16 units.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Carry out an engine request, as `fi.p0.b` / `fi.p0.a` do.
    pub fn apply(&mut self, request: &KeyboardRequest) -> Vec<TextCall> {
        match request {
            KeyboardRequest::Show { text_box, text, manual_focus_release } => {
                self.text_box = *text_box;
                if let Some(manual) = manual_focus_release {
                    self.manual_focus_release = *manual;
                }
                self.text = text.encode_utf16().collect();
                self.cursor = self.text.len();
                // `setText`: one change of the whole text, then `setSelection(length)`.
                self.edited(0, self.text.len())
            }
            KeyboardRequest::Hide => {
                // `RbxKeyboard.e()`: forget the text box, hide the field. No call is made.
                self.text_box = 0;
                Vec::new()
            }
        }
    }

    /// Commit `typed` at the cursor, as an input method does.
    pub fn type_text(&mut self, typed: &str) -> Vec<TextCall> {
        let units: Vec<u16> = typed.encode_utf16().collect();
        if !self.is_open() || units.is_empty() {
            return Vec::new();
        }
        let start = self.cursor;
        self.text.splice(start..start, units.iter().copied());
        self.cursor = start + units.len();
        self.edited(start, units.len())
    }

    /// Delete the character before the cursor -- **both halves of a surrogate pair**, which
    /// `BaseKeyListener.backspace` treats as the one character it is.
    pub fn backspace(&mut self) -> Vec<TextCall> {
        if !self.is_open() || self.cursor == 0 {
            // Nothing changes, so no watcher fires.
            return Vec::new();
        }
        let mut start = self.cursor - 1;
        let low = |unit: u16| (0xDC00..=0xDFFF).contains(&unit);
        let high = |unit: u16| (0xD800..=0xDBFF).contains(&unit);
        if start > 0 && low(self.text[start]) && high(self.text[start - 1]) {
            start -= 1;
        }
        self.text.drain(start..self.cursor);
        self.cursor = start;
        self.edited(start, 0)
    }

    /// The editor action: `RbxKeyboard$b.onEditorAction`.
    pub fn enter(&mut self) -> Vec<TextCall> {
        if !self.is_open() {
            return Vec::new();
        }
        let mut calls = vec![self.sync(), TextCall::ReturnPressed { text_box: self.text_box }];
        if !self.manual_focus_release {
            calls.push(self.pass(true, self.cursor));
            // `e()`.
            self.text_box = 0;
        }
        calls
    }

    /// One change that ended at `start + count` and moved the cursor: `onTextChanged`'s `k()` and
    /// `nativePassText`, then `onSelectionChanged`'s `k()`.
    fn edited(&self, start: usize, count: usize) -> Vec<TextCall> {
        vec![self.sync(), self.pass(false, start + count), self.sync()]
    }

    fn sync(&self) -> TextCall {
        TextCall::Sync { text: self.text(), cursor: java_int(self.cursor) }
    }

    fn pass(&self, enter: bool, cursor: usize) -> TextCall {
        TextCall::PassText { text_box: self.text_box, text: self.text(), enter, cursor: java_int(cursor) }
    }
}

/// A Java `int` index. A field longer than `i32::MAX` units cannot exist in Java.
fn java_int(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Declare [`TEXT_CLASS`], memberless, if nothing has yet -- [`super::keys`] declares the same
/// class for the same reason: a `jclass` for a static native.
fn declare_text_class(jni: &Jni) -> AbiResult<()> {
    jni.with_registry(|registry| {
        if registry.find(TEXT_CLASS).is_some() {
            return Ok(());
        }
        registry
            .declare(&super::classes::ClassSpec {
                name: TEXT_CLASS,
                tier: super::classes::Tier::Support,
                methods: &[],
                fields: &[],
            })
            .map(|_| ())
    })
}

/// **The embedding seam**: [`KeyboardRequest`]s and host window events in, the Java side's text
/// natives out, on the thread the embedding calls the lifecycle natives from -- the UI thread.
#[derive(Debug)]
pub struct TextInput {
    keyboard: Keyboard,
    sync: GuestAddr,
    pass_text: GuestAddr,
    return_pressed: GuestAddr,
    /// One `jclass` for [`TEXT_CLASS`], taken once, for the reason `TouchInput`'s is.
    class: u64,
    made: u64,
}

impl TextInput {
    /// A seam over the engine's three text natives, found through `resolve` -- the embedding's
    /// export table.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] naming the first symbol `resolve` does not know, and whatever
    /// declaring [`TEXT_CLASS`] or taking its `jclass` refuses for.
    pub fn new(jni: &Jni, resolve: &dyn Fn(&str) -> Option<GuestAddr>) -> AbiResult<Self> {
        let find = |symbol: &str| {
            resolve(symbol).ok_or_else(|| AbiError::JniRefused {
                function: symbol.to_string(),
                address: 0,
                detail: format!(
                    "`{symbol}` is exported by libroblox.so on a device and nothing resolved it \
                     here, so no typed text can reach the engine"
                ),
            })
        };
        let sync = find(SYNC_SYMBOL)?;
        let pass_text = find(PASS_TEXT_SYMBOL)?;
        let return_pressed = find(RETURN_SYMBOL)?;
        declare_text_class(jni)?;
        let class = jni.class_reference(TEXT_CLASS)?;
        Ok(Self { keyboard: Keyboard::default(), sync, pass_text, return_pressed, class, made: 0 })
    }

    /// Whether the field is open, and so takes the keys.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.keyboard.is_open()
    }

    /// The field's state.
    #[must_use]
    pub fn keyboard(&self) -> &Keyboard {
        &self.keyboard
    }

    /// How many calls into the engine have returned.
    #[must_use]
    pub fn made(&self) -> u64 {
        self.made
    }

    /// Carry out one engine request, making the calls it produces with `thread`'s `JNIEnv`.
    ///
    /// **The caller holds the activations**, as around [`super::input::TouchInput::deliver`].
    ///
    /// # Errors
    ///
    /// The first call that does not return.
    pub fn apply(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        request: &KeyboardRequest,
    ) -> AbiResult<Vec<TextCall>> {
        let calls = self.keyboard.apply(request);
        self.make(jni, boundary, cpu, thread, &calls)?;
        Ok(calls)
    }

    /// Deliver one host window event to the open field: typed text, Backspace (repeats too) and
    /// Enter (not its repeats). Anything else, or a closed field, makes no call.
    ///
    /// # Errors
    ///
    /// The first call that does not return.
    pub fn deliver(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        event: &WindowEvent,
    ) -> AbiResult<Vec<TextCall>> {
        let calls = match event {
            WindowEvent::Text { text } => self.keyboard.type_text(text),
            WindowEvent::KeyDown { scancode: SCAN_BACKSPACE, .. } => self.keyboard.backspace(),
            WindowEvent::KeyDown { scancode: SCAN_ENTER | SCAN_KEYPAD_ENTER, repeat: false, .. } => {
                self.keyboard.enter()
            }
            _ => Vec::new(),
        };
        self.make(jni, boundary, cpu, thread, &calls)?;
        Ok(calls)
    }

    fn make(
        &mut self,
        jni: &Jni,
        boundary: &Arc<Boundary>,
        cpu: &mut dyn GuestCpu,
        thread: usize,
        calls: &[TextCall],
    ) -> AbiResult<()> {
        let int = |value: i32| GuestArg::Int(i64::from(value) as u64);
        for call in calls {
            let env = GuestArg::Pointer(jni.env_for(thread));
            let class = GuestArg::Int(self.class);
            // The host's stand-in for the Java `String` argument, deleted once the call returns:
            // see `Jni::delete_local`.
            let (what, target, text, args): (&str, GuestAddr, Option<u64>, Vec<GuestArg>) = match call {
                TextCall::Sync { text, cursor } => {
                    let string = jni.new_string(text)?;
                    (
                        "NativeGLInterface.syncTextboxTextAndCursorPosition2 (RbxKeyboard.k)",
                        self.sync,
                        Some(string),
                        vec![env, class, GuestArg::Int(string), int(*cursor)],
                    )
                }
                TextCall::PassText { text_box, text, enter, cursor } => {
                    let string = jni.new_string(text)?;
                    (
                        "NativeGLInterface.nativePassText (RbxKeyboard)",
                        self.pass_text,
                        Some(string),
                        vec![
                            env,
                            class,
                            GuestArg::Int(*text_box as u64),
                            GuestArg::Int(string),
                            GuestArg::Int(u64::from(*enter)),
                            int(*cursor),
                        ],
                    )
                }
                TextCall::ReturnPressed { text_box } => (
                    "NativeGLInterface.nativeReturnPressedFromOnScreenKeyboard (RbxKeyboard$b)",
                    self.return_pressed,
                    None,
                    vec![env, class, GuestArg::Int(*text_box as u64)],
                ),
            };
            let made = boundary.call_guest(cpu, what, target, &args, super::input::PER_EVENT);
            if let Some(string) = text {
                // A native may delete a local it was handed, which JNI allows; if this one did,
                // the reference is already gone and there is nothing left to release.
                let _ = jni.delete_local(string);
            }
            made?;
            self.made += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOX: i64 = 0x7a_0000_1000;

    fn show(keyboard: &mut Keyboard, text: &str, manual: Option<bool>) -> Vec<TextCall> {
        keyboard.apply(&KeyboardRequest::Show {
            text_box: BOX,
            text: text.to_string(),
            manual_focus_release: manual,
        })
    }

    fn sync(text: &str, cursor: i32) -> TextCall {
        TextCall::Sync { text: text.to_string(), cursor }
    }

    fn pass(text: &str, enter: bool, cursor: i32) -> TextCall {
        TextCall::PassText { text_box: BOX, text: text.to_string(), enter, cursor }
    }

    /// **Showing is `setText` then `setSelection(length)`**: the watcher's sync and pass for the
    /// whole text, then the selection's sync, all with the cursor at the end.
    #[test]
    fn showing_sets_the_text_and_puts_the_cursor_at_its_end() {
        let mut keyboard = Keyboard::default();
        assert!(!keyboard.is_open());
        assert_eq!(show(&mut keyboard, "ab", None), [sync("ab", 2), pass("ab", false, 2), sync("ab", 2)]);
        assert!(keyboard.is_open());
        assert_eq!((keyboard.text(), keyboard.cursor()), ("ab".to_string(), 2));
    }

    /// **Typing inserts at the cursor and counts in UTF-16 units**, as Java does: `é` is one unit,
    /// `😀` two. Each commit is the watcher's sync and pass, ending where the insertion ends, and
    /// the selection's sync.
    #[test]
    fn typed_text_is_inserted_at_the_cursor_in_utf16_units() {
        let mut keyboard = Keyboard::default();
        show(&mut keyboard, "", None);
        assert_eq!(keyboard.type_text("a"), [sync("a", 1), pass("a", false, 1), sync("a", 1)]);
        assert_eq!(keyboard.type_text("é"), [sync("aé", 2), pass("aé", false, 2), sync("aé", 2)]);
        assert_eq!(keyboard.type_text("😀"), [sync("aé😀", 4), pass("aé😀", false, 4), sync("aé😀", 4)]);
        assert!(keyboard.type_text("").is_empty(), "an empty commit changes nothing");
    }

    /// **Backspace deletes one character, and a surrogate pair is one**; at the start of the text
    /// it changes nothing, so nothing is called.
    #[test]
    fn backspace_deletes_a_whole_character_and_nothing_before_the_start() {
        let mut keyboard = Keyboard::default();
        show(&mut keyboard, "a😀", None);
        assert_eq!(keyboard.backspace(), [sync("a", 1), pass("a", false, 1), sync("a", 1)]);
        assert_eq!(keyboard.backspace(), [sync("", 0), pass("", false, 0), sync("", 0)]);
        assert!(keyboard.backspace().is_empty());
        assert!(keyboard.is_open());
    }

    /// **Enter is the editor action**: sync, the return native, and -- without manual focus
    /// release -- the final pass with `true` and the field closed. With it, the field stays open
    /// and the pass is not made. A show that does not lay the field out keeps the setting.
    #[test]
    fn enter_passes_the_text_and_closes_unless_focus_release_is_manual() {
        let mut keyboard = Keyboard::default();
        show(&mut keyboard, "hi", Some(false));
        assert_eq!(
            keyboard.enter(),
            [sync("hi", 2), TextCall::ReturnPressed { text_box: BOX }, pass("hi", true, 2)]
        );
        assert!(!keyboard.is_open());

        show(&mut keyboard, "hi", Some(true));
        assert_eq!(keyboard.enter(), [sync("hi", 2), TextCall::ReturnPressed { text_box: BOX }]);
        assert!(keyboard.is_open());
        show(&mut keyboard, "yo", None);
        assert_eq!(keyboard.enter().len(), 2, "None leaves manual release as it was");
    }

    /// **A closed field takes nothing**: typing, Backspace and Enter make no call, and a hide
    /// closes an open one without a call.
    #[test]
    fn a_closed_field_makes_no_calls() {
        let mut keyboard = Keyboard::default();
        assert!(keyboard.type_text("a").is_empty());
        assert!(keyboard.backspace().is_empty());
        assert!(keyboard.enter().is_empty());
        show(&mut keyboard, "x", None);
        assert!(keyboard.apply(&KeyboardRequest::Hide).is_empty());
        assert!(!keyboard.is_open());
        assert!(keyboard.type_text("a").is_empty());
    }

    /// **A log line never carries the text** -- this is the path a password takes.
    #[test]
    fn a_redacted_call_carries_the_length_and_not_the_text() {
        let secret = pass("hunter2", true, 7);
        let line = secret.redacted();
        assert!(!line.contains("hunter2"), "{line}");
        assert!(line.contains("<7 chars>"), "{line}");
        assert!(!sync("hunter2", 7).redacted().contains("hunter2"));
    }
}
