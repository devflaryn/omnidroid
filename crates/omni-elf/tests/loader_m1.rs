//! Milestone **M1**: the real `libroblox.so` mapped, relocated and accounted for.
//!
//! Every number asserted here was measured from the real binary and is recorded in
//! `docs/DECISIONS.md` D9. Nothing here is asserted against a counter alone where memory can be read
//! instead: a counter cannot tell an applied relocation from a skipped one, and - as Task 4 found -
//! it cannot tell a correct value from a wrong one either. So
//! `relative_relocations_really_contain_base_plus_addend` reads the mapped memory back, and
//! `every_import_binds_to_its_provider_and_every_slot_holds_the_expected_value` checks all 612
//! symbolic slots against an independently computed expectation.
//!
//! The commit-charge measurements live in `loader_commit.rs`, because commit charge is a per-process
//! quantity and `cargo test` runs one binary's tests as parallel threads.
//!
//! When the APK is absent every test here **skips loudly** rather than passing quietly.

mod common;

use std::collections::BTreeMap;

use common::fixture::{
    fixture, load, measuring, Fixture, APS2_TOTAL, FINI_ARRAY_ENTRIES, GRAND_TOTAL, IMPORTS,
    IMPORTS_FUNC, IMPORTS_NOTYPE, IMPORTS_OBJECT, INIT_ARRAY_ENTRIES, N_ABS64, N_GLOB_DAT,
    N_RELATIVE, PLT_TOTAL, RELRO_BYTES, RELRO_VADDR, R_ABS64, R_GLOB_DAT, R_JUMP_SLOT, R_RELATIVE,
    SPAN,
};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry, SymbolKind, UnresolvedPolicy};
use omni_mem::Protection;

/// Held for the whole of each test, so only one test is mapping at a time.
///
/// Every test here maps the real 109 MB library, and `cargo test` runs a binary's tests as parallel
/// threads: eight concurrent loads stack more than a gigabyte of mappings and their relocation
/// windows, which is a flaky out-of-memory failure waiting for a loaded machine rather than a real
/// defect. `loader_commit.rs` has had this since it was written, for the related reason that commit
/// charge is a per-process quantity.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the serialising lock, ignoring poisoning: a panic in one test must not cascade into every
/// other test reporting a lock error instead of its own result.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

// -------------------------------------------------------------------------------------------------
// 1. Mapping
// -------------------------------------------------------------------------------------------------

#[test]
fn every_pt_load_lands_at_base_plus_p_vaddr() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let page = f.space.page_size() as u64;
    let targets = relocation_targets(&f);
    let object = load(&f, &measuring());

    assert_eq!(object.base % 0x4000, 0, "the load base honours p_align = 16 KiB");
    assert_eq!(object.start, object.base, "libroblox.so has base_vaddr 0");
    assert_eq!(
        object.span() as u64,
        (SPAN + page - 1) & !(page - 1),
        "the reserved span is the program headers' span, page-rounded"
    );

    // Every PT_LOAD is covered, from its first page to its last, at exactly base + p_vaddr.
    for (index, seg) in f.elf.segments().iter().enumerate() {
        if !seg.is_load() {
            continue;
        }
        let first = object.base + (seg.p_vaddr & !(page - 1)) as usize;
        let last = object.base + (seg.vaddr_end().next_multiple_of(page) - page) as usize;
        let head = object.range_at(first).unwrap_or_else(|| {
            panic!("PT_LOAD {index} has nothing mapped at its first page {first:#x}")
        });
        assert_eq!(head.segment, index);
        assert_eq!(head.start, first, "PT_LOAD {index} starts at base + page_down(p_vaddr)");
        assert!(
            object.range_at(last).is_some_and(|r| r.segment == index),
            "PT_LOAD {index} has nothing mapped at its last page {last:#x}"
        );
        // And the bytes at some virtual address are the bytes at the matching file offset: proof
        // the file offset was biased by the same amount as the address, which is what the gABI's
        // `p_vaddr ≡ p_offset (mod p_align)` congruence buys and what makes `…c1c0` mappable at all.
        //
        // The window has to be one no relocation wrote to, or this compares the relocated value
        // against the file's zero and fails for a loader that is working perfectly.
        if seg.p_filesz >= 64 {
            let vaddr = untouched_window(&targets, seg, 32)
                .unwrap_or_else(|| panic!("PT_LOAD {index} has no relocation-free 32-byte window"));
            let file_at = (seg.p_offset + (vaddr - seg.p_vaddr)) as usize;
            let expect = &f.elf.data()[file_at..file_at + 32];
            let at = object.base + vaddr as usize;
            let got = unsafe {
                core::slice::from_raw_parts(f.space.ptr(at, 32).expect("in the space"), 32)
            };
            assert_eq!(
                got, expect,
                "PT_LOAD {index} at p_vaddr {vaddr:#x} is mapped from the wrong file offset"
            );
        }
    }

    // The three segments of this library, at their measured protections.
    let protections: Vec<(usize, Protection, bool, usize)> = object
        .ranges
        .iter()
        .map(|r| (r.segment, r.rest, r.anonymous, r.len()))
        .collect();
    eprintln!("\nlibroblox.so layout, base {:#x}:", object.base);
    for r in &object.ranges {
        eprintln!(
            "  seg {} {:#014x}..{:#014x} {:>10} {:<12} {}",
            r.segment,
            r.start,
            r.end,
            r.len(),
            r.rest.to_string(),
            if r.anonymous { "private" } else { "file" }
        );
    }
    assert!(
        protections
            .iter()
            .any(|&(s, p, anon, _)| s == 1 && p == Protection::ReadExecute && !anon),
        "the text segment must rest ReadExecute and stay file-backed"
    );
    assert!(
        protections.iter().any(|&(_, p, anon, len)| p == Protection::ReadWrite
            && anon
            && len > 11 * 1024 * 1024),
        "the 11.6 MB .bss must be private anonymous memory"
    );

    // The holes between segments stay owned and inaccessible, as bionic leaves them PROT_NONE.
    let hole = object.base + 0x67d3000;
    assert!(object.range_at(hole).is_none(), "the gap before .data is not part of any segment");
    assert_eq!(
        f.space.region_at(hole).map(|r| r.protection),
        Some(Protection::None),
        "the gap is still owned by this process and inaccessible"
    );

    // dl_iterate_phdr state: PT_PHDR sits at p_vaddr 0x40 inside the text segment.
    let info = object.dl_phdr_info();
    assert_eq!(info.addr, object.base);
    assert_eq!(info.phdr, object.base + 0x40);
    assert_eq!(info.phnum, 9);
    assert_eq!(object.soname.as_deref(), Some("libroblox.so"));
    assert_eq!(object.needed.len(), 10);

    object.unload(&f.space).expect("unload");
}

