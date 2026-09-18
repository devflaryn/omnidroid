//! Exact function boundaries from `.eh_frame_hdr`, for a binary with no symbol table.
//!
//! # Why this exists
//!
//! `libroblox.so` is stripped: `DT_SYMTAB` holds only the 565 imports and the handful of exports,
//! so nothing in it says where the engine's own functions begin or end. But the C++ runtime is
//! statically linked and Roblox throws, so every function that can be unwound through carries a
//! **Frame Description Entry**, and `PT_GNU_EH_FRAME` carries a sorted binary-search table of them
//! for the unwinder to use at runtime (`ARCHITECTURE.md` section 4, point 7). Each FDE names its
//! function's start address and its exact length.
//!
//! That is a complete, exact function map obtained without a single symbol: **245,117 functions**
//! in `libroblox.so`. It is what [`crate::leaf`] scans, and it is how M2 found a function to run.
//!
//! # What it is not
//!
//! It is not a claim that every function in the binary is here. A function the compiler knows can
//! never be unwound through — a `noexcept` leaf with no frame — may have no FDE, and an address
//! range covered by no FDE is simply unknown rather than known to be data. Everything downstream
//! treats this as *a set of functions whose bounds are exact*, never as *the set of all functions*.
//!
//! # Hostile input
//!
//! The header is attacker-controlled (D6: this project's own APK is adversarially modified). Every
//! field is read through [`View`], the table length is checked against the bytes actually present
//! rather than trusted from `fde_count`, and an encoding this module does not implement is a typed
//! refusal naming the encoding byte — never a silent reinterpretation of the bytes as some other
//! shape.

use crate::consts::PT_GNU_EH_FRAME;
use crate::error::{ElfError, Result};
use crate::reader::View;
use crate::ElfImage;

/// `DW_EH_PE_omit`: the value is absent.
pub const DW_EH_PE_OMIT: u8 = 0xFF;

/// One function, as its FDE describes it. Addresses are `p_vaddr`-space, so a loaded address is
/// this plus the load bias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FunctionBounds {
    /// First byte of the function, in `p_vaddr` space.
    pub start: u64,
    /// Length in bytes, exactly as the FDE states it.
    pub len: u64,
}

impl FunctionBounds {
    /// One past the last byte. Saturating; a wrapping range is refused at parse time, so this
    /// cannot actually saturate for a value this module produced.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// Whether `vaddr` lies inside the function.
    #[must_use]
    pub const fn contains(self, vaddr: u64) -> bool {
        vaddr >= self.start && vaddr < self.end()
    }
}

/// The parsed `.eh_frame_hdr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EhFrameHdr {
    /// `p_vaddr` of the header itself. The `datarel` encodings are relative to this.
    pub hdr_vaddr: u64,
    /// `p_vaddr` of `.eh_frame`, as the header's `eh_frame_ptr` field resolves to.
    pub eh_frame_vaddr: u64,
    /// How many entries the binary-search table declares.
    pub fde_count: u64,
    /// `p_vaddr` of the first table entry.
    pub table_vaddr: u64,
    /// The encoding the table entries use.
    pub table_encoding: u8,
}

/// How a `DW_EH_PE_*` encoding byte is applied. Only the forms that appear in real AArch64
/// objects are implemented; anything else is refused by name rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Apply {
    /// The value stands alone.
    Absolute,
    /// Relative to the address of the encoded field itself.
    PcRelative,
    /// Relative to the start of `.eh_frame_hdr`.
    DataRelative,
}

/// Size in bytes of a `DW_EH_PE_*` value, and how to sign it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Format {
    bytes: usize,
    signed: bool,
}

fn format_of(encoding: u8, what: &'static str) -> Result<Format> {
    Ok(match encoding & 0x0F {
        0x00 => Format { bytes: 8, signed: false }, // DW_EH_PE_absptr, 64-bit target
        0x02 => Format { bytes: 2, signed: false }, // udata2
        0x03 => Format { bytes: 4, signed: false }, // udata4
        0x04 => Format { bytes: 8, signed: false }, // udata8
        0x0A => Format { bytes: 2, signed: true },  // sdata2
        0x0B => Format { bytes: 4, signed: true },  // sdata4
        0x0C => Format { bytes: 8, signed: true },  // sdata8
        // uleb128 (0x01) and sleb128 (0x09) are legal in the format and do not appear in any
        // AArch64 object this runtime loads. Refusing is what keeps a wrong guess from turning
        // into a plausible-looking but wrong function map.
        _ => {
            return Err(ElfError::UnsupportedEhFrameEncoding { what, encoding });
        }
    })
}

