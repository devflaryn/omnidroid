"""Place the QEMU window -- hidden, or shown as ours -- without giving up the GPU.

READ THIS FIRST IF YOU ARE HERE ABOUT THE WINDOW APPEARING LATE. It does not
appear late any more, and the mechanism moved: a boot somebody is WATCHING
(`_presents_a_window` in qemu_proc.py -- gaming, or an explicit `--gpu window`)
now calls `present_qemu_window` at spawn, so the window is on screen for the
whole boot, showing the Omni loading animation, instead of turning up when the
launch finishes. Only a boot nobody is watching -- farming, fifty at a time --
is still hidden. The rest of this docstring is about that second case, which
is why the module is shaped the way it is.


WHY THIS EXISTS, in one paragraph. On Windows, the only GL context QEMU will
hand virglrenderer that actually works is the one attached to a **window**.
Every windowless GL display on this platform -- `egl-headless` and
`dbus,p2p=on,gl=on` alike -- takes its context from ANGLE via EGL at
`EGL_CONTEXT_CLIENT_VERSION 2`, and virglrenderer cannot serve a scanout from
an ES 2.0 context: the guest's SET_SCANOUT is rejected
(`virtio_gpu_virgl_process_cmd: ctrl 0x103, error 0x1203`, 602 times in one
boot), SurfaceFlinger presents nothing, and the screen is black while the GPU
draws happily. The GTK path goes through WGL instead, gets desktop GL, and
exposes OpenGL ES 3.2 to the guest. Measured 2026-08-15; see MODES.md.

So the window is not a feature, it is the price of the GPU. This module stops
the user paying it visually: the window is created (QEMU needs it), and then it
is hidden. **A hidden window keeps rendering** -- measured, 303 frames in 30 s
with the window invisible -- so nothing is lost but the sight of it.

`view` then RESTYLES that same window in place -- see `apply_chrome` below and
`windowbar.py`'s bar, owned BY the window -- rather than copying its pixels
into a viewer of our own. No reparenting, no copy, no encode and no decode,
and input goes straight into the guest instead of being synthesised from RFB.

WHY THE OTHER TWO PLATFORMS ARE HERE. Windows is the only host where a window
is *forced*, but every host where QEMU opens one owes the user the same thing:
a view they can toggle, and nothing on screen they did not ask for. Hiding the
real window beats a copy-based viewer wherever it is possible -- same pixels,
drawn once, no encode -- so it is tried first everywhere and the VNC viewer is
the fallback rather than the plan. Backends, probed once per process:

  * `win32`               -- ShowWindow(SW_HIDE). Measured, load-bearing.
  * `x11-xdotool`         -- unmap the window, found by _NET_WM_NAME.
  * `x11-wmctrl`          -- ask the window manager for _NET_WM_STATE_HIDDEN.
  * `x11-xlib`            -- the same unmap, in-process, when python-xlib
                             happens to be installed (never required).
  * `macos-systemevents`  -- hide the whole QEMU *application* by its unix id.
                             There is no public API to hide one window of
                             another process on macOS, so the application is
                             the smallest unit available, and the handle is
                             therefore the pid rather than a window.

WAYLAND IS NOT SUPPORTED AND MUST NOT PRETEND TO BE. The protocol gives a
client no way to enumerate, map or unmap another client's surfaces -- that is a
design property, not a missing feature -- and QEMU's GTK display on a Wayland
session is a Wayland client, so there is no X11 window for xdotool to find
either. `backend()` returns "none" there with a reason the caller can print,
and every function returns False. Reporting "hidden" for a window still sitting
on the user's screen is worse than doing nothing.

Nothing here may raise into a boot path. Every probe, every subprocess and
every backend returns a falsy value on failure: a visible window is a cosmetic
problem, an exception here is a launch that died for one.
"""
import os
import shutil
import subprocess
import time
from pathlib import Path

from omnidroid.config import IS_LINUX, IS_MACOS, IS_WINDOWS

SW_HIDE = 0
SW_SHOWNOACTIVATE = 4
SW_SHOW = 5

# GetAncestor flag that walks a window up to its top-level (root) window. A
# window that IS its own root is a top-level one, which is how the search
# tells QEMU's real window from the GL drawing area inside it.
GA_ROOT = 2

# How long to wait for QEMU to put its window up. QEMU creates it during
# startup, before the guest's firmware runs, so this is fast in practice; the
# bound only covers a host under heavy load. A miss costs a visible window, not
# a boot -- which is why nothing here raises.
DEFAULT_TIMEOUT = 20.0
POLL_SECONDS = 0.1
# A subprocess-backed probe is not free: xdotool and osascript each cost a
# fork+exec, and polling one at 0.1 s against the 20 s bound is 200 process
# launches to discover a window that never appeared. The window turns up in
# well under a second when it turns up at all, so the coarser interval loses
# nothing real.
SUBPROCESS_POLL_SECONDS = 0.35

BACKEND_NONE = "none"
BACKEND_WIN32 = "win32"
BACKEND_XDOTOOL = "x11-xdotool"
BACKEND_WMCTRL = "x11-wmctrl"
BACKEND_XLIB = "x11-xlib"
BACKEND_MACOS = "macos-systemevents"

_SUBPROCESS_BACKENDS = (BACKEND_XDOTOOL, BACKEND_WMCTRL, BACKEND_MACOS)

# How long a helper command gets before it is abandoned. These are all
# millisecond-scale tools; a bound only matters because an X server that has
# gone away can leave xdotool blocked on a socket forever, and that would
# otherwise be a boot that never returns.
CMD_TIMEOUT = 5.0

_BACKEND_CACHE = {}
_WHICH_CACHE = {}
# Set when a backend that EXISTS refuses to work for a reason retrying cannot
# fix -- today only macOS's TCC permissions. Kept apart from the probe result
# so `can_hide()` can go False mid-process and stop `keep_hidden` asking the
# same denied question 375 times.
_DENIED = {}


# ---------- backend probe ----------

def _which(tool):
    """shutil.which, memoised, and the seam the tests monkeypatch.

    Memoised because `keep_hidden` asks the same questions every poll for two
    and a half minutes, and because the backend chosen from these answers is
    already frozen for the process -- a tool that appears mid-run would change
    nothing anyway, so remembering the miss is not a lie."""
    if tool not in _WHICH_CACHE:
        _WHICH_CACHE[tool] = shutil.which(tool)
    return _WHICH_CACHE[tool]


def _import_xlib():
    """python-xlib's display module, or None.

    Optional BY POLICY: this project ships no third-party dependencies, so xlib
    is used when a host happens to have it (many Linux desktops do, via other
    packages) and is never required."""
    try:
        from Xlib import display
        return display
    except Exception:      # noqa: BLE001 - an optional import must not raise
        return None


def _session_is_wayland():
    return bool(os.environ.get("WAYLAND_DISPLAY")) or \
        os.environ.get("XDG_SESSION_TYPE", "").lower() == "wayland"


def _detect_backend():
    """(name, reason). `reason` is "" unless the name is "none".

    Order on Linux is preference, not availability: xdotool actually unmaps the
    window, wmctrl only ASKS the window manager to, and xlib does what xdotool
    does but needs a package we refuse to depend on.
    """
    if IS_WINDOWS:
        return BACKEND_WIN32, ""
    if IS_MACOS:
        if not _which("osascript"):
            return BACKEND_NONE, ("osascript is not on PATH, so System Events "
                                  "cannot be asked to hide the application")
        return BACKEND_MACOS, ""
    if IS_LINUX:
        if _session_is_wayland():
            return BACKEND_NONE, (
                "this is a Wayland session: the protocol gives one client no "
                "way to map or unmap another's window, and QEMU's GTK display "
                "is a Wayland client with no X11 window to find. Use the VNC "
                "viewer, or log into an X11 session")
        if not os.environ.get("DISPLAY"):
            return BACKEND_NONE, ("no $DISPLAY: this session has no X server, "
                                  "so there is no window to hide")
        if _which("xdotool"):
            return BACKEND_XDOTOOL, ""
        if _which("wmctrl"):
            return BACKEND_WMCTRL, ""
        if _import_xlib() is not None:
            return BACKEND_XLIB, ""
        return BACKEND_NONE, ("no X11 window tool: install xdotool (best), "
                              "or wmctrl, or the python-xlib package")
    return BACKEND_NONE, "hiding windows is not implemented for this platform"


def _backend_probe():
    """The memoised (name, reason).

    Keyed on the platform flags rather than kept in a bare global so a test
    that patches them re-probes; in a real process the key never changes and
    the PATH lookups happen exactly once.
    """
    key = (IS_WINDOWS, IS_LINUX, IS_MACOS)
    if key not in _BACKEND_CACHE:
        try:
            _BACKEND_CACHE[key] = _detect_backend()
        except Exception:      # noqa: BLE001 - a probe must never raise
            return BACKEND_NONE, "the backend probe itself failed"
    return _BACKEND_CACHE[key]


def backend():
    """Short name of the mechanism that hides windows on this host, or "none".

    Callers print it. "The window did not get hidden and nobody said why" is
    the failure mode that costs an afternoon, so the answer is always a name
    plus, when it is "none", a `backend_reason()`."""
    return _backend_probe()[0]


