# Run the Roblox APK in a real window for a person to use: the gate's own run, with a long session
# and the host's GPU.
#
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1              # 30 minutes, fresh install
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Minutes 90
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Keep        # keep the app's storage
#
# **Every run is a fresh install by default.** With -Keep the app's storage is kept in -DataDir
# (default %LOCALAPPDATA%\Omnidroid\data), a signed-in session included -- but **a second launch of
# a kept directory does not start yet**: the engine judges the previous session a crash (it is
# handed no exit reason) and its crash report dies on a reporter this runtime does not set up. See
# docs/HANDOFF.md. Until that is fixed, sign in within the run you play in.
#
# **Signing in** needs the account owner. The route that needs no password typed here is the app's
# own Quick Sign-in: click Sign In, then Quick Sign-in, and enter the code it shows on a device that
# is already signed in (Roblox app: More > Quick Sign In). A username and password typed into the
# window also reach the app's fields.
#
# Closing the window ends the session: the app is then closed as a device closes it.
param(
    [int]$Minutes = 30,
    [switch]$Keep,
    [string]$DataDir = (Join-Path $env:LOCALAPPDATA "Omnidroid\data")
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$env:OMNI_M6_ROWS_21_22 = "1"
$env:OMNI_GFX_WINDOW_TESTS = "1"
$env:OMNI_SESSION_SECONDS = [string]($Minutes * 60)
if ($Keep) {
    New-Item -ItemType Directory -Force $DataDir | Out-Null
    $env:OMNI_DATA_DIR = $DataDir
    Write-Host "Omnidroid: a $Minutes-minute session; the app's storage is kept in $DataDir"
} else {
    Remove-Item Env:\OMNI_DATA_DIR -ErrorAction SilentlyContinue
    Write-Host "Omnidroid: a $Minutes-minute session on a fresh install"
}
Write-Host "Sign in with Quick Sign-in (Sign In > Quick Sign-in), then enter the code on a signed-in device."
Push-Location $repo
try {
    cargo test -p omni-android --release --test gameactivity -- --nocapture --test-threads=1 `
        initialize_native_code_returns_a_native_code_and_the_game_thread_starts
} finally {
    Pop-Location
}
