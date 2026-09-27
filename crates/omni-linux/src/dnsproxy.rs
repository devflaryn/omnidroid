//! netd's DNS proxy (`/dev/socket/dnsproxyd`), answered by the kernel for a host process in which
//! no socket is bound to that name. An app runs in a host process of its own, where `crate::unix`
//! names are its process's alone: its bionic resolver would find no `dnsproxyd` there (`ENOENT`),
//! and netd's own resolver has no network to ask. So the kernel speaks netd's `DnsProxyListener`
//! protocol itself and answers from the host's resolver (`omni_platform::net::resolve`, under
//! `crate::hostnet::policy`), after the image's `/system/etc/hosts` as netd consults it (bionic
//! answers the file's names itself before it asks -- measured, `tests/dns_proxy.rs` -- so the file
//! matters here for a caller that asks the proxy directly). Where netd has bound the name (the
//! system's host process), a connect reaches netd as before.
//!
//! The protocol, as bionic (`libc/dns/net/getaddrinfo.c` `android_getaddrinfo_proxy`,
//! `gethnamaddr.c` `android_read_hostent`) and libnetd_client (`NetdClient.cpp` `resNetworkSend`,
//! `resNetworkResult`) speak it: a command is space-separated words ending in a NUL (a
//! `FrameworkListener` command, no sequence number); `^` is a null argument.
//!
//! * `getaddrinfo <host> <service> <flags> <family> <socktype> <protocol> <netid>`: `"222\0"`
//!   (`DnsProxyQueryResult`), then per result a big-endian 1 and the `addrinfo` as netd's
//!   `sendaddrinfo` writes it -- flags, family, socktype, protocol (BE32 each), the address as a
//!   BE32 length and the `sockaddr`, the canonical name as a BE32 length (with its NUL) and bytes
//!   -- and a BE32 0 after the last. A failure is `"401\0"` (`DnsProxyOperationFailed`), BE32 4,
//!   and the `EAI_*` code (`sendBinaryMsg`).
//! * `gethostbyname <netid> <name> <af>` and `gethostbyaddr <addr> <len> <af> <netid>`: `"222\0"`
//!   and the `hostent` as netd's `sendhostent` writes it -- the name (BE32 length with its NUL,
//!   bytes), aliases likewise ending in a 0 length, the address type and length (BE32), the
//!   addresses (BE32 length, bytes) ending in a 0 length. A failure is as above.
//! * `resnsend <netid> <flags> <base64 DNS query>` (every `android_res_nquery`/`res_nsend` of an
//!   app): the answer's rcode (BE32), then its length (BE32) and bytes; a negative errno alone for
//!   a query that could not be sent. The answer is made here from the host resolver's addresses:
//!   an `A` or `AAAA` question is answered (TTL 60 s: the host resolver does not say the record's
//!   own), a name that does not exist is NXDOMAIN, another query type NOTIMP.
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use omni_platform::net::{self as platnet, IpFamily, ResolveFailure, SocketAddress};
use parking_lot::Mutex;

use crate::errno::{Errno, EAGAIN};

const QUERY_RESULT: &[u8; 4] = b"222\0";
const OPERATION_FAILED: &[u8; 4] = b"401\0";

/// bionic's `EAI_*` numbers.
const EAI_AGAIN: i32 = 2;
const EAI_FAIL: i32 = 4;
const EAI_FAMILY: i32 = 5;
const EAI_NODATA: i32 = 7;
const EAI_SERVICE: i32 = 9;
const EAI_SOCKTYPE: i32 = 10;
/// bionic's `AI_*` flags.
const AI_PASSIVE: i32 = 0x1;
const AI_CANONNAME: i32 = 0x2;
const AI_ADDRCONFIG: i32 = 0x400;
/// `HOST_NOT_FOUND`, `TRY_AGAIN`: `h_errno` values.
const HOST_NOT_FOUND: i32 = 1;
const TRY_AGAIN: i32 = 2;

const AF_UNSPEC: i32 = 0;
const AF_INET: i32 = 2;
const AF_INET6: i32 = 10;

