# omnidroid/qemu_proc.py
"""QEMU command construction, process spawn, and QMP monitor access."""
import json
import os
import shutil
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


def window_suppressed(cfg=None):
    """True when this boot must NOT put a window on the host, whatever the
    mode asks for.

    The escape hatch for `playable` becoming a windowed mode. `--no-window`
    used to mean only "do not spawn the VNC viewer", which was the whole
    meaning of a window back when no mode opened a native one; `cmd_start` now
    exports OMNI_NO_WINDOW for the same flag, so one flag means one thing —
    nothing appears on screen — for both kinds of window.

    Read from the ENVIRONMENT rather than threaded through six signatures on
    purpose: QEMU argv construction is reached from `_ensure_booted`,
    `spawn_qemu` and the bake/warm paths, and adding a parameter to each is how
    one of them ends up not passing it. Config `qemu.no_window` is the durable
    form for a headless server that should never open one.
    """
    env = os.environ.get("OMNI_NO_WINDOW", "").strip()
    if env:
        return env not in ("0", "false", "False", "no")
    return bool(((cfg or {}).get("qemu") or {}).get("no_window"))


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
# The guest panel size for the GL device, set EXPLICITLY.
#
# virtio-gpu-gl-pci documents xres/yres defaulting to 1280x800, but the mode
# list it hands the guest starts with 640x480 and Android takes the first one:
# a GL boot came up at 640x480 while the software path gave 1280x800. Naming
# them puts the preferred mode where the guest will pick it.
GL_XRES, GL_YRES = 1280, 800
DEFAULT_PANEL = (GL_XRES, GL_YRES)
# Farming draws a postage stamp (lean.FARMING_DISPLAY is 480x270), so the
# PANEL is sized down with it. This is not cosmetic: the panel decides how big
# every buffer in the pipeline is -- the guest's own scanout, SurfaceFlinger's
# triple buffer, and the host-side framebuffer VNC encodes from. 640x480
# rather than a literal 480x270 because 640x480 is a mode every guest kernel
# already has, and `wm size` handles the rest inside Android.
FARMING_PANEL = (640, 480)
# Panels a caller may ask for by name, so `--panel 1080p` is a thing.
PANEL_NAMES = {
    "480p": (854, 480), "720p": (1280, 720), "800p": (1280, 800),
    "900p": (1600, 900), "1080p": (1920, 1080), "1440p": (2560, 1440),
}


def parse_panel(text):
    """(w, h) from "1920x1080" or a name in PANEL_NAMES, else None.

    Never raises: an unparseable value is None, which every caller reads as
    "use the default" -- a typo in a config file must not fail a boot.
    """
    if not text:
        return None
    s = str(text).strip().lower()
    if s in PANEL_NAMES:
        return PANEL_NAMES[s]
    for sep in ("x", "*", ":"):
        if sep in s:
            a, _, b = s.partition(sep)
            try:
                w, h = int(a), int(b)
            except ValueError:
                return None
            # Even widths only: virtio-gpu scanout and every video-ish
            # consumer downstream assume it, and an odd width shows up as a
            # one-pixel tear rather than an error.
            if 320 <= w <= 7680 and 240 <= h <= 4320:
                return (w - (w % 2), h - (h % 2))
            return None
    return None


def panel_for(mode=None, cfg=None):
    """The (w, h) panel this boot hands the guest.

    Order: OMNI_PANEL -> config `qemu.panel` -> the mode's own `panel` ->
    DEFAULT_PANEL. An explicit request wins over the mode because "run it at
    1080p" is a statement about the product, not about the mode.

    ⚠ ASKING FOR MORE THAN THE BASE'S NATIVE MODE COSTS AND BUYS NOTHING.
    MEASURED 2026-08-15 on the x86 base (native 1280x800), `--panel 1080p`:
    the boot took **3.3+ minutes without reaching adbd** where the same image
    at 1280x800 took 0.8, QEMU stayed alive with **no scanout errors at all**,
    and when the guest finally came up **it was still 1280x800** -- `screencap`
    returned a 1280x800 image. So the guest ignores a mode its panel does not
    carry, after paying a long stall trying. Smaller-than-native panels are
    fine and are what farming uses. Raising the ceiling needs a base whose mode
    list carries the larger mode, not a bigger number here."""
    for candidate in (os.environ.get("OMNI_PANEL"),
                      ((cfg or {}).get("qemu") or {}).get("panel")):
        parsed = parse_panel(candidate)
        if parsed:
            return parsed
    declared = (mode or {}).get("panel")
    return parse_panel(declared) or tuple(declared or ()) or DEFAULT_PANEL


def display_override(cfg=None):
    """A verbatim `-display` argument from config `qemu.display` / OMNI_DISPLAY.

    An experiment hatch, and it earns its place: the difference between the
    display backends on this platform is not a matter of degree -- one of them
    renders and never scans out -- and finding that out costs a real boot each
    time. Being able to say `OMNI_DISPLAY=dbus,p2p=on,gl=on` and boot is what
    makes that a four-minute question instead of a code change.

    Whatever is given is passed straight through, so the caller owns its
    correctness; the capability probe is bypassed entirely.
    """
    for candidate in (os.environ.get("OMNI_DISPLAY"),
                      ((cfg or {}).get("qemu") or {}).get("display")):
        if candidate:
            return str(candidate).strip()
    return ""


def gpu_extra_opts(cfg=None):
    """Extra `-device virtio-gpu-gl-pci` suboptions, from config or env.

    `qemu.gpu_opts` / OMNI_GPU_OPTS, appended verbatim (e.g.
    "blob=true,hostmem=256M"). It exists because the interesting knobs on this
    device — blob resources, venus, drm_native_context — are guest-kernel and
    host-driver dependent in ways no capability probe can answer: they either
    fix the scanout or break the boot, and which one it is has to be measured
    per host. A bad value costs one boot and is undone by unsetting it.
    """
    for candidate in (os.environ.get("OMNI_GPU_OPTS"),
                      ((cfg or {}).get("qemu") or {}).get("gpu_opts")):
        if candidate:
            return str(candidate).strip().strip(",")
    return ""


def force_video_mode(cfg=None):
    """Whether to pin the guest's physical mode with a `video=` kernel arg.

    OFF by default, and that is a measurement rather than caution.

    The idea was sound: on a WINDOWED GL boot the guest takes the first mode
    the virtio GPU offers and comes up 640x480, which Android then papers over
    with a `wm size` OVERRIDE -- so the compositor renders the full 1280x800
    and SurfaceFlinger scales it down onto a quarter-size scanout. `video=
    <connector>:<mode>` is the kernel's own override for exactly that.

    MEASURED 2026-08-15, and it does not pay:
      * On the headless paths it changed NOTHING. Both `egl-headless` and plain
        `-display none` already come up at the panel size; a boot with the arg
        and a boot without it both reported `Physical size: 1280x800`.
      * On the WINDOWED GL path it HUNG THE GUEST. `-display gtk,gl=on` plus
        `video=Virtual-1:1280x800` never reached adbd -- five minutes of
        "starting QEMU", QEMU alive and `running` over QMP, the guest stuck
        before userspace. The same boot without the arg came up normally and
        joined the place.

    So it stays available (a future base, or a host where the 640x480 default
    actually bites) and stays off. Config `qemu.force_video_mode`, env
    OMNI_FORCE_VIDEO_MODE=1.
    """
    env = os.environ.get("OMNI_FORCE_VIDEO_MODE", "").strip()
    if env:
        return env not in ("0", "false", "False", "no")
    value = ((cfg or {}).get("qemu") or {}).get("force_video_mode")
    return False if value is None else bool(value)


def gl_device_arg(panel=None, cfg=None):
    w, h = panel or DEFAULT_PANEL
    arg = f"{GL_GPU_DEVICE},xres={w},yres={h}"
    extra = gpu_extra_opts(cfg)
    return f"{arg},{extra}" if extra else arg


_GL_DEVICE_ARG = gl_device_arg()
HEADLESS_GPU_ARGS = ["-device", "virtio-gpu-pci"]
HEADLESS_DISPLAY_ARGS = ["-display", "none"]

# ------------------------------------------------- headless GPU acceleration
#
# A FOURTH tier, and the one that matters for how the product actually runs.
#
# The three tiers above answer "what window can this host open", and GL was
# reachable only WITH a window (`gaming` mode). But every production instance
# is headless — no window, VNC only — so every production instance was
# rendering on llvmpipe, in software, on the CPU. Measured in-guest on the x86
# base: `ro.hardware.egl=mesa`, SurfaceFlinger on a software renderer, with
# Roblox's arm64 build already paying binary translation on top. That is the
# "why is this so slow" the hardware could not explain: a 4060 sat idle while
# the CPU drew every frame.
#
# `-display egl-headless` addresses exactly that: it gives QEMU a host GL
# context with NO window, so virglrenderer can hand the guest's GL calls to the
# real GPU. It is the ONLY tier that satisfies all three product requirements
# at once — no window on the host, a VNC framebuffer for the viewer, and the
# guest rendering on the GPU — and it is now the DEFAULT for every mode.
#
# THE VIEWER IS NOT LOST, and the belief that it was is what kept this off for
# a week. QEMU documents egl-headless as the display you pair WITH vnc/spice,
# and `ui/egl-headless.c` reads the rendered texture back into the 2D
# DisplaySurface (egl_fb_read) and then calls dpy_gfx_update — which is exactly
# what the VNC server encodes from. The black viewer that was measured came
# from a build of this file in which `-vnc` was still being appended next to a
# WINDOWED gl display; see blocks_vnc(), which is now the only thing allowed to
# drop it.
#
# Gated on the QEMU build advertising BOTH pieces, because neither is
# universal: the Windows bundle has them, and a Homebrew macOS QEMU has
# neither (no virglrenderer formula), where this degrades to the software
# path rather than failing a boot.
HEADLESS_GL_DISPLAY = "egl-headless"
HEADLESS_GL_GPU_ARGS = ["-device", _GL_DEVICE_ARG]
HEADLESS_GL_DISPLAY_ARGS = ["-display", HEADLESS_GL_DISPLAY]

# Windowing backends that can carry a real window, best first per platform.
# `vnc`, `none`, `curses`, `egl-headless` and `dbus` are deliberately absent:
# none of them opens a low-latency native window with input attached.
_WINDOW_BACKENDS = {"macos": ("cocoa",), "linux": ("gtk", "sdl"),
                    "windows": ("gtk", "sdl")}

# HOW to ask for GL, per platform. This is not a style choice — the wrong one
# does not degrade, it fails.
#
# `gl=on` means "give me desktop OpenGL". macOS DEPRECATED OpenGL in favour of
# Metal, and every macOS QEMU that can do GL at all does it through ANGLE,
# which speaks OpenGL **ES** and translates to Metal. So on macOS the option is
# `gl=es`; `gl=on`/`gl=core` either refuse outright or render upside down
# (documented by every build that ships this: knazarov/qemu-virgl and
# akihikodaki's patches both say `gl=es`, "not gl=on or gl=core"). Getting this
# wrong would have made a correctly-installed virgl QEMU look broken, and the
# obvious conclusion — "GPU acceleration does not work on macOS" — would have
# been wrong.
_GL_OPTION = {"macos": "gl=es", "linux": "gl=on", "windows": "gl=on"}

