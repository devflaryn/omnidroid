# Run the Roblox APK in a real window for a person to use: the gate's own run, with a long session
# and the host's GPU.
#
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1              # until the window is closed, storage kept
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Minutes 90
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Fresh       # a fresh install
#
# **The app's storage is kept** in -DataDir (default %LOCALAPPDATA%\Omnidroid\data), a signed-in
# session included, so a sign-in made in one run is there for the next. -Fresh runs a fresh install
# instead, and leaves -DataDir as it is.
#
# **End a run by closing the window.** With -Minutes N it also ends after N minutes; the default, 0,
# runs until the window is closed. The app is then closed as a
# device closes it, and the next launch is told so. A run ended any other way -- the console closed, Ctrl+C, a crash -- is judged a
# crash by the engine at the next launch, whose crash report dies in this runtime (docs/HANDOFF.md);
# after one, run once with -Fresh, or empty -DataDir (which signs you out).
#
# **Signing in** needs the account owner. The route that needs no password typed here is the app's
# own Quick Sign-in: click Sign In, then Quick Sign-in, and enter the code it shows on a device that
# is already signed in (Roblox app: More > Quick Sign In). A username and password typed into the
# window also reach the app's fields.
#
# **The keyboard and the mouse are this computer's** (OMNI_KEYBOARD_MOUSE=1): the app is told it has
# a hardware keyboard, and the mouse is a mouse -- hover, both buttons, the wheel, and a captured
# pointer while the game locks the mouse (shift-lock, first person; the cursor disappears then, and
# comes back when the game lets go or the window loses the focus). So WASD, Space, Tab and Esc, the
# right-drag camera and the wheel zoom are the game's own, as on a device with a keyboard and mouse.
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Phone      # a touch screen instead: the
#                                                                        # mouse is a finger, no keyboard
param(
    [int]$Minutes = 0,
    [switch]$Fresh,
    [switch]$Phone,
    [string]$DataDir = (Join-Path $env:LOCALAPPDATA "Omnidroid\data")
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$env:OMNI_M6_ROWS_21_22 = "1"
$env:OMNI_GFX_WINDOW_TESTS = "1"
if ($Phone) {
    Remove-Item Env:\OMNI_KEYBOARD_MOUSE -ErrorAction SilentlyContinue
    Write-Host "Omnidroid: the phone configuration -- the mouse is a finger, and there is no keyboard"
} else {
    $env:OMNI_KEYBOARD_MOUSE = "1"
    Write-Host "Omnidroid: this computer's keyboard and mouse (OMNI_KEYBOARD_MOUSE=1)"
}
# 0: until the window is closed -- ten years is the gate's way of saying no limit.
$seconds = if ($Minutes -gt 0) { $Minutes * 60 } else { 315360000 }
$length = if ($Minutes -gt 0) { "a $Minutes-minute session" } else { "a session until the window is closed" }
$env:OMNI_SESSION_SECONDS = [string]$seconds
if ($Fresh) {
    Remove-Item Env:\OMNI_DATA_DIR -ErrorAction SilentlyContinue
    Write-Host "Omnidroid: $length on a fresh install"
} else {
    New-Item -ItemType Directory -Force $DataDir | Out-Null
    $env:OMNI_DATA_DIR = $DataDir
    Write-Host "Omnidroid: $length; the app's storage is kept in $DataDir"
}
Write-Host "Sign in with Quick Sign-in (Sign In > Quick Sign-in), then enter the code on a signed-in device."
Write-Host "End the session by closing the window."
Push-Location $repo
try {
    cargo test -p omni-android --release --test gameactivity -- --nocapture --test-threads=1 `
        initialize_native_code_returns_a_native_code_and_the_game_thread_starts
} finally {
    Pop-Location
}
