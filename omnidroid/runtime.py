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
    """True iff 127.0.0.1:port is already taken. Final collision guard: never
    issue a port a live QEMU answers on.

    BIND, not connect. This used to infer "free" from ConnectionRefusedError,
    which assumes the host RSTs a SYN sent to a CLOSED loopback port. That is
    true on BSD/macOS and Linux; it is NOT true on Windows hosts whose
    endpoint-security filter silently DROPS those SYNs. There every probe
    timed out, every timeout resolved to True ("ambiguous -> occupied", the
    safe direction), so allocate_ports() below walked port indices forever:
    `omni start` hung with zero output, no QEMU process, no timeout, on a host
    whose version/doctor/bases all passed. Binding asks the kernel directly
    and is authoritative on every platform.

    SO_REUSEADDR is deliberately NOT set. On Windows it permits binding a port
    another socket already holds, which would turn this guard into a
    rubber stamp and reintroduce exactly the collision it exists to prevent.

    `timeout` is accepted and ignored — a bind does not wait on the network.
    It stays in the signature because callers and tests pass it.
    """
    del timeout  # not meaningful for a bind; kept for signature compatibility
    s = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
    try:
        s.bind(("127.0.0.1", port))
        return False          # the kernel gave it to us -> nobody home -> free
    except OSError:
        return True           # in use (or unusable) -> occupied
    finally:
        s.close()


# The three port ranges are 1000 apart, so index 1000 would put an adb port
# on top of the qmp range. That makes 1000 the natural ceiling, not a guess.
_MAX_PORT_INDEX = 1000


def allocate_ports(cfg):
    """Lowest free port-index across RUNNING instances (a stopped instance
    frees its slot immediately). The three ranges are 1000 apart, so the shared
    index keeps adb/qmp/vnc aligned and collision-free below 1000 concurrent."""
    q = cfg["qemu"]
    # Scan CLAIMED slots (running instances AND live reservations), not just
    # running_instances() -- a concurrent launch that has reserved but not yet
    # spawned still holds its slot, so this closes the allocate/spawn race.
    used = {p - q["adb_port_start"] for p in _claimed_port_indices()}
    for i in range(_MAX_PORT_INDEX):
        if i in used:
            continue
        adb_port = q["adb_port_start"] + i
        qmp_port = q["qmp_port_start"] + i
        if _port_answers(qmp_port) or _port_answers(adb_port):
            continue
        return (adb_port, qmp_port, vnc_start(cfg) + i)
    # Bounded on purpose. This loop was `while True`, so a host that reported
    # every port occupied (see _port_answers) made `omni start` hang forever
    # with no output instead of failing. An undiagnosable host must fail fast
    # and name the thing that is wrong.
    raise RuntimeError(
        f"no free port index below {_MAX_PORT_INDEX}: every adb/qmp port in "
        f"{q['adb_port_start']}..{q['adb_port_start'] + _MAX_PORT_INDEX - 1} "
        f"reads as occupied. Either that many instances are really running, "
        f"or this host is blackholing loopback probes.")


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
        # The usual detached case (`omnidroid start` exits, QEMU reparents to init) is
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


# How long the QMP identity probe gets, on the paths that still need one.
#
# 0.25 s was measured to be far too short: a QEMU running a game answered
# `query-name` in 3 s, because QMP is serviced by the same main loop the guest
# is keeping busy. Reading that as "the instance is dead" orphaned a live
# 3.2 GB QEMU and stopped its memory governor. Four seconds is slower than
# anybody wants in a loop, which is exactly why the loop does not use it any
# more -- `process_start_ticks` does, and this is the fallback for records and
# platforms that cannot.
QMP_IDENTITY_TIMEOUT = 4.0


def _cmdline_has_token(pid, token):
    """True iff the Linux /proc/<pid>/cmdline arg vector contains `token`.
    Cheap identity confirmation that kills PID-recycle false positives with
    no socket. Returns False anywhere /proc is unavailable (macOS/Windows)."""
    try:
        raw = (_PROC / str(pid) / "cmdline").read_bytes()
    except (OSError, ValueError):
        return False
    return token.encode() in raw.split(b"\x00")


