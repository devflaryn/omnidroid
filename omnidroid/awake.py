"""Never sleep, never blank — the guest-side display/power guarantee.

An omnidroid instance has no human at the panel. Android does not know that:
with no input it runs its ordinary ladder — dim, screen off, then doze — and
the moment the display goes off Roblox stops rendering, a farming instance
stops earning, and the VNC viewer shows the black screen this module exists to
prevent.

Like farming.py and gaming.py, this module only BUILDS the command sequence
(pure, unit-testable); the engine applies it over adb. It runs on EVERY boot
and in EVERY mode, before the mode tuning, because a black screen is wrong for
a farm worker and wrong for a played instance alike.

THE LADDER HAS SIX RUNGS, and this is the whole reason the sequence is longer
than the one setting everybody remembers:

  Settings.System  screen_off_timeout        the classic inactivity timer
  Settings.Global  stay_on_while_plugged_in  overrides that timer — but only
                                             while the battery service says
                                             "plugged in" (see below)
  Settings.Secure  sleep_timeout             the separate "user is away" timer
  Settings.Secure  attentive_timeout         Android 11+ attentive display
  Settings.Secure  adaptive_sleep            screen-attention (camera) sleep
  the dream manager                          daydreams/blanks on its own

Disable one and the other five still blank the screen.

THE TRAP: stay_on_while_plugged_in is a MASK OF PLUG TYPES, not a boolean.
PowerManagerService honours it only while BatteryService reports the device
plugged in. A QEMU guest with no battery HAL reports plugged=0, so writing the
setting on its own is a silent no-op — it is set, it reads back correctly, and
it does nothing. `dumpsys battery set ac 1` is what makes it real, so it comes
FIRST and the ordering is load-bearing.

WHAT THIS DELIBERATELY DOES NOT TOUCH: `deviceidle`. Doze is a mode decision —
farming force-idles on purpose, gaming disables it on purpose — and doze is a
consequence of the screen going off, not a cause of it. Forcing it either way
from here would silently overwrite whichever mode ran last. (Note that
`force-idle` is unaffected by the battery override: it sets mForceIdle, which
is exactly the flag that skips the charging/screen-on check.)

Nothing here is measured as a frame-rate or footprint number, and it is not
that kind of change: each step disables one specific, documented path to a
blank display. The honest check is the read-back — see parse_power_state.
"""

import os
import shlex

# The Roblox-side/farming-side quoting rule is a property of the adb
# transport, not of either mode, so both compound-script helpers are reused
# rather than re-derived. See farming.sh for the failure they prevent.
from omnidroid.farming import sh
from omnidroid.gaming import su_sh

# Settings.System.SCREEN_OFF_TIMEOUT is an int in milliseconds. Integer.MAX
# is Android's own idiom for "never" here (~24.8 days), and the framework
# clamps rather than overflowing. A merely long value is not the promise the
# product makes: a farm instance left up overnight would still blank.
SCREEN_OFF_TIMEOUT_MS = 2147483647

# BatteryManager.BATTERY_PLUGGED_AC(1) | _USB(2) | _WIRELESS(4) | _DOCK(8).
# Picking a single plug type loses the guarantee the moment a base's battery
# HAL reports a different one.
#
# 15 rather than the older 7 because that is what the ROM itself writes, and a
# disagreement here is not cosmetic: `svc power stayon true` runs AFTER the
# settings write in the same script, so a 7 would simply be overwritten and
# every log line would report a mask this module never chose. MEASURED on the
# live instance (2026-08-09) — after the sequence, mStayOnWhilePluggedInSetting
# read 15, not the 7 that had just been written.
STAY_ON_ANY_PLUG = 1 | 2 | 4 | 8

# BatteryManager.BATTERY_STATUS_CHARGING.
BATTERY_CHARGING = 2

# KeyEvent.KEYCODE_WAKEUP. NOT KEYCODE_POWER(26): power TOGGLES, so on an
# already-awake instance it produces exactly the black screen this module is
# here to prevent. WAKEUP is idempotent — it wakes, it never sleeps.
KEYCODE_WAKEUP = 224

# Tag for the kernel-level userspace wakelock. Tagged rather than anonymous so
# `cat /sys/power/wake_lock` names us, and so it can be released by name.
WAKE_LOCK_TAG = "omni_awake"

NO_AWAKE_ENV = "OMNI_NO_AWAKE"


def awake_enabled(env=None):
    """False iff the host asked to leave Android's power management alone.

    The kill switch, same shape as bases.NO_WARM_ENV: this changes behaviour on
    every boot of every mode, so there has to be a way to turn it off without
    editing code (e.g. to reproduce a suspend-related bug). On by default."""
    env = os.environ if env is None else env
    return str(env.get(NO_AWAKE_ENV, "")).strip().lower() not in (
        "1", "true", "yes", "on")


