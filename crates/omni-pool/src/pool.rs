//! **The pool**: the warm devices and the app instances on them, shared by the app's screen and
//! its MCP server. Every call answers at once; the work (a boot, an install, a start) runs on
//! threads of its own, and [`Pool::snapshot`] shows where each one is.
//!
//! * A launch is placed by [`crate::plan::place`] under the pool's lock -- launches under way count
//!   as running -- so many launches at once still land as the rules say.
//! * One device does one install-and-start at a time: two apps cold-starting together on one
//!   device made one of them miss its input deadline (an ANR, 2026-10-10), one after the other
//!   neither did.
//! * A ticker (every second) follows the devices' boots, their logs (an app process's death, its
//!   window's removal), the windows people close and the cards they click, the apps that end on
//!   their own, and the idle devices: closed after `idle_close_minutes` unless they are the
//!   `keep_warm` kept warm, and one booted when none is idle.
//! * **An app that ends on its own keeps its window**: it crashed, was killed, exited, or its start
//!   failed -- the device's log since its start says which ([`crate::crash::diagnose`], from its
//!   [`LogMark`]), and its window shows that on a card ([`device::show_card`]) instead of Android's
//!   own screens, until the person closes it (or `close` or `clear_ended` takes it away, or its
//!   device ends -- the idle close passes it over). Its app and user are stopped at once: it holds
//!   its display, and keeps its device up, nothing else. The card offers a restart (a click on it).
//! * **An app closed on purpose closes its instance**: a force-stop, the app leaving at the
//!   person's asking (`Diagnosis::on_purpose`) -- as `close` closes one: no card, no restart.
//! * **A restart starts the app again on the same window** ([`Pool::restart`], or an auto restart
//!   after a crash, [`Pool::set_auto_restart`]): its app and user stopped, its card counting down
//!   [`RESTART_DELAY`] (a click starts it at once), then a launch's work again on its display and
//!   device -- placed anew when that device is gone or runs another build of the package now, and
//!   never once that device is being killed. [`QUICK_ENDS`] quick ends in a row stop the auto
//!   restarts: a crash loop is not run forever.
//! * **Cold after a kill**: killing the last Android (`kill`, `stop_device`) warms none in its place
//!   -- the person wanted none running -- until a launch or `warm`; and `pool.cold` beside the
//!   settings keeps that for the next time the app opens, which then warms none either.
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

use omni_warm::Device;

use crate::config::Config;
use crate::crash::{self, Diagnosis};
use crate::device;
use crate::extras::{self, Extras};
use crate::plan::{self, DeviceId, DeviceView, IdleView, Place};

/// An instance's id in the pool.
pub type InstanceId = u32;

/// Where a device is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceState {
    Booting,
    Ready,
    Closing,
    /// It ended, or never came up: why.
    Gone(String),
}

/// Where an instance is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceState {
    /// Its device is booting.
    Waiting,
    /// Installing, or waiting for the device's previous install-and-start.
    Installing,
    Starting,
    Running,
    Closing,
    /// Its app stopped or ended, and starts again on the same window once its countdown is done
    /// ([`Pool::restart`], or an auto restart after a crash).
    Restarting,
    /// It ended: why.
    Ended(String),
}

/// One device, as the screen and the MCP server see it.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub dir: PathBuf,
    pub state: DeviceState,
    pub up_for: Duration,
    /// How long it has run no app (`None` while it runs one or shows an ended one's card, or before
    /// it is ready).
    pub idle_for: Option<Duration>,
    /// Package -> the SHA-256 of the build installed.
    pub installed: BTreeMap<String, String>,
    pub instances: usize,
}

/// One instance, as the screen and the MCP server see it.
#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub id: InstanceId,
    pub device: DeviceId,
    pub package: String,
    pub sha256: String,
    pub apk: PathBuf,
    pub user: Option<u32>,
    /// Android's id of its display, while it has one: none once its window closed (an end on
    /// purpose, the person's close) -- `list_instances` named a display long gone (2026-10-20).
    pub display: Option<i32>,
    /// Its display's screenshot (`<device dir>.d<hwc>.png`), once it has one.
    pub screenshot: Option<PathBuf>,
    pub state: InstanceState,
    pub up_for: Duration,
    /// What its device's log says about how its app ended ([`crate::crash::diagnose`]), as its
    /// card shows it: empty unless it ended on its own -- kept through its restart until its next
    /// end, so one started again still tells why (an auto restart's crash was otherwise told only
    /// on its card, for 5 s).
    pub details: Vec<String>,
    /// Whether its app starts again after a crash: its own setting, else the pool's
    /// ([`Pool::set_auto_restart`]).
    pub auto_restart: bool,
    /// How many times its app was started again.
    pub restarts: u32,
    /// While [`InstanceState::Restarting`]: how long until its app starts again.
    pub restart_in: Option<Duration>,
}

struct DeviceRec {
    dir: PathBuf,
    child: Option<Child>,
    state: DeviceState,
    born: Instant,
    idle_since: Option<Instant>,
    installed: BTreeMap<String, String>,
    /// Users other than 0 the device has (each runs one instance at a time).
    users: Vec<u32>,
    /// One install-and-start at a time.
    busy: Arc<Mutex<()>>,
    /// A user is being made ahead for it ([`Pool::spare_users`]): not offered until it was started
    /// once, and no second one made meanwhile.
    sparing: bool,
    /// When it came up here (not one adopted): no spare user is made on it for [`SPARE_AFTER_READY`].
    ready_at: Option<Instant>,
    /// (package, user) whose grants (`device::grant_script`) ran here: a start skips them
    /// ([`DeviceRec::forget_grants`] when the package is installed again, its data cleared, a user
    /// made). They were a `dumpsys package` and a `cmd` per permission at every start, though one
    /// user takes most launches of a package on a device and every relaunch.
    granted: std::collections::BTreeSet<(String, u32)>,
}

impl DeviceRec {
    /// A device booting in `dir`.
    fn booting(dir: PathBuf) -> Self {
        Self {
            dir,
            child: None,
            state: DeviceState::Booting,
            born: Instant::now(),
            idle_since: None,
            installed: BTreeMap::new(),
            users: Vec::new(),
            busy: Arc::default(),
            sparing: false,
            ready_at: None,
            granted: std::collections::BTreeSet::new(),
        }
    }

    fn device(&self) -> Device {
        Device { dir: self.dir.clone() }
    }

    /// Its grants of `package` (`None`: every package) to `user` (`None`: every user) forgotten: they
    /// run again at the next start. Runtime permissions are Android's (not the app's data, which the
    /// cookie store's plant empties by hand), and go with a new install, `pm clear` (its data
    /// cleared), or the user: a new user's id may be a removed one's.
    fn forget_grants(&mut self, package: Option<&str>, user: Option<u32>) {
        self.granted.retain(|(p, u)| !(package.is_none_or(|w| w == p) && user.is_none_or(|w| w == *u)));
    }

    /// Whether a start of `package` as `user` here skips its grants ([`DeviceRec::granted`]).
    fn grants_known(&self, package: &str, user: u32) -> bool {
        self.granted.contains(&(package.to_string(), user))
    }

    /// `package` is about to be installed here: its grants go for every user -- a build installed
    /// over may ask for more, and an install of another package may have removed it.
    fn installing(&mut self, package: &str) {
        self.forget_grants(Some(package), None);
    }

    /// The user `user` was made here: its id's grants go, a removed user's id given again.
    fn user_made(&mut self, user: u32) {
        self.forget_grants(None, Some(user));
    }

    /// A start of `package` as `user` here answered `started` (`device::start`'s): its grants kept
    /// when it ran them from the package's whole dump -- not when it skipped them (the cache's own
    /// word, which a reset read meanwhile took back), nor when it failed.
    fn started(&mut self, package: &str, user: u32, started: &Result<(String, bool), String>) {
        if matches!(started, Ok((_, true))) {
            self.granted.insert((package.to_string(), user));
        }
    }
}

struct InstanceRec {
    device: DeviceId,
    apk: Arc<omni_warm::Apk>,
    user: Option<u32>,
    /// The composer's id of its display, while it has one: an ended instance that still has one
    /// shows its card there.
    hwc: Option<i64>,
    display: Option<i32>,
    state: InstanceState,
    born: Instant,
    /// Ticks in a row its app was not seen on its display.
    missing: u32,
    /// When the device's log last said a window of its package went ([`Pool::follow_logs`]): its
    /// activities are read each second until [`WINDOW_LOOK`] after ([`InstanceRec::looking`]).
    window_went: Option<Instant>,
    /// Where the device's log was when its start began.
    log_mark: Option<LogMark>,
    /// What the device's log said about its end, once it ended on its own: kept through a restart,
    /// until its next end (an end that is no end of its own, `end`, clears it).
    details: Vec<String>,
    /// When its start began (as `log_mark` was taken): an end within [`QUICK_END`] of it is a
    /// quick one.
    started: Option<Instant>,
    /// When it last reached [`InstanceState::Running`] ([`InstanceRec::settling`]).
    running_since: Option<Instant>,
    /// What its launch does beyond the install and the start, kept for a restart -- less the
    /// cookie store, an account's session, which goes once the launch's starts took their copies.
    extras: Extras,
    /// Its own auto restart setting (`None`: the pool's, `Config::auto_restart`).
    auto_restart: Option<bool>,
    /// How many times its app was started again.
    restarts: u32,
    /// While [`InstanceState::Restarting`]: its countdown.
    countdown: Option<Countdown>,
    /// The text of the card its window shows, less the restart line: sent again with each second's
    /// count ([`CardText::with`]).
    card: Option<CardText>,
    /// Its ends on its own in a row that came within [`QUICK_END`] of their start: [`QUICK_ENDS`]
    /// stop its auto restarts. A manual restart, or an end later than that, starts it over.
    quick_ends: u32,
    /// A close was asked for (`close`, its window closed by the person, `clear_ended`): what ends it
    /// is that close -- never a card, never a restart, whatever its app does meanwhile.
    closed: bool,
    /// Stops of its app and user under way ([`Pool::halt`]): its user is no other launch's, and its
    /// relaunch waits, until they are done -- else a start could come first, and the stop end it.
    halting: u32,
    /// The pids of its app's main process the device's log named since its start (`[zygote]
    /// launching <package> as pid <P> uid <U>`, U its user's), for [`Pool::follow_logs`].
    pids: Vec<u32>,
}

impl InstanceRec {
    /// A launch's instance on `device`, waiting for it.
    fn new(device: DeviceId, apk: Arc<omni_warm::Apk>, extras: Extras) -> Self {
        Self {
            device,
            apk,
            user: None,
            hwc: None,
            display: None,
            state: InstanceState::Waiting,
            born: Instant::now(),
            missing: 0,
            window_went: None,
            log_mark: None,
            details: Vec::new(),
            started: None,
            running_since: None,
            extras,
            auto_restart: None,
            restarts: 0,
            countdown: None,
            card: None,
            quick_ends: 0,
            closed: false,
            halting: 0,
            pids: Vec::new(),
        }
    }

    /// Whether it keeps its device from the idle close: live (a restart's countdown too), or ended
    /// with its window showing its card -- the device's stop would close that window, which is the
    /// person's to close.
    fn holds_device(&self) -> bool {
        !matches!(self.state, InstanceState::Ended(_)) || self.hwc.is_some()
    }

    /// One more end on its own, quick when within [`QUICK_END`] of its start: whether an auto
    /// restart may follow -- not after [`QUICK_ENDS`] quick ones in a row.
    fn count_end(&mut self) -> bool {
        let quick = self.started.is_some_and(|t| t.elapsed() < QUICK_END);
        self.quick_ends = if quick { self.quick_ends + 1 } else { 0 };
        self.quick_ends < QUICK_ENDS
    }

    /// Whether a window of its package went within [`WINDOW_LOOK`]: its activities are read each
    /// second ([`Pool::check_running`]), not only its process looked for.
    fn looking(&self) -> bool {
        self.window_went.is_some_and(|t| t.elapsed() < WINDOW_LOOK)
    }

    /// Whether its launch is under way on its device, or it reached `Running` less than
    /// [`SPARE_AFTER_START`] ago: no spare user is made there meanwhile ([`Pool::spare_users`]).
    /// From its end on too (`Closing`, a restart's countdown), its user still its own: a restart
    /// takes that user again, an end gives it back. A spare made in a crash's countdown took the
    /// device's turn before the relaunch and started beside it: start-user 4.3 s, not 0.6-1.0 s,
    /// running in 10.3 s, not 5-7 s (2026-10-20).
    fn settling(&self) -> bool {
        match self.state {
            InstanceState::Waiting | InstanceState::Installing | InstanceState::Starting | InstanceState::Closing | InstanceState::Restarting => true,
            InstanceState::Running => self.running_since.is_some_and(|t| t.elapsed() < SPARE_AFTER_START),
            _ => false,
        }
    }
}

/// A restart counting down ([`Pool::restart`]).
#[derive(Debug, Clone, Copy)]
struct Countdown {
    /// When its app starts again.
    at: Instant,
    /// An auto restart after a crash: its card says `Automatically restarting`.
    auto: bool,
    /// Which countdown it is (`State::next_countdown`): its thread ends once its instance's is
    /// another (a newer restart) or none (a close).
    id: u64,
}

/// What an instance's card says but its restart line and what closing its window does: the title,
/// the lines over the restart block (what happened), and those under it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CardText {
    title: String,
    lines: Vec<String>,
    /// Under the restart block, after an empty line: where the whole log is.
    footer: Vec<String>,
}

impl CardText {
    /// The card's text with its restart line right after its lines (`omni_linux::card` draws that
    /// block under them, above the footer, wherever it is) -- [`RESTART_NOW`], or a countdown's
    /// (`auto`, `left`: [`countdown_line`]) -- then an empty line, the footer, and last what
    /// closing the window does: while a restart counts down, it calls the restart off. `Close this
    /// window when you are done.` stayed under `Automatically restarting in 4 seconds` (2026-10-20).
    fn with(&self, countdown: Option<(bool, Duration)>) -> String {
        let restart = countdown.map_or_else(|| RESTART_NOW.to_string(), |(auto, left)| countdown_line(auto, left));
        let close = if countdown.is_some() { "Close this window to cancel the restart." } else { "Close this window when you are done." };
        let mut out = vec![self.title.as_str()];
        out.extend(self.lines.iter().map(String::as_str));
        out.push(&restart);
        out.push("");
        out.extend(self.footer.iter().map(String::as_str));
        out.push(close);
        out.join("\n")
    }
}

/// The card line that offers a restart and counts down none: the link alone. `omni_linux::card`'s
/// `RESTART_LINE` and its fields (`\u{1}restart\t<headline>\t<link>`), written literally: this crate
/// does not depend on that one.
const RESTART_NOW: &str = "\u{1}restart\t\tClick here to restart";

/// The whole seconds a countdown `left` from its end shows: rounded up, never 0 -- `1 second` until
/// its end.
fn seconds_left(left: Duration) -> u64 {
    u64::try_from(left.as_millis().div_ceil(1000)).unwrap_or(u64::MAX).max(1)
}

/// The card line of a restart `left` from now: its headline (`Restarting in 4 seconds`,
/// `Automatically restarting in ...` for an auto restart), then its link -- and once the countdown
/// is done (`left` zero), `Restarting now` and no link: the relaunch may still wait for the stop
/// of its app and for the device's turn (13-30 s behind an install, 2026-10-19), and `1 second`
/// stayed up all that while, its link doing nothing.
fn countdown_line(auto: bool, left: Duration) -> String {
    let what = if auto { "Automatically restarting" } else { "Restarting" };
    if left.is_zero() {
        return format!("\u{1}restart\t{what} now\t");
    }
    let secs = seconds_left(left);
    let unit = if secs == 1 { "second" } else { "seconds" };
    format!("\u{1}restart\t{what} in {secs} {unit}\tClick here to restart now")
}

/// What a card (and the instance's details) adds once [`QUICK_ENDS`] quick ends stopped its auto
/// restarts.
fn not_restarted() -> String {
    format!("Not restarted automatically: it ended {QUICK_ENDS} times in a row within a minute of starting.")
}

/// What a line of a device's log says of an app process or its windows ([`Pool::follow_logs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcLine<'a> {
    /// `[zygote] launching <process> as pid <P> uid <U>`: its process, pid and Android user
    /// (U / 100000).
    Launched { process: &'a str, pid: u32, user: u32 },
    /// ActivityManager's `Process <process> (pid <P>) has died`, or the zygote's `pid <P> ended` or
    /// `pid <P>: signal <n>, its host process ended`.
    Died(u32),
    /// `Input channel object '<window> <package>/<activity> (client)' was disposed ..`: an
    /// activity window of the package went -- its activity finished, or its process died -- as
    /// `crash::diagnose` reads a finish. It names no user.
    WindowGone(&'a str),
}

/// The digits `text` begins with, as a number.
fn leading_number<T: std::str::FromStr>(text: &str) -> Option<T> {
    text[..text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len())].parse().ok()
}

/// What `line` says of an app process, if anything: found anywhere in it, a time first or not
/// (`OMNI_LOG_TIME`).
fn proc_line(line: &str) -> Option<ProcLine<'_>> {
    if let Some((_, rest)) = line.split_once("[zygote] launching ") {
        let (process, rest) = rest.split_once(" as pid ")?;
        let uid: u32 = leading_number(rest.split_once(" uid ")?.1)?;
        return Some(ProcLine::Launched { process, pid: leading_number(rest)?, user: uid / 100_000 });
    }
    if let Some((_, rest)) = line.split_once("[zygote] pid ") {
        let after = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        return leading_number(rest).filter(|_| after.starts_with(" ended") || after.starts_with(": signal ")).map(ProcLine::Died);
    }
    if let Some((_, rest)) = line.split_once("Input channel object '") {
        let (_, name) = rest.split_once("' was disposed")?.0.split_once(' ')?;
        return name.split_once('/').map(|(package, _)| package).filter(|p| !p.contains(char::is_whitespace)).map(ProcLine::WindowGone);
    }
    let (_, rest) = line.split_once("Process ")?;
    if !rest.contains(") has died") {
        return None;
    }
    leading_number(rest.split_once(" (pid ")?.1).map(ProcLine::Died)
}

