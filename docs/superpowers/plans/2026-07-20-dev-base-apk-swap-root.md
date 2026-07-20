# Dev base fit for bootstrapped-APK testing (root + swap-any-signature) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (recommended for THIS plan — several tasks are on-device, brick-risky, and need in-session judgment) or superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make the arm dev base able to install and run a resigned (or unsigned) Roblox build via `omni start --dev --apk`, by rooting the dev boot (Magisk + frida) and force-removing the baked system Roblox at install time on the ephemeral overlay.

**Architecture:** One required base mutation (Magisk-patch the dev boot) + a dev-only, root-gated force-install in the engine's install path. The install path, on a rooted dev account, remounts `/product`, removes the baked `com.roblox.client` system app, then installs the given APK regardless of signature. Prod and the non-dev install path are untouched. Everything the guest does is thrown away on stop (`snapshot=on`).

**Tech Stack:** Python 3.13, QEMU arm64 (Apple Silicon, HVF), Magisk (magiskboot boot patch), adb, pytest.

## Global Constraints

- **This host:** macOS Apple Silicon. Every `omni` invocation for dev needs `OMNI_DEV_MODE=1` and `OMNI_IMAGES_DIR=/Users/berat/OmniImages` (images live there; the darwin config falls back to `~/OmniImages`). Run `omni` as `python3 -m omnidroid` from the repo root, or the installed `omnidroid`.
- **Base mutations are brick-risky and gated:** never patch a base file that is not first backed up and checksum-verified. `base_arm.qcow2` (the shared immutable base_disk, v1, no Roblox) is NEVER modified.
- **Dev-gated only:** the force-install path runs solely when `acct_is_dev(acct)` is true AND working `su` is present. Prod (`start` without `--dev`) and the standalone `omni install` command stay byte-identical.
- **Load-bearing assumption, gated in Task 3:** that with root, removing the system Roblox APK + clearing the package lets a *differently-signed* `com.roblox.client` install. If Task 3 disproves it, STOP and switch to the fallback (rebuild the dev system without baked Roblox) — see Task 3.
- **Ephemeral preserved:** the dev instance boots `snapshot=on`; the force-remove + install are per-boot and discarded on stop.
- Test (unit): `python3 -m pytest tests/ -q` from the repo root (baseline 131 passing).
- Verified backup already taken: `~/OmniImages/base_arm_devsystem_v2.qcow2.safebak-20260720` (sha256 matches the live file).

## Known state (from on-device root-cause, 2026-07-20)

