# Status

What works, per component and per platform. **Verified** means run and observed, with the record
named; **partial** means built with a named gap; **not started** means no code runs it. Figures
and run names (w = Windows, m = Mac, l = Linux) come from `HANDOFF.md` and `docs/ports/*.md`.

Last updated: 2026-09-27.

## The APK

`Roblox-2.738.1397.apk` in the repository root is the **stock** build: 229,466,269 bytes, sha256
`bbe00ae306cc251c4ea55b7a932d9c524ecb0d6d9203c2a6161bcf0fae792742`, signed by Roblox
Corporation (v2+v3), versionCode 3092, `com.roblox.client`, 3 dex files. It ships `arm64-v8a`,
`armeabi-v7a` and `x86_64` (11 libraries each); only arm64-v8a is loaded. Its arm64
`libroblox.so` (109,193,800 bytes) is byte-identical to the one every 2.738.1397 figure was
measured on. The Java surface regenerated from its dex equals `jni/surface.rs`.

**The in-world runs cited here (2026-09-25) used the modified 2.739.691 build**, since removed; its
`libroblox.so` is a different binary. None has been repeated on the stock APK yet, including
whether Roblox's integrity check passes.

## Platforms

| Target | State | Evidence |
|---|---|---|
| Windows x86-64 | **Verified: plays in a world.** Signed in, PS99 joined, 30-minute sessions clean. Vulkan on an RTX 4060 | w19: 52.8 fps median, 3.9-4.0 GiB private; w36 (shared cache): 47.0 fps, 2.55 GiB, 2.06 cores. The engine caps itself at 60 fps |
| Linux x86-64 | **Verified: plays in a world, GPU-bound.** Renders through GLES on a Quadro 4000 (nouveau, ES 3.1); X11 or Xwayland | l9: 30 min clean, 3.4 fps median; 13.6-14.0 fps on a quiet desktop (the GPU is 96-98% busy). Port suites and 122/122 `lnx-` mutation rows (`ports/linux.md`) |
| macOS ARM64 | **Partial: plays in a world, with open freezes.** Apple M1, Vulkan via MoltenVK | m10: 30 min, 55.8 fps median, 2.9 GiB; m12: 30 min clean. m7, m9, m11 froze or died (dynarmic arm64, patch 0023 pending). Workspace on 2026-09-24: 2,069 passed, 1 failed (`ports/macos.md`) |
| Linux ARM64 | **Not started.** Never built or run | -- |
| macOS x86-64 | **Not started.** Never built or run; `fault` has no backend there | -- |

## Components

