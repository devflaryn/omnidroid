//! **The guest's clipboard, shared with the host's** (one way: what Android copies is pasted on the
//! host). On by default; `OMNI_CLIPBOARD=0` turns it off (`omnidroid aosp --no-clipboard`).
//!
//! # How a copy is heard
//!
//! The host is a binder client of system_server (as [`crate::inject`] is), with **the shell's
//! identity** (uid 2000, package `com.android.shell`, which holds `READ_CLIPBOARD_IN_BACKGROUND`:
//! "Shell can access the clipboard for testing purposes"). Not the system's (uid 1000, package
//! `android`): `ClipboardService` lets a reader without that permission see a clip only while its
//! uid has the focus, and uid 1000's apps (Settings) are the only ones that do -- a copy in any
//! other app was never heard ("Denying clipboard access to android, application is not in focus
//! nor is it a system service", run 2026-10-07, Roblox). It registers
//! a listener of its own with `ClipboardService` (`IClipboard.addPrimaryClipChangedListener`, the
//! listener a host service: [`crate::binder::Broker::create_host_service`]); each change it hears,
//! it reads the clip (`IClipboard.getPrimaryClip`). The image's own way for an emulator --
//! `EmulatorClipboardMonitor`, over vsock -- is not used: it needs `ro.boot.qemu=1`, an emulator
//! tell to every app, and carries text only.
//!
//! # What reaches the host, and what never does
//!
//! Copying can never run anything on the host: only **characters** and **pixels** are put on the
//! host's clipboard (`omni_platform::clipboard`), never a file, a file list, a link to a file,
//! HTML or any other rich type, and nothing is ever written to the host's disk or opened.
//!
//! * **Text** ([`sanitize_text`]): control characters (an ESC that starts a terminal escape
//!   sequence, NUL, C1 controls), bidirectional overrides and isolates ("Trojan Source": text that
//!   reads differently from what it is), Unicode tag characters (invisible ASCII) and other
//!   invisible format characters are removed; trailing line breaks are removed, so a paste into a
//!   terminal never presses Enter by itself; more than [`MAX_TEXT_BYTES`] is not shared.
//! * **An image**: only a clip whose description says `image/*` and whose item is a `content://`
//!   URI. Its bytes are read through `IActivityManager.openContentUri` (the provider's own
//!   permission check, with the grant `getPrimaryClip` gave the reader), at most
//!   [`MAX_IMAGE_BYTES`]; the format is taken from the bytes, never from the name or the declared
//!   type, and only PNG, JPEG, GIF, WebP and BMP are decoded -- by a memory-safe decoder with size
//!   limits ([`decode_image`]). The host gets a **fresh PNG** made from the decoded pixels, never
//!   the guest's bytes: nothing hidden in the file (a polyglot, an archive appended, metadata)
//!   survives. A copied file of any other type (a script, an executable, an archive) is not
//!   shared at all.
//! * **Only while the device's window is in use**: a clip that changes while no omnidroid window
//!   has had the keyboard focus for [`FOCUS_GRACE`] is not shared. Android lets an app write the
//!   clipboard from the background; without this, an app could replace what the host's clipboard
//!   holds (an address the user copied elsewhere) while the user works in another application.
//!
//! Every share, and every refusal, is said on the log (`[clipboard]`), without the content.
//!
//! # The other way: the host's text, pasted in the guest
//!
//! When an omnidroid window gets the keyboard focus back, the host clipboard's **plain text** (and
//! nothing else: no image, file or rich type) is made the device's clip, if it changed since it was
//! last shared either way (`IClipboard.setPrimaryClip` as the shell) -- so what was copied on the
//! host pastes in the app (Ctrl+V, or the app's own paste). Text from the host is cleaned as guest
//! text is ([`sanitize_text`]). The device's copy of it is not shared back.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use std::time::Duration;

use crate::binder::{Broker, Parcel, STABILITY_SYSTEM};

