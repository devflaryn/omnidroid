# omnidroid/accelprobe.py
"""Prove the hypervisor before betting a boot on it, and boot anyway if it
isn't there.

WHY THIS EXISTS
---------------
`qemu_proc.default_accel()` answers "what SHOULD accelerate a guest on this
platform" -- `whpx` on Windows, `hvf` on macOS, `kvm` on Linux. It is a
statement about the platform, not about the machine, and on Windows the gap
between those two is where the product stopped working for people:

  * **Windows Hypervisor Platform is an OPTIONAL Windows feature.** It is off on
    a stock install. Without it `-accel whpx` fails during machine init.
  * **VT-x / AMD-V is a BIOS switch**, and plenty of prebuilt desktops and
    business laptops ship with it disabled.
  * Windows Home has it too, but a machine whose firmware hides virtualization
    (some older Atom/Celeron parts) does not.

In every one of those cases QEMU exited before it opened a single file, the
engine's boot wait saw the process gone, and the launch reported

    QEMU exited before the guest booted -- see .../qemu.log

...which is true and completely unhelpful: the word "virtualization" appears
nowhere, and the fix is two BIOS keystrokes the user was never told to make.
`check_accel()` had a warning for exactly this, for Linux only, and even there
it only warned -- nothing fell back.

WHAT IT DOES
------------
Ask QEMU. Start it with no disks, no network, no devices and no display, in the
stopped state, with a QMP monitor on stdio, and watch for the QMP greeting:

    qemu-system-x86_64 -machine q35,accel=whpx -display none -m 64 \
                       -nodefaults -no-user-config -S -qmp stdio

The greeting is written after machine init, which is where accelerator init
happens -- so a greeting means the accelerator came up, and an exit without one
means it did not (QEMU prints its reason on stderr, which is captured and
carried in the result). Measured on the dev box: **0.06 s** for a working WHPX,
0.11 s to reject an unavailable accelerator. The verdict is cached per QEMU
binary, so a pool filling ten slots pays it once.

Then FALL BACK rather than fail: `whpx -> tcg`, `kvm -> tcg`, `hvf -> tcg`.
TCG is pure emulation and it is slow -- but slow is a thing the boot wait now
tolerates (see `bootwait.py`, which waits on progress instead of on a clock),
and a slow boot is worth immeasurably more than a machine that cannot run the
product at all. The fallback prints the exact command or switch that would give
the host its hypervisor back.

RULES
-----
This module may never raise into a boot path. Every entry point degrades to
"use the platform default and carry on", which is precisely the behaviour that
existed before it was written -- so the worst case of a bug in here is the old
behaviour, not a new failure.
"""
import os
import shutil
import subprocess
import sys
import threading
from pathlib import Path

IS_WINDOWS = sys.platform.startswith("win")
IS_MACOS = sys.platform == "darwin"

# How long to wait for the QMP greeting before calling the accelerator dead.
# The measured answer is under a tenth of a second; this is sized for a host
# that is thrashing, not for a host that is working.
PROBE_TIMEOUT_S = 25.0

# The emulated fallbacks, best first.
#
# `thread=multi` is not decoration: single-threaded TCG pins an entire guest to
# one host core, and the hosts that need TCG at all are exactly the hosts that
# cannot spare it. But it is NOT universally accepted, and finding that out the
# hard way is what taught this module its own lesson:
#
#     qemu-system-x86_64: Property 'pc-q35-11.1-machine.thread' not found
#
# x86 builds the accelerator into the MACHINE string (`-machine
# q35,accel=tcg,thread=multi`, see qemu_proc.machine_arg), where every
# comma-separated item is a *machine* property. `kernel-irqchip` happens to be
# one, which is why the WHPX string works; `thread` is an *accelerator*
# property and is not. So the multi-threaded form is a CANDIDATE, proven like
# any other, with plain `tcg` behind it.
#
# The bug this caused was in the shortcut, not the string: TCG used to be
# returned unproven on the grounds that it is "always compiled in", so the
# fallback nobody had tested would have failed to start QEMU at all -- on
# precisely the machines this module exists to rescue. Nothing is exempt from
# the probe now.
TCG_ACCEL = "tcg,thread=multi"
TCG_PLAIN = "tcg"
TCG_CANDIDATES = (TCG_ACCEL, TCG_PLAIN)

_CACHE = {}
_CACHE_LOCK = threading.Lock()


