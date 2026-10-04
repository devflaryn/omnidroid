//! `omnidroid`: run an APK in a real window for a person to use, on Windows, macOS or Linux.
//!
//! ```text
//! cargo run --release -p omnidroid -- play                      # the newest APK in the repository root
//! cargo run --release -p omnidroid -- play --apk Roblox-2.738.1397.apk
//! cargo run --release -p omnidroid -- play --minutes 90 --fresh --phone
//! cargo run --release -p omnidroid -- play --place 8737899170     # sign-in kept, then join the place
//! cargo run --release -p omnidroid -- play --cookie farm4.txt --place 8737899170  # sign in as that account
//! cargo run --release -p omnidroid -- login                     # sign in in Chromium, keep the cookie
//! cargo run --release -p omnidroid -- login <username> <password>
//! cargo run --release -p omnidroid -- which [--apk <path>]      # say which APK would run, and exit
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
//! * **`--place <id>` joins that place** once the app's own saved sign-in has reached Home
//!   (`OMNI_JOIN_PLACE`, `--join-delay` seconds in, default 20): the gate calls
//!   `nativeAppBridgeV2StartGameWithParam` as the app's Play button does. It needs a signed-in data
//!   directory -- with `--fresh` there is no session to join with -- or `--cookie`.
//! * **`--cookie <file or value>` signs in as that account**: its `.ROBLOSECURITY` value, as a
//!   file (the bare value, `.ROBLOSECURITY=<value>`, or a Netscape `cookies.txt`) or the value
//!   itself. It is put in the app's own cookie store before start (`OMNI_COOKIE`), where a kept
//!   sign-in lives, and never printed. Each account keeps its own storage --
//!   `<app-data>/../accounts/<file name>` unless `--data-dir` -- so one account's sign-in never
//!   replaces another's, and the default directory's is left alone. A name that is not a file is
//!   looked up in the cookies folder (`<app-data>/../cookies/<name>.txt`, where `login` keeps them).
//!   **A cookie Roblox has since replaced is kept**: Roblox rotates the session cookie while the
//!   app runs, and the store keeps the new one. The file's cookie is planted only when it is not the
//!   one last planted in that account's storage (`omnidroid-cookie-planted` there), so an unchanged
//!   file never puts a dead cookie back over a live one.
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
//! * **`login` signs in in a real browser and keeps the session** (`tools/login.py`, Selenium with
//!   Chromium in a fresh profile): with no arguments the person signs in; with a username and a
//!   password the form is filled and submitted; with a username alone, the password `login` kept
//!   for it is used. Whatever the page asks (a captcha, a 2-step code) is answered in the window.
//!   Once the page reaches /home, `<cookies>/<name>.txt` (the cookie) and `<name>.login.json` (the
//!   username and password, in plain text, readable by this user only) are written. The Python
//!   environment it runs in is made the first time (`<app-data>/../login-venv`), Selenium included,
//!   and the browser is omnidroid's own Chromium (Chrome for Testing), which Selenium Manager fetches
//!   with its driver into `<app-data>/../chromium` -- not an installed application.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod warm;

/// The gate test whose run is a session: `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`.
const SESSION_TEST: &str = "initialize_native_code_returns_a_native_code_and_the_game_thread_starts";
/// "No limit": ten years, the gate's way of saying until the window is closed.
const UNTIL_CLOSED_SECONDS: u64 = 315_360_000;

