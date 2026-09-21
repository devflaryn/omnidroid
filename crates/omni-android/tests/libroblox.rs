//! **The boundary against the real `libroblox.so`** (Global Constraint 2).
//!
//! Every test above this one uses guest code written by hand, which is the right way to assert where a
//! value is. This one asserts something the hand-written tests cannot: that the loader and the
//! boundary actually fit together — that all **565** of the real library's undefined symbols get a
//! thunk address, that the relocations really are written with those addresses, and that the one thing
//! this milestone's failures will look like is a typed error naming a real symbol.
//!
//! When the APK is absent every test here **skips loudly** rather than passing. A golden run that
//! looked exactly like a real one would be the worst outcome available.
//!
//! ```text
//! cargo test -p omni-android --release --test libroblox
//! ```

#![cfg(target_arch = "x86_64")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use omni_android::bionic::{ABSENT_SYMBOLS, DATA_OBJECTS};
use omni_android::{AbiError, Binding, Bionic, BoundaryBuilder, SLOT_BYTES};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_elf::SegmentFlags;
use omni_mem::{Backing, GuestSpace, MapExecutability};

/// The APK every golden assertion in this project is about.
const APK_NAME: &str = "Roblox-2.738.1397.apk";
const MAIN_LIB: &str = "libroblox.so";

/// Undefined, named symbols in `libroblox.so`'s `.dynsym`. An exact figure (Global Constraint 3).
const TOTAL_IMPORTS: usize = 565;
/// `STT_OBJECT` imports among the 188 the static initializers reach (D17).
const REACHABLE_DATA: usize = 18;
/// Thunk functions among them.
const REACHABLE_FUNCTIONS: usize = 170;
/// `STT_OBJECT` imports in the **whole** of `.dynsym`, reachable or not.
///
/// **Measured here, and it reconciles two figures that looked like they disagreed.** The plan says "23
/// imports are `STT_OBJECT` data symbols"; D17 says 18 of the reachable 188 are. Both are right and
/// they are counting different sets — 23 of 565 across the library, 18 of the 188 the initializers
/// reach. This test is what establishes that rather than leaving it to be rediscovered: it declares
/// exactly D17's 18, and the five `STT_OBJECT` imports left over are the difference.
const TOTAL_DATA: usize = 23;

/// Serializes the 109 MB load, which is otherwise done several times at once for no benefit.
static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate manifest directory has two ancestors")
        .to_path_buf()
}

/// `libroblox.so` in the shared extraction cache, or `None` when the APK is absent.
fn cached_main_lib() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let apk_path = repo_root().join(APK_NAME);
        if !apk_path.is_file() {
            // Straight to the process's stderr: `eprintln!` is captured by libtest and thrown away
            // for a passing test, so a skipped assertion would be indistinguishable from a real one.
            let notice = format!(
                "\nSKIP: the boundary's real-library tests need {APK_NAME}, which is not at {}. \
                 Every assertion against the real libroblox.so was skipped.\n\n",
                apk_path.display()
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), notice.as_bytes());
            return None;
        }
        let apk = omni_apk::Apk::open(&apk_path).expect("the real APK must open");
        let cache = omni_apk::LibraryCache::new(
            repo_root().join("target").join("omni-elf-fixtures").join("extraction-cache"),
        );
        let library = apk
            .native_libraries_for_abi("arm64-v8a")
            .into_iter()
            .find(|l| l.file_name() == MAIN_LIB)
            .expect("the APK must contain libroblox.so");
        let cached = cache.extract(&apk, library.entry()).expect("extract libroblox.so");
        Some(cached.path().to_path_buf())
    })
    .as_deref()
}

/// The real library, loaded with a real thunk boundary as its only symbol provider.
struct Loaded {
    object: LoadedObject,
    boundary: Arc<omni_android::Boundary>,
    _backing: Arc<Backing>,
    _space: Arc<GuestSpace>,
}

