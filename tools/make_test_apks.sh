#!/bin/bash
# Test APKs for the warm device's install-by-content (crates/omni-mcp/src/device.rs), built from the
# probe app (crates/omni-linux/tests/fixtures/probe-app): the SDK's build-tools 36.0.0 and platform
# android-36, JDK 21 -- as its build.sh.
#
#   tools/make_test_apks.sh [out dir]      (default: target/test-apks)
#
#   a.apk        com.omnidroid.probe   versionCode 1 "1.0", blue, the probe's key
#   a2.apk       the same package, version and key; other bytes (green)  -> reinstalled
#   a-v2.apk     versionCode 2 "2.0" (orange)                              -> reinstalled
#   a-key2.apk   versionCode 1 "1.0", signed with another key (purple)    -> uninstalled, installed
#   b.apk        com.omnidroid.probe.b, versionCode 1 (red)                -> a uninstalled, b installed
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/target/test-apks}"
SRC="$ROOT/crates/omni-linux/tests/fixtures/probe-app"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
mkdir -p "$OUT"
# The SDK's .bat tools split a path at its spaces ("Omni Apps"): everything is made in a temp dir.
W="$(mktemp -d)"
rm -rf "$W" && mkdir -p "$W"
KEY2="$OUT/key2.keystore"
[ -f "$KEY2" ] || keytool -genkeypair -keystore "$KEY2" -storepass omniprobe -keypass omniprobe -alias probe -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=omnidroid probe, O=another key" >/dev/null 2>&1

# build <name> <colour ARGB> <versionCode> <versionName> <package> <keystore>
build() {
    local name=$1 colour=$2 code=$3 vname=$4 pkg=$5 ks=$6 d="$W/$1"
    mkdir -p "$d/src/com/omnidroid/probe" "$d/classes" "$d/dex"
    sed "s/0xff2196f3/$colour/" "$SRC/src/com/omnidroid/probe/MainActivity.java" > "$d/src/com/omnidroid/probe/MainActivity.java"
    "$BT/aapt2.exe" link -I "$JAR" --manifest "$SRC/AndroidManifest.xml" --min-sdk-version 26 --target-sdk-version 35 \
        --version-code "$code" --version-name "$vname" --rename-manifest-package "$pkg" -o "$d/unsigned.apk"
    javac --release 8 -cp "$JAR" -d "$d/classes" "$d/src/com/omnidroid/probe/MainActivity.java" 2>/dev/null
    "$BT/d8.bat" --min-api 26 --lib "$JAR" --output "$d/dex" $(find "$d/classes" -name "*.class")
    (cd "$d/dex" && jar uf ../unsigned.apk classes.dex)
    "$BT/zipalign.exe" -f 4 "$d/unsigned.apk" "$d/aligned.apk"
    cp "$ks" "$d/key.keystore"
    "$BT/apksigner.bat" sign --ks "$d/key.keystore" --ks-pass pass:omniprobe --key-pass pass:omniprobe --out "$d/$name.apk" "$d/aligned.apk"
    cp "$d/$name.apk" "$OUT/$name.apk"
}
build a      0xff2196f3 1 1.0 com.omnidroid.probe   "$SRC/probe.keystore"
build a2     0xff4caf50 1 1.0 com.omnidroid.probe   "$SRC/probe.keystore"
build a-v2   0xffff9800 2 2.0 com.omnidroid.probe   "$SRC/probe.keystore"
build a-key2 0xff9c27b0 1 1.0 com.omnidroid.probe   "$KEY2"
build b      0xfff44336 1 1.0 com.omnidroid.probe.b "$SRC/probe.keystore"
rm -rf "$W"
cd "$OUT" && sha256sum *.apk
