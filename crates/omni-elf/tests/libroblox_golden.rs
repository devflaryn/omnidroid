//! Golden-data tests against the real `libroblox.so` from `Roblox-2.738.1397.apk`.
//!
//! Every number here was measured from the file and is asserted exactly. A test that would still
//! pass if the decoder were subtly wrong is worse than no test, so alongside the counts these
//! pin the byte-exact consumption, the first and last decoded relocation, and additive
//! checksums over all 568,272 `r_offset`, `r_info` and `r_addend` values — a decoder that got
//! the counts right but the values wrong fails here.
//!
//! Skips (loudly) when the APK is not present; see `tests/common/mod.rs`.

mod common;

use omni_elf::consts::*;
use omni_elf::{ElfImage, RelocEncoding};

/// Every relocation in the `APS2` blob, from `docs/DECISIONS.md` D9.
const APS2_TOTAL: usize = 568_272;
const APS2_RELATIVE: usize = 568_194;
const APS2_GLOB_DAT: usize = 56;
/// D9 and `apk-analysis.md` §3.3 label these 22 relocations `R_AARCH64_ABS32`. The type value
/// in the file is 257, which in AAELF64 is `R_AARCH64_ABS64`; `R_AARCH64_ABS32` is 258 and does
/// not occur. The count is right, the name in the docs is not — see the Task 4 report.
const APS2_ABS64: usize = 22;
const APS2_SYMBOLIC: usize = 78;
const APS2_BLOB_BYTES: usize = 2_100_778;

const PLT_JUMP_SLOT: usize = 534;
const GRAND_TOTAL: usize = 568_806;

const INIT_ARRAY_ENTRIES: usize = 3_594;
const RELRO_BYTES: u64 = 5_205_568;
const FILE_BYTES: usize = 109_193_800;
const UNDEFINED_SYMBOLS: usize = 565;

fn image(data: &[u8]) -> ElfImage<'_> {
    ElfImage::parse(data).expect("libroblox.so must parse")
}

#[test]
fn file_is_the_expected_size() {
    let Some(bytes) = common::main_lib() else { return };
    assert_eq!(bytes.len(), FILE_BYTES, "libroblox.so size");
}

#[test]
fn header_is_elfclass64_littleendian_etdyn_aarch64() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let h = elf.header();
    assert_eq!(h.ident.class, ELFCLASS64);
    assert_eq!(h.ident.data, ELFDATA2LSB);
    assert_eq!(h.ident.version, EV_CURRENT);
    assert_eq!(h.e_type, ET_DYN);
    assert_eq!(h.e_machine, EM_AARCH64);
    assert_eq!(h.e_phentsize as usize, SIZEOF_PHDR);
}

#[test]
fn relocation_style_is_android_packed_only() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let d = elf.dynamic();
    // The three assertions the whole crate exists for.
    assert!(d.android_rela.is_some(), "DT_ANDROID_RELA must be present");
    assert!(d.rela.is_none(), "DT_RELA must be absent");
    assert!(d.relr.is_none(), "DT_RELR must be absent");
    // And the ones that would let a wrong decoder hide.
    assert!(d.rel.is_none(), "DT_REL must be absent");
    assert!(d.android_rel.is_none(), "DT_ANDROID_REL must be absent");
    assert!(d.relacount.is_none(), "DT_RELACOUNT must be absent");
    assert!(d.relcount.is_none(), "DT_RELCOUNT must be absent");
    assert!(!d.wants_textrel(), "no DT_TEXTREL / DF_TEXTREL");

    // The tag really is DT_LOOS + 4 = 0x60000011, not 0x6000000f.
    assert_eq!(DT_ANDROID_RELA, 0x6000_0011);
    assert_eq!(DT_ANDROID_RELASZ, 0x6000_0012);
    assert_eq!(DT_ANDROID_REL, 0x6000_000f);
    assert_eq!(DT_ANDROID_RELSZ, 0x6000_0010);
    assert!(
        d.entries.iter().any(|e| e.tag == DT_ANDROID_RELA),
        "the raw dynamic array must contain tag 0x60000011"
    );
    assert_eq!(
        d.android_rela.unwrap().size as usize,
        APS2_BLOB_BYTES,
        "DT_ANDROID_RELASZ"
    );
}

