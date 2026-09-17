//! Relocation entries and the four tables they can arrive in.

use crate::consts::*;
use crate::error::{ElfError, Result};
use crate::reader::View;

/// One dynamic relocation, always in `Elf64_Rela` shape.
///
/// `Elf64_Rel` entries are widened to this on parse with `r_addend == 0`, so a consumer never
/// has to branch on which table a relocation came from. Whether the *implicit* addend at
/// `r_offset` must be read instead is a property of the table, recorded in
/// [`RelocationTable::implicit_addend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rela {
    pub r_offset: u64,
    pub r_info: u64,
    pub r_addend: i64,
}

impl Rela {
    /// `ELF64_R_TYPE`: the low 32 bits of `r_info`.
    #[inline]
    pub fn r_type(&self) -> u32 {
        (self.r_info & 0xffff_ffff) as u32
    }

    /// `ELF64_R_SYM`: the high 32 bits of `r_info`.
    #[inline]
    pub fn r_sym(&self) -> u32 {
        (self.r_info >> 32) as u32
    }

    /// Does this relocation reference a symbol?
    #[inline]
    pub fn is_symbolic(&self) -> bool {
        self.r_sym() != 0
    }

    /// Relocation type name, for diagnostics.
    #[inline]
    pub fn type_name(&self) -> Option<&'static str> {
        r_aarch64_name(self.r_type())
    }

    fn parse_rela(view: &View<'_>, at: usize) -> Result<Self> {
        Ok(Rela {
            r_offset: view.u64("r_offset", at)?,
            r_info: view.u64("r_info", at + 8)?,
            r_addend: view.i64("r_addend", at + 16)?,
        })
    }

    fn parse_rel(view: &View<'_>, at: usize) -> Result<Self> {
        Ok(Rela {
            r_offset: view.u64("r_offset", at)?,
            r_info: view.u64("r_info", at + 8)?,
            r_addend: 0,
        })
    }
}

/// Which encoding a relocation table used. Kept on the decoded table so a report — and Task 5's
/// apply step — can tell a packed table from a plain one without re-reading the dynamic section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocEncoding {
    /// `DT_RELA` / `DT_JMPREL` with `DT_PLTREL == DT_RELA`.
    Rela,
    /// `DT_REL` / `DT_JMPREL` with `DT_PLTREL == DT_REL`.
    Rel,
    /// `DT_RELR`, the bitmap format. Always `R_AARCH64_RELATIVE` with an implicit addend.
    Relr,
    /// `DT_ANDROID_RELA`, `APS2`-packed.
    AndroidPackedRela,
    /// `DT_ANDROID_REL`, `APS2`-packed without addends.
    AndroidPackedRel,
}

impl RelocEncoding {
    /// Does a consumer have to read the existing value at `r_offset` as the addend?
    #[inline]
    pub fn implicit_addend(self) -> bool {
        matches!(self, RelocEncoding::Rel | RelocEncoding::Relr | RelocEncoding::AndroidPackedRel)
    }
}

/// One decoded relocation table.
#[derive(Debug, Clone)]
pub struct RelocationTable {
    /// Which dynamic tag this came from, e.g. `"DT_ANDROID_RELA"`.
    pub tag: &'static str,
    pub encoding: RelocEncoding,
    /// Virtual address of the table in the unrelocated image.
    pub vaddr: u64,
    /// Size of the table in bytes, as declared by the matching `*SZ` tag.
    pub size: u64,
    pub relocations: Vec<Rela>,
    /// Present only for `APS2` tables: the byte-exactness proof.
    pub packed: Option<crate::aps2::Aps2Summary>,
}

impl RelocationTable {
    /// Whether consumers must read the implicit addend from the target.
    #[inline]
    pub fn implicit_addend(&self) -> bool {
        self.encoding.implicit_addend()
    }

    /// Count relocations of one type.
    pub fn count_of_type(&self, ty: u32) -> usize {
        self.relocations.iter().filter(|r| r.r_type() == ty).count()
    }

    /// Count relocations carrying a non-zero `r_sym`.
    pub fn symbolic_count(&self) -> usize {
        self.relocations.iter().filter(|r| r.is_symbolic()).count()
    }
}

/// Every relocation table an object declares.
///
/// `DT_RELA`, `DT_REL`, `DT_RELR` and `DT_ANDROID_RELA` are all *general* relocation tables and
/// an object may in principle declare more than one, so they are a list rather than an
/// `Option`. `DT_JMPREL` is separate because it is separate in the file: the 534
/// `R_AARCH64_JUMP_SLOT` relocations in `libroblox.so` are in `.rela.plt`, **not** in the
/// `APS2` blob, and conflating the two is a documented past mistake (see the plan ledger).
#[derive(Debug, Clone, Default)]
pub struct Relocations {
    /// General relocations: `DT_RELA`, `DT_REL`, `DT_RELR`, `DT_ANDROID_REL(A)`.
    pub general: Vec<RelocationTable>,
    /// PLT relocations from `DT_JMPREL`.
    pub plt: Option<RelocationTable>,
}

impl Relocations {
    /// Total relocation count across every table, PLT included.
    pub fn total(&self) -> usize {
        self.general.iter().map(|t| t.relocations.len()).sum::<usize>()
            + self.plt.as_ref().map_or(0, |t| t.relocations.len())
    }

    /// The first general table with the given encoding, if any.
    pub fn general_with(&self, encoding: RelocEncoding) -> Option<&RelocationTable> {
        self.general.iter().find(|t| t.encoding == encoding)
    }

    /// Count of a relocation type across every table.
    pub fn count_of_type(&self, ty: u32) -> usize {
        self.general.iter().map(|t| t.count_of_type(ty)).sum::<usize>()
            + self.plt.as_ref().map_or(0, |t| t.count_of_type(ty))
    }
}