def backend_reason():
    """Why hide/show cannot work here, or "" when it can.

    Covers both "no mechanism exists" (Wayland, no $DISPLAY, no tool) and "the
    mechanism exists and was refused" (macOS without Accessibility permission),
    because from the user's chair those are the same question."""
    name, reason = _backend_probe()
    return _DENIED.get(name) or reason


def can_hide():
    """Whether hiding is possible at all, so a caller can pick a policy without
    a failed attempt -- and, on a windowed boot, without the window flashing up
    while we find out."""
    name = backend()
    return name != BACKEND_NONE and not _DENIED.get(name)


def _poll_seconds():
    return SUBPROCESS_POLL_SECONDS if backend() in _SUBPROCESS_BACKENDS \
        else POLL_SECONDS


def _run(argv, timeout=CMD_TIMEOUT):
    """(rc, stdout, stderr) for a short helper command.

    (None, "", "") when it could not run at all -- a missing binary, a timeout,
    an X server that went away. Every caller treats that as "no", which is the
    only safe direction here."""
    try:
        p = subprocess.run(argv, capture_output=True, text=True,
                           timeout=timeout)
    except Exception:      # noqa: BLE001 - see the module docstring
        return None, "", ""
    return p.returncode, p.stdout or "", p.stderr or ""


# ---------- win32 ----------

def _window_title(hwnd):
    import ctypes
    u = ctypes.windll.user32
    length = u.GetWindowTextLengthW(hwnd)
    if not length:
        return ""
    buf = ctypes.create_unicode_buffer(length + 1)
    u.GetWindowTextW(hwnd, buf, length + 1)
    return buf.value


def _window_class(hwnd):
    import ctypes
    buf = ctypes.create_unicode_buffer(256)
    ctypes.windll.user32.GetClassNameW(hwnd, buf, 256)
    return buf.value


# Windows a QEMU process owns that are NEVER the guest's window. MEASURED by
# enumerating one `-display gtk,gl=on` process:
#
#   gdkWindowToplevel      <- the one, and the only one, that ever is
#   NVOpenGLPbuffer        'NVOGLDC invisible' / '__wglDummyWindowFodder',
#                          1914x994 and invisible -- the driver's own
#   GDI+ Hook Window Class 1x1
#   GdkDisplayChange       0x0, GDK's monitor-change listener
#   IME / MSCTFIME UI      0x0, the input method's
#
# This list is a SAFETY NET under the size test below, not the test itself: a
# class nobody has seen yet still gets judged on whether it looks like a
# window a guest could be drawn in.
_DECOY_CLASSES = frozenset((
    "nvopenglpbuffer", "gdi+ hook window class", "gdkdisplaychange",
    "ime", "msctfime ui", "tooltips_class32",
))
# Smaller than any guest panel this project will ever ask for (the smallest is
# farming's 640x480), and larger than every decoy measured.
_GUEST_MIN_CLIENT = 64


def _is_plausible_guest_window(hwnd):
    """Could this window be the one the guest is drawn in?

    THE PID ALONE IS NOT ENOUGH, and finding that out cost a subtle failure.
    `_enum_windows` will fall back to matching on the pid when the title does
    not match -- which it must, because we rename the window -- and a QEMU
    process owns half a dozen windows from the moment it starts. MEASURED at
    product timing: `find_window` was called ~50 ms after spawn, QEMU's real
    window did not exist until t+0.22 s, and the pid-only fallback happily
    returned the NVIDIA driver's invisible pbuffer instead. Everything
    downstream then styled, renamed, resized and watched a window nothing is
    ever drawn in, while the real one sat on the user's screen at 640x505
    wearing QEMU's own name.

    So a pid-only candidate has to LOOK like a guest window: a top-level, of a
    class that is not a known decoy, with a client area big enough to draw a
    guest in. Nothing qualifies until QEMU's real window exists, which is what
    puts `find_window` back to WAITING for it rather than grabbing whatever
    the process happened to own first.
    """
    try:
        u = _user32()
        if u.GetAncestor(hwnd, GA_ROOT) != hwnd:
            return False
        if _window_class(hwnd).lower() in _DECOY_CLASSES:
            return False
        size = _client_size(hwnd) or (0, 0)
        return size[0] >= _GUEST_MIN_CLIENT and size[1] >= _GUEST_MIN_CLIENT
    except Exception:      # noqa: BLE001
        return False


def _window_pid(hwnd):
    import ctypes
    from ctypes import wintypes
    pid = wintypes.DWORD()
    ctypes.windll.user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    return pid.value


def _walk_windows(visit):
    """Call `visit(hwnd)` for every top-level window AND every child of one.

    BELT-AND-BRACES, not load-bearing: nothing reparents QEMU's window today
    -- `view` restyles it in place and puts our own bar above it as an OWNED
    window, never a child (windowbar.py). This walk used to be load-bearing,
    when an earlier viewer made QEMU's window a child via SetParent:
    `EnumWindows` lists only top-level windows, so the moment that happened
    the window disappeared from the search, and a second `omnidroid view`
    found nothing and reported the guest's display as DESTROYED -- a far more
    alarming state than "you already have it open". That viewer is gone, but
    the walk stays: it costs one extra EnumChildWindows per top-level window,
    and keeping it means a future caller that reparents something again does
    not silently reintroduce the same failure.

    One level of children is enough for that case: a reparented window would
    sit directly on its new parent, never deeper.
    """
    import ctypes
    from ctypes import wintypes
    u = ctypes.windll.user32

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def _child_cb(hwnd, _lparam):
        visit(hwnd)
        return True

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def _top_cb(hwnd, _lparam):
        visit(hwnd)
        u.EnumChildWindows(hwnd, _child_cb, 0)
        return True

    u.EnumWindows(_top_cb, 0)


def _enum_windows(match=None, pid=None):
    """[(hwnd, title)] for windows matching `match` in the title, `pid`, or both.

    THE TWO CRITERIA ARE NOT ANDed, and that is a correction rather than a
    convenience. They used to be, while this docstring claimed "matching on
    the PID is what makes this immune to a retitled window" -- which it was
    not, because a title that no longer matched vetoed a pid that did. The
    window IS retitled now (`apply_identity` gives it our name instead of
    `QEMU (omni-<account>)`), so an ANDed search found nothing at all: `view`
    reported the window DESTROYED on an instance whose window was on screen in
    front of the user.

    So: both when both agree -- that is the unambiguous case, and it is what
    tells `omni-farm3` from `omni-farm30` with several instances up -- and the
    PID ALONE when nothing matched on both. The pid is the stronger claim of
    the two anyway; the title is a string QEMU and this module both write.

    AND THE TITLE IS NOT A TIE-BREAK EITHER, it is one signal among several.
    MEASURED on a live QEMU, after `apply_identity` renamed its window:

        hwnd 63182750  'QEMU (omni-probe)'  hidden   client 640x505
        hwnd 3412334   ''                   hidden   client 0x0
        hwnd 116527724 'omni: probe'        VISIBLE  client 867x537   <- real
        hwnd 55645868  'Default IME'        hidden   client 0x0

    GTK keeps a hidden helper top-level that still carries QEMU's ORIGINAL
    title, so "the window whose title contains the identity" is now the one
    nobody can see. A title-first search returned that handle, and everything
    downstream quietly worked on a window that is never on screen -- the
    aspect lock watched it resize nothing for the life of the instance.

    So every candidate of the right process is ranked, and the ranking says
    what actually makes a window the one you mean: a top-level, on screen,
    with a real client area, ideally titled. The identity match only decides
    which windows are candidates AT ALL when no pid was given.
    """
    candidates = []

    def _visit(hwnd):
        if pid is not None and _window_pid(hwnd) != pid:
            return
        title = _window_title(hwnd)
        titled = bool(match) and match.lower() in title.lower()
        if not titled:
            # Matched on the pid alone, so it has to look like a window a
            # guest could be drawn in -- see _is_plausible_guest_window.
            if pid is None or not _is_plausible_guest_window(hwnd):
                return
        candidates.append((hwnd, title, titled))

    _walk_windows(_visit)

    def _rank(entry):
        hwnd, title, titled = entry
        u = _user32()
        try:
            toplevel = u.GetAncestor(hwnd, GA_ROOT) == hwnd
        except Exception:      # noqa: BLE001
            toplevel = True
        try:
            visible = bool(u.IsWindowVisible(hwnd))
        except Exception:      # noqa: BLE001
            visible = False
        size = _client_size(hwnd) or (0, 0)
        return (0 if toplevel else 1,
                0 if visible else 1,
                0 if (size[0] and size[1]) else 1,
                0 if titled else 1,
                0 if title else 1)

    if backend() != BACKEND_WIN32:
        return [(h, t) for h, t, _ in candidates]
    return [(h, t) for h, t, _ in sorted(candidates, key=_rank)]


def _show(hwnd, how):
    try:
        import ctypes
        ctypes.windll.user32.ShowWindow(hwnd, how)
        return True
    except Exception:      # noqa: BLE001
        return False


# ---------- X11 ----------

# Characters with a meaning in a POSIX extended regex outside a bracket
# expression. NOT re.escape(): that also escapes `-`, and `\-` is undefined in
# POSIX ERE -- xdotool's regcomp is free to reject it, and every identity this
# project builds is `omni-<account>`.
_ERE_SPECIAL = ".[]{}()*+?|^$\\"


