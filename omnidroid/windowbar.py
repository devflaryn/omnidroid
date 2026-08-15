"""Our title bar for QEMU's window, owned BY that window.

QEMU's window stays top-level for its whole life and is restyled in place
(hostwin.apply_chrome). This module supplies the caption it gave up: our
title, our icon, minimise, and an X that ASKS -- hide the window, or stop the
instance.

THE OWNERSHIP DIRECTION IS THE WHOLE DESIGN. The viewer this replaces made
QEMU's window a CHILD of a Tk window, and Windows destroys a child with its
parent: a force-killed viewer left the instance alive, answering adb, and
rendering nothing at all (measured, totalFrames = 0). An OWNED window gives us
the two properties we wanted from that arrangement --

    * it always floats above its owner, so z-order needs no polling
    * it minimises and restores with its owner

-- and none of the property that hurt: destroying an owned window does nothing
to its owner. Kill this bar however you like; the guest keeps rendering.

The close button has to be ours because it cannot be anyone else's: one
process cannot intercept another's WM_CLOSE without injecting a DLL. QEMU is
therefore spawned `window-close=off` and its X is inert -- and its caption is
stripped, so there is no second title bar to click.

WINDOWS ONLY, for now. Linux is deferred -- no X11 code exists in this module
(own() returns False there, same as on every other non-Windows host); macOS
gets this from the QEMU patch instead, because it has no public API to
restyle or own another process's NSWindow.
"""
import sys

from omnidroid import hostwin
from omnidroid.config import IS_WINDOWS

# SetWindowLongPtr index for the OWNER of a window. Not GWLP_HWNDPARENT's
# other meaning: for a top-level window this sets the owner, and for a child
# it would set the parent -- which is why the bar must be a top-level popup
# and never a child of anything.
GWLP_HWNDPARENT = -8

BAR_HEIGHT = 34

SWP_NOZORDER = 0x0004
SWP_NOACTIVATE = 0x0010

# GetAncestor flag that walks a widget's HWND up to its top-level (root)
# window. Tk's winfo_id() is the WIDGET's own handle, which is not always the
# top-level one -- owning the wrong handle silently does nothing at all: no
# error, no effect, just a bar that never floats above anything.
GA_ROOT = 2

# Style bits stripped by _strip_resize_border, applied BY HAND instead of
# through Tk's `wm resizable(False, False)` -- see that function's docstring
# for why: the Tk call locks WM_GETMINMAXINFO to a stale size and the raw
# SetWindowPos in follow() loses that fight for the life of the window.
GWL_STYLE = -16
WS_THICKFRAME = 0x00040000
WS_MAXIMIZEBOX = 0x00010000
SWP_FRAMECHANGED = 0x0020
SWP_NOSIZE = 0x0001
SWP_NOMOVE = 0x0002


def _user32():
    import ctypes
    return ctypes.windll.user32


def _window_rect(hwnd):
    """(x, y, width, height) of `hwnd` via GetWindowRect, or None.

    Its own seam apart from `_user32()`: the real call needs
    `ctypes.byref(rect)`, and a `byref` object has no attributes a fake can
    set, so a test cannot stand in for GetWindowRect through `_user32()`
    alone -- this function is what gets replaced instead (same reasoning as
    `hostwin._window_rect`, which this mirrors).
    """
    import ctypes

    class RECT(ctypes.Structure):
        _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                    ("right", ctypes.c_long), ("bottom", ctypes.c_long)]

    rect = RECT()
    if not _user32().GetWindowRect(hwnd, ctypes.byref(rect)):
        return None
    return (rect.left, rect.top, rect.right - rect.left,
            rect.bottom - rect.top)


