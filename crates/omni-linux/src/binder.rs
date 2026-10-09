//! `/dev/binder`: Android's IPC driver, protocol version 8, as `drivers/android/binder.c` gives it.
//!
//! Split as the sub-project C design says: a [`Broker`] owns what the kernel owns across processes
//! -- nodes, references and handles, transaction routing and thread selection, the todo queues,
//! death notification -- and speaks in messages; each open of `/dev/binder` (one guest process)
//! owns its `mmap`'d receive area and copies a delivered transaction into it.
//!
//! Objects are translated as the kernel translates them: a local binder sent to another process
//! arrives as a handle in that process's table; a handle sent back to the node's own process
//! arrives as the binder; a file descriptor arrives as a new descriptor in the receiver.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EAGAIN, EFAULT, EINVAL, ENOMEM, EPIPE, ETIMEDOUT};
use crate::fd::OpenFile;
use crate::process::{Process, Task};

// ioctls
const BINDER_WRITE_READ: u64 = 0xc030_6201;
const BINDER_SET_IDLE_TIMEOUT: u64 = 0x4008_6203;
const BINDER_SET_MAX_THREADS: u64 = 0x4004_6205;
const BINDER_SET_IDLE_PRIORITY: u64 = 0x4004_6206;
const BINDER_SET_CONTEXT_MGR: u64 = 0x4004_6207;
const BINDER_THREAD_EXIT: u64 = 0x4004_6208;
const BINDER_VERSION: u64 = 0xc004_6209;
const BINDER_SET_CONTEXT_MGR_EXT: u64 = 0x4018_620d;
const BINDER_ENABLE_ONEWAY_SPAM_DETECTION: u64 = 0x4004_6210;
const BINDER_GET_EXTENDED_ERROR: u64 = 0xc00c_6211;

// BC_* (commands, userspace -> driver)
const BC_TRANSACTION: u32 = 0x4040_6300;
const BC_REPLY: u32 = 0x4040_6301;
const BC_FREE_BUFFER: u32 = 0x4008_6303;
const BC_INCREFS: u32 = 0x4004_6304;
const BC_ACQUIRE: u32 = 0x4004_6305;
const BC_RELEASE: u32 = 0x4004_6306;
const BC_DECREFS: u32 = 0x4004_6307;
const BC_INCREFS_DONE: u32 = 0x4010_6308;
const BC_ACQUIRE_DONE: u32 = 0x4010_6309;
const BC_REGISTER_LOOPER: u32 = 0x0000_630b;
const BC_ENTER_LOOPER: u32 = 0x0000_630c;
const BC_EXIT_LOOPER: u32 = 0x0000_630d;
const BC_REQUEST_DEATH_NOTIFICATION: u32 = 0x400c_630e;
const BC_CLEAR_DEATH_NOTIFICATION: u32 = 0x400c_630f;
const BC_DEAD_BINDER_DONE: u32 = 0x4008_6310;
const BC_TRANSACTION_SG: u32 = 0x4048_6311;
const BC_REPLY_SG: u32 = 0x4048_6312;

// BR_* (returns, driver -> userspace)
const BR_ERROR: u32 = 0x8004_7200;
const BR_OK: u32 = 0x0000_7201;
const BR_TRANSACTION_SEC_CTX: u32 = 0x8048_7202;
const BR_TRANSACTION: u32 = 0x8040_7202;
const BR_REPLY: u32 = 0x8040_7203;
const BR_DEAD_REPLY: u32 = 0x0000_7205;
const BR_TRANSACTION_COMPLETE: u32 = 0x0000_7206;
const BR_INCREFS: u32 = 0x8010_7207;
const BR_ACQUIRE: u32 = 0x8010_7208;
const BR_RELEASE: u32 = 0x8010_7209;
const BR_DECREFS: u32 = 0x8010_720a;
const BR_NOOP: u32 = 0x0000_720c;
const BR_SPAWN_LOOPER: u32 = 0x0000_720d;
const BR_DEAD_BINDER: u32 = 0x8008_720f;
const BR_CLEAR_DEATH_NOTIFICATION_DONE: u32 = 0x8008_7210;
const BR_FAILED_REPLY: u32 = 0x0000_7211;

// flat_binder_object types
const TYPE_BINDER: u32 = 0x7362_2a85;
const TYPE_WEAK_BINDER: u32 = 0x7762_2a85;
const TYPE_HANDLE: u32 = 0x7368_2a85;
const TYPE_WEAK_HANDLE: u32 = 0x7768_2a85;
const TYPE_FD: u32 = 0x6664_2a85;
/// `binder_fd_array_object`: fds in a parent buffer (HIDL's native handles).
const TYPE_FDA: u32 = 0x6664_6185;
/// `binder_buffer_object`: a buffer copied beside the data (scatter-gather; HIDL).
const TYPE_PTR: u32 = 0x7074_2a85;
const BUFFER_FLAG_HAS_PARENT: u32 = 1;

const TF_ONE_WAY: u32 = 0x01;
const FLAT_BINDER_FLAG_TXN_SECURITY_CTX: u32 = 0x1000;
/// What a transaction's sender is labelled when a node asks for a security context.
const SECCTX: &[u8] = b"u:r:untrusted_app:s0\0";

type ProcId = u64;
type NodeId = u64;

/// The host: the owner of host services' nodes and the sender of the host's own transactions. A
/// guest's id counts up from 1, so never meets it.
const HOST: ProcId = u64::MAX;
/// The uid the host's transactions come from: `system`, as a platform service's are.
const HOST_EUID: u32 = 1000;
/// How long the host waits for a reply before giving up on it.
const HOST_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// A transaction to a host service, its objects already translated for the host: a binder the
/// sender passed is a handle in the host's table, a file descriptor is the sender's open file.
pub struct HostCall {
    pub code: u32,
    pub data: Vec<u8>,
    /// Where each object is in `data`.
    pub offsets: Vec<u64>,
    /// The file descriptors it carried, in object order.
    pub fds: Vec<Arc<OpenFile>>,
    /// The binder handles it carried (in the host's table), in object order.
    pub handles: Vec<u32>,
    pub sender_pid: i32,
    pub sender_euid: u32,
}

/// A host service's reply: its parcel, the files to send as the `TYPE_FD` objects at the given
/// offsets of `data`, and the offsets of binder objects in it (a host service's `ptr` as a
/// `TYPE_BINDER`, or a handle of the host's as a `TYPE_HANDLE`), translated for the caller.
#[derive(Default)]
pub struct HostReply {
    pub data: Vec<u8>,
    pub fds: Vec<(usize, Arc<OpenFile>)>,
    pub binders: Vec<usize>,
}

impl HostReply {
    /// A reply of bytes only.
    #[must_use]
    pub fn bytes(data: Vec<u8>) -> Self {
        Self { data, ..Self::default() }
    }
}

/// A host service: given a transaction, its reply.
type HostHandler = Arc<dyn Fn(HostCall) -> HostReply + Send + Sync>;

thread_local! {
    /// On a thread running a host service's handler: the guest thread waiting for its reply, which
    /// a transaction from the handler to that thread's process goes to (the kernel's nested
    /// transaction).
    static HOST_SERVING: std::cell::Cell<Option<(ProcId, i32)>> = const { std::cell::Cell::new(None) };
}

/// `binder_host_pool=1` (`crate::lever`) or `OMNI_BINDER_HOST_POOL=1`: a host service's call runs
/// on a kept thread ([`host_pool_run`]) instead of a new one. **On by default** since the in-world
/// A/B of 2026-10-09 (PS99, 6 pairs: fps +1.6 median, 5/6; all hosts -2..-9 ms/frame);
/// `OMNI_BINDER_HOST_POOL=0` or the lever is the way back. MEASURED (Windows, i7-13700F E-cores,
/// `host_pool_round_trip`, 2000 calls each): handing a call to its thread and hearing it ran took
/// **104-111 us on a new thread, 13-16 us on a kept one** -- plus, off that path, the new thread's
/// exit.
pub static HOST_POOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn host_pool_on() -> bool {
    static FROM_ENV: std::sync::Once = std::sync::Once::new();
    FROM_ENV.call_once(|| {
        if let Ok(v) = std::env::var("OMNI_BINDER_HOST_POOL") {
            HOST_POOL.store(v.trim() == "1", std::sync::atomic::Ordering::Relaxed);
        }
    });
    HOST_POOL.load(std::sync::atomic::Ordering::Relaxed)
}

type HostJob = Box<dyn FnOnce() + Send>;

/// The host-service threads waiting for work, newest last: each one's id and its mailbox.
fn host_idle() -> &'static Mutex<Vec<(u64, std::sync::mpsc::Sender<HostJob>)>> {
    static IDLE: OnceLock<Mutex<Vec<(u64, std::sync::mpsc::Sender<HostJob>)>>> = OnceLock::new();
    IDLE.get_or_init(Mutex::default)
}

/// How long a kept host-service thread waits for another call before it ends.
const HOST_IDLE_FOR: Duration = Duration::from_secs(30);

/// **Run a host service's call on a kept thread** -- one waiting idle if there is one, else a new
/// one, which is kept for the next call once this one is done (ends after [`HOST_IDLE_FOR`] idle).
///
/// Exactly what a thread per call gives, minus the thread's creation: a call never waits for
/// another to finish (the pool has no bound, so a handler that blocks -- on a nested call back
/// into its sender, on a guest's reply -- holds one thread, as before, and the next call takes
/// another), and calls are as unordered as they were (a thread each). Nothing of one call stays on
/// the thread for the next: [`HOST_SERVING`] is cleared after each, and a handler's panic ends its
/// thread as it did. Every frame SurfaceFlinger makes at least one such call (the composer's
/// `executeCommands`), a thread each before.
fn host_pool_run(job: HostJob) -> Result<(), Errno> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut job = job;
    loop {
        let Some((_, mailbox)) = host_idle().lock().pop() else { break };
        // A thread whose mailbox is gone ended (its handler panicked between jobs: never); the next.
        match mailbox.send(job) {
            Ok(()) => return Ok(()),
            Err(std::sync::mpsc::SendError(back)) => job = back,
        }
    }
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel::<HostJob>();
    std::thread::Builder::new()
        .name("omni-binder-host".into())
        .spawn(move || {
            let mut next = Some(job);
            while let Some(job) = next.take() {
                job();
                HOST_SERVING.with(|s| s.set(None));
                host_idle().lock().push((id, tx.clone()));
                next = match rx.recv_timeout(HOST_IDLE_FOR) {
                    Ok(job) => Some(job),
                    Err(_) => {
                        // Idle long enough: leave the list -- unless a caller took this thread off
                        // it meanwhile, whose job is then on its way (or here) and is run.
                        let mut idle = host_idle().lock();
                        match idle.iter().position(|(i, _)| *i == id) {
                            Some(at) => {
                                idle.remove(at);
                                None
                            }
                            None => {
                                drop(idle);
                                rx.recv().ok()
                            }
                        }
                    }
                };
            }
        })
        .map(|_| ())
        .map_err(|_| ENOMEM)
}

struct Node {
    owner: ProcId,
    ptr: u64,
    cookie: u64,
    /// `BR_INCREFS`/`BR_ACQUIRE` sent: the owner holds the object for its remote references.
    held: bool,
    /// `BR_ACQUIRE` sent and its `BC_ACQUIRE_DONE` not yet back: a release waits for it (the
    /// kernel's `pending_strong_ref`), or the owner could drop the object before holding it.
    acquiring: bool,
    /// Released while `acquiring`: `BR_RELEASE`/`BR_DECREFS` go once the acquire is done.
    release_deferred: bool,
    txn_security_ctx: bool,
    dead: bool,
    watchers: Vec<(ProcId, u64)>,
    /// A oneway transaction to this node is out (delivered, its buffer not yet freed): the next
    /// waits in `async_todo`, so one node's oneway calls run one at a time, in order.
    has_async: bool,
    async_todo: VecDeque<Work>,
}

/// A transaction or reply on its way, with its objects already translated for the receiver.
struct Txn {
    reply: bool,
    oneway: bool,
    /// Who waits for the reply to this (a sync transaction): process and thread.
    from: Option<(ProcId, i32)>,
    target_ptr: u64,
    target_cookie: u64,
    secctx: bool,
    code: u32,
    flags: u32,
    sender_pid: i32,
    sender_euid: u32,
    data: Vec<u8>,
    offsets: Vec<u64>,
    /// File descriptors to install in the receiver, by the offset of their object in `data`.
    fds: Vec<(usize, Arc<OpenFile>)>,
    /// Scatter-gather buffers, in object order: (object offset in `data`, bytes, parent).
    sg: Vec<SgBuffer>,
    /// fd arrays: (index of the parent buffer in `sg`, offset in it, the files).
    fda: Vec<(usize, usize, Vec<Arc<OpenFile>>)>,
    /// A oneway transaction's node, whose next oneway waits until this one's buffer is freed.
    async_node: Option<NodeId>,
    /// The nodes it holds in flight ([`State::pin`]): its target's and those its objects name.
    pinned: Vec<NodeId>,
}

/// A delivered buffer not yet freed (`BC_FREE_BUFFER`), by (receiver, address) in
/// [`State::buffers`].
struct Buffer {
    /// A oneway's node: freeing the buffer hands the node's next oneway out.
    async_node: Option<NodeId>,
    /// The nodes the transaction held, let go when the buffer is freed.
    pinned: Vec<NodeId>,
    /// The thread it was delivered to, and the transaction's code (for the stall report).
    tid: i32,
    code: u32,
}

