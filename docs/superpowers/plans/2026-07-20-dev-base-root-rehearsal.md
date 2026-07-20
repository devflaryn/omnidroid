# Dev Base Root — Rehearsal-First Landing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A dev image that boots rooted and accepts any APK via `omnidroid start <account> --dev --apk <file>`, replacing the baked system Roblox regardless of signing cert, repeatably across boots.

**Architecture:** Rehearse root + the load-bearing signature gate on a disposable copy of `base_arm_devsystem.qcow2.bak` (which matches the flattened post-`--patch-boot` size and is likely already rooted) before any brick-risky write to the branded image. Only then patch the branded image, then add a dev-only root-gated force-install to the engine's existing install-recovery chain.

**Tech Stack:** Python 3.13 (`omnidroid` package, stdlib only), QEMU/qcow2 arm-uefi guests, adb, Magisk v30.7, `unittest` + `mock`.

**Spec:** `docs/superpowers/specs/2026-07-20-dev-base-root-rehearsal-design.md`

## Global Constraints

- **Never write to `base_arm_devsystem.qcow2.bak`.** It is the only possibly-rooted artifact on this host. Read-only reference; always work on a copy.
- Prod is untouched: no change to `bases.arm`, to `base_arm.qcow2`, to `base_arm_v2.qcow2`, or to the non-dev `install` path.
- Every engine change is gated on `acct_is_dev(acct)` AND a working `resolve_su(acct)`. Prod must never reach the force-remove path.
- The root helper is `resolve_su(acct)` (`engine.py:1809`) returning a su path or `None`. The spec's `_magisk_su` does **not** exist — do not call it.
- `current_base` is NEVER changed (existing HARD RULE in `build_dev_base`).
- Dev boots require `OMNI_DEV_MODE=1` in the environment AND an explicit `--dev`.
- The CLI entry point is `omnidroid` (from `[project.scripts]`), not `omni`.
- Host has **no `apksigner`/`zipalign`** — any resigning runs inside omni-agent's Docker sandbox (`tools/apk_tools.py:sign_apk`, which uses `/workspace/debug.keystore`, alias `androiddebugkey`).
- Baseline before this plan: 131 tests passing. Never let the suite regress.

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `configs/paths.json` | Base registry. Phase 0 temporarily repoints `bases.dev.system` at the probe image; Task 6 restores it. | 1, 2, 5 |
| `~/OmniImages/base_arm_devsystem_probe.qcow2` | Disposable rehearsal image (copy of `.bak`). Never the final artifact. | 1, 2 |
| `~/OmniImages/base_arm_devsystem.qcow2` | The branded dev system image — the real artifact, patched in Task 5. | 5 |
| `omnidroid/engine.py` | `build_dev_base` re-registration fix (Task 4); `_system_apk_dir` + `_dev_force_remove_game` + `_install_apk` hook (Task 6). | 4, 6 |
| `tests/test_dev_base_registration.py` | **Create.** Pins that re-registering dev preserves `base_disk`/`notes`. | 4 |
| `tests/test_dev_force_install.py` | **Create.** Pins the force-install decision logic + command construction, dev/root gating, prod no-op. | 6 |

Tasks 1, 2, 3, 5, 7 are **on-device** (boot a guest, observe). They have no unit tests — their deliverable is a recorded observation that gates the next task. Tasks 4 and 6 are pure TDD.

---

### Task 1: Probe image + root check (Phase 0)

Answer one question: **is `base_arm_devsystem.qcow2.bak` already rooted?**

**Files:**
- Create: `~/OmniImages/base_arm_devsystem_probe.qcow2` (copy)
- Modify: `configs/paths.json` (temporary repoint, reverted in Task 7)

**Interfaces:**
- Produces: a recorded verdict `PROBE_ROOTED=yes|no` written into `.superpowers/sdd/progress.md`. Task 2 runs only if `yes`; Task 3 is skipped if `yes`.

- [ ] **Step 1: Copy the `.bak` to a disposable probe image**

```bash
cd ~/OmniImages
cp base_arm_devsystem.qcow2.bak base_arm_devsystem_probe.qcow2
ls -l base_arm_devsystem_probe.qcow2
```

Expected: a 1,160,968,192-byte file. If the size differs from the `.bak`, STOP — the copy is wrong.

- [ ] **Step 2: Confirm the `.bak` itself is untouched**

```bash
cd ~/OmniImages && shasum -a 256 base_arm_devsystem.qcow2.bak | tee /tmp/bak.sha256
```

Record this checksum. Re-run it at the end of Task 2 — it MUST be identical.

- [ ] **Step 3: Point the dev base at the probe image**

Edit `configs/paths.json`, `bases.dev.system` only:

```json
"system": "base_arm_devsystem_probe.qcow2",
```

Leave `base_disk`, `data`, `efivars`, `devkit` alone. Verify:

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid" && python3 -c "
import json; d=json.load(open('configs/paths.json'))
print(d['bases']['dev'])"
```

Expected: `system` is `base_arm_devsystem_probe.qcow2`, `base_disk` is still `base_arm.qcow2`.

- [ ] **Step 4: Boot a throwaway dev account**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid start probe1 --dev --no-window --json
```

