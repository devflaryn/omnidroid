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

def _app_root():
    """Project root — works both as a .py and as a PyInstaller onefile exe.
    When frozen, files live next to the exe (sys.executable), not in the
    temporary _MEIPASS extraction dir."""
    if getattr(sys, "frozen", False):
        return Path(sys.executable).resolve().parent
    return Path(__file__).resolve().parent.parent


REPO = _app_root()
CONFIG_PATH = REPO / "configs" / "paths.json"
ACCOUNTS_DIR = REPO / "accounts"
QEMU_DIR = REPO / "qemu"          # local (auto-installed) QEMU lives here
IS_WINDOWS = platform.system() == "Windows"
IS_LINUX = platform.system() == "Linux"

# Linux KSM (kernel samepage merging) sysfs interface. Dedups identical
# guest RAM pages across instances (same immutable base => big overlap).
KSM_DIR = Path("/sys/kernel/mm/ksm")
PAGE_SIZE = 4096

FIRST_BOOT_TIMEOUT = 1500   # first boot runs full dexopt; be patient
NORMAL_BOOT_TIMEOUT = 360

# Default portable QEMU installer (Windows). Overridable in config
# ("qemu": {"download_url": ...}). NSIS installer supports silent install
# to a directory via /S /D=<dir>, so no global install is needed.
DEFAULT_QEMU_URL = ("https://qemu.weilnetz.de/w64/"
                    "qemu-w64-setup-20240423.exe")


# ---------- config / account state ----------

def read_config():
    """Plain JSON read, no validation (safe to call before QEMU exists)."""
    return json.loads(CONFIG_PATH.read_text())


def resolve_images_dir(cfg):
    """images_dir may be a plain string (legacy) or a per-platform dict
    ({"windows": ..., "linux": ...}) so one checkout works on both hosts.
    ~ is expanded (Linux convention: ~/OmniImages)."""
    v = cfg["images_dir"]
    if isinstance(v, dict):
        key = "windows" if IS_WINDOWS else "linux"
        v = v.get(key) or v.get("default")
        if not v:
            sys.exit(f"error: configs/paths.json images_dir has no entry "
                     f"for platform '{key}'")
    return str(Path(v).expanduser())


def load_config():
    cfg = read_config()
    cfg["images_dir"] = resolve_images_dir(cfg)   # normalized for callers
    base = cfg["bases"][cfg["current_base"]]
    images = Path(cfg["images_dir"])
    for key in ("disk", "kernel", "initrd"):
        p = images / base[key]
        if not p.exists():
            sys.exit(f"error: base asset missing: {p}")
    return cfg


# ---------- qemu resolution + auto-install ----------

def qemu_bin(tool):
    """Resolve a QEMU executable path. Order: config 'qemu.dir' -> local
    QEMU_DIR (auto-installed) -> bare name (found on PATH). Lets the shipped
    exe use a bundled/downloaded QEMU without a global install."""
    exe = tool + (".exe" if IS_WINDOWS else "")
    try:
        qd = read_config().get("qemu", {}).get("dir")
    except Exception:
        qd = None
    for cand in ([Path(qd) / exe] if qd else []) + [QEMU_DIR / exe]:
        if cand.exists():
            return str(cand)
    return tool          # PATH


def _qemu_present():
    import shutil
    p = qemu_bin("qemu-system-x86_64")
    return Path(p).exists() or shutil.which(p) is not None


def ensure_qemu():
    """Install QEMU into QEMU_DIR on first use if it is not already
    resolvable (config dir, local dir, or PATH). Not bundled in the exe —
    downloaded on demand. No-op when QEMU is already available."""
    if _qemu_present():
        return
    if not IS_WINDOWS:
        # Linux policy: SYSTEM QEMU only (no portable download).
        sys.exit("QEMU not found. Install the system packages:\n"
                 "  sudo apt install qemu-system-x86 qemu-utils "
                 "android-tools-adb\nthen re-run (see: qemu-manager setup)")
    import urllib.request
    url = (read_config().get("qemu", {}).get("download_url")
           or DEFAULT_QEMU_URL)
    QEMU_DIR.mkdir(parents=True, exist_ok=True)
    installer = QEMU_DIR / "qemu-setup.exe"
    print(f"[qemu] not found; downloading portable QEMU from {url}")
    print(f"[qemu] (one-time, ~150 MB) -> {QEMU_DIR}")
    urllib.request.urlretrieve(url, installer)
    print("[qemu] installing silently (no global install)...")
    # NSIS silent install into QEMU_DIR; /D must be last and unquoted.
    r = subprocess.run(f'"{installer}" /S /D={QEMU_DIR}', shell=True)
    installer.unlink(missing_ok=True)
    if not _qemu_present():
        sys.exit(f"[qemu] auto-install failed (exit {r.returncode}). "
                 f"Install QEMU manually or set qemu.dir in "
                 f"configs/paths.json")
    print(f"[qemu] ready: {qemu_bin('qemu-system-x86_64')}")


def account_dir(name):
    return ACCOUNTS_DIR / name


def load_account(name):
    p = account_dir(name) / "account.json"
    if not p.exists():
        sys.exit(f"error: no such account '{name}' (looked for {p})")
    return ensure_vnc_port(json.loads(p.read_text()))


def save_account(acct):
    d = account_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    (d / "account.json").write_text(json.dumps(acct, indent=2))


def all_accounts():
    if not ACCOUNTS_DIR.exists():
        return []
    return sorted(
        (ensure_vnc_port(json.loads((d / "account.json").read_text()))
         for d in ACCOUNTS_DIR.iterdir()
         if (d / "account.json").exists()),
        key=lambda a: a["name"])


# Per-instance PORT SCHEME (documented invariant):
#   instance index i (0-based)  ->  adb = adb_port_start + i   (16001+)
#                                   qmp = qmp_port_start + i   (17001+)
#                                   vnc = vnc_port_start + i   (18001+)
# One shared index per account keeps the triple aligned; the three ranges
# are 1000 apart, so adb/qmp/vnc can NEVER collide below 1000 instances
# (and instance counts are host-RAM-bound long before that). vnc_port is
# RESERVED now (recorded in account.json) but not yet passed to QEMU —
# a local VNC server will bind it later.
VNC_PORT_START_DEFAULT = 18001


def vnc_start(cfg):
    return cfg["qemu"].get("vnc_port_start", VNC_PORT_START_DEFAULT)


def allocate_ports(cfg):
    q = cfg["qemu"]
    used = set()
    for a in all_accounts():
        used.add(a["adb_port"] - q["adb_port_start"])
        used.add(a["qmp_port"] - q["qmp_port_start"])
    i = 0
    while i in used:
        i += 1
    return (q["adb_port_start"] + i, q["qmp_port_start"] + i,
            vnc_start(cfg) + i)


def ensure_vnc_port(acct):
    """Backfill the reserved vnc_port on accounts created before the
    scheme existed (derived from the account's adb index, so the triple
    stays aligned)."""
    if "vnc_port" not in acct:
        cfg = read_config()
        idx = acct["adb_port"] - cfg["qemu"]["adb_port_start"]
        acct["vnc_port"] = vnc_start(cfg) + idx
        save_account(acct)
    return acct


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


