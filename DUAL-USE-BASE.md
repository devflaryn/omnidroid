# omnidroid dual-use bases — one image ships AND debugs

There is **no separate dev base**. Every shipped base (`arm`, `x86`) is
**dual-use**: it boots as the production image, and omni-agent debugs on that
same image. What used to be "the dev base" is now three independent pieces.

| Piece | What it is | Where it lives |
|---|---|---|
| **root** | Magisk-patched boot, always present | baked into the shipped image (`"rooted": true`) |
| **hiding** | Zygisk + Enforce DenyList, game on the DenyList | baked into `/data` + re-enforced every boot (`_enforce_hiding`) |
| **toolkit** | frida-server + `omni-*` scripts | the **devkit disk** `base_<arch>_devkit.qcow2`, attached as **vdc** only on a `--debug` boot |

So **debug is a per-BOOT option, not a base and not an account property.** The
same account boots production on one run and debug on the next, off one image.

```bash
omnidroid start <name>                 # production boot: rooted, hidden, no devkit disk
omnidroid start <name> --debug         # debug boot: same image + the devkit disk (vdc)
omnidroid start <name> --apk build.apk # swap the Roblox build — works on ANY base, no --debug needed
```

Agent equivalents: `ensure_emulator_running(debug=true)` (or
`OMNI_DEBUG_BOOT=1`), `play_roblox(..., debug=true)`, `launch_roblox_build(...,
apk_path=..., debug=...)`. APK swap never requires `debug`.

## Building the pieces

Two orthogonal build steps; neither changes which base ships or `current_base`.

```bash
omnidroid build-devkit [--arch arm|x86]   # build the attachable devkit disk (frida + omni-* tools + Magisk bins)
omnidroid root-base   [--base <tag>]      # bake a Magisk-patched (rooted) boot into a thin overlay of the base
```

### `omnidroid build-devkit`

Assembles `base_<arch>_devkit.qcow2` entirely host-side (rootless,
cross-platform via `mke2fs -d`): the native-arch frida-server ELF, the Magisk
APK + its extracted multicall binaries, the LF-normalized `omni-*` scripts, and
a `manifest.json`. It belongs to no base entry — `omnidroid start --debug` attaches
it as vdc, and the guest mounts it read-only at `/mnt/omni-devkit`, staging the
toolkit into `/data/local/tmp/omni-devkit` to execute it (`/mnt` is noexec).

### `omnidroid root-base`

Bakes root **into** a shipped base without changing which image it is:

1. **Thin rooted system overlay** — `base_arm_system_rooted.qcow2` is a COW
   child of the current production system overlay, carrying only the
   Magisk-patched boot partition (vda6). The production system image is never
   modified and the backing chain (and therefore the whole production lineage)
   is preserved — no flatten.
2. **The boot patch is written through a throwaway guest.** The current boot is
   read out of the production system (raw export + a minimal GPT reader),
   patched with Magisk's `boot_patch.sh` inside a throwaway arm guest
   (`magiskboot` patches a *file*, so no in-guest root), then `dd`'d into the
   rooted overlay via an attached virtio-blk disk at the vda6 offset. Because
   the write goes through QEMU into the qcow2 overlay, only the changed boot
   blocks land there and the overlay stays thin.
3. **Rooted /data** — `base_arm_data_rooted.qcow2` is the production `/data`
   plus the Magisk policy (shell su granted **Forever**, `root_access=3`,
   `zygisk=1`, `denylist=1`, `com.roblox.client` on the DenyList) so root works
   **headlessly from first boot** (no GUI su prompt) and the game is hidden.
4. The base entry is re-registered with `"rooted": true`, pointing at the
   rooted matched pair. `current_base` is untouched.

It edits a boot partition, so it is **brick-risky** and must be verified on a
real boot. A base without the rooted images still registers and boots the
unrooted images (with a "root pending" note); root-needing operations then
report `root_unavailable` instead of failing the boot.

### The one-time MagiskSU grant (the manual step)

