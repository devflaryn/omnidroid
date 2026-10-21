//! **Why an instance's app ended**, read from its device's log: a Java exception, a native crash, an
//! ANR, the engine failing it, Android killing it, the app exiting by itself -- what the instance's
//! window then shows on a card (`device::show_card`) and the screen and the MCP server say.
//!
//! The device's log is the standard error of every host process of the device, merged: Android's
//! lines as logcat prints them (`omni_linux::socket::format_log`: `E/AndroidRuntime( 6856): ..`)
//! and the engine's own (`[zygote] ..`, `omni-linux: Killed { .. }`). **The number in a logcat
//! line's parentheses is not a guest pid**: it is the writing thread's id cut to 16 bits (liblog's
//! header holds no more) -- 6856 is thread 269000, the main thread of pid 269000 -- so it ties a
//! line to a process only loosely (`of_pid`). Which guest pids are the app's is read from the
//! lines that name them: the zygote's launch (`[zygote] launching <process> as pid <P> uid <U>`),
//! ActivityManager's records (`Killing <P>:<process>/u<user>a<app>`), a crash's own lines
//! (`Process: <package>, PID: <P>`). A pid is the instance's only when its process is the package's
//! and its user the instance's: user 0's system apps crash too, and another instance of the same
//! package runs as another user (`u11a116`, uid 1110116) on the same device.
//!
//! What ended the app, each taken over the ones after it: a Java exception; a native crash
//! (bionic's `Fatal signal` line, with the engine's account of the death); an ANR Android killed it
//! for; the engine failing the process; Android killing it; the process ending by itself; an ANR it
//! lived through, when nothing else says why. The app's main process is read first -- its last
//! one, when Android started it again (`last_end`) -- its others (`<package>:<name>`) only when
//! nothing ended the main one.
//!
//! **An end on purpose** is told from those ([`Diagnosis::on_purpose`]): a force-stop of the
//! package somebody asked for (Android's kill of it worded so, `asked`), an exit with status 0 and
//! the app's own SIGKILL with no crash before them, its activity finished while its process lived
//! (Back, its own exit) -- the pool then closes the instance with no card and no restart. A kill by
//! Android (its own stop of the package too), a signal it did not send itself and a death nothing
//! explains stay ends of their own.
use std::collections::HashMap;

use crate::extras::line_user;

/// The card's lines at most: the pool adds its own (where the whole log is, how to close it).
const MAX_LINES: usize = 30;
/// A longer line is cut, with `...`: a line of the log is evidence, not the card.
const MAX_WIDTH: usize = 200;
/// The summary's length at most: one line of the screen's instance list.
const MAX_SUMMARY: usize = 150;
/// Frames shown of an exception's root cause (the exception itself, when nothing caused it); of
/// the outermost exception of a chain; of each exception between.
const FRAMES: usize = 10;
const TOP_FRAMES: usize = 4;
const CAUSE_FRAMES: usize = 3;
/// The card's rows a Java exception is cut to, and their columns: the card holds 23 rows of 98
/// columns under its title at the pool's default instance size (1280x720, `omni_linux::card`'s
/// 12x24 cells and 4% margins), less the pool's three lines, the three its restart block takes
/// (an empty row, then the icon and headline's row and the link's: 58 px) and one to spare. Cut
/// to 30 lines instead, a three-deep chain lost its root cause -- the exception the summary names
/// -- off the card's bottom, while the outer exception's ten ActivityThread frames all showed
/// (2026-10-19); the card's detail rows give way from their end.
const CARD_ROWS: usize = 16;
const CARD_COLUMNS: usize = 98;
/// How many lines after its first a block's own lines (an exception's frames, an ANR's reasons, a
/// tombstone's backtrace) are looked for: other threads' lines interleave with them.
const BLOCK_SPAN: usize = 400;
/// How many lines before bionic's `Fatal signal` a process's fatal messages are looked for, at
/// least: from its first line on when the log names it earlier ([`fatal_said`]).
const ABORT_SPAN: usize = 300;
/// How many lines before a host process's end what the host printed of its own failure (a Rust
/// panic, Windows' `[host-crash]`) is looked for.
const HOST_SPAN: usize = 300;

/// What the device's log says about an app's end.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diagnosis {
    /// One line, for the card's title: e.g. `com.example.app crashed`.
    pub title: String,
    /// One line, for the screen's instance list and the MCP server: e.g. `native crash: SIGSEGV
    /// (bad memory access) at 0x8`.
    pub summary: String,
    /// The card's lines: what happened, then the evidence (an exception and its frames, the
    /// faulting instruction and its library). Where the whole log is, the pool adds.
    pub details: Vec<String>,
    /// The app was closed on purpose -- a force-stop, its activity finished while its process
    /// lived (Back, the app's own exit), an exit with status 0 or a SIGKILL of its own with no
    /// crash before it -- not a crash, a kill by Android, a signal it did not send itself or a
    /// death nothing tells: the pool then closes the instance as `stop_instance` does, no card and
    /// no restart. The last main process's end alone says (`last_end`).
    pub on_purpose: bool,
}

/// What `log` -- the device's log lines written since the instance's launch began -- says about
/// how `package`'s app, run as Android user `user`, ended.
#[must_use]
pub fn diagnose(log: &[String], package: &str, user: u32) -> Diagnosis {
    let log = Log::read(log, package, user);
    let [main, others] = log.pids();
    let found = last_end(&log, &main).or_else(|| (!others.is_empty()).then(|| ended(&log, &others)).flatten());
    found.unwrap_or_else(|| nothing(&log)).into_diagnosis(package)
}

/// How the app's main process ended, **its last process judged first**: Android starts it again
/// after a death (a crash's, a kill's), and when every process's crash was looked for before any
/// end the card named the first process's crash although the last one was SIGKILLed two minutes
/// later (2026-10-19). The one before it is judged only when the last says nothing of an end (it
/// lives); the latest crash of those before the one that ended is said after it (`Before that:
/// ..`), and whether the app was closed on purpose is that end's alone.
fn last_end(log: &Log<'_>, main: &[u32]) -> Option<Found> {
    let (k, mut found) = main.iter().enumerate().rev().find_map(|(k, &pid)| Some((k, main_end(log, pid)?)))?;
    let crashes: Vec<(u32, Found)> = main[..k].iter().filter_map(|&pid| Some((pid, ended(log, &[pid]).filter(|f| matches!(f.ending, Ending::Crashed))?))).collect();
    // The latest alone: the card has room for a line or two after the evidence.
    if let Some((pid, crash)) = crashes.last() {
        let more = if crashes.len() > 1 { format!(" (and {} earlier processes crashed before it)", crashes.len() - 1) } else { String::new() };
        found.details.push(format!("Before that: its earlier process (pid {pid}) ended: {}{more}", crash.summary));
    }
    Some(found)
}

/// How main process `pid` ended ([`ended`]), or that it quit ([`finished`]): its activity finished
/// while it lived is the app's end before any end of the process but a crash -- a process Back
/// left cached is Android's to trim later (`kill background`, `empty #1`), and a card for that, or
/// a restart, would bring back an app the person left -- and alone when nothing ended the process.
///
/// **But a finish after the app logged a crash, its process then ending by itself, is the crash's
/// end** ([`crash_said`]): an uncaught-exception handler of the app's own that logs the exception,
/// finishes its activities, then exits or kills its process -- taken for a quit, its card and its
/// restart were lost. A crash logged after the finish, a process Android trimmed later
/// (`Ending::Closed`) or one left cached leaves the finish the end: Back after a caught exception
/// the app logged is still a quit, unless the app then ends its process itself.
fn main_end(log: &Log<'_>, pid: u32) -> Option<Found> {
    let mut end = ended(log, &[pid]);
    if end.as_ref().is_some_and(|f| matches!(f.ending, Ending::Crashed)) {
        return end;
    }
    let Some(at) = finished(log, pid) else { return end };
    if let (Some(f), Some(said)) = (end.as_mut().filter(|f| matches!(f.ending, Ending::Stopped)), crash_said(log, pid, at)) {
        let said = format!("  {said}");
        // `exited` says it already for an exit with status 0.
        if !f.details.contains(&said) {
            f.details.push("Before its activity finished, the app logged an error, so it is not taken for a quit:".to_string());
            f.details.push(said);
        }
        return end;
    }
    let details = vec![
        format!("The app's activity left its display while its process (pid {pid}) lived: the person backed out of it (Back), or the app finished it (its own exit)."),
        format!("  {}", entry(log.lines[at]).map_or(log.lines[at], |e| e.msg)),
        end.map_or_else(
            || "No crash, no kill and no end of its process is in the log.".to_string(),
            |f| format!("Then, with no activity left, its process ended: {}", f.summary.strip_prefix("closed: ").unwrap_or(&f.summary)),
        ),
    ];
    Some(Found { ending: Ending::Quit, summary: "closed: its activity finished (Back, or the app's own exit)".to_string(), details })
}

/// The line where the last of `pid`'s activity windows went while it lived, when all went. Android
/// logs `Input channel object '<window> <package>/<activity> (client)' was disposed ..` as it
/// removes any window: before the first sign of the process's death ([`Log::dying`]) that is the
/// activity's finish, after it the death's (`WIN DEATH`, a force-stop's). Its windows are those
/// named the instance's user's (`Window{<window> u<user> <package>/..}`; the removal names no user)
/// first after its first line, and one named after the last removal is the activity back (a
/// relaunch, a rotation's new window). Back on TalkingTomCamp finished its activity so, twice: its
/// process left cached, no line of ActivityTaskManager's about it, no end (2026-10-19).
fn finished(log: &Log<'_>, pid: u32) -> Option<usize> {
    let to = log.dying(pid).unwrap_or(log.lines.len());
    let (ours, app) = (format!("u{} {}/", log.user, log.package), format!("{}/", log.package));
    let mut named: Vec<(&str, usize)> = Vec::new();
    let mut gone: Vec<(&str, usize)> = Vec::new();
    for (i, &line) in log.lines[..to].iter().enumerate() {
        for w in line.split("Window{").skip(1) {
            if let Some((window, _)) = w.split_once(' ').filter(|(_, rest)| rest.starts_with(&ours)) {
                if !named.iter().any(|(n, _)| *n == window) {
                    named.push((window, i));
                }
            }
        }
        if let Some((window, _)) = after(line, "Input channel object '").filter(|_| line.contains("' was disposed")).and_then(|w| w.split_once(' ')).filter(|(_, rest)| rest.starts_with(&app)) {
            gone.push((window, i));
        }
    }
    let first = log.first(pid);
    let mine: Vec<&str> = named.into_iter().filter(|&(_, at)| at >= first).map(|(w, _)| w).collect();
    if mine.is_empty() {
        return None;
    }
    mine.iter().map(|w| gone.iter().rev().find(|(g, _)| g == w).map(|&(_, at)| at)).collect::<Option<Vec<usize>>>()?.into_iter().max()
}

/// What ended the processes `pids`, in the module header's order. **An ANR ends nothing by
/// itself**: the person may wait (its dialog is shown here), and an input ANR's dialog goes once
/// the app answers again -- so it is the cause only when Android killed the process for it, and
/// one the app lived through is said beside what ended it later, or alone when nothing did.
fn ended(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    if let Some(found) = java(log, pids).or_else(|| native(log, pids)) {
        return Some(found);
    }
    let anr = anr(log, pids);
    if anr.as_ref().is_some_and(|a| a.killed) {
        return anr.map(|a| a.found);
    }
    match engine(log, pids).or_else(|| killed(log, pids)).or_else(|| exited(log, pids)) {
        Some(mut found) => {
            if let Some(a) = anr {
                found.details.push(format!("Before that, Android found it not responding (an ANR), and did not kill it for that: {}", a.said));
            }
            Some(found)
        }
        None => anr.map(|a| a.found),
    }
}

/// How the card's title words an end.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// A Java exception, a native crash, an ANR, the engine failing it.
    Crashed,
    /// Android killed it.
    Closed,
    /// It ended by itself, or nothing says why.
    Stopped,
    /// It was closed on purpose ([`Diagnosis::on_purpose`]).
    Quit,
}

/// A cause found: how the title words it, the summary, the card's lines.
struct Found {
    ending: Ending,
    summary: String,
    details: Vec<String>,
}

impl Found {
    /// As the card shows it: each line cut to [`MAX_WIDTH`], no more than [`MAX_LINES`] of them.
    fn into_diagnosis(self, package: &str) -> Diagnosis {
        let title = match self.ending {
            Ending::Crashed => format!("{package} crashed"),
            Ending::Closed => format!("{package} was closed by Android"),
            Ending::Stopped => format!("{package} stopped"),
            Ending::Quit => format!("{package} was closed"),
        };
        let mut details: Vec<String> = self.details.iter().map(|l| clip(&l.replace('\t', "  "), MAX_WIDTH)).collect();
        if details.len() > MAX_LINES {
            details.truncate(MAX_LINES - 1);
            details.push("...".to_string());
        }
        Diagnosis { title, summary: clip(&self.summary, MAX_SUMMARY), details, on_purpose: matches!(self.ending, Ending::Quit) }
    }
}

/// A logcat line (`omni_linux::socket::format_log`): `<level>/<tag>(<tid>): <message>`.
#[derive(Debug, Clone, Copy)]
struct Entry<'a> {
    level: u8,
    tag: &'a str,
    /// The writing thread's id, cut to 16 bits.
    tid: u32,
    msg: &'a str,
}

/// `line` as a logcat line, if it is one; a time first (`OMNI_LOG_TIME=1`: `<sec>.<ms> `) is
/// passed over.
fn entry(line: &str) -> Option<Entry<'_>> {
    let line = match line.split_once(' ') {
        Some((stamp, rest)) if stamp.contains('.') && stamp.bytes().all(|b| b.is_ascii_digit() || b == b'.') => rest,
        _ => line,
    };
    let b = line.as_bytes();
    if b.len() < 4 || b[1] != b'/' || !b"VDIWEF?".contains(&b[0]) {
        return None;
    }
    let close = line.find("):")?;
    let open = line[..close].rfind('(')?;
    let tid = line[open + 1..close].trim().parse().ok()?;
    let msg = &line[close + 2..];
    Some(Entry { level: b[0], tag: line[2..open].trim(), tid, msg: msg.strip_prefix(' ').unwrap_or(msg) })
}

/// Whether a logcat line's thread id `tid` (16 bits of it) can be one of `pid`'s threads: a guest
/// process's threads are the thousand ids from its pid on (`omni_linux::process`). Ids 65536 apart
/// look alike, so this is a check, never the evidence alone.
fn of_pid(tid: u32, pid: u32) -> bool {
    tid.wrapping_sub(pid & 0xffff) & 0xffff < 1000
}

/// What the log says of one guest process.
#[derive(Debug, Default)]
struct Proc {
    /// Its name: a package, `<package>:<name>`, or a name's last 15 characters (the main thread's
    /// name, as bionic's `Fatal signal` line gives it: `m.roblox.client`).
    name: String,
    /// Its Android user, when a line says (uid / 100000, `u<user>a<app>`).
    user: Option<u32>,
    /// The first line that names it.
    first: usize,
    /// Whether the log holds its launch (`[zygote] launching`).
    launched: bool,
}

/// Whether a process `name` is `package`'s: `Some(true)` for its main process (the package, or
/// the package's last 15 characters), `Some(false)` for another of its processes.
fn role(name: &str, package: &str) -> Option<bool> {
    if name == package || (name.len() == 15 && package.len() > 15 && package.ends_with(name)) {
        Some(true)
    } else if name.strip_prefix(package).is_some_and(|rest| rest.starts_with(':')) {
        Some(false)
    } else {
        None
    }
}

/// The log, read once: its lines, and the guest processes they name.
struct Log<'a> {
    lines: Vec<&'a str>,
    package: &'a str,
    user: u32,
    procs: HashMap<u32, Proc>,
}

impl<'a> Log<'a> {
    fn read(lines: &'a [String], package: &'a str, user: u32) -> Self {
        let mut log = Log { lines: lines.iter().map(|l| l.trim_end_matches(&['\r', '\n'][..])).collect(), package, user, procs: HashMap::new() };
        for i in 0..log.lines.len() {
            log.name(i);
        }
        log
    }