def _x11_name_pattern(identity):
    """xdotool matches `--name` as a POSIX extended regex, so an account whose
    name contains a `.` or a `+` would otherwise match the wrong window."""
    return "".join("\\" + c if c in _ERE_SPECIAL else c for c in identity)


def _xdotool_search(identity, pid=None, only_visible=False):
    """[window ids] as strings, or [].

    `--all` is NOT optional: xdotool ORs its criteria by default, so
    `--pid X --name Y` without it matches every window of that process OR every
    window with that name -- which on a host running several instances is the
    wrong window, silently.

    The pid criterion reads `_NET_WM_PID`, which GTK sets. A QEMU started
    through a wrapper, or on an X server that never saw the property, has none,
    so an empty pid-qualified result falls back to the name alone rather than
    concluding the window is gone -- the same "either alone finds it" rule the
    win32 path has.
    """
    base = ["xdotool", "search"]
    if only_visible:
        base = base + ["--onlyvisible"]
    pattern = _x11_name_pattern(identity) if identity else None
    tries = []
    if pid is not None and pattern:
        tries.append(base + ["--all", "--pid", str(pid), "--name", pattern])
    elif pid is not None:
        tries.append(base + ["--pid", str(pid)])
    if pattern:
        tries.append(base + ["--name", pattern])
    for argv in tries:
        rc, out, _err = _run(argv)
        if rc == 0 and out.strip():
            return out.split()
    return []


def _wmctrl_windows(identity, pid=None):
    """[(window id, title)] from `wmctrl -l -p`.

    `-p` adds the pid column, which is the only way wmctrl can tell two
    instances apart when the window titles are similar."""
    rc, out, _err = _run(["wmctrl", "-l", "-p"])
    if rc != 0:
        return []
    hits = []
    for line in out.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 5:
            continue
        wid, _desktop, wpid, _host, title = parts
        if pid is not None and wpid != str(pid):
            continue
        if identity and identity.lower() not in title.lower():
            continue
        if not identity and pid is None:
            continue
        hits.append((wid, title))
    return hits


def _xprop_is_mapped(wid):
    """Whether a wmctrl-found window is on screen, read from ICCCM WM_STATE.

    wmctrl itself cannot answer this: `wmctrl -l` lists a window whether or not
    the window manager has hidden it, and there is no state column. xprop ships
    in the same x11-utils family as wmctrl so it is usually present, and when
    it is not this reports False -- "no reason to re-hide" -- rather than
    guessing. A missed re-hide costs a visible window once; a wrong True costs
    a wmctrl fork every poll for two and a half minutes.

    Worth knowing while you are here: EWMH defines _NET_WM_STATE_HIDDEN as a
    state the window MANAGER sets, so `wmctrl -b add,hidden` is honoured by
    some window managers and quietly ignored by others. That is exactly why
    xdotool, which unmaps the window itself, is probed for first.
    """
    if not _which("xprop"):
        return False
    rc, out, _err = _run(["xprop", "-id", str(wid), "WM_STATE"])
    if rc != 0:
        return False
    return "normal" in out.lower()


def _xlib_window_id(wid):
    """Window ids arrive decimal from xdotool and hex from wmctrl; base 0 takes
    either without the caller having to know which backend it came from."""
    return int(str(wid), 0)


def _xlib_display():
    mod = _import_xlib()
    if mod is None:
        return None
    try:
        return mod.Display()
    except Exception:      # noqa: BLE001
        return None


def _xlib_close(d):
    try:
        d.close()
    except Exception:      # noqa: BLE001
        pass


def _xlib_windows(identity, pid=None):
    """[(window id, title)] via python-xlib.

    Reads `_NET_CLIENT_LIST` rather than walking the window tree: the client
    list is what the window manager considers a real window, so it skips the
    override-redirect menus and GTK's helper windows that a tree walk trips
    over. Ids, not window objects: every call opens its own Display and closes
    it, and an Xlib window outlives its display only as a broken reference.
    """
    d = _xlib_display()
    if d is None:
        return []
    try:
        root = d.screen().root
        prop = root.get_full_property(d.intern_atom("_NET_CLIENT_LIST"), 0)
        net_name = d.intern_atom("_NET_WM_NAME")
        net_pid = d.intern_atom("_NET_WM_PID")
        hits = []
        for wid in (list(prop.value) if prop else []):
            w = d.create_resource_object("window", wid)
            title = ""
            p = w.get_full_property(net_name, 0)
            if p is not None and p.value:
                title = p.value.decode("utf-8", "replace") \
                    if isinstance(p.value, bytes) else str(p.value)
            if not title:
                title = w.get_wm_name() or ""
            if identity and identity.lower() not in title.lower():
                continue
            if pid is not None:
                pp = w.get_full_property(net_pid, 0)
                if pp is None or not pp.value or int(pp.value[0]) != pid:
                    continue
            if not identity and pid is None:
                continue
            hits.append((int(wid), title))
        return hits
    except Exception:      # noqa: BLE001
        return []
    finally:
        _xlib_close(d)


def _xlib_set_visible(wid, visible):
    d = _xlib_display()
    if d is None:
        return False
    try:
        w = d.create_resource_object("window", _xlib_window_id(wid))
        if visible:
            w.map()
        else:
            w.unmap()
        # Without the sync the request is still sitting in Xlib's output buffer
        # when this returns True, and the window is demonstrably still up.
        d.sync()
        return True
    except Exception:      # noqa: BLE001
        return False
    finally:
        _xlib_close(d)


def _xlib_is_visible(wid):
    d = _xlib_display()
    if d is None:
        return False
    try:
        from Xlib import X
        w = d.create_resource_object("window", _xlib_window_id(wid))
        return w.get_attributes().map_state == X.IsViewable
    except Exception:      # noqa: BLE001
        return False
    finally:
        _xlib_close(d)


# ---------- macOS ----------

# What a TCC refusal looks like in osascript's stderr. Matched on the TEXT and
# deliberately NOT on the error number: -1719 is "Can't get <object>", which is
# also what a pid that has already exited returns, and sending a user to System
# Settings because their instance had stopped would be a worse bug than the one
# this detects.
_MACOS_DENIED_MARKERS = (
    "assistive access",                        # Accessibility not granted
    "not authorized to send apple events",     # Automation not granted
    "not authorised to send apple events",
)


def _macos_get_visible_script(pid):
    """Addressed by unix id, never by name: several QEMU instances are the same
    application, so `first process whose name is "qemu-system-x86_64"` would
    read (and hide) whichever one System Events happened to list first."""
    return ('tell application "System Events" to get visible of '
            f'(first process whose unix id is {int(pid)})')


def _macos_set_visible_script(pid, visible):
    return ('tell application "System Events" to set visible of '
            f'(first process whose unix id is {int(pid)}) to '
            f'{"true" if visible else "false"}')


def _osascript(script):
    return ["osascript", "-e", script]


def _note_macos_failure(err):
    """Record a permissions refusal so it is reported once and not retried.

    Returns True when this was a refusal rather than an ordinary failure."""
    low = (err or "").lower()
    if not any(m in low for m in _MACOS_DENIED_MARKERS):
        return False
    first = (err or "").strip().splitlines()
    _DENIED[BACKEND_MACOS] = (
        "System Events refused: grant this app Accessibility and Automation "
        "permission under System Settings > Privacy & Security, or the QEMU "
        "window cannot be hidden"
        + (f" ({first[0].strip()})" if first else ""))
    return True


def _macos_is_visible(pid):
    """True/False, or None when System Events could not answer -- which is also
    how "there is no such process" arrives, since a dead pid and a refused
    query both fail the same query."""
    rc, out, err = _run(_osascript(_macos_get_visible_script(pid)))
    if rc != 0:
        _note_macos_failure(err)
        return None
    text = out.strip().lower()
    return text == "true" if text in ("true", "false") else None


def _macos_set_visible(pid, visible):
    """Hide or show the whole QEMU APPLICATION. There is no public API to hide
    one window of another process on macOS, and QEMU's cocoa display is one
    window per instance, so the application is both the smallest and the right
    unit here.

    Unlike the Windows path this is NOT known to keep rendering while hidden --
    and it does not need to be: macOS has no virgl, so those frames are blitted
    by QEMU on the CPU from a framebuffer the guest fills either way. Nothing
    the guest does depends on the window being on screen.
    """
    rc, _out, err = _run(_osascript(_macos_set_visible_script(pid, visible)))
    if rc != 0:
        _note_macos_failure(err)
        return False
    # Read it back. The command succeeds against a process that is already
    # hidden, against one with no windows at all, and against an app that
    # declines to deactivate -- so the exit code does not answer "is it off the
    # user's screen now", which is the only question being asked.
    state = _macos_is_visible(pid)
    return state is not None and state == visible


def _macos_processes(identity, pid=None):
    """[pid] for the QEMU application to hide.

    The handle on this platform IS the pid, because applications are what macOS
    can hide. `pgrep -f` matches the whole command line, where `-name
    omni-<account>` sits, so a caller that only knows the identity still
    resolves to one process. Our own pid is excluded: an engine invoked with
    the identity somewhere on its own command line would otherwise ask System
    Events to hide the engine.
    """
    if pid is not None:
        return [pid] if _macos_is_visible(pid) is not None else []
    if not identity:
        return []
    rc, out, _err = _run(["pgrep", "-f", identity])
    if rc != 0:
        return []
    me = os.getpid()
    found = []
    for token in out.split():
        try:
            n = int(token)
        except ValueError:
            continue
        if n != me and _macos_is_visible(n) is not None:
            found.append(n)
    return found