`root-base` builds and boot-verifies the rooted **system** automatically (the
qemu-io write-back, then a boot with `magiskd` running). It then tries to
produce the pre-granted rooted **/data** fully headlessly: install the Magisk
app, reboot so `magiskd` registers it, trigger `su`, and approve the
SuRequestActivity dialog via `uiautomator`. On a fresh Magisk-patched boot the
app comes up needing "additional setup" and `magiskd` does not always route the
first `su` to a dialog, so this can fail — the same one-time GRANT that has
always been manual for this base.

**When the headless grant fails, `root-base` deliberately leaves the base
UNROOTED** (it never registers `rooted:true` against an ungranted /data — that
would make `su` prompt on every production boot). `base_arm_system_rooted.qcow2`
is kept for reuse. To finish it by hand, once:

```bash
# 1. Boot the rooted system with a fresh /data + the devkit, and watch it:
omnidroid view _rootdata --start --debug        # or drive it over VNC
# 2. In the guest: open the Magisk app, complete its setup, trigger su
#    (any omni-* tool), and tap GRANT (Forever) on the dialog. Then set the
#    policy so it is headless forever:
omnidroid adb _rootdata -- shell su 0 magisk --sqlite \
  "REPLACE INTO settings (key,value) VALUES('root_access',3)"
omnidroid adb _rootdata -- shell su 0 magisk --denylist add com.roblox.client
# 3. Capture that /data as the rooted /data, then re-run root-base:
qemu-img convert -O qcow2 -c <that account's data.qcow2> \
  $OMNI_IMAGES_DIR/base_arm_data_rooted.qcow2
omnidroid root-base --base arm     # now finds base_arm_data_rooted.qcow2 -> registers rooted
```

Until then the base ships **unrooted** and fully functional (production boots,
kiosk, login, APK swap, logcat, screenshots all work) — only frida/root hooking
waits on the grant.

## Every boot, production included

1. Boot the (rooted) image.
2. `_enforce_hiding()` — idempotent, needs only `su` (the `magisk` CLI is in the
   rooted boot; **no devkit disk required**): assert Zygisk + Enforce DenyList,
   add the game to the DenyList, resetprop the classic root/verified-boot props.
   On an unrooted base it is a logged no-op and never fails the boot.
3. Kiosk foregrounded + Magisk manager force-stopped.
4. Only with `--debug`: the vdc devkit is activated (`_devkit_activate` stages
   frida + `omni-*`).

A production instance therefore differs from an unrooted one in exactly one
observable way: root exists and is hidden. Its hardware profile is unchanged —
no extra block device unless `--debug` was asked for.

## Verify

```bash
omnidroid build-devkit --arch arm         # build the devkit disk (once)
omnidroid root-base --base arm            # bake root: system auto-verified; /data grant
                                     #  headless if it can, else the manual step above
omnidroid start dbg --debug               # boot with the devkit attached
omnidroid adb dbg -- shell /debug_ramdisk/su 0 id      # expect uid=0(root) once granted
# frida / hiding are then driven by the omni-* tools via the resolved su.
```

## Status on this checkout (2026-08-06)

- **arm base — ROOTED and headless (DONE).** `base_arm` is registered
  `"rooted": true` on the real production zram+baked-Roblox image:
  `base_arm_system_rooted.qcow2` (thin overlay, Magisk-patched boot) +
  `base_arm_data_rooted.qcow2` (shell su granted **Forever**, Zygisk + Enforce
  DenyList, `com.roblox.client` on the DenyList). Verified via `omnidroid start`:
  boots in ~0.8 min, `su 0 sh -c id` → `uid=0 context=u:r:magisk:s0` with **no
  prompt**, Roblox stays baked (`/product/app/Roblox`), DenyList lists it, and
  props read `release-keys` / `verifiedbootstate=green` (presents as unrooted).
