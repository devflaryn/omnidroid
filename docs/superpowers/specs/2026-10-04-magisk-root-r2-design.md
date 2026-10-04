# omnidroid Rooted Device R2 — Zygisk, Hiding, Emulator-Hiding, Frida — Design

Date: 2026-10-04
Status: draft (awaiting owner review)
Branch (planned): `feat/magisk-zygisk` from `feat/magisk-root` (R1 is there, unmerged)
Builds on: `docs/superpowers/specs/2026-10-03-magisk-root-design.md` (R1, merged on feat/magisk-root)

## Goal

Make the **stock Roblox APK join PS99 (place 8737899170) and stay in-world un-kicked** on the
rooted omnidroid device, and let a chosen app run a **real Zygisk module** and accept a **Frida**
connection. Today (R1) a rooted or plain Roblox run is disconnected ~100 s into the world with
`Disconnect reason received: 305` — the **emulator** kick. Roblox has two independent kicks in
`libroblox.so` (`AndroidEmulatorKick`, `AndroidRootedKick`; `docs/research/apk-analysis.md:272`),
so the un-kicked join needs **both** root hiding and emulator hiding. This spec therefore folds in
what the R1 spec called R3 (emulator hiding); there is no separate R3 after this.

Success (end to end): one launch — `omnidroid aosp --apk <stock Roblox> --cookie <acct> --place
8737899170 --module emu-hide,shamiko` with Roblox on the DenyList — joins PS99 and is **not kicked**
(no 305, no root kick), for the length of a normal session. And, separately, `--module
zygisk-frida` with a target app lets `frida -H 127.0.0.1:<port>` attach to it; and a real community
Zygisk module's `postAppSpecialize` runs with a working JNI hook.

Owner decisions (2026-10-04):
- **Full Zygisk API** — the host implements the real Zygisk API (v2–v5) so arbitrary community
  Zygisk modules load unchanged, not just our built-ins.
- **Primary goal is the un-kicked PS99 join** (root + emulator hidden). The Frida gadget is a
  first-class but secondary deliverable.

## Why engine-native (recap) and what R1 left for R2

R1 established: a host-only root **profile**, a module **catalog**, a **root layer** over `/system`,
the **`omni_root`** syscall, and the **`magisk`** binary. R1 deliberately left two seams for R2:
`Profile::hidden()` (returns `false` today, `root/profile.rs:172-175`) and the per-process root
layer (`Vfs::with_root_layer(Option<…>)`, `vfs.rs:576`). The `denylist=` and `shamiko=` profile
lines are already parsed and stored (`root/profile.rs:8-9,83-94`).

Real Magisk hides root by unmounting its magic mounts per process and loading into zygote; real
Zygisk injects at the zygote fork. omnidroid has no zygote fork — **each app is its own host OS
process** started through `WrapperInit` (`zygote.rs:174,229-261`). So both injection and hiding are
done by the engine, per app host process, at the point that process starts. The exploration pinned
the exact seams:

- **Injection seam.** The app's own code (ART `WrapperInit.execApplication` → `ActivityThread`)
  begins when the per-app host process maps `app_process64` + `linker64` and enters the guest
  (`process.rs:730-739`, `exec.rs:52`). A Zygisk host `.so` must be mapped and run **in that host
  process, after the image is mapped and before guest entry**. There is no such hook today; it is
  new machinery in the per-app start path (`omni-linux-run.rs:101-116`). The app's **uid**,
  **nice-name/package** and derived data dir are already known there (`--uid`, `--nice-name=`,
  `--package-name=`; `zygote.rs:133-137,170-173`) — exactly the DenyList key and the Zygisk
  `AppSpecializeArgs`.
- **No live-process dlopen.** Guest `.so` loading happens at process start or in the isolated lab
  (`crates/omni-debug/src/session.rs`); there is no attach-and-inject into an already-running app.
  The Zygisk host loads at the start seam, which is sufficient (that is where real Zygisk
  specializes too).
- **Hiding is cheap and per-process.** The `Vfs` is built per host process (`process.rs:638-640`);
  a hidden app simply gets `with_root_layer(None)` → `/system` shows no su/modules
  (`vfs.rs:709-717`). `PropertyService::global` is a **per-host-process** `OnceLock`
  (`props.rs:408-411`), so a hidden app can be served stock props. `/proc/self/maps` and mounts are
  generated per process (`procfs.rs:87-114`), so module/Zygisk `.so` mappings can be filtered out
  for a hidden process. `omni_root` already returns `ENOSYS` without a rooted profile.