/// A provider that supplies every import, at a distinctive non-zero address derived from its name.
///
/// Needed for more than exercising the seam. Two of the assertions below are impossible against the
/// [`EmptyProvider`]: an unresolved symbolic relocation writes a **null**, which is
/// indistinguishable from a slot that was never written — or from one a runaway `memset` cleared.
/// With every import bound to a unique non-zero value, every writable slot in the library has a
/// known expected value and both questions become answerable.
struct StubProvider;

fn stub_address(name: &str) -> u64 {
    // FNV-1a, folded into a 47-bit address range and 8-byte aligned. Never zero.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    0x7000_0000_0000 | (h & 0x0fff_ffff_fff8) | 0x8
}

impl omni_elf::SymbolProvider for StubProvider {
    fn name(&self) -> &str {
        "<stub>"
    }

    fn resolve(&self, request: &omni_elf::SymbolRequest<'_>) -> Option<omni_elf::SymbolValue> {
        Some(omni_elf::SymbolValue { address: stub_address(request.name), kind: request.kind })
    }
}

fn stub_registry() -> ProviderRegistry {
    let mut r = ProviderRegistry::new();
    r.register(StubProvider);
    r
}

/// The value every relocated slot must hold, keyed by unrelocated virtual address.
///
/// Built from the file and from the stub provider, never from anything the loader produced, so the
/// comparison is against an independently computed expectation.
fn expected_values(f: &Fixture, base: usize) -> BTreeMap<u64, u64> {
    let tables = f.elf.relocations().expect("decode");
    let symtab = f.elf.symbols().expect("symbols");
    let strtab = f.elf.strtab().expect("strtab");
    let mut out = BTreeMap::new();
    for r in tables.general.iter().chain(tables.plt.iter()).flat_map(|t| t.relocations.iter()) {
        let value = match r.r_type() {
            R_RELATIVE => (base as u64).wrapping_add(r.r_addend as u64),
            R_GLOB_DAT | R_JUMP_SLOT | R_ABS64 => {
                let sym = symtab.get(r.r_sym()).expect("symbol");
                let name = strtab.get(u64::from(sym.st_name)).expect("name");
                let target = if sym.is_undefined() {
                    stub_address(name)
                } else {
                    base as u64 + sym.st_value
                };
                target.wrapping_add(r.r_addend as u64)
            }
            other => panic!("unexpected relocation type {other}"),
        };
        out.insert(r.r_offset, value);
    }
    out
}

#[test]
fn every_import_binds_to_its_provider_and_every_slot_holds_the_expected_value() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = loader::load(
        &f.space,
        &f.backing,
        &f.elf,
        &stub_registry(),
        &LoaderConfig::default(),
    )
    .expect("load with a provider that supplies everything");

    assert_eq!(object.imports.resolved.len(), IMPORTS, "all 565 imports bind");
    assert!(object.imports.unresolved.is_empty());
    assert!(object.imports.kind_mismatches().is_empty(), "the stub answers with the asked-for kind");
    assert_eq!(
        object.stats.relocations.bound_to_null, 0,
        "nothing binds to null when every import is supplied"
    );

    // Every symbolic slot holds `provider address + addend`, read back from mapped memory. This is
    // what proves the 22 R_AARCH64_ABS64 relocations are **64-bit** stores: a 32-bit store would
    // leave the high half of the stub's 0x7000_.... address as zero.
    let expected = expected_values(&f, object.base);
    let tables = f.elf.relocations().expect("decode");
    let mut symbolic = 0usize;
    let mut abs64 = 0usize;
    for r in tables.general.iter().chain(tables.plt.iter()).flat_map(|t| t.relocations.iter()) {
        if r.r_type() == R_RELATIVE {
            continue;
        }
        let want = expected[&r.r_offset];
        let got = unsafe {
            f.space
                .ptr(object.base + r.r_offset as usize, 8)
                .expect("in the space")
                .cast::<u64>()
                .read_unaligned()
        };
        assert_eq!(
            got, want,
            "{} at r_offset {:#x} holds {got:#x}, expected {want:#x}",
            r.type_name().unwrap_or("?"),
            r.r_offset
        );
        assert!(want > u64::from(u32::MAX), "the expected value must exceed 32 bits");
        symbolic += 1;
        if r.r_type() == R_ABS64 {
            abs64 += 1;
        }
    }
    assert_eq!(symbolic, N_GLOB_DAT + N_ABS64 + PLT_TOTAL);
    assert_eq!(abs64, N_ABS64);
    eprintln!("{symbolic} symbolic slots verified against a resolving provider, {abs64} of them ABS64");

    object.unload(&f.space).expect("unload");
}

