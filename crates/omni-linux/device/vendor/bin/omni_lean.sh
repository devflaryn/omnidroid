#!/system/bin/sh
# omnidroid: this device keeps no background app processes -- Developer options' "Background
# process limit: No background processes", which is ActivityManager's own cached-process limit
# (`max_cached_processes` in its device configuration).
#
# Every app process here is a host process of ~260 MiB, and Android keeps an app's process cached
# after it has done what it was started for (a boot broadcast, a job) until memory runs short --
# which, on a host with memory to spare, it never does: a session kept 16 such beside the app
# (5.0 GiB, run 2026-09-28). With the limit, ActivityManager ends each one once it is cached, as a
# phone that is short of memory does; the app in front, the home screen, persistent processes and
# the services the system binds are not cached and stay.
#
# ActivityManager also spares cached processes for ten minutes after boot
# (`no_kill_cached_processes_post_boot_completed_duration_millis`, 600000: `dumpsys activity
# settings`, run 2026-09-28) -- the whole of a short session; here they go at once.
#
# `setprop persist.omni.cached_processes <n>` keeps up to n (read at boot); `off` leaves Android's
# default.
n=$(getprop persist.omni.cached_processes)
[ -z "$n" ] && n=0
[ "$n" = off ] && exit 0
# The device configuration answers once the settings provider is up.
i=0
# Both read back before it ends: the settings provider can take the first and not yet the second
# (run 2026-09-29: "post-boot grace null ms", and Android kept its ten minutes).
until { [ "$(device_config get activity_manager max_cached_processes 2>/dev/null)" = "$n" ] && [ "$(device_config get activity_manager no_kill_cached_processes_post_boot_completed_duration_millis 2>/dev/null)" = 0 ]; } || [ $i -ge 300 ]; do
    device_config put activity_manager no_kill_cached_processes_post_boot_completed_duration_millis 0 >/dev/null 2>&1
    device_config put activity_manager max_cached_processes "$n" >/dev/null 2>&1
    sleep 2
    i=$((i+1))
done
log -t omni_lean "max_cached_processes $(device_config get activity_manager max_cached_processes 2>/dev/null), post-boot grace $(device_config get activity_manager no_kill_cached_processes_post_boot_completed_duration_millis 2>/dev/null) ms"
