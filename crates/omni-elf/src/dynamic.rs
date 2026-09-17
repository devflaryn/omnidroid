//! The `PT_DYNAMIC` array, and the typed summary a loader actually wants.

use crate::consts::*;
use crate::error::{ElfError, Result};
use crate::reader::View;

/// One raw `Elf64_Dyn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynEntry {
    pub tag: i64,
    pub val: u64,
}

/// A `(vaddr, byte length)` pair, as `DT_*_ARRAY` and `DT_*SZ` come in pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynArray {
    pub vaddr: u64,
    pub size: u64,
}

impl DynArray {
    /// Number of pointer-sized entries. `DT_INIT_ARRAYSZ` is a *byte* count; forgetting to
    /// divide is how a loader ends up calling 3,594 × 8 function pointers.
    #[inline]
    pub fn entry_count(&self) -> u64 {
        self.size / SIZEOF_PTR as u64
    }
}

/// A `(vaddr, byte length)` pair for a relocation table, plus its declared entry size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynTable {
    pub vaddr: u64,
    pub size: u64,
    /// `DT_RELAENT` / `DT_RELENT` / `DT_RELRENT` if the object declared one.
    pub entsize: Option<u64>,
}

/// Typed view of the dynamic section.
///
/// Fields are `Option` where the tag is genuinely optional, so a caller cannot mistake "absent"
/// for "zero" — the difference between `DT_RELA` absent and `DT_RELASZ == 0` is the difference
/// between "packed relocations live elsewhere" and "there are no relocations".
#[derive(Debug, Clone, Default)]
pub struct Dynamic {
    /// Every entry, in file order, including the terminating `DT_NULL`.
    pub entries: Vec<DynEntry>,

    /// `DT_NEEDED`, as string-table offsets, in link order.
    pub needed: Vec<u64>,
    pub soname: Option<u64>,
    pub rpath: Vec<u64>,
    pub runpath: Vec<u64>,

    pub strtab: Option<u64>,
    pub strsz: Option<u64>,
    pub symtab: Option<u64>,
    pub syment: Option<u64>,

    pub hash: Option<u64>,
    pub gnu_hash: Option<u64>,
    pub versym: Option<u64>,
    pub verdef: Option<u64>,
    pub verdefnum: Option<u64>,
    pub verneed: Option<u64>,
    pub verneednum: Option<u64>,

    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub init_array: Option<DynArray>,
    pub fini_array: Option<DynArray>,
    pub preinit_array: Option<DynArray>,

    pub pltgot: Option<u64>,
    /// `DT_JMPREL` plus `DT_PLTRELSZ`. Its entry format is given by `DT_PLTREL`.
    pub jmprel: Option<DynTable>,
    /// `DT_PLTREL`: `DT_RELA` (7) or `DT_REL` (17).
    pub pltrel: Option<u64>,

    pub rela: Option<DynTable>,
    pub rel: Option<DynTable>,
    pub relr: Option<DynTable>,
    /// `DT_ANDROID_RELA` (0x60000011) + `DT_ANDROID_RELASZ` (0x60000012).
    pub android_rela: Option<DynTable>,
    /// `DT_ANDROID_REL` (0x6000000f) + `DT_ANDROID_RELSZ` (0x60000010).
    pub android_rel: Option<DynTable>,

    pub relacount: Option<u64>,
    pub relcount: Option<u64>,

    pub flags: u64,
    pub flags1: u64,
    /// `DT_TEXTREL` present as its own tag (as distinct from the `DF_TEXTREL` flag bit).
    pub textrel_tag: bool,
    /// `DT_BIND_NOW` present as its own tag.
    pub bind_now_tag: bool,
    pub symbolic_tag: bool,

    /// Tags this build does not model, kept so a report can say what was ignored rather than
    /// pretending the file contained nothing else.
    pub unrecognised: Vec<DynEntry>,
}

