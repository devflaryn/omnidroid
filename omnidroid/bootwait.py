# omnidroid/bootwait.py
"""Wait on PROGRESS, not on the clock.

THE PROBLEM THIS REPLACES
-------------------------
`engine.wait_for_boot` ran against a fixed wall-clock budget:

    FIRST_BOOT_TIMEOUT = 1500   # first boot runs full dexopt; be patient
    NORMAL_BOOT_TIMEOUT = 360

Both numbers were measured on one machine -- an i7-13700F with a working
hypervisor and an NVMe disk -- and then applied to every machine. Anything
slower had its boots killed while they were still going:

  * a weak or throttled CPU (a laptop on battery is a different computer),
  * a mechanical disk, or an SSD behind a busy antivirus scanner,
  * a host already running twenty other instances,
  * and, worst of all, a host with **no hardware virtualization**, where the
    entire guest is emulated by TCG and a boot legitimately takes several times
    as long (see accelprobe.py -- such a host used not to boot at all).

The user saw `boot_timeout` and a failed launch. QEMU, meanwhile, went right on
booting: the engine spawns it detached, so killing the wait orphans a live VM.
"Wait longer" was never a fix either -- it just makes a genuinely dead boot take
twenty-five minutes to be reported instead of six.

The budget was the wrong question. The right one is **"is this guest still
moving?"** A boot that is moving deserves however long it needs. A boot that has
stopped moving is dead inside a couple of minutes no matter what the budget says.

THE SIGNALS
-----------
Independent, cheap, and deliberately redundant -- each one covers a phase where
the others are blind:

  `serial/qemu log size`  firmware and kernel chatter; the only signal there is
                          before adbd exists.
  `adb endpoint state`    "" -> offline -> device. A one-way street, so a
                          transition is unambiguous progress.
  `guest boot properties` once adbd answers: `sys.boot_completed`,
                          `dev.bootcomplete`, `init.svc.bootanim`.
  `dexopt file count`     the number of entries under /data/dalvik-cache. This
                          is FIRST BOOT'S progress meter -- the phase that
                          needed a 25-minute budget writes nothing anywhere
                          else while it grinds, and this counts it directly.
  `host CPU time`         the universal one. A guest executing instructions
                          burns host CPU; a wedged one does not. Works before
                          adbd, on every platform, with no guest cooperation.

A fingerprint is the tuple of readings. If ANY element differs from the previous
sample, the guest moved and the stall clock resets. A reading of `None` means
"could not sample", and two `None`s in a row are deliberately NOT motion -- a
permanently broken signal source must not look like a permanently busy guest.

RULES
-----
Nothing in this module may raise. Every sampler returns `None` on any failure.
The wait is the least appropriate place in the product for an exception: by the
time it runs there is a live VM on the host costing real memory, and an
exception there leaks it.
"""
import ctypes
import os
import subprocess
import sys
import time

IS_WINDOWS = sys.platform.startswith("win")
IS_MACOS = sys.platform == "darwin"

# How long a boot may show NO sign of life before it is called dead.
#
# These are stall windows, not budgets: they bound silence, not duration. A boot
# that keeps moving is never measured against them at all, which is the whole
# point -- so they can be sized for "how long can a healthy guest plausibly go
# quiet" rather than for "how slow is the slowest PC we support", a question
# nobody can answer.
STALL_PRE_ADB_S = 240        # firmware/kernel: the log and the CPU meter both
                             # move continuously here, so silence is loud.
STALL_POST_ADB_S = 300       # Android starting services; adb answers but can
                             # queue behind a busy guest.
STALL_FIRST_BOOT_S = 600     # dexopt. Compiling the whole system can sit on one
                             # package for minutes on a slow disk, and the file
                             # count is the only thing that moves meanwhile.

# Poll cadence. The old loop slept a flat 5 s, so EVERY boot paid up to 5 s of
# pure latency after Android was already up -- 2.5 s expected, on every launch,
# for nothing. Once adbd answers, `boot_completed` is seconds away and it is
# worth watching for.
POLL_EARLY_S = 3.0
POLL_LATE_S = 1.0

# THE BACKSTOP that makes "no deadline" safe.
#
# Dropping the wall-clock budget also dropped a safety net it was providing by
# accident: a guest that never boots but never goes quiet either. A kernel that
# panics and resets in a loop keeps every signal moving -- the log grows, the
# CPU burns, adbd comes and goes -- so a pure stall watch waits for it forever,
# holding gigabytes, while `omnidroid start` blocks and the app's own watchdog
# stays disarmed because the engine is still printing progress lines.
#
# This is a BACKSTOP, not a budget. It is set far beyond any real boot on any
# real hardware -- an emulated first boot with full dexopt on a weak PC is
# measured in tens of minutes, not hours -- so reaching it means something is
# broken rather than slow. Overridable for the pathological host that proves
# this wrong; `looping()` below catches the common shape much sooner.
SANITY_CEILING_S = float(os.environ.get("OMNI_BOOT_SANITY_CEILING_S") or 3 * 3600)

