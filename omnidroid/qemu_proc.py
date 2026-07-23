# omnidroid/qemu_proc.py
"""QEMU command construction, process spawn, and QMP monitor access."""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

from omnidroid import config
from omnidroid.bases import (
    base_type, BASE_TYPE_ARM, acct_is_dev, arm_edk2_code, ARM_BASE_EFIVARS,
)
from omnidroid.config import IS_WINDOWS, IS_LINUX, IS_MACOS, qemu_bin


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
    """Hypervisor auto-detect: WHPX on Windows, HVF on macOS (Apple Silicon),
    KVM on Linux. Overridable per-start with --accel (e.g. 'tcg' for a
    no-hypervisor smoke test)."""
    if IS_WINDOWS:
        return "whpx,kernel-irqchip=off"
    if IS_MACOS:
        return "hvf"
    return "kvm"


def _gl_window_requested():
    """B2 spike apparatus: env OMNI_GL_WINDOW=1 asks a start to open a native
    GPU-accelerated window instead of the headless VNC path. Reversible and
    off by default — this is the experiment switch, replaced by the real
    capability-gated playable mode once the spike proves feasibility."""
    return os.environ.get("OMNI_GL_WINDOW", "").strip() not in ("", "0", "false", "False")


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
# exists anywhere. View/control happens via adb (screenshot/logcat) or an
# optional VNC viewer on the instance's vnc_port (127.0.0.1 only — see
# the port scheme note at allocate_ports). With no window the
# old VirGL path (needed a host GL window) and the R/B software-blit swap
# are both moot — guest-side rendering is unchanged and screencap is
# always true-color.
MODES = {
    "playable": {"mem": 4096, "smp": 4},
    "hard":     {"mem": 3072, "smp": 4},
    "brutal":   {"mem": 2048, "smp": 2},
    # farming: headless, joined-idle, squeezed as small as stable. mem is a
    # STARTING point the live measurement (Task 9) tunes; the runtime squeeze
    # (farming.py) does the rest after boot.
    "farming":  {"mem": 512, "smp": 2},
}
DEFAULT_MODE = "playable"


def resolve_mode(cfg, name=None, mem=None):
    m = dict(MODES[name or DEFAULT_MODE])
    m["name"] = name or DEFAULT_MODE
    if mem:
        m["mem"] = mem
    return m


def _assert_port_triple(acct):
    """The per-account port triple must be distinct (the shared-index scheme
    guarantees it below 1000 instances; assert anyway before handing the
    ports to QEMU). Returns the QEMU -vnc display number."""
    if len({acct["adb_port"], acct["qmp_port"], acct["vnc_port"]}) != 3:
        sys.exit(f"error: port collision for '{acct['name']}': "
                 f"adb {acct['adb_port']} qmp {acct['qmp_port']} "
                 f"vnc {acct['vnc_port']}")
    vnc_display = acct["vnc_port"] - 5900     # QEMU -vnc takes a display #
    if vnc_display < 0:
        sys.exit(f"error: vnc_port {acct['vnc_port']} is below QEMU's "
                 f"5900 display offset")
    return vnc_display


