//! Golden-data tests across all eleven ARM64 libraries in `Roblox-2.738.1397.apk`.
//!
//! These pin the facts in `docs/DECISIONS.md` D9 and `docs/research/apk-analysis.md` §3.3 that
//! are about the APK as a whole: that ten libraries use plain `DT_RELA` + `DT_JMPREL`, that no
//! library uses `DT_RELR` or `DT_ANDROID_REL`, that the union of undefined symbols is 669, and
//! that where both hash tables exist they agree.

mod common;

use std::collections::BTreeSet;

use omni_elf::consts::*;
use omni_elf::{Aps2Limits, ElfImage, RelocEncoding};

/// One row of D9's per-library relocation table, plus the other measured per-library facts.
///
/// The column D9 labels `ABS32` is asserted here as `R_AARCH64_ABS64` (257): the count is what
/// was measured, the name in the docs is wrong. See the Task 4 report.
struct Expected {
    name: &'static str,
    file_bytes: usize,
    relative: usize,
    abs64: usize,
    glob_dat: usize,
    jump_slot: usize,
    total: usize,
    relro_bytes: u64,
    init_array_entries: u64,
    undefined: usize,
    /// `nchain` from `DT_HASH`, when the library has one. `None` means `DT_GNU_HASH` only.
    sysv_symbol_count: Option<u32>,
    symbol_count: u32,
}

/// Sorted by total relocations, matching the table in `apk-analysis.md` §3.3.
const EXPECTED: &[Expected] = &[
    Expected {
        name: "libroblox.so",
        file_bytes: 109_193_800,
        relative: 568_194,
        abs64: 22,
        glob_dat: 56,
        jump_slot: 534,
        total: 568_806,
        relro_bytes: 5_205_568,
        init_array_entries: 3_594,
        undefined: 565,
        sysv_symbol_count: None,
        symbol_count: 1_109,
    },
    Expected {
        name: "libzstd-jni-1.5.7-6.so",
        file_bytes: 18_440_296,
        relative: 33_919,
        abs64: 5,
        glob_dat: 18,
        jump_slot: 333,
        total: 34_275,
        relro_bytes: 580_752,
        init_array_entries: 705,
        undefined: 338,
        sysv_symbol_count: None,
        symbol_count: 488,
    },
    Expected {
        name: "libbacktrace-native.so",
        file_bytes: 5_339_704,
        relative: 11_325,
        abs64: 5_906,
        glob_dat: 746,
        jump_slot: 4_601,
        total: 22_578,
        relro_bytes: 285_056,
        init_array_entries: 5,
        undefined: 315,
        sysv_symbol_count: Some(11_310),
        symbol_count: 11_310,
    },
    Expected {
        name: "librenderscript-toolkit.so",
        file_bytes: 394_112,
        relative: 1_265,
        abs64: 533,
        glob_dat: 49,
        jump_slot: 218,
        total: 2_065,
        relro_bytes: 20_384,
        init_array_entries: 2,
        undefined: 89,
        sysv_symbol_count: None,
        symbol_count: 820,
    },
    Expected {
        name: "libeigen_blas.so",
        file_bytes: 251_784,
        relative: 1_208,
        abs64: 381,
        glob_dat: 22,
        jump_slot: 77,
        total: 1_688,
        relro_bytes: 16_112,
        init_array_entries: 2,
        undefined: 45,
        sysv_symbol_count: None,
        symbol_count: 346,
    },
    Expected {
        name: "libimage_processing_util_jni.so",
        file_bytes: 32_544,
        relative: 43,
        abs64: 0,
        glob_dat: 0,
        jump_slot: 18,
        total: 61,
        relro_bytes: 3_280,
        init_array_entries: 0,
        undefined: 18,
        sysv_symbol_count: None,
        symbol_count: 27,
    },
    Expected {
        name: "libdatastore_shared_counter.so",
        file_bytes: 7_112,
        relative: 4,
        abs64: 0,
        glob_dat: 0,
        jump_slot: 14,
        total: 18,
        relro_bytes: 3_648,
        init_array_entries: 1,
        undefined: 10,
        sysv_symbol_count: Some(20),
        symbol_count: 20,
    },
    Expected {
        name: "libsurface_util_jni.so",
        file_bytes: 4_896,
        relative: 3,
        abs64: 0,
        glob_dat: 0,
        jump_slot: 9,
        total: 12,
        relro_bytes: 1_616,
        init_array_entries: 0,
        undefined: 9,
        sysv_symbol_count: None,
        symbol_count: 11,
    },
    Expected {
        name: "libtrampoline.so",
        file_bytes: 5_104,
        relative: 4,
        abs64: 0,
        glob_dat: 0,
        jump_slot: 7,
        total: 11,
        relro_bytes: 1_456,
        init_array_entries: 0,
        undefined: 7,
        sysv_symbol_count: Some(8),
        symbol_count: 8,
    },
    Expected {
        name: "libeigen_lapack.so",
        file_bytes: 4_032,
        relative: 3,
        abs64: 1,
        glob_dat: 0,
        jump_slot: 3,
        total: 7,
        relro_bytes: 2_400,
        init_array_entries: 0,
        undefined: 4,
        sysv_symbol_count: None,
        symbol_count: 5,
    },
    Expected {
        name: "libyuv_shared.so",
        file_bytes: 3_752,
        relative: 3,
        abs64: 0,
        glob_dat: 0,
        jump_slot: 3,
        total: 6,
        relro_bytes: 2_592,
        init_array_entries: 0,
        undefined: 3,
        sysv_symbol_count: None,
        symbol_count: 4,
    },
];

