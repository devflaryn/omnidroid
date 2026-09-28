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

/// Compose `layers` (bottom first) over black into `out`: RGBA rows of `width` pixels.
pub fn compose(out: &mut [u8], width: usize, height: usize, layers: &[Layer<'_>]) {
    out.fill(0);
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
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
                    let row = mapped_row(data, *stride, *rows, *bgra, *crop, *transform, layer.frame, y, x0, x1);
                    blend_row(dst_row, &row, layer.blend, *opaque, plane);
                }
                Source::Color(c) => {
                    let px = [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, (c[3] * 255.0) as u8];
                    for d in dst_row.chunks_exact_mut(4) {
                        blend_pixel(d, &px, Blend::Coverage, false, plane);
                    }
                }
            }
        }
    }
}

/// Display row `y`, columns `x0..x1`, of a [`Source::Mapped`] layer: each pixel the source pixel
/// its centre maps to (nearest), as RGBA.
#[allow(clippy::too_many_arguments)]
fn mapped_row(data: &[u8], stride: usize, rows: usize, bgra: bool, crop: (f32, f32, f32, f32), transform: u32, frame: (i32, i32, i32, i32), y: usize, x0: usize, x1: usize) -> Vec<u8> {
    let (l, t, r, b) = frame;
    let (fw, fh) = ((r - l) as f32, (b - t) as f32);
    let (cl, ct, cw, ch) = (crop.0, crop.1, crop.2 - crop.0, crop.3 - crop.1);
    let v = (y as f32 + 0.5 - t as f32) / fh;
    let mut out = vec![0u8; (x1 - x0) * 4];
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
    out
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

    #[test]
    fn a_colour_layer_with_plane_alpha() {
        let (w, h) = (2, 2);
        let mut out = vec![0u8; w * h * 4];
        compose(&mut out, w, h, &[Layer { source: Source::Color([0.0, 0.0, 1.0, 1.0]), frame: (0, 0, 2, 2), blend: Blend::Premultiplied, alpha: 0.5 }]);
        let p = px(&out, w, 1, 1);
        assert!((p[2] as i32 - 128).abs() <= 1 && p[0] == 0, "{p:?}");
    }
}