def process_start_ticks(pid):
    """When `pid` was created, as an opaque integer, or None if unknowable.

    THE POINT OF THIS IS TO KEEP QMP OUT OF THE LIVENESS PATH. A pid alone
    cannot say "this is still the process we spawned" because pids get
    recycled -- but a pid PLUS its creation time can, and that pair is what
    every process table in the world uses as a durable process identity. It
    costs microseconds, it cannot be starved, and unlike a socket round trip
    it does not care how busy the process is.

    MEASURED, and this is why it exists: `_qmp_name`'s 0.25 s budget is not
    enough for a QEMU running a game. On a live PS99 farming instance,
    `query-name` needed **3 s** -- QMP is serviced by QEMU's main loop, and
    that loop was busy with the guest and with one warning line per page of
    balloon traffic. So `instance_live()` said False about a perfectly healthy
    instance, and everything downstream believed it: `list` reported it
    stopped, `stop` could not stop it, and the MEMORY GOVERNOR exited with
    "QEMU process is gone" after a single shrink -- which is precisely the
    "it always uses the whole -m" symptom, with the instance left orphaned at
    3.2 GB and its ports still held.

    The value is only ever compared against another value from this same
    function on this same host, so its units do not matter and are not the
    same on every platform:

      * Windows -- the process creation FILETIME (100 ns since 1601).
      * Linux   -- field 22 of /proc/<pid>/stat, `starttime` in clock ticks
                   since boot.
      * elsewhere -- None, and callers fall back to the older checks.
    """
    if not pid:
        return None
    try:
        if IS_WINDOWS:
            import ctypes
            from ctypes import wintypes
            PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
            k = ctypes.windll.kernel32
            handle = k.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False,
                                   int(pid))
            if not handle:
                return None
            try:
                created = wintypes.FILETIME()
                exited = wintypes.FILETIME()
                kernel = wintypes.FILETIME()
                user = wintypes.FILETIME()
                if not k.GetProcessTimes(handle, ctypes.byref(created),
                                         ctypes.byref(exited),
                                         ctypes.byref(kernel),
                                         ctypes.byref(user)):
                    return None
                return ((created.dwHighDateTime << 32)
                        | created.dwLowDateTime)
            finally:
                k.CloseHandle(handle)
        stat = (_PROC / str(pid) / "stat").read_bytes()
        # The comm field is parenthesised and may itself contain spaces and
        # parentheses, so split after the LAST ')' rather than on whitespace.
        fields = stat[stat.rindex(b")") + 2:].split()
        return int(fields[19])          # field 22 overall == index 19 here
    except Exception:      # noqa: BLE001 - a probe must never raise
        return None


def _qmp_name(qmp_port, timeout=QMP_IDENTITY_TIMEOUT):
    """The guest name from QMP query-name on qmp_port, or None on any
    error/refusal.

    THE TIMEOUT IS NOT A HOT-PATH BUDGET ANY MORE. It used to be 0.25 s
    because this ran on every liveness check; it now runs only when
    `process_start_ticks` could not answer (an old run.json, an unsupported
    platform), so it can afford to be right. See QMP_IDENTITY_TIMEOUT.
    """
    from omnidroid.qemu_proc import qmp   # lazy: qemu_proc imports runtime_dir
    resp = qmp({"qmp_port": qmp_port}, "query-name", timeout=timeout)
    if not resp:
        return None
    return (resp.get("return") or {}).get("name")


def instance_live(rec):
    """Verified liveness: is `rec`'s recorded process THE QEMU for this
    instance (not a recycled pid, not a stranger)? Cheap-first.

    1. pid must be alive at all.
    2. pid + CREATION TIME match what spawn recorded -> live. This is the
       path that runs in practice, and the only one that cannot be starved
       by a busy guest -- see process_start_ticks for the measurement that
       moved it to the front.
    3. Legacy records (no identity) fall back to pid-only with a warning.
    4. Linux cheap path: /proc/<pid>/cmdline carries the -name token -> live.
    5. Last resort: QMP query-name equals the token -> live.

    Ambiguity resolves to NOT live, and that is still the safe direction for
    the question this was written for (never claim a stranger's pid). It is
    NOT safe for a false negative on our own process, which is why step 2
    exists: a mismatch there is definitive (the pid was recycled), and a
    match is definitive too, so neither answer has to be guessed at."""
    pid = rec.get("pid")
    if not pid_alive(pid):
        return False
    recorded = rec.get("pid_started")
    if recorded is not None:
        actual = process_start_ticks(pid)
        if actual is not None:
            return int(actual) == int(recorded)
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
                        # Whether THIS boot attached the devkit disk (vdc).
                        # A per-boot property, not a property of the account.
                        "debug": bool(data.get("debug")),
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


