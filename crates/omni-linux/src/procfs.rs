//! `/proc` and `/sys`, generated from the process itself (milestone A2).
//!
//! `vfs` reaches this through the [`ProcFs`] trait object a process attaches to its `Vfs` once it
//! exists, so `vfs` does not depend on `process`. A generated file's bytes are produced when it is
//! opened and read from that snapshot, as Linux's seq files are.
use std::fmt::Write as _;

use omni_mem::Protection;

use crate::process::Process;
use crate::vfs::{ino_of, DirEnt, Node, DT_DIR, DT_LNK, DT_REG};

/// The generated part of the file tree.
pub trait ProcFs: Send + Sync {
    /// The node at a normalized absolute path under `/proc` or `/sys`; `None` if there is none.
    fn node(&self, path: &[u8]) -> Option<Node>;
    /// The entries of a generated directory.
    fn list(&self, path: &[u8]) -> Vec<DirEnt>;
    /// The bytes of a generated file.
    fn read(&self, path: &[u8]) -> Option<Vec<u8>>;
}

/// `/dev/__properties__`'s three files, built once when the process is.
pub struct PropFiles {
    pub info: Vec<u8>,
    pub serial: Vec<u8>,
    pub area: Vec<u8>,
    /// `/apex/apex-info-list.xml`, as apexd writes it (sub-project B).
    pub apex_info: Vec<u8>,
}

/// What a path under `/proc` or `/sys` names.
enum Entry {
    Dir(Vec<(&'static str, u8)>),
    DynDir(Vec<(String, u8)>),
    File(fn(&Process) -> Vec<u8>),
    /// A generated file whose bytes depend on its path.
    Bytes(Vec<u8>),
    Link(Vec<u8>),
}

/// This boot's id, a random UUID (`/proc/sys/kernel/random/boot_id`): libcutils names the
/// ashmem device after it (`/dev/ashmem<boot_id>`).
#[must_use]
pub fn boot_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let mut b = [0u8; 16];
        let _ = omni_platform::process::random_bytes(&mut b);
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
    })
}

/// A fresh random UUID each read (`/proc/sys/kernel/random/uuid`).
fn random_uuid(_p: &Process) -> Vec<u8> {
    let mut b = [0u8; 16];
    let _ = omni_platform::process::random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}\n", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]).into_bytes()
}

fn boot_id_file(_p: &Process) -> Vec<u8> {
    format!("{}\n", boot_id()).into_bytes()
}

/// The image's APEXes as mounted at boot (`crate::apex::mounts`), computed once.
fn apex_mounts(p: &Process) -> &'static [crate::apex::ApexMount] {
    static MOUNTS: std::sync::OnceLock<Vec<crate::apex::ApexMount>> = std::sync::OnceLock::new();
    MOUNTS.get_or_init(|| crate::apex::mounts(p.vfs.sysroot()))
}

/// The CPUs this device has: at most 8 (D37), as `sched_getaffinity` answers.
fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from).min(8)
}

fn with_nl(mut v: Vec<u8>) -> Vec<u8> {
    v.push(b'\n');
    v
}

fn maps(p: &Process) -> Vec<u8> {
    let mut out = String::new();
    for r in p.mem.space().mapped_regions() {
        let start = r.start as u64;
        let end = start + r.len as u64;
        let perms = match r.protection {
            Protection::None => "---p",
            Protection::Read => "r--p",
            Protection::ReadWrite => "rw-p",
            Protection::ReadExecute => "r-xp",
            Protection::ReadWriteExecute => "rwxp",
        };
        match p.mm.name_at(start) {
            Some((name, offset)) if name.first() == Some(&b'/') => {
                let inode = ino_of(&name) & 0xff_ffff;
                let name = String::from_utf8_lossy(&name);
                let _ = writeln!(out, "{start:08x}-{end:08x} {perms} {offset:08x} fe:00 {inode:<10} {name}");
            }
            Some((name, _)) => {
                let name = String::from_utf8_lossy(&name);
                let _ = writeln!(out, "{start:08x}-{end:08x} {perms} 00000000 00:00 0          {name}");
            }
            None => {
                let _ = writeln!(out, "{start:08x}-{end:08x} {perms} 00000000 00:00 0");
            }
        }
    }
    out.into_bytes()
}

