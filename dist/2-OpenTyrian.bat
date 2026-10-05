@echo off
REM Demo 2: launch OpenTyrian (single app) on omnidroid.
title omnidroid demo - OpenTyrian
powershell -NoProfile -ExecutionPolicy Bypass -Command "& '%~dp0launch.ps1' -Apks @('%~dp0apks\OpenTyrian.apk')"
pause
