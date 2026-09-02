#!/usr/bin/env python3
"""Per-thread CPU attribution inside a running guest, from /proc deltas.

    python threadstat.py [--port 16001] [--secs 30] [--out DIR] [--top 25]

Two snapshots of every thread's utime+stime (/proc/<pid>/task/<tid>/stat) and
of /proc/stat, SECS apart, over one adb shell each. Prints:
  * guest-wide CPU split (user/sys/idle/iowait/steal) as % of ALL cores
  * per-process totals (sum of its threads)
  * the top N threads, with their process, as % of ONE core
`top -b` on this base reads 0 in its %CPU column, which is why this exists.
The stat 'comm' field may contain spaces and parentheses; everything is
parsed from the LAST ')' onward, as procps does.
"""
import argparse, os, re, subprocess, sys, time, json

SH = r'''
CLK=$(getconf CLK_TCK 2>/dev/null || echo 100); echo "CLK $CLK"
echo "NOW $(cat /proc/uptime)"
grep '^cpu' /proc/stat
for p in /proc/[0-9]*; do echo "P ${p#/proc/}"; cat $p/task/[0-9]*/stat 2>/dev/null; done
echo "END $(cat /proc/uptime)"
'''

def snap(port):
    out = subprocess.run(["adb", "-s", f"127.0.0.1:{port}", "shell", SH],
                         capture_output=True, text=True, timeout=60).stdout.replace("\r", "")
    cpu = {}; threads = {}; clk = 100; now = 0.0; pid = 0; pcomm_of = {}
    for line in out.splitlines():
        if line.startswith("CLK "): clk = int(line.split()[1]); continue
        if line.startswith("NOW "): now = float(line.split()[1]); continue
        if line.startswith("END "): now = (now + float(line.split()[1])) / 2; continue
        if line.startswith("cpu"):
            f = line.split(); cpu[f[0]] = [int(x) for x in f[1:]]; continue
        if line.startswith("P "):
            pid = int(line.split()[1]); continue
        if line[:1].isdigit() and "(" in line:
            rp = line.rfind(")")
            if rp < 0: continue
            tcomm = line[line.find("(") + 1:rp] or "(unnamed)"
            rest = line[rp + 2:].split()
            try: ut, st, nice = int(rest[11]), int(rest[12]), int(rest[16])
            except (IndexError, ValueError): continue
            tid = int(line.split()[0])
            if tid == pid: pcomm_of[pid] = tcomm
            threads[(pid, tid)] = (tcomm, ut + st, nice)
    threads = {k: (pcomm_of.get(k[0], "?"), v[0], v[1], v[2]) for k, v in threads.items()}
    return {"clk": clk, "now": now, "cpu": cpu, "threads": threads}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default=os.environ.get("ADBP", "16001"))
    ap.add_argument("--secs", type=float, default=30)
    ap.add_argument("--out", default=None)
    ap.add_argument("--top", type=int, default=25)
    a = ap.parse_args()
    t0 = time.time(); s0 = snap(a.port)
    time.sleep(a.secs)
    s1 = snap(a.port); wall = time.time() - t0
    clk = s1["clk"]; dt = s1["now"] - s0["now"] or wall
    # guest-wide
    c0, c1 = s0["cpu"]["cpu"], s1["cpu"]["cpu"]
    d = [b - a_ for a_, b in zip(c0, c1)]; tot = sum(d) or 1
    ncpu = len([k for k in s1["cpu"] if k != "cpu"])
    names = ["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal", "guest", "gnice"]
    lines = []
    lines.append(f"window {dt:.1f} s guest-clock ({wall:.1f} s wall), {ncpu} vCPU, CLK_TCK {clk}")
    lines.append("guest-wide: " + "  ".join(f"{n}={100*v/tot:.1f}%" for n, v in zip(names, d) if v))
    busy = tot - d[3] - d[4]
    lines.append(f"guest busy = {100*busy/tot:.1f}% of all cores = {ncpu*busy/tot*100:.0f}% of one core (of {ncpu*100}%)")
    percpu = []
    for k in sorted(s1["cpu"], key=lambda k: (len(k), k)):
        if k == "cpu": continue
        a_, b = s0["cpu"].get(k), s1["cpu"][k]
        if not a_: continue
        dd = [y - x for x, y in zip(a_, b)]; tt = sum(dd) or 1
        percpu.append(f"{k}={100*(tt-dd[3]-dd[4])/tt:.0f}%")
    lines.append("per-vCPU busy: " + " ".join(percpu))
    # per-thread
    rows = []
    for key, (pcomm, tcomm, cpu1, nice) in s1["threads"].items():
        prev = s0["threads"].get(key)
        cpu0 = prev[2] if prev else 0
        pct = 100.0 * (cpu1 - cpu0) / clk / dt
        rows.append((pct, key[0], key[1], pcomm, tcomm, prev is None, nice))
    rows.sort(reverse=True)
    procs = {}
    for pct, pid, tid, pcomm, tcomm, new, nice in rows:
        procs.setdefault((pid, pcomm), [0.0, 0]); procs[(pid, pcomm)][0] += pct; procs[(pid, pcomm)][1] += 1
    lines.append("")
    lines.append("per-process (% of one core, threads):")
    for (pid, pcomm), (pct, n) in sorted(procs.items(), key=lambda kv: -kv[1][0])[:12]:
        if pct < 0.5: break
        lines.append(f"  {pct:7.1f}%  {pid:6d}  {pcomm}  [{n} threads]")
    lines.append("")
    lines.append(f"top {a.top} threads (% of one core):")
    for pct, pid, tid, pcomm, tcomm, new, nice in rows[:a.top]:
        if pct < 0.3: break
        lines.append(f"  {pct:7.1f}%  {pid:6d}/{tid:<6d} {pcomm[:24]:24s} {tcomm:20s} nice={nice:3d}{'  (new)' if new else ''}")
    text = "\n".join(lines); print(text)
    if a.out:
        os.makedirs(a.out, exist_ok=True)
        open(os.path.join(a.out, "threadstat.txt"), "w", encoding="utf-8").write(text + "\n")
        json.dump({"rows": rows, "cpu0": s0["cpu"], "cpu1": s1["cpu"], "dt": dt, "clk": clk},
                  open(os.path.join(a.out, "threadstat.json"), "w"))

if __name__ == "__main__":
    main()