fn mapped_bytes(p: &Process) -> u64 {
    p.mem.space().mapped_regions().iter().map(|r| r.len as u64).sum()
}

fn committed_pages(p: &Process) -> u64 {
    p.mem.space().mapped_regions().iter().map(|r| r.committed as u64).sum::<u64>() / 4096
}

/// Clock ticks (100 Hz) since the process started.
fn ticks(p: &Process) -> u64 {
    p.sys.uptime().as_millis() as u64 / 10
}

fn stat(p: &Process) -> Vec<u8> {
    let pid = p.sys.pid;
    let comm = String::from_utf8_lossy(&p.comm.lock()).into_owned();
    let t = ticks(p);
    // proc(5)'s 52 fields: pid, (comm), state, then these 49. What nothing here tracks is 0.
    let fields: [u64; 49] = [
        1,                 // ppid
        pid as u64,        // pgrp
        pid as u64,        // session
        0,                 // tty_nr
        0,                 // tpgid
        0x40_0100,         // flags
        0, 0, 0, 0,        // minflt cminflt majflt cmajflt
        t, 0, 0, 0,        // utime stime cutime cstime
        20, 0,             // priority nice
        p.tids().len() as u64, // num_threads
        0,                 // itrealvalue
        0,                 // starttime
        mapped_bytes(p),   // vsize
        committed_pages(p),// rss
        u64::MAX,          // rsslim
        // startstack: bionic finds the main thread's stack by the maps line holding it.
        0, 0, p.start.lock().map_or(0, |(_, sp)| sp), 0, 0, // startcode endcode startstack kstkesp kstkeip
        0, 0, 0, 0,        // signal blocked sigignore sigcatch
        0, 0, 0,           // wchan nswap cnswap
        17, 0,             // exit_signal processor
        0, 0, 0, 0, 0,     // rt_priority policy delayacct guest_time cguest_time
        0, 0, 0,           // start_data end_data start_brk
        0, 0, 0, 0,        // arg_start arg_end env_start env_end
        0,                 // exit_code
    ];
    let mut out = format!("{pid} ({comm}) R");
    for f in fields {
        let _ = write!(out, " {f}");
    }
    with_nl(out.into_bytes())
}

fn status(p: &Process) -> Vec<u8> {
    let pid = p.sys.pid;
    let (uid, gid) = (p.sys.uid(), p.sys.gid());
    let name = String::from_utf8_lossy(&p.comm.lock()).into_owned();
    let mut out = String::new();
    let _ = write!(out, "Name:\t{name}\nUmask:\t{:04o}\nState:\tR (running)\n", p.sys.umask());
    let _ = write!(out, "Tgid:\t{pid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t1\nTracerPid:\t0\n");
    let _ = write!(out, "Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{gid}\t{gid}\t{gid}\t{gid}\n");
    let _ = write!(out, "FDSize:\t64\nGroups:\t\nVmSize:\t{} kB\nVmRSS:\t{} kB\n", mapped_bytes(p) / 1024, committed_pages(p) * 4);
    let threads = p.tids().len();
    let _ = write!(out, "Threads:\t{threads}\nSigQ:\t0/0\nSigPnd:\t0000000000000000\nCpus_allowed_list:\t0-{}\n", cpus() - 1);
    out.into_bytes()
}

fn statm(p: &Process) -> Vec<u8> {
    format!("{} {} 0 0 0 0 0\n", mapped_bytes(p) / 4096, committed_pages(p)).into_bytes()
}

fn cmdline(p: &Process) -> Vec<u8> {
    p.argv.iter().flat_map(|a| a.iter().copied().chain(std::iter::once(0))).collect()
}

fn comm(p: &Process) -> Vec<u8> {
    with_nl(p.comm.lock().clone())
}

fn limits(_p: &Process) -> Vec<u8> {
    let mut out = String::from("Limit                     Soft Limit           Hard Limit           Units     \n");
    let show = |v: u64| if v == u64::MAX { "unlimited".to_string() } else { v.to_string() };
    for (name, resource, unit) in [("Max stack size", 3, "bytes"), ("Max open files", 7, "files")] {
        let (soft, hard) = crate::sys::limit(resource);
        let _ = writeln!(out, "{name:<26}{:<21}{:<21}{unit:<10}", show(soft), show(hard));
    }
    out.into_bytes()
}

