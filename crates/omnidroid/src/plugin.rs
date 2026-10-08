//! Plugins: what one person's game needs, kept out of the launcher everybody runs.
//!
//! A plugin is a directory holding `omnidroid-plugin.json`, installed for this user with
//! `omnidroid plugins add <dir>` (a copy) or `omnidroid plugins add --link <dir>` (the directory
//! itself, so edits there are live). Nothing in the repository is loaded unless it is installed:
//! `plugins/` holds plugins to install, not plugins that run.
//!
//! What a plugin can do, all declared in its manifest:
//!
//! * **`args`**: options of its own for `play` and `aosp` (`--place <id>`). Each is taken out of the
//!   command line before the launcher reads it, checked against its `type` (`switch`, `string`,
//!   `path` -- made absolute --, `integer`, `number`, with `min`/`max`) and, when it names an `env`
//!   variable for the command, handed to the session in it. A hook sees it as `OMNI_ARG_<NAME>`.
//! * **`env`**: variables every session of a command is given (`"*"` for every command).
//! * **`defaults`**: arguments put in front of the person's own (theirs come later, so they win).
//! * **`hooks`**: programs run at `session-start` (once the APK is chosen, before the session starts)
//!   and `session-end`. A `session-start` hook answers on stdout, one directive a line:
//!   `env KEY=VALUE` (never printed), `unset KEY`, `data-dir <dir>` (`play`'s storage, unless
//!   `--data-dir`), `say <text>`, `error <text>` (the session does not start); any other line is
//!   shown as it is, and a hook that fails stops the session.
//! * **`commands`**: commands of its own (`omnidroid login`), run with the arguments after them.
//!
//! Every program a plugin runs gets `OMNI_PLUGIN_NAME`, `OMNI_PLUGIN_DIR`, `OMNI_REPO`,
//! `OMNI_COMMAND` and `OMNI_APP_DATA_DIR`, and a session's hooks also `OMNI_FRESH` (`1`/`0`),
//! `OMNI_GIVEN_DATA_DIR` (`--data-dir`), `OMNI_APK` and `OMNI_APK_PACKAGE`, and `session-end`'s
//! `OMNI_EXIT_CODE`. `${PLUGIN_DIR}`, `${REPO}`, `${APP_DATA_DIR}` and `${CARGO}` in a program's
//! arguments and in `env` values are replaced with theirs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// The MCP server's JSON: std only, and the launcher has no JSON crate of its own.
#[allow(dead_code)]
#[path = "../../omni-mcp/src/json.rs"]
mod json;
use json::Json;

/// The manifest's file name in a plugin's directory.
pub const MANIFEST: &str = "omnidroid-plugin.json";
/// The commands that run a session: the ones a plugin's options, env and hooks are for.
pub const SESSION_COMMANDS: &[&str] = &["play", "aosp"];
/// The launcher's own commands: a plugin's command cannot take one of these names.
const CORE_COMMANDS: &[&str] = &["play", "aosp", "which", "modules", "plugins", "help", "warm-release"];
/// The launcher's own options: a plugin's option cannot take one of these.
const CORE_FLAGS: &[&str] = &[
    "--apk", "--minutes", "--fresh", "--phone", "--data-dir", "--headless", "--no-window", "--control", "--size",
    "--gpu", "--with-systemui", "--fresh-device", "--standby", "--warm", "--instance", "--root", "--module",
    "--denylist", "--su", "--no-clipboard", "--help", "--link",
];
/// The hook events.
const EVENTS: &[&str] = &["session-start", "session-end"];
/// The installed plugins that are switched off, one name a line, in the plugin directory.
const DISABLED: &str = "disabled.txt";
/// An installed link: `<name>.link` in the plugin directory holds the plugin's own directory.
const LINK: &str = "link";

/// What an option's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Switch,
    String,
    Path,
    Integer,
    Number,
}

/// An option a plugin adds.
#[derive(Debug, Clone)]
pub struct Arg {
    pub flag: String,
    pub kind: Kind,
    /// The value's name in the help (`<id>`).
    pub value_name: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub help: String,
    /// The commands it is an option of.
    pub commands: Vec<String>,
    /// The variable it is handed to the session in, per command.
    pub env: BTreeMap<String, String>,
}

impl Arg {
    /// `OMNI_ARG_<NAME>`: `--join-delay` is `OMNI_ARG_JOIN_DELAY`.
    pub fn hook_var(&self) -> String {
        format!("OMNI_ARG_{}", self.flag.trim_start_matches('-').replace('-', "_").to_ascii_uppercase())
    }
}

/// A command a plugin adds.
#[derive(Debug, Clone)]
pub struct PluginCommand {
    pub name: String,
    pub run: Vec<String>,
    pub help: String,
}

