# Sub-project A — Diskless Account Model + Refactor Foundation

**Date:** 2026-07-19
**Status:** Approved design, ready for implementation planning
**Scope:** First of three sub-projects in the OmniDroid reimagining. This one rebuilds the
account/disk model and lays the refactor foundation everything else sits on.

## Context

OmniDroid runs Android (Roblox) instances on QEMU for a farming setup targeting 100+ accounts
on a single large VPS. The current model creates per-account disk overlays and per-account
folders (`accounts/<name>/` holding `account.json`, `session.json`, `run.json`, `efivars.fd`,
logs). This does not scale to 100+ accounts and mixes persistent identity with throwaway
runtime state.

Today the runtime logic lives in a single 6517-line `manager/omni.py`. The account registry is
split between a root `accounts.json` (just `{version, accounts:[names]}`) and the per-account
folders. An `--ephemeral` mode already boots shared base templates `snapshot=on` with no
per-account overlay — this design makes that the *only* model for accounts.

This is sub-project **A**. Two later sub-projects, designed separately, are out of scope here:

- **B** — dev APK-swap offset disks + fixing the dev cookie-login bug.
- **C** — launch-time resource profiles (farm vs. playable, GPU toggle) + scaling to ~120 concurrent.

## Goals

1. **Diskless accounts.** No per-account disk overlays and no per-account folders. An account
   is a small JSON record; the base image is booted `snapshot=on` and the session is carried by
   RAM, not storage. Storage does not scale per account.
2. **Single account store.** One `accounts.json` holds every account (username, token, default
   place, base preference). The `accounts/` folder and per-account files are retired.
3. **Clean persistent/runtime split.** Persistent identity lives in `accounts.json`; throwaway
   per-boot state lives in a `runtime/<id>/` dir that is wiped on stop after logs are archived.
4. **Dynamic resources at launch.** Ports are allocated from pools at launch, never stored on
   the account — the key enabler for many concurrent instances.
5. **Refactor foundation.** Move `manager/omni.py` to a root `manager.py` shim plus an
   `omnidroid/` package split into focused modules, with no feature regressions.
6. **Cross-platform, one source of truth for paths.** Windows, macOS, and Linux; x86 + arm on
   prod, arm-only on dev. All path/arch logic isolated in `config.py`.
7. **CDN-ready, no CDN built.** Leave clean seams for a future auto-download/installer without
   building any of it now; work from manually-placed local images today.

## Non-goals (explicitly deferred)

- Dev APK-swap offset disks and the dev cookie-login bug (**sub-project B**).
- GPU/farm resource profiles and 120-concurrent scaling/tuning (**sub-project C**).
- The CDN server and image-publishing/installer pipeline (separate future project).

## Design

### 1. Data model

**Persistent — `accounts.json`** (single file in the in-project data dir, mode `0600`):

```json
{
  "version": 2,
  "accounts": [
    {
      "username": "admn1b12farm3",
      "token": "<roblox cookie/token>",
      "place_id": null,
      "base": "prod",
      "proxy": null,
      "group": null,
      "created_at": 1784438529.0,
      "notes": null
    }
  ]
}
```

- `username` is the identifier used on the command line (required positional).
- `token` is the captured Roblox cookie/session used for cookie-login at boot.
- `place_id` is an optional default place to join; `null`/absent means boot to home.
- `base` is `"prod"` or `"dev"`.
- `proxy` and `group` are **reserved for future use** — always written (default `null`) so later
  updates (per-account proxying, account grouping/tagging) need no schema migration. A does not
  consume them yet.
- `version` supports future schema migration. (Current on-disk shape is `{version, accounts:[names]}`;
  the new shape replaces it — see Migration.)
- This record **is** the entire account. No folder, no disk, no stored ports.

**Runtime — `runtime/<id>/`** (throwaway, created at launch, wiped on stop):

