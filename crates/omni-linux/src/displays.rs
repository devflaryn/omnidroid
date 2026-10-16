//! **The device's displays beyond its own**, added and removed while it runs: each one a composer
//! display ([`Composer::add_display`]) shown in a host window of its own once something draws on
//! it, resized on its own, its pointer going to its own Android display, and kept as a PNG beside
//! the device's own screenshot. A host program asks for them through the device's control channel
//! (`#host` commands, [`Displays::handle`]): one display per app instance, so many apps -- or many
//! instances of one app, each an Android user of its own -- run side by side on one Android, each
//! in its window.
//!
//! | command | answer |
//! |---|---|
//! | `display add <w>x<h>` | the composer's id of the new display |
//! | `display remove <id>` | -- (its window closes) |
//! | `display bind <id> <android id>` | -- (the window's pointer goes to that Android display) |
//! | `display size <id> <w>x<h>` | its size now, as a window resized to it would make it |
//! | `display list` | a line `<id> <w>x<h> <android id>` per display beyond display 0 |
//! | `apps` | a line `<pid> <uid> <process name>` per app process running ([`crate::zygote::apps`]) |
//!
//! Android gives the display an id of its own; a host finds it by the display's name
//! ([`crate::hal::composer::display_name`]) in `dumpsys display`, and binds it.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::display_window::{Options, SharedInput};
use crate::hal::composer::Composer;

/// How the device's extra displays are shown.
pub struct Displays {
    composer: Arc<Composer>,
    /// Windows at all (`OMNI_WINDOW=1`).
    windows: bool,
    /// The device's keyboard and mouse, shared by every window.
    input: Option<SharedInput>,
    /// The device's own screenshot (`OMNI_SCREENSHOT`); an extra display's is beside it.
    screenshot: Option<PathBuf>,
    screenshot_every: Duration,
    /// Where a window the person closed is said (`display-<id>.closed`).
    closed_dir: Option<PathBuf>,
    extra: Mutex<BTreeMap<i64, Extra>>,
}

struct Extra {
    stop: Arc<AtomicBool>,
    android: Arc<AtomicI32>,
}

/// An extra display's screenshot: `<base stem>.d<id>.png` beside the device's `<base>`.
#[must_use]
pub fn screenshot_path(base: &Path, id: i64) -> PathBuf {
    let stem = base.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    base.with_file_name(format!("{stem}.d{id}.png"))
}

/// The file a reader makes to have a display's screenshot written now: `<png>.want` beside it.
#[must_use]
pub fn screenshot_want(png: &Path) -> PathBuf {
    png.with_extension("png.want")
}

/// **A display's screenshot, kept at `to`** until `stop`: written whole (a rename, so a reader
/// never sees half) when a reader asks for it -- [`screenshot_want`] is there; the file is removed
/// once the PNG is current -- and otherwise every `every`, only if a frame came since. Asked-for
/// shots are seen within 100 ms. Before, every display's 3.6 MB PNG (1280x720, stored) was written
/// each second for nobody: ~11 MB/s of disk writes and their CRCs with three displays (2026-10-16).
pub fn keep_screenshot(framebuffer: &crate::hal::framebuffer::Framebuffer, to: &Path, every: Duration, stop: &AtomicBool, mut each: impl FnMut()) {
    const LOOK: Duration = Duration::from_millis(100);
    let want = screenshot_want(to);
    let (mut written, mut last) = (0, std::time::Instant::now());
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(LOOK);
        each();
        let asked = want.exists();
        if !asked && last.elapsed() < every {
            continue;
        }
        last = std::time::Instant::now();
        let frames = framebuffer.frames();
        if frames != 0 && (frames != written || !to.exists()) {
            written = frames;
            let part = to.with_extension("png.part");
            if std::fs::write(&part, framebuffer.png()).is_ok() {
                let _ = std::fs::rename(&part, to);
            }
        }
        if asked {
            let _ = std::fs::remove_file(&want);
        }
    }
}

/// The marker a window the person closed leaves: `<dir>/display-<id>.closed`.
#[must_use]
pub fn closed_marker(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("display-{id}.closed"))
}

/// `<w>x<h>`.
fn size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

