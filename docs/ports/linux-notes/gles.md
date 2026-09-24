# Linux port notes: GLES (the engine's OpenGL ES fallback)

Branch `lnx-gles`. Host: Ubuntu 26.04, i5-4460, Quadro 4000 (Fermi). Vulkan on this host is Mesa
lavapipe only; OpenGL ES comes from two places: **llvmpipe** (Mesa's CPU rasteriser) on an Xvfb
display, which has no DRI device, and **NVC0** (nouveau, `OpenGL ES 3.1 Mesa 26.0.8`) on the owner's
Xwayland `:0`. Every test prints the host's `GL_RENDERER`/`GL_VERSION`, so each result below says
which device drew it.

> **Frame-rate caveat.** Every figure measured under Xvfb is llvmpipe: CPU rendering on the same
> four cores the translated guest runs on, with other workers' builds on the machine (load average
> 13-17 during the runs). None of it says anything about a GPU. The NVC0 figures would be a real
> GPU at boot clocks (nouveau cannot reclock Fermi) -- and there are none for the engine: see
> "The owner's display" below.

## Why it exists (measured)

With the network working, the engine reached `APP_READY` Home, refused lavapipe by its own rule
(`Vulkan: Device llvmpipe (LLVM 21.1.8, 256 bits) is emulated, skipping`, `Mode 6 failed: Unable to
pick Vulkan device` -- D8, not worked around), and fell back to OpenGL ES, whose first call,
`eglGetDisplay`, killed the render thread: nothing implemented the 91 `egl*`/`gl*` imports.

## What is built

| file | what |
|---|---|
| `tools/gen_gles_signatures.py` | reads the Khronos registries (OpenGL-Registry `1cdd228e`, EGL-Registry `db3425b8`; the XML sha256s are in the generated header) and emits `signatures.rs`: every `gles2` (ES 2.0-3.2 + extensions) and EGL command -- **1060** -- with its guest classes (`I`/`F`/`D`, AAPCS64), its exact host C widths (`P` 64, `W` 32, `B` 8 for `GLboolean`, `F`, `D`; returns `V`/`P`/`W`/`S` signed 32/`B`/`F`), its origin (feature or extension), and **one typed caller per distinct host shape (176)**: an `unsafe fn` that transmutes the host address to exactly that `extern "C" fn(...)` type. An unknown C type stops the script naming the command |
| `crates/omni-android/src/gles/mod.rs` | `Gles`: binds every **core** ES 2.0-3.2 and EGL 1.0-1.5 command by name (so the 91 imports resolve) plus a 256-slot `eglGetProcAddress` pool; one inline handler reads the arguments in AAPCS64 order by the registry's classes (x0-x7 then stack; s0-s7 then stack; each bank separately) and calls the host through the shape's caller. Census: calls by name (an atomic per slot, never dropped), every `eglGetProcAddress` with its answer, every substitution, presents |
| `crates/omni-android/src/gles/host.rs` | the `GlesHost` seam (`select(RawWindow)`, `proc_address`, `default_display`, `create_window_surface`, `destroy_window_surface`, `display_terminated`) and `HostProc`, the host address that only a caller ever uses -- never handed to the guest |
| `crates/omni-android/src/gles/egl.rs` | `eglGetDisplay(EGL_DEFAULT_DISPLAY)` -> the host display of the guest window's system; `eglCreateWindowSurface` -> the host window behind the `ANativeWindow` (checked against the NDK's live windows, raw window from the same `WindowSource` the Vulkan surface uses); `eglGetConfigAttrib(EGL_NATIVE_VISUAL_ID)` translated to Android's `WINDOW_FORMAT_*` from the host config's channel sizes; `EGL_RECORDABLE_ANDROID`/`EGL_FRAMEBUFFER_TARGET_ANDROID` dropped from `eglChooseConfig` when the host lacks their extensions (MEASURED: Mesa X11 rejects both with `EGL_BAD_ATTRIBUTE` 0x3004); `eglQueryString` copied to guest memory; `eglGetProcAddress`; swaps counted; native-object calls refused by name |
| `crates/omni-android/src/gles/gl.rs` | `glGetString`/`glGetStringi` copied into an interned guest pool; buffer mappings through a guest-memory **shadow** (filled when `GL_MAP_READ_BIT` is set or neither invalidate bit is; copied back at unmap under `GL_MAP_WRITE_BIT`, or only the flushed ranges under `GL_MAP_FLUSH_EXPLICIT_BIT`; `glGetBufferPointerv` answers the shadow); `GL_EXT_buffer_storage` **withheld** (below) |
| `crates/omni-gfx/src/gles.rs` | `GfxGlesHost`: a `HOSTS` table row per window system, chosen by the `RawWindow` variant at run time (no `cfg`). `Xlib` -> `libEGL.so.1` + `libGLESv2.so.2`, `eglGetPlatformDisplay(EGL_PLATFORM_X11_KHR, Display*)`, `eglCreatePlatformWindowSurface(&Window)`, the window claimed in `omni_gfx::claim`. `Win32` -> a typed refusal naming ANGLE |

