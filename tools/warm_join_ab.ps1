# Interleaved A/B of `omnidroid aosp --cookie --place` (tools/join_timer.py, to PS99's own loading
# screen): "saved" = no warm device up, the command boots its saved signed-in device (the path
# before the warm-device session); "warm" = a warm device booted and idle, nothing of the app on it.
# Pairs in ABBA order (saved,warm / warm,saved / ...), so a drift over the hour falls on both.
#
#   powershell -File tools/warm_join_ab.ps1 [-Pairs 4] [-Label ab] [-Kinds saved,warm] [-WarmEnv NAME=VALUE]
#
# Kinds: saved, warm, warm-env (a warm device booted with -WarmEnv set: a device-wide lever A/B'd
# against plain warm devices, `-Kinds warm,warm-env`).
#
# Measured with it, 2026-10-02: saved vs warm (below, the warm session's commit); warm vs a device
# that held a 1 ms host timer resolution and opted out of Windows' power throttling in every host
# process: 77.3 vs 72.1 s median, pairs both ways -- not kept.
param(
    [int]$Pairs = 4,
    [string]$Label = "ab",
    [string[]]$Kinds = @("saved", "warm"),
    [string]$WarmEnv = "",
    [string]$Apk = "$HOME\Desktop\Roblox-2.740.931.apk",
    [string]$Cookie = "$HOME\Desktop\cookies\HeZmI_ImYu1080.txt",
    [string]$Place = "8737899170",
    [string]$Exe = "target\release\omnidroid.exe"
)
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture
if ($Kinds.Count -eq 1) { $Kinds = $Kinds[0] -split "," } # -File passes "a,b" as one string
$env:OMNIDROID_DYNARMIC_BUILD_DIR = if ($env:OMNIDROID_DYNARMIC_BUILD_DIR) { $env:OMNIDROID_DYNARMIC_BUILD_DIR } else { "C:\od-unified" }

function Stop-Warm {
    Get-ChildItem $env:TEMP -Directory -Filter "omni-warm-*" | Where-Object { $_.Name -match '^omni-warm-\d+$' } | ForEach-Object {
        $stop = Join-Path $_.FullName "data\local\tmp"
        if (Test-Path $stop) { Set-Content -Path (Join-Path $stop "stop") -Value "1" }
    }
    $i = 0
    while ((Get-Process omni-linux-run, r_roblox* -ErrorAction SilentlyContinue) -and $i -lt 120) { Start-Sleep 1; $i++ }
}

function Start-Warm([bool]$WithEnv) {
    Stop-Warm
    $secs = [int][double]::Parse((Get-Date -UFormat %s))
    $dir = "$env:TEMP\omni-warm-$secs"
    $name, $value = $WarmEnv -split "=", 2
    if ($WithEnv -and $name) { Set-Item "Env:$name" $value }
    Start-Process -FilePath $Exe -ArgumentList @("aosp", "--warm", "--instance", $dir, "--gpu", "auto", "--minutes", "720") -WindowStyle Hidden
    if ($WithEnv -and $name) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
    $t = Get-Date
    while (-not (Test-Path "$dir\data\local\tmp\warm-ready") -and ((Get-Date) - $t).TotalSeconds -lt 300) { Start-Sleep 2 }
    Start-Sleep 20 # idle, its spare app process up, as a warm device waits
    return $dir
}

function Run-One([string]$kind, [string]$name) {
    if ($kind -like "warm*") { $null = Start-Warm ($kind -eq "warm-env") } else { Stop-Warm }
    $lines = python tools/join_timer.py --label $name --stop --limit 480 -- $Exe aosp --apk $Apk --cookie $Cookie --place $Place 2>&1
    $joining = $lines | Where-Object { "$_" -match "\+\s*([0-9.]+)s Joining game" } | Select-Object -First 1
    $j = if ("$joining" -match "\+\s*([0-9.]+)s Joining") { [double]$Matches[1] } else { [double]::NaN }
    $hit = $lines | Where-Object { "$_" -match "result: in-game loading screen at ([0-9.]+)" } | Select-Object -First 1
    $s = if ("$hit" -match "at ([0-9.]+) s") { [double]$Matches[1] } else { [double]::NaN }
    if ($kind -like "warm*") { Stop-Warm } else { Start-Sleep 5 }
    "{0,-6} {1,-12} joining {2,6:0.0} s  in-game screen {3,6:0.0} s" -f $kind, $name, $j, $s
    return [pscustomobject]@{ kind = $kind; joining = $j; screen = $s }
}

$all = @()
for ($p = 1; $p -le $Pairs; $p++) {
    $order = if ($p % 2 -eq 1) { @($Kinds[0], $Kinds[1]) } else { @($Kinds[1], $Kinds[0]) }
    foreach ($k in $order) {
        $r = Run-One $k "$Label-$p-$k"
        $r[0]
        $all += $r[1]
    }
}
foreach ($k in $Kinds) {
    $rs = $all | Where-Object { $_.kind -eq $k }
    $med = { param($v) $s = $v | Where-Object { -not [double]::IsNaN($_) } | Sort-Object; if ($s.Count) { $s[[int][Math]::Floor(($s.Count - 1) / 2)] } else { [double]::NaN } }
    "{0,-6} joining {1}  | screen {2}  | median joining {3:0.0} s, screen {4:0.0} s" -f $k, (($rs.joining | ForEach-Object { $_.ToString("0.0") }) -join " "), (($rs.screen | ForEach-Object { $_.ToString("0.0") }) -join " "), (& $med $rs.joining), (& $med $rs.screen)
}
