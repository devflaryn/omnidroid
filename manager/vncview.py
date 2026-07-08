#!/usr/bin/env python3
"""omnidroid self-contained VNC viewer — a Python-managed window that works
identically on Windows/macOS/Linux (no OS screen-sharing app, no external VNC
client).

A minimal RFB (VNC) client feeds a Tkinter window:
  - real-time framebuffer (Raw + CopyRect + DesktopSize pseudo-encoding),
  - full mouse control (move, left/middle/right, wheel),
  - keyboard forwarding (X11 keysyms).

It connects to a QEMU built-in VNC server on 127.0.0.1 (no auth — safe only
on the loopback bind). Network runs in a daemon thread; all rendering and Tk
calls happen on the main thread (Tk is not thread-safe). Socket writes are
serialized with a lock so input events (Tk thread) and update-requests
(net thread) never interleave.

Deps: tkinter + Pillow (Pillow does the fast framebuffer->widget conversion).
Run standalone:  python -m manager.vncview --host 127.0.0.1 --port 18001 \
                     --title "omni: dave"
"""
import argparse
import socket
import struct
import sys
import threading
import time

# ---------- RFB protocol constants ----------
_SET_PIXEL_FORMAT = 0
_SET_ENCODINGS = 2
_FB_UPDATE_REQUEST = 3
_KEY_EVENT = 4
_POINTER_EVENT = 5

_ENC_RAW = 0
_ENC_COPYRECT = 1
_ENC_DESKTOPSIZE = -223

# X11 keysyms for non-printable keys (Tk event.keysym -> keysym code).
_KEYSYMS = {
    "Return": 0xFF0D, "KP_Enter": 0xFF8D, "BackSpace": 0xFF08, "Tab": 0xFF09,
    "Escape": 0xFF1B, "Delete": 0xFFFF, "Home": 0xFF50, "End": 0xFF57,
    "Left": 0xFF51, "Up": 0xFF52, "Right": 0xFF53, "Down": 0xFF54,
    "Prior": 0xFF55, "Next": 0xFF56, "Insert": 0xFF63,
    "F1": 0xFFBE, "F2": 0xFFBF, "F3": 0xFFC0, "F4": 0xFFC1, "F5": 0xFFC2,
    "F6": 0xFFC3, "F7": 0xFFC4, "F8": 0xFFC5, "F9": 0xFFC6, "F10": 0xFFC7,
    "F11": 0xFFC8, "F12": 0xFFC9,
    "Shift_L": 0xFFE1, "Shift_R": 0xFFE2, "Control_L": 0xFFE3,
    "Control_R": 0xFFE4, "Alt_L": 0xFFE9, "Alt_R": 0xFFEA,
    "Meta_L": 0xFFE7, "Meta_R": 0xFFE8, "Super_L": 0xFFEB, "Super_R": 0xFFEC,
    "Caps_Lock": 0xFFE5, "space": 0x0020,
}


