//! Typed, diagnostic errors for the virtual-memory seam.
//!
//! Every variant names the operation that failed and the values it failed with (Global
//! Constraint 7). Where the failure corresponds to a documented OS error code, the code is
//! carried and rendered with its symbolic name, because the numbers are the only thing that can
//! be looked up against the measurements in `docs/research/windows-memory-model.md`.

use core::fmt;

/// Result alias for every operation on the virtual-memory seam.
pub type VmResult<T> = Result<T, VmError>;

/// A raw OS error code, rendered with its symbolic name when we know one.
///
/// Kept as a distinct type rather than a bare `u32` so that error messages always print the
/// symbolic name alongside the number. The five codes that matter for Omnidroid's memory work
/// were all observed during the measurements behind D10/D11: 5, 6, 87, 487 and 1132.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OsError(pub u32);

impl OsError {
    /// The raw code as reported by the OS (`GetLastError` on Windows, `errno` on unix).
    #[must_use]
    pub const fn code(self) -> u32 {
        self.0
    }

    /// The symbolic name of the code, if it is one we have a name for.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            2 => "ERROR_FILE_NOT_FOUND",
            3 => "ERROR_PATH_NOT_FOUND",
            5 => "ERROR_ACCESS_DENIED",
            6 => "ERROR_INVALID_HANDLE",
            8 => "ERROR_NOT_ENOUGH_MEMORY",
            32 => "ERROR_SHARING_VIOLATION",
            87 => "ERROR_INVALID_PARAMETER",
            127 => "ERROR_PROC_NOT_FOUND",
            487 => "ERROR_INVALID_ADDRESS",
            1132 => "ERROR_MAPPED_ALIGNMENT",
            1314 => "ERROR_PRIVILEGE_NOT_HELD",
            1455 => "ERROR_COMMITMENT_LIMIT",
            _ => return None,
        })
    }
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "os error {} ({name})", self.0),
            None => write!(f, "os error {}", self.0),
        }
    }
}

impl std::error::Error for OsError {}