# ---------- backend dispatch ----------

def _find_handles(identity, pid):
    name = backend()
    if name == BACKEND_WIN32:
        return [hwnd for hwnd, _t in _enum_windows(identity or None, pid)]
    if name == BACKEND_XDOTOOL:
        return _xdotool_search(identity, pid)
    if name == BACKEND_WMCTRL:
        return [wid for wid, _t in _wmctrl_windows(identity, pid)]
    if name == BACKEND_XLIB:
        return [wid for wid, _t in _xlib_windows(identity, pid)]
    if name == BACKEND_MACOS:
        return _macos_processes(identity, pid)
    return []


def _set_visible(handle, visible):
    name = backend()
    if name == BACKEND_WIN32:
        # SW_SHOWNOACTIVATE, not SW_SHOW: showing is always requested from a
        # viewer or a CLI the user is already looking at, and stealing focus
        # from it is not what "let me see that window" means.
        return _show(handle, SW_SHOWNOACTIVATE if visible else SW_HIDE)
    if name == BACKEND_XDOTOOL:
        rc, _o, _e = _run(["xdotool",
                           "windowmap" if visible else "windowunmap",
                           str(handle)])
        return rc == 0
    if name == BACKEND_WMCTRL:
        # `-i` is not optional: without it `-r` treats its argument as a title
        # to match, and the window id we found would be searched for as text.
        rc, _o, _e = _run(["wmctrl", "-i", "-r", str(handle), "-b",
                           "remove,hidden" if visible else "add,hidden"])
        return rc == 0
    if name == BACKEND_XLIB:
        return _xlib_set_visible(handle, visible)
    if name == BACKEND_MACOS:
        return _macos_set_visible(handle, visible)
    return False


def _handle_is_visible(handle, identity, pid):
    name = backend()
    if name == BACKEND_WIN32:
        try:
            import ctypes
            return bool(ctypes.windll.user32.IsWindowVisible(handle))
        except Exception:      # noqa: BLE001
            return False
    if name == BACKEND_XDOTOOL:
        # xdotool has no "is this id mapped" query, so ask the search itself:
        # `--onlyvisible` filters out the window we just unmapped.
        return str(handle) in _xdotool_search(identity, pid, only_visible=True)
    if name == BACKEND_WMCTRL:
        return _xprop_is_mapped(handle)
    if name == BACKEND_XLIB:
        return _xlib_is_visible(handle)
    if name == BACKEND_MACOS:
        return _macos_is_visible(handle) is True
    return False


# ---------- public API ----------

def find_window(identity, timeout=DEFAULT_TIMEOUT, pid=None):
    """The handle of the QEMU window for `identity`, or None.

    `identity` is what the engine passes to `-name` (`omni-<account>`), which
    QEMU uses as its window title; `pid` is the QEMU process, which the engine
    records in run.json. Either alone finds the window; together they are
    unambiguous when several instances are up.

    On Windows this searches CHILD windows too -- belt-and-braces rather than
    load-bearing today, since nothing reparents QEMU's window anymore (see
    _walk_windows).

    THE HANDLE'S TYPE IS THE BACKEND'S: an HWND on Windows, an X11 window id on
    Linux, and on macOS the QEMU pid itself, because that platform hides
    applications rather than windows. Callers hand it straight back to this
    module and must not interpret it.
    """
    if backend() == BACKEND_NONE or (not identity and pid is None):
        return None
    deadline = time.monotonic() + timeout
    poll = _poll_seconds()
    while True:
        try:
            hits = _find_handles(identity, pid)
        except Exception:      # noqa: BLE001 - a probe must never raise
            return None
        if hits:
            return hits[0]
        if time.monotonic() >= deadline:
            return None
        time.sleep(poll)


def hide_qemu_window(identity, timeout=DEFAULT_TIMEOUT, pid=None):
    """Hide the QEMU window for `identity`. Returns True if it was hidden.

    Never raises and never fails a boot: on any host, any error, or a window
    that never appears, this reports False and the window (if any) simply stays
    on screen. A visible window is a cosmetic problem; an exception here would
    be a launch that died for one.
    """
    handle = find_window(identity, timeout=timeout, pid=pid)
    if handle is None:
        return False
    return _set_visible(handle, False)


HWND_TOP = 0
SW_RESTORE = 9


def bring_to_front(identity, timeout=2.0, pid=None):
    """Raise an already-visible QEMU window. Returns True if it was raised.

    NOT the same operation as `show_qemu_window`, which is why it is its own
    function: showing a window that is already shown is a no-op, so a `view`
    against a boot that presented its own window at spawn did nothing at all
    and printed a success line. What is wanted there is the window found and
    RAISED -- and un-minimised, since "where did it go" is usually the taskbar
    rather than the desktop.

    Windows will not let a background process take the foreground outright
    (SetForegroundWindow is refused unless the caller owns it), so this raises
    with SetWindowPos and asks for the foreground afterwards: the raise always
    lands, and the focus is a bonus when the shell allows it. Never raises.
    """
    if backend() != BACKEND_WIN32:
        return show_qemu_window(identity, timeout=timeout, pid=pid)
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return False
    try:
        u = _user32()
        if u.IsIconic(hwnd):
            u.ShowWindow(hwnd, SW_RESTORE)
        raised = bool(u.SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0,
                                     SWP_NOMOVE | SWP_NOSIZE))
        u.SetForegroundWindow(hwnd)
        return raised
    except Exception:      # noqa: BLE001
        return False


def show_qemu_window(identity, timeout=2.0, pid=None):
    """Bring a hidden QEMU window back, for when someone needs to look at it
    directly (the one configuration where a GL problem is visible with none of
    this project's code in the path).

    `pid` is optional and additive: the macOS backend can only address a
    process, and while it will find one from the identity via pgrep, a caller
    that already has the pid (the engine reads it from run.json) should pass
    it rather than pay a process scan and risk the wrong match."""
    handle = find_window(identity, timeout=timeout, pid=pid)
    if handle is None:
        return False
    return _set_visible(handle, True)


# ---------- chrome: QEMU's window, restyled in place ----------
#
# The caption goes and the sizing border stays. The strip (windowbar.py) is
# the title bar; leaving QEMU's own would put TWO on screen -- a second title
# bar it is impossible to click. WS_THICKFRAME stays so the composite is
# still resizable by dragging the guest window's edges, with the strip
# following.
GWL_STYLE = -16
WS_CAPTION = 0x00C00000
WS_THICKFRAME = 0x00040000
WS_SYSMENU = 0x00080000
WS_MINIMIZEBOX = 0x00020000
WS_MAXIMIZEBOX = 0x00010000

WM_SETICON = 0x0080
ICON_SMALL, ICON_BIG = 0, 1

SWP_NOSIZE = 0x0001
SWP_NOMOVE = 0x0002
SWP_NOZORDER = 0x0004
SWP_NOACTIVATE = 0x0010
SWP_FRAMECHANGED = 0x0020


def _user32():
    """user32, imported lazily so this module stays importable anywhere."""
    import ctypes
    return ctypes.windll.user32


def _get_style(u, hwnd):
    if hasattr(u, "GetWindowLongPtrW"):
        return u.GetWindowLongPtrW(hwnd, GWL_STYLE)
    return u.GetWindowLongW(hwnd, GWL_STYLE)


def _set_style(u, hwnd, style):
    if hasattr(u, "SetWindowLongPtrW"):
        return u.SetWindowLongPtrW(hwnd, GWL_STYLE, style)
    return u.SetWindowLongW(hwnd, GWL_STYLE, style)


def _chrome_result(applied, reason="", hwnd=None):
    return {"applied": applied, "reason": reason, "hwnd": hwnd}


def apply_chrome(identity, pid=None, icon=None, geometry=None, title=None,
                 timeout=DEFAULT_TIMEOUT):
    """Restyle QEMU's window into ours. Never raises.

    Returns {"applied", "reason", "hwnd"}. A host that cannot do it gets a
    plain window and a reason -- this is chrome, and chrome is never worth a
    failed boot.
    """
    name = backend()
    if name in (BACKEND_XDOTOOL, BACKEND_WMCTRL, BACKEND_XLIB):
        return _chrome_result(
            False,
            "window chrome is not implemented on Linux yet: the window keeps "
            "QEMU's own frame. Nothing is broken -- the guest renders on the "
            "GPU and the VNC viewer works. _MOTIF_WM_HINTS is the route and "
            "it will be written against a real host rather than guessed at.")
    if name != BACKEND_WIN32:
        return _chrome_result(
            False,
            f"restyling another process's window is not implemented for "
            f"backend {name}; on macOS the chrome comes from our own QEMU "
            f"build instead")
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return _chrome_result(False, f"no window found for '{identity}'")
    try:
        u = _user32()
        style = _get_style(u, hwnd)
        # The caption STAYS, and it carries OUR name and OUR icon
        # (apply_identity, below). QEMU's own window is the window -- there is
        # no separate strip to be the title bar any more -- so stripping the
        # native frame would leave it with no title, no icon and nothing to
        # drag it by.
        style |= WS_THICKFRAME
        _set_style(u, hwnd, style)
        apply_identity(hwnd, title=title, icon=icon)
        if geometry:
            x, y, width, height = geometry
            u.SetWindowPos(hwnd, 0, int(x), int(y), int(width), int(height),
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED)
        else:
            # The frame changed even when the geometry did not, and Windows
            # does not recompute the non-client area until it is told.
            u.SetWindowPos(hwnd, 0, 0, 0, 0, 0,
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
                           | SWP_NOSIZE | SWP_NOMOVE)
        return _chrome_result(True, "", hwnd)
    except Exception as e:      # noqa: BLE001 - chrome never fails a boot
        return _chrome_result(False, f"could not restyle the window: {e}")


