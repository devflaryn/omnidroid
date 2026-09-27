# C5: an installed APK's launcher Activity, started by the real system_server

C5's gate: `am start` (the real `cmd activity start`) on an installed APK makes the real
`ActivityManagerService` start the app's process; `ActivityThread.main` attaches; the launcher
Activity's `onCreate` runs. Nothing about the app is written here.

## What stands in the way

1. **Two ART processes cannot share a host process** (binder-and-app-boot design, decision 3):
   system_server's ART holds the boot image near `0x70000000` and its heap below 4 GiB, and a
   guest address is a host address. The app runs in a host process of its own.
2. **Binder is one broker in one host process today.** The C3 plan -- the broker across host
   processes -- was not built. The app's `/dev/binder` must reach the system's broker.
3. **ActivityManager starts every app process through the zygote** (`ProcessList.startProcess` →
   `ZygoteProcess` on `/dev/socket/zygote`) and waits for the new pid. The zygote cannot fork an
   ART child here (one address space per host process: the fork design keeps the parent waiting
   until the child executes a program, and an app never does).

## The design

### Binder across host processes: remote execution of the ioctl

The app's host process does not run a broker. Each `ioctl` on its `/dev/binder` is sent, with the
calling thread's id, over a local socket to the system host process. There a **stand-in process**
for the app (a `Process` with the app's pid and uid, no CPU of its own) runs the ioctl on the real
broker exactly as a local process's would, on a host thread per app thread. The stand-in's
memory is the app's: every read and write the driver makes (the write buffer, a transaction's data
and offsets, the `mmap`'d receive area) goes back over the same socket to the app's host process,
which performs it in its guest space. The driver is unchanged; only memory access and descriptors
cross.

Descriptors a transaction carries (`TYPE_FD`): the kinds apps receive over binder at start are
shared memory (fonts, `SharedMemory`), whose host file the app's side reopens by path, and
signalled sync files, which it recreates. Others are refused by name until needed.

### The zygote: its protocol, the fork replaced by a launch

`/dev/socket/zygote` is answered on the host side (an init socket bound in the system instance):
`--query-abi-list` with `arm64-v8a`; the settings commands (`--set-api-denylist-exemptions`,
`--hidden-api-log-sampling-rate`, ...) with success; a start request by **launching** the app --
a new host process running the image's `app_process64 /system/bin android.app.ActivityThread
seq=<n>` (`RuntimeInit` → `ActivityThread.main`, the zygote-less path of decision 4) with the
request's uid, gids and nice name, and a pid assigned here -- and replying with that pid. Only the
fork is replaced; what the zygote's child does before `ActivityThread.main` (dropping to the
app's uid, its data directory, its target SDK) comes from the request's arguments, and the rest
(bindApplication, the Activity) is the framework's own.

### Installing the APK

As `adb push` to `/data/app` and a reboot install one: the APK is placed at
`/data/app/<pkg>-<id>/base.apk` in the instance before boot, and the real PackageManagerService
scans it in, with installd preparing its data directories.

## Gate

`tests/c5_app_launch.rs`: an instance with a small test APK (an Activity that logs from
`onCreate`, built with the SDK; a fixture) in `/data/app`, the boot through system_server, then
`cmd activity start -W -n <pkg>/.<Activity>`; the gate passes when the app's `onCreate` log line
appears from a process whose pid ActivityManager assigned. Then the stock APK the same way.

## Out of this design

- A real zygote (a forking snapshot across host processes).
- Descriptors other than shared memory and sync files across host processes; the app's
  `InputChannel` (a socket pair) is D5's.
