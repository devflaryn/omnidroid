# omnidroid portable demo launcher.
# Usage: launch.ps1 -Apks <apk>[,<apk2>] [-Parallel] [-Root <package root>]
# Boots full Android on omnidroid (via omni-linux-run.exe, no cargo/source needed), installs the
# APK(s) and starts them. With -Parallel, the second app runs on an overlay secondary display.
param(
  [Parameter(Mandatory=$true)][string[]]$Apks,
  [switch]$Parallel,
  [string]$Root = ""
)
$ErrorActionPreference = "Stop"
if (-not $Root) { $Root = $PSScriptRoot }
if (-not $Root) { $Root = Split-Path -Parent $MyInvocation.MyCommand.Definition }
$bin     = Join-Path $Root "bin"
$sysroot = Join-Path $Root "sysroot\aosp-35"
$cpFile  = Join-Path $Root "classpath.txt"
$exe     = Join-Path $bin "omni-linux-run.exe"
foreach ($p in @($exe,$sysroot,$cpFile)) { if (-not (Test-Path $p)) { Write-Error "missing: $p"; exit 1 } }

# fresh instance in the local temp (not on the USB -- the guest fs needs local-disk speed)
$inst = Join-Path $env:TEMP ("omnidroid-demo-" + [System.IO.Path]::GetRandomFileName().Substring(0,8))
New-Item -ItemType Directory -Force -Path (Join-Path $inst "data\local\tmp") | Out-Null

# copy the APK(s) into the guest-visible /data/local/tmp
$guestApks = @()
for ($i=0; $i -lt $Apks.Count; $i++) {
  $name = "app$i.apk"
  Copy-Item $Apks[$i] (Join-Path $inst "data\local\tmp\$name") -Force
  $guestApks += "/data/local/tmp/$name"
}

# classpath exports -> --env args (classpath.txt holds "export NAME VALUE" lines)
$envArgs = @()
foreach ($line in Get-Content $cpFile) {
  if ($line -match '^\s*export\s+(\S+)\s+(.+)$') { $envArgs += @("--env", ($matches[1] + "=" + $matches[2])) }
}
$envArgs += @("--env","ANDROID_ART_ROOT=/apex/com.android.art","--env","ANDROID_I18N_ROOT=/apex/com.android.i18n","--env","ANDROID_TZDATA_ROOT=/apex/com.android.tzdata")
$ss = ((Get-Content $cpFile) | Where-Object { $_ -match '^\s*export\s+SYSTEMSERVERCLASSPATH\s+(.+)$' }) -replace '^\s*export\s+SYSTEMSERVERCLASSPATH\s+',''

# the shell run after boot. NO double-quotes anywhere (they collide with Windows native-arg
# quoting); package/activity names have no spaces so bare vars are safe.
$booted = 'i=0; until getprop sys.boot_completed | grep -q 1; do sleep 1; i=$((i+1)); [ $i -ge 1200 ] && break; done; echo demo:boot=$(getprop sys.boot_completed); '
$settings = 'settings put global device_provisioned 1; settings put secure user_setup_complete 1; svc power stayon true; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; settings put global window_animation_scale 0; settings put global transition_animation_scale 0; settings put global animator_duration_scale 0; '
$installStart = ''
if ($Parallel -and $guestApks.Count -ge 2) {
  $installStart =
    "pm install -r -g $($guestApks[0]); pm install -r -g $($guestApks[1]); " +
    'p0=$(pm list packages -3 | sed -n 1p | sed s/^package://); p1=$(pm list packages -3 | sed -n 2p | sed s/^package://); ' +
    'a0=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER $p0 | tail -1); ' +
    'a1=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER $p1 | tail -1); ' +
    'am start -W -n $a0; echo demo:started $a0 on display 0; ' +
    'settings put global overlay_display_devices 1280x720/213; sleep 4; ' +
    'am start --display 2 -n $a1; echo demo:started $a1 on overlay display 2; '
} else {
  $installStart =
    "pm install -r -g $($guestApks[0]); " +
    'p0=$(pm list packages -3 | sed -n 1p | sed s/^package://); ' +
    'a0=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER $p0 | tail -1); ' +
    'am start -W -n $a0; echo demo:started $a0; '
}
$then = $booted + $settings + $installStart

$control = $inst + ".ctl"
if (Test-Path $control) { Remove-Item -Recurse -Force $control }

$runArgs = @(
  "--sysroot", $sysroot, "--instance", $inst, "--uid", "1000"
) + $envArgs + @(
  "--zygote",
  "--init","early_hal,core,hal,main,late_start","--hal","gralloc","--hal","composer",
  "--setprop","dalvik.vm.profilesystemserver=true",
  "--setprop","persist.sys.locale=en-US",
  "--caps","IPC_LOCK,KILL,NET_ADMIN,NET_BIND_SERVICE,NET_BROADCAST,NET_RAW,SYS_MODULE,SYS_NICE,SYS_PTRACE,SYS_TIME,SYS_TTY_CONFIG,WAKE_ALARM,BLOCK_SUSPEND",
  "--then", $then,
  "--control", $control,
  "--","/system/bin/app_process64","-Xgc:CMC","-Xhidden-api-policy:disabled","/system/bin","--application","--nice-name=system_server",
  "com.android.internal.os.WrapperInit","0","35","-cp",$ss,"com.android.server.SystemServer"
)

$env:OMNI_DEVICE_APPS = "kiosk"
$env:OMNI_WINDOW = "1"
$env:OMNI_WINDOW_INPUT = "1"
if ($Parallel) { $env:OMNI_DISPLAYS = "1" } else { $env:OMNI_DISPLAYS = "1" }
$env:PATH = $bin + ";" + $env:PATH   # find bundled MSVC runtime / GPU dlls

Write-Host "omnidroid demo: booting... (a window opens when the app draws; first boot ~1-2 min)"
& $exe @runArgs