#[test]
fn the_bss_tail_of_the_last_file_page_is_zeroed_without_disturbing_file_pages() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let page = f.space.page_size() as u64;
    // Loaded with the resolving provider so that every relocated slot has a non-zero expected
    // value: with the empty provider the slots just below one segment's file end are 534 unresolved
    // JUMP_SLOTs, all null, and a runaway zero-fill would be undetectable there.
    let object = loader::load(
        &f.space,
        &f.backing,
        &f.elf,
        &stub_registry(),
        &LoaderConfig::default(),
    )
    .expect("load");
    let expected = expected_values(&f, object.base);

    let mut checked = 0usize;
    for (index, seg) in f.elf.segments().iter().enumerate() {
        if !seg.is_load() || seg.p_memsz <= seg.p_filesz {
            continue;
        }
        let file_end = seg.p_vaddr + seg.p_filesz;
        let page_end = file_end.next_multiple_of(page).min(seg.vaddr_end().next_multiple_of(page));
        if page_end <= file_end {
            continue;
        }

        // The tail is zero...
        let at = object.base + file_end as usize;
        let len = (page_end - file_end) as usize;
        let tail = unsafe {
            core::slice::from_raw_parts(f.space.ptr(at, len).expect("in the space"), len)
        };
        assert!(
            tail.iter().all(|&b| b == 0),
            "PT_LOAD {index}: {len} bytes of .bss tail at {file_end:#x} must be zero"
        );
        // ...and the file held something else there, so the assertion means something.
        let from = (seg.p_offset + seg.p_filesz) as usize;
        assert!(
            f.elf.data()[from..from + len].iter().any(|&b| b != 0),
            "PT_LOAD {index}: the file already held zeroes in that tail"
        );

        // And the fill did not overrun downwards: the eight bytes immediately below the segment's
        // file end still hold what they should, whether that is file content or a relocated value.
        let probe = file_end - 8;
        let want = match expected.get(&probe) {
            Some(&v) => v,
            None => {
                let off = f.elf.vaddr_to_offset(probe).expect("inside the file image");
                u64::from_le_bytes(f.elf.data()[off..off + 8].try_into().unwrap())
            }
        };
        assert_ne!(want, 0, "PT_LOAD {index}: the no-overrun probe needs a non-zero expectation");
        let got = unsafe {
            f.space
                .ptr(object.base + probe as usize, 8)
                .expect("in the space")
                .cast::<u64>()
                .read_unaligned()
        };
        assert_eq!(
            got, want,
            "PT_LOAD {index}: the zero-fill overran into the segment's own last eight bytes at \
             {probe:#x}"
        );
        eprintln!(
            "PT_LOAD {index}: {len} bytes zeroed at {file_end:#x}; {probe:#x} still holds {want:#x}"
        );
        checked += 1;
    }
    assert_eq!(checked, 2, "libroblox.so has two segments whose p_memsz exceeds p_filesz");

    object.unload(&f.space).expect("unload");
}


// -------------------------------------------------------------------------------------------------
// 2. Relocations — asserted against memory, not against a counter
// -------------------------------------------------------------------------------------------------

#[test]
fn all_568806_relocations_are_applied() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = load(&f, &measuring());
    let s = &object.stats.relocations;

    assert_eq!(s.total, GRAND_TOTAL, "568,272 from APS2 plus 534 from DT_JMPREL");
    assert_eq!(s.packed, APS2_TOTAL);
    assert_eq!(s.plt, PLT_TOTAL);
    assert_eq!(s.applied, GRAND_TOTAL, "every one wrote to memory");
    assert_eq!(s.none, 0, "no R_AARCH64_NONE padding in this library");
    assert_eq!(s.count_of_type(R_RELATIVE), N_RELATIVE);
    assert_eq!(s.count_of_type(R_GLOB_DAT), N_GLOB_DAT);
    // 257, not 258. ABS64 is a **64-bit** store; an earlier draft of the plan said ABS32.
    assert_eq!(s.count_of_type(R_ABS64), N_ABS64);
    assert_eq!(s.count_of_type(R_JUMP_SLOT), PLT_TOTAL);
    assert_eq!(
        s.by_type.values().sum::<usize>(),
        GRAND_TOTAL,
        "the per-type counts account for every relocation"
    );
    assert_eq!(
        s.symbolic,
        N_GLOB_DAT + N_ABS64 + PLT_TOTAL,
        "78 symbolic relocations in the APS2 blob plus 534 JUMP_SLOTs"
    );
    // 611 of the 612, not all of them: one references a symbol `libroblox.so` itself **defines**,
    // so it resolves against the object's own symbol table and never reaches a provider. A loader
    // that asked the provider registry first would leave that one null.
    assert_eq!(
        s.bound_to_null,
        s.symbolic - 1,
        "611 of the 612 symbolic relocations have nothing to bind to"
    );
    let tables = f.elf.relocations().expect("decode");
    let symtab = f.elf.symbols().expect("symbols");
    let strtab = f.elf.strtab().expect("strtab");
    let mut self_bound = Vec::new();
    for r in tables.general.iter().chain(tables.plt.iter()).flat_map(|t| t.relocations.iter()) {
        if r.r_sym() == 0 {
            continue;
        }
        let sym = symtab.get(r.r_sym()).expect("symbol");
        if !sym.is_undefined() {
            self_bound.push((strtab.get(u64::from(sym.st_name)).unwrap_or("<unnamed>"), r.r_offset, sym.st_value));
        }
    }
    assert_eq!(self_bound.len(), 1, "exactly one symbolic relocation binds inside the object");
    eprintln!(
        "the one self-bound symbolic relocation: {:?} at r_offset {:#x}, st_value {:#x}",
        self_bound[0].0, self_bound[0].1, self_bound[0].2
    );
    // And memory holds `base + st_value`, not null.
    let object2 = object;
    let at = object2.base + self_bound[0].1 as usize;
    let got = unsafe { f.space.ptr(at, 8).expect("in the space").cast::<u64>().read_unaligned() };
    assert_eq!(
        got,
        (object2.base + self_bound[0].2 as usize) as u64,
        "the self-bound relocation must hold base + st_value"
    );
    let object = object2;
    object.unload(&f.space).expect("unload");
}

