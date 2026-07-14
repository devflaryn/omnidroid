# omnidroid devkit — the base-dev.qcow2 payload

These files are baked into **`base-dev.qcow2`** (and only that image) by
`omni build-dev-base`. They are the on-device half of the dev/debug base: the
production bases (`base_x86.qcow2`, `base_arm.qcow2`) never contain any of them.

Everything here lands under `/system` inside the flattened dev base, so it
survives into every account created from `base-dev` (per-account `/data` is
disposable and is *not* where the toolkit lives).

| File | Installed to (in base-dev `/system`) | Purpose |
|------|--------------------------------------|---------|
| `omni-fridad`      | `/system/bin/omni-fridad`            | Start frida-server hidden: custom loopback port (not 27042), randomly-named process, prefers a patched binary if present. |
| `omni-frida-stop`  | `/system/bin/omni-frida-stop`        | Stop any devkit frida-server. |
| `omni-hide`        | `/system/bin/omni-hide`              | Best-effort hide root+frida from a target app (resetprop spoofs via the baked Magisk applet + KernelSU per-app denylist). |
| `omni-devkit.rc`   | `/system/etc/init/omni-devkit.rc`    | `omni_fridad` init service (DISABLED by default; `start omni_fridad`). |

Also baked by the builder (not source-controlled here — fetched at build time):

| Path in base-dev | What |
|------------------|------|
| `/system/bin/frida-server`         | stock frida-server (x86_64), pinned version. |
| `/system/bin/frida-server-patched` | *optional* anti-detection build; drop one in to close the gum/gmain thread-name gap stock frida can't. |
| `/system/bin/omni-magisk`          | Magisk multicall binary, used only as `omni-magisk resetprop …` (NOT a full Magisk install). |
| `/system/etc/omni-devkit/manifest.json` | records versions, the frida port, and what was installed. |

## Why KernelSU + Magisk *tools* (not full Magisk)

The Bliss base is already rooted with **KernelSU**. Stacking a full Magisk
(patched boot ramdisk + its own su) on top on Android-x86 is a kernel-level
conflict and routinely soft-bricks the image. So the dev base keeps KernelSU as
the root provider and borrows only Magisk's userspace `resetprop` applet for
prop-spoofing — the piece you actually need to defeat build-tag / verified-boot
root checks — plus KernelSU's own per-app hiding for the target under test.

## Residual signals (be honest about these)

- This Bliss base already ships the classic root-detection props **clean**
  (`ro.build.tags=release-keys`, `ro.boot.verifiedbootstate=green`,
  `ro.debuggable=0`), and root is **KernelSU** (kernel-level, so `adb root`/`su`
  work regardless of `ro.debuggable`). So the prop side looks stock by default.
- SELinux is **Permissive** on this base (frida runs with no ptrace friction) —
  which is itself detectable via `getenforce`.
- Stock frida-server still names its worker threads `gmain` / `gum-js-loop` /
  `pool-frida`. `omni-fridad` hides the *process* name and *port*, but not those
  thread names — drop a patched `frida-server-patched` in to close that gap.

Edit a script here, then rebuild the base (`omni build-dev-base`) to ship the
change; the base is immutable once built, exactly like the production bases.
