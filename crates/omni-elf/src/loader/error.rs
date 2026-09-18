//! Loader errors. Every variant names the offending value (Global Constraint 7), and every one of
//! them is reachable from a tampered library — which, per D6, is the expected case here and not an
//! exceptional one. There is no `panic!`, `unwrap`, `expect`, slice index or arithmetic overflow on
//! any path that consumes file data in this module tree.

use thiserror::Error;

use crate::error::ElfError;

/// Result alias for the loader.
pub type LoadResult<T> = core::result::Result<T, LoadError>;

/// Everything that can go wrong turning a parsed [`ElfImage`](crate::ElfImage) into a loaded one.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoadError {
    /// The object could not be parsed, or a table it declares does not describe real bytes.
    #[error("parsing the object failed: {0}")]
    Elf(#[from] ElfError),

    /// A guest-memory operation failed.
    #[error("guest memory operation failed: {0}")]
    Memory(#[from] omni_mem::MemError),

    /// The chosen load base plus a virtual address does not fit in the address space. Reachable
    /// from a `p_vaddr` close to `u64::MAX`, which [`LoadImage`](crate::LoadImage) accepts as long
    /// as `p_vaddr + p_memsz` itself does not overflow.
    #[error(
        "{what}: load base {base:#x} plus virtual address {vaddr:#x} does not fit in the host \
         address space"
    )]
    AddressOverflow {
        /// Which computation overflowed.
        what: &'static str,
        /// The load base.
        base: usize,
        /// The unrelocated virtual address being biased.
        vaddr: u64,
    },

    /// Two `PT_LOAD` segments claim the same page. Accepted by the ELF parser — overlapping
    /// segments are merely unusual, and [`LoadImage`](crate::LoadImage) counts the union — but a
    /// loader cannot map two different file ranges onto one page, and silently letting the second
    /// win would corrupt the first.
    #[error(
        "PT_LOAD {first} covers [{first_start:#x}, {first_end:#x}) and PT_LOAD {second} covers \
         [{second_start:#x}, {second_end:#x}); their page-rounded ranges overlap and cannot both \
         be mapped"
    )]
    SegmentsOverlap {
        /// Index of the lower segment in the program-header table.
        first: usize,
        /// Page-rounded start of the lower segment.
        first_start: u64,
        /// Page-rounded end of the lower segment.
        first_end: u64,
        /// Index of the upper segment in the program-header table.
        second: usize,
        /// Page-rounded start of the upper segment.
        second_start: u64,
        /// Page-rounded end of the upper segment.
        second_end: u64,
    },

    /// `p_align` is smaller than the host page size, or is not a power of two. A zero `p_align`
    /// means "no alignment constraint" and is accepted; anything positive must be a power of two,
    /// which [`LoadImage::validate`](crate::LoadImage::validate) already enforces, and must be at
    /// least a page for the segment to be mappable at all.
    #[error(
        "PT_LOAD {index} has p_align {align:#x}, which is below the host page size {page_size:#x}; \
         the segment cannot be mapped at its required alignment"
    )]
    AlignBelowPageSize {
        /// Index of the segment in the program-header table.
        index: usize,
        /// The declared `p_align`.
        align: u64,
        /// The host page size.
        page_size: usize,
    },

    /// `p_align` is larger than [`MAX_SEGMENT_ALIGN`](crate::loader::MAX_SEGMENT_ALIGN). The load
    /// base is aligned to the largest `p_align` in the object and the reserved span is measured
    /// from `align_down(base_vaddr, max_align)`, so an absurd `p_align` inflates both — the unsafe
    /// direction, exactly as for `p_memsz`.
    #[error(
        "PT_LOAD {index} has p_align {align:#x}, past the {limit:#x} limit; the load base is \
         aligned to the largest p_align in the object, so this is a forgeable field bounded on \
         purpose"
    )]
    AlignTooLarge {
        /// Index of the segment in the program-header table.
        index: usize,
        /// The declared `p_align`.
        align: u64,
        /// The limit.
        limit: u64,
    },

    /// A `PT_LOAD` that is both writable and executable. There is no such [`Protection`] variant
    /// by design — D12 built the JIT arena precisely so that no page is ever writable and
    /// executable at once — so this is refused rather than quietly losing one of the two bits.
    ///
    /// [`Protection`]: omni_mem::Protection
    #[error(
        "PT_LOAD {index} has p_flags {flags:#x}, which is both writable and executable; Omnidroid \
         has no protection for that by design (D12)"
    )]
    WritableExecutableSegment {
        /// Index of the segment in the program-header table.
        index: usize,
        /// The declared `p_flags`.
        flags: u32,
    },

    /// `p_offset` is smaller than the distance from `p_vaddr` down to its page boundary, so the
    /// biased file offset would be negative.
    #[error(
        "PT_LOAD {index} has p_offset {offset:#x} and p_vaddr {vaddr:#x}; mapping from the page \
         below p_vaddr needs {bias} bytes of file before p_offset, which do not exist"
    )]
    SegmentOffsetBelowPageBias {
        /// Index of the segment in the program-header table.
        index: usize,
        /// `p_offset`.
        offset: u64,
        /// `p_vaddr`.
        vaddr: u64,
        /// How far `p_vaddr` is above its page boundary.
        bias: u64,
    },

    /// `p_vaddr` and `p_offset` are not congruent modulo the host page size, so no page-aligned
    /// file offset maps the segment's bytes to its addresses. The ELF gABI requires congruence
    /// modulo `p_align`, which [`LoadImage::validate`](crate::LoadImage::validate) enforces, but a
    /// `p_align` of 0 or 1 asks for nothing and still has to be mappable.
    #[error(
        "PT_LOAD {index} has p_vaddr {vaddr:#x} and p_offset {offset:#x}, which are not congruent \
         modulo the {page_size:#x}-byte page size, so the segment cannot be mapped from the file"
    )]
    SegmentOffsetNotCongruent {
        /// Index of the segment in the program-header table.
        index: usize,
        /// `p_offset`.
        offset: u64,
        /// `p_vaddr`.
        vaddr: u64,
        /// The host page size.
        page_size: usize,
    },

    /// A relocation's target does not lie inside any mapped part of the image.
    #[error(
        "relocation {ty} ({type_name}) at r_offset {r_offset:#x} targets [{target:#x}, {end:#x}), \
         which is not inside any mapped part of the loaded image"
    )]
    RelocationTargetUnmapped {
        /// `ELF64_R_TYPE`.
        ty: u32,
        /// Its name, or `"unknown"`.
        type_name: &'static str,
        /// The relocation's `r_offset`.
        r_offset: u64,
        /// The biased target address.
        target: usize,
        /// One past the last byte the relocation would write.
        end: usize,
    },

    /// A relocation's target straddles the boundary between two mapped ranges with different
    /// protections, so there is no single range that can be made writable to hold the whole store.
    #[error(
        "relocation {ty} at r_offset {r_offset:#x} writes [{target:#x}, {end:#x}), which crosses \
         the end of its mapped range at {range_end:#x}"
    )]
    RelocationTargetSpansRanges {
        /// `ELF64_R_TYPE`.
        ty: u32,
        /// The relocation's `r_offset`.
        r_offset: u64,
        /// The biased target address.
        target: usize,
        /// One past the last byte the relocation would write.
        end: usize,
        /// End of the mapped range the target starts in.
        range_end: usize,
    },

    /// A relocation type this loader does not implement. Refused rather than skipped: a skipped
    /// relocation leaves a pointer unrelocated, and the crash happens somewhere else entirely.
    #[error(
        "relocation type {ty} ({type_name}) at r_offset {r_offset:#x} is not implemented: {why}"
    )]
    UnsupportedRelocation {
        /// `ELF64_R_TYPE`.
        ty: u32,
        /// Its name, or `"unknown"`.
        type_name: &'static str,
        /// The relocation's `r_offset`.
        r_offset: u64,
        /// Why it is not implemented.
        why: &'static str,
    },

    /// A symbolic relocation whose `r_sym` is zero. `R_AARCH64_JUMP_SLOT`, `GLOB_DAT` and `ABS64`
    /// resolve a symbol by index, and index 0 is the reserved null entry, so there is nothing to
    /// resolve. Writing the addend alone would silently produce a pointer into the low addresses.
    #[error(
        "relocation type {ty} ({type_name}) at r_offset {r_offset:#x} needs a symbol but its \
         r_sym is 0, the reserved null symbol"
    )]
    RelocationWithoutSymbol {
        /// `ELF64_R_TYPE`.
        ty: u32,
        /// Its name.
        type_name: &'static str,
        /// The relocation's `r_offset`.
        r_offset: u64,
    },

    /// A strong undefined symbol was not supplied by any provider, and the configured policy is
    /// [`UnresolvedPolicy::Fail`](crate::loader::UnresolvedPolicy::Fail).
    #[error("unresolved symbol {name:?} ({kind}) required from {library}")]
    UnresolvedSymbol {
        /// The symbol name.
        name: String,
        /// `STT_FUNC`, `STT_OBJECT`, …
        kind: &'static str,
        /// The library `DT_VERNEED` names, or `"<unrecorded>"`.
        library: String,
    },

    /// `STT_GNU_IFUNC` needs the CPU backend to call a resolver before the symbol has a value.
    /// D9 measured that no library in the target APK has one, so this is refused rather than
    /// guessed at.
    #[error("symbol {name:?} is STT_GNU_IFUNC, which is not implemented (D9: no ifuncs in the APK)")]
    IfuncUnsupported {
        /// The symbol name.
        name: String,
    },

    /// `PT_GNU_RELRO` does not lie inside the image, or covers no whole page. A relro segment
    /// pointing outside the mapped ranges would either seal unrelated memory or silently seal
    /// nothing.
    #[error(
        "PT_GNU_RELRO covers [{vaddr:#x}, {end:#x}) ({memsz} bytes), which is not inside a mapped \
         part of the loaded image"
    )]
    RelroOutsideImage {
        /// `p_vaddr` of the relro segment.
        vaddr: u64,
        /// `p_vaddr + p_memsz`.
        end: u64,
        /// `p_memsz`.
        memsz: u64,
    },

    /// The program headers are not covered by any `PT_LOAD` file image, so `dl_iterate_phdr`
    /// could not report an address for them. The in-guest C++ unwinder walks 11.5 MB of
    /// `.eh_frame` through that call (D9), so a loaded object that cannot name its own program
    /// headers is not usable and is refused here rather than at the first thrown exception.
    #[error(
        "the program header table at file offset {phoff:#x} ({phnum} × {phentsize} bytes) is not \
         covered by any PT_LOAD file image, so dl_iterate_phdr could not report it"
    )]
    ProgramHeadersNotMapped {
        /// `e_phoff`.
        phoff: u64,
        /// `e_phnum`.
        phnum: u16,
        /// `e_phentsize`.
        phentsize: u16,
    },

    /// A `DT_INIT_ARRAY`, `DT_FINI_ARRAY` or `DT_PREINIT_ARRAY` slot holds an address outside the
    /// loaded image after relocation. The next milestone calls these, so accepting one would mean
    /// jumping to an attacker-chosen address.
    #[error(
        "{what} entry {index} of {count} holds {value:#x} after relocation, which is outside the \
         loaded image [{base:#x}, {end:#x})"
    )]
    InitArrayEntryOutsideImage {
        /// Which array.
        what: &'static str,
        /// Index within the array.
        index: usize,
        /// How many entries the array has.
        count: usize,
        /// The relocated value.
        value: u64,
        /// Load base.
        base: usize,
        /// One past the last byte of the loaded image.
        end: usize,
    },

    /// A relocation window could not be sized. Only reachable from a zero or misaligned
    /// configuration value, which is checked once at the start of the load rather than per window.
    #[error("relocation window size {size} is invalid: {why}")]
    InvalidWindow {
        /// The configured window size.
        size: usize,
        /// Why it is invalid.
        why: &'static str,
    },
}
