//! `/proc` and `/sys` generated from the process (milestone A2).
use std::sync::Arc;

use omni_linux::errno::ENOENT;
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

fn process() -> (Arc<Process>, omni_linux::Task, u64) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("omni-linux-procfs-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("objects/cc")).unwrap();
    std::fs::write(dir.join("objects/cc/cc01"), vec![7u8; 3 * 4096]).unwrap();
    let m = manifest::parse("d\t755\t/\nd\t755\t/system\nd\t755\t/system/lib64\nf\t644\t12288\tcc01\t/system/lib64/libx.so\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/system/bin/test".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

/// Read a whole file through the syscalls, as a guest would.
fn cat(p: &Process, t: &mut omni_linux::Task, s: u64, path: &str) -> Result<Vec<u8>, i64> {
    p.mem.write(s, format!("{path}\0").as_bytes()).unwrap();
    let fd = p.syscall(t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]) as i64;
    if fd < 0 {
        return Err(fd);
    }
    let mut out = Vec::new();
    loop {
        let n = p.syscall(t, nr::READ, [fd as u64, s + 4096, 1000, 0, 0, 0]) as i64;
        assert!(n >= 0, "read {path}: {n}");
        if n == 0 {
            break;
        }
        out.extend(p.mem.read(s + 4096, n as usize).unwrap());
    }
    p.syscall(t, nr::CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    Ok(out)
}

fn readlink(p: &Process, t: &mut omni_linux::Task, s: u64, path: &str) -> Vec<u8> {
    p.mem.write(s, format!("{path}\0").as_bytes()).unwrap();
    let n = p.syscall(t, nr::READLINKAT, [(-100i64) as u64, s, s + 2048, 512, 0, 0]) as i64;
    assert!(n >= 0, "readlink {path}: {n}");
    p.mem.read(s + 2048, n as usize).unwrap()
}

#[test]
fn proc_self_is_a_link_to_the_pid_and_reaches_the_same_files() {
    let (p, mut t, s) = process();
    assert_eq!(readlink(&p, &mut t, s, "/proc/self"), b"1000");
    assert_eq!(cat(&p, &mut t, s, "/proc/self/cmdline"), cat(&p, &mut t, s, "/proc/1000/cmdline"));
    assert_eq!(cat(&p, &mut t, s, "/proc/self/cmdline").unwrap(), b"/system/bin/test\0");
    assert_eq!(readlink(&p, &mut t, s, "/proc/self/exe"), b"/system/bin/test");
}

#[test]
fn another_pid_is_not_ours() {
    let (p, mut t, s) = process();
    assert_eq!(cat(&p, &mut t, s, "/proc/1/stat"), Err(-(ENOENT.0 as i64)));
}

#[test]
fn maps_names_each_file_mapping_with_its_own_offset() {
    let (p, mut t, s) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    let at = p.syscall(&mut t, nr::MMAP, [0, 2 * 4096, 1, 2, fd, 4096]);
    let maps = String::from_utf8(cat(&p, &mut t, s, "/proc/self/maps").unwrap()).unwrap();
    let line = maps.lines().find(|l| l.starts_with(&format!("{at:08x}-"))).unwrap_or_else(|| panic!("{maps}"));
    let fields: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(fields[1], "r--p", "{line}");
    assert_eq!(fields[2], "00001000", "{line}");
    assert_eq!(fields[5], "/system/lib64/libx.so", "{line}");
    assert!(maps.lines().any(|l| l.split_whitespace().count() == 5), "anonymous regions have no path:\n{maps}");
}

#[test]
fn stat_has_linuxs_52_fields_and_status_names_the_process() {
    let (p, mut t, s) = process();
    let stat = String::from_utf8(cat(&p, &mut t, s, "/proc/self/stat").unwrap()).unwrap();
    assert!(stat.starts_with("1000 (test) "), "{stat}");
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    assert_eq!(fields.len(), 50, "52 fields in all: {stat}");
    assert_eq!(fields[0], "R");
    let status = String::from_utf8(cat(&p, &mut t, s, "/proc/self/status").unwrap()).unwrap();
    assert!(status.contains("Name:\ttest\n") && status.contains("Pid:\t1000\n") && status.contains("Threads:\t1\n"), "{status}");
    assert_eq!(cat(&p, &mut t, s, "/proc/self/comm").unwrap(), b"test\n");
}

#[test]
fn cpuinfo_and_sys_describe_the_same_cpus_and_features() {
    let (p, mut t, s) = process();
    let cpuinfo = String::from_utf8(cat(&p, &mut t, s, "/proc/cpuinfo").unwrap()).unwrap();
    let features = cpuinfo.lines().find(|l| l.starts_with("Features")).expect("a Features line");
    assert!(features.contains(" asimd") && !features.contains("atomics"), "{features}");
    let cpus = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
    let possible = String::from_utf8(cat(&p, &mut t, s, "/sys/devices/system/cpu/possible").unwrap()).unwrap();
    assert_eq!(possible, format!("0-{}\n", cpus - 1));
    assert!(String::from_utf8(cat(&p, &mut t, s, "/proc/meminfo").unwrap()).unwrap().starts_with("MemTotal:"));
}

#[test]
fn proc_lists_itself_and_the_fd_directory_links_to_open_files() {
    let (p, mut t, s) = process();
    p.mem.write(s, b"/system/lib64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    assert_eq!(readlink(&p, &mut t, s, &format!("/proc/self/fd/{fd}")), b"/system/lib64/libx.so");
    p.mem.write(s, b"/proc\0").unwrap();
    let dir = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0o40000, 0, 0, 0]);
    let n = p.syscall(&mut t, nr::GETDENTS64, [dir, s + 4096, 4096, 0, 0, 0]) as usize;
    let buf = p.mem.read(s + 4096, n).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("1000") && text.contains("self") && text.contains("cpuinfo"), "{text}");
}
