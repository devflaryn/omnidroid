# Run the Roblox APK in a real window for a person to use: the gate's own run, with a long session,
# the host's GPU, and the app's storage kept between runs.
#
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1              # 30 minutes
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Minutes 90
#
# **The storage is kept** in -DataDir (default %LOCALAPPDATA%\Omnidroid\data): whatever the app
# writes stays there -- a signed-in session included, which is the point. Delete the directory to
# start again as a fresh install. Nothing here reads or writes the session; the engine does.
#
# **Signing in** needs the account owner. The route that needs no password typed here is the app's
# own Quick Sign-in: click Sign In, then Quick Sign-in, and enter the code it shows on a device that
# is already signed in (Roblox app: More > Quick Sign In). A username and password typed into the
# window also reach the app's fields.
#
# Closing the window ends the session: the app is then closed as a device closes it.
param(
    [int]$Minutes = 30,
    [string]$DataDir = (Join-Path $env:LOCALAPPDATA "Omnidroid\data")
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
New-Item -ItemType Directory -Force $DataDir | Out-Null
Write-Host "Omnidroid: a $Minutes-minute session; the app's storage is kept in $DataDir"
Write-Host "Sign in with Quick Sign-in (Sign In > Quick Sign-in), then enter the code on a signed-in device."
$env:OMNI_M6_ROWS_21_22 = "1"
$env:OMNI_GFX_WINDOW_TESTS = "1"
$env:OMNI_SESSION_SECONDS = [string]($Minutes * 60)
$env:OMNI_DATA_DIR = $DataDir
Push-Location $repo
try {
    cargo test -p omni-android --release --test gameactivity -- --nocapture --test-threads=1 `
        initialize_native_code_returns_a_native_code_and_the_game_thread_starts
} finally {
    Pop-Location
}
