"""Mouse-look that cannot escape the window: the GAME decides, the host obeys.

THE PROBLEM THIS CLOSES. The guest's pointing device is an absolute tablet,
so the host pointer maps 1:1 onto the guest screen -- exactly right for
clicking, and exactly wrong for right-drag camera rotation. Roblox handles
that drag with `UserInputService.MouseBehavior = LockCurrentPosition`: its
own cursor stays put and it turns the camera from the deltas between the
absolute positions it keeps receiving. Meanwhile the HOST pointer (which the
user cannot see -- the engine blanks it in a place) keeps walking across the
screen. Two symptoms follow, both reported by users: it reaches the window
edge and the rotation simply stops, and the moment the button is released
the game's cursor teleports to wherever the host pointer ended up.

WHY THE HOST CANNOT SIMPLY LOCK ON RIGHT-BUTTON-DOWN. A right-drag over a
GUI must NOT rotate anything -- the game's own camera script decides, per
click, whether the input is a camera drag (MouseBehavior changes) or a GUI
interaction (it does not). Any host-side rule keyed on the button would get
that wrong on every GUI. So the signal is the game's own property, read from
inside the game.

THE SHAPE OF THE FIX, three parts that only work together:

  1. In-game (this file's LUA_SCRIPT, pushed through the autoexec channel
     like any user script): watch `MouseBehavior` and tell the host over
     HTTP -- `http://10.0.2.2:<port>/lock?on=1|0`, 10.0.2.2 being slirp's
     alias for the host's loopback -- whenever it leaves or returns to
     Default. One request per change, nothing per frame.
  2. Host (`serve` below, a thread in the per-instance window-lock process,
     which lives exactly as long as the window): on lock, flip QEMU into
     relative mode with the `omni-pointer-lock` QMP command (qemu-patches/
     0013: host pointer hidden, grabbed, re-centred, clipped to the window,
     deltas delivered on a virtio-mouse) and raise the guest system property
     `omni.mouse.lock`; on unlock, undo both -- QEMU warps the host pointer
     back to where the grab began, which is where the game's cursor stayed.
  3. In the APK (the `-lock` build of the offset): the client's Java input
     layer requests Android POINTER CAPTURE while that property is set, so
     Android delivers relative motion straight to the game and its own
     pointer never walks to the screen edge either. Without capture the
     virtio-mouse would move Android's pointer, which clamps at the panel
     edge exactly like the host's did.

FAILS TOWARDS THE OLD BEHAVIOUR. No patched QEMU, no lock server, no
patched APK, a dead HTTP request: each one leaves the pre-fix picture (an
absolute pointer), never a stuck grab -- QEMU releases the clip on focus
loss, and every unlock request is honoured even when no lock was seen.
"""
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

# The host-side listener's port, derived from the instance's QMP port so it is
# unique per instance for the same reason the port triple is (engine.
# allocate_ports). 19001.. on a default config; nothing else in the product
# lives there.
PORT_OFFSET = 2000
GUEST_HOST = "10.0.2.2"          # slirp: the host's loopback, seen from the guest
PROP = "omni.mouse.lock"         # read by the patched client on every motion

# Runs inside the game via the autoexec channel. `__OMNI_LOCK_URL__` is filled
# in per launch (lock_url). Executors expose an HTTP call under one of several
# names; game:HttpGet is the one every build has.
LUA_SCRIPT = r'''-- OMNI MOUSE LOCK -- written by Omni Executor for this launch, not a user script.
-- Tells the host when the game locks the mouse (right-drag camera, first
-- person) so the real pointer is held inside the window while it does.
local UIS = game:GetService("UserInputService")
local RunService = game:GetService("RunService")
local URL = "__OMNI_LOCK_URL__"
local req = (syn and syn.request) or request or http_request or (http and http.request)
local function send(on)
    local u = URL .. "?on=" .. (on and "1" or "0")
    task.spawn(function()
        pcall(function()
            if req then req({Url = u, Method = "GET"}) else game:HttpGet(u) end
        end)
    end)
end
local last = nil
local function check()
    local locked = UIS.MouseBehavior ~= Enum.MouseBehavior.Default
    if locked ~= last then
        last = locked
        send(locked)
    end
end
-- a heartbeat while this script is alive: it only runs inside a place (the
-- executor loads autoexec at session start), and it dies with the place, so
-- "pings still arriving" is the cleanest "in a place" the host can get.
task.spawn(function()
    while true do
        pcall(function()
            local u = URL .. "?on=" .. (last and "1" or "0") .. "&alive=1"
            if req then req({Url = u, Method = "GET"}) else game:HttpGet(u) end
        end)
        task.wait(2)
    end
end)
pcall(function() UIS:GetPropertyChangedSignal("MouseBehavior"):Connect(check) end)
-- the engine resets MouseBehavior itself on GUI/teleport/menu events without
-- always firing the changed signal, so poll too: a compare per frame, a
-- request only on change.
RunService.Heartbeat:Connect(check)
check()
'''

SCRIPT_NAME = "00_omni_mouselock.lua"


def lock_port(acct):
    """The host listener's port for this instance. Pure."""
    return int(acct["qmp_port"]) + PORT_OFFSET


