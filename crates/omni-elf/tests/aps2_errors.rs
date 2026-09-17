//! Negative tests for the `APS2` decoder and the ELF header validator.
//!
//! These need no APK: they build blobs from the in-crate encoder and then damage them. Every
//! case must produce a typed error — never a panic, and never a silent success, because a
//! decoder that quietly returns fewer relocations than the file declares is the exact failure
//! this crate exists to prevent.

use omni_elf::aps2::{
    self, encode_rela_ungrouped, encode_sleb128, PackedFormat, Sleb128Decoder, APS2_MAGIC,
    RELOCATION_GROUPED_BY_ADDEND_FLAG, RELOCATION_GROUPED_BY_INFO_FLAG,
    RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG, RELOCATION_GROUP_HAS_ADDEND_FLAG,
};
use omni_elf::consts::*;
use omni_elf::{ElfError, Rela};

fn rela(offset: u64, sym: u32, ty: u32, addend: i64) -> Rela {
    Rela {
        r_offset: offset,
        r_info: ((sym as u64) << 32) | ty as u64,
        r_addend: addend,
    }
}

fn sample() -> Vec<Rela> {
    vec![
        rela(0x1000, 0, R_AARCH64_RELATIVE, 0x1000),
        rela(0x1008, 0, R_AARCH64_RELATIVE, -0x40),
        rela(0x1010, 7, R_AARCH64_GLOB_DAT, 0),
        // A backwards offset step and a large negative addend, so the round trip exercises
        // signed deltas in both fields.
        rela(0x0f00, 9, R_AARCH64_ABS64, -0x7fff_ffff_ffff),
        rela(0x2000, 0, R_AARCH64_RELATIVE, 0x2000),
    ]
}

#[test]
fn a_well_formed_blob_round_trips_exactly() {
    let relocs = sample();
    let blob = encode_rela_ungrouped(&relocs);
    let decoded = aps2::decode_rela(&blob).expect("the encoder must produce a decodable blob");
    assert_eq!(decoded.relocations, relocs);
    assert_eq!(decoded.summary.declared_count, relocs.len() as u64);
    assert_eq!(decoded.summary.decoded_count, relocs.len() as u64);
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(decoded.summary.bytes_total, blob.len());
    assert_eq!(decoded.summary.group_count, 1);
    assert_eq!(
        decoded.summary.observed_group_flags,
        RELOCATION_GROUP_HAS_ADDEND_FLAG
    );
}

#[test]
fn an_empty_relocation_set_is_legal() {
    let blob = encode_rela_ungrouped(&[]);
    let decoded = aps2::decode_rela(&blob).unwrap();
    assert!(decoded.relocations.is_empty());
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(decoded.summary.group_count, 0);
}

#[test]
fn bad_magic_is_rejected() {
    let mut blob = encode_rela_ungrouped(&sample());
    blob[3] = b'1'; // "APS1"
    assert_eq!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2BadMagic(*b"APS1")
    );
    // And a blob shorter than the magic.
    assert_eq!(
        aps2::decode_rela(b"AP").unwrap_err(),
        ElfError::Aps2TooShort(2)
    );
    assert_eq!(aps2::decode_rela(&[]).unwrap_err(), ElfError::Aps2TooShort(0));
}

#[test]
fn truncation_at_every_length_is_an_error_and_never_a_panic() {
    let blob = encode_rela_ungrouped(&sample());
    // Every proper prefix must fail. Chopping one byte at a time walks the truncation point
    // through the count, the initial offset, the group header and every per-relocation field.
    for cut in 0..blob.len() {
        let err = aps2::decode_rela(&blob[..cut])
            .unwrap_err_or_else(|| panic!("prefix of length {cut} must not decode"));
        assert!(
            matches!(
                err,
                ElfError::Aps2Truncated { .. }
                    | ElfError::Aps2TooShort(_)
                    | ElfError::Aps2BadMagic(_)
                    | ElfError::Aps2CountMismatch { .. }
                    | ElfError::Aps2GroupOverrun { .. }
            ),
            "prefix of length {cut} gave an unexpected error: {err}"
        );
    }
    // And the whole thing still decodes, so the loop above was testing something.
    assert!(aps2::decode_rela(&blob).is_ok());
}

