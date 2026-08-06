#!/usr/bin/env python3
"""Drive a running omnidroid instance's screen: taps, text, keys, swipes.

This is the hands-on counterpart to `omnidroid screenshot` — a thin, fast
wrapper over `adb -s 127.0.0.1:<adb_port> shell input ...` plus a UI-tree
reader so a caller can target an element by its TEXT instead of guessing
pixels.

Why it does not shell out to the `omnidroid` CLI: every `omnidroid adb ...`
call pays full engine-import startup, and an interactive drive loop is dozens
of calls. The instance's adb port is already written to
``runtime/<name>/run.json`` when it boots, so this reads that directly and
talks to adb itself.

Liveness: a run.json can outlive its QEMU (see the stale-instance failure
mode where a recycled port silently attaches you to the WRONG VM). So the pid
in run.json is checked to be alive AND to still be the qemu process named
``omni-<name>`` before any input is sent.

Usage (see --help):
    omni_input.py state
    omni_input.py shot --out /tmp/s.png
    omni_input.py ui                       # tappable elements + their centers
    omni_input.py ui download              # ...filtered
    omni_input.py tap 640 423 --shot
    omni_input.py tap --text "Download path"
    omni_input.py text "hello world"
    omni_input.py key BACK
    omni_input.py swipe 640 700 640 200 --ms 400
    omni_input.py launch com.android.settings

Every mutating command takes --shot (capture a PNG afterwards and print its
path) and --settle SECONDS (wait before that capture).
"""
import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from pathlib import Path

# Magisk su lands in one of these in the guest; a plain `su` is usually absent.
SU_CANDIDATES = ("/debug_ramdisk/su", "/sbin/su", "su")
GUEST_TMP = "/data/local/tmp"


def omni_root():
    """The omnidroid checkout that owns runtime/ and accounts.json."""
    env = os.environ.get("OMNI_ROOT")
    if env:
        return Path(env)
    return Path(__file__).resolve().parent.parent


def die(msg, code=2):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


# ---------------------------------------------------------------- instance --

def _pid_is_this_instance(pid, name):
    """True if `pid` is alive AND is the QEMU for `name`.

    Both halves matter: a dead pid means the port in run.json is stale, and a
    live pid whose -name is a DIFFERENT instance means the port was recycled
    onto someone else's VM. Either way, sending input would hit the wrong
    screen (or nothing), so callers must refuse.
    """
    try:
        r = subprocess.run(["ps", "-o", "command=", "-p", str(pid)],
                           capture_output=True, text=True, timeout=10)
    except Exception:
        return False
    cmd = (r.stdout or "").strip()
    return bool(cmd) and f"omni-{name}" in cmd


def running_instances():
    """[(name, adb_port)] for every instance whose QEMU is verifiably alive."""
    out = []
    rt = omni_root() / "runtime"
    if not rt.is_dir():
        return out
    for d in sorted(rt.iterdir()):
        rj = d / "run.json"
        if not rj.is_file():
            continue
        try:
            info = json.loads(rj.read_text())
        except Exception:
            continue
        pid, port = info.get("pid"), info.get("adb_port")
        if not pid or not port:
            continue
        if _pid_is_this_instance(pid, d.name):
            out.append((d.name, int(port)))
    return out


def resolve_instance(name):
    live = running_instances()
    if not live:
        die("no running omnidroid instance found "
            f"(looked in {omni_root() / 'runtime'}). Start one first.")
    if name:
        for n, port in live:
            if n == name:
                return n, port
        die(f"'{name}' is not running. Live now: "
            + ", ".join(n for n, _ in live))
    if len(live) > 1:
        die("more than one instance is running — pass --name. Live now: "
            + ", ".join(n for n, _ in live))
    return live[0]


# --------------------------------------------------------------------- adb --