#[test]
fn aps2_decode_is_byte_exact_and_counts_match_per_type() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let relocs = elf.relocations().expect("relocations must decode");

    let packed = relocs
        .general_with(RelocEncoding::AndroidPackedRela)
        .expect("DT_ANDROID_RELA table");
    let summary = packed.packed.expect("packed tables carry a summary");

    // Byte-exact consumption: the single strongest correctness signal for this format.
    assert_eq!(summary.bytes_total, APS2_BLOB_BYTES);
    assert_eq!(
        summary.bytes_consumed, APS2_BLOB_BYTES,
        "the decoder must consume {APS2_BLOB_BYTES} of {APS2_BLOB_BYTES} bytes"
    );
    assert_eq!(summary.declared_count, APS2_TOTAL as u64);
    assert_eq!(summary.decoded_count, APS2_TOTAL as u64);
    assert_eq!(packed.relocations.len(), APS2_TOTAL);

    // Per-type, not just the total.
    assert_eq!(
        packed.count_of_type(R_AARCH64_RELATIVE),
        APS2_RELATIVE,
        "R_AARCH64_RELATIVE (1027)"
    );
    assert_eq!(
        packed.count_of_type(R_AARCH64_GLOB_DAT),
        APS2_GLOB_DAT,
        "R_AARCH64_GLOB_DAT (1025)"
    );
    assert_eq!(
        packed.count_of_type(R_AARCH64_ABS64),
        APS2_ABS64,
        "R_AARCH64_ABS64 (257) — the docs call these ABS32, which is a naming error"
    );
    assert_eq!(
        packed.count_of_type(R_AARCH64_ABS32),
        0,
        "R_AARCH64_ABS32 (258) does not occur in this blob"
    );
    assert_eq!(packed.symbolic_count(), APS2_SYMBOLIC, "non-zero r_sym");
    // Those three types account for everything: nothing was silently dropped into a fourth.
    assert_eq!(
        APS2_RELATIVE + APS2_GLOB_DAT + APS2_ABS64,
        APS2_TOTAL,
        "the per-type counts must add up to the total"
    );
    // No JUMP_SLOT hides in the packed blob: an earlier draft of the plan claimed they did.
    assert_eq!(
        packed.count_of_type(R_AARCH64_JUMP_SLOT),
        0,
        "the 534 JUMP_SLOTs are in .rela.plt, not in the APS2 blob"
    );
    // No TLS or ifunc relocation types, per D9/D13.
    for ty in [
        R_AARCH64_TLS_DTPREL64,
        R_AARCH64_TLS_DTPMOD64,
        R_AARCH64_TLS_TPREL64,
        R_AARCH64_TLSDESC,
        R_AARCH64_IRELATIVE,
        R_AARCH64_COPY,
    ] {
        assert_eq!(packed.count_of_type(ty), 0, "type {ty} must not occur");
    }

    // Symbolic relocations are exactly the non-RELATIVE ones, which is what makes the
    // 56 + 22 == 78 coincidence in the docs not a coincidence.
    assert!(
        packed
            .relocations
            .iter()
            .all(|r| r.is_symbolic() == (r.r_type() != R_AARCH64_RELATIVE)),
        "every non-RELATIVE relocation is symbolic and every RELATIVE one is not"
    );

    // The blob only exercises three of the sixteen group-flag combinations; recording which
    // means a future binary that uses GROUPED_BY_ADDEND is visible rather than assumed.
    assert_eq!(
        summary.observed_group_flags,
        omni_elf::aps2::RELOCATION_GROUPED_BY_INFO_FLAG
            | omni_elf::aps2::RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG
            | omni_elf::aps2::RELOCATION_GROUP_HAS_ADDEND_FLAG,
        "observed group flags"
    );
    assert_eq!(summary.group_count, 46_184, "group count");
    assert_eq!(summary.initial_offset, 0, "the header seeds r_offset with 0");
}

#[test]
fn aps2_decoded_values_match_golden_data() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let relocs = elf.relocations().unwrap();
    let packed = relocs
        .general_with(RelocEncoding::AndroidPackedRela)
        .unwrap();
    let r = &packed.relocations;

    // First and last entries, in full. Counts alone cannot catch a decoder that mis-applies
    // deltas; these can.
    assert_eq!(r[0].r_offset, 0x062d_c1c0);
    assert_eq!(r[0].r_info, 0x403);
    assert_eq!(r[0].r_type(), R_AARCH64_RELATIVE);
    assert_eq!(r[0].r_addend, 0x062d_c1c0);

    assert_eq!(r[1].r_offset, 0x062d_c1c8);
    assert_eq!(r[1].r_addend, 0x003e_b50d);

    let last = r[r.len() - 1];
    assert_eq!(last.r_offset, 0x0681_7d30);
    assert_eq!(last.r_info, 0x00c1_0000_0101);
    assert_eq!(last.r_type(), R_AARCH64_ABS64);
    assert_eq!(last.r_sym(), 193);
    assert_eq!(last.r_addend, 0);

    // Additive checksums over the whole set. Any permutation-preserving bug survives these, but
    // every delta-accumulation bug — the realistic failure mode — does not.
    let sum_offset = r.iter().fold(0u64, |a, x| a.wrapping_add(x.r_offset));
    let sum_info = r.iter().fold(0u64, |a, x| a.wrapping_add(x.r_info));
    let sum_addend = r.iter().fold(0i128, |a, x| a + x.r_addend as i128);
    assert_eq!(sum_offset, 60_456_134_155_296, "sum of r_offset");
    assert_eq!(sum_info, 88_343_765_909_716, "sum of r_info");
    assert_eq!(sum_addend, 32_780_388_580_533, "sum of r_addend");

    assert_eq!(r.iter().map(|x| x.r_offset).min(), Some(0x062d_c1c0));
    assert_eq!(r.iter().map(|x| x.r_offset).max(), Some(0x0682_9e48));
    assert_eq!(r.iter().map(|x| x.r_addend).min(), Some(0));
    assert_eq!(r.iter().map(|x| x.r_addend).max(), Some(120_742_144));

    // Signed SLEB128 is load-bearing on this real input, not just in theory: the offset stream
    // steps backwards twice, and 219,252 of the addend deltas are negative. A decoder using
    // unsigned LEB128 would produce garbage for all of them.
    let decreasing = r.windows(2).filter(|w| w[1].r_offset < w[0].r_offset).count();
    assert_eq!(decreasing, 2, "backwards r_offset steps in the real stream");

    // Semantic sanity: every target lies inside a writable PT_LOAD segment, so the loader will
    // be able to write it. A decoder with a broken offset accumulator fails this even if its
    // counts are right.
    let writable: Vec<(u64, u64)> = elf
        .load_segments()
        .filter(|s| s.p_flags.contains(omni_elf::SegmentFlags::WRITE))
        .map(|s| (s.p_vaddr, s.vaddr_end()))
        .collect();
    assert_eq!(writable.len(), 2, "two writable PT_LOAD segments");
    let outside = r
        .iter()
        .filter(|x| !writable.iter().any(|&(a, b)| x.r_offset >= a && x.r_offset < b))
        .count();
    assert_eq!(outside, 0, "every r_offset must land in a writable PT_LOAD");

    // And most of them land in PT_GNU_RELRO, which is why RELRO can only be sealed after
    // relocation — a fact Task 5 depends on.
    let relro = elf.relro().expect("PT_GNU_RELRO");
    let in_relro = r
        .iter()
        .filter(|x| x.r_offset >= relro.p_vaddr && x.r_offset < relro.vaddr_end())
        .count();
    assert_eq!(in_relro, 560_410, "relocations inside PT_GNU_RELRO");
}

