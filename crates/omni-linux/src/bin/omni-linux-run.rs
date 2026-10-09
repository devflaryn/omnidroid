//! `omni-linux-run --sysroot <dir> [--instance <dir>] [--env KEY=VALUE]... [--service <program>]... -- <program> [args...]`
//!
//! A `--service` is started first, as `system` (uid 1000), in this host process -- a daemon the
//! program talks to over binder, as `servicemanager`. A `--hal` (`gralloc`) is a HAL the host
//! serves, published with servicemanager once it runs.
use std::path::PathBuf;
use std::process::ExitCode;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

/// `--features mimalloc`: this host process's heap is mimalloc (an A/B build; `tests/alloc_cost.rs`
/// has the numbers).
#[cfg(feature = "mimalloc")]
#[global_allocator]
static HEAP: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The system's heap, with `OMNI_ALLOC_TRACE_KB`'s trace of large blocks (`omni_linux::alloc_trace`;
/// a relaxed load per allocation while off).
#[cfg(not(feature = "mimalloc"))]
#[global_allocator]
static HEAP: omni_linux::alloc_trace::Tracing = omni_linux::alloc_trace::Tracing;

fn main() -> ExitCode {
    // The host's descriptor limit, as high as it allows: the system's host process holds every
    // guest process's files and sockets, and Linux's default soft limit (1024) ran out on a boot
    // pinned to 2 CPUs -- init's services then failed to start ("Too many open files", system_suspend
    // among them) and the Watchdog ended system_server (2026-10-01, the notebook flow).
    let _ = omni_platform::process::raise_descriptor_limit();
    omni_linux::poll::start_stats();
    omni_linux::code_trim::start();
    omni_linux::lever::start();
    omni_linux::proccpu::start();
    omni_linux::jit_time::start();
    omni_linux::zero_reclaim::start();
    omni_linux::alloc_trace::start();
    let mut args = std::env::args().skip(1);
    let mut sysroot = PathBuf::from("sysroot/aosp-35");
    let mut instance = std::env::temp_dir().join("omni-linux-run");
    let mut argv = Vec::new();
    let mut services: Vec<String> = Vec::new();
    let mut uid: u32 = 10_000;
    let mut init_classes: Vec<String> = Vec::new();
    let mut hals: Vec<String> = Vec::new();
    let mut caps: Option<u64> = None;
    let mut setprops: Vec<(String, String)> = Vec::new();
    let mut zygote = false;
    let mut then: Vec<String> = Vec::new();
    let mut control: Option<PathBuf> = None;
    let mut envp = vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sysroot" => sysroot = PathBuf::from(args.next().expect("--sysroot needs a value")),
            "--instance" => instance = PathBuf::from(args.next().expect("--instance needs a value")),
            "--env" => envp.push(args.next().expect("--env needs KEY=VALUE").into_bytes()),
            // A file of `export NAME VALUE` lines (the image's derived classpath, as
            // `data/system/environ/classpath` holds it): each becomes an environment variable. Lets
            // a launcher pass the long BOOTCLASSPATH etc. without a huge command line.
            "--classpath-file" => {
                let path = args.next().expect("--classpath-file needs a path");
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("--classpath-file {path}: {e}"));
                for line in text.lines() {
                    if let Some(rest) = line.trim().strip_prefix("export ") {
                        if let Some((name, value)) = rest.split_once(char::is_whitespace) {
                            envp.push(format!("{}={}", name.trim(), value.trim()).into_bytes());
                        }
                    }
                }
            }
            "--service" => services.push(args.next().expect("--service needs a program")),
            "--uid" => uid = args.next().and_then(|u| u.parse().ok()).expect("--uid needs a number"),
            "--hal" => hals.push(args.next().expect("--hal needs a name (gralloc, composer)")),
            // This host process's binder is the system's, in another host process (`crate::remote`).
            "--binder-server" => omni_linux::remote::set_server(&args.next().expect("--binder-server needs host:port")),
            // The credential the system issued this host process (`crate::remote`): one line of
            // hex on stdin, never on the command line other accounts can read.
            "--binder-credential-stdin" => {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let c = omni_linux::remote::credential_from_hex(&line).expect("--binder-credential-stdin: 32 hex characters on stdin");
                omni_linux::remote::set_credential(c);
            }
            // The program's pid, as the system assigned it.
            "--pid" => omni_linux::process::assign_next_pid(args.next().and_then(|v| v.parse().ok()).expect("--pid needs a number")),
            // Answer /dev/socket/zygote: apps ActivityManager starts are launched in host processes
            // of their own, their binder this one's (`omni_linux::zygote`).
            "--zygote" => zygote = true,
            // A shell command run beside the program, as the shell user (adb's `shell`).
            "--then" => then.push(args.next().expect("--then needs a shell command")),
            // The `--then` shell command read from a file (so a launcher need not put a shell
            // command, with its quotes and pipes, on the command line).
            "--then-file" => {
                let path = args.next().expect("--then-file needs a path");
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("--then-file {path}: {e}"));
                then.push(text);
            }
            // The device's control channel: shell commands handed over as files in this host
            // directory while the device runs (`serve_control`).
            "--control" => control = Some(PathBuf::from(args.next().expect("--control needs a directory"))),
            // A property set before the program starts (`name=value`), as init sets one.
            "--setprop" => {
                let kv = args.next().expect("--setprop needs name=value");
                let (k, v) = kv.split_once('=').expect("--setprop needs name=value");
                setprops.push((k.to_string(), v.to_string()));
            }
            // The program's capabilities (comma-separated names), as a zygote or init grants them.
            "--caps" => {
                let names = args.next().expect("--caps needs capability names");
                caps = Some(names.split(',').filter_map(omni_linux::sys::cap_number).fold(0, |m, n| m | (1u64 << n)));
            }
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
    // The device's global environment (init.environ.rc), as init gives every process: the program,
    // the apps the zygote launches, init's services and the --then shells. A variable set already
    // (--env) is kept.
    for (name, value) in omni_linux::device::global_environment() {
        let key = format!("{name}=");
        if !envp.iter().any(|e| e.starts_with(key.as_bytes())) {
            envp.push(format!("{name}={value}").into_bytes());
        }
    }
    // The system's host process holds its apps' host processes (the zygote's): a session that is
    // killed leaves none of them running. An app's host process ends with the system's.
    if zygote {
        omni_platform::process::hold_children();
    }
    if omni_linux::remote::is_remote() {
        omni_platform::process::end_with_parent();
    }
    let config = SpawnConfig {
        sysroot: sysroot.clone(),
        instance_dir: instance.clone(),
        argv: argv.clone(),
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
    if zygote {
        match omni_linux::remote::serve(std::sync::Arc::clone(p.vfs.sysroot())) {
            Ok(addr) => {
                let runner = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("omni-linux-run"));
                // The zygote's VM options: the system's app_process's `-X` options (argv before its
                // parent directory), less the hidden-API policy the zygote sets for each app.
                let vm_options: Vec<String> = argv
                    .iter()
                    .skip(1)
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .take_while(|a| a.starts_with("-X"))
                    .filter(|a| !a.starts_with("-Xhidden-api-policy"))
                    .collect();
                let launcher = omni_linux::zygote::Launcher { runner, sysroot: sysroot.clone(), instance: instance.clone(), envp: envp.clone(), binder: addr.to_string(), vm_options };
                omni_linux::zygote::serve(std::sync::Arc::as_ptr(p.vfs.binds()) as usize, launcher);
                eprintln!("[zygote] /dev/socket/zygote; apps' binder at {addr}");
                // OMNI_APP_SPARE=1: a spare app process once Android has booted.
                let sysroot = std::sync::Arc::clone(p.vfs.sysroot());
                omni_linux::zygote::start_spares_when(move || {
                    omni_linux::props::PropertyService::global(&sysroot).get("sys.boot_completed").as_deref() == Some("1")
                });
            }
            Err(e) => eprintln!("[zygote] {e}"),
        }
    }
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
                let started = init.boot(&classes);
                eprintln!("[init] started {} services: {}", started.len(), started.join(" "));
            }
            Err(e) => eprintln!("[init] {e}"),
        }
    }
    // The HALs below add themselves to servicemanager, and the program uses it: wait for it to be
    // the binder context manager. With init's classes it was started at `on init`, seconds ago, and
    // the fixed 1.5 s this used to sleep was spent on the boot's critical path (system_server starts
    // after it). At most 1.5 s, as before; `OMNI_INIT_FIXED_WAIT=1` sleeps the fixed 1.5 s.
    if !init_classes.is_empty() && std::env::var("OMNI_INIT_FIXED_WAIT").as_deref() != Ok("1") {
        let t = std::time::Instant::now();
        let broker = omni_linux::binder::broker(omni_linux::binder::Context::Binder);
        while !broker.has_context_manager() && t.elapsed() < std::time::Duration::from_millis(1500) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        eprintln!("[init] servicemanager ready after {} ms", t.elapsed().as_millis());
    } else if !daemons.is_empty() || !init_classes.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    let mut _served = Vec::new();
    let mut _composers = Vec::new();
    let mut framebuffer: Option<std::sync::Arc<omni_linux::hal::framebuffer::Framebuffer>> = None;
    for hal in &hals {
        let broker = omni_linux::binder::broker(omni_linux::binder::Context::Binder);
        match hal.as_str() {
            "gralloc" => {
                let allocator = omni_linux::hal::gralloc::Allocator::new();
                match allocator.register(&broker) {
                    Ok(()) => eprintln!("[hal] gralloc: {}", omni_linux::hal::gralloc::INSTANCE),
                    Err(e) => eprintln!("[hal] gralloc: {e}"),
                }
                _served.push(allocator);
            }
            "composer" => {
                // OMNI_WINDOW_SIZE=<w>x<h>: the display's size at boot (default 1280x720).
                let (w, h) = std::env::var("OMNI_WINDOW_SIZE")
                    .ok()
                    .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))))
                    .map_or((omni_linux::hal::composer::WIDTH, omni_linux::hal::composer::HEIGHT), |(w, h)| {
                        (w.max(omni_linux::hal::composer::MIN_SIDE), h.max(omni_linux::hal::composer::MIN_SIDE))
                    });
                let fb = std::sync::Arc::new(omni_linux::hal::framebuffer::Framebuffer::new(w, h));
                let composer = omni_linux::hal::composer::Composer::new(std::sync::Arc::clone(&broker), std::sync::Arc::clone(&fb));
                match composer.register() {
                    Ok(()) => eprintln!("[hal] composer: {}", omni_linux::hal::composer::INSTANCE),
                    Err(e) => eprintln!("[hal] composer: {e}"),
                }
                // OMNI_WINDOW=1: the display in a live host window whose size is the display's
                // (`omni_linux::display_window`); OMNI_WINDOW_CONTROL=<file> scripts its resizes.
                if std::env::var("OMNI_WINDOW").as_deref() == Ok("1") {
                    let options = omni_linux::display_window::Options {
                        title: "omnidroid".into(),
                        control: std::env::var_os("OMNI_WINDOW_CONTROL").map(PathBuf::from),
                        // OMNI_WINDOW_INPUT=0: the window shows, and Android has no keyboard or mouse.
                        input: std::env::var("OMNI_WINDOW_INPUT").as_deref() != Ok("0"),
                    };
                    // The window's keyboard types what the host's layout types (`omni_linux::keymap`):
                    // its key layout and character map written to the data partition here, before
                    // the program runs and so before EventHub opens the device. OMNI_HOST_KEYMAP=0:
                    // Android's own (Generic, US).
                    if options.input {
                        omni_linux::keymap::install(&instance, p.vfs.sysroot());
                    }
                    if let Err(e) = omni_linux::display_window::spawn(std::sync::Arc::clone(&fb), std::sync::Arc::clone(&composer), options) {
                        eprintln!("[window] {e}");
                    }
                    // What the guest copies, on the host's clipboard (text and images only; on by
                    // default, OMNI_CLIPBOARD=0 turns it off).
                    if omni_linux::clipboard::enabled() {
                        omni_linux::clipboard::start(std::sync::Arc::clone(&broker));
                    } else {
                        eprintln!("[clipboard] off (OMNI_CLIPBOARD=0): the device's clipboard is its own");
                    }
                    // A web page an app hands to the browser opens in a host window of its own,
                    // beside the device's (OMNI_BROWSER_WINDOW=0: the device's browser).
                    if omni_linux::browser::enabled() {
                        omni_linux::browser::start(std::sync::Arc::clone(&broker));
                    }
                }
                // OMNI_DISPLAYS=<n>: that many displays, each with a window of its own, so that an
                // app started with `am start --display <id>` runs beside the one on display 0
                // rather than backgrounding it. Added before SurfaceFlinger's callback, so
                // `registerCallback` hotplugs them all at once.
                // **Two is the ceiling, and it is SurfaceFlinger's**: its HWComposer takes a primary
                // and one external physical display and refuses the rest ("Ignoring connection of
                // tertiary display 2"), so a third app needs a virtual display, not a third
                // physical one. Asking for more is capped, and said so.
                let asked = std::env::var("OMNI_DISPLAYS").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);
                let displays = asked.min(2);
                if asked > displays {
                    eprintln!("[composer] OMNI_DISPLAYS={asked}: SurfaceFlinger takes one external display beside the primary, so {displays} are made");
                }
                for _ in 1..displays {
                    match composer.add_display(w, h) {
                        Ok((id, extra)) => {
                            if std::env::var("OMNI_WINDOW").as_deref() == Ok("1") {
                                // **Its window opens when it first draws.** A display with no app on
                                // it has nothing to show, and a device booted with displays to spare
                                // would otherwise put an empty window on screen for each.
                                let composer = std::sync::Arc::clone(&composer);
                                let _ = std::thread::Builder::new().name(format!("omni-display-{id}-wait")).spawn(move || {
                                    if !extra.wait_frame(1, std::time::Duration::from_secs(24 * 3600)) {
                                        return;
                                    }
                                    let options = omni_linux::display_window::Options {
                                        title: format!("omnidroid — display {id}"),
                                        control: None,
                                        // One window's keyboard and mouse are the device's; a second
                                        // set would reach display 0 too, until input is associated
                                        // with a display (the multi-instance design's phase 2).
                                        input: false,
                                    };
                                    if let Err(e) = omni_linux::display_window::spawn(extra, composer, options) {
                                        eprintln!("[window] display {id}: {e}");
                                    }
                                });
                            }
                        }
                        Err(e) => eprintln!("[composer] a display could not be added: {e}"),
                    }
                }
                framebuffer = Some(fb);
                _composers.push(composer);
            }
            other => eprintln!("[hal] unknown HAL {other:?}"),
        }
    }
    // OMNI_SCREENSHOT=<path>: the display's framebuffer, as a PNG, every few seconds
    // (`OMNI_SCREENSHOT_MS`, default 5000), written whole (a rename) so a reader never sees half.
    if let (Some(fb), Ok(path)) = (framebuffer.clone(), std::env::var("OMNI_SCREENSHOT")) {
        let every = std::env::var("OMNI_SCREENSHOT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(5000);
        std::thread::spawn(move || {
            let (mut seen, mut since) = (0, std::time::Instant::now());
            let mut last_line = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(every.max(100)));
                let frames = fb.frames();
                // Presented frames, and the rate since the previous line (at most one line in 5 s).
                if frames != seen && last_line.elapsed() >= std::time::Duration::from_secs(5) {
                    let fps = (frames - seen) as f64 / since.elapsed().as_secs_f64();
                    eprintln!("[display] {frames} frames presented ({fps:.2}/s)");
                    (seen, since, last_line) = (frames, std::time::Instant::now(), std::time::Instant::now());
                }
                let partial = format!("{path}.part");
                if std::fs::write(&partial, fb.png()).is_ok() {
                    let _ = std::fs::rename(&partial, &path);
                }
            }
        });
    }
    if let Some(caps) = caps {
        p.sys.set_caps(caps);
    }
    // OMNI_THREAD_DUMP=<seconds>: the program's threads waiting in a system call for 2 s or more,
    // every so often, by name and where they called from.
    if let Some(every) = std::env::var("OMNI_THREAD_DUMP").ok().and_then(|v| v.parse::<u64>().ok()) {
        let program = std::sync::Arc::clone(&p);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
            let describe = |at: u64| program.mm.describe(at & 0x00ff_ffff_ffff_ffff).unwrap_or_else(|| format!("{at:#x}"));
            let lines = omni_linux::process::blocked_calls(std::time::Duration::from_secs(2), describe);
            eprintln!("[threads] pid {}: {} waiting
[threads]   {}", program.sys.pid, lines.len(), lines.join("
[threads]   "));
        });
    }
    // Tagged guest accesses the CPU's slow path served (`OMNI_JIT_TBI=0` or the `jit_tbi=0` lever:
    // every one, a host fault each), every 30 s while there are any new ones.
    {
        let name = argv.iter().find_map(|a| String::from_utf8_lossy(a).strip_prefix("--nice-name=").map(String::from)).unwrap_or_else(|| "system".into());
        std::thread::spawn(move || {
            let mut seen = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let now = omni_cpu::dynarmic::tagged_accesses();
                if now != seen {
                    eprintln!(
                        "[tbi] {name} host process {}: {now} tagged accesses served by the slow path (+{} in 30 s); {} guest instructions learned the mask",
                        std::process::id(),
                        now - seen,
                        omni_cpu::dynarmic::tbi_sites_noted()
                    );
                    seen = now;
                }
            }
        });
    }
    // OMNI_SYSCALL_STATS=<seconds>: this host process's system calls over each period, most time
    // first (calls, and the time spent in them, summed over threads).
    if let Some(every) = std::env::var("OMNI_SYSCALL_STATS").ok().and_then(|v| v.parse::<u64>().ok()) {
        let name = argv.iter().find_map(|a| String::from_utf8_lossy(a).strip_prefix("--nice-name=").map(String::from)).unwrap_or_else(|| "system".into());
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
            let top: Vec<String> = omni_linux::process::syscall_stats().into_iter().take(8).map(|(n, c, ms)| format!("{n} {c}x {ms}ms")).collect();
            eprintln!("[syscalls] {name} {}s: {}", every, top.join(", "));
        });
    }
    // OMNI_MEM_TRACE=<seconds>: this host process's commit charge beside its program's guest
    // memory, every so often -- what the difference (the CPU backend's translations, the runtime's
    // own heap) costs per host process.
    if let Some(every) = std::env::var("OMNI_MEM_TRACE").ok().and_then(|v| v.parse::<u64>().ok()) {
        let space = std::sync::Arc::clone(p.mem.space());
        let program = std::sync::Arc::clone(&p);
        let name = argv.iter().find_map(|a| String::from_utf8_lossy(a).strip_prefix("--nice-name=").map(String::from)).unwrap_or_else(|| String::from_utf8_lossy(argv.first().map_or(&[][..], |a| a.as_slice())).into_owned());
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
            let host = omni_platform::vm::process_commit_charge().unwrap_or(0) >> 20;
            let s = space.stats();
            eprintln!(
                "[mem] {name} host process {}: commit {host} MiB; the program's guest space: committed {} MiB, mapped {} MiB (file {} MiB)",
                std::process::id(),
                s.committed >> 20,
                s.mapped >> 20,
                s.file_backed >> 20
            );
            // The largest committers, by mapping, with the name the guest gave each.
            let mut by: std::collections::HashMap<(usize, usize), usize> = std::collections::HashMap::new();
            for r in space.mapped_regions() {
                *by.entry((r.mapping_start, r.mapping_len)).or_default() += r.committed;
            }
            let mut top: Vec<_> = by.into_iter().filter(|(_, c)| *c >= 4 << 20).collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            let names: Vec<String> = top
                .iter()
                .take(8)
                .map(|((start, len), c)| {
                    let label = program.mm.name_at(*start as u64).map_or_else(|| "anon".into(), |(n, _)| String::from_utf8_lossy(&n).into_owned());
                    format!("{label} {start:#x}+{}M: {}M", len >> 20, c >> 20)
                })
                .collect();
            eprintln!("[mem] {name} top: {}", names.join("; "));
            // Every guest process of this host process: their guest memory and their translation
            // caches, the largest named.
            let all = omni_linux::process::all_live();
            let mut each: Vec<(String, u64, u64)> = all.iter().map(|p| (String::from_utf8_lossy(&p.comm.lock()).into_owned(), p.guest_committed() >> 20, p.code_cache_committed() >> 20)).collect();
            let (guest, code): (u64, u64) = each.iter().fold((0, 0), |(g, c), e| (g + e.1, c + e.2));
            each.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
            let largest: Vec<String> = each.iter().take(6).map(|(n, g, c)| format!("{n} {g}+{c}M")).collect();
            eprintln!("[mem] {name} host process {}: {} guest processes, guest memory {guest} MiB, translation caches {code} MiB; largest (guest+code): {}", std::process::id(), all.len(), largest.join(", "));
            // The translation caches' own tables, on the host's heap (not in the caches' commit).
            let census = omni_cpu::stats::code_caches_with_tables();
            let tables: Vec<String> = census.tables.iter().filter(|t| t.bytes > 0).map(|t| format!("{} {} entries {} KiB", t.name, t.entries, t.bytes >> 10)).collect();
            let heap: u64 = census.tables.iter().map(|t| t.bytes).sum();
            eprintln!("[mem] {name} host process {}: {} code caches' tables on the heap {} MiB: {}; dispatcher misses {}", std::process::id(), census.caches, heap >> 20, tables.join(", "), census.locked_lookups);
        });
    }
    // An app's host process: the system's properties, as they are now.
    if omni_linux::remote::is_remote() {
        match omni_linux::remote::system_properties() {
            Ok(props) => {
                let service = omni_linux::props::PropertyService::global(p.vfs.sysroot());
                for (k, v) in props {
                    if !k.starts_with("ro.") || service.get(&k).is_none() {
                        service.set(&k, &v);
                    }
                }
            }
            Err(e) => eprintln!("[remote] the system's properties: {e:?}"),
        }
    }
    if !setprops.is_empty() {
        let service = omni_linux::props::PropertyService::global(p.vfs.sysroot());
        for (k, v) in &setprops {
            service.set(k, v);
        }
    }
    for command in &then {
        let config = SpawnConfig {
            sysroot: sysroot.clone(),
            instance_dir: instance.clone(),
            argv: vec![b"/system/bin/sh".to_vec(), b"-c".to_vec(), command.as_bytes().to_vec()],
            envp: envp.iter().filter(|e| !e.starts_with(b"CLASSPATH=")).cloned().collect(),
            stdout: Output::Host,
            stderr: Output::Host,
            trace: false,
        };
        match Process::spawn_as(config, 2000) {
            Ok(sh) => {
                std::thread::spawn(move || {
                    let status = sh.run();
                    eprintln!("[then] {status:?}");
                });
            }
            Err(e) => eprintln!("[then] {e}"),
        }
    }
    if let Some(dir) = control {
        let (sysroot, instance) = (sysroot.clone(), instance.clone());
        let envp: Vec<Vec<u8>> = envp.iter().filter(|e| !e.starts_with(b"CLASSPATH=")).cloned().collect();
        serve_control(dir, move |command, uid, out| {
            let config = SpawnConfig {
                sysroot: sysroot.clone(),
                instance_dir: instance.clone(),
                argv: vec![b"/system/bin/sh".to_vec(), b"-c".to_vec(), command.as_bytes().to_vec()],
                envp: envp.clone(),
                stdout: Output::Capture(std::sync::Arc::clone(&out)),
                stderr: Output::Capture(out),
                trace: false,
            };
            Process::spawn_as(config, uid).map_err(|e| e.to_string())
        });
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

/// The device's control channel. A host program (the MCP server) drops a shell command in `dir` as
/// `<id>.cmd` (written whole: a rename); it is taken (`<id>.run`) and run as the shell user -- or as
/// the uid a first line `#uid=<n>` names -- beside the device, as `adb shell` runs one. Its output
/// (stdout and stderr, merged) is then `<id>.out` and its exit status `<id>.rc`, written in that
/// order, each whole. `dir/pid` names this host process. Commands run at once and side by side;
/// the directory is looked at every 50 ms, a host-side poll that costs the device nothing while
/// idle.
fn serve_control(
    dir: PathBuf,
    spawn: impl Fn(&str, u32, std::sync::Arc<parking_lot::Mutex<Vec<u8>>>) -> Result<std::sync::Arc<Process>, String> + Send + Sync + 'static,
) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("pid"), std::process::id().to_string());
    let spawn = std::sync::Arc::new(spawn);
    let mut beat = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let _ = std::thread::Builder::new().name("control".into()).spawn(move || loop {
        // `dir/alive`, written each second: the device lives (a reader goes by its time).
        if beat.elapsed() >= std::time::Duration::from_secs(1) {
            beat = std::time::Instant::now();
            let _ = std::fs::write(dir.join("alive"), b"1");
        }
        let mut taken: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "cmd")).collect())
            .unwrap_or_default();
        taken.sort();
        for cmd in taken {
            let run = cmd.with_extension("run");
            if std::fs::rename(&cmd, &run).is_err() {
                continue;
            }
            let text = std::fs::read_to_string(&run).unwrap_or_default();
            let uid = text.lines().next().and_then(|l| l.strip_prefix("#uid=")).and_then(|u| u.trim().parse().ok()).unwrap_or(2000);
            let spawn = std::sync::Arc::clone(&spawn);
            std::thread::spawn(move || {
                let out = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
                let code = match spawn(&text, uid, std::sync::Arc::clone(&out)) {
                    Ok(sh) => match sh.run() {
                        ExitStatus::Exited(code) => code as i64,
                        ExitStatus::Killed { signal, .. } => 128 + signal as i64,
                    },
                    Err(e) => {
                        out.lock().extend_from_slice(format!("control: {e}\n").as_bytes());
                        127
                    }
                };
                let whole = |to: PathBuf, bytes: &[u8]| {
                    let part = to.with_extension("part");
                    if std::fs::write(&part, bytes).is_ok() {
                        let _ = std::fs::rename(&part, &to);
                    }
                };
                whole(run.with_extension("out"), &out.lock());
                whole(run.with_extension("rc"), code.to_string().as_bytes());
                let _ = std::fs::remove_file(&run);
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    });
}