class Verdict:
    """What this host can actually do, and what to say about it.

    accel      the accelerator string to hand QEMU
    degraded   True when this is NOT the accelerator that was wanted
    requested  True when the caller named one explicitly (--accel)
    note       one line naming what happened, or "" when nothing did
    advice     the actionable fix for a degraded host, or ""
    """

    __slots__ = ("accel", "degraded", "requested", "note", "advice",
                 "unknown")

    def __init__(self, accel, degraded=False, requested=False, note="",
                 advice="", unknown=False):
        self.accel = accel
        self.degraded = degraded
        self.requested = requested
        self.note = note
        self.advice = advice
        # "I could not ask" -- NOT "the answer is no". Distinguishing these is
        # the difference between telling a first-boot machine that QEMU has not
        # arrived yet and telling it its CPU cannot do virtualization. See
        # NoQemuIsUnknownNotBroken in the tests for the frozen-build report
        # that made this necessary.
        self.unknown = unknown

    def __repr__(self):                                   # pragma: no cover
        return (f"Verdict(accel={self.accel!r}, degraded={self.degraded}, "
                f"requested={self.requested})")


def clear_cache():
    """Forget every probe verdict. For tests, and for `doctor --recheck`."""
    with _CACHE_LOCK:
        _CACHE.clear()


def qemu_runnable(tool):
    """Is there actually a QEMU binary to interrogate?

    On Windows `qemu_bin()` returns the PRODUCT path whether or not it exists
    -- that return value is what drives ensure_qemu() to download it -- so a
    deployment whose QEMU has not been installed yet resolves to a real-looking
    path that cannot be run. Probing it fails, and failing to distinguish that
    from "the accelerator does not work" is what had `doctor` reporting
    "neither whpx nor tcg initialises" on a perfectly healthy PC.
    """
    from omnidroid.config import qemu_bin
    try:
        path = Path(qemu_bin(tool))
    except Exception:      # noqa: BLE001 - unresolvable is also "cannot ask"
        return False
    if path.exists():
        return True
    # Linux/macOS resolve to a bare name (system QEMU on PATH) rather than a
    # product path, so a non-existent Path is not proof of absence there.
    return shutil.which(str(path)) is not None


def probe_command(tool, accel):
    """The argv that asks QEMU whether `accel` works.

    Deliberately inert: `-nodefaults` and `-no-user-config` strip every implicit
    device and any host config file, `-S` leaves the CPU stopped so no guest
    code ever runs, and there is no drive, no netdev and no image. A probe must
    not be able to change anything, must not need a base to be installed, and
    must not care what is in the user's config.
    """
    from omnidroid.config import qemu_bin
    return [qemu_bin(tool),
            "-machine", f"q35,accel={accel}",
            "-display", "none",
            "-m", "64",
            "-nodefaults",
            "-no-user-config",
            "-S",
            "-qmp", "stdio"]


def _qmp_greeting_seen(cmd, timeout):
    """Run `cmd` and return (saw_greeting, stderr_text).

    The greeting is QEMU's first line on the QMP socket and is written AFTER
    machine init -- so it is the positive signal that the accelerator came up.
    Read on a thread with a join deadline rather than with `communicate(timeout=)`
    because a successful probe never exits on its own (it is a paused VM waiting
    for orders) and we want to kill it the moment we have our answer, not sit
    out the whole timeout.
    """
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding="utf-8", errors="replace",
                            **_no_window())
    box = {}

    def read_one():
        try:
            box["line"] = proc.stdout.readline()
        except Exception:                       # noqa: BLE001 - probe, not logic
            box["line"] = ""

    reader = threading.Thread(target=read_one, daemon=True)
    reader.start()
    reader.join(timeout)
    saw = box.get("line", "").lstrip().startswith('{"QMP"')
    try:
        proc.kill()
    except OSError:
        pass
    err = ""
    try:
        err = (proc.stderr.read() or "").strip()
    except Exception:                           # noqa: BLE001
        pass
    try:
        proc.wait(timeout=5)
    except Exception:                           # noqa: BLE001
        pass
    return saw, err


def _no_window():
    """Keep a probe from flashing a console window on Windows."""
    if IS_WINDOWS:
        return {"creationflags": 0x08000000}    # CREATE_NO_WINDOW
    return {}


def probe(tool, accel, timeout=PROBE_TIMEOUT_S):
    """True if `accel` initialises on this host. Never raises."""
    try:
        saw, err = _qmp_greeting_seen(probe_command(tool, accel), timeout)
    except Exception:                           # noqa: BLE001 - see module docstring
        return False
    if not saw and err:
        # Kept for the caller's message: QEMU's own words beat ours.
        with _CACHE_LOCK:
            _CACHE[("why", tool, accel)] = err.splitlines()[-1][:300]
    return saw


def _platform():
    if IS_WINDOWS:
        return "windows"
    if IS_MACOS:
        return "macos"
    return "linux"


# Said alongside every degraded verdict. It is not consolation -- it is the
# difference between "this is unusably slow" and "the FIRST boot is slow". An
# emulated cold boot is minutes; the warm cache turns every launch after it into
# a restore, and TCG can migrate where WHPX cannot, so this host is precisely
# the one the cache was always able to help and was being refused (see
# engine._warm_cache_allowed).
_EMULATED_COST = (
    "  The FIRST boot is the slow one: the result is cached, so later "
    "launches restore instead of booting. Leave it running.")


