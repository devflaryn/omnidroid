//! Decode every compressed texture the real APK ships, and cross-check the census.
//!
//! `tools/texture_census.py` walked the same 813,802 blocks in Python and classified each one's
//! mode. This walks them in Rust through the real decoder. Two independent implementations
//! agreeing on the block count, the format of every container and the absence of any ETC2 mode is
//! what makes either believable (HANDOFF working agreement 3). It is *not* an oracle for the
//! decoded pixels -- the census does not decode -- and the per-pixel correctness lives in
//! `spec_vectors.rs`, derived from the specification.
//!
//! The APK is git-ignored, so this target skips when it is absent, following the convention in
//! `omni-elf/tests/common/mod.rs`. The skip is written straight to the process's stderr rather than
//! through `eprintln!`, which libtest captures and discards for a passing test: a skipped run that
//! looked exactly like a verifying one is the failure this project has already been bitten by
//! (review finding H1). Nothing else in this crate needs the APK, so a clone without it still
//! asserts every specification vector, every refusal and both exhaustive sweeps.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use omni_apk::Apk;
use omni_texture::{decode, decoded_len, CompressedFormat, TextureError};

const APK_NAME: &str = "Roblox-2.738.1397.apk";

/// The KTX1 file identifier, `OES_compressed_ETC1_RGB8_texture` / the KTX 1.1 specification.
const KTX1_IDENTIFIER: [u8; 12] = [
    0xAB, 0x4B, 0x54, 0x58, 0x20, 0x31, 0x31, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
];

/// `GL_ETC1_RGB8_OES`.
const ETC1_INTERNAL_FORMAT: u32 = 0x8D64;

/// Every KTX1 container in the APK, by path. Asserted as a **set**, not a count: HANDOFF records
/// two occasions here where a total stayed right while its membership was wrong in both
/// directions. Note the twelve `.tex` entries -- they are KTX1 files, and an extension-driven
/// census reports 26 of these instead of 38.
const EXPECTED_KTX_PATHS: &[&str] = &[
    "assets/android/textures/plastic/normaldetail.ktx",
    "assets/android/textures/sky/indoor512_bk.tex",
    "assets/android/textures/sky/indoor512_dn.tex",
    "assets/android/textures/sky/indoor512_ft.tex",
    "assets/android/textures/sky/indoor512_lf.tex",
    "assets/android/textures/sky/indoor512_rt.tex",
    "assets/android/textures/sky/indoor512_up.tex",
    "assets/android/textures/sky/sky512_bk.tex",
    "assets/android/textures/sky/sky512_dn.tex",
    "assets/android/textures/sky/sky512_ft.tex",
    "assets/android/textures/sky/sky512_lf.tex",
    "assets/android/textures/sky/sky512_rt.tex",
    "assets/android/textures/sky/sky512_up.tex",
    "assets/android/textures/water/normal_01.ktx",
    "assets/android/textures/water/normal_02.ktx",
    "assets/android/textures/water/normal_03.ktx",
    "assets/android/textures/water/normal_04.ktx",
    "assets/android/textures/water/normal_05.ktx",
    "assets/android/textures/water/normal_06.ktx",
    "assets/android/textures/water/normal_07.ktx",
    "assets/android/textures/water/normal_08.ktx",
    "assets/android/textures/water/normal_09.ktx",
    "assets/android/textures/water/normal_10.ktx",
    "assets/android/textures/water/normal_11.ktx",
    "assets/android/textures/water/normal_12.ktx",
    "assets/android/textures/water/normal_13.ktx",
    "assets/android/textures/water/normal_14.ktx",
    "assets/android/textures/water/normal_15.ktx",
    "assets/android/textures/water/normal_16.ktx",
    "assets/android/textures/water/normal_17.ktx",
    "assets/android/textures/water/normal_18.ktx",
    "assets/android/textures/water/normal_19.ktx",
    "assets/android/textures/water/normal_20.ktx",
    "assets/android/textures/water/normal_21.ktx",
    "assets/android/textures/water/normal_22.ktx",
    "assets/android/textures/water/normal_23.ktx",
    "assets/android/textures/water/normal_24.ktx",
    "assets/android/textures/water/normal_25.ktx",
];

/// `tools/texture_census.py --check`, reproduced here by a different implementation.
const EXPECTED_BLOCKS: u64 = 813_802;
/// The *images*, not the block grid: the last two mip levels of every texture are 2x2 and 1x1 and
/// each still occupies a whole 4x4 block, so whole blocks would be 52,083,328 B. That 4,104-byte
/// gap is what this assertion found when the census reported only the padded figure and called it
/// the decoded size.
const EXPECTED_DECODED_BYTES: u64 = 52_079_224;
const EXPECTED_BLOCK_PADDED_BYTES: u64 = 52_083_328;