fn mounts(p: &Process) -> Vec<u8> {
    let lines = [
        "/dev/root / ext4 ro 0 0",
        "proc /proc proc rw 0 0",
        "sysfs /sys sysfs rw 0 0",
        "/dev/root /system ext4 ro 0 0",
        "/dev/root /apex ext4 ro 0 0",
        "/dev/data /data ext4 rw 0 0",
        "tmpfs /tmp tmpfs rw 0 0",
    ];
    let mut out = String::new();
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    // What processes of the instance bind-mounted (vold's /data/data on /data/user/0).
    for (target, _) in p.vfs.binds().list() {
        let _ = writeln!(out, "/dev/data {} ext4 rw 0 0", String::from_utf8_lossy(&target));
    }
    // The APEXes, as apexd mounts them (and as its PopulateFromMounts reads them back).
    for m in apex_mounts(p) {
        let _ = writeln!(out, "/dev/block/loop{} /apex/{}@{} ext4 ro,dirsync,seclabel,nodev,noatime 0 0", m.index, m.name, m.version);
        let _ = writeln!(out, "/dev/block/loop{} /apex/{} ext4 ro,dirsync,seclabel,nodev,noatime 0 0", m.index, m.name);
    }
    out.into_bytes()
}

fn cpuinfo(_p: &Process) -> Vec<u8> {
    let mut out = String::new();
    for n in 0..cpus() {
        // `Features` names exactly `exec::HWCAP`: no `atomics` (D26).
        let _ = write!(out, "processor\t: {n}\nBogoMIPS\t: 48.00\n");
        let _ = write!(out, "Features\t: fp asimd aes pmull sha1 sha2 crc32\n");
        let _ = write!(out, "CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0xd08\nCPU revision\t: 3\n\n");
    }
    out.into_bytes()
}

fn meminfo(_p: &Process) -> Vec<u8> {
    let total = 8u64 << 20; // kB: D36 caps the device at 8 GiB
    let mut out = String::new();
    let _ = write!(out, "MemTotal:       {total} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\n", total / 2, total / 2);
    let _ = write!(out, "Buffers:        0 kB\nCached:         0 kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\n");
    out.into_bytes()
}

fn proc_stat(p: &Process) -> Vec<u8> {
    let t = ticks(p);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let boot = now.saturating_sub(crate::sys::monotonic().as_secs());
    let mut out = format!("cpu  {t} 0 0 {t} 0 0 0 0 0 0\n");
    for n in 0..cpus() {
        let _ = writeln!(out, "cpu{n} {t} 0 0 {t} 0 0 0 0 0 0");
    }
    let _ = write!(out, "intr 0\nctxt 0\nbtime {boot}\nprocesses 1\nprocs_running 1\nprocs_blocked 0\n");
    out.into_bytes()
}

fn uptime(_p: &Process) -> Vec<u8> {
    let up = crate::sys::monotonic().as_secs_f64();
    format!("{up:.2} {:.2}\n", up * cpus() as f64).into_bytes()
}

fn loadavg(p: &Process) -> Vec<u8> {
    format!("0.00 0.00 0.00 1/1 {}\n", p.sys.pid).into_bytes()
}

fn filesystems(_p: &Process) -> Vec<u8> {
    b"nodev\tsysfs\nnodev\tproc\nnodev\ttmpfs\nnodev\tselinuxfs\nnodev\tbinder\n\text4\n".to_vec()
}

/// `struct selinux_status_t`: version 1, sequence 0, permissive, no policy loads, allow unknown.
fn selinux_status(_p: &Process) -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    b[0..4].copy_from_slice(&1u32.to_le_bytes());
    b
}

fn zero(_p: &Process) -> Vec<u8> {
    b"0".to_vec()
}

fn one(_p: &Process) -> Vec<u8> {
    b"1".to_vec()
}

fn policyvers(_p: &Process) -> Vec<u8> {
    b"33".to_vec()
}

/// The process's SELinux context: its domain as a device's policy names it, by program.
/// The SELinux attributes a process sets for what it does next (`/proc/<pid>/attr/<name>`):
/// the context of the files it creates, of the program it executes, of its keys and sockets.
const SETTABLE_ATTRS: [&str; 4] = ["fscreate", "exec", "keycreate", "sockcreate"];

