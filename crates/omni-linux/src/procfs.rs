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
}

/// What a path under `/proc` or `/sys` names.
enum Entry {
    Dir(Vec<(&'static str, u8)>),
    DynDir(Vec<(String, u8)>),
    File(fn(&Process) -> Vec<u8>),
    Link(Vec<u8>),
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
        1,                 // num_threads
        0,                 // itrealvalue
        0,                 // starttime
        mapped_bytes(p),   // vsize
        committed_pages(p),// rss
        u64::MAX,          // rsslim
        0, 0, 0, 0, 0,     // startcode endcode startstack kstkesp kstkeip
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
    let uid = p.sys.uid;
    let name = String::from_utf8_lossy(&p.comm.lock()).into_owned();
    let mut out = String::new();
    let _ = write!(out, "Name:\t{name}\nUmask:\t{:04o}\nState:\tR (running)\n", p.sys.umask());
    let _ = write!(out, "Tgid:\t{pid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t1\nTracerPid:\t0\n");
    let _ = write!(out, "Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{uid}\t{uid}\t{uid}\t{uid}\n");
    let _ = write!(out, "FDSize:\t64\nGroups:\t\nVmSize:\t{} kB\nVmRSS:\t{} kB\n", mapped_bytes(p) / 1024, committed_pages(p) * 4);
    let _ = write!(out, "Threads:\t1\nSigQ:\t0/0\nSigPnd:\t0000000000000000\nCpus_allowed_list:\t0-{}\n", cpus() - 1);
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

fn mounts(_p: &Process) -> Vec<u8> {
    let lines = [
        "/dev/root / ext4 ro 0 0",
        "proc /proc proc rw 0 0",
        "sysfs /sys sysfs rw 0 0",
        "/dev/root /system ext4 ro 0 0",
        "/dev/root /apex ext4 ro 0 0",
        "/dev/data /data ext4 rw 0 0",
        "tmpfs /tmp tmpfs rw 0 0",
    ];
    lines.iter().flat_map(|l| l.bytes().chain(std::iter::once(b'\n'))).collect()
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
    let boot = now.saturating_sub(p.sys.uptime().as_secs() + 1000);
    let mut out = format!("cpu  {t} 0 0 {t} 0 0 0 0 0 0\n");
    for n in 0..cpus() {
        let _ = writeln!(out, "cpu{n} {t} 0 0 {t} 0 0 0 0 0 0");
    }
    let _ = write!(out, "intr 0\nctxt 0\nbtime {boot}\nprocesses 1\nprocs_running 1\nprocs_blocked 0\n");
    out.into_bytes()
}

fn uptime(p: &Process) -> Vec<u8> {
    let up = p.sys.uptime().as_secs_f64() + 1000.0;
    format!("{up:.2} {:.2}\n", up * cpus() as f64).into_bytes()
}

fn loadavg(p: &Process) -> Vec<u8> {
    format!("0.00 0.00 0.00 1/1 {}\n", p.sys.pid).into_bytes()
}

fn version(_p: &Process) -> Vec<u8> {
    b"Linux version 6.1.0-omnidroid (omnidroid) #1 SMP PREEMPT\n".to_vec()
}

fn cpu_range(_p: &Process) -> Vec<u8> {
    format!("0-{}\n", cpus() - 1).into_bytes()
}

impl Process {
    /// A `/dev/__properties__` file.
    fn blob(&self, path: &[u8]) -> Option<&[u8]> {
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
                entries.push(("self".to_string(), DT_LNK));
                entries.push(("thread-self".to_string(), DT_LNK));
                entries.push((pid, DT_DIR));
                return Some(Entry::DynDir(entries));
            }
            "/proc/self" => return Some(Entry::Link(pid.into_bytes())),
            "/proc/thread-self" => return Some(Entry::Link(format!("{pid}/task/{pid}").into_bytes())),
            "/proc/cpuinfo" => return Some(Entry::File(cpuinfo)),
            "/proc/meminfo" => return Some(Entry::File(meminfo)),
            "/proc/stat" => return Some(Entry::File(proc_stat)),
            "/proc/uptime" => return Some(Entry::File(uptime)),
            "/proc/loadavg" => return Some(Entry::File(loadavg)),
            "/proc/version" => return Some(Entry::File(version)),
            "/proc/mounts" => return Some(Entry::File(mounts)),
            "/sys" => return Some(Entry::Dir(vec![("devices", DT_DIR)])),
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
                return Some(Entry::DynDir(vec![(pid, DT_DIR)]));
            }
            let task = task.strip_prefix('/')?;
            let (tid, inner) = task.split_once('/').unwrap_or((task, ""));
            if tid != pid {
                return None;
            }
            return match inner {
                "" => Some(Entry::Dir(vec![("stat", DT_REG), ("status", DT_REG), ("comm", DT_REG)])),
                "stat" | "status" | "comm" => self.per_process(inner),
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
            ]),
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
            Entry::File(_) => Node::Generated,
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
            _ => None,
        }
    }
}
