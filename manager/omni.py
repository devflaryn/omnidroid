#!/usr/bin/env python3
"""omni — multi-account Bliss OS instance manager.

Each account = a cheap qcow2 overlay on the shared immutable base (system)
plus an independent data disk (Android /data). Instances boot via direct
kernel boot (-kernel/-initrd/-append): no GRUB, per-account kernel params.

Usage:
  python omni.py create <name> [--no-provision] [--data-size 8G]
  python omni.py start  <name> [--dev] [--timeout SECS]
  python omni.py stop   <name>
  python omni.py list   [--stats]
  python omni.py install <name> <apk> [package]
  python omni.py run-app <name> <package>
  python omni.py adb    <name> -- <adb args...>
"""
import argparse
import json
import platform
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CONFIG_PATH = REPO / "configs" / "paths.json"
ACCOUNTS_DIR = REPO / "accounts"
IS_WINDOWS = platform.system() == "Windows"

FIRST_BOOT_TIMEOUT = 1500   # first boot runs full dexopt; be patient
NORMAL_BOOT_TIMEOUT = 360


# ---------- config / account state ----------

def load_config():
    cfg = json.loads(CONFIG_PATH.read_text())
    base = cfg["bases"][cfg["current_base"]]
    images = Path(cfg["images_dir"])
    for key in ("disk", "kernel", "initrd"):
        p = images / base[key]
        if not p.exists():
            sys.exit(f"error: base asset missing: {p}")
    return cfg


def account_dir(name):
    return ACCOUNTS_DIR / name


def load_account(name):
    p = account_dir(name) / "account.json"
    if not p.exists():
        sys.exit(f"error: no such account '{name}' (looked for {p})")
    return json.loads(p.read_text())


def save_account(acct):
    d = account_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    (d / "account.json").write_text(json.dumps(acct, indent=2))


def all_accounts():
    if not ACCOUNTS_DIR.exists():
        return []
    return sorted(
        (json.loads((d / "account.json").read_text())
         for d in ACCOUNTS_DIR.iterdir()
         if (d / "account.json").exists()),
        key=lambda a: a["name"])


def allocate_ports(cfg):
    used_adb = {a["adb_port"] for a in all_accounts()}
    used_qmp = {a["qmp_port"] for a in all_accounts()}
    adb_port = cfg["qemu"]["adb_port_start"]
    while adb_port in used_adb:
        adb_port += 1
    qmp_port = cfg["qemu"]["qmp_port_start"]
    while qmp_port in used_qmp:
        qmp_port += 1
    return adb_port, qmp_port


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
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False


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


# ---------- adb / qmp ----------

def adb(acct, *args, timeout=20, check=False):
    serial = f"127.0.0.1:{acct['adb_port']}"
    cmd = ["adb", "-s", serial] + list(args)
    return subprocess.run(cmd, capture_output=True, text=True,
                          timeout=timeout, check=check)


def adb_connect(acct):
    try:
        subprocess.run(["adb", "connect", f"127.0.0.1:{acct['adb_port']}"],
                       capture_output=True, text=True, timeout=15)
    except subprocess.TimeoutExpired:
        pass


def adb_getprop(acct, prop):
    try:
        r = adb(acct, "shell", "getprop", prop, timeout=8)
        return r.stdout.strip()
    except Exception:
        return ""


def qmp(acct, execute, arguments=None, timeout=6):
    """Send one QMP command; returns parsed response line or None."""
    try:
        with socket.create_connection(("127.0.0.1", acct["qmp_port"]),
                                      timeout=timeout) as s:
            s.settimeout(timeout)
            f = s.makefile("rw", encoding="utf-8", newline="\n")
            f.readline()                                   # greeting
            f.write('{"execute":"qmp_capabilities"}\n'); f.flush()
            f.readline()
            msg = {"execute": execute}
            if arguments:
                msg["arguments"] = arguments
            f.write(json.dumps(msg) + "\n"); f.flush()
            return json.loads(f.readline())
    except Exception:
        return None


# ---------- qemu ----------

