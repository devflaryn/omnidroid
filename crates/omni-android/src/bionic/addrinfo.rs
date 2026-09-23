//! The guest's `struct addrinfo` list: its layout, its bytes, and the bounded slab it lives in.
//!
//! `getaddrinfo` is the one symbol in this group whose *answer* is guest memory rather than a
//! return value. The guest receives a pointer, walks a linked list through it, dereferences an
//! `ai_addr` out of every node, and later hands the head back to `freeaddrinfo`. So this module
//! is two things that have to agree: the byte layout of the three structures, and an allocator
//! that can give the memory back.
//!
//! # Why there is a slab at all, and why it is not the pool
//!
//! The refusal this replaces gave two reasons and D30 voided only one of them. The one that
//! stood is this: [`Bionic`]'s pool is [`POOL_BYTES`] of **bump allocator that never frees**, and
//! a guest resolving in a loop exhausts it and never gets it back. Task 2's finding **F9** closes
//! the other obvious route — a handler may not map guest memory, because an inline handler runs
//! with generated code live — so `getaddrinfo` cannot allocate on demand either.
//!
//! What is left is what this module is: a **fixed** region, mapped before any guest code runs,
//! carved into [`ADDRINFO_RESULTS`] slots of [`ADDRINFO_NODES_PER_RESULT`] nodes each, with a free
//! list. `getaddrinfo` takes a slot; `freeaddrinfo` returns it by matching the head pointer it was
//! given. **A full slab is a refusal naming the symbol** — never an overwrite of a list the guest
//! is still walking, and never a silently truncated answer, which is the shape that would show up
//! as a connection to an address the resolver did not return.
//!
//! [`Bionic`]: super::Bionic
//! [`POOL_BYTES`]: super::POOL_BYTES
//!
//! # Why it is its own mapping rather than a fifth arena table
//!
//! When this was written [`ARENA_BYTES`](super::ARENA_BYTES) was **65,280** bytes against
//! `omni_mem`'s measured 65,536-byte commit granule, which is what made `Bionic::new`'s eager
//! commit free -- D10 forbids committing speculatively otherwise -- and there were 256 bytes spare,
//! three `addrinfo` nodes. The arena has since grown to five granules for a loaded world's threads
//! and streams ([`ARENA_GRANULES`](super::ARENA_GRANULES) states that cost); the slab stays
//! separate because its own length is all it needs to commit.
//!
//! An eagerly-committed mapping commits **its own length** rather than a granule
//! (`omni_mem::DEFAULT_MAX_COMMIT_REQUEST` says so in as many words), so a separate
//! [`ADDRINFO_SLAB_BYTES`] mapping costs [`ADDRINFO_SLAB_BYTES`] rounded to pages and leaves the
//! arena's invariant exactly where it was. That is the whole reason for the split, and it is
//! written here rather than left to be inferred from the absence of a term in `ARENA_BYTES`.
//!
//! # The layout is ASSUMED, and the assumption is not symmetrical
//!
//! `sizeof(struct addrinfo)` on LP64 bionic is taken to be **48 bytes** — `int ai_flags,
//! ai_family, ai_socktype, ai_protocol`, a `socklen_t ai_addrlen` with four bytes of padding after
//! it, then `char *ai_canonname`, `struct sockaddr *ai_addr`, `struct addrinfo *ai_next`. **There
//! is no NDK on this machine to check it against**, which is the same gap
//! [`FILE_BYTES`](super::FILE_BYTES) and `omni_bionic::layouts` record.
//!
//! The dangerous half is not the size. **Bionic orders `ai_canonname` before `ai_addr` where glibc
//! orders them the other way round**, so a layout copied from a glibc header puts the canonical
//! name where the address belongs — and the guest would then dereference a null pointer as a
//! `sockaddr`, or a `sockaddr` as a string. The two have the same `sizeof`, so nothing about the
//! total would notice. `the_addrinfo_layout_is_bionics_and_not_glibcs` is the assertion that the
//! two offsets are in *this* order, and the falsifier is stated with it: a real device's
//! `offsetof(struct addrinfo, ai_addr)` is 32, not 24.
//!
//! # The port is network order and the two IPv6 words are not
//!
//! `sin_port` and `sin6_port` are big-endian on every architecture — that is what "network order"
//! means and it is not a property of the guest. `sin6_flowinfo` and `sin6_scope_id` are **host**
//! order, which for an ARM64 Android guest is little-endian. `omni_platform::net::SocketAddress`
//! deliberately carries the port as a *number* and the address as bytes-in-written-order, so the
//! byte swap happens in exactly two places: [`decode_sockaddr`] and [`encode_sockaddr`].

