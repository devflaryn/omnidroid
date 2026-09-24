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
/// # There are two ways to make one, and the second has a measured reason
///
/// [`open`](Backing::open), by path, read-only: every library segment and every extraction-cache
/// entry. A `from_mappable` constructor once existed beside it, justified by `omni-apk` opening
/// extraction-cache entries itself and handing over the open handle — an arrangement that did not
/// exist: `omni-apk` produces *paths* to immutable content-addressed files, and the constructor had
/// zero callers. It was removed, with a note that adopting an already-open handle would come back
/// when something had a true reason to.
///
/// [`share`](Backing::share) is that, and the reason is the guest's `MAP_SHARED` writable `mmap`.
/// MEASURED: three engine threads in the first signed-in session died on its refusal, each mapping
/// a cache file it had just created `O_RDWR | O_CREAT | O_TRUNC` and `posix_fallocate`d
/// (`libroblox.so` link `0x2273210`). Such a mapping has to write *that* file, through *that*
/// descriptor's access, so the handle is the guest's own rather than one opened by path here.
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
    guest_named: bool,
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
        Ok(Arc::new(Self::from_file(file, path.display().to_string(), false)))
    }

    /// [`open`](Backing::open), reported under `name` instead of the host path.
    ///
    /// `name` is what the region map -- and so the guest's `/proc/self/maps` and `dl_iterate_phdr`
    /// -- calls the mapping. A host that has placed a file where a device keeps it (a library under
    /// `/data/app/...`) names it by that guest path, because the host path is a Windows path the
    /// guest could never open.
    ///
    /// # Errors
    ///
    /// As [`open`](Backing::open).
    pub fn open_named(
        path: &Path,
        executability: MapExecutability,
        name: &str,
    ) -> MemResult<Arc<Self>> {
        let file = vm::open_file_for_mapping(path, executability)
            .map_err(platform("Backing::open_named", 0, 0))?;
        Ok(Arc::new(Self::from_file(file, name.to_string(), true)))
    }

    /// Adopt a file the guest holds open for reading and writing, so that a
    /// [`Protection::ReadWrite`](crate::Protection::ReadWrite) mapping of it **writes the file** --
    /// Linux's `MAP_SHARED` -- rather than privatising its pages as a mapping of an
    /// [`open`](Backing::open)ed file does.
    ///
    /// `file` is the caller's duplicate of the guest's descriptor and is consumed; `name` is what
    /// the region map reports the mapping as, which for a guest file is its guest path. What the
    /// host does and does not allow while such a mapping exists is on
    /// [`share_file_for_mapping`](omni_platform::vm::share_file_for_mapping).
    ///
    /// # Errors
    ///
    /// [`MemError::Platform`](crate::MemError::Platform) wrapping
    /// [`VmError::EmptyFile`](omni_platform::vm::VmError::EmptyFile) for a zero-length file, or
    /// [`VmError::SectionCreate`](omni_platform::vm::VmError::SectionCreate) for a handle without
    /// write access.
    pub fn share(file: std::fs::File, name: &str) -> MemResult<Arc<Self>> {
        let file = vm::share_file_for_mapping(file, Path::new(name))
            .map_err(platform("Backing::share", 0, 0))?;
        Ok(Arc::new(Self::from_file(file, name.to_string(), true)))
    }

    fn from_file(file: MappableFile, name: String, guest_named: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: BackingId(NEXT.fetch_add(1, Ordering::Relaxed)),
            file,
            name: Arc::from(name.as_str()),
            guest_named,
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

    /// Whether [`name`](Backing::name) is a path the guest gave or would see
    /// ([`open_named`](Backing::open_named), [`share`](Backing::share)) rather than the host path
    /// [`open`](Backing::open) records. **Recorded, not inferred from the name**: on macOS and Linux
    /// a host path starts with `/` exactly as a guest path does.
    #[must_use]
    pub fn is_guest_named(&self) -> bool {
        self.guest_named
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

    /// Whether a writable mapping of this file writes the file: it was made by
    /// [`share`](Backing::share).
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.file.is_shared()
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
            .field("shared", &self.is_shared())
            .finish()
    }
}