/// What grants of a device `line` of its log says Android took back ([`DeviceRec::forget_grants`]):
/// ActivityManager's `Force stopping <package> appid=<app> user=<user>: <reason>` for a reason no
/// process asked (`from pid <n>`: `am force-stop`, the pool's own stops) -- `clear data` (which
/// resets the package's runtime permissions), or the package installed over or removed.
/// `(package, Some(user))`, or `None` for every user (`user=-1`).
fn grants_reset(line: &str) -> Option<(&str, Option<u32>)> {
    let (_, rest) = line.split_once("Force stopping ")?;
    let (package, rest) = rest.split_once(" appid=")?;
    let (user, reason) = rest.split_once(" user=")?.1.split_once(':')?;
    let user: i64 = user.parse().ok()?;
    (!package.contains(char::is_whitespace) && !reason.trim_start().starts_with("from pid ")).then(|| (package, u32::try_from(user).ok()))
}

/// **Where a device's log was when an instance's start began**: what is read back when its app
/// ends, so its card tells of this start, not of what ran before it.
///
/// The log is renamed `<dir>.1.log` once, when a device's first boot is kept as the base image and
/// the device booted again from it (`omni-linux`'s `tests/common/boot.rs`, `Boot::reboot`, called by
/// `r_app`'s `warm_device`): a start on a device still in its first boot has its beginning there. A
/// `<dir>.1.log` that was not there at the start, or not the same one, is read from the mark first,
/// then the new log from its beginning. A log that shrank otherwise was started again, and is read
/// from its beginning (as [`omni_warm::LogTail`] does).
#[derive(Debug, Clone)]
struct LogMark {
    /// `<dir>.log`.
    log: PathBuf,
    /// `<dir>.1.log`.
    old: PathBuf,
    /// The log's length then.
    at: u64,
    /// `old`'s length and time then, if it was there.
    old_then: Option<(u64, Option<SystemTime>)>,
}

/// At most this much of a device's log is read for an instance's end (16 MiB): every guest log line
/// reaches it, so it grows for as long as the device runs, and what tells an end is at the two ends
/// of what was written since the start -- the app's processes started at the one, its death at the
/// other.
const LOG_READ: u64 = 16 << 20;
/// Of which the first (2 MiB); the rest is the last.
const LOG_HEAD: u64 = 2 << 20;

/// A file's length and modification time, if it is there: a log renamed over it is another.
fn file_id(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
}

impl LogMark {
    /// `dev`'s log as it is now.
    fn new(dev: &Device) -> Self {
        let (log, old) = (dev.log(), dev.dir.with_extension("1.log"));
        Self { at: std::fs::metadata(&log).map_or(0, |m| m.len()), old_then: file_id(&old), log, old }
    }

    /// The lines written since the mark, at most [`LOG_READ`] bytes of them ([`read_lines`]).
    fn lines(&self) -> Vec<String> {
        let len = std::fs::metadata(&self.log).map_or(0, |m| m.len());
        let segments = match file_id(&self.old) {
            Some(now) if Some(now) != self.old_then => vec![(self.old.clone(), self.at, now.0), (self.log.clone(), 0, len)],
            _ => vec![(self.log.clone(), if len < self.at { 0 } else { self.at }, len)],
        };
        read_lines(&segments, LOG_READ, LOG_HEAD)
    }
}

/// The bytes `segments` hold -- `(file, from, to)`, one after the other -- as lines, at most `max`
/// bytes of them: past that, the first `head` and the last `max - head`, each cut at a line's end,
/// and between them a line saying how much was left out. A file that cannot be read gives nothing.
fn read_lines(segments: &[(PathBuf, u64, u64)], max: u64, head: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let total: u64 = segments.iter().map(|(_, from, to)| to.saturating_sub(*from)).sum();
    // The bytes `skip..skip + len` of the segments joined.
    let read = |mut skip: u64, mut len: u64| {
        let mut out = Vec::new();
        for (path, from, to) in segments {
            let n = to.saturating_sub(*from);
            if skip >= n {
                skip -= n;
                continue;
            }
            let take = (n - skip).min(len);
            if let Ok(mut f) = std::fs::File::open(path) {
                if f.seek(SeekFrom::Start(from + skip)).is_ok() {
                    let _ = f.take(take).read_to_end(&mut out);
                }
            }
            (skip, len) = (0, len - take);
            if len == 0 {
                break;
            }
        }
        out
    };
    let lines = |bytes: &[u8]| String::from_utf8_lossy(bytes).lines().map(str::to_string).collect::<Vec<_>>();
    if total <= max {
        return lines(&read(0, total));
    }
    let mut first = read(0, head);
    first.truncate(first.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1));
    let mut last = read(total - (max - head), max - head);
    last.drain(..last.iter().position(|b| *b == b'\n').map_or(last.len(), |i| i + 1));
    let left_out = total - first.len() as u64 - last.len() as u64;
    let mut out = lines(&first);
    out.push(format!("[omnidroid: {:.1} MB of the log left out here]", left_out as f64 / 1e6));
    out.extend(lines(&last));
    out
}

/// The card an instance's window shows once its app ended on its own: the diagnosis' title and
/// lines, then (under its restart block) where the whole log is -- and what closing the window
/// does, [`CardText::with`] (`omni_linux::card` reads the first line as the title, every other as
/// one of its lines).
fn card_text(d: &Diagnosis, log: &Path) -> CardText {
    CardText { title: d.title.clone(), lines: d.details.clone(), footer: vec![format!("The whole log: {}", log.display())] }
}

/// The card of a restart asked while its app ran: nothing ended it, so nothing more to say.
fn asked_card(package: &str) -> CardText {
    CardText { title: format!("{package} is restarting"), lines: vec!["Restart asked.".to_string()], footer: Vec::new() }
}

