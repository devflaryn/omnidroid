"""Host QEMU's own window INSIDE our viewer, instead of copying pixels.

The problem this solves is specific and it is the whole reason the product had
to choose between speed and a viewer on Windows:

  * QEMU refuses `-vnc` beside a windowed GL display, and
  * on Windows the only GL context virglrenderer can actually serve a scanout
    from is the WGL one attached to a GTK window (`egl-headless` and
    `dbus,gl=on` both go through ANGLE at ES 2.0 and the guest's SET_SCANOUT is
    rejected -- measured, 602 rejections in one boot).

So on Windows a GPU-accelerated instance has a window and no VNC server. Every
way of getting those pixels into our own viewer by COPYING them -- VNC,
screendump, `screenrecord` over adb -- either does not exist on that boot or
costs an encode, a transfer and a decode per frame, on a guest CPU that is
already the bottleneck.

Reparenting costs none of that. `SetParent` makes QEMU's window a child of our
window: the same GL surface, drawn by the same GPU, composited by Windows into
our frame. Zero copies, zero encode, and input goes straight into the guest's
usb-tablet/usb-kbd instead of being synthesised from an RFB event. What the
user sees is our viewer -- our title, our chrome -- with the guest inside it.

WINDOWS ONLY, and that is not a gap: it is the only platform where the window
is forced. Linux renders windowless through `egl-headless` and macOS has no
virgl at all yet, so both keep the VNC viewer, which works there.

THE OTHER TWO PLATFORMS USE THE VNC VIEWER, AND THAT IS THE RIGHT ANSWER --
not a port waiting to be written. Say so out loud, because "embed the window
there too" is the obvious idea and it is wrong in a different way on each:

  * macOS has no public API for it at all. An NSWindow cannot adopt a view
    from another process; the only thing that does is the private
    CGSSetWindowParent, which is unsupported, unsigned-code-hostile and free
    to break on any system update. There is no measurement to take here.
  * Linux does not need it. `egl-headless` presents there, so QEMU renders on
    the GPU AND serves a framebuffer at the same time -- exactly the pair
    Windows refuses -- and a GPU boot on Linux therefore has no window to
    reparent. XEmbed would work if there were one; there is not.

Hiding is a separate question with a separate answer: `hostwin.py` hides the
real QEMU window on all three platforms where one exists, because that is
strictly better than a copy wherever it is possible. Embedding is the part
that stops at the Windows boundary.

Undo matters as much as the embed: a QEMU window left parented to a viewer that
has closed is a window with no title bar and no way to reach it. `release()`
always runs -- on clean exit, on error, and from the atexit hook -- and puts the
window back on the desktop, hidden, exactly as it was found.
"""
import sys
import time

from omnidroid.config import IS_LINUX, IS_MACOS, IS_WINDOWS

GWL_STYLE = -16
WS_CHILD = 0x40000000
WS_POPUP = 0x80000000
WS_CAPTION = 0x00C00000
WS_THICKFRAME = 0x00040000
WS_MINIMIZEBOX = 0x00020000
WS_MAXIMIZEBOX = 0x00010000
WS_VISIBLE = 0x10000000

SWP_NOZORDER = 0x0004
SWP_NOACTIVATE = 0x0010
SWP_FRAMECHANGED = 0x0020
SWP_SHOWWINDOW = 0x0040


def available():
    """Whether embedding is possible on this host at all."""
    return IS_WINDOWS


def available_reason():
    """Why embedding is not offered here, or "" when it is.

    Kept next to `available()` so the answer travels with the check: a bare
    False sends the next reader looking for a bug, and there is none to find --
    see the module docstring. Callers print this instead of "Windows only",
    which is true and tells nobody anything.
    """
    if IS_WINDOWS:
        return ""
    if IS_MACOS:
        return ("macOS has no public API to embed another process's window "
                "(only the private CGSSetWindowParent), so the viewer "
                "connects to QEMU's VNC server instead")
    if IS_LINUX:
        return ("a GPU boot on Linux is windowless -- `egl-headless` presents "
                "there, so QEMU renders on the GPU and serves VNC at the same "
                "time -- so there is no window to embed and no reason to")
    return "embedding is implemented for Windows only"