def _strip_resize_border(hwnd):
    """Remove the sizing border and maximize box from the BAR's own window.

    MEASURED BUG (task-9-report.md, 2026-08-16): the strip rendered as a
    ~216x239 square jammed into the guest window's top-left corner instead
    of a thin strip spanning its full width. Root cause, confirmed on this
    box by reproducing it in isolation: `run_window_bar` used to call Tk's
    own `root.resizable(False, False)` to strip these same two bits. On
    Windows, that Tk call does two things, not one -- it removes
    WS_THICKFRAME/WS_MAXIMIZEBOX (wanted), AND it locks WM_GETMINMAXINFO's
    min/max TRACK size to whatever Tk's own "natural" size happened to be at
    that instant. For a bare toplevel with no child widgets that natural size
    is Tk's built-in ~200x200 client-area default -- nothing to do with the
    real target -- and the lock is not a one-time race to lose: Windows
    re-enforces it on EVERY later SetWindowPos, including the raw one
    `WindowBar.follow()` makes with the real owner-relative rect. Reproduced
    exactly: calling resizable(False, False) before follow() measured
    216x239 immediately, before mainloop() ever ran, and it stayed clamped to
    that size through a mainloop that ran for 1.5s.

    Stripping the identical two bits BY HAND, without ever calling `wm
    resizable`, leaves no such lock behind: Windows' own WM_GETMINMAXINFO
    default for this style is a plain per-monitor minimum (measured height
    40px, not exactly BAR_HEIGHT's 34 -- Windows will not draw a caption
    shorter than its own system minimum, which is expected: BAR_HEIGHT is
    already documented as leaving near-zero client area on purpose), and
    follow()'s SetWindowPos sticks -- verified stable through a live
    mainloop, not just before it starts.

    The resulting style is byte-for-byte the hardware-measured bar style
    (0x16CA0008: WS_CAPTION|WS_SYSMENU|WS_MINIMIZEBOX, no WS_THICKFRAME, no
    WS_MAXIMIZEBOX) -- this changes HOW those bits get stripped, not WHAT the
    bar ends up looking like.

    Never raises: a failure here costs a resize border nobody asked for, not
    a boot.
    """
    try:
        u = _user32()
        style = u.GetWindowLongPtrW(hwnd, GWL_STYLE)
        style &= ~(WS_THICKFRAME | WS_MAXIMIZEBOX)
        u.SetWindowLongPtrW(hwnd, GWL_STYLE, style)
        u.SetWindowPos(hwnd, 0, 0, 0, 0, 0,
                       SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
                       | SWP_NOSIZE | SWP_NOMOVE)
        return True
    except Exception:      # noqa: BLE001
        return False


def bar_geometry(owner_rect, bar_height=BAR_HEIGHT):
    """Where the bar goes for a guest window at `owner_rect`.

    Directly above it, exactly as wide. Clamped at the top of the screen so a
    window dragged to y=0 does not put its own title bar off-screen.
    """
    x, y, width, _height = owner_rect
    return (x, max(0, y - bar_height), width, bar_height)


def _ask_close(parent=None):
    """Hide, stop, or cancel. The seam the tests replace.

    Three buttons rather than a yes/no, because the two real answers are not
    opposites: hiding keeps a booted instance that took a minute to reach the
    world, and stopping throws it away.
    """
    import tkinter as tk
    from tkinter import ttk

    answer = {"value": "cancel"}
    dialog = tk.Toplevel(parent) if parent else tk.Tk()
    dialog.title("Close window")
    dialog.resizable(False, False)
    ttk.Label(dialog,
              text="Hide this window, or stop the instance?\n"
                   "Hiding keeps it running — show it again from the app.",
              justify="left").pack(padx=16, pady=(16, 12))
    row = ttk.Frame(dialog)
    row.pack(padx=16, pady=(0, 16), fill="x")

    def choose(value):
        answer["value"] = value
        dialog.destroy()

    ttk.Button(row, text="Hide window",
               command=lambda: choose("hide")).pack(side="left")
    ttk.Button(row, text="Stop instance",
               command=lambda: choose("stop")).pack(side="left", padx=8)
    ttk.Button(row, text="Cancel",
               command=lambda: choose("cancel")).pack(side="right")
    dialog.grab_set()
    dialog.wait_window()
    return answer["value"]


