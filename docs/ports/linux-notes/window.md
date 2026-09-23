# Window and graphics on Linux (worker: gfx, branch `lnx-gfx`)

Every result below was measured on the port's machine (Ubuntu 26.04, i5-4460) against **its own
Xvfb servers**, never the owner's session: `Xvfb :92 -screen 0 1920x1080x24` (no window manager)
and `Xvfb :93 -screen 0 1920x1080x24` with `xfwm4 --compositor=off` on it. The only Vulkan device
is **Mesa lavapipe** (`llvmpipe (LLVM 21.1.8, 256 bits)`, `VK_PHYSICAL_DEVICE_TYPE_CPU`): every
graphics result is a correctness result, and no frame count or time below says anything about
speed.

## What was built

* `crates/omni-platform/src/window/linux.rs` (+ `linux/keymap.rs`, `linux/decode.rs`): an **X11
  backend through Xlib**, loaded with `dlopen` (`x11-dl` 2.21, MIT), one connection per window.
  It runs on Xorg, Xvfb and Xwayland (so on a Wayland desktop too). Native Wayland: not written.
* **Why Xlib, not xcb**: typed text needs an input method, and Xlib has one (`XOpenIM`/`XCreateIC`/
  `Xutf8LookupString`: layout, dead keys, compose, IME commits); XKB's
  `XkbSetDetectableAutoRepeat` makes auto-repeat a run of presses; and `omni-gfx`'s probe list
  already prefers `VK_KHR_xlib_surface`, so the guest's surface rewrite and the surface made
  agree. xcb would need libxkbcommon + a compose table of our choosing and still no IME.
