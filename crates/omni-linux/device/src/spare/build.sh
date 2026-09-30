#!/bin/bash
# Build ../../vendor/framework/omni-spare.jar (a dex jar): the SDK's build-tools 36.0.0, platform
# android-36, JDK 21. ../../SHA256SUMS pins the result.
set -e
cd "$(dirname "$0")"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
W="$(mktemp -d)"  # the SDK's .bat tools split paths at spaces
mkdir -p "$W/classes" "$W/dex"
javac --release 8 -cp "$JAR" -d "$W/classes" com/omnidroid/spare/Spare.java 2>/dev/null
"$BT/d8.bat" --min-api 26 --lib "$JAR" --output "$W/dex" "$W/classes/com/omnidroid/spare/Spare.class"
(cd "$W/dex" && jar cf ../omni-spare.jar classes.dex)
mkdir -p ../../vendor/framework
cp "$W/omni-spare.jar" ../../vendor/framework/omni-spare.jar
rm -rf "$W"
(cd ../.. && sha256sum vendor/framework/omni-spare.jar)
