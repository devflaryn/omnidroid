//! `omni-linux-run --sysroot <dir> [--instance <dir>] [--env KEY=VALUE]... [--service <program>]... -- <program> [args...]`
//!
//! A `--service` is started first, as `system` (uid 1000), in this host process -- a daemon the
//! program talks to over binder, as `servicemanager`.
use std::path::PathBuf;
use std::process::ExitCode;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut sysroot = PathBuf::from("sysroot/aosp-35");
    let mut instance = std::env::temp_dir().join("omni-linux-run");
    let mut argv = Vec::new();
    let mut services: Vec<String> = Vec::new();
    let mut uid: u32 = 10_000;
    let mut init_classes: Vec<String> = Vec::new();
    let mut envp = vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sysroot" => sysroot = PathBuf::from(args.next().expect("--sysroot needs a value")),
            "--instance" => instance = PathBuf::from(args.next().expect("--instance needs a value")),
            "--env" => envp.push(args.next().expect("--env needs KEY=VALUE").into_bytes()),
            "--service" => services.push(args.next().expect("--service needs a program")),
            "--uid" => uid = args.next().and_then(|u| u.parse().ok()).expect("--uid needs a number"),
            // init: read the image's services, and class_start these classes (comma-separated).
            "--init" => init_classes = args.next().expect("--init needs classes").split(',').map(String::from).collect(),
            "--" => {
                argv.extend(args.by_ref().map(String::into_bytes));
            }
            other => {
                eprintln!("unknown argument {other:?}; usage: omni-linux-run --sysroot <dir> -- <program> [args...]");
                return ExitCode::from(2);
            }
        }
    }
    let config = SpawnConfig {
        sysroot: sysroot.clone(),
        instance_dir: instance.clone(),
        argv,
        envp: envp.clone(),
        stdout: Output::Host,
        stderr: Output::Host,
        trace: std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
    };
    let p = match Process::spawn_as(config, uid) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("omni-linux-run: {e}");
            return ExitCode::from(127);
        }
    };
    // The daemons after the program: the program reserves its address space first, where ART
    // needs it (below 4 GiB); a native daemon lives anywhere.
    let mut daemons = Vec::new();
    for service in &services {
        let config = SpawnConfig {
            sysroot: sysroot.clone(),
            instance_dir: instance.clone(),
            argv: service.split(' ').map(|a| a.as_bytes().to_vec()).collect(),
            envp: envp.clone(),
            stdout: Output::Host,
            stderr: Output::Host,
            trace: std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
        };
        match Process::spawn_as(config, 1000) {
            Ok(d) => {
                let run = std::sync::Arc::clone(&d);
                std::thread::spawn(move || run.run());
                daemons.push(d);
            }
            Err(e) => {
                eprintln!("omni-linux-run: {service}: {e}");
                return ExitCode::from(127);
            }
        }
    }
    if !init_classes.is_empty() {
        match omni_linux::init::Init::start(sysroot.clone(), instance.clone(), envp.clone()) {
            Ok(init) => {
                let classes: Vec<&str> = init_classes.iter().map(String::as_str).collect();
                let started = init.class_start(&classes);
                eprintln!("[init] started {} services: {}", started.len(), started.join(" "));
            }
            Err(e) => eprintln!("[init] {e}"),
        }
    }
    if !daemons.is_empty() || !init_classes.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    let status = p.run();
    // OMNI_VERIFY_MAPS=1 -- every read-only file mapping still holds the file's bytes.
    if std::env::var("OMNI_VERIFY_MAPS").as_deref() == Ok("1") {
        let (mut checked, mut bad) = (0usize, 0usize);
        for (start, len, guest, offset) in p.mm.file_mappings() {
            let Some(region) = p.mem.space().region_at(start as usize) else { continue };
            if !matches!(region.protection, omni_mem::Protection::Read | omni_mem::Protection::ReadExecute) {
                continue;
            }
            let Some(file) = p.vfs.sysroot().read(&guest) else { continue };
            let end = (offset as usize + len as usize).min(file.len());
            if offset as usize >= end {
                continue;
            }
            let want = &file[offset as usize..end];
            let Ok(got) = p.mem.read(start, want.len()) else { continue };
            checked += 1;
            if let Some(i) = got.iter().zip(want).position(|(a, b)| a != b) {
                bad += 1;
                eprintln!("[verify] {start:#x}+{i:#x} {}+{:#x}: guest {:02x?} file {:02x?}", String::from_utf8_lossy(&guest), offset as usize + i, &got[i..(i + 8).min(got.len())], &want[i..(i + 8).min(want.len())]);
            }
        }
        eprintln!("[verify] {checked} read-only file mappings, {bad} differ");
    }
    // OMNI_DUMP=0xADDR:0xLEN:path -- guest memory as it was when the process ended, for a post-mortem.
    if let Ok(spec) = std::env::var("OMNI_DUMP") {
        let parts: Vec<&str> = spec.splitn(3, ':').collect();
        let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
        if let [addr, len, path] = parts[..] {
            if let (Some(addr), Some(len)) = (hex(addr), hex(len)) {
                let mut out = Vec::with_capacity(len as usize);
                let page = 4096u64;
                let mut at = addr;
                while at < addr + len {
                    let n = (page - at % page).min(addr + len - at);
                    out.extend(p.mem.read(at, n as usize).unwrap_or_else(|_| vec![0xEE; n as usize]));
                    at += n;
                }
                let _ = std::fs::write(path, out);
            }
        }
    }
    eprint!("{}", p.report());
    match status {
        ExitStatus::Exited(code) => ExitCode::from(code as u8),
        ExitStatus::Killed { signal, .. } => ExitCode::from(128 + signal as u8),
    }
}