    /// Note what line `i` says of a process: its name, its user.
    fn name(&mut self, i: usize) {
        let line = self.lines[i];
        if let Some(rest) = after(line, "[zygote] launching ") {
            if let (Some((name, _)), Some(pid), Some(uid)) = (rest.split_once(" as pid "), number(rest, " as pid "), number::<u32>(rest, " uid ")) {
                self.note(pid, name, Some(uid / 100_000), i);
                self.procs.entry(pid).or_default().launched = true;
            }
            return;
        }
        let Some(e) = entry(line) else { return };
        match e.tag {
            "ActivityManager" | "ActivityTaskManager" => {
                let rec = e
                    .msg
                    .strip_prefix("Start proc ")
                    .or_else(|| e.msg.strip_prefix("Killing "))
                    .or_else(|| after(e.msg, "ProcessRecord{").and_then(|r| r.split_once(' ')).map(|(_, r)| r));
                if let Some((pid, name, user)) = rec.and_then(record) {
                    self.note(pid, name, Some(user), i);
                } else if let Some(rest) = e.msg.strip_prefix("Process ").filter(|r| r.contains(") has died")) {
                    if let (Some((name, _)), Some(pid)) = (rest.split_once(" (pid "), number(rest, " (pid ")) {
                        self.note(pid, name, None, i);
                    }
                } else if let Some(rest) = e.msg.strip_prefix("ANR in ") {
                    let name = rest.split_whitespace().next().unwrap_or_default();
                    if let Some(pid) = self.block(i, &e, |m| m.starts_with("ANR in ")).iter().find_map(|m| m.strip_prefix("PID: ")?.trim().parse().ok()) {
                        self.note(pid, name, None, i);
                    }
                }
            }
            "AndroidRuntime" => {
                if let Some(rest) = e.msg.strip_prefix("Process: ") {
                    if let (Some((name, _)), Some(pid)) = (rest.split_once(", PID: "), number(rest, ", PID: ")) {
                        self.note(pid, name, None, i);
                    }
                }
            }
            "libc" => {
                if let Some(f) = fatal(e.msg) {
                    self.note(f.pid, f.process, None, i);
                }
            }
            "lowmemorykiller" | "lmkd" => {
                if let Some(rest) = after(e.msg, "Kill '") {
                    if let (Some((name, _)), Some(pid), Some(uid)) = (rest.split_once("' ("), number(rest, "' ("), number::<u32>(rest, ", uid ")) {
                        self.note(pid, name, Some(uid / 100_000), i);
                    }
                }
            }
            // A tombstone's head: `pid: 249000, tid: 249033, name: Thread-5  >>> com.roblox.client <<<`.
            "DEBUG" => {
                if let (Some(pid), Some(name)) = (number(e.msg, "pid: "), after(e.msg, ">>> ").and_then(|r| r.split_whitespace().next())) {
                    self.note(pid, name, None, i);
                }
            }
            _ => {}
        }
    }

    /// Note a process: a longer name over a shorter (the whole one over its last 15 characters),
    /// a user once one is said.
    fn note(&mut self, pid: u32, name: &str, user: Option<u32>, at: usize) {
        let p = self.procs.entry(pid).or_insert_with(|| Proc { first: at, ..Proc::default() });
        if name.len() > p.name.len() {
            p.name = name.to_string();
        }
        if user.is_some() {
            p.user = user;
        }
    }

    /// The app's pids, its main process's then its others', each in the order they appeared: the
    /// pids whose process is the package's and whose user a line says is the instance's, named
    /// first no earlier than the first of them the log holds the launch of. Every app process is
    /// launched by the zygote, which names its uid, and the log begins as the instance's launch
    /// does: a pid named before -- the user's earlier process of the package, force-stopped as a
    /// cookie is planted (`extras::plant`) -- or put in no user was there before the app's start,
    /// another's. A log that holds no such launch (one that begins later) takes the instance's
    /// user's pids, and those put in no user while no line puts the package in another user.
    fn pids(&self) -> [Vec<u32>; 2] {
        let ours: Vec<(u32, &Proc, bool)> = self.procs.iter().filter_map(|(&pid, p)| Some((pid, p, role(&p.name, self.package)?))).collect();
        let start = ours.iter().filter(|(_, p, _)| p.launched && p.user == Some(self.user)).map(|(_, p, _)| p.first).min();
        let known = ours.iter().any(|(_, p, _)| p.user.is_some());
        let mut mine: Vec<_> = ours
            .into_iter()
            .filter(|(_, p, _)| match start {
                Some(start) => p.user == Some(self.user) && p.first >= start,
                None => p.user == Some(self.user) || (p.user.is_none() && !known),
            })
            .collect();
        mine.sort_by_key(|(_, p, _)| p.first);
        let mut out = [Vec::new(), Vec::new()];
        for (pid, _, main) in mine {
            out[usize::from(!main)].push(pid);
        }
        out
    }

    /// The first line that names `pid`.
    fn first(&self, pid: u32) -> usize {
        self.procs.get(&pid).map_or(0, |p| p.first)
    }

    /// Whether `pid` is the app's main process, not one of its others: a side process's exit says
    /// nothing of the app being quit.
    fn main(&self, pid: u32) -> bool {
        self.procs.get(&pid).and_then(|p| role(&p.name, self.package)) == Some(true)
    }

    /// The first line from `pid`'s first on that shows it dying or dead: ActivityManager's kill and
    /// its word of the death (`has died`, `Got obituary of`), lmkd's kill, a crash's own lines, its
    /// own `Process.killProcess` or `System.exit`, the engine's and the zygote's lines of its end;
    /// its window's death (`WIN DEATH`) and its activity's removal for it (`app died`), which name
    /// no pid: the instance's user's and package's. A window removed after it went with the death.
    fn dying(&self, pid: u32) -> Option<usize> {
        let keys = [
            format!("Killing {pid}:"),
            format!("(pid {pid}) has died"),
            format!("Got obituary of {pid}:"),
            format!("' ({pid})"),
            format!(", pid {pid} ("),
            format!("Sending signal. PID: {pid} SIG: "),
            format!("[zygote] pid {pid}:"),
            format!("[zygote] pid {pid} ended"),
            format!("[process {pid}] ended"),
        ];
        let ours = format!("u{} {}/", self.user, self.package);
        (self.first(pid)..self.lines.len()).find(|&i| {
            let l = self.lines[i];
            keys.iter().any(|k| l.contains(k.as_str()))
                || number::<u32>(l, ", PID: ") == Some(pid)
                || ((l.contains("WIN DEATH: Window{") || l.contains(": app died")) && l.contains(&ours))
                || entry(l).is_some_and(|e| of_pid(e.tid, pid) && (e.msg.starts_with("System.exit called") || e.msg.starts_with("VM exiting with result code")))
        })
    }

    /// The messages of the block entry `e`, at line `at`, begins: the lines after it the same
    /// thread wrote under the same tag, other threads' lines passed over, up to one of that thread
    /// under another tag or one `stop` says begins another block.
    fn block(&self, at: usize, e: &Entry<'_>, stop: impl Fn(&str) -> bool) -> Vec<&'a str> {
        let mut out = Vec::new();
        for &line in self.lines.iter().skip(at + 1).take(BLOCK_SPAN) {
            let Some(next) = entry(line) else { continue };
            if next.tid != e.tid {
                continue;
            }
            if next.tag != e.tag || stop(next.msg) {
                break;
            }
            out.push(next.msg);
        }
        out
    }

    /// The line `[zygote] pid <pid> ended: <status>` is, and the status.
    fn end(&self, pid: u32) -> Option<(usize, Exit)> {
        let key = format!("[zygote] pid {pid} ended: ");
        self.lines.iter().enumerate().rev().find_map(|(i, l)| Some((i, exit_status(after(l, &key)?)?)))
    }

    /// The signal Android sent `pid` that ended its host process: `[zygote] pid <pid>: signal <n>,
    /// its host process ended` (ActivityManager's SIGKILL, `kill` from a shell).
    fn sent(&self, pid: u32) -> Option<i32> {
        let key = format!("[zygote] pid {pid}: signal ");
        self.lines.iter().find_map(|l| number(l, &key))
    }

    /// The engine's line for `pid` ending itself with a failure, and from where: `[exit] pid <pid>
    /// tid <t> exit_group(<code>) pc .. lr ..` (`omni_linux::sys`; an exit with 0 says nothing).
    fn exit_call(&self, pid: u32) -> Option<&'a str> {
        let key = format!("[exit] pid {pid} tid ");
        self.lines.iter().rev().find(|l| l.contains(&key)).copied()
    }

    /// How `pid`'s host process ended, and the line that says so.
    fn death(&self, pid: u32) -> Option<(usize, Death)> {
        let (at, exit) = self.end(pid)?;
        if let Some(signal) = self.sent(pid) {
            return Some((at, Death::Sent(signal)));
        }
        let death = match exit {
            Exit::Signal(9) => Death::Outside,
            Exit::Signal(n) => Death::Host(format!("its host process died of signal {n} ({})", host_signal(n))),
            Exit::Code(c) if c >= 0xC000_0000 => {
                Death::Host(format!("its host process died of Windows exception {c:#010X}{}", windows_exception(c).map_or_else(String::new, |w| format!(" ({w})"))))
            }
            Exit::Code(c @ 129..=192) => Death::Guest(c as i32 - 128),
            Exit::Code(101) if self.exit_call(pid).is_none() => Death::Host("its host process panicked (exit code 101)".to_string()),
            Exit::Code(c) if c != 0 && self.exit_call(pid).is_none() => Death::HostExit(c),
            Exit::Code(c) => Death::Exited(c),
        };
        Some((at, death))
    }

    /// The engine's account of `pid`'s death. Its host process prints it last and names no pid,
    /// so it is `pid`'s when a line just before says that process ended (`[process <pid>] ended;
    /// tasks .. abandoned`, printed when some of its threads would not stop), or else when it is
    /// the one account of `signal` between line `from` and `pid`'s end -- or the one of those
    /// naming the package's own files: processes die together (three SIGABRTs, one account after
    /// another, none naming its pid, 2026-10-19).
    fn report(&self, pid: u32, from: usize, signal: Option<i32>) -> Option<Report> {
        let to = self.end(pid).map_or(self.lines.len(), |(at, _)| at);
        let mut open = Vec::new();
        for i in from.min(to)..to {
            let Some(report) = report_line(self.lines[i]) else { continue };
            let claimed = self.lines[i.saturating_sub(3)..i].iter().rev().find(|l| l.contains("] ended")).and_then(|l| number::<u32>(l, "[process "));
            match claimed {
                Some(p) if p == pid => return Some(report),
                Some(_) => {}
                None => open.push(report),
            }
        }
        let signal = signal?;
        open.retain(|r| r.signal == signal);
        if open.len() > 1 {
            let own = format!("/{}-", self.package);
            open.retain(|r| r.pc.iter().chain(&r.lr).any(|l| l.contains(&own)));
        }
        (open.len() == 1).then(|| open.remove(0))
    }

    /// What the host printed of a host process's own failure in the lines before line `to`, after
    /// line `from`: a Rust panic and its message, Windows' `[host-crash]` report, `omni-linux-run`'s
    /// error as it gives up (exit code 127: the guest's program could not be started). Those name
    /// no process: they may be another one's.
    fn host_words(&self, from: usize, to: usize) -> Vec<String> {
        let from = from.max(to.saturating_sub(HOST_SPAN)).min(to);
        let mut out = Vec::new();
        for i in from..to {
            let line = self.lines[i];
            if line.contains("panicked at ") {
                out.push(format!("  {line}"));
                out.extend(self.lines.get(i + 1).filter(|_| i + 1 < to).map(|l| format!("  {l}")));
            } else if line.contains("[host-crash]") || line.starts_with("omni-linux-run: ") {
                out.push(format!("  {line}"));
            }
        }
        let keep = out.len().saturating_sub(4);
        out.split_off(keep)
    }
}

/// How a host process ended, as `std::process::ExitStatus` prints it in the zygote's line:
/// `Ok(ExitStatus(unix_wait_status(<wait status>)))` on Unix, `Ok(ExitStatus(ExitStatus(<code>)))`
/// on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    /// Killed by this host signal (a wait status's low 7 bits).
    Signal(i32),
    /// Exited with this code.
    Code(u32),
}

fn exit_status(s: &str) -> Option<Exit> {
    if let Some(w) = number::<u32>(s, "unix_wait_status(") {
        return Some(if w & 0x7f != 0 { Exit::Signal((w & 0x7f) as i32) } else { Exit::Code((w >> 8) & 0xff) });
    }
    number(s, "ExitStatus(ExitStatus(").map(Exit::Code)
}

/// What a host process's end means for the guest process it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Death {
    /// Android sent it this signal, and the zygote ended its host process for it (`zygote::signal`).
    Sent(i32),
    /// Its host process was killed with SIGKILL, and not by Android: the computer's out-of-memory
    /// killer, another program.
    Outside,
    /// The guest was killed by this signal: its host process exits 128 + the signal
    /// (`omni-linux-run`).
    Guest(i32),
    /// The guest exited with this code.
    Exited(u32),
    /// The host process itself died -- of a host signal, a Windows exception, a Rust panic (101):
    /// the engine failed, not the app.
    Host(String),
    /// The host process exited with this code, not 0, and the guest asked for no such exit: the
    /// engine logs each one (`[exit] .. exit_group(<code>)`, `omni_linux::sys`). The host gave up
    /// (`omni-linux-run: ..`, 127), or something outside Android ended it (Windows' Task Manager
    /// and `taskkill` end a process with 1) -- which, the log does not say.
    HostExit(u32),
}

/// The engine's account of a guest process's death: `omni-linux: Killed { signal, pc, detail }`,
/// the detail as Rust's `{:?}` escaped it (`omni_linux::process::Process::report`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Report {
    signal: i32,
    /// The detail's first line: what ended the process.
    what: String,
    /// The `pc` and `lr` lines: an address, and the module and offset mapped there.
    pc: Option<String>,
    lr: Option<String>,
}

fn report_line(line: &str) -> Option<Report> {
    let rest = after(line, "omni-linux: Killed {")?;
    let signal = number(rest, "signal: ")?;
    let detail = after(rest, "detail: \"")?;
    let detail = unescape(&detail[..detail.rfind('"').unwrap_or(detail.len())]);
    let mut lines = detail.lines();
    let what = lines.next().unwrap_or_default().trim().to_string();
    let (mut pc, mut lr) = (None, None);
    for l in lines.map(str::trim) {
        if let Some(v) = l.strip_prefix("pc ") {
            pc = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("lr ") {
            lr = Some(v.to_string());
        }
    }
    Some(Report { signal, what, pc, lr })
}

impl Report {
    /// The card's lines of it: what ended the process, then where -- `pc` and `lr` with their
    /// modules; the stack pointer and the registers are left in the log.
    fn lines(&self) -> Vec<String> {
        let mut out = vec![format!("How the engine saw its process end: {}", self.what)];
        out.extend(self.pc.iter().map(|pc| format!("  pc {}", short_path(pc))));
        out.extend(self.lr.iter().map(|lr| format!("  lr {}", short_path(lr))));
        out
    }

    /// The library the crash is best placed in: `pc`'s, or `lr`'s when `pc` is in `[vdso]` (the
    /// signal's re-raise, [`vdso_note`]).
    fn module(&self) -> Option<String> {
        let pc = self.pc.as_deref().and_then(place);
        match pc {
            Some((module, _)) if module != "[vdso]" => Some(module),
            _ => self.lr.as_deref().and_then(place).map(|(module, _)| module),
        }
    }
}

/// The module and offset an address line names: `0x7c00b9c48c ([vdso]+0x48c)` is `[vdso]`,
/// `0x48c`; a path is its file's name.
fn place(line: &str) -> Option<(String, String)> {
    let inner = paren(line)?;
    let (path, offset) = inner.rsplit_once("+0x")?;
    Some((path.rsplit('/').next().unwrap_or(path).to_string(), format!("0x{offset}")))
}

/// What a [`Report`] says ended the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// A signal its own fault or abort raised, with no handler left to take it.
    Crash,
    /// A signal another process sent it, SIGKILL, a signal it sent itself that is no fault's.
    Sent,
    /// The engine could not go on running it: the CPU backend's error, an instruction it cannot
    /// run, a stop no guest code asked for.
    Engine,
}

fn cause(r: &Report) -> Cause {
    let w = r.what.as_str();
    let fault = matches!(r.signal, 4 | 5 | 6 | 7 | 8 | 11 | 16 | 31);
    if w.starts_with("the default action of signal") {
        if fault && !w.contains("sent by another process") {
            Cause::Crash
        } else {
            Cause::Sent
        }
    } else if w.starts_with("killed by SIGKILL") || w.ends_with("by init") {
        Cause::Sent
    } else if w.starts_with("seccomp: ")
        || w.starts_with("breakpoint at ")
        || w.contains(": no room for its frame")
        || w.split_once(" at 0x").is_some_and(|(access, _)| matches!(access, "Read" | "Write" | "Execute"))
    {
        // A fault of the guest's own with no handler for it (`Process::fault`), a system call
        // Android's filter refuses, a stack with no room for a signal's frame.
        Cause::Crash
    } else {
        Cause::Engine
    }
}

/// The note a [`Report`] whose `pc` is in `[vdso]` needs: bionic's crash handler (debuggerd's)
/// re-raised the signal with its default action and returned through the vDSO's signal trampoline
/// (`__kernel_rt_sigreturn`, +0x48c in `omni_linux::vdso`'s image), where the pending signal ended
/// the process -- so `pc` is the re-raise, and `lr` the best evidence of where the fault was.
fn vdso_note(r: &Report) -> Vec<String> {
    let Some((module, offset)) = r.pc.as_deref().and_then(place).filter(|(m, _)| m == "[vdso]") else { return Vec::new() };
    let lr = r.lr.as_deref().and_then(place).map_or_else(|| "not in a library".to_string(), |(m, o)| format!("{m}+{o}"));
    vec![
        format!("pc is in {module} (+{offset}), its signal trampoline: bionic's crash handler re-raised the signal with its default action"),
        format!("and returned through the trampoline, so pc is that re-raise, not the fault. lr is the best evidence of where: {lr}."),
    ]
}

