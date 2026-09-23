# Brief: keyboard and mouse into the game, the way a device with a hardware keyboard and mouse gets them

Written 2026-09-23 night. The owner joined a game world (Pet Simulator 99) on this layer and
reports: **"keyboard and mouse controls do not reach the game -- WASD, Tab, the mouse camera; only
clicks/touch work."** The goal: the host window's keys, mouse buttons, movement and wheel reach the
engine the way an Android device with a hardware keyboard and mouse delivers them (Sober -- the
x86-64 Android client on Linux -- works this way), **not as hardcoded actions**. Roblox's Android
client handles WASD, Space, Tab, Esc, the right-drag camera and the wheel zoom itself once the
events arrive through its own Java-to-native path.

Read `docs/HANDOFF.md` from "# START HERE" and `docs/VERIFICATION.md` first -- entries 20 and 21
are about input specifically (a unit test that built its own input instead of the caller's; taps
aimed from one layout landing on another). Trust runs over summaries, including this one.

## What already exists -- read it before writing anything

* **`crates/omni-android/src/jni/keys.rs` -- the key path is DECODED and BUILT, opt-in.**
  `MainGameActivity.onKeyDown/onKeyUp` -> `vk.g` -> `NativeGLInterface.nativePassKeyEvent(ZIIZ)V`
  (`0x02baebdc`): `(down, scanCode, keyCode, repeat)`. The engine keys on the **Linux scan code**
  (table at `0x6e6414` -> HID usage -> Roblox KeyCode), never the Android key code; `KEY_LAYOUT`
  is AOSP's `Generic.kl`; `EXTENDED` maps Windows' E0 keys. `vk.g` passes keys only when
  `Configuration.keyboard == KEYBOARD_QWERTY` and `hardKeyboardHidden == NO`, which
  `declare_hardware_keyboard` states before step 13. **The gate turns it on only with
  `OMNI_HARDWARE_KEYBOARD=1`, and the owner's play sessions never set it** -- so in the world the
  keys went nowhere (or only to an open text field, `jni/text.rs`). Whether the path works in the
  world is **not measured**: that is the first thing to find out (the controller may already have
  a session result; it will be in the launch message if so).
* **`crates/omni-android/src/jni/input.rs` -- touch.** `vk.e.onTouch` ->
  `NativeInputInterface.nativePassInput(IFFIII)V` (`0x02bbba88`). `HostFinger` presents the host's
  **primary button only** as a finger; the secondary, middle and extended buttons, hover moves and
  the wheel are dropped. Its module docs say: *"Mouse-source events (`getSource() & 8194`) take
  `vk.e.y`, not this path."* That path is **not decoded**.
* **`crates/omni-platform/src/window/`** -- the host window seam (Win32 in `windows.rs`).
  `WindowEvent`: `PointerMoved`, `PointerDown/Up {button}`, `KeyDown {keycode, scancode, repeat}`,
  `KeyUp`, `Text`, `Resized`, `CloseRequested`. **No wheel, no relative/raw mouse motion, no pointer
  capture or cursor hiding.**
* The engine exports these mouse natives (`docs/research/jni-surface-lists.txt`, Section for
  `NativeInputInterface`, and the caller list around line 2084-2112):
  `nativePassMouseButton (FFZI)V`, `nativePassMouseMove (FFFF)V`, `nativePassMouseWheel (FFF)V`,
  `nativePassMousePan (FFFF)V`, `nativePassMousePinch (FFF)V`,
  `nativeGetMainWindowIsMouseLockedCenter ()Z`. Callers seen: `vk.e.y` -> Button, Move, Wheel;
  `vk.e.z` -> Move; `vk.e.onTouch` -> Wheel; `a` -> MousePinch.
* `tests/gameactivity.rs` is the gate that runs the APK in a real window; its session loop
  delivers each window event to the text field, the touch seam and the key seam (search for
  `seam.deliver`). Switches: `OMNI_HARDWARE_KEYBOARD`, `OMNI_LATE_TAP`, `OMNI_LATE_INPUT`,
  `OMNI_LATE_TEXT`, `OMNI_INPUT_PROBE`; see the top of the file and the handoff.

## The job

1. **Decode, from `classes2.dex` and `libroblox.so`, what a device does with a mouse** -- before
   building anything:
   * `vk.e.y` and `vk.e.z`: which `MotionEvent`s reach them (`onTouch` with a mouse source?
     `onGenericMotionEvent` for `ACTION_HOVER_MOVE` and `ACTION_SCROLL`?), and exactly what each
     passes to each native: coordinates (divided by density like touch?), the button int's
     meaning (`MotionEvent.getButtonState()` bits? an index?), the boolean, wheel axes
     (`AXIS_VSCROLL`/`AXIS_HSCROLL` sign and scale), the four floats of `nativePassMouseMove`
     (position and delta?).
   * **Mouse lock**: who polls `nativeGetMainWindowIsMouseLockedCenter`, and what the Java side does
     when it is true (`View.requestPointerCapture()` / `onCapturedPointerEvent` with relative axes,
     cursor hiding). The right-drag camera and shift-lock/first-person depend on it.
   * How the engine learns a mouse/keyboard exists (`UserInputService.MouseEnabled`/
     `KeyboardEnabled`): JNI queries of `InputDevice`/`InputManager`/`Configuration`, or only from
     events received. Whatever it reads, answer it as a device with this host's keyboard and mouse
     would -- a real fact of this host, stated by the embedding.
   * The native side of each mouse native where the Java side leaves questions (what it reads,
     which queue it posts to, which thread a device calls it on).
   Write the decode down (module docs, as `keys.rs` and `input.rs` do, with addresses).
