//! **The host's cursor, following the engine's own signals**: hidden while the engine draws its
//! own, held still while the engine holds its own still, and given back whenever the window is not
//! the one the user is using.
//!
//! # What a device does, decoded (on the modified 2.739.691 build)
//!
//! **The app hides the system pointer over the engine's surface, always.**
//! `com.roblox.client.RBXSurfaceView.onResolvePointerIcon(MotionEvent, int)` (`classes2.dex`) is
//! `return PointerIcon.getSystemIcon(this.a, 0)` -- `0` is `PointerIcon.TYPE_NULL`, the icon that
//! draws nothing -- for every event, with no condition. `RBXSurfaceView` is the engine's surface in
//! `res/layout/fragment_shared.xml`, the path `ActivityNativeMain` (`fi/r0` over it) takes. The path
//! this layer runs, `MainGameActivity`, hands `vk.e` AGDK's own `GameActivity$d` (`onCreate`
//! `0x00dc`-`0x00ea`, `GameActivity.H`), which overrides no pointer icon: there nothing hides the
//! system pointer, and a device with a mouse would show both. Nothing else in the three dex files
//! calls `setPointerIcon` or `getSystemIcon` (the only other override is Material's `Chip`), and
//! `libroblox.so` names neither. So the app's one statement of what the pointer over the engine's
//! surface should look like is **none: the engine draws it.**
//!
//! **The engine draws its own cursor in software.** Its 2D pass (`0x27a589c`, the profiler scope
//! `Render/Pass2d/SoftwareCursor` at `0x27a5dbc`) calls `MouseService`'s draw (`0x27b2384`; the
//! service's name is `0x1f62768`'s `MouseService`) on every frame. That draw keeps the cursor's
//! image when `MouseService+0x209` -- **`UserInputService.MouseIconEnabled`**, whose getter
//! `0x47eb328` reads exactly that byte -- is set or its mode at `+0x210` is 1 (`0x27b2688`-
//! `0x27b26bc`), and drops it while the last input is a gamepad (`0x27b26cc`-`0x27b271c`:
//! `UserInputService+0x338`, `LastInputType`, through `0x24c74b4`, `Gamepad1`..`Gamepad8` being
//! 12..19). Mouse and keyboard input -- all this layer's play configuration sends -- keep it. Where
//! the engine draws nothing (a game that turns `MouseIconEnabled` off), the game has asked for no
//! cursor, and a hidden host cursor is what it asked for too.
//!
//! **The engine holds its cursor still in two states, and says which.** The main window's lock
//! state, `[[input + 0xb00] + 0x88]` with `input` the input singleton (`0x2343bc8` returns it;
//! `0x6afaad0` in this build), is written only by `0x274c4dc` under the singleton's mutex
//! (`+0x1e0`) and announced through the signal at `+0xa30` (`0x24e29d0`): **0** at `0x274de70`
//! and `0x274df2c`, **1** (`MouseBehavior.LockCenter`: shift-lock, first person) at `0x274f320`,
//! **2** (`MouseBehavior.LockCurrentPosition`: the right-drag camera) at `0x274f360`.
//! `nativeGetMainWindowIsMouseLockedCenter` answers `state == 1` only (`0x2e73f28`-`0x2e73f38`,
//! `cmp w8, #1; cset w19, eq`), and that is all `vk.e` asks before `requestPointerCapture` -- so on
//! a device the right-drag camera **captures nothing**: the pointer, hidden over the view, keeps
//! moving while the engine's cursor stays put, and `vk.e.y`'s `dx`/`dy` turn the camera.
//!
//! # What the host does with them
//!
//! [`host_cursor`] is the whole rule, and it has no input of its own -- no button, no key:
//!
//! * **hidden** over the view -- **focused or not** -- while the engine draws its own cursor there
//!   (the app's `TYPE_NULL`; this embedding counts the view as drawn once the engine has presented
//!   a frame into it), and while the cursor is held. Not scoped to the focus, because the engine's
//!   cursor is not: the pointer's moves over an inactive window still reach it and it still draws
//!   (MEASURED, the owner's w33: two cursors over the unfocused window). The window scopes it to
//!   the client area, so leaving the view shows the host's cursor;
//! * **held** -- the window's pointer capture: hidden, still, raw motion -- while the engine holds
//!   its cursor still: its lock state is 1 or 2, or `vk.e` holds Android's pointer capture. For
//!   state 1 that is what a device does. For state 2 a device does not hold the pointer, and the
//!   host does, because a host cursor that went on moving under the engine's still one is exactly
//!   the second cursor the owner asked to be rid of; the motion still reaches the engine as a
//!   relative move at the held position (`super::mouse`, "What this embedding decides");
//! * **never held** while the window does not have the focus or is minimised, whatever the engine
//!   says: `Alt+Tab` always gives the user a cursor that moves, and it is taken again when the
//!   focus returns if the engine still wants it. Minimised, nothing is hidden either;
//! * **let go where the engine has its cursor**: before the capture is given back, the host's
//!   cursor is moved -- still invisible -- to the position the engine was last told
//!   (`MouseInput::pointer_px`), so it reappears exactly under the engine's cursor and the window's
//!   report of where it is makes no move (MEASURED, w33: a flash of a cursor elsewhere at every
//!   let-go of the right-drag camera).
//!
//! The lock state is read straight from the engine's memory -- no call, so it can be read every
//! turn of the UI loop -- at the location [`LockLocation::decode`] finds in the loaded library's
//! own `nativeGetMainWindowIsMouseLockedCenter`, so no address above is written into this layer.
//! The read takes no lock: an aligned word the engine writes whole, read a turn late at worst.

