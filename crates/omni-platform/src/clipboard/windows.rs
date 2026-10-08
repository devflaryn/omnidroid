//! The clipboard seam on Windows: the Win32 clipboard.
//!
//! Text is `CF_UNICODETEXT` (UTF-16, NUL-terminated, lines ending `\r\n`). An image is `CF_DIB` --
//! a `BITMAPV5HEADER` and 32-bit BGRA rows, bottom-up, with the alpha mask said -- which every
//! application reads, and the registered `PNG` format beside it, which browsers and Office prefer
//! (it keeps the alpha channel exactly). Each is moveable global memory whose ownership passes to
//! the clipboard once `SetClipboardData` takes it.
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

use super::{ClipboardError, ClipboardResult, Image};

/// `CF_DIB` and `CF_UNICODETEXT` (`winuser.h`).
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;

fn os_error(operation: &'static str, api: &'static str) -> ClipboardError {
    // SAFETY: reads the calling thread's last-error value.
    ClipboardError::Os { operation, api, code: i64::from(unsafe { GetLastError() }) }
}

/// The clipboard, open on this thread until dropped.
struct Open;

impl Open {
    /// Open it, waiting a little while another application holds it (as clipboard managers do for
    /// a moment after each change).
    fn new(operation: &'static str) -> ClipboardResult<Self> {
        for _ in 0..20 {
            // SAFETY: no owner window: the clipboard is opened for this task.
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Self);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        Err(os_error(operation, "OpenClipboard"))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: opened by `Open::new` on this thread.
        unsafe { CloseClipboard() };
    }
}

/// Hand `bytes` to the clipboard as `format`.
fn put(operation: &'static str, format: u32, bytes: &[u8]) -> ClipboardResult<()> {
    // SAFETY: a fresh moveable block of `bytes.len()` bytes, filled through its lock and unlocked
    // before the clipboard takes it; freed here only when the clipboard did not.
    unsafe {
        let block = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if block.is_null() {
            return Err(os_error(operation, "GlobalAlloc"));
        }
        let at = GlobalLock(block).cast::<u8>();
        if at.is_null() {
            let error = os_error(operation, "GlobalLock");
            GlobalFree(block);
            return Err(error);
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        GlobalUnlock(block);
        if SetClipboardData(format, block as HANDLE).is_null() {
            let error = os_error(operation, "SetClipboardData");
            GlobalFree(block);
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn set_text(text: &str) -> ClipboardResult<()> {
    let windows_lines = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let units: Vec<u16> = windows_lines.encode_utf16().chain([0]).collect();
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    let _open = Open::new("set_text")?;
    // SAFETY: the clipboard is open on this thread.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(os_error("set_text", "EmptyClipboard"));
    }
    put("set_text", CF_UNICODETEXT, &bytes)
}

pub(super) fn get_text() -> ClipboardResult<Option<String>> {
    let _open = Open::new("get_text")?;
    // SAFETY: the clipboard is open on this thread; the block it hands out is the clipboard's,
    // read through its lock within its size and unlocked before the clipboard is closed.
    unsafe {
        let block = GetClipboardData(CF_UNICODETEXT);
        if block.is_null() {
            return Ok(None);
        }
        let at = GlobalLock(block).cast::<u16>();
        if at.is_null() {
            return Err(os_error("get_text", "GlobalLock"));
        }
        let units = std::slice::from_raw_parts(at, GlobalSize(block) / 2);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let text = String::from_utf16_lossy(&units[..end]);
        GlobalUnlock(block);
        Ok(Some(text))
    }
}

/// `CF_DIB`'s bytes: a `BITMAPV5HEADER` (124 bytes; BI_BITFIELDS with an alpha mask, sRGB) and
/// the pixels as BGRA, bottom row first.
fn dib(image: &Image<'_>) -> Vec<u8> {
    let (w, h) = (image.width as usize, image.height as usize);
    let mut out = Vec::with_capacity(124 + w * h * 4);
    let i32le = |out: &mut Vec<u8>, v: i32| out.extend_from_slice(&v.to_le_bytes());
    let u32le = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    u32le(&mut out, 124); // bV5Size
    i32le(&mut out, image.width as i32);
    i32le(&mut out, image.height as i32); // positive: bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    u32le(&mut out, 3); // BI_BITFIELDS
    u32le(&mut out, (w * h * 4) as u32);
    i32le(&mut out, 2835); // 72 dpi
    i32le(&mut out, 2835);
    u32le(&mut out, 0); // colours used
    u32le(&mut out, 0); // important
    u32le(&mut out, 0x00ff_0000); // red mask
    u32le(&mut out, 0x0000_ff00); // green
    u32le(&mut out, 0x0000_00ff); // blue
    u32le(&mut out, 0xff00_0000); // alpha
    u32le(&mut out, u32::from_be_bytes(*b"sRGB")); // LCS_sRGB
    out.resize(124, 0); // endpoints, gamma, intent, profile: none
    for row in image.rgba.chunks_exact(w * 4).rev() {
        for p in row.chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

pub(super) fn set_image(image: &Image<'_>) -> ClipboardResult<()> {
    let dib = dib(image);
    let name: Vec<u16> = "PNG".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated UTF-16 name.
    let png_format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    let _open = Open::new("set_image")?;
    // SAFETY: the clipboard is open on this thread.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(os_error("set_image", "EmptyClipboard"));
    }
    put("set_image", CF_DIB, &dib)?;
    if png_format != 0 {
        put("set_image", png_format, image.png)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dib_is_its_header_and_bgra_rows_bottom_up() {
        // Two rows: red over blue.
        let rgba = [255, 0, 0, 255, 0, 0, 255, 128];
        let d = dib(&Image { width: 1, height: 2, rgba: &rgba, png: &[] });
        assert_eq!(d.len(), 124 + 8);
        assert_eq!(&d[0..4], &124u32.to_le_bytes());
        assert_eq!(&d[124..], &[255, 0, 0, 128, 0, 0, 255, 255], "blue (bottom) first, as BGRA");
    }
}
