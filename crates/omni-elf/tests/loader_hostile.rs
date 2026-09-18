//! Hostile input for the loader.
//!
//! D6 records that the supplied test APK is adversarially modified, so a tampered library is the
//! **expected** case here and not an exceptional one. This is also the task that writes into mapped
//! memory inside a 109 MB binary, so a reachable panic, abort or stray store is a Critical defect.
//!
//! Two properties are asserted for every case:
//!
//! 1. The load is **refused with a typed error naming the offending value** — never a panic, never a
//!    successful load of something the loader could not honestly map.
//! 2. The failure **leaves nothing behind**: no mapping, no view, no commit charge. A loader that
//!    leaks a 109 MB reservation per rejected file is its own denial of service, and the reservation
//!    is taken *before* most of these checks can run.
//!
//! Most cases use the synthetic 8 KiB library in `common::synth`, because the shapes that matter —
//! a `DT_JMPREL` outside the file, a relocation target that misses every `PT_LOAD`, two `PT_LOAD`s
//! claiming one page — cannot be edited into the real binary without breaking something else first.
//! The real library is used where only it will do.

mod common;

use common::synth;
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfError, ElfImage, LoadError};
use omni_mem::{Backing, GuestSpace, MapExecutability, Protection};

/// Load a tampered synthetic library, and assert that whatever happened left nothing behind.
fn attempt(label: &str, edit: impl FnOnce(&mut Vec<u8>)) -> Result<(), LoadError> {
    let file = synth::SynthFile::tampered(label, edit);
    attempt_file(label, &file)
}

fn attempt_file(label: &str, file: &synth::SynthFile) -> Result<(), LoadError> {
    let space = GuestSpace::new().expect("reserve a guest address space");
    let backing =
        Backing::open(file.path(), MapExecutability::Executable).expect("open the library");
    let outcome = (|| -> Result<(), LoadError> {
        let elf = ElfImage::parse(&file.bytes)?;
        let object = loader::load(
            &space,
            &backing,
            &elf,
            &ProviderRegistry::empty_provider(),
            &LoaderConfig::default(),
        )?;
        object.unload(&space)?;
        Ok(())
    })();

    // Whether it loaded or was refused, the space must be empty again.
    let stats = space.stats();
    assert_eq!(stats.mapped, 0, "{label}: a mapping survived");
    assert_eq!(stats.file_backed, 0, "{label}: a file view survived");
    assert_eq!(stats.committed, 0, "{label}: commit charge survived");
    space.close().expect("close the guest space");
    outcome
}

fn refuse(label: &str, edit: impl FnOnce(&mut Vec<u8>)) -> LoadError {
    match attempt(label, edit) {
        Ok(()) => panic!("{label}: the tampered library loaded successfully"),
        Err(e) => {
            // Every message names the offending value, per Global Constraint 7.
            let text = e.to_string();
            assert!(!text.is_empty(), "{label}: empty error message");
            eprintln!("{label:<44} -> {text}");
            e
        }
    }
}

// -------------------------------------------------------------------------------------------------
// The synthetic library itself has to work, or none of the refusals below mean anything.
// -------------------------------------------------------------------------------------------------

#[test]
fn the_pristine_synthetic_library_loads_and_relocates() {
    let file = synth::SynthFile::pristine();
    let space = GuestSpace::new().expect("space");
    let backing = Backing::open(file.path(), MapExecutability::Executable).expect("open");
    let elf = ElfImage::parse(&file.bytes).expect("parse");
    let object = loader::load(
        &space,
        &backing,
        &elf,
        &ProviderRegistry::empty_provider(),
        &LoaderConfig::default(),
    )
    .expect("the pristine synthetic library must load");

    assert_eq!(object.soname.as_deref(), Some("libsynth.so"));
    assert_eq!(object.span(), 0x3000);
    assert_eq!(object.phdr, object.base + synth::PHOFF);
    assert_eq!(object.phnum, synth::PHNUM as u16);

    let s = &object.stats.relocations;
    assert_eq!(s.total, synth::RELA_COUNT + synth::JMPREL_COUNT);
    assert_eq!(s.applied, s.total);
    assert_eq!(s.plt, synth::JMPREL_COUNT);

    // Two initializers, read from relocated memory, pointing at the two "functions".
    assert_eq!(
        object.init_array,
        vec![
            (object.base + synth::INIT_FN_0 as usize) as u64,
            (object.base + synth::INIT_FN_1 as usize) as u64,
        ]
    );
    // And they really address the marker bytes, which proves the mapping and the relocation agree.
    for (entry, marker) in object.init_array.iter().zip([0xa1u8, 0xa2]) {
        let byte = unsafe { *space.ptr(*entry as usize, 1).expect("in the space") };
        assert_eq!(byte, marker, "initializer {entry:#x} points at the wrong byte");
    }

    // The GOT: two RELATIVE, one GLOB_DAT and one ABS64 to an unsupplied import.
    let read = |vaddr: u64| unsafe {
        space.ptr(object.base + vaddr as usize, 8).expect("in the space").cast::<u64>().read_unaligned()
    };
    assert_eq!(read(synth::GOT), (object.base + synth::LOCAL_DATA as usize) as u64);
    assert_eq!(read(synth::GOT + 8), (object.base + 0x100) as u64);
    assert_eq!(read(synth::GOT + 16), 0, "GLOB_DAT for an unsupplied import is null");
    assert_eq!(read(synth::GOT + 24), 0, "ABS64 for an unsupplied import is null");
    // The JUMP_SLOT to the library's own defined symbol resolves locally, not to null.
    assert_eq!(read(synth::GOT_PLT), 0, "JUMP_SLOT to the import is null");
    assert_eq!(
        read(synth::GOT_PLT + 8),
        (object.base + synth::LOCAL_DATA as usize) as u64,
        "JUMP_SLOT to a defined symbol binds inside the object"
    );

    // One import, unresolved, classified as a function.
    assert_eq!(object.imports.total(), 1);
    assert_eq!(object.imports.unresolved[0].name, "imported_func");
    assert_eq!(object.imports.unresolved[0].kind, omni_elf::SymbolKind::Function);
    // No DT_VERNEED, so the loader claims no provider rather than guessing one.
    assert_eq!(object.imports.unresolved[0].library, None);

    // Relro seals the writable segment's first page, and `.bss` stays writable.
    let relro = object.relro.expect("PT_GNU_RELRO");
    assert_eq!(relro.sealed_bytes(), 0x1000);
    assert_eq!(object.range_at(object.base + synth::DYNAMIC as usize).map(|r| r.rest), Some(Protection::Read));
    assert_eq!(object.range_at(object.base + 0x2000).map(|r| r.rest), Some(Protection::ReadWrite));

    object.unload(&space).expect("unload");
}

