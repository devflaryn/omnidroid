#!/usr/bin/env python3
"""End-to-end check of the warm-restore cache against real base images.

    python3 tools/warm_restore_check.py <account>

Run 1 cold-boots the account and (on a healthy host) bakes a warm-cache
entry as a side effect. Run 2 should then RESTORE from that entry instead of
cold-booting. Both runs go through `omnidroid start --json --no-window`, so
each is timed at the wall-clock level and also parsed for the `timings`
block `start` emits internally (stage marks: `boot`, optionally
`apk_install`, `session_delivered`, `game_foreground`).

This is Phase 0's measurement, not an assertion: the Android half of a boot
was already known to drop from ~20s to ~3s under warm restore, but the
Roblox half -- its own cold start plus joining the place -- had never been
measured at all. Whether the product's ~15s "start -> standing in the
place" target is reachable depends on that number, so the split this script
prints (android = time to `boot`; roblox = `boot` -> `game_foreground`) is
the answer, not just the pass/fail line.

PASS requires BOTH:
  - both runs report `ok: true` in their JSON payload, and
  - run 2 is MATERIALLY faster than run 1 (a relative AND an absolute
    margin -- see `materially_faster()` -- so a noisy few-hundred-ms
    difference on two slow runs can't read as a win).

This is a host tool, not part of the unit suite: it needs a real base image,
a real Roblox account already logged in via `omnidroid login`, and (per the
warm-restore spec's interim concurrency rule) no other running instance
sharing this account's cache key. It never runs in CI.
"""
import argparse
import json
import subprocess
import sys
import time

# Declared order `cmd_start` marks in (omnidroid/engine.py:_start_timings_stages).
# `apk_install` only appears on a `--apk` boot, which this tool never uses.
STAGE_ORDER = ["boot", "apk_install", "session_delivered", "game_foreground"]

DEFAULT_TIMEOUT_S = 900   # generous: a cold boot + bake can be slow on a busy host
STOP_TIMEOUT_S = 120

# "Materially faster" = both a relative margin (run 2 takes at most this
# fraction of run 1's wall time) AND an absolute margin (run 1 minus run 2,
# in seconds). Both must hold: the relative margin alone would call a
# 25%-faster 1.0s->0.75s pair a win (pure scheduling noise), and the
# absolute margin alone would call a 2s-faster 300s->298s pair a win (noise
# at that scale too).
MIN_SPEEDUP_RATIO = 0.75
MIN_SPEEDUP_MARGIN_S = 2.0


def _run_omnidroid(*args, timeout):
    return subprocess.run([sys.executable, "-m", "omnidroid", *args],
                          capture_output=True, text=True, timeout=timeout)


def _fail_loud(name, label, reason, stdout, stderr):
    """A bad measurement is worse than none: print the tail of both streams
    -- never a bare traceback -- stop the instance so nothing is left
    running, then exit nonzero."""
    print(f"\n=== {label}: {reason} ===")
    print("--- stdout (tail) ---")
    print((stdout or "").strip()[-2000:] or "(empty)")
    print("--- stderr (tail) ---")
    print((stderr or "").strip()[-2000:] or "(empty)")
    stop(name)
    sys.exit(f"{label}: {reason}")


