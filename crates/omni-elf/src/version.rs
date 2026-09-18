//! `DT_VERNEED` / `DT_VERSYM`: which library each imported symbol is expected to come from.
//!
//! # Why the loader needs this
//!
//! An undefined symbol in `.dynsym` carries a name and a type and *nothing else*. It does not say
//! which of the ten `DT_NEEDED` libraries is supposed to define it, so "565 unresolved imports"
//! cannot be turned into a per-library work list from the symbol table alone.
//!
//! Symbol versioning answers it for the libraries that use it. `libroblox.so` has three
//! `Elf64_Verneed` records — `libc.so`, `libm.so`, `libdl.so` — and `DT_VERSYM` maps every symbol
//! to one of their version indices, which attributes **407 of the 565** imports to a named library
//! straight out of the file, with no guessing. The remaining 158 reference version index 1
//! (`VER_NDX_GLOBAL`, "unversioned"), because the Android libraries that provide them
//! (`libandroid.so`, `libmediandk.so`, `libEGL.so`, `libGLESv2.so`, `liblog.so`, `libOpenSLES.so`,
//! `libOpenMAXAL.so`) ship no version definitions at all. For those the file genuinely does not
//! record a provider, and this module says so — [`SymbolRequirement::Unversioned`] — rather than
//! inventing one from the symbol's name. Name-prefix attribution is a reporting heuristic and is
//! kept out of the loader.
//!
//! # Hostile input
//!
//! `vn_next` and `vna_next` are byte deltas read from the file. A zero delta is a cycle and an
//! attacker-chosen one is a pointer, so the walk is bounded three ways at once: by
//! `DT_VERNEEDNUM`, by strictly-increasing offsets, and by the containing `PT_LOAD`'s file image.
//! Nothing here allocates a count taken from the file without first bounding it by bytes that
//! actually exist.

use crate::consts::*;
use crate::error::{ElfError, Result};
use crate::reader::View;
use crate::symbols::StrTab;

/// One `vernaux`: a version of a needed library that this object references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionNeedAux<'a> {
    /// `vna_other`: the index `DT_VERSYM` uses to refer to this version.
    pub index: u16,
    /// `vna_name`, resolved: e.g. `"LIBC"`, `"LIBC_N"`.
    pub name: &'a str,
    /// `vna_hash`.
    pub hash: u32,
    /// `vna_flags`.
    pub flags: u16,
}

/// One `Elf64_Verneed`: a needed library, and the versions of it this object references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionNeed<'a> {
    /// `vn_file`, resolved: e.g. `"libc.so"`.
    pub library: &'a str,
    /// `vn_version`. 1 (`VER_NEED_CURRENT`) is the only value ever emitted.
    pub version: u16,
    /// The versions of `library` that this object references.
    pub auxes: Vec<VersionNeedAux<'a>>,
}

/// What `DT_VERSYM` says about one symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolRequirement<'a> {
    /// The object has no `DT_VERSYM` at all, so versioning says nothing about any symbol.
    NoVersionTable,
    /// Version index 0: a local symbol, not visible outside the object.
    Local,
    /// Version index 1: global and unversioned. For an *undefined* symbol this is the honest
    /// "the file does not record which library provides this".
    Unversioned,
    /// The symbol is required from a named library at a named version.
    Needed {
        /// `vn_file` of the matching `Elf64_Verneed`.
        library: &'a str,
        /// `vna_name` of the matching `vernaux`.
        version: &'a str,
        /// Whether the reference was marked hidden in `DT_VERSYM`.
        hidden: bool,
    },
    /// The symbol is *defined* by this object at one of its own `DT_VERDEF` versions, or names a
    /// version index that no `Elf64_Verneed` record claims. Carried rather than rejected: a
    /// dangling version index in a tampered file must not stop the object from loading, it must
    /// only stop the loader from claiming to know where the symbol comes from.
    Defined {
        /// The raw `DT_VERSYM` index.
        index: u16,
    },
}

