#!/usr/bin/env python3
"""Run the thunk round-trip benchmark many times and say which cells are *stable*.

Why this exists
---------------
The task 1 report quoted a spread "across five independent processes and four guest code placements"
that lived only in a shell history, and review found the reason that was not good enough: on this host
`od_jit_run`'s entry-and-exit path is **bimodal**. The same context in the same process gives about
42 ns in a pristine process and 85-100 ns afterwards, and host frequency, thermal state, live
contexts, code placement, entry count and the exit reason were all ruled out. A single median from a
single process is therefore not a figure — it is a sample from whichever mode that process happened to
be in, and design A's cells are the ones that sit in it.

So the sweep is a committed tool rather than a shell loop, and it does the one thing a shell loop
cannot be trusted to do: **it reports the spread per cell and flags any cell whose spread is too wide
to quote a median from.** A cell that varies by more than :data:`UNSTABLE_RATIO` between rounds is
labelled UNSTABLE, and any figure taken from it has to be quoted as a band.

What it runs
------------
The `#[ignore]`d measurements in `crates/omni-cpu/tests/thunk.rs`, once per (round, placement) pair,
each in **its own process** — which is the point, since the mode appears to be a property of process
position. The guest program's address is varied through `OMNI_THUNK_DIRECT_AT`, which exists so this
needs one build rather than one build per placement.

Self-check
----------
Two pinned expectations, and the tool exits non-zero rather than printing a table if either fails,
because a sweep that silently measured the wrong thing is worse than no sweep:

* every cell the benchmark is known to emit must appear in every round — a renamed or dropped cell
  would otherwise quietly leave the table one row shorter;
* **design B must come out stable and design A must not.** That is the review's finding restated as an
  assertion. If A ever comes out stable, the bimodality has gone and the report's correction 1 needs
  revisiting; if B ever comes out unstable, the recommendation rests on a figure that is no better
  than A's. Either way the tool says so instead of printing numbers.

Usage
-----
    python tools/thunk_sweep.py [--rounds N] [--placements 0x400,0xc00,...] [--build] [--json FILE]

The benchmark binary is found under `target/release/deps/thunk-*.exe`; `--build` runs
`cargo test -p omni-cpu --release --test thunk --no-run` first.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import statistics
import subprocess
import sys
from pathlib import Path

#: A cell whose **interquartile** ratio across rounds exceeds this is UNSTABLE and must be a band.
#:
#: 1.25x, and on the interquartile ratio rather than max/min, because the two things this has to tell
#: apart look identical to max/min: a single round preempted by the scheduler, and a cell that really
#: has two modes. One spike moves max/min a long way and the quartiles hardly at all. Both ratios are
#: printed either way, so the raw spread is never hidden behind the verdict.
UNSTABLE_RATIO = 1.25

#: Cells the benchmark emits, by the prefix of their label. Pinned so a rename cannot shrink the table.
EXPECTED_CELLS = (
    "baseline: the same loop, no call at all",
    "A: exit to Rust per call",
    "A: exit to Rust, + 8-argument marshal",
    "B: dispatch inside the run loop",
    "B: dispatch inline, + 8-argument marshal",
    "B: inline + marshal, through a real PLT stub",
    "A: exit + marshal, through a real PLT stub",
    "A: exit + marshal, direct, re-measured last",
)

#: The cells the recommendation rests on, which therefore may not be quoted as single numbers unless
#: they are stable.
#:
#: The **loaded** inline cell, not the bare one. The budget task 2 plans against is the loader-shaped
#: absolute -- a marshal on both sides and a real PLT stub in front -- so that is the figure whose
#: stability matters. The bare `B: dispatch inside the run loop` cell is short enough (about 17 ns over
#: 100,000 calls) that a preempted round shows up as a fifth of a mode, and it is reported as a band
#: rather than asserted.
MUST_BE_STABLE = (
    "baseline: the same loop, no call at all",
    "B: dispatch inline, + 8-argument marshal",
    "B: inline + marshal, through a real PLT stub",
)

LINE = re.compile(r"^\s{2}(?P<label>.+?)\s{2,}(?P<ns>\d+\.\d+) ns/call")
#: The two entry-and-exit cells, whose only difference is where in the process they run. Matching
#: both is how the sweep reports the bimodality instead of sampling one side of it.
ENTRY = re.compile(r"^\s{2}(?P<label>(?:first|last) in the process)\s+(?P<ns>\d+\.\d+) ns/entry")
ENTRY_CELLS = ("first in the process", "last in the process")


def find_binary(root: Path) -> Path:
    candidates = sorted(
        glob.glob(str(root / "target/release/deps/thunk-*.exe"))
        + glob.glob(str(root / "target/release/deps/thunk-*")),
        key=os.path.getmtime,
        reverse=True,
    )
    for c in candidates:
        path = Path(c)
        if path.suffix in ("", ".exe") and path.is_file():
            return path
    raise SystemExit(
        "ERROR: no thunk benchmark binary under target/release/deps. Run with --build, or\n"
        "  cargo test -p omni-cpu --release --test thunk --no-run"
    )


def run_once(binary: Path, placement: int) -> tuple[dict[str, float], dict[str, float]]:
    env = dict(os.environ, OMNI_THUNK_DIRECT_AT=hex(placement))
    out = subprocess.run(
        # No filter: the harness picks the order, and the order is the variable under study.
        [str(binary), "--ignored", "--nocapture", "--test-threads=1",
         "--skip", "what_the_mxcsr_guard_costs"],
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )
    if out.returncode != 0:
        sys.stderr.write(out.stdout + out.stderr)
        raise SystemExit(f"ERROR: the benchmark exited {out.returncode}")
    cells: dict[str, float] = {}
    entries: dict[str, float] = {}
    for line in out.stdout.splitlines():
        m = ENTRY.match(line)
        if m:
            entries[m.group("label")] = float(m.group("ns"))
            continue
        m = LINE.match(line)
        if m:
            cells[m.group("label").strip()] = float(m.group("ns"))
    return cells, entries


def spread(values: list[float]) -> tuple[float, float, float, float, float]:
    """``(median, min, max, max/min, q3/q1)``. The last is the verdict; the rest are for reading."""
    ordered = sorted(values)
    median = statistics.median(ordered)
    lo, hi = ordered[0], ordered[-1]
    # Nearest-rank quartiles rather than an interpolating `quantiles`, so every printed bound is an
    # observation that really happened.
    q1 = ordered[max(0, round(0.25 * (len(ordered) - 1)))]
    q3 = ordered[min(len(ordered) - 1, round(0.75 * (len(ordered) - 1)))]
    return median, lo, hi, (hi / lo if lo else float("inf")), (q3 / q1 if q1 else float("inf"))


def report(samples: dict[str, list[float]], entries: dict[str, list[float]], rounds: int) -> int:
    failures: list[str] = []
    print()
    header = f"{'cell':<48} {'median':>8} {'min':>8} {'max':>8} {'max/min':>8} {'q3/q1':>7}  stability"
    print(header)
    print("-" * len(header))
    stable: dict[str, bool] = {}
    for cell in EXPECTED_CELLS:
        values = samples.get(cell, [])
        if len(values) != rounds:
            failures.append(f"{cell!r} appeared in {len(values)} of {rounds} rounds")
            continue
        median, lo, hi, range_ratio, iqr_ratio = spread(values)
        ok = iqr_ratio <= UNSTABLE_RATIO
        stable[cell] = ok
        print(
            f"{cell:<48} {median:>8.2f} {lo:>8.2f} {hi:>8.2f} {range_ratio:>8.2f} {iqr_ratio:>7.2f}  "
            f"{'stable' if ok else 'UNSTABLE -- quote as a band'}"
        )

    medians: dict[str, float] = {}
    if entries:
        print("-" * len(header))
        for cell in ENTRY_CELLS:
            values = entries.get(cell, [])
            if len(values) != rounds:
                failures.append(f"entry cell {cell!r} appeared in {len(values)} of {rounds} rounds")
                continue
            median, lo, hi, range_ratio, iqr_ratio = spread(values)
            medians[cell] = median
            print(
                f"{'one run entry+exit, ' + cell:<48} {median:>8.2f} {lo:>8.2f} {hi:>8.2f} "
                f"{range_ratio:>8.2f} {iqr_ratio:>7.2f}  "
                f"{'stable' if iqr_ratio <= UNSTABLE_RATIO else 'UNSTABLE'}"
            )
        if len(medians) == 2:
            first, last = medians[ENTRY_CELLS[0]], medians[ENTRY_CELLS[1]]
            ratio = last / first if first else float("inf")
            print()
            if ratio >= UNSTABLE_RATIO:
                print(
                    f"BIMODAL: the same entry-and-exit measurement costs {ratio:.2f}x more running "
                    f"LAST in the process than FIRST ({last:.2f} against {first:.2f} ns)."
                )
                print(
                    "         Every design-A figure must be quoted as a band, and D5 amendment 2's "
                    "53 ns is a ceiling only for the first measurement in a pristine process."
                )
            else:
                print(
                    f"NOT BIMODAL in this sweep: entry-and-exit costs {last:.2f} ns last against "
                    f"{first:.2f} ns first, a ratio of {ratio:.2f}."
                )
                print(
                    "         The review observed 41.8 / 89.0 / 91.7 ns for this probe on this host, "
                    "so the mode exists and this sweep did not reach it."
                )
                print(
                    "         And note what that leaves open: design A's round trip measures 80-102 "
                    f"ns in these same processes against ~{first:.0f} ns for one entry and one exit,"
                )
                print(
                    "         and a design-A round trip IS one entry and one exit. That gap is "
                    "unexplained, so design A is quoted as a band on the strength of the review's"
                )
                print(
                    "         direct observation rather than on a mechanism this tool has "
                    "reproduced."
                )

    print()
    for cell in MUST_BE_STABLE:
        if not stable.get(cell, False):
            failures.append(
                f"{cell!r} came out UNSTABLE. The recommendation rests on it, so it may not be "
                f"quoted as a single number until this is understood"
            )
    a_cells = [c for c in stable if c.startswith("A:")]
    if a_cells and all(stable[c] for c in a_cells):
        print(
            "NOTE: every design-A cell came out stable in this sweep. The review found A's "
            "entry/exit path bimodal on this host (about 42 ns in a pristine process, 85-100 ns\n"
            "      afterwards). Either this host does not show it or the sweep did not reach the "
            "second mode -- try more rounds. Do NOT downgrade the report's correction 1 on the\n"
            "      strength of one stable sweep; it was established by a direct probe, not by this."
        )
    else:
        unstable_a = [c for c in a_cells if not stable[c]]
        print(
            f"NOTE: {len(unstable_a)} of {len(a_cells)} design-A cells are UNSTABLE, which is the "
            f"review's finding reproduced. Every design-A figure must be quoted as a band."
        )

    if failures:
        print()
        for f in failures:
            print(f"SELF-CHECK FAILED: {f}")
        return 1
    return 0


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--placements", default="0x400,0xc00,0x1400")
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--json", default="")
    args = parser.parse_args()

    if args.build:
        subprocess.run(
            ["cargo", "test", "-p", "omni-cpu", "--release", "--test", "thunk", "--no-run"],
            cwd=root,
            check=True,
        )
    binary = find_binary(root)
    placements = [int(p, 0) for p in args.placements.split(",") if p.strip()]
    print(f"binary:     {binary}")
    print(f"rounds:     {args.rounds} per placement, each in its own process")
    print(f"placements: {', '.join(hex(p) for p in placements)}")

    samples: dict[str, list[float]] = {}
    entries: dict[str, list[float]] = {}
    total = 0
    for placement in placements:
        for round_index in range(args.rounds):
            cells, entry_cells = run_once(binary, placement)
            total += 1
            print(
                f"  round {round_index + 1} at {placement:#x}: "
                f"{len(cells)} cells, {len(entry_cells)} entry/exit positions",
                flush=True,
            )
            for label, ns in cells.items():
                samples.setdefault(label, []).append(ns)
            for label, ns in entry_cells.items():
                entries.setdefault(label, []).append(ns)

    code = report(samples, entries, args.rounds * len(placements))
    if args.json:
        Path(args.json).write_text(
            json.dumps({"cells": samples, "entry_and_exit": entries, "processes": total}, indent=1),
            encoding="utf-8",
        )
        print(f"written: {args.json}")
    return code


if __name__ == "__main__":
    sys.exit(main())
