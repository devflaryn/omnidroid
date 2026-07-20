# B — Dev APK-Offset Disks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the dev flow a reusable **per-APK offset disk** holding a **bootstrapped** Roblox, so `omnidroid start <user> --dev --apk-offset <name>` logs the account in exactly like prod (which bakes a bootstrapped Roblox). This is also the fix for the "dev cookie-login lands on Sign In" bug. Prod is untouched.

**Architecture:** An APK-offset is a qcow2 COW overlay on the dev `/data` template (`base_arm_devdata.qcow2`) with a bootstrapped Roblox installed into it. `apk-offset create` boots a throwaway dev instance with the overlay as `/data` (persisting the install), installs the bootstrapped APK, and shuts down. `start --dev --apk-offset <name>` boots the dev base but swaps the data disk to the offset, opened `snapshot=on` (per-boot throwaway; the installed Roblox is shared read-mostly across accounts, the cookie injected per launch by the in-Roblox bootstrap). Same login mechanism as prod — **never frida**.

**Tech Stack:** Python 3.13+, QEMU (arm64/UEFI, qcow2 backing chains + `snapshot=on`), adb, pytest. Depends on a **bootstrapped** Roblox APK produced by omni-agent (`decode_apk → inject_session_bootstrap → recompile_apk → sign_apk`); omnidroid consumes it, never reimplements the RE tooling.

## Global Constraints