- Dev base config `dev`: `base_disk=base_arm.qcow2` (v1, no Roblox), `system=base_arm_devsystem_v2.qcow2` (branded, **has** baked Roblox in `/product/app/Roblox`, signed `ff081c2e`), `data=base_arm_devdata.qcow2`, `devkit=base_arm_devkit.qcow2`, `rooted=false`.
- `build_dev_base()` (engine.py:2893) re-registers dev to `system=base_arm_devsystem.qcow2` (the **non-v2** constant `ARM_DEVSYSTEM_DISK`, engine.py:243) and, if that file exists, **skips rebuild** and patches it in place. Both `base_arm_devsystem.qcow2` and `..._v2.qcow2` exist (2.26 GB each).
- `_patch_dev_boot()` writes the patched boot back in place, **no backup**.
- Engine helpers to reuse: `_magisk_su(acct)` (engine.py:1810, returns working su path or None), `acct_is_dev(acct)` (engine.py:175), `_install_apk(acct, apk_path, label, abi, no_abi_pin)` (engine.py:4420, B's factored installer), `_abi_install`, `_install_needs_clean_replace`, `adb(acct, *args)`.

## File Structure

```
~/OmniImages/                         # base image store (mutations here, backed up)
omnidroid/omnidroid/engine.py         # Task 1 (reconcile patch target), Task 4 (_dev_force_remove_game + _install_apk hook)
omnidroid/tests/test_session.py       # Task 4 unit tests (dev-gated force-install decision + command construction)
docs/superpowers/plans/...            # this plan (progress checkboxes)
```

No new modules; a focused helper added next to `_install_apk`.

---

### Task 1: Reconcile the patch target (make `build-dev-base --patch-boot` patch the branded dev system, backed up)

`build-dev-base --patch-boot` would patch `base_arm_devsystem.qcow2` (non-v2) and re-point the dev base at it. We want the patched, rooted system to be the **branded** one you run today (`devsystem_v2`). This task makes the canonical `base_arm_devsystem.qcow2` hold the branded content (backed up), so the patch + re-register land on the right image.

**Files:** none (image + config staging via shell). No engine edit if the copy approach works; if `build_dev_base` needs a flag to target a specific system file, add it here.

- [ ] **Step 1: Back up the non-v2 file too** (the one build-dev-base will overwrite/patch).

```bash
cd ~/OmniImages
[ -f base_arm_devsystem.qcow2.safebak-20260720 ] || cp base_arm_devsystem.qcow2 base_arm_devsystem.qcow2.safebak-20260720
ls -la base_arm_devsystem*.safebak-*
```
Expected: two `.safebak-20260720` files (the v2 backup from setup + this non-v2 one).

- [ ] **Step 2: Make the canonical patch target = the branded content.** Overwrite the non-v2 file with the branded v2 content (both already backed up), so whichever file build-dev-base patches/registers is the branded one.

```bash
cd ~/OmniImages
cp base_arm_devsystem_v2.qcow2 base_arm_devsystem.qcow2
shasum -a 256 base_arm_devsystem.qcow2 base_arm_devsystem_v2.qcow2
```
Expected: the two sha256 values are IDENTICAL (canonical == branded).

- [ ] **Step 3: Confirm the dev base still boots the branded system and Roblox is present** (baseline before patching), and that root is still absent (nothing changed yet).

```bash
export OMNI_DEV_MODE=1 OMNI_IMAGES_DIR=/Users/berat/OmniImages
# point dev base system at the canonical file if config differs:
python3 - <<'PY'
import json,os
from pathlib import Path
p=Path("omnidroid/configs/paths.json"); c=json.loads(p.read_text())
c["bases"]["dev"]["system"]="base_arm_devsystem.qcow2"
p.write_text(json.dumps(c,indent=2)); print("dev.system ->", c["bases"]["dev"]["system"])
PY
python3 -m omnidroid start admn1b12farm3 --dev --no-window --json 2>&1 | tail -3
python3 -m omnidroid adb admn1b12farm3 shell "pm path com.roblox.client; su -c id 2>&1 | head -1" 2>&1 | head -3
python3 -m omnidroid stop admn1b12farm3 2>&1 | tail -1
```
Expected: `package:/product/app/Roblox/Roblox.apk`; `su: not found`/denied (still unrooted). Boot works on the canonical branded system.

- [ ] **Step 4: Commit the config change** (dev.system now names the canonical file).

```bash
cd omnidroid && git add configs/paths.json && git commit -m "chore(dev-base): dev.system -> canonical base_arm_devsystem.qcow2 (branded)"
```

---

### Task 2: Patch the dev boot for Magisk root + frida (the one required base mutation)

**Files:** `~/OmniImages/base_arm_devsystem.qcow2` (patched in place — backed up in Task 1).

- [ ] **Step 1: Confirm free disk headroom** (the offline patch needs ~2 GB scratch).

```bash
df -h ~/OmniImages | tail -1
```
Expected: well over 4 GB free.

- [ ] **Step 2: Run the boot patch.**

```bash
export OMNI_DEV_MODE=1 OMNI_IMAGES_DIR=/Users/berat/OmniImages
python3 -m omnidroid build-dev-base --patch-boot --json 2>&1 | tail -20
```
Expected: staging + `_patch_dev_boot` output ending with the dev base registered `[rooted]` (`devkit_manifest.rooted=true` in the JSON). If it prints `Overlay left UNROOTED` or any patch failure, STOP — restore from backup (`cp base_arm_devsystem.qcow2.safebak-20260720 base_arm_devsystem.qcow2`) and report; do not proceed.

- [ ] **Step 3: Verify root on a real boot** (the patch is only trustworthy verified live).

```bash
python3 -m omnidroid start admn1b12farm3 --dev --no-window --json 2>&1 | tail -3
python3 -m omnidroid adb admn1b12farm3 shell 'su -c id' 2>&1 | head -2
```
Expected: `uid=0(root) gid=0(root)` (root works). If `su: not found`/denied, STOP — the patch did not take; restore backup and report.

- [ ] **Step 4: Verify frida can attach** (root's other purpose).

```bash
python3 -m omnidroid adb admn1b12farm3 shell 'su -c "ls -l /debug_ramdisk/su; getprop ro.build.type"' 2>&1 | head
# frida server is launched by _devkit_activate on the hidden port; confirm it is present
python3 -m omnidroid adb admn1b12farm3 shell 'su -c "ps -A | grep -i frida" ' 2>&1 | head
```
Expected: su binary present; a frida-server process (or the omni-fridad launcher available). Leave the instance running for Task 3.

---

### Task 3: On-device GATE — prove root force-remove enables a resigned install

This is the load-bearing test. Do it manually on the live rooted guest BEFORE writing any engine code. It decides Option B vs the fallback.

**Files:** none (on-device proof).

- [ ] **Step 1: Identify the /product mount and confirm the baked Roblox is there.**

```bash
export OMNI_DEV_MODE=1 OMNI_IMAGES_DIR=/Users/berat/OmniImages
python3 -m omnidroid adb admn1b12farm3 shell 'su -c "mount | grep -E \" /product \"; ls -la /product/app/Roblox"' 2>&1 | head
```
Expected: a `/product` mount line + `Roblox.apk` present.

- [ ] **Step 2: Force-remove the baked Roblox as root, then clear the package.**

```bash
python3 -m omnidroid adb admn1b12farm3 shell 'su -c "
  mnt=$(mount | awk \"\$3==\\\"/product\\\"{print \$1}\" | head -1);
  mount -o rw,remount /product 2>&1 || mount -o rw,remount $mnt /product 2>&1;
  rm -rf /product/app/Roblox && echo removed-apk;
  pm uninstall com.roblox.client 2>&1 | head -1;
  pm path com.roblox.client 2>&1 | head -1 || echo pkg-gone
"' 2>&1 | head
```
Expected: `removed-apk`; `Success` (or the package no longer resolves); `pm path` returns nothing/`pkg-gone`.

- [ ] **Step 3: Install the resigned bootstrapped APK — the decisive check.**

```bash
APK="/Users/berat/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk"
python3 -m omnidroid adb admn1b12farm3 install -r -g --no-incremental "$APK" 2>&1 | tail -3
python3 -m omnidroid adb admn1b12farm3 shell 'dumpsys package com.roblox.client | grep -E "codePath|signatures|versionName" | head' 2>&1 | head
```
Expected (Option B CONFIRMED): `Success`; `codePath=/data/app/...` (now a user app), `signatures` = the resigned cert (NOT `ff081c2e`).
If it STILL fails `signatures do not match`: **Option B is falsified.** STOP. Record the failure in this plan, switch to the fallback below, and revise the spec/plan.

- [ ] **Step 4: If confirmed — quick login sanity** (optional but valuable): let the kiosk launch it and check for the bootstrap line.

```bash
python3 -m omnidroid adb admn1b12farm3 shell 'monkey -p com.roblox.client -c android.intent.category.LAUNCHER 1' 2>&1 | tail -1
sleep 25
python3 -m omnidroid logcat admn1b12farm3 2>&1 | grep -i "OmniBootstrap" | head
python3 -m omnidroid stop admn1b12farm3 2>&1 | tail -1
```
Expected: `OmniBootstrap: session cookie installed` (proves the whole premise). Absence here is a bootstrap-APK/omni-agent concern, NOT a Task-3 blocker — note it and continue.

- [ ] **Step 5: Record the gate outcome** in this plan (B confirmed / fell back to A) and commit the note.

```bash
cd omnidroid && git commit --allow-empty -m "test(dev-base): Task 3 gate — root force-remove enables resigned install (B confirmed)"
```

**FALLBACK (only if Step 3 failed):** abandon runtime force-remove; rebuild the dev system WITHOUT baked Roblox — `nbd`/guest-mount `base_arm_devsystem.qcow2`, delete `/product/app/Roblox`, reseal, re-verify, then swaps are plain user-app installs (no root needed for swap; keep root for frida). Update the spec `Decision` section and rewrite Task 4 to drop the force-remove.

---

### Task 4: Engine — dev-only, root-gated force-install in the install path

Add a helper that removes the baked game as root, and hook it into `_install_apk`'s existing recovery so a signature-blocked install on a rooted dev account force-removes then reinstalls. TDD the decision logic + command construction (mockable exactly like B's `ApkInstallOnStart` tests); the live root behavior was already proven in Task 3.

**Files:**
- Modify: `omnidroid/omnidroid/engine.py` (add `_dev_force_remove_game`; hook into `_install_apk` recovery)
- Test: `omnidroid/tests/test_session.py` (new class `DevForceInstall`)

**Interfaces:**
- Consumes: `_magisk_su(acct)->str|None` (1810), `acct_is_dev(acct)->bool` (175), `adb(acct,*args)`, `_abi_install`, `_install_needs_clean_replace(out)`.
- Produces: `_dev_force_remove_game(acct, pkg, label)->bool` — as root, remount the game's partition rw and remove its baked APK + clear the package; returns True if root was available and the removal ran. Called from `_install_apk` when a signature/clean-replace failure occurs on a dev account.

- [ ] **Step 1: Write the failing test** — on a dev account with `su` available, when `_abi_install` first returns a signature-mismatch, `_install_apk` calls `_dev_force_remove_game` then retries the install; on a non-dev account it does NOT.

```python
class DevForceInstall(unittest.TestCase):
    """On a ROOTED DEV account, an install blocked by a system-app signature
    mismatch triggers a root force-remove of the baked game, then reinstall.
    Non-dev accounts never take this path (prod install unchanged)."""

    def _acct(self, dev=True):
        return {"name": "admn1b12farm3", "adb_port": 1, "vnc_port": 1,
                "base": "dev" if dev else "arm", "game_package": omni.ROBLOX_PACKAGE}

    def test_dev_sig_mismatch_forces_remove_then_reinstalls(self):
        acct = self._acct(dev=True)
        mismatch = SimpleNamespace(stdout="", stderr="INSTALL_FAILED_UPDATE_INCOMPATIBLE: signatures do not match", returncode=1)
        ok = SimpleNamespace(stdout="Success", stderr="", returncode=0)
        with mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "apk_package_name", return_value=omni.ROBLOX_PACKAGE), \
             mock.patch.object(omni, "acct_is_dev", return_value=True), \
             mock.patch.object(omni, "_magisk_su", return_value="/debug_ramdisk/su"), \
             mock.patch.object(omni, "_abi_install", side_effect=[mismatch, ok]) as abi, \
             mock.patch.object(omni, "_dev_force_remove_game", return_value=True) as frg, \
             mock.patch.object(omni, "installed_primary_abi", return_value="arm64-v8a"), \
             mock.patch.object(omni, "adb"):
            res = omni._install_apk(acct, "/tmp/x.apk", "start admn1b12farm3")
        self.assertTrue(res["ok"])
        frg.assert_called_once()
        self.assertEqual(abi.call_count, 2)   # blocked, then success after force-remove

    def test_non_dev_never_force_removes(self):
        acct = self._acct(dev=False)
        mismatch = SimpleNamespace(stdout="", stderr="INSTALL_FAILED_UPDATE_INCOMPATIBLE: signatures do not match", returncode=1)
        with mock.patch.object(omni, "adb_connect"), \
             mock.patch.object(omni, "apk_package_name", return_value=omni.ROBLOX_PACKAGE), \
             mock.patch.object(omni, "acct_is_dev", return_value=False), \
             mock.patch.object(omni, "_dev_force_remove_game") as frg, \
             mock.patch.object(omni, "_abi_install", return_value=mismatch), \
             mock.patch.object(omni, "adb"):
            res = omni._install_apk(acct, "/tmp/x.apk", "install admn1b12farm3")
        frg.assert_not_called()
        self.assertFalse(res["ok"])
```

- [ ] **Step 2: Run — FAIL.**

Run: `python3 -m pytest tests/test_session.py -q -k DevForceInstall`
Expected: FAIL (`_dev_force_remove_game` undefined / not called).

- [ ] **Step 3: Implement the helper** (engine.py, next to `_install_apk`).

```python
def _dev_force_remove_game(acct, pkg, label):
    """DEV+root only: remove a baked/system copy of `pkg` on the EPHEMERAL
    overlay so a differently-signed build can install. Remounts the package's
    partition read-write, deletes its code dir, then clears the package from
    PMS. Returns True if root was available and the removal ran; False if no su
    (caller then surfaces a clear error). Thrown away on stop (snapshot=on)."""
    su = _magisk_su(acct)
    if not su:
        return False
    # Find the baked apk dir (e.g. /product/app/Roblox) and its mount, remount
    # rw, delete it, and uninstall so PMS drops the retained system signature.
    script = (
        f'p=$(pm path {pkg} 2>/dev/null | sed "s/package://;s#/[^/]*$##" | head -1); '
        f'[ -n "$p" ] && m=$(df "$p" 2>/dev/null | awk "NR==2{{print \\$1}}") ; '
        f'mount -o rw,remount "${{m:-/product}}" 2>/dev/null; '
        f'[ -n "$p" ] && rm -rf "$p"; '
        f'pm uninstall {pkg} 2>&1 | head -1; '
        f'pm uninstall --user 0 {pkg} 2>/dev/null | head -1; true'
    )
    try:
        adb(acct, "shell", su, "-c", script, timeout=60)
        print(f"[{label}] dev: force-removed baked {pkg} (root) for a resigned install")
        return True
    except Exception as e:  # noqa: BLE001
        print(f"[{label}] dev: force-remove of {pkg} failed: {e}")
        return False
```

- [ ] **Step 4: Hook it into `_install_apk`'s recovery.** In the existing `if "Success" not in out and _install_needs_clean_replace(out) and pkg:` recovery block, BEFORE the plain force-stop/uninstall path, add the dev-root branch:

```python
    if "Success" not in out and _install_needs_clean_replace(out) and pkg:
        if acct_is_dev(acct) and _magisk_su(acct):
            # DEV: the block is usually the baked SYSTEM Roblox (different
            # signature, unremovable without root). Force-remove it as root on
            # the ephemeral overlay, then reinstall the given build as-is.
            if _dev_force_remove_game(acct, pkg, label):
                r = _abi_install(acct, apk_path, abi)
                out = (r.stdout + r.stderr).strip()
                print(f"[{label}] reinstall after dev force-remove: {out}")
        if "Success" not in out:
            # ...existing non-dev kiosk-pin/uninstall recovery unchanged...
```
(Keep the existing recovery intact as the fallback for the non-system/non-dev case.)

- [ ] **Step 5: Run tests — PASS + full suite green.**

Run: `python3 -m pytest tests/test_session.py -q -k DevForceInstall` then `python3 -m pytest tests/ -q`
Expected: the two new tests pass; full suite 133 passed (131 + 2).

- [ ] **Step 6: Commit.**

```bash
git add omnidroid/engine.py tests/test_session.py
git commit -m "feat(dev-apk): root force-remove baked game on dev so start --dev --apk swaps any signature"
```

---

### Task 5: End-to-end on-device verification

Prove the whole path on the rooted dev base with the engine change in place.

**Files:** none (verification; fix-forward any issue).

- [ ] **Step 1: Clean boot + one-shot install of the resigned bootstrapped build via the engine path.**

```bash
export OMNI_DEV_MODE=1 OMNI_IMAGES_DIR=/Users/berat/OmniImages
APK="/Users/berat/Desktop/overnight tests/update test/roblox-v2.726-bootstrap.apk"
python3 -m omnidroid start admn1b12farm3 --dev --apk "$APK" --no-window --json 2>&1 | tail -5
```
Expected: JSON `ok:true`, no `apk_install_failed`, no `not_logged_in` (the force-remove made the resigned install succeed and the bootstrap logged in).

- [ ] **Step 2: Confirm the login signal + in-game screenshot.**

```bash
python3 -m omnidroid logcat admn1b12farm3 2>&1 | grep -i "OmniBootstrap: session cookie installed" | head
python3 -m omnidroid screenshot admn1b12farm3 --json 2>&1 | tail -2
```
Expected: the `OmniBootstrap` line present; screenshot path emitted (open it: in-game as the account, not Sign In).

- [ ] **Step 3: Prove continuous multi-version swap** — stop (wipes), boot again, install a *different* build; it must also install cleanly.

```bash
python3 -m omnidroid stop admn1b12farm3 2>&1 | tail -1
APK2="/Users/berat/Desktop/overnight tests/update test/omni_build/admn1b12farm3_build.apk"
python3 -m omnidroid start admn1b12farm3 --dev --apk "$APK2" --no-window --json 2>&1 | tail -3
python3 -m omnidroid stop admn1b12farm3 2>&1 | tail -1
```
Expected: second build installs `ok:true` on a fresh ephemeral boot — no accumulation, no signature block.

- [ ] **Step 4: Prove the loud failure still fires** — a PLAIN (non-bootstrapped) Roblox must report `not_logged_in`.

```bash
PLAIN="/Users/berat/Desktop/Omni Apps/omni-exec-android/roblox.apk"
python3 -m omnidroid start admn1b12farm3 --dev --apk "$PLAIN" --no-window --json 2>&1 | tail -3
python3 -m omnidroid stop admn1b12farm3 2>&1 | tail -1
```
Expected: installs (force-remove works) but result `ok:false, error:"not_logged_in"` (plain Roblox lacks OmniBootstrap) — the silent-Sign-In bug stays closed.

- [ ] **Step 5: Commit the verification note.**

```bash
cd omnidroid && git commit --allow-empty -m "test(dev-base): end-to-end dev --apk swap + login verified on rooted dev base"
```

---

## Verification (whole-plan)

1. Root on a fresh dev boot: `su -c id` → uid 0; frida attaches.
2. `start --dev --apk <bootstrapped>` → `ok:true`, `OmniBootstrap` logcat line, in-game screenshot.
3. Two successive differently-signed builds both install cleanly (continuous multi-version testing).
4. Plain Roblox via `--apk` → loud `not_logged_in`.
5. Unit suite green (133). Prod path + standalone `omni install` unchanged (dev-gated force-remove).

## Follow-on (separate plan)

Once this is green, execute the **omni-agent CLI sync** (spec `omni-agent/docs/superpowers/specs/2026-07-20-omniagent-omnidroid-cli-sync-design.md`): fix `play`→`start` / `--ephemeral` / `--wait` / `session --show`, collapse `launch_roblox_build`'s apk path to the `start --dev --apk` one-shot, then produce the **handoff prompt** for the agent to convert a plain Roblox → bootstrapped and log in end-to-end.
