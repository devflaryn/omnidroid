# Build the single Windows omnidroid.exe that wraps the manager CLI.
# The exe is self-contained (Python bundled) but does NOT bundle QEMU —
# `omnidroid.exe setup` (or first use) downloads a PORTABLE QEMU into
# ./qemu next to the exe. Nothing is ever installed to the host system.
# Ship omnidroid.exe next to configs/ ; accounts/, qemu/, work/ are
# created beside it at runtime.
#
# NOTE (two-build process): PyInstaller CANNOT cross-build. This script
# produces the WINDOWS artifact only; the Linux `omnidroid` binary is
# built ON the Linux box with ./build-linux.sh. Same source, same CLI.
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Push-Location $root
py -3 -m PyInstaller --onefile --name omnidroid `
    --distpath "$root\dist" --workpath "$root\build\pyi" `
    --specpath "$root\build" `
    "$root\manager\omni.py"
Pop-Location
Write-Output "BUILT: $root\dist\omnidroid.exe"
