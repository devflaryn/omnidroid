//! AArch64 ELF64 parsing and the Android `APS2` packed-relocation decoder.
//!
//! # Scope
//!
//! This crate **parses and decodes only**. It never maps memory, never applies a relocation and
//! never binds a symbol to an implementation; those belong to the loader built on top of it. The
//! whole crate works from a `&[u8]` of the file, which is why it needs no OS-specific
//! dependency at all.
//!
//! # Why it exists in this shape
//!
//! `libroblox.so` has no `DT_RELA` and no `DT_RELR`. Every one of its 568,272 non-PLT
//! relocations is inside a 2,100,778-byte `DT_ANDROID_RELA` blob in the `APS2` packed format
//! (`docs/DECISIONS.md` D9). A loader that handles only the standard tables applies literally
//! zero relocations to it, and the failure surfaces much later as crashes in unrelated code.
//! The other ten libraries in the same APK use plain `DT_RELA` + `DT_JMPREL`, so both styles are
//! supported. See [`aps2`] for the format and for the deliberate divergences from bionic.
//!
//! # Hostile input
//!
//! A tampered or corrupt library is an expected case, not an exceptional one (D6). Nothing here
//! panics, and nothing aborts the process: every read is bounds-checked, every allocation sized
//! from file data is fallible, and the packed-relocation count is bounded by the object's own
//! loadable size — a fully-grouped `APS2` group costs zero bytes per relocation, so an
//! unvalidated count lets thirty bytes ask for terabytes. See [`Aps2Limits`].
//!
//! # Entry point
//!
//! ```no_run
//! # fn main() -> Result<(), omni_elf::ElfError> {
//! let bytes: Vec<u8> = std::fs::read("libroblox.so").unwrap();
//! let elf = omni_elf::ElfImage::parse(&bytes)?;
//! let relocs = elf.relocations()?;
//! for table in &relocs.general {
//!     if let Some(packed) = table.packed {
//!         // Byte-exact consumption is the correctness proof for this format.
//!         assert_eq!(packed.bytes_consumed, packed.bytes_total);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

pub mod aps2;
pub mod consts;
pub mod dynamic;
pub mod error;
pub mod header;
pub mod notes;
pub mod reader;
pub mod reloc;
pub mod segment;
pub mod symbols;

pub use crate::aps2::{Aps2Limits, Aps2Summary, PackedFormat, PackedRelocations, Sleb128Decoder};
pub use crate::dynamic::{DynArray, DynEntry, DynTable, Dynamic};
pub use crate::error::{ElfError, Result};
pub use crate::header::{FileHeader, Ident};
pub use crate::notes::{AndroidIdent, GnuProperties, Note};
pub use crate::reader::View;
pub use crate::reloc::{RelocEncoding, Rela, RelocationTable, Relocations};
pub use crate::segment::{Section, Segment, SegmentFlags};
pub use crate::symbols::{GnuHash, StrTab, Sym, SymbolTable, SysvHash};

use crate::consts::*;

/// A parsed, validated AArch64 ELF64 shared object, borrowing the file bytes.
pub struct ElfImage<'a> {
    view: View<'a>,
    header: FileHeader,
    segments: Vec<Segment>,
    sections: Vec<Section>,
    dynamic: Dynamic,
    /// Cached because both the symbol table and the exports walk need it.
    symbol_count: u32,
}

impl<'a> ElfImage<'a> {
    /// Parse and validate.
    ///
    /// Refuses anything that is not a little-endian 64-bit AArch64 `ET_DYN` object, anything
    /// without a `PT_DYNAMIC` segment, and — deliberately — anything with a `PT_TLS` segment:
    /// ELF thread-local storage is not implemented, and silently ignoring the segment would
    /// corrupt every thread-local access instead of failing here (D13).
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let view = View::new(data);
        let header = FileHeader::parse(&view)?;

        let mut segments = Vec::with_capacity(header.e_phnum as usize);
        let phoff = reader::to_usize("e_phoff", header.e_phoff)?;
        for i in 0..header.e_phnum as usize {
            let at = phoff
                .checked_add(i * SIZEOF_PHDR)
                .ok_or(ElfError::OutOfBounds {
                    what: "program header table",
                    offset: phoff,
                    need: i * SIZEOF_PHDR,
                    have: view.len(),
                })?;
            segments.push(Segment::parse(&view, at)?);
        }

