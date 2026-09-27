//! A kernel uevent socket (`AF_NETLINK`, `NETLINK_KOBJECT_UEVENT`): it binds, is named by the
//! port the kernel assigns, and has nothing to read while no device comes or goes. ueventd aborts
//! and vold ends without one.
mod common;

use omni_linux::ExitStatus;

#[test]
fn a_uevent_socket_binds_and_waits_for_events() {
    let Some((status, out, err)) = common::run_fixture("netlink", &[]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
    assert_eq!(out.trim(), "netlink ok pid =getpid", "{err}");
}

/// Routing netlink, as the image's `ip` reads it: the one interface is the loopback; a rule
/// added is acknowledged.
#[test]
fn ip_lists_the_loopback_and_adds_a_rule() {
    let Some(runs) = common::run_each(&[&["/system/bin/ip", "link"], &["/system/bin/ip", "rule", "add", "from", "all", "fwmark", "0x1", "lookup", "main", "pref", "9000"]]) else { return };
    assert_eq!(runs[0].0, ExitStatus::Exited(0), "{:?}", runs[0]);
    assert!(runs[0].1.contains("1: lo:"), "{:?}", runs[0]);
    assert_eq!(runs[1].0, ExitStatus::Exited(0), "{:?}", runs[1]);
}
