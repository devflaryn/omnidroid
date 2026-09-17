//! `Elf64_Phdr` and `Elf64_Shdr` parsing.
//!
//! Section headers are parsed for diagnostics and cross-checking only. A runtime loader must
//! never depend on them: they are not covered by any `PT_LOAD` segment and a stripped library
//! may not have them at all. Everything the loader needs comes from `PT_DYNAMIC`.

use crate::consts::*;
use crate::error::Result;
use crate::reader::View;

bitflags::bitflags! {
    /// `p_flags`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SegmentFlags: u32 {
        const EXEC  = 0x1;
        const WRITE = 0x2;
        const READ  = 0x4;
    }
}

/// One program header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub p_type: u32,
    pub p_flags: SegmentFlags,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

impl Segment {
    pub(crate) fn parse(view: &View<'_>, at: usize) -> Result<Self> {
        Ok(Segment {
            p_type: view.u32("p_type", at)?,
            p_flags: SegmentFlags::from_bits_retain(view.u32("p_flags", at + 4)?),
            p_offset: view.u64("p_offset", at + 8)?,
            p_vaddr: view.u64("p_vaddr", at + 16)?,
            p_paddr: view.u64("p_paddr", at + 24)?,
            p_filesz: view.u64("p_filesz", at + 32)?,
            p_memsz: view.u64("p_memsz", at + 40)?,
            p_align: view.u64("p_align", at + 48)?,
        })
    }

    /// End of this segment's memory image, saturating.
    #[inline]
    pub fn vaddr_end(&self) -> u64 {
        self.p_vaddr.saturating_add(self.p_memsz)
    }

    /// End of this segment's file image, saturating.
    #[inline]
    pub fn file_end(&self) -> u64 {
        self.p_offset.saturating_add(self.p_filesz)
    }

    #[inline]
    pub fn is_load(&self) -> bool {
        self.p_type == PT_LOAD
    }

    /// Does this segment's *file* image contain `[vaddr, vaddr + len)`?
    #[inline]
    pub fn file_contains_vaddr(&self, vaddr: u64, len: u64) -> bool {
        if !self.is_load() {
            return false;
        }
        let Some(end) = vaddr.checked_add(len) else {
            return false;
        };
        vaddr >= self.p_vaddr && end <= self.p_vaddr.saturating_add(self.p_filesz)
    }

    /// Human-readable `p_type` name, for diagnostics.
    pub fn type_name(&self) -> Option<&'static str> {
        Some(match self.p_type {
            PT_NULL => "PT_NULL",
            PT_LOAD => "PT_LOAD",
            PT_DYNAMIC => "PT_DYNAMIC",
            PT_INTERP => "PT_INTERP",
            PT_NOTE => "PT_NOTE",
            PT_SHLIB => "PT_SHLIB",
            PT_PHDR => "PT_PHDR",
            PT_TLS => "PT_TLS",
            PT_GNU_EH_FRAME => "PT_GNU_EH_FRAME",
            PT_GNU_STACK => "PT_GNU_STACK",
            PT_GNU_RELRO => "PT_GNU_RELRO",
            PT_GNU_PROPERTY => "PT_GNU_PROPERTY",
            PT_AARCH64_MEMTAG_MTE => "PT_AARCH64_MEMTAG_MTE",
            _ => return None,
        })
    }
}

/// One section header. Diagnostics only — see the module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub sh_name: u32,
    pub sh_type: u32,
    pub sh_flags: u64,
    pub sh_addr: u64,
    pub sh_offset: u64,
    pub sh_size: u64,
    pub sh_link: u32,
    pub sh_info: u32,
    pub sh_addralign: u64,
    pub sh_entsize: u64,
}

impl Section {
    pub(crate) fn parse(view: &View<'_>, at: usize) -> Result<Self> {
        Ok(Section {
            sh_name: view.u32("sh_name", at)?,
            sh_type: view.u32("sh_type", at + 4)?,
            sh_flags: view.u64("sh_flags", at + 8)?,
            sh_addr: view.u64("sh_addr", at + 16)?,
            sh_offset: view.u64("sh_offset", at + 24)?,
            sh_size: view.u64("sh_size", at + 32)?,
            sh_link: view.u32("sh_link", at + 40)?,
            sh_info: view.u32("sh_info", at + 44)?,
            sh_addralign: view.u64("sh_addralign", at + 48)?,
            sh_entsize: view.u64("sh_entsize", at + 56)?,
        })
    }
}
