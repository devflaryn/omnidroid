//! The symbol-resolution seam: where `omni-android` will later supply bionic, JNI, EGL and Vulkan.
//!
//! In this task the only registered provider supplies **nothing**, which is the point: reaching M1
//! does not require a single import to bind, it requires all 565 of them to be *enumerated and
//! accounted for*. Leaving the seam as a trait rather than a function means the later layer can
//! register one provider per guest library and keep the per-library attribution that
//! [`crate::version`] recovers from `DT_VERNEED`.

use crate::consts::{STT_FUNC, STT_GNU_IFUNC, STT_OBJECT};

/// What kind of thing an import needs to be bound to.
///
/// Kept distinct from a bare address because conflating the two is a real, named failure mode:
/// **23** of `libroblox.so`'s imports are `STT_OBJECT` *data* symbols, and binding one to a
/// function stub produces a crash that names no symbol (D9). A provider that returns a function
/// where an object was asked for is reported, not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    /// `STT_FUNC`: code.
    Function,
    /// `STT_OBJECT`: data.
    Object,
    /// `STT_NOTYPE` or anything else: the object did not say. Three of `libroblox.so`'s imports
    /// are in this bucket.
    Unspecified,
}

impl SymbolKind {
    /// Classify an `st_info` symbol type.
    #[must_use]
    pub fn from_st_type(st_type: u8) -> Self {
        match st_type {
            STT_FUNC | STT_GNU_IFUNC => SymbolKind::Function,
            STT_OBJECT => SymbolKind::Object,
            _ => SymbolKind::Unspecified,
        }
    }

    /// The ELF name, for reports and error messages.
    #[must_use]
    pub fn elf_name(self) -> &'static str {
        match self {
            SymbolKind::Function => "STT_FUNC",
            SymbolKind::Object => "STT_OBJECT",
            SymbolKind::Unspecified => "STT_NOTYPE",
        }
    }

    /// Whether a provider's answer of `self` is acceptable for a request of `wanted`.
    ///
    /// [`Unspecified`](SymbolKind::Unspecified) matches anything in either direction; a function
    /// offered for an object, or an object offered for a function, does not.
    #[must_use]
    pub fn satisfies(self, wanted: SymbolKind) -> bool {
        self == wanted || self == SymbolKind::Unspecified || wanted == SymbolKind::Unspecified
    }
}

/// One import, as presented to a provider.
#[derive(Debug, Clone, Copy)]
pub struct SymbolRequest<'a> {
    /// The symbol name, exactly as it appears in `.dynstr`.
    pub name: &'a str,
    /// What the importing object says it is.
    pub kind: SymbolKind,
    /// The library `DT_VERNEED` says it comes from, when the file records one. `None` for the 158
    /// unversioned imports; see [`crate::version`] for why that is honest rather than lazy.
    pub library: Option<&'a str>,
    /// The version name `DT_VERNEED` records, e.g. `"LIBC"`.
    pub version: Option<&'a str>,
    /// Whether the importing object marked the reference weak. A weak import that nothing supplies
    /// binds to zero rather than failing.
    pub weak: bool,
}

/// What a provider binds an import to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolValue {
    /// The host address, which is also the guest address (ARCHITECTURE section 1).
    pub address: u64,
    /// What the provider is actually offering, so a data-versus-function mismatch is detectable.
    pub kind: SymbolKind,
}

/// A source of symbol implementations.
///
/// `Send + Sync` because guest threads resolve lazily in later milestones and the registry is
/// shared between them.
pub trait SymbolProvider: Send + Sync {
    /// The name this provider answers to, for diagnostics: `"libc.so"`, `"<none>"`.
    fn name(&self) -> &str;

    /// Bind an import, or return `None` to let the next provider try.
    fn resolve(&self, request: &SymbolRequest<'_>) -> Option<SymbolValue>;
}

/// The provider that supplies nothing.
///
/// Not a placeholder to be deleted: it is how the loader is tested, and it is the only honest
/// provider until `omni-android` exists. With it registered, a load of `libroblox.so` reports
/// exactly 565 unresolved imports and writes a null into every slot that referenced one, which is
/// what the M1 gate asserts.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyProvider;

impl SymbolProvider for EmptyProvider {
    fn name(&self) -> &str {
        "<none>"
    }

    fn resolve(&self, _request: &SymbolRequest<'_>) -> Option<SymbolValue> {
        None
    }
}

/// An ordered set of providers. First match wins, as in a link line.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn SymbolProvider>>,
}

impl ProviderRegistry {
    /// An empty registry: nothing resolves.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry holding only [`EmptyProvider`]. What Task 5 loads with.
    #[must_use]
    pub fn empty_provider() -> Self {
        let mut r = Self::new();
        r.register(EmptyProvider);
        r
    }

    /// Append a provider. Earlier providers win.
    pub fn register(&mut self, provider: impl SymbolProvider + 'static) -> &mut Self {
        self.providers.push(Box::new(provider));
        self
    }

    /// How many providers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no provider is registered at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// The registered provider names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Ask each provider in turn.
    ///
    /// Returns the binding and the name of the provider that supplied it, plus whether the kind it
    /// offered disagrees with the kind that was asked for. A mismatch is *reported*, not silently
    /// accepted, and not silently rejected either: the caller decides, because a function stub
    /// standing in for a data symbol is sometimes exactly what a shim wants to do.
    pub fn resolve(&self, request: &SymbolRequest<'_>) -> Option<Binding<'_>> {
        for provider in &self.providers {
            if let Some(value) = provider.resolve(request) {
                return Some(Binding {
                    value,
                    provider: provider.name(),
                    kind_mismatch: !value.kind.satisfies(request.kind),
                });
            }
        }
        None
    }
}

impl core::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.names())
            .finish()
    }
}

/// A resolved import: what it bound to, and who supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding<'a> {
    /// The address and kind the provider offered.
    pub value: SymbolValue,
    /// Which provider supplied it.
    pub provider: &'a str,
    /// Whether the provider offered a kind the importer did not ask for — a data symbol bound to a
    /// function, or the reverse.
    pub kind_mismatch: bool,
}