def root_only_steps():
    """Human-readable names of the steps that need root, for honest reporting.

    /sys/power/wake_lock is 0220 root:system — a uid-shell write is denied, and
    every script here ends in `; true`, so an unreported skip would look
    exactly like success. Same rule as gaming.root_only_steps."""
    return ("kernel wakelock (block autosleep even if userspace gives up)",)


def _battery_override():
    """One step that makes the guest look plugged in, charging and full.

    Three `dumpsys battery set` calls in one shell script rather than three adb
    round trips: each round trip is ~100 ms and this runs on every boot. Level
    100 is not cosmetic — a low reported level lets battery saver dim the
    display and throttle the game.
    """
    return sh(f"dumpsys battery set ac 1 >/dev/null 2>&1; "
              f"dumpsys battery set status {BATTERY_CHARGING} "
              f">/dev/null 2>&1; "
              f"dumpsys battery set level 100 >/dev/null 2>&1; true")


def _wake_now():
    """Wake the display right now. Settings only govern what happens NEXT: a
    warm-restored instance, or one that blanked before this ran, arrives with
    the screen already off and no amount of timeout configuration turns it back
    on."""
    return ["shell", "input", "keyevent", str(KEYCODE_WAKEUP)]


# Every rung of the ladder that lives in a settings provider, as
# (namespace, key, value). ORDER IS PRESERVED into the script:
# stay_on_while_plugged_in is written first so it lands even if a later write
# on a locked-down base is refused.
SETTINGS = (
    # Rung 0. Turn Developer options on, so the lever below is the REAL
    # developer setting and is visible as such in Settings -> System ->
    # Developer options -> "Stay awake". It changes no behaviour by itself
    # (stay_on_while_plugged_in is honoured either way) — it is here so the
    # guarantee can be inspected and toggled from inside the guest's own UI
    # rather than existing only as an invisible provider row.
    ("global", "development_settings_enabled", 1),
    # Rung 2. The developer-options "Stay awake" lever itself. Only meaningful
    # once the battery override above says "plugged in".
    ("global", "stay_on_while_plugged_in", STAY_ON_ANY_PLUG),
    # Rung 1. The classic inactivity timer — the fallback for the case where
    # something resets the battery override out from under us.
    ("system", "screen_off_timeout", SCREEN_OFF_TIMEOUT_MS),
    # Rungs 3-5. Three separate "user is away" timers, distinct from
    # screen_off_timeout and from each other; -1 / 0 is the framework's own
    # "never" for each. Any one left alone blanks the screen by itself.
    ("secure", "sleep_timeout", -1),
    ("secure", "attentive_timeout", -1),
    ("secure", "adaptive_sleep", 0),
    # Rung 6. The dream manager blanks to a daydream on its own schedule,
    # entirely outside every timer above.
    ("secure", "screensaver_enabled", 0),
    ("secure", "screensaver_activate_on_sleep", 0),
    ("secure", "screensaver_activate_on_dock", 0),
)


def _settings_script():
    """All eight settings writes plus `svc power stayon` as ONE guest script.

    One adb round trip instead of nine. That is not micro-optimisation here:
    this runs on every boot of every instance, a round trip over the adb
    transport is ~100 ms, and the warm-restore cache exists to save seconds —
    spending one of them back on nine `settings put` calls would be absurd.

    `svc power stayon true` is redundant with the stay_on_while_plugged_in
    write BY DESIGN: `svc` goes through PowerManagerService while `settings
    put` writes the provider, and on a locked-down base either one can be the
    one that is refused. Both are idempotent, so writing both costs nothing.
    """
    parts = [f"settings put {ns} {key} {value}"
             for ns, key, value in SETTINGS]
    parts.append("svc power stayon true")
    return sh("; ".join(f"{p} >/dev/null 2>&1" for p in parts) + "; true")


def build_awake_sequence(su=None):
    """Ordered list of adb `shell` argv vectors: never sleep, never blank.

    `su` is the guest's su binary (engine.resolve_su) or None on an unrooted
    instance, in which case the root-only step is OMITTED rather than
    emitted-and-ignored — see root_only_steps().

    Order is load-bearing twice: the battery override precedes the settings
    that depend on it, and the wake-up keyevent goes last so it wakes a display
    that is already governed by the new timeouts.
    """
    steps = [
        # 1) Make the plug state real, so the stay-on setting is not a silent
        #    no-op.
        _battery_override(),
        # 2) Every settings-provider rung of the ladder, in one round trip.
        _settings_script(),
    ]

    # 3) Kernel autosleep. The rungs above keep the DISPLAY on; this keeps the
    #    kernel from suspending the whole guest if userspace ever drops its
    #    last wakelock (a suspended guest is a frozen instance, not merely a
    #    dark one). Root-only; the write is idempotent — re-locking an
    #    already-held tag is a no-op, not an error.
    if su:
        steps.append(su_sh(su, f"echo {WAKE_LOCK_TAG} > /sys/power/wake_lock "
                               f"2>/dev/null; true"))

    # 4) Wake whatever is currently dark.
    steps.append(_wake_now())

    return steps