# ---------- KSM (Linux kernel samepage merging) ----------

def ksm_available():
    return IS_LINUX and KSM_DIR.exists()


def ksm_stats():
    """Read all /sys/kernel/mm/ksm/* values (ints where possible).
    None when KSM is not available (non-Linux or kernel without KSM)."""
    if not ksm_available():
        return None
    out = {}
    for f in sorted(KSM_DIR.iterdir()):
        try:
            v = f.read_text().strip()
            out[f.name] = int(v) if v.lstrip("-").isdigit() else v
        except OSError:
            pass
    return out


def ksm_write(name, value):
    """Write one KSM sysfs knob; exits with sudo advice on EPERM."""
    try:
        (KSM_DIR / name).write_text(str(value))
    except PermissionError:
        sys.exit(f"error: no permission to write {KSM_DIR / name} - "
                 f"run with sudo (or install/enable ksmtuned)")


def ksm_saved_mb(stats):
    """Approx MB deduplicated: each page in pages_sharing points at a
    shared page instead of owning its own copy."""
    return stats.get("pages_sharing", 0) * PAGE_SIZE / (1024 * 1024)


def pid_ksm_merged_mb(pid):
    """Per-process KSM-merged pages (kernel >= 6.1 exposes
    ksm_merging_pages). None if unsupported."""
    try:
        n = int(Path(f"/proc/{pid}/ksm_merging_pages").read_text())
        return n * PAGE_SIZE / (1024 * 1024)
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

def default_accel():
    """Hypervisor auto-detect: WHPX on Windows, KVM elsewhere. Overridable
    per-start with --accel (e.g. 'tcg' for a no-hypervisor smoke test)."""
    return "whpx,kernel-irqchip=off" if IS_WINDOWS else "kvm"


def machine_arg(accel):
    """-machine string. On Linux/KVM add mem-merge=on explicitly: it marks
    guest RAM MADV_MERGEABLE so KSM can dedup identical pages across
    instances (it is the QEMU default, but distro builds vary — be
    explicit; it is what the whole Linux scaling story depends on)."""
    m = f"q35,accel={accel}"
    if IS_LINUX and accel.split(",")[0] == "kvm":
        m += ",mem-merge=on"
    return m


def check_accel():
    """Linux preflight: warn loudly if /dev/kvm is unusable (QEMU would
    fail or crawl under TCG). Windows/WHPX has no equivalent check."""
    if not IS_LINUX:
        return
    import os
    kvm = Path("/dev/kvm")
    if not kvm.exists():
        print("[accel] WARNING: /dev/kvm missing - KVM unavailable. "
              "Enable VT-x/AMD-V in BIOS and install qemu-system-x86; "
              "check with 'kvm-ok' (apt install cpu-checker).")
    elif not os.access(kvm, os.R_OK | os.W_OK):
        print("[accel] WARNING: no permission on /dev/kvm - add your user "
              "to the kvm group: sudo usermod -aG kvm $USER (re-login).")


# Per-instance performance modes. Counts are NEVER capped — these tune the
# per-instance footprint; the host's free RAM decides how many run.
# ALL instances are HEADLESS (-display none), always: no host window
# exists anywhere. View/control happens via adb (screenshot/logcat) today;
# a local VNC server will be wired to each instance's RESERVED vnc_port
# later (see the port scheme note at allocate_ports). With no window the
# old VirGL path (needed a host GL window) and the R/B software-blit swap
# are both moot — guest-side rendering is unchanged and screencap is
# always true-color.
MODES = {
    "playable": {"mem": 4096, "smp": 4},
    "hard":     {"mem": 3072, "smp": 4},
    "brutal":   {"mem": 2048, "smp": 2},
}
DEFAULT_MODE = "playable"


def resolve_mode(cfg, name=None, mem=None):
    m = dict(MODES[name or DEFAULT_MODE])
    m["name"] = name or DEFAULT_MODE
    if mem:
        m["mem"] = mem
    return m


