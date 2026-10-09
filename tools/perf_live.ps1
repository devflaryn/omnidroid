# Interleaved A/B inside ONE live in-world session (docs/NIGHT-2026-10-02.md): boot a prebuilt
# r_roblox test binary with OMNI_LEVER_FILE, join PS99, settle, then alternate a lever (see
# crates/omni-linux/src/lever.rs) between value A and value B for -Pairs pairs in ABBA order,
# each phase: write the lever, wait -WarmSec (translations redone, the switch settles), measure
# -MeasureSec. Per phase one CSV row; at the end the paired deltas (B - A) with their spread.
# Several levers, one after another in the same session: -Levers "name:A:B[:bg=v,bg2=v];name2:A:B",
# the optional fourth field being lines written beside the lever for that whole A/B.
#
#   powershell -File tools\perf_live.ps1 -Exe <r_roblox-*.exe> -Lever jit_fp -A 0 -B f0000 -Tag fp-all
#   powershell -File tools\perf_live.ps1 -Exe <exe> -Levers "compose_fast:0:1;fence_poll:0:250:compose_fast=1" -Tag c4c5
param(
  [Parameter(Mandatory)][string]$Exe,
  [string]$Lever = "",
  [string]$A = "",
  [string]$B = "",
  [string]$Levers = "",
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
$sum = Join-Path $Dir "$name.summary.txt"
$leverFile = Join-Path $Dir "$name.lever.txt"

# The A/Bs to take: (lever, valueA, valueB, background lines).
$plan = @()
if ($Levers) {
  foreach ($spec in $Levers.Split(";")) {
    if (-not $spec) { continue }
    $f = $spec.Split(":", 4)
    $plan += , @($f[0], $f[1], $f[2], $(if ($f.Count -gt 3) { $f[3].Split(",") -join "`n" } else { "" }))
  }
} else {
  $plan += , @($Lever, $A, $B, "")
}
function Write-Lever($bg, $lv, $val) {
  $text = if ($bg) { "$bg`n$lv=$val" } else { "$lv=$val" }
  Set-Content -Path $leverFile -Value $text -NoNewline -Encoding ascii
}
Write-Lever $plan[0][3] $plan[0][0] $plan[0][1]

$env:OMNI_WINDOW = "1"; $env:OMNI_R_KIOSK = "1"; $env:OMNI_GPU = "auto"
$env:OMNI_R_MINUTES = [string]($JoinTimeoutMin + 2 + [math]::Ceiling(($SettleSec + $plan.Count * 2 * $Pairs * ($WarmSec + $MeasureSec)) / 60) + 2)
$env:OMNI_TEST_APK = $Apk; $env:OMNI_R_COOKIE = $Cookie; $env:OMNI_R_PLACE = $Place
$env:OMNI_SYSROOT = $Sysroot; $env:OMNI_LEVER_FILE = $leverFile
Remove-Item env:OMNI_SCREENSHOT -ErrorAction SilentlyContinue
if ($ExtraEnv) { foreach ($kv in $ExtraEnv.Split(";")) { if ($kv) { $p = $kv.Split("=", 2); Set-Item -Path ("env:" + $p[0]) -Value $p[1] } } }

function Remove-DeadShm {
  # Graphics regions (`omni-shm-<host pid>-<n>`) of host processes no longer alive: a killed run
  # leaves them, and a later host process given the same pid collided with them (2026-10-09).
  $live = @{}; Get-Process | ForEach-Object { $live[[string]$_.Id] = $true }
  Get-ChildItem -Path $env:TEMP -Filter "omni-shm-*" -File -ErrorAction SilentlyContinue | ForEach-Object {
    if ($_.Name -match '^omni-shm-(\d+)-' -and -not $live[$Matches[1]]) { Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue }
  }
}

function Stop-Guests {
  # The run's own instance first, through its stop file: it shuts down and removes its instance
  # directory itself (a killed run leaves ~0.8 GB in %TEMP% for good). Then whatever is left.
  if ($proc -and -not $proc.HasExited) {
    $inst = Join-Path $env:TEMP ("omni-linux-r-{0}" -f $proc.Id)
    $stop = Join-Path $inst "data\local\tmp\stop"
    if (Test-Path (Split-Path -Parent $stop)) { Set-Content -Path $stop -Value "stop" -ErrorAction SilentlyContinue }
    for ($i = 0; $i -lt 40 -and -not $proc.HasExited; $i++) { Start-Sleep 1 }
  }
  Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" -or $_.Name -like "r_roblox*" } | Stop-Process -Force -ErrorAction SilentlyContinue
  if ($proc) {
    Start-Sleep 2
    foreach ($d in @(("omni-linux-r-{0}" -f $proc.Id), ("omni-linux-r-{0}-shots" -f $proc.Id))) {
      $full = Join-Path $env:TEMP $d
      if (Test-Path $full) { Remove-Item $full -Recurse -Force -ErrorAction SilentlyContinue }
    }
  }
  Remove-DeadShm
}
Stop-Guests; Start-Sleep 3
# A lean kiosk boot needs ~4-6 GB of free commit (an instance commits ~3.7 GB in-world); below that it would starve this host's other processes (the
# owner's games) as well as itself. Wait for it (up to 30 min); never boot without it.
$okCommit = $false
for ($i = 0; $i -lt 900; $i++) { if ((Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory -ge 6GB / 1KB) { $okCommit = $true; break }; Start-Sleep 2 }
if (-not $okCommit) { Write-Output "LOWCOMMIT: free commit stayed under 6 GB for 30 min; not booting"; exit 3 }
$tree = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $Exe)))
$proc = Start-Process -FilePath $Exe -ArgumentList "--ignored", "--nocapture", "--exact", "the_apk_is_installed_started_and_draws" `
  -WorkingDirectory $tree -RedirectStandardOutput (Join-Path $Dir "$name.out.log") -RedirectStandardError (Join-Path $Dir "$name.err.log") -PassThru -WindowStyle Hidden
$log = Join-Path $env:TEMP ("omni-linux-r-{0}.log" -f $proc.Id)
Add-Content $sum "run $name exe $Exe plan $(($plan | ForEach-Object { $_[0] + ':' + $_[1] + '/' + $_[2] }) -join ' ') pairs $Pairs warm $WarmSec measure $MeasureSec log $log"

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
    elseif ($l -match "Process com\.roblox\.client \(pid \d+\) has died") { [void]$script:marks.Add("appdied") }
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
  $sysCpu = 0.0; $sp = $g | Where-Object { $_.Id -eq $script:sysPid } | Select-Object -First 1; if ($sp) { $sysCpu = $sp.TotalProcessorTime.TotalMilliseconds }
  [pscustomobject]@{ t = Get-Date; appId = $app.Id; appCpu = $app.TotalProcessorTime.TotalMilliseconds; threads = $threads; all = $all; sys = $sysCpu
    priv = ($g | Measure-Object PrivateMemorySize64 -Sum).Sum; appPriv = $app.PrivateMemorySize64; n = $g.Count
    ws = ($g | Measure-Object WorkingSet64 -Sum).Sum }
}

$t0 = Get-Date
while (((Get-Date) - $t0).TotalMinutes -lt $JoinTimeoutMin) { Wait-Pumping 5; if ($script:marks -contains "join" -or $proc.HasExited) { break } }
if (-not ($script:marks -contains "join")) { Add-Content $sum "NOJOIN"; Stop-Guests; Get-Content $sum; exit 1 }
Add-Content $sum ("joined after {0:N0} s" -f ((Get-Date) - $t0).TotalSeconds)
Wait-Pumping $SettleSec
$script:sysPid = (Get-CimInstance Win32_Process -Filter "Name like 'omni-linux-run%'" | Where-Object { $_.CommandLine -match 'nice-name=system_server' } | Select-Object -First 1).ProcessId

$csvs = @()
foreach ($ab in $plan) {
  $lv = $ab[0]; $valA = $ab[1]; $valB = $ab[2]; $bg = $ab[3]
  $csv = Join-Path $Dir "$name.$lv.csv"; $csvs += $csv
  Set-Content -Path $csv -Value "pair,arm,value,fps,top_ms,top2_ms,app_ms,all_ms,priv_gb,app_priv_gb,procs,lines,sys_ms,ws_gb" -Encoding ascii
  for ($i = 0; $i -lt $Pairs; $i++) {
    $order = if ($i % 2 -eq 0) { @("A", "B") } else { @("B", "A") }
    foreach ($arm in $order) {
      $value = if ($arm -eq "A") { $valA } else { $valB }
      if ($script:marks -contains "appdied") { break }
      Write-Lever $bg $lv $value
      Wait-Pumping $WarmSec
      $s0 = Snap; $d0 = $script:disp.Count
      Wait-Pumping $MeasureSec
      $s1 = Snap
      $frames = 0.0; $secs = 0.0
      for ($k = [math]::Max(1, $d0); $k -lt $script:disp.Count; $k++) {
        $dl = $script:disp[$k]
        if ($dl.r -gt 0) { $df = $dl.f - $script:disp[$k - 1].f; $frames += $df; $secs += $df / $dl.r }
      }
      $fps = if ($secs -gt 0) { $frames / $secs } else { 0 }
      $wall = ($s1.t - $s0.t).TotalSeconds; $nf = [math]::Max(1, $fps * $wall)
      $deltas = foreach ($k in $s1.threads.Keys) { if ($s0.threads.ContainsKey($k)) { $s1.threads[$k] - $s0.threads[$k] } }
      $top = @($deltas | Sort-Object -Descending | Select-Object -First 2)
      if ($s0.appId -ne $s1.appId) { $fps = 0 }
      Add-Content $csv ("{0},{1},{2},{3:F3},{4:F3},{5:F3},{6:F3},{7:F3},{8:F3},{9:F3},{10},{11},{12:F3},{13:F3}" -f $i, $arm, $value, $fps, ($top[0] / $nf), ($top[1] / $nf), (($s1.appCpu - $s0.appCpu) / $nf), (($s1.all - $s0.all) / $nf), ($s1.priv / 1GB), ($s1.appPriv / 1GB), $s1.n, ($script:disp.Count - $d0), (($s1.sys - $s0.sys) / $nf), ($s1.ws / 1GB))
    }
  }
}
Pump
Stop-Guests
if ($script:marks -contains "appdied") { Add-Content $sum "APP DIED during the A/B: phases after it are missing (the CSV stops there)" }
foreach ($csv in $csvs) {
  Add-Content $sum "== $csv"
  & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "perf_pairs.ps1") -Csv $csv | Add-Content $sum
}
Add-Content $sum ("marks: " + (($script:marks | Where-Object { $_ -ne "join" } | Select-Object -Last 8) -join " | "))
Get-Content $sum