/// Everything that can go wrong on the virtual-memory seam.
///
/// There is deliberately no catch-all `Other(String)`: a new failure mode gets a new variant with
/// the values that explain it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmError {
    /// This backend does not implement the operation at all.
    ///
    /// Returned by the Linux and macOS backends, which are structural only. A build for those
    /// targets therefore fails honestly at the first call rather than silently misbehaving.
    #[error(
        "virtual-memory operation `{operation}` is not implemented on {platform}: \
         omni-platform's {platform} backend is structural only and has never been run \
         (see docs/ARCHITECTURE.md \"Portability rule\")"
    )]
    Unsupported {
        /// The seam operation that was called, e.g. `"map_file"`.
        operation: &'static str,
        /// The target the backend was compiled for, e.g. `"linux"`.
        platform: &'static str,
    },

    /// A required OS entry point could not be resolved at runtime.
    ///
    /// `VirtualAlloc2`, `MapViewOfFile3` and `UnmapViewOfFile2` are exported from
    /// `kernelbase.dll` and *not* from `kernel32.dll` (D11), so they are resolved with
    /// `GetProcAddress`. A missing symbol is an error, never a panic.
    #[error(
        "`{symbol}` could not be resolved from `{library}` ({source}); \
         placeholder-based mapping requires Windows 10 1803 or later"
    )]
    MissingSymbol {
        /// The symbol we tried to resolve.
        symbol: &'static str,
        /// The module we tried to resolve it from.
        library: &'static str,
        /// The code from the failed `LoadLibrary`/`GetProcAddress`.
        source: OsError,
    },

    /// A size argument was zero, which is never meaningful.
    #[error("`{operation}` was called with a size of 0 bytes")]
    ZeroSize {
        /// The seam operation that was called.
        operation: &'static str,
    },

    /// An alignment argument was not a power of two.
    #[error("`{operation}` was called with alignment {align}, which is not a power of two")]
    AlignmentNotPowerOfTwo {
        /// The seam operation that was called.
        operation: &'static str,
        /// The rejected alignment.
        align: u64,
    },

    /// A value that must be a multiple of some granularity was not.
    ///
    /// This is the pre-flight form of the Windows `ERROR_MAPPED_ALIGNMENT` (1132) failure: a
    /// sub-page file offset can never be mapped, so it is rejected here with the offending value
    /// instead of being handed to the kernel and coming back as a bare error number.
    #[error(
        "`{operation}`: {what} is {value}, which is not a multiple of {required} \
         (the kernel reports this as {os_equivalent})"
    )]
    Misaligned {
        /// The seam operation that was called.
        operation: &'static str,
        /// Which argument was misaligned, e.g. `"file offset"`.
        what: &'static str,
        /// The offending value.
        value: u64,
        /// The granularity it must be a multiple of.
        required: u64,
        /// The OS error this condition surfaces as if it reaches the kernel.
        os_equivalent: OsError,
    },

    /// `MEM_REPLACE_PLACEHOLDER` needs a placeholder of exactly the requested size (D11).
    ///
    /// Observed as `ERROR_INVALID_ADDRESS` (487): a 64 KB view into a 1 MB placeholder fails.
    /// Split the placeholder to the exact size first, then replace it.
    #[error(
        "`{operation}` at {address:#x} for {size} bytes failed with {source}: \
         replacing a placeholder requires a placeholder of exactly {size} bytes \
         — call split_placeholder to carve an exact-size piece first"
    )]
    PlaceholderNotExactSize {
        /// The seam operation that was called.
        operation: &'static str,
        /// The address that was being replaced.
        address: usize,
        /// The size requested.
        size: usize,
        /// The code the OS returned.
        source: OsError,
    },

    /// A range was not contained in the reservation it was supposed to be inside.
    #[error(
        "`{operation}`: range {address:#x}..{end:#x} is not inside reservation \
         {reservation_base:#x}..{reservation_end:#x}"
    )]
    OutsideReservation {
        /// The seam operation that was called.
        operation: &'static str,
        /// Start of the requested range.
        address: usize,
        /// End of the requested range, exclusive.
        end: usize,
        /// Start of the reservation.
        reservation_base: usize,
        /// End of the reservation, exclusive.
        reservation_end: usize,
    },

    /// The file was not opened in a way that allows an executable mapping (D11).
    ///
    /// Measured: `CreateFileW(GENERIC_READ)` followed by `CreateFileMapping(PAGE_EXECUTE_READ)`
    /// fails with `ERROR_INVALID_HANDLE` (6), and a view of a `PAGE_READONLY` section can never
    /// be raised to `PAGE_EXECUTE_READ` afterwards — `VirtualProtect` fails with
    /// `ERROR_INVALID_PARAMETER` (87). The section protection caps the maximum protection of
    /// every view, for the whole life of the mapping.
    #[error(
        "`{operation}`: {path} was opened non-executable, so it can never be mapped or \
         protected executable; reopen it with MapExecutability::Executable \
         (the section protection caps the maximum protection of every view)"
    )]
    FileNotOpenedExecutable {
        /// The seam operation that was called.
        operation: &'static str,
        /// The file in question.
        path: String,
    },

    /// A file-backed view cannot be created with this protection.
    #[error(
        "`{operation}`: protection {protection} is not a legal protection for a file-backed \
         view of {path}: {reason}"
    )]
    UnsupportedViewProtection {
        /// The seam operation that was called.
        operation: &'static str,
        /// The requested protection.
        protection: super::Protection,
        /// The file in question.
        path: String,
        /// Why it cannot be honoured.
        reason: &'static str,
    },

    /// Opening a file for mapping failed.
    #[error("could not open {path} for mapping ({executability}): {source}")]
    FileOpen {
        /// The path we tried to open.
        path: String,
        /// How it was to be opened.
        executability: super::MapExecutability,
        /// The code the OS returned.
        source: OsError,
    },

    /// Creating the section (file mapping object) failed.
    #[error(
        "could not create a {section_protection} section for {path} of {len} bytes: {source}"
    )]
    SectionCreate {
        /// The path we tried to map.
        path: String,
        /// The length of the file.
        len: u64,
        /// The section protection we asked for, as a symbolic name.
        section_protection: &'static str,
        /// The code the OS returned.
        source: OsError,
    },

    /// A file of zero length cannot be mapped.
    #[error("{path} is 0 bytes long; a zero-length file cannot be mapped")]
    EmptyFile {
        /// The path we tried to map.
        path: String,
    },

    /// The requested view extends past the end of the file.
    #[error(
        "`map_file`: view of {size} bytes at file offset {file_offset} extends to {end}, \
         past the end of {path} which is {len} bytes long"
    )]
    ViewPastEndOfFile {
        /// The file in question.
        path: String,
        /// Requested file offset.
        file_offset: u64,
        /// Requested view size.
        size: usize,
        /// `file_offset + size`.
        end: u64,
        /// The length of the file.
        len: u64,
    },

    /// `unmap` was asked to unmap something other than a whole view.
    ///
    /// Windows unmaps an entire view from its base address; there is no partial unmap. Silently
    /// unmapping more than the caller asked for would corrupt a neighbouring mapping, so this is
    /// refused.
    #[error(
        "`unmap`: {address:#x} is not the base of a mapped view (the view containing it starts \
         at {view_base:#x}); Windows cannot partially unmap a view"
    )]
    NotViewBase {
        /// The address the caller passed.
        address: usize,
        /// The base of the view that actually contains it.
        view_base: usize,
    },

    /// A plain OS failure with the operation and arguments that produced it.
    #[error("`{operation}` at {address:#x} for {size} bytes failed: {source}")]
    Os {
        /// The seam operation that was called.
        operation: &'static str,
        /// The address involved, or 0 where there is none.
        address: usize,
        /// The size involved, or 0 where there is none.
        size: usize,
        /// The code the OS returned.
        source: OsError,
    },
}

impl VmError {
    /// The OS error code behind this failure, if there is one.
    ///
    /// Lets callers and tests assert on the measured code (487, 1132, …) without matching on
    /// every variant shape.
    #[must_use]
    pub fn os_error(&self) -> Option<OsError> {
        match self {
            VmError::MissingSymbol { source, .. }
            | VmError::PlaceholderNotExactSize { source, .. }
            | VmError::FileOpen { source, .. }
            | VmError::SectionCreate { source, .. }
            | VmError::Os { source, .. } => Some(*source),
            VmError::Misaligned { os_equivalent, .. } => Some(*os_equivalent),
            VmError::Unsupported { .. }
            | VmError::ZeroSize { .. }
            | VmError::AlignmentNotPowerOfTwo { .. }
            | VmError::OutsideReservation { .. }
            | VmError::FileNotOpenedExecutable { .. }
            | VmError::UnsupportedViewProtection { .. }
            | VmError::EmptyFile { .. }
            | VmError::ViewPastEndOfFile { .. }
            | VmError::NotViewBase { .. } => None,
        }
    }

    /// True when this failure means "this backend has no implementation".
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, VmError::Unsupported { .. })
    }
}
