//! **The device's displays beyond its own**, added and removed while it runs: each one a composer
//! display ([`Composer::add_display`]) shown in a host window of its own once something draws on
//! it, resized on its own, its pointer going to its own Android display, and kept as a PNG beside
//! the device's own screenshot -- or showing a card instead of its frames (`crate::card`: what
//! happened to the app it ran) until the card is taken away. A host program asks for them through
//! the device's control channel (`#host` commands, [`Displays::handle`]): one display per app
//! instance, so many apps -- or many instances of one app, each an Android user of its own -- run
//! side by side on one Android, each in its window.
//!
//! | command | answer |
//! |---|---|
//! | `display add <w>x<h>` | the composer's id of the new display |
//! | `display remove <id>` | -- (its window closes) |
//! | `display bind <id> <android id>` | -- (the window's pointer goes to that Android display) |
//! | `display size <id> <w>x<h>` | its size now, as a window resized to it would make it |
//! | `display card <id> <hex>` | -- (its window shows the card whose text is `<hex>`, [`crate::card::from_hex`], instead of its frames) |
//! | `display show <id>` | -- (its window shows its frames again) |
//! | `display owners <id>` | a line `<pid> <uid>` per process whose buffers a layer of the display holds ([`Composer::owners`]: which app draws there) |
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

use crate::card::Card;
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
    /// Where a window says the person closed it ([`closed_marker`]) or clicked its card's restart
    /// block ([`restart_marker`]), and reads a script's commands ([`control_file`]).
    closed_dir: Option<PathBuf>,
    extra: Mutex<BTreeMap<i64, Extra>>,
}

struct Extra {
    stop: Arc<AtomicBool>,
    android: Arc<AtomicI32>,
    /// Set while the display shows a card: its window lets go of the app's keyboard and mouse.
    card: Arc<AtomicBool>,
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

/// The marker a window leaves when the person clicks its card's restart block:
/// `<dir>/display-<id>.restart`, beside [`closed_marker`]. The host program polls it, removes it,
/// and starts the app again on the display.
#[must_use]
pub fn restart_marker(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("display-{id}.restart"))
}