fn load() -> Option<Loaded> {
    let path = cached_main_lib()?;
    let bytes = std::fs::read(path).expect("read the cache entry");
    let backing = Backing::open(path, MapExecutability::Executable).expect("open the cache entry");
    let elf = ElfImage::parse(&bytes).expect("parse libroblox.so");
    let space = Arc::new(GuestSpace::new().expect("reserve a guest address space"));

    // Room for every import, not only the 188 the initializers are predicted to reach: D17 records
    // that 188 is a lower bound, with 17,698 unresolvable indirect call sites behind it, so a symbol
    // outside the prediction must get a named slot rather than a null.
    let builder = BoundaryBuilder::new(Arc::clone(&space), TOTAL_IMPORTS, 4096)
        .expect("a thunk region for 565 imports");
    // The 18 `STT_OBJECT` imports need a size, which `.dynsym` has and `SymbolRequest` does not
    // carry. Task 3 phase 2 owns both the sizes and the contents, so they come from there rather
    // than from a second list that could drift away from it.
    for object in DATA_OBJECTS {
        builder.declare_data(object.symbol, object.len, object.align).expect("a data object");
    }
    // The two `__gcov_*` imports are declared **absent**, so that the weak references to them
    // resolve to null exactly as they do on a device. `bionic::absent`'s documentation has the
    // decoded guest instructions; `the_two_gcov_imports_are_weak_null_tested_and_left_unresolved`
    // below is where they are asserted against the real library rather than quoted.
    Bionic::declare_absent_into(&builder).expect("the absent list");

    // The registry takes a provider by value, and `finish` needs the builder back, so the builder is
    // shared and the registry is dropped before it is reclaimed. `declare_*` taking `&self` is what
    // makes that possible — the loader resolves from `&self` while relocating.
    let shared = Arc::new(builder);
    let object = {
        let mut providers = ProviderRegistry::new();
        providers.register(ProviderHandle(Arc::clone(&shared)));
        loader::load(&space, &backing, &elf, &providers, &LoaderConfig::default())
            .expect("libroblox.so must load with a thunk boundary")
    };
    let builder = Arc::try_unwrap(shared)
        .unwrap_or_else(|_| panic!("the registry must have released the builder"));
    Some(Loaded { object, boundary: builder.finish(), _backing: backing, _space: space })
}

/// A `SymbolProvider` that forwards to a shared builder.
struct ProviderHandle(Arc<BoundaryBuilder>);

impl omni_elf::loader::SymbolProvider for ProviderHandle {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn resolve(
        &self,
        request: &omni_elf::loader::SymbolRequest<'_>,
    ) -> Option<omni_elf::loader::SymbolValue> {
        self.0.resolve(request)
    }
}

/// Whether `name` is one of the eighteen the adapter places.
fn is_declared_data(name: &str) -> bool {
    DATA_OBJECTS.iter().any(|object| object.symbol == name)
}

/// The first six sections of the reachable-import list: the 188 the initializers statically reach.
fn reachable_imports() -> std::collections::BTreeSet<String> {
    let path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/research/init-reachable-imports.txt");
    let text = std::fs::read_to_string(path).expect("the reachable-import list");
    let mut out = std::collections::BTreeSet::new();
    let mut section = 0usize;
    for line in text.lines() {
        if line.starts_with("###") {
            section += 1;
            continue;
        }
        let symbol = line.trim();
        if symbol.is_empty() || section == 0 || section > 6 {
            continue;
        }
        out.insert(symbol.to_string());
    }
    assert_eq!(out.len(), 188, "the reachable set's first six sections are 188 symbols");
    out
}