/// `IClipboard`'s interface token and its transaction codes in the image (`IClipboard$Stub`'s
/// `TRANSACTION_*` constants, read from `framework.jar`).
const CLIPBOARD: &str = "android.content.IClipboard";
const SET_PRIMARY_CLIP: u32 = 1;
const GET_PRIMARY_CLIP: u32 = 4;
const ADD_PRIMARY_CLIP_CHANGED_LISTENER: u32 = 7;
/// `IOnPrimaryClipChangedListener.dispatchPrimaryClipChanged`, its only (oneway) call.
const DISPATCH_PRIMARY_CLIP_CHANGED: u32 = 1;
/// `IActivityManager.openContentUri(String)`, transaction 1 (`IActivityManager$Stub`).
const ACTIVITY_MANAGER: &str = "android.app.IActivityManager";
const OPEN_CONTENT_URI: u32 = 1;
/// The package, and the uid, the host reads the clipboard as: the shell's (see the module's
/// header).
const PACKAGE: &str = "com.android.shell";
const SHELL_UID: u32 = 2000;
/// The owner, and `Context.DEVICE_ID_DEFAULT`.
const USER: i32 = 0;
const DEVICE: i32 = 0;

/// The most text shared (UTF-8 bytes, after cleaning).
pub const MAX_TEXT_BYTES: usize = 1 << 20;
/// The largest image file read.
pub const MAX_IMAGE_BYTES: u64 = 32 << 20;
/// The largest side of an image decoded, and the most its pixels may take.
pub const MAX_IMAGE_SIDE: u32 = 16_384;
pub const MAX_IMAGE_ALLOC: u64 = 512 << 20;
/// How long after a window last had the keyboard focus a copy is still the user's.
pub const FOCUS_GRACE: Duration = Duration::from_secs(2);

/// Whether sharing is on: unless `OMNI_CLIPBOARD=0`.
#[must_use]
pub fn enabled() -> bool {
    std::env::var("OMNI_CLIPBOARD").as_deref() != Ok("0")
}

/// When an omnidroid window last had the keyboard focus (ms on [`crate::sys::monotonic`]; 0:
/// never).
static FOCUSED_AT: AtomicU64 = AtomicU64::new(0);

/// Whether a window had the focus at its pump's last turn.
static FOCUSED: AtomicBool = AtomicBool::new(false);

/// What the clipboard's thread is woken for.
enum Wake {
    /// The device's clip changed.
    Guest,
    /// A window got the focus: the host's text, if it changed, for the device.
    Host,
}

/// The clipboard thread's queue, once it runs.
static WAKE: OnceLock<Mutex<mpsc::Sender<Wake>>> = OnceLock::new();

/// A window's pump, each turn: whether it has the keyboard focus now.
pub fn window_focus(focused: bool) {
    if focused {
        FOCUSED_AT.store(now_ms().max(1), Ordering::Relaxed);
    }
    if focused && !FOCUSED.swap(focused, Ordering::Relaxed) {
        if let Some(tx) = WAKE.get() {
            let _ = tx.lock().send(Wake::Host);
        }
    } else if !focused {
        FOCUSED.store(false, Ordering::Relaxed);
    }
}

/// The text last shared either way (what the host's and the device's clipboards both hold), so a
/// share is not echoed back.
static LAST_SHARED: Mutex<Option<String>> = Mutex::new(None);

fn now_ms() -> u64 {
    crate::sys::monotonic().as_millis() as u64
}

/// Whether a window had the focus within [`FOCUS_GRACE`].
fn window_in_use() -> bool {
    let at = FOCUSED_AT.load(Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) <= FOCUS_GRACE.as_millis() as u64
}

/// Start sharing the device's clipboard with the host's (once per host process): a thread waits for
/// `ClipboardService`, registers the listener, and shares each clip it is told of.
pub fn start(broker: Arc<Broker>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("omni-clipboard".into()).spawn(move || watch(&broker));
}