/// A signal's name, and what it means in words: Android's numbers (Linux's).
fn signal_name(n: i32) -> (&'static str, &'static str) {
    match n {
        1 => ("SIGHUP", "hang-up"),
        2 => ("SIGINT", "interrupt"),
        3 => ("SIGQUIT", "quit"),
        4 => ("SIGILL", "illegal instruction"),
        5 => ("SIGTRAP", "trap: a breakpoint or trap instruction"),
        6 => ("SIGABRT", "abort"),
        7 => ("SIGBUS", "bus error: a misaligned or unbacked memory access"),
        8 => ("SIGFPE", "arithmetic error"),
        9 => ("SIGKILL", "killed"),
        11 => ("SIGSEGV", "bad memory access"),
        13 => ("SIGPIPE", "broken pipe"),
        15 => ("SIGTERM", "terminated"),
        16 => ("SIGSTKFLT", "stack fault"),
        31 => ("SIGSYS", "bad system call"),
        _ => ("a signal", "a signal"),
    }
}

/// A host process's death signal by name. The host is Linux or macOS (Windows has no signals),
/// whose numbers agree on the classic signals alone, and this crate does not ask which it runs on:
/// another number is named for both -- a fault the engine did not take ends a host process on the
/// Mac with SIGBUS, 10, Linux's SIGUSR1.
fn host_signal(n: i32) -> &'static str {
    match n {
        7 => "SIGBUS on Linux, SIGEMT on macOS",
        10 => "SIGBUS on macOS, SIGUSR1 on Linux",
        12 => "SIGSYS on macOS, SIGUSR2 on Linux",
        16 => "SIGSTKFLT on Linux, SIGURG on macOS",
        31 => "SIGSYS on Linux, SIGUSR2 on macOS",
        _ => signal_name(n).0,
    }
}

/// What a signal's code (bionic's `Fatal signal` line names it) says, in words.
fn code_words(code: &str) -> Option<&'static str> {
    Some(match code {
        "SEGV_MAPERR" => "nothing is mapped at that address",
        "SEGV_ACCERR" => "the memory there may not be accessed that way",
        "SEGV_MTEAERR" | "SEGV_MTESERR" => "a memory tag mismatch",
        "BUS_ADRALN" => "a misaligned address",
        "BUS_ADRERR" => "an address with no memory behind it",
        "ILL_ILLOPC" | "ILL_ILLOPN" | "ILL_ILLADR" => "an instruction the CPU does not have",
        "ILL_PRVOPC" => "a privileged instruction",
        "TRAP_BRKPT" => "a breakpoint instruction (brk)",
        "FPE_INTDIV" => "an integer divided by zero",
        "SI_TKILL" | "SI_USER" | "SI_QUEUE" => "sent by a process, as abort() sends it to itself",
        "SYS_SECCOMP" => "a system call Android's seccomp filter does not allow",
        _ => return None,
    })
}

/// What a Windows exception code is, for the common ones.
fn windows_exception(code: u32) -> Option<&'static str> {
    Some(match code {
        0xC000_0005 => "access violation",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer divided by zero",
        0xC000_00FD => "stack overflow",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "fail-fast: an abort or a failed security check",
        _ => return None,
    })
}

/// An exception of a Java crash: its line (and its message's further lines), its frames, and how
/// many frames Java itself left out (`... 12 more`).
#[derive(Debug, Default)]
struct Thrown<'a> {
    head: Vec<&'a str>,
    frames: Vec<&'a str>,
    more: usize,
}

impl Thrown<'_> {
    /// Its card lines with `n` frames: `Caused by: ` before a cause's line.
    fn show(&self, n: usize, cause: bool, out: &mut Vec<String>) {
        let mut head = self.head.iter();
        if let Some(first) = head.next() {
            out.push(if cause { format!("Caused by: {first}") } else { (*first).to_string() });
        }
        out.extend(head.take(1).map(|l| format!("  {l}")));
        out.extend(self.frames.iter().take(n).map(|f| format!("  at {f}")));
        let left = self.frames.len().saturating_sub(n) + self.more;
        if left > 0 {
            out.push(format!("  ... {left} more"));
        }
    }
}

/// The card's rows `lines` take at [`CARD_COLUMNS`], each as the card gets it (cut to
/// [`MAX_WIDTH`]).
fn card_rows(lines: &[String]) -> usize {
    lines.iter().map(|l| l.chars().count().min(MAX_WIDTH).div_ceil(CARD_COLUMNS).max(1)).sum()
}

/// The exception and what caused it, from a crash's lines after `Process: ..`: `Caused by: ` begins
/// each cause; a suppressed exception's lines (one tab deeper) are passed over.
fn thrown<'a>(block: &[&'a str]) -> Vec<Thrown<'a>> {
    let mut chain: Vec<Thrown<'a>> = Vec::new();
    for &m in block {
        if let Some(cause) = m.strip_prefix("Caused by: ") {
            chain.push(Thrown { head: vec![cause], ..Thrown::default() });
            continue;
        }
        let Some(last) = chain.last_mut() else {
            chain.push(Thrown { head: vec![m], ..Thrown::default() });
            continue;
        };
        if let Some(frame) = m.strip_prefix("\tat ") {
            last.frames.push(frame);
        } else if let Some(more) = m.strip_prefix("\t... ") {
            last.more = number(more, "").unwrap_or(0);
        } else if !m.starts_with('\t') && last.frames.is_empty() {
            last.head.push(m);
        }
    }
    chain
}

/// 1. A Java exception nothing caught: ART's `FATAL EXCEPTION` block of one of `pids`, the last
///    one. The block a failure to report it begins (`Error reporting crash`: the binder call to
///    ActivityManager failing on the driver app_process had closed, TalkingTomCamp 2026-10-19;
///    reported since, `omni_linux::zygote`'s `com.omnidroid.spare.App`) is not the cause.
///    The summary names the root cause: `Caused by` holds what an exception wraps
///    (`Unable to start activity` around the `NullPointerException`).
fn java(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    let mut last = None;
    for (i, &line) in log.lines.iter().enumerate() {
        let Some(e) = entry(line) else { continue };
        let Some(thread) = e.msg.strip_prefix("FATAL EXCEPTION: ") else { continue };
        if e.tag != "AndroidRuntime" {
            continue;
        }
        let block = log.block(i, &e, |m| m.starts_with("FATAL EXCEPTION") || m.starts_with("Error reporting crash"));
        let Some(pid) = block.first().and_then(|m| number::<u32>(m.strip_prefix("Process: ")?, ", PID: ")) else { continue };
        if pids.contains(&pid) {
            last = Some((thread.trim(), pid, block));
        }
    }
    let (thread, pid, block) = last?;
    let chain = thrown(&block[1..]);
    let root = chain.last()?.head.first()?;
    let details = |take: &[usize]| {
        let mut out = vec![format!("The app threw an exception nothing caught, on its thread \"{thread}\" (pid {pid}):")];
        for (k, (t, &n)) in chain.iter().zip(take).enumerate() {
            t.show(n, k > 0, &mut out);
        }
        out
    };
    // Frames as the card has room for them ([`CARD_ROWS`]): the root cause's most, and the outer
    // exceptions' (`Unable to start activity`, framework frames) given up first, each keeping one.
    let last = chain.len() - 1;
    let mut take: Vec<usize> = chain.iter().enumerate().map(|(k, t)| t.frames.len().min(if k == last { FRAMES } else if k == 0 { TOP_FRAMES } else { CAUSE_FRAMES })).collect();
    while card_rows(&details(&take)) > CARD_ROWS {
        let Some(k) = (0..=last).find(|&k| take[k] > 1) else { break };
        take[k] -= 1;
    }
    Some(Found { ending: Ending::Crashed, summary: format!("Java exception: {root}"), details: details(&take) })
}

/// Bionic's account of a fatal signal, from its crash handler: `Fatal signal 11 (SIGSEGV), code 1
/// (SEGV_MAPERR), fault addr 0x8 in tid 249033 (), pid 249000 (m.roblox.client)`.
#[derive(Debug, Clone, Copy)]
struct Fatal<'a> {
    signal: i32,
    /// The code's name: `SEGV_MAPERR`.
    code: Option<&'a str>,
    /// The faulting address, when the signal has one (SIGABRT has none).
    addr: Option<&'a str>,
    tid: u32,
    thread: &'a str,
    pid: u32,
    /// The main thread's name: the process's last 15 characters.
    process: &'a str,
}

fn fatal(msg: &str) -> Option<Fatal<'_>> {
    let rest = msg.strip_prefix("Fatal signal ")?;
    let tail = &rest[rest.rfind(", pid ")?..];
    Some(Fatal {
        signal: number(msg, "Fatal signal ")?,
        code: after(rest, "), code ").and_then(paren),
        addr: after(rest, "fault addr ").and_then(|a| a.split_whitespace().next()),
        tid: number(rest, " in tid ")?,
        thread: after(rest, " in tid ").and_then(paren).unwrap_or_default(),
        pid: number(tail, ", pid ")?,
        process: paren(tail).unwrap_or_default(),
    })
}

/// What `pid` said at the fatal level before bionic's `Fatal signal` line at `at`, each message
/// once (a message is often logged by its tag and again by ART), three at most: **the first burst**
/// of `F` lines one of its threads wrote from the process's first line on -- libc's `FORTIFY: ..`,
/// ART's `java_vm_ext.cc] JNI DETECTED ERROR ..` and `thread.cc] No pending exception expected:
/// ..`, `__android_log_assert`'s. Not the last ones: ART's abort logs its dump of every thread at
/// that level from the aborting thread (`Runtime aborting...`, then thousands of lines) between
/// its reason and the signal, so the burst ends there, and a message's own frames and a dump's
/// lines are passed over. A signal other than an abort can follow it too: Roblox's SIGSEGV in
/// libzstd-jni came 20 s into ART's abort for a `NoSuchMethodError` its `RegisterNatives` hit, 2465
/// lines after it (2026-10-19).
fn fatal_said<'a>(log: &Log<'a>, pid: u32, at: usize) -> Vec<&'a str> {
    /// What begins a dump's line or a frame, its source's place left out.
    const DUMP: &[&str] = &["\"", "|", "- ", "at ", "#", "native:", "DALVIK THREADS", "All threads", "Aborting thread", "Dumping all threads", "(no managed stack frames)"];
    let mut said: Vec<&'a str> = Vec::new();
    let mut thread = None;
    for &line in &log.lines[log.first(pid).min(at.saturating_sub(ABORT_SPAN))..at] {
        let Some(e) = entry(line) else { continue };
        // Its own threads': never crash_dump's, nor another app process's (its name the tag) whose
        // thread ids share these 16 bits.
        let others = e.tag == "DEBUG" || (e.tag.contains('.') && role(e.tag, log.package).is_none());
        if e.level != b'F' || !of_pid(e.tid, pid) || others || e.msg.starts_with("Fatal signal") || e.msg.contains("crash_dump") {
            continue;
        }
        if *thread.get_or_insert(e.tid) != e.tid {
            break;
        }
        let m = e.msg.trim();
        let bare = source_less(m).trim_start();
        if bare.starts_with("Runtime aborting") {
            break;
        }
        if bare.is_empty() || DUMP.iter().any(|d| bare.starts_with(d)) || said.iter().any(|s| source_less(s) == source_less(m)) {
            continue;
        }
        said.push(m);
        if said.len() == 3 {
            break;
        }
    }
    said
}

/// 2. A native crash: bionic's `Fatal signal` line for one of `pids` (the last), what the process
///    said at the fatal level before it ([`fatal_said`]: an abort's message), the tombstone if
///    crash_dump could write one, and the engine's account of the death. With no `Fatal signal`
///    line, the engine's account of a fault alone ([`native_report`]).
fn native(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    let last = log.lines.iter().enumerate().rev().find_map(|(i, &l)| {
        let e = entry(l).filter(|e| e.tag == "libc")?;
        let f = fatal(e.msg)?;
        pids.contains(&f.pid).then_some((i, e, f))
    });
    let Some((at, e, f)) = last else { return native_report(log, pids) };
    let (name, words) = signal_name(f.signal);
    let thread = if f.thread.is_empty() { String::new() } else { format!(" \"{}\"", f.thread) };
    let mut details = vec![format!("The app's native code crashed: signal {} ({name}, {words}) in thread {}{thread} of pid {}.", f.signal, f.tid, f.pid)];
    if let Some(code) = f.code {
        let addr = f.addr.map_or_else(String::new, |a| format!(" (fault address {a})"));
        details.push(code_words(code).map_or_else(|| format!("Code {code}{addr}."), |w| format!("{code}: {w}{addr}.")));
    }
    details.push(format!("  {}", e.msg));
    // An abort's reason; or, before another signal, the fatal error the process was going down
    // for when the signal came.
    let aborts = fatal_said(log, f.pid, at);
    if f.signal != 6 && !aborts.is_empty() {
        details.push("A fatal error came first, the likely cause (the signal came as the process went down):".to_string());
    }
    for (k, m) in aborts.iter().enumerate() {
        details.push(if k == 0 && f.signal == 6 { format!("Abort message: {m}") } else { format!("  {m}") });
    }
    let tomb = tombstone(log, f.pid, at);
    let helper_failed = log.block(at, &e, |_| false).iter().any(|m| m.starts_with("crash_dump helper failed"));
    match &tomb {
        Some(t) => {
            details.extend(t.abort.iter().map(|a| format!("Tombstone's abort message: {a}")));
            if !t.frames.is_empty() {
                details.push("Backtrace (the tombstone's):".to_string());
                details.extend(t.frames.iter().map(|f| format!("  {f}")));
            }
        }
        None if helper_failed => details.push("No tombstone (Android's crash report, with the backtrace) could be written: crash_dump failed to run here.".to_string()),
        None => details.push("No tombstone (Android's crash report, with the backtrace) is in the log.".to_string()),
    }
    let report = log.report(f.pid, at, Some(f.signal));
    if let Some(r) = &report {
        details.extend(r.lines());
        details.extend(vdso_note(r));
    }
    if f.signal == 4 {
        details.push("An illegal instruction here may be one the engine's CPU backend cannot run yet, not the app's own fault.".to_string());
    }
    let module = tomb.as_ref().and_then(|t| t.module.clone()).or_else(|| report.as_ref().and_then(Report::module));
    let mut summary = format!("native crash: {name} ({words})");
    let said = aborts.first().map(|m| source_less(m)).or_else(|| tomb.as_ref().and_then(|t| t.abort.as_deref()));
    if let Some(m) = said.filter(|_| f.signal == 6) {
        summary += &format!(": {m}");
    } else if let Some(m) = said {
        // The cause ahead of the signal, which is its consequence.
        summary = format!("native crash: {name} after a fatal error: {m}");
    } else {
        if let Some(a) = f.addr {
            summary += &format!(" at {a}");
        }
        // An abort is always in libc's abort(): that says nothing.
        if let Some(m) = module.filter(|m| !(f.signal == 6 && m == "libc.so")) {
            summary += &format!(" in {m}");
        }
    }
    Some(Found { ending: Ending::Crashed, summary, details })
}

/// A native crash bionic's handler did not report (no `Fatal signal` line): the engine's account
/// of a fault alone, for the last of `pids` it can be tied to.
fn native_report(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    pids.iter().rev().find_map(|&pid| {
        let signal = match log.death(pid) {
            Some((_, Death::Guest(s))) => Some(s),
            _ => None,
        };
        let r = log.report(pid, log.first(pid), signal).filter(|r| cause(r) == Cause::Crash)?;
        let (name, words) = signal_name(r.signal);
        let mut details = vec![format!("The app's native code crashed: signal {} ({name}, {words}) in pid {pid}.", r.signal)];
        details.extend(r.lines());
        details.extend(vdso_note(&r));
        let summary = format!("native crash: {name} ({words}){}", r.module().map_or_else(String::new, |m| format!(" in {m}")));
        Some(Found { ending: Ending::Crashed, summary, details })
    })
}

/// What crash_dump's tombstone says, when it could write one (`DEBUG` lines).
#[derive(Debug, Default)]
struct Tombstone {
    abort: Option<String>,
    /// The backtrace's first frames, their build ids left out.
    frames: Vec<String>,
    /// Frame #00's library.
    module: Option<String>,
}

/// The tombstone of `pid` written after line `from`: its head (`pid: <pid>, tid: ..`), then the
/// lines crash_dump's thread wrote after it.
fn tombstone(log: &Log<'_>, pid: u32, from: usize) -> Option<Tombstone> {
    let key = format!("pid: {pid}, tid: ");
    let (at, e) = log.lines.iter().enumerate().skip(from).take(BLOCK_SPAN).find_map(|(i, &l)| Some((i, entry(l).filter(|e| e.tag == "DEBUG" && e.msg.contains(&key))?)))?;
    let mut t = Tombstone::default();
    let mut backtrace = false;
    for m in log.block(at, &e, |_| false) {
        let m = m.trim();
        if let Some(abort) = m.strip_prefix("Abort message: ") {
            t.abort = Some(abort.trim_matches('\'').to_string());
        } else if m.starts_with("backtrace:") {
            backtrace = true;
        } else if backtrace && m.starts_with('#') {
            let frame = short_path(m.split(" (BuildId:").next().unwrap_or(m));
            if t.module.is_none() {
                t.module = m.split_whitespace().nth(3).map(|p| p.rsplit('/').next().unwrap_or(p).to_string());
            }
            if t.frames.len() < 8 {
                t.frames.push(frame);
            }
        } else if backtrace && !t.frames.is_empty() {
            break;
        }
    }
    Some(t)
}

