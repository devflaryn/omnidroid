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

# How often the bar re-reads the guest window's rect and re-aligns itself.
#
# A POLL, not SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE). The hook is the
# textbook answer and it is the wrong tool here: it delivers a cross-process
# callback on a thread of Windows' choosing, which then has to be marshalled
# into Tk's event loop before it may touch a single widget, and getting that
# wrong is a class of bug (a Tk call from the wrong thread) that shows up as
# an intermittent hang rather than an error. What it buys over a poll is
# sub-frame latency on a title bar.
#
# 60 ms (~16 looks a second) is the interval. One GetWindowRect is a few
# microseconds and the SetWindowPos only happens when the rect actually
# CHANGED, so an idle bar costs ~16 syscalls a second and nothing else, while
# a window being dragged or resized keeps its bar within about one frame at
# 60 Hz. Slower reads as the bar lagging behind the window; faster buys
# nothing an eye can see.
FOLLOW_POLL_MS = 60


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
        # The owner rect the bar is currently aligned to. It is what stops
        # the follow poll and the drag handler fighting each other: the poll
        # re-aligns only when the owner has moved to somewhere this is NOT,
        # and `drag_owner_to_bar` updates it the instant it moves the owner,
        # so the owner move the drag itself caused never reads as one the
        # poll has to chase.
        self.synced_owner_rect = None
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
                    # bar_geometry AGAIN, with the height Windows actually
                    # granted, rather than an open-coded `owner_top -
                    # actual_height`. That open-coded form skipped
                    # bar_geometry's top-of-screen clamp, and MEASURED on a
                    # live instance it put the bar at y=-14 against a guest
                    # window at y=26: the caption -- our title, our minimise,
                    # our X, the only close prompt that exists -- hanging off
                    # the top of the screen. One rule for where the bar goes,
                    # used twice, instead of two rules that disagree at the
                    # edge of the desktop.
                    #
                    # The clamp is not a fudge now that the bar drags the
                    # composite: it pins the bar at y=0, the <Configure> that
                    # move generates reads the difference as a drag, and the
                    # guest is nudged down to sit under it. Converges in one
                    # step (verified) and leaves the title bar reachable,
                    # which is the invariant that matters.
                    cx, cy, _cw, _ch = bar_geometry(owner_rect,
                                                    bar_height=actual_height)
                    u.SetWindowPos(self.bar_hwnd, 0, cx, cy, 0, 0,
                                  SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE)
            self.synced_owner_rect = tuple(owner_rect)
            return True
        except Exception:      # noqa: BLE001
            return False

    def owner_rect(self):
        """The guest window's (x, y, w, h) right now, or None if it is gone.

        Read straight off the owner HWND we already hold, NOT via
        `hostwin.window_geometry`: that re-finds the window by title/pid on
        every call, which enumerates every top-level window on the desktop
        and every one of their children. Fine once at startup, absurd
        sixteen times a second.
        """
        if self.owner_hwnd is None:
            return None
        try:
            return _window_rect(self.owner_hwnd)
        except Exception:      # noqa: BLE001
            return None

    def owner_is_alive(self):
        """False once the guest's window has been destroyed.

        The bar is an OWNED window, so Windows destroys it with its owner and
        this should never come back False in the normal case. It covers the
        gap where it does not fire (an owner that vanished without a clean
        destroy) rather than leaving a title bar captioning nothing.
        """
        if self.owner_hwnd is None:
            return False
        try:
            return bool(_user32().IsWindow(self.owner_hwnd))
        except Exception:      # noqa: BLE001
            return True     # cannot tell: never close the bar on a guess

    def owner_is_minimised(self):
        """Whether the guest window is iconic right now.

        A minimised window's GetWindowRect is Windows' off-screen parking
        position (-32000, -32000), not where the window will be when it comes
        back, so following it would move the bar somewhere meaningless and
        then have to undo it on restore. The bar minimises with its owner
        anyway -- that is one of the three properties ownership buys -- so
        there is nothing to follow while it is down.
        """
        if self.owner_hwnd is None:
            return False
        try:
            return bool(_user32().IsIconic(self.owner_hwnd))
        except Exception:      # noqa: BLE001
            return False

    def poll_follow(self):
        """One tick: re-align the bar if the guest window has moved or resized.

        Returns True when it moved the bar. Cheap on the common tick (one
        GetWindowRect, no move) -- see FOLLOW_POLL_MS.
        """
        if self.bar_hwnd is None or self.owner_is_minimised():
            return False
        rect = self.owner_rect()
        if rect is None or tuple(rect) == self.synced_owner_rect:
            return False
        return self.follow(rect)

    def drag_owner_to_bar(self):
        """The bar was dragged: move the guest window under it. Returns True
        when it moved something.

        THE BAR IS THE TITLE BAR, so dragging it has to move the window --
        that is what a title bar is for, and here it is also the only way the
        window can be moved at all. `apply_chrome` strips WS_CAPTION *and*
        WS_SYSMENU from QEMU's window, which leaves it with nothing to drag
        and no Alt+Space -> Move either; without this the composite was
        nailed to wherever QEMU first put it, and our bar could be dragged
        off on its own and never came back.

        Called from the bar's own <Configure>, so it runs for a native
        caption drag (Windows' own move loop, which is what actually moves
        this window), for a keyboard move, and for anything else that
        repositions the bar. It compares where the bar IS against where it
        BELONGS relative to the owner and moves the owner by the difference,
        which needs no drag-start bookkeeping and cannot drift.

        "Belongs" uses the bar's ACTUAL height, not BAR_HEIGHT: Windows
        enforces its own minimum caption height (measured 40px against a
        requested 34), and follow() already lands the bar's BOTTOM edge on
        the owner's TOP edge. Comparing against the requested height instead
        would read that correction as a drag of the difference and walk the
        guest window down the screen a few pixels per tick.
        """
        if self.bar_hwnd is None or self.owner_hwnd is None:
            return False
        if self.owner_is_minimised():
            return False
        try:
            bar = _window_rect(self.bar_hwnd)
        except Exception:      # noqa: BLE001
            return False
        owner = self.owner_rect()
        if bar is None or owner is None:
            return False
        dx = bar[0] - owner[0]
        dy = bar[1] - (owner[1] - bar[3])
        if dx == 0 and dy == 0:
            return False
        try:
            _user32().SetWindowPos(self.owner_hwnd, 0,
                                   owner[0] + dx, owner[1] + dy, 0, 0,
                                   SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE)
        except Exception:      # noqa: BLE001
            return False
        # Record where the owner now is BEFORE the poll can look, or the poll
        # reads the move this drag just made as one it has to chase and
        # yanks the bar back mid-drag.
        self.synced_owner_rect = (owner[0] + dx, owner[1] + dy,
                                  owner[2], owner[3])
        return True

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
            # `pid`, not the identity alone. hostwin matches the window TITLE
            # as a SUBSTRING, so "omni-farm3" also matches "omni-farm30" --
            # with several instances up, the X on one bar hid a different
            # user's window. Same defect, same fix as `cmd_view`'s own calls;
            # this one was simply never carried across, which is why
            # __init__ has stored `self.pid` all along without using it.
            #
            # timeout=2, not hostwin's 20 s default: this runs on the bar's
            # UI thread inside a Tk callback, so the default would freeze the
            # bar -- unrepaintable, unclickable -- for twenty seconds in
            # exactly the case where there is no window to find.
            hostwin.hide_qemu_window(self.identity, pid=self.pid, timeout=2)
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