# Suboptions we set on a PRESENTED window, per backend. Not one list, because
# QEMU refuses an unknown suboption outright rather than ignoring it -- so a
# `show-menubar=off` sent to `cocoa` does not degrade, it fails the boot.
#
#   show-menubar=off      QEMU's own View/Machine menus are not our chrome
#   window-close=off      the X must not quit QEMU: the strip asks first, and a
#                         window that closes the VM by accident costs a boot
#   zoom-to-fit=on        the guest panel is fixed at the base's native mode, so
#                         the window scales rather than letterboxing
#   keep-aspect-ratio=on  and it scales UNIFORMLY. See below.
#
# WHY keep-aspect-ratio IS NAMED EXPLICITLY EVEN THOUGH QEMU DEFAULTS IT ON.
# It is the one flag in this list that decides whether the picture is
# geometrically correct, and the whole of `zoom-to-fit` funnels through it:
# ui/gtk.c's `gd_update_scale()` is
#
#     if (vc->s->keep_aspect_ratio) { scale_x = scale_y = MIN(sx, sy); }
#     else                          { scale_x = sx; scale_y = sy;      }
#
# so with it off, every window whose shape is not the guest's shape STRETCHES
# the guest -- and with it on, the guest is letterboxed inside it instead,
# undistorted at every window size. That matters most in exactly the window
# this product now shows for the whole boot: the loading animation and the
# running game are not the same resolution, so one of them is always being
# fitted into a window sized for the other. Defaults are not a contract, this
# one is unnamed in `-help` (it lives in the QAPI schema, and an unknown
# suboption is a REFUSED BOOT, so it was verified against the shipped binary
# before being added here -- accepted on QEMU 11.0.50, 2026-08-17), and a
# silent flip upstream would come back as "the picture looks squashed" with
# nothing in this repo to point at.
#
# WHY window-close=off IS BACK. It was dropped on the belief that "our patched
# QEMU asks 'Stop this instance?' on the X itself" -- there is no such build
# (see _apply_window_env), so on the binary this actually ships the X quits
# QEMU on the spot: no prompt, the guest is killed mid-frame, and a launch that
# took a minute is gone. That was survivable while the window only appeared
# once the game was already running and the user had deliberately asked for it.
# It is not survivable now that the window goes up at spawn and sits there for
# the whole boot, which is exactly the minute the user has nothing to do but
# look at it.
#
# The cost is a window whose X does nothing, and that is a real cost, taken
# deliberately: the window is a VIEW onto an instance, and the instance is
# managed from the app, which now offers Hide (put it away, keep playing) and
# Stop (power it off) as separate buttons. `--gpu window` -- the debugging
# hatch -- gets none of these flags and keeps a working X.
_WINDOW_FLAGS = {
    "gtk":   ("show-menubar=off", "zoom-to-fit=on", "keep-aspect-ratio=on",
              "window-close=off"),
    "sdl":   ("window-close=off",),
    "cocoa": ("zoom-to-fit=on",),
}


# Platforms whose window suboptions a REAL QEMU BINARY HAS ACCEPTED.
#
# Deliberately not _WINDOW_PRESENT_PLATFORMS, and deliberately narrower than
# it. Those two constants answer different questions: that one is "should this
# boot present in a window", this one is "have these suboptions been run".
# QEMU refuses an unknown suboption OUTRIGHT rather than ignoring it, so
# guessing here does not cost the chrome, it costs the BOOT -- and "a
# detection bug may cost the GPU; it may never cost a boot" is the rule this
# file is built on.
#
# Windows is the only entry because it is the only host that has run them:
# gtk with all three, measured on this hardware.
#
# LINUX is out for the reason it has always been out -- untested gtk/sdl.
# MACOS is out as of 2026-08-16 for the SAME reason and it is not
# hypothetical: today's Homebrew QEMU has no virglrenderer, so a Mac gaming
# boot lands on the SOFTWARE window tier, and with macOS in this gate that
# tier emitted `-display cocoa,zoom-to-fit=on` where it used to emit plain
# `cocoa`. cocoa has never met a real binary in this project. If it refuses
# the suboption, every Mac gaming boot fails outright rather than degrading.
# Put "macos" back the day a real Mac QEMU has accepted `zoom-to-fit=on`.
#
# NOTE the asymmetry, which is intended: macOS stays in
# _WINDOW_PRESENT_PLATFORMS. Taking it out of THAT would change which display
# a Mac gaming boot picks (an explicit `qemu.headless_gl: true` would beat the
# profile again and land on a non-presenting egl-headless context), which is a
# policy change nobody asked for and the opposite of a safe one.
_WINDOW_FLAG_PLATFORMS = ("windows",)


def window_flags(backend, policy=None):
    """Comma-joined suboptions for a presented window on `backend`.

    Two gates, and a boot has to clear both.

    PLATFORM (_WINDOW_FLAG_PLATFORMS, defined above): these suboptions are
    OUR chrome policy, not QEMU's defaults, and QEMU refuses an unknown one
    outright rather than ignoring it -- so a platform whose backend has not
    actually been run gets QEMU's bare backend/gl argv instead of a guess
    that would fail the boot rather than degrade it. See that constant for
    which platforms are out and why.

    POLICY: `--gpu window` is defined by the design spec (3d) as "always a
    visible native window, UNSTYLED -- for debugging a GL problem with none
    of this code in the path", and flags are this code. It was getting
    `window-close=off` while `_hide_window_if_wanted` deliberately leaves a
    `window` boot on screen and `view` spawns no bar for it, which left the
    debug window with an inert X, no bar offering the close prompt, and no
    way to close it at all short of `omnidroid stop`. The debugging hatch has
    to be the configuration with the least of our behaviour in it, not the
    most.

    An unrecognised backend gets "" rather than a guess: an unknown suboption
    is a refused boot, and no flag at all is merely a plainer window.
    """
    if policy == GPU_WINDOW:
        return ""
    if _platform_key() not in _WINDOW_FLAG_PLATFORMS:
        return ""
    return ",".join(_WINDOW_FLAGS.get(backend, ()))


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


_HELP_CACHE = {}


def _qemu_help_texts(tool):
    """(display_help, device_help) as reported by the resolved QEMU binary.

    Returns ("", "") on ANY failure — missing binary, timeout, non-zero exit.
    default_display reads that as "no window is possible", which degrades to
    the headless path. Asking QEMU beats hardcoding a build matrix: the same
    version is compiled with wildly different feature sets by brew, apt and
    the Windows bundle.

    MEMOISED per process: it costs two QEMU launches, the answer cannot change
    while this process runs, and every headless boot now asks (not just the
    rare `gaming` one). A host bringing up fifty farming instances would
    otherwise pay a hundred subprocess launches for one constant.
    """
    if tool in _HELP_CACHE:
        return _HELP_CACHE[tool]
    try:
        b = qemu_bin(tool)
        def _run(*args):
            return subprocess.run([b, *args], capture_output=True, text=True,
                                  timeout=10).stdout or ""
        answer = _run("-display", "help"), _run("-device", "help")
    except Exception:
        # NOT cached: a failure here can be transient (a QEMU still being
        # installed by `setup`), and caching "no GPU" for the process lifetime
        # would outlast the condition.
        return "", ""
    _HELP_CACHE[tool] = answer
    return answer


