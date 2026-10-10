//! **The omnidroid app** (`omnidroid` with no command): a full-screen terminal app that keeps warm
//! Android devices and runs any APK on them, as many instances as asked, each in a window of its
//! own (`omni_pool`). It warms a device as it opens, so the first launch starts at once; it closes
//! idle ones (`idle_close_minutes`, `keep_warm`); and, when `mcp_enabled`, it serves MCP on
//! 127.0.0.1 for coding agents, which then drive the same pool.
//!
//! The one-shot commands (`aosp`, `play`, `which`, `plugins`, `modules`) remain for scripts and
//! for the app's own use (a device is `omnidroid aosp --warm`), and are left out of the help.
pub mod command;
pub mod live;
pub mod mcp;
pub mod tui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use omni_pool::{Config, Pool};

/// Open the app, launching `first` (an APK and a count) once it is up.
#[must_use]
pub fn run(first: Option<(PathBuf, u32)>) -> ExitCode {
    let path = Config::path();
    let (config, problems) = path.as_deref().map(Config::load).unwrap_or_default();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("omnidroid: where this program is: {e}");
            return ExitCode::FAILURE;
        }
    };
    let pool = Pool::start(config.clone(), path.clone(), exe);
    pool.say(format!("omnidroid {} -- settings in {}", env!("CARGO_PKG_VERSION"), path.as_ref().map_or_else(|| "(none)".to_string(), |p| p.display().to_string())));
    for p in problems {
        pool.say(format!("settings: {p}"));
    }
    let server = if config.mcp_enabled {
        match mcp::McpServer::start(Arc::clone(&pool), config.mcp_port) {
            Ok(s) => {
                pool.say(format!("MCP: listening on {} (coding agents: `mcp status`)", s.url()));
                Some(s)
            }
            Err(e) => {
                pool.say(format!("MCP: {e}"));
                None
            }
        }
    } else {
        None
    };
    if let Some((apk, n)) = first {
        if let Err(e) = pool.launch(&apk, n) {
            pool.say(format!("run: {e}"));
        }
    }
    pool.say("type `run <apk>` to launch any APK, `help` for the rest");
    let app = match tui::App::new(Arc::clone(&pool), server).run() {
        Ok(app) => app,
        Err(e) => {
            eprintln!("omnidroid: the terminal: {e}");
            pool.shutdown(pool.config().stop_on_exit);
            return ExitCode::FAILURE;
        }
    };
    if let Some(s) = &app.mcp {
        s.stop();
    }
    let stop = pool.config().stop_on_exit;
    if stop {
        println!("omnidroid: stopping the Androids...");
    } else {
        println!("omnidroid: the Androids keep running (stop_on_exit = false); the app adopts them when it opens again");
    }
    pool.shutdown(stop);
    ExitCode::SUCCESS
}

/// **The app without its screen** (`omnidroid serve [--port <n>]`): the pool and its MCP server
/// (on, whatever the settings say), the log on stdout, until stdin closes -- for a host with no
/// terminal to show it on, and for tests.
#[must_use]
pub fn serve(port: Option<u16>) -> ExitCode {
    let path = Config::path();
    let (config, problems) = path.as_deref().map(Config::load).unwrap_or_default();
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("omnidroid: where this program is unknown");
        return ExitCode::FAILURE;
    };
    let pool = Pool::start(config.clone(), path, exe);
    for p in problems {
        pool.say(format!("settings: {p}"));
    }
    let server = match mcp::McpServer::start(Arc::clone(&pool), port.unwrap_or(config.mcp_port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("omnidroid: MCP: {e}");
            pool.shutdown(false);
            return ExitCode::FAILURE;
        }
    };
    println!("omnidroid serve: MCP on {} (until stdin closes)", server.url());
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut sink = String::new();
            while std::io::stdin().read_line(&mut sink).is_ok_and(|n| n > 0) {
                sink.clear();
            }
            done.store(true, std::sync::atomic::Ordering::Release);
        });
    }
    let mut seen = 0;
    while !done.load(std::sync::atomic::Ordering::Acquire) {
        let (lines, total) = pool.log_since(seen);
        for line in lines {
            println!("{line}");
        }
        seen = total;
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    server.stop();
    pool.shutdown(pool.config().stop_on_exit);
    println!("omnidroid serve: stopped");
    ExitCode::SUCCESS
}
