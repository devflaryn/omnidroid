//! Typed errors. Every variant names the offending value, per Global Constraint 7.

use thiserror::Error;

/// Result alias for this crate.
pub type Result<T> = core::result::Result<T, ElfError>;

/// What kind of structure a bounds check was reading when it failed. Keeping this as a
/// `&'static str` label rather than a nested enum keeps the error messages readable without
/// inventing a taxonomy nobody matches on.
pub type What = &'static str;

#[derive(Debug, Error, PartialEq, Eq, Clone)]
#[non_exhaustive]
pub enum ElfError {
    #[error("{what}: need {need} bytes at offset {offset}, but only {have} bytes are available")]
    OutOfBounds {
        what: What,
        offset: usize,
        need: usize,
        have: usize,
    },

    #[error("not an ELF file: e_ident magic is {0:02x?}, expected [7f, 45, 4c, 46]")]
    BadMagic([u8; 4]),

    #[error("unsupported ELF class {0}: only ELFCLASS64 (2) is supported")]
    UnsupportedClass(u8),

    #[error("unsupported ELF data encoding {0}: only ELFDATA2LSB (1) is supported")]
    UnsupportedEncoding(u8),

    #[error("unsupported e_ident[EI_VERSION] {0}: only EV_CURRENT (1) is supported")]
    UnsupportedIdentVersion(u8),

    #[error("unsupported e_version {0}: only EV_CURRENT (1) is supported")]
    UnsupportedVersion(u32),

    #[error("unsupported e_type {0}: only ET_DYN (3) is supported")]
    UnsupportedObjectType(u16),

    #[error("unsupported e_machine {0}: only EM_AARCH64 (183) is supported")]
    UnsupportedMachine(u16),

    #[error("e_phentsize is {0}, expected 56 for ELF64")]
    BadPhentsize(u16),

    #[error("e_shentsize is {0}, expected 64 for ELF64")]
    BadShentsize(u16),