/// What each process wrote to its settable attributes, by pid and name.
fn attrs() -> &'static parking_lot::Mutex<std::collections::HashMap<(i32, String), Vec<u8>>> {
    static ATTRS: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<(i32, String), Vec<u8>>>> = std::sync::OnceLock::new();
    ATTRS.get_or_init(Default::default)
}

/// The pid and attribute a path names, when it is a settable attribute:
/// `/proc/<pid>/attr/<name>` or `/proc/<pid>/task/<tid>/attr/<name>`.
fn settable_attr(path: &[u8]) -> Option<(i32, &'static str)> {
    let path = std::str::from_utf8(path).ok()?;
    let rest = path.strip_prefix("/proc/")?;
    let (pid, rest) = rest.split_once('/')?;
    let pid = pid.parse().ok()?;
    let rest = match rest.strip_prefix("task/") {
        Some(task) => task.split_once('/')?.1,
        None => rest,
    };
    let name = rest.strip_prefix("attr/")?;
    SETTABLE_ATTRS.iter().find(|a| **a == name).map(|a| (pid, *a))
}

/// Whether a path is an attribute a process may write (`setfscreatecon` and its kin), or a
/// selinuxfs transaction file.
#[must_use]
pub fn is_settable_attr(path: &[u8]) -> bool {
    settable_attr(path).is_some() || path == SELINUX_CONTEXT || path == TRACE_MARKER || std::str::from_utf8(path).is_ok_and(|p| sysctl_default(p).is_some())
}

const SELINUX_CONTEXT: &[u8] = b"/sys/fs/selinux/context";

/// tracefs's marker.
const TRACE_MARKER: &[u8] = b"/sys/kernel/tracing/trace_marker";