use omni_mem::GuestAddr;
use omni_platform::net::{IpFamily, SocketAddress};
use parking_lot::Mutex;

/// `AF_INET`, as the **guest** numbers it. 2 here and 2 on this host, which is a coincidence.
pub(super) const AF_INET: i32 = omni_bionic::net::AF_INET;
/// `AF_INET6`, as the **guest** numbers it: 10. This host spells it 23, which is why the number
/// comes from `omni-bionic` and never from the development machine's headers.
pub(super) const AF_INET6: i32 = omni_bionic::net::AF_INET6;
/// `AF_UNSPEC`: "either family", which is what a client that will try both asks for.
pub(super) const AF_UNSPEC: i32 = 0;

/// `sizeof(struct sockaddr_in)`: `u16 sin_family`, `u16 sin_port`, `u32 sin_addr`, 8 bytes of
/// `sin_zero`.
pub(super) const SOCKADDR_IN_BYTES: usize = 16;
/// `sizeof(struct sockaddr_in6)`: `u16 sin6_family`, `u16 sin6_port`, `u32 sin6_flowinfo`,
/// 16 bytes of `sin6_addr`, `u32 sin6_scope_id`.
pub(super) const SOCKADDR_IN6_BYTES: usize = 28;

/// Offset of `sin6_flowinfo` in `struct sockaddr_in6`.
const SIN6_FLOWINFO_OFFSET: usize = 4;
/// Offset of `sin6_addr` in `struct sockaddr_in6`.
const SIN6_ADDR_OFFSET: usize = 8;
/// Offset of `sin6_scope_id` in `struct sockaddr_in6`.
const SIN6_SCOPE_ID_OFFSET: usize = 24;

/// `sizeof(struct addrinfo)` on LP64 bionic. **ASSUMED** — see this module's header.
pub const ADDRINFO_BYTES: usize = 48;

/// `offsetof(struct addrinfo, ai_flags)`.
const AI_FLAGS_OFFSET: usize = 0;
/// `offsetof(struct addrinfo, ai_family)`.
const AI_FAMILY_OFFSET: usize = 4;
/// `offsetof(struct addrinfo, ai_socktype)`.
const AI_SOCKTYPE_OFFSET: usize = 8;
/// `offsetof(struct addrinfo, ai_protocol)`.
const AI_PROTOCOL_OFFSET: usize = 12;
/// `offsetof(struct addrinfo, ai_addrlen)`. A `socklen_t`, so 32 bits, with four bytes of padding
/// after it before the first pointer.
const AI_ADDRLEN_OFFSET: usize = 16;
/// `offsetof(struct addrinfo, ai_canonname)`. **Before `ai_addr` — that is bionic's order and it
/// is the reverse of glibc's.**
const AI_CANONNAME_OFFSET: usize = 24;
/// `offsetof(struct addrinfo, ai_addr)`. **After `ai_canonname`.**
const AI_ADDR_OFFSET: usize = 32;
/// `offsetof(struct addrinfo, ai_next)`.
const AI_NEXT_OFFSET: usize = 40;

/// **The name comes before the address**, which is bionic's order and the reverse of glibc's.
///
/// A **compile-time** assertion rather than a test, because both sides are constants: written as
/// a test, clippy folds it to `assert!(true)`, and an assertion that cannot fail reads like a
/// covered case while covering nothing. The same lint, on the same ground, is what moved
/// [`MAX_GUEST_FILES`](super::MAX_GUEST_FILES)'s ceiling check up beside its constant.
///
/// It is the single most dangerous thing in this file to get wrong, and the least visible: the
/// two structures have the same `sizeof`, the same field set and the same alignment, so every
/// size-based check passes against either. A list built to glibc's order hands the guest a null
/// `char *` where its `sockaddr *` belongs, and the guest faults inside its own resolver code.
const _: () = assert!(AI_CANONNAME_OFFSET < AI_ADDR_OFFSET);