def present_qemu_window(identity, pid=None, icon=None, panel=None,
                        geometry=None, title=None, timeout=DEFAULT_TIMEOUT):
    """Put QEMU's window on screen, ours, AT SPAWN. Returns a result dict.

    {"presented", "reason", "hwnd", "client", "identity"} -- `client` is the
    client size the guest ends up being shown at, which is the size it will
    modeset to, and `identity` says whether our title and icon landed.

    THIS IS THE OPPOSITE END OF `hide_qemu_window`, and the two are the whole
    policy between them: a boot nobody is watching hides the window it was
    forced to open, and a boot somebody IS watching shows it -- from the first
    frame, through the whole boot, rather than at the end of one. The window
    exists either way; only whether it is on screen differs.

    ORDER IS LOAD-BEARING. Style, then icon, then SIZE, and only then show.
    Sizing after showing puts a 640x480 window on screen and then yanks it to
    1280x800 a frame later, which reads as a glitch in the product; and every
    one of those steps before the window is visible costs nothing, because
    nothing has been drawn yet.

    `panel` is the (w, h) the virtio GPU was told to advertise. It is the
    CLIENT size to open at, not the window size -- see the client/window note
    above `_client_size`. Sizing to it matters beyond looking right: QEMU tells
    the guest the client size and the guest modesets to it, so opening at the
    panel is what makes the guest's physical mode equal the `wm size` override
    the tune-up is about to set. Opened at GTK's own default instead, the guest
    comes up 640x480 with a 1280x800 override laid over it -- a 4:3 panel
    carrying a 16:10 layout, which is exactly the squashed picture this is
    fixing.

    `geometry` (x, y, w, h), when given, wins: it is where the user last left
    this window, and restoring it beats re-centring a window they had already
    placed.
    """
    name = backend()
    if name != BACKEND_WIN32:
        return {"presented": False, "hwnd": None, "client": None,
                "identity": {}, "reason":
                    (f"presenting another process's window at spawn is not "
                     f"implemented for backend {name}; the window comes up "
                     f"wherever QEMU put it")}
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return {"presented": False, "hwnd": None, "client": None,
                "identity": {}, "reason": f"no window found for '{identity}'"}
    named = {}
    try:
        u = _user32()
        _set_style(u, hwnd, _get_style(u, hwnd) | WS_THICKFRAME)
        # OUR NAME AND OUR ICON, before it is on screen. QEMU calls its window
        # `QEMU (omni-<account>)` and gives it the QEMU logo; both are set
        # from outside via messages USER32 marshals between processes, so
        # neither needs a patched build nor a second window to caption this
        # one. Done here rather than after the show so the taskbar entry is
        # never briefly somebody else's product.
        named = apply_identity(hwnd, title=title, icon=icon)
        if geometry:
            x, y, width, height = geometry
            u.SetWindowPos(hwnd, 0, int(x), int(y), int(width), int(height),
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED)
        elif panel:
            set_client_size(hwnd, panel[0], panel[1])
        else:
            u.SetWindowPos(hwnd, 0, 0, 0, 0, 0,
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
                           | SWP_NOSIZE | SWP_NOMOVE)
        apply_dwm_style(hwnd)
        # SW_SHOWNOACTIVATE, not SW_SHOW: this fires DURING a launch the user
        # started from the app and is probably still looking at. A window that
        # steals focus mid-boot is what makes people click Stop.
        _show(hwnd, SW_SHOWNOACTIVATE)
        return {"presented": True, "reason": "", "hwnd": hwnd,
                "identity": named, "client": _client_size(hwnd)}
    except Exception as e:      # noqa: BLE001 - a window is never worth a boot
        return {"presented": False, "hwnd": hwnd, "client": None,
                "identity": named,
                "reason": f"could not present the window: {e}"}


IMAGE_ICON = 1
LR_LOADFROMFILE = 0x00000010
LR_DEFAULTSIZE = 0x00008000

WM_SETTEXT = 0x000C
WM_GETICON = 0x007F

# The two sizes Windows asks a window for. Loading each at its own size beats
# loading one and letting the shell scale it: the taskbar's 32px slot showing
# a stretched 16px icon is the difference between "an app" and "a script".
ICON_SIZES = {ICON_SMALL: (16, 16), ICON_BIG: (32, 32)}

_PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
# The icon-resource version CreateIconFromResourceEx wants. It is not a
# Windows version -- it is the RT_ICON resource format version, and 0x00030000
# is the only value any current Windows accepts.
_ICON_RESOURCE_VERSION = 0x00030000


def _icon_bytes(path):
    try:
        return Path(str(path)).read_bytes()
    except Exception:      # noqa: BLE001
        return b""


def _load_icon(u, icon, cx, cy):
    """An HICON for `icon` at `cx` x `cy`, or 0. Never raises.

    TWO LOADERS, because the asset this project actually has is a PNG.
    `omnidroid/assets/omni-icon.png` is a 1024x1024 RGBA PNG (it was added for
    a QEMU build that reads `QEMU_WINDOW_ICON`, which does not exist -- see
    qemu_proc), and `LoadImageW` cannot read a PNG at all: it wants a `.ico`,
    and this repository ships none. `CreateIconFromResourceEx` DOES take PNG
    bytes directly -- a PNG-compressed icon image is a documented icon
    resource form since Vista -- so the PNG needs no conversion, no generated
    `.ico` beside it, and no temporary file. Measured on this host: it returns
    a valid HICON at 16, 32, 256 and default size.

    A real `.ico` still goes through `LoadImageW`, so pointing this at one
    later needs no change here.
    """
    try:
        data = _icon_bytes(icon)
        if data[:8] != _PNG_MAGIC:
            return u.LoadImageW(None, str(icon), IMAGE_ICON, cx, cy,
                                LR_LOADFROMFILE
                                | (LR_DEFAULTSIZE if not cx else 0))
        import ctypes
        from ctypes import wintypes
        fn = u.CreateIconFromResourceEx
        fn.restype = wintypes.HICON
        fn.argtypes = [ctypes.POINTER(ctypes.c_ubyte), wintypes.DWORD,
                       wintypes.BOOL, wintypes.DWORD, ctypes.c_int,
                       ctypes.c_int, wintypes.UINT]
        buf = (ctypes.c_ubyte * len(data)).from_buffer_copy(data)
        return fn(buf, len(data), True, _ICON_RESOURCE_VERSION, cx, cy, 0)
    except Exception:      # noqa: BLE001 - an icon is never worth a boot
        return 0


def _apply_icon(u, hwnd, icon):
    """Put our icon on the window (`WM_SETICON`). Best-effort.

    `u` is the `_user32()` handle the caller already has, and every call goes
    through it. It used to reach for `ctypes.windll.user32` directly for the
    load and use `u` for the SendMessageW calls, which meant a test could
    stand in for half of this function while the other half went to the real
    Win32 API. One seam or none.

    Cross-process on purpose and that is not a trick: `WM_SETICON` is one of
    the messages USER32 marshals between processes, so QEMU's window takes our
    icon without QEMU knowing anything about it.
    """
    for which, (cx, cy) in ICON_SIZES.items():
        hicon = _load_icon(u, icon, cx, cy)
        if hicon:
            u.SendMessageW(hwnd, WM_SETICON, which, hicon)


def _apply_title(u, hwnd, title):
    """Put our title on the window. Best-effort, and cross-process.

    QEMU names its window `QEMU (omni-<account>)` -- its own product name,
    then ours in brackets. `WM_SETTEXT` is marshalled across processes like
    `WM_SETICON`, so the window can carry OUR name instead without a patched
    build and without a second window stacked on top of it to caption it.

    Not `SetWindowTextW`: that is documented not to work on a window owned by
    another process. The message it sends does.
    """
    try:
        import ctypes
        u.SendMessageW(hwnd, WM_SETTEXT, 0, ctypes.c_wchar_p(str(title)))
    except Exception:      # noqa: BLE001
        pass


def apply_identity(hwnd, title=None, icon=None):
    """Our name and our icon on someone else's window. Never raises.

    Returns {"title", "icon"} -> bool, so a caller can see which half landed.
    Both are read back rather than trusted: `SendMessageW` returns 0 for
    `WM_SETTEXT` on success and 0 on a window that has gone away, so its
    return value answers nothing.
    """
    if backend() != BACKEND_WIN32 or not hwnd:
        return {"title": False, "icon": False}
    result = {"title": False, "icon": False}
    try:
        u = _user32()
        if title:
            _apply_title(u, hwnd, title)
            result["title"] = _window_title(hwnd) == str(title)
        if icon:
            before = u.SendMessageW(hwnd, WM_GETICON, ICON_BIG, 0)
            _apply_icon(u, hwnd, icon)
            result["icon"] = u.SendMessageW(hwnd, WM_GETICON,
                                            ICON_BIG, 0) != before
    except Exception:      # noqa: BLE001 - chrome never fails anything
        pass
    return result


