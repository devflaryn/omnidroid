# omnidroid/qemu_proc.py
"""QEMU command construction, process spawn, and QMP monitor access."""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

from omnidroid import config
from omnidroid import lean
from omnidroid.bases import (
    base_type, BASE_TYPE_ARM, devkit_disk_for_base, arm_edk2_code,
    ARM_BASE_EFIVARS,
)
from omnidroid.config import IS_WINDOWS, IS_LINUX, IS_MACOS, qemu_bin


def qmp(acct, execute, arguments=None, timeout=6):
    """Send one QMP command; returns parsed response line or None."""
    try:
        with socket.create_connection(("127.0.0.1", acct["qmp_port"]),
                                      timeout=timeout) as s:
            s.settimeout(timeout)
            f = s.makefile("rw", encoding="utf-8", newline="\n")
            f.readline()                                   # greeting
            f.write('{"execute":"qmp_capabilities"}\n'); f.flush()
            f.readline()
            msg = {"execute": execute}
            if arguments:
                msg["arguments"] = arguments
            f.write(json.dumps(msg) + "\n"); f.flush()
            return json.loads(f.readline())
    except Exception:
        return None


# ---------- qemu ----------

def default_accel():
    """Hypervisor auto-detect: WHPX on Windows, HVF on macOS (Apple Silicon),
    KVM on Linux. Overridable per-start with --accel (e.g. 'tcg' for a
    no-hypervisor smoke test)."""
    if IS_WINDOWS:
        return "whpx,kernel-irqchip=off"
    if IS_MACOS:
        return "hvf"
    return "kvm"


def _gl_window_requested():
    """env OMNI_GL_WINDOW=1 asks ANY start to open a native window.

    This began as the B2 spike switch and is kept as an alias for `--mode
    gaming`, so the B2 runbook's one-liner still means something. What changed
    is that it no longer hardcodes GPU args: it only sets the REQUEST, and
    default_display still decides what the host can actually give it. The old
    behaviour emitted `-device virtio-gpu-gl` unconditionally, which on a QEMU
    without virglrenderer (the Homebrew macOS build — see the capability block
    below) is not a device model at all, so QEMU exited instead of booting."""
    return os.environ.get("OMNI_GL_WINDOW", "").strip() not in ("", "0", "false", "False")


# ------------------------------------------------------- display capability
#
# THREE tiers, because the middle one is what the primary host actually has.
# MEASURED 2026-08-06 on the dev Mac (Homebrew QEMU 11.0.2, Apple Silicon):
#
#     $ qemu-system-aarch64 -display cocoa,gl=on
#     qemu-system-aarch64: OpenGL support was not enabled in this build of QEMU
#     $ qemu-system-aarch64 -device help | grep gpu
#     name "virtio-gpu-pci", bus PCI, alias "virtio-gpu"    <- no -gl variant
#
# and there is no `virglrenderer` Homebrew formula to add one. So the `gl`
# tier is real but NOT reachable on this host today; it needs a QEMU built
# with --enable-opengl --enable-virglrenderer.
#
# Collapsing that to "headless" would be the expensive mistake, because a
# NATIVE WINDOW is available right now and is already the big input-latency
# win: with `-display cocoa` the host's mouse/key events go straight into the
# guest's usb-tablet/usb-kbd, instead of a VNC round trip through
# framebuffer encode -> decode -> synthesised input. Rendering is still
# software in that tier; latency is not.
GL_GPU_DEVICE = "virtio-gpu-gl-pci"          # needs virglrenderer in QEMU
HEADLESS_GPU_ARGS = ["-device", "virtio-gpu-pci"]
HEADLESS_DISPLAY_ARGS = ["-display", "none"]

# Windowing backends that can carry a real window, best first per platform.
# `vnc`, `none`, `curses`, `egl-headless` and `dbus` are deliberately absent:
# none of them opens a low-latency native window with input attached.
_WINDOW_BACKENDS = {"macos": ("cocoa",), "linux": ("gtk", "sdl"),
                    "windows": ("gtk", "sdl")}


def _platform_key():
    if IS_MACOS:
        return "macos"
    if IS_WINDOWS:
        return "windows"
    return "linux"


def _host_has_gui():
    """Whether a host GUI session plausibly exists to put a window in.

    Conservative on purpose: a false negative costs a window (we fall back to
    the headless path that already works), a false positive costs a BOOT —
    QEMU exits when a display backend cannot connect. macOS and Windows always
    have a window server; Linux is judged by $DISPLAY/$WAYLAND_DISPLAY, which
    is exactly what is missing over a plain ssh session."""
    if IS_MACOS or IS_WINDOWS:
        return True
    return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))


def _qemu_help_texts(tool):
    """(display_help, device_help) as reported by the resolved QEMU binary.

    Returns ("", "") on ANY failure — missing binary, timeout, non-zero exit.
    default_display reads that as "no window is possible", which degrades to
    the headless path. Asking QEMU beats hardcoding a build matrix: the same
    version is compiled with wildly different feature sets by brew, apt and
    the Windows bundle."""
    try:
        b = qemu_bin(tool)
        def _run(*args):
            return subprocess.run([b, *args], capture_output=True, text=True,
                                  timeout=10).stdout or ""
        return _run("-display", "help"), _run("-device", "help")
    except Exception:
        return "", ""


