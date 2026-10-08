//! `omnidroid`: run an APK in a real window for a person to use, on Windows, macOS or Linux.
//!
//! ```text
//! cargo run --release -p omnidroid -- play                      # the newest APK in the repository root
//! cargo run --release -p omnidroid -- play --apk Roblox-2.738.1397.apk
//! cargo run --release -p omnidroid -- play --minutes 90 --fresh --phone
//! cargo run --release -p omnidroid -- which [--apk <path>]      # say which APK would run, and exit
//! cargo run --release -p omnidroid -- plugins add --link plugins/roblox   # this user's plugins
//! ```
//!
//! **The APK is chosen, not built in**: `--apk`, else `OMNI_APK`, else the newest APK (by
//! `versionCode`) in the repository root (`omni_apk::choose_apk`). Its version is read from its own
//! manifest and is what the engine is told, so a new APK needs no code change.
//!
//! What `play` runs is the M5 gate's own run -- the embedding lives there today -- with a session as
//! long as the window stays open, the host's GPU, and the app's storage kept between runs. That is
//! exactly what `tools/play.ps1` (Windows) and `tools/play.sh` (macOS) ran, with the same switches;
//! both now forward here, and Linux has a launcher for the first time.
//!
//! * **Storage** is kept in `--data-dir`, default `omni_platform::process::app_data_dir()` -- each
//!   host's own convention. `--fresh` runs a fresh install and leaves the directory as it is.
//! * **End a run by closing the window**: the app is closed as a device closes it. Any other end --
//!   Ctrl+C, a crash -- is judged a crash by the engine at the next launch; after one, run once
//!   with `--fresh`.
//! * **Keyboard and mouse are this computer's** unless `--phone`, which makes the mouse a finger.
//! * **What one game needs is a plugin, not the launcher's** (`plugin.rs`): options of its own,
//!   variables, default arguments, hooks around a session and commands, declared in a plugin's
//!   `omnidroid-plugin.json` and installed per user (`omnidroid plugins`). Signing in to Roblox
//!   with a cookie (`--cookie`), joining a place (`--place`, `--join-delay`) and `login` are the
//!   `roblox` plugin's (`plugins/roblox`), for whoever installs it.
//! * **`--no-window`: no window at all** (`OMNI_NO_WINDOW=1`, and headless): for a host with no
//!   display. The engine's surface is an off-screen pbuffer on an EGL display that needs no window
//!   system (a GPU's EGL device, or Mesa's surfaceless platform -- `LIBGL_ALWAYS_SOFTWARE=1` for the
//!   CPU), and no Vulkan is bound, so the engine renders with GLES. `--headless` where no window
//!   can be opened falls back to the same.
//! * **`--headless` starts headless** (`OMNI_HEADLESS=1`, `omni_android::headless`): the engine runs
//!   and presents every frame, and the GPU draws none of them. **`--control <file>`** follows that
//!   file for commands (`OMNI_CONTROL`), one per line, appended by anything; the session's stdin is
//!   read for the same commands. `headless on`, `headless off` (the next frame is drawn again),
//!   `screenshot <path>` (that frame, rendered for real and saved as a PNG -- a relative path is
//!   this launcher's directory's) and `status`. Each is answered on stderr: `CONTROL: headless on`,
//!   `SCREENSHOT: saved <path> <w>x<h>`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod plugin;
mod warm;

/// The gate test whose run is a session: `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`.
const SESSION_TEST: &str = "initialize_native_code_returns_a_native_code_and_the_game_thread_starts";
/// "No limit": ten years, the gate's way of saying until the window is closed.
const UNTIL_CLOSED_SECONDS: u64 = 315_360_000;