const USAGE: &str = "\
usage: omnidroid [play] [--apk <path>] [--cookie <file|value>] [--place <id>] [--join-delay <s>]
                       [--minutes <n>] [--fresh] [--phone] [--data-dir <dir>]
                       [--headless] [--no-window] [--control <file>]
       omnidroid aosp [--apk <path>] [--cookie <file|name>] [--place <id>] [--minutes <n>]
                      [--size <w>x<h>] [--gpu vulkan|gl|auto] [--with-systemui]
                      [--fresh-device] [--standby] [--instance <dir>]
                      [--root] [--module <id,id,...>] [--su all|<pkg,pkg>]
       omnidroid modules [list | add <zip>]
       omnidroid aosp --warm [--instance <dir>] [--minutes <n>] [--size <w>x<h>] [--gpu vulkan|gl|auto]
       omnidroid which [--apk <path>]
       omnidroid login [<username> [<password>]] [--dir <dir>]

  --apk <path>      the APK to run (else OMNI_APK, else the newest *.apk in the repository root)
  --cookie <c>      sign in as this account: a file holding its .ROBLOSECURITY cookie, a name
                    `login` saved (<app-data>/../cookies/<name>.txt), or the value itself;
                    storage is kept per account (<app-data>/../accounts/<name>)
  --place <id>      join this place once the sign-in reaches Home (OMNI_JOIN_PLACE)
  --join-delay <s>  seconds after start before joining (default 20; OMNI_JOIN_DELAY)
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
                    (`omni-linux`'s r_roblox session). --cookie is a file or a saved name, planted
                    in the app's own cookie store; --place opens the place's deep link once signed
                    in; --minutes bounds the session (default 30); --size is the display's size at
                    boot; --gpu is the device's GPU backend (OMNI_GPU; auto: Vulkan on a host
                    with a Vulkan GPU, else the host's GLES); --with-systemui keeps SystemUI and
                    the launcher (default: a single-app device). The first session for an APK and
                    account saves its device once signed in (<session dir>/omni-golden); later
                    sessions boot a copy of it and open the place at once -- no install, no
                    first boot, no cookie planted. --fresh-device makes a new device (the saved
                    one is kept); --standby keeps the device waiting once launched (and in the
                    place): a place id written to <instance>/data/local/tmp/join-place is joined,
                    and <instance>/data/local/tmp/stop ends the session; --instance names the
                    instance's directory. With a warm device up (below) the session runs on it
                    instead -- no boot: the APK installed, the cookie put in the app's store
                    before its first start, the place's link sent once the app's main Activity
                    starts; the app is stopped when the session ends, the device stays warm
                    (not with --instance, --standby or --fresh-device; OMNI_AOSP_WARM=0: never)
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

  login             sign in to Roblox in Chromium and keep the cookie, username and password in
                    <dir> (default <app-data>/../cookies): no arguments -- you sign in; a username
                    and password -- they are entered for you; a username alone -- its saved password";

struct Options {
    apk: Option<PathBuf>,
    cookie: Option<String>,
    place: Option<u64>,
    join_delay: Option<f32>,
    minutes: u64,
    fresh: bool,
    phone: bool,
    data_dir: Option<PathBuf>,
    headless: bool,
    no_window: bool,
    control: Option<PathBuf>,
}

/// The command and the arguments after it. Options with no command in front of them mean
/// `play`: `omnidroid --cookie <file> --place <id>` is how the owner starts a game.
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

fn main() -> ExitCode {
    let (command, rest) = command_and_rest(std::env::args().skip(1));
    // The helper a warm-device session leaves behind to stop its app once the session is gone.
    if command.as_deref() == Some("warm-release") {
        return warm::release(&rest);
    }
    if command.as_deref() == Some("modules") {
        return modules_command(&rest);
    }
    let args = rest.into_iter();
    if command.as_deref() == Some("aosp") {
        return match parse_aosp(args) {
            Ok(aosp_options) => aosp(&aosp_options),
            Err(message) => {
                eprintln!("omnidroid: {message}\n\n{USAGE}");
                ExitCode::from(2)
            }
        };
    }
    if command.as_deref() == Some("login") {
        return match parse_login(args) {
            Ok(login_options) => login(&login_options),
            Err(message) => {
                eprintln!("omnidroid: {message}\n\n{USAGE}");
                ExitCode::from(2)
            }
        };
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("omnidroid: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match command.as_deref() {
        Some("play") => play(&options),
        Some("which") => which(&options),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!(
                "omnidroid: {}\n\n{USAGE}",
                other.map_or_else(|| "no command".to_string(), |c| format!("unknown command `{c}`"))
            );
            ExitCode::from(2)
        }
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        apk: None,
        cookie: None,
        place: None,
        join_delay: None,
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
            "--cookie" => options.cookie = Some(value("--cookie")?),
            "--minutes" => {
                let text = value("--minutes")?;
                options.minutes =
                    text.parse().map_err(|_| format!("--minutes wants a whole number, not `{text}`"))?;
            }
            "--place" => {
                let text = value("--place")?;
                let place: u64 =
                    text.parse().map_err(|_| format!("--place wants a numeric placeId, not `{text}`"))?;
                if place == 0 {
                    return Err("--place wants a placeId above 0".to_string());
                }
                options.place = Some(place);
            }
            "--join-delay" => {
                let text = value("--join-delay")?;
                options.join_delay = Some(
                    text.parse()
                        .ok()
                        .filter(|s: &f32| s.is_finite() && *s >= 0.0)
                        .ok_or_else(|| format!("--join-delay wants seconds, not `{text}`"))?,
                );
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

struct LoginOptions {
    username: Option<String>,
    password: Option<String>,
    dir: Option<PathBuf>,
}

fn parse_login(mut args: impl Iterator<Item = String>) -> Result<LoginOptions, String> {
    let mut options = LoginOptions { username: None, password: None, dir: None };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => options.dir = Some(PathBuf::from(args.next().ok_or("--dir needs a value")?)),
            flag if flag.starts_with("--") => return Err(format!("unknown argument `{flag}`")),
            _ if options.username.is_none() => options.username = Some(arg),
            _ if options.password.is_none() => options.password = Some(arg),
            _ => return Err(format!("login takes a username and a password, and `{arg}` is a third")),
        }
    }
    Ok(options)
}

/// `omnidroid login`: `tools/login.py` in the Python environment made for it. See the module docs.
fn login(options: &LoginOptions) -> ExitCode {
    let Some(dir) = options.dir.clone().or_else(cookies_dir) else {
        eprintln!("omnidroid: this host names no app-data directory (its HOME or LOCALAPPDATA is unset); pass --dir");
        return ExitCode::FAILURE;
    };
    // A username alone: the password `login` kept for it.
    let password = match (&options.username, &options.password) {
        (Some(username), None) => match saved_password(&dir, username) {
            Ok(password) => Some(password),
            Err(message) => {
                eprintln!("omnidroid: {message}");
                return ExitCode::from(2);
            }
        },
        (_, password) => password.clone(),
    };
    let python = match login_python() {
        Ok(python) => python,
        Err(message) => {
            eprintln!("omnidroid: {message}");
            return ExitCode::FAILURE;
        }
    };
    let mut run = Command::new(&python);
    run.arg(repo_root().join("tools").join("login.py")).arg("--dir").arg(&dir);
    // omnidroid's own Chromium and driver live beside its other data, not in a shared cache.
    if let Some(chromium) = omni_platform::process::app_data_dir().and_then(|d| Some(d.parent()?.join("chromium"))) {
        run.env("SE_CACHE_PATH", chromium);
    }
    if let Some(username) = &options.username {
        run.args(["--username", username]);
    }
    // The password in the child's environment, not on its command line.
    match &password {
        Some(password) => run.env("OMNI_LOGIN_PASSWORD", password),
        None => run.env_remove("OMNI_LOGIN_PASSWORD"),
    };
    match run.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(error) => {
            eprintln!("omnidroid: could not start {}: {error}", python.display());
            ExitCode::FAILURE
        }
    }
}

/// The password `login` kept for `username` in `dir` (`<username>.login.json`).
fn saved_password(dir: &Path, username: &str) -> Result<String, String> {
    let file = dir.join(format!("{username}.login.json"));
    let text = std::fs::read_to_string(&file).map_err(|error| {
        format!("no saved login for `{username}` ({}: {error}); pass its password too", file.display())
    })?;
    json_string_field(&text, "password")
        .filter(|password| !password.is_empty())
        .ok_or_else(|| format!("{} holds no password; pass it", file.display()))
}

/// The string value of `"field"` in the flat JSON object `login.py` writes -- enough of JSON for
/// that file (escapes included) without a JSON crate in the launcher.
fn json_string_field(text: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let after = text[text.find(&key)? + key.len()..].trim_start().strip_prefix(':')?.trim_start();
    let mut chars = after.strip_prefix('"')?.chars();
    let mut value = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                'b' => value.push('\u{8}'),
                'f' => value.push('\u{c}'),
                'u' => {
                    let code: String = chars.by_ref().take(4).collect();
                    value.push(char::from_u32(u32::from_str_radix(&code, 16).ok()?)?);
                }
                other => value.push(other),
            },
            c => value.push(c),
        }
    }
    None
}

/// The Python that runs `tools/login.py`, with Selenium: a virtual environment in
/// `<app-data>/../login-venv`, made (and given pip and Selenium) the first time.
fn login_python() -> Result<PathBuf, String> {
    let venv = omni_platform::process::app_data_dir()
        .and_then(|dir| Some(dir.parent()?.join("login-venv")))
        .ok_or("this host names no app-data directory for the login environment")?;
    // The two layouts `venv` makes: Windows' and everyone else's.
    let in_venv = || {
        [venv.join("Scripts").join("python.exe"), venv.join("bin").join("python3"), venv.join("bin").join("python")]
            .into_iter()
            .find(|candidate| candidate.is_file())
    };
    let works = |python: &Path, check: &[&str]| {
        Command::new(python).args(check).output().is_ok_and(|output| output.status.success())
    };
    let python = match in_venv() {
        Some(python) => python,
        None => {
            let host = ["python3", "python", "py"]
                .into_iter()
                .find(|name| works(Path::new(name), &["-c", "import sys; assert sys.version_info >= (3, 9)"]))
                .ok_or("login needs Python 3.9 or later (python3, python or py on PATH)")?;
            println!("Omnidroid: making the login environment in {} (once)", venv.display());
            let made = works(Path::new(host), &["-m", "venv", &venv.to_string_lossy()])
                // Without `ensurepip` (Debian and Ubuntu without python3-venv): no pip, fetched below.
                || works(Path::new(host), &["-m", "venv", "--without-pip", &venv.to_string_lossy()]);
            if !made {
                return Err(format!("{host} -m venv {} failed", venv.display()));
            }
            in_venv().ok_or_else(|| format!("{} holds no Python after `venv`", venv.display()))?
        }
    };
    if !works(&python, &["-m", "pip", "--version"]) {
        println!("Omnidroid: fetching pip into the login environment");
        let get_pip = "import urllib.request; exec(urllib.request.urlopen('https://bootstrap.pypa.io/get-pip.py').read())";
        let status = Command::new(&python).args(["-c", get_pip, "--quiet"]).status();
        if !status.is_ok_and(|s| s.success()) {
            return Err("pip could not be installed in the login environment".to_string());
        }
    }
    if !works(&python, &["-c", "import selenium"]) {
        println!("Omnidroid: installing Selenium into the login environment");
        let status = Command::new(&python).args(["-m", "pip", "install", "--quiet", "--disable-pip-version-check", "selenium"]).status();
        if !status.is_ok_and(|s| s.success()) {
            return Err("Selenium could not be installed in the login environment".to_string());
        }
    }
    Ok(python)
}

/// The account `--cookie` names: its `.ROBLOSECURITY` value (never printed) and the name its
/// storage is kept under.
struct AccountCookie {
    value: String,
    account: String,
}

/// `--cookie <file or value>`. An existing file is read; anything else long enough to be a cookie
/// is the value itself, and a short one is a file name that was not found (a real
/// `.ROBLOSECURITY` is several hundred characters).
fn account_cookie(arg: &str) -> Result<AccountCookie, String> {
    account_cookie_in(arg, cookies_dir().as_deref())
}

/// [`account_cookie`], with the folder a bare name is looked up in.
fn account_cookie_in(arg: &str, cookies: Option<&Path>) -> Result<AccountCookie, String> {
    let saved = cookies.and_then(|dir| {
        [dir.join(arg), dir.join(format!("{arg}.txt"))].into_iter().find(|candidate| candidate.is_file())
    });
    let path = if Path::new(arg).is_file() { Path::new(arg) } else { saved.as_deref().unwrap_or(Path::new(arg)) };
    if path.is_file() {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("--cookie: could not read {}: {error}", path.display()))?;
        let value = cookie_value(&text)
            .ok_or_else(|| format!("--cookie: {} holds no .ROBLOSECURITY cookie", path.display()))?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(AccountCookie { value, account: account_name(&stem) });
    }
    if arg.len() < 100 {
        return Err(format!(
            "--cookie: no file `{arg}` (from {}{}), and it is too short to be a .ROBLOSECURITY value",
            std::env::current_dir().map_or_else(|_| "here".to_string(), |d| d.display().to_string()),
            cookies.map_or_else(String::new, |dir| format!(", or in {}", dir.display()))
        ));
    }
    let value = cookie_value(arg).ok_or_else(|| "--cookie: that is not a .ROBLOSECURITY value".to_string())?;
    let hash = fingerprint(&value);
    Ok(AccountCookie { value, account: format!("cookie-{hash}") })
}

/// A stable name for a cookie that does not carry it: FNV-1a 64, in hex.
fn fingerprint(value: &str) -> String {
    let hash = value
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{hash:016x}")
}

/// Where `login` keeps cookies and `--cookie <name>` finds them: `<app-data>/../cookies`.
fn cookies_dir() -> Option<PathBuf> {
    Some(omni_platform::process::app_data_dir()?.parent()?.join("cookies"))
}

/// The file in an account's storage naming the cookie last planted there.
const PLANTED_MARKER: &str = "omnidroid-cookie-planted";

/// Whether `value` is to be planted in the kept storage `dir`: not when it is the cookie last
/// planted there -- the store then holds that one, or the one Roblox replaced it with.
fn cookie_is_new_for(dir: &Path, value: &str) -> bool {
    std::fs::read_to_string(dir.join(PLANTED_MARKER)).map_or(true, |kept| kept.trim() != fingerprint(value))
}

/// The `.ROBLOSECURITY` value in `text`: a Netscape `cookies.txt` line naming it, a
/// `.ROBLOSECURITY=<value>` pair (a `Cookie` header included), or else the first non-empty line as
/// the bare value. `None` for nothing a cookie store could hold.
fn cookie_value(text: &str) -> Option<String> {
    let text = text.trim_start_matches('\u{feff}');
    let lines = || text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let netscape = lines().find_map(|line| {
        let fields: Vec<&str> = line.split('\t').collect();
        (fields.len() == 7 && fields[5] == ".ROBLOSECURITY").then(|| fields[6].to_string())
    });
    let pair = || {
        text.split([';', '\n', '\r'])
            .find_map(|part| part.trim().strip_prefix(".ROBLOSECURITY=").map(|v| v.trim().to_string()))
    };
    let value = netscape.or_else(pair).or_else(|| lines().next().map(str::to_string))?;
    let value = value.trim_matches('"').to_string();
    (!value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control() || c == ';'))
        .then_some(value)
}

/// A file name made safe as a directory name on every host.
fn account_name(stem: &str) -> String {
    let name: String =
        stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if name.is_empty() {
        "account".to_string()
    } else {
        name
    }
}

fn play(options: &Options) -> ExitCode {
    let cookie = match options.cookie.as_deref().map(account_cookie).transpose() {
        Ok(cookie) => cookie,
        Err(message) => {
            eprintln!("omnidroid: {message}");
            return ExitCode::from(2);
        }
    };
    if options.place.is_some() && options.fresh && cookie.is_none() {
        eprintln!(
            "omnidroid: --place joins with the saved sign-in, and --fresh starts without one; \
             drop --fresh (sign in once first), or pass --cookie, to join"
        );
        return ExitCode::from(2);
    }
    let apk = match chosen(options) {
        Ok(apk) => apk,
        Err(code) => return code,
    };
    // Absolute, because the gate resolves it from its own working directory.
    let apk_path = std::fs::canonicalize(&apk.path).unwrap_or_else(|_| apk.path.clone());
    println!("Omnidroid: {}", describe(&apk));

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

    // Whether the account's cookie goes into the store at start: always on a fresh install, and in
    // kept storage only when it is not the one last planted there (see `cookie_is_new_for`).
    let mut plant = true;
    if options.fresh {
        run.env_remove("OMNI_DATA_DIR");
        println!("Omnidroid: {length} on a fresh install");
    } else {
        // An account named by --cookie keeps its own storage beside the default one.
        let default_dir = || {
            let dir = omni_platform::process::app_data_dir()?;
            Some(match &cookie {
                Some(cookie) => dir.parent()?.join("accounts").join(&cookie.account),
                None => dir,
            })
        };
        let Some(dir) = options.data_dir.clone().or_else(default_dir) else {
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
        if let Some(cookie) = &cookie {
            plant = cookie_is_new_for(&dir, &cookie.value);
            if plant {
                if let Err(error) = std::fs::write(dir.join(PLANTED_MARKER), fingerprint(&cookie.value)) {
                    eprintln!("omnidroid: could not write {}: {error}", dir.join(PLANTED_MARKER).display());
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    if let Some(place) = options.place {
        run.env("OMNI_JOIN_PLACE", place.to_string());
        if let Some(delay) = options.join_delay {
            run.env("OMNI_JOIN_DELAY", delay.to_string());
        }
        println!(
            "Omnidroid: joining place {place} {}s after start, once the sign-in is at Home",
            options.join_delay.unwrap_or(20.0)
        );
    }
    match &cookie {
        Some(cookie) if plant => {
            run.env("OMNI_COOKIE", &cookie.value);
            println!("Omnidroid: signing in as account `{}` with its cookie (--cookie; never printed)", cookie.account);
        }
        Some(cookie) => {
            run.env_remove("OMNI_COOKIE");
            println!(
                "Omnidroid: signing in as account `{}` with the cookie its storage holds -- the file's was \
                 planted before, and Roblox may have replaced it since",
                cookie.account
            );
        }
        None => {
            println!("Sign in with Quick Sign-in (Sign In > Quick Sign-in), then enter the code on a signed-in device.");
        }
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
    println!("End the session by closing the window.");

    match run.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(error) => {
            eprintln!("omnidroid: could not start cargo: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `omnidroid aosp`'s options.
#[derive(Debug, PartialEq, Eq)]
struct AospOptions {
    apk: Option<PathBuf>,
    cookie: Option<String>,
    place: Option<u64>,
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
}

fn parse_aosp(mut args: impl Iterator<Item = String>) -> Result<AospOptions, String> {
    let mut options =
        AospOptions {
        apk: None,
        cookie: None,
        place: None,
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
    };
    let mut minutes_given = false;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--apk" => options.apk = Some(PathBuf::from(value("--apk")?)),
            "--cookie" => options.cookie = Some(value("--cookie")?),
            "--place" => {
                let text = value("--place")?;
                let place: u64 = text.parse().map_err(|_| format!("--place wants a numeric placeId, not `{text}`"))?;
                if place == 0 {
                    return Err("--place wants a placeId above 0".to_string());
                }
                options.place = Some(place);
            }
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
/// `tools/aosp_play.ps1` sets on Windows, from these options.
fn aosp_env(options: &AospOptions, apk: &Path, cookie: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("OMNI_WINDOW", "1".to_string()),
        ("OMNI_R_MINUTES", options.minutes.to_string()),
        ("OMNI_TEST_APK", apk.display().to_string()),
        ("OMNI_R_KIOSK", if options.with_systemui { "0" } else { "1" }.to_string()),
    ];
    if let Some(cookie) = cookie {
        env.push(("OMNI_R_COOKIE", cookie.display().to_string()));
    }
    if let Some(place) = options.place {
        env.push(("OMNI_R_PLACE", place.to_string()));
    }
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
fn aosp(options: &AospOptions) -> ExitCode {
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
    let apk_path = if options.warm {
        println!("Omnidroid (real AOSP): a warm device, no app");
        PathBuf::new()
    } else {
        let apk = match omni_apk::choose_apk(options.apk.as_deref(), &repo_root()) {
            Ok(apk) => apk,
            Err(error) => {
                eprintln!("omnidroid: {error}");
                return ExitCode::FAILURE;
            }
        };
        println!("Omnidroid (real AOSP): {}", describe(&apk));
        std::fs::canonicalize(&apk.path).unwrap_or_else(|_| apk.path.clone())
    };
    // The session plants the cookie from a file (never printed): a file, or a name `login` saved.
    let cookie = match &options.cookie {
        None => None,
        Some(arg) => {
            let saved = cookies_dir().map(|d| d.join(format!("{arg}.txt"))).filter(|p| p.is_file());
            let path = if Path::new(arg).is_file() { PathBuf::from(arg) } else if let Some(p) = saved { p } else {
                eprintln!("omnidroid: --cookie: no file `{arg}` (aosp takes a cookie file or a name `login` saved)");
                return ExitCode::from(2);
            };
            if let Err(message) = account_cookie(&path.to_string_lossy()) {
                eprintln!("omnidroid: {message}");
                return ExitCode::from(2);
            }
            Some(std::fs::canonicalize(&path).unwrap_or(path))
        }
    };
    // A warm device is up: the session is an app on it -- no boot, no device of its own (a second
    // device beside it would only take the host's memory). Not for a session that names its own
    // device (--instance, --standby, --fresh-device), nor with OMNI_AOSP_WARM=0.
    if !options.warm && !options.standby && !options.fresh_device && options.instance.is_none() {
        if let Some(dev) = warm::usable(root_hash.as_deref()) {
            if options.size.is_some() || options.gpu.is_some() || options.with_systemui {
                println!("Omnidroid: --size, --gpu and --with-systemui are the warm device's own (set when it booted); not applied");
            }
            return warm::session(&dev, &repo_root(), &apk_path, cookie.as_deref(), options.place, options.minutes);
        }
    }
    // The session (cargo, the test, the device's host processes) ends with this process, however it
    // ends: a caller that kills the launcher (the MCP server) leaves nothing running.
    omni_platform::process::hold_children();
    let mut run = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    run.current_dir(repo_root())
        .args(["test", "--release", "-q", "-p", "omni-linux", "--test", "r_roblox", "--", "--ignored", "--nocapture"]);
    for (k, v) in aosp_env(options, &apk_path, cookie.as_deref()) {
        run.env(k, v);
    }
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
        "Omnidroid: a {}-minute session; {}{}the log is {}/omni-linux-r-<pid>.log, the display's screenshots beside it",
        options.minutes,
        cookie.as_ref().map_or(String::new(), |_| "signed in with --cookie (never printed); ".to_string()),
        options.place.map_or(String::new(), |p| format!("joining place {p} once signed in; ")),
        session_dir.display()
    );
    match run.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(error) => {
            eprintln!("omnidroid: could not start cargo: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUE: &str = "_|WARNING:-DO-NOT-SHARE-THIS.--Sharing-this-will-allow-someone-to-log-in-as-you.|_ABC123";

    #[test]
    fn a_cookie_is_read_from_every_shape_it_is_kept_in() {
        assert_eq!(cookie_value(&format!("{VALUE}\n")).as_deref(), Some(VALUE), "the bare value");
        assert_eq!(cookie_value(&format!("\u{feff}  {VALUE}\r\n")).as_deref(), Some(VALUE), "BOM, CRLF");
        assert_eq!(cookie_value(&format!(".ROBLOSECURITY={VALUE}")).as_deref(), Some(VALUE), "a pair");
        assert_eq!(
            cookie_value(&format!("RBXEventTrackerV2=x; .ROBLOSECURITY={VALUE}; other=1")).as_deref(),
            Some(VALUE),
            "a Cookie header"
        );
        let netscape =
            format!("# Netscape HTTP Cookie File\n.roblox.com\tTRUE\t/\tTRUE\t0\t.ROBLOSECURITY\t{VALUE}\n");
        assert_eq!(cookie_value(&netscape).as_deref(), Some(VALUE), "cookies.txt");
        assert_eq!(cookie_value("  \n"), None, "empty");
        assert_eq!(cookie_value("two words"), None, "a value a cookie store cannot hold");
    }

    #[test]
    fn a_file_names_its_account_a_value_is_hashed_and_a_missing_file_is_said() {
        let dir = std::env::temp_dir().join(format!("omnidroid-cookie-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let file = dir.join("farm 4.txt");
        std::fs::write(&file, format!("{VALUE}\n")).expect("the cookie file");
        let from_file = account_cookie(file.to_str().expect("utf-8")).expect("read");
        assert_eq!((from_file.value.as_str(), from_file.account.as_str()), (VALUE, "farm_4"));
        let long = format!("{VALUE}{}", "0".repeat(100));
        let from_value = account_cookie(&long).expect("a value");
        assert_eq!(from_value.value, long);
        assert!(from_value.account.starts_with("cookie-") && !from_value.account.contains(VALUE));
        assert!(account_cookie_in("missing.txt", None).is_err(), "a short argument is a file that is not there");
        let by_name = account_cookie_in("farm 4", Some(&dir)).expect("a name found in the cookies folder");
        assert_eq!((by_name.value.as_str(), by_name.account.as_str()), (VALUE, "farm_4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cookie_is_planted_once_per_account_so_a_rotated_one_is_kept() {
        let dir = std::env::temp_dir().join(format!("omnidroid-planted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        assert!(cookie_is_new_for(&dir, VALUE), "nothing planted yet");
        std::fs::write(dir.join(PLANTED_MARKER), fingerprint(VALUE)).expect("the marker");
        assert!(!cookie_is_new_for(&dir, VALUE), "the same file again: the store's cookie is kept");
        assert!(cookie_is_new_for(&dir, "_|WARNING:-a-new-export|_X"), "a new export is planted");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        let env = aosp_env(&parse(&["--module", "a,b", "--su", "all"]), Path::new("x.apk"), None);
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("OMNI_R_ROOT"), get("OMNI_R_MODULES"), get("OMNI_R_SU")), (Some("1"), Some("a,b"), Some("all")));
        let plain = aosp_env(&parse(&[]), Path::new("x.apk"), None);
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
        let env = aosp_env(&parse(&["--denylist", "a.b,c.d"]), Path::new("x.apk"), None);
        assert!(env.iter().any(|(n, v)| *n == "OMNI_R_DENYLIST" && v == "a.b,c.d"));
        let plain = aosp_env(&parse(&[]), Path::new("x.apk"), None);
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
    fn headless_and_the_control_file_are_options_of_play() {
        let parsed = |a: &[&str]| parse(a.iter().map(|s| (*s).to_string()));
        let options = parsed(&["--headless", "--control", "cmds.txt", "--place", "1"]).expect("parsed");
        assert!(options.headless);
        assert_eq!(options.control.as_deref(), Some(Path::new("cmds.txt")));
        let plain = parsed(&[]).expect("parsed");
        assert!(!plain.headless && plain.control.is_none());
        assert!(parsed(&["--control"]).is_err(), "--control needs a file");
        let windowless = parsed(&["--no-window"]).expect("parsed");
        assert!(windowless.no_window && windowless.headless, "--no-window is headless");
    }

    #[test]
    fn the_saved_login_is_read_back_and_arguments_are_parsed() {
        let text = "{\n  \"username\": \"HezMi_ImYu\",\n  \"login\": \"a@b.c\",\n  \"password\": \"p\\\"q\\\\r\\u00e9\"\n}\n";
        assert_eq!(json_string_field(text, "password").as_deref(), Some("p\"q\\r\u{e9}"));
        assert_eq!(json_string_field(text, "username").as_deref(), Some("HezMi_ImYu"));
        assert_eq!(json_string_field(text, "missing"), None);
        let args = |a: &[&str]| parse_login(a.iter().map(|s| (*s).to_string()));
        let both = args(&["name", "secret", "--dir", "x"]).expect("parsed");
        assert_eq!((both.username.as_deref(), both.password.as_deref()), (Some("name"), Some("secret")));
        assert_eq!(both.dir.as_deref(), Some(Path::new("x")));
        assert!(args(&["a", "b", "c"]).is_err());
        assert!(args(&["--nope"]).is_err());
    }

    #[test]
    fn aosp_takes_the_apk_the_account_and_the_place() {
        let args = |a: &[&str]| parse_aosp(a.iter().map(|s| (*s).to_string()));
        let o = args(&["--apk", "r.apk", "--cookie", "c.txt", "--place", "8737899170"]).expect("parsed");
        assert_eq!(o.apk.as_deref(), Some(Path::new("r.apk")));
        assert_eq!((o.cookie.as_deref(), o.place, o.minutes), (Some("c.txt"), Some(8_737_899_170), 30));
        let o = args(&["--minutes", "12", "--size", "1280x720", "--gpu", "gl", "--with-systemui"]).expect("parsed");
        assert_eq!((o.minutes, o.size, o.gpu.as_deref(), o.with_systemui), (12, Some((1280, 720)), Some("gl"), true));
        assert!(args(&["--gpu", "metal"]).is_err());
        assert!(args(&["--size", "1280"]).is_err());
        assert!(args(&["--place", "0"]).is_err());
        assert!(args(&["--minutes", "0"]).is_err());
        assert!(args(&["--fresh"]).is_err(), "play's options are not aosp's");
        let o = args(&["--fresh-device", "--standby", "--instance", "d"]).expect("parsed");
        assert_eq!((o.fresh_device, o.standby, o.instance.as_deref()), (true, true, Some(Path::new("d"))));
        let env = aosp_env(&o, Path::new("r.apk"), None);
        assert!(env.contains(&("OMNI_R_STANDBY", "1".to_string())) && env.contains(&("OMNI_R_INSTANCE", "d".to_string())));
    }

    #[test]
    fn aosp_gives_the_session_what_aosp_play_ps1_gave_it() {
        let o = parse_aosp(["--place", "1", "--gpu", "gl", "--size", "960x540"].iter().map(|s| (*s).to_string())).expect("parsed");
        let env = aosp_env(&o, Path::new("/a/r.apk"), Some(Path::new("/c/k.txt")));
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("OMNI_WINDOW"), Some("1"));
        assert_eq!(get("OMNI_R_MINUTES"), Some("30"));
        assert_eq!(get("OMNI_TEST_APK"), Some("/a/r.apk"));
        assert_eq!(get("OMNI_R_COOKIE"), Some("/c/k.txt"));
        assert_eq!(get("OMNI_R_PLACE"), Some("1"));
        assert_eq!(get("OMNI_R_KIOSK"), Some("1"), "a single-app device unless --with-systemui");
        assert_eq!(get("OMNI_WINDOW_SIZE"), Some("960x540"));
        assert_eq!(get("OMNI_GPU"), Some("gl"));
        let bare = aosp_env(&parse_aosp(std::iter::empty()).expect("parsed"), Path::new("r.apk"), None);
        assert!(bare.iter().all(|(k, _)| !matches!(*k, "OMNI_R_COOKIE" | "OMNI_R_PLACE" | "OMNI_GPU" | "OMNI_R_STANDBY" | "OMNI_R_INSTANCE")));
    }

    #[test]
    fn a_tmpfs_temp_directory_moves_the_session_to_the_disk() {
        let data = Path::new("/home/u/.local/share/omnidroid/data");
        assert_eq!(aosp_work_dir(false, true, Some(data)), Some(PathBuf::from("/home/u/.local/share/omnidroid/aosp")));
        assert_eq!(aosp_work_dir(true, true, Some(data)), None, "TMPDIR named is kept");
        assert_eq!(aosp_work_dir(false, false, Some(data)), None, "a temp directory on a disk is kept");
    }
}