// -------------------------------------------------------------------------------------------------
// Segment shapes
// -------------------------------------------------------------------------------------------------

#[test]
fn two_pt_loads_claiming_one_page_are_refused() {
    // Grow the text segment over the data segment's first page. Both remain individually valid,
    // and `LoadImage` accepts overlapping segments by design — it counts their union — so this can
    // only be caught where the mapping is planned.
    let err = refuse("PT_LOAD ranges overlap", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_FILESZ, 0x2000);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_MEMSZ, 0x2000);
    });
    assert!(
        matches!(
            err,
            LoadError::SegmentsOverlap { first: 1, first_end: 0x2000, second: 2, second_start: 0x1000, .. }
        ),
        "expected SegmentsOverlap, got {err}"
    );
}

#[test]
fn a_p_vaddr_that_overflows_when_biased_is_refused() {
    // The eight-byte attack, aimed at the loader instead of the relocation budget: a p_vaddr near
    // the top of the address space. `p_vaddr + p_memsz` overflows, which the parser catches before
    // the loader can bias it.
    let err = refuse("p_vaddr near u64::MAX", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_VADDR, u64::MAX & !0xfff);
    });
    assert!(
        matches!(
            err,
            LoadError::Elf(ElfError::SegmentMemRangeOverflow { .. })
                | LoadError::Elf(ElfError::SegmentAlignMismatch { .. })
        ),
        "expected an overflow or congruence refusal, got {err}"
    );

    // And a p_vaddr that is merely enormous but does not overflow is refused by the span ceiling.
    let err = refuse("p_vaddr 1 TiB", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_VADDR, 1 << 40);
    });
    assert!(
        matches!(err, LoadError::Elf(ElfError::ImageSpanTooLarge { .. })),
        "expected ImageSpanTooLarge, got {err}"
    );
}

#[test]
fn a_writable_executable_pt_load_is_refused_rather_than_silently_downgraded() {
    let err = refuse("PT_LOAD is RWX", |b| {
        synth::put_u32(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_FLAGS, 7);
    });
    assert!(
        matches!(err, LoadError::WritableExecutableSegment { index: 1, flags: 7 }),
        "expected WritableExecutableSegment, got {err}"
    );
}

#[test]
fn p_align_of_zero_or_one_is_accepted_and_the_broken_values_are_refused() {
    // 0 and 1 both mean "no alignment constraint". The gABI congruence requirement says nothing at
    // those values, so they must load rather than being rejected on a technicality.
    for align in [0u64, 1] {
        attempt(&format!("p_align {align}"), |b| {
            synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_ALIGN, align);
            synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_ALIGN, align);
        })
        .unwrap_or_else(|e| panic!("p_align {align} must be accepted, got {e}"));
    }

    // Not a power of two: refused while the PT_LOAD set is validated.
    let err = refuse("p_align 3", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_ALIGN, 3);
    });
    assert!(
        matches!(err, LoadError::Elf(ElfError::SegmentAlignNotPowerOfTwo { align: 3, .. })),
        "expected SegmentAlignNotPowerOfTwo, got {err}"
    );

    // A power of two below the host page size cannot be honoured at all.
    let err = refuse("p_align 0x800", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_ALIGN, 0x800);
    });
    assert!(
        matches!(err, LoadError::AlignBelowPageSize { align: 0x800, .. }),
        "expected AlignBelowPageSize, got {err}"
    );

    // And an absurdly large one, which would inflate both the base alignment and the reserved span.
    let err = refuse("p_align 256 MiB", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_ALIGN, 0x1000_0000);
    });
    assert!(
        matches!(err, LoadError::AlignTooLarge { align: 0x1000_0000, .. }),
        "expected AlignTooLarge, got {err}"
    );
}

