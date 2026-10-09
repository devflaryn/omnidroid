# One in-world measurement run of a prebuilt r_roblox test binary (an "arm"), for interleaved A/B
# across builds (docs/NIGHT-2026-10-02.md). Boots the APK, waits for "Joining game", lets the world
# load, then measures a fixed window and appends one CSV row:
#   fps         -- presented frames / second over the window (from the [display] lines, exact)
#   top_ms      -- the busiest thread of the app's host process, CPU ms per presented frame (the
#                  engine worker: guest compute per frame -- the fps limit)
#   top2_ms     -- the second busiest (the render thread), CPU ms per frame
#   app_ms      -- the app's host process, all threads, CPU ms per frame
#   all_ms      -- every guest host process (omni-linux-run*), CPU ms per frame
#   priv_gb     -- median summed private bytes of the guest host processes over the window
#   app_priv_gb -- median private bytes of the app's host process
# Thread CPU is read from Windows (Process.Threads), so nothing inside the guest host is perturbed.
#
#   powershell -ExecutionPolicy Bypass -File tools\perf_ab.ps1 -Arm main -Exe <path to r_roblox-*.exe>
param(
  [Parameter(Mandatory)][string]$Arm,
  [Parameter(Mandatory)][string]$Exe,
  [string]$Csv = "C:\od-unified\perf\ab\runs.csv",
  [int]$SettleSec = $(if ($env:OMNI_AB_SETTLE) { [int]$env:OMNI_AB_SETTLE } else { 75 }),
  [int]$WindowSec = $(if ($env:OMNI_AB_WINDOW) { [int]$env:OMNI_AB_WINDOW } else { 150 }),
  [int]$JoinTimeoutMin = $(if ($env:OMNI_AB_JOIN_MIN) { [int]$env:OMNI_AB_JOIN_MIN } else { 14 }),
  [string]$Apk = $(if ($env:OMNI_AB_APK) { $env:OMNI_AB_APK } else { "C:\Users\berat\Desktop\Roblox-2.740.931.apk" }),
  [string]$Cookie = "C:\Users\berat\Desktop\cookies\HeZmI_ImYu1080.txt",
  [string]$Place = "8737899170",
  [string]$Sysroot = "C:\Users\berat\Desktop\Omni Apps\omnidroid\sysroot\aosp-35",
  [string]$ExtraEnv = "",
  # Free commit a boot waits for (GB). 13 is safe on a quiet host; one lean instance commits ~4-5 GB.
  [double]$MinCommitGB = $(if ($env:OMNI_AB_MIN_COMMIT_GB) { [double]$env:OMNI_AB_MIN_COMMIT_GB } else { 13 }),
  # CPU affinity mask (hex) for the run and every host process it spawns (they inherit it), to
  # stand in for a weaker PC: e.g. F0000 = four E-cores of the i7-13700F. Empty = all CPUs.
  [string]$Affinity = $(if ($env:OMNI_AB_AFFINITY) { $env:OMNI_AB_AFFINITY } else { "" })
)
$ErrorActionPreference = "Continue"
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture
$dir = Split-Path -Parent $Csv
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$stamp = Get-Date -Format "MMdd-HHmmss"
$tag = "$Arm-$stamp"
$out = Join-Path $dir "$tag.out.log"; $err = Join-Path $dir "$tag.err.log"

