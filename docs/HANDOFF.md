# Handoff

## PS99 AT 60 FPS, HALF THE CPU, LIGHTER (2026-10-08/09, ~20 h, Windows; branch `perf/ps99-60fps` @ the docs commit after 212550c, not merged, not pushed)

Goal (owner): PS99 fully loaded toward 60 fps, CPU and RAM well below, no feature removed. Morning
report `docs/MORNING-2026-10-09-ps99-60fps.md` (with the two checkpoints); every A/B (winners and
losers, sessions s1-s23) in `docs/NIGHT-2026-10-09-ps99-60fps.md`. Same harness, `main` 6b1ab88 vs
the branch, fresh boots: **fps 35.0 -> 59.8 (+24.8), CPU a frame 169 -> ~42 ms (-127 ms, -75%;
5.9 -> ~2.5 cores), private working set 3.865 -> ~3.35 GB (-515 MB) before the day's last defaults
and ~3.12 GB with them (s23, one clean pair: -296 MB more), threads ~1,220 -> ~800; "Joining game"
~183 -> 85-100 s on a new device.**

**State at the end (2026-10-09 ~17:30):**
- New defaults of the day, each with its way back: the game's code aging (`OMNI_CODE_AGE_GAME`, 3 min,
  one pass, later only past keep + 48 MiB, 1 s between regions; `=0` off), the JIT tables shrinking
  after evictions (dynarmic 0073, `OMNI_JIT_TABLE_SHRINK=0` off), property areas kept without their
  zero tails (~-77 MB in the system host; `OMNI_PROP_FULL_COPY=1` the old copy), SurfaceFlinger's code
  aging, a saved device reading its launcher (`OMNI_R_LAUNCHER_SAVED=0` off), a degraded memory path
  in a slice that only ran out of budget logged once and run on instead of killing the process.
- **Translation snapshots** (`OMNI_JIT_SNAPSHOT=<dir>`, dynarmic 0070/0071/0074/0075; opt-in): -10..-11 s
  to the world (onGameLoaded ~100 -> ~89 s, boot_completed -4 s, the game launched -11 s), fps the same,
  but +80..+250 MB: the game and system_server verify only 2-11% of what they restore (their
  libraries land elsewhere each boot; the perf harness also reinstalls the APK into a new random
  path every boot -- a saved device would not). `OMNI_JIT_SNAPSHOT_LAZY=1` reads restored code in as
  entered (works: the game read ~3-4k pages). **Next:** branch `perf/jit-snapshot-fixes` @f5003bf has
  the unbuilt WIP for per-library placement (`OMNI_JIT_SNAPSHOT_LIB_ZONE=1`), forgetting unentered
  restored blocks (0076, `OMNI_JIT_SNAPSHOT_FORGET=1`) and `OMNI_JIT_SNAPSHOT_WHY=1` (which says where
  never-entered blocks went) -- build, test, A/B on a saved device (`OMNI_R_GOLDEN`).
- **Fixed today, worth knowing:** region files `omni-shm-<pid>-<n>` of killed runs under a reused
  host pid made gralloc refuse SurfaceFlinger's first buffer and the boot loop (20,372 stale files, 7
  GB, in %TEMP%); `perf_ab.ps1` no longer waits out a failed launch and then kills later runs' guests.
- **Open:** the s23 defaults A/B was noisy (one boot lost to the degraded-path abort, now non-fatal;
  the later pair in a slow world) -- rerun it with more pairs; the game's one aging pass costs ~15 s at
  47-57 fps once a session; ~55 MiB of unattributed resident zero pages in the system host's heap
  (`OMNI_ALLOC_TRACE_KB=256` + `tools/zeroscan.ps1`); the Mac arm64 build unverified.

- **Measure first, with the tools made for it:** `tools/perf_live.ps1` (levers A/B'd live in one
  session via `OMNI_LEVER_FILE`; every lever is in `crates/omni-linux/src/lever.rs`),
  `tools/perf_ab.ps1` + `perf_ab_seq.ps1` (fresh-boot arms; records private working set and
  threads; env `OMNI_AB_APK/SETTLE/WINDOW/JOIN_MIN`), `OMNI_PROC_CPU=<s>` (whole-process CPU by
  thread name -- it found the window thread spinning), `OMNI_THREAD_CPU` + `OMNI_GUEST_PROF=1`
  (guest functions of hot threads), `[thread-sys]`, `OMNI_MEM_TRACE`, `OMNI_MMAP_LOG_MB`.
  At 60 fps an A/B is decided by CPU ms/frame, not fps. PS99 migrates servers every 15-50 min
  ("reason: 285"): drop those phases.
- **What it was:** herds and spins more than codegen -- futex `notify_all` per process (31 -> 44.5
  fps), poll waits on "anything" and 50 ms slices (system host 54 -> 28 ms/frame), the display
  window's pump spinning a core, EPOLLET ignored (a DoH resolver spinning a core in some boots); then
  the JIT (dynarmic 0037 precise GetSetElimination, 0040/0041 TBI unmasked with learning, 0042 inline
  dispatch, 0039 FP in XMM, 0061 compact code, 0063 a 1.73x faster emitter); binder host pool,
  Vulkan batching, `remote_direct`, zero-copy GPU present; RAM: zero sweep, code aging, JIT tables
  compaction (0051/0052), kernel binder spawn rule, uncompressed boot image (`-bootu` saved devices).
- **Bugs fixed on the way:** a fork time-sharing memory hand-over during a side's own fork (the
  setup's `sh` crash; Clash of Clans-style forks), two JIT bugs (an unused faulting load dropped; NZCV
  forwarding), present_zero switching itself off after resizes, a fork restore racing remote writes,
  a lost binder spawn request, ppoll signal latency, Vulkan batch leaks.
- **Open:** the Mac arm64 build is unverified (offline all night); resident zero pages in the host
  heaps (~100-200 MiB); `OMNI_R_FAST_SETUP` and `OMNI_DEVICE_IDLE_APPS=out` are opt-in.

## APKS IN PARALLEL, AND THE FORK A WATCHDOG NEEDS (2026-10-04, Windows; branch `feat/fork-timeshare`)

Goal (owner): launch different APKs in parallel on one Android, each in a window of its own; the
same package at another version gets an Android of its own. Clash of Clans first, then all three of
`Desktop/test-apks` together.

**Clash of Clans: three blockers removed, one left, and it is the app's, not ours.**

- It deadlocked in `clone`. `fork` was a `vfork` -- the parent frozen until the child execs or exits
  -- and `libsupercell_clashofclans.so` forks an anti-tamper watchdog that never execs and blocks
  reading a pipe its frozen parent must write. ANR at 60 s, killed. **Fixed**: the memory is
  time-shared, each side keeping its own view (`ce6a158`, spec
  `docs/superpowers/specs/2026-10-04-fork-timeshare-design.md`, gate
  `fork_exec::a_forked_child_that_waits_on_its_parent_lives_beside_it`). The fork-exec path every
  boot takes is untouched: a pair is made live only if the child is still there 500 ms on.
- Then `ptrace(PTRACE_ATTACH)` was `ENOSYS`: the parent does `prctl(PR_SET_PTRACER, child)` and the
  child attaches so the one tracer slot is taken. **Added** (`crates/omni-linux/src/ptrace.rs`):
  attach bookkeeping per thread, a tracer waiting for and signalling a tracee that is not its child,
  and a tracer reading its tracee's `/proc/<pid>/task`. Registers, memory and real stops stay
  refused by name.
- The watchdog's protocol now runs end to end -- attach, wait, set options, continue for all
  thirteen threads, then its report to the parent -- and the child's whole divergence is 24 pages.
- **WHAT IS PROVEN (2026-10-04).** `cnsbodqak.az` is thrown by `cnsbodqak.g.a(String)`, which is a
  message deserializer, not a check: it reads the first hex byte as a type tag and dispatches, and a
  string that **starts with `!` is the error marker** -- `g.a` wraps it in `az` and throws
  (decompiled with `tools/dexdis.py`). So `cnsbodqak.az: !<base64>` is the app's native->Java
  protocol surfacing an **error-tagged, encrypted message** that something upstream produced; the
  exception is the wrapper, not the gate. The `!` payload appears only once the native watchdog runs
  (with `OMNI_FORK_NOCHILD` it was a bare `!`), so the native protection layer produces the verdict,
  after its anti-debug (fork + ptrace of every thread) passes. **The verdict is encrypted and cannot
  be read without the key; which specific condition set it is not determinable from outside without
  decrypting it or finishing the native RE.** What is also true, but not proven to be the trigger: The failure is a Java exception (`cnsbodqak.az`, thrown from Thread-1
  `cnsbodqak.V.run`): `cnsbodqak` is a dedicated obfuscated Java anti-tamper package. The native
  anti-debug it runs first (the fork watchdog, ptrace of every thread) now completes; the **Java**
  layer then rejects the environment. The APK carries Play Integrity (`StandardIntegrity` x22,
  `IntegrityTokenRequest` x11, `IntegrityManager` x12), Play Licensing / LVL (`ILicensingService`,
  the `CHECK_LICENSE` manifest permission), and install-source checks (`getInstallerPackageName`,
  `InstallSourceInfo`). The image is plain AOSP: **no GMS, no Play Store, no licensing service**
  (checked `sysroot/aosp-35`), and the APK was `pm install`ed so its installer is null, not
  `com.android.vending`. Play Integrity and LVL **cannot be satisfied off the Play Store on any
  device** without GMS and a genuine Google attestation -- the same sideloaded universal APK fails
  the same way on a bare AOSP build or any GMS-less emulator. Talking Tom and Roblox run because
  they do not hard-gate on Play Integrity at startup. **To run this APK, the image needs a GMS +
  Play attestation path (microG or GMS integration -- a separate project), not an omnidroid fix.**
  The hypotheses below were chased before this was found; they are kept so they are not re-tried,
  but the root cause above supersedes them.

  Confirmed three ways: (a) the APK contains the Play Integrity / LVL / install-source APIs above;
  (b) the image has no GMS, Play Store or licensing service; (c) you cannot even make the install
  look Play-sourced -- `pm install -i com.android.vending` leaves the installer null and `pm
  set-installer ... com.android.vending` throws, because no `com.android.vending` package exists to
  attribute the install to. The realistic path to run it is **microG** (open-source GMS with a Play
  Integrity / DroidGuard provider and signature spoofing) integrated into the image -- a separate
  project, scoped as its own task. Clash of Clans is known to run on microG/GMS setups and not on
  bare AOSP, which matches exactly.

- **Left**: after that the app raises `cnsbodqak.az` with an **encrypted** verdict, from a thread of
  its own (`cnsbodqak.V.run`), and `exit_group(1)`. Its packer is a commercial one -- its state lives
  under hex-encoded names, `/data/user/0/com.supercell.clashofclans/736869656c64/75706431/...`
  ("shield"/"upd1") -- so the verdict cannot be read, only the inputs it gathers.

  **Ruled out, each tried and each leaving the verdict unchanged** (do not re-try these):

  | tried | result |
  |---|---|
  | `--module emu-hide --denylist com.supercell.clashofclans` (really applied: `[r] rooted device staged (root profile 282fac68)`) | same verdict |
  | `TracerPid` reporting the real tracer, `PPid` the real parent (`d36ae3c`) | same verdict; the app never reads `/proc/self/status` in the window |
  | `/proc/sys/kernel/yama/ptrace_scope` = 1 (`d36ae3c`); it *does* read this, right after `prctl(PR_SET_PTRACER)` | opens now (was ENOENT), same verdict |
  | `uname` no longer reporting release `6.1.99-omnidroid` (`fea3867`); it calls `uname` exactly once in the window | same verdict |

  **The APK and our reading of it are both sound, so neither is the cause.** In the window it walks
  its own package: 5464 reads of exactly 46 bytes (a zip central-directory header) over `base.apk`,
  for an archive of 5411 entries. Our reads serve it correctly -- 10817 full reads, one short, and
  that one is the last, at EOF, which is what a regular file does. The file is the real one: its
  APK Signing Block carries v2 and v3 schemes and the certificate reads `Supercell`, `Helsinki`, so
  it is Supercell's own signature and not a repackager's (worth checking first for any such app --
  a merged universal APK from a mirror would be resigned and would fail its own check on a real
  phone too).

  **Exact failure identifiers extracted (2026-10-04).** The pointer-dump exit diagnostic (`ccbd605`)
  read the report-detail buffer the native side passes its reporter: **reason 5, subcode 1, check-id
  `0xc9162e24`**. The full backtrace shows the native check is **invoked from Java over JNI**
  (`boot.oat` -> `libopenjdkjvm` -> `libart` -> `libsupercell+0x60dc9c`), so a Java method of the
  RASP calls the native check, which runs and exits with these codes. This is the maximum observable:
  the identifiers are numeric (no readable strings), and their meaning is defined inside the
  obfuscated RASP. Decoding "reason 5 / sub 1 / id 0xc9162e24" is what reversing the VM would yield.

  **The decrypted library is rebuildable and lab-loadable (for future RE).** `OMNI_DUMP_AT_EXIT`'s
  first-LOAD dump, overlaid on the original `.so`'s layout (`orig[0:first_LOAD_filesz] = dump`),
  makes a complete ELF whose `.text` is 100% valid arm64, and `lab_load` accepts it. But calling the
  reason-5 check functions in the lab faults dereferencing their context argument (e.g. a read at
  `+0x130` of a null arg): the checks need the RASP's runtime context object, built by its init, so
  isolating one means reconstructing that state -- the same reversing effort as the static route.
  Dynamic probing in the lab is therefore blocked the same way; the path forward is reconstructing
  the context or reversing the VM interpreter, both specialist multi-session work.

  **Pinned to a commercial RASP verdict, reason code 5 (2026-10-04).** With the exit backtrace
  diagnostic (`7b3a3cb`: `[exit]` now walks the frame chain and logs x19..x23), the native side
  exits(1) through `libsupercell+0x605210`, and `x19 = 0x5` is the **reason code** passed to its
  report function -- the decision is in the `libsupercell+0x60dc9c` region. Disassembling that in the
  decrypted dump shows **control-flow-flattened VM obfuscation**: a dispatch loop on a state register
  (`w9`) compared against computed constants, calling through `blr` on runtime-resolved pointers.
  That is commercial RASP (the "shield"), built to resist static RE -- decoding what reason 5 checks
  means reversing the obfuscation interpreter, a major specialised effort. So the blocker is not a
  missing omnidroid feature with a cheap fix; it is a protected check whose condition is hidden
  behind a VM. The handshake/anti-debug all pass; reason 5 is a later, virtualised check.

  **The two leading omnidroid-specific suspects for *why* native writes `!`** (unconfirmed, because
  the verdict is encrypted -- but these are where the next effort should look, and both are
  semantic gaps a memory-and-debugger integrity watchdog is built to catch):
  1. **The time-shared fork serializes parent and child.** A real `fork` yields two processes that
     run concurrently; `crate::fork` gives them one memory they take turns in (only one resident at
     a time). A watchdog that relies on genuine parent/child concurrency -- both live at once,
     checking each other -- sees turn-taking instead. The verdict payload is new behaviour that only
     appears once this fork runs (on `main` it deadlocked before reaching it), so the watchdog's
     result is produced *by* this fork.
  2. ~~ptrace is bookkeeping, not real debugging~~ -- **RULED OUT by trace data.** The watchdog
     makes only `PTRACE_ATTACH` (0x10), `SETOPTIONS` (0x4200) and `CONT` (0x7), 13 of each (one per
     thread), and **no** `GETREGS`/`PEEKTEXT`/`GETREGSET`. Everything it calls is satisfied, so the
     fake-ptrace semantics are not what it detects. The anti-debug (take the one tracer slot) passes;
     the verdict comes from a *different* native check -- a candidate is the shield's provisioning
     data file (`/data/user/0/com.supercell.clashofclans/<hex 'shield'>/<hex 'upd1'>/*.dat`), absent
     on a fresh install and perhaps needing a network round-trip the device cannot make the way the
     shield expects (Roblox's sockets work, but `ConnectivityManager` reports `Active default
     network: none` -- no NetworkAgent is registered, a real omnidroid gap for any app that gates on
     `getActiveNetwork()`).
  Confirming either needs the verdict decrypted or the native check reversed; fixing #1 is real
  concurrent fork (a host process per child -- CRIU/checkpoint territory) and #2 is a real ptrace
  stop/step/peek subsystem. Both are large, separate from the launcher work.

  **The verdict mechanism, fully mapped (decompiled).** Native code sets up a pipe and stores its
  read fd in the Java static `cnsbodqak.g.b:I` (no Java writes it -- JNI sets it). The native
  watchdog runs its checks and writes a verdict string to the pipe, terminated by `?`. Java's
  `cnsbodqak.V.run` polls up to 10 s (`g.a()` = the fd, `g.d()`->`g.b()` reads until `?`), then
  `g.a(String)` parses it: a leading `!` is the error marker -> wrap in `az` and throw -> FATAL.
  So the whole verdict is produced by native code and is encrypted; nothing in the Java layer
  decides, and nothing omnidroid serves is read wrongly -- the pipe read returns exactly what the
  native side wrote. Reading *why* it wrote `!` needs the native RE / the key; it is not reachable
  from the Java side or from any host-served value.

  What it is seen to gather after the handshake: `/proc/self/cmdline`, its own `lib/arm64` directory,
  its `base.apk`, `/proc/<pid>/task`, the shield `.dat`, then ~4.7 MB of `mprotect` RWX->RX (its
  unpacker) and a page-at-a-time `mprotect` loop. The decision itself makes no system call, so a
  syscall trace cannot see it: the next step is the lab debugger (`omnidroid-frida`) on the
  *unpacked* `libsupercell_clashofclans.so`, dumped after it decrypts itself -- the static file is
  packed and disassembles to nothing.

  **It says nothing.** Its whole log for a run is the fatal report and the signal that follows --
  no Titan or Supercell lines, no diagnostics. Both evidence channels are therefore exhausted: the
  decision makes no system call, and the app writes no log. What is left is the binary.

  **A false lead, recorded so it is not followed again:** `nc` from `shell` cannot reach anything,
  loopback included, and `dumpsys connectivity` says `Active default network: none`. Neither is the
  app's situation. The shell runs in the **system's** host process, where a connect through
  `libnetd_client` reaches netd's fwmarkd and fails for want of SCM_RIGHTS -- a known gap, recorded
  in "Network (2026-09-28)" below; apps are unaffected, which is why Roblox signs in and joins.
  Worth fixing on its own, separately from this app: **ConnectivityService has no network**, so
  `ConnectivityManager` tells every app there is no internet (there is no ethernet service in the
  image and wifi has no HAL -- `cmd wifi set-wifi-enabled enabled` does nothing). Sockets work
  regardless, so it has not blocked anything yet, but any app that gates on `getActiveNetwork()`
  before it starts would see an offline device.

  **The unpacked library is now obtainable** (`a793ea2`). `OMNI_DUMP_AT_EXIT=libsupercell` with
  `OMNI_DUMP_DIR=<dir>` writes the library as the live process holds it when it exits -- 6.6 MB that
  disassemble as valid arm64 everywhere, where the on-disk file is packed (0% valid). The strings
  stay encrypted (only the `uname` import name is plain), so the detection is in code, not a string
  scan. The exit comes from a report-and-exit stub at `libsupercell+0x5f24f4`/`+0x5f2500` (both
  `mov w0,#1; bl exit_group`); the decision is upstream of it. Reversing 6.6 MB of obfuscated arm64
  from there to the check is the remaining work, and it is ordinary static RE now that the image is
  in hand. Load the dumped `.bin` in the lab (`lab_load`) or any disassembler.

  **Tracing it is observer-sensitive, so trace sparingly.** `OMNI_TRACE_PATHS_ONLY=1` exists for
  this (`08f6a1d`) and is still not free: with it the app took 81 s to its first screen instead of
  12, and in one run looped on `statfs` without ever reaching its fork. Scope it with
  `OMNI_TRACE_APP=com.supercell.clashofclans` **in the device's own environment** (a CLI
  `omnidroid aosp --warm`, not `<device>.appenv`, which would trace every app host) together with
  `OMNI_APP_SPARE=0` -- otherwise the app starts in the spare process, whose name is `omni-spare`,
  and the trace never engages. A device-wide path trace wrote 568 MB in two minutes and is not worth
  it.

  Read any of this with `OMNI_FORK_TRACE_CALLS=1` in `<device>.appenv`, which traces both sides'
  calls from the handover only -- the window that matters, without tracing the whole start of the app.

