//! `/dev/socket/zygote`, answered below the framework (docs/superpowers/specs/2026-09-27-c5-app-
//! launch-design.md). ActivityManager starts every app process through the zygote's socket
//! protocol and waits for the new pid. The zygote's fork cannot be made here (an ART child would
//! share its parent's address space, and the fork design keeps a parent waiting until its child
//! executes a program), so every start is answered the way the zygote answers one with an
//! invoke-with wrapper: `WrapperInit.execApplication`'s command -- the image's `app_process64
//! /system/bin --application --nice-name=<name> com.android.internal.os.WrapperInit 0 <sdk>
//! android.app.ActivityThread seq=<n>` -- run in a host process of its own, whose binder is the
//! system's (`crate::remote`), under the uid and pid the reply gives, and the reply says a wrapper
//! is used (ActivityManager then waits for the app as long as for a wrapped one). WrapperInit
//! preloads as the zygote does; everything the app then does (`attachApplication`,
//! `bindApplication`, its Activity) is the framework's own.
use std::path::PathBuf;
use std::sync::Arc;

use crate::socket::Socket;

/// What a launch needs: this host process's runner, the image, the instance, the environment,
/// and the system's binder address.
pub struct Launcher {
    pub runner: PathBuf,
    pub sysroot: PathBuf,
    pub instance: PathBuf,
    pub envp: Vec<Vec<u8>>,
    pub binder: String,
    /// The zygote's VM options (`-X...`), which every app it forks runs with: the ones the
    /// system's `app_process` was started with, less what the zygote sets per app.
    pub vm_options: Vec<String>,
}

/// Bind `/dev/socket/zygote` in `instance` (the key a process's sockets are named in) and answer
/// it on host threads.
pub fn serve(instance: usize, launcher: Launcher) {
    let bound = crate::unix::Bound::bind_replacing(instance, b"/dev/socket/zygote", 1, [0, 0, 0]);
    bound.listening.store(true, std::sync::atomic::Ordering::SeqCst);
    let launcher = Arc::new(launcher);
    let _ = std::thread::Builder::new().name("zygote".into()).spawn(move || loop {
        match bound.accept() {
            Some(socket) => {
                let launcher = Arc::clone(&launcher);
                let _ = std::thread::Builder::new().name("zygote-conn".into()).spawn(move || connection(socket, &launcher));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    });
}

/// Read one line; `None` when the peer closed.
fn line(socket: &mut Socket, pending: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(at) = pending.iter().position(|b| *b == b'\n') {
            let l = String::from_utf8_lossy(&pending[..at]).into_owned();
            pending.drain(..=at);
            return Some(l);
        }
        let mut buf = [0u8; 4096];
        match crate::socket::receive(socket, &mut buf) {
            Ok(0) => return None,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}

fn connection(mut socket: Socket, launcher: &Launcher) {
    let mut pending = Vec::new();
    loop {
        let Some(count) = line(&mut socket, &mut pending) else { return };
        let Ok(n) = count.trim().parse::<usize>() else { return };
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            let Some(a) = line(&mut socket, &mut pending) else { return };
            args.push(a);
        }
        let reply = answer(&args, launcher);
        if let Some(reply) = reply {
            if crate::socket::send(&mut socket, &reply).is_err() {
                return;
            }
        }
    }
}

fn int(v: i32) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

fn string(s: &str) -> Vec<u8> {
    let mut out = int(s.len() as i32);
    out.extend_from_slice(s.as_bytes());
    out
}

/// The zygote's answer to one command (`None`: it answers nothing).
fn answer(args: &[String], launcher: &Launcher) -> Option<Vec<u8>> {
    let first = args.first().map(String::as_str).unwrap_or_default();
    if first.starts_with("--usap-pool-enabled") {
        return None;
    }
    if first == "--query-abi-list" {
        return Some(string("arm64-v8a"));
    }
    if first == "--get-pid" {
        return Some(string(&std::process::id().to_string()));
    }
    // A start: options, then the class and its arguments.
    let mut uid = None;
    let mut nice = None;
    let mut sdk = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--invoke-with" {
            it.next();
            continue;
        }
        if let Some(v) = a.strip_prefix("--setuid=") {
            uid = v.parse::<u32>().ok();
        } else if let Some(v) = a.strip_prefix("--nice-name=") {
            nice = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--target-sdk-version=") {
            sdk = v.parse::<u32>().ok();
        } else if !a.starts_with("--") {
            rest.push(a.clone());
            rest.extend(it.cloned());
            break;
        }
    }
    if rest.is_empty() {
        // A setting (`--set-api-denylist-exemptions`, `--boot-completed`, ...): accepted.
        return Some(int(0));
    }
    let pid = launch(launcher, uid.unwrap_or(10000), nice.as_deref(), sdk.unwrap_or(0), &rest).unwrap_or(-1);
    let mut reply = int(pid);
    reply.push(1); // usingWrapper: the process is WrapperInit's
    Some(reply)
}

/// Launch `class args...` in a host process of its own; the pid it runs under.
fn launch(launcher: &Launcher, uid: u32, nice: Option<&str>, sdk: u32, class_and_args: &[String]) -> Option<i32> {
    let pid = crate::process::reserve_pid();
    let mut cmd = std::process::Command::new(&launcher.runner);
    cmd.arg("--sysroot").arg(&launcher.sysroot).arg("--instance").arg(&launcher.instance);
    cmd.args(["--binder-server", &launcher.binder, "--pid", &pid.to_string(), "--uid", &uid.to_string()]);
    for e in &launcher.envp {
        // The system server's class path is not an app's.
        if !e.starts_with(b"CLASSPATH=") {
            cmd.arg("--env").arg(String::from_utf8_lossy(e).into_owned());
        }
    }
    cmd.args(["--", "/system/bin/app_process64"]);
    cmd.args(&launcher.vm_options);
    cmd.args(["/system/bin", "--application"]);
    if let Some(n) = nice {
        cmd.arg(format!("--nice-name={n}"));
    }
    // WrapperInit <pipe fd> <target sdk>: no pipe (the pid is the one this reply gives).
    cmd.args(["com.android.internal.os.WrapperInit", "0", &sdk.to_string()]);
    cmd.args(class_and_args);
    eprintln!("[zygote] launching {} as pid {pid} uid {uid}: {}", nice.unwrap_or("?"), class_and_args.join(" "));
    match cmd.spawn() {
        Ok(child) => {
            // Reaped by a thread of its own; the app's end reaches the system through binder.
            std::thread::spawn(move || {
                let mut child = child;
                let status = child.wait();
                eprintln!("[zygote] pid {pid} ended: {status:?}");
            });
            Some(pid)
        }
        Err(e) => {
            eprintln!("[zygote] launch: {e}");
            None
        }
    }
}