/// The union across all eleven libraries, including the injected `libtrampoline.so`. Not in
/// conflict with `libroblox.so`'s own 565 — both figures are correct (D9).
const UNION_UNDEFINED: usize = 669;

#[test]
fn every_library_is_accounted_for() {
    let Some(libs) = common::require_libs() else { return };
    assert_eq!(libs.len(), common::EXPECTED_LIB_COUNT);
    assert_eq!(EXPECTED.len(), common::EXPECTED_LIB_COUNT);
    let found: BTreeSet<&str> = libs.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = EXPECTED.iter().map(|e| e.name).collect();
    assert_eq!(found, expected, "library names in lib/arm64-v8a/");
}

#[test]
fn per_library_relocations_and_layout_match_d9() {
    let Some(libs) = common::require_libs() else { return };
    for e in EXPECTED {
        let bytes = &libs[e.name];
        assert_eq!(bytes.len(), e.file_bytes, "{}: file size", e.name);
        let elf = ElfImage::parse(bytes)
            .unwrap_or_else(|err| panic!("{} must parse: {err}", e.name));
        let relocs = elf
            .relocations()
            .unwrap_or_else(|err| panic!("{}: relocations must decode: {err}", e.name));

        assert_eq!(
            relocs.count_of_type(R_AARCH64_RELATIVE),
            e.relative,
            "{}: R_AARCH64_RELATIVE",
            e.name
        );
        assert_eq!(
            relocs.count_of_type(R_AARCH64_ABS64),
            e.abs64,
            "{}: R_AARCH64_ABS64 (257) — D9 calls this column ABS32",
            e.name
        );
        assert_eq!(
            relocs.count_of_type(R_AARCH64_ABS32),
            0,
            "{}: R_AARCH64_ABS32 (258) occurs nowhere in this APK",
            e.name
        );
        assert_eq!(
            relocs.count_of_type(R_AARCH64_GLOB_DAT),
            e.glob_dat,
            "{}: R_AARCH64_GLOB_DAT",
            e.name
        );
        assert_eq!(
            relocs.count_of_type(R_AARCH64_JUMP_SLOT),
            e.jump_slot,
            "{}: R_AARCH64_JUMP_SLOT",
            e.name
        );
        assert_eq!(relocs.total(), e.total, "{}: total relocations", e.name);
        assert_eq!(
            e.relative + e.abs64 + e.glob_dat + e.jump_slot,
            e.total,
            "{}: the expected per-type counts must add up",
            e.name
        );

        // No ifunc and no COPY relocations anywhere in the APK (D9).
        assert_eq!(relocs.count_of_type(R_AARCH64_IRELATIVE), 0, "{}", e.name);
        assert_eq!(relocs.count_of_type(R_AARCH64_COPY), 0, "{}", e.name);
        for ty in [
            R_AARCH64_TLS_DTPREL64,
            R_AARCH64_TLS_DTPMOD64,
            R_AARCH64_TLS_TPREL64,
            R_AARCH64_TLSDESC,
        ] {
            assert_eq!(relocs.count_of_type(ty), 0, "{}: TLS reloc type {ty}", e.name);
        }

        let relro = elf
            .relro()
            .unwrap_or_else(|| panic!("{}: PT_GNU_RELRO", e.name));
        assert_eq!(relro.p_memsz, e.relro_bytes, "{}: PT_GNU_RELRO", e.name);
        assert_eq!(
            elf.dynamic()
                .init_array
                .map_or(0, |a| a.entry_count()),
            e.init_array_entries,
            "{}: DT_INIT_ARRAY entries",
            e.name
        );
        assert_eq!(
            elf.init_array().unwrap().len() as u64,
            e.init_array_entries,
            "{}: DT_INIT_ARRAY read back",
            e.name
        );
        assert_eq!(
            elf.symbol_count(),
            e.symbol_count,
            "{}: .dynsym entry count",
            e.name
        );
        assert_eq!(
            elf.sysv_hash().unwrap().map(|h| h.symbol_count()),
            e.sysv_symbol_count,
            "{}: DT_HASH nchain (None means the library has no DT_HASH)",
            e.name
        );
        assert_eq!(
            elf.undefined_symbols().unwrap().len(),
            e.undefined,
            "{}: undefined symbols",
            e.name
        );
        assert!(!elf.dynamic().wants_textrel(), "{}: no DT_TEXTREL", e.name);
    }
}

