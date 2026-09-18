//! Negative tests for the `APS2` decoder and the ELF header validator.
//!
//! These need no APK: they build blobs from the in-crate encoder and then damage them. Every
//! case must produce a typed error — never a panic, and never a silent success, because a
//! decoder that quietly returns fewer relocations than the file declares is the exact failure
//! this crate exists to prevent.

use omni_elf::aps2::{
    self, encode_rela_ungrouped, encode_sleb128, Aps2Limits, PackedFormat, Sleb128Decoder,
    APS2_MAGIC, RELOCATION_GROUPED_BY_ADDEND_FLAG, RELOCATION_GROUPED_BY_INFO_FLAG,
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

/// A ceiling far above anything these hand-built blobs declare, so the limit never masks the
/// behaviour under test. The limit itself is tested separately, and against the real binary.
fn generous() -> Aps2Limits {
    Aps2Limits::new(1_000_000, u64::MAX)
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
    let decoded =
        aps2::decode_rela(&blob, generous()).expect("the encoder must produce a decodable blob");
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
    let decoded = aps2::decode_rela(&blob, generous()).unwrap();
    assert!(decoded.relocations.is_empty());
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(decoded.summary.group_count, 0);
}

#[test]
fn bad_magic_is_rejected() {
    let mut blob = encode_rela_ungrouped(&sample());
    blob[3] = b'1'; // "APS1"
    assert_eq!(
        aps2::decode_rela(&blob, generous()).unwrap_err(),
        ElfError::Aps2BadMagic(*b"APS1")
    );
    // And a blob shorter than the magic.
    assert_eq!(
        aps2::decode_rela(b"AP", generous()).unwrap_err(),
        ElfError::Aps2TooShort(2)
    );
    assert_eq!(aps2::decode_rela(&[], generous()).unwrap_err(), ElfError::Aps2TooShort(0));
}

#[test]
fn truncation_at_every_length_is_an_error_and_never_a_panic() {
    let blob = encode_rela_ungrouped(&sample());
    // Every proper prefix must fail. Chopping one byte at a time walks the truncation point
    // through the count, the initial offset, the group header and every per-relocation field.
    for cut in 0..blob.len() {
        let err = aps2::decode_rela(&blob[..cut], generous())
            .unwrap_err_or_else(|| panic!("prefix of length {cut} must not decode"));
        assert!(
            matches!(
                err,
                ElfError::Aps2Truncated { .. }
                    | ElfError::Aps2TooShort(_)
                    | ElfError::Aps2BadMagic(_)
                    | ElfError::Aps2CountMismatch { .. }
                    | ElfError::Aps2GroupOverrun { .. }
                    // Layer 1 often notices the truncation from the group header alone, before
                    // the SLEB128 reader runs out. That is a better error, not a worse one.
                    | ElfError::Aps2GroupLargerThanStream { .. }
            ),
            "prefix of length {cut} gave an unexpected error: {err}"
        );
    }
    // And the whole thing still decodes, so the loop above was testing something.
    assert!(aps2::decode_rela(&blob, generous()).is_ok());
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
        aps2::decode_rela(&blob, generous()).unwrap_err(),
        ElfError::Aps2TrailingBytes {
            consumed: good,
            total: good + 1,
            remaining: 1,
        }
    );
    // A whole extra group's worth of junk must not be mistaken for more relocations either.
    blob.extend_from_slice(&[0x7f, 0x01, 0x02, 0x03]);
    assert!(matches!(
        aps2::decode_rela(&blob, generous()).unwrap_err(),
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
        aps2::decode_rela(&blob, generous()).unwrap_err(),
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
        aps2::decode_rela(&blob, generous()).unwrap_err(),
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
            aps2::decode_rela(&blob, generous()).unwrap_err(),
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
        aps2::decode_rela(&blob, generous()).unwrap_err(),
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
        aps2::decode_rela(&blob, generous()).unwrap_err(),
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
        aps2::decode_rel(&blob, generous()).unwrap_err(),
        ElfError::Aps2AddendInRelFormat { group_index: 0 }
    );
    assert!(aps2::decode(&blob, PackedFormat::Rela, generous()).is_ok());
}

#[test]
fn grouped_by_addend_applies_one_delta_per_group_and_accumulates_across_groups() {
    // Hand-built, because no library in the target APK exercises this flag, so this test is the
    // only guard on the branch. Two things have to be distinguishable here and a single group
    // starting from zero distinguishes neither:
    //
    //   * per-group vs per-relocation: a group of three with one delta must not triple it;
    //   * `r_addend += delta` (bionic) vs `r_addend = delta`: only visible once a *second*
    //     grouped-addend group runs with a non-zero running total behind it.
    //
    // So: group one establishes a running addend of 0x4242 across three relocations, group two
    // adds 0x100 to it, giving 0x4342 rather than 0x100.
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(5, &mut blob);
    encode_sleb128(0x1000, &mut blob); // initial r_offset
    let flags = (RELOCATION_GROUPED_BY_INFO_FLAG
        | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
        | RELOCATION_GROUPED_BY_ADDEND_FLAG
        | RELOCATION_GROUP_HAS_ADDEND_FLAG) as i64;

    encode_sleb128(3, &mut blob); // group one: three relocations
    encode_sleb128(flags, &mut blob);
    encode_sleb128(8, &mut blob); // shared offset delta
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob); // shared r_info
    encode_sleb128(0x4242, &mut blob); // one shared addend delta

    encode_sleb128(2, &mut blob); // group two: two more
    encode_sleb128(flags, &mut blob);
    encode_sleb128(16, &mut blob);
    encode_sleb128(R_AARCH64_GLOB_DAT as i64, &mut blob);
    encode_sleb128(0x100, &mut blob); // accumulates onto 0x4242

    let decoded = aps2::decode_rela(&blob, generous()).unwrap();
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(
        decoded.relocations,
        vec![
            // Group one: one delta shared by all three, not applied three times.
            rela(0x1008, 0, R_AARCH64_RELATIVE, 0x4242),
            rela(0x1010, 0, R_AARCH64_RELATIVE, 0x4242),
            rela(0x1018, 0, R_AARCH64_RELATIVE, 0x4242),
            // Group two: 0x4242 + 0x100, so the running total carried across the group boundary.
            rela(0x1028, 0, R_AARCH64_GLOB_DAT, 0x4342),
            rela(0x1038, 0, R_AARCH64_GLOB_DAT, 0x4342),
        ],
        "one delta per group, accumulated onto the running addend"
    );
    // Spelled out so the two failure modes are named rather than buried in the vector compare.
    assert_eq!(
        decoded.relocations[2].r_addend, 0x4242,
        "a per-relocation application would have produced 0x{:x}",
        0x4242 * 3
    );
    assert_eq!(
        decoded.relocations[3].r_addend, 0x4342,
        "`r_addend = delta` instead of `+=` would have produced 0x100"
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

    let decoded = aps2::decode_rela(&blob, generous()).unwrap();
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
    let decoded = aps2::decode_rela(&blob, generous()).unwrap();
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
    assert_eq!(aps2::decode_rela(&blob, generous()).unwrap().relocations, relocs);
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
            if let Ok(d) = aps2::decode_rela(&bad, generous()) {
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

/// A valid AArch64 `ET_DYN` header with exactly one `PT_LOAD` covering the whole file, and no
/// `PT_DYNAMIC`. Used to reach gates that sit after `PT_LOAD` validation.
fn one_load_segment_object() -> Vec<u8> {
    const PHOFF: usize = SIZEOF_EHDR;
    let len = PHOFF + SIZEOF_PHDR;
    let mut v = minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, EM_AARCH64);
    v.resize(len, 0);
    v[32..40].copy_from_slice(&(PHOFF as u64).to_le_bytes()); // e_phoff
    v[54..56].copy_from_slice(&(SIZEOF_PHDR as u16).to_le_bytes()); // e_phentsize
    v[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    let p = PHOFF;
    v[p..p + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
    v[p + 4..p + 8].copy_from_slice(&4u32.to_le_bytes()); // PF_R
    v[p + 8..p + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    v[p + 16..p + 24].copy_from_slice(&0u64.to_le_bytes()); // p_vaddr
    v[p + 24..p + 32].copy_from_slice(&0u64.to_le_bytes()); // p_paddr
    v[p + 32..p + 40].copy_from_slice(&(len as u64).to_le_bytes()); // p_filesz
    v[p + 40..p + 48].copy_from_slice(&(len as u64).to_le_bytes()); // p_memsz
    v[p + 48..p + 56].copy_from_slice(&1u64.to_le_bytes()); // p_align
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
    // A valid header with no program headers has no loadable image. PT_LOAD validation runs
    // before the dynamic section is read, deliberately: everything downstream derives bounds from
    // the segments, so they are checked first.
    assert_eq!(
        ElfImage::parse(&minimal_header(ELFCLASS64, ELFDATA2LSB, ET_DYN, EM_AARCH64)).unwrap_err(),
        ElfError::NoLoadSegments
    );
    // With a valid PT_LOAD but no PT_DYNAMIC, the next gate fires instead.
    assert_eq!(
        ElfImage::parse(&one_load_segment_object()).unwrap_err(),
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

// ---------------------------------------------------------------------------------------------
// Hostile input: unbounded counts and amplification
// ---------------------------------------------------------------------------------------------

/// A blob whose single group spends **zero** bytes per relocation, declaring `count` of them.
///
/// This shape is legal — sharing the offset delta, the `r_info` and the addend is exactly what
/// the grouping flags are for — which is why the count cannot be bounded from `blob.len()`.
fn zero_cost_blob(count: i64) -> Vec<u8> {
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(count, &mut blob);
    encode_sleb128(0x1000, &mut blob); // initial r_offset
    encode_sleb128(count, &mut blob); // one group holding all of them
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG
            | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
            | RELOCATION_GROUPED_BY_ADDEND_FLAG
            | RELOCATION_GROUP_HAS_ADDEND_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(8, &mut blob); // shared offset delta
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob); // shared r_info
    encode_sleb128(0, &mut blob); // shared addend delta
    blob
}

#[test]
fn a_tiny_blob_declaring_an_astronomical_count_is_refused_immediately() {
    // Thirty-odd bytes declaring 2^62 relocations. Without a count bound the streaming decoder
    // runs for the rest of the decade and the Vec decoder asks the allocator for 110 exabytes.
    let blob = zero_cost_blob(1 << 62);
    assert!(
        blob.len() < 40,
        "the whole hostile blob is {} bytes",
        blob.len()
    );

    let limits = Aps2Limits::new(60_383_782, 120_798_268); // libroblox.so's own validated image

    // Streaming: rejected before the sink is called even once, and in negligible time.
    let mut produced = 0u64;
    let start = std::time::Instant::now();
    let err = aps2::decode_with(&blob, PackedFormat::Rela, limits, |_| {
        produced += 1;
        Ok(())
    })
    .unwrap_err_or_else(|| panic!("a 2^62 count must be refused"));
    let elapsed = start.elapsed();
    assert_eq!(
        err,
        ElfError::Aps2CountExceedsLimit {
            declared: 1 << 62,
            limit: 60_383_782,
        }
    );
    assert_eq!(produced, 0, "nothing may be produced before the count check");
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "refusal took {elapsed:?}; the check must happen before decoding, not during"
    );

    // And the Vec path, which is the one that used to abort the process on allocation failure.
    assert_eq!(
        aps2::decode_rela(&blob, limits).unwrap_err(),
        ElfError::Aps2CountExceedsLimit {
            declared: 1 << 62,
            limit: 60_383_782,
        }
    );

    // i64::MAX and a count just one past the limit are refused the same way — no off-by-one
    // that lets `limit + 1` through.
    assert!(matches!(
        aps2::decode_rela(&zero_cost_blob(i64::MAX), limits).unwrap_err(),
        ElfError::Aps2CountExceedsLimit { .. }
    ));
    assert!(matches!(
        aps2::decode_rela(&zero_cost_blob(60_383_783), limits).unwrap_err(),
        ElfError::Aps2CountExceedsLimit {
            declared: 60_383_783,
            limit: 60_383_782
        }
    ));
}

#[test]
fn the_limit_does_not_reject_the_legitimate_zero_cost_encoding() {
    // The point of the bound is that it constrains the *count*, never the encoding. A 31-byte
    // blob describing 250,000 real relocations at zero bytes each still decodes, in full, with
    // correct values — so nothing a linker could legally emit is lost.
    let count = 250_000i64;
    let blob = zero_cost_blob(count);
    assert!(blob.len() < 40);
    let decoded = aps2::decode_rela(&blob, Aps2Limits::new(1_000_000, u64::MAX)).unwrap();
    assert_eq!(decoded.relocations.len() as i64, count);
    assert_eq!(decoded.summary.decoded_count as i64, count);
    assert_eq!(decoded.summary.bytes_consumed, blob.len());
    assert_eq!(decoded.summary.bytes_consumed, decoded.summary.bytes_total);
    assert_eq!(
        decoded.relocations[0],
        rela(0x1008, 0, R_AARCH64_RELATIVE, 0)
    );
    assert_eq!(
        decoded.relocations[(count - 1) as usize],
        rela(0x1000 + 8 * count as u64, 0, R_AARCH64_RELATIVE, 0)
    );
    // Exactly at the limit is accepted; the boundary is inclusive.
    assert!(aps2::decode_rela(&blob, Aps2Limits::new(count as u64, u64::MAX)).is_ok());
}

#[test]
fn a_sink_error_stops_the_decode() {
    // The fallible sink is what lets the `Vec` wrapper fail on allocation rather than abort, and
    // what will let the applying loader reject a relocation target without decoding the rest.
    let blob = zero_cost_blob(100_000);
    let mut produced = 0u64;
    let err = aps2::decode_with(&blob, PackedFormat::Rela, Aps2Limits::new(1_000_000, u64::MAX), |_| {
        produced += 1;
        if produced == 17 {
            Err(ElfError::AllocationFailed { bytes: 42 })
        } else {
            Ok(())
        }
    })
    .unwrap_err_or_else(|| panic!("the sink error must propagate"));
    assert_eq!(err, ElfError::AllocationFailed { bytes: 42 });
    assert_eq!(produced, 17, "the decode must stop at the rejected relocation");
}

#[test]
fn relr_amplification_is_counted_before_it_is_allocated() {
    use omni_elf::reloc::parse_relr_table;

    // Each odd word is a bitmap of 63 relocations, so 8 bytes of input become 63 * 24 = 1512
    // bytes of output: 189x. 800 bytes of table therefore describes 6,300 relocations.
    let words = 100usize;
    let mut buf = Vec::new();
    buf.extend_from_slice(&0x1000u64.to_le_bytes()); // one address word
    for _ in 0..words - 1 {
        buf.extend_from_slice(&u64::MAX.to_le_bytes()); // all 63 bits set
    }
    let view = omni_elf::View::new(&buf);
    let expected = 1 + (words - 1) * 63;

    // Under a limit that allows it, the expansion is exact.
    let relocs = parse_relr_table(&view, buf.len() as u64, Some(8), Aps2Limits::new(1 << 20, u64::MAX))
        .expect("a RELR table below the limit must decode");
    assert_eq!(relocs.len(), expected);
    assert!(relocs.iter().all(|r| r.r_type() == R_AARCH64_RELATIVE));

    // Over the limit it is refused by counting, before a single Rela is allocated.
    assert_eq!(
        parse_relr_table(&view, buf.len() as u64, Some(8), Aps2Limits::new(100, u64::MAX)).unwrap_err(),
        ElfError::RelocationCountExceedsLimit {
            what: "DT_RELR",
            count: expected as u64,
            limit: 100,
        }
    );
    // The limit derived from a loadable image large enough to hold them all still accepts.
    assert!(parse_relr_table(
        &view,
        buf.len() as u64,
        Some(8),
        Aps2Limits::new(expected as u64, u64::MAX)
    )
    .is_ok());
}

#[test]
fn a_zero_stride_group_cannot_declare_more_than_one_relocation() {
    // The reviewer's second probe: fifteen bytes, a shared offset delta of zero, a thousand
    // relocations at one address. Every one of those addresses is valid, so no amount of
    // per-relocation offset checking rejects it — but they are bit-identical, so all but the
    // first are dead. This is layer 2, and it needs no external information at all.
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(1000, &mut blob);
    encode_sleb128(0x1000, &mut blob);
    encode_sleb128(1000, &mut blob);
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG
            | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
            | RELOCATION_GROUPED_BY_ADDEND_FLAG
            | RELOCATION_GROUP_HAS_ADDEND_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(0, &mut blob); // the shared stride
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
    encode_sleb128(0, &mut blob);
    assert!(blob.len() <= 16, "the probe blob is {} bytes", blob.len());

    // Refused even with no image bound and a limit that would otherwise permit all 1,000.
    assert_eq!(
        aps2::decode_rela(&blob, generous()).unwrap_err(),
        ElfError::Aps2DeadGroup {
            group_index: 0,
            size: 1000,
        }
    );
    // A single relocation with a zero stride is fine: there is nothing dead about it.
    let mut ok = Vec::from(APS2_MAGIC);
    encode_sleb128(1, &mut ok);
    encode_sleb128(0x1000, &mut ok);
    encode_sleb128(1, &mut ok);
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG) as i64,
        &mut ok,
    );
    encode_sleb128(0, &mut ok);
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut ok);
    let decoded = aps2::decode_rela(&ok, generous()).unwrap();
    assert_eq!(
        decoded.relocations,
        vec![rela(0x1000, 0, R_AARCH64_RELATIVE, 0)]
    );
}

#[test]
fn a_zero_cost_group_must_fit_inside_the_image() {
    // Layer 3: with a non-zero stride the targets march across the image, so
    // (size - 1) * |stride| must fit in it. A 0x4000-byte image at an 8-byte stride holds 2,048.
    let build = |size: i64, stride: i64| {
        let mut blob = Vec::from(APS2_MAGIC);
        encode_sleb128(size, &mut blob);
        encode_sleb128(0, &mut blob);
        encode_sleb128(size, &mut blob);
        encode_sleb128(
            (RELOCATION_GROUPED_BY_INFO_FLAG | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG) as i64,
            &mut blob,
        );
        encode_sleb128(stride, &mut blob);
        encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
        blob
    };
    let limits = Aps2Limits::new(1 << 40, 0x4000);

    assert!(
        aps2::decode_rela(&build(2049, 8), limits).is_ok(),
        "2049 relocations reach exactly 0x4000 bytes, which fits"
    );
    assert_eq!(
        aps2::decode_rela(&build(2050, 8), limits).unwrap_err(),
        ElfError::Aps2GroupExceedsImage {
            group_index: 0,
            size: 2050,
            stride: 8,
            reach: 2049 * 8,
            image_span: 0x4000,
        }
    );
    // A negative stride is bounded by its magnitude, not waved through.
    assert_eq!(
        aps2::decode_rela(&build(2050, -8), limits).unwrap_err(),
        ElfError::Aps2GroupExceedsImage {
            group_index: 0,
            size: 2050,
            stride: 8,
            reach: 2049 * 8,
            image_span: 0x4000,
        }
    );
    // And a stride whose product overflows is an error rather than a wrap into a small reach.
    assert!(matches!(
        aps2::decode_rela(&build(1 << 32, 1 << 32), limits).unwrap_err(),
        ElfError::Aps2GroupExceedsImage { .. }
    ));
}

#[test]
fn a_group_that_pays_bytes_cannot_exceed_the_bytes_left() {
    // Layer 1: the group claims 10,000 relocations each needing at least one addend byte, but
    // the blob ends. Bounded by the blob alone, with no image or count limit involved.
    let mut blob = Vec::from(APS2_MAGIC);
    encode_sleb128(10_000, &mut blob);
    encode_sleb128(0x1000, &mut blob);
    encode_sleb128(10_000, &mut blob);
    encode_sleb128(
        (RELOCATION_GROUPED_BY_INFO_FLAG
            | RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
            | RELOCATION_GROUP_HAS_ADDEND_FLAG) as i64,
        &mut blob,
    );
    encode_sleb128(8, &mut blob);
    encode_sleb128(R_AARCH64_RELATIVE as i64, &mut blob);
    encode_sleb128(0, &mut blob); // one addend byte, 9,999 short
    assert_eq!(
        aps2::decode_rela(&blob, generous()).unwrap_err(),
        ElfError::Aps2GroupLargerThanStream {
            group_index: 0,
            size: 10_000,
            min_bytes_each: 1,
            needed: 10_000,
            remaining: 1,
        }
    );
}

#[test]
fn the_flat_ceiling_applies_even_to_an_enormous_validated_image() {
    // The derived bound comes from validated header fields, but validation only removes absurd
    // values. The flat ceiling depends on no file data at all, so it still holds when the image
    // is as large as the crate will ever accept.
    use omni_elf::{LoadImage, MAX_IMAGE_SPAN};
    assert_eq!(MAX_IMAGE_SPAN, 4 * 1024 * 1024 * 1024);
    let huge = LoadImage {
        base_vaddr: 0,
        end_vaddr: MAX_IMAGE_SPAN,
        span: MAX_IMAGE_SPAN,
        mapped_bytes: MAX_IMAGE_SPAN,
        max_align: 0x1000,
        segment_count: 1,
    };
    let limits = Aps2Limits::for_image(&huge);
    // Unclamped the derived figure would be 2^31; the ceiling holds it to 64 Mi.
    assert_eq!(
        huge.mapped_bytes / Aps2Limits::MIN_RELOCATION_FOOTPRINT,
        2_147_483_648
    );
    assert_eq!(limits.max_relocations, Aps2Limits::MAX_RELOCATIONS);
    assert_eq!(limits.max_relocations, 67_108_864);
    assert_eq!(
        aps2::decode_rela(&zero_cost_blob(67_108_865), limits).unwrap_err(),
        ElfError::Aps2CountExceedsLimit {
            declared: 67_108_865,
            limit: 67_108_864,
        }
    );
}