impl Displays {
    #[must_use]
    pub fn new(composer: Arc<Composer>, windows: bool, input: Option<SharedInput>, screenshot: Option<PathBuf>, screenshot_every: Duration, closed_dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self { composer, windows, input, screenshot, screenshot_every, closed_dir, extra: Mutex::new(BTreeMap::new()) })
    }

    /// Add a display of `w` x `h`: its window opens when it first draws, and its screenshot is
    /// kept from then on. Answers its id.
    ///
    /// # Errors
    /// The composer's.
    pub fn add(&self, w: u32, h: u32) -> Result<i64, String> {
        let (id, framebuffer) = self.composer.add_display(w, h)?;
        let (stop, android) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicI32::new(0)));
        self.extra.lock().insert(id, Extra { stop: Arc::clone(&stop), android: Arc::clone(&android) });
        if let Some(dir) = &self.closed_dir {
            let _ = std::fs::remove_file(closed_marker(dir, id));
        }
        if self.windows {
            let composer = Arc::clone(&self.composer);
            let fb = Arc::clone(&framebuffer);
            let options = Options {
                title: "Omnidroid".into(),
                control: None,
                input: self.input.is_some(),
                display: id,
                android_display: Some(Arc::clone(&android)),
                shared_input: self.input.clone(),
                stop: Some(Arc::clone(&stop)),
                closed_marker: self.closed_dir.as_deref().map(|d| closed_marker(d, id)),
            };
            let stop = Arc::clone(&stop);
            // **Its window opens when it first draws**: a display nothing is on yet has nothing to
            // show.
            let _ = std::thread::Builder::new().name(format!("omni-display-{id}-wait")).spawn(move || {
                while !fb.wait_frame(1, Duration::from_secs(1)) {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                }
                if let Err(e) = crate::display_window::spawn(fb, composer, options) {
                    eprintln!("[window] display {id}: {e}");
                }
            });
        }
        if let Some(base) = &self.screenshot {
            let to = screenshot_path(base, id);
            let (every, stop) = (self.screenshot_every, Arc::clone(&stop));
            let _ = std::thread::Builder::new().name(format!("omni-display-{id}-shot")).spawn(move || {
                keep_screenshot(&framebuffer, &to, every, &stop, || {});
                let _ = std::fs::remove_file(&to);
                let _ = std::fs::remove_file(screenshot_want(&to));
            });
        }
        Ok(id)
    }

    /// Remove the display `id`: its window closes, its screenshot goes.
    ///
    /// # Errors
    /// Not one of the displays added here, or the composer's.
    pub fn remove(&self, id: i64) -> Result<(), String> {
        let extra = self.extra.lock().remove(&id).ok_or_else(|| format!("no display {id} was added"))?;
        extra.stop.store(true, Ordering::Release);
        self.composer.remove_display(id)
    }

    /// The window of the display `id` sends its pointer to the Android display `android`.
    ///
    /// # Errors
    /// Not one of the displays added here.
    pub fn bind(&self, id: i64, android: i32) -> Result<(), String> {
        let extra = self.extra.lock();
        let e = extra.get(&id).ok_or_else(|| format!("no display {id} was added"))?;
        e.android.store(android, Ordering::Relaxed);
        Ok(())
    }

    /// A line per added display: `<id> <w>x<h> <android id>`.
    #[must_use]
    pub fn list(&self) -> String {
        self.extra
            .lock()
            .iter()
            .map(|(id, e)| {
                let (w, h) = self.composer.display_size_of(*id);
                format!("{id} {w}x{h} {}\n", e.android.load(Ordering::Relaxed))
            })
            .collect()
    }

    /// One `#host` command (see the module's table): its exit status and output.
    pub fn handle(&self, command: &str) -> (i64, String) {
        let words: Vec<&str> = command.split_whitespace().collect();
        let answer = match words.as_slice() {
            ["display", "add", wh] => size(wh).ok_or_else(|| format!("not <w>x<h>: {wh}")).and_then(|(w, h)| self.add(w, h)).map(|id| format!("{id}\n")),
            ["display", "remove", id] => id.parse().map_err(|_| format!("not a display id: {id}")).and_then(|id| self.remove(id)).map(|()| String::new()),
            ["display", "bind", id, android] => match (id.parse(), android.parse()) {
                (Ok(id), Ok(android)) => self.bind(id, android).map(|()| String::new()),
                _ => Err(format!("not ids: {id} {android}")),
            },
            ["display", "size", id, wh] => match (id.parse::<i64>(), size(wh)) {
                (Ok(id), Some((w, h))) if self.extra.lock().contains_key(&id) => self.composer.set_display_size_of(id, w, h).map(|(w, h)| format!("{w}x{h}\n")),
                (Ok(id), Some(_)) => Err(format!("no display {id} was added")),
                _ => Err(format!("not <id> <w>x<h>: {id} {wh}")),
            },
            ["display", "list"] => Ok(self.list()),
            ["apps"] => Ok(crate::zygote::apps().into_iter().map(|(pid, uid, name)| format!("{pid} {uid} {name}\n")).collect()),
            _ => Err(format!("unknown host command: {command:?} (display add <w>x<h> | remove <id> | bind <id> <android id> | size <id> <w>x<h> | list; apps)")),
        };
        match answer {
            Ok(out) => (0, out),
            Err(e) => (1, format!("{e}\n")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_extra_display_s_files_are_beside_the_device_s() {
        assert_eq!(screenshot_path(Path::new("/t/omni-warm-1.png"), 3), PathBuf::from("/t/omni-warm-1.d3.png"));
        assert_eq!(closed_marker(Path::new("/t/omni-warm-1.ctl"), 3), PathBuf::from("/t/omni-warm-1.ctl/display-3.closed"));
        assert_eq!(size("1280x720"), Some((1280, 720)));
        assert_eq!(size("1280"), None);
    }
}
