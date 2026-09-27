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
    DeadReply,
    FailedReply,
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
    looper: bool,
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
    /// The buffers oneway transactions were delivered in, by (receiver, address): freeing one
    /// hands its node's next oneway transaction out.
    async_buffers: HashMap<(ProcId, u64), NodeId>,
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
}

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
    /// (type, binder or handle, cookie), or `None` when it arrives unchanged.
    fn translate_ref(&mut self, sender: (ProcId, i32), target: ProcId, obj: &[u8]) -> Result<Option<(u32, u64, u64)>, Errno> {
        let (kind, flags) = (u32_at(obj, 0), u32_at(obj, 4));
        let (ptr, cookie) = (u64_at(obj, 8), u64_at(obj, 16));
        match kind {
            TYPE_BINDER | TYPE_WEAK_BINDER => {
                let node = self.local_node(sender.0, ptr, cookie, flags);
                if target == sender.0 {
                    return Ok(None);
                }
                let h = self.handle_for(target, node, Some(sender));
                Ok(Some((if kind == TYPE_BINDER { TYPE_HANDLE } else { TYPE_WEAK_HANDLE }, u64::from(h), 0)))
            }
            TYPE_HANDLE | TYPE_WEAK_HANDLE => {
                let node = self.node_for_handle(sender.0, ptr as u32).ok_or(EINVAL)?;
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
    /// the next waiting one goes to `owner` -- to thread `tid` when given, as the kernel hands it
    /// to the thread that freed the buffer.
    fn async_done(&mut self, node: NodeId, owner: ProcId, tid: Option<i32>) {
        let next = self.nodes.get_mut(&node).and_then(|n| {
            let w = n.async_todo.pop_front();
            n.has_async = w.is_some();
            w
        });
        if let Some(w) = next {
            self.queue(owner, tid, w);
        }
    }

    /// No live process refers to `node` any more: its owner lets it go (`BR_RELEASE`,
    /// `BR_DECREFS`), and the object's address may later name a new node.
    fn release_if_unreferenced(&mut self, node: NodeId) -> bool {
        if self.context_mgr == Some(node) || self.procs.values().any(|p| !p.dead && p.by_node.contains_key(&node)) {
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

    /// Queue work for a thread of `id`: `tid`'s own queue when given, the process's otherwise.
    fn queue(&mut self, id: ProcId, tid: Option<i32>, work: Work) {
        let proc = self.proc_mut(id);
        match tid {
            Some(t) => proc.threads.entry(t).or_default().todo.push_back(work),
            None => proc.todo.push_back(work),
        }
        crate::poll::notify();
    }
}

impl State {
    /// A host service's reply as `caller` receives it: its binder objects translated for the
    /// caller, its files attached. `EINVAL` for an object that is not where the reply says.
    fn reply_from_host(&mut self, caller: ProcId, reply: HostReply) -> Result<Txn, Errno> {
        let HostReply { mut data, fds, binders } = reply;
        let mut offsets: Vec<u64> = Vec::with_capacity(fds.len() + binders.len());
        for &off in &binders {
            let obj = data.get(off..off + 24).ok_or(EINVAL)?.to_vec();
            if let Some(to) = self.translate_ref((HOST, 0), caller, &obj)? {
                rewrite_ref(&mut data, off, to);
            }
            offsets.push(off as u64);
        }
        for (off, _) in &fds {
            if data.get(*off..off + 24).map(|o| u32_at(o, 0)) != Some(TYPE_FD) {
                return Err(EINVAL);
            }
            offsets.push(*off as u64);
        }
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
        })
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

    /// A sync transaction from the host to `handle` (in the host's handle table; 0 is the context
    /// manager): the reply's parcel. `data` may carry the host's own services as binder objects,
    /// at `offsets`.
    ///
    /// # Errors
    /// `EINVAL` for an unknown handle or an object that is not a binder or handle, `EPIPE` when the
    /// target is dead or dies before replying, `ETIMEDOUT` when no reply comes in time.
    pub fn host_transact(&self, handle: u32, code: u32, mut data: Vec<u8>, offsets: &[u64]) -> Result<Vec<u8>, Errno> {
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
        for &off in offsets {
            let off = off as usize;
            let obj = data.get(off..off + 24).ok_or(EINVAL)?.to_vec();
            if let Some(to) = st.translate_ref((HOST, tid), target, &obj)? {
                rewrite_ref(&mut data, off, to);
            }
        }
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
            sender_euid: HOST_EUID,
            data,
            offsets: offsets.to_vec(),
            fds: Vec::new(),
            sg: Vec::new(),
            fda: Vec::new(),
            async_node: None,
        };
        let nested = HOST_SERVING.with(std::cell::Cell::get).filter(|(p, _)| *p == target).map(|(_, tid)| tid);
        st.queue(target, nested, Work::Txn(Box::new(txn)));
        let deadline = Instant::now() + HOST_REPLY_TIMEOUT;
        let result = loop {
            let seen = crate::poll::generation();
            let work = st.proc_mut(HOST).threads.entry(tid).or_default().todo.pop_front();
            match work {
                Some(Work::Txn(r)) if r.reply => break Ok(r.data),
                Some(Work::DeadReply | Work::FailedReply) => break Err(EPIPE),
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
    /// `IComposerCallback.onVsync`): queued for the target's process, not waited on.
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
        for &off in offsets {
            let off = off as usize;
            let obj = data.get(off..off + 24).ok_or(EINVAL)?.to_vec();
            if let Some(to) = st.translate_ref((HOST, 0), target, &obj)? {
                rewrite_ref(&mut data, off, to);
            }
        }
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
        };
        st.queue(target, None, Work::Txn(Box::new(txn)));
        Ok(())
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
struct Parcel {
    bytes: Vec<u8>,
}

impl Parcel {
    /// `Parcel::writeInterfaceToken`, byte for byte as the image's libbinder writes it: strict-mode
    /// policy, work source, the partition header, the interface's name.
    fn with_interface_token(interface: &str) -> Self {
        let mut p = Self { bytes: Vec::new() };
        p.i32(STRICT_MODE_PENALTY_GATHER);
        p.i32(UNSET_WORK_SOURCE);
        p.i32(INTERFACE_HEADER_SYSTEM);
        p.string16(interface);
        p
    }

    fn i32(&mut self, v: i32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// Its length in UTF-16 units, the units and a NUL, padded to 4 bytes.
    fn string16(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
        self.i32(units.len() as i32 - 1);
        for u in units {
            self.bytes.extend_from_slice(&u.to_le_bytes());
        }
        self.bytes.resize((self.bytes.len() + 3) & !3, 0);
    }

    /// `writeStrongBinder` of host service `ptr`: a `flat_binder_object` and its stability. The
    /// object's offset, for the offsets array.
    fn binder(&mut self, ptr: u64, stability: i32) -> u64 {
        let at = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&TYPE_BINDER.to_le_bytes());
        self.bytes.extend_from_slice(&FLAT_BINDER_FLAG_ACCEPTS_FDS.to_le_bytes());
        self.bytes.extend_from_slice(&ptr.to_le_bytes());
        self.bytes.extend_from_slice(&ptr.to_le_bytes());
        self.i32(stability);
        at
    }
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
        let dying: Vec<NodeId> = st.nodes.iter().filter(|(_, n)| n.owner == self.id).map(|(id, _)| *id).collect();
        for node in dying {
            let watchers = {
                let n = st.nodes.get_mut(&node).expect("node");
                n.dead = true;
                std::mem::take(&mut n.watchers)
            };
            for (proc, cookie) in watchers {
                st.queue(proc, None, Work::DeadBinder { cookie });
            }
            if st.context_mgr == Some(node) {
                st.context_mgr = None;
            }
        }
        let pending: Vec<(ProcId, i32)> = st
            .procs
            .get(&self.id)
            .map(|p| {
                p.todo
                    .iter()
                    .chain(p.threads.values().flat_map(|t| t.todo.iter()))
                    .filter_map(|w| match w {
                        Work::Txn(t) if !t.reply && !t.oneway => t.from,
                        _ => None,
                    })
                    .chain(p.threads.values().flat_map(|t| t.serving.iter().filter_map(|f| *f)))
                    .collect()
            })
            .unwrap_or_default();
        for (proc, tid) in pending {
            st.queue(proc, Some(tid), Work::DeadReply);
        }
        // Its references are gone with it: an object no live process refers to any more is let go
        // by its owner (`BR_RELEASE`, `BR_DECREFS`), as the kernel drops a dead process's refs --
        // SurfaceFlinger removes a client's layers when their handles are released so.
        let held: Vec<NodeId> = st.procs.get_mut(&self.id).map(|p| {
            p.by_node.clear();
            std::mem::take(&mut p.refs).into_values().collect()
        }).unwrap_or_default();
        let mut released = 0;
        for node in held {
            if st.release_if_unreferenced(node) {
                released += 1;
            }
        }
        if std::env::var_os("OMNI_BINDER_TRACE").is_some() || released > 0 {
            let pid = st.procs.get(&self.id).map_or(0, |p| p.pid);
            eprintln!("[binder] pid {pid} closed its driver: {released} objects released");
        }
        if let Some(p) = st.procs.get_mut(&self.id) {
            p.dead = true;
            p.todo.clear();
            p.threads.clear();
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
            let n = u32::from_le_bytes(p.mem.read(arg, 4)?.try_into().expect("4"));
            file.broker.state.lock().proc_mut(file.id).max_threads = n;
            Ok(0)
        }
        BINDER_SET_IDLE_TIMEOUT | BINDER_SET_IDLE_PRIORITY | BINDER_ENABLE_ONEWAY_SPAM_DETECTION => Ok(0),
        BINDER_GET_EXTENDED_ERROR => p.mem.write(arg, &[0u8; 12]).map(|()| 0),
        BINDER_SET_CONTEXT_MGR | BINDER_SET_CONTEXT_MGR_EXT => {
            let flags = if cmd == BINDER_SET_CONTEXT_MGR_EXT { u32::from_le_bytes(p.mem.read(arg + 4, 4)?.try_into().expect("4")) } else { 0 };
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
            if let Some(proc) = file.broker.state.lock().procs.get_mut(&file.id) {
                proc.threads.remove(&t.tid);
            }
            Ok(0)
        }
        BINDER_WRITE_READ => write_read(p, t, file, arg, nonblocking),
        _ => Err(EINVAL),
    }
}

fn write_read(p: &Process, t: &mut Task, file: &Arc<BinderFile>, arg: u64, nonblocking: bool) -> SysResult {
    let bwr = p.mem.read(arg, 48)?;
    let (write_size, mut write_consumed, write_buffer) = (u64_at(&bwr, 0), u64_at(&bwr, 8), u64_at(&bwr, 16));
    let (read_size, mut read_consumed, read_buffer) = (u64_at(&bwr, 24), u64_at(&bwr, 32), u64_at(&bwr, 40));
    if std::env::var("OMNI_BINDER_TRACE").as_deref() == Ok("2") {
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
        let cmds = p.mem.read(write_buffer + write_consumed, (write_size - write_consumed).min(1 << 20) as usize)?;
        let mut at = 0usize;
        while at + 4 <= cmds.len() {
            match command(p, t, file, &cmds, at) {
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
                    result = Err(e);
                    break;
                }
            }
        }
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
            // The caller a reply answers, before the reply takes it off the thread.
            let caller = if reply {
                let mut st = file.broker.state.lock();
                st.proc_mut(file.id).threads.entry(t.tid).or_default().serving.last().copied().flatten()
            } else {
                None
            };
            if let Err(e) = transaction(p, t, file, arg, reply) {
                if std::env::var("OMNI_BINDER_TRACE").is_ok() || crate::remote::is_remote() || p.trace {
                    eprintln!("[binder] {}:{} {} failed: {e:?}", p.sys.pid, t.tid, if reply { "reply" } else { "transaction" });
                }
                let mut st = file.broker.state.lock();
                if reply {
                    // binder_transaction's error path for a reply: the replier completes, and the
                    // one waiting for it hears BR_FAILED_REPLY.
                    let th = st.proc_mut(file.id).threads.entry(t.tid).or_default();
                    if th.serving.last().copied().flatten() == caller && caller.is_some() {
                        th.serving.pop();
                    }
                    st.queue(file.id, Some(t.tid), Work::Complete);
                    if let Some((proc, tid)) = caller {
                        st.queue(proc, Some(tid), Work::FailedReply);
                    }
                } else {
                    st.queue(file.id, Some(t.tid), Work::FailedReply);
                }
                return Err(Failed::Transaction(4 + size));
            }
        }
        BC_FREE_BUFFER => {
            let ptr = u64_at(arg, 0);
            file.area.lock().free(ptr);
            // A oneway transaction's buffer: its node's next oneway goes to this thread.
            let mut st = file.broker.state.lock();
            if let Some(node) = st.async_buffers.remove(&(file.id, ptr)) {
                st.async_done(node, file.id, Some(t.tid));
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
            st.proc_mut(file.id).threads.entry(t.tid).or_default().looper = false;
        }
        BC_REQUEST_DEATH_NOTIFICATION | BC_CLEAR_DEATH_NOTIFICATION => {
            let (handle, cookie) = (u32_at(arg, 0), u64_at(arg, 4));
            let mut st = file.broker.state.lock();
            if let Some(node) = st.node_for_handle(file.id, handle) {
                if code == BC_REQUEST_DEATH_NOTIFICATION {
                    let dead = st.nodes.get(&node).is_none_or(|n| n.dead);
                    if dead {
                        st.queue(file.id, None, Work::DeadBinder { cookie });
                    } else if let Some(n) = st.nodes.get_mut(&node) {
                        n.watchers.push((file.id, cookie));
                    }
                } else {
                    if let Some(n) = st.nodes.get_mut(&node) {
                        n.watchers.retain(|w| *w != (file.id, cookie));
                    }
                    st.queue(file.id, Some(t.tid), Work::ClearDeathDone { cookie });
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
/// and queue it.
fn transaction(p: &Process, t: &mut Task, file: &Arc<BinderFile>, tr: &[u8], reply: bool) -> Result<(), Errno> {
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
    let (target_proc, target_tid, target_ptr, target_cookie, secctx, from) = if reply {
        let serving = st.proc_mut(file.id).threads.entry(t.tid).or_default().serving.pop().flatten();
        let Some((proc, tid)) = serving else {
            // The one who asked is gone, or this was not a sync transaction: nothing to answer.
            st.queue(file.id, Some(t.tid), Work::Complete);
            return Ok(());
        };
        (proc, Some(tid), 0, 0, false, None)
    } else {
        let Some(node) = st.node_for_handle(file.id, handle) else {
            st.queue(file.id, Some(t.tid), Work::DeadReply);
            return Ok(());
        };
        let n = st.nodes.get(&node).expect("a node");
        if n.dead {
            st.queue(file.id, Some(t.tid), Work::DeadReply);
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
        (owner, nested, ptr, cookie, sec, (!oneway).then_some((file.id, t.tid)))
    };

    // Translate the objects for the receiver.
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
                if let Some(to) = st.translate_ref((file.id, t.tid), target_proc, &obj)? {
                    rewrite_ref(&mut data, off, to);
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
    };
    let mut txn = txn;
    if !reply && !oneway {
        st.proc_mut(file.id).threads.entry(t.tid).or_default().awaiting += 1;
    }
    if !reply && target_proc == HOST {
        // A host service answers on a host thread of its own, while the sender waits in `read`
        // as it would for any process: a handler may take time, and may call back into the
        // sender, which the waiting thread then serves. The sender hears its transaction went,
        // then the reply, as the kernel orders them.
        let handler = st.host_services.get(&target_ptr).cloned();
        st.queue(file.id, Some(t.tid), Work::Complete);
        drop(st);
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
        std::thread::Builder::new()
            .name("omni-binder-host".into())
            .spawn(move || {
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
            })
            .map_err(|_| ENOMEM)?;
        return Ok(());
    }
    let dead = st.procs.get(&target_proc).is_none_or(|pr| pr.dead);
    if dead {
        st.queue(file.id, Some(t.tid), if reply { Work::FailedReply } else { Work::DeadReply });
        return Ok(());
    }
    // A oneway transaction waits while one to the same node is out, as the driver orders them.
    if oneway {
        if let Some(id) = st.local.get(&(target_proc, target_ptr)).copied() {
            txn.async_node = Some(id);
            if let Some(n) = st.nodes.get_mut(&id) {
                if n.has_async {
                    n.async_todo.push_back(Work::Txn(Box::new(txn)));
                    if n.async_todo.len() % 64 == 0 {
                        let (owner, queued) = (n.owner, n.async_todo.len());
                        let pid = st.procs.get(&owner).map_or(0, |pr| pr.pid);
                        eprintln!("[binder] {queued} oneway calls wait on node {id} of pid {pid} (its last one's buffer not freed)");
                    }
                    st.queue(file.id, Some(t.tid), Work::Complete);
                    return Ok(());
                }
                n.has_async = true;
            }
        }
    }
    st.queue(target_proc, target_tid, Work::Txn(Box::new(txn)));
    st.queue(file.id, Some(t.tid), Work::Complete);
    Ok(())
}

/// Fill the read buffer with work for this thread, waiting for some when there is none.
#[allow(clippy::too_many_arguments)]
fn read(p: &Process, t: &mut Task, file: &Arc<BinderFile>, at: u64, size: u64, first: bool, nonblocking: bool) -> Result<u64, Errno> {
    let mut out: Vec<u8> = Vec::new();
    if first {
        out.extend_from_slice(&BR_NOOP.to_le_bytes());
    }
    let flushed = file.flushes.load(std::sync::atomic::Ordering::SeqCst);
    loop {
        let seen = crate::poll::generation();
        let work = {
            let mut st = file.broker.state.lock();
            let proc = st.proc_mut(file.id);
            let thread = proc.threads.entry(t.tid).or_default();
            let own = thread.todo.pop_front();
            // A thread waiting for a reply takes only its own work; an idle looper also the
            // process's.
            let may_take_proc = thread.awaiting == 0 && thread.serving.is_empty();
            let looper = thread.looper;
            match own {
                Some(w) => Some(w),
                None if may_take_proc => {
                    let w = proc.todo.pop_front();
                    // Ask for another looper when this one takes the last idle slot.
                    if w.is_some() && looper && proc.spawn_requested == 0 {
                        let loopers = proc.threads.values().filter(|th| th.looper).count() as u32;
                        if loopers < proc.max_threads + 1 {
                            proc.spawn_requested += 1;
                            out.extend_from_slice(&BR_SPAWN_LOOPER.to_le_bytes());
                        }
                    }
                    w
                }
                None => None,
            }
        };
        match work {
            Some(w) => {
                deliver(p, t, file, w, &mut out)?;
                // Return what is there; more comes on the next read.
                let n = (out.len() as u64).min(size);
                p.mem.write(at, &out[..n as usize])?;
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
            None => crate::poll::wait_for_change(seen, None, t)?,
        }
    }
}

fn deliver(p: &Process, t: &mut Task, file: &Arc<BinderFile>, work: Work, out: &mut Vec<u8>) -> Result<(), Errno> {
    let mut put = |cmd: u32, words: &[u64]| {
        out.extend_from_slice(&cmd.to_le_bytes());
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
    };
    match work {
        Work::Complete => put(BR_TRANSACTION_COMPLETE, &[]),
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
            // Install the file descriptors it carries in this process.
            for (off, open) in &txn.fds {
                let fd = p.fds.insert(Arc::clone(open), true, 0)?;
                txn.data[*off + 8..*off + 12].copy_from_slice(&(fd as u32).to_le_bytes());
            }
            let data_len = (txn.data.len() as u64 + 7) & !7;
            let offsets_len = txn.offsets.len() as u64 * 8;
            let sg_len: u64 = txn.sg.iter().map(|b| (b.bytes.len() as u64 + 7) & !7).sum();
            let sec_len = if txn.secctx && !txn.reply { SECCTX.len() as u64 } else { 0 };
            let Some(buf) = file.area.lock().alloc(data_len + offsets_len + sg_len + sec_len) else {
                // Not delivered: the node's next oneway may go.
                if let Some(node) = txn.async_node {
                    file.broker.state.lock().async_done(node, file.id, None);
                }
                return Err(ENOMEM);
            };
            if let Some(node) = txn.async_node {
                file.broker.state.lock().async_buffers.insert((file.id, buf), node);
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
            p.mem.write(buf, &bytes)?;
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
                end_wait(file, t.tid);
                BR_REPLY
            } else {
                // A sync transaction: this thread now serves it, and its reply goes to `from`.
                if !txn.oneway {
                    let mut st = file.broker.state.lock();
                    st.proc_mut(file.id).threads.entry(t.tid).or_default().serving.push(txn.from);
                }
                if sec_len > 0 {
                    tr.extend_from_slice(&(buf + data_len + offsets_len + sg_len).to_le_bytes());
                    BR_TRANSACTION_SEC_CTX
                } else {
                    BR_TRANSACTION
                }
            };
            out.extend_from_slice(&cmd.to_le_bytes());
            out.extend_from_slice(&tr);
        }
    }
    let _ = (BR_ERROR, BR_OK);
    Ok(())
}

/// The reply (or its failure) this thread waited for has come.
fn end_wait(file: &Arc<BinderFile>, tid: i32) {
    let mut st = file.broker.state.lock();
    let th = st.proc_mut(file.id).threads.entry(tid).or_default();
    th.awaiting = th.awaiting.saturating_sub(1);
}