use core::fmt;
use std::time::{Duration, Instant};

use omni_mem::GuestAddr;
use omni_platform::window::{Window, WindowEvent};

use crate::mem::{Blame, GuestMem};

/// The engine's main-window mouse lock: `[[input + 0xb00] + 0x88]` (see this module's
/// documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// 0: the cursor moves.
    Free,
    /// 1: `MouseBehavior.LockCenter` -- shift-lock and first person. `vk.e` captures the pointer.
    Center,
    /// 2: `MouseBehavior.LockCurrentPosition` -- the right-drag camera. The engine's cursor stays
    /// where it was; nothing on a device captures.
    CurrentPosition,
}

impl LockState {
    /// The state a word of the engine's is, or `None` for a value the engine never writes.
    #[must_use]
    pub const fn from_word(word: u32) -> Option<Self> {
        match word {
            0 => Some(LockState::Free),
            1 => Some(LockState::Center),
            2 => Some(LockState::CurrentPosition),
            _ => None,
        }
    }

    /// The engine's name for it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            LockState::Free => "Default",
            LockState::Center => "LockCenter",
            LockState::CurrentPosition => "LockCurrentPosition",
        }
    }
}

/// **Where the lock state lives, decoded from the loaded library's own code**: the input
/// singleton's getter, and the two offsets `nativeGetMainWindowIsMouseLockedCenter`'s handler
/// loads through before it compares the state with 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockLocation {
    /// The function the native calls first; it returns the input singleton.
    pub getter: GuestAddr,
    /// The function it calls second, which locks the singleton and answers `state == 1`.
    pub handler: GuestAddr,
    /// `ldr xM, [x<singleton>, #window_offset]`: the main window's input state (`0xb00`).
    pub window_offset: usize,
    /// `ldr wN, [xM, #state_offset]`: the lock state in it (`0x88`).
    pub state_offset: usize,
}

/// The target of a `b`/`bl` at `pc`.
fn branch_target(pc: GuestAddr, word: u32) -> GuestAddr {
    let imm26 = (((word & 0x03FF_FFFF) << 6) as i32 >> 6) as isize;
    pc.wrapping_add_signed(imm26 * 4)
}

