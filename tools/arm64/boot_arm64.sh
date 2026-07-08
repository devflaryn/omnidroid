#!/bin/zsh
# boot_arm64.sh — PROOF-OF-LIFE reference boot for arm64 Android under QEMU/HVF
# on Apple Silicon (macOS). NOT the production omnidroid boot path — this is the
# minimal command that got LineageOS 23.2 (Android 16, arm64-v8a) to a booted,
# provisioned home screen on a Mac Mini (M-series), headless + localhost VNC.
#
# See HANDOFF.md → "ARM64 / Apple Silicon (proof-of-life, 2026-07-08)" for the
# full findings, the image source, and the build-time notes for base_arm.
#
# Prereqs (Apple Silicon macOS):
#   brew install qemu android-platform-tools
#   HVF is Apple's hypervisor; verify with:  sysctl kern.hv_support  (want 1)
#
# Image (viable base source — arm64-native, NO translation layer needed):
#   jqssun/android-lineage-qemu, release asset
#   UTM-VM-lineage-23.2-*-virtio_arm64only.zip
#   Unzip it; it contains a  LineageOS_on_arm64.utm/Data/  dir holding
#   vda.qcow2, vdb.qcow2, efi_vars.fd.
#
# Usage:
#   ./boot_arm64.sh /path/to/LineageOS_on_arm64.utm/Data   [vnc_display_N]
#   (vnc_display_N default 7 → VNC on 127.0.0.1:5907; adb on 127.0.0.1:5555)
#
# Then, in another shell:
#   adb connect 127.0.0.1:5555
# NOTE: this image's adbd starts in "trade-in mode" and refuses `shell:` until
# the first-boot setup wizard is completed — finish setup (via VNC) first.
set -e

DATA="${1:?usage: boot_arm64.sh <VM Data dir with vda.qcow2/vdb.qcow2/efi_vars.fd> [vnc_display_N]}"
VNC_N="${2:-7}"

# EDK2 aarch64 firmware ships with the Homebrew qemu formula.
CODE=$(/usr/bin/find /opt/homebrew/Cellar/qemu -name edk2-aarch64-code.fd 2>/dev/null | head -1)
: "${CODE:?edk2-aarch64-code.fd not found — is qemu installed via brew?}"

for f in vda.qcow2 vdb.qcow2 efi_vars.fd; do
  [ -f "$DATA/$f" ] || { echo "missing $DATA/$f" >&2; exit 1; }
done

exec qemu-system-aarch64 \
  -machine virt \
  -accel hvf \
  -cpu host \
  -smp 4 \
  -m 4096 \
  -drive if=pflash,unit=0,file="$CODE",file.locking=off,format=raw,readonly=on \
  -drive if=pflash,unit=1,file="$DATA/efi_vars.fd" \
  -device virtio-blk-pci,drive=vda,bootindex=0 \
  -device virtio-blk-pci,drive=vdb,bootindex=1 \
  -drive file="$DATA/vda.qcow2",if=none,id=vda,discard=unmap,detect-zeroes=unmap \
  -drive file="$DATA/vdb.qcow2",if=none,id=vdb,discard=unmap,detect-zeroes=unmap \
  -device virtio-net-pci,netdev=net0 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:5555-:5555 \
  -device nec-usb-xhci,id=usb-bus \
  -device qemu-xhci,id=usb-controller-0 \
  -device usb-tablet,bus=usb-bus.0 \
  -device usb-kbd,bus=usb-bus.0 \
  -device virtio-gpu-pci \
  -display none \
  -vnc 127.0.0.1:"$VNC_N" \
  -device virtio-serial \
  -device virtio-rng-pci \
  -qmp tcp:127.0.0.1:4444,server,nowait \
  -pidfile /tmp/omnidroid-arm64-poc.pid

# Clean shutdown (from another shell):
#   adb -s emulator-5554 shell reboot -p        # cleanest (guest-side)
#   -- or QMP:
#   printf '{"execute":"qmp_capabilities"}\n{"execute":"system_powerdown"}\n' | nc -w3 127.0.0.1 4444
# (ACPI powerdown alone may not halt an idle home screen; the adb path is reliable.)