impl<'a> SymbolRequirement<'a> {
    /// The library this symbol is required from, when the file records one.
    #[must_use]
    pub fn library(&self) -> Option<&'a str> {
        match self {
            SymbolRequirement::Needed { library, .. } => Some(library),
            _ => None,
        }
    }

    /// The version name this symbol is required at, when the file records one.
    #[must_use]
    pub fn version(&self) -> Option<&'a str> {
        match self {
            SymbolRequirement::Needed { version, .. } => Some(version),
            _ => None,
        }
    }
}

/// The object's symbol-versioning tables, resolved against its string table.
#[derive(Debug, Clone)]
pub struct VersionInfo<'a> {
    needs: Vec<VersionNeed<'a>>,
    /// `DT_VERSYM`, one `u16` per `.dynsym` entry. Absent when the object has no `DT_VERSYM`.
    versym: Option<&'a [u8]>,
    symbol_count: u32,
}

impl<'a> VersionInfo<'a> {
    /// Build from the raw tables. `versym` must already be `2 * symbol_count` bytes or longer.
    pub(crate) fn new(
        needs: Vec<VersionNeed<'a>>,
        versym: Option<&'a [u8]>,
        symbol_count: u32,
    ) -> Self {
        Self { needs, versym, symbol_count }
    }

    /// The `Elf64_Verneed` records, in file order.
    #[must_use]
    pub fn needs(&self) -> &[VersionNeed<'a>] {
        &self.needs
    }

    /// Whether the object has a `DT_VERSYM` table.
    #[must_use]
    pub fn has_versym(&self) -> bool {
        self.versym.is_some()
    }

    /// The raw `DT_VERSYM` entry for a symbol index, hidden bit included.
    ///
    /// # Errors
    ///
    /// [`ElfError::OutOfBounds`] if `index` is past the end of `.dynsym`.
    pub fn raw_index(&self, index: u32) -> Result<Option<u16>> {
        let Some(versym) = self.versym else {
            return Ok(None);
        };
        if index >= self.symbol_count {
            return Err(ElfError::SymbolIndexOutOfBounds { index, count: self.symbol_count });
        }
        let at = index as usize * 2;
        View::new(versym).u16("DT_VERSYM", at).map(Some)
    }

    /// What the version tables say about one symbol.
    ///
    /// # Errors
    ///
    /// [`ElfError::SymbolIndexOutOfBounds`] if `index` is past the end of `.dynsym`.
    pub fn requirement(&self, index: u32) -> Result<SymbolRequirement<'a>> {
        let Some(raw) = self.raw_index(index)? else {
            return Ok(SymbolRequirement::NoVersionTable);
        };
        let hidden = raw & VERSYM_HIDDEN != 0;
        let ndx = raw & !VERSYM_HIDDEN;
        match ndx {
            VER_NDX_LOCAL => Ok(SymbolRequirement::Local),
            VER_NDX_GLOBAL => Ok(SymbolRequirement::Unversioned),
            _ => {
                for need in &self.needs {
                    for aux in &need.auxes {
                        if aux.index == ndx {
                            return Ok(SymbolRequirement::Needed {
                                library: need.library,
                                version: aux.name,
                                hidden,
                            });
                        }
                    }
                }
                Ok(SymbolRequirement::Defined { index: ndx })
            }
        }
    }
}