def default_display(qemu_display_help="", qemu_device_help="", has_gui=True):
    """What kind of window this host can open, as a capability descriptor.

    Pure: every host fact is an argument, so the whole matrix is unit-testable
    and nothing here shells out. Mirrors default_accel() in spirit — detect,
    never assume. Returns a dict with:

        tier          "gl" | "window" | "none"
        available     tier != "none"
        gpu_args      the -device pair to use
        display_args  the -display pair to use
        reason        human-readable, and ACTIONABLE when a tier was missed
                      ("your QEMU lacks X") rather than just "unavailable"
    """
    def _none(reason):
        return {"available": False, "tier": "none", "gpu_args": [],
                "display_args": [], "reason": reason}

    if not has_gui:
        return _none("no host GUI session (headless/ssh) to open a window in")
    backend = next((b for b in _WINDOW_BACKENDS[_platform_key()]
                    if b in qemu_display_help), None)
    if not backend:
        wanted = "/".join(_WINDOW_BACKENDS[_platform_key()])
        return _none(f"this QEMU build has no {wanted} display backend")
    if GL_GPU_DEVICE in qemu_device_help:
        return {"available": True, "tier": "gl",
                "gpu_args": ["-device", GL_GPU_DEVICE],
                "display_args": ["-display", f"{backend},gl=on"],
                "reason": f"{backend},gl=on + {GL_GPU_DEVICE} (3D accelerated)"}
    return {"available": True, "tier": "window",
            "gpu_args": list(HEADLESS_GPU_ARGS),
            "display_args": ["-display", backend],
            "reason": (f"native {backend} window, software rendering — this "
                       f"QEMU has no {GL_GPU_DEVICE} (built without "
                       f"virglrenderer/OpenGL)")}


def gpu_display_args(want_window, capability):
    """(gpu_args, display_args) for this boot. Pure, and NEVER raises.

    Any malformed capability, any tier of "none", or simply not wanting a
    window, yields today's headless pair unchanged. That total-degradation
    property is the compatibility guarantee: a detection bug can cost a
    window, never a boot."""
    if not want_window or not isinstance(capability, dict):
        return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)
    gpu = capability.get("gpu_args") or []
    display = capability.get("display_args") or []
    if not capability.get("available") or not gpu or not display:
        return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)
    return list(gpu), list(display)


def x86_cpu_model(accel, cfg=None):
    """The -cpu model for an x86 guest. `qemu64` unless config says otherwise.

    `qemu64` is QEMU's portable BASELINE: roughly a K8-era x86-64 with SSE2
    and nothing since -- no SSE4.2, no AVX, no AES-NI. It LOOKS like an
    obvious win to pass `host` through instead, especially here, where the
    x86 base exists to run an arm64 APK through libndk_translation and every
    translated NEON instruction has to land on some vector ISA.

    MEASURED, and it is not. On a Windows/WHPX host (i7-13700F, Bliss 16.9.7,
    Android 13) three boots with `-cpu host` did not complete at all -- one
    hit a 6-minute timeout, two hit 15-minute timeouts -- while the same
    image on the same machine with `qemu64` booted in 0.9 min. The guest sat
    near-idle rather than working hard, which points at the kernel taking a
    bad path on features it did not expect (a 13th-gen hybrid P/E topology
    is an unusual thing to hand a 2016-era Android x86 kernel) rather than at
    raw compute.

    So the baseline stays the default, and `host` is available for a host
    that wants it via config qemu.cpu. Revisit with a newer base kernel;
    do NOT flip this back on reasoning alone -- re-measure.
    """
    override = ((cfg or {}).get("qemu") or {}).get("cpu")
    if override:
        return override
    return "qemu64"


def machine_arg(accel):
    """-machine string. On Linux/KVM add mem-merge=on explicitly: it marks
    guest RAM MADV_MERGEABLE so KSM can dedup identical pages across
    instances (it is the QEMU default, but distro builds vary — be
    explicit; it is what the whole Linux scaling story depends on)."""
    m = f"q35,accel={accel}"
    if IS_LINUX and accel.split(",")[0] == "kvm":
        m += ",mem-merge=on"
    return m


def check_accel():
    """Linux preflight: warn loudly if /dev/kvm is unusable (QEMU would
    fail or crawl under TCG). Windows/WHPX has no equivalent check."""
    if not IS_LINUX:
        return
    import os
    kvm = Path("/dev/kvm")
    if not kvm.exists():
        print("[accel] WARNING: /dev/kvm missing - KVM unavailable. "
              "Enable VT-x/AMD-V in BIOS and install qemu-system-x86; "
              "check with 'kvm-ok' (apt install cpu-checker).")
    elif not os.access(kvm, os.R_OK | os.W_OK):
        print("[accel] WARNING: no permission on /dev/kvm - add your user "
              "to the kvm group: sudo usermod -aG kvm $USER (re-login).")