/// One connection to the proxy: what the client sent and has not completed a command with, the
/// commands waiting for an answer, and the answers not yet read.
pub struct Proxy {
    hosts: String,
    pending: Mutex<Vec<u8>>,
    queue: Mutex<VecDeque<String>>,
    working: AtomicBool,
    out: Mutex<VecDeque<u8>>,
}

impl Proxy {
    /// A connection, answering from `hosts` (the image's `/system/etc/hosts`) and the host's
    /// resolver.
    #[must_use]
    pub fn new(hosts: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            hosts: String::from_utf8_lossy(hosts).into_owned(),
            pending: Mutex::default(),
            queue: Mutex::default(),
            working: AtomicBool::new(false),
            out: Mutex::default(),
        })
    }

    /// Bytes the client wrote: each command completed by them is answered on a worker thread (a
    /// lookup can take seconds, and nothing waits for it under a lock).
    pub fn send(self: &Arc<Self>, bytes: &[u8]) -> usize {
        let mut pending = self.pending.lock();
        pending.extend_from_slice(bytes);
        while let Some(end) = pending.iter().position(|&b| b == 0) {
            let command = String::from_utf8_lossy(&pending[..end]).into_owned();
            pending.drain(..=end);
            self.queue.lock().push_back(command);
        }
        drop(pending);
        if !self.queue.lock().is_empty() && !self.working.swap(true, Ordering::SeqCst) {
            let me = Arc::clone(self);
            std::thread::Builder::new().name("omni-dnsproxy".into()).spawn(move || me.work()).ok();
        }
        bytes.len()
    }

    fn work(&self) {
        loop {
            let next = self.queue.lock().pop_front();
            match next {
                Some(command) => {
                    let reply = answer(&command, &self.hosts);
                    self.out.lock().extend(reply);
                    crate::poll::notify();
                }
                None => {
                    self.working.store(false, Ordering::SeqCst);
                    if self.queue.lock().is_empty() || self.working.swap(true, Ordering::SeqCst) {
                        return;
                    }
                }
            }
        }
    }

    /// Answers into `buf`, as many bytes as are there and fit.
    ///
    /// # Errors
    /// `EAGAIN` while no answer is there.
    pub fn receive(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut out = self.out.lock();
        if out.is_empty() {
            return Err(EAGAIN);
        }
        let n = buf.len().min(out.len());
        for (slot, b) in buf.iter_mut().zip(out.drain(..n)) {
            *slot = b;
        }
        Ok(n)
    }

    /// Answer bytes not yet read.
    #[must_use]
    pub fn available(&self) -> usize {
        self.out.lock().len()
    }
}

/// The reply to one command (without its NUL).
#[must_use]
pub fn answer(command: &str, hosts: &str) -> Vec<u8> {
    let args: Vec<&str> = command.split(' ').filter(|a| !a.is_empty()).collect();
    let null = |a: &str| (a != "^").then(|| a.to_owned());
    let int = |a: Option<&&str>| a.and_then(|s| s.parse::<i32>().ok());
    match args.first().copied() {
        Some("getaddrinfo") if args.len() >= 8 => {
            let (Some(flags), Some(family), Some(socktype), Some(protocol)) = (int(args.get(3)), int(args.get(4)), int(args.get(5)), int(args.get(6))) else {
                return failed(EAI_FAIL);
            };
            getaddrinfo(null(args[1]).as_deref(), null(args[2]).as_deref(), flags, family, socktype, protocol, hosts)
        }
        Some("gethostbyname") if args.len() >= 4 => match int(args.get(3)) {
            Some(af) => gethostbyname(args[2], af, hosts),
            None => failed(HOST_NOT_FOUND),
        },
        Some("gethostbyaddr") if args.len() >= 5 => match int(args.get(3)) {
            Some(af) => gethostbyaddr(args[1], af, hosts),
            None => failed(HOST_NOT_FOUND),
        },
        Some("resnsend") if args.len() >= 4 => resnsend(args[3]),
        _ => b"500 Command not recognized\0".to_vec(),
    }
}