class Device:
    def __init__(self, name, port):
        self.name = name
        self.port = port
        self.serial = f"127.0.0.1:{port}"
        self._connected = False

    def connect(self):
        if not self._connected:
            subprocess.run(["adb", "connect", self.serial],
                           capture_output=True, text=True, timeout=20)
            self._connected = True

    def run(self, *args, timeout=60):
        self.connect()
        return subprocess.run(["adb", "-s", self.serial, *args],
                              capture_output=True, text=True, timeout=timeout)

    def sh(self, script, timeout=60):
        """Run one shell line in the guest. `script` is sent verbatim, so any
        caller-supplied value inside it must already be shlex.quote()d."""
        return self.run("shell", script, timeout=timeout)

    def input(self, *parts, timeout=30):
        """`input <parts>`; raises on a non-zero exit so a silently-dropped
        gesture can never be reported as a success."""
        r = self.sh("input " + " ".join(parts), timeout=timeout)
        blob = ((r.stdout or "") + (r.stderr or "")).strip()
        if r.returncode != 0:
            die(f"input failed (exit {r.returncode}): {blob[:400]}")
        return blob

    def su(self):
        """A working root shell path in the guest, or None."""
        for cand in SU_CANDIDATES:
            # Must go through `sh -c`: MagiskSU permutes argv, so a bare
            # trailing flag is misread as an su option rather than passed on.
            r = self.sh(f"{cand} 0 sh -c {shlex.quote('id -u')}", timeout=20)
            if (r.stdout or "").strip().splitlines()[-1:] == ["0"]:
                return cand
        return None


# ------------------------------------------------------------------ screen --

def screen_size(dev):
    r = dev.sh("wm size", timeout=20)
    m = re.search(r"(\d+)x(\d+)", r.stdout or "")
    return (int(m.group(1)), int(m.group(2))) if m else (None, None)


def top_activity(dev):
    r = dev.sh("dumpsys activity activities", timeout=40)
    m = re.search(r"topResumedActivity=ActivityRecord\{\S+ \S+ (\S+)",
                  r.stdout or "")
    return m.group(1) if m else None


def lock_task_state(dev):
    r = dev.sh("dumpsys activity", timeout=40)
    m = re.search(r"mLockTaskModeState=(\S+)", r.stdout or "")
    return m.group(1) if m else None


def draw_grid(path, step=100):
    """Overlay a labelled coordinate grid on a saved screenshot, in place.

    The whole point of this tool is 'look at the screen, tap a pixel'. Reading
    a pixel off a bare screenshot is guesswork that drifts by tens of pixels —
    enough to miss a button. Ruled, labelled lines turn that estimate into
    counting to the nearest gridline, so the coordinate handed to `tap` is the
    one that was actually meant.

    Labels are drawn light-on-dark with a dark halo so they stay readable over
    both a white settings page and a dark game frame.
    """
    from PIL import Image, ImageDraw, ImageFont
    img = Image.open(path).convert("RGB")
    w, h = img.size
    overlay = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(overlay)
    try:
        font = ImageFont.load_default(size=15)
    except TypeError:                      # Pillow < 10 has no sized default
        font = ImageFont.load_default()

    minor, major = (255, 0, 255, 60), (255, 0, 255, 130)
    for x in range(0, w, step):
        d.line([(x, 0), (x, h)], fill=major if x % (step * 5) == 0 else minor)
    for y in range(0, h, step):
        d.line([(0, y), (w, y)], fill=major if y % (step * 5) == 0 else minor)

    img = Image.alpha_composite(img.convert("RGBA"), overlay).convert("RGB")
    d = ImageDraw.Draw(img)
    for x in range(0, w, step):
        for y in range(0, h, step):
            label = f"{x},{y}"
            # Halo first, then the glyph, so it reads over any background.
            for dx, dy in ((-1, 0), (1, 0), (0, -1), (0, 1)):
                d.text((x + 3 + dx, y + 2 + dy), label, font=font,
                       fill=(0, 0, 0))
            d.text((x + 3, y + 2), label, font=font, fill=(255, 255, 0))
    img.save(path)
    return path


