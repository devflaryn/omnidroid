# Build the Omni Kiosk APK without Gradle: aapt2 -> javac -> d8 -> sign.
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$sdk = "$env:LOCALAPPDATA\Android\Sdk"
$bt = "$sdk\build-tools\36.0.0"
$androidJar = "$sdk\platforms\android-36\android.jar"
$out = "$root\build"

Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$out\classes" | Out-Null

# 1. Link manifest (no resources needed - UI is programmatic)
& "$bt\aapt2.exe" link --manifest "$root\AndroidManifest.xml" `
    -I $androidJar --min-sdk-version 26 --target-sdk-version 33 `
    -o "$out\base.apk"
if ($LASTEXITCODE -ne 0) { throw "aapt2 link failed" }

# 2. Compile Java (release 11 bytecode; d8 desugars for minSdk 26)
javac --release 11 -cp $androidJar -d "$out\classes" `
    (Get-ChildItem "$root\src" -Recurse -Filter *.java | ForEach-Object FullName)
if ($LASTEXITCODE -ne 0) { throw "javac failed" }

# 3. Dex
$classFiles = Get-ChildItem "$out\classes" -Recurse -Filter *.class | ForEach-Object FullName
& "$bt\d8.bat" --release --lib $androidJar --min-api 26 `
    --output $out @classFiles
if ($LASTEXITCODE -ne 0) { throw "d8 failed" }

# 4. Add classes.dex into the APK (jar adds at archive root)
Push-Location $out
jar uf base.apk classes.dex
Pop-Location
if ($LASTEXITCODE -ne 0) { throw "jar update failed" }

# 5. Align + sign (debug keystore, generated once)
$ks = "$root\omni-debug.jks"
if (-not (Test-Path $ks)) {
    keytool -genkeypair -keystore $ks -alias omni -keyalg RSA -keysize 2048 `
        -validity 10000 -storepass omnidroid -keypass omnidroid `
        -dname "CN=OmniKiosk" | Out-Null
}
& "$bt\zipalign.exe" -f 4 "$out\base.apk" "$out\omni-kiosk-unsigned.apk"
if ($LASTEXITCODE -ne 0) { throw "zipalign failed" }
& "$bt\apksigner.bat" sign --ks $ks --ks-pass pass:omnidroid `
    --key-pass pass:omnidroid --out "$out\omni-kiosk.apk" `
    "$out\omni-kiosk-unsigned.apk"
if ($LASTEXITCODE -ne 0) { throw "apksigner failed" }

Write-Output "BUILT: $out\omni-kiosk.apk ($((Get-Item "$out\omni-kiosk.apk").Length) bytes)"
