"""The memory governor: keep the guest's RAM ceiling near what it actually uses.

An uncapped instance costs the host its whole `-m` within about twenty seconds
of spawn, whatever the game is doing. MEASURED 2026-08-16, `-m 3072`, host RSS
sampled every second:

    t+10s     38 MB      <- QEMU is up, the guest has touched almost nothing
    t+20s   1305 MB
    t+29s   3307 MB      <- pinned at `-m`, and it never comes down again
    t+69s   3350 MB

The client was still on its loading screen throughout: that is Android's page
cache filling whatever RAM it is offered, not the game needing it. So an idle
instance and a busy one cost the same, which is the thing this module fixes.

TWO HALVES, AND ON WINDOWS THE SECOND ONE IS THE WHOLE TRICK:

  * `next_cap()` — the policy. Grows the instant the guest's free slack runs
    out (the pressure valve, never delayed), shrinks only in steps, only once
    usage has plateaued, never below the mode's floor. The asymmetry is the
    safety property: being too generous costs host RAM, being too tight costs
    the game, and this project has measured what too tight looks like (Roblox
    "has died: fg TOP" plus a mem-pressure-event).

  * `runtime.trim_working_set()` — what makes the shrink real. Inflating the
    balloon does NOT return memory to a Windows host on its own: QEMU's
    `ram_block_discard_range()` is behind `CONFIG_MADVISE`, so the pages the
    guest hands back stay in its working set forever. Measured on a live
    instance, an inflate of 3072 -> 2048 MB moved host RSS 3414 -> 3403 MB.
    `EmptyWorkingSet` asks the OS the same question from outside the process
    and needs nothing from QEMU: same instance, 1873 MB -> 18 MB, settling at
    131 MB once the guest had re-faulted its live set, adb answering in 0.2 s
    throughout.

ORDER MATTERS: inflate first, trim second. The inflate is what makes the spare
pages cold, so the guest never faults them back; trimming alone would evict
pages the guest still wants and buy nothing but page faults.

THERE IS NO BOOT CAP, and that was tried. Capping at spawn so the guest never
touches the pages sounds better than reclaiming afterwards, and it measured
worse: ~60 MB saved of 3.4 GB, boot 0.3 -> 1.4 min, 31 MB of qemu.log. The host
pays for the union of pages ever TOUCHED, and the balloon descends at only
~25 MB/s, so the descent overlaps Android's boot and the guest is handed
different physical pages each round. See `governor_wanted()`. With the trim
available there is nothing left to prevent: the guest boots fast at its full
`-m` and gives the memory back once it has settled.
"""

# How much room above current usage the guest is always given. Generous on
# purpose: the governor polls on a timer, and anything the guest allocates
# between two polls has to fit in here or lmkd starts killing. 512 MB is ~20 s
# of the fastest allocation this project has measured a Roblox client do.
DEFAULT_HEADROOM_MB = 512

# Grow when the guest's free slack falls BELOW this, not whenever `used` ticks
# up. Without this trigger the governor chases every megabyte: MEASURED
# 2026-08-16, a first cut with no grow-side dead band moved the cap
# 1536 -> 1543 -> 1585 in consecutive polls, and then oscillated against the
# shrink rule for the life of the instance.
#
# On Windows that oscillation is not merely untidy, it is EXPENSIVE, and in
# the one direction that cannot be undone: every grow lets the guest touch
# pages it had given up, every touch is permanent (no madvise), so a governor
# that breathes ratchets the host's cost upward one cycle at a time. It also
# costs a log line per 4 KB page moved -- 266 KB of qemu.log in 30 s at the
# rate first measured.
#
# Together with `headroom` this makes the steady state a BAND, not a point:
# nothing happens while slack sits between GROW_TRIGGER and
# headroom + SHRINK_SLACK.
GROW_TRIGGER_MB = 256

# Shrinking is only considered when the cap is at least this far above what the
# guest wants. Without a dead band the governor would chase every 30 MB of
# ordinary churn, and each chase is a balloon inflate the guest has to service.
#
# Kept SMALL on purpose. The dead band is permanent overshoot: the governor
# comes to rest anywhere inside it, so it is added to `headroom` in the final
# footprint, not amortised against it. At 384 MB a guest using 2 GB rested near
# 2.9 GB, which gives back a third of what the caller asked for. 128 MB leaves
# the confirmation count and the step size to do the anti-churn work, which is
# what they are for.
SHRINK_SLACK_MB = 128

