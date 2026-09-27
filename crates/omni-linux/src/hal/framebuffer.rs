//! The display's framebuffer on the host: what the host composer presents (D3), a frame count to
//! wait on, and a screenshot of it as a PNG.
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    state: Mutex<State>,
    presented: Condvar,
}

struct State {
    /// RGBA, 8 bits a channel, `width` pixels a row.
    pixels: Vec<u8>,
    frames: u64,
}

impl Framebuffer {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, state: Mutex::new(State { pixels: vec![0; width as usize * height as usize * 4], frames: 0 }), presented: Condvar::new() }
    }

    /// Present a frame of RGBA rows `stride` pixels apart (a buffer's stride may exceed the
    /// display's width); rows or bytes the source lacks are left as they were.
    pub fn present_rgba(&self, src: &[u8], stride: u32) {
        let (w, stride) = (self.width as usize * 4, stride as usize * 4);
        let mut st = self.state.lock();
        for y in 0..self.height as usize {
            let Some(row) = src.get(y * stride..y * stride + w) else { break };
            st.pixels[y * w..y * w + w].copy_from_slice(row);
        }
        st.frames += 1;
        self.presented.notify_all();
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
        self.state.lock().pixels.clone()
    }

    /// The current frame as a PNG (8-bit RGBA; stored, uncompressed deflate blocks: a screenshot
    /// is taken to be looked at, not kept small).
    #[must_use]
    pub fn png(&self) -> Vec<u8> {
        let pixels = self.pixels();
        let row = self.width as usize * 4;
        let mut raw = Vec::with_capacity((row + 1) * self.height as usize);
        for y in 0..self.height as usize {
            raw.push(0); // filter: none
            raw.extend_from_slice(&pixels[y * row..y * row + row]);
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
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
