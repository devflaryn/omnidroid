//! **Frames as `IOSurface`s** (`present_rgba`/`present_bgra`): the frame's pixels are copied once,
//! on the presenting thread, into an `IOSurface` the window server composites from as it is -- the
//! layer's contents -- instead of a `CGImage` the main thread copies (`CFDataCreate`) and Core
//! Animation converts, pixel by pixel on the CPU, from device RGB to the display's colour space.
//!
//! MEASURED (M4, two 1280x720 game windows at ~55 fps each, `thcpu`): the `CGImage` path held the
//! AppKit thread at 74% of a core (`vImageConvert_AnyToAny`, `CGBlt_copyBytes`, `CFDataCreate`).
//! An `IOSurface` tagged sRGB is converted to the display's space by the GPU, while compositing.
//!
//! The surfaces are 32-bit BGRA (Core Animation's own format for layer contents); RGBA rows are
//! swizzled as they are copied, and alpha is written opaque (the old path's `kCGImageAlphaNoneSkipLast`).
//! Each window keeps a few, in turn: a surface the window server still reads (`IOSurfaceIsInUse`)
//! is not written, so a frame never changes under the compositor.
use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type CFTypeRef = *const c_void;
type IOSurfaceRef = *mut c_void;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: CFTypeRef;
    static kIOSurfaceHeight: CFTypeRef;
    static kIOSurfaceBytesPerElement: CFTypeRef;
    static kIOSurfacePixelFormat: CFTypeRef;
    static kIOSurfaceColorSpace: CFTypeRef;
    fn IOSurfaceCreate(properties: CFTypeRef) -> IOSurfaceRef;
    fn IOSurfaceLock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: IOSurfaceRef) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: IOSurfaceRef) -> usize;
    fn IOSurfaceIsInUse(surface: IOSurfaceRef) -> bool;
    fn IOSurfaceSetValue(surface: IOSurfaceRef, key: CFTypeRef, value: CFTypeRef);
}

#[repr(C)]
struct CFDictionaryCallBacks([usize; 6]);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
    fn CFNumberCreate(allocator: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const CFDictionaryCallBacks,
        value_callbacks: *const CFDictionaryCallBacks,
    ) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
}

/// A reference [`Surface::retained_pending`] took, given back.
pub(super) fn release(surface: usize) {
    // SAFETY: a reference `retained_pending` took.
    unsafe { CFRelease(surface as CFTypeRef) };
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: CFTypeRef;
    fn CGColorSpaceCreateWithName(name: CFTypeRef) -> *mut c_void;
    fn CGColorSpaceCopyPropertyList(space: *mut c_void) -> CFTypeRef;
    fn CGColorSpaceRelease(space: *mut c_void);
}

/// `kCFNumberSInt32Type`.
const SINT32: isize = 3;
/// `'BGRA'`: `kCVPixelFormatType_32BGRA`.
const BGRA: i32 = 0x4247_5241;
/// Surfaces a window keeps at most: one shown, one the compositor may still read, one written.
const MOST: usize = 4;

/// One `IOSurface`, released when dropped; and whether a show of it is queued and not done yet
/// (`present_fill`), during which it is not written again.
pub(super) struct Surface(IOSurfaceRef, Arc<std::sync::atomic::AtomicBool>);

// SAFETY: an `IOSurfaceRef` is a Core Foundation object, usable from any thread; its pixels are
// written only between `IOSurfaceLock` and `IOSurfaceUnlock`, by the one holder of the ring's lock.
unsafe impl Send for Surface {}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: created by `IOSurfaceCreate` (+1), released once.
        unsafe { CFRelease(self.0) };
    }
}

impl Surface {
    /// The surface as an Objective-C object, for `-[CALayer setContents:]` (which retains it).
    pub(super) fn object(&self) -> *mut c_void {
        self.0
    }