impl LockLocation {
    /// Read `native` -- `nativeGetMainWindowIsMouseLockedCenter` -- through `words_at` (`count`
    /// instruction words at an address) and find the getter, the handler and the two offsets:
    ///
    /// * the native's first two `bl`s are the getter and the handler (a handler that is a lone `b`
    ///   is followed);
    /// * in the handler, the register `x0` is moved to (`mov xK, x0`) holds the singleton; the
    ///   comparison `cmp wN, #1` that decides the answer is followed by `cset wX, eq`, and loads
    ///   `wN` by `ldr wN, [xM, #state]` from a pointer loaded by `ldr xM, [xK, #window]`.
    ///
    /// # Errors
    ///
    /// Why not, naming the shape that was missing: this build's code is not the one decoded.
    pub fn decode(
        words_at: &dyn Fn(GuestAddr, usize) -> Option<Vec<u32>>,
        native: GuestAddr,
    ) -> Result<LockLocation, String> {
        let body = words_at(native, 32).ok_or_else(|| format!("the native's code at {native:#x} is not readable"))?;
        let calls: Vec<GuestAddr> = body
            .iter()
            .enumerate()
            .filter(|(_, word)| *word & 0xFC00_0000 == 0x9400_0000)
            .map(|(index, word)| branch_target(native + 4 * index, *word))
            .take(2)
            .collect();
        let [getter, mut handler] = calls[..] else {
            return Err(format!("the native at {native:#x} does not make two calls in its first 32 instructions"));
        };
        let first = words_at(handler, 1).ok_or_else(|| format!("the handler at {handler:#x} is not readable"))?;
        if first[0] & 0xFC00_0000 == 0x1400_0000 {
            handler = branch_target(handler, first[0]);
        }
        let code = words_at(handler, 48).ok_or_else(|| format!("the handler at {handler:#x} is not readable"))?;
        let compare = code
            .iter()
            .position(|&word| word & 0xFFFF_FC1F == 0x7100_041F)
            .ok_or_else(|| format!("the handler at {handler:#x} compares nothing with 1 (`cmp wN, #1`)"))?;
        if code.get(compare + 1).is_none_or(|&word| word & 0xFFFF_FFE0 != 0x1A9F_17E0) {
            return Err(format!(
                "the handler at {handler:#x} does not answer equality after its `cmp wN, #1` (`cset wX, eq`)"
            ));
        }
        let state_reg = (code[compare] >> 5) & 31;
        // The singleton: where the handler keeps its first argument, or `x0` itself.
        let object_reg = code[..compare]
            .iter()
            .find(|&&word| word & 0xFFFF_FFE0 == 0xAA00_03E0)
            .map_or(0, |&word| word & 31);
        let state_load = (compare.saturating_sub(4)..compare)
            .rev()
            .find(|&index| code[index] & 0xFFC0_001F == 0xB940_0000 | state_reg)
            .ok_or_else(|| format!("the handler at {handler:#x} does not load w{state_reg} before comparing it"))?;
        let window_reg = (code[state_load] >> 5) & 31;
        let state_offset = ((code[state_load] >> 10) & 0xFFF) as usize * 4;
        let window_load = (state_load.saturating_sub(6)..state_load)
            .rev()
            .find(|&index| {
                code[index] & 0xFFC0_001F == 0xF940_0000 | window_reg && (code[index] >> 5) & 31 == object_reg
            })
            .ok_or_else(|| {
                format!("the handler at {handler:#x} does not load x{window_reg} from the singleton (x{object_reg})")
            })?;
        let window_offset = ((code[window_load] >> 10) & 0xFFF) as usize * 8;
        Ok(LockLocation { getter, handler, window_offset, state_offset })
    }

    /// The lock state of the singleton at `object`, the getter's answer.
    #[must_use]
    pub fn at(&self, object: GuestAddr) -> EngineLock {
        EngineLock { object, window_offset: self.window_offset, state_offset: self.state_offset }
    }
}

/// **The engine's lock state, where it is**: read with [`EngineLock::read`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineLock {
    /// The input singleton.
    pub object: GuestAddr,
    /// See [`LockLocation::window_offset`].
    pub window_offset: usize,
    /// See [`LockLocation::state_offset`].
    pub state_offset: usize,
}

impl EngineLock {
    /// The state now. `None` while the engine has no main window's input state (a null pointer),
    /// when the memory is not readable, or for a word the engine never writes.
    #[must_use]
    pub fn read(&self, mem: &GuestMem) -> Option<LockState> {
        let blame = Blame::new("the engine's mouse-lock state (jni::cursor)", self.object, 0);
        let window = mem.read_u64(self.object.wrapping_add(self.window_offset), blame).ok()?;
        if window == 0 {
            return None;
        }
        let state = (window as GuestAddr).wrapping_add(self.state_offset);
        LockState::from_word(mem.read_u32(state, blame).ok()?)
    }
}

impl fmt::Display for EngineLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[[{:#x} + {:#x}] + {:#x}]", self.object, self.window_offset, self.state_offset)
    }
}

/// What the engine and the app say about the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSignals {
    /// The engine draws its own cursor over its view: the app's `TYPE_NULL` over the engine's
    /// surface, once that surface has content.
    pub draws_own_cursor: bool,
    /// The engine's lock state, `Free` when it cannot be read.
    pub lock: LockState,
    /// `vk.e` holds Android's pointer capture (it asked, and has not given it back).
    pub view_captured: bool,
}

/// What the host's window says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFacts {
    /// The window has the keyboard focus.
    pub focused: bool,
    /// The window is minimised.
    pub minimized: bool,
}

/// What the host's cursor should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostCursor {
    /// Not drawn over the client area.
    pub hidden: bool,
    /// Held still, hidden, with raw motion: the window's pointer capture.
    pub held: bool,
}

/// **The rule** (this module's "What the host does with them").
#[must_use]
pub const fn host_cursor(engine: EngineSignals, host: HostFacts) -> HostCursor {
    let still = engine.view_captured || !matches!(engine.lock, LockState::Free);
    let held = host.focused && !host.minimized && still;
    HostCursor { hidden: !host.minimized && (engine.draws_own_cursor || held), held }
}