def qemu_command(acct, cfg, dev):
    base = cfg["bases"][acct["base"]]
    images = Path(cfg["images_dir"])
    q = cfg["qemu"]
    d = account_dir(acct["name"])
    accel = "whpx,kernel-irqchip=off" if IS_WINDOWS else "kvm"

    append = ("stack_depot_disable=on cgroup_disable=pressure "
              "root=/dev/ram0 noexec=off "
              f"SRC={base['src']} DATA=vdb")
    if dev:
        append += " console=tty0 console=ttyS0,115200"
    else:
        # Silent boot: no kernel log spam, no blinking cursor, skip setup
        # wizard. GRUB is already absent (direct kernel boot).
        append += " quiet loglevel=0 vt.global_cursor_default=0 SETUPWIZARD=0"

    cmd = [
        "qemu-system-x86_64",
        "-machine", f"q35,accel={accel}",
        "-cpu", "qemu64",
        "-smp", str(q["smp"]),
        "-m", str(q["mem_mb"]),
        "-drive", f"file={d / 'system.qcow2'},format=qcow2,if=virtio",
        "-drive", f"file={d / 'data.qcow2'},format=qcow2,if=virtio",
        "-device", "virtio-vga",
        "-display", "sdl",
        "-device", "qemu-xhci",
        "-device", "usb-kbd",
        "-device", "usb-tablet",
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", "virtio-net-pci,netdev=net0",
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-kernel", str(images / base["kernel"]),
        "-initrd", str(images / base["initrd"]),
        "-append", append,
        "-name", f"omni-{acct['name']}",
    ]
    if dev:
        cmd += ["-serial", f"file:{d / 'serial.log'}"]
    return cmd


def spawn_qemu(acct, cfg, dev):
    d = account_dir(acct["name"])
    log = open(d / "qemu.log", "w")
    kwargs = {}
    if IS_WINDOWS:
        DETACHED = 0x00000008          # DETACHED_PROCESS
        NEW_GROUP = 0x00000200         # CREATE_NEW_PROCESS_GROUP
        kwargs["creationflags"] = DETACHED | NEW_GROUP
    else:
        kwargs["start_new_session"] = True
    proc = subprocess.Popen(qemu_command(acct, cfg, dev),
                            stdout=log, stderr=log, **kwargs)
    (d / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time()}))
    return proc.pid


def running_pid(name):
    p = account_dir(name) / "run.json"
    if not p.exists():
        return None
    pid = json.loads(p.read_text()).get("pid")
    return pid if pid_alive(pid) else None


# ---------- boot waiting with visible progress ----------

def wait_for_boot(acct, timeout, label, first_boot=False):
    """Poll until sys.boot_completed=1, printing honest progress lines."""
    d = account_dir(acct["name"])
    serial_log = d / "serial.log"
    start = time.time()
    phase = "starting QEMU"
    last_print = 0.0
    last_change = time.time()
    adbd_seen = False
    initrd_found = False
    while time.time() - start < timeout:
        elapsed = time.time() - start

        if not initrd_found and serial_log.exists():
            try:
                if "Found at" in serial_log.read_text(errors="ignore"):
                    initrd_found = True
            except OSError:
                pass
        adb_connect(acct)
        if adb_getprop(acct, "sys.boot_completed") == "1":
            print(f"[{label}] boot completed after {elapsed/60:.1f} min")
            return True
        try:
            if adb(acct, "get-state").stdout.strip() == "device":
                adbd_seen = True
        except subprocess.TimeoutExpired:
            pass

        if adbd_seen and first_boot:
            new_phase = ("Android first boot: app optimization (dexopt), "
                         "one-time, ~15 min")
        elif adbd_seen:
            new_phase = "Android booting (adbd up)"
        elif initrd_found:
            new_phase = "initrd found OS; Android starting (adbd not up yet)"
        else:
            new_phase = "starting QEMU"

        if new_phase != phase:
            last_change = time.time()
        stalled = time.time() - last_change > 600 and not adbd_seen
        if stalled:
            new_phase += ("  [WARNING: no progress signal for 10+ min - "
                          f"check accounts/{acct['name']}/serial.log "
                          "and qemu.log]")
        if new_phase != phase or elapsed - last_print >= 15:
            phase = new_phase
            last_print = elapsed
            print(f"[{label}] {elapsed/60:.1f} min - {phase}", flush=True)
        time.sleep(5)
    print(f"[{label}] TIMED OUT after {timeout/60:.0f} min")
    return False


