"""Sample a macOS process's memory from outside it: phys_footprint, resident size, and the peak.

    python3 tools/footprint_mac.py --pid 1234 [--every 1.0] [--seconds 600] [--csv out.csv]
    python3 tools/footprint_mac.py --match gameactivity    # the newest process whose name matches

`phys_footprint` is the number the kernel charges a process for (dirty private memory plus what
the compressor and swap hold of it) -- Activity Monitor's "Memory" column and what memory-pressure
policy acts on. It is read with `proc_pid_rusage(RUSAGE_INFO_V4)`, the same ledger
`omni_platform::vm::process_commit_charge` reads from inside with `task_info(TASK_VM_INFO)`.
`ri_lifetime_max_phys_footprint` is the kernel's own peak, so a spike between two samples is not
missed.

macOS only. Reads another process's usage, which a process may do for its own user's processes.

    python3 tools/footprint_mac.py --launch 4 --stagger 90 --session 330 --out DIR

starts the gate that many times, one after another (each with its own fresh `OMNI_DATA_DIR`, its
own log and window), samples every instance's footprint and the system's compressor, swap and
`kern.memorystatus_level` every 2 s into `DIR/samples.csv`, and writes each instance's exit code and
whether it reached the landing screen to `DIR/meta.txt`. `docs/ports/macos-memory.md` has what it
measured.
"""

import argparse
import ctypes
import glob
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

RUSAGE_INFO_V4 = 4


class RusageInfoV4(ctypes.Structure):
    # <sys/resource.h>, struct rusage_info_v4, field for field.
    _fields_ = [
        ("ri_uuid", ctypes.c_uint8 * 16),
        ("ri_user_time", ctypes.c_uint64),
        ("ri_system_time", ctypes.c_uint64),
        ("ri_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_interrupt_wkups", ctypes.c_uint64),
        ("ri_pageins", ctypes.c_uint64),
        ("ri_wired_size", ctypes.c_uint64),
        ("ri_resident_size", ctypes.c_uint64),
        ("ri_phys_footprint", ctypes.c_uint64),
        ("ri_proc_start_abstime", ctypes.c_uint64),
        ("ri_proc_exit_abstime", ctypes.c_uint64),
        ("ri_child_user_time", ctypes.c_uint64),
        ("ri_child_system_time", ctypes.c_uint64),
        ("ri_child_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_child_interrupt_wkups", ctypes.c_uint64),
        ("ri_child_pageins", ctypes.c_uint64),
        ("ri_child_elapsed_abstime", ctypes.c_uint64),
        ("ri_diskio_bytesread", ctypes.c_uint64),
        ("ri_diskio_byteswritten", ctypes.c_uint64),
        ("ri_cpu_time_qos_default", ctypes.c_uint64),
        ("ri_cpu_time_qos_maintenance", ctypes.c_uint64),
        ("ri_cpu_time_qos_background", ctypes.c_uint64),
        ("ri_cpu_time_qos_utility", ctypes.c_uint64),
        ("ri_cpu_time_qos_legacy", ctypes.c_uint64),
        ("ri_cpu_time_qos_user_initiated", ctypes.c_uint64),
        ("ri_cpu_time_qos_user_interactive", ctypes.c_uint64),
        ("ri_billed_system_time", ctypes.c_uint64),
        ("ri_serviced_system_time", ctypes.c_uint64),
        ("ri_logical_writes", ctypes.c_uint64),
        ("ri_lifetime_max_phys_footprint", ctypes.c_uint64),
        ("ri_instructions", ctypes.c_uint64),
        ("ri_cycles", ctypes.c_uint64),
        ("ri_billed_energy", ctypes.c_uint64),
        ("ri_serviced_energy", ctypes.c_uint64),
        ("ri_interval_max_phys_footprint", ctypes.c_uint64),
        ("ri_runnable_time", ctypes.c_uint64),
    ]


LIBC = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
LIBC.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.POINTER(RusageInfoV4)]
LIBC.proc_pid_rusage.restype = ctypes.c_int

MIB = 1024 * 1024


def sample(pid):
    info = RusageInfoV4()
    if LIBC.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(info)) != 0:
        return None
    return info


def newest_matching(name):
    out = subprocess.run(["pgrep", "-n", "-f", name], capture_output=True, text=True).stdout
    return int(out.split()[0]) if out.split() else None


GATE_TEST = "initialize_native_code_returns_a_native_code_and_the_game_thread_starts"


def system_memory():
    """The compressor, free pages, swap and the kernel's free-memory level, in MiB and percent."""
    text = subprocess.run(["vm_stat"], capture_output=True, text=True).stdout
    page = int(re.search(r"page size of (\d+)", text).group(1))

    def pages(name):
        found = re.search(name + r":\s+(\d+)", text)
        return int(found.group(1)) * page / MIB if found else float("nan")

    swap = subprocess.run(["sysctl", "-n", "vm.swapusage"], capture_output=True, text=True).stdout
    used = re.search(r"used = ([\d.]+)M", swap)
    level = subprocess.run(["sysctl", "-n", "kern.memorystatus_level"], capture_output=True,
                           text=True).stdout.strip()
    return {
        "compressor_mib": pages("Pages occupied by compressor"),
        "compressed_mib": pages("Pages stored in compressor"),
        "free_mib": pages("Pages free"),
        "swap_used_mib": float(used.group(1)) if used else float("nan"),
        "free_level_pct": int(level) if level.isdigit() else -1,
    }


