# Shared Android — Phase 0: the guest network boundary — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A guest can no longer name a real host loopback port, loopback is private per (system, user), and the binder listener takes a process's identity from a credential it issued, never from what a frame claims.

**Architecture:** A new `crate::loopns` keeps each namespace's port table as small files in `<instance>/.omni-loopback/<ns>/`, shared by every host process of the instance, as `crate::xsocket` publishes abstract sockets. `crate::hostnet::Host` translates every loopback or wildcard address through its socket's namespace: a guest port is bound on the host as port 0, connects and sends resolve through the table, and accepted connections and datagrams from outside the namespace are dropped. `crate::remote` issues a random credential per app host (the zygote hands it over on the child's stdin) and binds the stand-in's pid and uid to it.

**Tech Stack:** Rust (workspace crates `omni-linux`, `omni-platform`), `windows-sys` / `libc`, Android NDK r28c for one guest C fixture.

**Spec:** `docs/superpowers/specs/2026-09-30-shared-android-multi-instance-design.md` (Isolation 1 and 2; Phases and gates, phase 0).

## Global Constraints

- The repo is at `~/Desktop/Omni Apps/omnidroid` on every host: **quote the path** (it has a space).
- Build and test on Windows in **PowerShell** with `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'` set in the same command, `--release` (the Bash tool mangles that variable). Example: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --lib loopns`.
- Multi-line files and scripts are written with the Write tool, never a shell heredoc (heredocs truncate and eat backslash-newlines).
- From Bash, `export MSYS_NO_PATHCONV=1` before running `omni-linux-run` with guest paths.
- The guest is always arm64; host-agnostic code must build and pass on Windows x64, Linux x64, macOS arm64.
- Namespaces: an app uid (`uid % 100000 >= 10000`) is in `u<uid / 100000>`; every other uid is in `sys`.
- Guest loopback = 127.0.0.0/8, `::1`, `::ffff:127.0.0.0/104`; wildcard = `0.0.0.0`, `::`.
- A guest bind to loopback or the wildcard binds the host socket to **the same address with port 0**; the guest sees the port it asked for (or the host's port when it asked for 0).
- A process without an instance directory (the HLE embedding) keeps today's behaviour exactly.
- Omnidroid's own host-to-host sockets (`relay.rs`, `xsocket.rs`, `remote.rs`, control endpoints) use `std::net` and are not touched.
- Commits end with the two trailer lines:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01Pz5iSt5A1TGvEq1PZudR7v`.
- Work on branch `feat/shared-android`.

## Review Focus

1. **A guest server restarted after its host process crashed** re-binds its fixed port: the dead owner's entry must be taken over, not answered `EADDRINUSE` (Task 2 test `a_dead_owners_port_is_taken_over`).
2. **UDP request/response over loopback between two processes of one namespace** — an unbound client's reply address must resolve (Task 3 fixture checks `udp reply to an unbound client`); and a UDP socket connected before its peer binds must reach the peer once it does, as on Linux (`a peer that binds later is reached`).
3. **Two sockets sharing a UDP port with `SO_REUSEADDR`** (mDNS-style) — the second bind succeeds, the first keeps the port (Task 2 test `a_shared_udp_port_takes_a_second_socket`).
4. **An IPv6 dual-stack guest server reached from an IPv4 guest client over 127.0.0.1** (Task 3 fixture checks `v4 client reaches a v6 wildcard server`).
5. **An app host whose credential was never issued, or was revoked when its process ended**, connects: the listener closes the connection and makes no stand-in (Task 5 test `an_unknown_credential_is_refused`).

---

### Task 1: `omni_platform::process::is_alive`

**Files:**
- Modify: `crates/omni-platform/src/process/mod.rs` (add the public fn after `pid()`, ~line 97)
- Modify: `crates/omni-platform/src/process/windows.rs`, `linux.rs`, `macos.rs`, `unix.rs` (backend fns)

**Interfaces:**
- Produces: `pub fn omni_platform::process::is_alive(pid: u32) -> bool` — `true` while a process with that id exists (including one this user may not open).

- [ ] **Step 1: Write the failing test** — append to the `#[cfg(test)] mod tests` of `crates/omni-platform/src/process/mod.rs` (create the module at the end of the file if it has none):

```rust
#[cfg(test)]
mod is_alive_tests {
    #[test]
    fn this_process_is_alive_and_an_ended_child_is_not() {
        assert!(super::is_alive(std::process::id()));
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let pid = child.id();
        child.wait().unwrap();
        // Reaped: the id names no process (a reuse this fast is not a real risk in a test).
        assert!(!super::is_alive(pid));
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run (PowerShell): `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-platform is_alive`
Expected: compile error `cannot find function is_alive`.

- [ ] **Step 3: Implement**

In `mod.rs`, after `pid()`:

```rust
/// Whether a process with id `pid` exists now -- one this user cannot open counts as existing.
/// For a record another host process left (`omni_linux::loopns`): its owner is gone when this is
/// false. A reused id answers true; the caller treats that as "still held", never as "free".
#[must_use]
pub fn is_alive(pid: u32) -> bool {
    backend::is_alive(pid)
}
```

In `windows.rs`:

```rust
pub(super) fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: OpenProcess takes plain values; the handle is closed below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        // SAFETY: no arguments.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut code = 0u32;
    // SAFETY: `h` is a live process handle and `code` a valid out pointer.
    let ok = unsafe { GetExitCodeProcess(h, &mut code) } != 0;
    // SAFETY: `h` came from OpenProcess and is closed once.
    unsafe { CloseHandle(h) };
    ok && code == STILL_ACTIVE as u32
}
```

(If this crate's `windows-sys` version types `HANDLE` as `isize`, the null test is `h == 0`; match the style of the other `OpenProcess`-free handle code in this file.)

In `linux.rs`, `macos.rs` and `unix.rs` (the same body in each):

```rust
pub(super) fn is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    // SAFETY: signal 0 delivers nothing; it only asks whether the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-platform is_alive`
Expected: `test process::is_alive_tests::this_process_is_alive_and_an_ended_child_is_not ... ok`

- [ ] **Step 5: Commit**

```bash
git add crates/omni-platform/src/process
git commit -m "feat(platform): process::is_alive -- whether a pid names a live process"
```

---

### Task 2: `crate::loopns` — namespaces and their port tables

**Files:**
- Create: `crates/omni-linux/src/loopns.rs`
- Modify: `crates/omni-linux/src/lib.rs` (add `pub mod loopns;` beside `pub mod inet;`)

**Interfaces:**
- Consumes: `omni_platform::process::is_alive(u32) -> bool` (Task 1).
- Produces (used by Tasks 3 and 4):
  - `pub enum Proto { Tcp, Udp }`
  - `pub fn ns_of(uid: u32) -> String`
  - `pub fn is_loopback(raw: &[u8]) -> bool`, `pub fn is_wildcard(raw: &[u8]) -> bool`
  - `pub fn port_of(raw: &[u8]) -> u16`, `pub fn with_port(raw: &[u8], port: u16) -> Vec<u8>`, `pub fn wildcard(domain: u16) -> Vec<u8>`
  - `pub struct Namespace` with `pub fn open(instance: &Path, ns: &str) -> Arc<Namespace>`, `pub fn bind(&self, proto: Proto, guest: u16, host: u16, shared: bool) -> Result<Binding, Errno>`, `pub fn lookup(&self, proto: Proto, guest: u16) -> Option<u16>`, `pub fn guest_port(&self, proto: Proto, host: u16) -> Option<u16>`
  - `pub struct Binding` (dropping it releases its entries)
  - `pub fn expose(instance: &Path, ns: &str, proto: Proto, guest: u16, host: u16) -> Result<Binding, Errno>`

