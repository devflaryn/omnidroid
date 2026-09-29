#!/bin/sh
# Run an APK on the real-AOSP path (omni-linux: real Android boots, the app installed with
# `pm install` and started from its launcher) in the live window, on macOS or Linux. The shell
# spelling of tools/aosp_play.ps1: the same run (`tests/r_roblox.rs`), the same switches.
#
#   tools/aosp_play.sh                                       # the stock APK, logged out
#   tools/aosp_play.sh --apk ~/Desktop/Roblox-2.740.931.apk --cookie cookie.txt --place 8737899170
#   tools/aosp_play.sh --minutes 60 --size 1600x900
#   tools/aosp_play.sh --not-resizable | --with-systemui | --show-chrome
#   tools/aosp_play.sh --target x86_64-apple-darwin ...     # the x64 backend under Rosetta 2
#
# The session ends after --minutes (default 30). The whole log is in $TMPDIR/omni-linux-r-<pid>.log;
# screenshots of the display in $TMPDIR/omni-linux-r-<pid>-shots. The sysroot is sysroot/aosp-35
# (tools/make_sysroot.py) or OMNI_SYSROOT. On macOS the guest's low 4 GiB are a based window (D41).
set -eu
apk="" cookie="" place="" minutes=30 size="" target=""
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        --apk) apk="$2"; shift 2 ;;
        --cookie) cookie="$2"; shift 2 ;;
        --place) place="$2"; shift 2 ;;
        --minutes) minutes="$2"; shift 2 ;;
        --size) size="$2"; shift 2 ;;
        --target) target="$2"; shift 2 ;;
        --not-resizable) export OMNI_R_RESIZABLE=0; shift ;;
        --with-systemui) export OMNI_R_KIOSK=0; shift ;;
        --show-chrome) export OMNI_APP_ONLY=0; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
repo="$(cd "$(dirname "$0")/.." && pwd)"
abs() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd)" "$(basename "$1")"); }
export OMNI_WINDOW=1 OMNI_R_MINUTES="$minutes"
export OMNI_TEST_APK="$(abs "${apk:-$repo/Roblox-2.738.1397.apk}")"
[ -n "$cookie" ] && export OMNI_R_COOKIE="$(abs "$cookie")"
[ -n "$place" ] && export OMNI_R_PLACE="$place"
[ -n "$size" ] && export OMNI_WINDOW_SIZE="$size"
export OMNI_R_KIOSK="${OMNI_R_KIOSK:-1}"
# The CPU backend's CMake build lives outside the checkout on macOS, in
# ~/Library/Caches/omnidroid/dynarmic (OMNIDROID_DYNARMIC_BUILD_DIR overrides): see
# crates/dynarmic-sys/build.rs.
rm -f "${TMPDIR:-/tmp}"/omni-shm-* 2>/dev/null || true
cd "$repo"
if [ -n "$target" ]; then
    # Another architecture's build on this Mac (x86-64 under Rosetta 2): Apple's libc++ needs a
    # deployment target of 13 for what dynarmic uses.
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
    exec cargo test --release -q --target "$target" -p omni-linux --test r_roblox -- --ignored --nocapture
fi
exec cargo test --release -q -p omni-linux --test r_roblox -- --ignored --nocapture