- **system_server** runs in the primary host process (`omni-linux-run.rs:110`), the seam for
  `preServerSpecialize`.
- **The spare** app process reaches `WrapperInit` too (`zygote.rs:356-427`); the same injection
  seam applies, but its uid/package are unknown until `take_spare` writes the go-file, so the
  Zygisk host loads at spare start and **defers the AppSpecialize callbacks** until the app is
  named.
- **Gadget networking.** Guest TCP binds are real host sockets behind a per-instance loopback
  namespace: a guest loopback/wildcard bind lands on host `127.0.0.1:<host_port>`
  (`hostnet.rs:379-394`, `loopns`). A host-side forwarder reaches the gadget at that mapped host
  port.

## Decomposition

Four phases, each its own implementation plan, merged to the branch only when its tests pass. The
un-kicked PS99 join is **P2 + P3**; **P1** is the substrate both ride on; **P4** is the RE
deliverable.

| | Delivers | Proven by |
|---|---|---|
| **P1** | The Zygisk host: full Zygisk API, loads modules into each app + system_server at the start seam | A real community Zygisk module's `postAppSpecialize` runs; a JNI and a PLT hook it registers fire |
| **P2** | DenyList + Shamiko hiding (fills `Profile::hidden`) | A DenyList app fails a root-detector; Roblox is not root-kicked |
| **P3** | Emulator hiding (`emu-hide`, the former R3) | **Stock Roblox joins PS99 and gets no 305** — the headline |
| **P4** | `zygisk-frida` built-in module + host forwarder | `frida -H 127.0.0.1:<port>` attaches to a gadget app and hooks it |

Each phase gets a short spec/brief of its own before its plan, as R1's tasks did. P1 is the
largest and riskiest; the full-API bar means it carries most of R2's effort.

---

## P1 — The Zygisk host (full API)

### The injection