fn watch(broker: &Arc<Broker>) {
    let handle = loop {
        match broker.check_service("clipboard") {
            Ok(Some(h)) => break h,
            Ok(None) => {}
            Err(e) => eprintln!("[clipboard] looking up the clipboard service: {e}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let (tx, rx) = mpsc::channel::<Wake>();
    let listener_tx = Mutex::new(tx.clone());
    let _ = WAKE.set(Mutex::new(tx));
    let listener = broker.create_host_service(move |code, _| {
        if code == DISPATCH_PRIMARY_CLIP_CHANGED {
            let _ = listener_tx.lock().send(Wake::Guest);
        }
        Vec::new()
    });
    let mut p = Parcel::with_interface_token(CLIPBOARD);
    let object = p.binder(listener, STABILITY_SYSTEM);
    p.string16(PACKAGE);
    p.null_string16(); // attributionTag
    p.i32(USER);
    p.i32(DEVICE);
    match broker.host_transact_as(SHELL_UID, handle, ADD_PRIMARY_CLIP_CHANGED_LISTENER, p.bytes, &[object]).map(|(r, _)| r) {
        Ok(reply) if exception(&reply).is_ok_and(|(code, _)| code == 0) => {
            eprintln!("[clipboard] sharing the device's clipboard with the host's (text and images; OMNI_CLIPBOARD=0 turns it off)");
        }
        Ok(reply) => {
            eprintln!("[clipboard] addPrimaryClipChangedListener refused: {:?}", exception(&reply));
            return;
        }
        Err(e) => {
            eprintln!("[clipboard] addPrimaryClipChangedListener failed: errno {}", e.0);
            return;
        }
    }
    // The host's text as it is now is the device's once a window has the focus.
    if FOCUSED.load(Ordering::Relaxed) {
        give_host_text(broker, handle);
    }
    while let Ok(first) = rx.recv() {
        // A burst of changes is one: the clip as it is now.
        std::thread::sleep(Duration::from_millis(50));
        let (mut guest, mut host) = (matches!(first, Wake::Guest), matches!(first, Wake::Host));
        while let Ok(w) = rx.try_recv() {
            guest |= matches!(w, Wake::Guest);
            host |= matches!(w, Wake::Host);
        }
        if host {
            give_host_text(broker, handle);
        }
        if !guest {
            continue;
        }
        if !window_in_use() {
            eprintln!("[clipboard] the clip changed while no omnidroid window was in use: not shared");
            continue;
        }
        share(broker, handle);
    }
}

/// The host clipboard's text, made the device's clip if it changed since it was last shared.
fn give_host_text(broker: &Broker, handle: u32) {
    let text = match omni_platform::clipboard::get_text() {
        Ok(Some(t)) if !t.is_empty() => t,
        Ok(_) => return,
        Err(e) => {
            eprintln!("[clipboard] reading the host clipboard: {e}");
            return;
        }
    };
    let clean = match sanitize_text(&text) {
        Ok(c) => c.text,
        Err(why) => {
            eprintln!("[clipboard] host text not given to the device: {why}");
            return;
        }
    };
    if LAST_SHARED.lock().as_deref() == Some(clean.as_str()) {
        return;
    }
    let mut p = Parcel::with_interface_token(CLIPBOARD);
    p.i32(1);
    write_text_clip(&mut p, &clean);
    p.string16(PACKAGE);
    p.null_string16();
    p.i32(USER);
    p.i32(DEVICE);
    match broker.host_transact_as(SHELL_UID, handle, SET_PRIMARY_CLIP, p.bytes, &[]).map(|(r, _)| r) {
        Ok(reply) if exception(&reply).is_ok_and(|(code, _)| code == 0) => {
            *LAST_SHARED.lock() = Some(clean.clone());
            eprintln!("[clipboard] host text on the device's clipboard: {} characters", clean.chars().count());
        }
        Ok(reply) => eprintln!("[clipboard] setPrimaryClip refused: {:?}", exception(&reply)),
        Err(e) => eprintln!("[clipboard] setPrimaryClip failed: errno {}", e.0),
    }
}

/// A plain-text `ClipData` as Android 15's `ClipData.writeToParcel` writes one: its description
/// (label, MIME types, no extras, timestamp, not styled, classification not complete, an empty
/// confidence bundle), no icon, and one item of text alone.
fn write_text_clip(p: &mut Parcel, text: &str) {
    p.i32(1);
    p.string8(Some("omnidroid"));
    p.i32(1);
    p.string16("text/plain");
    p.i32(-1);
    p.i64(0);
    p.i32(0);
    p.i32(1);
    p.i32(0);
    p.i32(0);
    p.i32(1);
    p.i32(1);
    p.string8(Some(text));
    p.string8(None);
    for _ in 0..5 {
        p.i32(0); // intent, intent sender, URI, activity info, text links
    }
}

/// Read the clip and put what may be shared of it on the host's clipboard.
fn share(broker: &Broker, handle: u32) {
    let mut p = Parcel::with_interface_token(CLIPBOARD);
    p.string16(PACKAGE);
    p.null_string16();
    p.i32(USER);
    p.i32(DEVICE);
    let clip = match broker.host_transact_as(SHELL_UID, handle, GET_PRIMARY_CLIP, p.bytes, &[]).map(|(r, _)| r).map_err(|e| format!("errno {}", e.0)).and_then(|r| parse_clip_reply(&r)) {
        Ok(Some(clip)) => clip,
        Ok(None) => {
            eprintln!("[clipboard] the clip is empty, or not readable by the host");
            return;
        }
        Err(e) => {
            eprintln!("[clipboard] getPrimaryClip: {e}");
            return;
        }
    };
    if let Some(text) = clip.text.as_deref().filter(|t| !t.is_empty()) {
        match sanitize_text(text) {
            Ok(clean) if LAST_SHARED.lock().as_deref() == Some(clean.text.as_str()) => {}
            Ok(clean) => match omni_platform::clipboard::set_text(&clean.text) {
                Ok(()) => {
                    *LAST_SHARED.lock() = Some(clean.text.clone());
                    eprintln!(
                    "[clipboard] guest text on the host clipboard: {} characters{}",
                    clean.text.chars().count(),
                    if clean.removed > 0 { format!(" ({} control or invisible characters removed)", clean.removed) } else { String::new() }
                    );
                }
                Err(e) => eprintln!("[clipboard] the host clipboard: {e}"),
            },
            Err(why) => eprintln!("[clipboard] guest text not shared: {why}"),
        }
        return;
    }
    let image_type = clip.mime_types.iter().any(|m| m.to_ascii_lowercase().starts_with("image/"));
    match (clip.uri.as_deref(), image_type) {
        (Some(uri), true) if uri.starts_with("content://") => share_image(broker, uri),
        (Some(_), _) => eprintln!("[clipboard] guest clip of type {:?} not shared: only text and images are", clip.mime_types),
        (None, _) => eprintln!("[clipboard] guest clip of type {:?} has no text or image: not shared", clip.mime_types),
    }
}

fn share_image(broker: &Broker, uri: &str) {
    let bytes = match read_content_uri(broker, uri) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[clipboard] guest image not shared: {e}");
            return;
        }
    };
    let image = match decode_image(&bytes) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[clipboard] guest image not shared: {e}");
            return;
        }
    };
    let shown = omni_platform::clipboard::Image { width: image.width, height: image.height, rgba: &image.rgba, png: &image.png };
    match omni_platform::clipboard::set_image(&shown) {
        Ok(()) => eprintln!("[clipboard] guest image on the host clipboard: {}x{} {} (re-encoded as PNG)", image.width, image.height, image.format),
        Err(e) => eprintln!("[clipboard] the host clipboard: {e}"),
    }
}