* Events, against the Windows contract: `WM_DELETE_WINDOW` advertised -> `CloseRequested`
  (never a killed connection); `ConfigureNotify` + ICCCM `WM_STATE` -> `Resized` in pixels, 0x0
  while iconic; focus filtered of grab/pointer-root bookkeeping; buttons 1/2/3/8/9 by role; the
  wheel from core buttons 4-7 at 120 a notch (4 = away = `+dy`, 7 = right = `+dx`);
  `KeyDown.keycode` = the key's level-0 keysym (`XK_a` with or without Shift, as `VK_A`),
  `scancode` = the **set-1 code of the physical key** (X keycode - 8 = Linux input code, then the
  inverse of `keys.rs`'s table: 1-83 and 86-88 as is, Num Lock `0xE045`, Pause `0x45`, the `E0`
  block; 0 for keys `keys.rs` has no code for, and 0 for every key on a keymap whose keycodes are
  not `evdev`); repeats; `Text` per character with `WindowEvent::Text`'s control rule; raw
  relative motion (XInput 2.2 `XI_RawMotion`, absolute-mode devices differenced) under a capture
  that is `XGrabPointer` confined to the window with a blank cursor; `wait()` polls the
  connection's descriptor; DPI = `Xft.dpi` from the root's `RESOURCE_MANAGER` (the user's scale),
  else the screen's pixels per mm (Xvfb: 1920 px / 488 mm = 100). `set_minimized` refuses by
  name without a window manager (ICCCM gives iconic state to the manager).
* `omni-gfx`: Xlib arms beside every Win32 one (renderer instance extension, surface, claim key,
  the guest's `vkCreateAndroidSurfaceKHR` -> `vkCreateXlibSurfaceKHR`). On Xlib the window's 0x0
  means no swapchain, because an iconified X window keeps its geometry and Mesa reports it as
  `currentExtent` (MEASURED 640x480).

## Measured along the way (each is in a doc comment where the code depends on it)

| finding | method | n |
|---|---|---|
| XTEST pointer and keyboard are `XIModeRelative`; `xdotool mousemove_relative` produces `XI_RawMotion` with the deltas as `raw_values`, absolute `mousemove` produces none | XIQueryDevice + XI2 probe on :92 | 1 run each |
| Raw motion keeps arriving at the confinement edge (`mousemove_relative 900 900` from (120,90) in a 400x300 window: pointer stays at (399,299), `PointerMotion {900, 900}`) | probe through the seam on :92 | 1 |
| **Another client's failed `XGrabPointer` (AlreadyGrabbed, no `confine_to`) lifts the holder's confinement**; re-grabbing restores it | C probe on :92 | 1 (deterministic) |
| The built-in input method rewrites the key completing a compose sequence to keycode 0 inside `XFilterEvent` | live test, AZERTY `^` + `e` | every run |
| An Xvfb **resets** (keymap, root background, resources) when its last client disconnects: `setxkbmap`/`xmodmap`/`xsetroot` from a lone shell have no lasting effect on a bare Xvfb | xmodmap on :92 vs :93 (xfwm4 connected) | 3 |
| xfwm4 iconifies a window it has not managed yet (`XIconifyWindow` straight after `XMapRaised`: `WM_STATE` = 3) | C probe on :93 | 3/3 |
| A minimise before `show`'s activation reached the manager was undone by it (focus lost, regained, no iconic state; 14,613 frames presented to a window meant to be minimised in 10 s) | `renderer_linux_wm` before the fix | 1 |
| Mesa lavapipe's X11 surface reports an iconified window's geometry as `currentExtent` | `renderer_live` minimise on :93 before the fix | 2 |

## Tests (process exit codes)

Every run is `--release`, one process per line, verdict = the process exit code. `:92` = Xvfb
with no window manager, `:93` = Xvfb + xfwm4. Runs with `OMNI_GFX_WINDOW_TESTS=1` and
`-- --ignored --test-threads=1` where gated.

| target | display | result | exit |
|---|---|---|---|
| `omni-platform --lib window` (keymap, decode, focus rules, push_event) | none | 20/20 | 0 |
| `omni-platform --test window_seam` | none | 7/7 | 0 |
| `omni-platform --test window_linux_nodisplay` | none (a dead `:187`) | 1/1 | 0 |
| `omni-platform --test window_linux` (xdotool/XTEST input) | :92 | 12/12 | 0 |
| `omni-platform --test window_linux_wm` | :93 | 5/5 | 0 |
| `omni-platform --test window_live` (the Windows file) | :92 and :93 | 10/11 each; the failure is `the_raw_handle_is_a_live_win32_handle`, which asserts a Win32 handle | 101 |
| `omni-gfx --test renderer_linux` (**framebuffer capture**) | :92 | 1/1 | 0 |
| `omni-gfx --test renderer_linux_wm` | :93 | 1/1 | 0 |
| `omni-gfx --test renderer_live` | :93 | 8/9; fails `the_renderer_picks_a_genuine_device...`, which refuses a CPU device (lavapipe) by design | 101 |
| `omni-gfx --test renderer_live` | :92 | 7/9; also the minimise test, refused by name with no window manager | 101 |
| `omni-android --test keys_linux` | none | 2/2 | 0 |
| `omni-android --test vulkan_present` (guest ARM64 code, merged `lnx-mem`) | :92 and :93 | 8/9 each; fails `the_real_drivers_pipeline_cache_is_saved...`: lavapipe's cache does not grow when a pipeline is built through it | 101 |
| `omni-android --test ndk_host_window` | :93 | 2/2 | 0 |
| `omni-android --test ndk_host_window` | :92 | 1/2; the minimise test, no window manager | 101 |
| `omni-android --test vulkan_instance` | :92 | 2/2 | 0 |
| `omni-android --test vulkan_device` | :92 | 0/1: asserts `RewriteSite::SurfaceCall { system: "win32" }` literally; on Linux the log it prints is `VK_KHR_android_surface -> VK_KHR_xlib_surface`, `vkCreateAndroidSurfaceKHR -> vkCreateXlibSurfaceKHR`, `SurfaceCall { system: "xlib" }` -- the pairing correct, the literal Windows' | 101 |

**Pixels.** `renderer_linux`: after `xsetroot -solid '#3366CC'` a 64x64 capture of empty screen
is 4096/4096 that colour (the instrument on a known answer, VERIFICATION entry 19); then a
320x240 window cleared red and green is 76,800/76,800 exact pixels each, a four-quadrant RGBA8
image blitted 1:1 is 76,800/76,800 exact and exactly 4 distinct colours, and the root beside the
window is still the root colour. Method: ImageMagick `import -window root -crop` (a separate X
client) at the position `xwininfo` reports. The guest path (`vulkan_present`, translated ARM64
driving the host through `vkCreateXlibSurfaceKHR`) reads back the presented swapchain image:
centre and corners `[51, 153, 204, 255]` for the clear, and the four texels
`[32,96,160] [16,176,64] [200,48,16] [240,224,80]` at the triangle's quadrants.

## Mutation rows

`tools/lnx_rows/window.py`, run as `flock ~/odb/build.lock python3 tools/mutate_linux.py --only
lnx-win` / `--only lnx-gfx`, with both Xvfbs up (pre-flight: every pattern once, every command
passes on the clean tree). **lnx-win: 30/30 caught, lnx-gfx: 2/2 caught** on the final tree
(commit `a58411c` plus this notes file; one run of each table, n = 1 per row). History worth keeping: `lnx-win-A17` (show never asks for the focus) was NOT CAUGHT on
its first run through `window_live.rs`, because that file calls `show` twice and the second call
takes `show`'s other path; re-targeted at `window_linux.rs` it is caught by 9 tests.
`lnx-gfx-A1` (wrong instance extension) is caught by the test process dying rather than by a
named assertion (the harness reports "failed without naming a test"); calling
`vkCreateXlibSurfaceKHR` on an instance that never enabled it has no defined outcome, and here it
was the process.

## Shared edits (merge notes)

* `crates/omni-platform/src/window/mod.rs`: `RawWindow::Xlib { display: usize, window: u64 }` and
  its `system_name` arm (`"xlib"`); a Linux-only `pub use linux::keymap::scancode_from_evdev`
  (so `omni-android`'s round-trip test can call it).
* `crates/omni-platform/src/window/error.rs`: `WindowError::X11 { operation, api, detail }` --
  `LastError` is a `GetLastError` code, and an X failure is a protocol error with its request, or
  a reason with no number.
* `crates/omni-platform/src/window/unix.rs`: `#![cfg_attr(target_os = "linux", allow(dead_code))]`
  (Linux no longer uses the structural body; macOS still does).
* `crates/omni-platform/tests/window_seam.rs`: the structural-refusal test narrowed from
  `not(windows)` to `not(any(windows, linux))`.
* `crates/omni-platform/Cargo.toml`: `x11-dl = "2.21"` in the Linux target sections (dependency and
  dev-dependency); `Cargo.lock` gains `x11-dl` and `pkg-config`.
* `crates/omni-gfx/src/vulkan.rs`: `platform_extension(window)` feeds `create_instance` (the Win32
  row moved into a match arm, text unchanged), Xlib arms in `create_surface` and `window_key`,
  and `zero_size_is_the_windows` (false for Win32, so Windows behaviour is unchanged).
* `crates/omni-gfx/src/host.rs`: an Xlib arm in `create_platform_surface`; the refusal text of the
  wildcard arm names `VK_KHR_xlib_surface` too.
* `crates/omni-gfx/src/claim.rs`: `WindowKey::xlib(window)`.
* `crates/omni-gfx/src/error.rs`: `UnsupportedWindowSystem`'s message names both implemented
  systems.

## Open / not done

* **Native Wayland**: not written (X11 reaches Wayland desktops through Xwayland). Fractional
  scale and a zero-copy present are what it would add.
* **Shared tests that assert Windows literals** fail on Linux by design and are Windows-owned:
  `window_live.rs::the_raw_handle_is_a_live_win32_handle`, `vulkan_device.rs`'s
  `SurfaceCall { system: "win32" }`. Doc comments that still call the Linux window structural:
  `omni-platform/src/lib.rs`, `window/mod.rs` (`Window::new`'s errors), `window/unix.rs`'s
  header, `omni-android/src/ndk/host_window.rs`'s "Five targets" section.
* **Lavapipe-only results**: `renderer_live`'s genuine-device test and `vulkan_present`'s
  pipeline-cache test fail on a CPU driver; neither can be run meaningfully on this machine.
* **Minimising needs a window manager** (ICCCM); on a bare X server `set_minimized(true)` is a typed
  refusal, and the two minimise tests of the shared files fail there by that refusal.
* **Keycodes on a non-`evdev` keymap** (the old `xfree86` set) are reported as scancode 0 for every
  key -- honest but keyless for the engine; not met on Xvfb, Xorg+libinput or Xwayland.
* **The I/O error handler is Xlib's**: losing the X server ends the process (`exit(1)`), as for
  every Xlib client; `XSetIOErrorExitHandler` could change that and was not needed.
* **An Xvfb resets when its last client disconnects** (keymap, root colour, resources): tests hold
  a connection before changing server state. Starting Xvfb with `-noreset` avoids it.
* Unrelated failures seen in `cargo test -p omni-platform --release` on this branch after merging
  `lnx-mem`: `fs::tests::dev_urandom_is_a_device_that_fills_the_buffer_with_entropy` and
  `vm_seam.rs::structural_backends_report_unsupported` -- neither in this worker's files.