/// The kernel tunables a process may write (`/proc/sys/...`), with their boot values: the BPF
/// loader enables the JIT and unprivileged BPF.
/// `/proc/sys/net/ipv{4,6}/{conf,neigh}` (0), or one interface's directory in it (1).
fn net_if_dir(path: &str) -> Option<u8> {
    let rest = path.strip_prefix("/proc/sys/net/ipv4/").or_else(|| path.strip_prefix("/proc/sys/net/ipv6/"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts[..] {
        ["conf" | "neigh"] => Some(0),
        ["conf" | "neigh", "all" | "default" | "lo"] => Some(1),
        _ => None,
    }
}

fn sysctl_default(path: &str) -> Option<&'static str> {
    // An interface's setting (netd writes many): any name, 0 until written.
    if let Some((dir, name)) = path.rsplit_once('/') {
        if net_if_dir(dir) == Some(1) && !name.is_empty() {
            return Some("0
");
        }
    }
    Some(match path {
        "/proc/sys/kernel/unprivileged_bpf_disabled" => "2\n",
        "/proc/sys/kernel/perf_event_paranoid" => "3\n",
        "/proc/sys/net/core/bpf_jit_enable" => "0\n",
        "/proc/sys/net/core/bpf_jit_kallsyms" => "0\n",
        // tracefs, with tracing off: what atrace and the tracing HAL set before they trace.
        "/sys/kernel/tracing/tracing_on" => "0\n",
        "/sys/kernel/tracing/current_tracer" => "nop\n",
        "/sys/kernel/tracing/buffer_size_kb" => "1408\n",
        "/sys/kernel/tracing/trace_clock" => "[local] global counter uptime perf mono mono_raw boot\n",
        "/sys/kernel/tracing/set_event" => "",
        _ => return None,
    })
}

fn sysctls() -> &'static parking_lot::Mutex<std::collections::HashMap<String, Vec<u8>>> {
    static S: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<String, Vec<u8>>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn sysctl_value(path: &str) -> Vec<u8> {
    sysctls().lock().get(path).cloned().unwrap_or_else(|| sysctl_default(path).unwrap_or_default().as_bytes().to_vec())
}

/// A write to a writable generated file: an attribute, or selinuxfs's `context` transaction,
/// whose answer (`answer`) the same descriptor then reads.
pub fn write_generated(path: &[u8], bytes: &[u8], answer: &mut Vec<u8>) -> Result<usize, crate::errno::Errno> {
    if path == SELINUX_CONTEXT {
        *answer = check_context(bytes)?;
        return Ok(bytes.len());
    }
    // tracefs's marker, with tracing off: a write is accepted and goes nowhere.
    if path == TRACE_MARKER {
        return Ok(bytes.len());
    }
    if let Some(p) = std::str::from_utf8(path).ok().filter(|p| sysctl_default(p).is_some()) {
        sysctls().lock().insert(p.to_string(), bytes.to_vec());
        return Ok(bytes.len());
    }
    write_attr(path, bytes)
}

/// `security_check_context`: with no policy loaded here (permissive), a context is valid when it
/// is well formed -- `user:role:type:level`, the level optionally with categories -- and its
/// canonical form is itself, NUL-terminated.
fn check_context(bytes: &[u8]) -> Result<Vec<u8>, crate::errno::Errno> {
    let text = bytes.split(|b| *b == 0).next().unwrap_or_default();
    let text = std::str::from_utf8(text).map_err(|_| crate::errno::EINVAL)?.trim_end_matches('\n');
    let fields: Vec<&str> = text.splitn(4, ':').collect();
    let word = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
    if fields.len() != 4 || !fields[..3].iter().all(|f| word(f)) || fields[3].is_empty() || fields[3].bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(crate::errno::EINVAL);
    }
    let mut canonical = text.as_bytes().to_vec();
    canonical.push(0);
    Ok(canonical)
}

/// A write to a settable attribute: it holds the context written (empty clears it).
pub fn write_attr(path: &[u8], bytes: &[u8]) -> Result<usize, crate::errno::Errno> {
    let (pid, name) = settable_attr(path).ok_or(crate::errno::EACCES)?;
    let value: Vec<u8> = bytes.iter().copied().take_while(|b| *b != 0 && *b != b'\n').collect();
    let mut attrs = attrs().lock();
    if value.is_empty() {
        attrs.remove(&(pid, name.to_string()));
    } else {
        let mut stored = value;
        stored.push(0);
        attrs.insert((pid, name.to_string()), stored);
    }
    Ok(bytes.len())
}

fn attr_value(pid: i32, name: &str) -> Vec<u8> {
    attrs().lock().get(&(pid, name.to_string())).cloned().unwrap_or_default()
}

fn selinux_context(p: &Process) -> Vec<u8> {
    let comm = String::from_utf8_lossy(&p.comm.lock()).into_owned();
    let domain = match comm.as_str() {
        "servicemanager" => "servicemanager",
        "system_server" => "system_server",
        "surfaceflinger" => "surfaceflinger",
        _ => "untrusted_app",
    };
    format!("u:r:{domain}:s0\0").into_bytes()
}

fn version(_p: &Process) -> Vec<u8> {
    b"Linux version 6.1.99-omnidroid (omnidroid) #1 SMP PREEMPT\n".to_vec()
}

fn cpu_range(_p: &Process) -> Vec<u8> {
    format!("0-{}\n", cpus() - 1).into_bytes()
}

impl Process {
    /// A `/dev/__properties__` file.
    fn blob(&self, path: &[u8]) -> Option<&[u8]> {
        if path == b"/apex/apex-info-list.xml" {
            return Some(&self.props.apex_info);
        }
        let name = path.strip_prefix(b"/dev/__properties__/")?;
        match name {
            b"property_info" => Some(&self.props.info),
            b"properties_serial" => Some(&self.props.serial),
            _ if name == crate::props::CONTEXT.as_bytes() => Some(&self.props.area),
            _ => None,
        }
    }

    fn entry(&self, path: &[u8]) -> Option<Entry> {
        if path == b"/dev/__properties__" {
            return Some(Entry::DynDir(vec![
                ("property_info".to_string(), DT_REG),
                ("properties_serial".to_string(), DT_REG),
                (crate::props::CONTEXT.to_string(), DT_REG),
            ]));
        }
        let pid = self.sys.pid.to_string();
        let path = std::str::from_utf8(path).ok()?;
        match path {
            "/proc" => {
                let mut entries: Vec<(String, u8)> = ["cpuinfo", "meminfo", "stat", "uptime", "loadavg", "version", "mounts"]
                    .iter()
                    .map(|n| ((*n).to_string(), DT_REG))
                    .collect();
                entries.push(("sys".to_string(), DT_DIR));
                entries.push(("self".to_string(), DT_LNK));
                entries.push(("thread-self".to_string(), DT_LNK));
                entries.push((pid, DT_DIR));
                return Some(Entry::DynDir(entries));
            }
            "/proc/self" => return Some(Entry::Link(pid.into_bytes())),
            "/proc/sys" => return Some(Entry::Dir(vec![("kernel", DT_DIR), ("net", DT_DIR)])),
            "/proc/sys/kernel" => return Some(Entry::Dir(vec![("random", DT_DIR), ("unprivileged_bpf_disabled", DT_REG), ("perf_event_paranoid", DT_REG)])),
            "/proc/sys/net" => return Some(Entry::Dir(vec![("core", DT_DIR), ("ipv4", DT_DIR), ("ipv6", DT_DIR)])),
            "/proc/sys/net/core" => return Some(Entry::Dir(vec![("bpf_jit_enable", DT_REG), ("bpf_jit_kallsyms", DT_REG)])),
            "/proc/sys/net/ipv4" | "/proc/sys/net/ipv6" => return Some(Entry::Dir(vec![("conf", DT_DIR), ("neigh", DT_DIR)])),
            // Per interface: all, default and the one there is, the loopback.
            _ if net_if_dir(path) == Some(0) => return Some(Entry::Dir(vec![("all", DT_DIR), ("default", DT_DIR), ("lo", DT_DIR)])),
            _ if net_if_dir(path) == Some(1) => {
                let prefix = format!("{path}/");
                let names: Vec<(String, u8)> = sysctls().lock().keys().filter_map(|k| k.strip_prefix(&prefix)).map(|n| (n.to_string(), DT_REG)).collect();
                return Some(Entry::DynDir(names));
            }
            // The network interfaces: the loopback.
            "/sys/class" => return Some(Entry::Dir(vec![("net", DT_DIR)])),
            "/sys/class/net" => return Some(Entry::Dir(vec![("lo", DT_DIR)])),
            "/sys/class/net/lo" => return Some(Entry::Dir(vec![("ifindex", DT_REG), ("mtu", DT_REG), ("type", DT_REG), ("flags", DT_REG), ("address", DT_REG), ("operstate", DT_REG)])),
            "/sys/class/net/lo/ifindex" => return Some(Entry::Bytes(b"1
".to_vec())),
            "/sys/class/net/lo/mtu" => return Some(Entry::Bytes(b"65536
".to_vec())),
            "/sys/class/net/lo/type" => return Some(Entry::Bytes(b"772
".to_vec())),
            "/sys/class/net/lo/flags" => return Some(Entry::Bytes(b"0x9
".to_vec())),
            "/sys/class/net/lo/address" => return Some(Entry::Bytes(b"00:00:00:00:00:00
".to_vec())),
            "/sys/class/net/lo/operstate" => return Some(Entry::Bytes(b"unknown
".to_vec())),
            _ if sysctl_default(path).is_some() => return Some(Entry::Bytes(sysctl_value(path))),
            "/proc/sys/kernel/random" => return Some(Entry::Dir(vec![("boot_id", DT_REG), ("uuid", DT_REG)])),
            "/proc/sys/kernel/random/boot_id" => return Some(Entry::File(boot_id_file)),
            "/proc/sys/kernel/random/uuid" => return Some(Entry::File(random_uuid)),
            "/proc/thread-self" => return Some(Entry::Link(format!("{pid}/task/{pid}").into_bytes())),
            "/proc/cpuinfo" => return Some(Entry::File(cpuinfo)),
            "/proc/meminfo" => return Some(Entry::File(meminfo)),
            "/proc/stat" => return Some(Entry::File(proc_stat)),
            "/proc/uptime" => return Some(Entry::File(uptime)),
            "/proc/loadavg" => return Some(Entry::File(loadavg)),
            "/proc/version" => return Some(Entry::File(version)),
            "/proc/mounts" => return Some(Entry::File(mounts)),
            "/proc/filesystems" => return Some(Entry::File(filesystems)),
            "/sys" => return Some(Entry::Dir(vec![("block", DT_DIR), ("class", DT_DIR), ("devices", DT_DIR), ("fs", DT_DIR), ("kernel", DT_DIR)])),
            // tracefs, mounted with tracing off: its settings, a marker that takes writes, and no
            // events -- a kernel built without tracepoints (atrace finds each category absent).
            "/sys/kernel" => return Some(Entry::Dir(vec![("tracing", DT_DIR)])),
            "/sys/kernel/tracing" => {
                return Some(Entry::Dir(vec![
                    ("tracing_on", DT_REG),
                    ("current_tracer", DT_REG),
                    ("buffer_size_kb", DT_REG),
                    ("trace_clock", DT_REG),
                    ("set_event", DT_REG),
                    ("trace_marker", DT_REG),
                    ("events", DT_DIR),
                    ("options", DT_DIR),
                ]))
            }
            "/sys/kernel/tracing/events" | "/sys/kernel/tracing/options" => return Some(Entry::Dir(Vec::new())),
            "/sys/kernel/tracing/trace_marker" => return Some(Entry::Bytes(Vec::new())),
            "/sys/block" => return Some(Entry::DynDir(apex_mounts(self).iter().map(|m| (format!("loop{}", m.index), DT_DIR)).collect())),
            "/sys/fs" => return Some(Entry::Dir(vec![("selinux", DT_DIR), ("bpf", DT_DIR)])),
            // The BPF filesystem (`crate::bpf`): its directories and pins.
            _ if crate::bpf::on_bpffs(path.as_bytes()) => {
                return match crate::bpf::lookup(path.as_bytes())? {
                    crate::bpf::Node::Dir => Some(Entry::DynDir(
                        crate::bpf::list(path.as_bytes()).into_iter().map(|(n, dir)| (n, if dir { DT_DIR } else { DT_REG })).collect(),
                    )),
                    crate::bpf::Node::Pin(_) => Some(Entry::Bytes(Vec::new())),
                };
            }
            // selinuxfs, permissive: libselinux finds SELinux present (servicemanager insists on
            // it), every check is allowed -- enforce 0, and deny_unknown 0 for the classes this
            // policy-less filesystem does not list.
            "/sys/fs/selinux" => {
                return Some(Entry::Dir(vec![
                    ("status", DT_REG),
                    ("enforce", DT_REG),
                    ("deny_unknown", DT_REG),
                    ("reject_unknown", DT_REG),
                    ("policyvers", DT_REG),
                    ("mls", DT_REG),
                    ("checkreqprot", DT_REG),
                    ("context", DT_REG),
                ]))
            }
            // A transaction file: a context written is checked, and the same descriptor reads
            // back its canonical form (`write_selinux_context`).
            "/sys/fs/selinux/context" => return Some(Entry::Bytes(Vec::new())),
            "/sys/fs/selinux/status" => return Some(Entry::File(selinux_status)),
            "/sys/fs/selinux/enforce" | "/sys/fs/selinux/deny_unknown" | "/sys/fs/selinux/reject_unknown" | "/sys/fs/selinux/checkreqprot" => {
                return Some(Entry::File(zero))
            }
            "/sys/fs/selinux/mls" => return Some(Entry::File(one)),
            "/sys/fs/selinux/policyvers" => return Some(Entry::File(policyvers)),
            "/sys/devices" => return Some(Entry::Dir(vec![("system", DT_DIR)])),
            "/sys/devices/system" => return Some(Entry::Dir(vec![("cpu", DT_DIR)])),
            "/sys/devices/system/cpu" => {
                let mut e: Vec<(String, u8)> = (0..cpus()).map(|n| (format!("cpu{n}"), DT_DIR)).collect();
                e.extend(["possible", "present", "online"].map(|n| (n.to_string(), DT_REG)));
                return Some(Entry::DynDir(e));
            }
            "/sys/devices/system/cpu/possible" | "/sys/devices/system/cpu/present" | "/sys/devices/system/cpu/online" => {
                return Some(Entry::File(cpu_range));
            }
            _ => {}
        }
        // A loop device of an APEX: its backing file.
        if let Some(rest) = path.strip_prefix("/sys/block/loop") {
            let (n, tail) = rest.split_once('/').unwrap_or((rest, ""));
            let m = n.parse::<usize>().ok().and_then(|n| apex_mounts(self).get(n))?;
            return Some(match tail {
                "" => Entry::Dir(vec![("loop", DT_DIR)]),
                "loop" => Entry::Dir(vec![("backing_file", DT_REG)]),
                "loop/backing_file" => Entry::Bytes(format!("{}\n", m.backing).into_bytes()),
                _ => return None,
            });
        }
        if let Some(n) = path.strip_prefix("/sys/devices/system/cpu/cpu") {
            return n.parse::<usize>().ok().filter(|n| *n < cpus()).map(|_| Entry::Dir(Vec::new()));
        }
        let rest = path.strip_prefix("/proc/")?;
        let (first, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if first != pid {
            return None; // another pid, or no such file
        }
        if let Some(task) = tail.strip_prefix("task") {
            if task.is_empty() {
                return Some(Entry::DynDir(self.tids().into_iter().map(|t| (t.to_string(), DT_DIR)).collect()));
            }
            let task = task.strip_prefix('/')?;
            let (tid, inner) = task.split_once('/').unwrap_or((task, ""));
            if !tid.parse::<i32>().is_ok_and(|t| self.tids().contains(&t)) {
                return None;
            }
            return match inner {
                "" => Some(Entry::Dir(vec![("stat", DT_REG), ("status", DT_REG), ("comm", DT_REG), ("attr", DT_DIR)])),
                "stat" | "status" | "comm" => self.per_process(inner),
                _ if inner == "attr" || inner.starts_with("attr/") => self.per_process(inner),
                _ => None,
            };
        }
        self.per_process(tail)
    }

    /// `/proc/<pid>/<tail>`.
    fn per_process(&self, tail: &str) -> Option<Entry> {
        Some(match tail {
            "" => Entry::Dir(vec![
                ("maps", DT_REG),
                ("stat", DT_REG),
                ("status", DT_REG),
                ("statm", DT_REG),
                ("cmdline", DT_REG),
                ("comm", DT_REG),
                ("exe", DT_LNK),
                ("cwd", DT_LNK),
                ("fd", DT_DIR),
                ("task", DT_DIR),
                ("limits", DT_REG),
                ("mounts", DT_REG),
                ("attr", DT_DIR),
            ]),
            "attr" => Entry::Dir(vec![
                ("current", DT_REG),
                ("prev", DT_REG),
                ("fscreate", DT_REG),
                ("exec", DT_REG),
                ("keycreate", DT_REG),
                ("sockcreate", DT_REG),
            ]),
            "attr/current" | "attr/prev" => Entry::File(selinux_context),
            _ if tail.strip_prefix("attr/").is_some_and(|a| SETTABLE_ATTRS.contains(&a)) => {
                Entry::Bytes(attr_value(self.sys.pid, &tail["attr/".len()..]))
            }
            "maps" => Entry::File(maps),
            "stat" => Entry::File(stat),
            "status" => Entry::File(status),
            "statm" => Entry::File(statm),
            "cmdline" => Entry::File(cmdline),
            "comm" => Entry::File(comm),
            "limits" => Entry::File(limits),
            "mounts" => Entry::File(mounts),
            "exe" => Entry::Link(self.vfs.exe().to_vec()),
            "cwd" => Entry::Link(self.cwd.lock().clone()),
            "fd" => Entry::DynDir(self.fds.list().into_iter().map(|(fd, _)| (fd.to_string(), DT_LNK)).collect()),
            _ => {
                let n = tail.strip_prefix("fd/")?;
                let file = self.fds.get(n.parse().ok()?).ok()?;
                Entry::Link(crate::fd::guest_path_of(&file))
            }
        })
    }
}

impl ProcFs for Process {
    fn node(&self, path: &[u8]) -> Option<Node> {
        if let Some(blob) = self.blob(path) {
            return Some(Node::Blob { size: blob.len() as u64 });
        }
        Some(match self.entry(path)? {
            Entry::Dir(_) | Entry::DynDir(_) => Node::Dir,
            Entry::File(_) | Entry::Bytes(_) => Node::Generated,
            Entry::Link(target) => Node::Symlink { target },
        })
    }

    fn list(&self, path: &[u8]) -> Vec<DirEnt> {
        let named = |name: String, kind: u8| {
            let mut full = path.to_vec();
            full.push(b'/');
            full.extend_from_slice(name.as_bytes());
            DirEnt { ino: ino_of(&full), name: name.into_bytes(), kind }
        };
        match self.entry(path) {
            Some(Entry::Dir(e)) => e.into_iter().map(|(n, k)| named(n.to_string(), k)).collect(),
            Some(Entry::DynDir(e)) => e.into_iter().map(|(n, k)| named(n, k)).collect(),
            _ => Vec::new(),
        }
    }

    fn read(&self, path: &[u8]) -> Option<Vec<u8>> {
        if let Some(blob) = self.blob(path) {
            return Some(blob.to_vec());
        }
        match self.entry(path)? {
            Entry::File(generate) => Some(generate(self)),
            Entry::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}
