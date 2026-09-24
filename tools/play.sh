#!/bin/sh
# Run the Roblox APK in a real window on macOS: the gate's own run, with a long session and this
# Mac's GPU through MoltenVK. The macOS twin of tools/play.ps1, with the same switches.
#
#   tools/play.sh                  # until the window is closed, storage kept
#   tools/play.sh --minutes 90
#   tools/play.sh --fresh          # a fresh install
#   tools/play.sh --phone          # a touch screen instead: the mouse is a finger, no keyboard
#
# **The app's storage is kept** in --data-dir (default ~/Library/Application Support/Omnidroid/data),
# a signed-in session included. --fresh runs a fresh install and leaves --data-dir as it is.
#
# **End a run by closing the window** (the red button asks; the app is then closed as a device
# closes it). A run ended any other way -- Ctrl+C, a crash -- is judged a crash by the engine at the
# next launch; after one, run once with --fresh, or empty --data-dir (which signs you out).
#
# **The keyboard and the mouse are this Mac's** (OMNI_KEYBOARD_MOUSE=1) unless --phone: WASD, Space,
# Tab, Esc, right-drag camera, the wheel/trackpad zoom, and a captured pointer while the game locks
# the mouse.
#
# Requires: Homebrew's cmake, ninja, molten-vk and vulkan-loader (docs/ports/macos.md, "Build from a
# fresh Mac"), and the APK at the repository root.
set -eu

minutes=0
fresh=0
phone=0
data_dir="$HOME/Library/Application Support/Omnidroid/data"
while [ $# -gt 0 ]; do
    case "$1" in
        --minutes) minutes="$2"; shift 2 ;;
        --fresh) fresh=1; shift ;;
        --phone) phone=1; shift ;;
        --data-dir) data_dir="$2"; shift 2 ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

repo="$(cd "$(dirname "$0")/.." && pwd)"
export OMNI_M6_ROWS_21_22=1
export OMNI_GFX_WINDOW_TESTS=1
if [ "$phone" -eq 1 ]; then
    unset OMNI_KEYBOARD_MOUSE || true
    echo "Omnidroid: the phone configuration -- the mouse is a finger, and there is no keyboard"
else
    export OMNI_KEYBOARD_MOUSE=1
    echo "Omnidroid: this Mac's keyboard and mouse (OMNI_KEYBOARD_MOUSE=1)"
fi
# 0: until the window is closed -- ten years is the gate's way of saying no limit.
if [ "$minutes" -gt 0 ]; then
    export OMNI_SESSION_SECONDS=$((minutes * 60))
    length="a $minutes-minute session"
else
    export OMNI_SESSION_SECONDS=315360000
    length="a session until the window is closed"
fi
if [ "$fresh" -eq 1 ]; then
    unset OMNI_DATA_DIR || true
    echo "Omnidroid: $length on a fresh install"
else
    mkdir -p "$data_dir"
    export OMNI_DATA_DIR="$data_dir"
    echo "Omnidroid: $length; the app's storage is kept in $data_dir"
fi
echo "End the session by closing the window."
cd "$repo"
exec cargo test -p omni-android --release --test gameactivity -- --nocapture --test-threads=1 \
    initialize_native_code_returns_a_native_code_and_the_game_thread_starts
