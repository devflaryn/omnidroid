# Linux

Host: Ubuntu 26.04, kernel 7.0, glibc 2.43; Intel i5-4460 (4 cores, AVX2, Meltdown PTI active);
7 GB RAM + 4 GiB swap; NVIDIA Quadro 4000 (Fermi): **no Vulkan driver exists for it**, OpenGL ES
3.1 through nouveau (`NVC0`, Mesa 26.0.8, boot clocks: nouveau cannot reclock Fermi); Vulkan
tests run on Mesa lavapipe (CPU). The owner's desktop is GNOME on Wayland (Mutter, Xwayland `:0`);
audio is PipeWire. `ssh berat@192.168.0.38`, checkout `~/Desktop/Omni Apps/omnidroid` (was `~/Desktop/omnidroid-unified`). Nothing has
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

## The real-AOSP path on Linux (2026-09-29, branch `linux-port`)

Checkout `~/Desktop/Omni Apps/omnidroid`, `OMNIDROID_DYNARMIC_BUILD_DIR=~/od-dynarmic-linux-port`,
sysroot `OMNI_SYSROOT=~/aosp-sysroot/aosp-35`. The release build had no Linux compile errors.

```sh
export OMNI_SYSROOT=~/aosp-sysroot/aosp-35 OMNIDROID_DYNARMIC_BUILD_DIR=~/od-dynarmic-linux-port
target/release/omnidroid aosp --apk ~/Desktop/Roblox-2.740.931.apk --cookie ~/Desktop/cookies/<name>.txt --place 8737899170 --minutes 45
```

`omnidroid aosp` is `tools/aosp_play.ps1` for every host (the `r_roblox` session). Ubuntu's `/tmp`
is a 3.6 GB tmpfs, so with `TMPDIR` unset the instance goes to `~/.local/share/omnidroid/aosp`
(log `omni-linux-r-<pid>.log`, screenshots `omni-linux-r-<pid>-shots/`); graphics regions go to
`/dev/shm` (`shm::host_dir`). Leftover `omni-shm-*` there are RAM: the launcher removes them.

**The GL backend** (`OMNI_GPU=vulkan|gl|auto`, `gpu::backend`). This Quadro has no Vulkan driver,
and ANGLE on lavapipe faulted in SurfaceFlinger (D3b, open before). `auto` now takes GL here: the
guest's `libGLES_omni.so` (`device/src/gl/`, NDK r28c Linux) forwards every GLES command to the
host's GLES (`gpu::gl`, Mesa on NVC0 via `EGL_PLATFORM_DEVICE_EXT`, GLES 3.1), and the device has no
Vulkan (`ro.hardware.vulkan` empty, Vulkan feature files left out). Window surfaces are host pbuffers
read back into the window's gralloc buffer at `eglSwapBuffers`; EGLImages are host textures (read
back at flush points once they are render targets). Gate `tests/d3g_gl_fallback.rs` 2/2.

Measured (runs `omni-linux-r-720229`, `-749163`, 2026-09-29, APK 2.740.931, WARP `loc=TR`):

| step | result |
|---|---|
| boot | bootanimation drawn by the guest's GLES on the Quadro and shown in the live window (the Linux `display_window`/`Presenter`, first run here); `sys.boot_completed=1` ~188 s after start |
| install, start | `pm install` 0; the stock 15 s `bindApplication` timeout ANR-killed Roblox twice at first (system_server at 100% of a core) -> `ro.hw_timeout_multiplier` 5 on < 8 CPUs (`props::timeout_multiplier`) |
| sign-in | cookie planted once the app's store exists (`r_roblox`); `DID_LOG_IN` (countryCode TR) ~466 s after start |
| engine | Vulkan: "Unable to pick Vulkan device" (none, by design) -> GLES: "OpenGL ES 3.1 Mesa 26.0.8"; its loading screen drawn; composer up to ~14-15 presents/s |
| join | `gamejoin.roblox.com/v1/join-game` **403** (a security challenge, the engine logs `challengedByGcs`), 4 runs of 4 -- the 4th with Android's default cached processes (the lean device reaps a WebView process right after the 403), same screen; the app's "Security" screen then shows "Unable to contact server". The WebView has network (host TCP to Roblox and CloudFront, IPv6 included). **Not worked around** (VERIFICATION rules): the owner's to look at |
| memory | ~4.3 GB RAM + ~0.9 GB swap for the device, signed in (system `free`, 1.5 GB desktop baseline subtracted) |

