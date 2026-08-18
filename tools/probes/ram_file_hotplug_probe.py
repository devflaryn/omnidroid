"""Standing smoke check for the omni-win32-ram-file patch series (0007+).

WHY THIS EXISTS. Both Criticals fix-round 1 found in patch 0007 are invisible
to a boot-and-measure test: `mr->align` staying 0 only blows up the first time
something hot-plugs a `memory-backend-ram` device against a block this
allocator touched (STATUS_INTEGER_DIVIDE_BY_ZERO, no message, QMP socket
reset), and the VirtualFree-on-a-mapped-view leak only shows up across
`object-add`/`object-del` cycles. `omnidroid/engine.py`'s `plug_mem` /
`unplug_mem` (the growth/shrink governor) do exactly this in production, at
`MEM_STEP_MB = 512` -- comfortably over the 64 MiB omni threshold -- the
moment `QEMU_RAM_FILE_DIR` is set for real (Task 6). Both bugs are
two-minutes-of-QMP away and neither leaves a trace in a paused, unplugged
boot.

WHAT IT DOES. Boots the target QEMU twice (env unset, then
`QEMU_RAM_FILE_DIR` set), and against each:
  1. object-add a memory-backend-ram + device_add pc-dimm (512 MB, matching
     MEM_STEP_MB) and confirm the process is still alive afterward -- this is
     Critical 1's repro. Left plugged: a hot-UNPLUG needs the guest OS to
     offline the range first, which a `-S`-paused guest never will, so that
     part of `unplug_mem`'s real path cannot be probed against a paused
     boot -- this is not attempted here, and is not what Critical 2 is.
  2. Three bare object-add/object-del cycles (memory-backend-ram objects
     created and destroyed WITHOUT ever being wired into the guest via
     device_add) and confirm process handle count and *.bin file count in
     QEMU_RAM_FILE_DIR both return to baseline rather than climbing -- this
     is Critical 2's repro. `object-del` on an unattached backend frees the
     RAMBlock through QOM unref immediately, no guest cooperation needed,
     which is why this is the shape that actually isolates the allocator's
     free path from ACPI hot-unplug timing.

USAGE.
    python tools/probes/ram_file_hotplug_probe.py <path-to-qemu-system-x86_64.exe> <ram-file-dir>

Exit code 0 means both checks passed on both runs. Anything else, plus a
printed diagnosis, means a regression -- run this before believing patch
0007 (or anything layered on it, e.g. 0008's discard path) is safe to ship.
"""
from __future__ import annotations

import ctypes
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
sys.path.insert(0, str(REPO))
from omnidroid.qmpsession import QmpSession  # noqa: E402

kernel32 = ctypes.WinDLL("kernel32")
MEM_STEP_MB = 512  # omnidroid.engine.MEM_STEP_MB -- kept literal, not
                   # imported, so this probe has no import-time dependency on
                   # engine.py's much larger module graph.


def _free_tcp_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _handle_count(pid: int) -> int:
    h = kernel32.OpenProcess(0x0400, False, pid)  # PROCESS_QUERY_INFORMATION
    if not h:
        raise ctypes.WinError()
    try:
        n = ctypes.c_ulong(0)
        if not kernel32.GetProcessHandleCount(h, ctypes.byref(n)):
            raise ctypes.WinError()
        return n.value
    finally:
        kernel32.CloseHandle(h)


def _ram_file_count(ram_dir: str) -> int:
    p = Path(ram_dir)
    if not p.is_dir():
        return 0
    return len(list(p.glob("omni-ram-*.bin")))


def _boot(exe: str, biosdir: str, extra_env: dict | None):
    env = os.environ.copy()
    if extra_env:
        env.update(extra_env)
    port = _free_tcp_port()
    argv = [exe, "-accel", "whpx",
            "-m", "size=1536,slots=4,maxmem=4096M",
            "-display", "none", "-S",
            "-qmp", f"tcp:127.0.0.1:{port},server=on,wait=off",
            "-L", biosdir]
    proc = subprocess.Popen(argv, env=env, cwd=os.path.dirname(exe))
    qmp = QmpSession(port, connect_timeout=30.0)
    return proc, qmp


def _alive(proc) -> bool:
    return proc.poll() is None


def _safe_cmd(qmp, execute, arguments=None):
    """qmp.cmd(), but a process that dies mid-command (this probe's whole
    reason to exist: Critical 1 killed QEMU with STATUS_INTEGER_DIVIDE_BY_ZERO
    partway through device_add, which severs the QMP socket) degrades to the
    same {"error": ...} shape as a clean QMP-level error, instead of an
    uncaught ConnectionResetError/OSError blowing up this script. QmpSession
    .cmd() already does this for a failed WRITE; the read loop's
    self._f.readline() has no equivalent guard, which this probe's first,
    unhardened run against a genuinely broken build hit directly."""
    try:
        return qmp.cmd(execute, arguments)
    except OSError as e:
        return {"error": {"desc": f"QMP connection died: {e}"}}


def _plug_one(qmp, n, mb=MEM_STEP_MB):
    mem_id, dev_id = f"probemem{n}", f"probedimm{n}"
    r = _safe_cmd(qmp, "object-add", {"qom-type": "memory-backend-ram",
                                      "id": mem_id, "size": mb * 1024 * 1024})
    if "error" in r:
        return None, f"object-add failed: {r['error']}"
    r = _safe_cmd(qmp, "device_add", {"driver": "pc-dimm", "id": dev_id,
                                      "memdev": mem_id})
    if "error" in r:
        _safe_cmd(qmp, "object-del", {"id": mem_id})
        return None, f"device_add failed: {r['error']}"
    return (mem_id, dev_id), None


