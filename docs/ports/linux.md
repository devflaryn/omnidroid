# Linux

Host: Ubuntu 26.04, kernel 7.0, glibc 2.43; Intel i5-4460 (4 cores, AVX2, Meltdown PTI active);
7 GB RAM + 4 GiB swap; NVIDIA Quadro 4000 (Fermi): **no Vulkan driver exists for it**, OpenGL ES
3.1 through nouveau (`NVC0`, Mesa 26.0.8, boot clocks: nouveau cannot reclock Fermi); Vulkan
tests run on Mesa lavapipe (CPU). The owner's desktop is GNOME on Wayland (Mutter, Xwayland `:0`);
audio is PipeWire. `ssh berat@192.168.0.38`, checkout `~/Desktop/omnidroid-unified`. Nothing has
run on Linux ARM64. Topic files: `linux-notes/mem.md` (memory, faults), `linux-notes/posix.md`
(files, process, network).

## What runs (measured in PS99, place 8737899170, `omnidroid play`)

* **GLES on the Quadro.** The engine refuses lavapipe by its own rule (`Device llvmpipe ... is
  emulated, skipping`, D8, not worked around) and falls back to OpenGL ES. On a quiet desktop:
  13.6-14.0 fps at 1280x720 and 17.2 at 960x540 (`OMNI_WINDOW_SIZE`); 6.8-7.8 while another GPU
  client (a GNOME Remote Desktop session) shares the card. **GPU-bound**: 96-98% busy, 71-96 ms
  of GPU per frame; the GLES layer adds ~2-3 ms of render-thread CPU and no GPU work
  (`OMNI_GLES_TIMING`, `9e7b027`).
* **l9, 30 min** (`b4918a9`, lowest graphics): no guest thread lost, clean close, gate passed;
  3.8-4.0 GiB private (`VM_ACCOUNT`) / 2.8-3.1 GiB resident, flat; ~1.9 of 4 cores.
* Linux-specific costs fixed: the engine's memory monitor's `/proc` reads are served from a 500 ms
  cache instead of a `smaps` walk each (`a12cac5`); GLES buffer maps reuse their guest shadows
  (4 KiB map/write/unmap 1,023 -> 2.9 us, native 2.4).

## Build and run

```sh
sudo apt-get install -y build-essential cmake ninja-build pkg-config rustup libvulkan-dev \
  mesa-vulkan-drivers libasound2-dev libx11-dev libxi-dev libxfixes-dev \
  xvfb xdotool imagemagick x11-utils vulkan-validationlayers   # the last line: tests only
```

* `cargo` is `/usr/bin/cargo` (no `~/.cargo/env`; in `/bin/sh` a failed `.` ends the script).
  **7 GB: one cargo build or one app run at a time.** X11 is loaded with `dlopen` (`x11-dl`) at the
  first window; ALSA is linked.
* On the desktop: `export DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.*)`, then
  `setsid nohup target/release/omnidroid play ... &` (a plain `nohup` dies with ssh). Storage:
  `$XDG_DATA_HOME/omnidroid/data` (default `~/.local/share`), accounts beside it.
* **Never start an Xvfb that can open the nouveau render node** (Fermi faulted and Xwayland hung
  until a reboot): `LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe Xvfb :99 -screen 0
  1920x1080x24 -extension GLX`.
* **`RLIMIT_NICE`**: FMOD asks `setpriority(-16)`; with Ubuntu's limit 0 the guest gets `-1`
  `EACCES`, as from a kernel. Android gives apps 40; to match it,
  `echo "$USER - nice -20" | sudo tee /etc/security/limits.d/omnidroid.conf` and log in again
  (`bionic.rs`'s `setpriority_applies_...` needs it).
* The gate hard-links the APK into the guest's root; across filesystems it copies instead.

## What differs on this host

| area | Linux |
|---|---|
| memory | `mmap(PROT_NONE)` reservations (not `MAP_NORESERVE`), commit charge = `VM_ACCOUNT` VMAs, decommit = a fresh `PROT_NONE` `MAP_FIXED` mapping, and a ledger that makes Windows' refusals (`vm/linux.rs`, `linux-notes/mem.md`) |
| guest faults | `SIGSEGV`/`SIGBUS` with `SA_SIGINFO\|SA_ONSTACK`; dynarmic installs its handler at the first jit, so `omni-cpu` re-asserts ours first after every `od_jit_new` (`fault::reassert_precedence`). Kernel soft fault 1,437 ns against Windows' 398 |
| CPU/JIT | the x64 backend, as on Windows: shared translation cache by default (D38) |
| window, input | X11 through Xlib: Xorg, Xvfb, Xwayland (so Wayland desktops); no native Wayland. Scancode = set-1 code of the physical key (keycode - 8 = evdev), raw motion from XInput 2.2 under a confined grab, text through Xlib's input method. Minimising needs a window manager |
| GPU | Vulkan through `VK_KHR_xlib_surface`. GLES: `omni-android/src/gles/` forwards every ES 2.0-3.2 and EGL call to the host EGL (`omni-gfx/src/gles.rs`, `EGL_PLATFORM_X11_KHR`), one typed caller per signature shape (`tools/gen_gles_signatures.py`) |
| audio | ALSA `default` (PipeWire's plugin), 48 kHz stereo float asked, the granted format reported; a wait asks for the frames it needs (`avail_min`: ALSA's wait is level-triggered, and waiting for one period made FMOD's feed thread spin, `e09039f`); xruns counted |
| web view | not ported (`webview/unix.rs` refuses by name; WebKitGTK would be the backend) |
| process, files, network | `linux-notes/posix.md` |
| profiling | `sampler/linux.rs` (x86-64); `OMNI_MEM_REPORT` host rows from `/proc/self/smaps` |

GLES substitutions, each measured as needed: `GL_EXT_buffer_storage` is withheld (a persistent
coherent map cannot be shadowed; the engine then runs with `Persistent 0`); the two Android EGL
config attributes are dropped when the host lacks them; seven desktop-GL names answer NULL from
`eglGetProcAddress`. 35 terrain shaders fail Mesa's strict GLSL ES compiler (the engine's source).

## Open

* The GPU is the frame-rate limit. Options: a smaller window, the lowest quality (set), NVIDIA's
  390 legacy driver (a system change for the owner), no other GPU client on the desktop.
* After a live resize the engine's GLES renderer keeps its old viewport.
* The engine's own Vulkan path on a real Linux GPU is unmeasured.
* The demand pager locks and allocates inside the `SIGSEGV` handler: sound for the synchronous
  faults it serves (the Windows VEH's bargain), not async-signal-safe in the POSIX sense.
* `vm.overcommit_memory = 2` and the default `vm.max_map_count` (65,530; this host 1,048,576) are
  argued from kernel source, not measured.

## Merge notes

Linux is in `unified`; what is still stale:

* Docs that still call the Linux backends structural: `omni-platform/src/lib.rs`, `vm/error.rs`
  (`VmError::Unsupported`), `process/mod.rs`, `fs/mod.rs`, `window/mod.rs`,
  `omni-android/src/ndk/host_window.rs`.
* `OsError::name()` uses Windows' table for every number, so on Linux errno 5 (`EIO`) prints as
  `ERROR_ACCESS_DENIED`; a unix table was backed out because `vm_seam.rs` asserts the Windows names.
* Two tests assert Windows literals and fail on Linux: `window_live.rs`'s
  `the_raw_handle_is_a_live_win32_handle`, `vulkan_device.rs`'s `SurfaceCall { system: "win32" }`.
* Mutation rows: `tools/mutate_linux.py` over `tools/lnx_rows/*.py` (ids `lnx-`).
