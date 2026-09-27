//! Internet sockets on a machine whose only interface is lo (`crate::inet`): the wildcard and
//! loopback addresses bind, port 0 is given an ephemeral port, a port is one socket's at a time,
//! and an address no interface has is refused -- what system_server's MulticastSocket needs.
mod common;

use omni_linux::ExitStatus;

#[test]
fn inet_sockets_bind_as_a_loopback_only_kernel_binds_them() {
    let Some((status, out, err)) = common::run_fixture("inetbind", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 10, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