fn apply_of(encoding: u8, what: &'static str) -> Result<Apply> {
    // DW_EH_PE_indirect (0x80) would make the value a pointer to the real value, which needs a
    // relocated image rather than a file. It never appears in the tables this module reads.
    if encoding & 0x80 != 0 {
        return Err(ElfError::UnsupportedEhFrameEncoding { what, encoding });
    }
    Ok(match encoding & 0x70 {
        0x00 => Apply::Absolute,
        0x10 => Apply::PcRelative,
        0x30 => Apply::DataRelative,
        _ => return Err(ElfError::UnsupportedEhFrameEncoding { what, encoding }),
    })
}

/// Read one encoded value at `offset` within `view`, whose byte at index 0 has virtual address
/// `view_vaddr`. Returns the value and the number of bytes it occupied.
fn read_encoded(
    view: &View<'_>,
    view_vaddr: u64,
    offset: usize,
    encoding: u8,
    hdr_vaddr: u64,
    what: &'static str,
) -> Result<(u64, usize)> {
    let format = format_of(encoding, what)?;
    let raw = match (format.bytes, format.signed) {
        (2, false) => u64::from(view.u16(what, offset)?),
        (4, false) => u64::from(view.u32(what, offset)?),
        (8, false) => view.u64(what, offset)?,
        (2, true) => i64::from(view.u16(what, offset)? as i16) as u64,
        (4, true) => i64::from(view.u32(what, offset)? as i32) as u64,
        (8, true) => view.u64(what, offset)?,
        _ => unreachable!("format_of only produces 2, 4 and 8 byte widths"),
    };
    let base = match apply_of(encoding, what)? {
        Apply::Absolute => 0,
        Apply::PcRelative => view_vaddr.wrapping_add(offset as u64),
        Apply::DataRelative => hdr_vaddr,
    };
    Ok((base.wrapping_add(raw), format.bytes))
}

/// An unsigned LEB128, bounded to ten bytes so a run of `0x80` cannot spin.
fn uleb128(view: &View<'_>, offset: usize, what: &'static str) -> Result<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut used = 0usize;
    loop {
        let byte = view.u8(what, offset + used)?;
        used += 1;
        value |= u64::from(byte & 0x7F) << shift.min(63);
        if byte & 0x80 == 0 {
            return Ok((value, used));
        }
        shift += 7;
        if used >= 10 {
            return Err(ElfError::MalformedEhFrame {
                what,
                offset: offset as u64,
                reason: "a LEB128 ran past ten bytes, which no 64-bit value needs",
            });
        }
    }
}

/// Skip a NUL-terminated string, bounded to 32 bytes: the augmentation strings that occur are
/// `zR`, `zPLR` and the like, and a 109 MB scan for a NUL that is not there is not a search we
/// want to perform on hostile input.
fn augmentation<'a>(view: &View<'a>, offset: usize) -> Result<(&'a [u8], usize)> {
    const MAX: usize = 32;
    for len in 0..MAX {
        if view.u8("CIE augmentation", offset + len)? == 0 {
            return Ok((view.slice("CIE augmentation", offset, len)?, len + 1));
        }
    }
    Err(ElfError::MalformedEhFrame {
        what: "CIE augmentation",
        offset: offset as u64,
        reason: "the augmentation string is not NUL-terminated within 32 bytes",
    })
}

