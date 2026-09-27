//! netd's DNS proxy answered by the kernel (`crate::dnsproxy`) where no socket in the host process
//! is bound to `/dev/socket/dnsproxyd` -- an app's host process. First the replies themselves,
//! byte for byte as bionic's resolver client reads them; then a guest program (`fixtures/dnsquery`)
//! resolving through the real bionic and libnetd_client.
mod common;

use std::net::{IpAddr, ToSocketAddrs};

use omni_linux::dnsproxy::{answer, dns_answer};
use omni_linux::ExitStatus;

/// The image's `/system/etc/hosts`.
const AOSP_HOSTS: &str = "127.0.0.1\t\t    localhost\n::1\t\t\t    ip6-localhost\n";

fn be32(b: &[u8], at: &mut usize) -> i32 {
    let v = i32::from_be_bytes(b[*at..*at + 4].try_into().unwrap());
    *at += 4;
    v
}

/// A `getaddrinfo` reply as bionic's `android_getaddrinfo_proxy` reads it: (flags, family,
/// socktype, protocol, address, canonical name) per entry.
fn parse_addrinfo(reply: &[u8]) -> Vec<(i32, i32, i32, i32, std::net::SocketAddr, Option<String>)> {
    assert_eq!(&reply[..4], b"222\0", "{reply:?}");
    let mut at = 4;
    let mut out = Vec::new();
    while be32(reply, &mut at) == 1 {
        let (flags, family, socktype, protocol) = (be32(reply, &mut at), be32(reply, &mut at), be32(reply, &mut at), be32(reply, &mut at));
        let len = be32(reply, &mut at) as usize;
        let sa = &reply[at..at + len];
        at += len;
        let port = u16::from_be_bytes([sa[2], sa[3]]);
        let ip: IpAddr = if family == 2 {
            assert_eq!(len, 16);
            <[u8; 4]>::try_from(&sa[4..8]).unwrap().into()
        } else {
            assert_eq!(len, 28);
            <[u8; 16]>::try_from(&sa[8..24]).unwrap().into()
        };
        let name_len = be32(reply, &mut at) as usize;
        let name = (name_len > 0).then(|| {
            assert_eq!(reply[at + name_len - 1], 0, "the canonical name ends in its NUL");
            String::from_utf8(reply[at..at + name_len - 1].to_vec()).unwrap()
        });
        at += name_len;
        out.push((flags, family, socktype, protocol, std::net::SocketAddr::new(ip, port), name));
    }
    assert_eq!(at, reply.len(), "nothing after the terminating 0");
    out
}

/// A name the host resolves without the network if it can (its own name), else a public one;
/// `None` when the host resolves none of them.
fn host_resolvable() -> Option<(String, Vec<IpAddr>)> {
    let mut names: Vec<String> = ["COMPUTERNAME", "HOSTNAME"].iter().filter_map(|v| std::env::var(v).ok()).collect();
    names.extend(std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_owned()));
    names.push("example.com".to_owned());
    names.into_iter().filter(|n| !n.is_empty()).find_map(|n| {
        let mut ips: Vec<IpAddr> = (n.as_str(), 0).to_socket_addrs().ok()?.map(|a| a.ip()).collect();
        ips.sort();
        ips.dedup();
        (!ips.is_empty()).then_some((n, ips))
    })
}

#[test]
fn getaddrinfo_is_answered_from_the_hosts_file_first() {
    let entries = parse_addrinfo(&answer("getaddrinfo localhost ^ 0 0 1 0 0", AOSP_HOSTS));
    assert_eq!(entries, vec![(0, 2, 1, 6, "127.0.0.1:0".parse().unwrap(), None)]);
    let entries = parse_addrinfo(&answer("getaddrinfo ip6-localhost 443 2 10 2 0 0", AOSP_HOSTS));
    assert_eq!(entries, vec![(2, 10, 2, 17, "[::1]:443".parse().unwrap(), Some("ip6-localhost".into()))]);
    // localhost has no IPv6 address in the file: no address of that family.
    assert_eq!(answer("getaddrinfo localhost ^ 0 10 1 0 0", AOSP_HOSTS), [&b"401\0"[..], &4u32.to_be_bytes(), &7i32.to_le_bytes()].concat());
}