/// **The eighteen are derived from the real library, not copied from a list — and the list they
/// used to be copied from was wrong.**
///
/// D17's *count* of 18 is right. The membership this file carried was not: it named `timezone` and
/// `tzname`, which `init-reachable-imports.txt` puts in its "never referenced from the Tier C
/// closure at all" section, and it omitted `AMEDIAFORMAT_KEY_STRIDE` and `AMEDIAFORMAT_KEY_WIDTH`,
/// which are in the reachable `libmediandk` group. Two wrong and two missing, so the count stayed
/// at eighteen and nothing noticed — the exact shape of error this project has now made five
/// times, and the reason a count is not a specification.
///
/// It was invisible because the assertions around it were about *counts*: `timezone` and `tzname`
/// really are `STT_OBJECT` imports of `libroblox.so`, so declaring them still produced eighteen
/// resolved data symbols and five unresolved ones. This test is the one that cannot be satisfied
/// by the wrong set.
#[test]
fn the_eighteen_data_symbols_are_derived_from_the_real_library_and_not_from_a_list() {
    let _guard = serialized();
    let Some(path) = cached_main_lib() else { return };
    let bytes = std::fs::read(path).expect("read the cache entry");
    let elf = ElfImage::parse(&bytes).expect("parse libroblox.so");
    let reachable = reachable_imports();

    let derived: std::collections::BTreeSet<String> = elf
        .undefined_symbols()
        .expect("the undefined symbols")
        .into_iter()
        .filter(|s| s.sym.is_object() && reachable.contains(s.name))
        .map(|s| s.name.to_string())
        .collect();

    let declared: std::collections::BTreeSet<String> =
        DATA_OBJECTS.iter().map(|o| o.symbol.to_string()).collect();
    assert_eq!(derived, declared, "the adapter's data table must be the derived set exactly");
    assert_eq!(derived.len(), REACHABLE_DATA, "D17's count, re-derived");
    for withdrawn in ["timezone", "tzname"] {
        assert!(!derived.contains(withdrawn), "`{withdrawn}` is not reachable");
    }
    for missed in ["AMEDIAFORMAT_KEY_STRIDE", "AMEDIAFORMAT_KEY_WIDTH"] {
        assert!(derived.contains(missed), "`{missed}` is reachable and was omitted");
    }
}

/// **Every reference to a data import is `R_AARCH64_GLOB_DAT` with a zero addend.**
///
/// `BoundaryBuilder::declare_data` is documented against the worry that "`__sF` is an array of
/// three `FILE`s that the guest reaches as `__sF + addend`". Measured here: it is not, and neither
/// is anything else — each of the eighteen has exactly one relocation and every addend is zero.
/// The size still matters, because `&__sF[2]` is arithmetic guest code does at run time rather
/// than arithmetic the loader does; what changes is the *evidence*, and a documented reason that
/// rests on a wrong measurement is worth correcting even when its conclusion survives.
#[test]
fn every_data_import_is_referenced_with_a_zero_addend() {
    let _guard = serialized();
    let Some(path) = cached_main_lib() else { return };
    let bytes = std::fs::read(path).expect("read the cache entry");
    let elf = ElfImage::parse(&bytes).expect("parse libroblox.so");

    let indices: std::collections::BTreeMap<u32, String> = elf
        .undefined_symbols()
        .expect("undefined symbols")
        .into_iter()
        .filter(|s| is_declared_data(s.name))
        .map(|s| (s.index, s.name.to_string()))
        .collect();
    assert_eq!(indices.len(), REACHABLE_DATA);

    let relocations = elf.relocations().expect("relocations");
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let tables = relocations.general.iter().chain(relocations.plt.iter());
    for table in tables {
        for entry in &table.relocations {
            let Some(name) = indices.get(&entry.r_sym()) else { continue };
            assert_eq!(
                entry.r_addend, 0,
                "`{name}` is referenced as {name} + {}, so its object must be at least that \
                 wide and the sizes in `bionic::DATA_OBJECTS` have to account for it",
                entry.r_addend
            );
            // 1025 is `R_AARCH64_GLOB_DAT`: the loader writes the symbol's address into a GOT
            // slot and the guest loads it from there.
            assert_eq!(entry.r_type(), 1025, "`{name}`");
            *seen.entry(name.as_str()).or_default() += 1;
        }
    }
    assert_eq!(seen.len(), REACHABLE_DATA, "every one is referenced: {seen:?}");
    for (name, count) in &seen {
        assert_eq!(*count, 1, "`{name}` has {count} relocations, not one");
    }
}