# Per-instance performance modes. Counts are NEVER capped — these tune the
# per-instance footprint; the host's free RAM decides how many run.
# ALL instances are HEADLESS (-display none), always: no host window
# exists anywhere. View/control happens via adb (screenshot/logcat) or an
# optional VNC viewer on the instance's vnc_port (127.0.0.1 only — see
# the port scheme note at allocate_ports). With no window the
# old VirGL path (needed a host GL window) and the R/B software-blit swap
# are both moot — guest-side rendering is unchanged and screencap is
# always true-color.
#
# THE MEMORY MODEL — `mem` is not the footprint. Read this before tuning it.
#
# `mem` is the guest's ADDRESS SPACE: how much RAM Android thinks it has. It
# has to be big enough for the guest to boot and to hold the game, and it is
# NOT what the host pays. What the host pays is how many of those pages are
# actually backed, and that is governed by `balloon`/free-page-reporting
# below. Sizing `mem` down to "the footprint we want" is the mistake this
# comment exists to prevent: it was set to 512 here on the theory that a
# squeezed instance needs no more, and a 512 MB instance simply never boots —
# measured 2026-08-05, two separate 5- and 7-minute boots that never reached
# adbd. 1024 boots but idles at 143 MB available, which the game does not fit
# into. 2048 boots in ~40 s and leaves ~1 GB available with Roblox resident.
#
# So: `mem` = enough to boot and run comfortably, `balloon` = the number you
# actually care about.
MODES = {
    # gaming: the OTHER use case, and the only mode that asks for a host
    # window. Everything above is a RAM/CPU tier on the same headless boot;
    # this one optimises for frames and input latency instead, and expects
    # one or two instances rather than fifty.
    #
    # Why a new mode rather than teaching `playable` to do it: `playable` is
    # DEFAULT_MODE, so every bare `omnidroid start` resolves to it. Opening a
    # window there would put a QEMU window on every existing start, including
    # the ones running under automation with nobody at the screen. Additive
    # beats surprising.
    #
    # `window: True` is a REQUEST, never a promise — default_display decides
    # what the host can actually provide and gpu_display_args degrades to the
    # headless pair when the answer is "nothing". A gaming boot on a machine
    # with no window server is a normal headless boot, not an error.
    #
    # No balloon: reclaiming pages out from under a running game is a stutter
    # source, and this mode is not trying to fit fifty instances in a host.
    # free-page-reporting is still attached (balloon_device is unconditional),
    # which costs nothing while nobody inflates it.
    #
    # `profile` is the POST-BOOT intent, and it is what the engine branches on
    # instead of the mode name. Two values:
    #   "performance"  spend host resources on one instance: no balloon, no
    #                  squeeze, native resolution, the game on the top-app
    #                  cpuset, the quality ClientAppSettings profile. Used by
    #                  gaming/playable/hard/brutal.
    #   "density"      spend quality on instance COUNT: squeeze, zram, balloon,
    #                  5 fps tick, 480x270. Used by farming.
    # Branching on the name was a real bug: `_ensure_booted` compared the RAW
    # --mode argument, so a bare `omnidroid start` (which resolves to
    # `playable`) matched neither "gaming" nor "farming" and got NO post-boot
    # tuning at all — the default mode was the only untuned one.
    #
    # `autoscale` says this mode should GROW to the host. playable and gaming
    # are "give this instance the machine"; hard/brutal are explicit "give it
    # less" requests and must stay the fixed tiers they advertise.
    "gaming":   {"mem": 4096, "smp": 4, "balloon": None, "usb": True,
                 "display": lean.NATIVE_DISPLAY, "window": True,
                 "profile": "performance", "autoscale": True,
                 "quality": "high"},
    "playable": {"mem": 4096, "smp": 4, "balloon": None,
                 "usb": True, "display": lean.NATIVE_DISPLAY,
                 "profile": "performance", "autoscale": True,
                 "quality": "high"},
    "hard":     {"mem": 3072, "smp": 4, "balloon": None,
                 "usb": True, "display": lean.NATIVE_DISPLAY,
                 "profile": "performance", "quality": "balanced"},
    "brutal":   {"mem": 2048, "smp": 2, "balloon": None,
                 "usb": True, "display": lean.NATIVE_DISPLAY,
                 "profile": "performance", "quality": "balanced"},
    # farming: headless, joined-idle, squeezed as small as stable. smp 1
    # because 50+ instances means 50+ vCPU threads, and a joined-idle game
    # loop does not need a second core. `balloon` is the post-boot reclaim
    # target applied once the runtime squeeze has finished — see
    # apply_balloon_target.
    #
    # usb stays TRUE, and that is a reversal worth recording. Dropping the
    # xHCI controller + tablet + keyboard looked like free savings: nothing
    # taps a farming instance by hand, input goes through `adb shell input`.
    # Then a guest that rebooted came up with adb UNAUTHORIZED (the
    # androidboot.insecure_adb authorization does not survive a guest-
    # initiated reboot), which puts an "Allow USB debugging?" dialog on
    # screen — and with no input device there is no way to dismiss it, from
    # adb (unauthorized) or from QMP (input-send-event needs a device). The
    # instance was permanently unreachable. Across a 50-instance fleet an
    # unrecoverable instance costs far more than the handful of device models
    # it saves, and measurement put the real wins in the balloon and the
    # package trim, not here. Keep the hands on the machine.
    #
    # balloon=1536 is MEASURED, not chosen (2026-08-05, arm64 base + the
    # squeeze below). At 1024 the guest boots and looks healthy, and then
    # Roblox dies the moment it finishes loading: "Process com.roblox.client
    # has died: fg TOP" with "Rescheduling restart ... for mem-pressure-event"
    # in logcat. At 1536 the same instance holds the game at 614 MB resident
    # with 336 MB still available and no kills. The floor is set by the game
    # (~614 MB) plus a squeezed Android (~690 MB), and no amount of host-side
    # tuning moves it — only shrinking the guest workload does.
    # Two balloon targets, because the safe floor depends on whether the
    # guest has zram. MEASURED 2026-08-05 (arm64, real Roblox APK):
    #   no zram   -> 1024 kills the game ("has died: fg TOP" +
    #                mem-pressure-event); 1536 holds it at 614 MB with 336 MB
    #                spare. So 1536 is the floor for an unsqueezed guest.
    #   with zram -> lz4 compresses ~3x, so the guest holds far less. The
    #                floor was then walked down on a real PRODUCTION instance
    #                (non-rooted, zram from the baked property, game running):
    #                  896 -> alive, ~200 MB RSS, ~590 MB swapped, 0 kills,
    #                         85-131 MB available, sustained 3+ min  <- default
    #                  768 -> alive, 0 kills, but only 34 MB available
    #                  640 -> the game DIES (2 mem-pressure kills)
    # 896 rather than 768: "only the instances must be on" is the stated
    # requirement, and 34 MB of headroom is not a margin, it is luck. 896 held
    # with zero kills and real headroom, and is 12.5% denser than the 1024
    # this started at. Anyone who wants 768 can now ask for it — `--balloon`
    # is honoured exactly (see resolve_mode; it used to be overridden here).
    # Every number was measured against the game on its login screen, so a
    # joined instance has less headroom than these suggest.
    #
    # The no-zram 1536 is likewise deliberately NOT lowered to 1280, even
    # though 1280 was measured to hold after the 34-package trim landed
    # (game alive at ~517 MB, zero kills, stable). It leaves only 117 MB
    # available against 336 MB at 1536, and the stated requirement is "the
    # instances must be on" — an OOM-killed game is an instance that is off,
    # which costs more than the density gains. Anyone who wants that trade
    # can take it explicitly with `--balloon 1280`.
    "farming":  {"mem": 2048, "smp": 1, "balloon": 1536, "balloon_zram": 896,
                 "usb": True, "display": lean.FARMING_DISPLAY,
                 "profile": "density", "quality": "low"},
}
DEFAULT_MODE = "playable"


