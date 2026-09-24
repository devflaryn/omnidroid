# macOS port: the window, graphics-surface and audio seams

Workstream notes for the orchestrator to fold into `docs/ports/macos.md`. Branch `mac-window`, host
Apple M1, 16 GB, macOS 26.5, MoltenVK 1.4.2 + Vulkan loader 1.4.357 (Homebrew). Every figure below
carries its n and method; "MEASURED" means on this host, in the run named.

## What exists

| Seam | File(s) | State |
|---|---|---|
| window | `crates/omni-platform/src/window/macos.rs`, `macos/{main_thread,appkit,keys}.rs` | implemented, live-tested |
| audio | `crates/omni-platform/src/audio/macos.rs` | implemented, live-tested |
| Vulkan surface / loader / portability | `crates/omni-gfx/src/portability.rs`, `vulkan.rs`, `host.rs`, `claim.rs` | implemented, pixel-verified |

## Decisions, with the measurements behind them

### 1. The main thread is handed to AppKit before `main` (the orchestrator's design, implemented)

AppKit is main-thread-only; libtest never runs a test on the main thread (MEASURED:
`pthread_main_np() == 0` in a test, asserted again in
`window_macos.rs::a_window_is_made_from_a_thread_that_is_not_the_main_thread`); the gate creates and
polls its window on its test thread.

* A constructor in `__DATA,__mod_init_func` (`window/macos/main_thread.rs`) starts a pthread with the
  main thread's stack size (`getrlimit(RLIMIT_STACK)`, at least 8 MiB), which waits behind a gate,
  then calls the real `main(argc, argv, envp, apple)` and `exit`s with its value. The main thread
  runs any initializers after the constructor, opens the gate, and serves: a plain CFRunLoop (kept
  alive by a far-future timer) until the first window, then `NSApplication`'s event pump
  (`nextEventMatchingMask:… distantFuture` + `sendEvent:`), both of which drain the main dispatch
  queue.
