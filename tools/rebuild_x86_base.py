#!/usr/bin/env python3
"""Rebuild the shipped x86 base + arceus offset in one builder boot.

WHY THIS EXISTS
---------------
The x86 base image shipped a kiosk build from before the SET_SESSION contract
existed. Two consequences, both fatal to the product's whole promise:

  * `omnidroid start` reached every stage but the last and reported
    `no_kiosk_reply` — com.omni.kiosk had no SessionReceiver, so the cookie was
    never delivered and Roblox never logged in or joined.
  * the instance booted to Bliss's LOCK SCREEN ("swipe up to enter") and waited
    there. On a headless farming instance nobody is watching to swipe.

Both live in the IMAGE, not in the code, so no amount of host-side work fixes
them: the kiosk is a /system app (a signed system APK — `pm install -r` is
refused, the signing key was rotated after this base was built), and the lock
screen is a /data setting inside the pre-baked offset.

`omnidroid update-kiosk` (update_kiosk_base) already does the /system half, but
it emits a NEW base under the old flat `base-vN.qcow2` naming and leaves the
offset alone. What ships is `images/x86/base_x86.qcow2` plus a matching
`base_x86_data_offset_arceusremote.qcow2`, and the two have to be rebuilt
TOGETHER from ONE boot — a /data that was device-owner-provisioned against a
different /system is exactly the kind of mismatch that boots fine and then
misbehaves.

WHAT IT DOES
------------
Boots one throwaway instance with a persistent overlay of the base and a copy
of the offset, then:

  /system   replace /system/app/OmniKiosk/OmniKiosk.apk with launcher/build/
            omni-kiosk.apk (the current build, with SessionReceiver)
  /data     kill the lock screen, mark setup complete, make the kiosk the HOME
            app and the DEVICE OWNER, disable the Bliss launchers, and pin
            com.roblox.client as the game

...powers off cleanly, and flattens both disks back over the shipped names.
Originals are kept as .bak so a bad bake is one `mv` from being undone.

    python tools/rebuild_x86_base.py [--kiosk PATH] [--offset NAME] [--dry-run]

Run it with the same OMNIDROID_CONFIG_PATH / OMNI_DATA_DIR / OMNI_IMAGES_DIR
environment the product uses.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from omnidroid import engine as E                          # noqa: E402
from omnidroid.config import qemu_bin                      # noqa: E402

BUILDER = "_x86rebuild"
KIOSK_PKG = "com.omni.kiosk"
ROBLOX_PKG = "com.roblox.client"
SYSTEM_APP_DIR = "/system/app/OmniKiosk"


_OFFLINE = ("device offline", "device not found", "no devices", "closed")


def sh(acct, script, timeout=60, label="build", tries=6):
    """One adb shell command, retried through a flaky endpoint.

    Restarting the framework (`stop; start`, which the kiosk swap needs) drops
    adbd, and the host's adb server parks the endpoint in `offline` — where it
    STAYS, because `adb connect` on a known endpoint just answers "already
    connected". A single-shot call therefore comes back "device offline" and,
    if that answer is treated as the command's output, every provisioning step
    silently does nothing while reporting something that looks like a result.
    That is exactly what happened on the first rebuild: the kiosk was swapped
    and not one /data setting was written.
    """
    last = ""
    for attempt in range(tries):
        try:
            r = E.adb(acct, "shell", script, timeout=timeout)
            last = ((r.stdout or "") + (r.stderr or "")).strip()
        except Exception as exc:  # noqa: BLE001
            last = f"<adb error: {exc}>"
        if not any(marker in last.lower() for marker in _OFFLINE):
            return last
        E.adb_recover(acct, hard=attempt >= 2)
        time.sleep(3 if attempt < 2 else 6)
    print(f"[{label}] adb still offline after {tries} attempts: {last}")
    return last


def swap_kiosk(acct, kiosk_apk, label):
    """Replace the /system kiosk. Root adbd + a writable rootfs is required;
    on the Bliss base adbd already runs as uid 0, so this is a remount away."""
    print(f"[{label}] remounting / read-write")
    print("        " + sh(acct, "mount -o remount,rw / 2>&1 || true", label=label))
    E.adb(acct, "push", str(kiosk_apk), "/data/local/tmp/omni-kiosk.apk", timeout=180)
    out = sh(acct,
             f"rm -f {SYSTEM_APP_DIR}/OmniKiosk.apk && "
             f"mkdir -p {SYSTEM_APP_DIR} && "
             f"cp /data/local/tmp/omni-kiosk.apk {SYSTEM_APP_DIR}/OmniKiosk.apk && "
             f"chmod 644 {SYSTEM_APP_DIR}/OmniKiosk.apk && "
             f"chcon u:object_r:system_file:s0 {SYSTEM_APP_DIR} "
             f"{SYSTEM_APP_DIR}/OmniKiosk.apk && "
             f"rm -f /data/local/tmp/omni-kiosk.apk && echo KIOSK_OK",
             timeout=120, label=label)
    if "KIOSK_OK" not in out:
        sys.exit(f"[{label}] kiosk swap failed: {out}")
    print(f"[{label}] /system kiosk replaced")

    # The framework caches the parsed package; restart it so the new receivers
    # register and the device-owner set below lands on the NEW kiosk.
    print(f"[{label}] restarting the framework to pick up the new kiosk")
    sh(acct, "stop", timeout=30, label=label)
    time.sleep(2)
    sh(acct, "start", timeout=30, label=label)
    deadline = time.time() + 240
    while time.time() < deadline:
        if sh(acct, "getprop sys.boot_completed", timeout=15,
              label=label, tries=2).strip() == "1":
            break
        time.sleep(3)
    # PackageManager finishes scanning after boot_completed; the device-owner
    # set below needs the new kiosk fully registered, not merely installed.
    time.sleep(10)
    receivers = sh(acct, f"dumpsys package {KIOSK_PKG} | grep -c SET_SESSION",
                   timeout=30, label=label)
    if receivers.strip() in ("", "0"):
        sys.exit(f"[{label}] the new kiosk registered no SET_SESSION receiver — "
                 f"the swap did not take effect")
    print(f"[{label}] SessionReceiver is registered")


def provision_data(acct, label):
    """Everything that has to live in /data for a boot to reach the game with
    nobody watching."""
    steps = [
        # --- the lock screen. Both halves: locksettings owns the credential
        # side, the secure setting owns the "show it at all" side, and images
        # in the wild have been seen honouring one but not the other.
        ("lock screen off", "locksettings set-disabled true; "
                            "settings put secure lockscreen.disabled 1"),
        # --- no setup wizard, no first-run gate
        ("setup complete", "settings put global device_provisioned 1; "
                           "settings put secure user_setup_complete 1"),
        # --- no "swipe down to exit full screen" toast over the game
        ("immersive confirmed",
         "settings put secure immersive_mode_confirmations confirmed"),
        # --- which package the kiosk should launch
        ("game package", f"settings put global omni_game_package {ROBLOX_PKG}"),
    ]
    for name, script in steps:
        out = sh(acct, script, timeout=45, label=label)
        print(f"[{label}] {name}: {out or 'ok'}")

    # Read it back rather than trusting the write: a `settings put` that lands
    # on an offline endpoint reports nothing and changes nothing, and the whole
    # point of this rebuild is that the lock screen is actually gone.
    got = sh(acct, "settings get secure lockscreen.disabled", timeout=30, label=label)
    if got.strip() != "1":
        sys.exit(f"[{label}] lock screen is STILL enabled "
                 f"(lockscreen.disabled={got!r}) — refusing to bake an image "
                 f"with the bug it exists to fix")

    # Device owner. This is what lets the kiosk disable the keyguard outright,
    # auto-grant runtime permissions, and hold Lock Task — none of which an
    # ordinary app can do. It must be set BEFORE any account exists on the
    # device, which on a freshly-provisioned image is now.
    out = sh(acct, f"dpm set-device-owner {KIOSK_PKG}/.OmniDeviceAdminReceiver",
             timeout=60, label=label)
    print(f"[{label}] device owner: {out}")
    if "Success" not in out:
        # Not fatal: the lock screen is already off via locksettings and the
        # host grants permissions over adb. Say so loudly rather than pretend.
        print(f"[{label}] WARNING: device owner NOT set — the kiosk keeps "
              f"working, but keyguard-disable and Lock Task will be skipped")

    # The kiosk becomes HOME, and the stock launchers stop competing for it.
    out = sh(acct, f"cmd package set-home-activity --user 0 "
                   f"{KIOSK_PKG}/.MainActivity", timeout=45, label=label)
    print(f"[{label}] home activity: {out}")
    for pkg in E.BLISS_HOME_PACKAGES:
        sh(acct, f"pm disable-user --user 0 {pkg}", timeout=30, label=label)
    print(f"[{label}] disabled Bliss launchers: {', '.join(E.BLISS_HOME_PACKAGES)}")

    # Deliberately NOT launching the kiosk here. As device owner it enters Lock
    # Task on start, and Lock Task BLOCKS `reboot -p` — the guest would then
    # never power off and the flatten below would capture a dirty /data. (Same
    # trap update_kiosk_arm documents.)


def verify(acct, label):
    """Prove the two things this rebuild exists to fix, before we bake."""
    checks = {
        "lockscreen.disabled": sh(acct, "settings get secure lockscreen.disabled",
                                  label=label),
        "locksettings": sh(acct, "locksettings get-disabled", label=label),
        "kiosk receivers": sh(acct, f"dumpsys package {KIOSK_PKG} "
                                    f"| grep -c SET_SESSION", label=label),
        "device owner": sh(acct, "dumpsys device_policy | grep -c "
                                 "'Device Owner'", label=label),
    }
    for k, v in checks.items():
        print(f"[{label}] verify {k}: {v}")
    return checks


def overlay(path, backing, label):
    """A writable qcow2 layered on `backing`, absolute-referenced."""
    Path(path).unlink(missing_ok=True)
    subprocess.run([qemu_bin("qemu-img"), "create", "-f", "qcow2",
                    "-b", str(Path(backing).resolve()), "-F", "qcow2", str(path)],
                   check=True, capture_output=True, timeout=300)
    print(f"[{label}] {Path(path).name} <- {Path(backing).name}")


def commit(src, dst, label):
    """Fold the builder's delta down into the shipped image, in place.

    `qemu-img commit`, not `convert`. The arceus offset is itself a THIN
    OVERLAY of `data-template-8g.qcow2`, and flattening it would produce a
    standalone multi-gigabyte /data in place of a 750 MB delta — a much larger
    artifact for every user to download, for no gain. Commit writes only the
    new blocks into the existing image and leaves its own backing pointer
    exactly as it was.

    (Copying the offset instead of layering on it is what broke the first
    attempt: its backing reference is the RELATIVE name `data-template-8g.qcow2`,
    which resolves against the image's own directory, so a copy elsewhere could
    not find it and QEMU exited with "Could not open backing file".)
    """
    backup = Path(str(dst) + ".bak")
    if Path(dst).exists() and not backup.exists():
        print(f"[{label}] backing up {Path(dst).name} -> {backup.name}")
        shutil.copy2(dst, backup)
    before = Path(dst).stat().st_size
    print(f"[{label}] committing {Path(src).name} into {Path(dst).name}")
    subprocess.run([qemu_bin("qemu-img"), "commit", "-f", "qcow2", str(src)],
                   check=True, timeout=3600)
    after = Path(dst).stat().st_size
    print(f"[{label}] {Path(dst).name}: {before/1048576:.0f} -> "
          f"{after/1048576:.0f} MiB")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--kiosk", default=None,
                    help="path to omni-kiosk.apk (default: launcher/build/omni-kiosk.apk)")
    ap.add_argument("--offset", default=None,
                    help="offset name to rebake (default: the base's default_offset)")
    ap.add_argument("--base", default=None, help="base tag (default: current_base)")
    ap.add_argument("--dry-run", action="store_true",
                    help="boot and mutate, but do not flatten over the shipped images")
    args = ap.parse_args()

    E.ensure_qemu()
    cfg = E.load_config()
    tag = args.base or cfg["current_base"]
    base = cfg["bases"][tag]
    images = Path(cfg["images_dir"])
    kiosk = Path(args.kiosk) if args.kiosk else (
        Path(__file__).resolve().parent.parent / "launcher" / "build" / "omni-kiosk.apk")
    if not kiosk.exists():
        sys.exit(f"kiosk apk not found: {kiosk} (build it with launcher/build.ps1)")

    offset_name = args.offset or base.get("default_offset")
    offsets = base.get("offsets") or {}
    if offset_name not in offsets:
        sys.exit(f"no offset '{offset_name}' on base '{tag}' "
                 f"(have: {', '.join(offsets) or 'none'})")
    system_img = images / base["disk"]
    offset_img = images / offsets[offset_name]["data"]
    for p in (system_img, offset_img):
        if not p.exists():
            sys.exit(f"missing image: {p}")

    live = [a["name"] for a in E.all_accounts() if E.running_pid(a["name"])]
    if live:
        sys.exit(f"stop running instances first: {', '.join(live)} — the base "
                 f"is rebuilt from a clean boot")

    label = f"rebuild {tag}/{offset_name}"
    print(f"[{label}] system {system_img.name} ({system_img.stat().st_size/1048576:.0f} MiB)")
    print(f"[{label}] offset {offset_img.name} ({offset_img.stat().st_size/1048576:.0f} MiB)")
    print(f"[{label}] kiosk  {kiosk} ({kiosk.stat().st_size} bytes)")

    d = E.account_dir(BUILDER)
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    adb_port, qmp_port, vnc_port = E.allocate_ports(cfg)
    acct = {"name": BUILDER, "base": tag, "adb_port": adb_port,
            "qmp_port": qmp_port, "vnc_port": vnc_port,
            "first_boot_done": True}
    E.save_account(acct)
    # Persistent overlays, NOT the production snapshot=on path: the whole point
    # is to keep what this boot writes, and to commit it back down afterwards.
    overlay(d / "system.qcow2", system_img, label)
    overlay(d / "data.qcow2", offset_img, label)

    try:
        print(f"[{label}] booting the builder")
        E.spawn_qemu(acct, cfg, interactive=True)
        if not E.wait_for_boot(acct, E.FIRST_BOOT_TIMEOUT, label):
            sys.exit(f"[{label}] the builder did not boot; images untouched")
        E.adb(acct, "root")
        time.sleep(3)
        E.adb_connect(acct)

        swap_kiosk(acct, kiosk, label)
        provision_data(acct, label)
        checks = verify(acct, label)

        sh(acct, "sync", timeout=60, label=label)
        method = E._shutdown(acct, label, timeout=180)
        print(f"[{label}] power-off: {method}")
        if method in ("killed", "kill-failed"):
            sys.exit(f"[{label}] the builder would not power off ({method}); "
                     f"refusing to bake a possibly-inconsistent image")

        if args.dry_run:
            print(f"[{label}] --dry-run: shipped images left untouched")
            print(json.dumps({"ok": True, "dry_run": True, "checks": checks}))
            return

        commit(d / "system.qcow2", system_img, label)
        commit(d / "data.qcow2", offset_img, label)
        print(json.dumps({"ok": True, "base": tag, "offset": offset_name,
                          "system": str(system_img), "data": str(offset_img),
                          "checks": checks}))
    finally:
        try:
            if E.running_pid(BUILDER):
                E._shutdown(acct, label, timeout=60)
        except Exception:  # noqa: BLE001
            pass
        shutil.rmtree(d, ignore_errors=True)
        print(f"[{label}] removed the builder account")


if __name__ == "__main__":
    main()