/// A plugin, as its manifest declares it.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub description: String,
    pub dir: PathBuf,
    pub args: Vec<Arg>,
    /// Variables per command (`"*"`: every command).
    pub env: BTreeMap<String, BTreeMap<String, String>>,
    /// Arguments put in front of the person's, per command.
    pub defaults: BTreeMap<String, Vec<String>>,
    /// The program run at each event.
    pub hooks: BTreeMap<String, Vec<String>>,
    pub commands: Vec<PluginCommand>,
}

fn is_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn strings(value: &Json, what: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{what} is not a list"))?
        .iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("{what} holds something that is not a string")))
        .collect()
}

fn string_map(value: &Json, what: &str) -> Result<BTreeMap<String, String>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} is not an object"))?
        .iter()
        .map(|(k, v)| Ok((k.clone(), v.as_str().ok_or_else(|| format!("{what}.{k} is not a string"))?.to_string())))
        .collect()
}

fn known_command(command: &str, what: &str) -> Result<(), String> {
    if command == "*" || SESSION_COMMANDS.contains(&command) {
        Ok(())
    } else {
        Err(format!("{what} names `{command}`, which is not one of {} (or `*`)", SESSION_COMMANDS.join(", ")))
    }
}

impl Plugin {
    /// The plugin in `dir`: its manifest, read and checked.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let file = dir.join(MANIFEST);
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        Self::parse(&text, dir).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// A manifest's text, for the plugin in `dir`.
    pub fn parse(text: &str, dir: &Path) -> Result<Self, String> {
        let root = json::parse(text).map_err(|e| e.to_string())?;
        let field = |key: &str| root.get(key);
        let name = field("name").and_then(Json::as_str).ok_or("no \"name\"")?.to_string();
        if !is_name(&name) {
            return Err(format!("the name `{name}` is not lower-case letters, digits, `-` and `_`"));
        }
        let text_of = |key: &str| field(key).and_then(Json::as_str).unwrap_or_default().to_string();
        let mut plugin = Plugin {
            name,
            version: text_of("version"),
            description: text_of("description"),
            dir: dir.to_path_buf(),
            args: Vec::new(),
            env: BTreeMap::new(),
            defaults: BTreeMap::new(),
            hooks: BTreeMap::new(),
            commands: Vec::new(),
        };
        for (i, arg) in field("args").map(|a| a.as_array().ok_or("\"args\" is not a list")).transpose()?.unwrap_or_default().iter().enumerate() {
            plugin.args.push(Self::arg(arg).map_err(|e| format!("args[{i}]: {e}"))?);
        }
        if let Some(env) = field("env") {
            for (command, vars) in env.as_object().ok_or("\"env\" is not an object")? {
                known_command(command, "\"env\"")?;
                plugin.env.insert(command.clone(), string_map(vars, &format!("env.{command}"))?);
            }
        }
        if let Some(defaults) = field("defaults") {
            for (command, args) in defaults.as_object().ok_or("\"defaults\" is not an object")? {
                known_command(command, "\"defaults\"")?;
                if command == "*" {
                    return Err("\"defaults\" are per command; `*` is not one".to_string());
                }
                plugin.defaults.insert(command.clone(), strings(args, &format!("defaults.{command}"))?);
            }
        }
        if let Some(hooks) = field("hooks") {
            for (event, run) in hooks.as_object().ok_or("\"hooks\" is not an object")? {
                if !EVENTS.contains(&event.as_str()) {
                    return Err(format!("hooks: `{event}` is not an event (they are {})", EVENTS.join(", ")));
                }
                let run = strings(run, &format!("hooks.{event}"))?;
                if run.is_empty() {
                    return Err(format!("hooks.{event} names no program"));
                }
                plugin.hooks.insert(event.clone(), run);
            }
        }
        if let Some(commands) = field("commands") {
            for (name, command) in commands.as_object().ok_or("\"commands\" is not an object")? {
                if !is_name(name) || CORE_COMMANDS.contains(&name.as_str()) {
                    return Err(format!("commands: `{name}` is the launcher's own, or not a name"));
                }
                let run = strings(command.get("run").ok_or_else(|| format!("commands.{name} has no \"run\""))?, &format!("commands.{name}.run"))?;
                if run.is_empty() {
                    return Err(format!("commands.{name}.run names no program"));
                }
                let help = command.get("help").and_then(Json::as_str).unwrap_or_default().to_string();
                plugin.commands.push(PluginCommand { name: name.clone(), run, help });
            }
        }
        Ok(plugin)
    }

