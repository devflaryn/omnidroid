# Sub-project D: the display, and host-side HALs

Status: design, 2026-09-27. Follows C (binder, services, `system_server` booting ~27 services in).

## The wall C hit

`system_server` boots as far as `StartDisplayManager` and then every later service blocks on
`SurfaceFlingerAIDL`, which never appears because `surfaceflinger` aborts at start. It aborts
because the prebuilt AOSP image is the **emulator (ranchu) image**, whose graphics HALs are the
only ones it ships:

- composer: `android.hardware.graphics.composer3-service.ranchu`
- allocator: `android.hardware.graphics.allocator@3.0-service.ranchu`
- EGL/GLES: `/vendor/lib64/egl/lib{EGL,GLESv2}_emulation.so`

All three talk to the emulator's virtual GPU over `/dev/goldfish_pipe` (the gfxstream/rcPipe
protocol), which this runtime does not provide. Other vendor HALs (audio, sensors, ...) abort the
same way, on the same emulator transports. The image has a CPU gralloc (`gralloc.default.so`) and
ANGLE (`libEGL_angle.so`) but **no software composer service** and no goldfish-free allocator
service, so nothing brings SurfaceFlinger up out of the box.

## Decision: host-side HALs on the binder broker

A guest process cannot serve these HALs (there is no goldfish hardware for the shipped `.ranchu`
services to drive), so **omnidroid provides them itself, from the host, as binder services the
broker owns.** The broker ([`crate::binder`]) already routes transactions between processes; a host
service is one more endpoint on it:

1. **A host binder endpoint.** The broker gains a host-owned node: host Rust code registers a node,
   receives the transactions sent to it (code, parcel bytes, objects), and sends replies -- the
   same `Work`/reply path a guest looper uses, driven by a host thread instead of a guest one.

2. **A host client of `servicemanager`.** To publish a HAL, the host issues `addService` to
   servicemanager (handle 0) -- one binder transaction with the AIDL `IServiceManager` parcel
   (interface token, instance name, the host node as a `flat_binder_object`, flags). A guest's
   `waitForService`/`getService` then hands the guest a handle to the host node.

3. **The HALs, in Rust, composing into omnidroid's framebuffer.** `IAllocator`/`IMapper` back
   graphics buffers with `crate::shm` regions (already live shared memory across processes).
   `IComposer3` accepts layers and `presentDisplay`, and composes the client target buffer into the
   headless framebuffer omnidroid already renders and screenshots. A single fixed display is
   reported (the `OMNI_WINDOW_SIZE`, 1280x720 at 160 dpi). Fences are satisfied synchronously
   (a present completes before it returns), since composition is on the host and immediate.

   This is the same framebuffer the omni-android path draws into, so the notebook's screenshot and
   headless controls work unchanged once an app renders through it.

The alternative -- implementing `/dev/goldfish_pipe` and the gfxstream protocol -- reproduces the
emulator's GPU transport and then still needs a host GL to service it. The host-HAL route is
smaller, needs no emulator hardware, and is the general mechanism for **every** HAL an app asks for
(audio, sensors, vibrator, ...): each becomes a host service on the broker, so an arbitrary APK
finds the platform services it expects without goldfish hardware.

## GL for the app

With the composer and allocator host-side, the app's own rendering still needs an EGL/GLES driver
that does not use goldfish. Two routes, decided when D reaches it:

- **ANGLE** (`libEGL_angle.so`, in the image) over a host-provided backend, or
- **SwiftShader** (CPU) staged into the image's `egl/` directory.

Either renders into a gralloc buffer (a `shm` region), which the host composer presents. No GPU on
the host is required; a GPU, when present, accelerates the host backend only.

## Milestones

- **D1** the host binder endpoint and a host client of servicemanager: a host service is
  registered and a guest `service check`/`getService` finds it. Gate: a host "echo" service
  answers a guest transaction.
- **D2** `IAllocator`/`IMapper`: a guest allocates a buffer, whose memory is a `shm` region both
  sides map.
- **D3** `IComposer3`: SurfaceFlinger initializes against the host composer; `system_server` boots
  past DisplayManager to `SystemReady`.
- **D4** an app's EGL/GLES renders into a gralloc buffer the host composer presents; a screenshot
  shows it.
- **D5** the app's launcher Activity via `am start`, drawn through the host display.

## Mac

Unchanged from C: macOS arm64 maps nothing below 4 GiB, where ART's heap must be, so B, C and D run
on Windows and Linux; the Mac needs the separate low-memory design (a non-identity guest base with
translation at the syscall boundary, or the HVF backend).
