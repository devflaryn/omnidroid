# Checkpoint/restore of a booted, idle device -- feasibility (2026-10-01)

Question (owner's overnight brief, lever 6): can a booted, idle Android device be saved and
brought back in seconds, instead of booting a saved `/data` (~1-2 min)?

## What a running device is

A device is several **host processes**, each a user-space Linux kernel for the guest processes it
runs (`crates/omni-linux`):

- the **system's host process** (`omni-linux-run --zygote --init ...`): init and its ~45 services,
  system_server, SurfaceFlinger, the host-served HALs (gralloc, composer), the zygote's socket;
  every guest process of it shares its one host address space ("a guest address is a host
  address", `fork.rs`);
- one **app host process** per app process the zygote starts (`zygote::launch`), talking to the
  system's binder over TCP (`crate::remote`) with a per-process credential.

The state to capture is therefore, per host process: guest memory (mapped regions, their
protections, file-backed mappings of the sysroot and the instance), every guest thread's CPU
context (dynarmic's JIT state, which can be rebuilt from registers), and the emulated kernel's
objects -- fds of every kind (`FileKind`: host files, pipes, unix and inet sockets, epoll, timerfd,
eventfd, binder, ashmem, sync files, fuse), binder nodes/refs/transactions in flight, futex
waiters, signal state, timers, the property area. Plus what lives in the host: TCP connections
between host processes (remote binder), host sockets of guest inet sockets (`hostnet`), the GPU
(Vulkan/GL objects and swapchain in `crate::gpu`, the window), the audio device.

## Options

1. **Serialise it ourselves** (omnidroid owns the whole kernel, unlike CRIU which must extract it
   from Linux). Feasible in principle and host-independent (Windows too), but the surface is every
   kernel object above plus GPU objects that cannot be serialised at all (they would have to be
   recreated and SurfaceFlinger's buffers re-imported). Weeks of work, and every new syscall
   feature would need a serialiser. **Not a night's lever.**
2. **CRIU on Linux hosts.** On Linux the host processes are ordinary processes: CRIU can dump and
   restore their memory, threads and most fds, and established TCP between them (repair mode,
   `CAP_CHECKPOINT_RESTORE` or root; Colab runs as root). The blockers are what CRIU cannot dump:
   DRM/GPU device fds (the NVIDIA EGL path) and X11 connections (a window). A **headless device on
   Mesa llvmpipe with a surfaceless/pbuffer EGL** (the Colab notebook's shape) has neither: its GL is
   plain process memory. Restore would cost roughly the device's resident memory read back (~3-4 GB,
   seconds) instead of a ~2-3 min boot on 2 vCPUs. Risks: timers and `CLOCK_MONOTONIC` jumps
   (Android's watchdogs, binder timeouts), the host-side TCP ports reused on restore, and the
   per-boot credentials (`remote::issue_credential`) -- all inside one restored process tree, so
   consistent. **The promising path for Linux/Colab: a 1-2 day spike** (`criu dump --tree <omnidroid
   aosp pid> --tcp-established --shell-job`, restore, then a control-channel command to prove the
   device answers).
3. **Windows.** No fork, no CRIU: only option 1. The warm device (booted once, kept idle, apps
   installed and started on it through its control channel) is the Windows answer, and what this
   branch lands.

## Recommendation

Keep the warm device as the product path on every host (it needs no restore at all while it
lives). For the cold path on Linux notebooks, spike CRIU on a headless llvmpipe device; on Windows,
the cold path is the saved `/data` plus a shorter boot (this branch's boot levers).
