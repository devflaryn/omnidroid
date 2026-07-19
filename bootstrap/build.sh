#!/bin/bash
# Build the in-Roblox session bootstrap and emit it as SMALI, ready for the
# omni-agent to drop into a decoded Roblox APK.
#
#   javac -> d8 (dex) -> baksmali (smali)
#
# Output:
#   build/omni-bootstrap.dex          the dex, if you prefer to merge dexes
#   build/smali/com/omni/bootstrap/*  smali, to copy into apktool's smali tree
#
# Why smali: the agent patches Roblox with apktool (decode -> edit smali ->
# rebuild), so the bootstrap has to arrive in the same currency as everything
# else it edits. Writing it in Java and compiling keeps the SOURCE reviewable
# instead of hand-writing register allocation.
#
# baksmali is optional: without it you still get the dex, and the agent can merge
# that instead. See contracts/omni-session.md §4.
set -euo pipefail
root="$(cd "$(dirname "$0")" && pwd)"

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
for cand in "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"; do
    [ -z "$sdk" ] && [ -d "$cand" ] && sdk="$cand"
done
[ -n "$sdk" ] || { echo "error: Android SDK not found (set ANDROID_HOME)"; exit 1; }

bt="$(ls -d "$sdk"/build-tools/* 2>/dev/null | sort -V | tail -1)"
androidJar="$(ls "$sdk"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -1)"
[ -n "$bt" ] && [ -n "$androidJar" ] || { echo "error: need build-tools + a platform in $sdk"; exit 1; }
echo "[build] build-tools=$bt"

out="$root/build"
rm -rf "$out"; mkdir -p "$out/classes"

# 1. Compile. minSdk 26 matches the Roblox client's own floor.
find "$root/src" -name '*.java' -print0 | xargs -0 -I{} echo '"{}"' > "$out/java.args"
javac --release 11 -cp "$androidJar" -d "$out/classes" "@$out/java.args"

# 2. Dex. Jar first so d8 takes one input (paths here contain spaces).
( cd "$out/classes" && jar cf "$out/classes-all.jar" . )
"$bt/d8" --release --lib "$androidJar" --min-api 26 --output "$out" \
    "$out/classes-all.jar"
mv "$out/classes.dex" "$out/omni-bootstrap.dex"
echo "[build] dex: $out/omni-bootstrap.dex"

# 3. Disassemble to smali, when a baksmali CLI is available.
#
# The dex above is the real artifact and is enough on its own: the agent injects
# it as an extra classes<N>.dex (see omni-agent tools/session_bootstrap.py), and
# cross-dex references resolve fine at runtime. Smali is only a convenience for
# an apktool decode/rebuild workflow that wants the class in its smali tree.
#
# Note: apktool 3.x bundles the baksmali LIBRARY but no CLI entry point
# (com.android.tools.smali.baksmali.Main is absent), so it cannot stand in here.
# omni-agent's sandbox pins apktool 2.9.3 and exposes a real wrapper.
baksmali=""
if command -v baksmali >/dev/null 2>&1; then
    baksmali="baksmali"
elif [ -f "$root/tools/baksmali.jar" ]; then
    baksmali="java -jar $root/tools/baksmali.jar"
fi
if [ -n "$baksmali" ]; then
    $baksmali d "$out/omni-bootstrap.dex" -o "$out/smali"
    echo "[build] smali: $out/smali/com/omni/bootstrap/"
    find "$out/smali" -name '*.smali' | sed 's/^/  /'
else
    echo "[build] note: no baksmali CLI; emitted the dex only (that is the"
    echo "[build]       artifact the agent injects - smali is optional)."
fi

echo "BUILT: $out/omni-bootstrap.dex"