# ------------------------------------------------------- performance sizing
#
# What "use the most resources" has to mean in practice. An autoscaled mode
# takes as much of the host as it can WITHOUT putting the host itself under
# memory pressure, because the failure this guards against is not a slow
# guest — it is the host swapping, which makes everything (including QEMU's
# own vCPU threads) miss deadlines, and on macOS eventually kills the process.
#
# So: half of physical RAM, never leaving the host less than HOST_RESERVE_MB,
# clamped into [FLOOR, CEIL]. The floor is the measured requirement (a guest
# under ~2 GB cannot hold Roblox at all — see the balloon notes above, where
# 1024 MB OOM-killed the game); the ceiling is where more guest RAM stops
# buying frames, since Roblox's working set is ~614 MB resident and the rest
# is page cache.
#
# vCPUs: cores minus two, so the host keeps a core for QEMU's own I/O threads
# and a core for everything else. Never more than PERF_SMP_CEIL — QEMU's
# per-vCPU threads cost real host CPU even when the guest is idle, and Android
# scales poorly past a handful of cores in an emulated SoC.
PERF_MEM_FLOOR_MB = 4096
PERF_MEM_CEIL_MB = 8192
PERF_HOST_RESERVE_MB = 6144
PERF_MEM_FRACTION = 0.5
PERF_MEM_GRANULARITY_MB = 512
PERF_SMP_FLOOR = 4
PERF_SMP_CEIL = 8
PERF_SMP_HOST_RESERVE = 2

# WHPX-only ceilings. See the comment in autoscale_perf(): on Windows the
# autoscaled 8192 MB / 8 vCPU booted 6.5x SLOWER than 4096 MB / 4 vCPU on the
# same host and image, so on WHPX these are caps, not targets.
WHPX_MEM_CEIL_MB = 4096
WHPX_SMP_CEIL = 4


def autoscale_perf(mode, host_mem_mb=None, host_cpus=None):
    """Grow an autoscaling mode to fit THIS host. Pure; never raises.

    Returns a NEW dict. A mode without `autoscale`, or a host whose capacity
    could not be read, comes back byte-identical to what went in — an
    unreadable host must cost you the upgrade, never the boot.
    """
    if not isinstance(mode, dict) or not mode.get("autoscale"):
        return dict(mode) if isinstance(mode, dict) else mode
    m = dict(mode)
    if host_mem_mb:
        want = min(host_mem_mb * PERF_MEM_FRACTION,
                   host_mem_mb - PERF_HOST_RESERVE_MB)
        want = int(want // PERF_MEM_GRANULARITY_MB) * PERF_MEM_GRANULARITY_MB
        # max() with the mode's own floor, not just the constant: a mode that
        # declares more than the floor is stating a requirement, and shrinking
        # to fit a small host would silently break the thing it asked for.
        m["mem"] = max(PERF_MEM_FLOOR_MB, m.get("mem", 0),
                       min(want, PERF_MEM_CEIL_MB))
    if host_cpus:
        m["smp"] = max(PERF_SMP_FLOOR,
                       min(PERF_SMP_CEIL, host_cpus - PERF_SMP_HOST_RESERVE))
    # WHPX does not scale the way KVM/HVF do. Growing an instance to the
    # host's capacity makes it DRAMATICALLY slower to boot there, which is
    # the opposite of what autoscaling is for.
    #
    # MEASURED on Windows/WHPX (i7-13700F, 32 GB, Bliss 16.9.7): the very
    # same image and offset booted in 0.9 min at 4096 MB / 4 vCPU and took
    # 5.9 min at the autoscaled 8192 MB / 8 vCPU -- 6.5x worse for twice the
    # resources. The guest sat near-idle while slow, so this is WHPX's
    # per-vCPU exit/IPI cost rather than the guest wanting more.
    #
    # Deliberately WHPX-only: KVM and HVF scale as expected and keep the
    # larger ceilings.
    if IS_WINDOWS:
        m["mem"] = min(m.get("mem", WHPX_MEM_CEIL_MB), WHPX_MEM_CEIL_MB)
        m["smp"] = min(m.get("smp", WHPX_SMP_CEIL), WHPX_SMP_CEIL)
    return m


def host_capacity():
    """(total_ram_mb, cpu_count) for this host; either may be None.

    Impure counterpart to autoscale_perf, kept separate so the sizing policy
    stays unit-testable without a host probe. Every failure yields None, which
    autoscale_perf reads as 'keep the declared defaults'."""
    import multiprocessing
    mem = None
    try:
        if IS_MACOS:
            r = subprocess.run(["sysctl", "-n", "hw.memsize"],
                               capture_output=True, text=True, timeout=10)
            mem = int((r.stdout or "0").strip()) // 1048576 or None
        elif IS_LINUX:
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    mem = int(line.split()[1]) // 1024
                    break
        elif IS_WINDOWS:
            import ctypes

            class _MS(ctypes.Structure):
                _fields_ = [("dwLength", ctypes.c_ulong),
                            ("dwMemoryLoad", ctypes.c_ulong),
                            ("ullTotalPhys", ctypes.c_ulonglong),
                            ("ullAvailPhys", ctypes.c_ulonglong),
                            ("ullTotalPageFile", ctypes.c_ulonglong),
                            ("ullAvailPageFile", ctypes.c_ulonglong),
                            ("ullTotalVirtual", ctypes.c_ulonglong),
                            ("ullAvailVirtual", ctypes.c_ulonglong),
                            ("ullAvailExtendedVirtual", ctypes.c_ulonglong)]
            st = _MS()
            st.dwLength = ctypes.sizeof(_MS)
            ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(st))
            mem = int(st.ullTotalPhys) // 1048576 or None
    except Exception:  # noqa: BLE001 — an unreadable host is not an error here
        mem = None
    try:
        cpus = multiprocessing.cpu_count()
    except Exception:  # noqa: BLE001
        cpus = None
    return mem, cpus