Calls whose values do not pass through, and why, are tabled in `gles/mod.rs`'s module docs. Values
that do: GL object names, `EGLDisplay`/`EGLContext`/`EGLSurface`/`EGLConfig` (the host's, identity),
and every guest pointer -- including client-side vertex arrays, which the driver reads at the draw,
possibly from its own threads: guest memory the guest still owns, and a fault on an uncommitted
page of it is the demand pager's (`omni_platform::fault` serves host code touching guest memory).

## Substitutions the engine was measured needing

* **`GL_EXT_buffer_storage` withheld.** Run 2: with Mesa advertising it, the engine's render thread
  called `glMapBufferRange(access = 0xc2)` -- `WRITE | PERSISTENT | COHERENT`. A coherent mapping's
  writes must reach the driver with no call; a shadow cannot do that, and admitting driver memory
  into the guest's validated memory would void `admit`. So the extension is removed from
  `glGetString(GL_EXTENSIONS)`, from `glGetStringi` (the guest's index remapped past it) and from
  `GL_NUM_EXTENSIONS`, and `glBufferStorageEXT` answers NULL -- recorded. The engine then logged
  `Caps: ... Persistent 0` and ran.
* `eglGetProcAddress` answered NULL for seven names no `gles2`/EGL registry has (`glMapBuffer`,
  `glBufferStorage`, `glQueryCounter`, `glGetQueryObjectiv`, `glGetQueryObjectui64v`,
  `glPushGroupMarker`, `glPopGroupMarker` -- desktop GL spellings); the engine then asked for and got
  the `EXT`/`OES` spellings. GLVND's `eglGetProcAddress` answers every `gl*` name with a dispatch
  stub, so "not in the registry" is decided here, not by the host.

## Test commands and results (process exit codes)

| command | display | result |
|---|---|---|
| `cargo test -p omni-android --release --lib gles::` | -- | 13/13, exit 0 |
| `cargo test -p omni-gfx --release --lib gles::` | -- | 2/2, exit 0 (the harness pre-flight) |
| `OMNI_GFX_WINDOW_TESTS=1 DISPLAY=:94 cargo test -p omni-android --release --test gles_present -- --ignored --test-threads=1` | Xvfb :94, llvmpipe, ES 3.2 | 6/6, exit 0 |
| same, `DISPLAY=:0` | owner's Xwayland, **NVC0**, ES 3.1 | 6/6, exit 0 (before the GPU hang below) |
| the gate (`docs/ports/linux.md`), `OMNI_SESSION_SECONDS=120`, fresh `OMNI_DATA_DIR` | Xvfb :95, llvmpipe | run 3: **exit 0**, 179 presents, `M5 teardown: 0 guest thread(s) still running, failures []` |

The live tests (guest `BLR`s into bound thunks): display/config/window surface/ES 3 context;
`glClearColor` floats read back through `GL_COLOR_CLEAR_VALUE` (unclamped values too); a cleared
frame read back by `glReadPixels` before the swap and **captured from the X window** after it with
ImageMagick `import -window` -- colour A, then colour B, so the capture is shown seeing a known colour
and following a change (VERIFICATION 19); `glGetString` equal to the host's own text, at a guest
address, one address per string; a GLSL ES 3.00 textured draw from a client-side array with a
`glUniform4f` tint (read back with `glGetUniformfv`); `glTexImage3D` (10 args) and
`glTexSubImage3D` (11 args, three on the guest stack) with one texel read back through a layer
attachment; map/unmap round trips (read, write+invalidate, write without invalidate touching two
bytes, explicit flush); `eglGetProcAddress` NULL for an EGL name the host lacks (asked of the host
first) and for a non-registry name; the withheld extension absent from all three views (as sets:
ES 3.2 does not fix the order and Mesa's two lists differ). Unit tests: all 91 imports have a
signature and are bound by name, the shapes are the registry's (`glUniform4f` = `IFFFF` / host
`WFFFF`, `glColorMask` host `BBBB`, ...), every signature points at a caller of its shape, and the
callers against Rust `extern "C"` functions: nine floats (past SysV's eight vector registers),
interleaved ints and floats, eleven arguments, `GLboolean` bytes with garbage high bits, returns
widened by type (`GLint -1` sign-extended), doubles.

## The real engine on GLES (gate, Xvfb :95, llvmpipe; n = 1 run each)

Run 1 died on an environment fault, not this layer: the worktree's APK was a **symlink**, the gate
hard-links it into the guest root, and `link(2)` links the symlink itself, which the filesystem
seam refuses (`base.apk is a symbolic link`). Fixed by making the worktree's APK a hard link to the
real file. (Every worktree set up with a symlinked APK has this.) Run 2 died on the coherent
mapping above. Run 3, the committed tree:

```
[FLog::Graphics] Vulkan: Device llvmpipe (LLVM 21.1.8, 256 bits) is emulated, skipping
[FLog::SurfaceController] Mode 6 failed: Unable to pick Vulkan device
[FLog::Graphics] Trying to choose EGL config r8 g8 b8 vsync0 ...
[FLog::Graphics] Created  context 0x795f4847f840
[FLog::Graphics] Initialized EGL context ... with renderbuffer 1280x720
[FLog::Graphics] GL Renderer: llvmpipe (LLVM 21.1.8, 256 bits)
[FLog::Graphics] GL Version: OpenGL ES 3.2 Mesa 26.0.8-1ubuntu0.3
[FLog::Graphics] Caps: VAO 1 TexStorage 1 MapBuffer 1 MapBufferRange 1
[FLog::Graphics] Caps: TimerQuery 0 Sync 1 UBO 1 Views 1 Persistent 0
[FLog::Graphics] GL feature level: OpenGL 3.2 UBO
[FLog::Graphics] Loaded 1477 shaders from pack glsles3 variant default (83247019 bytes)
[FLog::Graphics] Compiled 636 shaders in 433 ms.
GLES: 188110 call(s) to 88 entry point(s); 179 present(s) (eglSwapBuffers answered EGL_TRUE)
FRAMES: 179 presents in all
M5 teardown: 0 guest thread(s) still running, failures []
test result: ok. 1 passed   (exit 0)
```

* **First frames:** the EGL context at engine time 15.4 s; FRAMES (sampled every 5 s): 0 at +10 s,
  3 at +15 s, 17 at +20 s; peak 29 presents in one 5 s window (+55..60 s, ~5.8/s), 179 in the
  120 s session (~1.5/s average), 2-3 per 5 s at the end (the landing screen idle). llvmpipe on the
  guest's own four cores with the machine loaded -- **not a GPU figure**.
* **Pixels:** `import -window` of the 1280x720 window at +~60 s: **5,790 distinct colours** on a
  160x90 grid (one sample per 8x8 px, `convert -sample 12.5%`): the Roblox landing screen (logo,
  Create Account / Sign In, game tiles) -- the engine's own image, drawn by llvmpipe through this
  layer. The same capture method was shown seeing known colours first by the live test.
* The engine resized its framebuffer twice and recreated the window surface each time
  (`updateMainFramebuffer needs to resize`); `eglCreateWindowSurface` x3, `eglDestroySurface` fine.
* **Not this layer's, reported:** 35 `failed to create shader` (`SmoothClusterVSUnified` x32,
  `SmoothClusterShadowVS_` x2, `SmoothClusterDepthVS` x1 -- the terrain shaders) with Mesa's GLSL
  compiler saying `could not implicitly convert operands to arithmetic operator`; the other 636
  compile. That is the engine's GLSL against Mesa's strict ES compiler (the source reaches the
  driver unchanged: guest pointers, identity mapping). Also 4 `shader ... is not available`.
  The engine logged `Excluded 'Omnidroid:llvmpipe ...' - disabling SuperHQ shaders` -- its own rule.
* Engine GLES 3 entry points asked through `eglGetProcAddress`: 55 (list in the run log's `GLES:`
  line); all answered by the host except the seven desktop spellings above.

## The owner's display: a nouveau GPU hang (READ FIRST)

**The owner's desktop (Xwayland `:0`) stopped answering X clients during this work, and the kernel
log says the nouveau GPU wedged.** Timeline (kernel journal):

* ~06:37: `gles_present` 6/6 on `:0` (NVC0) -- worked.
* **06:39:59**: at the moment `Xvfb :95` was started (plain `Xvfb :95 -screen 0 1920x1080x24`),
  `nouveau: fifo: fault 00 [READ] at 0 engine 00 [PGRAPH] ... on channel 11 [Xvfb[389353]] ...
  channel 11 killed!`. **Xvfb itself opened the nouveau render node** (its server-side GLX loads a
  Mesa driver, and `/dev/dri/renderD128` is readable by the user), and Fermi faulted.
* From then on, every ~7 minutes: `failed to idle channel 11 [Xvfb[389353]]`, `SCHED_ERROR 0d`,
  `timeout`; `Xvfb :94` (started 06:07 the same way) got channel 12, then hung in `D` state (a 1 s
  EGL test took 70-86 s on it). At 07:28 a kernel `WARNING` in `nv50_runl_wait` (Xvfb freeing
  its channel).
* 07:3x: `:0` no longer answers new clients (`xdpyinfo` times out); **Xwayland (pid 45990) sleeps
  in `dma_fence_default_wait`** -- a GPU fence that never signals. Killing both Xvfbs did not
  recover it. It needs a GPU reset or a reboot (root). **The gate run on `:0` (run 4) never got
  past opening its window** (its main thread waited on the X server) and was killed; no engine
  frame was drawn by NVC0.
* The Xvfb I use now is started so it cannot touch the GPU:
  `LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe Xvfb :94 -screen 0 1920x1080x24 -extension GLX`
  (checked: no `/dev/dri` descriptor in the process). **Every worker's Xvfb should be started this
  way on this host.**

## Mutation rows (`tools/lnx_rows/gles.py`)

`flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-gles` (Xvfb :94 started the
GPU-free way above): pre-flight 16/16 patterns match once, 3/3 commands pass on the unmutated tree,
**16/16 caught**, harness exit 0, `git diff --exit-code crates tools` clean afterwards. Each row
and the test that caught it:

| row | mutation | caught by |
|---|---|---|
| A1 | a float argument read from the integer registers | the cleared frame (`GL_COLOR_CLEAR_VALUE`), the shaded draw |
| A2 | arguments past the eighth not read from the guest stack | the eleven-argument call, the shaded draw |
| A3 | the eleven-argument caller swaps its 9th and 10th arguments | unit `an_eleven_argument_shape...` |
| A4 | a signed 32-bit return zero-extended | unit `returns_are_widened_by_their_own_type` |
| A5 | `glGetString` returns the host's pointer | 5 live tests |
| B1 | host strings re-copied instead of interned | the cleared frame ("one address") |
| A6 | unmap without the shadow copy-back | the mapping round trip |
| A7 | a write mapping's shadow not filled | the mapping round trip |
| B2 | an explicit-flush mapping copied back whole | the mapping round trip |
| B3 | `eglGetProcAddress` non-NULL for a missing name | `get_proc_address...` |
| A8 | `EGL_RECORDABLE_ANDROID` passed through | the config test |
| A9 | `EGL_NATIVE_VISUAL_ID` untranslated | the cleared frame |
| A10 | a swap not counted as a present | the cleared frame |
| A11 | `GL_EXT_buffer_storage` not withheld | `get_proc_address...` |
| A12 | `glGetStringi` not remapped past it | `get_proc_address...` |
| B4 | a Win32 window given the X11 row | omni-gfx unit `a_win32_window_is_refused_naming_angle` |

The whole Linux table's 138 patterns were checked to match exactly once on this tree (no row
staled by the shared edits); only the lnx-gles rows were run.

## Shared edits (merge notes)

| file | what | why |
|---|---|---|
| `crates/omni-android/src/lib.rs` | `pub mod gles;` | the new module |
| `crates/omni-gfx/src/lib.rs` | `pub mod gles;`, `pub use gles::GfxGlesHost` | the new host |
| `crates/omni-gfx/Cargo.toml` (+ `Cargo.lock`) | `libloading = "0.8"` in `[dependencies]` | loading the host EGL at run time; the crate and version `ash`'s `loaded` already brings in |
| `crates/omni-android/tests/gameactivity.rs` | under the graphics gate: bind a `Gles` beside Vulkan (its slots added), `GfxGlesHost` as its host, published to created threads; FRAMES adds `Gles::presents`; the census printed as `GLES:` lines | the gate must reach the engine's GLES fallback and count its frames |
| `tools/gen_gles_signatures.py` | rewritten emitter (exact widths, EGL origins, callers) | the coordinator's generator emitted the classes only |

No Windows behaviour changes: without `OMNI_GFX_WINDOW_TESTS` the gate binds no GLES, exactly as
before; with it, the 91 imports now resolve where they refused.

## For the Windows and macOS hosts: exactly what to do

Nothing in `omni-android` changes. The typed callers are `extern "C"` function-pointer types, so
on `x86_64-pc-windows-msvc` rustc already passes them in the Microsoft x64 convention (a float's
register follows its position: `glUniform4f(GLint, 4 x float)` goes `ecx, xmm1, xmm2, xmm3,
[stack]`); the unit tests `a_ninth_float_goes_past...` and `integers_and_floats_interleaved...`
are what prove it there.

**Windows (`crates/omni-gfx/src/gles.rs`):**
1. Ship ANGLE's `libEGL.dll` and `libGLESv2.dll` beside the executable (BSD-3; Chromium's or a
   standalone build with the D3D11 backend).