| Component | State | Notes |
|---|---|---|
| Milestones M0-M8 | verified | M0-M7 on Windows and macOS; Linux M0-M5 and M7 through GLES (M6, the engine's own Vulkan device, unreached); M8 (the owner playing) on Windows, w32-w34 |
| APK choice, extraction cache | verified | `choose_apk`; one cache file shared by all instances |
| ELF loader, APS2 | verified | 568,806 relocations; relro sealed as bionic seals it, also at 16 KiB pages |
| `omni-mem` guest space, pager, JIT arena | verified | Windows, Linux, macOS; D10 re-measured on each (`ports/linux-notes/mem.md`, `ports/macos-memory.md`) |
| dynarmic, x86-64 | verified | shared translation cache default (D38): w27/w29 input with 0 s under 20 fps |
| dynarmic, arm64 | partial | per-thread caches only; stale-code transfer (m11) under investigation |
| native backend (Hypervisor.framework) | partial | runs M2 and all initializers; not adopted (D34) |
| native backend for Linux ARM64 | not started | |
| bionic adapter | verified | 322 bound symbols; raw `SVC #0` routed |
| guest signals | not started | `sigaction`, `raise`, `pthread_sigmask` refuse by name |
| JNI and the Java transcription | partial | works in-world; an untranscribed method reached from a menu kills its thread (w31, fixed case by case; `research/jni-audit-2.739.md`) |
| NDK (`ALooper`, assets, config, window) | verified | |
| Network (sockets, TLS by the guest's OpenSSL) | verified | Windows, Linux, macOS |
| Vulkan forwarding | verified on Windows, macOS | Linux: the engine refuses lavapipe as emulated, so unreached there |
| GLES forwarding | verified on Linux (X11) | Windows (ANGLE) and macOS hosts not started |
| Headless EGL (`set_driverless`) | verified on Windows, Linux | Mac not re-run |
| Headless mode (`--headless`, `--no-window`, `--control`; D40) | verified on Windows (Vulkan), Linux (GLES: X11, no display, llvmpipe) | stock APK, PS99. w2: the engine's GPU timer 3.7-4.6 ms/frame drawn, 0.87-1.13 headless; the game's 3D engine 22.5-23.3% -> 7.8%. l1/l2: 12-15 -> 18-22 fps (GPU-bound Quadro). l3 (llvmpipe, no display): 4-6 -> 18-21 fps. Screenshots while headless are the real frame; `headless off` redraws at once. macOS not run |
| Audio (`libaaudio.so`) | verified | WASAPI, ALSA (`audio_live_linux`), Core Audio (`mac-win-` rows) |
| Window, keyboard, mouse | verified | Win32; Xlib incl. Xwayland; AppKit. Native Wayland not started |
| Host cursor following the engine | verified on Windows (w33, w34) | Linux on Xvfb tests; macOS type-checked only |
| Web view (sign-in pages) | verified on Windows (WebView2) and macOS (WKWebView, `webview_live` 10/10, `45b2131`) | Linux not started: sign in with `--cookie` |
| Cookie store, `omnidroid login`, per-account storage | verified on Windows | |
| Memory report (`OMNI_MEM_REPORT`) | verified on Windows, Linux | macOS: guest side only |
| Sampling profiler (`OMNI_PERF`) | verified on Windows, Linux x86-64 | macOS arm64 backend built |
| Multi-instance | partial | one process each works (Windows 3, Mac 4 of 4, Linux 4 before swapping on 7 GiB); per instance ~2.5 GiB and ~0.6 cores capped (H2) against a 0.8-0.9 GiB target |
| `omni-core`, `omni-cli` | not started | the embedding lives in the gate test (ARCHITECTURE §2) |
| Linux personality (`omni-linux`, D39) | A verified on Windows (A1-A5); A1 on Linux x86-64 and macOS arm64; B and C verified on Windows + Linux; D1 verified on Windows, Linux and macOS; D2 and D3a verified on Windows and Linux | A1: the real AOSP 15 `toybox` runs through the real `linker64` and bionic to exit 0 (`tests/a1_toybox.rs`, debug and release; Linux with no change, 30/30 runs; macOS with the host page as the guest page, 16 KiB); the one refusal is liblog's `socket`. A2: the real `ls -l`, `cat /proc/self/maps`, `ps -A` (`tests/a2_proc.rs`). A3: the real `getprop` reads the property area written in bionic's formats (`tests/a3_props.rs`). In-loop syscall 43.8-50.1 ns per call including a 3-instruction loop (target <= 40 ns; `omni-cpu/tests/svc.rs`). A4: real bionic threads (`tests/a4_threads.rs`). A5: signals as the arm64 kernel delivers them (`tests/a5_signals.rs`). **B (ART runs dex):** real ART runs a `.dex` (`tests/b_hello_dex.rs`) and loads the stock Roblox APK's three `classes*.dex` into one `PathClassLoader`, 26,612/26,615 classes linked, all 11 native libraries incl. `libroblox.so` loaded through it, `JNI_OnLoad` run (`tests/c2_apk_in_app_process.rs`). Needed: msync, a low-4-GiB guest base stepping around host holes, TBI/DC-ZVA, MADV_REMOVE and MREMAP_DONTUNMAP zeroing, W^X granted as writable memory, memfd/ashmem live shared memory. **C (binder + services):** `/dev/binder` (+hwbinder/vndbinder) as the version-8 driver split into a broker and a per-process side; the property service (live, cross-process); selinuxfs; epoll/eventfd/timerfd/ppoll; pipes; `/dev/kmsg`; init reading the image's `.rc` services (`class_start`, `ctl.start`, boot-phase `start`s). The real `servicemanager` answers another guest process over binder (`tests/c1_binder.rs`); `hwservicemanager`, `apexd` and ~50 init services run; `system_server` boots ~27 services deep, to `StartDisplayManager`. **D1 (host binder services):** a service implemented in host Rust is a node on the broker, published with the real `servicemanager` (`addService`, its parcel byte for byte as the image's libbinder writes one), and found and called by a guest: `service check`/`service call omni.echo` (`tests/d1_host_service.rs`); its handler runs on a host thread, so it can call back into the waiting caller as a nested transaction (`tests/binder_host.rs`). **D2 (graphics buffers):** gralloc 5 -- the host serves the AIDL `IAllocator` (V2) on the broker and omnidroid's device overlay (`crates/omni-linux/device/`, files added over the pinned image) declares it in VINTF and supplies the stable-C mapper `mapper.omni.so`; each buffer is one `shm` region. The real libnativewindow/libui path (`AHardwareBuffer_allocate`, lock, write) in a guest, the host reading and writing the same memory (`tests/d2_gralloc.rs`; parcels `tests/hal_gralloc.rs`; host binder objects `tests/binder_host_objects.rs`). **D3a (the GPU):** a paravirtual Vulkan driver -- `vulkan.omni.so` (device overlay, an `hwvulkan` HAL) forwards each command through `ioctl(/dev/omni-gpu)` to the host's Vulkan (`crate::gpu`; 432 of 464 commands generated from vk.xml 1.3.275 by `tools/gen_vk_forward.py`, the rest by hand); GLES is the image's own ANGLE on it (`ro.hardware.egl=angle`). `VK_ANDROID_native_buffer` (swapchain images copied into the gralloc region at release), `VK_ANDROID_external_memory_android_hardware_buffer` (linear host-visible mirrors kept in step by the region's content generation) and SYNC_FD fences/semaphores are emulated on the host. The kernel no longer offers the tagged address ABI, so bionic's heap is untagged (a tagged pointer inside a Vulkan struct faulted the host driver). `tests/d3a_gpu.rs`: Vulkan through the real libvulkan, GLES through libEGL -> ANGLE, and an EGLImage of a gralloc buffer rendered and read by the host -- on the RTX 4060 (Windows) and on lavapipe (Linux, no GPU). **D3b onward (display) not started:** `system_server` stops at `StartDisplayManager` because the emulator image ships only the goldfish (`ranchu`) composer/allocator/EGL, which need `/dev/goldfish_pipe`. Design: host-side HALs on the binder broker composing into omnidroid's framebuffer (`docs/superpowers/specs/2026-09-27-display-and-host-hals-design.md`). 31 omni-linux test binaries green on Windows and Linux; on macOS the same minus the three ART gates (macOS maps nothing below 4 GiB) |

## Measured figures decisions rest on

| Quantity | Value |
|---|---|
| Guest reservation | 0 bytes of commit (Windows to 97.7 TB, Linux to 64 TiB) |
| Commit granule | 64 KiB: 150 ns/page on Windows against 2,414 at 4 KiB |
| Reclaim | only `MEM_DECOMMIT` returns commit on Windows; Linux re-maps `PROT_NONE` |
| JIT emit+execute, dual-mapped | 162 ns Windows, 117.5 ns Linux, against 2,259 / 2,418 ns flipping protection |
| `libroblox.so` per instance | ~16.4 MiB private; text shared |
| Three guest instances in one process | each costs only its private part; asserted per instance (`omni-elf/tests/loader_commit.rs`, `three_instances_share_the_file_backed_image`) |
| `libroblox.so` load | 11.8 ms release; extraction 413 ms once |
| Thunk crossing | 26.7-31.0 ns inline, 81-101 ns exit to Rust (D17) |
| Losing identity fastmem | 30-49x slower (D4) |
| Per guest thread, x86-64 | 4.47 MiB with patch 0017 (was 24.56) |
| Shared translation cache | 241-249 MiB committed in a world (w32, w36) |

Figures are indicative unless a test pins them; a measured quantity lives in one place.
