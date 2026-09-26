//! **The control channel's words**: one command per line, from stdin or an append-only file.
//!
//! | line | command |
//! |---|---|
//! | `headless on` / `headless off` | [`Command::Headless`] |
//! | `screenshot <path>` | [`Command::Screenshot`] -- the rest of the line, trimmed, is the path |
//! | `status` | [`Command::Status`] |
//! | empty, or starting with `#` | nothing |
//!
//! Words are case-insensitive; the path is taken as written.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

/// One control command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Drop (`true`) or stop dropping (`false`) the frame's GPU work.
    Headless(bool),
    /// Render the next frame for real and save it as a PNG here.
    Screenshot(PathBuf),
    /// Say whether headless is on, and what it has dropped.
    Status,
}

/// One line, parsed: `Ok(None)` for a blank line or a comment, `Err` naming what is wrong.
///
/// # Errors
///
/// A line that is not one of the module's commands, with the reason.
pub fn parse(line: &str) -> Result<Option<Command>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (line, ""),
    };
    match word.to_ascii_lowercase().as_str() {
        "headless" => match rest.to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Ok(Some(Command::Headless(true))),
            "off" | "0" | "false" => Ok(Some(Command::Headless(false))),
            other => Err(format!("`headless` takes `on` or `off`, not `{other}`")),
        },
        "screenshot" if rest.is_empty() => Err("`screenshot` needs a path".to_string()),
        "screenshot" => Ok(Some(Command::Screenshot(PathBuf::from(rest)))),
        "status" if rest.is_empty() => Ok(Some(Command::Status)),
        "status" => Err("`status` takes nothing".to_string()),
        other => Err(format!("unknown command `{other}` (headless on|off, screenshot <path>, status)")),
    }
}

/// An append-only command file, read from where the last read stopped.
///
/// Only **complete** lines are taken: a line a writer has not finished (no newline yet) is left
/// for the next poll. A file that does not exist yet is nothing to read; one that shrank (it was
/// truncated or replaced) is read again from its start.
#[derive(Debug)]
pub struct ControlFile {
    path: PathBuf,
    offset: u64,
}

impl ControlFile {
    /// Follow `path` from its start: commands already in it when the session starts are run.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), offset: 0 }
    }

    /// The file this follows.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every complete line appended since the last poll.
    pub fn poll(&mut self) -> Vec<String> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.offset = 0;
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        let Some(end) = bytes.iter().rposition(|&b| b == b'\n') else {
            return Vec::new();
        };
        self.offset += end as u64 + 1;
        String::from_utf8_lossy(&bytes[..end]).lines().map(str::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commands_parse_and_anything_else_is_named() {
        assert_eq!(parse("headless on"), Ok(Some(Command::Headless(true))));
        assert_eq!(parse("  HEADLESS   Off \r"), Ok(Some(Command::Headless(false))));
        assert_eq!(
            parse("screenshot C:\\shots\\a b.png"),
            Ok(Some(Command::Screenshot(PathBuf::from("C:\\shots\\a b.png"))))
        );
        assert_eq!(parse("status"), Ok(Some(Command::Status)));
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("# a comment"), Ok(None));
        assert!(parse("headless maybe").unwrap_err().contains("maybe"));
        assert!(parse("screenshot").unwrap_err().contains("path"));
        assert!(parse("fly away").unwrap_err().contains("fly"));
    }

    #[test]
    fn a_control_file_is_followed_line_by_complete_line() {
        let path = std::env::temp_dir().join(format!("omni-control-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut control = ControlFile::new(&path);
        assert!(control.poll().is_empty(), "no file yet");
        std::fs::write(&path, "headless on\nscreen").unwrap();
        assert_eq!(control.poll(), ["headless on"]);
        assert!(control.poll().is_empty(), "the half-written line waits");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("shot x.png\nstatus\n");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(control.poll(), ["screenshot x.png", "status"]);
        std::fs::write(&path, "headless off\n").unwrap();
        assert_eq!(control.poll(), ["headless off"], "a shorter file is read again from its start");
        let _ = std::fs::remove_file(&path);
    }
}