def qemu_command(acct, cfg, dev, mode=None, accel=None):
    base = cfg["bases"][acct["base"]]
    images = Path(cfg["images_dir"])
    q = cfg["qemu"]
    d = account_dir(acct["name"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)

    append = ("stack_depot_disable=on cgroup_disable=pressure "
              "root=/dev/ram0 noexec=off "
              f"SRC={base['src']} DATA=vdb")
    smp = q["smp"]
    mem = q["mem_mb"]

    if dev:
        # Dev/builder boot: serial console log for debugging (headless like
        # everything else; virtio-vga kept so the guest has its usual DRM
        # device during provisioning/builder sessions).
        append += " console=tty0 console=ttyS0,115200"
        gpu = ["-device", "virtio-vga"]
        nic = "virtio-net-pci,netdev=net0"
    else:
        # Production silent boot (no firmware/console text).
        append += (" quiet loglevel=0 console=null "
                   "vt.global_cursor_default=0 SETUPWIZARD=0")
        nic = "virtio-net-pci,netdev=net0,romfile="   # no iPXE option ROM
        smp = mode["smp"]
        mem = mode["mem"]
        gpu = ["-vga", "none", "-device", "virtio-gpu-pci"]

    cmd = [
        qemu_bin("qemu-system-x86_64"),
        "-machine", machine_arg(accel),
        "-cpu", "qemu64",
        "-smp", str(smp),
        "-m", str(mem),
        "-drive", f"file={d / 'system.qcow2'},format=qcow2,if=virtio",
        "-drive", f"file={d / 'data.qcow2'},format=qcow2,if=virtio",
        *gpu,
        "-display", "none",       # headless ALWAYS; VNC will attach later
        "-device", "qemu-xhci",
        "-device", "usb-kbd",
        "-device", "usb-tablet",
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", nic,
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-kernel", str(images / base["kernel"]),
        "-initrd", str(images / base["initrd"]),
        "-append", append,
        "-name", f"omni-{acct['name']}",
    ]
    if dev:
        cmd += ["-serial", f"file:{d / 'serial.log'}"]
    return cmd


def spawn_qemu(acct, cfg, dev, mode=None, accel=None):
    check_accel()
    d = account_dir(acct["name"])
    log = open(d / "qemu.log", "w")
    kwargs = {}
    if IS_WINDOWS:
        DETACHED = 0x00000008          # DETACHED_PROCESS
        NEW_GROUP = 0x00000200         # CREATE_NEW_PROCESS_GROUP
        kwargs["creationflags"] = DETACHED | NEW_GROUP
    else:
        kwargs["start_new_session"] = True
    proc = subprocess.Popen(
        qemu_command(acct, cfg, dev, mode, accel=accel),
        stdout=log, stderr=log, **kwargs)
    (d / "run.json").write_text(json.dumps(
        {"pid": proc.pid, "started": time.time(),
         "mode": (mode or {}).get("name", "dev" if dev else DEFAULT_MODE)}))
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
        # Launch the kiosk once now so it sets the solid-black wallpaper
        # into /data before the first production boot (no wallpaper flash).
        adb(acct, "shell", "am", "start", "-n",
            "com.omni.kiosk/.MainActivity", timeout=15)
        time.sleep(3)
        print(f"[{label}] kiosk set as HOME, Bliss launchers disabled, "
              f"black wallpaper applied")
    # Which game the kiosk should launch: an adb-installed one (dev) takes
    # precedence, else the base's pre-installed system-app game (production).
    game = acct.get("game_package")
    if not game:
        try:
            game = read_config().get("base_game", {}).get(acct["base"])
        except Exception:
            game = None
    if game:
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", game, timeout=10)
        print(f"[{label}] kiosk game package = {game}")

    lockdown_and_trim(acct, label)

    print(f"[{label}] provisioned /data settings (lock screen off, "
          f"immersive confirmed, setup complete)")


# Packages a single-game kiosk never needs. Disabling frees RAM and trims
# boot (all `pm disable-user` = per-/data, reversible, no /system touched).
# GMS + Play Store are intentionally KEPT (the game may use Play Integrity).
TRIM_PACKAGES = (
    "com.google.android.setupwizard",         # setup wizard + its notification
    "com.google.android.googlequicksearchbox",  # Assistant/search (~215 MB)
    "com.google.android.apps.restore",        # device restore
    "org.blissroms.aboutbliss",               # Bliss about app
    "net.sourceforge.opencamera",             # preinstalled camera
    "com.termux",                             # preinstalled terminal
    "com.amaze.filemanager",                  # preinstalled file manager
    # Tier-1 trims (2026-07-06, measured -119 MB guest-used w/ Roblox).
    # Persistent/running services a single-game kiosk never needs:
    "org.omnirom.omnijaws",                   # weather service (was running)
    "org.lineageos.updater",                  # OTA updater (persistent)
    "com.android.touch.gestures",             # Bliss gestures (persistent;
                                              # Lock Task blocks them anyway)
    # Boot-spawned apps that idle in cached state (page-touch avoidance):
    "com.farmerbb.taskbar",                   # taskbar main pkg
    "io.chaldeaprjkt.gamespace",              # game overlay
    "player.phonograph.plus",                 # music player
    "com.android.deskclock",
    "com.android.dialer",                     # dialer UI (telephony svc kept)
    "com.android.contacts",
    "com.android.messaging",
    "com.google.android.projection.gearhead",  # Android Auto
    "com.google.android.gm.exchange",
    "com.google.android.syncadapters.calendar",
    "com.android.printspooler",
    "com.android.imsserviceentitlement",
    "com.android.cellbroadcastreceiver.module",
    # Deliberately KEPT: GMS/Play Store (Play Integrity), latin IME (login
    # typing), Settings (FallbackHome), managedprovisioning (device owner),
    # networkstack / com.android.phone / media provider (stability).
)


def lockdown_and_trim(acct, label):
    """Kiosk lockdown (device-owner Lock Task) + boot/RAM trims. All
    per-/data: sets the kiosk as device owner so it can fully disable the
    status bar / Quick-Settings pull-down / nav gestures, kills the setup
    wizard, disables unneeded apps, and zeroes animations."""
    # If an older kiosk was ever adb-installed into /data (e.g. via
    # 'kioskify'), that copy shadows the base's /system kiosk and may lack
    # the device-admin receiver -> "Unknown admin". Revert to the system
    # kiosk first so device owner can be set.
    adb(acct, "shell", "cmd", "package", "uninstall-system-updates",
        "com.omni.kiosk", timeout=30)
    # Device owner: enables the kiosk's Lock Task Mode. Works only on a
    # device with no added accounts (kiosk accounts have none). Idempotent-
    # ish: ignore "already set" failures.
    r = adb(acct, "shell", "dpm", "set-device-owner",
            "com.omni.kiosk/.OmniDeviceAdminReceiver", timeout=20)
    out = (r.stdout + r.stderr)
    if "Success" in out:
        print(f"[{label}] kiosk is DEVICE OWNER (Lock Task lockdown active)")
    elif "already" in out.lower() or "not allowed" in out.lower():
        print(f"[{label}] device owner already set / present")
    else:
        print(f"[{label}] NOTE: could not set device owner: "
              f"{out.strip()[:120]}")
    # Trim unneeded packages (RAM + boot).
    for pkg in TRIM_PACKAGES:
        try:
            adb(acct, "shell", "pm", "disable-user", "--user", "0", pkg,
                timeout=15)
        except Exception:
            pass
    # Zero UI animations (snappier, tiny boot win).
    for k in ("window_animation_scale", "transition_animation_scale",
              "animator_duration_scale"):
        adb(acct, "shell", "settings", "put", "global", k, "0", timeout=10)
    print(f"[{label}] trimmed {len(TRIM_PACKAGES)} unneeded packages, "
          f"animations off")


# ---------- commands ----------

def make_overlay(system_path, base_disk):
    """(Re)create a cheap qcow2 system overlay backed by base_disk. The
    overlay only absorbs disposable system-partition COW writes, so it is
    safe to delete and recreate against a different base (account data
    lives on the separate data.qcow2, never in this overlay)."""
    Path(system_path).unlink(missing_ok=True)
    subprocess.run([qemu_bin("qemu-img"), "create", "-f", "qcow2",
                    "-b", str(base_disk), "-F", "qcow2",
                    str(system_path)], check=True, capture_output=True)


def cmd_create(args):
    ensure_qemu()
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
    adb_port, qmp_port, vnc_port = allocate_ports(cfg)
    acct = {"name": name, "base": cfg["current_base"],
            "adb_port": adb_port, "qmp_port": qmp_port,
            "vnc_port": vnc_port,        # reserved for future local VNC
            "first_boot_done": False, "created": time.time()}
    save_account(acct)

    make_overlay(d / "system.qcow2", base_disk)
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
    ensure_qemu()
    return _cmd_start(args)


def _cmd_start(args):
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
    dev = args.dev or first          # first boot always uses the dev profile
    mode = resolve_mode(cfg, args.mode, mem=args.mem)
    accel = getattr(args, "accel", None)
    pid = spawn_qemu(acct, cfg, dev=dev, mode=None if dev else mode,
                     accel=accel)
    modestr = "dev" if dev else mode["name"]
    print(f"[start {args.name}] detached: qemu pid {pid}, mode {modestr} "
          f"(headless), adb 127.0.0.1:{acct['adb_port']}, "
          f"qmp 127.0.0.1:{acct['qmp_port']}, "
          f"vnc-reserved {acct['vnc_port']}")
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


# ---------- base migration (update accounts to a newer base) ----------

def migrate_account(name, cfg, target=None, reprovision=True):
    """Repoint an account's disposable system overlay onto `target` base
    (default: current). The account's data.qcow2 is NEVER touched, so all
    logins/settings/installed apps survive. Re-provisioning (idempotent)
    applies the new base's kiosk/HOME/settings. This is how a base update
    (e.g. an updated pre-installed game) reaches every account without
    erasing per-account data."""
    cfg = cfg or load_config()
    target = target or cfg["current_base"]
    acct = load_account(name)
    label = f"update {name}"
    if acct["base"] == target:
        print(f"[{label}] already on base {target}; re-provisioning")
    if running_pid(name):
        _shutdown(acct, label)
    d = account_dir(name)
    base = cfg["bases"][target]
    base_disk = Path(cfg["images_dir"]) / base["disk"]
    old = acct["base"]
    make_overlay(d / "system.qcow2", base_disk)      # data.qcow2 untouched
    acct["base"] = target
    save_account(acct)
    print(f"[{label}] overlay {old} -> {target}; data.qcow2 preserved")
    if not reprovision:
        return
    # Boot once (dev) and re-apply settings so the new base's kiosk/system
    # game/HOME take effect on the existing data disk.
    spawn_qemu(acct, cfg, dev=True)
    first = not acct.get("first_boot_done")
    if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT if first
                         else NORMAL_BOOT_TIMEOUT, label, first_boot=first):
        print(f"[{label}] WARNING: boot timed out; leaving instance for "
              f"inspection")
        return
    post_boot(acct, label)
    provision_settings(acct, label)
    acct["first_boot_done"] = True
    save_account(acct)
    _shutdown(acct, label)
    print(f"[{label}] migrated to {target} and re-provisioned")


