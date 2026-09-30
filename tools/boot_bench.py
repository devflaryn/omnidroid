#!/usr/bin/env python3
"""One timed `omnidroid aosp` session: the log's `[t]` marks for each boot, as one JSON line.

    python tools/boot_bench.py --label base --apk ~/Desktop/Roblox-2.740.931.apk [--fresh-device]
        [--forget-saved] [--after-launch 10] [--minutes 12] [--out runs.jsonl] [-- extra aosp args]

The session runs in `<temp>/omni-bench-<label>-<secs>`; once the last boot has shown `[r] am start`
(the launcher's Activity displayed: `am start -W` answered) and `--after-launch` seconds more, its
`stop` file ends it. `--forget-saved` moves the APK's saved guest device aside first, so the run boots
a new device, saves it and boots the saved one (two boots, `boots[0]` the new one). Each boot's marks
are seconds from that boot's start (`tests/common/boot.rs`); `wall` adds the seconds from this
script's launch of `omnidroid` to when the mark was read, for the whole cost (cargo, the copy of a
saved device, derive_classpath).
"""
import argparse, json, os, re, shutil, subprocess, sys, tempfile, time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MARKS = {
    "first_app": r"\[zygote\] launching ",
    "boot_completed": r"\[r\] boot_completed=1",
    "pm_install": r"\[r\] pm install",
    "am_start": r"\[r\] am start",
    "device_quiet": r"\[r\] device quiet",
}


def marks(text):
    out = {}
    for line in text.splitlines():
        m = re.match(r"\[t\] \+([0-9.]+)s (.*)", line)
        if not m:
            continue
        for k, pat in MARKS.items():
            if k not in out and re.search(pat, m.group(2)):
                out[k] = float(m.group(1))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--apk", required=True)
    ap.add_argument("--fresh-device", action="store_true")
    ap.add_argument("--forget-saved", action="store_true")
    ap.add_argument("--after-launch", type=float, default=10)
    ap.add_argument("--minutes", type=int, default=12)
    ap.add_argument("--out")
    ap.add_argument("--repo", default=str(REPO), help="the checkout whose build runs (a worktree of another commit)")
    ap.add_argument("--dyn-dir", help="OMNIDROID_DYNARMIC_BUILD_DIR for that checkout's cargo")
    ap.add_argument("rest", nargs="*")
    a = ap.parse_args()
    temp = Path(tempfile.gettempdir())
    apk = Path(os.path.expanduser(a.apk)).resolve()
    if a.forget_saved:
        prefix = f"{apk.stem}-{apk.stat().st_size}-guest-"
        for g in (temp / "omni-golden").glob(prefix + "*"):
            if re.fullmatch(re.escape(prefix) + r"[A-Za-z0-9_-]+-v\d+", g.name):
                shutil.rmtree(g)
    inst = temp / f"omni-bench-{a.label}-{int(time.time())}"
    repo = Path(a.repo)
    exe = repo / "target" / "release" / ("omnidroid.exe" if os.name == "nt" else "omnidroid")
    env = dict(os.environ)
    # A worktree has no sysroot of its own: this checkout's.
    if not (repo / "sysroot/aosp-35/sysroot.manifest").exists():
        env.setdefault("OMNI_SYSROOT", str(REPO / "sysroot/aosp-35"))
    if a.dyn_dir:
        env["OMNIDROID_DYNARMIC_BUILD_DIR"] = a.dyn_dir
    cmd = [str(exe), "aosp", "--apk", str(apk), "--instance", str(inst), "--minutes", str(a.minutes)]
    if a.fresh_device:
        cmd.append("--fresh-device")
    cmd += a.rest
    t0 = time.time()
    proc = subprocess.Popen(cmd, cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    log, first_log = inst.with_suffix(".log"), inst.with_suffix(".1.log")
    wall, launched_at, stop_at = {}, None, None
    expect_boots = 2 if (a.forget_saved and not a.fresh_device) else 1
    while proc.poll() is None:
        time.sleep(0.5)
        try:
            text = log.read_text(errors="replace")
        except OSError:
            continue
        boot = 1 if first_log.exists() else 0
        for k, v in marks(text).items():
            wall.setdefault(f"{boot}:{k}", round(time.time() - t0, 1))
        done = boot + 1 >= expect_boots and "am start" in "".join(re.findall(r"\[r\] am start[^\n]*", text))
        if done and launched_at is None:
            launched_at = time.time()
        if launched_at and time.time() - launched_at > a.after_launch and stop_at is None:
            (inst / "data/local/tmp").mkdir(parents=True, exist_ok=True)
            (inst / "data/local/tmp/stop").write_text("1")
            stop_at = time.time()
        if stop_at and time.time() - stop_at > 60:
            proc.kill()
    boots = []
    for f in ([first_log] if first_log.exists() else []) + [log]:
        try:
            boots.append(marks(f.read_text(errors="replace")))
        except OSError:
            boots.append({})
    row = {"label": a.label, "repo": str(repo), "at": time.strftime("%Y-%m-%d %H:%M:%S"), "cmd": " ".join(cmd[1:]), "boots": boots, "wall": wall, "log": str(log), "exit": proc.returncode}
    line = json.dumps(row)
    print(line)
    if a.out:
        with open(a.out, "a") as f:
            f.write(line + "\n")


if __name__ == "__main__":
    main()
