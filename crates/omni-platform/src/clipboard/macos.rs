//! The clipboard seam on macOS: the general `NSPasteboard`.
//!
//! An image is written as PNG and as TIFF (what `NSImage` makes of the PNG): AppKit applications
//! read TIFF first, browsers and most others PNG. `NSPasteboard` may be used from any thread.
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_app_kit::{NSImage, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF};
use objc2_foundation::{NSData, NSString};

use super::{ClipboardError, ClipboardResult, Image};

fn failed(operation: &'static str, api: &'static str) -> ClipboardError {
    ClipboardError::Os { operation, api, code: 0 }
}

pub(super) fn set_text(text: &str) -> ClipboardResult<()> {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    // SAFETY: an AppKit constant, a static `NSString` for the life of the process.
    let kind = unsafe { NSPasteboardTypeString };
    if !board.setString_forType(&NSString::from_str(text), kind) {
        return Err(failed("set_text", "-[NSPasteboard setString:forType:]"));
    }
    Ok(())
}

pub(super) fn get_text() -> ClipboardResult<Option<String>> {
    let board = NSPasteboard::generalPasteboard();
    // SAFETY: an AppKit constant, a static `NSString` for the life of the process.
    let kind = unsafe { NSPasteboardTypeString };
    Ok(board.stringForType(kind).map(|s| s.to_string()))
}

pub(super) fn set_image(image: &Image<'_>) -> ClipboardResult<()> {
    let png = NSData::with_bytes(image.png);
    let tiff: Option<Retained<NSData>> = NSImage::initWithData(NSImage::alloc(), &png).and_then(|i| i.TIFFRepresentation());
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    // SAFETY: AppKit constants, static `NSString`s for the life of the process.
    let (png_kind, tiff_kind) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    if !board.setData_forType(Some(&png), png_kind) {
        return Err(failed("set_image", "-[NSPasteboard setData:forType:] (PNG)"));
    }
    if let Some(tiff) = tiff {
        if !board.setData_forType(Some(&tiff), tiff_kind) {
            return Err(failed("set_image", "-[NSPasteboard setData:forType:] (TIFF)"));
        }
    }
    Ok(())
}