/// An ANR of the app's, as [`anr`] read it.
struct Anr {
    /// The card's account of it.
    found: Found,
    /// Android killed the process for it.
    killed: bool,
    /// Its reason (else its first line), for the card of an end it did not cause.
    said: String,
}

/// 3. Android found the app not responding: ActivityManager's `ANR in <process>` block whose
///    `PID:` is one of `pids` (the last), its reason, and the kill that followed it -- for it
///    (`bg anr`; the dialog's close, `user request after error`) or not ([`ended`]).
fn anr(log: &Log<'_>, pids: &[u32]) -> Option<Anr> {
    let mut last = None;
    for (i, &line) in log.lines.iter().enumerate() {
        let Some(e) = entry(line).filter(|e| e.tag == "ActivityManager" && e.msg.starts_with("ANR in ")) else { continue };
        let block = log.block(i, &e, |m| m.starts_with("ANR in "));
        let Some(pid) = block.iter().find_map(|m| m.strip_prefix("PID: ")?.trim().parse::<u32>().ok()) else { continue };
        if pids.contains(&pid) {
            last = Some((i, e.msg, block, pid));
        }
    }
    let (at, head, block, pid) = last?;
    let reason = block.iter().find_map(|m| m.strip_prefix("Reason: ")).map(str::trim).filter(|r| !r.is_empty());
    let mut details = vec![format!("The app stopped responding (an ANR): it did not answer Android in time (pid {pid})."), format!("  {head}")];
    details.extend(block.iter().filter(|m| ["Reason: ", "Subject: ", "Parent: "].iter().any(|k| m.starts_with(k))).map(|m| format!("  {m}")));
    let kill = kill_after(log, pid, at);
    let killed = kill.as_deref().is_some_and(|k| k.contains("after error") || k.contains("user-terminated") || k.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("anr")));
    if let Some(kill) = kill.filter(|_| killed) {
        details.push(format!("Then Android killed it: {kill}"));
    }
    let summary = reason.map_or_else(|| "not responding (ANR)".to_string(), |r| format!("not responding (ANR): {r}"));
    Some(Anr { found: Found { ending: Ending::Crashed, summary, details }, killed, said: reason.unwrap_or(head).to_string() })
}

/// ActivityManager's reason for killing `pid`, from its first `Killing` line after line `from`.
fn kill_after(log: &Log<'_>, pid: u32, from: usize) -> Option<String> {
    log.lines.iter().skip(from).find_map(|&l| {
        let e = entry(l).filter(|e| e.tag == "ActivityManager")?;
        let (p, _, _) = record(e.msg.strip_prefix("Killing ")?)?;
        (p == pid).then(|| after(e.msg, "): ").unwrap_or(e.msg).to_string())
    })
}

/// 4. The engine failing the process, for the last of `pids` it did: its account of the death
///    names no signal of the app's own (an instruction the CPU backend cannot run, its error), or
///    the host process that ran it died itself (a host signal, a Windows exception, a Rust panic).
fn engine(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    for &pid in pids.iter().rev() {
        let death = log.death(pid);
        let signal = match &death {
            Some((_, Death::Guest(s))) => Some(*s),
            _ => None,
        };
        if let Some(r) = log.report(pid, log.first(pid), signal).filter(|r| cause(r) == Cause::Engine) {
            let mut details = vec![format!("The engine that runs Android here could not go on running the app's process (pid {pid}) and ended it:")];
            details.extend(r.lines());
            details.push("This is a failure of the engine, not of the app.".to_string());
            return Some(Found { ending: Ending::Crashed, summary: format!("emulation failure: {}", r.what), details });
        }
        if let Some((at, Death::Host(what))) = death {
            let mut details = vec![format!("The host process that ran the app's process (pid {pid}) failed: {what}."), "This is a failure of the engine that runs Android here, not of the app.".to_string()];
            let words = log.host_words(log.first(pid), at);
            if !words.is_empty() {
                details.push("What the host printed just before (perhaps of another process):".to_string());
                details.extend(words);
            }
            details.push(format!("  {}", log.lines[at]));
            return Some(Found { ending: Ending::Crashed, summary: format!("emulation failure: {what}"), details });
        }
    }
    None
}

/// What ActivityManager's reason for a kill means, for the common ones.
fn kill_hint(reason: &str) -> Option<&'static str> {
    Some(if reason.starts_with("empty") {
        "it had no activity left, and Android trims such empty processes"
    } else if reason.starts_with("stop user") {
        "its Android user was stopped"
    } else if reason.split_once(" due to ").is_some_and(|(stop, why)| stop.starts_with("stop ") && asked(why)) {
        "it was force-stopped (am force-stop, or its data cleared)"
    } else if reason.starts_with("stop ") && reason.contains(" due to ") {
        "Android stopped its package on its own, as it does when the package is installed over or removed"
    } else if reason.contains("remove task") {
        "its task was removed"
    } else if reason.contains("bg anr") {
        "it stopped responding in the background (an ANR)"
    } else if reason.contains("excessive cpu") {
        "it used too much CPU in the background"
    } else if reason.contains("cached") {
        "Android keeps a few cached processes, and it was one too many"
    } else if reason.contains("crash") {
        "it crashed"
    } else if reason.contains("depends on provider") {
        "a content provider it used went away"
    } else {
        return None;
    })
}

/// 5. Android killing the app: ActivityManager's `Killing <pid>:..` or the low-memory killer's
///    `Kill '<process>' (<pid>)` for one of `pids`, the last -- a force-stop's kill an end on
///    purpose ([`force_stopped`]).
fn killed(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    let mut last = None;
    for (i, &line) in log.lines.iter().enumerate() {
        let Some(e) = entry(line) else { continue };
        if e.tag == "ActivityManager" {
            if let Some((pid, _, _)) = e.msg.strip_prefix("Killing ").and_then(record).filter(|(p, _, _)| pids.contains(p)) {
                last = Some((i, pid, e.msg, false));
            }
        } else if matches!(e.tag, "lowmemorykiller" | "lmkd") {
            if let Some(pid) = after(e.msg, "Kill '").and_then(|r| number::<u32>(r, "' (")).filter(|p| pids.contains(p)) {
                last = Some((i, pid, e.msg, true));
            }
        }
    }
    let (at, pid, msg, lmk) = last?;
    let reason = after(msg, "): ").unwrap_or(msg).trim();
    if !lmk && reason.strip_prefix(&format!("stop {} due to ", log.package)).is_some_and(asked) {
        return Some(force_stopped(pid, force_stop_line(log, log.first(pid), at), Some(msg)));
    }
    let (summary, first) = if lmk {
        let reason = after(msg, "reason: ").map_or_else(|| "low memory".to_string(), |r| format!("low memory ({})", r.trim()));
        (format!("killed by Android: {reason}"), format!("Android's low-memory killer (lmkd) ended the app's process (pid {pid}) to free memory."))
    } else {
        let hint = kill_hint(reason).map_or_else(String::new, |h| format!(": {h}"));
        (format!("killed by Android: {reason}"), format!("Android's ActivityManager ended the app's process (pid {pid}){hint}."))
    };
    Some(Found { ending: Ending::Closed, summary, details: vec![first, format!("  {msg}")] })
}

/// Whether a force-stop's reason (`stop <package> due to <reason>`, `Force stopping ..: <reason>`)
/// is one somebody asked for: `from pid <n>` (a process asked: `am force-stop`, its settings' Force
/// stop, the pool's own stop) or `clear data`. Android stops a package in the same words on its own
/// as it is installed over or removed -- another build installed under its instances killed them so
/// --, and that is a kill by Android: a card, a restart.
fn asked(reason: &str) -> bool {
    reason.starts_with("from pid ") || reason.starts_with("clear data")
}

/// A force-stop of the package -- `am force-stop` (the MCP server's shell, a person's own), its
/// settings' Force stop, its data cleared -- closed it on purpose: its kill, `stop <package> due to
/// <reason>` (the reason [`asked`]), after `Force stopping <package> appid=.. user=<user>: ..`
/// ([`force_stop_line`]). The words are the same whoever asked (2026-10-19): the
/// pool's own stop of an app that ended comes after its log is read (`Pool::ended_on_its_own`), and
/// a closed instance's after it is closing, so neither is read as its end.
fn force_stopped(pid: u32, stop: Option<&str>, kill: Option<&str>) -> Found {
    let mut details = vec![format!("The app was force-stopped (am force-stop, its settings' Force stop, its data cleared): Android ended its process (pid {pid}).")];
    details.extend(stop.into_iter().chain(kill).map(|l| format!("  {l}")));
    Found { ending: Ending::Quit, summary: "closed: force-stopped".to_string(), details }
}

/// ActivityManager's `Force stopping <package> appid=<app> user=<user>: <reason>` of the package for
/// the instance's user (`-1`: every user), the reason [`asked`], between lines `from` and `to`, the
/// last: its message.
/// `Force stopping u10: finish user` is a user's stop (`stop user 10 due to finish user`), which
/// Android does on its own too (its running users' limit): not one of these.
fn force_stop_line<'a>(log: &Log<'a>, from: usize, to: usize) -> Option<&'a str> {
    log.lines[from.min(to)..to].iter().rev().find_map(|&l| {
        let e = entry(l).filter(|e| e.tag == "ActivityManager")?;
        let (name, rest) = e.msg.strip_prefix("Force stopping ")?.split_once(" appid=")?;
        let (user, why) = after(rest, " user=")?.split_once(':')?;
        let user: i64 = user.parse().ok()?;
        (name == log.package && (user == -1 || user == i64::from(log.user)) && asked(why.trim())).then_some(e.msg)
    })
}

/// What `pid`'s threads said of a crash at the error level in the lines before line `at` (from its
/// first line on, [`ABORT_SPAN`] at most): an exception's frames -- an uncaught-exception handler
/// of the app's own logs the exception and then kills the process or exits, as Android's does after
/// `FATAL EXCEPTION` ([`java`] takes that) -- or Android's handler failing to report one (`Error
/// reporting crash`, TalkingTomCamp 2026-10-19): the exception's line, or that one. An exit or a
/// SIGKILL of its own after such a line may be a crash's end, so never a quit: a card shown for a
/// quit costs a click, a crash taken for a quit loses its card and its restart.
fn crash_said<'a>(log: &Log<'a>, pid: u32, at: usize) -> Option<&'a str> {
    let mut head = None;
    for &line in &log.lines[log.first(pid).max(at.saturating_sub(ABORT_SPAN)).min(at)..at] {
        let Some(e) = entry(line).filter(|e| matches!(e.level, b'E' | b'F') && of_pid(e.tid, pid)) else { continue };
        if e.msg.starts_with("Error reporting crash") || e.msg.starts_with("FATAL EXCEPTION") {
            return Some(e.msg);
        }
        if e.msg.starts_with("\tat ") {
            return Some(head.unwrap_or(e.msg));
        }
        head = Some(e.msg);
    }
    None
}

/// 6. The process ending by itself, or by a signal no line explains, or its host process exiting
///    when it asked for no exit: the zygote's `pid <pid> ended` for the last of `pids` that has
///    one. The main process's exit with status 0, or its own SIGKILL, with no crash before it
///    ([`crash_said`]) is the app quitting -- its exit button's `System.exit(0)` or
///    `Process.killProcess(myPid())` -- and a signal sent after a force-stop the force-stop's.
fn exited(log: &Log<'_>, pids: &[u32]) -> Option<Found> {
    let (pid, at, death) = pids.iter().rev().find_map(|&pid| log.death(pid).map(|(at, d)| (pid, at, d)))?;
    let ended = format!("  {}", log.lines[at]);
    let mut ending = Ending::Stopped;
    let (summary, mut details) = match death {
        Death::Exited(code) => {
            let quit = code == 0 && log.main(pid);
            let said = quit.then(|| crash_said(log, pid, log.dying(pid).unwrap_or(at))).flatten();
            if quit && said.is_none() {
                ending = Ending::Quit;
                // ART says it first, on the thread that called it: `System.exit called, status: 0`.
                let call = log.lines[log.first(pid).min(at)..at].iter().rev().find_map(|&l| entry(l).filter(|e| of_pid(e.tid, pid) && e.msg.starts_with("System.exit called")));
                let mut details = vec![format!("The app's process (pid {pid}) ended itself with exit status 0, no crash before it: the app quit.")];
                details.extend(call.map(|e| format!("  {}", e.msg)));
                ("closed: the app quit (exit status 0)".to_string(), details)
            } else {
                let mut details = vec![format!("The app's process (pid {pid}) ended itself with exit code {code}.")];
                if let Some(call) = log.exit_call(pid) {
                    details.push("Its exit, and where it was called from:".to_string());
                    details.push(format!("  {}", short_path(call)));
                }
                if let Some(said) = said {
                    details.push("Before it, the app logged an error, so it is not taken for a quit:".to_string());
                    details.push(format!("  {said}"));
                }
                (format!("exited by itself (code {code})"), details)
            }
        }
        Death::Guest(signal) => {
            let (name, words) = signal_name(signal);
            // `Process.killProcess` says so first: `I/Process(<tid>): Sending signal. PID: <pid> SIG: 9`.
            let key = format!("Sending signal. PID: {pid} SIG: {signal}");
            let sent = log.lines.iter().enumerate().rev().find_map(|(i, &l)| Some((i, entry(l).filter(|e| e.msg.starts_with(&key))?)));
            let own = sent.filter(|(_, e)| of_pid(e.tid, pid));
            let said = own.and_then(|(i, _)| crash_said(log, pid, i));
            if let (9, true, Some((_, e)), None) = (signal, log.main(pid), own, said) {
                ending = Ending::Quit;
                ("closed: the app quit itself".to_string(), vec![format!("The app's process (pid {pid}) killed itself (Process.killProcess), no crash before it: the app quit."), format!("  {}", e.msg)])
            } else {
                let mut details = vec![format!("The app's process (pid {pid}) was killed by signal {signal} ({name}, {words}).")];
                if let Some((_, e)) = sent {
                    details.push(if of_pid(e.tid, pid) { "It sent the signal itself (Process.killProcess):" } else { "Another process sent it:" }.to_string());
                    details.push(format!("  {}", e.msg));
                }
                if let Some(said) = said {
                    details.push("Before it, the app logged an error, so it is not taken for a quit:".to_string());
                    details.push(format!("  {said}"));
                }
                if let Some(r) = log.report(pid, log.first(pid), Some(signal)) {
                    details.extend(r.lines());
                }
                (format!("killed by signal {signal} ({name})"), details)
            }
        }
        Death::Sent(signal) => {
            // A force-stop Android logged no kill of ([`killed`] reads the ones it did).
            if let Some(stop) = force_stop_line(log, log.first(pid), at) {
                let mut found = force_stopped(pid, Some(stop), None);
                found.details.push(ended);
                return Some(found);
            }
            let name = signal_name(signal).0;
            (format!("killed by signal {signal} ({name})"), vec![format!("Something in Android sent the app's process (pid {pid}) signal {signal} ({name}) and logged no reason; its host process was ended.")])
        }
        Death::Outside => (
            "killed by signal 9 (SIGKILL)".to_string(),
            vec![format!("The host process that ran the app's process (pid {pid}) was killed from outside Android: by the computer's out-of-memory killer, or another program.")],
        ),
        Death::HostExit(code) => {
            let mut details = vec![format!(
                "The host process that ran the app's process (pid {pid}) ended with code {code}, an exit the app did not ask for: the host gave up, or something outside Android ended it."
            )];
            let words = log.host_words(log.first(pid), at);
            if !words.is_empty() {
                details.push("What the host printed just before (perhaps of another process):".to_string());
                details.extend(words);
            }
            (format!("its host process ended (code {code})"), details)
        }
        Death::Host(_) => return None,
    };
    details.push(ended);
    Some(Found { ending, summary, details })
}

/// 7. Nothing says why: the last lines naming the package, but not another user's.
fn nothing(log: &Log<'_>) -> Found {
    let mut details = vec!["The app left its screen; the device log says nothing about why (it may still run in the background, or have finished its activity).".to_string()];
    let naming: Vec<&str> = log.lines.iter().rev().filter(|l| l.contains(log.package) && !other_user(l, log.user)).take(5).copied().collect();
    if !naming.is_empty() {
        details.push("The last lines naming it:".to_string());
        details.extend(naming.iter().rev().map(|l| format!("  {}", short_path(l))));
    }
    Found { ending: Ending::Stopped, summary: "the app stopped".to_string(), details }
}

/// Whether a line names a user other than `user`: an Activity's record or start ([`line_user`]),
/// a `u<user>` word (`Window{dafc344 u10 ..}`, `/u10a116`).
fn other_user(line: &str, user: u32) -> bool {
    if line_user(line).is_some_and(|u| u != user) {
        return true;
    }
    line.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        let Some(digits) = w.strip_prefix('u') else { return false };
        let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
        let rest = &digits[end..];
        let app = rest.is_empty() || (matches!(rest.as_bytes()[0], b'a' | b's' | b'i') && rest[1..].bytes().all(|b| b.is_ascii_digit()));
        end > 0 && app && digits[..end].parse::<u32>().is_ok_and(|u| u != user)
    })
}