    /// The surface retained (+1, for a show queued on another thread, which releases it with
    /// [`release`]), and its pending flag, set.
    pub(super) fn retained_pending(&self) -> (usize, Arc<std::sync::atomic::AtomicBool>) {
        self.1.store(true, std::sync::atomic::Ordering::Release);
        // SAFETY: a live surface; the reference taken is released by `release`.
        unsafe { CFRetain(self.0) };
        (self.0 as usize, Arc::clone(&self.1))
    }

    fn new(width: u32, height: u32) -> Option<Self> {
        // SAFETY: plain Core Foundation constructors; every object made here is released below
        // (the dictionary holds its own references to the numbers).
        unsafe {
            let number = |v: i32| CFNumberCreate(core::ptr::null(), SINT32, (&raw const v).cast());
            let values = [number(width as i32), number(height as i32), number(4), number(BGRA)];
            let keys = [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat];
            let props = CFDictionaryCreate(
                core::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as isize,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            for v in values {
                if !v.is_null() {
                    CFRelease(v);
                }
            }
            if props.is_null() {
                return None;
            }
            let surface = IOSurfaceCreate(props);
            CFRelease(props);
            if surface.is_null() {
                return None;
            }
            // Android draws sRGB: the compositor converts it to the display's space on the GPU.
            let space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            if !space.is_null() {
                let list = CGColorSpaceCopyPropertyList(space);
                if !list.is_null() {
                    IOSurfaceSetValue(surface, kIOSurfaceColorSpace, list);
                    CFRelease(list);
                }
                CGColorSpaceRelease(space);
            }
            Some(Self(surface, Arc::default()))
        }
    }

    /// `fill` writes the surface's rows (BGRA, `stride` bytes apart) while it is locked.
    fn fill_with(&self, height: usize, fill: &mut dyn FnMut(&mut [u8], usize)) -> bool {
        // SAFETY: locked for writing while `fill` has its memory: `height` rows of `stride` bytes.
        unsafe {
            if IOSurfaceLock(self.0, 0, core::ptr::null_mut()) != 0 {
                return false;
            }
            let stride = IOSurfaceGetBytesPerRow(self.0);
            let base = IOSurfaceGetBaseAddress(self.0).cast::<u8>();
            fill(core::slice::from_raw_parts_mut(base, stride * height), stride);
            IOSurfaceUnlock(self.0, 0, core::ptr::null_mut());
        }
        true
    }

    /// Copy `pixels` (`width` x `height`, four bytes a pixel, BGRA when `bgra` else RGBA) in, opaque.
    fn fill(&self, pixels: &[u8], width: usize, height: usize, bgra: bool) -> bool {
        // SAFETY: the surface is `width` x `height` x 4 (made so), locked for writing while its
        // rows are written, each within `bytes_per_row`; `pixels` holds `width * height * 4` (the
        // seam checked).
        unsafe {
            if IOSurfaceLock(self.0, 0, core::ptr::null_mut()) != 0 {
                return false;
            }
            let base = IOSurfaceGetBaseAddress(self.0).cast::<u8>();
            let stride = IOSurfaceGetBytesPerRow(self.0);
            let row = width * 4;
            for y in 0..height {
                let src = &pixels[y * row..y * row + row];
                let dst = core::slice::from_raw_parts_mut(base.add(y * stride), row);
                copy_row(src, dst, bgra);
            }
            IOSurfaceUnlock(self.0, 0, core::ptr::null_mut());
        }
        true
    }
}

/// One row to BGRA with alpha opaque: as it is (BGRA), or with red and blue swapped (RGBA).
fn copy_row(src: &[u8], dst: &mut [u8], bgra: bool) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let v = if bgra { v } else { (v & 0x0000_FF00) | ((v & 0xFF) << 16) | ((v >> 16) & 0xFF) };
        d.copy_from_slice(&(v | 0xFF00_0000).to_le_bytes());
    }
}

/// A window's surfaces: their size, and the one shown last (`surfaces[shown]`).
struct Ring {
    size: (u32, u32),
    surfaces: Vec<Surface>,
    shown: usize,
}

/// Every window's surfaces, by window id (each its own lock: windows present side by side).
static RINGS: Mutex<Option<HashMap<u64, Arc<Mutex<Ring>>>>> = Mutex::new(None);