def account_needs_full_update(acct, cfg, target):
    """FAST vs FULL decision for one account.

    FAST (overlay repoint, no boot) is correct whenever the account's
    provisioned /data state stays valid on the new base. Everything that
    lives in /system — the OS, a baked game APK update (same package), a
    new kiosk APK — arrives via the overlay itself. The ONE thing the
    repoint can't deliver is a /data change, and the only /data value
    derived from the base is the kiosk's target game package
    (omni_game_package). So:
      - account has a dev-installed game  -> base game irrelevant -> FAST
      - base game package unchanged       -> FAST
      - base game package differs         -> FULL (boot + re-provision)
    Policy/settings changes (lockdown, trims) are invisible here — force
    them with update-all --full.
    """
    if acct.get("game_package"):
        return False
    games = cfg.get("base_game", {})
    return games.get(acct["base"]) != games.get(target)


def migrate_account_fast(name, cfg, target):
    """FAST path: discard the disposable system overlay, create a fresh
    one backed by the NEW base (a metadata-only qemu-img create — the
    clean way to change backing files; never rebase, never edit a base in
    place). data.qcow2 untouched; no boot; seconds per account."""
    acct = load_account(name)
    label = f"update {name}"
    if running_pid(name):
        _shutdown(acct, label)
    d = account_dir(name)
    base_disk = Path(cfg["images_dir"]) / cfg["bases"][target]["disk"]
    old = acct["base"]
    make_overlay(d / "system.qcow2", base_disk)      # data.qcow2 untouched
    acct["base"] = target
    save_account(acct)
    print(f"[{label}] FAST: overlay {old} -> {target} "
          f"(no boot; data.qcow2 preserved)")


def cmd_update_base(args):
    ensure_qemu()
    cfg = load_config()
    migrate_account(args.name, cfg, target=args.to,
                    reprovision=not args.no_reprovision)


def cmd_update_all(args):
    """Migrate ALL accounts to a base. Default AUTO: per account, take the
    near-instant overlay-repoint FAST path unless the base's game package
    changed for that account (then boot + re-provision). --fast / --full
    force one path for every account. Scales to 100+ accounts: a pure
    system/game base swap is seconds total, not hours."""
    if args.fast and args.full:
        sys.exit("error: --fast and --full are mutually exclusive")
    ensure_qemu()
    cfg = load_config()
    target = args.to or cfg["current_base"]
    if target not in cfg["bases"]:
        sys.exit(f"error: no base '{target}'. Known: {list(cfg['bases'])}")
    names = [a["name"] for a in all_accounts()]
    todo = [n for n in names
            if load_account(n)["base"] != target or not args.skip_current]
    print(f"[update-all] target base {target}; "
          f"{len(todo)}/{len(names)} account(s) to migrate: {todo}")
    t0 = time.time()
    slow = []
    for n in todo:
        if args.full:
            full = True
        elif args.fast:
            full = False
        else:
            full = account_needs_full_update(load_account(n), cfg, target)
        if full:
            slow.append(n)
            migrate_account(n, cfg, target=target,
                            reprovision=not args.no_reprovision)
        else:
            migrate_account_fast(n, cfg, target)
    print(f"[update-all] done in {time.time() - t0:.1f}s. "
          f"{len(todo) - len(slow)} fast / {len(slow)} full; all on "
          f"{target}; per-account data preserved."
          + (f" Full (booted): {slow}" if slow else ""))


# ---------- production base rebuild (update pre-installed game) ----------

def _next_base_tag(cfg):
    nums = [int(k[1:]) for k in cfg["bases"] if re.fullmatch(r"v\d+", k)]
    return f"v{max(nums) + 1}"


# APK lib/<abi> dir -> Android system-app nativeLibraryDir name.
_ABI_TO_SYSLIB = {"arm64-v8a": "arm64", "armeabi-v7a": "arm",
                  "armeabi": "arm", "x86_64": "x86_64", "x86": "x86"}


def _bake_native_libs(acct, game_apk, appdir, label):
    """Extract native .so files from the APK and place them under
    <appdir>/lib/<abi> so a /system/app game can load them (required for
    ARM games, which libndk then translates)."""
    import zipfile
    import tempfile
    pushed = 0
    with zipfile.ZipFile(game_apk) as z:
        by_abi = {}
        for name in z.namelist():
            parts = name.split("/")
            if (len(parts) == 3 and parts[0] == "lib"
                    and name.endswith(".so")):
                by_abi.setdefault(parts[1], []).append(name)
        tmp = Path(tempfile.mkdtemp(prefix="omnilib_"))
        try:
            for abi, names in by_abi.items():
                syslib = _ABI_TO_SYSLIB.get(abi, abi)
                local = tmp / abi
                local.mkdir(parents=True, exist_ok=True)
                for name in names:
                    (local / Path(name).name).write_bytes(z.read(name))
                dst = f"{appdir}/lib/{syslib}"
                adb(acct, "shell", f"mkdir -p {dst}", timeout=15)
                adb(acct, "push", str(local) + "/.", dst + "/", timeout=300)
                adb(acct, "shell",
                    f"chmod 755 {dst}/*.so; "
                    f"chcon u:object_r:system_file:s0 {dst} {dst}/*.so",
                    timeout=30)
                pushed += len(names)
                print(f"[{label}]   libs {abi} -> lib/{syslib} "
                      f"({len(names)} .so)")
        finally:
            import shutil
            shutil.rmtree(tmp, ignore_errors=True)
    return pushed