#[test]
fn jump_slots_come_from_rela_plt_and_are_not_in_the_packed_blob() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let relocs = elf.relocations().unwrap();

    let plt = relocs.plt.as_ref().expect("DT_JMPREL table");
    assert_eq!(plt.tag, "DT_JMPREL");
    assert_eq!(plt.encoding, RelocEncoding::Rela, "DT_PLTREL is DT_RELA");
    assert_eq!(plt.relocations.len(), PLT_JUMP_SLOT);
    assert_eq!(
        plt.count_of_type(R_AARCH64_JUMP_SLOT),
        PLT_JUMP_SLOT,
        "all 534 are R_AARCH64_JUMP_SLOT (1026)"
    );
    assert!(
        plt.relocations.iter().all(|r| r.is_symbolic()),
        "every JUMP_SLOT names a symbol"
    );
    assert!(
        plt.relocations.iter().all(|r| r.r_addend == 0),
        "JUMP_SLOT addends are zero"
    );
    // 534 distinct symbols: one PLT slot each, no duplicates.
    let mut syms: Vec<u32> = plt.relocations.iter().map(|r| r.r_sym()).collect();
    syms.sort_unstable();
    syms.dedup();
    assert_eq!(syms.len(), PLT_JUMP_SLOT);
    assert_eq!(plt.relocations[0].r_offset, 0x067d_1710);
    assert_eq!(plt.relocations[PLT_JUMP_SLOT - 1].r_offset, 0x067d_27b8);

    // Grand total: packed blob plus PLT, and nothing else.
    assert_eq!(relocs.general.len(), 1, "exactly one general table");
    assert_eq!(relocs.total(), GRAND_TOTAL, "568,272 + 534 = 568,806");
    assert_eq!(APS2_TOTAL + PLT_JUMP_SLOT, GRAND_TOTAL);
    assert_eq!(relocs.count_of_type(R_AARCH64_JUMP_SLOT), PLT_JUMP_SLOT);
    assert_eq!(relocs.count_of_type(R_AARCH64_RELATIVE), APS2_RELATIVE);
}

#[test]
fn streaming_decode_agrees_with_the_vec_decode() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    // The allocation-free path Task 5 will use must produce the same thing as the Vec path.
    let mut count = 0usize;
    let mut sum_offset = 0u64;
    let mut sum_addend = 0i128;
    let summary = elf
        .decode_packed_with(|r| {
            count += 1;
            sum_offset = sum_offset.wrapping_add(r.r_offset);
            sum_addend += r.r_addend as i128;
            Ok(())
        })
        .unwrap()
        .expect("libroblox.so has a packed table");
    assert_eq!(count, APS2_TOTAL);
    assert_eq!(sum_offset, 60_456_134_155_296);
    assert_eq!(sum_addend, 32_780_388_580_533);
    assert_eq!(summary.bytes_consumed, APS2_BLOB_BYTES);
    assert_eq!(summary.bytes_consumed, summary.bytes_total);
}

#[test]
fn decoding_the_whole_relocation_set_is_not_accidentally_quadratic() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    // Timed and reported rather than merely run: the brief asks for the wall time, and a
    // generous ceiling is the only assertion that stays honest across machines while still
    // catching an accidental O(n^2). A correct linear decoder is three orders of magnitude
    // inside this bound.
    let start = std::time::Instant::now();
    let mut count = 0usize;
    let summary = elf
        .decode_packed_with(|_| {
            count += 1;
            Ok(())
        })
        .unwrap()
        .unwrap();
    let streaming = start.elapsed();

    let start = std::time::Instant::now();
    let table = elf.relocations().unwrap();
    let with_vec = start.elapsed();

    assert_eq!(count, APS2_TOTAL);
    assert_eq!(summary.bytes_consumed, APS2_BLOB_BYTES);
    assert_eq!(table.total(), GRAND_TOTAL);
    eprintln!(
        "APS2 decode of {APS2_TOTAL} relocations: streaming {streaming:?}, \
         into a Vec (incl. .rela.plt) {with_vec:?} \
         [{} build]",
        if cfg!(debug_assertions) { "debug" } else { "release" }
    );
    assert!(
        streaming < std::time::Duration::from_secs(10),
        "streaming decode took {streaming:?}"
    );
}

