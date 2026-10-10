#!/bin/sh
# Run any APK on the real-AOSP path (real Android boots, the APK is installed with `pm install` and
# started from its launcher Activity) in the live window, on macOS or Linux: `omnidroid aosp`, with
# the same switches (`omnidroid help`), plus a few of this script's own.
#
#   tools/aosp_play.sh ~/apks/app.apk                        # any APK
#   tools/aosp_play.sh --apk app.apk --minutes 60 --size 1600x900
#   tools/aosp_play.sh --apk app.apk --not-resizable | --show-chrome
#   tools/aosp_play.sh --apk app.apk <a plugin's options>    # e.g. those `omnidroid help` lists
#   tools/aosp_play.sh --target x86_64-apple-darwin --apk app.apk   # the x64 backend under Rosetta 2
#
# The session ends after --minutes (default 30). The whole log is in $TMPDIR/omni-linux-r-<pid>.log;
# screenshots of the display in $TMPDIR/omni-linux-r-<pid>-shots. The sysroot is sysroot/aosp-35
# (tools/make_sysroot.py) or OMNI_SYSROOT. On macOS the guest's low 4 GiB are a based window (D41).
set -eu
target=""
for a in "$@"; do
    case "$a" in -h|--help) sed -n '2,14p' "$0"; exit 0 ;; esac
done
# This script's own switches; everything else goes to `omnidroid aosp`.
n=$#
while [ "$n" -gt 0 ]; do
    a="$1"; shift; n=$((n - 1))
    case "$a" in
        --target) target="$1"; shift; n=$((n - 1)) ;;
        --not-resizable) export OMNI_R_RESIZABLE=0 ;;
        --show-chrome) export OMNI_APP_ONLY=0 ;;
        *) set -- "$@" "$a" ;;
    esac
done
repo="$(cd "$(dirname "$0")/.." && pwd)"
if [ -n "$target" ]; then
    # Another architecture's build on this Mac (x86-64 under Rosetta 2): Apple's libc++ needs a
    # deployment target of 13 for what dynarmic uses.
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
    exec cargo run --release -q --target "$target" --manifest-path "$repo/Cargo.toml" -p omnidroid -- aosp "$@"
fi
# No `cd`: an --apk given relative to where you are stays relative to it.
exec cargo run --release -q --manifest-path "$repo/Cargo.toml" -p omnidroid -- aosp "$@"
