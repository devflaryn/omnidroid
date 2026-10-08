//! The display's framebuffer on the host: what the host composer presents (D3), a frame count to
//! wait on, and a screenshot of it as a PNG. Its size is the last frame's: the display can be
//! resized (the composer's `set_display_size`), and a frame of the new size replaces the old one
//! whole.
//!
//! **A frame is shared, not copied, once presented**: the held frame is an `Arc`, and a reader
//! ([`Framebuffer::frame`], the live window each frame) takes a reference to it rather than a copy
//! made under the lock the composer's next present waits on (19 MB at a Retina window's size, and
//! the window's copy held SurfaceFlinger's present up while it was made). A present copies the new
//! frame into a spare buffer outside the lock -- the frame before, once no reader holds it -- and
//! swaps it in.
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

pub struct Framebuffer {
    state: Mutex<State>,
    presented: Condvar,
    /// A frame's buffer no reader holds any more, for the next present to copy into.
    spare: Mutex<Option<Vec<u8>>>,
}

struct State {
    width: u32,
    height: u32,
    /// RGBA, 8 bits a channel, `width` pixels a row.
    pixels: Arc<Vec<u8>>,
    frames: u64,
}

impl Framebuffer {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            state: Mutex::new(State { width, height, pixels: Arc::new(vec![0; width as usize * height as usize * 4]), frames: 0 }),
            presented: Condvar::new(),
            spare: Mutex::new(None),
        }
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

    /// Present a `width` x `height` frame, RGBA rows `stride` pixels apart. A frame of another size
    /// than the one held replaces it, black where the source lacks rows or bytes.
    pub fn present_frame(&self, src: &[u8], width: u32, height: u32, stride: u32) {
        let (w, stride) = (width as usize * 4, stride as usize * 4);
        let size = w * height as usize;
        let whole = height == 0 || src.len() >= (height as usize - 1) * stride + w;
        let mut pixels = self.spare.lock().take().filter(|b| b.len() == size).unwrap_or_else(|| vec![0; size]);
        if !whole {
            // Rows the source lacks are the held frame's (when it is of this size).
            let st = self.state.lock();
            if (st.width, st.height) == (width, height) {
                pixels.copy_from_slice(&st.pixels);
            } else {
                pixels.fill(0);
            }
        }
        for y in 0..height as usize {
            let Some(row) = src.get(y * stride..y * stride + w) else { break };
            pixels[y * w..y * w + w].copy_from_slice(row);
        }
        let old = {
            let mut st = self.state.lock();
            (st.width, st.height) = (width, height);
            st.frames += 1;
            std::mem::replace(&mut st.pixels, Arc::new(pixels))
        };
        self.presented.notify_all();
        if let Ok(old) = Arc::try_unwrap(old) {
            *self.spare.lock() = Some(old);
        }
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
        let pixels = Arc::clone(&self.state.lock().pixels);
        pixels.to_vec()
    }

    /// The current frame: its number (the frame count when it was presented), its size and its
    /// pixels (RGBA), taken together -- the pixels shared, not copied.
    #[must_use]
    pub fn frame(&self) -> (u64, u32, u32, Arc<Vec<u8>>) {
        let st = self.state.lock();
        (st.frames, st.width, st.height, Arc::clone(&st.pixels))
    }

    /// The current frame as a PNG (8-bit RGBA; stored, uncompressed deflate blocks: a screenshot
    /// is taken to be looked at, not kept small).
    #[must_use]
    pub fn png(&self) -> Vec<u8> {
        let (_, width, height, pixels) = self.frame();
        let row = width as usize * 4;
        let mut raw = Vec::with_capacity((row + 1) * height as usize);
        for y in 0..height as usize {
            raw.push(0); // filter: none
            raw.extend_from_slice(&pixels[y * row..y * row + row]);
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

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}