/// `Result::unwrap_err` needs `T: Debug`; this keeps the message specific without that bound.
trait UnwrapErrOrElse<T, E> {
    fn unwrap_err_or_else(self, f: impl FnOnce() -> E) -> E;
}

impl<T, E> UnwrapErrOrElse<T, E> for Result<T, E> {
    fn unwrap_err_or_else(self, f: impl FnOnce() -> E) -> E {
        match self {
            Ok(_) => f(),
            Err(e) => e,
        }
    }
}

#[test]
fn trailing_bytes_are_an_error() {
    let mut blob = encode_rela_ungrouped(&sample());
    let good = blob.len();
    blob.push(0);
    assert_eq!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2TrailingBytes {
            consumed: good,
            total: good + 1,
            remaining: 1,
        }
    );
    // A whole extra group's worth of junk must not be mistaken for more relocations either.
    blob.extend_from_slice(&[0x7f, 0x01, 0x02, 0x03]);
    assert!(matches!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2TrailingBytes { .. }
    ));
}

#[test]
fn a_declared_count_larger_than_the_stream_is_an_error() {
    // Declare six relocations but encode a single group of five.
    let relocs = sample();
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(relocs.len() as i64 + 1, &mut blob);
    encode_sleb128(0, &mut blob);
    encode_sleb128(relocs.len() as i64, &mut blob);
    encode_sleb128(RELOCATION_GROUP_HAS_ADDEND_FLAG as i64, &mut blob);
    let mut offset = 0u64;
    let mut addend = 0i64;
    for r in &relocs {
        encode_sleb128(r.r_offset.wrapping_sub(offset) as i64, &mut blob);
        offset = r.r_offset;
        encode_sleb128(r.r_info as i64, &mut blob);
        encode_sleb128(r.r_addend - addend, &mut blob);
        addend = r.r_addend;
    }
    // The stream ends after five, so the decoder runs out looking for a sixth group.
    assert!(matches!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2Truncated { .. }
    ));
}

#[test]
fn a_declared_count_smaller_than_the_stream_is_an_error() {
    let relocs = sample();
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(relocs.len() as i64 - 1, &mut blob); // declare four
    encode_sleb128(0, &mut blob);
    encode_sleb128(relocs.len() as i64, &mut blob); // but group says five
    encode_sleb128(RELOCATION_GROUP_HAS_ADDEND_FLAG as i64, &mut blob);
    for _ in &relocs {
        encode_sleb128(8, &mut blob);
        encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
        encode_sleb128(0, &mut blob);
    }
    assert_eq!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2GroupOverrun {
            group_index: 0,
            size: 5,
            would_be: 5,
            declared: 4,
        }
    );
}

#[test]
fn a_zero_or_negative_group_size_is_an_error_rather_than_a_hang() {
    for size in [0i64, -1, -1000] {
        let mut blob = Vec::from(APS2_MAGIC);
        encode_sleb128(3, &mut blob);
        encode_sleb128(0, &mut blob);
        encode_sleb128(size, &mut blob);
        encode_sleb128(RELOCATION_GROUPED_BY_INFO_FLAG as i64, &mut blob);
        encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
        assert_eq!(
            aps2::decode_rela(&blob).unwrap_err(),
            ElfError::Aps2BadGroupSize {
                group_index: 0,
                size,
            },
            "group size {size}"
        );
    }
}

#[test]
fn a_negative_relocation_count_is_an_error() {
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(-5, &mut blob);
    encode_sleb128(0, &mut blob);
    assert_eq!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2NegativeCount(-5)
    );
}

#[test]
fn unknown_group_flag_bits_are_an_error() {
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(1, &mut blob);
    encode_sleb128(0, &mut blob);
    encode_sleb128(1, &mut blob);
    encode_sleb128(0x30, &mut blob); // bits 4 and 5 are not defined
    assert_eq!(
        aps2::decode_rela(&blob).unwrap_err(),
        ElfError::Aps2UnknownGroupFlags {
            group_index: 0,
            unknown: 0x30,
        }
    );
}

#[test]
fn addends_in_a_dt_android_rel_blob_are_an_error() {
    let blob = encode_rela_ungrouped(&sample());
    // The same bytes read as DT_ANDROID_REL must be refused: REL has no addend field, so
    // accepting the stream would silently shift every subsequent value.
    assert_eq!(
        aps2::decode_rel(&blob).unwrap_err(),
        ElfError::Aps2AddendInRelFormat { group_index: 0 }
    );
    assert!(aps2::decode(&blob, PackedFormat::Rela).is_ok());
}