**Parallel APKs: the Android half is already there; what is left is the composer.**

Two different packages, one warm device, no engine change (spike recorded in the multi-instance
spec):

```
settings put global overlay_display_devices "1280x720/213"     -> displayId 2
am start -n com.roblox.client/.startup.ActivitySplash          -> Display #0 resumed
am start --display 2 -n com.outfit7.talkingtomcamp/.MainActivity -> Display #2 resumed
```

Both app host processes live, each resumed on a display of its own, minutes later still. **A second
display, not a second user, is what makes two apps run side by side**; users remain the answer for
isolation, and two versions of one package still need a second system (PackageManager keeps one code
path per package name).

**Built since (1 and 2 of the list below are done):**

- `hal/composer.rs` serves a **map of displays** (`b0d1d5b`): a `Screen` is a framebuffer, a mode and
  that display's own layer state; the client keeps only its callback and the layer-id counter.
  Behaviour for behaviour what it was with one display.
- `Composer::add_display(w, h)` makes and hotplugs a display, and `OMNI_DISPLAYS=<n>` asks for that
  many at boot, each with **a window of its own that opens on that display's first frame**
  (`925ad99`, `b62fb39`). Verified on a warm device: Talking Tom on display 0 and Roblox on display
  2, both resumed, two windows presenting their own frames (716 and 520), one Android.

**Two physical displays is SurfaceFlinger's ceiling, not ours**: `E/HWComposer: Ignoring connection
of tertiary display 2`. A third app side by side therefore needs a **virtual** display
(SurfaceFlinger makes many), presented into a framebuffer and window of its own.

**The launch rule is in and run** (`6e03fe0`, `3c41b7a`): `start_instance` *places* the app -- a
different package goes on the live device's next free display, the same package at other bytes gets
an Android of its own, and a device with every display taken sends it on. `fits` is the rule without
a device and `server::tests::where_an_app_goes` is its gate; `find_all`, `boot_another`,
`install_beside`, `start_on` and `Device::display_ids` are underneath it.

**Both branches, through the real server** (`python tools/mcp_demo.py start:<apk> start:<apk>`,
which drives `target/release/omni-mcp.exe` over stdio -- the way to test it when the editor's own
MCP server is an older binary):

```
com.outfit7.talkingtomcamp  display=0  device=omni-warm-1791129845  booted  app_on_screen
com.roblox.client           display=2  device=omni-warm-1791129845  warm    app_on_screen

com.roblox.client  sha=86fca8a47a  display=0  device=omni-warm-1791130032  booted  app_on_screen
com.roblox.client  sha=4bcb90eee4  display=0  device=omni-warm-1791130104  booted  app_on_screen
```

Two packages share one Android on displays 0 and 2; two builds of one package get an Android each,
both alive at once (18 host processes, 7.5 GB of 31.8 GB free). `3c41b7a` is what made the first
work: `OMNI_DISPLAYS` had been set on the Roblox session's command instead of the warm boot, so a
warm device still had one display and the second app was sent to a second Android.

**Three apps in parallel, each in a window of its own** (2026-10-04), one `mcp_demo` run of
`start:<talkingtom> start:<roblox> start:<opentyrian>`:

```
com.outfit7.talkingtomcamp   display=0  android=omni-warm-1791130359  app_on_screen
com.roblox.client            display=2  android=omni-warm-1791130359  app_on_screen
com.googlecode.opentyrian    display=0  android=omni-warm-1791130442  app_on_screen

omni-warm-1791130359: Display #0 topResumed com.outfit7.talkingtomcamp/.MainActivity
                      Display #2 topResumed com.roblox.client/.ActivityNativeMain
omni-warm-1791130442: Display #0 topResumed com.googlecode.opentyrian/.MainActivity
```

Three windows presenting their own frames (1249 and 1126 on the first Android, 926 on the second);
Talking Tom at 76% of its load, OpenTyrian at its menu, Roblox resumed on display 2. **Two Androids,
not one, because SurfaceFlinger takes one external display beside the primary** -- the rule put the
third app on a second Android by itself, which is what "no display free" is for.

Next, in order:

1. **A virtual display path**, for a third app *on one Android* (`createVirtualDisplay`, its output
   into a `Framebuffer`), since physical displays stop at two per Android.
2. **An instance's display does not survive the server**: instances are the server's, so after a
   restart an app already installed is started without `--display` and lands on display 0. The
   device could be asked instead (`dumpsys activity activities`, resumed per display).
3. **Input association per display** and per-display focus: a second window's keyboard and mouse
   would reach display 0 today, so its window is opened with `input: false`.
4. A screenshot path for a display other than the first (`screenshot` and `OMNI_SCREENSHOT` are
   display 0's), then phase 3's isolation gates.

## `--cookie --place` ON THE WARM DEVICE (2026-10-02, Windows; branch `perf/warm-join`, not merged)

Goal (owner): warm Android up, no APK installed, from the `--cookie --place` command to PS99's own
(in-game) loading screen. Report: `docs/MORNING-2026-10-02-warm-join.md`.

- `omnidroid aosp --cookie --place` uses the warm device when one is up (`crates/omnidroid/src/warm.rs`,
  crate `omni-warm`): install + cookie store planted before the first start + start in one command,
  the place's link at the main Activity, `warm-release` stops the app if the launcher is killed.
  A/B 4 pairs: **92.1 -> 79.2 s** median to PS99's screen, "Joining game" 84.2 -> 43.8 s.
- `omni-linux` owners table: a rename was a full scan (~20 ms each, Roblox's cache does hundreds)
  and the table leaked every uninstalled app's files -- fixed (1.2 ms a rename, no growth).
- Open: the game load after "Joining game" (10-50 s warm, 7-17 s on a saved device -- why the saved
  device is faster there is not found yet); app start is translation-bound; MCP `start_instance` with a
  cookie still boots its own device.
- Measure: `tools/join_timer.py`, `tools/warm_join_bench.ps1`, `tools/warm_join_ab.ps1`.

## ANY APK IN SECONDS: THE WARM DEVICE (2026-10-01 night, Windows + Linux; branch `perf/fast-boot`, not merged)

Goal (owner): an agent behind the MCP server builds APKs and tests them one after another, seconds
from its call to the APK's first screen. Report with every figure and how to reproduce:
`docs/MORNING-2026-10-01-fast-boot.md`.

| through the real `omni-mcp.exe` (i7, no cookie) | `main` | branch |
|---|---|---|
| nothing running -> first APK on screen | ~190 s (a new device per APK) | 70-80 s |
| warm device: another APK / a rebuilt APK (same version) / the same APK | -- | 9.9 / 8.3 / 2.1 s |
| Roblox's first screen | 162.8 (saved) / 189.6 (new) | 26.6 s on the warm device |
| a saved device's `boot_completed` | 124.3 s | 45.5 s |
| Linux, 2 CPUs + llvmpipe (the notebook): new / saved device, app on screen | ~389 / ~265 s | 266.3 / 205.6 s |

How it works:
- **The warm device** (`omnidroid aosp --warm`; r_roblox `OMNI_R_WARM=1`): the kiosk device with no
  app, saved once as `omni-golden/base-kiosk-<locale>-v2`, booted from a copy. `omni-mcp` finds the
  host's one live `<temp>/omni-warm-<secs>` (heartbeat `<dir>.ctl/alive`), or boots one under
  `<temp>/omni-warm.lock` -- never a second, and not while a Roblox session/standby runs. It outlives
  the server. `start_instance`/`install_apk` **without `cookie`** act on it; with `cookie` the
  Roblox session path (standby, saved devices) is unchanged.
- **The control channel** (`omni-linux-run --control <dir>`, every harness boot: `<instance>.ctl`):
  `<id>.cmd` -> `<id>.out` + `<id>.rc`, shell user or `#uid=<n>`. `tools/device_ctl.py` is adb shell.
