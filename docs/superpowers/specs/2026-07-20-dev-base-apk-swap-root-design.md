# Dev base fit for bootstrapped-APK testing (root + swap-any-signature)

**Date:** 2026-07-20
**Repo:** omnidroid (base + engine). Prerequisite for the omni-agent CLI sync
(`omni-agent/docs/superpowers/specs/2026-07-20-omniagent-omnidroid-cli-sync-design.md`)
and for exercising sub-project B's `start --dev --apk` on real hardware (B Task 4).
**Status:** Approved direction; execute deliberately with backups + verification gates.

## Problem (root-caused on-device 2026-07-20)

`OMNI_DEV_MODE=1 omnidroid start <acct> --dev --apk <bootstrapped.apk>` fails to install
the build. Two independent causes, both confirmed on the live dev guest:

1. **Roblox is a baked SYSTEM app** on the dev base: `pm path com.roblox.client` →
   `/product/app/Roblox/Roblox.apk`, `pkgFlags=[ SYSTEM HAS_CODE ]`, signed with Roblox's
   official cert `ff081c2e` (v3), v2.726.1142. A resigned bootstrapped APK (omni-agent's
   `ANDROIDD` cert) hits `INSTALL_FAILED_UPDATE_INCOMPATIBLE: signatures do not match`, and a
   system app cannot be plain-uninstalled (`DELETE_FAILED_INTERNAL_ERROR`).
   `pm uninstall --user 0` was tested and does **not** help — PMS retains the system package's
   signature as the authority, so the resigned install still fails.
2. **The dev base is not rooted**: live guest has no `su` (`/debug_ramdisk/su`, `/sbin/su`,
   `/system/bin/su` all absent), no Magisk package, `ro.build.type=user`. `_devkit_activate`
   reports `su denied/missing`. The dev system overlay was never Magisk-boot-patched
   (`devkit_manifest.rooted=false`, "root pending: --patch-boot"). So frida can't attach and
   root-based image edits are unavailable.

The B spec assumed the dev base was a clean slate for `--apk`; in reality `base_arm_devsystem_v2`
inherited a plain baked Roblox from prod branding, and root was never applied.

## Design decision (and why it reverses the first instinct)

Two ways to make swap work:

- **Option A — no baked Roblox (rebuild/strip the dev system image):** swap becomes a normal
  user-app reinstall, no root required. BUT removing Roblox is *additional* irreversible base
  surgery (rebuild the devsystem overlay from the Roblox-free `base_arm.qcow2` v1, or nbd/guest-
  mount and delete `/product/app/Roblox` then reseal — awkward on macOS, /product may be
  verity/read-only).
- **Option B — keep baked, root force-remove at runtime (CHOSEN):** the boot-patch for root is
  **required anyway** (frida + the whole dev-tools purpose). Once root works, the install path
  removes the baked Roblox on the **ephemeral** overlay each boot (`su → mount -o rw,remount the
  /product mount → rm -rf /product/app/Roblox → pm uninstall → install <apk>`), thrown away on
  stop. This adds the **least** irreversible surgery (only the unavoidable boot-patch) and keeps
  the base image otherwise untouched.

**Chosen: Option B.** Rationale: the boot-patch is unavoidable; given it, runtime force-remove is
free and reversible (ephemeral), whereas Option A layers extra base surgery on top. Option A
remains the documented fallback (see the gate below).

**Load-bearing assumption (must be verified before building the feature):** that *with root*,
removing the system Roblox APK + clearing the package actually lets a differently-signed
`com.roblox.client` install. This could not be tested (no root on the current boot). The plan
gates on proving it on-device the moment root is available; if it fails, fall back to Option A.

## The one required base mutation: root the dev boot

`omnidroid build-dev-base --patch-boot` runs `_patch_dev_boot`: extracts the boot partition from the
dev system overlay, magiskboot-patches it inside a throwaway arm guest, writes it back. It is
**brick-risky** (edits ~2 GB image in place) and, as-is, has caveats this plan must handle:

- It **re-registers** the dev base to `system: base_arm_devsystem.qcow2` (the **non-v2** file),
  switching away from the branded `devsystem_v2` you run today. The plan must reconcile this:
  decide the canonical dev system file and ensure the one that gets patched is the one the dev
  base points at, with branding intact.
- It writes the patched image **in place with no fresh backup**. The plan takes a **verified
  backup first** (checksum) of whatever devsystem file will be patched.
- `base_arm.qcow2` (the shared, immutable base_disk, v1, **no Roblox**) is never touched — Roblox
  lives only in the devsystem overlay, so no shared-base risk.

## Engine change (omnidroid): dev-only force-install

Extend the install path used by `start --dev --apk` (`_install_apk` / its recovery) so that, on a
**rooted dev** account only, when a normal install is blocked by a system-app signature conflict:

1. resolve working `su` (`_magisk_su`, already exists);
2. `su -c` remount the /product mount read-write and `rm -rf /product/app/Roblox` (+ any stale
   data), then `pm uninstall com.roblox.client` (now unbacked → should succeed);
3. install the given APK (`_abi_install`), signature-agnostic — resigned **or** unsigned/any build;
4. surface a clear error if root is required but unavailable (never silently fall through).

This is strictly dev-gated (`acct_is_dev` + working su); prod and the non-dev `install` path are
untouched. It composes with B's existing pin/sig recovery.

## Safe execution sequence (each step gated)

1. **Backup** the dev system overlay(s) to be touched — verified by checksum. (Started already for
   `devsystem_v2`.)
2. **Reconcile the devsystem file** (v2 vs non-v2): confirm which the dev base should use, ensure
   branding, and that `build-dev-base --patch-boot` patches *that* file (adjust config/flow if the
   re-register would switch it).
3. **Patch boot** (`--patch-boot`) on the backed-up target. Verify on a **real boot**: `su -c id`
   returns uid 0, Magisk present, frida attaches on the hidden port.
4. **On-device GATE (the load-bearing test):** with root, manually run the force-remove +
   resigned-install sequence. **If it installs and runs → Option B confirmed; build the engine
   feature. If it fails → switch to Option A** (rebuild the dev system without baked Roblox) and
   revise this doc.
5. **Implement** the dev force-install engine change (TDD for the unit-testable parts; on-device
   for the root path).
6. **End-to-end verify:** `start --dev --apk <bootstrapped>` → `OmniBootstrap: session cookie
   installed` in logcat + in-game screenshot (logged in); repeat with a second/different build to
   prove continuous multi-version swap; plain APK → loud `not_logged_in`.
7. **omni-agent CLI sync** (its own spec) + the **handoff prompt**.

## Verification

- Root: `su` uid 0 + frida attach on a fresh dev boot.
- Swap: two successive `start --dev --apk` runs with differently-signed builds both install + run.
- Login: the `OmniBootstrap` logcat line + in-game screenshot (the B success signal), proving the
  bootstrapped APK logs in on dev exactly as prod does.
- Prod untouched: prod `start` (no `--dev`/`--apk`) unchanged; the force-install path is dev-gated.

## Out of scope / boundary

- No change to prod, to `base_arm.qcow2` (v1), or to the non-dev install path.
- Bootstrap METHOD unchanged (omni-agent's smali-inject chain).
- Option A (no-baked-Roblox rebuild) is the documented fallback, not the primary path.