Entry files in `<instance>/.omni-loopback/<ns>/`:
- `f-<tcp|udp>-<guest port>` holds `<host port> <owner host pid>[ s]` (`s`: shared, `SO_REUSEADDR`)
- `r-<tcp|udp>-<host port>` holds `<guest port> <owner host pid>`

A forward entry is made with create-new (exclusive), so two host processes binding one guest port race safely. An entry whose owner is not alive is stale: a bind takes it over, and a lookup ignores it.

- [ ] **Step 1: Write the failing tests** — create `crates/omni-linux/src/loopns.rs` with only the test module:

```rust
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
```

- [ ] **Step 2: Add the module and run the tests to verify they fail**

Add `pub mod loopns;` to `crates/omni-linux/src/lib.rs` next to `pub mod inet;`.
Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --lib loopns`
Expected: compile errors (`ns_of`, `Namespace`, ... not found).

- [ ] **Step 3: Implement** — put this above the test module in `loopns.rs`:

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --lib loopns`
Expected: 8 tests, all `ok`.

- [ ] **Step 5: Commit**

```bash
git add crates/omni-linux/src/loopns.rs crates/omni-linux/src/lib.rs
git commit -m "feat(net): loopback namespaces per (instance, user) -- the port tables"
```

---

### Task 3: `hostnet::Host` translates loopback through its namespace

**Files:**
- Modify: `crates/omni-linux/src/hostnet.rs` (`Host` struct ~225, `create` ~252, `adopt` ~263, `bind` ~284, `connect` ~302, `listen` ~391, `accept` ~416, `send_inner` ~500, `recv` ~556, `name` ~707, `peer` ~716)
- Modify: `crates/omni-linux/src/socket.rs:283-285` (`sys_socket` passes the namespace)
- Create: `crates/omni-linux/tests/fixtures/loopiso.c`, its binary `loopiso`, entries in `tests/fixtures/build.txt` and `tests/fixtures/SHA256SUMS`
- Create: `crates/omni-linux/tests/loopback_ns.rs`
- Modify: `crates/omni-linux/tests/common/mod.rs` (add `run_fixture_as`)

**Interfaces:**
- Consumes: everything `crate::loopns` produces (Task 2).
- Produces: `pub fn Host::create(domain: u16, stream: bool, ns: Option<Arc<crate::loopns::Namespace>>) -> Result<Arc<Host>, Errno>`; `pub fn loopns::Namespace::for_process(p: &crate::Process) -> Option<Arc<Namespace>>`; test helper `common::run_fixture_as(instance: &Path, name: &str, args: &[&str], uid: u32) -> Option<(ExitStatus, String, String)>`.

Rules implemented here (from the spec):

| Guest call | With a namespace |
|---|---|
| `bind(loopback or wildcard, P)` | host bind to the same address, port 0; `ns.bind(proto, P, host port, SO_REUSEADDR)`; on `EADDRINUSE` the host socket is replaced by a fresh unbound one |
| `listen` unbound (TCP) | first binds the wildcard, port 0, as above |
| `connect(loopback, P)` TCP | `ns.lookup(Tcp, P)`; none -> `ECONNREFUSED`; an unbound socket first binds the same loopback address, port 0, so its peer can resolve it; then connects to the same address, host port |
| `connect(loopback, P)` UDP | as Linux: it only names the peer and succeeds (an unbound socket first binds the wildcard, port 0); the host socket is not connected. Each `send` resolves the peer again: a port no one holds yet drops the datagram, a peer that binds later is reached. `recv` takes only the peer's datagrams |
| `sendto(loopback, P)` (UDP) | lookup; none -> the datagram is dropped (`Ok(len)`, as to a closed port); an unbound socket first binds the wildcard, port 0 |
| `accept` | a connection whose peer is not loopback, or whose port is not this namespace's, is closed and the wait goes on |
| `recvfrom` (UDP) | a datagram from a loopback port not of this namespace is dropped and the wait goes on; a loopback source's port is shown as its guest port |
| `getsockname` / `getpeername` | the guest port where one was translated |

- [ ] **Step 1: Write the guest fixture** — create `crates/omni-linux/tests/fixtures/loopiso.c`:

```c
// Loopback private per namespace (crate::loopns): run as `loopiso server <port>` it listens on
// 127.0.0.1:<port> (TCP) and 127.0.0.1:<port> (UDP), prints "ready", answers one TCP client with
// "hi" and echoes UDP datagrams for 20 s. Run as `loopiso client <port> <expect: reach|refused>`
// it checks what reaching that port does; `loopiso self` runs the single-process checks.
// Prints "ok ..." or "FAIL ...".
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); fflush(stdout); if (!ok) failed = 1; }

static struct sockaddr_in lo(int port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

static int server(int port) {
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(port);
    check(bind(t, (struct sockaddr*)&a, sizeof(a)) == 0 && listen(t, 4) == 0, "server tcp bind+listen");
    struct sockaddr_in got;
    socklen_t len = sizeof(got);
    check(getsockname(t, (struct sockaddr*)&got, &len) == 0 && ntohs(got.sin_port) == port, "getsockname shows the guest port");
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    check(bind(u, (struct sockaddr*)&a, sizeof(a)) == 0, "server udp bind");
    printf("ready\n");
    fflush(stdout);
    struct pollfd fds[2] = {{t, POLLIN, 0}, {u, POLLIN, 0}};
    for (int i = 0; i < 200; i++) {
        if (poll(fds, 2, 100) <= 0) continue;
        if (fds[0].revents & POLLIN) {
            int c = accept(t, NULL, NULL);
            if (c >= 0) { write(c, "hi", 2); close(c); }
        }
        if (fds[1].revents & POLLIN) {
            char buf[64];
            struct sockaddr_in from;
            socklen_t flen = sizeof(from);
            ssize_t n = recvfrom(u, buf, sizeof(buf), 0, (struct sockaddr*)&from, &flen);
            if (n > 0) sendto(u, buf, n, 0, (struct sockaddr*)&from, flen);
        }
    }
    return 0;
}

static int client(int port, int reach) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(port);
    int r = connect(c, (struct sockaddr*)&a, sizeof(a));
    if (reach) {
        char buf[4] = {0};
        check(r == 0 && read(c, buf, 2) == 2 && memcmp(buf, "hi", 2) == 0, "tcp: reaches the server");
        int u = socket(AF_INET, SOCK_DGRAM, 0); // unbound: its reply address must still resolve
        check(sendto(u, "ping", 4, 0, (struct sockaddr*)&a, sizeof(a)) == 4, "udp send");
        struct pollfd p = {u, POLLIN, 0};
        char got[8] = {0};
        check(poll(&p, 1, 5000) == 1 && recv(u, got, sizeof(got), 0) == 4 && memcmp(got, "ping", 4) == 0, "udp reply to an unbound client");
    } else {
        check(r == -1 && errno == ECONNREFUSED, "tcp: refused");
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        sendto(u, "ping", 4, 0, (struct sockaddr*)&a, sizeof(a));
        struct pollfd p = {u, POLLIN, 0};
        check(poll(&p, 1, 1500) == 0, "udp: nothing comes back");
        // This namespace's own server may take the same port.
        int t = socket(AF_INET, SOCK_STREAM, 0);
        check(bind(t, (struct sockaddr*)&a, sizeof(a)) == 0 && listen(t, 1) == 0, "the same port is free here");
    }
    return 0;
}

static int self_checks(void) {
    // A v6 dual-stack wildcard server, reached by a v4 client over 127.0.0.1.
    int s6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 w6;
    memset(&w6, 0, sizeof(w6));
    w6.sin6_family = AF_INET6;
    w6.sin6_port = htons(47111);
    w6.sin6_addr = in6addr_any;
    check(bind(s6, (struct sockaddr*)&w6, sizeof(w6)) == 0 && listen(s6, 1) == 0, "v6 wildcard server");
    int c4 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(47111);
    check(connect(c4, (struct sockaddr*)&a, sizeof(a)) == 0, "v4 client reaches a v6 wildcard server");
    struct sockaddr_in peer;
    socklen_t plen = sizeof(peer);
    check(getpeername(c4, (struct sockaddr*)&peer, &plen) == 0 && ntohs(peer.sin_port) == 47111, "getpeername shows the guest port");
    int two = socket(AF_INET6, SOCK_STREAM, 0);
    check(bind(two, (struct sockaddr*)&w6, sizeof(w6)) == -1 && errno == EADDRINUSE, "a held port: EADDRINUSE");
    // UDP connect names a peer, as Linux: it succeeds before the peer binds, the early send is
    // dropped, and a peer that binds later is reached.
    int uc = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in p2 = lo(47112);
    check(connect(uc, (struct sockaddr*)&p2, sizeof(p2)) == 0 && send(uc, "x", 1, 0) == 1, "udp connect to an unheld port succeeds; the send is dropped");
    int us = socket(AF_INET, SOCK_DGRAM, 0);
    bind(us, (struct sockaddr*)&p2, sizeof(p2));
    send(uc, "y", 1, 0);
    struct pollfd pf = {us, POLLIN, 0};
    char g1[2] = {0};
    check(poll(&pf, 1, 3000) == 1 && recv(us, g1, 1, 0) == 1 && g1[0] == 'y', "a peer that binds later is reached");
    return 0;
}

int main(int argc, char** argv) {
    if (argc >= 3 && strcmp(argv[1], "server") == 0) server(atoi(argv[2]));
    else if (argc >= 4 && strcmp(argv[1], "client") == 0) client(atoi(argv[2]), strcmp(argv[3], "reach") == 0);
    else if (argc >= 2 && strcmp(argv[1], "self") == 0) self_checks();
    else { printf("FAIL usage\n"); return 2; }
    return failed;
}
```

Build it (Git Bash, from `crates/omni-linux/tests/fixtures`):

```bash
~/android-ndk/android-ndk-r28c/toolchains/llvm/prebuilt/windows-x86_64/bin/aarch64-linux-android35-clang.cmd -O2 -Wall -Wextra -Werror -o loopiso loopiso.c
sha256sum loopiso >> SHA256SUMS
```

Append to `build.txt`, in the block of the other `-Werror` fixtures:

```
    <ndk>/toolchains/llvm/prebuilt/windows-x86_64/bin/aarch64-linux-android35-clang.cmd -O2 -Wall -Wextra -Werror -o loopiso loopiso.c
```

- [ ] **Step 2: Add the fixture runner for a uid and an instance** — in `crates/omni-linux/tests/common/mod.rs`, beside `run_fixture`:

```rust
/// `run_fixture` in a given instance directory, as `uid` (a loopback namespace is the uid's).
pub fn run_fixture_as(instance: &Path, name: &str, args: &[&str], uid: u32) -> Option<(ExitStatus, String, String)> {
    let Some(sysroot) = sysroot() else {
        eprintln!("SKIPPED: no sysroot (tools/make_sysroot.py, plan Task 1)");
        return None;
    };
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("the instance's /data/local/tmp");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let _ = std::fs::copy(&fixture, tmp.join(name));
    let guest = format!("/data/local/tmp/{name}");
    let mut argv = vec![guest.as_bytes().to_vec()];
    argv.extend(args.iter().map(|a| a.as_bytes().to_vec()));
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot,
            instance_dir: instance.to_path_buf(),
            argv,
            envp: vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec()],
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::clone(&err)),
            trace: false,
        },
        uid,
    )
    .expect("spawn");
    let status = p.run();
    let text = |b: &Arc<parking_lot::Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock()).into_owned();
    Some((status, text(&out), text(&err)))
}
```

(`Path`, `PathBuf`, `Arc`, `Process`, `SpawnConfig`, `Output`, `ExitStatus` are already imported by `common/mod.rs` for `run_in`; add any that are not.)

- [ ] **Step 3: Write the failing integration test** — create `crates/omni-linux/tests/loopback_ns.rs`:

```rust
//! Loopback private per (instance, user) (`omni_linux::loopns`, `crate::hostnet`): a guest server
//! on 127.0.0.1 is reached by its own namespace, refused to another -- which may take the same
//! port -- and a real host listener cannot be named at all unless it is exposed on purpose.
mod common;

use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;

use omni_linux::loopns::{expose, Proto};
use omni_linux::ExitStatus;

fn instance(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("omni-loopns-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A guest server in `u0` (uid 10000), running beside the test for 20 s.
fn server(inst: &std::path::Path, port: &'static str) {
    let inst = inst.to_path_buf();
    std::thread::spawn(move || common::run_fixture_as(&inst, "loopiso", &["server", port], 10_000));
    std::thread::sleep(Duration::from_secs(3));
}

#[test]
fn a_namespace_reaches_its_own_server() {
    let inst = instance("own");
    server(&inst, "47100");
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47100", "reach"], 10_001) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 3, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
}

#[test]
fn another_users_namespace_is_refused_and_may_take_the_same_port() {
    let inst = instance("other");
    server(&inst, "47101");
    // User 10's app: another namespace.
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", "47101", "refused"], 1_010_000) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 3, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
}

#[test]
fn a_real_host_listener_cannot_be_named_unless_exposed() {
    let inst = instance("host");
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut c = c;
            let _ = c.write_all(b"hi");
        }
    });
    let p = port.to_string();
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", &p, "refused"], 10_000) else { return };
    assert!(out.contains("ok tcp: refused"), "a host port is not reachable: {out}\n{err}");
    let _exposed = expose(&inst, "u0", Proto::Tcp, port, port).expect("expose");
    let Some((_, out, err)) = common::run_fixture_as(&inst, "loopiso", &["client", &p, "reach"], 10_000) else { return };
    assert!(out.contains("ok tcp: reaches the server"), "exposed, it is: {out}\n{err}");
}

#[test]
fn ports_translate_for_dual_stack_and_a_held_port_is_in_use() {
    let inst = instance("self");
    let Some((status, out, err)) = common::run_fixture_as(&inst, "loopiso", &["self"], 10_000) else { return };
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 6, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0));
}
```