def _advice_for(platform, family):
    """The one thing the user can DO about a host with no hypervisor."""
    if platform == "windows":
        return (
            "This PC has no usable hardware virtualization, so the guest is "
            "being EMULATED and will boot several times slower.\n"
            + _EMULATED_COST + "\n"
            "  To fix it, both of these have to be true:\n"
            "   1. Virtualization (Intel VT-x / AMD-V, sometimes called SVM) "
            "is enabled in the BIOS/UEFI setup.\n"
            "   2. Windows Hypervisor Platform is installed. In an "
            "Administrator PowerShell:\n"
            "      dism /online /Enable-Feature /FeatureName:HypervisorPlatform /All\n"
            "      ...then reboot.")
    if platform == "macos":
        return (
            "hvf (Apple's Hypervisor framework) did not initialise, so the "
            "guest is being EMULATED and will boot several times slower.\n"
            + _EMULATED_COST + "\n"
            "  hvf needs the binary to carry the "
            "com.apple.security.hypervisor entitlement - a QEMU installed "
            "with `brew install qemu` has it; a hand-built one usually does "
            "not until it is codesigned.")
    return (
        "/dev/kvm is not usable, so the guest is being EMULATED and will boot "
        "several times slower.\n"
        + _EMULATED_COST + "\n"
        "  Enable virtualization (VT-x/AMD-V) in the BIOS, install "
        "qemu-system-x86, and make sure your user can open /dev/kvm:\n"
        "     sudo usermod -aG kvm $USER   (then log out and back in)\n"
        "  Check with: kvm-ok   (apt install cpu-checker)")


def resolve(tool, requested, default=None, platform=None):
    """Decide which accelerator this host will actually use.

    `requested` is the user's `--accel` (None when they did not say). `default`
    is the platform's preferred answer, normally `qemu_proc.default_accel()`;
    it is a parameter so this module does not import qemu_proc (which imports
    plenty) and so tests can drive it directly.

    Returns a `Verdict`. Never raises: any failure inside the probe leaves the
    wanted accelerator in place and the caller boots exactly as it did before
    this module existed.
    """
    platform = platform or _platform()
    want = requested or default or TCG_ACCEL
    # NOTHING TO ASK. Deliberately NOT cached: QEMU is usually about to be
    # installed (the app downloads it during first boot), and caching "unknown"
    # would keep the deployment reporting unknown for the life of the process.
    if not qemu_runnable(tool):
        return Verdict(want, unknown=True,
                       note="QEMU is not installed yet - virtualization has "
                            "not been checked")
    key = (tool, want, platform)
    with _CACHE_LOCK:
        hit = _CACHE.get(key)
    if hit is not None:
        return hit

    verdict = _resolve_uncached(tool, requested, want, platform)
    with _CACHE_LOCK:
        _CACHE[key] = verdict
    return verdict


def _resolve_uncached(tool, requested, want, platform):
    family = want.split(",")[0]
    try:
        if probe(tool, want):
            return Verdict(want, requested=bool(requested))
    except Exception:                           # noqa: BLE001 - see module docstring
        # A broken probe must not cost anyone their hypervisor. Hand back what
        # was wanted and let QEMU be the judge, as it always was.
        return Verdict(want, requested=bool(requested))

    with _CACHE_LOCK:
        why = _CACHE.get(("why", tool, want), "")

    # WORK DOWN THE FALLBACKS, PROVING EACH. `tcg,thread=multi` is worth
    # having (it is the difference between one host core and several on a
    # machine that has no other help) but it is not universally accepted --
    # see TCG_CANDIDATES for the exact error and why. Nothing is assumed.
    fallback = None
    for candidate in TCG_CANDIDATES:
        if candidate == want:
            continue                            # already proven not to work
        try:
            if probe(tool, candidate):
                fallback = candidate
                break
        except Exception:                       # noqa: BLE001
            continue
    if fallback is None:
        # Nothing works, which is a QEMU problem rather than a host one. Hand
        # back what was asked for so the failure names the real thing.
        return Verdict(want, requested=bool(requested),
                       note=f"neither {family} nor tcg initialises"
                            + (f": {why}" if why else ""))

    note = (f"{family} is unavailable on this host"
            + (f" ({why})" if why else "")
            + f" - falling back to software emulation ({fallback})")
    if requested:
        note = (f"--accel {requested} was asked for but {family} is "
                f"unavailable on this host"
                + (f" ({why})" if why else "")
                + f" - falling back to software emulation ({fallback})")
    return Verdict(fallback, degraded=True, requested=bool(requested),
                   note=note, advice=_advice_for(platform, family))


def report(tool, requested, default=None, label="accel"):
    """`resolve`, plus the printing. The one call a boot path wants."""
    v = resolve(tool, requested, default=default)
    if v.note:
        print(f"[{label}] {v.note}")
    if v.advice:
        for line in v.advice.splitlines():
            print(f"[{label}] {line}")
    return v