fn apk_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate manifest directory has two ancestors")
        .join(APK_NAME)
}

/// `true` when the APK is there; otherwise print an uncaptured notice and `false`.
fn apk_available() -> bool {
    if apk_path().is_file() {
        return true;
    }
    let notice = format!(
        "\nSKIP: omni-texture's real-asset sweep needs {}, which is not at {}. \
         Every assertion against the APK's own textures was skipped; the specification vectors \
         and the exhaustive sweeps in this crate still ran.\n\n",
        APK_NAME,
        apk_path().display()
    );
    let _ = std::io::stderr().write_all(notice.as_bytes());
    let _ = std::io::stderr().flush();
    false
}

/// The fields of a KTX 1.1 header this test needs.
struct Ktx1 {
    internal_format: u32,
    width: u32,
    height: u32,
    levels: u32,
    payload_offset: usize,
}

/// Parse a KTX 1.1 header. Deliberately local to the test: the guest's own engine parses the
/// container (`libroblox.so` carries "Invalid KTX header" and the KTX2 equivalents), and
/// `omni-texture` decodes blocks. Putting a container parser in the crate would be scope it does
/// not have.
fn parse_ktx1(data: &[u8]) -> Option<Ktx1> {
    if data.len() < 64 || data.get(..12)? != KTX1_IDENTIFIER {
        return None;
    }
    let word = |at: usize| -> u32 {
        u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    };
    assert_eq!(
        word(12),
        0x0403_0201,
        "the APK's KTX files are little-endian"
    );
    let kv_bytes = word(60) as usize;
    Some(Ktx1 {
        internal_format: word(28),
        width: word(36),
        height: word(40),
        levels: word(56),
        payload_offset: 64 + kv_bytes,
    })
}

/// Every mip level of an ETC1 texture decodes, and the block and byte counts match the census.
#[test]
fn every_etc1_texture_in_the_apk_decodes() {
    if !apk_available() {
        return;
    }
    let apk = Apk::open(apk_path()).expect("the APK must open");

    let mut found: Vec<String> = Vec::new();
    let mut total_blocks: u64 = 0;
    let mut total_decoded: u64 = 0;
    let mut opaque_texels: u64 = 0;

    let names: Vec<String> = apk
        .entries()
        .iter()
        .map(|entry| entry.name().to_owned())
        .collect();

    for name in names {
        let bytes = match apk.read_named(&name) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let Some(header) = parse_ktx1(&bytes) else {
            continue;
        };
        found.push(name.clone());
        assert_eq!(
            header.internal_format, ETC1_INTERNAL_FORMAT,
            "{name} is a KTX1 container in a format the census did not find"
        );

        let mut offset = header.payload_offset;
        let mut width = header.width;
        let mut height = header.height;
        for level in 0..header.levels.max(1) {
            let size = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize;
            offset += 4;
            let payload = &bytes[offset..offset + size];

            let decoded_bytes = decoded_len(width, height)
                .unwrap_or_else(|e| panic!("{name} level {level} is {width}x{height}: {e}"));
            let mut out = vec![0u8; decoded_bytes];
            decode(CompressedFormat::Etc1Rgb8, payload, width, height, &mut out)
                .unwrap_or_else(|e| panic!("{name} level {level} ({width}x{height}): {e}"));

            for texel in out.chunks_exact(4) {
                if texel[3] == 0xFF {
                    opaque_texels += 1;
                }
            }

            total_blocks += u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4));
            total_decoded += decoded_bytes as u64;

            // Levels are padded to a four-byte boundary; every payload here is a multiple of 8.
            offset += size + (4 - size % 4) % 4;
            width = (width / 2).max(1);
            height = (height / 2).max(1);
        }
    }

    found.sort();
    assert_eq!(
        found, EXPECTED_KTX_PATHS,
        "the set of KTX1 containers in the APK is not the one the record names"
    );
    assert_eq!(
        total_blocks, EXPECTED_BLOCKS,
        "block count disagrees with tools/texture_census.py"
    );
    assert_eq!(
        total_decoded, EXPECTED_DECODED_BYTES,
        "decoded size disagrees with tools/texture_census.py"
    );
    assert_eq!(
        total_blocks * 64,
        EXPECTED_BLOCK_PADDED_BYTES,
        "block-padded size disagrees with tools/texture_census.py"
    );
    assert_eq!(
        opaque_texels * 4,
        EXPECTED_DECODED_BYTES,
        "every decoded texel of an RGB format must be opaque"
    );
}

