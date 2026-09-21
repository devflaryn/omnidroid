//! The exhaustive sweeps, in their own target because they are slow in a debug build.
//!
//! MEASURED: 27.4 s for the sweep below on this host in `cargo test` debug, against under a second
//! for every other target in this crate. The mutation harness runs debug and runs its command once
//! per row, so this target is deliberately **not** in `tools/mutate.py`'s `TEXTURE` command -- the
//! same reasoning, and the same precedent, as `omni-bionic`'s `tests/stress.rs` being left out of
//! `BIONIC` there.
//!
//! Leaving it out costs nothing in detection: the ETC2 escape has named detectors in
//! `spec_vectors.rs` (the three modes, the underflow case and the channel order), and this sweep is
//! corroboration over the whole input domain rather than the detector for any one row. It still
//! runs in `cargo test --workspace --release`, where it is seconds.

use omni_texture::{decode, CompressedFormat};

const ETC1: CompressedFormat = CompressedFormat::Etc1Rgb8;

/// Every one of the 2^24 base-colour byte triples, in both modes, against an independently written
/// predicate for which of them escape into ETC2.
///
/// The predicate here is derived from the specification text a second time rather than shared with
/// the decoder, so this is a cross-check of the *rule* and not a restatement of the code: a
/// decoder that tested `> 31` but not `< 0`, or tested the channels in the wrong order, disagrees
/// with it. It also establishes the headline proportion: of the 16,777,216 differential-mode base
/// triples, how many are reachable only as ETC2.
#[test]
fn the_etc2_escape_matches_an_independently_derived_predicate() {
    fn escapes(byte: u8) -> bool {
        let base = i32::from(byte >> 3);
        let bits = byte & 0x07;
        // A 3-bit two's-complement value, written out rather than shared with the decoder.
        let delta = if bits >= 4 {
            i32::from(bits) - 8
        } else {
            i32::from(bits)
        };
        let sum = base + delta;
        // Written as two explicit comparisons rather than `!(0..=31).contains(&sum)`, which is
        // what the decoder uses: the value of this predicate is that it was derived from the
        // specification a second time, and spelling it the same way would quietly turn the
        // cross-check into a restatement.
        #[allow(clippy::manual_range_contains)]
        {
            sum < 0 || sum > 31
        }
    }

    let mut out = vec![0u8; 64];
    let mut differential_escapes = 0u64;
    let mut differential_ok = 0u64;

    for value in 0u32..0x0100_0000 {
        let b0 = (value >> 16) as u8;
        let b1 = (value >> 8) as u8;
        let b2 = value as u8;

        // Individual mode (diffbit = 0) can never escape.
        let individual = [b0, b1, b2, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(
            decode(ETC1, &individual, 4, 4, &mut out).is_ok(),
            "individual mode must decode {b0:#04x} {b1:#04x} {b2:#04x}"
        );

        // Differential mode (diffbit = 1).
        let differential = [b0, b1, b2, 0x02, 0x00, 0x00, 0x00, 0x00];
        let result = decode(ETC1, &differential, 4, 4, &mut out);
        let expected_escape = escapes(b0) || escapes(b1) || escapes(b2);
        assert_eq!(
            result.is_err(),
            expected_escape,
            "differential {b0:#04x} {b1:#04x} {b2:#04x}"
        );
        if expected_escape {
            differential_escapes += 1;
        } else {
            differential_ok += 1;
        }
    }

    assert_eq!(differential_escapes + differential_ok, 0x0100_0000);
    // A channel byte escapes for exactly 16 of its 256 values, counted from the encoding: base 0
    // needs a delta below 0 (4 of the 8 deltas), base 1 below -1 (3), base 2 (2), base 3 (1), and
    // symmetrically base 31 needs a delta above 0 (3), base 30 (2), base 29 (1) -- 16 in all, so
    // 240 do not escape. A triple escapes unless all three do not: 256^3 - 240^3 = 2,953,216.
    assert_eq!(differential_escapes, 256 * 256 * 256 - 240 * 240 * 240);
    assert_eq!(differential_escapes, 2_953_216);
}

/// Every one of the 2^32 pixel-index words, on a fixed black block, never writes a texel outside
/// `[0, 255]` and never leaves a texel unwritten.
///
/// The index word is entirely guest-controlled and is the one field with no validity condition at
/// all -- every bit pattern is legal -- so the property to assert is that the decoder is total over
/// it. Swept over all 2^32 words would be hours; this walks every value of each of the four bytes
/// against every value of one neighbour, which reaches all 256 values of all four bytes and all
/// 65,536 values of each adjacent pair.
#[test]
fn every_pixel_index_byte_pair_decodes_totally() {
    let sentinel = 0xA5u8;
    let mut out = vec![sentinel; 64];
    for position in 0..3usize {
        for first in 0u8..=255 {
            for second in 0u8..=255 {
                let mut bytes = [0x88, 0x88, 0x88, 0xFC, 0x00, 0x00, 0x00, 0x00];
                bytes[4 + position] = first;
                bytes[5 + position] = second;
                out.fill(sentinel);
                decode(ETC1, &bytes, 4, 4, &mut out).expect("any index word decodes");
                for texel in 0..16 {
                    assert_eq!(out[texel * 4 + 3], 0xFF, "alpha at texel {texel}");
                    for channel in 0..3 {
                        assert_ne!(
                            out[texel * 4 + channel],
                            sentinel,
                            "texel {texel} channel {channel} was never written"
                        );
                    }
                }
            }
        }
    }
}
