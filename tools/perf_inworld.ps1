# Measure the real-AOSP path in-world (Windows): boot the APK, join a place, then sample the
# guest-host processes' private bytes / working set and the on-screen present rate, and print a
# steady-state summary. Built for Phase C perf work (docs/NIGHT-2026-10-01.md).
#
#   powershell -ExecutionPolicy Bypass -File tools\perf_inworld.ps1 -Tag baseline
#   powershell -ExecutionPolicy Bypass -File tools\perf_inworld.ps1 -Tag try1 -ExtraEnv "OMNI_JIT_SHARED_CACHE_LIVE_MB=192"
#   powershell -ExecutionPolicy Bypass -File tools\perf_inworld.ps1 -Tag census -ExtraEnv "OMNI_GPU_MEM=15;OMNI_MEM_TRACE=30"
#
# Output goes to C:\od-unified\perf\<Tag>.{result.txt,log,err.log,png}. The [display]/[window] fps
# lines live in the omni-linux-run CHILD log (%TEMP%\omni-linux-r-<pid>.log), which this reads --
# not the test's own stderr. Note: the in-world private figure has ~100 MiB run-to-run variance
# (which background app processes are alive, when trimming fires), so compare medians over several
# runs, not a single pair.
param(
  [string]$Tag = "run",
  [int]$Minutes = 13,
  [string]$Apk = "C:\Users\berat\Desktop\Roblox-2.740.931.apk",
  [string]$Cookie = "C:\Users\berat\Desktop\cookies\HeZmI_ImYu1080.txt",
  [string]$Place = "8737899170",
  [string]$ExtraEnv = ""    # "NAME=VAL;NAME2=VAL2", applied before the run (e.g. a lever to test)
)
$ErrorActionPreference = "Continue"
$repo = Split-Path -Parent $PSScriptRoot
$out  = "C:\od-unified\perf"
New-Item -ItemType Directory -Force -Path $out | Out-Null
$log = Join-Path $out "$Tag.log"; $errlog = Join-Path $out "$Tag.err.log"
$res = Join-Path $out "$Tag.result.txt"; $shot = Join-Path $out "$Tag.png"
Remove-Item $log,$errlog,$res,$shot -ErrorAction SilentlyContinue

$env:OMNIDROID_DYNARMIC_BUILD_DIR = "C:\od-unified"
$env:OMNI_WINDOW = "1"; $env:OMNI_R_KIOSK = "1"; $env:OMNI_GPU = "auto"
$env:OMNI_R_MINUTES = [string]$Minutes
$env:OMNI_TEST_APK = $Apk; $env:OMNI_R_COOKIE = $Cookie; $env:OMNI_R_PLACE = $Place
$env:OMNI_SCREENSHOT = $shot
if ($ExtraEnv) { foreach ($kv in $ExtraEnv.Split(";")) { if ($kv) { $p=$kv.Split("=",2); Set-Item -Path ("env:"+$p[0]) -Value $p[1]; Add-Content $res ("env "+$kv) } } }

$proc = Start-Process -FilePath "cargo" `
  -ArgumentList "test","--release","-q","-p","omni-linux","--test","r_roblox","--","--ignored","--nocapture" `
  -WorkingDirectory $repo -RedirectStandardOutput $log -RedirectStandardError $errlog -PassThru -WindowStyle Hidden

function Sample-Mem {
  $procs = Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" }
  if (-not $procs) { return $null }
  [pscustomobject]@{ count=@($procs).Count; priv=($procs|Measure-Object PrivateMemorySize64 -Sum).Sum; ws=($procs|Measure-Object WorkingSet64 -Sum).Sum }
}
function Child-Log { Get-ChildItem "$env:TEMP\omni-linux-r-*.log" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty FullName }
function Last-Fps($c) { if (-not $c -or -not (Test-Path $c)) { return $null }; $m = Get-Content $c -Tail 120 -ErrorAction SilentlyContinue | Select-String "frames presented \(([\d.]+)/s\)" | Select-Object -Last 1; if ($m) { [double]$m.Matches[0].Groups[1].Value } else { $null } }

$start = Get-Date; $deadline = $start.AddMinutes($Minutes)
$samples = New-Object System.Collections.ArrayList
while ((Get-Date) -lt $deadline) {
  Start-Sleep -Seconds 15
  $el = [int]((Get-Date)-$start).TotalSeconds; $m = Sample-Mem; $fps = Last-Fps (Child-Log)
  if ($m) { [void]$samples.Add([pscustomobject]@{t=$el;priv=$m.priv;ws=$m.ws;count=$m.count;fps=$fps}); Add-Content $res ("t={0}s priv={1:N0} ws={2:N0} procs={3} fps={4}" -f $el,$m.priv,$m.ws,$m.count,($(if($fps){"{0:N2}" -f $fps}else{"-"}))) }
  if ($proc.HasExited) { Add-Content $res "run exited early"; break }
}
function Median($a){ if(@($a).Count -eq 0){return 0}; $s=@($a|Sort-Object); $s[[int][math]::Floor($s.Count/2)] }
function P10($a){ if(@($a).Count -eq 0){return 0}; $s=@($a|Sort-Object); $s[[int][math]::Floor($s.Count*0.1)] }
$withproc = @($samples | Where-Object { $_.count -gt 0 })
$steady = @($withproc | Select-Object -Last ([math]::Max(1,[int]($withproc.Count/2))))
$fpsvals = @($steady | Where-Object { $_.fps -ne $null } | ForEach-Object { $_.fps })
$privvals = @($steady | ForEach-Object { $_.priv }); $wsvals = @($steady | ForEach-Object { $_.ws })
Add-Content $res "==== SUMMARY $Tag ===="
Add-Content $res ("priv_median_GB={0:N3} priv_max_GB={1:N3} ws_median_GB={2:N3} fps_median={3:N2} fps_p10={4:N2}" -f ((Median $privvals)/1GB),(($privvals|Measure-Object -Max).Maximum/1GB),((Median $wsvals)/1GB),(Median $fpsvals),(P10 $fpsvals))
try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch {}
Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" -or $_.Name -like "r_roblox*" -or $_.Name -eq "cargo" } | Stop-Process -Force -ErrorAction SilentlyContinue
Add-Content $res "DONE $(Get-Date -Format o)"; Get-Content $res