def capture(dev, out=None, settle=0.0, grid=0):
    if settle:
        time.sleep(settle)
    if out:
        out = Path(out).expanduser()
    else:
        d = omni_root() / "runtime" / dev.name / "input-shots"
        d.mkdir(parents=True, exist_ok=True)
        out = d / f"shot-{int(time.time() * 1000)}.png"
    out.parent.mkdir(parents=True, exist_ok=True)
    guest = f"{GUEST_TMP}/_omni_input_shot.png"
    dev.sh(f"screencap -p {guest}", timeout=60)
    dev.run("pull", guest, str(out), timeout=90)
    if not (out.exists() and out.stat().st_size > 0):
        die("screenshot failed (nothing pulled)")
    if grid:
        draw_grid(out, grid)
    return str(out)


# ----------------------------------------------------------------- ui tree --

def ui_nodes(dev):
    """Parse a uiautomator dump into flat dicts with tap centers."""
    guest = f"{GUEST_TMP}/_omni_input_ui.xml"
    r = dev.sh(f"uiautomator dump {shlex.quote(guest)} >/dev/null 2>&1; "
               f"cat {shlex.quote(guest)}", timeout=90)
    xml = (r.stdout or "").strip()
    start = xml.find("<hierarchy")
    if start < 0:
        die("uiautomator dump produced no hierarchy "
            f"({((r.stderr or '') + xml)[:300]})")
    try:
        root = ET.fromstring(xml[start:])
    except ET.ParseError as e:
        die(f"could not parse the UI dump: {e}")
    nodes = []
    for el in root.iter("node"):
        b = el.get("bounds") or ""
        m = re.match(r"\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]", b)
        if not m:
            continue
        x1, y1, x2, y2 = (int(v) for v in m.groups())
        rid = el.get("resource-id") or ""
        nodes.append({
            "text": el.get("text") or "",
            "desc": el.get("content-desc") or "",
            "id": rid.split("/")[-1] if rid else "",
            "full_id": rid,
            "cls": (el.get("class") or "").split(".")[-1],
            "clickable": el.get("clickable") == "true",
            "focused": el.get("focused") == "true",
            "enabled": el.get("enabled") == "true",
            "bounds": [x1, y1, x2, y2],
            "center": [(x1 + x2) // 2, (y1 + y2) // 2],
        })
    return nodes


def interesting(nodes):
    """Nodes worth showing: anything labelled, identified, or clickable."""
    return [n for n in nodes
            if n["text"] or n["desc"] or n["id"] or n["clickable"]]


def fmt_node(n):
    bits = [f"({n['center'][0]:>4},{n['center'][1]:>4})",
            "[C]" if n["clickable"] else "   ",
            f"{n['cls']:<14}"]
    if n["text"]:
        bits.append(f'text={n["text"]!r}')
    if n["desc"]:
        bits.append(f'desc={n["desc"]!r}')
    if n["id"]:
        bits.append(f'id={n["id"]}')
    if n["focused"]:
        bits.append("<focused>")
    return " ".join(bits)


def match_nodes(nodes, *, text=None, desc=None, rid=None):
    """Exact matches win; fall back to case-insensitive substring so a caller
    can target 'download' without reproducing the label exactly."""
    def pick(key, want):
        exact = [n for n in nodes if n[key] == want]
        if exact:
            return exact
        low = want.lower()
        return [n for n in nodes if low in n[key].lower()]

    if text is not None:
        return pick("text", text)
    if desc is not None:
        return pick("desc", desc)
    return [n for n in nodes if n["id"] == rid or n["full_id"] == rid] or \
           [n for n in nodes if rid.lower() in n["id"].lower()]


# ------------------------------------------------------------------- text --

def encode_text(s):
    """Encode for Android's `input text`, then quote for the guest shell.

    `input text` maps the literal token %s back to a space, which is the only
    supported way to send one — so spaces are encoded first and the whole
    string is then single-quoted so the guest shell cannot reinterpret
    quotes, $, backticks or ;. A literal '%s' in the source text therefore
    arrives as a space; use `key` for anything this cannot express.
    """
    return shlex.quote(s.replace(" ", "%s"))


# --------------------------------------------------------------- commands --

def after(dev, args, payload):
    if getattr(args, "shot", False):
        payload["screenshot"] = capture(dev, getattr(args, "out", None),
                                        settle=args.settle,
                                        grid=getattr(args, "grid", 0))
    elif args.settle:
        time.sleep(args.settle)
    return payload


def emit(args, payload):
    if args.json:
        print(json.dumps(payload, indent=2))
        return
    shot = payload.pop("screenshot", None)
    ok = payload.pop("ok", None)
    detail = " ".join(f"{k}={v}" for k, v in payload.items() if v not in (None, ""))
    print(f"{'ok' if ok else '--'} {detail}".strip())
    if shot:
        print(f"screenshot: {shot}")


def cmd_state(dev, args):
    w, h = screen_size(dev)
    emit(args, {"ok": True, "instance": dev.name, "adb": dev.serial,
                "screen": f"{w}x{h}" if w else "?",
                "top": top_activity(dev), "lock_task": lock_task_state(dev)})


def cmd_shot(dev, args):
    emit(args, {"ok": True,
                "screenshot": capture(dev, args.out, args.settle, args.grid)})


def cmd_ui(dev, args):
    nodes = ui_nodes(dev)
    shown = nodes if args.all else interesting(nodes)
    if args.pattern:
        low = args.pattern.lower()
        shown = [n for n in shown
                 if low in (n["text"] + n["desc"] + n["id"] + n["cls"]).lower()]
    shown.sort(key=lambda n: (n["center"][1], n["center"][0]))
    if args.json:
        print(json.dumps(shown, indent=2))
        return
    if not shown:
        print("(no matching elements)")
        return
    for n in shown:
        print(fmt_node(n))


def cmd_tap(dev, args):
    if args.text is not None or args.desc is not None or args.id is not None:
        hits = match_nodes(ui_nodes(dev), text=args.text, desc=args.desc,
                           rid=args.id)
        if not hits:
            die("no element matched — run `ui` to see what is on screen")
        if len(hits) > 1 and not args.first:
            lines = "\n  ".join(fmt_node(n) for n in hits[:10])
            die(f"{len(hits)} elements matched; narrow it or pass --first:\n"
                f"  {lines}")
        # Prefer a clickable match: labels often sit inside the real button.
        node = next((n for n in hits if n["clickable"]), hits[0])
        x, y = node["center"]
        label = node["text"] or node["desc"] or node["id"]
    else:
        if args.x is None or args.y is None:
            die("give X and Y, or one of --text / --desc / --id")
        x, y, label = args.x, args.y, None
    dev.input("tap", str(x), str(y))
    emit(args, after(dev, args, {"ok": True, "tapped": f"{x},{y}",
                                 "element": label}))


def cmd_text(dev, args):
    if not args.value:
        die("nothing to type")
    dev.input("text", encode_text(args.value))
    emit(args, after(dev, args, {"ok": True, "typed": args.value}))


def cmd_key(dev, args):
    for k in args.keys:
        dev.input("keyevent", shlex.quote(str(k)))
    emit(args, after(dev, args, {"ok": True, "keys": ",".join(map(str, args.keys))}))


def cmd_swipe(dev, args):
    ms = max(50, min(int(args.ms), 60000))
    dev.input("swipe", str(args.x1), str(args.y1), str(args.x2), str(args.y2),
              str(ms))
    emit(args, after(dev, args, {
        "ok": True, "swipe": f"{args.x1},{args.y1}->{args.x2},{args.y2}",
        "ms": ms}))


def cmd_launch(dev, args):
    """Foreground another app. The production kiosk pins itself with Lock Task
    Mode, which blocks a plain `am start`; the only way past it is root, and
    even then the OS refuses `am task lock stop` for a device-owner lock. So
    this reports exactly which step lost, rather than claiming a launch."""
    su = dev.su()
    if su:
        dev.sh(f"{su} 0 sh -c {shlex.quote('am task lock stop')}", timeout=20)
        launcher = f"monkey -p {args.package} -c android.intent.category.LAUNCHER 1"
        dev.sh(f"{su} 0 sh -c {shlex.quote(launcher)}", timeout=40)
    else:
        dev.sh("monkey -p " + shlex.quote(args.package)
               + " -c android.intent.category.LAUNCHER 1", timeout=40)
    time.sleep(max(args.settle, 2.0))
    top = top_activity(dev) or ""
    ok = args.package in top
    payload = {"ok": ok, "package": args.package, "top": top,
               "root": bool(su), "lock_task": lock_task_state(dev)}
    if not ok:
        payload["hint"] = ("still pinned — Lock Task Mode blocks foregrounding "
                           "another app; drive the pinned app instead")
    if args.shot:
        payload["screenshot"] = capture(dev, args.out, grid=args.grid)
    emit(args, payload)
    sys.exit(0 if ok else 1)


def build_parser():
    p = argparse.ArgumentParser(
        prog="omni_input",
        description="Send taps, text and keys to a running omnidroid instance.")
    p.add_argument("--name", default=None,
                   help="instance name (default: the only running one)")
    p.add_argument("--json", action="store_true", help="machine-readable output")
    sub = p.add_subparsers(dest="cmd", required=True)

    def gestural(sp, with_shot=True):
        sp.add_argument("--settle", type=float, default=0.0,
                        help="seconds to wait after the action")
        if with_shot:
            sp.add_argument("--shot", action="store_true",
                            help="capture a screenshot afterwards")
            sp.add_argument("--out", default=None, help="screenshot path")
            sp.add_argument("--grid", nargs="?", type=int, const=100, default=0,
                            metavar="PX",
                            help="overlay a labelled coordinate grid "
                                 "(default every 100px)")
        return sp

    s = sub.add_parser("state", help="instance, screen size, top app, lock state")
    s.set_defaults(func=cmd_state)

    s = sub.add_parser("shot", help="pull a screenshot")
    s.add_argument("--out", default=None)
    s.add_argument("--settle", type=float, default=0.0)
    s.add_argument("--grid", nargs="?", type=int, const=100, default=0,
                   metavar="PX",
                   help="overlay a labelled coordinate grid so a pixel can be "
                        "read off the image exactly (default every 100px)")
    s.set_defaults(func=cmd_shot)

    s = sub.add_parser("ui", help="list on-screen elements with tap centers")
    s.add_argument("pattern", nargs="?", default=None,
                   help="case-insensitive filter over text/desc/id/class")
    s.add_argument("--all", action="store_true",
                   help="include unlabelled layout nodes too")
    s.set_defaults(func=cmd_ui)

    s = sub.add_parser("tap", help="tap a pixel, or an element by text/id")
    s.add_argument("x", nargs="?", type=int)
    s.add_argument("y", nargs="?", type=int)
    s.add_argument("--text", default=None, help="match an element's text")
    s.add_argument("--desc", default=None, help="match a content-description")
    s.add_argument("--id", default=None, help="match a resource-id")
    s.add_argument("--first", action="store_true",
                   help="tap the first match instead of refusing an ambiguous one")
    gestural(s).set_defaults(func=cmd_tap)

    s = sub.add_parser("text", help="type into the focused field")
    s.add_argument("value")
    gestural(s).set_defaults(func=cmd_text)

    s = sub.add_parser("key", help="press keycodes by name or number")
    s.add_argument("keys", nargs="+", help="e.g. BACK HOME ENTER DEL 66")
    gestural(s).set_defaults(func=cmd_key)

    s = sub.add_parser("swipe", help="swipe/drag (same point + long --ms = long press)")
    for a in ("x1", "y1", "x2", "y2"):
        s.add_argument(a, type=int)
    s.add_argument("--ms", type=int, default=300)
    gestural(s).set_defaults(func=cmd_swipe)

    s = sub.add_parser("launch", help="foreground a package (needs root if pinned)")
    s.add_argument("package")
    gestural(s).set_defaults(func=cmd_launch)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    name, port = resolve_instance(args.name)
    args.func(Device(name, port), args)


if __name__ == "__main__":
    main()