/// The window the rule is carried out on -- [`Window`], or a test's -- and the two facts about it
/// the rule reads, **asked of it every time** rather than followed from its events: a window's
/// first focus arrives while it is being shown, before anyone here is listening.
pub trait CursorHost {
    /// [`Window::has_focus`].
    fn focused(&self) -> bool;
    /// Minimised: a client area of zero ([`Window::client_size`]).
    fn minimized(&self) -> bool;
    /// [`Window::set_cursor_hidden`].
    ///
    /// # Errors
    ///
    /// The host's refusal, as text.
    fn set_cursor_hidden(&mut self, hidden: bool) -> Result<(), String>;
    /// [`Window::set_pointer_capture`].
    ///
    /// # Errors
    ///
    /// The host's refusal, as text.
    fn set_pointer_capture(&mut self, captured: bool) -> Result<bool, String>;
    /// [`Window::has_pointer_capture`].
    fn has_pointer_capture(&self) -> bool;
    /// [`Window::warp_pointer`].
    ///
    /// # Errors
    ///
    /// The host's refusal, as text.
    fn warp_pointer(&mut self, x: i32, y: i32) -> Result<(), String>;
}

impl CursorHost for Window {
    fn focused(&self) -> bool {
        Window::has_focus(self)
    }
    fn minimized(&self) -> bool {
        self.client_size().is_ok_and(|(width, height)| width == 0 || height == 0)
    }
    fn set_cursor_hidden(&mut self, hidden: bool) -> Result<(), String> {
        Window::set_cursor_hidden(self, hidden).map_err(|error| error.to_string())
    }
    fn set_pointer_capture(&mut self, captured: bool) -> Result<bool, String> {
        Window::set_pointer_capture(self, captured).map_err(|error| error.to_string())
    }
    fn has_pointer_capture(&self) -> bool {
        Window::has_pointer_capture(self)
    }
    fn warp_pointer(&mut self, x: i32, y: i32) -> Result<(), String> {
        Window::warp_pointer(self, x, y).map_err(|error| error.to_string())
    }
}

/// Counts of what a [`CursorController`] did, for the end-of-run report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorCounts {
    /// Times the host cursor was hidden.
    pub hides: u64,
    /// Times it was shown again.
    pub shows: u64,
    /// Times it was held (the window's capture taken).
    pub holds: u64,
    /// Times it was let go.
    pub releases: u64,
    /// Times the engine's lock state was seen to become `LockCenter`.
    pub lock_center: u64,
    /// Times it was seen to become `LockCurrentPosition`.
    pub lock_current_position: u64,
    /// Times it was read.
    pub lock_reads: u64,
}

/// How often a change of the host cursor is said, at most: a right-drag camera is a hold and a
/// release per drag, and a person turning the camera does that several times a second.
pub const SAY_EVERY: Duration = Duration::from_secs(1);

/// **The rule, kept and carried out**: the engine's signals and the window's facts in, the window
/// set to match, what changed said -- at most once per [`SAY_EVERY`] -- and counted.
#[derive(Debug)]
pub struct CursorController {
    engine: EngineSignals,
    /// Whether the window's cursor is hidden now, as last set.
    hidden: bool,
    counts: CursorCounts,
    said_at: Option<Instant>,
    /// Changes since the last line said.
    unsaid: u64,
    /// Where the engine has the pointer, in the view's pixels, as last told: where the host's
    /// cursor goes before a hold is let go.
    pointer: Option<(i32, i32)>,
}

impl Default for CursorController {
    fn default() -> Self {
        CursorController::new()
    }
}

impl CursorController {
    /// Nothing drawn, nothing locked, nothing captured, the cursor as the window has it.
    #[must_use]
    pub fn new() -> CursorController {
        CursorController {
            engine: EngineSignals { draws_own_cursor: false, lock: LockState::Free, view_captured: false },
            hidden: false,
            counts: CursorCounts::default(),
            said_at: None,
            unsaid: 0,
            pointer: None,
        }
    }

    /// Where the engine has the pointer now, in the view's pixels (`MouseInput::pointer_px`).
    pub fn set_pointer(&mut self, at: Option<(i32, i32)>) {
        self.pointer = at;
    }

    /// Take in what a window event says about Android's pointer capture: losing the focus, or the
    /// window's capture, ends it, as a window losing the focus ends a view's on a device.
    pub fn observe(&mut self, event: &WindowEvent) {
        if matches!(event, WindowEvent::PointerCaptureLost | WindowEvent::FocusChanged { focused: false }) {
            self.engine.view_captured = false;
        }
    }

    /// The engine's lock state, as just read; `None` (unreadable, or no window yet) is `Free`.
    pub fn set_lock(&mut self, lock: Option<LockState>) {
        self.counts.lock_reads += u64::from(lock.is_some());
        let lock = lock.unwrap_or(LockState::Free);
        if lock != self.engine.lock {
            match lock {
                LockState::Center => self.counts.lock_center += 1,
                LockState::CurrentPosition => self.counts.lock_current_position += 1,
                LockState::Free => {}
            }
        }
        self.engine.lock = lock;
    }

    /// Whether the engine draws its own cursor over its view now.
    pub fn set_draws_own_cursor(&mut self, draws: bool) {
        self.engine.draws_own_cursor = draws;
    }

