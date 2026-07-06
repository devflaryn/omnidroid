#!/bin/sh
# Build the single Linux `qemu-manager` ELF binary (PyInstaller onefile).
#
# RUN THIS ON THE LINUX BOX — PyInstaller cannot cross-build, so the
# Windows .exe is built on Windows (build-exe.ps1) and this binary is
# built on Linux. Same source, identical CLI on both platforms.
#
# Linux packaging policy: the binary relies on SYSTEM QEMU (never a
# portable download). One-time host prep:
#   sudo apt install qemu-system-x86 qemu-utils android-tools-adb
#   sudo apt install python3-pip && pip install pyinstaller
# Then: ./build-linux.sh  ->  dist/qemu-manager
# First run: ./dist/qemu-manager setup   (creates ~/OmniImages, preflights
# qemu / /dev/kvm / KSM and prints exact fixes for anything missing).
set -eu
root="$(cd "$(dirname "$0")" && pwd)"
cd "$root"
python3 -m PyInstaller --onefile --name qemu-manager \
    --distpath "$root/dist" --workpath "$root/build/pyi" \
    --specpath "$root/build" \
    "$root/manager/omni.py"
echo "BUILT: $root/dist/qemu-manager"