def start(name, label, timeout=DEFAULT_TIMEOUT_S):
    """Run `omnidroid start <name> --json --no-window`, timed end to end.
    Returns (wall_seconds, payload_dict). Exits loudly (see `_fail_loud`) if
    the process times out or its stdout is not a single parseable JSON line
    -- `--json` mode redirects every informational print() to stderr, so
    stdout should contain exactly one JSON object followed by a newline."""
    t0 = time.time()
    try:
        r = _run_omnidroid("start", name, "--json", "--no-window",
                           timeout=timeout)
    except subprocess.TimeoutExpired as e:
        dt = time.time() - t0
        _fail_loud(name, label, f"timed out after {dt:.0f}s (limit "
                  f"{timeout}s)", e.stdout or "", e.stderr or "")
        return  # unreachable; _fail_loud always exits
    dt = time.time() - t0
    stdout = r.stdout or ""
    try:
        payload = json.loads(stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        _fail_loud(name, label, f"could not parse `start --json` output "
                  f"(exit code {r.returncode})", stdout, r.stderr or "")
        return  # unreachable
    return dt, payload


def stop(name, timeout=STOP_TIMEOUT_S):
    """Best-effort stop -- leaves no VM behind between or after runs. A
    failure here must not hide the real measurement, so it only warns."""
    try:
        r = _run_omnidroid("stop", name, "--json", timeout=timeout)
        if r.returncode != 0:
            tail = (r.stderr or r.stdout or "").strip()[-300:]
            print(f"(warning: `stop {name}` exited {r.returncode}: {tail})")
    except subprocess.TimeoutExpired:
        print(f"(warning: `stop {name}` timed out after {timeout}s)")


def stage_deltas(timings):
    """[(stage, delta_seconds)] in declared order, skipping any stage this
    run didn't mark (e.g. a run that failed before `game_foreground`)."""
    stages = timings.get("stages", {})
    return [(s, stages[s]) for s in STAGE_ORDER if s in stages]


def boot_to_game_s(timings):
    """The 'roblox half': game_foreground mark minus boot mark. None if
    either mark is missing."""
    marks = timings.get("marks", {})
    if "boot" not in marks or "game_foreground" not in marks:
        return None
    return round(marks["game_foreground"] - marks["boot"], 3)


def print_run(label, wall_dt, payload):
    timings = payload.get("timings", {})
    print(f"\n{label}: {wall_dt:.1f}s wall   ok={payload.get('ok')}"
          + ("" if payload.get("ok") else f"   error={payload.get('error')}"))
    for stage, delta in stage_deltas(timings):
        print(f"    {stage:<18} {delta:6.2f}s")
    print(f"    {'total_s (in-process)':<18} "
          f"{timings.get('total_s', 0):6.2f}s")


def materially_faster(cold_dt, warm_dt):
    if warm_dt >= cold_dt:
        return False
    return (warm_dt <= cold_dt * MIN_SPEEDUP_RATIO
            and (cold_dt - warm_dt) >= MIN_SPEEDUP_MARGIN_S)


def main():
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("account", help="an existing, logged-in account name")
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_S,
                    help=f"seconds to wait for each `start` before giving "
                         f"up (default {DEFAULT_TIMEOUT_S:.0f})")
    args = ap.parse_args()
    name = args.account

    print(f"warm-restore-check: {name}")
    print("stopping any running instance first...")
    stop(name)

    cold_dt, cold = start(name, "run 1 (cold boot + bake)",
                          timeout=args.timeout)
    print_run("run 1 (cold boot + bake)", cold_dt, cold)
    stop(name)

    warm_dt, warm = start(name, "run 2 (expected warm restore)",
                          timeout=args.timeout)
    print_run("run 2 (expected warm restore)", warm_dt, warm)
    stop(name)

    cold_timings, warm_timings = cold.get("timings", {}), warm.get("timings", {})
    cold_boot = cold_timings.get("stages", {}).get("boot")
    warm_boot = warm_timings.get("stages", {}).get("boot")
    cold_b2g = boot_to_game_s(cold_timings)
    warm_b2g = boot_to_game_s(warm_timings)

    print("\n" + "=" * 64)
    print("ANDROID vs ROBLOX SPLIT  (android = time to `boot`; "
          "roblox = boot -> game_foreground)")
    print(f"{'':<26}{'run 1 (cold)':>16}{'run 2 (warm)':>16}")
    print(f"{'android (boot)':<26}"
          f"{('%.2fs' % cold_boot if cold_boot is not None else 'n/a'):>16}"
          f"{('%.2fs' % warm_boot if warm_boot is not None else 'n/a'):>16}")
    print(f"{'roblox (boot->game)':<26}"
          f"{('%.2fs' % cold_b2g if cold_b2g is not None else 'n/a'):>16}"
          f"{('%.2fs' % warm_b2g if warm_b2g is not None else 'n/a'):>16}")

    both_ok = bool(cold.get("ok")) and bool(warm.get("ok"))
    faster = materially_faster(cold_dt, warm_dt)
    passed = both_ok and faster

    print("\n" + "=" * 64)
    print(f"run 1 (cold):  {cold_dt:6.1f}s wall   ok={cold.get('ok')}")
    print(f"run 2 (warm):  {warm_dt:6.1f}s wall   ok={warm.get('ok')}")
    if cold_dt:
        print(f"speedup: {(1 - warm_dt / cold_dt) * 100:5.1f}%  "
              f"({cold_dt - warm_dt:+.1f}s)")
    # Informational only -- NOT part of the PASS/FAIL determination, which
    # is exactly "both ok:true and run 2 materially faster" per the task
    # contract. This just answers the product question the module docstring
    # opens with: is the ~15s "start -> in the place" target in reach yet.
    print(f"15s end-to-end target (run 2 wall time): {warm_dt:.1f}s -> "
          f"{'UNDER TARGET' if warm_dt < 15 else 'over target'}")

    print(f"\nRESULT: {'PASS' if passed else 'FAIL'}")
    if not both_ok:
        print("  reason: at least one run did not report ok:true "
              f"(run 1 ok={cold.get('ok')}, run 2 ok={warm.get('ok')})")
    elif not faster:
        print(f"  reason: run 2 not materially faster than run 1 (need "
              f"wall time <= {MIN_SPEEDUP_RATIO:.0%} of run 1's AND >= "
              f"{MIN_SPEEDUP_MARGIN_S:.0f}s faster in absolute terms)")
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
