//! eBPF as Android's loaders and daemons use it (`bpf(2)`): maps, which hold data that netd and
//! system_server read and write; programs, which are loaded and pinned but never run -- no packet
//! or kernel event reaches one here; and the BPF filesystem at `/sys/fs/bpf`, where both are
//! pinned by name. netbpfload and the platform bpfloader create and pin them at boot
//! (`load_bpf_programs`), then set `bpf.progs_loaded`, which netd waits for.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, E2BIG, EBADF, EEXIST, EINVAL, ENOENT, ENOTDIR};
use crate::fd::{FileKind, OpenFile};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const ENOTSUPP: Errno = Errno(524);

// Map types (`enum bpf_map_type`).
const ARRAY: u32 = 2;
const PROG_ARRAY: u32 = 3;
const PERF_EVENT_ARRAY: u32 = 4;
const PERCPU_HASH: u32 = 5;
const PERCPU_ARRAY: u32 = 6;
const LRU_HASH: u32 = 9;
const LRU_PERCPU_HASH: u32 = 10;
const ARRAY_OF_MAPS: u32 = 12;
const DEVMAP: u32 = 14;
const DEVMAP_HASH: u32 = 25;
/// `BPF_F_RDONLY_PROG`: the kernel sets it on a device map (read-only to programs).
const RDONLY_PROG: u32 = 1 << 7;
const RINGBUF: u32 = 27;

/// A map: its definition and its entries, by key.
pub struct Map {
    pub id: u32,
    pub kind: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub flags: u32,
    pub name: [u8; 16],
    entries: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

/// A program: its type and name. Its instructions are not kept: it never runs.
pub struct Prog {
    pub id: u32,
    pub kind: u32,
    pub name: [u8; 16],
    pub expected_attach: u32,
}

/// What a BPF descriptor or pin holds.
#[derive(Clone)]
pub enum Object {
    Map(Arc<Map>),
    Prog(Arc<Prog>),
    /// A BTF blob, a raw tracepoint or a link: held, never consulted.
    Other,
}

impl Object {
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Map(_) => "anon_inode:bpf-map",
            Self::Prog(_) => "anon_inode:bpf-prog",
            Self::Other => "anon_inode:bpf_link",
        }
    }
}

fn next_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Every map and program made, by id (`BPF_*_GET_FD_BY_ID`, `BPF_*_GET_NEXT_ID`).
fn registry() -> &'static Mutex<BTreeMap<u32, (Weak<Map>, Weak<Prog>)>> {
    static R: OnceLock<Mutex<BTreeMap<u32, (Weak<Map>, Weak<Prog>)>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

fn possible_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get).min(8)
}

impl Map {
    fn is_array(&self) -> bool {
        matches!(self.kind, ARRAY | PERCPU_ARRAY | PROG_ARRAY | PERF_EVENT_ARRAY | ARRAY_OF_MAPS | DEVMAP)
    }

    /// The bytes a value is to user space: per-CPU maps hold one (8-byte aligned) value per CPU.
    fn user_value_size(&self) -> usize {
        if matches!(self.kind, PERCPU_HASH | PERCPU_ARRAY | LRU_PERCPU_HASH) {
            (self.value_size as usize).div_ceil(8) * 8 * possible_cpus()
        } else {
            self.value_size as usize
        }
    }

    fn index(&self, key: &[u8]) -> Option<u32> {
        let i = u32::from_le_bytes(key.get(..4)?.try_into().ok()?);
        (i < self.max_entries).then_some(i)
    }

    /// `BPF_MAP_LOOKUP_ELEM`.
    pub fn lookup(&self, key: &[u8]) -> Result<Vec<u8>, Errno> {
        let entries = self.entries.lock();
        if self.is_array() {
            self.index(key).ok_or(ENOENT)?;
            return Ok(entries.get(key).cloned().unwrap_or_else(|| vec![0; self.user_value_size()]));
        }
        entries.get(key).cloned().ok_or(ENOENT)
    }

