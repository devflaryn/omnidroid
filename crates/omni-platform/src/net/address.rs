//! Socket addresses, in a shape the guest's `sockaddr_in`/`sockaddr_in6` can be marshalled from.
//!
//! # Why this is not `std::net::SocketAddr`, and not the guest's bytes either
//!
//! It is not [`std::net::SocketAddr`] because the *caller* is an ARM64 Android guest, and what it
//! passes to `connect` is sixteen or twenty-eight bytes of `struct sockaddr_in6` with a family
//! number, a big-endian port, a flow label and a scope id in a layout that is the **guest's**, not
//! this host's. `SocketAddr` cannot say "these sixteen bytes" without going through a `Display`
//! and a parse, which is a round trip through text for data that was never text.
//!
//! It is not the guest's bytes either, and that boundary is the one worth being exact about:
//! **`omni-platform` does not know the guest's `sockaddr` layout and must not learn it.** The
//! guest's `AF_INET6` is 10 and this host's is 23; the guest's `sin6_scope_id` sits at a different
//! offset from this host's because `sockaddr_in6` is laid out differently on Linux and Windows.
//! Encoding either of those here would put a second copy of the guest ABI in a crate that sits
//! below `omni-bionic`, which is exactly the drift D19 separates the two crates to prevent.
//!
//! So this type is **structured**: a family, the address bytes in network order, a port as a
//! number, and for IPv6 the flowinfo and scope id as numbers. `omni-android`'s adapter reads the
//! guest's bytes into it and writes it back out, and it is the only place in the workspace that
//! knows what the guest's `sockaddr` looks like.
//!
//! # The bytes are in network order and the port is not
//!
//! [`SocketAddress::V4::address`] and [`SocketAddress::V6::address`] are exactly the bytes of
//! `struct in_addr`/`struct in6_addr` — network order, which for an address means "the order they
//! are written in" and needs no swap on any host. The port is an ordinary `u16` in host order,
//! because every byte-order mistake this project can make with a port is made at the *boundary*,
//! and having one representation that is unambiguously a number means the swap happens in exactly
//! two places: the adapter's read and the adapter's write.
//!
//! # `Display` here is a diagnostic and is not the guest's `inet_ntop`
//!
//! VERIFICATION entry 7 is about exactly this type of confusion: Rust's `Ipv6Addr: Display` and
//! bionic's `inet_ntop` **disagree**, measured, on 43 of 200,000 pseudo-random addresses, because
//! Rust stopped printing the deprecated IPv4-compatible form in dotted notation and BIND — which
//! bionic ships — still does. The guest's `inet_ntop` is `omni_bionic::net::inet_ntop` and is
//! derived from the specification. What [`SocketAddress`]'s `Display` produces goes into error
//! messages read by people, and nothing in this workspace may use it as an oracle for the other.

use core::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// Which IP version a socket or an address is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IpFamily {
    /// IPv4. The guest spells it `AF_INET` = 2; this host spells it 2 as well, and that agreement
    /// is a coincidence the adapter must not rely on — `AF_INET6` is 10 there and 23 here.
    V4,
    /// IPv6.
    V6,
}

impl IpFamily {
    /// Every variant, in order. Exists so that invariants can be asserted over all of them.
    pub const ALL: [IpFamily; 2] = [IpFamily::V4, IpFamily::V6];

    /// A short name, for a message that has to say which family refused.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            IpFamily::V4 => "IPv4",
            IpFamily::V6 => "IPv6",
        }
    }

    /// How many bytes an address of this family has: 4 or 16.
    #[must_use]
    pub const fn address_bytes(self) -> usize {
        match self {
            IpFamily::V4 => 4,
            IpFamily::V6 => 16,
        }
    }
}

impl fmt::Display for IpFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An address and a port, as one endpoint of a socket.
///
/// Round-trips losslessly with [`std::net::SocketAddr`] in both directions, including the two
/// IPv6 fields that are easy to drop: see [`SocketAddress::V6`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SocketAddress {
    /// An IPv4 endpoint: `struct sockaddr_in`.
    V4 {
        /// The four bytes of `struct in_addr`, in the order they are written.
        address: [u8; 4],
        /// The port, as a number. The guest's `sin_port` is this value in big-endian order, and
        /// the swap belongs to whoever reads the guest's bytes.
        port: u16,
    },
    /// An IPv6 endpoint: `struct sockaddr_in6`.
    ///
    /// **`flowinfo` and `scope_id` are carried, and dropping them would be a silent defect.**
    /// `scope_id` is what makes a link-local address (`fe80::/10`) usable at all — the same
    /// address exists on every interface and the scope id says which one — and a
    /// `sockaddr_in6` marshalled without it connects to whichever interface the host guesses.
    /// `flowinfo` matters less and is carried for the same reason `std::net::SocketAddrV6`
    /// carries it: a round trip that loses a field is not a round trip.
    V6 {
        /// The sixteen bytes of `struct in6_addr`, in the order they are written.
        address: [u8; 16],
        /// The port, as a number.
        port: u16,
        /// `sin6_flowinfo`, as a number.
        flowinfo: u32,
        /// `sin6_scope_id`, as a number: which interface a link-local address is on.
        scope_id: u32,
    },
}

