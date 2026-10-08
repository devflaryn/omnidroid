# Status

What works, per component and per platform. **Verified** means run and observed, with the record
named; **partial** means built with a named gap; **not started** means no code runs it. Run names
(w = Windows, m = Mac, l = Linux) and figures come from `HANDOFF.md` and `docs/ports/*.md`.

Last updated: 2026-10-08. Omnidroid's direction is now Android app testing and debugging.

**Scope of the evidence.** The runs recorded below were made on a Roblox build (`Roblox-2.7xx.apk`),
the APK the engine was first brought up on. The mechanisms they check (the loader, the Android
compatibility layer, the display and input path, the debugger) are not specific to that app, but
no general test-app run has been recorded yet. Rows that name Roblox describe that run.

## APKs

The APK under test is chosen by `--apk <path>`, else `OMNI_APK`, else the newest `*.apk` in the
repository root (`omni_apk::choose_apk`). Only `lib/arm64-v8a` is loaded. Test APKs are not in
version control.

## Platforms

| Target | State | Evidence |
|---|---|---|
| Windows x86-64 | **Verified: runs an app in a window** (Vulkan, RTX 4060). Sessions of 30 minutes ran clean | w19, w36 (`ports/windows.md`) |
| Linux x86-64 | **Verified: runs an app, GPU-bound** (GLES on a Quadro 4000, X11 or Xwayland). Real-AOSP path boots, installs and starts the app on the GL backend | l9 (`ports/linux.md`) |
| macOS ARM64 | **Partial: runs an app, with open freezes.** Apple M1, Vulkan via MoltenVK. Freezes in some runs on the dynarmic arm64 backend (patch 0023 pending) | m10, m12; m7, m9, m11 froze (`ports/macos.md`) |
| Linux ARM64 | **Not started.** Never built or run | -- |
| macOS x86-64 | **Not started.** Never built or run; `fault` has no backend there | -- |

## Components