impl EhFrameHdr {
    /// Parse the `PT_GNU_EH_FRAME` segment's header.
    ///
    /// Returns `Ok(None)` when the object has no such segment — which every real shared object
    /// does have, but a hand-built test object need not.
    ///
    /// # Errors
    ///
    /// [`ElfError::UnsupportedEhFrameEncoding`] for an encoding this module does not implement,
    /// [`ElfError::MalformedEhFrame`] for a structurally impossible header, or
    /// [`ElfError::OutOfBounds`] for a truncated one.
    pub fn parse(elf: &ElfImage<'_>) -> Result<Option<Self>> {
        let Some(seg) = elf.segments().iter().find(|s| s.p_type == PT_GNU_EH_FRAME) else {
            return Ok(None);
        };
        let hdr_vaddr = seg.p_vaddr;
        let bytes = elf.slice_at_vaddr(".eh_frame_hdr", hdr_vaddr, seg.p_filesz)?;
        let view = View::new(bytes);

        let version = view.u8(".eh_frame_hdr version", 0)?;
        if version != 1 {
            return Err(ElfError::MalformedEhFrame {
                what: ".eh_frame_hdr version",
                offset: 0,
                reason: "only version 1 is defined",
            });
        }
        let ptr_encoding = view.u8(".eh_frame_hdr eh_frame_ptr_enc", 1)?;
        let count_encoding = view.u8(".eh_frame_hdr fde_count_enc", 2)?;
        let table_encoding = view.u8(".eh_frame_hdr table_enc", 3)?;

        let (eh_frame_vaddr, ptr_len) =
            read_encoded(&view, hdr_vaddr, 4, ptr_encoding, hdr_vaddr, ".eh_frame_hdr eh_frame_ptr")?;

        if count_encoding == DW_EH_PE_OMIT || table_encoding == DW_EH_PE_OMIT {
            // A header with no search table is legal and useless to us: say so rather than
            // returning an empty function list that looks like a binary with no functions.
            return Err(ElfError::MalformedEhFrame {
                what: ".eh_frame_hdr",
                offset: 0,
                reason: "the binary-search table is omitted, so this header names no functions",
            });
        }

        let (fde_count, count_len) = read_encoded(
            &view,
            hdr_vaddr,
            4 + ptr_len,
            count_encoding,
            hdr_vaddr,
            ".eh_frame_hdr fde_count",
        )?;
        let table_offset = 4 + ptr_len + count_len;
        let table_vaddr = hdr_vaddr.wrapping_add(table_offset as u64);

        // The declared count is attacker-controlled; the bytes present are not. A table entry is
        // two encoded values of the same width, so the bound is exact rather than approximate.
        let entry_bytes = 2 * format_of(table_encoding, ".eh_frame_hdr table")?.bytes as u64;
        let available = (bytes.len() as u64).saturating_sub(table_offset as u64);
        let fits = available / entry_bytes;
        if fde_count > fits {
            return Err(ElfError::MalformedEhFrame {
                what: ".eh_frame_hdr binary-search table",
                offset: table_offset as u64,
                reason: "fde_count declares more entries than the segment has bytes for",
            });
        }

        Ok(Some(Self { hdr_vaddr, eh_frame_vaddr, fde_count, table_vaddr, table_encoding }))
    }

    /// Every function the search table names, with its exact extent, in table order — which is
    /// ascending by address, because the table is sorted for binary search.
    ///
    /// The start address is read from the **FDE**, not from the table, and the two are checked
    /// against each other. They are two independent encodings of the same fact, so a disagreement
    /// means the header and `.eh_frame` do not describe the same binary, and a function map built
    /// from a header that disagrees with its own FDEs is not a function map.
    ///
    /// # Errors
    ///
    /// [`ElfError::MalformedEhFrame`] if an FDE disagrees with the table, has a wrapping range, or
    /// is structurally impossible; [`ElfError::UnsupportedEhFrameEncoding`] for a CIE whose FDE
    /// pointer encoding this module does not implement.
    pub fn functions(&self, elf: &ElfImage<'_>) -> Result<Vec<FunctionBounds>> {
        let table_format = format_of(self.table_encoding, ".eh_frame_hdr table")?;
        let entry_bytes = 2 * table_format.bytes;
        let table_len = self.fde_count.saturating_mul(entry_bytes as u64);
        let table = View::new(elf.slice_at_vaddr(
            ".eh_frame_hdr binary-search table",
            self.table_vaddr,
            table_len,
        )?);

        // `.eh_frame`'s length is not recorded anywhere, so it is taken as everything from its
        // start to the end of the segment that contains it.
        let frame = FrameReader::new(elf, self.eh_frame_vaddr)?;

        let mut out = Vec::new();
        out.try_reserve(usize::try_from(self.fde_count).unwrap_or(0)).map_err(|_| {
            ElfError::MalformedEhFrame {
                what: ".eh_frame_hdr binary-search table",
                offset: 0,
                reason: "the declared entry count does not fit in memory",
            }
        })?;

        for i in 0..self.fde_count {
            let at = (i as usize) * entry_bytes;
            let (initial_location, _) = read_encoded(
                &table,
                self.table_vaddr,
                at,
                self.table_encoding,
                self.hdr_vaddr,
                ".eh_frame_hdr table initial_location",
            )?;
            let (fde_vaddr, _) = read_encoded(
                &table,
                self.table_vaddr,
                at + table_format.bytes,
                self.table_encoding,
                self.hdr_vaddr,
                ".eh_frame_hdr table fde_ptr",
            )?;
            let bounds = frame.fde_bounds(fde_vaddr)?;
            if bounds.start != initial_location {
                return Err(ElfError::MalformedEhFrame {
                    what: ".eh_frame_hdr binary-search table",
                    offset: at as u64,
                    reason: "the table's initial_location disagrees with the FDE's pc_begin",
                });
            }
            out.push(bounds);
        }
        Ok(out)
    }
}

