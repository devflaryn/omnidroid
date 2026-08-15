"""Per-stage wall-clock timings for one `omnidroid start`.

Phase 0 of the warm-restore work: before optimizing the boot we have to know
which half is slow. Android boot was measured at ~20 s; Roblox's own cold start
and join were never measured at all, so this is permanent instrumentation
rather than a throwaway script.

The clock is injected so the whole shape is unit-testable without a real boot.
"""
import time


class Timings:
    """Records the instant each named stage COMPLETED, relative to creation."""

    def __init__(self, clock=time.monotonic):
        self._clock = clock
        self._start = clock()
        self._marks = []          # [(stage, seconds_since_start)]

    def mark(self, stage):
        """Record that `stage` just finished. Returns self so calls chain."""
        self._marks.append((stage, round(self._clock() - self._start, 3)))
        return self

    def as_dict(self):
        """{'total_s', 'stages' (per-stage deltas), 'marks' (absolute)}."""
        stages, prev = {}, 0.0
        for name, at in self._marks:
            stages[name] = round(at - prev, 3)
            prev = at
        return {"total_s": self._marks[-1][1] if self._marks else 0.0,
                "stages": stages,
                "marks": dict(self._marks)}