- **By content**: SHA-256 vs the device's own `base.apk` (read on the host); reuse / `pm install -r
  -d -g` / uninstall first on a refused signature or downgrade / other test apps uninstalled. The
  launcher from the APK's manifest (`omni_apk::launch_info_of`). Answers after `am start -W`.
- **Boot levers kept** (each with its log evidence in the commit): `/dev/loop-control` (-20 s),
  odsign left out (-5 s), a static RRO `config_checkWallpaperAtBoot=false` in the device's vendor
  overlay (-30 s; `device/src/overlay/build.sh`), no dexopt at install (artd fails it here anyway),
  no zygote class preload in app host processes (`OMNI_APP_PRELOAD=1` restores). Reverted: no boot
  animation (slower, 3/3 pairs).
- **A spare app process** (`crate::zygote`, warm device only, `OMNI_APP_SPARE`): an app host process
  started ahead of need, ART + binder + the zygote's preload done, waiting in
  `com.omnidroid.spare.Spare` (`/vendor/framework/omni-spare.jar`, `device/src/spare/build.sh`)
  under a reserved pid; the next *installed* app (in `/data/app`) ActivityManager starts is answered
  with that pid, its uid rebound (`remote::rebind_uid`), its arguments read from a file. -4.7 s probe,
  -2.9 s Roblox; ~200-350 MB while it waits; the next one 5 s after.
- `omni-linux-run` raises its descriptor limit (Linux's 1024 ran out on 2 CPUs: init's services
  failed to start, the Watchdog ended system_server). Switches: `OMNI_DEVICE_OVERLAY=0`, `OMNI_DEVICE_BOOT_RC=0`,
  `OMNI_KEEP_SERVICES=odsign`.

Open (next, in order): app start is the clock now (~6 s probe from a spare, ~19 s Roblox, one thread,
~50% in dynarmic translating) -> a translation cache shared across app host processes; a persistent control shell (0.43 s a
command); checkpoint/restore via CRIU on headless Linux (`docs/research/2026-10-01-checkpoint-restore.md`).
Gotchas: `<instance>.ctl` is a directory beside every instance -- match instances as
`<prefix><digits>`; a Claude session holds `target/release/omni-mcp.exe` (rename it to rebuild);
`omnidroid aosp` runs `cargo test`, so it builds any source edits first (baselines need a clean
worktree: `C:\od-base` at `main`).


## FAST STARTS: SAVED DEVICES AND A STANDBY INSTANCE (2026-09-30 evening, Windows; uncommitted on `main`)

Goal (owner): an agent driving omnidroid over MCP, paid per hour of LLM serving, must not wait
minutes for a boot -- a few seconds from `start_instance` to being in the place. Every run:
`~/Desktop/Roblox-2.740.931.apk`, `Desktop/cookies/HeZmI_ImYu1080.txt`, PS99, kiosk, release. Each
log's `[t] +<secs>s` lines (`tests/common/boot.rs`: a mark each second, the milestones again) give
the timeline.

| from `start_instance` (or the run's start) | seconds | what |
|---|---|---|
| a device made new (before; still the first session for an APK + account) | **492** to `onGameLoaded` | boot_completed 164, install 197, first start, cookie planted and started again 271, DID_LOG_IN 398, a fixed 45 s, join 444 |
| a saved device (`omnidroid aosp`, always) | **210-215** when the app lives (one start from the launcher; the relaunch below adds ~45 s) | device copied 1.4 s, boot_completed 118-125, `am start` 153, DID_LOG_IN 171, join 172, Joining 182, loaded 215 |
| **the standby, already in the place** (`OMNI_MCP_STANDBY=1`) | **0.008** (`start_instance` answers `state: in_game`) | measured through the real `omni-mcp.exe` over stdio |
| the standby, another place | 31 (NDS 189707) / 50 (back to PS99) | pick-up <1 s, Joining +15-22 s, loaded +7-32 s |

What makes it:
- **Saved devices** (`r_roblox`, `OMNI_R_GOLDEN`; `omnidroid aosp` sets it to
  `<session dir>/omni-golden`, `--fresh-device` opts out). The first session boots a new device as
  before, and once signed in stops the app, syncs, kills the device, copies its directory (790-803
  MiB, 1.5 s; less `.omni-binds`, `.omni-loopback`, `/data/local/tmp`) and boots the same device
  again. Later sessions boot a copy: no APEX decompression, no first package scan or role grants,
  no install or dexopt, no cookie dance. Keyed by APK stem+size, cookie file stem+mtime, kiosk,
  locale and `DEVICE_SETUP` (now 2).
  - The package installer must stay enabled on a saved device: PackageManager dies at the next boot
    without one ("There must be exactly one installer; found []"). The cold setup no longer
    disables it when it will save; every boot of a saved device disables it once up.
  - A saved device whose app runs 10 minutes without signing in (and never died) is set aside.
- **Start from the launcher, then the link.** Opening the place's link on a cold app goes
  ActivityProtocolLaunch -> "no AppSettings ... Finish self!" -> a pause timeout -> the splash, and
  that put libzstd-jni's initialisation past its worker's ~20 s (library load -> `nativeInitClientSettings`
  18.1-18.3 s against 10-16 s from the launcher): 5 of 6 such starts died with the known SIGSEGV at
  0x40 (research note 2026-09-29). From the launcher: none in the standby runs.
- **An app that dies is started again at once** (host writes `app-died` on "Process
  com.roblox.client (pid ...) has died"; the join loop and the sign-in wait react), and a standby
  in a place joins it again when the app dies or the server disconnects it.
- **The standby** (`omni-mcp`, `OMNI_MCP_STANDBY=1`): at `initialized` the server boots
  `omnidroid aosp --standby --instance <temp>/omni-linux-r-standby-<secs>` with the configured
  APK, cookie and place, not killed on exit. `start_instance` with the same APK and account takes it
  over (`claimed`); a different place is written to `join-place`, which the device's wait loop
  joins. On server exit a taken-over standby is given back still running; `stop_instance` ends it
  (the `stop` file) and boots the next. `list_instances` reports `state` (booting, signed_in,
  joining, in_game + `in_place`, stopped) from the instance's files.
- **Sessions end whole.** A Windows job object (`omni_platform::process::hold_children`) in
  `omnidroid aosp` and in the system's host process: killing either ends every host process under
  it (before, 4 of 6 app host processes outlived a killed system). Linux: app host processes ask
  `PR_SET_PDEATHSIG` (`end_with_parent`). Every instance stops through its `stop` file first.
- **`/proc/<pid>` of another live process** (`procfs::another`: `stat`, `status`, `statm`,
  `cmdline`, `comm` only). ActivityManager checks a provider's process with `/proc/<pid>/stat`
  (`isProcessAliveLocked`); with none, a live MediaProvider whose priority had just changed was
  "crashing", Roblox was detached from it and waited 20 s, and the game load stalled at 0.01 fps.

Open:
- **30 s of every boot is WindowManager's BOOT_TIMEOUT**: a kiosk device has no wallpaper (the image
  wallpaper is SystemUI's) and `config_checkWallpaperAtBoot` holds the display, and with it
  `boot_completed`, until "***** BOOT TIMEOUT: forcing display enabled". A fabricated overlay
  (`cmd overlay fabricate`, root only) sets it, but does not survive a reboot here (idmap2d fails at
  boot, "service 'idmap' died"). A static RRO in the device overlay, or a wallpaper, would end it.
- The Android boot of a saved device (~120 s) is the rest: 22 s before servicemanager starts, 20 s
  of init services, 21 s of system_server services, each app host process preloading the zygote's
  classes (no fork).
- The standby holds ~4-5 GB and some CPU while it waits; it lives `OMNI_MCP_STANDBY_MINUTES` (720).
- Linux: the benchmark notebook's session pinned to 2 CPUs with llvmpipe (Colab's shape) boots,
  saves and reboots: new device boot_completed ~218 s / app on screen ~389 s, saved device ~179 s /
  ~265 s. It first died on Colab: system_server had no `EXTERNAL_STORAGE` (init.environ.rc's
  exports now reach every process `omni-linux-run` starts, `device::global_environment`). **Open:**
  when system_server dies, the system's host process then crashes ("Unhandled SIGSEGV at rip", not
  in translated code) instead of the device restarting its system. macOS not run.

## PERF-WIN: LIGHTER, THE MOUSE THE APP'S, NO HIDDEN DIALOG (2026-09-29 early, Windows; branch `perf-win`, `af44f0e`..`ca3a505`)

Goal (owner): RAM well below ~5.1-5.5 GB (toward ~4 GB); the mouse absolute-free like the old path
(host cursor hidden when Roblox draws its own, held on its mouse-lock); the fps hit while input
streams; CPU. Every run boots **`~/Desktop/Roblox-2.740.931.apk`** with `Desktop/cookies/
HeZmI_ImYu1080.txt`, PS99, kiosk, 1575x890, release; host input by `SendInput` (the path a physical
mouse and keyboard take), phases marked in the log through the control file. Runs and scripts in the
session scratchpad (`runs/<name>-{run,mem,phases}.txt`, logs `%TEMP%\omni-linux-r-<pid>.log`).

**Read first -- the APK and what "in-world fps" now means.** That APK is not the stock client: in
the world it shows a **Delta executor** overlay ("Access Delta by completing the key system ... Start
exploiting after completing our key system!"). Nothing here touched its UI. **Its client is never
kicked** (no reason-305 disconnect in any run), so the world stays live -- other players, server
updates, chat -- and fps is lower and noisier than last session's 57.1, which was a disconnected,
static world after the kick. Same build (`main`, `3aefcd3`), two runs: 36.4 (base1, n=20) and 32.0
(base2, n=47) median idle -- the live world moves the figure by ~+-2.

| | `main` (base2; base1) | `perf-win` (run F) |
|---|---|---|
| private bytes, all host processes, in-world steady | 5.26 GB (base1 5.15) | **4.58 GB** (IME left out: ~4.45) |
| working set | 5.47 GB (base1 5.97) | **4.79 GB** |
| host processes | 7 | 6 (5 with `OMNI_KIOSK_IME=0`) |
| system host process | 1.74 GB | 1.41 GB |
| idle fps, median (p10) | 32.0 (30.6) / base1 36.4 (34.2) | 33.5 (32.4), n=48 |
| mouse streaming over the window (1000 Hz host) | base1 32.9 (-10%) | 31.8 (-5%) |
| key, event -> handled, p50 | 1.5-3.3 ms | 2.0-2.4 ms (1.6-3.8 without the IME) |
| a pointer move / a click, p50 | 2.5-7 / 4-7 ms | 1.9-6.7 / 2.1-7.8 ms |
| startup "isn't responding" dialogs | base1 1, base2 2, runs C/D 4 and 2 | 0 (runs E, F) |
| host cursor over the engine | shown (two cursors) | hidden: the app asks for `TYPE_NULL` |
| right-button camera drag | host cursor wanders off | held where pressed, given back there |

### A hidden "isn't responding" dialog ate the input (`7fb8f33`)

Translated, Roblox's `LauncherAliasMain` can hold its main thread past the input dispatcher's 5 s
while it starts; a hover of the host cursor over the window is enough. The "Roblox yanıt vermiyor"
dialog then sits over the game (seen in run D's screenshots) and **takes the pointer and the keys**:
the app never hears the mouse (no pointer icon, no camera drag -- why cursor-follow "failed" in runs C
and D), and **a space presses "Uygulamayı kapat"**: the game is gone ("Force finishing activity"),
which first looked like a crash on typing. `main` has it too (base1 1, base2 2 of them). Fix:
**`ro.hw_timeout_multiplier` 5** (`props::OVERLAY`), Android's own scale for a slow device's timeouts
(`Build.HW_TIMEOUT_MULTIPLIER`, `HwTimeoutMultiplier()`: the dispatcher's and ActivityManager's ANR
timeouts), as emulators and Cuttlefish set it. Runs E and F: 0 dialogs; run C's key sequence twice,
W held, clicks, drags -- the game stays.

### Memory: 5.26 -> 4.58 GB

- **A trimmed process's translations are really given back** (dynarmic **0030**, `aee93ac`). A clear
  (`code_trim`) forgot every block but retired only *full* regions; the region being filled stayed
  committed. The ~60 services of the system's host process each fit in one 16 MiB region, so after
  the trims 65 caches still held 462 MiB. Now a clear retires that region too (same retire/reclaim
  path as a full one) and the next block starts a fresh region. `code_trim`'s floor 24 -> 8 MiB.
  `tests/shared_cache.rs::a_clear_gives_back_the_region_being_filled` (5.7 -> 3.0 MB committed).
- **...and the block map** (dynarmic **0031**, `99d6081`): `clear()` kept the robin_map's buckets;
  the system process held 86 MiB of block maps for 562k blocks (157 B a block). Now a new map:
  44 MiB for 860k (51 B), run E.
- **The kiosk's placeholder home is ended behind the app** (`cb20b4c`): FallbackHome (all of Settings,
  195-240 MB) is kept by ActivityManager at HOME_APP_ADJ; `omni_lean` runs `cmd activity kill
  com.android.settings` every 30 s on a device without a launcher (background only; Android restarts
  it if the home is asked for; `persist.omni.home_process=keep`).
- **`OMNI_KIOSK_IME=0`** leaves LatinIME out (another ~90-120 MB, and keys lose the IME round trip:
  p50 1.6-3.8 ms, run E). **Not the default**: PS99 did not open its chat on "/", so typing into a
  text box without an IME was not seen to work.
- Measured, not changed: Roblox's engine heaps (mimalloc, 971 + 608 MiB) are the same with a 4 GiB
  device (`OMNI_DEVICE_RAM_MB`, `e30fb34`, default still 8 GiB); it gives memory back only with
  `MADV_DONTNEED` (~70 MiB a minute, decommitted -- `OMNI_MADVISE_STATS`), so its heap is live data.
  Its Vulkan device memory is 321 MiB device-local (VRAM) and ~28 MiB host-visible
  (`OMNI_GPU_MEM`); the ~512 MiB of 32 MiB private blocks in its host process are not the app's
  host-visible allocations nor our staging (5.6 MB an image) -- **unexplained, likely the driver's
  per-pool memory: the next RAM lever to pin**. Its cache's tables: 125 MiB for 238 MiB of code.
- Diagnostics: `OMNI_MEM_TRACE` also prints the code caches' table census; `OMNI_GPU_MEM=<s>`;
  `OMNI_MADVISE_STATS=<s>`.

### The mouse (`f58547a`, `07a2639`)

- **The host cursor follows the app's own**: Roblox's `RBXSurfaceView` resolves `TYPE_NULL`, and its
  `ViewRootImpl` sends `IInputManager.setPointerIcon` (transaction 36 in the image) when the icon
  changes. The broker got a tap (`Broker::tap`: a host callback reading every transaction of one
  code to one node as it passes); the injector installs it on the input service; the window hides
  the host cursor over the view while the icon is `TYPE_NULL`, shows it otherwise
  (`OMNI_HOST_CURSOR=show|hide` still decide instead). One cursor on screen: the engine's.
- **Camera drag held**: a secondary-button drag over a view drawing its own pointer (Roblox's
  right-drag, `LockCurrentPosition`, which takes no pointer capture on Android -- confirmed: no
  capture in any run) captures the host cursor where it was pressed; raw motion moves the app's
  absolute pointer (kept on the display, as a device's); the release is where that pointer is, then
  the pointer goes back to the press, where the engine kept its cursor. `[window] mouse held (a
  camera drag)` / `free` in runs E and F.
- **The app's capture (shift-lock, first person) holds the cursor at the window's centre** (`Out::
  Center`), so it is given back there. PS99 caps zoom short of first person, so not exercised
  in-world; D7 covers the capture round trip.
- **A moving pointer at most 60 times a second** (was 125): the streaming cost halved (-10% -> -5%).
- Input that lands in the game costs frames as game work: W held 29.8, right-drag camera 22.3,
  clicks 26.5 (run F; base2's input phases hit the dialog, not the game). No <10 fps seen in any run.

### CPU: where a frame goes now (run D, `OMNI_THREAD_CPU_APP`, `OMNI_SYSCALL_STATS`, `OMNI_GPU_STATS`)

- **"RBX Worker C" at 98-105% of a core, 76% translated code**; the render thread (`FunctionMarshal`)
  88-91%, half translated code, half kernel -- about a third of its samples waiting. In a live world
  the limit is guest compute on that worker, not the forwarding.
- Vulkan: ~107k forwarded calls/s (~3,100 a frame at ~33 fps), ~0.68 s per 15 s in the host driver
  plus ~0.35 us of crossing each: **~8% of the render thread**; batching the `vkCmd*` calls could win
  back about half of that. The release's GPU wait (5.6 ms) is on the asynchronous worker.
  `clock_gettime` no longer appears (the vDSO); mediaextractor's timer restarts are gone (POSIX timers).
  Not done: batching, and faster translated code (the lever that remains).

### Gates

- `d7_window_input` (lean device, probe app): **passes** (119 s) -- keys, exact clicks, hover, wheel,
  the capture round trip (`mouse held (the app holds the pointer capture)`, `rel 30,-10`, `free`).
  Its wheel step had failed twice with the 8 MiB trim floor: a notification shown over the probe
  between the hover and the wheel (the notification assistant was started exactly there, both
  times) took them; D7 now turns heads-up notifications off (`ca3a505`). Correlated 4 of 4, not
  proven; passes also with trimming off and with `OMNI_CODE_TRIM_MIN_MB=24`.
- `lean_image`, `omni-linux` unit tests (29), dynarmic-sys and omni-cpu suites (101 binaries):
  green. `r_roblox` in-world: runs B-F above.

### Found, not fixed

- The ~512 MiB of 32 MiB private blocks in the game's host process (above).
- The system host process runs ~950 host threads; each guest thread's fast-dispatch table is 64 KiB.
- SurfaceFlinger's translations (57 MiB) are never trimmed (it runs every frame).
## LINUX PORT OF THE REAL-AOSP PATH, AND A GL BACKEND (2026-09-29, Linux; branch `linux-port`)

Goal (owner): the tree builds and runs natively on Linux x86-64 without breaking Windows or macOS;
verify by launching Roblox (APK 2.740.931) into PS99. Mid-way the owner asked for an **OpenGL
fallback** for a host without Vulkan (this Ubuntu: Quadro 4000, Fermi), kept open for the Windows
and Mac agents to plug their hosts into, tested only here. Branched from `unified` (the checkout was
on `unified`, 1,037 commits ahead of `main`); the owner merges.

- **Build**: release, no Linux compile errors (the earlier `port-linux` merge covered the tree).
  Suite: see `docs/ports/linux.md` and the branch's final report.
- **GL backend** (`OMNI_GPU=vulkan|gl|auto`; `crates/omni-linux/src/gpu/{backend,gl}.rs`,
  `device/src/gl/`, `tools/gen_gl_forward.py`, gate `tests/d3g_gl_fallback.rs`). The guest's
  `libGLES_omni.so` is an all-in-one EGL 1.4 + GLES driver (`ro.hardware.egl=omni`); every `gl*`
  command is one `ioctl(OMNI_GL_CALL)` on `/dev/omni-gpu`, generated from the old path's
  `omni-android/src/gles/signatures.rs`, which the host includes by path (one table; its fingerprint
  is checked at `eglInitialize`). Pointers pass as they are (one address space). By hand: strings
  (copied), buffer maps (guest shadows), debug callbacks (not delivered), EGLImages (host textures,
  read back at flush points once render targets), window surfaces (host pbuffers read back into the
  window's gralloc buffer at swap). **Plugging in Windows or macOS**: `gpu::gl::ROWS` has ANGLE rows
  (D3D11, Metal) with their displays; run the gate there with `OMNI_GPU=gl`. Nothing else is
  host-specific. `auto` keeps Vulkan wherever the host has a Vulkan GPU (Windows, Mac: unchanged).
- **Linux-specific**: `/dev/shm` for graphics regions (`shm::host_dir`); `omnidroid aosp` moves the
  instance off Ubuntu's tmpfs `/tmp`; `python3` for the cookie step; `ro.hw_timeout_multiplier` 5 on
  hosts with < 8 CPUs (Android's startup timeout killed Roblox's first start twice).
- **Verdict**: booted, signed in, PS99 **not reached**: `join-game` answers 403 with a security
  challenge (`challengedByGcs`), 4 runs of 4 (the 4th with Android's default cached processes), and the app's "Security" WebView says "Unable to contact server" (its
  network works: host TCP to Roblox and CloudFront). Not worked around. For the owner: does this
  account join PS99 on a phone right now; is a challenge pending on it?
- Open, next: the GL backend's per-frame readback (`glReadPixels` at every swap) and the upload of
  every targeted EGLImage are the obvious costs; a PBO ring or a shared host texture would remove
  them. The old path cannot run 2.740.931 (`dladdr` unimplemented).

## PLAYABLE INPUT, LIGHTER, FASTER (2026-09-28/29 night, Windows; `5d01e0e`..`f664bc4`)

Goal (owner): input unplayable (mouse captured on click, Right Ctrl to release; a click took 5-7 s;
input dropped in-world fps below 10), memory ~6-7 GB, CPU. Runs: `work/inworld*-run.txt`,
`work/inworld*-mem.txt` (host private bytes per process every 30 s), logs
`%TEMP%\omni-linux-r-<pid>.log`. APKs: run 1 the owner's modified 2.738.1397
(`Desktop/Omni Apps/omnidroid/Roblox-2.738.1397.apk`); from run 3 on
**`Downloads/Roblox-2.739.691.apk`** at the owner's word (2.738 is no longer accepted by Roblox's
servers: run 2 never joined). 2.739.691 is kicked ~80 s in (error 305, "emulator"); the scene keeps
rendering behind the dialog and in-world fps is measured after the kick, as before.

| | start of session (run 1, 2.738) | end (run 7, 2.739.691) |
|---|---|---|
| mouse | captured on a click, Right Ctrl releases | free, absolute; held only while the app holds the pointer capture |
| a key, event -> handled by the app | p50 ~1-4 s, max 5.3 s | **p50 4.2-5.5 ms**, max 13 ms |
| a click | same queue as the keys (seconds) | **p50 5.1-7.0 ms**, max 16 ms |
| 1000 Hz mouse flood | fps < 10 (owner) | p50 5-16 ms, max 28 ms; fps 52 -> 46-48 |
| in-world fps, median after the kick | 34.5 (p10 33.9, n=108) | **57.1** (p10 55.6, n=144) |
| private bytes, all host processes | 7.36 GB (8 processes) | **5.11 GB** (7) |
| system host process | 2.61 GB, 67 init services | 1.69 GB, 46 init services |

### Input

- **The mouse is free and absolute** (`window_input`, `inject`). The host cursor's position is the
  app's pointer. Android has no absolute mouse device (CursorInputMapper is relative, with its
  own acceleration), so the host injects the mouse's MotionEvents itself:
  `IInputManager.injectInputEvent` (transaction 11, read from the image's `IInputManager$Stub.
  getDefaultTransactionName` switch), as binder client uid 1000, one way, in CursorInputMapper's
  order (BUTTON_RELEASE, DOWN/UP/MOVE/HOVER_MOVE, BUTTON_PRESS, HOVER_MOVE after UP, SCROLL),
  `MotionEvent::writeToParcel`'s AOSP 15 layout (LineageOS 22.1 mirror; `PointerCoords` bits are
  `BitSet64`, bit n = `0x8000000000000000 >> n` -- the first attempt put every click at 0,0).
  A guest-side injector (Java, app_process) was tried first and cannot run: a second ART in the
  system's host process finds no low 4 GiB (system_server holds it) -- the same reason `svc`
  aborts there.
- **Held only while the app holds Android's pointer capture**: `input_channel` reads the
  dispatcher's `CAPTURE` message on the window's input channel (`InputMessage`, type 4); then
  the host window captures, raw motion goes to the relative evdev mouse (`SOURCE_MOUSE_RELATIVE`,
  what `onCapturedPointer` gets). Focus loss frees it (Alt+Tab); focus back retakes it if the app
  still holds it. No Right Ctrl. `OMNI_HOST_CURSOR=hide` hides the host cursor over the window
  (default shown).
- **Coalesced**: a moving pointer at most every 8 ms (`OMNI_POINTER_HZ`), captured motion summed,
  a press carries its position. The old held mouse sent one evdev packet per raw-input batch into
  a 4096-event queue that translated InputReader drained slower than it filled.
- **The 5-7 s**: every key went to the IME first (`ImeInputStage`) and the IME's answers were
  lost: `Timeout waiting for IME to handle input event after 2500 ms`, `spent 2501ms processing
  KeyEvent`, and the dispatcher held pointer events behind the waiting keys. Cause, in `remote`/
  `relay`: InputMethodManagerService hands the app a dup of the IME session's channel at every
  `startInput`; each crossing became a separate local end with its own relay, all taking turns at
  the one queue in the system process, so replies went to ends the app had let go. Now an end
  that crosses to the same host process again is that same end (`crossing_id`), as a dup of one
  socket is one socket (`remote::tests::an_end_that_crosses_twice_is_one_end_and_loses_nothing`).
- **Latency is measured in the kernel** (`input_channel`): each KEY/MOTION's event time to the
  app's FINISHED on the same channel: `[input] N events answered ... p50/p90/max; presses (n) ...;
  keys (n) ...` every 5 s while there is input.
- Gate `tests/d7_window_input.rs` rewritten: absolute clicks exact at (400,300) and (1000,150),
  hover, wheel, the probe's pointer capture round trip (C/R keys; `captured ... source 0x20004 rel
  30,-10`), a 1000 Hz flood's latency line. Probe app: C requests, R releases the capture.

### Kernel

- **Wait queues by key** (`poll`): pipes, socket pairs, bound sockets, eventfd, timerfd, a
  binder process's work, evdev devices notify their own key; epoll/ppoll/read/binder/relay waiters
  register the keys they wait on before they look. Unkeyed changes still wake everyone (unconverted
  sites keep the old semantics). Boot: 10.8k/30.4k/77k wake-ups/s -> 3.1k/6.8k/24.9k at the same
  phases (`OMNI_POLL_STATS=<s>`). Much of what remains is the 50 ms signal slice.
- **vDSO** (`vdso`, `device/src/vdso/`): `__kernel_clock_gettime/gettimeofday/clock_getres/
  rt_sigreturn` over `CNTVCT_EL0` and a data page; `AT_SYSINFO_EHDR`. The system calls compute
  CLOCK_MONOTONIC from the same counter (`sys::counter_ns`, `counter_offset`) and CLOCK_REALTIME
  as monotonic + the instance's origin, so both paths are one function of time (`tests/vdso.rs`:
  libc 53 ns vs the call 153 ns idle; interleaved reads never go back).
- **POSIX timers** (`timer`): create/settime/gettime/getoverrun/delete, SIGEV_SIGNAL/THREAD_ID/NONE;
  a timer's `SI_TIMER` siginfo queued per thread and taken by delivery or `rt_sigtimedwait` (bionic's
  SIGEV_THREAD helper checks exactly that); overruns counted while the signal waits
  (`tests/timers.rs`). mediaextractor's `timer_create` restarts are gone.

### Speed: 34.5 -> 57.1 fps in-world

- **Asynchronous swapchain release** (`gpu::native`): `vkQueueSignalReleaseImageANDROID`
  submits the copy and returns; a worker waits for the fence (NVIDIA spins in it: ~3.1 ms a frame
  on the render thread) and copies into the region; the region's metadata page carries the pending
  generation (`PENDING_GENERATION_AT` 4080) that the composer and the AHB mirrors wait on
  (`wait_written`, 50 ms at most). An image's next release and its destruction wait for its copy.
- **Confound, said plainly**: the async release and the APK switch landed in the same run (run 5:
  52.9 fps). 2.739.691 was not measured on this path without it. The render thread is still the
  limit (FunctionMarshal ~1 core; its workers mostly waiting).
- Not done (next levers, unchanged from before): `vkCmd*` batching (<= ~1.6 ms/frame at 34 fps;
  less now), translated bionic memcpy/malloc (the old path ran them natively).

### Memory: 7.36 -> 5.11 GB

- **Lean hardware** (`device::HARDWARE_LEFT_OUT`, on in `lean` and `kiosk`; `OMNI_DEVICE_APPS=
  lean-hw` keeps it): the HALs of hardware the device lacks -- camera providers, fingerprint/face
  (the emulator's fingerprint HAL aborted every boot: QEMU pipe), Bluetooth, USB, context hub,
  identity, lights, vibrator, power stats, thermal mock, GNSS, goldfish codec2/allocator 3/hwc3,
  atrace -- and incidentd, storaged, update_verifier, misctrl, each with its VINTF fragment and
  feature files. **cameraserver stays**: without it Roblox's engine waits for `media.camera` and
  its display stalled (run 4). 67 -> 46 init services, system host process 2.61 -> 2.12 GB (run 2).
- **Whole-page file views** (`mm`): a mapping reaching the end of a file whose size is not a page
  multiple asked for a rounded-up view, which a read-only view may not do, and silently fell back
  to a private copy of the whole file -- ICU data (26 MiB, twice per ART process), fonts, APKs,
  vdex. Now the whole pages are the view and only the partial last page is copied
  (`tests/mm.rs::a_file_mapping_is_a_view_of_its_whole_pages_and_copies_only_the_last`). Idle app
  guest memory 135-150 -> 58-78 MiB. Installed apps' `lib/*.so` are views too (libroblox.so was
  98 MiB private); only those -- a live view blocks installd's rename of a staging directory
  (`INSTALL_FAILED_INSUFFICIENT_STORAGE`, run 3, when every `/data/app` file was a view).
- **Translation trimming** (`code_trim`): a guest process quiet for a minute (< 1 MiB newly
  translated) holding > 24 MiB of translations has them dropped (`clear_code_cache`, patch 0022),
  given back once its threads leave generated code; never in a busy host process (the game's) nor
  SurfaceFlinger; at most every 10 min (`OMNI_CODE_TRIM=0`). system_server 219 -> 14 MiB, idle
  apps 66-91 -> ~15-35 MiB. No fps cost measured (57.1).
- `omni_lean` confirms both settings (run 5 got "post-boot grace null ms" and Android kept its
  10-minute grace for cached processes: 12 host processes).
- Where it is now (run 7): Roblox 2.88 GB (engine mimalloc heaps ~1.55 GB, translations 234 MiB,
  the rest mostly the host GPU driver's), system host 1.69 GB (65 guest processes: guest 359 MiB,
  translations 461 MiB -- SurfaceFlinger 59, the rest small services near one region each),
  Settings/FallbackHome 197 MB, media module 139, network stack 118, IME 107, ext services 105.
  A custom minimal AOSP image was not built: the lean device leaves out at the image level
  (packages, HALs, features) what such a build would, on the pinned image.
- `OMNI_MEM_TRACE=<s>` now also prints every guest process of the host process (guest memory +
  translations).

### Found, not fixed

- SurfaceFlinger logs `trackPendingFrame: Invalid present fence` every frame (the host composer
  answers -1); pre-existing, ~25k lines a run.
- `svc` (app_process) aborts in the system host process (no low 4 GiB); the setup's `svc power
  stayon` never ran. Pre-existing.
- D7's FORTIFY `pthread_mutex_lock called on a destroyed mutex` (2 lines each run), pre-existing.

## LIGHTER AND FASTER (2026-09-28 night, Windows; `6eff7ff`..`7669317`)

Goal: heavily less RAM, heavily more speed on the real-AOSP path. Measured first, every figure
below is from a run named in `work/` (logs in `%TEMP%\omni-linux-r-<pid>.log`), Roblox stock APK,
release, live window, 1280x720.

### Memory: 8.96 GB -> 5.81 GB on Landing (kiosk, the default now)

Per host process, sampled every 30 s from the host (private bytes / working set;
`scratchpad memsample.ps1` -- `Get-Process` of each `omni-linux-run` by its `--nice-name`):

| device | processes | private | working set | run |
|---|---|---|---|---|
| before (runtime `pm disable-user` list only) | 18 | 8.96 GB | 9.8 GB | `perf-base-mem.txt` |
| lean image (default device) | 15 | 8.13 GB | 8.8 GB | `perf-lean-mem.txt` |
| **kiosk** (lean + no SystemUI/launcher + no cached processes) | **8** | **5.81 GB** | **6.04 GB** | `perf-kiosk-mem.txt` |
| kiosk, signed in, in PS99 (loading) | 7 | 7.1 GB | 7.2 GB | `perf-ingame-mem.txt` |

Before: system_server's host process 2.5 GB, Roblox 1.4, SystemUI 0.55, launcher 0.52, Settings
(FallbackHome) 0.40, and 13 more app processes of 250-320 MB each (media, permission controller,
phone, IME, network stack, ext services, adservices, SE, shell, webview zygote + service,
multidisplay, ...). The runtime's `pm disable-user` did not stop **persistent** apps: Android
never kills a persistent process when its package is disabled (`com.android.se`,
`com.android.emulator.multidisplay` alive after it). `com.android.phone` was started once, not
crash-looping (`d36233b` fixed that); it is simply gone now.

What changed (`512cd79`), all Android's own mechanisms:
- **Apps left out of the image** (`device::LEAVES_OUT`, the way a product build leaves packages
  out of `PRODUCT_PACKAGES`): telephony (TeleService and the phone-uid apps), SecureElement,
  MultiDisplayProvider, Bluetooth's feature files, print, backup transport, contacts, calendar,
  messaging, a phone's own apps. PackageManager never scans them, nothing starts them.
  Bluetooth, backup and print are also gone from `handheld_core_hardware.xml`, so SystemServer
  starts none of those services. `OMNI_DEVICE_APPS=full|lean|kiosk` (default lean).
- **Kiosk from the first boot**: `kiosk` also leaves out SystemUI and Launcher3QuickStep; the
  home is Settings' FallbackHome. The second boot the old kiosk needed is gone. **`r_roblox` and
  `tools/aosp_play.ps1` now run the kiosk device by default** (`OMNI_R_KIOSK=0` /
  `-WithSystemUI`: with them, ~1.1 GB more).
- **No cached app processes** (`omni_lean.sh`, an init service): ActivityManager's
  `max_cached_processes 0` -- Developer options' "No background processes" -- and its
  `no_kill_cached_processes_post_boot_completed_duration_millis` 0 (Android spares cached
  processes for 10 minutes after boot; the first attempt reaped nothing because of it,
  `dumpsys activity settings`). `persist.omni.cached_processes=<n>|off`.
- Kept (measured need): the WebView (Roblox binds `VariationsSeedServer` at start), the IME,
  MediaProvider (persistent, storage), the network stack, ext services, the package installer.
- Left at 8: system_server 2.4 GB, Roblox 1.5, IME 0.40, Settings 0.40, media 0.32, network
  stack 0.28, ext services 0.27, webview zygote 0.25. The next lever is the system host process
  itself (all of init's ~66 services and system_server in one host process).
- Gate `tests/lean_image.rs`; r_roblox passes in lean (`perf-lean-run1.txt`) and kiosk
  (`perf-kiosk-run1.txt`) -- Roblox boots, installs, reaches Landing and draws.

### Speed: in-world PS99 4.0 -> 34.6 fps

**Landing's 0.99 fps is the engine idling, not a stall.** Profiled (`OMNI_SLOW_APP` stacks,
`OMNI_THREAD_CPU_APP`): the Roblox process used **0.07 cores**; one engine thread (entry near
`libroblox.so+0x22a3398`) times out a 1000 ms condition wait every second and the engine's main
thread (`FunctionMarshal`) renders once after it. Every run shows ~25 fps while Landing animates
in, then 0.99 once the screen is still. Input raises it: the frames follow the input (a scripted
`move` every 50 ms gave ~4.4 fps only because the window's control file is read every 250 ms,
`CONTROL_EVERY`). So the Landing number that matters is the animated phase.

**Where a frame went** (`OMNI_GPU_STATS`, per forwarded Vulkan command): ~220 commands per frame at
~1 us each -- the per-call forwarding is not the cost -- and **`vkQueueSignalReleaseImageANDROID`
~24 ms**, of which **22.5 ms putting the 1280x720 frame into its gralloc region** and ~1.5 ms the
GPU wait. Two causes (`246bc9e`):
1. The staging buffer was `HOST_VISIBLE|HOST_COHERENT` without `HOST_CACHED` -- on NVIDIA that is
   uncached, write-combined memory, which the CPU reads at a fraction of memory speed. Now
   host-cached (invalidated when not coherent); writable AHB mirrors likewise.
2. The region was written (and the composer read it back) with a 3.6 MiB file write/read
   through the host's file system each frame. Graphics-buffer regions (`Shm::as_graphics_buffer`)
   now go through a host view of their file (`omni_platform::vm`), a memory copy.
   `OMNI_SHM_VIEW=0` restores file I/O for comparison.

Release now ~2 ms (0.3-0.7 ms into the buffer). Results:

| screen | before | after | runs |
|---|---|---|---|
| Landing, animating in | 25-30 fps (peak) | **52 fps** (peak) | `perf-base-run1`, `perf-cached-run1` |
| Landing, still | 0.99 (engine idle) | 0.99 (engine idle) | same |
| signed-in Home | -- | **47 fps** | `perf-ingame-run2` |
| PS99 join/loading screen | ~4 fps (r26, modified APK) | **40-45 fps** (run 1), 7-18 (run 2) | `perf-ingame-run1/2` |
| PS99 in-world, after the kick | 4.0 (r26, modified APK) | see below: **34.6 fps** median | `perf-inworld-run1..4` |

**In-world is measured after the emulator kick** (the owner: the client disconnects but goes on
rendering the scene, so the frame rate is the real one). The world loads for ~30-60 s after
`onGameLoaded` (0.1-0.3 fps, the engine's workers busy), then renders steadily. Median of the
5-s `[display]` rates after the kick line, each run ~10 minutes:

| change | median fps | p10 / p90 | n | run |
|---|---|---|---|---|
| release fix (`246bc9e`) | 29.0 | 28.0 / 29.8 | 87 | `perf-inworld-run1` |
| + composer keeps a refilled slot (`c6c7cc8`, below) | 28.5 | 27.9 / 28.7 | 66 | `perf-inworld-run2` |
| + `ensure_committed` bumps nothing (`348477c`) | 32.9 | 27.1 / 35.5 | 42 | `perf-inworld-run3` |
| + guest memory checked without the lock (`7669317`) | **34.6** | 33.5 / 35.7 | 42 | `perf-inworld-run4` |

- **The composer** set a layer command's buffer and *then* cleared the command's slots;
  SurfaceFlinger frees and refills a slot in one command (logged: "slots cleared [1]; this
  command's buffer: (1, true)", as the app's swapchain is made anew at the join), so that slot
  held nothing and every third frame went to SurfaceFlinger's own composition (1,474 of 6,000).
  Clearing first: 32 of 9,600. No fps change -- SurfaceFlinger's work ran beside the app's --
  but the system process no longer composes a third of the frames.
- **The guest space's lock.** Every read or write of guest memory by the kernel (a Vulkan call's
  arguments, each `clock_gettime` result -- ~117k a second) called `ensure_committed`, which took
  the space's one mutex and bumped the map's generation, making every thread's remembered
  regions stale, so the next `region_at` everywhere went to the same mutex: all threads'
  system calls serialized on one lock. Now a committed range bumps nothing (omni-mem), and
  `guest.rs` does not even ask when its regions (from the thread's cache) are committed. A
  forwarded Vulkan call's kernel time ~5.6 -> ~2.9 us; `clock_gettime` ~2.6 -> ~0.8 us.
- In-world memory: 7 host processes, 6.98 GB private (Roblox 2.9 GB).

**What limits it now** (run 4): the engine's render thread (`FunctionMarshal`) is saturated,
~100% of a core, ~29 ms a frame: ~53% of its samples in translated code, ~40% in system calls.
Per frame ~2,240 forwarded Vulkan calls at ~1.3 us each (~0.64 us the host driver, ~0.7 us the
crossing: ~1.6 ms -- batching the `vkCmd*` calls would save at most that), and the release,
~3.5 ms, of which ~3.1 ms `vkWaitForFences` (which NVIDIA's driver spins in) -- an asynchronous
release (a real fence handed to the consumer) is the larger forwarding lever. The rest is the
engine's own code, translated: the old path (omni-android, 47-53 fps in-world) ran bionic's libc
(memcpy, malloc, ...) natively on the host; this path translates real bionic. That, not the
graphics forwarding, is the remaining gap.

**Reading OMNI_THREAD_CPU's `kern`**: its samples, now bucketed by host address, sit almost all
at `ntdll!ZwWaitForAlertByThreadId` -- a thread parked on a host lock or condition variable,
sampled just after it ran. A high `kern` share mostly means waiting, not handler time (r26's
"93-96% kern" read as handler cost was this).

Found on the way, not fixed:
- `mediaextractor` aborts on `timer_create` (ENOSYS: POSIX timers are not implemented) each
  time MediaProvider's boot scan sniffs a sound file (libmidiextractor's `Watchdog`): 25
  restarts in one session. bionic's `SIGEV_THREAD` timers need `SI_TIMER` siginfo, so a real
  implementation, not a stub.
- `clock_gettime` is a system call here (~117k/s in-world): the `[vdso]` page holds only the
  signal trampoline, so bionic falls back to `svc`. A real vDSO would remove the crossing.

Regression with everything above (Windows, release): D5 passed (128 s, was 155: the lean image
boots faster), D8 passed (338 s: app only, chrome shown and hidden, kiosk by a second boot), D7
passed (153 s); omni-linux `shm`, `guest_mem`, `mm`, `d2_gralloc`, `d3a_gpu`, `lean_image`; omni-mem
all. Portability: no platform code added -- the views go through `omni_platform::vm`, the rest
is omni-linux/omni-mem logic; not built off Windows (the goal's rule: Linux/Mac not run).

Diagnostics added (`6eff7ff`): `OMNI_SLOW_SYSCALL_MS` / `OMNI_SLOW_APP=<process>`
(`OMNI_SLOW_APP_MS`): each system call at least that long, with its frame-pointer chain;
`OMNI_GPU_STATS=<s>`: per forwarded Vulkan command, calls and host time, with the release split.
`OMNI_COMPOSER_TRACE=layers` also names a bufferless layer's slot, each slot clear, and any
buffer handle the composer refuses.

## ONLY THE APP IN THE WINDOW (2026-09-28 evening, Windows; `68ad3fe`, `904c79f`, `e43f680`)

The live window (and every framebuffer screenshot) now shows **only the app**: no status bar, no
navigation bar, no taskbar. Two mechanisms, both Android's own objects, nothing in the framework
changed:

- **The composer leaves the chrome out** (`hal::composer`, default on; `OMNI_APP_ONLY=0` shows the
  whole display; `chrome show|hide` in the window's control file toggles it live, with `onRefresh`
  so a still screen changes). It knows a chrome layer by the window its buffers are for: a view
  root's BufferQueue names its buffers after the window when it asks the allocator
  (`VRI[StatusBar]#0(BLAST Consumer)0`, `VRI[Taskbar]#0...`; `hal::gralloc` keeps the name in the
  region's metadata page; `is_chrome`: `StatusBar`, `NavigationBar*`, `Taskbar`,
  `ScreenDecorOverlay*`, `ScreenDecorHwcLayer`). A chrome layer stays `DEVICE` (the composer's to
  draw) and is not drawn; the rest are composed here, or -- when one is not the composer's -- go
  to SurfaceFlinger as `CLIENT`, whose client target then lacks only the chrome. Limit: a chrome
  layer SurfaceFlinger itself insists on composing (`CLIENT` requested, e.g. a blur) would be
  drawn; not seen.
- **Kiosk: a device without SystemUI** (`OMNI_R_KIOSK=1`, `tools/aosp_play.ps1 -Kiosk`). With
  SystemUI running, the app still *lays out* around the bars (its own background where they were)
  unless it hides them itself -- Roblox in a game does, its Landing and Home do not. Android 15
  has no system-side switch to force an arbitrary app immersive (`policy_control` and
  `qemu.hw.mainkeys` are gone from this image's `services.jar`; `cmd window` has nothing). So the
  kiosk: set the device up, `pm disable-user com.android.systemui`, start the device again. No
  SystemUI: no bars, no taskbar (the launcher's taskbar exists only while SystemUI binds its
  service), no keyguard (`KeyguardServiceDelegate` cannot bind it and marks the device as having
  none), and the app is given the whole display. **Disabling SystemUI on a running device does
  not work**: Android shows the keyguard when the keyguard's service dies (D8 run 1: black
  display, SystemUI restarted as a persistent app). The second boot costs ~1 minute
  (`common::boot::Boot::reboot` keeps the instance).
- The composer's own composition also takes more now: a source crop scaled to its frame (nearest
  pixel), the eight HWC transforms, BGRA (`hal::compose`, unit-tested).
- `OMNI_COMPOSER_TRACE=layers`: each frame's layers whenever they change -- buffer name, geometry,
  whether the composer takes it, `HIDDEN` for chrome left out.

Evidence (Windows, release):
- **Gate `tests/d8_app_only.rs`** (366 s, pass): the probe in the live window. App only: the
  status bar's 24 rows and the taskbar's bottom 56 have no near-white pixel (the probe's own bar
  colour and nav scrim there: 1 colour each). `chrome show`: 0.47% near-white in the top strip
  (clock, icons), 96.1% in the bottom one (the taskbar). `chrome hide`: gone again. Kiosk (second
  boot, whole display presented): SurfaceFlinger lists no `StatusBar`/`NavigationBar`/`Taskbar`
  layer, the probe's action bar at the top edge and its blue to the bottom edge. Window captures:
  `docs/runs/2026-09-28-app-only/d8-window-{app-only,chrome-shown,kiosk}.png`;
  `python tools/chrome_check.py <png>` measures the same strips on a capture ("chrome absent /
  present").
- **Mixed path** (D8 with `OMNI_COMPOSER_DEVICE=0 OMNI_D8_KIOSK=0`, 169 s, pass): every frame
  SurfaceFlinger's, the status bar and taskbar `DEVICE`, `HIDDEN`, and absent from the display.
- **Roblox (stock APK, logged out, Landing, live window)**, captures in
  `docs/runs/2026-09-28-app-only/`:
  - with SystemUI, app only (`roblox-landing-app-only.png`): no clock, no taskbar; the bottom 56
    rows are one black colour -- Roblox's own window (`VRI[ActivityNativeMain]`, frame 664..720)
    painting behind where the taskbar was, since the app keeps clear of its insets on this screen;
  - kiosk (`OMNI_R_KIOSK=1`, 565 s run, pass; `roblox-landing-kiosk.png`): **edge to edge** -- one
    layer, `SurfaceView[com.roblox.client/...]` 1280x720, the game art to the bottom edge (539
    colours in the bottom strip) and the Landing UI laid out over the whole display.
  - In kiosk the lean setup must keep `com.android.packageinstaller` enabled: PackageManager does
    not start without an installer ("There must be exactly one installer; found []", system_server
    dead at the second boot, run 1). That run's runner then ended on a dynarmic assertion
    (`IsImmediate() && GetType() == IR::Type::AccType`), after system_server's death -- seen once,
    never before in any kept log; not investigated.

**fps (the goal's item 3) -- not the jump the goal expected, and why.** Device composition of
full-screen app layers already existed (`5b6a0d2`, before this session): on Roblox's Landing
**every** frame was already composed by the composer, none by SurfaceFlinger
(`OMNI_COMPOSER_TRACE=layers`: `SurfaceView[com.roblox.client/...ActivityNativeMain]` 1280x720
RGBA at 1:1, `ours`), and the app presents **0.99 frames/s** there before this change, after it
(app only, 1,200 frames composed here, 0 by SurfaceFlinger) and in the kiosk -- the app's own
rendering is the limit on that screen, not SurfaceFlinger. The in-game 4.0 fps
(r26, 2026-09-28, the modified APK) had ~1/3 of its frames composed by SurfaceFlinger
(`frames composed here 4069, by SurfaceFlinger 1331`) for layers not recorded then; this
session's wider composer (scaling, transforms, BGRA, chrome left out) may take them. **Follow-up:
an in-game run with the owner's cookie and `OMNI_COMPOSER_TRACE=layers`** shows which layers
still go to SurfaceFlinger and the fps with them taken; HANDOFF's r26 profile (the app's threads
93-96% inside system-call handlers) says the app side is the larger cost.

Regression with app-only the default (Windows, release): `omni-linux` lib 20/20; D5 passed
(155 s); D6 passed (817x542 in 2.0 s, 1531x877 in 4.2 s, window frames 1146 = framebuffer
1146); D7 passed (170 s).

Portability: nothing platform-specific changed. `hal::composer`, `hal::compose` and
`display_window` hold no `cfg` and no platform call; the window seam (`omni-platform::window`,
`Presenter`) was already enough -- no new primitive was needed, so Xlib and AppKit are untouched.
Not cross-checked this session (omni-linux cannot be, dynarmic's C++); the D6 note below on the
scratch-crate check still describes how.

## LIVE WINDOW (2026-09-28 day session, Windows; `a1509cd`, `b5a4277`, `6d8db75`, `63a7f4a`)

The real-AOSP path's display is no longer only headless: **`OMNI_WINDOW=1` shows it live in a
native window, and the window's size is Android's display size** -- drag the border and the
display, SystemUI and the running app relayout and redraw at exactly that size, any size (no
aspect ratio, no preset list; 320 px floor per side, Android's minimum screen width).

How it works:
- **Seam (`omni-platform::window`)**: `Window::present_rgba` (host-CPU RGBA, stretched to the
  client area, kept and repainted by the window itself) and `Window::presenter()` -> `Presenter`
  (`Send + Sync`: `present_rgba` and `client_size` from any thread). Win32: `StretchDIBits` from
  `WM_PAINT`, image in a shared canvas -- a presenter on another thread stays live through the
  border drag's modal loop, and `GetClientRect` follows the border. Xlib: `XPutImage` of a
  CPU-scaled image in the visual's layout, repainted on `Expose`. AppKit: a `CGImage` as the
  contents of a sublayer over the view.
- **Sink (`omni_linux::display_window`)**: platform-agnostic (no `cfg`). A window thread pumps and
  runs `OMNI_WINDOW_CONTROL` (`size WxH` lines: scripted resizes through `set_client_size`); a
  present thread shows each framebuffer frame and watches `Presenter::client_size`; a size that
  holds 300 ms becomes the display's. `[window] N frames presented to the window; framebuffer M`
  every 5 s.
- **Resize = Android's own mechanism** (`hal::composer::Composer::set_display_size`): one HWC
  configuration of the new size under a new config id, then `onHotplug(display, connected)` again.
  SurfaceFlinger logs `Reconnecting display 0`, recreates the display at the new mode;
  DisplayManager's LocalDisplayAdapter picks the new mode ("New display modes are added and the
  active mode has changed"); WindowManager sends the configuration change. `Framebuffer` takes each
  frame's own size; client targets carry theirs. `setActiveConfigWithConstraints` is answered.

Evidence (Windows, release):
- **Gate `tests/d6_window_resize.rs`** (179 s, pass): probe at 1280x720 (79.3% its blue), window
  -> **817x542**: display in 2.0 s, `Reconnecting`, probe `onConfigurationChanged screen 817x542
  dp`, view relaid out and drawn at 817x542, 82.8% blue, `wm size` = 817x542; window ->
  **1531x877**: 4.0 s, drawn 1531x877, 89.4%; **window frames 962 = framebuffer frames 962**.
  Window captures (`tools/window_shot.ps1`, PrintWindow of the real window):
  `docs/runs/2026-09-28-live-window/d6-window-start.png` (1280x720), `d6-window-shrink.png`
  (817x542), `d6-window-grow.png` (1531x877); the framebuffer's at each, `work/d6-run2/d6-display-*.png`. Run 1 (960x600 /
  1600x900) passed every stage and then failed only because the window was resized by hand
  to 1024x900 after the grow -- the display followed it; the gate now checks `wm size` against
  the display size in force.
- **Roblox (stock APK, logged out, Landing, `r_roblox` with `OMNI_WINDOW=1`)**: the engine's own
  Vulkan swapchain recreated at **817x542, 1222x633 (a hand drag), 1531x877, 817x542**,
  each within ~3 s (`[FLog::Graphics] Vulkan: swapchain images 3 ... size WxH`,
  `doUpdateAppUISizes() vw:W`); window frames 1545 = framebuffer 1545. Captures:
  `work/roblox-window/r2-1280x720.png`, `r2-817x542-b.png`, `r2-1531x877.png`,
  `r2-817x542.png` (in fact 1222x633: a hand drag).
- **The one thing that is not the app's size by default**: Roblox's `<application>` declares
  `resizeableActivity="false"`, and Android answers a display resize under a non-resizable app
  with size-compatibility mode -- kept at 1280x720, scaled, a "restart for a better view" bubble
  (`work/roblox-window/roblox-817x542-a.png`, run 1). With the window on, `r_roblox` sets the
  device up with Developer options' **"Force activities to be resizable"**
  (`settings put global force_resizable_activities 1`, every app, `OMNI_R_RESIZABLE=0` to leave
  it off); then Android itself gives the app each size. It is a device-wide setting, not a per-app
  patch -- but it is the setting named "force", so the owner decides whether it stays the default.
- Seam tests: `omni-platform/tests/window_present_windows.rs` (read-back of the painted window,
  a presenter on another thread, a presenter outliving its window; 3/3 x 3). GDI `GetPixel` on
  this host reads colour-managed values (0xe01020 -> 0xf1256b) and the opening animation's blend:
  the test compares dominant channels and polls until settled.

Portability (not run off Windows, as the goal said): `cargo check -p omni-platform --tests` for
`x86_64-unknown-linux-gnu` is clean; for `aarch64-apple-darwin` the lib is clean with this PC's
rustc 1.89 given `-Zcrate-attr=feature(new_zeroed_alloc)` (the pre-existing `fault/macos.rs`
uses an API stable only from 1.92; the Mac's own toolchain is newer). `omni-linux` itself cannot
be cross-checked here (dynarmic's C++), so `display_window.rs` + `framebuffer.rs` were
type-checked for all three targets through a scratch crate that includes them verbatim against
the real `omni-platform` (nightly 1.100). **Owed on Linux**: `window_linux.rs`'s new
`a_presented_image_fills_the_window_and_is_repainted_at_a_new_size` (XGetImage read-back, written,
never run) and one D6 run on Xlib. **Owed on macOS**: the AppKit present (type-checked only);
note the window needs the AppKit main thread the backend takes before `main`.

Regression after it (Windows, release): `cargo test -p omni-linux` 86/86 binaries green;
D5 (no window) passed in 149 s; `omni-platform` lib 202/202, `window_live` 11/11,
`window_seam` 7/7; `r_roblox` with the window, 15 minutes, passed.

### Keyboard and mouse in the live window (`e489061`, same day)

**Superseded for the mouse (2026-09-29)**: it is free and absolute now, held only while the app
holds the pointer capture, and there is no Right Ctrl ("PLAYABLE INPUT, LIGHTER, FASTER"). The
placement below (`PLACE_GAIN`) is gone.

The window's keyboard and mouse are now **the device's**, through Android's real input stack:
- **Kernel** (`crate::evdev`): `/dev/input/event0` (keyboard) and `event1` (mouse) as the evdev
  driver presents them -- listed, per-open queues of arm64 `input_event`s on the instance's
  `CLOCK_MONOTONIC`, pollable, the `EVIOC*` requests EventHub makes. system_server's own
  InputReader: "Device added ... 'omnidroid mouse' sources=MOUSE", "'omnidroid keyboard'
  sources=KEYBOARD". `tests/evdev.rs` runs the `evdev` fixture under the real bionic.
- **Translation** (`window_input`, platform-agnostic): keys by physical key (the set-1 ->
  `KEY_*` table moved from omni-android into the window seam as `evdev_code`); **the mouse is held
  on a click** (pointer capture: host cursor hidden, raw motion) and **Right Ctrl gives it back**
  (so does losing the focus); **the Windows (Meta) keys stay the host's** (Meta alone opens Android's
  app list: a D6 run found it open, cause not pinned -- a Win key press or a click on the taskbar;
  `[window] mouse held/given back` is logged since); the title bar says which (`Window::set_title`, new, all three
  backends). Why a hold: Android has no absolute mouse -- `SOURCE_MOUSE`, what Roblox reads a mouse
  by (`omni-android/src/jni/mouse.rs`), comes only from a relative device that Android moves its
  own pointer for, through its own acceleration, and the switch to turn that off
  (`setMousePointerAccelerationEnabled`) is internal to system_server.
- **The grabbing click lands where it was made**: the pointer is sent home (one huge negative move,
  clamped to 0,0), then after 400 ms at rest one move of the target over **`PLACE_GAIN` = 2.04**
  -- MEASURED (d7 run 1: (400,300) after rest landed at (817,612); this image uses the curved
  "new ballistics", whose first sample after rest gets the curve's base gain). A changed Android
  pointer speed would change it.
- **Gate `tests/d7_window_input.rs`** (release, 170 s, pass): `KEYCODE_A` scan 30 from
  `SOURCE_KEYBOARD`; a mouse press (`SOURCE_MOUSE`, tool MOUSE, primary) at exactly (400,300) and at
  (1000,151) for (1000,150); a rested move of (+50,-20) at the predicted (502,259); one wheel notch
  `vscroll 1.0`. Control-file commands: `key`, `click`, `move`, `wheel` (module doc).
- **Roblox**: both devices added, Landing reached; its hand cursor follows the mouse over "Giriş Yap"
  (`work/roblox-input/after-click.png`, a 2305x1085 window the display followed).
- Regression with input on: D6 passes (817x542 in 2.0 s, 1531x877 in 4.2 s, 1090/1090 frames).
- `r_roblox` now sets `force_resizable_activities` **by default** (the owner's choice), window or
  not; `tools/aosp_play.ps1 [-Cookie f] [-Place id] [-Minutes n] [-Size WxH] [-NotResizable]`
  runs it in the live window with input.

Known limits: frames
are copied on the CPU (RGBA -> BGRA and GDI's stretch each present); the display density stays
160 dpi whatever the host's scaling; a Windows drag shows the last frame stretched until the size
has held 300 ms, then the app redraws at it mid-drag.

## MORNING REPORT (overnight run 2026-09-28, Windows only, stock APK)

APK used throughout: `omnidroid-unified/Roblox-2.738.1397.apk`, 229,466,269 bytes, sha256
`BBE00AE3...2742`, signer "Roblox Corporation", 3 dex. **Not** the 159,853,296-byte file of the
same name in the main checkout `Desktop/Omni Apps/omnidroid` (sha256 `4BCB90EE...6128`): that one
has an extra `classes4.dex`, an 18,440,296-byte `libzstd-jni` (stock: 603,960) and is re-signed
with a non-Roblox key (`META-INF/KEY.RSA`, "Gloopiest Man") -- the modified build this file warns
about. `libroblox.so` is byte-identical in both. A `Roblox-2.739.691.apk` is also still in the
unified root; `choose_apk` would pick it (higher versionCode), so always name the APK.

| phase | result | artifact |
|---|---|---|
**Update 09:50 (r21-r26, at the owner's request: the main checkout's modified APK).** That APK
first waited on Android's "All files access" page (r21), then aborted in its own loader
(`com.roblox.gloop.Loader.nativeStart`: "JNI DETECTED ERROR: obj == null") because shared storage
did not exist here (r22: /mnt and /storage absent, vold "emulated;0 failed to create mount
points", the volume unmountable). Fixed as platform features (`760bb59`): shared storage without
FUSE, and an `omni_autogrant` service granting installed apps what they ask for. r26: "Mounted
volume emulated;0", "omni_autogrant: granted com.roblox.client", DID_LOG_IN, `onGameLoaded
placeId:8737899170`, **in-game PS99 on the display at 4.0 fps (steady), no emulator kick in 17
min** (`work/overnight/r26-ps99-in-game.png`). But the APK is the **Delta exploit executor** (its
"Access Delta ... key system ... Start exploiting" panel is on the screenshot); its injected code
is the likely reason the kick did not come. Not interacted with, session stopped. The Roblox host
process in-game: 0.68 cores; its threads' samples are 93-96% `kern` (inside system-call handlers),
FunctionMarshal 26% of a core -- the next speed target is the handlers' own time (OMNI_THREAD_CPU,
`cd5aaac`).

| 1 address space | **fixed** (`d65fbe8`): 10/10 boots reach "System now ready" (72-75 s each, 0 `[mm]` refusals, 0 guest deaths). Before: 5/7 D5 logs of 09-27 reached it, 1/7 exhausted the space (49 refusals, 1,048,576 bytes free of 64 GiB) | `docs/runs/2026-09-28-phase1-c4-10boots.csv`; `tests/low_space.rs` (fails without the fix with the boot log's exact figure) |
| 2 login screen | **the logged-out login screen on the host display** (r15): ROBLOX logo, "Hesap Oluştur" (Create Account), "Giriş Yap" (Log In), Koşullar/Gizlilik over the game-art backdrop, drawn by the engine's own Vulkan (RTX 4060 via `vulkan.omni.so`) through SurfaceFlinger; the engine's `onDataModelNotification() type:APP_READY data:Landing`. Also the **signed-in Home** (r12) | `work/overnight/r15-login-landing.png` (1,531 colours, 43.8% #000000, 9.7% #f8f8f8); `r12-home-signed-in.png` (12,189 colours: Home, "HeZmI_ImYu1080", "Arkadaşlar (10)"); `r15.log` |
| 3 `--cookie` | **signed in, 4/4 runs** (r10-r13): `onDataModelNotification() type:DID_LOG_IN data:{"username":"HeZmI_ImYu1080",...,"countryCode":"TR","userId":5457009831}`; 0 HTTP 401/403 after it (r10) | `work/overnight/r10-signin-markers.txt`, `r13-markers.txt` |
| 3 `--place` | **joined** (r13, `am start -d roblox://experiences/start?placeId=8737899170 -n .../.ActivityProtocolLaunch`): `GameJoinLoadTime join_time:1.773 referral_page:DeepLink placeid:8737899170`, `onGameStarted`, **`D/rbx.jni onGameLoaded() SessionReporterState_GameLoaded placeId:8737899170`**, `ExperienceSession onGameLoaded: placeId = 8737899170`. (The HLE gate's `JOIN: ... returned 1` does not exist on this path; the engine's `GameJoinLoadTime`/`onGameLoaded` are its equivalents.) | `work/overnight/r13-markers.txt`, `r13.log` |
| 4 in place | **ran ~80 s, then Roblox ended it**: after `onGameLoaded`, frames kept presenting (727 -> 1,147 in 281 s, ~1.5/s); PS99's own in-place loading GUI drawn (BIG Games logo, 3/5 dots) under Roblox's in-experience top bar; the game's client scripts ran (r12: `✅ CLIENT \| _L took 1818ms to initialize!`). Then **"Client has been disconnected with reason: Roblox cannot be used in an emulated environment. Please run Roblox on a supported device."** (reason 305, `connectionTime 80142` ms). This is Roblox's emulator detection -- a security control; per this file's rules it is **not** worked around. The in-world screenshot and a settled world are therefore not reached | `work/overnight/r13-inplace-loading.png` (233 colours: 97.6% #f8f8f8, 1.3% #f89840 = the BIG Games logo), `r13-markers.txt` (the disconnect lines) |

| regression (06:45-06:58) | **C4 5/5** "System now ready" (64-67 s); **D5 3/3** -- the probe's blue at the centre, 79.4% of the screen #2090f0 (was 1 pass in 4 on 09-27: the splash-occlusion bug #1 is gone with the binder, socket and property fixes); 0 oneway stalls | `docs/runs/2026-09-28-regression-c4x5-d5x3.csv` |

**The wall (phase 4):** the game server disconnects this device as an emulator ~80 s after the
join. The image is Google's SDK emulator build ("Unknown Android SDK built for arm64", goldfish/
ranchu props); hiding that from Roblox would be evading its security, which the owner's rule
forbids. The owner decides whether and how to proceed (a non-emulator system image is the honest
route; nothing here spoofs one).

| 2 launch | **Roblox starts on the real AOSP stack**: `pm install` 0 (libs extracted), `LauncherAliasMain` -> `ActivitySplash`, "Displayed ... +24-25 s", `libroblox.so` loaded by its class loader, the engine's own `I/Roblox [FLog::...]` lines, crashpad up. r1: its splash on the display, then its main thread died (below). r2 (after `4f600d3`): alive, its own **"Connection error -- Unable to contact server"** dialog: no network on this path yet | `work/overnight/r1-roblox-splash.png` (97.4% #f8f8f8, 2.0% #3058f8 = the Roblox logo), `work/overnight/r2-roblox-connection-error.png` (80.9% #606060 scrim + 16.6% #f8f8f8 dialog); logs `work/overnight/r1.log`, `r2.log` (untracked) |

Fixed on the way (phase 2), each with a test that fails without it:
- `4f600d3` binder: a transaction that cannot be made is `BR_FAILED_REPLY` to the sender (the
  command consumed, the ioctl 0), not an ioctl errno -- libbinder kept its out-buffer and every
  later call of the thread failed (`tests/binder_failed.rs`; old driver: -9). And a plain file's
  descriptor crosses host processes (the WebView's `variations_seed_new`), by
  `omni_platform::fs::path_of` (`remote::tests`).
- `617f943` ART's JIT code cache: a memfd's section was never executable, so every ART process
  (system_server, SystemUI, Roblox) ran without JIT (`tests/jit_cache_map.rs`: -17 before).

- `efd0e33` (merge `aosp-net`): TCP/UDP are real host sockets (`tests/host_sockets.rs`, 37 checks
  in a guest), `pselect6`, and the kernel answers `/dev/socket/dnsproxyd` in an app's host process
  (`tests/dns_proxy.rs`: a guest `getaddrinfo` through the real bionic gets the host resolver's
  addresses). Before it, every inet `connect` was ENETUNREACH.
- merge `binder-oneway` (7 commits, a test each that fails on the old driver): a freed oneway
  buffer hands the node's next oneway to the process, not the freeing thread; only free loopers
  take process work; undelivered work is freed and its sender told; nodes named by a transaction
  are held until the receiver frees the buffer (the system_server SIGSEGV of r1: a use-after-free
  in `Parcel::unflattenBinder`, symbolized to `RefBase::incStrongRequireStrong` on a freed object).
- `2832702` no modem: the image's RIL spun on its modem's vsock (198,217 log lines per 100 s; one
  session's log reached 2.4 GB and filled C:); it is declared and not started, as on a device
  without a radio. C4 logs 4 MB -> 1.2 MB. 3/3 C4 boots after both merges: ready in 70-84 s.

- `7f1bde0` binder: BC_FREE_BUFFER freed the memory before removing the buffer's record; a
  transaction delivered into the reused address replaced the record, and SurfaceFlinger's composer
  callback node wedged ("101,312 oneway calls wait on node 85 ... the one out: none found", boot
  stalled). 5/5 C4 boots after: 0 stall lines (before: 1 of 3 C4 + r5).
- `e8e3461` mremap: ART's CMC GC moves its 512 MiB space with MREMAP_DONTUNMAP; the byte copy
  committed all of it. Idle app host process 765 -> 263 MiB (guest 636 -> 135), system_server
  2,971 -> 2,486 MiB (`OMNI_MEM_TRACE`). Two sessions (r4, r6) had been stopped by Claude Code's
  low-memory reaper before it.
- `1792321` ftruncate of a mapped file (SQLite `-shm`): `SQLITE_IOERR_SHMOPEN` killed the
  contacts provider. `b1a8f08` kill(2) of an app in its own host process ("refused to die":
  `am force-stop` never stopped Roblox, so the planted cookie was never read); fallocate(2).

### Later fixes (after r12)

- `9724db0` abstract unix sockets across host processes (the WebView zygote; ActivityManager's
  2,402 connect retries stalled system_server; `tests/xsocket.rs`, ENOENT before).
- `b1a8f08` kill(2) of an app in its own host process; fallocate(2).
- `1792321` ftruncate of a mapped file (SQLite `-shm`).
- `fa11ddc` property changes published in order (r14: system_server killed by its Watchdog after
  vold waited forever on a property; not reproduced by `tests/props_order.rs`, argued in props.rs).
- `5dd6ba1` the test device boots in the account's locale (tr-TR) so the app's locale change does
  not relaunch its game activity; `5bfcc91` 24 idle image apps disabled (memory: the reaper).
- `d36233b` no telephony feature (the device has no modem): com.android.phone restarted 47 times in
  r12 ("failed to complete startup"); one D5 run after: 0 deaths, 0 IRadioModem waits (was 66).

### r11/r12 (2026-09-28 02:28-03:00 guest time): signed-in Home on the display, the game joined

- **The engine draws on the host display**: `work/overnight/r12-home-signed-in.png` -- Roblox's
  signed-in Home (HeZmI_ImYu1080, Turkish UI: "Ana Sayfa", "Arkadaşlar (10)", game tiles), 12,189
  distinct colours, 66.5% #101010. The black window of r8 was system_server stalled by the WebView
  zygote's socket (`9724db0`).
- **The game joined**: r12 `GameJoinLoadTime ... join_time:1.856, referral_page:DeepLink,
  placeid:8737899170, userid:5457009831`; `! Joining game ... place 8737899170`; RakNet to
  128.116.5.33 "Handshake complete", "Connection accepted", "Replicator created"; PS99's own client
  scripts ran (`✅ CLIENT | _L took 1818ms to initialize!`, `[MiningFrontend] inMine=false`, server
  `RobloxGitHash: ef44663a...`). Markers: `work/overnight/r12-join-markers.txt`.
- **No `onGameLoaded`**: 2.3 s into the game the app's activity was relaunched (the app applied the
  account's locale tr_tr on an en-US device; ActivityNativeMain's configChanges 0xfb0 has no
  locale), its Java game session ended ("Ending game session with place ID 8737899170") and the
  UI went back to Home while the engine stayed in the server. The test device now boots in the
  account's locale (`persist.sys.locale=tr-TR`, as the owner's phone) and joins 45 s after sign-in.

### Phase 2/3 state (r8, 2026-09-28 01:31 guest time)

- With the network: `ActivityNativeMain` displayed (+6.6 s); the engine's **own Vulkan** on the RTX
  4060 through `vulkan.omni.so` ("Vulkan Device: NVIDIA GeForce RTX 4060", "swapchain images 3
  present mode 2 format 37 size 1280x720", 2,919 shaders loaded); `SingleSurfaceApp` reaches
  `stage:LuaApp`; HTTPS answered by Roblox (401 "Authentication required to access Asset" while
  signed out). **But the app's surface is black** (r8 shots 0008-0068: 92.2% #000000 = the whole
  window; status and task bars drawn) and presented frames nearly stop once the native activity
  shows (~500 in total, then ~1/min): the engine does not present. Under investigation (r9: the
  app's syscalls traced).
- **`--cookie`: signed in (r10, 02:13 guest time).** Planted into the app's own Chromium `Cookies`
  store, `am force-stop` now ends the app (`[zygote] pid 172000: signal 9`), the fresh process
  (pid 196000) calls `initializeLuaAppWithLoggedInUser` and the engine logs
  `onDataModelNotification() type:DID_LOG_IN data:{"username":"HeZmI_ImYu1080",...,
  "countryCode":"TR","userId":5457009831}`. After it: **0** HTTP 401/403 and no logout line in the
  log. Evidence: `work/overnight/r10-signin-markers.txt`, `work/overnight/r10.log` (untracked).
- `--place` (r10): the deep link `roblox://experiences/start?placeId=8737899170` was delivered to
  `ActivityProtocolLaunch` (`am start` 0), but the app was killed ~60 s later: ANR "Input
  dispatching timed out" -- its main thread waited 50 s in a binder call on a system_server stalled
  by ActivityManager retrying the WebView zygote's socket (2,402 x "Got error connecting to zygote").
  Fixed below the framework in `9724db0` (abstract unix sockets across host processes); r11 re-runs
  it.

Cause (phase 1): every guest process asks for the same low range; system_server holds it,
reserved *around* the host's pieces there, host threads' 1 MiB stacks among them. When such a
thread exited, its stack was the only free piece, and the next process's space "succeeded" with
1 MiB free. `process::reserve_space` now refuses a low space less than half free.

Markers on this path: `DID_LOG_IN`, `onGameLoaded: placeId:`, `submitStartGameTask` are the
engine's own (logcat tag `Roblox`). `JOIN: ... returned 1` and `FRAMES:` are printed only by the
HLE gate (`omni-android/tests/gameactivity.rs`) and cannot appear here; their stand-ins are the
engine's `onGameLoaded` / `NativeDM ... placeId` and the runner's `[display] N frames presented`.

Current as of **2026-09-27**, branch `unified`. This file is the state and the next steps only.
Design is in `ARCHITECTURE.md`, reasons in `DECISIONS.md`, capabilities in `STATUS.md`, per-host
detail in `ports/<os>.md`, the current goal in `briefs/goal-performance.md`. Read
`VERIFICATION.md` before writing a test you will rely on; it also holds the Global Constraints
that code comments cite by number.

## Source, branches, machines

- **`unified` is the source.** It contains every other line of work (`perf-windows`,
  `port-macos`, `port-linux`, `input-kbm`, `perf-world`, `main`). The one unmerged branch with
  pending work is `arm64-clear-audit` (patch 0023, see "Open").
- **`perf-windows` is superseded, not a merge target.** Its two commits since (`799aca4`,
  `c566be4`, 2026-09-26; `unified..perf-windows` = 2, `perf-windows..unified` = 306) add nothing
  `unified` lacks. `c566be4` (a mapped file shortened as Linux does, by a logical end of file)
  duplicates `struct Logical` in `omni-platform`'s `fs/windows.rs` and bionic's
  `a_file_a_shared_mapping_holds_is_reopened_truncating_as_linux_does` (`memProfStorage`).
  `799aca4` (raw `close`/`read`/`mprotect`) is superseded by `sysroute::ROUTES` (57, 63, 226
  among them, each with a per-argument kernel-to-import map), which `service_raw_syscall`
  dispatches through `call_routed` on the exit path: the two defects of its draft (arguments
  read as zero; a re-entrant `mprotect` unreachable inline) cannot arise here.
- `main` is behind. **Never merge to `main` or push to GitHub (`origin`) without the owner.**
- Remotes `mac` and `linux` are the other machines' checkouts. The `mac` remote URL still says
  `192.168.0.24`; the Mac was moved to **`192.168.0.37`** (macOS 27.0) on 2026-09-25.

| host | checkout | notes |
|---|---|---|
| Windows (this PC, RTX 4060, 24 threads, 31.8 GB) | `C:\Users\berat\Desktop\Omni Apps\omnidroid-unified` | build with `OMNIDROID_DYNARMIC_BUILD_DIR=C:\od-unified` (MAX_PATH). Other `Omni Apps\omnidroid*` worktrees and `C:\od*` build dirs belong to other work. |
| macOS (Apple M1, 16 GB) | `~/Desktop/omnidroid-unified` | non-interactive ssh: `. ~/.cargo/env` first. `screencapture`/`osascript` do not work over ssh. |
| Linux (i5-4460, **7 GB**, Quadro 4000 = Fermi, no Vulkan) | `~/Desktop/Omni Apps/omnidroid` (2026-09-29; was `~/Desktop/omnidroid-unified`), dynarmic `~/od-dynarmic-linux-port`, sysroot `~/aosp-sysroot/aosp-35` | one build or one app at a time; GLES through nouveau NVC0; `DISPLAY=:0`, start with `setsid nohup`. Never start an Xvfb that opens the nouveau node (it wedged the GPU once). |

Keep the three at one commit: commit on Windows, `git bundle create <f> <old>..unified`, `scp`,
`git pull --ff-only <f> unified` on the others. Live runs on one host at a time; builds may run in
parallel. Remove a finished agent's worktree `target/` (a full disk stopped a build on 09-25).

## The APK

Since 2026-09-26 the fixture is the **stock** `Roblox-2.738.1397.apk` (identity in `STATUS.md`,
contents in `research/apk-analysis.md`). The builds used before it were modified by third parties
and are gone from this checkout: the old 2.738.1397 (trojanised `libzstd-jni`, injected
`classes4.dex`) and 2.739.691 (another executor in `libzstd-jni`). Copies of them still exist in
other worktrees (`omnidroid-play`, `C:\odw\*`, `.claude/worktrees/*`): do not use them.

- The APK is chosen at run time: `--apk`, else `OMNI_APK`, else the APK in the repo root with
  the highest `versionCode` (`omni_apk::choose_apk`; `omnidroid which` prints the choice).
- Only `lib/arm64-v8a` is loaded; the APK's `armeabi-v7a` and `x86_64` sets are ignored by design.
- **Anything decoded on 2.739.691** (a different `libroblox.so`, 3,610 initializers) carries that
  build's link addresses. Code that locates things by decoding the engine at run time (e.g.
  `jni::cursor`'s lock word) is version-independent; comments and docs that quote 2.739.691
  addresses are not valid for the stock binary until re-measured.

## Running

```text
cargo build --release -p omnidroid
cargo test -p omni-android --release --test gameactivity --no-run
target/release/omnidroid play [--cookie <file|name>] [--place <id>] [--minutes N] [--fresh] [--phone]
target/release/omnidroid login            # sign in in Chromium once; keeps the cookie by name
powershell -File tools\play.ps1 [-Cookie ..] [-Place ..] [-Minutes N] [-Fresh] [-Phone]
```

`play` runs the gate test `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`
as the session; the gate failing because a guest thread died is a real defect. Storage is per
account (`<app-data>/../accounts/<name>`); a session's cookies are kept by the app's own cookie
store, as on a phone.

- **End every run cleanly** (the window's X, or `--minutes`). A killed run is judged a crash at
  the next launch of that storage; after one, use `--fresh` or a new storage.
- **Never build on Windows while the owner plays**: a build exhausted the commit limit once and
  killed two guest threads.
- **Network:** Roblox is ISP-blocked; each host uses a bypass (WARP / SplitWire / VPN). The
  accounts are Turkish and Roblox ends a session whose cookie arrives through another country's
  exit: check `loc=TR` (`https://www.cloudflare.com/cdn-cgi/trace`) first. GoodbyeDPI breaks
  UDP/QUIC and teleports. A `TlsVerificationFail` / `195.175.254.2` answer means no bypass is on.
- **Credentials:** never print, log or copy cookie values or passwords; never work around
  Roblox security (captchas, the country check). A dead cookie: ask the owner.
- Measure in **Pet Simulator 99, place 8737899170** (joined directly, no teleport).

Log markers: `DID_LOG_IN`; `JOIN: ... returned 1`; `submitStartGameTask`; `onGameLoaded:
placeId:...` (in-world timing starts after it plus ~2 min settle); `FRAMES: +Ns ..., T presents
(+X in the last 5s)` (fps = X/5); `GUEST THREAD DIED` (always a bug); the close's
`SessionHistory "IAB"`/`"IB"` (clean).

### Switches

Read once at start; each announces itself in the log.

| switch | effect |
|---|---|
| `OMNI_KEYBOARD_MOUSE=1` | host keyboard and mouse as a device's (play's default; `--phone` = touch) |
| `OMNI_PERF=<s>`, `OMNI_PERF_SAMPLE=0`, `OMNI_PERF_WAITS`, `OMNI_PERF_DUMP` | per-thread PERF blocks every s seconds (jit/monitor/dispatch/handler shares, translation); sampler off; waits; symbol dump |
| `OMNI_MEM_REPORT=1` / `=<s>,..` / `=every:<s>` | `MEMREPORT`: memory by owner, guest and host (Windows, Linux; macOS guest side only) |
| `OMNI_FPS_CAP=<fps>` | pace frames from underneath (every frame still presented) |
| `OMNI_GRAPHICS_QUALITY=1..10` | the game's own saved quality (edits an existing `GlobalBasicSettings_13.xml`) |
| `OMNI_AUDIO=off` | no AAudio; FMOD's NOSOUND path |
| `OMNI_GUEST_MEMORY_MB`, `OMNI_GUEST_CPUS` | the device's RAM (D36) and CPU count (D37; default ≤ 8) |
| `OMNI_LOOPER_IDLE_US` | the game loop's idle wait (default 1000) |
| `OMNI_JIT_SHARED_CACHE=0/1`, `..._MB`, `..._LIVE_MB`, `..._REGION_MB` | D38 shared translation cache (default on for x64, off on arm64) and its sizes |
| `OMNI_JIT_CACHE_MB`, `OMNI_JIT_EXCLUSIVE_MONITOR=global`, `OMNI_JIT_OPTIMIZATIONS` | per-thread cache size; the old monitor (D31); dynarmic optimization mask |
| `OMNI_PAUSE_IN_BACKGROUND=1`, `OMNI_FOLLOW_FOCUS=1` | Android's pause-in-background (default: keep playing, as desktop Roblox) |
| `OMNI_WINDOW_SIZE=<w>x<h>` | initial window size; on the real-AOSP path, the display's size at boot |
| `OMNI_WINDOW=1`, `OMNI_WINDOW_CONTROL=<file>` | real-AOSP path: the display live in a resizable host window (`display_window`); the file takes `size`, `key`, `click`, `point`, `move` (captured motion), `wheel`, `flood <hz> <s>`, `chrome show|hide` lines; `OMNI_WINDOW_INPUT=0`: no keyboard or mouse; `OMNI_POINTER_HZ` (default 125), `OMNI_HOST_CURSOR=hide`. `r_roblox` sets `force_resizable_activities` by default (`OMNI_R_RESIZABLE=0`: not) |
| `OMNI_APP_ONLY=0`, `OMNI_R_KIOSK=0` | real-AOSP path: present the whole display, bars and taskbar included (default: only the app); run the app on a device with SystemUI and the launcher (default: the kiosk device, without them, from the first boot). `OMNI_COMPOSER_TRACE=layers` lists each frame's layers |
| `OMNI_POLL_STATS=<s>`, `OMNI_CODE_TRIM=0` | real-AOSP path: wake-up counters per host process; no translation trimming |
| `OMNI_DEVICE_APPS=full\|lean\|lean-hw\|kiosk`, `persist.omni.cached_processes=<n>\|off` | real-AOSP path: what the device leaves out of its image (default lean; `device::LEAVES_OUT`); ActivityManager's cached-process limit (default 0, `omni_lean.sh`) |
| `OMNI_SLOW_APP=<process>` (`OMNI_SLOW_APP_MS`), `OMNI_SLOW_SYSCALL_MS`, `OMNI_GPU_STATS=<s>`, `OMNI_SHM_VIEW=0` | real-AOSP path diagnostics: slow system calls with stacks; per-Vulkan-command calls and host time; graphics buffers through file I/O instead of a view |
| `OMNI_JOIN_PLACE`, `OMNI_JOIN_DELAY`, `OMNI_DEEPLINK` | join a place (the app's own join URL) |
| `OMNI_GUEST_ENV=K=V,..` | extra guest environment (e.g. `MIMALLOC_PURGE_DELAY`) |
| `OMNI_FILE_TRACE`, `OMNI_WAIT_TRACE`, `OMNI_PROFILE`, `OMNI_IMPORT_CENSUS=off`, `OMNI_GLES_TIMING` | diagnostics |
| `OMNI_CLIENT_APP_SETTINGS=<json>` | the engine's own ClientAppSettings (measurement only) |
| gate/test: `OMNI_GFX_WINDOW_TESTS=1`, `OMNI_M6_ROWS_21_22=1`, `OMNI_SESSION_SECONDS`, `OMNI_DATA_DIR`, `OMNI_LATE_{TAP,TEXT,INPUT,DRAG,KEYS,WHEEL}`, `OMNI_RESIZE_PROBE`, `OMNI_INPUT_PROBE`, `OMNI_INJECT_DEATH` | set by `play`, or synthetic stimuli for unattended runs |

## The real-AOSP path (`omni-linux`, sub-projects C and D) -- the current goal

Goal: an installed APK's launcher Activity starts through the real AOSP stack and renders to a
screenshot. Milestones D2 -> D3 (a: GPU, b: composer + SurfaceFlinger, c: system_server to
SystemReady) -> C4 -> C5 -> D4 -> D5; specs in `docs/superpowers/specs/2026-09-27-*`, plans in
`docs/superpowers/plans/`. Done: D2 (gralloc 5), D3a (guest Vulkan + ANGLE on the host GPU) on
Windows and Linux (see STATUS).

- **D3b done on Windows** (`tests/d3b_display.rs`, the bootanimation screenshot). Linux open:
  RenderEngine faults in ANGLE on lavapipe (STATUS).
- **C4 now (2026-09-27)**: on a fresh instance, system_server boots through PackageManager,
  SettingsProvider, WindowManager, input, Bluetooth and NetworkManagement (netd up) to
  `StartNetworkStatsService`; the last run then aborted in ClatCoordinator on bpffs pin modes and
  contexts (fixed since in `1f21f27..`: bpffs keeps modes/owners, genfs contexts from the image's
  policy). What the boot needed, all below the framework: init's `wait_for_prop`, `setprop`,
  `restart`, `init_user0` (vdc), `perform_apex_config` (linkerconfig), `load_bpf_programs`, APEX
  versioned `.rc`, `socket` (init sockets); the kernel's xattrs/restorecon, bind mounts, fork/
  exec/wait (vfork child in the parent's memory, the parent's private memory snapshot-restored),
  ownership (persisted per instance), record locks, flock, capabilities, set*id, pwrite, sendfile,
  eBPF (maps, programs, bpffs, attach/query), xtables, netlink, Unix server sockets, the boot id
  and `/dev/ashmem<boot_id>`. The device lists no sensor sub-HAL (`device/vendor/etc/sensors/
  hals.conf`). system_server is started directly: `--caps` (the zygote's set) and `--setprop
  dalvik.vm.profilesystemserver=true` (standalone jars' class loaders on first use).
  Later the same day (`5043072..f38e95e`) it passed NetworkStats, Connectivity, Audio and
  SoundTrigger to BiometricService (gatekeeperd: init's `late_start` class, now started). Filled
  on the way, each below the framework: the image's owners/modes/labels/capabilities
  (`sysroot.meta`, `tools/make_sysroot.py --meta`), inet `bind`, MAP_SHARED file mappings
  (SQLite WAL), peer credentials (SO_PEERCRED, SCM_CREDENTIALS, init's `+passcred`), `*at`
  dirfd as an int (AT_FDCWD), PI futexes (audioserver), the bootloader's `ro.boot.*` and the
  vendor build.prop (KeyMint), init restarting services (`onrestart`, `ctl.stop`), seccomp
  filters (a cBPF interpreter; minijail in the media daemons), 512 threads per process,
  tracefs (tracing off), and fork: private file views snapshotted, descriptors closed at exit,
  pids allocated onward (mksh pipelines and `$(...)`). A killed process no longer waits for
  threads a halt cannot reach (5 s). Gates: `tests/c4_system_server.rs`,
  `tests/c5_app_launch.rs` (`--ignored`, minutes; `tests/common/boot.rs`).
- **Run the boot in release** (`cargo test --release ... -- --ignored`, or a release
  `omni-linux-run`): the debug build's syscall/binder/memory paths made system_server ~5x slower
  (PackageManagerService 61 s debug, 12.7 s release; system_server start to "System now ready"
  41 s release), and its watchdog (60 s on the main thread) killed it after systemReady in debug.
  JIT on (`dalvik.vm.usejit=true`) changed nothing measurable. `OMNI_LOG_TIME=1` stamps log lines;
  `OMNI_FORK_TRACE=1` times forks (system_server's: 43 MiB kept, ~0.7 s frozen).
- **C5 in progress** (spec `docs/superpowers/specs/2026-09-27-c5-app-launch-design.md`): binder
  across host processes works (`tests/c3_remote_binder.rs`; `--binder-server`, `--pid`); the
  zygote responder (`--zygote`, `crate::zygote`) launches an app's `app_process64 ...
  android.app.ActivityThread seq=<n>` in its own host process. Probe: scratchpad `run_c5.sh`
  (probe APK `tests/fixtures/probe-app/probe.apk` in `/data/app`, `--then` runs `am start`).
- Probing: `OMNI_INIT_TRACE=1` (init's commands), `OMNI_TRACE_SERVICE=<name>` (one service's
  syscalls), `OMNI_FS_TRACE=1` (unlink/rename), `OMNI_REMOTE_TRACE=1` (cross-host binder frames).
  A full-disk C: shows as ENOMEM in every spawn (page file cannot grow): prune target/debug/deps.
- system_server command = `app_process64 -Xgc:CMC -Xhidden-api-policy:disabled /system/bin
  com.android.server.SystemServer` as uid 1000 with CLASSPATH=$SYSTEMSERVERCLASSPATH and the
  derive_classpath environment, `omni-linux-run --init early_hal,core,hal,main --hal gralloc`
  (init starts ~58 services incl. apexd, which system_server needs). It stops at
  StartDisplayManager today (waits for SurfaceFlinger).
- Probing from Git Bash: `export MSYS_NO_PATHCONV=1` or guest paths are rewritten.
- Diagnostics: `OMNI_GPU_TRACE=1` (every forwarded Vulkan command and its answer).
- **Network (2026-09-28, `aosp-net`)**: TCP/UDP sockets are host sockets (`crate::hostnet`, via
  `omni_platform::net`, policy `hostnet::set_policy`, unrestricted by default); the guest shares
  the host's ports. One watcher thread per host process turns host readiness into `crate::poll`
  wakeups (`tests/host_sockets.rs`). In a host process with no netd (an app's), the kernel answers
  `/dev/socket/dnsproxyd` itself (`crate::dnsproxy`: getaddrinfo, gethostby*, resnsend from the
  host resolver); where netd bound it, netd answers. `OMNI_NET_TRACE=1` prints host socket and
  lookup failures. Open: SCM_RIGHTS is not passed, so in the system's host process a connect by
  a process whose libnetd_client reaches netd's fwmarkd gets netd's error for the missing fd
  (apps are unaffected: fwmarkd is absent there and a failed fwmarkd connect is "no error").
- Known gaps: AHB mirrors are linear images (input-attachment usage and other-format views are
  dropped when the host refuses them linear); no sync_file fds (fences are -1: synchronous);
  `vkDestroyCommandPool` leaves guest command-buffer wrappers behind; CLOCK_MONOTONIC is shared per
  host process (not across host processes).

### D4/D5: the app on the display (2026-09-28, Windows)

`tests/d5_app_on_display.rs` (release, `--ignored`): boot, set the device up (provisioned, awake,
animations off), `pm install` the probe, `am start`, then require the probe's `frame committed`,
ActivityTaskManager's `Displayed`, and the probe's blue (0xff2196f3) at the centre of the host
display's screenshot. Passed once (161 s); 3 of 4 runs failed -- the flakiness is the open work:

1. **Starting window left on top.** WindowManager removes the splash window (its input channel is
   disposed) but its layer stays under a `window_animation` leash at z max; the removal transition
   never completes (seen with animations on and off). `BLASTSyncEngine: Sync group 0 timed-out
   because not ready` and `SurfaceSyncGroup ... Failed to receive transaction ready` precede it.
2. **Guest space exhausted early in boot** (~1 run in 3): right after init stops odsign, every
   `map_anonymous` fails with ~225 KiB free of 64 GiB. `mm.rs` now lists what holds the space at the
   first refusal (by label and call site) -- not yet caught with it.
3. **Live handle refcounts.** BC_RELEASE/BC_DECREFS are still no-ops (a live process's handles last
   as long as it does). Counting them deleted handles in use because a write stops at its first
   failed command, dropping later BC_ACQUIREs; the kernel goes on past a failed transaction. Fix
   that first, then restore the counting (commit `5e62d85` has it, with `tests/binder_release.rs`).
4. `probe.apk` logs `frame committed` (ViewTreeObserver.registerFrameCommitCallback).

Diagnostics added: `OMNI_COMPOSER_TRACE=2` (each presented frame: slot, centre and corner pixel),
`[display] N frames presented` with `OMNI_SCREENSHOT`, `[remote]` binder ioctl failures and
descriptors that cannot cross, `[binder] N oneway calls wait on node` (every 64 queued),
`[binder] pid P closed its driver: N objects released`, `OMNI_TRACE_SERVICE=<service>` for one
init service's syscalls. Crossed gralloc files stay in `%TEMP%\omni-shm-*` (clean them between
sessions).

## Where it stands

- The whole startup contract (`research/jni-surface.md` §8, 26 steps) runs on the real engine:
  sign-in (Quick Sign-in, password, or a kept cookie), Home, joining a game, the world rendered
  and played with keyboard and mouse, audio, the web view, a device-style close and relaunch.
- **2026-09-26, stock APK**, logged out, `--fresh`, 4 minutes, Windows: all 3,594 initializers,
  Landing at ~+10 s, 0 guest threads lost, clean close (`IAB`), gate passed. Not yet run in a
  world or signed in on the stock APK.
- In-world figures per host (all on the modified 2.739.691 build, 2026-09-25) are in `STATUS.md`.
  Windows sits at the engine's own 60 Hz pacing (decoded: a phone on these flags does the same);
  Linux is limited by its Fermi GPU, not the GLES layer.
- **Multi-instance.** The owner's two products: (A) 3-4 instances, high quality; (B) 30-35
  instances at lowest quality, capped. A is in reach. A B instance (`OMNI_FPS_CAP=10
  OMNI_GRAPHICS_QUALITY=1 OMNI_AUDIO=off OMNI_GUEST_MEMORY_MB=3072`, minimised) measured
  **~2.5 GiB and ~0.6 cores** (H2); the rest is mostly the engine's own heap (1.25-1.6 GiB of live
  data). Remaining levers, unmeasured: engine read-only file `mmap`s as shared views, a 16 KiB
  commit granule for the heap, `onTrimMemory` after the join, a persistent translation cache.

## Test state

2026-09-26, Windows, stock APK: `cargo test --workspace --release` **2,409 passed, 0 failed, 110
ignored** (233 binaries). Switching to the stock APK re-pinned `omni-apk/tests/real_apk.rs` (the
container), `omni-elf/tests/all_libraries.rs` and `loader_hostile.rs` (the genuine `libzstd-jni`:
641-symbol import union, `DT_HASH` only, BTI/PAC, 16 KiB-aligned), `tools/texture_census.py` (entry
count; the texture set is unchanged) and one dex decode in `tests/gameactivity.rs` (an obfuscated
name). Not run on the Mac or Linux since the switch.

- `dynarmic-sys --test shared_cache`'s `threads_stay_right_while_another_rewrites_and_invalidates_
  their_code` failed once in a full run and passed 5/5 alone: it waits up to 60 s for a region to
  retire and takes 40-50 s unloaded, so a busy host can hit the cap. A timing-bound test to fix.
- 44 mutation rows no longer match their pattern exactly once (34 in `tools/mutate.py`, 6 in
  `tools/lnx_rows`, 4 in `mutate_shim.py`), all already stale at `374df41`; re-anchor them before
  the next full-table run.

## Open, in order

0. **Real-AOSP, next levers** (HANDOFF "PLAYABLE INPUT, LIGHTER, FASTER": 57.1 fps, 5.11 GB, input
   in milliseconds): the render thread is the limit -- translated bionic (memcpy/malloc natively,
   as the old path), `vkCmd*` batching; memory -- the system host's 65 guest processes (one
   translation region each), the idle app processes (~100-200 MB each), Roblox's engine heap.
   The composer's present fence (-1: SurfaceFlinger complains every frame). Done since the old
   item 0: async release, vDSO, POSIX timers, keyed wake-ups.
1. **Re-validate on the stock APK in a world** (needs the owner's sign-in): a 30-minute PS99 run
   per host; watch for Roblox's "missing or corrupted files" kick (seen only on the modified
   builds); re-measure what was decoded on 2.739.691 wherever it is still used.
2. **macOS freezes** (m7, m9, m11): a control transfer into stale translated code after a
   mid-run cache clear on the arm64 JIT. Patch 0023 (`arm64-clear-audit`) awaits verification:
   the unpatched build must fail its new test, the `mac-cpu-C` rows, the arm64 suites. m12 with
   `OMNI_JIT_EXCLUSIVE_MONITOR=global` ran 30 min clean. Also owed on the Mac: the RSB-off run.
3. **Intermittent heap corruption on Windows** ("-1 pointer": OpenSSL `impls`, a `shared_ptr`
   control block, w23's flag-registry node): ~2 in 16 runs before the `MADV_FREE` fix
   (`e6b7769`); not seen since, not proven gone.
4. **Live checks pending**: the window-change freeze fix (w26), the JNI-audit fixes (w31's "unlock
   chat"), D38 amendment 3 on Windows at 256 MiB (w36 confirmed the default). Open on the menus
   path: Linking `openURL` ("Continue" in the age-check modal does nothing), voice's
   `WebRtcAudioManager`.
5. **A `poll` over sockets parks 60 s per slice and can hold teardown** (was in progress on
   09-25; unverified whether fixed).
6. **`/proc/<own pid>/cmdline` is not answered** (measured on `perf-windows`, 2026-09-26; not
   fixed). With the logical end of file in place, the engine's "Evaluating deferred inferred
   crashes" opens it; nothing answers that path (`bionic/procfs.rs` has `meminfo`,
   `self/statm`, `self/maps`), so the engine takes its own `memProfStorage<pid>.json` for a dead
   session's record -- re-opens it `O_TRUNC`, `fallocate`s, unlinks it -- and the gate's close
   assertion failed with `SessionHistory None`. The windowed gate on `unified` is green today,
   so the path is latent, not absent. The answer is **not** a `/proc` file invented to satisfy
   the check: measure first what the engine reads there and what a device answers.
7. Multi-instance B: the unmeasured levers above; which game and RAM the B machine has is the
   owner's answer.
8. Re-anchor the 44 stale mutation rows (above), then run the whole table once; it has never run
   whole in one pass (VERIFICATION entry 8).

## Working rules that have paid for themselves

- `tools/mutate.py` mutates the tree in place: it is exclusive (no build, commit or second run
  meanwhile). Stage explicit paths, never `-A`; commit in small pieces.
- Parallel agents own disjoint crates and their own worktree + `target` + dynarmic build dir.
- When vendored dynarmic C++ changes, touch `crates/dynarmic-sys/vendor/PIN.txt` before building.
- A measured figure lives in one doc with its n; everything else links to it.
- The five-target rule: never claim a platform works that has not been run there; a new platform
  primitive gets honest `unsupported` arms elsewhere (see `ports/`).
