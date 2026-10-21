//! **The app's screen**: the Androids and the instances, the log, and a command line -- full-screen
//! in the terminal (ratatui over crossterm: the Windows console and Unix terminals alike). It is a
//! window on the pool's host ([`super::host`]): what it shows is the host's, what is typed is done
//! there, and other windows open at the same time show the same.
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use omni_pool::{DeviceInfo, DeviceState, InstanceInfo, InstanceState};

use super::command::{self, Command};
use super::host::{Client, View};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Devices,
    Instances,
}

/// What the screen holds between frames.
pub struct App {
    pub client: Client,
    /// The pool as last heard.
    view: View,
    /// The host stopped answering: since when.
    lost: Option<Instant>,
    /// The settings' `stop_on_exit` when this window quit.
    pub stop_on_exit: bool,
    input: String,
    history: Vec<String>,
    history_at: Option<usize>,
    focus: Focus,
    devices: TableState,
    instances: TableState,
    /// A first Ctrl+C / `quit` with instances running: the next one quits.
    quit_asked: Option<Instant>,
    quit: bool,
}

/// `1h02m`, `3m05s`, `12s`.
fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s / 60 % 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn device_state(s: &DeviceState) -> (String, Color) {
    match s {
        DeviceState::Booting => ("booting".into(), Color::Yellow),
        DeviceState::Ready => ("ready".into(), Color::Green),
        DeviceState::Closing => ("closing".into(), Color::DarkGray),
        DeviceState::Gone(why) => (format!("gone: {why}"), Color::Red),
    }
}

/// A restart's countdown as its row says it, as the card counts (`omni_pool`'s `countdown_line`):
/// whole seconds rounded up (5, 4, 3, 2, 1), then `now` -- its relaunch waiting for the stop of its
/// app or the device's turn, which `0 s` said for as long as that took.
fn restart_text(left: Option<Duration>) -> String {
    match left {
        None => "restarting".into(),
        Some(t) if t.is_zero() => "restarting now".into(),
        Some(t) => format!("restarting in {} s", t.as_secs_f64().ceil() as u64),
    }
}

/// An instance's state as its row says it: a restart's countdown, and -- unless it ended -- `auto`
/// when its app starts again after a crash, and how many restarts so far.
fn instance_state(i: &InstanceInfo) -> (String, Color) {
    let (state, color) = match &i.state {
        InstanceState::Waiting => ("waiting for its Android".into(), Color::Yellow),
        InstanceState::Installing => ("installing".into(), Color::Yellow),
        InstanceState::Starting => ("starting".into(), Color::Yellow),
        InstanceState::Running => ("running".into(), Color::Green),
        InstanceState::Closing => ("closing".into(), Color::DarkGray),
        InstanceState::Restarting => (restart_text(i.restart_in), Color::Yellow),
        InstanceState::Ended(why) => return (format!("ended: {why}"), Color::DarkGray),
    };
    let tags: Vec<String> = i.auto_restart.then(|| "auto".to_string()).into_iter().chain((i.restarts > 0).then(|| format!("{} restart{}", i.restarts, if i.restarts == 1 { "" } else { "s" }))).collect();
    if tags.is_empty() {
        (state, color)
    } else {
        (format!("{state} ({})", tags.join(", ")), color)
    }
}

