@echo off
rem omnidroid portable demo launcher -- pure batch, no PowerShell.
rem Tries Vulkan first (best on modern GPUs); if that does not bring the app up
rem (e.g. an older GPU with a broken Vulkan driver), it automatically retries on
rem the Direct3D 11 / GL path.
rem usage: run.bat single  <apk0>
rem        run.bat parallel <apk0> <apk1>
setlocal
set "ROOT=%~dp0"
set "BIN=%ROOT%bin"
set "SYS=%ROOT%sysroot\aosp-35"
set "CP=%ROOT%classpath.txt"
set "MODE=%~1"
set "APK0=%~2"
set "APK1=%~3"

if not exist "%BIN%\omni-linux-run.exe" ( echo ERROR: %BIN%\omni-linux-run.exe not found & pause & exit /b 1 )
if not exist "%SYS%\sysroot.manifest" ( echo ERROR: sysroot missing at %SYS% & pause & exit /b 1 )

set "THENFILE=%ROOT%then-single.txt"
if /I "%MODE%"=="parallel" set "THENFILE=%ROOT%then-parallel.txt"

rem the SystemServer classpath value, read from classpath.txt
set "SSCP="
for /f "usebackq tokens=2,*" %%A in (`findstr /b /c:"export SYSTEMSERVERCLASSPATH" "%CP%"`) do set "SSCP=%%B"

set "OMNI_DEVICE_APPS=kiosk"
set "OMNI_WINDOW=1"
set "OMNI_WINDOW_INPUT=1"
set "PATH=%BIN%;%PATH%"

rem --- attempt 1: automatic backend (Vulkan where the GPU supports it) ---
set "OMNI_GPU="
echo omnidroid demo: booting... first boot takes ~1-2 min; a window opens when the app draws.
call :boot
if exist "%LASTINST%\data\local\tmp\gpu-ok" goto :done

rem --- attempt 2: Direct3D 11 / GL fallback (older GPUs without working Vulkan) ---
echo.
echo The Vulkan path did not bring the app up -- retrying on the Direct3D/GL fallback...
echo.
set "OMNI_GPU=gl"
call :boot
goto :done

:boot
set "INST=%TEMP%\omnidroid-demo-%RANDOM%%RANDOM%"
set "LASTINST=%INST%"
mkdir "%INST%\data\local\tmp" >nul 2>&1
copy /Y "%APK0%" "%INST%\data\local\tmp\app0.apk" >nul
if /I "%MODE%"=="parallel" copy /Y "%APK1%" "%INST%\data\local\tmp\app1.apk" >nul
"%BIN%\omni-linux-run.exe" --sysroot "%SYS%" --instance "%INST%" --uid 1000 --classpath-file "%CP%" --env ANDROID_ART_ROOT=/apex/com.android.art --env ANDROID_I18N_ROOT=/apex/com.android.i18n --env ANDROID_TZDATA_ROOT=/apex/com.android.tzdata --zygote --init early_hal,core,hal,main,late_start --hal gralloc --hal composer --setprop dalvik.vm.profilesystemserver=true --setprop persist.sys.locale=en-US --caps IPC_LOCK,KILL,NET_ADMIN,NET_BIND_SERVICE,NET_BROADCAST,NET_RAW,SYS_MODULE,SYS_NICE,SYS_PTRACE,SYS_TIME,SYS_TTY_CONFIG,WAKE_ALARM,BLOCK_SUSPEND --then-file "%THENFILE%" --control "%INST%.ctl" -- /system/bin/app_process64 -Xgc:CMC -Xhidden-api-policy:disabled /system/bin --application --nice-name=system_server com.android.internal.os.WrapperInit 0 35 -cp "%SSCP%" com.android.server.SystemServer
exit /b

:done
endlocal