/// Bytes each node's `sockaddr` gets in the slab.
///
/// [`SOCKADDR_IN6_BYTES`] rounded up to eight, so that the sockaddr array that follows the node
/// array stays 8-aligned whichever family each entry is. A `sockaddr_in` uses the first sixteen
/// bytes of its slot and the rest is zero, which is what `sin_zero` is anyway.
pub(super) const SOCKADDR_SLOT_BYTES: usize = SOCKADDR_IN6_BYTES.next_multiple_of(8);

/// How many `addrinfo` nodes one resolution may return.
///
/// **A policy number, and stated as one.** A resolver can legitimately return more — a CDN with
/// many edge addresses, or an `ai_socktype` of 0 that asks for one node per socket type — and
/// past this the call is **refused by name** rather than truncated. A truncated list is the worse
/// answer: the guest connects to whichever addresses survived and nothing anywhere records that
/// the resolver had offered others.
///
/// **32, raised from 8 on a measurement**: once the Lua app was loading, `fts.rbxcdn.com` resolved
/// to 9 addresses and the call was refused. A CDN name answering a dozen edges is ordinary, and an
/// `ai_socktype` of 0 multiplies it by three; 32 covers both with room, for 20 KiB of slab.
pub const ADDRINFO_NODES_PER_RESULT: usize = 32;

/// How many resolutions may be live — handed to the guest and not yet freed — at once.
///
/// **A policy number.** A correct caller frees each list before or shortly after it finishes with
/// it, so the live count is the number of resolutions genuinely in flight, which for a client
/// fetching settings and then joining a game is one or two. Eight leaves room for a guest that
/// resolves several names in parallel and still refuses a guest that leaks, which is the case the
/// bump-allocating pool could not survive.
pub const ADDRINFO_RESULTS: usize = 8;

/// Bytes one result slot occupies: every node, then every node's `sockaddr`.
///
/// The nodes come first and are contiguous, so `ai_next` is slot arithmetic rather than a second
/// allocation, and every address in the slot is 8-aligned because 48 and 32 both are.
pub const ADDRINFO_RESULT_BYTES: usize =
    ADDRINFO_NODES_PER_RESULT * (ADDRINFO_BYTES + SOCKADDR_SLOT_BYTES);

/// Bytes of guest address space the resolver slab occupies. Mapped once, in `Bionic::new`.
pub const ADDRINFO_SLAB_BYTES: usize = ADDRINFO_RESULTS * ADDRINFO_RESULT_BYTES;

/// Every address in a slot is 8-aligned, which a node's three pointers require.
///
/// A **compile-time** assertion rather than a test, because both sides are constants — the same
/// reasoning [`super::MAX_GUEST_FILES`]'s ceiling check gives. A slot size that was not a multiple
/// of eight would misalign every slot after the first, and an ARM64 guest would fault on the
/// first `ldr` of `ai_next` rather than on anything this layer did.
const _: () = assert!(ADDRINFO_BYTES % 8 == 0);
const _: () = assert!(SOCKADDR_SLOT_BYTES % 8 == 0);
const _: () = assert!(ADDRINFO_RESULT_BYTES % 8 == 0);

/// One address the resolver returned, with the socket parameters the guest will create it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResultNode {
    /// `ai_socktype`: `SOCK_STREAM` or `SOCK_DGRAM`, in the guest's numbering.
    pub socktype: i32,
    /// `ai_protocol`: `IPPROTO_TCP` or `IPPROTO_UDP`, in the guest's numbering.
    pub protocol: i32,
    /// The endpoint, which decides `ai_family`, `ai_addrlen` and the `sockaddr` bytes.
    pub address: SocketAddress,
}

