//! Loopback, private per instance and per user (docs/superpowers/specs/2026-09-30-shared-android-
//! multi-instance-design.md, Isolation 2). A guest's TCP and UDP sockets are the host's
//! (`crate::hostnet`), so a guest bind of `127.0.0.1:P` would be the whole machine's port P: two
//! instances collide, and a guest can reach every listener on the host's loopback -- omnidroid's
//! own among them. Here a guest's loopback and wildcard ports are its **namespace's**: the host
//! socket is bound to port 0, and the guest's port is an entry in a table the namespace's host
//! processes share, as small files in `<instance>/.omni-loopback/<ns>/` (the way `crate::xsocket`
//! publishes abstract sockets). A connect or send to a loopback port resolves through the table;
//! a port not in it cannot be reached. An app's namespace is its user's; system uids share one.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::errno::{Errno, EADDRINUSE, EIO};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn tag(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// The namespace `uid`'s sockets are in: an app's (app id 10000 and up, isolated processes
/// included) is its user's, `u<user>`; every other uid -- the system's, in any user -- `sys`.
#[must_use]
pub fn ns_of(uid: u32) -> String {
    if uid % 100_000 >= 10_000 { format!("u{}", uid / 100_000) } else { "sys".to_string() }
}

fn family(raw: &[u8]) -> u16 {
    raw.get(0..2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

/// Whether a guest `sockaddr_in`/`sockaddr_in6` is a loopback address: 127.0.0.0/8, `::1`, or
/// `::ffff:127.0.0.0/104`.
#[must_use]
pub fn is_loopback(raw: &[u8]) -> bool {
    match family(raw) {
        2 => raw.get(4) == Some(&127),
        10 => match raw.get(8..24) {
            Some(a) if a[..15].iter().all(|b| *b == 0) && a[15] == 1 => true,
            Some(a) => a[..10].iter().all(|b| *b == 0) && a[10] == 0xff && a[11] == 0xff && a[12] == 127,
            None => false,
        },
        _ => false,
    }
}

/// Whether it is the wildcard address (`0.0.0.0`, `::`).
#[must_use]
pub fn is_wildcard(raw: &[u8]) -> bool {
    match family(raw) {
        2 => raw.get(4..8).is_some_and(|a| a.iter().all(|b| *b == 0)),
        10 => raw.get(8..24).is_some_and(|a| a.iter().all(|b| *b == 0)),
        _ => false,
    }
}

/// The port of a guest `sockaddr_in`/`sockaddr_in6`.
#[must_use]
pub fn port_of(raw: &[u8]) -> u16 {
    raw.get(2..4).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]))
}

/// The same address with another port.
#[must_use]
pub fn with_port(raw: &[u8], port: u16) -> Vec<u8> {
    let mut out = raw.to_vec();
    if out.len() >= 4 {
        out[2..4].copy_from_slice(&port.to_be_bytes());
    }
    out
}

/// The wildcard address of `domain` (`AF_INET` 2 or `AF_INET6` 10), port 0.
#[must_use]
pub fn wildcard(domain: u16) -> Vec<u8> {
    let mut b = vec![0u8; if domain == 2 { 16 } else { 28 }];
    b[0..2].copy_from_slice(&domain.to_le_bytes());
    b
}

/// One namespace's table.
pub struct Namespace {
    dir: PathBuf,
}

/// An entry, held while its socket is open: dropped, its files go.
pub struct Binding {
    files: Vec<PathBuf>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// An entry's host port (or guest port), its owner, and whether it is shared.
fn read_entry(path: &Path) -> Option<(u16, u32, bool)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut w = text.split_whitespace();
    let port = w.next()?.parse().ok()?;
    let owner = w.next()?.parse().ok()?;
    Some((port, owner, w.next() == Some("s")))
}

/// Whether an entry's owner still runs: this host process, or a live one.
fn live(owner: u32) -> bool {
    owner == std::process::id() || omni_platform::process::is_alive(owner)
}

impl Namespace {
    /// The namespace `ns` of the instance at `instance`.
    #[must_use]
    pub fn open(instance: &Path, ns: &str) -> Arc<Self> {
        let dir = instance.join(".omni-loopback").join(ns);
        let _ = std::fs::create_dir_all(&dir);
        Arc::new(Self { dir })
    }

    fn forward(&self, proto: Proto, guest: u16) -> PathBuf {
        self.dir.join(format!("f-{}-{guest}", proto.tag()))
    }