    /// `BPF_MAP_UPDATE_ELEM` with `BPF_ANY` (0), `BPF_NOEXIST` (1) or `BPF_EXIST` (2).
    pub fn update(&self, key: &[u8], value: &[u8], flags: u64) -> Result<(), Errno> {
        let mut entries = self.entries.lock();
        if self.is_array() {
            self.index(key).ok_or(E2BIG)?;
            if flags & 3 == 1 {
                return Err(EEXIST); // every index of an array exists
            }
        } else {
            let present = entries.contains_key(key);
            match flags & 3 {
                1 if present => return Err(EEXIST),
                2 if !present => return Err(ENOENT),
                _ => {}
            }
            if !present && entries.len() >= self.max_entries as usize {
                if matches!(self.kind, LRU_HASH | LRU_PERCPU_HASH) {
                    let oldest = entries.keys().next().cloned();
                    if let Some(k) = oldest {
                        entries.remove(&k);
                    }
                } else {
                    return Err(E2BIG);
                }
            }
        }
        entries.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    /// `BPF_MAP_DELETE_ELEM`: an array's elements cannot be deleted.
    pub fn delete(&self, key: &[u8]) -> Result<(), Errno> {
        if self.is_array() {
            return Err(EINVAL);
        }
        self.entries.lock().remove(key).map(|_| ()).ok_or(ENOENT)
    }

    /// `BPF_MAP_GET_NEXT_KEY`: the first key with no key (or one not in the map), else the next.
    pub fn next_key(&self, key: Option<&[u8]>) -> Result<Vec<u8>, Errno> {
        if self.is_array() {
            let next = match key.and_then(|k| self.index(k)) {
                Some(i) => i + 1,
                None => 0,
            };
            return if next < self.max_entries { Ok(next.to_le_bytes().to_vec()) } else { Err(ENOENT) };
        }
        let entries = self.entries.lock();
        match key {
            Some(k) if entries.contains_key(k) => entries.range::<[u8], _>((std::ops::Bound::Excluded(k), std::ops::Bound::Unbounded)).next().map(|(k, _)| k.clone()).ok_or(ENOENT),
            _ => entries.keys().next().cloned().ok_or(ENOENT),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The BPF filesystem.

/// What a path under `/sys/fs/bpf` is.
#[derive(Clone)]
pub enum Node {
    Dir,
    Pin(Object),
}

const ROOT: &str = "/sys/fs/bpf";

/// The BPF filesystem: every directory and pin, by guest path (the root is always there).
fn fs() -> &'static Mutex<BTreeMap<String, Node>> {
    static FS: OnceLock<Mutex<BTreeMap<String, Node>>> = OnceLock::new();
    FS.get_or_init(Mutex::default)
}

/// Whether a guest path is on the BPF filesystem.
#[must_use]
pub fn on_bpffs(path: &[u8]) -> bool {
    path == ROOT.as_bytes() || (path.starts_with(ROOT.as_bytes()) && path.get(ROOT.len()) == Some(&b'/'))
}

fn key(path: &[u8]) -> String {
    String::from_utf8_lossy(path).trim_end_matches('/').to_string()
}

fn parent_is_dir(fs: &BTreeMap<String, Node>, path: &str) -> bool {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent == ROOT || matches!(fs.get(parent), Some(Node::Dir)),
        None => false,
    }
}

/// The node at a BPF filesystem path.
#[must_use]
pub fn lookup(path: &[u8]) -> Option<Node> {
    let path = key(path);
    if path == ROOT {
        return Some(Node::Dir);
    }
    fs().lock().get(&path).cloned()
}

/// The names in a BPF filesystem directory, with whether each is a directory.
#[must_use]
pub fn list(path: &[u8]) -> Vec<(String, bool)> {
    let dir = key(path);
    let prefix = format!("{dir}/");
    fs().lock()
        .iter()
        .filter_map(|(p, n)| {
            let rest = p.strip_prefix(&prefix)?;
            (!rest.contains('/')).then(|| (rest.to_string(), matches!(n, Node::Dir)))
        })
        .collect()
}

/// `mkdir` on the BPF filesystem.
pub fn mkdir(path: &[u8]) -> Result<(), Errno> {
    let path = key(path);
    if path == ROOT {
        return Err(EEXIST);
    }
    let mut fs = fs().lock();
    if fs.contains_key(&path) {
        return Err(EEXIST);
    }
    if !parent_is_dir(&fs, &path) {
        return Err(ENOENT);
    }
    fs.insert(path, Node::Dir);
    Ok(())
}

/// `unlink`/`rmdir` on the BPF filesystem (a directory must be empty).
pub fn remove(path: &[u8], dir: bool) -> Result<(), Errno> {
    let path = key(path);
    let mut fs = fs().lock();
    match (fs.get(&path), dir) {
        (None, _) => Err(ENOENT),
        (Some(Node::Dir), false) => Err(crate::errno::EISDIR),
        (Some(Node::Pin(_)), true) => Err(ENOTDIR),
        (Some(Node::Dir), true) if fs.keys().any(|k| k.starts_with(&format!("{path}/"))) => Err(crate::errno::ENOTEMPTY),
        _ => {
            fs.remove(&path);
            Ok(())
        }
    }
}

/// `rename` on the BPF filesystem (`noreplace`: `RENAME_NOREPLACE`, `EEXIST` over a name).
pub fn rename(from: &[u8], to: &[u8], noreplace: bool) -> Result<(), Errno> {
    let (from, to) = (key(from), key(to));
    let mut fs = fs().lock();
    let node = fs.get(&from).cloned().ok_or(ENOENT)?;
    if fs.contains_key(&to) && noreplace {
        return Err(EEXIST);
    }
    if !parent_is_dir(&fs, &to) {
        return Err(ENOENT);
    }
    // A directory moves with everything under it.
    let moved: Vec<(String, Node)> = fs.iter().filter(|(k, _)| k.starts_with(&format!("{from}/"))).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, _) in &moved {
        fs.remove(k);
    }
    fs.remove(&from);
    fs.insert(to.clone(), node);
    for (k, v) in moved {
        fs.insert(format!("{to}{}", &k[from.len()..]), v);
    }
    Ok(())
}

fn pin(path: &[u8], object: Object) -> Result<(), Errno> {
    if !on_bpffs(path) {
        return Err(EINVAL);
    }
    let path = key(path);
    let mut fs = fs().lock();
    if path == ROOT || fs.contains_key(&path) {
        return Err(EEXIST);
    }
    if !parent_is_dir(&fs, &path) {
        return Err(ENOENT);
    }
    fs.insert(path, Node::Pin(object));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// bpf(2).

fn object_of(p: &Process, fd: u32) -> Result<Object, Errno> {
    match &*p.fds.get(fd as i32)?.kind.lock() {
        FileKind::Bpf(o) => Ok(o.clone()),
        _ => Err(EBADF),
    }
}

fn map_of(p: &Process, fd: u32) -> Result<Arc<Map>, Errno> {
    match object_of(p, fd)? {
        Object::Map(m) => Ok(m),
        _ => Err(EINVAL),
    }
}

fn install(p: &Process, object: Object) -> SysResult {
    let file = OpenFile { kind: parking_lot::Mutex::new(FileKind::Bpf(object)), flags: parking_lot::Mutex::new(2) };
    Ok(p.fds.insert(Arc::new(file), true, 0)? as u64)
}

fn u32_at(attr: &[u8], at: usize) -> u32 {
    attr.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().expect("4")))
}

