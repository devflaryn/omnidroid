# Graphics de-risking spike: native window + Vulkan on the target host

Spike code: `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps-omnidroid\2692e040-d7b4-4250-b114-62e3f26c66c9\scratchpad\gfxspike\` (throwaway, not in the repo). Built and run on this machine (Windows 11 Pro 10.0.26200, RTX 4060, driver 591.86, Vulkan loader 1.4.321 / device 1.4.325, Rust 1.89 stable-msvc — all pre-verified, not re-checked here). Every claim below was produced by code that was actually compiled and executed on this host; anything not directly measured is explicitly marked "unverified."

## 1. Native resizable window + Vulkan triangle

Built with `ash` 0.38 + `winit` 0.30 + `ash-window` 0.13 + `raw-window-handle` 0.6. The app implements `winit::application::ApplicationHandler` (0.30's required trait — `resumed()` creates the Vulkan instance/device/surface/swapchain, `window_event()` handles `Resized`/`RedrawRequested`/`CloseRequested`). `ash-window::create_surface` and `enumerate_required_extensions` take `raw-window-handle` 0.6's `DisplayHandle`/`WindowHandle` via `.as_raw()` — this interop is a single obvious call, no friction.

**Result: the triangle renders.** Verified by screen capture of the live window (not `PrintWindow`/GDI — see below): a full-window dark-navy clear (`(0.02, 0.02, 0.08)`) with an interpolated red/green/blue triangle, confirmed at the initial 1024×768 size and again after 6 live resizes up to 1350×1000.

**Resize handling found and fixed a real bug.** The first version of `recreate_swapchain()` destroyed the outgoing `VkSwapchainKHR` handle *before* passing it as `oldSwapchain` to the replacement `vkCreateSwapchainKHR` call — a use-after-free. On this machine that didn't just validate-warn (no validation layers, see task 6) — it reliably **crashed the NVIDIA driver** on the very first live resize, every time: Windows Event Log recorded repeated faults in `nvoglv64.dll`, exception code `0xc0000409` (`STATUS_STACK_BUFFER_OVERRUN`/`__fastfail`). Fix: destroy only the framebuffers/image views/render pass immediately, keep the old swapchain handle alive, create the new swapchain with `oldSwapchain = old_handle`, *then* destroy the retired handle. After the fix: 6 consecutive live resizes (500×400 up to 1250×900) with zero crash events and correct rendering at every size, confirmed by screenshot at each end state. This is exactly the kind of bug that validation layers exist to catch instantly and obviously — without them, the only signal was "driver silently died," which took an Event Log lookup to diagnose. Direct evidence for task 6's severity claim.

Separately, and worth recording because it will bite test automation: `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT)` reliably captures the window's title bar but returns solid black `(0,0,0)` for the client area — it does not capture DXGI/Vulkan flip-model swapchain content on this system. Real verification required activating the window and doing a `CopyFromScreen` capture instead.

**Crate versions that actually compiled together** (see full list under "Cargo.toml" below): `ash 0.38.0+1.3.281`, `ash-window 0.13.0`, `raw-window-handle 0.6.2`, `winit 0.30.13`. No version conflicts, no patch/override needed.

## 2. Shader compilation without a Vulkan SDK

Confirmed absent from PATH: `glslc`, `glslangValidator`, `cl.exe`, `VULKAN_SDK` unset. `cl.exe` does exist on disk (`...VC\Tools\MSVC\14.44.35207\...\cl.exe`) but is not on PATH.

**Chosen and verified working: `naga`** (pure Rust, `wgsl-in` + `spv-out`), invoked from `build.rs` at compile time. WGSL source (`shaders/triangle.wgsl`, one vertex-color triangle) is parsed, validated (`naga::valid::Validator`), and lowered to SPIR-V words per entry point via `naga::back::spv::write_vec`, written to `OUT_DIR`, and `include_bytes!`'d into the binary. This worked on the first successful build with zero native toolchain involvement — no CMake, no C++ compiler, no SDK. Resolved version: `naga 22.1.0` (note: the requested `"22"` resolved here; `naga 30.0.1` exists upstream but its `back::spv` API differs and would need adaptation). Minor API friction: this version's `spv::PipelineOptions` has no `multiview` field (present in some naga docs/examples) — trivial fix.

**`shaderc` attempted and documented as friction, not chosen.** In a separate probe crate: `cargo build` with `shaderc = "0.8"` correctly detected no prebuilt native shaderc and fell back to `build-from-source`. Notably, the `cc`/`cmake` crates **did auto-locate `cl.exe` via vswhere/registry despite it not being on PATH** — "MSVC not on PATH" is not itself a blocker for cc-rs-based builds. However, the build failed for two real, verified reasons:
1. The vendored `shaderc-sys` 0.8.3 `CMakeLists.txt` declares `cmake_minimum_required` below 3.5, and the installed CMake (4.4.3) has dropped support for that — hard error unless `CMAKE_POLICY_VERSION_MINIMUM=3.5` is set as an environment variable (confirmed: setting it gets past this step).
2. With that workaround, the build then fails during CMake's own compiler self-test because the generated object-file path exceeds Windows' ~250-character object-path limit — a direct consequence of building inside this deeply-nested scratch directory (`...\claude\...\scratchpad\shaderc-probe\target\debug\build\...`), not of shaderc itself.
Neither is fundamental: a real project building from a short path with `CMAKE_POLICY_VERSION_MINIMUM=3.5` set would likely get further, but the full shaderc/glslang/SPIRV-Tools C++ build is heavy (many minutes) and depends on a working MSVC + CMake + Ninja chain existing at all — none of which is guaranteed on an end-user's machine. Not attempted to completion given the spike's time budget; this is reported as friction, not a verified failure of shaderc itself.

`glsl-to-spirv` was not tried: it wraps an unmaintained fork of glslang and is generally superseded by naga/shaderc for new work; skipped in favor of spending the budget on the two realistic candidates.

**Tradeoffs and recommendation for Omnidroid:**
- **naga**: zero external toolchain, pure Rust, trivially reproducible on any dev/CI/end-user machine — but its GLSL front-end (`glsl-in`) is less complete than glslang's, and it does not perform glslang/shaderc-grade optimization of the emitted SPIR-V.
- **shaderc**: industry-standard, robust GLSL(+HLSL) front end and mature optimization, but pulls in a large C++ dependency requiring a working CMake+MSVC+Ninja chain to build from source on Windows, adding real setup/build-time cost.
- **Embedding pre-compiled SPIR-V**: zero runtime/build cost, but only viable if the shader set is fixed ahead of time — not viable here since the whole point is *running arbitrary Roblox-supplied shaders*.

Given Omnidroid will receive Roblox's *own* shaders at runtime — either GLSL ES (if Roblox's Android GLES path is what gets intercepted) or already-compiled SPIR-V (if a Vulkan path is used) — the long-term shader pipeline needs to handle both:
- If Roblox supplies **GLSL ES source**: neither naga nor shaderc's normal GLSL front end targets GLSL ES semantics out of the box; this will need either a GLSL-ES-aware front end (a real gap — flag for the team determining Roblox's actual shader format) or a transpile step from Roblox's GLSL ES to desktop GLSL/SPIR-V.
- If Roblox supplies **SPIR-V directly**: no compilation is needed at all — just validation/loading, which sidesteps this entire question and is the best case.
This spike deliberately used WGSL/naga because it was the fastest path to a *working* verification of "can we get SPIR-V into a `VkShaderModule` with no SDK installed" — it answers that question (yes) but does not resolve which compiler Omnidroid needs long-term; that depends on the separate investigation into Roblox's actual shader format.

## 3. Device capability report

Single physical device, queried via `vkGetPhysicalDeviceProperties`/`Properties2`/`MemoryProperties`/`QueueFamilyProperties`/`FormatProperties` from code run on this machine (`gfxspike caps`).

**Device:** NVIDIA GeForce RTX 4060, discrete, apiVersion 1.4.325.

**Limits**

| Limit | Value |
|---|---|
| maxImageDimension2D | 32768 |
| maxUniformBufferRange | 65536 |
| maxPushConstantsSize | 256 |
| maxVertexInputAttributes | 32 |
| maxColorAttachments | 8 |
| maxSamplerAnisotropy | 16 |
| timestampPeriod (ns/tick) | 1 |
| minUniformBufferOffsetAlignment | 64 |
| nonCoherentAtomSize | 64 |
| optimalBufferCopyOffsetAlignment | 1 |

**Memory heaps**

| Heap | Size | Flags |
|---|---|---|
| 0 | 8343519232 B (~7.77 GiB) | DEVICE_LOCAL |
| 1 | 17093709824 B (~15.92 GiB) | (system RAM, not device-local) |
| 2 | 224395264 B (~214 MiB) | DEVICE_LOCAL |

**Memory types**

| Type | Heap | Flags |
|---|---|---|
| 0 | 1 | (none — not host visible, not device local: unusual/protected-style type) |
| 1 | 0 | DEVICE_LOCAL |
| 2 | 1 | HOST_VISIBLE \| HOST_COHERENT |
| 3 | 1 | HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED |
| 4 | 2 | **DEVICE_LOCAL \| HOST_VISIBLE \| HOST_COHERENT** |

Type 4 (heap 2, ~214 MiB) is a ReBAR-style host-visible + device-local pool — present, but small (~214 MiB, not the full 8 GiB VRAM heap). This matters for upload paths: a small fast-path exists for host-visible device-local allocations, but bulk texture/vertex data still needs a staged upload through heap-1 host-visible memory into heap-0 device-local memory; the app cannot assume it can map all of VRAM directly.

**Queue families**

| Idx | Count | Flags | timestampValidBits |
|---|---|---|---|
| 0 | 16 | GRAPHICS \| COMPUTE \| TRANSFER \| SPARSE_BINDING | 64 |
| 1 | 2 | TRANSFER \| SPARSE_BINDING | 64 |
| 2 | 8 | COMPUTE \| TRANSFER \| SPARSE_BINDING | 64 |
| 3 | 1 | TRANSFER \| SPARSE_BINDING \| VIDEO_DECODE_KHR | 32 |
| 4 | 1 | TRANSFER \| SPARSE_BINDING \| VIDEO_ENCODE_KHR | 32 |
| 5 | 1 | TRANSFER \| SPARSE_BINDING \| OPTICAL_FLOW_NV | 64 |

A **dedicated transfer-only queue family** (idx 1) and a **dedicated compute-only queue family** (idx 2) both exist, separate from the graphics family — the renderer can use async transfer/compute queues instead of contending with the graphics queue.

**Extension / feature support** (all `true` unless noted)

| Extension/feature | Supported |
|---|---|
| VK_KHR_swapchain | true |
| VK_EXT_descriptor_indexing (+ non-uniform indexing, partially-bound) | true |
| VK_KHR_dynamic_rendering | true |
| VK_EXT_extended_dynamic_state / state2 / state3 | true (all three) |
| VK_KHR_timeline_semaphore | true |
| VK_EXT_host_image_copy | true |
| VK_KHR_push_descriptor | true |
| VK_EXT_memory_budget | true |
| VK_KHR_external_memory_win32 | true |
| VK_EXT_shader_object | true |
| VK_KHR_maintenance4 | true |
| VK_KHR_maintenance5 | true |
| VK_EXT_texture_compression_astc_hdr | **false** |
| VK_EXT_debug_utils (instance ext) | true |

**Texture compression formats — the major finding.** Queried `vkGetPhysicalDeviceFormatProperties` optimal-tiling `SAMPLED_IMAGE` support:

| Format | Supported (optimal tiling, sampled) |
|---|---|
| ETC2_R8G8B8A8_UNORM_BLOCK | **false** |
| ETC2_R8G8B8_UNORM_BLOCK | **false** |
| ASTC_4x4_UNORM_BLOCK | **false** |
| ASTC_8x8_UNORM_BLOCK | **false** |
| BC1_RGBA_UNORM_BLOCK | true |
| BC3_UNORM_BLOCK | true |
| BC7_UNORM_BLOCK | true |

Confirms the expected worst case exactly: **this desktop NVIDIA GPU supports neither ETC2 nor ASTC**, the two formats Android games (Roblox included) actually ship textures in, but does support the desktop BC/DXT family. **Consequence: any ETC2/ASTC-compressed texture the Roblox APK ships must be transcoded at load time** — either decompressed to a plain format (RGBA8, simplest, highest VRAM/bandwidth cost) or re-encoded to a supported compressed format (BC7 for color, BC5/BC3 as appropriate — better VRAM/bandwidth but needs a real-time or load-time ASTC/ETC2→BC transcoder, e.g. something in the spirit of `astcenc`/`etc2comp`, or a fast block-transcode library). This is a hard runtime requirement, not an edge case — every compressed texture asset needs the same treatment. Not something to defer.

## 4. Presentation modes and frame pacing

Surface: 7 formats offered (incl. `B8G8R8A8_SRGB`/`SRGB_NONLINEAR`, HDR10 and extended-linear variants); app selects `B8G8R8A8_SRGB`. `minImageCount`/`maxImageCount` = 2/8.

**Present modes available: FIFO, FIFO_RELAXED, MAILBOX, IMMEDIATE** (plus an unrecognized mode value `1000361000`, likely a newer/vendor present-mode extension not decoded by this ash version — not investigated further). All four requested modes were actually granted (none silently fell back).

300-frame measurement per mode, single windowed instance, 1024×768:

| Mode | Mean frame time | Median | Max | Stdev | Approx. FPS |
|---|---|---|---|---|---|
| FIFO | 5.96 ms | 6.06 ms | 32.3 ms | 3.12 | ~168 |
| FIFO_RELAXED | 6.05 ms | 6.06 ms | 28.2 ms | 1.91 | ~165 |
| MAILBOX | 0.20 ms | 0.13 ms | 6.1 ms | 0.41 | ~5065 (uncapped) |
| IMMEDIATE | 0.24 ms | 0.15 ms | 6.6 ms | 0.50 | ~4226 (uncapped) |

**Evidence of Parsec/virtual-adapter distortion — directly observed, not hypothetical.** During the FIFO and FIFO_RELAXED runs (the two long-duration runs, each ~5 s of wall time for 300 frames), the surface's `currentExtent` drifted upward continuously across many spontaneous swapchain recreations — from 1024×768 to 1173×768 over 41 recreations (FIFO) and to 1221×768 over 14 recreations (FIFO_RELAXED) — **despite the window never being touched or resized by the test**. The short MAILBOX/IMMEDIATE runs (well under a second of wall time each) showed no drift at all (2 surface queries = just the initial one, clean 300/300 frame samples). This strongly correlates spurious `VK_SUBOPTIMAL_KHR`/extent-change events with wall-clock exposure time rather than anything intrinsic to FIFO/FIFO_RELAXED, consistent with something external (the Parsec virtual display adapter noted as present on this host, or a similar periodic desktop-resolution-negotiation mechanism) nudging window/surface geometry on a roughly fixed interval. **Practical consequence: treat the FIFO/FIFO_RELAXED numbers on this machine as contaminated** — their inflated max (32 ms / 28 ms vs. 6.1 ms / 6.6 ms for MAILBOX/IMMEDIATE) and elevated stdev are partly swapchain-recreation stalls, not real driver/compositor behavior. MAILBOX/IMMEDIATE numbers are clean by this evidence and are the more trustworthy pair for judging raw submit/present overhead. Re-measure on bare metal or with Parsec/remote-display fully disabled before trusting absolute FIFO frame-pacing numbers for the real renderer.

## 5. Multiple instances

Launched 4 independent processes simultaneously (`gfxspike run`), each opening its own native window; confirmed via 4 distinct PIDs, 4 distinct HWNDs, and a screenshot showing all 4 windows independently rendering the triangle correctly and simultaneously.

**Host memory per instance** (steady state, `run` mode, 3 s after launch): Working Set ~109–133 MB, Private bytes ~143–158 MB per process.

**GPU memory per instance:** `nvidia-smi --query-compute-apps` does not report per-process `used_memory` for these graphics-only contexts on this driver (all entries `N/A`) — a real limitation of that query for this workload type, not a gap in testing. Worked around by measuring `nvidia-smi --query-gpu=memory.used` incrementally as each instance launched: baseline 2780 MiB → +52 → +53 → +52 → +52 MiB, i.e. **a very consistent ~52 MiB VRAM per instance**.

**Frame-rate degradation under concurrency:** single-instance IMMEDIATE mode averaged 0.237 ms/frame (~4226 fps, uncapped). Four concurrent IMMEDIATE-mode 300-frame benchmarks averaged 0.42–0.58 ms/frame per instance (~1713–2362 fps each) — roughly 45–55% of single-instance per-process throughput, but **aggregate throughput across the 4 processes (~7900 fps combined) exceeded single-instance throughput**, indicating the single-instance number wasn't GPU-bound and the degradation seen is CPU-side submission/present overhead multiplying across processes, not a GPU capacity ceiling. Uncapped/IMMEDIATE numbers only — not re-measured under FIFO given the task-4 measurement contamination noted above.

## 6. Validation layer situation

Confirmed via `vkEnumerateInstanceLayerProperties`: 5 layers present (`VK_LAYER_NV_optimus`, `VK_LAYER_NV_present`, `VK_LAYER_EOS_Overlay`, `VK_LAYER_VALVE_steam_overlay`, `VK_LAYER_VALVE_steam_fossilize`) — **`VK_LAYER_KHRONOS_validation` is absent**, confirming the host fact.

**`VK_EXT_debug_utils` works without any layers**, exactly as expected: creating an instance with only that extension enabled (no layers) and a debug messenger callback still receives real, useful messages — in this case the loader/driver's own informational trace of implicit-layer insertion and the `vkCreateDevice` layer callstack setup (`VK_LAYER_NV_optimus`/`VK_LAYER_NV_present` insertion, final device selection). These are **driver/loader-native messages only** — no semantic API-misuse detection. The swapchain use-after-free bug found and fixed in task 1 produced **zero** debug_utils output before or after the crash; only validation layers (or a full driver crash + Event Log correlation, as done here) would have caught it. This is direct, measured evidence of how much harder GPU debugging is on this machine without validation: a real handle-lifetime bug was silent until it hard-crashed the driver.

**Smallest install for validation (not verified on this machine — not installed, per the task's constraint of not modifying the host beyond the spike):** the standalone `VK_LAYER_KHRONOS_validation` runtime package (distributed via LunarG as a standalone "Vulkan Validation Layers" package, or installable via `winget`/vcpkg on some platforms) is the smaller option — it installs just the layer + loader-visible manifest, no headers/glslc/full SDK. The full Vulkan SDK is a superset (adds glslc/glslangValidator, spirv-tools, RenderDoc-adjacent tooling, headers) and is worth it anyway for shader tooling (see task 2), but if validation alone is the goal, the standalone runtime package is smaller and sufficient.

## Cargo.toml (final, working)

```toml
[package]
name = "gfxspike"
version = "0.1.0"
edition = "2021"

