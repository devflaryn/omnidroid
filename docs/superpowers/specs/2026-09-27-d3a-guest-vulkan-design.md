# D3a: the GPU -- a guest Vulkan driver that forwards to the host's, with ANGLE for GLES

Status: design, 2026-09-27. Follows D2 (gralloc 5). Part of sub-project D
(`2026-09-27-display-and-host-hals-design.md`, whose "GL for the app" left ANGLE vs SwiftShader
open "until D reaches it"; D3 reaches it).

## Why D3 needs this first

SurfaceFlinger cannot initialize without a GPU driver: its RenderEngine (`skiaglthreaded`, the
image's `debug.renderengine.backend`) creates an EGL display and context at start, and aborts if
it cannot. The image's only GL and Vulkan drivers are goldfish's (`libEGL_emulation.so`,
`vulkan.ranchu.so`), which need the emulator's pipe. The image also carries **ANGLE**
(`/system/lib64/lib{EGL,GLESv1_CM,GLESv2}_angle.so`), which implements GLES on Vulkan, and the
EGL loader takes it as the system driver when `ro.hardware.egl=angle` (`Loader.cpp`: suffix
`angle` loads from `/system/lib64` in the default namespace). The Vulkan loader, `libvulkan.so`,
loads the vendor HAL `vulkan.<ro.hardware.vulkan>.so` from the sphal namespace
(`/vendor/lib64/hw`). So one missing piece -- a Vulkan driver -- gives the guest Vulkan *and*
GLES (through the image's own ANGLE), for SurfaceFlinger, HWUI and any app.

## Decision: a paravirtual Vulkan driver

omnidroid's device overlay adds `/vendor/lib64/hw/vulkan.omni.so`, an `hwvulkan` HAL module
(`hardware/hwvulkan.h`) in C. Each Vulkan command it implements is forwarded to the host's Vulkan
driver through one device, `/dev/omni-gpu`, as an `ioctl`. The host (`crate::gpu`, Rust) calls the
real host driver (`ash::Entry::load()`; the RTX's on Windows, Mesa's -- GPU or lavapipe -- on
Linux). This is the shape of a virtual GPU (virtio-gpu's venus), with the transport being a syscall
instead of a ring, because guest and host share one address space.

Rejected: SwiftShader (or Mesa lavapipe) inside the guest -- no arm64 Android build to use, and its
JIT would run under dynarmic's; forwarding GLES instead of Vulkan -- needs a host GLES (ANGLE on
Windows, none on macOS) and still leaves apps' own Vulkan unserved.

## What makes forwarding small: one address space, one struct layout

- **A guest pointer is a host pointer** (D4; fastmem is a full identity map). A struct the guest
  passes is read by the host driver where it lies, and memory the host driver maps
  (`vkMapMemory`) is used by guest code directly. No marshalling of structs or buffers.
- **Vulkan's structs have one layout on arm64 (LP64) and x86-64 (LP64 / Windows LLP64)**: Vulkan
  uses fixed-width integers, `size_t`, pointers and 32-bit enums, never `long`. So a guest struct
  *is* a host struct.
- **Non-dispatchable handles are 64-bit values** and pass through unchanged.
- The host driver reading guest-chosen pointers does not widen what guest code can do: guest code
  already runs in the host process with every host page reachable (D4 amendment 1; one instance
  per process). The host validates what it itself interprets (the command id, argument count,
  dispatchable wrappers, Android structs), not what the driver reads.

## What forwarding must translate

1. **Dispatchable handles** (`VkInstance`, `VkPhysicalDevice`, `VkDevice`, `VkQueue`,
   `VkCommandBuffer`). The Android loader overwrites the first word of each with its dispatch
   table, so the guest driver hands out a **wrapper** `{ hwvulkan_dispatch_t (magic
   0x01CDC0DE); uint64_t host; }` and the host handle lives in `host`. The guest driver makes
   wrappers for handles the host returns (the handful of commands that create them) and frees them
   when they are destroyed; the host unwraps every dispatchable handle it receives -- a parameter,
   or an array of them (`vkQueueSubmit*`'s command buffers, `vkCmdExecuteCommands`) -- by reading
   `wrapper + 8` through the checked guest-memory reader, into copies of the structs it must change.
2. **Allocation callbacks**: guest function pointers the host cannot call. The guest driver passes
   `pAllocator = NULL` always (the specification allows an implementation to ignore them only in
   the sense that a NULL is always valid).
3. **Android-only structures and extensions**, which no host driver knows: the host removes them
   from `pNext` chains (copying the chain; the guest's memory is not changed) and implements them
   itself (below). Extension names are filtered in both directions: the guest is offered the host's
   extensions that forwarding supports, minus host-platform ones (win32/xlib/metal surfaces,
   external memory by win32 handle), plus the Android ones implemented here.
4. **Floats** in scalar parameters (`vkCmdSetLineWidth`, `vkCmdSetDepthBias`, ...) travel as their
   bits; the host calls each command through its exact generated signature.

## The transport

`ioctl(fd, OMNI_GPU_CALL, struct omni_gpu_call *c)`:

```c
struct omni_gpu_call {        /* 32 bytes */
    uint32_t command;         /* the command's id in the generated table */
    uint32_t argc;            /* how many 64-bit arguments follow at args */
    uint64_t args;            /* guest address of uint64_t[argc] */
    uint64_t result;          /* out: the return value (VkResult, PFN, ...) */
    uint64_t reserved;
};
```

`OMNI_GPU_CALL = _IOWR('G', 0x01, struct omni_gpu_call)`. An unknown id or a wrong `argc` is
`EINVAL` (and the guest driver reports `VK_ERROR_INITIALIZATION_FAILED`/`DEVICE_LOST`). Each
ioctl runs on the calling guest thread's host thread, so a blocking command (`vkWaitForFences`)
blocks only that thread.

## Generated code

`tools/gen_vk_forward.py` reads the Vulkan registry **`vk.xml` 1.3.275** (the NDK r28c headers'
version) and emits, from one command list with stable ids:

- `crates/omni-linux/device/src/vk/generated.c`: a guest entry point per forwarded command, packing
  its arguments into `uint64_t[]` and making the call; the name -> function table the driver's
  `GetInstanceProcAddr`/`GetDeviceProcAddr` answer from.
- `crates/omni-linux/src/gpu/generated.rs`: per command, its exact `extern "system"` signature and
  a dispatcher that unwraps dispatchable parameters and calls the host function pointer.

The command list is core 1.0-1.3 plus the device and instance extensions that are platform-neutral;
a command whose parameters or structs need more than the rules above is marked **special** and
written by hand on both sides (list below). The generator refuses a command it cannot classify.

## Hand-written (special) commands

- **Instance and device lifetime:** `vkCreateInstance` (filter extension names, request the host
  API version), `vkEnumerateInstanceExtensionProperties`/`Version`, `vkEnumeratePhysicalDevices`
  (+`Groups`), `vkCreateDevice` (filter extension names, strip Android chain structs),
  `vkGetDeviceQueue`(`2`), `vkAllocateCommandBuffers`, `vkFreeCommandBuffers`, the `vkDestroy`s of
  dispatchables, `vkEnumerateDeviceExtensionProperties`, `vkGet{Instance,Device}ProcAddr` (guest
  only).
- **Queries that carry Android outputs:** `vkGetPhysicalDeviceProperties2` (strip
  `VkPhysicalDevicePresentationPropertiesANDROID`, answer it: shared presentable image false),
  `vkGetPhysicalDeviceImageFormatProperties2` (strip and answer
  `VkAndroidHardwareBufferUsageANDROID`; reject the AHB external handle type for formats gralloc
  cannot allocate).
- **Submission:** `vkQueueSubmit`, `vkQueueSubmit2`, `vkCmdExecuteCommands` (command buffer
  arrays), plus the gralloc synchronisation below.
- **`VK_ANDROID_native_buffer`** (spec version 8; what the loader's swapchain needs):
  `vkGetSwapchainGrallocUsage{,2,3,4}ANDROID` (CPU read/write + texture + render usage),
  `vkCreateImage` with `VkNativeBufferANDROID`, `vkAcquireImageANDROID` (the fence fd is waited on
  and closed; the semaphore/fence are signalled by an empty submit), and
  `vkQueueSignalReleaseImageANDROID` (the image's pixels are written into the gralloc buffer; the
  returned fence fd is -1, already signalled).
- **`VK_ANDROID_external_memory_android_hardware_buffer`** (ANGLE's `EGLImage` from a buffer, which
  SurfaceFlinger's RenderEngine uses for every layer and for its output):
  `vkGetAndroidHardwareBufferPropertiesANDROID`, memory import (`VkImportAndroidHardwareBufferInfoANDROID`),
  `vkGetMemoryAndroidHardwareBufferANDROID` (export: the host allocates a gralloc buffer through D2's
  allocator).
- **Sync fds** (`VK_KHR_external_semaphore_fd`, `VK_KHR_external_fence_fd`, `SYNC_FD` only; what
  `EGL_ANDROID_native_fence_sync` becomes in ANGLE): an export first waits for the work to finish
  and answers -1 (a signalled sync file); an import of -1 signals; an import of any other fd waits
  for it (`poll`), closes it, and signals.

## Gralloc buffers on the host GPU

A gralloc buffer is a `shm` region (D2). The host GPU cannot render into a guest file, so an
image on a gralloc buffer is a **host image with a mirror**: a `VK_IMAGE_TILING_LINEAR`,
host-visible image of the buffer's format and stride, whose mapped memory the host copies to and
from the region's pixels:

- **Into the GPU:** before a submit, every gralloc image of that device whose region changed since
  its last upload is uploaded. "Changed" is the region's **content generation**, a `u64` at
  offset 4088 of the metadata page (D2's layout; nothing else lives there), which every writer
  bumps: `mapper.omni.so`'s unlock after a CPU-write lock, and the host after a download.
- **Out of the GPU:** after a submit that wrote a gralloc image (it is an attachment of a render
  pass or dynamic rendering begun in, or the destination of a transfer recorded in, one of the
  submitted command buffers -- the host records this as it forwards those commands), the host waits
  for the queue and copies the image's memory into the region, bumping the generation.

Linear, host-visible images keep every copy a `memcpy` and avoid tracking layouts. Zero-copy
(importing the region's host mapping with `VK_EXT_external_memory_host`) is an optimisation for
later, measured first.

Formats: `RGBA_8888`, `RGBX_8888`, `IMPLEMENTATION_DEFINED` -> `R8G8B8A8_UNORM`; `BGRA_8888` ->
`B8G8R8A8_UNORM`; `RGB_565` -> `R5G6B5_UNORM_PACK16`; `RGBA_FP16` -> `R16G16B16A16_SFLOAT`;
`RGBA_1010102` -> `A2B10G10R10_UNORM_PACK32`; `R_8` -> `R8_UNORM`.

## Device properties

`ro.hardware.egl=angle` and `ro.hardware.vulkan=omni`, set with the device's other properties
(`props.rs`, where `ro.hardware` is). The image's `init.ranchu.rc` would set them from `ro.boot.*`,
but init's `setprop`s are not run here (`init.rs`).

## The gate (`tests/d3a_gpu.rs`)

The real `servicemanager` and D2's allocator, then three NDK fixtures through the real loaders:

1. `vkclear`: `vkCreateInstance` through `libvulkan.so`, a device, an image cleared to a colour,
   copied to a buffer, mapped, checked; prints the device name (the host GPU's).
2. `glclear`: `eglGetDisplay`/`eglInitialize` (ANGLE), a pbuffer, an ES 3 context, `glClear`,
   `glReadPixels`, checked; prints `GL_RENDERER` (ANGLE's, naming the host GPU).
3. `ahbrender`: an `AHardwareBuffer` (D2), an `EGLImage` from it, a framebuffer on it, `glClear`,
   `glFinish`; then the buffer locked for CPU read shows the colour -- and the host reads the same
   colour out of the buffer's `shm` region.

Fails first: no `vulkan.omni.so`, so `vkCreateInstance` answers `VK_ERROR_INCOMPATIBLE_DRIVER`.

## Hosts

Windows (RTX 4060) and Linux (Quadro 4000 has no Vulkan: Mesa's lavapipe, which is the "no GPU
required" case). macOS: MoltenVK would serve, but ART does not run there yet (the low-4-GiB
design), so D3a is verified on Windows and Linux.