fn u64_at(attr: &[u8], at: usize) -> u64 {
    attr.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8")))
}

fn name_at(attr: &[u8], at: usize) -> [u8; 16] {
    let mut name = [0u8; 16];
    if let Some(b) = attr.get(at..at + 16) {
        name.copy_from_slice(b);
        name[15] = 0;
    }
    name
}

/// `BPF_MAP_CREATE`: `{map_type, key_size, value_size, max_entries, map_flags, inner_map_fd,
/// numa_node, map_name[16]}`.
fn map_create(p: &Process, attr: &[u8]) -> SysResult {
    let (kind, key_size, value_size, max_entries, flags) = (u32_at(attr, 0), u32_at(attr, 4), u32_at(attr, 8), u32_at(attr, 12), u32_at(attr, 16));
    if max_entries == 0 {
        return Err(EINVAL);
    }
    match kind {
        RINGBUF => {
            if key_size != 0 || value_size != 0 || !max_entries.is_power_of_two() || max_entries < 4096 {
                return Err(EINVAL);
            }
        }
        ARRAY | PERCPU_ARRAY | PROG_ARRAY | PERF_EVENT_ARRAY | ARRAY_OF_MAPS | DEVMAP if key_size != 4 => return Err(EINVAL),
        _ if key_size == 0 || value_size == 0 => return Err(EINVAL),
        _ => {}
    }
    let flags = if matches!(kind, DEVMAP | DEVMAP_HASH) { flags | RDONLY_PROG } else { flags };
    let map = Arc::new(Map { id: next_id(), kind, key_size, value_size, max_entries, flags, name: name_at(attr, 28), entries: Mutex::default() });
    registry().lock().insert(map.id, (Arc::downgrade(&map), Weak::new()));
    install(p, Object::Map(map))
}