#[test]
fn relative_relocations_really_contain_base_plus_addend() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = load(&f, &LoaderConfig::default());

    // Re-decode independently of the loader, so the expected values come from the file and not from
    // anything the loader computed.
    let tables = f.elf.relocations().expect("decode the relocation tables");
    let packed = tables
        .general
        .iter()
        .find(|t| t.tag == "DT_ANDROID_RELA")
        .expect("libroblox.so carries DT_ANDROID_RELA");

    let relative: Vec<_> =
        packed.relocations.iter().filter(|r| r.r_type() == R_RELATIVE).collect();
    assert_eq!(relative.len(), N_RELATIVE);

    // A spread sample plus both ends. Every one of these is a *read of mapped memory*: the file
    // holds zero at each of these offsets, so a match proves the store happened and produced the
    // right value, which no counter can show.
    let mut checked = 0usize;
    let mut nonzero_expected = 0usize;
    let step = relative.len() / 4096;
    for (i, r) in relative.iter().enumerate() {
        if i != 0 && i != relative.len() - 1 && i % step != 0 {
            continue;
        }
        let expect = (object.base as u64).wrapping_add(r.r_addend as u64);
        let at = object.base + r.r_offset as usize;
        let got =
            unsafe { f.space.ptr(at, 8).expect("in the space").cast::<u64>().read_unaligned() };
        assert_eq!(
            got, expect,
            "R_AARCH64_RELATIVE at r_offset {:#x} (sample {i}) holds {got:#x}, expected \
             base {:#x} + addend {:#x}",
            r.r_offset, object.base, r.r_addend
        );
        // And the file really did hold something else there, so this is not a coincidence.
        if let Some(off) = f.elf.vaddr_to_offset(r.r_offset) {
            let in_file = u64::from_le_bytes(f.elf.data()[off..off + 8].try_into().unwrap());
            assert_ne!(in_file, expect, "the file already held the relocated value");
        }
        if expect != 0 {
            nonzero_expected += 1;
        }
        checked += 1;
    }
    assert!(checked >= 4096, "sampled only {checked} relocations");
    assert_eq!(nonzero_expected, checked, "every expected value is non-zero");
    eprintln!("verified {checked} R_AARCH64_RELATIVE targets against mapped memory");

    // The symbolic ones bound to null, which is what the empty provider means.
    for r in packed.relocations.iter().filter(|r| r.r_type() != R_RELATIVE).take(78) {
        let at = object.base + r.r_offset as usize;
        let got =
            unsafe { f.space.ptr(at, 8).expect("in the space").cast::<u64>().read_unaligned() };
        assert_eq!(got, 0, "unresolved {} at {:#x} must hold null", r.type_name().unwrap_or("?"), r.r_offset);
    }

    object.unload(&f.space).expect("unload");
}

#[test]
fn the_534_jump_slots_come_from_dt_jmprel_and_land_in_the_plt_got() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let tables = f.elf.relocations().expect("decode");
    let plt = tables.plt.as_ref().expect("DT_JMPREL");
    assert_eq!(plt.relocations.len(), PLT_TOTAL);
    assert!(
        plt.relocations.iter().all(|r| r.r_type() == R_JUMP_SLOT),
        "every DT_JMPREL entry is a JUMP_SLOT"
    );
    // They are not in the packed blob, which is the whole point of them being a separate table.
    let packed = tables
        .general
        .iter()
        .find(|t| t.tag == "DT_ANDROID_RELA")
        .expect("DT_ANDROID_RELA");
    assert_eq!(packed.count_of_type(R_JUMP_SLOT), 0);

    // Where they land is load-bearing and was not what the brief assumed. `DT_PLTGOT` is
    // **inside** `PT_GNU_RELRO` (0x67d16f8, against a relro region of 0x62dc1c0..0x67d3000), and
    // `DT_FLAGS` carries `DF_BIND_NOW`. So this is a full-RELRO binary: the PLT GOT is sealed
    // read-only at the end of the load and lazy PLT binding is impossible. Every JUMP_SLOT must
    // therefore be applied *before* relro is sealed, which fixes the order of the whole load.
    let pltgot = f.elf.dynamic().pltgot.expect("DT_PLTGOT");
    let relro = f.elf.relro().expect("PT_GNU_RELRO");
    assert_eq!(f.elf.dynamic().flags & 0x8, 0x8, "DF_BIND_NOW");
    assert!(
        pltgot >= relro.p_vaddr && pltgot < relro.vaddr_end(),
        "DT_PLTGOT {pltgot:#x} is inside PT_GNU_RELRO"
    );
    for r in &plt.relocations {
        assert!(r.r_offset >= pltgot, "JUMP_SLOT at {:#x} is below DT_PLTGOT", r.r_offset);
        assert!(
            r.r_offset >= relro.p_vaddr && r.r_offset < relro.vaddr_end(),
            "JUMP_SLOT at {:#x} is outside PT_GNU_RELRO",
            r.r_offset
        );
    }

    // And after the load they are sealed read-only, holding the null the empty provider produced.
    let Some(f2) = fixture() else { return };
    let object = load(&f2, &LoaderConfig::default());
    let at = object.base + plt.relocations[0].r_offset as usize;
    assert_eq!(
        object.range_at(at).map(|r| r.rest),
        Some(Protection::Read),
        "the PLT GOT is sealed by relro"
    );
    let got = unsafe { f2.space.ptr(at, 8).expect("in the space").cast::<u64>().read_unaligned() };
    assert_eq!(got, 0, "an unresolved JUMP_SLOT holds null");
    object.unload(&f2.space).expect("unload");
}