const USAGE: &str = "\
usage: omnidroid [play] [--apk <path>] [--minutes <n>] [--fresh] [--phone] [--data-dir <dir>]
                       [--headless] [--no-window] [--control <file>]
       omnidroid aosp [--apk <path>] [--minutes <n>]
                      [--size <w>x<h>] [--gpu vulkan|gl|auto] [--with-systemui]
                      [--fresh-device] [--standby] [--instance <dir>]
                      [--root] [--module <id,id,...>] [--su all|<pkg,pkg>] [--no-clipboard]
       omnidroid modules [list | add <zip>]
       omnidroid aosp --warm [--instance <dir>] [--minutes <n>] [--size <w>x<h>] [--gpu vulkan|gl|auto]
       omnidroid which [--apk <path>]
       omnidroid plugins [list | add [--link] <dir> | remove <name> | enable <name> | disable <name>
                         | new <name> [<dir>]]

  --apk <path>      the APK to run (else OMNI_APK, else the newest *.apk in the repository root)
  --minutes <n>     end the session after n minutes (default: when the window is closed)
  --fresh           a fresh install; the kept storage is left as it is
  --phone           a touch screen: the mouse is a finger, and there is no keyboard
  --data-dir <dir>  where the app's storage is kept (default: this host's app-data directory)
  --headless        start headless: frames are run and presented, and the GPU draws none of them
                    (OMNI_HEADLESS=1); `headless off` on the control channel draws them again
  --no-window       no window at all (OMNI_NO_WINDOW=1; implies --headless): the engine's surface
                    is an off-screen buffer, for a host with no display -- a container, a notebook
  --control <file>  read commands from this file as lines are appended to it (OMNI_CONTROL), as
                    well as from stdin: headless on|off, screenshot <path>, status

  aosp              run the APK on the real-AOSP path instead: real Android boots, the APK is
                    installed with `pm install` and started from its launcher, in a live window
                    (`omni-linux`'s r_roblox session). --minutes bounds the session (default 30);
                    --size is the display's size at boot; --gpu is the device's GPU backend
                    (OMNI_GPU; auto: Vulkan on a host
                    with a Vulkan GPU, else the host's GLES); --with-systemui keeps SystemUI and
                    the launcher (default: a single-app device). The first session for an APK and
                    account saves its device once signed in (<session dir>/omni-golden); later
                    sessions boot a copy of it and open the place at once -- no install, no
                    first boot. --fresh-device makes a new device (the saved one is kept); --standby keeps the device waiting once launched (and in the
                    place): a place id written to <instance>/data/local/tmp/join-place is joined,
                    and <instance>/data/local/tmp/stop ends the session; --instance names the
                    instance's directory. With a warm device up (below) the session runs on it
                    instead -- no boot: the APK installed, a cookie a plugin names (OMNI_R_COOKIE)
                    put in the app's store before its first start, a place it names (OMNI_R_PLACE)
                    sent once the app's main Activity starts; the app is stopped when the
                    session ends, the device stays warm
                    (not with --instance, --standby or --fresh-device; OMNI_AOSP_WARM=0: never).
                    What is copied in the device (text, an image) is on the host's clipboard too,
                    to paste in any host application: only as plain text or a fresh PNG of the
                    image's pixels -- never a file, a script or a link to one -- with control and
                    invisible characters removed and no trailing Enter, and only while the
                    device's window is in use. --no-clipboard keeps the device's clipboard its own
                    (OMNI_CLIPBOARD=0)
  aosp --root      a rooted device (Magisk-compatible: su, modules over /system, resetprop);
                    --module installs the named modules (implies --root; built-in ids emu-hide,
                    shamiko, zygisk-frida need no catalog entry), --denylist a,b hides root from those
                    packages (implies --root).
                    NOTE: --module shamiko = whitelist mode: hides root from EVERY app not in --su
                    (use --denylist for one app); --module zygisk-frida is accepted but a NO-OP
                    until the P1 Zygisk host exists, --su all lets every
                    app su (default: root and shell only). A rooted device is its own saved
                    and warm device -- never one made without root or with other modules.
                    `omnidroid modules` lists the module catalog; `modules add <zip>` adds one.
  aosp --warm      a warm device: Android booted and idle with no app (no --apk), for apps a
                    host program installs, starts and stops on it through its control channel
                    (<instance>.ctl: a shell command in <id>.cmd, its output in <id>.out and exit
                    status in <id>.rc) -- the MCP server's install_apk and start_instance. Saved
                    once (<session dir>/omni-golden/base-...), booted from a copy after; ready when
                    <instance>/data/local/tmp/warm-ready is there. --minutes defaults to 720

  plugins           this user's plugins (<app-data>/../plugins, or OMNI_PLUGINS): what one game
                    needs -- options, variables, default arguments, hooks, commands -- declared
                    in a plugin's omnidroid-plugin.json. `add` copies a plugin's directory in,
                    `add --link` uses it where it is; `new` makes one to start from. The
                    repository's plugins/ holds plugins to install (plugins/roblox: --cookie,
                    --place, --join-delay, login); none is loaded until installed. The installed
                    plugins' options and commands are listed below";

struct Options {
    apk: Option<PathBuf>,
    minutes: u64,
    fresh: bool,
    phone: bool,
    data_dir: Option<PathBuf>,
    headless: bool,
    no_window: bool,
    control: Option<PathBuf>,
}

/// The command and the arguments after it. Options with no command in front of them mean
/// `play`: `omnidroid --apk <path> --minutes 30` is `omnidroid play --apk <path> --minutes 30`.
fn command_and_rest(args: impl Iterator<Item = String>) -> (Option<String>, Vec<String>) {
    let mut args: Vec<String> = args.collect();
    if args.is_empty() {
        return (None, args);
    }
    if args[0].starts_with("--") && !matches!(args[0].as_str(), "--help") {
        return (Some("play".to_string()), args);
    }
    let command = args.remove(0);
    (Some(command), args)
}

/// The installed plugins a session runs with, and the options of theirs it was given.
struct Plugged<'a> {
    plugins: &'a [&'a plugin::Plugin],
    given: Vec<plugin::Given>,
}

/// What every plugin program is told about this launcher.
fn plugin_base() -> plugin::Base {
    plugin::Base { repo: repo_root(), app_data: omni_platform::process::app_data_dir() }
}

/// `message`, and -- when it names an option or command the launcher does not know -- the
/// repository's plugin that adds it, not installed here.
fn with_hint(message: String) -> String {
    let unknown = message.split('`').nth(1).filter(|_| message.starts_with("unknown "));
    match unknown.and_then(|name| plugin::hint_for(name, &repo_root())) {
        Some(hint) => format!("{message}\n  {hint}"),
        None => message,
    }
}