/// Write a `sockaddr_in` or `sockaddr_in6` into a fixed slot, and say how many bytes are
/// meaningful.
///
/// The returned length is the guest's `ai_addrlen` and the `socklen_t` a `connect` would be given:
/// 16 for IPv4 and 28 for IPv6, **not** [`SOCKADDR_SLOT_BYTES`]. A caller that reported the slot
/// size instead would hand the guest a length that is right for neither family.
#[must_use]
pub(super) fn encode_sockaddr(address: &SocketAddress) -> ([u8; SOCKADDR_SLOT_BYTES], usize) {
    let mut out = [0u8; SOCKADDR_SLOT_BYTES];
    match address {
        SocketAddress::V4 { address, port } => {
            out[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
            // **Network order**, which is big-endian on every architecture and is not a property
            // of the guest's own byte order.
            out[2..4].copy_from_slice(&port.to_be_bytes());
            out[4..8].copy_from_slice(address);
            // `sin_zero[8]` stays zero: bionic's `connect` does not read it and a device leaves
            // whatever was there, so zero is the one value that cannot carry information.
            (out, SOCKADDR_IN_BYTES)
        }
        SocketAddress::V6 { address, port, flowinfo, scope_id } => {
            out[0..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
            out[2..4].copy_from_slice(&port.to_be_bytes());
            // Host order, and the guest's host is little-endian ARM64.
            out[SIN6_FLOWINFO_OFFSET..SIN6_FLOWINFO_OFFSET + 4]
                .copy_from_slice(&flowinfo.to_le_bytes());
            out[SIN6_ADDR_OFFSET..SIN6_ADDR_OFFSET + 16].copy_from_slice(address);
            out[SIN6_SCOPE_ID_OFFSET..SIN6_SCOPE_ID_OFFSET + 4]
                .copy_from_slice(&scope_id.to_le_bytes());
            (out, SOCKADDR_IN6_BYTES)
        }
    }
}

/// Why a guest `sockaddr` could not be read.
///
/// Two cases and they are different answers to the guest: a family this layer has no socket for
/// is `EAFNOSUPPORT`, and a `socklen_t` too short for the family the guest itself named is
/// `EINVAL`. Collapsing them would tell a caller trying IPv6 first that its *arguments* were
/// wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SockaddrError {
    /// `sa_family` is neither `AF_INET` nor `AF_INET6`.
    Family(i32),
    /// The `socklen_t` the guest passed is shorter than the family it named needs.
    TooShort { family: i32, given: usize, needed: usize },
}

/// Read a guest `sockaddr_in`/`sockaddr_in6` out of bytes the guest supplied.
///
/// `given` is the guest's own `socklen_t`, and it is checked against the family **the guest
/// named** rather than against the buffer: a `connect` with `sizeof(struct sockaddr_in)` on an
/// `AF_INET6` address is `EINVAL` on a device, and reading the sixteen IPv6 bytes anyway would
/// connect somewhere the caller never described.
pub(super) fn decode_sockaddr(
    bytes: &[u8],
    given: usize,
) -> Result<SocketAddress, SockaddrError> {
    let family = i32::from(u16::from_le_bytes([bytes[0], bytes[1]]));
    match family {
        AF_INET => {
            if given < SOCKADDR_IN_BYTES {
                return Err(SockaddrError::TooShort {
                    family,
                    given,
                    needed: SOCKADDR_IN_BYTES,
                });
            }
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let mut address = [0u8; 4];
            address.copy_from_slice(&bytes[4..8]);
            Ok(SocketAddress::V4 { address, port })
        }
        AF_INET6 => {
            if given < SOCKADDR_IN6_BYTES {
                return Err(SockaddrError::TooShort {
                    family,
                    given,
                    needed: SOCKADDR_IN6_BYTES,
                });
            }
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let flowinfo = u32::from_le_bytes(
                bytes[SIN6_FLOWINFO_OFFSET..SIN6_FLOWINFO_OFFSET + 4]
                    .try_into()
                    .expect("four bytes"),
            );
            let mut address = [0u8; 16];
            address.copy_from_slice(&bytes[SIN6_ADDR_OFFSET..SIN6_ADDR_OFFSET + 16]);
            let scope_id = u32::from_le_bytes(
                bytes[SIN6_SCOPE_ID_OFFSET..SIN6_SCOPE_ID_OFFSET + 4]
                    .try_into()
                    .expect("four bytes"),
            );
            Ok(SocketAddress::V6 { address, port, flowinfo, scope_id })
        }
        other => Err(SockaddrError::Family(other)),
    }
}

/// The guest's `ai_family` number for an address.
#[must_use]
pub(super) const fn family_number(family: IpFamily) -> i32 {
    match family {
        IpFamily::V4 => AF_INET,
        IpFamily::V6 => AF_INET6,
    }
}

/// Build the whole of one result slot's bytes: every node, then every node's `sockaddr`.
///
/// **One buffer for the whole slot, written as one access**, so a slot is either entirely the new
/// list or entirely what it was — the all-or-nothing shape `files::write_struct` exists for, and
/// the direction review finding M1 says to err in. It matters more here than anywhere else in the
/// adapter: the guest walks this list by following pointers, so a half-written one is a walk into
/// whatever the previous resolution left.
///
/// `at` is the slot's base, which is also the head pointer the guest is handed.
#[must_use]
pub(super) fn encode_result(nodes: &[ResultNode], at: GuestAddr) -> Vec<u8> {
    debug_assert!(nodes.len() <= ADDRINFO_NODES_PER_RESULT, "the caller bounds the node count");
    let mut out = vec![0u8; ADDRINFO_RESULT_BYTES];
    let sockaddrs_at = ADDRINFO_NODES_PER_RESULT * ADDRINFO_BYTES;
    for (index, node) in nodes.iter().enumerate() {
        let (sockaddr, addrlen) = encode_sockaddr(&node.address);
        let sockaddr_offset = sockaddrs_at + index * SOCKADDR_SLOT_BYTES;
        out[sockaddr_offset..sockaddr_offset + SOCKADDR_SLOT_BYTES].copy_from_slice(&sockaddr);

        let node_at = index * ADDRINFO_BYTES;
        let field = |offset: usize| node_at + offset;
        // **`ai_flags` is zero on a returned node, not the hints' flags.** Nothing a caller does
        // with the list reads it, and echoing the request back would be this layer describing the
        // question rather than the answer.
        out[field(AI_FLAGS_OFFSET)..field(AI_FLAGS_OFFSET) + 4]
            .copy_from_slice(&0i32.to_le_bytes());
        out[field(AI_FAMILY_OFFSET)..field(AI_FAMILY_OFFSET) + 4]
            .copy_from_slice(&family_number(node.address.family()).to_le_bytes());
        out[field(AI_SOCKTYPE_OFFSET)..field(AI_SOCKTYPE_OFFSET) + 4]
            .copy_from_slice(&node.socktype.to_le_bytes());
        out[field(AI_PROTOCOL_OFFSET)..field(AI_PROTOCOL_OFFSET) + 4]
            .copy_from_slice(&node.protocol.to_le_bytes());
        out[field(AI_ADDRLEN_OFFSET)..field(AI_ADDRLEN_OFFSET) + 4]
            .copy_from_slice(&(addrlen as u32).to_le_bytes());
        // `ai_canonname` stays null. `std::net::ToSocketAddrs` returns no canonical name, so
        // there is none to report — `omni_platform::net::resolve` says so in its own
        // documentation, and a name invented here would be the one the guest logged and trusted.
        out[field(AI_CANONNAME_OFFSET)..field(AI_CANONNAME_OFFSET) + 8]
            .copy_from_slice(&0u64.to_le_bytes());
        let sockaddr_address = (at + sockaddr_offset) as u64;
        out[field(AI_ADDR_OFFSET)..field(AI_ADDR_OFFSET) + 8]
            .copy_from_slice(&sockaddr_address.to_le_bytes());
        let next = if index + 1 < nodes.len() {
            (at + (index + 1) * ADDRINFO_BYTES) as u64
        } else {
            0
        };
        out[field(AI_NEXT_OFFSET)..field(AI_NEXT_OFFSET) + 8].copy_from_slice(&next.to_le_bytes());
    }
    out
}

/// The bounded result slab, with its free list.
///
/// The base address is mapped once in `Bionic::new` — F9's constraint — and never moves. What is
/// dynamic is only which slots are live, which is [`ADDRINFO_RESULTS`] booleans behind one lock.
#[derive(Debug)]
pub struct AddrinfoSlab {
    base: GuestAddr,
    /// The head pointer handed out for each slot, or `None` when the slot is free.
    ///
    /// **The head is stored rather than derived**, although slot *i*'s head is always
    /// `base + i * ADDRINFO_RESULT_BYTES`. Deriving it would make `freeaddrinfo` a range check,
    /// and a range check accepts a pointer into the *middle* of a live list — `res->ai_next`,
    /// which a guest that walked the list and freed as it went would pass. Matching the head
    /// exactly refuses that, which is what a real `freeaddrinfo` does to it as well.
    live: Mutex<[Option<GuestAddr>; ADDRINFO_RESULTS]>,
}

impl AddrinfoSlab {
    /// Build the slab over a region `Bionic::new` has already mapped.
    pub(super) fn new(base: GuestAddr) -> AddrinfoSlab {
        AddrinfoSlab { base, live: Mutex::new([None; ADDRINFO_RESULTS]) }
    }

    /// First address of the slab.
    #[must_use]
    pub fn base(&self) -> GuestAddr {
        self.base
    }

    /// Take a free slot, returning the head pointer the guest will be given.
    ///
    /// `None` when every slot is live, which the caller must turn into a refusal naming the
    /// symbol — never into an overwrite of a list the guest is still walking.
    pub(super) fn take(&self) -> Option<GuestAddr> {
        let mut live = self.live.lock();
        let index = live.iter().position(Option::is_none)?;
        let head = self.base + index * ADDRINFO_RESULT_BYTES;
        live[index] = Some(head);
        Some(head)
    }

    /// Give a slot back, matching on the head pointer that was handed out.
    ///
    /// `true` when a live slot had exactly this head. `false` for anything else — a pointer into
    /// the middle of a list, a slot already freed, a pointer from somewhere else entirely — and
    /// the caller refuses by name, because `freeaddrinfo` returns `void` and a silent no-op there
    /// is indistinguishable from a correct free.
    pub(super) fn release(&self, head: GuestAddr) -> bool {
        let mut live = self.live.lock();
        match live.iter().position(|slot| *slot == Some(head)) {
            Some(index) => {
                live[index] = None;
                true
            }
            None => false,
        }
    }

    /// How many results are live: handed to the guest and not yet freed.
    ///
    /// Diagnostic, and the one number that distinguishes a guest that leaks lists from a slab
    /// that is merely small — a run that refuses with this at [`ADDRINFO_RESULTS`] and a run that
    /// refuses with it at zero are different defects.
    #[must_use]
    pub fn live(&self) -> usize {
        self.live.lock().iter().filter(|slot| slot.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The field order is bionic's, not glibc's**, and the offsets are the LP64 ones.
    ///
    /// The claim that matters — `ai_canonname` before `ai_addr` — is a **compile-time** assertion
    /// beside the constants rather than a line here, because both sides are constants and clippy
    /// is right that `assert!(24 < 32)` in a test folds away to nothing. What this test adds is
    /// the individual offsets, each against its own literal, so that a field moving is a named
    /// failure rather than an arithmetic one.
    ///
    /// **What would falsify it:** `offsetof(struct addrinfo, ai_addr)` on a real device is 32.
    /// There is no NDK on this machine, so that number has not been read from a header here — the
    /// claim is carried by this test and by nothing else.
    #[test]
    fn the_addrinfo_layout_is_bionics_and_not_glibcs() {
        assert_eq!(ADDRINFO_BYTES, 48, "four ints, a socklen_t with padding, three pointers");
        assert_eq!(AI_FLAGS_OFFSET, 0);
        assert_eq!(AI_FAMILY_OFFSET, 4);
        assert_eq!(AI_SOCKTYPE_OFFSET, 8);
        assert_eq!(AI_PROTOCOL_OFFSET, 12);
        assert_eq!(AI_ADDRLEN_OFFSET, 16);
        assert_eq!(AI_CANONNAME_OFFSET, 24, "bionic puts the name first");
        assert_eq!(AI_ADDR_OFFSET, 32, "and the address second; glibc is the other way round");
        assert_eq!(AI_NEXT_OFFSET, 40);
        assert_eq!(AI_NEXT_OFFSET + 8, ADDRINFO_BYTES, "ai_next is the last field");
    }

    /// The two `sockaddr` layouts, field by field, against a value where nothing is symmetrical.
    ///
    /// **The port is asserted big-endian and the scope id little-endian in the same test**,
    /// because the defect this catches is applying one rule to both: 443 byte-swaps to 46,593 and
    /// is not a palindrome, and a scope id of `0x0000_0011` written big-endian lands in the wrong
    /// byte of the word.
    #[test]
    fn a_sockaddr_carries_a_network_order_port_and_host_order_ipv6_words() {
        let (v4, len) = encode_sockaddr(&SocketAddress::V4 { address: [93, 184, 216, 34], port: 443 });
        assert_eq!(len, 16, "sizeof(struct sockaddr_in)");
        assert_eq!(&v4[0..2], &[2, 0], "AF_INET = 2, little-endian u16");
        assert_eq!(&v4[2..4], &[0x01, 0xBB], "443 = 0x01BB, big-endian: network order");
        assert_ne!(&v4[2..4], &[0xBB, 0x01], "and not the guest's own byte order");
        assert_eq!(&v4[4..8], &[93, 184, 216, 34], "the address bytes in the order written");
        assert_eq!(&v4[8..16], &[0u8; 8], "sin_zero");

        let (v6, len) = encode_sockaddr(&SocketAddress::V6 {
            address: [
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55,
            ],
            port: 443,
            flowinfo: 0x000a_bcde,
            scope_id: 17,
        });
        assert_eq!(len, 28, "sizeof(struct sockaddr_in6)");
        assert_eq!(&v6[0..2], &[10, 0], "the GUEST's AF_INET6 is 10; this host's is 23");
        assert_eq!(&v6[2..4], &[0x01, 0xBB], "network order here too");
        assert_eq!(&v6[4..8], &[0xde, 0xbc, 0x0a, 0x00], "sin6_flowinfo is HOST order");
        assert_eq!(&v6[8..24], &[
            0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x02, 0x11, 0x22, 0xff, 0xfe, 0x33, 0x44, 0x55,
        ]);
        assert_eq!(&v6[24..28], &[17, 0, 0, 0], "sin6_scope_id is HOST order");
    }

    /// A guest `sockaddr` round-trips, and the two rejections stay different answers.
    #[test]
    fn a_guest_sockaddr_round_trips_and_a_short_one_is_not_an_unknown_family() {
        for original in [
            SocketAddress::V4 { address: [1, 2, 3, 4], port: 4433 },
            SocketAddress::V6 {
                address: [0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                port: 4433,
                flowinfo: 7,
                scope_id: 9,
            },
        ] {
            let (bytes, len) = encode_sockaddr(&original);
            assert_eq!(decode_sockaddr(&bytes, len), Ok(original));
            // One byte short of the family's own size is EINVAL, not a family failure.
            assert_eq!(
                decode_sockaddr(&bytes, len - 1),
                Err(SockaddrError::TooShort {
                    family: family_number(original.family()),
                    given: len - 1,
                    needed: len
                })
            );
        }
        // `AF_UNIX` is a family this layer has no socket for, and it is reported as one.
        let mut unix = [0u8; SOCKADDR_SLOT_BYTES];
        unix[0..2].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(decode_sockaddr(&unix, SOCKADDR_IN_BYTES), Err(SockaddrError::Family(1)));
    }

    /// A list of two addresses is a chain the guest can walk, with every pointer inside the slot.
    ///
    /// **Checked by reading the bytes back the way the guest reads them** — follow `ai_next`,
    /// dereference `ai_addr` — rather than by comparing against a second copy of the encoder's
    /// own arithmetic, which would agree with any layout.
    #[test]
    fn a_two_node_list_chains_through_ai_next_and_ends_in_a_null() {
        let at: GuestAddr = 0x4000;
        let nodes = [
            ResultNode {
                socktype: 1,
                protocol: 6,
                address: SocketAddress::V4 { address: [10, 0, 0, 1], port: 443 },
            },
            ResultNode {
                socktype: 1,
                protocol: 6,
                address: SocketAddress::V6 {
                    address: [0x20, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
                    port: 443,
                    flowinfo: 0,
                    scope_id: 0,
                },
            },
        ];
        let bytes = encode_result(&nodes, at);
        assert_eq!(bytes.len(), ADDRINFO_RESULT_BYTES);

        let word = |offset: usize| {
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
        };
        let int = |offset: usize| {
            i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
        };

        // Node 0.
        assert_eq!(int(AI_FAMILY_OFFSET), AF_INET);
        assert_eq!(int(AI_ADDRLEN_OFFSET), SOCKADDR_IN_BYTES as i32);
        assert_eq!(word(AI_CANONNAME_OFFSET), 0, "no canonical name is reported");
        let next = word(AI_NEXT_OFFSET);
        assert_eq!(next, (at + ADDRINFO_BYTES) as u64, "ai_next points at the second node");

        // Node 1, reached the way the guest reaches it.
        let second = (next as usize) - at;
        assert_eq!(int(second + AI_FAMILY_OFFSET), AF_INET6);
        assert_eq!(int(second + AI_ADDRLEN_OFFSET), SOCKADDR_IN6_BYTES as i32);
        assert_eq!(word(second + AI_NEXT_OFFSET), 0, "the list ends in a null pointer");

        // And each `ai_addr` points at that node's own sockaddr, inside this slot.
        for (index, node) in nodes.iter().enumerate() {
            let node_at = index * ADDRINFO_BYTES;
            let addr = word(node_at + AI_ADDR_OFFSET) as usize;
            assert!(
                addr >= at && addr + SOCKADDR_SLOT_BYTES <= at + ADDRINFO_RESULT_BYTES,
                "ai_addr {addr:#x} is outside the result slot"
            );
            let offset = addr - at;
            let (expected, len) = encode_sockaddr(&node.address);
            assert_eq!(&bytes[offset..offset + len], &expected[..len]);
        }
        // Nothing past the two nodes was written, so a guest that walked one node too far would
        // find a zeroed node rather than a stale pointer from the previous resolution.
        assert_eq!(
            &bytes[2 * ADDRINFO_BYTES..3 * ADDRINFO_BYTES],
            &[0u8; ADDRINFO_BYTES],
            "the unused nodes are zero"
        );
    }

    /// The free list hands out every slot, refuses when it is full, and takes back only the exact
    /// head it gave.
    ///
    /// The middle assertion is the one worth having: a `freeaddrinfo` implemented as a *range*
    /// check accepts `head + ADDRINFO_BYTES`, which is what a guest walking and freeing as it
    /// goes would pass, and would then hand the slot to the next resolution while the guest was
    /// still reading it.
    #[test]
    fn the_free_list_matches_the_head_exactly_and_refuses_a_full_slab() {
        let slab = AddrinfoSlab::new(0x1_0000);
        let mut heads = Vec::new();
        for expected in 0..ADDRINFO_RESULTS {
            let head = slab.take().expect("a free slot");
            assert_eq!(head, 0x1_0000 + expected * ADDRINFO_RESULT_BYTES);
            heads.push(head);
        }
        assert_eq!(slab.live(), ADDRINFO_RESULTS);
        assert_eq!(slab.take(), None, "a full slab refuses rather than reusing a live slot");

        let head = heads[3];
        assert!(!slab.release(head + ADDRINFO_BYTES), "a pointer into the middle is not a head");
        assert!(!slab.release(head + 1), "nor is a pointer one byte in");
        assert!(!slab.release(0xDEAD_0000), "nor is a pointer from somewhere else");
        assert_eq!(slab.live(), ADDRINFO_RESULTS, "and none of those freed anything");

        assert!(slab.release(head));
        assert_eq!(slab.live(), ADDRINFO_RESULTS - 1);
        assert!(!slab.release(head), "freeing twice is not a free");
        assert_eq!(slab.take(), Some(head), "and the freed slot is the one handed out next");
    }

    /// The slab's arithmetic is its parts, and every slot is inside it and 8-aligned.
    #[test]
    fn every_slot_is_inside_the_slab_and_aligned_for_the_pointers_in_it() {
        assert_eq!(
            ADDRINFO_RESULT_BYTES,
            ADDRINFO_NODES_PER_RESULT * (ADDRINFO_BYTES + SOCKADDR_SLOT_BYTES)
        );
        assert_eq!(ADDRINFO_SLAB_BYTES, ADDRINFO_RESULTS * ADDRINFO_RESULT_BYTES);
        let slab = AddrinfoSlab::new(0x2_0000);
        let mut seen = Vec::new();
        while let Some(head) = slab.take() {
            assert_eq!(head % 8, 0, "a node's three pointers need eight-byte alignment");
            assert!(
                head + ADDRINFO_RESULT_BYTES <= slab.base() + ADDRINFO_SLAB_BYTES,
                "slot at {head:#x} runs past the end of the slab"
            );
            assert!(!seen.contains(&head), "slot {head:#x} was handed out twice");
            seen.push(head);
        }
        assert_eq!(seen.len(), ADDRINFO_RESULTS);
    }
}
