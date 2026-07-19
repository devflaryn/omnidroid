# omnidroid dev base — the arm "devkit disk" (`base_arm_devkit.qcow2`)

The dev/debug base is **not a separate flattened image** anymore. It is the
**shared, immutable `base_arm` plus one extra virtio disk** attached to dev
accounts as **vdc**: `base_arm_devkit.qcow2`. That disk carries the whole
reverse-engineering toolkit. `base_arm.qcow2` is **never modified** — a dev
account is an ordinary arm account (system-overlay + data + efivars trio) with
the devkit disk added, exactly like accounts already carry a separate `/data`
disk.

The retired x86 `base-dev.qcow2` (frida/Magisk baked into `/system`, x86_64
under libndk translation) is **gone**. The arm base runs **arm64 natively** — no
translation — so native frida hooks (Interceptor/Stalker) now work too.

| Base tag | Disks | Shipped? | Contents |
|----------|-------|----------|----------|
| `x86` (production) | `base_x86.qcow2` (+kernel/initrd) | **Yes** | Bliss OS + kiosk. Untouched. |
| `arm` (production) | `base_arm.qcow2` + provisioned trio | **Yes** | LineageOS arm64 + kiosk. Untouched. |
| `dev` (dev/debug) | `base_arm` + **`base_arm_devkit.qcow2` (vdc)** + a rooted dev system overlay | **No** | frida-server (arm64) + Magisk + omni tools. Only `omni-agent` boots it. |

`current_base` is **never** changed by building the dev base. It is selected
**only** explicitly: `omni create <name> --base dev` (agent:
`ensure_emulator_running(dev=true)` / `OMNI_USE_DEV_BASE=1`).

## Building it

```bash
omni build-dev-base                 # build the devkit disk + register 'dev' (no root yet)
omni build-dev-base --json
omni build-dev-base --frida-version 17.15.4 --frida-port 27142
omni build-dev-base --no-magisk     # frida only, no Magisk (no root/hiding)
omni build-dev-base --patch-boot    # ALSO Magisk-patch the boot to ROOT it (see below)
```

What `build-dev-base` does (all host-side, **no guest boot, no root, cross-
platform**):

1. **Stages the toolkit** into a directory: the android-**arm64** frida-server
   (xz→ELF), the **Magisk** APK (the release build, not `app-debug.apk`) + its
   extracted arm64 `magiskboot`/`magiskinit`/`magisk`/`init-ld`/`magiskpolicy`/
   `busybox` + `boot_patch.sh`/`util_functions.sh`/`stub.apk`, the LF-normalized
   `omni-*` scripts, and a `manifest.json`.
2. **Builds `base_arm_devkit.qcow2`** — a populated ext4 image via `mke2fs -d`
   (rootless, works on macOS/Linux/Windows-arm64) → `qemu-img convert` to qcow2.
   This is the extra vdc disk. **~256 MiB.**
3. **Creates `base_arm_devsystem.qcow2`** — a copy of the provisioned arm system
   overlay (thin, still COW-backed by `base_arm.qcow2`). This is where the
   Magisk-patched (rooted) boot will live. `base_arm.qcow2` is only ever read.
4. **Registers the `dev` base** in `configs/paths.json` (arm-uefi + `devkit`).
   `current_base` is left unchanged.

Downloads use `curl` (system cert store) with a urllib fallback, so a fresh
python.org install (no bundled CA certs) still works.

## What lands on the devkit disk

Source for the scripts is `devkit/` (see `devkit/README.md`); binaries are
fetched at build time.

| Path on the disk | What |
|------------------|------|
| `/frida-server` | android-**arm64** frida-server, pinned version. |
| `/frida-server-patched` | *optional* anti-detection build — drop one in and `omni-fridad` prefers it. |
| `/magisk.apk` | the Magisk installer/manager APK (also used to root the boot). |
| `/bin/magiskboot`, `/bin/magiskinit`, `/bin/magiskpolicy`, `/bin/busybox` | Magisk arm64 multicall binaries. |
| `/bin/boot_patch.sh`, `/bin/util_functions.sh` | Magisk's boot-image patch scripts. |
| `/omni-fridad` | start frida-server **hidden** (custom loopback port, randomized process name). |
| `/omni-frida-stop` | stop the devkit frida-server. |
| `/omni-hide` | hide root + Magisk + frida from a target app (Magisk DenyList + resetprop). |
| `/omni-magisk-setup` | one-time: enable Zygisk + Enforce DenyList (+ Shamiko/manager if present). |
| `/manifest.json` | versions, hidden frida port, mount paths. |

## How a dev account uses it

- `omni create <name> --base dev` copies the arm trio **and** creates a cheap
  per-account COW overlay of the devkit disk (`accounts/<name>/devkit.qcow2`),
  wired into QEMU as **vdc**. The account is flagged `"dev": true`.
