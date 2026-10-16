//! **The omnidroid app** (`omnidroid` with no command): a full-screen terminal app that keeps warm
//! Android devices and runs any APK on them, as many instances as asked, each in a window of its
//! own (`omni_pool`). It warms a device as it opens, so the first launch starts at once (unless the
//! last one was killed: then it opens with none until a launch or `warm`); it closes
//! idle ones (`idle_close_minutes`, `keep_warm`); and, when `mcp_enabled`, it serves MCP on
//! 127.0.0.1 for coding agents, which then drive the same pool.
//!
//! Every window open on this computer shows **one** pool, kept by a process of its own ([`host`]):
//! a second window boots no Android and touches nothing the first runs.
//!
//! The one-shot commands (`aosp`, `play`, `which`, `plugins`, `modules`) remain for scripts and
//! for the app's own use (a device is `omnidroid aosp --warm`), and are left out of the help.
pub mod command;
pub mod host;
pub mod live;
pub mod mcp;
pub mod plugged;
pub mod tui;
pub mod web;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Open the app, launching `first` (an APK, a count and the plugins' options) once it is up: a
/// window on the pool -- the one already running on this computer, or one started now ([`host`]).
#[must_use]
pub fn run(first: Option<(PathBuf, u32, Vec<String>)>) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("omnidroid: where this program is: {e}");
            return ExitCode::FAILURE;
        }
    };
    let joined = host::running().is_some();
    let url = match host::ensure(&exe) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("omnidroid: the pool: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut client = match host::Client::attach(&url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("omnidroid: the pool at {url}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if joined {
        client.say("joined the omnidroid already open: its Androids and instances are this window's too");
    }
    if let Some((apk, n, options)) = first {
        tui::run_apk(&mut client, &apk, n, &options);
    }
    client.say("type `run <apk>` to launch any APK, `help` for the rest");
    let app = match tui::App::new(client).run() {
        Ok(app) => app,
        Err(e) => {
            eprintln!("omnidroid: the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let client = app.client;
    let stop = app.stop_on_exit;
    match client.detach() {
        Ok(0) if stop => {
            println!("omnidroid: stopping the Androids...");
            client.wait_gone(Duration::from_secs(120));
        }
        Ok(0) => println!("omnidroid: the Androids keep running (stop_on_exit = false); the app adopts them when it opens again"),
        Ok(n) => println!("omnidroid: {n} other window{} still open: the Androids and their apps run on", if n == 1 { "" } else { "s" }),
        Err(e) => eprintln!("omnidroid: the pool: {e}"),
    }
    ExitCode::SUCCESS
}

/// **The app without its screen** (`omnidroid serve [--port <n>]`): the pool and its MCP server
/// (on, whatever the settings say), the log on stdout, until stdin closes -- for a host with no
/// terminal to show it on, and for tests. Windows opened meanwhile attach to it.
#[must_use]
pub fn serve(port: Option<u16>) -> ExitCode {
    host::run(host::Mode::Serve(port))
}