/// Parse a plain `Elf64_Rela` table.
pub fn parse_rela_table(view: &View<'_>, size: u64, entsize: Option<u64>) -> Result<Vec<Rela>> {
    parse_plain(view, size, entsize, SIZEOF_RELA, "DT_RELA", Rela::parse_rela)
}

/// Parse a plain `Elf64_Rel` table, widening each entry to [`Rela`] with a zero addend.
pub fn parse_rel_table(view: &View<'_>, size: u64, entsize: Option<u64>) -> Result<Vec<Rela>> {
    parse_plain(view, size, entsize, SIZEOF_REL, "DT_REL", Rela::parse_rel)
}

fn parse_plain(
    view: &View<'_>,
    size: u64,
    entsize: Option<u64>,
    expected: usize,
    what: &'static str,
    parse_one: fn(&View<'_>, usize) -> Result<Rela>,
) -> Result<Vec<Rela>> {
    if let Some(actual) = entsize {
        if actual != expected as u64 {
            return Err(ElfError::BadEntrySize {
                what,
                actual,
                expected: expected as u64,
            });
        }
    }
    if size % expected as u64 != 0 {
        return Err(ElfError::UnalignedTableSize {
            what,
            size,
            entsize: expected as u64,
        });
    }
    let count = (size / expected as u64) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(parse_one(view, i * expected)?);
    }
    Ok(out)
}

/// Decode a `DT_RELR` bitmap into explicit `R_AARCH64_RELATIVE` relocations.
///
/// No library in the target APK uses `RELR` (see D9), but a table that exists and is ignored
/// applies zero relocations silently, so it is decoded rather than skipped. `RELR` carries no
/// addend: the value already at `r_offset` is the addend.
pub fn parse_relr_table(view: &View<'_>, size: u64, entsize: Option<u64>) -> Result<Vec<Rela>> {
    const WORD: usize = 8;
    if let Some(actual) = entsize {
        if actual != WORD as u64 {
            return Err(ElfError::BadEntrySize {
                what: "DT_RELR",
                actual,
                expected: WORD as u64,
            });
        }
    }
    if size % WORD as u64 != 0 {
        return Err(ElfError::UnalignedTableSize {
            what: "DT_RELR",
            size,
            entsize: WORD as u64,
        });
    }
    let count = (size / WORD as u64) as usize;
    let r_info = (R_AARCH64_RELATIVE as u64) & 0xffff_ffff;
    let mut out = Vec::new();
    let mut where_: u64 = 0;
    for i in 0..count {
        let entry = view.u64("DT_RELR entry", i * WORD)?;
        if entry & 1 == 0 {
            // An even word is an address, and the next relocation slot follows it.
            where_ = entry;
            out.push(Rela {
                r_offset: where_,
                r_info,
                r_addend: 0,
            });
            where_ = where_.wrapping_add(WORD as u64);
        } else {
            // An odd word is a bitmap for the 63 slots starting at `where_`.
            let mut bits = entry >> 1;
            let mut offset = where_;
            while bits != 0 {
                if bits & 1 != 0 {
                    out.push(Rela {
                        r_offset: offset,
                        r_info,
                        r_addend: 0,
                    });
                }
                bits >>= 1;
                offset = offset.wrapping_add(WORD as u64);
            }
            where_ = where_.wrapping_add(63 * WORD as u64);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_info_splits_into_sym_and_type() {
        let r = Rela {
            r_offset: 0x1000,
            r_info: (7u64 << 32) | R_AARCH64_GLOB_DAT as u64,
            r_addend: -8,
        };
        assert_eq!(r.r_sym(), 7);
        assert_eq!(r.r_type(), R_AARCH64_GLOB_DAT);
        assert!(r.is_symbolic());
        assert_eq!(r.type_name(), Some("R_AARCH64_GLOB_DAT"));
    }

    #[test]
    fn abs64_is_257_and_abs32_is_258() {
        // Pinned deliberately: an off-by-one here mislabels 64-bit absolute relocations as
        // 32-bit, which corrupts four bytes of every one at apply time.
        assert_eq!(R_AARCH64_ABS64, 257);
        assert_eq!(R_AARCH64_ABS32, 258);
        assert_eq!(r_aarch64_name(257), Some("R_AARCH64_ABS64"));
        assert_eq!(r_aarch64_name(258), Some("R_AARCH64_ABS32"));
    }

    #[test]
    fn relr_bitmap_decodes_address_then_bits() {
        // 0x2000, then a bitmap with bits 0 and 2 set covering 0x2008, 0x2010, 0x2018.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x2000u64.to_le_bytes());
        let bitmap = (((1u64 << 0) | (1u64 << 2)) << 1) | 1;
        buf.extend_from_slice(&bitmap.to_le_bytes());
        let view = View::new(&buf);
        let relocs = parse_relr_table(&view, buf.len() as u64, Some(8)).unwrap();
        let offsets: Vec<u64> = relocs.iter().map(|r| r.r_offset).collect();
        assert_eq!(offsets, vec![0x2000, 0x2008, 0x2018]);
        assert!(relocs.iter().all(|r| r.r_type() == R_AARCH64_RELATIVE));
        assert!(relocs.iter().all(|r| r.r_addend == 0));
    }

    #[test]
    fn rela_entsize_mismatch_is_an_error() {
        let view = View::new(&[0u8; 24]);
        assert!(matches!(
            parse_rela_table(&view, 24, Some(16)).unwrap_err(),
            ElfError::BadEntrySize { actual: 16, expected: 24, .. }
        ));
        assert!(matches!(
            parse_rela_table(&view, 20, Some(24)).unwrap_err(),
            ElfError::UnalignedTableSize { size: 20, .. }
        ));
    }
}