fn main() -> ExitCode {
    let (command, rest) = command_and_rest(std::env::args().skip(1));
    // The helper a warm-device session leaves behind to stop its app once the session is gone.
    if command.as_deref() == Some("warm-release") {
        return warm::release(&rest);
    }
    if command.as_deref() == Some("modules") {
        return modules_command(&rest);
    }
    if command.as_deref() == Some("plugins") {
        return plugins_command(&rest);
    }
    let registry = plugin::Registry::installed();
    let plugins = match registry.active() {
        Ok(plugins) => plugins,
        Err(message) => {
            eprintln!("omnidroid: {message}");
            return ExitCode::from(2);
        }
    };
    let usage = || format!("{USAGE}{}", plugin::usage(&plugins));
    let refused = |message: String| {
        eprintln!("omnidroid: {}\n\n{}", with_hint(message), usage());
        ExitCode::from(2)
    };
    // A plugin's own command.
    if let Some(ran) = command.as_deref().and_then(|name| plugin::run_command(&plugins, name, &rest, &plugin_base())) {
        return match ran {
            Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            Err(message) => {
                eprintln!("omnidroid: {message}");
                ExitCode::FAILURE
            }
        };
    }
    match command.as_deref() {
        Some(session @ ("play" | "aosp")) => {
            let (rest, given) = match plugin::take_args(&plugins, session, rest) {
                Ok(split) => split,
                Err(message) => return refused(message),
            };
            let plugged = Plugged { plugins: &plugins, given };
            if session == "aosp" {
                match parse_aosp(rest.into_iter()) {
                    Ok(options) => aosp(&options, &plugged),
                    Err(message) => refused(message),
                }
            } else {
                match parse(rest.into_iter()) {
                    Ok(options) => play(&options, &plugged),
                    Err(message) => refused(message),
                }
            }
        }
        Some("which") => match parse(rest.into_iter()) {
            Ok(options) => which(&options),
            Err(message) => refused(message),
        },
        Some("-h" | "--help" | "help") => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        None => refused("no command".to_string()),
        Some(other) => refused(format!("unknown command `{other}`")),
    }
}

/// `omnidroid plugins [list | add [--link] <dir> | remove <name> | enable <name> | disable <name> | new <name> [<dir>]]`.
fn plugins_command(args: &[String]) -> ExitCode {
    let registry = plugin::Registry::installed();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match args.as_slice() {
        [] | ["list"] => {
            match &registry.dir {
                Some(dir) => println!("plugins in {}", dir.display()),
                None => println!("this host names no plugin directory (set OMNI_PLUGINS)"),
            }
            if registry.entries.is_empty() {
                println!("none installed (the repository's plugins/ holds some: omnidroid plugins add --link plugins/<name>)");
            }
            for entry in &registry.entries {
                let state = if entry.enabled { "on" } else { "off" };
                let from = if entry.linked { format!("linked to {}", entry.dir.display()) } else { entry.dir.display().to_string() };
                match &entry.plugin {
                    Ok(p) => println!("{}\t{}\t{state}\t{}\t{from}", p.name, p.version, p.description),
                    Err(error) => println!("{}\t?\t{state}\tBROKEN: {error}\t{from}", entry.name),
                }
            }
            Ok(String::new())
        }
        ["add", "--link", dir] | ["add", dir, "--link"] => plugin::add(&registry, Path::new(dir), true),
        ["add", dir] => plugin::add(&registry, Path::new(dir), false),
        ["remove", name] => plugin::remove(&registry, name),
        ["enable", name] => plugin::set_enabled(&registry, name, true),
        ["disable", name] => plugin::set_enabled(&registry, name, false),
        ["new", name] => plugin::scaffold(name, Path::new(name)),
        ["new", name, dir] => plugin::scaffold(name, Path::new(dir)),
        _ => {
            eprintln!("omnidroid: usage: omnidroid plugins [list | add [--link] <dir> | remove <name> | enable <name> | disable <name> | new <name> [<dir>]]");
            return ExitCode::from(2);
        }
    };
    match done {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("omnidroid: {message}");
            ExitCode::FAILURE
        }
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        apk: None,
        minutes: 0,
        fresh: false,
        phone: false,
        data_dir: None,
        headless: false,
        no_window: false,
        control: None,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--apk" => options.apk = Some(PathBuf::from(value("--apk")?)),
            "--minutes" => {
                let text = value("--minutes")?;
                options.minutes =
                    text.parse().map_err(|_| format!("--minutes wants a whole number, not `{text}`"))?;
            }
            "--fresh" => options.fresh = true,
            "--phone" => options.phone = true,
            "--data-dir" => options.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--headless" => options.headless = true,
            "--no-window" => {
                options.no_window = true;
                options.headless = true;
            }
            "--control" => options.control = Some(PathBuf::from(value("--control")?)),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(options)
}

/// The repository this launcher was built from: the gate it runs is that checkout's.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/omnidroid has two ancestors")
        .to_path_buf()
}

fn chosen(options: &Options) -> Result<omni_apk::ChosenApk, ExitCode> {
    omni_apk::choose_apk(options.apk.as_deref(), &repo_root()).map_err(|error| {
        eprintln!("omnidroid: {error}");
        ExitCode::FAILURE
    })
}

fn describe(apk: &omni_apk::ChosenApk) -> String {
    format!(
        "{} -- {} {} (versionCode {}), chosen by {}",
        apk.path.display(),
        apk.manifest.package,
        apk.manifest.version_name,
        apk.manifest.version_code,
        apk.chosen_by
    )
}

