//! The app's command line: what a line typed at its prompt asks for. Pure, so every form is a test.

use std::path::PathBuf;

/// A command typed at the app's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `run <apk> [xN | N] [plugin options]`: launch N instances of the APK, doing what the
    /// plugins' options say (`--cookie <name> --place <id>`: `super::plugged`).
    Run(PathBuf, u32, Vec<String>),
    /// `close <#id>... | all`.
    Close(Vec<u32>),
    CloseAll,
    /// `restart <#id>... | all`: start instances' apps again on their windows once their cards have
    /// counted down (`omni_pool::RESTART_DELAY`); `all`: every instance running or showing its card.
    Restart(Vec<u32>),
    RestartAll,
    /// `autorestart on|off [#id... | all]`: start an app again when it crashes -- these instances'
    /// own setting, or (no ids, or `all`: a one-shot command's way, `main`) the pool's, for every
    /// instance without its own.
    AutoRestart(bool, Vec<u32>),
    /// `autorestart`: the pool's setting, and the instances whose own differs from it.
    AutoRestartStatus,
    /// `warm`: boot one more device now.
    Warm,
    /// `kill <D#>... | all`: stop devices.
    Kill(Vec<u32>),
    KillAll,
    /// `shell <D#> <command>`.
    Shell(u32, String),
    /// `plugins [list | add [--link] <dir> | remove <name> | enable <name> | disable <name>]`.
    Plugins(Vec<String>),
    /// `settings`: list them.
    Settings,
    /// `set <key> <value>`.
    Set(String, String),
    /// `mcp [on | off | register | status]`.
    Mcp(String),
    /// `clear`: forget ended instances.
    Clear,
    Help,
    Quit,
}

/// The help the app shows.
pub const HELP: &[&str] = &[
    "run <apk> [xN]       launch N instances of any APK (Tab completes the path)",
    "  ... --cookie <c> --place <id>   with a plugin's options (`plugins` lists them)",
    "close <#id>... | all close instances (or Delete on a selected one)",
    "restart <#id>... | all  restart instances' apps on their windows (in 5 s; the card counts down)",
    "autorestart on|off [#id...]  restart an app when it crashes (no ids: the pool's setting, for instances without their own)",
    "warm                 boot one more Android now",
    "kill <D#>... | all   stop Androids (or Delete on a selected one): after the last, none warms till run or warm",
    "shell <D#> <cmd>     a shell command on an Android, its output in the log",
    "plugins [list|add [--link] <dir>|remove|enable|disable <name>]",
    "settings | set <key> <value>   the app's settings (idle_close_minutes, keep_warm, mcp_port, ...)",
    "mcp on|off|register|status     the MCP server for coding agents (127.0.0.1 only)",
    "clear                forget ended instances      help | quit",
    "keys: Tab switches Androids/Instances, Up/Down select, Delete closes, Ctrl+P/N history, Ctrl+C quits",
];

/// `D3` or `3`.
fn device_id(word: &str) -> Option<u32> {
    word.trim_start_matches(['D', 'd']).parse().ok()
}

/// `#3` or `3`.
fn instance_id(word: &str) -> Option<u32> {
    word.trim_start_matches('#').parse().ok()
}

/// Instance ids (`#3` or `3`), each one, or what is not one.
fn instance_ids(words: &[&str]) -> Result<Vec<u32>, String> {
    words.iter().map(|w| instance_id(w).ok_or_else(|| format!("not an instance: {w}"))).collect()
}

/// A path as typed: quotes taken off, `~` expanded.
fn path(text: &str) -> PathBuf {
    let t = text.trim().trim_matches('"').trim_matches('\'');
    match t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        Some(rest) => home().map_or_else(|| PathBuf::from(t), |h| h.join(rest)),
        None => PathBuf::from(t),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Words as a shell splits them: on spaces, a quoted word kept whole (its quotes taken off).
pub fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let (mut quote, mut any) = (None, false);
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => (quote, any) = (Some(c), true),
            (None, c) if c.is_whitespace() => {
                if any || !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                any = false;
            }
            (None, c) => word.push(c),
        }
    }
    if any || !word.is_empty() {
        out.push(word);
    }
    out
}

/// `run`'s argument: the path (which may hold spaces), a count (`x3`, `3`), and the plugins'
/// options from the first ` --` on.
fn run_args(rest: &str) -> Result<(PathBuf, u32, Vec<String>), String> {
    let rest = rest.trim();
    let (rest, options) = match rest.find(" --") {
        Some(at) => (rest[..at].trim(), words(&rest[at..])),
        None => (rest, Vec::new()),
    };
    let (path, n) = path_and_count(rest)?;
    Ok((path, n, options))
}