Expected: JSON with `ok: true` and a boot. If it fails to boot, capture the full output — a probe image that will not boot is itself the answer (verdict `no`, proceed to Task 3).

- [ ] **Step 5: The root check**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell '/debug_ramdisk/su 0 sh -c "id -u"'
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell 'pm list packages | grep -i magisk'
```

Expected if rooted: the first command prints `0`. The second may print a Magisk package (it can be repackaged/hidden — absence here is NOT disqualifying; the `id -u` result is authoritative).

If `/debug_ramdisk/su` is missing, try the other candidates the engine tries:

```bash
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell '/sbin/su 0 sh -c "id -u"'
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell '/system/bin/su 0 sh -c "id -u"'
```

- [ ] **Step 6: Record the verdict**

Append to `.superpowers/sdd/progress.md`:

```markdown
## Task 1 (Phase 0) — probe root check
PROBE_ROOTED=<yes|no>
su path: <the candidate that returned 0, or "none">
raw `id -u` output: <paste>
.bak sha256 (unchanged): <paste>
```

- [ ] **Step 7: Commit the verdict**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add .superpowers/sdd/progress.md
git commit -m "test(dev-base): Task 1 — probe root verdict"
```

**Branch:** If `PROBE_ROOTED=yes` → Task 2. If `no` → leave the instance stopped, skip Task 2, go to Task 3 (the probe told us nothing; we patch the branded image directly and run the gate there in Task 5).

---

### Task 2: The signature gate on the probe (Phase 1)

The load-bearing test: **with root, can a differently-signed `com.roblox.client` replace the baked system app?** Runs only if Task 1 returned `yes`.

**Files:** none in-repo. On-device only, plus a recorded verdict.

**Interfaces:**
- Consumes: `PROBE_ROOTED=yes` and the working su path from Task 1.
- Produces: verdict `GATE_PASSED=yes|no` in `.superpowers/sdd/progress.md`. `yes` → Option B confirmed, Task 6 builds the force-install. `no` → Option A fallback, STOP and revise the spec.

- [ ] **Step 1: Pull the stock baked Roblox off the guest**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell pm path com.roblox.client
```

Expected: `package:/product/app/Roblox/Roblox.apk`. Record the exact path — Task 6's helper derives its behavior from this shape.

```bash
OMNI_DEV_MODE=1 omnidroid adb probe1 -- pull /product/app/Roblox/Roblox.apk /tmp/stock-roblox.apk
ls -l /tmp/stock-roblox.apk
```

- [ ] **Step 2: Resign it — no injection, signature change only**

The host has no `apksigner`; omni-agent's Docker sandbox has it. `sign_apk`
(`omni-agent/tools/apk_tools.py:498`) signs in place with `/workspace/debug.keystore`
(alias `androiddebugkey`, storepass/keypass `android`), auto-creating that keystore if
absent.

```bash
# stage the APK into the sandbox workspace under the name sign_apk expects
cp /tmp/stock-roblox.apk "/Users/berat/Desktop/Omni Apps/omni-agent/workspace/stock-roblox-resigned.apk"
```

Then invoke `sign_apk("stock-roblox-resigned.apk")` via omni-agent (its sandbox mounts
that workspace at `/workspace`), and copy the result back:

```bash
cp "/Users/berat/Desktop/Omni Apps/omni-agent/workspace/stock-roblox-resigned.apk" /tmp/stock-roblox-resigned.apk
```

If the sandbox is unavailable, confirm the workspace path first
(`ls "/Users/berat/Desktop/Omni Apps/omni-agent"` — the mounted dir may be named
differently) rather than guessing; the gate needs a genuinely differently-signed APK, so
do not substitute an unsigned or identically-signed file.

Verify the cert actually differs:

```bash
keytool -printcert -jarfile /tmp/stock-roblox.apk | grep -i "SHA-?256\|Owner" | head -4
keytool -printcert -jarfile /tmp/stock-roblox-resigned.apk | grep -i "SHA-?256\|Owner" | head -4
```

Expected: different owners/fingerprints. If they match, the resign did not happen — STOP; the gate would be meaningless.

**Why resign-only:** this isolates the *signature* question from the smali-injection question. A failure here means signatures, not omni-agent's bootstrap chain — and it removes the "bootstrapped APK doesn't exist yet" blocker from the critical path.

- [ ] **Step 3: Confirm the plain install fails (the baseline)**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- install -r -g --no-incremental /tmp/stock-roblox-resigned.apk
```

Expected: `INSTALL_FAILED_UPDATE_INCOMPATIBLE ... signatures do not match`. If this unexpectedly SUCCEEDS, record it — the whole force-remove feature is unnecessary and Task 6 shrinks dramatically.

- [ ] **Step 4: The gate — force-remove as root, then install**

