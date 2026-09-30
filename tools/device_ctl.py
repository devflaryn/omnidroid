#!/usr/bin/env python3
"""Run a shell command on the host's live warm device through its control channel -- `adb shell`
for omnidroid, without the MCP server.

    python tools/device_ctl.py [--uid N] [--timeout S] <command...>
    python tools/device_ctl.py --status

The device is the newest `<temp>/omni-warm-*` (or `--dir <instance>`, any instance with a control
channel) whose heartbeat is fresh; the command is written as `<instance>.ctl/<id>.cmd`, its output
read from `<id>.out` and its status from `<id>.rc`. Prints the output, then `[exit N in S s]` on
stderr; exits with the command's status.
"""
import argparse, glob, os, sys, tempfile, time
from pathlib import Path


def live(ctl: Path) -> bool:
    try:
        return time.time() - (ctl / "alive").stat().st_mtime < 5
    except OSError:
        return False


def find_device():
    temp = Path(os.environ.get("OMNI_MCP_WARM_DIR") or tempfile.gettempdir())
    for d in sorted(temp.glob("omni-warm-*"), reverse=True):
        if d.is_dir() and d.name[len("omni-warm-"):].isdigit() and live(d.with_suffix(".ctl")):
            return d
    return None


def run(instance: Path, command: str, uid=None, timeout=300.0):
    ctl = instance.with_suffix(".ctl")
    ident = f"{time.time_ns():024d}-{os.getpid()}"
    text = (f"#uid={uid}\n" if uid is not None else "") + command
    (ctl / f"{ident}.part").write_text(text, newline="\n")
    os.replace(ctl / f"{ident}.part", ctl / f"{ident}.cmd")
    t, deadline = time.time(), time.time() + timeout
    rc, out = ctl / f"{ident}.rc", ctl / f"{ident}.out"
    while True:
        if time.time() > deadline:
            raise SystemExit(f"timed out after {timeout} s")
        try:
            # A file just renamed into place can be held a moment (a virus scanner): read it again.
            code = int(rc.read_text().strip() or -1)
            output = out.read_text(errors="replace") if out.exists() else ""
            break
        except (FileNotFoundError, PermissionError):
            time.sleep(0.01)
    for f in (rc, out):
        try:
            f.unlink(missing_ok=True)
        except PermissionError:
            pass
    return code, output, time.time() - t


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir")
    ap.add_argument("--uid", type=int)
    ap.add_argument("--timeout", type=float, default=300)
    ap.add_argument("--status", action="store_true")
    ap.add_argument("command", nargs="*")
    a = ap.parse_args()
    inst = Path(a.dir) if a.dir else find_device()
    if a.status:
        print(f"device: {inst or 'none'}" + (f", ready: {(inst / 'data/local/tmp/warm-ready').exists()}" if inst else ""))
        return
    if not inst:
        raise SystemExit("no live warm device")
    code, output, secs = run(inst, " ".join(a.command), a.uid, a.timeout)
    sys.stdout.write(output)
    print(f"[exit {code} in {secs:.2f} s]", file=sys.stderr)
    sys.exit(code if 0 <= code < 256 else 1)


if __name__ == "__main__":
    main()