def resolve_mode(cfg, name=None, mem=None, balloon=None, smp=None,
                 host=None):
    """The resolved mode dict for one boot.

    Order is load-bearing: AUTOSCALE FIRST, explicit flags second, so an
    explicit `--mem`/`--smp` always wins outright over the host-derived size.
    `host` is an injectable (mem_mb, cpus) pair for tests; None probes."""
    m = dict(MODES[name or DEFAULT_MODE])
    m["name"] = name or DEFAULT_MODE
    if m.get("autoscale"):
        host_mem, host_cpus = host if host is not None else host_capacity()
        m = autoscale_perf(m, host_mem, host_cpus)
        m["name"] = name or DEFAULT_MODE
    if smp:
        m["smp"] = smp
    if mem:
        m["mem"] = mem
    if balloon is not None:
        # 0 disables the post-boot reclaim without having to special-case
        # None at the call site (argparse cannot express "absent vs zero").
        m["balloon"] = balloon or None
        # An EXPLICIT --balloon must win outright. Dropping balloon_zram is
        # what makes that true: otherwise apply_balloon_target probes the
        # guest, finds zram, and quietly substitutes the mode's own zram
        # figure — so `--balloon 896` on a zram guest silently ran at 1024.
        # Same family as the `--mem` bug this file already documents: a flag
        # accepted by argparse and then overridden downstream is worse than
        # one that was never offered.
        m.pop("balloon_zram", None)
    return m


def resolve_gpu_display(mode, interactive, tool):
    """The (gpu_args, display_args) pair for one boot, host-checked.

    A window is wanted when the mode asks for one (`gaming`) or OMNI_GL_WINDOW
    is set — never on an interactive builder boot, which already redirects the
    console to a serial log and runs unattended.

    The host probe is skipped entirely unless a window is wanted: it costs two
    QEMU subprocess launches, and a host starting fifty farming instances must
    not pay that fifty times for an answer it would discard."""
    want = (bool(mode.get("window")) or _gl_window_requested()) and not interactive
    if not want:
        return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)
    cap = default_display(*_qemu_help_texts(tool), has_gui=_host_has_gui())
    if not cap.get("available"):
        print(f"[gpu] no host window available ({cap.get('reason')}); "
              f"booting headless — attach with `omnidroid view <name>`")
    return gpu_display_args(True, cap)


def balloon_device(mode):
    """The virtio-balloon device args for this mode, or [] when unwanted.

    free-page-reporting is the load-bearing flag, not the balloon itself:
    with it, the guest hands every page it frees straight back to the host
    without anyone asking, so host RSS tracks the guest's live set instead of
    its `-m` size. Measured on the arm64 base (2026-08-05): a booted 2 GB
    guest that idles at ~850 MB guest-used sits at ~120-250 MB host RSS
    instead of 2 GB.

    It is attached in EVERY mode, including playable. A guest kernel without
    the driver just leaves the device unused, so there is no downside branch
    to maintain, and the memory win is not farming-specific.

    PLATFORM NOTE: the reclaim is real on Linux/KVM, where the madvise QEMU
    issues actually decommits the page. On macOS/HVF it is advisory — the
    same test showed host RSS staying high (and rising, from thrash) after a
    balloon inflate. That is why the 50+-instance target is a Linux number;
    macOS runs the 2-3 playable instances and does not pretend otherwise."""
    return ["-device", "virtio-balloon-pci,free-page-reporting=on,id=omniball"]


def usb_devices(mode, arm):
    """USB controller + input devices, or [] for a mode that has no hands on
    it. Two controllers used to be attached on arm (nec-usb-xhci AND
    qemu-xhci) with the input devices bound only to the first — the second
    was dead weight on every single arm boot."""
    if not mode.get("usb", True):
        return []
    if arm:
        return [
            "-device", "nec-usb-xhci,id=usb-bus",
            "-device", "usb-tablet,bus=usb-bus.0",
            "-device", "usb-kbd,bus=usb-bus.0",
        ]
    return ["-device", "qemu-xhci", "-device", "usb-kbd",
            "-device", "usb-tablet"]


def _assert_port_triple(acct):
    """The per-account port triple must be distinct (the shared-index scheme
    guarantees it below 1000 instances; assert anyway before handing the
    ports to QEMU). Returns the QEMU -vnc display number."""
    if len({acct["adb_port"], acct["qmp_port"], acct["vnc_port"]}) != 3:
        sys.exit(f"error: port collision for '{acct['name']}': "
                 f"adb {acct['adb_port']} qmp {acct['qmp_port']} "
                 f"vnc {acct['vnc_port']}")
    vnc_display = acct["vnc_port"] - 5900     # QEMU -vnc takes a display #
    if vnc_display < 0:
        sys.exit(f"error: vnc_port {acct['vnc_port']} is below QEMU's "
                 f"5900 display offset")
    return vnc_display


