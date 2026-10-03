#!/bin/bash
# Build su-probe.apk (debug-signed with the probe's key): the SDK's build-tools 36.0.0 and platform
# android-36, JDK 21 -- as ../probe-app/build.sh. Not committed (*.apk): the su-probe test builds it.
#   build.sh [out.apk]      (default: ./su-probe.apk)
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
OUTAPK="${1:-$HERE/su-probe.apk}"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
# The SDK's .bat tools split a path at its spaces: everything is made in a temp dir.
W="$(mktemp -d)"
mkdir -p "$W/classes" "$W/dex"
"$BT/aapt2.exe" link -I "$JAR" --manifest "$HERE/AndroidManifest.xml" --min-sdk-version 26 --target-sdk-version 35 -o "$W/unsigned.apk"
javac --release 8 -cp "$JAR" -d "$W/classes" "$HERE/src/com/omnidroid/suprobe/MainActivity.java"
"$BT/d8.bat" --min-api 26 --lib "$JAR" --output "$W/dex" $(find "$W/classes" -name "*.class")
(cd "$W/dex" && jar uf ../unsigned.apk classes.dex)
"$BT/zipalign.exe" -f 4 "$W/unsigned.apk" "$W/aligned.apk"
cp "$HERE/../probe-app/probe.keystore" "$W/key.keystore"
"$BT/apksigner.bat" sign --ks "$W/key.keystore" --ks-pass pass:omniprobe --key-pass pass:omniprobe --out "$W/su-probe.apk" "$W/aligned.apk"
cp "$W/su-probe.apk" "$OUTAPK"
rm -rf "$W"
sha256sum "$OUTAPK"