- **arm devkit** (`base_arm_devkit.qcow2`) — built; attaches on `--debug`.
- **x86 devkit** (`base_x86_devkit.qcow2`) — built (x86_64 frida + Magisk).
- **x86 root** — the Bliss initrd Magisk-patch is a separate x86-host build step
  (`base_x86_rooted.initrd.img`, auto-registered when present); not built here.

### How the one-time grant was captured (headless)

The MagiskSU grant that produced `base_arm_data_rooted.qcow2` was done fully
headlessly over adb. The load-bearing order: **set Superuser access = "Apps and
ADB" and Automatic response = "Grant" BEFORE ever firing `su`.** Firing `su`
first records a sticky *deny* policy for the shell (uid 2000) that overrides the
Grant default forever after. Full sequence: boot rooted system + fresh prod
/data + devkit → `pm install` the Magisk app → tap its "additional setup" OK
(populates `/data/adb/magisk` + reboots) → set the two settings → reboot → the
first `su` auto-grants → write the Forever policy (`magisk --sqlite` +
`--denylist add`) → capture that `/data`. This is the accepted checkpoint that
ships. (An interactive GRANT tap on `SuRequestActivity` also works, but the
guest ANR-storms during first-boot dexopt make blind taps unreliable; the
Grant-before-su path avoids the dialog entirely.)

## The tools on the devkit disk

Source for the scripts is `devkit/` (see `devkit/README.md`); binaries are
fetched at build time.

| Path on the disk | What |
|------------------|------|
| `/frida-server` | native-arch frida-server, pinned version. |
| `/frida-server-patched` | *optional* anti-detection build — drop one in and `omni-fridad` prefers it. |
| `/magisk.apk` | the Magisk installer/manager APK. |
| `/bin/magiskboot`, `/bin/magiskinit`, `/bin/magiskpolicy`, `/bin/busybox` | Magisk multicall binaries (used by `omnidroid root-base`). |
| `/bin/boot_patch.sh`, `/bin/util_functions.sh` | Magisk's boot-image patch scripts. |
| `/omni-fridad` | start frida-server **hidden** (custom loopback port, randomized process name). |
| `/omni-frida-stop` | stop the devkit frida-server. |
| `/omni-hide` | hide root + Magisk + frida from a target app (Magisk DenyList + resetprop). |
| `/omni-magisk-setup` | one-time: enable Zygisk + Enforce DenyList (+ Shamiko/manager if present). |
| `/manifest.json` | versions, hidden frida port, mount paths. |

## su + hiding gotchas (handled for you)

1. **`su` is not on `$PATH`.** This LineageOS is all-read-only, so Magisk keeps
   `su` in its own tmpfs at `/debug_ramdisk/su`. The engine + agent probe
   `/debug_ramdisk/su` → `/sbin/su` → `su` (`resolve_su` / `_resolve_su`).
2. **MagiskSU prompts on first su request.** The rooted `/data`
   (`base_arm_data_rooted.qcow2`) pre-grants the adb shell **Forever**, so root
   works headlessly from first boot — no prompt, no taps.
3. **Hiding.** `_enforce_hiding` re-asserts the hiding on every boot and adapts
   to whether **Shamiko** (module id `zygisk_shamiko`) is installed:
   - **With Shamiko** (the shipped arm base — baked into
     `base_arm_data_rooted.qcow2`): Zygisk ON, the game on the DenyList, and
     DenyList **enforcement OFF** — Shamiko reads the list itself and *requires*
     enforcement disabled (per its README). Shamiko hides Magisk, Zygisk, and
     its modules more thoroughly than Enforce-DenyList alone.
   - **Without Shamiko**: Zygisk ON + Enforce DenyList ON (Magisk's own hiding).

   The enforce flag is the only thing that flips on Shamiko's presence, and
   `_enforce_hiding` sets it correctly every boot (otherwise Magisk's enforce
   would silently turn back on and break Shamiko). `Shamiko-v1.2.5-414.zip` is
   kept in the images dir; reinstall with
   `su 0 magisk --install-module Shamiko.zip` then set `denylist=0`.