fn which(options: &Options) -> ExitCode {
    match chosen(options) {
        Ok(apk) => {
            println!("{}", describe(&apk));
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

fn play(options: &Options, plugged: &Plugged) -> ExitCode {
    let apk = match chosen(options) {
        Ok(apk) => apk,
        Err(code) => return code,
    };
    // Absolute, because the gate resolves it from its own working directory.
    let apk_path = std::fs::canonicalize(&apk.path).unwrap_or_else(|_| apk.path.clone());
    println!("Omnidroid: {}", describe(&apk));
    let info = plugin::SessionInfo {
        command: "play",
        fresh: options.fresh,
        given_data_dir: options.data_dir.as_deref(),
        apk: Some(&apk_path),
        package: Some(&apk.manifest.package),
    };
    let base = plugin_base();
    let session = match plugin::start(plugged.plugins, &plugged.given, &info, &base) {
        Ok(session) => session,
        Err(message) => {
            eprintln!("omnidroid: {message}");
            return ExitCode::from(2);
        }
    };

    let mut run = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    run.current_dir(repo_root())
        .args(["test", "-p", "omni-android", "--release", "--test", "gameactivity", "--"])
        .args(["--nocapture", "--test-threads=1", SESSION_TEST])
        .env(omni_apk::APK_ENV, &apk_path)
        .env("OMNI_M6_ROWS_21_22", "1")
        .env("OMNI_GFX_WINDOW_TESTS", "1");

    if options.phone {
        run.env_remove("OMNI_KEYBOARD_MOUSE");
        println!("Omnidroid: the phone configuration -- the mouse is a finger, and there is no keyboard");
    } else {
        run.env("OMNI_KEYBOARD_MOUSE", "1");
        println!("Omnidroid: this computer's keyboard and mouse (OMNI_KEYBOARD_MOUSE=1)");
    }

    let (seconds, length) = if options.minutes > 0 {
        (options.minutes * 60, format!("a {}-minute session", options.minutes))
    } else {
        (UNTIL_CLOSED_SECONDS, "a session until the window is closed".to_string())
    };
    run.env("OMNI_SESSION_SECONDS", seconds.to_string());

    if options.fresh {
        run.env_remove("OMNI_DATA_DIR");
        println!("Omnidroid: {length} on a fresh install");
    } else {
        // --data-dir, else the directory a plugin named (one account's own), else the host's.
        let Some(dir) = options.data_dir.clone().or_else(|| session.data_dir.clone()).or_else(omni_platform::process::app_data_dir) else {
            eprintln!(
                "omnidroid: this host names no app-data directory (its HOME or LOCALAPPDATA is \
                 unset); pass --data-dir, or --fresh"
            );
            return ExitCode::FAILURE;
        };
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("omnidroid: could not create {}: {error}", dir.display());
            return ExitCode::FAILURE;
        }
        run.env("OMNI_DATA_DIR", &dir);
        println!("Omnidroid: {length}; the app's storage is kept in {}", dir.display());
    }
    // Headless mode and its control channel (the gate reads them; stdin stays this process's).
    if let Ok(here) = std::env::current_dir() {
        run.env("OMNI_CONTROL_CWD", here);
    }
    if options.no_window {
        run.env("OMNI_NO_WINDOW", "1");
        println!("Omnidroid: no window -- the engine draws into an off-screen buffer (OMNI_NO_WINDOW=1)");
    } else {
        run.env_remove("OMNI_NO_WINDOW");
    }
    if options.headless {
        run.env("OMNI_HEADLESS", "1");
        println!("Omnidroid: headless -- frames are run and presented, and the GPU draws none of them");
    } else {
        run.env_remove("OMNI_HEADLESS");
    }
    if let Some(control) = &options.control {
        // Absolute, because the gate runs from its own working directory.
        let control = std::path::absolute(control).unwrap_or_else(|_| control.clone());
        run.env("OMNI_CONTROL", &control);
        println!(
            "Omnidroid: commands from {} and from stdin: headless on|off, screenshot <path>, status",
            control.display()
        );
    } else {
        run.env_remove("OMNI_CONTROL");
        println!("Omnidroid: commands from stdin: headless on|off, screenshot <path>, status");
    }
    // The plugins' variables last: what a plugin sets is what the session gets.
    session.apply(&mut run);
    println!("End the session by closing the window.");

    let code = match run.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("omnidroid: could not start cargo: {error}");
            plugin::end(plugged.plugins, &plugged.given, &info, &base, 1);
            return ExitCode::FAILURE;
        }
    };
    plugin::end(plugged.plugins, &plugged.given, &info, &base, code);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// `omnidroid aosp`'s options.
#[derive(Debug, PartialEq, Eq)]
struct AospOptions {
    apk: Option<PathBuf>,
    minutes: u64,
    size: Option<(u32, u32)>,
    gpu: Option<String>,
    with_systemui: bool,
    fresh_device: bool,
    standby: bool,
    warm: bool,
    instance: Option<PathBuf>,
    /// A rooted device (`--root`; `--module` implies it).
    root: bool,
    /// The root modules to install (`--module a,b,c`).
    modules: Vec<String>,
    /// Who may `su`: `all`, or comma-separated packages (`--su`).
    su: Option<String>,
    /// Packages hidden from root (`--denylist a,b`; implies root).
    denylist: Vec<String>,
    /// The device's clipboard kept from the host's (`--no-clipboard`); shared by default.
    no_clipboard: bool,
}

fn parse_aosp(mut args: impl Iterator<Item = String>) -> Result<AospOptions, String> {
    let mut options =
        AospOptions {
        apk: None,
        minutes: 30,
        size: None,
        gpu: None,
        with_systemui: false,
        fresh_device: false,
        standby: false,
        warm: false,
        instance: None,
        root: false,
        modules: Vec::new(),
        su: None,
        denylist: Vec::new(),
        no_clipboard: false,
    };
    let mut minutes_given = false;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--apk" => options.apk = Some(PathBuf::from(value("--apk")?)),
            "--minutes" => {
                minutes_given = true;
                let text = value("--minutes")?;
                options.minutes = text
                    .parse()
                    .ok()
                    .filter(|m| *m > 0)
                    .ok_or_else(|| format!("--minutes wants a whole number above 0, not `{text}`"))?;
            }
            "--size" => {
                let text = value("--size")?;
                let size = text
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                    .filter(|(w, h)| *w >= 320 && *h >= 240)
                    .ok_or_else(|| format!("--size wants <width>x<height> (at least 320x240), not `{text}`"))?;
                options.size = Some(size);
            }
            "--gpu" => {
                let text = value("--gpu")?;
                if !matches!(text.as_str(), "vulkan" | "gl" | "auto") {
                    return Err(format!("--gpu wants vulkan, gl or auto, not `{text}`"));
                }
                options.gpu = Some(text);
            }
            "--with-systemui" => options.with_systemui = true,
            "--no-clipboard" => options.no_clipboard = true,
            "--fresh-device" => options.fresh_device = true,
            "--standby" => options.standby = true,
            "--warm" => options.warm = true,
            "--instance" => options.instance = Some(PathBuf::from(value("--instance")?)),
            "--root" => options.root = true,
            "--module" => {
                let text = value("--module")?;
                options.modules.extend(text.split(',').map(str::trim).filter(|m| !m.is_empty()).map(str::to_string));
                options.root = true;
            }
            "--denylist" => {
                let text = value("--denylist")?;
                options.denylist.extend(text.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string));
                options.root = true;
            }
            "--su" => {
                options.su = Some(value("--su")?);
                options.root = true;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    // A warm device waits for apps: half a day unless told.
    if options.warm && !minutes_given {
        options.minutes = 720;
    }
    Ok(options)
}

/// The environment the real-AOSP session (`omni-linux`'s `r_roblox`) is given: what
/// `tools/aosp_play.ps1` sets on Windows, from these options. A plugin adds its own on top
/// (`OMNI_R_COOKIE`, `OMNI_R_PLACE`: the `roblox` plugin's).
fn aosp_env(options: &AospOptions, apk: &Path) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("OMNI_WINDOW", "1".to_string()),
        ("OMNI_R_MINUTES", options.minutes.to_string()),
        ("OMNI_TEST_APK", apk.display().to_string()),
        ("OMNI_R_KIOSK", if options.with_systemui { "0" } else { "1" }.to_string()),
    ];
    if let Some((w, h)) = options.size {
        env.push(("OMNI_WINDOW_SIZE", format!("{w}x{h}")));
    }
    if let Some(gpu) = &options.gpu {
        env.push(("OMNI_GPU", gpu.clone()));
    }
    if options.standby {
        env.push(("OMNI_R_STANDBY", "1".to_string()));
    }
    if options.warm {
        env.push(("OMNI_R_WARM", "1".to_string()));
    }
    if options.no_clipboard {
        env.push(("OMNI_CLIPBOARD", "0".to_string()));
    }
    if let Some(dir) = &options.instance {
        env.push(("OMNI_R_INSTANCE", dir.display().to_string()));
    }
    if options.root {
        env.push(("OMNI_R_ROOT", "1".to_string()));
        if !options.modules.is_empty() {
            env.push(("OMNI_R_MODULES", options.modules.join(",")));
        }
        if let Some(su) = &options.su {
            env.push(("OMNI_R_SU", su.clone()));
        }
        if !options.denylist.is_empty() {
            env.push(("OMNI_R_DENYLIST", options.denylist.join(",")));
        }
    }
    env
}