    fn arg(value: &Json) -> Result<Arg, String> {
        let flag = value.get("flag").and_then(Json::as_str).ok_or("no \"flag\"")?.to_string();
        let bare = flag.strip_prefix("--").unwrap_or_default();
        if !is_name(bare) {
            return Err(format!("`{flag}` is not `--` and a name"));
        }
        if CORE_FLAGS.contains(&flag.as_str()) {
            return Err(format!("`{flag}` is the launcher's own option"));
        }
        let kind = match value.get("type").and_then(Json::as_str).unwrap_or("string") {
            "switch" => Kind::Switch,
            "string" => Kind::String,
            "path" => Kind::Path,
            "integer" => Kind::Integer,
            "number" => Kind::Number,
            other => return Err(format!("`{flag}`: type `{other}` is not switch, string, path, integer or number")),
        };
        let commands = match value.get("commands") {
            Some(list) => strings(list, "commands")?,
            None => SESSION_COMMANDS.iter().map(|c| (*c).to_string()).collect(),
        };
        for command in &commands {
            if !SESSION_COMMANDS.contains(&command.as_str()) {
                return Err(format!("`{flag}`: `{command}` is not one of {}", SESSION_COMMANDS.join(", ")));
            }
        }
        // `"env": "VAR"` for every command it is an option of, or `{ "<command>": "VAR" }`.
        let env = match value.get("env") {
            None => BTreeMap::new(),
            Some(Json::Str(var)) => commands.iter().map(|c| (c.clone(), var.clone())).collect(),
            Some(map) => {
                let map = string_map(map, "env")?;
                if let Some(c) = map.keys().find(|c| !commands.contains(c)) {
                    return Err(format!("`{flag}`: env names `{c}`, which it is not an option of"));
                }
                map
            }
        };
        let number = |key: &str| value.get(key).map(|v| v.as_f64().ok_or_else(|| format!("`{flag}`: \"{key}\" is not a number"))).transpose();
        Ok(Arg {
            value_name: value.get("value").and_then(Json::as_str).unwrap_or(bare).to_string(),
            min: number("min")?,
            max: number("max")?,
            help: value.get("help").and_then(Json::as_str).unwrap_or_default().to_string(),
            flag,
            kind,
            commands,
            env,
        })
    }

    /// `${PLUGIN_DIR}`, `${REPO}`, `${APP_DATA_DIR}` and `${CARGO}` in `text`.
    fn expand(&self, text: &str, base: &Base) -> String {
        text.replace("${PLUGIN_DIR}", &self.dir.display().to_string())
            .replace("${REPO}", &base.repo.display().to_string())
            .replace("${APP_DATA_DIR}", &base.app_data.as_ref().map(|d| d.display().to_string()).unwrap_or_default())
            .replace("${CARGO}", &std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
    }

    /// The program `run` names, with what every plugin program is given.
    fn program(&self, run: &[String], command: &str, base: &Base) -> Command {
        let run: Vec<String> = run.iter().map(|a| self.expand(a, base)).collect();
        let mut program = Command::new(&run[0]);
        program
            .args(&run[1..])
            .env("OMNI_PLUGIN_NAME", &self.name)
            .env("OMNI_PLUGIN_DIR", &self.dir)
            .env("OMNI_REPO", &base.repo)
            .env("OMNI_COMMAND", command);
        match &base.app_data {
            Some(dir) => program.env("OMNI_APP_DATA_DIR", dir),
            None => program.env_remove("OMNI_APP_DATA_DIR"),
        };
        program
    }
}

/// What every plugin program is told about the launcher.
pub struct Base {
    pub repo: PathBuf,
    pub app_data: Option<PathBuf>,
}

/// An installed plugin: where it is, whether it is on, and what loading it gave.
pub struct Entry {
    /// The name it is installed under.
    pub name: String,
    pub dir: PathBuf,
    pub linked: bool,
    pub enabled: bool,
    pub plugin: Result<Plugin, String>,
}

/// The plugins installed for this user.
pub struct Registry {
    pub dir: Option<PathBuf>,
    pub entries: Vec<Entry>,
}

/// Where this user's plugins are installed: `OMNI_PLUGINS`, else `<app-data>/../plugins`.
pub fn user_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OMNI_PLUGINS").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    Some(omni_platform::process::app_data_dir()?.parent()?.join("plugins"))
}

fn disabled_names(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(DISABLED))
        .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