A new native library **`libomni_zygisk.so`** (our source, NDK-built, committed + sha-pinned like
`device/root/magisk`) is mapped into **every app host process and the primary (system_server)
process** at the start seam, when the profile is rooted. New machinery in the per-app start path
(`omni-linux-run.rs`): after `Process::spawn_as` maps `app_process64`+`linker64` and before guest
entry, the engine maps `libomni_zygisk.so` into the guest space (reusing the production ELF mapper,
`exec::load_elf` / the `Session` loader's relocation+RELRO logic, `session.rs:317-345`, generalized
to map into a live process's `GuestSpace`), resolves its one exported entry, and calls it on the
guest's startup thread before `WrapperInit` runs. This is gated on the rooted profile and the
process's hidden/deny state (P2): a plainly-hidden app is not injected.

For the **spare** (`zygote.rs:356-427`), the host loads at spare start and defers the
`AppSpecialize` callbacks until `take_spare` names the app (the go-file carries uid + class).

### The host's job (inside the app, as guest code)

`libomni_zygisk.so`'s entry, running as guest arm64 code with ART + JNI already up:

1. Reads the enabled modules and this process's `AppSpecializeArgs` (uid, nice-name, data dir) from
   a small per-process handoff the engine writes (an env var or a fixed guest file the engine
   populates at injection — the uid/package are already known host-side).
2. For each enabled module with a `zygisk/arm64-v8a.so`, `dlopen`s it (guest `android_dlopen_ext`),
   resolves `zygisk_module_entry`, and calls it with the Zygisk API table + a JNI env.
3. Calls each module's `preAppSpecialize(AppSpecializeArgs*)` then `postAppSpecialize(...)`. For the
   primary process it calls `preServerSpecialize`/`postServerSpecialize` instead.
4. Honors module options: `FORCE_DENYLIST_UNMOUNT`, `DLCLOSE_MODULE_LIBRARY` (unload the module
   `.so` after post-specialize).

### The Zygisk API (v2–v5)

The host exports the real Zygisk API ABI so unmodified community modules link and run:

- `registerModule(api, module)`, the `zygisk_module_entry` handshake, API version negotiation.
- **`hookJniNativeMethods(env, className, methods, n)`** — swap a class's registered JNI natives,
  returning the originals (the common hook path).
- **PLT hooks** — `pltHookRegister(regex, symbol, newFunc, oldFunc)` + `pltHookCommit()`,
  implemented lsplt-style over the engine's knowledge of each mapping's relocations/GOT (the loader
  already resolves these, `session.rs:317-345`).
- **`connectCompanion()`** — a module companion runs as a **uid-0 companion host process** the
  engine spawns on first request (a dedicated `omni-linux-run` process under the module's dir), the
  socket pair bridged to the module. Companions are where modules do root work.
- `getModuleDir(fd)`, `getFlags()` (PROCESS_ON_DENYLIST etc.), `setOption(...)`, `exemptFd(fd)`.

### Tests (P1)

- Host unit tests (lab/`Session`): map `libomni_zygisk.so` with a stub module, assert
  `zygisk_module_entry` + `preAppSpecialize` + `postAppSpecialize` are called in order with the
  right args; a registered JNI hook replaces and chains; a PLT hook redirects a symbol; a companion
  process is spawned as uid 0 and the socketpair round-trips.
- Boot test (`#[ignore]`): a rooted device with a tiny test Zygisk module whose `postAppSpecialize`
  writes a marker and installs a JNI hook on a known method in the test app; assert the marker and
  the hook effect. The spare path loads the host and defers correctly.
- A **real community Zygisk module** smoke (guarded by an env var pointing at a module zip), e.g. a
  simple property/UI Zygisk module: its callbacks run with no error in `install.log`.
- Non-rooted and hidden processes are **not** injected (assert no host `.so` in `/proc/self/maps`).

---

## P2 — DenyList + Shamiko (fills `Profile::hidden`)

`Profile::hidden(package) -> bool`: true when the package is on `profile.denylist`, or when Shamiko
mode is on and the package is not whitelisted. Threaded into the per-app start path from the
already-known package (`zygote.rs:137,170-173`).

A **hidden** process:
- gets `Vfs::with_root_layer(None)` → no su, no `/system` module overlays, no `/debug_ramdisk`
  (`vfs.rs:576,709-717`);
- gets **stock props** — the per-process `PropertyService` is built without the Magisk props
  (`props.rs:408-411`); `ro.*` Magisk markers absent;
- gets `ENOSYS` from `omni_root` for every op (the handler already gates on the profile; add the
  hidden check so even a planted `su` can't elevate);
- gets **filtered `/proc/self/maps` and `/proc/self/mounts`** — no module/Zygisk `.so` names, no
  Magisk mount lines (`procfs.rs:87-114`);
- sees no `/data/adb` (the layer and `/data/adb` tree are not presented).

**Plain DenyList** (`denylist=<pkg>`): the hidden app is **not** Zygisk-injected and sees nothing
of root — maximum invisibility, no modules in it.

**Shamiko mode** (`shamiko=on`, the `shamiko` marker module selected): hidden apps **keep** their
Zygisk modules injected (so a hook/hide module still runs) while the engine hides the modules'
`.so` maps, `/proc/self/mounts`, and the root layer from that app. **Whitelist mode**
(`shamiko=whitelist`): hide from **every** app except those on a su allow list. `magisk --denylist
add|rm|ls` edits the live profile (via the `omni_root` SETPROP-style path or a new op) so the set
can change without a reboot.

### Tests (P2)
- Unit: `Profile::hidden` truth table (denylist, shamiko on, whitelist).
- Boot: a root-checker app on the DenyList reports **no root** (`su` not found, no Magisk props, no
  su in maps, `omni_root`→ENOSYS); a non-denylisted app still has root.
- Shamiko: a denylisted app that keeps a Zygisk module sees the module's effect but not its `.so`
  in maps; whitelist mode hides from an app not on the allow list.
- **Roblox not root-kicked**: stock Roblox on the DenyList is not disconnected by
  `AndroidRootedKick` (it is still 305-kicked until P3 — that is the P3 gate, not P2's).

---

## P3 — Emulator hiding (`emu-hide`, the headline)

A built-in module `emu-hide` whose `module.prop` turns on **engine spoofing** for all apps or
listed packages (per launch: `--module emu-hide`, optionally scoped). It makes the device look like
a real phone, which is what defeats `AndroidEmulatorKick` / the 305. Covered surfaces, each
per-spoofed-process:

- **Props**: `ro.hardware`, `ro.kernel.qemu`, `ro.boot.qemu*`, `ro.hardware.*`, the build
  fingerprint, `ro.product.{brand,device,model,manufacturer,name}`, `ro.build.characteristics`,
  `ro.bootloader`, `ro.serialno` — set to one real device profile (a modern Pixel by default;
  configurable). Served through the per-process `PropertyService` (`props.rs:408-411`).
- **`/proc`**: `/proc/cpuinfo` (a real ARM SoC, no `goldfish`/`ranchu`), `/proc/version`
  (`procfs.rs`).
- **Goldfish/ranchu devices**: hide `/dev/qemu_pipe`, `/dev/goldfish_*`, and the `ranchu`/`goldfish`
  names from a spoofed process (VFS lookup/list, `vfs.rs`).
- **Packages**: the emulator packages are not listed (R1's device already leaves most out).
- **Hardware presence**: the device currently has **no sensors and no telephony** (a strong
  emulator tell, R1 device overlay). `emu-hide` adds a stub sensor set and a no-SIM telephony
  presence so a probing app sees plausible hardware.
- **SELinux**: R1 is permissive, which a detector can read as odd. If Roblox checks it, present an
  enforcing-looking status to spoofed processes (investigate during P3).

The exact set is driven by **what Roblox actually checks**: P3 starts by using the R1 frida/lab
tooling and the live 305 to find which inputs feed `AndroidEmulatorKick` in `libroblox.so`
(`docs/research/apk-analysis.md:272`), then spoofs those. This is empirical — iterate against the
kick.

### Tests (P3)
- Unit: a spoofed process reads the Pixel props, a clean `/proc/cpuinfo`, and no goldfish devices;
  an unspoofed process is unchanged.
- **The headline boot test**: `--module emu-hide,shamiko` with Roblox on the DenyList → stock
  Roblox joins PS99 (`onGameLoaded`) and runs **past 101 s with no `Disconnect reason received:
  305`** for a sustained window (e.g. ≥5 min). This is the R2 acceptance gate.
- Cross-host (per the owner's rule) on Mac + Linux after Windows is green.

---

## P4 — `zygisk-frida` gadget + host forwarder

A built-in Zygisk module `zygisk-frida` that, in its `postAppSpecialize`, loads the pinned
**frida-gadget** arm64 `.so` (fetched + sha-pinned like the Magisk assets, `tools/`) into the
packages named in its config, with a gadget config set to **`listen`** on a fixed port. The guest
bind lands on a host `127.0.0.1:<host_port>` via the loopback namespace (`hostnet.rs:379-394`); a
small **host-side forwarder** (in the launcher / MCP) maps a stable host port to it and prints it,
so the owner runs `frida -H 127.0.0.1:<port> -n Gadget`. `frida-server` and guest `ptrace` remain
out of scope (dropped in the RE-workbench spec); the gadget is the sanctioned live-hook route.

### Tests (P4)
- Boot: `--module zygisk-frida` scoped to a test app → the gadget loads (its `.so` in the app's
  maps on a non-hidden app), the port is listening, and a scripted `frida` client attaches and
  reads a known value / installs a trivial interceptor.

---

## Risks and sequencing

- **P1 is the hard part.** Full Zygisk API fidelity — PLT hooks, JNI hooks, the companion process,
  and mapping a `.so` into a live app host process before guest entry — is substantial new native
  + engine machinery. If full fidelity proves too large in one go, P1 ships the injection + entry +
  specialize callbacks first and the hook/companion API as a second plan, but the owner chose the
  full-API bar, so the spec targets it.
- **P3 is empirical.** Beating `AndroidEmulatorKick` is a measure-spoof-remeasure loop against the
  live 305; the spec fixes the mechanism (per-process spoofing surfaces), not the exact value set.
- **Order.** P1 → P2 → P3 gets to the headline un-kicked join; P4 can land any time after P1.
- **Do not chase the 305 before P1+P2+P3.** A rooted run is 305-kicked today regardless; the kick
  is addressed only when root (P2) and emulator (P3) are both hidden, on the P1 substrate.

## Out of scope (R2)

- `frida-server`, guest `ptrace` (dropped in the RE-workbench spec).
- The Magisk app UI, `magiskd`, `magisk.db`, in-guest module management.
- KernelSU / APatch module formats.
- LSPosed itself (its manager + the system_server Xposed bridge) — the Zygisk host makes it
  *possible* later, but LSPosed is its own sub-project.