/// The image-derived relocation ceiling, and the margin over the real count, for every library.
///
/// One library does not establish a range, and the margin assertion is what makes drift visible,
/// so this covers all eleven. The worst case in the whole APK is `libeigen_blas.so` at 74x.
const MARGINS: &[(&str, u64, u64, u64)] = &[
    // (library, mapped_bytes, image-derived cap, minimum headroom over its real count)
    ("libroblox.so", 120_767_564, 60_383_782, 106),
    ("libzstd-jni-1.5.7-6.so", 18_499_192, 9_249_596, 269),
    ("libbacktrace-native.so", 5_359_936, 2_679_968, 118),
    ("librenderscript-toolkit.so", 397_792, 198_896, 96),
    ("libeigen_blas.so", 252_800, 126_400, 74),
    ("libimage_processing_util_jni.so", 32_772, 16_386, 268),
    ("libdatastore_shared_counter.so", 5_161, 2_580, 143),
    ("libsurface_util_jni.so", 4_096, 2_048, 170),
    ("libtrampoline.so", 4_128, 2_064, 187),
    ("libeigen_lapack.so", 4_104, 2_052, 293),
    ("libyuv_shared.so", 4_096, 2_048, 341),
];

#[test]
fn every_library_sits_far_below_its_own_relocation_ceiling() {
    let Some(libs) = common::require_libs() else { return };
    assert_eq!(MARGINS.len(), common::EXPECTED_LIB_COUNT);

    let mut worst = (u64::MAX, "");
    for &(name, mapped_bytes, cap, min_headroom) in MARGINS {
        let bytes = &libs[name];
        let elf = ElfImage::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let img = elf.load_image();

        // The image measurement the bound rests on, pinned per library.
        assert_eq!(img.mapped_bytes, mapped_bytes, "{name}: mapped bytes");
        assert!(
            img.mapped_bytes <= img.span,
            "{name}: mapped bytes cannot exceed the span"
        );
        assert!(img.span <= omni_elf::MAX_IMAGE_SPAN, "{name}: span ceiling");
        assert!(img.segment_count >= 1, "{name}: at least one PT_LOAD");

        let limits = elf.aps2_limits();
        assert_eq!(limits.max_relocations, cap, "{name}: derived cap");
        assert!(
            limits.max_relocations < Aps2Limits::MAX_RELOCATIONS,
            "{name}: the derived cap should bind, not the flat ceiling"
        );

        let total = elf.relocations().unwrap().total() as u64;
        let headroom = limits.max_relocations / total;
        assert_eq!(
            headroom, min_headroom,
            "{name}: {total} relocations against a cap of {}",
            limits.max_relocations
        );
        if headroom < worst.0 {
            worst = (headroom, name);
        }
    }
    // Pinned so that a future library eating into the margin is a test failure rather than a
    // silent narrowing of the safety factor.
    assert_eq!(
        worst,
        (74, "libeigen_blas.so"),
        "worst-case headroom across the APK"
    );
}