impl Dynamic {
    /// Parse the dynamic array out of the bytes of the `PT_DYNAMIC` segment.
    ///
    /// The array must be terminated by `DT_NULL` inside `p_filesz`; a run-on array is an error,
    /// because the alternative is walking off into whatever follows.
    pub fn parse(seg: &View<'_>, filesz: u64) -> Result<Self> {
        let mut d = Dynamic::default();
        let mut at = 0usize;
        let mut terminated = false;
        let mut counts = TagCounts::default();

        while at + SIZEOF_DYN <= seg.len() {
            let tag = seg.i64("d_tag", at)?;
            let val = seg.u64("d_un", at + 8)?;
            at += SIZEOF_DYN;
            d.entries.push(DynEntry { tag, val });
            if tag == DT_NULL {
                terminated = true;
                break;
            }
            counts.bump(tag);
            d.apply(tag, val);
        }
        if !terminated {
            return Err(ElfError::UnterminatedDynamic { filesz });
        }

        // Pair up the (vaddr, size[, entsize]) tags now that every entry has been seen. Doing
        // this in a second pass means the order of tags in the file does not matter.
        let get = |t: i64| -> Option<u64> {
            d.entries
                .iter()
                .find(|e| e.tag == t)
                .map(|e| e.val)
        };
        d.init_array = get(DT_INIT_ARRAY).map(|vaddr| DynArray {
            vaddr,
            size: get(DT_INIT_ARRAYSZ).unwrap_or(0),
        });
        d.fini_array = get(DT_FINI_ARRAY).map(|vaddr| DynArray {
            vaddr,
            size: get(DT_FINI_ARRAYSZ).unwrap_or(0),
        });
        d.preinit_array = get(DT_PREINIT_ARRAY).map(|vaddr| DynArray {
            vaddr,
            size: get(DT_PREINIT_ARRAYSZ).unwrap_or(0),
        });
        d.rela = get(DT_RELA).map(|vaddr| DynTable {
            vaddr,
            size: get(DT_RELASZ).unwrap_or(0),
            entsize: get(DT_RELAENT),
        });
        d.rel = get(DT_REL).map(|vaddr| DynTable {
            vaddr,
            size: get(DT_RELSZ).unwrap_or(0),
            entsize: get(DT_RELENT),
        });
        // Accept both the standardised DT_RELR tags and Android's pre-standardisation aliases.
        d.relr = get(DT_RELR)
            .map(|vaddr| DynTable {
                vaddr,
                size: get(DT_RELRSZ).unwrap_or(0),
                entsize: get(DT_RELRENT),
            })
            .or_else(|| {
                get(DT_ANDROID_RELR).map(|vaddr| DynTable {
                    vaddr,
                    size: get(DT_ANDROID_RELRSZ).unwrap_or(0),
                    entsize: get(DT_ANDROID_RELRENT),
                })
            });
        d.android_rela = get(DT_ANDROID_RELA).map(|vaddr| DynTable {
            vaddr,
            size: get(DT_ANDROID_RELASZ).unwrap_or(0),
            entsize: None,
        });
        d.android_rel = get(DT_ANDROID_REL).map(|vaddr| DynTable {
            vaddr,
            size: get(DT_ANDROID_RELSZ).unwrap_or(0),
            entsize: None,
        });
        d.jmprel = get(DT_JMPREL).map(|vaddr| DynTable {
            vaddr,
            size: get(DT_PLTRELSZ).unwrap_or(0),
            entsize: None,
        });

        counts.check()?;
        Ok(d)
    }