struct SgBuffer {
    obj_off: usize,
    bytes: Vec<u8>,
    /// (index in `sg` of the parent buffer, offset in it where this buffer's address goes).
    parent: Option<(usize, usize)>,
}

enum Work {
    Txn(Box<Txn>),
    Complete,
    /// A sync transaction this thread waits on failed: the wait is over.
    DeadReply,
    FailedReply,
    /// A transaction this thread could not send (the kernel's `thread->return_error`): read as
    /// `cmd`, and no wait ends -- the thread never began one.
    ReturnError(u32),
    Increfs { ptr: u64, cookie: u64 },
    Acquire { ptr: u64, cookie: u64 },
    Release { ptr: u64, cookie: u64 },
    Decrefs { ptr: u64, cookie: u64 },
    DeadBinder { cookie: u64 },
    ClearDeathDone { cookie: u64 },
}

#[derive(Default)]
struct ThreadState {
    todo: VecDeque<Work>,
    /// `BC_ENTER_LOOPER` or `BC_REGISTER_LOOPER` came: the thread may take its process's work.
    /// The kernel never clears it (`BC_EXIT_LOOPER` adds `exited`).
    looper: bool,
    /// `BC_EXIT_LOOPER` came: no longer counted as a looper of the thread pool.
    exited: bool,
    /// Waiting in a read for its process's work (the kernel's `proc->waiting_threads`): a looper
    /// with no transaction on its stack and nothing to do. Cleared whenever its read returns.
    idle: bool,
    /// Transactions this thread is serving, innermost last: whom each reply goes to.
    serving: Vec<Option<(ProcId, i32)>>,
    /// Sync transactions this thread sent and waits on the reply of.
    awaiting: usize,
}

#[derive(Default)]
struct ProcState {
    pid: i32,
    refs: BTreeMap<u32, NodeId>,
    by_node: HashMap<NodeId, u32>,
    /// The last handle number given out: numbers are not reused (a process's cached proxy for a
    /// deleted handle must not name another object).
    last_handle: u32,
    todo: VecDeque<Work>,
    threads: HashMap<i32, ThreadState>,
    max_threads: u32,
    spawn_requested: u32,
    dead: bool,
}

#[derive(Default)]
struct State {
    procs: HashMap<ProcId, ProcState>,
    nodes: HashMap<NodeId, Node>,
    /// Delivered buffers that hold something until freed -- a oneway's node slot, nodes in
    /// flight -- by (receiver, address).
    buffers: HashMap<(ProcId, u64), Buffer>,
    /// Nodes in flight, with how many transactions hold each: named by a transaction on its way
    /// or by a delivered buffer not yet freed. The kernel takes a node reference for each
    /// (`binder_inc_node` as it translates) and drops it with the buffer
    /// (`binder_transaction_buffer_release`): an object in flight is not released by its owner.
    pins: HashMap<NodeId, usize>,
    /// (owner, ptr) -> node
    local: HashMap<(ProcId, u64), NodeId>,
    context_mgr: Option<NodeId>,
    next_proc: ProcId,
    next_node: NodeId,
    /// Host services, by their node's `ptr`.
    host_services: HashMap<u64, HostHandler>,
    next_host_ptr: u64,
    /// The host's transactions each wait as a thread of [`HOST`] of their own.
    next_host_tid: i32,
    /// What the host listens to ([`Broker::tap`]): transactions of one code to one node, by
    /// anyone, whose parcels a host callback reads as they pass.
    taps: Vec<(NodeId, u32, Tap)>,
    /// What the host answers in a node's place ([`Broker::intercept`]).
    intercepts: Vec<(NodeId, u32, Intercept)>,
}

/// A host callback reading a tapped transaction's parcel ([`Broker::tap`]).
pub type Tap = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// A host callback that may answer a transaction in its target's place ([`Broker::intercept`]):
/// `Some(reply)` is the reply's parcel, and the target never hears of the call; `None` lets it go
/// on as usual.
pub type Intercept = Arc<dyn Fn(&[u8]) -> Option<Vec<u8>> + Send + Sync>;

/// The cross-process part of the driver.
#[derive(Default)]
pub struct Broker {
    state: Mutex<State>,
}

/// The binder devices: each its own context, as the kernel's `binder`, `hwbinder` (HIDL) and
/// `vndbinder` are -- a context manager and handles of one are nothing to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    Binder,
    HwBinder,
    VndBinder,
}

/// The broker a device's opens in this host process talk to.
pub fn broker(context: Context) -> Arc<Broker> {
    static BROKERS: OnceLock<Mutex<HashMap<Context, Arc<Broker>>>> = OnceLock::new();
    let brokers = BROKERS.get_or_init(Mutex::default);
    Arc::clone(brokers.lock().entry(context).or_default())
}

impl State {
    fn proc_mut(&mut self, id: ProcId) -> &mut ProcState {
        self.procs.entry(id).or_default()
    }

    /// `id`'s handle for `node`, made (and the node held by its owner) if it has none. When the
    /// owner is the sender of the transaction making the handle, the hold (`BR_INCREFS`,
    /// `BR_ACQUIRE`) goes to the sending thread, ahead of its `BR_TRANSACTION_COMPLETE`, as the
    /// kernel queues it: the object is held before the sender frees the parcel that carried it.
    fn handle_for(&mut self, id: ProcId, node: NodeId, sender: Option<(ProcId, i32)>) -> u32 {
        if self.context_mgr == Some(node) {
            return 0;
        }
        if let Some(h) = self.proc_mut(id).by_node.get(&node) {
            return *h;
        }
        let proc = self.proc_mut(id);
        proc.last_handle = proc.last_handle.max(proc.refs.keys().next_back().copied().unwrap_or(0)) + 1;
        let h = proc.last_handle;
        proc.refs.insert(h, node);
        proc.by_node.insert(node, h);
        let n = self.nodes.get_mut(&node).expect("a node");
        if !n.held {
            n.held = true;
            n.acquiring = true;
            let (owner, ptr, cookie) = (n.owner, n.ptr, n.cookie);
            let thread = sender.filter(|(p, _)| *p == owner).map(|(_, t)| t);
            self.queue(owner, thread, Work::Increfs { ptr, cookie });
            self.queue(owner, thread, Work::Acquire { ptr, cookie });
        }
        h
    }

    fn node_for_handle(&self, id: ProcId, h: u32) -> Option<NodeId> {
        if h == 0 {
            return self.context_mgr;
        }
        self.procs.get(&id)?.refs.get(&h).copied()
    }

    fn local_node(&mut self, owner: ProcId, ptr: u64, cookie: u64, flags: u32) -> NodeId {
        if let Some(n) = self.local.get(&(owner, ptr)) {
            return *n;
        }
        self.next_node += 1;
        let id = self.next_node;
        self.nodes.insert(
            id,
            Node {
                owner,
                ptr,
                cookie,
                held: false,
                acquiring: false,
                release_deferred: false,
                txn_security_ctx: flags & FLAT_BINDER_FLAG_TXN_SECURITY_CTX != 0,
                dead: false,
                watchers: Vec::new(),
                has_async: false,
                async_todo: VecDeque::new(),
            },
        );
        self.local.insert((owner, ptr), id);
        id
    }

    /// A binder or handle object `sender` sends to `target`, as the target receives it: the new
    /// (type, binder or handle, cookie), or `None` when it arrives unchanged. The node it names is
    /// pinned into `pins` until the transaction is done with.
    fn translate_ref(&mut self, sender: (ProcId, i32), target: ProcId, obj: &[u8], pins: &mut Vec<NodeId>) -> Result<Option<(u32, u64, u64)>, Errno> {
        let (kind, flags) = (u32_at(obj, 0), u32_at(obj, 4));
        let (ptr, cookie) = (u64_at(obj, 8), u64_at(obj, 16));
        match kind {
            TYPE_BINDER | TYPE_WEAK_BINDER => {
                let node = self.local_node(sender.0, ptr, cookie, flags);
                self.pin(node, pins);
                if target == sender.0 {
                    return Ok(None);
                }
                let h = self.handle_for(target, node, Some(sender));
                Ok(Some((if kind == TYPE_BINDER { TYPE_HANDLE } else { TYPE_WEAK_HANDLE }, u64::from(h), 0)))
            }
            TYPE_HANDLE | TYPE_WEAK_HANDLE => {
                let node = self.node_for_handle(sender.0, ptr as u32).ok_or(EINVAL)?;
                self.pin(node, pins);
                let n = self.nodes.get(&node).expect("a node");
                if n.owner == target {
                    let kind = if kind == TYPE_HANDLE { TYPE_BINDER } else { TYPE_WEAK_BINDER };
                    Ok(Some((kind, n.ptr, n.cookie)))
                } else {
                    let h = self.handle_for(target, node, Some(sender));
                    Ok(Some((kind, u64::from(h), 0)))
                }
            }
            _ => Err(EINVAL),
        }
    }

    /// A oneway transaction to `node` is done with (its buffer freed, or it was not delivered):
    /// the next waiting one goes to `owner`'s process queue, for whichever thread is available for
    /// process work -- as the kernel's `binder_free_buf` does (`binder_enqueue_work_ilocked(w,
    /// &proc->todo)`). Kernels before 4.14 moved it to the freeing thread's own queue; since then
    /// the kernel never queues a oneway to a thread. The node's oneways still run one at a time,
    /// in order: the next is handed out only now. Handing it to the freeing thread held it on a
    /// thread that only writes (libbinder's `flushCommands`, a thread's exit), and every later
    /// oneway to the node queued behind it.
    fn async_done(&mut self, node: NodeId, owner: ProcId) {
        let next = self.nodes.get_mut(&node).and_then(|n| {
            let w = n.async_todo.pop_front();
            n.has_async = w.is_some();
            w
        });
        if let Some(w) = next {
            self.queue(owner, None, w);
        }
    }

    /// Hold `node` for a transaction in flight (recorded in its `pins`).
    fn pin(&mut self, node: NodeId, pins: &mut Vec<NodeId>) {
        *self.pins.entry(node).or_default() += 1;
        pins.push(node);
    }

    /// A transaction in flight is done with the nodes it held (its buffer freed, or it was
    /// dropped): one no live process refers to any more is released now. How many were.
    fn unpin(&mut self, nodes: Vec<NodeId>) -> usize {
        let mut released = 0;
        for node in nodes {
            let Some(count) = self.pins.get_mut(&node) else { continue };
            *count -= 1;
            if *count == 0 {
                self.pins.remove(&node);
                if self.release_if_unreferenced(node) {
                    released += 1;
                }
            }
        }
        released
    }

    /// Work that will never be read (its process or thread gone): a sync transaction's sender
    /// hears BR_DEAD_REPLY, a oneway's node lets its next one go, and the nodes it held are let
    /// go (the kernel's `binder_release_work`). How many nodes that released.
    fn drop_work(&mut self, receiver: ProcId, work: Work) -> usize {
        let Work::Txn(txn) = work else { return 0 };
        if let (false, false, Some((proc, tid))) = (txn.reply, txn.oneway, txn.from) {
            self.queue(proc, Some(tid), Work::DeadReply);
        }
        if let Some(node) = txn.async_node {
            self.async_done(node, receiver);
        }
        self.unpin(txn.pinned)
    }

    /// No live process refers to `node` any more, and no transaction in flight names it: its
    /// owner lets it go (`BR_RELEASE`, `BR_DECREFS`), and the object's address may later name a
    /// new node.
    fn release_if_unreferenced(&mut self, node: NodeId) -> bool {
        if self.context_mgr == Some(node) || self.pins.contains_key(&node) || self.procs.values().any(|p| !p.dead && p.by_node.contains_key(&node)) {
            return false;
        }
        let Some(n) = self.nodes.get_mut(&node) else { return false };
        if !n.held || n.dead || n.owner == HOST {
            return false;
        }
        n.held = false;
        let (owner, ptr, cookie, acquiring) = (n.owner, n.ptr, n.cookie, n.acquiring);
        if acquiring {
            n.release_deferred = true;
        }
        if self.local.get(&(owner, ptr)) == Some(&node) {
            self.local.remove(&(owner, ptr));
        }
        if !acquiring {
            self.queue(owner, None, Work::Release { ptr, cookie });
            self.queue(owner, None, Work::Decrefs { ptr, cookie });
        }
        true
    }

    /// `BC_ACQUIRE_DONE` from `owner` for its object at `ptr`: a release that waited goes now.
    fn acquire_done(&mut self, owner: ProcId, ptr: u64) {
        let Some((&id, _)) = self.nodes.iter().find(|(_, n)| n.owner == owner && n.ptr == ptr && n.acquiring) else { return };
        let n = self.nodes.get_mut(&id).expect("the node");
        n.acquiring = false;
        if std::mem::take(&mut n.release_deferred) {
            let cookie = n.cookie;
            self.queue(owner, None, Work::Release { ptr, cookie });
            self.queue(owner, None, Work::Decrefs { ptr, cookie });
        }
    }