/// Copy a frame into a surface of window `id`'s that the compositor is not reading, and hand it to
/// `show` (which sets it as the layer's contents, on the AppKit thread). `None`: no surface could
/// be made or locked -- the caller presents the old way.
pub(super) fn present<R>(id: u64, pixels: &[u8], width: u32, height: u32, bgra: bool, show: impl FnOnce(&Surface) -> R) -> Option<R> {
    let ring = {
        let mut rings = RINGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring = rings.get_or_insert_with(HashMap::new).entry(id).or_insert_with(|| Arc::new(Mutex::new(Ring { size: (width, height), surfaces: Vec::new(), shown: 0 })));
        Arc::clone(ring)
    };
    let mut ring = ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if ring.size != (width, height) {
        ring.surfaces.clear();
        ring.size = (width, height);
        ring.shown = 0;
    }
    let at = pick(&mut ring, width, height)?;
    if !ring.surfaces[at].fill(pixels, width as usize, height as usize, bgra) {
        return None;
    }
    ring.shown = at;
    Some(show(&ring.surfaces[at]))
}

/// A surface of `ring` to write the next frame into: one the window server is not reading and no
/// queued show holds, else a new one (up to [`MOST`]), else the oldest in turn.
fn pick(ring: &mut Ring, width: u32, height: u32) -> Option<usize> {
    let shown = ring.shown;
    let free = (0..ring.surfaces.len()).find(|&i| {
        // SAFETY: live surfaces of this ring.
        i != shown && !ring.surfaces[i].1.load(std::sync::atomic::Ordering::Acquire) && !unsafe { IOSurfaceIsInUse(ring.surfaces[i].0) }
    });
    Some(match free {
        Some(i) => i,
        None if ring.surfaces.len() < MOST => {
            ring.surfaces.push(Surface::new(width, height)?);
            ring.surfaces.len() - 1
        }
        // All still read: the one after the shown one, the oldest in turn.
        None => (shown + 1) % ring.surfaces.len(),
    })
}

/// **A frame written straight into window `id`'s surface** by `fill` (BGRA rows, `stride` bytes
/// apart, every pixel written; alpha ignored), then handed to `show`. `None`: no surface could be
/// had -- the caller presents another way.
pub(super) fn present_fill<R>(id: u64, width: u32, height: u32, fill: &mut dyn FnMut(&mut [u8], usize), show: impl FnOnce(&Surface) -> R) -> Option<R> {
    let ring = {
        let mut rings = RINGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring = rings.get_or_insert_with(HashMap::new).entry(id).or_insert_with(|| Arc::new(Mutex::new(Ring { size: (width, height), surfaces: Vec::new(), shown: 0 })));
        Arc::clone(ring)
    };
    let mut ring = ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if ring.size != (width, height) {
        ring.surfaces.clear();
        ring.size = (width, height);
        ring.shown = 0;
    }
    let at = pick(&mut ring, width, height)?;
    if !ring.surfaces[at].fill_with(height as usize, fill) {
        return None;
    }
    ring.shown = at;
    Some(show(&ring.surfaces[at]))
}

/// Window `id` is closed: its surfaces go.
pub(super) fn forget(id: u64) {
    let mut rings = RINGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(rings) = rings.as_mut() {
        rings.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_becomes_opaque_bgra() {
        let mut out = [0u8; 8];
        copy_row(&[1, 2, 3, 4, 5, 6, 7, 8], &mut out, false);
        assert_eq!(out, [3, 2, 1, 255, 7, 6, 5, 255], "RGBA swizzled");
        copy_row(&[1, 2, 3, 4, 5, 6, 7, 8], &mut out, true);
        assert_eq!(out, [1, 2, 3, 255, 5, 6, 7, 255], "BGRA as it is");
    }

    #[test]
    fn a_surface_is_made_and_filled() {
        let s = Surface::new(3, 2).expect("an IOSurface");
        assert!(s.fill(&[9u8; 3 * 2 * 4], 3, 2, false));
    }
}
