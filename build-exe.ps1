# Build a single Windows omni.exe that wraps the manager CLI.
# The exe is self-contained (Python bundled) but does NOT bundle QEMU —
# QEMU is auto-downloaded into ./qemu on first use (see ensure_qemu).
# Ship omni.exe next to configs/ ; accounts/, qemu/, work/ are created
# beside it at runtime.
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Push-Location $root
py -3 -m PyInstaller --onefile --name omni `
    --distpath "$root\dist" --workpath "$root\build\pyi" `
    --specpath "$root\build" `
    "$root\manager\omni.py"
Pop-Location
Write-Output "BUILT: $root\dist\omni.exe"