def post_boot(acct, label):
    """adb root (KernelSU image allows it) and basic sanity props."""
    adb(acct, "root")
    time.sleep(3)
    adb_connect(acct)
    bridge = adb_getprop(acct, "ro.dalvik.vm.native.bridge")
    ok = bridge == "libndk_translation.so"
    print(f"[{label}] native bridge: {bridge or '<unreadable>'}"
          f" {'OK' if ok else '*** ARM TRANSLATION CHECK FAILED ***'}")
    return ok


def provision_settings(acct, label):
    """One-time per-account /data settings: kill the lock screen and mark
    setup complete so boot goes straight to HOME. Idempotent."""
    adb(acct, "root")
    time.sleep(2)
    adb_connect(acct)
    for args in (
        ("shell", "locksettings", "set-disabled", "true"),
        ("shell", "settings", "put", "secure", "lockscreen.disabled", "1"),
        ("shell", "settings", "put", "global", "device_provisioned", "1"),
        ("shell", "settings", "put", "secure", "user_setup_complete", "1"),
        # Suppress the "Viewing full screen / swipe down to exit" immersive
        # confirmation so it never appears when the kiosk/game hides bars.
        ("shell", "settings", "put", "secure",
         "immersive_mode_confirmations", "confirmed"),
    ):
        try:
            adb(acct, *args, timeout=10)
        except Exception:
            pass
    # If the kiosk is present (system app in base-v2), make it the only
    # HOME: set default launcher + disable Bliss launchers/taskbar. All
    # per-/data and reversible; skipped cleanly on bases without it.
    r = adb(acct, "shell", "pm", "path", "com.omni.kiosk", timeout=10)
    if "package:" in (r.stdout or ""):
        adb(acct, "shell", "cmd", "package", "set-home-activity",
            "--user", "0", "com.omni.kiosk/.MainActivity", timeout=15)
        for pkg in BLISS_HOME_PACKAGES:
            adb(acct, "shell", "pm", "disable-user", "--user", "0", pkg,
                timeout=15)
        print(f"[{label}] kiosk set as HOME, Bliss launchers disabled")
    if acct.get("game_package"):
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", acct["game_package"], timeout=10)

    print(f"[{label}] provisioned /data settings (lock screen off, "
          f"immersive confirmed, setup complete)")


# ---------- commands ----------

def cmd_create(args):
    cfg = load_config()
    name = args.name
    if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
        sys.exit("error: account name must be [A-Za-z0-9_-]+")
    d = account_dir(name)
    if (d / "account.json").exists():
        sys.exit(f"error: account '{name}' already exists")
    d.mkdir(parents=True, exist_ok=True)

    base = cfg["bases"][cfg["current_base"]]
    base_disk = Path(cfg["images_dir"]) / base["disk"]
    adb_port, qmp_port = allocate_ports(cfg)
    acct = {"name": name, "base": cfg["current_base"],
            "adb_port": adb_port, "qmp_port": qmp_port,
            "first_boot_done": False, "created": time.time()}
    save_account(acct)

    subprocess.run(["qemu-img", "create", "-f", "qcow2",
                    "-b", str(base_disk), "-F", "qcow2",
                    str(d / "system.qcow2")], check=True,
                   capture_output=True)
    # /data disk must be a pre-formatted ext4 filesystem: the Bliss initrd
    # only mounts DATA= devices, it never formats them (a blank disk hangs
    # Android before adbd). Copy the formatted-empty template.
    import shutil
    template = Path(cfg["images_dir"]) / cfg["data_template"]
    if not template.exists():
        sys.exit(f"error: data template missing: {template}")
    if args.data_size != cfg["qemu"]["data_disk_size"]:
        print(f"[create {name}] note: --data-size ignored for now; "
              f"template is {cfg['qemu']['data_disk_size']}")
    shutil.copyfile(template, d / "data.qcow2")
    print(f"[create {name}] disks ready "
          f"(overlay on {base['disk']}, data {args.data_size}); "
          f"adb port {adb_port}, qmp port {qmp_port}")

    if args.no_provision:
        print(f"[create {name}] skipping provisioning; first 'start' "
              f"will run the one-time first boot (~15 min)")
        return

    print(f"[create {name}] provisioning: first boot runs Android's "
          f"one-time app optimization (dexopt). Expect ~15 minutes; "
          f"progress below.", flush=True)
    spawn_qemu(acct, cfg, dev=True)
    if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, f"create {name}",
                         first_boot=True):
        sys.exit(f"[create {name}] provisioning failed (timeout)")
    post_boot(acct, f"create {name}")
    provision_settings(acct, f"create {name}")
    acct["first_boot_done"] = True
    save_account(acct)
    _shutdown(acct, f"create {name}")
    print(f"[create {name}] provisioned and shut down. "
          f"Subsequent boots take ~2-4 min.")


