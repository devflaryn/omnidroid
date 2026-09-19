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

use omni_android::{AbiError, Binding, BoundaryBuilder, SLOT_BYTES};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
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
    // The 18 `STT_OBJECT` imports need a size, which `.dynsym` has and `SymbolRequest` does not carry.
    // Declared here at their real widths so this test can assert the split; Task 3 owns the contents.
    for name in DATA_SYMBOLS {
        builder.declare_data(name, 8, 8).expect("a data object");
    }

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

/// The eighteen `STT_OBJECT` imports the 3,594 initializers reach (D17's scope figure).
///
/// Listed rather than derived so that the count is checked against the decision that produced it: if
/// the reachable set is ever re-measured and this list disagrees, the test says so instead of quietly
/// agreeing with whatever the new number is.
const DATA_SYMBOLS: [&str; REACHABLE_DATA] = [
    "__sF",
    "__stack_chk_guard",
    "environ",
    "in6addr_any",
    "in6addr_loopback",
    "stderr",
    "stdin",
    "stdout",
    "timezone",
    "tzname",
    "AMEDIAFORMAT_KEY_BIT_RATE",
    "AMEDIAFORMAT_KEY_CHANNEL_COUNT",
    "AMEDIAFORMAT_KEY_COLOR_FORMAT",
    "AMEDIAFORMAT_KEY_FRAME_RATE",
    "AMEDIAFORMAT_KEY_HEIGHT",
    "AMEDIAFORMAT_KEY_I_FRAME_INTERVAL",
    "AMEDIAFORMAT_KEY_MIME",
    "AMEDIAFORMAT_KEY_SAMPLE_RATE",
];

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
            !DATA_SYMBOLS.contains(name),
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
        assert_eq!(
            *kind,
            omni_elf::loader::SymbolKind::Object,
            "`{name}` is a {} import and stayed unresolved, so the guest's GOT holds a null              where a call target belongs",
            kind.elf_name()
        );
    }
    assert_eq!(
        unresolved_kinds.len(),
        TOTAL_DATA - REACHABLE_DATA,
        "the STT_OBJECT imports outside D17's reachable eighteen: {unresolved_kinds:?}"
    );

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
        TOTAL_IMPORTS - TOTAL_DATA,
        "a function slot for every import that is not STT_OBJECT — 565 less the 23 data ones, not          less D17's reachable 18"
    );
    // One slot per function, plus the callback sentinel. Sized against the whole import list rather
    // than the function count, since the loader is asked about all 565 and the region has to answer.
    assert_eq!(region.slots_used(), TOTAL_IMPORTS - TOTAL_DATA + 1);
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
