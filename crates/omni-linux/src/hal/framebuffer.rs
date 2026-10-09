//! The display's framebuffer on the host: what the host composer presents (D3), a frame count to
//! wait on, and a screenshot of it as a PNG. Its size is the last frame's: the display can be
//! resized (the composer's `set_display_size`), and a frame of the new size replaces the old one
//! whole.
//!
//! **A frame is shared, not copied, once presented**: the held frame is an `Arc`, and a reader
//! ([`Framebuffer::frame`], the live window each frame) takes a reference to it rather than a copy
//! made under the lock the composer's next present waits on (19 MB at a Retina window's size, and
//! the window's copy held SurfaceFlinger's present up while it was made). A present copies the new
//! frame into a spare buffer outside the lock -- a frame before, once no reader holds it -- and
//! swaps it in; or (`compose_zero`) the frame is composed straight into that spare
//! ([`Framebuffer::present_with`]) and not copied at all.
//!
//! **A frame may be BGRA** (`present_bgra`, [`super::compose::BGRA_OUT`]): the window takes it as
//! it is ([`Framebuffer::frame_raw`]); every other reader -- [`Framebuffer::frame`],
//! [`Framebuffer::pixels`], the PNG -- still gets RGBA.
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

/// How many replaced frames are kept for presents to come. The frame before is normally free again
/// by the next present (one spare, as before); under `present_bgra` the window holds the frame it
/// shows until the next one, so the one before that is the free one.
const SPARES: usize = 2;

pub struct Framebuffer {
    state: Mutex<State>,
    presented: Condvar,
    /// Frames replaced, for a present to write into once no reader holds one any more.
    spares: Mutex<Vec<Arc<Vec<u8>>>>,
    /// Where the composer can present a frame of share images itself (`present_zero`): the
    /// display window's swapchain, while it has one.
    sink: Mutex<Option<Arc<dyn ZeroSink>>>,
}

/// **A window that shows frames composed on the GPU from share images** (`present_zero`,
/// `crate::gpu::share`), which the composer presents to directly.
pub trait ZeroSink: Send + Sync {
    /// Show `layers` (bottom first) of a `display`-sized display; the share images are read by the
    /// GPU when this returns.
    ///
    /// # Errors
    /// Anything it could not do: the composer then composes the frame on the CPU.
    fn present(&self, layers: &[crate::gpu::share::ShareLayer], display: (u32, u32)) -> Result<(), String>;
}

/// The CPU composition of a frame shown from share images, run only when a reader asks for its
/// pixels: it writes every byte of a frame of the framebuffer's size, RGBA.
pub type LazyFrame = Arc<dyn Fn(&mut [u8]) + Send + Sync>;

struct State {
    width: u32,
    height: u32,
    /// 8 bits a channel, `width` pixels a row: RGBA, or BGRA when `bgra`.
    pixels: Arc<Vec<u8>>,
    bgra: bool,
    frames: u64,
    /// The current frame was shown from share images and its pixels are not composed yet: how to.
    external: Option<LazyFrame>,
}