/// What a start that failed after its display was made says: `<package> did not start`, the
/// error, then what the device's log says about the app -- only when it says something
/// (`found`): what an empty log gets is no news, and would put a crash that never was on the card.
fn did_not_start(package: &str, error: &str, d: Diagnosis, found: bool) -> Diagnosis {
    let mut error_lines = error.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut details = vec![format!("The start failed: {}", error_lines.next().unwrap_or("no answer"))];
    details.extend(error_lines.map(str::to_string));
    let summary = if found { format!("did not start: {}", d.summary) } else { format!("start: {}", error.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ")) };
    if found {
        details.push(String::new());
        details.extend(d.details);
    }
    Diagnosis { title: format!("{package} did not start"), summary, details, on_purpose: false }
}

/// The marker the window of the composer's display `hwc` leaves when the person closes it
/// (`omni_linux::displays::closed_marker`).
fn closed_marker(dev: &Device, hwc: i64) -> PathBuf {
    dev.ctl().join(format!("display-{hwc}.closed"))
}

/// The marker the window of the composer's display `hwc` leaves when the person clicks its card's
/// restart block (`omni_linux::displays::restart_marker`).
fn restart_marker(dev: &Device, hwc: i64) -> PathBuf {
    dev.ctl().join(format!("display-{hwc}.restart"))
}

/// The display `hwc` removed, and its window's markers: a removed display's id is the next one's.
fn remove_display(dev: &Device, hwc: i64) {
    if dev.alive() {
        let _ = device::remove_display(dev, hwc);
    }
    let _ = std::fs::remove_file(closed_marker(dev, hwc));
    let _ = std::fs::remove_file(restart_marker(dev, hwc));
}

/// Remove the displays that showed ended instances' cards, and their windows' markers -- on a
/// thread of its own: each is a round trip on its device's control channel, and the caller is the
/// ticker or a window's request.
fn remove_cards(cards: Vec<(Device, i64)>) {
    if cards.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("omni-pool-cards".into()).spawn(move || {
        for (dev, hwc) in cards {
            remove_display(&dev, hwc);
        }
    });
}

#[derive(Default)]
struct State {
    devices: BTreeMap<DeviceId, DeviceRec>,
    instances: BTreeMap<InstanceId, InstanceRec>,
    next_device: DeviceId,
    next_instance: InstanceId,
    /// The last countdown's id ([`Countdown::id`]).
    next_countdown: u64,
    /// The last device was killed: none is warmed until a launch or `warm`.
    cold: bool,
}

impl State {
    /// An instance takes up its device (and a user there) unless it ended.
    fn live(&self, device: DeviceId) -> impl Iterator<Item = (&InstanceId, &InstanceRec)> {
        self.instances.iter().filter(move |(_, i)| i.device == device && !matches!(i.state, InstanceState::Ended(_)))
    }

    /// The users of `device` instances other than `except` hold: the live ones', and those of the
    /// ones whose stop is still under way ([`InstanceRec::halting`]) -- a launch given one of
    /// those would have its app stopped under it once that stop had its turn.
    fn users_taken(&self, device: DeviceId, except: Option<InstanceId>) -> Vec<u32> {
        self.instances.iter().filter(|(id, i)| i.device == device && Some(**id) != except && (!matches!(i.state, InstanceState::Ended(_)) || i.halting > 0)).filter_map(|(_, i)| i.user).collect()
    }

    /// A new countdown's id.
    fn countdown(&mut self) -> u64 {
        self.next_countdown += 1;
        self.next_countdown
    }

    /// The ready devices a user is made ahead on now ([`Pool::spare_users`]): none made there
    /// already, none free there -- and none while an instance there is settling
    /// ([`InstanceRec::settling`]), nor for [`SPARE_AFTER_READY`] after the device came up.
    fn wants_spare(&self) -> Vec<DeviceId> {
        self.devices
            .iter()
            .filter(|(_, d)| d.state == DeviceState::Ready && !d.sparing && d.ready_at.is_none_or(|t| t.elapsed() >= SPARE_AFTER_READY))
            .filter(|(id, d)| {
                let taken = self.users_taken(**id, None);
                !d.users.iter().any(|u| !taken.contains(u)) && !self.instances.values().any(|i| i.device == **id && i.settling())
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// The running instances on `device`, when each one's app process is there (`apps`, the
    /// device's host's: uid and process name) and none calls for Android's activities -- no miss
    /// pending, no window of its package gone lately: each one's display (the composer's id) and
    /// its app's uid. `None` when any one does ([`Pool::check_running`]).
    fn seen_running(&self, device: DeviceId, apps: &[(u32, String)]) -> Option<Vec<(Option<i64>, u32)>> {
        self.instances
            .values()
            .filter(|i| i.device == device && i.state == InstanceState::Running)
            .map(|i| {
                let process_of = |name: &str| name == i.apk.package || name.strip_prefix(i.apk.package.as_str()).is_some_and(|rest| rest.starts_with(':'));
                let uid = i.user.and_then(|u| apps.iter().find(|(uid, name)| uid / 100_000 == u && process_of(name)).map(|(uid, _)| *uid))?;
                (i.missing == 0 && !i.looking()).then_some((i.hwc, uid))
            })
            .collect()
    }
}

/// Whether each of `seen`'s apps (`(display, uid)`, [`State::seen_running`]) draws on its display,
/// as the device's host says (`owners`: who draws on a display, `None` when it cannot say).
fn all_drawn(seen: &[(Option<i64>, u32)], mut owners: impl FnMut(i64) -> Option<Vec<(i32, u32)>>) -> bool {
    seen.iter().all(|&(hwc, uid)| hwc.and_then(&mut owners).is_some_and(|o| o.iter().any(|(_, u)| *u == uid)))
}

/// Whether a round of [`Pool::check_running`] takes the device's host's word for its running
/// instances (`there`, [`State::seen_running`]) and reads no `dumpsys`: each one's process there
/// -- and on a `full` round its app drawing on its display ([`all_drawn`]; `owners` asked then
/// alone).
fn vouched(full: bool, there: Option<&[(Option<i64>, u32)]>, owners: impl FnMut(i64) -> Option<Vec<(i32, u32)>>) -> bool {
    there.is_some_and(|there| !full || all_drawn(there, owners))
}

/// How many lines of the pool's log are kept.
const LOG_LINES: usize = 400;
/// How long a device may take to come up.
const BOOT_LIMIT: Duration = Duration::from_secs(20 * 60);
/// How long a restart counts down on the card before the app starts again: long enough to read
/// what happened, and to click the card to restart at once.
pub const RESTART_DELAY: Duration = Duration::from_secs(5);
/// An end on its own this soon after its start began is a quick one: an app that cannot start here
/// (a crash at its launch, a library the engine cannot run) ends within this, every time.
const QUICK_END: Duration = Duration::from_secs(60);
/// This many quick ends in a row stop an instance's auto restarts: a crash loop would otherwise
/// hold a device's turn and a user for nothing, a start every few seconds, forever.
const QUICK_ENDS: u32 = 3;
/// How often a countdown looks at its instance and its card's link: a click starts the app within
/// this.
const COUNTDOWN_POLL: Duration = Duration::from_millis(100);
/// How long after the device's log says an app process died its end is read: the other lines of
/// that death (the zygote's account of its host process's exit, the engine's report) are written
/// by other processes, around ActivityManager's -- before it or after (2026-10-19's logs, both).
const LOG_SETTLE: Duration = Duration::from_secs(1);
/// How long after the device's log says a window of a running app's package went its activities
/// are read each second ([`Pool::check_running`]): long enough for the two misses in a row an end
/// needs, a dumpsys taking ~1.5-1.9 s; a window back meanwhile (a rotation's, the next activity's)
/// is seen there, and costs those reads alone.
const WINDOW_LOOK: Duration = Duration::from_secs(10);
/// No user is made ahead on a device while a launch there is under way, nor for this long after
/// its app reached `Running` ([`Pool::spare_users`]): a user's making and first start took
/// 0.45-0.57 of a core and ~500k binder calls over ~25 s, and came during a first launch's sign-in
/// (Roblox, 2026-10-20). A launch in that while makes its own user, as one did before.
const SPARE_AFTER_START: Duration = Duration::from_secs(60);
/// Nor for this long after the device came up: the first spare user was made in its first 30 s,
/// beside a launch that came with the boot.
const SPARE_AFTER_READY: Duration = Duration::from_secs(30);

pub struct Pool {
    state: Mutex<State>,
    config: Mutex<Config>,
    config_path: Option<PathBuf>,
    /// The launcher a device is booted with (`omnidroid aosp --warm`).
    exe: PathBuf,
    /// Where devices live (`omni_warm::root`).
    root: PathBuf,
    log: Mutex<VecDeque<String>>,
    /// Lines ever logged.
    logged: std::sync::atomic::AtomicU64,
    started: Instant,
    stopping: std::sync::atomic::AtomicBool,
    /// The (device, package) a link is being followed for now: one instance at a time each
    /// ([`Pool::follow_link`]).
    link_turns: Mutex<std::collections::BTreeSet<(DeviceId, String)>>,
    /// Where the ticker is in each ready device's log ([`Pool::follow_logs`]).
    tails: Mutex<BTreeMap<DeviceId, omni_warm::LogTail>>,
}

/// An instance's turn at its link on its device ([`Pool::follow_link`]), given back when dropped.
struct LinkTurn<'a> {
    turns: &'a Mutex<std::collections::BTreeSet<(DeviceId, String)>>,
    key: (DeviceId, String),
}

impl Drop for LinkTurn<'_> {
    fn drop(&mut self) {
        self.turns.lock().remove(&self.key);
    }
}

impl Pool {
    /// A pool with these settings, booting devices with `exe`: the devices this app left running
    /// are adopted, and its ticker started (it warms a device when the settings say so, and the
    /// last device was not killed).
    #[must_use]
    pub fn start(config: Config, config_path: Option<PathBuf>, exe: PathBuf) -> Arc<Self> {
        let pool = Arc::new(Self::new(config, config_path, exe));
        if pool.state.lock().cold {
            pool.say("no Android warmed: the last one was killed (`warm` boots one; a launch does too)");
        }
        pool.adopt();
        let ticker = Arc::clone(&pool);
        let _ = std::thread::Builder::new().name("omni-pool-ticker".into()).spawn(move || ticker.tick_forever());
        pool
    }

    /// [`start`](Self::start), but adopting none of the devices left running: a pool beside the
    /// person's own (a test's), whose `start` took that pool's devices over and removed their
    /// displays -- closing its apps' windows.
    #[must_use]
    pub fn start_alone(config: Config, config_path: Option<PathBuf>, exe: PathBuf) -> Arc<Self> {
        let pool = Arc::new(Self::new(config, config_path, exe));
        let ticker = Arc::clone(&pool);
        let _ = std::thread::Builder::new().name("omni-pool-ticker".into()).spawn(move || ticker.tick_forever());
        pool
    }

    /// The pool, nothing adopted and no ticker: cold if `pool.cold` is beside the settings.
    fn new(config: Config, config_path: Option<PathBuf>, exe: PathBuf) -> Self {
        let cold = config_path.as_deref().and_then(cold_file).is_some_and(|f| f.exists());
        Self {
            state: Mutex::new(State { next_device: 1, next_instance: 1, cold, ..State::default() }),
            config: Mutex::new(config),
            config_path,
            exe,
            root: omni_warm::root(),
            log: Mutex::new(VecDeque::new()),
            logged: std::sync::atomic::AtomicU64::new(0),
            started: Instant::now(),
            stopping: std::sync::atomic::AtomicBool::new(false),
            link_turns: Mutex::default(),
            tails: Mutex::default(),
        }
    }

    /// The last device was killed: no device is warmed until a launch or `warm`.
    #[must_use]
    pub fn cold(&self) -> bool {
        self.state.lock().cold
    }

    /// Cold or not, and kept so (`pool.cold`) for the next time the app opens.
    fn keep_cold(&self, cold: bool) {
        let Some(file) = self.config_path.as_deref().and_then(cold_file) else { return };
        let done = if cold { std::fs::write(&file, "the last Android was killed: none is warmed when the app opens\n") } else { std::fs::remove_file(&file).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }) };
        if let Err(e) = done {
            self.say(format!("{}: {e}", file.display()));
        }
    }

    /// A launch or a `warm`: devices are warmed again.
    fn warm_again(&self) {
        let was = std::mem::take(&mut self.state.lock().cold);
        if was {
            self.keep_cold(false);
            self.say(format!("Androids are kept warm again (keep_warm = {})", self.config.lock().keep_warm));
        }
    }

    /// A line in the pool's log.
    pub fn say(&self, line: impl Into<String>) {
        let secs = self.started.elapsed().as_secs();
        let mut log = self.log.lock();
        log.push_back(format!("{:02}:{:02}:{:02} {}", secs / 3600, secs / 60 % 60, secs % 60, line.into()));
        self.logged.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        while log.len() > LOG_LINES {
            log.pop_front();
        }
    }

    /// The pool's log, oldest first.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.lock().iter().cloned().collect()
    }

    /// The lines logged after the first `seen` ever logged (those still kept), and how many have
    /// been logged in all -- the next call's `seen`.
    #[must_use]
    pub fn log_since(&self, seen: u64) -> (Vec<String>, u64) {
        let log = self.log.lock();
        let total = self.logged.load(std::sync::atomic::Ordering::Relaxed);
        let new = usize::try_from(total.saturating_sub(seen)).unwrap_or(usize::MAX).min(log.len());
        (log.iter().skip(log.len() - new).cloned().collect(), total)
    }

    /// The settings now.
    #[must_use]
    pub fn config(&self) -> Config {
        self.config.lock().clone()
    }

    /// Change a setting, and keep it.
    ///
    /// # Errors
    /// An unknown key, a bad value, or the file could not be written.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let mut c = self.config.lock();
        if !c.set(key, value)? {
            return Err(format!("no setting `{key}` (settings lists them)"));
        }
        if let Some(path) = &self.config_path {
            c.save(path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        drop(c);
        self.say(format!("setting {key} = {value}"));
        Ok(())
    }

    /// What there is now.
    #[must_use]
    pub fn snapshot(&self) -> (Vec<DeviceInfo>, Vec<InstanceInfo>) {
        let auto_restart = self.config.lock().auto_restart;
        let s = self.state.lock();
        let devices = s
            .devices
            .iter()
            .map(|(id, d)| DeviceInfo {
                id: *id,
                dir: d.dir.clone(),
                state: d.state.clone(),
                up_for: d.born.elapsed(),
                idle_for: d.idle_since.map(|t| t.elapsed()),
                installed: d.installed.clone(),
                instances: s.live(*id).count(),
            })
            .collect();
        let instances = s
            .instances
            .iter()
            .map(|(id, i)| {
                let dir = s.devices.get(&i.device).map(|d| d.dir.clone());
                InstanceInfo {
                    id: *id,
                    device: i.device,
                    package: i.apk.package.clone(),
                    sha256: i.apk.sha256.clone(),
                    apk: i.apk.bundle.clone().unwrap_or_else(|| i.apk.path.clone()),
                    user: i.user,
                    display: i.display.filter(|_| i.hwc.is_some()),
                    screenshot: dir.zip(i.hwc).map(|(d, h)| {
                        let stem = d.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        d.with_file_name(format!("{stem}.d{h}.png"))
                    }),
                    state: i.state.clone(),
                    up_for: i.born.elapsed(),
                    details: i.details.clone(),
                    auto_restart: i.auto_restart.unwrap_or(auto_restart),
                    restarts: i.restarts,
                    restart_in: i.countdown.filter(|_| i.state == InstanceState::Restarting).map(|c| c.at.saturating_duration_since(Instant::now())),
                }
            })
            .collect();
        (devices, instances)
    }

    /// A device's directory (for its log, its shell).
    #[must_use]
    pub fn device(&self, id: DeviceId) -> Option<Device> {
        self.state.lock().devices.get(&id).filter(|d| !matches!(d.state, DeviceState::Gone(_))).map(DeviceRec::device)
    }

    /// Launch `count` instances of the APK at `apk` -- any APK. Answers their ids at once; each one's
    /// state is in [`Pool::snapshot`].
    ///
    /// # Errors
    /// Not an APK, or one with no launcher Activity; a device limit reached.
    pub fn launch(self: &Arc<Self>, apk: &Path, count: u32) -> Result<Vec<InstanceId>, String> {
        self.launch_with(apk, count, Extras::default())
    }

    /// [`launch`](Self::launch), each instance doing `extras` beyond the install and the start (a
    /// plugin's: a cookie planted for its user, a link opened on its display).
    ///
    /// # Errors
    /// As `launch`.
    pub fn launch_with(self: &Arc<Self>, apk: &Path, count: u32, extras: Extras) -> Result<Vec<InstanceId>, String> {
        self.launch_package(apk, None, count, extras)
    }

    /// [`launch_with`](Self::launch_with) of a file that may be a bundle of APKs (`.apkm`, `.xapk`,
    /// `.apks`, a zip of APKs: `omni_warm::bundle`), its package `package` -- needed when it holds
    /// several (the error names them).
    ///
    /// # Errors
    /// As `launch`; a bundle of several packages and no `package`, or `package` not in it.
    pub fn launch_package(self: &Arc<Self>, apk: &Path, package: Option<&str>, count: u32, extras: Extras) -> Result<Vec<InstanceId>, String> {
        let apk = std::fs::canonicalize(apk).map_err(|e| format!("{}: {e}", apk.display()))?;
        let apk = Arc::new(omni_warm::Apk::read_package(&apk, package)?);
        let launcher = apk.launcher.clone().ok_or_else(|| format!("{} has no launcher Activity: nothing to start", apk.package))?;
        let max = self.config.lock().max_devices;
        let mut ids = Vec::new();
        let mut boots = Vec::new();
        {
            let mut s = self.state.lock();
            for _ in 0..count.max(1) {
                let Some(device) = self.place(&mut s, &apk, max, &mut boots) else {
                    if ids.is_empty() {
                        return Err(format!("{max} devices run already (max_devices): close one, or raise it"));
                    }
                    self.say(format!("max_devices ({max}) reached: {} of {count} launched", ids.len()));
                    break;
                };
                let id = s.next_instance;
                s.next_instance += 1;
                s.instances.insert(id, InstanceRec::new(device, Arc::clone(&apk), extras.kept()));
                ids.push(id);
            }
        }
        self.warm_again();
        for id in boots {
            self.boot(id);
        }
        for &id in &ids {
            let pool = Arc::clone(self);
            let launcher = launcher.clone();
            let extras = extras.clone();
            let _ = std::thread::Builder::new().name(format!("omni-pool-launch-{id}")).spawn(move || pool.run_launch(id, &launcher, extras));
        }
        self.say(format!("launch {} x{} ({}): instance{} {}", apk.describe(), ids.len(), &apk.sha256[..12], if ids.len() == 1 { "" } else { "s" }, ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" ")));
        for line in &extras.said {
            self.say(format!("  {line}"));
        }
        Ok(ids)
    }

    /// Where a launch of `apk` goes, under the pool's lock (`s`): the device [`plan::place`] names,
    /// or a new one, booting -- pushed to `boots`, booted once the lock is let go. The build is the
    /// device's from then on, so the next launch placed under the lock sees it. `None`: a device
    /// would be booted, and `max` (`max_devices`, 0 none) run already.
    fn place(&self, s: &mut State, apk: &omni_warm::Apk, max: u32, boots: &mut Vec<DeviceId>) -> Option<DeviceId> {
        let views: Vec<DeviceView> = s
            .devices
            .iter()
            .filter(|(_, d)| matches!(d.state, DeviceState::Booting | DeviceState::Ready))
            .map(|(id, d)| DeviceView {
                id: *id,
                ready: d.state == DeviceState::Ready,
                installed: d.installed.clone(),
                running: s.live(*id).map(|(_, i)| i.apk.package.clone()).collect(),
                closing: false,
            })
            .collect();
        let device = match plan::place(&views, &apk.package, &apk.sha256) {
            Place::Run(d) | Place::Install(d) | Place::Replace(d) => d,
            Place::Boot => {
                let alive = s.devices.values().filter(|d| matches!(d.state, DeviceState::Booting | DeviceState::Ready)).count() as u32;
                if max > 0 && alive >= max {
                    return None;
                }
                let id = s.next_device;
                s.next_device += 1;
                s.devices.insert(id, DeviceRec::booting(device::new_dir(&self.root)));
                boots.push(id);
                id
            }
        };
        if let Some(d) = s.devices.get_mut(&device) {
            d.installed.insert(apk.package.clone(), apk.sha256.clone());
            d.idle_since = None;
        }
        Some(device)
    }

    /// Warm a device now (one more than there are); the pool is not cold any more.
    pub fn warm(self: &Arc<Self>) -> DeviceId {
        self.warm_again();
        // Never `None`: not `unless_cold`.
        self.add_device(false).unwrap_or_default()
    }

    /// A device booted; none `unless_cold` and the pool is cold (the ticker's, racing a kill).
    fn add_device(&self, unless_cold: bool) -> Option<DeviceId> {
        let id = {
            let mut s = self.state.lock();
            if unless_cold && s.cold {
                return None;
            }
            let id = s.next_device;
            s.next_device += 1;
            s.devices.insert(id, DeviceRec::booting(device::new_dir(&self.root)));
            id
        };
        self.boot(id);
        Some(id)
    }

    fn boot(&self, id: DeviceId) {
        // Nothing boots once the app is quitting (a launch or the ticker racing its shutdown).
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            self.device_gone(id, "not booted: the app is quitting", false);
            return;
        }
        let (dir, config) = {
            let s = self.state.lock();
            let Some(d) = s.devices.get(&id) else { return };
            (d.dir.clone(), self.config.lock().clone())
        };
        match device::boot(&self.exe, &dir, &config) {
            Ok(child) => {
                if let Some(d) = self.state.lock().devices.get_mut(&id) {
                    d.child = Some(child);
                }
                self.say(format!("device D{id} booting ({})", dir.display()));
            }
            Err(e) => {
                self.say(format!("device D{id} could not boot: {e}"));
                self.device_gone(id, &format!("could not boot: {e}"), false);
            }
        }
    }

    fn set_state(&self, id: InstanceId, state: InstanceState) {
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.state = state;
        }
    }

    /// One launch's work: wait for the device, install, a display and a user, start.
    fn run_launch(self: Arc<Self>, id: InstanceId, launcher: &str, mut extras: Extras) {
        let Some((device_id, apk)) = self.state.lock().instances.get(&id).map(|i| (i.device, Arc::clone(&i.apk))) else { return };
        // The device up.
        let t = Instant::now();
        loop {
            let state = self.state.lock().devices.get(&device_id).map(|d| d.state.clone());
            match state {
                Some(DeviceState::Ready) => break,
                Some(DeviceState::Booting) if t.elapsed() < BOOT_LIMIT => std::thread::sleep(Duration::from_millis(500)),
                Some(DeviceState::Gone(why)) => return self.end(id, &format!("its device D{device_id} is gone: {why}")),
                _ => return self.end(id, &format!("its device D{device_id} did not come up")),
            }
            if matches!(self.state.lock().instances.get(&id).map(|i| &i.state), Some(InstanceState::Closing | InstanceState::Ended(_)) | None) {
                return;
            }
        }
        self.set_state(id, InstanceState::Installing);
        let Some((dev, busy, size)) = self.state.lock().devices.get(&device_id).map(|d| (d.device(), Arc::clone(&d.busy), self.config.lock().instance_size)) else { return };
        let turn = busy.lock();
        if matches!(self.state.lock().instances.get(&id).map(|i| &i.state), Some(InstanceState::Closing | InstanceState::Ended(_)) | None) {
            return;
        }
        // Every end from here gives the device's turn back first: `cleanup` takes it, and a
        // `parking_lot` mutex taken twice by one thread waits for itself -- the launch's thread, and
        // every later install-and-start on the device, then waited forever.
        // A restart on the display it kept (it has one before its own is made) fails on its card.
        let kept_display = || self.state.lock().instances.get(&id).is_some_and(|i| i.hwc.is_some());
        // Installed unless the device holds these bytes; another build of the package (none of it
        // running: the placement saw to that, a relaunch's too) is replaced.
        let held = dev.installed_apk(&apk.package).and_then(|p| omni_warm::sha256_file(&p).ok());
        if held.as_deref() != Some(apk.sha256.as_str()) {
            if let Some(d) = self.state.lock().devices.get_mut(&device_id) {
                d.installing(&apk.package);
            }
            match device::install(&dev, &apk) {
                Ok(i) => self.say(format!("#{id} {} {} on D{device_id} in {:.1} s", apk.package, i.action, i.seconds)),
                Err(e) => {
                    drop(turn);
                    return self.not_started(id, &format!("install: {e}"), kept_display());
                }
            }
        }
        // Its user: one of its own, never user 0. With users visible on displays of their own,
        // Android counts user 0 visible on display 0 alone, and refuses an Activity the app itself
        // starts on another display ("Not allow to launch ... on display 3": Super Mechs' sign-in,
        // 2026-10-10) -- moving the app to display 0, which has no window. A user started on the
        // instance's display is visible there, and the app's own Activities stay with it.
        // A restart's own user first, when free: its app's data is there (a sign-in its launch
        // planted, which a restart without a new cookie store keeps so).
        let user = {
            let s = self.state.lock();
            let taken = s.users_taken(device_id, Some(id));
            let before = s.instances.get(&id).and_then(|i| i.user);
            s.devices.get(&device_id).and_then(|d| before.filter(|u| d.users.contains(u)).into_iter().chain(d.users.iter().copied()).find(|u| !taken.contains(u)))
        };
        let user = match user {
            Some(u) => u,
            None => match device::create_user(&dev) {
                Ok(u) => {
                    if let Some(d) = self.state.lock().devices.get_mut(&device_id) {
                        d.users.push(u);
                        d.user_made(u);
                    }
                    self.say(format!("D{device_id}: user {u} made for #{id}"));
                    u
                }
                Err(e) => {
                    drop(turn);
                    return self.not_started(id, &format!("a user for it: {e}"), kept_display());
                }
            },
        };
        // Unless a close came during the install: its end, waiting for the turn, has nothing to
        // undo yet (taken to `Starting` over it, the start went on, and its failure put a card up
        // on a closed instance's display).
        let closed = match self.state.lock().instances.get_mut(&id) {
            Some(i) if i.state == InstanceState::Installing => {
                i.user = Some(user);
                i.state = InstanceState::Starting;
                false
            }
            _ => true,
        };
        if closed {
            return;
        }
        // A restart keeps the display it had (its window, showing the restart's card until the
        // start); a launch gets one.
        let kept = self.state.lock().instances.get(&id).and_then(|i| i.hwc.zip(i.display));
        let (hwc, display) = match kept.map_or_else(|| device::add_display(&dev, size), Ok) {
            Ok(d) => d,
            Err(e) => {
                drop(turn);
                return self.end(id, &format!("its display: {e}"));
            }
        };
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            (i.hwc, i.display) = (Some(hwc), Some(display));
        }
        // The launch's cookie store, handed over to the device for this user's app: the host's copy
        // removed once the start is done, whatever happens (the guest's goes with the plant).
        let mut handed = None;
        let before = match &extras.cookie_store {
            None => String::new(),
            Some(store) => match omni_warm::hand_over_path(&dev, &format!("cookies-{id}.db")).and_then(|(host, guest)| {
                std::fs::copy(store.path(), &host).map_err(|e| format!("{}: {e}", host.display()))?;
                Ok((host, guest))
            }) {
                Ok((host, guest)) => {
                    handed = Some(extras::TempFile::new(host));
                    extras::plant(&apk.package, user, &guest)
                }
                Err(e) => {
                    drop(turn);
                    return self.not_started(id, &format!("its cookie store: {e}"), kept.is_some());
                }
            },
        };
        // A bundle's OBB files, copied to this user's OBB directory (user 0's has them).
        let obbs = omni_warm::obbs_for_user(&apk, user);
        let before = if obbs.is_empty() { before } else if before.is_empty() { obbs } else { format!("{before}
{obbs}") };
        // The device's log from before the start: what says the app is ready for the link.
        let log = extras.link.as_ref().map(|_| omni_warm::LogTail::from_end(&dev.log()));
        // And where it was, for what the card says should the app end on its own.
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.log_mark = Some(LogMark::new(&dev));
            i.started = Some(Instant::now());
        }
        // A kept display's card taken away: its window black until the app's frames come (the
        // composer hides Android's home), its keyboard and mouse the app's again. A click on the
        // card meanwhile is no news: its marker goes with it.
        if kept.is_some() {
            if let Err(e) = device::show_display(&dev, hwc) {
                self.say(format!("#{id}: its card stays over it: {e}"));
            }
            let _ = std::fs::remove_file(restart_marker(&dev, hwc));
        }
        let granted = self.state.lock().devices.get(&device_id).is_some_and(|d| d.grants_known(&apk.package, user));
        let mut steps = device::Steps::default();
        let t = Instant::now();
        let started = device::start(&dev, &apk.package, launcher, user, display, &before, granted, &mut steps);
        if let Some(d) = self.state.lock().devices.get_mut(&device_id) {
            d.started(&apk.package, user, &started);
        }
        // Done with the account's session: the launch's store goes with its last instance's start.
        drop(handed);
        let planted = extras.cookie_store.take().is_some();
        match started {
            Ok((out, _)) => {
                if planted {
                    if out.contains("cookie store planted") {
                        self.say(format!("#{id}: signed in with the cookie (never shown)"));
                    } else {
                        self.say(format!("#{id}: the cookie store was not planted"));
                    }
                }
                let closing = {
                    let mut s = self.state.lock();
                    match s.instances.get_mut(&id) {
                        Some(i) if i.state == InstanceState::Starting => {
                            i.state = InstanceState::Running;
                            i.running_since = Some(Instant::now());
                            false
                        }
                        _ => true,
                    }
                };
                self.say(format!("#{id} {} running on D{device_id} (user {user}, display {display}) in {:.1} s", apk.package, t.elapsed().as_secs_f64()));
                self.say(format!("#{id}: its start: {steps}"));
                // The device's next install-and-start need not wait for this one's link.
                drop(turn);
                if closing {
                    self.cleanup(id);
                } else if let (Some(link), Some(log)) = (&extras.link, log) {
                    self.follow_link(id, (device_id, &apk.package), &dev, (user, display), link, &extras, log);
                }
            }
            // Its display there: the window keeps it, and says why on a card -- unless a close came
            // first, whose end, waiting for the turn given back here, stops its user and removes
            // its display (read then, `cleanup`).
            Err(e) => {
                drop(turn);
                self.say(format!("#{id}: its start: {steps}"));
                if self.take_ended(id, &InstanceState::Starting) {
                    self.ended_on_its_own(id, Some(&e));
                }
            }
        }
    }

    /// The launch's link, opened on the instance's display once the device's log says its app is
    /// ready for it (or signed in) -- at once when nothing names either -- and again while nothing
    /// says it was taken. Ends with the instance, or after [`extras::LINK_WAIT`].
    ///
    /// **One instance of an app on a device at a time**: most of an app's log lines name no user, so
    /// a sign-in or a join logged while two instances of it wait would be either's. The link is
    /// sent, and a line naming no user counted, only while this instance holds the device's turn
    /// for the package; a line naming a user (an Activity's record, a data path) counts any time.
    #[allow(clippy::too_many_arguments)]
    fn follow_link(&self, id: InstanceId, (device_id, package): (DeviceId, &str), dev: &Device, (user, display): (u32, i32), link: &str, extras: &Extras, mut log: omni_warm::LogTail) {
        let t = Instant::now();
        let mut ready = extras.link_ready.is_none() && extras.signed_in.is_none();
        let (mut signed_in, mut taken, mut sent_at, mut tries) = (false, false, None::<Instant>, 0u32);
        let mut turn: Option<LinkTurn> = None;
        let mut waited = false;
        while t.elapsed() < extras::LINK_WAIT && !taken {
            if !matches!(self.state.lock().instances.get(&id).map(|i| &i.state), Some(InstanceState::Running)) {
                return;
            }
            for line in log.lines() {
                let alone = turn.is_some();
                ready |= extras::says(&line, extras.link_ready.as_deref(), user, alone);
                if !signed_in && extras::says(&line, extras.signed_in.as_deref(), user, alone) {
                    signed_in = true;
                    self.say(format!("#{id}: signed in"));
                }
                if sent_at.is_some() && extras::says(&line, extras.link_taken.as_deref(), user, alone) {
                    taken = true;
                    self.say(format!("#{id}: the link was taken"));
                }
            }
            let due = match sent_at {
                None => ready || signed_in,
                Some(at) => extras.link_taken.is_some() && !taken && at.elapsed() > extras::LINK_RETRY && tries < extras::LINK_TRIES,
            };
            if due && turn.is_none() {
                let key = (device_id, package.to_string());
                if self.link_turns.lock().insert(key.clone()) {
                    turn = Some(LinkTurn { turns: &self.link_turns, key });
                } else {
                    if !waited {
                        self.say(format!("#{id}: its link waits for the {package} beside it on D{device_id} to join"));
                        waited = true;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            }
            if due {
                tries += 1;
                match dev.shell_as(&extras::link_command(link, user, display), Some(0), Duration::from_secs(60)) {
                    Ok(_) => self.say(format!("#{id}: link sent{}", if tries > 1 { format!(" (try {tries})") } else { String::new() })),
                    Err(e) => self.say(format!("#{id}: the link: {e}")),
                }
                sent_at = Some(Instant::now());
                if extras.link_taken.is_none() {
                    return;
                }
            }
            if sent_at.is_some_and(|at| tries >= extras::LINK_TRIES && at.elapsed() > extras::LINK_RETRY) {
                self.say(format!("#{id}: the link was never taken"));
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// End an instance with `why`, and give back what it held. What an earlier end of its own said
    /// (kept through its restart) goes: `why` is this end's.
    fn end(&self, id: InstanceId, why: &str) {
        self.say(format!("#{id} ended: {why}"));
        self.cleanup(id);
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.state = InstanceState::Ended(why.to_string());
            i.countdown = None;
            i.details.clear();
        }
        self.mark_idle();
    }

    /// A launch's work that failed before its start (`why`): a restart on the display it kept
    /// (`kept`) says so on its card ([`Pool::not_restarted`]), its user stopped by nobody (nothing
    /// ran as it this time) -- the window is the person's, and a click on it tries again; through
    /// [`Pool::end`] it closed with nothing said on it. Else the instance ends, its display (one
    /// just made, nothing on it yet) removed.
    fn not_started(&self, id: InstanceId, why: &str, kept: bool) {
        if kept {
            self.not_restarted(id, why);
        } else {
            self.end(id, why);
        }
    }

    /// Stop an instance's app and user, and remove its display -- both read under the device's
    /// turn, the display taken from its record. A launch that held the turn as the end came set them
    /// meanwhile: read before, a close while the display was made left that display behind, its
    /// window open on nothing until the device stopped. And two ends never remove one display
    /// twice: the composer gives a removed display's id to the next one added. The user of an
    /// instance that ended already is not stopped again: it is free, perhaps another's by now.
    fn cleanup(&self, id: InstanceId) {
        let Some((dev, package, busy)) = ({
            let s = self.state.lock();
            s.instances.get(&id).and_then(|i| s.devices.get(&i.device).map(|d| (d.device(), i.apk.package.clone(), Arc::clone(&d.busy))))
        }) else {
            return;
        };
        if !dev.alive() {
            return;
        }
        let _turn = busy.lock();
        let Some((user, hwc)) = self.state.lock().instances.get_mut(&id).map(|i| (i.user.filter(|_| !matches!(i.state, InstanceState::Ended(_))), i.hwc.take())) else {
            return;
        };
        if let Some(u) = user {
            if let Err(e) = device::stop(&dev, &package, u) {
                self.say(format!("#{id}: stopping it: {e}"));
            }
        }
        if let Some(h) = hwc {
            remove_display(&dev, h);
        }
    }

    /// An instance whose app ended on its own taken out of `from` (`Running`, or `Starting` for a
    /// start that failed) into `Closing`, under the lock: no other round of the ticker ends it again
    /// while its end is read. False when it was not there: a close, or its device's end, came first
    /// and ends it.
    fn take_ended(&self, id: InstanceId, from: &InstanceState) -> bool {
        match self.state.lock().instances.get_mut(&id) {
            Some(i) if i.state == *from => {
                i.state = InstanceState::Closing;
                true
            }
            _ => false,
        }
    }

    /// An instance whose app ended on its own -- it stopped (`failed` `None`), or its start failed
    /// after its display was made (`Some(error)`) -- once [`take_ended`](Self::take_ended): what
    /// happened read from the device's log since its start ([`crash::diagnose`]), and then:
    ///
    /// * closed on purpose (`Diagnosis::on_purpose`: a force-stop, the app leaving at the person's
    ///   asking): closed as [`Pool::close`] closes one -- its display removed, no card, no restart;
    /// * its auto restart on, and not after [`QUICK_ENDS`] quick ends: [`InstanceState::Restarting`],
    ///   its card counting down ([`Pool::count_down`]);
    /// * else ended, its card in its window ([`card_text`], a click on it restarts), its display
    ///   kept until the person closes the window ([`Pool::tick`]).
    ///
    /// Its state, details and the pool's line are set before the device's turn is waited for: the
    /// stop of its app and user ([`Pool::halt`]) may wait behind an install, and the screen and the
    /// MCP server said `running` for 13-30 s after the card showed when that came first
    /// (2026-10-19). The card shows before the stop too (the stop's own lines, a force-stop, are not
    /// the app's end). A close that came first ends it instead (`closed`), as that close does.
    fn ended_on_its_own(self: &Arc<Self>, id: InstanceId, failed: Option<&str>) {
        let Some((dev, package, user, busy, mark)) = ({
            let s = self.state.lock();
            s.instances.get(&id).and_then(|i| s.devices.get(&i.device).map(|d| (d.device(), i.apk.package.clone(), i.user, Arc::clone(&d.busy), i.log_mark.clone())))
        }) else {
            // Its device is gone: that ended it.
            return;
        };
        let lines = mark.map(|m| m.lines()).unwrap_or_default();
        let as_user = user.unwrap_or(0);
        let mut d = crash::diagnose(&lines, &package, as_user);
        if let Some(error) = failed {
            let found = d.summary != crash::diagnose(&[], &package, as_user).summary;
            d = did_not_start(&package, error, d, found);
        }
        let alive = dev.alive();
        let auto_restart = self.config.lock().auto_restart;
        // Nothing restarts once the app is quitting.
        let stopping = self.stopping.load(std::sync::atomic::Ordering::Acquire);
        /// What its end comes to.
        enum Then {
            /// A close, or its device's end, came first: that does the rest.
            Nothing,
            /// Closed on purpose: its display (taken from its record) removed.
            Closed(Option<i64>),
            /// Ended: its card shown on its display.
            Card(Option<i64>, CardText),
            /// Restarting: the countdown's, on its display.
            Restart(u64, Option<i64>),
        }
        let (then, halt) = {
            let mut s = self.state.lock();
            let countdown = s.countdown();
            match s.instances.get_mut(&id) {
                None => (Then::Nothing, false),
                Some(i) if i.closed || matches!(i.state, InstanceState::Ended(_)) => (Then::Nothing, false),
                Some(i) => {
                    // Its user kept from other launches until its stop is done.
                    let halt = alive && user.is_some();
                    if halt {
                        i.halting += 1;
                    }
                    i.details = d.details.clone();
                    let then = if d.on_purpose {
                        i.state = InstanceState::Ended(d.summary.clone());
                        i.card = None;
                        Then::Closed(i.hwc.take())
                    } else {
                        let mut card = card_text(&d, &dev.log());
                        let auto = i.auto_restart.unwrap_or(auto_restart) && !stopping;
                        let again = i.count_end();
                        if auto && !again {
                            card.lines.push(String::new());
                            card.lines.push(not_restarted());
                            i.details.push(not_restarted());
                        }
                        i.card = Some(card.clone());
                        if auto && again {
                            i.state = InstanceState::Restarting;
                            i.countdown = Some(Countdown { at: Instant::now() + RESTART_DELAY, auto: true, id: countdown });
                            Then::Restart(countdown, i.hwc.filter(|_| alive))
                        } else {
                            i.state = InstanceState::Ended(d.summary.clone());
                            Then::Card(i.hwc.filter(|_| alive), card)
                        }
                    };
                    (then, halt)
                }
            }
        };
        // A click on an earlier card is not one on this.
        if let Then::Card(Some(h), _) | Then::Restart(_, Some(h)) = &then {
            let _ = std::fs::remove_file(restart_marker(&dev, *h));
        }
        match then {
            Then::Nothing => {}
            Then::Closed(hwc) => {
                self.say(format!("#{id} closed on purpose: {}", d.summary));
                if let Some(h) = hwc {
                    remove_display(&dev, h);
                }
            }
            Then::Card(hwc, card) => {
                self.say(format!("#{id} ended: {}", d.summary));
                if let Some(h) = hwc {
                    self.show(id, &dev, h, &card.with(None));
                }
            }
            Then::Restart(countdown, _) => {
                self.say(format!("#{id} ended: {}; restarting automatically in {} s", d.summary, RESTART_DELAY.as_secs()));
                self.follow_countdown(id, countdown);
            }
        }
        if let Some(u) = user.filter(|_| halt) {
            self.halt(id, &dev, &package, u, &busy);
        }
        self.mark_idle();
    }

    /// Show `text` on the card of instance `id`'s display `hwc` -- or, when it cannot be (a device
    /// built before cards), close its window instead: the display taken from its record first,
    /// unless a close took it already -- the composer gives a removed display's id to the next one
    /// added, another launch's, which a later close of this instance would remove. A restart then
    /// gets a new one.
    fn show(&self, id: InstanceId, dev: &Device, hwc: i64, text: &str) {
        let Err(e) = device::show_card(dev, hwc, text) else { return };
        self.say(format!("#{id}: its card: {e} (its window closes instead)"));
        let taken = self.state.lock().instances.get_mut(&id).is_some_and(|i| {
            let here = i.hwc == Some(hwc);
            if here {
                (i.hwc, i.display) = (None, None);
            }
            here
        });
        if taken {
            remove_display(dev, hwc);
        }
    }

    /// Stop instance `id`'s app and user (`user`, of `package`) under its device's turn (`busy`),
    /// then give back the [`InstanceRec::halting`] it was counted under: its user is free again, and
    /// its relaunch may go.
    fn halt(&self, id: InstanceId, dev: &Device, package: &str, user: u32, busy: &Mutex<()>) {
        {
            let _turn = busy.lock();
            if dev.alive() {
                if let Err(e) = device::stop(dev, package, user) {
                    self.say(format!("#{id}: stopping it: {e}"));
                }
            }
        }
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.halting = i.halting.saturating_sub(1);
        }
    }

    /// Close an instance: its app stopped, its window gone -- an ended one's too, while it shows
    /// its card (the screen's close, the MCP server's `stop_instance`: as the person closing the
    /// window). A restart counting down is called off; whatever its app does meanwhile (a crash as
    /// it is stopped, a start that fails) shows no card and restarts nothing (`closed`).
    ///
    /// # Errors
    /// No such instance, or it ended and its window is gone already.
    pub fn close(self: &Arc<Self>, id: InstanceId) -> Result<(), String> {
        let card = {
            let mut s = self.state.lock();
            let State { devices, instances, .. } = &mut *s;
            let i = instances.get_mut(&id).ok_or_else(|| format!("no instance #{id}"))?;
            if matches!(i.state, InstanceState::Ended(_)) {
                // Taken from it here, as the ticker does for a window the person closed.
                let Some(card) = devices.get(&i.device).map(DeviceRec::device).zip(i.hwc.take()) else {
                    return Err(format!("#{id} ended already"));
                };
                i.closed = true;
                Some(card)
            } else {
                (i.state, i.closed, i.countdown) = (InstanceState::Closing, true, None);
                None
            }
        };
        if let Some(card) = card {
            self.say(format!("#{id}: its card's window closed"));
            remove_cards(vec![card]);
            self.mark_idle();
            return Ok(());
        }
        let pool = Arc::clone(self);
        let _ = std::thread::Builder::new().name(format!("omni-pool-close-{id}")).spawn(move || pool.end(id, "closed"));
        Ok(())
    }

    /// **Start an instance's app again on its window** after `delay` (`Duration::ZERO`: now), its
    /// card counting down until then (a click on it: now): one running -- its app and user stopped
    /// first, as an end stops them --, one that ended (its window shown or not: one closed gets a
    /// new one), or one restarting already (the new countdown replaces the old). Its quick ends are
    /// forgotten: the person asked.
    ///
    /// # Errors
    /// No such instance, or one still on its way up or closing (`#3 is still starting`), or the app
    /// is quitting.
    pub fn restart(self: &Arc<Self>, id: InstanceId, delay: Duration) -> Result<(), String> {
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return Err("the app is quitting: nothing restarts".to_string());
        }
        let (countdown, halt) = {
            let mut s = self.state.lock();
            let countdown = s.countdown();
            let taken = s.instances.get(&id).map(|i| s.users_taken(i.device, Some(id))).unwrap_or_default();
            let State { devices, instances, .. } = &mut *s;
            let i = instances.get_mut(&id).ok_or_else(|| format!("no instance #{id}"))?;
            let mut halt = None;
            match i.state {
                InstanceState::Waiting | InstanceState::Installing | InstanceState::Starting => return Err(format!("#{id} is still starting")),
                InstanceState::Closing => return Err(format!("#{id} is closing")),
                // Restarting under the lock before its stop: the running check (`check_running`,
                // `follow_logs`) does not take that stop for an end on its own. Nothing ended it on
                // its own: an earlier end's details go.
                InstanceState::Running => {
                    if let Some((d, u)) = devices.get(&i.device).zip(i.user) {
                        i.halting += 1;
                        halt = Some((d.device(), i.apk.package.clone(), u, Arc::clone(&d.busy)));
                    }
                    i.card = Some(asked_card(&i.apk.package));
                    i.details.clear();
                }
                InstanceState::Ended(_) | InstanceState::Restarting => {
                    if i.hwc.is_some() && i.card.is_none() {
                        i.card = Some(asked_card(&i.apk.package));
                    }
                    // An ended one's user was free once its stop was done, and another launch may
                    // hold it now: kept, a close before the relaunch's start stopped that one's app
                    // and user (`cleanup` stops a live instance's), and its relaunch takes another.
                    if i.user.is_some_and(|u| taken.contains(&u)) {
                        i.user = None;
                    }
                }
            }
            i.state = InstanceState::Restarting;
            i.countdown = Some(Countdown { at: Instant::now() + delay, auto: false, id: countdown });
            (i.quick_ends, i.closed) = (0, false);
            (countdown, halt)
        };
        self.say(if delay.is_zero() { format!("#{id}: restart asked: now") } else { format!("#{id}: restart asked: in {} s", seconds_left(delay)) });
        self.follow_countdown(id, countdown);
        if let Some((dev, package, user, busy)) = halt {
            let pool = Arc::clone(self);
            let _ = std::thread::Builder::new().name(format!("omni-pool-halt-{id}")).spawn(move || pool.halt(id, &dev, &package, user, &busy));
        }
        Ok(())
    }

    /// A thread for the countdown `countdown` of instance `id` ([`Pool::count_down`]).
    fn follow_countdown(self: &Arc<Self>, id: InstanceId, countdown: u64) {
        let pool = Arc::clone(self);
        let _ = std::thread::Builder::new().name(format!("omni-pool-restart-{id}")).spawn(move || pool.count_down(id, countdown));
    }

    /// **A restart's countdown**, every [`COUNTDOWN_POLL`]: its card sent again each whole second
    /// with the new count (then `Restarting now`), its link's click (the window's restart marker)
    /// taken as the countdown's end, and at its end the relaunch -- once the stop of its app is done
    /// ([`Pool::halt`]). Ends as soon as the instance is gone, closed, or counting down another (a
    /// newer restart), or the app quits. No device command is sent under the pool's lock.
    fn count_down(self: Arc<Self>, id: InstanceId, countdown: u64) {
        let mut shown = None;
        loop {
            if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let Some((at, auto, card, text, halting)) = ({
                let mut s = self.state.lock();
                let State { devices, instances, .. } = &mut *s;
                instances.get_mut(&id).filter(|i| i.state == InstanceState::Restarting && !i.closed).and_then(|i| {
                    let c = i.countdown.as_mut().filter(|c| c.id == countdown)?;
                    let card = devices.get(&i.device).map(DeviceRec::device).zip(i.hwc);
                    if let Some((dev, h)) = &card {
                        if std::fs::remove_file(restart_marker(dev, *h)).is_ok() {
                            c.at = c.at.min(Instant::now());
                        }
                    }
                    Some((c.at, c.auto, card, i.card.clone(), i.halting))
                })
            }) else {
                return;
            };
            let left = at.saturating_duration_since(Instant::now());
            // Each whole second's count, then (0) `Restarting now`, before the relaunch: it may
            // wait for the stop of its app here, and for the device's turn after.
            if let (Some((dev, h)), Some(text)) = (card, text) {
                let secs = if left.is_zero() { 0 } else { seconds_left(left) };
                if shown != Some(secs) {
                    shown = Some(secs);
                    self.show(id, &dev, h, &text.with(Some((auto, left))));
                }
            }
            if left.is_zero() && halting == 0 {
                return self.relaunch(id, countdown);
            }
            std::thread::sleep(COUNTDOWN_POLL);
        }
    }

    /// **An instance's app started again** at its countdown's end (`countdown`, unless another came
    /// or a close): its record as a new launch's -- its start's time and log mark gone (what its
    /// last end said kept, for the screen and the MCP server), one restart more --, its launch's
    /// extras made again ([`Extras::remake`]: a plugin's cookie store went with the last start), and
    /// a launch's work ([`Pool::run_launch`]) on its device and display -- placed as a launch is
    /// ([`Pool::place`]) when that device is gone, or runs another build of the package now. Its
    /// user is whichever is free there, as a launch's.
    fn relaunch(self: Arc<Self>, id: InstanceId, countdown: u64) {
        // Taken out of the countdown under the lock: `Waiting`, as a launch, so no other restart of
        // it starts meanwhile, and a close ends it as a launch's.
        let Some((auto, restarts, extras, apk)) = ({
            let mut s = self.state.lock();
            s.instances.get_mut(&id).filter(|i| i.state == InstanceState::Restarting && !i.closed && i.halting == 0 && i.countdown.is_some_and(|c| c.id == countdown)).map(|i| {
                let auto = i.countdown.take().is_some_and(|c| c.auto);
                i.state = InstanceState::Waiting;
                (i.born, i.missing, i.window_went, i.log_mark, i.started, i.running_since, i.card) = (Instant::now(), 0, None, None, None, None, None);
                i.pids.clear();
                i.restarts += 1;
                (auto, i.restarts, i.extras.clone(), Arc::clone(&i.apk))
            })
        }) else {
            return;
        };
        self.say(format!("#{id} restarting ({}, restart {restarts})", if auto { "automatic" } else { "manual" }));
        // Asked of the plugin off the lock: it may take a while (a cookie store made).
        let remade = extras.remake.is_some();
        let extras = match extras.remake.as_ref().map(|remake| remake()) {
            None => Ok(extras),
            Some(Ok(new)) => Ok(Extras { remake: new.remake.clone().or_else(|| extras.remake.clone()), ..new }),
            Some(Err(e)) => Err(e),
        };
        let max = self.config.lock().max_devices;
        let mut boots = Vec::new();
        // Still `Waiting` (no close came): on its device when that is ready and its build may run
        // there, else placed anew (the display it kept there, answered, to remove); else why not.
        let placed = {
            let mut s = self.state.lock();
            match (s.instances.get(&id).filter(|i| i.state == InstanceState::Waiting && !i.closed).map(|i| i.device), &extras) {
                (None, _) => Err(None),
                (Some(_), Err(e)) => Err(Some(format!("restart: its launch could not be made again: {e}"))),
                (Some(device), Ok(_)) => {
                    // `plan`'s rule: a device running another build of the package is never
                    // touched. A launch may have put one there since this one ended (an ended
                    // instance runs nothing): installed over, that build's instances were killed,
                    // their ends read as force-stops on purpose -- no card, no restart.
                    let other_build = s.devices.get(&device).and_then(|d| d.installed.get(&apk.package)).is_some_and(|sha| *sha != apk.sha256)
                        && s.live(device).any(|(other, i)| *other != id && i.apk.package == apk.package);
                    match s.devices.get(&device).map(|d| d.state.clone()) {
                        // Its device being stopped (`kill`): that stop ends it (`device_gone`).
                        // Placed anew, it escaped the kill on another device -- booted for it when
                        // none was left, the pool warm again.
                        Some(DeviceState::Closing) => Err(None),
                        // Its build the device's from now on, as a placement makes it.
                        Some(DeviceState::Ready) if !other_build => {
                            if let Some(d) = s.devices.get_mut(&device) {
                                d.installed.insert(apk.package.clone(), apk.sha256.clone());
                                d.idle_since = None;
                            }
                            Ok(None)
                        }
                        _ => match self.place(&mut s, &apk, max, &mut boots) {
                            // Its display, user and Android display were the old device's.
                            Some(new) => {
                                let old = s.devices.get(&device).map(DeviceRec::device);
                                Ok(s.instances.get_mut(&id).and_then(|i| {
                                    let kept = old.zip(i.hwc);
                                    (i.device, i.hwc, i.display, i.user) = (new, None, None, None);
                                    kept
                                }))
                            }
                            None => Err(Some(format!("not restarted: {max} devices run already (max_devices)"))),
                        },
                    }
                }
            }
        };
        let extras = match (placed, extras) {
            (Ok(kept), Ok(extras)) => {
                // Its window on the device it left closes; a new one opens with its start.
                if let Some((dev, h)) = kept {
                    remove_display(&dev, h);
                }
                extras
            }
            // A close came, or its device's stop: that ends it. The extras go with this (a cookie
            // store's file too).
            (Err(None), _) => return,
            (Err(Some(why)), _) | (Ok(_), Err(why)) => return self.not_restarted(id, &why),
        };
        if let Some(i) = self.state.lock().instances.get_mut(&id) {
            i.extras = extras.kept();
        }
        self.warm_again();
        for d in boots {
            self.boot(d);
        }
        // What the plugin said of it anew (the launch's own lines were logged with the launch).
        for line in extras.said.iter().filter(|_| remade) {
            self.say(format!("  {line}"));
        }
        let launcher = apk.launcher.clone().unwrap_or_default();
        let pool = Arc::clone(&self);
        let _ = std::thread::Builder::new().name(format!("omni-pool-launch-{id}")).spawn(move || pool.run_launch(id, &launcher, extras));
    }

    /// A relaunch that could not go, or failed before its start (`why`, [`Pool::not_started`]): the
    /// instance ended with that, said on its card when it still has its window -- a click on it
    /// tries again. A close that came first ends it instead.
    fn not_restarted(&self, id: InstanceId, why: &str) {
        let shown = {
            let mut s = self.state.lock();
            let State { devices, instances, .. } = &mut *s;
            let Some(i) = instances.get_mut(&id).filter(|i| matches!(i.state, InstanceState::Waiting | InstanceState::Installing | InstanceState::Starting) && !i.closed) else { return };
            let card = CardText { title: format!("{} did not restart", i.apk.package), lines: vec![why.to_string()], footer: Vec::new() };
            i.state = InstanceState::Ended(why.to_string());
            i.details = vec![why.to_string()];
            i.card = Some(card.clone());
            devices.get(&i.device).map(DeviceRec::device).zip(i.hwc).map(|(dev, h)| (dev, h, card))
        };
        self.say(format!("#{id} ended: {why}"));
        if let Some((dev, h, card)) = shown {
            self.show(id, &dev, h, &card.with(None));
        }
        self.mark_idle();
    }

    /// Auto restart after a crash on or off: the pool's setting (`id` `None`: `auto_restart`, kept
    /// as [`Pool::set`] keeps it, for every instance without its own), or one instance's own, which
    /// overrides the pool's.
    ///
    /// # Errors
    /// No such instance, or the settings could not be written.
    pub fn set_auto_restart(&self, id: Option<InstanceId>, on: bool) -> Result<(), String> {
        let Some(id) = id else { return self.set("auto_restart", if on { "true" } else { "false" }) };
        self.state.lock().instances.get_mut(&id).ok_or_else(|| format!("no instance #{id}"))?.auto_restart = Some(on);
        self.say(format!("#{id}: auto restart {}", if on { "on" } else { "off" }));
        Ok(())
    }

    /// Whether a window still shows an ended instance's card: the person has yet to close it, and
    /// stopping its device (the app quitting with `stop_on_exit`) would close it for them.
    #[must_use]
    pub fn shows_cards(&self) -> bool {
        self.state.lock().instances.values().any(|i| matches!(i.state, InstanceState::Ended(_)) && i.hwc.is_some())
    }

    /// Forget the instances that ended; the windows still showing their cards close. A restart
    /// counting down is called off, and its instance ends as closed and is forgotten too -- but for
    /// one whose stop is still under way (kept, ended, until a later clear: its user is not free
    /// before that stop is done).
    pub fn clear_ended(&self) {
        let cards = {
            let mut s = self.state.lock();
            let State { devices, instances, .. } = &mut *s;
            for i in instances.values_mut().filter(|i| i.state == InstanceState::Restarting) {
                (i.state, i.closed, i.countdown) = (InstanceState::Ended("closed".to_string()), true, None);
            }
            let ended = |i: &InstanceRec| matches!(i.state, InstanceState::Ended(_));
            let cards: Vec<(Device, i64)> = instances.values_mut().filter(|i| ended(i)).filter_map(|i| Some((devices.get(&i.device)?.device(), i.hwc.take()?))).collect();
            instances.retain(|_, i| !ended(i) || i.halting > 0);
            cards
        };
        remove_cards(cards);
    }

    /// Stop a device and end its instances, as the person asked: when it was the last one, the
    /// pool is cold -- none warmed in its place, nor when the app opens next -- until a launch or
    /// `warm`.
    ///
    /// # Errors
    /// No such device.
    pub fn kill(&self, id: DeviceId) -> Result<(), String> {
        self.stop_device(id, true)
    }

    /// Stop a device and end its instances; `by_hand`, a kill (the last one makes the pool cold),
    /// else the idle close.
    fn stop_device(&self, id: DeviceId, by_hand: bool) -> Result<(), String> {
        let (dev, child, last) = {
            let mut s = self.state.lock();
            let d = s.devices.get_mut(&id).ok_or_else(|| format!("no device D{id}"))?;
            d.state = DeviceState::Closing;
            let (dev, child) = (d.device(), d.child.take());
            // Its restarts counting down called off now, as its end would: the wait for its
            // launcher below takes up to 20 s, and a countdown that ran out meanwhile was placed on
            // another device (`relaunch` refuses a closing one too).
            for i in s.instances.values_mut().filter(|i| i.device == id && i.state == InstanceState::Restarting) {
                (i.state, i.countdown) = (InstanceState::Ended(format!("its device D{id} stopped")), None);
            }
            // Under the lock the ticker counts devices with: it never sees none and warms one.
            let last = by_hand && !s.cold && !s.devices.values().any(|d| matches!(d.state, DeviceState::Booting | DeviceState::Ready));
            if last {
                s.cold = true;
            }
            (dev, child, last)
        };
        self.say(format!("device D{id} stopping"));
        if last {
            self.keep_cold(true);
            self.say("that was the last Android: none is warmed again, nor when the app opens next, until a launch or `warm`");
        }
        dev.stop();
        if let Some(mut child) = child {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(20) && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(200));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(device::marker(&dev.dir));
        self.device_gone(id, "stopped", false);
        Ok(())
    }

    /// A device gone, and its instances with it -- but when it died on its own (`relaunch`: the
    /// ticker's, not a kill, the idle close or the app quitting), those running with auto restart
    /// on start again elsewhere after [`RESTART_DELAY`] (no window is left to count down on), and
    /// those restarting already do at their countdown's end, placed anew ([`Pool::relaunch`]).
    /// Answers the countdowns begun, for their threads.
    fn device_gone(&self, id: DeviceId, why: &str, relaunch: bool) -> Vec<(InstanceId, u64)> {
        let auto_restart = self.config.lock().auto_restart;
        let relaunch = relaunch && !self.stopping.load(std::sync::atomic::Ordering::Acquire);
        let mut again = Vec::new();
        let mut s = self.state.lock();
        if let Some(d) = s.devices.get_mut(&id) {
            d.state = DeviceState::Gone(why.to_string());
        }
        let ids: Vec<InstanceId> = s.instances.iter().filter(|(_, i)| i.device == id).map(|(iid, _)| *iid).collect();
        for iid in ids {
            let countdown = s.countdown();
            let Some(i) = s.instances.get_mut(&iid) else { continue };
            // Its window went with the device, a card's too: nothing to remove.
            i.hwc = None;
            if matches!(i.state, InstanceState::Ended(_)) {
                continue;
            }
            let restarting = relaunch && i.state == InstanceState::Restarting && !i.closed;
            let auto = relaunch && i.state == InstanceState::Running && !i.closed && i.auto_restart.unwrap_or(auto_restart);
            let again_ok = auto && i.count_end();
            if restarting || again_ok {
                // Its display and user were that device's.
                (i.display, i.user) = (None, None);
            }
            if again_ok {
                i.state = InstanceState::Restarting;
                i.countdown = Some(Countdown { at: Instant::now() + RESTART_DELAY, auto: true, id: countdown });
                (i.details, i.card) = (vec![format!("Its device D{id} {why}.")], None);
                again.push((iid, countdown));
            } else if !restarting {
                if auto {
                    i.details = vec![format!("Its device D{id} {why}."), not_restarted()];
                }
                i.state = InstanceState::Ended(format!("its device D{id} {why}"));
            }
        }
        s.devices.retain(|_, d| !matches!(d.state, DeviceState::Gone(_)));
        again
    }

    /// Stop everything (when the app quits with `stop_on_exit`).
    pub fn shutdown(&self, stop_devices: bool) {
        self.stopping.store(true, std::sync::atomic::Ordering::Release);
        if !stop_devices {
            return;
        }
        let ids: Vec<DeviceId> = self.state.lock().devices.keys().copied().collect();
        let threads: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                let dev = self.device(id)?;
                Some(std::thread::spawn(move || dev.stop()))
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }
        let mut s = self.state.lock();
        for d in s.devices.values_mut() {
            if let Some(mut c) = d.child.take() {
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(20) && matches!(c.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = std::fs::remove_file(device::marker(&d.dir));
        }
    }

    /// A ready device no instance holds ([`InstanceRec::holds_device`]) is idle from now.
    fn mark_idle(&self) {
        let mut s = self.state.lock();
        let busy: Vec<DeviceId> = s.instances.values().filter(|i| i.holds_device()).map(|i| i.device).collect();
        for (id, d) in &mut s.devices {
            if d.state == DeviceState::Ready && !busy.contains(id) && d.idle_since.is_none() {
                d.idle_since = Some(Instant::now());
            } else if busy.contains(id) {
                d.idle_since = None;
            }
        }
    }

    /// The devices this app left running (`<dir>.pool`), taken back as idle devices: their extra
    /// displays removed, their users and builds read.
    fn adopt(&self) {
        for dev in omni_warm::find_all() {
            if !device::marker(&dev.dir).exists() || !dev.ready() {
                continue;
            }
            for hwc in device::extra_displays(&dev) {
                let _ = device::remove_display(&dev, hwc);
            }
            let installed: BTreeMap<String, String> = dev
                .installed_packages()
                .into_iter()
                .filter_map(|p| Some((p.clone(), omni_warm::sha256_file(&dev.installed_apk(&p)?).ok()?)))
                .collect();
            let users: Vec<u32> = device::list_users(&dev).unwrap_or_default().into_iter().filter(|u| *u != 0).collect();
            let mut s = self.state.lock();
            let id = s.next_device;
            s.next_device += 1;
            s.devices.insert(id, DeviceRec { state: DeviceState::Ready, idle_since: Some(Instant::now()), installed, users, ..DeviceRec::booting(dev.dir.clone()) });
            drop(s);
            self.say(format!("device D{id} adopted ({}), left running by the app before", dev.dir.display()));
        }
    }

    fn tick_forever(self: Arc<Self>) {
        let mut n = 0u64;
        while !self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(Duration::from_secs(1));
            n += 1;
            self.tick(n);
        }
    }

    fn tick(self: &Arc<Self>, n: u64) {
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // Boots and deaths.
        let mut came = Vec::new();
        let mut gone = Vec::new();
        {
            let mut s = self.state.lock();
            for (id, d) in &mut s.devices {
                let exited = d.child.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None)));
                match d.state {
                    DeviceState::Booting if d.device().ready() => {
                        d.state = DeviceState::Ready;
                        d.idle_since = Some(Instant::now());
                        d.ready_at = d.idle_since;
                        came.push((*id, d.born.elapsed()));
                    }
                    DeviceState::Booting if exited => gone.push((*id, format!("ended while booting (its log: {})", d.dir.with_extension("launcher.log").display()))),
                    DeviceState::Booting if d.born.elapsed() > BOOT_LIMIT => gone.push((*id, "did not come up".to_string())),
                    DeviceState::Ready if exited || (d.child.is_none() && !d.device().alive()) => gone.push((*id, "ended".to_string())),
                    _ => {}
                }
            }
        }
        for (id, took) in came {
            self.say(format!("device D{id} ready in {:.0} s", took.as_secs_f64()));
        }
        for (id, why) in gone {
            self.say(format!("device D{id} {why}"));
            for (instance, countdown) in self.device_gone(id, &why, true) {
                self.say(format!("#{instance}: restarting automatically on another device in {} s", RESTART_DELAY.as_secs()));
                self.follow_countdown(instance, countdown);
            }
        }
        self.mark_idle();
        // Windows closed by the person: their instances close -- an ended one's, showing its card,
        // has its display removed (taken from it here, so the next tick does not again); one on its
        // way up or restarting on its window closes as a running one does. And an ended one's card
        // clicked: restarted at once (a countdown's card is its countdown's to watch).
        let (closed, cards, clicked) = {
            let mut s = self.state.lock();
            let State { devices, instances, .. } = &mut *s;
            let (mut closed, mut cards, mut clicked) = (Vec::new(), Vec::new(), Vec::new());
            for (id, i) in instances.iter_mut() {
                let Some((dev, h)) = devices.get(&i.device).map(DeviceRec::device).zip(i.hwc) else { continue };
                if closed_marker(&dev, h).exists() {
                    match i.state {
                        InstanceState::Closing => {}
                        InstanceState::Ended(_) => {
                            i.hwc = None;
                            cards.push((*id, dev, h));
                        }
                        _ => closed.push(*id),
                    }
                } else if matches!(i.state, InstanceState::Ended(_)) && std::fs::remove_file(restart_marker(&dev, h)).is_ok() {
                    clicked.push(*id);
                }
            }
            (closed, cards, clicked)
        };
        for id in closed {
            self.say(format!("#{id}: its window was closed"));
            let _ = self.close(id);
        }
        for (id, _, _) in &cards {
            self.say(format!("#{id}: its card's window was closed"));
        }
        remove_cards(cards.into_iter().map(|(_, dev, h)| (dev, h)).collect());
        for id in clicked {
            self.say(format!("#{id}: restart clicked on its card"));
            if let Err(e) = self.restart(id, Duration::ZERO) {
                self.say(format!("#{id}: {e}"));
            }
        }
        // App processes the devices' logs say died: their instances ended at once.
        self.follow_logs();
        // Apps that ended on their own otherwise (every 5 s; each second where a window of one went
        // lately): not on their display twice in a row.
        self.check_running(n % 5 == 0, n % 60 == 0);
        // Idle devices out (one showing a card is not); a warm one in.
        let (views, config, cold) = {
            let s = self.state.lock();
            let views: Vec<IdleView> = s
                .devices
                .iter()
                .filter(|(_, d)| matches!(d.state, DeviceState::Booting | DeviceState::Ready))
                .map(|(id, d)| IdleView {
                    id: *id,
                    ready: d.state == DeviceState::Ready,
                    running: s.instances.values().filter(|i| i.device == *id && i.holds_device()).count(),
                    idle_for: d.idle_since.map(|t| t.elapsed()),
                })
                .collect();
            (views, self.config.lock().clone(), s.cold)
        };
        for id in plan::to_close(&views, config.idle_close_minutes, config.keep_warm) {
            self.say(format!("device D{id} idle {} min: closing it", config.idle_close_minutes));
            let pool = Arc::clone(self);
            let _ = std::thread::spawn(move || pool.stop_device(id, false));
        }
        // A user made ahead on each ready device that has none free (~10-15 s off the next launch).
        if n % 5 == 0 {
            self.spare_users();
        }
        if config.warm_on_start && !cold {
            for _ in 0..plan::to_warm(&views, config.keep_warm, config.max_devices) {
                let Some(id) = self.add_device(true) else { break };
                self.say(format!("device D{id}: warming one, so the next launch starts at once"));
            }
        }
    }

    /// One free user ahead on each ready device that has none, made off the launch path. Only its
    /// making (`pm create-user`) takes the device's turn: its first start (`device::warm_user`,
    /// 25-35 s) held the turn too, and an app's end waited behind it for its stop, a launch for its
    /// install (2026-10-19). The user is offered to launches once that start is done; a launch that
    /// comes meanwhile makes one of its own rather than wait. Never made while a launch on the
    /// device is under way or settling, nor right after its boot ([`State::wants_spare`]).
    fn spare_users(self: &Arc<Self>) {
        let wanted: Vec<(DeviceId, Device, Arc<Mutex<()>>)> = {
            let mut s = self.state.lock();
            let ids = s.wants_spare();
            ids.into_iter()
                .filter_map(|id| {
                    let d = s.devices.get_mut(&id)?;
                    d.sparing = true;
                    Some((id, d.device(), Arc::clone(&d.busy)))
                })
                .collect()
        };
        for (id, dev, busy) in wanted {
            let pool = Arc::clone(self);
            let _ = std::thread::spawn(move || {
                // Never beside a launch's install-and-start on the device: skipped if one holds it.
                let made = busy.try_lock().map(|_turn| device::create_user(&dev));
                if let Some(Ok(u)) = made {
                    // Its first start too, before a launch can take it (`device::warm_user`).
                    let warmed = device::warm_user(&dev, u);
                    if let Some(d) = pool.state.lock().devices.get_mut(&id) {
                        if !d.users.contains(&u) {
                            d.users.push(u);
                        }
                        d.user_made(u);
                    }
                    match warmed {
                        Ok(()) => pool.say(format!("D{id}: user {u} made and started once ahead for the next launch")),
                        Err(e) => pool.say(format!("D{id}: user {u} made ahead for the next launch (its first start failed: {e})")),
                    }
                }
                if let Some(d) = pool.state.lock().devices.get_mut(&id) {
                    d.sparing = false;
                }
            });
        }
    }

    /// **App processes whose death the devices' logs tell** (each second, the lines written since
    /// the last: cheap): the pids of each starting or running instance's main process learnt from
    /// the zygote's launches after its start (`[zygote] launching <package> as pid <P> uid <U>`, U
    /// its user's), and a running one whose pid died (ActivityManager's `Process <package> (pid <P>)
    /// has died`, the zygote's `pid <P> ended` or `: signal`) ended on its own at once -- even if
    /// Android starts it again. A crashing app whose `handleApplicationCrash` binder call failed
    /// (EBADF) kept its activity, and ActivityTaskManager restarted its process 0.5 s after its
    /// death when a display change came then: [`Pool::check_running`], every 5 s and two misses in
    /// a row, never saw it gone (2026-10-19). Ends are seen in ~1 s so, not 5-17 s; that check
    /// stays for the ends no process death tells (an activity finished, a process left cached).
    ///
    /// **A window of a running app's package gone** (`Input channel object '<window>
    /// <package>/..' was disposed`, no user named: every running instance of the package on the
    /// device) has that check read its activities each second for [`WINDOW_LOOK`]: Back finished
    /// an app's activity while its process lived, and with its process still listed, only the
    /// check's once-a-minute read of the activities saw it -- 72 s after, the window showing the
    /// app's last frame, then black, and `list_instances` saying `running` all that while
    /// (2026-10-20).
    ///
    /// A death read while it is still starting is forgotten, not its end: a user's start can launch
    /// the app for a boot broadcast, and a cookie's plant force-stops it before the app's own start
    /// (`extras::plant`); a start the app does not survive fails `am start` (`run_launch`).
    ///
    /// **Grants Android took back** ([`grants_reset`]: its data cleared, the package installed over
    /// or removed) are forgotten from the device's ([`DeviceRec::granted`]), whoever asked -- the
    /// MCP server's shell, the person in Settings.
    fn follow_logs(self: &Arc<Self>) {
        let logs: Vec<(DeviceId, PathBuf)> = {
            let s = self.state.lock();
            s.devices.iter().filter(|(_, d)| d.state == DeviceState::Ready).map(|(id, d)| (*id, d.device().log())).collect()
        };
        let mut ended = Vec::new();
        {
            let mut tails = self.tails.lock();
            tails.retain(|id, _| logs.iter().any(|(d, _)| d == id));
            for (device_id, log) in logs {
                let lines = tails.entry(device_id).or_insert_with(|| omni_warm::LogTail::from_end(&log)).lines();
                let events: Vec<ProcLine<'_>> = lines.iter().filter_map(|l| proc_line(l)).collect();
                let resets: Vec<(&str, Option<u32>)> = lines.iter().filter_map(|l| grants_reset(l)).collect();
                let mut s = self.state.lock();
                if let Some(d) = s.devices.get_mut(&device_id) {
                    for &(package, user) in &resets {
                        d.forget_grants(Some(package), user);
                    }
                }
                for (id, i) in s.instances.iter_mut().filter(|(_, i)| i.device == device_id && matches!(i.state, InstanceState::Starting | InstanceState::Running)) {
                    let mut died = false;
                    for e in &events {
                        match *e {
                            ProcLine::Launched { process, pid, user } => {
                                if i.started.is_some() && process == i.apk.package && i.user == Some(user) && !i.pids.contains(&pid) {
                                    i.pids.push(pid);
                                }
                            }
                            ProcLine::Died(pid) if i.pids.contains(&pid) => {
                                if i.state == InstanceState::Running {
                                    died = true;
                                } else {
                                    i.pids.retain(|p| *p != pid);
                                }
                            }
                            ProcLine::Died(_) => {}
                            ProcLine::WindowGone(package) => {
                                if i.state == InstanceState::Running && package == i.apk.package {
                                    i.window_went = Some(Instant::now());
                                }
                            }
                        }
                    }
                    // Taken out of the running here, under the lock (`take_ended`).
                    if died {
                        i.state = InstanceState::Closing;
                        ended.push(*id);
                    }
                }
            }
        }
        for id in ended {
            self.say(format!("#{id}: its app's process died (the device's log)"));
            let pool = Arc::clone(self);
            let _ = std::thread::Builder::new().name(format!("omni-pool-ended-{id}")).spawn(move || {
                std::thread::sleep(LOG_SETTLE);
                pool.ended_on_its_own(id, None);
            });
        }
    }

    /// Whether each running instance's app is still on its display: on a `round` (every 5 s) the
    /// devices running one, else those where one is [`looking`](InstanceRec::looking). The
    /// device's host says which app processes run (`device::apps`) and, on a `full` round (each
    /// minute), who draws on each instance's display (`device::display_owners`: the processes whose
    /// buffers its layers hold) -- no guest process started for either. When every instance's
    /// process is there -- and on a full round its app draws on its display -- that is taken as
    /// it ([`State::seen_running`], [`vouched`]). Else, as for an instance a window of whose
    /// package went lately ([`Pool::follow_logs`]) or with a miss pending, Android's activities are
    /// read (`dumpsys`, ~1.5-1.9 s of a shell, dumpsys and grep translated): an app whose activity
    /// left its display while its process stays cached (Back, the app's own exit: closed on
    /// purpose, `crash::diagnose`) is ended within seconds of its window's removal, and within
    /// about a minute when the log tells none -- its window's layers left the display with it, and
    /// the host's answer sends it to dumpsys. That read ran each minute on every device running an
    /// app, whatever the host knew, until 2026-10-20. Its process alone does not take a miss back:
    /// a quick round that reset it let such an app run on forever, its close never seen. An app
    /// not there twice in a row ended on its own ([`Pool::ended_on_its_own`]: its window says why,
    /// or closes).
    fn check_running(self: &Arc<Self>, round: bool, full: bool) {
        let devices: Vec<(DeviceId, Device)> = {
            let s = self.state.lock();
            s.devices.iter().filter(|(id, d)| d.state == DeviceState::Ready && s.live(**id).any(|(_, i)| i.state == InstanceState::Running && (round || i.looking()))).map(|(id, d)| (*id, d.device())).collect()
        };
        for (device_id, dev) in devices {
            if let Ok(apps) = device::apps(&dev) {
                let there = self.state.lock().seen_running(device_id, &apps);
                // The host asked off the pool's lock: a round trip on the device's control channel.
                if vouched(full, there.as_deref(), |hwc| device::display_owners(&dev, hwc).ok()) {
                    continue;
                }
            }
            let Ok(seen) = device::running(&dev) else { continue };
            let mut ended = Vec::new();
            {
                let mut s = self.state.lock();
                for (id, i) in s.instances.iter_mut().filter(|(_, i)| i.device == device_id && i.state == InstanceState::Running) {
                    let here = matches!((i.display, i.user), (Some(d), Some(u)) if seen.iter().any(|(sd, su, p)| *sd == d && *su == u && *p == i.apk.package));
                    i.missing = if here { 0 } else { i.missing + 1 };
                    if i.missing >= 2 {
                        // Taken out of the running here, under the lock (`take_ended`).
                        i.state = InstanceState::Closing;
                        ended.push(*id);
                    }
                }
            }
            // What happened read and shown off the ticker: the log since the start (megabytes, at
            // times), the card, the stop under the device's turn (behind an install, at times).
            for id in ended {
                let pool = Arc::clone(self);
                let _ = std::thread::Builder::new().name(format!("omni-pool-ended-{id}")).spawn(move || pool.ended_on_its_own(id, None));
            }
        }
    }
}

