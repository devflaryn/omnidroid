omnidroid -- portable Android app demo (Windows)
=================================================

omnidroid runs real Android apps natively on Windows: the app's arm64 code runs on
omnidroid's own translator (no VM, no emulator image), with Android (system_server,
SurfaceFlinger, the zygote) booted on omnidroid's Linux layer, and graphics on the
host GPU. Nothing needs to be installed on this PC.

HOW TO RUN
----------
Double-click one of:

  1-TalkingTomCamp.bat   -- launches Talking Tom Camp (one app, one window)
  2-OpenTyrian.bat       -- launches OpenTyrian (one app, one window)
  3-Both-Parallel.bat    -- launches BOTH apps at once on ONE background Android,
                            each in its own window (Talking Tom Camp on the main
                            display, OpenTyrian on a second display)

A console window shows the boot progress. The first launch takes about 1-2 minutes
(Android boots, then the app is installed and started); a window opens when the app
draws its first frame. Close the app's window (or the console) to end the demo.

(The launchers are plain .bat files -- no PowerShell, no .NET, nothing to install.)

REQUIREMENTS
------------
- 64-bit Windows 10 or 11.
- A GPU with its normal drivers (Intel/AMD/NVIDIA). omnidroid uses Vulkan when the
  GPU supports it; if the Vulkan path does not come up (e.g. an older GPU with a
  broken Vulkan driver), the launcher AUTOMATICALLY retries on the bundled
  Direct3D 11 / GL path -- so it still works. (That retry means a second ~1-2 min
  boot on those machines; modern GPUs boot once on Vulkan.)
- ~4 GB free space on the system drive (a temporary Android instance is created in
  the Windows TEMP folder for each run and removed when you unplug/clean up).

WHAT'S IN THIS FOLDER
---------------------
  bin\          omnidroid binaries + the runtime DLLs they need
  sysroot\      the Android system image omnidroid boots
  apks\         the demo apps (TalkingTomCamp.apk, OpenTyrian.apk)
  run.bat       the launcher the numbered .bat files call
  then-*.txt    the boot/install/start script each demo runs
  classpath.txt the Android boot classpath

NOTES
-----
- This is a demo build. The apps run unmodified.
- To run a different APK:  launch.ps1 -Apks C:\path\to\your.apk
- Two apps of the SAME package but different versions would each get their own
  Android instance; different packages (as here) share one Android on separate
  displays.