def attach_follow(root, bar):
    """Keep the bar and the guest window locked together, both ways.

    Two mechanisms, because the composite can be moved from either end:

      * a `root.after` poll (FOLLOW_POLL_MS) re-aligns the bar when the GUEST
        window moves or resizes -- dragging its sizing border, a Windows snap,
        Win+arrow, anything;
      * the bar's own `<Configure>` moves the GUEST when the BAR is dragged,
        because the bar IS the title bar (drag_owner_to_bar).

    `<Configure>` rather than Tk button bindings: the bar's client area is
    ~0px tall (BAR_HEIGHT sits under Windows' own minimum caption height), so
    there is no widget under the pointer to bind to -- the drag happens on
    the real Win32 caption and Windows runs its own modal move loop for it.
    Tk's Windows window procedure calls Tcl_ServiceAll() on every message, so
    the binding still fires DURING that loop rather than only at the end.

    Returns the tick function, so a test can step the poll by hand instead of
    running a mainloop.
    """
    def tick():
        # The owner is gone: close, rather than leave a title bar captioning
        # nothing. Ownership normally does this for us (Windows destroys an
        # owned window with its owner, which is what lets `stop` clean the
        # bar up for free); this covers the case where it does not fire.
        if not bar.owner_is_alive():
            try:
                root.destroy()
            except Exception:      # noqa: BLE001
                pass
            return
        bar.poll_follow()
        try:
            root.after(FOLLOW_POLL_MS, tick)
        except Exception:      # noqa: BLE001 - the window went away mid-tick
            pass

    def on_configure(event):
        # Only the toplevel's own Configure, not a child widget's.
        if event.widget is root:
            bar.drag_owner_to_bar()

    root.bind("<Configure>", on_configure)
    root.after(FOLLOW_POLL_MS, tick)
    return tick


