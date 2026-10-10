# Run any APK on the real-AOSP path (real Android boots, the APK is installed with `pm install` and
# started from its launcher Activity) in the live window: the display shown as it is drawn, the
# window's size the display's (drag the border: Android and the app relayout at that size), the
# window's keyboard and mouse the device's (click in the window to hold the mouse, Right Ctrl to
# give it back). Windows' spelling of `omnidroid aosp`; the arguments after this script's own go
# to it (a plugin's options too: `omnidroid help` lists them).
#
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Apk C:\apks\app.apk
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Apk app.apk -Minutes 60 -Size 1600x900
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Apk app.apk -NotResizable          # the app's own resizeableActivity
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Apk app.apk -WithSystemUI          # the device with SystemUI and its launcher (~1.1 GB more)
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Apk app.apk -WithSystemUI -ShowChrome  # ... and the whole display, bars and taskbar included
#
# The device is a single-app one: no SystemUI and no launcher in its image, so no bars, no
# taskbar, and the app edge to edge; apps it has no use for are not in its image either, and
# Android keeps no background app process. -WithSystemUI keeps SystemUI and the launcher (the
# composer then leaves their bars out of the window; the app keeps clear of them unless it hides
# them, as a game in play does).
#
# The session ends after -Minutes (default 30). Closing the window ends the window, not the session.
# The whole log is in %TEMP%\omni-linux-r-<pid>.log; screenshots of the display in
# %TEMP%\omni-linux-r-<pid>-shots. Needs ~8 GB of free commit (close big games first).
param(
    [string]$Apk = "",
    [int]$Minutes = 30,
    [string]$Size = "",
    [switch]$NotResizable,
    [switch]$WithSystemUI,
    [switch]$ShowChrome,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest = @()
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$arguments = @("aosp", "--minutes", [string]$Minutes)
if ($Apk) { $arguments += @("--apk", (Resolve-Path $Apk).Path) }
if ($Size) { $arguments += @("--size", $Size) }
if ($WithSystemUI) { $arguments += "--with-systemui" }
if ($NotResizable) { $env:OMNI_R_RESIZABLE = "0" }
if ($ShowChrome) { $env:OMNI_APP_ONLY = "0" }
# The build directory the vendored CPU backend needs (a short path: MAX_PATH).
if (-not $env:OMNIDROID_DYNARMIC_BUILD_DIR) { $env:OMNIDROID_DYNARMIC_BUILD_DIR = "C:\od-unified" }
Push-Location $repo
try {
    cargo run --release -q -p omnidroid -- @arguments @Rest
} finally {
    Pop-Location
}