/// Every relocation target in the library, as unrelocated virtual addresses, ascending.
fn relocation_targets(f: &Fixture) -> Vec<u64> {
    let tables = f.elf.relocations().expect("decode the relocation tables");
    let mut out: Vec<u64> = tables
        .general
        .iter()
        .chain(tables.plt.iter())
        .flat_map(|t| t.relocations.iter().map(|r| r.r_offset))
        .collect();
    out.sort_unstable();
    out
}

/// The first `len`-byte window of a segment's file image that no relocation wrote into.
///
/// Needed because comparing mapped memory against the file is only meaningful where relocation did
/// not change it — and 560,410 of this library's relocations land in one 5.2 MB segment.
fn untouched_window(targets: &[u64], seg: &omni_elf::Segment, len: u64) -> Option<u64> {
    let mut vaddr = seg.p_vaddr;
    let limit = seg.p_vaddr + seg.p_filesz;
    while vaddr + len <= limit {
        // Any relocation writing 8 bytes and starting in [vaddr - 7, vaddr + len) touches it.
        let lo = vaddr.saturating_sub(7);
        let from = targets.partition_point(|&t| t < lo);
        match targets.get(from).filter(|&&t| t < vaddr + len) {
            None => return Some(vaddr),
            Some(&hit) => vaddr = hit + 8,
        }
    }
    None
}

// -------------------------------------------------------------------------------------------------
// 3. RELRO
// -------------------------------------------------------------------------------------------------

#[test]
fn relro_covers_5205568_bytes_and_is_read_only_afterwards() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = load(&f, &LoaderConfig::default());
    let page = f.space.page_size();
    let relro = object.relro.expect("libroblox.so has PT_GNU_RELRO");

    assert_eq!(relro.memsz, RELRO_BYTES, "PT_GNU_RELRO coverage");
    assert_eq!(relro.vaddr, RELRO_VADDR);
    assert!(relro.sealed);
    // bionic rounds both ends **down**, so the sealed span is the whole pages the segment touches:
    // 448 bytes below p_vaddr belong to the same page and are sealed with it.
    assert_eq!(relro.start, object.base + (RELRO_VADDR as usize & !(page - 1)));
    assert_eq!(relro.end, object.base + (RELRO_VADDR + RELRO_BYTES) as usize);
    assert_eq!(relro.sealed_bytes(), 5_206_016);
    eprintln!(
        "PT_GNU_RELRO: p_memsz {} bytes, sealed {} bytes ({} pages), head {} bytes below p_vaddr",
        relro.memsz,
        relro.sealed_bytes(),
        relro.sealed_bytes() / page,
        RELRO_VADDR as usize - (RELRO_VADDR as usize & !(page - 1)),
    );

    // Every mapped range inside it rests read-only, and the OS agrees.
    for r in &object.ranges {
        if r.start >= relro.start && r.end <= relro.end {
            assert_eq!(r.rest, Protection::Read, "{:#x} is inside relro but rests {}", r.start, r.rest);
        }
    }
    for probe in [relro.start, relro.start + relro.sealed_bytes() / 2, relro.end - page] {
        assert_eq!(
            f.space.region_at(probe).map(|r| r.protection),
            Some(Protection::Read),
            "{probe:#x} must be read-only after sealing"
        );
    }
    // And it is still readable, holding the relocated values.
    let init_slot = object.base + f.elf.dynamic().init_array.expect("DT_INIT_ARRAY").vaddr as usize;
    let got =
        unsafe { f.space.ptr(init_slot, 8).expect("in the space").cast::<u64>().read_unaligned() };
    assert_eq!(got, object.init_array[0], "relro is read-only but still readable");

    object.unload(&f.space).expect("unload");
}