def cmd_start(args):
    """Spawn a detached QEMU instance and return immediately.

    The VM is never tied to this process: PID + ports are recorded in
    accounts/<name>/run.json, lifecycle is managed via PID/adb/QMP.
    Use --wait (or 'omni resume <name>') to block until boot completes.
    """
    cfg = load_config()
    acct = load_account(args.name)
    if running_pid(args.name):
        sys.exit(f"error: '{args.name}' is already running")
    first = not acct.get("first_boot_done")
    pid = spawn_qemu(acct, cfg, dev=args.dev or first)
    print(f"[start {args.name}] detached: qemu pid {pid}, "
          f"adb 127.0.0.1:{acct['adb_port']}, "
          f"qmp 127.0.0.1:{acct['qmp_port']}")
    if first:
        print(f"[start {args.name}] first boot of this account: one-time "
              f"dexopt, ~15 min. Track progress: omni resume {args.name}",
              flush=True)
    if not args.wait:
        return
    timeout = args.timeout or (FIRST_BOOT_TIMEOUT if first
                               else NORMAL_BOOT_TIMEOUT)
    if not wait_for_boot(acct, timeout, f"start {args.name}",
                         first_boot=first):
        sys.exit(1)
    post_boot(acct, f"start {args.name}")
    if first:
        provision_settings(acct, f"start {args.name}")
        acct["first_boot_done"] = True
        save_account(acct)


def _shutdown(acct, label):
    """Graceful in-guest shutdown, then QMP quit fallback. Host-side
    fallback is mandatory: never rely on the guest self-killing."""
    name = acct["name"]
    pid = running_pid(name)
    if not pid:
        print(f"[{label}] not running")
        return
    try:
        adb(acct, "shell", "svc", "power", "shutdown", timeout=10)
        print(f"[{label}] sent in-guest shutdown, waiting for QEMU exit...")
    except Exception:
        print(f"[{label}] adb unreachable, using QMP fallback")
    deadline = time.time() + 90
    while time.time() < deadline:
        if not pid_alive(pid):
            print(f"[{label}] instance is down (clean)")
            return
        time.sleep(3)
    print(f"[{label}] guest did not power off in 90s - QMP quit")
    qmp(acct, "quit")
    time.sleep(5)
    if pid_alive(pid):
        print(f"[{label}] still alive — killing pid {pid}")
        if IS_WINDOWS:
            subprocess.run(["taskkill", "/PID", str(pid), "/F"],
                           capture_output=True)
        else:
            import os
            import signal
            os.kill(pid, signal.SIGKILL)


def cmd_resume(args):
    """Re-attach to an already-running instance: wait for boot, run
    post-boot checks, mark first boot done. Leaves the instance running."""
    acct = load_account(args.name)
    if not running_pid(args.name):
        sys.exit(f"error: '{args.name}' is not running")
    first = not acct.get("first_boot_done")
    timeout = FIRST_BOOT_TIMEOUT if first else NORMAL_BOOT_TIMEOUT
    if not wait_for_boot(acct, timeout, f"resume {args.name}",
                         first_boot=first):
        sys.exit(1)
    post_boot(acct, f"resume {args.name}")
    if first:
        provision_settings(acct, f"resume {args.name}")
        acct["first_boot_done"] = True
        save_account(acct)


def cmd_stop(args):
    acct = load_account(args.name)
    _shutdown(acct, f"stop {args.name}")


def cmd_list(args):
    accts = all_accounts()
    if not accts:
        print("no accounts. create one: python omni.py create <name>")
        return
    for a in accts:
        pid = running_pid(a["name"])
        state = f"RUNNING pid {pid}" if pid else "stopped"
        line = (f"{a['name']:<16} base {a['base']}  adb {a['adb_port']}  "
                f"qmp {a['qmp_port']}  {state}")
        if pid and args.stats:
            rss = host_rss_mb(pid)
            guest = ""
            try:
                mem = adb(a, "shell", "head", "-3", "/proc/meminfo",
                          timeout=8).stdout
                tot = int(re.search(r"MemTotal:\s+(\d+)", mem).group(1))
                avail = int(re.search(r"MemAvailable:\s+(\d+)", mem).group(1))
                guest = f"  guest-used {(tot - avail) / 1024:.0f} MB"
            except Exception:
                pass
            line += (f"  host-rss {rss:.0f} MB" if rss else "") + guest
        print(line)