#[test]
fn a_segment_whose_offset_and_vaddr_disagree_modulo_the_page_size_is_refused() {
    // With p_align = 1 the gABI congruence check does not apply, so a segment can reach the loader
    // claiming a p_vaddr that no page-aligned file offset can serve.
    let err = refuse("p_vaddr not congruent with p_offset", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_ALIGN, 1);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_VADDR, 0x1800);
    });
    assert!(
        matches!(err, LoadError::SegmentOffsetNotCongruent { index: 2, .. }),
        "expected SegmentOffsetNotCongruent, got {err}"
    );

    // And one whose p_offset is below the bias from its page boundary, so the file would have to
    // start before its own beginning.
    let err = refuse("p_offset below the page bias", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_ALIGN, 1);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_VADDR, 0x1800);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_DATA) + synth::P_OFFSET, 0x400);
    });
    assert!(
        matches!(err, LoadError::SegmentOffsetBelowPageBias { index: 2, offset: 0x400, .. }),
        "expected SegmentOffsetBelowPageBias, got {err}"
    );
}

#[test]
fn a_truncated_file_is_refused() {
    let mut bytes = synth::build();
    bytes.truncate(0x1800);
    let file = synth::SynthFile::new("truncated", bytes);
    let err = attempt_file("truncated file", &file).expect_err("a truncated library must be refused");
    assert!(
        matches!(err, LoadError::Elf(ElfError::SegmentOutsideFile { .. })),
        "expected SegmentOutsideFile, got {err}"
    );
}

#[test]
fn program_headers_outside_every_pt_load_are_refused() {
    // `dl_iterate_phdr` has to report a real address for them: the in-guest C++ unwinder walks
    // 11.5 MB of `.eh_frame` through that call, and a wrong answer looks like a compiler bug.
    let err = refuse("phdrs outside every PT_LOAD", |b| {
        synth::put_u32(b, synth::phdr(synth::PH_PHDR) + synth::P_TYPE, 0); // PT_NULL
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_OFFSET, 0x100);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_VADDR, 0x100);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_FILESZ, 0xf00);
        synth::put_u64(b, synth::phdr(synth::PH_LOAD_TEXT) + synth::P_MEMSZ, 0xf00);
    });
    assert!(
        matches!(err, LoadError::ProgramHeadersNotMapped { phoff: 0x40, .. }),
        "expected ProgramHeadersNotMapped, got {err}"
    );
}

// -------------------------------------------------------------------------------------------------
// Relocations
// -------------------------------------------------------------------------------------------------

#[test]
fn a_relocation_target_outside_every_pt_load_is_refused() {
    let err = refuse("relocation target past the image", |b| {
        synth::put_u64(b, synth::rela(synth::RELA_GOT_0), 0x4000);
    });
    assert!(
        matches!(err, LoadError::RelocationTargetUnmapped { r_offset: 0x4000, .. }),
        "expected RelocationTargetUnmapped, got {err}"
    );

    // Below the image as well as above it.
    let err = refuse("relocation target below the image", |b| {
        synth::put_u64(b, synth::rela(synth::RELA_GOT_0), u64::MAX - 7);
    });
    assert!(
        matches!(err, LoadError::RelocationTargetUnmapped { .. } | LoadError::AddressOverflow { .. }),
        "expected a refusal naming the target, got {err}"
    );
}

#[test]
fn a_relocation_straddling_the_end_of_its_mapped_range_is_refused() {
    // Four bytes before the end of `.bss`: the eight-byte store would run off the end of the image.
    // A loader that clipped the window instead of refusing would write four bytes into nothing.
    let err = refuse("relocation straddles the image end", |b| {
        synth::put_u64(b, synth::rela(synth::RELA_GOT_0), 0x2ffc);
    });
    assert!(
        matches!(err, LoadError::RelocationTargetSpansRanges { target: _, end: _, .. }),
        "expected RelocationTargetSpansRanges, got {err}"
    );
}

#[test]
fn a_symbol_index_past_the_end_of_dynsym_is_refused() {
    let err = refuse("r_sym past .dynsym", |b| {
        let at = synth::rela(synth::RELA_GLOB_DAT) + 8;
        synth::put_u64(b, at, (9999u64 << 32) | 1025);
    });
    assert!(
        matches!(err, LoadError::Elf(ElfError::SymbolIndexOutOfBounds { index: 9999, count: 3 })),
        "expected SymbolIndexOutOfBounds, got {err}"
    );

    // And the largest possible index, which must not be allowed to wrap into a valid one.
    let err = refuse("r_sym u32::MAX", |b| {
        let at = synth::rela(synth::RELA_GLOB_DAT) + 8;
        synth::put_u64(b, at, (u64::from(u32::MAX) << 32) | 1025);
    });
    assert!(
        matches!(err, LoadError::Elf(ElfError::SymbolIndexOutOfBounds { index: u32::MAX, .. })),
        "expected SymbolIndexOutOfBounds, got {err}"
    );
}