#[test]
fn init_array_has_3594_entries_all_awaiting_relocation() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let d = elf.dynamic();
    let ia = d.init_array.expect("DT_INIT_ARRAY");
    assert_eq!(ia.entry_count(), INIT_ARRAY_ENTRIES as u64);
    assert_eq!(ia.size, INIT_ARRAY_ENTRIES as u64 * 8);
    assert_eq!(ia.vaddr, 0x067c_27a8);

    let ptrs = elf.init_array().unwrap();
    assert_eq!(ptrs.len(), INIT_ARRAY_ENTRIES);
    // Every slot is zero in the file: the pointers are produced by R_AARCH64_RELATIVE
    // relocations, so a loader that runs init_array before relocating calls 3,594 null pointers.
    assert!(
        ptrs.iter().all(|&p| p == 0),
        "every DT_INIT_ARRAY slot is zero before relocation"
    );
    // And the array is inside PT_GNU_RELRO, so it is written before RELRO is sealed.
    let relro = elf.relro().unwrap();
    assert!(ia.vaddr >= relro.p_vaddr && ia.vaddr < relro.vaddr_end());

    // DT_FINI_ARRAY exists with three entries; DT_INIT and DT_FINI themselves do not.
    assert_eq!(d.fini_array.map(|a| a.entry_count()), Some(3));
    assert!(d.init.is_none(), "no DT_INIT");
    assert!(d.fini.is_none(), "no DT_FINI");
    assert!(d.preinit_array.is_none(), "no DT_PREINIT_ARRAY");
}

#[test]
fn segments_match_the_measured_layout() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);

    let relro = elf.relro().expect("PT_GNU_RELRO must be present");
    assert_eq!(relro.p_memsz, RELRO_BYTES, "PT_GNU_RELRO coverage");
    assert_eq!(relro.p_vaddr, 0x062d_c1c0);

    assert_eq!(elf.load_segments().count(), 3, "three PT_LOAD segments");
    let (base, span) = elf.load_span();
    assert_eq!(base, 0, "the image starts at vaddr 0");
    assert_eq!(span, 0x0733_3c3c, "total loadable span");
    // Max p_align across PT_LOAD is 16 KiB: this is a 16 KiB-page binary.
    assert_eq!(
        elf.load_segments().map(|s| s.p_align).max(),
        Some(0x4000),
        "PT_LOAD alignment"
    );
    assert_eq!(elf.note_segments().count(), 1);
}

#[test]
fn no_tls_anywhere() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    // PT_TLS would have made `parse` fail outright; assert the absence directly too.
    assert!(
        !elf.segments().iter().any(|s| s.p_type == PT_TLS),
        "no PT_TLS segment"
    );
    assert!(elf.dynamic().flags & DF_STATIC_TLS == 0, "no DF_STATIC_TLS");
    elf.reject_tls_symbols()
        .expect("no symbol may have type STT_TLS");
    let symtab = elf.symbols().unwrap();
    assert_eq!(
        symtab.iter().filter(|s| s.as_ref().unwrap().is_tls()).count(),
        0,
        "zero STT_TLS symbols"
    );
}