class RFBClient:
    """Minimal RFB 3.x client. Maintains a BGRX framebuffer bytearray that a
    Tk front-end reads. Thread model: recv loop in its own thread; sends
    (input + update requests) guarded by _wlock."""

    def __init__(self, host, port):
        self.host, self.port = host, port
        self.sock = None
        self.width = self.height = 0
        self.fb = bytearray()          # width*height*4, BGRX
        self.name = ""
        self._wlock = threading.Lock()
        self.dirty = threading.Event()
        self.closed = threading.Event()
        self.error = None

    # --- low-level io ---
    def _recvn(self, n):
        buf = bytearray()
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("server closed connection")
            buf += chunk
        return bytes(buf)

    def _send(self, data):
        with self._wlock:
            self.sock.sendall(data)

    # --- handshake ---
    def connect(self):
        self.sock = socket.create_connection((self.host, self.port),
                                             timeout=10)
        self.sock.settimeout(None)
        server_ver = self._recvn(12)          # e.g. b"RFB 003.008\n"
        try:
            major, minor = int(server_ver[4:7]), int(server_ver[8:11])
        except ValueError:
            major, minor = 3, 8
        proto = b"RFB 003.008\n" if (major, minor) >= (3, 8) else \
            (b"RFB 003.007\n" if (major, minor) >= (3, 7) else b"RFB 003.003\n")
        self.sock.sendall(proto)
        self._security(proto)
        self.sock.sendall(struct.pack("!B", 1))     # ClientInit: shared=1
        self._server_init()
        self._set_pixel_format()
        self._set_encodings([_ENC_RAW, _ENC_COPYRECT, _ENC_DESKTOPSIZE])
        self.request_update(incremental=False)

    def _security(self, proto):
        if proto >= b"RFB 003.007\n":
            n = self._recvn(1)[0]
            if n == 0:                          # failure
                reason = self._recvn(struct.unpack("!I", self._recvn(4))[0])
                raise ConnectionError(f"VNC security failed: {reason!r}")
            types = self._recvn(n)
            if 1 not in types:                  # 1 = None
                raise ConnectionError(
                    "server requires VNC auth/password; this viewer only "
                    "supports no-auth localhost servers (type 'None'). "
                    f"offered: {list(types)}")
            self.sock.sendall(struct.pack("!B", 1))
        else:                                    # RFB 3.3: server dictates
            sec = struct.unpack("!I", self._recvn(4))[0]
            if sec != 1:
                raise ConnectionError(f"server security type {sec} "
                                      "unsupported (need None)")
        # SecurityResult (present for None in 3.8; in 3.3 too)
        if proto >= b"RFB 003.008\n":
            res = struct.unpack("!I", self._recvn(4))[0]
            if res != 0:
                raise ConnectionError("VNC auth rejected")

    def _server_init(self):
        w, h = struct.unpack("!HH", self._recvn(4))
        self._recvn(16)                          # server pixel format (ignored)
        nlen = struct.unpack("!I", self._recvn(4))[0]
        self.name = self._recvn(nlen).decode("latin-1", "replace")
        self._resize(w, h)

    def _resize(self, w, h):
        self.width, self.height = w, h
        self.fb = bytearray(w * h * 4)           # BGRX, opaque black

    def _set_pixel_format(self):
        # 32bpp, depth 24, little-endian, true-colour, RGB shifts 16/8/0 ->
        # in-memory bytes per pixel are B,G,R,x (BGRX) — matched by PIL's
        # "BGRX" raw decoder for correct colours on any QEMU build.
        pf = struct.pack("!BBBB HHH BBB xxx",
                         32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
        self._send(struct.pack("!Bxxx", _SET_PIXEL_FORMAT) + pf)

    def _set_encodings(self, encs):
        msg = struct.pack("!BxH", _SET_ENCODINGS, len(encs))
        msg += b"".join(struct.pack("!i", e) for e in encs)
        self._send(msg)

    # --- client -> server messages ---
    def request_update(self, incremental=True):
        self._send(struct.pack("!BBHHHH", _FB_UPDATE_REQUEST,
                               1 if incremental else 0,
                               0, 0, self.width, self.height))

    def pointer(self, button_mask, x, y):
        x = max(0, min(x, self.width - 1))
        y = max(0, min(y, self.height - 1))
        self._send(struct.pack("!BBHH", _POINTER_EVENT, button_mask & 0xFF,
                               x, y))

    def key(self, keysym, down):
        self._send(struct.pack("!BBHI", _KEY_EVENT, 1 if down else 0, 0,
                               keysym & 0xFFFFFFFF))

    # --- server -> client loop ---
    def run(self):
        try:
            while not self.closed.is_set():
                msg_type = self._recvn(1)[0]
                if msg_type == 0:
                    self._framebuffer_update()
                    self.request_update(incremental=True)
                elif msg_type == 1:              # SetColourMapEntries
                    self._recvn(3)
                    n = struct.unpack("!H", self._recvn(2))[0]
                    self._recvn(n * 6)
                elif msg_type == 2:              # Bell
                    pass
                elif msg_type == 3:              # ServerCutText
                    self._recvn(3)
                    n = struct.unpack("!I", self._recvn(4))[0]
                    self._recvn(n)
                else:
                    raise ConnectionError(f"unknown server msg {msg_type}")
        except Exception as e:                   # noqa: BLE001
            if not self.closed.is_set():
                self.error = e
        finally:
            self.closed.set()
            try:
                self.sock.close()
            except Exception:
                pass

    def _framebuffer_update(self):
        self._recvn(1)                           # padding
        nrects = struct.unpack("!H", self._recvn(2))[0]
        for _ in range(nrects):
            x, y, w, h, enc = struct.unpack("!HHHHi", self._recvn(12))
            if enc == _ENC_RAW:
                self._raw_rect(x, y, w, h)
            elif enc == _ENC_COPYRECT:
                sx, sy = struct.unpack("!HH", self._recvn(4))
                self._copy_rect(x, y, w, h, sx, sy)
            elif enc == _ENC_DESKTOPSIZE:
                self._resize(w, h)
                self.request_update(incremental=False)
                break
            else:
                raise ConnectionError(f"unsupported encoding {enc}")
        self.dirty.set()

    def _raw_rect(self, x, y, w, h, ):
        data = self._recvn(w * h * 4)
        row = w * 4
        fbw = self.width * 4
        mv = memoryview(self.fb)
        for r in range(h):
            dst = (y + r) * fbw + x * 4
            mv[dst:dst + row] = data[r * row:(r + 1) * row]

    def _copy_rect(self, x, y, w, h, sx, sy):
        fbw = self.width * 4
        row = w * 4
        src = bytearray(self.fb)                 # snapshot (regions may overlap)
        mv = memoryview(self.fb)
        rows = range(h - 1, -1, -1) if sy < y else range(h)
        for r in rows:
            s = (sy + r) * fbw + sx * 4
            d = (y + r) * fbw + x * 4
            mv[d:d + row] = src[s:s + row]

    def close(self):
        self.closed.set()
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass


def run_viewer(host, port, title):
    """Open the Tk window and drive the RFB client. Blocks until the window
    is closed. Returns 0 on clean exit, non-zero on connection error."""
    import tkinter as tk
    try:
        from PIL import Image, ImageTk
    except Exception:
        sys.stderr.write(
            "error: Pillow is required for the built-in viewer.\n"
            "  install it:  python3 -m pip install pillow\n"
            "  (or use a native client: omni view <acct> --native)\n")
        return 3

    client = RFBClient(host, port)
    try:
        client.connect()
    except Exception as e:                       # noqa: BLE001
        sys.stderr.write(f"error: cannot connect to VNC {host}:{port}: {e}\n")
        return 2

    root = tk.Tk()
    root.title(title or f"omni VNC {host}:{port}")
    root.configure(bg="black")
    label = tk.Label(root, bg="black", bd=0, highlightthickness=0)
    label.pack()
    state = {"button_mask": 0, "photo": None, "img": None}

    net = threading.Thread(target=client.run, daemon=True)
    net.start()

    # --- input handlers (main thread) -> RFB ---
    def send_pointer(event):
        try:
            client.pointer(state["button_mask"], event.x, event.y)
        except Exception:
            pass

    def on_press(mask):
        def h(event):
            state["button_mask"] |= mask
            send_pointer(event)
        return h

    def on_release(mask):
        def h(event):
            state["button_mask"] &= ~mask
            send_pointer(event)
        return h

    def on_wheel(delta_btn):
        def h(event):
            # a wheel "click": press+release the wheel button bit
            try:
                client.pointer(state["button_mask"] | delta_btn, event.x,
                               event.y)
                client.pointer(state["button_mask"], event.x, event.y)
            except Exception:
                pass
        return h

    def keysym_for(event):
        if event.keysym in _KEYSYMS:
            return _KEYSYMS[event.keysym]
        if event.char and len(event.char) == 1 and 0x20 <= ord(event.char):
            return ord(event.char)
        # letters/digits without a printable char (e.g. with modifiers)
        ks = event.keysym
        if len(ks) == 1:
            return ord(ks)
        return None

    def on_key(down):
        def h(event):
            ks = keysym_for(event)
            if ks is not None:
                try:
                    client.key(ks, down)
                except Exception:
                    pass
        return h

    label.bind("<Motion>", send_pointer)
    label.bind("<Button-1>", on_press(1))
    label.bind("<ButtonRelease-1>", on_release(1))
    label.bind("<Button-2>", on_press(2))
    label.bind("<ButtonRelease-2>", on_release(2))
    label.bind("<Button-3>", on_press(4))
    label.bind("<ButtonRelease-3>", on_release(4))
    label.bind("<Button-4>", on_wheel(8))        # X11 wheel up
    label.bind("<Button-5>", on_wheel(16))       # X11 wheel down
    label.bind("<MouseWheel>",                    # Win/mac wheel
               lambda e: on_wheel(8 if e.delta > 0 else 16)(e))
    root.bind("<KeyPress>", on_key(True))
    root.bind("<KeyRelease>", on_key(False))
    label.focus_set()

    def tick():
        if client.closed.is_set():
            err = client.error
            root.destroy()
            if err:
                sys.stderr.write(f"viewer: connection ended: {err}\n")
            return
        if client.dirty.is_set() and client.width:
            client.dirty.clear()
            try:
                img = Image.frombytes("RGB", (client.width, client.height),
                                      bytes(client.fb), "raw", "BGRX")
                photo = ImageTk.PhotoImage(img)
                label.configure(image=photo)
                state["photo"] = photo           # keep a ref (Tk GCs images)
            except Exception:
                pass
        root.after(40, tick)                     # ~25 fps cap

    def on_close():
        client.close()
        root.destroy()
    root.protocol("WM_DELETE_WINDOW", on_close)
    root.after(40, tick)
    root.mainloop()
    client.close()
    return 0


def main(argv=None):
    p = argparse.ArgumentParser(prog="vncview",
                                description="omnidroid self-contained VNC "
                                            "viewer (Tk + RFB)")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--title", default=None)
    a = p.parse_args(argv)
    return run_viewer(a.host, a.port, a.title)


if __name__ == "__main__":
    sys.exit(main())
