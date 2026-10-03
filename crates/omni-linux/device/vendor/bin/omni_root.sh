#!/system/bin/sh
# omnidroid: the rooted device's modules, Magisk's post-fs-data / late_start phases.
#
# The host staged the modules into /data/adb/modules/<id>/ (omni_linux::root::install::stage) and
# wrote /data/adb/omni/enabled. That marker only says "run the module scripts": it grants no
# privilege (root is granted by the omni_root syscall, whose trust root is a host-only profile the
# guest cannot reach). Without it -- every device that is not rooted -- this script does nothing.
#
#   post-fs-data  per enabled module: customize.sh, system.prop, post-fs-data.sh; then layer.gen
#   service       per enabled module: service.sh (in the background)

[ -f /data/adb/omni/enabled ] || exit 0

OMNI=/data/adb/omni
MODS=/data/adb/modules
MAGISK=/data/adb/magisk
LOG=$OMNI/install.log
export PATH=$MAGISK:$PATH

say() {
    echo "[omni-root] $*"
    echo "[omni-root] $*" >> $LOG
}

# A module is enabled unless it carries one of Magisk's markers.
enabled() {
    [ -f "$1/module.prop" ] && [ ! -e "$1/disable" ] && [ ! -e "$1/remove" ]
}

# customize.sh under Magisk's installer environment (the pinned util_functions.sh).
customize() {
    (
        MODPATH=$1
        MAGISK_VER=$(cat $OMNI/version 2>/dev/null)
        MAGISK_VER_CODE=$(cat $OMNI/version_code 2>/dev/null)
        BOOTMODE=true
        OUTFD=1
        export MODPATH MAGISK_VER MAGISK_VER_CODE BOOTMODE OUTFD
        [ -f $MAGISK/util_functions.sh ] && . $MAGISK/util_functions.sh
        # Magisk's installer prints through ui_print and fixes modes with set_perm.
        type ui_print >/dev/null 2>&1 || ui_print() { echo "$1"; }
        . "$1/customize.sh"
    ) >> $LOG 2>&1
}

case "$1" in
post-fs-data)
    : > $LOG
    for dir in $MODS/*; do
        enabled "$dir" || continue
        id=${dir##*/}
        say "install $id"
        if [ -f "$dir/customize.sh" ] && [ ! -f "$dir/.omni-customized" ]; then
            customize "$dir" || say "$id: customize.sh failed"
            : > "$dir/.omni-customized"
        fi
        if [ -f "$dir/system.prop" ]; then
            resetprop --file "$dir/system.prop" >> $LOG 2>&1 || say "$id: system.prop failed"
        fi
        if [ -f "$dir/post-fs-data.sh" ]; then
            say "$id: post-fs-data.sh"
            (cd "$dir" && /system/bin/sh ./post-fs-data.sh) >> $LOG 2>&1
        fi
    done
    # The layer is rebuilt from what is installed now.
    touch $OMNI/layer.gen
    say "post-fs-data done"
    ;;
service)
    for dir in $MODS/*; do
        enabled "$dir" || continue
        [ -f "$dir/service.sh" ] || continue
        say "${dir##*/}: service.sh"
        (cd "$dir" && /system/bin/sh ./service.sh) >> $LOG 2>&1 &
    done
    wait
    ;;
esac
exit 0
