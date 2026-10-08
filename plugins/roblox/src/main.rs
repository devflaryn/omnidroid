//! The `roblox` omnidroid plugin: what signing in to Roblox and joining a place need, kept out of
//! the launcher everybody runs. Installed with `omnidroid plugins add --link plugins/roblox`.
//!
//! ```text
//! omnidroid play --place 8737899170                      # sign-in kept, then join the place
//! omnidroid play --cookie farm4.txt --place 8737899170   # sign in as that account
//! omnidroid aosp --cookie farm4 --place 8737899170       # the same on the real-AOSP path
//! omnidroid login                                        # sign in in Chromium, keep the cookie
//! omnidroid login <username> [<password>]
//! ```
//!
//! This binary is the plugin's `session-start` hook and its `login` command (see
//! `omnidroid-plugin.json`); omnidroid runs it with `OMNI_COMMAND`, `OMNI_APP_DATA_DIR`,
//! `OMNI_FRESH`, `OMNI_GIVEN_DATA_DIR` and the options as `OMNI_ARG_COOKIE`, `OMNI_ARG_PLACE`,
//! `OMNI_ARG_JOIN_DELAY`. `--place` and `--join-delay` reach the session by the manifest alone
//! (`OMNI_JOIN_PLACE`, `OMNI_JOIN_DELAY` for `play`; `OMNI_R_PLACE` for `aosp`); the hook answers
//! for `--cookie` and says what the session will do.
//!
//! * **`--place <id>` joins that place** once the app's own saved sign-in has reached Home
//!   (`--join-delay` seconds in, default 20): the gate calls `nativeAppBridgeV2StartGameWithParam`
//!   as the app's Play button does. It needs a signed-in data directory -- with `--fresh` there is no
//!   session to join with -- or `--cookie`.
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
//!   file never puts a dead cookie back over a live one. On `aosp` the file is handed to the session
//!   (`OMNI_R_COOKIE`), which plants it in the device's app.
//! * **`login` signs in in a real browser and keeps the session** (`login.py`, Selenium with
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("session-start") => session_start(),
        Some("login") => match parse_login(args.into_iter().skip(1)) {
            Ok(options) => login(&options),
            Err(message) => {
                eprintln!("omnidroid login: {message}\nusage: omnidroid login [<username> [<password>]] [--dir <dir>]");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("the roblox plugin's program: run by omnidroid (session-start | login)");
            ExitCode::from(2)
        }
    }
}

/// A variable omnidroid gave, when it is not empty.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// omnidroid's app-data directory (`OMNI_APP_DATA_DIR`).
fn app_data_dir() -> Option<PathBuf> {
    var("OMNI_APP_DATA_DIR").map(PathBuf::from)
}

/// Where `login` keeps cookies and `--cookie <name>` finds them: `<app-data>/../cookies`.
fn cookies_dir() -> Option<PathBuf> {
    Some(app_data_dir()?.parent()?.join("cookies"))
}

/// The hook's answers to omnidroid, one directive a line (`plugin.rs` in the launcher).
#[derive(Default)]
struct Answer(Vec<String>);

impl Answer {
    fn say(&mut self, text: impl AsRef<str>) {
        self.0.push(format!("say {}", text.as_ref()));
    }
    fn env(&mut self, key: &str, value: &str) {
        self.0.push(format!("env {key}={value}"));
    }
    fn unset(&mut self, key: &str) {
        self.0.push(format!("unset {key}"));
    }
    fn data_dir(&mut self, dir: &Path) {
        self.0.push(format!("data-dir {}", dir.display()));
    }
}

