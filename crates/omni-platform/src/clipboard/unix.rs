//! The clipboard seam on Linux (and any other unix but macOS): the desktop's clipboard tool.
//!
//! An X11 or Wayland clipboard is a selection some live process owns and answers each paste from;
//! `wl-copy`, `xclip` and `xsel` fork one that does and outlives the call. Which one: `wl-copy`
//! when the session is Wayland's (`WAYLAND_DISPLAY`), else `xclip`, else `xsel` (text only) when
//! it has an X display (`DISPLAY`). Each is found on `PATH`, started with fixed arguments -- never
//! through a shell -- and given the data on its standard input.
use std::io::Write;
use std::process::{Command, Stdio};

use super::{ClipboardError, ClipboardResult, Image};

/// A tool's invocation: its program name and arguments.
type Tool = (&'static str, &'static [&'static str]);

const WL_TEXT: Tool = ("wl-copy", &["--type", "text/plain;charset=utf-8"]);
const WL_PNG: Tool = ("wl-copy", &["--type", "image/png"]);
const XCLIP_TEXT: Tool = ("xclip", &["-selection", "clipboard", "-in"]);
const XCLIP_PNG: Tool = ("xclip", &["-selection", "clipboard", "-target", "image/png", "-in"]);
const XSEL_TEXT: Tool = ("xsel", &["--clipboard", "--input"]);

/// The tools to try for this session, in order.
fn tools(image: bool) -> Vec<Tool> {
    let mut out = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        out.push(if image { WL_PNG } else { WL_TEXT });
    }
    if std::env::var_os("DISPLAY").is_some() {
        out.push(if image { XCLIP_PNG } else { XCLIP_TEXT });
        if !image {
            out.push(XSEL_TEXT);
        }
    }
    out
}

/// Whether `program` is an executable file in a directory on `PATH`.
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| std::fs::metadata(dir.join(program)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
    })
}

/// Hand `bytes` to the first tool there is.
fn run(operation: &'static str, image: bool, bytes: &[u8]) -> ClipboardResult<()> {
    let candidates = tools(image);
    if candidates.is_empty() {
        return Err(ClipboardError::Unavailable { detail: "no display (neither WAYLAND_DISPLAY nor DISPLAY is set)".into() });
    }
    let Some((program, args)) = candidates.iter().copied().find(|(p, _)| on_path(p)) else {
        let names: Vec<&str> = candidates.iter().map(|(p, _)| *p).collect();
        return Err(ClipboardError::Unavailable { detail: format!("none of {} is on PATH (install one)", names.join(", ")) });
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ClipboardError::Os { operation, api: program, code: i64::from(e.raw_os_error().unwrap_or(0)) })?;
    let written = child.stdin.take().map(|mut stdin| stdin.write_all(bytes));
    // The tool reads to the end, forks the selection's owner and exits.
    let status = child.wait().map_err(|e| ClipboardError::Os { operation, api: program, code: i64::from(e.raw_os_error().unwrap_or(0)) })?;
    match written {
        Some(Ok(())) if status.success() => Ok(()),
        _ => Err(ClipboardError::Os { operation, api: program, code: i64::from(status.code().unwrap_or(-1)) }),
    }
}

pub(super) fn set_text(text: &str) -> ClipboardResult<()> {
    run("set_text", false, text.as_bytes())
}

pub(super) fn set_image(image: &Image<'_>) -> ClipboardResult<()> {
    run("set_image", true, image.png)
}

/// The clipboard's text, read by the session's tool (`wl-paste`, `xclip -o`, `xsel -o`).
pub(super) fn get_text() -> ClipboardResult<Option<String>> {
    let mut candidates: Vec<Tool> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        candidates.push(("wl-paste", &["--no-newline", "--type", "text/plain"]));
    }
    if std::env::var_os("DISPLAY").is_some() {
        candidates.push(("xclip", &["-selection", "clipboard", "-out"]));
        candidates.push(("xsel", &["--clipboard", "--output"]));
    }
    let Some((program, args)) = candidates.iter().copied().find(|(p, _)| on_path(p)) else {
        return Err(ClipboardError::Unavailable { detail: "no wl-paste, xclip or xsel for the session's display".into() });
    };
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| ClipboardError::Os { operation: "get_text", api: program, code: i64::from(e.raw_os_error().unwrap_or(0)) })?;
    // An empty clipboard, or one holding no text, is a failure to these tools.
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}
