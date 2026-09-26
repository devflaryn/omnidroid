# Which compressed texture formats Roblox uses

Measured on the stock `Roblox-2.738.1397.apk` with `python tools/texture_census.py --check`, which
classifies every entry by its leading bytes (never its extension), parses KTX1/DDS format fields,
walks every ETC 4x4 block, and asserts the exact path-to-format mapping of all 66 texture
containers. Re-run 2026-09-26 on the stock APK: every texture figure below matches; the only
difference is the entry count (2,382 stock vs. the script's `EXPECTED_ENTRY_COUNT = 2365`, which
was taken from the earlier modified fixture).

Why it matters: the host GPU samples neither ETC2 nor ASTC (`graphics-spike.md` §3), so any such
texture must be transcoded.

## 1. What the APK ships

| Container / format | Files | Bytes | Host GPU samples it? |
|---|---:|---:|---|
| KTX1, `GL_ETC1_RGB8_OES` (`0x8D64`) | **38** | 6,514,292 | **no** |
| DDS `DXT5` (BC3) | 14 | 618,000 | yes |
| DDS `DX10`, `DXGI_FORMAT_R8_UNORM` | 4 | 2,788,419 | yes |
| DDS uncompressed BGRA8 | 3 | 1,468,156 | yes |
| DDS `DXT1` (BC1) | 2 | 87,664 | yes |
| DDS uncompressed L8 | 2 | 699,326 | yes |
| DDS `DXT3` (BC2) | 1 | 5,648 | yes |
| DDS uncompressed A8L8 | 1 | 32,896 | yes |
| DDS uncompressed L8 (alpha-flagged) | 1 | 384 | yes |

Zero ASTC (any container or raw), zero ETC2/EAC, zero PVRTC, zero KTX2 files. **ETC1 is the only
format that needs transcoding.** The 38 ETC1 files are all of `assets/android/textures/`,
including the skybox: the 12 `.tex` files (`sky/{indoor512,sky512}_{bk,dn,ft,lf,rt,up}.tex`) are
KTX1, so a census by extension (`apk-analysis.md` §8.2: `.ktx` 26, `.tex` 12) undercounts ETC
containers. None has an alternative encoding in the APK.

## 2. What the engine asks the CDN for

A JSON table in `libroblox.so` `.rodata` pairs `semantic` with `clientCompressionCaps`. The whole
vocabulary is `dxt`, `etc`, `etc2`, `uncompressed`; `"astc"`, `"bc7"` and `"pvr"` do not occur.
The engine also has telemetry keys `gpu/supportsTextureDXT/ETC1/ETC2/ASTC`. Inference (not run):
if Omnidroid advertises DXT and not ETC2/ASTC, streamed content arrives as `dxt` and needs no
transcoding. ASTC appears only as GPU encoder shaders (`CompressAstc4x4RgbCS` etc.), not as a
decode obligation.

## 3. ETC1 vs. ETC2 at block level

ETC2 reuses ETC1's out-of-range differential encodings as escapes into T, H and planar modes, so
an ETC1 decoder would silently mis-decode them. Measured over all blocks:

| Mode | Blocks | Share |
|---|---:|---:|
| individual | 133,801 | 16.441% |
| differential | 680,001 | 83.559% |
| T / H / planar (ETC2 only) | **0** | 0% |
| total | 813,802 | |

Decoded images: 52,079,224 bytes RGBA8 (whole blocks would be 52,083,328; the difference is the
2x2 and 1x1 mips).

## 4. What was built

`crates/omni-texture` (`#![no_std]`, no dependencies, non-allocating; D27): `GL_ETC1_RGB8_OES` to
RGBA8. ETC2 T/H/planar blocks and every ETC2, EAC, ASTC and S3TC GL enum are refused by name
(`format.rs`), not approximated. Not built: ETC2/EAC/ASTC decoders, a KTX parser (the engine parses
KTX itself), an ETC1-to-BC1 re-encoder (D27), PNG/JPEG/WebP (the engine ships its own libs).

## 5. Measured cost

Whole baked set (38 textures, 361 mip levels, 813,802 blocks) to RGBA8, release build, one core,
11 runs: median **31.72 ms** (min 31.30, max 32.11), 39.0 ns per block, 1,642 MB/s output.
Reproduce: `cargo test -p omni-texture --release -- --ignored --nocapture`.

## 6. Open

- Whether the engine loads the ETC1 assets at all when ETC1 is not advertised.
- The format streamed assets actually arrive in (depends on what Omnidroid advertises).
- `VK_EXT_texture_compression_astc_hdr` is queried by the engine and false on this GPU; nothing
  in the APK needs it.