impl Registry {
    /// The plugins installed in `dir`: each directory holding a manifest, and each `<name>.link`.
    pub fn discover(dir: Option<&Path>) -> Self {
        let mut entries = Vec::new();
        if let Some(dir) = dir {
            let disabled = disabled_names(dir);
            let mut found: Vec<_> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            found.sort();
            for path in found {
                let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (name, plugin_dir, linked) = if path.is_dir() && path.join(MANIFEST).is_file() {
                    (file_name, path, false)
                } else if path.extension().is_some_and(|e| e == LINK) {
                    let name = file_name.trim_end_matches(&format!(".{LINK}")).to_string();
                    let target = std::fs::read_to_string(&path).map(|t| PathBuf::from(t.trim())).unwrap_or_default();
                    (name, target, true)
                } else {
                    continue;
                };
                let plugin = Plugin::load(&plugin_dir).and_then(|p| {
                    if p.name == name {
                        Ok(p)
                    } else {
                        Err(format!("installed as `{name}`, but its manifest names `{}`", p.name))
                    }
                });
                entries.push(Entry { enabled: !disabled.contains(&name), name, dir: plugin_dir, linked, plugin });
            }
        }
        Registry { dir: dir.map(Path::to_path_buf), entries }
    }

    /// This user's plugins.
    pub fn installed() -> Self {
        Self::discover(user_dir().as_deref())
    }

    /// The plugins that are on, checked against each other: a broken one, or two that take the same
    /// option or command, is an error naming what to do.
    pub fn active(&self) -> Result<Vec<&Plugin>, String> {
        let mut active = Vec::new();
        let mut flags: BTreeMap<(&str, &str), &str> = BTreeMap::new();
        let mut commands: BTreeMap<&str, &str> = BTreeMap::new();
        for entry in self.entries.iter().filter(|e| e.enabled) {
            let plugin = entry.plugin.as_ref().map_err(|e| {
                format!("plugin `{}`: {e}\n  (fix it, or switch it off: omnidroid plugins disable {})", entry.name, entry.name)
            })?;
            for arg in &plugin.args {
                for command in &arg.commands {
                    if let Some(other) = flags.insert((command, &arg.flag), &plugin.name) {
                        return Err(format!("plugins `{other}` and `{}` both add `{command} {}`; disable one", plugin.name, arg.flag));
                    }
                }
            }
            for command in &plugin.commands {
                if let Some(other) = commands.insert(&command.name, &plugin.name) {
                    return Err(format!("plugins `{other}` and `{}` both add the command `{}`; disable one", plugin.name, command.name));
                }
            }
            active.push(plugin);
        }
        Ok(active)
    }
}

/// An option a plugin added, as given.
#[derive(Debug, Clone, PartialEq)]
pub struct Given {
    pub plugin: String,
    pub flag: String,
    /// The value, checked (a path made absolute); `None` for a switch.
    pub value: Option<String>,
}

fn check(arg: &Arg, text: &str) -> Result<String, String> {
    let number = match arg.kind {
        Kind::Switch => unreachable!("a switch has no value"),
        Kind::String => return Ok(text.to_string()),
        Kind::Path => return Ok(std::path::absolute(text).map_or_else(|_| text.to_string(), |p| p.display().to_string())),
        Kind::Integer => text.parse::<i64>().map(|n| n as f64).map_err(|_| format!("{} wants a whole number, not `{text}`", arg.flag))?,
        Kind::Number => text.parse::<f64>().ok().filter(|n| n.is_finite()).ok_or_else(|| format!("{} wants a number, not `{text}`", arg.flag))?,
    };
    if let Some(min) = arg.min.filter(|min| number < *min) {
        return Err(format!("{} wants {min} or more, not `{text}`", arg.flag));
    }
    if let Some(max) = arg.max.filter(|max| number > *max) {
        return Err(format!("{} wants {max} or less, not `{text}`", arg.flag));
    }
    Ok(text.to_string())
}

/// `command`'s arguments, split into the launcher's own and the ones `plugins` added; the plugins'
/// `defaults` go in front of the person's.
pub fn take_args(plugins: &[&Plugin], command: &str, args: Vec<String>) -> Result<(Vec<String>, Vec<Given>), String> {
    let mut all: Vec<String> = plugins.iter().flat_map(|p| p.defaults.get(command).cloned().unwrap_or_default()).collect();
    all.extend(args);
    let mut rest = Vec::new();
    let mut given = Vec::new();
    let mut args = all.into_iter();
    while let Some(arg) = args.next() {
        let found = plugins.iter().find_map(|p| {
            p.args.iter().find(|a| a.flag == arg && a.commands.iter().any(|c| c == command)).map(|a| (p, a))
        });
        let Some((plugin, spec)) = found else {
            rest.push(arg);
            continue;
        };
        let value = if spec.kind == Kind::Switch {
            None
        } else {
            let text = args.next().ok_or_else(|| format!("{arg} needs a value (<{}>)", spec.value_name))?;
            Some(check(spec, &text)?)
        };
        given.push(Given { plugin: plugin.name.clone(), flag: arg, value });
    }
    Ok((rest, given))
}