- **Verified premise (do not re-litigate):** prod login works via the injected `OmniBootstrap` — confirmed on-device (in-game screenshot + `OmniBootstrap: session cookie installed (1199 chars)` in logcat). Dev must replicate this with a bootstrapped Roblox in the offset. The proof line to assert in verification is exactly `OmniBootstrap: session cookie installed`.
- **Never frida for login.** Prod has no frida; dev must validate the real prod path. Frida stays a dev-only debugging tool, unrelated to login.
- **Prod is untouched.** No offsets, no `--apk-offset`, baked Roblox. `--apk-offset` and `apk-offset *` are **dev-only**, gated by the existing dev boundary (`assert_dev_allowed` / `OMNI_DEV_MODE`); rejected on a prod base.
- **Offset = per-APK, reusable across accounts.** Keyed to an APK build, never to an account. The same offset launches with many accounts (account = runtime cookie, delivered per launch; offset writes are `snapshot=on` throwaway).
- **arm-only** (dev is arm). The offset is a COW overlay on `base_arm_devdata.qcow2`.
- **Diskless model preserved.** Accounts stay JSON in the store; the offset is the only per-APK disk, and per-boot account state is still `snapshot=on` throwaway wiped on stop.
- **Stock-APK guard.** `apk-offset create` must refuse an APK that lacks the bootstrap (so the original bug can't recur silently) — detect the `OmniBootstrap` class/marker before building the offset.
- **Suite stays green after every task.** Run: `python3 -m pytest tests/ -q` from the repo root.

## File Structure

```
omnidroid/config.py       # offsets_dir() + offset registry path
omnidroid/offsets.py      # NEW: offset registry (create/list/remove records) + path resolution
omnidroid/engine.py       # apk-offset create/list/remove commands; start --apk-offset wiring;
                          #   qemu_command_arm data_src swap; dev-gating; stock-APK guard
tests/test_offsets.py     # NEW: registry CRUD, dev-gate, stock-APK guard, data_src swap (unit)
```

`apk-offset create`'s boot→install→capture reuses the existing `make_overlay`, `spawn_qemu`, `wait_for_boot`, `adb`, `_shutdown` (the `update_kiosk_arm` pattern).

---

### Task 1: Offset registry + storage (`offsets.py` + config)

The data layer: where offset qcow2 files live and a JSON registry mapping name → {path, source APK identity/version, created_at}. Pure library, unit-tested.

**Files:** Create `omnidroid/offsets.py`, `tests/test_offsets.py`; modify `omnidroid/config.py`.

**Interfaces:**
- `config.offsets_dir() -> Path` — where offset qcow2 files live (default: images dir `/ offsets`, or data dir `/ offsets`; decide to sit beside the base images since they back onto `base_arm_devdata`). Created on demand.
- `offsets.registry_path() -> Path`, `offsets.add(name, apk_pkg, apk_version) -> dict`, `offsets.get(name) -> dict|None`, `offsets.list_all() -> list`, `offsets.remove(name) -> bool`, `offsets.qcow2_path(name) -> Path`.

- [ ] **Step 1: Write failing tests** — registry add/get/list/remove round-trip (OMNI_DATA_DIR tmp); `qcow2_path(name)` under `offsets_dir()`; `add` rejects a duplicate name; `remove` returns False for a missing name.
- [ ] **Step 2: Run — FAIL** (`No module named omnidroid.offsets`).
- [ ] **Step 3: Implement** `config.offsets_dir()` + `offsets.py` (a small JSON registry mirroring `accounts.py`'s read/write pattern, `0644` — no secrets in it).
- [ ] **Step 4: Run new tests — PASS.**
- [ ] **Step 5: Full suite green.**
- [ ] **Step 6: Commit** `feat(offsets): per-APK offset registry + storage`.

---

### Task 2: Stock-APK guard (`_apk_has_bootstrap`)

Refuse a non-bootstrapped APK at create time, so an offset can never silently produce the login-page bug.

**Files:** Modify `omnidroid/engine.py`; extend `tests/test_offsets.py`.

**Interfaces:**
- `_apk_has_bootstrap(apk_path) -> bool` — true iff the APK carries the `OmniBootstrap` component. Detection: unzip the APK and scan its dex for the `com/omni/bootstrap/OmniBootstrap` class string (no apktool needed — a substring scan of `classes*.dex` is enough), or check the manifest's `application:name` if the bootstrap sets it. Choose the most robust cheap check; document it.

- [ ] **Step 1: Write failing tests** — a fixture APK (a tiny zip containing a `classes.dex` with the marker string) returns True; one without returns False; a missing file raises a clear error. (Build the fixtures in the test with `zipfile`.)
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement `_apk_has_bootstrap`** (zip + dex substring scan for the `OmniBootstrap` class descriptor).
- [ ] **Step 4–5: tests + suite green.**
- [ ] **Step 6: Commit** `feat(offsets): guard rejecting a stock (non-bootstrapped) APK`.

---

### Task 3: `apk-offset create` (boot → install → capture)

Build the offset: COW overlay on the dev data template, boot a throwaway dev instance with it persisting, install the bootstrapped APK, shut down, register.

**Files:** Modify `omnidroid/engine.py` (`cmd_apk_offset_create` + parser); this task is exercised on-device (see verification), with a unit test for the guard/dev-gate/registry wiring using mocks.

**Interfaces:**
- `cmd_apk_offset_create(args)` — dev-gated (`assert_dev_allowed` on the dev base; refuse without `OMNI_DEV_MODE`). Steps: `_apk_has_bootstrap(args.apk)` guard → `make_overlay(offsets.qcow2_path(name), images/ARM_DEVDATA_DISK)` → boot a throwaway dev instance with that overlay as `/data` **persisting** (not snapshot=on) → `adb install -r -g --no-incremental <apk>` → clean `_shutdown` → `offsets.add(name, pkg, version)`. Refuse an existing offset name.

- [ ] **Step 1: Write failing test (mocked)** — with `_apk_has_bootstrap→True`, `make_overlay`/`spawn_qemu`/`wait_for_boot`/`adb`/`_shutdown` mocked, `cmd_apk_offset_create` registers the offset and calls install; with `_apk_has_bootstrap→False`, it fails `stock_apk` before creating anything; without OMNI_DEV_MODE it fails dev-gate.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** `cmd_apk_offset_create` (mirror `update_kiosk_arm`'s boot/install/shutdown; the QEMU boot here uses the overlay as the data disk WITHOUT snapshot=on so the install persists — reuse `_make_persistent_arm_account`-style disk handling but pointed at the offset overlay).
- [ ] **Step 4–5: tests + suite green.**
- [ ] **Step 6: Commit** `feat(offsets): apk-offset create (boot/install/capture into a per-APK overlay)`.

---

### Task 4: `start --dev --apk-offset` wiring (data_src swap)

Make `start` boot the dev base with the offset swapped in as the data disk, `snapshot=on`.

**Files:** Modify `omnidroid/engine.py` (`cmd_start` parser + flow; `qemu_command_arm` data_src selection; `build_acct` to carry the offset); extend `tests/test_offsets.py` / `tests/test_ephemeral_boot.py`.

**Interfaces:**
- `start` gains `--apk-offset <name>` (dev-only; error if given without `--dev` or on prod, or if the offset doesn't exist).
- `build_acct(name, cfg, dev, apk_offset=None)` records the offset on the handle (`acct["apk_offset"]`).
- `qemu_command_arm`: when `acct.get("apk_offset")`, set `data_src = offsets.qcow2_path(acct["apk_offset"])` instead of `images / base["data"]`, still opened `snapshot=on`. Everything else (system/devkit/efivars) unchanged.

- [ ] **Step 1: Write failing test** — with an offset registered, `qemu_command_arm` for an acct carrying `apk_offset` points `-drive ...vdb` at the offset path with `snapshot=on` and NOT at `base_arm_devdata`; without it, unchanged (regression-guards the prod/default path). Use the `test_ephemeral_boot` string-command pattern.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** the parser flag (dev-gated), `build_acct` param, and the `data_src` swap.
- [ ] **Step 4–5: tests + suite green.** Confirm the non-offset path is byte-identical (prod/default unaffected).
- [ ] **Step 6: Commit** `feat(offsets): start --dev --apk-offset boots the per-APK offset as /data`.

---

### Task 5: `apk-offset list` / `remove` + end-to-end on-device verification

Finish the command surface and prove the whole thing on the arm base.

**Files:** Modify `omnidroid/engine.py` (`cmd_apk_offset_list`, `cmd_apk_offset_remove`, parser); the verification is manual/on-device.

- [ ] **Step 1: Implement `apk-offset list`** (from the registry) and **`remove`** (delete qcow2 + registry entry), dev-gated. Unit tests for both.
- [ ] **Step 2: Suite green.**
- [ ] **Step 3: On-device end-to-end** (needs the arm dev base + a bootstrapped Roblox APK from omni-agent `inject_session_bootstrap` on test.apk):
  1. `omni-agent`: bootstrap `test.apk` → `roblox-bootstrapped.apk`.
  2. `OMNI_DEV_MODE=1 omnidroid apk-offset create roblox-2726 --apk roblox-bootstrapped.apk`.
  3. `OMNI_DEV_MODE=1 omnidroid start admn1b12farm3 --dev --apk-offset roblox-2726 --place 8737899170 --no-window --json`.
  4. Assert logcat contains `OmniBootstrap: session cookie installed` **and** a screenshot shows the account **in-game / logged in** (not Sign In) — the exact success signal prod showed.
  5. **Multi-account:** launch the **same** offset with a **second** account; assert it logs in as that account (cookie swap clean, no residual state).
  6. Assert `--apk-offset` is **rejected on prod** / without `OMNI_DEV_MODE`.
- [ ] **Step 4: Commit** `feat(offsets): apk-offset list/remove; end-to-end dev login verified`.

---

## Verification (whole-plan)

1. `python3 -m pytest tests/ -q` green.
2. On-device: `apk-offset create` with a bootstrapped Roblox → `start --dev --apk-offset` shows the `OmniBootstrap: session cookie installed` line + a logged-in screenshot (the prod-equivalent success).
3. The same offset logs in **two different accounts** correctly.
4. `--apk-offset` rejected on prod / without `OMNI_DEV_MODE`; a **stock** APK refused by `apk-offset create`.
5. Default/prod `start` unchanged (no offset path regression).

## Self-Review notes (author)

- **Spec coverage:** implements the B spec — per-APK reusable offset, bootstrapped-Roblox login (same as prod, never frida), dev-only gating, prod untouched, stock-APK guard.
- **Dependency:** a bootstrapped APK from omni-agent (`inject_session_bootstrap`) — omnidroid consumes, never reimplements the RE toolchain. Task 5's on-device step needs it.
- **Risk:** the multi-account-shares-one-offset behavior (Task 5 step 5) is the real unknown — `snapshot=on` should make each account's runtime cookie throwaway over a shared installed Roblox, exactly like prod's shared baked Roblox; verify on-device. If residual per-account state leaks, revisit whether the offset needs the account's `/data/data/com.roblox.client` excluded.
- **Type consistency:** `offsets.qcow2_path(name)`, `build_acct(..., apk_offset=)`, `acct["apk_offset"]`, `_apk_has_bootstrap(apk)` used consistently across tasks.