/// The bytes `content://` `uri` names, through `IActivityManager.openContentUri` -- at most
/// [`MAX_IMAGE_BYTES`].
fn read_content_uri(broker: &Broker, uri: &str) -> Result<Vec<u8>, String> {
    let am = broker.check_service("activity").map_err(|e| format!("the activity service: {e}"))?.ok_or("no activity service")?;
    let mut p = Parcel::with_interface_token(ACTIVITY_MANAGER);
    p.string16(uri);
    let (reply, fds) = broker.host_transact_as(SHELL_UID, am, OPEN_CONTENT_URI, p.bytes, &[]).map_err(|e| format!("openContentUri: errno {}", e.0))?;
    let (code, at) = exception(&reply).map_err(|e| format!("openContentUri: {e}"))?;
    if code != 0 {
        return Err(format!("openContentUri: exception {code}"));
    }
    let mut r = Reader { b: &reply, at };
    if r.i32()? == 0 {
        return Err("the provider gave no file (not found, or not readable by the host)".into());
    }
    let file = fds.into_iter().next().ok_or("the reply carried no file descriptor")?;
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = crate::fd::pread_all(&file, &mut chunk, out.len() as u64).map_err(|e| format!("reading the image: errno {} (a pipe, not a file?)", e.0))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!("over {} MiB", MAX_IMAGE_BYTES >> 20));
        }
    }
    Ok(out)
}

/// What is read of a clip: its description's MIME types, and its first item's text or URI.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Clip {
    pub mime_types: Vec<String>,
    pub text: Option<String>,
    pub uri: Option<String>,
}