#[test]
fn a_symbolic_relocation_with_no_symbol_is_refused() {
    // r_sym 0 is the reserved null entry. Writing the addend alone would produce a pointer into the
    // low addresses that looks plausible and is not.
    for (label, index) in
        [("GLOB_DAT", synth::RELA_GLOB_DAT), ("ABS64", synth::RELA_ABS64)]
    {
        let err = refuse(&format!("{label} with r_sym 0"), |b| {
            let at = synth::rela(index) + 8;
            let info = synth::get_u64(b, at) & 0xffff_ffff;
            synth::put_u64(b, at, info);
        });
        assert!(
            matches!(err, LoadError::RelocationWithoutSymbol { .. }),
            "expected RelocationWithoutSymbol, got {err}"
        );
    }
}

#[test]
fn relocation_types_this_loader_does_not_implement_are_refused_not_skipped() {
    // A skipped relocation leaves a pointer unrelocated and the crash happens somewhere else, so
    // each of these is refused by name and reason.
    for (ty, what) in [
        (258u32, "R_AARCH64_ABS32"),
        (1024, "R_AARCH64_COPY"),
        (1031, "R_AARCH64_TLSDESC"),
        (1032, "R_AARCH64_IRELATIVE"),
        (0xdead, "an unknown type"),
    ] {
        let err = refuse(&format!("relocation type {ty} ({what})"), |b| {
            let at = synth::rela(synth::RELA_GOT_0) + 8;
            synth::put_u64(b, at, u64::from(ty));
        });
        assert!(
            matches!(err, LoadError::UnsupportedRelocation { ty: got, .. } if got == ty),
            "expected UnsupportedRelocation for {ty}, got {err}"
        );
    }

    // R_AARCH64_NONE is padding and is skipped, not refused — an object may legitimately contain it.
    attempt("R_AARCH64_NONE", |b| {
        let at = synth::rela(synth::RELA_GOT_0) + 8;
        synth::put_u64(b, at, 0);
    })
    .expect("R_AARCH64_NONE must be accepted as padding");
}

#[test]
fn a_dt_jmprel_outside_the_file_is_refused_after_the_image_is_already_mapped() {
    // This one matters twice: the table is read *after* the whole span has been reserved and every
    // segment mapped, so it is the case that proves the cleanup path releases everything.
    let err = refuse("DT_JMPREL outside the file", |b| {
        let at = synth::dyn_value(b, 23);
        synth::put_u64(b, at, 0x10_0000);
    });
    assert!(
        matches!(
            err,
            LoadError::Elf(ElfError::UnmappedVaddrRange { .. })
                | LoadError::Elf(ElfError::UnmappedVaddr(_))
        ),
        "expected an unmapped-vaddr refusal, got {err}"
    );

    // And a DT_JMPREL whose declared size runs off the end of its segment.
    let err = refuse("DT_PLTRELSZ past the segment", |b| {
        let at = synth::dyn_value(b, 2);
        synth::put_u64(b, at, 0x10_0000);
    });
    assert!(
        matches!(err, LoadError::Elf(_)),
        "expected a typed parse refusal, got {err}"
    );
}

// -------------------------------------------------------------------------------------------------
// RELRO and the initializer arrays
// -------------------------------------------------------------------------------------------------

#[test]
fn a_pt_gnu_relro_that_does_not_lie_inside_the_image_is_refused() {
    // The real library's relro is exactly 5,205,568 bytes inside one PT_LOAD (D9); the assertion on
    // that number lives in the M1 suite. What the loader must refuse is a relro that does not
    // describe a region it can seal, because sealing the wrong pages makes `.data` read-only for
    // the life of the process and sealing nothing silently loses the protection.
    let err = refuse("relro larger than its segment", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_RELRO) + synth::P_MEMSZ, 0x10_0000);
    });
    assert!(
        matches!(err, LoadError::RelroOutsideImage { memsz: 0x10_0000, .. }),
        "expected RelroOutsideImage, got {err}"
    );

    let err = refuse("relro outside the image", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_RELRO) + synth::P_VADDR, 0x9000);
    });
    assert!(
        matches!(err, LoadError::RelroOutsideImage { vaddr: 0x9000, .. }),
        "expected RelroOutsideImage, got {err}"
    );

    // A relro of zero bytes seals nothing, which is legal and must not be turned into an error.
    attempt("relro of zero bytes", |b| {
        synth::put_u64(b, synth::phdr(synth::PH_RELRO) + synth::P_MEMSZ, 0);
        synth::put_u64(b, synth::phdr(synth::PH_RELRO) + synth::P_FILESZ, 0);
    })
    .expect("an empty relro segment is legal");
}