def trim_working_set(pid):
    """Ask Windows to evict this process's idle pages. Returns True if it ran.

    THE MISSING HALF OF THE BALLOON ON WINDOWS, and the reason the memory
    governor works here at all.

    QEMU cannot release guest RAM on Windows: `ram_block_discard_range()` is
    behind `CONFIG_MADVISE`, so every discard returns -ENOSYS and the pages the
    guest hands back stay in QEMU's working set forever. Every in-QEMU
    mechanism fails on that one fact -- balloon reclaim, capping at boot, and
    virtio-mem (which QEMU builds `depends on LINUX` for exactly this reason).

    But QEMU is not the only thing that can free those pages. `EmptyWorkingSet`
    is the OS asking the same question from outside the process, and it needs
    nothing from QEMU at all. MEASURED 2026-08-16 against a live instance:

        before                  1873 MB
        immediately after         18 MB
        settled 30 s later       131 MB   (guest re-faulted its live set)

    with the guest fully responsive throughout (adb answered in 0.2 s).

    It is paired with a balloon inflate rather than used alone, and the order
    matters: the inflate is what makes the spare pages genuinely COLD, so the
    guest never faults them back. Trimming without it would evict pages the
    guest still wants and simply buy a burst of page faults.
    """
    if not IS_WINDOWS:
        # Linux/macOS already decommit through the balloon's own discard path;
        # there is nothing left for an external trim to do.
        return False
    try:
        import ctypes
        PROCESS_QUERY_INFORMATION = 0x0400
        PROCESS_SET_QUOTA = 0x0100
        h = ctypes.windll.kernel32.OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_SET_QUOTA, False, int(pid))
        if not h:
            return False
        try:
            return bool(ctypes.windll.psapi.EmptyWorkingSet(h))
        finally:
            ctypes.windll.kernel32.CloseHandle(h)
    except Exception:      # noqa: BLE001 — an optimisation, never a failure
        return False


# Flags for SetProcessWorkingSetSizeEx. HARDWS_MAX_ENABLE is the one that
# matters: without it the maximum is a hint the memory manager may ignore
# entirely, and the whole point here is that it is not a hint.
QUOTA_LIMITS_HARDWS_MIN_DISABLE = 0x00000002
QUOTA_LIMITS_HARDWS_MAX_ENABLE = 0x00000004
# What the process is always allowed to keep. Small enough to be no constraint
# at the caps this project sets, present because the API takes a pair.
WORKING_SET_MIN_MB = 32