/// Writing to sealed relro must fault.
///
/// There is no way to assert this in process: the point is that the store is not survivable. So it
/// runs in a child process and the parent asserts the child died of `STATUS_ACCESS_VIOLATION`, the
/// same shape `omni-mem` uses for the JIT arena's W^X guarantee.
#[test]
fn writing_to_sealed_relro_faults() {
    let _serial = serial();
    const CHILD: &str = "OMNI_ELF_RELRO_WRITE_CHILD";
    const NO_FAULT: i32 = 7;
    const SKIPPED: i32 = 9;
    const ACCESS_VIOLATION: i32 = 0xC000_0005u32 as i32;

    if std::env::var_os(CHILD).is_some() {
        let Some(f) = fixture() else { std::process::exit(SKIPPED) };
        let object = load(&f, &LoaderConfig::default());
        let relro = object.relro.expect("PT_GNU_RELRO");
        let at = relro.start + relro.sealed_bytes() / 2;
        // SAFETY: none. The store is expected to raise an access violation and kill this process,
        // because the relro region is `PAGE_READONLY` after sealing. `write_volatile` so nothing
        // can optimise it away.
        unsafe { std::ptr::write_volatile(f.space.ptr(at, 1).expect("in the space"), 0x5a) };
        std::process::exit(NO_FAULT);
    }

    let status = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["writing_to_sealed_relro_faults", "--exact", "--nocapture"])
        .env(CHILD, "1")
        .status()
        .expect("run the child");
    let code = status.code();
    if code == Some(SKIPPED) {
        return;
    }
    assert_ne!(
        code,
        Some(NO_FAULT),
        "the child wrote into PT_GNU_RELRO after sealing: relro is not actually read-only"
    );
    // A unix child killed by a fault has no exit code at all: the verdict is the signal. SIGSEGV
    // is 11 on every unix this project names.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(11), "expected the child to die of SIGSEGV, got {status:?}");
        return;
    }
    #[cfg(not(unix))]
    assert_eq!(
        code,
        Some(ACCESS_VIOLATION),
        "expected STATUS_ACCESS_VIOLATION ({ACCESS_VIOLATION:#x}), got {code:?}"
    );
}

// -------------------------------------------------------------------------------------------------
// 4. Imports
// -------------------------------------------------------------------------------------------------

#[test]
fn exactly_565_imports_are_unresolved_and_classified() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = load(&f, &LoaderConfig::default());

    assert_eq!(object.imports.total(), IMPORTS, "libroblox.so imports 565 undefined symbols");
    assert_eq!(
        object.imports.unresolved.len(),
        IMPORTS,
        "with a provider that supplies nothing, all 565 are unresolved"
    );
    assert!(object.imports.resolved.is_empty());
    assert_eq!(object.imports.unresolved_of_kind(SymbolKind::Function), IMPORTS_FUNC);
    assert_eq!(object.imports.unresolved_of_kind(SymbolKind::Object), IMPORTS_OBJECT);
    assert_eq!(object.imports.unresolved_of_kind(SymbolKind::Unspecified), IMPORTS_NOTYPE);
    assert_eq!(IMPORTS_FUNC + IMPORTS_OBJECT + IMPORTS_NOTYPE, IMPORTS);

    // Distinct: no name appears twice.
    let mut names: Vec<&str> = object.imports.unresolved.iter().map(|i| i.name.as_str()).collect();
    names.sort_unstable();
    let distinct = names.len();
    names.dedup();
    assert_eq!(names.len(), distinct, "the 565 imports are distinct names");

    // 23 STT_OBJECT data symbols: D9 names this as a failure mode with no symbol name to guide
    // diagnosis, so they are listed rather than counted.
    let data: Vec<&str> = object
        .imports
        .unresolved
        .iter()
        .filter(|i| i.kind == SymbolKind::Object)
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(data.len(), IMPORTS_OBJECT);
    assert!(
        data.iter().filter(|n| n.starts_with("AMEDIAFORMAT_KEY_")).count() >= 10,
        "the 10 AMEDIAFORMAT_KEY_* data symbols are a subset of the 23"
    );

    // Provider attribution, from DT_VERNEED where the file records one.
    let by_library = object.imports.unresolved_by_library();
    let counts: BTreeMap<&str, usize> = by_library
        .iter()
        .map(|(k, v)| (k.unwrap_or("<unversioned>"), v.len()))
        .collect();
    eprintln!("\n565 unresolved imports by provider library (DT_VERNEED):");
    for (library, count) in &counts {
        eprintln!("  {library:<16} {count:>4}");
    }
    assert_eq!(counts.get("libc.so"), Some(&345));
    assert_eq!(counts.get("libm.so"), Some(&56));
    assert_eq!(counts.get("libdl.so"), Some(&6));
    assert_eq!(counts.get("<unversioned>"), Some(&158));
    assert_eq!(counts.values().sum::<usize>(), IMPORTS);

    // The unversioned 158 come from the Android libraries that ship no version definitions. The
    // file does not record which, so the loader does not claim to know; this is the reporting
    // heuristic, kept in the test where it belongs.
    let mut heuristic: BTreeMap<&str, usize> = BTreeMap::new();
    for import in by_library.get(&None).map(|v| v.as_slice()).unwrap_or_default() {
        *heuristic.entry(guess_library(&import.name)).or_default() += 1;
    }
    eprintln!("the 158 unversioned imports, attributed by name prefix (heuristic, not from the file):");
    for (library, count) in &heuristic {
        eprintln!("  {library:<24} {count:>4}");
    }
    assert_eq!(heuristic.values().sum::<usize>(), 158);

    // Every import kind, broken out per library, for the report.
    eprintln!("per-library STT_FUNC / STT_OBJECT / STT_NOTYPE split:");
    for (library, imports) in &by_library {
        let f_ = imports.iter().filter(|i| i.kind == SymbolKind::Function).count();
        let o = imports.iter().filter(|i| i.kind == SymbolKind::Object).count();
        let n = imports.iter().filter(|i| i.kind == SymbolKind::Unspecified).count();
        let weak = imports.iter().filter(|i| i.weak).count();
        eprintln!(
            "  {:<16} func {f_:>4}  object {o:>3}  notype {n:>2}  weak {weak:>3}",
            library.unwrap_or("<unversioned>")
        );
    }

    object.unload(&f.space).expect("unload");
}