/// What the plugins made of a session: variables to set (`Some`) or remove (`None`), in order, and
/// the storage directory one named.
#[derive(Debug, Default)]
pub struct Session {
    pub env: Vec<(String, Option<String>)>,
    pub data_dir: Option<PathBuf>,
}

impl Session {
    /// The last value a plugin gave `key` (`None` when unset or never given).
    pub fn var(&self, key: &str) -> Option<&str> {
        self.env.iter().rev().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
    }

    /// Every variable onto `command`.
    pub fn apply(&self, command: &mut Command) {
        for (key, value) in &self.env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
    }
}

/// What a session's hooks are told about it.
pub struct SessionInfo<'a> {
    pub command: &'a str,
    pub fresh: bool,
    pub given_data_dir: Option<&'a Path>,
    pub apk: Option<&'a Path>,
    pub package: Option<&'a str>,
}

fn session_program(plugin: &Plugin, run: &[String], given: &[Given], info: &SessionInfo, base: &Base) -> Command {
    let mut program = plugin.program(run, info.command, base);
    program.env("OMNI_FRESH", if info.fresh { "1" } else { "0" });
    for (key, value) in [("OMNI_GIVEN_DATA_DIR", info.given_data_dir), ("OMNI_APK", info.apk)] {
        match value {
            Some(path) => program.env(key, path),
            None => program.env_remove(key),
        };
    }
    match info.package {
        Some(package) => program.env("OMNI_APK_PACKAGE", package),
        None => program.env_remove("OMNI_APK_PACKAGE"),
    };
    for arg in &plugin.args {
        program.env_remove(arg.hook_var());
    }
    for g in given.iter().filter(|g| g.plugin == plugin.name) {
        if let Some(arg) = plugin.args.iter().find(|a| a.flag == g.flag) {
            program.env(arg.hook_var(), g.value.as_deref().unwrap_or("1"));
        }
    }
    program
}

/// A `session-start` hook's directives, read into `session`.
fn read_directives(plugin: &str, out: &str, session: &mut Session) -> Result<(), String> {
    for line in out.lines() {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "env" => {
                let (key, value) = rest.split_once('=').filter(|(k, _)| !k.is_empty()).ok_or_else(|| format!("plugin `{plugin}`: `env` wants KEY=VALUE"))?;
                session.env.push((key.to_string(), Some(value.to_string())));
            }
            "unset" if !rest.is_empty() => session.env.push((rest.to_string(), None)),
            "data-dir" if !rest.is_empty() => session.data_dir = Some(PathBuf::from(rest)),
            "say" => println!("Omnidroid [{plugin}]: {rest}"),
            "error" => return Err(format!("[{plugin}] {rest}")),
            _ => println!("{line}"),
        }
    }
    Ok(())
}

/// A session's start: each plugin's `env`, the options it was given that name a variable, and its
/// `session-start` hook, in the order the plugins are installed.
pub fn start(plugins: &[&Plugin], given: &[Given], info: &SessionInfo, base: &Base) -> Result<Session, String> {
    let mut session = Session::default();
    for plugin in plugins {
        for scope in ["*", info.command] {
            for (key, value) in plugin.env.get(scope).into_iter().flatten() {
                session.env.push((key.clone(), Some(plugin.expand(value, base))));
            }
        }
        for g in given.iter().filter(|g| g.plugin == plugin.name) {
            let var = plugin.args.iter().find(|a| a.flag == g.flag).and_then(|a| a.env.get(info.command));
            if let Some(var) = var {
                session.env.push((var.clone(), Some(g.value.clone().unwrap_or_else(|| "1".to_string()))));
            }
        }
        let Some(run) = plugin.hooks.get("session-start") else { continue };
        let out = session_program(plugin, run, given, info, base)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| format!("plugin `{}`: could not run {}: {e}", plugin.name, plugin.expand(&run[0], base)))?;
        let text = String::from_utf8_lossy(&out.stdout);
        read_directives(&plugin.name, &text, &mut session)?;
        if !out.status.success() {
            return Err(format!("plugin `{}`: its session-start hook failed ({})", plugin.name, out.status));
        }
    }
    Ok(session)
}

/// A session's end: each plugin's `session-end` hook, told the session's exit code. Its output is
/// shown; a failure is said and changes nothing.
pub fn end(plugins: &[&Plugin], given: &[Given], info: &SessionInfo, base: &Base, code: i32) {
    for plugin in plugins {
        let Some(run) = plugin.hooks.get("session-end") else { continue };
        let status = session_program(plugin, run, given, info, base).env("OMNI_EXIT_CODE", code.to_string()).stdin(Stdio::null()).status();
        if !status.is_ok_and(|s| s.success()) {
            eprintln!("omnidroid: plugin `{}`: its session-end hook failed", plugin.name);
        }
    }
}