class EmbeddedQemuWindow:
    """QEMU's window, reparented into a container window. Reversible.

    `container` is a native window handle (Tk gives one via `winfo_id()`).
    Nothing here raises: `attach()` reports False and the caller falls back to
    the VNC viewer, which is the correct behaviour on every host that is not
    Windows and on any Windows host where the window could not be found.
    """

    def __init__(self, identity, pid=None):
        self.identity = identity
        self.pid = pid
        self.hwnd = None
        self.container = None
        self._old_style = None
        self._old_parent = None

    # -- ctypes helpers, imported lazily so this module is importable anywhere
    @staticmethod
    def _user32():
        import ctypes
        return ctypes.windll.user32

    @classmethod
    def _get_style(cls, hwnd):
        u = cls._user32()
        if hasattr(u, "GetWindowLongPtrW"):
            return u.GetWindowLongPtrW(hwnd, GWL_STYLE)
        return u.GetWindowLongW(hwnd, GWL_STYLE)

    @classmethod
    def _set_style(cls, hwnd, style):
        u = cls._user32()
        if hasattr(u, "SetWindowLongPtrW"):
            return u.SetWindowLongPtrW(hwnd, GWL_STYLE, style)
        return u.SetWindowLongW(hwnd, GWL_STYLE, style)

    def attach(self, container, width=None, height=None, timeout=20.0):
        """Make QEMU's window a child of `container`. True if it took."""
        if not available():
            return False
        from omnidroid import hostwin
        hwnd = hostwin.find_window(self.identity, timeout=timeout,
                                   pid=self.pid)
        if hwnd is None:
            return False
        try:
            u = self._user32()
            self._old_style = self._get_style(hwnd)
            self._old_parent = u.GetParent(hwnd)
            # A top-level window keeps its caption and resizing frame when
            # reparented, so without this the guest appears inside our viewer
            # wearing a second title bar it is impossible to click.
            style = self._old_style
            style &= ~(WS_POPUP | WS_CAPTION | WS_THICKFRAME
                       | WS_MINIMIZEBOX | WS_MAXIMIZEBOX)
            style |= WS_CHILD | WS_VISIBLE
            self._set_style(hwnd, style)
            if not u.SetParent(hwnd, container):
                self._set_style(hwnd, self._old_style)
                return False
            self.hwnd = hwnd
            self.container = container
            if width and height:
                self.resize(width, height)
            u.ShowWindow(hwnd, 4)                       # SW_SHOWNOACTIVATE
            return True
        except Exception:      # noqa: BLE001 - fall back to the VNC viewer
            return False

    def resize(self, width, height):
        """Fit the guest's window to the container. Never raises."""
        if self.hwnd is None:
            return False
        try:
            self._user32().SetWindowPos(
                self.hwnd, 0, 0, 0, int(width), int(height),
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
                | SWP_SHOWWINDOW)
            return True
        except Exception:      # noqa: BLE001
            return False

    def focus(self):
        """Give the guest the keyboard. Tk owns our container's focus, so a
        click in the viewer has to be forwarded or typing goes nowhere."""
        if self.hwnd is None:
            return False
        try:
            self._user32().SetFocus(self.hwnd)
            return True
        except Exception:      # noqa: BLE001
            return False

    def release(self, hide=True):
        """Put QEMU's window back on the desktop, hidden. Idempotent.

        Called on every exit path. A window left parented to a destroyed
        container is unreachable -- no title bar, no taskbar entry, and the
        instance is still running behind it."""
        if self.hwnd is None:
            return False
        try:
            u = self._user32()
            u.SetParent(self.hwnd, self._old_parent or 0)
            if self._old_style is not None:
                self._set_style(self.hwnd, self._old_style)
            u.ShowWindow(self.hwnd, 0 if hide else 4)   # SW_HIDE / NOACTIVATE
            u.SetWindowPos(self.hwnd, 0, 0, 0, 0, 0,
                           SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED)
            return True
        except Exception:      # noqa: BLE001
            return False
        finally:
            self.hwnd = None
            self.container = None


def run_embedded_viewer(identity, title=None, size=None, pid=None):
    """Open a Tk window with the guest's QEMU window living inside it.

    Blocks until the window is closed, then hands QEMU's window back. Returns 0
    on success and non-zero when embedding was not possible, so the caller can
    fall back to the RFB viewer without having to duplicate the checks.
    """
    if not available():
        sys.stderr.write(f"embedded viewer: not available here — "
                         f"{available_reason()}\n")
        return 3
    import tkinter as tk

    root = tk.Tk()
    root.title(title or f"omni: {identity}")
    root.configure(bg="black")
    width, height = size or (1280, 800)
    root.geometry(f"{width}x{height}")
    host = tk.Frame(root, bg="black", width=width, height=height)
    host.pack(fill="both", expand=True)
    root.update_idletasks()          # the frame needs a real hwnd first

    win = EmbeddedQemuWindow(identity, pid=pid)
    if not win.attach(host.winfo_id(), width, height):
        root.destroy()
        sys.stderr.write(
            f"embedded viewer: no QEMU window found for '{identity}'. On a "
            f"boot that renders windowless there is nothing to embed — use "
            f"the VNC viewer instead.\n")
        return 2

    def on_resize(event):
        if event.widget is host:
            win.resize(event.width, event.height)

    def on_click(_event):
        win.focus()

    host.bind("<Configure>", on_resize)
    host.bind("<Button-1>", on_click)
    root.bind("<FocusIn>", lambda _e: win.focus())

    closing = {"done": False}

    def on_close():
        if closing["done"]:
            return
        closing["done"] = True
        # ORDER MATTERS: hand the window back BEFORE destroying the container,
        # or it is reparented to a window that no longer exists and vanishes
        # with the instance still running behind it.
        win.release(hide=True)
        root.destroy()

    root.protocol("WM_DELETE_WINDOW", on_close)
    try:
        root.mainloop()
    finally:
        win.release(hide=True)
    return 0
