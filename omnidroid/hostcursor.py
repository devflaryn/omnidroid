"""One pointer on screen, ever: the host's, until Roblox paints its own.

THE PROBLEM THIS CLOSES. The gaming window is QEMU's own gtk window showing
the guest's framebuffer, and the guest (Bliss) paints a mouse pointer INTO
that framebuffer -- a software sprite moved by InputReader. So the pointer the
user saw was drawn one whole pipeline late (host motion -> virtio-tablet ->
InputReader -> SurfaceFlinger -> virtio-gpu scanout -> gtk present), which on
a 144 Hz monitor reads as a pointer dragging behind the hand; and inside a
place Roblox paints its OWN cursor as well, so there were two. Meanwhile the
host's real pointer, the one with zero latency, was blanked by ui/gtk.c
because the guest's pointing device is absolute.

THE SHAPE OF THE FIX, in three parts that only work together:

  1. The guest never paints a pointer. A static framework overlay in the x86
     base makes every Android pointer bitmap transparent
     (tools/build_pointer_overlay.py, `omnidroid bake-overlay`).
  2. The host pointer is shown over the window: `-display gtk,...,show-cursor=on`
     (qemu_proc._WINDOW_FLAGS). Zero latency, the OS's own arrow.
  3. While Roblox is in a place -- the one time the guest paints a cursor --
     the host's is blanked over QMP (`omni-host-cursor`, qemu-patches/0010),
     so the in-game cursor is the only one. This module is part 3.

WHAT "IN A PLACE" IS READ FROM. The client's own log: the same join markers
the launch path trusts (engine.CLIENT_JOINED) against the disconnect markers
below, whichever came LAST wins, and a game process that is gone means no
cursor is being painted whatever the log says. Polled from the per-instance
window-lock process every POLL_SECS, which already lives exactly as long as
the window does. `dumpsys` cannot tell the home screen from a place (both are
ActivityNativeMain), and a Lua-side probe would depend on the executor
being healthy on every launch; the log is what the client writes itself.

FAILS TOWARDS VISIBLE. Unknown state, an unreadable log, an unpatched QEMU,
a busy QMP -- every one of those leaves the host pointer ON. The worst case
of this module failing is the pre-fix picture minus the lag (host arrow on
top of Roblox's), never a window with no pointer in it.
"""
import re
import shlex
import threading

# How the client log reads once the client has left a place. Position
# against engine.CLIENT_JOINED decides (see in_place_from_log); a marker that
# never appears costs nothing, one that appears while still in a place would
# show the host pointer on top of Roblox's -- the pre-fix picture, not a
# window with no pointer. Checked against a real 2.735 client log on the x86
# guest; see the note at the bottom of this file for what was observed.
CLIENT_LEFT = (r"Client:Disconnect", r"Sending disconnect",
               r"leaveGame", r"Leaving game", r"doDataModelClose",
               r"TeleportService", r"Teleporting")

POLL_SECS = 2.0
# The log tail that is read each tick. Roblox writes a few KB a second in a
# busy place; 48 KB covers well over the poll interval, and the markers
# being looked for are single lines.
TAIL_BYTES = 48_000
# The line the guest prints between the pid probe and the log tail.
_SEP = "__OMNI_SEP__"


def host_cursor_visible(in_place, game_running):
    """Should the host pointer be shown? Pure.

    Hidden ONLY when the client is known to be in a place AND its process is
    known (or not known to be gone). Every unknown leans visible."""
    if game_running is False:
        return True
    return in_place is not True


def in_place_from_log(text, joined_markers, left_markers=CLIENT_LEFT):
    """True/False/None: is the client in a place, from a tail of its log?

    Position decides: the LAST join marker against the LAST leave marker,
    because a log naturally carries both (join, play, leave, join again) and
    only the latest event says where the client is now. None when neither
    kind of marker is in the tail -- the caller treats that as unknown."""
    if not text:
        return None
    last_join = max((m.end() for p in joined_markers
                     for m in re.finditer(p, text)), default=-1)
    last_left = max((m.end() for p in left_markers
                     for m in re.finditer(p, text)), default=-1)
    if last_join < 0 and last_left < 0:
        return None
    return last_join > last_left