/// The loader binds every one of the real library's imports into the thunk region.
///
/// The figure that matters: **zero** unresolved *function* imports. Before this task every one of the
/// 565 was unresolved and written as a null, so every imported call the initializers make would have
/// been a branch to address zero with no symbol attached to it.
#[test]
fn every_import_of_the_real_library_gets_a_named_thunk_address() {
    let _guard = serialized();
    let Some(loaded) = load() else { return };

    assert_eq!(loaded.object.imports.total(), TOTAL_IMPORTS, "the exact figure from M1");

    let unresolved: Vec<&str> =
        loaded.object.imports.unresolved.iter().map(|i| i.name.as_str()).collect();
    // Only data can be unresolved, and only data that was not declared with a size. Every function
    // got a slot.
    for name in &unresolved {
        assert!(
            !is_declared_data(name),
            "`{name}` was declared with a size and should have bound"
        );
    }
    let resolved = loaded.object.imports.resolved.len();
    assert_eq!(resolved + unresolved.len(), TOTAL_IMPORTS);
    assert!(
        resolved >= REACHABLE_FUNCTIONS + REACHABLE_DATA,
        "at least the reachable set must have bound; {resolved} did"
    );
    // **Everything unresolved is data, and only data.** A function that stayed unresolved would be a
    // null in the guest's `GOT`, which is the failure shape this whole task replaces.
    let unresolved_kinds: Vec<_> = loaded
        .object
        .imports
        .unresolved
        .iter()
        .map(|import| (import.name.as_str(), import.kind))
        .collect();
    for (name, kind) in &unresolved_kinds {
        if ABSENT_SYMBOLS.iter().any(|absent| absent.symbol == *name) {
            // The two deliberate ones. A null in *their* GOT slots is the answer, not a gap: the
            // guest tests them for null before calling, and a real device's linker leaves them
            // null too. See `bionic::absent` and the test below.
            continue;
        }
        assert_eq!(
            *kind,
            omni_elf::loader::SymbolKind::Object,
            "`{name}` is a {} import and stayed unresolved, so the guest's GOT holds a null              where a call target belongs",
            kind.elf_name()
        );
    }
    assert_eq!(
        unresolved_kinds.len(),
        TOTAL_DATA - REACHABLE_DATA + ABSENT_SYMBOLS.len(),
        "the STT_OBJECT imports outside D17's reachable eighteen, plus the two deliberately          absent weak imports: {unresolved_kinds:?}"
    );
    for absent in ABSENT_SYMBOLS {
        assert!(
            unresolved.contains(&absent.symbol),
            "`{}` was declared absent and still bound to an address",
            absent.symbol
        );
    }

    // Every address the loader handed out is inside the region, at a slot boundary, and named.
    let region = loaded.boundary.region();
    for import in &loaded.object.imports.resolved {
        let address = import.address as usize;
        assert!(
            region.holds_function(address) || region.holds_data(address),
            "`{}` bound to {address:#x}, which is outside the thunk region",
            import.name
        );
        if region.holds_function(address) {
            assert_eq!(
                region.slot_of(address).map(|(_, offset)| offset),
                Some(0),
                "`{}` bound to {address:#x}, which is not the start of a slot",
                import.name
            );
        }
        let slot = loaded
            .boundary
            .slot_named(&import.name)
            .unwrap_or_else(|| panic!("`{}` has no slot", import.name));
        assert_eq!(slot.address, address, "`{}`", import.name);
    }
}