* `main` is found from `LC_MAIN`'s `entryoff` and cross-checked against `dlsym(RTLD_MAIN_ONLY,
  "main")` when that answers. MEASURED with a prototype (scratch crate, debug and thin-LTO release,
  n=1 each): both give the same address.
* A `Window` is a `!Send` proxy: every AppKit call is `dispatch_sync_f` to the main queue (inline if
  already there); events are pushed on the main thread into an `Arc<Mutex<Vec>> + Condvar` with the
  seam's own `push_event` coalescing; `poll` drains without crossing threads; `wait` blocks on the
  condvar.
* **It declines** (returns to dyld, `main` stays on thread 0) when: not on the main thread; not
  linked into the main executable (`dladdr`); the executable is in `….app/Contents/MacOS/`; it
  cannot find itself in the single `__mod_init_func` (or the list is `__TEXT,__init_offsets`, a
  form never read); `LC_MAIN` is missing or disagrees with `dlsym`; `pthread_create` fails.
  `Window::new` then returns `WindowError::MainThreadUnavailable { why }` naming the reason — it
  never hangs and never creates a window off-main. Tested: the app-bundle refusal
  (`inside_an_app_bundle_the_window_is_refused_by_name`, the test binary copied into
  `X.app/Contents/MacOS/`).
* **Initializers after it are run by it**, in order, with dyld's arguments — the first version
  declined instead, which would have left a binary with C++ static initializers linked after
  omni-platform (dynarmic has three in its own test binaries) without a window. MEASURED
  (`otool -s __DATA_CONST __mod_init_func`): the linker places `window_macos.rs`'s own test
  initializer *after* the constructor; it runs exactly once, on the main thread
  (`another_initializer_runs_once_on_the_main_thread_before_main`), and all window tests pass in that
  binary.
* **What changes for ordinary binaries**: every macOS binary that links the window seam's object
  (all release builds, since `codegen-units = 1`) runs `main` on a pthread. Tested in a child
  process (`the_hand_over_keeps_exit_codes_and_panic_messages`): a passing libtest binary exits 0, a
  panicking test exits 101 and its message prints, `std::process::exit(7)` exits 7. MEASURED with
  the prototype: std still names the thread `main` and still catches a stack overflow on it
  (`thread 'main' has overflowed its stack`, exit 134, same as without the hand-over). Not
  preserved: dyld's own "main is being called" notification to a debugger.
* Rejected alternatives: requiring callers to be on the main thread (libtest and the gate never
  are); `harness = false` binaries (fixes this crate's tests only, not omni-gfx's, omni-android's or
  the embedding); a main-thread-affine seam type (right if an embedding ever owns its main thread,
  not needed by any caller today).

### 2. Keys: `kVK` in `keycode`, the set-1 code of the same physical key in `scancode`

`kVK_*` codes are layout-independent positions (HIToolbox `Events.h`). `keys.rs` maps each to the
Linux `KEY_*` of the same key (`input-event-codes.h`; where no PC key has the name, by the USB HID
usage the Mac keyboard sends: Help→`KEY_INSERT`, keypad Clear→`KEY_NUMLOCK`, ISO §→`KEY_102ND`,
contextual-menu→`KEY_COMPOSE`), then to set-1 by the exact inverse of
`omni_android::jni::keys::evdev_code`. `window_keys_macos.rs`, against the consumer itself:
102 kVKs round-trip to their Linux code; the 18 that carry scancode 0 (Fn, F13–F20, volume ×3,
keypad `=`, the five JIS keys) have **no** set-1 preimage in the consumer (searched over all 65,536
codes); `set1_of_linux` inverts the consumer over its whole domain (105 codes).
Modifiers arrive as `flagsChanged:`; down/up is the key's own device-dependent bit
(`NX_DEVICE*KEYMASK`, IOLLEvent.h); Caps Lock (a toggle flag) is reported as a down+up keystroke;
Fn is believed only for the Fn key. Command-key-up, which `-[NSApplication sendEvent:]` swallows,
is sent to its window directly by the pump. Text comes from `NSTextInputClient`
(`interpretKeyEvents:` → `insertText:replacementRange:`), one event per character, dropping C0,
DEL and Apple's function-key private-use range U+F700–U+F8FF. MEASURED by the mutation run:
through the keyboard, AppKit's input context never hands `insertText:` a control code (Return,
Escape, Tab, Control+Q, F13 type nothing — they become commands) and inserts no text for
Command+key; so the Command guard that once stood in `keyDown:` was unreachable and was deleted
(VERIFICATION entry 12), and the control-code filter is tested where it is reachable, by calling
`insertText:replacementRange:` as an input method does.

### 3. Wheel units and signs

`dy = deltaY × 120` (AppKit's line delta, the same field for notched and precise devices, carrying
fractions between events), signs made physical: undo `isDirectionInvertedFromDevice` ("natural"
scrolling), and negate the horizontal axis (AppKit's positive is left, as SDL's Cocoa backend also
reads it). MEASURED with CoreGraphics line-unit scroll events (n=3): wheel1 +1 → deltaY 1 →
`dy 120`; wheel1 −2 → `dy −240`; wheel2 +1 → deltaX 1 → `dx −120`; `inverted false` for synthesized
events. **Not measured**: what a physical wheel gives under natural scrolling (no test can turn a
wheel), so the physical-sign claim rests on Apple's documentation of the inversion flag.

### 4. Pointer capture: honest about acceleration

`CGAssociateMouseAndMouseCursorPosition(false)` + `[NSCursor hide]`, the cursor first warped into the
client area if outside; `PointerMotion` from `-[NSEvent deltaX/deltaY]` with sub-count remainders
carried. **These deltas are accelerated** (post-ballistics; AppKit has no unaccelerated figure — an
`IOHIDManager` reading the mouse would be needed). The seam asks for device counts; this is a gap,
stated in `appkit.rs`. Only granted to the key window of the active app; resigning key ends it with
`PointerCaptureLost` then `FocusChanged { false }`.

### 5. Pixels and DPI

Sizes are `convertRectToBacking:` of the view bounds; positions are points × `backingScaleFactor`,
y turned over from AppKit's bottom-left. `dpi() = round(96 × backingScaleFactor)` — the Windows
convention applied to the same fact, so the gate's `density = dpi / 96` is the backing scale.
MEASURED: backingScaleFactor 2, dpi 192 (external 2560×1440 display at "looks like" 1280×720).
**Odd pixel sizes on a 2× display round up**: asked for 641×481, AppKit made 642×482 (integral
points); `client_size` and the `Resized` event both say so (n=1).

### 6. Surface: `CAMetalLayer` via `makeBackingLayer`

Layer-backed view, `wantsUpdateLayer`, `contentsScale` kept equal to the backing scale on
`viewDidChangeBackingProperties`/`windowDidChangeBackingProperties` (MoltenVK sizes `currentExtent`
as bounds × contentsScale). `RawWindow::AppKit { ns_window, ns_view, ca_metal_layer }`.
**A miniaturised window's surface keeps its size**: MEASURED, MoltenVK reported 640×480
`currentExtent` for a minimised window and the renderer went on presenting (1,107 frames in 10 s).
The renderer now treats the window's own `Resized {0,0}` as authoritative (a no-op where both say
zero, i.e. on Win32).

### 7. Loader discovery

`omni_platform::window::vulkan_loader_candidates()`: empty on Windows/Linux (omni-gfx then calls
`Entry::load()`, unchanged); on macOS the leaf names, Homebrew's and `/usr/local`'s
`libvulkan.1.dylib`, then MoltenVK itself. MEASURED (scratch probe, n=1 each):
`dlopen("libvulkan.dylib")`/`("libvulkan.1.dylib")` fail (dyld searches `/usr/lib`, not
`/opt/homebrew/lib`); `/opt/homebrew/lib/libvulkan.1.dylib` loads; `/usr/local/lib/…` absent;
`/opt/homebrew/lib/libMoltenVK.dylib` loaded directly creates an instance and enumerates the M1
with no portability flag. Presenting through MoltenVK-as-loader was **not** measured. When nothing
loads, `GfxError::LoaderMissing` names every path with dyld's message.

### 8. Portability

MEASURED: without `VK_KHR_portability_enumeration` + `ENUMERATE_PORTABILITY_KHR`, the Homebrew loader
answers `VK_ERROR_INCOMPATIBLE_DRIVER`. Both the renderer and the guest-facing `GfxVulkanHost` create
the instance **as asked first** and retry with the extension and flag only after exactly that
answer, when the loader offers the extension (so a native driver's instance is never altered, and
the Windows loader, which also offers the extension, is not touched). Below API 1.1,
`VK_KHR_get_physical_device_properties2` is added when offered, so the subset's features can be
read. A device exposing `VK_KHR_portability_subset` gets it enabled (spec: must) with exactly the
features it reports chained into `vkCreateDevice` (in front of the guest's `pNext` chain in the host).

**MoltenVK 1.4.2's gaps on the Apple M1** (`portability_live.rs`, the renderer's report equal to an
independent 1.1 `vkGetPhysicalDeviceFeatures2` read; n=1): `pointPolygons`, `samplerMipLodBias`,
`tessellationIsolines`, `tessellationPointMode`. All other 11 subset features are supported.
Consequence to watch: a guest sampler with a non-zero `mipLodBias` is outside what the device
supports.

### 9. Audio

`AudioUnit` `kAudioUnitSubType_DefaultOutput`; the render callback pulls from a lock-free SPSC ring
of interleaved f32 and signals a dispatch semaphore once per cycle; `wait_writable` waits on it and
collapses signals that piled up (at most one stale wake, like the Windows auto-reset event).
MEASURED (`the_device_and_input_formats_are_what_open_says`, n=1): device side 48,000 Hz, 2
channels, `lpcm` flags 9 (float, packed), 8 bytes/frame — already interleaved float32; the input
side is set identical, so nothing is converted. Period (`kAudioDevicePropertyBufferFrameSize`) 512
frames; ring = max(request, 2 periods). `audio_live.rs` 3/3: buffer 4,800 for a request of 4,800;
first drain 10.6 / 10.4 ms after start (512 free); 46,320 and 47,755 frames/s over ~210 ms windows
at a reported 48,000 (ratios 0.965, 0.995; n=2 runs); a waiter on a refilled buffer woke with 512
free after 2.7 / 11.6 ms.

## Verification

All on this host, release, exit code 0 unless stated. Binaries confirmed to contain the tests
(`-- --list`).

| Command | Result |
|---|---|
| `cargo test -p omni-platform --release --no-fail-fast` | every target green **except** the lib's `fs::tests::dev_urandom_is_a_device_that_fills_the_buffer_with_entropy` — the orchestrator's `process::random_bytes` is still `Unsupported` on this base (its message says so); not this workstream's |
| `OMNI_GFX_WINDOW_TESTS=1 … --test window_live -- --ignored` | 10/11; the 11th, `the_raw_handle_is_a_live_win32_handle`, asserts a Win32 handle and fails by its own words on macOS |
| `OMNI_GFX_WINDOW_TESTS=1 … --test window_macos -- --include-ignored --test-threads=1` | 17/17 (16 gated tests, one ungated, plus the `child` helper that the hand-over tests spawn) |
| `… --test window_keys_macos` | 2/2 |
| `OMNI_AUDIO_LIVE_TESTS=1 … --test audio_live -- --ignored` | 3/3; plus the lib's gated format test |
| `OMNI_GFX_WINDOW_TESTS=1 … --test screen_capture_macos -- --ignored` | 1/1 (below) |
| `OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-gfx --release -- --include-ignored --test-threads=1` | renderer_live 9/9, host_present_live 2/2, portability_live 1/1, unit/select/image green |

**Pixel verification (VERIFICATION entry 19).** Two instruments, each first shown to see something
known — two frames of two different colours must read as two different, matching results:

* *Swapchain read-back* (`omni-gfx/tests/host_present_live.rs`): guest-shaped requests through
  `VulkanHost` (no portability extension asked for), clear, present, `read_presented_image`:
  `[51,102,204,255]` then `[255,128,0,255]`, exact at three points, on 1.0 and 1.1 instances. Sees
  exactly what was handed to the presentation engine; not the display.
* *Window-server capture* (`omni-platform/tests/screen_capture_macos.rs`): renderer on MoltenVK,
  `screencapture -l <window>`: BGR `[26,25,204]` for an expected `[26,26,204]` and `[229,52,25]` for
  `[230,51,26]` (colour-managed, within 1 step; n=1). Sees the window's composited surface; needs
  Screen Recording permission (MEASURED granted, `CGPreflightScreenCaptureAccess` true) — without
  it macOS returns the desktop picture and the test fails on colour, never passes.
* `omni-android --test vulkan_present` was **not** run: the file is `#![cfg(target_arch =
  "x86_64")]` (its harness runs guest ARM64 through the x86-64 translator), so on this host it
  compiles to nothing. `host_present_live.rs` drives the same host calls from Rust instead.

**Mutation rows** (`python3 tools/mutate.py --only mac-win-` / `--only mac-gfx-`, release, on this
host): **`mac-win-` 22/22 caught, `mac-gfx-` 9/9 caught**, both pre-flights green (22/22 and 9/9
patterns, 2/2 and 1/1 commands pass unmutated); `git diff --exit-code crates tools` clean afterwards,
and the harness process confirmed gone. The first `mac-win-` run was 20/22: the Command guard in
`keyDown:` (row A4) and the text filter through the keyboard (A5) were caught by nothing. A4's guard
was unreachable and was deleted, so the row is gone; A5 is now caught by a test that reaches the
filter the way an input method does. In that run `mac-win-A8`/`A9` (wheel) were classified "the suite failed
without naming a test": the wheel test printed from the main thread, which libtest does not capture,
so the line landed inside `test … FAILED` and the harness could not read it (reproduced by hand,
restored with `git checkout`, tree checked clean). The print now happens on the test thread, and
both rows, rerun alone, are caught by `wheel_lines_are_120_each_with_physical_signs` by name. Two gfx fixes (enabling the subset
extension, chaining its features) have no row, because MoltenVK accepts a device without them and
this host has no validation layer; `tools/mutate_mac/gfx.py` says so.

## Merge notes: every edit to a shared file

* `crates/omni-platform/src/window/mod.rs`: `mod unix` now `cfg(all(unix, not(target_os =
  "macos")))`; `RawWindow::AppKit { ns_window, ns_view, ca_metal_layer }` and its `system_name`
  `"appkit"`; `pub fn vulkan_loader_candidates()` (empty off macOS); `#[doc(hidden)] pub use
  macos::keys as macos_keys` (macOS only, for `tests/window_keys_macos.rs`).
* `crates/omni-platform/src/window/error.rs`: variants `MainThreadUnavailable { operation, why }`
  and `AppKit { operation, api, detail }` (additive; `WindowError` is not `#[non_exhaustive]`, and
  nothing in the workspace matches it exhaustively — checked by grep).