def qemu_command_arm(acct, cfg, dev, mode=None, accel=None):
    """arm-uefi (LineageOS arm64) QEMU command — native under HVF on Apple
    Silicon, NO translation layer. UEFI/GRUB disk boot: EDK2 pflash CODE +
    per-account writable efivars, GPT system disk (vda, the provisioned
    overlay carrying /metadata FBE keys) + /data (vdb). Same headless +
    localhost-VNC model as x86; base flags proven in tools/arm64/boot_arm64.sh.
    Silent boot is handled inside the guest image (GRUB/kernel), not via a
    Bliss-style -append, so there is no dev/prod append split here — the dev
    flag only adds a serial log."""
    from omnidroid.engine import runtime_dir, account_dir
    base = cfg["bases"][acct["base"]]
    q = cfg["qemu"]
    d = account_dir(acct["name"])          # overlay disks only (Task 4 removes these)
    rd = runtime_dir(acct["name"])         # per-boot files: efivars.fd, serial.log
    images = Path(cfg["images_dir"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)
    vnc_display = _assert_port_triple(acct)
    smp = q["smp"] if dev else mode["smp"]
    mem = q["mem_mb"] if dev else mode["mem"]
    # B2 spike apparatus (see _gl_window_requested): normally headless ALWAYS
    # (same rule as x86). Only when OMNI_GL_WINDOW is set, on macOS, and not a
    # dev boot, swap to a native GPU-accelerated cocoa window for the spike.
    gpu_display = (["-device", "virtio-gpu-gl", "-display", "cocoa,gl=on"]
                   if (_gl_window_requested() and IS_MACOS and not dev)
                   else ["-device", "virtio-gpu-pci", "-display", "none"])

    # EPHEMERAL (fully-shared, no-persistence) instances boot the SHARED provisioned
    # base templates DIRECTLY with snapshot=on: every write goes to a throwaway
    # per-process overlay that QEMU discards on exit, so nothing persists and many
    # instances of the same base run CONCURRENTLY (each opens the backing read-only).
    # The instance is then pure config (accounts.json cookie/alias) with NO
    # per-account system/data/devkit qcow2 files — only a fresh per-boot efivars.
    # Non-ephemeral accounts keep their per-account COW overlays (unchanged).
    ephemeral = bool(acct.get("ephemeral"))
    if ephemeral:
        sys_src = images / base["system"]
        data_src = images / base["data"]
        disk_opts = ",discard=unmap,detect-zeroes=unmap,snapshot=on"
        # Ephemeral efivars is refreshed fresh EVERY boot (see
        # _refresh_ephemeral_efivars, called from spawn_qemu before this
        # command is built) into runtime_dir — genuinely per-boot, throwaway.
        efivars_src = rd / "efivars.fd"
    else:
        sys_src = d / "system.qcow2"
        data_src = d / "data.qcow2"
        disk_opts = ",discard=unmap,detect-zeroes=unmap"
        # Non-ephemeral efivars is written ONCE at account creation (see
        # _make_persistent_arm_account, used only by base-build/maintenance
        # flows now) and persists across boots like the overlay disks —
        # stays under account_dir; this whole non-ephemeral path is
        # live-path-straggler territory (Task 5).
        efivars_src = d / "efivars.fd"

    code = arm_edk2_code()
    if not code:
        sys.exit("error: edk2-aarch64-code.fd (UEFI firmware) not found - "
                 "install qemu (brew install qemu) or set qemu.arm_edk2_code "
                 "in configs/paths.json")
    cmd = [
        qemu_bin("qemu-system-aarch64"),
        "-machine", "virt",
        "-accel", accel,          # hvf on Apple Silicon (no translation)
        "-cpu", "host",
        "-smp", str(smp),
        "-m", str(mem),
        # UEFI firmware: read-only CODE + per-account writable vars.
        "-drive", (f"if=pflash,unit=0,file={code},file.locking=off,"
                   "format=raw,readonly=on"),
        "-drive", f"if=pflash,unit=1,file={efivars_src}",
        # System overlay (vda, has /metadata FBE keys) + /data (vdb).
        "-device", "virtio-blk-pci,drive=vda,bootindex=0",
        "-device", "virtio-blk-pci,drive=vdb,bootindex=1",
        "-drive", f"file={sys_src},if=none,id=vda{disk_opts}",
        "-drive", f"file={data_src},if=none,id=vdb{disk_opts}",
        *gpu_display,
        # Built-in VNC server, LOCALHOST ONLY (no auth is safe ONLY because
        # of the 127.0.0.1 bind — HARD RULE, same as x86; never bind a
        # network interface without adding auth in the same change).
        "-vnc", f"127.0.0.1:{vnc_display}",
        "-device", "nec-usb-xhci,id=usb-bus",
        "-device", "qemu-xhci,id=usb-controller-0",
        "-device", "usb-tablet,bus=usb-bus.0",
        "-device", "usb-kbd,bus=usb-bus.0",
        "-netdev", ("user,id=net0,"
                    f"hostfwd=tcp:127.0.0.1:{acct['adb_port']}-:5555"),
        "-device", "virtio-net-pci,netdev=net0",
        "-device", "virtio-serial",
        "-device", "virtio-rng-pci",
        "-qmp", f"tcp:127.0.0.1:{acct['qmp_port']},server=on,wait=off",
        "-name", f"omni-{acct['name']}",
    ]
    # Dev accounts: attach the devkit disk as a THIRD virtio-blk (vdc). It is a
    # cheap per-account COW overlay of the shared base_arm_devkit.qcow2 (frida +
    # Magisk + omni tools). The guest sees it as /dev/block/vdc and mounts it
    # read-only during activation (see _devkit_activate). Not bootable.
    # Dev vdc: the shared devkit template (snapshot=on) for ephemeral instances,
    # else the per-account COW overlay.
    devkit_src = (images / base["devkit"]) if (ephemeral and base.get("devkit")) \
        else (d / "devkit.qcow2")
    if acct_is_dev(acct) and Path(devkit_src).exists():
        cmd += [
            "-device", "virtio-blk-pci,drive=vdc",
            "-drive", f"file={devkit_src},if=none,id=vdc{disk_opts}",
        ]
    if dev:
        cmd += ["-serial", f"file:{rd / 'serial.log'}"]
    return cmd


def qemu_command(acct, cfg, dev, mode=None, accel=None):
    from omnidroid.engine import account_dir
    base = cfg["bases"][acct["base"]]
    if base_type(base) == BASE_TYPE_ARM:
        return qemu_command_arm(acct, cfg, dev, mode=mode, accel=accel)
    images = Path(cfg["images_dir"])
    q = cfg["qemu"]
    d = account_dir(acct["name"])
    accel = accel or default_accel()
    mode = mode or resolve_mode(cfg)

    vnc_display = _assert_port_triple(acct)

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
        "-display", "none",       # headless ALWAYS; VNC below is an
                                  # attach point, never a window
        # Built-in VNC server on the account's reserved port. LOCALHOST
        # ONLY: no auth is safe ONLY because of the 127.0.0.1 bind — never
        # bind a network interface without adding auth in the same change.
        # Idle (no viewer) it does no framebuffer encoding, so leaving it
        # on costs ~nothing across hours-long headless runs; a viewer
        # disconnecting never affects the instance.
        "-vnc", f"127.0.0.1:{vnc_display}",
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


def _refresh_ephemeral_efivars(acct, cfg):
    """Give an ephemeral instance a FRESH copy of the base UEFI vars for this boot,
    so nothing persists across boots (the system/data/devkit disks are the shared
    templates opened snapshot=on; efivars is the only writable file, and pflash
    needs a real file). arm-only; no-op otherwise."""
    from omnidroid.engine import runtime_dir
    import shutil
    base = cfg["bases"][acct["base"]]
    if base_type(base) != BASE_TYPE_ARM:
        return
    images = Path(cfg["images_dir"])
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    efi_tmpl = images / base.get("efivars", ARM_BASE_EFIVARS)
    if efi_tmpl.exists():
        shutil.copyfile(efi_tmpl, d / "efivars.fd")


def spawn_qemu(acct, cfg, dev, mode=None, accel=None):
    from omnidroid.runtime import runtime_dir
    check_accel()
    d = runtime_dir(acct["name"])
    d.mkdir(parents=True, exist_ok=True)
    if acct.get("ephemeral"):
        _refresh_ephemeral_efivars(acct, cfg)
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
         "identity": f"omni-{acct['name']}",
         "mode": (mode or {}).get("name", "dev" if dev else DEFAULT_MODE),
         "base": acct["base"],
         "adb_port": acct["adb_port"], "qmp_port": acct["qmp_port"],
         "vnc_port": acct["vnc_port"]}))
    return proc.pid