/// ActivityManager's short name of a process, `<pid>:<process>/u<user><a|s|i><app>`: its pid,
/// process and user.
fn record(s: &str) -> Option<(u32, &str, u32)> {
    let (pid, rest) = s.split_once(':')?;
    let (name, rest) = rest.split_once('/')?;
    let user = rest.strip_prefix('u')?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some((pid.parse().ok()?, name, user))
}

/// What follows `key` in `s`.
fn after<'s>(s: &'s str, key: &str) -> Option<&'s str> {
    s.find(key).map(|at| &s[at + key.len()..])
}

/// The number right after `key` in `s`.
fn number<T: std::str::FromStr>(s: &str, key: &str) -> Option<T> {
    let rest = after(s, key)?;
    rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())].parse().ok()
}

/// What the first parentheses in `s` hold.
fn paren(s: &str) -> Option<&str> {
    let open = s.find('(')? + 1;
    Some(&s[open..open + s[open..].find(')')?])
}

/// A fatal message without its source's place: `runtime.cc:708] Runtime aborting` is `Runtime
/// aborting`, and `thread.cc:2560]` alone (a message's last line, empty) is empty.
fn source_less(m: &str) -> &str {
    match m.split_once(']') {
        Some((place, rest)) if place.contains(':') && !place.contains(' ') && (rest.is_empty() || rest.starts_with(' ')) => rest.strip_prefix(' ').unwrap_or(rest),
        _ => m,
    }
}