def run_window_bar(identity, title=None, pid=None, on_stop=None):
    """Show the guest's window with our bar above it. Blocks until closed.

    Returns 0 when the bar ran, 2 when there was no window to attach to, 3 on
    a host where this is not implemented, 4 when the bar's own top-level
    handle could not be resolved (see below), and 5 when the ownership call
    itself failed -- each distinct, because "no window", "wrong platform",
    "no handle" and "handle, but it would not own" need different answers.
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
    # own()'s return value is NOT discardable. On failure `bar.bar_hwnd` is
    # never set, so follow() returns False on its first line and the bar sits
    # at Tk's own default size -- reproducing the exact 216x239 square
    # task-9-report.md measured and fixed -- AND it is not owned, so `stop`
    # destroying QEMU's window no longer takes it with it and the user is
    # left with an orphan bar over nothing. A failed GetAncestor is already a
    # hard failure here for precisely those reasons; this is the same failure
    # one call later and gets the same treatment.
    if not bar.own(bar_hwnd, owner):
        root.destroy()
        sys.stderr.write(
            f"window bar: could not make the bar an owned window of the "
            f"QEMU window for '{identity}' (SetWindowLongPtr GWLP_HWNDPARENT "
            f"failed); not opening a bar that would not float above the "
            f"guest, would not minimise with it, and would outlive it\n")
        return 5

    # Design spec 3a/3b: the strip is the window that HAS a caption now, so
    # the DWM styling the spec asks for belongs on it. Best-effort by
    # contract -- an older Windows silently keeps square corners and a light
    # caption, which is chrome, not function.
    hostwin.apply_dwm_style(bar_hwnd)

    # The OWNER HANDLE we already resolved, not window_geometry(identity).
    #
    # MEASURED on real windows, 2026-08-16: window_geometry() re-finds the
    # window by title, hostwin matches the title as a SUBSTRING, and by this
    # point OUR OWN window exists and is called "omni: <identity>" -- which
    # contains <identity>. It can therefore hand back the BAR's rect, and
    # follow() then aligns the bar to itself: a 216px-wide strip, the exact
    # shape of the 216x239 bug task-9-report.md fixed, and (now that the bar
    # drags the composite) the guest window gets pulled under it as well.
    # Reproduced by running run_window_bar() against a real stand-in guest
    # window WITHOUT a pid, which is the case the pid filter cannot save.
    #
    # `view` always passes the QEMU pid, so this never fired in the product;
    # it fired the moment anything called this the way its own signature says
    # it may. The handle is unambiguous, needs no enumeration at all, and is
    # what every later re-alignment already uses.
    rect = bar.owner_rect()
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
    # AFTER the first follow(), so the poll's very first tick compares
    # against a rect the bar is already aligned to and the <Configure> that
    # follow() itself generated cannot be read as a user drag.
    attach_follow(root, bar)
    root.mainloop()
    return 0
