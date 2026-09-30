# A short boot for a RAM A/B of boot-time state (docs/NIGHT-2026-10-02.md): boot a prebuilt r_roblox
# test binary, wait until the app has been relaunched signed in ("[r] relaunched"), wait -AfterSec,
# then census every guest host process and append one CSV row:
#   sys_priv_mb      -- the system's host process (system_server's), private bytes
#   sys_noexec_mb    -- the same less its executable private memory (translation caches, which the
#                       trimmer drops on its own schedule -- the noise this leaves out)
#   app_priv_mb      -- the app's host process
#   all_priv_mb      -- every guest host process
#   all_noexec_mb    -- every guest host process, less executable private memory
#   powershell -File tools\perf_boot.ps1 -Arm full -Exe <r_roblox-*.exe> -ExtraEnv "OMNI_PROP_FULL_COPY=1"
param(
  [Parameter(Mandatory)][string]$Arm,
  [Parameter(Mandatory)][string]$Exe,
  [string]$Csv = "C:\od-unified\perf\boot\runs.csv",
  [int]$AfterSec = 30,
  [int]$TimeoutMin = 10,
  [string]$Apk = "C:\Users\berat\Desktop\Roblox-2.740.931.apk",
  [string]$Cookie = "C:\Users\berat\Desktop\cookies\HeZmI_ImYu1080.txt",
  [string]$Sysroot = "C:\Users\berat\Desktop\Omni Apps\omnidroid\sysroot\aosp-35",
  [string]$ExtraEnv = ""
)
$ErrorActionPreference = "Continue"
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture
$dir = Split-Path -Parent $Csv; New-Item -ItemType Directory -Force -Path $dir | Out-Null
$tag = "$Arm-" + (Get-Date -Format "MMdd-HHmmss")
$env:OMNI_WINDOW = "1"; $env:OMNI_R_KIOSK = "1"; $env:OMNI_GPU = "auto"; $env:OMNI_R_MINUTES = [string]($TimeoutMin + 2)
$env:OMNI_TEST_APK = $Apk; $env:OMNI_R_COOKIE = $Cookie; Remove-Item env:OMNI_R_PLACE -ErrorAction SilentlyContinue
$env:OMNI_SYSROOT = $Sysroot; Remove-Item env:OMNI_SCREENSHOT -ErrorAction SilentlyContinue
foreach ($n in "OMNI_PROP_FULL_COPY", "OMNI_JIT_UNSAFE_FP", "OMNI_LEVER_FILE") { Remove-Item "env:$n" -ErrorAction SilentlyContinue }
if ($ExtraEnv) { foreach ($kv in $ExtraEnv.Split(";")) { if ($kv) { $p = $kv.Split("=", 2); Set-Item -Path ("env:" + $p[0]) -Value $p[1] } } }
function Stop-Guests { Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "omni-linux-run*" -or $_.Name -like "r_roblox*" } | Stop-Process -Force -ErrorAction SilentlyContinue }
Stop-Guests; Start-Sleep 3
for ($i = 0; $i -lt 30; $i++) { if ((Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory -ge 13GB / 1KB) { break }; Start-Sleep 2 }
$tree = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $Exe)))
$proc = Start-Process -FilePath $Exe -ArgumentList "--ignored", "--nocapture", "--exact", "the_apk_is_installed_started_and_draws" `
  -WorkingDirectory $tree -RedirectStandardOutput (Join-Path $dir "$tag.out.log") -RedirectStandardError (Join-Path $dir "$tag.err.log") -PassThru -WindowStyle Hidden
$log = Join-Path $env:TEMP ("omni-linux-r-{0}.log" -f $proc.Id)
$t0 = Get-Date; $ok = $false
while (((Get-Date) - $t0).TotalMinutes -lt $TimeoutMin) {
  Start-Sleep 3
  if ($proc.HasExited) { break }
  if ((Test-Path $log) -and (Select-String -Path $log -Pattern "\[r\] relaunched" -Quiet)) { $ok = $true; break }
}
if (-not $ok) { Stop-Guests; Add-Content $Csv "$tag,$Arm,norelaunch"; exit 1 }
$at_s = [int]((Get-Date) - $t0).TotalSeconds
Start-Sleep $AfterSec
$census = & (Join-Path $PSScriptRoot "vmcensus_rows.ps1")
Stop-Guests
if (-not (Test-Path $Csv)) { Set-Content $Csv "tag,arm,status,relaunch_s,sys_priv_mb,sys_noexec_mb,app_priv_mb,all_priv_mb,all_noexec_mb,procs" -Encoding ascii }
$sys = $census | Where-Object { $_.name -eq "system_server" } | Select-Object -First 1
$app = $census | Where-Object { $_.name -eq "com.roblox.client" } | Select-Object -First 1
$line = "{0},{1},ok,{2},{3:F1},{4:F1},{5:F1},{6:F1},{7:F1},{8}" -f $tag, $Arm, $at_s, $sys.priv, $sys.noexec, $app.priv, (($census | Measure-Object priv -Sum).Sum), (($census | Measure-Object noexec -Sum).Sum), @($census).Count
Add-Content $Csv $line -Encoding ascii
$line