def cmd_install(args):
    acct = load_account(args.name)
    print(f"[install {args.name}] installing {args.apk} ...")
    r = adb(acct, "install", "-r", "-g", "--no-incremental", args.apk,
            timeout=600)
    out = (r.stdout + r.stderr).strip()
    print(f"[install {args.name}] {out}")
    if "Success" not in out:
        sys.exit(1)
    pkg = apk_package_name(args.apk)
    if pkg:
        acct["game_package"] = pkg
        save_account(acct)
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", pkg, timeout=10)
        print(f"[install {args.name}] game package = {pkg} "
              f"(saved + pushed to guest)")


def apk_package_name(apk):
    """Read the package name from an APK via build-tools aapt2/aapt."""
    import glob
    sdk = Path.home() / "AppData/Local/Android/Sdk/build-tools"
    for bt in sorted(glob.glob(str(sdk / "*")), reverse=True):
        for tool, argv in (("aapt2.exe", ["dump", "packagename", apk]),
                           ("aapt.exe", ["dump", "badging", apk])):
            exe = Path(bt) / tool
            if not exe.exists():
                continue
            try:
                r = subprocess.run([str(exe)] + argv, capture_output=True,
                                   text=True, timeout=30)
                if tool == "aapt2.exe" and r.returncode == 0:
                    return r.stdout.strip().splitlines()[0]
                m = re.search(r"package: name='([^']+)'", r.stdout)
                if m:
                    return m.group(1)
            except Exception:
                pass
    return None


# Bliss packages that claim HOME; disabled per-account so the kiosk is
# the only launcher. Settings' FallbackHome must NOT be disabled.
BLISS_HOME_PACKAGES = ("com.android.launcher3",
                       "com.farmerbb.taskbar.support",
                       "cu.axel.smartdock")


def cmd_kioskify(args):
    """Install the kiosk APK on a running instance, make it the HOME app,
    and disable Bliss launchers/taskbar (all per-/data, reversible)."""
    acct = load_account(args.name)
    label = f"kioskify {args.name}"
    adb(acct, "root")
    time.sleep(2)
    adb_connect(acct)
    r = adb(acct, "install", "-r", "-g", "--no-incremental", args.apk,
            timeout=120)
    out = (r.stdout + r.stderr).strip()
    print(f"[{label}] install kiosk: {out}")
    if "Success" not in out:
        sys.exit(1)
    adb(acct, "shell", "cmd", "package", "set-home-activity",
        "--user", "0", "com.omni.kiosk/.MainActivity", timeout=15)
    for pkg in BLISS_HOME_PACKAGES:
        adb(acct, "shell", "pm", "disable-user", "--user", "0", pkg,
            timeout=15)
    if acct.get("game_package"):
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", acct["game_package"], timeout=10)
    r = adb(acct, "shell", "cmd", "shortcut", "get-default-launcher",
            timeout=10)
    print(f"[{label}] default launcher now: {r.stdout.strip()}")


WATCH_POLL_SECS = 3