- `efivars.fd` — writable per-boot UEFI vars, **arm only** (x86 boots kernel+initrd directly, no efivars).
- `qemu.log` — QEMU stdout/stderr for this boot.
- `run.json` — `{pid, ports:{adb,qmp,vnc}, username, mode, started}`.
- screenshots captured during the run.
- Total footprint under ~5MB. All guest disk writes go to a `$TMPDIR` overlay that QEMU creates
  for `snapshot=on` and auto-deletes on exit — that transient scratch (a few hundred MB to ~1GB
  per session) is **not** in `runtime/`. `$TMPDIR` is configurable so it can be pinned to a fast disk.

**Boot model:** the base image is booted `snapshot=on`; the account's `token` is injected for
cookie-login; if `place_id` is set (or `--place` given) the instance joins that place, otherwise
it sits on the Roblox mobile home screen. **One live instance per account at a time** — the
identifier is the username; `start` on an already-running username is rejected (with a clear
message) rather than launching a duplicate.

**Ports** are allocated dynamically from pools at launch. Config defines the pool start values
(`adb_port_start`, `qmp_port_start`, `vnc_port_start`); the launcher picks free ports, records
them in `run.json`, and releases them on stop.

### 2. Logging

```
logs/
  omnidroid.log                     # rolling manager/CLI log (actions, errors), rotated
  instances/
    <username>/
      2026-07-19T18-14-02/          # one dir per run (timestamped)
        qemu.log
        session.json                # cookie/place used, exit status, duration
        *.png                       # screenshots from that run
        logcat.log                  # only if capture was requested
```

- On **stop**: the useful files (qemu.log, session summary, screenshots, logcat if any) are
  archived from `runtime/<id>/` into `logs/instances/<username>/<timestamp>/`, then
  `runtime/<id>/` is wiped.
- **logcat is opt-in only.** Never automatic. omni-agent can enable it in dev; in prod it is a
  manual capture on support request. No logcat overhead at scale by default.
- **Retention is time-based, not run-count.** Default: keep the last **7 days** of run archives
  per username, auto-prune older; the day count is configurable. Time-based retention survives
  crash-reopen loops (many short runs) gracefully. If per-run dirs become noisy under crash
  loops, grouping them under a per-day folder is an available option (not baked in).

### 3. Command surface

Invoked as `omnidroid <cmd>` (see Refactor for the three equivalent invocation routes).

| Command | Behavior |
|---|---|
| `omnidroid login <username>` | Selenium/cookie capture → registers `{username, token, place_id, base}` into `accounts.json`. **Replaces `create`** (account creation no longer creates a disk). |
| `omnidroid start <username> [--dev] [--place <id>]` | Diskless boot (`snapshot=on`), allocate ports, inject cookie. No `--place` → Roblox mobile home; `--place <id>` → join that place. `--dev` selects the dev base. |
| `omnidroid stop <username>` | Stop the instance, archive logs, wipe `runtime/<id>/`, release ports. |
| `omnidroid remove <username>` | Delete the account entry from `accounts.json`. |
| `omnidroid accounts` | List registered accounts. |
| `omnidroid list` | List running instances. |
| `omnidroid session <username>` | View/refresh an account's token or place_id. |

**Removed:** `create`, `play`, `resume` (`start` covers launch; there is no saved state to resume
with `snapshot=on`). **Preserved:** all base-image build/maintenance commands (update-base,
rebuild, brand, bake-game, build-dev-base, kioskify, doctor, setup, etc.), moved into `baseimg.py`.

### 4. Refactor / module layout

```
manager.py               # root: 3-line shim -> omnidroid.cli:main (no-install dev route)
omnidroid/
  __init__.py            # package API surface (import omnidroid)
  __main__.py            # `python -m omnidroid`
  cli.py                 # argparse + dispatch (thin)
  config.py              # paths.json, constants, per-OS/per-arch path + image + qemu-bin resolution
  accounts.py            # accounts.json store: load/save/CRUD
  login.py               # selenium cookie capture + cookie injection at boot
  launch.py              # port-pool allocation, runtime/<id>/ lifecycle, qemu-arg build, snapshot=on boot
  disks.py               # qemu-img helpers, image resolution (dev APK-offset creation lands in B)
  adb.py                 # adb wrappers
  logs.py                # logging setup, per-run archive, time-based retention
  baseimg.py             # heavy dev/build commands (update/rebuild/brand/bake/build-dev/kioskify)
  cookies.py capture.py vncview.py   # existing modules, moved in unchanged
pyproject.toml           # console_scripts: omnidroid = "omnidroid.cli:main"
```