def qemu_command_arm(acct, cfg, interactive, mode=None, accel=None,
                     debug=False, warm=None, bake=False):
    """arm-uefi (LineageOS arm64) QEMU command — native under HVF on Apple
    Silicon, NO translation layer. UEFI/GRUB disk boot: EDK2 pflash CODE +
    per-account writable efivars, GPT system disk (vda, the provisioned
    overlay carrying /metadata FBE keys) + /data (vdb). Same headless +
    localhost-VNC model as x86; base flags proven in tools/arm64/boot_arm64.sh.

    `interactive` is a BOOT PROFILE (builder/maintenance boots: full host
    smp/mem + a serial log), NOT a base selection. `debug` attaches the devkit
    disk as vdc for reverse-engineering work. They are independent: a normal
    headless production boot can be a debug boot, and an interactive builder
    boot need not be."""
    from omnidroid.engine import runtime_dir, account_dir
    base = cfg["bases"][acct["base"]]
    q = cfg["qemu"]
    d = account_dir(acct["name"])          # overlay disks only (Task 4 removes these)
    rd = runtime_dir(acct["name"])         # per-boot files: efivars.fd, serial.log
    images = Path(cfg["images_dir"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)
    vnc_display = _assert_port_triple(acct)
    smp = q["smp"] if interactive else mode["smp"]
    mem = q["mem_mb"] if interactive else mode["mem"]
    # Headless unless THIS boot asked for a window and the host can open one
    # (see resolve_gpu_display). Every other mode gets the byte-for-byte
    # headless pair it has always had.
    gpu_args, display_args = resolve_gpu_display(mode, interactive,
                                                 "qemu-system-aarch64")
    gpu_display = gpu_args + display_args

    # EPHEMERAL (fully-shared, no-persistence) instances boot the SHARED provisioned
    # base templates DIRECTLY with snapshot=on: every write goes to a throwaway
    # per-process overlay that QEMU discards on exit, so nothing persists and many
    # instances of the same base run CONCURRENTLY (each opens the backing read-only).
    # The instance is then pure config (accounts.json cookie/alias) with NO
    # per-account system/data/devkit qcow2 files — only a fresh per-boot efivars.
    # Non-ephemeral accounts keep their per-account COW overlays (unchanged).
    ephemeral = bool(acct.get("ephemeral"))
    # WARM RESTORE: the disks are the golden entry's frozen overlays, opened
    # snapshot=on exactly like a shared template -- so N instances can share
    # one entry and no restore can ever modify it. Unlike the disks, pflash
    # cannot be opened snapshot=on (it needs a real writable file), so
    # efivars is a PRIVATE per-instance copy staged into runtime_dir by
    # spawn_qemu (see _stage_warm_efivars) -- never the entry's own file,
    # or a restore could write into the shared golden entry.
    from omnidroid import warmcache
    if warm is not None and bake:
        # Programming error, not a runtime condition: no legitimate caller
        # asks to both restore a frozen entry AND capture a fresh freeze
        # point in the same boot. Restore silently winning would hide the
        # caller's bug instead of surfacing it.
        raise ValueError("qemu_command_arm: warm and bake are mutually "
                         "exclusive")
    if warm is not None:
        warm = Path(warm)
        sys_src = warm / warmcache.SYSTEM_NAME
        data_src = warm / warmcache.DATA_NAME
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on"
        # The instance's OWN copy (staged by spawn_qemu via
        # _stage_warm_efivars), not warm / warmcache.EFIVARS_NAME -- pflash
        # needs a writable file, and pointing it at the entry directly would
        # let this restore write into a file every other restore of the same
        # entry shares.
        efivars_src = rd / "efivars.fd"
    elif bake:
        # BAKE: writable overlays under the instance's runtime dir. The freeze
        # point has to be persistable, so snapshot=on is exactly wrong here.
        sys_src = rd / "bake_system.qcow2"
        data_src = rd / "bake_data.qcow2"
        disk_opts = ",discard=unmap,detect-zeroes=unmap"
        efivars_src = rd / "efivars.fd"
    elif ephemeral:
        sys_src = images / base["system"]
        # WHICH ROBLOX this boot runs is chosen HERE, by the resolved offset —
        # a thin COW overlay of the base's pristine /data carrying one baked
        # game version (see omnidroid/offsets.py). `data_image` is put on the
        # handle by build_acct(); falling back to base["data"] boots the base's
        # own /data, which on a clean base means NO game at all (the `--apk`
        # and `--offset none` paths).
        data_src = images / (acct.get("data_image") or base["data"])
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on"
        # Ephemeral efivars is refreshed fresh EVERY boot (see
        # _refresh_ephemeral_efivars, called from spawn_qemu before this
        # command is built) into runtime_dir — genuinely per-boot, throwaway.
        efivars_src = rd / "efivars.fd"
    else:
        sys_src = d / "system.qcow2"
        data_src = d / "data.qcow2"
        disk_opts = ",discard=unmap,detect-zeroes=unmap"
        # Non-ephemeral efivars is written ONCE at account creation (see
        # _make_persistent_arm_account, used only by base-build/maintenance
        # flows now) and persists across boots like the overlay disks —
        # stays under account_dir; this whole non-ephemeral path is
        # live-path-straggler territory (Task 5).
        efivars_src = d / "efivars.fd"

    code = arm_edk2_code()
    if not code:
        sys.exit("error: edk2-aarch64-code.fd (UEFI firmware) not found - "
                 "install qemu (brew install qemu) or set qemu.arm_edk2_code "
                 "in configs/paths.json")
    cmd = [
        qemu_bin("qemu-system-aarch64"),
        "-machine", "virt",
        "-accel", accel,          # hvf on Apple Silicon (no translation)
        "-cpu", "host",
        "-smp", str(smp),
        "-m", str(mem),
        # UEFI firmware: read-only CODE + per-account writable vars.
        "-drive", (f"if=pflash,unit=0,file={code},file.locking=off,"
                   "format=raw,readonly=on"),
        "-drive", f"if=pflash,unit=1,file={efivars_src}",
        # System overlay (vda, has /metadata FBE keys) + /data (vdb).
        "-device", "virtio-blk-pci,drive=vda,bootindex=0",
        "-device", "virtio-blk-pci,drive=vdb,bootindex=1",
        "-drive", f"file={sys_src},if=none,id=vda{disk_opts}",
        "-drive", f"file={data_src},if=none,id=vdb{disk_opts}",
        *gpu_display,
        # Built-in VNC server, LOCALHOST ONLY (no auth is safe ONLY because
        # of the 127.0.0.1 bind — HARD RULE, same as x86; never bind a
        # network interface without adding auth in the same change).
        "-vnc", f"127.0.0.1:{vnc_display}",
        *usb_devices(mode, arm=True),
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", "virtio-net-pci,netdev=net0",
        # virtio-rng stays: without it the guest's early entropy pool fills
        # from nothing and boot stalls. virtio-serial was dropped — no guest
        # or host component has ever opened a port on it.
        "-device", "virtio-rng-pci",
        *balloon_device(mode),
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-name", f"omni-{acct['name']}",
    ]
    cmd += devkit_drive_args(acct, cfg, debug, disk_opts)
    if interactive:
        cmd += ["-serial", f"file:{rd / 'serial.log'}"]
    if warm is not None:
        # NOT `-incoming file:<path>`. mapped-ram/multifd must be enabled on
        # the destination before the stream is read, and capabilities can only
        # be set over QMP -- so the load is deferred and driven by
        # warmboot.restore_into(). See the design spec, section 2b(a).
        cmd += ["-incoming", "defer"]
    return cmd


def devkit_drive_args(acct, cfg, debug, disk_opts):
    """QEMU args attaching the devkit disk as vdc — ONLY on a debug boot.

    This is what makes a shipped base dual-use without changing production: a
    production instance never gets this disk, so its hardware profile is
    unchanged and there is no extra block device for an app to enumerate. The
    guest mounts it read-only at /mnt/omni-devkit during activation (see
    _devkit_activate); it is never booted from.

    Source: the shared per-arch template opened snapshot=on (writes discarded)
    for ephemeral instances, else a per-account COW overlay if one exists.
    Returns [] when debug is off or the disk has not been built."""
    if not debug:
        return []
    from omnidroid.engine import account_dir
    base = cfg["bases"][acct["base"]]
    images = Path(cfg["images_dir"])
    per_acct = account_dir(acct["name"]) / "devkit.qcow2"
    shared = not (not acct.get("ephemeral") and per_acct.exists())
    src = devkit_disk_for_base(images, base) if shared else per_acct
    if not src or not Path(src).exists():
        return []
    # The SHARED template must be opened snapshot=on even for a non-ephemeral
    # instance: without it two concurrent debug boots would write the same
    # file. A per-account overlay is already private, so it keeps disk_opts.
    opts = disk_opts
    if shared and "snapshot=on" not in opts:
        opts += ",snapshot=on"
    return ["-device", "virtio-blk-pci,drive=vdc",
            "-drive", f"file={src},if=none,id=vdc{opts}"]


def qemu_command(acct, cfg, interactive, mode=None, accel=None, debug=False,
                 warm=None, bake=False):
    from omnidroid.engine import account_dir, runtime_dir
    base = cfg["bases"][acct["base"]]
    if base_type(base) == BASE_TYPE_ARM:
        return qemu_command_arm(acct, cfg, interactive, mode=mode, accel=accel,
                                debug=debug, warm=warm, bake=bake)
    images = Path(cfg["images_dir"])
    q = cfg["qemu"]
    d = account_dir(acct["name"])
    rd = runtime_dir(acct["name"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)

    vnc_display = _assert_port_triple(acct)

    if warm is not None and bake:
        # Same rule as the arm builder: a caller asking for both is a bug,
        # not a runtime state -- restore silently winning would hide it.
        raise ValueError("qemu_command: warm and bake are mutually exclusive")
    # WARM RESTORE / BAKE: same disk-sourcing rule as arm (see
    # qemu_command_arm), adapted to x86's virtio disk syntax. x86 boots by
    # direct kernel/initrd rather than UEFI pflash, so there is no efivars
    # file here -- that part of the arm branch has no x86 equivalent.
    from omnidroid import warmcache
    if warm is not None:
        warm = Path(warm)
        sys_src = warm / warmcache.SYSTEM_NAME
        data_src = warm / warmcache.DATA_NAME
        disk_opts = ",format=qcow2,if=virtio,snapshot=on"
    elif bake:
        sys_src = rd / "bake_system.qcow2"
        data_src = rd / "bake_data.qcow2"
        disk_opts = ",format=qcow2,if=virtio"
    elif acct.get("ephemeral"):
        # Same model as arm (see qemu_command_arm): boot the SHARED base
        # templates directly with snapshot=on, so every write lands in a
        # throwaway per-process overlay QEMU discards on exit and N instances
        # can run concurrently off one base.
        #
        # WHICH ROBLOX this boot runs is chosen HERE, by the resolved offset —
        # a thin COW overlay of the base's pristine /data carrying one baked
        # version. `data_image` is put on the handle by build_acct(). x86 has
        # no base["data"], so a boot with no offset falls back to the shared
        # empty /data template, which means no game at all (the `--apk` and
        # `--offset none` paths).
        sys_src = images / base["disk"]
        data_src = images / (acct.get("data_image") or cfg["data_template"])
        disk_opts = ",format=qcow2,if=virtio,snapshot=on"
    else:
        sys_src = d / "system.qcow2"
        data_src = d / "data.qcow2"
        disk_opts = ",format=qcow2,if=virtio"

    append = ("stack_depot_disable=on cgroup_disable=pressure "
              "root=/dev/ram0 noexec=off "
              f"SRC={base['src']} DATA=vdb")
    smp = q["smp"]
    mem = q["mem_mb"]

    # Same rule as arm: headless unless this boot asked for a window AND the
    # host can open one. Interactive boots always come back headless.
    gpu_args, display_args = resolve_gpu_display(mode, interactive,
                                                 "qemu-system-x86_64")
    if interactive:
        # Interactive/builder boot: serial console log for debugging (headless
        # like everything else; virtio-vga kept so the guest has its usual DRM
        # device during provisioning/builder sessions).
        append += " console=tty0 console=ttyS0,115200"
        gpu = ["-device", "virtio-vga"]
        nic = "virtio-net-pci,netdev=net0"
    else:
        # Production silent boot (no firmware/console text).
        append += (" quiet loglevel=0 console=null "
                   "vt.global_cursor_default=0 SETUPWIZARD=0")
        nic = "virtio-net-pci,netdev=net0,romfile="   # no iPXE option ROM
        smp = mode["smp"]
        mem = mode["mem"]
        # -vga none first: the emulated VGA adapter is dead weight next to the
        # virtio GPU, in every tier.
        gpu = ["-vga", "none"] + gpu_args

    cmd = [
        qemu_bin("qemu-system-x86_64"),
        "-machine", machine_arg(accel),
        "-cpu", x86_cpu_model(accel, cfg),
        "-smp", str(smp),
        "-m", str(mem),
        "-drive", f"file={sys_src}{disk_opts}",
        "-drive", f"file={data_src}{disk_opts}",
        *gpu,
        *display_args,            # "none" in every mode but gaming
        # Built-in VNC server on the account's reserved port. It stays on even
        # when a native window is open: `omnidroid screenshot`, the auto-capture
        # recorder and the omnidroid-input skill all attach to this
        # framebuffer (capture.py), and dropping it on a gaming boot would
        # silently blind every one of them. LOCALHOST
        # ONLY: no auth is safe ONLY because of the 127.0.0.1 bind — never
        # bind a network interface without adding auth in the same change.
        # Idle (no viewer) it does no framebuffer encoding, so leaving it
        # on costs ~nothing across hours-long headless runs; a viewer
        # disconnecting never affects the instance.
        "-vnc", f"127.0.0.1:{vnc_display}",
        *usb_devices(mode, arm=False),
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", nic,
        *balloon_device(mode),
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-kernel", str(images / base["kernel"]),
        "-initrd", str(images / base["initrd"]),
        "-append", append,
        "-name", f"omni-{acct['name']}",
    ]
    cmd += devkit_drive_args(acct, cfg, debug, ",format=qcow2")
    if interactive:
        cmd += ["-serial", f"file:{d / 'serial.log'}"]
    if warm is not None:
        # Same reasoning as the arm builder: caps (mapped-ram/multifd) can
        # only be set over QMP before the stream is read, so this must be
        # `-incoming defer`, never `-incoming file:<path>`.
        cmd += ["-incoming", "defer"]
    return cmd


def command_opens_a_window(cmd):
    """True when this QEMU command will put a window on the host's screen.

    Read off the command actually being handed to QEMU rather than re-running
    the decision: the argv IS what the process will do, so this cannot drift
    from it the way a second copy of the capability logic would."""
    for i, arg in enumerate(cmd):
        if arg == "-display" and i + 1 < len(cmd):
            return cmd[i + 1].split(",")[0] not in ("none", "")
    return False


def _refresh_ephemeral_efivars(acct, cfg):
    """Give an ephemeral instance a FRESH copy of the base UEFI vars for this boot,
    so nothing persists across boots (the system/data/devkit disks are the shared
    templates opened snapshot=on; efivars is the only writable file, and pflash
    needs a real file). arm-only; no-op otherwise."""
    from omnidroid.engine import runtime_dir
    import shutil
    base = cfg["bases"][acct["base"]]
    if base_type(base) != BASE_TYPE_ARM:
        return
    images = Path(cfg["images_dir"])
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    efi_tmpl = images / base.get("efivars", ARM_BASE_EFIVARS)
    if efi_tmpl.exists():
        shutil.copyfile(efi_tmpl, d / "efivars.fd")


def _stage_warm_efivars(acct, cfg, warm):
    """Give a warm-RESTORE boot its OWN writable copy of the entry's
    efivars.fd, so pflash never writes into the shared golden entry (see the
    comment on efivars_src in qemu_command_arm). Same shape as
    _refresh_ephemeral_efivars: arm-only; a no-op otherwise, since x86 has no
    efivars/pflash concept at all and never reaches here with a warm entry
    (the cache is arm-only -- see engine._ensure_booted)."""
    from omnidroid.engine import runtime_dir
    import shutil
    base = cfg["bases"][acct["base"]]
    if base_type(base) != BASE_TYPE_ARM:
        return
    from omnidroid import warmcache
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    src = Path(warm) / warmcache.EFIVARS_NAME
    if src.exists():
        shutil.copyfile(src, d / "efivars.fd")


def spawn_qemu(acct, cfg, interactive, mode=None, accel=None, debug=False,
               warm=None, bake=False, warm_key=None):
    from omnidroid.runtime import runtime_dir
    check_accel()
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    if acct.get("ephemeral"):
        _refresh_ephemeral_efivars(acct, cfg)
    if warm is not None:
        _stage_warm_efivars(acct, cfg, warm)
    log = open(d / "qemu.log", "w")
    kwargs = {}
    if IS_WINDOWS:
        DETACHED = 0x00000008          # DETACHED_PROCESS
        NEW_GROUP = 0x00000200         # CREATE_NEW_PROCESS_GROUP
        kwargs["creationflags"] = DETACHED | NEW_GROUP
    else:
        kwargs["start_new_session"] = True
    cmd = qemu_command(acct, cfg, interactive, mode, accel=accel, debug=debug,
                       warm=warm, bake=bake)
    proc = subprocess.Popen(cmd, stdout=log, stderr=log, **kwargs)
    (d / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time(),
         # Whether THIS boot put a real window on the host's screen. Recorded
         # rather than recomputed, so `omnidroid start` can stand its VNC viewer
         # down instead of showing a second, laggier window onto the same
         # instance — and so a degraded gaming boot (host could not open one)
         # still gets the viewer it needs to be watchable at all.
         "native_window": command_opens_a_window(cmd),
         "identity": f"omni-{acct['name']}",
         "mode": (mode or {}).get(
             "name", "interactive" if interactive else DEFAULT_MODE),
         # Whether THIS boot attached the devkit — read back by the debug
         # tooling so it can tell "not a debug boot" from "activation failed".
         "debug": bool(debug),
         "base": acct["base"],
         # WHICH Roblox version this boot is running: the resolved offset name
         # and the /data overlay it selected. Recorded rather than recomputed
         # so `list`/`debug-info` can report the live instance's actual version
         # even after the default offset has been changed underneath it.
         "offset": acct.get("offset"),
         "data_image": acct.get("data_image"),
         "adb_port": acct["adb_port"], "qmp_port": acct["qmp_port"],
         "vnc_port": acct["vnc_port"], "warm_key": warm_key}))
    return proc.pid