- On `start`/`resume`, the engine **activates** the devkit (`_devkit_activate`):
  mounts vdc **read-only** at `/mnt/omni-devkit` and stages the exec-capable copy
  at `/data/local/tmp/omni-devkit` (the tools all run as **root via Magisk `su`**;
  `/mnt` is a noexec tmpfs, so binaries are read from the mount but never
  exec'd there directly). If the boot is not yet rooted, activation prints a
  clear "not rooted — run `--patch-boot`" note and does not fail the start.

## Root & hiding model — Magisk (user-chosen)

The LineageOS arm base is a **`user` build**: `adb root` is disabled and there is
no su-addon, so root comes **only from a Magisk-patched boot**. `--patch-boot`
roots the **dev system overlay's** boot partition (`boot` = `vda6`), so:

- `base_arm.qcow2` stays byte-identical — the patch lives in
  `base_arm_devsystem.qcow2` (still COW on the immutable base).
- root is **Magisk `su`** (the whole devkit runs as root via `su 0`).
- hiding is Magisk's own machinery: `omni-magisk-setup` turns on **Zygisk +
  Enforce DenyList** (and installs **Shamiko** if you drop `Shamiko.zip` into the
  disk's `/modules`, which also hides the Magisk app itself); `omni-hide <pkg>`
  adds the target to the **DenyList** (Magisk unmounts its modifications + hides
  su/daemon for that app) and resetprop-spoofs the classic root/verified-boot
  props. frida is hidden by `omni-fridad` (custom port + randomized process name).

### The boot patch (`--patch-boot`) — how it works

`_patch_dev_boot` roots the dev overlay **offline** (no prior root needed, cross-
platform — no `qemu-nbd`/libguestfs): export the overlay to raw (merged through
its base backing), pull `vda6` (the `boot` partition) out via a minimal GPT
reader, run Magisk's `boot_patch.sh` (with the full arm64 toolset: `magiskboot`,
`magiskinit`, `magisk`, `init-ld`, `stub.apk`) on that boot **file** inside a
throwaway arm guest (it patches a file — no in-guest root needed), write the
patched image back at the same offset, and re-import to qcow2. base_arm.qcow2 is
never touched.

It is OFF by default (editing boot is inherently risky) and needs real disk
headroom (the raw export is ~the disk's virtual size, ~5 GiB); it refuses to run
and leaves the overlay UNROOTED if there isn't enough free space.

**VERIFIED WORKING (2026-07-14, arm64 LineageOS 23.2 under HVF):** after the
patch the guest boots with `magiskd` running as root and the Magisk manager app
auto-installed; `su` grants `uid=0(root) … context=u:r:magisk:s0`.

### Headless su + the reproducible dev `/data` template

Two gotchas that the build handles for you:

1. **`su` is not on `$PATH`.** This LineageOS is all-read-only, so Magisk can't
   symlink `su` into a PATH dir — it lives in Magisk's own tmpfs at
   **`/debug_ramdisk/su`**. The engine + agent probe `/debug_ramdisk/su` →
   `/sbin/su` → `su` (`resolve_su` / `_resolve_su`), so callers never hardcode it.
2. **MagiskSU prompts for approval** (a GUI dialog) the first time a uid requests
   root — which hangs a headless run. So root is granted **once** through the app
   and baked into a dev `/data` template: **`base_arm_devdata.qcow2`** is a copy
   of the provisioned `/data` where the Magisk policy DB already grants the adb
   shell **Forever**, with `root_access=3` + **Zygisk + Enforce DenyList** on.
   Dev accounts use it (config `dev.data`), so **root works headlessly from first
   boot — no prompt, no taps**. It is a matched FBE pair with
   `base_arm_devsystem.qcow2` (same `/metadata` keys), so all dev accounts share
   it safely.

Regenerating the template (only if you rebuild the rooted boot from scratch):
boot a dev account, install the full Magisk apk (`pm install`), trigger `su`,
tap **GRANT (Forever)** on the `SuRequestActivity` dialog (drive it over VNC / via
`adb shell input tap`), set `root_access=3`/`zygisk=1`/`denylist=1` via
`magisk --sqlite`, then capture that account's `data.qcow2` →
`base_arm_devdata.qcow2`.

Verify at any time:
```bash
omni create dbg --base dev
omni start dbg --wait
omni adb dbg -- shell /debug_ramdisk/su 0 id     # expect uid=0(root)
```

## Using it

```bash
# omnidroid, directly (tools run as root via the resolved su):
omni create dbg --base dev
omni start dbg --wait
omni adb dbg -- shell /debug_ramdisk/su 0 /data/local/tmp/omni-devkit/omni-fridad
omni adb dbg -- shell /debug_ramdisk/su 0 /data/local/tmp/omni-devkit/omni-hide com.target.app

# omni-agent (handles su resolution + activation for you):
ensure_emulator_running(dev=true)     # or export OMNI_USE_DEV_BASE=1
ensure_frida_server()                 # -> frida -H 127.0.0.1:<forwarded port>
hide_root_from_app("com.target.app")
```

The dev base is a superset of the production arm base, so everything else
(install, kiosk launch, screenshots, logcat, capture, always-on auto-
screenshots) works identically. Auto-screenshots remain a **dev-base-only**
feature (now detected by the account's `dev` flag / the vdc disk, not an x86
base tag).