/// Name-prefix attribution for the imports `DT_VERNEED` says nothing about. A **heuristic**, used
/// only to make the report readable; the loader never guesses.
fn guess_library(name: &str) -> &'static str {
    match name {
        n if n.starts_with("__android_log") || n.starts_with("android_get") => "liblog.so",
        n if n.starts_with("AMedia") || n.starts_with("AMEDIA") => "libmediandk.so",
        n if n.starts_with("egl") || n.starts_with("EGL") => "libEGL.so",
        n if n.starts_with("gl") => "libGLESv2.so",
        n if n.starts_with("sl") || n.starts_with("SL_") => "libOpenSLES.so",
        n if n.starts_with('A') || n.starts_with("android_") => "libandroid.so",
        _ => "<unattributed>",
    }
}

#[test]
fn an_unresolved_strong_import_can_be_made_fatal() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let config =
        LoaderConfig { unresolved: UnresolvedPolicy::Fail, ..LoaderConfig::default() };
    let err = loader::load(
        &f.space,
        &f.backing,
        &f.elf,
        &ProviderRegistry::empty_provider(),
        &config,
    )
    .expect_err("UnresolvedPolicy::Fail must refuse a library with 565 unsupplied imports");
    assert!(
        matches!(err, omni_elf::LoadError::UnresolvedSymbol { .. }),
        "expected UnresolvedSymbol, got {err}"
    );
    // And the failed load left nothing behind: no mapping, no commit charge.
    let stats = f.space.stats();
    assert_eq!(stats.mapped, 0, "a failed load must not leave a mapping");
    assert_eq!(stats.committed, 0, "a failed load must not leave commit charge");
}

// -------------------------------------------------------------------------------------------------
// 5. init_array
// -------------------------------------------------------------------------------------------------

#[test]
fn exactly_3594_init_array_entries_in_order_and_in_range() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = load(&f, &LoaderConfig::default());

    assert_eq!(object.init_array.len(), INIT_ARRAY_ENTRIES);
    assert_eq!(object.fini_array.len(), FINI_ARRAY_ENTRIES);
    assert!(object.preinit_array.is_empty(), "no DT_PREINIT_ARRAY");

    // Every slot is **zero in the file**; the pointers are produced by R_AARCH64_RELATIVE
    // relocations, so collecting them from the file image yields 3,594 nulls. This is the check
    // that the loader read relocated memory.
    let from_file = f.elf.init_array().expect("DT_INIT_ARRAY");
    assert_eq!(from_file.len(), INIT_ARRAY_ENTRIES);
    assert!(from_file.iter().all(|&p| p == 0), "the file's slots are all zero");
    assert!(
        object.init_array.iter().all(|&p| p != 0),
        "every collected initializer must be a real address"
    );

    // In order, in range, and every one inside the executable segment — these are functions.
    let array = f.elf.dynamic().init_array.expect("DT_INIT_ARRAY");
    for (i, &entry) in object.init_array.iter().enumerate() {
        let at = object.base + array.vaddr as usize + i * 8;
        let in_memory =
            unsafe { f.space.ptr(at, 8).expect("in the space").cast::<u64>().read_unaligned() };
        assert_eq!(in_memory, entry, "init_array[{i}] was collected out of order");
        let address = entry as usize;
        assert!(
            address >= object.start && address < object.end,
            "init_array[{i}] = {entry:#x} is outside the loaded image"
        );
        assert_eq!(
            object.range_at(address).map(|r| r.rest),
            Some(Protection::ReadExecute),
            "init_array[{i}] = {entry:#x} does not point at executable memory"
        );
    }
    eprintln!(
        "3,594 initializers collected; first {:#x}, last {:#x}, all inside [{:#x}, {:#x})",
        object.init_array[0],
        object.init_array[INIT_ARRAY_ENTRIES - 1],
        object.start,
        object.end
    );

    object.unload(&f.space).expect("unload");
}

// -------------------------------------------------------------------------------------------------
// 6. The properties an over-zealous fix would destroy
// -------------------------------------------------------------------------------------------------

/// Sealing too much passes every correctness test above and then crashes the guest.
///
/// A loader that made the whole writable region read-only — the obvious over-correction from
/// "`PT_GNU_RELRO` must be read-only" — would map correctly, relocate correctly, collect the right
/// initializers and satisfy every relro assertion. It would fail the first time guest code stored to
/// a global. So the complement is asserted directly: everything outside relro that should be
/// writable **is** writable, by writing to it.
#[test]
fn data_and_bss_stay_writable_after_the_load() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let object = load(&f, &LoaderConfig::default());
    let relro = object.relro.expect("PT_GNU_RELRO");

    let mut written = Vec::new();
    for r in &object.ranges {
        let inside_relro = r.start >= relro.start && r.end <= relro.end;
        if inside_relro {
            assert_eq!(r.rest, Protection::Read, "{:#x} is inside relro", r.start);
            continue;
        }
        if r.rest != Protection::ReadWrite {
            continue;
        }
        let at = (r.start + r.len() / 2) & !7;
        // SAFETY: the range rests ReadWrite and, with the default eager `.bss` policy, is committed.
        unsafe {
            let p = f.space.ptr(at, 8).expect("in the space").cast::<u64>();
            let before = p.read_unaligned();
            p.write_unaligned(0x0bad_f00d_dead_beef);
            assert_eq!(p.read_unaligned(), 0x0bad_f00d_dead_beef, "{at:#x} is not writable");
            p.write_unaligned(before);
        }
        written.push((r.segment, r.len(), r.anonymous));
    }
    assert!(
        written.len() >= 2,
        "both .data and .bss must remain writable after the load, wrote to {written:?}"
    );
    assert!(written.iter().any(|&(_, _, anon)| anon), "one of them is .bss");
    assert!(written.iter().any(|&(_, _, anon)| !anon), "one of them is file-backed .data");

    // And the whole image is accounted for: every range rests at exactly one of three protections,
    // and nothing is left writable-and-executable or inaccessible.
    for r in &object.ranges {
        assert!(
            matches!(r.rest, Protection::Read | Protection::ReadWrite | Protection::ReadExecute),
            "{:#x} rests {}",
            r.start,
            r.rest
        );
    }
    object.unload(&f.space).expect("unload");
}