def default_display(qemu_display_help="", qemu_device_help="", has_gui=True,
                    panel=None, cfg=None, policy=None):
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

    `policy` is this boot's GPU policy, passed through to window_flags():
    `--gpu window` is specified as an UNSTYLED window and must therefore get
    none of our suboptions. It defaults to None so every caller that only
    asks "what can this host do" keeps the styled answer.
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
        gl = _GL_OPTION[_platform_key()]        # gl=es on macOS — see _GL_OPTION
        return {"available": True, "tier": "gl",
                "gpu_args": ["-device", gl_device_arg(panel, cfg)],
                "display_args": ["-display",
                                 ",".join(filter(None,
                                                 (backend, gl,
                                                  window_flags(backend,
                                                               policy))))],
                "reason": f"{backend},{gl} + {GL_GPU_DEVICE} (3D accelerated)"}
    return {"available": True, "tier": "window",
            "gpu_args": list(HEADLESS_GPU_ARGS),
            "display_args": ["-display",
                             ",".join(filter(None,
                                             (backend,
                                              window_flags(backend,
                                                           policy))))],
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
    # +aes is NOT optional on this base, and it is not a performance tweak.
    # Roblox ships arm64 only, so every instruction runs through
    # libndk_translation -- and the translator ASSERTS on a host without
    # AES-NI the first time the app touches AES, which Roblox does during
    # startup:
    #
    #   CHECK failed: HostPlatform::kHasAES
    #     libndk_translation.so AesEncode<16>
    #     Fatal signal 6 (SIGABRT) in tid (AppStartupTaskM)
    #
    # The splash appears and the process is gone about two seconds later.
    # `qemu64` is a K8-era baseline that does not carry AES, so the feature
    # has to be asked for by name. It cost nothing to leave implicit while
    # the shipped QEMU happened to include it; a different QEMU build does
    # not, and then the game dies instantly with no message anywhere in this
    # engine's own logs.
    return "qemu64,+aes"


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


# ------------------------------------------------------------- the GPU policy
#
# One setting, three answers, because the honest answer differs by host and the
# user is the only one who can make the trade when it does.
#
#   auto      (default) get the guest onto the GPU by whatever means this host
#             supports, PREFERRING no window. On a host whose egl-headless can
#             present that is headless + VNC + GPU, all three at once. On one
#             whose cannot -- Windows today -- the only GL context QEMU will
#             give is attached to a native window, so `auto` opens one, and
#             says so.
#   headless  never put a window on the screen, whatever it costs. GPU if it
#             can be had without one, software otherwise. This is the setting
#             for an unattended host, and the one to pick if a QEMU window on
#             screen is unacceptable.
#   window    always ask for the native window (the lowest-latency way to drive
#             a guest by hand; also the only configuration where a GL problem
#             is visible without any of this code in the path).
#   off       software rendering, headless. The old behaviour, kept because it
#             is the one configuration with no host-GPU dependency at all.
#
# WHY THE WINDOW AND VNC CANNOT BOTH BE HAD: QEMU refuses them together --
#
#     qemu: -vnc 127.0.0.1:12101: Display vnc is incompatible with the GL context
#
# -- for every windowed GL backend (re-verified on 11.0.50 across gtk/sdl x
# gl=on/es/core). So on a host where the window is the only GL context, a
# GPU-accelerated boot has no VNC server, and `omnidroid view` / capture /
# autocap do not work on it. `omnidroid screenshot` still does: it goes through
# adb, not VNC.
#
# The numbers behind making `auto` prefer the GPU over the viewer, measured at
# 1280x800 on PS99 with the render confirmed by screenshot at the same instant:
#
#     software (llvmpipe)       95 frames / 30.1 s  ->  3.2 fps
#     GPU (virgl, RTX 4060)    728 frames / 30.1 s  -> 24.2 fps
#     GPU (virgl, RTX 4060)   1346 frames / 30.1 s  -> 44.8 fps
#
# Two separate in-world runs for the GPU row, and the spread is real: PS99 is a
# busy server-authoritative place, so how much is streaming in at the moment of
# the sample moves the number a long way. What does not move is the ratio to
# the software row -- 7x at the low end.
GPU_AUTO, GPU_HEADLESS, GPU_WINDOW, GPU_OFF = "auto", "headless", "window", "off"
GPU_POLICIES = (GPU_AUTO, GPU_HEADLESS, GPU_WINDOW, GPU_OFF)


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
    # gaming: spend the whole host on ONE instance. Frames, resolution and
    # input latency are what matter; density and host footprint are not.
    #
    # It is HEADLESS, like everything else, and that is the change this table
    # exists to record. `gaming` used to be the one mode that asked for a
    # native QEMU window, because a window was the only way QEMU would hand
    # itself a GL context -- and without a GL context virglrenderer cannot run
    # and Mesa falls back to llvmpipe, which is the "3 fps, unplayable" this
    # project spent a week on. `-display egl-headless` gives the same GL
    # context with NO window (see the headless-GL block above), so the window
    # bought nothing that could not be had without it, and it cost the VNC
    # viewer -- QEMU refuses `-vnc` alongside a WINDOWED gl display, so every
    # GPU boot came up with no framebuffer for anyone to look at.
    #
    # No balloon: reclaiming pages out from under a running game is a stutter
    # source, and this mode is not trying to fit fifty instances in a host.
    # free-page-reporting is still attached (balloon_device is unconditional),
    # which costs nothing while nobody inflates it.
    #
    # `profile` is the POST-BOOT intent, and it is what the engine branches on
    # instead of the mode name. Two values, one per mode now:
    #   "performance"  spend host resources on one instance: no balloon, no
    #                  squeeze, native resolution, the game on the top-app
    #                  cpuset, the quality ClientAppSettings profile.
    #   "density"      spend quality on instance COUNT: squeeze, zram, balloon,
    #                  5 fps tick, 480x270.
    # Branching on the name was a real bug once: `_ensure_booted` compared the
    # RAW --mode argument, so a bare `omnidroid start` matched neither literal
    # and got NO post-boot tuning at all. The profile key survives the mode
    # cull because it is the thing the engine actually reads.
    #
    # `autoscale` says this mode should GROW to the host, up to the WHPX caps
    # in autoscale_perf().
    #
    # `panel` is the physical guest display this boot asks for. It is a mode
    # default, not a fixed constant: `--panel 1080p` / OMNI_PANEL / config
    # `qemu.panel` all override it (see panel_for).
    # `balloon_floor` / `balloon_headroom` are the MEMORY
    # GOVERNOR's three numbers, and they are a different mechanism from
    # `balloon` above -- prevention rather than reclaim. See balloon.py for the
    # measurements; the short version is that a guest offered 4096 MB touches
    # all of it within ~20 s of spawn (Android page cache), the host's cost is
    # driven by what the guest TOUCHES, and on Windows nothing that has been
    # touched is ever given back. Capping at spawn is therefore the only lever
    # that works there, and it works because the pages are never touched at all.
    #
    # Gaming keeps `balloon: None` -- the post-boot reclaim inflate is still
    # wrong here, for the reason below. The governor is not that: it never
    # takes memory the guest is using, only memory it never asked for.
    "gaming":  {"mem": 4096, "smp": 4, "balloon": None, "usb": True,
                "balloon_floor": 1024,
                "balloon_headroom": 512,
                "display": lean.NATIVE_DISPLAY, "panel": DEFAULT_PANEL,
                "gpu": GPU_AUTO,
                "profile": "performance", "autoscale": True,
                "quality": "high"},
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
    # Two balloon targets, because the safe floor depends on whether the
    # guest has zram. MEASURED 2026-08-05 (arm64, real Roblox APK):
    #   no zram   -> 1024 kills the game ("has died: fg TOP" +
    #                mem-pressure-event); 1536 holds it at 614 MB with 336 MB
    #                spare. So 1536 is the floor for an unsqueezed guest.
    #   with zram -> lz4 compresses ~3x, so the guest holds far less. The
    #                floor was then walked down on a real PRODUCTION instance
    #                (non-rooted, zram from the baked property, game running):
    #                  896 -> alive, ~200 MB RSS, ~590 MB swapped, 0 kills,
    #                         85-131 MB available, sustained 3+ min
    #                  768 -> alive, 0 kills, but only 34 MB available
    #                  640 -> the game DIES (2 mem-pressure kills)
    # Every number there was taken against the game on its LOGIN screen, which
    # is why they did not survive contact with a real place: see FARMING_*
    # below and FOOTPRINT.md for the PS99 re-measurement.
    #
    # `smp_x86` is an ARCH OVERRIDE, not a preference. smp 1 is right on arm,
    # where the guest runs Roblox's own arm64 build natively. On the x86 base
    # every instruction of that same build goes through libndk_translation, and
    # one vCPU is not enough to get through startup: MEASURED 2026-08-15 on
    # PS99, a farming boot reached boot_completed fine and then the ordered
    # `am broadcast` that hands over the session did not return within 45 s,
    # taking the whole launch down with it. Two vCPUs is the cheapest thing
    # that makes an x86 farming instance actually reach the game, and a joined
    # idle instance goes back to using almost none of the second one.
    #
    # IT IS 3 NOW, NOT 2, AND THE THIRD ONE IS NOT FOR THE GAME. Reaching the
    # world turned out not to be the only bar: the in-guest EXECUTOR (the
    # patched Arceus APK every production offset ships) has its own startup
    # chain, and at smp 2 it never finishes it -- so the OMNI-EXEC menu never
    # appears and no auto-exec ever runs, on an instance that otherwise looks
    # perfect. Farming was the only mode with the bug; gaming has 4 vCPUs.
    #
    # MEASURED 2026-08-17, PS99, x86 base, by packet-capturing the executor's
    # own HTTP chain inside the guest (`tcpdump host <exec server>`), which is
    # what made this legible at all -- the chain is silent in logcat and in
    # Roblox's client log, so from outside it is indistinguishable from "the
    # executor is not installed". The chain is 11 font fetches, then
    # `Costumers/arceus.lua`, then `/gist` (the menu), then `/omni/exec/claim`
    # and a 1 Hz poll. Same account, same place, same offset:
    #
    #   smp 2  3 fonts in 11 s, then NOTHING, ever -- capture ran for the whole
    #          session and saw not one more packet. No arceus.lua, no /gist, no
    #          menu. (The stall is 85 s BEFORE the squeeze runs, so the squeeze
    #          is not what does it.)
    #   smp 3  all 11 fonts, arceus.lua, /gist, claim, 53 polls -- menu on
    #          screen, verified by screenshot.
    #   smp 4  same, ~5 s sooner. Not worth a fourth vCPU across a fleet.
    #
    # WHAT IT COSTS, which is less than it looks: `cpu_ceiling_pct` below caps
    # the whole QEMU process at 50% of ONE core once the client has loaded, and
    # that cap is per-PROCESS, not per-vCPU. So the third vCPU is spent where
    # the starvation actually was -- startup, while the executor's chain races
    # the place load -- and buys nothing extra at steady state, which is the
    # state a farming fleet spends its life in. The arm base runs Roblox
    # NATIVELY and needs none of this; it keeps smp 1.
    #
    # `swappiness_x86` is the second arch override, and like the first it is a
    # REQUIREMENT rather than a preference.
    #
    # MEASURED 2026-08-15: a farming instance on the x86 base joins PS99 and
    # then Roblox ABORTS, with plenty of memory free and no OOM kill:
    #
    #   F libc  : Fatal signal 6 (SIGABRT) ... pid (Main), tid (Thread-19)
    #   F DEBUG : Abort message: 'Cannot process signal 11'
    #   F DEBUG : #04 libndk_translation.so (HandleHostSignal(int, siginfo*, ...))
    #
    # That is the TRANSLATOR aborting: Roblox ships arm64 only, the x86 base
    # runs it through libndk_translation, translated code took a SIGSEGV, and
    # the translator's host-signal handler could not process a fault arriving
    # in translated context. Farming is the only mode that swaps hard --
    # `swappiness 100`, `page-cluster 0`, zram on -- and evicting translated
    # code pages is exactly how you manufacture that fault. Gaming, which runs
    # at swappiness 10 with no zram, has never crashed this way.
    #
    # SWAPPINESS ALONE IS THE FIX, and turning zram off as well was measured
    # to be actively worse. Two runs, everything else identical:
    #
    #   swappiness 10, zram ON    aborts 0, guest 830 MB / 315 MB free,
    #                             client reached Roblox's loading screen
    #   swappiness 10, zram OFF   aborts 0, guest 1485 MB / 648 MB free, and
    #                             Roblox was OOM-KILLED over and over
    #                             ("has died: fg TOP" x3 + "mem-pressure-event")
    #
    # zram is not what breaks the translator -- swapping HARD is. With lz4
    # compressing ~3x, zram is also the only reason a 2 GB guest holds this
    # game at all, so removing it traded a crash for a different crash. Keep
    # it; just stop being eager about using it.
    #
    # The arm base runs Roblox NATIVELY, has no translator to upset, and keeps
    # the aggressive setting.
    # The governor's floor here is the ZRAM figure (896), not the 1536 above,
    # and that is safe for a reason the fixed targets could not rely on: the
    # floor is a BACKSTOP, not a target. The governor only ever descends
    # towards `used + headroom`, so a guest whose game genuinely needs 1.4 GB
    # is never taken to 896 whatever the floor says. That is the advantage of
    # sizing against measured demand instead of a constant chosen in advance --
    # the constant has to be right for the worst case, the governor does not.
    "farming": {"mem": 2048, "smp": 1, "smp_x86": 3,
                "balloon": 1536, "balloon_zram": 896,
                "balloon_floor": 896,
                "balloon_headroom": 384,
                "swappiness": 100, "swappiness_x86": 10,
                "zram": True,
                "usb": True, "display": lean.FARMING_DISPLAY,
                # GPU_AUTO, not GPU_HEADLESS, and this is the single biggest
                # density lever measured on this project.
                #
                # MEASURED 2026-08-15, PS99, in-world, farming, per-THREAD out
                # of /proc/<pid>/task/*/stat over 20 s:
                #
                #   software (GPU_HEADLESS)          GPU (hidden GL window)
                #   ------------------------------   ----------------------
                #   llvmpipe-1      52.8%            (gone)
                #   llvmpipe-0      51.8%            (gone)
                #    RBX Worker A    6.8%             RBX Worker A   17.3%
                #   ------------------------------   ----------------------
                #   TOTAL          141.1%            TOTAL          72.3%
                #
                # THREE QUARTERS of a software farming instance's CPU is
                # llvmpipe rasterising frames nobody looks at. Moving that to
                # the host GPU halves the per-instance cost, which is the
                # thing that decides how many instances a host can hold --
                # and it is why the fps cap and the smaller panel both
                # measured as nothing: they throttle Roblox's scheduler and
                # its pixel count, not the software rasteriser's per-frame
                # work.
                #
                # `auto` and not `window`: an explicit `window` request means
                # "I want to see it", so _hide_window_if_wanted deliberately
                # leaves it on screen. `auto` opens the window only because
                # this host has no other route to a GL context, then HIDES it
                # -- verified, the guest keeps rendering while invisible.
                #
                # It degrades correctly rather than bravely: on a host with no
                # window server (a real headless farm box) `auto` finds no
                # display and falls back to software, which is exactly the old
                # behaviour. The cost where it does engage is VNC -- QEMU
                # refuses `-vnc` beside a GL context -- so `capture`/`autocap`
                # are unavailable and `omnidroid view` restyles and shows this
                # same window instead. `screenshot` goes through adb and is
                # unaffected, which is what farming actually needs.
                "panel": FARMING_PANEL, "gpu": GPU_AUTO,
                # How little of `-m` the HOST has to keep resident. The
                # governor walks a working-set ceiling down from `mem` while
                # the guest stays healthy and stops clear of anything that
                # hurt it, so this is how far it is ALLOWED to go, not where
                # it lands. MEASURED on PS99 at `-m 2048`, 2026-08-17: the
                # search walked all the way here and HELD, client in-world for
                # 15 minutes, adb 0.10 s, 780-810 MB still available inside the
                # guest -- 384 MB against 3417 MB uncapped, 8.9x. 300 MB is
                # where the client dies. (An earlier pass recorded 384 as fatal
                # and this comment claimed the search settles at 640-760; both
                # were wrong, and runtime.cap_working_set carries the retraction
                # next to its table.) A different game finds its own number,
                # which is the point of searching rather than naming one.
                # See balloon.next_ceiling and runtime.cap_working_set.
                "ws_floor": 384,
                # ...and how much CPU one farming instance may take, as a
                # share of ONE core. This is the lever for instance COUNT:
                # uncapped, a PS99 farming instance takes 161% of a core, so
                # 24 logical processors hold about 15 of them. At 50% they
                # hold about 40, and MEASURED at that cap the client is still
                # alive and adb still answers in 0.06 s. Rendering is not what
                # is being cut -- SurfaceFlinger is 6.7% of a guest core
                # against the client's 148% -- the game simply runs at the
                # pace it is given. See runtime.CpuCeiling.
                "cpu_ceiling_pct": 50,
                "profile": "density", "quality": "low"},
}
DEFAULT_MODE = "gaming"

# The mode list used to have five entries: playable (the default), gaming,
# hard, brutal and farming. Three of them are gone, and nothing was lost with
# them:
#
#   playable  was gaming with `window: False`. Once gaming stopped opening a
#             window there was no difference left to name.
#   hard      3072 MB / 4 vCPU, and
#   brutal    2048 MB / 2 vCPU — two fixed "give this instance less" tiers
#             that predate `--mem` / `--smp` being honoured properly. They are
#             `--mem 3072` and `--mem 2048 --smp 2`, which is the same request
#             said in the flag that already exists.
#
# They stay ACCEPTED as aliases rather than becoming argparse errors, because
# an installed app is a client of this CLI: omni-executor persists the chosen
# mode in its settings and 1.0.14 ships `"mode": "playable"` as its default, so
# rejecting the name would break every launch from an app that has not been
# updated yet. An alias resolves silently; nothing downstream ever sees the old
# name (`mode["name"]` is the resolved one), so run.json, the warm-cache key
# and the UI all agree on the two real modes.
MODE_ALIASES = {"playable": "gaming", "hard": "gaming", "brutal": "gaming"}