Use the su path recorded in Task 1 (shown here as `/debug_ramdisk/su`):

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
SU=/debug_ramdisk/su
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell "$SU 0 sh -c 'mount -o rw,remount /product'"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell "$SU 0 sh -c 'rm -rf /product/app/Roblox'"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell "$SU 0 sh -c 'pm uninstall com.roblox.client'"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell 'pm path com.roblox.client'
```

Expected: the remount succeeds, the `rm` is silent, `pm uninstall` prints `Success`, and the final `pm path` prints **nothing** (package gone).

If the remount fails (`/product` read-only / verity), record the exact error — that is an Option A trigger.

```bash
OMNI_DEV_MODE=1 omnidroid adb probe1 -- install -r -g --no-incremental /tmp/stock-roblox-resigned.apk
```

Expected on success: `Success`.

- [ ] **Step 5: Prove it actually runs (installed ≠ working)**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell monkey -p com.roblox.client -c android.intent.category.LAUNCHER 1
sleep 15
OMNI_DEV_MODE=1 omnidroid screenshot probe1
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell 'dumpsys package com.roblox.client | grep -i "versionName\|pkgFlags"'
```

Expected: the app launches; `pkgFlags` no longer contains `SYSTEM`. Look at the screenshot — a crash loop counts as a FAIL.

- [ ] **Step 6: Confirm the `.bak` is still untouched**

```bash
cd ~/OmniImages && shasum -a 256 base_arm_devsystem.qcow2.bak
```

Expected: identical to `/tmp/bak.sha256` from Task 1 Step 2. If it changed, something wrote to the reference image — STOP and investigate.

- [ ] **Step 7: Record the verdict and commit**

Append to `.superpowers/sdd/progress.md`:

```markdown
## Task 2 (Phase 1) — signature gate
GATE_PASSED=<yes|no>
pm path (before): <paste>
plain install result: <paste>
force-remove sequence output: <paste>
resigned install result: <paste>
launch/screenshot: <pass|fail, note>
mount point remounted: /product
.bak sha256 (unchanged): <paste>
```

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add .superpowers/sdd/progress.md
git commit -m "test(dev-base): Task 2 gate — root force-remove enables resigned install"
```

- [ ] **Step 8: Stop the probe instance**

```bash
OMNI_DEV_MODE=1 omnidroid stop probe1
```

**Branch:** `GATE_PASSED=no` → **STOP the plan.** Option A fallback (rebuild the dev system without baked Roblox) needs a spec revision before any further work. Report to the user; do not improvise.

---

### Task 3: Fallback root landing — only if Task 1 returned `no`

Skip entirely if `PROBE_ROOTED=yes`. This is the original plan's Task 2: patch the branded image directly, then run Task 2's gate against it.

**Files:** Modify: `~/OmniImages/base_arm_devsystem.qcow2` (in place, brick-risky)

- [ ] **Step 1: Do Task 4 first**

The `build_dev_base` re-registration fix (Task 4) MUST land before running `--patch-boot`, or the patch run will silently repoint `bases.dev.base_disk` to `base_arm_v2.qcow2`. Go do Task 4, then return here.

- [ ] **Step 2: Verify the backup by checksum**

```bash
cd ~/OmniImages
shasum -a 256 base_arm_devsystem.qcow2 base_arm_devsystem.qcow2.safebak-20260720
```

Expected: identical checksums (the safebak is a true copy of the current branded image). If they differ, re-take the backup before proceeding:

```bash
cp base_arm_devsystem.qcow2 base_arm_devsystem.qcow2.safebak-$(date +%Y%m%d-%H%M)
```

- [ ] **Step 3: Restore `bases.dev.system` to the branded image**

```json
"system": "base_arm_devsystem.qcow2",
```

- [ ] **Step 4: Run the boot patch**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid build-dev-base --patch-boot
```

Expected: the patch runs and the final line reports `[ROOTED]`. Capture ALL output.

- [ ] **Step 5: Verify root on a real boot**

```bash
OMNI_DEV_MODE=1 omnidroid start probe1 --dev --no-window --json
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell '/debug_ramdisk/su 0 sh -c "id -u"'
```

Expected: `0`. If not, restore from the safebak and STOP.

- [ ] **Step 6: Now run Task 2's gate against this instance**

Execute Task 2 Steps 1–7 verbatim against `probe1` (now backed by the branded image). Record both verdicts.

- [ ] **Step 7: Commit**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add .superpowers/sdd/progress.md
git commit -m "feat(dev-base): root the branded dev system (fallback path) + gate verdict"
```

---

### Task 4: `build_dev_base` must not clobber the dev registration

`build_dev_base` rewrites the whole `bases.dev` entry, including `"base_disk": arm["base_disk"]` — which silently repoints dev from `base_arm.qcow2` to `base_arm_v2.qcow2` and destroys the hand-written `notes`. This is the reconcile that Task 1 of the earlier plan was supposed to make in code.

**Files:**
- Modify: `omnidroid/engine.py:2949` (the `raw.setdefault("bases", {})[DEV_BASE_TAG] = {...}` block)
- Test: `tests/test_dev_base_registration.py` (create)

**Interfaces:**
- Produces: `bases.dev.base_disk` and `bases.dev.notes` survive re-registration when the entry already exists.

- [ ] **Step 1: Write the failing test**

```python
#!/usr/bin/env python3
"""Re-registering the dev base must PRESERVE an existing base_disk and notes.

`build_dev_base` rewrites bases.dev wholesale. Copying base_disk from the arm
base silently repoints dev from base_arm.qcow2 to base_arm_v2.qcow2 -- changing
which image the dev guest actually boots, with no log line and no opt-in.
"""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402


