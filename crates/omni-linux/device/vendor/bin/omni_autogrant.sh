#!/system/bin/sh
# omnidroid: every installed app is granted what it asks for, as soon as it is installed.
#
# The device is closed: an emulated phone with no owner's files, contacts, camera or location on
# it, so the prompts and Settings pages an app would send its user to ("Allow access to all
# files", "Display over other apps", ...) guard nothing here and only stop the app. For each app
# installed (not the image's own), its runtime permissions not yet granted are granted, and the
# special-access app-ops it requests are allowed.
#
# `setprop persist.omni.autogrant 0` turns it off (from the next install on).
#
# omni-linux reports no inotify events, so /data/app is polled -- with the shell's own globbing,
# which starts no process: the package manager is asked only when an app came, went or changed.

grant() {
    pkg=$1
    info=$(dumpsys package "$pkg")
    # Runtime permissions requested and not granted: "android.permission.X: granted=false, ...".
    for perm in $(echo "$info" | sed -n 's/^ *\([A-Za-z0-9_.]*\): granted=false.*/\1/p' | sort -u); do
        cmd package grant --user 0 "$pkg" "$perm" >/dev/null 2>&1
    done
    # Special access: the app-op each of these permissions is gated by, if requested.
    requested=$(echo "$info" | sed -n '/requested permissions:/,/permissions:$/p')
    for pair in \
        MANAGE_EXTERNAL_STORAGE:MANAGE_EXTERNAL_STORAGE \
        SYSTEM_ALERT_WINDOW:SYSTEM_ALERT_WINDOW \
        REQUEST_INSTALL_PACKAGES:REQUEST_INSTALL_PACKAGES \
        WRITE_SETTINGS:WRITE_SETTINGS \
        SCHEDULE_EXACT_ALARM:SCHEDULE_EXACT_ALARM \
        MANAGE_MEDIA:MANAGE_MEDIA \
        USE_FULL_SCREEN_INTENT:USE_FULL_SCREEN_INTENT \
        PACKAGE_USAGE_STATS:GET_USAGE_STATS \
        ACCESS_NOTIFICATION_POLICY:ACCESS_NOTIFICATION_POLICY; do
        case "$requested" in
            *"android.permission.${pair%%:*}"*) cmd appops set --user 0 "$pkg" "${pair#*:}" allow >/dev/null 2>&1 ;;
        esac
    done
    log -t omni_autogrant "granted $pkg"
}

last=
granted=
while true; do
    set -- /data/app/*/*
    now="$*"
    # The package manager answers once the system is up; until then, asked again next time.
    if [ "$now" != "$last" ] && [ "$(getprop persist.omni.autogrant)" != 0 ] && packages=$(cmd package list packages -3 --show-versioncode 2>/dev/null); then
        last=$now
        # A package and its version: an update may ask for more.
        for line in $(echo "$packages" | sed 's/^package://; s/ versionCode:/@/'); do
            case " $granted " in
                *" $line "*) ;;
                *) grant "${line%@*}"; granted="$granted $line" ;;
            esac
        done
    fi
    sleep 2
done