#[test]
fn a_tls_segment_is_refused_rather_than_ignored() {
    let Some(bytes) = common::main_lib() else { return };
    // Rewrite the first PT_GNU_STACK header in place as PT_TLS and confirm the parser refuses
    // the result. Doing it on the real file proves the check is on the real path rather than on
    // a synthetic header a bug could bypass.
    let mut mutated = bytes.to_vec();
    let phoff = usize::try_from(ElfImage::parse(bytes).unwrap().header().e_phoff).unwrap();
    let phnum = ElfImage::parse(bytes).unwrap().header().e_phnum as usize;
    let mut patched = false;
    for i in 0..phnum {
        let at = phoff + i * SIZEOF_PHDR;
        let ty = u32::from_le_bytes(mutated[at..at + 4].try_into().unwrap());
        if ty == PT_GNU_STACK {
            mutated[at..at + 4].copy_from_slice(&PT_TLS.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(patched, "libroblox.so has a PT_GNU_STACK to repurpose");
    let err = ElfImage::parse(&mutated).expect_err("PT_TLS must be refused");
    assert!(
        matches!(err, omni_elf::ElfError::TlsSegmentUnsupported { .. }),
        "expected TlsSegmentUnsupported, got {err}"
    );
}

#[test]
fn has_565_undefined_symbols_with_data_imports_distinguished() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let undef = elf.undefined_symbols().unwrap();
    assert_eq!(undef.len(), UNDEFINED_SYMBOLS, "undefined symbols");

    // STT_FUNC vs STT_OBJECT must not be conflated: binding a data import to a function stub
    // produces a failure that names no symbol (D9).
    let funcs = undef.iter().filter(|s| s.sym.is_func()).count();
    let objects = undef.iter().filter(|s| s.sym.is_object()).count();
    let notype = undef
        .iter()
        .filter(|s| s.sym.sym_type() == STT_NOTYPE)
        .count();
    assert_eq!(funcs, 539, "STT_FUNC imports");
    assert_eq!(objects, 23, "STT_OBJECT (data) imports");
    assert_eq!(notype, 3, "STT_NOTYPE imports");
    assert_eq!(funcs + objects + notype, UNDEFINED_SYMBOLS);

    // The ten AMEDIAFORMAT_KEY_* data imports D9 calls out by name.
    let media_keys: Vec<&str> = undef
        .iter()
        .filter(|s| s.name.starts_with("AMEDIAFORMAT_KEY_"))
        .map(|s| s.name)
        .collect();
    assert_eq!(media_keys.len(), 10, "AMEDIAFORMAT_KEY_* imports: {media_keys:?}");
    assert!(
        undef
            .iter()
            .filter(|s| s.name.starts_with("AMEDIAFORMAT_KEY_"))
            .all(|s| s.sym.is_object()),
        "every AMEDIAFORMAT_KEY_* import is STT_OBJECT, not STT_FUNC"
    );

    // Named imports D9 depends on existing.
    for name in [
        "pthread_key_create",
        "pthread_getspecific",
        "pthread_setspecific",
        "dl_iterate_phdr",
    ] {
        assert!(
            undef.iter().any(|s| s.name == name),
            "{name} must be an undefined symbol"
        );
    }
    // A weak undefined import, which a loader must be allowed to leave unresolved.
    let weak: Vec<&str> = undef
        .iter()
        .filter(|s| s.sym.is_weak())
        .map(|s| s.name)
        .collect();
    assert!(
        weak.contains(&"__cxa_thread_atexit_impl"),
        "expected a weak undefined __cxa_thread_atexit_impl, weak set is {weak:?}"
    );
    // No ELF TLS and no ifunc imports.
    assert!(undef.iter().all(|s| !s.sym.is_tls()));
    assert!(undef.iter().all(|s| !s.sym.is_ifunc()));
}

#[test]
fn symbol_table_is_sized_from_dt_gnu_hash_alone() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    // libroblox.so has no DT_HASH, so the symbol count can only come from walking the GNU hash
    // chains. Section headers would answer it too, but a runtime loader does not have them.
    assert!(elf.dynamic().hash.is_none(), "DT_HASH is absent");
    assert!(elf.dynamic().gnu_hash.is_some(), "DT_GNU_HASH is present");
    assert_eq!(elf.symbol_count(), 1109, "derived .dynsym entry count");

    let gnu = elf.gnu_hash().unwrap().unwrap();
    assert_eq!(gnu.symndx(), 566, "first hashed symbol index");
    // Indices 1..symndx are exactly the imports: 566 - 1 == 565.
    assert_eq!(gnu.symndx() as usize - 1, UNDEFINED_SYMBOLS);

    // Cross-check against the section headers, which this file happens to retain. They are not
    // used by the parser, so agreement is independent evidence that the chain walk is right.
    let dynsym = elf
        .sections()
        .iter()
        .find(|s| s.sh_type == SHT_DYNSYM)
        .expect("libroblox.so retains section headers");
    assert_eq!(
        dynsym.sh_size / SIZEOF_SYM as u64,
        elf.symbol_count() as u64,
        ".dynsym section size must agree with the GNU-hash-derived count"
    );
}

#[test]
fn exported_symbols_all_resolve_through_dt_gnu_hash() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let exports = elf.exported_symbols().unwrap();
    assert_eq!(exports.len(), 543, "exported symbols");

    // Every export must be findable by name, at its own index. This exercises the bloom filter,
    // bucket and chain walk across all 543 of them rather than on one lucky name.
    for e in &exports {
        let found = elf
            .lookup_gnu(e.name)
            .unwrap_or_else(|err| panic!("lookup of {} failed: {err}", e.name));
        assert_eq!(
            found,
            Some(e.index),
            "DT_GNU_HASH lookup of {} returned {found:?}, expected {}",
            e.name,
            e.index
        );
    }
    assert!(exports.iter().any(|e| e.name == "JNI_OnLoad"));
    // Undefined symbols are deliberately unreachable through the hash table: the format only
    // indexes defined exports. A loader must not expect to find its own imports there.
    assert_eq!(elf.lookup_gnu("dl_iterate_phdr").unwrap(), None);
    // A name that is not there must miss cleanly rather than resolve to something adjacent.
    assert_eq!(elf.lookup_gnu("omnidroid_no_such_symbol").unwrap(), None);
    assert_eq!(elf.lookup_sysv("JNI_OnLoad").unwrap(), None, "no DT_HASH");
}

#[test]
fn dynamic_section_names_and_needed_libraries() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    assert_eq!(elf.soname().unwrap(), Some("libroblox.so"));
    let needed = elf.needed().unwrap();
    assert_eq!(
        needed,
        vec![
            "libOpenMAXAL.so",
            "libmediandk.so",
            "libandroid.so",
            "libm.so",
            "libOpenSLES.so",
            "libGLESv2.so",
            "libEGL.so",
            "liblog.so",
            "libdl.so",
            "libc.so",
        ],
        "DT_NEEDED in link order"
    );
    assert_eq!(elf.dynamic().entries.len(), 35, "dynamic entries incl. DT_NULL");
    assert_eq!(elf.dynamic().syment, Some(SIZEOF_SYM as u64));
    assert!(elf.dynamic().wants_bind_now(), "DF_BIND_NOW is set");
    assert!(
        elf.dynamic().unrecognised.is_empty(),
        "unmodelled dynamic tags: {:?}",
        elf.dynamic().unrecognised
    );
}

