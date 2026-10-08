//! The composer's own composition (`DEVICE`): the layers of a frame blended straight into the
//! display's pixels on the host CPU, so SurfaceFlinger's RenderEngine -- GLES on ANGLE on the
//! paravirtual Vulkan driver, every command a forwarded call -- draws nothing. What it takes: RGBA
//! (or BGRA) buffers, their source crop scaled to the display frame (nearest pixel) under any of
//! the eight HWC transforms (flips and quarter turns), blending `NONE`, `PREMULTIPLIED` or
//! `COVERAGE` with a plane alpha; and solid colours. Anything else the composer leaves to
//! SurfaceFlinger (`crate::hal::composer`).
//!
//! Roblox in a world ran at 1.65 composed frames a second with SurfaceFlinger composing every frame
//! (r16: its own process at 1.37 cores, waiting; the system's at 3.97).

/// How a layer's pixels combine with what is under them (`BlendMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// Opaque: the layer replaces what is under it.
    None,
    /// `dst = src * alpha + dst * (1 - src.a * alpha)`, the source's colour already premultiplied.
    Premultiplied,
    /// `dst = src * src.a * alpha + dst * (1 - src.a * alpha)`.
    Coverage,
}

/// One layer to compose.
pub enum Source<'a> {
    /// RGBA rows `stride` pixels apart; `opaque` for a format with no alpha (RGBX): its alpha is 1.
    /// The pixels shown are the crop's, at 1:1 from its corner.
    Pixels { data: &'a [u8], stride: usize, opaque: bool, crop_x: usize, crop_y: usize },
    /// The same, but the crop (left, top, right, bottom, in source pixels) scaled to the frame
    /// under `transform` (HWC's bits: 1 flip horizontally, 2 flip vertically, 4 turn a quarter
    /// clockwise, applied in that order); `bgra` for red and blue swapped.
    Mapped { data: &'a [u8], stride: usize, rows: usize, opaque: bool, bgra: bool, crop: (f32, f32, f32, f32), transform: u32 },
    /// One colour, RGBA in 0..=1.
    Color([f32; 4]),
}

pub struct Layer<'a> {
    pub source: Source<'a>,
    /// Where on the display: left, top, right, bottom (may reach past the display; clipped).
    pub frame: (i32, i32, i32, i32),
    pub blend: Blend,
    pub alpha: f32,
}

/// **The fast path** (`omni_linux::lever`'s `compose_fast=0|1`; **on by default** since 2026-10-07 --
/// 7.04 -> 1.15 ms a frame at 1280x720, 16 of 16 pairs (docs/NIGHT-2026-10-02.md), and ~5x the
/// pixels at a Retina window's size, all of it inside SurfaceFlinger's present; `OMNI_COMPOSE_FAST=0`
/// starts with it off):
/// fewer passes -- black written once rather than zeroed then given its alpha, and a premultiplied
/// layer's fully transparent pixels skipped and its fully opaque ones copied rather than blended
/// (an app window over its SurfaceView is mostly a transparent hole: ~921,600 blends a frame at
/// 1280x720, four integer divisions each). Every shortcut is the blend's own result for that pixel,
/// so the output is identical (`tests::the_fast_path_composes_the_same_pixels`).
pub static FAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// **Zero-copy composition** (`omni_linux::lever`'s `compose_zero=0|1`, `OMNI_COMPOSE_ZERO=1` from
/// the start; **off by default**). Every frame the composer read each layer's whole gralloc
/// buffer into a buffer of its own (5.6 MB a layer at 1575x890, two in a world: the SurfaceView
/// and the app's window over it), composed into a third, and the framebuffer copied that into a
/// fourth. With this on, the layers are read where they are -- borrowed from the regions' host
/// views (`crate::shm::Shm::bytes`), after the release's copy has landed as before -- and composed
/// straight into the framebuffer's next frame ([`super::framebuffer::Framebuffer::present_with`]),
/// by [`compose_into`]'s run path: no black fill under a layer that covers the display (a
/// SurfaceView does), layers under an opaque one skipped, and a premultiplied layer taken four
/// pixels at a time -- four transparent pixels skipped and four opaque ones copied with one test
/// each. The pixels are the same (`tests::the_run_path_composes_the_same_pixels`).
pub static ZERO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// **The frame composed as BGRA** (`present_bgra=0|1`, `OMNI_PRESENT_BGRA=1`; **off by default**):
/// the Win32 window paints GDI's BGRA, and it swizzled every frame into its canvas under the
/// canvas's lock (5.6 MB a frame at 1575x890). The blend is the same arithmetic on each colour
/// channel, so composing with red and blue swapped from the start costs nothing, and the window then
/// takes the frame as it is -- shared, not copied (`omni_platform::window::Presenter::present_bgra`).
/// Screenshots and every other reader of the framebuffer still get RGBA.
pub static BGRA_OUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Read `OMNI_COMPOSE_FAST`, `OMNI_COMPOSE_ZERO` and `OMNI_PRESENT_BGRA` once.
pub fn levers_from_env() {
    static FROM_ENV: std::sync::Once = std::sync::Once::new();
    FROM_ENV.call_once(|| {
        use std::sync::atomic::Ordering::Relaxed;
        if std::env::var("OMNI_COMPOSE_FAST").as_deref() == Ok("0") {
            FAST.store(false, Relaxed);
        }
        if std::env::var("OMNI_COMPOSE_ZERO").as_deref() == Ok("1") {
            ZERO.store(true, Relaxed);
        }
        if std::env::var("OMNI_PRESENT_BGRA").as_deref() == Ok("1") {
            BGRA_OUT.store(true, Relaxed);
        }
    });
}

