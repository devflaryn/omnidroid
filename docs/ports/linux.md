# Omnidroid on Linux

Branch `port-linux`, from `base-0923-night` (`ce10eb8`). **One source** with Windows and macOS:
the Linux backends live in `omni-platform`'s `linux.rs` and `unix.rs` files and in new
Linux-only modules and test files; every edit outside them is listed under **Merge notes**.
Nothing in this document has been run on Linux ARM64, on macOS, or on any GPU but lavapipe.

**The host this was built and measured on** -- every figure below is from it:

| | |
|---|---|
| OS | Ubuntu 26.04 LTS, kernel 7.0.0-30-generic, glibc 2.43, `vm.overcommit_memory = 0` |
| CPU | Intel Core i5-4460 (Haswell, 4 cores / 4 threads, AVX2 + BMI2 = x86-64-v3, D2), Meltdown mitigation (PTI) active |
| RAM | 7.2 GiB, 4 GiB swap. The owner's GNOME session holds ~2.6 GB of it |
| GPU | NVIDIA Quadro 4000 (Fermi). **No Vulkan driver exists for it.** Vulkan is Mesa **lavapipe** (`llvmpipe (LLVM 21.1.8, 256 bits)`, Mesa 26.0.8, `VK_PHYSICAL_DEVICE_TYPE_CPU`) |
| Display | the owner's GNOME Wayland session (Mutter, Xwayland `:0`) is live but the owner was away; tests ran on **their own Xvfb servers** (`:87`-`:99`, some with xfwm4) unless a row says `:0` |
| Audio | PipeWire 1.6.2 (ALSA `default` -> PipeWire's plugin), HDA Intel PCH (ALC887-VD) |
| Network | no VPN. The ISP's resolver answers `195.175.254.2` (a block page) for Roblox's hosts |
| Toolchain | gcc 15.2, clang 21.1.8, CMake 4.2.3, Ninja 1.13.2, rustc 1.98.1 |

> **Lavapipe caveat.** Lavapipe renders on the CPU, on the same four cores the translated guest
> runs on. Every correctness result below is real; **no frame rate, frame time or present count
> measured here says anything about a real GPU**, and each one is marked. Nothing in the code
> selects, special-cases or names lavapipe: the renderer takes the best-ranked device the loader
> lists, and on this host that is the only one.

## From a fresh Ubuntu 26.04 to the gate

```sh
sudo apt-get update
sudo apt-get install -y git build-essential clang libclang-dev lld cmake ninja-build pkg-config \
  rustup libvulkan-dev mesa-vulkan-drivers vulkan-tools vulkan-validationlayers \
  xvfb x11-apps x11-utils xdotool imagemagick \
  libx11-dev libx11-xcb-dev libxi-dev libxcb1-dev libxcb-xinput-dev libxcb-xkb-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libxrandr-dev libxfixes-dev libxcursor-dev \
  libwayland-dev wayland-protocols libasound2-dev libpipewire-0.3-dev libspa-0.2-dev
rustup default stable
git clone <repo> omnidroid && cd omnidroid && git checkout port-linux
cp /path/to/Roblox-2.738.1397.apk .      # the APK the gate names, at the repository root
```

That is the complete list installed on the measured host, which had none of it. **What the build
and the tests actually use** of it:

| need | packages |
|---|---|
| build (Rust + dynarmic's CMake tree + the C++ shim) | `build-essential` (gcc/g++), `cmake`, `ninja-build`, `pkg-config`, `rustup` |
| run: Vulkan | `libvulkan1` (pulled in by `libvulkan-dev`) and an ICD -- here `mesa-vulkan-drivers` (lavapipe) |
| run: window | `libx11-6`, `libxi6` (XInput 2), `libxfixes3`, loaded with `dlopen` at the first window (`x11-dl`), so a machine without them still builds and runs everything that is not a window |
| build + run: audio | `libasound2-dev` to link, `libasound2` to run; PipeWire (or any ALSA `default`) |
| tests only | `xvfb`, `xdotool` (XTEST input), `imagemagick` (`import`, framebuffer capture), `x11-utils`, `xfwm4` for the window-manager tests (it was already present on the host; not in the list above), `vulkan-validationlayers` (the renderer enables it when present), util-linux `prlimit` |
| installed, not used | `clang`, `libclang-dev`, `lld`, `libxcb*-dev`, `libxkbcommon*`, `libxrandr-dev`, `libxcursor-dev`, `libwayland-dev`, `wayland-protocols`, `libpipewire-0.3-dev`, `libspa-0.2-dev` (the xcb, Wayland and native-PipeWire paths were not taken; see below) |

**Two host settings, both optional, both said by the gate if they matter:**

* **`RLIMIT_NICE` 40**, which is what Android's `init.rc` gives every app (`setrlimit nice 40 40`).
  FMOD starts each thread with `setpriority(PRIO_PROCESS, 0, -16)`; Ubuntu's default limit is 0,
  so the kernel answers `EACCES`, and the guest now gets exactly that (`-1`, `errno = EACCES`)
  instead of the thread dying. With the limit raised, the priority is really applied. Per user:
  `echo "$USER - nice -20" | sudo tee /etc/security/limits.d/omnidroid.conf` and log in again;
  for one shell: `sudo prlimit --pid $$ --nice=40:40`.
* **`TMPDIR` or `OMNI_DATA_DIR` on the checkout's filesystem.** Ubuntu's `/tmp` is a tmpfs; the
  gate hard-links the APK into the guest's root, which cannot cross filesystems, so on `/tmp` it
  copies the 160 MB instead (into RAM) and says so on stderr.

**The gate** (with no desktop session, start an X server first and say so):

```sh
Xvfb :99 -screen 0 1920x1080x24 &  export DISPLAY=:99      # or the desktop's own DISPLAY
OMNI_M6_ROWS_21_22=1 OMNI_GFX_WINDOW_TESTS=1 OMNI_KEYBOARD_MOUSE=1 \
  cargo test -p omni-android --release --test gameactivity -- --nocapture --test-threads=1 \
  initialize_native_code_returns_a_native_code_and_the_game_thread_starts
```

On a machine with 8 GB or less, run one build or test at a time: a release build of the
`omni-android` tests and a running guest do not fit together (this port ran every cargo command
through one `flock`).

## Status, measured

| feature | Linux backend | evidence (process exit codes; `--release`) |
|---|---|---|
| **virtual memory** | `vm/linux.rs`: `mmap(PROT_NONE)` reservations, `mprotect` / `MAP_FIXED` commit, decommit by re-mapping `PROT_NONE` (returns RSS *and* commit), `/proc/self/smaps` `VM_ACCOUNT` as commit charge, a ledger that refuses what Windows refuses | `vm_linux` 30/30, `vm_commit_charge_linux`, omni-mem `space/arena/commit_charge/pager/probe_linux`, omni-elf `loader_commit/cache_sharing/relro_linux`: exit 0 |
| **D12 JIT arena** | `memfd_create` + two `MAP_SHARED` views (`rw-s`, `r-xs`), never W+X | `vm_linux`; 117.5 ns per emit+execute vs 2,418 ns for `mprotect` flipping |
| **guest faults** | `fault/linux.rs`: `SIGSEGV`/`SIGBUS`, `SA_SIGINFO\|SA_ONSTACK`, access kind from `REG_ERR`, the Windows slot table and quiescence protocol; **first place re-asserted over dynarmic's own handler after every jit** | `fault_linux`, `fault_chain_linux`, `fault_teardown_race_linux`, omni-cpu `pager_precedence_linux` (3 JIT loads paged by us, 0 slow-path entries, D4 amendment 2 invariant armed) and `faults.rs` (no longer an early "SKIPPED" on Linux): exit 0 |
| **dynarmic** | `build.rs` hands CMake a C compiler on GNU toolchains | dynarmic-sys suite 46 tests incl. the 18-cell stoppability matrix: exit 0 |
| **files** | `pread`/`pwrite` (one call), `posix_fallocate`, `statvfs` (`f_frsize`) | `fs_linux` 10 -- the path-confinement rules with **real symlinks, run for the first time anywhere** (Windows cannot create them unprivileged): no escape |
| **process** | `getrandom` (looped), `sched_getcpu`, per-thread nice via `gettid`, `/sys/class/dmi/id/sys_vendor`, `CLOCK_PROCESS_CPUTIME_ID` | omni-platform lib 153: exit 0 |
| **network** | poll-based: `socket`/`accept4` (`SOCK_CLOEXEC`, blocking by default), non-blocking connect reporting as Winsock does, every socket option (Linux numbers), `poll(2)` with `POLLHUP`/`POLLERR`, `getifaddrs` | `net_loopback_linux` 35 (the Windows `net_loopback.rs`, mirrored): exit 0 |
| **clock** | shared `clock.rs` unchanged; its non-Windows "no tick to raise" is **measured** true | `clock_linux`: a 1 ms sleep is 1.082 ms median (n = 41) |
| **window** | `window/linux.rs`: X11 through Xlib (`x11-dl`, `dlopen`), so Xorg, Xvfb and **Xwayland -- i.e. Wayland desktops**; the same `WindowEvent`s as Windows | `window_linux` 12/12 with real XTEST input (keys incl. E0-extended, scancodes, text incl. dead keys, all 5 buttons, wheel, raw relative motion under capture, capture lost on focus loss, close, resize), `window_linux_wm` 6/6 (xfwm4), `window_linux_ewmh` 1/1 (a Mutter-modelled manager): exit 0 |
| **keyboard** | `scancode` = the set-1 code of the physical key (X keycode - 8 is the evdev code, then the inverse of `keys.rs`'s table), `keycode` = the level-0 keysym, XKB detectable auto-repeat, `Text` from the input method | omni-android `keys_linux`: every evdev code `keys.rs` maps round-trips through `keys::evdev_code` |
| **mouse** | buttons by role, wheel at 120 a notch, pointer capture = confined grab + blank cursor + XI 2.2 `XI_RawMotion` | `window_linux` |
| **Vulkan surfaces** | `RawWindow::Xlib` -> `VK_KHR_xlib_surface`; the guest's `vkCreateAndroidSurfaceKHR` is rewritten to `vkCreateXlibSurfaceKHR` | `renderer_linux` 1/1, `renderer_linux_wm` 1/1, `vulkan_present` 8/9, `vulkan_instance` 2/2, `ndk_host_window` 2/2 |
| **audio** | `audio/linux.rs`: ALSA `default` (-> PipeWire), `FLOAT` interleaved, 48 kHz stereo asked, **what was granted reported**; xruns and suspends recovered and counted | `audio_live_linux` 5/5, the unchanged Windows contract test `audio_live` 3/3, `--lib audio::` 17/17 live: exit 0 |
| web view (sign-in pages) | **not ported** -- `webview/unix.rs` stays structural (`Unsupported`, by name). WebKitGTK would be the Linux backend; the no-VPN run never opens a page | -- |
| native Wayland | **not written**; Wayland desktops are served through Xwayland | -- |

### The gate: Windows-without-VPN parity

The exact command above, on Xvfb `:99` with lavapipe (run2; run1 was the same with
`OMNI_DATA_DIR`), exits **101**, and it ends **where the Windows machine ends without its VPN**
(HANDOFF, "MEASURED 2026-09-23: without the VPN this machine's network cannot reach Roblox"):

* all 3,594 initializers, `JNI_OnLoad`, 20 of 20 scripted downcalls, every §8 row including 21
  and 24, the 24 GameActivity natives registered, a real 1280x720 window with Vulkan bound to the
  host driver, `APP_CMD_INIT_WINDOW`, `START`, `RESUME`, `GAINED_FOCUS`, the web-view protocol
  installed, and on close `LOST_FOCUS`, `PAUSE`, `TERM_WINDOW`, `STOP`;
* the settings fetch fails as the network makes it fail: `fetch flag exception: HttpError:
  TlsVerificationFail`, `getFlags: success = false` (the block page's certificate is not in the
  APK's own CA bundle), so the engine never asks for a renderer: `FRAMES: 0 presents`,
  `VULKAN: the engine resolved 0 entry point(s)`;
* **`M5 teardown: 0 guest thread(s) still running, failures []`** -- no guest thread was killed;
* the failure is the gate's close assertion, `SessionHistory Some("I")`, which the handoff
  records as the network's signature on Windows too.

So the engine's own Vulkan device, swapchain and frames were **not** reached here, as they are not
reached on Windows without the VPN; whether the engine would accept a CPU device (D8: "Device %s
is emulated, skipping") is therefore unmeasured. The swapchain and presented pixels are proven by
the tests below instead. The runtime does not work around a network's block (HANDOFF).

### Rendering, proven by pixels

* **`vulkan_present` -- translated ARM64 driving the host through `vkCreateXlibSurfaceKHR`**, the
  presented swapchain image read back: centre and corners `[51, 153, 204, 255]` for the clear, and
  the four texels `[32,96,160] [16,176,64] [200,48,16] [240,224,80]` at the triangle's quadrants.
  8/9 (Xvfb, with and without xfwm4); the ninth is `the_real_drivers_pipeline_cache_is_saved...`,
  because lavapipe's pipeline cache stays header-only when a pipeline is built through it -- a
  driver property.
* **`renderer_linux` -- the Xvfb framebuffer captured by a separate X client** (`import -window
  root`), after first showing the capture sees a known colour (VERIFICATION entry 19: a
  `xsetroot` root is 4096/4096 its colour): a 320x240 window cleared red, then green, is
  76,800/76,800 exact pixels each; a four-quadrant RGBA8 frame is 76,800/76,800 exact with exactly
  4 colours; the root beside the window is untouched.
* `renderer_live` (the Windows file): 8/9 on Xvfb+xfwm4 -- the ninth refuses a CPU device by
  design (D8). On the owner's real desktop (Xwayland `:0`, Mutter) it was 6/9 or 7/9 before this
  port's last fix (the minimise test waited out its 10 s); after it, four full-file runs gave 8/9
  twice and 7/9 twice -- the remaining desktop-only failure is below (open issues).

### The whole suite

`cargo test --workspace --release --no-fail-fast` with `RLIMIT_NICE` 40: **155 test binaries
passed, 2 failed** (exit 101), and both are fixed on this branch since: `bionic.rs`'s shared-mapping
test required a Windows-only `ftruncate` refusal (`ERROR_USER_MAPPED_FILE`, which Linux, like the
device, does not have) -- now 242/242; and `gameactivity.rs`'s headless run died hard-linking the
APK into `/tmp` (`EXDEV`) -- now copies, and ends as the network allows. The gated live tests are
listed in the table above. Without the raised nice limit `bionic.rs`'s
`setpriority_applies_the_nice_value_to_the_calling_host_thread` fails on this host, truthfully:
the host refuses the priority.

### Mutation rows

`tools/mutate_linux.py` runs `tools/mutate.py`'s harness, unchanged, over `tools/lnx_rows/*.py`
(all ids `lnx-`, checked against `mutate.py`'s table for collisions), so this branch never edits
`mutate.py`.

**The whole table, run on the merged tree** (`flock ~/odb/build.lock python3 -u
tools/mutate_linux.py`, with `RLIMIT_NICE` 40, Xvfb `:92` and `:93`+xfwm4 up; pre-flight 122/122
patterns and 25/25 commands passing on the clean tree; `git diff --exit-code` clean after):
**119/122 caught.** The three it missed were detectors, not defects, and each was made structural:
`lnx-proc-A5`/`-B1` -- the nice test derives its expectation from the process's own limit, so with
the limit raised it could not reach a refusal (a new test lowers its own limit to 0 in a child:
8/8 `lnx-proc` caught with the limit at 40); `lnx-win-A24` -- caught 3/3 alone and missed once in
the whole run, because the renderer test saw the defect only when its first frame beat the
`MapNotify` (a new window test minimises with nothing pumped in between: caught 2/2). FINAL_TABLE

| area | rows | tally (each area's own run) |
|---|---|---|
| `lnx-build` (dynarmic's build script) | 1 | 1/1 |
| `lnx-vm`, `lnx-fault` | 14 + 9 | 23/23 |
| `lnx-proc`, `lnx-fs`, `lnx-net`, `lnx-clock` | 8 + 9 + 22 + 2 | 41/41 (after one NOT CAUGHT, `lnx-net-A7`, exposed a kernel fact: the first `connect` after completion answers 0, only the next `EISCONN`) |
| `lnx-win`, `lnx-gfx` | 30 + 2 | 32/32 |
| `lnx-audio` | 18 | 18/18 (a 19th row, blocking-mode open, was NOT CAUGHT and removed: non-blocking mode could not be shown necessary) |
| `lnx-integ` (integration fixes: FMOD's nice, the relro verdict, Mutter's restore) | 7 | 7/7 |

## Numbers

### D10 and D12, re-measured on Linux (`docs/ports/linux-notes/mem.md` has every method and n)

| quantity | Linux (this host) | Windows (D10/D12) |
|---|---|---|
| reserve 1 GiB .. 64 TiB | ~2 us per call, **0 B** commit, 0 B resident (n = 31 each) | 0 B |
| largest single reservation | 96.44 TiB | 125.57 TB |
| 64 guest spaces x 16 GiB | 0 B | 240.8 MB |
| kernel soft fault | 1,437 ns/page (n = 11 x 16,384) | 398 ns |
| our SIGSEGV demand-pager fault, 4 KiB granule | 7,181 ns (n = 11 x 8,192) | 2,053 ns (VEH) |
| demand paging at the default 64 KiB granule | 1,761 ns per page touched | -- |
| commit ahead in 64 KiB granules | 180 ns/page | 150 ns/page |
| JIT emit+execute, dual-mapped memfd | **117.5 ns** (0 mismatches in 1,000,000) | 162 ns |
| JIT emit+execute, `mprotect` flipping | 2,418 ns | 2,259 ns |
| libroblox.so loaded, steady commit (eager / lazy `.bss`) | 16.332 / 5.293 MiB | 16.7 / ~5.4 MiB |
| grow to 3 GiB and release | +3,072.000 MiB, back to +0.000 (no page-table term) | +3,078.020 |
| per guest thread (dynarmic's cache + dispatch table) | 22.355 MiB (n = 8) | 24.5 MiB |

**`MAP_NORESERVE` was measured and not used.** It makes a 16 GiB `PROT_NONE` reservation no
cheaper (0 kB of `Committed_AS` either way: a `PROT_NONE` private mapping is not accountable), and
it switches commit accounting off entirely (a 64 MiB `mprotect` read-write charged 65,536 kB
without it and 0 with it). Reservation stays free, backing stays on demand, and commit is still
charged at commit, which is D10's asymmetry. **`MADV_DONTNEED` alone is Linux's `MEM_RESET`
trap**: it frees the pages and keeps the charge; decommit re-maps `PROT_NONE` over the range,
the only call measured to return both.

### Memory per instance, and how many fit (the owner's multi-instance requirement)

Instances of the gate binary started **one at a time, 90 s apart**, each with its own data
directory and window on Xvfb, sampled every 5 s from `/proc/<pid>/smaps_rollup`
(`RSS/PSS/Private_Dirty/Swap`, MiB; one run; the host started with 1.0 GiB of swap already in use
by the desktop and earlier builds):

| instances | per-instance RSS / PSS at +90 s | MemAvailable | swap in use |
|---|---|---|---|
| 1 | 693 / 687 | 4,577 MiB | 1,008 MiB |
| 2 | 719/679, 696/656 | 3,999 | 1,075 |
| 3 | 753/702, 718/667, 709/658 | 3,364 | 1,145 |
| 4 | 754/697, 735/678, 721/664, 694/637 | 2,933 | 1,356 |
| 5 | the kernel starts swapping the oldest instances' cold pages (194 and 331 MiB) | 2,909 | 1,957 |
| 6 | 241/179 ... 694/631 (1.1 GiB of the six swapped) | 2,888 | 2,573 |

Stopped at six: from five on, the host was relying on swap, which the requirement rules out.
**What was measured is an engine that never received its flags** (the network), so it is the
idle-startup shape, not a world: boot rises to ~690 MiB in ~15 s and stays there (no separate
boot peak), and the owner's 4 GB-boot / 800 MB-steady figures are about a loaded world.

Where one instance's ~690 MiB is (`/proc/<pid>/smaps` and glibc `malloc_stats`, n = 1):

* **~151 MiB is guest memory** (the 16 GiB space, 165 VMAs) -- the engine's own heap and data.
* **~60 MiB is `libroblox.so` text, file-backed and shared between instances**: a second process
  pays PSS for exactly half (53,316 of 106,632 kB), private 0, commit 0.
* **~104 MiB is the gate harness's own copy of `libroblox.so`** -- `main_lib_bytes()` in
  `tests/gameactivity.rs` reads the cache entry into a `Vec` that lives for the whole session
  (confirmed: the allocation starts `\x7fELF`). Mapping the cache entry instead would make those
  pages file-backed and shared like the text. Shared test code, Windows-owned: recorded here, not
  changed.
* **~360 MiB is other host heap**: glibc reports 463 MiB in use of 519 MiB held (incl. mmapped
  chunks); `malloc_trim(0)` on a live instance returned only 46 MiB, so it is live data, not
  allocator slack. One thread's arena alone holds 99 MiB live; dynarmic's per-thread 16 MiB
  fast-dispatch tables are fully resident; its 32 MiB code caches are 5-31 MiB touched.

Everything here is on demand (a reservation is 0 B until committed, decommit returns it, read-only
pages are shared), and the D10 machinery works as measured above; **what limits the instance
count on this host is ~540 MiB of host-side heap per instance**, in shared code: the harness's
library copy, the translator's per-thread state, and live host allocations not yet attributed.
Those are the levers, for whoever owns them.

### Audio (silence only; `docs/ports/linux-notes/audio.md`)

Granted 48,000 Hz x 2, period 480; the default device consumed 47,999-48,038 frames/s over 2 s
(5 runs, 0 xruns); server-less `plughw:` 47,999-48,002 (3 runs). A 100 ms wait takes
100.06-100.11 ms; xruns are recovered and counted (1 then 2, 5 runs).

### Other host facts that decided code

* `getrandom` never returned short through glibc's vDSO under a 50 us signal storm (0/200) but
  the raw syscall did (200/200): the loop is tested through the raw call.
* Linux clears `SO_ERROR` on read; a fresh TCP socket polls `POLLOUT|POLLHUP`; `SO_RCVBUF` reads
  back doubled; the first `connect` after completion answers 0, the next `EISCONN`.
* Mutter restores an iconified X11 client only through `_NET_ACTIVE_WINDOW`, never on a map.
* Another client's failed pointer grab lifts our confinement on this X server; a held capture is
  re-grabbed every poll.
* Xlib's input method zeroes the keycode of the key that completes a compose sequence; the raw key
  is read before the IM sees it.
* A minimised window's FIFO present blocked ~800 ms under Xwayland+lavapipe (n = 1).

## Merge notes

Every edit to a file this port does not own. All are additive; none changes what Windows builds
or does, except where a row says a Windows failure path became a success (none do).

| file | what | why |
|---|---|---|
| `crates/dynarmic-sys/build.rs` | `c_compiler_path()`: CMake's C compiler asked of `cc` on non-MSVC toolchains; MSVC keeps the C++ path | CMake refused `c++` as a C compiler |
| `crates/omni-platform/src/fault/mod.rs` | Linux backend selection (macOS keeps `unsupported`); `FaultError::Signal`, `FaultError::PrecedenceContested`; `pub fn reassert_precedence()` (a no-op `Ok` off Linux); scope doc | the backend, and D4's ordering over dynarmic's lazily installed handler |
| `crates/omni-mem/src/pager.rs` | `DemandPager::reassert_precedence()` pass-through; install error doc | omni-cpu reaches omni-platform through omni-mem |
| `crates/omni-cpu/src/dynarmic/mod.rs` | after every `od_jit_new` (when the backend owns paging): re-assert, refusing on error | dynarmic installs its SIGSEGV handler at the first jit |
| `crates/omni-mem/Cargo.toml`, `Cargo.lock` | Linux-only dev-dependency `libc` | `sigaltstack` in a stack-use measurement |
| `crates/omni-platform/Cargo.toml`, `Cargo.lock` | Linux target sections: `x11-dl = "2.21"` (dependency and dev-dependency); the lock gains `x11-dl`, `pkg-config` | the window backend |
| `crates/omni-platform/src/window/mod.rs` | `RawWindow::Xlib { display, window }`, `system_name` `"xlib"`; Linux-only `pub use scancode_from_evdev` | the handle, and the round-trip test |
| `crates/omni-platform/src/window/error.rs` | `WindowError::X11 { operation, api, detail }` | `LastError` is a `GetLastError` code |
| `crates/omni-platform/src/window/unix.rs` | `cfg_attr(target_os = "linux", allow(dead_code))` | only macOS uses the structural body now |
| `crates/omni-platform/src/audio/mod.rs` | `unix` for non-Linux unix, `linux` for Linux; Linux-only `Recoveries` and `AudioOutput::recoveries()` | the backend; xrun counts WASAPI has no equivalent of |
| `crates/omni-platform/src/audio/error.rs` | `AudioError::Alsa { operation, api, errno, description }` | `Os` prints an `HRESULT` |
| `crates/omni-platform/src/process/error.rs` | `ProcessError::Errno { operation, api, errno }`; `is_permission_denied()` (never true of a Windows variant) | POSIX errno is a third number space; the `setpriority` fix |
| `crates/omni-android/src/bionic/procenv.rs` | `setpriority`: a host permission refusal is the guest's `-1`/`EACCES`, not a refusal | FMOD's threads would die on every Linux host without `RLIMIT_NICE` |
| `crates/omni-gfx/src/{vulkan,host,claim,error}.rs` | Xlib arms beside every Win32 arm (instance extension, surface, window key, the guest's surface-call rewrite), `zero_size_is_the_windows` (false for Win32), refusal texts naming xlib | the surface |
| `crates/omni-platform/src/process/mod.rs`, `fs/mod.rs` (tests only) | "only Windows has a backend" asserts and two ignores widened to Windows or Linux; socket tests compiled on Linux | they asserted Linux had none |
| `crates/omni-platform/tests/vm_seam.rs`, `window_seam.rs`, `crates/omni-mem/tests/config.rs` | the "Linux is structural" tests narrowed to `not(any(windows, linux))` | Linux is no longer structural |
| `crates/omni-elf/tests/loader_m1.rs` | on unix the relro child's verdict is `SIGSEGV`, not `0xC0000005`; Windows arm unchanged | a unix child killed by a signal has no exit code |
| `crates/omni-android/tests/bionic.rs` | the mapped-file `ftruncate` refusal (`1224`) under `cfg(windows)`; on unix the call is answered 0 | a Windows host limit Linux does not have |
| `crates/omni-android/tests/gameactivity.rs` | on unix a cross-filesystem APK link falls back to a copy, announced | Ubuntu's `/tmp` is tmpfs (`EXDEV`) |

**Left for the merge, deliberately not edited** (docs and prints that are now stale on Linux, and
tests that assert Windows literals): `omni-platform/src/lib.rs`, `vm/error.rs`, `process/mod.rs`,
`fs/mod.rs`, `net/mod.rs`, `window/mod.rs` and `omni-android/src/ndk/host_window.rs` still describe
the Linux backends as structural; `omni-mem/tests/windows_only.rs` and `omni-elf/tests/windows_only.rs`
print that the gated suites did not run on Linux (their Linux mirrors live in `tests/*_linux/`
directories precisely so those lists stay true); `OsError::name()` uses Windows' table for every
number, so on Linux e.g. errno 5 prints as `ERROR_ACCESS_DENIED` (a unix table was written and
backed out because `vm_seam.rs` asserts the Windows names on every target);
`window_live.rs::the_raw_handle_is_a_live_win32_handle` and `vulkan_device.rs`'s
`SurfaceCall { system: "win32" }` assert Windows literals (the Linux log shows the correct
`VK_KHR_android_surface -> VK_KHR_xlib_surface` pairing). The mutation rows are in their own files
(`tools/mutate_linux.py`, `tools/lnx_rows/`) and concatenate with `mutate.py`'s without a collision.

## Open issues

1. **The engine's own Vulkan path is unreached** here (no flags without the network), so D8's
   "emulated device" refusal of lavapipe is unmeasured. On a real GPU, or with the network, that
   is the first thing to look at.
2. **Host heap per instance (~540 MiB)** limits the instance count, not the memory seam (above).
3. **On the owner's desktop (Mutter)** `renderer_live`'s `a_renderer_can_be_torn_down_and_rebuilt`
   fails in 2 of 4 full-file runs and 3/3 when run right after the minimise test, and passes
   3/3 alone: round 0 builds a second swapchain (an out-of-date recreate following the preceding
   test's minimise). A probe of the same sequence in one process did not reproduce it. It passes on Xvfb and
   Xvfb+xfwm4. Cause not established.
4. **Signal safety.** The demand pager takes locks and allocates inside a `SIGSEGV` handler. It is
   sound for the synchronous faults it serves under its documented invariant (the same bargain the
   Windows VEH makes), not async-signal-safe in the POSIX sense; a panic caught inside it takes
   stderr's lock.
5. **`vm.overcommit_memory = 2`** and a host with the kernel's default `vm.max_map_count`
   (65,530; this one has 1,048,576) are argued from kernel source, not measured.
6. **Not ported**: the web view (sign-in pages), native Wayland, Linux ARM64 (native execution per
   ARCHITECTURE §6), the PipeWire native API (ALSA's `default` reaches it).
7. `allocate` (`posix_fallocate`) allocates from 0 because the shared signature passes only the
   end; the path seam's check-then-use symlink race (documented in `path.rs`) could be closed on
   Linux with `openat` + `O_NOFOLLOW`, a shared-code change.

## Where the details are

`docs/ports/linux-notes/mem.md` (memory and faults), `posix.md` (process, files, network, clock),
`window.md` (window and graphics), `audio.md` -- each with its methods, sample sizes, test
commands and rows. The rows: `tools/lnx_rows/*.py`.
