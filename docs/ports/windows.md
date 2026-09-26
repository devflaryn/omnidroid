# Windows

The reference host: x86-64, 24 hardware threads, 31.8 GB, RTX 4060 (8 GB). The D10/D12/D16
figures in `docs/DECISIONS.md` were measured here, and the other hosts are compared to it. The
cross-host state is in `docs/STATUS.md`; live-run history is in HANDOFF.

## What runs (measured in PS99, place 8737899170, `omnidroid play`)

| run | commit | result |
|---|---|---|
| w36, 30 min, default settings | `db56a5f` | 47.0 fps median, 2.55 GiB private, 2.06 cores; no guest thread lost, clean close, gate passed |
| w32, 30 min, 14 window switches | `1fe4307` | 45.2 fps, 2.57 GiB private / 2.17 GiB working set, flat |
| H2, the 30-35-instance setting (3 GiB device, `OMNI_FPS_CAP=10`, `OMNI_GRAPHICS_QUALITY=1`, `OMNI_AUDIO=off`, minimised) | `a1ef0c5` | 2.48 / 2.16 GiB, 0.63 cores |

* **fps stops near 60.** The engine's TaskScheduler paces at 1/60 s; the saved `FramerateCap`
  applies only under a server flag that is off. A phone on this APK and these flags is capped the
  same way, so ~120 fps needs Roblox's own flags (HANDOFF).
* **Sign-in survives a restart**: `jni::cookies` keeps the engine's cookies in the app's storage
  (`app_webview/omnidroid-cookies`), as the Java side's `CookieManager` does on a device.
* **A guest thread's death is recorded as `REASON_CRASH_NATIVE`** and the close watchdog halts the
  UI thread's call, so the next launch of that storage does not hang. Verified by the injected runs
  I1/I2 (`OMNI_INJECT_DEATH=<symbol>@<seconds>`), not by a mutation row.

## Build and run

* MSVC x64 toolchain, stable Rust, CMake; Ninja is used when present. dynarmic is built from the
  vendored tree (`crates/dynarmic-sys`), nothing is fetched.
* **Path length**: CMake nests objects deep and MSVC fails past 260 characters. From a long
  checkout path set `OMNIDROID_DYNARMIC_BUILD_DIR` to a short directory (this checkout uses
  `C:\od-unified`; other `C:\od*` directories belong to other worktrees).
* Commands are in HANDOFF, "Running". Storage: `%LOCALAPPDATA%\Omnidroid\data`, accounts beside
  it. Start a long run detached (`Start-Process`) so no tool timeout kills it.

## What differs on this host

| area | Windows |
|---|---|
| memory | placeholders (`VirtualAlloc2`, `MEM_RESERVE_PLACEHOLDER`); a real commit charge; guest faults through a vectored exception handler (2.05 us resolved, D10) |
| files | shortening a mapped file is refused by the host (`ERROR_USER_MAPPED_FILE`, 1224); `fs/windows.rs` keeps a logical end of file until the last section closes, because the engine truncates `memProfStorage<pid>.json` under a live mapping |
| CPU/JIT | dynarmic's x64 backend. One shared translation cache per process by default (patch 0022, D38; `OMNI_JIT_SHARED_CACHE=0` restores per-thread caches); its per-block tables as flat records (0024-0027), a full region retires the oldest only (0028), 256 MiB live by default. Patches 0017-0019: per-thread fixed cost on demand, RSB and fast dispatch keep the budget/halt checks, so `INTERRUPTIBLE` is `ALL_SAFE` (D32, D33, D35). Guest exclusives value-compare (D31) |
| window, input | Win32; host cursor hidden or held by the engine's own lock state (`jni::cursor`) |
| GPU | Vulkan on the real driver. The GLES fallback has no Windows host: `omni-gfx/src/gles.rs` refuses a Win32 window and names ANGLE (D3D11) as what would plug in; headless runs get a driverless EGL (`Gles::set_driverless`) |
| audio | WASAPI shared mode |
| web view | WebView2 |
| profiling | `omni_platform::sampler` (thread suspend and context read); `OMNI_MEM_REPORT` has host rows (`VirtualQuery`, working set) |

## Open

* ~0.6 GiB of display-sized blocks outside the guest space (2560x1440x8 and 1920x1080x8) at the
  landing screen (`05efab2`); probably the driver's, for the two displays. UNVERIFIED.
* ~8 allocations of ~31 MiB committed and never touched; owner unnamed. The engine's own heap is
  1.25-1.6 GiB of live data in a world; the memory plan is HANDOFF's "Multi-instance".

## Merge notes

Nothing pending: `perf-windows` is wholly in `unified`. Its shared-code additions others rely on:
`GuestCpu::jit_counters()` (a default method); `DynarmicOptions` has public fields
(`exclusive_monitor`, `optimizations_override`, the `shared_code_*` set), so build it with
`..Default::default()`; `jni::classes::Answer` gained cookie variants, so an exhaustive `match` needs them;
the bound/inline counts pinned in `omni-android/tests/bionic.rs` conflict textually whenever two
branches bind a symbol.
