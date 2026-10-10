//! **The app's screen**: the Androids and the instances, the log, and a command line -- full-screen
//! in the terminal (ratatui over crossterm: the Windows console and Unix terminals alike).
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use omni_pool::{DeviceInfo, DeviceState, InstanceInfo, InstanceState, Pool};

use super::command::{self, Command};
use super::mcp::McpServer;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Devices,
    Instances,
}

/// What the screen holds between frames.
pub struct App {
    pub pool: Arc<Pool>,
    pub mcp: Option<McpServer>,
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

fn instance_state(s: &InstanceState) -> (String, Color) {
    match s {
        InstanceState::Waiting => ("waiting for its Android".into(), Color::Yellow),
        InstanceState::Installing => ("installing".into(), Color::Yellow),
        InstanceState::Starting => ("starting".into(), Color::Yellow),
        InstanceState::Running => ("running".into(), Color::Green),
        InstanceState::Closing => ("closing".into(), Color::DarkGray),
        InstanceState::Ended(why) => (format!("ended: {why}"), Color::DarkGray),
    }
}

impl App {
    #[must_use]
    pub fn new(pool: Arc<Pool>, mcp: Option<McpServer>) -> Self {
        Self { pool, mcp, input: String::new(), history: Vec::new(), history_at: None, focus: Focus::Instances, devices: TableState::default(), instances: TableState::default(), quit_asked: None, quit: false }
    }

    /// Run the screen until the person quits.
    ///
    /// # Errors
    /// The terminal's.
    pub fn run(mut self) -> std::io::Result<Self> {
        let mut terminal = ratatui::init();
        let result = (|| -> std::io::Result<()> {
            while !self.quit {
                let (devices, instances) = self.pool.snapshot();
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
        result.map(|()| self)
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
                    let id = i.id;
                    if let Err(e) = self.pool.close(id) {
                        self.pool.say(e);
                    }
                }
            }
            Focus::Devices => {
                if let Some(d) = self.devices.selected().and_then(|s| devices.get(s)) {
                    let (pool, id) = (Arc::clone(&self.pool), d.id);
                    std::thread::spawn(move || pool.kill(id));
                }
            }
        }
    }

    fn ask_quit(&mut self, instances: &[InstanceInfo]) {
        let running = instances.iter().filter(|i| !matches!(i.state, InstanceState::Ended(_))).count();
        let stop = self.pool.config().stop_on_exit;
        if running == 0 || !stop || self.quit_asked.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            self.quit = true;
        } else {
            self.quit_asked = Some(Instant::now());
            self.pool.say(format!("{running} instance{} still run: quit again (Ctrl+C or `quit`) within 10 s to stop them and every Android", if running == 1 { "" } else { "s" }));
        }
    }

    fn execute(&mut self, line: &str, instances: &[InstanceInfo]) {
        let pool = Arc::clone(&self.pool);
        match command::parse(line) {
            Err(e) if e.is_empty() => {}
            Err(e) => pool.say(e),
            Ok(Command::Run(apk, n)) => {
                if let Err(e) = pool.launch(&apk, n) {
                    pool.say(format!("run: {e}"));
                }
            }
            Ok(Command::Close(ids)) => {
                for id in ids {
                    if let Err(e) = pool.close(id) {
                        pool.say(e);
                    }
                }
            }
            Ok(Command::CloseAll) => {
                for i in instances.iter().filter(|i| !matches!(i.state, InstanceState::Ended(_))) {
                    let _ = pool.close(i.id);
                }
            }
            Ok(Command::Warm) => {
                let id = pool.warm();
                pool.say(format!("D{id}: warming"));
            }
            Ok(Command::Kill(ids)) => {
                for id in ids {
                    let pool = Arc::clone(&pool);
                    std::thread::spawn(move || {
                        if let Err(e) = pool.kill(id) {
                            pool.say(e);
                        }
                    });
                }
            }
            Ok(Command::KillAll) => {
                let (devices, _) = pool.snapshot();
                for d in devices {
                    let pool = Arc::clone(&pool);
                    std::thread::spawn(move || pool.kill(d.id));
                }
            }
            Ok(Command::Shell(id, cmd)) => match pool.device(id) {
                Some(dev) => {
                    std::thread::spawn(move || match dev.shell_as(&cmd, Some(0), Duration::from_secs(120)) {
                        Ok((code, out)) => {
                            for l in out.lines().take(60) {
                                pool.say(format!("D{id}> {l}"));
                            }
                            pool.say(format!("D{id}: exit {code}"));
                        }
                        Err(e) => pool.say(format!("D{id}: {e}")),
                    });
                }
                None => pool.say(format!("no Android D{id}")),
            },
            Ok(Command::Plugins(args)) => match crate::plugins_lines(&args) {
                Ok(lines) => lines.into_iter().for_each(|l| pool.say(l)),
                Err(e) => pool.say(format!("plugins: {e}")),
            },
            Ok(Command::Settings) => {
                let c = pool.config();
                for (key, what) in omni_pool::config::KEYS {
                    pool.say(format!("{key} = {}   ({what})", c.get(key).unwrap_or_default()));
                }
            }
            Ok(Command::Set(key, value)) => match pool.set(&key, &value) {
                Ok(()) if key.starts_with("mcp_") => self.mcp_follow(),
                Ok(()) => {}
                Err(e) => pool.say(e),
            },
            Ok(Command::Mcp(what)) => self.mcp_command(&what),
            Ok(Command::Clear) => pool.clear_ended(),
            Ok(Command::Help) => command::HELP.iter().for_each(|l| pool.say(*l)),
            Ok(Command::Quit) => self.ask_quit(instances),
        }
    }