impl SocketAddress {
    /// Which family this endpoint is.
    #[must_use]
    pub const fn family(&self) -> IpFamily {
        match self {
            SocketAddress::V4 { .. } => IpFamily::V4,
            SocketAddress::V6 { .. } => IpFamily::V6,
        }
    }

    /// The port, as a number.
    #[must_use]
    pub const fn port(&self) -> u16 {
        match self {
            SocketAddress::V4 { port, .. } | SocketAddress::V6 { port, .. } => *port,
        }
    }

    /// The address bytes, in the order they are written: four of them or sixteen.
    #[must_use]
    pub fn address_bytes(&self) -> &[u8] {
        match self {
            SocketAddress::V4 { address, .. } => address.as_slice(),
            SocketAddress::V6 { address, .. } => address.as_slice(),
        }
    }

    /// The wildcard address of a family with port 0: `0.0.0.0:0` or `[::]:0`.
    ///
    /// What an unbound socket's `getsockname` reports, and what this seam binds to when a guest
    /// sends from a socket it never bound.
    #[must_use]
    pub const fn unspecified(family: IpFamily) -> SocketAddress {
        match family {
            IpFamily::V4 => SocketAddress::V4 { address: [0; 4], port: 0 },
            IpFamily::V6 => {
                SocketAddress::V6 { address: [0; 16], port: 0, flowinfo: 0, scope_id: 0 }
            }
        }
    }

    /// The loopback address of a family: `127.0.0.1` or `::1`.
    #[must_use]
    pub const fn loopback(family: IpFamily, port: u16) -> SocketAddress {
        match family {
            IpFamily::V4 => SocketAddress::V4 { address: [127, 0, 0, 1], port },
            IpFamily::V6 => SocketAddress::V6 {
                address: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                port,
                flowinfo: 0,
                scope_id: 0,
            },
        }
    }

    /// Whether this address never leaves the machine.
    ///
    /// **`127.0.0.0/8` and `::1`, and deliberately not the IPv4-mapped form.** `::ffff:127.0.0.1`
    /// is a v6 socket's spelling of a v4 loopback address and it *is* loopback traffic, so it is
    /// recognised here too — a policy that admitted `127.0.0.1` and refused its mapped spelling
    /// would refuse the same destination under a second name, which is the kind of near-miss a
    /// test on one spelling never finds.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        match self.ip() {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
        }
    }

    /// Whether this is the wildcard address of its family.
    #[must_use]
    pub fn is_unspecified(&self) -> bool {
        match self {
            SocketAddress::V4 { address, .. } => *address == [0; 4],
            SocketAddress::V6 { address, .. } => *address == [0; 16],
        }
    }

    /// The address without its port, as `std`'s type.
    #[must_use]
    pub fn ip(&self) -> IpAddr {
        match self {
            SocketAddress::V4 { address, .. } => IpAddr::V4(Ipv4Addr::from(*address)),
            SocketAddress::V6 { address, .. } => IpAddr::V6(Ipv6Addr::from(*address)),
        }
    }

    /// Convert to `std`'s endpoint type, which is what every portable `std::net` call takes.
    #[must_use]
    pub fn to_std(self) -> SocketAddr {
        match self {
            SocketAddress::V4 { address, port } => {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(address), port))
            }
            SocketAddress::V6 { address, port, flowinfo, scope_id } => SocketAddr::V6(
                SocketAddrV6::new(Ipv6Addr::from(address), port, flowinfo, scope_id),
            ),
        }
    }

    /// Convert from `std`'s endpoint type, keeping both IPv6 fields.
    #[must_use]
    pub fn from_std(address: SocketAddr) -> SocketAddress {
        match address {
            SocketAddr::V4(v4) => {
                SocketAddress::V4 { address: v4.ip().octets(), port: v4.port() }
            }
            SocketAddr::V6(v6) => SocketAddress::V6 {
                address: v6.ip().octets(),
                port: v6.port(),
                flowinfo: v6.flowinfo(),
                scope_id: v6.scope_id(),
            },
        }
    }
}

