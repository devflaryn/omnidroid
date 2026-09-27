//! A socket pair's end and a pipe's end carried to another host process (`relay`): what one side
//! writes the other reads, a packet whole, and a close is the other side's hang-up. Both host
//! processes are this one here; the relay between them is the same local connection.
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::fd::{FileKind, OpenFile};
use omni_linux::socket::{self, Socket};

const SOCK_SEQPACKET: u64 = 5;

fn socket_file(s: Socket) -> Arc<OpenFile> {
    Arc::new(OpenFile { kind: parking_lot::Mutex::new(FileKind::Socket(s)), flags: parking_lot::Mutex::new(2) })
}

/// A pair `(a, b)`: two connected sockets.
fn socket_pair() -> (Arc<OpenFile>, Arc<OpenFile>) {
    let (peer, b) = socket::pair(SOCK_SEQPACKET, [0; 3], [0; 3]);
    let a = Socket { domain: 1, ty: SOCK_SEQPACKET, peer: Some(peer), inbox: Default::default(), name: None, protocol: 0, owner: 0, passcred: false };
    (socket_file(a), socket_file(b))
}

fn send(f: &OpenFile, bytes: &[u8]) {
    let FileKind::Socket(s) = &mut *f.kind.lock() else { panic!("socket") };
    socket::send(s, bytes).expect("send");
}

/// Wait (5 s at most) for something to receive: its bytes, or `Ok(0)` at end of file.
fn recv(f: &OpenFile) -> Result<Vec<u8>, i32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut buf = [0u8; 256];
        let r = {
            let FileKind::Socket(s) = &mut *f.kind.lock() else { panic!("socket") };
            socket::receive(s, &mut buf)
        };
        match r {
            Ok(n) => return Ok(buf[..n].to_vec()),
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e.0),
        }
    }
}

/// `sender_end` handed across: the receiver's own end, relayed to it.
fn cross_socket(sender_end: Arc<OpenFile>) -> Arc<OpenFile> {
    let port = omni_linux::relay::offer(sender_end).expect("offer");
    let (mine, other) = socket_pair();
    omni_linux::relay::attach(other, port).expect("attach");
    mine
}

#[test]
fn a_socket_pair_end_carries_packets_both_ways_and_the_hang_up() {
    let (kept, handed) = socket_pair();
    let received = cross_socket(handed);
    send(&kept, b"vsync 1");
    send(&kept, b"vsync 2");
    assert_eq!(recv(&received).expect("first"), b"vsync 1");
    assert_eq!(recv(&received).expect("second"), b"vsync 2");
    send(&received, b"ack");
    assert_eq!(recv(&kept).expect("back"), b"ack");
    drop(kept);
    assert_eq!(recv(&received).expect("hang-up"), b"", "end of file once the far end closes");
}

#[test]
fn the_receiver_closing_hangs_up_the_sender() {
    let (kept, handed) = socket_pair();
    let received = cross_socket(handed);
    drop(received);
    assert_eq!(recv(&kept).expect("hang-up"), b"");
}

#[test]
fn a_pipe_read_end_receives_what_the_sender_writes() {
    let (read, write) = omni_linux::pipe::pair();
    let port = omni_linux::relay::offer(read).expect("offer");
    let (my_read, other_write) = omni_linux::pipe::pair();
    omni_linux::relay::attach(other_write, port).expect("attach");
    {
        let FileKind::Pipe(end) = &*write.kind.lock() else { panic!("pipe") };
        omni_linux::pipe::give(end, b"dump output");
    }
    drop(write);
    let pipe = {
        let FileKind::Pipe(end) = &*my_read.kind.lock() else { panic!("pipe") };
        end.pipe()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    loop {
        let r = {
            let FileKind::Pipe(end) = &*my_read.kind.lock() else { panic!("pipe") };
            omni_linux::pipe::take(end)
        };
        match r {
            omni_linux::relay::Took::Data(d) => got.extend_from_slice(&d),
            omni_linux::relay::Took::Closed => break,
            omni_linux::relay::Took::Nothing => {
                assert!(Instant::now() < deadline, "no end of file (got {got:?}, {} queued)", omni_linux::pipe::available(&pipe));
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    assert_eq!(got, b"dump output");
}