# How many times adbd may come up and go away again before this is a reboot
# loop rather than a boot. One flap is ordinary -- `adb root` alone causes one.
REBOOT_LOOP_LIMIT = 4


def poll_interval(adbd_seen):
    return POLL_LATE_S if adbd_seen else POLL_EARLY_S


def stall_limit(first_boot=False, adbd_seen=False):
    if first_boot and adbd_seen:
        return STALL_FIRST_BOOT_S
    return STALL_POST_ADB_S if adbd_seen else STALL_PRE_ADB_S


# --------------------------------------------------------------- the samplers

def file_size(path):
    """Bytes, or None if it cannot be read. A directory is None, not a size."""
    try:
        st = os.stat(path)
    except (OSError, TypeError, ValueError):
        return None
    if not os.path.isfile(path):
        return None
    return st.st_size


def _cpu_seconds_windows(pid):
    kernel32 = ctypes.windll.kernel32
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    h = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    try:
        creation = ctypes.c_ulonglong()
        exited = ctypes.c_ulonglong()
        kern = ctypes.c_ulonglong()
        user = ctypes.c_ulonglong()
        ok = kernel32.GetProcessTimes(h, ctypes.byref(creation),
                                      ctypes.byref(exited),
                                      ctypes.byref(kern), ctypes.byref(user))
        if not ok:
            return None
        # FILETIME is 100-nanosecond units.
        return (kern.value + user.value) / 1e7
    finally:
        kernel32.CloseHandle(h)


def _cpu_seconds_linux(pid):
    with open(f"/proc/{pid}/stat", "rb") as fh:
        raw = fh.read().decode("utf-8", "replace")
    # comm can contain spaces and parentheses; everything after the LAST ')'
    # is positional, and utime/stime are fields 14 and 15 counting from 1.
    tail = raw[raw.rindex(")") + 2:].split()
    ticks = os.sysconf("SC_CLK_TCK") or 100
    return (int(tail[11]) + int(tail[12])) / float(ticks)


def _cpu_seconds_ps(pid):
    """macOS and anything else with a POSIX `ps`. `[[dd-]hh:]mm:ss[.ff]`."""
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)],
                         capture_output=True, text=True, timeout=10)
    text = (out.stdout or "").strip()
    if not text:
        return None
    days = 0.0
    if "-" in text:
        d, text = text.split("-", 1)
        days = float(d)
    parts = [float(p) for p in text.split(":")]
    while len(parts) < 3:
        parts.insert(0, 0.0)
    return days * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2]


def cpu_seconds(pid):
    """Total host CPU seconds this pid has burnt, or None.

    THE universal progress signal: it needs nothing from the guest, works from
    the instant QEMU is spawned, and separates "slow" from "wedged" better than
    anything the guest could tell us. None means "could not read", which
    includes the pid being gone -- the caller has a much better death check than
    this one (see `engine.running_pid`), so this stays a progress signal only.
    """
    if not pid:
        return None
    try:
        if IS_WINDOWS:
            return _cpu_seconds_windows(int(pid))
        if IS_MACOS:
            return _cpu_seconds_ps(int(pid))
        return _cpu_seconds_linux(int(pid))
    except Exception:      # noqa: BLE001 - a progress probe may never raise
        return None


def sample(sources):
    """Call every source; a source that raises contributes None.

    `sources` is {name: callable}. Returns {name: reading}. Ordering is the
    dict's, which is insertion order, so the fingerprint is stable.
    """
    out = {}
    for name, fn in sources.items():
        try:
            out[name] = fn()
        except Exception:      # noqa: BLE001 - see module docstring
            out[name] = None
    return out


# ------------------------------------------------------------------ the watch