/// Every `JUMP_SLOT` is written *before* relro seals the page it lives on.
///
/// This is an ordering constraint, not a placement one, and the placement test above does not cover
/// it. `DT_PLTGOT` is inside `PT_GNU_RELRO` and `DT_FLAGS` carries `DF_BIND_NOW`, so all 534
/// `JUMP_SLOT` targets end up on read-only pages. Today that is safe only because the loader seals
/// relro by *never raising* those pages — the relocation windows raise and restore them, and the
/// final protection pass has nothing left to do. It stops being safe the moment somebody converts
/// sealing into a protect-at-the-end that runs before, or instead of, the relocation pass: the
/// relocations would then be silently lost or refused, and the guest would jump through a GOT full
/// of nulls into nothing.
///
/// So the assertion is made against **content**, twice, with a provider that supplies every import
/// at a unique non-zero address: sealed and unsealed loads must write the same 534 values. If
/// sealing ever prevents or alters a `JUMP_SLOT` store, the two disagree and this fails loudly.
#[test]
fn every_jump_slot_is_written_before_relro_seals_its_page() {
    let _serial = serial();
    let Some(f) = fixture() else { return };
    let tables = f.elf.relocations().expect("decode");
    let plt = tables.plt.as_ref().expect("DT_JMPREL");
    assert_eq!(plt.relocations.len(), PLT_TOTAL);

    let read_slots = |seal: bool| -> (Vec<u64>, Option<Protection>, usize, usize) {
        let space = omni_mem::GuestSpace::new().expect("space");
        let object = loader::load(
            &space,
            &f.backing,
            &f.elf,
            &stub_registry(),
            &LoaderConfig { seal_relro: seal, ..LoaderConfig::default() },
        )
        .expect("load");
        let values = plt
            .relocations
            .iter()
            .map(|r| unsafe {
                space
                    .ptr(object.base + r.r_offset as usize, 8)
                    .expect("in the space")
                    .cast::<u64>()
                    .read_unaligned()
            })
            .collect();
        let protection = object
            .range_at(object.base + plt.relocations[0].r_offset as usize)
            .map(|r| r.rest);
        let applied = object.stats.relocations.applied;
        let base = object.base;
        object.unload(&space).expect("unload");
        space.close().expect("close");
        (values, protection, applied, base)
    };

    let (sealed, sealed_protection, sealed_applied, sealed_base) = read_slots(true);
    let (unsealed, unsealed_protection, unsealed_applied, unsealed_base) = read_slots(false);

    assert_eq!(sealed_protection, Some(Protection::Read), "the PLT GOT is sealed by relro");
    assert_eq!(
        unsealed_protection,
        Some(Protection::ReadWrite),
        "without the seal the same pages rest writable, so the two loads really do differ"
    );
    assert_eq!(sealed_applied, GRAND_TOTAL, "sealing must not cost a single relocation");
    assert_eq!(unsealed_applied, GRAND_TOTAL);
    assert_eq!(sealed.len(), PLT_TOTAL, "534 JUMP_SLOT slots read back from the sealed load");

    // Each load is compared against an expectation computed for **its own** base, from the file and
    // the provider rather than from anything the loader produced. Comparing the two loads' raw
    // values directly does not work and the difference is instructive: 533 of the 534 slots hold a
    // provider address, which is base-independent, but one holds `base + st_value` for a symbol
    // `libroblox.so` both imports and exports, and the two loads land at different bases.
    let mut relative = 0usize;
    for (load, base) in [(&sealed, sealed_base), (&unsealed, unsealed_base)] {
        let expected = expected_values(&f, base);
        for (r, got) in plt.relocations.iter().zip(load) {
            let want = expected[&r.r_offset];
            assert_ne!(want, 0, "every JUMP_SLOT must have a non-zero expectation");
            assert_eq!(
                *got, want,
                "JUMP_SLOT at r_offset {:#x} holds {got:#x}, expected {want:#x} at base {base:#x}",
                r.r_offset
            );
        }
    }
    let bias = (sealed_base as u64).wrapping_sub(unsealed_base as u64);
    for (i, (a, b)) in sealed.iter().zip(&unsealed).enumerate() {
        if a != b {
            // The one self-bound slot, when the two loads did not happen to land at the same base:
            // it must differ by exactly the bias and by nothing else.
            assert_eq!(
                a.wrapping_sub(*b),
                bias,
                "JUMP_SLOT {i} differs between the two loads by something other than the load bias"
            );
            relative += 1;
        }
    }
    // The guest space is closed between the two loads, so the OS may hand back the same base. Which
    // slots differ therefore depends on that, and only the *reason* they differ is asserted: a
    // non-zero bias moves exactly the one base-relative slot, and nothing else ever moves.
    assert_eq!(
        relative,
        usize::from(bias != 0),
        "with a load bias of {bias:#x}, {relative} of the 534 JUMP_SLOTs moved between loads"
    );

    eprintln!(
        "534 JUMP_SLOTs inside sealed PT_GNU_RELRO all hold the value their provider supplied; \
         first {:#x}, and the one base-relative slot tracks the load bias",
        sealed[0]
    );
}