    fn reverse(&self, proto: Proto, host: u16) -> PathBuf {
        self.dir.join(format!("r-{}-{host}", proto.tag()))
    }

    /// Claim guest port `guest` (0: the host port) for a host socket bound to host port `host`.
    /// `shared`: the socket set `SO_REUSEADDR`, and a port another shared socket holds is joined
    /// (the first keeps it; this one is known by it).
    ///
    /// # Errors
    /// `EADDRINUSE` when a live socket holds the port and they do not both share it; `EIO` when the
    /// table cannot be written.
    pub fn bind(&self, proto: Proto, guest: u16, host: u16, shared: bool) -> Result<Binding, Errno> {
        let guest = if guest == 0 { host } else { guest };
        let me = std::process::id();
        let fwd = self.forward(proto, guest);
        let mut files = Vec::new();
        let body = format!("{host} {me}{}", if shared { " s" } else { "" });
        let mut tries = 0;
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&fwd) {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(body.as_bytes()).map_err(|_| EIO)?;
                    files.push(fwd.clone());
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_entry(&fwd) {
                    Some((_, owner, held_shared)) if live(owner) => {
                        if shared && held_shared {
                            break; // joined: the first keeps the forward entry
                        }
                        return Err(EADDRINUSE);
                    }
                    // Stale (its owner is gone) or unreadable: taken over, once.
                    _ if tries == 0 => {
                        let _ = std::fs::remove_file(&fwd);
                        tries += 1;
                    }
                    _ => return Err(EADDRINUSE),
                },
                Err(_) => return Err(EIO),
            }
        }
        let rev = self.reverse(proto, host);
        std::fs::write(&rev, format!("{guest} {me}")).map_err(|_| EIO)?;
        files.push(rev);
        Ok(Binding { files })
    }

    /// The host port behind guest port `guest`, if a live socket of this namespace holds it.
    #[must_use]
    pub fn lookup(&self, proto: Proto, guest: u16) -> Option<u16> {
        let (host, owner, _) = read_entry(&self.forward(proto, guest))?;
        live(owner).then_some(host)
    }

    /// The guest port a host port of this namespace is known by -- `None` for a host port that
    /// is not this namespace's (another instance's, or a program's of the host).
    #[must_use]
    pub fn guest_port(&self, proto: Proto, host: u16) -> Option<u16> {
        let (guest, owner, _) = read_entry(&self.reverse(proto, host))?;
        live(owner).then_some(guest)
    }
}