class BootWatch:
    """Stall bookkeeping. Knows nothing about QEMU, adb or Android.

    Feed it a fingerprint every poll with `note()`; ask `stalled()` whether
    nothing has moved for the stall window, and `expired()` whether an
    explicitly-requested absolute cap has been passed.
    """

    def __init__(self, stall_limit, cap=None, clock=time.monotonic):
        self._stall = stall_limit
        self._cap = cap
        self._clock = clock
        self._start = clock()
        self._last_change = self._start
        self._last = _NOTHING
        self.changes = 0
        self._adb_up = False
        self.adb_drops = 0

    def set_stall_limit(self, seconds):
        """The window widens as the boot changes phase -- dexopt is allowed to
        be quiet in a way a kernel boot is not."""
        self._stall = seconds

    def stall_limit(self):
        return self._stall

    def note(self, fingerprint):
        """Record this poll's readings. Returns True if anything moved."""
        moved = fingerprint != self._last and not _all_blank(fingerprint)
        # A fingerprint of nothing but Nones is not motion even the first time:
        # a boot whose every signal is unreadable must reach `stalled()`, not
        # sit forever on the strength of one unreadable sample.
        if moved:
            self._last_change = self._clock()
            self.changes += 1
        self._last = fingerprint
        return moved

    def quiet_for(self):
        return self._clock() - self._last_change

    def elapsed(self):
        return self._clock() - self._start

    def stalled(self):
        return self.quiet_for() >= self._stall

    def expired(self):
        return self._cap is not None and self.elapsed() >= self._cap

    def cap(self):
        return self._cap

    def note_adb_state(self, state):
        """Track adbd coming up and going away again.

        A guest that reaches adbd and then loses it, repeatedly, is resetting --
        the one failure shape that keeps every progress signal moving and would
        therefore never stall. Counted here rather than inferred from the
        fingerprint, because the fingerprint deliberately cannot tell a
        transition apart from any other change.
        """
        up = state == "device"
        if self._adb_up and not up:
            self.adb_drops += 1
        self._adb_up = up

    def looping(self):
        return self.adb_drops >= REBOOT_LOOP_LIMIT

    def past_sanity_ceiling(self):
        return self.elapsed() >= SANITY_CEILING_S


_NOTHING = object()


# ----------------------------------------------------------------- the verdict

# The reasons a boot can end. Every one of them is a DIFFERENT thing for a user
# to do about it, which is the whole argument for naming them.
BOOTED = "booted"                # Android came up
QEMU_EXITED = "qemu_exited"      # the VM process died; nothing timed out
STALLED = "stalled"              # every progress signal froze
REBOOT_LOOP = "reboot_loop"      # adbd reached and lost, repeatedly
SANITY_CEILING = "sanity_ceiling"  # still moving, but implausibly long
BOOT_TIMEOUT = "boot_timeout"    # an explicit --timeout cap was hit
BOOT_FAILED = "boot_failed"      # a caller that has not been converted yet



class BootOutcome:
    """Why a boot ended -- and still a plain yes/no answer.

    THE BUG THIS EXISTS FOR: `wait_for_boot` returned a bare `False` for every
    kind of failure, so its callers had nothing to report but a guess, and the
    guess they all made was the literal string "boot_timeout". A user's fresh
    install then failed with QEMU dying 13.7 s after spawn -- the process was
    GONE, nothing timed out -- and the report said "boot timeout", which sent
    everyone reading it to the boot-wait code. The one fact that would have
    identified it was discarded at the return statement.

    Falsy on failure, so the thirteen `if not wait_for_boot(...)` call sites go
    on asking exactly the question they always asked. What is new is that the
    callers which REPORT the failure can now say which one it was.

    `detail` is the evidence, when there is any: for a dead QEMU that is the
    tail of its own log, which is the only place its reason is ever written.
    """

    __slots__ = ("ok", "reason", "detail")

    def __init__(self, ok, reason, detail=""):
        if not ok and not reason:
            # An unnamed failure is precisely the defect above. Refuse to
            # construct one rather than let it travel.
            raise ValueError("a failed boot must be given a reason")
        self.ok = bool(ok)
        self.reason = reason
        self.detail = detail or ""

    def __bool__(self):
        return self.ok

    def __repr__(self):
        return (f"BootOutcome(ok={self.ok}, reason={self.reason!r}, "
                f"detail={self.detail[:60]!r})")


def _readings_of(fingerprint):
    """The VALUES in a fingerprint, whatever shape it arrived in.

    Three shapes are in use and the difference matters, because getting it
    wrong silently disables the all-blank guard (it did: a fingerprint of
    `(("cpu", None), ("serial", None))` was read as two non-None *pairs*, so a
    boot whose every signal was unreadable counted as moving and could never
    reach `stalled()`):

      * a mapping           -> its values
      * a sequence of PAIRS -> the second element of each, which is what
                               `dict.items()` produces and what wait_for_boot
                               passes
      * anything else       -> itself
    """
    if hasattr(fingerprint, "values"):
        return list(fingerprint.values())
    try:
        items = list(fingerprint)
    except TypeError:
        return [fingerprint]
    if items and all(isinstance(i, tuple) and len(i) == 2 for i in items):
        return [v for _k, v in items]
    return items


def _all_blank(fingerprint):
    try:
        return all(v is None for v in _readings_of(fingerprint))
    except TypeError:
        return fingerprint is None