impl Framebuffer {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            state: Mutex::new(State { width, height, pixels: Arc::new(vec![0; width as usize * height as usize * 4]), bgra: false, frames: 0, external: None }),
            presented: Condvar::new(),
            spares: Mutex::new(Vec::new()),
            sink: Mutex::new(None),
        }
    }

    /// Give the composer a window to present share images to, or take it away.
    pub fn set_sink(&self, sink: Option<Arc<dyn ZeroSink>>) {
        *self.sink.lock() = sink;
    }

    /// The window the composer can present share images to, if any.
    #[must_use]
    pub fn sink(&self) -> Option<Arc<dyn ZeroSink>> {
        self.sink.lock().clone()
    }

    /// **A frame the composer showed from share images** (`present_zero`): counted, waited on and
    /// sized as any frame, its pixels composed by `compose` only when a reader asks for them
    /// ([`frame`](Self::frame), [`pixels`](Self::pixels), [`png`](Self::png)) -- the window shows
    /// it already, so nothing is composed on the CPU per frame.
    pub fn present_external(&self, width: u32, height: u32, compose: LazyFrame) {
        {
            let mut st = self.state.lock();
            (st.width, st.height) = (width, height);
            st.frames += 1;
            st.external = Some(compose);
        }
        self.presented.notify_all();
    }

    /// Compose the current frame's pixels if it was shown from share images and they are not yet.
    fn materialize(&self) {
        loop {
            let (n, w, h, job) = {
                let st = self.state.lock();
                match &st.external {
                    None => return,
                    Some(job) => (st.frames, st.width, st.height, Arc::clone(job)),
                }
            };
            let mut pixels = self.take_spare(w as usize * h as usize * 4);
            job(&mut pixels);
            let mut st = self.state.lock();
            if st.frames != n {
                // A newer frame came meanwhile: compose that one instead.
                drop(st);
                self.retire(Arc::new(pixels));
                continue;
            }
            st.external = None;
            st.bgra = false;
            let old = std::mem::replace(&mut st.pixels, Arc::new(pixels));
            drop(st);
            self.retire(old);
            return;
        }
    }

    /// The current frame for the display window: its number, size and pixels with whether they
    /// are BGRA -- or no pixels for a frame the composer showed in the window itself.
    #[must_use]
    pub fn frame_for_window(&self) -> (u64, u32, u32, Option<(Arc<Vec<u8>>, bool)>) {
        let st = self.state.lock();
        let pixels = st.external.is_none().then(|| (Arc::clone(&st.pixels), st.bgra));
        (st.frames, st.width, st.height, pixels)
    }

    /// The size of the frame it holds.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        let st = self.state.lock();
        (st.width, st.height)
    }

    /// Present a frame of the framebuffer's current size, RGBA rows `stride` pixels apart (a
    /// buffer's stride may exceed the display's width); rows or bytes the source lacks are left
    /// as they were.
    pub fn present_rgba(&self, src: &[u8], stride: u32) {
        let (width, height) = self.size();
        self.present_frame(src, width, height, stride);
    }

    /// A buffer of `size` bytes for the next frame: a spare no reader holds, else a new one. What
    /// it holds is an earlier frame's.
    fn take_spare(&self, size: usize) -> Vec<u8> {
        let mut spares = self.spares.lock();
        let free = spares.iter().position(|a| Arc::strong_count(a) == 1 && a.len() == size);
        free.and_then(|i| Arc::try_unwrap(spares.swap_remove(i)).ok()).unwrap_or_else(|| vec![0; size])
    }

    /// Make `pixels` the frame, count it and wake its waiters; the frame it replaces is kept as a
    /// spare.
    fn swap_in(&self, pixels: Vec<u8>, width: u32, height: u32, bgra: bool) {
        let old = {
            let mut st = self.state.lock();
            (st.width, st.height, st.bgra) = (width, height, bgra);
            st.frames += 1;
            st.external = None;
            std::mem::replace(&mut st.pixels, Arc::new(pixels))
        };
        self.presented.notify_all();
        self.retire(old);
    }

    /// Keep a frame's buffer as a spare: spares of another size are let go, and the oldest past
    /// [`SPARES`].
    fn retire(&self, old: Arc<Vec<u8>>) {
        let mut spares = self.spares.lock();
        spares.retain(|a| a.len() == old.len());
        spares.push(old);
        if spares.len() > SPARES {
            spares.remove(0);
        }
    }

    /// Present a `width` x `height` frame, RGBA rows `stride` pixels apart. A frame of another size
    /// than the one held replaces it, black where the source lacks rows or bytes.
    pub fn present_frame(&self, src: &[u8], width: u32, height: u32, stride: u32) {
        let (w, stride) = (width as usize * 4, stride as usize * 4);
        let size = w * height as usize;
        let whole = height == 0 || src.len() >= (height as usize - 1) * stride + w;
        let mut pixels = self.take_spare(size);
        if !whole {
            // Rows the source lacks are the held frame's (when it is of this size).
            let st = self.state.lock();
            if (st.width, st.height) == (width, height) {
                pixels.copy_from_slice(&st.pixels);
                if st.bgra {
                    super::compose::swizzle_in_place(&mut pixels);
                }
            } else {
                pixels.fill(0);
            }
        }
        for y in 0..height as usize {
            let Some(row) = src.get(y * stride..y * stride + w) else { break };
            pixels[y * w..y * w + w].copy_from_slice(row);
        }
        self.swap_in(pixels, width, height, false);
    }

    /// **Present a frame made in place** (`compose_zero`): `fill` writes every byte of a
    /// `width` x `height` frame, RGBA or (`bgra`) BGRA, into the buffer it is handed -- a spare,
    /// holding an earlier frame -- which then becomes the frame, not copied.
    pub fn present_with(&self, width: u32, height: u32, bgra: bool, fill: impl FnOnce(&mut [u8])) {
        let mut pixels = self.take_spare(width as usize * height as usize * 4);
        fill(&mut pixels);
        self.swap_in(pixels, width, height, bgra);
    }

    /// How many frames have been presented.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.state.lock().frames
    }

    /// Wait until `n` frames have been presented, at most `timeout`.
    pub fn wait_frame(&self, n: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.state.lock();
        while st.frames < n {
            if self.presented.wait_until(&mut st, deadline).timed_out() {
                return st.frames >= n;
            }
        }
        true
    }

    /// The current frame's pixels (RGBA).
    #[must_use]
    pub fn pixels(&self) -> Vec<u8> {
        self.frame().3.to_vec()
    }

    /// The current frame: its number (the frame count when it was presented), its size and its
    /// pixels (RGBA), taken together -- the pixels shared, not copied (but for a BGRA frame, whose
    /// pixels are made RGBA here).
    #[must_use]
    pub fn frame(&self) -> (u64, u32, u32, Arc<Vec<u8>>) {
        let (n, w, h, pixels, bgra) = self.frame_raw();
        if !bgra {
            return (n, w, h, pixels);
        }
        let mut rgba = pixels.to_vec();
        super::compose::swizzle_in_place(&mut rgba);
        (n, w, h, Arc::new(rgba))
    }

    /// [`frame`](Self::frame) as it is held: its pixels shared, and whether they are BGRA.
    #[must_use]
    pub fn frame_raw(&self) -> (u64, u32, u32, Arc<Vec<u8>>, bool) {
        self.materialize();
        let st = self.state.lock();
        (st.frames, st.width, st.height, Arc::clone(&st.pixels), st.bgra)
    }

    /// The current frame as a PNG (8-bit RGBA; stored, uncompressed deflate blocks: a screenshot
    /// is taken to be looked at, not kept small).
    #[must_use]
    pub fn png(&self) -> Vec<u8> {
        let (_, width, height, pixels, bgra) = self.frame_raw();
        let row = width as usize * 4;
        let mut raw = Vec::with_capacity((row + 1) * height as usize);
        for y in 0..height as usize {
            raw.push(0); // filter: none
            let at = raw.len();
            raw.extend_from_slice(&pixels[y * row..y * row + row]);
            if bgra {
                super::compose::swizzle_in_place(&mut raw[at..]);
            }
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &zlib_stored(&raw));
        chunk(&mut png, b"IEND", &[]);
        png
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(65_535).collect() };
    for (i, block) in blocks.iter().enumerate() {
        out.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    b << 16 | a
}