# Consecutive polls of slack before a shrink. One quiet poll is not idleness,
# it is the gap between two allocations.
SHRINK_CONFIRMATIONS = 3

# A shrink moves this far, not all the way to the target. A 1.3 GB inflate in
# one command takes the guest ~50 s to service at the ~25 MB/s measured here,
# and for all of it the guest is walking free lists instead of running the
# game. Stepping keeps each move cheap and lets an allocation interrupt the
# descent at the next poll.
SHRINK_STEP_MB = 256


def governor_wanted(mode, host_can_reclaim=True):
    """Should a governor run for this mode? Returns its floor, or None.

    None rather than an exception for every "this does not apply" case: the
    governor is an optimisation, and an optimisation may never be the reason a
    launch fails.

    THERE IS DELIBERATELY NO BOOT CAP. An earlier cut capped the guest at spawn
    on the theory that a host which cannot take pages BACK might at least avoid
    handing them out. MEASURED 2026-08-16 on Windows, PS99, `-m 3072`, cap 1536,
    against an uncapped control:

        uncapped        host RSS 3403-3414 MB at rest
        boot cap 1536   host RSS 3335-3345 MB at rest, guest using 862 MB
                        boot 0.3 min -> 1.4 min, 31 MB of qemu.log

    ~60 MB for a 4x slower boot. The host pays for the UNION OF PAGES EVER
    TOUCHED, and the balloon descends at only ~25 MB/s, so the descent runs
    concurrently with Android's boot and the guest is handed different physical
    pages each round -- touching nearly all of `-m` regardless. Capping at boot
    INCREASES page-set churn rather than preventing the fill.

    None of which matters any more, because the governor no longer needs to
    prevent anything: `runtime.trim_working_set()` gets the pages back after
    the fact, which is what the boot cap was a (bad) substitute for. So the
    guest boots fast at its full `-m` and gives memory back once it has
    settled.

    An explicit `--balloon` wins outright. resolve_mode() already marks that
    case, and it means the operator has named the number they want — the
    governor talking over it is exactly the surprise that flag exists to
    prevent (see resolve_mode's own note about the same trap).
    """
    mode = mode or {}
    if not host_can_reclaim:
        return None
    # DENSITY ONLY, and this is a latency decision rather than a memory one.
    # The trim's cost is that an evicted page comes back from the pagefile, so
    # a trimmed instance can hitch for as long as that read takes. Farming does
    # not care -- nobody is looking at those frames, and instance COUNT is the
    # entire point. `performance` is the opposite trade by definition ("frames,
    # resolution, input latency" -- MODES.md), and the frame-time cost of
    # trimming it has never been measured. Until it has, gaming keeps the
    # behaviour it has always had.
    if mode.get("profile") != "density":
        return None
    if mode.get("balloon_explicit"):
        return None
    floor = mode.get("balloon_floor")
    mem = mode.get("mem")
    if not floor or not mem:
        return None
    # `--mem 512` against a mode whose floor is 1024 is not a misconfiguration
    # to reject; it is a guest already smaller than the floor, which the
    # governor has nothing to do for.
    if floor >= mem:
        return None
    return int(floor)