def build_awake_recheck(su=None):
    """The cheap subset the watchdog re-asserts while the instance runs.

    Not paranoia: these are the levers with a known expiry. `dumpsys battery
    set` is an override the battery service drops on a framework restart (and
    that a devkit session can `reset`), and stay_on_while_plugged_in follows it
    straight back to doing nothing when it goes. The Settings.Secure/System
    writes in the full sequence persist in /data and do not need re-asserting,
    which is exactly why this is a subset rather than a re-run.

    `su` is accepted for symmetry and to keep the caller from branching; the
    kernel wakelock is already held for the life of the boot, so re-taking it
    every poll would be waste.
    """
    return [
        sh(f"dumpsys battery set ac 1 >/dev/null 2>&1; "
           f"dumpsys battery set status {BATTERY_CHARGING} >/dev/null 2>&1; "
           f"dumpsys battery set level 100 >/dev/null 2>&1; "
           f"settings put global stay_on_while_plugged_in "
           f"{STAY_ON_ANY_PLUG} >/dev/null 2>&1; true"),
        _wake_now(),
    ]


# ---------- reading the guest back ----------

def parse_power_state(dumpsys_power_output):
    """Pull the facts that matter out of `dumpsys power`.

    Returns {"wakefulness": str|None, "display": str|None, "powered": bool,
             "screen_off_timeout_ms": int|None, "stay_on_setting": int|None}.

    `screen_off_timeout_ms` is the important one and it is deliberately read
    from PowerManagerService's own "Screen off timeout: N ms" line rather than
    from `settings get`, because THE SETTING IS NOT THE ANSWER. Measured on a
    live instance (2026-08-09, arm base, LineageOS):

        $ settings get system screen_off_timeout
        -1
        $ dumpsys power | grep 'Screen off timeout'
        Screen off timeout: 10000 ms

    -1 is not "never" here — PowerManagerService clamps the setting up to
    mMinimumScreenOffTimeoutConfig (10000), so the base ships with a TEN
    SECOND blank. Reading the setting back would have reported the instance
    fine while it went black ten seconds after the last input, which is the
    bug this module was written for.

    Unreadable output (adb hiccup, offline device, empty string) yields None
    rather than a default — reporting a satisfied guarantee from missing
    information is the silent-no-op failure this repo keeps re-learning.
    """
    out = dumpsys_power_output or ""
    state = {"wakefulness": None, "display": None, "powered": False,
             "screen_off_timeout_ms": None, "stay_on_setting": None}
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("mWakefulness=") and state["wakefulness"] is None:
            state["wakefulness"] = line.split("=", 1)[1].strip()
        elif line.startswith("Display Power:") and state["display"] is None:
            # "Display Power: state=ON" on builds that print it. Others print
            # the callback object here, which is why display is a nice-to-have
            # and the timeout above is the load-bearing reading.
            if "state=" in line:
                state["display"] = line.split("state=", 1)[1].strip()
        elif line.startswith("mIsPowered="):
            state["powered"] = line.split("=", 1)[1].strip() == "true"
        elif line.startswith("Screen off timeout:"):
            state["screen_off_timeout_ms"] = _int_or_none(
                line.split(":", 1)[1].replace("ms", ""))
        elif line.startswith("mStayOnWhilePluggedInSetting="):
            state["stay_on_setting"] = _int_or_none(line.split("=", 1)[1])
    return state


def _int_or_none(text):
    try:
        return int(str(text).strip())
    except (TypeError, ValueError):
        return None


def is_awake(dumpsys_power_output):
    """True only for a genuinely awake guest.

    Dozing/Dreaming/Asleep all read as not-awake even when a stale display
    state still says ON — the point of the check is to catch the instance
    sliding down the ladder, so only the top rung counts.
    """
    return parse_power_state(dumpsys_power_output)["wakefulness"] == "Awake"


def never_blanks(dumpsys_power_output):
    """True iff the guest CANNOT blank from inactivity, per its own dump.

    Distinct from is_awake, and the distinction is the whole point:
    `mWakefulness=Awake` was true on the live instance that was ten seconds
    away from going black. Awake is a snapshot; this is the guarantee.

    Two independent ways to satisfy it, matching the two rungs the sequence
    writes — the inactivity timer is effectively infinite, OR the device is
    plugged in on a plug type stay-on covers. Either alone is enough; the
    sequence sets both so that losing one is not losing the instance.
    """
    st = parse_power_state(dumpsys_power_output)
    timeout = st["screen_off_timeout_ms"]
    if timeout is not None and timeout >= SCREEN_OFF_TIMEOUT_MS:
        return True
    stay_on = st["stay_on_setting"]
    return bool(st["powered"] and stay_on)


def build_state_probe():
    """The adb step whose output parse_power_state reads."""
    return ["shell", "dumpsys", "power"]