/// The data and function halves of the reachable set are what D17 says they are.
#[test]
fn the_data_symbols_land_in_the_data_area_and_the_functions_in_the_function_area() {
    let _guard = serialized();
    let Some(loaded) = load() else { return };

    let region = loaded.boundary.region();
    let mut data = 0usize;
    let mut functions = 0usize;
    for slot in loaded.boundary.slots() {
        match slot.binding {
            Binding::Data => {
                assert!(region.holds_data(slot.address), "`{}`", slot.symbol);
                data += 1;
            }
            // Nothing is implemented yet — that is Task 3 — so every function slot is `Unbound`, and
            // that is the honest state rather than a gap: calling one names it.
            Binding::Unbound => {
                assert!(region.holds_function(slot.address), "`{}`", slot.symbol);
                functions += 1;
            }
            other => panic!("`{}` is {other:?}, which nothing in this task binds", slot.symbol),
        }
    }
    assert_eq!(data, REACHABLE_DATA, "the eighteen STT_OBJECT imports D17 counted");
    assert_eq!(
        functions,
        TOTAL_IMPORTS - TOTAL_DATA - ABSENT_SYMBOLS.len(),
        "a function slot for every import that is not STT_OBJECT and is not deliberately absent —          565 less the 23 data ones and the two weak `__gcov_*`, not less D17's reachable 18"
    );
    // One slot per function, plus the callback sentinel. Sized against the whole import list rather
    // than the function count, since the loader is asked about all 565 and the region has to answer.
    assert_eq!(region.slots_used(), TOTAL_IMPORTS - TOTAL_DATA - ABSENT_SYMBOLS.len() + 1);
    assert_eq!((TOTAL_IMPORTS + 1) * SLOT_BYTES, 9_056, "566 slots of 16 bytes: three pages");
}

/// **The error a caller will read 3,000 initializers deep**, produced from the real symbol table.
///
/// The hand-written suites prove the mechanism; this proves it names a symbol somebody will actually
/// recognise, at the address the relocation really holds.
#[test]
fn an_unimplemented_real_import_is_named_precisely_with_its_guest_address() {
    let _guard = serialized();
    let Some(loaded) = load() else { return };

    // Four imports from four different libraries in the reachable set, so the message is checked
    // against names of different shapes rather than one.
    for name in ["pthread_rwlock_init", "__android_log_print", "dl_iterate_phdr", "sincosf"] {
        let slot = loaded
            .boundary
            .slot_named(name)
            .unwrap_or_else(|| panic!("`{name}` is a real import and must have a slot"));
        assert!(matches!(slot.binding, Binding::Unbound), "`{name}` is {:?}", slot.binding);

        let error = AbiError::Unbound { symbol: slot.symbol.clone(), address: slot.address };
        let text = error.to_string();
        assert!(text.contains(name), "{text}");
        assert!(text.contains(&format!("{:#x}", slot.address)), "{text}");
        assert_eq!(error.symbol(), Some(name));
        assert_eq!(error.guest_address(), Some(slot.address));
    }
}

