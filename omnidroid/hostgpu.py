# omnidroid/hostgpu.py
"""Make Windows give QEMU the machine's REAL graphics card.

WHY THIS EXISTS
---------------
This project measured the difference between rendering the guest on a GPU and
rendering it on the CPU and it is not a matter of degree: **3.2 fps against
16.5-58**. All of that work assumed that once QEMU has a GL context, the
context is on the good GPU. On a desktop with one card that is true. On a
LAPTOP -- which is what most of the people this product ships to are using --
it is not.

A hybrid laptop has two adapters: the CPU's integrated one (Intel Iris /
AMD Radeon Graphics) and a discrete one (GeForce / Radeon RX). Which one an
application gets is decided by Windows' **GPU preference**, and the default
for an unknown executable is `GpuPreference=0` -- "let Windows decide", which
in practice means the **power-saving** adapter. QEMU is an unknown executable
on every machine this product installs onto: it is downloaded into
`%LOCALAPPDATA%`, it is not on any vendor's optimisation list, and it renders
through ANGLE/WGL rather than through anything a driver profile recognises as
a game.

So the guest is composited and virgl-rendered on an iGPU while the discrete
card the user paid for sits idle -- and the symptom is exactly the one that
was reported: "it is slow on a high-tier computer". A high-tier computer is
precisely the machine where the gap between its two GPUs is widest.

THE LEVER
---------
Windows 10 1803+ keeps per-application GPU preferences in the registry, under
the CURRENT USER -- no elevation, no driver control panel, no vendor SDK:

    HKCU\\Software\\Microsoft\\DirectX\\UserGpuPreferences
        "<full path to the exe>" = "GpuPreference=2;"

    0 = let Windows decide (the default, and the problem)
    1 = power saving      -> the integrated adapter
    2 = high performance  -> the discrete adapter

This is the same key the Settings app writes from
*System -> Display -> Graphics*, so it is a supported, user-visible setting
rather than a trick; a user who has set a preference of their own by hand is
left alone (see `apply()`).

It has to be set BEFORE the process starts -- the preference is read when the
adapter is enumerated -- which is why this runs from the spawn path and not
from the viewer.

WHAT IT DOES NOT DO
-------------------
It does not force a GPU that does not exist, it does not touch HKLM, it does
not survive uninstalling, and it never raises: every entry point degrades to
"leave the registry alone and boot", because a machine whose registry cannot
be written is a machine that should still get an instance.
"""
import os
import sys

IS_WINDOWS = sys.platform.startswith("win")

GPU_PREF_KEY = r"Software\Microsoft\DirectX\UserGpuPreferences"
PREF_HIGH_PERFORMANCE = "GpuPreference=2;"
PREF_POWER_SAVING = "GpuPreference=1;"


def _winreg():
    try:
        import winreg
        return winreg
    except Exception:                       # noqa: BLE001 - not Windows
        return None


def read_preference(exe_path):
    """The GPU preference currently recorded for `exe_path`, or None.

    None means "nothing has ever been written for this executable", which is
    the state that gets Windows' own default -- and Windows' own default is
    what this module exists to overrule.
    """
    winreg = _winreg()
    if winreg is None or not exe_path:
        return None
    # abspath, because the VALUE NAME is the path and Windows compares it
    # literally: `.../qemu/qemu-system-x86_64.exe` and
    # `...\qemu\qemu-system-x86_64.exe` are two different entries, and only
    # the backslash form is the one Windows itself looks up when it launches
    # the process. `apply` normalises before writing, so this has to match.
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, GPU_PREF_KEY) as k:
            value, _kind = winreg.QueryValueEx(k, os.path.abspath(str(exe_path)))
            return value
    except FileNotFoundError:
        return None
    except OSError:
        return None


def apply(exe_path, preference=PREF_HIGH_PERFORMANCE, force=False):
    """Ask Windows to run `exe_path` on the high-performance adapter.

    Returns one of:
        "set"        we wrote the preference
        "already"    it already said what we wanted
        "user-set"   somebody set a DIFFERENT preference by hand; left alone
        "skipped"    not Windows, or no path
        "failed"     the registry would not take it (never raises)

    **A preference the user chose is never overwritten**, and that matters
    more than it looks: the one legitimate reason to pin QEMU to the
    integrated adapter is a laptop on battery, or a machine whose discrete
    driver is broken. Someone who has been into Settings and made that choice
    has made it about this exact executable. `force=True` is the override, for
    a user who asks the product to fix it for them.
    """
    if not IS_WINDOWS or not exe_path:
        return "skipped"
    winreg = _winreg()
    if winreg is None:
        return "skipped"
    path = os.path.abspath(str(exe_path))
    current = read_preference(path)
    if current == preference:
        return "already"
    if current and not force:
        return "user-set"
    try:
        with winreg.CreateKeyEx(winreg.HKEY_CURRENT_USER, GPU_PREF_KEY, 0,
                                winreg.KEY_SET_VALUE) as k:
            winreg.SetValueEx(k, path, 0, winreg.REG_SZ, preference)
        return "set"
    except OSError:
        return "failed"


def describe(result, exe_path):
    """One line for the launch log, or "" when there is nothing to say.

    Silent on "already" on purpose: this runs on every boot, and a line that
    appears on every boot and never changes is a line people stop reading.
    """
    if result == "set":
        return ("host gpu: asked Windows to run QEMU on the "
                "HIGH-PERFORMANCE adapter (Settings > Display > Graphics). "
                "On a laptop with two GPUs this is the difference between "
                "the discrete card and the integrated one.")
    if result == "user-set":
        return (f"host gpu: leaving your own graphics preference for "
                f"{os.path.basename(str(exe_path))} alone "
                f"({read_preference(os.path.abspath(str(exe_path)))!r}). "
                f"If the guest renders slowly, set it to High performance in "
                f"Settings > Display > Graphics.")
    if result == "failed":
        return ("host gpu: could not record a graphics preference for QEMU "
                "(registry not writable). If this PC has two GPUs, set QEMU "
                "to High performance in Settings > Display > Graphics.")
    return ""


def adapters():
    """Names of this host's display adapters, best-effort, for diagnostics.

    Never raises and never blocks a boot; an empty list means "could not
    ask", which `doctor` reports as unknown rather than as "one GPU".
    """
    if not IS_WINDOWS:
        return []
    try:
        import subprocess
        out = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command",
             "(Get-CimInstance Win32_VideoController).Name"],
            capture_output=True, text=True, timeout=20,
            creationflags=0x08000000).stdout or ""
        return [l.strip() for l in out.splitlines() if l.strip()]
    except Exception:                       # noqa: BLE001 - diagnostics only
        return []


def is_hybrid(names=None):
    """True when this host has more than one display adapter.

    The heuristic is deliberately just "more than one", not a vendor match: a
    machine with an Intel iGPU and an NVIDIA dGPU, one with an AMD APU and an
    RX card, and one with two discrete cards all want the same answer, and a
    vendor list is a thing that goes stale.
    """
    names = adapters() if names is None else names
    return len([n for n in names if n]) > 1
