# omnidroid/runtime.py
"""Per-instance runtime tracking: port allocation, run.json reservations,
and process-liveness checks. run.json is the claim; the running QEMU is the
truth (see verified-liveness, Tasks 8-12)."""
import contextlib
import json
import os
import pathlib
import re
import socket as _socket
import time
from pathlib import Path

from omnidroid import config
from omnidroid.config import IS_WINDOWS


_PROC = pathlib.Path("/proc")   # overridable in tests


# Per-instance PORT SCHEME (documented invariant):
#   instance index i (0-based)  ->  adb = adb_port_start + i   (16001+)
#                                   qmp = qmp_port_start + i   (17001+)
#                                   vnc = vnc_port_start + i   (18001+)
# One shared index per account keeps the triple aligned; the three ranges
# are 1000 apart, so adb/qmp/vnc can NEVER collide below 1000 instances
# (and instance counts are host-RAM-bound long before that). vnc_port is
# WIRED: QEMU's built-in VNC server listens on it, 127.0.0.1 ONLY. No
# auth — that is safe ONLY because of the localhost bind (HARD RULE:
# never bind VNC to a network interface without adding auth).
VNC_PORT_START_DEFAULT = 18001


def vnc_start(cfg):
    return cfg["qemu"].get("vnc_port_start", VNC_PORT_START_DEFAULT)


def _port_answers(port, timeout=0.25):
    """True iff something accepts a TCP connection on 127.0.0.1:port. Final
    collision guard: never issue a port a live QEMU answers on. A refused
    connection means free; any other socket error resolves to True (treat as
    occupied) — ambiguity costs one port index, never a collision."""
    try:
        with _socket.create_connection(("127.0.0.1", port), timeout=timeout):
            return True
    except ConnectionRefusedError:
        return False          # nobody home -> free
    except OSError:
        return True           # ambiguous -> treat as occupied (safe direction)


def allocate_ports(cfg):
    """Lowest free port-index across RUNNING instances (a stopped instance
    frees its slot immediately). The three ranges are 1000 apart, so the shared
    index keeps adb/qmp/vnc aligned and collision-free below 1000 concurrent."""
    q = cfg["qemu"]
    # Scan CLAIMED slots (running instances AND live reservations), not just
    # running_instances() -- a concurrent launch that has reserved but not yet
    # spawned still holds its slot, so this closes the allocate/spawn race.
    used = {p - q["adb_port_start"] for p in _claimed_port_indices()}
    i = 0
    while True:
        if i in used:
            i += 1
            continue
        adb_port = q["adb_port_start"] + i
        qmp_port = q["qmp_port_start"] + i
        if _port_answers(qmp_port) or _port_answers(adb_port):
            i += 1
            continue
        return (adb_port, qmp_port, vnc_start(cfg) + i)


@contextlib.contextmanager
def _launch_lock():
    """Serialize the allocate-ports + reserve-slot critical section across
    concurrent `start` launches on one host. Without it, two parallel launches
    race to the same free port index. POSIX flock; a no-op on Windows (the
    120-concurrent farm is Linux, dev is macOS -- both POSIX; the shipped
    Windows product launches one instance at a time)."""
    lock_path = config.runtime_root() / ".launch.lock"
    f = open(lock_path, "w")
    try:
        try:
            import fcntl
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        except (ImportError, OSError):
            pass   # Windows / no-flock: degrade to no lock (single-launch host)
        yield
    finally:
        f.close()


# ---------- process helpers ----------

def pid_alive(pid):
    if pid is None:
        return False
    if IS_WINDOWS:
        # NEVER use os.kill(pid, 0) on Windows: it TERMINATES the process.
        import ctypes
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        STILL_ACTIVE = 259
        h = ctypes.windll.kernel32.OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not h:
            return False
        code = ctypes.c_ulong()
        ok = ctypes.windll.kernel32.GetExitCodeProcess(h, ctypes.byref(code))
        ctypes.windll.kernel32.CloseHandle(h)
        return bool(ok) and code.value == STILL_ACTIVE
    else:
        import os
        # Reap first, and treat a zombie as DEAD. os.kill(pid, 0) succeeds on a
        # zombie, so a QEMU we spawned IN-PROCESS (update-kiosk, play's
        # _ensure_booted) reports as "still running" after it has exited — which
        # made _shutdown escalate powerdown -> QMP quit -> SIGKILL against an
        # already-dead process and then return 'kill-failed'. Callers ask "is the
        # instance running?"; a zombie is not.
        #
        # The usual detached case (`omni start` exits, QEMU reparents to init) is
        # unaffected: waitpid raises ChildProcessError and we fall through.
        try:
            wpid, _status = os.waitpid(pid, os.WNOHANG)
            if wpid == pid:
                return False          # exited; just reaped it
        except (ChildProcessError, OSError):
            pass                      # not our child — the normal case
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False


