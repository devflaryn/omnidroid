# Graphics spike: native window + Vulkan on the development host

Measured with a throwaway `ash` 0.38 + `winit` 0.30 program (not in the repo) on the Windows
development host: Windows 11 Pro 10.0.26200, NVIDIA RTX 4060, driver 591.86, Vulkan loader
1.4.321 / device 1.4.325. The repo itself uses `ash` (`loaded`, no SDK needed) and its own Win32
window (`omni-platform::window`), not `winit`. Figures are for this host only.

## 1. Window, triangle, resize

- A resizable window with a Vulkan triangle rendered correctly, including after live resizes.
- **Swapchain recreation bug that crashed the driver.** Destroying the old `VkSwapchainKHR`
  *before* passing it as `oldSwapchain` to `vkCreateSwapchainKHR` crashed the NVIDIA driver on
  the first live resize, every time (`nvoglv64.dll`, `0xc0000409`, seen only in the Windows
  Event Log; no other diagnostic). Fix: destroy framebuffers/views first, create the new
  swapchain with `oldSwapchain = old`, then destroy the old handle. Verified with six consecutive
  live resizes from 500x400 to 1250x900. `omni-gfx::vulkan` follows this order.
- `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT)` returns solid black for the client area of a
  flip-model Vulkan swapchain on this host. Only a screen capture (`CopyFromScreen`) of the
  active window shows presented pixels.

## 2. Shader compilation

No Vulkan SDK, `glslc` or `glslangValidator` on this host. `naga` (pure Rust, WGSL to SPIR-V from
`build.rs`) worked with no native toolchain; building `shaderc` from source failed on CMake 4 and
Windows path length. Moot for the runtime: Roblox ships its own SPIR-V (D8) and `omni-gfx` has no
shaders and no shader compiler in the build.

## 3. Device capabilities

RTX 4060, discrete, apiVersion 1.4.325.

| Limit | Value |
|---|---|
| maxImageDimension2D | 32768 |
| maxUniformBufferRange | 65536 |
| maxPushConstantsSize | 256 |
| maxColorAttachments | 8 |
| maxSamplerAnisotropy | 16 |
| minUniformBufferOffsetAlignment / nonCoherentAtomSize | 64 / 64 |

Memory: three heaps, five types.

| Type | Heap (size) | Flags |
|---|---|---|
| 0 | 1 (15.92 GiB system RAM) | none |
| 1 | 0 (7.77 GiB VRAM) | DEVICE_LOCAL |
| 2 | 1 | HOST_VISIBLE, HOST_COHERENT |
| 3 | 1 | HOST_VISIBLE, HOST_COHERENT, HOST_CACHED |
| 4 | 2 (~214 MiB) | DEVICE_LOCAL, HOST_VISIBLE, HOST_COHERENT (ReBAR window) |

Only ~214 MiB is both host-visible and device-local, so bulk uploads need staging.

Queue families (six):

| Idx | Count | Flags |
|---|---|---|
| 0 | 16 | GRAPHICS, COMPUTE, TRANSFER, SPARSE_BINDING |
| 1 | 2 | TRANSFER, SPARSE_BINDING |
| 2 | 8 | COMPUTE, TRANSFER, SPARSE_BINDING |
| 3 | 1 | TRANSFER, SPARSE_BINDING, VIDEO_DECODE |
| 4 | 1 | TRANSFER, SPARSE_BINDING, VIDEO_ENCODE |
| 5 | 1 | TRANSFER, SPARSE_BINDING, OPTICAL_FLOW_NV |

Supported: `VK_KHR_swapchain`, descriptor indexing, dynamic rendering, extended dynamic state
1/2/3, timeline semaphores, `VK_EXT_host_image_copy`, push descriptors, memory budget,
`VK_KHR_external_memory_win32`, shader object, maintenance4/5, `VK_EXT_debug_utils`. Not
supported: `VK_EXT_texture_compression_astc_hdr`.

**Compressed textures (optimal tiling, sampled):** ETC2 RGB8/RGBA8 and ASTC 4x4/8x8 are **not
supported**; BC1, BC3, BC7 are. So every ETC/ASTC texture must be transcoded before upload
(D27; `omni-texture` decodes `GL_ETC1_RGB8_OES` to RGBA8, see `texture-formats.md`).

## 4. Present modes and frame pacing

Surface: 7 formats (the program chose `B8G8R8A8_SRGB` / `SRGB_NONLINEAR`),
`minImageCount`/`maxImageCount` = 2/8. Present modes FIFO, FIFO_RELAXED, MAILBOX and IMMEDIATE
were all offered and all granted when requested. 300 frames, one window, 1024x768:

| Mode | Mean | Median | Max |
|---|---|---|---|
| FIFO | 5.96 ms | 6.06 ms | 32.3 ms |
| FIFO_RELAXED | 6.05 ms | 6.06 ms | 28.2 ms |
| MAILBOX | 0.20 ms | 0.13 ms | 6.1 ms |
| IMMEDIATE | 0.24 ms | 0.15 ms | 6.6 ms |

**Surface extent drifts with the window untouched.** During the ~5 s FIFO run `currentExtent`
grew from 1024x768 to 1173x768 across 41 swapchain recreations (14 in FIFO_RELAXED), with no
resize by the test. Attributed to the Parsec virtual-display adapter renegotiating the desktop.
Consequence: window size must be read from the OS when needed, not remembered from the last
resize event, and the FIFO maxima above include recreation stalls.

## 5. Multiple instances

Four processes rendered in four windows at once. Per instance: ~52 MiB VRAM (measured as the
`nvidia-smi` total rising 52-53 MiB per launch), 109-133 MB working set, 143-158 MB private
bytes. Per-process IMMEDIATE throughput fell to ~45-55% of a single instance, but aggregate
throughput (~7,900 fps) exceeded one instance's (~4,226 fps): the cost is CPU-side submission,
not a GPU ceiling.

## 6. Validation layers

`vkEnumerateInstanceLayerProperties` reports five layers (`VK_LAYER_NV_optimus`,
`VK_LAYER_NV_present`, `VK_LAYER_EOS_Overlay`, `VK_LAYER_VALVE_steam_overlay`,
`VK_LAYER_VALVE_steam_fossilize`). **`VK_LAYER_KHRONOS_validation` is absent.**
`VK_EXT_debug_utils` works without layers but only relays loader/driver informational messages:
the swapchain use-after-free in §1 produced **zero** debug output before the driver crashed.
So on this host, Vulkan misuse (lifetimes, synchronisation, cross-device handles) is undefined
behaviour with no diagnostic, which is why the `omni-gfx` and `omni-android::vulkan` code checks
such rules itself. `Renderer::new` enables validation only when the layer is present. Installing
the standalone validation layer (or the full Vulkan SDK) would change this.
