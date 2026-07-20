# Sub-project B — Dev `--apk` Install (+ the dev cookie-login fix)

**Date:** 2026-07-20
**Status:** Approved direction (supersedes the earlier offset-disk design)
**Scope:** A dev-only `start --apk` that installs a Roblox APK onto the dev base at launch, so a
dev instance logs in like prod (plus the dev tools). Prod is unchanged. This is also the fix for
the "dev cookie-login lands on Sign In" bug.

## The bug, root-caused (verified on-device)

`start --dev` on a plain Roblox = Sign In page even though the cookie is delivered. Cause: a
**plain/stock Roblox APK has no code that reads the session cookie**; login only happens if the
Roblox build contains the injected **`OmniBootstrap`** component (reads the cookie from the kiosk's
`SessionProvider`, writes it into Roblox's WebView cookie jar on startup). Proven:

- **Prod** (`start`, no `--dev`): in-game screenshot **as the account** + logcat
  `OmniBootstrap: session cookie installed (1199 chars)`. ✅ logs in.
- **Dev** (`--dev`, plain Roblox installed): Sign In screenshot, **no** `OmniBootstrap` line. ❌.

omni-agent has been installing **plain** Roblox on dev — that is the entire bug.

## Design (simplified — no offset disks)

### Split of responsibility

- **omni-agent produces a login-capable Roblox APK.** Its "create the Roblox APK" step must run
  the bootstrap injection it already owns (`decode_apk → inject_session_bootstrap → recompile_apk →
  sign_apk`; the `launch_roblox_build` tool already chains these). Plain Roblox in → bootstrapped
  Roblox out. The user does not manage this as a separate step — it is part of building the APK.
- **omnidroid `start --dev --apk <apk>` is a dumb installer.** Boot the dev base (ephemeral,
  `snapshot=on`), `adb install` the given APK after boot, deliver the cookie, join. It does NOT
  bootstrap, decode, or resign — it installs whatever it is handed. No RE tooling in the engine.

### Behavior

`omnidroid start <username> --dev --apk <path-or-name>`:
1. Boot the dev base diskless (`snapshot=on`) — dev tools (frida/Magisk) already present.
2. After boot, `adb install -r -g --no-incremental <apk>` (reuse the existing install path with its
   ABI-safety + pin/sig auto-recovery).
3. Deliver the session (cookie + place) via the kiosk → the APK's bootstrap installs the cookie →
   logged in → join (or home with no `--place`). Same login path as prod.
4. On stop: the whole thing is thrown away (`snapshot=on` + `runtime/<user>/` wipe). Next launch
   re-installs — fine for dev iteration (each run tests a specific build).

- **Dev-only.** `--apk` is rejected on a prod base / without `OMNI_DEV_MODE` (same
  `assert_dev_allowed` boundary). **Prod never accepts `--apk`** — its Roblox is baked, immutable.
- **Loud failure on a non-login-capable APK.** After install + session delivery, if the
  `OmniBootstrap: session cookie installed` line does NOT appear (i.e. a plain APK was installed),
  `start --apk` reports a clear `not_logged_in`/`stock_apk` warning in its result instead of
  silently showing "launched: true" — so the old silent-Sign-In failure cannot recur unnoticed.

### Non-goals

- No offset disks (scrapped — `--apk` installs directly, re-installed per ephemeral boot).
- No bootstrap/decode/resign in omnidroid (omni-agent's job).
- Prod unchanged; frida never used for login.

## Open questions (resolve in planning)

1. **APK source for `--apk`:** a filesystem path each call, or a small named cache of dev APKs?
   Path is simplest; a name cache is a convenience. Lean: path.
2. **Re-install cost:** `snapshot=on` throws the install away each boot, so a ~130MB re-install per
   launch. Acceptable for dev iteration; note it. (If it becomes painful, a future persistent dev
   data overlay could hold the install — deferred, not now.)
3. **The loud-failure check:** poll logcat for the `OmniBootstrap` line for N seconds after
   delivery; absence → `not_logged_in`. Confirm timing.

## Verification

1. omni-agent bootstraps a Roblox APK; `OMNI_DEV_MODE=1 omnidroid start <user> --dev --apk
   <bootstrapped.apk> --place <id>` → logcat `OmniBootstrap: session cookie installed` + in-game
   screenshot (logged in). Same success signal prod showed.
2. `start --dev --apk <plain-roblox.apk>` → clear `not_logged_in` warning (not a silent Sign In).
3. `--apk` rejected on prod / without `OMNI_DEV_MODE`.
4. Default/prod `start` unchanged.
