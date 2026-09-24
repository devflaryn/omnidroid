#!/bin/sh
# Run an APK in a real window for a person to use, on macOS or Linux. The shell spelling of the
# launcher, which is now the `omnidroid` binary (crates/omnidroid) -- the same run, and the same
# switches, on Windows, macOS and Linux.
#
#   tools/play.sh                          # the newest APK in the repository root
#   tools/play.sh --apk ~/apks/Roblox-2.740.1.apk
#   tools/play.sh --place 8737899170       # join that place once the saved sign-in is at Home
#   tools/play.sh --minutes 90
#   tools/play.sh --fresh                  # a fresh install
#   tools/play.sh --phone                  # a touch screen instead: the mouse is a finger, no keyboard
#   tools/play.sh --data-dir <dir>         # default: ~/Library/Application Support/Omnidroid/data on
#                                          # macOS, $XDG_DATA_HOME/omnidroid/data on Linux
#
# **The APK is chosen, not built in**: --apk, else OMNI_APK, else the newest APK (by versionCode) in
# the repository root. Its version comes from its own manifest. See crates/omnidroid/src/main.rs.
#
# macOS requires Homebrew's cmake, ninja, molten-vk and vulkan-loader (docs/ports/macos.md, "Build
# from a fresh Mac"); Linux, docs/ports/linux.md.
set -eu
case "${1:-}" in
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
esac
repo="$(cd "$(dirname "$0")/.." && pwd)"
# No `cd`: an --apk or --data-dir given relative to where you are stays relative to it.
exec cargo run --release -q --manifest-path "$repo/Cargo.toml" -p omnidroid -- play "$@"