2. Add a `HostRow { system: "win32", egl: &["libEGL.dll"], gles: &["libGLESv2.dll"], platform:
   0x3202 /* EGL_PLATFORM_ANGLE_ANGLE */, platform_name: "EGL_PLATFORM_ANGLE_ANGLE",
   platform_extensions: &["EGL_ANGLE_platform_angle"] }` to `HOSTS`, and in `host_for` replace the
   `Win32` refusal with `Ok(&HOSTS[1])`.
3. `default_display`: add a `RawWindow::Win32` arm calling `eglGetPlatformDisplayEXT(0x3202,
   EGL_DEFAULT_DISPLAY, {EGL_PLATFORM_ANGLE_TYPE_ANGLE 0x3203, EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE
   0x3208, EGL_NONE})` (an `EGLint` list for the EXT form).
4. `native_window`: a `Win32 { hwnd, .. }` arm returning `(WindowKey::win32(hwnd), hwnd as u64)`;
   in `create_window_surface`, for Win32 call `eglCreateWindowSurface(dpy, cfg, hwnd, attribs)`
   (the `HWND` itself is `EGLNativeWindowType`; with the platform form pass `&hwnd`).
5. Run: `cargo test -p omni-gfx --release --lib gles::` (update the Win32 unit test: it asserts
   the refusal), `cargo test -p omni-android --release --lib gles::` (the shape callers under the
   Microsoft ABI), then `OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-android --release --test
   gles_present -- --ignored --test-threads=1` -- the `capture` helper is ImageMagick-on-X11; give
   it a Windows arm (the composed-screen copy VERIFICATION 19 used) or skip only that assertion by
   name. Then the gate. `WITHHELD` may need entries for what ANGLE advertises and a shadow cannot
   honour; measure first.

**macOS (`crates/omni-gfx/src/gles.rs`, after `omni-platform` grows a macOS `RawWindow` variant
carrying a `CAMetalLayer *` -- the Mac branch's):** ANGLE over Metal: `libEGL.dylib` +
`libGLESv2.dylib`, display `EGL_PLATFORM_ANGLE_ANGLE` with `EGL_PLATFORM_ANGLE_TYPE_ANGLE =
EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE (0x3489)`, native window = the `CAMetalLayer *`. One `HOSTS`
row, one `host_for` arm, one `native_window` arm, the same tests. On Apple silicon the guest is not
translated, and Apple's arm64 ABI packs stack arguments by size -- the generator's exact widths
(`W`/`B`, not everything-64-bit) are what keep the callers right there.
