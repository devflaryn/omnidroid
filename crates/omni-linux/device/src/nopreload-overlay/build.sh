#!/bin/bash
# Build ../../vendor/overlay/omni-nopreload-overlay.apk, as ../overlay/build.sh builds the device's
# overlay (the same tools, signed with its keystore). ../../SHA256SUMS pins the result.
set -e
cd "$(dirname "$0")"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
W="$(mktemp -d)"  # the SDK's .bat tools split paths at spaces
cp -r AndroidManifest.xml res "$W/"
cp ../overlay/overlay.keystore "$W/"
(cd "$W" && "$BT/aapt2.exe" compile --dir res -o res.zip \
  && "$BT/aapt2.exe" link -I "$JAR" --manifest AndroidManifest.xml --min-sdk-version 35 --target-sdk-version 35 -o unsigned.apk res.zip \
  && "$BT/zipalign.exe" -f 4 unsigned.apk aligned.apk \
  && "$BT/apksigner.bat" sign --ks overlay.keystore --ks-pass pass:omnidevice --key-pass pass:omnidevice --out overlay.apk aligned.apk)
mkdir -p ../../vendor/overlay
cp "$W/overlay.apk" ../../vendor/overlay/omni-nopreload-overlay.apk
rm -rf "$W"
(cd ../.. && sha256sum vendor/overlay/omni-nopreload-overlay.apk)