class DevBaseReRegistration(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="omni-devreg-"))
        self.cfg_path = self.tmp / "paths.json"
        self.existing = {
            "current_base": "x86",
            "bases": {
                "arm": {"type": "arm-uefi", "base_disk": "base_arm_v2.qcow2",
                        "system": "base_arm_system.qcow2",
                        "data": "base_arm_data.qcow2"},
                "dev": {"type": "arm-uefi", "base_disk": "base_arm.qcow2",
                        "system": "base_arm_devsystem.qcow2",
                        "data": "base_arm_devdata.qcow2",
                        "notes": "hand-written note that must survive"},
            },
        }
        self.cfg_path.write_text(json.dumps(self.existing))

    def _merged(self):
        """Run just the registration merge against the existing config."""
        raw = json.loads(self.cfg_path.read_text())
        arm = raw["bases"]["arm"]
        return omni._dev_base_entry(raw, arm, devkit_disk="base_arm_devkit.qcow2",
                                    dev_data="base_arm_devdata.qcow2",
                                    frida_version="17.15.4", frida_port=27142,
                                    magisk=True, magisk_version="v30.7",
                                    rooted=True)

    def test_existing_base_disk_is_preserved(self):
        entry = self._merged()
        self.assertEqual(entry["base_disk"], "base_arm.qcow2",
                         "re-registration must not repoint dev at the arm base disk")

    def test_existing_notes_are_preserved(self):
        entry = self._merged()
        self.assertIn("hand-written note", entry["notes"])

    def test_rooted_flag_still_updates(self):
        entry = self._merged()
        self.assertTrue(entry["devkit_manifest"]["rooted"])

    def test_fresh_registration_falls_back_to_arm_base_disk(self):
        raw = json.loads(self.cfg_path.read_text())
        del raw["bases"]["dev"]
        arm = raw["bases"]["arm"]
        entry = omni._dev_base_entry(raw, arm, devkit_disk="base_arm_devkit.qcow2",
                                     dev_data="base_arm_devdata.qcow2",
                                     frida_version="17.15.4", frida_port=27142,
                                     magisk=True, magisk_version="v30.7",
                                     rooted=False)
        self.assertEqual(entry["base_disk"], "base_arm_v2.qcow2",
                         "with no existing dev entry, inherit the arm base disk")


if __name__ == "__main__":
    unittest.main(verbosity=2)
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 tests/test_dev_base_registration.py
```

Expected: FAIL — `AttributeError: module 'omnidroid.engine' has no attribute '_dev_base_entry'`.

- [ ] **Step 3: Extract the registration into `_dev_base_entry`**

Add above `build_dev_base` in `omnidroid/engine.py`:

```python
def _dev_base_entry(raw, arm, devkit_disk, dev_data, frida_version,
                    frida_port, magisk, magisk_version, rooted):
    """Build the bases['dev'] entry, PRESERVING fields an existing dev
    registration already carries.

    `base_disk` and `notes` are deliberately sticky: the dev system image is
    STANDALONE (it shadows the shared base — see _brand_target), so its
    base_disk is an independent choice, not something to inherit from the arm
    base on every rebuild. Silently repointing it changes which image the dev
    guest boots.
    """
    prev = (raw.get("bases") or {}).get(DEV_BASE_TAG) or {}
    default_notes = (f"arm dev base: base_arm + {devkit_disk} (vdc) with "
                     f"frida {frida_version} (arm64) + Magisk"
                     f"{' [rooted]' if rooted else ' [root pending: --patch-boot]'}"
                     f". omni-agent only; NOT shipped. hidden frida port "
                     f"{frida_port}.")
    return {
        "type": BASE_TYPE_ARM,
        "base_disk": prev.get("base_disk") or arm["base_disk"],
        "system": ARM_DEVSYSTEM_DISK,
        "data": dev_data,
        "efivars": arm.get("efivars", ARM_BASE_EFIVARS),
        "devkit": devkit_disk,
        "src": "base_arm + devkit disk (frida + Magisk + omni tools)",
        "notes": prev.get("notes") or default_notes,
        "devkit_manifest": {
            "frida_version": frida_version,
            "frida_port": frida_port,
            "magisk": bool(magisk),
            "magisk_version": magisk_version,
            "rooted": rooted,
            "tools": ["frida-server", "omni-fridad", "omni-frida-stop",
                      "omni-hide", "omni-magisk-setup"],
        },
    }
