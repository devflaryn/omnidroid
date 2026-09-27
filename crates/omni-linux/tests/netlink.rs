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