/// A parcel, read as `android.os.Parcel` reads one; every read is bounds-checked.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| format!("the parcel ends at {} (wanted {n} bytes at {})", self.b.len(), self.at))?;
        let out = &self.b[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.take(n).map(|_| ())
    }

    /// `readString8`: the length in bytes (-1: null), the bytes and a NUL, padded to 4.
    fn string8(&mut self) -> Result<Option<String>, String> {
        let len = self.i32()?;
        if len < 0 {
            return Ok(None);
        }
        let len = len as usize;
        let bytes = self.take((len + 1 + 3) & !3)?;
        Ok(Some(String::from_utf8_lossy(&bytes[..len]).into_owned()))
    }

    /// `readString16`: the length in UTF-16 units (-1: null), the units and a NUL, padded to 4.
    fn string16(&mut self) -> Result<Option<String>, String> {
        let len = self.i32()?;
        if len < 0 {
            return Ok(None);
        }
        let len = len as usize;
        let bytes = self.take(((len + 1) * 2 + 3) & !3)?;
        let units: Vec<u16> = bytes[..len * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(Some(String::from_utf16_lossy(&units)))
    }

    /// A `Bundle` or `PersistableBundle` (`readBundle`): its length (-1: null, 0: empty), then its
    /// magic and that many bytes -- skipped.
    fn skip_bundle(&mut self) -> Result<(), String> {
        let len = self.i32()?;
        if len > 0 {
            self.skip(4)?;
            self.skip(len as usize)?;
        }
        Ok(())
    }
}

/// The reply's exception code (`Parcel.readExceptionCode`) and where what follows it starts. A
/// StrictMode header (-128) is skipped (it is only sent with no exception).
fn exception(reply: &[u8]) -> Result<(i32, usize), String> {
    /// `Parcel.EX_HAS_STRICTMODE_REPLY_HEADER` and `EX_HAS_NOTED_APPOPS_REPLY_HEADER`.
    const STRICTMODE_HEADER: i32 = -128;
    const NOTED_APPOPS_HEADER: i32 = -127;
    let mut r = Reader { b: reply, at: 0 };
    match r.i32()? {
        STRICTMODE_HEADER => {
            let start = r.at;
            let size = r.i32()?;
            if size > 0 {
                r.at = start;
                r.skip(size as usize)?;
            }
            Ok((0, r.at))
        }
        NOTED_APPOPS_HEADER => Err("a noted-app-ops reply header (not read here)".into()),
        code => Ok((code, r.at)),
    }
}

/// `IClipboard.getPrimaryClip`'s reply, read as Android 15's `ClipData(Parcel)` reads it, as far as
/// the first item's text and URI: `None` for no clip. What is not understood is an error, never a
/// guess.
///
/// # Errors
/// An exception, a parcel that ends early, or a clip with an icon (a `Bitmap` in the parcel).
pub fn parse_clip_reply(reply: &[u8]) -> Result<Option<Clip>, String> {
    let (code, at) = exception(reply)?;
    if code != 0 {
        return Err(format!("exception {code}"));
    }
    let mut r = Reader { b: reply, at };
    if r.i32()? == 0 {
        return Ok(None);
    }
    // ClipDescription: label (a CharSequence), MIME types, extras, timestamp, styled, the
    // classification status, the classification's bundle.
    let mut clip = Clip::default();
    if r.i32()? != 1 {
        return Err("a styled label (spans are not read here)".into());
    }
    r.string8()?;
    let n = r.i32()?;
    for _ in 0..n.clamp(0, 64) {
        if let Some(m) = r.string16()? {
            clip.mime_types.push(m);
        }
    }
    if n > 64 {
        return Err(format!("{n} MIME types"));
    }
    r.skip_bundle()?;
    r.skip(8)?;
    r.i32()?;
    r.i32()?;
    r.skip_bundle()?;
    // ClipData: the icon, the items.
    if r.i32()? != 0 {
        return Err("a clip with an icon (a Bitmap is not read here)".into());
    }
    if r.i32()? < 1 {
        return Ok(Some(clip));
    }
    // The first item: text (a CharSequence: 1 and a String8, or 0 and a String8 and its spans),
    // HTML, intent, intent sender, URI.
    let kind = r.i32()?;
    clip.text = r.string8()?;
    if kind != 1 {
        // Styled text: its spans follow, which are not read; the text is what is shared.
        return Ok(Some(clip));
    }
    r.string8()?;
    if r.i32()? != 0 || r.i32()? != 0 {
        // An intent or an intent sender: nothing to share (and not read here).
        return Ok(Some(clip));
    }
    if r.i32()? != 0 {
        // Uri: every kind writes its type and then its string.
        let ty = r.i32()?;
        if !(1..=3).contains(&ty) {
            return Err(format!("a Uri of type {ty}"));
        }
        clip.uri = r.string8()?;
    }
    Ok(Some(clip))
}