def probe_script(pkg, log_dir, tail_bytes=TAIL_BYTES):
    """The one guest shell that answers both questions. Pure.

    Prints the game pid (or nothing), a separator, then the tail of the
    newest client log. One adb round trip per tick instead of two, on a
    guest whose adb answers in its own time."""
    return (f"pidof {shlex.quote(pkg)}; echo {_SEP}; "
            f"L=$(ls -t {shlex.quote(log_dir)} 2>/dev/null | head -1); "
            f'[ -n "$L" ] && tail -c {int(tail_bytes)} '
            f"{shlex.quote(log_dir)}/$L")


def parse_probe(stdout):
    """(game_running, log_tail) from probe_script's output. Pure.

    game_running is None when the separator never came back -- the shell did
    not run, so nothing is known -- and False only when it did and pidof
    printed nothing."""
    if not stdout or _SEP not in stdout:
        return None, ""
    head, _, tail = stdout.partition(_SEP)
    return bool(head.strip()), tail.strip()


def probe(acct, pkg, log_dir, timeout=8):
    """Ask the guest. (game_running, in_place); both None on no answer."""
    from omnidroid import engine
    try:
        r = engine.root_shell(acct, probe_script(pkg, log_dir),
                              timeout=timeout)
    except Exception:      # noqa: BLE001 -- a probe, never a crash
        return None, None
    if r is None:
        return None, None
    running, tail = parse_probe(r.stdout or "")
    if running is None:
        return None, None
    return running, in_place_from_log(tail, engine.CLIENT_JOINED)


def watch(name, stop=None, poll=POLL_SECS, log=print):
    """Keep exactly one pointer over this instance's window until `stop`.

    Blocks. Runs on a daemon thread inside the window-lock process (see
    engine._run_windowlock): the lock lives exactly as long as the window,
    and the pointer policy has no meaning without a window. Every step here
    fails open (visible) and never raises."""
    from omnidroid import engine, qemu_proc
    stop = stop or threading.Event()
    label = f"cursor {name}"
    try:
        acct = engine.load_account(name)
    except Exception as e:      # noqa: BLE001
        log(f"[{label}] no account handle: {e}; leaving the host pointer on")
        return False
    if not qemu_proc.qemu_supports_host_cursor():
        log(f"[{label}] this QEMU has no omni-host-cursor (patch 0010); "
            f"the host pointer stays on")
        return False
    pkg = acct.get("game_package") or engine.GAME_PACKAGE
    log_dir = engine.CLIENT_LOG_DIR.format(pkg=pkg)
    applied = None            # what QEMU last acknowledged
    last_state = None
    while not stop.is_set():
        running, in_place = probe(acct, pkg, log_dir)
        want = host_cursor_visible(in_place, running)
        state = (running, in_place)
        if state != last_state:
            log(f"[{label}] game_running={running} in_place={in_place} "
                f"-> host pointer {'on' if want else 'off'}")
            last_state = state
        if want != applied:
            if qemu_proc.set_host_cursor(acct, want):
                applied = want
            # else: QMP did not answer this tick (busy main loop, or the
            # window is already gone); the next tick asks again.
        stop.wait(poll)
    return True


def start_thread(name, log=print):
    """`watch` on a daemon thread. Returns (thread, stop_event)."""
    stop = threading.Event()
    t = threading.Thread(target=watch, args=(name, stop), kwargs={"log": log},
                         daemon=True, name=f"hostcursor-{name}")
    t.start()
    return t, stop


__all__ = ["CLIENT_LEFT", "POLL_SECS", "host_cursor_visible",
           "in_place_from_log", "probe_script", "parse_probe", "probe",
           "watch", "start_thread"]
