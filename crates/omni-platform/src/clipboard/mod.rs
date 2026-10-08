//! The host's clipboard: putting text or an image on it, for the guest's clipboard shared with the
//! host (`omni-linux`'s `clipboard`).
//!
//! # Only inert data
//!
//! This seam writes **two kinds of thing and nothing else**: plain text (the host's plain-text
//! type: `NSPasteboardTypeString`, `CF_UNICODETEXT`, `UTF8_STRING`/`text/plain`) and a bitmap
//! image (`public.png` and `public.tiff`, `CF_DIB` and the registered `PNG` format, `image/png`).
//! There is deliberately **no** call for a file list (`public.file-url`, `CF_HDROP`,
//! `text/uri-list`), HTML, RTF or any other rich type: those are the clipboard types a paste can
//! turn into a file dropped on the host, a link opened, or markup with script in it. What the guest
//! copies reaches the host as characters or pixels, which pasting cannot run.
//!
//! The caller has already cleaned what it hands over (`omni-linux` strips control and invisible
//! characters from text and re-encodes images from decoded pixels); this seam only writes it.
//!
//! ```text
//! set_text(&str) -> ClipboardResult<()>
//! set_image(&Image) -> ClipboardResult<()>
//! get_text() -> ClipboardResult<Option<String>>
//! ```
//!
//! [`get_text`] reads the host's plain text the other way, for a host copy pasted in the guest:
//! only the plain-text type, never a file list or a rich type.
//!
//! macOS: `NSPasteboard`'s general pasteboard. Windows: the Win32 clipboard. Linux and other unix:
//! the desktop's own clipboard tool -- `wl-copy` under Wayland, else `xclip` or `xsel` under X11 --
//! run with fixed arguments and the data on its standard input (never a shell). An X11 or Wayland
//! selection has to be *served* by a process that stays alive to answer each paste; those tools
//! fork one that does, so the clipboard keeps the copy after this process is gone, as it does on
//! the other hosts.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(all(unix, not(target_os = "macos")))]
mod unix;
#[cfg(all(unix, not(target_os = "macos")))]
use unix as backend;

/// Result alias for the clipboard seam.
pub type ClipboardResult<T> = Result<T, ClipboardError>;

/// What putting something on the host's clipboard can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClipboardError {
    /// A host call failed (`code`: the OS's error code, `GetLastError` on Windows; 0 where the
    /// call reports only failure, as `NSPasteboard`'s setters do).
    #[error("`{operation}`: {api} failed (code {code})")]
    Os {
        /// The seam operation that was called.
        operation: &'static str,
        /// The host entry point that failed.
        api: &'static str,
        /// The host's error code.
        code: i64,
    },
    /// No way to reach the clipboard on this host: on Linux, no `wl-copy`, `xclip` or `xsel` for
    /// the session's display, or no display at all. `detail` says what was looked for.
    #[error("no host clipboard: {detail}")]
    Unavailable {
        /// What was looked for and what was found.
        detail: String,
    },
    /// The image handed over does not describe itself: `rgba` is not `width * height * 4` bytes, or
    /// a side is 0.
    #[error("an image of {width}x{height} with {len} bytes of pixels")]
    BadImage {
        /// Its width.
        width: u32,
        /// Its height.
        height: u32,
        /// The length of its pixels.
        len: usize,
    },
}

/// An image for the clipboard, in the two forms hosts take one in: its pixels (straight RGBA, 8
/// bits a channel, rows top to bottom) and the same pixels as a PNG.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: &'a [u8],
    /// The same image, encoded as a PNG.
    pub png: &'a [u8],
}

/// Replace what the host's clipboard holds with `text`, as plain text (lines end in `\n`; the
/// Windows backend writes `\r\n`).
///
/// # Errors
/// [`ClipboardError`]: the host call that failed, or no clipboard to reach.
pub fn set_text(text: &str) -> ClipboardResult<()> {
    backend::set_text(text)
}

/// The host clipboard's plain text, if it holds any (lines end in `\n`).
///
/// # Errors
/// [`ClipboardError`]: the host call that failed, or no clipboard to reach.
pub fn get_text() -> ClipboardResult<Option<String>> {
    backend::get_text().map(|t| t.map(|t| t.replace("\r\n", "\n")))
}

/// Replace what the host's clipboard holds with `image`.
///
/// # Errors
/// [`ClipboardError::BadImage`] for an image whose pixels do not match its size; otherwise the host
/// call that failed, or no clipboard to reach.
pub fn set_image(image: &Image<'_>) -> ClipboardResult<()> {
    let want = (image.width as usize).checked_mul(image.height as usize).and_then(|n| n.checked_mul(4));
    if image.width == 0 || image.height == 0 || want != Some(image.rgba.len()) {
        return Err(ClipboardError::BadImage { width: image.width, height: image.height, len: image.rgba.len() });
    }
    backend::set_image(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_whose_pixels_do_not_match_its_size_is_refused_before_the_host_is_called() {
        let rgba = [0u8; 12];
        for (width, height) in [(0, 3), (3, 0), (2, 2), (4, 1)] {
            let image = Image { width, height, rgba: &rgba, png: &[] };
            assert_eq!(set_image(&image), Err(ClipboardError::BadImage { width, height, len: 12 }), "{width}x{height}");
        }
    }
}