/// Text cleaned for the host ([`sanitize_text`]), and how many characters were removed.
#[derive(Debug, PartialEq, Eq)]
pub struct CleanText {
    pub text: String,
    pub removed: usize,
}

/// Whether `c` is removed from shared text: a control character other than tab and line feed
/// (C0, DEL, C1 -- an ESC or a CSI starts a terminal's escape sequence), a bidirectional mark,
/// embedding, override or isolate, a Unicode tag character, an interlinear annotation, a byte-order
/// mark, or a non-character.
fn removed(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
        | '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0000}'..='\u{e007f}'
        | '\u{fdd0}'..='\u{fdef}')
        || (c as u32) & 0xfffe == 0xfffe
}

/// Clean guest text for the host's clipboard: line endings made `\n` (`\r\n`, a lone `\r`, and the
/// line and paragraph separators), the characters [`removed`] says dropped, and trailing line
/// breaks trimmed -- so a paste into a terminal does not press Enter.
///
/// # Errors
/// Text that is more than [`MAX_TEXT_BYTES`], or nothing once cleaned.
pub fn sanitize_text(raw: &str) -> Result<CleanText, String> {
    if raw.len() > MAX_TEXT_BYTES * 4 {
        return Err(format!("{} bytes (the most shared is {} MiB)", raw.len(), MAX_TEXT_BYTES >> 20));
    }
    let mut text = String::with_capacity(raw.len());
    let mut removed_count = 0;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                text.push('\n');
            }
            '\u{2028}' | '\u{2029}' => text.push('\n'),
            c if removed(c) => removed_count += 1,
            c => text.push(c),
        }
    }
    let trimmed = text.trim_end_matches(['\n', '\t', ' ']).len();
    if trimmed < text.len() && text[trimmed..].contains('\n') {
        text.truncate(trimmed);
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!("{} bytes (the most shared is {} MiB)", text.len(), MAX_TEXT_BYTES >> 20));
    }
    if text.trim().is_empty() {
        return Err("nothing left once control characters were removed".into());
    }
    Ok(CleanText { text, removed: removed_count })
}

/// An image decoded and encoded again: its pixels and a fresh PNG of them.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub png: Vec<u8>,
    /// The format it was in.
    pub format: &'static str,
}