def _build_next_base(cfg, mutate, notes, base_game=None):
    """Boot a throwaway builder on the current base, let `mutate(acct,label)`
    modify /system (remount rw already up to the caller), flatten to the
    next base version, register it, and make it current. Returns the tag."""
    import shutil
    cur = cfg["current_base"]
    nxt = _next_base_tag(cfg)
    label = f"build {cur}->{nxt}"
    images = Path(cfg["images_dir"])
    cur_disk = images / cfg["bases"][cur]["disk"]

    bname = "_builder"
    d = account_dir(bname)
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    adb_port, qmp_port, vnc_port = allocate_ports(cfg)
    acct = {"name": bname, "base": cur, "adb_port": adb_port,
            "qmp_port": qmp_port, "vnc_port": vnc_port,
            "first_boot_done": True}
    save_account(acct)
    make_overlay(d / "system.qcow2", cur_disk)
    shutil.copyfile(images / cfg["data_template"], d / "data.qcow2")

    try:
        print(f"[{label}] booting builder on {cur}")
        spawn_qemu(acct, cfg, dev=True)
        if not wait_for_boot(acct, FIRST_BOOT_TIMEOUT, label):
            sys.exit(f"[{label}] builder boot failed")
        adb(acct, "root")
        time.sleep(3)
        adb_connect(acct)
        adb(acct, "shell", "mount -o remount,rw /", timeout=20)
        mutate(acct, label)
        adb(acct, "shell", "sync", timeout=15)
        _shutdown(acct, label)

        newdisk = images / f"base-{nxt}.qcow2"
        print(f"[{label}] flattening overlay -> {newdisk} (self-contained)")
        subprocess.run([qemu_bin("qemu-img"), "convert", "-O", "qcow2",
                        "-c", str(d / "system.qcow2"), str(newdisk)],
                       check=True)
        shutil.copyfile(images / cfg["bases"][cur]["kernel"],
                        images / f"base-{nxt}.kernel")
        shutil.copyfile(images / cfg["bases"][cur]["initrd"],
                        images / f"base-{nxt}.initrd.img")

        raw = read_config()
        raw["bases"][nxt] = {
            "disk": f"base-{nxt}.qcow2",
            "kernel": f"base-{nxt}.kernel",
            "initrd": f"base-{nxt}.initrd.img",
            "src": cfg["bases"][cur]["src"],
            "notes": notes}
        # Carry forward the prior base's pre-installed game, unless changed.
        prior_game = raw.get("base_game", {}).get(cur)
        if base_game:
            raw.setdefault("base_game", {})[nxt] = base_game
        elif prior_game:
            raw.setdefault("base_game", {})[nxt] = prior_game
        raw["current_base"] = nxt
        CONFIG_PATH.write_text(json.dumps(raw, indent=2))
        print(f"[{label}] base {nxt} built and current. "
              f"Roll out: omni update-all")
    finally:
        if d.exists():
            shutil.rmtree(d, ignore_errors=True)
    return nxt


def rebuild_base(cfg, game_apk):
    """Bake/replace the pre-installed game APK as a /system/app system app in
    a NEW base version. update-all rolls it out, keeping each data.qcow2."""
    pkg = apk_package_name(game_apk)
    if not pkg:
        sys.exit(f"could not read package name from {game_apk}")

    def mutate(acct, label):
        appdir = "/system/app/OmniGame"
        adb(acct, "push", game_apk, "/data/local/tmp/game.apk", timeout=600)
        r = adb(acct, "shell",
                f"rm -rf {appdir} && mkdir -p {appdir} && "
                f"cp /data/local/tmp/game.apk {appdir}/OmniGame.apk && "
                f"chmod 644 {appdir}/OmniGame.apk && "
                f"chcon u:object_r:system_file:s0 {appdir} && "
                f"chcon u:object_r:system_file:s0 {appdir}/OmniGame.apk && "
                f"rm /data/local/tmp/game.apk && echo BAKED", timeout=120)
        if "BAKED" not in r.stdout:
            sys.exit(f"[{label}] baking failed: {r.stdout}{r.stderr}")
        n = _bake_native_libs(acct, game_apk, appdir, label)
        print(f"[{label}] baked {pkg} as system app ({n} native libs)")

    return _build_next_base(cfg, mutate,
                            notes=f"pre-installed game {pkg} (system app)",
                            base_game=pkg)


def update_kiosk_base(cfg, kiosk_apk):
    """Replace the kiosk system app (/system/app/OmniKiosk) in a NEW base
    version — e.g. to ship a new launcher with Lock Task lockdown."""
    if not Path(kiosk_apk).exists():
        sys.exit(f"kiosk apk not found: {kiosk_apk}")

    def mutate(acct, label):
        appdir = "/system/app/OmniKiosk"
        adb(acct, "push", kiosk_apk, "/data/local/tmp/kiosk.apk",
            timeout=120)
        r = adb(acct, "shell",
                f"rm -rf {appdir} && mkdir -p {appdir} && "
                f"cp /data/local/tmp/kiosk.apk {appdir}/OmniKiosk.apk && "
                f"chmod 644 {appdir}/OmniKiosk.apk && "
                f"chcon u:object_r:system_file:s0 {appdir} && "
                f"chcon u:object_r:system_file:s0 {appdir}/OmniKiosk.apk && "
                f"rm /data/local/tmp/kiosk.apk && echo KIOSK_OK", timeout=60)
        if "KIOSK_OK" not in r.stdout:
            sys.exit(f"[{label}] kiosk swap failed: {r.stdout}{r.stderr}")
        print(f"[{label}] replaced kiosk system app")

    return _build_next_base(cfg, mutate,
                            notes="updated kiosk (Lock Task lockdown + "
                                  "boot/RAM trims)")


def cmd_rebuild_base(args):
    ensure_qemu()
    cfg = load_config()
    rebuild_base(cfg, args.game)


def cmd_update_kiosk(args):
    ensure_qemu()
    cfg = load_config()
    update_kiosk_base(cfg, args.apk)


def cmd_use_base(args):
    """Set the default base for NEW accounts (mode switch: e.g. a dev base
    without the game vs a production base with the game pre-installed).
    Does not touch existing accounts (use update-all for that)."""
    raw = read_config()
    if args.tag not in raw["bases"]:
        sys.exit(f"error: no base '{args.tag}'. Known: "
                 f"{list(raw['bases'])}")
    raw["current_base"] = args.tag
    CONFIG_PATH.write_text(json.dumps(raw, indent=2))
    print(f"current base = {args.tag} "
          f"({raw['bases'][args.tag].get('notes','')})")


def cmd_bases(args):
    raw = read_config()
    cur = raw["current_base"]
    for tag, b in raw["bases"].items():
        game = raw.get("base_game", {}).get(tag)
        mark = " *" if tag == cur else "  "
        print(f"{mark}{tag}: {b.get('notes','')}"
              + (f"  [game: {game}]" if game else ""))
    print(f"\ncurrent (default for new accounts): {cur}")


def cmd_qemu_info(args):
    import shutil
    if args.install:
        ensure_qemu()
    resolved = qemu_bin("qemu-system-x86_64")
    where = resolved if Path(resolved).exists() else shutil.which(resolved)
    print(json.dumps({
        "qemu_system": resolved,
        "resolved_to": where,
        "present": _qemu_present(),
        "local_qemu_dir": str(QEMU_DIR),
        "qemu_img": qemu_bin("qemu-img"),
    }, indent=2))


