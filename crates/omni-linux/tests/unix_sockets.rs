//! Unix-domain sockets bound to names (`crate::unix`): what a service listens on and a client
//! connects to -- init's `/dev/socket/<name>` sockets (netd's dnsproxyd, fwmarkd).
mod common;

use omni_linux::ExitStatus;

#[test]
fn servers_bound_to_names_are_connected_to() {
    let Some((status, out, err)) = common::run_fixture("unixsock", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 11, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}

/// A peer's credentials are its process's: `SO_PEERCRED`, and `SCM_CREDENTIALS` under
/// `SO_PASSCRED` (lmkd refuses a client whose messages carry none).
#[test]
fn a_peer_is_known_by_its_credentials() {
    let Some((status, out, err)) = common::run_fixture("peercred", &[]) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 10, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