def lock_url(acct):
    """What the in-game script calls, as the guest sees it. Pure."""
    return f"http://{GUEST_HOST}:{lock_port(acct)}/lock"


def script_for(acct):
    """The autoexec entry for this launch: {name, body}. Pure."""
    return {"name": SCRIPT_NAME,
            "body": LUA_SCRIPT.replace("__OMNI_LOCK_URL__", lock_url(acct))}


# Last heartbeat from the in-game script, by instance name (time.monotonic).
# hostcursor.watch reads it: a fresh ping is proof the client is in a place,
# independent of the log heuristics.
HEARTBEAT_SECS = 2.0
_last_alive = {}


def note_alive(name, now=None):
    _last_alive[name] = time.monotonic() if now is None else now


def in_place_by_heartbeat(name, now=None, stale=HEARTBEAT_SECS * 3):
    """True while pings keep coming, None once they stop (unknown, not
    'left' -- the executor may just be unhealthy). Pure given `now`."""
    t = _last_alive.get(name)
    if t is None:
        return None
    now = time.monotonic() if now is None else now
    return True if now - t <= stale else None


def parse_request(path):
    """(lock, alive) from a request path. Pure.

    `lock` is True/False for /lock?on=1|0 and None otherwise; `alive` is
    True when the request carries alive=1 (the heartbeat), which may ride
    on a lock request or come alone. (None, False) for anything else."""
    parsed = urllib.parse.urlparse(path)
    if parsed.path.rstrip("/") != "/lock":
        return None, False
    q = urllib.parse.parse_qs(parsed.query)
    alive = q.get("alive", [""])[0] == "1"
    on = q.get("on", [""])[0]
    if on in ("1", "true", "on"):
        return True, alive
    if on in ("0", "false", "off"):
        return False, alive
    return None, alive


def apply(acct, locked, log=print):
    """Push one lock state to QEMU and to the guest. Never raises.

    Order matters on the way OUT: the guest property drops first, so the
    client's next captured event releases Android's capture, then QEMU
    lets go of the host pointer (warping it back to where the grab began).
    On the way IN the QMP lock goes first -- it is the part the user sees --
    and the property follows on the adb round trip."""
    from omnidroid import engine, qemu_proc
    label = f"mouselock {acct.get('name')}"

    def prop(v):
        try:
            engine.root_shell(acct, f"setprop {PROP} {v}", timeout=8)
        except Exception as e:      # noqa: BLE001
            log(f"[{label}] setprop {PROP} {v} failed: {e}")

    def qmp_lock(v):
        r = qemu_proc.qmp(acct, "omni-pointer-lock", {"enabled": bool(v)},
                          timeout=4)
        if not r or "error" in r:
            log(f"[{label}] omni-pointer-lock {v}: QEMU said {r}")
        return bool(r) and "error" not in r

    if locked:
        ok = qmp_lock(True)
        threading.Thread(target=prop, args=(1,), daemon=True).start()
    else:
        prop(0)
        ok = qmp_lock(False)
    log(f"[{label}] {'LOCK' if locked else 'unlock'} -> qemu {'ok' if ok else 'no'}")
    return ok


def serve(name, stop=None, log=print):
    """Listen for the in-game script until `stop`. Blocks; never raises."""
    from omnidroid import engine
    stop = stop or threading.Event()
    try:
        acct = engine.load_account(name)
    except Exception as e:      # noqa: BLE001
        log(f"[mouselock {name}] no account handle: {e}; no pointer lock")
        return False
    port = lock_port(acct)
    applied = {"locked": None}

    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            want, alive = parse_request(self.path)
            if want is None and not alive:
                self.send_response(404); self.end_headers(); return
            self.send_response(200)
            self.send_header("Content-Length", "2"); self.end_headers()
            self.wfile.write(b"ok")
            if alive:
                note_alive(name)
            # a heartbeat repeats the current state; only a CHANGE is acted on
            if want is not None and want != applied.get("locked"):
                applied["locked"] = want
                apply(acct, want, log=log)

        def log_message(self, *a):     # quiet; apply() logs the events
            pass

    try:
        srv = HTTPServer(("127.0.0.1", port), H)
    except OSError as e:
        log(f"[mouselock {name}] cannot listen on 127.0.0.1:{port}: {e}")
        return False
    srv.timeout = 0.5
    log(f"[mouselock {name}] listening on 127.0.0.1:{port} "
        f"(guest: {lock_url(acct)})")
    try:
        while not stop.is_set():
            srv.handle_request()
    finally:
        # the window is going; never leave the pointer grabbed behind it
        try:
            apply(acct, False, log=log)
        except Exception:      # noqa: BLE001
            pass
        srv.server_close()
    return True


def start_thread(name, log=print):
    """`serve` on a daemon thread. Returns (thread, stop_event)."""
    stop = threading.Event()
    t = threading.Thread(target=serve, args=(name, stop), kwargs={"log": log},
                         daemon=True, name=f"mouselock-{name}")
    t.start()
    return t, stop


__all__ = ["PORT_OFFSET", "PROP", "LUA_SCRIPT", "SCRIPT_NAME", "lock_port",
           "lock_url", "script_for", "parse_request", "apply", "serve",
           "start_thread"]