    fn apply(&mut self, tag: i64, val: u64) {
        match tag {
            DT_NEEDED => self.needed.push(val),
            DT_SONAME => self.soname = Some(val),
            DT_RPATH => self.rpath.push(val),
            DT_RUNPATH => self.runpath.push(val),
            DT_STRTAB => self.strtab = Some(val),
            DT_STRSZ => self.strsz = Some(val),
            DT_SYMTAB => self.symtab = Some(val),
            DT_SYMENT => self.syment = Some(val),
            DT_HASH => self.hash = Some(val),
            DT_GNU_HASH => self.gnu_hash = Some(val),
            DT_VERSYM => self.versym = Some(val),
            DT_VERDEF => self.verdef = Some(val),
            DT_VERDEFNUM => self.verdefnum = Some(val),
            DT_VERNEED => self.verneed = Some(val),
            DT_VERNEEDNUM => self.verneednum = Some(val),
            DT_INIT => self.init = Some(val),
            DT_FINI => self.fini = Some(val),
            DT_PLTGOT => self.pltgot = Some(val),
            DT_PLTREL => self.pltrel = Some(val),
            DT_RELACOUNT => self.relacount = Some(val),
            DT_RELCOUNT => self.relcount = Some(val),
            DT_FLAGS => self.flags = val,
            DT_FLAGS_1 => self.flags1 = val,
            DT_TEXTREL => self.textrel_tag = true,
            DT_BIND_NOW => self.bind_now_tag = true,
            DT_SYMBOLIC => self.symbolic_tag = true,
            // Handled by the pairing pass, or deliberately size-only companions.
            DT_INIT_ARRAY | DT_INIT_ARRAYSZ | DT_FINI_ARRAY | DT_FINI_ARRAYSZ
            | DT_PREINIT_ARRAY | DT_PREINIT_ARRAYSZ | DT_RELA | DT_RELASZ | DT_RELAENT
            | DT_REL | DT_RELSZ | DT_RELENT | DT_RELR | DT_RELRSZ | DT_RELRENT | DT_JMPREL
            | DT_PLTRELSZ | DT_ANDROID_REL | DT_ANDROID_RELSZ | DT_ANDROID_RELA
            | DT_ANDROID_RELASZ | DT_ANDROID_RELR | DT_ANDROID_RELRSZ | DT_ANDROID_RELRENT
            | DT_DEBUG | DT_SYMTAB_SHNDX => {}
            _ => self.unrecognised.push(DynEntry { tag, val }),
        }
    }

    /// Does the object request text relocations, by either mechanism?
    #[inline]
    pub fn wants_textrel(&self) -> bool {
        self.textrel_tag || self.flags & DF_TEXTREL != 0
    }

    /// Does the object request eager binding, by either mechanism?
    #[inline]
    pub fn wants_bind_now(&self) -> bool {
        self.bind_now_tag || self.flags & DF_BIND_NOW != 0 || self.flags1 & DF_1_NOW != 0
    }

    /// `DT_STRTAB` or a named error.
    pub fn require_strtab(&self) -> Result<u64> {
        self.strtab.ok_or(ElfError::MissingDynamicTag("DT_STRTAB"))
    }

    /// `DT_SYMTAB` or a named error.
    pub fn require_symtab(&self) -> Result<u64> {
        self.symtab.ok_or(ElfError::MissingDynamicTag("DT_SYMTAB"))
    }

    /// `DT_STRSZ` or a named error.
    pub fn require_strsz(&self) -> Result<u64> {
        self.strsz.ok_or(ElfError::MissingDynamicTag("DT_STRSZ"))
    }