/// The path (which may hold spaces) and a trailing count (`x3`, `3`).
fn path_and_count(rest: &str) -> Result<(PathBuf, u32), String> {
    if rest.is_empty() {
        return Err("run needs an APK's path".into());
    }
    if let Some((head, last)) = rest.rsplit_once(char::is_whitespace) {
        let count = last.strip_prefix(['x', 'X', '*']).unwrap_or(last);
        if let Ok(n) = count.parse::<u32>() {
            if n == 0 || n > 64 {
                return Err(format!("{n} instances: 1 to 64 at once"));
            }
            return Ok((path(head), n));
        }
    }
    Ok((path(rest), 1))
}

/// Parse one line.
///
/// # Errors
/// What is wrong with it, said for the log.
pub fn parse(line: &str) -> Result<Command, String> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(w, r)| (w, r.trim()));
    let words: Vec<&str> = rest.split_whitespace().collect();
    match word.to_ascii_lowercase().as_str() {
        "run" | "r" | "launch" => run_args(rest).map(|(p, n, o)| Command::Run(p, n, o)),
        "close" | "c" => match words.as_slice() {
            ["all"] => Ok(Command::CloseAll),
            [] => Err("close #id... or close all".into()),
            ids => instance_ids(ids).map(Command::Close),
        },
        "restart" => match words.as_slice() {
            ["all"] => Ok(Command::RestartAll),
            [] => Err("restart #id... or restart all".into()),
            ids => instance_ids(ids).map(Command::Restart),
        },
        "autorestart" | "auto-restart" => match words.as_slice() {
            [] => Ok(Command::AutoRestartStatus),
            [on, ids @ ..] => {
                let on = match on.to_ascii_lowercase().as_str() {
                    "on" | "true" | "yes" | "1" => true,
                    "off" | "false" | "no" | "0" => false,
                    _ => return Err(format!("autorestart on|off [#id... | all], not `{on}`")),
                };
                match ids {
                    ["all"] => Ok(Command::AutoRestart(on, Vec::new())),
                    ids => instance_ids(ids).map(|ids| Command::AutoRestart(on, ids)),
                }
            }
        },
        "warm" | "w" => Ok(Command::Warm),
        "kill" | "k" => match words.as_slice() {
            ["all"] => Ok(Command::KillAll),
            [] => Err("kill D#... or kill all".into()),
            ids => ids.iter().map(|w| device_id(w).ok_or_else(|| format!("not an Android: {w}"))).collect::<Result<_, _>>().map(Command::Kill),
        },
        "shell" | "sh" => {
            let (d, cmd) = rest.split_once(char::is_whitespace).ok_or("shell D# <command>")?;
            Ok(Command::Shell(device_id(d).ok_or_else(|| format!("not an Android: {d}"))?, cmd.trim().to_string()))
        }
        "plugins" | "plugin" => Ok(Command::Plugins(words.iter().map(|w| (*w).to_string()).collect())),
        "settings" => Ok(Command::Settings),
        "set" => match words.as_slice() {
            [key, value @ ..] if !value.is_empty() => Ok(Command::Set((*key).to_string(), value.join(" "))),
            _ => Err("set <key> <value> (settings lists the keys)".into()),
        },
        "mcp" => Ok(Command::Mcp(words.first().copied().unwrap_or("status").to_string())),
        "clear" => Ok(Command::Clear),
        "help" | "h" | "?" => Ok(Command::Help),
        "quit" | "q" | "exit" => Ok(Command::Quit),
        "" => Err(String::new()),
        other => {
            // A path to an APK typed alone runs it.
            if other.to_ascii_lowercase().ends_with(".apk") || line.to_ascii_lowercase().contains(".apk") {
                return run_args(line).map(|(p, n, o)| Command::Run(p, n, o));
            }
            Err(format!("unknown command `{other}` (help lists them)"))
        }
    }
}