/// Every module id asked for must be in the catalog: said before anything boots.
fn validate_modules(ids: &[String], catalog: &omni_linux::root::Catalog) -> Result<(), String> {
    for id in ids {
        if !omni_linux::root::module::is_builtin(id) && catalog.find(id).is_none() {
            let known: Vec<&str> = catalog.list().iter().map(|m| m.prop.id.as_str()).collect();
            return Err(format!(
                "unknown module `{id}` (known: {}); `omnidroid modules` lists them, `omnidroid modules add <zip>` adds one",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            ));
        }
    }
    Ok(())
}

/// The module catalog: the repository's `modules/` and the user's directory.
fn module_catalog() -> Result<omni_linux::root::Catalog, String> {
    use omni_linux::root::module;
    omni_linux::root::Catalog::discover(&module::builtin_dir(&repo_root()), module::user_dir().as_deref())
}

/// `omnidroid modules [list | add <zip>]`.
fn modules_command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        None | Some("list") if args.len() <= 1 => match module_catalog() {
            Ok(catalog) => {
                if catalog.list().is_empty() {
                    println!("no modules (the repository's modules/ and the user directory are empty)");
                }
                for m in catalog.list() {
                    let source = match &m.source {
                        omni_linux::root::ModuleSource::Dir(p) | omni_linux::root::ModuleSource::Zip(p) => p.display().to_string(),
                    };
                    println!("{}\t{}\t{}\t{source}", m.prop.id, m.prop.name, m.prop.version);
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("omnidroid: {error}");
                ExitCode::FAILURE
            }
        },
        Some("add") if args.len() == 2 => match add_module(Path::new(&args[1])) {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("omnidroid: {error}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("omnidroid: usage: omnidroid modules [list | add <zip>]");
            ExitCode::from(2)
        }
    }
}

