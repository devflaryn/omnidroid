# Gaming: one GPU path, and QEMU's own window as our window

Design, 2026-08-15. Sub-project **D** of the omnidroid update. Scope is the
gaming profile's render and presentation path on all three platforms, plus the
window the user actually looks at. Farming (density), the boot path, the QEMU
builds and the memory model are separate specs.

---

## 1. What is wrong today

Gaming reaches the GPU on one platform out of three, and the reason is never
the GPU — it is how the pixels get from QEMU to a window.

| platform | render | present | result |
|---|---|---|---|
| Windows | virgl, `gtk,gl=on` | window hidden at spawn, then `SetParent`-ed into a Tk viewer | 24–58 fps on PS99, zero copies |
| Linux | virgl, `egl-headless` | VNC: GPU readback → RFB encode → **Python** RFB decode → Tk | renders on the GPU, then pays for a copy per frame |
| macOS | none — Homebrew QEMU has no `virtio-gpu-gl-pci` | VNC | ~3 fps, software |

So "near-native, no input delay" is true on Windows only, and the two things
that make it true there — no copy, and host input delivered straight into
`usb-tablet`/`usb-kbd` — are exactly what the other two give up.

The Windows solution also carries a failure mode that is worse than the problem
it solves. QEMU's window is a **child** of our Tk viewer, and Windows destroys
a child window with its parent, so a viewer that is force-killed rather than
closed takes the guest's display with it permanently: the instance stays alive,
answers adb, and renders nothing (measured, `totalFrames = 0`). The engine has
a whole diagnostic path (`display_lost`) that exists only to report this.

## 2. Decisions this design is built on

Settled with the user before writing:

1. **The wrapper is QEMU's own window, modified** — not a viewer that hosts it.
   Nothing is reparented anywhere.
2. **Restyled chrome**, not a frameless surface and not a compiled-in toolbar:
   our title, our icon, no QEMU menubar, dark caption and accent border,
   remembered geometry.
3. **All three platforms are in scope**, macOS included, with its dependency on
   sub-project B stated rather than hidden.
4. **Input stays passthrough** — `usb-tablet` + `usb-kbd`, no pointer-lock mode
   and no key-mapping layer. Work is removing latency that exists, not adding a
   layer.
5. **Closing the window asks.** Hide, or stop the instance. A hidden window is
   shown again from the app.

## 3. The window model

QEMU's window stays top-level for its whole life. We restyle it in place and
dock a thin title bar of our own above it.

```
   +-- omni - farm3 ------------------------------ _  X --+   OUR strip
   +------------------------------------------------------+
   |                                                      |
   |              Android guest, GPU rendered             |   QEMU's window:
   |              QEMU's GL surface, zero copies          |   caption stripped,
   |                                                      |   menubar off
   +------------------------------------------------------+

   X  ->  [ Hide window ]  [ Stop instance ]  [ Cancel ]
```

### 3a. Ownership direction is the load-bearing detail

On Windows, `SetWindowLongPtr(strip, GWLP_HWNDPARENT, qemu_hwnd)` makes the
strip an **owned** window of QEMU's window. Three properties follow from that
and none of them needs polling:

* an owned window always floats above its owner, so z-order is solved;
* it minimises and restores with its owner;
* **destroying an owned window does nothing to its owner.**

That last one inverts today's hazard exactly. Force-killing the strip leaves
QEMU's window standing and the guest rendering. X11 gets the same relationship
from `XSetTransientForHint`.

The close button has to be ours because it cannot be anyone else's: one process
cannot intercept another's `WM_CLOSE` without DLL injection, so QEMU is spawned
`window-close=off` (its own X is inert and is stripped from the caption) and the
prompt lives on the strip.

### 3b. Per-platform mechanism

| | render + present | chrome applied by | close prompt |
|---|---|---|---|
| Windows | `virtio-gpu-gl-pci` + `-display gtk,gl=on` | Win32 from outside: `WM_SETICON`, `WS_CAPTION`/close box stripped, DWM dark caption + border colour, `SetWindowPos` | strip |
| Linux | `virtio-gpu-gl-pci` + `-display gtk,gl=on` | X11 from outside: `_MOTIF_WM_HINTS`, `_NET_WM_ICON`, `XSetTransientForHint` | strip |
| macOS | `virtio-gpu-gl-pci` + `-display cocoa,gl=es` | **compiled into our QEMU build** — macOS has no public API to restyle another process's `NSWindow`, only the private `CGSSetWindowParent` | the patch |

