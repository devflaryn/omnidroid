# Dev base root — rehearsal-first landing (amends the apk-swap/root spec)

**Date:** 2026-07-20
**Repo:** omnidroid (base + engine).
**Amends:** `2026-07-20-dev-base-apk-swap-root-design.md` — that spec's design decision
(Option B: keep baked Roblox, root force-remove at runtime) is UNCHANGED. This document
replaces its *execution sequence* (steps 1–4) with a rehearsal-first one, and records state
verified on-device 2026-07-20.
**Status:** Approved direction.

## User-facing goal (the acceptance statement)

A dev image that:

1. boots **rooted** — `su` works, so arbitrary commands / frida / file edits are possible; and
2. accepts **any APK** via `omni start <account> --dev --apk <file>`, replacing the built-in
   Roblox **regardless of signing cert**, repeatably across boots.

Both routes below deliver this. Which route we take is an implementation detail.

## Verified state (2026-07-20, this host)

- `configs/paths.json` → `bases.dev.system = base_arm_devsystem.qcow2` (hand-edited, uncommitted);
  `devkit_manifest.rooted = false`, notes `[root pending: --patch-boot]`. **Task 2 never landed.**
- Task 1 was done by hand (v2 branding copied to `base_arm_devsystem.qcow2`, `.safebak-20260720`
  taken for both it and `_v2`). Its **code** reconcile did NOT land — no commit after `ea63866`.
- `base_arm_devsystem.qcow2` and `_v2` are **standalone** (no backing file, 2.1 GiB), matching
  `_brand_target`'s note that `--patch-boot` flattens them. `engine.py:241`'s "COW on
  base_arm.qcow2" comment is stale for these files.
- `base_arm_devsystem.qcow2.bak` is **1,160,968,192 B = 1.08 GiB** — exactly the flattened size
  `_brand_target` attributes to a completed `--patch-boot`. **Hypothesis: it is already rooted**
  (unbranded, which is why it was superseded). Unverified.
- Base images ARE materialized on this host (contradicts the older B-Task-4 blocker note).

## Design

### Phase 0 — Rehearse on a disposable copy

`.bak` is the only possibly-rooted artifact; treat it as read-only reference. Copy it to
`base_arm_devsystem_probe.qcow2`, register a temporary `devprobe` base (probe image + existing
devdata + devkit), boot a throwaway account.

**Check:** `su -c id` → uid 0, Magisk present, frida attaches on 27142.

- rooted → the patch procedure is proven on this host AND a root guest exists now.
- not rooted → size match was coincidence; skip to Phase 2 and patch the branded image directly
  (the original plan's Task 2). One boot spent, nothing damaged. **Phase 1's gate is not
  skipped — it runs against the branded image once Phase 2 yields root**, and a gate failure
  there still means the Option A fallback.

### Phase 1 — The load-bearing gate, on the probe

Prove that **with root, a differently-signed `com.roblox.client` can replace the baked system
app**. On the probe guest: resolve `su` → remount the `/product` mount rw → `rm -rf
/product/app/Roblox` → `pm uninstall com.roblox.client` → install the test APK.

**Decoupling (important):** the gate needs NO bootstrapped APK. It asks a *signature* question
only. Use a **resign-only** artifact: `adb pull /product/app/Roblox/Roblox.apk`, run it through
omni-agent's `sign_apk` (ANDROIDD cert), no injection. Same package, same code, different cert.

This removes the "bootstrapped APK does not exist yet" blocker from B Task 4's critical path, and
makes a failure unambiguous: signatures, not the smali-inject chain.

**Outcome:** installs + runs → **Option B confirmed**, proceed. Fails → **Option A fallback**
(rebuild dev system without baked Roblox), chosen having made zero irreversible writes.

### Phase 2 — Land root on the branded image

Only after Phase 0/1. Verify `.safebak-20260720` by checksum, then `--patch-boot` the branded
`base_arm_devsystem.qcow2`, then verify root on a real branded boot.

**Required code fix (unguarded today):** `build_dev_base` rewrites the whole `bases.dev` entry,
including `"base_disk": arm["base_disk"]` — which would silently repoint dev from
`base_arm.qcow2` to `base_arm_v2.qcow2`, and clobber the hand-edited `notes`. Preserve the
existing `base_disk` (and `notes`) on re-registration rather than overwriting from the arm base.
This is the reconcile Task 1 was supposed to make in code.

### Phase 3 — Engine: dev-only, root-gated force-install

Unchanged from the amended spec's "Engine change" section: extend `_install_apk`'s recovery so a
signature-blocked install on a **rooted dev** account force-removes the baked game, then
reinstalls; clear error when root is required but unavailable; strictly dev-gated (`acct_is_dev`
+ working su); prod and the non-dev `install` path untouched.

### Phase 4 — End-to-end, then omni-agent CLI sync

`start --dev --apk <bootstrapped>` → `OmniBootstrap: session cookie installed` in logcat +
in-game screenshot; a second differently-signed build proves repeatable swap; plain APK → loud
`not_logged_in`. Then the omni-agent CLI sync (its own spec,
`omni-agent/docs/superpowers/specs/2026-07-20-omniagent-omnidroid-cli-sync-design.md`) — user
chose root-first sequencing so the sync targets a surface proven on-device.

## Verification

- **Phase 0:** `su -c id` = uid 0 on the probe.
- **Phase 1:** resigned stock Roblox installs and launches after root force-remove.
- **Phase 2:** checksum-verified backup; `su -c id` = uid 0 on the **branded** boot; `bases.dev`
  retains `base_disk: base_arm.qcow2` after re-registration.
- **Phase 3:** unit tests for the force-install decision + command construction (mockable, as in
  B's `ApkInstallOnStart` tests); prod path unchanged.
- **Phase 4:** the acceptance statement above, twice, with two differently-signed APKs.

## Out of scope

- No change to prod, to `base_arm.qcow2`, or to the non-dev install path.
- Bootstrap METHOD unchanged (omni-agent's smali-inject chain). The single-native-binary swap
  finding is NOT adopted here — user chose to keep the resigned-APK install so dev tests the
  same artifact prod ships.
- `.bak` is never written to.