```

- [ ] **Step 4: Call it from `build_dev_base`**

Replace the `raw.setdefault("bases", {})[DEV_BASE_TAG] = { ... }` literal (around `engine.py:2946-2972`) with:

```python
        raw = read_config()
        dev_data = (ARM_DEVDATA_DISK if (images / ARM_DEVDATA_DISK).exists()
                    else arm["data"])
        raw.setdefault("bases", {})[DEV_BASE_TAG] = _dev_base_entry(
            raw, arm, devkit_disk=ARM_DEVKIT_DISK, dev_data=dev_data,
            frida_version=frida_version, frida_port=frida_port,
            magisk=bool(staging.get("magisk")),
            magisk_version=staging.get("magisk_version"), rooted=rooted)
```

Keep the `# HARD RULE: do NOT change current_base` comment and the `CONFIG_PATH.write_text(...)` line exactly as they are.

- [ ] **Step 5: Run the tests**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 tests/test_dev_base_registration.py
```

Expected: 4 tests PASS.

- [ ] **Step 6: Run the full suite — no regressions**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 -m pytest tests/ -q
```

Expected: 135 passed (131 baseline + 4 new). Any failure means the extraction changed behavior — fix before committing.

- [ ] **Step 7: Commit**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add omnidroid/engine.py tests/test_dev_base_registration.py
git commit -m "fix(dev-base): re-registration preserves base_disk and notes"
```

---

### Task 5: Land root on the branded image (Phase 2)

Only if `PROBE_ROOTED=yes` AND `GATE_PASSED=yes`. (If Task 3 ran, root is already landed — skip to Task 6.)

**Files:** Modify: `~/OmniImages/base_arm_devsystem.qcow2` (in place, brick-risky)

**Interfaces:**
- Consumes: Task 4's `_dev_base_entry` (must be committed first, or the patch run repoints `base_disk`).
- Produces: a branded, rooted `base_arm_devsystem.qcow2`; `devkit_manifest.rooted == true`.

- [ ] **Step 1: Confirm Task 4 is committed**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git log --oneline -3 && grep -n "_dev_base_entry" omnidroid/engine.py | head -2
```

Expected: the fix commit is present and `_dev_base_entry` exists. If not, STOP and do Task 4.

- [ ] **Step 2: Verify the backup by checksum**

```bash
cd ~/OmniImages
shasum -a 256 base_arm_devsystem.qcow2 base_arm_devsystem.qcow2.safebak-20260720
```

Expected: identical. If not, take a fresh backup:

```bash
cp base_arm_devsystem.qcow2 base_arm_devsystem.qcow2.safebak-$(date +%Y%m%d-%H%M)
```

- [ ] **Step 3: Restore `bases.dev.system` to the branded image**

In `configs/paths.json`:

```json
"system": "base_arm_devsystem.qcow2",
```

- [ ] **Step 4: Patch the boot**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid build-dev-base --patch-boot 2>&1 | tee /tmp/patch-boot.log
```

Expected: completes with `[ROOTED]`. Keep the log.

- [ ] **Step 5: Verify root AND that branding survived**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid start probe1 --dev --no-window --json
OMNI_DEV_MODE=1 omnidroid adb probe1 -- shell '/debug_ramdisk/su 0 sh -c "id -u"'
OMNI_DEV_MODE=1 omnidroid screenshot probe1
```

Expected: `0`, and the screenshot shows the Omni loading screen / branded UI (not the stock vendor boot animation). Branding loss means the patch flattened away the v2 branding — restore from safebak and STOP.

- [ ] **Step 6: Verify the registration was NOT repointed**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid" && python3 -c "
import json; d=json.load(open('configs/paths.json'))['bases']['dev']
print('base_disk:', d['base_disk']); print('rooted:', d['devkit_manifest']['rooted'])"
```

Expected: `base_disk: base_arm.qcow2` and `rooted: True`. A `base_arm_v2.qcow2` here means Task 4's fix did not take.

- [ ] **Step 7: Commit**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add configs/paths.json .superpowers/sdd/progress.md
git commit -m "feat(dev-base): branded dev system rooted via --patch-boot"
```

---

### Task 6: Engine — dev-only, root-gated force-install (Phase 3)

Hook the proven force-remove sequence into `_install_apk`'s existing recovery, so `start --dev --apk` handles the baked system app automatically.

**Files:**
- Modify: `omnidroid/engine.py` (add two helpers; extend `_install_apk` around line 4462)
- Test: `tests/test_dev_force_install.py` (create)

**Interfaces:**
- Consumes: `resolve_su(acct)` (`engine.py:1809`), `acct_is_dev(acct)` (`engine.py:175`), `_install_needs_clean_replace(out)`, `_abi_install(acct, apk, abi)`.
- Produces: `_system_apk_dir(acct, pkg) -> str|None` and `_dev_force_remove_game(acct, pkg, label) -> dict` with keys `ok: bool` and, on failure, `error: str` (`"root_required"` | `"not_system_app"`).

- [ ] **Step 1: Write the failing tests**