#[test]
fn getaddrinfo_asks_the_host_resolver_for_what_the_file_does_not_name() {
    // localhost with no hosts file: the host's resolver answers it, with loopback addresses.
    let entries = parse_addrinfo(&answer("getaddrinfo localhost 80 0 0 1 0 0", ""));
    assert!(!entries.is_empty() && entries.iter().all(|e| e.4.ip().is_loopback() && e.4.port() == 80), "{entries:?}");
    let Some((name, host_ips)) = host_resolvable() else {
        eprintln!("SKIPPED: the host resolves none of its own name or example.com");
        return;
    };
    let entries = parse_addrinfo(&answer(&format!("getaddrinfo {name} 443 0 0 1 0 100"), AOSP_HOSTS));
    let mut ips: Vec<IpAddr> = entries.iter().map(|e| e.4.ip()).collect();
    ips.sort();
    assert_eq!(ips, host_ips, "{name}: the addresses the host resolves, once each");
    assert!(entries.iter().all(|e| e.2 == 1 && e.3 == 6 && e.4.port() == 443));
    // A name no one has (RFC 6761's .invalid): DnsProxyOperationFailed with an EAI_ code.
    let failed = answer("getaddrinfo no-such-name.invalid ^ 0 0 0 0 0", AOSP_HOSTS);
    assert_eq!(&failed[..8], &[&b"401\0"[..], &4u32.to_be_bytes()].concat()[..]);
    // A service name: this resolver has no services database, and says so (EAI_SERVICE).
    assert_eq!(answer("getaddrinfo localhost https 0 0 1 0 0", AOSP_HOSTS)[8..], 9i32.to_le_bytes());
}

#[test]
fn gethostbyname_and_gethostbyaddr_answer_a_hostent() {
    let mut want = b"222\0".to_vec();
    for w in [10i32, 0, 2, 4, 4] {
        want.extend_from_slice(&w.to_be_bytes());
        if w == 10 {
            want.extend_from_slice(b"localhost\0");
        }
    }
    want.extend_from_slice(&[127, 0, 0, 1]);
    want.extend_from_slice(&0i32.to_be_bytes());
    assert_eq!(answer("gethostbyname 0 localhost 2", AOSP_HOSTS), want);
    assert_eq!(answer("gethostbyaddr 127.0.0.1 4 2 0", AOSP_HOSTS), want);
    // An address the file does not name: not found (there is no reverse resolver to ask).
    assert_eq!(&answer("gethostbyaddr 10.1.2.3 4 2 0", AOSP_HOSTS)[..4], b"401\0");
}

/// `localhost IN A`, as `res_mkquery` makes it (id 0x1234, RD).
fn query(qtype: u8) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 9];
    q.extend_from_slice(b"localhost");
    q.extend_from_slice(&[0, 0, qtype, 0, 1]);
    q
}

#[test]
fn resnsend_answers_a_dns_packet() {
    let (rcode, a) = dns_answer(&query(1));
    assert_eq!(rcode, 0);
    let q = query(1);
    assert_eq!(&a[..2], &[0x12, 0x34], "the query's id");
    assert_eq!(a[2] & 0x80, 0x80, "a response");
    assert_eq!(a[2] & 0x01, 0x01, "RD kept");
    assert_eq!(u16::from_be_bytes([a[6], a[7]]), 1, "one A record: {a:?}");
    assert_eq!(&a[12..q.len()], &q[12..], "the question, as asked");
    let rr = &a[q.len()..];
    assert_eq!(rr, [0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1]);
    assert_eq!(dns_answer(&query(16)).0, 4, "TXT: NOTIMP");
    assert_eq!(dns_answer(&[1, 2, 3]).0, 1, "a short packet: FORMERR");
    // Through the command: base64 in, rcode, length and packet out.
    let b64 = "EjQBAAABAAAAAAAACWxvY2FsaG9zdAAAAQAB";
    let reply = answer(&format!("resnsend 0 0 {b64}"), AOSP_HOSTS);
    assert_eq!(&reply[..4], &0i32.to_be_bytes());
    assert_eq!(i32::from_be_bytes(reply[4..8].try_into().unwrap()) as usize, a.len());
    assert_eq!(&reply[8..], &a[..]);
    assert_eq!(answer("resnsend 0 0 !!!", AOSP_HOSTS), (-84i32).to_be_bytes(), "undecodable: -EILSEQ");
}

/// Without the kernel's answer (connect ENOENT) the host-resolved name and both resnsend checks
/// fail; the hosts-file checks pass either way, bionic answering those names itself.
#[test]
fn a_guest_resolves_names_through_bionic() {
    let resolvable = host_resolvable();
    let name = resolvable.as_ref().map_or("-", |(n, _)| n.as_str());
    let Some((status, out, err)) = common::run_fixture("dnsquery", &[name]) else { return };
    eprintln!("{out}");
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    let want = if resolvable.is_some() { 9 } else { 8 };
    assert_eq!(out.lines().filter(|l| l.starts_with("ok ")).count(), want, "{out}\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "{out}\n{err}");
    if let Some((_, host_ips)) = resolvable {
        let mut ips: Vec<IpAddr> = out.lines().filter_map(|l| l.strip_prefix("addr ")?.parse().ok()).collect();
        ips.sort();
        ips.dedup();
        assert_eq!(ips, host_ips, "the guest's addresses are the host's");
    }
}