/// `BPF_MAP_LOOKUP_ELEM`, `UPDATE_ELEM`, `DELETE_ELEM`, `GET_NEXT_KEY`: `{map_fd, pad, key,
/// value (or next_key), flags}`.
fn map_elem(p: &Process, cmd: u64, attr: &[u8]) -> SysResult {
    let map = map_of(p, u32_at(attr, 0))?;
    let (key_at, value_at, flags) = (u64_at(attr, 8), u64_at(attr, 16), u64_at(attr, 24));
    let key = |at: u64| p.mem.read(at, map.key_size as usize);
    match cmd {
        1 => {
            let value = map.lookup(&key(key_at)?)?;
            p.mem.write(value_at, &value)?;
        }
        2 => {
            let value = p.mem.read(value_at, map.user_value_size())?;
            map.update(&key(key_at)?, &value, flags)?;
        }
        3 => map.delete(&key(key_at)?)?,
        4 => {
            let current = if key_at == 0 { None } else { Some(key(key_at)?) };
            let next = map.next_key(current.as_deref())?;
            p.mem.write(value_at, &next)?;
        }
        _ => return Err(EINVAL),
    }
    Ok(0)
}

/// `BPF_PROG_LOAD`: `{prog_type, insn_cnt, insns, license, log_level, log_size, log_buf,
/// kern_version, prog_flags, prog_name[16], prog_ifindex, expected_attach_type}`.
fn prog_load(p: &Process, attr: &[u8]) -> SysResult {
    let (kind, insn_cnt, insns, license) = (u32_at(attr, 0), u32_at(attr, 4), u64_at(attr, 8), u64_at(attr, 16));
    if insn_cnt == 0 || insns == 0 || license == 0 {
        return Err(EINVAL);
    }
    let prog = Arc::new(Prog { id: next_id(), kind, name: name_at(attr, 48), expected_attach: u32_at(attr, 68) });
    registry().lock().insert(prog.id, (Weak::new(), Arc::downgrade(&prog)));
    install(p, Object::Prog(prog))
}

