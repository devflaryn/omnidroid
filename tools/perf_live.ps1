# Interleaved A/B inside ONE live in-world session (docs/NIGHT-2026-10-02.md): boot a prebuilt
# r_roblox test binary with OMNI_LEVER_FILE, join PS99, settle, then alternate a lever (see
# crates/omni-linux/src/lever.rs) between value A and value B for -Pairs pairs in ABBA order,
# each phase: write the lever, wait -WarmSec (translations redone, the switch settles), measure
# -MeasureSec. Per phase one CSV row; at the end the paired deltas (B - A) with their spread.
#
#   powershell -File tools\perf_live.ps1 -Exe <r_roblox-*.exe> -Lever jit_fp -A 0 -B f0000 -Tag fp-all
param(
  [Parameter(Mandatory)][string]$Exe,
  [Parameter(Mandatory)][string]$Lever,
  [Parameter(Mandatory)][string]$A,
  [Parameter(Mandatory)][string]$B,
  [string]$Tag = "live",
  [int]$Pairs = 8,
  [int]$WarmSec = 20,
  [int]$MeasureSec = 30,
  [int]$SettleSec = 60,
  [int]$JoinTimeoutMin = 14,
  [string]$Dir = "C:\od-unified\perf\live",
  [string]$Apk = "C:\Users\berat\Desktop\Roblox-2.740.931.apk",
  [string]$Cookie = "C:\Users\berat\Desktop\cookies\HeZmI_ImYu1080.txt",
  [string]$Place = "8737899170",
  [string]$Sysroot = "C:\Users\berat\Desktop\Omni Apps\omnidroid\sysroot\aosp-35",
  [string]$ExtraEnv = ""
)
$ErrorActionPreference = "Continue"
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$stamp = Get-Date -Format "MMdd-HHmmss"
$name = "$Tag-$stamp"
$csv = Join-Path $Dir "$name.csv"; $sum = Join-Path $Dir "$name.summary.txt"
$leverFile = Join-Path $Dir "$name.lever.txt"
Set-Content -Path $leverFile -Value "$Lever=$A" -NoNewline -Encoding ascii

$env:OMNI_WINDOW = "1"; $env:OMNI_R_KIOSK = "1"; $env:OMNI_GPU = "auto"
$env:OMNI_R_MINUTES = [string]($JoinTimeoutMin + 2 + [math]::Ceiling(($SettleSec + 2 * $Pairs * ($WarmSec + $MeasureSec)) / 60) + 2)
$env:OMNI_TEST_APK = $Apk; $env:OMNI_R_COOKIE = $Cookie; $env:OMNI_R_PLACE = $Place
$env:OMNI_SYSROOT = $Sysroot; $env:OMNI_LEVER_FILE = $leverFile
Remove-Item env:OMNI_SCREENSHOT -ErrorAction SilentlyContinue
if ($ExtraEnv) { foreach ($kv in $ExtraEnv.Split(";")) { if ($kv) { $p = $kv.Split("=", 2); Set-Item -Path ("env:" + $p[0]) -Value $p[1] } } }