/// The relocations really were written with the thunk addresses.
///
/// Read back out of the loaded, relocated image rather than inferred from the provider's answers: the
/// provider returning an address and the loader writing it into the guest's `GOT` are two different
/// things, and it is the second one the guest's `BL` depends on.
#[test]
fn the_relocated_image_really_holds_the_thunk_addresses() {
    let _guard = serialized();
    let Some(loaded) = load() else { return };

    let space = loaded.boundary.mem().space();
    let region = loaded.boundary.region();
    // Every `.rela` entry that referenced an import wrote its address somewhere in the image. Rather
    // than re-deriving which offsets those are, scan the writable part of the loaded image for
    // pointers into the thunk region and check that there are as many distinct ones as there are
    // resolved imports whose slots differ.
    let mut found: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for region_info in space.mapped_regions() {
        // **Readable and not executable.** The first version of this scan looked only at `ReadWrite`
        // regions and found 22 of 560 — because the `GOT` lives in `PT_GNU_RELRO`, which the loader
        // *seals to read-only* after relocating it. The addresses were there all along; the scan was
        // looking in the only place they could not be.
        if !matches!(
            region_info.protection,
            omni_mem::Protection::Read | omni_mem::Protection::ReadWrite
        ) {
            continue;
        }
        if region.holds_function(region_info.start) || region.holds_data(region_info.start) {
            continue;
        }
        let len = region_info.len;
        let Ok(ptr) = space.ptr(region_info.start, len) else { continue };
        for offset in (0..len.saturating_sub(8)).step_by(8) {
            // SAFETY: `ptr` is `GuestSpace`'s own pointer for this mapped, committed, readable range,
            // and D4's identity mapping makes the guest address a host address. Read unaligned because
            // the scan steps by eight from a page boundary and makes no claim about what is there.
            let value = unsafe { ptr.add(offset).cast::<u64>().read_unaligned() } as usize;
            if region.holds_function(value) || region.holds_data(value) {
                found.insert(value);
            }
        }
    }
    let resolved: std::collections::BTreeSet<usize> = loaded
        .object
        .imports
        .resolved
        .iter()
        .map(|import| import.address as usize)
        .collect();
    let missing: Vec<usize> = resolved.difference(&found).copied().collect();
    assert!(
        missing.len() * 20 < resolved.len(),
        "{} of {} thunk addresses were not found anywhere in the relocated image, which means the \
         loader did not write them: {:?}",
        missing.len(),
        resolved.len(),
        missing.iter().take(5).collect::<Vec<_>>()
    );
    assert!(found.len() > 400, "only {} thunk addresses appear in the image", found.len());
}

