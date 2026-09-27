//! Unix-domain sockets bound to a name (`/dev/socket/dnsproxyd`, an abstract `@name`): the
//! services' side of `connect`. A stream or seqpacket socket that `listen`s is connected to by
//! making a pair (`crate::socket::PairChannel`) whose one end waits for `accept`; a datagram
//! socket receives what a socket connected to it sends, a message at a time. Names are per
//! instance, as `/dev/socket` is per device. init makes a service's sockets so (`socket` in its
//! `.rc` stanza), and hands them over in `ANDROID_SOCKET_<name>`.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use crate::errno::{Errno, EADDRINUSE, ECONNREFUSED};
use crate::socket::{Peer, Socket};

/// A bound socket: its type, whether it listens, and what waits on it.
pub struct Bound {
    pub ty: u64,
    /// The credentials of the process that bound it: what its clients see as their peer.
    pub cred: crate::socket::Cred,
    /// `SO_PASSCRED`, which the connections it accepts inherit.
    pub passcred: std::sync::atomic::AtomicBool,
    pub listening: std::sync::atomic::AtomicBool,
    /// Connections not yet accepted: the server ends.
    backlog: Mutex<VecDeque<Socket>>,
    /// Datagrams not yet received.
    datagrams: Mutex<VecDeque<Vec<u8>>>,
}

type Names = HashMap<(usize, Vec<u8>), Weak<Bound>>;

fn names() -> &'static Mutex<Names> {
    static N: OnceLock<Mutex<Names>> = OnceLock::new();
    N.get_or_init(Mutex::default)
}

impl Bound {
    /// Bind a socket of type `ty` to `name` in instance `instance`.
    ///
    /// # Errors
    /// `EADDRINUSE` when a live socket has the name.
    pub fn bind(instance: usize, name: &[u8], ty: u64, cred: crate::socket::Cred) -> Result<Arc<Self>, Errno> {
        let mut names = names().lock();
        let key = (instance, name.to_vec());
        if names.get(&key).is_some_and(|b| b.strong_count() > 0) {
            return Err(EADDRINUSE);
        }
        let bound = Arc::new(Self { ty, cred, passcred: false.into(), listening: false.into(), backlog: Mutex::default(), datagrams: Mutex::default() });
        names.insert(key, Arc::downgrade(&bound));
        Ok(bound)
    }

    /// Bind to `name`, taking it from whatever held it (init unlinks a service's old socket file
    /// before it makes a new one).
    #[must_use]
    pub fn bind_replacing(instance: usize, name: &[u8], ty: u64, cred: crate::socket::Cred) -> Arc<Self> {
        let bound = Arc::new(Self { ty, cred, passcred: false.into(), listening: false.into(), backlog: Mutex::default(), datagrams: Mutex::default() });
        names().lock().insert((instance, name.to_vec()), Arc::downgrade(&bound));
        bound
    }

    /// The live socket bound to `name`, if any.
    #[must_use]
    pub fn find(instance: usize, name: &[u8]) -> Option<Arc<Self>> {
        names().lock().get(&(instance, name.to_vec())).and_then(Weak::upgrade)
    }

    /// `connect` to this socket: a stream or seqpacket client gets its end of a new pair (the
    /// server's waits for `accept`); a datagram client sends to this socket from now on.
    ///
    /// # Errors
    /// `ECONNREFUSED` when a stream or seqpacket socket does not listen, or the types differ.
    pub fn connect(self: &Arc<Self>, client: &mut Socket, cred: crate::socket::Cred) -> Result<(), Errno> {
        if client.ty != self.ty {
            return Err(ECONNREFUSED);
        }
        if self.ty == 2 {
            client.peer = Some(Peer::Dgram(Arc::clone(self)));
            return Ok(());
        }
        if !self.listening.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ECONNREFUSED);
        }
        let (mine, mut theirs) = crate::socket::pair(self.ty, cred, self.cred);
        theirs.passcred = self.passcred.load(std::sync::atomic::Ordering::SeqCst);
        client.peer = Some(mine);
        self.backlog.lock().push_back(theirs);
        crate::poll::notify();
        Ok(())
    }

    /// The next connection waiting, if any.
    pub fn accept(&self) -> Option<Socket> {
        self.backlog.lock().pop_front()
    }

    /// A datagram sent to this socket.
    pub fn deliver(&self, message: &[u8]) {
        self.datagrams.lock().push_back(message.to_vec());
        crate::poll::notify();
    }

    /// The next datagram, whole (the rest of it discarded when `buf` is short, as a datagram's is).
    pub fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        let msg = self.datagrams.lock().pop_front()?;
        let n = msg.len().min(buf.len());
        buf[..n].copy_from_slice(&msg[..n]);
        Some(n)
    }

    /// Whether `accept` or a receive has something waiting.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.backlog.lock().is_empty() || !self.datagrams.lock().is_empty()
    }
}
