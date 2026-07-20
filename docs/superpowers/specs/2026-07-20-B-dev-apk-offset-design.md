# Sub-project B — Dev APK-Offset Disks (+ the dev cookie-login fix)

**Date:** 2026-07-20
**Status:** Approved design direction, ready for implementation planning
**Scope:** The dev-side APK-swap mechanism, which is also the fix for the "dev cookie-login
lands on the Sign In page" bug. Prod is unchanged.

## Context — the bug this closes

Real end-to-end testing showed `omnidroid start <user> --dev --place <id>` landing on Roblox's
**Sign In / Create Account** screen even though the cookie was delivered (`has_token: true`,
kiosk `launched: true`). Root cause, confirmed against `contracts/omni-session.md` §1.2 and the
omni-agent skill docs:

> A **stock (unmodified) Roblox APK has NO code path that reads a session cookie.** The deep link
> (`roblox://…placeId=…`) has no auth parameter; the session is the `.ROBLOSECURITY` cookie in
> Roblox's own WebView cookie jar, which only code running as Roblox can write. Installing a stock
> build and firing the deep link lands on Roblox's own login screen — while the tools still report
> `install: ok` / `launched: true`.

Logging in requires the **injected bootstrap** (`OmniBootstrap`, source
`omnidroid/bootstrap/src/com/omni/bootstrap/OmniBootstrap.java`) inside the Roblox process: it
reads the token from the kiosk's `SessionProvider` and installs it into Roblox's cookie jar. A
build only logs in if it was produced by `decode_apk → inject_session_bootstrap → recompile_apk →
sign_apk` (omni-agent's tooling). The confirmation is the logcat line
`OmniBootstrap: session cookie installed (N chars)` — absent in the failing repro (a stock APK).

**Prod works because its baked Roblox is a bootstrapped build. Dev failed because a stock APK was
installed.** This sub-project makes the dev path install a bootstrapped Roblox — via a reusable
per-APK offset — so dev login works exactly like prod.

## Goals

1. **Same login mechanism on dev and prod.** Both use the injected `OmniBootstrap`. **Frida is
   never used for login** — prod has no frida, and dev testing must validate the real prod login
   path. (Frida stays a dev-only debugging tool for other purposes.)
2. **Prod unchanged.** The prod base bakes a bootstrapped Roblox; no offsets, no `--apk*` flags.
3. **Dev base stays clean** — it contains **no APK files**, just the dev base + devkit.
4. **Per-APK offset, reusable across accounts.** An offset is keyed to an **APK build**, not an
   account. One offset (e.g. "roblox-2.726") launches with *any* number of accounts — the account
   is just a cookie delivered at runtime, exactly as prod shares one baked Roblox across accounts.
5. **Diskless account model preserved.** Accounts stay JSON in the store; offsets are per-APK
   artifacts, not per-account. Nothing per-account is baked to disk.

## Non-goals

- Changing prod (baked, immutable Roblox — unchanged).
- Frida-based login (explicitly rejected — see Goal 1).
- Reworking the bootstrap-injection tooling itself (it exists in omni-agent and works).

## Design

### Ownership boundary (who does what)

- **omni-agent owns APK manipulation.** It already has the Android RE tooling
  (`apktool`/`baksmali`, `omni-agent/tools/session_bootstrap.py::inject_session_bootstrap`) and
  produces a **bootstrapped, signed** Roblox APK: `decode_apk → inject_session_bootstrap →
  recompile_apk → sign_apk`. omnidroid does **not** reimplement this (keeps heavy RE tools out of
  the engine).
- **omnidroid owns disks + boot.** It packages a bootstrapped APK into an offset and mounts it at
  launch.

So the input to omnidroid's offset flow is an **already-bootstrapped APK** produced by omni-agent.
(An `apktool` toolchain is not an omnidroid dependency.)

### What an APK-offset is (technically)

An APK-offset is a **qcow2 COW overlay on the dev `/data` template** (`base_arm_devdata.qcow2`)
with the bootstrapped Roblox **installed into it**. Storage chain at launch:

```
base_arm_devdata.qcow2   (dev /data template: kiosk + provisioning, clean, no Roblox)
   └── <apk>.offset.qcow2 (COW overlay: + the installed bootstrapped Roblox — the persistent, per-APK delta ~130MB)
         └── per-boot throwaway (snapshot=on: the account's runtime cookie/state, discarded on stop)
```

- The offset is **small** (only the Roblox-install delta over the template), persistent, and
  **reused across accounts**.
- At launch the offset is opened **`snapshot=on`** (like every diskless instance), so an account's
  runtime login state is written to a throwaway top layer and discarded on stop. The **installed
  Roblox** on the offset is read-only in effect and shared by all accounts — the account cookie is
  injected per launch by the bootstrap, exactly as prod shares one baked Roblox.
- This mirrors how prod works (shared baked Roblox + per-account runtime cookie), so dev testing
  validates the prod login path.

### Creating an offset (the "empty offset → boot → install → capture" flow)

`omnidroid apk-offset create <name> --apk <bootstrapped.apk>` (dev-only):

1. Create an empty COW overlay on `base_arm_devdata.qcow2` → `<name>.offset.qcow2`.
2. Boot a throwaway dev instance with that overlay as `/data` (writes land on the overlay, **not**
   `snapshot=on` here — this boot must persist the install).
3. `adb install` the **bootstrapped** APK into it (auto-recover on signature/pin conflicts, as
   `install_apk_on_emulator` already does).
4. Clean shutdown; the overlay now persistently holds the installed bootstrapped Roblox.
5. Register the offset (name → path, source APK identity/version, created_at) so `start` can
   reference it by name.

(This is analogous to `update_kiosk_arm`'s boot→install→capture, but it writes a small COW overlay
instead of flattening a full template.)

### Launching with an offset

`omnidroid start <username> --dev --apk-offset <name>` (dev-only):

1. Resolve the account from the store (cookie/place) — unchanged diskless identity.
2. Boot the dev base with `<name>.offset.qcow2` as `/data`, opened `snapshot=on` (throwaway
   per-boot writes; the installed Roblox is shared read-mostly).
3. Deliver the session (cookie + place) via the kiosk → the bootstrap in the offset's Roblox
   installs the cookie → **logged in** → join (or home). Same path as prod.
4. On stop: wipe the per-boot throwaway + `runtime/<username>/` (unchanged). The offset persists
   for the next account.

The **same offset** is launched with many different accounts — only the runtime cookie differs.

### Flags & gating

- `--apk-offset <name>` on `start` — **dev-only**. Rejected on a prod base / when `OMNI_DEV_MODE`
  is not set (same `assert_dev_allowed` boundary the dev base already uses). **Prod never accepts
  `--apk-offset`.**
- `apk-offset` subcommands (`create`, `list`, `remove`) — dev-only.
- Prod `start` has no APK flag — it always uses the baked bootstrapped Roblox.

### Command surface (new)

| Command | Behavior |
|---|---|
| `omnidroid apk-offset create <name> --apk <bootstrapped.apk>` | Build a per-APK offset (boot dev, install, capture). Dev-only. |
| `omnidroid apk-offset list` | List registered offsets (name, source APK version, size, created). |
| `omnidroid apk-offset remove <name>` | Delete an offset disk + its registration. |
| `omnidroid start <user> --dev --apk-offset <name>` | Launch the account against that offset's installed Roblox. Dev-only. |

## Open questions (resolve during planning)

1. **Bootstrapped-APK handoff:** does `apk-offset create` take a path to an omni-agent-produced
   bootstrapped APK (recommended, clean boundary), or should there be an omni-agent tool that
   drives `apk-offset create` end-to-end (bootstrap → create)? Leaning: omni-agent produces the
   APK; omnidroid `apk-offset create` consumes it. A guard should reject an obviously-stock APK
   (e.g. verify the bootstrap smali/class or a marker is present) so a stock APK can't silently
   produce a non-logging-in offset — reproducing the original bug.
2. **Offset `/data` vs the shared account model:** confirm on-device that one offset booted
   `snapshot=on` across several accounts logs each in correctly (cookie swap overwrites cleanly,
   no residual account state) — the key real-device test.
3. **Offset storage location + registry format** (alongside `OmniImages`? a `offsets/` dir? entry
   in `paths.json` vs a separate registry).
4. **Prod baked-Roblox bootstrap provenance:** document/verify that the prod base's baked Roblox is
   itself a bootstrapped build (so "works like prod" is a real equivalence).

## Verification (when built)

1. `apk-offset create` with a bootstrapped Roblox → an offset that, on `start <user> --dev
   --apk-offset <name>`, shows `OmniBootstrap: session cookie installed` in logcat and a
   **logged-in** screen (not Sign In) — the exact line absent in the bug repro.
2. The **same** offset launched with **two different accounts** logs each in as itself.
3. `--apk-offset` is **rejected on prod** / without `OMNI_DEV_MODE`.
4. A **stock** (non-bootstrapped) APK is refused by `apk-offset create` (guard), so the original
   bug cannot recur silently.
5. Dev base stays clean (no Roblox) when no offset is used.