* `crates/omni-platform/src/audio/mod.rs`: `mod unix`/`use unix as backend` now
  `cfg(all(unix, not(target_os = "macos")))`; `mod macos; use macos as backend` on macOS.
* `crates/omni-platform/src/audio/error.rs`: variants `OsStatus { operation, api, status }` (prints
  the four-character code) and `DeviceFormatUnusable { operation, sample_rate, channels }`
  (additive; nothing matches `AudioError` exhaustively).
* `crates/omni-platform/tests/window_seam.rs`: the structural-refusal test's cfg is now
  `not(any(target_os = "windows", target_os = "macos"))`.
* `crates/omni-platform/Cargo.toml`: `[target.'cfg(target_os = "macos")'.dependencies]` under
  `# window + audio seams (macOS)` (objc2, objc2-foundation, objc2-app-kit, objc2-quartz-core, named
  features only), and `[target.'cfg(target_os = "macos")'.dev-dependencies]` (omni-android,
  omni-gfx — dev-dependency cycles, as omni-gfx's own manifest already has). If the orchestrator's
  lines go in the same `dependencies` table, keep mine contiguous under my comment.
* `crates/omni-gfx/src/lib.rs`: `mod portability;`.
* `crates/omni-gfx/src/portability.rs`: new.
* `crates/omni-gfx/src/claim.rs`: `WindowKey::appkit(ns_view)`.
* `crates/omni-gfx/src/vulkan.rs`: loader via `portability::load_entry`; the required platform
  surface extension follows the window's variant (Win32 → `VK_KHR_win32_surface`, as before);
  instance retry with portability; device subset; the Metal arms of `create_surface` and
  `window_key`; `DeviceReport::portability_gaps` (new pub field); the minimised-window skip.
  **Windows behaviour**: same loader call, same instance request (the retry fires only after
  `VK_ERROR_INCOMPATIBLE_DRIVER`), one extra `vkEnumerateDeviceExtensionProperties` before
  `vkCreateDevice`, and a zero `Resized` now skips even if the surface disagreed (on Win32 it does
  not disagree).
* `crates/omni-gfx/src/host.rs`: loader via `load_entry`; a `features2` per-instance leaf lock; the
  instance retry; subset enabling in `create_device` and on the import-probe device; the Metal arm
  of `create_platform_surface` (host_call = `PLATFORM_SURFACE_ENTRY_POINTS[4]`); its refusal text
  names both surface calls.
* `crates/omni-gfx/tests/renderer_live.rs`: the layer assertion was `!available_layers.is_empty()`
  ("the loader reports its implicit layers" — a Windows-host fact; this host has none, MEASURED by
  `vulkaninfo`); it now asserts the report equals the loader's own enumeration, read independently.
* New test files: `omni-gfx/tests/host_present_live.rs`, `omni-gfx/tests/portability_live.rs`
  (both cfg-free, gated), `omni-platform/tests/{window_macos,window_keys_macos,screen_capture_macos}.rs`.

## Open, with consequences

* **Captured motion is accelerated** (see 4). Consequence: mouse-look in the guest has macOS pointer
  acceleration on top of whatever the engine applies; fixing it means an `IOHIDManager` reader.
* **The physical wheel's sign under natural scrolling is documented, not measured** (see 3).
  Consequence if Apple's flag means something else: the guest scrolls the wrong way for users with
  natural scrolling on — visible at once, but not to any test.
* **Subset enabling has no detector** here (no validation layer; MoltenVK accepts a device without
  `VK_KHR_portability_subset`). The gfx mutation table says so instead of carrying a NOT CAUGHT row.
* **Default-device changes during a stream** (headphones plugged in) are followed by the
  DefaultOutput unit, but `format()` is fixed at open; a rate change would make the reported format
  stale. Not handled, not measured.
* **Presenting through MoltenVK loaded directly as the loader** (last candidate) is unmeasured
  beyond instance creation and enumeration.
* The gate (`omni-android/tests/gameactivity.rs`) is `cfg(target_arch = "x86_64")` today, so its
  window path has not run on this host; the seam calls it makes (`Window::new`, `show`,
  `poll_events`, `client_size`, `dpi`, `HostWindowSource`) are each exercised by the tests above.