/// `.eh_frame` itself, with a small cache of the CIEs already parsed.
struct FrameReader<'a> {
    view: View<'a>,
    base_vaddr: u64,
}

impl<'a> FrameReader<'a> {
    fn new(elf: &ElfImage<'a>, eh_frame_vaddr: u64) -> Result<Self> {
        // `.eh_frame` lives inside one `PT_LOAD`; take the rest of that segment as its extent.
        let seg = elf
            .load_segments()
            .find(|s| eh_frame_vaddr >= s.p_vaddr && eh_frame_vaddr < s.p_vaddr + s.p_filesz)
            .ok_or(ElfError::MalformedEhFrame {
                what: ".eh_frame",
                offset: eh_frame_vaddr,
                reason: "eh_frame_ptr does not land inside any PT_LOAD's file image",
            })?;
        let len = seg.p_vaddr + seg.p_filesz - eh_frame_vaddr;
        let bytes = elf.slice_at_vaddr(".eh_frame", eh_frame_vaddr, len)?;
        Ok(Self { view: View::new(bytes), base_vaddr: eh_frame_vaddr })
    }

    fn offset_of(&self, vaddr: u64, what: &'static str) -> Result<usize> {
        let delta = vaddr.checked_sub(self.base_vaddr).ok_or(ElfError::MalformedEhFrame {
            what,
            offset: vaddr,
            reason: "the entry lies before the start of .eh_frame",
        })?;
        usize::try_from(delta).map_err(|_| ElfError::MalformedEhFrame {
            what,
            offset: vaddr,
            reason: "the entry lies past the addressable end of .eh_frame",
        })
    }

    /// The `pc_begin`/`pc_range` of the FDE at `fde_vaddr`.
    fn fde_bounds(&self, fde_vaddr: u64) -> Result<FunctionBounds> {
        let at = self.offset_of(fde_vaddr, "FDE")?;
        let length = self.view.u32("FDE length", at)?;
        if length == 0 {
            return Err(ElfError::MalformedEhFrame {
                what: "FDE",
                offset: fde_vaddr,
                reason: "a zero length marks the end of .eh_frame, so this is not an FDE",
            });
        }
        if length == 0xFFFF_FFFF {
            // 64-bit DWARF. No AArch64 toolchain emits it for `.eh_frame`, and guessing would
            // shift every subsequent field by eight bytes.
            return Err(ElfError::MalformedEhFrame {
                what: "FDE",
                offset: fde_vaddr,
                reason: "64-bit DWARF lengths are not implemented",
            });
        }
        let cie_pointer_at = at + 4;
        let cie_delta = self.view.u32("FDE CIE_pointer", cie_pointer_at)?;
        if cie_delta == 0 {
            return Err(ElfError::MalformedEhFrame {
                what: "FDE",
                offset: fde_vaddr,
                reason: "a zero CIE_pointer makes this a CIE, not an FDE",
            });
        }
        let cie_at = (cie_pointer_at as u64).checked_sub(u64::from(cie_delta)).ok_or(
            ElfError::MalformedEhFrame {
                what: "FDE CIE_pointer",
                offset: fde_vaddr,
                reason: "the CIE_pointer reaches back before the start of .eh_frame",
            },
        )?;
        let fde_encoding = self.cie_fde_encoding(usize::try_from(cie_at).unwrap_or(usize::MAX))?;

        let pc_begin_at = cie_pointer_at + 4;
        let (start, pc_begin_len) = read_encoded(
            &self.view,
            self.base_vaddr,
            pc_begin_at,
            fde_encoding,
            0,
            "FDE pc_begin",
        )?;
        // `pc_range` uses the same *format* as `pc_begin` but is never relative to anything: it is
        // a length. Applying the `pcrel` base to it would produce an address-sized nonsense.
        let (len, _) = read_encoded(
            &self.view,
            self.base_vaddr,
            pc_begin_at + pc_begin_len,
            fde_encoding & 0x0F,
            0,
            "FDE pc_range",
        )?;
        // A zero `pc_range` is legal and does occur: `libroblox.so` has exactly **one**, at
        // `0x364f404`. It describes an empty range, so it names no instruction, and it is kept
        // rather than refused — refusing it would reject the whole 245,117-entry map over one
        // entry, and silently dropping it would make the count disagree with `fde_count`.
        // [`crate::leaf`] treats an empty body as undecodable, so it can never become a candidate.
        if start.checked_add(len).is_none() {
            return Err(ElfError::MalformedEhFrame {
                what: "FDE pc_range",
                offset: fde_vaddr,
                reason: "the function's extent wraps the address space",
            });
        }
        Ok(FunctionBounds { start, len })
    }