def next_cap(*, used_mb, cap_mb, mem_mb, floor_mb,
             headroom_mb=DEFAULT_HEADROOM_MB, slack_polls=0, may_shrink=True):
    """Decide this poll's balloon cap.

    Returns `(cap_mb, slack_polls, reason)`. `slack_polls` is the governor's
    only state and is threaded back in on the next call, so the policy stays a
    pure function and the loop that drives it stays trivial.

    `used_mb` is None when the guest could not be read (an adb hiccup). That
    holds everything where it is: never advance on missing information, which
    is the rule cmd_watch already applies to the game's pid.
    """
    if used_mb is None:
        return cap_mb, slack_polls, "unknown usage — holding"

    # The floor can legitimately exceed `-m` (a small --mem against a mode
    # whose floor was written for a bigger guest). `-m` wins: it is a hard
    # limit, the floor is only a preference.
    low = min(floor_mb, mem_mb)
    want = max(low, min(mem_mb, used_mb + headroom_mb))
    slack = cap_mb - used_mb

    if slack < GROW_TRIGGER_MB and want > cap_mb:
        # THE PRESSURE VALVE. No confirmations, no stepping, no waiting for a
        # settled client — the guest is short of room now, and every second
        # spent confirming that is a second lmkd might spend killing the game.
        #
        # Gated on SLACK rather than on `want > cap` alone: the latter is true
        # of any guest sitting less than `headroom` below its cap, which is
        # the normal, healthy state, and acting on it is the oscillation
        # GROW_TRIGGER_MB documents.
        return want, 0, f"grow to {want} MB (used {used_mb} MB)"

    if not may_shrink:
        # A client that is still loading. Every lever that makes an idle
        # instance cheap starves a loading one: MEASURED 2026-08-16 on PS99,
        # the squeeze applied before the session was delivered left the client
        # alive with PSS flat at ~400 MB and the guest 200% idle for six
        # minutes. Growth above still applies; only the giving-back waits.
        return cap_mb, 0, "hold — client not settled"

    if slack <= headroom_mb + SHRINK_SLACK_MB:
        # Inside the band. This is the steady state and it must be a no-op:
        # the whole point of a band is that the ordinary drift of a running
        # game moves nothing at all.
        return cap_mb, 0, "hold — in band"

    slack_polls += 1
    if slack_polls < SHRINK_CONFIRMATIONS:
        return cap_mb, slack_polls, (f"hold — slack {slack_polls}/"
                                     f"{SHRINK_CONFIRMATIONS}")
    new_cap = max(want, cap_mb - SHRINK_STEP_MB)
    return new_cap, 0, f"shrink to {new_cap} MB (used {used_mb} MB)"


# How flat the guest's usage has to be, and for how many polls, before the
# governor is allowed to start giving memory back. This is the "wait for the
# game" gate, and it exists because the growth rules alone do not close one
# hole: early in a boot the guest is genuinely using very little, so the
# governor would happily shrink to fit -- moments before the client allocates
# a gigabyte and needs it all back. Waiting for a plateau costs nothing (the
# instance is at its boot cap meanwhile, which is already the saving) and
# removes the churn entirely.
PLATEAU_TOLERANCE_MB = 64
PLATEAU_POLLS = 4


def plateaued(history):
    """Has the guest's usage stopped CLIMBING?

    The same shape of question `wait_for_game_settled` asks of the client's
    PSS, asked here of the whole guest and without adb-side knowledge of which
    process matters.

    GROWTH, not flatness, and the difference decides whether this ever fires.
    The first cut asked for `max - min <= tolerance` over the window, i.e. for
    the guest to hold still. A loading client does not hold still and neither
    does a loaded one: MEASURED 2026-08-16, PS99 in-world moved between 2137
    and 2217 MB over four polls purely from asset streaming, so a 64 MB
    flatness test was never satisfied and the governor sat at `-m` forever
    doing nothing at all.

    What the gate is actually for is "the load phase is over", so it only has
    to rule out sustained GROWTH. Ordinary churn and any downward move are
    fine, and a guest that is genuinely still loading climbs monotonically
    past the tolerance within one window.
    """
    if len(history) < PLATEAU_POLLS:
        return False
    window = history[-PLATEAU_POLLS:]
    if any(h is None for h in window):
        # An adb hiccup. Missing information never advances the decision to
        # start giving memory away.
        return False
    return (window[-1] - window[0]) <= PLATEAU_TOLERANCE_MB


