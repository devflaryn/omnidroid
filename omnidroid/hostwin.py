"""Hide the QEMU window without giving up the GPU.

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

from omnidroid.config import IS_LINUX, IS_MACOS, IS_WINDOWS

SW_HIDE = 0
SW_SHOWNOACTIVATE = 4
SW_SHOW = 5

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

    Matching on the PID is what makes this immune to a retitled window (and
    would survive a window being made a child of something else, if anything
    ever did that again -- see _walk_windows); the title match stays because
    the pid is not always to hand (the engine spawns QEMU detached and a
    caller may only know the identity).
    """
    found = []

    def _visit(hwnd):
        if pid is not None and _window_pid(hwnd) != pid:
            return
        title = _window_title(hwnd)
        if match and match.lower() not in title.lower():
            return
        if match is None and pid is None:
            return
        found.append((hwnd, title))

    _walk_windows(_visit)
    return found


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


def apply_chrome(identity, pid=None, icon=None, geometry=None,
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
        style &= ~(WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX)
        style |= WS_THICKFRAME
        _set_style(u, hwnd, style)
        if icon:
            _apply_icon(u, hwnd, icon)
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


IMAGE_ICON = 1
LR_LOADFROMFILE = 0x00000010
LR_DEFAULTSIZE = 0x00008000


def _apply_icon(u, hwnd, icon):
    """Put our icon on the window (spec §3b: `WM_SETICON`). Best-effort.

    `u` is the `_user32()` handle the caller already has, and BOTH calls go
    through it -- LoadImageW included. It used to reach for
    `ctypes.windll.user32` directly for the load and then use `u` for the
    two SendMessageW calls, which meant a test could stand in for half of
    this function and the other half went to the real Win32 API. One seam or
    none.

    NO CALLER PASSES AN ICON TODAY, and that is not an oversight to fix by
    inventing one: this repository ships no `.ico` (the only icon asset in
    the product family is `omni-executor/packaging/icon.icns`, a macOS
    bundle icon that `LoadImageW` cannot read). The function stays because
    the spec requires the icon and the mechanism is the part worth getting
    right; point it at a real `.ico` when one is drawn and both QEMU's
    window and the bar get it from here.
    """
    hicon = u.LoadImageW(None, str(icon), IMAGE_ICON, 0, 0,
                         LR_LOADFROMFILE | LR_DEFAULTSIZE)
    if hicon:
        u.SendMessageW(hwnd, WM_SETICON, ICON_SMALL, hicon)
        u.SendMessageW(hwnd, WM_SETICON, ICON_BIG, hicon)


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


def keep_hidden(identity, seconds=KEEP_HIDDEN_SECONDS):
    """Re-hide the window whenever it comes back, for `seconds`. Returns a
    stop() callable, or None where there is nothing to watch.

    A daemon thread on purpose: it must never hold the engine process open, and
    a launch that fails on some other path must not have to remember to tear
    this down. Losing the watcher costs a visible window, nothing else.
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
                if window_is_visible(identity):
                    hide_qemu_window(identity, timeout=0)
            except Exception:      # noqa: BLE001 - never raise off a daemon
                pass
            stop_flag.wait(poll)

    threading.Thread(target=_watch, daemon=True,
                     name=f"hide-{identity}").start()
    return stop_flag.set
