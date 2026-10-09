//! `omni_platform::peer::PeerMemory` across two real host processes: this test binary starts
//! itself again as a child holding memory (`PEER_CHILD=1`), which answers memory requests over a
//! loopback connection as `omni_linux::remote` does (a length, a kind byte, a payload; a read's
//! answer its bytes) -- so one test reads the same bytes both ways, checks the direct path
//! refuses what the child may not be written (a read-only page) and has not committed (a reserved
//! page), and (`--ignored`) times both.
#![cfg(any(windows, target_os = "linux"))]

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use omni_platform::peer::PeerMemory;
use omni_platform::vm;

const MEM_READ: u8 = 10;
const MEM_DATA: u8 = 11;

fn send(s: &mut TcpStream, kind: u8, payload: &[u8]) {
    let mut f = ((1 + payload.len()) as u32).to_le_bytes().to_vec();
    f.push(kind);
    f.extend_from_slice(payload);
    s.write_all(&f).unwrap();
}

fn receive(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut body).ok()?;
    let kind = body.remove(0);
    Some((kind, body))
}

/// The child: memory of three kinds, their addresses on stdout, then reads served until the
/// parent hangs up.
fn child() {
    let page = vm::page_size();
    let data: &'static mut [u8] = Box::leak(vec![0u8; 1 << 16].into_boxed_slice());
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let ro = vm::reserve(page, page).unwrap();
    // SAFETY: the reservation is this process's, one page.
    unsafe {
        vm::commit(ro.as_ptr(), page, vm::Protection::ReadWrite).unwrap();
        ro.as_ptr().write(0x5a);
        vm::protect(ro.as_ptr(), page, vm::Protection::Read).unwrap();
    }
    let reserved = vm::reserve(page, page).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    println!("{} {} {} {}", data.as_ptr() as usize, ro.base(), reserved.base(), listener.local_addr().unwrap().port());
    std::io::stdout().flush().unwrap();
    let (mut s, _) = listener.accept().unwrap();
    s.set_nodelay(true).unwrap();
    while let Some((kind, body)) = receive(&mut s) {
        assert_eq!(kind, MEM_READ);
        let addr = u64::from_le_bytes(body[0..8].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        // SAFETY: the parent asks only inside `data`.
        let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
        send(&mut s, MEM_DATA, &[&[1u8][..], bytes].concat());
    }
}

struct Child {
    proc: std::process::Child,
    data: usize,
    ro: usize,
    reserved: usize,
    conn: TcpStream,
    /// Its stdout, kept open until it ends (its harness writes there last).
    _out: std::io::BufReader<std::process::ChildStdout>,
}

fn start() -> Child {
    let mut proc = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "peer_child_entry", "--nocapture", "--test-threads", "1"])
        .env("PEER_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(proc.stdout.take().unwrap());
    let mut line = String::new();
    // The harness prints its own lines first; the child's is four numbers.
    let nums: Vec<usize> = loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "the child said nothing");
        let n: Vec<usize> = line.split_whitespace().filter_map(|w| w.parse().ok()).collect();
        if n.len() == 4 {
            break n;
        }
    };
    let conn = TcpStream::connect(("127.0.0.1", nums[3] as u16)).unwrap();
    conn.set_nodelay(true).unwrap();
    Child { proc, data: nums[0], ro: nums[1], reserved: nums[2], conn, _out: out }
}

fn tcp_read(c: &mut Child, addr: usize, len: usize) -> Vec<u8> {
    let mut req = (addr as u64).to_le_bytes().to_vec();
    req.extend_from_slice(&(len as u32).to_le_bytes());
    send(&mut c.conn, MEM_READ, &req);
    let (kind, body) = receive(&mut c.conn).unwrap();
    assert_eq!((kind, body[0]), (MEM_DATA, 1));
    body[1..].to_vec()
}

/// Not a test of its own: the child's body, run when this binary is started as the child.
#[test]
fn peer_child_entry() {
    if std::env::var("PEER_CHILD").as_deref() == Ok("1") {
        child();
    }
}

#[test]
fn another_processs_memory_reads_and_writes_as_its_own_and_refuses_what_it_refuses() {
    let mut c = start();
    let data = c.data;
    let peer = PeerMemory::open(c.proc.id()).unwrap();
    let mut direct = vec![0u8; 4096];
    peer.read(c.data + 100, &mut direct).unwrap();
    assert_eq!(direct, tcp_read(&mut c, data + 100, 4096), "the same bytes both ways");
    peer.write(c.data + 7, b"written").unwrap();
    assert_eq!(tcp_read(&mut c, data + 7, 7), b"written", "a direct write is the child's memory");
    let mut one = [0u8; 1];
    peer.read(c.ro, &mut one).unwrap();
    assert_eq!(one, [0x5a], "a read-only page reads");
    assert!(peer.write(c.ro, b"x").is_err(), "a read-only page is not written (nor made writable)");
    peer.read(c.ro, &mut one).unwrap();
    assert_eq!(one, [0x5a]);
    assert!(peer.read(c.reserved, &mut one).is_err(), "a reserved, uncommitted page does not read");
    assert!(peer.write(c.reserved, b"x").is_err(), "nor write");
    drop(c.conn);
    let _ = c.proc.wait();
}

/// **The measurement**: N reads of 64 B and of 4 KiB from the child, over the loopback request and
/// answer (what a stand-in's every guest-memory access costs) and directly.
/// `cargo test --release -p omni-platform --test peer_memory -- --ignored --nocapture`.
#[test]
#[ignore = "timing; run by hand"]
fn peer_memory_against_the_loopback() {
    let mut c = start();
    let data = c.data;
    let peer = PeerMemory::open(c.proc.id()).unwrap();
    for (len, n) in [(64usize, 20_000u32), (4096, 20_000)] {
        let cpu0 = omni_platform::process::cpu_time().unwrap();
        let t = Instant::now();
        for i in 0..n {
            std::hint::black_box(tcp_read(&mut c, data + (i as usize % 8) * 4096, len));
        }
        let (tcp, tcp_cpu) = (t.elapsed() / n, omni_platform::process::cpu_time().unwrap() - cpu0);
        let mut buf = vec![0u8; len];
        let cpu0 = omni_platform::process::cpu_time().unwrap();
        let t = Instant::now();
        for i in 0..n {
            peer.read(c.data + (i as usize % 8) * 4096, &mut buf).unwrap();
            std::hint::black_box(&buf);
        }
        let (direct, direct_cpu) = (t.elapsed() / n, omni_platform::process::cpu_time().unwrap() - cpu0);
        eprintln!(
            "[peer] {len} B x {n}: loopback {tcp:?} a read (this process's CPU {:?} a read), direct {direct:?} a read (CPU {:?})",
            tcp_cpu / n,
            direct_cpu / n
        );
    }
    drop(c.conn);
    let _ = c.proc.wait_timeout_or_kill(Duration::from_secs(5));
}

trait WaitOrKill {
    fn wait_timeout_or_kill(&mut self, d: Duration) -> Option<std::process::ExitStatus>;
}

impl WaitOrKill for std::process::Child {
    fn wait_timeout_or_kill(&mut self, d: Duration) -> Option<std::process::ExitStatus> {
        let t = Instant::now();
        while t.elapsed() < d {
            if let Ok(Some(s)) = self.try_wait() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.kill();
        None
    }
}