/// Decode `bytes` -- PNG, JPEG, GIF (its first frame), WebP or BMP, told by their own signature --
/// within [`MAX_IMAGE_SIDE`] and [`MAX_IMAGE_ALLOC`], and encode the pixels as a new PNG.
///
/// # Errors
/// Any other format (what a copied script or executable is), an image too large, or bytes that do
/// not decode.
pub fn decode_image(bytes: &[u8]) -> Result<Decoded, String> {
    use image::ImageFormat;
    let format = image::guess_format(bytes).map_err(|_| "not an image (no image signature)".to_string())?;
    let name = match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WebP",
        ImageFormat::Bmp => "BMP",
        other => return Err(format!("{other:?} images are not shared (only PNG, JPEG, GIF, WebP and BMP)")),
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    let rgba = reader.decode().map_err(|e| format!("a {name} that does not decode: {e}"))?.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut png = Vec::new();
    image::ImageEncoder::write_image(image::codecs::png::PngEncoder::new(&mut png), rgba.as_raw(), width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("encoding the PNG: {e}"))?;
    Ok(Decoded { width, height, rgba: rgba.into_raw(), png, format: name })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parcel as Java's `Parcel` writes one.
    #[derive(Default)]
    struct W(Vec<u8>);

    impl W {
        fn i32(&mut self, v: i32) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn s8(&mut self, s: Option<&str>) -> &mut Self {
            match s {
                None => self.i32(-1),
                Some(s) => {
                    self.i32(s.len() as i32);
                    self.0.extend_from_slice(s.as_bytes());
                    self.0.push(0);
                    self.0.resize((self.0.len() + 3) & !3, 0);
                    self
                }
            }
        }
        fn s16(&mut self, s: &str) -> &mut Self {
            let units: Vec<u16> = s.encode_utf16().collect();
            self.i32(units.len() as i32);
            for u in units.iter().chain([&0]) {
                self.0.extend_from_slice(&u.to_le_bytes());
            }
            self.0.resize((self.0.len() + 3) & !3, 0);
            self
        }
        /// `writeNoException`, the non-null marker, and a `ClipDescription` with these types.
        fn description(&mut self, mimes: &[&str]) -> &mut Self {
            self.i32(0).i32(1);
            self.i32(1).s8(None); // label
            self.i32(mimes.len() as i32);
            for m in mimes {
                self.s16(m);
            }
            // Extras: a bundle of 8 bytes; the timestamp; styled; the status; an empty bundle.
            self.i32(8).i32(0x4C44_4E42).i32(7).i32(7);
            self.0.extend_from_slice(&1_700_000_000_000i64.to_le_bytes());
            self.i32(0).i32(1).i32(0)
        }
    }

    #[test]
    fn a_text_clip_is_read() {
        let mut w = W::default();
        w.description(&["text/plain"]).i32(0).i32(1);
        w.i32(1).s8(Some("héllo")).s8(None).i32(0).i32(0).i32(0).i32(0).i32(0);
        let clip = parse_clip_reply(&w.0).expect("parsed").expect("a clip");
        assert_eq!(clip, Clip { mime_types: vec!["text/plain".into()], text: Some("héllo".into()), uri: None });
    }

    #[test]
    fn the_hosts_text_clip_reads_back_as_android_writes_one() {
        // getPrimaryClip's reply is the same ClipData setPrimaryClip carries: written by the
        // host, read by the reader checked against Android's own.
        let mut p = Parcel { bytes: Vec::new() };
        p.i32(0);
        p.i32(1);
        write_text_clip(&mut p, "kopyalandı ✓");
        let clip = parse_clip_reply(&p.bytes).expect("parsed").expect("a clip");
        assert_eq!(clip, Clip { mime_types: vec!["text/plain".into()], text: Some("kopyalandı ✓".into()), uri: None });
    }

    #[test]
    fn styled_text_is_read_without_its_spans() {
        let mut w = W::default();
        w.description(&["text/plain"]).i32(0).i32(1);
        w.i32(0).s8(Some("bold")).i32(5).i32(99); // a span that is not read
        assert_eq!(parse_clip_reply(&w.0).unwrap().unwrap().text.as_deref(), Some("bold"));
    }

    #[test]
    fn an_image_clip_gives_its_uri() {
        for ty in [1, 2, 3] {
            let mut w = W::default();
            w.description(&["image/png"]).i32(0).i32(1);
            w.i32(1).s8(None).s8(None).i32(0).i32(0).i32(1).i32(ty).s8(Some("content://com.example.fileprovider/images/a.png"));
            let clip = parse_clip_reply(&w.0).unwrap().unwrap();
            assert_eq!(clip.text, None);
            assert_eq!(clip.uri.as_deref(), Some("content://com.example.fileprovider/images/a.png"), "Uri type {ty}");
            assert_eq!(clip.mime_types, vec!["image/png".to_string()]);
        }
    }

    #[test]
    fn no_clip_an_exception_and_a_short_parcel() {
        assert_eq!(parse_clip_reply(&W::default().i32(0).i32(0).0), Ok(None));
        assert!(parse_clip_reply(&W::default().i32(-1).0).is_err(), "SecurityException");
        let mut w = W::default();
        w.description(&["image/png"]).i32(0).i32(1).i32(1);
        assert!(parse_clip_reply(&w.0).is_err(), "ends inside the item");
        let mut huge = W::default();
        huge.i32(0).i32(1).i32(1).s8(None).i32(i32::MAX);
        assert!(parse_clip_reply(&huge.0).is_err(), "a count past the parcel");
    }

    #[test]
    fn a_strictmode_header_is_skipped() {
        let mut w = W::default();
        w.i32(-128).i32(12).i32(0).i32(0); // a 12-byte header (its size word included)
        let mut body = W::default();
        body.description(&["text/plain"]);
        w.0.extend_from_slice(&body.0[4..]); // without its own exception code
        w.i32(0).i32(1).i32(1).s8(Some("x")).s8(None).i32(0).i32(0).i32(0).i32(0).i32(0);
        assert_eq!(parse_clip_reply(&w.0).unwrap().unwrap().text.as_deref(), Some("x"));
    }

    #[test]
    fn terminal_escapes_and_invisible_characters_are_removed() {
        let raw = "ls\u{1b}[2J\u{1b}]0;title\u{7}\u{9b}31m ok\u{0}";
        let clean = sanitize_text(raw).unwrap();
        assert_eq!(clean.text, "ls[2J]0;title31m ok");
        assert_eq!(clean.removed, 5);
        // Trojan Source: an RLO and isolates; tag characters (invisible ASCII); a BOM.
        let clean = sanitize_text("\u{feff}if a\u{202e} } \u{2066}x\u{2069}\u{e0041}\u{e0042}").unwrap();
        assert_eq!(clean.text, "if a } x");
        assert_eq!(clean.removed, 6);
        // What is kept: tabs, line feeds, emoji with joiners, other scripts.
        let kept = "a\tb\nc 👩\u{200d}💻 مرحبا 日本語";
        assert_eq!(sanitize_text(kept).unwrap(), CleanText { text: kept.into(), removed: 0 });
    }

    #[test]
    fn line_endings_are_line_feeds_and_a_trailing_enter_is_dropped() {
        assert_eq!(sanitize_text("rm -rf ~\n").unwrap().text, "rm -rf ~");
        assert_eq!(sanitize_text("a\r\nb\rc\u{2028}d\r\n\r\n  ").unwrap().text, "a\nb\nc\nd");
        assert_eq!(sanitize_text("keep  ").unwrap().text, "keep  ", "trailing spaces with no line break are the text's");
    }

    #[test]
    fn nothing_or_too_much_is_not_shared() {
        assert!(sanitize_text("\u{1b}\u{7}\n").is_err());
        assert!(sanitize_text(&"a".repeat(MAX_TEXT_BYTES + 1)).is_err());
        assert!(sanitize_text(&"a".repeat(MAX_TEXT_BYTES)).is_ok());
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let pixels: Vec<u8> = (0..width * height).flat_map(|i| [i as u8, 0x40, 0x80, 0xff]).collect();
        let mut out = Vec::new();
        image::ImageEncoder::write_image(image::codecs::png::PngEncoder::new(&mut out), &pixels, width, height, image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    #[test]
    fn an_image_is_decoded_and_made_a_new_png() {
        let mut file = png(3, 2);
        // Bytes after the image (an archive appended to it, say) do not reach the host.
        file.extend_from_slice(b"PK\x03\x04 a zip appended");
        let d = decode_image(&file).expect("decoded");
        assert_eq!((d.width, d.height, d.format), (3, 2, "PNG"));
        assert_eq!(&d.rgba[0..8], &[0, 0x40, 0x80, 0xff, 1, 0x40, 0x80, 0xff]);
        assert!(!d.png.windows(4).any(|w| w == b"PK\x03\x04"), "the PNG is made from the pixels");
        assert_eq!(decode_image(&d.png).unwrap().rgba, d.rgba);
    }

    #[test]
    fn scripts_executables_and_other_files_are_not_images() {
        for bytes in [&b"#!/bin/sh\ncurl x | sh\n"[..], b"MZ\x90\x00\x03", b"\x7fELF\x02\x01\x01", b"\xcf\xfa\xed\xfe", b"PK\x03\x04", b"%PDF-1.7", b"<svg onload=alert(1)>"] {
            assert!(decode_image(bytes).is_err(), "{:?}", String::from_utf8_lossy(bytes));
        }
        // An image signature with nothing behind it.
        assert!(decode_image(b"\x89PNG\r\n\x1a\n").is_err());
    }

    #[test]
    fn an_image_past_the_size_limit_is_refused() {
        // A PNG header that claims 20000x1: refused by the decoder's limits before any pixel.
        let mut file = png(1, 1);
        file[16..20].copy_from_slice(&20_000u32.to_be_bytes());
        let ihdr_crc = crc(&file[12..29]);
        file[29..33].copy_from_slice(&ihdr_crc.to_be_bytes());
        assert!(decode_image(&file).is_err());
    }

    fn crc(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    #[test]
    fn a_copy_is_shared_only_while_a_window_was_focused_lately() {
        FOCUSED_AT.store(0, Ordering::Relaxed);
        assert!(!window_in_use(), "never focused");
        window_focus(true);
        assert!(window_in_use());
        FOCUSED_AT.store(now_ms().saturating_sub(FOCUS_GRACE.as_millis() as u64 + 500).max(1), Ordering::Relaxed);
        assert!(!window_in_use(), "focused long ago");
        window_focus(false);
        assert!(!window_in_use(), "an unfocused window does not count");
    }
}
