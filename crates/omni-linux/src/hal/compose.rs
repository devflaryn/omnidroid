//! The composer's own composition (`DEVICE`): the layers of a frame blended straight into the
//! display's pixels on the host CPU, so SurfaceFlinger's RenderEngine -- GLES on ANGLE on the
//! paravirtual Vulkan driver, every command a forwarded call -- draws nothing. What it takes: RGBA
//! buffers at 1:1 (a source crop the size of the display frame), no transform, blending `NONE`,
//! `PREMULTIPLIED` or `COVERAGE` with a plane alpha; and solid colours. Anything else the composer
//! leaves to SurfaceFlinger for the whole frame (`crate::hal::composer`).
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
    Pixels { data: &'a [u8], stride: usize, opaque: bool, crop_x: usize, crop_y: usize },
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

    #[test]
    fn a_colour_layer_with_plane_alpha() {
        let (w, h) = (2, 2);
        let mut out = vec![0u8; w * h * 4];
        compose(&mut out, w, h, &[Layer { source: Source::Color([0.0, 0.0, 1.0, 1.0]), frame: (0, 0, 2, 2), blend: Blend::Premultiplied, alpha: 0.5 }]);
        let p = px(&out, w, 1, 1);
        assert!((p[2] as i32 - 128).abs() <= 1 && p[0] == 0, "{p:?}");
    }
}