#[test]
fn an_init_array_entry_outside_the_image_is_refused() {
    // The next milestone calls these, so an entry pointing outside the loaded image is an
    // attacker-chosen jump target. Note the entry is produced by a *relocation*, so this is a
    // forged addend rather than a forged pointer.
    let err = refuse("init_array entry outside the image", |b| {
        synth::put_u64(b, synth::rela(synth::RELA_INIT_0) + 16, 0x9000);
    });
    assert!(
        matches!(
            err,
            LoadError::InitArrayEntryOutsideImage { what: "DT_INIT_ARRAY", index: 0, value: _, .. }
        ),
        "expected InitArrayEntryOutsideImage, got {err}"
    );

    // A null entry is equally outside the image, and is the shape a loader that read the array from
    // the *file* would produce for every single one of the real library's 3,594 slots.
    let err = refuse("init_array entry left null", |b| {
        // Turn the relocation that fills the slot into padding, so the slot keeps its file value.
        synth::put_u64(b, synth::rela(synth::RELA_INIT_0) + 8, 0);
    });
    assert!(
        matches!(err, LoadError::InitArrayEntryOutsideImage { value: 0, .. }),
        "expected InitArrayEntryOutsideImage for a null entry, got {err}"
    );
}

#[test]
fn a_dt_init_array_whose_size_is_not_a_multiple_of_eight_is_refused() {
    let err = refuse("DT_INIT_ARRAYSZ not a multiple of 8", |b| {
        let at = synth::dyn_value(b, 27);
        synth::put_u64(b, at, 12);
    });
    assert!(
        matches!(err, LoadError::Elf(ElfError::UnalignedTableSize { size: 12, .. })),
        "expected UnalignedTableSize, got {err}"
    );

    let err = refuse("DT_INIT_ARRAY outside the image", |b| {
        let at = synth::dyn_value(b, 25);
        synth::put_u64(b, at, 0x10_0000);
    });
    assert!(matches!(err, LoadError::Elf(_)), "expected a typed refusal, got {err}");
}

// -------------------------------------------------------------------------------------------------
// The sweep: no single-byte corruption anywhere in the metadata may panic or leak
// -------------------------------------------------------------------------------------------------

#[test]
fn no_single_byte_corruption_of_the_metadata_panics_leaks_or_hangs() {
    // Every byte of the ELF header, the program headers, the symbol and string tables, the hash
    // table, both relocation tables and the whole dynamic array, flipped one at a time. 1,600-odd
    // loads of an 8 KiB library. Each one must either load or be refused with a typed error, and
    // must leave the guest address space empty either way.
    //
    // This is the check that the four previous tasks each turned out to need: passing tests on
    // well-formed input told them nothing about the hostile case.
    let regions: &[(usize, usize)] = &[
        (0, 0x40),                                          // ELF header
        (synth::PHOFF, synth::PHNUM * synth::PHENTSIZE),    // program headers
        (synth::SYMTAB as usize, 24 * synth::SYMCOUNT as usize),
        (synth::STRTAB as usize, 0x30),
        (synth::HASH as usize, 24),
        (synth::RELA as usize, 24 * synth::RELA_COUNT),
        (synth::JMPREL as usize, 24 * synth::JMPREL_COUNT),
        (synth::DYNAMIC as usize, 0xf0),
    ];

    let started = std::time::Instant::now();
    let space = GuestSpace::new().expect("space");
    let mut loaded = 0usize;
    let mut refused = 0usize;
    let mut total = 0usize;
    for &(from, len) in regions {
        for offset in from..from + len {
            let mut bytes = synth::build();
            bytes[offset] ^= 0xff;
            let file = synth::SynthFile::new("sweep", bytes);
            let backing = Backing::open(file.path(), MapExecutability::Executable).expect("open");
            match ElfImage::parse(&file.bytes).map_err(LoadError::from).and_then(|elf| {
                loader::load(
                    &space,
                    &backing,
                    &elf,
                    &ProviderRegistry::empty_provider(),
                    &LoaderConfig::default(),
                )
            }) {
                Ok(object) => {
                    object.unload(&space).expect("unload");
                    loaded += 1;
                }
                Err(_) => refused += 1,
            }
            // The invariant that matters: nothing survives, whichever way it went.
            let stats = space.stats();
            assert_eq!(stats.mapped, 0, "offset {offset:#x}: a mapping survived");
            assert_eq!(stats.committed, 0, "offset {offset:#x}: commit charge survived");
            assert_eq!(stats.file_backed, 0, "offset {offset:#x}: a view survived");
            total += 1;
        }
    }
    space.close().expect("close");
    let elapsed = started.elapsed();
    eprintln!(
        "single-byte corruption sweep: {total} mutations, {loaded} loaded, {refused} refused, \
         0 panics, in {elapsed:.1?}"
    );
    assert_eq!(loaded + refused, total);
    assert!(refused > total / 10, "only {refused} of {total} mutations were refused at all");
    assert!(
        elapsed < std::time::Duration::from_secs(120),
        "the sweep took {elapsed:?}, which suggests a mutation made the loader do unbounded work"
    );
}

