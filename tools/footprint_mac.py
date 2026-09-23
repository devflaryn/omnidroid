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
"""

import argparse
import ctypes
import subprocess
import sys
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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int)
    parser.add_argument("--match")
    parser.add_argument("--every", type=float, default=1.0)
    parser.add_argument("--seconds", type=float, default=0.0, help="0: until the process exits")
    parser.add_argument("--csv")
    args = parser.parse_args()
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