    /// Human-readable tag name, for reports and error text.
    pub fn tag_name(tag: i64) -> Option<&'static str> {
        Some(match tag {
            DT_NULL => "DT_NULL",
            DT_NEEDED => "DT_NEEDED",
            DT_PLTRELSZ => "DT_PLTRELSZ",
            DT_PLTGOT => "DT_PLTGOT",
            DT_HASH => "DT_HASH",
            DT_STRTAB => "DT_STRTAB",
            DT_SYMTAB => "DT_SYMTAB",
            DT_RELA => "DT_RELA",
            DT_RELASZ => "DT_RELASZ",
            DT_RELAENT => "DT_RELAENT",
            DT_STRSZ => "DT_STRSZ",
            DT_SYMENT => "DT_SYMENT",
            DT_INIT => "DT_INIT",
            DT_FINI => "DT_FINI",
            DT_SONAME => "DT_SONAME",
            DT_RPATH => "DT_RPATH",
            DT_SYMBOLIC => "DT_SYMBOLIC",
            DT_REL => "DT_REL",
            DT_RELSZ => "DT_RELSZ",
            DT_RELENT => "DT_RELENT",
            DT_PLTREL => "DT_PLTREL",
            DT_DEBUG => "DT_DEBUG",
            DT_TEXTREL => "DT_TEXTREL",
            DT_JMPREL => "DT_JMPREL",
            DT_BIND_NOW => "DT_BIND_NOW",
            DT_INIT_ARRAY => "DT_INIT_ARRAY",
            DT_FINI_ARRAY => "DT_FINI_ARRAY",
            DT_INIT_ARRAYSZ => "DT_INIT_ARRAYSZ",
            DT_FINI_ARRAYSZ => "DT_FINI_ARRAYSZ",
            DT_RUNPATH => "DT_RUNPATH",
            DT_FLAGS => "DT_FLAGS",
            DT_PREINIT_ARRAY => "DT_PREINIT_ARRAY",
            DT_PREINIT_ARRAYSZ => "DT_PREINIT_ARRAYSZ",
            DT_SYMTAB_SHNDX => "DT_SYMTAB_SHNDX",
            DT_RELRSZ => "DT_RELRSZ",
            DT_RELR => "DT_RELR",
            DT_RELRENT => "DT_RELRENT",
            DT_ANDROID_REL => "DT_ANDROID_REL",
            DT_ANDROID_RELSZ => "DT_ANDROID_RELSZ",
            DT_ANDROID_RELA => "DT_ANDROID_RELA",
            DT_ANDROID_RELASZ => "DT_ANDROID_RELASZ",
            DT_ANDROID_RELR => "DT_ANDROID_RELR",
            DT_ANDROID_RELRSZ => "DT_ANDROID_RELRSZ",
            DT_ANDROID_RELRENT => "DT_ANDROID_RELRENT",
            DT_GNU_HASH => "DT_GNU_HASH",
            DT_VERSYM => "DT_VERSYM",
            DT_RELACOUNT => "DT_RELACOUNT",
            DT_RELCOUNT => "DT_RELCOUNT",
            DT_FLAGS_1 => "DT_FLAGS_1",
            DT_VERDEF => "DT_VERDEF",
            DT_VERDEFNUM => "DT_VERDEFNUM",
            DT_VERNEED => "DT_VERNEED",
            DT_VERNEEDNUM => "DT_VERNEEDNUM",
            _ => return None,
        })
    }
}

/// Duplicate detection for the tags that must be unique. A second `DT_STRTAB` would mean the
/// loader and the file disagree about where strings live, and picking the first silently is
/// exactly the sort of guess that shows up much later.
#[derive(Default)]
struct TagCounts {
    strtab: usize,
    symtab: usize,
    hash: usize,
    gnu_hash: usize,
    rela: usize,
    android_rela: usize,
    android_rel: usize,
    jmprel: usize,
}

impl TagCounts {
    fn bump(&mut self, tag: i64) {
        match tag {
            DT_STRTAB => self.strtab += 1,
            DT_SYMTAB => self.symtab += 1,
            DT_HASH => self.hash += 1,
            DT_GNU_HASH => self.gnu_hash += 1,
            DT_RELA => self.rela += 1,
            DT_ANDROID_RELA => self.android_rela += 1,
            DT_ANDROID_REL => self.android_rel += 1,
            DT_JMPREL => self.jmprel += 1,
            _ => {}
        }
    }

    fn check(&self) -> Result<()> {
        for (tag, count) in [
            ("DT_STRTAB", self.strtab),
            ("DT_SYMTAB", self.symtab),
            ("DT_HASH", self.hash),
            ("DT_GNU_HASH", self.gnu_hash),
            ("DT_RELA", self.rela),
            ("DT_ANDROID_RELA", self.android_rela),
            ("DT_ANDROID_REL", self.android_rel),
            ("DT_JMPREL", self.jmprel),
        ] {
            if count > 1 {
                return Err(ElfError::DuplicateDynamicTag { tag, count });
            }
        }
        Ok(())
    }
}