    /// Why `node`'s oneway calls wait: how many, the code of the next, and where the one that is
    /// out is -- queued for the process or a thread (and whether any looper is free to take it),
    /// or delivered to a thread whose buffer is not yet freed.
    fn async_stall(&self, node: NodeId) -> String {
        let Some(n) = self.nodes.get(&node) else { return format!("node {node} is gone") };
        let pid_of = |id: ProcId| self.procs.get(&id).map_or(0, |pr| pr.pid);
        let code_of = |w: &Work| match w {
            Work::Txn(t) => format!("{:#x}", t.code),
            _ => "?".into(),
        };
        let next = n.async_todo.front().map_or_else(|| "none".into(), code_of);
        let thread_state = |th: &ThreadState| {
            format!("looper {}, awaiting {}, serving {}, {} queued", th.looper && !th.exited, th.awaiting, th.serving.len(), th.todo.len())
        };
        let ours = |w: &Work| matches!(w, Work::Txn(t) if t.async_node == Some(node));
        let owner = self.procs.get(&n.owner);
        let out = owner.and_then(|pr| {
            if let Some(w) = pr.todo.iter().find(|w| ours(w)) {
                let free = pr.threads.values().filter(|th| th.looper && th.awaiting == 0 && th.serving.is_empty() && th.todo.is_empty()).count();
                return Some(format!("code {} queued for the process ({} of {} threads free to take it)", code_of(w), free, pr.threads.len()));
            }
            pr.threads.iter().find_map(|(tid, th)| th.todo.iter().find(|w| ours(w)).map(|w| format!("code {} queued for thread {tid} ({})", code_of(w), thread_state(th))))
        });
        let out = out.or_else(|| {
            self.buffers.iter().find(|(_, b)| b.async_node == Some(node)).map(|((proc, buf), b)| {
                let th = self.procs.get(proc).and_then(|pr| pr.threads.get(&b.tid)).map_or_else(|| "gone".into(), thread_state);
                format!("code {:#x} delivered to thread {} in buffer {buf:#x}, not freed (thread: {th})", b.code, b.tid)
            })
        });
        format!(
            "{} oneway calls wait on node {node} ({:#x}) of pid {}: next code {next}; the one out: {}",
            n.async_todo.len(),
            n.ptr,
            pid_of(n.owner),
            out.unwrap_or_else(|| "none found".into())
        )
    }

    /// Queue a oneway transaction to `node` for its process `target` -- or, while one to the same
    /// node is out, on the node, as the kernel's `binder_proc_transaction` orders them
    /// (`node->has_async_transaction`, `node->async_todo`).
    fn queue_oneway(&mut self, target: ProcId, node: NodeId, mut txn: Txn) {
        txn.async_node = Some(node);
        if let Some(n) = self.nodes.get_mut(&node) {
            if n.has_async {
                n.async_todo.push_back(Work::Txn(Box::new(txn)));
                if n.async_todo.len() % 64 == 0 {
                    eprintln!("[binder] {}", self.async_stall(node));
                }
                return;
            }
            n.has_async = true;
        }
        self.queue(target, None, Work::Txn(Box::new(txn)));
    }

    /// Queue work for a thread of `id`: `tid`'s own queue when given, the process's otherwise.
    fn queue(&mut self, id: ProcId, tid: Option<i32>, work: Work) {
        let proc = self.proc_mut(id);
        match tid {
            Some(t) => proc.threads.entry(t).or_default().todo.push_back(work),
            None => proc.todo.push_back(work),
        }
        crate::poll::notify_key(proc_key(std::ptr::from_ref(self), id));
    }
}

impl State {
    /// A host service's reply as `caller` receives it: its binder objects translated for the
    /// caller, its files attached. `EINVAL` for an object that is not where the reply says.
    fn reply_from_host(&mut self, caller: ProcId, reply: HostReply) -> Result<Txn, Errno> {
        let HostReply { mut data, fds, binders } = reply;
        for (off, _) in &fds {
            if data.get(*off..off + 24).map(|o| u32_at(o, 0)) != Some(TYPE_FD) {
                return Err(EINVAL);
            }
        }
        let binder_offsets: Vec<u64> = binders.iter().map(|&o| o as u64).collect();
        let pinned = self.translate_refs((HOST, 0), caller, &mut data, &binder_offsets)?;
        let mut offsets = binder_offsets;
        offsets.extend(fds.iter().map(|(off, _)| *off as u64));
        offsets.sort_unstable();
        Ok(Txn {
            reply: true,
            oneway: false,
            from: None,
            target_ptr: 0,
            target_cookie: 0,
            secctx: false,
            code: 0,
            flags: 0,
            sender_pid: 0,
            sender_euid: HOST_EUID,
            data,
            offsets,
            fds,
            sg: Vec::new(),
            fda: Vec::new(),
            async_node: None,
            pinned,
        })
    }

    /// Translate the binder and handle objects at `offsets` of `data` (the host's own
    /// transactions carry no others) that `sender` sends to `target`: the nodes they hold in flight.
    /// On an error none is held.
    fn translate_refs(&mut self, sender: (ProcId, i32), target: ProcId, data: &mut [u8], offsets: &[u64]) -> Result<Vec<NodeId>, Errno> {
        let mut pins = Vec::new();
        for &off in offsets {
            let off = off as usize;
            let translated = match data.get(off..off + 24) {
                Some(obj) => self.translate_ref(sender, target, &obj.to_vec(), &mut pins),
                None => Err(EINVAL),
            };
            match translated {
                Ok(Some(to)) => rewrite_ref(data, off, to),
                Ok(None) => {}
                Err(e) => {
                    self.unpin(pins);
                    return Err(e);
                }
            }
        }
        Ok(pins)
    }
}

/// Write a translated binder or handle object (type, binder or handle, cookie) at `off`.
fn rewrite_ref(data: &mut [u8], off: usize, (kind, value, cookie): (u32, u64, u64)) {
    data[off..off + 4].copy_from_slice(&kind.to_le_bytes());
    data[off + 8..off + 16].copy_from_slice(&value.to_le_bytes());
    data[off + 16..off + 24].copy_from_slice(&cookie.to_le_bytes());
}

impl Broker {
    /// A binder service served by the host: a node owned by [`HOST`] whose transactions `handler`
    /// answers. Its `ptr` (the node's binder in a parcel) is returned; the service lives as long as
    /// the broker.
    pub fn create_host_service(&self, handler: impl Fn(u32, &[u8]) -> Vec<u8> + Send + Sync + 'static) -> u64 {
        self.create_host_service_objects(move |call| HostReply::bytes(handler(call.code, &call.data)))
    }

    /// A host service that sees the objects a transaction carries and may send objects back
    /// ([`HostCall`], [`HostReply`]).
    pub fn create_host_service_objects(&self, handler: impl Fn(HostCall) -> HostReply + Send + Sync + 'static) -> u64 {
        let mut st = self.state.lock();
        st.proc_mut(HOST);
        st.next_host_ptr += 1;
        let ptr = st.next_host_ptr;
        st.host_services.insert(ptr, Arc::new(handler));
        let node = st.local_node(HOST, ptr, ptr, 0);
        // Held from the start: the host has no looper to hear `BR_INCREFS`/`BR_ACQUIRE`.
        st.nodes.get_mut(&node).expect("the node").held = true;
        ptr
    }

    /// Hear every transaction of `code` that anyone sends to the object the host's `handle` names:
    /// `tap` reads its parcel as it passes (the objects in it not yet translated), on the sender's
    /// thread, under the broker's lock -- so it must be quick and must not call the broker. Nothing
    /// is changed or delayed for the transaction. `false` for a handle the host does not have.
    pub fn tap(&self, handle: u32, code: u32, tap: Tap) -> bool {
        let mut st = self.state.lock();
        let Some(node) = st.node_for_handle(HOST, handle) else { return false };
        st.taps.retain(|(n, c, _)| (*n, *c) != (node, code));
        st.taps.push((node, code, tap));
        true
    }

    /// Answer, in the place of the object the host's `handle` names, the sync transactions of
    /// `code` that `intercept` takes: it reads each parcel (its objects not yet translated) on the
    /// sender's thread, under the broker's lock -- so, as a [`Self::tap`], it must be quick and must
    /// not call the broker -- and a `Some` reply goes straight back to the sender, the target never
    /// hearing of the call. One-way transactions are never intercepted. `false` for a handle the
    /// host does not have.
    pub fn intercept(&self, handle: u32, code: u32, intercept: Intercept) -> bool {
        let mut st = self.state.lock();
        let Some(node) = st.node_for_handle(HOST, handle) else { return false };
        st.intercepts.retain(|(n, c, _)| (*n, *c) != (node, code));
        st.intercepts.push((node, code, intercept));
        true
    }

    /// A sync transaction from the host to `handle` (in the host's handle table; 0 is the context
    /// manager): the reply's parcel. `data` may carry the host's own services as binder objects,
    /// at `offsets`.
    ///
    /// # Errors
    /// `EINVAL` for an unknown handle or an object that is not a binder or handle, `EPIPE` when the
    /// target is dead or dies before replying, `ETIMEDOUT` when no reply comes in time.
    pub fn host_transact(&self, handle: u32, code: u32, data: Vec<u8>, offsets: &[u64]) -> Result<Vec<u8>, Errno> {
        self.host_transact_with_fds(handle, code, data, offsets).map(|(reply, _)| reply)
    }

    /// [`Self::host_transact`], and the file descriptors the reply carried, in object order (a
    /// `ParcelFileDescriptor` answered: `IActivityManager.openContentUri`).
    ///
    /// # Errors
    /// As [`Self::host_transact`].
    pub fn host_transact_with_fds(&self, handle: u32, code: u32, data: Vec<u8>, offsets: &[u64]) -> Result<(Vec<u8>, Vec<Arc<OpenFile>>), Errno> {
        self.host_transact_as(HOST_EUID, handle, code, data, offsets)
    }

    /// [`Self::host_transact_with_fds`], the caller seen as `euid` (`Binder.getCallingUid()`) rather
    /// than the system's: a service that grants by the caller's package checks the package against
    /// it (`ClipboardService` reading the clipboard as the shell, uid 2000).
    ///
    /// # Errors
    /// As [`Self::host_transact`].
    pub fn host_transact_as(&self, euid: u32, handle: u32, code: u32, mut data: Vec<u8>, offsets: &[u64]) -> Result<(Vec<u8>, Vec<Arc<OpenFile>>), Errno> {
        let mut st = self.state.lock();
        st.proc_mut(HOST);
        st.next_host_tid += 1;
        let tid = st.next_host_tid;
        let node = st.node_for_handle(HOST, handle).ok_or(EINVAL)?;
        let n = st.nodes.get(&node).expect("a node");
        let (target, ptr, cookie, secctx, dead) = (n.owner, n.ptr, n.cookie, n.txn_security_ctx, n.dead);
        if dead || st.procs.get(&target).is_none_or(|pr| pr.dead) {
            return Err(EPIPE);
        }
        let mut pinned = st.translate_refs((HOST, tid), target, &mut data, offsets)?;
        st.pin(node, &mut pinned);
        let txn = Txn {
            reply: false,
            oneway: false,
            from: Some((HOST, tid)),
            target_ptr: ptr,
            target_cookie: cookie,
            secctx,
            code,
            flags: 0,
            sender_pid: 0,
            sender_euid: euid,
            data,
            offsets: offsets.to_vec(),
            fds: Vec::new(),
            sg: Vec::new(),
            fda: Vec::new(),
            async_node: None,
            pinned,
        };
        let nested = HOST_SERVING.with(std::cell::Cell::get).filter(|(p, _)| *p == target).map(|(_, tid)| tid);
        st.queue(target, nested, Work::Txn(Box::new(txn)));
        let deadline = Instant::now() + HOST_REPLY_TIMEOUT;
        let result = loop {
            let seen = crate::poll::generation();
            let work = st.proc_mut(HOST).threads.entry(tid).or_default().todo.pop_front();
            match work {
                Some(Work::Txn(r)) if r.reply => {
                    let mut r = *r;
                    r.fds.sort_by_key(|(at, _)| *at);
                    let fds = std::mem::take(&mut r.fds).into_iter().map(|(_, f)| f).collect();
                    break Ok((std::mem::take(&mut r.data), fds));
                }
                Some(Work::DeadReply | Work::FailedReply | Work::ReturnError(_)) => break Err(EPIPE),
                Some(_) => continue,
                None if Instant::now() >= deadline => break Err(ETIMEDOUT),
                None => {
                    drop(st);
                    crate::poll::wait_for_change_host(seen, deadline);
                    st = self.state.lock();
                }
            }
        };
        st.proc_mut(HOST).threads.remove(&tid);
        result
    }

    /// A one-way transaction from the host to `handle` (a HAL calling back its client:
    /// `IComposerCallback.onVsync`): queued for the target's process, not waited on -- after the
    /// node's earlier oneways, one at a time, as any sender's.
    ///
    /// # Errors
    /// `EINVAL` for an unknown handle or an object that is not a binder or handle, `EPIPE` when the
    /// target is dead.
    pub fn host_transact_oneway(&self, handle: u32, code: u32, mut data: Vec<u8>, offsets: &[u64]) -> Result<(), Errno> {
        let mut st = self.state.lock();
        st.proc_mut(HOST);
        let node = st.node_for_handle(HOST, handle).ok_or(EINVAL)?;
        let n = st.nodes.get(&node).expect("a node");
        let (target, ptr, cookie, secctx, dead) = (n.owner, n.ptr, n.cookie, n.txn_security_ctx, n.dead);
        if dead || st.procs.get(&target).is_none_or(|pr| pr.dead) {
            return Err(EPIPE);
        }
        let mut pinned = st.translate_refs((HOST, 0), target, &mut data, offsets)?;
        st.pin(node, &mut pinned);
        let txn = Txn {
            reply: false,
            oneway: true,
            from: None,
            target_ptr: ptr,
            target_cookie: cookie,
            secctx,
            code,
            flags: TF_ONE_WAY,
            sender_pid: 0,
            sender_euid: HOST_EUID,
            data,
            offsets: offsets.to_vec(),
            fds: Vec::new(),
            sg: Vec::new(),
            fda: Vec::new(),
            async_node: None,
            pinned,
        };
        st.queue_oneway(target, node, txn);
        Ok(())
    }