2. **Build it in the window seam and the Java-side model.**
   * `omni-platform` window seam: the wheel (`WM_MOUSEWHEEL`, `WM_MOUSEHWHEEL`), hover moves with no
     button held, all buttons, and -- if the lock decode says so -- pointer capture: hide and confine
     the cursor and deliver **relative** motion (raw input `WM_INPUT`, or whatever is faithful and
     robust) while the engine has the mouse locked, released when it lets go and when the window
     loses focus. `cfg(target_os)` only inside `omni-platform`; the Linux/macOS files keep compiling
     with typed "unsupported" answers.
   * `omni-android`: a mouse model (`jni/mouse.rs` or in `input.rs`) that transcribes `vk.e.y`/`z`
     the way `TouchListener` transcribes `vk.e.onTouch`, and the routing a device performs: a mouse
     is a mouse, not a finger. **Clicks on the engine's UI must keep working** (Sign In on the
     landing screen, the Play button) -- verify a mouse click navigates before making the mouse
     path the default, and keep touch available only where a device would use it.
   * **Make the hardware keyboard and mouse the play configuration** (the host has both; that is a
     fact of this host). Keep the phone configuration reachable for the gate's measured default if
     other tests depend on it, and say which.
3. **Verify in the real engine.** On the landing screen (no sign-in needed): a mouse click on Sign
   In navigates (`APP_READY(Login)`), keys reach `nativePassKeyEvent`, the wheel and moves reach
   their natives without refusals or deaths. In the world (needs a signed-in data directory -- the
   controller will provide a cleanly-closed one to COPY per run, never to run in place, one run at
   a time, ending every run cleanly): WASD moves the character, the right-drag turns the camera,
   the wheel zooms, Tab does what Tab does. What cannot be verified without the owner, say so; the
   owner tests it in the next play session.
4. Tests whose input comes from the real caller (entry 20) -- the window seam's own events, the
   gate's routing -- and mutation rows for each decoded fact that a wrong constant would break.

## Constraints

* **Work in the worktree and target directories named in the launch message**, never in the main
  checkout (VERIFICATION 18). Set `OMNIDROID_DYNARMIC_BUILD_DIR` to the dynarmic build dir you are
  given (MSVC fails on long paths) and `CARGO_TARGET_DIR` to your worktree's own `target`.
* **Files you own** are listed in the launch message; do not edit anything else. The other agent
  (performance) owns the CPU, bionic, Vulkan and graphics code and `omni-android/src/lib.rs`; you
  own `tests/gameactivity.rs` -- keep your changes there to input, and do not reorganise it.
* `tools/mutate.py` is shared: add rows only by inserting before the list terminator (never
  slicing), with your own prefix (`mouse-`, `kbd-`, ...; check they are free), and run only
  `--only <your-prefix>`, never while editing, never while one of your gate runs is going. The
  controller merges the file.
* **Memory is the scarce resource on this host** (31.8 GB RAM, ~48.6 GB commit limit, most held by
  the owner's apps; a build during a session once exhausted it and killed guest threads). One gate
  run at a time; build with `CARGO_BUILD_JOBS=6`; check free commit before a gate run
  (`(Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory`) and wait if under ~6 GB. On a 1455
  or os error 112 (disk), stop and report.
* **If the controller tells you the owner is in a session, stop building and running until told
  otherwise.**
* No plausible stubs, never fake success: missing behaviour fails clearly, naming the symbol,
  address and context. Host-supplied values must be read from the APK, decoded from the binary,
  true of this host, or supplied by the embedding.
* No commits by you. Keep the tree compiling. Windows x86-64 host only. No Vulkan validation layers.
* Scratch tools from the previous session, at
  `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps-omnidroid\a4b3c2d9-3e3f-42b2-a30d-3fdb78dfe571\scratchpad`:
  `dexdis.py`, `dexgrep.py`, `dexclass.py`, `dexdump.py` (dex), `a64.py <hex> <count>` (capstone at
  link addresses), `blcallers.py`, `anyref.py`, `addrref.py`, `dynsym.py`, `gotslot.py`,
  `shoot_both.ps1` (composed-screen capture of the window). Read each before trusting it.

## Report

The decode (with addresses and dex offsets), what was built, files changed, tests run (verbatim,
whole affected suites), mutation rows and results, what was verified in the engine (landing / world)
and what only the owner can verify, and anything left. MEASURED versus inferred, precisely.