def cmd_setup(args):
    """First-run setup. Idempotent; also runs implicitly on first use.

    Windows: fully self-contained/portable — creates the tool's folders
    and downloads a PORTABLE QEMU into ./qemu ONLY. Never installs
    anything to the host system (no global install, no registry, no PATH).
    Linux: creates folders/config; uses SYSTEM QEMU (never a portable
    download) — preflights qemu/adb//dev/kvm/KSM and prints the exact
    install command for anything missing.
    """
    if not CONFIG_PATH.exists():
        # Blank deployment (e.g. qemu-manager.exe dropped into a new
        # folder): bootstrap a default config. Base images arrive
        # out-of-band today (later from the update server) and are then
        # registered under "bases".
        CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
        CONFIG_PATH.write_text(json.dumps({
            "images_dir": {"windows": "C:/Users/berat/OmniImages",
                           "linux": "~/OmniImages"},
            "current_base": None,
            "data_template": "data-template-8g.qcow2",
            "bases": {},
            "qemu": {"mem_mb": 4096, "smp": 4, "data_disk_size": "8G",
                     "adb_port_start": 16001, "qmp_port_start": 17001,
                     "vnc_port_start": 18001},
        }, indent=2))
        print(f"[setup] created default config: {CONFIG_PATH}")
    cfg = read_config()
    images = Path(resolve_images_dir(cfg))
    report = {"platform": "windows" if IS_WINDOWS else "linux",
              "images_dir": str(images), "ok": True}
    for d in (images, ACCOUNTS_DIR):
        d.mkdir(parents=True, exist_ok=True)
    if IS_WINDOWS:
        ensure_qemu()                       # portable download into ./qemu
        report["qemu"] = qemu_bin("qemu-system-x86_64")
        report["qemu_portable_dir"] = str(QEMU_DIR)
    else:
        import shutil as _sh
        missing = [t for t in ("qemu-system-x86_64", "qemu-img", "adb")
                   if not _sh.which(t)]
        if missing:
            report["ok"] = False
            report["missing"] = missing
            report["install"] = ("sudo apt install qemu-system-x86 "
                                 "qemu-utils android-tools-adb")
        else:
            report["qemu"] = _sh.which("qemu-system-x86_64")
        import os
        kvm = Path("/dev/kvm")
        report["kvm"] = (kvm.exists()
                         and os.access(kvm, os.R_OK | os.W_OK))
        if not report["kvm"]:
            report["ok"] = False
            report["kvm_fix"] = ("enable VT-x/AMD-V in BIOS; sudo usermod "
                                 "-aG kvm $USER; re-login; check kvm-ok")
        report["ksm"] = ksm_available()
        if report["ksm"] and ksm_stats().get("run") != 1:
            report["ksm_hint"] = "enable page dedup: qemu-manager ksm on"
    # Base assets present? (they arrive out-of-band today; later from the
    # update server — see HANDOFF 'server base updates').
    base = cfg["bases"].get(cfg.get("current_base"), {})
    have = all((images / base.get(k, "_")).exists()
               for k in ("disk", "kernel", "initrd")) if base else False
    report["base_assets"] = have
    if not have:
        report["ok"] = False
        report["base_hint"] = (f"copy base files + "
                               f"{cfg.get('data_template','data template')} "
                               f"into {images}")
    print(json.dumps(report, indent=2))
    if not report["ok"]:
        sys.exit(1)


def cmd_ksm(args):
    """Inspect/control Linux KSM. On Windows: documented no-op (KSM is a
    Linux kernel feature; WHPX shares nothing between VMs)."""
    if IS_WINDOWS:
        print("ksm: no-op on Windows. KSM (kernel samepage merging) is a "
              "Linux kernel feature - on the Linux/KVM host it dedups "
              "identical guest pages across instances. Nothing to do here.")
        return
    if not ksm_available():
        sys.exit("error: /sys/kernel/mm/ksm not present - kernel built "
                 "without KSM")
    if args.action == "on":
        if args.aggressive:
            ksm_write("pages_to_scan", 1000)
            ksm_write("sleep_millisecs", 20)
        ksm_write("run", 1)
        print("[ksm] scanning enabled"
              + (" (aggressive)" if args.aggressive else ""))
    elif args.action == "off":
        # run=0 stops scanning but keeps already-merged pages shared;
        # run=2 would force-unmerge everything (not offered: it spikes RAM).
        ksm_write("run", 0)
        print("[ksm] scanning stopped (existing merges kept)")
    st = ksm_stats()
    st["_deduped_mb"] = round(ksm_saved_mb(st), 1)
    print(json.dumps(st, indent=2))


