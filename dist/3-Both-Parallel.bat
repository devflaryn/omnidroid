@echo off
title omnidroid demo - Both in parallel
call "%~dp0run.bat" parallel "%~dp0apks\TalkingTomCamp.apk" "%~dp0apks\OpenTyrian.apk"
echo.
echo (demo ended -- close this window)
pause