def resolve_mode_name(name):
    """The canonical mode name for whatever a caller typed.

    None/empty -> DEFAULT_MODE; a legacy name -> its replacement; anything
    else comes back unchanged so the caller can report an honest "unknown
    mode" against the real list."""
    if not name:
        return DEFAULT_MODE
    key = str(name).strip().lower()
    return MODE_ALIASES.get(key, key)


# Every name `--mode` will accept: the real modes first, then the aliases.
# Kept in this order so `--help` shows the two that exist before the three
# that are only tolerated.
MODE_CHOICES = list(MODES) + list(MODE_ALIASES)


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


def parse_guest_display(text):
    """(w, h, dpi) from "480x270", "480x270x80" or "native"/"off" -> None.

    The GUEST display is not the same thing as the PANEL: the panel is the
    virtio-gpu device's mode (what the hardware advertises), this is what
    Android is told to lay out at with `wm size`/`wm density`. Farming shrinks
    the second one hard -- 480x270 at 80 dpi -- because it is the single
    cheapest way to make the whole compositing pipeline small.

    Exposed so that "is the postage-stamp display what stops Roblox loading?"
    is a question you can answer with a flag instead of an edit. Returns
    ("native",) sentinel handling to the caller: None means leave it alone.
    """
    if text is None:
        return "unset"
    s = str(text).strip().lower()
    if s in ("native", "off", "none", "reset"):
        return None
    parts = s.replace("*", "x").split("x")
    try:
        if len(parts) == 2:
            return (int(parts[0]), int(parts[1]), 80)
        if len(parts) == 3:
            return (int(parts[0]), int(parts[1]), int(parts[2]))
    except ValueError:
        pass
    return "unset"


def resolve_mode(cfg, name=None, mem=None, balloon=None, smp=None,
                 host=None, arch=None, guest_display="unset"):
    """The resolved mode dict for one boot.

    Order is load-bearing: AUTOSCALE FIRST, explicit flags second, so an
    explicit `--mem`/`--smp` always wins outright over the host-derived size.
    `host` is an injectable (mem_mb, cpus) pair for tests; None probes.

    A legacy mode name (playable/hard/brutal) resolves to its replacement HERE,
    once, so everything downstream — run.json, the warm-cache key, the tuning
    branch, the UI — sees the canonical name and cannot disagree about which
    mode this boot is.

    `arch` ("x86" | "arm") lets a mode declare a per-architecture override.
    Only farming uses one, and it is not a preference: see `smp_x86`."""
    resolved = resolve_mode_name(name)
    if resolved not in MODES:
        raise KeyError(f"unknown mode {name!r}; known modes: "
                       f"{', '.join(MODES)}")
    if name and resolved != str(name).strip().lower():
        print(f"[mode] '{name}' is a retired mode name; running "
              f"'{resolved}'. Known modes: {', '.join(MODES)}")
    m = dict(MODES[resolved])
    m["name"] = resolved
    # Per-arch overrides, applied BEFORE autoscale and before the explicit
    # flags, so both still win over them in the usual order. ANY key can carry
    # one as `<key>_<arch>`; the arch-suffixed keys are then stripped so
    # nothing downstream has to know the mechanism exists.
    if arch:
        for key in [k for k in list(m) if k.endswith(f"_{arch}")]:
            m[key[: -len(arch) - 1]] = m[key]
        for junk in [k for k in list(m)
                     if k.rsplit("_", 1)[-1] in ("x86", "arm")]:
            m.pop(junk, None)
    if m.get("autoscale"):
        host_mem, host_cpus = host if host is not None else host_capacity()
        m = autoscale_perf(m, host_mem, host_cpus)
        m["name"] = resolved
    if smp:
        m["smp"] = smp
    if mem:
        m["mem"] = mem
    if guest_display != "unset":
        # None is a MEANINGFUL value here ("leave the base's own resolution
        # alone"), which is why the no-op sentinel is the string rather than
        # None -- the same trap `--balloon 0` documents below.
        m["display"] = guest_display
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
        # ...and the same rule against the HOST-capability skip:
        # apply_balloon_target declines to inflate on a host that cannot take
        # the pages back (Windows), which is right for a mode's own default
        # and wrong for something a human typed. Recorded here because that is
        # where "the user asked for this" is still known.
        m["balloon_explicit"] = True
    return m


def headless_gl_capability(qemu_display_help="", qemu_device_help="",
                           panel=None, cfg=None):
    """Can this QEMU render a HEADLESS guest on the host GPU?

    Pure, like default_display: every host fact is an argument. Needs the
    virgl-backed GPU model AND the windowless GL display backend -- either one
    alone is useless.
    """
    has_gpu = GL_GPU_DEVICE in qemu_device_help
    has_display = HEADLESS_GL_DISPLAY in qemu_display_help
    if has_gpu and has_display:
        w, h = panel or DEFAULT_PANEL
        return {"available": True,
                "gpu_args": ["-device", gl_device_arg((w, h), cfg)],
                "display_args": list(HEADLESS_GL_DISPLAY_ARGS),
                "reason": f"{HEADLESS_GL_DISPLAY} + {GL_GPU_DEVICE} "
                          f"at {w}x{h} (host GPU, no window)"}
    missing = []
    if not has_gpu:
        missing.append(GL_GPU_DEVICE)
    if not has_display:
        missing.append(f"-display {HEADLESS_GL_DISPLAY}")
    return {"available": False, "gpu_args": [], "display_args": [],
            "reason": f"this QEMU build has no {' and no '.join(missing)} "
                      f"(built without virglrenderer/OpenGL) -- rendering "
                      f"stays on the CPU"}


# Can `-display egl-headless` on THIS PLATFORM actually put the guest's
# scanout on screen? Not "does QEMU accept the flag" -- it accepts it
# everywhere -- but "does a frame ever come out".
#
# MEASURED 2026-08-15, QEMU 11.0.50 on Windows 11 / RTX 4060, x86 Bliss guest,
# three separate boots (plain, blob=true+hostmem=512M, and with the forced
# `video=` mode removed). Every one of them:
#
#     dmesg:  [drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1203
#                                                        (command 0x103)
#     dumpsys SurfaceFlinger --timestats:   totalFrames = 0
#     adb exec-out screencap:               solid black
#     VNC framebuffer:                      1 update, mean brightness 0.0
#
# 0x103 is VIRTIO_GPU_CMD_SET_SCANOUT and 0x1203 is
# VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: QEMU is refusing to scan out the
# buffer the guest hands it. The guest's GL itself came up fine -- SurfaceFlinger
# reported `GLES: Mesa, virgl (ANGLE (NVIDIA ... RTX 4060)), OpenGL ES 2.0` and
# logcat had no GL errors at all -- so this is a PRESENTATION failure, not a
# rendering one. The likely cause is that egl-headless advertises dmabuf
# support, the guest allocates its scanout accordingly, and Windows has no
# dmabuf for QEMU to import; but the cause matters less than the measurement.
#
# So on Windows, headless GL renders into a hole. The honest options there are
# a native window (GPU, no VNC -- QEMU refuses the pair) or headless VNC on
# llvmpipe. Linux is the platform egl-headless was written for and where it
# pairs with VNC as documented; macOS is unknown and is marked False rather
# than assumed, since a wrong True costs a black screen and a wrong False
# costs nothing but a config flag.
#
# Override with config `qemu.headless_gl` / OMNI_HEADLESS_GL=1 -- re-measure
# with `dumpsys SurfaceFlinger --timestats -dump | grep totalFrames` before
# believing any change here.
HEADLESS_GL_PRESENTS = {"windows": False, "linux": True, "macos": False}


def headless_gl_presents():
    """Whether this platform's egl-headless is known to actually present."""
    return HEADLESS_GL_PRESENTS.get(_platform_key(), False)


def _headless_gl_wanted(cfg):
    """Config `qemu.headless_gl`, overridable by OMNI_HEADLESS_GL.

    The DEFAULT is per-platform (see HEADLESS_GL_PRESENTS), not a constant,
    because the answer genuinely differs: on Linux this is the whole point of
    the display backend, and on Windows it produces a guest that renders
    perfectly and shows nothing.

    An explicit setting always wins, in either direction. Forcing it ON on
    Windows is a supported thing to do -- it is how the measurement above gets
    re-taken on a newer QEMU -- and it is why this stayed a flag rather than
    becoming a hardcoded platform branch.
    """
    env = os.environ.get("OMNI_HEADLESS_GL", "").strip()
    if env:
        return env not in ("0", "false", "False", "no")
    value = ((cfg or {}).get("qemu") or {}).get("headless_gl")
    return headless_gl_presents() if value is None else bool(value)




def gpu_policy(cfg=None, mode=None):
    """This boot's GPU policy: env OMNI_GPU -> config `qemu.gpu` -> the mode's
    own -> auto.

    The MODE gets a say because the two modes want opposite things from the
    same trade. `gaming` is one instance somebody is playing, so it takes the
    GPU even where that costs a window. `farming` is fifty instances nobody is
    watching: its tick is capped at 5 fps and its render quality is the floor,
    so the GPU buys it almost nothing -- and fifty QEMU windows is not a
    product. Farming therefore defaults to `headless`, which is exactly what it
    has always done, and still picks up windowless GPU rendering for free on a
    host that can present it.

    An unrecognised value falls back rather than failing a boot; the caller
    (cmd_start) validates an explicitly-typed one up front, where a typo can
    still be reported.
    """
    for candidate in (os.environ.get("OMNI_GPU"),
                      ((cfg or {}).get("qemu") or {}).get("gpu"),
                      (mode or {}).get("gpu")):
        value = str(candidate or "").strip().lower()
        if value in GPU_POLICIES:
            return value
    # OMNI_GL_WINDOW predates this setting and is kept as an alias, so the B2
    # runbook one-liner still means something.
    if _gl_window_requested():
        return GPU_WINDOW
    return GPU_AUTO


# Platforms where the performance profile PRESENTS in a host window.
#
# Linux is absent ON PURPOSE and only until a Linux host exists to verify it
# (2026-08-16). It is the one platform whose egl-headless really presents, so
# gaming works there today; switching it blind would trade a working copy path
# for an unrun one and drop the VNC server with it, because QEMU refuses -vnc
# beside a GL window. macOS is present and needs no gate: its egl-headless
# does NOT present (HEADLESS_GL_PRESENTS), so _headless_gl_pair returns None
# there and the window path is reached anyway.
#
# NOT the same list as _WINDOW_FLAG_PLATFORMS, which gates the window
# SUBOPTIONS. "Should this boot present in a window" and "has a real binary
# accepted these suboptions" are different questions with different costs for
# getting them wrong: the first costs the GPU, the second costs the boot. See
# _WINDOW_FLAG_PLATFORMS for why macOS answers yes here and no there.
_WINDOW_PRESENT_PLATFORMS = ("windows", "macos")


def _presents_a_window(policy, mode):
    """Whether this boot should PRESENT the guest in a host window.

    The two profiles want opposite things from the same trade and this is the
    one line that says so. `performance` is one instance somebody is playing:
    a window is zero copies and native input, and it costs only the VNC server
    nobody was watching. `density` is many instances nobody is watching: it
    wants the GPU without a window, which is what the windowless pair gives.

    MEASURED, and the reason this predicate exists at all: on Linux
    `egl-headless` presents, so `auto` resolved there for BOTH profiles and
    gaming paid a GPU readback, an RFB encode and a Python RFB decode on every
    frame while rendering on the GPU the whole time.
    """
    if policy == GPU_WINDOW:
        return True
    if policy != GPU_AUTO:
        return False
    return ((mode or {}).get("profile") == "performance"
            and _platform_key() in _WINDOW_PRESENT_PLATFORMS)


