# window_shot.ps1 -Title <window title> -Out <png> [-Process omni-linux-run]
#
# A screenshot of one window's client area as the desktop compositor holds it (PrintWindow with
# PW_RENDERFULLCONTENT, so a covered window is captured whole), in physical pixels (the script
# declares per-monitor DPI awareness first). Windows only: a test tool, beside the runtime, for the
# live display window (`omni_linux::display_window`). Prints "<out> <width>x<height>".
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$Process = "omni-linux-run"
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class OmniShot {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    public delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc proc, IntPtr lparam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, System.Text.StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
}
"@
# DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4: coordinates in physical pixels.
[void][OmniShot]::SetProcessDpiAwarenessContext([IntPtr](-4))
$pids = @(Get-Process -Name $Process -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
$script:found = [IntPtr]::Zero
$callback = [OmniShot+EnumProc]{
    param($hwnd, $lparam)
    if (-not [OmniShot]::IsWindowVisible($hwnd)) { return $true }
    $text = New-Object System.Text.StringBuilder 256
    [void][OmniShot]::GetWindowText($hwnd, $text, 256)
    if ($text.ToString() -ne $Title) { return $true }
    $procId = [uint32]0
    [void][OmniShot]::GetWindowThreadProcessId($hwnd, [ref]$procId)
    if ($pids.Count -gt 0 -and -not ($pids -contains [int]$procId)) { return $true }
    $script:found = $hwnd
    return $false
}
[void][OmniShot]::EnumWindows($callback, [IntPtr]::Zero)
if ($script:found -eq [IntPtr]::Zero) { Write-Error "no visible window titled '$Title' of process $Process" }
$hwnd = $script:found
$outer = New-Object OmniShot+RECT
[void][OmniShot]::GetWindowRect($hwnd, [ref]$outer)
$client = New-Object OmniShot+RECT
[void][OmniShot]::GetClientRect($hwnd, [ref]$client)
$origin = New-Object OmniShot+POINT
[void][OmniShot]::ClientToScreen($hwnd, [ref]$origin)
$ow = $outer.Right - $outer.Left; $oh = $outer.Bottom - $outer.Top
$whole = New-Object System.Drawing.Bitmap $ow, $oh
$g = [System.Drawing.Graphics]::FromImage($whole)
$hdc = $g.GetHdc()
# PW_RENDERFULLCONTENT = 2
$ok = [OmniShot]::PrintWindow($hwnd, $hdc, 2)
$g.ReleaseHdc($hdc); $g.Dispose()
if (-not $ok) { Write-Error "PrintWindow failed" }
$cw = $client.Right; $ch = $client.Bottom
$area = New-Object System.Drawing.Rectangle ($origin.X - $outer.Left), ($origin.Y - $outer.Top), $cw, $ch
$shot = $whole.Clone($area, $whole.PixelFormat)
$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
$shot.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$shot.Dispose(); $whole.Dispose()
Write-Output "$Out ${cw}x${ch}"