// -------------------------------------------------------------------------------------------------
// The real library, tampered
// -------------------------------------------------------------------------------------------------

/// Tamper the real `libroblox.so`, write it to a temporary file, and try to load it.
///
/// Expensive — 109 MB copied and written per case — so only the cases that need the real binary's
/// `APS2` blob and 16 KiB alignment live here.
fn attempt_real(label: &str, edit: impl FnOnce(&mut Vec<u8>)) -> Option<Result<(), LoadError>> {
    let bytes = common::cached_main_lib_bytes()?;
    let mut mutated = bytes.to_vec();
    edit(&mut mutated);
    // One directory per call, not per process: `cargo test` runs this binary's tests as parallel
    // threads, and two of them call this function. Sharing a path meant one test deleting the file
    // another had just written — observed as `Backing::open` failing, and, worse, reachable as one
    // test mapping the *other's* tampered library while parsing its own bytes. That is precisely the
    // parsed-bytes-versus-mapped-bytes confusion `LoadError::BackingLengthMismatch` now refuses.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("omni-elf-tamper-{}-{serial}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the directory");
    let path = dir.join("libroblox.so");
    std::fs::write(&path, &mutated).expect("write the tampered library");

    let space = GuestSpace::new().expect("space");
    let backing = Backing::open(&path, MapExecutability::Executable).expect("open");
    let outcome = (|| -> Result<(), LoadError> {
        let elf = ElfImage::parse(&mutated)?;
        let object = loader::load(
            &space,
            &backing,
            &elf,
            &ProviderRegistry::empty_provider(),
            &LoaderConfig::default(),
        )?;
        object.unload(&space)?;
        Ok(())
    })();
    let stats = space.stats();
    assert_eq!(stats.mapped, 0, "{label}: a mapping survived");
    assert_eq!(stats.committed, 0, "{label}: commit charge survived");
    space.close().expect("close");
    drop(backing);
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = &outcome {
        eprintln!("{label:<44} -> {e}");
    }
    Some(outcome)
}

/// File offset of one program header of the real library.
fn real_phdr(bytes: &[u8], index: usize) -> usize {
    let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    phoff + index * 56
}

#[test]
fn tampering_the_real_librarys_relro_size_is_refused() {
    let Some(bytes) = common::cached_main_lib_bytes() else { return };
    // PT_GNU_RELRO is program header 5 of libroblox.so.
    let at = real_phdr(bytes, 5);
    assert_eq!(
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
        0x6474_e552,
        "program header 5 is PT_GNU_RELRO"
    );
    let outcome = attempt_real("real relro grown past its segment", |b| {
        synth::put_u64(b, at + 40, 0x0800_0000);
    })
    .expect("the APK is present");
    let err = outcome.expect_err("a relro larger than its segment must be refused");
    assert!(
        matches!(err, LoadError::RelroOutsideImage { memsz: 0x0800_0000, .. }),
        "expected RelroOutsideImage, got {err}"
    );
}

#[test]
fn tampering_the_real_librarys_packed_relocation_targets_is_refused() {
    let Some(bytes) = common::cached_main_lib_bytes() else { return };
    // DT_ANDROID_RELA is at vaddr 0x15340 and is SLEB128-encoded, so a single byte flip in the
    // first group's offset delta moves hundreds of thousands of targets at once — the shape that
    // would have a loader writing outside the image half a million times over.
    let elf = ElfImage::parse(bytes).expect("parse");
    let blob = elf.dynamic().android_rela.expect("DT_ANDROID_RELA");
    let at = elf.vaddr_to_offset(blob.vaddr).expect("the blob is in the file image") + 8;
    let outcome = attempt_real("real APS2 blob byte flipped", |b| {
        b[at] ^= 0x40;
    })
    .expect("the APK is present");
    match outcome {
        // Either the decoder refuses the blob, or the loader refuses a target it cannot map. What
        // must not happen is a store outside the image, which would have killed the process.
        Err(e) => assert!(
            matches!(
                e,
                LoadError::RelocationTargetUnmapped { .. }
                    | LoadError::RelocationTargetSpansRanges { .. }
                    | LoadError::UnsupportedRelocation { .. }
                    | LoadError::RelocationWithoutSymbol { .. }
                    | LoadError::InitArrayEntryOutsideImage { .. }
                    | LoadError::Elf(_)
            ),
            "expected a typed refusal, got {e}"
        ),
        Ok(()) => eprintln!(
            "real APS2 blob byte flipped             -> still loadable: the flip stayed inside the image"
        ),
    }
}