#[test]
fn grouped_by_addend_applies_one_delta_to_the_whole_group() {
    // Hand-built, because no library in the target APK exercises this flag and the two plausible
    // readings of the format differ here: bionic's `for_all_packed_relocs` reads the delta once
    // per *group*, not once per relocation.
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(3, &mut blob);
    encode_sleb128(0x1000, &mut blob); // initial r_offset
    encode_sleb128(3, &mut blob); // group_size
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG
            | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
            | RELOCATION_GROUPED_BY_ADDEND_FLAG
            | RELOCATION_GROUP_HAS_ADDEND_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(8, &mut blob); // shared offset delta
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob); // shared r_info
    encode_sleb128(0x4242, &mut blob); // one shared addend delta
    let decoded = aps2::decode_rela(&blob).unwrap();
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(
        decoded.relocations,
        vec![
            rela(0x1008, 0, R_AARCH64_RELATIVE, 0x4242),
            rela(0x1010, 0, R_AARCH64_RELATIVE, 0x4242),
            rela(0x1018, 0, R_AARCH64_RELATIVE, 0x4242),
        ],
        "all three relocations share the single group addend"
    );
}

#[test]
fn a_group_without_has_addend_resets_the_running_addend_to_zero() {
    // Group 1 carries addends; group 2 does not, so its relocations must have addend 0 rather
    // than inheriting group 1's running total.
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(4, &mut blob);
    encode_sleb128(0x1000, &mut blob);

    encode_sleb128(2, &mut blob);
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG | RELOCATION_GROUP_HAS_ADDEND_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
    encode_sleb128(8, &mut blob);
    encode_sleb128(0x500, &mut blob);
    encode_sleb128(8, &mut blob);
    encode_sleb128(0x100, &mut blob); // running addend now 0x600

    encode_sleb128(2, &mut blob);
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(8, &mut blob);
    encode_sleb128(R_AARCH64_GLOB_DAT as i64, &mut blob);

    let decoded = aps2::decode_rela(&blob).unwrap();
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(
        decoded.relocations,
        vec![
            rela(0x1008, 0, R_AARCH64_RELATIVE, 0x500),
            rela(0x1010, 0, R_AARCH64_RELATIVE, 0x600),
            rela(0x1018, 0, R_AARCH64_GLOB_DAT, 0),
            rela(0x1020, 0, R_AARCH64_GLOB_DAT, 0),
        ]
    );
    // GROUPED_BY_ADDEND without HAS_ADDEND must also reset, and must read nothing.
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(1, &mut blob);
    encode_sleb128(0x1000, &mut blob);
    encode_sleb128(1, &mut blob);
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG
            | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
            | RELOCATION_GROUPED_BY_ADDEND_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(8, &mut blob);
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
    let decoded = aps2::decode_rela(&blob).unwrap();
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(
        decoded.relocations,
        vec![rela(0x1008, 0, R_AARCH64_RELATIVE, 0)]
    );
}

#[test]
fn offset_deltas_may_be_negative() {
    let relocs = vec![
        rela(0x2000, 0, R_AARCH64_RELATIVE, 0),
        rela(0x1000, 0, R_AARCH64_RELATIVE, 0),
        rela(0x3000, 0, R_AARCH64_RELATIVE, 0),
    ];
    let blob = encode_rela_ungrouped(&relocs);
    assert_eq!(aps2::decode_rela(&blob).unwrap().relocations, relocs);
}

#[test]
fn every_single_byte_corruption_either_decodes_differently_or_errors() {
    // Not "every corruption is detected" — a delta encoding cannot detect all of them — but the
    // decoder must never panic, and it must never return the original relocations unchanged
    // while also claiming byte-exact consumption of a blob it did not actually match.
    let relocs = sample();
    let blob = encode_rela_ungrouped(&relocs);
    let mut silent_identical = 0usize;
    for i in 0..blob.len() {
        for bit in 0..8 {
            let mut bad = blob.clone();
            bad[i] ^= 1 << bit;
            // An error is a perfectly good outcome; what matters is that a success stays
            // self-consistent and that corruption is not silently invisible.
            if let Ok(d) = aps2::decode_rela(&bad) {
                if d.relocations == relocs {
                    silent_identical += 1;
                }
                assert_eq!(d.summary.bytes_consumed, d.summary.bytes_total);
                assert_eq!(d.summary.decoded_count, d.relocations.len() as u64);
                assert_eq!(d.summary.declared_count, d.summary.decoded_count);
            }
        }
    }
    // Only the redundant high bits of the final byte of a SLEB128 value can flip without
    // changing anything. Keeping this bounded means a decoder that ignored whole fields would
    // show up here.
    assert!(
        silent_identical <= 8,
        "{silent_identical} single-bit corruptions decoded to the identical relocation set"
    );
}