def resolve_gpu_display(mode, interactive, tool, cfg=None):
    """The (gpu_args, display_args) pair for one boot, host-checked.

    Pure policy on top of two capability probes; never raises, and every path
    degrades to the byte-for-byte headless software pair rather than to an
    argv QEMU would refuse. A detection bug can cost the GPU; it cannot cost a
    boot.

    An interactive builder boot ignores the policy entirely: it exists to
    mutate an image, not to draw, and it must behave identically on every host.
    """
    if interactive:
        return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)

    override = display_override(cfg)
    if override:
        # The GPU device follows the display: a `gl=`/egl/dbus-gl display with
        # the plain virtio-gpu behind it is a boot with no acceleration at all,
        # which is never what someone typing this is testing for.
        wants_gl = uses_gl_context(["-display", override]) or "gl=" in override
        gpu = (["-device", gl_device_arg(panel_for(mode, cfg), cfg)]
               if wants_gl else list(HEADLESS_GPU_ARGS))
        print(f"[gpu] OMNI_DISPLAY override: -display {override} "
              f"({'GL' if wants_gl else 'no GL'})")
        return gpu, ["-display", override]

    policy = gpu_policy(cfg, mode)
    # --no-window / OMNI_NO_WINDOW / config qemu.no_window is absolute: it
    # means "nothing on my screen", so it can only ever narrow the policy.
    if window_suppressed(cfg) and policy in (GPU_AUTO, GPU_WINDOW):
        if policy == GPU_WINDOW:
            print("[gpu] --no-window overrides the `window` GPU policy.")
        policy = GPU_HEADLESS

    if policy == GPU_OFF:
        return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)

    if policy == GPU_HEADLESS or (policy == GPU_AUTO
                                  and not _presents_a_window(policy, mode)):
        pair = _headless_gl_pair(tool, cfg, mode)
        if pair is not None:
            return pair
        if policy == GPU_HEADLESS:
            print("[gpu] headless: this host cannot render a windowless guest "
                  "on the GPU, so it renders in SOFTWARE. The VNC viewer "
                  "works. Use --gpu window for the GPU (it costs the viewer "
                  "-- QEMU refuses -vnc beside a GL window).")
            return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)

    # auto with no headless GL, or an explicit window request.
    cap = default_display(*_qemu_help_texts(tool), has_gui=_host_has_gui(),
                          panel=panel_for(mode, cfg), cfg=cfg, policy=policy)
    if not cap.get("available"):
        print(f"[gpu] no host window available ({cap.get('reason')}); "
              f"booting headless in software")
        return list(HEADLESS_GPU_ARGS), list(HEADLESS_DISPLAY_ARGS)
    if cap.get("tier") == "gl":
        # WHAT HAPPENS TO THAT WINDOW is decided a few lines later, by
        # place_window, and this line has to agree with it or it is the most
        # misleading output in the launch: it is the one message a user reads
        # while waiting, and it used to promise a window "hidden during boot"
        # on the very boots that now show it for exactly that.
        watched = window_shown_at_spawn(cfg, mode)
        fate = ("the window goes up now and stays up, so you can watch the "
                "boot; hide it any time with `omnidroid view <name> --hide`"
                if watched else
                "the window is hidden, and `omnidroid view` shows it when you "
                "want it")
        print(f"[gpu] {cap['reason']}. This host can only take a GL context "
              f"through a window, so QEMU serves no VNC on this boot — "
              f"{fate} (GPU-rendered, native input, no copy). "
              f"`screenshot` works either way; capture/autocap do not. "
              f"--gpu headless keeps VNC and gives up the GPU.")
    else:
        print(f"[gpu] {cap['reason']}")
    return gpu_display_args(True, cap)


def _headless_gl_pair(tool, cfg, mode):
    """The windowless GPU pair, or None when this host cannot present one."""
    if not _headless_gl_wanted(cfg):
        return None
    cap = headless_gl_capability(*_qemu_help_texts(tool),
                                 panel=panel_for(mode, cfg), cfg=cfg)
    if not cap.get("available"):
        print(f"[gpu] no headless GPU acceleration: {cap['reason']}")
        return None
    print(f"[gpu] headless GPU acceleration: {cap['reason']} — VNC viewer "
          f"stays available")
    return list(cap["gpu_args"]), list(cap["display_args"])


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
    macOS runs the 2-3 playable instances and does not pretend otherwise.

    ON WINDOWS free-page-reporting is not merely advisory, it is NEGATIVE, and
    it is dropped there. QEMU has no madvise on Windows, so every page the
    guest reports free fails `ram_block_discard_range` and QEMU logs a line
    about it. MEASURED 2026-08-15 on a normal playable boot: 925 such lines in
    ~60 s of runtime, 78 KB of qemu.log, and one failed discard attempt per
    4 MB block for the life of the instance -- all of it buying exactly zero
    reclaimed memory, since the discard IS the reclaim. The balloon device
    itself stays, so `apply_balloon_target`'s QMP inflate still exists."""
    if IS_WINDOWS:
        return ["-device", "virtio-balloon-pci,id=omniball"]
    return ["-device", "virtio-balloon-pci,free-page-reporting=on,id=omniball"]


# ---------------------------------------------------------------- guest MTU

def nic_mtu_suffix(cfg=None, label=None):
    """`,host_mtu=N` for the guest NIC when this host's egress is smaller than
    a standard Ethernet frame, else "".

    virtio has a feature bit for exactly this (`VIRTIO_NET_F_MTU`), so the
    guest kernel brings `eth0` up at N by itself -- nothing has to run inside
    the guest, and it is right from the first packet rather than after a
    post-boot fix-up.

    WHY IT MATTERS HERE: QEMU's user networking gives the guest 1500 and then
    sends its packets out through the HOST's stack. Behind a VPN that stack is
    smaller. TCP survives (MSS negotiation); **UDP does not**, and Roblox's
    gameplay traffic is UDP. MEASURED 2026-08-15 on this host: ProtonVPN's IP
    interface is 1420, the guest was at 1500, and PS99 connected to a real
    game server ("Connection accepted from 128.116.13.34") and then dropped
    with "Disconnected (Error Code: 277)" every time the world started
    streaming. See omnidroid/netmtu.py.
    """
    from omnidroid import netmtu
    mtu, why = netmtu.guest_mtu(cfg)
    if mtu >= netmtu.DEFAULT_MTU:
        return ""
    if label:
        print(f"[{label}] guest MTU {mtu} (from {why}) — this host's path to "
              f"the internet cannot carry a full 1500-byte frame, and UDP "
              f"has no way to find that out for itself")
    return f",host_mtu={mtu}"