    /// Whether `vk.e` holds Android's pointer capture: it asked (`true`) or gave it back (`false`).
    pub fn set_view_captured(&mut self, captured: bool) {
        self.engine.view_captured = captured;
    }

    /// What the cursor should be over `host` now.
    #[must_use]
    pub fn wanted(&self, host: &dyn CursorHost) -> HostCursor {
        host_cursor(self.engine, HostFacts { focused: host.focused(), minimized: host.minimized() })
    }

    /// The signals as last given.
    #[must_use]
    pub fn engine(&self) -> EngineSignals {
        self.engine
    }

    /// What this controller has done.
    #[must_use]
    pub fn counts(&self) -> CursorCounts {
        self.counts
    }

    /// **Set the window to match**, and answer the line to say about it, if a change is due one:
    /// at most one per [`SAY_EVERY`], naming how many changes went unsaid since the last.
    ///
    /// A hold the window declines (it has no focus yet, as far as the host knows) is not an error:
    /// it is asked again on the next call.
    ///
    /// # Errors
    ///
    /// The window's refusal, naming the operation. The cursor is then shown and let go, as far as
    /// the window allows.
    pub fn apply(&mut self, host: &mut dyn CursorHost, now: Instant) -> Result<Option<String>, String> {
        let facts = HostFacts { focused: host.focused(), minimized: host.minimized() };
        let want = host_cursor(self.engine, facts);
        let mut changes: Vec<String> = Vec::new();
        let why_held = || match (self.engine.view_captured, self.engine.lock) {
            (true, _) => "vk.e holds Android's pointer capture".to_string(),
            (false, lock) => format!("the engine holds its cursor still ({})", lock.name()),
        };
        let why_free = || {
            if !facts.focused {
                "the window lost the focus".to_string()
            } else if facts.minimized {
                "the window is minimised".to_string()
            } else {
                format!("the engine's cursor moves again ({})", self.engine.lock.name())
            }
        };
        if want.held && !host.has_pointer_capture() {
            match host.set_pointer_capture(true) {
                Ok(true) => {
                    self.counts.holds += 1;
                    changes.push(format!("held -- {}", why_held()));
                }
                Ok(false) => {}
                Err(error) => return Err(self.fail(host, format!("holding the cursor: {error}"))),
            }
        } else if !want.held && host.has_pointer_capture() {
            // Put the cursor under the engine's first, while it is still held and invisible: it
            // reappears there, and the window's report of where it is is no move.
            if let Some((x, y)) = self.pointer {
                if let Err(error) = host.warp_pointer(x, y) {
                    return Err(self.fail(host, format!("moving the cursor to the engine's: {error}")));
                }
            }
            if let Err(error) = host.set_pointer_capture(false) {
                return Err(self.fail(host, format!("letting the cursor go: {error}")));
            }
            self.counts.releases += 1;
            changes.push(format!("let go -- {}", why_free()));
        }
        if want.hidden != self.hidden {
            if let Err(error) = host.set_cursor_hidden(want.hidden) {
                return Err(self.fail(host, format!("hiding the cursor: {error}")));
            }
            self.hidden = want.hidden;
            if want.hidden {
                self.counts.hides += 1;
                changes.push(if self.engine.draws_own_cursor {
                    "hidden -- the engine draws its own over its view".to_string()
                } else {
                    format!("hidden -- {}", why_held())
                });
            } else {
                self.counts.shows += 1;
                changes.push(format!("shown -- {}", why_free()));
            }
        }
        if changes.is_empty() {
            return Ok(None);
        }
        let due = self.said_at.is_none_or(|at| now.saturating_duration_since(at) >= SAY_EVERY);
        if !due {
            self.unsaid += changes.len() as u64;
            return Ok(None);
        }
        let earlier = std::mem::take(&mut self.unsaid);
        self.said_at = Some(now);
        let mut line = format!("host cursor {}", changes.join("; "));
        if earlier > 0 {
            line.push_str(&format!(" ({earlier} change(s) since the last line not said)"));
        }
        Ok(Some(line))
    }

    /// Show the cursor and let it go, whatever the rule says: the session is ending, or the mouse
    /// is gone. Best effort -- nothing is left to report a refusal to.
    pub fn give_back(&mut self, host: &mut dyn CursorHost) {
        if host.has_pointer_capture() && host.set_pointer_capture(false).is_ok() {
            self.counts.releases += 1;
        }
        if self.hidden && host.set_cursor_hidden(false).is_ok() {
            self.counts.shows += 1;
        }
        self.hidden = false;
    }