/// The plugin command named `name`, run with `args`; `None` when no plugin adds it.
pub fn run_command(plugins: &[&Plugin], name: &str, args: &[String], base: &Base) -> Option<Result<i32, String>> {
    let (plugin, command) = plugins.iter().find_map(|p| p.commands.iter().find(|c| c.name == name).map(|c| (p, c)))?;
    let status = plugin.program(&command.run, name, base).args(args).status();
    Some(match status {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(e) => Err(format!("plugin `{}`: could not run {}: {e}", plugin.name, plugin.expand(&command.run[0], base))),
    })
}

/// The plugins' part of the help.
pub fn usage(plugins: &[&Plugin]) -> String {
    let mut text = String::new();
    for plugin in plugins {
        text.push_str(&format!("\n\n  plugin {} {}{}", plugin.name, plugin.version, if plugin.description.is_empty() { String::new() } else { format!(" -- {}", plugin.description) }));
        for arg in &plugin.args {
            let head = if arg.kind == Kind::Switch { arg.flag.clone() } else { format!("{} <{}>", arg.flag, arg.value_name) };
            text.push_str(&format!("\n    {head:<22} {} ({})", arg.help, arg.commands.join(", ")));
        }
        for command in &plugin.commands {
            text.push_str(&format!("\n    {:<22} {}", command.name, command.help));
        }
    }
    text
}

/// A hint for an option the launcher does not know: the installed plugin that adds it and is
/// switched off, or else the plugin in the repository's `plugins/` that adds it, and how to install it.
pub fn hint_for(flag: &str, repo: &Path) -> Option<String> {
    let adds = |p: &Plugin| p.args.iter().any(|a| a.flag == flag) || p.commands.iter().any(|c| c.name == flag);
    // One installed and switched off says so first.
    let off = Registry::installed().entries.into_iter().filter(|e| !e.enabled).find_map(|e| e.plugin.ok().filter(|p| adds(p)));
    if let Some(p) = off {
        return Some(format!("`{flag}` comes from the `{}` plugin, which is switched off: omnidroid plugins enable {}", p.name, p.name));
    }
    let mut dirs: Vec<_> = std::fs::read_dir(repo.join("plugins")).ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter().filter_map(|d| Plugin::load(&d).ok()).find_map(|p| {
        adds(&p).then(|| format!("`{flag}` comes from the `{}` plugin, which is not installed: omnidroid plugins add --link {}", p.name, p.dir.display()))
    })
}

/// A directory copied, all but build output and version control.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(&name))?;
        } else {
            std::fs::copy(&path, to.join(&name))?;
        }
    }
    Ok(())
}