    /// The FDE pointer encoding a CIE's `R` augmentation declares, or `DW_EH_PE_absptr` when it
    /// declares none.
    fn cie_fde_encoding(&self, cie_at: usize) -> Result<u8> {
        let length = self.view.u32("CIE length", cie_at)?;
        if length == 0 || length == 0xFFFF_FFFF {
            return Err(ElfError::MalformedEhFrame {
                what: "CIE",
                offset: cie_at as u64,
                reason: "a CIE with a terminator or 64-bit DWARF length",
            });
        }
        if self.view.u32("CIE id", cie_at + 4)? != 0 {
            return Err(ElfError::MalformedEhFrame {
                what: "CIE",
                offset: cie_at as u64,
                reason: "the CIE id is not zero, so the FDE's CIE_pointer does not point at a CIE",
            });
        }
        let version = self.view.u8("CIE version", cie_at + 8)?;
        if version != 1 && version != 3 {
            return Err(ElfError::MalformedEhFrame {
                what: "CIE version",
                offset: cie_at as u64,
                reason: "only CIE versions 1 and 3 are implemented",
            });
        }
        let mut at = cie_at + 9;
        let (aug, aug_len) = augmentation(&self.view, at)?;
        at += aug_len;
        let (_code_align, n) = uleb128(&self.view, at, "CIE code_alignment_factor")?;
        at += n;
        // The data alignment factor is a SLEB128; only its length matters here, and a SLEB128's
        // length is the same as a ULEB128's.
        let (_data_align, n) = uleb128(&self.view, at, "CIE data_alignment_factor")?;
        at += n;
        if version == 1 {
            at += 1; // return_address_register is a single byte in version 1
        } else {
            let (_ra, n) = uleb128(&self.view, at, "CIE return_address_register")?;
            at += n;
        }

        if !aug.starts_with(b"z") {
            // No augmentation data, so no `R`: the FDE pointer is DW_EH_PE_absptr.
            return Ok(0x00);
        }
        let (_aug_len, n) = uleb128(&self.view, at, "CIE augmentation data length")?;
        at += n;
        for &c in &aug[1..] {
            match c {
                b'R' => return self.view.u8("CIE augmentation R", at),
                b'L' => at += 1,
                b'S' | b'B' | b'G' => {}
                b'P' => {
                    let encoding = self.view.u8("CIE augmentation P", at)?;
                    at += 1;
                    at += format_of(encoding, "CIE personality pointer")?.bytes;
                }
                other => {
                    let _ = other;
                    return Err(ElfError::MalformedEhFrame {
                        what: "CIE augmentation",
                        offset: cie_at as u64,
                        reason: "an augmentation character this module does not know, so the \
                                 augmentation data cannot be walked to reach 'R'",
                    });
                }
            }
        }
        Ok(0x00)
    }
}