    /// A guest service by name (`IServiceManager.checkService`, which does not wait for one): its
    /// handle in the host's table, `None` while none is published.
    ///
    /// # Errors
    /// The transaction's failure, or the exception `servicemanager` answered with.
    pub fn check_service(&self, name: &str) -> Result<Option<u32>, String> {
        let mut parcel = Parcel::with_interface_token(SERVICE_MANAGER);
        parcel.string16(name);
        let reply = self.host_transact(0, CHECK_SERVICE, parcel.bytes, &[]).map_err(|e| format!("transaction: errno {}", e.0))?;
        match reply.get(0..4).map(|b| i32::from_le_bytes(b.try_into().expect("4"))) {
            Some(0) => Ok(reply.get(4..24).filter(|o| u32_at(o, 0) == TYPE_HANDLE).map(|o| u32_at(o, 8))),
            Some(exception) => Err(format!("exception {exception}: {}", read_string16(&reply, 4).unwrap_or_default())),
            None => Err("an empty reply".into()),
        }
    }

    /// Publish host service `ptr` as `name` with `servicemanager` (`IServiceManager.addService`).
    ///
    /// # Errors
    /// The transaction's failure, or the exception `servicemanager` answered with.
    pub fn add_service(&self, name: &str, ptr: u64) -> Result<(), String> {
        self.add_service_with_stability(name, ptr, STABILITY_SYSTEM)
    }

    /// [`Self::add_service`] with the stability written after the binder: [`STABILITY_VINTF`] for a
    /// HAL, which `servicemanager` then requires to be declared in the VINTF manifest.
    ///
    /// # Errors
    /// The transaction's failure, or the exception `servicemanager` answered with.
    pub fn add_service_with_stability(&self, name: &str, ptr: u64, stability: i32) -> Result<(), String> {
        let mut parcel = Parcel::with_interface_token(SERVICE_MANAGER);
        parcel.string16(name);
        let object = parcel.binder(ptr, stability);
        parcel.i32(0); // allowIsolated
        parcel.i32(DUMP_FLAG_PRIORITY_DEFAULT);
        let reply = self.host_transact(0, ADD_SERVICE, parcel.bytes, &[object]).map_err(|e| format!("transaction: errno {}", e.0))?;
        match reply.get(0..4).map(|b| i32::from_le_bytes(b.try_into().expect("4"))) {
            Some(0) => Ok(()),
            Some(exception) => Err(format!("exception {exception}: {}", read_string16(&reply, 4).unwrap_or_default())),
            None => Err("an empty reply".into()),
        }
    }
}

/// `IServiceManager`'s interface token and the transaction codes of its AIDL (Android 15).
const SERVICE_MANAGER: &str = "android.os.IServiceManager";
const CHECK_SERVICE: u32 = 2;
const ADD_SERVICE: u32 = 3;
/// `IServiceManager.DUMP_FLAG_PRIORITY_DEFAULT`.
const DUMP_FLAG_PRIORITY_DEFAULT: i32 = 1 << 3;
/// libbinder's `Stability::Level::SYSTEM`, written after a binder in a parcel.
pub const STABILITY_SYSTEM: i32 = 0b00_1100;
/// libbinder's `Stability::Level::VINTF`: a HAL's binder, usable from the system and vendor sides.
pub const STABILITY_VINTF: i32 = 0b11_1111;
/// libbinder's interface header for the system partition, `'SYST'`.
const INTERFACE_HEADER_SYSTEM: i32 = 0x5359_5354;
/// The strict-mode word libbinder's `writeInterfaceToken` leads with (as `service call` sends it).
const STRICT_MODE_PENALTY_GATHER: i32 = i32::MIN;
/// `IBinder::UNSET_WORKSOURCE`.
const UNSET_WORK_SOURCE: i32 = -1;
const FLAT_BINDER_FLAG_ACCEPTS_FDS: u32 = 0x100;

/// A parcel as libbinder writes one, for the host's transactions.
pub(crate) struct Parcel {
    pub(crate) bytes: Vec<u8>,
}

impl Parcel {
    /// `Parcel::writeInterfaceToken`, byte for byte as the image's libbinder writes it: strict-mode
    /// policy, work source, the partition header, the interface's name.
    pub(crate) fn with_interface_token(interface: &str) -> Self {
        let mut p = Self { bytes: Vec::new() };
        p.i32(STRICT_MODE_PENALTY_GATHER);
        p.i32(UNSET_WORK_SOURCE);
        p.i32(INTERFACE_HEADER_SYSTEM);
        p.string16(interface);
        p
    }