$env:OMNI_WINDOW = "1"; $env:OMNI_R_KIOSK = "1"; $env:OMNI_GPU = "auto"
$env:OMNI_R_MINUTES = [string]($JoinTimeoutMin + 6)
$env:OMNI_TEST_APK = $Apk; $env:OMNI_R_COOKIE = $Cookie; $env:OMNI_R_PLACE = $Place
$env:OMNI_SYSROOT = $Sysroot
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
Stop-Guests
Start-Sleep 3
# The previous run's memory is given back over a few seconds; a boot needs ~13 GB of free commit.
# A boot needs ~13 GB of free commit; below that it would starve this host's other processes (the
# owner's games) as well as itself. Wait for it (up to 30 min); never boot without it.
$okCommit = $false
for ($i = 0; $i -lt 900; $i++) { if ((Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory -ge $MinCommitGB * 1GB / 1KB) { $okCommit = $true; break }; Start-Sleep 2 }
if (-not $okCommit) { Write-Output "LOWCOMMIT: free commit stayed under $MinCommitGB GB for 30 min; not booting"; exit 3 }

# The network: Roblox is ISP-blocked here, and a run without the bypass (WARP) gets its TLS
# connections reset -- the app never initialises and (Delta's build) dies ~20 s after start, which
# looks like a crash, not a network fault (2026-10-09 22:2x). Wait for roblox.com to answer.
$netOk = $false; $warp = ""
for ($i = 0; $i -lt 180; $i++) {
  try {
    $r = Invoke-WebRequest -UseBasicParsing -Uri "https://www.roblox.com/" -TimeoutSec 10 -Method Head
    if ($r.StatusCode -lt 500) {
      try { $warp = ((Invoke-WebRequest -UseBasicParsing -Uri "https://www.cloudflare.com/cdn-cgi/trace" -TimeoutSec 10).Content -split "`n" | Where-Object { $_ -like "warp=*" }) -replace "warp=", "" } catch {}
      $netOk = $true; break
    }
  } catch {}
  Start-Sleep 10
}
if (-not $netOk) { Write-Output "NONET: roblox.com did not answer for 30 min; not booting"; exit 5 }

$crate = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $Exe)))  # <tree>/target/release/deps -> <tree>
$proc = Start-Process -FilePath $Exe -ArgumentList "--ignored", "--nocapture", "--exact", "the_apk_is_installed_started_and_draws" `
  -WorkingDirectory $crate -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -WindowStyle Hidden
# A launch that failed (e.g. an exe outside <tree>/target/release/deps) must not wait out the join
# timeout and then kill whatever guests are running by then.
if (-not $proc) { Write-Output "NOSTART: $Exe did not start"; exit 4 }
if ($Affinity) { try { $proc.ProcessorAffinity = [IntPtr][Convert]::ToInt64($Affinity, 16) } catch { Write-Output "AFFINITY: could not set $Affinity" } }
$log = Join-Path $env:TEMP ("omni-linux-r-{0}.log" -f $proc.Id)

function Guests { @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" }) }
function Result($status, $extra) {
  $row = [ordered]@{ tag = $tag; arm = $Arm; status = $status; when = (Get-Date -Format o); warp = $warp }
  if ($extra) { foreach ($k in $extra.Keys) { $row[$k] = $extra[$k] } }
  $obj = [pscustomobject]$row
  $cols = "tag,arm,status,when,fps,top_ms,top2_ms,app_ms,all_ms,priv_gb,app_priv_gb,procs,top_name,join_s,ws_gb,wspriv_gb,sys_wspriv_gb,threads,sys_threads,loaded_s,cores,affinity,warp,t_ss,t_boot,t_login,t_join,t_loaded,log"
  if (-not (Test-Path $Csv)) { Set-Content -Path $Csv -Value $cols -Encoding utf8 }
  $line = ($cols.Split(",") | ForEach-Object { $v = $obj.$_; if ($null -eq $v) { "" } else { [string]$v } }) -join ","
  Add-Content -Path $Csv -Value $line -Encoding utf8
  Write-Output $line
}

# 1. Wait for the join.
$t0 = Get-Date; $joined = $null
while (((Get-Date) - $t0).TotalMinutes -lt $JoinTimeoutMin) {
  Start-Sleep 5
  if ($proc.HasExited) { break }
  if ((Test-Path $log) -and (Select-String -Path $log -Pattern "Joining game" -Quiet)) { $joined = Get-Date; break }
}
if (-not $joined) { Stop-Guests; Result "nojoin" @{}; exit 1 }
$join_s = [int]($joined - $t0).TotalSeconds
# The settle, watching for the world itself (onGameLoaded) to time the load.
$loaded_s = $null; $sEnd = (Get-Date).AddSeconds($SettleSec)
while ((Get-Date) -lt $sEnd) {
  Start-Sleep 2
  if (-not $loaded_s -and (Select-String -Path $log -Pattern "onGameLoaded" -Quiet)) { $loaded_s = [int]((Get-Date) - $t0).TotalSeconds }
}

# 2. The window: thread CPU at both ends, memory every 15 s, the [display] lines in between.
function Snap {
  $g = Guests
  $app = $g | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1
  $threads = @{}
  foreach ($t in $app.Threads) { try { $threads[$t.Id] = $t.TotalProcessorTime.TotalMilliseconds } catch {} }
  $all = 0.0; foreach ($p in $g) { try { $all += $p.TotalProcessorTime.TotalMilliseconds } catch {} }
  [pscustomobject]@{ t = Get-Date; appId = $app.Id; appCpu = $app.TotalProcessorTime.TotalMilliseconds; threads = $threads; all = $all }
}
$lines0 = @(Get-Content $log).Count
$s0 = Snap
$priv = New-Object System.Collections.ArrayList; $appPriv = New-Object System.Collections.ArrayList; $procs = 0
$ws = New-Object System.Collections.ArrayList; $wsPriv = New-Object System.Collections.ArrayList; $sysWsPriv = New-Object System.Collections.ArrayList
$sysThreads = New-Object System.Collections.ArrayList; $threadsAll = New-Object System.Collections.ArrayList
$wEnd = (Get-Date).AddSeconds($WindowSec)
while ((Get-Date) -lt $wEnd) {
  Start-Sleep 15
  $g = Guests
  [void]$priv.Add(($g | Measure-Object PrivateMemorySize64 -Sum).Sum)
  [void]$appPriv.Add((($g | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First 1).PrivateMemorySize64))
  $procs = [math]::Max($procs, $g.Count)
  # What Task Manager shows (private working set), the working set, and thread counts.
  [void]$ws.Add(($g | Measure-Object WorkingSet64 -Sum).Sum)
  $wmi = @(Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" })
  [void]$wsPriv.Add(($wmi | Measure-Object WorkingSetPrivate -Sum).Sum)
  $sysP = $g | Sort-Object { $_.Threads.Count } -Descending | Select-Object -First 1
  if ($sysP) { [void]$sysThreads.Add($sysP.Threads.Count); $sw = $wmi | Where-Object { $_.IDProcess -eq $sysP.Id } | Select-Object -First 1; if ($sw) { [void]$sysWsPriv.Add([double]$sw.WorkingSetPrivate) } }
  [void]$threadsAll.Add((($g | ForEach-Object { $_.Threads.Count }) | Measure-Object -Sum).Sum)
}
$s1 = Snap
# Milestones, from the log's own clock (`[t] +N.Ns` ticks, about one a second): when system_server's
# runtime started, Android booted, the account signed in, the place was being joined, the world was
# loaded -- finer than the join poll above (5 s).
function Milestones {
  $m = [ordered]@{ t_ss = ""; t_boot = ""; t_login = ""; t_join = ""; t_loaded = "" }
  $pat = [ordered]@{ t_ss = "START com.android.internal.os.RuntimeInit uid 1000"; t_boot = "[r] boot_completed=1"; t_login = "DID_LOG_IN"; t_join = "Joining game"; t_loaded = "onGameLoaded()" }
  $t = ""
  # The log is still being written: open it sharing reads and writes, as Get-Content does.
  $fs = [System.IO.FileStream]::new($log, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
  $rd = [System.IO.StreamReader]::new($fs)
  while ($null -ne ($line = $rd.ReadLine())) {
    if ($line.StartsWith("[t] +")) { $t = $line.Substring(5).Split("s")[0]; continue }
    foreach ($k in $pat.Keys) { if ($m[$k] -eq "" -and $line.Contains($pat[$k])) { $m[$k] = $t } }
    if ($m.t_loaded -ne "") { break }
  }
  $rd.Dispose(); $fs.Dispose()
  $m
}
$ms = Milestones
$new = @(Get-Content $log | Select-Object -Skip $lines0)
$kicked = $new | Select-String -Pattern "Client has been disconnected" -Quiet

# fps: exact frames over exact time, from consecutive [display] lines (each gives its own rate).
$disp = @($new | Select-String -Pattern "\[display\] (\d+) frames presented \(([\d.]+)/s\)" | ForEach-Object { [pscustomobject]@{ f = [double]$_.Matches[0].Groups[1].Value; r = [double]$_.Matches[0].Groups[2].Value } })
$frames = 0.0; $secs = 0.0
for ($i = 1; $i -lt $disp.Count; $i++) {
  $df = $disp[$i].f - $disp[$i - 1].f
  if ($disp[$i].r -gt 0) { $frames += $df; $secs += $df / $disp[$i].r }
}
Stop-Guests
if ($s0.appId -ne $s1.appId -or $secs -le 0) { Result "badwindow" @{ join_s = $join_s }; exit 1 }
$fps = $frames / $secs
$wall = ($s1.t - $s0.t).TotalSeconds
$nframes = $fps * $wall
$deltas = foreach ($k in $s1.threads.Keys) { if ($s0.threads.ContainsKey($k)) { [pscustomobject]@{ id = $k; d = $s1.threads[$k] - $s0.threads[$k] } } }
$top = @($deltas | Sort-Object d -Descending | Select-Object -First 2)
function Med($a) { $s = @($a | Sort-Object); if ($s.Count -eq 0) { 0 } else { $s[[int][math]::Floor($s.Count / 2)] } }
Result ($(if ($kicked) { "kicked" } else { "ok" })) ([ordered]@{
  fps = "{0:N2}" -f $fps
  top_ms = "{0:N2}" -f ($top[0].d / $nframes)
  top2_ms = "{0:N2}" -f ($top[1].d / $nframes)
  app_ms = "{0:N2}" -f (($s1.appCpu - $s0.appCpu) / $nframes)
  all_ms = "{0:N2}" -f (($s1.all - $s0.all) / $nframes)
  priv_gb = "{0:N3}" -f ((Med $priv) / 1GB)
  app_priv_gb = "{0:N3}" -f ((Med $appPriv) / 1GB)
  procs = $procs
  top_name = "tid$($top[0].id)"
  join_s = $join_s
  ws_gb = "{0:N3}" -f ((Med $ws) / 1GB)
  wspriv_gb = "{0:N3}" -f ((Med $wsPriv) / 1GB)
  sys_wspriv_gb = "{0:N3}" -f ((Med $sysWsPriv) / 1GB)
  threads = (Med $threadsAll)
  sys_threads = (Med $sysThreads)
  loaded_s = $loaded_s
  cores = "{0:N2}" -f (($s1.all - $s0.all) / 1000 / $wall)
  affinity = $Affinity
  t_ss = $ms.t_ss
  t_boot = $ms.t_boot
  t_login = $ms.t_login
  t_join = $ms.t_join
  t_loaded = $ms.t_loaded
  log = Split-Path -Leaf $log
})
