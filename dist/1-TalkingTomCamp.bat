@echo off
REM Demo 1: launch Talking Tom Camp (single app) on omnidroid.
title omnidroid demo - Talking Tom Camp
powershell -NoProfile -ExecutionPolicy Bypass -Command "& '%~dp0launch.ps1' -Apks @('%~dp0apks\TalkingTomCamp.apk')"
pause