#[test]
fn only_libroblox_uses_packed_relocations_and_nothing_uses_relr() {
    let Some(libs) = common::require_libs() else { return };
    for (name, bytes) in libs {
        let elf = ElfImage::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let d = elf.dynamic();

        // No library in the APK uses DT_RELR or DT_ANDROID_REL (D9).
        assert!(d.relr.is_none(), "{name}: DT_RELR must be absent");
        assert!(d.android_rel.is_none(), "{name}: DT_ANDROID_REL must be absent");
        assert!(d.rel.is_none(), "{name}: DT_REL must be absent");
        // Every one of them has a PLT relocation table, always in RELA form.
        assert!(d.jmprel.is_some(), "{name}: DT_JMPREL");
        assert_eq!(d.pltrel, Some(DT_RELA as u64), "{name}: DT_PLTREL");

        if name == common::MAIN_LIB {
            assert!(d.android_rela.is_some(), "{name}: DT_ANDROID_RELA");
            assert!(d.rela.is_none(), "{name}: DT_RELA must be absent");
        } else {
            // The other ten use plain DT_RELA. The parser must handle both styles.
            assert!(d.android_rela.is_none(), "{name}: no DT_ANDROID_RELA");
            assert!(d.rela.is_some(), "{name}: DT_RELA");
            assert_eq!(d.rela.unwrap().entsize, Some(SIZEOF_RELA as u64));
            let relocs = elf.relocations().unwrap();
            assert_eq!(relocs.general.len(), 1, "{name}: one general table");
            assert_eq!(relocs.general[0].encoding, RelocEncoding::Rela, "{name}");
            assert!(relocs.general[0].packed.is_none(), "{name}: not packed");
            assert!(
                !relocs.general[0].implicit_addend(),
                "{name}: RELA carries explicit addends"
            );
        }
    }
}

#[test]
fn no_library_has_tls_of_any_kind() {
    let Some(libs) = common::require_libs() else { return };
    for (name, bytes) in libs {
        // `parse` refuses PT_TLS outright, so reaching this point already proves there is none.
        let elf = ElfImage::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            !elf.segments().iter().any(|s| s.p_type == PT_TLS),
            "{name}: PT_TLS"
        );
        elf.reject_tls_symbols()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(elf.dynamic().flags & DF_STATIC_TLS, 0, "{name}: DF_STATIC_TLS");
        // And no STT_GNU_IFUNC symbols either (D9).
        let symtab = elf.symbols().unwrap();
        assert_eq!(
            symtab
                .iter()
                .filter(|s| s.as_ref().unwrap().is_ifunc())
                .count(),
            0,
            "{name}: STT_GNU_IFUNC symbols"
        );
    }
}

#[test]
fn undefined_symbol_union_is_669_and_matches_the_research_appendix() {
    let Some(libs) = common::require_libs() else { return };

    let mut union: BTreeSet<String> = BTreeSet::new();
    for (name, bytes) in libs {
        let elf = ElfImage::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        for s in elf.undefined_symbols().unwrap() {
            union.insert(s.name.to_owned());
        }
    }
    assert_eq!(union.len(), UNION_UNDEFINED, "union of undefined symbols");

    // Cross-check against docs/research/apk-undefined-symbols.txt, which was produced by a
    // separate tool. Two independent parsers agreeing on all 669 names is much stronger than
    // agreeing on the count.
    let appendix = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/research/apk-undefined-symbols.txt");
    let text = std::fs::read_to_string(&appendix)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", appendix.display()));
    let documented: BTreeSet<String> = text
        .lines()
        .filter_map(|line| {
            // `name    BINDING  TYPE      <- lib,lib`
            let (head, _) = line.split_once("<-")?;
            let mut parts = head.split_whitespace();
            let name = parts.next()?;
            let binding = parts.next()?;
            matches!(binding, "GLOBAL" | "WEAK" | "LOCAL").then(|| name.to_owned())
        })
        .collect();
    assert_eq!(
        documented.len(),
        UNION_UNDEFINED,
        "the research appendix should list {UNION_UNDEFINED} symbols"
    );
    let only_parsed: Vec<&String> = union.difference(&documented).collect();
    let only_documented: Vec<&String> = documented.difference(&union).collect();
    assert!(
        only_parsed.is_empty() && only_documented.is_empty(),
        "undefined-symbol sets differ.\nonly in omni-elf: {only_parsed:?}\nonly in the appendix: {only_documented:?}"
    );

    // Two DT_NEEDED libraries import nothing at all but must still exist (D9).
    let roblox = ElfImage::parse(common::main_lib().unwrap()).unwrap();
    let needed = roblox.needed().unwrap();
    for lib in ["libOpenSLES.so", "libOpenMAXAL.so"] {
        assert!(needed.contains(&lib), "{lib} must be DT_NEEDED");
    }
}

