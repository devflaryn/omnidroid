# D3b: the display -- a host IComposer3, the real SurfaceFlinger, a frame on the host

Status: design, 2026-09-27. Follows D3a (the guest has a GPU: Vulkan and ANGLE on the host's).
Part of sub-project D (`2026-09-27-display-and-host-hals-design.md`).

## Measured

With D2 and D3a, the real `/system/bin/surfaceflinger` starts, creates its RenderEngine (threaded
SkiaGL on ANGLE on the host GPU, GLES 3.2), and waits for
`android.hardware.graphics.composer3.IComposer/default`, which the image declares
(`hwc3.xml`, version 3) but whose only implementation is goldfish's. Two defects fixed on the way:
a process's property area was the snapshot of its spawn (so SurfaceFlinger never saw
`hwservicemanager.ready`), and `ro.sf.lcd_density` was unset.

## Decisions

1. **AIDL types are generated.** `tools/gen_aidl.py` turns frozen AIDL (`tools/aidl/<package>/<version>`,
   vendored from android15-release) into Rust (`src/hal/aidl/`): every parcelable, union and enum
   with its exact wire format, and for every interface a server trait + dispatcher and a client
   proxy. composer3 V3 (with graphics.common V5, hardware.common V2) first; every later HAL
   (power, health, lights, sensors, audio for C4) is the same generator over its frozen files.

2. **The host composer composes nothing itself.** `validateDisplay` changes every layer to
   `CLIENT`, so SurfaceFlinger's RenderEngine composes all of them into the client target (on the
   host GPU, D3a), and `presentDisplay` copies the client target's gralloc region (D2) into the host
   framebuffer (`hal::framebuffer`). One display: 1280x720, 60 Hz, 160 dpi, internal,
   `ColorMode::NATIVE`, `RenderIntent::COLORIMETRIC`; no HDR, no virtual displays, no readback, no
   display identification data (`EX_UNSUPPORTED`).

3. **No fences.** Composition is synchronous; a present or release fence of -1 is "already
   signalled" and, as the AOSP `ComposerClientWriter` does, is simply not reported. Incoming acquire
   fences are waited on (`poll`) before a buffer is read, then closed.

4. **Callbacks are oneway host transactions** (`Broker::host_transact_oneway`): `onHotplug`
   (connected) after `registerCallback`, and `onVsync` every 16.67 ms on a host thread while
   `setVsyncEnabled(true)`, with `CLOCK_MONOTONIC` timestamps as the guest reads them.

5. **Pixel formats.** The client target is read as RGBA_8888 (SurfaceFlinger's choice for
   `NATIVE`); another format is refused with a log line naming it, not converted wrongly.

## The gate (`tests/d3b_display.rs`)

servicemanager, hwservicemanager, the allocator and the composer (host); then the real
`surfaceflinger`, then the real `/system/bin/bootanimation` (an AOSP client: a surface from
SurfaceComposerClient, EGL through ANGLE, the default Android logo animation from framework-res).
Asserts: SurfaceFlinger registers `SurfaceFlinger` with servicemanager; the framebuffer receives
frames; a frame after the animation starts is not uniform (the logo is drawn). The screenshot is
written as a PNG for the owner to look at. Fails first: no composer, SurfaceFlinger waits.

## Then (D3c)

`system_server` boots past `StartDisplayManager` to `SystemReady` with SurfaceFlinger running.