        if let Some(tls) = segments.iter().find(|s| s.p_type == PT_TLS) {
            return Err(ElfError::TlsSegmentUnsupported {
                vaddr: tls.p_vaddr,
                filesz: tls.p_filesz,
                memsz: tls.p_memsz,
                align: tls.p_align,
            });
        }

        // Section headers are optional and diagnostics-only; a missing or truncated table must
        // not stop a loadable object from parsing.
        let mut sections = Vec::new();
        if header.e_shnum != 0 {
            if let Ok(shoff) = reader::to_usize("e_shoff", header.e_shoff) {
                for i in 0..header.e_shnum as usize {
                    match shoff
                        .checked_add(i * SIZEOF_SHDR)
                        .map(|at| Section::parse(&view, at))
                    {
                        Some(Ok(s)) => sections.push(s),
                        _ => {
                            sections.clear();
                            break;
                        }
                    }
                }
            }
        }

        let dyn_segs: Vec<&Segment> = segments.iter().filter(|s| s.p_type == PT_DYNAMIC).collect();
        let dyn_seg = match dyn_segs.len() {
            0 => return Err(ElfError::NoDynamicSegment),
            1 => *dyn_segs[0],
            n => return Err(ElfError::MultipleDynamicSegments { count: n }),
        };
        let dyn_view = view.subview(
            "PT_DYNAMIC",
            reader::to_usize("PT_DYNAMIC p_offset", dyn_seg.p_offset)?,
            reader::to_usize("PT_DYNAMIC p_filesz", dyn_seg.p_filesz)?,
        )?;
        let dynamic = Dynamic::parse(&dyn_view, dyn_seg.p_filesz)?;