#[test]
fn notes_report_the_ndk_and_no_hardening_features() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let notes = elf.notes().unwrap();
    let owners: Vec<&str> = notes.iter().filter_map(|n| n.name_str()).collect();
    assert!(owners.contains(&"Android"), "note owners: {owners:?}");
    assert!(owners.contains(&"GNU"), "note owners: {owners:?}");

    let ident = elf.android_ident().unwrap().expect(".note.android.ident");
    assert_eq!(ident.android_api, 26, "minSdkVersion recorded in the note");
    assert_eq!(ident.ndk_version, "r28c");
    assert_eq!(ident.ndk_build_number, "13676358");

    // No .note.gnu.property at all, so no BTI/PAC/MTE to honour — D9's "no BTI/PAC/MTE".
    assert!(
        elf.gnu_properties().unwrap().is_none(),
        "no .note.gnu.property"
    );
    assert!(
        !elf.segments().iter().any(|s| s.p_type == PT_GNU_PROPERTY),
        "no PT_GNU_PROPERTY segment"
    );
    assert_eq!(elf.build_id().unwrap().map(|b| b.len()), Some(20));
}

#[test]
fn the_relocation_count_limit_has_two_orders_of_magnitude_of_headroom() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);

    // The ceiling is derived from the object's own *validated* image, so it is worth pinning the
    // derivation as well as the margin: if a future library ever came close to its own cap, this
    // number would shrink long before anything was rejected.
    let img = elf.load_image();
    assert_eq!(img.base_vaddr, 0);
    assert_eq!(img.end_vaddr, 0x0733_3c3c);
    assert_eq!(img.span, 0x0733_3c3c, "image span");
    assert_eq!(
        img.mapped_bytes, 120_767_564,
        "measure of the union of the PT_LOAD memory ranges"
    );
    assert_eq!(img.segment_count, 3);
    assert_eq!(img.max_align, 0x4000);
    // The three segments do not overlap, so the union happens to equal the sum. It is still
    // computed as a union, because an overlap would otherwise inflate the figure the bound rests
    // on — and inflating it is the unsafe direction.
    assert_eq!(
        elf.load_segments().map(|s| s.p_memsz).sum::<u64>(),
        img.mapped_bytes
    );
    assert!(
        img.mapped_bytes <= img.span,
        "mapped bytes can never exceed the span"
    );
    assert_eq!(elf.loadable_size(), img.mapped_bytes);

    let limits = elf.aps2_limits();
    assert_eq!(limits.max_relocations, 60_383_782);
    assert_eq!(limits.image_span, 0x0733_3c3c);
    assert_eq!(limits, omni_elf::Aps2Limits::for_image(img));
    // For this object the derived figure binds, not the flat ceiling.
    assert!(limits.max_relocations < omni_elf::Aps2Limits::MAX_RELOCATIONS);
    let headroom = limits.max_relocations / GRAND_TOTAL as u64;
    assert!(
        headroom >= 100,
        "only {headroom}x headroom between {GRAND_TOTAL} relocations and the {} cap",
        limits.max_relocations
    );
    // And the blob's own declared count is of course far below it, which is why the real decode
    // in the other tests never touches the limit.
    let relocs = elf.relocations().unwrap();
    let packed = relocs
        .general_with(RelocEncoding::AndroidPackedRela)
        .unwrap();
    assert!(packed.packed.unwrap().declared_count < limits.max_relocations);
}

#[test]
fn a_tampered_declared_count_is_refused_through_elfimage_rather_than_hanging() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let table = elf.dynamic().android_rela.expect("DT_ANDROID_RELA");
    let blob_at = elf
        .vaddr_to_offset(table.vaddr)
        .expect("the blob has a file offset");

    // Overwrite the real blob's header in place with the magic followed by a 2^62 relocation
    // count, leaving the remaining ~2.1 MB of the blob untouched. This is D6's expected case: a
    // tampered binary. Done on the real 109 MB file so the check is proven to be on the real
    // ElfImage path, not only on a synthetic blob handed straight to the decoder.
    let mut mutated = bytes.to_vec();
    let mut header = Vec::from(*b"APS2");
    omni_elf::aps2::encode_sleb128(1 << 62, &mut header);
    mutated[blob_at..blob_at + header.len()].copy_from_slice(&header);

    let tampered = ElfImage::parse(&mutated).expect("only the blob was touched");
    assert_eq!(tampered.aps2_limits().max_relocations, 60_383_782);

    // Both entry points, both bounded, both fast. Before the fix, `relocations()` aborted the
    // process on a failed 12 GB allocation and `decode_packed_with` streamed hundreds of
    // millions of relocations without stopping.
    let start = std::time::Instant::now();
    let err = tampered
        .relocations()
        .expect_err("a 2^62 declared count must be refused");
    assert_eq!(
        err,
        omni_elf::ElfError::Aps2CountExceedsLimit {
            declared: 1 << 62,
            limit: 60_383_782,
        }
    );

    let mut produced = 0u64;
    let err = tampered
        .decode_packed_with(|_| {
            produced += 1;
            Ok(())
        })
        .expect_err("the streaming path must be refused too");
    assert!(matches!(
        err,
        omni_elf::ElfError::Aps2CountExceedsLimit { .. }
    ));
    assert_eq!(produced, 0, "nothing may be produced");
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "refusing a tampered count took {elapsed:?}"
    );
}

/// File offset of the first `PT_LOAD` program header in the real file, for the tamper tests below.
fn first_load_phdr_offset(bytes: &[u8]) -> usize {
    let elf = image(bytes);
    let phoff = usize::try_from(elf.header().e_phoff).unwrap();
    for i in 0..elf.header().e_phnum as usize {
        let at = phoff + i * SIZEOF_PHDR;
        if u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) == PT_LOAD {
            return at;
        }
    }
    panic!("libroblox.so has a PT_LOAD");
}