def smp_arg(smp):
    """The -smp string, with an EXPLICIT one-socket topology.

    A bare `-smp 4` leaves QEMU to factor the count itself, and it factors it
    into 4 SOCKETS of 1 core -- four single-core packages, a shape no physical
    phone or PC has. Android's scheduler builds its cpusets and its energy
    model around cores that share a package and treats separate sockets as
    separate scheduling domains, so the topology is not cosmetic. Naming
    sockets=1,cores=N,threads=1 hands the guest the machine it expects.

    NOT a throughput claim: measured on the Windows host it was inside the
    run-to-run noise on frame rate. It is here because the guest's view of its
    own topology should be true, and because the cpuset tuning in gaming.py
    reasons about exactly that.
    """
    n = max(1, int(smp))
    return f"{n},sockets=1,cores={n},threads=1"


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
                                                 "qemu-system-aarch64", cfg)
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
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on,cache=unsafe"
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
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on,cache=unsafe"
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
        "-smp", smp_arg(smp),
        *mem_args(mode, mem),
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
        *vnc_args(display_args, vnc_display),
        *usb_devices(mode, arm=True),
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", "virtio-net-pci,netdev=net0" + nic_mtu_suffix(cfg),
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
        disk_opts = ",format=qcow2,if=virtio,snapshot=on,cache=unsafe"
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
        #
        # `cache=unsafe` goes with `snapshot=on` and ONLY with it. It makes the
        # guest's flushes no-ops, which is normally how you lose a filesystem
        # to a host crash — but the thing being flushed here is the throwaway
        # overlay QEMU deletes when the process exits. There is no state to
        # lose, so honouring a durability barrier for it is pure cost: an
        # Android boot fsyncs constantly (packages, dalvik cache, logs) and
        # every one of those was hitting the host disk to protect data that
        # was already guaranteed to be discarded. The persistent branch below
        # keeps QEMU's default caching, because there it is real data.
        sys_src = images / base["disk"]
        data_src = images / (acct.get("data_image") or cfg["data_template"])
        disk_opts = ",format=qcow2,if=virtio,snapshot=on,cache=unsafe"
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
                                                 "qemu-system-x86_64", cfg)
    if interactive:
        # Interactive/builder boot: serial console log for debugging (headless
        # like everything else; virtio-vga kept so the guest has its usual DRM
        # device during provisioning/builder sessions).
        append += " console=tty0 console=ttyS0,115200"
        gpu = ["-device", "virtio-vga"]
        nic = "virtio-net-pci,netdev=net0" + nic_mtu_suffix(cfg)
    else:
        # Production silent boot (no firmware/console text).
        append += (" quiet loglevel=0 console=null "
                   "vt.global_cursor_default=0 SETUPWIZARD=0")
        nic = ("virtio-net-pci,netdev=net0,romfile="   # no iPXE option ROM
               + nic_mtu_suffix(cfg))
        smp = mode["smp"]
        mem = mode["mem"]
        # -vga none first: the emulated VGA adapter is dead weight next to the
        # virtio GPU, in every tier.
        gpu = ["-vga", "none"] + gpu_args
        # FORCE THE PHYSICAL MODE, because xres/yres on the device is only a
        # request. MEASURED 2026-08-15 on a GL boot that asked for 1280x800:
        #
        #     $ adb shell wm size
        #     Physical size: 640x480
        #     Override size: 1280x800
        #
        # The device's xres/yres set the PREFERRED mode in the EDID, but the
        # guest kernel took the first entry in the mode list anyway, so the
        # real scanout was 640x480 and Android was compositing 1280x800 and
        # letting SurfaceFlinger scale it DOWN to fit. That is the worst of
        # both: the full pixel cost of the big surface and the sharpness of the
        # small one. `video=<connector>:<mode>` is the kernel's own override
        # and settles it before DRM ever reads the EDID. virtio-gpu's connector
        # is `Virtual-1`; an unknown connector name is silently ignored by the
        # kernel, so this cannot cost a boot on a base that names it otherwise.
        if force_video_mode(cfg):
            pw, ph = panel_for(mode, cfg)
            append += f" video=Virtual-1:{pw}x{ph}"

    cmd = [
        qemu_bin("qemu-system-x86_64"),
        "-machine", machine_arg(accel),
        "-cpu", x86_cpu_model(accel, cfg),
        "-smp", smp_arg(smp),
        *mem_args(mode, mem),
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
        *vnc_args(display_args, vnc_display),
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


# Display backends that render WITHOUT putting a window on screen. `egl-headless`
# is the subtle one: it takes a real host GL context (that is the whole point)
# but shows nothing. Reading it as "a window opened" would make `start` stand
# its VNC viewer down, leaving the user with no way to see the instance at all.
_WINDOWLESS_DISPLAYS = ("none", "", HEADLESS_GL_DISPLAY, "egl-headless")


def uses_gl_context(display_args):
    """Does this display pair give QEMU a GL context?

    Any `gl=` that is not `off` on a windowed backend, or egl-headless.

    Matching `gl=` generally rather than the literal `gl=on` is load-bearing:
    macOS takes `gl=es` (ANGLE -> Metal; see _GL_OPTION), and a check that only
    knew `gl=on` would read a macOS GL boot as non-GL.
    """
    for arg in display_args or []:
        text = str(arg)
        if text.split(",")[0] == HEADLESS_GL_DISPLAY:
            return True
        for opt in text.split(",")[1:]:
            if opt.startswith("gl=") and opt != "gl=off":
                return True
    return False


def blocks_vnc(display_args):
    """Would QEMU refuse `-vnc` next to this display?

    This is NOT the same question as uses_gl_context(), and conflating the two
    cost this project its viewer on every GPU-accelerated boot.

    QEMU refuses the combination for a WINDOWED display that has taken a GL
    context:

        qemu: -vnc 127.0.0.1:12101: Display vnc is incompatible with the GL context

    It does not refuse `egl-headless`. That display exists precisely to be
    paired with vnc/spice -- QEMU's own manual says so ("this display needs to
    be paired with either VNC or SPICE displays") and `ui/egl-headless.c` reads
    the rendered texture back into the 2D surface (egl_fb_read) and then calls
    dpy_gfx_update, which is the surface the VNC server encodes. VERIFIED on
    this host by starting QEMU with both and watching the framebuffer.

    So exactly one case drops `-vnc`: a windowed backend with `gl=` on. That
    only happens under OMNI_GL_WINDOW=1 now, and it is the one boot where
    losing the VNC server does not matter -- the window IS the viewer.
    """
    for arg in display_args or []:
        text = str(arg)
        parts = text.split(",")
        if parts[0] in _WINDOWLESS_DISPLAYS:
            continue                       # none / egl-headless: VNC is fine
        for opt in parts[1:]:
            if opt.startswith("gl=") and opt != "gl=off":
                return True
    return False


def vnc_args(display_args, vnc_display):
    """The `-vnc` pair, dropped only for a windowed GL boot (see blocks_vnc).

    What a windowed GL boot gives up: `omnidroid view`, and capture.py/autocap,
    which attach to this framebuffer. `omnidroid screenshot` is unaffected --
    it goes through adb, not VNC. Every headless boot -- which is every boot
    the product makes -- keeps the server, GPU-accelerated or not.
    """
    if blocks_vnc(display_args):
        return []
    return ["-vnc", f"127.0.0.1:{vnc_display}"]


def command_renders_on_gpu(cmd):
    """True when this QEMU command hands the guest a virgl-backed GPU.

    Read off the argv for the same reason as command_opens_a_window: the
    command IS what the process will do, so a second copy of the capability
    logic cannot drift from it. What it decides downstream is not cosmetic —
    the Roblox quality profile and the guest panel size are both wrong answers
    when rendering turns out to be software (see engine._ensure_booted)."""
    return any(GL_GPU_DEVICE in str(a) for a in cmd)


def gl_panel_size(cmd):
    """(w, h) the GL GPU device was told to advertise, or None.

    Parsed back out of the argv rather than assumed from the constants,
    because a config or a future mode may override the device string and the
    guest must be resized to what was ACTUALLY asked for."""
    for arg in cmd:
        text = str(arg)
        if not text.startswith(GL_GPU_DEVICE):
            continue
        opts = dict(p.split("=", 1) for p in text.split(",")[1:] if "=" in p)
        try:
            return int(opts["xres"]), int(opts["yres"])
        except (KeyError, ValueError):
            return None
    return None


def command_opens_a_window(cmd):
    """True when this QEMU command will put a window on the host's screen.

    Read off the command actually being handed to QEMU rather than re-running
    the decision: the argv IS what the process will do, so this cannot drift
    from it the way a second copy of the capability logic would."""
    for i, arg in enumerate(cmd):
        if arg == "-display" and i + 1 < len(cmd):
            return cmd[i + 1].split(",")[0] not in _WINDOWLESS_DISPLAYS
    return False


def display_kind(cmd):
    """What this boot put on screen, read off the argv it was spawned with.

    Written into run.json so `view` never re-derives the policy: the argv IS
    what the process did, and a second copy of the decision is a second thing
    that can drift.
    """
    display = []
    for i, arg in enumerate(cmd):
        if arg == "-display" and i + 1 < len(cmd):
            display = [cmd[i + 1]]
            break
    if not command_opens_a_window(["-display"] + display):
        return "vnc" if "-vnc" in cmd else "none"
    return "gl-window" if uses_gl_context(display) else "window"


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


def window_shown_at_spawn(cfg=None, mode=None):
    """Whether THIS boot's window belongs on screen from the first frame.

    The product question this answers: is anybody watching? A gaming launch is
    one instance a person started and is waiting on, so its window goes up at
    spawn and stays up through the boot -- they see the Omni loading animation
    and then Android, which is what "it's working" looks like. A farming launch
    is fifty instances nobody is watching, and fifty windows appearing across
    the desktop is not a product; those still open a window (it is the only
    working GL context on Windows -- see hostwin.py) and still get it hidden.

    That is `_presents_a_window`, which already draws exactly this line for the
    display argv, so the window's visibility and the display it was chosen for
    cannot drift apart.

    `OMNI_HIDE_BOOT_WINDOW=1` / config `qemu.hide_boot_window` forces the old
    behaviour back -- the window hidden until `omnidroid view` asks for it.
    The escape hatch is here rather than absent because "show me the boot" is a
    preference, and somebody running a gaming instance on a second machine they
    are not looking at should not have to take the window.
    """
    env = os.environ.get("OMNI_HIDE_BOOT_WINDOW", "").strip()
    if env:
        if env not in ("0", "false", "False", "no"):
            return False
    else:
        value = ((cfg or {}).get("qemu") or {}).get("hide_boot_window")
        if value is not None and bool(value):
            return False
    return _presents_a_window(gpu_policy(cfg, mode), mode)


def window_icon_path():
    """Our window icon, or None. INSIDE the package on purpose: PyInstaller's
    collect_data_files only picks up data that lives in the package, so a
    repo-root assets/ dir would vanish from the frozen build and the window
    would silently fall back to QEMU's own logo."""
    icon = Path(__file__).resolve().parent / "assets" / WINDOW_ICON_NAME
    return icon if icon.is_file() else None


def window_title(name):
    """What the window is called. `omni: <account>`, matching what every other
    window this product opens is called (the VNC viewer's title, and the title
    bar that used to be stacked above this window before QEMU's own frame
    became the frame).

    Replaces QEMU's `QEMU (omni-<account>)`, which names somebody else's
    product first and ours in brackets."""
    return f"omni: {name}"


def place_window(cmd, identity, cfg, mode=None, icon=None, geometry=None,
                 title=None, pid=None):
    """Decide what happens to the window this spawn just opened. Never raises.

    Returns {"hidden", "visible", "client"} -- `client` is the size the guest
    is being shown at when we set it, else None. There are exactly THREE
    answers, and they have not changed shape, only which one is the common one:

      * no window at all (headless/farming on a host with windowless GL)
        -> nothing to place.
      * `--gpu window`, the debugging hatch -> LEFT EXACTLY AS QEMU MADE IT.
        The design spec defines that flag as "a visible native window,
        UNSTYLED -- for debugging a GL problem with none of this code in the
        path", and presenting it as ours would put this code straight back in
        the path it exists to stay out of.
      * somebody is watching (gaming) -> presented as ours, at the panel size,
        from the first frame.
      * nobody is watching (farming) -> hidden, and kept hidden.
    """
    if not command_opens_a_window(cmd):
        return {"hidden": False, "visible": False, "client": None}
    if gpu_policy(cfg, mode) == GPU_WINDOW:
        return {"hidden": False, "visible": True, "client": None}
    if window_shown_at_spawn(cfg, mode):
        result = _present_window(cmd, identity, cfg, mode=mode, icon=icon,
                                 geometry=geometry, title=title, pid=pid)
        return {"hidden": False, "visible": bool(result.get("presented")),
                "client": result.get("client")}
    return {"hidden": _hide_window_if_wanted(cmd, identity, cfg, pid=pid),
            "visible": False, "client": None}


def _present_window(cmd, identity, cfg, mode=None, icon=None, geometry=None,
                    title=None, pid=None):
    """Put this spawn's window on screen as OURS. Returns the hostwin result.

    Runs INLINE, before spawn_qemu returns, for the same reason the hide does:
    it costs about as long as QEMU needs to map its window, and doing it here
    is what makes the window correct the first time anybody sees it rather than
    a plain GTK window that changes shape and name a moment later.
    """
    from omnidroid import hostwin
    panel = panel_for(mode, cfg)
    result = hostwin.present_qemu_window(
        identity, pid=pid, panel=panel, geometry=geometry, title=title,
        icon=icon if icon is not None else window_icon_path())
    if result.get("presented"):
        client = result.get("client")
        named = result.get("identity") or {}
        print(f"[gpu] the QEMU window is on screen for this boot"
              + (f" at {client[0]}x{client[1]}" if client else "") +
              f" — you will see the loading animation while Android comes up, "
              f"not a blank desktop. Hide it with `omnidroid view {identity} "
              f"--hide`.")
        # Say which half of the naming failed rather than leaving somebody to
        # wonder whether the QEMU logo in their taskbar is by design.
        missing = [k for k in ("title", "icon") if not named.get(k)]
        if missing:
            print(f"[gpu] the window keeps QEMU's own "
                  f"{' and '.join(missing)}; everything else is ours.")
    else:
        print(f"[gpu] the window came up as QEMU's own, unstyled: "
              f"{result.get('reason')}. Rendering and input are unaffected.")
    return result


def _hide_window_if_wanted(cmd, identity, cfg, pid=None):
    """Hide the QEMU window this spawn just opened, unless it was asked for.

    On Windows the window is the price of the GPU, not a feature (see
    hostwin.py). Only an explicit `--gpu window` means "put it on my screen";
    `auto` opens it because it has to and then gets it out of the way.

    Runs INLINE rather than on a thread, and that is deliberate: it takes about
    as long as QEMU needs to map its window (measured well under a second), and
    doing it before spawn_qemu returns means the window cannot flash on screen
    after `start` has already told the caller the instance is up. It also can
    never delay a boot past its bound, because find_window() has one.
    """
    if not command_opens_a_window(cmd):
        return False
    if gpu_policy(cfg) == GPU_WINDOW:
        return False
    from omnidroid import hostwin
    # Ask BEFORE trying. A host with no way to hide a window (Wayland, macOS
    # without Accessibility, a Linux box with none of xdotool/wmctrl/xlib)
    # would otherwise spend find_window's entire timeout rediscovering that on
    # every single boot, and then say only "could not". backend() is probed
    # once per process, so this costs nothing on the host that can.
    if not hostwin.can_hide():
        print(f"[gpu] the QEMU window stays on screen: "
              f"{hostwin.backend_reason()}. Rendering is unaffected, and "
              f"`omnidroid view` still works.")
        return False
    hidden = hostwin.hide_qemu_window(identity, pid=pid)
    if hidden:
        # ...and KEEP it hidden. GTK re-shows the window during early boot (the
        # GL area being realised, the guest's first modeset), so a single hide
        # at spawn is undone before the guest has even joined a place. The
        # watcher is bounded and stops on its own; after the boot, one hide
        # sticks (measured).
        hostwin.keep_hidden(identity, pid=pid)
        print(f"[gpu] the QEMU window is hidden — it exists only because it is "
              f"the only working GL context on this host, and it keeps "
              f"rendering while invisible. Watch with `omnidroid view`.")
    else:
        print(f"[gpu] could not hide the QEMU window for {identity} "
              f"(backend {hostwin.backend()}); it stays on screen. Rendering "
              f"is unaffected.")
    return hidden


# --------------------------------------------------------------- the scratch
#
# Every ephemeral boot runs its disks `snapshot=on`, which is what makes an
# instance diskless: QEMU keeps the guest's writes in a TEMPORARY OVERLAY and
# throws it away at exit. That overlay is not small and it is not on a disk
# anybody chose.
#
# MEASURED 2026-08-15, PS99, one farming instance: the overlay reached
# **1.3 GB** by the time the client was in the world — the game downloads its
# assets into /data and every byte of that lands here. QEMU creates the file
# with the libc temp directory (`GetTempPath` on Windows, `TMPDIR` elsewhere),
# so by default it goes to `%TEMP%`, which is exactly where nobody is looking.
#
# Two failures came out of that on this box, and the second one is nasty:
#
#   * the overlays LEAK. A QEMU that dies rather than exiting cleanly never
#     unlinks its file. `%TEMP%` held 3.7 GB of them from three sessions, one
#     of them two days old.
#   * a full volume kills instances SILENTLY. With the disk exhausted QEMU
#     aborts, and it cannot write the reason into `qemu.log` because writing
#     the log needs the same disk — so the symptom is a guest that was fine a
#     moment ago and is now simply gone, with a zero-byte log and no Windows
#     error report. That is what it looked like twice before the temp
#     directory was measured.
#
# So: point QEMU at a scratch directory we own, reap what leaks into it, and
# refuse a boot that cannot fit rather than discovering it three minutes in.
# The number this caps is INSTANCE COUNT — at ~1.3 GB of scratch each, a host
# runs out of disk long before it runs out of the RAM everyone budgets for.

SCRATCH_DIRNAME = "scratch"
# What one ephemeral instance is assumed to want. PS99 measured 1.3 GB; the
# reserve is deliberately above it, because the cost of guessing low is a dead
# instance and the cost of guessing high is a warning.
SCRATCH_PER_INSTANCE_MB = 2048
# Never let a boot take the volume below this. A Windows host with no free
# disk does not merely stop this program.
SCRATCH_FLOOR_MB = 2048


def scratch_dir(cfg=None):
    """Where QEMU puts its `snapshot=on` overlays. Ours, not `%TEMP%`.

    Beside `runtime/` rather than inside it: `reconcile_runtime()` treats every
    directory under `runtime/` that has no `run.json` as an orphaned instance,
    so putting the scratch there would have it reported as one on every sweep.
    """
    override = (os.environ.get("OMNI_SCRATCH_DIR")
                or ((cfg or {}).get("qemu") or {}).get("scratch_dir"))
    d = Path(override) if override else config.data_dir() / SCRATCH_DIRNAME
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        return None
    return d


def scratch_env(cfg=None, env=None):
    """The child environment that puts QEMU's overlay in `scratch_dir`.

    All three names are set on purpose: Windows' GetTempPath reads TMP then
    TEMP, and glibc/glib read TMPDIR. Setting only the one that matters on the
    platform you happen to be testing is how this silently reverts."""
    base = dict(os.environ if env is None else env)
    d = scratch_dir(cfg)
    if d is None:
        return base
    for name in ("TMP", "TEMP", "TMPDIR"):
        base[name] = str(d)
    _apply_window_env(base)
    return base


# ⚠ NO QEMU READS THESE. They were written for a patched build that does not
# exist -- verified 2026-08-16 against the shipped binaries, which carry no
# `QEMU_WINDOW_*` string. They are harmless (a stock QEMU ignores an unknown
# environment variable, so no boot can break on them) and are kept only as the
# names such a build would use. THE BEHAVIOUR THEY NAME IS PROVIDED ELSEWHERE
# AND IS REAL:
#   icon          -> hostwin.present_qemu_window(icon=...) via WM_SETICON
#   aspect lock   -> `-display gtk,...,keep-aspect-ratio=on` (QEMU letterboxes
#                    instead of stretching) plus hostwin.aspect_lock (the
#                    window itself is held at the guest's ratio)
#   confirm close -> not implemented; see _WINDOW_FLAGS' window-close note.
WINDOW_ICON_NAME = "omni-icon.png"


def _apply_window_env(env):
    """Set the QEMU_WINDOW_* names. INERT on every binary this project ships --
    see the block above before you build anything on top of them."""
    # INSIDE the package on purpose: PyInstaller's collect_data_files only
    # picks up data that lives in the package, so a repo-root assets/ dir
    # would vanish from the frozen build and the window would silently fall
    # back to the stock QEMU icon.
    icon = Path(__file__).resolve().parent / "assets" / WINDOW_ICON_NAME
    if icon.is_file():
        env["QEMU_WINDOW_ICON"] = str(icon)
    env["QEMU_WINDOW_LOCK_ASPECT"] = "1"
    env["QEMU_WINDOW_CONFIRM_CLOSE"] = "1"
    return env


def scratch_free_mb(cfg=None):
    """Free MB on the volume holding the scratch, or None if unknowable."""
    d = scratch_dir(cfg)
    if d is None:
        return None
    try:
        return shutil.disk_usage(str(d)).free // (1024 * 1024)
    except OSError:
        return None


def scratch_room(cfg=None, want_mb=None):
    """(has_room, free_mb, needed_mb). Never raises; unknown free = has room.

    Unknown must mean YES. A disk-usage call that fails on some future host is
    not a reason to refuse to boot; it is a reason to stop asking."""
    want = int(want_mb or SCRATCH_PER_INSTANCE_MB)
    needed = want + SCRATCH_FLOOR_MB
    free = scratch_free_mb(cfg)
    if free is None:
        return True, None, needed
    return free >= needed, free, needed


def reap_scratch(cfg=None, live_pids=()):
    """Delete leaked QEMU overlays. Returns (files, megabytes) reclaimed.

    An overlay belonging to a RUNNING QEMU cannot be deleted on Windows -- the
    open handle makes the unlink fail -- and that is what makes this safe to
    run at any time rather than only when nothing is booted. `live_pids` is
    accepted for the POSIX case, where an open file unlinks happily and the
    running guest would lose its writes.

    QEMU names these `vl.XXXXXX` (qemu-file's template). Matching the name
    rather than deleting the directory's contents matters: the scratch dir is
    a plain directory a user may well have pointed somewhere shared."""
    d = scratch_dir(cfg)
    if d is None:
        return 0, 0
    files = megabytes = 0
    live = set(int(p) for p in live_pids or ())
    for p in d.glob("vl.*"):
        try:
            if not IS_WINDOWS and live and _scratch_owner(p) in live:
                continue
            size = p.stat().st_size
            p.unlink()
        except OSError:      # noqa: PERF203 - a locked file is the live case
            continue
        files += 1
        megabytes += size // (1024 * 1024)
    return files, megabytes


def _scratch_owner(path):
    """The pid holding `path` open on POSIX, or None. Best-effort by design:
    on Windows the failed unlink already answers this question."""
    if IS_WINDOWS:
        return None
    try:
        out = subprocess.run(["fuser", str(path)], capture_output=True,
                             text=True, timeout=5).stdout.split()
        return int(out[0]) if out else None
    except Exception:      # noqa: BLE001 - no fuser, no answer, no crash
        return None


def _pid_started_ticks(pid):
    """runtime.process_start_ticks, imported lazily.

    Lazily because runtime imports from this module; a top-level import here
    is a cycle. Never raises -- a record without this key just falls back to
    the older liveness checks.
    """
    try:
        from omnidroid.runtime import process_start_ticks
        return process_start_ticks(pid)
    except Exception:      # noqa: BLE001
        return None


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
    qemu_env = scratch_env(cfg)
    # ⚠ NOTHING READS QEMU_WINDOW_PANEL. It is set for a QEMU build that was
    # planned and never made; verified 2026-08-16 by byte-scanning the shipped
    # qemu-system-{x86_64,aarch64}.exe, which contain no `QEMU_WINDOW_*` string
    # at all. It is left in place because it is inert on a stock binary and
    # would be the right name if that build ever happens -- but DO NOT add
    # behaviour behind it, and do not read this as "the guest is told the panel
    # size". It is not.
    #
    # What actually keeps the guest's mode and the window's shape in agreement
    # is the pair below, and both are real:
    #   * the window is OPENED at the panel size (place_window -> the client
    #     area, which is the size QEMU hands the guest), and
    #   * it is HELD at that aspect ratio afterwards (hostwin.aspect_lock,
    #     driven by the engine's `_windowlock` helper).
    # Read back out of the argv (gl_panel_size) rather than re-derived, so a
    # config override cannot make the two disagree.
    gl_panel = gl_panel_size(cmd)
    if gl_panel:
        qemu_env["QEMU_WINDOW_PANEL"] = f"{gl_panel[0]}x{gl_panel[1]}"
    proc = subprocess.Popen(cmd, stdout=log, stderr=log,
                            env=qemu_env, **kwargs)
    identity = f"omni-{acct['name']}"
    # Hidden, presented as ours, or left exactly as QEMU made it -- one
    # decision, taken before spawn_qemu returns so the window is never briefly
    # wrong on screen. See place_window.
    placed = place_window(cmd, identity, cfg, mode=mode, pid=proc.pid,
                          geometry=acct.get("geometry"),
                          title=window_title(acct["name"]))
    hidden = placed["hidden"]
    (d / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time(),
         # WHEN that pid was created, which is what makes the pid a durable
         # identity rather than a number the OS will hand to somebody else.
         # Every liveness check reads this instead of asking QEMU over QMP --
         # a busy guest starved that probe and got a live instance declared
         # dead, its governor stopped and its 3.2 GB orphaned. See
         # runtime.process_start_ticks.
         "pid_started": _pid_started_ticks(proc.pid),
         # Whether THIS boot put a real window on the host's screen. Recorded
         # rather than recomputed, so `omnidroid start` can stand its VNC viewer
         # down instead of showing a second, laggier window onto the same
         # instance — and so a degraded gaming boot (host could not open one)
         # still gets the viewer it needs to be watchable at all.
         "native_window": command_opens_a_window(cmd),
         # ...and whether it was then HIDDEN. The two are different questions:
         # a hidden window still holds the GL context (that is the whole point
         # of hiding rather than not opening it), but there is nothing on
         # screen for a user to look at, so the viewer has to come from
         # somewhere else. `omnidroid view` reads this to decide what to say.
         "window_hidden": hidden,
         # ...and whether it is ON SCREEN RIGHT NOW, which is a third question
         # again and the one the app asks. `window_hidden` false used to mean
         # only "`--gpu window`, so it was never hidden"; since a watched boot
         # presents its window at spawn, false now covers both, and `view` has
         # to tell "already up, bring it forward" from "up but unstyled,
         # deliberately". Never inferred from `not window_hidden`.
         "window_visible": placed["visible"],
         # The client size the guest is being SHOWN at, when we set it. Not the
         # same number as gl_panel on a window the user has since resized, and
         # it is the one that has to stay proportional -- see _windowlock.
         "window_client": list(placed["client"]) if placed["client"] else None,
         # What `-m` this boot ACTUALLY got, which is not the mode's default:
         # a place with a measured floor raises it (PS99 boots at 3072 against
         # farming's 2048) and `--mem` overrides both. The working-set governor
         # starts its search at this number, and starting at the mode's default
         # instead would begin the descent already below the guest's real size.
         "mem_mb": (mode or {}).get("mem"),
         # Whether THIS boot rendered on the host GPU, and at what panel size.
         # Recorded for the same reason as native_window — the argv is the
         # truth — and read back by _ensure_booted, which cannot otherwise
         # tell a virgl guest from an llvmpipe one and would install a quality
         # profile the renderer cannot afford. `gl_panel` also carries the fix
         # for the GL device's 640x480 default: the guest takes the first mode
         # it is offered, so the tune-up has to name the one we asked for.
         "gpu": "gl" if command_renders_on_gpu(cmd) else "software",
         "gl_panel": gl_panel_size(cmd),
         # What this boot actually put on screen -- read once off the argv
         # rather than re-derived, so `view` never carries a second copy of
         # the display policy that could drift from what the process did.
         "display_kind": display_kind(cmd),
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


# How many hotplug slots to offer. Each is free until used; four is enough to
# walk a guest from its boot size to `-m` in sensible steps and still have one
# spare, and the count is fixed at spawn (slots cannot be added later).
MEM_SLOTS = 4


def mem_args(mode, mem):
    """The `-m` arguments: a plain size, or a growable one.

    THE POINT OF THE GROWABLE FORM, and why it is not the balloon again.

    A balloon can only take back memory the guest already has, and on Windows
    QEMU cannot decommit what the guest returns (no `madvise`) -- so the host
    pays for the union of every page the guest ever touched, and capping at
    boot was measured to save ~60 MB of 3.4 GB (see balloon.py). Memory that
    was never PLUGGED IN is a different thing entirely: it is not allocated,
    not committed, and the guest cannot touch it because it does not exist.
    There is nothing for the host to fail to give back.

    It also gives back for real, which the balloon cannot do here: unplugging
    a DIMM frees its whole `memory-backend-ram` object, and freeing an
    allocation needs no `madvise`.

    `mem` stays the MAXIMUM -- what the guest may grow to, and what every
    existing caller means by it. `mem_boot` is what it starts with.

    NO MODE SETS `mem_boot` TODAY, and the reason is the guest, not the host.
    MEASURED 2026-08-16 on the Bliss x86 base: QEMU plugs a DIMM happily under
    WHPX (`plugged-memory` 0 -> 512 MB) and the guest's `MemTotal` does not
    move a byte, because the kernel is built `# CONFIG_MEMORY_HOTPLUG is not
    set` -- there is no `/sys/devices/system/memory` for it to online through.
    So growth does not work, and a smaller boot size with no way to grow is
    just `--mem` with extra steps: the same run booted at 1536 MB, cost the
    host 1800 MB instead of 3414 MB, and PS99's client was killed.

    This function is kept, with a test, because it is the whole host-side
    half of the feature and it becomes live the moment a base ships a kernel
    with `CONFIG_MEMORY_HOTPLUG=y` -- which is the cheapest of the two routes
    to a genuinely elastic instance on Windows (the other being the
    `docs/windows-ram-discard.md` QEMU patch).
    """
    boot = (mode or {}).get("mem_boot")
    if not boot or not mem or boot >= mem:
        return ["-m", str(mem)]
    # maxmem needs an explicit unit. Without one QEMU reads it as BYTES and
    # refuses with "maximum memory size (0x1000) must be at least the initial
    # memory size" -- which reads like a sizing mistake rather than a missing
    # suffix.
    return ["-m", f"size={int(boot)},slots={MEM_SLOTS},maxmem={int(mem)}M"]


# ---------------------------------------------------------------------------
# HOW MANY INSTANCES ACTUALLY FIT, and which wall you hit first.
#
# "Why can't I run 30" has four possible answers on Windows and only one of
# them is the RAM everybody plans for. MEASURED on this project's own box
# (i7-13700F / 24 threads, 32 GB, RTX 4060) with one PS99 farming instance
# under the working-set and CPU governors:
#
#   resident memory   384 MB   (3417 MB before the ceiling -- 8.9x)
#   CPU                50%     of one core (161% before the ceiling)
#   COMMIT           3072 MB   = `-m`, and NOTHING reduces it
#   scratch          1300 MB   of disk for the ephemeral overlay
#
# Commit is the one that surprises people. Windows charges the full `-m`
# against RAM+pagefile the moment QEMU maps guest memory, whether or not a
# byte is touched, and there is no way around it in the shipped build:
# `-object memory-backend-file` is not registered in the Windows QEMU
# (verified 2026-08-16), so guest RAM cannot be moved off the commit limit.
# The governors make an instance's RAM and CPU cheap; they cannot make its
# commit cheap. On a 32 GB box with a same-size pagefile that is a ceiling of
# roughly a dozen PS99 instances no matter how well everything else behaves,
# and the fix is disk: a bigger pagefile.

# What one instance costs beyond `-m`, in MB of commit: QEMU itself, its
# threads, the GL context, and the per-instance processes that come with it.
#
# ⚠ THIS WAS 192 UNTIL 2026-08-17 AND IT WAS WRONG BY 4x, because it came from
# the wrong experiment. The old numbers were taken against a PAUSED QEMU with
# no guest in it:
#
#     -m 1024 whpx, -display none      1065 MB   (+41)
#     -m 2048 whpx, -display none      2092 MB   (+44)
#     -m 3072 whpx, -display none      3117 MB   (+45)
#     -m 3072 whpx + gtk,gl=on         3258 MB   (+186)
#
# The source of those said so, and called them a floor. Then this constant
# used them as the COST, and `instance_capacity` divided free commit by it --
# so every capacity answer this project gave was 1.8x too optimistic. A guest
# that is actually running a game maps and touches far more than a paused one:
# virtio-gpu buffers, the GL driver's own allocations, the translated code
# buffers, dirty guest pages.
#
# MEASURED 2026-08-17 on six live PS99 farming instances, in-world, `-accel
# whpx` with the hidden GL window farming actually uses:
#
#     per QEMU process, -m 3072        3777 / 3904 / 4009 MB   (+825 mean)
#     per QEMU process, -m 2048                       2770 MB  (+722)
#     MARGINAL system commit per instance, -m 3072    4065 MB  (+993)
#
# THE MARGINAL NUMBER IS THE RIGHT ONE and it is what this constant holds.
# `instance_capacity` asks "how much of the commit limit does one more
# instance consume", and the answer includes the governor, the window lock and
# adb's share -- not just QEMU's own private bytes. Rounded up from 993:
# being wrong HIGH costs a couple of instances of headroom, being wrong LOW
# fills the commit limit and the host starts failing allocations, which is the
# failure that actually hurts.
#
# The overhead does grow slightly with `-m` (+722 at 2048, +825 at 3072 --
# roughly 10% of the guest plus a ~520 MB base), but two points is not a model
# and a flat conservative constant is honest about that.
#
# ⚠ MEASURE THIS WITH `-accel whpx`: the same probe without it reads +1070 MB,
# because TCG reserves a ~1 GB translation buffer by default and that has
# nothing to do with an instance.
COMMIT_OVERHEAD_MB = 1024


def _commit_status_mb():
    """(limit, charged, available) MB of Windows commit, or None off Windows."""
    if not IS_WINDOWS:
        return None
    try:
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

        m = _MS()
        m.dwLength = ctypes.sizeof(_MS)
        if not ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(m)):
            return None
        mb = 1024 * 1024
        return (m.ullTotalPageFile // mb,
                (m.ullTotalPageFile - m.ullAvailPageFile) // mb,
                m.ullAvailPageFile // mb)
    except Exception:      # noqa: BLE001
        return None


def instance_capacity(mode=None, cfg=None, mem_mb=None):
    """How many more instances of `mode` this host can take, and why not more.

    Returns a dict with one entry per wall and the binding one named. Every
    number is measured off THIS host now, not assumed: free RAM, free commit,
    free scratch disk and idle CPU, divided by what one instance of this mode
    actually costs.

    Never raises. A wall this host cannot measure is reported as None and
    excluded from the verdict rather than guessed at -- an estimate that
    silently drops a constraint is how a capacity plan turns into an
    out-of-memory host.
    """
    mode = mode or MODES.get(DEFAULT_MODE) or {}
    mem = int(mem_mb or mode.get("mem") or 2048)
    walls = {}

    # RESIDENT MEMORY -- what the governor holds an instance to, not `-m`.
    ws = int(mode.get("ws_floor") or mem)
    from omnidroid.runtime import host_mem_available_mb   # lazy: cycle
    free_ram = host_mem_available_mb()
    if free_ram:
        walls["ram"] = {"fits": int(free_ram // max(1, ws)),
                        "free_mb": int(free_ram), "each_mb": ws}

    # COMMIT -- `-m` plus overhead, and nothing reduces it.
    commit = _commit_status_mb()
    if commit:
        _limit, _charged, avail = commit
        each = mem + COMMIT_OVERHEAD_MB
        walls["commit"] = {"fits": int(avail // each), "free_mb": int(avail),
                           "each_mb": each, "limit_mb": _limit}

    # SCRATCH DISK -- the ephemeral overlay every instance writes into.
    free_disk = scratch_free_mb(cfg)
    if free_disk is not None:
        each = SCRATCH_PER_INSTANCE_MB
        walls["disk"] = {
            "fits": int(max(0, free_disk - SCRATCH_FLOOR_MB) // each),
            "free_mb": int(free_disk), "each_mb": each}

    # CPU -- the ceiling the governor holds a farming instance to, or what one
    # was measured to take uncapped.
    _mem, cpus = host_capacity()
    pct = mode.get("cpu_ceiling_pct")
    if cpus:
        each_pct = int(pct or 160)
        walls["cpu"] = {"fits": int((cpus * 100) // max(1, each_pct)),
                        "cores": int(cpus), "each_pct": each_pct}

    counted = {k: v["fits"] for k, v in walls.items()}
    if not counted:
        return {"walls": walls, "fits": None, "binding": None}
    binding = min(counted, key=counted.get)
    return {"walls": walls, "fits": counted[binding], "binding": binding,
            "mode": mode.get("name"), "mem_mb": mem}


def capacity_ladder(mode=None, cfg=None, want=30,
                    sizes=(3072, 2048, 1536, 1024)):
    """How many instances fit at each `-m`, so "how do I get to 30" has an
    answer rather than only "you cannot".

    Commit tracks `-m` at 1:1 (COMMIT_OVERHEAD_MB), so this is the one lever
    that is entirely in the launcher's hands -- everything else needs disk or
    a different machine. Returns [{mem_mb, fits, binding}], smallest guest
    last, plus the first size that reaches `want`.
    """
    rungs = []
    for mem in sizes:
        report = instance_capacity(mode, cfg, mem_mb=mem)
        rungs.append({"mem_mb": mem, "fits": report.get("fits"),
                      "binding": report.get("binding")})
    reaches = next((r["mem_mb"] for r in rungs
                    if (r["fits"] or 0) >= want), None)
    return {"want": want, "rungs": rungs, "reaches_want_at_mem_mb": reaches}


def capacity_shortfall(want, mode=None, cfg=None, mem_mb=None):
    """What it would take to run `want` instances: the gap on every wall.

    "You cannot" is not an answer anybody can act on. This turns the walls
    into a shopping list -- how much more commit, how much more disk, how much
    more RAM -- so the question becomes "free 90 GB and raise the pagefile"
    rather than "buy a bigger computer".

    Returns {wall: {have, need, short}} in MB, plus a `lines` list of prose.
    """
    mode = mode or MODES.get("farming") or {}
    mem = int(mem_mb or mode.get("mem") or 2048)
    report = instance_capacity(mode, cfg, mem_mb=mem)
    gaps, lines = {}, []
    for wall, w in (report.get("walls") or {}).items():
        each = w.get("each_mb") or w.get("each_pct")
        if wall == "cpu":
            have, need = w["cores"] * 100, want * w["each_pct"]
        else:
            have, need = w["free_mb"], want * each
        gaps[wall] = {"have_mb": have, "need_mb": need,
                      "short_mb": max(0, need - have)}
    for wall, g in sorted(gaps.items(), key=lambda kv: -kv[1]["short_mb"]):
        if not g["short_mb"]:
            continue
        lines.append(f"{wall}: {g['short_mb'] // 1024 or 1} GB short "
                     f"({g['need_mb'] // 1024} GB needed, "
                     f"{g['have_mb'] // 1024} GB free)")
    return {"want": want, "mem_mb": mem, "gaps": gaps, "lines": lines,
            "fits_now": report.get("fits")}


def capacity_advice(report):
    """One line saying what to change to fit more. "" when nothing is binding.

    Names the LEVER, not the number: "you are short on commit" is a fact
    nobody can act on, and "make the pagefile bigger" is.
    """
    binding = (report or {}).get("binding")
    walls = (report or {}).get("walls") or {}
    if not binding:
        return ""
    wall = walls.get(binding, {})
    if binding == "commit":
        return (f"COMMIT is the wall: Windows charges the whole `-m` "
                f"({wall.get('each_mb')} MB each) against RAM+pagefile whether "
                f"the guest touches it or not, and the working-set governor "
                f"cannot help with that. Raise the pagefile (System > About > "
                f"Advanced system settings > Performance > Virtual memory), or "
                f"launch with a smaller `--mem`.")
    if binding == "disk":
        return (f"DISK is the wall: each instance keeps a ~"
                f"{wall.get('each_mb')} MB ephemeral overlay in "
                f"{scratch_dir()}. Free space there, or point "
                f"qemu.scratch_dir / OMNI_SCRATCH_DIR at a roomier volume.")
    if binding == "cpu":
        return (f"CPU is the wall: {wall.get('cores')} logical processors "
                f"against {wall.get('each_pct')}% of a core each. Lower the "
                f"mode's cpu_ceiling_pct to fit more (they run slower, and "
                f"they all keep farming).")
    return (f"RAM is the wall: {wall.get('free_mb')} MB free against "
            f"{wall.get('each_mb')} MB resident each.")
