# Warm-restore verification (run on a host with real base images)

**Goal:** get the Phase 0 measurement the warm-restore-boot-cache spec (§10)
needs — how much of `omnidroid start` the warm-restore cache actually saves,
split into the Android half (boot) and the Roblox half (join). That split is
what decides whether the product's "~15s from `start` to standing in the
place" target is reachable as-is, or needs the deferred follow-up (freezing
the snapshot with Roblox already warm, not just Android).

This is a **host tool**, not a unit test. It launches real QEMU instances and
needs real assets. It is not run in CI and never will be.

---

## Prerequisites

- An **existing account** already logged in (`omnidroid login <name>`, or
  `omnidroid login <name> --token-file cookie.txt`) with a **place set**
  (`omnidroid session <name> --place <id>`) — without a place the boot lands
  on the home screen and `game_foreground` still marks, but there is no
  "join" to measure.
- A **base installed** for that account's arch (`omnidroid bases` shows the
  current one; `omnidroid use-base <tag>` to switch).
- A **baked offset** the account resolves to (`omnidroid offset create
  <version> --apk <apk>`, or rely on the base's default offset). The account
  must have already completed at least one ordinary boot on this host —
  a truly first-ever boot pays full dexopt (up to 25 minutes) and is not
  representative of either a cold-cache boot or a warm restore.
- Enough free disk for a bake: the cache requires the entry's projected size
  **plus a 10 GiB reserve** before it will bake at all (spec §5.4). If the
  images volume is nearly full, run 1 will cold-boot successfully but
  silently skip the bake, and run 2 will cold-boot again — not a bug, just
  no room for the cache.
- **No other running instance sharing this account's cache key** (same base,
  offset, and mode). See the note on the interim concurrency rule below —
  if one is running, `stop` it first or the restore path will refuse and
  both runs will cold-boot.

## Running it

```sh
python3 tools/warm_restore_check.py <account>
```

It stops any running instance for `<account>`, runs `omnidroid start
<account> --json --no-window` twice (stopping the instance between and
after each run so nothing is left behind), and times both. Run 1 cold-boots
and — if the disk budget allows — bakes a cache entry as a side effect. Run
2 should restore from that entry.

Add `--timeout <seconds>` if a very slow host needs longer than the default
900s per `start` call.

## Reading the output

For each run it prints the wall-clock time and the parsed `timings` block
(`boot`, optionally `apk_install`, `session_delivered`, `game_foreground` —
whichever stages that boot reached), then a side-by-side split:

```
ANDROID vs ROBLOX SPLIT  (android = time to `boot`; roblox = boot -> game_foreground)
                            run 1 (cold)    run 2 (warm)
android (boot)                    19.80s           3.00s
roblox (boot->game)               12.70s           9.40s
```

- **android (boot)** is the number that was already known to drop from
  ~20s to ~3s under warm restore. Confirm it does — that's the cache
  working at all.
- **roblox (boot->game)** is the number that had never been measured. If it
  stays roughly the same between run 1 and run 2 (expected — a warm restore
  currently only freezes the Android side, not Roblox), that gap is exactly
  what the deferred "freeze with Roblox already warm" follow-up would need
  to close to hit the 15s target. If run 2's total wall time is already
  under ~15s without that follow-up, it isn't needed yet — the printed `15s
  end-to-end target` line makes this an at-a-glance read, though it's
  informational only and is not itself the PASS/FAIL criterion (see below).

## What PASS means

The tool prints `RESULT: PASS` only when **both**:

1. both runs report `ok: true` in their `start --json` payload, and
2. run 2 is **materially** faster than run 1 — at most 75% of run 1's wall
   time, **and** at least 2.0s faster in absolute terms. (Both margins
   guard against reading scheduler noise as a win at either end of the
   scale.)

A FAIL with both runs `ok: true` but no material speedup usually means the
bake didn't happen (disk budget, see Prerequisites) or the restore path
declined and silently cold-booted again (e.g. the concurrency rule below).
Re-run with nothing else using the same account/base/offset/mode and check
free disk before concluding the cache itself is broken.

If `start --json`'s output can't be parsed at all (a crash before it prints
its JSON line), the tool exits loudly with the tail of stdout/stderr instead
of a bare traceback or a silent hang — that tail is the thing to attach to a
bug report.

---

## Manual follow-up checklist

The automated tool only proves the cache is *fast*. These three checks —
run once, by hand, on the same host right after a PASS — prove it's
*correct*. They correspond to Steps 3 and 4 of the warm-restore-boot-cache
plan's task 12.

### (a) CLI surface behaves identically on a restored instance

Start the account again (it should restore, per the run above) and run:

```sh
python3 -m omnidroid list --json
python3 -m omnidroid screenshot <account>
python3 -m omnidroid debug-info <account> --json
```

*(The plan's brief calls this last one `status --json`; this codebase's
actual per-instance inspector is `debug-info --json` — same intent: ports,
mode, offset, root/devkit availability for one running instance.)*

Expected: same shape as against a cold-booted instance — `list --json`
shows the account `running` with real `adb_port`/`vnc_port`/`qmp_port`,
`screenshot` pulls a real frame, `debug-info --json` reports the same
offset/mode/ports a cold boot would. Nothing about the executor surface
should look different from a warm-restored instance.

### (b) The account is genuinely logged in and in its place

From the `screenshot` above (or a VNC view), confirm the account is
actually signed into Roblox and standing in its place — not on a login
screen. This is the important check: the **golden cache entry is
account-free** (the frozen image belongs to no one), so a restore that
looks fast but lands on a login screen would mean per-launch cookie
injection stopped happening against a cached image. A restore that's both
fast *and* logged in proves cookie injection still runs on every launch,
warm or cold.

### (c) Two accounts sharing one cache entry: first restores, second cold-boots, neither goes `offline`

Pick (or create) **two accounts that resolve to the same golden entry** —
same base, same offset, same mode. Start the first, let it settle, then
start the second while the first is still running:

```sh
python3 -m omnidroid start <account-A> --json --no-window
python3 -m omnidroid start <account-B> --json --no-window   # while A is still up
adb devices
```

Expected: `<account-A>` restores (fast). `<account-B>` **cold-boots**
(slow, today's ordinary speed) — this is the interim rule in
`_warm_cache_allowed` refusing a second concurrent restore off the same
entry, not a bug. Both must show up as `device` in `adb devices` — **neither
may show `offline`**.

**Why this check exists.** A second instance restored *concurrently* from
the *same* cache entry comes up alive on VNC but permanently `offline` on
adb — the root cause is not yet known (see the design spec's §8b: ruled out
so far — not a per-instance defect, not pre-existing since two concurrent
cold boots are fine, not host-side adb-server dedup since a per-instance
`ANDROID_ADB_SERVER_PORT` doesn't help, and quiescing adbd before the
freeze makes it worse). The interim rule in the code deliberately refuses a
second concurrent restore off one entry specifically to avoid that failure
mode; this check is what confirms the refusal is actually working on a real
host. Do **not** bypass `_warm_cache_allowed`'s in-use check to "test
harder" as part of this checklist — that reproduces the broken state on
purpose and is Phase 1 root-cause work (plan task 12, Step 4), not routine
verification.

---

## Recording results

When you run this, note here (or in the spec's §10/§8b) for the record:

- Host / date:
- Account, base, offset, mode used:
- `timings` blocks for both runs (paste the tool's split table):
- PASS/FAIL and, if FAIL, the printed reason:
- (a)/(b)/(c) checklist results:
