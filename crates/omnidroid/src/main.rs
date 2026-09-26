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

/// The gate test whose run is a session: `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`.
const SESSION_TEST: &str = "initialize_native_code_returns_a_native_code_and_the_game_thread_starts";
/// "No limit": ten years, the gate's way of saying until the window is closed.
const UNTIL_CLOSED_SECONDS: u64 = 315_360_000;

const USAGE: &str = "\
usage: omnidroid [play] [--apk <path>] [--cookie <file|value>] [--place <id>] [--join-delay <s>]
                       [--minutes <n>] [--fresh] [--phone] [--data-dir <dir>]
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
    let args = rest.into_iter();
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
}