#[test]
fn dt_hash_and_dt_gnu_hash_agree_wherever_both_exist() {
    let Some(libs) = common::require_libs() else { return };

    let mut with_both = Vec::new();
    let mut checked = 0usize;
    for (name, bytes) in libs {
        let elf = ElfImage::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let sysv = elf.sysv_hash().unwrap();
        let gnu = elf.gnu_hash().unwrap();
        assert!(gnu.is_some(), "{name}: every library has DT_GNU_HASH");
        let Some(sysv) = sysv else { continue };
        with_both.push(name.clone());

        // nchain is the .dynsym entry count by construction, so it must equal what the GNU hash
        // chain walk derives. This is the cross-check that makes `derive_symbol_count` credible
        // for libroblox.so, which has no DT_HASH to check it against.
        assert_eq!(
            sysv.symbol_count(),
            elf.symbol_count(),
            "{name}: DT_HASH nchain vs derived count"
        );
        let gnu_derived = gnu.unwrap().derive_symbol_count().unwrap();
        assert_eq!(
            gnu_derived,
            sysv.symbol_count(),
            "{name}: GNU-hash-derived count vs DT_HASH nchain"
        );

        // Both tables must resolve every export to the same index.
        for e in elf.exported_symbols().unwrap() {
            let via_gnu = elf.lookup_gnu(e.name).unwrap();
            let via_sysv = elf.lookup_sysv(e.name).unwrap();
            assert_eq!(
                via_gnu,
                Some(e.index),
                "{name}: DT_GNU_HASH lookup of {}",
                e.name
            );
            assert_eq!(
                via_sysv, via_gnu,
                "{name}: DT_HASH and DT_GNU_HASH disagree on {}",
                e.name
            );
            checked += 1;
        }
        // And both must miss on a name that is not there.
        assert_eq!(elf.lookup_gnu("omnidroid_absent_symbol").unwrap(), None);
        assert_eq!(elf.lookup_sysv("omnidroid_absent_symbol").unwrap(), None);
        // DT_HASH can also see undefined symbols, unlike DT_GNU_HASH; that asymmetry is a real
        // property of the formats and is pinned here so nobody "fixes" it.
        for u in elf.undefined_symbols().unwrap() {
            assert_eq!(
                elf.lookup_gnu(u.name).unwrap(),
                None,
                "{name}: DT_GNU_HASH must not index the undefined symbol {}",
                u.name
            );
            assert_eq!(
                elf.lookup_sysv(u.name).unwrap(),
                Some(u.index),
                "{name}: DT_HASH must index the undefined symbol {}",
                u.name
            );
        }
    }

    assert_eq!(
        with_both,
        vec![
            "libbacktrace-native.so".to_owned(),
            "libdatastore_shared_counter.so".to_owned(),
            "libtrampoline.so".to_owned(),
        ],
        "libraries carrying both hash tables"
    );
    assert!(
        checked > 10_000,
        "expected thousands of two-table lookups, did {checked}"
    );
}

#[test]
fn every_library_reports_its_ndk_note() {
    let Some(libs) = common::require_libs() else { return };
    for (name, bytes) in libs {
        let elf = ElfImage::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let ident = elf
            .android_ident()
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .unwrap_or_else(|| panic!("{name}: no .note.android.ident"));
        assert!(
            ident.android_api >= 21,
            "{name}: android_api {}",
            ident.android_api
        );
        assert!(
            ident.ndk_version.starts_with('r'),
            "{name}: ndk_version {:?}",
            ident.ndk_version
        );
        // No AArch64 hardening features are requested anywhere in the APK (D9).
        if let Some(props) = elf.gnu_properties().unwrap() {
            assert!(!props.bti, "{name}: BTI");
            assert!(!props.pac, "{name}: PAC");
            assert!(!props.gcs, "{name}: GCS");
        }
    }
}
