# The game's frame to the window without the CPU (design, 2026-10-09)

Goal (coordinator): when the game is the only real layer, take the system host process off the
per-frame path. Branch `worktree-agent-ab868a756486efc2e`, on `perf/ps99-60fps`.

## Today, per frame (1575x890, 5.6 MB a full-screen buffer)

| where | what | CPU |
|---|---|---|
| game host process, app thread | `vkQueueSignalReleaseImageANDROID`: GPU copy image -> host-cached staging (`gpu::native::release`) | submit only |
| game host process, `omni-gpu-release` | wait the fence, **memcpy staging -> gralloc region** (`Landing::land`) | **1.04 ms** (probe, E-core) |
| system host, SurfaceFlinger's present (binder) | `wait_written`, compose both layers from the regions into the framebuffer (`compose_zero`) | 1.8-2.8 ms (`frame_cost`, E-core) |
| system host, `omni-display-present` + window thread | GDI: swizzled copy + `StretchDIBits` (default) / `StretchDIBits` only (`present_bgra`) | 2.4-3.1 / 1.3-1.5 ms (`present_cost`) |

## Measured on this host (Windows 11, RTX 4060, driver 591.86)

- Extensions: `VK_EXT_external_memory_host` (min alignment 0x1000), `VK_KHR_external_memory_win32`,
  `VK_KHR_external_semaphore_win32`, `VK_KHR_timeline_semaphore`, `VK_KHR_win32_keyed_mutex`.
- **A gralloc region's host view imports as Vulkan memory**, and a GPU write into it reads back
  through `Shm::read_at`, i.e. as the composer and any other host process see the region
  (`gpu::window_present::tests::a_gralloc_region_imports_as_gpu_memory`, memory types 2|3).
- GPU copy of a frame from device-local memory: into host-cached staging 0.52 ms, into the
  imported region 0.55 ms (same PCIe write either way).
- **(b) GPU present from the CPU frame (`present_gpu`) saves no CPU**: 2.1 ms at 1:1 against 1.3-1.5
  for GDI with `present_bgra`; at parity when stretched (1.5-1.8 vs 1.6-1.8). Its cost is the copy
  into host-visible memory (~1.1 ms) plus ~0.9 ms of driver present. It only pays once nothing is
  copied into it.

## Landed

- **A0 `gralloc_direct`** (lever, off; devices need `OMNI_GRALLOC_DIRECT=ready|1`): the release's
  copy goes straight into the gralloc region (its view imported, `Shm::pin_view`), the worker
  copies nothing. Removes 1.04 ms of CPU and ~1 ms of latency before `wait_written` returns.
  `special.rs::create_device` appends `VK_EXT_external_memory_host` (+`VK_KHR_external_memory`)
  only when asked and offered, and makes the device again without them if refused.
- **(b) `present_gpu`** (lever, off): `gpu::window_present::WindowPresenter`, the window's own
  Vulkan swapchain (MAILBOX, GPU blit-scale, two frames in flight); GDI canvas cleared while on;
  falls back to GDI on any error. The GPU end that (a) needs.

## (a) The rest: the game's image shown by the system process's GPU

**A1. A share image per gralloc buffer** (game host process, `native::attach` / `release`).
Beside the region import, a device-local image of the buffer's size and format, allocated with
`VkExportMemoryAllocateInfo{OPAQUE_WIN32}` + `VkExportMemoryWin32HandleInfoKHR{name}` with a
**named** handle `Local\omni-gralloc-<host pid>-<n>`: the system process opens it by name
(`VkImportMemoryWin32HandleInfoKHR{name}`), so no `DuplicateHandle` and no handle channel between
host processes is needed. The name and the image's format/size go into the region's metadata page
(a new field beside `NAME_AT`, e.g. at 3072, with a version word), written once at attach. The
release records one more command: `vkCmdCopyImage` guest image -> share image (GPU-local, ~0.05 ms)
before the copy into the region. The guest's own image is not changed (no extra chain on its
`vkCreateImage`), so nothing the guest sees differs. Same-GPU check: both processes compare
`VkPhysicalDeviceIDProperties::deviceUUID/driverUUID` (written in the metadata page too).

**A2. Sync** with an exported **timeline semaphore** per share image
(`VK_KHR_external_semaphore_win32`, also named): the game's copy into the share image waits for
value `2k` (the system's last read done) and signals `2k+1`; the system's blit waits `2k+1` and
signals `2k+2`. The region's generation protocol stays as it is for every CPU reader. A system
process that never imports a buffer is never waited for: the game waits only on values the system
has promised (it writes the promised value into the metadata page when it imports).

**A3. The composer's GPU frame** (`hal::composer::present`): a device frame is GPU-presentable when
the window is on `present_gpu`, its bottom layer is a buffer layer with a share image covering the
display at 1:1 (opaque, or premultiplied at plane alpha 1 -- `compose::cover_start`'s rule), and
every other layer is **known transparent**: a per-buffer cache of "all alpha 0" keyed by the
region's content generation, filled by one run-scan (`blend_row_runs`' 16-byte test, ~0.3 ms) when
the generation changes -- the app's window layer changes rarely in a world. Then the composer does
not read or compose: `Framebuffer::present_external(w, h, share)` counts the frame (`[display]`
line unchanged), records which share image is the frame, and marks the pixels stale; a reader that
wants pixels (`png`, `pixels`, `frame`) composes them on demand from the regions (the CPU path,
every 5 s at most for `OMNI_SCREENSHOT`). The present thread sees a shared frame and calls
`WindowPresenter::present_shared` (import by name once, blit, present). Not eligible (a visible
overlay, a scaled layer, chrome shown): the CPU path as today, same frame.

**A4. Overlays on the GPU**, after A3 is measured: the app window layer uploaded only when its
generation changes (one `memcpy` into the presenter's staging, rare) and blended over the game image
by a fullscreen-triangle pipeline with premultiplied blending. SPIR-V embedded as `u32` words (two
~300-byte shaders), no shader compiler in the build.

**Expected**: system host per in-world frame from ~3-4 ms (compose + present, E-core) to ~1 ms
(the driver's present); the game host process's release worker from ~1 ms of copying to none (A0).
The in-world fps effect is bounded: the system host process is not the game's critical thread, so
the win is CPU and memory bandwidth freed for the game's worker and render threads, plus ~1 ms of
present latency.

**Tests per step**: A1 two Vulkan devices in one test process (export by name on one, import on the
other, copy, read back); then across processes (the test spawning itself). A2 a two-device
ping-pong on the timeline. A3 the composer's eligibility (unit), `present_external` + on-demand
compose bit-identical to the CPU path (unit), a window run (`present_cost`-style) comparing CPU.

## In-world A/B for the coordinator

1. `present_bgra=0|1` with `compose_zero=1` (already the best CPU path for the window).
2. Boot with `OMNI_GRALLOC_DIRECT=ready`, then `gralloc_direct=0|1`: the game host process's CPU
   (`omni-gpu-release` thread) and `[display]` fps; look for `[gpu] gralloc_direct: device ... can
   import gralloc regions` and no "could not be imported" line.
3. `present_gpu=0|1` only as a check that the swapchain path works in-world (window content,
   resizes, `[window] present_gpu` lines); not expected to lower CPU on its own.