# ---------- DWM: the caption is ours, so it should look like ours ----------
#
# Design spec §3a: QEMU's window gives up its whole caption and the strip
# (windowbar.py) becomes the title bar -- "DWM styling (dark mode, rounded
# corners, border colour) therefore applies to the strip, which is the window
# that has a caption". §3b lists the same three under Windows' "chrome applied
# by" column. This is the half of "restyled chrome" that is not geometry.
#
# The attribute numbers are DWMWINDOWATTRIBUTE values. They are passed as bare
# ints rather than through an enum import because there is no such enum in the
# stdlib and the numbers are the API:
#
#   20  DWMWA_USE_IMMERSIVE_DARK_MODE   BOOL   dark caption + dark title text
#   33  DWMWA_WINDOW_CORNER_PREFERENCE  int    2 = DWMWCP_ROUND
#   34  DWMWA_BORDER_COLOR              COLORREF (0x00BBGGRR)
#
# 33 and 34 are Windows 11 (build 22000) only, and 20 needs Windows 10 2004.
# An OLDER Windows does not crash on them: DwmSetWindowAttribute returns a
# non-zero HRESULT (E_INVALIDARG) and changes nothing, which is why each is
# applied independently and none of them is allowed to abort the others. On
# Windows 10, 20 lands and 33/34 do not, which is exactly the right outcome --
# a dark caption with square corners.
DWMWA_USE_IMMERSIVE_DARK_MODE = 20
DWMWA_WINDOW_CORNER_PREFERENCE = 33
DWMWA_BORDER_COLOR = 34

DWMWCP_ROUND = 2

# COLORREF is 0x00BBGGRR, NOT RGB. This is #2B2B2B either way (neutral dark),
# picked to sit against the dark caption rather than to be a brand colour --
# there is no brand palette in this repository to draw one from.
BAR_BORDER_COLOR = 0x002B2B2B


def _dwmapi():
    """dwmapi, imported lazily -- its own seam, like `_user32()`.

    Separate from `_user32()` because it is a different DLL and because a
    host too old to have it at all (or a non-Windows one reached by mistake)
    must fail as a False, not as an ImportError out of a window's setup.
    """
    import ctypes
    return ctypes.windll.dwmapi


def _set_dwm_attribute(hwnd, attribute, value):
    """One DwmSetWindowAttribute call. True when DWM accepted it.

    Each attribute is set on its own so one the running Windows does not know
    (33/34 on Windows 10) cannot take the ones it does know down with it.
    """
    import ctypes
    try:
        data = ctypes.c_int(int(value))
        hresult = _dwmapi().DwmSetWindowAttribute(
            hwnd, ctypes.c_uint(attribute), ctypes.byref(data),
            ctypes.sizeof(data))
        return hresult == 0
    except Exception:      # noqa: BLE001 - chrome never fails anything
        return False


def apply_dwm_style(hwnd, dark=True, rounded=True,
                    border_color=BAR_BORDER_COLOR):
    """Dark caption, rounded corners and our border colour on `hwnd`.

    Returns {"dark", "rounded", "border"} -> bool, so a caller (and a test)
    can see WHICH of the three this Windows accepted rather than one blended
    answer. Never raises: this is paint, and paint has never been worth a
    boot or a window.

    Applied to the BAR, not to QEMU's window: QEMU's window no longer has a
    caption for a dark caption to apply to (apply_chrome strips WS_CAPTION),
    and its corners are the composite's bottom corners, which the strip does
    not own.
    """
    if backend() != BACKEND_WIN32 or not hwnd:
        return {"dark": False, "rounded": False, "border": False}
    return {
        "dark": bool(dark) and _set_dwm_attribute(
            hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, 1),
        "rounded": bool(rounded) and _set_dwm_attribute(
            hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND),
        "border": border_color is not None and _set_dwm_attribute(
            hwnd, DWMWA_BORDER_COLOR, border_color),
    }


def _window_rect(hwnd):
    """(left, top, right, bottom) from GetWindowRect, or None.

    Its own seam, apart from `_user32()`: the real call has to pass
    `ctypes.byref(rect)`, and a `byref` object has no attributes a fake can
    set, so a test cannot stand in for GetWindowRect through `_user32()`
    alone. Replacing this function instead is what the tests do.
    """
    import ctypes

    class RECT(ctypes.Structure):
        _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                    ("right", ctypes.c_long), ("bottom", ctypes.c_long)]

    rect = RECT()
    if not _user32().GetWindowRect(hwnd, ctypes.byref(rect)):
        return None
    return (rect.left, rect.top, rect.right, rect.bottom)


def window_geometry(identity, pid=None, timeout=2.0):
    """(x, y, width, height) of QEMU's window, or None."""
    if backend() != BACKEND_WIN32:
        return None
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return None
    try:
        rect = _window_rect(hwnd)
        if rect is None:
            return None
        left, top, right, bottom = rect
        return (left, top, right - left, bottom - top)
    except Exception:      # noqa: BLE001
        return None


# ---------- the CLIENT area, which is the only size the guest ever sees ------
#
# Everything below works in CLIENT pixels, never window pixels, and the
# difference is load-bearing rather than pedantic. QEMU's GTK display hands the
# guest the size of its DRAWING AREA -- `gd_configure`/`gd_resize_event` call
# `gd_set_ui_size()` with the widget allocation -- and the guest re-modesets to
# exactly that. The caption and the sizing border are ours; the guest never
# hears about them. Locking a WINDOW to 16:10 therefore hands the guest a
# client area that is 16:10 minus ~39 pixels of caption, which is not 16:10,
# and the letterbox bars this whole exercise exists to remove come straight
# back. MEASURED on this box before the client/window split was made: a window
# locked to 1280x800 gave the guest 1264x722.

def _client_size(hwnd):
    """(width, height) of `hwnd`'s client area, or None.

    Its own seam for the same reason as `_window_rect`: the real call needs
    `ctypes.byref`, which a fake cannot stand in for through `_user32()`.
    """
    import ctypes

    class RECT(ctypes.Structure):
        _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                    ("right", ctypes.c_long), ("bottom", ctypes.c_long)]

    rect = RECT()
    if not _user32().GetClientRect(hwnd, ctypes.byref(rect)):
        return None
    return (rect.right - rect.left, rect.bottom - rect.top)


def _frame_padding(hwnd):
    """(extra width, extra height) the frame adds around the client area.

    MEASURED off the live window rather than computed with
    `AdjustWindowRectEx`: that function takes a style and a menu flag and knows
    nothing about DWM's invisible resize borders or per-monitor DPI, so its
    answer is a few pixels out on Windows 11 -- and a few pixels out is exactly
    the error that puts the letterbox bars back.
    """
    rect = _window_rect(hwnd)
    client = _client_size(hwnd)
    if rect is None or client is None:
        return None
    left, top, right, bottom = rect
    return (right - left - client[0], bottom - top - client[1])


def client_size(identity, pid=None, timeout=2.0):
    """(width, height) the GUEST is being shown at, or None.

    This is the number to compare against `wm size`'s `Physical size:` -- they
    are the same quantity, and when they disagree the guest is being scaled.
    """
    if backend() != BACKEND_WIN32:
        return None
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return None
    try:
        return _client_size(hwnd)
    except Exception:      # noqa: BLE001
        return None


def set_client_size(hwnd, width, height, move=None):
    """Resize `hwnd` so its CLIENT area is exactly `width` x `height`.

    `move` is an optional (x, y) top-left for the window; without it the window
    keeps its position and grows/shrinks toward the bottom-right, which is what
    a user who just dragged the bottom-right corner expects.

    Returns True when the resize was issued. Never raises: every caller is
    either a boot path or a daemon watching a window, and neither may die for
    a cosmetic pixel.
    """
    try:
        padding = _frame_padding(hwnd)
        if padding is None:
            return False
        flags = SWP_NOZORDER | SWP_NOACTIVATE
        x = y = 0
        if move is None:
            flags |= SWP_NOMOVE
        else:
            x, y = int(move[0]), int(move[1])
        return bool(_user32().SetWindowPos(
            hwnd, 0, x, y,
            int(width) + padding[0], int(height) + padding[1], flags))
    except Exception:      # noqa: BLE001
        return False


def _is_zoomed(hwnd):
    """True when the window is maximised.

    A maximised window is EXEMPT from the aspect lock, and that is a decision
    rather than an omission: "maximise" means "use the whole screen", and a
    lock that answered it by shrinking the window back off the edges would be
    overriding the one sizing request the user made explicitly. QEMU letterboxes
    the guest inside it (keep-aspect-ratio=on), so the picture is still
    undistorted -- it just has bars, which is the correct answer to a screen
    that is not the guest's shape.
    """
    try:
        return bool(_user32().IsZoomed(hwnd))
    except Exception:      # noqa: BLE001
        return False


def _is_window(hwnd):
    try:
        return bool(_user32().IsWindow(hwnd))
    except Exception:      # noqa: BLE001
        return False


