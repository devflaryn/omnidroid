#!/system/bin/sh
# omnidroid: the boot receivers that start an app process for work this device has no use for,
# disabled. After every boot, BOOT_COMPLETED (`cmd package query-receivers -a ...`) started Settings
# (~220 MiB), the package installer, Shell, AdServices' extension and the media process's MTP and
# downloads (each a host process of its own, ~70-220 MiB and ~1-3 s of CPU) beside the app being
# launched, and set PermissionController's periodic checks going (warm device, 2026-10-22):
#   Settings: battery usage and its migration, the 16 KiB-page developer option, the SIM, the
#     Safety Center's sources, private space -- a phone's;
#   PermissionController: background-location, notification-listener and accessibility checks,
#     hibernation of unused apps, the Safety Center and its labels -- reminders for a phone's owner
#     (the process itself stays: system_server binds its service);
#   AdServices' extension boot work; the package installer's temporary-file sweep (its own
#     sessions, which `pm install` never makes); Shell's heap dump notifications; MTP (no USB); the
#     downloads' resume at boot (DownloadManager itself is untouched).
# Kept: what a system service binds, MediaProvider's scan, devicelock (disabled with the idle apps).
#
# Root (a shell may not disable a system app's component: "Shell cannot change component state").
# With no argument (init's omni_quiet, /vendor/etc/init/omni_quiet.rc): for user 0, once the system
# has booted, once per device -- the device's first boot still sends these their broadcast; a saved
# device never again. `setprop persist.omni.quiet_boot 0` (before that boot) leaves them. With a
# user id (the pool, for each user it makes): for that user, at once.
if [ -z "$1" ]; then
    [ "$(getprop persist.omni.quiet_boot)" = 0 ] && exit 0
    [ "$(getprop persist.omni.quieted)" = 1 ] && exit 0
    until [ "$(getprop sys.boot_completed)" = 1 ]; do sleep 1; done
fi
u=${1:-0}
n=0
for c in \
    com.android.settings/com.android.settings.fuelgauge.batteryusage.BatteryUsageBroadcastReceiver \
    com.android.settings/com.android.settings.fuelgauge.batteryusage.BootBroadcastReceiver \
    com.android.settings/com.android.settings.fuelgauge.BatterySettingsMigrateChecker \
    com.android.settings/com.android.settings.development.Enable16KBootReceiver \
    com.android.settings/com.android.settings.sim.receivers.SimCompleteBootReceiver \
    com.android.settings/com.android.settings.safetycenter.SafetySourceBroadcastReceiver \
    com.android.settings/com.android.settings.privatespace.PrivateSpaceBroadcastReceiver \
    'com.android.permissioncontroller/com.android.permissioncontroller.permission.service.LocationAccessCheck$SetupPeriodicBackgroundLocationAccessCheck' \
    com.android.permissioncontroller/com.android.permissioncontroller.privacysources.SetupPeriodicNotificationListenerCheck \
    com.android.permissioncontroller/com.android.permissioncontroller.hibernation.HibernationBroadcastReceiver \
    com.android.permissioncontroller/com.android.permissioncontroller.privacysources.SafetyCenterReceiver \
    'com.android.permissioncontroller/com.android.permissioncontroller.safetycenter.service.SafetyCenterBackgroundRefreshJobService$SetupSafetyCenterBackgroundRefreshReceiver' \
    'com.android.permissioncontroller/com.android.permissioncontroller.permission.service.v34.SafetyLabelChangesJobService$Receiver' \
    com.android.permissioncontroller/com.android.permissioncontroller.privacysources.AccessibilityOnBootReceiver \
    android.ext.services/com.android.adservices.service.common.AdExtBootCompletedReceiver \
    com.android.packageinstaller/com.android.packageinstaller.common.TemporaryFileManager \
    com.android.shell/com.android.shell.HeapDumpReceiver \
    com.android.mtp/com.android.mtp.MtpReceiver \
    com.android.providers.downloads/com.android.providers.downloads.DownloadReceiver; do
    case "$(cmd package disable --user "$u" "$c" 2>&1)" in
        *disabled*) n=$((n+1)) ;;
        *) log -t omni_quiet "not disabled for user $u: $c" ;;
    esac
done
# The notification assistant (ExtServices' ranking and smart replies of notifications nobody sees
# here): system_server keeps android.ext.services bound for it, a host process of its own (~72 MiB)
# on every warm device and for every user. Without one it is not started (2026-10-22).
cmd notification disallow_assistant android.ext.services/android.ext.services.notification.Assistant "$u" >/dev/null 2>&1 \
    || log -t omni_quiet "notification assistant kept for user $u"
[ -z "$1" ] && setprop persist.omni.quieted 1
log -t omni_quiet "boot receivers disabled for user $u: $n"