```python
#!/usr/bin/env python3
"""Dev-only, root-gated force-install of a differently-signed system app.

The dev base ships Roblox as a BAKED SYSTEM app signed with Roblox's cert, so a
resigned build hits INSTALL_FAILED_UPDATE_INCOMPATIBLE and cannot be plain-
uninstalled. With root we remove the system APK first. This path must NEVER be
reachable from prod or from a dev account without working su.
"""
import os
import sys
import unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from omnidroid import engine as omni  # noqa: E402

SYS_PATH = "package:/product/app/Roblox/Roblox.apk\n"
DATA_PATH = "package:/data/app/~~abc==/com.roblox.client-1/base.apk\n"


def _r(stdout="", stderr=""):
    return SimpleNamespace(stdout=stdout, stderr=stderr)


class SystemApkDir(unittest.TestCase):
    def test_system_path_yields_its_directory(self):
        with mock.patch.object(omni, "adb", return_value=_r(SYS_PATH)):
            self.assertEqual(
                omni._system_apk_dir({"name": "a"}, "com.roblox.client"),
                "/product/app/Roblox")

    def test_data_app_path_is_not_a_system_app(self):
        with mock.patch.object(omni, "adb", return_value=_r(DATA_PATH)):
            self.assertIsNone(
                omni._system_apk_dir({"name": "a"}, "com.roblox.client"))

    def test_missing_package_yields_none(self):
        with mock.patch.object(omni, "adb", return_value=_r("")):
            self.assertIsNone(
                omni._system_apk_dir({"name": "a"}, "com.roblox.client"))

    def test_adb_failure_yields_none_not_raise(self):
        with mock.patch.object(omni, "adb", side_effect=RuntimeError("boom")):
            self.assertIsNone(
                omni._system_apk_dir({"name": "a"}, "com.roblox.client"))


class DevForceRemove(unittest.TestCase):
    def test_without_root_it_refuses(self):
        with mock.patch.object(omni, "resolve_su", return_value=None):
            res = omni._dev_force_remove_game({"name": "a"},
                                              "com.roblox.client", "lbl")
        self.assertFalse(res["ok"])
        self.assertEqual(res["error"], "root_required")

    def test_non_system_app_is_not_force_removed(self):
        with mock.patch.object(omni, "resolve_su", return_value="/debug_ramdisk/su"), \
             mock.patch.object(omni, "_system_apk_dir", return_value=None):
            res = omni._dev_force_remove_game({"name": "a"},
                                              "com.roblox.client", "lbl")
        self.assertFalse(res["ok"])
        self.assertEqual(res["error"], "not_system_app")

    def test_command_sequence_remounts_removes_and_uninstalls(self):
        calls = []

        def fake_adb(acct, *argv, **kw):
            calls.append(" ".join(argv))
            return _r("Success")

        with mock.patch.object(omni, "resolve_su", return_value="/debug_ramdisk/su"), \
             mock.patch.object(omni, "_system_apk_dir",
                               return_value="/product/app/Roblox"), \
             mock.patch.object(omni, "adb", fake_adb):
            res = omni._dev_force_remove_game({"name": "a"},
                                              "com.roblox.client", "lbl")

        self.assertTrue(res["ok"])
        joined = "\n".join(calls)
        self.assertIn("mount -o rw,remount /product", joined)
        self.assertIn("rm -rf /product/app/Roblox", joined)
        self.assertIn("pm uninstall com.roblox.client", joined)
        # every root command must go through the resolved su
        for c in calls:
            if "mount" in c or "rm -rf" in c or "pm uninstall" in c:
                self.assertIn("/debug_ramdisk/su", c)

    def test_mount_point_is_the_first_path_component(self):
        calls = []

        def fake_adb(acct, *argv, **kw):
            calls.append(" ".join(argv))
            return _r("Success")

        with mock.patch.object(omni, "resolve_su", return_value="/debug_ramdisk/su"), \
             mock.patch.object(omni, "_system_apk_dir",
                               return_value="/system/app/Roblox"), \
             mock.patch.object(omni, "adb", fake_adb):
            omni._dev_force_remove_game({"name": "a"}, "com.roblox.client", "lbl")
        self.assertIn("mount -o rw,remount /system", "\n".join(calls))


class ForceInstallGating(unittest.TestCase):
    """The force-remove must fire ONLY for a dev account, and only after the
    ordinary clean-replace recovery has already failed."""

    def test_prod_account_never_force_removes(self):
        with mock.patch.object(omni, "acct_is_dev", return_value=False), \
             mock.patch.object(omni, "_dev_force_remove_game") as frm:
            self.assertFalse(omni._should_force_remove(
                {"name": "a"}, "INSTALL_FAILED_UPDATE_INCOMPATIBLE", "pkg"))
        frm.assert_not_called()

    def test_dev_account_with_sig_failure_force_removes(self):
        with mock.patch.object(omni, "acct_is_dev", return_value=True):
            self.assertTrue(omni._should_force_remove(
                {"name": "a"}, "INSTALL_FAILED_UPDATE_INCOMPATIBLE", "pkg"))

    def test_dev_account_with_unrelated_failure_does_not(self):
        with mock.patch.object(omni, "acct_is_dev", return_value=True):
            self.assertFalse(omni._should_force_remove(
                {"name": "a"}, "INSTALL_FAILED_INSUFFICIENT_STORAGE", "pkg"))

    def test_success_never_force_removes(self):
        with mock.patch.object(omni, "acct_is_dev", return_value=True):
            self.assertFalse(omni._should_force_remove(
                {"name": "a"}, "Success", "pkg"))

    def test_no_package_name_does_not(self):
        with mock.patch.object(omni, "acct_is_dev", return_value=True):
            self.assertFalse(omni._should_force_remove(
                {"name": "a"}, "INSTALL_FAILED_UPDATE_INCOMPATIBLE", None))


if __name__ == "__main__":
    unittest.main(verbosity=2)
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 tests/test_dev_force_install.py
```