/// Complete the path at the end of a `run` line: the longest common start of the entries it could
/// be, a directory ending in a separator. `None`: nothing to complete.
#[must_use]
pub fn complete(line: &str) -> Option<String> {
    let (head, partial) = if let Some(rest) = line.strip_prefix("run ") {
        ("run ", rest)
    } else {
        return None;
    };
    let typed = path(partial);
    let (dir, stem) = if partial.ends_with('/') || partial.ends_with('\\') || partial.is_empty() {
        (if partial.is_empty() { PathBuf::from(".") } else { typed.clone() }, String::new())
    } else {
        (typed.parent().map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from(".")), typed.file_name()?.to_string_lossy().into_owned())
    };
    let mut names: Vec<(String, bool)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().ok()?.is_dir();
            (name.starts_with(&stem) && (is_dir || name.to_ascii_lowercase().ends_with(".apk")) && !name.starts_with('.')).then_some((name, is_dir))
        })
        .collect();
    names.sort();
    let first = names.first()?;
    let common = names.iter().fold(first.0.clone(), |acc, (n, _)| acc.chars().zip(n.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect());
    let mut done = if dir == std::path::Path::new(".") && !partial.starts_with("./") { PathBuf::from(&common) } else { dir.join(&common) };
    if names.len() == 1 && first.1 {
        done.push("");
    }
    Some(format!("{head}{}", done.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_takes_a_path_with_spaces_and_a_count() {
        assert_eq!(parse("run /a b/Talking Tom.apk x3"), Ok(Command::Run(PathBuf::from("/a b/Talking Tom.apk"), 3, vec![])));
        assert_eq!(parse("run game.apk 5"), Ok(Command::Run(PathBuf::from("game.apk"), 5, vec![])));
        assert_eq!(parse("run \"/x/y.apk\""), Ok(Command::Run(PathBuf::from("/x/y.apk"), 1, vec![])));
        assert_eq!(parse("/x/y.apk x2"), Ok(Command::Run(PathBuf::from("/x/y.apk"), 2, vec![])), "a path alone runs it");
        let o = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse("run /a b/Roblox.apk x2 --cookie farm4 --place 8737899170"),
            Ok(Command::Run(PathBuf::from("/a b/Roblox.apk"), 2, o(&["--cookie", "farm4", "--place", "8737899170"])))
        );
        assert_eq!(parse("/x/r.apk --cookie \"/my cookies/a.txt\""), Ok(Command::Run(PathBuf::from("/x/r.apk"), 1, o(&["--cookie", "/my cookies/a.txt"]))));
        assert!(parse("run").is_err() && parse("run a.apk x0").is_err() && parse("run a.apk x65").is_err());
    }

    #[test]
    fn close_kill_and_the_rest() {
        assert_eq!(parse("close #2 3"), Ok(Command::Close(vec![2, 3])));
        assert_eq!(parse("close all"), Ok(Command::CloseAll));
        assert_eq!(parse("kill D1"), Ok(Command::Kill(vec![1])));
        assert_eq!(parse("kill all"), Ok(Command::KillAll));
        assert_eq!(parse("shell D2 pm list packages -3"), Ok(Command::Shell(2, "pm list packages -3".into())));
        assert_eq!(parse("set idle_close_minutes 10"), Ok(Command::Set("idle_close_minutes".into(), "10".into())));
        assert_eq!(parse("mcp"), Ok(Command::Mcp("status".into())));
        assert_eq!(parse("plugins add --link plugins/x"), Ok(Command::Plugins(vec!["add".into(), "--link".into(), "plugins/x".into()])));
        assert_eq!(parse("q"), Ok(Command::Quit));
        assert!(parse("close x").is_err() && parse("frobnicate").is_err() && parse("set k").is_err());
    }

    #[test]
    fn restart_and_auto_restart() {
        assert_eq!(parse("restart #3"), Ok(Command::Restart(vec![3])));
        assert_eq!(parse("restart 1 #2"), Ok(Command::Restart(vec![1, 2])));
        assert_eq!(parse("restart all"), Ok(Command::RestartAll));
        assert_eq!(parse("autorestart on"), Ok(Command::AutoRestart(true, vec![])), "no ids: the pool's setting");
        assert_eq!(parse("autorestart off all"), Ok(Command::AutoRestart(false, vec![])), "the same, said");
        assert_eq!(parse("autorestart off #2 3"), Ok(Command::AutoRestart(false, vec![2, 3])));
        assert_eq!(parse("autorestart"), Ok(Command::AutoRestartStatus));
        assert_eq!(parse("set auto_restart on"), Ok(Command::Set("auto_restart".into(), "on".into())));
        assert!(parse("restart").is_err() && parse("restart x").is_err());
        assert!(parse("autorestart maybe").is_err() && parse("autorestart on x").is_err());
    }

    #[test]
    fn tab_completes_an_apk_s_path() {
        let dir = std::env::temp_dir().join(format!("omni-app-complete-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("apks")).unwrap();
        std::fs::write(dir.join("apks/TalkingTom.apk"), b"").unwrap();
        std::fs::write(dir.join("apks/notes.txt"), b"").unwrap();
        let line = format!("run {}/ap", dir.display());
        assert_eq!(complete(&line), Some(format!("run {}/apks/", dir.display())), "the one directory, with a separator");
        let line = format!("run {}/apks/T", dir.display());
        assert_eq!(complete(&line), Some(format!("run {}/apks/TalkingTom.apk", dir.display())), "an APK, not the text file");
        assert_eq!(complete("close 1"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