/// The `session-start` hook: the account `--cookie` names, made the session's.
fn session_start() -> ExitCode {
    let command = var("OMNI_COMMAND").unwrap_or_default();
    let answer = match command.as_str() {
        "play" => play(),
        "aosp" => aosp(),
        _ => Ok(Answer::default()),
    };
    match answer {
        Ok(answer) => {
            for line in answer.0 {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            println!("error {message}");
            ExitCode::from(2)
        }
    }
}

/// `play`: the account's cookie in `OMNI_COMMAND`'s store, and its own storage.
fn play() -> Result<Answer, String> {
    let mut answer = Answer::default();
    let cookie = var("OMNI_ARG_COOKIE").as_deref().map(account_cookie).transpose()?;
    let fresh = var("OMNI_FRESH").as_deref() == Some("1");
    let place = var("OMNI_ARG_PLACE");
    if place.is_some() && fresh && cookie.is_none() {
        return Err("--place joins with the saved sign-in, and --fresh starts without one; \
                    drop --fresh (sign in once first), or pass --cookie, to join"
            .to_string());
    }
    // Whether the account's cookie goes into the store at start: always on a fresh install, and in
    // kept storage only when it is not the one last planted there (see `cookie_is_new_for`).
    let mut plant = true;
    if let (Some(cookie), false) = (&cookie, fresh) {
        // An account named by --cookie keeps its own storage beside the default one.
        let given = var("OMNI_GIVEN_DATA_DIR").map(PathBuf::from);
        let dir = match &given {
            Some(dir) => dir.clone(),
            None => app_data_dir()
                .and_then(|d| Some(d.parent()?.join("accounts").join(&cookie.account)))
                .ok_or("this host names no app-data directory (its HOME or LOCALAPPDATA is unset); pass --data-dir")?,
        };
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        if given.is_none() {
            answer.data_dir(&dir);
        }
        plant = cookie_is_new_for(&dir, &cookie.value);
        if plant {
            std::fs::write(dir.join(PLANTED_MARKER), fingerprint(&cookie.value))
                .map_err(|e| format!("could not write {}: {e}", dir.join(PLANTED_MARKER).display()))?;
        }
    }
    if let Some(place) = &place {
        answer.say(format!(
            "joining place {place} {}s after start, once the sign-in is at Home",
            var("OMNI_ARG_JOIN_DELAY").unwrap_or_else(|| "20".to_string())
        ));
    }
    match &cookie {
        Some(cookie) if plant => {
            answer.env("OMNI_COOKIE", &cookie.value);
            answer.say(format!("signing in as account `{}` with its cookie (--cookie; never printed)", cookie.account));
        }
        Some(cookie) => {
            answer.unset("OMNI_COOKIE");
            answer.say(format!(
                "signing in as account `{}` with the cookie its storage holds -- the file's was planted before, \
                 and Roblox may have replaced it since",
                cookie.account
            ));
        }
        None => answer.say("sign in with Quick Sign-in (Sign In > Quick Sign-in), then enter the code on a signed-in device"),
    }
    Ok(answer)
}

/// `aosp`: the session plants the cookie from a file (never printed) -- a file, or a name `login`
/// saved.
fn aosp() -> Result<Answer, String> {
    let mut answer = Answer::default();
    if let Some(arg) = var("OMNI_ARG_COOKIE") {
        let saved = cookies_dir().map(|d| d.join(format!("{arg}.txt"))).filter(|p| p.is_file());
        let path = match (Path::new(&arg).is_file(), saved) {
            (true, _) => PathBuf::from(&arg),
            (false, Some(saved)) => saved,
            (false, None) => return Err(format!("--cookie: no file `{arg}` (aosp takes a cookie file or a name `login` saved)")),
        };
        account_cookie(&path.to_string_lossy())?;
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        answer.env("OMNI_R_COOKIE", &path.display().to_string());
        answer.say("signed in with --cookie (never printed)");
    }
    if let Some(place) = var("OMNI_ARG_PLACE") {
        answer.say(format!("joining place {place} once signed in"));
    }
    Ok(answer)
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

/// `omnidroid login`: `login.py` in the Python environment made for it. See the module docs.
fn login(options: &LoginOptions) -> ExitCode {
    let Some(dir) = options.dir.clone().or_else(cookies_dir) else {
        eprintln!("omnidroid login: this host names no app-data directory (its HOME or LOCALAPPDATA is unset); pass --dir");
        return ExitCode::FAILURE;
    };
    // A username alone: the password `login` kept for it.
    let password = match (&options.username, &options.password) {
        (Some(username), None) => match saved_password(&dir, username) {
            Ok(password) => Some(password),
            Err(message) => {
                eprintln!("omnidroid login: {message}");
                return ExitCode::from(2);
            }
        },
        (_, password) => password.clone(),
    };
    let python = match login_python() {
        Ok(python) => python,
        Err(message) => {
            eprintln!("omnidroid login: {message}");
            return ExitCode::FAILURE;
        }
    };
    let script = var("OMNI_PLUGIN_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from).join("login.py");
    let mut run = Command::new(&python);
    run.arg(script).arg("--dir").arg(&dir);
    // omnidroid's own Chromium and driver live beside its other data, not in a shared cache.
    if let Some(chromium) = app_data_dir().and_then(|d| Some(d.parent()?.join("chromium"))) {
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
            eprintln!("omnidroid login: could not start {}: {error}", python.display());
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
/// that file (escapes included) without a JSON crate.
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

/// The Python that runs `login.py`, with Selenium: a virtual environment in
/// `<app-data>/../login-venv`, made (and given pip and Selenium) the first time.
fn login_python() -> Result<PathBuf, String> {
    let venv = app_data_dir()
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
        let from_file = account_cookie_in(file.to_str().expect("utf-8"), None).expect("read");
        assert_eq!((from_file.value.as_str(), from_file.account.as_str()), (VALUE, "farm_4"));
        let long = format!("{VALUE}{}", "0".repeat(100));
        let from_value = account_cookie_in(&long, None).expect("a value");
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
