//! A [`SymbolProvider`] backed by an owned snapshot of already-loaded modules' exports.
//!
//! This is the first link in the resolving load ([`crate::Session::load_resolved`]): when a target
//! library is loaded, its imports resolve first against the libraries already in the session (its
//! co-loaded `DT_NEEDED` siblings from the same APK), and only then against the caller's bionic
//! provider. The snapshot is **owned**, not a borrow of the session, because
//! [`SymbolProvider`] is `'static` — the registry outlives the borrow that built it.

use omni_elf::loader::{SymbolKind, SymbolProvider, SymbolRequest, SymbolValue};
use std::collections::HashMap;

/// Resolves imports against a fixed set of `(name, address, kind)` exports.
pub struct ModuleExportsProvider {
    label: String,
    exports: HashMap<String, (u64, SymbolKind)>,
}

impl ModuleExportsProvider {
    /// Build a provider from a module's exports. `name` is only for diagnostics.
    #[must_use]
    pub fn from_exports(name: &str, exports: Vec<(String, u64, SymbolKind)>) -> Self {
        Self {
            label: name.to_string(),
            exports: exports.into_iter().map(|(n, a, k)| (n, (a, k))).collect(),
        }
    }

    /// Whether this snapshot holds no exports (so it can never resolve anything).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exports.is_empty()
    }
}

impl SymbolProvider for ModuleExportsProvider {
    fn name(&self) -> &str {
        &self.label
    }

    fn resolve(&self, req: &SymbolRequest<'_>) -> Option<SymbolValue> {
        self.exports.get(req.name).map(|&(address, kind)| SymbolValue { address, kind })
    }
}
