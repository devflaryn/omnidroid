//! File backing for guest mappings.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omni_platform::vm::{self, MapExecutability, MappableFile};

use crate::error::{platform, MemResult};

/// Identity of a file that guest memory is mapped from.
///
/// Small and `Copy`, so a region enumeration can report *which* file a range came from without
/// cloning paths — which is what `/proc/self/maps` synthesis and `dl_iterate_phdr` need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackingId(pub u64);

/// A file that guest memory can be mapped from, with the identity the region map records.
///
/// Held behind an [`Arc`] because one file backs many mappings — every `PT_LOAD` of a library
/// comes from the same extraction-cache entry — and because the guest address space must keep the
/// backing alive for as long as any view of it exists.
///
/// # Executability is decided here, not at map time
///
/// [`open`](Backing::open) takes a [`MapExecutability`], and passing
/// [`MapExecutability::NonExecutable`] means no view of this file can *ever* be executable, on any
/// later call: the section protection caps every view's protection for the life of the mapping
/// (D11). Guest `.text` must be opened, and mapped, executable from the outset.
pub struct Backing {
    id: BackingId,
    file: MappableFile,
    name: Arc<str>,
}

impl Backing {
    /// Open a file so guest memory can be mapped from it.
    ///
    /// # Errors
    ///
    /// [`MemError::Platform`](crate::MemError::Platform) if the file cannot be opened or its
    /// section cannot be created — a missing file, or a file without execute access when
    /// [`MapExecutability::Executable`] was asked for.
    pub fn open(path: &Path, executability: MapExecutability) -> MemResult<Arc<Self>> {
        let file = vm::open_file_for_mapping(path, executability)
            .map_err(platform("Backing::open", 0, 0))?;
        Ok(Arc::new(Self::from_file(file, path.display().to_string())))
    }

    /// Wrap a file that has already been opened for mapping.
    ///
    /// `omni-apk` opens extraction-cache entries itself, and re-opening them here would mean a
    /// second handle on the same immutable file for no reason.
    #[must_use]
    pub fn from_mappable(file: MappableFile, name: impl Into<String>) -> Arc<Self> {
        Arc::new(Self::from_file(file, name.into()))
    }

    fn from_file(file: MappableFile, name: String) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: BackingId(NEXT.fetch_add(1, Ordering::Relaxed)),
            file,
            name: Arc::from(name.as_str()),
        }
    }

    /// The identity recorded in the region map for ranges mapped from this file.
    #[must_use]
    pub fn id(&self) -> BackingId {
        self.id
    }

    /// The name reported for this backing, for `/proc/self/maps` synthesis and diagnostics.
    #[must_use]
    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// Length of the file in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.file.len()
    }

    /// Whether the file is empty. Always false: a zero-length file cannot be opened for mapping.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file.len() == 0
    }

    /// Whether views of this file may be executable.
    #[must_use]
    pub fn executability(&self) -> MapExecutability {
        self.file.executability()
    }

    pub(crate) fn file(&self) -> &MappableFile {
        &self.file
    }
}

impl core::fmt::Debug for Backing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Backing")
            .field("id", &self.id.0)
            .field("name", &self.name)
            .field("len", &self.len())
            .field("executability", &self.executability())
            .finish()
    }
}