/// Make host port `host` reachable as guest port `guest` in namespace `ns` of `instance`, for as
/// long as the returned binding is held: a host service handed to one namespace on purpose (a
/// test's server; an embedding's). Owned by this host process.
///
/// # Errors
/// As [`Namespace::bind`].
pub fn expose(instance: &Path, ns: &str, proto: Proto, guest: u16, host: u16) -> Result<Binding, Errno> {
    Namespace::open(instance, ns).bind(proto, guest, host, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("omni-loopns-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn v4(a: [u8; 4], port: u16) -> Vec<u8> {
        let mut b = vec![0u8; 16];
        b[0..2].copy_from_slice(&2u16.to_le_bytes());
        b[2..4].copy_from_slice(&port.to_be_bytes());
        b[4..8].copy_from_slice(&a);
        b
    }

    fn v6(a: [u8; 16], port: u16) -> Vec<u8> {
        let mut b = vec![0u8; 28];
        b[0..2].copy_from_slice(&10u16.to_le_bytes());
        b[2..4].copy_from_slice(&port.to_be_bytes());
        b[8..24].copy_from_slice(&a);
        b
    }

    #[test]
    fn an_apps_namespace_is_its_users_and_system_uids_share_one() {
        assert_eq!(ns_of(10_115), "u0");
        assert_eq!(ns_of(1_010_115), "u10");
        assert_eq!(ns_of(1_099_001), "u10"); // an isolated process of user 10
        assert_eq!(ns_of(1000), "sys");
        assert_eq!(ns_of(1_001_000), "sys"); // user 10's system uid
        assert_eq!(ns_of(0), "sys");
    }

    #[test]
    fn loopback_and_wildcard_are_told_apart() {
        assert!(is_loopback(&v4([127, 0, 0, 1], 80)));
        assert!(is_loopback(&v4([127, 9, 9, 9], 80)));
        let mut one = [0u8; 16];
        one[15] = 1;
        assert!(is_loopback(&v6(one, 80)));
        let mut mapped = [0u8; 16];
        mapped[10] = 0xff;
        mapped[11] = 0xff;
        mapped[12..].copy_from_slice(&[127, 0, 0, 1]);
        assert!(is_loopback(&v6(mapped, 80)));
        assert!(!is_loopback(&v4([10, 0, 0, 1], 80)));
        assert!(is_wildcard(&v4([0; 4], 80)) && is_wildcard(&v6([0; 16], 80)));
        assert!(!is_wildcard(&v4([127, 0, 0, 1], 80)));
        assert_eq!(port_of(&v4([127, 0, 0, 1], 4242)), 4242);
        assert_eq!(port_of(&with_port(&v6(one, 1), 5353)), 5353);
        assert_eq!(wildcard(2), v4([0; 4], 0));
        assert_eq!(wildcard(10), v6([0; 16], 0));
    }

    #[test]
    fn a_bound_port_is_found_by_its_namespace_only() {
        let d = dir("find");
        let u0 = Namespace::open(&d, "u0");
        let u10 = Namespace::open(&d, "u10");
        let held = u0.bind(Proto::Tcp, 47000, 51234, false).expect("bind");
        assert_eq!(u0.lookup(Proto::Tcp, 47000), Some(51234));
        assert_eq!(u0.guest_port(Proto::Tcp, 51234), Some(47000));
        assert_eq!(u10.lookup(Proto::Tcp, 47000), None, "another namespace does not see it");
        assert_eq!(u0.lookup(Proto::Udp, 47000), None, "nor does the other protocol");
        // The same guest port is free in another namespace.
        let other = u10.bind(Proto::Tcp, 47000, 51235, false).expect("u10's own 47000");
        assert_eq!(u10.lookup(Proto::Tcp, 47000), Some(51235));
        drop(held);
        assert_eq!(u0.lookup(Proto::Tcp, 47000), None, "released with its binding");
        assert_eq!(u0.guest_port(Proto::Tcp, 51234), None);
        drop(other);
    }

    #[test]
    fn a_port_is_one_sockets_at_a_time() {
        let d = dir("twice");
        let ns = Namespace::open(&d, "u0");
        let _held = ns.bind(Proto::Tcp, 47001, 50001, false).expect("first");
        assert_eq!(ns.bind(Proto::Tcp, 47001, 50002, false).err(), Some(EADDRINUSE));
    }

    #[test]
    fn port_zero_is_the_host_port() {
        let d = dir("zero");
        let ns = Namespace::open(&d, "u0");
        let _held = ns.bind(Proto::Udp, 0, 50003, false).expect("bind");
        assert_eq!(ns.lookup(Proto::Udp, 50003), Some(50003));
        assert_eq!(ns.guest_port(Proto::Udp, 50003), Some(50003));
    }

    #[test]
    fn a_dead_owners_port_is_taken_over() {
        let d = dir("dead");
        let ns = Namespace::open(&d, "u0");
        // An entry left by a host process that has ended (a crashed app host).
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(d.join(".omni-loopback/u0/f-tcp-47002"), format!("50004 {dead}")).unwrap();
        assert_eq!(ns.lookup(Proto::Tcp, 47002), None, "a dead owner's entry is not found");
        let _held = ns.bind(Proto::Tcp, 47002, 50005, false).expect("taken over");
        assert_eq!(ns.lookup(Proto::Tcp, 47002), Some(50005));
    }

    #[test]
    fn a_shared_udp_port_takes_a_second_socket() {
        let d = dir("shared");
        let ns = Namespace::open(&d, "sys");
        let first = ns.bind(Proto::Udp, 5353, 50006, true).expect("first");
        let second = ns.bind(Proto::Udp, 5353, 50007, true).expect("second, shared");
        assert_eq!(ns.lookup(Proto::Udp, 5353), Some(50006), "the first keeps the port");
        assert_eq!(ns.guest_port(Proto::Udp, 50007), Some(5353), "the second is known by its port");
        // Not shared by the newcomer: refused.
        assert_eq!(ns.bind(Proto::Udp, 5353, 50008, false).err(), Some(EADDRINUSE));
        drop((first, second));
    }

    #[test]
    fn an_exposed_host_port_is_reachable_in_that_namespace_only() {
        let d = dir("expose");
        let _e = expose(&d, "u0", Proto::Tcp, 8080, 50009).expect("expose");
        assert_eq!(Namespace::open(&d, "u0").lookup(Proto::Tcp, 8080), Some(50009));
        assert_eq!(Namespace::open(&d, "u10").lookup(Proto::Tcp, 8080), None);
    }
}
