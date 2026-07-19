# omnidroid devkit — the `base_arm_devkit.qcow2` payload

These scripts are the on-device half of the **dev/debug base**. They are copied
onto the extra **devkit disk** (`base_arm_devkit.qcow2`) by `omni build-dev-base`
and attached to dev accounts as **vdc**. The production bases (`base_x86.qcow2`,
`base_arm.qcow2`) never contain any of them, and `base_arm.qcow2` is never
modified — the toolkit lives entirely on the separate vdc disk.

At start, the engine mounts vdc read-only at `/mnt/omni-devkit` and stages an
exec-capable copy at `/data/local/tmp/omni-devkit`. Everything here runs as
**root via Magisk `su`** (the arm base is a `user` build — `adb root` is
unavailable; root comes from a Magisk-patched boot, see `../DEV-BASE.md`).

| File | Purpose |
|------|---------|
| `omni-fridad`      | Start the android-**arm64** frida-server hidden: custom loopback port (not 27042), randomly-named process, prefers `frida-server-patched` if present. Self-elevates via `su 0`. |
| `omni-frida-stop`  | Stop any devkit frida-server. |
| `omni-hide`        | Hide root + Magisk + frida from a target app: add it to the Magisk **DenyList** + resetprop-spoof the classic root/verified-boot props. |
| `omni-magisk-setup`| One-time: enable **Zygisk + Enforce DenyList**, install **Shamiko** (if `Shamiko.zip` is dropped into the disk's `/modules`), optionally install the Magisk manager app. |

Also placed on the disk by the builder (not source-controlled here — fetched at
build time):

| Path on the disk | What |
|------------------|------|
| `/frida-server`         | android-arm64 frida-server, pinned version. |
| `/frida-server-patched` | *optional* anti-detection build; drop one in to close the gum/gmain thread-name gap stock frida can't. |
| `/magisk.apk`           | the Magisk installer/manager APK. |
| `/bin/magiskboot`, `/bin/magiskinit`, `/bin/magiskpolicy`, `/bin/busybox` | Magisk arm64 multicall binaries. |
| `/bin/boot_patch.sh`, `/bin/util_functions.sh` | Magisk's boot-image patch scripts (used by `--patch-boot`). |
| `/manifest.json` | records versions, the hidden frida port, the mount paths, and what was installed. |

## Why Magisk (not KernelSU)

The retired x86 dev base was rooted with KernelSU (already in the Bliss image).
The arm LineageOS base ships **no root** (a `user` build), so the dev base roots
it with **Magisk** (a patched boot in the dev system overlay), which is also the
maintainer's documented root path for this image. Magisk brings its own hiding
(Zygisk + DenyList, plus Shamiko) which is what `omni-hide` / `omni-magisk-setup`
drive to hide root, the Magisk install itself, and frida from a target app.

## Notes / honest limits

- The boot patch (`omni build-dev-base --patch-boot`) is brick-risky and must be
  verified on a real boot; without it these scripts have no `su` to run under.
- SELinux is **Enforcing** on this LineageOS base (unlike the old Permissive
  Bliss dev base) — frida-server runs fine under Magisk, but a target can still
  read `getenforce`.
- Stock frida-server still names its worker threads `gmain`/`gum-js-loop`/
  `pool-frida`; `omni-fridad` hides the process name + port but not those thread
  names — drop a patched `frida-server-patched` on the disk to close that gap.

Edit a script here, then rebuild the disk (`omni build-dev-base`) to ship the
change; the devkit disk is immutable once built, like the bases.
