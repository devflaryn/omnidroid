//! Internet sockets' addresses, on a machine whose only interface is `lo` (what `crate::netlink`
//! reports): the wildcard and loopback addresses can be bound, port 0 is given an ephemeral port
//! (`ip_local_port_range`, 32768-60999), and a stream port is one socket's at a time. A bound port
//! is held while its socket is open.
//!
//! TCP and UDP sockets are the host's (`crate::hostnet`): their ports are the host's, and only
//! [`check`] -- which addresses lo has -- applies to them. The bookkeeping here is what the
//! sockets the host's network does not stand behind (raw, ICMP) bind.
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::errno::{Errno, EADDRINUSE, EADDRNOTAVAIL, EAFNOSUPPORT, EINVAL};

pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
const EPHEMERAL: std::ops::RangeInclusive<u16> = 32768..=60999;

/// (instance, socket type, port): the ports bound, and by how many sockets.
type Key = (usize, u64, u16);

fn bound() -> &'static Mutex<HashMap<Key, usize>> {
    static B: OnceLock<Mutex<HashMap<Key, usize>>> = OnceLock::new();
    B.get_or_init(Mutex::default)
}

/// A bound port, released when its socket is gone.
pub struct Port {
    key: Key,
    /// The socket's address, as `getsockname` reports it.
    pub name: Vec<u8>,
}

impl Drop for Port {
    fn drop(&mut self) {
        let mut ports = bound().lock();
        if let Some(n) = ports.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                ports.remove(&self.key);
            }
        }
    }
}

/// Whether an IPv4 address (network order bytes) is one of lo's, or the wildcard.
fn local_v4(a: [u8; 4]) -> bool {
    a == [0; 4] || a[0] == 127
}

/// Whether `addr` is one a socket of `domain` may bind here: long enough, of the socket's family,
/// and the wildcard or one of lo's addresses. A host socket (`crate::hostnet`) is checked by it
/// too, before the host is asked: the guest's machine has lo alone.
///
/// # Errors
/// `EINVAL` for a short address, `EAFNOSUPPORT` for another family, `EADDRNOTAVAIL` for an address
/// no interface has.
pub fn check(domain: u16, addr: &[u8]) -> Result<(), Errno> {
    let want = if domain == AF_INET { 16 } else { 24 };
    if addr.len() < want {
        return Err(EINVAL);
    }
    if u16::from_le_bytes([addr[0], addr[1]]) != domain {
        return Err(EAFNOSUPPORT);
    }
    let local = if domain == AF_INET {
        local_v4(addr[4..8].try_into().expect("4"))
    } else {
        let a: [u8; 16] = addr[8..24].try_into().expect("16");
        let v4_mapped = a[..10] == [0; 10] && a[10..12] == [0xff, 0xff];
        a == [0; 16] || a == { let mut l = [0; 16]; l[15] = 1; l } || (v4_mapped && local_v4(a[12..16].try_into().expect("4")))
    };
    if local { Ok(()) } else { Err(EADDRNOTAVAIL) }
}

/// `bind(addr)` for a socket of `domain` and type `ty` in `instance`.
///
/// # Errors
/// `EINVAL` for a short address, `EAFNOSUPPORT` for another family, `EADDRNOTAVAIL` for an address
/// no interface has, `EADDRINUSE` for a stream port already bound.
pub fn bind(instance: usize, domain: u16, ty: u64, addr: &[u8]) -> Result<Arc<Port>, Errno> {
    check(domain, addr)?;
    let want = if domain == AF_INET { 16 } else { 24 };
    let mut name = addr[..want].to_vec();
    if domain == AF_INET6 {
        name.resize(28, 0); // sin6_scope_id
    }
    let mut ports = bound().lock();
    let port = match u16::from_be_bytes([addr[2], addr[3]]) {
        0 => EPHEMERAL.clone().find(|p| !ports.contains_key(&(instance, ty, *p))).ok_or(EADDRINUSE)?,
        // A datagram port may be shared (SO_REUSEADDR, as mDNS responders bind 5353).
        p if ty == 1 && ports.contains_key(&(instance, ty, p)) => return Err(EADDRINUSE),
        p => p,
    };
    name[2..4].copy_from_slice(&port.to_be_bytes());
    *ports.entry((instance, ty, port)).or_default() += 1;
    Ok(Arc::new(Port { key: (instance, ty, port), name }))
}

/// An unbound socket's address: its family, the rest zero.
#[must_use]
pub fn unbound_name(domain: u16) -> Vec<u8> {
    let mut name = vec![0; if domain == AF_INET { 16 } else { 28 }];
    name[..2].copy_from_slice(&domain.to_le_bytes());
    name
}
