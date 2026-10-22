#!/bin/bash
# The bundle fixtures (`tests/bundle.rs`): com.omnidroid.splittest -- a base APK running the probe
# app's Activity, and three config splits (arm64_v8a, x86, xxhdpi) -- signed with the probe's key
# and packed as splittest.xapk (with an OBB file and manifest.json), splittest.apkm and
# splittest.apks; and two-apps.zip, holding it and the probe (two packages). The SDK's build-tools
# 36.0.0 and platform android-36, JDK 21.
set -e
cd "$(dirname "$0")"
REPO=$(cd ../../../../.. && pwd)
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
BT=$SDK/build-tools/36.0.0
JAR=$SDK/platforms/android-36/android.jar
PROBE=$REPO/crates/omni-linux/tests/fixtures/probe-app
KS="--ks $PROBE/probe.keystore --ks-pass pass:omniprobe --key-pass pass:omniprobe"
rm -rf out && mkdir -p out/classes out/dex out/apks out/xapk/Android/obb/com.omnidroid.splittest out/two
cat > out/base.xml <<'X'
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="com.omnidroid.splittest" android:versionCode="7">
    <application android:label="Split Test">
        <activity android:name="com.omnidroid.probe.MainActivity" android:exported="true">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
X
"$BT/aapt2.exe" link -I "$JAR" --manifest out/base.xml --min-sdk-version 26 --target-sdk-version 35 -o out/base-unsigned.apk
javac --release 8 -cp "$JAR" -d out/classes $PROBE/src/com/omnidroid/probe/MainActivity.java
"$BT/d8.bat" --min-api 26 --lib "$JAR" --output out/dex $(find out/classes -name "*.class")
(cd out/dex && jar uf ../base-unsigned.apk classes.dex)
sign() { "$BT/zipalign.exe" -f 4 "$1" "$1.aligned"; "$BT/apksigner.bat" sign $KS --out "$2" "$1.aligned"; rm -f "$2.idsig"; }
sign out/base-unsigned.apk out/apks/base.apk
for split in config.arm64_v8a config.x86 config.xxhdpi; do
cat > out/$split.xml <<X
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="com.omnidroid.splittest" android:versionCode="7" split="$split">
    <application android:hasCode="false" />
</manifest>
X
"$BT/aapt2.exe" link -I "$JAR" --manifest out/$split.xml --min-sdk-version 26 --target-sdk-version 35 -o out/$split-unsigned.apk
sign out/$split-unsigned.apk out/apks/split_$split.apk
done
head -c 4096 /dev/urandom > out/xapk/Android/obb/com.omnidroid.splittest/main.7.com.omnidroid.splittest.obb
echo '{"package_name":"com.omnidroid.splittest","version_code":"7"}' > out/xapk/manifest.json
cp out/apks/*.apk out/xapk/
cp out/apks/*.apk out/two/ && cp $PROBE/probe.apk out/two/probe.apk
rm -f splittest.xapk splittest.apkm splittest.apks two-apps.zip
(cd out/xapk && jar cfM ../../splittest.xapk .)
(cd out/apks && jar cfM ../../splittest.apkm . && jar cfM ../../splittest.apks .)
(cd out/two && jar cfM ../../two-apps.zip .)
rm -rf out