fn failed(code: i32) -> Vec<u8> {
    let mut b = OPERATION_FAILED.to_vec();
    b.extend_from_slice(&4u32.to_be_bytes());
    b.extend_from_slice(&code.to_le_bytes());
    b
}

fn be32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Addresses `hosts` gives `name` (netd's "files" source, consulted before DNS), in file order.
fn from_hosts(hosts: &str, name: &str) -> Vec<IpAddr> {
    hosts
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or_default();
            let mut words = line.split_whitespace();
            let addr: IpAddr = words.next()?.parse().ok()?;
            words.any(|n| n.eq_ignore_ascii_case(name)).then_some(addr)
        })
        .collect()
}

/// A name `hosts` gives `addr`: its first.
fn name_in_hosts(hosts: &str, addr: IpAddr) -> Option<String> {
    hosts.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or_default();
        let mut words = line.split_whitespace();
        (words.next()?.parse::<IpAddr>().ok()? == addr).then(|| words.next().map(str::to_owned)).flatten()
    })
}

fn family_of(af: i32) -> Option<Option<IpFamily>> {
    match af {
        AF_UNSPEC | -1 => Some(None),
        AF_INET => Some(Some(IpFamily::V4)),
        AF_INET6 => Some(Some(IpFamily::V6)),
        _ => None,
    }
}

/// Look `name` up: the hosts file first, then the host's resolver. `Err` is the failure class.
fn lookup(name: &str, want: Option<IpFamily>, hosts: &str) -> Result<Vec<IpAddr>, ResolveFailure> {
    let listed: Vec<IpAddr> = from_hosts(hosts, name);
    if !listed.is_empty() {
        let wanted: Vec<IpAddr> = listed.into_iter().filter(|a| want.is_none_or(|f| family(a) == f)).collect();
        return if wanted.is_empty() { Err(ResolveFailure::NoAddressOfFamily) } else { Ok(wanted) };
    }
    match platnet::resolve(name, 0, want, &crate::hostnet::policy()) {
        Ok(addrs) => {
            let mut ips: Vec<IpAddr> = Vec::new();
            for a in addrs {
                if !ips.contains(&a.ip()) {
                    ips.push(a.ip());
                }
            }
            Ok(ips)
        }
        Err(e) => {
            if std::env::var("OMNI_NET_TRACE").as_deref() == Ok("1") {
                eprintln!("[dnsproxy] {name}: {e}");
            }
            Err(e.resolve_failure().unwrap_or(ResolveFailure::NonRecoverable))
        }
    }
}

fn family(a: &IpAddr) -> IpFamily {
    if a.is_ipv4() { IpFamily::V4 } else { IpFamily::V6 }
}

/// Whether this machine has a route for `f` -- what `AI_ADDRCONFIG` asks, answered as netd does:
/// a datagram socket connected (nothing is sent) to 8.8.8.8 or 2000::.
fn routable(f: IpFamily) -> bool {
    let target: SocketAddress = match f {
        IpFamily::V4 => std::net::SocketAddr::from(([8, 8, 8, 8], 53)).into(),
        IpFamily::V6 => std::net::SocketAddr::from(([0x2000, 0, 0, 0, 0, 0, 0, 0], 53)).into(),
    };
    platnet::Socket::new(platnet::SocketKind::Datagram, f, crate::hostnet::policy()).is_ok_and(|mut s| s.connect(&target).is_ok())
}