(The UDP check in the `reach` client needs the server's reply to reach an unbound client: that is `recv`'s reverse lookup of the client's implicit wildcard binding, which `send_inner` registers.)

- [ ] **Step 4: Run it to verify it fails**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --test loopback_ns`
Expected: `another_users_namespace_is_refused...` and `a_real_host_listener...` FAIL (today the other namespace and the host port are reached); `ports_translate...` may pass or fail on `getpeername`.

- [ ] **Step 5: Implement the namespace hook for a process** — append to `loopns.rs` (above the tests):

```rust
impl Namespace {
    /// The namespace of process `p`'s sockets: its instance's, by its uid. `None` for a process
    /// with no instance directory (the HLE embedding): the host's loopback, as before.
    #[must_use]
    pub fn for_process(p: &crate::Process) -> Option<Arc<Self>> {
        let instance = p.vfs.binds().instance_dir()?.to_path_buf();
        Some(Self::open(&instance, &ns_of(p.sys.uid())))
    }
}
```

In `socket.rs` `sys_socket` (line ~284) replace the `Host::create` call:

```rust
        Some(Peer::Host(crate::hostnet::Host::create(domain as u16, ty & SOCK_TYPE_MASK == 1, crate::loopns::Namespace::for_process(p))?))
```

- [ ] **Step 6: Implement the translation in `hostnet.rs`**

Add to `struct Host` (after `state`):

```rust
    /// The loopback namespace of this socket's loopback and wildcard ports (`crate::loopns`);
    /// `None` for a process without an instance: the host's loopback, as before.
    ns: Option<Arc<crate::loopns::Namespace>>,
    /// The namespace entries this socket holds; released when it is dropped.
    held: Mutex<Vec<crate::loopns::Binding>>,
    /// The port the guest bound, where it is not the host's (`getsockname`).
    guest_port: Mutex<Option<u16>>,
    /// The loopback address the guest connected to, as it named it (`getpeername`).
    guest_peer: Mutex<Option<Vec<u8>>>,
```

Change `create` and `adopt`:

```rust
    pub fn create(domain: u16, stream: bool, ns: Option<Arc<crate::loopns::Namespace>>) -> Result<Arc<Self>, Errno> {
        let sock = Self::fresh(domain, stream)?;
        Ok(Self::adopt(sock, domain, stream, State::default(), ns))
    }

    /// A new unbound, non-blocking host socket (dual-stack for IPv6).
    fn fresh(domain: u16, stream: bool) -> Result<platnet::Socket, Errno> {
        let family = if domain == AF_INET { IpFamily::V4 } else { IpFamily::V6 };
        let kind = if stream { SocketKind::Stream } else { SocketKind::Datagram };
        let mut sock = platnet::Socket::new(kind, family, policy()).map_err(|e| errno(&e))?;
        sock.set_nonblocking(true).map_err(|e| errno(&e))?;
        if family == IpFamily::V6 {
            sock.set_option(SocketOption::V6Only(false)).map_err(|e| errno(&e))?;
        }
        Ok(sock)
    }

    fn adopt(sock: platnet::Socket, domain: u16, stream: bool, state: State, ns: Option<Arc<crate::loopns::Namespace>>) -> Arc<Self> {
        let host = Arc::new(Self {
            sock: RwLock::new(sock),
            domain,
            stream,
            seen: AtomicU32::new(UNSEEN),
            state: Mutex::new(state),
            ns,
            held: Mutex::new(Vec::new()),
            guest_port: Mutex::new(None),
            guest_peer: Mutex::new(None),
        });
        watch(&host);
        host
    }

    fn proto(&self) -> crate::loopns::Proto {
        if self.stream { crate::loopns::Proto::Tcp } else { crate::loopns::Proto::Udp }
    }

    /// The host's port for this socket.
    fn host_port(&self) -> Result<u16, Errno> {
        let addr = self.sock.read().local_address().map_err(|e| errno(&e))?;
        Ok(match addr {
            SocketAddress::V4 { port, .. } | SocketAddress::V6 { port, .. } => port,
        })
    }
```

Replace `bind`:

```rust
    pub fn bind(&self, raw: &[u8]) -> Result<(), Errno> {
        crate::inet::check(self.domain, raw)?;
        if self.state.lock().bound {
            return Err(EINVAL);
        }
        let Some(ns) = self.ns.clone() else {
            let addr = parse(self.domain, raw)?;
            self.sock_mut().bind(&addr).map_err(|e| errno(&e))?;
            self.state.lock().bound = true;
            wake();
            return Ok(());
        };
        // `inet::check` let through only loopback and the wildcard: bound on the host as the same
        // address with port 0, and the guest's port is the namespace's.
        let guest = crate::loopns::port_of(raw);
        let host_addr = parse(self.domain, &crate::loopns::with_port(raw, 0))?;
        self.sock_mut().bind(&host_addr).map_err(|e| errno(&e))?;
        let host = self.host_port()?;
        let shared = self.state.lock().reuse;
        match ns.bind(self.proto(), guest, host, shared) {
            Ok(b) => self.held.lock().push(b),
            Err(e) => {
                // The guest's bind failed: its socket is unbound again.
                *self.sock_mut() = Self::fresh(self.domain, self.stream)?;
                return Err(e);
            }
        }
        *self.guest_port.lock() = Some(if guest == 0 { host } else { guest });
        self.state.lock().bound = true;
        wake();
        Ok(())
    }

    /// For a loopback destination: the host address it resolves to in this socket's namespace
    /// (the same address, the host's port), binding an unbound socket first -- the same loopback
    /// address for a stream, the wildcard for a datagram -- so its peer can resolve it too.
    /// `Ok(None)`: not a loopback destination, or no namespace: as given. `Err(ECONNREFUSED)`: a
    /// port no socket of the namespace holds.
    fn resolve(&self, raw: &[u8]) -> Result<Option<Vec<u8>>, Errno> {
        let Some(ns) = &self.ns else { return Ok(None) };
        if !crate::loopns::is_loopback(raw) {
            return Ok(None);
        }
        let host = ns.lookup(self.proto(), crate::loopns::port_of(raw)).ok_or(ECONNREFUSED)?;
        if !self.state.lock().bound {
            let local = if self.stream { crate::loopns::with_port(raw, 0) } else { crate::loopns::wildcard(self.domain) };
            self.bind(&local)?;
        }
        Ok(Some(crate::loopns::with_port(raw, host)))
    }
```

In `connect`, as its first lines (before `let addr = parse(...)`):

```rust
        if !self.stream && self.ns.is_some() && crate::loopns::is_loopback(raw) {
            // Connecting a datagram socket only names its peer (Linux): resolved at each send, so
            // a send before the peer binds is dropped and a peer that binds later is reached.
            if !self.state.lock().bound {
                self.bind(&crate::loopns::wildcard(self.domain))?;
            }
            *self.guest_peer.lock() = Some(raw.to_vec());
            let mut st = self.state.lock();
            st.connected = true;
            st.bound = true;
            return Ok(());
        }
        let translated = self.resolve(raw)?;
        if translated.is_some() {
            *self.guest_peer.lock() = Some(raw.to_vec());
        }
        let raw: &[u8] = translated.as_deref().unwrap_or(raw);
```

In `listen`, before `self.sock_mut().listen(...)`:

```rust
        if self.ns.is_some() && !self.state.lock().bound {
            self.bind(&crate::loopns::wildcard(self.domain))?;
        }
```

In `accept`, replace the body after `let deadline = ...` with a loop that drops foreign connections:

```rust
        loop {
            let (sock, peer) = self.retry(IN, nonblocking, deadline, t, || {
                let st = self.state.lock();
                if !st.listening || st.shut_rd {
                    return Err(EINVAL);
                }
                drop(st);
                self.sock.read().accept().map_err(|e| errno(&e))
            })?;
            let mut raw_peer = sockaddr(&peer);
            if let Some(ns) = &self.ns {
                // Only a connection from this namespace: loopback, from a port of its own.
                let theirs = crate::loopns::is_loopback(&raw_peer)
                    .then(|| ns.guest_port(crate::loopns::Proto::Tcp, crate::loopns::port_of(&raw_peer)))
                    .flatten();
                match theirs {
                    Some(g) => raw_peer = crate::loopns::with_port(&raw_peer, g),
                    None => {
                        drop(sock); // closed: another instance's, or a host program's
                        continue;
                    }
                }
            }
            let mut sock = sock;
            sock.set_nonblocking(true).map_err(|e| errno(&e))?;
            let state = State { bound: true, connected: true, ..State::default() };
            return Ok((Self::adopt(sock, self.domain, true, state, self.ns.clone()), raw_peer));
        }
```

In `send_inner`, replace the `dest` computation (a datagram socket connected to a loopback peer has no host peer: its guest peer is the destination, resolved now):

```rust
        let named_peer = if self.stream { None } else { self.guest_peer.lock().clone() };
        let to = to.or(named_peer.as_deref());
        let dest = match to {
            Some(raw) if !self.stream => match self.resolve(raw) {
                Ok(Some(host)) => Some(parse(self.domain, &host)?),
                Ok(None) => Some(parse(self.domain, raw)?),
                // A datagram to a port no one here holds: gone, as to a closed port.
                Err(e) if e == ECONNREFUSED => return Ok(bytes.len()),
                Err(e) => return Err(e),
            },
            _ => None,
        };
```

Replace `recv` whole (it drops datagrams from another namespace's loopback ports and shows a loopback source by its guest port; an unbound datagram socket's implicit bind goes through the namespace):

```rust
    pub fn recv(&self, buf: &mut [u8], flags: u64, nonblocking: bool, t: &Task) -> Result<(usize, Option<SocketAddress>), Errno> {
        let nonblocking = nonblocking || flags & MSG_DONTWAIT != 0;
        {
            let st = self.state.lock();
            if st.shut_rd {
                return Ok((0, None));
            }
            if self.stream && !st.connected && !st.connecting {
                return Err(ENOTCONN);
            }
        }
        if !self.stream && !self.state.lock().bound {
            // Linux waits on an unbound datagram socket; Winsock refuses a receive there. Bound to
            // the wildcard address and an ephemeral port, it waits on both.
            if self.ns.is_some() {
                self.bind(&crate::loopns::wildcard(self.domain))?;
            } else {
                let any = if self.domain == AF_INET { SocketAddress::unspecified(IpFamily::V4) } else { SocketAddress::unspecified(IpFamily::V6) };
                self.sock_mut().bind(&any).map_err(|e| errno(&e))?;
                self.state.lock().bound = true;
            }
        }
        let deadline = self.deadline(false);
        let peek = flags & MSG_PEEK != 0;
        let waitall = self.stream && !nonblocking && !peek && flags & MSG_WAITALL != 0;
        'next: loop {
            let mut done = 0;
            let mut from = None;
            loop {
                let r = self.retry(IN, nonblocking, deadline, t, || {
                    if self.stream && self.still_connecting()? {
                        return Err(EAGAIN);
                    }
                    let s = self.sock.read();
                    let into = &mut buf[done..];
                    let r = if peek {
                        s.peek(into)
                    } else if self.stream {
                        s.recv(into).map(|n| (n, None))
                    } else {
                        s.recv_from(into).map(|(n, a)| (n, Some(a)))
                    };
                    match r {
                        Ok(v) => Ok(v),
                        // Winsock reports an ICMP port-unreachable for an earlier datagram on the next
                        // receive; Linux reports it only on a connected socket, as ECONNREFUSED.
                        Err(e) if !self.stream && e.kind() == Some(NetErrorKind::ConnectionReset) => {
                            if self.state.lock().connected { Err(ECONNREFUSED) } else { Err(EAGAIN) }
                        }
                        // A datagram larger than the buffer: what fit, the rest discarded.
                        Err(e) if !self.stream && e.kind() == Some(NetErrorKind::MessageSize) => Ok((into.len(), None)),
                        Err(e) => Err(errno(&e)),
                    }
                });
                match r {
                    Ok((n, a)) => {
                        done += n;
                        from = from.or(a);
                        if !waitall || n == 0 || done >= buf.len() {
                            break;
                        }
                    }
                    Err(_) if done > 0 => break,
                    Err(e) => return Err(e),
                }
            }
            if let (Some(ns), false, Some(src)) = (&self.ns, self.stream, from) {
                let raw_src = sockaddr(&self.guest_family(src));
                if crate::loopns::is_loopback(&raw_src) {
                    let peer = self.guest_peer.lock().as_deref().map(crate::loopns::port_of);
                    match ns.guest_port(crate::loopns::Proto::Udp, crate::loopns::port_of(&raw_src)) {
                        // Connected to a loopback peer: only that peer's datagrams (the host socket
                        // is not connected, so this is where Linux's filter is kept).
                        Some(g) if peer.is_some_and(|p| p != g) => {
                            if peek {
                                let mut scratch = vec![0u8; 64 << 10];
                                let _ = self.sock.read().recv_from(&mut scratch);
                            }
                            continue 'next;
                        }
                        Some(g) => return Ok((done, Some(with_sa_port(src, g)))),
                        // Another namespace's datagram (or a host program's): dropped, and the
                        // wait goes on. A peek saw it without taking it: taken now, then dropped.
                        None => {
                            if peek {
                                let mut scratch = vec![0u8; 64 << 10];
                                let _ = self.sock.read().recv_from(&mut scratch);
                            }
                            continue 'next;
                        }
                    }
                }
            }
            return Ok((done, from));
        }
    }