| Component | State | Notes |
|---|---|---|
| Milestones M0-M8 (`ARCHITECTURE.md` §9) | verified | M0-M7 on Windows and macOS; Linux M0-M5 and M7 through GLES (M6, the app's own Vulkan device, unreached) |
| APK choice, extraction cache | verified | `choose_apk`; one cache file shared by all instances |
| ELF loader | verified | relocation, RELRO sealing (also at 16 KiB pages), `init_array`, `dl_iterate_phdr` |
| `omni-mem` guest space, pager, JIT arena | verified | Windows, Linux, macOS (`ports/linux-notes/mem.md`, `ports/macos-memory.md`) |
| dynarmic, x86-64 | verified | shared translation cache default (D38) |
| dynarmic, arm64 | partial | per-thread caches only; stale-code transfer on macOS under investigation (m11) |
| native backend (Hypervisor.framework) | partial | runs the M2 path and all initializers; not adopted (D34) |
| native backend for Linux ARM64 | not started | |
| bionic adapter | verified | 322 bound symbols; raw `SVC #0` routed |
| guest signals | verified on the real-AOSP path | the arm64 kernel's delivery (`omni-linux`, A5); the emulation layer refuses `sigaction`, `raise`, `pthread_sigmask` by name |
| JNI and the Java transcription | partial | an untranscribed method reached from the app kills its thread; fixed case by case (`research/jni-audit-2.739.md`) |
| NDK (`ALooper`, assets, config, window) | verified | |
| Network (sockets, TLS by the app's own libraries) | verified | Windows, Linux, macOS |
| Vulkan forwarding | verified on Windows, macOS | Linux: a CPU rasterizer is refused by apps that check for emulation |
| GLES forwarding | verified on Linux (X11) | Windows (ANGLE) and macOS hosts not started |
| Headless EGL (`set_driverless`) | verified on Windows, Linux | Mac not re-run |
| Headless mode (`--headless`, `--no-window`, `--control`; D40) | verified on Windows (Vulkan) and Linux (GLES: X11, no display, llvmpipe). Screenshots while headless are the real frame; `headless off` redraws at once | macOS not run |
| Audio (`libaaudio.so`) | verified | WASAPI, ALSA, Core Audio |
| Window, keyboard, mouse | verified | Win32; Xlib incl. Xwayland; AppKit. Native Wayland not started |
| Live display window on the real-AOSP path (`OMNI_WINDOW=1`) | **verified on Windows 2026-09-28**; Linux (Xwayland) 2026-09-29; macOS type-checked only | the composed display shown live, at the window's size; resize follows the window. Keyboard and mouse as evdev devices Android's InputReader reads (`crate::evdev`, `window_input`); the mouse is free and absolute, held only while the app holds pointer capture (`input_channel`) |
| Only the app on the display (kiosk, no SystemUI) | **verified on Windows 2026-09-28**; platform-agnostic | `OMNI_APP_ONLY=0` shows the chrome; `OMNI_R_KIOSK=1` disables SystemUI |
| Shared clipboard, both ways (`crate::clipboard`; `--no-clipboard` / `OMNI_CLIPBOARD=0` off) | **verified on macOS 2026-10-07**; Windows and Linux backends type-checked only | the host listens as the shell (uid 2000), which can read clips set by any app. Text and bitmaps only |
| Host keyboard layout (`crate::keymap`; `OMNI_HOST_KEYMAP=0` off) | **verified on macOS 2026-10-07**; Windows type-checked only; Linux: US | the device's key layout is generated from the host's current layout |
| No soft keyboard (`OMNI_KIOSK_IME=1` keeps it) | **verified on macOS 2026-10-07** | `ime list` empty; apps take typed keys with no on-screen keyboard |
| Browser pages in a host window (`crate::browser`; `OMNI_BROWSER_WINDOW=0` off) | **verified on macOS 2026-10-07** (WKWebView); Windows (WebView2) built only | a `startActivity(ACTION_VIEW http(s))` opens in a host browser window beside the display; app-scheme links go back to the device |
| Host cursor following the engine | verified on Windows; Linux on Xvfb tests; macOS type-checked only | |
| Web view (sign-in pages) | verified on Windows (WebView2) and macOS (WKWebView, `webview_live` 10/10) | Linux not started |
| Plugins (`omnidroid plugins`, `plugins/README.md`) | verified on macOS 2026-10-08 (unit tests; `play`/`aosp` env end to end with a stand-in session) | |
| Memory report (`OMNI_MEM_REPORT`) | verified on Windows, Linux | macOS: guest side only |
| Sampling profiler (`OMNI_PERF`) | verified on Windows, Linux x86-64 | macOS arm64 backend built |
| Multi-instance | partial | one process each works (Windows 3, Mac 4 of 4, Linux 4 on 7 GiB); about 2.5 GiB and 0.6 cores per instance against a 0.8-0.9 GiB target |
| `omni-core`, `omni-cli` | not started | the embedding lives in the gate test (ARCHITECTURE §2) |
| Lean single-app device (real-AOSP path) | **verified on Windows 2026-09-29**; platform-agnostic | HAL services and apps a device of this kind has no use for are left out of the image; kiosk from the first boot by default. Gate `tests/lean_image.rs` |
| Frame path (`compose::FAST`, on by default; `OMNI_COMPOSE_FAST=0` off; `OMNI_DISPLAY_MAX=WxH`) | built and run on macOS, not A/B-measured | the fast compose path is bit-identical by test and measured 7.04 -> 1.15 ms/frame at 1280x720 on Windows |
| Kernel: vDSO, POSIX timers, wait queues by key (`vdso`, `timer`, `poll`) | **verified on Windows 2026-09-29**; platform-agnostic | `tests/vdso.rs`, `tests/timers.rs`, IPC/wait suites. Linux/macOS not run |
| GPU backend of the real-AOSP device (`OMNI_GPU=vulkan\|gl\|auto`, `gpu::backend`) | Vulkan verified on Windows; **GL verified on Linux 2026-09-29** | GL: `libGLES_omni.so` forwards every GLES command to the host's GLES. `auto` takes GL on a host with no Vulkan GPU. Gate `tests/d3g_gl_fallback.rs`. ANGLE on D3D11 (Windows) and Metal (macOS) rows written, not run |
| Real-AOSP app launch (`omni-linux`, `tests/r_roblox.rs`, `tests/c5_app_launch.rs`) | **verified on Windows 2026-09-27/28**; Linux 2026-09-29 | `pm install` and `am start` of the app's launcher Activity; its view drawn on the host display through the paravirtual GPU. Recorded on a Roblox build: sign-in with `--cookie` and a `roblox://` deep link reached the app's game screen; the server then ended the session (reason 305), which is not worked around. Not yet re-run on a general test app |
| Linux personality (`omni-linux`, D39) | A1-A5 verified on Windows; A1 also on Linux x86-64 and macOS arm64; B and C verified on Windows and Linux; D1 on all three; D2 and D3a on Windows and Linux; D3b on Windows (Linux: open) | A1: the real AOSP 15 `toybox` runs through the real `linker64` and bionic (`tests/a1_toybox.rs`). A2: `ls -l`, `cat /proc/self/maps`, `ps -A`. A3: `getprop` reads the property area (`tests/a3_props.rs`). A4: bionic threads. A5: signals (`tests/a5_signals.rs`). B: real ART runs `.dex` and loads an APK's dex files (`tests/b_hello_dex.rs`, `tests/c2_apk_in_app_process.rs`). C: `/dev/binder` as a broker and per-process side, the property service, init's `.rc` services, `servicemanager` (`tests/c1_binder.rs`), `system_server` to `systemReady` (`tests/c4_system_server.rs`). D1: host-Rust binder services (`tests/d1_host_service.rs`). D2: gralloc 5 (`tests/d2_gralloc.rs`). D3a: paravirtual Vulkan and GLES through ANGLE (`tests/d3a_gpu.rs`). D3b: host `IComposer3` under the real SurfaceFlinger (`tests/d3b_display.rs`). D4/D5: an app's launcher Activity drawn on the host display (`tests/d5_app_on_display.rs`, 1 of 4 runs fully reliable; the splash-removal transition and an intermittent guest-space exhaustion are open) |
| Debugger, `omni-debug` (emulation layer) | **verified on Windows, Linux, macOS** | a lab `Session` loads a guest arm64 library on the translating backend and drives it below the guest: no ptrace, invisible to the target. resolve_symbol, list_maps, read_mem, write_mem, dump_module, call_function, alloc_data/load_code, breakpoints, get_registers, backtrace, intercept, trace_calls, trace_syscalls. Gate `tests/lab.rs` (12/12 on all three hosts) |
| MCP server, `omni-mcp` (stdio) | **verified on Windows, Linux, macOS** | JSON-RPC 2.0 over stdio, 25 tools (lifecycle, screenshots, and the `omni-debug` family). `crates/omni-mcp/README.md` has the setup. 16 unit and 4 end-to-end tests green on all three |

## Measured figures decisions rest on

| Quantity | Value |
|---|---|
| Guest reservation | 0 bytes of commit (Windows to 97.7 TB, Linux to 64 TiB) |
| Commit granule | 64 KiB: 150 ns/page on Windows against 2,414 at 4 KiB |
| Reclaim | only `MEM_DECOMMIT` returns commit on Windows; Linux re-maps `PROT_NONE` |
| JIT emit+execute, dual-mapped | 162 ns Windows, 117.5 ns Linux, against 2,259 / 2,418 ns flipping protection |
| Thunk crossing | 26.7-31.0 ns inline, 81-101 ns exit to Rust (D17) |
| Losing identity fastmem | 30-49x slower (D4) |
| Per guest thread, x86-64 | 4.47 MiB with patch 0017 (was 24.56) |

Figures are indicative unless a test pins them; a measured quantity lives in one place. The full
per-run records for the earlier app-specific measurements are in `HANDOFF.md` and `docs/ports/`.