fn getaddrinfo(host: Option<&str>, service: Option<&str>, flags: i32, af: i32, socktype: i32, protocol: i32, hosts: &str) -> Vec<u8> {
    let Some(want) = family_of(af) else { return failed(EAI_FAMILY) };
    let socktype = socktype.max(0);
    let protocol = match (socktype, protocol.max(0)) {
        (1, 0) => 6,
        (2, 0) => 17,
        (0..=3, p) => p,
        _ => return failed(EAI_SOCKTYPE),
    };
    let port = match service {
        None => 0,
        // Service names are the resolver's services database, which this answer has none of.
        Some(s) => match s.parse::<u16>() {
            Ok(p) => p,
            Err(_) => return failed(EAI_SERVICE),
        },
    };
    let addrs: Vec<IpAddr> = match host {
        None => {
            // No host: the wildcard (AI_PASSIVE) or loopback addresses, IPv6 first.
            let passive = flags & AI_PASSIVE != 0;
            let v6: IpAddr = if passive { std::net::Ipv6Addr::UNSPECIFIED.into() } else { std::net::Ipv6Addr::LOCALHOST.into() };
            let v4: IpAddr = if passive { std::net::Ipv4Addr::UNSPECIFIED.into() } else { std::net::Ipv4Addr::LOCALHOST.into() };
            [v6, v4].into_iter().filter(|a| want.is_none_or(|f| family(a) == f)).collect()
        }
        Some(name) => match lookup(name, want, hosts) {
            Ok(a) => a,
            Err(ResolveFailure::Transient) => return failed(EAI_AGAIN),
            Err(_) => return failed(EAI_NODATA),
        },
    };
    let addrs: Vec<IpAddr> = if flags & AI_ADDRCONFIG != 0 {
        let (v4, v6) = (routable(IpFamily::V4), routable(IpFamily::V6));
        addrs.into_iter().filter(|a| a.is_loopback() || if a.is_ipv4() { v4 } else { v6 }).collect()
    } else {
        addrs
    };
    if addrs.is_empty() {
        return failed(EAI_NODATA);
    }
    let mut out = QUERY_RESULT.to_vec();
    for (i, addr) in addrs.iter().enumerate() {
        be32(&mut out, 1);
        let sa = crate::hostnet::sockaddr(&std::net::SocketAddr::new(*addr, port).into());
        be32(&mut out, flags.max(0));
        be32(&mut out, if addr.is_ipv4() { AF_INET } else { AF_INET6 });
        be32(&mut out, socktype);
        be32(&mut out, protocol);
        be32(&mut out, sa.len() as i32);
        out.extend_from_slice(&sa);
        match host {
            Some(name) if i == 0 && flags & AI_CANONNAME != 0 => {
                be32(&mut out, name.len() as i32 + 1);
                out.extend_from_slice(name.as_bytes());
                out.push(0);
            }
            _ => be32(&mut out, 0),
        }
    }
    be32(&mut out, 0);
    out
}

fn hostent(name: &str, af: i32, addrs: &[IpAddr]) -> Vec<u8> {
    let mut out = QUERY_RESULT.to_vec();
    be32(&mut out, name.len() as i32 + 1);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    be32(&mut out, 0); // no aliases
    be32(&mut out, af);
    be32(&mut out, if af == AF_INET { 4 } else { 16 });
    for a in addrs {
        let bytes = match a {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        };
        be32(&mut out, bytes.len() as i32);
        out.extend_from_slice(&bytes);
    }
    be32(&mut out, 0);
    out
}

fn gethostbyname(name: &str, af: i32, hosts: &str) -> Vec<u8> {
    let want = match af {
        AF_INET => IpFamily::V4,
        AF_INET6 => IpFamily::V6,
        _ => return failed(HOST_NOT_FOUND),
    };
    match lookup(name, Some(want), hosts) {
        Ok(addrs) => hostent(name, af, &addrs),
        Err(ResolveFailure::Transient) => failed(TRY_AGAIN),
        Err(_) => failed(HOST_NOT_FOUND),
    }
}

/// A reverse lookup: the hosts file's name for the address. The host layer has no reverse
/// resolver (`omni_platform::net` does no `getnameinfo`), so an address the file does not name
/// is HOST_NOT_FOUND -- not a name made up for it.
fn gethostbyaddr(addr: &str, af: i32, hosts: &str) -> Vec<u8> {
    let Ok(ip) = addr.parse::<IpAddr>() else { return failed(HOST_NOT_FOUND) };
    if (af == AF_INET) != ip.is_ipv4() {
        return failed(HOST_NOT_FOUND);
    }
    match name_in_hosts(hosts, ip) {
        Some(name) => hostent(&name, af, &[ip]),
        None => failed(HOST_NOT_FOUND),
    }
}