def _work_area(hwnd):
    """(width, height) of the work area of the monitor `hwnd` is on, or None.

    The MONITOR's, not the primary screen's: a window dragged onto a second
    display has to be clamped against that display, and `GetSystemMetrics`
    only ever answers for the primary one. The work area rather than the full
    bounds, because the taskbar is not usable space.
    """
    import ctypes
    from ctypes import wintypes

    class RECT(ctypes.Structure):
        _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                    ("right", ctypes.c_long), ("bottom", ctypes.c_long)]

    class MONITORINFO(ctypes.Structure):
        _fields_ = [("cbSize", wintypes.DWORD), ("rcMonitor", RECT),
                    ("rcWork", RECT), ("dwFlags", wintypes.DWORD)]

    u = _user32()
    monitor = u.MonitorFromWindow(hwnd, 2)      # MONITOR_DEFAULTTONEAREST
    if not monitor:
        return None
    info = MONITORINFO()
    info.cbSize = ctypes.sizeof(MONITORINFO)
    if not u.GetMonitorInfoW(monitor, ctypes.byref(info)):
        return None
    return (info.rcWork.right - info.rcWork.left,
            info.rcWork.bottom - info.rcWork.top)


def _max_client_for(hwnd):
    """The largest CLIENT size that still fits this window's monitor, or None.

    Subtracts the frame, because the clamp has to be expressed in the same
    units as everything else here -- see the client/window note above
    `_client_size`.
    """
    work = _work_area(hwnd)
    padding = _frame_padding(hwnd)
    if work is None or padding is None:
        return None
    return (max(1, work[0] - padding[0]), max(1, work[1] - padding[1]))


AXIS_WIDTH, AXIS_HEIGHT = "w", "h"


def aspect_fit(width, height, ratio_w, ratio_h, previous=None,
               minimum=(320, 200), maximum=None, axis=None):
    """The nearest `ratio_w`:`ratio_h` client size to `width` x `height`.

    Pure, so the snapping rule is testable without a window. WHICH dimension
    survives is the whole design: a user dragging the RIGHT edge changed the
    width and means it, so the height follows; a user dragging the BOTTOM edge
    means the height, so the width follows. `previous` (the last size the lock
    settled on) is what makes that answerable -- the dimension that moved
    further is the one the user was holding. Without it, always solving for
    height would undo a bottom-edge drag completely, which reads as a window
    that refuses to be resized at all.

    Both dimensions are floored at `minimum` so a drag toward zero cannot hand
    the guest a 4x2 panel and a modeset it will not come back from, and capped
    at `maximum` (the monitor's work area) so the OTHER dimension growing to
    satisfy the ratio cannot push the window off the bottom of the screen --
    widening a 16:10 window by 700 px asks for 437 px more height, and on a
    window already near the bottom of the display that is a title bar the user
    can no longer reach.
    """
    width, height = max(1, int(width)), max(1, int(height))
    ratio_w, ratio_h = max(1, int(ratio_w)), max(1, int(ratio_h))
    if axis is not None:
        # THE CALLER LATCHED IT, and during a live drag it has to. Deciding
        # per-frame from `previous` flips mid-drag once our own correction has
        # moved the other dimension, and the two then argue: the user drags
        # the bottom edge, we widen to match, the next mouse move puts the
        # width back (Windows recomputes it from the rect the drag STARTED
        # with), and now the width looks like the dimension that moved.
        drove_width = (axis == AXIS_WIDTH)
    elif previous:
        drove_width = abs(width - int(previous[0])) >= \
            abs(height - int(previous[1]))
    else:
        # No history: keep the larger relative dimension, which grows the
        # window to cover what was asked for rather than shrinking it.
        drove_width = (width / ratio_w) >= (height / ratio_h)
    if drove_width:
        out = (width, int(round(width * ratio_h / float(ratio_w))))
    else:
        out = (int(round(height * ratio_w / float(ratio_h))), height)
    w = max(int(minimum[0]), out[0])
    h = max(int(minimum[1]), out[1])
    if maximum:
        # Scale the PAIR down by whichever axis overflows, rather than
        # clamping each independently: clamping one axis alone is the same
        # distortion this function exists to prevent, just applied by us.
        over = max(w / float(max(1, int(maximum[0]))),
                   h / float(max(1, int(maximum[1]))))
        if over > 1.0:
            w, h = int(w / over), int(h / over)
    # A floor or a cap can break the ratio it was applied to; re-solve from
    # whichever dimension moved, so a clamped size is still the right shape.
    if (w, h) != out:
        if w / float(ratio_w) >= h / float(ratio_h):
            h = int(round(w * ratio_h / float(ratio_w)))
        else:
            w = int(round(h * ratio_w / float(ratio_h)))
        w, h = max(int(minimum[0]), w), max(int(minimum[1]), h)
    # Even widths: virtio-gpu's scanout assumes it, and an odd one shows up as
    # a one-pixel tear rather than an error (same rule as qemu_proc.parse_panel).
    return (w - (w % 2), h - (h % 2))


def aspect_is_close(size, ratio_w, ratio_h, tolerance=2):
    """Whether `size` is already the right shape, within `tolerance` pixels.

    The lock has to be able to say "nothing to do", or it re-issues a
    SetWindowPos every poll for as long as the instance runs -- and every one
    of those re-arms QEMU's one-second ui_info timer, so the guest would be
    told about a resize that never happened, forever.
    """
    width, height = int(size[0]), int(size[1])
    want = int(round(width * int(ratio_h) / float(int(ratio_w))))
    return abs(height - want) <= tolerance


# THE LOCK IS LIVE. It corrects the window WHILE it is being dragged, not
# after the drag ends, and both halves of that are measured rather than
# assumed.
#
# The obvious objection is that it cannot work: a user resize runs inside
# `DefWindowProc`'s modal size loop, which recomputes the window rect from its
# own tracked state on every mouse move, so an outside `SetWindowPos` should
# be undone by the next one. MEASURED 2026-08-17 against a real modal drag
# (WM_NCLBUTTONDOWN + HTBOTTOMRIGHT, then SendInput mouse moves), sampling the
# client rect every 10 ms across a ~700 ms corner drag:
#
#     corrector      samples off-ratio by >2%     ratio at the end
#     none                  76 / 246  (31%)       1.98  (24% off 16:10)
#     every 8 ms             2 / 246   (1%)       1.600
#
# So the correction wins: between two mouse moves there is far more than 8 ms,
# and what is on screen for almost all of that time is the corrected shape.
#
# WHY THE GUEST IS NOT DRAGGED THROUGH A HUNDRED MODESETS. QEMU coalesces:
# `qemu_console_set_ui_info(..., delay=true)` does `timer_mod(ui_timer, now +
# 1000)` on EVERY change, re-arming, so a drag tells the guest nothing until
# it has been still for a second (ui/console.c: "wait until the dust has
# settled"). The lock stops correcting the moment the shape is right
# (`aspect_is_close`), so the dust settles and the guest is told once -- with
# a size that is already the right shape. A corrector that never stopped would
# re-arm that timer forever; that is what an earlier 120 ms attempt did, and
# the guest ended up at "Display output is not active".
ASPECT_POLL_SECONDS = 0.008
# What the poll drops to when nothing is happening. A window at rest does not
# need looking at 125 times a second, and this runs for the life of the
# instance. It is still fast enough that the START of a drag is caught within
# two frames -- a slower idle tier would show as the lock "kicking in" a
# moment after you begin, which is the exact complaint the live correction is
# here to answer.
ASPECT_IDLE_POLL_SECONDS = 0.03
# No user-driven change for this long ends the drag: the latched axis is
# released and the current size becomes the new resting shape.
ASPECT_IDLE_SECONDS = 0.35


def _resolve_lock(identity, ratio, pid, timeout):
    """(hwnd, ratio_w, ratio_h) for a lock, or None when there is nothing to
    hold -- a non-Windows backend, a degenerate ratio, no window."""
    if backend() != BACKEND_WIN32:
        return None
    try:
        ratio_w, ratio_h = int(ratio[0]), int(ratio[1])
    except (TypeError, ValueError, IndexError):
        return None
    if ratio_w <= 0 or ratio_h <= 0:
        return None
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return None
    return hwnd, ratio_w, ratio_h


def _initial_fit(hwnd, ratio_w, ratio_h, on_change=None):
    """Snap a window that is ALREADY off-aspect, once, and return its size.

    Returns the size the window ends up at, whether or not anything was
    changed -- the watcher uses it as its starting history.
    """
    size = _client_size(hwnd)
    if size is None or _is_zoomed(hwnd) or aspect_is_close(size, ratio_w,
                                                           ratio_h):
        return size
    want = aspect_fit(size[0], size[1], ratio_w, ratio_h, previous=size,
                      maximum=_max_client_for(hwnd))
    if want == size or not set_client_size(hwnd, want[0], want[1]):
        return size
    if on_change is not None:
        on_change(want)
    return want


# How often the watcher re-checks that the window is still called what we
# called it.
#
# IT DOES DRIFT, and not randomly: QEMU's `gd_update_caption()` rewrites the
# title whenever the machine's run state changes -- paused, resumed, and the
# `[Stopped]` suffix -- which puts `QEMU (omni-<account>)` back over ours. The
# check is one GetWindowTextW, so this could run every tick; two seconds is
# slow enough to cost nothing measurable and fast enough that nobody reads the
# wrong name off their taskbar.
IDENTITY_RECHECK_SECONDS = 2.0