def cap_working_set(pid, max_mb):
    """Hold this process's resident memory at or below `max_mb`. True if set.

    THIS REPLACES `trim_working_set` AS THE WAY FARMING GIVES MEMORY BACK, and
    the difference between them is the difference between a working instance
    and a wedged one.

    `EmptyWorkingSet` evicts EVERYTHING, in one go. Run once that is merely
    dramatic -- MEASURED, 3192 MB -> 1 MB, back to ~150 MB in 20 s as the guest
    re-faults its live set. Run on a TIMER, which is what the governor did, it
    never converges: the next trim lands before the guest has faulted its set
    back, so the guest spends all its time faulting. MEASURED 2026-08-17,
    trimming every 30 s against a live PS99 farming instance:
    host RSS stuck at 1-37 MB, **adb stopped answering within 12 s**, the game
    was killed, and the guest never recovered even after the trims stopped.

    A hard working-set MAXIMUM asks the memory manager for the same thing and
    lets IT choose which pages and when: it trims least-recently-used pages
    continuously and keeps the process exactly at the ceiling. Same instance,
    same game, capped instead of trimmed:

        cap      host RSS    game     adb round trip
        (none)      3417     alive    0.05 s
        1000        1000     alive    0.05 s
        800          800     alive    0.05 s
        650          650     alive    0.04 s
        500          500     alive    0.10 s
        384          384     DEAD     0.04 s
        300          300     DEAD     0.04 s

    -- and at 650 MB, over 90 s: the client burned 149% of a guest core (it is
    genuinely playing, not merely resident), the host saw 2570 page faults a
    second, and QEMU read **0.09 MB/s** off disk. That last number is the one
    that makes this safe: the faults are SOFT, served from the standby list at
    RAM speed, so the pages come back without touching the pagefile.

    Below ~500 MB PS99 is killed in-guest. The floor is the game's, not the
    mechanism's.

    Windows only. Linux and macOS decommit for real through the balloon's own
    discard path, so there is nothing here for them to do -- returns False,
    and callers treat that as "this host does it another way", never as an
    error.
    """
    if not IS_WINDOWS or not pid or not max_mb:
        return False
    try:
        import ctypes
        from ctypes import wintypes
        PROCESS_SET_QUOTA = 0x0100
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        k = ctypes.windll.kernel32
        k.SetProcessWorkingSetSizeEx.argtypes = [
            wintypes.HANDLE, ctypes.c_size_t, ctypes.c_size_t, wintypes.DWORD]
        k.SetProcessWorkingSetSizeEx.restype = wintypes.BOOL
        h = k.OpenProcess(PROCESS_SET_QUOTA
                          | PROCESS_QUERY_LIMITED_INFORMATION, False, int(pid))
        if not h:
            return False
        try:
            return bool(k.SetProcessWorkingSetSizeEx(
                h, int(WORKING_SET_MIN_MB) << 20, int(max_mb) << 20,
                QUOTA_LIMITS_HARDWS_MAX_ENABLE))
        finally:
            k.CloseHandle(h)
    except Exception:      # noqa: BLE001 — an optimisation, never a failure
        return False


def uncap_working_set(pid):
    """Give the process its memory back: no ceiling, OS defaults.

    The symmetric operation, and it has a real caller: a farming instance the
    user opens a window on is not a farming instance any more for as long as
    they are watching it, and a 650 MB ceiling on a window somebody is looking
    at is frame-time they can see.
    """
    if not IS_WINDOWS or not pid:
        return False
    try:
        import ctypes
        from ctypes import wintypes
        PROCESS_SET_QUOTA = 0x0100
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        k = ctypes.windll.kernel32
        k.SetProcessWorkingSetSizeEx.argtypes = [
            wintypes.HANDLE, ctypes.c_size_t, ctypes.c_size_t, wintypes.DWORD]
        k.SetProcessWorkingSetSizeEx.restype = wintypes.BOOL
        h = k.OpenProcess(PROCESS_SET_QUOTA
                          | PROCESS_QUERY_LIMITED_INFORMATION, False, int(pid))
        if not h:
            return False
        try:
            # (size_t)-1 for both is the documented "reset to default" pair.
            return bool(k.SetProcessWorkingSetSizeEx(
                h, ctypes.c_size_t(-1).value, ctypes.c_size_t(-1).value, 0))
        finally:
            k.CloseHandle(h)
    except Exception:      # noqa: BLE001
        return False


# Job-object plumbing for the CPU ceiling. The numbers are the Win32 ones and
# they are the API, so they are written as themselves rather than hidden.
_JobObjectCpuRateControlInformation = 15
_JOB_CPU_RATE_CONTROL_ENABLE = 0x1
_JOB_CPU_RATE_CONTROL_HARD_CAP = 0x4
# AssignProcessToJobObject needs PROCESS_SET_QUOTA *and* PROCESS_TERMINATE --
# a job may kill its members, so the caller has to be allowed to. Opening
# without TERMINATE fails with ACCESS_DENIED and nothing else explains why.
_PROCESS_QUERY_INFORMATION = 0x0400
_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
_PROCESS_SET_QUOTA = 0x0100
_PROCESS_TERMINATE = 0x0001