/// How [`compose_into`] composes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Opts {
    /// [`FAST`]'s shortcuts.
    pub fast: bool,
    /// The run path ([`ZERO`]): no fill under a covering layer, layers under an opaque one
    /// skipped, premultiplied rows four pixels at a time. Same pixels as without.
    pub runs: bool,
    /// The output's red and blue swapped (BGRA, [`BGRA_OUT`]).
    pub bgra: bool,
}

/// Compose `layers` (bottom first) over black into `out`: RGBA rows of `width` pixels.
pub fn compose(out: &mut [u8], width: usize, height: usize, layers: &[Layer<'_>]) {
    levers_from_env();
    compose_with(out, width, height, layers, FAST.load(std::sync::atomic::Ordering::Relaxed));
}

/// [`compose`], with these options.
pub fn compose_into(out: &mut [u8], width: usize, height: usize, layers: &[Layer<'_>], opts: Opts) {
    if opts.runs {
        compose_runs(out, width, height, layers, opts.bgra);
        return;
    }
    compose_with(out, width, height, layers, opts.fast);
    if opts.bgra {
        swizzle_in_place(out);
    }
}

/// Swap red and blue in every pixel.
pub fn swizzle_in_place(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        p.copy_from_slice(&swap_rb(v).to_le_bytes());
    }
}

/// A pixel (as a little-endian `u32`) with its first and third bytes swapped.
#[inline]
fn swap_rb(v: u32) -> u32 {
    (v & 0xff00_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16)
}

/// [`compose`], with or without [`FAST`]'s shortcuts.
pub fn compose_with(out: &mut [u8], width: usize, height: usize, layers: &[Layer<'_>], fast: bool) {
    if fast {
        for px in out.chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
    } else {
        out.fill(0);
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    let blend_row: fn(&mut [u8], &[u8], Blend, bool, u32) = if fast { blend_row_fast } else { blend_row };
    // A scaled layer's row, its memory used again from row to row (it was a `Vec` a row).
    let mut row = Vec::new();
    for layer in layers {
        let (l, t, r, b) = layer.frame;
        let (x0, y0) = (l.max(0) as usize, t.max(0) as usize);
        let (x1, y1) = ((r.max(0) as usize).min(width), (b.max(0) as usize).min(height));
        if x0 >= x1 || y0 >= y1 || layer.alpha <= 0.0 {
            continue;
        }
        let plane = (layer.alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        for y in y0..y1 {
            let dst_row = &mut out[(y * width + x0) * 4..(y * width + x1) * 4];
            match &layer.source {
                Source::Pixels { data, stride, opaque, crop_x, crop_y } => {
                    // The source pixel under display (x, y): 1:1 from the crop's corner.
                    let sy = crop_y + (y as i32 - t) as usize;
                    let sx0 = crop_x + (x0 as i32 - l) as usize;
                    let at = (sy * stride + sx0) * 4;
                    let Some(src_row) = data.get(at..at + (x1 - x0) * 4) else { continue };
                    blend_row(dst_row, src_row, layer.blend, *opaque, plane);
                }
                Source::Mapped { data, stride, rows, opaque, bgra, crop, transform } => {
                    mapped_row(&mut row, data, *stride, *rows, *bgra, *crop, *transform, layer.frame, y, x0, x1);
                    blend_row(dst_row, &row, layer.blend, *opaque, plane);
                }
                Source::Color(c) => {
                    let px = color_px(c, false);
                    for d in dst_row.chunks_exact_mut(4) {
                        blend_pixel(d, &px, Blend::Coverage, false, plane);
                    }
                }
            }
        }
    }
}

/// A [`Source::Color`]'s pixel, RGBA (or BGRA).
fn color_px(c: &[f32; 4], bgra: bool) -> [u8; 4] {
    let px = [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, (c[3] * 255.0) as u8];
    if bgra { [px[2], px[1], px[0], px[3]] } else { px }
}

/// The layer that composing starts from: the topmost that hides everything under it (a copy
/// covering the display), or the bottom one when it covers the display premultiplied at full
/// plane alpha -- whose blend over black is its own colour with alpha 255, so the black need not be
/// written first. `None`: start at the bottom, over black.
///
/// Answers the layer's index and whether its alpha is made 255.
fn cover_start(layers: &[Layer<'_>], width: usize, height: usize) -> Option<(usize, bool)> {
    if width == 0 || height == 0 {
        return None;
    }
    let covers = |layer: &Layer<'_>| {
        let (l, t, r, b) = layer.frame;
        if l > 0 || t > 0 || (r.max(0) as usize) < width || (b.max(0) as usize) < height {
            return false;
        }
        match &layer.source {
            // Every display row's source row is there (else that row would stay black).
            Source::Pixels { data, stride, crop_x, crop_y, .. } => {
                let sy = crop_y + (height as i32 - 1 - t) as usize;
                let sx0 = crop_x + (-l) as usize;
                (sy * stride + sx0 + width) * 4 <= data.len()
            }
            // A scaled row is always whole (a pixel past the data is 0).
            Source::Mapped { .. } => true,
            Source::Color(_) => false,
        }
    };
    let plane = |layer: &Layer<'_>| (layer.alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    let opaque = |layer: &Layer<'_>| match &layer.source {
        Source::Pixels { opaque, .. } | Source::Mapped { opaque, .. } => *opaque,
        Source::Color(_) => false,
    };
    for (i, layer) in layers.iter().enumerate().rev() {
        if layer.alpha > 0.0 && plane(layer) == 255 && (layer.blend == Blend::None || opaque(layer)) && covers(layer) {
            return Some((i, opaque(layer)));
        }
    }
    let first = layers.first()?;
    (first.alpha > 0.0 && plane(first) == 255 && first.blend == Blend::Premultiplied && covers(first)).then_some((0, true))
}

/// [`compose_with`]'s pixels by the run path ([`Opts::runs`]), RGBA or (`bgra`) BGRA.
fn compose_runs(out: &mut [u8], width: usize, height: usize, layers: &[Layer<'_>], bgra: bool) {
    let (start, first_alpha) = match cover_start(layers, width, height) {
        Some((i, alpha)) => (i, Some(alpha)),
        None => {
            fill_black(out);
            (0, None)
        }
    };
    let mut row = Vec::new();
    for (i, layer) in layers.iter().enumerate().skip(start) {
        let (l, t, r, b) = layer.frame;
        let (x0, y0) = (l.max(0) as usize, t.max(0) as usize);
        let (x1, y1) = ((r.max(0) as usize).min(width), (b.max(0) as usize).min(height));
        if x0 >= x1 || y0 >= y1 || layer.alpha <= 0.0 {
            continue;
        }
        let plane = (layer.alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        // The first layer of a covered display is copied: its alpha its own (a `NONE` layer's) or 255.
        let copy = (i == start).then_some(first_alpha).flatten();
        for y in y0..y1 {
            let dst_row = &mut out[(y * width + x0) * 4..(y * width + x1) * 4];
            match &layer.source {
                Source::Pixels { data, stride, opaque, crop_x, crop_y } => {
                    let sy = crop_y + (y as i32 - t) as usize;
                    let sx0 = crop_x + (x0 as i32 - l) as usize;
                    let at = (sy * stride + sx0) * 4;
                    let Some(src_row) = data.get(at..at + (x1 - x0) * 4) else { continue };
                    match copy {
                        Some(alpha) => copy_row(dst_row, src_row, bgra, alpha),
                        None => blend_row_runs(dst_row, src_row, layer.blend, *opaque, plane, bgra),
                    }
                }
                Source::Mapped { data, stride, rows, opaque, bgra: src_bgra, crop, transform } => {
                    // Red and blue swapped while the row is fetched when exactly one side is BGRA.
                    mapped_row(&mut row, data, *stride, *rows, *src_bgra != bgra, *crop, *transform, layer.frame, y, x0, x1);
                    match copy {
                        Some(alpha) => copy_row(dst_row, &row, false, alpha),
                        None => blend_row_runs(dst_row, &row, layer.blend, *opaque, plane, false),
                    }
                }
                Source::Color(c) => {
                    let px = color_px(c, bgra);
                    for d in dst_row.chunks_exact_mut(4) {
                        blend_pixel(d, &px, Blend::Coverage, false, plane);
                    }
                }
            }
        }
    }
}

/// Opaque black, every pixel (the same bytes RGBA or BGRA).
fn fill_black(out: &mut [u8]) {
    let black = u32::from_le_bytes([0, 0, 0, 255]);
    for px in out.chunks_exact_mut(4) {
        px.copy_from_slice(&black.to_le_bytes());
    }
}

/// `dst = src`, red and blue swapped when `swap`, alpha made 255 when `alpha`.
#[inline]
fn copy_row(dst: &mut [u8], src: &[u8], swap: bool, alpha: bool) {
    if !swap && !alpha {
        dst.copy_from_slice(src);
        return;
    }
    // Four pixels at a time: the masks keep each pixel's bytes within its own 32 bits.
    const RB_KEEP: u128 = 0xff00_ff00_ff00_ff00_ff00_ff00_ff00_ff00;
    const LOW: u128 = 0x0000_00ff_0000_00ff_0000_00ff_0000_00ff;
    let or4: u128 = if alpha { 0xff00_0000_ff00_0000_ff00_0000_ff00_0000 } else { 0 };
    let mut d16 = dst.chunks_exact_mut(16);
    let mut s16 = src.chunks_exact(16);
    for (d, s) in (&mut d16).zip(&mut s16) {
        let v = u128::from_le_bytes(s.try_into().expect("16"));
        let v = if swap { (v & RB_KEEP) | ((v >> 16) & LOW) | ((v & LOW) << 16) } else { v };
        d.copy_from_slice(&(v | or4).to_le_bytes());
    }
    let or = if alpha { 0xff00_0000 } else { 0 };
    for (d, s) in d16.into_remainder().chunks_exact_mut(4).zip(s16.remainder().chunks_exact(4)) {
        let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let v = if swap { swap_rb(v) } else { v };
        d.copy_from_slice(&(v | or).to_le_bytes());
    }
}

/// [`blend_row_fast`]'s pixels, red and blue of the source swapped first when `swap` (the
/// blend's arithmetic is the same on every colour channel, so a BGRA `dst` blends a swapped source
/// exactly as RGBA would), a premultiplied row taken four pixels at a time: all four transparent,
/// nothing to do; all four opaque at full plane alpha, a copy.
fn blend_row_runs(dst: &mut [u8], src: &[u8], blend: Blend, opaque: bool, plane: u32, swap: bool) {
    if plane == 255 && (blend == Blend::None || opaque) {
        copy_row(dst, src, swap, opaque);
        return;
    }
    let sw = |s: &[u8]| if swap { [s[2], s[1], s[0], s[3]] } else { [s[0], s[1], s[2], s[3]] };
    if blend != Blend::Premultiplied || opaque {
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            blend_pixel(d, &sw(s), blend, opaque, plane);
        }
        return;
    }
    const ALPHAS: u128 = 0xff00_0000_ff00_0000_ff00_0000_ff00_0000;
    let one = |d: &mut [u8], s: &[u8]| {
        if s == [0, 0, 0, 0] {
            return;
        }
        if plane == 255 && s[3] == 255 {
            d.copy_from_slice(&sw(s));
            return;
        }
        blend_pixel(d, &sw(s), blend, false, plane);
    };
    let n = src.len().min(dst.len()) / 16;
    let chunk = |i: usize| u128::from_le_bytes(src[i * 16..i * 16 + 16].try_into().expect("16"));
    let mut i = 0;
    while i < n {
        let v = chunk(i);
        if v == 0 {
            // A transparent run: nothing to do.
            i += 1;
            while i < n && chunk(i) == 0 {
                i += 1;
            }
            continue;
        }
        if plane == 255 && v & ALPHAS == ALPHAS {
            // An opaque run: one copy.
            let start = i;
            i += 1;
            while i < n && chunk(i) & ALPHAS == ALPHAS {
                i += 1;
            }
            copy_row(&mut dst[start * 16..i * 16], &src[start * 16..i * 16], swap, false);
            continue;
        }
        for (d, s) in dst[i * 16..i * 16 + 16].chunks_exact_mut(4).zip(src[i * 16..i * 16 + 16].chunks_exact(4)) {
            one(d, s);
        }
        i += 1;
    }
    for (d, s) in dst[n * 16..].chunks_exact_mut(4).zip(src[n * 16..].chunks_exact(4)) {
        one(d, s);
    }
}

/// Display row `y`, columns `x0..x1`, of a [`Source::Mapped`] layer into `out`: each pixel the
/// source pixel its centre maps to (nearest), as RGBA (`bgra`: red and blue swapped).
#[allow(clippy::too_many_arguments)]
fn mapped_row(out: &mut Vec<u8>, data: &[u8], stride: usize, rows: usize, bgra: bool, crop: (f32, f32, f32, f32), transform: u32, frame: (i32, i32, i32, i32), y: usize, x0: usize, x1: usize) {
    let (l, t, r, b) = frame;
    let (fw, fh) = ((r - l) as f32, (b - t) as f32);
    let (cl, ct, cw, ch) = (crop.0, crop.1, crop.2 - crop.0, crop.3 - crop.1);
    let v = (y as f32 + 0.5 - t as f32) / fh;
    out.clear();
    out.resize((x1 - x0) * 4, 0);
    let src_px = |s: f32, tt: f32| -> [u8; 4] {
        let sx = ((cl + s * cw) as isize).clamp(0, stride as isize - 1) as usize;
        let sy = ((ct + tt * ch) as isize).clamp(0, rows as isize - 1) as usize;
        let at = (sy * stride + sx) * 4;
        match data.get(at..at + 4) {
            Some(p) if bgra => [p[2], p[1], p[0], p[3]],
            Some(p) => [p[0], p[1], p[2], p[3]],
            None => [0; 4],
        }
    };
    for (i, o) in out.chunks_exact_mut(4).enumerate() {
        let u = ((x0 + i) as f32 + 0.5 - l as f32) / fw;
        // Undo the transform: the quarter turn first (it was applied last), then the flips.
        let (mut s, mut tt) = if transform & 4 != 0 { (v, 1.0 - u) } else { (u, v) };
        if transform & 1 != 0 {
            s = 1.0 - s;
        }
        if transform & 2 != 0 {
            tt = 1.0 - tt;
        }
        o.copy_from_slice(&src_px(s.clamp(0.0, 0.999_999), tt.clamp(0.0, 0.999_999)));
    }
}

/// [`blend_row`] with [`FAST`]'s shortcuts for a premultiplied layer with alpha: a pixel whose four
/// bytes are 0 leaves `dst` as it is (`a` is 0, so `dst * 255 / 255`), and at plane alpha 1 a pixel
/// whose alpha is 255 is its own colour with alpha 255 (`src * 255 / 255 + dst * 0`) -- exactly what
/// [`blend_pixel`] computes for each.
fn blend_row_fast(dst: &mut [u8], src: &[u8], blend: Blend, opaque: bool, plane: u32) {
    if (plane == 255 && (blend == Blend::None || opaque)) || blend != Blend::Premultiplied || opaque {
        blend_row(dst, src, blend, opaque, plane);
        return;
    }
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        if s == [0, 0, 0, 0] {
            continue;
        }
        if plane == 255 && s[3] == 255 {
            d.copy_from_slice(s);
            continue;
        }
        blend_pixel(d, s, blend, opaque, plane);
    }
}

fn blend_row(dst: &mut [u8], src: &[u8], blend: Blend, opaque: bool, plane: u32) {
    if plane == 255 && (blend == Blend::None || opaque) {
        dst.copy_from_slice(src);
        if opaque {
            for d in dst.chunks_exact_mut(4) {
                d[3] = 255;
            }
        }
        return;
    }
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        blend_pixel(d, s, blend, opaque, plane);
    }
}

#[inline]
fn blend_pixel(d: &mut [u8], s: &[u8], blend: Blend, opaque: bool, plane: u32) {
    let sa = if opaque || blend == Blend::None { 255 } else { u32::from(s[3]) };
    // Coverage (and a plain colour) is not premultiplied: its colour is scaled by its alpha here.
    let premul = |c: u8| if blend == Blend::Coverage { u32::from(c) * sa / 255 } else { u32::from(c) };
    let a = sa * plane / 255;
    for i in 0..3 {
        let src = premul(s[i]) * plane / 255;
        d[i] = (src + u32::from(d[i]) * (255 - a) / 255).min(255) as u8;
    }
    d[3] = (a + u32::from(d[3]) * (255 - a) / 255).min(255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(out: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        out[(y * w + x) * 4..(y * w + x) * 4 + 4].try_into().unwrap()
    }

    #[test]
    fn an_opaque_layer_is_copied_and_a_premultiplied_one_blended_over_it() {
        let (w, h) = (8, 4);
        let red: Vec<u8> = [255, 0, 0, 255].repeat(w * h);
        // Half-transparent white, premultiplied: (128, 128, 128, 128).
        let veil: Vec<u8> = [128, 128, 128, 128].repeat(4 * 4);
        let mut out = vec![0u8; w * h * 4];
        compose(
            &mut out,
            w,
            h,
            &[
                Layer { source: Source::Pixels { data: &red, stride: w, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, 8, 4), blend: Blend::None, alpha: 1.0 },
                Layer { source: Source::Pixels { data: &veil, stride: 4, opaque: false, crop_x: 0, crop_y: 0 }, frame: (4, 0, 8, 4), blend: Blend::Premultiplied, alpha: 1.0 },
            ],
        );
        assert_eq!(px(&out, w, 1, 1), [255, 0, 0, 255], "the opaque layer, copied");
        let blended = px(&out, w, 5, 1);
        assert!((blended[0] as i32 - 255).abs() <= 1 && (blended[1] as i32 - 128).abs() <= 1, "white over red at half: {blended:?}");
    }

    #[test]
    fn a_frame_past_the_display_is_clipped_and_a_crop_offsets_the_source() {
        let (w, h) = (4, 4);
        // A 4x4 source whose pixel (x, y) is (x*10, y*10, 0, 255).
        let src: Vec<u8> = (0..16).flat_map(|i| [(i % 4) as u8 * 10, (i / 4) as u8 * 10, 0, 255]).collect();
        let mut out = vec![0u8; w * h * 4];
        compose(&mut out, w, h, &[Layer { source: Source::Pixels { data: &src, stride: 4, opaque: true, crop_x: 1, crop_y: 1 }, frame: (-1, 0, 2, 3), blend: Blend::None, alpha: 1.0 }]);
        // Display (0, 0) is frame-relative (1, 0): source (1 + 1, 1 + 0).
        assert_eq!(px(&out, w, 0, 0), [20, 10, 0, 255]);
        assert_eq!(px(&out, w, 3, 3), [0, 0, 0, 255], "outside the frame stays black");
    }

    /// A 2x1 source (red, green) at 4x2: each source pixel twice as wide and tall; turned a quarter
    /// clockwise the left pixel is on top; flipped horizontally green comes first.
    #[test]
    fn a_crop_is_scaled_to_its_frame_and_transformed() {
        let src: Vec<u8> = [[255, 0, 0, 255], [0, 255, 0, 255]].concat();
        let layer = |frame, transform| Layer {
            source: Source::Mapped { data: &src, stride: 2, rows: 1, opaque: true, bgra: false, crop: (0.0, 0.0, 2.0, 1.0), transform },
            frame,
            blend: Blend::None,
            alpha: 1.0,
        };
        let mut out = vec![0u8; 4 * 2 * 4];
        compose(&mut out, 4, 2, &[layer((0, 0, 4, 2), 0)]);
        assert_eq!([px(&out, 4, 1, 1), px(&out, 4, 2, 0)], [[255, 0, 0, 255], [0, 255, 0, 255]], "scaled");
        compose(&mut out, 4, 2, &[layer((0, 0, 4, 2), 1)]);
        assert_eq!([px(&out, 4, 0, 0), px(&out, 4, 3, 1)], [[0, 255, 0, 255], [255, 0, 0, 255]], "flipped");
        let mut out = vec![0u8; 2 * 4 * 4];
        compose(&mut out, 2, 4, &[layer((0, 0, 2, 4), 4)]);
        assert_eq!([px(&out, 2, 0, 0), px(&out, 2, 1, 3)], [[255, 0, 0, 255], [0, 255, 0, 255]], "turned: red on top, green below");
    }

    #[test]
    fn a_bgra_source_is_swapped() {
        let src: Vec<u8> = vec![255, 0, 0, 255];
        let mut out = vec![0u8; 4];
        compose(
            &mut out,
            1,
            1,
            &[Layer { source: Source::Mapped { data: &src, stride: 1, rows: 1, opaque: false, bgra: true, crop: (0.0, 0.0, 1.0, 1.0), transform: 0 }, frame: (0, 0, 1, 1), blend: Blend::None, alpha: 1.0 }],
        );
        assert_eq!(px(&out, 1, 0, 0), [0, 0, 255, 255]);
    }

    /// The fast path's shortcuts give exactly the blend's pixels: random stacks of premultiplied,
    /// coverage and opaque layers, alphas biased to 0 and 255 (where the shortcuts fire), plane
    /// alphas 1 and below, frames overlapping and past the display.
    #[test]
    fn the_fast_path_composes_the_same_pixels() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (w, h) = (37, 23);
        for round in 0..200 {
            let count = 1 + (next() % 4) as usize;
            let mut datas = Vec::new();
            let mut specs = Vec::new();
            for _ in 0..count {
                let (sw, sh) = (1 + (next() % 48) as usize, 1 + (next() % 32) as usize);
                let data: Vec<u8> = (0..sw * sh)
                    .flat_map(|_| {
                        let a = match next() % 4 {
                            0 => 0,
                            1 => 255,
                            _ => (next() % 256) as u8,
                        };
                        // Premultiplied-valid colour (not above alpha), or all zero.
                        let c = |n: u64| if a == 0 { 0 } else { (n % (u64::from(a) + 1)) as u8 };
                        [c(next()), c(next()), c(next()), a]
                    })
                    .collect();
                let blend = match next() % 3 {
                    0 => Blend::None,
                    1 => Blend::Coverage,
                    _ => Blend::Premultiplied,
                };
                let l = (next() % 50) as i32 - 10;
                let t = (next() % 30) as i32 - 5;
                specs.push((sw, sh, next() % 5 == 0, blend, (l, t, l + sw as i32, t + sh as i32), if next() % 3 == 0 { 0.5 } else { 1.0 }));
                datas.push(data);
            }
            let layers: Vec<Layer<'_>> = specs
                .iter()
                .zip(&datas)
                .map(|(&(sw, _, opaque, blend, frame, alpha), data)| Layer { source: Source::Pixels { data, stride: sw, opaque, crop_x: 0, crop_y: 0 }, frame, blend, alpha })
                .collect();
            let (mut slow, mut fast) = (vec![7u8; w * h * 4], vec![9u8; w * h * 4]);
            compose_with(&mut slow, w, h, &layers, false);
            compose_with(&mut fast, w, h, &layers, true);
            assert_eq!(slow, fast, "round {round}");
        }
    }

    /// A measurement, not a check (`cargo test --release -p omni-linux --lib -- --ignored
    /// compose_cost`): one 1280x720 frame as an app in a world makes it -- its SurfaceView (opaque
    /// pixels, a premultiplied layer) under its window (a transparent hole but for a 200-pixel bar
    /// of translucent UI) -- composed by each path, interleaved, 16 pairs of 30 frames.
    #[test]
    #[ignore = "a measurement"]
    fn compose_cost() {
        let (w, h) = (1280usize, 720usize);
        let scene: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8, 255]).collect();
        let window: Vec<u8> = (0..w * h).flat_map(|i| if i / w < 200 { [60, 60, 60, 128] } else { [0, 0, 0, 0] }).collect();
        let layers = [
            Layer { source: Source::Pixels { data: &scene, stride: w, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
            Layer { source: Source::Pixels { data: &window, stride: w, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
        ];
        let mut out = vec![0u8; w * h * 4];
        let mut time = |fast: bool| {
            let t = std::time::Instant::now();
            for _ in 0..30 {
                compose_with(&mut out, w, h, &layers, fast);
            }
            t.elapsed().as_secs_f64() * 1000.0 / 30.0
        };
        let (mut slow, mut fast) = (Vec::new(), Vec::new());
        for pair in 0..16 {
            if pair % 2 == 0 {
                slow.push(time(false));
                fast.push(time(true));
            } else {
                fast.push(time(true));
                slow.push(time(false));
            }
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let mut d: Vec<f64> = fast.iter().zip(&slow).map(|(f, s)| f - s).collect();
        eprintln!(
            "compose 1280x720, 2 layers: slow {:.2} ms/frame, fast {:.2} ms/frame, paired d median {:.2} (min {:.2}, max {:.2})",
            med(&mut slow.clone()),
            med(&mut fast.clone()),
            med(&mut d),
            d.first().copied().unwrap_or(0.0),
            d.last().copied().unwrap_or(0.0)
        );
    }

    /// The run path ([`ZERO`]) and BGRA out ([`BGRA_OUT`]) give exactly the plain blend's pixels
    /// (red and blue swapped for BGRA): random stacks of 1:1, scaled and transformed, and colour
    /// layers, every blend, plane alphas 0 to 1, opaque formats, frames covering the display (where
    /// the run path writes no black and skips what an opaque layer hides) and past it, sources too
    /// short for their frame, arbitrary bytes (premultiplied-invalid too). The output starts as
    /// garbage, so a pixel the run path failed to write shows.
    #[test]
    fn the_run_path_composes_the_same_pixels() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut covered, mut hidden) = (0, 0);
        for round in 0..2000 {
            let (w, h) = (1 + (next() % 40) as usize, 1 + (next() % 24) as usize);
            let count = 1 + (next() % 4) as usize;
            let mut datas: Vec<Vec<u8>> = Vec::new();
            struct Spec {
                kind: u64,
                sh: usize,
                stride: usize,
                frame: (i32, i32, i32, i32),
                crop: (usize, usize),
                fcrop: (f32, f32, f32, f32),
                transform: u32,
                opaque: bool,
                bgra: bool,
                blend: Blend,
                alpha: f32,
                color: [f32; 4],
            }
            let mut specs = Vec::new();
            for _ in 0..count {
                let cover = next() % 2 == 0;
                let frame = if cover {
                    let (l, t) = (-((next() % 3) as i32), -((next() % 3) as i32));
                    (l, t, w as i32 + (next() % 3) as i32, h as i32 + (next() % 3) as i32)
                } else {
                    let (l, t) = ((next() % 50) as i32 - 10, (next() % 30) as i32 - 5);
                    (l, t, l + 1 + (next() % 48) as i32, t + 1 + (next() % 32) as i32)
                };
                let (fw, fh) = ((frame.2 - frame.0) as usize, (frame.3 - frame.1) as usize);
                let crop = ((next() % 3) as usize, (next() % 3) as usize);
                // A 1:1 source as big as its frame from the crop (or, now and then, a row short).
                let (sw, sh) = match next() % 3 {
                    0 => (1 + (next() % 48) as usize, 1 + (next() % 32) as usize),
                    _ => (fw + crop.0, fh + crop.1),
                };
                let stride = sw + (next() % 3) as usize;
                let mut len = stride * sh * 4;
                if next() % 6 == 0 {
                    len = len.saturating_sub(4 * (1 + (next() % (2 * stride as u64)) as usize));
                }
                let data: Vec<u8> = (0..len / 4)
                    .flat_map(|_| {
                        let a = match next() % 4 {
                            0 => 0,
                            1 => 255,
                            _ => (next() % 256) as u8,
                        };
                        if next() % 8 == 0 {
                            return [(next() % 256) as u8, (next() % 256) as u8, (next() % 256) as u8, a];
                        }
                        let c = |n: u64| if a == 0 { 0 } else { (n % (u64::from(a) + 1)) as u8 };
                        [c(next()), c(next()), c(next()), a]
                    })
                    .collect();
                let x0 = (next() % sw as u64) as f32;
                let y0 = (next() % sh as u64) as f32;
                let fcrop = (x0, y0, x0 + 1.0 + (next() % (sw as u64 - x0 as u64)) as f32, y0 + 1.0 + (next() % (sh as u64 - y0 as u64)) as f32);
                specs.push(Spec {
                    kind: next() % 5,
                    sh,
                    stride,
                    frame,
                    crop,
                    fcrop,
                    transform: (next() % 8) as u32,
                    opaque: next() % 4 == 0,
                    bgra: next() % 2 == 0,
                    blend: match next() % 3 {
                        0 => Blend::None,
                        1 => Blend::Coverage,
                        _ => Blend::Premultiplied,
                    },
                    alpha: match next() % 6 {
                        0 => 0.0,
                        1 => 0.5,
                        2 => 0.999,
                        _ => 1.0,
                    },
                    color: [(next() % 256) as f32 / 255.0, (next() % 256) as f32 / 255.0, (next() % 256) as f32 / 255.0, (next() % 256) as f32 / 255.0],
                });
                datas.push(data);
            }
            let layers: Vec<Layer<'_>> = specs
                .iter()
                .zip(&datas)
                .map(|(s, data)| {
                    let source = match s.kind {
                        0 | 1 | 2 => Source::Pixels { data, stride: s.stride, opaque: s.opaque, crop_x: s.crop.0, crop_y: s.crop.1 },
                        3 => Source::Mapped { data, stride: s.stride, rows: s.sh, opaque: s.opaque, bgra: s.bgra, crop: s.fcrop, transform: s.transform },
                        _ => Source::Color(s.color),
                    };
                    Layer { source, frame: s.frame, blend: s.blend, alpha: s.alpha }
                })
                .collect();
            if let Some((i, _)) = cover_start(&layers, w, h) {
                covered += 1;
                hidden += usize::from(i > 0);
            }
            let mut want = vec![7u8; w * h * 4];
            compose_with(&mut want, w, h, &layers, false);
            let mut want_bgra = want.clone();
            swizzle_in_place(&mut want_bgra);
            for (opts, expect) in [
                (Opts { fast: true, runs: true, bgra: false }, &want),
                (Opts { fast: true, runs: true, bgra: true }, &want_bgra),
                (Opts { fast: true, runs: false, bgra: true }, &want_bgra),
            ] {
                let mut got = vec![(round % 251) as u8; w * h * 4];
                compose_into(&mut got, w, h, &layers, opts);
                assert!(got == *expect, "round {round}, {opts:?}");
            }
        }
        // The shortcuts were taken, not only the plain path.
        assert!(covered > 300 && hidden > 50, "covered {covered}, of them under an opaque layer {hidden}");
    }

    /// `Windows`' canvas store (`omni_platform::window::windows::store`): the window's swizzled copy
    /// of each RGBA frame, the copy `present_bgra` removes.
    fn window_store(canvas: &mut Vec<u8>, rgba: &[u8]) {
        canvas.resize(rgba.len(), 0);
        for (d, s) in canvas.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], 0xff]);
        }
    }

    /// **A measurement, not a check** (`cargo test --release -p omni-linux --lib -- --ignored
    /// frame_cost --nocapture`): one in-world frame at 1575x890 from the two gralloc regions to what
    /// the window paints, through the system process's whole path -- an opaque SurfaceView
    /// (premultiplied, alpha 255) under the app's window (transparent but for a 60-row opaque bar
    /// and a translucent 300x200 panel), both 1600 pixels a row in real shared-memory regions --
    /// by each path, interleaved, 16 pairs of 30 frames:
    ///
    /// - **old**: each region read into a buffer of its own, composed (`FAST`) into a third, copied
    ///   into the framebuffer, swizzled into the window's canvas;
    /// - **zero** (`compose_zero=1`): composed from the regions straight into the framebuffer, the
    ///   window's swizzled copy as before;
    /// - **zero+bgra** (`compose_zero=1`, `present_bgra=1`): the same composed BGRA, the frame
    ///   handed to the window shared.
    #[test]
    #[ignore = "a measurement"]
    fn frame_cost() {
        use crate::hal::framebuffer::Framebuffer;
        use crate::shm::Shm;
        let (w, h, stride) = (1575usize, 890usize, 1600usize);
        let scene: Vec<u8> = (0..stride * h).flat_map(|i| [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8, 255]).collect();
        let window: Vec<u8> = (0..stride * h)
            .flat_map(|i| {
                let (x, y) = (i % stride, i / stride);
                if y < 60 {
                    [30, 30, 30, 255]
                } else if (40..340).contains(&x) && (600..800).contains(&y) {
                    [60, 60, 60, 128]
                } else {
                    [0, 0, 0, 0]
                }
            })
            .collect();
        let region = |pixels: &[u8]| {
            let shm = Shm::create("frame_cost").expect("region");
            shm.set_len(pixels.len() as u64).expect("size");
            shm.as_graphics_buffer();
            shm.write_at(pixels, 0).expect("write");
            shm
        };
        let (scene_shm, window_shm) = (region(&scene), region(&window));
        let fb = Framebuffer::new(w as u32, h as u32);
        let (mut scratch_a, mut scratch_b, mut scratch_out, mut canvas) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut held: Option<std::sync::Arc<Vec<u8>>> = None;
        let need = stride * h * 4;
        let layers2 = |a: &[u8], b: &[u8], f: &mut dyn FnMut(&[Layer<'_>])| {
            let layers = [
                Layer { source: Source::Pixels { data: a, stride, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
                Layer { source: Source::Pixels { data: b, stride, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
            ];
            f(&layers);
        };
        // One frame by path 0 (old), 1 (zero), 2 (zero+bgra).
        let mut frame = |path: u32| match path {
            0 => {
                scratch_a.resize(need, 0);
                scratch_b.resize(need, 0);
                scene_shm.read_at(&mut scratch_a, 0).expect("read");
                window_shm.read_at(&mut scratch_b, 0).expect("read");
                scratch_out.resize(w * h * 4, 0);
                layers2(&scratch_a, &scratch_b, &mut |layers| compose_with(&mut scratch_out, w, h, layers, true));
                fb.present_frame(&scratch_out, w as u32, h as u32, w as u32);
                let (_, _, _, px) = fb.frame();
                window_store(&mut canvas, &px);
            }
            _ => {
                let bgra = path == 2;
                let (a, b) = (scene_shm.bytes(0, need).expect("view"), window_shm.bytes(0, need).expect("view"));
                layers2(&a, &b, &mut |layers| fb.present_with(w as u32, h as u32, bgra, |out| compose_into(out, w, h, layers, Opts { fast: true, runs: true, bgra })));
                let (_, _, _, px, is_bgra) = fb.frame_raw();
                if is_bgra {
                    held = Some(px); // the window keeps the frame it shows
                } else {
                    held = None;
                    window_store(&mut canvas, &px);
                }
            }
        };
        let mut time = |path: u32| {
            let t = std::time::Instant::now();
            for _ in 0..30 {
                frame(path);
            }
            t.elapsed().as_secs_f64() * 1000.0 / 30.0
        };
        let mut ms = [Vec::new(), Vec::new(), Vec::new()];
        for _ in 0..2 {
            for p in 0..3 {
                time(p);
            }
        }
        for pair in 0..16u32 {
            for k in 0..3 {
                let p = (pair + k) % 3;
                ms[p as usize].push(time(p));
            }
        }
        let med = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            (v[v.len() / 2], v[0], v[v.len() - 1])
        };
        for (name, v) in ["old (read x2, compose, copy, swizzle)", "compose_zero=1", "compose_zero=1 present_bgra=1"].iter().zip(&ms) {
            let (m, lo, hi) = med(v);
            eprintln!("frame_cost 1575x890 {name}: median {m:.2} ms/frame (min {lo:.2}, max {hi:.2})");
        }
        // The compositions alone, from the same bytes.
        let mut out = vec![0u8; w * h * 4];
        let mut only = |opts: Option<Opts>| {
            let t = std::time::Instant::now();
            for _ in 0..30 {
                layers2(&scene, &window, &mut |layers| match opts {
                    None => compose_with(&mut out, w, h, layers, true),
                    Some(o) => compose_into(&mut out, w, h, layers, o),
                });
            }
            t.elapsed().as_secs_f64() * 1000.0 / 30.0
        };
        let mut c = [Vec::new(), Vec::new(), Vec::new()];
        for _ in 0..16 {
            c[0].push(only(None));
            c[1].push(only(Some(Opts { fast: true, runs: true, bgra: false })));
            c[2].push(only(Some(Opts { fast: true, runs: true, bgra: true })));
        }
        eprintln!("compose only: FAST {:.2} ms/frame, run path RGBA {:.2}, run path BGRA {:.2}", med(&c[0]).0, med(&c[1]).0, med(&c[2]).0);
        drop(held);
    }

    #[test]
    fn a_colour_layer_with_plane_alpha() {
        let (w, h) = (2, 2);
        let mut out = vec![0u8; w * h * 4];
        compose(&mut out, w, h, &[Layer { source: Source::Color([0.0, 0.0, 1.0, 1.0]), frame: (0, 0, 2, 2), blend: Blend::Premultiplied, alpha: 0.5 }]);
        let p = px(&out, w, 1, 1);
        assert!((p[2] as i32 - 128).abs() <= 1 && p[0] == 0, "{p:?}");
    }
}