Also found: `svc` (app_process) aborts in the system host process at boot (pre-existing, Windows
too). The old path (`omnidroid play`) cannot run 2.740.931: its first `libzstd`-side constructor
imports `dladdr`, which the compatibility layer does not implement (any host).

**Suite** (`cargo test --release --workspace --no-fail-fast`, this host, 2026-09-29, before the vDSO
threshold fix): **2,778 passed, 32 failed, 153 ignored**. `x86_64-pc-windows-msvc` and
`aarch64-apple-darwin` type-check (`cargo check --tests -p omni-linux -p omnidroid`, dynarmic's
build script overridden through its `links` key). The 32:

* **No stock APK here** (`Roblox-2.738.1397.apk` is not on this machine; the old-path gates are
  pinned to it and refuse to skip), 18: `omni-android` `gameactivity` (10: `initialize_native_code_...`,
  `dex_shape_...`, `every_ndk_symbol_...`, `facial_age_...`, `the_application_name_...`,
  `the_idle_timer_...`, `the_mouse_lock_state_...`, `the_mouse_natives_...`, `the_scan_codes_...`,
  `the_touch_native_...`), `initializers` (3), `jni_startup` (2), and the libroblox fixtures:
  `omni-elf` `cache_sharing_linux`, `loader_commit_linux`, `relro_linux` (`writing_to_sealed_relro_...`).
* **This host's setup**: `bionic` `setpriority_applies_the_nice_value_...` (RLIMIT_NICE 0, see above);
  `omni-cpu` `exclusive` `no_increment_is_lost_under_value_compare` (no store-exclusive failed across
  16 threads on 4 cores: no contention to prove anything with); `omni-mem` `around_host`
  `a_space_steps_around_a_host_allocation_...` (the fixed test address is taken on this host, EEXIST).
* **Expectations that no longer match `omni-android`'s adapter** (platform-independent logic, not
  touched here): `bionic` `the_query_form_of_sigaction_...`, `the_undeliverable_signal_family_...`,
  `the_process_symbols_answer_...`, `sigpipe_answers_sig_dfl_...`, `the_bound_count_is_exactly_...`,
  `the_final_split_of_the_reachable_set_...`, `every_bound_symbol_is_in_the_reachable_set_...`
  (`toupper`), `proc_self_statm_...` (a 64 MiB reservation moved VmSize by 0 pages), and the lib's
  `bionic::guestmem::tests::a_protection_with_no_expressible_state_...` (RWX must be refused).
* `omni-linux` `vdso`: the vDSO path taken (122 ns vs 205 for the system call) but not under half --
  a Windows-tuned margin, fixed on the branch (under three quarters).

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

The Linux personality (`omni-linux`, 2026-09-27): A1-A5 pass here in debug and release, the A4
and A5 gates 10/10 each in release. **A thread that takes guest faults is given a 64 KiB alternate signal stack**
(`fault::prepare_thread`, called by `omni-cpu` on entry to every run). The demand pager's
`SIGSEGV` path needs 12,496 bytes of it in a debug build against the 8,192 Rust gives a std
thread (release: 3,016; `omni-mem/tests/pager_linux`), and the overrun killed the debug suites
that serve demand faults (`omni-linux` A1-A5, `omni-mem`, `omni-cpu` `faults`/`hostile`,
`omni-android`) by `SIGSEGV`. `omni-platform/tests/fault_altstack_linux.rs` pins both sides: a
32 KiB-deep handler serves a fault on a prepared thread, and kills an unprepared one. Host code
that touches lazy guest memory from a thread of its own calls `DemandPager::prepare_thread` first.

## Open

* The GPU is the frame-rate limit. Options: a smaller window, the lowest quality (set), NVIDIA's
  390 legacy driver (a system change for the owner), no other GPU client on the desktop.
* After a live resize the engine's GLES renderer keeps its old viewport.
* Real-AOSP: PS99 not reached -- Roblox's join-game answers this account with a challenge (above).
  GL backend gaps: a multisampled window config is not offered; an EGLImage's content is re-uploaded
  only when the guest targets it again; no `EGL_ANDROID_native_fence_sync` (fences are glFinish).
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
