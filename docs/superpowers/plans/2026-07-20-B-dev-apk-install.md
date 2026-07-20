# B — Dev `--apk` Install Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a dev-only `omnidroid start <user> --dev --apk <apk>` that boots the dev base, installs the given Roblox APK, delivers the cookie, and joins — logging in like prod (plus dev tools) when handed a bootstrapped APK, and failing *loudly* (not silent Sign In) when handed a plain one. Prod is untouched. No offset disks.

**Architecture:** `start --dev --apk` is a dumb installer: boot the dev base diskless (`snapshot=on`), `adb install` the APK after boot (reusing the existing ABI-safe install path), then the normal `deliver_session` flow runs. The APK must already contain the `OmniBootstrap` login component — that is omni-agent's responsibility (its `inject_session_bootstrap`/`launch_roblox_build` tooling), NOT omnidroid's. omnidroid adds a post-delivery check for the `OmniBootstrap: session cookie installed` logcat line so a plain (non-login-capable) APK surfaces as `not_logged_in` instead of a silent Sign In.

**Tech Stack:** Python 3.13+, QEMU arm64 dev base (`snapshot=on`), adb, pytest.

## Global Constraints

- **Dev-only.** `--apk` is valid only with `--dev` on the dev base, gated by `assert_dev_allowed` / `OMNI_DEV_MODE`. **Prod never accepts `--apk`** (prod Roblox is baked, immutable). Reject `--apk` without `--dev`, on a prod base, or without `OMNI_DEV_MODE`.
- **omnidroid does NOT bootstrap/decode/resign.** It installs whatever APK it is handed. The bootstrap injection is omni-agent's job (existing `inject_session_bootstrap`).
- **Loud failure, never silent.** After session delivery, if `OmniBootstrap: session cookie installed` is absent from logcat within a bounded wait, the result reports `not_logged_in` (the APK lacked the bootstrap) — the exact silent bug we are closing.
- **Diskless preserved.** Ephemeral `snapshot=on`; the install is thrown away on stop; re-installed each launch. `runtime/<user>/` wiped on stop as today.
- **Prod/default `start` path unchanged** (byte-identical when `--apk` is absent).
- **Verified premise:** prod logs in via `OmniBootstrap` (on-device screenshot + logcat proof) — the target success signal.
- Run tests: `python3 -m pytest tests/ -q` from the repo root.

## File Structure

```
omnidroid/engine.py   # cmd_start: --apk parser flag (dev-gated); install-after-boot;
                      #   post-delivery OmniBootstrap logcat check -> not_logged_in
tests/test_session.py # --apk dev-gating; install-invoked-when-given; not_logged_in on missing bootstrap line
```

No new modules; this is a focused addition to the existing `start` flow.

---

### Task 1: `--apk` parser flag + dev gating