def _cmdline_has_token(pid, token):
    """True iff the Linux /proc/<pid>/cmdline arg vector contains `token`.
    Cheap identity confirmation that kills PID-recycle false positives with
    no socket. Returns False anywhere /proc is unavailable (macOS/Windows)."""
    try:
        raw = (_PROC / str(pid) / "cmdline").read_bytes()
    except (OSError, ValueError):
        return False
    return token.encode() in raw.split(b"\x00")


def _qmp_name(qmp_port, timeout=0.25):
    """The guest name from QMP query-name on qmp_port, or None on any
    error/refusal. Fast (short timeout) — runs in hot paths."""
    from omnidroid.qemu_proc import qmp   # lazy: qemu_proc imports runtime_dir
    resp = qmp({"qmp_port": qmp_port}, "query-name", timeout=timeout)
    if not resp:
        return None
    return (resp.get("return") or {}).get("name")


def instance_live(rec):
    """Verified liveness: is `rec`'s recorded process THE QEMU for this
    instance (not a recycled pid, not a stranger)? Cheap-first.

    1. pid must be alive at all.
    2. Legacy records (no identity) fall back to pid-only with a warning.
    3. Linux cheap path: /proc/<pid>/cmdline carries the -name token -> live.
    4. Authoritative path: QMP query-name equals the token -> live.
    Any ambiguity resolves to NOT live (safe direction: frees nothing that
    is actually answering, and never claims a stranger)."""
    pid = rec.get("pid")
    if not pid_alive(pid):
        return False
    token = rec.get("identity")
    if not token:
        import sys
        sys.stderr.write(
            f"warn: {rec.get('name')} run.json has no identity; "
            f"trusting pid {pid} (pre-upgrade record)\n")
        return True
    if _cmdline_has_token(pid, token):
        return True
    return _qmp_name(rec.get("qmp_port")) == token


def expected_identity(rec):
    """The QEMU -name token for this instance: f"omni-{name}". Both
    qemu_command and qemu_command_arm emit exactly this, so it is readable
    back from /proc/<pid>/cmdline and from QMP query-name."""
    return f"omni-{rec['name']}"


def runtime_dir(username):
    """Per-instance throwaway dir: efivars, run.json (ports+pid), qemu.log,
    autocap frames. Wiped on `stop` and `remove` (see _wipe_runtime).
    Replaces the old accounts/<name>/ for the product path."""
    return config.runtime_root() / username


def _reserve_ports(name, adb_port, qmp_port, vnc_port):
    """Claim a port slot for `name` by writing a run.json reservation with THIS
    launcher process's pid, so a concurrent allocate_ports() (which counts
    runtime/*/run.json with a live pid) sees the slot as taken until
    spawn_qemu() overwrites it with the real QEMU pid. Self-healing: if the
    launch aborts before spawn, the launcher exits, its pid dies, and
    running_instances() stops counting the stale reservation -> slot freed."""
    d = runtime_dir(name)
    d.mkdir(parents=True, exist_ok=True)
    (d / "run.json").write_text(json.dumps(
        {"pid": os.getpid(), "started": time.time(), "reserving": True,
         "adb_port": adb_port, "qmp_port": qmp_port, "vnc_port": vnc_port}))


def _wipe_runtime(name):
    """Delete the per-instance runtime dir (efivars.fd, run.json, qemu.log,
    autocap frames) once an ephemeral instance (build_acct) has stopped.
    Called from cmd_stop (after a successful power-off) and cmd_remove.
    Ephemeral instances write nothing under accounts/<name>/, so this IS
    the entire teardown -- no folder to remove there."""
    import shutil
    shutil.rmtree(runtime_dir(name), ignore_errors=True)