impl App {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client, view: View::default(), lost: None, stop_on_exit: true, input: String::new(), history: Vec::new(), history_at: None, focus: Focus::Instances, devices: TableState::default(), instances: TableState::default(), quit_asked: None, quit: false }
    }

    /// Run the screen until the person quits.
    ///
    /// # Errors
    /// The terminal's.
    pub fn run(mut self) -> std::io::Result<Self> {
        let mut terminal = ratatui::init();
        let result = (|| -> std::io::Result<()> {
            while !self.quit {
                self.refresh();
                let (devices, instances) = (self.view.devices.clone(), self.view.instances.clone());
                terminal.draw(|f| self.draw(f, &devices, &instances))?;
                if event::poll(Duration::from_millis(250))? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            self.key(key.code, key.modifiers, &devices, &instances);
                        }
                    }
                }
            }
            Ok(())
        })();
        ratatui::restore();
        self.stop_on_exit = self.view.config.stop_on_exit;
        result.map(|()| self)
    }

    /// The pool as the host has it now. A host gone silent is said once; after 10 s the window
    /// quits (its pool is gone: the next window starts one).
    fn refresh(&mut self) {
        match self.client.state() {
            Ok(view) => {
                self.view = view;
                if self.lost.take().is_some() {
                    self.client.say("the pool answers again");
                }
            }
            Err(e) => match self.lost {
                None => {
                    self.lost = Some(Instant::now());
                    self.client.say(format!("the pool does not answer: {e}"));
                }
                Some(t) if t.elapsed() > Duration::from_secs(10) => self.quit = true,
                Some(_) => {}
            },
        }
    }

    /// A line for the host: said in the log when it cannot be sent.
    fn send(&mut self, line: &str) {
        if let Err(e) = self.client.line(line) {
            self.client.say(format!("the pool: {e}"));
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers, devices: &[DeviceInfo], instances: &[InstanceInfo]) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('c') if ctrl => self.ask_quit(instances),
            KeyCode::Char('p') if ctrl => self.recall(-1),
            KeyCode::Char('n') if ctrl => self.recall(1),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char(c) => {
                self.input.push(c);
                self.history_at = None;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => self.input.clear(),
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                    self.history_at = None;
                    self.execute(&line, instances);
                }
            }
            KeyCode::Tab => match command::complete(&self.input) {
                Some(done) => self.input = done,
                None if self.input.is_empty() => self.focus = if self.focus == Focus::Devices { Focus::Instances } else { Focus::Devices },
                None => {}
            },
            KeyCode::Up => self.move_selection(-1, devices.len(), instances.len()),
            KeyCode::Down => self.move_selection(1, devices.len(), instances.len()),
            KeyCode::Delete => self.delete_selected(devices, instances),
            _ => {}
        }
    }

    fn recall(&mut self, step: i32) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() as i32 - 1;
        let at = match self.history_at {
            None if step < 0 => last,
            None => return,
            Some(a) => a as i32 + step,
        };
        if at > last {
            self.history_at = None;
            self.input.clear();
        } else {
            let at = at.max(0) as usize;
            self.history_at = Some(at);
            self.input = self.history[at].clone();
        }
    }

    fn move_selection(&mut self, step: i32, devices: usize, instances: usize) {
        let (state, len) = match self.focus {
            Focus::Devices => (&mut self.devices, devices),
            Focus::Instances => (&mut self.instances, instances),
        };
        if len == 0 {
            state.select(None);
            return;
        }
        let at = state.selected().map_or(0, |s| (s as i32 + step).clamp(0, len as i32 - 1) as usize);
        state.select(Some(at));
    }

    fn delete_selected(&mut self, devices: &[DeviceInfo], instances: &[InstanceInfo]) {
        match self.focus {
            Focus::Instances => {
                if let Some(i) = self.instances.selected().and_then(|s| instances.get(s)) {
                    let line = format!("close {}", i.id);
                    self.send(&line);
                }
            }
            Focus::Devices => {
                if let Some(d) = self.devices.selected().and_then(|s| devices.get(s)) {
                    let line = format!("kill {}", d.id);
                    self.send(&line);
                }
            }
        }
    }

    fn ask_quit(&mut self, instances: &[InstanceInfo]) {
        let running = instances.iter().filter(|i| !matches!(i.state, InstanceState::Ended(_))).count();
        let stop = self.view.config.stop_on_exit;
        // Another window open keeps the pool: this one just goes.
        let last = self.view.windows <= 1;
        if running == 0 || !stop || !last || self.lost.is_some() || self.quit_asked.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            self.quit = true;
        } else {
            self.quit_asked = Some(Instant::now());
            self.client.say(format!("{running} instance{} still run: quit again (Ctrl+C or `quit`) within 10 s to stop them and every Android", if running == 1 { "" } else { "s" }));
        }
    }

    fn execute(&mut self, line: &str, instances: &[InstanceInfo]) {
        match command::parse(line) {
            Err(e) if e.is_empty() => {}
            Err(e) => self.client.say(e),
            Ok(Command::Run(apk, n, options)) => run_apk(&mut self.client, &apk, n, &options),
            Ok(Command::Help) => command::HELP.iter().for_each(|l| self.client.say(*l)),
            Ok(Command::Quit) => self.ask_quit(instances),
            // The rest is the pool's: done by the host, its answer in every window's log.
            Ok(_) => self.send(line),
        }
    }

    fn draw(&mut self, f: &mut Frame, devices: &[DeviceInfo], instances: &[InstanceInfo]) {
        let [top, middle, log, input] = Layout::vertical([Constraint::Length(1), Constraint::Min(8), Constraint::Length(10), Constraint::Length(3)]).areas(f.area());
        let config = self.view.config.clone();
        let ready = devices.iter().filter(|d| d.state == DeviceState::Ready).count();
        let running = instances.iter().filter(|i| i.state == InstanceState::Running).count();
        let mcp = self.view.mcp.as_ref().map_or_else(|| "MCP off".to_string(), |url| format!("MCP {url}"));
        let windows = if self.view.windows > 1 { format!("  {} windows", self.view.windows) } else { String::new() };
        let idle = if config.idle_close_minutes == 0 { "idle-close never".to_string() } else { format!("idle-close {}m, keep {}", config.idle_close_minutes, config.keep_warm) };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Omnidroid ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  Androids {} ({ready} ready)  Instances {running} running  {mcp}  {idle}{windows}  `help`", devices.len())),
            ])),
            top,
        );
        let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(middle);
        self.draw_devices(f, left, devices);
        self.draw_instances(f, right, instances);
        let lines = &self.client.log;
        let shown = log.height.saturating_sub(2) as usize;
        let text: Vec<Line> = lines[lines.len().saturating_sub(shown)..].iter().map(|l| Line::raw(l.clone())).collect();
        f.render_widget(Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Log ")).wrap(Wrap { trim: false }), log);
        let hint = if self.quit_asked.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) { " quit again to stop everything " } else { " run <apk> [xN] | close #id | restart #id | warm | kill D# | plugins | settings | mcp | help " };
        f.render_widget(Paragraph::new(format!("> {}", self.input)).block(Block::default().borders(Borders::ALL).title(hint)), input);
        f.set_cursor_position((input.x + 3 + self.input.chars().count() as u16, input.y + 1));
    }

    fn draw_devices(&mut self, f: &mut Frame, area: Rect, devices: &[DeviceInfo]) {
        let rows = devices.iter().map(|d| {
            let (state, color) = device_state(&d.state);
            Row::new(vec![
                Cell::from(format!("D{}", d.id)),
                Cell::from(state).style(Style::default().fg(color)),
                Cell::from(ago(d.up_for)),
                Cell::from(d.idle_for.map_or_else(|| "-".into(), ago)),
                Cell::from(d.installed.len().to_string()),
                Cell::from(d.instances.to_string()),
            ])
        });
        let focused = self.focus == Focus::Devices;
        let table = Table::new(rows, [Constraint::Length(4), Constraint::Min(8), Constraint::Length(7), Constraint::Length(7), Constraint::Length(4), Constraint::Length(4)])
            .header(Row::new(vec!["ID", "State", "Up", "Idle", "Apps", "Inst"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(Block::default().borders(Borders::ALL).title(" Androids ").border_style(Style::default().fg(if focused { Color::Green } else { Color::Gray })))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        if devices.is_empty() {
            self.devices.select(None);
        } else if self.devices.selected().is_none_or(|s| s >= devices.len()) {
            self.devices.select(Some(devices.len() - 1));
        }
        f.render_stateful_widget(table, area, &mut self.devices);
    }

    fn draw_instances(&mut self, f: &mut Frame, area: Rect, instances: &[InstanceInfo]) {
        let rows = instances.iter().map(|i| {
            let (state, color) = instance_state(i);
            Row::new(vec![
                Cell::from(format!("#{}", i.id)),
                Cell::from(i.package.clone()),
                Cell::from(format!("D{}", i.device)),
                Cell::from(i.user.map_or_else(|| "-".into(), |u| u.to_string())),
                Cell::from(i.display.map_or_else(|| "-".into(), |d| d.to_string())),
                Cell::from(state).style(Style::default().fg(color)),
                Cell::from(ago(i.up_for)),
            ])
        });
        let focused = self.focus == Focus::Instances;
        let table = Table::new(rows, [Constraint::Length(4), Constraint::Min(16), Constraint::Length(4), Constraint::Length(4), Constraint::Length(4), Constraint::Min(10), Constraint::Length(7)])
            .header(Row::new(vec!["#", "Package", "On", "User", "Disp", "State", "Up"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(Block::default().borders(Borders::ALL).title(" Instances ").border_style(Style::default().fg(if focused { Color::Green } else { Color::Gray })))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        if instances.is_empty() {
            self.instances.select(None);
        } else if self.instances.selected().is_none_or(|s| s >= instances.len()) {
            self.instances.select(Some(instances.len() - 1));
        }
        f.render_stateful_widget(table, area, &mut self.instances);
    }
}

/// `run`: the APK's path made whole here (the host's working directory is not this window's), as is
/// an option's value naming a file here (a cookie file), then launched by the host.
pub fn run_apk(client: &mut Client, apk: &Path, n: u32, options: &[String]) {
    let options: String = options
        .iter()
        .map(|o| {
            let o = if o.starts_with("--") { o.clone() } else { std::fs::canonicalize(o).map_or_else(|_| o.clone(), |p| p.display().to_string()) };
            if o.contains(char::is_whitespace) { format!(" \"{o}\"") } else { format!(" {o}") }
        })
        .collect();
    match std::fs::canonicalize(apk) {
        Ok(full) => {
            if let Err(e) = client.line(&format!("run {} x{n}{options}", full.display())) {
                client.say(format!("run: the pool: {e}"));
            }
        }
        Err(e) => client.say(format!("run: {}: {e}", apk.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_screen_shows_the_panels_the_log_and_the_prompt() {
        let mut config = omni_pool::Config::default();
        config.warm_on_start = false;
        // Adopting none: the person's own pool keeps its devices and windows.
        let pool = omni_pool::Pool::start_alone(config, None, std::path::PathBuf::from("omnidroid"));
        pool.say("hello from the pool");
        let host = super::super::host::Host::new(std::sync::Arc::clone(&pool));
        let control = omni_mcp::http::serve(0, std::sync::Arc::clone(&host)).unwrap();
        let url = format!("http://127.0.0.1:{}{}", control.port, omni_mcp::http::PATH);
        let mut app = App::new(Client::attach(&url).unwrap());
        app.refresh();
        app.input = "run x.apk".into();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        let (devices, instances) = (app.view.devices.clone(), app.view.instances.clone());
        terminal.draw(|f| app.draw(f, &devices, &instances)).unwrap();
        let screen: String = terminal.backend().buffer().content().iter().map(ratatui::buffer::Cell::symbol).collect();
        for want in [" Omnidroid ", "Androids", "Instances", "Log", "hello from the pool", "> run x.apk", "MCP off"] {
            assert!(screen.contains(want), "{want:?} not on the screen");
        }
        app.execute("set keep_warm 2", &instances);
        assert_eq!(pool.config().keep_warm, 2);
        app.execute("nonsense", &instances);
        assert!(app.client.log.last().unwrap().contains("unknown command"));
        control.stop();
        pool.shutdown(false);
    }

    #[test]
    fn a_restart_counts_down_and_auto_restart_is_marked() {
        let i = InstanceInfo {
            id: 3,
            device: 1,
            package: "p".into(),
            sha256: "s".into(),
            apk: std::path::PathBuf::from("/a.apk"),
            user: Some(10),
            display: Some(2),
            screenshot: None,
            state: InstanceState::Restarting,
            up_for: Duration::from_secs(9),
            details: Vec::new(),
            auto_restart: true,
            restarts: 1,
            restart_in: Some(Duration::from_millis(2300)),
        };
        assert_eq!(instance_state(&i).0, "restarting in 3 s (auto, 1 restart)");
        let done = InstanceInfo { restart_in: Some(Duration::ZERO), ..i.clone() };
        assert_eq!(instance_state(&done).0, "restarting now (auto, 1 restart)", "its countdown done, its relaunch waiting: never 0 s");
        let i = InstanceInfo { state: InstanceState::Running, auto_restart: false, restarts: 0, restart_in: None, ..i };
        assert_eq!(instance_state(&i).0, "running");
        let i = InstanceInfo { state: InstanceState::Ended("closed".into()), auto_restart: true, ..i };
        assert_eq!(instance_state(&i).0, "ended: closed", "an ended one's setting is no matter");
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(ago(Duration::from_secs(12)), "12s");
        assert_eq!(ago(Duration::from_secs(185)), "3m05s");
        assert_eq!(ago(Duration::from_secs(3720)), "1h02m");
    }
}
