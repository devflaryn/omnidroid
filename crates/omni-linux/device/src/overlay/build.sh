#!/bin/bash
# Build ../../vendor/overlay/omni-device-overlay.apk: the SDK's build-tools 36.0.0, platform
# android-36, JDK 21 (keytool). Signed with overlay.keystore (a preinstalled overlay needs a
# signature, not a particular one). ../../SHA256SUMS pins the result.
set -e
cd "$(dirname "$0")"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
W="$(mktemp -d)"  # the SDK's .bat tools split paths at spaces
cp -r AndroidManifest.xml res "$W/"
[ -f overlay.keystore ] || keytool -genkeypair -keystore overlay.keystore -storepass omnidevice -keypass omnidevice -alias overlay -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=omnidroid device overlay"
cp overlay.keystore "$W/"
(cd "$W" && "$BT/aapt2.exe" compile --dir res -o res.zip \
  && "$BT/aapt2.exe" link -I "$JAR" --manifest AndroidManifest.xml --min-sdk-version 35 --target-sdk-version 35 -o unsigned.apk res.zip \
  && "$BT/zipalign.exe" -f 4 unsigned.apk aligned.apk \
  && "$BT/apksigner.bat" sign --ks overlay.keystore --ks-pass pass:omnidevice --key-pass pass:omnidevice --out overlay.apk aligned.apk)
mkdir -p ../../vendor/overlay
cp "$W/overlay.apk" ../../vendor/overlay/omni-device-overlay.apk
rm -rf "$W"
(cd ../.. && sha256sum vendor/overlay/omni-device-overlay.apk)