```

and add, at module level beside `sockaddr`:

```rust
/// `addr` with another port.
fn with_sa_port(addr: SocketAddress, port: u16) -> SocketAddress {
    match addr {
        SocketAddress::V4 { address, .. } => SocketAddress::V4 { address, port },
        SocketAddress::V6 { address, flowinfo, scope_id, .. } => SocketAddress::V6 { address, port, flowinfo, scope_id },
    }
}
```

Replace `name` and `peer`:

```rust
    pub fn name(&self) -> Result<Vec<u8>, Errno> {
        let addr = self.sock.read().local_address().map_err(|e| errno(&e))?;
        let raw = sockaddr(&self.guest_family(addr));
        Ok(match *self.guest_port.lock() {
            Some(p) => crate::loopns::with_port(&raw, p),
            None => raw,
        })
    }

    pub fn peer(&self) -> Result<Vec<u8>, Errno> {
        if self.stream && self.still_connecting()? {
            return Err(ENOTCONN);
        }
        if !self.state.lock().connected {
            return Err(ENOTCONN);
        }
        if let Some(named) = self.guest_peer.lock().clone() {
            return Ok(named);
        }
        let addr = self.sock.read().peer_address().map_err(|e| errno(&e))?;
        let raw = sockaddr(&self.guest_family(addr));
        if let Some(ns) = &self.ns {
            if crate::loopns::is_loopback(&raw) {
                if let Some(g) = ns.guest_port(self.proto(), crate::loopns::port_of(&raw)) {
                    return Ok(crate::loopns::with_port(&raw, g));
                }
            }
        }
        Ok(raw)
    }