Common QEMU flags on a gaming boot:
`-display <backend>,gl=<...>,show-menubar=off,window-close=off,zoom-to-fit=on`
and `-name "omni-<account>"`, which is the identity everything finds the window
by.

**`gl=es` on macOS is a requirement, not a preference.** macOS deprecated
OpenGL for Metal, so its QEMU goes through ANGLE, which speaks GL ES; `gl=on`
and `gl=core` either refuse or render upside down. Two latent bugs from this are
already fixed in the tree (`default_display` hardcoded `gl=on` for every
platform, and `uses_gl_context` matched only the literal `gl=on`, so a
`cocoa,gl=es` boot kept `-vnc` — a pair QEMU rejects outright). Both would have
read as "the GPU does not work on macOS" on the first Mac to get a GL QEMU.

### 3c. Linux changes, and what it costs

Gaming moves off `egl-headless` onto a GL window. QEMU refuses `-vnc` beside a
windowed GL display — re-verified on 11.0.50 across `gtk`/`sdl` ×
`gl=on`/`gl=es`/`gl=core` — so **a gaming boot has no VNC server on any
platform now**. Screenshots come from `adb screencap`, which is verified working
against a GPU instance. `blocks_vnc` already encodes the rule; it starts
applying on one more platform.

Farming is unaffected: it is headless by construction and keeps `egl-headless`
and the VNC viewer for its render-when-checked path.

### 3d. `--gpu` becomes profile-directed

The four values stay (installed clients persist them), and `auto` stops meaning
something different on every host:

| value | meaning |
|---|---|
| `auto` | `performance` profile → GL window. `density` profile → GPU without presenting (on Windows, the hidden GL window that already halves farming's CPU) |
| `headless` | never a window; GPU only if it can be had windowless |
| `window` | always a visible native window, unstyled — for debugging a GL problem with none of this code in the path |
| `off` | software, headless |

## 4. Lifecycle

```
spawn      window flags above + -name "omni-<acct>"
window     found by identity, hidden immediately, keep_hidden() through boot
             (GTK re-shows it during early boot — known, already handled)
chrome     applied once on first find: icon, caption, DWM, geometry from run.json
view       show the window; spawn the strip, owned by it
X          prompt -> Hide  (SW_HIDE, strip exits)
                  -> Stop  (engine stop; both windows go)
                  -> Cancel
stop       strip sees its owner is gone and exits
```

`run.json` gains: the window identity, the display kind (`gl-window` | `vnc`),
and last geometry. The strip is a short-lived separate process, as
`_embedview` is today — it simply holds nothing hostage.

**DPI and multi-monitor** are named here so they are not discovered later: the
strip must be per-monitor-DPI-aware and follow the window between displays.
`SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE)` works cross-process
out-of-context, so tracking needs neither polling nor injection.

## 5. Degradation

Every rung falls back and **says why**. The current code's specific sin is a
GPU boot that could not present leaving a black viewer with no explanation.

| missing | behaviour |
|---|---|
| no virglrenderer in this QEMU | software render + VNC viewer, reason reported |
| GL window cannot be created (no session, RDP, no GPU) | same |
| chrome cannot be applied | window works, looks plain, warned once — never fatal |
| strip fails to start | window usable; show/hide/stop still work from the app |
| strip force-killed | **guest unaffected** — §3a guarantees it |

## 6. What is deleted

* `omnidroid/embedview.py`, all 274 lines. Its slot is taken by
  `omnidroid/windowbar.py` — the strip: a window we own, owned *by* QEMU's
  window, carrying the title, the icon and the X. It is spawned by a hidden
  `_windowbar` subcommand exactly as `_embedview` is today, so the process
  shape and its plumbing are unchanged; only the direction of the relationship
  is.
* In `cmd_view`: `_spawn_embedded_viewer`, `EMBED_VIEWER_SETTLE`, the
  three-state `window_is_embedded` detection, and the **`display_lost` failure
  entirely** — it exists only to report a force-killed viewer destroying the
  guest's window, which this design makes impossible.
* `tests/test_hidden_window_viewer.py` and `tests/test_hostwin_backends.py` are
  rewritten; the `display_lost` tests go with the failure mode.

`vncview.py` stays: farming needs it, and it is the fallback on any host where
the GL window cannot be had.

`hostwin.py` grows from find/hide/show into the window's styling owner — find,
hide at spawn, `keep_hidden` through boot, show on demand, apply chrome,
remember geometry — keeping the backend split its tests already assume.

## 7. Verification

Two tiers, because this subsystem has a history of unit tests pinning the wrong
answer: `tests/test_gpu_display.py` once asserted `gl=on` on every platform.

**Pure, in CI.** Argument construction per platform × mode × `--gpu` value
(`gl=es` on macOS is a test, not a comment); the VNC-vs-GL-window exclusion;
chrome operations against a fake window API; ownership direction; every
degradation reason.

**Measured, on hardware, written into MODES.md.**

| check | how |
|---|---|
| frames over 30 s, each platform | `dumpsys SurfaceFlinger --timestats` |
| the guest is really on Mesa/virgl | `adb shell dumpsys SurfaceFlinger \| grep GLES:` → `Mesa, virgl`, never `ANGLE … SwiftShader` |
| strip force-killed, guest still renders | `taskkill /F` on the strip, then timestats |
| the three close outcomes | by hand |
| hidden at boot never flashes on screen | watch a cold boot |

Windows can be done on the dev box. Linux needs a host. macOS needs §8.

## 8. Dependencies and sequencing

**Windows and Linux ship first and independently.** Nothing in §3b for those two
needs a QEMU patch or a new image.

**macOS is gated on sub-project B and on an image rebuild:**

1. *A virgl-capable QEMU.* Homebrew core has no `virtio-gpu-gl-pci`. The route
   that works: install knazarov's `libangle`, `libepoxy-angle` and
   `virglrenderer` formulae, build QEMU against them into a **private prefix**,
   point config `qemu.dir` at it. Never replace the system qemu —
   `startergo/qemu-virgl-kosmickrisp` names its formula `qemu` and Homebrew
   evicts the working one. `omni-executor/scripts/setup-macos.sh --gpu`
   automates the host half.
2. *An arm base rebuilt with `ro.hardware.egl=mesa`.* The image currently ships
   `angle`, so guest GL goes ANGLE → SwiftShader in software whatever the host
   offers. Mesa (`/vendor/lib64/egl/libEGL_mesa.so`) and the render node
   (`/sys/class/drm/renderD128`) are already in the image; only the property is
   wrong, and `ro.*` is immutable after init. Needs the Mac-only bake toolchain.
3. *Xcode CLT for macOS 26.* Homebrew refuses every source build until it is
   updated, and that needs a password at a GUI — a human, not a script. Do not
   pre-check with `brew install --dry-run` on a bottled formula: a bottle never
   invokes a compiler, so the probe passes on a machine that cannot build
   anything.

The Mac at `192.168.0.30` is **reachable again as of 2026-08-15** (ProtonVPN
off; 0% packet loss), so only (1)–(3) remain.

## 9. Non-goals

* **Resolution above the base's native panel.** `--panel 1080p` on the x86 base
  was measured taking 3.3+ minutes without reaching adbd and came up 1280x800
  regardless — the guest ignores a mode its panel does not carry. The window
  scales with `zoom-to-fit`; anything sharper is an image change.
* **Frame rate parity with a native client.** The GPU removes llvmpipe, which
  per-thread attribution put at ~75% of a software instance's CPU. What remains
  is `libndk_translation`: Roblox ships arm64 only, so every instruction is
  translated on the x86 base, and 640x480 gave 19 fps against 14 fps at
  1280x800 — a 4.2× pixel cut for 33% more frames. No display flag touches it.
  On the arm base under HVF the game runs natively and this ceiling does not
  exist.
* **Pointer lock and key mapping.** Deliberately out (decision 4). Each is its
  own feature with its own UI.
* **Farming's density work, the boot path, the QEMU builds, the memory model.**
  Separate specs (sub-projects A, B, C, E).