/// `BPF_OBJ_GET_INFO_BY_FD`: `{bpf_fd, info_len, info}`; the kernel writes back how much it wrote.
fn info_by_fd(p: &Process, attr_at: u64, attr: &[u8]) -> SysResult {
    let object = object_of(p, u32_at(attr, 0))?;
    let room = u32_at(attr, 4) as usize;
    let mut info = vec![0u8; 256];
    let put = |info: &mut Vec<u8>, at: usize, v: u32| info[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let len = match &object {
        // struct bpf_map_info: type, id, key_size, value_size, max_entries, map_flags, name[16], ...
        Object::Map(m) => {
            put(&mut info, 0, m.kind);
            put(&mut info, 4, m.id);
            put(&mut info, 8, m.key_size);
            put(&mut info, 12, m.value_size);
            put(&mut info, 16, m.max_entries);
            put(&mut info, 20, m.flags);
            info[24..40].copy_from_slice(&m.name);
            88
        }
        // struct bpf_prog_info: type, id, tag[8], ..., name[16] at 64.
        Object::Prog(pr) => {
            put(&mut info, 0, pr.kind);
            put(&mut info, 4, pr.id);
            info[64..80].copy_from_slice(&pr.name);
            228
        }
        Object::Other => return Err(EINVAL),
    };
    let n = len.min(room);
    p.mem.write(u64_at(attr, 8), &info[..n])?;
    p.mem.write_u32(attr_at + 4, n as u32)?;
    Ok(0)
}

fn sys_bpf(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let (cmd, attr_at, size) = (a[0], a[1], (a[2] as usize).min(256));
    let mut attr = p.mem.read(attr_at, size)?;
    attr.resize(256, 0);
    match cmd {
        0 => map_create(p, &attr),
        1..=4 => map_elem(p, cmd, &attr),
        5 => prog_load(p, &attr),
        6 => {
            // BPF_OBJ_PIN: {pathname, bpf_fd, file_flags}
            let path = p.mem.read_cstr(u64_at(&attr, 0), 4096)?;
            let object = object_of(p, u32_at(&attr, 8))?;
            pin(&path, object).map(|()| 0)
        }
        7 => {
            // BPF_OBJ_GET
            let path = p.mem.read_cstr(u64_at(&attr, 0), 4096)?;
            match lookup(&path) {
                Some(Node::Pin(object)) => install(p, object),
                Some(Node::Dir) => Err(EINVAL),
                None => Err(ENOENT),
            }
        }
        // BPF_PROG_ATTACH, BPF_PROG_DETACH: accepted; nothing here would run the program.
        8 | 9 => Ok(0),
        // BPF_PROG_GET_NEXT_ID, BPF_MAP_GET_NEXT_ID: {start_id, next_id}
        11 | 12 => {
            let start = u32_at(&attr, 0);
            let next = registry().lock().iter().find(|(id, (m, pr))| **id > start && if cmd == 12 { m.strong_count() > 0 } else { pr.strong_count() > 0 }).map(|(id, _)| *id).ok_or(ENOENT)?;
            p.mem.write_u32(attr_at + 4, next)?;
            Ok(0)
        }
        // BPF_PROG_GET_FD_BY_ID, BPF_MAP_GET_FD_BY_ID: {id}
        13 | 14 => {
            let id = u32_at(&attr, 0);
            let entry = registry().lock().get(&id).cloned().ok_or(ENOENT)?;
            let object = if cmd == 14 { entry.0.upgrade().map(Object::Map) } else { entry.1.upgrade().map(Object::Prog) };
            install(p, object.ok_or(ENOENT)?)
        }
        15 => info_by_fd(p, attr_at, &attr),
        // BPF_PROG_QUERY: nothing attached.
        16 => {
            p.mem.write_u32(attr_at + 12, 0)?; // attach_flags
            p.mem.write_u32(attr_at + 24, 0)?; // prog_cnt
            Ok(0)
        }
        // BPF_RAW_TRACEPOINT_OPEN, BPF_BTF_LOAD, BPF_LINK_CREATE: a descriptor that holds it.
        17 | 18 | 28 => install(p, Object::Other),
        // BPF_MAP_FREEZE
        22 => map_of(p, u32_at(&attr, 0)).map(|_| 0),
        other => {
            p.refusals.record(format!("bpf cmd {other}"), t.pc, t.lr);
            Err(ENOTSUPP)
        }
    }
}

pub fn install_syscalls(table: &mut Table) {
    table.set(nr::BPF, sys_bpf);
}