    /// `writeInt32` (and `writeByte`, `writeBool`, `writeUint32`: libbinder widens them to 32 bits).
    pub(crate) fn i32(&mut self, v: i32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeInt64`: 8 bytes where the parcel is (libbinder aligns to 4 only).
    pub(crate) fn i64(&mut self, v: i64) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn f32(&mut self, v: f32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// `writeByteVector`: the length, then the bytes padded to 4.
    pub(crate) fn byte_vector(&mut self, v: &[u8]) {
        self.i32(v.len() as i32);
        self.bytes.extend_from_slice(v);
        self.bytes.resize((self.bytes.len() + 3) & !3, 0);
    }

    /// Its length in UTF-16 units, the units and a NUL, padded to 4 bytes.
    pub(crate) fn string16(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
        self.i32(units.len() as i32 - 1);
        for u in units {
            self.bytes.extend_from_slice(&u.to_le_bytes());
        }
        self.bytes.resize((self.bytes.len() + 3) & !3, 0);
    }

    /// A null `String16` (`writeString16(null)`): length -1.
    pub(crate) fn null_string16(&mut self) {
        self.i32(-1);
    }

    /// `writeString8`: the length in bytes (-1: null), the UTF-8 bytes and a NUL, padded to 4.
    pub(crate) fn string8(&mut self, s: Option<&str>) {
        let Some(s) = s else {
            self.i32(-1);
            return;
        };
        self.i32(s.len() as i32);
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
        self.bytes.resize((self.bytes.len() + 3) & !3, 0);
    }

    /// `writeStrongBinder` of host service `ptr`: a `flat_binder_object` and its stability. The
    /// object's offset, for the offsets array.
    pub(crate) fn binder(&mut self, ptr: u64, stability: i32) -> u64 {
        let at = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&TYPE_BINDER.to_le_bytes());
        self.bytes.extend_from_slice(&FLAT_BINDER_FLAG_ACCEPTS_FDS.to_le_bytes());
        self.bytes.extend_from_slice(&ptr.to_le_bytes());
        self.bytes.extend_from_slice(&ptr.to_le_bytes());
        self.i32(stability);
        at
    }
}

/// The key a process's work is told by: its broker's state and its id.
fn proc_key(state: *const State, id: ProcId) -> crate::poll::Key {
    (state as crate::poll::Key).wrapping_add((id as crate::poll::Key).wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as crate::poll::Key))
}

/// A `String16` at `at` in a parcel.
fn read_string16(b: &[u8], at: usize) -> Option<String> {
    let len = usize::try_from(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)).ok()?;
    let units: Vec<u16> = b.get(at + 4..at + 4 + len * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8"))
}

/// The per-process side: one open of `/dev/binder`.
pub struct BinderFile {
    broker: Arc<Broker>,
    id: ProcId,
    area: Mutex<Area>,
    /// [`BinderFile::release`] has run.
    released: std::sync::atomic::AtomicBool,
    /// Descriptors on it closed so far ([`BinderFile::flush`]): a read waiting across one returns.
    flushes: std::sync::atomic::AtomicU64,
}

/// The `mmap`'d receive area and its allocator.
#[derive(Default)]
struct Area {
    base: u64,
    size: u64,
    /// offset -> length, of buffers handed to userspace and not yet freed.
    used: BTreeMap<u64, u64>,
}

impl Area {
    fn alloc(&mut self, len: u64) -> Option<u64> {
        let len = (len.max(8) + 7) & !7;
        let mut at = 0;
        for (&off, &l) in &self.used {
            if off - at >= len {
                break;
            }
            at = off + l;
        }
        if at + len > self.size {
            return None;
        }
        self.used.insert(at, len);
        Some(self.base + at)
    }

    fn free(&mut self, addr: u64) {
        if addr >= self.base {
            self.used.remove(&(addr - self.base));
        }
    }
}

impl BinderFile {
    #[must_use]
    pub fn open(context: Context) -> Arc<Self> {
        let broker = broker(context);
        let id = {
            let mut st = broker.state.lock();
            st.next_proc += 1;
            let id = st.next_proc;
            st.procs.insert(id, ProcState { max_threads: 0, ..ProcState::default() });
            id
        };
        Arc::new(Self { broker, id, area: Mutex::default(), released: std::sync::atomic::AtomicBool::new(false), flushes: std::sync::atomic::AtomicU64::new(0) })
    }

    /// What work queued for this process is told by (`crate::poll::notify_key`).
    #[must_use]
    pub fn key(&self) -> crate::poll::Key {
        proc_key(self.broker.state.data_ptr(), self.id)
    }

    /// `EPOLLIN` when there is work a looper of this process could take.
    #[must_use]
    pub fn readiness(&self) -> u32 {
        let st = self.broker.state.lock();
        st.procs.get(&self.id).map_or(0, |p| {
            if !p.todo.is_empty() || p.threads.values().any(|t| !t.todo.is_empty()) {
                crate::poll::IN
            } else {
                0
            }
        })
    }

    /// The receive area: the guest's `mmap` of this descriptor.
    pub fn set_area(&self, base: u64, size: u64) {
        let mut a = self.area.lock();
        a.base = base;
        a.size = size;
    }
}

impl Drop for BinderFile {
    fn drop(&mut self) {
        self.release();
    }
}

impl BinderFile {
    /// A descriptor on it was closed (the kernel's `binder_flush`): every thread waiting in a
    /// read returns, so a process closing the driver (`IPCThreadState::stopProcess`) or exiting
    /// is not kept by a thread parked in its thread pool -- the last close then releases it.
    pub fn flush(&self) {
        self.flushes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::poll::notify();
    }

    /// The process is gone (its last descriptor on the driver closed, as the kernel's
    /// `binder_release` -- even while one of its threads still waits in a read): its nodes die,
    /// the watchers of each hear so, every sync transaction waiting on it fails, and its
    /// references are dropped. Once.
    pub fn release(&self) {
        if self.released.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let mut st = self.broker.state.lock();
        // Work that will never be read: the oneways queued on its nodes, its queues.
        let mut dropped: Vec<Work> = Vec::new();
        let dying: Vec<NodeId> = st.nodes.iter().filter(|(_, n)| n.owner == self.id).map(|(id, _)| *id).collect();
        for node in dying {
            let watchers = {
                let n = st.nodes.get_mut(&node).expect("node");
                n.dead = true;
                n.has_async = false;
                dropped.extend(n.async_todo.drain(..));
                std::mem::take(&mut n.watchers)
            };
            for (proc, cookie) in watchers {
                st.queue(proc, None, Work::DeadBinder { cookie });
            }
            if st.context_mgr == Some(node) {
                st.context_mgr = None;
            }
        }
        // Its references are gone with it: an object no live process refers to any more is let go
        // by its owner (`BR_RELEASE`, `BR_DECREFS`), as the kernel drops a dead process's refs --
        // SurfaceFlinger removes a client's layers when their handles are released so. One still
        // in flight -- in a transaction queued elsewhere, or in a buffer not yet freed -- waits
        // for that.
        let mut serving = Vec::new();
        let mut held = Vec::new();
        if let Some(p) = st.procs.get_mut(&self.id) {
            p.dead = true;
            dropped.extend(p.todo.drain(..));
            for (_, th) in p.threads.drain() {
                dropped.extend(th.todo);
                serving.extend(th.serving.into_iter().flatten());
            }
            p.by_node.clear();
            held = std::mem::take(&mut p.refs).into_values().collect();
        }
        // Every sync transaction waiting on it fails.
        for (proc, tid) in serving {
            st.queue(proc, Some(tid), Work::DeadReply);
        }
        let mut released = 0;
        for work in dropped {
            released += st.drop_work(self.id, work);
        }
        let buffers: Vec<(ProcId, u64)> = st.buffers.keys().filter(|(proc, _)| *proc == self.id).copied().collect();
        for key in buffers {
            if let Some(b) = st.buffers.remove(&key) {
                released += st.unpin(b.pinned);
            }
        }
        for node in held {
            if st.release_if_unreferenced(node) {
                released += 1;
            }
        }
        if trace_level() > 0 || released > 0 {
            let pid = st.procs.get(&self.id).map_or(0, |p| p.pid);
            eprintln!("[binder] pid {pid} closed its driver: {released} objects released");
        }
        crate::poll::notify();
    }
}

/// `mmap` of `/dev/binder`: the receive area, which only the driver writes (the guest's mapping
/// of it is read-only; to the host it is ordinary writable memory the driver fills).
pub fn mmap(p: &Process, t: &Task, file: &Arc<BinderFile>, len: u64) -> Result<u64, Errno> {
    let at = p.mm.map(p, t, crate::mm::MapRequest { addr: 0, len, prot: 3, flags: 0x22, fd: -1, offset: 0 })?;
    p.mm.label(at, (len + p.mm.page_size() - 1) & !(p.mm.page_size() - 1), b"/dev/binderfs/binder");
    file.set_area(at, len);
    Ok(at)
}

/// `nonblocking`: the descriptor is O_NONBLOCK -- a read with nothing to hand out answers EAGAIN.
pub fn ioctl(p: &Process, t: &mut Task, file: &Arc<BinderFile>, cmd: u64, arg: u64, nonblocking: bool) -> SysResult {
    match cmd {
        BINDER_VERSION => p.mem.write_u32(arg, 8).map(|()| 0),
        BINDER_SET_MAX_THREADS => {
            let n = p.mem.read_u32(arg)?;
            file.broker.state.lock().proc_mut(file.id).max_threads = n;
            Ok(0)
        }
        BINDER_SET_IDLE_TIMEOUT | BINDER_SET_IDLE_PRIORITY | BINDER_ENABLE_ONEWAY_SPAM_DETECTION => Ok(0),
        BINDER_GET_EXTENDED_ERROR => p.mem.write(arg, &[0u8; 12]).map(|()| 0),
        BINDER_SET_CONTEXT_MGR | BINDER_SET_CONTEXT_MGR_EXT => {
            let flags = if cmd == BINDER_SET_CONTEXT_MGR_EXT { p.mem.read_u32(arg + 4)? } else { 0 };
            let mut st = file.broker.state.lock();
            if st.context_mgr.is_some_and(|n| st.nodes.get(&n).is_some_and(|n| !n.dead)) {
                return Err(crate::errno::EBUSY);
            }
            let node = st.local_node(file.id, 0, 0, flags);
            if let Some(n) = st.nodes.get_mut(&node) {
                n.held = true; // the context manager lives as long as its process
            }
            st.context_mgr = Some(node);
            Ok(0)
        }
        BINDER_THREAD_EXIT => {
            // The kernel's `binder_thread_release`: the caller of the transaction it was serving
            // hears BR_DEAD_REPLY, and the work left in its queue is dropped as a dead process's.
            let mut st = file.broker.state.lock();
            if let Some(th) = st.procs.get_mut(&file.id).and_then(|p| p.threads.remove(&t.tid)) {
                if let Some(Some((proc, tid))) = th.serving.last().copied() {
                    st.queue(proc, Some(tid), Work::DeadReply);
                }
                for work in th.todo {
                    st.drop_work(file.id, work);
                }
            }
            Ok(0)
        }
        BINDER_WRITE_READ => write_read(p, t, file, arg, nonblocking),
        _ => Err(EINVAL),
    }
}

fn write_read(p: &Process, t: &mut Task, file: &Arc<BinderFile>, arg: u64, nonblocking: bool) -> SysResult {
    let bwr: [u8; 48] = p.mem.read_array(arg)?;
    let (write_size, mut write_consumed, write_buffer) = (u64_at(&bwr, 0), u64_at(&bwr, 8), u64_at(&bwr, 16));
    let (read_size, mut read_consumed, read_buffer) = (u64_at(&bwr, 24), u64_at(&bwr, 32), u64_at(&bwr, 40));
    if trace_level() == 2 {
        let cmds = if write_size > write_consumed { p.mem.read(write_buffer + write_consumed, (write_size - write_consumed).min(64) as usize).unwrap_or_default() } else { Vec::new() };
        let first = cmds.get(0..4).map(|c| u32_at(c, 0));
        eprintln!("[binder] {}:{} write {} (first cmd {first:x?}) read {}", p.sys.pid, t.tid, write_size - write_consumed, read_size - read_consumed);
    }
    {
        let mut st = file.broker.state.lock();
        let proc = st.proc_mut(file.id);
        proc.pid = p.sys.pid;
        proc.threads.entry(t.tid).or_default();
    }
    let mut result = Ok(0);
    if write_size > write_consumed {
        // The commands into this thread's scratch buffer when they fit (a transaction's header is
        // ~70 bytes; its data is read by the command).
        let at = crate::guest::with_scratch((write_size - write_consumed).min(1 << 20) as usize, |cmds| -> Result<usize, Errno> {
            p.mem.read_into(write_buffer + write_consumed, cmds)?;
            Ok(run_commands(p, t, file, cmds, &mut result))
        })?;
        write_consumed += at as u64;
    }
    if result.is_ok() && read_size > read_consumed {
        match read(p, t, file, read_buffer + read_consumed, read_size - read_consumed, read_consumed == 0, nonblocking) {
            Ok(n) => read_consumed += n,
            Err(e) => result = Err(e),
        }
    }
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&write_consumed.to_le_bytes());
    p.mem.write(arg + 8, &out[0..8])?;
    out[8..16].copy_from_slice(&read_consumed.to_le_bytes());
    p.mem.write(arg + 32, &out[8..16])?;
    result
}

/// The write buffer's commands, in order, until one fails: the bytes consumed. A failed command's
/// error goes to `result`.
fn run_commands(p: &Process, t: &mut Task, file: &Arc<BinderFile>, cmds: &[u8], result: &mut SysResult) -> usize {
    let mut at = 0usize;
    while at + 4 <= cmds.len() {
        match command(p, t, file, cmds, at) {
            Ok(len) => at += len,
            // A transaction that failed is answered as the kernel answers it: consumed, the
            // write stops there, and the error is the thread's to read (BR_FAILED_REPLY) --
            // not the ioctl's. An ioctl error left libbinder's out-buffer unconsumed and every
            // later call of that thread failed with it (an app's `unbindService` threw
            // IllegalArgumentException and killed its main thread).
            Err(Failed::Transaction(len)) => {
                at += len;
                break;
            }
            Err(Failed::Command(e)) => {
                *result = Err(e);
                break;
            }
        }
    }
    at
}

/// Why a write stopped at a command.
enum Failed {
    /// A transaction or reply that could not be made (`len` bytes, consumed): its failure was
    /// queued for the threads that must hear of it.
    Transaction(usize),
    /// A command the driver refuses: the ioctl's error.
    Command(Errno),
}

impl From<Errno> for Failed {
    fn from(e: Errno) -> Self {
        Self::Command(e)
    }
}

/// One `BC_*` command at `at` in `cmds`: its length, or why the write stops.
fn command(p: &Process, t: &mut Task, file: &Arc<BinderFile>, cmds: &[u8], at: usize) -> Result<usize, Failed> {
    let code = u32_at(cmds, at);
    let size = ((code >> 16) & 0x3fff) as usize;
    let arg = cmds.get(at + 4..at + 4 + size).ok_or(EFAULT)?;
    match code {
        BC_TRANSACTION | BC_REPLY | BC_TRANSACTION_SG | BC_REPLY_SG => {
            let reply = matches!(code, BC_REPLY | BC_REPLY_SG);
            // A reply takes the transaction it answers off the thread first, whatever becomes of
            // the reply (the kernel's `thread->transaction_stack = in_reply_to->to_parent`): the
            // caller it answers, if any is still waiting.
            let caller = if reply {
                let mut st = file.broker.state.lock();
                st.proc_mut(file.id).threads.entry(t.tid).or_default().serving.pop().flatten()
            } else {
                None
            };
            if let Err(e) = transaction(p, t, file, arg, reply, caller) {
                if trace_level() > 0 || crate::remote::is_remote() || p.trace {
                    eprintln!("[binder] {}:{} {} failed: {e:?}", p.sys.pid, t.tid, if reply { "reply" } else { "transaction" });
                }
                let mut st = file.broker.state.lock();
                if reply {
                    // binder_transaction's error path for a reply: the replier completes, and the
                    // one waiting for it hears BR_FAILED_REPLY.
                    st.queue(file.id, Some(t.tid), Work::Complete);
                    if let Some((proc, tid)) = caller {
                        st.queue(proc, Some(tid), Work::FailedReply);
                    }
                } else {
                    st.queue(file.id, Some(t.tid), Work::ReturnError(BR_FAILED_REPLY));
                }
                return Err(Failed::Transaction(4 + size));
            }
        }
        BC_FREE_BUFFER => {
            let ptr = u64_at(arg, 0);
            // As the kernel's `binder_free_buf`: a oneway's node hands its next oneway to the
            // process, and the nodes the transaction held are let go. The buffer's record goes
            // before its memory: freed first, the memory could be given to a transaction another
            // thread is delivered, whose record then replaced this one -- the oneway's node never
            // heard its call was done, and every later oneway to it waited (SurfaceFlinger's
            // composer callback: 100,000 vsyncs queued, the boot stalled).
            let mut st = file.broker.state.lock();
            let record = st.buffers.remove(&(file.id, ptr));
            file.area.lock().free(ptr);
            if let Some(b) = record {
                if let Some(node) = b.async_node {
                    st.async_done(node, file.id);
                }
                st.unpin(b.pinned);
            }
        }
        // A live process's handle references are not counted: its handles last as long as it
        // does, and are released with it (`BinderFile::release`). Counting them needs the whole
        // write to go on past a failed command, as the kernel's does (a dropped BC_ACQUIRE with
        // its BC_RELEASE counted deleted handles still in use).
        BC_INCREFS | BC_ACQUIRE | BC_RELEASE | BC_DECREFS => {}
        BC_ACQUIRE_DONE => file.broker.state.lock().acquire_done(file.id, u64_at(arg, 0)),
        BC_INCREFS_DONE | BC_DEAD_BINDER_DONE => {}
        BC_REGISTER_LOOPER | BC_ENTER_LOOPER => {
            let mut st = file.broker.state.lock();
            let proc = st.proc_mut(file.id);
            if code == BC_REGISTER_LOOPER {
                proc.spawn_requested = proc.spawn_requested.saturating_sub(1);
            }
            proc.threads.entry(t.tid).or_default().looper = true;
        }
        BC_EXIT_LOOPER => {
            let mut st = file.broker.state.lock();
            st.proc_mut(file.id).threads.entry(t.tid).or_default().exited = true;
        }
        BC_REQUEST_DEATH_NOTIFICATION | BC_CLEAR_DEATH_NOTIFICATION => {
            let (handle, cookie) = (u32_at(arg, 0), u64_at(arg, 4));
            let mut st = file.broker.state.lock();
            // The kernel's: the answer goes to this thread when it is a looper, else to the
            // process (a thread that only writes, as `unlinkToDeath`'s `flushCommands`, would
            // never read it).
            let looper = st.proc_mut(file.id).threads.get(&t.tid).is_some_and(|th| th.looper);
            let to = looper.then_some(t.tid);
            if let Some(node) = st.node_for_handle(file.id, handle) {
                if code == BC_REQUEST_DEATH_NOTIFICATION {
                    let dead = st.nodes.get(&node).is_none_or(|n| n.dead);
                    if dead {
                        st.queue(file.id, to, Work::DeadBinder { cookie });
                    } else if let Some(n) = st.nodes.get_mut(&node) {
                        n.watchers.push((file.id, cookie));
                    }
                } else {
                    if let Some(n) = st.nodes.get_mut(&node) {
                        n.watchers.retain(|w| *w != (file.id, cookie));
                    }
                    st.queue(file.id, to, Work::ClearDeathDone { cookie });
                }
            }
        }
        _ => {
            p.refusals.record(format!("binder command {code:#x}"), t.pc, t.lr);
            return Err(Failed::Command(EINVAL));
        }
    }
    Ok(4 + size)
}

/// `BC_TRANSACTION`/`BC_REPLY`: read the sender's buffer, translate its objects for the receiver,
/// and queue it. A reply goes to `caller`, the waiting thread of the transaction it answers.
fn transaction(p: &Process, t: &mut Task, file: &Arc<BinderFile>, tr: &[u8], reply: bool, caller: Option<(ProcId, i32)>) -> Result<(), Errno> {
    let handle = u32_at(tr, 0);
    let (code, flags) = (u32_at(tr, 16), u32_at(tr, 20));
    let (data_size, offsets_size) = (u64_at(tr, 32), u64_at(tr, 40));
    let (buffer, offsets_ptr) = (u64_at(tr, 48), u64_at(tr, 56));
    if data_size > 1 << 20 || offsets_size > 1 << 18 {
        return Err(EINVAL);
    }
    let mut data = p.mem.read(buffer, data_size as usize)?;
    let offsets: Vec<u64> = p.mem.read(offsets_ptr, offsets_size as usize)?.chunks_exact(8).map(|c| u64_at(c, 0)).collect();
    let oneway = !reply && flags & TF_ONE_WAY != 0;
    let mut st = file.broker.state.lock();

    // Where it goes.
    let (target_proc, target_tid, target_ptr, target_cookie, secctx, from, target_node) = if reply {
        let Some((proc, tid)) = caller else {
            // The one who asked is gone, or this was not a sync transaction: nothing to answer.
            st.queue(file.id, Some(t.tid), Work::Complete);
            return Ok(());
        };
        (proc, Some(tid), 0, 0, false, None, None)
    } else {
        // The kernel's answers: no context manager is BR_DEAD_REPLY, a handle the process does
        // not have BR_FAILED_REPLY, a dead node BR_DEAD_REPLY.
        let Some(node) = st.node_for_handle(file.id, handle) else {
            st.queue(file.id, Some(t.tid), Work::ReturnError(if handle == 0 { BR_DEAD_REPLY } else { BR_FAILED_REPLY }));
            return Ok(());
        };
        let n = st.nodes.get(&node).expect("a node");
        if n.dead {
            st.queue(file.id, Some(t.tid), Work::ReturnError(BR_DEAD_REPLY));
            return Ok(());
        }
        let (owner, ptr, cookie, sec) = (n.owner, n.ptr, n.cookie, n.txn_security_ctx);
        // A thread already serving a transaction from the target process takes the nested one.
        let nested = if oneway {
            None
        } else {
            st.procs
                .get(&file.id)
                .and_then(|pr| pr.threads.get(&t.tid))
                .and_then(|th| th.serving.iter().rev().flatten().find(|(pid, _)| *pid == owner).map(|(_, tid)| *tid))
        };
        (owner, nested, ptr, cookie, sec, (!oneway).then_some((file.id, t.tid)), Some(node))
    };
    // A dead receiver, before anything is translated for it (a handle made for a dead process
    // would hold its node for no one). The kernel's: a transaction to a dead process fails as
    // the sender's BR_DEAD_REPLY; a reply to a caller gone completes for the replier.
    if target_proc != HOST && st.procs.get(&target_proc).is_none_or(|pr| pr.dead) {
        st.queue(file.id, Some(t.tid), if reply { Work::Complete } else { Work::ReturnError(BR_DEAD_REPLY) });
        return Ok(());
    }

    // What the host answers itself (`Broker::intercept`): the reply goes back at once, and the
    // target never hears of the call (nothing of it is translated or held).
    if let (Some(node), false, false) = (target_node, reply, oneway) {
        let answer = st.intercepts.iter().filter(|(n, c, _)| (*n, *c) == (node, code)).find_map(|(_, _, i)| i(&data));
        if let Some(answer) = answer {
            let r = st.reply_from_host(file.id, HostReply::bytes(answer))?;
            st.proc_mut(file.id).threads.entry(t.tid).or_default().awaiting += 1;
            st.queue(file.id, Some(t.tid), Work::Complete);
            st.queue(file.id, Some(t.tid), Work::Txn(Box::new(r)));
            return Ok(());
        }
    }

    // What the host listens to (`Broker::tap`), before the objects are translated.
    if let Some(node) = target_node {
        for (_, _, tap) in st.taps.iter().filter(|(n, c, _)| (*n, *c) == (node, code)) {
            tap(&data);
        }
    }

    // Translate the objects for the receiver; the nodes it names are held while it is in flight,
    // its target's too (the kernel's `binder_inc_node` for the transaction's buffer).
    let mut pinned = Vec::new();
    let translated = translate_objects(p, t, &mut st, (file.id, t.tid), target_proc, &mut data, &offsets, &mut pinned);
    let (fds, sg, fda) = match translated {
        Ok(objects) => objects,
        Err(e) => {
            st.unpin(pinned);
            return Err(e);
        }
    };
    if let Some(node) = target_node {
        st.pin(node, &mut pinned);
    }
    if target_proc == HOST {
        // The host's handles hold what it is sent for as long as it runs.
        st.unpin(std::mem::take(&mut pinned));
    }

    // OMNI_BINDER_TRACE=1: every transaction and reply, sender to receiver.
    static TRACE: OnceLock<bool> = OnceLock::new();
    if *TRACE.get_or_init(|| std::env::var("OMNI_BINDER_TRACE").is_ok_and(|v| v == "1" || v == "2")) {
        let to = if target_proc == HOST { "host".to_string() } else { format!("{}", st.procs.get(&target_proc).map_or(0, |pr| pr.pid)) };
        eprintln!(
            "[binder] {} {}:{} -> {to} code {code:#x}{} ({} bytes)",
            if reply { "reply" } else { "call" },
            p.sys.pid,
            t.tid,
            if oneway { " oneway" } else { "" },
            data.len()
        );
    }
    let txn = Txn {
        reply,
        oneway,
        from,
        target_ptr,
        target_cookie,
        secctx,
        code,
        flags,
        sender_pid: p.sys.pid,
        sender_euid: p.sys.uid(),
        data,
        offsets,
        fds,
        sg,
        fda,
        async_node: None,
        pinned,
    };
    if !reply && target_proc == HOST {
        // A host service answers on a host thread of its own, while the sender waits in `read`
        // as it would for any process: a handler may take time, and may call back into the
        // sender, which the waiting thread then serves. The sender hears its transaction went,
        // then the reply, as the kernel orders them (the handler's reply waits for the lock).
        let handler = st.host_services.get(&target_ptr).cloned();
        let handles = txn
            .offsets
            .iter()
            .filter_map(|&off| {
                let obj = txn.data.get(off as usize..off as usize + 16)?;
                matches!(u32_at(obj, 0), TYPE_HANDLE | TYPE_WEAK_HANDLE).then(|| u32_at(obj, 8))
            })
            .collect();
        let call = HostCall {
            code,
            fds: txn.fds.into_iter().map(|(_, f)| f).collect(),
            handles,
            offsets: txn.offsets,
            data: txn.data,
            sender_pid: txn.sender_pid,
            sender_euid: txn.sender_euid,
        };
        let (broker, caller) = (Arc::clone(&file.broker), (file.id, t.tid));
        let job = move || {
            HOST_SERVING.with(|s| s.set(Some(caller)));
            let reply = handler.map_or_else(HostReply::default, |h| h(call));
            if !oneway {
                let mut st = broker.state.lock();
                if st.procs.get(&caller.0).is_some_and(|pr| !pr.dead) {
                    // A reply the host built wrong fails the call, as a malformed reply would.
                    let work = st.reply_from_host(caller.0, reply).map_or(Work::FailedReply, |r| Work::Txn(Box::new(r)));
                    st.queue(caller.0, Some(caller.1), work);
                }
            }
        };
        // The handler's thread is started while the broker's lock is held, as before: its reply
        // waits for the lock, so the sender hears BR_TRANSACTION_COMPLETE first.
        if host_pool_on() {
            host_pool_run(Box::new(job))?;
        } else {
            std::thread::Builder::new().name("omni-binder-host".into()).spawn(job).map_err(|_| ENOMEM)?;
        }
        if !oneway {
            st.proc_mut(file.id).threads.entry(t.tid).or_default().awaiting += 1;
        }
        st.queue(file.id, Some(t.tid), Work::Complete);
        return Ok(());
    }
    // A sync transaction: the sender now waits for its reply (the kernel's transaction_stack).
    if !reply && !oneway {
        st.proc_mut(file.id).threads.entry(t.tid).or_default().awaiting += 1;
    }
    match (oneway, target_node) {
        (true, Some(node)) => st.queue_oneway(target_proc, node, txn),
        _ => st.queue(target_proc, target_tid, Work::Txn(Box::new(txn))),
    }
    st.queue(file.id, Some(t.tid), Work::Complete);
    Ok(())
}

/// `OMNI_BINDER_TRACE`, read once: 0 unset, 2 for `2`, 1 for any other value. Read on every
/// `BINDER_WRITE_READ`, thousands of times a second, where an environment lookup (the environment's
/// lock and a `String`) is a cost of its own.
fn trace_level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("OMNI_BINDER_TRACE") {
        Ok(v) if v == "2" => 2,
        Ok(_) => 1,
        Err(_) => 0,
    })
}

/// The descriptors, scatter-gather buffers and fd arrays a transaction carries.
type Objects = (Vec<(usize, Arc<OpenFile>)>, Vec<SgBuffer>, Vec<(usize, usize, Vec<Arc<OpenFile>>)>);

/// Translate the objects at `offsets` of `data` that `sender` sends to `target`: binders and
/// handles rewritten in place (the nodes they name held in `pins`), descriptors, scatter-gather
/// buffers and fd arrays gathered.
#[allow(clippy::too_many_arguments)]
fn translate_objects(p: &Process, t: &Task, st: &mut State, sender: (ProcId, i32), target: ProcId, data: &mut [u8], offsets: &[u64], pins: &mut Vec<NodeId>) -> Result<Objects, Errno> {
    let mut fds = Vec::new();
    let mut sg: Vec<SgBuffer> = Vec::new();
    let mut fda = Vec::new();
    // Which `sg` entry each object index is (buffer objects only).
    let mut sg_of_object: HashMap<usize, usize> = HashMap::new();
    for (index, &off) in offsets.iter().enumerate() {
        let off = off as usize;
        let kind = u32_at(data.get(off..off + 4).ok_or(EINVAL)?, 0);
        if kind == TYPE_PTR {
            let obj = data.get(off..off + 40).ok_or(EINVAL)?.to_vec();
            let (flags, buffer, length) = (u32_at(&obj, 4), u64_at(&obj, 8), u64_at(&obj, 16));
            if length > 1 << 20 {
                return Err(EINVAL);
            }
            let bytes = p.mem.read(buffer, length as usize)?;
            let parent = if flags & BUFFER_FLAG_HAS_PARENT != 0 {
                let parent_index = u64_at(&obj, 24) as usize;
                let parent_sg = *sg_of_object.get(&parent_index).ok_or(EINVAL)?;
                Some((parent_sg, u64_at(&obj, 32) as usize))
            } else {
                None
            };
            sg_of_object.insert(index, sg.len());
            sg.push(SgBuffer { obj_off: off, bytes, parent });
            continue;
        }
        if kind == TYPE_FDA {
            let obj = data.get(off..off + 32).ok_or(EINVAL)?.to_vec();
            let (num, parent_index, parent_offset) = (u64_at(&obj, 8) as usize, u64_at(&obj, 16) as usize, u64_at(&obj, 24) as usize);
            let parent_sg = *sg_of_object.get(&parent_index).ok_or(EINVAL)?;
            let parent_bytes = &sg[parent_sg].bytes;
            let mut files = Vec::with_capacity(num);
            for i in 0..num {
                let at = parent_offset + i * 4;
                let fd = u32_at(parent_bytes.get(at..at + 4).ok_or(EINVAL)?, 0) as i32;
                files.push(p.fds.get(fd)?);
            }
            fda.push((parent_sg, parent_offset, files));
            continue;
        }
        let obj = data.get(off..off + 24).ok_or(EINVAL)?.to_vec();
        match u32_at(&obj, 0) {
            TYPE_BINDER | TYPE_WEAK_BINDER | TYPE_HANDLE | TYPE_WEAK_HANDLE => {
                if let Some(to) = st.translate_ref(sender, target, &obj, pins)? {
                    rewrite_ref(data, off, to);
                }
            }
            TYPE_FD => {
                let fd = u64_at(&obj, 8) as u32 as i32;
                let open = p.fds.get(fd)?;
                fds.push((off, open));
            }
            other => {
                p.refusals.record(format!("binder object type {other:#x}"), t.pc, t.lr);
                return Err(EINVAL);
            }
        }
    }
    Ok((fds, sg, fda))
}

/// Fill the read buffer with work for this thread, waiting for some when there is none. However
/// the read ends, the thread is no longer idle (`ThreadState::idle`): a stale mark would hold back
/// a looper the process needs ([`spawn`]).
fn read(p: &Process, t: &mut Task, file: &Arc<BinderFile>, at: u64, size: u64, first: bool, nonblocking: bool) -> Result<u64, Errno> {
    let tid = t.tid;
    let done = read_waiting(p, t, file, at, size, first, nonblocking);
    if let Some(th) = file.broker.state.lock().proc_mut(file.id).threads.get_mut(&tid) {
        th.idle = false;
    }
    done
}

#[allow(clippy::too_many_arguments)]
fn read_waiting(p: &Process, t: &mut Task, file: &Arc<BinderFile>, at: u64, size: u64, first: bool, nonblocking: bool) -> Result<u64, Errno> {
    let mut out: Vec<u8> = Vec::new();
    if first {
        out.extend_from_slice(&BR_NOOP.to_le_bytes());
    }
    let flushed = file.flushes.load(std::sync::atomic::Ordering::SeqCst);
    let mut watching: Option<crate::poll::Watch> = None;
    loop {
        // Woken by work for this process (`queue`), or by a descriptor's close (`flush`, told to
        // everyone).
        let work = {
            let mut st = file.broker.state.lock();
            let proc = st.proc_mut(file.id);
            let thread = proc.threads.entry(t.tid).or_default();
            let own = thread.todo.pop_front();
            // The kernel's `binder_available_for_proc_work_ilocked`: only a looper
            // (BC_ENTER_LOOPER/BC_REGISTER_LOOPER) with no transaction on its stack -- neither
            // waiting for a reply nor serving a call -- and nothing of its own takes the
            // process's work. A thread waiting for a reply is never handed a oneway.
            let may_take_proc = thread.looper && thread.awaiting == 0 && thread.serving.is_empty();
            let looper = thread.looper;
            let w = match own {
                Some(w) => Some(w),
                None if may_take_proc => {
                    let w = proc.todo.pop_front();
                    // Ask for another looper when this one takes the last idle slot.
                    if w.is_some() && looper && spawn::wanted(&p.comm.lock(), proc, t.tid) {
                        proc.spawn_requested += 1;
                        out.extend_from_slice(&BR_SPAWN_LOOPER.to_le_bytes());
                    }
                    w
                }
                None => None,
            };
            // About to wait for the process's work: idle until the read ends (the watch above is
            // already set, so work queued from here on still wakes it).
            if let Some(th) = proc.threads.get_mut(&t.tid) {
                th.idle = w.is_none() && may_take_proc;
            }
            w
        };
        match work {
            Some(w) => {
                let handed = match deliver(p, t, file, w, &mut out) {
                    Delivery::Put => None,
                    Delivery::Txn(h) => Some(h),
                    // It failed on its way in, its sender told: on to the next.
                    Delivery::Nothing => continue,
                };
                // Return what is there; more comes on the next read.
                let n = (out.len() as u64).min(size);
                if let Err(e) = p.mem.write(at, &out[..n as usize]) {
                    // The kernel's copy_to_user failure: the transaction fails towards its sender
                    // and the read is EFAULT.
                    if let Some(h) = handed {
                        undeliver(p, file, t.tid, h);
                    }
                    return Err(e);
                }
                return Ok(n);
            }
            None if nonblocking || (!first && !out.is_empty()) => {
                if out.is_empty() {
                    return Err(EAGAIN);
                }
                p.mem.write(at, &out)?;
                return Ok(out.len() as u64);
            }
            // A descriptor was closed meanwhile: back to the caller with what there is.
            None if file.flushes.load(std::sync::atomic::Ordering::SeqCst) != flushed => {
                p.mem.write(at, &out)?;
                return Ok(out.len() as u64);
            }
            // Registered only once a look found nothing, then looked again before sleeping.
            None => match watching.take() {
                None => watching = Some(crate::poll::watch(Some(vec![file.key()]))),
                Some(w) => w.wait(None, t)?,
            },
        }
    }
}

/// What [`deliver`] did with a work item.
enum Delivery {
    /// Its return is in `out`.
    Put,
    /// A transaction or reply, its return in `out`: undone ([`undeliver`]) if the read cannot
    /// reach the thread.
    Txn(Handed),
    /// Nothing reached this thread: the transaction failed on its way in, and whoever waited on it
    /// heard so.
    Nothing,
}

/// A transaction handed to this thread: what taking it back undoes.
struct Handed {
    buf: u64,
    fds: Vec<i32>,
    reply: bool,
    oneway: bool,
    from: Option<(ProcId, i32)>,
}

fn deliver(p: &Process, t: &mut Task, file: &Arc<BinderFile>, work: Work, out: &mut Vec<u8>) -> Delivery {
    let mut put = |cmd: u32, words: &[u64]| {
        out.extend_from_slice(&cmd.to_le_bytes());
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
    };
    match work {
        Work::Complete => put(BR_TRANSACTION_COMPLETE, &[]),
        Work::ReturnError(cmd) => put(cmd, &[]),
        Work::DeadReply => {
            end_wait(file, t.tid);
            put(BR_DEAD_REPLY, &[]);
        }
        Work::FailedReply => {
            end_wait(file, t.tid);
            put(BR_FAILED_REPLY, &[]);
        }
        Work::Increfs { ptr, cookie } => put(BR_INCREFS, &[ptr, cookie]),
        Work::Acquire { ptr, cookie } => put(BR_ACQUIRE, &[ptr, cookie]),
        Work::Release { ptr, cookie } => put(BR_RELEASE, &[ptr, cookie]),
        Work::Decrefs { ptr, cookie } => put(BR_DECREFS, &[ptr, cookie]),
        Work::DeadBinder { cookie } => put(BR_DEAD_BINDER, &[cookie]),
        Work::ClearDeathDone { cookie } => put(BR_CLEAR_DEATH_NOTIFICATION_DONE, &[cookie]),
        Work::Txn(txn) => {
            let mut txn = *txn;
            let mut fds = Vec::new();
            let (buf, cmd, tr) = match place(p, file, &mut txn, &mut fds) {
                Ok(placed) => placed,
                Err(e) => return fail_undelivered(file, t.tid, txn, e, out),
            };
            let mut st = file.broker.state.lock();
            // Freeing the buffer hands a oneway's node's next oneway out, and lets go the nodes
            // the transaction held.
            if txn.async_node.is_some() || !txn.pinned.is_empty() {
                let pinned = std::mem::take(&mut txn.pinned);
                st.buffers.insert((file.id, buf), Buffer { async_node: txn.async_node, pinned, tid: t.tid, code: txn.code });
            }
            let th = st.proc_mut(file.id).threads.entry(t.tid).or_default();
            if txn.reply {
                th.awaiting = th.awaiting.saturating_sub(1);
            } else if !txn.oneway {
                // A sync transaction: this thread now serves it, and its reply goes to `from`.
                th.serving.push(txn.from);
            }
            drop(st);
            out.extend_from_slice(&cmd.to_le_bytes());
            out.extend_from_slice(&tr);
            return Delivery::Txn(Handed { buf, fds, reply: txn.reply, oneway: txn.oneway, from: txn.from });
        }
    }
    let _ = (BR_ERROR, BR_OK);
    Delivery::Put
}

/// Copy `txn` into this process's receive area, installing the descriptors it carries (their
/// numbers in `fds`): the buffer, and the return and its `binder_transaction_data`. On an error
/// nothing of it is left: the descriptors are closed and the buffer is freed.
fn place(p: &Process, file: &Arc<BinderFile>, txn: &mut Txn, fds: &mut Vec<i32>) -> Result<(u64, u32, Vec<u8>), Errno> {
    let data_len = (txn.data.len() as u64 + 7) & !7;
    let offsets_len = txn.offsets.len() as u64 * 8;
    let sg_len: u64 = txn.sg.iter().map(|b| (b.bytes.len() as u64 + 7) & !7).sum();
    let sec_len = if txn.secctx && !txn.reply { SECCTX.len() as u64 } else { 0 };
    let buf = file.area.lock().alloc(data_len + offsets_len + sg_len + sec_len).ok_or(ENOMEM)?;
    let filled = fill(p, txn, buf, (data_len, offsets_len, sg_len, sec_len), fds);
    if filled.is_err() {
        for fd in fds.drain(..) {
            // A stand-in's descriptors are its app's: those it cannot take back (the app closes
            // them with the transaction it never sees).
            let _ = p.fds.remove(fd);
        }
        file.area.lock().free(buf);
    }
    filled?;
    let mut tr = Vec::with_capacity(72);
    tr.extend_from_slice(&txn.target_ptr.to_le_bytes());
    tr.extend_from_slice(&txn.target_cookie.to_le_bytes());
    tr.extend_from_slice(&txn.code.to_le_bytes());
    tr.extend_from_slice(&txn.flags.to_le_bytes());
    tr.extend_from_slice(&txn.sender_pid.to_le_bytes());
    tr.extend_from_slice(&txn.sender_euid.to_le_bytes());
    tr.extend_from_slice(&(txn.data.len() as u64).to_le_bytes());
    tr.extend_from_slice(&offsets_len.to_le_bytes());
    tr.extend_from_slice(&buf.to_le_bytes());
    tr.extend_from_slice(&(buf + data_len).to_le_bytes());
    let cmd = if txn.reply {
        BR_REPLY
    } else if sec_len > 0 {
        tr.extend_from_slice(&(buf + data_len + offsets_len + sg_len).to_le_bytes());
        BR_TRANSACTION_SEC_CTX
    } else {
        BR_TRANSACTION
    };
    Ok((buf, cmd, tr))
}

/// Install `txn`'s descriptors (recording them in `fds`) and write it into `buf`, laid out by
/// `lens` (data, offsets, scatter-gather buffers, security context).
fn fill(p: &Process, txn: &mut Txn, buf: u64, (data_len, offsets_len, _sg_len, sec_len): (u64, u64, u64, u64), fds: &mut Vec<i32>) -> Result<(), Errno> {
    for (off, open) in &txn.fds {
        let fd = p.fds.insert(Arc::clone(open), true, 0)?;
        fds.push(fd);
        txn.data[*off + 8..*off + 12].copy_from_slice(&(fd as u32).to_le_bytes());
    }
    // Scatter-gather buffers go after the offsets: each object points at its copy, and a
    // child's address is written into its parent's copy where the sender had it.
    let mut sg_at = Vec::with_capacity(txn.sg.len());
    let mut cursor = buf + data_len + offsets_len;
    for b in &txn.sg {
        sg_at.push(cursor);
        cursor += (b.bytes.len() as u64 + 7) & !7;
    }
    for (i, b) in txn.sg.iter().enumerate() {
        txn.data[b.obj_off + 8..b.obj_off + 16].copy_from_slice(&sg_at[i].to_le_bytes());
    }
    let mut sg_bytes: Vec<Vec<u8>> = txn.sg.iter().map(|b| b.bytes.clone()).collect();
    for (i, b) in txn.sg.iter().enumerate() {
        if let Some((parent, at)) = b.parent {
            if let Some(slot) = sg_bytes[parent].get_mut(at..at + 8) {
                slot.copy_from_slice(&sg_at[i].to_le_bytes());
            }
        }
    }
    for (parent, at, files) in &txn.fda {
        for (i, open) in files.iter().enumerate() {
            let fd = p.fds.insert(Arc::clone(open), true, 0)?;
            fds.push(fd);
            if let Some(slot) = sg_bytes[*parent].get_mut(at + i * 4..at + i * 4 + 4) {
                slot.copy_from_slice(&(fd as u32).to_le_bytes());
            }
        }
    }
    let mut bytes = txn.data.clone();
    bytes.resize(data_len as usize, 0);
    for o in &txn.offsets {
        bytes.extend_from_slice(&o.to_le_bytes());
    }
    for b in &sg_bytes {
        let start = bytes.len();
        bytes.extend_from_slice(b);
        bytes.resize(start + ((b.len() + 7) & !7), 0);
    }
    bytes.extend_from_slice(if sec_len > 0 { SECCTX } else { &[] });
    p.mem.write(buf, &bytes)
}

/// `txn` could not be placed in this process (`error`): nothing of it reached this thread. The
/// kernel translates descriptors when the transaction is sent, so it fails such a transaction
/// towards its sender, never the receiver; here they go in on delivery, so the failure goes
/// where the kernel's would -- a sync transaction's sender reads BR_FAILED_REPLY, a oneway is
/// dropped (its sender was told it went) and its node's next goes, and a reply this thread
/// waited for is its BR_FAILED_REPLY.
fn fail_undelivered(file: &Arc<BinderFile>, tid: i32, txn: Txn, error: Errno, out: &mut Vec<u8>) -> Delivery {
    let mut st = file.broker.state.lock();
    let pid = st.procs.get(&file.id).map_or(0, |pr| pr.pid);
    eprintln!(
        "[binder] {pid}:{tid} {} code {:#x} not delivered: errno {}",
        if txn.reply { "reply" } else if txn.oneway { "oneway" } else { "transaction" },
        txn.code,
        error.0
    );
    st.unpin(txn.pinned);
    if txn.reply {
        let th = st.proc_mut(file.id).threads.entry(tid).or_default();
        th.awaiting = th.awaiting.saturating_sub(1);
        out.extend_from_slice(&BR_FAILED_REPLY.to_le_bytes());
        return Delivery::Put;
    }
    if let Some(node) = txn.async_node {
        st.async_done(node, file.id);
    }
    if let (false, Some((proc, from))) = (txn.oneway, txn.from) {
        st.queue(proc, Some(from), Work::FailedReply);
    }
    Delivery::Nothing
}

/// A transaction handed to this thread whose read could not reach it (the kernel's
/// copy_to_user failure): its descriptors are closed, its buffer freed, and it fails towards its
/// sender as [`fail_undelivered`] does -- a reply's wait has ended, and the read's error answers it.
fn undeliver(p: &Process, file: &Arc<BinderFile>, tid: i32, h: Handed) {
    for fd in h.fds {
        let _ = p.fds.remove(fd);
    }
    let mut st = file.broker.state.lock();
    // The record before the memory, as BC_FREE_BUFFER.
    let record = st.buffers.remove(&(file.id, h.buf));
    file.area.lock().free(h.buf);
    if let Some(b) = record {
        if let Some(node) = b.async_node {
            st.async_done(node, file.id);
        }
        st.unpin(b.pinned);
    }
    if !h.reply && !h.oneway {
        let th = st.proc_mut(file.id).threads.entry(tid).or_default();
        if th.serving.last().copied() == Some(h.from) {
            th.serving.pop();
        }
        if let Some((proc, from)) = h.from {
            st.queue(proc, Some(from), Work::FailedReply);
        }
    }
}

/// The reply (or its failure) this thread waited for has come.
fn end_wait(file: &Arc<BinderFile>, tid: i32) {
    let mut st = file.broker.state.lock();
    let th = st.proc_mut(file.id).threads.entry(tid).or_default();
    th.awaiting = th.awaiting.saturating_sub(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oneway(node: NodeId, code: u32) -> Work {
        Work::Txn(Box::new(Txn {
            reply: false,
            oneway: true,
            from: None,
            target_ptr: 0x10,
            target_cookie: 0,
            secctx: false,
            code,
            flags: TF_ONE_WAY,
            sender_pid: 0,
            sender_euid: 0,
            data: Vec::new(),
            offsets: Vec::new(),
            fds: Vec::new(),
            sg: Vec::new(),
            fda: Vec::new(),
            async_node: Some(node),
            pinned: Vec::new(),
        }))
    }

    /// The stall report names the next call's code and where the one out is.
    #[test]
    fn a_stalled_nodes_report_says_where_its_oneway_is() {
        let mut st = State::default();
        st.procs.insert(1, ProcState { pid: 42, ..ProcState::default() });
        let node = st.local_node(1, 0x10, 0x20, 0);
        st.nodes.get_mut(&node).unwrap().async_todo.push_back(oneway(node, 0x44));
        st.proc_mut(1).threads.insert(7, ThreadState { looper: true, awaiting: 1, ..ThreadState::default() });

        st.buffers.insert((1, 0x9000), Buffer { async_node: Some(node), pinned: Vec::new(), tid: 7, code: 0x33 });
        let delivered = st.async_stall(node);
        assert!(delivered.starts_with("1 oneway calls wait on node 1 (0x10) of pid 42: next code 0x44"), "{delivered}");
        assert!(delivered.contains("code 0x33 delivered to thread 7 in buffer 0x9000, not freed (thread: looper true, awaiting 1"), "{delivered}");

        st.buffers.clear();
        st.proc_mut(1).threads.get_mut(&7).unwrap().todo.push_back(oneway(node, 0x33));
        let queued = st.async_stall(node);
        assert!(queued.contains("code 0x33 queued for thread 7 (looper true, awaiting 1, serving 0, 1 queued)"), "{queued}");

        st.proc_mut(1).threads.get_mut(&7).unwrap().todo.clear();
        st.proc_mut(1).todo.push_back(oneway(node, 0x33));
        let for_proc = st.async_stall(node);
        assert!(for_proc.contains("code 0x33 queued for the process (0 of 1 threads free to take it)"), "{for_proc}");
    }

    /// The pool runs calls that wait on each other at once (no call waits for a thread), uses a
    /// thread again for the next call, and leaves nothing of a call's `HOST_SERVING` behind.
    #[test]
    fn host_pool_calls_run_at_once_on_kept_threads() {
        use std::sync::mpsc::channel;
        // Two calls, the first blocked until the second has run: a bounded pool of one deadlocks.
        let (go_tx, go_rx) = channel::<()>();
        let (done_tx, done_rx) = channel::<&str>();
        let first_done = done_tx.clone();
        host_pool_run(Box::new(move || {
            go_rx.recv_timeout(Duration::from_secs(10)).expect("the second call ran meanwhile");
            first_done.send("first").unwrap();
        }))
        .unwrap();
        host_pool_run(Box::new(move || {
            go_tx.send(()).unwrap();
            done_tx.send("second").unwrap();
        }))
        .unwrap();
        let mut done = [done_rx.recv_timeout(Duration::from_secs(10)).unwrap(), done_rx.recv_timeout(Duration::from_secs(10)).unwrap()];
        done.sort_unstable();
        assert_eq!(done, ["first", "second"]);

        // One call after another: a kept thread, and the last call's caller not left on it.
        let (tx, rx) = channel();
        let mut threads = Vec::new();
        for _ in 0..20 {
            let tx = tx.clone();
            host_pool_run(Box::new(move || {
                let leftover = HOST_SERVING.with(std::cell::Cell::get);
                HOST_SERVING.with(|s| s.set(Some((1, 2))));
                tx.send((std::thread::current().id(), leftover)).unwrap();
            }))
            .unwrap();
            let (thread, leftover) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(leftover, None);
            threads.push(thread);
            // Let the thread go back on the idle list before the next call.
            std::thread::sleep(Duration::from_millis(2));
        }
        threads.sort_by_key(|t| format!("{t:?}"));
        threads.dedup();
        assert!(threads.len() <= 3, "{} threads for 20 calls one after another", threads.len());
    }

    /// **The measurement behind `HOST_POOL`**: a call's round trip (hand it to a thread, hear it
    /// ran) on a new thread each against a kept one, 2000 each. Timing, so ignored by default:
    /// `cargo test --release -p omni-linux --lib -- --ignored --nocapture host_pool_round_trip`.
    #[test]
    #[ignore = "timing; run by hand"]
    fn host_pool_round_trip() {
        use std::sync::mpsc::channel;
        const N: u32 = 2000;
        let (tx, rx) = channel::<()>();
        let mut spawned = Duration::ZERO;
        for _ in 0..N {
            let tx = tx.clone();
            let t = Instant::now();
            let h = std::thread::Builder::new().name("omni-binder-host".into()).spawn(move || tx.send(()).unwrap()).unwrap();
            rx.recv().unwrap();
            spawned += t.elapsed();
            h.join().unwrap();
        }
        let mut pooled = Duration::ZERO;
        for _ in 0..N {
            // A kept thread is back waiting before the next call, as between two frames' calls.
            while host_idle().lock().is_empty() && pooled > Duration::ZERO {
                std::hint::spin_loop();
            }
            let tx = tx.clone();
            let t = Instant::now();
            host_pool_run(Box::new(move || tx.send(()).unwrap())).unwrap();
            rx.recv().unwrap();
            pooled += t.elapsed();
        }
        eprintln!("[binder] a host call's round trip: new thread {:?}, kept thread {:?}", spawned / N, pooled / N);
    }
}

/// **When a process is asked for another binder looper** (`BR_SPAWN_LOOPER`), and how many it may
/// have.
///
/// The kernel's rule (`binder_thread_read`, at its end): a looper is told to spawn another only if
/// none was asked for and not yet registered (`requested_threads == 0`), **no other thread waits
/// idle for the process's work** (`list_empty(&proc->waiting_threads)`) and the pool is under the
/// process's `BINDER_SET_MAX_THREADS`. This driver had all but the second, so a process that took
/// work while other loopers sat idle was asked again and again, up to its maximum: libbinder's
/// default 15 (+1, the main looper) in each of the system host process's ~60 guest processes --
/// the bulk of its ~900 host threads (2026-10-09). A spawn is a request; libbinder copes with
/// fewer (a pool only grows when asked, and a call back into a thread waiting for a reply is
/// delivered to that thread, not to the pool).
///
/// - `OMNI_BINDER_SPAWN=kernel` (or the live lever `binder_spawn=kernel`): the kernel's rule,
///   idle loopers counted. `eager` (the default, unchanged) asks whenever a looper takes work and
///   the pool is under its maximum.
/// - `OMNI_BINDER_MAX_LOOPERS=<n>`: at most `n` loopers (the main one included) in a process of a
///   **system** host process (never an app's: `OMNI_LINUX_APP`), whatever maximum it set; the
///   processes named in `OMNI_BINDER_LOOPERS_KEEP` (default `system_server,surfaceflinger`) keep
///   theirs. Unset: no cap. A cap too small can starve a service whose every looper is blocked in
///   an outgoing call that needs a *new* incoming call to finish -- the kernel has the same limit
///   at `max_threads`; this only lowers it.
/// - `OMNI_BINDER_CENSUS=<seconds>`: every so often, per guest process of this host process, its
///   threads and binder loopers (`[binder]` lines).
pub mod spawn {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    /// The kernel's rule (see the module); off: the eager one this driver had.
    pub static KERNEL_RULE: AtomicBool = AtomicBool::new(false);

    /// Read the environment (once, at start).
    pub fn from_env() {
        if std::env::var("OMNI_BINDER_SPAWN").as_deref() == Ok("kernel") {
            KERNEL_RULE.store(true, Ordering::Relaxed);
            eprintln!("[lever] OMNI_BINDER_SPAWN: binder_spawn=kernel");
        }
        if let Some(n) = cap() {
            eprintln!("[lever] OMNI_BINDER_MAX_LOOPERS: at most {n} binder loopers a process (but {})", keep().join(","));
        }
        census_start();
    }

    /// `OMNI_BINDER_MAX_LOOPERS`, in a system host process only.
    pub fn cap() -> Option<u32> {
        static CAP: OnceLock<Option<u32>> = OnceLock::new();
        *CAP.get_or_init(|| {
            if std::env::var_os("OMNI_LINUX_APP").is_some() {
                return None;
            }
            std::env::var("OMNI_BINDER_MAX_LOOPERS").ok().and_then(|v| v.trim().parse::<u32>().ok()).map(|n| n.max(1))
        })
    }

    fn keep() -> &'static [String] {
        static KEEP: OnceLock<Vec<String>> = OnceLock::new();
        KEEP.get_or_init(|| {
            std::env::var("OMNI_BINDER_LOOPERS_KEEP")
                .unwrap_or_else(|_| "system_server,surfaceflinger".into())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
    }

    /// The most loopers a process named `comm` may be asked for, with `BINDER_SET_MAX_THREADS`
    /// `max_threads`, under cap `cap`.
    fn limit(comm: &[u8], max_threads: u32, cap: Option<u32>) -> u32 {
        let own = max_threads.saturating_add(1);
        match cap {
            Some(n) if !keep().iter().any(|k| k.as_bytes() == comm) => own.min(n),
            _ => own,
        }
    }

    /// Whether thread `tid` of `proc` (named `comm`), taking its process's work, asks for another
    /// looper.
    pub(super) fn wanted(comm: &[u8], proc: &super::ProcState, tid: i32) -> bool {
        decide(comm, proc, tid, KERNEL_RULE.load(Ordering::Relaxed), cap())
    }

    fn decide(comm: &[u8], proc: &super::ProcState, tid: i32, kernel_rule: bool, cap: Option<u32>) -> bool {
        if proc.spawn_requested != 0 {
            return false;
        }
        let live = |th: &super::ThreadState| th.looper && !th.exited;
        if kernel_rule && proc.threads.iter().any(|(&other, th)| other != tid && live(th) && th.idle) {
            return false;
        }
        let loopers = proc.threads.values().filter(|th| live(th)).count() as u32;
        loopers < limit(comm, proc.max_threads, cap)
    }

    fn census_start() {
        let Some(every) = std::env::var("OMNI_BINDER_CENSUS").ok().and_then(|v| v.parse::<u64>().ok()) else { return };
        let _ = std::thread::Builder::new().name("omni-binder-census".into()).spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
            report();
        });
    }

    /// One `[binder]` line per guest process: its threads, and per binder device its loopers (how
    /// many idle), its maximum and an outstanding spawn request; then the totals.
    pub fn report() {
        let live = crate::process::all_live();
        let (mut threads, mut loopers) = (0usize, 0usize);
        for p in &live {
            let n = p.task_count();
            threads += n;
            let mut parts = Vec::new();
            for (name, ctx) in [("binder", super::Context::Binder), ("hwbinder", super::Context::HwBinder), ("vndbinder", super::Context::VndBinder)] {
                let broker = super::broker(ctx);
                let st = broker.state.lock();
                for proc in st.procs.values().filter(|pr| pr.pid == p.sys.pid && !pr.dead) {
                    let l = proc.threads.values().filter(|th| th.looper && !th.exited).count();
                    let idle = proc.threads.values().filter(|th| th.looper && !th.exited && th.idle).count();
                    loopers += l;
                    parts.push(format!(
                        "{name} {l} loopers ({idle} idle, max {}{})",
                        proc.max_threads,
                        if proc.spawn_requested > 0 { ", spawn asked" } else { "" }
                    ));
                }
            }
            eprintln!("[binder] {} pid {}: {n} threads; {}", String::from_utf8_lossy(&p.comm.lock()), p.sys.pid, parts.join("; "));
        }
        eprintln!("[binder] host pid {}: {} guest processes, {threads} threads, {loopers} binder loopers", std::process::id(), live.len());
    }

    #[cfg(test)]
    mod tests {
        use super::super::{ProcState, ThreadState};
        use super::decide;

        fn pool(max: u32, loopers: &[(i32, bool)]) -> ProcState {
            let mut p = ProcState { max_threads: max, ..ProcState::default() };
            for &(tid, idle) in loopers {
                p.threads.insert(tid, ThreadState { looper: true, idle, ..ThreadState::default() });
            }
            p
        }

        #[test]
        fn the_eager_rule_asks_until_the_maximum() {
            let p = pool(15, &[(1, false), (2, true), (3, true)]);
            assert!(decide(b"svc", &p, 1, false, None), "idle loopers do not stop the eager rule");
            let full = pool(2, &[(1, false), (2, false), (3, false)]);
            assert!(!decide(b"svc", &full, 1, false, None), "max_threads + 1 loopers: no more");
        }

        #[test]
        fn the_kernel_rule_asks_only_when_no_other_looper_is_idle() {
            let p = pool(15, &[(1, false), (2, true)]);
            assert!(!decide(b"svc", &p, 1, true, None), "thread 2 waits idle: no spawn");
            let busy = pool(15, &[(1, false), (2, false)]);
            assert!(decide(b"svc", &busy, 1, true, None), "every other looper busy: spawn");
            // The taker's own idle mark (stale or not) does not count.
            let own = pool(15, &[(1, true), (2, false)]);
            assert!(decide(b"svc", &own, 1, true, None));
            // One already asked for and not registered: none more.
            let mut asked = pool(15, &[(1, false)]);
            asked.spawn_requested = 1;
            assert!(!decide(b"svc", &asked, 1, true, None));
            // An exited looper is not a looper.
            let mut gone = pool(15, &[(1, false), (2, true)]);
            gone.threads.get_mut(&2).unwrap().exited = true;
            assert!(decide(b"svc", &gone, 1, true, None));
        }

        #[test]
        fn the_cap_lowers_the_maximum_but_not_for_the_kept_processes() {
            let p = pool(15, &[(1, false), (2, false)]);
            assert!(!decide(b"svc", &p, 1, false, Some(2)), "two loopers, cap 2");
            assert!(decide(b"svc", &p, 1, false, Some(3)));
            assert!(decide(b"system_server", &p, 1, false, Some(2)), "system_server keeps its own maximum");
            assert!(decide(b"surfaceflinger", &p, 1, false, Some(1)));
            // The cap never raises a process's own maximum.
            let small = pool(0, &[(1, false)]);
            assert!(!decide(b"svc", &small, 1, false, Some(8)));
        }
    }
}
