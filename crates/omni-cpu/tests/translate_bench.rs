//! Translation throughput: how fast the backend turns guest code it has never seen into host code.
//! A measurement, not an assertion (`#[ignore]`d):
//!
//! ```text
//! cargo test -p omni-cpu --release --features dynarmic --test translate_bench -- --ignored --nocapture
//! ```
//!
//! Why it exists: an app's startup on the arm64 host is translation-bound -- Roblox 2.740.931's main
//! thread spends two thirds of its time in dynarmic (the register allocator above all) and a tenth
//! running translated code between loading its packed `libzstd-jni` and initialising it, and that
//! library's own worker gives it about 20 s (`docs/research/2026-09-29-libzstd-jni-16k.md`). The
//! code here is shaped like that library's: long runs of mixed-boolean arithmetic over a dozen
//! registers, a few stack loads and stores, each block run once.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use std::time::Instant;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::{ExitReason, GuestCpu, RunLimit};
use omni_mem::{CommitPolicy, Placement, Protection};

const BLOCKS: usize = 20_000;
const OPS_PER_BLOCK: usize = 40;

/// A deterministic xorshift, so every run translates the same code.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn reg(&mut self) -> u32 {
        (self.next() % 12) as u32 + 1 // x1..x12; x0 is the stack-like base
    }
}

/// One block: mixed-boolean arithmetic and a few loads and stores against `[x0, #0..0xff8]`, then
/// a branch to the next block.
fn block(rng: &mut Rng) -> Vec<u32> {
    let mut out = Vec::with_capacity(OPS_PER_BLOCK + 1);
    for _ in 0..OPS_PER_BLOCK {
        let (d, n, m) = (rng.reg(), rng.reg(), rng.reg());
        let shift = (rng.next() % 4) as u32;
        let w = match rng.next() % 10 {
            0 => 0xCA00_0000 | (m << 16) | (n << 5) | d,                 // eor
            1 => 0xAA00_0000 | (m << 16) | (n << 5) | d,                 // orr
            2 => 0x8A00_0000 | (m << 16) | (n << 5) | d,                 // and
            3 => 0x8B00_0000 | (m << 16) | (shift << 10) | (n << 5) | d, // add, lsl
            4 => 0xAA20_0000 | (m << 16) | (n << 5) | d,                 // orn
            5 => 0xCA20_0000 | (m << 16) | (n << 5) | d,                 // eon
            6 => 0xAA20_03E0 | (m << 16) | d,                            // mvn
            7 => add_imm(d, n, (rng.next() % 64) as u32),
            8 => ldr_imm(d, 0, (rng.next() % 512) as u32 * 8),
            _ => str_imm(n, 0, (rng.next() % 512) as u32 * 8),
        };
        out.push(w);
    }
    out.push(b(1));
    out
}

#[test]
#[ignore = "measurement, not a test"]
fn translation_throughput_on_mixed_boolean_blocks() {
    let g = Guest::new();
    let page = g.space.page_size();
    let words_per_block = OPS_PER_BLOCK + 1;
    let bytes = (BLOCKS * words_per_block + 1) * 4;
    let len = bytes.next_multiple_of(page);
    let code = g.space.map_anonymous(Placement::Anywhere { align: page }, len, Protection::ReadWrite, CommitPolicy::Eager).expect("code");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut program: Vec<u32> = Vec::with_capacity(BLOCKS * words_per_block + 1);
    for _ in 0..BLOCKS {
        program.extend(block(&mut rng));
    }
    program.push(ret(30));
    let image: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
    g.space.write_forced(code, &image).expect("write the code");
    g.space.protect(code, len, Protection::ReadExecute).expect("executable");

    let (mut cpu, sentinel) = g.thread();
    cpu.set_x(x(0), g.data as u64);
    cpu.set_x(x(30), sentinel as u64);
    let start = Instant::now();
    let exit = cpu.run(code, RunLimit::Unlimited).expect("runs");
    let first = start.elapsed();
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit}");

    // Again, translated: what running the same code costs without translating it.
    cpu.set_x(x(0), g.data as u64);
    cpu.set_x(x(30), sentinel as u64);
    let start = Instant::now();
    cpu.run(code, RunLimit::Unlimited).expect("runs again");
    let second = start.elapsed();

    let instructions = BLOCKS * words_per_block;
    println!(
        "translate_bench: {BLOCKS} blocks x {words_per_block} instructions: first run {:.1} ms ({:.0} blocks/s, {:.2} us per instruction translated), second run {:.2} ms",
        first.as_secs_f64() * 1e3,
        BLOCKS as f64 / first.as_secs_f64(),
        first.as_secs_f64() * 1e6 / instructions as f64,
        second.as_secs_f64() * 1e3,
    );
}
