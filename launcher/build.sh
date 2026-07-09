#!/bin/bash
# Build the Omni Kiosk APK without Gradle: aapt2 -> javac -> d8 -> sign.
# macOS/Linux counterpart of build.ps1 (identical output: build/omni-kiosk.apk).
# The kiosk is pure Java (no native code) so one APK runs on x86 AND arm64.
set -euo pipefail
root="$(cd "$(dirname "$0")" && pwd)"

# Locate the Android SDK (macOS default, then Linux, then $ANDROID_HOME).
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
for cand in "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"; do
    [ -z "$sdk" ] && [ -d "$cand" ] && sdk="$cand"
done
[ -n "$sdk" ] || { echo "error: Android SDK not found (set ANDROID_HOME)"; exit 1; }

# Newest build-tools that has aapt2/d8/apksigner; newest installed platform jar.
bt="$(ls -d "$sdk"/build-tools/* 2>/dev/null | sort -V | tail -1)"
androidJar="$(ls "$sdk"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -1)"
[ -n "$bt" ] && [ -n "$androidJar" ] || { echo "error: need build-tools + a platform in $sdk"; exit 1; }
echo "[build] sdk=$sdk"
echo "[build] build-tools=$bt"
echo "[build] android.jar=$androidJar"

out="$root/build"
rm -rf "$out"; mkdir -p "$out/classes" "$out/compiled"

# 1a. Compile resources (res/xml/device_admin.xml for the device-admin receiver)
"$bt/aapt2" compile --dir "$root/res" -o "$out/compiled/res.zip"
# 1b. Link manifest + compiled resources
"$bt/aapt2" link --manifest "$root/AndroidManifest.xml" \
    -I "$androidJar" --min-sdk-version 26 --target-sdk-version 33 \
    "$out/compiled/res.zip" -o "$out/base.apk"

# 2. Compile Java (release 11 bytecode; d8 desugars for minSdk 26).
# Use an argfile so paths containing spaces (e.g. "Omni Apps") are safe.
find "$root/src" -name '*.java' -print0 | xargs -0 -I{} echo '"{}"' > "$out/java.args"
javac --release 11 -cp "$androidJar" -d "$out/classes" "@$out/java.args"

# 3. Dex. Jar the classes first so d8 takes a single .jar input (no
# per-.class file list -> immune to spaces in the project path).
( cd "$out/classes" && jar cf "$out/classes-all.jar" . )
"$bt/d8" --release --lib "$androidJar" --min-api 26 --output "$out" \
    "$out/classes-all.jar"

# 4. Add classes.dex into the APK (jar adds at archive root)
( cd "$out" && jar uf base.apk classes.dex )

# 5. Align + sign (debug keystore, generated once)
ks="$root/omni-debug.jks"
if [ ! -f "$ks" ]; then
    keytool -genkeypair -keystore "$ks" -alias omni -keyalg RSA -keysize 2048 \
        -validity 10000 -storepass omnidroid -keypass omnidroid -dname "CN=OmniKiosk"
fi
"$bt/zipalign" -f 4 "$out/base.apk" "$out/omni-kiosk-unsigned.apk"
"$bt/apksigner" sign --ks "$ks" --ks-pass pass:omnidroid \
    --key-pass pass:omnidroid --out "$out/omni-kiosk.apk" \
    "$out/omni-kiosk-unsigned.apk"

echo "BUILT: $out/omni-kiosk.apk ($(stat -f%z "$out/omni-kiosk.apk" 2>/dev/null || stat -c%s "$out/omni-kiosk.apk") bytes)"
