# Run an APK in a real window for a person to use. Windows' spelling of the launcher, which is now
# the `omnidroid` binary (crates/omnidroid) -- the same run on Windows, macOS and Linux.
#
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1              # the newest APK in the repository root
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Apk C:\apks\Roblox-2.740.1.apk
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Minutes 90
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Fresh       # a fresh install
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Phone       # the mouse is a finger, no keyboard
#
# **The APK is chosen, not built in**: -Apk, else OMNI_APK, else the newest APK (by versionCode) in
# the repository root. Its version comes from its own manifest. See crates/omnidroid/src/main.rs for
# storage (-DataDir, default %LOCALAPPDATA%\Omnidroid\data), ending a run, and signing in.
param(
    [string]$Apk = "",
    [int]$Minutes = 0,
    [switch]$Fresh,
    [switch]$Phone,
    [string]$DataDir = ""
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$arguments = @("play")
if ($Apk) { $arguments += @("--apk", (Resolve-Path $Apk).Path) }
if ($Minutes -gt 0) { $arguments += @("--minutes", [string]$Minutes) }
if ($Fresh) { $arguments += "--fresh" }
if ($Phone) { $arguments += "--phone" }
if ($DataDir) { $arguments += @("--data-dir", $DataDir) }
Push-Location $repo
try {
    cargo run --release -q -p omnidroid -- @arguments
} finally {
    Pop-Location
}