impl fmt::Display for SocketAddress {
    /// **A diagnostic rendering, and not the guest's `inet_ntop`.** See this module's header: the
    /// two disagree on a measured 43 of 200,000 addresses, and the guest's answer comes from
    /// `omni_bionic::net::inet_ntop`, which is derived from the specification rather than from
    /// this.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_std())
    }
}

impl From<SocketAddr> for SocketAddress {
    fn from(address: SocketAddr) -> SocketAddress {
        SocketAddress::from_std(address)
    }
}

impl From<SocketAddress> for SocketAddr {
    fn from(address: SocketAddress) -> SocketAddr {
        address.to_std()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip keeps every field, including the two IPv6 ones that are easy to drop.
    ///
    /// **Asserted on a value where every field is distinct and non-zero**, because a round trip
    /// tested with `flowinfo: 0, scope_id: 0` passes against an implementation that drops both —
    /// which is the shape VERIFICATION entry 1 records under a different name.
    #[test]
    fn a_v6_address_round_trips_through_std_with_its_flowinfo_and_scope_id_intact() {
        let original = SocketAddress::V6 {
            address: [
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55,
            ],
            port: 49_152,
            flowinfo: 0x000a_bcde,
            scope_id: 17,
        };
        let there_and_back = SocketAddress::from_std(original.to_std());
        assert_eq!(there_and_back, original);
        let SocketAddress::V6 { flowinfo, scope_id, .. } = there_and_back else {
            panic!("a v6 address must come back as a v6 address");
        };
        assert_eq!(flowinfo, 0x000a_bcde, "flowinfo was dropped by the round trip");
        assert_eq!(scope_id, 17, "scope_id was dropped, and a link-local address needs it");
    }

    /// The v4 round trip keeps the bytes in the order they are written.
    ///
    /// The address is chosen so that a byte-swapped implementation produces a *different* address
    /// rather than the same one: `1.2.3.4` reversed is `4.3.2.1`, and neither is a palindrome.
    #[test]
    fn a_v4_address_round_trips_without_reversing_its_bytes() {
        let original = SocketAddress::V4 { address: [1, 2, 3, 4], port: 443 };
        assert_eq!(SocketAddress::from_std(original.to_std()), original);
        assert_eq!(original.address_bytes(), &[1, 2, 3, 4]);
        assert_eq!(original.to_std().to_string(), "1.2.3.4:443");
    }

    /// The port is a number here and stays one across the round trip.
    ///
    /// 443 byte-swaps to 46,593, so an implementation that swapped on the way in and not on the
    /// way out — or the other way round — fails this rather than cancelling out.
    #[test]
    fn the_port_is_a_number_and_is_not_byte_swapped_by_this_type() {
        let address = SocketAddress::V4 { address: [93, 184, 216, 34], port: 443 };
        assert_eq!(address.port(), 443);
        assert_eq!(address.to_std().port(), 443);
        assert_ne!(443_u16.swap_bytes(), 443, "443 is not a byte-order palindrome");
    }

    /// Loopback is recognised through both spellings, including the IPv4-mapped one.
    #[test]
    fn loopback_is_recognised_under_every_spelling_a_policy_would_have_to_admit() {
        assert!(SocketAddress::loopback(IpFamily::V4, 0).is_loopback());
        assert!(SocketAddress::loopback(IpFamily::V6, 0).is_loopback());
        // 127.0.0.2 is loopback too: the whole of 127.0.0.0/8 is.
        assert!(SocketAddress::V4 { address: [127, 0, 0, 2], port: 1 }.is_loopback());
        // `::ffff:127.0.0.1` — a v6 socket's spelling of the v4 loopback address.
        let mapped = SocketAddress::V6 {
            address: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1],
            port: 1,
            flowinfo: 0,
            scope_id: 0,
        };
        assert!(mapped.is_loopback(), "the mapped spelling reaches the same destination");
        // And an address that is not loopback is not, under either family.
        assert!(!SocketAddress::V4 { address: [128, 0, 0, 1], port: 1 }.is_loopback());
        assert!(!SocketAddress::V6 {
            address: [0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            port: 1,
            flowinfo: 0,
            scope_id: 0,
        }
        .is_loopback());
    }

    /// The wildcard address of each family is the one an unbound socket reports.
    #[test]
    fn the_unspecified_address_is_the_wildcard_of_its_family_with_port_zero() {
        for family in IpFamily::ALL {
            let any = SocketAddress::unspecified(family);
            assert_eq!(any.family(), family);
            assert_eq!(any.port(), 0);
            assert!(any.is_unspecified());
            assert!(!any.is_loopback(), "the wildcard address is not loopback");
            assert_eq!(any.address_bytes().len(), family.address_bytes());
        }
    }
}