// ------------------------------------------------------------------------------------ resnsend

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let body = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in body {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn resnsend(encoded: &str) -> Vec<u8> {
    const EILSEQ: i32 = 84;
    let Some(query) = base64_decode(encoded) else {
        return (-EILSEQ).to_be_bytes().to_vec();
    };
    let (rcode, answer) = dns_answer(&query);
    let mut out = Vec::with_capacity(8 + answer.len());
    be32(&mut out, rcode);
    be32(&mut out, answer.len() as i32);
    out.extend_from_slice(&answer);
    out
}

/// The DNS response to `query` (one question), from the host resolver, and its rcode.
#[must_use]
pub fn dns_answer(query: &[u8]) -> (i32, Vec<u8>) {
    const NOERROR: i32 = 0;
    const FORMERR: i32 = 1;
    const SERVFAIL: i32 = 2;
    const NXDOMAIN: i32 = 3;
    const NOTIMP: i32 = 4;
    let respond = |rcode: i32, question: &[u8], answers: &[Vec<u8>]| -> (i32, Vec<u8>) {
        let mut r = Vec::with_capacity(12 + question.len() + answers.len() * 32);
        r.extend_from_slice(query.get(0..2).unwrap_or(&[0, 0]));
        let flags = query.get(2).copied().unwrap_or(0);
        // QR, the query's opcode and RD; RA; the rcode.
        r.push(0x80 | (flags & 0x79));
        r.push(0x80 | rcode as u8);
        r.extend_from_slice(&u16::from(!question.is_empty()).to_be_bytes());
        r.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        r.extend_from_slice(&[0, 0, 0, 0]);
        r.extend_from_slice(question);
        for a in answers {
            r.extend_from_slice(a);
        }
        (rcode, r)
    };
    if query.len() < 12 || u16::from_be_bytes([query[4], query[5]]) != 1 {
        return respond(FORMERR, &[], &[]);
    }
    // The question: labels to a zero byte, then QTYPE and QCLASS.
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let Some(&len) = query.get(at) else { return respond(FORMERR, &[], &[]) };
        at += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 || at + len as usize > query.len() {
            return respond(FORMERR, &[], &[]);
        }
        labels.push(String::from_utf8_lossy(&query[at..at + len as usize]).into_owned());
        at += len as usize;
    }
    let Some(tail) = query.get(at..at + 4) else { return respond(FORMERR, &[], &[]) };
    let question = &query[12..at + 4];
    let (qtype, qclass) = (u16::from_be_bytes([tail[0], tail[1]]), u16::from_be_bytes([tail[2], tail[3]]));
    let want = match (qtype, qclass) {
        (1, 1) => IpFamily::V4,
        (28, 1) => IpFamily::V6,
        _ => return respond(NOTIMP, question, &[]),
    };
    let name = labels.join(".");
    let addrs = match platnet::resolve(&name, 0, Some(want), &crate::hostnet::policy()) {
        Ok(a) => a,
        Err(e) => {
            return match e.resolve_failure() {
                Some(ResolveFailure::NoSuchHost) => respond(NXDOMAIN, question, &[]),
                // The name has addresses of the other family only: no records of this type.
                Some(ResolveFailure::NoAddressOfFamily) => respond(NOERROR, question, &[]),
                _ => respond(SERVFAIL, question, &[]),
            };
        }
    };
    let mut seen = Vec::new();
    let answers: Vec<Vec<u8>> = addrs
        .iter()
        .filter_map(|a| {
            let ip = a.ip();
            if seen.contains(&ip) {
                return None;
            }
            seen.push(ip);
            let rdata = match ip {
                IpAddr::V4(v4) => v4.octets().to_vec(),
                IpAddr::V6(v6) => v6.octets().to_vec(),
            };
            let mut rr = vec![0xc0, 12]; // the question's name
            rr.extend_from_slice(&qtype.to_be_bytes());
            rr.extend_from_slice(&1u16.to_be_bytes());
            rr.extend_from_slice(&60u32.to_be_bytes());
            rr.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            rr.extend_from_slice(&rdata);
            Some(rr)
        })
        .collect();
    respond(NOERROR, question, &answers)
}
