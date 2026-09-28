#!/bin/bash
# Build probe.apk: the SDK's build-tools 36.0.0 and platform android-36, JDK 21.
set -e
cd "$(dirname "$0")"
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
rm -rf out && mkdir -p out/classes out/dex
"$BT/aapt2.exe" link -I "$JAR" --manifest AndroidManifest.xml --min-sdk-version 26 --target-sdk-version 35 -o out/unsigned.apk
javac --release 8 -cp "$JAR" -d out/classes src/com/omnidroid/probe/MainActivity.java
"$BT/d8.bat" --min-api 26 --lib "$JAR" --output out/dex $(find out/classes -name "*.class")
(cd out/dex && jar uf ../unsigned.apk classes.dex)
"$BT/zipalign.exe" -f 4 out/unsigned.apk out/aligned.apk
[ -f probe.keystore ] || keytool -genkeypair -keystore probe.keystore -storepass omniprobe -keypass omniprobe -alias probe -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=omnidroid probe"
"$BT/apksigner.bat" sign --ks probe.keystore --ks-pass pass:omniprobe --key-pass pass:omniprobe --out probe.apk out/aligned.apk
rm -rf out probe.apk.idsig
sha256sum probe.apk