Expected: FAIL — `_system_apk_dir` / `_dev_force_remove_game` / `_should_force_remove` do not exist.

- [ ] **Step 3: Add the helpers**

Add to `omnidroid/engine.py`, immediately above `_install_apk` (~line 4420):

```python
def _system_apk_dir(acct, pkg):
    """Directory holding the BAKED SYSTEM apk for <pkg>, or None when the
    installed build is not a system app (or not installed at all).

    The dev base ships Roblox at /product/app/Roblox/Roblox.apk with
    pkgFlags=[ SYSTEM ]; a system package cannot be plain-uninstalled and PMS
    keeps its signature as the authority, so a resigned build can never replace
    it until the on-disk APK is gone.
    """
    try:
        out = (adb(acct, "shell", "pm", "path", pkg, timeout=15).stdout or "")
    except Exception:
        return None
    for line in out.splitlines():
        line = line.strip()
        if not line.startswith("package:"):
            continue
        path = line.split(":", 1)[1].strip()
        if path.startswith(("/product/", "/system/", "/vendor/", "/system_ext/")):
            return posixpath.dirname(path)
    return None


def _should_force_remove(acct, out, pkg):
    """Whether to escalate to the root force-remove: a dev account, a real
    package name, and an install still blocked by a signature/downgrade
    conflict after the ordinary clean-replace recovery already ran."""
    return bool(pkg) and _install_needs_clean_replace(out) and acct_is_dev(acct)


def _dev_force_remove_game(acct, pkg, label):
    """DEV ONLY. Remove the baked system build of <pkg> as root so a
    differently-signed APK can install. Ephemeral: the dev guest boots
    snapshot=on, so this is thrown away on stop -- the base image is never
    modified. Returns {"ok": bool, "error": str|None}.
    """
    su = resolve_su(acct)
    if not su:
        return {"ok": False, "error": "root_required"}
    apk_dir = _system_apk_dir(acct, pkg)
    if not apk_dir:
        return {"ok": False, "error": "not_system_app"}
    mount_point = "/" + apk_dir.strip("/").split("/")[0]
    print(f"[{label}] {pkg} is a baked system app at {apk_dir}; "
          f"force-removing as root (ephemeral) ...")
    for cmd in (f"mount -o rw,remount {mount_point}",
                f"rm -rf {apk_dir}",
                f"pm uninstall {pkg}"):
        r = adb(acct, "shell", f"{su} 0 sh -c {shlex.quote(cmd)}", timeout=60)
        print(f"[{label}] su: {cmd} -> "
              f"{((r.stdout or '') + (r.stderr or '')).strip()[:200]}")
    return {"ok": True, "error": None}
```

Confirm `posixpath` and `shlex` are imported at the top of `engine.py`; add `import posixpath` if absent (`shlex` is already used by `resolve_su`).

- [ ] **Step 4: Hook it into `_install_apk`**

In `_install_apk`, immediately AFTER the existing clean-replace recovery block and BEFORE the `if "Success" not in out:` failure return, insert:

```python
    # Still blocked after the ordinary recovery. On a ROOTED DEV account the
    # cause is the baked SYSTEM build (pkgFlags=[ SYSTEM ]) -- PMS keeps the
    # system signature as the authority, so no amount of uninstalling at the
    # user level helps. Remove the on-disk system APK as root, then reinstall.
    # Strictly dev-gated: prod accounts never reach this.
    if "Success" not in out and _should_force_remove(acct, out, pkg):
        fr = _dev_force_remove_game(acct, pkg, label)
        if not fr["ok"]:
            return {"ok": False, "error": "apk_install_failed",
                    "detail": f"{fr['error']}: cannot replace the baked system "
                              f"build of {pkg} ({out[:200]})",
                    "package": pkg}
        r = _abi_install(acct, apk_path, abi)
        out = (r.stdout + r.stderr).strip()
        print(f"[{label}] reinstall after force-remove: {out}")
```

- [ ] **Step 5: Run the new tests**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 tests/test_dev_force_install.py
```

Expected: 13 tests PASS.

- [ ] **Step 6: Run the full suite**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 -m pytest tests/ -q
```

