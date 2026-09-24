//! `omnidroid`: run an APK in a real window for a person to use, on Windows, macOS or Linux.
//!
//! ```text
//! cargo run --release -p omnidroid -- play                      # the newest APK in the repository root
//! cargo run --release -p omnidroid -- play --apk Roblox-2.739.691.apk
//! cargo run --release -p omnidroid -- play --minutes 90 --fresh --phone
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

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The gate test whose run is a session: `initialize_native_code_returns_a_native_code_and_the_game_thread_starts`.
const SESSION_TEST: &str = "initialize_native_code_returns_a_native_code_and_the_game_thread_starts";
/// "No limit": ten years, the gate's way of saying until the window is closed.
const UNTIL_CLOSED_SECONDS: u64 = 315_360_000;

const USAGE: &str = "\
usage: omnidroid play  [--apk <path>] [--minutes <n>] [--fresh] [--phone] [--data-dir <dir>]
       omnidroid which [--apk <path>]

  --apk <path>      the APK to run (else OMNI_APK, else the newest *.apk in the repository root)
  --minutes <n>     end the session after n minutes (default: when the window is closed)
  --fresh           a fresh install; the kept storage is left as it is
  --phone           a touch screen: the mouse is a finger, and there is no keyboard
  --data-dir <dir>  where the app's storage is kept (default: this host's app-data directory)";

struct Options {
    apk: Option<PathBuf>,
    minutes: u64,
    fresh: bool,
    phone: bool,
    data_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next();
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
    let mut options = Options { apk: None, minutes: 0, fresh: false, phone: false, data_dir: None };
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

fn play(options: &Options) -> ExitCode {
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

    if options.fresh {
        run.env_remove("OMNI_DATA_DIR");
        println!("Omnidroid: {length} on a fresh install");
    } else {
        let Some(dir) = options.data_dir.clone().or_else(omni_platform::process::app_data_dir) else {
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
    println!("Sign in with Quick Sign-in (Sign In > Quick Sign-in), then enter the code on a signed-in device.");
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