def used_mb_from_meminfo(text):
    """Guest RAM in use, from /proc/meminfo. None if it cannot be read.

    MemTotal - MemAvailable, not MemTotal - MemFree: MemFree counts the page
    cache as used, and the page cache is precisely the thing the governor is
    trying to stop the guest from hoarding. Sizing against MemFree would read
    every capped guest as "still full" and never shrink at all.

    MemTotal is the BALLOONED total — the guest's own view already excludes
    what the balloon holds — so `used` is directly comparable to the cap.
    """
    if not text:
        return None
    fields = {}
    for line in text.splitlines():
        name, _, rest = line.partition(":")
        parts = rest.split()
        if parts and parts[0].isdigit():
            fields[name.strip()] = int(parts[0])          # kB
    total = fields.get("MemTotal")
    avail = fields.get("MemAvailable")
    if total is None or avail is None:
        return None
    return max(0, (total - avail) // 1024)


# ---------------------------------------------------------------------------
# The WORKING-SET CEILING: how a Windows host actually gets farming's memory
# back. Everything above this line is the balloon, which is the mechanism on
# hosts where QEMU can decommit; this is the mechanism on the host where it
# cannot.
#
# WHY THIS EXISTS AT ALL. QEMU on Windows has no madvise, so nothing the guest
# gives back is ever released -- an idle instance costs the host its whole
# `-m`, which is the entire "30 instances drain the RAM" problem. The balloon
# does not fix it (measured: a full inflate moved host RSS 3414 -> 3403 MB, and
# it descends at ~0.4 MB/s, so it never finishes) and `EmptyWorkingSet` on a
# timer wedges the guest (measured: adb stopped answering, the game was killed,
# and it never recovered). A hard working-set MAXIMUM does fix it -- see
# runtime.cap_working_set for the table.
#
# WHAT THIS POLICY ADDS is that the right ceiling is not a constant. It is a
# property of THE GAME: PS99 dies below ~500 MB, another place will differ, and
# nobody should have to measure each one by hand. So the ceiling walks DOWN
# while the guest is healthy and jumps UP the moment it is not, and remembers
# the lowest level that hurt so it never goes back there.

# How far the ceiling moves per step, in MB. Small enough that one step down
# is never a cliff, large enough to reach the floor from a cold start in a
# couple of minutes.
CEILING_STEP_MB = 128
# Never below this, whatever the search decides. A ceiling under a couple of
# hundred MB is not a guest any more, it is a thrash.
CEILING_HARD_FLOOR_MB = 256
# How much room to leave above the level that hurt. One step is not enough:
# the level that hurt was measured while the guest was ALREADY under pressure,
# so the safe level is meaningfully above it.
CEILING_BACKOFF_STEPS = 2

# What "unhealthy" means, and the order matters: adb latency is the LEADING
# indicator and the game dying is the lagging one. MEASURED on a wedged
# instance: adb round trips went from 0.05 s to 15 s (timeouts) while the game
# was still running, and the game died afterwards. Backing off on latency
# alone is what keeps the game alive.
ADB_SLOW_SECONDS = 2.0


def next_ceiling(*, ceiling_mb, healthy, floor_mb, unsafe_mb=None,
                 step_mb=CEILING_STEP_MB, max_mb=None):
    """The next working-set ceiling, and whether this level is now known bad.

    Returns (ceiling, unsafe) -- `unsafe` is the lowest ceiling that has been
    observed to hurt, and the search never goes at or below it again.

    `max_mb` is the guest's own `-m` and is a HARD STOP, not a preference.
    Without it the backoff runs away: an instance whose client had died for
    reasons of its own read as unhealthy every poll and the ceiling climbed
    3072 -> 4736 MB and kept going, which is both meaningless (there is
    nothing above `-m` to hand back) and hides the real problem behind a
    number that looks like it is still working on it. At the top the search
    STOPS -- if a guest is unhealthy while holding all the memory it was ever
    given, memory is not what is wrong with it.

    Pure, because the interesting part is the decision and not the syscall:
    the caller measures health, this decides the number.
    """
    floor = max(int(floor_mb or 0), CEILING_HARD_FLOOR_MB)
    ceiling = int(ceiling_mb)
    top = int(max_mb) if max_mb else None
    if not healthy:
        # This level hurt. Remember it, and get clear of it -- upward, now.
        unsafe = ceiling if unsafe_mb is None else max(int(unsafe_mb), ceiling)
        backed_off = ceiling + CEILING_BACKOFF_STEPS * step_mb
        if top is not None:
            backed_off = min(backed_off, top)
        return backed_off, unsafe
    lower_bound = floor
    if unsafe_mb is not None:
        # Stay a full backoff above anything that has hurt, rather than
        # creeping back down to it one step at a time and hurting again.
        lower_bound = max(lower_bound,
                          int(unsafe_mb) + CEILING_BACKOFF_STEPS * step_mb)
    if ceiling - step_mb < lower_bound:
        return max(ceiling, lower_bound), unsafe_mb
    return ceiling - step_mb, unsafe_mb


def starting_ceiling(mem_mb, mode=None):
    """Where the search begins: the mode's own number, else the guest's `-m`.

    Starting AT `-m` rather than at a guess is deliberate. The ceiling can only
    hurt the guest by being too low, so the search approaches from the safe
    side and the first minutes of an instance's life -- when the game is still
    loading and needs the most -- are spent uncapped.
    """
    named = (mode or {}).get("ws_ceiling")
    if named:
        return int(named)
    return int(mem_mb)


def ceiling_floor_mb(mode=None):
    """The lowest ceiling this mode's search may reach."""
    return max(int((mode or {}).get("ws_floor") or CEILING_HARD_FLOOR_MB),
               CEILING_HARD_FLOOR_MB)