#[test]
fn every_library_in_the_apk_loads() {
    // The other ten use plain `DT_RELA` plus `DT_JMPREL` rather than `APS2`, and several are small
    // enough that their last file page is not fully present in the file — the case that has to be
    // copied into private memory rather than mapped, because a view cannot run past the end of the
    // section.
    let Some(_) = common::cached_main_lib() else { return };
    let mut total_relocations = 0usize;
    let mut total_imports = 0usize;
    let mut tail_copies = 0usize;
    eprintln!("\n{:<40} {:>10} {:>8} {:>8} {:>7}", "library", "relocs", "imports", "init", "span");
    for name in common::libraries().expect("the APK is present").keys() {
        let path = common::cached_library(name).expect("in the cache");
        let bytes = std::fs::read(&path).expect("read the cache entry");
        let space = GuestSpace::new().expect("space");
        let backing = Backing::open(&path, MapExecutability::Executable).expect("open");
        let elf = ElfImage::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let object = loader::load(
            &space,
            &backing,
            &elf,
            &ProviderRegistry::empty_provider(),
            &LoaderConfig::default(),
        )
        .unwrap_or_else(|e| panic!("{name} must load: {e}"));
        eprintln!(
            "{name:<40} {:>10} {:>8} {:>8} {:>7}",
            object.stats.relocations.applied,
            object.imports.total(),
            object.init_array.len(),
            object.span()
        );
        assert_eq!(object.stats.relocations.applied + object.stats.relocations.none, object.stats.relocations.total);
        total_relocations += object.stats.relocations.applied;
        total_imports += object.imports.total();
        if object.stats.anonymous_bytes > 0 && object.span() < 0x10000 {
            tail_copies += 1;
        }
        object.unload(&space).expect("unload");
        let stats = space.stats();
        assert_eq!(stats.mapped, 0, "{name}: a mapping survived");
        assert_eq!(stats.committed, 0, "{name}: commit charge survived");
        space.close().expect("close");
    }
    eprintln!(
        "11 libraries loaded: {total_relocations} relocations applied, {total_imports} imports \
         accounted for, {tail_copies} needed a private final page"
    );
    assert!(total_relocations > 568_806, "the main library alone has 568,806");
}

// -------------------------------------------------------------------------------------------------
// `p_memsz`: the field that decides how much commit charge a load spends
// -------------------------------------------------------------------------------------------------

/// A `p_memsz` inflated to a gigabyte is refused by the loader, before anything is reserved.
///
/// The bytes past `p_filesz` become private anonymous memory, and `LoaderConfig::bss_commit`
/// defaults to `CommitPolicy::Eager` (D14), which charges a mapping its full size the moment it is
/// made. So this one file field decides the load's commit charge, and it had no bound at all: the
/// whole-branch review measured an **eight-byte** edit to the real `libroblox.so` producing
/// +1026.004 MiB at 1 GiB and +3406.664 MiB at 3.3 GiB, **with the load succeeding**, against 16.7
/// MiB for a stock load.
#[test]
fn a_gigabyte_of_bss_is_refused_before_anything_is_reserved() {
    let at = synth::PHOFF + synth::PH_LOAD_DATA * synth::PHENTSIZE + synth::P_MEMSZ;
    for (label, memsz) in [
        ("p_memsz 1 GiB", 0x4000_0000u64),
        ("p_memsz 3.3 GiB", 0xd400_0000),
        ("p_memsz 257 MiB, just past the limit", 257 * 1024 * 1024 + synth::DATA_VADDR),
    ] {
        let err = refuse(label, |b| synth::put_u64(b, at, memsz));
        match err {
            LoadError::AnonymousMemoryTooLarge { requested, limit } => {
                assert!(
                    requested as u64 >= memsz - synth::DATA_VADDR - synth::DATA_FILESZ,
                    "{label}: the error must name the real size, got {requested}"
                );
                assert_eq!(limit, omni_elf::DEFAULT_MAX_ANONYMOUS_BYTES, "{label}");
            }
            other => panic!("{label}: expected AnonymousMemoryTooLarge, got {other}"),
        }
    }

    // And the limit does not reject what it is supposed to allow: 16 MiB of `.bss` is more than the
    // real `libroblox.so` has and loads without complaint.
    attempt("p_memsz 16 MiB of .bss", |b| {
        synth::put_u64(b, at, synth::DATA_VADDR + 16 * 1024 * 1024);
    })
    .expect("16 MiB of .bss is a legitimate library");
}

/// With the loader's own limit lifted, `omni-mem`'s commit ceiling refuses the same file.
///
/// The two are deliberately separate layers and this asserts the lower one on its own, because the
/// lesson the branch keeps relearning is that a bound is only as trustworthy as its least-validated
/// input: a loader-level cap protects loads, and the ceiling in `omni-mem` protects *commit*, which
/// is what M2's per-thread code caches (D5: 20-35 MiB each) will spend without going near a loader.
#[test]
fn the_guest_spaces_commit_ceiling_refuses_a_gigabyte_of_bss_on_its_own() {
    let at = synth::PHOFF + synth::PH_LOAD_DATA * synth::PHENTSIZE + synth::P_MEMSZ;
    let file = synth::SynthFile::tampered("p_memsz 1 GiB, loader limit lifted", |b| {
        synth::put_u64(b, at, 0x4000_0000);
    });
    let space = GuestSpace::new().expect("space");
    let backing = Backing::open(file.path(), MapExecutability::Executable).expect("open");
    let elf = ElfImage::parse(&file.bytes).expect("parse");
    let config = LoaderConfig {
        // Exactly the mistake the layering exists to survive: a caller that does not set, or wrongly
        // raises, its own limit.
        max_anonymous_bytes: usize::MAX,
        ..LoaderConfig::default()
    };
    let err = loader::load(&space, &backing, &elf, &ProviderRegistry::empty_provider(), &config)
        .expect_err("omni-mem must refuse to commit a gigabyte");
    match &err {
        LoadError::Memory(inner) => {
            let text = inner.to_string();
            assert!(
                text.contains("per-request ceiling") || text.contains("commit charge"),
                "the refusal must be the commit ceiling, got {text}"
            );
            assert!(text.contains("268435456"), "and must name the limit: {text}");
        }
        other => panic!("expected a memory error, got {other}"),
    }
    eprintln!("p_memsz 1 GiB, loader limit lifted           -> {err}");

    // And it left nothing behind: no mapping, and above all no commit charge.
    let stats = space.stats();
    assert_eq!(stats.mapped, 0, "a mapping survived the refusal");
    assert_eq!(stats.committed, 0, "commit charge survived the refusal");
    space.close().expect("close");
}