/// Same, but for a `PT_LOAD` whose `p_vaddr` is non-zero, so that adding to `p_memsz` can overflow.
fn load_phdr_offset_with_nonzero_vaddr(bytes: &[u8]) -> usize {
    let elf = image(bytes);
    let phoff = usize::try_from(elf.header().e_phoff).unwrap();
    for i in 0..elf.header().e_phnum as usize {
        let at = phoff + i * SIZEOF_PHDR;
        let is_load = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) == PT_LOAD;
        let vaddr = u64::from_le_bytes(bytes[at + 16..at + 24].try_into().unwrap());
        if is_load && vaddr != 0 {
            return at;
        }
    }
    panic!("libroblox.so has a PT_LOAD at a non-zero vaddr");
}

#[test]
fn a_forged_p_memsz_cannot_buy_a_larger_relocation_budget() {
    let Some(bytes) = common::main_lib() else { return };
    let at = first_load_phdr_offset(bytes);

    // The eight-byte attack: p_memsz = u64::MAX on one PT_LOAD. Before PT_LOAD validation this
    // raised the derived cap to 2^63-1, and the count check then waved a 2^62 declaration through.
    //
    // On the *first* PT_LOAD, whose p_vaddr is 0, `0 + u64::MAX` does not overflow, so it is the
    // span ceiling that catches it rather than the overflow check. Both are refusals naming the
    // value, and the two are asserted separately below so neither is assumed to cover the other.
    let mut mutated = bytes.to_vec();
    mutated[at + 40..at + 48].copy_from_slice(&u64::MAX.to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("a p_memsz of u64::MAX must be refused");
    assert!(
        matches!(
            err,
            omni_elf::ElfError::ImageSpanTooLarge {
                span: u64::MAX,
                limit: omni_elf::MAX_IMAGE_SPAN
            }
        ),
        "expected ImageSpanTooLarge, got {err}"
    );

    // And on a PT_LOAD with a non-zero p_vaddr the same value overflows the address space, which
    // must be an error rather than a saturated (i.e. larger) figure.
    let mut mutated = bytes.to_vec();
    let at_nonzero = load_phdr_offset_with_nonzero_vaddr(bytes);
    mutated[at_nonzero + 40..at_nonzero + 48].copy_from_slice(&u64::MAX.to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("an overflowing p_memsz must be refused");
    assert!(
        matches!(
            err,
            omni_elf::ElfError::SegmentMemRangeOverflow {
                memsz: u64::MAX,
                ..
            }
        ),
        "expected SegmentMemRangeOverflow, got {err}"
    );

    // And a *plausible* lie, which overflows nothing and contradicts no other header field: 1 TiB.
    // This is the case a pure overflow check would miss, which is why there is a span ceiling too.
    let mut mutated = bytes.to_vec();
    mutated[at + 40..at + 48].copy_from_slice(&(1u64 << 40).to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("a 1 TiB p_memsz must be refused");
    assert!(
        matches!(
            err,
            omni_elf::ElfError::ImageSpanTooLarge {
                limit: omni_elf::MAX_IMAGE_SPAN,
                ..
            }
        ),
        "expected ImageSpanTooLarge, got {err}"
    );

    // A p_memsz that is merely larger but still under the span ceiling is accepted, and the cap
    // then rises only as far as the flat ceiling allows. This is the honest boundary of the
    // defence, asserted rather than hidden: header validation removes the absurd values, the span
    // ceiling bounds the plausible ones, and the flat relocation ceiling bounds what is left.
    let mut mutated = bytes.to_vec();
    mutated[at + 40..at + 48].copy_from_slice(&(3u64 << 30).to_le_bytes()); // 3 GiB
    let widened = ElfImage::parse(&mutated).expect("3 GiB is under the span ceiling");
    // Exactly 3 GiB, not 3 GiB plus the other two segments: widening the first PT_LOAD swallowed
    // them, and the union counts the overlap once. A naive sum would have reported 3 GiB + 17 MB,
    // which is the double-counting that would inflate the bound.
    assert_eq!(widened.load_image().mapped_bytes, 3 << 30);
    assert_eq!(widened.load_image().span, 3 << 30);
    assert!(
        widened.load_segments().map(|s| s.p_memsz).sum::<u64>() > widened.load_image().mapped_bytes,
        "the sum over-counts here, which is why mapped_bytes is a union"
    );
    assert_eq!(
        widened.aps2_limits().max_relocations,
        omni_elf::Aps2Limits::MAX_RELOCATIONS,
        "the flat ceiling, not the forged image, is what binds now"
    );
    assert_eq!(omni_elf::Aps2Limits::MAX_RELOCATIONS, 67_108_864);

    // With the forged-but-accepted image, a 2^62 declaration is still refused — by the ceiling
    // rather than by the derived figure, which is the point of having both.
    let table = widened.dynamic().android_rela.expect("DT_ANDROID_RELA");
    let blob_at = widened.vaddr_to_offset(table.vaddr).expect("blob offset");
    let mut header = Vec::from(*b"APS2");
    omni_elf::aps2::encode_sleb128(1 << 62, &mut header);
    let mut both = mutated.clone();
    both[blob_at..blob_at + header.len()].copy_from_slice(&header);
    let tampered = ElfImage::parse(&both).expect("only headers and the blob were touched");
    let mut produced = 0u64;
    let err = tampered
        .decode_packed_with(|_| {
            produced += 1;
            Ok(())
        })
        .expect_err("2^62 must be refused even with an inflated image");
    assert_eq!(
        err,
        omni_elf::ElfError::Aps2CountExceedsLimit {
            declared: 1 << 62,
            limit: 67_108_864,
        }
    );
    assert_eq!(produced, 0);
}

#[test]
fn other_forged_pt_load_fields_are_refused_on_the_real_file() {
    let Some(bytes) = common::main_lib() else { return };
    let at = first_load_phdr_offset(bytes);

    // p_filesz running past the end of the file.
    let mut mutated = bytes.to_vec();
    mutated[at + 32..at + 40].copy_from_slice(&(bytes.len() as u64 + 1).to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("a p_filesz past EOF must be refused");
    assert!(
        matches!(err, omni_elf::ElfError::SegmentOutsideFile { .. }),
        "got {err}"
    );

    // p_filesz greater than p_memsz.
    let mut mutated = bytes.to_vec();
    let memsz = u64::from_le_bytes(mutated[at + 40..at + 48].try_into().unwrap());
    mutated[at + 32..at + 40].copy_from_slice(&(memsz + 8).to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("p_filesz > p_memsz must be refused");
    assert!(
        matches!(
            err,
            omni_elf::ElfError::SegmentFileSizeExceedsMemSize { .. }
                | omni_elf::ElfError::SegmentOutsideFile { .. }
        ),
        "got {err}"
    );

    // p_vaddr moved off its alignment congruence with p_offset.
    let mut mutated = bytes.to_vec();
    let vaddr = u64::from_le_bytes(mutated[at + 16..at + 24].try_into().unwrap());
    mutated[at + 16..at + 24].copy_from_slice(&(vaddr + 1).to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("a non-congruent p_vaddr must be refused");
    assert!(
        matches!(err, omni_elf::ElfError::SegmentAlignMismatch { .. }),
        "got {err}"
    );

    // A p_align that is not a power of two.
    let mut mutated = bytes.to_vec();
    mutated[at + 48..at + 56].copy_from_slice(&3u64.to_le_bytes());
    let err = ElfImage::parse(&mutated).expect_err("a non-power-of-two p_align must be refused");
    assert!(
        matches!(
            err,
            omni_elf::ElfError::SegmentAlignNotPowerOfTwo { align: 3, .. }
        ),
        "got {err}"
    );

    // The untampered file still parses, so each rejection above was about the mutation rather than
    // about something incidental to the validator.
    assert!(ElfImage::parse(bytes).is_ok());
}

#[test]
fn every_group_in_the_real_blob_pays_bytes_per_relocation() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let table = elf.dynamic().android_rela.expect("DT_ANDROID_RELA");
    let blob = elf
        .slice_at_vaddr("DT_ANDROID_RELA", table.vaddr, table.size)
        .unwrap();

    // Decode with `image_span = 0`, which makes layers 2 and 3 reject *any* group that spends no
    // bytes per relocation: a zero stride is refused outright, and a non-zero stride cannot fit a
    // zero-byte image. Getting all 568,272 relocations out anyway proves every one of the 46,184
    // groups is byte-paying — so layer 1 alone bounds this binary and no header field is consulted
    // to do it.
    //
    // `observed_group_flags` cannot establish this. It is a union over groups, and a union says
    // nothing about *every* group: flags 11, 9 and 8 all happen to be byte-paying, but a union of
    // {11, 9, 8} is equally consistent with a fourth group of flags 3 that is not.
    let mut count = 0usize;
    let summary = omni_elf::aps2::decode_with(
        blob,
        omni_elf::PackedFormat::Rela,
        omni_elf::Aps2Limits::new(u64::MAX, 0),
        |_| {
            count += 1;
            Ok(())
        },
    )
    .expect("every group must be byte-paying, so a zero image span changes nothing");
    assert_eq!(count, APS2_TOTAL);
    assert_eq!(summary.group_count, 46_184);
    assert_eq!(summary.bytes_consumed, APS2_BLOB_BYTES);
    assert_eq!(summary.bytes_consumed, summary.bytes_total);

    // Sanity check on the probe itself: a zero-cost group really is refused at this image span,
    // so the decode above succeeded because of the blob's shape and not because the layers are
    // inert when `image_span` is zero.
    let mut zero_cost = Vec::from(*b"APS2");
    omni_elf::aps2::encode_sleb128(2, &mut zero_cost);
    omni_elf::aps2::encode_sleb128(0x1000, &mut zero_cost);
    omni_elf::aps2::encode_sleb128(2, &mut zero_cost);
    omni_elf::aps2::encode_sleb128(
        (omni_elf::aps2::RELOCATION_GROUPED_BY_INFO_FLAG
            | omni_elf::aps2::RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG) as i64,
        &mut zero_cost,
    );
    omni_elf::aps2::encode_sleb128(8, &mut zero_cost);
    omni_elf::aps2::encode_sleb128(R_AARCH64_RELATIVE as i64, &mut zero_cost);
    assert!(matches!(
        omni_elf::aps2::decode_rela(&zero_cost, omni_elf::Aps2Limits::new(u64::MAX, 0)).unwrap_err(),
        omni_elf::ElfError::Aps2GroupExceedsImage { image_span: 0, .. }
    ));
}