class CpuCeiling:
    """A live CPU ceiling on one process. Holding this object IS the ceiling.

    WHY AN OBJECT AND NOT A FUNCTION. The limit lives on a Windows job object,
    and a job object dies with its last handle -- so whatever applies the cap
    has to stay alive to keep it. The memory governor already runs for the
    life of the instance, which makes it the natural owner; a fire-and-forget
    `cap_cpu(pid, 50)` would be undone the moment the caller returned, and
    would look like it worked.

    Closing it (or exiting) removes the limit and leaves the process running:
    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is deliberately NOT set, because a
    governor that crashed must never take a farming instance with it.
    """

    def __init__(self, pid):
        self._job = None
        self._proc = None
        self.cores = 1
        self.attached = False
        if not IS_WINDOWS or not pid:
            return
        try:
            import ctypes
            k = ctypes.windll.kernel32
            self.cores = max(1, k.GetActiveProcessorCount(0xFFFF))
            job = k.CreateJobObjectW(None, None)
            if not job:
                return
            proc = k.OpenProcess(
                _PROCESS_QUERY_INFORMATION | _PROCESS_QUERY_LIMITED_INFORMATION
                | _PROCESS_SET_QUOTA | _PROCESS_TERMINATE, False, int(pid))
            if not proc:
                k.CloseHandle(job)
                return
            if not k.AssignProcessToJobObject(job, proc):
                k.CloseHandle(proc)
                k.CloseHandle(job)
                return
            self._job, self._proc, self.attached = job, proc, True
        except Exception:      # noqa: BLE001 - an optimisation, never a failure
            self._job = self._proc = None

    def set(self, percent_of_one_core):
        """Hold the process at `percent_of_one_core`% of ONE core. True if set.

        The API takes a share of the WHOLE MACHINE in hundredths of a percent,
        which is a terrible unit to think in when what you mean is "half a
        core"; the conversion lives here so every caller can speak in cores.

        MEASURED on a live PS99 farming instance, 24 logical processors:

            cap          host CPU    client   adb
            (none)         160.9%    alive    0.05 s
            100% core      102.0%    alive    0.05 s
            70%             71.9%    alive    0.37 s
            50%             49.9%    alive    0.06 s
            35%             35.5%    alive    0.07 s

        The host tracks the cap exactly and the guest stays responsive
        throughout. This is the lever for instance COUNT: farming's cost is
        the game's own arm64 translation (148% of a guest core, against
        SurfaceFlinger's 6.7% -- rendering is not the expense), and that is
        not something the guest can be asked to do less of.
        """
        if not self.attached:
            return False
        try:
            import ctypes
            from ctypes import wintypes

            class RATE(ctypes.Structure):
                class _U(ctypes.Union):
                    _fields_ = [("CpuRate", wintypes.DWORD),
                                ("Weight", wintypes.DWORD)]
                _anonymous_ = ("u",)
                _fields_ = [("ControlFlags", wintypes.DWORD), ("u", _U)]

            info = RATE()
            info.ControlFlags = (_JOB_CPU_RATE_CONTROL_ENABLE
                                 | _JOB_CPU_RATE_CONTROL_HARD_CAP)
            share = float(percent_of_one_core) / self.cores
            # 1..10000, in hundredths of a percent of the whole machine.
            info.CpuRate = max(1, min(10000, int(round(share * 100))))
            return bool(ctypes.windll.kernel32.SetInformationJobObject(
                self._job, _JobObjectCpuRateControlInformation,
                ctypes.byref(info), ctypes.sizeof(info)))
        except Exception:      # noqa: BLE001
            return False

    def close(self):
        try:
            import ctypes
            for handle in (self._proc, self._job):
                if handle:
                    ctypes.windll.kernel32.CloseHandle(handle)
        except Exception:      # noqa: BLE001
            pass
        self._job = self._proc = None
        self.attached = False


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


def reconcile_runtime():
    """Sweep runtime/*: GC directories whose recorded process is dead AND whose
    ports are silent; report (do NOT adopt) any run.json-less directory. Returns
    {"gc": [names], "orphans": [names]}. Synchronous — callers trigger it."""
    result = {"gc": [], "orphans": []}
    root = config.data_dir() / "runtime"
    if not root.exists():
        return result
    for d in sorted(root.iterdir()):
        rj = d / "run.json"
        if not rj.exists():
            result["orphans"].append(d.name)
            continue
        try:
            data = json.loads(rj.read_text())
        except Exception:  # noqa: BLE001
            continue
        if data.get("reserving"):
            continue
        if instance_live(data):
            continue
        qmp_port = data.get("qmp_port")
        adb_port = data.get("adb_port")
        silent = not ((qmp_port and _port_answers(qmp_port))
                      or (adb_port and _port_answers(adb_port)))
        if silent:
            _wipe_runtime(d.name)
            result["gc"].append(d.name)
    try:
        from omnidroid import warmcache
        from omnidroid.engine import read_config
        from omnidroid.config import images_dir
        warmcache.prune_staging(images_dir(read_config()))
    except Exception:      # noqa: BLE001 - housekeeping never fails a command
        pass
    return result