/// The file of commands a display's window reads (`display_window::Options::control`: a resize,
/// input, a click -- on its card's restart block when it shows one): `<dir>/display-<id>.control`,
/// beside [`closed_marker`]. Display 0's window alone had one (`OMNI_WINDOW_CONTROL`), and it never
/// shows a card: no script could press a restart block.
#[must_use]
pub fn control_file(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("display-{id}.control"))
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
        let (stop, android, card) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicI32::new(0)), Arc::new(AtomicBool::new(false)));
        self.extra.lock().insert(id, Extra { stop: Arc::clone(&stop), android: Arc::clone(&android), card: Arc::clone(&card) });
        // A removed display's id is the next one's: what its window said, or was told, is not this
        // one's.
        if let Some(dir) = &self.closed_dir {
            let _ = std::fs::remove_file(closed_marker(dir, id));
            let _ = std::fs::remove_file(restart_marker(dir, id));
            let _ = std::fs::remove_file(control_file(dir, id));
        }
        if self.windows {
            let composer = Arc::clone(&self.composer);
            let fb = Arc::clone(&framebuffer);
            let options = Options {
                title: "Omnidroid".into(),
                control: self.closed_dir.as_deref().map(|d| control_file(d, id)),
                input: self.input.is_some(),
                display: id,
                android_display: Some(Arc::clone(&android)),
                shared_input: self.input.clone(),
                stop: Some(Arc::clone(&stop)),
                closed_marker: self.closed_dir.as_deref().map(|d| closed_marker(d, id)),
                restart_marker: self.closed_dir.as_deref().map(|d| restart_marker(d, id)),
                card: Some(card),
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

    /// **Show a card on the display `id` instead of its frames** ([`Composer::set_card`]): `text`'s
    /// first line its title, every other line one of its lines. A window not open yet opens on it,
    /// and its window lets go of the app's keyboard and mouse (`display_window::Options::card`):
    /// the app it showed is gone, and Android's word on the pointer capture and icon stays the dead
    /// app's.
    ///
    /// # Errors
    /// Not one of the displays added here, or the composer's.
    pub fn card(&self, id: i64, text: &str) -> Result<(), String> {
        let flag = self.card_flag(id)?;
        self.composer.set_card(id, Some(Card::parse(text)))?;
        flag.store(true, Ordering::Release);
        Ok(())
    }

    /// The display `id`'s frames in its window again, its card taken away, and its keyboard and
    /// mouse the app's.
    ///
    /// # Errors
    /// Not one of the displays added here, or the composer's.
    pub fn show(&self, id: i64) -> Result<(), String> {
        let flag = self.card_flag(id)?;
        self.composer.set_card(id, None)?;
        flag.store(false, Ordering::Release);
        Ok(())
    }

    /// The flag the window of the display `id` reads for a card, if `id` is one of the displays
    /// added here.
    fn card_flag(&self, id: i64) -> Result<Arc<AtomicBool>, String> {
        self.extra.lock().get(&id).map(|e| Arc::clone(&e.card)).ok_or_else(|| format!("no display {id} was added"))
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
            ["display", "card", id, hex] => match (id.parse(), crate::card::from_hex(hex)) {
                (Ok(id), Some(text)) => self.card(id, &text).map(|()| String::new()),
                (Ok(_), None) => Err(format!("not the hex of UTF-8 text: {}", hex.get(..32).unwrap_or(hex))),
                (Err(_), _) => Err(format!("not a display id: {id}")),
            },
            ["display", "show", id] => id.parse().map_err(|_| format!("not a display id: {id}")).and_then(|id| self.show(id)).map(|()| String::new()),
            ["display", "owners", id] => id
                .parse()
                .map_err(|_| format!("not a display id: {id}"))
                .and_then(|id| self.composer.owners(id).ok_or_else(|| format!("no display {id}")))
                .map(|owners| owners.into_iter().map(|(pid, uid)| format!("{pid} {uid}\n")).collect()),
            ["display", "list"] => Ok(self.list()),
            ["apps"] => Ok(crate::zygote::apps().into_iter().map(|(pid, uid, name)| format!("{pid} {uid} {name}\n")).collect()),
            _ => Err(format!("unknown host command: {command:?} (display add <w>x<h> | remove <id> | bind <id> <android id> | size <id> <w>x<h> | card <id> <hex> | show <id> | owners <id> | list; apps)")),
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
        assert_eq!(restart_marker(Path::new("/t/omni-warm-1.ctl"), 3), PathBuf::from("/t/omni-warm-1.ctl/display-3.restart"));
        assert_eq!(control_file(Path::new("/t/omni-warm-1.ctl"), 3), PathBuf::from("/t/omni-warm-1.ctl/display-3.control"));
        assert_eq!(size("1280x720"), Some((1280, 720)));
        assert_eq!(size("1280"), None);
    }

    /// `display card` puts the card in the display's framebuffer, `display show` takes it away; a
    /// payload that is not hex, an id that is not a number or a display that was not added is
    /// refused, and the help names both.
    #[test]
    fn a_card_is_set_and_taken_away_by_its_commands() {
        let composer = Composer::new(crate::binder::broker(crate::binder::Context::Binder), Arc::new(crate::hal::framebuffer::Framebuffer::new(64, 32)));
        let displays = Displays::new(Arc::clone(&composer), false, None, None, Duration::from_secs(5), None);
        let (id, fb) = composer.add_display(400, 330).unwrap();
        let card = Arc::new(AtomicBool::new(false));
        displays.extra.lock().insert(id, Extra { stop: Arc::default(), android: Arc::default(), card: Arc::clone(&card) });
        let text = "Talking Tom stopped\nJava exception: java.lang.IllegalStateException: boom";
        let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();

        assert_eq!(displays.handle(&format!("display card {id} {hex}")), (0, String::new()));
        assert_eq!(fb.pixels(), crate::card::render(&Card::parse(text), 400, 330), "the card");
        assert!(card.load(Ordering::Acquire), "its window told: the app's input let go");
        assert_eq!(displays.handle(&format!("display show {id}")), (0, String::new()));
        assert!(!card.load(Ordering::Acquire), "and the app's again");
        assert_eq!(displays.handle(&format!("display show {id}")), (0, String::new()), "no card: nothing to take away");

        assert_eq!(displays.handle(&format!("display card {id} 4g")).0, 1, "not hex");
        assert_eq!(displays.handle(&format!("display card {id} ff")).0, 1, "not UTF-8");
        assert_eq!(displays.handle(&format!("display card x {hex}")), (1, "not a display id: x\n".to_string()));
        let other = id + 1;
        assert_eq!(displays.handle(&format!("display card {other} {hex}")), (1, format!("no display {other} was added\n")));
        assert_eq!(displays.handle(&format!("display show {other}")), (1, format!("no display {other} was added\n")));
        let (status, help) = displays.handle("display card");
        assert!(status == 1 && help.contains("card <id> <hex>") && help.contains("show <id>"), "{help}");
    }

    /// `display owners` answers who draws on a display -- nobody on one with no layer yet -- and
    /// refuses a display there is not; the help names it.
    #[test]
    fn a_display_s_owners_are_asked_by_their_command() {
        let composer = Composer::new(crate::binder::broker(crate::binder::Context::Binder), Arc::new(crate::hal::framebuffer::Framebuffer::new(64, 32)));
        let displays = Displays::new(Arc::clone(&composer), false, None, None, Duration::from_secs(5), None);
        let (id, _) = composer.add_display(400, 330).unwrap();
        assert_eq!(displays.handle(&format!("display owners {id}")), (0, String::new()), "no layer: nobody");
        let other = id + 1;
        assert_eq!(displays.handle(&format!("display owners {other}")), (1, format!("no display {other}\n")));
        assert_eq!(displays.handle("display owners x"), (1, "not a display id: x\n".to_string()));
        let (status, help) = displays.handle("display owners");
        assert!(status == 1 && help.contains("owners <id>"), "{help}");
    }
}