Expected: 148 passed (131 baseline + 4 from Task 4 + 13 here). `test_install_recovery.py` and `test_session.py::ApkInstallOnStart` must still pass — they pin the untouched prod behavior.

- [ ] **Step 7: Commit**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add omnidroid/engine.py tests/test_dev_force_install.py
git commit -m "feat(dev-apk): root-gated force-install over the baked system Roblox (dev only)"
```

---

### Task 7: End-to-end acceptance (Phase 4)

Prove the user-facing goal: any APK swaps in, repeatably, on the branded rooted image.

**Files:** none in-repo except the progress ledger.

- [ ] **Step 1: Confirm the dev base is the branded, rooted one**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid" && python3 -c "
import json; d=json.load(open('configs/paths.json'))['bases']['dev']
print(d['system'], d['base_disk'], d['devkit_manifest']['rooted'])"
```

Expected: `base_arm_devsystem.qcow2 base_arm.qcow2 True`.

- [ ] **Step 2: Swap #1 — the resigned stock APK**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid stop probe1 2>/dev/null
OMNI_DEV_MODE=1 omnidroid start probe1 --dev --apk /tmp/stock-roblox-resigned.apk --no-window --json
```

Expected: JSON reporting a successful install. The `not_logged_in` result is EXPECTED here — this APK has no `OmniBootstrap`. That is the loud-failure signal working correctly, not a bug.

- [ ] **Step 3: Swap #2 — a second, differently-signed build**

Make a second keystore with a *different* key, so this is a genuine signature change rather
than a reinstall of the same cert:

```bash
cd "/Users/berat/Desktop/Omni Apps/omni-agent/workspace"
keytool -genkey -v -keystore second.keystore -alias secondkey \
  -keyalg RSA -keysize 2048 -validity 10000 \
  -storepass android -keypass android \
  -dname "CN=Second, OU=Test, O=Test, L=X, ST=X, C=US"
cp /tmp/stock-roblox.apk stock-roblox-resigned-2.apk
```

Sign `stock-roblox-resigned-2.apk` with `second.keystore` / alias `secondkey` inside the
sandbox (same apksigner invocation as `sign_apk`, with `--ks second.keystore --ks-key-alias
secondkey`), copy it back to `/tmp/`, and confirm the certs differ:

```bash
keytool -printcert -jarfile /tmp/stock-roblox-resigned.apk | grep -i "SHA-256"
keytool -printcert -jarfile /tmp/stock-roblox-resigned-2.apk | grep -i "SHA-256"
```

Expected: two different SHA-256 fingerprints. Then:

```bash
OMNI_DEV_MODE=1 omnidroid stop probe1
OMNI_DEV_MODE=1 omnidroid start probe1 --dev --apk /tmp/stock-roblox-resigned-2.apk --no-window --json
```

Expected: installs again. Two successive differently-signed installs prove the swap is repeatable, not a one-shot.

- [ ] **Step 4: The login proof (only if a bootstrapped APK exists)**

If omni-agent has produced a bootstrapped build:

```bash
OMNI_DEV_MODE=1 omnidroid stop probe1
OMNI_DEV_MODE=1 omnidroid start probe1 --dev --apk <bootstrapped.apk> --place 8737899170 --json
OMNI_DEV_MODE=1 omnidroid logcat probe1 | grep -i "OmniBootstrap"
OMNI_DEV_MODE=1 omnidroid screenshot probe1
```

Expected: `OmniBootstrap: session cookie installed (N chars)` and an in-game screenshot. If no bootstrapped APK exists yet, record Step 4 as DEFERRED — Steps 2–3 already prove the user-facing swap goal.

- [ ] **Step 5: Confirm prod is untouched**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
python3 -m pytest tests/ -q
git diff --stat HEAD~3 -- omnidroid/engine.py
```

Expected: full suite green; the engine diff touches only `_dev_base_entry`, the three new helpers, and the gated block inside `_install_apk`.

- [ ] **Step 6: Clean up the probe artifacts**

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
OMNI_DEV_MODE=1 omnidroid stop probe1
OMNI_DEV_MODE=1 omnidroid remove probe1
rm -f ~/OmniImages/base_arm_devsystem_probe.qcow2
```

Leave every `.bak` and `.safebak-*` in place.

- [ ] **Step 7: Record and commit**

Append the acceptance results to `.superpowers/sdd/progress.md`, then:

```bash
cd "/Users/berat/Desktop/Omni Apps/omnidroid"
git add .superpowers/sdd/progress.md configs/paths.json
git commit -m "test(dev-base): end-to-end APK swap acceptance on the rooted branded base"
```

---

## After this plan

The omni-agent CLI sync is a **separate plan** against its own already-written spec
(`omni-agent/docs/superpowers/specs/2026-07-20-omniagent-omnidroid-cli-sync-design.md`). The user
chose root-first sequencing so that sync targets a surface proven on-device. Note that
`play_roblox` remains broken until that plan runs — `roblox_session.py:134` still emits the
deleted `play` subcommand.