- Three equivalent invocation routes, all hitting `omnidroid.cli:main`:
  `omnidroid <cmd>` (after `pip install -e .`), `python -m omnidroid <cmd>`, `python manager.py <cmd>`.
- omni-agent and the future UI `import omnidroid` and call the API directly — they do not shell out.
- The ~15 dev/build commands (the bulk of the old 6500 lines) are quarantined in `baseimg.py`, so
  the everyday runtime path (login/start/stop) lives in small, readable modules. `baseimg.py` may
  be subdivided further during implementation planning if it grows unwieldy (e.g. `baseimg/` with
  `brand.py`, `bake.py`, `devbase.py`); the runtime modules stay as listed.
- **No feature regressions**: every preserved command behaves as before after the move.

### 5. Cross-platform / paths (`config.py` owns all paths)

- **Images dir** (base images, huge, external, never in git):
  - Windows `%USERPROFILE%\OmniImages` · macOS `~/OmniImages` · Linux `~/OmniImages`
  - Override: `OMNI_IMAGES_DIR`. (macOS is newly added; current `paths.json` only has win/linux.)
- **Data dir** (`accounts.json`, `logs/`, `runtime/`): defaults **inside the project folder** so
  "transfer everything" = copy the one small folder; override `OMNI_DATA_DIR`.
- **QEMU binary + arch resolution** centralized: `qemu-system-x86_64` vs `-aarch64`, `.exe` on
  Windows, bundled `qemu/` vs system. Prod = x86 + arm selected by host arch; dev = arm-only.
- Nothing else in the codebase hardcodes a path — all path/arch decisions route through `config.py`.

### 6. Image provisioning (CDN-ready seam, no CDN built)

- `config.py` carries an **optional** CDN base URL + a per-image **manifest** (files, version,
  checksum) for `base_x86`, `base_arm`, and the `dev` set. Unset today → pure local-file mode.
- One **image-resolver seam** — `ensure_image(name) -> local path`:
  - Today: checks presence + verifies; if missing, errors with clear guidance (place the image / run setup).
  - Future: the *same* function auto-downloads from the CDN. Per-image versions in the manifest
    let a future updater download only the image that changed (e.g. a rebaked APK), never the whole set.
- **No download client is built in A** (or only a stub that reports "CDN not configured").
- Dev images are gated (`--dev`) so a customer build path can never require or pull them.
- The installer (small setup file, Cloudflare CDN, per-OS install + desktop shortcut) is a
  separate future project that plugs into these seams.

### 7. Migration

Fresh start: delete the existing `accounts/admn1b12farm3/` and `accounts/admn1b12farm4/` folders,
retire the `accounts/` directory, and re-`login` the accounts. No token migration script needed
(only two dev accounts exist). Also delete the stale `test.apk` temp file as part of cleanup.

## Testing / verification

Because this is a refactor plus a behavior change, verification exercises the real flows, not just
imports:

1. **Prod cookie-login + place-join still works** — the known-good path must survive the refactor
   (login → start `--place <id>` → in-game).
2. **Start-to-home works** — `start <username>` with no `--place` lands on the Roblox mobile home.
3. **Concurrency** — 2–3 diskless instances start with distinct dynamically-allocated ports and
   run simultaneously off the shared base (`snapshot=on`).
4. **Stop lifecycle** — stop archives logs into `logs/instances/<username>/<ts>/`, wipes
   `runtime/<id>/`, and releases ports.
5. **Retention** — runs older than the configured window are pruned; recent ones kept.
6. **Cross-platform paths** — path/arch/qemu-bin resolution verified on the current host (macOS
   now included), with Windows/Linux branches guarded.
7. **Invocation routes** — `omnidroid`, `python -m omnidroid`, and `python manager.py` all dispatch
   the same commands; `import omnidroid` exposes the API omni-agent uses.

## Open questions

None blocking. The CDN server + installer are intentionally deferred; A only defines and consumes
the seams.