/// Real content, truncated one byte at a time, refuses rather than decoding a partial block.
///
/// A real KTX payload rather than a synthetic one because the hostile case that matters is a real
/// asset cut short -- by a partial download, a corrupt cache entry, or a modified APK (D6).
#[test]
fn a_truncated_real_texture_refuses_at_every_length() {
    if !apk_available() {
        return;
    }
    let apk = Apk::open(apk_path()).expect("the APK must open");
    let bytes = apk
        .read_named("assets/android/textures/water/normal_01.ktx")
        .expect("the fixture names a file that is in the APK");
    let header = parse_ktx1(&bytes).expect("it is a KTX1 file");
    let size = u32::from_le_bytes([
        bytes[header.payload_offset],
        bytes[header.payload_offset + 1],
        bytes[header.payload_offset + 2],
        bytes[header.payload_offset + 3],
    ]) as usize;
    let payload = &bytes[header.payload_offset + 4..header.payload_offset + 4 + size];
    assert_eq!(size, 32_768, "level 0 of a 256x256 ETC1 image is 32,768 B");

    let mut out = vec![0u8; decoded_len(header.width, header.height).unwrap()];
    // Full payload decodes.
    decode(
        CompressedFormat::Etc1Rgb8,
        payload,
        header.width,
        header.height,
        &mut out,
    )
    .expect("the untruncated level decodes");

    // Every truncation refuses, including the one-byte-short case a length check that used `>`
    // instead of `>=` would let through.
    for cut in [1usize, 2, 7, 8, 9, 1024, size / 2, size - 8, size - 1] {
        let provided = size - cut;
        assert_eq!(
            decode(
                CompressedFormat::Etc1Rgb8,
                &payload[..provided],
                header.width,
                header.height,
                &mut out
            ),
            Err(TextureError::TruncatedBlockData {
                needed: size,
                provided
            }),
            "truncated to {provided} bytes"
        );
    }
}

/// What the transcode costs, measured rather than asserted.
///
/// `#[ignore]`d because it is a measurement, not a property: a throughput assertion on a shared
/// desktop is a flake in both directions. Reproduce with
/// `cargo test -p omni-texture --release -- --ignored --nocapture`.
///
/// It decodes the APK's entire baked ETC1 set -- all 38 textures, every mip level, 813,802 blocks
/// into 52,079,224 B of RGBA8 -- once per run, `RUNS` times, and prints every run so the spread is
/// visible rather than summarised. The figure that matters is per texture on a load path, so the
/// whole-set time and the per-block time are both printed.
#[test]
#[ignore = "a measurement, not a property; see the doc comment"]
fn measure_transcode_throughput() {
    const RUNS: usize = 11;
    if !apk_available() {
        return;
    }
    let apk = Apk::open(apk_path()).expect("the APK must open");

    // Read and parse outside the timed region: this measures the decoder, not the zip reader.
    let mut levels: Vec<(Vec<u8>, u32, u32)> = Vec::new();
    let names: Vec<String> = apk
        .entries()
        .iter()
        .map(|entry| entry.name().to_owned())
        .collect();
    for name in names {
        let Ok(bytes) = apk.read_named(&name) else {
            continue;
        };
        let Some(header) = parse_ktx1(&bytes) else {
            continue;
        };
        let mut offset = header.payload_offset;
        let (mut width, mut height) = (header.width, header.height);
        for _ in 0..header.levels.max(1) {
            let size = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize;
            offset += 4;
            levels.push((bytes[offset..offset + size].to_vec(), width, height));
            offset += size + (4 - size % 4) % 4;
            width = (width / 2).max(1);
            height = (height / 2).max(1);
        }
    }

    let mut out = vec![
        0u8;
        levels
            .iter()
            .map(|(_, w, h)| (*w as usize) * (*h as usize) * 4)
            .max()
            .unwrap()
    ];
    let mut millis = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        for (payload, width, height) in &levels {
            decode(
                CompressedFormat::Etc1Rgb8,
                payload,
                *width,
                *height,
                &mut out,
            )
            .expect("the whole set decodes");
        }
        millis.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    millis.sort_by(|a, b| a.partial_cmp(b).expect("no NaNs"));
    let median = millis[RUNS / 2];
    println!(
        "omni-texture ETC1 -> RGBA8: {} levels, {} blocks, {} B out; n={RUNS} runs, min {:.2} ms, median {:.2} ms, max {:.2} ms; {:.1} ns/block, {:.1} MB/s of output",
        levels.len(),
        EXPECTED_BLOCKS,
        EXPECTED_DECODED_BYTES,
        millis[0],
        median,
        millis[RUNS - 1],
        median * 1e6 / EXPECTED_BLOCKS as f64,
        EXPECTED_DECODED_BYTES as f64 / (median / 1000.0) / 1e6,
    );
}
