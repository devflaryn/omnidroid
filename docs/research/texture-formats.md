# Which compressed texture formats Roblox actually uses

Produced 2026-09-21 as M6 groundwork, before any decoder was written. Everything below is
reproducible with `python tools/texture_census.py --check`, which re-derives it and exits non-zero
on any disagreement.

## Why the question had to be asked first

`docs/research/graphics-spike.md` §3 measured this host's GPU with
`vkGetPhysicalDeviceFormatProperties`: **ETC2 and ASTC are not supported for sampled images; BC1,
BC3 and BC7 are.** Roblox is an Android application, so runtime transcoding is mandatory
infrastructure — without it there is no correct first frame, only a wrongly-coloured one.

What was *not* known is **which** formats. "ETC2 or ASTC" names two families, not two formats. ETC2
has RGB8, sRGB8, punch-through-alpha, RGBA8/EAC and the R11/RG11 EAC forms in signed and unsigned;
ASTC has fourteen block footprints from 4×4 to 12×12, each in LDR and HDR. Implementing both is
weeks of work and most of it may be dead code. *The import list is the specification*; this is that
principle applied to textures.

## Method

`tools/texture_census.py` walks all 2,365 APK entries and classifies each by **its leading bytes,
never its extension**. Every container found is parsed down to its format field: a KTX1 header's
`glInternalFormat` at offset 28, a DDS `DDS_PIXELFORMAT` FourCC at offset 84 and, for `DX10`, the
`DXGI_FORMAT` at offset 128. KTX2, PVR3, raw ASTC (`0x5CA1AB13`), PKM, PNG, JPEG and RIFF are
detected and counted as well, so "none present" is a measurement rather than an assumption.

For every ETC container it then walks **every 4×4 block** and classifies its mode from the bits
that select it. That is the measurement that sets the decoder's scope, and the reason it is needed
is in §3 below.

The check asserts the **exact path-to-format mapping of all 66 texture containers**, not a total.
HANDOFF records two occasions in this project where a total stayed right while the set under it was
wrong in both directions.

### Why an extension-driven census gets this wrong

`docs/research/apk-analysis.md` §8.2 lists `.ktx` 26 and `.tex` 12 as separate rows, the second
described only as "Roblox texture container". **The twelve `.tex` files are KTX1 files** — they are
the skybox, `assets/android/textures/sky/{indoor512,sky512}_{bk,dn,ft,lf,rt,up}.tex`, and their
payload is ETC1 like the rest. An extension-driven census therefore reports 26 ETC containers where
there are **38**, and misses every skybox face. That row in `apk-analysis.md` is not wrong — the
file-type census it belongs to is by extension by design — but anything scoping a decoder from it
would be.

## 1. What the APK ships

| container | files | bytes | host GPU samples it? |
|---|---:|---:|---|
| KTX1, `GL_ETC1_RGB8_OES` (`0x8D64`) | **38** | 6,514,292 | **no** |
| DDS `DXT5` (BC3) | 14 | 618,000 | yes |
| DDS `DX10`, `DXGI_FORMAT_R8_UNORM` (61) | 4 | 2,788,419 | yes |
| DDS uncompressed BGRA8 | 3 | 1,468,156 | yes |
| DDS `DXT1` (BC1) | 2 | 87,664 | yes |
| DDS uncompressed L8 | 2 | 699,326 | yes |
| DDS `DXT3` (BC2) | 1 | 5,648 | yes |
| DDS uncompressed A8L8 | 1 | 32,896 | yes |
| DDS uncompressed L8 (alpha-flagged) | 1 | 384 | yes |

And, counted so their absence is a measurement:

| format family | occurrences in the APK |
|---|---:|
| ASTC, in any container or as a raw `.astc` file | **0** |
| ETC2 or EAC, in any of their eleven GL forms | **0** |
| PVRTC | **0** |
| KTX2 | **0** (the KTX2 identifier occurs once, inside `libroblox.so`, as the engine's own sniffing constant) |

**So exactly one compressed format in the APK needs transcoding: ETC1.** Every other
block-compressed asset is already in the BC family the host supports.

`assets/android/textures/` is the whole ETC set and it has **no alternative encoding in the APK**:
the skybox and the 25 water normal maps exist only as ETC1. `plastic/` ships `diffuse.dds` and
`normal.dds` alongside `normaldetail.ktx`, but those are different textures, not the same texture
twice.

## 2. What the engine will ask the CDN for

The APK's baked assets are a lower bound: Roblox streams most content at run time. That half is
settled by a literal JSON table in `libroblox.so`'s `.rodata`, which pairs a `semantic` with a
`clientCompressionCaps`:

```json
{ "semantic": [ "generic" ], "clientCompressionCaps": [ "dxt"  ], "mipPack": [ "all" ] },
{ "semantic": [ "generic" ], "clientCompressionCaps": [ "etc"  ], "mipPack": [ "all" ] },
{ "semantic": [ "generic" ], "clientCompressionCaps": [ "etc2" ], "mipPack": [ "all" ] },
{ "semantic": [ "generic" ], "clientCompressionCaps": [ "uncompressed" ], "mipPack": [ "all" ] },
```

The whole vocabulary is **`dxt`, `etc`, `etc2`, `uncompressed`** — twelve entries, that set crossed
with three `mipPack` values. `"astc"` **does not occur in `libroblox.so` at all** (`d.count(b'"astc"')
== 0`), and neither does `"bc7"` or `"pvr"`. The engine also carries the matching capability gates
as telemetry keys — `gpu/supportsTextureDXT`, `gpu/supportsTextureETC1`, `gpu/supportsTextureETC2`,
`gpu/supportsTextureASTC` — and a log line `[channel] Caps: Texture: DXT %d PVR %d ETC1 %d ETC2 %d
Half %d`.

**Consequence, and it is a design lever rather than a constraint.** Omnidroid is the thing that
answers those capability queries, because it implements the GLES/Vulkan surface the engine talks to.
If it advertises DXT — which the host GPU genuinely has — and does not advertise ETC2 or ASTC, the
engine requests `dxt` from the CDN and streamed content needs **no transcoding at all**. The APK's
38 baked ETC1 files are fixed whatever we advertise, which is why the ETC1 decoder is mandatory and
an ETC2 or ASTC decoder currently is not.

This is INFERENCE about engine behaviour, drawn from strings; it is not verified by running the
engine, and it will not be until M6 has a renderer. What is VERIFIED is the vocabulary itself and
the absence of `"astc"` from it.

Note also that ASTC appears in the engine as an **encoder**, not a decoder: the Vulkan shader pack
contains `CompressAstc4x4RgbCS`, `CompressAstc6x6RgUncorrelatedCS`, `CompressAstc8x8RgbCS` and four
more (`apk-analysis.md` §7.4). Those compress *into* ASTC on the GPU for its own texture streaming,
which is a path that requires the device to support ASTC storage images in the first place. It is
not a decode obligation on us.

## 3. ETC1 or ETC2? The block-level measurement

`GL_ETC1_RGB8_OES` and `GL_COMPRESSED_RGB8_ETC2` share a container, a block size and most of a bit
layout. They differ in one place, and it is the place where a decoder fails silently.

With the block's `diffbit` set, each channel carries a 5-bit base and a 3-bit two's-complement
delta. **ETC1 forbids an encoder from producing a sum outside `[0, 31]`; ETC2 reuses exactly those
encodings as escapes** into its three added modes — red out of range selects T mode, then green
selects H, then blue selects planar. An ETC1 decoder that reads such a block as differential
produces a plausible colour and no error.

So the census walks every block:

| mode | blocks | share |
|---|---:|---:|
| individual (ETC1) | 133,801 | 16.441% |
| differential (ETC1) | 680,001 | 83.559% |
| T mode (ETC2 only) | **0** | 0.000% |
| H mode (ETC2 only) | **0** | 0.000% |
| planar (ETC2 only) | **0** | 0.000% |
| **total** | **813,802** | |

Zero, over 813,802 real blocks. The header said ETC1 and the payload agrees with it.

Those blocks decode to **52,079,224 bytes** of RGBA8 — the *images*. Whole blocks would be
52,083,328 bytes; the 4,104-byte difference is the 2×2 and 1×1 mip level of each of the 38 textures,
each of which still occupies a full 4×4 block. 27 padded texels × 38 files = 1,026.

## 4. What was built, and what was refused

`crates/omni-texture` — a zero-dependency, `#![no_std]`, non-allocating crate. See **D27** for why
it is its own crate rather than a module of `omni-gfx`, and for the RGBA8-versus-BC1 decision.

**Implemented:** `GL_ETC1_RGB8_OES` → RGBA8, both modes, both sub-block splits, all eight intensity
modifier tables, non-multiple-of-four extents clipped.

**Refused by name, never approximated:**

- ETC2's T, H and planar modes, each named individually in the error with the block index.
- Every ETC2, EAC, ASTC and S3TC GL enum, by its specification name — `from_gl_internal_format`
  carries the full table so that a refusal reads "GL_COMPRESSED_RGBA8_ETC2_EAC is not decoded here"
  rather than "unknown format 0x9278". A refusal that names the format is an instruction for
  whoever hits it.

**Deliberately not built:**

- **Any ETC2, EAC or ASTC decoder.** Zero blocks of any of them exist in the APK and `"astc"` is not
  in the engine's compression vocabulary. Building them now would be dead code whose correctness
  nobody could check against real content.
- **A container parser.** The engine parses KTX itself (`Invalid KTX header`, `Unsupported KTX2:
  unknown format ({})` and a dozen more are its strings); it hands us block data through
  `glCompressedTexImage2D`. A KTX reader here would be scope this crate does not have. The tests
  carry a minimal one for reading the APK's own files.
- **An ETC1 → BC1 re-encoder.** The argument is in D27: it trades an exactly-verifiable decode for
  an encode whose only oracle would be somebody else's encoder.
- **PNG, JPEG and WebP decoding.** The engine ships libpng, libjpeg-turbo and libwebp and their
  error strings are in `libroblox.so`; the 273 PNGs and 3 JPEGs in `assets/` are its problem, not
  ours.

## 5. Measured cost

ETC1 → RGBA8 over the APK's entire baked set — 38 textures, 361 mip levels, 813,802 blocks,
52,079,224 bytes of output — on this host, release build:

| | |
|---|---|
| n | 11 runs |
| min | 31.30 ms |
| **median** | **31.72 ms** |
| max | 32.11 ms |
| per block | 39.0 ns |
| output throughput | 1,642 MB/s |

Single-threaded, one core, no SIMD. Reproduce with
`cargo test -p omni-texture --release -- --ignored --nocapture`. The whole baked texture set is
about **32 ms of one core**, which is a load-path cost worth knowing and not one worth optimising
against anything yet.

## 6. What this does not settle

- **Whether the engine loads the ETC1 assets at all when ETC1 is not advertised.** It might skip
  them, in which case there is no skybox rather than a wrong one. Not determinable statically;
  settled by running M6.
- **Any format a *streamed* asset arrives in.** §2's reasoning says `dxt`, but that depends on what
  Omnidroid advertises and on server-side behaviour neither of which has been exercised.
- **HDR and 3D textures.** `VK_EXT_texture_compression_astc_hdr` is queried by the engine and is
  **false** on this GPU (graphics-spike §3). Nothing in the APK needs it.
- **Whether a stock, Play-signed APK ships the same texture set.** The fixture is cheat-injected
  (D6). The injected content is `assets/gloop/dlt.zip`, and it was
  opened and checked for this: 74 entries, of which **72 are PNG** by leading bytes (55 named
  `.png`, 18 with no extension at all, and one `.rbxmx`) and 2 are JSON. **No compressed texture in
  it**, so it does not affect this census — but the stock APK has still never been supplied.