[dependencies]
ash = "0.38"
ash-window = "0.13"
raw-window-handle = "0.6"
winit = "0.30"

[build-dependencies]
naga = { version = "22", features = ["wgsl-in", "spv-out"] }
```

Resolved (`Cargo.lock`): `ash 0.38.0+1.3.281`, `ash-window 0.13.0`, `raw-window-handle 0.6.2`, `winit 0.30.13`, `naga 22.1.0`.

## Findings that constrain the renderer design

- **ETC2 and ASTC are unsupported natively on this GPU** (measured, both families, all variants checked). Every Android/Roblox compressed texture asset must go through a load-time transcode step (to RGBA8 or to a supported BC format) before upload — this is mandatory infrastructure, not an optimization, and its cost (CPU time, temporary memory, possibly a transcode cache on disk) needs to be budgeted into asset load paths from day one.
- **Only ~214 MiB of memory is both host-visible and device-local** (ReBAR-style). Bulk uploads (textures, large buffers) cannot rely on direct mapped writes into VRAM at scale; the upload path needs a staging-buffer strategy (host-visible heap → device-local heap via transfer queue), using the dedicated transfer queue family (idx 1) to avoid contending with graphics submission.
- A **dedicated compute queue family** (idx 2, 8 queues) is available separately from graphics — worth using for any compute-based transcode/post-processing work (e.g. texture transcoding, or a compute-based ASTC/ETC2 decoder) so it doesn't serialize behind rendering.
- **GLES-vs-Vulkan implication**: whichever shader source Roblox actually ships (GLSL ES vs. SPIR-V — still being determined) directly decides the shader-compile story. If it's SPIR-V already, task 2's problem mostly disappears. If it's GLSL ES, neither naga nor a stock shaderc GLSL front end is a perfect match for GLES semantics out of the box, and this needs its own follow-up spike once the actual shader format is confirmed.
- **Present-mode choice matters for latency/CPU cost, and FIFO/FIFO_RELAXED numbers measured here are suspect** due to apparent Parsec-driven surface-extent drift; MAILBOX/IMMEDIATE numbers are clean and show sub-millisecond frame submission overhead, meaning present-mode choice is unlikely to be a bottleneck once real rendering load exists — but frame-pacing behavior should be re-verified on hardware without a remote-display adapter in the loop before finalizing.
- **Multi-instance cost is cheap and roughly linear**: ~52 MiB VRAM and ~110–160 MB host memory per idle-ish instance, with per-instance throughput dropping under concurrency but aggregate throughput scaling — supports running several guest sessions concurrently without needing per-instance GPU resource partitioning schemes, at least at this small scale (4 instances, trivial rendering load). Should be re-checked once real rendering load (not a single triangle) is in place.
- **Without validation layers, a real swapchain-lifetime bug was silent until it hard-crashed the NVIDIA driver.** `VK_EXT_debug_utils` alone gives only driver/loader informational messages, not semantic validation — expect this class of bug (resource lifetime, synchronization, invalid handle) to be materially harder to catch during Omnidroid development until validation tooling is installed.

## Recommended install

Ranked by benefit for this project:

1. **Standalone `VK_LAYER_KHRONOS_validation` runtime layer (or the full Vulkan SDK, which includes it)** — highest priority. Directly addresses the task-6 finding: a real use-after-free bug was silent and only surfaced as a driver crash. Validation layers would have caught it immediately and precisely, which matters enormously once the renderer is more than a single triangle.
2. **Full Vulkan SDK** (supersedes #1) — also brings `glslc`/`glslangValidator`, useful once Omnidroid needs a GLSL-ES-capable or more heavily optimizing shader compiler than naga, and brings other debugging tools (e.g. layers for GPU-assisted validation, best-practices layer).
3. **A short, non-deeply-nested build directory** (e.g. `C:\build\...` instead of a deep `%TEMP%` path) if `shaderc` (or any other CMake+MSVC native-build crate) is adopted later — the path-length failure hit during this spike is avoidable and will otherwise waste real debugging time.
4. Not recommended right now: installing CMake<4 or pinning `CMAKE_POLICY_VERSION_MINIMUM` project-wide just to unblock `shaderc` — naga already meets the spike's SPIR-V need with zero native toolchain; only revisit if the Roblox shader format investigation concludes GLSL (ES) source needs shaderc-grade compilation.