/// The same edit, eight bytes, in the **real** 109 MB library.
///
/// The synthetic cases above are the ones that assert the refusal precisely; this is the one that
/// asserts it against the binary the measurement was taken from, because the synthetic library
/// shares none of its structure.
#[test]
fn tampering_the_real_librarys_p_memsz_is_refused() {
    let Some(bytes) = common::cached_main_lib_bytes() else { return };
    // Program header 3 is the writable PT_LOAD: the one that owns `.data` and `.bss`.
    let at = real_phdr(bytes, 3);
    assert_eq!(
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
        1,
        "program header 3 is PT_LOAD"
    );
    let filesz = u64::from_le_bytes(bytes[at + 32..at + 40].try_into().unwrap());
    let memsz = u64::from_le_bytes(bytes[at + 40..at + 48].try_into().unwrap());
    assert!(memsz > filesz, "this segment has .bss: filesz {filesz}, memsz {memsz}");

    for (label, grown) in [
        ("real p_memsz -> 1 GiB", 0x4000_0000u64),
        ("real p_memsz -> 3.3 GiB", 0xd400_0000),
    ] {
        let outcome = attempt_real(label, |b| synth::put_u64(b, at + 40, grown))
            .expect("the APK is present");
        let err = outcome.expect_err("an inflated p_memsz must be refused, not loaded");
        assert!(
            matches!(err, LoadError::AnonymousMemoryTooLarge { .. }),
            "{label}: expected AnonymousMemoryTooLarge, got {err}"
        );
    }
}

/// The parsed bytes and the mapped file must be the same file, and the loader — not a test helper —
/// is what says so.
///
/// `load` takes a `Backing` and an `ElfImage` as independent arguments and uses both: the plan comes
/// from `backing.len()`, while a segment's tail bytes are copied out of `elf.data()`. A backing
/// *longer* than the parsed slice therefore maps and relocates file pages the parser never
/// validated; a shorter one substitutes parsed bytes for the mapped file's.
#[test]
fn a_backing_that_is_not_the_parsed_file_is_refused() {
    let pristine = synth::SynthFile::pristine();
    for (label, edit) in [
        (
            "backing one page longer than the parsed image",
            Box::new(|b: &mut Vec<u8>| b.extend_from_slice(&[0u8; 0x1000]))
                as Box<dyn FnOnce(&mut Vec<u8>)>,
        ),
        (
            "backing truncated below the parsed image",
            Box::new(|b: &mut Vec<u8>| b.truncate(0x1000)),
        ),
    ] {
        let other = synth::SynthFile::tampered(label, edit);
        let space = GuestSpace::new().expect("space");
        let backing = Backing::open(other.path(), MapExecutability::Executable).expect("open");
        let elf = ElfImage::parse(&pristine.bytes).expect("the pristine bytes still parse");
        let err = loader::load(
            &space,
            &backing,
            &elf,
            &ProviderRegistry::empty_provider(),
            &LoaderConfig::default(),
        )
        .expect_err("a backing that is not the parsed file must be refused");
        match err {
            LoadError::BackingLengthMismatch { backing: b, parsed, .. } => {
                assert_eq!(b, other.bytes.len() as u64, "{label}");
                assert_eq!(parsed, pristine.bytes.len() as u64, "{label}");
            }
            other => panic!("{label}: expected BackingLengthMismatch, got {other}"),
        }
        assert_eq!(space.stats().mapped, 0, "{label}: nothing may be mapped");
        space.close().expect("close");
    }

    // The matching pair still loads, so the check is not simply refusing everything.
    let space = GuestSpace::new().expect("space");
    let backing = Backing::open(pristine.path(), MapExecutability::Executable).expect("open");
    let elf = ElfImage::parse(&pristine.bytes).expect("parse");
    loader::load(&space, &backing, &elf, &ProviderRegistry::empty_provider(), &LoaderConfig::default())
        .expect("the same file parsed and mapped must load");
    space.close().expect("close");
}
