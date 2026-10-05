@echo off
REM Demo 3: launch BOTH apps in parallel on ONE background Android.
REM Talking Tom Camp runs on the main display; OpenTyrian runs on a second
REM (overlay) display -- two apps, one Android instance, separate windows.
title omnidroid demo - Both in parallel
powershell -NoProfile -ExecutionPolicy Bypass -Command "& '%~dp0launch.ps1' -Apks @('%~dp0apks\TalkingTomCamp.apk','%~dp0apks\OpenTyrian.apk') -Parallel"
pause
