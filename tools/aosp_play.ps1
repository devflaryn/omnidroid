# Run an APK on the real-AOSP path (omni-linux: real Android boots, the app installed with
# `pm install` and started from its launcher) in the live window: the display shown as it is drawn,
# the window's size the display's (drag the border: Android and the app relayout at that size), the
# window's keyboard and mouse the device's (click in the window to hold the mouse, Right Ctrl to
# give it back). Windows' launcher for `tests/r_roblox.rs`, which boots and runs the session.
#
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1                         # the stock APK, logged out
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Cookie cookie.txt      # signed in (the app's own cookie store)
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Cookie cookie.txt -Place 8737899170
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -Minutes 60 -Size 1600x900
#   powershell -ExecutionPolicy Bypass -File tools\aosp_play.ps1 -NotResizable          # the app's own resizeableActivity
#
# The session ends after -Minutes (default 30). Closing the window ends the window, not the session.
# The whole log is in %TEMP%\omni-linux-r-<pid>.log; screenshots of the display in
# %TEMP%\omni-linux-r-<pid>-shots. Needs ~13 GB of free commit (close big games first).
param(
    [string]$Apk = "",
    [string]$Cookie = "",
    [long]$Place = 0,
    [int]$Minutes = 30,
    [string]$Size = "",
    [switch]$NotResizable
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$env:OMNI_WINDOW = "1"
$env:OMNI_R_MINUTES = [string]$Minutes
$env:OMNI_TEST_APK = if ($Apk) { (Resolve-Path $Apk).Path } else { Join-Path $repo "Roblox-2.738.1397.apk" }
if ($Cookie) { $env:OMNI_R_COOKIE = (Resolve-Path $Cookie).Path }
if ($Place -gt 0) { $env:OMNI_R_PLACE = [string]$Place }
if ($Size) { $env:OMNI_WINDOW_SIZE = $Size }
if ($NotResizable) { $env:OMNI_R_RESIZABLE = "0" }
# The build directory the vendored CPU backend needs (a short path: MAX_PATH).
if (-not $env:OMNIDROID_DYNARMIC_BUILD_DIR) { $env:OMNIDROID_DYNARMIC_BUILD_DIR = "C:\od-unified" }
Get-ChildItem $env:TEMP -Filter "omni-shm-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
Push-Location $repo
try {
    cargo test --release -q -p omni-linux --test r_roblox -- --ignored --nocapture
} finally {
    Pop-Location
}