function Stop-Guests { Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" -or $_.Name -like "r_roblox*" } | Stop-Process -Force -ErrorAction SilentlyContinue }
Stop-Guests; Start-Sleep 3
for ($i = 0; $i -lt 30; $i++) { if ((Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory -ge 13GB / 1KB) { break }; Start-Sleep 2 }
$tree = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $Exe)))
$proc = Start-Process -FilePath $Exe -ArgumentList "--ignored", "--nocapture", "--exact", "the_apk_is_installed_started_and_draws" `
  -WorkingDirectory $tree -RedirectStandardOutput (Join-Path $Dir "$name.out.log") -RedirectStandardError (Join-Path $Dir "$name.err.log") -PassThru -WindowStyle Hidden
$log = Join-Path $env:TEMP ("omni-linux-r-{0}.log" -f $proc.Id)
Add-Content $sum "run $name exe $Exe lever $Lever A=$A B=$B pairs $Pairs warm $WarmSec measure $MeasureSec log $log"

# The log, read as it grows: [display] lines stamped with their arrival time.
$script:pos = 0L; $script:disp = New-Object System.Collections.ArrayList; $script:marks = New-Object System.Collections.ArrayList
function Pump {
  if (-not (Test-Path $log)) { return }
  $fs = [IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
  try {
    if ($fs.Length -le $script:pos) { return }
    $fs.Seek($script:pos, 'Begin') | Out-Null
    $sr = New-Object IO.StreamReader($fs)
    $text = $sr.ReadToEnd(); $script:pos = $fs.Length
  } finally { $fs.Dispose() }
  $now = Get-Date
  foreach ($l in $text -split "`n") {
    if ($l -match "\[display\] (\d+) frames presented \(([\d.]+)/s\)") { [void]$script:disp.Add([pscustomobject]@{ t = $now; f = [double]$Matches[1]; r = [double]$Matches[2] }) }
    elseif ($l -match "Joining game") { [void]$script:marks.Add("join") }
    elseif ($l -match "Client has been disconnected") { [void]$script:marks.Add("kick") }
    elseif ($l -match "\[lever\]") { [void]$script:marks.Add($l.Trim()) }
  }
}
function Wait-Pumping($sec) { $end = (Get-Date).AddSeconds($sec); while ((Get-Date) -lt $end) { Start-Sleep -Milliseconds 1000; Pump } }
function Guests { @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" }) }
function Snap {
  $g = Guests
  $app = $g | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1
  $threads = @{}
  foreach ($t in $app.Threads) { try { $threads[$t.Id] = $t.TotalProcessorTime.TotalMilliseconds } catch {} }
  $all = 0.0; foreach ($p in $g) { try { $all += $p.TotalProcessorTime.TotalMilliseconds } catch {} }
  [pscustomobject]@{ t = Get-Date; appId = $app.Id; appCpu = $app.TotalProcessorTime.TotalMilliseconds; threads = $threads; all = $all
    priv = ($g | Measure-Object PrivateMemorySize64 -Sum).Sum; appPriv = $app.PrivateMemorySize64; n = $g.Count }
}

$t0 = Get-Date
while (((Get-Date) - $t0).TotalMinutes -lt $JoinTimeoutMin) { Wait-Pumping 5; if ($script:marks -contains "join" -or $proc.HasExited) { break } }
if (-not ($script:marks -contains "join")) { Add-Content $sum "NOJOIN"; Stop-Guests; Get-Content $sum; exit 1 }
Add-Content $sum ("joined after {0:N0} s" -f ((Get-Date) - $t0).TotalSeconds)
Wait-Pumping $SettleSec

Set-Content -Path $csv -Value "pair,arm,value,fps,top_ms,top2_ms,app_ms,all_ms,priv_gb,app_priv_gb,procs,lines" -Encoding ascii
$rows = New-Object System.Collections.ArrayList
for ($i = 0; $i -lt $Pairs; $i++) {
  $order = if ($i % 2 -eq 0) { @("A", "B") } else { @("B", "A") }
  foreach ($arm in $order) {
    $value = if ($arm -eq "A") { $A } else { $B }
    Set-Content -Path $leverFile -Value "$Lever=$value" -NoNewline -Encoding ascii
    Wait-Pumping $WarmSec
    $s0 = Snap; $d0 = $script:disp.Count
    Wait-Pumping $MeasureSec
    $s1 = Snap
    $lines = @($script:disp | Select-Object -Skip $d0)
    $frames = 0.0; $secs = 0.0
    foreach ($d in $lines) { if ($d.r -gt 0) { $idx = $script:disp.IndexOf($d); if ($idx -gt 0) { $df = $d.f - $script:disp[$idx - 1].f; $frames += $df; $secs += $df / $d.r } } }
    $fps = if ($secs -gt 0) { $frames / $secs } else { 0 }
    $wall = ($s1.t - $s0.t).TotalSeconds; $nf = [math]::Max(1, $fps * $wall)
    $deltas = foreach ($k in $s1.threads.Keys) { if ($s0.threads.ContainsKey($k)) { $s1.threads[$k] - $s0.threads[$k] } }
    $top = @($deltas | Sort-Object -Descending | Select-Object -First 2)
    $row = [pscustomobject]@{ pair = $i; arm = $arm; value = $value; fps = $fps; top_ms = $top[0] / $nf; top2_ms = $top[1] / $nf
      app_ms = ($s1.appCpu - $s0.appCpu) / $nf; all_ms = ($s1.all - $s0.all) / $nf; priv_gb = $s1.priv / 1GB; app_priv_gb = $s1.appPriv / 1GB; procs = $s1.n; lines = $lines.Count }
    if ($s0.appId -ne $s1.appId) { $row.fps = 0 }
    [void]$rows.Add($row)
    Add-Content $csv ("{0},{1},{2},{3:N3},{4:N3},{5:N3},{6:N3},{7:N3},{8:N3},{9:N3},{10},{11}" -f $row.pair, $row.arm, $row.value, $row.fps, $row.top_ms, $row.top2_ms, $row.app_ms, $row.all_ms, $row.priv_gb, $row.app_priv_gb, $row.procs, $row.lines)
  }
}
Pump
Stop-Guests
function Med($a) { $s = @($a | Sort-Object); if ($s.Count -eq 0) { return 0 }; if ($s.Count % 2) { $s[[int][math]::Floor($s.Count / 2)] } else { ($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2 } }
foreach ($m in "fps", "top_ms", "top2_ms", "app_ms", "all_ms", "priv_gb", "app_priv_gb") {
  $d = @(); $ra = @(); $rb = @()
  for ($i = 0; $i -lt $Pairs; $i++) {
    $a = $rows | Where-Object { $_.pair -eq $i -and $_.arm -eq "A" }; $b = $rows | Where-Object { $_.pair -eq $i -and $_.arm -eq "B" }
    if ($a -and $b -and $a.fps -gt 0 -and $b.fps -gt 0) { $d += ($b.$m - $a.$m); $ra += $a.$m; $rb += $b.$m }
  }
  if ($d.Count -eq 0) { continue }
  $pos = @($d | Where-Object { $_ -gt 0 }).Count
  $ma = Med $ra
  Add-Content $sum ("{0,-12} A med {1,9:N3}  B med {2,9:N3}  paired d med {3,8:N3} ({4,6:N1}%)  d min {5,8:N3} max {6,8:N3}  B>A in {7}/{8}" -f $m, $ma, (Med $rb), (Med $d), (100 * (Med $d) / [math]::Max(1e-9, [math]::Abs($ma))), ($d | Measure-Object -Minimum).Minimum, ($d | Measure-Object -Maximum).Maximum, $pos, $d.Count)
}
Add-Content $sum ("marks: " + (($script:marks | Where-Object { $_ -ne "join" } | Select-Object -First 6) -join " | "))
Get-Content $sum