def _wait_game_running(acct, pkg, timeout=180):
    """Poll until the game process exists in the guest (kiosk launches it
    on boot). Process presence, not foreground - same rule as the watchdog."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if adb(acct, "shell", "pidof", pkg, timeout=8).stdout.strip():
                return True
        except Exception:
            pass
        time.sleep(3)
    return False


def _ksm_wait_settle(settle_secs, timeout=600):
    """Block until KSM pages_sharing stops moving (<1% drift held for
    settle_secs). Returns the settled pages_sharing value."""
    last = None
    stable_since = None
    start = time.time()
    while time.time() - start < timeout:
        cur = ksm_stats().get("pages_sharing", 0)
        if last is not None and abs(cur - last) <= max(last, 100) * 0.01:
            stable_since = stable_since or time.time()
            if time.time() - stable_since >= settle_secs:
                return cur
        else:
            stable_since = None
        last = cur
        time.sleep(10)
    return last or 0


def cmd_bench_ksm(args):
    """Measure REAL instances-per-GB with KSM on a Linux/KVM host.

    SCAFFOLD - written on Windows 2026-07-06, first executed when the
    Ubuntu laptop exists; treat the first Linux run as its test.

    Method (honest marginal cost, not RSS - RSS double-counts pages KSM
    shares): start identical instances one at a time (default brutal/
    headless), each to boot_completed + game process up; after each, wait
    for pages_sharing to plateau, then record the marginal drop in host
    MemAvailable. Stops when MemAvailable < --floor-mb: the RAM floor ends
    the bench, never a count cap. One JSON line per step + summary table.
    """
    if IS_WINDOWS:
        sys.exit("bench-ksm needs a Linux host with KVM+KSM (Phase 8 "
                 "measurement tool; Windows/WHPX shares nothing).")
    ensure_qemu()
    check_accel()
    if not ksm_available():
        sys.exit("error: /sys/kernel/mm/ksm not present on this kernel")
    cfg = load_config()
    if ksm_stats().get("run") != 1:
        print("[bench] enabling KSM (aggressive scan for the bench)")
        ksm_write("pages_to_scan", 1000)
        ksm_write("sleep_millisecs", 20)
        ksm_write("run", 1)

    base_game = read_config().get("base_game", {}).get(cfg["current_base"])
    if not base_game and not args.apk:
        sys.exit("error: current base has no pre-installed game; pass "
                 "--apk <game.apk> so instances run the real workload")

    base_avail = host_mem_available_mb()
    print(f"[bench] baseline: MemAvailable {base_avail:.0f} MB, "
          f"pages_sharing {ksm_stats().get('pages_sharing', 0)}, "
          f"mode {args.mode}, floor {args.floor_mb} MB")
    rows = []
    prev_avail = base_avail
    for i in range(1, args.max + 1):
        name = f"{args.prefix}{i}"
        if not (account_dir(name) / "account.json").exists():
            import argparse as _a
            print(f"[bench] creating {name} (one-time first-boot dexopt - "
                  f"slow now, fast on reruns)")
            cmd_create(_a.Namespace(
                name=name, no_provision=False,
                data_size=cfg["qemu"]["data_disk_size"]))
        acct = load_account(name)
        pkg = acct.get("game_package") or base_game
        if not running_pid(name):
            mode = resolve_mode(cfg, args.mode)
            spawn_qemu(acct, cfg, dev=False, mode=mode)
        if not wait_for_boot(acct, NORMAL_BOOT_TIMEOUT, f"bench {name}"):
            print(f"[bench] {name} boot timeout - stopping bench")
            break
        post_boot(acct, f"bench {name}")
        if args.apk and not acct.get("game_package"):
            r = adb(acct, "install", "-r", "-g", "--no-incremental",
                    args.apk, timeout=600)
            if "Success" in (r.stdout + r.stderr):
                pkg = apk_package_name(args.apk) or pkg
                if pkg:
                    acct["game_package"] = pkg
                    save_account(acct)
                    adb(acct, "shell", "settings", "put", "global",
                        "omni_game_package", pkg, timeout=10)
        if pkg and not _wait_game_running(acct, pkg):
            print(f"[bench] WARNING: {pkg} never came up in {name}; "
                  f"numbers for this step measure an idle instance")
        sharing = _ksm_wait_settle(args.settle_secs)
        avail = host_mem_available_mb()
        st = ksm_stats()
        pids = [running_pid(f"{args.prefix}{k}") for k in range(1, i + 1)]
        rss = sum(r for r in (host_rss_mb(p) for p in pids if p) if r)
        row = {"n": i, "name": name,
               "mem_available_mb": round(avail),
               "marginal_mb": round(prev_avail - avail),
               "sum_qemu_rss_mb": round(rss),
               "ksm_pages_sharing": sharing,
               "ksm_deduped_mb": round(ksm_saved_mb(st), 1)}
        if isinstance(st.get("general_profit"), int):
            row["ksm_general_profit_mb"] = round(
                st["general_profit"] / (1024 * 1024), 1)
        rows.append(row)
        print(json.dumps(row))
        prev_avail = avail
        if avail < args.floor_mb:
            print(f"[bench] MemAvailable {avail:.0f} < floor "
                  f"{args.floor_mb} MB - stopping (RAM floor, not a cap)")
            break
    print("\n[bench]  n  marginal_MB  avail_MB  ksm_deduped_MB")
    for r in rows:
        print(f"[bench] {r['n']:>2}  {r['marginal_mb']:>11}  "
              f"{r['mem_available_mb']:>8}  {r['ksm_deduped_mb']:>14}")
    if rows and not args.keep:
        print("[bench] stopping bench instances (--keep leaves them up)")
        for r in rows:
            _shutdown(load_account(r["name"]), f"bench {r['name']}")


def cmd_list(args):
    accts = all_accounts()
    if not accts:
        print("no accounts. create one: qemu-manager create <name>")
        return
    for a in accts:
        pid = running_pid(a["name"])
        state = f"RUNNING pid {pid}" if pid else "stopped"
        line = (f"{a['name']:<16} base {a['base']}  adb {a['adb_port']}  "
                f"qmp {a['qmp_port']}  vnc {a.get('vnc_port', '?')}  "
                f"{state}")
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
            if IS_LINUX:
                merged = pid_ksm_merged_mb(pid)
                if merged is not None:
                    line += f"  ksm-merged {merged:.0f} MB"
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


# ---------- dev / testing harness (scriptable, JSON output) ----------

def _foreground(acct):
    try:
        r = adb(acct, "shell", "dumpsys", "activity", "activities",
                timeout=10)
        m = re.search(r"topResumedActivity=ActivityRecord\{\S+ \S+ (\S+)",
                      r.stdout)
        return m.group(1) if m else None
    except Exception:
        return None


def cmd_screenshot(args):
    """Pull a screenshot from the guest framebuffer (true colors, works
    headless). Prints JSON: {ok, path}."""
    acct = load_account(args.name)
    out = args.out or str(account_dir(args.name)
                          / f"shot-{int(time.time())}.png")
    try:
        adb(acct, "shell", "screencap", "-p", "/data/local/tmp/_s.png",
            timeout=30)
        adb(acct, "pull", "/data/local/tmp/_s.png", out, timeout=60)
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        sys.exit(1)
    ok = Path(out).exists() and Path(out).stat().st_size > 0
    print(json.dumps({"ok": ok, "path": out if ok else None}))
    if not ok:
        sys.exit(1)


def cmd_logcat(args):
    """Read guest logcat (raw, machine-parseable). --clear wipes it;
    --tag filters to a tag; otherwise dumps and returns."""
    acct = load_account(args.name)
    if args.clear:
        adb(acct, "logcat", "-c", timeout=15)
        print(json.dumps({"ok": True, "cleared": True}))
        return
    argv = ["logcat", "-d"]
    if args.tag:
        argv += ["-s", args.tag]
    r = adb(acct, *argv, timeout=args.timeout)
    sys.stdout.write(r.stdout)


def cmd_test_apk(args):
    """One-shot dev harness: ensure a FRESH session with NO app pre-baked
    (v3 dev base, kiosk), install the given APK, let the kiosk launch it,
    and report machine-readable JSON. Headless by default. After this,
    drive with: omni screenshot / logcat / adb.

    Emits a single JSON line: {account, adb_port, qmp_port, package,
    installed, launched, foreground, pid, mode}."""
    ensure_qemu()
    cfg = load_config()
    name = args.name
    result = {"account": name}
    fresh = not (account_dir(name) / "account.json").exists()
    if fresh and not args.reuse:
        # Create a clean dev account on the current (dev) base.
        import argparse as _a
        ca = _a.Namespace(name=name, no_provision=False,
                          data_size=cfg["qemu"]["data_disk_size"])
        cmd_create(ca)
    acct = load_account(name)
    result["base"] = acct["base"]
    if not running_pid(name):
        mode = resolve_mode(cfg, args.mode)
        spawn_qemu(acct, cfg, dev=False, mode=mode)
        if not wait_for_boot(acct, NORMAL_BOOT_TIMEOUT, f"test {name}"):
            print(json.dumps({**result, "ok": False,
                              "error": "boot timeout"}))
            sys.exit(1)
        post_boot(acct, f"test {name}")
        result["mode"] = mode["name"]
    pkg = apk_package_name(args.apk)
    result["package"] = pkg
    adb(acct, "logcat", "-c", timeout=15)
    r = adb(acct, "install", "-r", "-g", "--no-incremental", args.apk,
            timeout=600)
    result["installed"] = "Success" in (r.stdout + r.stderr)
    if pkg:
        acct["game_package"] = pkg
        save_account(acct)
        adb(acct, "shell", "settings", "put", "global",
            "omni_game_package", pkg, timeout=10)
        # kiosk auto-launches on PACKAGE_ADDED; also nudge explicitly.
        adb(acct, "shell", "monkey", "-p", pkg, "-c",
            "android.intent.category.LAUNCHER", "1", timeout=30)
    deadline = time.time() + 30
    while time.time() < deadline:
        fg = _foreground(acct)
        if fg and pkg and fg.startswith(pkg):
            break
        time.sleep(3)
    result["foreground"] = _foreground(acct)
    try:
        result["pid"] = (adb(acct, "shell", "pidof", pkg, timeout=8)
                         .stdout.strip() or None) if pkg else None
    except Exception:
        result["pid"] = None
    result["launched"] = bool(result["pid"])
    result["adb_port"] = acct["adb_port"]
    result["qmp_port"] = acct["qmp_port"]
    result["adb_serial"] = f"127.0.0.1:{acct['adb_port']}"
    result["ok"] = result["installed"] and result["launched"]
    print(json.dumps(result))


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
    s.add_argument("--mode", choices=list(MODES), default=None,
                   help="RAM/CPU tier (all headless): playable 4G/4c | "
                        "hard 3G/4c | brutal 2G/2c. Default: playable")
    s.add_argument("--mem", type=int, default=None,
                   help="override guest RAM in MB")
    s.add_argument("--accel", default=None,
                   help="override hypervisor (auto: Windows=whpx, "
                        "Linux=kvm). E.g. 'tcg' for a no-hypervisor test")
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

    ub = sub.add_parser("update-base",
                        help="migrate one account to a base (default: "
                             "current), keeping its data")
    ub.add_argument("name")
    ub.add_argument("--to", default=None, help="target base tag (e.g. v3)")
    ub.add_argument("--no-reprovision", action="store_true")
    ub.set_defaults(func=cmd_update_base)

    ua = sub.add_parser("update-all",
                        help="migrate ALL accounts to a base (default: "
                             "current); per-account data preserved. AUTO "
                             "picks the near-instant no-boot fast path "
                             "when the base game is unchanged")
    ua.add_argument("--to", default=None)
    ua.add_argument("--fast", action="store_true",
                    help="force overlay-repoint only (no boot) for every "
                         "account, even if the base game changed")
    ua.add_argument("--full", action="store_true",
                    help="force boot + re-provision for every account "
                         "(needed for /data policy/settings changes, e.g. "
                         "lockdown or trim updates)")
    ua.add_argument("--no-reprovision", action="store_true")
    ua.add_argument("--skip-current", action="store_true",
                    help="skip accounts already on the target base")
    ua.set_defaults(func=cmd_update_all)

    rb = sub.add_parser("rebuild-base",
                        help="bake/replace the pre-installed game in a new "
                             "base version (production); then update-all")
    rb.add_argument("--game", required=True, help="path to the game APK")
    rb.set_defaults(func=cmd_rebuild_base)

    uk = sub.add_parser("update-kiosk",
                        help="ship a new kiosk launcher in a new base "
                             "version; then update-all")
    uk.add_argument("--apk", default=str(REPO / "launcher" / "build"
                                         / "omni-kiosk.apk"))
    uk.set_defaults(func=cmd_update_kiosk)

    su = sub.add_parser("setup",
                        help="first-run setup: folders + QEMU (Windows: "
                             "portable download, self-contained; Linux: "
                             "system QEMU preflight). Idempotent")
    su.set_defaults(func=cmd_setup)

    qi = sub.add_parser("qemu-info",
                        help="show resolved QEMU path / install if missing")
    qi.add_argument("--install", action="store_true")
    qi.set_defaults(func=cmd_qemu_info)

    km = sub.add_parser("ksm",
                        help="Linux KSM status/on/off (clean no-op on "
                             "Windows)")
    km.add_argument("action", nargs="?", default="status",
                    choices=["status", "on", "off"])
    km.add_argument("--aggressive", action="store_true",
                    help="with 'on': pages_to_scan=1000, sleep=20ms "
                         "(faster dedup, more ksmd CPU)")
    km.set_defaults(func=cmd_ksm)

    bk = sub.add_parser("bench-ksm",
                        help="Linux-only: measure real instances-per-GB "
                             "with KSM (Phase 8; scaffold until the Linux "
                             "host exists)")
    bk.add_argument("--mode", choices=list(MODES), default="brutal")
    bk.add_argument("--max", type=int, default=99,
                    help="safety bound on bench steps (the RAM floor is "
                         "what actually stops the bench)")
    bk.add_argument("--floor-mb", type=int, default=2048, dest="floor_mb",
                    help="stop when host MemAvailable drops below this")
    bk.add_argument("--settle-secs", type=int, default=60,
                    dest="settle_secs",
                    help="KSM pages_sharing must be stable this long "
                         "before measuring")
    bk.add_argument("--apk", default=None,
                    help="game APK to install per instance (needed on a "
                         "dev base with no pre-installed game)")
    bk.add_argument("--prefix", default="bench",
                    help="bench account name prefix")
    bk.add_argument("--keep", action="store_true",
                    help="leave bench instances running afterwards")
    bk.set_defaults(func=cmd_bench_ksm)

    bs = sub.add_parser("bases", help="list registered bases + current")
    bs.set_defaults(func=cmd_bases)

    ubz = sub.add_parser("use-base",
                         help="set default base for new accounts (dev vs "
                              "production mode switch)")
    ubz.add_argument("tag")
    ubz.set_defaults(func=cmd_use_base)

    sc = sub.add_parser("screenshot", help="pull a screenshot (JSON out)")
    sc.add_argument("name")
    sc.add_argument("--out", default=None)
    sc.set_defaults(func=cmd_screenshot)

    lc = sub.add_parser("logcat", help="read/clear guest logcat")
    lc.add_argument("name")
    lc.add_argument("--tag", default=None, help="filter to a log tag")
    lc.add_argument("--clear", action="store_true")
    lc.add_argument("--timeout", type=int, default=30)
    lc.set_defaults(func=cmd_logcat)

    ta = sub.add_parser("test-apk",
                        help="dev harness: fresh session, install+launch an "
                             "APK, report JSON (headless, scriptable)")
    ta.add_argument("name")
    ta.add_argument("--apk", required=True)
    ta.add_argument("--mode", choices=list(MODES), default="hard")
    ta.add_argument("--reuse", action="store_true",
                    help="reuse the account if it already exists")
    ta.set_defaults(func=cmd_test_apk)

    args = p.parse_args()
    if getattr(args, "data_size", None) is None and args.cmd == "create":
        args.data_size = load_config()["qemu"]["data_disk_size"]
    args.func(args)


if __name__ == "__main__":
    main()