def cmd_watch(args):
    """Host-side shutdown watchdog. THE decider for 'game closed'.

    States:
      WAITING  - game process has never been seen yet (kiosk may still
                 be launching it). No timeout here by default.
      RUNNING  - game process exists. Blips (ads, dialogs, webviews,
                 focus loss, loading screens) keep the process alive,
                 so they never leave this state.
      GRACE    - process is GONE. Confirm it stays gone for --grace
                 seconds of consecutive polls; any reappearance (quick
                 relaunch, in-place restart) returns to RUNNING.
      -> after grace expires: power the instance off (in-guest adb
         shutdown first, QMP quit fallback -> _shutdown chain).

    'Not foreground' is deliberately NOT a shutdown signal - only
    process death is. adb hiccups count as 'unknown' and never advance
    the grace timer; if the QEMU process itself dies we just exit.
    """
    try:
        sys.stdout.reconfigure(line_buffering=True)   # visible in logs
    except Exception:
        pass
    acct = load_account(args.name)
    pkg = args.package or acct.get("game_package")
    if not pkg:
        sys.exit("error: no game package known; pass --package or "
                 "run 'omni install' first")
    grace = args.grace
    label = f"watch {args.name}"
    print(f"[{label}] pkg={pkg} grace={grace}s poll={WATCH_POLL_SECS}s")

    state = "WAITING"
    gone_since = None
    while True:
        if not running_pid(args.name):
            print(f"[{label}] QEMU process is gone - exiting watchdog")
            return
        pid_out = None
        try:
            r = adb(acct, "shell", "pidof", pkg, timeout=8)
            pid_out = r.stdout.strip()
        except Exception:
            pid_out = None          # adb hiccup -> unknown

        if pid_out is None:
            # Unknown: never advance grace on missing information.
            print(f"[{label}] adb unreachable (state={state}) - holding")
        elif pid_out:
            if state != "RUNNING":
                print(f"[{label}] game process up (pid {pid_out}) "
                      f"[{state} -> RUNNING]")
            state = "RUNNING"
            gone_since = None
        else:
            if state == "RUNNING":
                state = "GRACE"
                gone_since = time.time()
                print(f"[{label}] game process GONE - grace "
                      f"{grace}s starts [RUNNING -> GRACE]")
            elif state == "GRACE":
                waited = time.time() - gone_since
                if waited >= grace:
                    print(f"[{label}] gone for {waited:.0f}s >= "
                          f"{grace}s - shutting instance down")
                    _shutdown(acct, label)
                    return
                print(f"[{label}] still gone ({waited:.0f}/{grace}s)")
            # WAITING: game never started yet; keep waiting.
        time.sleep(WATCH_POLL_SECS)


def cmd_run_app(args):
    acct = load_account(args.name)
    r = adb(acct, "shell", "monkey", "-p", args.package,
            "-c", "android.intent.category.LAUNCHER", "1", timeout=30)
    print((r.stdout + r.stderr).strip())


def cmd_adb(args):
    acct = load_account(args.name)
    r = adb(acct, *args.rest, timeout=120)
    sys.stdout.write(r.stdout)
    sys.stderr.write(r.stderr)
    sys.exit(r.returncode)


def main():
    p = argparse.ArgumentParser(prog="omni")
    sub = p.add_subparsers(dest="cmd", required=True)

    c = sub.add_parser("create")
    c.add_argument("name")
    c.add_argument("--no-provision", action="store_true")
    c.add_argument("--data-size", default=None)
    c.set_defaults(func=cmd_create)

    s = sub.add_parser("start")
    s.add_argument("name")
    s.add_argument("--dev", action="store_true")
    s.add_argument("--wait", action="store_true",
                   help="block until boot completes (default: detach)")
    s.add_argument("--timeout", type=int, default=None)
    s.set_defaults(func=cmd_start)

    rs = sub.add_parser("resume")
    rs.add_argument("name")
    rs.set_defaults(func=cmd_resume)

    st = sub.add_parser("stop")
    st.add_argument("name")
    st.set_defaults(func=cmd_stop)

    l = sub.add_parser("list")
    l.add_argument("--stats", action="store_true")
    l.set_defaults(func=cmd_list)

    i = sub.add_parser("install")
    i.add_argument("name")
    i.add_argument("apk")
    i.set_defaults(func=cmd_install)

    w = sub.add_parser("watch")
    w.add_argument("name")
    w.add_argument("--package", default=None)
    w.add_argument("--grace", type=int, default=20,
                   help="seconds the game process must stay gone "
                        "before shutdown (default 20)")
    w.set_defaults(func=cmd_watch)

    k = sub.add_parser("kioskify")
    k.add_argument("name")
    k.add_argument("--apk", default=str(REPO / "launcher" / "build"
                                        / "omni-kiosk.apk"))
    k.set_defaults(func=cmd_kioskify)

    r = sub.add_parser("run-app")
    r.add_argument("name")
    r.add_argument("package")
    r.set_defaults(func=cmd_run_app)

    a = sub.add_parser("adb")
    a.add_argument("name")
    a.add_argument("rest", nargs=argparse.REMAINDER)
    a.set_defaults(func=cmd_adb)

    args = p.parse_args()
    if getattr(args, "data_size", None) is None and args.cmd == "create":
        args.data_size = load_config()["qemu"]["data_disk_size"]
    args.func(args)


if __name__ == "__main__":
    main()
