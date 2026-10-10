# Run an APK on the fast in-process path (`omnidroid play`: no Java runtime, so only the apps whose
# Java side the engine answers itself; any APK runs with tools\aosp_play.ps1). Windows' spelling of
# the launcher, which is now the `omnidroid` binary (crates/omnidroid) -- the same run on Windows,
# macOS and Linux. Arguments after this script's own go to it (a plugin's options too).
#
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1              # the newest APK in the repository root
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Apk C:\apks\game.apk
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Minutes 90
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Fresh       # a fresh install
#   powershell -ExecutionPolicy Bypass -File tools\play.ps1 -Phone       # the mouse is a finger, no keyboard
#
# **The APK is chosen, not built in**: -Apk, else OMNI_APK, else the newest APK (by versionCode) in
# the repository root. Its version comes from its own manifest. See crates/omnidroid/src/main.rs for
# storage (-DataDir, default %LOCALAPPDATA%\Omnidroid\data) and ending a run.
param(
    [string]$Apk = "",
    [int]$Minutes = 0,
    [switch]$Fresh,
    [switch]$Phone,
    [string]$DataDir = "",
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest = @()
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
    cargo run --release -q -p omnidroid -- @arguments @Rest
} finally {
    Pop-Location
}