class WindowBar:
    """The strip. Nothing here raises into a caller."""

    def __init__(self, identity, pid=None, on_stop=None):
        self.identity = identity
        self.pid = pid
        self.on_stop = on_stop
        self.owner_hwnd = None
        self.bar_hwnd = None
        # Set by on_close() when a "stop" answer's hook raised. The RETURN
        # VALUE of on_close() stays "stop" regardless -- that is the CHOICE
        # the user made, not whether it worked -- so this flag is the only
        # place a caller can tell a failed stop from a real one. Checked by
        # run_window_bar before it destroys the bar: closing the window on a
        # failed stop would tell the user a still-running ~3 GB instance had
        # gone away, with no console anywhere to say otherwise.
        self.stop_failed = False

    def own(self, bar_hwnd, owner_hwnd):
        """Make the bar an owned window of the guest's window.

        NEVER the reverse -- see the module docstring.
        """
        if not IS_WINDOWS:
            return False
        try:
            _user32().SetWindowLongPtrW(bar_hwnd, GWLP_HWNDPARENT, owner_hwnd)
            self.bar_hwnd, self.owner_hwnd = bar_hwnd, owner_hwnd
            return True
        except Exception:      # noqa: BLE001
            return False

    def follow(self, owner_rect):
        """Move the bar to sit above the guest window at `owner_rect`."""
        if self.bar_hwnd is None:
            return False
        x, y, width, height = bar_geometry(owner_rect)
        try:
            u = _user32()
            u.SetWindowPos(self.bar_hwnd, 0, x, y, width, height,
                          SWP_NOZORDER | SWP_NOACTIVATE)
            # Windows enforces its own minimum caption height for a window
            # with WS_CAPTION and will not shrink it below that floor no
            # matter what height is requested -- measured 40px on real
            # hardware against a requested BAR_HEIGHT of 34
            # (task-9-report.md's "Geometry fix"). `y` above assumed the
            # REQUESTED height, so when the floor is taller than that the
            # bar's bottom edge overlaps the guest window's top edge by the
            # difference. Read back what Windows actually granted and, if it
            # differs, reposition (size unchanged) so the bar's bottom lands
            # exactly on the guest's top edge regardless of what floor this
            # machine's DPI/theme happens to enforce -- do not trust the
            # request, read back reality, the same rule hostwin's chrome and
            # visibility checks already follow.
            actual = _window_rect(self.bar_hwnd)
            if actual is not None:
                actual_height = actual[3]
                if actual_height != height:
                    owner_top = owner_rect[1]
                    u.SetWindowPos(self.bar_hwnd, 0, x, owner_top - actual_height,
                                  0, 0,
                                  SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE)
            return True
        except Exception:      # noqa: BLE001
            return False

    def on_close(self, parent=None):
        """The X was clicked. Returns "hide", "stop" or "cancel".

        The return value is always the CHOICE the user made, never whether
        it succeeded -- callers and tests both depend on that contract. A
        "stop" hook that raises does not change the return value to
        something else; it sets `self.stop_failed` instead (and writes the
        reason to stderr for anyone with a console), so a caller can tell a
        real stop from a failed one without this method lying about which
        button was pressed.
        """
        answer = _ask_close(parent)
        self.stop_failed = False
        if answer == "hide":
            hostwin.hide_qemu_window(self.identity)
        elif answer == "stop" and self.on_stop:
            try:
                self.on_stop(self.identity)
            # SystemExit alongside Exception, NOT a bare `except:` --
            # KeyboardInterrupt must still propagate. The hook is
            # engine.cmd_stop, which ends in sys.exit(1) on a failed
            # shutdown, and load_account (called first) does sys.exit(str)
            # on a bad account -- both raise SystemExit, which `except
            # Exception` does NOT catch. Left uncaught it escapes on_close,
            # Tkinter re-raises it out of mainloop(), and this whole
            # (detached, console-less) process dies -- indistinguishable
            # from a successful stop, on an instance that may still be
            # running with several GB attached. Treat it exactly like any
            # other failed stop: record it, do not let it vanish the bar.
            except (Exception, SystemExit) as e:      # noqa: BLE001
                self.stop_failed = True
                sys.stderr.write(
                    f"window bar: stop hook for '{self.identity}' failed: "
                    f"{e}\n")
        return answer


