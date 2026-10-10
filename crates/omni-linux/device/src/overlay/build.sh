#!/bin/bash
# Build ../../vendor/overlay/omni-device-overlay.apk: aapt2, zipalign and apksigner (the SDK's
# build-tools, or ones on PATH), a JDK for apksigner, and the framework to link against
# (platform android-36's android.jar, or the device's own framework-res.apk as FRAMEWORK).
# Signed with overlay.keystore (a preinstalled overlay needs a signature, not a particular one).
# ../../SHA256SUMS pins the result.
set -e
cd "$(dirname "$0")"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
tool() { if [ -x "$BT/$1$2" ]; then echo "$BT/$1$2"; else command -v "$1"; fi; }
AAPT2=${AAPT2:-$(tool aapt2 .exe)}
ZIPALIGN=${ZIPALIGN:-$(tool zipalign .exe)}
APKSIGNER=${APKSIGNER:-$(tool apksigner .bat)}
FRAMEWORK=${FRAMEWORK:-$SDK/platforms/android-36/android.jar}
W="$(mktemp -d)"  # the SDK's .bat tools split paths at spaces
cp -r AndroidManifest.xml res "$W/"
cp "$FRAMEWORK" "$W/framework.jar"
[ -f overlay.keystore ] || keytool -genkeypair -keystore overlay.keystore -storepass omnidevice -keypass omnidevice -alias overlay -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=omnidroid device overlay"
cp overlay.keystore "$W/"
(cd "$W" && "$AAPT2" compile --dir res -o res.zip \
  && "$AAPT2" link -I framework.jar --manifest AndroidManifest.xml --min-sdk-version 35 --target-sdk-version 35 -o unsigned.apk res.zip \
  && "$ZIPALIGN" -f 4 unsigned.apk aligned.apk \
  && "$APKSIGNER" sign --ks overlay.keystore --ks-pass pass:omnidevice --key-pass pass:omnidevice --out overlay.apk aligned.apk)
mkdir -p ../../vendor/overlay
cp "$W/overlay.apk" ../../vendor/overlay/omni-device-overlay.apk
rm -rf "$W"
(cd ../.. && shasum -a 256 vendor/overlay/omni-device-overlay.apk 2>/dev/null || sha256sum vendor/overlay/omni-device-overlay.apk)