def running_instances():
    """Every instance with a LIVE qemu pid, read from runtime/*/run.json.
    Dead/stale run.json files are ignored. Returns dicts with name + ports."""
    out = []
    root = config.data_dir() / "runtime"
    if not root.exists():
        return out
    for d in sorted(root.iterdir()):
        rj = d / "run.json"
        if not rj.exists():
            continue
        try:
            data = json.loads(rj.read_text())
        except Exception:  # noqa: BLE001
            continue
        # A reservation is not a running instance (see running_pid). It holds a
        # port slot (allocate_ports scans _claimed_port_indices, which DOES
        # count live reservations) but must not appear as a live game to
        # list/all_accounts/_ensure_booted.
        if data.get("reserving"):
            continue
        if instance_live(data):
            out.append({"name": d.name, "pid": data["pid"],
                        "base": data.get("base"),
                        "adb_port": data.get("adb_port"),
                        "qmp_port": data.get("qmp_port"),
                        "vnc_port": data.get("vnc_port")})
    return out


def _claimed_port_indices():
    """Port indices currently CLAIMED across runtime/*/run.json -- a slot is
    claimed by a live running instance OR a live reservation (build_acct's
    pre-spawn run.json). This is what allocate_ports must avoid, so a concurrent
    launch that has reserved-but-not-yet-spawned still blocks the slot. A dead
    pid (crashed launcher, exited QEMU) frees its slot."""
    root = config.data_dir() / "runtime"
    claimed = set()
    if not root.exists():
        return claimed
    for d in sorted(root.iterdir()):
        rj = d / "run.json"
        if not rj.exists():
            continue
        try:
            data = json.loads(rj.read_text())
        except Exception:  # noqa: BLE001
            continue
        if data.get("adb_port") is not None and instance_live(data):
            claimed.add(data["adb_port"])
    return claimed


def host_rss_mb(pid):
    """Resident memory of a host process, in MB."""
    try:
        if IS_WINDOWS:
            import ctypes
            import ctypes.wintypes as wt

            class PMC(ctypes.Structure):
                _fields_ = [("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
                            ("PeakWorkingSetSize", ctypes.c_size_t),
                            ("WorkingSetSize", ctypes.c_size_t),
                            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                            ("PagefileUsage", ctypes.c_size_t),
                            ("PeakPagefileUsage", ctypes.c_size_t)]
            PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
            h = ctypes.windll.kernel32.OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
            if not h:
                return None
            pmc = PMC()
            pmc.cb = ctypes.sizeof(PMC)
            ok = ctypes.windll.psapi.GetProcessMemoryInfo(
                h, ctypes.byref(pmc), pmc.cb)
            ctypes.windll.kernel32.CloseHandle(h)
            return pmc.WorkingSetSize / (1024 * 1024) if ok else None
        else:
            txt = Path(f"/proc/{pid}/status").read_text()
            m = re.search(r"VmRSS:\s+(\d+) kB", txt)
            return int(m.group(1)) / 1024 if m else None
    except Exception:
        return None


def host_mem_available_mb():
    """Host free-for-use memory in MB (the number that decides how many
    instances fit). Linux: MemAvailable. Windows: ullAvailPhys."""
    try:
        if IS_WINDOWS:
            import ctypes

            class MEMSTAT(ctypes.Structure):
                _fields_ = [("dwLength", ctypes.c_uint32),
                            ("dwMemoryLoad", ctypes.c_uint32),
                            ("ullTotalPhys", ctypes.c_uint64),
                            ("ullAvailPhys", ctypes.c_uint64),
                            ("ullTotalPageFile", ctypes.c_uint64),
                            ("ullAvailPageFile", ctypes.c_uint64),
                            ("ullTotalVirtual", ctypes.c_uint64),
                            ("ullAvailVirtual", ctypes.c_uint64),
                            ("ullAvailExtendedVirtual", ctypes.c_uint64)]
            st = MEMSTAT()
            st.dwLength = ctypes.sizeof(MEMSTAT)
            if not ctypes.windll.kernel32.GlobalMemoryStatusEx(
                    ctypes.byref(st)):
                return None
            return st.ullAvailPhys / (1024 * 1024)
        txt = Path("/proc/meminfo").read_text()
        m = re.search(r"MemAvailable:\s+(\d+) kB", txt)
        return int(m.group(1)) / 1024 if m else None
    except Exception:
        return None


def running_pid(name):
    p = runtime_dir(name) / "run.json"
    if not p.exists():
        return None
    data = json.loads(p.read_text())
    # A reservation (build_acct's pre-spawn run.json, carrying the live LAUNCHER
    # pid) is NOT a running instance: no QEMU exists yet. spawn_qemu overwrites
    # it with the real QEMU pid and no `reserving` flag. Treating a reservation
    # as running would make _ensure_booted skip the spawn and, after the
    # launcher exits, leave the real QEMU untracked.
    if data.get("reserving"):
        return None
    pid = data.get("pid")
    return pid if instance_live(data) else None