/// Walk the `Elf64_Verneed` chain.
///
/// `view` is the `DT_VERNEED` table clipped to the end of its containing `PT_LOAD` file image —
/// the table declares no size of its own, exactly like the hash tables.
pub(crate) fn parse_verneed<'a>(
    view: &View<'a>,
    count: u64,
    strtab: &StrTab<'a>,
) -> Result<Vec<VersionNeed<'a>>> {
    // `DT_VERNEEDNUM` is a file-controlled count, so it is bounded by the bytes that exist before
    // it is allowed to size anything: one record is 16 bytes and they cannot overlap.
    let max_records = view.len() / SIZEOF_VERNEED;
    let count = crate::reader::to_usize("DT_VERNEEDNUM", count)?;
    if count > max_records {
        return Err(ElfError::VersionCountExceedsTable {
            what: "DT_VERNEEDNUM",
            count: count as u64,
            max: max_records as u64,
        });
    }

    let mut out = Vec::new();
    out.try_reserve(count).map_err(|_| ElfError::AllocationFailed {
        bytes: count.saturating_mul(core::mem::size_of::<VersionNeed<'_>>()),
    })?;

    let mut at = 0usize;
    for _ in 0..count {
        let version = view.u16("vn_version", at)?;
        let vn_cnt = view.u16("vn_cnt", at + 2)?;
        let vn_file = view.u32("vn_file", at + 4)?;
        let vn_aux = view.u32("vn_aux", at + 8)?;
        let vn_next = view.u32("vn_next", at + 12)?;

        let library = strtab.get(u64::from(vn_file))?;
        let mut auxes = Vec::new();
        // `vn_cnt` is likewise bounded by the bytes that exist before it sizes anything.
        let aux_capacity = usize::from(vn_cnt).min(view.len() / SIZEOF_VERNAUX);
        auxes
            .try_reserve(aux_capacity)
            .map_err(|_| ElfError::AllocationFailed {
                bytes: aux_capacity.saturating_mul(core::mem::size_of::<VersionNeedAux<'_>>()),
            })?;

        let mut aux_at = at
            .checked_add(vn_aux as usize)
            .ok_or(ElfError::OutOfBounds { what: "vn_aux", offset: at, need: vn_aux as usize, have: view.len() })?;
        for _ in 0..vn_cnt {
            let hash = view.u32("vna_hash", aux_at)?;
            let flags = view.u16("vna_flags", aux_at + 4)?;
            let index = view.u16("vna_other", aux_at + 6)?;
            let name = view.u32("vna_name", aux_at + 8)?;
            let next = view.u32("vna_next", aux_at + 12)?;
            auxes.push(VersionNeedAux {
                index,
                name: strtab.get(u64::from(name))?,
                hash,
                flags,
            });
            if next == 0 {
                break;
            }
            // Strictly increasing, so a self-referential or backwards `vna_next` terminates the
            // walk instead of spinning on it.
            let step = next as usize;
            aux_at = aux_at
                .checked_add(step)
                .ok_or(ElfError::OutOfBounds { what: "vna_next", offset: aux_at, need: step, have: view.len() })?;
        }

        out.push(VersionNeed { library, version, auxes });

        if vn_next == 0 {
            break;
        }
        let step = vn_next as usize;
        at = at
            .checked_add(step)
            .ok_or(ElfError::OutOfBounds { what: "vn_next", offset: at, need: step, have: view.len() })?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// One `Elf64_Verneed` for `"libc.so"` with one `vernaux` for `"LIBC"`, at index 3.
    fn one_need() -> (Vec<u8>, &'static [u8]) {
        const STRINGS: &[u8] = b"\0libc.so\0LIBC\0";
        let mut b = vec![0u8; 64];
        put_u16(&mut b, 0, 1); // vn_version
        put_u16(&mut b, 2, 1); // vn_cnt
        put_u32(&mut b, 4, 1); // vn_file -> "libc.so"
        put_u32(&mut b, 8, 16); // vn_aux
        put_u32(&mut b, 12, 0); // vn_next: last record
        put_u32(&mut b, 16, 0xdead); // vna_hash
        put_u16(&mut b, 20, 0); // vna_flags
        put_u16(&mut b, 22, 3); // vna_other -> version index 3
        put_u32(&mut b, 24, 9); // vna_name -> "LIBC"
        put_u32(&mut b, 28, 0); // vna_next
        (b, STRINGS)
    }

    #[test]
    fn resolves_a_symbol_to_its_provider_library() {
        let (bytes, strings) = one_need();
        let strtab = StrTab::new(strings);
        let needs = parse_verneed(&View::new(&bytes), 1, &strtab).unwrap();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].library, "libc.so");
        assert_eq!(needs[0].auxes.len(), 1);
        assert_eq!(needs[0].auxes[0].name, "LIBC");
        assert_eq!(needs[0].auxes[0].index, 3);

        // Three symbols: local, unversioned, and required from libc.so at LIBC.
        let mut versym = vec![0u8; 6];
        put_u16(&mut versym, 0, VER_NDX_LOCAL);
        put_u16(&mut versym, 2, VER_NDX_GLOBAL);
        put_u16(&mut versym, 4, 3 | VERSYM_HIDDEN);
        let info = VersionInfo::new(needs, Some(&versym), 3);
        assert_eq!(info.requirement(0).unwrap(), SymbolRequirement::Local);
        assert_eq!(info.requirement(1).unwrap(), SymbolRequirement::Unversioned);
        assert_eq!(
            info.requirement(2).unwrap(),
            SymbolRequirement::Needed { library: "libc.so", version: "LIBC", hidden: true }
        );
        assert_eq!(info.requirement(2).unwrap().library(), Some("libc.so"));
        // Past the end of `.dynsym` is refused, not read.
        assert_eq!(
            info.requirement(3).unwrap_err(),
            ElfError::SymbolIndexOutOfBounds { index: 3, count: 3 }
        );
    }

    #[test]
    fn an_absent_versym_says_nothing_rather_than_guessing() {
        let info = VersionInfo::new(Vec::new(), None, 3);
        assert!(!info.has_versym());
        assert_eq!(info.requirement(0).unwrap(), SymbolRequirement::NoVersionTable);
        assert_eq!(info.raw_index(9999).unwrap(), None);
    }

    #[test]
    fn a_version_index_no_verneed_claims_is_carried_rather_than_refused() {
        // A dangling index in a tampered file must not stop the object from loading; it must only
        // stop the loader from claiming to know where the symbol comes from.
        let mut versym = vec![0u8; 2];
        put_u16(&mut versym, 0, 77);
        let info = VersionInfo::new(Vec::new(), Some(&versym), 1);
        assert_eq!(info.requirement(0).unwrap(), SymbolRequirement::Defined { index: 77 });
        assert_eq!(info.requirement(0).unwrap().library(), None);
    }

    #[test]
    fn a_forged_verneednum_cannot_size_an_allocation() {
        // `DT_VERNEEDNUM` is eight bytes an attacker types. Bounding it by the bytes that exist
        // before it sizes anything is the same discipline the relocation counts get.
        let (bytes, strings) = one_need();
        let strtab = StrTab::new(strings);
        let err = parse_verneed(&View::new(&bytes), u64::MAX, &strtab).unwrap_err();
        assert_eq!(
            err,
            ElfError::VersionCountExceedsTable {
                what: "DT_VERNEEDNUM",
                count: u64::MAX,
                max: 4,
            }
        );
    }

    #[test]
    fn a_zero_vn_next_terminates_the_walk_instead_of_spinning() {
        // vn_next == 0 means "last record". A walk that treated it as a delta would never advance,
        // and the count alone would keep it going for as many records as the table can hold.
        let (mut bytes, strings) = one_need();
        let strtab = StrTab::new(strings);
        put_u32(&mut bytes, 12, 0);
        let needs = parse_verneed(&View::new(&bytes), 4, &strtab).unwrap();
        assert_eq!(needs.len(), 1, "the walk stopped at the terminator, not at the count");

        // Same for `vna_next` inside one record.
        let mut b2 = bytes.clone();
        put_u16(&mut b2, 2, 4); // vn_cnt says four auxes
        put_u32(&mut b2, 28, 0); // but the first one terminates
        let needs = parse_verneed(&View::new(&b2), 1, &strtab).unwrap();
        assert_eq!(needs[0].auxes.len(), 1);
    }

    #[test]
    fn a_vn_next_that_points_outside_the_table_is_refused() {
        let (mut bytes, strings) = one_need();
        let strtab = StrTab::new(strings);
        put_u32(&mut bytes, 12, u32::MAX); // vn_next
        let err = parse_verneed(&View::new(&bytes), 2, &strtab).unwrap_err();
        assert!(
            matches!(err, ElfError::OutOfBounds { what: "vn_version", .. } | ElfError::OutOfBounds { what: "vn_next", .. }),
            "expected an out-of-bounds refusal, got {err}"
        );

        let mut b2 = bytes.clone();
        put_u32(&mut b2, 12, 0);
        put_u32(&mut b2, 8, u32::MAX); // vn_aux
        let err = parse_verneed(&View::new(&b2), 1, &strtab).unwrap_err();
        assert!(
            matches!(err, ElfError::OutOfBounds { .. }),
            "expected an out-of-bounds refusal, got {err}"
        );
    }
}