```

Fix every other `Self::adopt(` call in `hostnet.rs` to pass `self.ns.clone()` (or `None` where there is no `self`), and every other `Host::create(` caller in the crate (`grep -rn "Host::create" crates/`) to pass the namespace.

- [ ] **Step 7: Run the new test and the existing socket tests**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --test loopback_ns --test inet_sockets --test relay --test host_sockets`
Expected: `loopback_ns` 4 passed; `inet_sockets` and `relay` pass; **`host_sockets` fails** (its host servers are no longer reachable -- fixed in Task 4).

- [ ] **Step 8: Commit**

```bash
git add crates/omni-linux/src/hostnet.rs crates/omni-linux/src/socket.rs crates/omni-linux/src/loopns.rs crates/omni-linux/tests/loopback_ns.rs crates/omni-linux/tests/common/mod.rs crates/omni-linux/tests/fixtures/loopiso.c crates/omni-linux/tests/fixtures/loopiso crates/omni-linux/tests/fixtures/build.txt crates/omni-linux/tests/fixtures/SHA256SUMS
git commit -m "feat(net): a guest's loopback is its namespace's -- host ports cannot be named"
```

---

### Task 4: the host-network test exposes its servers on purpose

**Files:**
- Modify: `crates/omni-linux/tests/host_sockets.rs:67-76`

**Interfaces:**
- Consumes: `omni_linux::loopns::{expose, Proto}` (Task 2), `common::run_fixture_as` (Task 3).

- [ ] **Step 1: Run the test to see it fail**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --test host_sockets`
Expected: FAIL — `FAIL tcp connect to a host server` (the guest cannot name the host's port).

- [ ] **Step 2: Expose the servers into the fixture's namespace** — replace the test function:

```rust
#[test]
fn a_guest_talks_tcp_and_udp_over_the_hosts_network() {
    use omni_linux::loopns::{expose, Proto};
    let (echo, late, closed, udp) = (echo_server(), late_server(), closed_port(), udp_echo());
    // The host's servers stand in for the network's: a guest's loopback is its namespace's
    // (`crate::loopns`), so each is handed to the fixture's namespace -- an app's, user 0's -- on
    // purpose, as the same port. The closed port is not: refused either way.
    let instance = std::env::temp_dir().join(format!("omni-linux-hostnet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let _held = [
        expose(&instance, "u0", Proto::Tcp, echo, echo).expect("echo"),
        expose(&instance, "u0", Proto::Tcp, late, late).expect("late"),
        expose(&instance, "u0", Proto::Udp, udp, udp).expect("udp"),
    ];
    let ports = [echo, late, closed, udp].map(|p| p.to_string());
    let args: Vec<&str> = ports.iter().map(String::as_str).collect();
    let Some((status, out, err)) = common::run_fixture_as(&instance, "inetnet", &args, 10_000) else { return };
    eprintln!("{out}");
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), 37, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
}
```

- [ ] **Step 3: Run it to verify it passes**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --test host_sockets`
Expected: PASS, 37 `ok` lines. (The UDP echo's replies come from the host port the test exposed, so `recvfrom` shows `udp` as the source port: the reverse entry `expose` wrote.)

- [ ] **Step 4: Commit**

```bash
git add crates/omni-linux/tests/host_sockets.rs
git commit -m "test(net): the host-network gate hands its servers to the guest's namespace on purpose"
```

---

### Task 5: `remote` — identity from a credential, not the frame

**Files:**
- Modify: `crates/omni-linux/src/remote.rs` (frames `OPEN`/`PROPS`/`ATTACH`, `Server`, `serve`, client `RemoteBinder::open`, `thread`, `system_properties`, `set_server`)

**Interfaces:**
- Consumes: `omni_platform::process::random_bytes(&mut [u8])`.
- Produces:
  - `pub type Credential = [u8; 16];`
  - `pub fn issue_credential(pid: i32, uid: u32) -> Credential` — registers the identity with this host process's binder listener
  - `pub fn revoke_credential(pid: i32)`
  - `pub fn set_credential(c: Credential)` (app side), `pub fn credential_from_hex(s: &str) -> Option<Credential>`, `pub fn credential_hex(c: &Credential) -> String`
  - Frames: `OPEN` = `[context u8][credential 16]`; `OPENED` = `[open token u64][pid u32][uid u32]`; `ATTACH` = `[open token u64][tid u32][credential 16]`; `PROPS` = `[credential 16]`. Open tokens are random.

- [ ] **Step 1: Write the failing tests** — add to `remote.rs`'s `mod tests`:

```rust
    fn frame(stream: &mut TcpStream, kind: u8, payload: &[u8]) {
        send(stream, kind, payload).expect("send");
    }

    fn sysroot() -> Option<Arc<crate::vfs::Sysroot>> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35");
        crate::vfs::Sysroot::open(&dir).ok()
    }

    #[test]
    fn an_unknown_credential_is_refused() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&[7u8; 16]); // never issued
        frame(&mut s, OPEN, &req);
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(s.read(&mut b).unwrap_or(0), 0, "closed without an answer");
    }

    #[test]
    fn the_stand_ins_identity_is_the_credentials_whatever_the_process_claims() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let c = issue_credential(4242_000, 10_115);
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&c);
        frame(&mut s, OPEN, &req);
        let (kind, body) = receive(&mut s).expect("answer");
        assert_eq!(kind, OPENED);
        assert_eq!(u32_at(&body, 8) as i32, 4242_000, "pid from the credential");
        assert_eq!(u32_at(&body, 12), 10_115, "uid from the credential");
        revoke_credential(4242_000);
        let mut again = TcpStream::connect(addr).expect("connect");
        frame(&mut again, OPEN, &req);
        again.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(again.read(&mut b).unwrap_or(0), 0, "revoked: refused");
    }

    #[test]
    fn an_attach_needs_the_opens_own_credential() {
        let Some(root) = sysroot() else { return };
        let addr = serve(root).expect("serve");
        let mine = issue_credential(4243_000, 10_116);
        let theirs = issue_credential(4244_000, 10_117);
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut req = vec![0u8];
        req.extend_from_slice(&mine);
        frame(&mut s, OPEN, &req);
        let (_, body) = receive(&mut s).expect("opened");
        let token = u64_at(&body, 0);
        let mut a = TcpStream::connect(addr).expect("connect");
        let mut att = token.to_le_bytes().to_vec();
        att.extend_from_slice(&7u32.to_le_bytes());
        att.extend_from_slice(&theirs);
        frame(&mut a, ATTACH, &att);
        frame(&mut a, MMAP, &[0u8; 16]);
        a.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(a.read(&mut b).unwrap_or(0), 0, "another's credential cannot attach to this open");
        revoke_credential(4243_000);
        revoke_credential(4244_000);
    }

    #[test]
    fn a_credential_round_trips_as_hex() {
        let c = issue_credential(4245_000, 10_000);
        assert_eq!(credential_from_hex(&credential_hex(&c)), Some(c));
        assert_eq!(credential_from_hex("zz"), None);
        revoke_credential(4245_000);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --lib remote::tests`
Expected: compile errors (`issue_credential` etc. not found).

- [ ] **Step 3: Implement the server side** — in `remote.rs`:

```rust
/// A process's credential with the system's binder listener: 16 random bytes the zygote hands an
/// app's host process at launch (on its stdin), which that host process presents on every
/// connection. The listener takes the stand-in's pid and uid from it, never from a frame: before
/// it, any local program -- or a guest's socket to the host's loopback -- could claim uid 1000 or
/// another instance's uid.
pub type Credential = [u8; 16];

/// The credentials issued in this host process: credential -> (pid, uid).
fn issued() -> &'static Mutex<HashMap<Credential, (i32, u32)>> {
    static ISSUED: OnceLock<Mutex<HashMap<Credential, (i32, u32)>>> = OnceLock::new();
    ISSUED.get_or_init(Mutex::default)
}

/// Issue the credential of the process `pid` running as `uid`.
///
/// # Panics
/// When the host has no entropy to give (`omni_platform::process::random_bytes`).
#[must_use]
pub fn issue_credential(pid: i32, uid: u32) -> Credential {
    let mut c = [0u8; 16];
    omni_platform::process::random_bytes(&mut c).expect("entropy for a binder credential");
    issued().lock().insert(c, (pid, uid));
    c
}

/// Withdraw process `pid`'s credential (its host process ended).
pub fn revoke_credential(pid: i32) {
    issued().lock().retain(|_, (p, _)| *p != pid);
}

fn identity(c: &[u8]) -> Option<(i32, u32)> {
    let c: Credential = c.try_into().ok()?;
    issued().lock().get(&c).copied()
}

#[must_use]
pub fn credential_hex(c: &Credential) -> String {
    c.iter().map(|b| format!("{b:02x}")).collect()
}

#[must_use]
pub fn credential_from_hex(s: &str) -> Option<Credential> {
    let s = s.trim();
    if s.len() != 32 {
        return None;
    }
    let mut c = [0u8; 16];
    for (i, b) in c.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(c)
}

fn random_token() -> u64 {
    let mut b = [0u8; 8];
    omni_platform::process::random_bytes(&mut b).expect("entropy for an open's token");
    u64::from_le_bytes(b)
}
```

Change `Server`: remove the `next` field; store the open's owner credential with it:

```rust
struct Server {
    sysroot: Arc<crate::vfs::Sysroot>,
    stand_ins: Mutex<HashMap<i32, Weak<Process>>>,
    /// An open's binder file, its stand-in, and the credential that opened it, by random token.
    opens: Mutex<HashMap<u64, (Arc<Process>, Arc<BinderFile>, Credential)>>,
}
```

(`serve` builds it without `next`.) Replace the three arms of `connection`:

```rust
            OPEN => {
                let Some((pid, uid)) = identity(body.get(1..17).unwrap_or_default()) else { return };
                let cred: Credential = body[1..17].try_into().expect("16");
                let context = context_of(body[0]);
                let p = self.stand_in(pid, uid);
                let file = BinderFile::open(context);
                let token = random_token();
                self.opens.lock().insert(token, (p, file, cred));
                let mut reply = token.to_le_bytes().to_vec();
                reply.extend_from_slice(&(pid as u32).to_le_bytes());
                reply.extend_from_slice(&uid.to_le_bytes());
                if send(&mut stream, OPENED, &reply).is_err() {
                    self.opens.lock().remove(&token);
                    return;
                }
                let mut sink = [0u8; 1];
                let _ = stream.read(&mut sink);
                if let Some((_, file, _)) = self.opens.lock().remove(&token) {
                    file.release();
                }
            }
            PROPS => {
                if identity(&body).is_none() {
                    return;
                }
                // The system's properties as they are now, `name value ` each.
                let service = crate::props::PropertyService::global(&self.sysroot);
                let mut out = Vec::new();
                for (k, v) in service.entries() {
                    out.extend_from_slice(k.as_bytes());
                    out.push(0);
                    out.extend_from_slice(v.as_bytes());
                    out.push(0);
                }
                let _ = send(&mut stream, PROPS_DATA, &out);
            }
            ATTACH => {
                let (token, tid) = (u64_at(&body, 0), u32_at(&body, 8) as i32);
                let Some((p, file, owner)) = self.opens.lock().get(&token).cloned() else { return };
                // Only the credential that made the open attaches threads to it.
                if body.get(12..28) != Some(&owner[..]) {
                    return;
                }
                let mut task = Task::new(tid, Arc::clone(&p));
                CURRENT.with(|c| *c.borrow_mut() = stream.try_clone().ok());
                loop {
                    let Ok((kind, body)) = receive(&mut stream) else { break };
                    let ret: u64 = match kind {
                        IOCTL => match crate::binder::ioctl(&p, &mut task, &file, u64_at(&body, 0), u64_at(&body, 8), false) {
                            Ok(v) => v,
                            Err(e) => e.as_return(),
                        },
                        MMAP => {
                            file.set_area(u64_at(&body, 0), u64_at(&body, 8));
                            0
                        }
                        other => {
                            eprintln!("[remote] pid {} tid {tid}: unexpected frame {other} on its thread's connection", p.sys.pid);
                            break;
                        }
                    };
                    if send(&mut stream, DONE, &ret.to_le_bytes()).is_err() {
                        break;
                    }
                }
                CURRENT.with(|c| *c.borrow_mut() = None);
            }
```

- [ ] **Step 4: Implement the app side** — replace the client-side statics and calls:

```rust
static CREDENTIAL: OnceLock<Credential> = OnceLock::new();

/// This host process's credential with the system's binder listener (read from stdin by the
/// runner: `--binder-credential-stdin`).
pub fn set_credential(c: Credential) {
    let _ = CREDENTIAL.set(c);
}

fn credential() -> Result<Credential, Errno> {
    CREDENTIAL.get().copied().ok_or(EIO)
}
```

In `system_properties`: `send(&mut s, PROPS, &credential()?).map_err(|_| EIO)?;`.

In `RemoteBinder::open`: the request becomes

```rust
        let mut req = vec![context_byte(context)];
        req.extend_from_slice(&credential()?);
```

(the pid and uid are no longer sent; `p` stays a parameter for the caller's signature).

In `RemoteBinder::thread`: append the credential to the `ATTACH` request after the tid:

```rust
        req.extend_from_slice(&credential()?);
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --lib remote::tests`
Expected: the 4 new tests and the 3 existing ones pass (the new ones print nothing and return early when there is no sysroot at `sysroot/aosp-35`).

- [ ] **Step 6: Commit**

```bash
git add crates/omni-linux/src/remote.rs
git commit -m "feat(binder): the remote listener takes identity from a credential it issued, never a frame"
```

---

### Task 6: the zygote hands each app host its credential

**Files:**
- Modify: `crates/omni-linux/src/zygote.rs` (`launch` ~192-287)
- Modify: `crates/omni-linux/src/bin/omni-linux-run.rs:28-65` (new flag)
- Modify: `crates/omni-linux/tests/c3_remote_binder.rs:40-52`

**Interfaces:**
- Consumes: `remote::{issue_credential, revoke_credential, credential_hex, credential_from_hex, set_credential}` (Task 5).
- Produces: runner flag `--binder-credential-stdin` — reads one line of 32 hex characters from stdin and calls `remote::set_credential`.

- [ ] **Step 1: Make the gate expect a credential** — in `c3_remote_binder.rs` replace the `omni-linux-run` spawn:

```rust
    let pid = omni_linux::process::reserve_pid();
    let cred = omni_linux::remote::issue_credential(pid, 10_000);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_omni-linux-run"))
        .args(["--sysroot", &sysroot.to_string_lossy(), "--instance", &instance.to_string_lossy()])
        .args(["--binder-server", &addr.to_string(), "--binder-credential-stdin", "--pid", &pid.to_string(), "--uid", "10000", "--", "/system/bin/service", "list"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("omni-linux-run");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "{}", omni_linux::remote::credential_hex(&cred)).expect("the credential");
    }
    let out = child.wait_with_output().expect("omni-linux-run");
```

- [ ] **Step 2: Run it to verify it fails**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --test c3_remote_binder`
Expected: FAIL — `unknown argument "--binder-credential-stdin"` (exit code 2).

- [ ] **Step 3: Implement the runner flag** — in `omni-linux-run.rs`'s argument `match`, beside `--binder-server`:

```rust
            // The credential the system issued this host process (`crate::remote`): one line of
            // hex on stdin, never on the command line other accounts can read.
            "--binder-credential-stdin" => {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let c = omni_linux::remote::credential_from_hex(&line).expect("--binder-credential-stdin: 32 hex characters on stdin");
                omni_linux::remote::set_credential(c);
            }
```

- [ ] **Step 4: Implement the zygote side** — in `zygote.rs` `launch`, add the flag to the command (beside `--binder-server`) and pipe stdin:

```rust
    cmd.args(["--binder-server", &launcher.binder, "--binder-credential-stdin", "--pid", &pid.to_string(), "--uid", &uid.to_string()]);
    cmd.stdin(std::process::Stdio::piped());
```

Replace the `match cmd.spawn()` success arm's start:

```rust
        Ok(mut child) => {
            let cred = crate::remote::issue_credential(pid, uid);
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = writeln!(stdin, "{}", crate::remote::credential_hex(&cred));
                // Dropped: closed, so the app host's own stdin reads end of file after it.
            }
            let child = Arc::new(parking_lot::Mutex::new(child));
            CHILDREN.lock().insert(pid, Arc::clone(&child));
            std::thread::spawn(move || {
                let status = loop {
                    match child.lock().try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {}
                        Err(e) => break Err(e),
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                };
                CHILDREN.lock().remove(&pid);
                crate::remote::revoke_credential(pid);
                eprintln!("[zygote] pid {pid} ended: {status:?}");
            });
            Some(pid)
        }
```

(The credential is issued after `spawn` succeeds and written before any binder open can happen: the child's runner reads stdin while parsing its arguments, before it starts the program.)

- [ ] **Step 5: Run the gates**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-linux --test c3_remote_binder; cargo test --release -p omni-linux --test c5_app_launch -- --ignored --nocapture`
Expected: both PASS (`c5` launches an app through the zygote, so the credential crosses end to end).

- [ ] **Step 6: Commit**

```bash
git add crates/omni-linux/src/zygote.rs crates/omni-linux/src/bin/omni-linux-run.rs crates/omni-linux/tests/c3_remote_binder.rs
git commit -m "feat(zygote): each app host gets its binder credential on stdin; revoked when it ends"
```

---

### Task 7: Phase 0 gate — whole suite, in-world, three hosts

**Files:** none new (fixes, if any, go in the task whose code they touch).

- [ ] **Step 1: The whole `omni-linux` and `omni-platform` suites on Windows**

Run: `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; cargo test --release -p omni-platform -p omni-linux`
Expected: every test passes (ignored ones stay ignored).

- [ ] **Step 2: The in-world gate on Windows**

Run (the owner's account for this gate: `HeZmI_ImYu1080`, a TR account -- its cookie must be used from its own exit country, TR, i.e. WARP or no VPN, never a US/NL exit): `$env:OMNIDROID_DYNARMIC_BUILD_DIR='C:\od-unified'; $env:OMNI_R_COOKIE="C:\Users\berat\Desktop\cookies\HeZmI_ImYu1080.txt"; $env:OMNI_R_PLACE='8737899170'; cargo test --release -p omni-linux --test r_roblox -- --ignored --nocapture`
Expected: PASS, and the log (`%TEMP%\omni-linux-r-<pid>.log`) shows `Joining game`. Search that log for `ECONNREFUSED` near Roblox lines: any refusal of a loopback port Roblox itself bound means a translation bug (fix in Task 3), not an expected refusal.

- [ ] **Step 3: Sync to Linux and macOS and run the suites there**

```bash
git push origin feat/shared-android
ssh berat@192.168.0.38 'cd "$HOME/Desktop/Omni Apps/omnidroid" && git fetch && git checkout feat/shared-android && git pull && cargo test --release -p omni-platform -p omni-linux 2>&1 | tail -30'
ssh berat@macmini.local 'ulimit -n 65536; cd "$HOME/Desktop/Omni Apps/omnidroid" && git fetch && git checkout feat/shared-android && git pull && cargo test --release -p omni-platform -p omni-linux 2>&1 | tail -30'
```

Expected: both end with `test result: ok` for every test binary. On Linux, afterwards: `ls /tmp /dev/shm | grep -E 'omni-(loopns|linux|shm)'` and remove what this run left.

- [ ] **Step 4: Record the phase** — append to `docs/superpowers/specs/2026-09-30-shared-android-multi-instance-design.md` under "Phases and gates" one line: `Phase 0 landed <commit>: suites green on Windows/Linux/macOS; r_roblox in-world on Windows.` Commit:

```bash
git add docs/superpowers/specs/2026-09-30-shared-android-multi-instance-design.md
git commit -m "docs(spec): phase 0 landed -- the guest network boundary"
```
