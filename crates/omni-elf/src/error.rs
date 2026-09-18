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

    #[error("no PT_LOAD segment: this object has no loadable image")]
    NoLoadSegments,

    #[error(
        "PT_LOAD {index}: p_offset {offset} + p_filesz {filesz} overflows a 64-bit file offset"
    )]
    SegmentFileRangeOverflow {
        index: usize,
        offset: u64,
        filesz: u64,
    },

    #[error(
        "PT_LOAD {index}: p_offset {offset} + p_filesz {filesz} runs past the end of the \
         {file_len}-byte file"
    )]
    SegmentOutsideFile {
        index: usize,
        offset: u64,
        filesz: u64,
        file_len: u64,
    },

    #[error("PT_LOAD {index}: p_filesz {filesz} exceeds p_memsz {memsz}")]
    SegmentFileSizeExceedsMemSize {
        index: usize,
        filesz: u64,
        memsz: u64,
    },

    #[error(
        "PT_LOAD {index}: p_vaddr {vaddr:#x} + p_memsz {memsz} overflows the 64-bit address space"
    )]
    SegmentMemRangeOverflow {
        index: usize,
        vaddr: u64,
        memsz: u64,
    },

    #[error("PT_LOAD {index}: p_align {align} is not a power of two")]
    SegmentAlignNotPowerOfTwo { index: usize, align: u64 },

    #[error(
        "PT_LOAD {index}: p_vaddr {vaddr:#x} and p_offset {offset:#x} are not congruent modulo \
         p_align {align:#x}, so the segment cannot be mapped from the file"
    )]
    SegmentAlignMismatch {
        index: usize,
        vaddr: u64,
        offset: u64,
        align: u64,
    },

    #[error("the loadable image spans {base_vaddr:#x}..{end_vaddr:#x}, which is not a valid range")]
    ImageSpanOverflow { base_vaddr: u64, end_vaddr: u64 },

    /// Refused rather than clamped: limits derived from the image would otherwise grow with a
    /// forged `p_memsz`. See `image::MAX_IMAGE_SPAN`.
    #[error(
        "the loadable image spans {span} bytes, past the {limit}-byte maximum this runtime can \
         load; a forged p_memsz looks exactly like this"
    )]
    ImageSpanTooLarge { span: u64, limit: u64 },

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

    /// A `DW_EH_PE_*` byte naming a pointer format this runtime does not implement.
    ///
    /// Refused by name rather than guessed at: every encoding is a *different width*, so reading
    /// one as another does not fail, it produces a function map that is plausible and wrong.
    #[error(
        "{what}: DWARF pointer encoding {encoding:#04x} is not implemented; reading it as some \
         other encoding would silently produce wrong addresses"
    )]
    UnsupportedEhFrameEncoding { what: What, encoding: u8 },

    /// A structurally impossible `.eh_frame_hdr` or `.eh_frame` entry.
    #[error("{what} at {offset:#x}: {reason}")]
    MalformedEhFrame {
        what: What,
        offset: u64,
        reason: &'static str,
    },

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

    /// Every relocation in the group consumes at least `min_bytes_each` bytes from the stream, and
    /// the stream does not contain them. Bounds a byte-paying group without needing any external
    /// information, and cannot reject a valid blob: a valid blob really does contain those bytes.
    #[error(
        "packed relocation group {group_index} of size {size} needs at least {needed} more bytes \
         ({min_bytes_each} per relocation) but only {remaining} remain in the blob"
    )]
    Aps2GroupLargerThanStream {
        group_index: usize,
        size: u64,
        min_bytes_each: u64,
        needed: u64,
        remaining: u64,
    },

    /// A group sharing its offset delta, `r_info` and addend whose shared delta is **zero** emits
    /// bit-identical relocations: same target, same symbol, same addend. Every one after the first
    /// is overwritten by its successor, so it is dead, and no encoder emits it. Refusing it is what
    /// stops a fifteen-byte blob describing a billion relocations at one address.
    #[error(
        "packed relocation group {group_index} declares {size} relocations sharing an offset delta \
         of zero, so all but the first would be bit-identical and dead"
    )]
    Aps2DeadGroup { group_index: usize, size: u64 },

    /// A group that spends no bytes per relocation still has to land its relocations inside the
    /// image: `(size - 1) * |delta|` cannot exceed the image span.
    #[error(
        "packed relocation group {group_index} of size {size} strides {stride} bytes at a time, \
         reaching {reach} bytes past its first target, which does not fit in the {image_span}-byte \
         loadable image"
    )]
    Aps2GroupExceedsImage {
        group_index: usize,
        size: u64,
        stride: u64,
        reach: u64,
        image_span: u64,
    },

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

    /// The consumer of [`ElfImage::decode_packed_with`](crate::ElfImage::decode_packed_with)
    /// stopped the decode.
    ///
    /// Carries no detail on purpose. The sink's error type is this crate's, so a consumer with a
    /// richer error of its own — the loader, refusing a relocation target — cannot return it
    /// directly; it returns this and hands back its own error separately. Discarding the real reason
    /// instead would turn "relocation 412,003 targets an unmapped address" into "decode failed".
    #[error("the relocation consumer stopped the decode")]
    RelocationSinkStopped,

    // -----------------------------------------------------------------------------------------
    // Symbol versioning
    // -----------------------------------------------------------------------------------------
    /// `DT_VERNEEDNUM` and `vn_cnt` are file-controlled counts, so they are bounded by the bytes
    /// that actually exist before either is allowed to size an allocation. Same discipline as the
    /// relocation counts, for the same reason.
    #[error(
        "{what} declares {count} records, but at most {max} can fit in the bytes that follow \
         the table"
    )]
    VersionCountExceedsTable { what: What, count: u64, max: u64 },
}
