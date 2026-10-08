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
    let launcher = Arc::new(launcher);
    let _ = LAUNCHER.set(Arc::clone(&launcher));
    serve_at(instance, b"/dev/socket/zygote", launcher);
}

/// The launcher `serve` was given: what a spare app process is started with.
static LAUNCHER: std::sync::OnceLock<Arc<Launcher>> = std::sync::OnceLock::new();

/// Answer the zygote protocol on socket `name` in `instance`.
fn serve_at(instance: usize, name: &[u8], launcher: Arc<Launcher>) {
    let bound = crate::unix::Bound::bind_replacing(instance, name, 1, [0, 0, 0]);
    bound.listening.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::thread::Builder::new().name("zygote".into()).spawn(move || loop {
        match bound.accept() {
            Some(socket) => {
                let launcher = Arc::clone(&launcher);
                let _ = std::thread::Builder::new().name("zygote-conn".into()).spawn(move || connection(socket, &launcher, instance));
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

fn connection(mut socket: Socket, launcher: &Arc<Launcher>, instance: usize) {
    let mut pending = Vec::new();
    loop {
        let Some(count) = line(&mut socket, &mut pending) else { return };
        let Ok(n) = count.trim().parse::<usize>() else { return };
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            let Some(a) = line(&mut socket, &mut pending) else { return };
            args.push(a);
        }
        let reply = answer(&args, launcher, instance);
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
fn answer(args: &[String], launcher: &Arc<Launcher>, instance: usize) -> Option<Vec<u8>> {
    let first = args.first().map(String::as_str).unwrap_or_default();
    if first.starts_with("--usap-pool-enabled") {
        return None;
    }
    // A child zygote's preload (`ZygoteProcess.preloadPackageForAbi`, `preloadApp`): the paths
    // after it are not a class to start. Nothing is preloaded -- each process this answers starts
    // in a host process of its own and loads what it uses -- and 0 is the zygote's success.
    if matches!(first, "--preload-package" | "--preload-app" | "--preload-default") {
        return Some(int(0));
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
    let mut package = String::new();
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
        } else if let Some(v) = a.strip_prefix("--package-name=") {
            package = v.to_string();
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
    // A child zygote (`--start-child-zygote`: the WebView's `WebViewZygoteInit`, an app's
    // `AppZygoteInit`), which forks the processes asked of it on `--zygote-socket=<name>`. An ART
    // started here is not a zygote (no `-Xzygote`: its first fork aborts, "runtime instance not
    // started with -Xzygote", and every WebView renderer failed to start), so this answers that
    // socket itself, as it answers its own: each process asked of it starts in a host process of
    // its own. The pid given is the child zygote's; nothing runs under it.
    if let Some(socket) = child_zygote_socket(args) {
        let pid = crate::process::reserve_pid();
        let name = format!("@{socket}");
        serve_at(instance, name.as_bytes(), Arc::clone(launcher));
        eprintln!("[zygote] child zygote {} as pid {pid}: answered here, on {name}", nice.as_deref().unwrap_or("?"));
        let mut reply = int(pid);
        reply.push(0);
        return Some(reply);
    }
    // A spare is for an installed app (the agent's), not the image's own: those start around an
    // install or when the placeholder home comes back, and took the spare just before the app
    // (2026-10-01: Settings' FallbackHome, the package installer, the WebView's service). An
    // installed app is in the device's /data/app (`--package-name`), the image's are not.
    let uid = uid.unwrap_or(10000);
    // (Without `--package-name`, an app's main process is named after its package.)
    let package = if package.is_empty() { nice.clone().unwrap_or_default() } else { package };
    let installed = uid >= 10_000 && installed_app(&launcher.instance, &package);
    let pid = launch(launcher, uid, nice.as_deref(), sdk.unwrap_or(0), &rest, installed).unwrap_or(-1);
    let mut reply = int(pid);
    reply.push(1); // usingWrapper: the process is WrapperInit's
    Some(reply)
}

/// The socket a child zygote is asked to listen on, when `args` start one.
fn child_zygote_socket(args: &[String]) -> Option<&str> {
    if !args.iter().any(|a| a == "--start-child-zygote") {
        return None;
    }
    args.iter().find_map(|a| a.strip_prefix("--zygote-socket="))
}

/// The app processes launched here and still running, by the pid the system gave each.
static CHILDREN: parking_lot::Mutex<std::collections::BTreeMap<i32, Arc<parking_lot::Mutex<std::process::Child>>>> =
    parking_lot::Mutex::new(std::collections::BTreeMap::new());

/// A signal to an app process launched here, which lives in a host process of its own: `None` if
/// `pid` is not one. SIGKILL and SIGTERM (whose default action ends the process) end its host
/// process -- ActivityManager's `kill` of an app it stops (`am force-stop`, a restart), which
/// otherwise "refused to die"; signal 0 answers that it lives; any other is accepted and not
/// delivered (SIGQUIT's ANR traces, SIGUSR1's heap profile), which the app's host process would
/// need to be told of.
pub fn signal(pid: i32, sig: i32) -> Option<()> {
    let child = CHILDREN.lock().get(&pid).cloned()?;
    if matches!(sig, 9 | 15) {
        eprintln!("[zygote] pid {pid}: signal {sig}, its host process ended");
        let _ = child.lock().kill();
    }
    Some(())
}

/// Whether `package` is installed in the device's /data/app (`<random>/<package>-<random>/base.apk`).
fn installed_app(instance: &std::path::Path, package: &str) -> bool {
    if package.is_empty() {
        return false;
    }
    let prefix = format!("{package}-");
    std::fs::read_dir(instance.join("data/app")).into_iter().flatten().flatten().any(|outer| {
        std::fs::read_dir(outer.path())
            .into_iter()
            .flatten()
            .flatten()
            .any(|inner| inner.file_name().to_string_lossy().starts_with(&prefix) && inner.path().join("base.apk").is_file())
    })
}

/// Launch `class args...` in a host process of its own; the pid it runs under. With
/// `OMNI_APP_SPARE=1` and `spare` (an installed app) a spare app process waiting for an app
/// (`start_spare`) becomes it instead.
fn launch(launcher: &Launcher, uid: u32, nice: Option<&str>, sdk: u32, class_and_args: &[String], spare: bool) -> Option<i32> {
    if let Some(pid) = spare.then(|| take_spare(uid, nice, sdk, class_and_args)).flatten() {
        return Some(pid);
    }
    let pid = crate::process::reserve_pid();
    let mut cmd = host_command(launcher, pid, uid, nice, &[]);
    // WrapperInit <pipe fd> <target sdk>: no pipe (the pid is the one this reply gives).
    cmd.args(["com.android.internal.os.WrapperInit", "0", &sdk.to_string()]);
    cmd.args(class_and_args);
    eprintln!("[zygote] launching {} as pid {pid} uid {uid}: {}", nice.unwrap_or("?"), class_and_args.join(" "));
    spawn(cmd, pid, uid)
}

/// An app host process's command, up to the class app_process runs: the runner, the instance,
/// the system's binder, the pid and uid, the environment, the VM options and the per-app switches.
fn host_command(launcher: &Launcher, pid: i32, uid: u32, nice: Option<&str>, env: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new(&launcher.runner);
    cmd.arg("--sysroot").arg(&launcher.sysroot).arg("--instance").arg(&launcher.instance);
    cmd.args(["--binder-server", &launcher.binder, "--binder-credential-stdin", "--pid", &pid.to_string(), "--uid", &uid.to_string()]);
    cmd.stdin(std::process::Stdio::piped());
    for e in &launcher.envp {
        // The system server's class path is not an app's.
        if !e.starts_with(b"CLASSPATH=") {
            cmd.arg("--env").arg(String::from_utf8_lossy(e).into_owned());
        }
    }
    for e in env {
        cmd.arg("--env").arg(e);
    }
    // One CLOCK_MONOTONIC for the instance: SurfaceFlinger's vsync times are the app's frame times.
    cmd.env("OMNI_MONOTONIC_ORIGIN", crate::sys::monotonic_origin());
    cmd.args(["--", "/system/bin/app_process64"]);
    cmd.args(&launcher.vm_options);
    cmd.args(["/system/bin", "--application"]);
    if let Some(n) = nice {
        cmd.arg(format!("--nice-name={n}"));
    }
    // OMNI_TRACE_APP=<process name>: that app's host process traces its system calls.
    if nice.is_some() && std::env::var("OMNI_TRACE_APP").ok().as_deref() == nice {
        cmd.env("OMNI_SYSCALL_TRACE", "1");
    }
    // OMNI_SIGNAL_TRACE_APP=<process name>: that app's host process traces every signal it
    // delivers, with registers (`OMNI_SIGNAL_TRACE`). Only that process: traced system-wide, every
    // ART implicit null check in the system prints thirty lines, a boot slower than it need be --
    // and system_server's 60 s Watchdog has little margin on a busy host (2026-09-29: five boots
    // killed at ~220 s on a Mac at load 25, with and without the trace).
    if nice.is_some() && std::env::var("OMNI_SIGNAL_TRACE_APP").ok().as_deref() == nice {
        cmd.env("OMNI_SIGNAL_TRACE", "1");
    }
    // An app's host process: its CPU options are an app's (`Process::cpu_options`).
    cmd.env("OMNI_LINUX_APP", "1");
    // Per-app CPU switches, `<process name>:<value>`, that process only -- the system keeps its own,
    // so the boot is not slowed into system_server's Watchdog (nor its other apps changed):
    //   OMNI_DYNARMIC_OPT_APP=<name>:<hex mask>       only these JIT optimizations (`OMNI_DYNARMIC_OPT`)
    //   OMNI_JIT_EXCLUSIVE_MONITOR_APP=<name>:global  the exclusive monitor (`OMNI_JIT_EXCLUSIVE_MONITOR`)
    //   OMNI_JIT_FORCE_ORDERED_APP=<name>:1           every guest access fenced (a dynarmic probe)
    //   OMNI_JIT_CODE_CACHE_MB_APP=<name>:<MiB>       each thread's code cache (`OMNI_JIT_CODE_CACHE_MB`)
    if let Some(n) = nice {
        for (knob, var) in [
            ("OMNI_DYNARMIC_OPT_APP", "OMNI_DYNARMIC_OPT"),
            ("OMNI_JIT_EXCLUSIVE_MONITOR_APP", "OMNI_JIT_EXCLUSIVE_MONITOR"),
            ("OMNI_JIT_FORCE_ORDERED_APP", "OMNI_JIT_FORCE_ORDERED"),
            ("OMNI_JIT_CODE_CACHE_MB_APP", "OMNI_JIT_CODE_CACHE_MB"),
        ] {
            if let Some(value) = std::env::var(knob).ok().and_then(|v| v.strip_prefix(&format!("{n}:")).map(str::to_string)) {
                cmd.env(var, value);
            }
        }
    }
    // OMNI_THREAD_DUMP_APP=<process name>: that app's host process lists its threads waiting in a
    // system call every 15 s (`OMNI_THREAD_DUMP`).
    if nice.is_some() && std::env::var("OMNI_THREAD_DUMP_APP").ok().as_deref() == nice {
        cmd.env("OMNI_THREAD_DUMP", "15");
    }
    // OMNI_THREAD_CPU_APP=<process name>: that app's host process reports its threads' processor
    // time and where it went every 10 s (`OMNI_THREAD_CPU`); with `OMNI_GUEST_PROF=1` (inherited)
    // also which guest functions its translated-code samples ran (`[guestprof]`, crate::guestprof).
    if nice.is_some() && std::env::var("OMNI_THREAD_CPU_APP").ok().as_deref() == nice {
        cmd.env("OMNI_THREAD_CPU", "10");
    }
    // OMNI_SLOW_APP=<process name>: that app's host process logs each system call taking
    // `OMNI_SLOW_APP_MS` (default 200) ms or more (`[slow]`).
    if nice.is_some() && std::env::var("OMNI_SLOW_APP").ok().as_deref() == nice {
        cmd.env("OMNI_SLOW_SYSCALL_MS", std::env::var("OMNI_SLOW_APP_MS").unwrap_or_else(|_| "200".into()));
    }
    // OMNI_APP_ENV_FILE=<host file>: `NAME=VALUE` lines given to each app host process started
    // after the file changed -- a per-app switch measured launch against launch on one live device.
    if let Some(text) = std::env::var_os("OMNI_APP_ENV_FILE").and_then(|f| std::fs::read_to_string(f).ok()) {
        for (k, v) in text.lines().filter_map(|l| l.trim().split_once('=')) {
            cmd.env(k.trim(), v.trim());
        }
    }
    cmd
}

/// Start a prepared host process as pid `pid`, `uid`: its binder credential on its stdin, reaped by
/// a thread of its own, kept by pid for the signals the system sends it.
fn spawn(mut cmd: std::process::Command, pid: i32, uid: u32) -> Option<i32> {
    match cmd.spawn() {
        Ok(mut child) => {
            let cred = crate::remote::issue_credential(pid, uid);
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = writeln!(stdin, "{}", crate::remote::credential_hex(&cred));
                // Dropped: closed, so the app host's own stdin reads end of file after it.
            }
            // Reaped by a thread of its own; the app's end reaches the system through binder. Kept
            // by pid meanwhile, for a signal the system sends it (`signal`).
            let child = Arc::new(parking_lot::Mutex::new(child));
            CHILDREN.lock().insert(pid, Arc::clone(&child));
            std::thread::spawn(move || {
                let status = loop {
                    match child.lock().try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {}
                        Err(e) => break Err(e),
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                };
                CHILDREN.lock().remove(&pid);
                crate::remote::revoke_credential(pid);
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

/// **A spare app process** (`OMNI_APP_SPARE=1`): an app host process started before the app it will
/// be is known -- its host process, ART and its binder up, as a zygote's unspecialised process is
/// -- waiting in `com.omnidroid.spare.Spare` (`/vendor/framework/omni-spare.jar`) for a file that
/// names the app: `<uid> <target sdk> <class> [args...]`. The pid it was started under is the one
/// the zygote's reply gives ActivityManager, its uid is rebound (`remote::rebind_uid`) before it
/// runs WrapperInit (its preload done while it waited), and a new spare is started some seconds
/// after (`OMNI_APP_SPARE_DELAY_S`, default 5: the app's start runs on one thread, the spare's on
/// others). The warm device keeps one (`tests/r_roblox.rs`); an app session does not.
struct Spare {
    pid: i32,
    /// The host path of the file it waits for.
    go: PathBuf,
}

static SPARE: parking_lot::Mutex<Option<Spare>> = parking_lot::Mutex::new(None);

fn spares() -> bool {
    std::env::var("OMNI_APP_SPARE").as_deref() == Ok("1") && !LAUNCHER.get().is_some_and(|l| hiding_device(&l.instance))
}

/// Whether `instance` is a rooted device that hides root from some apps (a non-empty DenyList, or
/// Shamiko's whitelist mode). A spare's address space is built before its app is known, as a
/// non-hidden process, and cannot be hidden afterwards: such a device starts every app fresh.
pub(crate) fn hiding_device(instance: &std::path::Path) -> bool {
    crate::root::Profile::of(instance).is_some_and(|p| p.is_rooted() && (!p.denylist.is_empty() || p.shamiko == crate::root::Shamiko::Whitelist))
}

/// Start the first spare once `ready` (Android has booted), and keep one after.
pub fn start_spares_when(ready: impl Fn() -> bool + Send + 'static) {
    if !spares() {
        if std::env::var("OMNI_APP_SPARE").as_deref() == Ok("1") && LAUNCHER.get().is_some_and(|l| hiding_device(&l.instance)) {
            eprintln!("[zygote] spare app process disabled: this device hides root from apps (a spare cannot be hidden after it starts)");
        }
        return;
    }
    let _ = std::thread::Builder::new().name("zygote-spare".into()).spawn(move || {
        while !ready() {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        start_spare();
    });
}

/// Start a spare app process, unless one waits.
fn start_spare() {
    let Some(launcher) = LAUNCHER.get() else { return };
    if SPARE.lock().is_some() {
        return;
    }
    let pid = crate::process::reserve_pid();
    let guest = format!("/data/local/tmp/omni-spare/{pid}");
    let Some(go) = crate::vfs::host_path(&launcher.instance, guest.trim_start_matches('/').as_bytes()) else { return };
    if let Some(dir) = go.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&go);
    let mut cmd = host_command(launcher, pid, 0, Some("omni-spare"), &["CLASSPATH=/vendor/framework/omni-spare.jar"]);
    cmd.args(["com.omnidroid.spare.Spare", &guest]);
    if spawn(cmd, pid, 0).is_some() {
        eprintln!("[zygote] a spare app process as pid {pid}");
        *SPARE.lock() = Some(Spare { pid, go });
    }
}

/// The waiting spare becomes `class args...` as `uid`: its pid, or `None` (no spare, or it ended).
fn take_spare(uid: u32, nice: Option<&str>, sdk: u32, class_and_args: &[String]) -> Option<i32> {
    if !spares() {
        return None;
    }
    // `OMNI_APP_SPARE=0` in the per-app switches: this start does without (an A/B on one device).
    let off = std::env::var_os("OMNI_APP_ENV_FILE").and_then(|f| std::fs::read_to_string(f).ok()).is_some_and(|t| t.lines().any(|l| l.trim() == "OMNI_APP_SPARE=0"));
    if off {
        return None;
    }
    let spare = SPARE.lock().take()?;
    if !CHILDREN.lock().contains_key(&spare.pid) {
        return None;
    }
    crate::remote::rebind_uid(spare.pid, uid);
    let part = spare.go.with_extension("part");
    let line = format!("{uid} {sdk} {}", class_and_args.join(" "));
    if std::fs::write(&part, line).is_err() || std::fs::rename(&part, &spare.go).is_err() {
        return None;
    }
    eprintln!("[zygote] launching {} as pid {} uid {uid} (the spare process): {}", nice.unwrap_or("?"), spare.pid, class_and_args.join(" "));
    let delay = std::env::var("OMNI_APP_SPARE_DELAY_S").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(delay));
        start_spare();
    });
    Some(spare.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_that_hides_root_starts_no_spare() {
        // One instance dir per profile: the profile cache is per directory.
        let hides = |tag: &str, text: Option<&str>| {
            let dir = std::env::temp_dir().join(format!("omni-spare-hiding-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(t) = text {
                std::fs::write(dir.join(".omni-root-profile"), t).unwrap();
            }
            let r = hiding_device(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            r
        };
        assert!(!hides("none", None), "no profile: spares are fine");
        assert!(!hides("plain", Some("root=1
")), "rooted, nothing hidden");
        assert!(hides("deny", Some("root=1
denylist=com.x
")));
        assert!(hides("white", Some("root=1
shamiko=whitelist
")));
    }

    fn launcher() -> Arc<Launcher> {
        Arc::new(Launcher {
            runner: PathBuf::from("/nonexistent/omni-linux-run"),
            sysroot: PathBuf::new(),
            instance: PathBuf::new(),
            envp: Vec::new(),
            binder: String::new(),
            vm_options: Vec::new(),
        })
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_preload_is_answered_as_done_not_started_as_a_class() {
        let l = launcher();
        for first in ["--preload-package", "--preload-app"] {
            let a = args(&[first, "/product/app/webview/webview.apk", "/product/app/webview/lib/arm64", "libwebviewchromium.so", "arm64-v8a"]);
            assert_eq!(answer(&a, &l, 0x5157), Some(int(0)), "{first}");
        }
    }

    /// The WebView's zygote, as ActivityManager asks for it (`ZygoteProcess.startChildZygote`).
    #[test]
    fn a_child_zygote_is_answered_here_on_its_socket() {
        let l = launcher();
        let a = args(&[
            "--runtime-args", "--setuid=1053", "--setgid=1053", "--start-child-zygote", "--nice-name=webview_zygote",
            "com.android.internal.os.WebViewZygoteInit", "--zygote-socket=com.android.internal.os.WebViewZygoteInit/test-socket",
            "--abi-list=arm64-v8a", "--uid-range-start=99000", "--uid-range-end=99999",
        ]);
        assert_eq!(child_zygote_socket(&a), Some("com.android.internal.os.WebViewZygoteInit/test-socket"));
        let reply = answer(&a, &l, 0x5158).expect("a reply");
        let pid = i32::from_be_bytes(reply[0..4].try_into().unwrap());
        assert!(pid > 0, "a pid for the child zygote: {pid}");
        // Its socket is listening in the instance: a start is asked of it next.
        let bound = crate::unix::Bound::find(0x5158, b"@com.android.internal.os.WebViewZygoteInit/test-socket");
        assert!(bound.is_some_and(|b| b.listening.load(std::sync::atomic::Ordering::SeqCst)));
        // Not a child zygote: an app.
        assert_eq!(child_zygote_socket(&args(&["--setuid=10115", "android.app.ActivityThread", "seq=1"])), None);
    }

    #[test]
    fn a_spare_is_for_an_app_installed_in_data_app() {
        let dir = std::env::temp_dir().join(format!("omni-zygote-installed-{}", std::process::id()));
        let base = dir.join("data/app/~~a==/com.example.app-b-c==/base.apk");
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        std::fs::write(&base, b"apk").unwrap();
        assert!(installed_app(&dir, "com.example.app"));
        assert!(!installed_app(&dir, "com.example"), "a prefix of it is another package");
        assert!(!installed_app(&dir, "com.android.settings"), "the image's apps are not in /data/app");
        assert!(!installed_app(&dir, ""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
