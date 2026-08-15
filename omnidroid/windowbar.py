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

WINDOWS FIRST. The X11 backend is written but UNVERIFIED (no Linux host in
this setup); macOS gets this from the QEMU patch, because it has no public API
to restyle or own another process's NSWindow.
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


def _user32():
    import ctypes
    return ctypes.windll.user32


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
            _user32().SetWindowPos(self.bar_hwnd, 0, x, y, width, height,
                                   SWP_NOZORDER | SWP_NOACTIVATE)
            return True
        except Exception:      # noqa: BLE001
            return False

    def on_close(self, parent=None):
        """The X was clicked. Returns "hide", "stop" or "cancel"."""
        answer = _ask_close(parent)
        try:
            if answer == "hide":
                hostwin.hide_qemu_window(self.identity)
            elif answer == "stop" and self.on_stop:
                self.on_stop(self.identity)
        except Exception:      # noqa: BLE001 - a caller-supplied hook (or a
            # host call) must not blow up the close prompt's caller.
            pass
        return answer


def run_window_bar(identity, title=None, pid=None, on_stop=None):
    """Show the guest's window with our bar above it. Blocks until closed.

    Returns 0 when the bar ran, 2 when there was no window to attach to, and
    3 on a host where this is not implemented.
    """
    if not IS_WINDOWS:
        sys.stderr.write(
            "window bar: implemented for Windows only; other hosts use the "
            "VNC viewer or the patched QEMU UI\n")
        return 3
    import tkinter as tk

    owner = hostwin.find_window(identity, timeout=20.0, pid=pid)
    if owner is None:
        sys.stderr.write(f"window bar: no QEMU window for '{identity}'\n")
        return 2

    root = tk.Tk()
    root.title(title or f"omni: {identity}")
    root.overrideredirect(False)
    root.resizable(False, False)
    root.update_idletasks()

    bar = WindowBar(identity, pid=pid, on_stop=on_stop)
    # GetAncestor(GA_ROOT): Tk's winfo_id() is the widget's HWND, which is not
    # always the top-level one. Owning the wrong handle silently does nothing.
    try:
        bar_hwnd = _user32().GetAncestor(root.winfo_id(), GA_ROOT)
    except Exception:      # noqa: BLE001 - fall back to the widget's own
        # handle rather than crash the bar; own() still guards IS_WINDOWS and
        # a wrong handle only costs the floating/minimise behaviour, not a
        # boot.
        bar_hwnd = root.winfo_id()
    bar.own(bar_hwnd, owner)

    rect = hostwin.window_geometry(identity, pid=pid)
    if rect:
        bar.follow(rect)

    def on_delete():
        if bar.on_close(root) in ("hide", "stop"):
            root.destroy()

    root.protocol("WM_DELETE_WINDOW", on_delete)
    root.mainloop()
    return 0
