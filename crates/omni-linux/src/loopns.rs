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
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::errno::{Errno, EADDRINUSE, EIO};

/// Process-wide counter for unique temp file names across threads and processes.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Get or create a per-namespace lock.
fn get_namespace_lock(dir: &Path) -> Arc<Mutex<()>> {
    use std::collections::HashMap;
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut lock_map = locks.lock().unwrap();
    Arc::clone(
        lock_map
            .entry(dir.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

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
    namespace_lock: Option<Arc<Mutex<()>>>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        // Take lock to safely remove entries.
        if let Some(lock_arc) = self.namespace_lock.as_ref() {
            let _lock = lock_arc.lock().unwrap();
            let me = std::process::id();
            for f in &self.files {
                // Only remove if we still own it: verify the pid in the entry.
                if let Some((_, owner, _)) = read_entry(f) {
                    if owner == me {
                        let _ = std::fs::remove_file(f);
                    }
                }
            }
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

/// Atomically write content to a file via temp + rename.
fn atomic_write(path: &Path, content: &str) -> Result<(), Errno> {
    let counter = WRITE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let tmp = path.parent().ok_or(EIO)?
        .join(format!(".tmp-{}-{}", std::process::id(), counter));
    std::fs::write(&tmp, content).map_err(|_| EIO)?;
    std::fs::rename(&tmp, path).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        EIO
    })?;
    Ok(())
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
        let rev = self.reverse(proto, host);
        let fwd_body = format!("{host} {me}{}", if shared { " s" } else { "" });
        let rev_body = format!("{guest} {me}");

        // Get the lock Arc for this namespace.
        let lock_arc = get_namespace_lock(&self.dir);
        let _lock = lock_arc.lock().unwrap();

        // Critical section: read, decide, write under lock.
        // Read forward entry if it exists.
        match read_entry(&fwd) {
            Some((_, owner, held_shared)) => {
                if live(owner) {
                    // Live owner: check for shared join.
                    if shared && held_shared {
                        // Build binding without forward entry (joined).
                        atomic_write(&rev, &rev_body)?;
                        return Ok(Binding {
                            files: vec![rev],
                            namespace_lock: Some(Arc::clone(&lock_arc)),
                        });
                    }
                    return Err(EADDRINUSE);
                } else {
                    // Stale: replace it.
                    atomic_write(&fwd, &fwd_body)?;
                    atomic_write(&rev, &rev_body)?;
                    return Ok(Binding {
                        files: vec![fwd, rev],
                        namespace_lock: Some(Arc::clone(&lock_arc)),
                    });
                }
            }
            None => {
                // Unreadable: treat as corrupt/stale, replace it.
                atomic_write(&fwd, &fwd_body)?;
                atomic_write(&rev, &rev_body)?;
                return Ok(Binding {
                    files: vec![fwd, rev],
                    namespace_lock: Some(Arc::clone(&lock_arc)),
                });
            }
        }
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

    #[test]
    fn one_guest_port_has_one_winner_under_concurrent_binds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;
        use std::sync::Arc as StdArc;

        let d = dir("concurrent");
        let ns = Namespace::open(&d, "u0");
        let barrier = StdArc::new(Barrier::new(8));
        let success_count = StdArc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for i in 0..8 {
            let ns_clone = Arc::clone(&ns);
            let barrier_clone = StdArc::clone(&barrier);
            let success_clone = StdArc::clone(&success_count);
            let handle = std::thread::spawn(move || {
                barrier_clone.wait(); // synchronize all threads
                let host_port = 51240 + i as u16;
                match ns_clone.bind(Proto::Tcp, 47020, host_port, false) {
                    Ok(_binding) => {
                        success_clone.fetch_add(1, Ordering::SeqCst);
                        // Hold the binding for a bit to ensure atomicity is observable.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(EADDRINUSE) => {
                        // expected for losers
                    }
                    Err(_) => panic!("unexpected error"),
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }

        // Exactly one should win: core atomicity property.
        assert_eq!(
            success_count.load(Ordering::SeqCst),
            1,
            "exactly one bind succeeded among 8 concurrent attempts"
        );
    }

    #[test]
    fn shared_drop_does_not_remove_first_binding() {
        let d = dir("shared-drop");
        let ns = Namespace::open(&d, "sys");
        let first = ns.bind(Proto::Udp, 5354, 50010, true).expect("first");
        let second = ns.bind(Proto::Udp, 5354, 50011, true).expect("second, shared");
        assert_eq!(ns.lookup(Proto::Udp, 5354), Some(50010), "the first keeps the port");
        drop(second);
        // After dropping second, first's binding and lookup should still work.
        assert_eq!(ns.lookup(Proto::Udp, 5354), Some(50010), "first still reachable after second drops");
        assert_eq!(ns.guest_port(Proto::Udp, 50011), None, "second's reverse entry is removed");
        drop(first);
    }

    #[test]
    fn shared_bind_rejects_non_shared_holder() {
        let d = dir("shared-vs-nonshared");
        let ns = Namespace::open(&d, "u0");
        let _held = ns.bind(Proto::Tcp, 47021, 50012, false).expect("non-shared");
        assert_eq!(ns.bind(Proto::Tcp, 47021, 50013, true).err(), Some(EADDRINUSE));
    }

    #[test]
    fn a_taken_over_entry_is_not_removed_by_its_old_binding() {
        let d = dir("takeover-keeps-new");
        let ns = Namespace::open(&d, "u0");
        let binding = ns.bind(Proto::Tcp, 47022, 50014, false).expect("bind");

        // Simulate takeover: overwrite forward file with a live process's pid.
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "ping -n 5 127.0.0.1 >nul"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("sleep").arg("5").spawn().unwrap()
        };
        let child_pid = child.id();
        let fwd = d.join(".omni-loopback/u0/f-tcp-47022");
        std::fs::write(&fwd, format!("50015 {child_pid}")).unwrap();

        // Drop original binding: it should NOT remove the entry (new owner).
        drop(binding);
        assert!(fwd.exists(), "forward entry still exists after original binding drops");

        // Verify it still has the new owner.
        let (_, owner, _) = read_entry(&fwd).expect("entry readable");
        assert_eq!(owner, child_pid, "entry has new owner");

        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn dead_owner_takeover_updates_reverse_entry() {
        let d = dir("takeover-reverse");
        let ns = Namespace::open(&d, "u0");
        // Leave a dead owner's entry.
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(d.join(".omni-loopback/u0/f-tcp-47023"), format!("50016 {dead}")).unwrap();

        // Bind same guest port with different host port.
        let _held = ns.bind(Proto::Tcp, 47023, 50017, false).expect("taken over");

        // Lookup should find the new host port (not the old dead owner's).
        assert_eq!(ns.lookup(Proto::Tcp, 47023), Some(50017), "lookup returns new host port");
        // Reverse entry should map to new host port.
        assert_eq!(ns.guest_port(Proto::Tcp, 50017), Some(47023), "reverse entry is new");
        // Old reverse entry should not exist or be stale.
        assert_eq!(ns.guest_port(Proto::Tcp, 50016), None, "old reverse entry is gone");
    }
}