/// **The two `__gcov_*` imports are weak, are null-tested by the guest's own code, and are left
/// unresolved — asserted against the real library rather than quoted from a comment.**
///
/// This is the evidence behind `bionic::absent`, and it is the reason those two symbols are the
/// only ones in the reachable 188 that get no thunk address. Four facts, each of which has to
/// hold for "resolve them to nothing" to be right:
///
/// 1. both are **weak** undefined symbols, so a null is a legal resolution rather than a link
///    error;
/// 2. each has a `GLOB_DAT` relocation as well as a `JUMP_SLOT`, so the guest materialises the
///    address rather than only branching through the PLT;
/// 3. the instruction immediately after the `LDR` of that GOT slot is a **`CBZ` on the register
///    the `LDR` wrote** — the guest tests for null before calling;
/// 4. after a real load with the boundary as the only provider, the GOT slot holds **zero**.
///
/// Fact 3 is the one that inverts `Binding::Unbound`'s usual argument. Without it, absence would
/// be a branch to address zero; with it, absence is the path the guest already has.
#[test]
fn the_two_gcov_imports_are_weak_null_tested_and_left_unresolved() {
    let _guard = serialized();
    let Some(path) = cached_main_lib() else { return };
    let bytes = std::fs::read(path).expect("read the cache entry");
    let elf = ElfImage::parse(&bytes).expect("parse libroblox.so");

    // (1) weak, undefined, and `STT_NOTYPE` — which is why the boundary's provider sees them as
    // `SymbolKind::Unspecified` and would otherwise hand them a function slot.
    let indices: std::collections::BTreeMap<u32, &str> = elf
        .undefined_symbols()
        .expect("undefined symbols")
        .into_iter()
        .filter(|s| ABSENT_SYMBOLS.iter().any(|absent| absent.symbol == s.name))
        .map(|s| {
            assert!(s.sym.is_weak(), "`{}` must be a weak reference for a null to be legal", s.name);
            assert!(!s.sym.is_func() && !s.sym.is_object(), "`{}` is STT_NOTYPE", s.name);
            (s.index, s.name)
        })
        .collect();
    assert_eq!(indices.len(), ABSENT_SYMBOLS.len(), "both must be in .dynsym: {indices:?}");

    // (2) one `GLOB_DAT` and one `JUMP_SLOT` each: the address is taken *and* there is a PLT stub
    // for the guarded call to go through.
    let relocations = elf.relocations().expect("relocations");
    let mut got_slots: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    let mut plt_entries: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    for table in relocations.general.iter().chain(relocations.plt.iter()) {
        for entry in &table.relocations {
            let Some(name) = indices.get(&entry.r_sym()) else { continue };
            match entry.r_type() {
                1025 => assert!(
                    got_slots.insert(name, entry.r_offset).is_none(),
                    "`{name}` has two GLOB_DAT relocations"
                ),
                1026 => *plt_entries.entry(name).or_default() += 1,
                other => panic!("`{name}` has an unexpected relocation type {other}"),
            }
        }
    }
    assert_eq!(got_slots.len(), ABSENT_SYMBOLS.len(), "the address of each is taken: {got_slots:?}");
    for (name, count) in &plt_entries {
        assert_eq!(*count, 1, "`{name}` has {count} JUMP_SLOT relocations");
    }

    // (3) the guarding `CBZ`. Scan every executable `PT_LOAD` for an `ADRP` whose page, combined
    // with a following `LDR (unsigned offset, 64-bit)` on the same base register, names one of
    // those GOT slots — the same windowed pairing `tools/init_reach.py` uses — and require the
    // very next instruction to be a `CBZ` on the register the `LDR` wrote.
    const WINDOW: usize = 8;
    let mut guarded: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut materialised: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for segment in elf.load_segments().filter(|s| s.p_flags.contains(SegmentFlags::EXEC)) {
        let start = segment.p_offset as usize;
        let len = segment.p_filesz as usize;
        let code = &bytes[start..start + len];
        let words: Vec<u32> = code
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for (i, word) in words.iter().enumerate() {
            if word & 0x9F00_0000 != 0x9000_0000 {
                continue; // not an ADRP
            }
            let pc = segment.p_vaddr + (i as u64) * 4;
            let page = adrp_page(*word, pc);
            let base = word & 31;
            for step in 1..=WINDOW {
                let Some(next) = words.get(i + step) else { break };
                // `LDR Xt, [Xn, #imm12*8]`
                if next & 0xFFC0_0000 != 0xF940_0000 || (next >> 5) & 31 != base {
                    continue;
                }
                let target = page.wrapping_add(u64::from((next >> 10) & 0xFFF) * 8);
                let Some((name, _)) = got_slots.iter().find(|(_, slot)| **slot == target) else {
                    break;
                };
                materialised.insert(name);
                let destination = next & 31;
                if let Some(after) = words.get(i + step + 1) {
                    if after & 0xFF00_0000 == 0xB400_0000 && after & 31 == destination {
                        guarded.insert(name);
                    }
                }
                break;
            }
        }
    }
    let expected: std::collections::BTreeSet<&str> =
        ABSENT_SYMBOLS.iter().map(|a| a.symbol).collect();
    assert_eq!(materialised, expected, "each GOT slot must be loaded by real guest code");
    assert_eq!(
        guarded, expected,
        "every materialisation of a deliberately-absent symbol must be followed by a CBZ on the \
         loaded register. Without that test in the guest, a null is a branch to address zero and \
         `declare_absent` would be the wrong answer for it"
    );

    // (4) and after a real load, the slot holds zero.
    let Some(loaded) = load() else { return };
    let bias = loaded.object.base;
    for (name, slot) in &got_slots {
        let at = bias.wrapping_add(*slot as usize);
        let word = loaded
            .boundary
            .mem()
            .read_u64(at, omni_android::Blame::new("__gcov", 0, 0))
            .expect("the GOT slot is mapped");
        assert_eq!(
            word, 0,
            "`{name}`'s GOT slot at {at:#x} must hold null, which is what the guest's CBZ tests"
        );
    }
}

/// The page an `ADRP` names, from its two immediate fields.
fn adrp_page(word: u32, pc: u64) -> u64 {
    let immlo = u64::from((word >> 29) & 3);
    let immhi = u64::from((word >> 5) & 0x7FFFF);
    let imm = (immhi << 2) | immlo;
    // 21-bit signed.
    let imm = if imm & (1 << 20) != 0 { imm as i64 - (1 << 21) } else { imm as i64 };
    (pc & !0xFFF).wrapping_add((imm * 4096) as u64)
}