    fn fail(&mut self, host: &mut dyn CursorHost, why: String) -> String {
        self.give_back(host);
        why
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREE: LockState = LockState::Free;
    const CENTER: LockState = LockState::Center;
    const CURRENT: LockState = LockState::CurrentPosition;

    fn engine(draws: bool, lock: LockState, view: bool) -> EngineSignals {
        EngineSignals { draws_own_cursor: draws, lock, view_captured: view }
    }

    fn host(focused: bool, minimized: bool) -> HostFacts {
        HostFacts { focused, minimized }
    }

    /// **The whole table**: engine signal x focus x minimised -> the host cursor. Every
    /// combination, each asserted against a spelled-out expectation rather than the rule's own
    /// arithmetic.
    #[test]
    fn the_rule_over_every_combination() {
        for draws in [false, true] {
            for lock in [FREE, CENTER, CURRENT] {
                for view in [false, true] {
                    for focused in [false, true] {
                        for minimized in [false, true] {
                            let got = host_cursor(engine(draws, lock, view), host(focused, minimized));
                            let still = view || lock == CENTER || lock == CURRENT;
                            let expected = if minimized {
                                HostCursor { hidden: false, held: false }
                            } else if !focused {
                                // Never held; hidden while the engine draws, as over any window.
                                HostCursor { hidden: draws, held: false }
                            } else {
                                HostCursor { hidden: draws || still, held: still }
                            };
                            assert_eq!(
                                got, expected,
                                "draws {draws}, lock {lock:?}, view {view}, focused {focused}, minimised {minimized}"
                            );
                        }
                    }
                }
            }
        }
        // The rows the owner named, spelled out.
        let front = host(true, false);
        assert_eq!(host_cursor(engine(true, FREE, false), front), HostCursor { hidden: true, held: false }, "hover");
        assert_eq!(host_cursor(engine(true, CURRENT, false), front), HostCursor { hidden: true, held: true }, "right-drag");
        assert_eq!(host_cursor(engine(true, CENTER, true), front), HostCursor { hidden: true, held: true }, "shift-lock");
        assert_eq!(host_cursor(engine(false, FREE, false), front), HostCursor::default(), "nothing drawn yet");
        assert_eq!(
            host_cursor(engine(true, CENTER, true), host(false, false)),
            HostCursor { hidden: true, held: false },
            "alt-tab: free, and hidden only where the engine draws its own"
        );
        assert_eq!(
            host_cursor(engine(true, FREE, false), host(false, false)),
            HostCursor { hidden: true, held: false },
            "w33: the pointer over the unfocused window, the engine's cursor following it"
        );
        assert_eq!(host_cursor(engine(true, CURRENT, false), host(true, true)), HostCursor::default(), "minimised");
    }

    #[test]
    fn the_lock_words_are_the_three_the_engine_writes() {
        assert_eq!(LockState::from_word(0), Some(FREE));
        assert_eq!(LockState::from_word(1), Some(CENTER));
        assert_eq!(LockState::from_word(2), Some(CURRENT));
        for word in [3, 4, 0xFFFF_FFFF] {
            assert_eq!(LockState::from_word(word), None, "{word}");
        }
    }

    /// A window as the controller sees one, recording every call.
    #[derive(Default)]
    struct FakeHost {
        hidden: bool,
        captured: bool,
        focused: bool,
        minimized: bool,
        calls: Vec<String>,
    }

    impl CursorHost for FakeHost {
        fn focused(&self) -> bool {
            self.focused
        }
        fn minimized(&self) -> bool {
            self.minimized
        }
        fn set_cursor_hidden(&mut self, hidden: bool) -> Result<(), String> {
            self.calls.push(format!("hidden {hidden}"));
            self.hidden = hidden;
            Ok(())
        }
        fn set_pointer_capture(&mut self, captured: bool) -> Result<bool, String> {
            self.calls.push(format!("capture {captured}"));
            // As every backend: granted only with the focus, released always.
            self.captured = captured && self.focused;
            Ok(self.captured)
        }
        fn has_pointer_capture(&self) -> bool {
            self.captured
        }
        fn warp_pointer(&mut self, x: i32, y: i32) -> Result<(), String> {
            self.calls.push(format!("warp {x} {y}"));
            Ok(())
        }
    }

    /// **A session, turn by turn**: hidden once the engine draws, focus or not; held for the
    /// right-drag and let go after it; `Alt+Tab` lets go (the window ends its own capture, as the
    /// backends do) and leaves it hidden over the view, and the focus coming back takes it again
    /// while the engine still holds; minimised gives everything back; and every change is counted.
    #[test]
    fn a_session_hides_holds_and_gives_back_with_the_focus() {
        let mut fake = FakeHost::default();
        let mut cursor = CursorController::new();
        let t0 = Instant::now();
        let turn = |cursor: &mut CursorController, fake: &mut FakeHost, seconds: u64| {
            cursor.apply(fake, t0 + Duration::from_secs(seconds)).expect("applied")
        };
        cursor.set_draws_own_cursor(true);
        let said = turn(&mut cursor, &mut fake, 0).expect("said");
        assert!(said.contains("hidden") && said.contains("draws its own"), "no focus yet, and hidden: {said}");
        assert!(fake.hidden && !fake.captured);

        fake.focused = true;
        assert_eq!(turn(&mut cursor, &mut fake, 1), None, "the focus changes nothing while nothing is held");

        // The right-drag: the engine writes 2, then 0.
        cursor.set_lock(Some(CURRENT));
        let said = turn(&mut cursor, &mut fake, 2).expect("said");
        assert!(said.contains("held") && said.contains("LockCurrentPosition"), "{said}");
        assert!(fake.captured && fake.hidden);
        cursor.set_lock(Some(FREE));
        let said = turn(&mut cursor, &mut fake, 3).expect("said");
        assert!(said.contains("let go"), "{said}");
        assert!(!fake.captured && fake.hidden, "let go, still hidden: the engine still draws its own");

        // Shift-lock: the engine writes 1 and vk.e asks; then the user alt-tabs away.
        cursor.set_lock(Some(CENTER));
        cursor.set_view_captured(true);
        turn(&mut cursor, &mut fake, 4);
        assert!(fake.captured);
        fake.focused = false;
        fake.captured = false; // the window ends its own capture with the focus
        cursor.observe(&WindowEvent::PointerCaptureLost);
        cursor.observe(&WindowEvent::FocusChanged { focused: false });
        assert_eq!(turn(&mut cursor, &mut fake, 5), None, "nothing left to change");
        assert!(fake.hidden && !fake.captured, "alt-tab: free, still hidden over the view (w33)");
        assert!(!cursor.engine().view_captured, "the view's capture ended with the focus");
        // Back: the engine still holds (state 1), so it is taken again without vk.e asking.
        fake.focused = true;
        turn(&mut cursor, &mut fake, 6);
        assert!(fake.captured && fake.hidden, "taken again with the focus");

        // Minimised: given back; restored: taken again.
        fake.minimized = true;
        turn(&mut cursor, &mut fake, 7);
        assert!(!fake.captured && !fake.hidden);
        fake.minimized = false;
        turn(&mut cursor, &mut fake, 8);
        assert!(fake.captured && fake.hidden);

        cursor.give_back(&mut fake);
        assert!(!fake.captured && !fake.hidden, "the session's end gives everything back");
        let counts = cursor.counts();
        assert_eq!((counts.hides, counts.shows), (2, 2), "{counts:?}");
        assert_eq!((counts.holds, counts.releases), (4, 3), "{counts:?}: alt-tab's release was the window's own");
        assert_eq!((counts.lock_center, counts.lock_current_position), (1, 1));
    }

    /// **A hold is let go where the engine has its cursor** (w33's flash at every let-go of the
    /// right-drag camera): the host's cursor is moved there while still held -- and so invisible
    /// -- and only then given back, and it is never shown in between. Without a position (nothing
    /// told yet) it is given back where it was held.
    #[test]
    fn a_hold_is_let_go_under_the_engines_cursor_and_never_shown_on_the_way() {
        let mut fake = FakeHost { focused: true, ..FakeHost::default() };
        let mut cursor = CursorController::new();
        cursor.set_draws_own_cursor(true);
        cursor.set_lock(Some(CURRENT));
        cursor.apply(&mut fake, Instant::now()).unwrap();
        assert!(fake.captured && fake.hidden);
        fake.calls.clear();
        cursor.set_pointer(Some((412, 300)));
        cursor.set_lock(Some(FREE));
        cursor.apply(&mut fake, Instant::now()).unwrap();
        assert_eq!(fake.calls, ["warp 412 300", "capture false"], "moved while held, then let go; never shown");
        assert!(fake.hidden && !fake.captured);

        fake.calls.clear();
        cursor.set_pointer(None);
        cursor.set_lock(Some(CURRENT));
        cursor.apply(&mut fake, Instant::now()).unwrap();
        cursor.set_lock(Some(FREE));
        cursor.apply(&mut fake, Instant::now()).unwrap();
        assert_eq!(fake.calls, ["capture true", "capture false"], "no position: no move");
    }

    /// **Said at most once a second**, and the line after a quiet spell names what went unsaid.
    #[test]
    fn changes_are_said_at_most_once_a_second() {
        let mut fake = FakeHost { focused: true, ..FakeHost::default() };
        let mut cursor = CursorController::new();
        cursor.set_draws_own_cursor(true);
        let t0 = Instant::now();
        assert!(cursor.apply(&mut fake, t0).unwrap().is_some());
        for (step, lock) in [CURRENT, FREE, CURRENT, FREE].into_iter().enumerate() {
            cursor.set_lock(Some(lock));
            let at = t0 + Duration::from_millis(100 * (step as u64 + 1));
            assert_eq!(cursor.apply(&mut fake, at).unwrap(), None, "within the second: counted, not said");
        }
        cursor.set_lock(Some(CURRENT));
        let said = cursor.apply(&mut fake, t0 + Duration::from_millis(1100)).unwrap().expect("due");
        assert!(said.contains("4 change(s)"), "{said}");
        assert_eq!(cursor.counts().holds, 3);
        assert_eq!(cursor.counts().releases, 2);
    }

    /// A lock state that cannot be read is `Free`: the cursor is never held on a guess.
    #[test]
    fn an_unreadable_lock_holds_nothing() {
        let mut fake = FakeHost { focused: true, ..FakeHost::default() };
        let mut cursor = CursorController::new();
        cursor.set_lock(Some(CURRENT));
        cursor.apply(&mut fake, Instant::now()).unwrap();
        assert!(fake.captured);
        cursor.set_lock(None);
        cursor.apply(&mut fake, Instant::now()).unwrap();
        assert!(!fake.captured);
        assert_eq!(cursor.counts().lock_reads, 1);
    }

    /// Encoders for the instructions the decoder reads, so the fixtures below are what they say.
    fn bl(from: usize, to: usize) -> u32 {
        0x9400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x03FF_FFFF)
    }
    fn b(from: usize, to: usize) -> u32 {
        0x1400_0000 | (((to as i64 - from as i64) / 4) as u32 & 0x03FF_FFFF)
    }
    fn ldr_x(rt: u32, rn: u32, offset: u32) -> u32 {
        0xF940_0000 | ((offset / 8) << 10) | (rn << 5) | rt
    }
    fn ldr_w(rt: u32, rn: u32, offset: u32) -> u32 {
        0xB940_0000 | ((offset / 4) << 10) | (rn << 5) | rt
    }
    const NOP: u32 = 0xD503_201F;