#[test]
fn sleb128_decoder_reports_position_and_remaining() {
    let mut buf = Vec::new();
    encode_sleb128(300, &mut buf); // two bytes
    encode_sleb128(-1, &mut buf); // one byte
    let mut dec = Sleb128Decoder::new(&buf);
    assert_eq!(dec.len(), 3);
    assert_eq!(dec.pop_front().unwrap(), 300);
    assert_eq!(dec.position(), 2);
    assert_eq!(dec.remaining(), 1);
    assert_eq!(dec.pop_front().unwrap(), -1);
    assert_eq!(dec.remaining(), 0);
    assert!(dec.pop_front().is_err());
}

// ---------------------------------------------------------------------------------------------
// Header validation
// ---------------------------------------------------------------------------------------------

/// The smallest byte sequence that reaches each validation gate, built field by field so each
/// rejection is provably about the field it names.
fn minimal_header(class: u8, data: u8, e_type: u16, machine: u16) -> Vec<u8> {
    let mut v = vec![0u8; SIZEOF_EHDR];
    v[0..4].copy_from_slice(&ELF_MAGIC);
    v[4] = class;
    v[5] = data;
    v[6] = EV_CURRENT;
    v[16..18].copy_from_slice(&e_type.to_le_bytes());
    v[18..20].copy_from_slice(&machine.to_le_bytes());
    v[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    v
}

#[test]
fn header_validation_names_the_offending_value() {
    use omni_elf::ElfImage;

    assert_eq!(
        ElfImage::parse(b"not an elf file at all, really").unwrap_err(),
        ElfError::BadMagic([b'n', b'o', b't', b' '])
    );
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS32, ELFDATA2LSB, ET_DYN, EM_AARCH64)).unwrap_err(),
        ElfError::UnsupportedClass(ELFCLASS32)
    );
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2MSB, ET_DYN, EM_AARCH64)).unwrap_err(),
        ElfError::UnsupportedEncoding(ELFDATA2MSB)
    );
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2LSB, ET_EXEC, EM_AARCH64)).unwrap_err(),
        ElfError::UnsupportedObjectType(ET_EXEC)
    );
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2LSB, ET_REL, EM_AARCH64)).unwrap_err(),
        ElfError::UnsupportedObjectType(ET_REL)
    );
    // x86-64 is EM_X86_64 = 62.
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, 62)).unwrap_err(),
        ElfError::UnsupportedMachine(62)
    );
    // 32-bit Arm is EM_ARM = 40, the most likely wrong-ABI mistake in an Android APK.
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, 40)).unwrap_err(),
        ElfError::UnsupportedMachine(40)
    );
    // A valid header with no program headers has no PT_DYNAMIC.
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, EM_AARCH64)).unwrap_err(),
        ElfError::NoDynamicSegment
    );
    // A bad e_ident[EI_VERSION].
    let mut v = minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, EM_AARCH64);
    v[6] = 7;
    assert_eq!(
        ElfImage::parse(&v).unwrap_err(),
        ElfError::UnsupportedIdentVersion(7)
    );
    // A truncated file must not panic.
    for cut in 0..SIZEOF_EHDR {
        let v = minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, EM_AARCH64);
        assert!(ElfImage::parse(&v[..cut]).is_err(), "prefix of {cut} bytes");
    }
}

#[test]
fn a_bad_phentsize_is_rejected() {
    use omni_elf::ElfImage;
    let mut v = minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, EM_AARCH64);
    v[54..56].copy_from_slice(&32u16.to_le_bytes()); // e_phentsize
    v[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    assert_eq!(ElfImage::parse(&v).unwrap_err(), ElfError::BadPhentsize(32));
}
