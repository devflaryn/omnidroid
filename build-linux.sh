#!/bin/sh
# Build the single Linux `omnidroid` ELF binary (PyInstaller onefile).
#
# RUN THIS ON THE LINUX BOX — PyInstaller cannot cross-build, so the
# Windows .exe is built on Windows (build-exe.ps1) and this binary is
# built on Linux. Same source, identical CLI on both platforms.
#
# Linux packaging policy: the binary relies on SYSTEM QEMU (never a
# portable download). One-time host prep:
#   sudo apt install qemu-system-x86 qemu-utils android-tools-adb
#   sudo apt install python3-pip && pip install pyinstaller
# Then: ./build-linux.sh  ->  dist/omnidroid
# First run: ./dist/omnidroid setup   (creates ~/OmniImages, preflights
# qemu / /dev/kvm / KSM and prints exact fixes for anything missing).
set -eu
root="$(cd "$(dirname "$0")" && pwd)"
cd "$root"
# The built-in VNC viewer (manager/vncview.py) is imported lazily by name,
# so PyInstaller can't auto-detect it — add it and its GUI deps explicitly.
# (tkinter is usually picked up by PyInstaller's hooks; keep it listed to be
# safe. Linux needs the system Tk: sudo apt install python3-tk.)
python3 -m PyInstaller --onefile --name omnidroid \
    --distpath "$root/dist" --workpath "$root/build/pyi" \
    --specpath "$root/build" \
    --paths "$root/manager" \
    --hidden-import vncview \
    --hidden-import tkinter \
    --hidden-import PIL.Image --hidden-import PIL.ImageTk \
    "$root/manager/omni.py"
echo "BUILT: $root/dist/omnidroid"