        let mut image = ElfImage {
            view,
            header,
            segments,
            sections,
            dynamic,
            symbol_count: 0,
        };
        image.symbol_count = image.compute_symbol_count()?;
        tracing::debug!(
            phnum = image.header.e_phnum,
            shnum = image.sections.len(),
            dyn_entries = image.dynamic.entries.len(),
            symbols = image.symbol_count,
            "parsed ELF image"
        );
        Ok(image)
    }

    // -----------------------------------------------------------------------------------------
    // Basics
    // -----------------------------------------------------------------------------------------

    #[inline]
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    #[inline]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Section headers, or an empty slice when the object has none or they were unreadable.
    /// Diagnostics only; nothing in the loader path depends on them.
    #[inline]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    #[inline]
    pub fn dynamic(&self) -> &Dynamic {
        &self.dynamic
    }

    /// The raw file bytes this image borrows.
    #[inline]
    pub fn data(&self) -> &'a [u8] {
        self.view.bytes()
    }

    /// `PT_LOAD` segments, in file order.
    pub fn load_segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.is_load())
    }

    /// The `PT_GNU_RELRO` segment, if any.
    pub fn relro(&self) -> Option<&Segment> {
        self.segments.iter().find(|s| s.p_type == PT_GNU_RELRO)
    }

    /// `PT_NOTE` segments.
    pub fn note_segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.p_type == PT_NOTE)
    }

    /// Total span of the loadable image in virtual address space: `max(vaddr_end) - min(vaddr)`.
    ///
    /// This is the size a loader must reserve. Returns `None` if there is no `PT_LOAD`.
    pub fn load_span(&self) -> Option<(u64, u64)> {
        let mut min = u64::MAX;
        let mut max = 0u64;
        for s in self.load_segments() {
            min = min.min(s.p_vaddr);
            max = max.max(s.vaddr_end());
        }
        (min != u64::MAX).then_some((min, max - min))
    }

    /// Total `p_memsz` across every `PT_LOAD` segment: the bytes this object can ever write to.
    ///
    /// Saturating rather than wrapping, so a malformed header inflates the figure instead of
    /// collapsing it — a bound derived from this must fail safe towards *accepting* input.
    pub fn loadable_size(&self) -> u64 {
        self.load_segments()
            .fold(0u64, |acc, s| acc.saturating_add(s.p_memsz))
    }

    /// The relocation-count ceiling this object's own size justifies.
    ///
    /// See [`Aps2Limits`] for the argument that this cannot reject a real binary. It is exposed
    /// so a caller can inspect the bound it is being held to, and so tests can assert the margin
    /// between the real relocation count and the cap.
    pub fn aps2_limits(&self) -> Aps2Limits {
        Aps2Limits::for_loadable_size(self.loadable_size())
    }

    // -----------------------------------------------------------------------------------------
    // Address translation
    // -----------------------------------------------------------------------------------------

    /// Translate an unrelocated virtual address to a file offset, using the `PT_LOAD` file
    /// images. Addresses in `.bss` have no file offset and correctly fail.
    pub fn vaddr_to_offset(&self, vaddr: u64) -> Option<usize> {
        for s in self.load_segments() {
            if vaddr >= s.p_vaddr && vaddr < s.p_vaddr.saturating_add(s.p_filesz) {
                return usize::try_from(s.p_offset + (vaddr - s.p_vaddr)).ok();
            }
        }
        None
    }

    /// `len` bytes of file image at unrelocated virtual address `vaddr`.
    pub fn slice_at_vaddr(&self, what: error::What, vaddr: u64, len: u64) -> Result<&'a [u8]> {
        if !self
            .load_segments()
            .any(|s| s.file_contains_vaddr(vaddr, len))
        {
            return Err(ElfError::UnmappedVaddrRange {
                vaddr,
                end: vaddr.saturating_add(len),
                len,
            });
        }
        let off = self
            .vaddr_to_offset(vaddr)
            .ok_or(ElfError::UnmappedVaddr(vaddr))?;
        self.view
            .slice(what, off, reader::to_usize(what, len)?)
    }

    fn view_at_vaddr(&self, what: error::What, vaddr: u64, len: u64) -> Result<View<'a>> {
        Ok(View::new(self.slice_at_vaddr(what, vaddr, len)?))
    }

    /// A view starting at `vaddr` and running to the end of its containing `PT_LOAD` file image.
    ///
    /// Needed for the hash tables, whose sizes are not declared anywhere in the dynamic section.
    fn view_to_segment_end(&self, what: error::What, vaddr: u64) -> Result<View<'a>> {
        let seg = self
            .load_segments()
            .find(|s| vaddr >= s.p_vaddr && vaddr < s.p_vaddr.saturating_add(s.p_filesz))
            .ok_or(ElfError::UnmappedVaddr(vaddr))?;
        let len = seg.p_vaddr + seg.p_filesz - vaddr;
        self.view_at_vaddr(what, vaddr, len)
    }

    // -----------------------------------------------------------------------------------------
    // Strings and symbols
    // -----------------------------------------------------------------------------------------

    /// The dynamic string table.
    pub fn strtab(&self) -> Result<StrTab<'a>> {
        let vaddr = self.dynamic.require_strtab()?;
        let size = self.dynamic.require_strsz()?;
        Ok(StrTab::new(self.slice_at_vaddr("DT_STRTAB", vaddr, size)?))
    }

    /// `DT_SONAME`, if present.
    pub fn soname(&self) -> Result<Option<&'a str>> {
        match self.dynamic.soname {
            Some(off) => Ok(Some(self.strtab()?.get(off)?)),
            None => Ok(None),
        }
    }

    /// `DT_NEEDED` names, in link order.
    pub fn needed(&self) -> Result<Vec<&'a str>> {
        let st = self.strtab()?;
        self.dynamic.needed.iter().map(|&o| st.get(o)).collect()
    }

    /// The classic `DT_HASH` table, if the object has one.
    pub fn sysv_hash(&self) -> Result<Option<SysvHash<'a>>> {
        match self.dynamic.hash {
            Some(vaddr) => Ok(Some(SysvHash::parse(
                self.view_to_segment_end("DT_HASH", vaddr)?,
            )?)),
            None => Ok(None),
        }
    }

    /// The `DT_GNU_HASH` table, if the object has one.
    pub fn gnu_hash(&self) -> Result<Option<GnuHash<'a>>> {
        match self.dynamic.gnu_hash {
            Some(vaddr) => Ok(Some(GnuHash::parse(
                self.view_to_segment_end("DT_GNU_HASH", vaddr)?,
            )?)),
            None => Ok(None),
        }
    }

    /// How many entries `.dynsym` has.
    ///
    /// The dynamic section never states this. `DT_HASH`'s `nchain` is it by construction; when
    /// only `DT_GNU_HASH` exists — the case for `libroblox.so` — it has to be derived by
    /// walking the hash chains. Section headers would also answer it, but a runtime loader does
    /// not have them, so they are not used here either.
    #[inline]
    pub fn symbol_count(&self) -> u32 {
        self.symbol_count
    }

    fn compute_symbol_count(&self) -> Result<u32> {
        if let Some(h) = self.sysv_hash()? {
            return Ok(h.symbol_count());
        }
        if let Some(g) = self.gnu_hash()? {
            return g.derive_symbol_count();
        }
        Err(ElfError::NoSymbolCountSource)
    }

    /// The dynamic symbol table.
    pub fn symbols(&self) -> Result<SymbolTable<'a>> {
        let vaddr = self.dynamic.require_symtab()?;
        if let Some(syment) = self.dynamic.syment {
            if syment != SIZEOF_SYM as u64 {
                return Err(ElfError::BadEntrySize {
                    what: "DT_SYMENT",
                    actual: syment,
                    expected: SIZEOF_SYM as u64,
                });
            }
        }
        let bytes = self.symbol_count as u64 * SIZEOF_SYM as u64;
        Ok(SymbolTable::new(
            self.view_at_vaddr("DT_SYMTAB", vaddr, bytes)?,
            self.symbol_count,
        ))
    }

    /// Look a name up through `DT_GNU_HASH`, falling back to `DT_HASH`.
    pub fn lookup(&self, name: &str) -> Result<Option<u32>> {
        let symtab = self.symbols()?;
        let strtab = self.strtab()?;
        if let Some(g) = self.gnu_hash()? {
            return g.lookup(name.as_bytes(), &symtab, &strtab);
        }
        if let Some(h) = self.sysv_hash()? {
            return h.lookup(name.as_bytes(), &symtab, &strtab);
        }
        Ok(None)
    }

    /// Look a name up specifically through `DT_HASH`, for cross-checking the two tables.
    pub fn lookup_sysv(&self, name: &str) -> Result<Option<u32>> {
        match self.sysv_hash()? {
            Some(h) => h.lookup(name.as_bytes(), &self.symbols()?, &self.strtab()?),
            None => Ok(None),
        }
    }

    /// Look a name up specifically through `DT_GNU_HASH`.
    pub fn lookup_gnu(&self, name: &str) -> Result<Option<u32>> {
        match self.gnu_hash()? {
            Some(g) => g.lookup(name.as_bytes(), &self.symbols()?, &self.strtab()?),
            None => Ok(None),
        }
    }

    /// Undefined symbols: the object's imports.
    ///
    /// `STT_FUNC` and `STT_OBJECT` are both reported with their type intact. Conflating them is
    /// a real failure mode here: ten of `libroblox.so`'s imports are *data* objects, and binding
    /// one to a function stub produces a crash that names no symbol (D9).
    pub fn undefined_symbols(&self) -> Result<Vec<SymbolRef<'a>>> {
        let symtab = self.symbols()?;
        let strtab = self.strtab()?;
        let mut out = Vec::new();
        for index in 0..symtab.len() {
            let sym = symtab.get(index)?;
            if sym.is_undefined() && sym.st_name != 0 {
                out.push(SymbolRef {
                    index,
                    name: strtab.get(sym.st_name as u64)?,
                    sym,
                });
            }
        }
        Ok(out)
    }

    /// Defined, externally visible symbols: the object's exports.
    pub fn exported_symbols(&self) -> Result<Vec<SymbolRef<'a>>> {
        let symtab = self.symbols()?;
        let strtab = self.strtab()?;
        let mut out = Vec::new();
        for index in 0..symtab.len() {
            let sym = symtab.get(index)?;
            if sym.is_exported() {
                out.push(SymbolRef {
                    index,
                    name: strtab.get(sym.st_name as u64)?,
                    sym,
                });
            }
        }
        Ok(out)
    }

    /// Refuse the object if any symbol is `STT_TLS`, for the same reason `PT_TLS` is refused.
    ///
    /// Not called from [`Self::parse`]: it needs the symbol table, which needs a hash table, and
    /// a caller that only wants program headers should not be made to pay for that.
    pub fn reject_tls_symbols(&self) -> Result<()> {
        let symtab = self.symbols()?;
        let strtab = self.strtab()?;
        for index in 0..symtab.len() {
            let sym = symtab.get(index)?;
            if sym.is_tls() {
                return Err(ElfError::TlsSymbolUnsupported {
                    name: strtab.get(sym.st_name as u64).unwrap_or("<unnamed>").to_owned(),
                    index,
                });
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // init / fini arrays
    // -----------------------------------------------------------------------------------------

    /// The unrelocated function pointers in `DT_INIT_ARRAY`.
    ///
    /// `libroblox.so` has 3,594 of these and every one must run before `JNI_OnLoad` (D9).
    pub fn init_array(&self) -> Result<Vec<u64>> {
        self.pointer_array("DT_INIT_ARRAY", self.dynamic.init_array)
    }

    /// The unrelocated function pointers in `DT_FINI_ARRAY`.
    pub fn fini_array(&self) -> Result<Vec<u64>> {
        self.pointer_array("DT_FINI_ARRAY", self.dynamic.fini_array)
    }

    /// The unrelocated function pointers in `DT_PREINIT_ARRAY`.
    pub fn preinit_array(&self) -> Result<Vec<u64>> {
        self.pointer_array("DT_PREINIT_ARRAY", self.dynamic.preinit_array)
    }

    fn pointer_array(&self, what: error::What, array: Option<DynArray>) -> Result<Vec<u64>> {
        let Some(a) = array else { return Ok(Vec::new()) };
        if a.size % SIZEOF_PTR as u64 != 0 {
            return Err(ElfError::UnalignedTableSize {
                what,
                size: a.size,
                entsize: SIZEOF_PTR as u64,
            });
        }
        let view = self.view_at_vaddr(what, a.vaddr, a.size)?;
        (0..a.entry_count() as usize)
            .map(|i| view.u64(what, i * SIZEOF_PTR))
            .collect()
    }

    // -----------------------------------------------------------------------------------------
    // Relocations
    // -----------------------------------------------------------------------------------------

    /// Decode every relocation table the object declares.
    ///
    /// General relocations (`DT_RELA`, `DT_REL`, `DT_RELR`, `DT_ANDROID_REL(A)`) and PLT
    /// relocations (`DT_JMPREL`) are kept separate, because they *are* separate in the file.
    pub fn relocations(&self) -> Result<Relocations> {
        let mut out = Relocations::default();

        if let Some(t) = self.dynamic.android_rela {
            out.general.push(self.decode_packed(
                "DT_ANDROID_RELA",
                RelocEncoding::AndroidPackedRela,
                t,
                PackedFormat::Rela,
            )?);
        }
        if let Some(t) = self.dynamic.android_rel {
            out.general.push(self.decode_packed(
                "DT_ANDROID_REL",
                RelocEncoding::AndroidPackedRel,
                t,
                PackedFormat::Rel,
            )?);
        }
        if let Some(t) = self.dynamic.rela {
            let view = self.view_at_vaddr("DT_RELA", t.vaddr, t.size)?;
            out.general.push(RelocationTable {
                tag: "DT_RELA",
                encoding: RelocEncoding::Rela,
                vaddr: t.vaddr,
                size: t.size,
                relocations: reloc::parse_rela_table(&view, t.size, t.entsize)?,
                packed: None,
            });
        }
        if let Some(t) = self.dynamic.rel {
            let view = self.view_at_vaddr("DT_REL", t.vaddr, t.size)?;
            out.general.push(RelocationTable {
                tag: "DT_REL",
                encoding: RelocEncoding::Rel,
                vaddr: t.vaddr,
                size: t.size,
                relocations: reloc::parse_rel_table(&view, t.size, t.entsize)?,
                packed: None,
            });
        }
        if let Some(t) = self.dynamic.relr {
            let view = self.view_at_vaddr("DT_RELR", t.vaddr, t.size)?;
            out.general.push(RelocationTable {
                tag: "DT_RELR",
                encoding: RelocEncoding::Relr,
                vaddr: t.vaddr,
                size: t.size,
                relocations: reloc::parse_relr_table(&view, t.size, t.entsize, self.aps2_limits())?,
                packed: None,
            });
        }

        if let Some(t) = self.dynamic.jmprel {
            // DT_PLTREL says which encoding .rela.plt / .rel.plt uses. Defaulting silently
            // would mean reading 24-byte entries out of a 16-byte table, so an unrecognised
            // value is refused.
            let pltrel = self
                .dynamic
                .pltrel
                .ok_or(ElfError::MissingDynamicTag("DT_PLTREL"))?;
            let view = self.view_at_vaddr("DT_JMPREL", t.vaddr, t.size)?;
            let (encoding, relocations) = match pltrel as i64 {
                DT_RELA => (
                    RelocEncoding::Rela,
                    reloc::parse_rela_table(&view, t.size, None)?,
                ),
                DT_REL => (
                    RelocEncoding::Rel,
                    reloc::parse_rel_table(&view, t.size, None)?,
                ),
                _ => return Err(ElfError::BadPltRel(pltrel)),
            };
            out.plt = Some(RelocationTable {
                tag: "DT_JMPREL",
                encoding,
                vaddr: t.vaddr,
                size: t.size,
                relocations,
                packed: None,
            });
        }

        Ok(out)
    }

    fn decode_packed(
        &self,
        tag: &'static str,
        encoding: RelocEncoding,
        t: DynTable,
        format: PackedFormat,
    ) -> Result<RelocationTable> {
        let blob = self.slice_at_vaddr(tag, t.vaddr, t.size)?;
        let decoded = aps2::decode(blob, format, self.aps2_limits())?;
        tracing::debug!(
            tag,
            relocations = decoded.relocations.len(),
            bytes_consumed = decoded.summary.bytes_consumed,
            bytes_total = decoded.summary.bytes_total,
            groups = decoded.summary.group_count,
            "decoded packed relocations"
        );
        Ok(RelocationTable {
            tag,
            encoding,
            vaddr: t.vaddr,
            size: t.size,
            relocations: decoded.relocations,
            packed: Some(decoded.summary),
        })
    }

    /// Decode the object's `APS2` blob without materialising the relocations.
    ///
    /// Returns `Ok(None)` when the object has neither Android packed-relocation tag.
    pub fn decode_packed_with<F>(&self, sink: F) -> Result<Option<Aps2Summary>>
    where
        F: FnMut(Rela) -> Result<()>,
    {
        let (tag, table, format) = if let Some(t) = self.dynamic.android_rela {
            ("DT_ANDROID_RELA", t, PackedFormat::Rela)
        } else if let Some(t) = self.dynamic.android_rel {
            ("DT_ANDROID_REL", t, PackedFormat::Rel)
        } else {
            return Ok(None);
        };
        let blob = self.slice_at_vaddr(tag, table.vaddr, table.size)?;
        Ok(Some(aps2::decode_with(blob, format, self.aps2_limits(), sink)?))
    }

    // -----------------------------------------------------------------------------------------
    // Notes
    // -----------------------------------------------------------------------------------------

    /// Every note in every `PT_NOTE` segment.
    pub fn notes(&self) -> Result<Vec<Note<'a>>> {
        let mut out = Vec::new();
        for seg in self.note_segments() {
            let view = self.view.subview(
                "PT_NOTE",
                reader::to_usize("PT_NOTE p_offset", seg.p_offset)?,
                reader::to_usize("PT_NOTE p_filesz", seg.p_filesz)?,
            )?;
            out.extend(notes::parse_notes(&view)?);
        }
        Ok(out)
    }

    /// `.note.android.ident`, if present: the NDK that built this object.
    pub fn android_ident(&self) -> Result<Option<AndroidIdent>> {
        for note in self.notes()? {
            if note.name == b"Android" && note.n_type == 1 {
                return Ok(Some(AndroidIdent::parse(note.desc)?));
            }
        }
        Ok(None)
    }

    /// `.note.gnu.property`, if present: BTI / PAC / GCS feature bits.
    pub fn gnu_properties(&self) -> Result<Option<GnuProperties>> {
        for note in self.notes()? {
            if note.name == b"GNU" && note.n_type == NT_GNU_PROPERTY_TYPE_0 {
                return Ok(Some(GnuProperties::parse(note.desc)?));
            }
        }
        Ok(None)
    }

    /// The `NT_GNU_BUILD_ID` bytes, if present.
    pub fn build_id(&self) -> Result<Option<&'a [u8]>> {
        for note in self.notes()? {
            if note.name == b"GNU" && note.n_type == NT_GNU_BUILD_ID {
                return Ok(Some(note.desc));
            }
        }
        Ok(None)
    }
}

impl core::fmt::Debug for ElfImage<'_> {
    /// Deliberately terse: the interesting contents are behind accessors, and a derived `Debug`
    /// on a 109 MB image would print the whole file.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ElfImage")
            .field("bytes", &self.view.len())
            .field("e_type", &self.header.e_type)
            .field("e_machine", &self.header.e_machine)
            .field("segments", &self.segments.len())
            .field("sections", &self.sections.len())
            .field("dynamic_entries", &self.dynamic.entries.len())
            .field("symbol_count", &self.symbol_count)
            .finish()
    }
}

/// A symbol together with its index and resolved name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolRef<'a> {
    pub index: u32,
    pub name: &'a str,
    pub sym: Sym,
}