/// zlib's CRC-32, table-driven (flate2's, which is `crc32fast`): the bit-at-a-time loop it replaces
/// went eight steps a byte over the whole frame for every screenshot (5.6 MB at 1575x890, every 5 s
/// under `OMNI_SCREENSHOT`).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

#[cfg(test)]
mod tests {
    use super::Framebuffer;

    /// The bit-at-a-time CRC-32 the PNG chunks had.
    fn crc32_bitwise(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn the_table_crc_is_the_bitwise_one() {
        let data: Vec<u8> = (0..100_003u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for n in [0, 1, 3, 4, 13, 4096, data.len()] {
            assert_eq!(super::crc32(&data[..n]), crc32_bitwise(&data[..n]), "{n} bytes");
        }
        assert_eq!(super::crc32(b"IEND"), 0xAE42_6082, "the IEND chunk's well-known CRC");
    }

    /// A frame shown from share images is counted at once, the window is told it has no pixels to
    /// show, and its pixels are composed once, when first asked for; a CPU frame after it is held
    /// as before.
    #[test]
    fn an_external_frame_is_composed_only_when_read() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let fb = Framebuffer::new(4, 2);
        let runs = std::sync::Arc::new(AtomicU32::new(0));
        let job = {
            let runs = std::sync::Arc::clone(&runs);
            std::sync::Arc::new(move |out: &mut [u8]| {
                runs.fetch_add(1, Ordering::SeqCst);
                for (i, p) in out.chunks_exact_mut(4).enumerate() {
                    p.copy_from_slice(&[i as u8, 7, 9, 255]);
                }
            })
        };
        fb.present_external(4, 2, job);
        assert_eq!(fb.frames(), 1);
        assert!(fb.wait_frame(1, std::time::Duration::ZERO));
        assert!(fb.frame_for_window().3.is_none(), "the window shows it already");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "nothing composed yet");
        let want: Vec<u8> = (0..8u8).flat_map(|i| [i, 7, 9, 255]).collect();
        assert_eq!(fb.pixels(), want);
        assert_eq!(fb.png(), {
            let plain = Framebuffer::new(4, 2);
            plain.present_frame(&want, 4, 2, 4);
            plain.png()
        });
        assert_eq!(runs.load(Ordering::SeqCst), 1, "composed once, for both readers");
        assert!(fb.frame_for_window().3.is_some());
        fb.present_frame(&[5; 32], 4, 2, 4);
        assert_eq!(fb.pixels(), vec![5; 32]);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// A BGRA frame reads back as RGBA everywhere but `frame_raw`, its PNG is the RGBA frame's byte
    /// for byte, and a frame made in place reuses a spare once no reader holds it.
    #[test]
    fn a_bgra_frame_reads_back_as_rgba() {
        let rgba: Vec<u8> = (0..6 * 4u32).flat_map(|i| [i as u8, (i * 3) as u8, 200 - i as u8, 255]).collect();
        let a = Framebuffer::new(6, 4);
        a.present_frame(&rgba, 6, 4, 6);
        let b = Framebuffer::new(6, 4);
        b.present_with(6, 4, true, |out| {
            out.copy_from_slice(&rgba);
            crate::hal::compose::swizzle_in_place(out);
        });
        assert_eq!(b.pixels(), rgba);
        assert_eq!(*b.frame().3, rgba);
        assert!(b.frame_raw().4, "held as BGRA");
        assert_eq!(a.png(), b.png());
        let held = b.frame_raw().3;
        let held_at = held.as_ptr();
        b.present_with(6, 4, false, |out| out.copy_from_slice(&rgba));
        // The BGRA frame is held (as the window holds the frame it shows): not reused.
        b.present_with(6, 4, false, |out| assert_ne!(out.as_ptr(), held_at, "a held frame is not written"));
        drop(held);
        assert_eq!(b.frames(), 3);
    }
}
