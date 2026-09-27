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

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EAGAIN, EFAULT, EINVAL, ENOMEM};
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

struct Node {
    owner: ProcId,
    ptr: u64,
    cookie: u64,
    /// `BR_INCREFS`/`BR_ACQUIRE` sent: the owner holds the object for its remote references.
    held: bool,
    txn_security_ctx: bool,
    dead: bool,
    watchers: Vec<(ProcId, u64)>,
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
    /// (owner, ptr) -> node
    local: HashMap<(ProcId, u64), NodeId>,
    context_mgr: Option<NodeId>,
    next_proc: ProcId,
    next_node: NodeId,
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
        let h = proc.refs.keys().next_back().map_or(1, |k| k + 1).max(1);
        proc.refs.insert(h, node);
        proc.by_node.insert(node, h);
        let n = self.nodes.get_mut(&node).expect("a node");
        if !n.held {
            n.held = true;
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
                txn_security_ctx: flags & FLAT_BINDER_FLAG_TXN_SECURITY_CTX != 0,
                dead: false,
                watchers: Vec::new(),
            },
        );
        self.local.insert((owner, ptr), id);
        id
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
        Arc::new(Self { broker, id, area: Mutex::default() })
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
    /// The process is gone (its last descriptor on the driver closed): its nodes die, the watchers
    /// of each hear so, and every sync transaction waiting on it fails.
    fn drop(&mut self) {
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

pub fn ioctl(p: &Process, t: &mut Task, file: &Arc<BinderFile>, cmd: u64, arg: u64) -> SysResult {
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
        BINDER_WRITE_READ => write_read(p, t, file, arg),
        _ => Err(EINVAL),
    }
}

fn write_read(p: &Process, t: &mut Task, file: &Arc<BinderFile>, arg: u64) -> SysResult {
    let bwr = p.mem.read(arg, 48)?;
    let (write_size, mut write_consumed, write_buffer) = (u64_at(&bwr, 0), u64_at(&bwr, 8), u64_at(&bwr, 16));
    let (read_size, mut read_consumed, read_buffer) = (u64_at(&bwr, 24), u64_at(&bwr, 32), u64_at(&bwr, 40));
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
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        write_consumed += at as u64;
    }
    if result.is_ok() && read_size > read_consumed {
        let nonblocking = false;
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

/// One `BC_*` command at `at` in `cmds`: its length, or an error that stops the write.
fn command(p: &Process, t: &mut Task, file: &Arc<BinderFile>, cmds: &[u8], at: usize) -> Result<usize, Errno> {
    let code = u32_at(cmds, at);
    let size = ((code >> 16) & 0x3fff) as usize;
    let arg = cmds.get(at + 4..at + 4 + size).ok_or(EFAULT)?;
    match code {
        BC_TRANSACTION | BC_REPLY | BC_TRANSACTION_SG | BC_REPLY_SG => {
            transaction(p, t, file, arg, matches!(code, BC_REPLY | BC_REPLY_SG))?;
        }
        BC_FREE_BUFFER => file.area.lock().free(u64_at(arg, 0)),
        BC_INCREFS | BC_ACQUIRE | BC_RELEASE | BC_DECREFS | BC_INCREFS_DONE | BC_ACQUIRE_DONE | BC_DEAD_BINDER_DONE => {}
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
            return Err(EINVAL);
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
        let kind = u32_at(&obj, 0);
        let (ptr, cookie) = (u64_at(&obj, 8), u64_at(&obj, 16));
        let rewrite = |data: &mut Vec<u8>, kind: u32, value: u64, cookie: u64| {
            data[off..off + 4].copy_from_slice(&kind.to_le_bytes());
            data[off + 8..off + 16].copy_from_slice(&value.to_le_bytes());
            data[off + 16..off + 24].copy_from_slice(&cookie.to_le_bytes());
        };
        match kind {
            TYPE_BINDER | TYPE_WEAK_BINDER => {
                let node = st.local_node(file.id, ptr, cookie, u32_at(&obj, 4));
                if target_proc == file.id {
                    continue;
                }
                let h = st.handle_for(target_proc, node, Some((file.id, t.tid)));
                let kind = if kind == TYPE_BINDER { TYPE_HANDLE } else { TYPE_WEAK_HANDLE };
                rewrite(&mut data, kind, u64::from(h), 0);
            }
            TYPE_HANDLE | TYPE_WEAK_HANDLE => {
                let node = st.node_for_handle(file.id, ptr as u32).ok_or(EINVAL)?;
                let n = st.nodes.get(&node).expect("a node");
                if n.owner == target_proc {
                    let (nptr, ncookie) = (n.ptr, n.cookie);
                    let kind = if kind == TYPE_HANDLE { TYPE_BINDER } else { TYPE_WEAK_BINDER };
                    rewrite(&mut data, kind, nptr, ncookie);
                } else {
                    let h = st.handle_for(target_proc, node, Some((file.id, t.tid)));
                    rewrite(&mut data, kind, u64::from(h), 0);
                }
            }
            TYPE_FD => {
                let fd = ptr as u32 as i32;
                let open = p.fds.get(fd)?;
                fds.push((off, open));
            }
            other => {
                p.refusals.record(format!("binder object type {other:#x}"), t.pc, t.lr);
                return Err(EINVAL);
            }
        }
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
        sender_euid: p.sys.uid,
        data,
        offsets,
        fds,
        sg,
        fda,
    };
    if !reply && !oneway {
        st.proc_mut(file.id).threads.entry(t.tid).or_default().awaiting += 1;
    }
    let dead = st.procs.get(&target_proc).is_none_or(|pr| pr.dead);
    if dead {
        st.queue(file.id, Some(t.tid), if reply { Work::FailedReply } else { Work::DeadReply });
        return Ok(());
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
            let buf = file.area.lock().alloc(data_len + offsets_len + sg_len + sec_len).ok_or(ENOMEM)?;
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