/// `omnidroid plugins add [--link] <dir>`, into `registry`'s directory.
pub fn add(registry: &Registry, from: &Path, link: bool) -> Result<String, String> {
    let dir = registry.dir.as_deref().ok_or("this host names no plugin directory; set OMNI_PLUGINS")?;
    let from = std::fs::canonicalize(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let plugin = Plugin::load(&from)?;
    if registry.entries.iter().any(|e| e.name == plugin.name) {
        return Err(format!("a plugin named `{}` is installed already (omnidroid plugins remove {} first)", plugin.name, plugin.name));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let installed = if link {
        let file = dir.join(format!("{}.{LINK}", plugin.name));
        std::fs::write(&file, from.display().to_string()).map_err(|e| format!("{}: {e}", file.display()))?;
        file
    } else {
        let to = dir.join(&plugin.name);
        copy_dir(&from, &to).map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
        to
    };
    // Checked beside what is installed: one that clashes is taken out again.
    if let Err(clash) = Registry::discover(Some(dir)).active() {
        let _ = remove(&Registry::discover(Some(dir)), &plugin.name);
        return Err(clash);
    }
    Ok(format!("installed {} {} ({})", plugin.name, plugin.version, installed.display()))
}

/// `omnidroid plugins remove <name>`: a copy is deleted, a link forgotten (its directory stays).
pub fn remove(registry: &Registry, name: &str) -> Result<String, String> {
    let entry = registry.entries.iter().find(|e| e.name == name).ok_or_else(|| format!("no plugin `{name}` is installed"))?;
    let dir = registry.dir.as_deref().ok_or("no plugin directory")?;
    if entry.linked {
        let file = dir.join(format!("{name}.{LINK}"));
        std::fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    } else {
        std::fs::remove_dir_all(&entry.dir).map_err(|e| format!("{}: {e}", entry.dir.display()))?;
    }
    set_enabled(registry, name, true).ok();
    Ok(format!("removed {name}"))
}

/// `omnidroid plugins enable|disable <name>`.
pub fn set_enabled(registry: &Registry, name: &str, enabled: bool) -> Result<String, String> {
    let dir = registry.dir.as_deref().ok_or("no plugin directory")?;
    let mut disabled = disabled_names(dir);
    disabled.retain(|n| n != name);
    if !enabled {
        if !registry.entries.iter().any(|e| e.name == name) {
            return Err(format!("no plugin `{name}` is installed"));
        }
        disabled.push(name.to_string());
    }
    let file = dir.join(DISABLED);
    let text: String = disabled.iter().map(|n| format!("{n}\n")).collect();
    std::fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(format!("{name} {}", if enabled { "enabled" } else { "disabled" }))
}

/// `omnidroid plugins new <name> [<dir>]`: a plugin to start from, with an option, a variable and a
/// hook-less manifest, and a README saying the rest.
pub fn scaffold(name: &str, dir: &Path) -> Result<String, String> {
    if !is_name(name) {
        return Err(format!("`{name}` is not lower-case letters, digits, `-` and `_`"));
    }
    if dir.join(MANIFEST).exists() {
        return Err(format!("{} holds a plugin already", dir.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let manifest = format!(
        r#"{{
  "name": "{name}",
  "version": "0.1.0",
  "description": "What this plugin is for",
  "args": [
    {{
      "flag": "--{name}-level",
      "type": "integer",
      "value": "n",
      "min": 1,
      "commands": ["play", "aosp"],
      "env": "OMNI_{upper}_LEVEL",
      "help": "an example option: handed to the session as OMNI_{upper}_LEVEL"
    }}
  ],
  "env": {{
    "*": {{}}
  }},
  "defaults": {{}},
  "hooks": {{}},
  "commands": {{}}
}}
"#,
        upper = name.replace('-', "_").to_ascii_uppercase()
    );
    let readme = format!(
        "# {name}\n\nAn omnidroid plugin. Install it for yourself with\n\n    omnidroid plugins add --link {}\n\n\
         and see `omnidroid help` for its options. What a manifest can declare -- `args`, `env`,\n\
         `defaults`, `hooks` (`session-start`, `session-end`) and `commands` -- is described in\n\
         `crates/omnidroid/src/plugin.rs` and `plugins/README.md`; `plugins/roblox` is a full example.\n",
        dir.display()
    );
    std::fs::write(dir.join(MANIFEST), manifest).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("README.md"), readme).map_err(|e| e.to_string())?;
    Plugin::load(dir)?;
    Ok(format!("made {} -- install it with: omnidroid plugins add --link {}", dir.join(MANIFEST).display(), dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_TEXT: &str = r#"{
      "name": "demo", "version": "1.2.3", "description": "a demo",
      "args": [
        {"flag": "--place", "type": "integer", "min": 1, "value": "id",
         "env": {"play": "OMNI_JOIN_PLACE", "aosp": "OMNI_R_PLACE"}, "help": "join it"},
        {"flag": "--turbo", "type": "switch", "commands": ["play"], "env": "OMNI_TURBO"},
        {"flag": "--delay", "type": "number", "min": 0, "commands": ["play"]}
      ],
      "env": {"*": {"OMNI_ALL": "${PLUGIN_DIR}/x"}, "aosp": {"OMNI_ONLY_AOSP": "1"}},
      "defaults": {"play": ["--phone"]},
      "commands": {"hello": {"run": ["echo", "hi"], "help": "says hi"}}
    }"#;

    fn demo() -> Plugin {
        Plugin::parse(MANIFEST_TEXT, Path::new("/p/demo")).expect("parsed")
    }

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_manifest_declares_options_variables_defaults_and_commands() {
        let p = demo();
        assert_eq!((p.name.as_str(), p.version.as_str(), p.args.len()), ("demo", "1.2.3", 3));
        assert_eq!(p.args[0].env.get("aosp").map(String::as_str), Some("OMNI_R_PLACE"));
        assert_eq!(p.args[1].commands, ["play"]);
        assert_eq!(p.args[2].hook_var(), "OMNI_ARG_DELAY");
        assert_eq!(p.defaults["play"], ["--phone"]);
        assert_eq!(p.commands[0].name, "hello");
    }

    #[test]
    fn a_bad_manifest_is_refused_with_what_is_wrong() {
        let bad = |text: &str| Plugin::parse(text, Path::new("/p")).unwrap_err();
        assert!(bad(r#"{"name": "Bad Name"}"#).contains("name"));
        assert!(bad(r#"{"name": "x", "args": [{"flag": "--apk"}]}"#).contains("launcher's own"));
        assert!(bad(r#"{"name": "x", "args": [{"flag": "--y", "type": "colour"}]}"#).contains("colour"));
        assert!(bad(r#"{"name": "x", "commands": {"play": {"run": ["a"]}}}"#).contains("play"));
        assert!(bad(r#"{"name": "x", "hooks": {"whenever": ["a"]}}"#).contains("whenever"));
        assert!(bad(r#"{"name": "x", "env": {"which": {}}}"#).contains("which"));
    }

    #[test]
    fn plugin_options_are_taken_out_checked_and_the_rest_left_to_the_launcher() {
        let p = demo();
        let (rest, given) = take_args(&[&p], "play", strings(&["--apk", "a.apk", "--place", "42", "--turbo", "--minutes", "3"])).unwrap();
        assert_eq!(rest, ["--phone", "--apk", "a.apk", "--minutes", "3"], "defaults first, then the person's");
        assert_eq!(given.len(), 2);
        assert_eq!((given[0].flag.as_str(), given[0].value.as_deref()), ("--place", Some("42")));
        assert_eq!((given[1].flag.as_str(), given[1].value.as_deref()), ("--turbo", None));
        assert!(take_args(&[&p], "play", strings(&["--place", "0"])).unwrap_err().contains("1 or more"));
        assert!(take_args(&[&p], "play", strings(&["--place", "x"])).is_err());
        assert!(take_args(&[&p], "play", strings(&["--place"])).is_err());
        let (rest, given) = take_args(&[&p], "aosp", strings(&["--turbo"])).unwrap();
        assert!(given.is_empty() && rest == ["--turbo"], "--turbo is play's only: the launcher says it is unknown");
    }

    #[test]
    fn a_session_gets_the_variables_and_hook_directives_are_read() {
        let p = demo();
        let base = Base { repo: PathBuf::from("/r"), app_data: None };
        let given = vec![Given { plugin: "demo".into(), flag: "--place".into(), value: Some("7".into()) }];
        let info = SessionInfo { command: "aosp", fresh: false, given_data_dir: None, apk: None, package: None };
        let session = start(&[&p], &given, &info, &base).unwrap();
        assert_eq!(session.var("OMNI_R_PLACE"), Some("7"));
        assert_eq!(session.var("OMNI_ALL"), Some("/p/demo/x"));
        assert_eq!(session.var("OMNI_ONLY_AOSP"), Some("1"));
        assert_eq!(session.var("OMNI_JOIN_PLACE"), None);
        let mut s = Session::default();
        read_directives("demo", "env A=b=c\nsay hello\nunset A\ndata-dir /d\nplain line", &mut s).unwrap();
        assert_eq!((s.var("A"), s.data_dir.as_deref()), (None, Some(Path::new("/d"))));
        assert_eq!(s.env[0], ("A".to_string(), Some("b=c".to_string())));
        assert!(read_directives("demo", "error no account", &mut s).unwrap_err().contains("no account"));
    }

    #[test]
    fn plugins_are_installed_switched_off_and_clashes_refused() {
        let root = std::env::temp_dir().join(format!("omnidroid-plugins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (src, installed) = (root.join("src"), root.join("installed"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(MANIFEST), MANIFEST_TEXT).unwrap();
        std::fs::create_dir_all(src.join("target")).unwrap();
        let registry = Registry::discover(Some(&installed));
        assert!(registry.entries.is_empty());
        add(&registry, &src, false).expect("added");
        assert!(installed.join("demo").join(MANIFEST).is_file() && !installed.join("demo/target").exists());
        let registry = Registry::discover(Some(&installed));
        assert_eq!(registry.active().unwrap().len(), 1);
        assert!(add(&registry, &src, true).is_err(), "the same name twice");
        set_enabled(&registry, "demo", false).unwrap();
        assert!(Registry::discover(Some(&installed)).active().unwrap().is_empty());
        set_enabled(&registry, "demo", true).unwrap();
        // Another plugin adding the same option clashes, and is taken out again.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(MANIFEST), r#"{"name": "other", "args": [{"flag": "--place"}]}"#).unwrap();
        let err = add(&Registry::discover(Some(&installed)), &other, true).unwrap_err();
        assert!(err.contains("--place"), "{err}");
        assert_eq!(Registry::discover(Some(&installed)).entries.len(), 1);
        remove(&Registry::discover(Some(&installed)), "demo").unwrap();
        assert!(Registry::discover(Some(&installed)).entries.is_empty());
        scaffold("mine", &root.join("mine")).expect("a new plugin loads");
        let _ = std::fs::remove_dir_all(&root);
    }
}
