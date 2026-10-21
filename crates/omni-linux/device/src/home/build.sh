#!/bin/bash
# Build ../../vendor/app/OmniHome/OmniHome.apk: the kiosk device's home (OmniKioskHome.java says
# why). Needs a JDK (javac, jar, keytool; `$JAVA_HOME/bin` first when set) and the SDK's
# build-tools 36.0.0 and platform android-36 (`$ANDROID_SDK`, else `%LOCALAPPDATA%/Android/Sdk`),
# as ../spare/build.sh and ../overlay/build.sh do. Signed with home.keystore (a system app needs a
# signature, not a particular one). ../../SHA256SUMS pins the result.
set -e
cd "$(dirname "$0")"
if [ -n "$JAVA_HOME" ]; then
    PATH="$JAVA_HOME/bin:$PATH"
fi
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
tool() { if [ -e "$BT/$1$2" ]; then echo "$BT/$1$2"; elif [ -e "$BT/$1" ]; then echo "$BT/$1"; else command -v "$1"; fi; }
AAPT2=${AAPT2:-$(tool aapt2 .exe)}
ZIPALIGN=${ZIPALIGN:-$(tool zipalign .exe)}
APKSIGNER=${APKSIGNER:-$(tool apksigner .bat)}
D8=${D8:-$(tool d8 .bat)}
FRAMEWORK=${FRAMEWORK:-$SDK/platforms/android-36/android.jar}
W="$(mktemp -d)"  # the SDK's .bat tools split paths at spaces
cp AndroidManifest.xml "$W/"
cp "$FRAMEWORK" "$W/framework.jar"
[ -f home.keystore ] || keytool -genkeypair -keystore home.keystore -storepass omnihome -keypass omnihome -alias home -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=omnidroid kiosk home"
cp home.keystore "$W/"
mkdir -p "$W/classes" "$W/dex"
javac --release 8 -Xlint:-options -cp "$W/framework.jar" -d "$W/classes" com/omnidroid/home/*.java
(cd "$W" && "$D8" --release --min-api 35 --lib framework.jar --output dex classes/com/omnidroid/home/*.class \
  && "$AAPT2" link -I framework.jar --manifest AndroidManifest.xml --min-sdk-version 35 --target-sdk-version 35 -o unsigned.apk \
  && (cd dex && jar uf ../unsigned.apk classes.dex) \
  && "$ZIPALIGN" -f 4 unsigned.apk aligned.apk \
  && "$APKSIGNER" sign --ks home.keystore --ks-pass pass:omnihome --key-pass pass:omnihome --out home.apk aligned.apk)
mkdir -p ../../vendor/app/OmniHome
cp "$W/home.apk" ../../vendor/app/OmniHome/OmniHome.apk
rm -rf "$W"
(cd ../.. && sha256sum -b vendor/app/OmniHome/OmniHome.apk 2>/dev/null || shasum -a 256 -b vendor/app/OmniHome/OmniHome.apk)