def running_pid(name):
    """The live QEMU pid for `name`, or None. NEVER raises on a bad file.

    An unreadable or malformed run.json reads as "not running" rather than
    propagating a JSONDecodeError. This is the single most-called predicate
    in the product -- `list`, `stop`, `view` and `start` all go through it --
    so an exception here does not fail one command, it takes out every
    command including the one that would clean the mess up, leaving an
    instance nobody can stop.

    The writer is atomic (engine._write_run_record: tmp + os.replace), so a
    partial file should not exist in the first place; this guard is the
    backstop for the cases atomicity cannot cover -- a truncated file left by
    an older build, a disk that filled mid-write, a half-restored backup.
    """
    p = runtime_dir(name) / "run.json"
    if not p.exists():
        return None
    try:
        data = json.loads(p.read_text())
    except Exception:      # noqa: BLE001 - see the docstring
        return None
    # A reservation (build_acct's pre-spawn run.json, carrying the live LAUNCHER
    # pid) is NOT a running instance: no QEMU exists yet. spawn_qemu overwrites
    # it with the real QEMU pid and no `reserving` flag. Treating a reservation
    # as running would make _ensure_booted skip the spawn and, after the
    # launcher exits, leave the real QEMU untracked.
    if data.get("reserving"):
        return None
    pid = data.get("pid")
    return pid if instance_live(data) else None


def warm_keys_in_use():
    """Golden-entry keys backing a RUNNING instance right now.

    Eviction uses it so a live instance's disks are never deleted, and the
    interim concurrency rule uses it so a second launch against an in-use
    entry cold-boots instead of landing `offline` on adb (design spec 8b).

    A housekeeping sweep on the boot path: an unreadable runtime root
    (permissions damage, a half-mounted volume) degrades to "no keys in
    use" rather than raising -- same treatment as warmcache.py's own
    `_safe_iterdir()`, and for the same reason (Path.iterdir() raises
    PermissionError/OSError on a directory that exists but can't be read,
    unlike Path.glob()). One bad instance directory (unreadable run.json,
    or a directory that vanishes mid-sweep) must not abort the sweep for
    every other instance either.
    """
    keys = set()
    root = config.runtime_root()
    if not root.is_dir():
        return keys
    try:
        entries = list(root.iterdir())
    except OSError:
        return keys
    for d in entries:
        if not d.is_dir():
            continue
        try:
            data = json.loads((d / "run.json").read_text())
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict):
            # A run.json can be valid JSON and still not be an object (e.g.
            # `[]` or `"x"`) -- same guard warmcache.read_meta() already
            # applies to meta.json, and for the same reason: one malformed
            # sibling directory must not raise into every other launch's
            # cache-key resolution.
            continue
        key = data.get("warm_key")
        if not key:
            continue
        try:
            live = running_pid(d.name)
        except (OSError, ValueError):
            continue
        if live:
            keys.add(key)
    return keys


def live_qemu_pids():
    """Pids of the QEMU processes this install currently has running.

    Used by the scratch reaper to avoid unlinking an overlay out from under a
    guest on POSIX, where an open file unlinks happily and the instance loses
    every write it has made. On Windows the open handle refuses the unlink by
    itself, which is why the reaper is safe there without consulting this.

    Same degradation rule as warm_keys_in_use(): an unreadable runtime root is
    "no pids", never an exception, because this runs on the boot path."""
    pids = set()
    root = config.runtime_root()
    if not root.is_dir():
        return pids
    try:
        entries = list(root.iterdir())
    except OSError:
        return pids
    for d in entries:
        if not d.is_dir():
            continue
        try:
            pid = running_pid(d.name)
        except (OSError, ValueError):
            continue
        if pid:
            pids.add(int(pid))
    return pids