    /// The server as the settings say: started, stopped or moved.
    fn mcp_follow(&mut self) {
        let c = self.pool.config();
        if let Some(s) = self.mcp.take() {
            s.stop();
        }
        if c.mcp_enabled {
            match McpServer::start(Arc::clone(&self.pool), c.mcp_port) {
                Ok(s) => {
                    self.pool.say(format!("MCP: listening on {}", s.url()));
                    self.mcp = Some(s);
                }
                Err(e) => self.pool.say(format!("MCP: {e}")),
            }
        } else {
            self.pool.say("MCP: off");
        }
    }

    fn mcp_command(&mut self, what: &str) {
        let pool = Arc::clone(&self.pool);
        match what {
            "on" => {
                if let Err(e) = pool.set("mcp_enabled", "true") {
                    return pool.say(e);
                }
                self.mcp_follow();
                self.mcp_command("register");
            }
            "off" => {
                if let Err(e) = pool.set("mcp_enabled", "false") {
                    return pool.say(e);
                }
                self.mcp_follow();
            }
            "register" => match &self.mcp {
                Some(s) => {
                    let url = s.url();
                    for r in [super::mcp::register_claude(&url), super::mcp::register_pi(&url)] {
                        pool.say(match r {
                            Ok(m) => m,
                            Err(e) => format!("not registered: {e}"),
                        });
                    }
                    super::mcp::snippet(&url).into_iter().for_each(|l| pool.say(l));
                }
                None => pool.say("MCP is off: `mcp on` first"),
            },
            "status" => match &self.mcp {
                Some(s) => {
                    pool.say(format!("MCP: on, {}", s.url()));
                    super::mcp::snippet(&s.url()).into_iter().for_each(|l| pool.say(l));
                }
                None => pool.say("MCP: off (`mcp on` serves it on 127.0.0.1 and registers it for Claude Code and Omni Agent)"),
            },
            other => pool.say(format!("mcp on | off | register | status, not `{other}`")),
        }
    }

    fn draw(&mut self, f: &mut Frame, devices: &[DeviceInfo], instances: &[InstanceInfo]) {
        let [top, middle, log, input] = Layout::vertical([Constraint::Length(1), Constraint::Min(8), Constraint::Length(10), Constraint::Length(3)]).areas(f.area());
        let config = self.pool.config();
        let ready = devices.iter().filter(|d| d.state == DeviceState::Ready).count();
        let running = instances.iter().filter(|i| i.state == InstanceState::Running).count();
        let mcp = self.mcp.as_ref().map_or_else(|| "MCP off".to_string(), |s| format!("MCP {}", s.url()));
        let idle = if config.idle_close_minutes == 0 { "idle-close never".to_string() } else { format!("idle-close {}m, keep {}", config.idle_close_minutes, config.keep_warm) };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Omnidroid ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  Androids {} ({ready} ready)  Instances {running} running  {mcp}  {idle}  `help`", devices.len())),
            ])),
            top,
        );
        let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(middle);
        self.draw_devices(f, left, devices);
        self.draw_instances(f, right, instances);
        let lines = self.pool.log();
        let shown = log.height.saturating_sub(2) as usize;
        let text: Vec<Line> = lines[lines.len().saturating_sub(shown)..].iter().map(|l| Line::raw(l.clone())).collect();
        f.render_widget(Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Log ")).wrap(Wrap { trim: false }), log);
        let hint = if self.quit_asked.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) { " quit again to stop everything " } else { " run <apk> [xN] | close #id | warm | kill D# | plugins | settings | mcp | help " };
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
            let (state, color) = instance_state(&i.state);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_short() {
        assert_eq!(ago(Duration::from_secs(12)), "12s");
        assert_eq!(ago(Duration::from_secs(185)), "3m05s");
        assert_eq!(ago(Duration::from_secs(3720)), "1h02m");
    }
}