def _create_bar_window(title):
    """A bare Tk toplevel with its resize border already stripped by hand.

    Returns `(root, bar_hwnd)` -- `bar_hwnd` is None when the top-level
    window handle could not be resolved via GetAncestor, in which case the
    style was never touched either.

    Split out of run_window_bar() so a test can drive the EXACT sequence
    that produced the measured 216x239 bug (task-9-report.md: the bar
    rendered as a small square jammed into the guest window's top-left
    corner instead of a thin strip spanning its full width) without also
    having to drive the blocking mainloop() or a real QEMU owner window.
    """
    import tkinter as tk

    root = tk.Tk()
    root.title(title)
    root.overrideredirect(False)
    # NOT root.resizable(False, False) -- see _strip_resize_border's
    # docstring. That Tk call locks the window's min/max track size to
    # whatever Tk's own natural (pre-geometry) size was, and the real rect
    # follow() sets later would lose to that lock for the window's whole
    # life. The style it produces is stripped by hand instead.
    root.update_idletasks()

    # GetAncestor(GA_ROOT): Tk's winfo_id() is the widget's HWND, which is not
    # always the top-level one. Owning the wrong handle silently does
    # nothing at all -- no error, no effect -- so a failure to resolve it is
    # NOT a degraded mode to fall back from. It is treated as a hard failure:
    # do not attach, do not open a bar. Falling back to the widget handle
    # would produce a bar that looks fine but never floats above its owner
    # and never minimises with it, with no diagnostic trail at all -- exactly
    # the class of silent misfeature this whole design exists to remove.
    try:
        bar_hwnd = _user32().GetAncestor(root.winfo_id(), GA_ROOT)
    except Exception:      # noqa: BLE001
        bar_hwnd = None
    if bar_hwnd:
        _strip_resize_border(bar_hwnd)
    return root, bar_hwnd


def run_window_bar(identity, title=None, pid=None, on_stop=None):
    """Show the guest's window with our bar above it. Blocks until closed.

    Returns 0 when the bar ran, 2 when there was no window to attach to, 3 on
    a host where this is not implemented, and 4 when the bar's own top-level
    handle could not be resolved (see below) -- a distinct code from 2/3
    because it is neither "no window" nor "wrong platform".
    """
    if not IS_WINDOWS:
        sys.stderr.write(
            "window bar: implemented for Windows only; other hosts use the "
            "VNC viewer or the patched QEMU UI\n")
        return 3

    owner = hostwin.find_window(identity, timeout=20.0, pid=pid)
    if owner is None:
        sys.stderr.write(f"window bar: no QEMU window for '{identity}'\n")
        return 2

    root, bar_hwnd = _create_bar_window(title or f"omni: {identity}")
    bar = WindowBar(identity, pid=pid, on_stop=on_stop)
    if not bar_hwnd:
        root.destroy()
        sys.stderr.write(
            f"window bar: could not resolve the top-level window handle "
            f"for '{identity}' (GetAncestor failed); not attaching a bar "
            f"it could not actually own\n")
        return 4
    bar.own(bar_hwnd, owner)

    rect = hostwin.window_geometry(identity, pid=pid)
    if rect:
        bar.follow(rect)

    def on_delete():
        answer = bar.on_close(root)
        # A failed stop must NOT close the bar -- an open bar is the only
        # signal that reaches the user when this process has no console
        # (Task 5 spawns it detached), and destroying it here would look
        # exactly like a successful stop.
        if answer == "hide" or (answer == "stop" and not bar.stop_failed):
            root.destroy()

    root.protocol("WM_DELETE_WINDOW", on_delete)
    root.mainloop()
    return 0
