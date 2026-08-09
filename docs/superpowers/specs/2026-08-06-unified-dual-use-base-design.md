# Unified dual-use base — remove the dev base

**Date:** 2026-08-06
**Status:** approved, implementing

## Goal

Delete the separate `dev` base. The shipped `arm` and `x86` bases become
**dual-use**: they ship to production *and* omni-agent can debug on them.

## The reframe

"dev" today fuses three separable capabilities into one base tag. Splitting
them is the whole design:

| Capability | Today | After |
|---|---|---|
| **Root** (Magisk-patched boot) | dev base only | baked into every shipped base, always on |
| **Hiding** (Zygisk + Enforce DenyList) | manual `omni-magisk-setup`, dev only | baked into the shipped `/data` + re-enforced every boot, active in production |
| **Toolkit** (frida-server, `omni-*` scripts) | the vdc devkit disk, welded to the dev base | an attachable disk, opt-in per boot |

`dev` therefore stops being a **base** and becomes a **boot option**:
`omnidroid start <acct> --debug` / agent `debug=true` attaches the devkit as vdc.
The base underneath is the same production image either way.

This kills the drift that made today's dev base a generation behind
production (dev = `base_arm` v1, no zram, no baked Roblox; prod =
`base_arm_v2` + `base_arm_system_zram`).

## Removed

`DEV_BASE_TAG`, `base_is_dev`, `acct_is_dev`, `dev_mode_enabled`,
`assert_dev_allowed`, `visible_bases`, `_dev_mode_for_play`, `OMNI_DEV_MODE`,
`OMNI_USE_DEV_BASE`, the `dev` entry in `configs/paths.json`, `start --dev`,
the dev-only gating on `start --apk` and on auto-screenshots, and the
`dev_base_locked` failure code.

`build-dev-base` splits into two orthogonal commands:

- `omnidroid build-devkit [--arch arm|x86]` — builds the attachable toolkit disk.
- `omnidroid root-base <tag>` — Magisk-patches a base's system overlay to root it.

Separately, `spawn_qemu(acct, cfg, dev=...)` is a *different* "dev": it means
**interactive-window boot profile**, not dev base. Renamed to `interactive`
so the two never get confused again.

## Image layout (arm)

The existing `_patch_dev_boot` exports the disk to raw, patches, and
re-imports — which **flattens** the qcow2. That is why
`base_arm_devsystem.qcow2` is a standalone 1.16 GB file with no backing chain
(`DEV-BASE.md` claims it stays COW-backed; the file on disk disagrees).
Applying that to production would destroy the thin-overlay lineage, so the
patch writer changes.

```
base_arm_v2.qcow2                        (backing, untouched)
└── base_arm_system.qcow2                (untouched)
    └── base_arm_system_zram.qcow2       (untouched — today's prod system)
        └── base_arm_system_rooted.qcow2 NEW, thin: only the patched vda6
base_arm_data_rooted.qcow2               NEW: prod /data + Magisk policy
base_arm_devkit.qcow2                    unchanged; no longer in any base entry
```

The patched boot is written **through a throwaway guest**: the new overlay is
attached as a raw virtio block device and the patched image is `dd`'d to the
GPT partition offset, so writes land in the overlay and it stays thin instead
of flattening.

`base_arm_data_rooted` rebuilds the proven `base_arm_devdata` recipe
(`root_access=3`, `zygisk=1`, `denylist=1`, adb shell granted **Forever** —
this is what makes root headless with no GRANT dialog) on top of *current*
prod `/data`, plus `com.roblox.client` pre-added to the DenyList.

x86 mirrors this: `base_x86_devkit.qcow2` (x86_64 frida-server + Magisk
x86_64) and a rooted Bliss boot.

## Every boot, production included

1. Boot the rooted image.
2. `_enforce_hiding()` — idempotent, needs only `su` (the `magisk` CLI lives in
   the rooted boot; **no devkit disk required**): assert Zygisk + Enforce
   DenyList, `magisk --denylist add com.roblox.client`, resetprop the classic
   root/verified-boot props. Non-fatal on failure, logged loudly.
3. Kiosk foregrounded + Magisk manager force-stopped — now unconditional.
4. Only with `--debug`: vdc attached at spawn, then `_devkit_activate` stages
   frida + the `omni-*` tools exactly as today.

Production instances differ from today in exactly one observable way: root
exists and is hidden. The hardware profile is unchanged — no extra block
device unless debug was requested.

## Compatibility

Code must not hard-require the rooted images. A base entry carries an optional
`"rooted": true` marker; when the rooted files are absent the engine registers
and boots the unrooted images exactly as today, and root-needing operations
report `root_unavailable` instead of failing the boot. This keeps Phase 1
shippable before Phase 2 produces the images.

## Risks (accepted)

- Root in production means detection rests entirely on Magisk's hiding.
  Recommend dropping `Shamiko.zip` into the shipped image (the devkit already
  supports it) rather than relying on DenyList alone.
- The zram/balloon-1024 tuning was measured on an **unrooted** instance.
  Rooting adds magiskd + Zygisk to every zygote, so Phase 2 re-runs
  `omnidroid measure`.
- x86 cannot be boot-verified on an Apple Silicon host except under very slow
  TCG. The x86 half ships code-complete and image-built; live verification
  happens on the x86 box.

## Phases

1. **Code** — dev-base removal, `--debug`, `interactive` rename, agent
   rewiring, contract + docs, full test suite. Verifiable offline.
2. **arm images** — `base_arm_system_rooted` + `base_arm_data_rooted`,
   boot-verified locally (root, hiding, Roblox launch, frida under `--debug`).
3. **x86** — devkit + rooted Bliss boot; code-complete, verified by the user.
