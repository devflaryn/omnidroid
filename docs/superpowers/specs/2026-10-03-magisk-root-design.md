# omnidroid Rooted Device (engine-native, Magisk-compatible) — Design

Date: 2026-10-03
Status: draft (awaiting owner review)
Branch: `feat/magisk-root` (from `perf/warm-join` @1c718e6, so the warm `--cookie --place` session is there)

## Goal

Launch omnidroid as a **rooted device** that behaves like a phone running Magisk, with Magisk
modules picked per launch from a catalog. The owner's uses:

- **Hooking / RE**: Zygisk modules and Frida (gadget) inside live apps.
- **Testing APKs that need root**: `su` works for apps and the shell.
- **Roblox rooted**: the stock Roblox APK still signs in and joins. Root is hidden from it
  (DenyList, Shamiko behavior), and so is the emulator (emulator hiding).

Success, end to end (after R3): one launch with `--module zygisk-frida,emu-hide,shamiko` meets all of these:

- The stock Roblox APK joins place 8737899170 and is not kicked by `AndroidRootedKick` or `AndroidEmulatorKick`.
- In the same device, another app runs a Zygisk module and accepts a `frida` connection.
- `adb`-style shell `su -c id` prints `uid=0`.

## Why engine-native, not real Magisk

omnidroid does not boot Android's kernel, `/init` or zygote. Rust plays all three:

- `init.rs` is a host-side init.
- `zygote.rs` answers `/dev/socket/zygote` by starting each app as its own host process
  through `WrapperInit`.

There is no ramdisk to patch, no tmpfs, no mount namespaces, no `/proc/self/mountinfo`, and
binds over image paths are refused (`mount.rs:51`). Real Magisk works by replacing `/init`,
magic-mounting over `/system` and loading into zygote, so it has nothing to attach to here.

The engine already owns the file view, the props, process start and credentials. So it provides
what Magisk provides, reading **standard Magisk module zips**. Some things do not carry over:

- **The Magisk app UI and `magiskd`.** Neither exists. The picker is omnidroid's CLI and MCP.
- **The real Shamiko binary.** It is closed-source and hooks Magisk internals. Its *behavior* is
  provided by the engine (R2), and selecting the `shamiko` module turns that behavior on.
- **`frida-server`.** It needs guest `ptrace`, which was dropped in the RE-workbench spec. Frida
  runs as a **gadget** loaded by a Zygisk module (R2).

## Decomposition

There are three sub-projects. Each gets its own implementation plan and is merged to the branch
only when its tests pass.

| | Delivers | Proven by |
|---|---|---|
| **R1** | Root profile, module catalog, module loader, su/resetprop, CLI/MCP picker, saved-device key | `su -c id` → uid 0; an overlay+script community module works; nothing changes when root is off |
| **R2** | Zygisk host, DenyList, Shamiko mode, built-in `zygisk-frida` module | A real Zygisk module's callbacks run; `frida` attaches to a gadget app; a DenyList app sees no root |
| **R3** | Built-in `emu-hide` module (engine spoofing) | Stock Roblox APK joins with root and is not kicked |

This spec details **R1** and fixes the interfaces R2 and R3 build on. R2 and R3 each get a short
spec of their own before their plans.

---

## R1 — Root core and module picker

### 1. Root profile

A launch's root configuration is one file, `<instance>/data/adb/omni/profile`. It is written on
the host by the launcher before the device boots. Its format is plain `key=value` lines, the way
`build.prop` is:

```
root=1
magisk=<pinned Magisk version code, e.g. 29000>
module=<id>          # one line per selected module, in order
su=all               # or: su=<package>,<package>,...  (uid 0 / 2000 always allowed)
denylist=<package>   # R2; parsed and kept in R1
shamiko=0|1|whitelist  # R2; parsed and kept in R1
```

- **When the file is absent, the device is not rooted.** Nothing below takes effect: no root
  layer, no stubs, and the root syscall returns `ENOSYS`.