Add the flag and its validation; no install behavior yet (that's Task 2).

**Files:** Modify `omnidroid/engine.py` (`start` parser + early validation in `cmd_start`); `tests/test_session.py`.

**Interfaces:**
- `start` gains `--apk <path>` (default None).
- `cmd_start`: if `args.apk` is set, require `dev` truthy (from `_dev_mode_for_play`) AND the resolved base to be the dev base (`assert_dev_allowed` already runs via `build_acct`); else `fail("apk_dev_only", ...)`. Validate the path exists (`fail("bad_apk", ...)`).

- [ ] **Step 1: Write failing tests** — `--apk x.apk` without `--dev` → `fail("apk_dev_only")`; `--apk /nonexistent` with `--dev` → `fail("bad_apk")`; `--apk` present + `--dev` + existing file → passes validation (mock the rest of cmd_start). Mirror the `StartHomeVsJoin`/`PlayGatesOnLogin` mocking style.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** the parser flag + the two guards early in `cmd_start` (before `build_acct`).
- [ ] **Step 4–5: tests + full suite green.**
- [ ] **Step 6: Commit** `feat(dev-apk): start --apk flag, dev-only gated`.

---

### Task 2: Install the APK after boot

Between `_ensure_booted` and `deliver_session`, install the APK when `--apk` is given.

**Files:** Modify `omnidroid/engine.py` (`cmd_start`); `tests/test_session.py`.

**Interfaces:**
- A helper `_install_apk(acct, apk_path, label) -> dict` reusing the existing install path (`adb_connect` + the ABI-safe `adb install -r -g --no-incremental` with the pin/sig auto-recovery that `cmd_install` already has — factor the shared core if clean, else call the existing install routine). Returns `{ok, error?}`.
- `cmd_start`: after a successful boot and before `deliver_session`, if `args.apk`, call `_install_apk`; on failure, emit the result with `error: apk_install_failed` and `sys.exit(1)` (do not deliver a session to a failed install).

- [ ] **Step 1: Write failing test** — with `--apk` set and `_install_apk` mocked, `cmd_start` calls it after boot and before `deliver_session`; on install failure it aborts before delivery. Assert call ordering.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** `_install_apk` (reuse `cmd_install`'s core) + the wiring in `cmd_start`.
- [ ] **Step 4–5: tests + suite green.**
- [ ] **Step 6: Commit** `feat(dev-apk): install the APK on the dev base after boot`.

---

### Task 3: Loud `not_logged_in` check (post-delivery OmniBootstrap probe)

After session delivery, confirm the account actually logged in by looking for the bootstrap line; report `not_logged_in` if absent (a plain APK).

**Files:** Modify `omnidroid/engine.py` (`cmd_start` result handling; a `_await_bootstrap_login` helper); `tests/test_session.py`.

**Interfaces:**
- `_await_bootstrap_login(acct, timeout=25) -> bool` — polls `adb logcat -d` for `OmniBootstrap: session cookie installed` up to `timeout`s; True if seen. (Condition-based poll, not a fixed sleep.)
- `cmd_start`: only when `args.apk` was used (dev testing), after `deliver_session` reports delivered, call `_await_bootstrap_login`; if False, set `result["ok"]=False`, `result["error"]="not_logged_in"`, print a clear message ("the installed APK has no OmniBootstrap — a plain/stock Roblox cannot log in; build it via omni-agent's inject_session_bootstrap"), and exit non-zero. (Do NOT run this probe on the normal prod path — prod is trusted/baked.)

- [ ] **Step 1: Write failing test** — with `_await_bootstrap_login` mocked False and `--apk` used, `cmd_start` result is `ok:False, error:"not_logged_in"`; mocked True → `ok:True`. Mock deliver_session delivered.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** `_await_bootstrap_login` (logcat poll) + the gated check in `cmd_start`.
- [ ] **Step 4–5: tests + suite green.**
- [ ] **Step 6: Commit** `feat(dev-apk): loud not_logged_in when the APK lacks the bootstrap`.

---

### Task 4: On-device end-to-end verification

Prove it against the arm dev base with a real bootstrapped Roblox.

**Files:** none (verification); fix-forward any issue found.

- [ ] **Step 1:** omni-agent bootstraps a Roblox APK (`inject_session_bootstrap` on a plain Roblox) → `roblox-bootstrapped.apk`.
- [ ] **Step 2:** `OMNI_DEV_MODE=1 omnidroid start admn1b12farm3 --dev --apk roblox-bootstrapped.apk --place 8737899170 --no-window --json`.
- [ ] **Step 3:** Assert logcat has `OmniBootstrap: session cookie installed` **and** a screenshot shows the account **in-game** (logged in) — the prod-equivalent success. Result `ok:true`.
- [ ] **Step 4:** `start --dev --apk <plain-roblox.apk>` → result `ok:false, error:"not_logged_in"` (loud, not a silent Sign In).
- [ ] **Step 5:** `omnidroid start <user> --apk x.apk` (no `--dev`, and on prod) → rejected `apk_dev_only`. Prod `start` (no `--apk`) → unchanged, logs in.
- [ ] **Step 6: Commit** `test(dev-apk): end-to-end dev login verified with a bootstrapped APK`.

---

## Verification (whole-plan)

1. `python3 -m pytest tests/ -q` green.
2. On-device: bootstrapped APK via `--dev --apk` → `OmniBootstrap` line + logged-in screenshot.
3. Plain APK via `--dev --apk` → loud `not_logged_in`, no silent Sign In.
4. `--apk` rejected without `--dev` / on prod / without `OMNI_DEV_MODE`.
5. Prod/default `start` unchanged.

## Self-Review notes (author)

- **Spec coverage:** implements the simplified B spec (dev `--apk` dumb install, dev-gated, prod untouched, loud not-logged-in, no offsets, omni-agent owns bootstrap).
- **Cross-repo dependency (Task 4):** a login-capable APK requires omni-agent's `inject_session_bootstrap`. The real end-to-end dev-login fix is: omni-agent must feed a bootstrapped APK (it already can). omnidroid's job here is the install path + the loud check.
- **Risk:** the logcat probe timing (Task 3) — the `OmniBootstrap` line appears after Roblox cold-starts; 25s bound may need tuning on-device (Task 4 confirms).
- **Type consistency:** `_install_apk(acct, apk_path, label)`, `_await_bootstrap_login(acct, timeout)`, `args.apk` used consistently.