/// Copy a module zip into the user directory as `<id>.zip`, once its `module.prop` parses.
fn add_module(zip: &Path) -> Result<String, String> {
    let apk = omni_apk::Apk::open(zip).map_err(|e| format!("{}: not a zip: {e}", zip.display()))?;
    let bytes = apk.read_named("module.prop").map_err(|e| format!("{}: no module.prop: {e}", zip.display()))?;
    let prop = omni_linux::root::ModuleProp::parse(&String::from_utf8_lossy(&bytes)).map_err(|e| format!("{}: {e}", zip.display()))?;
    if module_catalog()?.find(&prop.id).is_some() {
        return Err(format!("module `{}` is already in the catalog", prop.id));
    }
    let dir = omni_linux::root::user_dir().ok_or("no user module directory (set OMNI_MODULES)")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let to = dir.join(format!("{}.zip", prop.id));
    std::fs::copy(zip, &to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(format!("added {} ({} {}) as {}", prop.id, prop.name, prop.version, to.display()))
}

/// Where the session's instance (the device's `/data`, its log, its screenshots) is made: the temp
/// directory, unless it is a tmpfs and nothing named one (`TMPDIR`) -- then `<app-data>/../aosp`, on
/// the disk. Ubuntu's `/tmp` is a tmpfs of half the RAM: the instance (an installed APK, ART's
/// compiled code, the app's data) would fill it and be held in memory beside the running device.
fn aosp_work_dir(tmpdir_named: bool, temp_is_tmpfs: bool, app_data: Option<&Path>) -> Option<PathBuf> {
    if tmpdir_named || !temp_is_tmpfs {
        return None;
    }
    Some(app_data?.parent()?.join("aosp"))
}

/// Whether `dir` is mounted as a tmpfs (`/proc/mounts`; false where there is none).
fn is_tmpfs(dir: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else { return false };
    let dir = dir.to_string_lossy();
    mounts.lines().filter_map(|l| {
        let mut f = l.split_whitespace();
        let (_, at, kind) = (f.next()?, f.next()?, f.next()?);
        Some((at.to_string(), kind == "tmpfs"))
    })
    .filter(|(at, _)| dir == at.as_str() || dir.starts_with(&format!("{}/", at.trim_end_matches('/'))))
    .max_by_key(|(at, _)| at.len())
    .is_some_and(|(_, tmpfs)| tmpfs)
}

/// `omnidroid aosp`: the real-AOSP session in a live window (see USAGE).
fn aosp(options: &AospOptions, plugged: &Plugged) -> ExitCode {
    // Root is checked before anything boots: an unknown module, or the Magisk assets not fetched
    // (`tools/fetch_magisk.py`), is an error here -- never a half-rooted device.
    let root_hash = if options.root {
        let checked = module_catalog().and_then(|catalog| validate_modules(&options.modules, &catalog)).and_then(|()| {
            omni_linux::root::key::request_with(&repo_root(), &options.modules, options.su.as_deref(), &options.denylist)
        });
        match checked {
            Ok(request) => Some(request.hash),
            Err(message) => {
                eprintln!("omnidroid: {message}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    // A warm device has no APK of its own.
    let (apk_path, package) = if options.warm {
        println!("Omnidroid (real AOSP): a warm device, no app");
        (PathBuf::new(), None)
    } else {
        let apk = match omni_apk::choose_apk(options.apk.as_deref(), &repo_root()) {
            Ok(apk) => apk,
            Err(error) => {
                eprintln!("omnidroid: {error}");
                return ExitCode::FAILURE;
            }
        };
        println!("Omnidroid (real AOSP): {}", describe(&apk));
        (std::fs::canonicalize(&apk.path).unwrap_or_else(|_| apk.path.clone()), Some(apk.manifest.package))
    };
    let info = plugin::SessionInfo {
        command: "aosp",
        fresh: options.fresh_device,
        given_data_dir: None,
        apk: (!options.warm).then_some(apk_path.as_path()),
        package: package.as_deref(),
    };
    let base = plugin_base();
    let session = match plugin::start(plugged.plugins, &plugged.given, &info, &base) {
        Ok(session) => session,
        Err(message) => {
            eprintln!("omnidroid: {message}");
            return ExitCode::from(2);
        }
    };
    // The account's cookie file and the place to join, when a plugin names them: the warm session
    // reads them as the cold one (`r_roblox`) does.
    let cookie = session.var("OMNI_R_COOKIE").map(PathBuf::from);
    let place = match session.var("OMNI_R_PLACE").map(str::parse::<u64>).transpose() {
        Ok(place) => place,
        Err(_) => {
            eprintln!("omnidroid: OMNI_R_PLACE is not a place id");
            return ExitCode::from(2);
        }
    };
    // A warm device is up: the session is an app on it -- no boot, no device of its own (a second
    // device beside it would only take the host's memory). Not for a session that names its own
    // device (--instance, --standby, --fresh-device), nor with OMNI_AOSP_WARM=0.
    if !options.warm && !options.standby && !options.fresh_device && options.instance.is_none() {
        if let Some(dev) = warm::usable(root_hash.as_deref()) {
            if options.size.is_some() || options.gpu.is_some() || options.with_systemui || options.no_clipboard {
                println!("Omnidroid: --size, --gpu, --with-systemui and --no-clipboard are the warm device's own (set when it booted); not applied");
            }
            let code = warm::session(&dev, &repo_root(), &apk_path, cookie.as_deref(), place, options.minutes);
            plugin::end(plugged.plugins, &plugged.given, &info, &base, i32::from(code != ExitCode::SUCCESS));
            return code;
        }
    }
    // The session (cargo, the test, the device's host processes) ends with this process, however it
    // ends: a caller that kills the launcher (the MCP server) leaves nothing running.
    omni_platform::process::hold_children();
    let mut run = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    run.current_dir(repo_root())
        .args(["test", "--release", "-q", "-p", "omni-linux", "--test", "r_roblox", "--", "--ignored", "--nocapture"]);
    for (k, v) in aosp_env(options, &apk_path) {
        run.env(k, v);
    }
    session.apply(&mut run);
    let temp = std::env::temp_dir();
    let work = aosp_work_dir(std::env::var_os("TMPDIR").is_some(), is_tmpfs(&temp), omni_platform::process::app_data_dir().as_deref());
    let session_dir = match &work {
        Some(dir) => {
            if let Err(error) = std::fs::create_dir_all(dir) {
                eprintln!("omnidroid: could not create {}: {error}", dir.display());
                return ExitCode::FAILURE;
            }
            run.env("TMPDIR", dir);
            dir.clone()
        }
        None => temp,
    };
    // The saved devices (r_roblox's OMNI_R_GOLDEN), beside the sessions.
    if !options.fresh_device {
        run.env("OMNI_R_GOLDEN", session_dir.join("omni-golden"));
    }
    // Graphics buffers a finished session handed between its processes (`omni-shm-*`).
    for dir in [session_dir.clone(), PathBuf::from("/dev/shm")] {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("omni-shm-")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    println!(
        "Omnidroid: a {}-minute session; the log is {}/omni-linux-r-<pid>.log, the display's screenshots beside it",
        options.minutes,
        session_dir.display()
    );
    let code = match run.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("omnidroid: could not start cargo: {error}");
            1
        }
    };
    plugin::end(plugged.plugins, &plugged.given, &info, &base, code);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aosp_reads_root_and_modules() {
        let parse = |a: &[&str]| parse_aosp(a.iter().map(|s| (*s).to_string()));
        let o = parse(&["--module", "zygisk-frida,emu-hide", "--su", "all"]).unwrap();
        assert!(o.root, "--module implies --root");
        assert_eq!(o.modules, vec!["zygisk-frida".to_string(), "emu-hide".to_string()]);
        assert_eq!(o.su.as_deref(), Some("all"));
        let o = parse(&["--root"]).unwrap();
        assert!(o.root && o.modules.is_empty() && o.su.is_none());
        let o = parse(&[]).unwrap();
        assert!(!o.root && o.modules.is_empty(), "unrooted by default");
        let o = parse(&["--su", "all"]).unwrap();
        assert!(o.root && o.su.as_deref() == Some("all"), "--su implies --root");
    }

    #[test]
    fn unknown_module_id_is_rejected() {
        let empty = omni_linux::root::Catalog { modules: Vec::new() };
        let err = validate_modules(&["ghost".to_string()], &empty).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
        assert!(validate_modules(&[], &empty).is_ok());
    }

    #[test]
    fn root_options_reach_the_session_environment_and_unrooted_adds_none() {
        let parse = |a: &[&str]| parse_aosp(a.iter().map(|s| (*s).to_string())).unwrap();
        let env = aosp_env(&parse(&["--module", "a,b", "--su", "all"]), Path::new("x.apk"));
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("OMNI_R_ROOT"), get("OMNI_R_MODULES"), get("OMNI_R_SU")), (Some("1"), Some("a,b"), Some("all")));
        let plain = aosp_env(&parse(&[]), Path::new("x.apk"));
        assert!(plain.iter().all(|(n, _)| !n.starts_with("OMNI_R_ROOT") && *n != "OMNI_R_MODULES" && *n != "OMNI_R_SU"));
    }

    #[test]
    fn builtin_modules_and_denylist_parse() {
        let parse = |a: &[&str]| parse_aosp(a.iter().map(|s| (*s).to_string())).unwrap();
        let o = parse(&["--module", "emu-hide,shamiko", "--denylist", "com.roblox.client"]);
        assert!(o.root && o.modules == vec!["emu-hide", "shamiko"] && o.denylist == vec!["com.roblox.client"]);
        let o = parse(&["--denylist", "a.b, c.d"]);
        assert!(o.root, "--denylist implies --root");
        assert_eq!(o.denylist, vec!["a.b", "c.d"]);
        let empty = omni_linux::root::Catalog { modules: Vec::new() };
        assert!(validate_modules(&["emu-hide".into(), "shamiko".into(), "zygisk-frida".into()], &empty).is_ok());
        let err = validate_modules(&["ghost".into()], &empty).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
        let env = aosp_env(&parse(&["--denylist", "a.b,c.d"]), Path::new("x.apk"));
        assert!(env.iter().any(|(n, v)| *n == "OMNI_R_DENYLIST" && v == "a.b,c.d"));
        let plain = aosp_env(&parse(&[]), Path::new("x.apk"));
        assert!(plain.iter().all(|(n, _)| *n != "OMNI_R_DENYLIST"));
    }

    #[test]
    fn options_without_a_command_mean_play() {
        let split = |a: &[&str]| command_and_rest(a.iter().map(|s| (*s).to_string()));
        let (command, rest) = split(&["--cookie", "c.txt", "--place", "1"]);
        assert_eq!(command.as_deref(), Some("play"));
        assert_eq!(rest, ["--cookie", "c.txt", "--place", "1"]);
        let (command, rest) = split(&["login", "name"]);
        assert_eq!((command.as_deref(), rest.as_slice()), (Some("login"), &["name".to_string()][..]));
        assert_eq!(split(&[]).0, None);
    }

    #[test]
    fn an_option_of_a_plugin_not_installed_names_the_plugin() {
        let said = with_hint("unknown argument `--cookie`".to_string());
        assert!(said.contains("`roblox` plugin") && said.contains("plugins add --link"), "{said}");
        assert_eq!(with_hint("unknown argument `--nothing`".to_string()), "unknown argument `--nothing`");
        assert!(with_hint("unknown command `login`".to_string()).contains("roblox"));
    }

    #[test]
    fn the_repository_roblox_plugin_loads_and_takes_cookie_and_place() {
        let roblox = plugin::Plugin::load(&repo_root().join("plugins/roblox")).expect("plugins/roblox loads");
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let (rest, given) = plugin::take_args(&[&roblox], "aosp", args(&["--cookie", "c.txt", "--place", "8737899170", "--gpu", "gl"])).unwrap();
        assert_eq!(rest, ["--gpu", "gl"]);
        assert_eq!(given.iter().map(|g| g.flag.as_str()).collect::<Vec<_>>(), ["--cookie", "--place"]);
        assert!(plugin::take_args(&[&roblox], "play", args(&["--place", "0"])).is_err());
        assert!(plugin::take_args(&[&roblox], "aosp", args(&["--join-delay", "3"])).unwrap().1.is_empty(), "play's only");
    }

    #[test]
    fn headless_and_the_control_file_are_options_of_play() {
        let parsed = |a: &[&str]| parse(a.iter().map(|s| (*s).to_string()));
        let options = parsed(&["--headless", "--control", "cmds.txt"]).expect("parsed");
        assert!(options.headless);
        assert_eq!(options.control.as_deref(), Some(Path::new("cmds.txt")));
        let plain = parsed(&[]).expect("parsed");
        assert!(!plain.headless && plain.control.is_none());
        assert!(parsed(&["--control"]).is_err(), "--control needs a file");
        let windowless = parsed(&["--no-window"]).expect("parsed");
        assert!(windowless.no_window && windowless.headless, "--no-window is headless");
    }

    #[test]
    fn aosp_takes_the_apk_and_the_device() {
        let args = |a: &[&str]| parse_aosp(a.iter().map(|s| (*s).to_string()));
        let o = args(&["--apk", "r.apk"]).expect("parsed");
        assert_eq!(o.apk.as_deref(), Some(Path::new("r.apk")));
        assert_eq!(o.minutes, 30);
        assert!(args(&["--cookie", "c.txt"]).is_err() && args(&["--place", "1"]).is_err(), "the roblox plugin's, not the launcher's");
        let o = args(&["--minutes", "12", "--size", "1280x720", "--gpu", "gl", "--with-systemui"]).expect("parsed");
        assert_eq!((o.minutes, o.size, o.gpu.as_deref(), o.with_systemui), (12, Some((1280, 720)), Some("gl"), true));
        assert!(args(&["--gpu", "metal"]).is_err());
        assert!(args(&["--size", "1280"]).is_err());
        assert!(args(&["--minutes", "0"]).is_err());
        assert!(args(&["--fresh"]).is_err(), "play's options are not aosp's");
        let o = args(&["--fresh-device", "--standby", "--instance", "d"]).expect("parsed");
        assert_eq!((o.fresh_device, o.standby, o.instance.as_deref()), (true, true, Some(Path::new("d"))));
        let env = aosp_env(&o, Path::new("r.apk"));
        assert!(env.contains(&("OMNI_R_STANDBY", "1".to_string())) && env.contains(&("OMNI_R_INSTANCE", "d".to_string())));
    }

    #[test]
    fn aosp_gives_the_session_what_aosp_play_ps1_gave_it() {
        let o = parse_aosp(["--gpu", "gl", "--size", "960x540"].iter().map(|s| (*s).to_string())).expect("parsed");
        let env = aosp_env(&o, Path::new("/a/r.apk"));
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("OMNI_WINDOW"), Some("1"));
        assert_eq!(get("OMNI_R_MINUTES"), Some("30"));
        assert_eq!(get("OMNI_TEST_APK"), Some("/a/r.apk"));
        assert_eq!(get("OMNI_R_KIOSK"), Some("1"), "a single-app device unless --with-systemui");
        assert_eq!(get("OMNI_WINDOW_SIZE"), Some("960x540"));
        assert_eq!(get("OMNI_GPU"), Some("gl"));
        let bare = aosp_env(&parse_aosp(std::iter::empty()).expect("parsed"), Path::new("r.apk"));
        assert!(bare.iter().all(|(k, _)| !matches!(*k, "OMNI_R_COOKIE" | "OMNI_R_PLACE" | "OMNI_GPU" | "OMNI_R_STANDBY" | "OMNI_R_INSTANCE")));
    }

    #[test]
    fn the_clipboard_is_shared_unless_no_clipboard() {
        let env = |a: &[&str]| aosp_env(&parse_aosp(a.iter().map(|s| (*s).to_string())).expect("parsed"), Path::new("r.apk"));
        assert!(env(&[]).iter().all(|(k, _)| *k != "OMNI_CLIPBOARD"), "shared by default");
        assert!(env(&["--no-clipboard"]).contains(&("OMNI_CLIPBOARD", "0".to_string())));
    }

    #[test]
    fn a_tmpfs_temp_directory_moves_the_session_to_the_disk() {
        let data = Path::new("/home/u/.local/share/omnidroid/data");
        assert_eq!(aosp_work_dir(false, true, Some(data)), Some(PathBuf::from("/home/u/.local/share/omnidroid/aosp")));
        assert_eq!(aosp_work_dir(true, true, Some(data)), None, "TMPDIR named is kept");
        assert_eq!(aosp_work_dir(false, false, Some(data)), None, "a temp directory on a disk is kept");
    }
}