- **Every host process of the instance reads it at start.** That covers `omni-linux-run`, the
  app host processes started by `zygote.rs`, and spares. It lives in Rust as
  `omni_linux::root::Profile`, which handles parse/serialize and exposes `is_rooted()`,
  `su_allowed(uid)` and `hidden(process)` (the last returns `false` in R1, so R2 has a stable
  hook).
- **It is part of the saved-device key** (§7).

### 2. Module catalog (host)

A module is a standard Magisk module zip containing `module.prop` and any of `system/`,
`system.prop`, `post-fs-data.sh`, `service.sh`, `customize.sh`, `uninstall.sh`, `sepolicy.rule`
and `zygisk/`. The catalog is searched in this order:

1. `$OMNI_MODULES` (if set), else `%USERPROFILE%\.omnidroid\modules\` (`~/.omnidroid/modules/` on
   Mac/Linux). The owner drops zips here.
2. `modules/` in the repo: built-in modules in the same zip-source layout, one folder per module,
   zipped at launch. These are `emu-hide` (R3), `zygisk-frida` (R2) and `shamiko` (R2). The
   `shamiko` module is a marker module whose `module.prop` turns Shamiko mode on.

A module's **id** is `module.prop`'s `id=`. Two catalog entries with the same id are an error
that names both files.

`omnidroid modules` lists `id`, `name`, `version`, source (user or built-in) and its features
(`system`, `scripts`, `zygisk`, `props`). `omnidroid modules add <zip>` copies a zip into the
user folder after checking that `module.prop` parses.

### 3. Magisk pin

The installer environment is taken from **one pinned official Magisk release APK**:

- `assets/util_functions.sh` (`install_module`, `set_perm`, `ui_print`, …).
- `lib/arm64-v8a/libbusybox.so`, which becomes `busybox`.

`tools/fetch_magisk.py` downloads the APK from Magisk's GitHub releases and checks its sha256
against `tools/magisk.pin` (version, version code, URL, sha256). It then extracts those files to
`sysroot/magisk-<ver>/`, which is not committed (GPL binaries, same policy as `sysroot/aosp-35`).
The version is the latest stable release when R1 is built, and it is recorded in the pin.

### 4. The root tools: `magisk` multi-call binary and the root syscall

There is one small arm64 C program, `device/src/root/magisk.c`. It is built with NDK r28c like
the other device code and committed with its sha in `device/SHA256SUMS`. Its applets are chosen
by `argv[0]` or by `magisk <applet>`:

| Applet | Does |
|---|---|
| `su` | Parses `su [-] [-l] [-p] [-mm\|--mount-master] [-c cmd...] [uid]`. Asks the engine to elevate, then `execv("/system/bin/sh", ...)`, with `-c` → `sh -c`. |
| `resetprop` | `resetprop name value`, `-n`, `--delete`, `--file f`, `name` (get), no args (list). Sets props even when they are `ro.*`. |
| `magisk` | `-v` → `<ver>:MAGISK:R`, `-V` → version code, `--path` → `/debug_ramdisk`, `-c` → same as `-v`, `--sqlite`/`--denylist` → R2 (R1 prints "unsupported" and exits 1). |

The binary talks to the engine through **one engine-private syscall**, `omni_root(op, a0, a1, a2)`,
on an unused arm64 number fixed in `syscall.rs` and documented there. Its ops:

- **`ELEVATE(target_uid)`.** The engine checks `profile.su_allowed(caller uid)`. On success it
  sets uid/gid/euid/egid (and the supplementary group list for the target) and caps
  (`ALL_CAPS` for 0), the way `SysState::become_user` and `set_caps` already do. On refusal it
  returns `EACCES`, and `su` prints `Permission denied` and exits 1.
- **`RESETPROP(name, value, flags)`.** This adds a `PropertyService` method that writes the live
  prop area directly, bypassing the `ro.*` refusal at `props.rs:519`. It also supports delete.
  Only uid 0 may call it.
- **`STATUS`.** Returns the profile's root state and Magisk version code. This is how the stub
  answers `magisk -v` without reading files.

When there is no profile, or (in R2) the process is hidden, every op returns `ENOSYS`, the same as
any unknown syscall, so a non-rooted or hidden process cannot tell the syscall exists.

The exec path does not honour setuid bits and does not need to: elevation is an explicit request
the engine grants, which is how Magisk's `su` → `magiskd` handshake works too.

### 5. The root layer and module overlays (VFS)

There is a new per-instance **root layer**, `omni_linux::root::Layer`. It maps guest paths to host
files and directories and sits in `Vfs::lookup` **after binds/writable mounts and before the
sysroot manifest** (`vfs.rs:677-704`). `Vfs::list` merges it with the image's children for a
directory (today there is no merging across layers, vfs.rs:773-830; this adds one for directories
the layer touches).

It is built from:

1. **The root tools.** `/system/bin/su`, `/system/bin/magisk` and `/system/bin/resetprop` (the
   stub, mode 0755 root), and `/debug_ramdisk/` → `<instance>/data/adb/omni/debug_ramdisk/`
   holding `magisk`, `su` and `resetprop`. Magisk puts these in the same places.
2. **Each enabled module's files**, for every module in `/data/adb/modules/<id>/` that has no
   `disable`, `remove` or `skip_mount` file:
   - `system/**` maps onto `/system/**`.
   - `system/vendor/**`, `system/product/**` and `system/system_ext/**` map onto `/vendor`,
     `/product` and `/system_ext`, following Magisk's rule.
   - New files are added and image files are replaced.
   - A directory containing `.replace` replaces the image's directory wholesale.
   - Modules are applied in profile order, and a later module wins on a conflict.

**Rebuilds.** The layer is rebuilt when `<instance>/data/adb/omni/layer.gen` changes, checked by
mtime at most once a second (the same way `.omni-binds` is reloaded, vfs.rs:349-378). The boot
script touches `layer.gen` after modules are installed. Processes started after that see the
modules, and system_server starts after `post-fs-data` (§6), as on a Magisk phone.

**Hook for R2.** `lookup` and `list` take the calling process's `hidden` flag. A hidden process
does not see the root layer at all. In R1 the flag is always false.

Modes and owners of layer files come from the instance's `.omni-owners` (`owners.rs`), where the
installer's `set_perm` records them. This works because `/data/adb/modules` is on the writable
`/data` mount.

### 6. Boot: install, post-fs-data, service

**Launcher, on the host, before boot** (only when the profile is rooted):

- Write `profile`.
- Copy the selected module zips that are not yet installed at the same version into
  `<instance>/data/adb/omni/pending/<id>.zip`.
- Copy `util_functions.sh` and `busybox` from the pin into `<instance>/data/adb/magisk/`.
- A module that was installed before but is no longer selected gets a `disable` file. It is not
  removed, so switching back is cheap.

**`/vendor/etc/init/omni_root.rc` and `/vendor/bin/omni_root.sh`** go in the device overlay
(compiled in like `omni_autogrant.rc`). When the profile is absent the script exits at once.

- **`on post-fs-data`: `exec_start omni_root_pfd`** (uid 0, `oneshot`, `disabled`). `init.rs`
  runs boot-phase blocks before `class_start` and before the system_server program is spawned
  (init.rs:470-525). The plan confirms this ordering with a test. The service runs
  `omni_root.sh post-fs-data`, which:
  1. Installs each pending zip the way Magisk's `magisk --install-module` does: `busybox sh` with
     `BOOTMODE=true`, `MODPATH`, `ZIPFILE`, `OUTFD` and `util_functions.sh`'s `install_module`
     (which runs `customize.sh` and `set_perm`). It installs into `/data/adb/modules/<id>`
     directly (no `modules_update` + reboot, since the layer is consulted lazily). It logs to
     `/data/adb/omni/install.log`.
  2. For each enabled module, applies `system.prop` with `resetprop --file`.
  3. Runs each enabled module's `post-fs-data.sh` under `busybox sh`, in order, with a 10 s budget
     per script (Magisk's limit is 40 s total; a slow script is logged and left running).
  4. Touches `layer.gen`.
- **A `class late_start` service, `omni_root_service`** (uid 0, `oneshot`), runs each enabled
  module's `service.sh` in the background, as Magisk's late_start service stage does.

Script output goes to the instance's log with an `[omni-root]` prefix, so a module failure can be
seen from the log, the way every other boot step is.

### 7. Launch surfaces and the saved-device key

**CLI** (`crates/omnidroid/src/main.rs`):

- `omnidroid aosp --root`, `--module a,b,c` (implies `--root`) and `--su all|pkg,pkg`. These turn
  into `OMNI_R_ROOT`, `OMNI_R_MODULES` and `OMNI_R_SU` in `aosp_env`, the way the existing options
  do.
- `omnidroid modules [list|add <zip>]`.

**Test harness** (`r_roblox.rs`) and the warm device (`omni-warm`) write the profile and stage
files into the instance before boot (§6).

**MCP**: `start_instance` gains `root: bool` and `modules: [string]`, passed through to the
launcher command (`server.rs:219-235`). `list_instances` and `device_status` report a device's
root profile.

**Saved-device key.**
- `golden_dir` (`r_roblox.rs:177`) gains `-root-<hash8>` when rooted. The hash covers the profile
  text, each selected module zip's sha256, the Magisk pin and the root stub's sha. Unrooted keys
  are **unchanged**, so existing saved devices stay valid.
- The base device (`base-…`) is shared: root is applied on top of a base, never baked into it.
- A warm device records its profile hash. A rooted request against a warm device with a different
  hash boots a new device rather than reusing it. The warm-device user sees this in the log line
  `[warm] root profile differs: booting a new device`.

### 8. Errors

Every failure is visible, and none of them silently produces a non-rooted device that claims to be
rooted.

- **Unknown module id, duplicate id, unparsable `module.prop`.** The CLI fails before boot and
  names the id and the file.
- **Missing Magisk pin files.** The CLI fails before boot with "run `python tools/fetch_magisk.py`".
- **A module's install fails** (`customize.sh` exits non-zero or `abort`s). That module is left
  disabled, its `install.log` lines appear in the instance log, and the boot continues. The
  launcher's summary line lists the module as `failed` (`[omni-root] modules: a ok, b failed (see
  install.log)`).
- **`su` refused.** Exits 1 with `Permission denied`, as Magisk does.

### 9. Testing (R1)

**Unit tests (no boot):**
- `Profile` parse/serialize round-trip; `su_allowed`.
- `module.prop` parse.
- `Layer` build: added file, replaced file, `.replace` dir, `system/vendor` → `/vendor`, disabled
  and skip_mount modules ignored, later module wins.
- `Vfs::list` merge.
- `su` argument parsing, as a table test of argv → (uid, sh argv).
- The golden key: unrooted is unchanged, rooted differs per module set and per module sha.
- The resetprop bypass on a `PropertyService`.

**Boot test** (`tests/root_boot.rs`, `#[ignore]` like `r_roblox`, using `tests/common/boot.rs`
with a `--then` shell as uid 2000). The device is rooted with a test module
(`tests/data/omni-test-module/`) that has a `system/etc/omni-test.txt`, a `.replace` dir, a
`system.prop`, a `post-fs-data.sh` and a `service.sh` that write markers, and a `customize.sh` that
calls `set_perm`. The test checks:
- `su -c id` → `uid=0(root)`.
- `su 2000 -c id` → `uid=2000`.
- `cat /system/etc/omni-test.txt` → the module's content.
- The `.replace` dir lists only the module's files.
- `getprop ro.omni.test` → the `system.prop` value.
- `resetprop ro.build.tags test-keys` followed by `getprop` → the new value.
- The post-fs-data and service markers exist, and `magisk -v` → `<ver>:MAGISK:R`.

**The same boot without a profile:**
- `su` is not found, `/debug_ramdisk` is absent and `/system/etc/omni-test.txt` is absent.
- The `omni_root` syscall returns `ENOSYS`.
- Boot time is within noise of the unrooted baseline. The root code costs nothing when off, and
  this is checked against the same-day `main` (perf rules).

**Community module smoke:** boot rooted with one real module that uses scripts and props heavily:
**MagiskHide Props Config** (busybox-heavy `customize.sh` and `service.sh`, plus resetprop). It
must install with no errors in `install.log`, and its `props` command must run under `su`. This is
also the tool R3 will lean on.

**Apps:** install a minimal test APK (`tests/data/su-probe/`, built the way `device/src/overlay`
is) whose activity runs `su -c id` and logs the result, and
verify it gets `uid=0` with `su=all`, and is refused when the profile is `su=<some other pkg>`.

**Cross-host** (after Windows passes): sync Mac and Linux and run the boot test on each, per the
cross-host rule.

---

## R2 — Zygisk host, DenyList, Shamiko mode, Frida gadget (outline)

R1 provides the interfaces R2 builds on: `Profile::hidden(process)` and the root layer's
`hidden` flag.

- **Zygisk host.** `libomni_zygisk.so` is loaded into every app process inside the `WrapperInit`
  start (the app host command in `zygote.rs`), before `ActivityThread.main` and after ART and JNI
  are up.
  - For each enabled module with `zygisk/arm64-v8a.so` it `dlopen`s the library, calls
    `zygisk_module_entry(api, env)`, then calls `preAppSpecialize`/`postAppSpecialize` with an
    `AppSpecializeArgs` built from the real uid, gids, nice name and app data dir.
  - System_server gets `preServerSpecialize`/`postServerSpecialize`.
  - API v4/v5 covers `hookJniNativeMethods`, `pltHookRegister`/`pltHookCommit` (lsplt-style),
    `connectCompanion` (companion process as uid 0), `getModuleDir`, `getFlags`, `exemptFd` and
    the `DLCLOSE_MODULE_LIBRARY` option.
  - The spare app process (`omni-spare.jar`) must load it the same way.
- **DenyList.** A process is hidden when its package is on the profile's denylist. A hidden
  process:
  - does not see the root layer or `/data/adb`;
  - gets `ENOSYS` from `omni_root`;
  - gets no Zygisk injection, so no module `.so` is in its maps.

  `magisk --denylist add|rm|ls` edits the profile live.
- **Shamiko mode** (`shamiko` module selected). Hidden processes keep their Zygisk modules, while
  the engine hides those modules' `.so` mappings, `/proc/self/maps` entries, `/proc/self/mounts`
  and the root layer. Whitelist mode hides **every** app except those on a su allow list. This is
  Shamiko's documented behavior, implemented by the engine.
- **`zygisk-frida`** (built-in). It loads the pinned `frida-gadget` arm64 `.so` into the packages
  listed in its config file, using gadget config `listen` on a port the instance forwards to the
  host. You then run `frida -H 127.0.0.1:<port> -n Gadget`.

## R3 — Emulator hiding (outline)

`emu-hide` is a built-in module whose `module.prop` turns on engine spoofing for all apps or for
listed packages. It covers:

- **Props.** `ro.hardware`, `ro.kernel.qemu`, `ro.boot.qemu*`, the fingerprint, model, brand,
  device, `ro.product.*` and `ro.build.characteristics`, set to one real device profile (a
  modern Pixel by default).
- **`/proc`.** `cpuinfo` and `version`.
- **Goldfish devices.** `/dev/qemu_pipe`, `/dev/goldfish_*` and the `ranchu` and `goldfish` file
  names, hidden for spoofed processes.
- **Emulator packages** (`com.android.emulator.*`), not listed.
- **Hardware presence.** Sensors and telephony must look like they exist. The device currently has
  neither, which is a strong tell, so this adds stub sensors and a no-SIM telephony declaration.

The test is the stock Roblox APK with `--module emu-hide,shamiko` and Roblox on the denylist: it
joins place 8737899170 and is not kicked. The research for R3 starts from `AndroidEmulatorKick` and
`AndroidRootedKick` in `libroblox.so` (`docs/research/apk-analysis.md:272`), finding which checks
feed them before choosing what to spoof.

## Out of scope

- Magisk app UI, `magiskd`, `magisk.db`, and module install from within the guest UI.
- Real `frida-server` and guest `ptrace`.
- SELinux policy (`sepolicy.rule` is ignored because the device is permissive). Hiding permissive
  SELinux from apps is an R3 question, if Roblox checks it.
- KernelSU and APatch module formats.