impl ElfImage<'_> {
    /// Every function `.eh_frame_hdr` names, with its exact extent.
    ///
    /// `Ok(None)` when the object has no `PT_GNU_EH_FRAME`.
    ///
    /// # Errors
    ///
    /// As [`EhFrameHdr::parse`] and [`EhFrameHdr::functions`].
    pub fn eh_frame_functions(&self) -> Result<Option<Vec<FunctionBounds>>> {
        match EhFrameHdr::parse(self)? {
            None => Ok(None),
            Some(hdr) => Ok(Some(hdr.functions(self)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_encoding_table_matches_the_dwarf_specification() {
        // Widths and signs, checked one by one: getting `sdata4` wrong turns every pc-relative
        // offset above 2 GiB into a forward reference and produces a plausible but wrong map.
        for (enc, bytes, signed) in [
            (0x00u8, 8usize, false),
            (0x02, 2, false),
            (0x03, 4, false),
            (0x04, 8, false),
            (0x0A, 2, true),
            (0x0B, 4, true),
            (0x0C, 8, true),
        ] {
            let f = format_of(enc, "test").expect("a known encoding");
            assert_eq!((f.bytes, f.signed), (bytes, signed), "encoding {enc:#04x}");
        }
        for enc in [0x01u8, 0x09, 0x05, 0x0D, 0x0E, 0x0F] {
            assert!(
                matches!(
                    format_of(enc, "test"),
                    Err(ElfError::UnsupportedEhFrameEncoding { .. })
                ),
                "encoding {enc:#04x} must be refused by name, not guessed at"
            );
        }
        assert_eq!(apply_of(0x03, "t").expect("absolute"), Apply::Absolute);
        assert_eq!(apply_of(0x1B, "t").expect("pcrel"), Apply::PcRelative);
        assert_eq!(apply_of(0x3B, "t").expect("datarel"), Apply::DataRelative);
        // Indirect and the two application bases nothing emits.
        for enc in [0x9Bu8, 0x2B, 0x4B, 0x5B] {
            assert!(apply_of(enc, "t").is_err(), "{enc:#04x} must be refused");
        }
    }

    #[test]
    fn a_pc_relative_value_is_relative_to_its_own_address() {
        // `sdata4 | pcrel` at offset 8 of a view whose byte 0 is at 0x1000, holding -0x10.
        let mut bytes = vec![0u8; 16];
        bytes[8..12].copy_from_slice(&(-0x10i32).to_le_bytes());
        let view = View::new(&bytes);
        let (value, len) = read_encoded(&view, 0x1000, 8, 0x1B, 0, "t").expect("a value");
        assert_eq!(len, 4);
        assert_eq!(value, 0x1000 + 8 - 0x10, "pcrel is relative to the field, not the view");

        // The same bytes, datarel to a header at 0x2000.
        let (value, _) = read_encoded(&view, 0x1000, 8, 0x3B, 0x2000, "t").expect("a value");
        assert_eq!(value, 0x2000 - 0x10);

        // And absolute, where the sign still applies.
        let (value, _) = read_encoded(&view, 0x1000, 8, 0x0B, 0x2000, "t").expect("a value");
        assert_eq!(value, (-0x10i64) as u64);
    }

    #[test]
    fn a_leb128_that_never_terminates_is_refused_rather_than_spun_on() {
        let bytes = vec![0x80u8; 64];
        let view = View::new(&bytes);
        assert!(matches!(
            uleb128(&view, 0, "t"),
            Err(ElfError::MalformedEhFrame { .. })
        ));
        let bytes = [0xE5u8, 0x8E, 0x26];
        let view = View::new(&bytes);
        assert_eq!(uleb128(&view, 0, "t").expect("a value"), (624_485, 3));
    }

    #[test]
    fn an_unterminated_augmentation_string_is_refused() {
        let bytes = vec![b'z'; 64];
        let view = View::new(&bytes);
        assert!(matches!(
            augmentation(&view, 0),
            Err(ElfError::MalformedEhFrame { .. })
        ));
        let bytes = b"zPLR\0rest";
        let view = View::new(bytes);
        let (aug, len) = augmentation(&view, 0).expect("an augmentation");
        assert_eq!((aug, len), (b"zPLR".as_slice(), 5));
    }

    /// A minimal, valid `.eh_frame`: one CIE with a `zR` augmentation declaring
    /// `DW_EH_PE_pcrel | DW_EH_PE_sdata4`, and one FDE after it describing a 64-byte function.
    ///
    /// Built by hand so that the hostile sweeps below have something whose *correct* reading is
    /// known, which is the only way a sweep can tell "refused because it is malformed" from
    /// "refused because the reader is broken".
    fn one_cie_and_one_fde(base_vaddr: u64) -> (Vec<u8>, FunctionBounds) {
        let mut out = Vec::new();
        let cie_body: Vec<u8> = {
            let mut b = Vec::new();
            b.extend_from_slice(&0u32.to_le_bytes()); // CIE id
            b.push(1); // version
            b.extend_from_slice(b"zR\0");
            b.push(0x01); // code_alignment_factor, ULEB 1
            b.push(0x78); // data_alignment_factor, SLEB -8
            b.push(30); // return_address_register, one byte at version 1
            b.push(0x01); // augmentation data length
            b.push(0x1B); // DW_EH_PE_pcrel | sdata4
            while b.len() % 4 != 0 {
                b.push(0); // DW_CFA_nop padding
            }
            b
        };
        out.extend_from_slice(&(cie_body.len() as u32).to_le_bytes());
        out.extend_from_slice(&cie_body);

        let fde_at = out.len();
        let fde_body: Vec<u8> = {
            let mut b = Vec::new();
            b.extend_from_slice(&((fde_at + 4) as u32).to_le_bytes()); // CIE_pointer, backwards
            // `pc_begin` is pcrel from its own position, which is `fde_at + 8`.
            b.extend_from_slice(&0x1000i32.to_le_bytes());
            b.extend_from_slice(&64u32.to_le_bytes()); // pc_range
            while b.len() % 4 != 0 {
                b.push(0);
            }
            b
        };
        out.extend_from_slice(&(fde_body.len() as u32).to_le_bytes());
        out.extend_from_slice(&fde_body);

        let expected = FunctionBounds { start: base_vaddr + (fde_at as u64) + 8 + 0x1000, len: 64 };
        (out, expected)
    }

    fn reader(bytes: &[u8], base_vaddr: u64) -> FrameReader<'_> {
        FrameReader { view: View::new(bytes), base_vaddr }
    }

    /// Where the hand-built FDE starts, in the same arithmetic the reader uses.
    fn fde_offset(bytes: &[u8]) -> usize {
        4 + u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes")) as usize
    }

    #[test]
    fn a_hand_built_cie_and_fde_read_back_exactly() {
        const BASE: u64 = 0x4000;
        let (bytes, expected) = one_cie_and_one_fde(BASE);
        let fde_vaddr = BASE + fde_offset(&bytes) as u64;
        assert_eq!(reader(&bytes, BASE).fde_bounds(fde_vaddr).expect("a valid FDE"), expected);
    }

    /// **Global Constraint 11.** Every truncation of a valid `.eh_frame` is a typed error, never a
    /// panic and never a plausible-looking function.
    ///
    /// The header is attacker-controlled (D6), and a 109 MB file gives an attacker a lot of room;
    /// "it would never be truncated there" is not a property, it is a hope.
    #[test]
    fn every_truncation_of_a_valid_frame_is_an_error_and_never_a_panic() {
        const BASE: u64 = 0x4000;
        let (bytes, _) = one_cie_and_one_fde(BASE);
        let fde_vaddr = BASE + fde_offset(&bytes) as u64;
        for cut in 0..bytes.len() {
            assert!(
                reader(&bytes[..cut], BASE).fde_bounds(fde_vaddr).is_err(),
                "a frame truncated to {cut} of {} bytes was accepted",
                bytes.len()
            );
        }
        // And the whole thing is still fine, so the sweep is not passing because everything fails.
        assert!(reader(&bytes, BASE).fde_bounds(fde_vaddr).is_ok());
    }

    /// Every single-byte corruption either reads back differently or is refused.
    ///
    /// A byte that changes nothing is a byte the reader is not looking at, and in this format
    /// every byte decides a length, an encoding or an address. The two genuine exceptions are
    /// named rather than skipped by a tolerance: the CIE's own length field, which this FDE reaches
    /// its CIE without consulting, and the `DW_CFA_nop` padding.
    #[test]
    fn every_single_byte_corruption_is_seen() {
        const BASE: u64 = 0x4000;
        let (bytes, expected) = one_cie_and_one_fde(BASE);
        let fde_at = fde_offset(&bytes);
        let fde_vaddr = BASE + fde_at as u64;
        // The bytes this FDE genuinely does not consult, named one by one rather than covered by a
        // tolerance:
        //   0..4    the CIE's own length -- the FDE reaches its CIE through `CIE_pointer`;
        //   12..16  code_alignment_factor, data_alignment_factor, return_address_register and the
        //           augmentation-data length, all of which are *walked over* to reach `R` and
        //           whose values decide nothing about a function's bounds;
        //   padding the trailing DW_CFA_nops;
        //   fde..+4 the FDE's own length, which decides nothing about the bounds -- only its two
        //           sentinel values, 0 and 0xFFFFFFFF, are checked, and both are provoked in
        //           `each_structural_impossibility_is_refused_by_name`.
        // Their *lengths* are consulted, though, and that is checked separately below.
        let padding_starts = fde_at - 3;
        let ignorable = |i: usize| {
            i < 4
                || (12..16).contains(&i)
                || (i >= padding_starts && i < fde_at)
                || (fde_at..fde_at + 4).contains(&i)
        };
        let mut seen = 0usize;
        for i in 0..bytes.len() {
            if ignorable(i) {
                continue;
            }
            for delta in [1u8, 0x80, 0xFF] {
                let mut corrupt = bytes.clone();
                corrupt[i] = corrupt[i].wrapping_add(delta);
                if corrupt == bytes {
                    continue;
                }
                let got = reader(&corrupt, BASE).fde_bounds(fde_vaddr);
                assert!(
                    !got.as_ref().is_ok_and(|b| *b == expected),
                    "byte {i} += {delta:#x} changed nothing the reader looks at"
                );
                seen += 1;
            }
        }
        assert!(seen > 20, "the sweep must actually have corrupted something: {seen}");

        // The four walked-over bytes decide nothing by value, but they decide where `R` is by
        // *length*. Setting the continuation bit on the first LEB lengthens it, which moves every
        // later field -- and that must not be silently absorbed.
        let mut lengthened = bytes.clone();
        lengthened[12] |= 0x80;
        let got = reader(&lengthened, BASE).fde_bounds(fde_vaddr);
        assert!(
            !got.as_ref().is_ok_and(|b| *b == expected),
            "lengthening the CIE's code_alignment_factor moves the FDE pointer encoding, so it              cannot read back the same function"
        );
    }

    /// The structural refusals, each provoked on its own so that a passing sweep cannot hide one of
    /// them never firing.
    #[test]
    fn each_structural_impossibility_is_refused_by_name() {
        const BASE: u64 = 0x4000;
        let (bytes, _) = one_cie_and_one_fde(BASE);
        let fde_at = fde_offset(&bytes);
        let fde_vaddr = BASE + fde_at as u64;

        let refusal = |edit: &dyn Fn(&mut Vec<u8>)| -> String {
            let mut b = bytes.clone();
            edit(&mut b);
            format!(
                "{}",
                reader(&b, BASE).fde_bounds(fde_vaddr).expect_err("this must be refused")
            )
        };

        // A zero length is `.eh_frame`'s terminator, not an FDE.
        let e = refusal(&|b| b[fde_at..fde_at + 4].copy_from_slice(&0u32.to_le_bytes()));
        assert!(e.contains("end of .eh_frame"), "{e}");

        // 64-bit DWARF, which no AArch64 toolchain emits and which would shift every field by
        // eight bytes -- producing addresses rather than an error.
        let e = refusal(&|b| b[fde_at..fde_at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()));
        assert!(e.contains("64-bit DWARF"), "{e}");

        // A zero `CIE_pointer` makes the entry a CIE.
        let e = refusal(&|b| b[fde_at + 4..fde_at + 8].copy_from_slice(&0u32.to_le_bytes()));
        assert!(e.contains("not an FDE"), "{e}");

        // A `CIE_pointer` reaching back past the start of the section.
        let e = refusal(&|b| {
            b[fde_at + 4..fde_at + 8].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        });
        assert!(e.contains("before the start"), "{e}");

        // A CIE whose id is not zero is not a CIE.
        let e = refusal(&|b| b[4..8].copy_from_slice(&1u32.to_le_bytes()));
        assert!(e.contains("not zero"), "{e}");

        // A CIE version nothing implements.
        let e = refusal(&|b| b[8] = 4);
        assert!(e.contains("versions 1 and 3"), "{e}");

        // An augmentation character the walker cannot step over: it would have to guess how many
        // bytes of augmentation data it consumes, and a wrong guess finds `R` in the wrong place.
        let e = refusal(&|b| b[10] = b'Q');
        assert!(e.contains("augmentation"), "{e}");

        // An FDE pointer encoding this module does not implement. The `R` byte is the last one
        // before the `DW_CFA_nop` padding.
        let e = refusal(&|b| b[padding_start(&bytes) - 1] = 0x01);
        assert!(e.contains("not implemented"), "{e}");
    }

    /// Where the CIE's `DW_CFA_nop` padding begins: three bytes before the FDE.
    fn padding_start(bytes: &[u8]) -> usize {
        fde_offset(bytes) - 3
    }

    #[test]
    fn function_bounds_do_not_wrap() {
        let f = FunctionBounds { start: 0x1000, len: 0x40 };
        assert_eq!(f.end(), 0x1040);
        assert!(f.contains(0x1000) && f.contains(0x103F));
        assert!(!f.contains(0x0FFF) && !f.contains(0x1040));
        assert_eq!(FunctionBounds { start: u64::MAX, len: 8 }.end(), u64::MAX);
    }
}