def _add_unattached(qmp, n, mb=MEM_STEP_MB):
    """object-add a memory-backend-ram that is never device_add'd. Its
    RAMBlock is allocated exactly the way a device_add'd one is (both go
    through ram_block_add), but freeing it needs no guest cooperation: QOM
    unref on object-del calls the destructor synchronously from QMP's
    perspective, RCU-deferred only by one grace period, not gated on the
    guest offlining an address range the way a real DIMM unplug is."""
    mem_id = f"probemem{n}"
    r = _safe_cmd(qmp, "object-add", {"qom-type": "memory-backend-ram",
                                      "id": mem_id, "size": mb * 1024 * 1024})
    if "error" in r:
        return None, f"object-add failed: {r['error']}"
    return mem_id, None


def _del_unattached(qmp, mem_id, settle=0.5):
    r = _safe_cmd(qmp, "object-del", {"id": mem_id})
    # Let the RCU callback (call_rcu(block, reclaim_ramblock, rcu) in
    # ram_block_add's cleanup) actually run before the next measurement --
    # it is asynchronous, not synchronous with the QMP reply.
    time.sleep(settle)
    return r


def run_one(tag: str, exe: str, biosdir: str, ram_dir: str | None):
    print(f"=== {tag} ===")
    extra_env = {"QEMU_RAM_FILE_DIR": ram_dir} if ram_dir else None
    proc, qmp = _boot(exe, biosdir, extra_env)
    ok = True
    try:
        # --- Critical 1 repro: one hot-plug must not kill the process ---
        ids, err = _plug_one(qmp, 0)
        if err:
            print(f"  FAIL: initial plug_mem-equivalent errored: {err}")
            ok = False
        elif not _alive(proc):
            print(f"  FAIL: process died immediately after the plug "
                  f"(exit {proc.poll():#x}) -- this is the "
                  f"STATUS_INTEGER_DIVIDE_BY_ZERO repro")
            ok = False
        else:
            print(f"  ok: DIMM plug survived, process alive (pid {proc.pid})")

        if ok:
            # --- Critical 2 repro: leak across object-add/object-del,
            # deliberately NOT wired into the guest (see _add_unattached) ---
            base_handles = _handle_count(proc.pid)
            base_files = _ram_file_count(ram_dir) if ram_dir else 0
            for i in range(1, 4):
                mem_id, err = _add_unattached(qmp, i)
                if err:
                    print(f"  FAIL: cycle {i} object-add errored: {err}")
                    ok = False
                    break
                if not _alive(proc):
                    print(f"  FAIL: process died during cycle {i}")
                    ok = False
                    break
                r = _del_unattached(qmp, mem_id)
                if "error" in r:
                    print(f"  FAIL: cycle {i} object-del errored: {r['error']}")
                    ok = False
                    break
            if ok:
                end_handles = _handle_count(proc.pid)
                end_files = _ram_file_count(ram_dir) if ram_dir else 0
                print(f"  handles: base={base_handles} end={end_handles}")
                print(f"  *.bin files in ram dir: base={base_files} "
                      f"end={end_files}")
                # A little slack: QEMU's own steady-state churn (timers,
                # short-lived event handles) is not zero-variance. What
                # matters is NOT climbing by one full HANDLE+FILE pair per
                # cycle, which is what an unmap/close leak looks like across
                # three cycles (>= 3 extra of each).
                if ram_dir and end_files - base_files >= 3:
                    print(f"  FAIL: {end_files - base_files} ram files "
                          f"leaked across 3 object-del cycles -- this is the "
                          f"VirtualFree-on-a-mapped-view leak")
                    ok = False
                if end_handles - base_handles >= 6:
                    print(f"  FAIL: {end_handles - base_handles} handles "
                          f"leaked across 3 cycles")
                    ok = False
                if ok:
                    print("  ok: no leak across 3 object-add/object-del cycles")
    finally:
        qmp.close()
        if _alive(proc):
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=10)
        time.sleep(1)
    return ok


def main(argv=None):
    # Win32 error text (GetLastError/WinError messages) is emitted in the
    # OS's configured language, not necessarily ASCII, and a console using a
    # legacy codepage (cp1252 etc.) can't encode it -- a crash report should
    # never itself crash the reporter. reconfigure() is a no-op-safe best
    # effort; anything that doesn't support it (a piped/redirected stream on
    # some platforms) just keeps its current encoding.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            try:
                stream.reconfigure(errors="backslashreplace")
            except (ValueError, OSError):
                pass

    argv = argv if argv is not None else sys.argv[1:]
    if len(argv) < 2:
        print(__doc__)
        return 2
    exe, ram_dir = argv[0], argv[1]
    biosdir = str(Path(exe).resolve().parent.parent / "pc-bios")
    if not Path(biosdir).is_dir():
        biosdir = str(Path(exe).resolve().parent / "pc-bios")

    results = []
    results.append(("baseline (env unset)",
                     run_one("baseline (env unset)", exe, biosdir, None)))
    results.append(("file-backed (QEMU_RAM_FILE_DIR set)",
                     run_one("file-backed (QEMU_RAM_FILE_DIR set)", exe,
                             biosdir, ram_dir)))

    print()
    print("=== summary ===")
    all_ok = True
    for name, ok in results:
        print(f"  {'PASS' if ok else 'FAIL'}  {name}")
        all_ok = all_ok and ok
    return 0 if all_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