/// `pool.cold` beside the settings at `config_path`: there while the last device killed is the last
/// word, so the app opens with none.
fn cold_file(config_path: &Path) -> Option<PathBuf> {
    Some(config_path.parent()?.join("pool.cold"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool with no ticker and nothing adopted, its settings in `dir`, holding `n` ready devices
    /// (directories of nothing: stopping one writes a file there).
    fn pool(dir: &Path, n: u32) -> Arc<Pool> {
        let pool = Arc::new(Pool::new(Config::default(), Some(dir.join("app.conf")), PathBuf::from("omnidroid")));
        let mut s = pool.state.lock();
        for _ in 0..n {
            let id = s.next_device;
            s.next_device += 1;
            s.devices.insert(id, DeviceRec { state: DeviceState::Ready, idle_since: Some(Instant::now()), ..DeviceRec::booting(dir.join(format!("omni-warm-{id}"))) });
        }
        drop(s);
        pool
    }

    #[test]
    fn killing_the_last_android_keeps_the_app_cold_until_a_warm() {
        let dir = std::env::temp_dir().join(format!("omni-pool-cold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 2);
        p.kill(1).unwrap();
        assert!(!p.cold(), "another Android is up");
        p.kill(2).unwrap();
        assert!(p.cold(), "the last one killed");
        assert!(dir.join("pool.cold").exists(), "kept for the next time the app opens");
        assert_eq!(p.add_device(true), None, "the ticker warms none");
        assert!(pool(&dir, 0).cold(), "the app opens cold");
        // Not booted (the pool is quitting), but asked for: warm again.
        p.stopping.store(true, std::sync::atomic::Ordering::Release);
        p.warm();
        assert!(!p.cold());
        assert!(!dir.join("pool.cold").exists());
        assert!(!pool(&dir, 0).cold(), "the app opens warm again");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_card_says_what_happened_where_the_log_is_and_who_closes_it() {
        let d = Diagnosis { title: "com.a crashed".into(), summary: "Java exception: java.lang.IllegalStateException".into(), details: vec!["java.lang.IllegalStateException: no".into(), "  at com.a.Main.onCreate(Main.java:12)".into()], on_purpose: false };
        let text = card_text(&d, Path::new("/t/omni-warm-1.log")).with(None);
        assert_eq!(text, "com.a crashed\njava.lang.IllegalStateException: no\n  at com.a.Main.onCreate(Main.java:12)\n\u{1}restart\t\tClick here to restart\n\nThe whole log: /t/omni-warm-1.log\nClose this window when you are done.");
        assert_eq!(text.lines().next(), Some("com.a crashed"), "the first line is the card's title");
    }

    /// The restart line right under what happened, above the empty line and the footer; a countdown
    /// counts whole seconds up, `1 second` alone singular, never 0 -- once done, `now`, no link --
    /// and its card's last line says closing the window calls it off.
    #[test]
    fn a_card_s_restart_line_goes_under_what_happened_and_counts_down() {
        let d = Diagnosis { title: "com.a crashed".into(), summary: "native crash: SIGSEGV".into(), details: vec!["SIGSEGV at 0x8".into()], on_purpose: false };
        let card = card_text(&d, Path::new("/t/omni-warm-1.log"));
        let text = card.with(Some((true, RESTART_DELAY)));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..3], ["com.a crashed", "SIGSEGV at 0x8", "\u{1}restart\tAutomatically restarting in 5 seconds\tClick here to restart now"]);
        assert_eq!(lines[3..], ["", "The whole log: /t/omni-warm-1.log", "Close this window to cancel the restart."]);
        assert_eq!(countdown_line(false, Duration::from_millis(4001)), "\u{1}restart\tRestarting in 5 seconds\tClick here to restart now", "a part of a second counts as one");
        assert_eq!(countdown_line(false, Duration::from_secs(4)), "\u{1}restart\tRestarting in 4 seconds\tClick here to restart now");
        assert_eq!(countdown_line(false, Duration::from_millis(1500)), "\u{1}restart\tRestarting in 2 seconds\tClick here to restart now");
        assert_eq!(countdown_line(false, Duration::from_millis(300)), "\u{1}restart\tRestarting in 1 second\tClick here to restart now");
        assert_eq!(countdown_line(true, Duration::from_millis(1)), "\u{1}restart\tAutomatically restarting in 1 second\tClick here to restart now", "never 0");
        assert_eq!(countdown_line(true, Duration::ZERO), "\u{1}restart\tAutomatically restarting now\t", "done: the relaunch waits, nothing to click");
        assert_eq!(countdown_line(false, Duration::ZERO), "\u{1}restart\tRestarting now\t");
        let asked = asked_card("com.a").with(Some((false, RESTART_DELAY)));
        assert_eq!(asked, "com.a is restarting\nRestart asked.\n\u{1}restart\tRestarting in 5 seconds\tClick here to restart now\n\nClose this window to cancel the restart.");
        assert!(card.with(Some((true, Duration::ZERO))).ends_with("\nClose this window to cancel the restart."), "until the relaunch takes it");
        // Each field as `omni_linux::card::Card::parse` splits it: the control character and the
        // word, then the headline and the link split by a tab.
        let fields: Vec<&str> = RESTART_NOW.strip_prefix("\u{1}restart\t").unwrap().split('\t').collect();
        assert_eq!(fields, ["", "Click here to restart"], "no headline: the link alone");
    }

    #[test]
    fn three_quick_ends_in_a_row_stop_the_auto_restarts() {
        let apk = Arc::new(omni_warm::Apk { path: PathBuf::from("/a.apk"), sha256: "ff".into(), package: "com.a".into(), launcher: Some(".Main".into()), version_code: None, ..Default::default() });
        let mut i = InstanceRec::new(1, apk, Extras::default());
        i.started = Some(Instant::now());
        assert!(i.count_end() && i.count_end(), "one and two quick ends: restarted");
        assert!(!i.count_end(), "the third in a row: not");
        assert_eq!(i.quick_ends, QUICK_ENDS);
        i.quick_ends = 2;
        i.started = Instant::now().checked_sub(QUICK_END + Duration::from_secs(1));
        assert!(i.count_end(), "an end later than a minute after its start");
        assert_eq!(i.quick_ends, 0, "starts the count over");
        i.started = None;
        assert!(i.count_end() && i.quick_ends == 0, "a start that never began is no quick end");
    }

    #[test]
    fn a_device_s_log_tells_an_app_process_s_launch_and_death() {
        assert_eq!(proc_line("[zygote] launching com.a as pid 310000 uid 1010116: android.app.ActivityThread seq=50"), Some(ProcLine::Launched { process: "com.a", pid: 310_000, user: 10 }));
        assert_eq!(proc_line("12.345 [zygote] launching com.a:remote as pid 311000 uid 1110116: x"), Some(ProcLine::Launched { process: "com.a:remote", pid: 311_000, user: 11 }), "a time first");
        assert_eq!(proc_line("I/ActivityManager( 1210): Process com.roblox.client (pid 249000) has died: fg  TOP "), Some(ProcLine::Died(249_000)));
        assert_eq!(proc_line("[zygote] pid 249000 ended: Ok(ExitStatus(unix_wait_status(35584)))"), Some(ProcLine::Died(249_000)));
        assert_eq!(proc_line("[zygote] pid 310000: signal 9, its host process ended"), Some(ProcLine::Died(310_000)));
        assert_eq!(proc_line("E/AndroidRuntime( 6856): Process: com.a, PID: 310000"), None, "a crash's own line is not its death");
        assert_eq!(proc_line("[zygote] pid 310000 forked"), None);
        assert_eq!(proc_line("I/ActivityManager( 1210): Start proc 310000:com.a/u10a116 for activity"), None);
        assert_eq!(
            proc_line("W/InputManager-JNI( 1263): Input channel object '353bf6d com.outfit7.talkingtomcamp/com.outfit7.talkingtomcamp.MainActivity (client)' was disposed without first being removed with the input manager!"),
            Some(ProcLine::WindowGone("com.outfit7.talkingtomcamp"))
        );
        assert_eq!(proc_line("W/InputManager-JNI( 1263): Input channel object 'a1b2c3 PopupWindow:9f1e (client)' was disposed without first being removed with the input manager!"), None, "no activity's");
        assert_eq!(proc_line("W/InputManager-JNI( 1263): Input channel object 'a1b2c3 Splash Screen com.a/x (client)' was disposed"), None);
    }

    /// Back finishing a running app's activity, its process living on (2026-10-20): the device's
    /// log tells its window's removal, and its activities are read each second for a while --
    /// every running instance of the package there, the line naming no user; none other.
    #[test]
    fn a_window_of_a_running_app_gone_has_its_activities_read_each_second() {
        let dir = std::env::temp_dir().join(format!("omni-pool-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        let other = Arc::new(omni_warm::Apk { path: PathBuf::from("/b.apk"), sha256: "e".repeat(64), package: "com.b".into(), launcher: Some(".Main".into()), version_code: None, ..Default::default() });
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Running, Some(2)));
            s.instances.insert(2, InstanceRec { user: Some(11), ..instance(1, InstanceState::Running, Some(5)) });
            s.instances.insert(3, InstanceRec { apk: other, user: Some(12), ..instance(1, InstanceState::Running, Some(7)) });
            s.instances.insert(4, InstanceRec { user: Some(13), ..instance(1, InstanceState::Starting, Some(8)) });
        }
        let log = p.device(1).unwrap().log();
        std::fs::write(&log, "boot\n").unwrap();
        p.follow_logs();
        let looking = |id| p.state.lock().instances[&id].looking();
        assert!(!looking(1), "nothing went yet");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        std::io::Write::write_all(&mut f, b"W/InputManager-JNI( 1263): Input channel object '353bf6d com.a/com.a.Main (client)' was disposed without first being removed with the input manager!\n").unwrap();
        p.follow_logs();
        assert!(looking(1) && looking(2), "every running com.a there");
        assert!(!looking(3) && !looking(4), "another package's, one not running yet");
        assert_eq!(p.state.lock().instances[&1].state, InstanceState::Running, "a window gone is no end by itself: Android's activities say");
        p.state.lock().instances.get_mut(&1).unwrap().window_went = Instant::now().checked_sub(WINDOW_LOOK);
        assert!(!looking(1), "for a while only");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A device's grants are forgotten when Android takes them back -- the package's data cleared,
    /// the package installed over (every user) -- and not for a force-stop a process asked for: the
    /// pool's own stops, at every close and restart.
    #[test]
    fn grants_android_took_back_are_forgotten() {
        assert_eq!(grants_reset("I/ActivityManager( 1210): Force stopping com.a appid=10116 user=10: clear data"), Some(("com.a", Some(10))));
        assert_eq!(grants_reset("I/ActivityManager( 1210): Force stopping com.a appid=10116 user=-1: installPackageLI"), Some(("com.a", None)), "every user");
        assert_eq!(grants_reset("I/ActivityManager( 1210): Force stopping com.a appid=10116 user=10: from pid 4321"), None, "asked: `am force-stop`");
        assert_eq!(grants_reset("I/ActivityManager( 1210): Force stopping u10: finish user"), None, "a user's stop");
        let dir = std::env::temp_dir().join(format!("omni-pool-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        let granted = || p.state.lock().devices[&1].granted.iter().map(|(a, u)| format!("{a}/{u}")).collect::<Vec<_>>();
        {
            let mut s = p.state.lock();
            let d = s.devices.get_mut(&1).unwrap();
            for g in [("com.a", 10), ("com.a", 11), ("com.b", 10), ("com.b", 12)] {
                d.granted.insert((g.0.to_string(), g.1));
            }
        }
        let log = p.device(1).unwrap().log();
        std::fs::write(&log, "boot\n").unwrap();
        p.follow_logs();
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        std::io::Write::write_all(&mut f, b"I/ActivityManager( 1210): Force stopping com.a appid=10116 user=10: from pid 4321\nI/ActivityManager( 1210): Force stopping com.a appid=10116 user=11: clear data\n").unwrap();
        p.follow_logs();
        assert_eq!(granted(), ["com.a/10", "com.b/10", "com.b/12"], "its data cleared for user 11 alone");
        std::io::Write::write_all(&mut f, b"I/ActivityManager( 1210): Force stopping com.b appid=10117 user=-1: deletePackageX\n").unwrap();
        p.follow_logs();
        assert_eq!(granted(), ["com.a/10"], "removed: for every user");
        // A user made: its id's grants go (a removed user's id given again).
        p.state.lock().devices.get_mut(&1).unwrap().user_made(10);
        assert!(granted().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A start keeps its grants only when it ran them from the package's whole dump -- not one that
    /// skipped them, nor one whose dump came back empty, nor a failed one; an install of the package
    /// forgets them for every user, a user made for its id.
    #[test]
    fn a_start_keeps_the_grants_it_ran() {
        let mut d = DeviceRec::booting(PathBuf::from("unused"));
        let ok = |done: bool| Ok::<_, String>((String::new(), done));
        d.started("com.a", 10, &ok(false));
        assert!(!d.grants_known("com.a", 10), "skipped, or its dump came back empty");
        d.started("com.a", 10, &Err("am start: Error".into()));
        assert!(!d.grants_known("com.a", 10), "failed");
        for (package, user) in [("com.a", 10), ("com.a", 11), ("com.b", 10)] {
            d.started(package, user, &ok(true));
        }
        assert!(d.grants_known("com.a", 10) && d.grants_known("com.a", 11) && !d.grants_known("com.a", 12));
        d.installing("com.a");
        assert!(!d.grants_known("com.a", 10) && !d.grants_known("com.a", 11) && d.grants_known("com.b", 10), "a build of com.a installed over");
        d.user_made(10);
        assert!(!d.grants_known("com.b", 10));
    }

    /// No user is made ahead on a device while a launch there is under way (a restart's, from its
    /// end on) or reached `Running` less than a minute ago, nor in the half minute after the device
    /// came up.
    #[test]
    fn no_spare_user_is_made_beside_a_launch_or_right_after_a_boot() {
        let dir = std::env::temp_dir().join(format!("omni-pool-spare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 5);
        let long_ago = Instant::now().checked_sub(SPARE_AFTER_START + Duration::from_secs(1));
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Starting, Some(2)));
            s.instances.insert(2, InstanceRec { running_since: Some(Instant::now()), ..instance(2, InstanceState::Running, Some(2)) });
            s.instances.insert(3, InstanceRec { running_since: long_ago, ..instance(3, InstanceState::Running, Some(2)) });
            s.devices.get_mut(&4).unwrap().ready_at = Some(Instant::now());
            s.devices.get_mut(&5).unwrap().users = vec![10];
        }
        assert_eq!(p.state.lock().wants_spare(), [3], "D1 starting, D2 just running, D4 just up, D5 has a user free");
        {
            let mut s = p.state.lock();
            s.instances.get_mut(&2).unwrap().running_since = long_ago;
            s.devices.get_mut(&4).unwrap().ready_at = Instant::now().checked_sub(SPARE_AFTER_READY);
        }
        for (state, why) in [(InstanceState::Closing, "D1's end being read"), (InstanceState::Restarting, "D1's restart counting down, its user kept")] {
            p.state.lock().instances.get_mut(&1).unwrap().state = state;
            assert_eq!(p.state.lock().wants_spare(), [2, 3, 4], "{why}");
        }
        p.state.lock().instances.get_mut(&1).unwrap().state = InstanceState::Ended("crashed".into());
        assert_eq!(p.state.lock().wants_spare(), [1, 2, 3, 4]);
        p.state.lock().devices.get_mut(&3).unwrap().sparing = true;
        assert_eq!(p.state.lock().wants_spare(), [1, 2, 4], "one made there already");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The minute's check reads Android's activities only when the device's host cannot vouch for
    /// every running instance: its process there, no miss pending, no window of it gone lately --
    /// and its app drawing on its display.
    #[test]
    fn the_host_vouches_for_an_app_drawing_on_its_display_or_dumpsys_is_read() {
        let dir = std::env::temp_dir().join(format!("omni-pool-seen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Running, Some(2)));
            s.instances.insert(2, InstanceRec { user: Some(11), ..instance(1, InstanceState::Running, Some(5)) });
            s.instances.insert(3, InstanceRec { user: Some(12), ..instance(1, InstanceState::Starting, Some(7)) });
        }
        let apps = vec![(1_010_116, "com.a".to_string()), (1_110_116, "com.a:remote".to_string()), (1_110_116, "com.a".to_string()), (10_045, "com.android.settings".to_string())];
        let seen = p.state.lock().seen_running(1, &apps);
        assert_eq!(seen, Some(vec![(Some(2), 1_010_116), (Some(5), 1_110_116)]), "each running one's display and app uid; not one starting");
        assert_eq!(p.state.lock().seen_running(1, &apps[..1]), None, "#2's process not there");
        p.state.lock().instances.get_mut(&2).unwrap().missing = 1;
        assert_eq!(p.state.lock().seen_running(1, &apps), None, "a miss pending");
        p.state.lock().instances.get_mut(&2).unwrap().missing = 0;
        p.state.lock().instances.get_mut(&1).unwrap().window_went = Some(Instant::now());
        assert_eq!(p.state.lock().seen_running(1, &apps), None, "a window of it gone lately");
        let seen = seen.unwrap();
        let owners = |hwc: i64| match hwc {
            2 => Some(vec![(310_000, 1_010_116), (1210, 1000)]),
            5 => Some(vec![(320_000, 1_110_116)]),
            _ => None,
        };
        assert!(all_drawn(&seen, owners));
        assert!(!all_drawn(&seen, |hwc| owners(hwc).filter(|_| hwc != 5)), "the host could not say for #2's display");
        assert!(!all_drawn(&seen, |hwc| Some(if hwc == 2 { vec![(1210, 1000)] } else { owners(hwc).unwrap() })), "only the system draws on #1's: its window left");
        assert!(!all_drawn(&[(None, 1_010_116)], owners), "no display");
        assert!(all_drawn(&[], owners), "nothing running");
        // The round's word: a quick round takes the processes and never asks who draws; a full
        // one asks, and reads dumpsys unless every app draws on its display.
        let never = |_: i64| -> Option<Vec<(i32, u32)>> { panic!("who draws asked on a quick round") };
        assert!(vouched(false, Some(&seen[..]), never));
        assert!(!vouched(false, None, never) && !vouched(true, None, never), "a process not there, or a miss pending");
        assert!(vouched(true, Some(&seen[..]), owners));
        assert!(!vouched(true, Some(&seen[..]), |hwc| owners(hwc).filter(|_| hwc != 5)), "the host could not say for #2's display");
        assert!(!vouched(true, Some(&seen[..]), |hwc| Some(if hwc == 2 { vec![(1210, 1000)] } else { owners(hwc).unwrap() })), "#1's window left");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_start_says_its_error_and_what_the_log_found() {
        let crash = || Diagnosis { title: "com.a crashed".into(), summary: "native crash: SIGSEGV".into(), details: vec!["SIGSEGV at 0x8 in libgame.so".into()], on_purpose: false };
        let error = "am start: Starting: Intent { cmp=com.a/.Main }\nStatus: timeout\nComplete\n";
        let d = did_not_start("com.a", error, crash(), true);
        assert_eq!(d.title, "com.a did not start");
        assert_eq!(d.summary, "did not start: native crash: SIGSEGV");
        assert_eq!(d.details, ["The start failed: am start: Starting: Intent { cmp=com.a/.Main }", "Status: timeout", "Complete", "", "SIGSEGV at 0x8 in libgame.so"]);
        let d = did_not_start("com.a", error, crash(), false);
        assert_eq!(d.summary, "start: am start: Starting: Intent { cmp=com.a/.Main }; Status: timeout; Complete", "one line");
        assert_eq!(d.details, ["The start failed: am start: Starting: Intent { cmp=com.a/.Main }", "Status: timeout", "Complete"], "nothing the log did not say");
    }

    #[test]
    fn the_log_since_a_start_is_read_across_its_rotation() {
        let root = std::env::temp_dir().join(format!("omni-pool-logmark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dev = Device { dir: root.join("omni-warm-1") };
        let append = |path: &Path, text: &str| {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
        };
        append(&dev.log(), "boot\n");
        let mark = LogMark::new(&dev);
        assert!(mark.lines().is_empty(), "what came before the start is not read");
        append(&dev.log(), "one\ntwo\n");
        assert_eq!(mark.lines(), ["one", "two"]);
        // The first boot kept: its log renamed, the device booted again into a new one, longer than
        // the mark (so not taken for a log started again).
        std::fs::rename(dev.log(), dev.dir.with_extension("1.log")).unwrap();
        append(&dev.log(), "again\nthree\nfour\n");
        assert_eq!(mark.lines(), ["one", "two", "again", "three", "four"]);
        // A mark taken after the rotation: the old log is no longer news.
        let later = LogMark::new(&dev);
        append(&dev.log(), "five\n");
        assert_eq!(later.lines(), ["five"]);
        // A log started again, with no rotation: read from its beginning.
        std::fs::write(dev.log(), "new\n").unwrap();
        assert_eq!(later.lines(), ["new"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_long_log_is_read_by_its_two_ends() {
        let root = std::env::temp_dir().join(format!("omni-pool-longlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (a, b) = (root.join("a.log"), root.join("b.log"));
        std::fs::write(&a, "skip\nstart proc\nline 2\nline 3\n").unwrap();
        std::fs::write(&b, "line 4\nline 5\nline 6\nfatal\n").unwrap();
        // From the mark (after "skip") in the first, all of the second: 25 + 27 bytes.
        let segments = [(a.clone(), 5, 30), (b.clone(), 0, 27)];
        assert_eq!(read_lines(&segments, 1000, 10), ["start proc", "line 2", "line 3", "line 4", "line 5", "line 6", "fatal"], "under the limit: all of it, across both files");
        // 30 of the 52: the first 14 ("start proc\nlin") and the last 16 ("5\nline 6\nfatal\n"),
        // each cut at a line's end.
        assert_eq!(read_lines(&segments, 30, 14), ["start proc", "[omnidroid: 0.0 MB of the log left out here]", "line 6", "fatal"]);
        assert!(read_lines(&[(root.join("none.log"), 0, 10)], 1000, 10).is_empty(), "a file not there gives nothing");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_ended_instance_s_card_goes_when_it_is_closed_cleared_or_its_device_goes() {
        let dir = std::env::temp_dir().join(format!("omni-pool-cards-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 2);
        let apk = Arc::new(omni_warm::Apk { path: dir.join("a.apk"), sha256: "ff".into(), package: "com.a".into(), launcher: Some(".Main".into()), version_code: None, ..Default::default() });
        let rec = |device, hwc| InstanceRec { user: Some(10), hwc: Some(hwc), display: Some(3), state: InstanceState::Ended("native crash: SIGSEGV".into()), details: vec!["SIGSEGV at 0x8".into()], ..InstanceRec::new(device, Arc::clone(&apk), Extras::default()) };
        {
            let mut s = p.state.lock();
            s.instances.insert(1, rec(1, 2));
            s.instances.insert(2, rec(2, 5));
            s.instances.insert(3, rec(1, 7));
        }
        let (_, instances) = p.snapshot();
        assert_eq!(instances[0].details, ["SIGSEGV at 0x8"], "what happened, for the screen and the MCP server");
        assert!(p.state.lock().live(1).next().is_none(), "an ended instance takes no user or place on its device, its card shown or not");
        // But its card keeps the device from the idle close: its stop would close the window.
        p.mark_idle();
        assert!(p.shows_cards() && p.state.lock().devices[&1].idle_since.is_none(), "a device showing a card is not idle");
        // Its device stops: its window went with it.
        p.stop_device(2, false).unwrap();
        assert_eq!(p.state.lock().instances[&2].hwc, None);
        // Closed (the screen's close, `stop_instance`): its card's window closes (the device answers
        // nothing here: its marker goes), and once gone there is nothing left to close.
        let dev = p.device(1).unwrap();
        std::fs::create_dir_all(dev.ctl()).unwrap();
        let gone = |hwc| {
            let t = Instant::now();
            while closed_marker(&dev, hwc).exists() && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            !closed_marker(&dev, hwc).exists()
        };
        std::fs::write(closed_marker(&dev, 7), b"closed").unwrap();
        p.close(3).unwrap();
        assert_eq!(p.state.lock().instances[&3].hwc, None);
        assert!(gone(7), "the card's display and its marker removed");
        let shown = |id| p.snapshot().1.into_iter().find(|i| i.id == id).map(|i| (i.display, i.screenshot.is_some()));
        assert_eq!((shown(1), shown(3)), (Some((Some(3), true)), Some((None, false))), "a display gone is named no more");
        assert_eq!(p.close(3), Err("#3 ended already".to_string()));
        // Forgotten: its card's window closes, and the device is idle again.
        std::fs::write(closed_marker(&dev, 2), b"closed").unwrap();
        p.clear_ended();
        assert!(p.state.lock().instances.is_empty());
        assert!(gone(2), "the card's display and its marker removed");
        p.mark_idle();
        assert!(!p.shows_cards() && p.state.lock().devices[&1].idle_since.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An instance of `com.a` on `device`, in `state`, as user 10 on display `hwc`.
    fn instance(device: DeviceId, state: InstanceState, hwc: Option<i64>) -> InstanceRec {
        let apk = Arc::new(omni_warm::Apk { path: PathBuf::from("/a.apk"), sha256: "f".repeat(64), package: "com.a".into(), launcher: Some(".Main".into()), version_code: None, ..Default::default() });
        InstanceRec { user: Some(10), hwc, display: hwc.map(|_| 3), state, ..InstanceRec::new(device, apk, Extras::default()) }
    }

    /// Until `done` says so, at most 5 s.
    fn wait_for(done: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while !done() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    #[test]
    fn a_restart_is_refused_on_the_way_up_and_a_newer_one_or_a_close_replaces_its_countdown() {
        let dir = std::env::temp_dir().join(format!("omni-pool-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Waiting, None));
            s.instances.insert(2, instance(1, InstanceState::Starting, Some(2)));
            s.instances.insert(3, InstanceRec { quick_ends: 2, closed: true, ..instance(1, InstanceState::Ended("closed".into()), None) });
            s.instances.insert(4, instance(1, InstanceState::Closing, Some(4)));
        }
        assert_eq!(p.restart(1, Duration::ZERO), Err("#1 is still starting".to_string()));
        assert_eq!(p.restart(2, Duration::ZERO), Err("#2 is still starting".to_string()));
        assert_eq!(p.restart(4, Duration::ZERO), Err("#4 is closing".to_string()));
        assert_eq!(p.restart(9, Duration::ZERO), Err("no instance #9".to_string()));
        // One that ended, its window gone (closed): restarting, its countdown shown; its quick ends
        // forgotten, its close too.
        p.restart(3, Duration::from_secs(3600)).unwrap();
        let info = |id| p.snapshot().1.into_iter().find(|i| i.id == id).unwrap();
        assert_eq!(info(3).state, InstanceState::Restarting);
        assert!(info(3).restart_in.is_some_and(|t| t > Duration::from_secs(3590)));
        assert!(p.state.lock().instances[&3].holds_device(), "a countdown keeps its device from the idle close");
        let first = p.state.lock().instances[&3].countdown.unwrap().id;
        let forgot = p.state.lock().instances.get(&3).map(|i| (i.quick_ends, i.closed));
        assert_eq!(forgot, Some((0, false)));
        // Again: the new countdown replaces the old.
        p.restart(3, Duration::from_secs(1800)).unwrap();
        let second = p.state.lock().instances[&3].countdown.unwrap();
        assert!(second.id != first && !second.auto);
        assert!(info(3).restart_in.is_some_and(|t| t <= Duration::from_secs(1800)));
        // A close calls it off, and ends it as closed.
        p.close(3).unwrap();
        assert!(wait_for(|| info(3).state == InstanceState::Ended("closed".into())));
        assert_eq!(info(3).restart_in, None);
        // Nothing restarts once the app quits.
        p.stopping.store(true, std::sync::atomic::Ordering::Release);
        assert!(p.restart(3, Duration::ZERO).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_restart_makes_its_launch_s_extras_again_and_says_when_it_cannot() {
        use crate::extras::{Remake, TempFile};
        let dir = std::env::temp_dir().join(format!("omni-pool-remake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        let store = dir.join("cookies.db");
        let made = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let remake: Remake = {
            let (store, made) = (store.clone(), Arc::clone(&made));
            Arc::new(move || {
                made.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::fs::write(&store, b"a session").map_err(|e| e.to_string())?;
                Ok(Extras { cookie_store: Some(Arc::new(TempFile::new(store.clone()))), ..Extras::default() })
            })
        };
        let failing: Remake = Arc::new(|| Err("no such account".to_string()));
        {
            let mut s = p.state.lock();
            s.instances.insert(1, InstanceRec { extras: Extras { remake: Some(remake), ..Extras::default() }, ..instance(1, InstanceState::Ended("crashed".into()), None) });
            s.instances.insert(2, InstanceRec { extras: Extras { remake: Some(failing), ..Extras::default() }, ..instance(1, InstanceState::Ended("crashed".into()), None) });
        }
        let info = |id| p.snapshot().1.into_iter().find(|i| i.id == id).unwrap();
        // The plugin could not make it again: ended, saying why.
        p.restart(2, Duration::ZERO).unwrap();
        assert!(wait_for(|| info(2).state == InstanceState::Ended("restart: its launch could not be made again: no such account".into())));
        assert_eq!(info(2).restarts, 1);
        // Made again: a launch's work with the new cookie store (the device here takes no install),
        // which goes with the start; the instance keeps the rest, and what makes it again.
        p.restart(1, Duration::ZERO).unwrap();
        assert!(wait_for(|| matches!(&info(1).state, InstanceState::Ended(why) if why.starts_with("install: "))), "{:?}", info(1).state);
        assert!(wait_for(|| !store.exists()), "the cookie store goes with the start");
        assert_eq!((made.load(std::sync::atomic::Ordering::Relaxed), info(1).restarts), (1, 1));
        let kept = p.state.lock().instances.get(&1).map(|i| (i.extras.cookie_store.is_none(), i.extras.remake.is_some()));
        assert_eq!(kept, Some((true, true)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_instance_s_own_auto_restart_overrides_the_pool_s() {
        let dir = std::env::temp_dir().join(format!("omni-pool-auto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Running, Some(2)));
            s.instances.insert(2, instance(1, InstanceState::Running, Some(5)));
        }
        let auto = |id| p.snapshot().1.into_iter().find(|i| i.id == id).unwrap().auto_restart;
        assert!(!auto(1) && !auto(2), "off by default");
        p.set_auto_restart(None, true).unwrap();
        assert!(p.config().auto_restart && auto(1) && auto(2), "the pool's, for every instance without its own");
        let (kept, _) = Config::load(&dir.join("app.conf"));
        assert!(kept.auto_restart, "kept as a setting");
        p.set_auto_restart(Some(1), false).unwrap();
        assert!(!auto(1) && auto(2), "its own over the pool's");
        p.set_auto_restart(None, false).unwrap();
        p.set_auto_restart(Some(2), true).unwrap();
        assert!(!auto(1) && auto(2), "on for it alone");
        assert_eq!(p.set_auto_restart(Some(9), true), Err("no instance #9".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_crash_restarts_with_auto_restart_on_until_three_quick_ends() {
        let dir = std::env::temp_dir().join(format!("omni-pool-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        p.set_auto_restart(None, true).unwrap();
        {
            let mut s = p.state.lock();
            s.instances.insert(1, InstanceRec { started: Some(Instant::now()), ..instance(1, InstanceState::Running, Some(2)) });
            s.instances.insert(2, InstanceRec { started: Some(Instant::now()), quick_ends: QUICK_ENDS - 1, ..instance(1, InstanceState::Running, Some(5)) });
            s.instances.insert(3, InstanceRec { auto_restart: Some(false), ..instance(1, InstanceState::Running, Some(7)) });
        }
        let info = |id| p.snapshot().1.into_iter().find(|i| i.id == id).unwrap();
        for id in [1, 2, 3] {
            assert!(p.take_ended(id, &InstanceState::Running));
            p.ended_on_its_own(id, None);
        }
        // Restarting, its countdown on, what happened still told.
        assert_eq!(info(1).state, InstanceState::Restarting);
        assert!(info(1).restart_in.is_some_and(|t| t <= RESTART_DELAY));
        assert!(p.state.lock().instances[&1].countdown.is_some_and(|c| c.auto));
        assert!(!info(1).details.is_empty(), "what happened, for list_instances");
        // The third quick end in a row: ended, its card says why it did not restart.
        assert!(matches!(info(2).state, InstanceState::Ended(_)));
        assert_eq!(info(2).details.last().map(String::as_str), Some(not_restarted().as_str()));
        assert!(p.state.lock().instances[&2].card.as_ref().is_some_and(|c| c.lines.contains(&not_restarted())));
        // Its own setting off: ended, its card offering a restart.
        assert!(matches!(info(3).state, InstanceState::Ended(_)));
        assert!(!info(3).details.contains(&not_restarted()));
        // A close during the countdown: called off, no card, ended as closed.
        p.close(1).unwrap();
        assert!(wait_for(|| info(1).state == InstanceState::Ended("closed".into())));
        // A close that came before its end was read: that close ends it, nothing else.
        p.state.lock().instances.insert(4, instance(1, InstanceState::Running, Some(9)));
        assert!(p.take_ended(4, &InstanceState::Running));
        p.state.lock().instances.get_mut(&4).unwrap().closed = true;
        p.ended_on_its_own(4, None);
        assert_eq!(info(4).state, InstanceState::Closing, "the close's end sets it");
        assert!(p.state.lock().instances[&4].card.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_device_that_dies_on_its_own_relaunches_what_restarts_and_a_kill_ends_all() {
        let dir = std::env::temp_dir().join(format!("omni-pool-died-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 2);
        p.set_auto_restart(None, true).unwrap();
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Running, Some(2)));
            s.instances.insert(2, InstanceRec { auto_restart: Some(false), ..instance(1, InstanceState::Running, Some(5)) });
            s.instances.insert(3, InstanceRec { countdown: Some(Countdown { at: Instant::now(), auto: false, id: 7 }), ..instance(1, InstanceState::Restarting, Some(7)) });
            s.instances.insert(4, instance(1, InstanceState::Starting, Some(8)));
            s.instances.insert(5, instance(2, InstanceState::Running, Some(2)));
            s.instances.insert(6, InstanceRec { countdown: Some(Countdown { at: Instant::now(), auto: false, id: 8 }), ..instance(2, InstanceState::Restarting, Some(5)) });
        }
        let again = p.device_gone(1, "ended", true);
        let s = p.state.lock();
        assert_eq!(again.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1], "a countdown for the one running with auto restart on");
        assert_eq!((&s.instances[&1].state, s.instances[&1].hwc, s.instances[&1].user), (&InstanceState::Restarting, None, None), "placed anew: no display, no user");
        assert!(matches!(s.instances[&2].state, InstanceState::Ended(_)), "auto restart off");
        assert_eq!((&s.instances[&3].state, s.instances[&3].hwc), (&InstanceState::Restarting, None), "restarting already: its countdown goes on");
        assert!(matches!(s.instances[&4].state, InstanceState::Ended(_)), "on its way up: its launch's thread ends it");
        drop(s);
        // A kill ends all, its countdowns too.
        assert!(p.device_gone(2, "stopped", false).is_empty());
        let s = p.state.lock();
        assert!(matches!(s.instances[&5].state, InstanceState::Ended(_)) && matches!(s.instances[&6].state, InstanceState::Ended(_)));
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_user_whose_stop_is_under_way_is_no_launch_s_and_clear_calls_off_countdowns() {
        let dir = std::env::temp_dir().join(format!("omni-pool-halting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, InstanceRec { halting: 1, user: Some(11), ..instance(1, InstanceState::Ended("crashed".into()), None) });
            s.instances.insert(2, InstanceRec { user: Some(12), ..instance(1, InstanceState::Ended("crashed".into()), None) });
            s.instances.insert(3, InstanceRec { user: Some(13), countdown: Some(Countdown { at: Instant::now(), auto: true, id: 1 }), ..instance(1, InstanceState::Restarting, None) });
            assert_eq!(s.users_taken(1, None), [11, 13], "ended but still being stopped, and restarting");
            assert_eq!(s.users_taken(1, Some(3)), [11], "its own is its relaunch's");
        }
        p.clear_ended();
        let s = p.state.lock();
        assert_eq!(s.instances.keys().copied().collect::<Vec<_>>(), [1], "the one whose stop is under way stays until it is done");
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An ended instance's user is free, and another launch may take it: a restart gives it up then
    /// (kept, a close during the countdown stopped the other's app and user), and keeps a free one
    /// (its app's data is there).
    #[test]
    fn a_restart_of_an_ended_instance_gives_up_a_user_another_took() {
        let dir = std::env::temp_dir().join(format!("omni-pool-reuser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Ended("crashed".into()), None));
            s.instances.insert(2, instance(1, InstanceState::Running, Some(5)));
            s.instances.insert(3, InstanceRec { user: Some(12), ..instance(1, InstanceState::Ended("crashed".into()), None) });
            s.instances.insert(4, InstanceRec { user: Some(13), details: vec!["an earlier crash".into()], ..instance(1, InstanceState::Running, Some(7)) });
        }
        for id in [1, 3, 4] {
            p.restart(id, Duration::from_secs(3600)).unwrap();
        }
        let s = p.state.lock();
        assert_eq!((s.instances[&1].user, s.instances[&2].user), (None, Some(10)), "#2's now");
        assert_eq!(s.instances[&3].user, Some(12), "free: its own still");
        assert!(s.instances[&4].details.is_empty(), "restarted while it ran: nothing ended it");
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `plan`'s rule 4 holds for a relaunch: another build of the package running on its device
    /// since it ended, it goes to another; a build of its package there that runs nowhere is
    /// replaced, the relaunch's the device's from then on.
    #[test]
    fn a_relaunch_never_replaces_another_build_running_on_its_device() {
        let dir = std::env::temp_dir().join(format!("omni-pool-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 2);
        let build = |package: &str, sha: &str| Arc::new(omni_warm::Apk { path: dir.join("a.apk"), sha256: sha.repeat(64), package: package.into(), launcher: Some(".Main".into()), version_code: None, ..Default::default() });
        {
            let mut s = p.state.lock();
            let d1 = s.devices.get_mut(&1).unwrap();
            d1.installed.insert("com.a".into(), "b".repeat(64));
            d1.installed.insert("com.b".into(), "d".repeat(64));
            s.instances.insert(1, InstanceRec { apk: build("com.a", "a"), ..instance(1, InstanceState::Ended("crashed".into()), None) });
            s.instances.insert(2, InstanceRec { apk: build("com.a", "b"), user: Some(11), ..instance(1, InstanceState::Running, Some(5)) });
            s.instances.insert(3, InstanceRec { apk: build("com.b", "c"), user: Some(12), ..instance(1, InstanceState::Ended("crashed".into()), None) });
        }
        p.restart(1, Duration::ZERO).unwrap();
        p.restart(3, Duration::ZERO).unwrap();
        let info = |id| p.snapshot().1.into_iter().find(|i| i.id == id).unwrap();
        // The devices here take no install: each relaunch's work ends there.
        for id in [1, 3] {
            assert!(wait_for(|| matches!(&info(id).state, InstanceState::Ended(why) if why.starts_with("install: "))), "#{id}: {:?}", info(id).state);
        }
        assert_eq!((info(1).device, info(3).device), (2, 1), "#1 placed anew; #3 on its device");
        let s = p.state.lock();
        assert_eq!(s.devices[&1].installed["com.a"], "b".repeat(64), "the running build's still");
        assert_eq!(s.devices[&2].installed["com.a"], "a".repeat(64));
        assert_eq!(s.devices[&1].installed["com.b"], "c".repeat(64), "replaced by the relaunch's");
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A kill ends its device's restarts at once, and a countdown that ran out while its device
    /// stops starts nothing elsewhere: that stop ends it.
    #[test]
    fn a_restart_never_escapes_its_device_s_kill() {
        let dir = std::env::temp_dir().join(format!("omni-pool-escape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 2);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, InstanceRec { countdown: Some(Countdown { at: Instant::now() + RESTART_DELAY, auto: true, id: 1 }), ..instance(1, InstanceState::Restarting, Some(2)) });
            s.instances.insert(2, InstanceRec { countdown: Some(Countdown { at: Instant::now(), auto: true, id: 2 }), details: vec!["boom".into()], ..instance(2, InstanceState::Restarting, Some(5)) });
        }
        p.kill(1).unwrap();
        let info = |id| p.snapshot().1.into_iter().find(|i| i.id == id).unwrap();
        assert_eq!((info(1).state, info(1).restart_in), (InstanceState::Ended("its device D1 stopped".into()), None));
        // Its device closing as its countdown ends: still on it, waiting for that stop's end.
        p.state.lock().devices.get_mut(&2).unwrap().state = DeviceState::Closing;
        Arc::clone(&p).relaunch(2, 2);
        assert_eq!((info(2).state, info(2).device), (InstanceState::Waiting, 2));
        assert_eq!(p.state.lock().devices.len(), 1, "no device booted for it");
        assert_eq!(info(2).details, ["boom"], "what its last end said, kept through the relaunch");
        p.device_gone(2, "stopped", false);
        assert_eq!(info(2).state, InstanceState::Ended("its device D2 stopped".into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A restart that fails before its start, on the display it kept, says so on its card (a click
    /// tries again); a launch's failure ends it, nothing on its card.
    #[test]
    fn a_restart_that_fails_before_its_start_says_so_on_its_card() {
        let dir = std::env::temp_dir().join(format!("omni-pool-notstarted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        {
            let mut s = p.state.lock();
            s.instances.insert(1, instance(1, InstanceState::Installing, Some(2)));
            s.instances.insert(2, instance(1, InstanceState::Starting, Some(5)));
            s.instances.insert(3, InstanceRec { closed: true, ..instance(1, InstanceState::Closing, Some(7)) });
        }
        p.not_started(1, "install: no space", true);
        p.not_started(2, "its cookie store: gone", false);
        p.not_started(3, "install: no space", true);
        let s = p.state.lock();
        let one = &s.instances[&1];
        assert_eq!((&one.state, &one.details), (&InstanceState::Ended("install: no space".into()), &vec!["install: no space".to_string()]));
        assert!(one.card.as_ref().is_some_and(|c| c.title == "com.a did not restart" && c.lines == ["install: no space"]), "{:?}", one.card);
        assert_eq!((&s.instances[&2].state, s.instances[&2].card.is_none()), (&InstanceState::Ended("its cookie store: gone".into()), true));
        assert_eq!(s.instances[&3].state, InstanceState::Closing, "a close came first: its end does the rest");
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_idle_close_does_not_make_the_app_cold() {
        let dir = std::env::temp_dir().join(format!("omni-pool-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = pool(&dir, 1);
        p.stop_device(1, false).unwrap();
        assert!(!p.cold());
        assert!(!dir.join("pool.cold").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
