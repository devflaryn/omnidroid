#!/usr/bin/env python3
"""Interleaved A/B on the live warm device (ABBA order, one device, minutes apart at most).

    python tools/warm_ab.py launch  --apk A.apk --package P --activity C --pairs 4 --b-env OMNI_APP_PRELOAD=0
    python tools/warm_ab.py install --apk A.apk --apk2 A2.apk --pairs 4 --b-prop pm.dexopt.install=skip

launch:  each trial force-stops the app, then `am start -W`; arm B's app processes get --b-env
         (the device's `<instance>.appenv`, read by the zygote at each app start). Timed: the
         control channel's wall seconds and Android's TotalTime.
install: each trial installs the other of two APKs of one package (so every install is a real
         reinstall); arm B sets --b-prop (as root) first, arm A the property's default back.
Prints each trial, then the median of B-A over the pairs.
"""
import argparse, json, statistics, sys, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from device_ctl import find_device, run  # noqa: E402


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("what", choices=["launch", "install"])
    ap.add_argument("--apk", required=True)
    ap.add_argument("--apk2")
    ap.add_argument("--package")
    ap.add_argument("--activity")
    ap.add_argument("--pairs", type=int, default=4)
    ap.add_argument("--b-env", action="append", default=[])
    ap.add_argument("--b-prop")
    ap.add_argument("--out")
    a = ap.parse_args()
    dev = find_device()
    if not dev:
        raise SystemExit("no live warm device")
    appenv = dev.with_suffix(".appenv")
    order = []
    for i in range(a.pairs):
        order += ["A", "B"] if i % 2 == 0 else ["B", "A"]
    rows = []
    drop = dev / "data/local/tmp/omni-apk"
    drop.mkdir(parents=True, exist_ok=True)
    apks = [Path(a.apk), Path(a.apk2 or a.apk)]
    for i, p in enumerate(apks):
        (drop / f"ab{i}.apk").write_bytes(p.read_bytes())
    if a.what == "launch":
        code, out, s = run(dev, f"pm install -r -d -g /data/local/tmp/omni-apk/ab0.apk")
        print("installed:", out.strip(), f"{s:.1f}s")
    prop_name, _, prop_value = (a.b_prop or "=").partition("=")
    default = ""
    if prop_name:
        _, default, _ = run(dev, f"getprop {prop_name}")
        default = default.strip()
    for n, arm in enumerate(order):
        if a.what == "launch":
            appenv.write_text("\n".join(a.b_env) + "\n" if arm == "B" else "")
            run(dev, f"am force-stop {a.package}")
            time.sleep(2)
            code, out, s = run(dev, f"am start -W -n {a.package}/{a.activity}")
            total = next((int(l.split(":")[1]) for l in out.splitlines() if l.startswith("TotalTime:")), None)
            row = {"n": n, "arm": arm, "wall_s": round(s, 2), "total_ms": total, "ok": "Status: ok" in out}
        else:
            if prop_name:
                v = prop_value if arm == "B" else (default or "''")
                run(dev, f"setprop {prop_name} {v}", uid=0)
            code, out, s = run(dev, f"pm install -r -d -g /data/local/tmp/omni-apk/ab{n % 2}.apk")
            row = {"n": n, "arm": arm, "wall_s": round(s, 2), "ok": "Success" in out}
        print(json.dumps(row), flush=True)
        rows.append(row)
    appenv.write_text("")
    if prop_name:
        run(dev, f"setprop {prop_name} {default or chr(39) * 2}", uid=0)
    key = "wall_s"
    deltas = []
    for i in range(a.pairs):
        pair = rows[2 * i:2 * i + 2]
        b = next(r[key] for r in pair if r["arm"] == "B")
        aa = next(r[key] for r in pair if r["arm"] == "A")
        deltas.append(round(b - aa, 2))
    summary = {"what": a.what, "b": a.b_env or a.b_prop, "A_median": statistics.median(r[key] for r in rows if r["arm"] == "A"),
               "B_median": statistics.median(r[key] for r in rows if r["arm"] == "B"), "paired_deltas": deltas,
               "median_delta": statistics.median(deltas)}
    if a.what == "launch":
        summary["A_total_ms"] = statistics.median(r["total_ms"] or 0 for r in rows if r["arm"] == "A")
        summary["B_total_ms"] = statistics.median(r["total_ms"] or 0 for r in rows if r["arm"] == "B")
    print(json.dumps(summary))
    if a.out:
        with open(a.out, "a") as f:
            f.write(json.dumps({"summary": summary, "rows": rows}) + "\n")


if __name__ == "__main__":
    main()
