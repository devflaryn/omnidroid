# Sub-project C: binder, services, and an app process that boots like on a device

Status: design, 2026-09-27. Follows sub-projects A (the Linux kernel personality, `omni-linux`)
and B (ART runs dex: `tests/b_hello_dex.rs`, and the stock Roblox APK's three `classes*.dex` load
into one `PathClassLoader`, 26,612 of 26,615 classes link, and all eleven `.so` load through that
loader with `libroblox.so`'s `JNI_OnLoad` running).

## Goal

An app's process starts the way Android starts it: `app_process64` → `RuntimeInit` →
`android.app.ActivityThread.main`, attached to a running `system_server` over binder, which
launches the app's launcher Activity. Nothing about any app is written into omnidroid: the app's
`classes*.dex` load into its `PathClassLoader`, its native libraries load through that loader,
and everything the app asks of the platform is answered by the real AOSP code that answers it on a
device.

## Where B stops, and why C is next

`dalvikvm64` does not register the framework's JNI (`libandroid_runtime`), so an app's first
framework native (`SystemClock.elapsedRealtime`, from `libroblox.so`'s `JNI_OnLoad`) has no
implementation. `app_process64` does register it, and gets as far as starting ART's daemons; then
`ProcessState` opens `/dev/binder` and terminates the process because there is none.

## Decisions

1. **Binder is a kernel driver in `omni-linux`**, as it is in Linux: `/dev/binder` with the
   version-8 UAPI (`BINDER_WRITE_READ` and the `BC_*`/`BR_*` protocol, `mmap`'d receive buffers,
   `flat_binder_object` translation, the context manager at handle 0, death notifications). The
   real `servicemanager`, `libbinder` and every AIDL service run unmodified on it.

2. **The driver is split in two**, because guest processes live in separate host processes (3):
   - a **broker** that owns what a kernel owns across processes -- nodes, references and handle
     tables, transaction routing and thread selection, the todo queues, death notification -- and
     speaks in messages (a transaction is its bytes, its offsets and its objects), and
   - a **per-process side** in each `Process` that owns what lives in that process's memory -- the
     `mmap`'d buffer area and its allocator, copying a delivered transaction into it and rewriting
     its objects, `BC_FREE_BUFFER`.
   In one host process the broker is an `Arc`; across host processes it is the same object behind
   a local socket. Tests run several guest processes in one host process.

3. **One guest process per host process** for anything that runs ART. ART puts its boot image
   near `0x70000000` and its heap below 4 GiB, and a guest address is a host address (D4), so two
   ART processes cannot share a host address space. Native daemons (`servicemanager`) may share
   one, since their mappings go above 4 GiB (the space's `mmap_base` rule).

4. **No zygote fork, at first.** Android can start an app without the zygote -- the `wrap.sh`
   path, `WrapperInit` → `RuntimeInit` → `ActivityThread.main` -- and `system_server` is started
   directly with `app_process64 ... com.android.server.SystemServer`. What the zygote adds is
   speed (a preloaded, forked image), not behaviour; a snapshot fork can come later.

5. **The real `system_server`**, not a stand-in. Where it needs something the host lacks (a
   display, input devices, audio), the gap is filled below it -- a device, a HAL, a socket --
   never by replacing a framework service with a hand-written answer.

6. **`epoll`, `eventfd`, `timerfd`** come first, with readiness for pipes, sockets, eventfd,
   timerfd and binder: every `Looper` (ART's main thread, `servicemanager`, `system_server`) sits
   in `epoll_pwait`.

## Milestones and gates

- **C1** `epoll`/`eventfd`/`timerfd`; `/dev/binder` in one host process; the real
  `servicemanager` runs as one guest process and `/system/bin/service list` in another finds it.
- **C2** `app_process64` in application mode runs a class with the framework's JNI registered:
  `ApkLoad` on the stock APK gets past `SystemClock.elapsedRealtime` into `JNI_OnLoad`'s next need.
- **C3** the broker across host processes (a local socket; file descriptors passed as host
  handles); `servicemanager` and an ART process in different host processes.
- **C4** `system_server` boots far enough to publish `activity`, `package`, `window`: each missing
  piece recorded and filled below the framework.
- **C5** `am start`-equivalent: `ActivityThread.main` of an installed APK attaches, and its launcher
  Activity's `onCreate` runs.

Sub-project D then connects D's existing graphics, input and audio bridges under that Activity and
retires the transcription.

## Testing

Each milestone has a gate test in `crates/omni-linux/tests/` running the real AOSP binaries, like
A1-A5 and B. The binder protocol has unit tests at the ioctl level (two tasks in one process, and
two processes in one host process): a transaction and its reply, a oneway transaction, an object
passed as a binder and received as a handle, a handle passed back received as a binder, a death
notification when a process ends.

## Mac

macOS arm64 does not let a process map anything below 4 GiB (a `__PAGEZERO` smaller than 4 GiB is
killed at exec, and `mach_vm_allocate` below it fails), and ART's heap must be below 4 GiB. B and C
run on Windows and Linux; the Mac needs a guest address space that is not the host's (a non-zero
fastmem base with translation at the syscall boundary, or the HVF backend) -- a separate design.