    /// Refusing rather than ignoring is deliberate; see `docs/DECISIONS.md` D13.
    #[error(
        "PT_TLS segment found (p_vaddr {vaddr:#x}, p_filesz {filesz}, p_memsz {memsz}, \
         p_align {align}): ELF thread-local storage is not implemented, and ignoring the \
         segment would silently corrupt every thread-local access"
    )]
    TlsSegmentUnsupported {
        vaddr: u64,
        filesz: u64,
        memsz: u64,
        align: u64,
    },

    #[error("STT_TLS symbol {name:?} at dynsym index {index}: ELF thread-local storage is not implemented")]
    TlsSymbolUnsupported { name: String, index: u32 },

    #[error("no PT_DYNAMIC segment: this is not a dynamically linked object")]
    NoDynamicSegment,

    #[error("{count} PT_DYNAMIC segments found; exactly one is expected")]
    MultipleDynamicSegments { count: usize },

    #[error("PT_DYNAMIC is not terminated by a DT_NULL entry within its {filesz} bytes")]
    UnterminatedDynamic { filesz: u64 },

    #[error("required dynamic tag {0} is missing")]
    MissingDynamicTag(&'static str),

    #[error("dynamic tag {tag} appears {count} times; at most one is expected")]
    DuplicateDynamicTag { tag: &'static str, count: usize },

    #[error("virtual address {0:#x} is not covered by any PT_LOAD segment's file image")]
    UnmappedVaddr(u64),

    #[error(
        "virtual range {vaddr:#x}..{end:#x} ({len} bytes) is not contained in a single PT_LOAD \
         segment's file image"
    )]
    UnmappedVaddrRange { vaddr: u64, end: u64, len: u64 },

    #[error("string table offset {offset} is past the end of the {strsz}-byte string table")]
    StringOffsetOutOfBounds { offset: u64, strsz: u64 },

    #[error("string at table offset {offset} is not NUL-terminated before the end of the table")]
    UnterminatedString { offset: u64 },

    #[error("string at table offset {offset} is not valid UTF-8")]
    NonUtf8String { offset: u64 },

    #[error("{what} entry size is {actual}, expected {expected}")]
    BadEntrySize {
        what: What,
        actual: u64,
        expected: u64,
    },

    #[error("{what} size {size} is not a multiple of the {entsize}-byte entry size")]
    UnalignedTableSize {
        what: What,
        size: u64,
        entsize: u64,
    },

    #[error("DT_PLTREL is {0}, expected DT_REL (17) or DT_RELA (7)")]
    BadPltRel(u64),

    #[error("symbol index {index} is out of range for a {count}-entry dynamic symbol table")]
    SymbolIndexOutOfBounds { index: u32, count: u32 },

    #[error(
        "cannot determine the dynamic symbol table size: neither DT_HASH nor DT_GNU_HASH is present"
    )]
    NoSymbolCountSource,

    #[error("DT_GNU_HASH is malformed: {0}")]
    BadGnuHash(&'static str),

    #[error("DT_HASH is malformed: {0}")]
    BadSysvHash(&'static str),

    #[error("note at offset {offset} is malformed: {reason}")]
    BadNote { offset: usize, reason: &'static str },

    // -----------------------------------------------------------------------------------------
    // APS2 packed relocations
    // -----------------------------------------------------------------------------------------
    #[error("packed relocation blob has magic {0:02x?}, expected \"APS2\" [41, 50, 53, 32]")]
    Aps2BadMagic([u8; 4]),

    #[error("packed relocation blob is {0} bytes, too short to hold even the 4-byte magic")]
    Aps2TooShort(usize),

    #[error("packed relocation stream ran out of bytes at offset {offset} of {total} while reading a SLEB128 value")]
    Aps2Truncated { offset: usize, total: usize },

    #[error(
        "packed relocation stream declared {declared} relocations but decoded {decoded}; the \
         decoder and the blob disagree, so relocations would be silently dropped"
    )]
    Aps2CountMismatch { declared: u64, decoded: u64 },

    #[error(
        "packed relocation stream has {remaining} unconsumed trailing byte(s): consumed \
         {consumed} of {total}. A correct decoder consumes the blob exactly."
    )]
    Aps2TrailingBytes {
        consumed: usize,
        total: usize,
        remaining: usize,
    },

    #[error(
        "packed relocation group {group_index} declares a size of {size}, which cannot make \
         progress towards the declared relocation count"
    )]
    Aps2BadGroupSize { group_index: usize, size: i64 },

    #[error(
        "packed relocation group {group_index} of size {size} would push the decoded count to \
         {would_be}, past the declared {declared}"
    )]
    Aps2GroupOverrun {
        group_index: usize,
        size: u64,
        would_be: u64,
        declared: u64,
    },

    #[error("packed relocation stream declares a negative relocation count {0}")]
    Aps2NegativeCount(i64),

    #[error(
        "packed relocation group {group_index} sets RELOCATION_GROUP_HAS_ADDEND_FLAG, but this \
         blob came from DT_ANDROID_REL, which has no addends"
    )]
    Aps2AddendInRelFormat { group_index: usize },

    #[error("packed relocation group {group_index} has unknown group flag bits {unknown:#x} set")]
    Aps2UnknownGroupFlags { group_index: usize, unknown: u64 },

    /// A fully-grouped group spends zero bytes per relocation, so a tiny blob can declare an
    /// astronomical count. See `aps2::Aps2Limits` for why the bound is both necessary and safe.
    #[error(
        "packed relocation stream declares {declared} relocations, past the limit of {limit} \
         derived from the object's own loadable size; a blob this small cannot describe that \
         many distinct relocations"
    )]
    Aps2CountExceedsLimit { declared: u64, limit: u64 },

    #[error(
        "{what} would expand to {count} relocations, past the limit of {limit} derived from the \
         object's own loadable size"
    )]
    RelocationCountExceedsLimit {
        what: What,
        count: u64,
        limit: u64,
    },

    /// Returned instead of aborting the process, which is what an infallible `Vec` growth does
    /// when the allocator refuses.
    #[error("could not allocate {bytes} bytes while decoding relocations")]
    AllocationFailed { bytes: usize },
}
