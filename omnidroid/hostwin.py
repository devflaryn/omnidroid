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

The viewer then HOSTS that window rather than connecting to a framebuffer --
see `embedview.py`. Reparenting it costs no copy, no encode and no decode, and
input goes straight into the guest instead of being synthesised from RFB.

Windows-only by design, and that is not a gap: it is the only platform where a
window is forced. Linux renders windowless through egl-headless as intended,
and macOS has no virgl at all yet, so both are already headless there. Every
function here is a no-op that reports False elsewhere.
"""
import time

from omnidroid.config import IS_WINDOWS

SW_HIDE = 0
SW_SHOWNOACTIVATE = 4
SW_SHOW = 5

# How long to wait for QEMU to put its window up. QEMU creates it during
# startup, before the guest's firmware runs, so this is fast in practice; the
# bound only covers a host under heavy load. A miss costs a visible window, not
# a boot -- which is why nothing here raises.
DEFAULT_TIMEOUT = 20.0
POLL_SECONDS = 0.1


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

    THE CHILDREN MATTER, and leaving them out was a real defect. `EnumWindows`
    lists only top-level windows -- so the moment the viewer embeds QEMU's
    window (SetParent makes it a child), it disappears from the search. A
    second `omnidroid view` on an already-embedded instance then found no
    window and reported the guest's display as DESTROYED, which is a different
    and much more alarming state than "you already have it open".

    One level of children is enough: QEMU's window is reparented directly onto
    the viewer's frame, never deeper.
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

    Matching on the PID is what makes this survive embedding and a retitled
    window; the title match stays because the pid is not always to hand (the
    engine spawns QEMU detached and a caller may only know the identity).
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


def find_window(identity, timeout=DEFAULT_TIMEOUT, pid=None):
    """The hwnd of the QEMU window for `identity`, or None.

    `identity` is what the engine passes to `-name` (`omni-<account>`), which
    QEMU uses as its window title; `pid` is the QEMU process, which the engine
    records in run.json. Either alone finds the window; together they are
    unambiguous when several instances are up.

    Searches CHILD windows too -- see _walk_windows for why that is not
    optional once the viewer starts embedding.
    """
    if not IS_WINDOWS or (not identity and pid is None):
        return None
    deadline = time.monotonic() + timeout
    while True:
        try:
            hits = _enum_windows(identity or None, pid)
        except Exception:      # noqa: BLE001 - a probe must never raise
            return None
        if hits:
            return hits[0][0]
        if time.monotonic() >= deadline:
            return None
        time.sleep(POLL_SECONDS)


def window_is_embedded(identity, pid=None):
    """True when the QEMU window exists but is a CHILD of something.

    That is the "a viewer already has it" state, and it has to be told apart
    from "the window is gone": one means open the window you already have, the
    other means the guest is blind and the instance needs restarting."""
    hwnd = find_window(identity, timeout=0, pid=pid)
    if hwnd is None:
        return False
    try:
        import ctypes
        return bool(ctypes.windll.user32.GetParent(hwnd))
    except Exception:      # noqa: BLE001
        return False


def _show(hwnd, how):
    try:
        import ctypes
        ctypes.windll.user32.ShowWindow(hwnd, how)
        return True
    except Exception:      # noqa: BLE001
        return False


def hide_qemu_window(identity, timeout=DEFAULT_TIMEOUT, pid=None):
    """Hide the QEMU window for `identity`. Returns True if it was hidden.

    Never raises and never fails a boot: on any host, any error, or a window
    that never appears, this reports False and the window (if any) simply stays
    on screen. A visible window is a cosmetic problem; an exception here would
    be a launch that died for one.
    """
    hwnd = find_window(identity, timeout=timeout, pid=pid)
    if hwnd is None:
        return False
    return _show(hwnd, SW_HIDE)


# How long to keep re-hiding after the first hide.
#
# MEASURED: hiding once at spawn works, and then GTK puts the window BACK
# during early boot -- by the time the guest had joined a place it was visible
# again. After the guest is up, one hide sticks: 30 s of polling at 1.5 s never
# saw it return. So the window that needs watching is the boot, and the watcher
# can stop once the boot is over rather than run forever fighting the user.
KEEP_HIDDEN_SECONDS = 150.0
KEEP_HIDDEN_POLL = 0.4


def keep_hidden(identity, seconds=KEEP_HIDDEN_SECONDS):
    """Re-hide the window whenever it comes back, for `seconds`. Returns a
    stop() callable, or None where there is nothing to watch.

    A daemon thread on purpose: it must never hold the engine process open, and
    a launch that fails on some other path must not have to remember to tear
    this down. Losing the watcher costs a visible window, nothing else.
    """
    if not IS_WINDOWS or not identity:
        return None
    import threading
    stop_flag = threading.Event()

    def _watch():
        deadline = time.monotonic() + seconds
        while not stop_flag.is_set() and time.monotonic() < deadline:
            try:
                if window_is_visible(identity):
                    hide_qemu_window(identity, timeout=0)
            except Exception:      # noqa: BLE001 - never raise off a daemon
                pass
            stop_flag.wait(KEEP_HIDDEN_POLL)

    threading.Thread(target=_watch, daemon=True,
                     name=f"hide-{identity}").start()
    return stop_flag.set


def show_qemu_window(identity, timeout=2.0):
    """Bring a hidden QEMU window back, for when someone needs to look at it
    directly (the one configuration where a GL problem is visible with none of
    this project's code in the path).

    SW_SHOWNOACTIVATE, not SW_SHOW: this is called from a viewer or a CLI the
    user is already looking at, and stealing focus from it is not what asking
    to see a window means."""
    hwnd = find_window(identity, timeout=timeout)
    if hwnd is None:
        return False
    return _show(hwnd, SW_SHOWNOACTIVATE)


def window_is_visible(identity, pid=None):
    """True if a window for `identity` is currently on screen. Used by tests
    and by `debug-info`, so "did the hide actually take" is answerable."""
    hwnd = find_window(identity, timeout=0, pid=pid)
    if hwnd is None:
        return False
    try:
        import ctypes
        return bool(ctypes.windll.user32.IsWindowVisible(hwnd))
    except Exception:      # noqa: BLE001
        return False