/// A string as Rust's `{:?}` escaped it, back: `\n`, `\t`, `\"`, `\\`, `\u{..}`.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let hex: String = chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// `s` with an installed app's two directories (`/data/app/~~<random>/<package>-<random>/`) as
/// `/data/app/.../`: random names that say nothing and take half a line.
fn short_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("/data/app/") {
        let mut parts = rest[at + "/data/app/".len()..].splitn(3, '/');
        let (Some(_), Some(_), Some(tail)) = (parts.next(), parts.next(), parts.next()) else { break };
        out.push_str(&rest[..at]);
        out.push_str("/data/app/.../");
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// `s` cut to `max` characters, the cut said with `...`.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|l| (*l).to_string()).collect()
    }

    /// What every card must be: a title, a summary, lines that fit.
    fn fits(d: &Diagnosis) {
        assert!(!d.summary.is_empty() && d.summary.chars().count() <= MAX_SUMMARY, "{d:#?}");
        assert!(!d.details.is_empty() && d.details.len() <= MAX_LINES, "{d:#?}");
        assert!(d.details.iter().all(|l| l.chars().count() <= MAX_WIDTH && !l.contains('\t')), "{d:#?}");
    }

    /// TalkingTomCamp killed by `am crash` (2026-10-19): the exception, the failed report of it,
    /// the app's own SIGKILL. No launch line: the crash's `Process:` line names the pid.
    const JAVA: &[&str] = &[
        "D/AndroidRuntime( 6856): Shutting down VM",
        "E/AndroidRuntime( 6856): FATAL EXCEPTION: main",
        "E/AndroidRuntime( 6856): Process: com.outfit7.talkingtomcamp, PID: 269000",
        "E/AndroidRuntime( 6856): android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash",
        "E/AndroidRuntime( 6856): \tat android.app.ActivityThread.throwRemoteServiceException(ActivityThread.java:2257)",
        "E/AndroidRuntime( 6856): \tat android.app.ActivityThread.-$$Nest$mthrowRemoteServiceException(Unknown Source:0)",
        "E/AndroidRuntime( 6856): \tat android.app.ActivityThread$H.handleMessage(ActivityThread.java:2557)",
        "W/StorageManagerService( 1020): storaged not found; trying again",
        "E/AndroidRuntime( 6856): \tat android.os.Handler.dispatchMessage(Handler.java:107)",
        "E/AndroidRuntime( 6856): \tat android.os.Looper.loopOnce(Looper.java:232)",
        "E/AndroidRuntime( 6856): \tat android.os.Looper.loop(Looper.java:317)",
        "E/AndroidRuntime( 6856): \tat android.app.ActivityThread.main(ActivityThread.java:8705)",
        "E/AndroidRuntime( 6856): \tat java.lang.reflect.Method.invoke(Native Method)",
        "E/AndroidRuntime( 6856): \tat com.android.internal.os.RuntimeInit$MethodAndArgsCaller.run(RuntimeInit.java:580)",
        "E/AndroidRuntime( 6856): \tat com.android.internal.os.WrapperInit.main(WrapperInit.java:94)",
        "E/AndroidRuntime( 6856): \tat com.android.internal.os.RuntimeInit.nativeFinishInit(Native Method)",
        "E/AndroidRuntime( 6856): \tat com.android.internal.os.RuntimeInit.main(RuntimeInit.java:369)",
        "D/ActivityThread( 6856): Too many transaction errors, throttling freezer binder callback.",
        "E/AndroidRuntime( 6856): Error reporting crash",
        "E/AndroidRuntime( 6856): java.lang.RuntimeException: Bad file descriptor",
        "E/AndroidRuntime( 6856): \tat android.os.BinderProxy.transactNative(Native Method)",
        "E/AndroidRuntime( 6856): \tat com.outfit7.talkingtomcamp.TTCTopExceptionHandler.uncaughtException(TTCTopExceptionHandler.java:48)",
        "I/Process( 6856): Sending signal. PID: 269000 SIG: 9",
        "[process 269000] ended; tasks [269008, 269009] did not stop and are abandoned",
        "omni-linux: Killed { signal: 9, pc: 537639253032, detail: \"the default action of signal 9\\n  pc 0x7d2dcbf428 (/apex/com.android.runtime/lib64/bionic/libc.so+0xbd428)\\n  lr 0x71ac4514 (/system/framework/arm64/boot-framework.oat+0x1f4514)\\n  sp 0x7d00b95db0\\n\" }",
        "[zygote] pid 269000 ended: Ok(ExitStatus(unix_wait_status(35072)))",
        "I/WindowManager( 1050): WIN DEATH: Window{dafc344 u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}",
        "I/ActivityManager( 1272): Process com.outfit7.talkingtomcamp (pid 269000) has died: fg  TOP ",
    ];

    /// Roblox's SIGSEGV in libzstd-jni (2026-10-19): crash_dump could not run, the engine's account
    /// has pc in [vdso]. The main thread's name is the package's last 15 characters.
    const ROBLOX: &[&str] = &[
        "I/ActivityManager( 1050): Killing 277000:com.android.managedprovisioning/u10a68 (adj 905): empty #1",
        "[zygote] pid 277000 ended: Ok(ExitStatus(unix_wait_status(9)))",
        "F/libc(52425): Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x8 in tid 249033 (), pid 249000 (m.roblox.client)",
        "[t] +57.6s",
        "E/DEBUG( 4000): failed to open directory /proc/249033/fd: No such file or directory",
        "F/crash_dump64( 4000): crash_dump.cpp:609] failed to get process threads: No such file or directory",
        "E/libc( 4000): failed to connect to tombstoned: No such file or directory",
        "[exit] pid 4000 tid 4000 exit_group(1) pc 0x8c04b439e4 lr 0x8c04ae5fb4",
        "F/libc(52425): crash_dump helper failed to exec, or was killed",
        "D/AndroidRuntime(16856): >>>>>> START com.android.internal.os.RuntimeInit uid 1001027 <<<<<<",
        "[process 249000] ended; tasks [249008, 249009] did not stop and are abandoned",
        "omni-linux: Killed { signal: 11, pc: 544603105112, detail: \"the default action of signal 11\\n  pc 0x7c00b9c48c ([vdso]+0x48c)\\n  lr 0x7eccdfe6d8 (/data/app/~~GltRtz_PyFisxlBre6kglQ==/com.roblox.client-kSNCAZ8BCEMCFPszDG2qfQ==/lib/arm64/libzstd-jni-1.5.7-6.so+0x5f86d8)\\n  sp 0x7ee016f780\\n  x0  0x0000007eccdfe504  x1  0x0000007de863bd30  x2  0x0000000000000020  x3  0x0000000000000010\\n\" }",
        "  guest space: 213 MiB committed of a 3584 MiB ceiling, 15990 MiB mapped",
        "[binder] pid 249000 closed its driver: 1 objects released",
        "I/ActivityManager( 1210): Process com.roblox.client (pid 249000) has died: fg  TOP ",
        "W/ActivityTaskManager( 1210): ProcessRecord{2cd3d41 249000:com.roblox.client/u10a116} is removed with pending start ActivityRecord{db9940c u10 com.roblox.client/.startup.LauncherAliasMain t3}",
        "[zygote] pid 249000 ended: Ok(ExitStatus(unix_wait_status(35584)))",
    ];

    const LAUNCH: &str = "[zygote] launching com.a as pid 310000 uid 1010116: android.app.ActivityThread seq=50";

    #[test]
    fn a_java_exception_is_the_cause_and_its_frames_the_evidence() {
        let d = diagnose(&lines(JAVA), "com.outfit7.talkingtomcamp", 10);
        fits(&d);
        assert_eq!(d.title, "com.outfit7.talkingtomcamp crashed");
        assert!(!d.on_purpose, "the handler's own SIGKILL after it is the crash's end: {d:#?}");
        assert_eq!(d.summary, "Java exception: android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash");
        assert!(d.details[0].contains("thread \"main\" (pid 269000)"), "{:#?}", d.details);
        assert_eq!(d.details[1], "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash");
        assert_eq!(d.details[2], "  at android.app.ActivityThread.throwRemoteServiceException(ActivityThread.java:2257)");
        assert_eq!(d.details.iter().filter(|l| l.starts_with("  at ")).count(), FRAMES, "another thread's line between the frames is passed over");
        assert_eq!(d.details.last().unwrap(), "  ... 2 more");
    }

    #[test]
    fn the_failure_to_report_a_crash_is_not_its_cause() {
        let d = diagnose(&lines(JAVA), "com.outfit7.talkingtomcamp", 10);
        assert!(!d.summary.contains("Bad file descriptor") && d.details.iter().all(|l| !l.contains("Bad file descriptor")), "{d:#?}");
        // Alone, the secondary block is no Java exception: the app's own SIGKILL is what is left.
        let secondary: Vec<&str> = JAVA.iter().copied().filter(|l| !l.contains("FATAL EXCEPTION")).collect();
        let d = diagnose(&lines(&secondary), "com.outfit7.talkingtomcamp", 10);
        fits(&d);
        assert_eq!(d.summary, "killed by signal 9 (SIGKILL)");
        assert!(d.details.iter().any(|l| l.contains("sent the signal itself")), "{d:#?}");
        // The failed report says a crash came first: no quit.
        assert!(!d.on_purpose && d.details.iter().any(|l| l.starts_with("Before it, the app logged an error")), "{d:#?}");
    }

    /// The same `am crash` reported to ActivityManager (a pool instance under `omni_linux::zygote`'s
    /// `com.omnidroid.spare.App`, 2026-10-20): no `Shutting down VM` before it, no failed report;
    /// Android's handling of it (no dialog for a background user, the DropBox entry) beside the
    /// app's own SIGKILL, its user named by ActivityManager's record alone. Frames and other
    /// processes' lines left out.
    #[test]
    fn a_crash_reported_to_activity_manager_is_its_java_exception() {
        let reported = lines(&[
            "E/AndroidRuntime(63320): FATAL EXCEPTION: main",
            "E/AndroidRuntime(63320): Process: com.outfit7.talkingtomcamp, PID: 391000",
            "E/AndroidRuntime(63320): android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash",
            "E/AndroidRuntime(63320): \tat android.app.ActivityThread.throwRemoteServiceException(ActivityThread.java:2257)",
            "E/AndroidRuntime(63320): \tat android.app.ActivityThread.main(ActivityThread.java:8705)",
            "E/AndroidRuntime(63320): \tat com.android.internal.os.WrapperInit.main(WrapperInit.java:94)",
            "E/AndroidRuntime(63320): \tat java.lang.reflect.Method.invoke(Native Method)",
            "E/AndroidRuntime(63320): \tat com.omnidroid.spare.App$1.run(App.java:42)",
            "E/AndroidRuntime(63320): \tat com.omnidroid.spare.App.guarded(App.java:56)",
            "E/AndroidRuntime(63320): \tat com.omnidroid.spare.App.main(App.java:40)",
            "E/AndroidRuntime(63320): \tat com.android.internal.os.RuntimeInit.nativeFinishInit(Native Method)",
            "W/Resources(10784): Preloaded drawable resource #0x1080846 (android:drawable/progress_horizontal_material) that varies with configuration!!",
            "W/System.err(63320): android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash",
            "W/ActivityManager( 1012): Skipping crash dialog of ProcessRecord{6204352 391000:com.outfit7.talkingtomcamp/u11a116}: background",
            "I/Process(63320): Sending signal. PID: 391000 SIG: 9",
            "I/DropBoxManagerService( 1266): add tag=data_app_crash isTagEnabled=true flags=0x2",
            "W/ActivityTaskManager( 1014): Activity pause timeout for ActivityRecord{d9809f9 u11 com.outfit7.talkingtomcamp/.MainActivity t3 f}}",
            "[zygote] pid 391000 ended: Ok(ExitStatus(unix_wait_status(35072)))",
            "I/WindowManager( 1209): WIN DEATH: Window{849c9cc u11 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}",
            "I/ActivityManager( 1237): Process com.outfit7.talkingtomcamp (pid 391000) has died: cch CRE ",
        ]);
        let d = diagnose(&reported, "com.outfit7.talkingtomcamp", 11);
        fits(&d);
        assert!(!d.on_purpose, "{d:#?}");
        assert_eq!(d.summary, "Java exception: android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash");
        assert!(d.details[0].contains("thread \"main\" (pid 391000)"), "{:#?}", d.details);
        assert!(d.details.iter().any(|l| l == "  at com.omnidroid.spare.App.main(App.java:40)"), "{:#?}", d.details);
        // Another user's instance of the package is not this one's.
        assert_eq!(diagnose(&reported, "com.outfit7.talkingtomcamp", 10).summary, "the app stopped");
    }

    #[test]
    fn a_java_exception_s_root_cause_is_the_summary_and_the_chain_is_kept() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "E/AndroidRuntime(47856): FATAL EXCEPTION: main",
                "E/AndroidRuntime(47856): Process: com.a, PID: 310000",
                "E/AndroidRuntime(47856): java.lang.RuntimeException: Unable to start activity ComponentInfo{com.a/com.a.Main}: java.lang.NullPointerException",
                "E/AndroidRuntime(47856): \tat android.app.ActivityThread.performLaunchActivity(ActivityThread.java:4164)",
                "E/AndroidRuntime(47856): Caused by: java.lang.NullPointerException: Attempt to invoke virtual method 'int java.lang.String.length()' on a null object reference",
                "E/AndroidRuntime(47856): \tat com.a.Main.onCreate(Main.java:12)",
                "E/AndroidRuntime(47856): \t... 11 more",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert!(d.summary.starts_with("Java exception: java.lang.NullPointerException: Attempt to invoke"), "{}", d.summary);
        assert!(d.details.iter().any(|l| l.starts_with("java.lang.RuntimeException: Unable to start activity")));
        assert!(d.details.iter().any(|l| l.starts_with("Caused by: java.lang.NullPointerException")));
        assert!(d.details.contains(&"  ... 11 more".to_string()));
    }

    /// A chain three deep, as Android 15 logs a layout that names a missing class: the card's
    /// rows hold it with the pool's lines after it, the root cause's frames kept over the outer
    /// exceptions'.
    #[test]
    fn a_long_chain_fits_the_card_with_its_root_cause() {
        let mut log = vec![
            LAUNCH.to_string(),
            "E/AndroidRuntime(47856): FATAL EXCEPTION: main".to_string(),
            "E/AndroidRuntime(47856): Process: com.a, PID: 310000".to_string(),
            "E/AndroidRuntime(47856): java.lang.RuntimeException: Unable to start activity ComponentInfo{com.a/com.a.Main}: android.view.InflateException: Binary XML file line #12 in com.a:layout/main: Error inflating class com.a.View".to_string(),
        ];
        let frames = |log: &mut Vec<String>, at: &str, n: usize| log.extend((0..n).map(|k| format!("E/AndroidRuntime(47856): \tat {at}{k}(Source.java:{k})")));
        frames(&mut log, "android.app.ActivityThread.performLaunchActivity", 14);
        log.push("E/AndroidRuntime(47856): Caused by: android.view.InflateException: Binary XML file line #12 in com.a:layout/main: Error inflating class com.a.View".to_string());
        frames(&mut log, "android.view.LayoutInflater.inflate", 8);
        log.push("E/AndroidRuntime(47856): \t... 14 more".to_string());
        log.push("E/AndroidRuntime(47856): Caused by: java.lang.ClassNotFoundException: Didn't find class \"com.a.View\" on path: DexPathList[[zip file \"/data/app/~~x==/com.a-y==/base.apk\"],nativeLibraryDirectories=[/data/app/~~x==/com.a-y==/lib/arm64, /system/lib64, /system_ext/lib64]]".to_string());
        frames(&mut log, "dalvik.system.BaseDexClassLoader.findClass", 9);
        log.push("E/AndroidRuntime(47856): \t... 22 more".to_string());
        let d = diagnose(&log, "com.a", 10);
        fits(&d);
        assert!(card_rows(&d.details) <= CARD_ROWS, "{} rows: {d:#?}", card_rows(&d.details));
        let at = |head: &str| d.details.iter().position(|l| l.starts_with(head)).unwrap_or_else(|| panic!("{head}: {d:#?}"));
        let root = at("Caused by: java.lang.ClassNotFoundException");
        let shown = |from: usize| d.details[from + 1..].iter().take_while(|l| l.starts_with("  ")).filter(|l| l.starts_with("  at ")).count();
        assert!(shown(root) > shown(at("java.lang.RuntimeException")) && shown(root) > shown(at("Caused by: android.view.InflateException")), "{d:#?}");
    }

    #[test]
    fn a_native_crash_names_its_signal_address_and_library_and_the_vdso_re_raise() {
        let d = diagnose(&lines(ROBLOX), "com.roblox.client", 10);
        fits(&d);
        assert_eq!(d.title, "com.roblox.client crashed");
        assert_eq!(d.summary, "native crash: SIGSEGV (bad memory access) at 0x8 in libzstd-jni-1.5.7-6.so");
        let text = d.details.join("\n");
        assert!(text.contains("SEGV_MAPERR: nothing is mapped at that address (fault address 0x8)"), "{text}");
        assert!(text.contains("No tombstone") && text.contains("crash_dump failed"), "{text}");
        assert!(text.contains("  pc 0x7c00b9c48c ([vdso]+0x48c)"), "{text}");
        assert!(text.contains("  lr 0x7eccdfe6d8 (/data/app/.../lib/arm64/libzstd-jni-1.5.7-6.so+0x5f86d8)"), "{text}");
        assert!(text.contains("re-raise, not the fault") && text.contains("evidence of where: libzstd-jni-1.5.7-6.so+0x5f86d8."), "{text}");
        assert!(!text.contains("x0 ") && !text.contains("sp 0x"), "the registers stay in the log: {text}");
    }

    #[test]
    fn the_engine_s_account_is_tied_to_the_crash_without_the_line_that_names_it() {
        // No `[process <pid>] ended` before it: the one account of SIGSEGV before the pid's end.
        let roblox: Vec<&str> = ROBLOX.iter().copied().filter(|l| !l.starts_with("[process ")).collect();
        let d = diagnose(&lines(&roblox), "com.roblox.client", 10);
        assert!(d.summary.ends_with("in libzstd-jni-1.5.7-6.so"), "{}", d.summary);
        // Two accounts of SIGSEGV, neither in the package's files: neither is taken.
        let mut two = roblox.clone();
        let other = "omni-linux: Killed { signal: 11, pc: 1, detail: \"the default action of signal 11\\n  pc 0x1 (/system/lib64/libgui.so+0x1)\\n\" }";
        two.insert(4, other);
        let at = two.iter().position(|l| l.starts_with("omni-linux: Killed { signal: 11, pc: 5")).unwrap();
        two[at] = "omni-linux: Killed { signal: 11, pc: 1, detail: \"the default action of signal 11\\n  pc 0x2 (/system/lib64/libui.so+0x2)\\n\" }";
        let d = diagnose(&lines(&two), "com.roblox.client", 10);
        assert_eq!(d.summary, "native crash: SIGSEGV (bad memory access) at 0x8");
        assert!(d.details.iter().all(|l| !l.contains("libgui") && !l.contains("libui")), "{d:#?}");
    }

    #[test]
    fn an_abort_says_its_message() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "F/libc(13938): fdsan: attempted to close file descriptor 7, expected to be unowned, actually owned by RandomAccessFile 0x5b47388",
                "F/libc(47865): FORTIFY: pthread_mutex_lock called on a destroyed mutex (0x3531158d054)",
                "F/libc(47865): Fatal signal 6 (SIGABRT), code -6 (SI_TKILL) in tid 310009 (binder:310000_2), pid 310000 (com.a)",
                "F/libc(47865): crash_dump helper failed to exec, or was killed",
                "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(34304)))",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!(d.summary, "native crash: SIGABRT (abort): FORTIFY: pthread_mutex_lock called on a destroyed mutex (0x3531158d054)");
        assert!(d.details.iter().any(|l| l == "SI_TKILL: sent by a process, as abort() sends it to itself."), "{d:#?}");
        assert!(d.details.iter().all(|l| !l.contains("fdsan")), "another process's thread said that: {d:#?}");
        let twice = diagnose(
            &lines(&[
                LAUNCH,
                "F/IPCThreadState(47865): getAndExecuteCommand(fd=47) returned unexpected error -5, aborting",
                "F/com.a(47865): runtime.cc:708] getAndExecuteCommand(fd=47) returned unexpected error -5, aborting",
                "F/com.a(47865): runtime.cc:708] #00 pc 00515600  /apex/com.android.art/lib64/libart.so (art::DumpNativeStack+108)",
                "F/libc(47865): Fatal signal 6 (SIGABRT), code -6 (SI_TKILL) in tid 310009 (binder:310000_2), pid 310000 (com.a)",
            ]),
            "com.a",
            10,
        );
        assert_eq!(twice.details.iter().filter(|l| l.contains("getAndExecuteCommand")).count(), 1, "{twice:#?}");
        assert!(twice.details.iter().all(|l| !l.contains("#00")), "{twice:#?}");
    }

    /// Roblox's real end (2026-10-19, both runs alike): ART's fatal error for the
    /// `NoSuchMethodError` `RegisterNatives` hit, then -- its abort unwinding the threads for 20 s
    /// -- the SIGSEGV in libzstd-jni, 2465 lines later. The signal came of the error.
    #[test]
    fn a_fatal_error_before_another_signal_is_its_cause() {
        let mut log = vec![
            "[zygote] launching com.roblox.client as pid 249000 uid 1010116: android.app.ActivityThread seq=24",
            "E/com.roblox.client(52403): Failed to register native method com.roblox.engine.jni.NativeQuoteInterface.requestResponseContinue([B)[B in /data/app/~~x==/com.roblox.client-y==/base.apk!classes2.dex",
            "F/com.roblox.client(52403): thread.cc:2560] No pending exception expected: java.lang.NoSuchMethodError: no static or non-static method \"Lcom/roblox/engine/jni/NativeQuoteInterface;.requestResponseContinue([B)[B\"",
            "F/com.roblox.client(52403): thread.cc:2560]   at java.lang.String java.lang.Runtime.nativeLoad(java.lang.String, java.lang.ClassLoader, java.lang.Class) (Runtime.java:-2)",
            "F/com.roblox.client(52403): thread.cc:2560]   at void com.github.luben.zstd.util.Native.load() (SourceFile:2)",
            "F/com.roblox.client(52403): thread.cc:2560] ",
        ];
        log.extend(std::iter::repeat_n("I/ActivityManager( 1050): Start proc 312000:com.b/u0a90 for broadcast {com.b/com.b.R}", 2465));
        log.push("E/libunwindstack(52403): Timeout waiting for ucontext");
        log.extend_from_slice(ROBLOX);
        let d = diagnose(&lines(&log), "com.roblox.client", 10);
        fits(&d);
        assert!(d.summary.starts_with("native crash: SIGSEGV after a fatal error: No pending exception expected: java.lang.NoSuchMethodError"), "{}", d.summary);
        let text = d.details.join("\n");
        assert!(text.contains("A fatal error came first, the likely cause (the signal came as the process went down):\n  thread.cc:2560] No pending exception expected: java.lang.NoSuchMethodError: no static or non-static method \"Lcom/roblox/engine/jni/NativeQuoteInterface;.requestResponseContinue([B)[B\"\n"), "{text}");
        assert!(!text.contains("nativeLoad") && !text.contains("Native.load"), "its frames stay in the log: {text}");
        assert!(text.contains("evidence of where: libzstd-jni-1.5.7-6.so+0x5f86d8."), "the signal's own account still: {text}");
    }

    /// ART's abort of a JNI error: the reason, `Runtime aborting...` and a dump of 40 threads at
    /// the fatal level from the aborting thread, longer than [`ABORT_SPAN`], then the signal.
    #[test]
    fn an_abort_s_message_is_its_reason_not_the_thread_dump_after_it() {
        let mut log = vec![
            LAUNCH.to_string(),
            "F/com.a(47856): java_vm_ext.cc:591] JNI DETECTED ERROR IN APPLICATION: use of deleted local reference 0x1d".to_string(),
            "F/com.a(47856): java_vm_ext.cc:591]     in call to NewStringUTF".to_string(),
            "F/com.a(47856): java_vm_ext.cc:591] \"main\" prio=5 tid=1 Runnable".to_string(),
            "F/com.a(47856): java_vm_ext.cc:591]   | group=\"main\" sCount=0 ucsCount=0 flags=0 obj=0x72f6d4b8 self=0xb400007".to_string(),
            "F/com.a(47856): java_vm_ext.cc:591]   native: #00 pc 000000000054d9a8  /apex/com.android.art/lib64/libart.so".to_string(),
            "F/com.a(47856): java_vm_ext.cc:591]   at com.a.Main.text(Native method)".to_string(),
            "F/com.a(47856): runtime.cc:708] Runtime aborting...".to_string(),
            "F/com.a(47856): runtime.cc:708] All threads:".to_string(),
            "F/com.a(47856): runtime.cc:708] DALVIK THREADS (40):".to_string(),
        ];
        for t in 0..40 {
            log.push(format!("F/com.a(47856): runtime.cc:708] \"Thread-{t}\" prio=5 tid={} Native", t + 2));
            log.push("F/com.a(47856): runtime.cc:708]   | group=\"main\" sCount=1 ucsCount=0 flags=1 obj=0x13c00000 self=0xb400007".to_string());
            log.push(format!("F/com.a(47856): runtime.cc:708]   | sysTid={} nice=0 cgrp=default sched=0/0 handle=0x7", 310000 + t));
            for k in 0..6 {
                log.push(format!("F/com.a(47856): runtime.cc:708]   native: #0{k} pc 00000000000{k}1000  /apex/com.android.runtime/lib64/bionic/libc.so"));
            }
            log.push("F/com.a(47856): runtime.cc:708]   at java.lang.Object.wait(Native method)".to_string());
        }
        log.push("F/libc(47856): Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 310000 (com.a), pid 310000 (com.a)".to_string());
        let d = diagnose(&log, "com.a", 10);
        fits(&d);
        assert_eq!(d.summary, "native crash: SIGABRT (abort): JNI DETECTED ERROR IN APPLICATION: use of deleted local reference 0x1d");
        assert!(d.details.contains(&"Abort message: java_vm_ext.cc:591] JNI DETECTED ERROR IN APPLICATION: use of deleted local reference 0x1d".to_string()), "{d:#?}");
        assert!(d.details.contains(&"  java_vm_ext.cc:591]     in call to NewStringUTF".to_string()), "{d:#?}");
        assert!(d.details.iter().all(|l| !l.contains("prio=5") && !l.contains("| ") && !l.contains("Runtime aborting")), "{d:#?}");
    }

    #[test]
    fn a_tombstone_gives_the_backtrace() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "F/libc(47865): Fatal signal 11 (SIGSEGV), code 2 (SEGV_ACCERR), fault addr 0x7c00001000 in tid 310009 (GameThread), pid 310000 (com.a)",
                "F/DEBUG( 4001): pid: 310000, tid: 310009, name: GameThread  >>> com.a <<<",
                "F/DEBUG( 4001): uid: 1010116",
                "F/DEBUG( 4001): backtrace:",
                "F/DEBUG( 4001):       #00 pc 0000000000001000  /data/app/~~x==/com.a-y==/lib/arm64/libgame.so (Game::tick()+16) (BuildId: 0123)",
                "F/DEBUG( 4001):       #01 pc 0000000000002000  /apex/com.android.runtime/lib64/bionic/libc.so (__pthread_start+196) (BuildId: 4567)",
                "F/DEBUG( 4001): stack:",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!(d.summary, "native crash: SIGSEGV (bad memory access) at 0x7c00001000 in libgame.so");
        assert!(d.details.contains(&"  #00 pc 0000000000001000  /data/app/.../lib/arm64/libgame.so (Game::tick()+16)".to_string()), "{d:#?}");
        assert!(d.details.iter().any(|l| l.starts_with("SEGV_ACCERR: the memory there may not be accessed")));
    }

    #[test]
    fn an_anr_says_why() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "E/ActivityManager( 1050): ANR in com.a (com.a/.Main)",
                "I/ActivityManager( 1049): Start proc 312000:com.b/u0a90 for broadcast {com.b/com.b.R}",
                "E/ActivityManager( 1050): PID: 310000",
                "E/ActivityManager( 1050): Reason: Input dispatching timed out (5b0e2a1 com.a/com.a.Main (server) is not responding. Waited 5001ms for KeyEvent)",
                "E/ActivityManager( 1050): Parent: com.a/.Main",
                "E/ActivityManager( 1050): ErrorId: 6f1c9e1d",
                "E/ActivityManager( 1050): Frozen: false",
                "I/ActivityManager( 1050): Killing 310000:com.a/u10a116 (adj 0): bg anr",
                "[zygote] pid 310000: signal 9, its host process ended",
                "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(9)))",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!(d.title, "com.a crashed");
        assert!(d.summary.starts_with("not responding (ANR): Input dispatching timed out (5b0e2a1"), "{}", d.summary);
        assert!(d.details.contains(&"  ANR in com.a (com.a/.Main)".to_string()) && d.details.contains(&"  Parent: com.a/.Main".to_string()), "{d:#?}");
        assert!(d.details.contains(&"Then Android killed it: bg anr".to_string()), "{d:#?}");
        assert!(d.details.iter().all(|l| !l.contains("ErrorId")));
    }

    /// An ANR the app lived through (the person waited, or it answered again) is not what ended it
    /// later: that is, with the ANR said beside it -- and the ANR alone when nothing else says why.
    #[test]
    fn an_anr_the_app_lived_through_is_not_its_end() {
        let anr = [
            LAUNCH,
            "E/ActivityManager( 1050): ANR in com.a (com.a/.Main)",
            "E/ActivityManager( 1050): PID: 310000",
            "E/ActivityManager( 1050): Reason: Input dispatching timed out (5b0e2a1 com.a/com.a.Main (server) is not responding. Waited 5001ms for KeyEvent)",
        ];
        let then = |end: &[&str]| diagnose(&lines(&[&anr[..], end].concat()), "com.a", 10);
        let lmk = then(&["I/lowmemorykiller(15000): Kill 'com.a' (310000), uid 1010116, oom_score_adj 900 to free 204800kB rss, 0kB swap; reason: critical pressure"]);
        fits(&lmk);
        assert_eq!((lmk.title.as_str(), lmk.summary.as_str()), ("com.a was closed by Android", "killed by Android: low memory (critical pressure)"));
        assert!(lmk.details.last().unwrap().starts_with("Before that, Android found it not responding (an ANR), and did not kill it for that: Input dispatching timed out"), "{lmk:#?}");
        let exit = then(&["[exit] pid 310000 tid 310000 exit_group(3) pc 0x7c00001000 lr 0x7c00002000", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(768)))"]);
        assert_eq!(exit.summary, "exited by itself (code 3)");
        let other = then(&["I/ActivityManager( 1050): Killing 310000:com.a/u10a116 (adj 905): empty #17"]);
        assert_eq!(other.summary, "killed by Android: empty #17");
        let alone = then(&[]);
        assert!(alone.summary.starts_with("not responding (ANR): Input dispatching timed out"), "{}", alone.summary);
        // Its dialog closed by the person: killed for it.
        let closed = then(&["I/ActivityManager( 1050): Killing 310000:com.a/u10a116 (adj 0): user request after error", "[zygote] pid 310000: signal 9, its host process ended"]);
        assert!(closed.summary.starts_with("not responding (ANR)"), "{}", closed.summary);
        assert!(closed.details.contains(&"Then Android killed it: user request after error".to_string()), "{closed:#?}");
    }

    #[test]
    fn the_engine_failing_the_process_is_said_to_be_the_engine_s() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "[process 310000] ended; tasks [310008] did not stop and are abandoned",
                "omni-linux: Killed { signal: 4, pc: 534790322176, detail: \"the guest executed an unsupported instruction 0xd53be060 at 0x7c84a01000\\n  pc 0x7c84a01000 (/data/app/~~x==/com.a-y==/lib/arm64/libgame.so+0x1000)\\n  lr 0x7c84a02000\\n  sp 0x7ee016f780\\n\" }",
                "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(33792)))",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!(d.title, "com.a crashed");
        assert_eq!(d.summary, "emulation failure: the guest executed an unsupported instruction 0xd53be060 at 0x7c84a01000");
        assert!(d.details.contains(&"  pc 0x7c84a01000 (/data/app/.../lib/arm64/libgame.so+0x1000)".to_string()), "{d:#?}");
        // Its host process dying of a signal of its own, or a Windows exception, or a panic.
        let host = |status: &str| diagnose(&lines(&[LAUNCH, "thread 'cpu' panicked at crates/omni-linux/src/x.rs:1:2:", "oops", &format!("[zygote] pid 310000 ended: Ok({status})")]), "com.a", 10);
        assert_eq!(host("ExitStatus(unix_wait_status(11))").summary, "emulation failure: its host process died of signal 11 (SIGSEGV)");
        assert_eq!(host("ExitStatus(ExitStatus(3221225477))").summary, "emulation failure: its host process died of Windows exception 0xC0000005 (access violation)");
        let panic = host("ExitStatus(unix_wait_status(25856))");
        fits(&panic);
        assert_eq!(panic.summary, "emulation failure: its host process panicked (exit code 101)");
        assert!(panic.details.contains(&"  oops".to_string()), "{panic:#?}");
        // A Mac's host signal numbers differ from Linux's past the classic ones: named for both.
        assert_eq!(host("ExitStatus(unix_wait_status(10))").summary, "emulation failure: its host process died of signal 10 (SIGBUS on macOS, SIGUSR1 on Linux)");
        assert_eq!(host("ExitStatus(unix_wait_status(7))").summary, "emulation failure: its host process died of signal 7 (SIGBUS on Linux, SIGEMT on macOS)");
    }

    /// A host process's exit code the app asked for no exit with (no `[exit]` line): not the app
    /// exiting -- the host giving up as it started the program (127), or Windows' Task Manager (1).
    #[test]
    fn a_host_exit_the_app_did_not_ask_for_is_not_its_own() {
        let ended = |end: &[&str]| diagnose(&lines(&[&[LAUNCH][..], end].concat()), "com.a", 10);
        let d = ended(&["omni-linux-run: the guest's address space: Cannot allocate memory", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(32512)))"]);
        fits(&d);
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.a stopped", "its host process ended (code 127)"));
        assert!(d.details.contains(&"  omni-linux-run: the guest's address space: Cannot allocate memory".to_string()), "{d:#?}");
        let windows = ended(&["[zygote] pid 310000 ended: Ok(ExitStatus(ExitStatus(1)))", "I/ActivityManager( 1210): Process com.a (pid 310000) has died: fg  TOP "]);
        assert_eq!(windows.summary, "its host process ended (code 1)");
        assert!(windows.details[0].contains("an exit the app did not ask for"), "{windows:#?}");
    }

    #[test]
    fn a_kill_by_android_says_its_reason() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "[zygote] launching com.a as pid 320000 uid 1110116: android.app.ActivityThread seq=51",
                "I/ActivityManager( 1050): Killing 310000:com.a/u10a116 (adj 905): empty #1",
                "I/ActivityManager( 1050): Killing 320000:com.a/u11a116 (adj 0): excessive cpu",
                "[zygote] pid 310000: signal 9, its host process ended",
                "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(9)))",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!(d.title, "com.a was closed by Android");
        assert_eq!(d.summary, "killed by Android: empty #1");
        assert!(!d.on_purpose, "a kill by Android is no quit");
        assert!(d.details[0].contains("pid 310000") && d.details[0].contains("empty processes"), "{d:#?}");
        let lmk = diagnose(
            &lines(&[LAUNCH, "I/lowmemorykiller(15000): Kill 'com.a' (310000), uid 1010116, oom_score_adj 900 to free 204800kB rss, 0kB swap; reason: low watermark is breached"]),
            "com.a",
            10,
        );
        fits(&lmk);
        assert_eq!(lmk.summary, "killed by Android: low memory (low watermark is breached)");
    }

    #[test]
    fn an_exit_says_its_code_or_its_signal() {
        let exit = |status: &str| {
            diagnose(
                &lines(&[
                    LAUNCH,
                    "[exit] pid 310000 tid 310000 exit_group(3) pc 0x7c00001000 (/data/app/~~x==/com.a-y==/lib/arm64/libgame.so+0x1000) lr 0x7c00002000",
                    &format!("[zygote] pid 310000 ended: Ok({status})"),
                ]),
                "com.a",
                10,
            )
        };
        let d = exit("ExitStatus(unix_wait_status(768))");
        fits(&d);
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.a stopped", "exited by itself (code 3)"));
        assert!(d.details.iter().any(|l| l.contains("exit_group(3) pc 0x7c00001000 (/data/app/.../lib/arm64/libgame.so+0x1000)")), "{d:#?}");
        assert_eq!(exit("ExitStatus(ExitStatus(3))").summary, "exited by itself (code 3)", "Windows' form");
        let sent = diagnose(&lines(&[LAUNCH, "[zygote] pid 310000: signal 9, its host process ended", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(9)))"]), "com.a", 10);
        assert_eq!(sent.summary, "killed by signal 9 (SIGKILL)");
        assert!(sent.details[0].starts_with("Something in Android sent"), "{sent:#?}");
        let outside = diagnose(&lines(&[LAUNCH, "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(9)))"]), "com.a", 10);
        assert!(outside.details[0].contains("from outside Android"), "{outside:#?}");
        assert!(!d.on_purpose && !sent.on_purpose && !outside.on_purpose, "an exit not 0, a signal it did not send itself");
    }

    #[test]
    fn nothing_found_says_so_with_the_last_lines_naming_the_app() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "I/ActivityTaskManager( 1049): START u10 {cmp=com.a/.Main} with LAUNCH_SINGLE_TASK from uid 0 (BAL_ALLOW_PERMISSION) result code=0",
                "I/ActivityTaskManager( 1049): START u11 {cmp=com.a/.Main} with LAUNCH_SINGLE_TASK from uid 0 (BAL_ALLOW_PERMISSION) result code=0",
                "I/WindowManager( 1050): WIN DEATH: Window{dafc344 u11 com.a/com.a.Main}",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.a stopped", "the app stopped"));
        assert_eq!(d.details[0], "The app left its screen; the device log says nothing about why (it may still run in the background, or have finished its activity).");
        assert!(d.details.iter().any(|l| l.contains("START u10")), "{d:#?}");
        assert!(d.details.iter().all(|l| !l.contains("u11")), "another instance's lines: {d:#?}");
        assert_eq!(diagnose(&[], "com.a", 10).details.len(), 1);
    }

    #[test]
    fn another_package_s_crash_is_not_the_app_s() {
        let d = diagnose(
            &lines(&[
                "[zygote] launching com.other as pid 300000 uid 1010117: android.app.ActivityThread seq=49",
                LAUNCH,
                "E/AndroidRuntime(37856): FATAL EXCEPTION: main",
                "E/AndroidRuntime(37856): Process: com.other, PID: 300000",
                "E/AndroidRuntime(37856): java.lang.IllegalStateException: not ours",
                "F/libc(37870): Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0 in tid 300014 (), pid 300000 (com.other)",
                "E/ActivityManager( 1050): ANR in com.android.settings (com.android.settings/.FallbackHome)",
                "E/ActivityManager( 1050): PID: 145000",
                "[exit] pid 310000 tid 310000 exit_group(1) pc 0x7c00001000 lr 0x7c00002000",
                "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(256)))",
            ]),
            "com.a",
            10,
        );
        fits(&d);
        assert_eq!(d.summary, "exited by itself (code 1)");
        assert!(d.details.iter().all(|l| !l.contains("com.other") && !l.contains("not ours")), "{d:#?}");
    }

    #[test]
    fn another_user_s_crash_of_the_same_package_is_not_this_instance_s() {
        let other = "[zygote] launching com.outfit7.talkingtomcamp as pid 269000 uid 1110116: android.app.ActivityThread seq=40";
        let mut java = vec![other];
        java.extend_from_slice(JAVA);
        assert_eq!(diagnose(&lines(&java), "com.outfit7.talkingtomcamp", 10).summary, "the app stopped");
        assert!(diagnose(&lines(&java), "com.outfit7.talkingtomcamp", 11).summary.starts_with("Java exception: "));
        // A crash no line puts in a user: launched before this instance's log began, so another
        // instance's when this one's own pid is known, and whose it is cannot be told when another
        // user's is.
        let mut unknown = vec!["[zygote] launching com.outfit7.talkingtomcamp as pid 280000 uid 1110116: android.app.ActivityThread seq=41"];
        unknown.extend_from_slice(JAVA);
        assert_eq!(diagnose(&lines(&unknown), "com.outfit7.talkingtomcamp", 10).summary, "the app stopped");
        assert_eq!(diagnose(&lines(&unknown), "com.outfit7.talkingtomcamp", 11).summary, "the app stopped");
        // Roblox's crash as user 11's: its record says so.
        let roblox: Vec<String> = ROBLOX.iter().map(|l| l.replace("/u10a116", "/u11a116")).collect();
        assert_eq!(diagnose(&roblox, "com.roblox.client", 10).summary, "the app stopped");
        assert!(diagnose(&roblox, "com.roblox.client", 11).summary.starts_with("native crash: SIGSEGV"));
    }

    #[test]
    fn the_user_s_earlier_process_stopped_before_the_app_s_start_is_not_its_end() {
        let d = diagnose(
            &lines(&[
                "I/ActivityManager( 1050): Force stopping com.a appid=10116 user=10: from pid 998000",
                "I/ActivityManager( 1050): Killing 300000:com.a/u10a116 (adj 905): stop com.a due to from pid 998000",
                LAUNCH,
                "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(0)))",
            ]),
            "com.a",
            10,
        );
        // Its own exit with status 0 (a quit), not the force-stop of the process before it.
        assert_eq!(d.summary, "closed: the app quit (exit status 0)");
    }

    #[test]
    fn the_main_process_s_end_comes_before_another_process_s_crash() {
        let d = diagnose(
            &lines(&[
                LAUNCH,
                "[zygote] launching com.a:push as pid 311000 uid 1010116: android.app.ActivityThread seq=51",
                "F/libc(48870): Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0 in tid 311014 (), pid 311000 (com.a:push)",
                "I/ActivityManager( 1050): Killing 310000:com.a/u10a116 (adj 905): empty #1",
            ]),
            "com.a",
            10,
        );
        assert_eq!(d.summary, "killed by Android: empty #1");
        let side = diagnose(&lines(&[LAUNCH, "[zygote] launching com.a:push as pid 311000 uid 1010116: x", "F/libc(48870): Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0 in tid 311014 (), pid 311000 (com.a:push)"]), "com.a", 10);
        assert_eq!(side.summary, "native crash: SIGSEGV (bad memory access) at 0x0", "nothing ended the main one");
    }

    #[test]
    fn the_log_s_forms_are_read() {
        assert_eq!(exit_status("Ok(ExitStatus(unix_wait_status(35072)))"), Some(Exit::Code(137)));
        assert_eq!(exit_status("Ok(ExitStatus(unix_wait_status(9)))"), Some(Exit::Signal(9)));
        assert_eq!(exit_status("Ok(ExitStatus(ExitStatus(3221225477)))"), Some(Exit::Code(0xC000_0005)));
        assert_eq!(exit_status("Err(Os { code: 10 })"), None);
        assert!(of_pid(6856, 269000) && of_pid(52425, 249000) && !of_pid(13938, 310000));
        assert_eq!(unescape(r#"a\nb \"c\" \\ \u{e9}"#), "a\nb \"c\" \\ \u{e9}");
        assert_eq!(short_path("x /data/app/~~a==/com.a-b==/lib/arm64/libz.so+0x1 y /data/app/c"), "x /data/app/.../lib/arm64/libz.so+0x1 y /data/app/c");
        assert_eq!(record("249000:com.roblox.client/u10a116}"), Some((249000, "com.roblox.client", 10)));
        assert_eq!(role("m.roblox.client", "com.roblox.client"), Some(true));
        assert_eq!(role("com.roblox.client:remote", "com.roblox.client"), Some(false));
        assert_eq!(role("com.roblox.clientx", "com.roblox.client"), None);
        let e = entry("123.456 E/AndroidRuntime( 6856): \tat x").unwrap();
        assert_eq!((e.tag, e.tid, e.msg), ("AndroidRuntime", 6856, "\tat x"));
        assert!(entry("[zygote] pid 1 ended").is_none());
        assert_eq!(clip("abcdef", 5), "ab...");
        assert!(other_user("Killing 1:com.a/u11a116 (adj 0): x", 10) && !other_user("Killing 1:com.a/u10a116 (adj 0): x", 10));
        assert!(!other_user("I/ActivityManager( 1050): uid 1010116 unique", 10));
    }

    const TTC: &str = "com.outfit7.talkingtomcamp";

    /// `am force-stop --user 10` through the MCP server's shell (2026-10-19): Android's force-stop
    /// and its kill, then the window's removal with the death.
    const FORCE_STOP: &[&str] = &[
        "[zygote] launching com.outfit7.talkingtomcamp as pid 280000 uid 1010116: android.app.ActivityThread seq=29",
        "I/ActivityManager( 1022): Start proc 280000:com.outfit7.talkingtomcamp/u10a116 for top-activity {com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}",
        "D/CoreBackPreview( 1240): Window{e2a82a3 u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback OnBackInvokedCallbackInfo{mCallback=android.window.IOnBackInvokedCallback$Stub$Proxy@c99391e, mPriority=0, mIsAnimationCallback=false}",
        "I/ActivityManager( 1008): Force stopping com.outfit7.talkingtomcamp appid=10116 user=10: from pid 330000",
        "E/ActivityManager( 1008): Unable to freeze binder for 280000: java.lang.RuntimeException: Unable to freeze/unfreeze binder",
        "I/ActivityManager( 1008): Killing 280000:com.outfit7.talkingtomcamp/u10a116 (adj 0): stop com.outfit7.talkingtomcamp due to from pid 330000",
        "[zygote] pid 280000: signal 9, its host process ended",
        "W/ActivityTaskManager( 1008): Force removing ActivityRecord{82939ec u10 com.outfit7.talkingtomcamp/.MainActivity t3 f}}: app died, no saved state",
        "W/InputManager-JNI( 1008): Input channel object 'e2a82a3 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity (client)' was disposed without first being removed with the input manager!",
        "[zygote] pid 280000 ended: Ok(ExitStatus(unix_wait_status(9)))",
        "V/ActivityManager( 1238): Got obituary of 280000:com.outfit7.talkingtomcamp",
    ];

    #[test]
    fn a_force_stop_is_closed_on_purpose() {
        let d = diagnose(&lines(FORCE_STOP), TTC, 10);
        fits(&d);
        assert!(d.on_purpose, "{d:#?}");
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.outfit7.talkingtomcamp was closed", "closed: force-stopped"));
        assert!(d.details.contains(&"  Force stopping com.outfit7.talkingtomcamp appid=10116 user=10: from pid 330000".to_string()), "{d:#?}");
        assert!(d.details.iter().any(|l| l.contains("stop com.outfit7.talkingtomcamp due to from pid 330000")), "{d:#?}");
        // The MCP server's stop_instance stops it in the same words (from pid 851000): were its log
        // read, the same end on purpose.
        let pools: Vec<String> = FORCE_STOP.iter().map(|l| l.replace("330000", "851000")).collect();
        assert_eq!(diagnose(&pools, TTC, 10).summary, "closed: force-stopped");
        // No kill logged: the force-stop of the user's (or every user's) package before the signal.
        for user in ["10", "-1"] {
            let quiet: Vec<String> = FORCE_STOP.iter().filter(|l| !l.contains("Killing ")).map(|l| l.replace("user=10:", &format!("user={user}:"))).collect();
            let q = diagnose(&quiet, TTC, 10);
            assert!(q.on_purpose && q.summary == "closed: force-stopped", "user={user}: {q:#?}");
        }
        // Android's own stop of the package, as another build is installed over it: a kill by
        // Android, its card and its restart kept -- its kill logged or not.
        let replaced: Vec<String> = FORCE_STOP.iter().map(|l| l.replace("from pid 330000", "replace pkg")).collect();
        let r = diagnose(&replaced, TTC, 10);
        fits(&r);
        assert!(!r.on_purpose && r.summary == "killed by Android: stop com.outfit7.talkingtomcamp due to replace pkg", "{r:#?}");
        assert!(r.details[0].contains("on its own"), "{r:#?}");
        let quiet: Vec<String> = replaced.into_iter().filter(|l| !l.contains("Killing ")).collect();
        assert!(!diagnose(&quiet, TTC, 10).on_purpose);
        // Its data cleared (`pm clear`): asked for.
        let cleared: Vec<String> = FORCE_STOP.iter().map(|l| l.replace("from pid 330000", "clear data")).collect();
        assert!(diagnose(&cleared, TTC, 10).on_purpose);
    }

    #[test]
    fn another_user_s_force_stop_is_not_this_instance_s() {
        let log = lines(&[
            LAUNCH,
            "[zygote] launching com.a as pid 320000 uid 1110116: android.app.ActivityThread seq=51",
            "I/ActivityManager( 1050): Force stopping com.a appid=10116 user=11: from pid 998000",
            "I/ActivityManager( 1050): Killing 320000:com.a/u11a116 (adj 0): stop com.a due to from pid 998000",
            "[zygote] pid 320000: signal 9, its host process ended",
            "[zygote] pid 320000 ended: Ok(ExitStatus(unix_wait_status(9)))",
            "[zygote] pid 310000: signal 9, its host process ended",
            "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(9)))",
        ]);
        let d = diagnose(&log, "com.a", 10);
        fits(&d);
        assert!(!d.on_purpose, "{d:#?}");
        assert_eq!(d.summary, "killed by signal 9 (SIGKILL)");
        assert!(d.details.iter().all(|l| !l.contains("Force stopping")), "{d:#?}");
        assert!(diagnose(&log, "com.a", 11).on_purpose, "user 11's own");
        // Nor another package's, though its name begins with this one's.
        let other: Vec<String> = log.iter().filter(|l| !l.contains("320000")).map(|l| l.replace("user=11:", "user=10:").replace("Force stopping com.a ", "Force stopping com.a.b ")).collect();
        assert!(!diagnose(&other, "com.a", 10).on_purpose);
    }

    /// One Back on TalkingTomCamp (2026-10-19): its activity finished, its window went, its process
    /// stayed cached; no line of ActivityTaskManager's, no end.
    const BACK: &[&str] = &[
        "[zygote] launching com.outfit7.talkingtomcamp as pid 465000 uid 1010116: android.app.ActivityThread seq=50",
        "I/ActivityManager( 1022): Start proc 465000:com.outfit7.talkingtomcamp/u10a116 for top-activity {com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}",
        "D/CoreBackPreview( 1236): Window{353bf6d u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback OnBackInvokedCallbackInfo{mCallback=android.window.IOnBackInvokedCallback$Stub$Proxy@f572c33, mPriority=0, mIsAnimationCallback=false}",
        "I/ActivityTaskManager( 1011): Displayed com.outfit7.talkingtomcamp/.MainActivity for user 10: +4s344ms",
        "[input] 39 events answered by the app in 5s: event -> handled p50 9.8 ms, p90 18.1, max 21.0; keys (1) p50 13.3 ms, max 13.3",
        "W/system_server( 1014): Long monitor contention with owner binder:1000_5 (1054) at boolean com.android.server.wm.ActivityClientController.finishActivity(android.os.IBinder, int, android.content.Intent, int)(ActivityClientController.java:536) waiters=1 in void com.android.server.wm.WindowManagerServi",
        "W/lowmemorykiller(15000): Failed to open /proc/465000/oom_score_adj; errno=2: process 465000 might have been killed",
        "I/InputDispatcher( 1054): Touched window was removed: 353bf6d com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity in display %3",
        "D/CoreBackPreview( 1263): Window{353bf6d u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback null",
        "W/InputManager-JNI( 1263): Input channel object '353bf6d com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity (client)' was disposed without first being removed with the input manager!",
        "I/ActivityManager( 1238): Killing 452000:com.android.devicelockcontroller/u10a93 (adj 905): empty #1",
        "[code] .talkingtomcamp (pid 465000) quiet: 140 MiB of translations dropped",
    ];

    const FINISHED: &str = "closed: its activity finished (Back, or the app's own exit)";

    #[test]
    fn back_finishing_the_app_s_activity_is_closed_on_purpose() {
        let d = diagnose(&lines(BACK), TTC, 10);
        fits(&d);
        assert!(d.on_purpose, "{d:#?}");
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.outfit7.talkingtomcamp was closed", FINISHED));
        assert!(d.details[1].starts_with("  Input channel object '353bf6d com.outfit7.talkingtomcamp/"), "{d:#?}");
        assert_eq!(d.details[2], "No crash, no kill and no end of its process is in the log.");
        // In the game's camp view (2026-10-19): the same, and a second Back finding no window.
        let game = diagnose(
            &lines(&[
                "[zygote] launching com.outfit7.talkingtomcamp as pid 1072000 uid 1010116: android.app.ActivityThread seq=94",
                "D/CoreBackPreview( 1008): Window{4556d44 u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback OnBackInvokedCallbackInfo{mCallback=android.window.IOnBackInvokedCallback$Stub$Proxy@ba8cf62, mPriority=0, mIsAnimationCallback=false}",
                "V/SDL(23424): onDestroy()",
                "D/CoreBackPreview( 1241): Window{4556d44 u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback null",
                "W/InputManager-JNI( 1241): Input channel object '4556d44 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity (client)' was disposed without first being removed with the input manager!",
                "W/InputDispatcher( 1078): Waiting because no window has focus but ActivityRecord{27385e7 u10 com.outfit7.talkingtomcamp/.MainActivity t8} may eventually add a window when it finishes starting up. Will wait for 60000ms",
                "E/WindowManager( 1078): Unknown app appToken:ActivityRecord{27385e7 u10 com.outfit7.talkingtomcamp/.MainActivity t8}. Dropping notifyNoFocusedWindowAnr request",
            ]),
            TTC,
            10,
        );
        assert!(game.on_purpose && game.summary == FINISHED, "{game:#?}");
        // Android trimming the cached process later (`am kill`, 4 min on, 2026-10-19): still the
        // Back's end, not a kill's card and restart.
        let mut kill = BACK.to_vec();
        kill.extend_from_slice(&[
            "I/ActivityManager( 1238): Killing 465000:com.outfit7.talkingtomcamp/u10a116 (adj 900): kill background",
            "[zygote] pid 465000: signal 9, its host process ended",
            "V/ActivityManager( 1049): Got obituary of 465000:com.outfit7.talkingtomcamp",
            "[zygote] pid 465000 ended: Ok(ExitStatus(unix_wait_status(9)))",
        ]);
        let killed = diagnose(&lines(&kill), TTC, 10);
        fits(&killed);
        assert!(killed.on_purpose && killed.summary == FINISHED, "{killed:#?}");
        assert_eq!(killed.details.last().unwrap(), "Then, with no activity left, its process ended: killed by Android: kill background");
    }

    /// An uncaught-exception handler of the app's own, on a thread of its own: the exception logged,
    /// its activities finished (their window gone before the exit), then its own exit or kill -- a
    /// crash's end, not Back.
    #[test]
    fn a_finish_after_the_app_logged_a_crash_then_its_own_end_is_no_quit() {
        let handled = |end: &[&str]| {
            let mut log = vec![
                LAUNCH,
                "D/CoreBackPreview( 1236): Window{353bf6d u10 com.a/com.a.Main}: Setting back callback OnBackInvokedCallbackInfo{mCallback=x, mPriority=0, mIsAnimationCallback=false}",
                "E/CrashHandler(47857): java.lang.IllegalStateException: boom",
                "E/CrashHandler(47857): \tat com.a.Worker.run(Worker.java:31)",
                "W/InputManager-JNI( 1263): Input channel object '353bf6d com.a/com.a.Main (client)' was disposed without first being removed with the input manager!",
            ];
            log.extend_from_slice(end);
            diagnose(&lines(&log), "com.a", 10)
        };
        let boom = "  java.lang.IllegalStateException: boom".to_string();
        let exit = handled(&["I/com.a(47857): System.exit called, status: 0", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(0)))"]);
        fits(&exit);
        assert!(!exit.on_purpose && exit.summary == "exited by itself (code 0)", "{exit:#?}");
        assert_eq!(exit.details.iter().filter(|l| **l == boom).count(), 1, "said once: {exit:#?}");
        let code = handled(&["[exit] pid 310000 tid 310009 exit_group(10) pc 0x7c23109428 lr 0x71ac4514", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(2560)))"]);
        assert!(!code.on_purpose && code.summary == "exited by itself (code 10)" && code.details.contains(&boom), "{code:#?}");
        let killed = handled(&["I/Process(47857): Sending signal. PID: 310000 SIG: 9", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(35072)))"]);
        assert!(!killed.on_purpose && killed.summary == "killed by signal 9 (SIGKILL)" && killed.details.contains(&boom), "{killed:#?}");
        // Its process left cached, or trimmed by Android later: the finish is the end still (Back
        // after an exception the app caught and logged).
        let cached = handled(&[]);
        assert!(cached.on_purpose && cached.summary == FINISHED, "{cached:#?}");
        let trimmed = handled(&["I/ActivityManager( 1238): Killing 310000:com.a/u10a116 (adj 900): kill background", "[zygote] pid 310000: signal 9, its host process ended", "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(9)))"]);
        assert!(trimmed.on_purpose && trimmed.summary == FINISHED, "{trimmed:#?}");
    }

    #[test]
    fn a_window_that_came_back_or_another_user_s_is_no_finish() {
        // A new window after the removal (a relaunch, a rotation's): the activity is there still.
        let mut back = BACK.to_vec();
        back.push("D/CoreBackPreview( 1263): Window{7a1b2c3 u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback OnBackInvokedCallbackInfo{mCallback=x, mPriority=0, mIsAnimationCallback=false}");
        let alive = diagnose(&lines(&back), TTC, 10);
        assert!(!alive.on_purpose && alive.summary == "the app stopped", "{alive:#?}");
        back.push("I/lowmemorykiller(15000): Kill 'com.outfit7.talkingtomcamp' (465000), uid 1010116, oom_score_adj 0 to free 204800kB rss, 0kB swap; reason: critical pressure");
        let lmk = diagnose(&lines(&back), TTC, 10);
        assert!(!lmk.on_purpose && lmk.summary == "killed by Android: low memory (critical pressure)", "{lmk:#?}");
        // User 11's window going says nothing of user 10's.
        let other: Vec<String> = BACK.iter().map(|l| l.replace("Window{353bf6d u10", "Window{353bf6d u11")).collect();
        assert!(!diagnose(&other, TTC, 10).on_purpose);
    }

    #[test]
    fn a_kill_from_a_shell_is_not_on_purpose() {
        // `kill -9` from the MCP server's root shell (2026-10-19): the window went with the death.
        let d = diagnose(
            &lines(&[
                "[zygote] launching com.outfit7.talkingtomcamp as pid 902000 uid 1010116: android.app.ActivityThread seq=83",
                "D/CoreBackPreview( 1239): Window{f4c68cc u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}: Setting back callback OnBackInvokedCallbackInfo{mCallback=android.window.IOnBackInvokedCallback$Stub$Proxy@a55ca2a, mPriority=0, mIsAnimationCallback=false}",
                "[zygote] pid 902000: signal 9, its host process ended",
                "[binder] pid 902000 closed its driver: 14 objects released",
                "I/ActivityManager( 1240): Process com.outfit7.talkingtomcamp (pid 902000) has died: fg  TOP ",
                "I/WindowManager( 1239): WIN DEATH: Window{f4c68cc u10 com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity}",
                "W/InputManager-JNI( 1239): Input channel object 'f4c68cc com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity (client)' was disposed without first being removed with the input manager!",
                "[zygote] pid 902000 ended: Ok(ExitStatus(unix_wait_status(9)))",
                "W/ActivityTaskManager( 1240): Force removing ActivityRecord{90cc597 u10 com.outfit7.talkingtomcamp/.MainActivity t7}: app died, no saved state",
            ]),
            TTC,
            10,
        );
        fits(&d);
        assert!(!d.on_purpose, "{d:#?}");
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.outfit7.talkingtomcamp stopped", "killed by signal 9 (SIGKILL)"));
    }

    #[test]
    fn an_exit_with_status_0_is_the_app_quitting_unless_a_crash_came_first() {
        // As WebViewLoader's `System.exit(0)` says it (2026-10-19).
        let exit = [
            LAUNCH,
            "I/com.a(47856): System.exit called, status: 0",
            "I/AndroidRuntime(47856): VM exiting with result code 0, cleanup skipped.",
            "[process 310000] ended; tasks [310009, 310008] did not stop and are abandoned",
            "omni-linux: Exited(0)",
            "I/ActivityManager( 1208): Process com.a (pid 310000) has died: fg  TOP ",
            "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(0)))",
        ];
        let d = diagnose(&lines(&exit), "com.a", 10);
        fits(&d);
        assert!(d.on_purpose, "{d:#?}");
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.a was closed", "closed: the app quit (exit status 0)"));
        assert!(d.details.contains(&"  System.exit called, status: 0".to_string()), "{d:#?}");
        // An uncaught-exception handler of the app's own logging the exception, then exiting.
        let handled = [&exit[..1], &["E/CrashHandler(47856): uncaught on main", "E/CrashHandler(47856): java.lang.IllegalStateException: boom", "E/CrashHandler(47856): \tat com.a.Main.onCreate(Main.java:12)"], &exit[1..]].concat();
        let c = diagnose(&lines(&handled), "com.a", 10);
        fits(&c);
        assert!(!c.on_purpose && c.summary == "exited by itself (code 0)", "{c:#?}");
        assert!(c.details.contains(&"  java.lang.IllegalStateException: boom".to_string()), "{c:#?}");
        // A side process's exit says nothing of the app being quit.
        let side = diagnose(&lines(&["[zygote] launching com.a:remote as pid 311000 uid 1010116: x", "[zygote] pid 311000 ended: Ok(ExitStatus(unix_wait_status(0)))"]), "com.a", 10);
        assert!(!side.on_purpose && side.summary == "exited by itself (code 0)", "{side:#?}");
    }

    #[test]
    fn the_app_killing_its_own_process_is_quitting_unless_a_crash_came_first() {
        // Its exit button's `Process.killProcess(myPid())`: TalkingTomCamp's own SIGKILL's lines,
        // no crash before them.
        let quit = lines(&[
            LAUNCH,
            "I/Process(47856): Sending signal. PID: 310000 SIG: 9",
            "[process 310000] ended; tasks [310009, 310008] did not stop and are abandoned",
            "omni-linux: Killed { signal: 9, pc: 533164233768, detail: \"the default action of signal 9\\n  pc 0x7c23109428 (/apex/com.android.runtime/lib64/bionic/libc.so+0xbd428)\\n  lr 0x71ac4514 (/system/framework/arm64/boot-framework.oat+0x1f4514)\\n\" }",
            "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(35072)))",
            "I/ActivityManager( 1208): Process com.a (pid 310000) has died: fg  TOP ",
        ]);
        let d = diagnose(&quit, "com.a", 10);
        fits(&d);
        assert!(d.on_purpose, "{d:#?}");
        assert_eq!((d.title.as_str(), d.summary.as_str()), ("com.a was closed", "closed: the app quit itself"));
        assert!(d.details.contains(&"  Sending signal. PID: 310000 SIG: 9".to_string()), "{d:#?}");
        // Android's handler ends a crash so: `FATAL EXCEPTION` first is a crash ([`JAVA`]), and so is
        // its failure to report one alone (`the_failure_to_report_a_crash_is_not_its_cause`).
        assert!(!diagnose(&lines(JAVA), TTC, 10).on_purpose);
        // Another process's signal to it is not its quit.
        let sent: Vec<String> = quit.iter().map(|l| l.replace("I/Process(47856)", "I/Process( 1050)")).collect();
        let s = diagnose(&sent, "com.a", 10);
        assert!(!s.on_purpose && s.summary == "killed by signal 9 (SIGKILL)", "{s:#?}");
    }

    /// The last live test's end (2026-10-19): Android started the app's process again after its
    /// crash, and the new one was SIGKILLed minutes later. The last process's end is the card's.
    #[test]
    fn android_starting_the_app_again_has_its_last_process_judged_first() {
        let crash = [
            LAUNCH,
            "E/AndroidRuntime(47856): FATAL EXCEPTION: main",
            "E/AndroidRuntime(47856): Process: com.a, PID: 310000",
            "E/AndroidRuntime(47856): java.lang.IllegalStateException: boom",
            "E/AndroidRuntime(47856): \tat com.a.Main.onCreate(Main.java:12)",
            "I/Process(47856): Sending signal. PID: 310000 SIG: 9",
            "[zygote] pid 310000 ended: Ok(ExitStatus(unix_wait_status(35072)))",
            "I/ActivityManager( 1050): Process com.a (pid 310000) has died: fg  TOP ",
            "I/ActivityManager( 1050): Start proc 330000:com.a/u10a116 for service {com.a/com.a.Sync}",
            "[zygote] launching com.a as pid 330000 uid 1010116: android.app.ActivityThread seq=52",
        ];
        let then = |end: &[&str]| diagnose(&lines(&[&crash[..], end].concat()), "com.a", 10);
        let d = then(&["[zygote] pid 330000: signal 9, its host process ended", "[zygote] pid 330000 ended: Ok(ExitStatus(unix_wait_status(9)))"]);
        fits(&d);
        assert!(!d.on_purpose, "{d:#?}");
        assert_eq!(d.summary, "killed by signal 9 (SIGKILL)");
        assert!(d.details[0].contains("pid 330000"), "{d:#?}");
        assert_eq!(d.details.last().unwrap(), "Before that: its earlier process (pid 310000) ended: Java exception: java.lang.IllegalStateException: boom");
        // The last end alone says whether it was on purpose: force-stopped after the crash.
        let stopped = then(&["I/ActivityManager( 1050): Force stopping com.a appid=10116 user=10: from pid 998000", "I/ActivityManager( 1050): Killing 330000:com.a/u10a116 (adj 0): stop com.a due to from pid 998000"]);
        assert!(stopped.on_purpose && stopped.summary == "closed: force-stopped", "{stopped:#?}");
        assert!(stopped.details.last().unwrap().starts_with("Before that: its earlier process (pid 310000)"), "{stopped:#?}");
        // The new process alive, nothing said of its end: the crash is the end still.
        let alive = then(&[]);
        assert!(!alive.on_purpose && alive.summary == "Java exception: java.lang.IllegalStateException: boom", "{alive:#?}");
    }
}
