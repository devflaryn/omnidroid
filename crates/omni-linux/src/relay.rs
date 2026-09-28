//! A socket pair's end or a pipe's end handed to another host process (`crate::remote`): the
//! receiver gets a local end of its own, and what either side writes reaches the other over a
//! local connection between the two host processes -- a window's vsync channel from
//! SurfaceFlinger, its input channel from the input dispatcher, a pipe `dumpsys` writes into.
//!
//! Each host process holds one end on the relay's behalf: the sender the end it handed over, the
//! receiver the far end of the pair it made. What that held end is sent (a message, or a pipe's
//! bytes) goes across, framed so a packet's boundaries survive; what comes across is written from
//! it. When the held end's other side closes, the connection's sending half is shut; when the
//! connection ends, the held end is let go -- its peer then sees the hang-up as it would locally.
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::fd::{FileKind, OpenFile};

/// What a held end has to hand across.
pub enum Took {
    /// A message (a pipe's bytes: what was queued).
    Data(Vec<u8>),
    /// Nothing yet.
    Nothing,
    /// Nothing, and nothing will come: its other side is closed.
    Closed,
}

fn take(file: &OpenFile) -> Took {
    match &*file.kind.lock() {
        FileKind::Socket(s) => crate::socket::take(s),
        FileKind::Pipe(end) => crate::pipe::take(end),
        _ => Took::Closed,
    }
}

fn give(file: &OpenFile, bytes: &[u8]) {
    match &mut *file.kind.lock() {
        FileKind::Socket(s) => {
            let _ = crate::socket::send(s, bytes);
        }
        FileKind::Pipe(end) => crate::pipe::give(end, bytes),
        _ => {}
    }
}

/// Whether `file` can be relayed: a socket pair's end or a pipe's end.
#[must_use]
pub fn relayable(file: &OpenFile) -> bool {
    match &*file.kind.lock() {
        FileKind::Socket(s) => crate::socket::is_pair(s),
        FileKind::Pipe(_) => true,
        _ => false,
    }
}

/// The sender's side: hold `file` and wait (a minute at most) for the receiver to connect; the
/// port it connects to.
#[must_use]
pub fn offer(file: Arc<OpenFile>) -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    listener.set_nonblocking(true).ok()?;
    std::thread::Builder::new()
        .name("relay-offer".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        run(file, stream);
                        return;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        })
        .ok()?;
    Some(port)
}

/// The receiver's side: `held` (the far end of the pair made for the receiver) relayed to the
/// sender listening on `port`.
///
/// # Errors
/// The sender cannot be reached.
pub fn attach(held: Arc<OpenFile>, port: u16) -> std::io::Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port))?;
    run(held, stream);
    Ok(())
}

/// Pump both ways on threads of their own.
fn run(file: Arc<OpenFile>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let held = Arc::new(Mutex::new(Some(file)));
    let Ok(mut out) = stream.try_clone() else { return };
    let outbound = Arc::clone(&held);
    let _ = std::thread::Builder::new().name("relay-out".into()).spawn(move || {
        loop {
            let Some(file) = outbound.lock().clone() else { return };
            let watch = crate::poll::watch(crate::poll::key_of(&file).map(|k| vec![k]));
            match take(&file) {
                Took::Data(bytes) => {
                    let mut frame = (bytes.len() as u32).to_le_bytes().to_vec();
                    frame.extend_from_slice(&bytes);
                    if out.write_all(&frame).is_err() {
                        return;
                    }
                }
                Took::Closed => {
                    let _ = out.shutdown(Shutdown::Write);
                    return;
                }
                Took::Nothing => {
                    drop(file);
                    watch.sleep(Instant::now() + Duration::from_millis(50));
                }
            }
        }
    });
    let _ = std::thread::Builder::new().name("relay-in".into()).spawn(move || {
        let mut stream = stream;
        loop {
            let mut len = [0u8; 4];
            if stream.read_exact(&mut len).is_err() {
                break;
            }
            let mut bytes = vec![0u8; u32::from_le_bytes(len) as usize];
            if stream.read_exact(&mut bytes).is_err() {
                break;
            }
            let Some(file) = held.lock().clone() else { break };
            give(&file, &bytes);
        }
        // The other host process's end is gone: let this one go, so its peer sees the hang-up.
        held.lock().take();
        let _ = stream.shutdown(Shutdown::Both);
        crate::poll::notify();
    });
}