def launch(args):
    """Start `args.launch` gates `args.stagger` seconds apart and sample them all until they exit."""
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    binary = args.binary or max(
        (p for p in glob.glob(root + "/target/release/deps/gameactivity-*") if not p.endswith(".d")),
        key=os.path.getmtime)
    os.makedirs(args.out, exist_ok=True)
    meta = open(os.path.join(args.out, "meta.txt"), "w")
    meta.write(f"binary {binary}\nn {args.launch} stagger {args.stagger} session {args.session}\n")
    meta.write(f"system at start: {system_memory()}\n")
    procs, dirs, rows = [], [], []
    start = time.time()
    next_start = start
    while True:
        now = time.time()
        if len(procs) < args.launch and now >= next_start:
            data = tempfile.mkdtemp(prefix=f"omni-footprint-{len(procs)}-")
            dirs.append(data)
            env = dict(os.environ, OMNI_M6_ROWS_21_22="1", OMNI_GFX_WINDOW_TESTS="1",
                       OMNI_KEYBOARD_MOUSE="1", OMNI_SESSION_SECONDS=str(args.session),
                       OMNI_DATA_DIR=data)
            log = open(os.path.join(args.out, f"gate{len(procs)}.log"), "w")
            proc = subprocess.Popen([binary, "--nocapture", "--test-threads=1", GATE_TEST],
                                    cwd=os.path.join(root, "crates", "omni-android"), env=env,
                                    stdout=log, stderr=subprocess.STDOUT)
            procs.append((proc, now - start, log))
            next_start = now + args.stagger
        footprints = []
        for proc, _, _ in procs:
            info = sample(proc.pid) if proc.poll() is None else None
            footprints.append(info.ri_phys_footprint / MIB if info else 0.0)
        rows.append((now - start, footprints, system_memory()))
        if len(procs) == args.launch and all(p.poll() is not None for p, _, _ in procs):
            break
        time.sleep(2)
    with open(os.path.join(args.out, "samples.csv"), "w") as out:
        out.write("t," + ",".join(f"fp{i}" for i in range(args.launch)) +
                  ",total,compressor_mib,compressed_mib,free_mib,swap_used_mib,free_level_pct\n")
        for at, footprints, system in rows:
            footprints = footprints + [0.0] * (args.launch - len(footprints))
            out.write(f"{at:.1f}," + ",".join(f"{v:.1f}" for v in footprints) +
                      f",{sum(footprints):.1f},{system['compressor_mib']:.1f},"
                      f"{system['compressed_mib']:.1f},{system['free_mib']:.1f},"
                      f"{system['swap_used_mib']:.1f},{system['free_level_pct']}\n")
    for index, (proc, started, log) in enumerate(procs):
        log.close()
        text = open(os.path.join(args.out, f"gate{index}.log"), errors="replace").read()
        meta.write(f"instance {index}: started +{started:.0f}s pid {proc.pid} exit {proc.returncode} "
                   f"landing_lines {text.count('data(Landing)')}\n")
    meta.close()
    for data in dirs:
        shutil.rmtree(data, ignore_errors=True)
    print(open(os.path.join(args.out, "meta.txt")).read())
    return 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int)
    parser.add_argument("--match")
    parser.add_argument("--every", type=float, default=1.0)
    parser.add_argument("--seconds", type=float, default=0.0, help="0: until the process exits")
    parser.add_argument("--csv")
    parser.add_argument("--launch", type=int, help="start this many gates, one after another")
    parser.add_argument("--stagger", type=float, default=90.0)
    parser.add_argument("--session", type=int, default=330)
    parser.add_argument("--out", default="footprint-instances")
    parser.add_argument("--binary", help="the gate binary (default: the newest gameactivity-*)")
    args = parser.parse_args()
    if args.launch:
        return launch(args)
    pid = args.pid
    if pid is None and args.match:
        deadline = time.time() + 120
        while pid is None and time.time() < deadline:
            pid = newest_matching(args.match)
            time.sleep(0.2)
    if pid is None:
        print("no process to sample", file=sys.stderr)
        return 2
    started = time.time()
    rows = []
    while True:
        info = sample(pid)
        if info is None:
            break
        at = time.time() - started
        rows.append((at, info.ri_phys_footprint, info.ri_resident_size, info.ri_lifetime_max_phys_footprint))
        print(f"+{at:6.1f}s footprint {info.ri_phys_footprint / MIB:8.1f} MiB  resident "
              f"{info.ri_resident_size / MIB:8.1f} MiB  peak {info.ri_lifetime_max_phys_footprint / MIB:8.1f} MiB",
              flush=True)
        if args.seconds and at >= args.seconds:
            break
        time.sleep(args.every)
    if args.csv:
        with open(args.csv, "w") as handle:
            handle.write("seconds,phys_footprint,resident,lifetime_peak\n")
            for row in rows:
                handle.write(",".join(str(v) for v in row) + "\n")
    if rows:
        peak = max(r[3] for r in rows)
        last = rows[-1][1]
        print(f"pid {pid}: n = {len(rows)} samples every {args.every}s; lifetime peak {peak / MIB:.1f} MiB; "
              f"last {last / MIB:.1f} MiB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
