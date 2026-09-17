//! `Elf64_Ehdr` parsing and validation.

use crate::consts::*;
use crate::error::{ElfError, Result};
use crate::reader::View;

/// The `e_ident` bytes, kept separately because every one of them is a validation gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ident {
    pub magic: [u8; 4],
    pub class: u8,
    pub data: u8,
    pub version: u8,
    pub osabi: u8,
    pub abiversion: u8,
}

/// A parsed and validated ELF64 file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    pub ident: Ident,
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

impl FileHeader {
    /// Parse and validate. Anything that is not a little-endian 64-bit AArch64 `ET_DYN` object
    /// is refused with the offending value named.
    pub fn parse(view: &View<'_>) -> Result<Self> {
        let magic = view.array4("e_ident magic", 0)?;
        if magic != ELF_MAGIC {
            return Err(ElfError::BadMagic(magic));
        }
        let ident = Ident {
            magic,
            class: view.u8("e_ident[EI_CLASS]", 4)?,
            data: view.u8("e_ident[EI_DATA]", 5)?,
            version: view.u8("e_ident[EI_VERSION]", 6)?,
            osabi: view.u8("e_ident[EI_OSABI]", 7)?,
            abiversion: view.u8("e_ident[EI_ABIVERSION]", 8)?,
        };
        if ident.class != ELFCLASS64 {
            return Err(ElfError::UnsupportedClass(ident.class));
        }
        if ident.data != ELFDATA2LSB {
            return Err(ElfError::UnsupportedEncoding(ident.data));
        }
        if ident.version != EV_CURRENT {
            return Err(ElfError::UnsupportedIdentVersion(ident.version));
        }

        // Only now is it safe to read past e_ident: the class fixes the layout.
        let hdr = FileHeader {
            ident,
            e_type: view.u16("e_type", EI_NIDENT)?,
            e_machine: view.u16("e_machine", EI_NIDENT + 2)?,
            e_version: view.u32("e_version", EI_NIDENT + 4)?,
            e_entry: view.u64("e_entry", EI_NIDENT + 8)?,
            e_phoff: view.u64("e_phoff", EI_NIDENT + 16)?,
            e_shoff: view.u64("e_shoff", EI_NIDENT + 24)?,
            e_flags: view.u32("e_flags", EI_NIDENT + 32)?,
            e_ehsize: view.u16("e_ehsize", EI_NIDENT + 36)?,
            e_phentsize: view.u16("e_phentsize", EI_NIDENT + 38)?,
            e_phnum: view.u16("e_phnum", EI_NIDENT + 40)?,
            e_shentsize: view.u16("e_shentsize", EI_NIDENT + 42)?,
            e_shnum: view.u16("e_shnum", EI_NIDENT + 44)?,
            e_shstrndx: view.u16("e_shstrndx", EI_NIDENT + 46)?,
        };

        if hdr.e_version != EV_CURRENT as u32 {
            return Err(ElfError::UnsupportedVersion(hdr.e_version));
        }
        if hdr.e_type != ET_DYN {
            return Err(ElfError::UnsupportedObjectType(hdr.e_type));
        }
        if hdr.e_machine != EM_AARCH64 {
            return Err(ElfError::UnsupportedMachine(hdr.e_machine));
        }
        if hdr.e_phnum != 0 && hdr.e_phentsize as usize != SIZEOF_PHDR {
            return Err(ElfError::BadPhentsize(hdr.e_phentsize));
        }
        if hdr.e_shnum != 0 && hdr.e_shentsize as usize != SIZEOF_SHDR {
            return Err(ElfError::BadShentsize(hdr.e_shentsize));
        }
        Ok(hdr)
    }
}
