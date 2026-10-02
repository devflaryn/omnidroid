#!/usr/bin/env python3
"""Time a command from its start to Pet Simulator 99's own loading screen (the place's, after
"Joining game" -- Roblox's join screens before it do not count).

    python tools/join_timer.py [--label L] [--out DIR] [--after S] [--limit S] [--stop] -- <command...>

The session's display and log are found by themselves: the newest `<temp>/omni-linux-r-*.png` or
`<temp>/omni-warm-*.png` written since the start, its log beside it (`.log`; only what is
appended after the start is read). The display is copied every second to `<out>/<label>/<ms>.png`;
the log's marks and the loading screen (`ingame_screen.py`) are printed with their seconds since
the start and saved in `marks.json`. `--stop` ends the session afterwards: the command's process
tree is ended, and a session directory's `data/local/tmp/stop` written.
"""
import argparse, glob, json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from ingame_screen import is_loading_screen  # noqa: E402

MARKS = ["DID_LOG_IN", "Joining game", "onGameLoaded", "Upgrade required", "GUEST THREAD DIED",
         "[r] cookie planted", "[r] relaunched", "[r] am start", "[r] join intent", "[r] warm",
         "[zygote] launching com.roblox.client", "START u0 {cmp=com.roblox.client/.ActivityNativeMain"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", default=time.strftime("jt-%H%M%S"))
    ap.add_argument("--out", default=os.environ.get("WJ_OUT", tempfile.gettempdir()))
    ap.add_argument("--after", type=float, default=3)
    ap.add_argument("--limit", type=float, default=600)
    ap.add_argument("--stop", action="store_true")
    ap.add_argument("command", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.command[1:] if a.command[:1] == ["--"] else a.command
    out = Path(a.out) / a.label
    out.mkdir(parents=True, exist_ok=True)
    temp = Path(tempfile.gettempdir())
    pngs = lambda: [Path(p) for p in glob.glob(str(temp / "omni-linux-r-*.png")) + glob.glob(str(temp / "omni-warm-*.png"))]
    sizes = {}
    for p in pngs():
        lg = p.with_suffix(".log")
        sizes[lg] = lg.stat().st_size if lg.exists() else 0
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=open(out / "command.out", "wb"), stderr=subprocess.STDOUT)
    marks, first = [], {}

    def mark(s, name, extra=""):
        marks.append((round(s, 2), name, extra))
        first.setdefault(name, s)
        print(f"[jt] +{s:6.2f}s {name} {extra}".rstrip(), flush=True)

    last_png, pos, log = 0.0, 0, None
    done_at = None
    while time.time() - t0 < a.limit:
        now = time.time() - t0
        live = [p for p in pngs() if p.stat().st_mtime > t0]
        if live:
            png = max(live, key=lambda p: p.stat().st_mtime)
            if log != png.with_suffix(".log"):
                log = png.with_suffix(".log")
                pos = sizes.get(log, 0)
                mark(now, "display", png.name)
            m = png.stat().st_mtime
            if m != last_png:
                last_png = m
                to = out / f"{int(now * 1000):07d}.png"
                try:
                    shutil.copyfile(png, to)
                    if "Joining game" in first and "in-game loading screen" not in first and is_loading_screen(to):
                        mark(now, "in-game loading screen", to.name)
                        done_at = time.time()
                except Exception:
                    pass
        if log and log.exists():
            if log.stat().st_size < pos:
                pos = 0  # a reboot started the log again (the old one is now `.1.log`)
            with open(log, "rb") as f:
                f.seek(pos)
                data = f.read()
            cut = data.rfind(b"\n") + 1
            pos += cut
            for line in data[:cut].decode("utf-8", "replace").splitlines():
                for k in MARKS:
                    if k in line and (k not in first or k.startswith("[r]")):
                        mark(now, k, line.strip()[:150])
        if proc.poll() is not None and "in-game loading screen" not in first:
            mark(now, f"command exited {proc.returncode}")
            break
        if done_at and time.time() - done_at > a.after:
            break
        time.sleep(0.5)
    json.dump({"label": a.label, "command": cmd, "marks": marks, "first": first}, open(out / "marks.json", "w"), indent=1)
    if a.stop:
        if log:
            stop = log.with_suffix("") / "data/local/tmp/stop"
            if stop.parent.exists() and "omni-warm-" not in str(stop):
                stop.write_text("1")
        if proc.poll() is None:
            # The launcher alone, as a user's Ctrl+C or a kill would end it: what it started ends by
            # its own means (a session's job object; a warm session's release helper).
            subprocess.run(["taskkill", "/F", "/PID", str(proc.pid)] if os.name == "nt" else ["kill", str(proc.pid)],
                           capture_output=True)
    print(f"[jt] result: in-game loading screen at {first.get('in-game loading screen', 'never')} s; frames in {out}")


if __name__ == "__main__":
    main()