    /// **The modified 2.739.691 build's shape**: `bl getter; bl handler`, and in the handler
    /// `mov x19, x0` ...
    /// `ldr x8, [x19, #0xb00]; add x0, sp, #8; ldr w8, [x8, #0x88]; cmp w8, #1; cset w19, eq`
    /// (`0x2e73f04`-`0x2e73f38`, the words as the library has them). The real library is decoded
    /// by the gate's own test, `the_mouse_lock_state_is_found_in_the_loaded_library`.
    #[test]
    fn the_decoder_finds_the_getter_and_the_two_offsets() {
        let native = 0x2bd_875c_usize;
        let (getter, handler) = (0x234_3bc8_usize, 0x2e7_3ef0_usize);
        let mut body = vec![NOP; 32];
        body[9] = bl(native + 36, getter);
        body[10] = bl(native + 40, handler);
        let mut code = vec![NOP; 48];
        code[5] = 0xAA00_03F3; // mov x19, x0
        code[14] = ldr_x(8, 19, 0xb00);
        code[15] = 0x9100_23E0; // add x0, sp, #8
        code[16] = ldr_w(8, 8, 0x88);
        code[17] = 0x7100_051F; // cmp w8, #1
        code[18] = 0x1A9F_17F3; // cset w19, eq
        let words_at = |at: usize, count: usize| -> Option<Vec<u32>> {
            let (base, words) = if at == native { (native, &body) } else if at == handler { (handler, &code) } else { return None };
            let start = ((at - base) / 4) as usize;
            words.get(start..start + count).map(<[u32]>::to_vec)
        };
        assert_eq!(
            LockLocation::decode(&words_at, native),
            Ok(LockLocation { getter, handler, window_offset: 0xb00, state_offset: 0x88 })
        );

        // A handler behind a shim is followed.
        let shim = 0x100_0000_usize;
        let shimmed = |at: usize, count: usize| -> Option<Vec<u32>> {
            if at == native {
                let mut body = body.clone();
                body[10] = bl(native + 40, shim);
                return Some(body[..count].to_vec());
            }
            if at == shim {
                return Some(vec![b(shim, handler)]);
            }
            words_at(at, count)
        };
        assert_eq!(LockLocation::decode(&shimmed, native).map(|l| l.handler), Ok(handler));

        // Each missing piece is named: no `cset eq` after the compare, no load of the singleton.
        let mut no_cset = code.clone();
        no_cset[18] = NOP;
        let missing = |code: Vec<u32>| {
            let words_at = |at: usize, count: usize| -> Option<Vec<u32>> {
                if at == native { Some(body[..count].to_vec()) } else if at == handler { Some(code[..count].to_vec()) } else { None }
            };
            LockLocation::decode(&words_at, native).unwrap_err()
        };
        assert!(missing(no_cset).contains("cset"));
        let mut other_base = code.clone();
        other_base[14] = ldr_x(8, 20, 0xb00);
        assert!(missing(other_base).contains("singleton"));
        let mut no_compare = code;
        no_compare[17] = NOP;
        assert!(missing(no_compare).contains("cmp"));
    }
}