def _aspect_watch(hwnd, ratio_w, ratio_h, stop_flag, seconds=None,
                  on_change=None, title=None, icon=None):
    """The lock's loop. Returns when the window is gone or `stop_flag` is set.

    WHY A POLL AND NOT A HOOK. The window belongs to QEMU, and Windows will
    not let one process handle another's `WM_SIZING` without injecting a DLL
    into it. What is available from outside is `GetClientRect`, a handful of
    microseconds, so the lock watches instead of intercepting -- and it
    watches FAST, correcting mid-drag rather than waiting for the drag to end.
    See ASPECT_POLL_SECONDS for the measurement that says that works.

    TWO PIECES OF STATE, and neither is optional:

      `applied`  the size WE last set. Anything else the window reads back is
                 a change the USER made, and telling those apart is what stops
                 the loop reacting to its own corrections.
      `axis`     which dimension the user is dragging, LATCHED for the drag.
                 Re-deciding it per frame flips it as soon as our correction
                 has moved the other dimension, and then the lock and the drag
                 argue -- see aspect_fit's `axis` note.
    """
    deadline = None if seconds is None else time.monotonic() + seconds
    # CORRECT WHAT IS ALREADY THERE, before watching for changes. The loop
    # below only reacts to a size the user MOVED, so a lock handed a window
    # that is already the wrong shape would sit and watch it stay wrong -- and
    # that is not a corner case: `view` starts a lock on a window the user hid
    # at some arbitrary size, and a lock restarted after a crash inherits
    # whatever the window drifted to meanwhile.
    settled = _initial_fit(hwnd, ratio_w, ratio_h, on_change)
    applied = settled
    axis = None
    idle_since = time.monotonic()
    next_identity_check = time.monotonic()
    while not stop_flag.is_set():
        if deadline is not None and time.monotonic() >= deadline:
            return
        dragging = axis is not None
        stop_flag.wait(ASPECT_POLL_SECONDS if dragging
                       else ASPECT_IDLE_POLL_SECONDS)
        if stop_flag.is_set():
            return
        try:
            # The instance stopped, or the user closed the window. Nothing to
            # hold, and a lock that kept polling a dead handle would outlive
            # every instance the host ever ran.
            if not _is_window(hwnd):
                return
            # ...and while we are here, is it still called what we called it?
            # This process is the only long-lived thing watching this window,
            # so it is the only place that can notice QEMU renaming it back.
            if title and time.monotonic() >= next_identity_check:
                next_identity_check = (time.monotonic()
                                       + IDENTITY_RECHECK_SECONDS)
                if _window_title(hwnd) != str(title):
                    apply_identity(hwnd, title=title, icon=icon)
            size = _client_size(hwnd)
            if size is None:
                continue
            if size == applied:
                # Nothing new from the user. Once that has been true for long
                # enough the drag is over: release the axis and let this be
                # the shape the next drag is measured against.
                if axis is not None and \
                        time.monotonic() - idle_since >= ASPECT_IDLE_SECONDS:
                    axis, settled = None, size
                continue
            # A user-driven change.
            idle_since = time.monotonic()
            if _is_zoomed(hwnd):
                # Maximised: the user asked for the whole screen and gets it,
                # letterboxed by QEMU. Do not fight that.
                applied, axis, settled = size, None, size
                continue
            if axis is None:
                dw = abs(size[0] - settled[0])
                dh = abs(size[1] - settled[1])
                axis = AXIS_WIDTH if dw >= dh else AXIS_HEIGHT
            if aspect_is_close(size, ratio_w, ratio_h):
                # Already the right shape -- the user landed on it, or this is
                # a plain move. Nothing to correct, and correcting a rounding
                # pixel would re-arm QEMU's ui_info timer forever.
                applied = size
                continue
            want = aspect_fit(size[0], size[1], ratio_w, ratio_h, axis=axis,
                              maximum=_max_client_for(hwnd))
            if want == size or not set_client_size(hwnd, want[0], want[1]):
                # Nothing to do, or the write failed (a window mid-destruction,
                # a denied SetWindowPos). Accept it rather than retrying every
                # 8 ms for the life of the instance.
                applied = size
                continue
            # Record what we ASKED for, not what we read back: the read-back
            # races the resize, and a stale `applied` reads as another user
            # change on the next tick.
            applied = want
            if on_change is not None:
                on_change(want)
        except Exception:      # noqa: BLE001 - never raise off a daemon
            pass


def run_aspect_lock(identity, ratio, pid=None, timeout=DEFAULT_TIMEOUT,
                    seconds=None, on_change=None, title=None, icon=None):
    """Hold QEMU's window at `ratio` UNTIL IT IS GONE. Blocks. Never raises.

    Returns True if it ever held anything, False when there was nothing to
    hold. This is what the engine's detached `_windowlock` process runs: that
    process exists only to be this loop, so it has no reason to put it on a
    thread and then work out how to wait for it.

    `title`/`icon` are re-asserted whenever QEMU renames the window back --
    the shape and the name are held by the same watcher because they are the
    same job (keeping the window ours) and neither justifies a process of its
    own.
    """
    resolved = _resolve_lock(identity, ratio, pid, timeout)
    if resolved is None:
        return False
    import threading
    _aspect_watch(*resolved, threading.Event(), seconds=seconds,
                  on_change=on_change, title=title, icon=icon)
    return True


def aspect_lock(identity, ratio, pid=None, timeout=DEFAULT_TIMEOUT,
                seconds=None, on_change=None, title=None, icon=None):
    """The same lock on a daemon thread. Returns a stop() callable, or None.

    For a caller that has other work to do -- a test, or a future in-process
    viewer. The daemon flag is the same promise `keep_hidden` makes: this must
    never hold a process open, because losing the lock costs a window's shape
    and nothing else.

    `on_change` is called with each corrected (w, h). The engine uses it to
    keep run.json honest about the size the guest is actually being shown at.
    """
    resolved = _resolve_lock(identity, ratio, pid, timeout)
    if resolved is None:
        return None
    import threading
    stop_flag = threading.Event()
    threading.Thread(target=_aspect_watch,
                     args=resolved + (stop_flag,),
                     kwargs={"seconds": seconds, "on_change": on_change,
                             "title": title, "icon": icon},
                     daemon=True, name=f"aspect-{identity}").start()
    return stop_flag.set


def window_is_visible(identity, pid=None):
    """True if a window for `identity` is currently on screen. Used by tests
    and by `debug-info`, so "did the hide actually take" is answerable."""
    handle = find_window(identity, timeout=0, pid=pid)
    if handle is None:
        return False
    try:
        return _handle_is_visible(handle, identity, pid)
    except Exception:      # noqa: BLE001
        return False


# How long to keep re-hiding after the first hide.
#
# MEASURED: hiding once at spawn works, and then GTK puts the window BACK
# during early boot -- by the time the guest had joined a place it was visible
# again. After the guest is up, one hide sticks: 30 s of polling at 1.5 s never
# saw it return. So the window that needs watching is the boot, and the watcher
# can stop once the boot is over rather than run forever fighting the user.
KEEP_HIDDEN_SECONDS = 150.0
KEEP_HIDDEN_POLL = 0.4
# The same watch, paced for backends that cost a fork per question. 0.4 s would
# be ~750 process launches over the full 150 s window; the thing being caught
# is GTK re-mapping the window during boot, which stays re-mapped until we act,
# so a slower look loses nothing but the seconds it is visible.
KEEP_HIDDEN_POLL_SLOW = 1.5


def keep_hidden(identity, seconds=KEEP_HIDDEN_SECONDS, pid=None):
    """Re-hide the window whenever it comes back, for `seconds`. Returns a
    stop() callable, or None where there is nothing to watch.

    A daemon thread on purpose: it must never hold the engine process open, and
    a launch that fails on some other path must not have to remember to tear
    this down. Losing the watcher costs a visible window, nothing else.

    PASS THE PID. A title-only search stops finding this window the moment
    anything renames it -- and `view` renames it, by design (`apply_identity`).
    Today the two paths do not overlap (only a PRESENTED window is renamed, and
    a presented window is not one being kept hidden), so a watcher without a
    pid still works by coincidence rather than by construction. The pid makes
    it construction.
    """
    if not can_hide() or not identity:
        return None
    import threading
    stop_flag = threading.Event()
    poll = KEEP_HIDDEN_POLL if backend() == BACKEND_WIN32 \
        else KEEP_HIDDEN_POLL_SLOW

    def _watch():
        deadline = time.monotonic() + seconds
        while not stop_flag.is_set() and time.monotonic() < deadline:
            try:
                # A macOS TCC refusal is permanent for this process, so stop
                # rather than spend the remaining minutes re-asking a question
                # already answered with "no".
                if not can_hide():
                    return
                if window_is_visible(identity, pid=pid):
                    hide_qemu_window(identity, timeout=0, pid=pid)
            except Exception:      # noqa: BLE001 - never raise off a daemon
                pass
            stop_flag.wait(poll)

    threading.Thread(target=_watch, daemon=True,
                     name=f"hide-{identity}").start()
    return stop_flag.set
