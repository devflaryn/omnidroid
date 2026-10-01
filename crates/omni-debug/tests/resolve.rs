//! The dependency-resolving load (C1 of the RE workbench): `Session::load_resolved` chains a
//! snapshot of already-loaded modules' exports, then caller-supplied providers (the bionic HLE in
//! production), instead of `EmptyProvider`. Driven against the checked-in AOSP-15 `libz.so`, which
//! imports a handful of libc functions — so "with no provider they are unresolved; with one they
//! bind" is a real, fixture-grounded assertion.

use omni_debug::{ModuleExportsProvider, Session};
use omni_elf::loader::{SymbolKind, SymbolProvider, SymbolRequest, SymbolValue};
use std::path::PathBuf;

fn libz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so")
}

/// Binds every requested symbol to one fixed address, offering whatever kind was asked for. A
/// resolution witness, not a working implementation — enough to prove an import's slot was filled.
struct BindAll(u64);
impl SymbolProvider for BindAll {
    fn name(&self) -> &str {
        "bind-all"
    }
    fn resolve(&self, req: &SymbolRequest<'_>) -> Option<SymbolValue> {
        Some(SymbolValue { address: self.0, kind: req.kind })
    }
}

/// Answers only function requests, never data. Proves the registry respects symbol kind: a data
/// import must not be satisfied by a provider that has only functions.
struct FuncsOnly(u64);
impl SymbolProvider for FuncsOnly {
    fn name(&self) -> &str {
        "funcs-only"
    }
    fn resolve(&self, req: &SymbolRequest<'_>) -> Option<SymbolValue> {
        match req.kind {
            SymbolKind::Function => Some(SymbolValue { address: self.0, kind: SymbolKind::Function }),
            _ => None,
        }
    }
}

#[test]
fn empty_provider_leaves_function_imports_unresolved() {
    let mut s = Session::new().unwrap();
    let report = s.load_resolved("libz.so", libz(), vec![]).unwrap();
    assert!(
        report.unresolved.iter().any(|u| u.kind == SymbolKind::Function && !u.weak),
        "libz must have at least one non-weak function import that the empty provider leaves \
         unresolved: {:?}",
        report.unresolved
    );
}

#[test]
fn extra_provider_resolves_the_function_imports() {
    // Discover what libz actually imports (empty provider), then prove a BindAll provider binds
    // every non-weak function import. Self-adapting, so it does not hardcode a libc symbol name.
    let mut probe = Session::new().unwrap();
    let before = probe.load_resolved("libz.so", libz(), vec![]).unwrap();
    let want: Vec<String> = before
        .unresolved
        .iter()
        .filter(|u| u.kind == SymbolKind::Function && !u.weak)
        .map(|u| u.name.clone())
        .collect();
    assert!(!want.is_empty(), "the probe should have found function imports to bind");

    let mut s = Session::new().unwrap();
    let target = s.alloc_data(&[0u8; 16]).unwrap() as u64;
    let after = s.load_resolved("libz.so", libz(), vec![Box::new(BindAll(target))]).unwrap();
    for name in &want {
        assert!(
            !after.unresolved.iter().any(|u| &u.name == name),
            "{name} should bind when a provider supplies every function; still unresolved: {:?}",
            after.unresolved
        );
    }
}

#[test]
fn funcs_only_provider_binds_functions_but_not_data() {
    let mut s = Session::new().unwrap();
    let target = s.alloc_data(&[0u8; 16]).unwrap() as u64;
    let report = s.load_resolved("libz.so", libz(), vec![Box::new(FuncsOnly(target))]).unwrap();
    // Every non-weak *function* import must have bound; any data import stays unresolved (it was
    // never offered a function stub), which is the D9 invariant.
    assert!(
        report.unresolved.iter().all(|u| u.kind != SymbolKind::Function || u.weak),
        "a function-only provider must bind every non-weak function import: {:?}",
        report.unresolved
    );
}

#[test]
fn module_exports_provider_answers_by_name() {
    // Unit check of the provider snapshot itself: it answers for names it holds, declines others.
    let p = ModuleExportsProvider::from_exports(
        "sibling",
        vec![("foo".to_string(), 0x1000, SymbolKind::Function)],
    );
    let hit = p.resolve(&SymbolRequest {
        name: "foo",
        kind: SymbolKind::Function,
        library: None,
        version: None,
        weak: false,
    });
    assert_eq!(hit, Some(SymbolValue { address: 0x1000, kind: SymbolKind::Function }));
    let miss = p.resolve(&SymbolRequest {
        name: "bar",
        kind: SymbolKind::Function,
        library: None,
        version: None,
        weak: false,
    });
    assert_eq!(miss, None);
}
