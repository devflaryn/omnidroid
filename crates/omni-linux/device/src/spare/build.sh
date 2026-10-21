#!/bin/bash
# Build ../../vendor/framework/omni-spare.jar (a dex jar) on Windows, macOS or Linux: a JDK's javac
# that takes `--release 8` (`$JAVA_HOME/bin` first when set; 21 on Windows, Homebrew's 27 on the
# Mac) and the SDK build-tools' d8 (36.0.0 under `$ANDROID_SDK`, else the `d8` on PATH: Homebrew's
# android-commandlinetools 35.0.0 on the Mac, 2026-10-20). The classes use java.* alone --
# Android's own by reflection -- so no android.jar. ../../SHA256SUMS pins the result.
set -e
cd "$(dirname "$0")"
if [ -n "$JAVA_HOME" ]; then
    PATH="$JAVA_HOME/bin:$PATH"  # d8's launcher runs the `java` on PATH
fi
SDK=${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}
D8=$SDK/build-tools/36.0.0/d8
if [ -e "$D8.bat" ]; then
    D8=$D8.bat
elif [ ! -e "$D8" ]; then
    D8=$(command -v d8)
fi
W="$(mktemp -d)"  # the SDK's .bat tools split paths at spaces
mkdir -p "$W/classes" "$W/dex"
javac --release 8 -Xlint:-options -d "$W/classes" com/omnidroid/spare/*.java
"$D8" --min-api 26 --output "$W/dex" "$W"/classes/com/omnidroid/spare/*.class
(cd "$W/dex" && jar cf ../omni-spare.jar classes.dex)
mkdir -p ../../vendor/framework
cp "$W/omni-spare.jar" ../../vendor/framework/omni-spare.jar
rm -rf "$W"
if command -v sha256sum >/dev/null; then
    (cd ../.. && sha256sum -b vendor/framework/omni-spare.jar)
else
    (cd ../.. && shasum -a 256 -b vendor/framework/omni-spare.jar)
fi
