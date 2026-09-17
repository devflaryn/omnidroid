//! ELF and AArch64 constants, spelled out so no magic number appears in the logic.
//!
//! Values are taken from the ELF gABI, the *ELF for the Arm 64-bit Architecture* (AAELF64)
//! document and, for the `DT_ANDROID_*` tags, from bionic's `libc/include/elf.h`.

// ---------------------------------------------------------------------------------------------
// e_ident
// ---------------------------------------------------------------------------------------------

/// `\x7fELF`.
pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

pub const EI_NIDENT: usize = 16;

pub const ELFCLASSNONE: u8 = 0;
pub const ELFCLASS32: u8 = 1;
pub const ELFCLASS64: u8 = 2;

pub const ELFDATANONE: u8 = 0;
pub const ELFDATA2LSB: u8 = 1;
pub const ELFDATA2MSB: u8 = 2;

pub const EV_CURRENT: u8 = 1;

// ---------------------------------------------------------------------------------------------
// e_type / e_machine
// ---------------------------------------------------------------------------------------------

pub const ET_NONE: u16 = 0;
pub const ET_REL: u16 = 1;
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;
pub const ET_CORE: u16 = 4;

/// `EM_AARCH64`.
pub const EM_AARCH64: u16 = 183;

// ---------------------------------------------------------------------------------------------
// Program header types
// ---------------------------------------------------------------------------------------------

pub const PT_NULL: u32 = 0;
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub const PT_NOTE: u32 = 4;
pub const PT_SHLIB: u32 = 5;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
pub const PT_GNU_STACK: u32 = 0x6474_e551;
pub const PT_GNU_RELRO: u32 = 0x6474_e552;
pub const PT_GNU_PROPERTY: u32 = 0x6474_e553;
pub const PT_AARCH64_MEMTAG_MTE: u32 = 0x7000_0002;

// ---------------------------------------------------------------------------------------------
// Section header types
// ---------------------------------------------------------------------------------------------

pub const SHT_NULL: u32 = 0;
pub const SHT_PROGBITS: u32 = 1;
pub const SHT_SYMTAB: u32 = 2;
pub const SHT_STRTAB: u32 = 3;
pub const SHT_RELA: u32 = 4;
pub const SHT_HASH: u32 = 5;
pub const SHT_DYNAMIC: u32 = 6;
pub const SHT_NOTE: u32 = 7;
pub const SHT_NOBITS: u32 = 8;
pub const SHT_REL: u32 = 9;
pub const SHT_DYNSYM: u32 = 11;
pub const SHT_INIT_ARRAY: u32 = 14;
pub const SHT_FINI_ARRAY: u32 = 15;
pub const SHT_PREINIT_ARRAY: u32 = 16;
pub const SHT_RELR: u32 = 19;
pub const SHT_GNU_HASH: u32 = 0x6fff_fff6;

pub const SHN_UNDEF: u16 = 0;

// ---------------------------------------------------------------------------------------------
// Dynamic tags
// ---------------------------------------------------------------------------------------------

pub const DT_NULL: i64 = 0;
pub const DT_NEEDED: i64 = 1;
pub const DT_PLTRELSZ: i64 = 2;
pub const DT_PLTGOT: i64 = 3;
pub const DT_HASH: i64 = 4;
pub const DT_STRTAB: i64 = 5;
pub const DT_SYMTAB: i64 = 6;
pub const DT_RELA: i64 = 7;
pub const DT_RELASZ: i64 = 8;
pub const DT_RELAENT: i64 = 9;
pub const DT_STRSZ: i64 = 10;
pub const DT_SYMENT: i64 = 11;
pub const DT_INIT: i64 = 12;
pub const DT_FINI: i64 = 13;
pub const DT_SONAME: i64 = 14;
pub const DT_RPATH: i64 = 15;
pub const DT_SYMBOLIC: i64 = 16;
pub const DT_REL: i64 = 17;
pub const DT_RELSZ: i64 = 18;
pub const DT_RELENT: i64 = 19;
pub const DT_PLTREL: i64 = 20;
pub const DT_DEBUG: i64 = 21;
pub const DT_TEXTREL: i64 = 22;
pub const DT_JMPREL: i64 = 23;
pub const DT_BIND_NOW: i64 = 24;
pub const DT_INIT_ARRAY: i64 = 25;
pub const DT_FINI_ARRAY: i64 = 26;
pub const DT_INIT_ARRAYSZ: i64 = 27;
pub const DT_FINI_ARRAYSZ: i64 = 28;
pub const DT_RUNPATH: i64 = 29;
pub const DT_FLAGS: i64 = 30;
pub const DT_PREINIT_ARRAY: i64 = 32;
pub const DT_PREINIT_ARRAYSZ: i64 = 33;
pub const DT_SYMTAB_SHNDX: i64 = 34;
pub const DT_RELRSZ: i64 = 35;
pub const DT_RELR: i64 = 36;
pub const DT_RELRENT: i64 = 37;

/// `DT_LOOS`, the base the four `DT_ANDROID_*` tags are defined against in bionic.
pub const DT_LOOS: i64 = 0x6000_000d;

/// `DT_ANDROID_REL` = `DT_LOOS + 2` = `0x6000000f`. Packed `Elf64_Rel` (no addends).
pub const DT_ANDROID_REL: i64 = DT_LOOS + 2;
/// `DT_ANDROID_RELSZ` = `DT_LOOS + 3` = `0x60000010`.
pub const DT_ANDROID_RELSZ: i64 = DT_LOOS + 3;
/// `DT_ANDROID_RELA` = `DT_LOOS + 4` = `0x60000011`. Packed `Elf64_Rela` (with addends).
///
/// This is the tag `libroblox.so` actually uses; see `docs/DECISIONS.md` D9.
pub const DT_ANDROID_RELA: i64 = DT_LOOS + 4;
/// `DT_ANDROID_RELASZ` = `DT_LOOS + 5` = `0x60000012`.
pub const DT_ANDROID_RELASZ: i64 = DT_LOOS + 5;

/// Android's pre-standardisation `DT_RELR` aliases, kept so an older binary is not silently
/// treated as having no `RELR` table at all.
pub const DT_ANDROID_RELR: i64 = 0x6fff_e000;
pub const DT_ANDROID_RELRSZ: i64 = 0x6fff_e001;
pub const DT_ANDROID_RELRENT: i64 = 0x6fff_e003;

pub const DT_GNU_HASH: i64 = 0x6fff_fef5;
pub const DT_VERSYM: i64 = 0x6fff_fff0;
pub const DT_RELACOUNT: i64 = 0x6fff_fff9;
pub const DT_RELCOUNT: i64 = 0x6fff_fffa;
pub const DT_FLAGS_1: i64 = 0x6fff_fffb;
pub const DT_VERDEF: i64 = 0x6fff_fffc;
pub const DT_VERDEFNUM: i64 = 0x6fff_fffd;
pub const DT_VERNEED: i64 = 0x6fff_fffe;
pub const DT_VERNEEDNUM: i64 = 0x6fff_ffff;

// DT_FLAGS bits we care about.
pub const DF_ORIGIN: u64 = 0x1;
pub const DF_SYMBOLIC: u64 = 0x2;
pub const DF_TEXTREL: u64 = 0x4;
pub const DF_BIND_NOW: u64 = 0x8;
pub const DF_STATIC_TLS: u64 = 0x10;

pub const DF_1_NOW: u64 = 0x1;
pub const DF_1_GLOBAL: u64 = 0x2;
pub const DF_1_NODELETE: u64 = 0x8;
pub const DF_1_PIE: u64 = 0x0800_0000;

// ---------------------------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------------------------

pub const STB_LOCAL: u8 = 0;
pub const STB_GLOBAL: u8 = 1;
pub const STB_WEAK: u8 = 2;
pub const STB_GNU_UNIQUE: u8 = 10;

pub const STT_NOTYPE: u8 = 0;
pub const STT_OBJECT: u8 = 1;
pub const STT_FUNC: u8 = 2;
pub const STT_SECTION: u8 = 3;
pub const STT_FILE: u8 = 4;
pub const STT_COMMON: u8 = 5;
pub const STT_TLS: u8 = 6;
pub const STT_GNU_IFUNC: u8 = 10;

pub const STV_DEFAULT: u8 = 0;
pub const STV_INTERNAL: u8 = 1;
pub const STV_HIDDEN: u8 = 2;
pub const STV_PROTECTED: u8 = 3;

/// Size of one `Elf64_Sym`.
pub const SIZEOF_SYM: usize = 24;
/// Size of one `Elf64_Rela`.
pub const SIZEOF_RELA: usize = 24;
/// Size of one `Elf64_Rel`.
pub const SIZEOF_REL: usize = 16;
/// Size of one `Elf64_Dyn`.
pub const SIZEOF_DYN: usize = 16;
/// Size of one `Elf64_Phdr`.
pub const SIZEOF_PHDR: usize = 56;
/// Size of one `Elf64_Shdr`.
pub const SIZEOF_SHDR: usize = 64;
/// Size of the `Elf64_Ehdr`.
pub const SIZEOF_EHDR: usize = 64;
/// Size of one pointer-sized `init_array` / `fini_array` slot.
pub const SIZEOF_PTR: usize = 8;

// ---------------------------------------------------------------------------------------------
// AArch64 dynamic relocation types (AAELF64 section 5.7.13)
// ---------------------------------------------------------------------------------------------
//
// Note the numbering carefully: `R_AARCH64_ABS64` is 0x101 = 257, **not** 256. An off-by-one
// here mislabels the 22 static-data relocations in `libroblox.so` as 32-bit when they are
// 64-bit, which would corrupt eight bytes of every one of them at apply time.

pub const R_AARCH64_NONE: u32 = 0;
pub const R_AARCH64_ABS64: u32 = 257;
pub const R_AARCH64_ABS32: u32 = 258;
pub const R_AARCH64_ABS16: u32 = 259;
pub const R_AARCH64_PREL64: u32 = 260;
pub const R_AARCH64_PREL32: u32 = 261;
pub const R_AARCH64_PREL16: u32 = 262;
pub const R_AARCH64_COPY: u32 = 1024;
pub const R_AARCH64_GLOB_DAT: u32 = 1025;
pub const R_AARCH64_JUMP_SLOT: u32 = 1026;
pub const R_AARCH64_RELATIVE: u32 = 1027;
pub const R_AARCH64_TLS_DTPREL64: u32 = 1028;
pub const R_AARCH64_TLS_DTPMOD64: u32 = 1029;
pub const R_AARCH64_TLS_TPREL64: u32 = 1030;
pub const R_AARCH64_TLSDESC: u32 = 1031;
pub const R_AARCH64_IRELATIVE: u32 = 1032;

/// Human-readable name for an AArch64 dynamic relocation type, for diagnostics.
pub fn r_aarch64_name(ty: u32) -> Option<&'static str> {
    Some(match ty {
        R_AARCH64_NONE => "R_AARCH64_NONE",
        R_AARCH64_ABS64 => "R_AARCH64_ABS64",
        R_AARCH64_ABS32 => "R_AARCH64_ABS32",
        R_AARCH64_ABS16 => "R_AARCH64_ABS16",
        R_AARCH64_PREL64 => "R_AARCH64_PREL64",
        R_AARCH64_PREL32 => "R_AARCH64_PREL32",
        R_AARCH64_PREL16 => "R_AARCH64_PREL16",
        R_AARCH64_COPY => "R_AARCH64_COPY",
        R_AARCH64_GLOB_DAT => "R_AARCH64_GLOB_DAT",
        R_AARCH64_JUMP_SLOT => "R_AARCH64_JUMP_SLOT",
        R_AARCH64_RELATIVE => "R_AARCH64_RELATIVE",
        R_AARCH64_TLS_DTPREL64 => "R_AARCH64_TLS_DTPREL64",
        R_AARCH64_TLS_DTPMOD64 => "R_AARCH64_TLS_DTPMOD64",
        R_AARCH64_TLS_TPREL64 => "R_AARCH64_TLS_TPREL64",
        R_AARCH64_TLSDESC => "R_AARCH64_TLSDESC",
        R_AARCH64_IRELATIVE => "R_AARCH64_IRELATIVE",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------------------------

/// `NT_GNU_ABI_TAG`.
pub const NT_GNU_ABI_TAG: u32 = 1;
/// `NT_GNU_BUILD_ID`.
pub const NT_GNU_BUILD_ID: u32 = 3;
/// `NT_GNU_PROPERTY_TYPE_0`.
pub const NT_GNU_PROPERTY_TYPE_0: u32 = 5;

/// `GNU_PROPERTY_AARCH64_FEATURE_1_AND`.
pub const GNU_PROPERTY_AARCH64_FEATURE_1_AND: u32 = 0xc000_0000;
pub const GNU_PROPERTY_AARCH64_FEATURE_1_BTI: u32 = 1 << 0;
pub const GNU_PROPERTY_AARCH64_FEATURE_1_PAC: u32 = 1 << 1;
pub const GNU_PROPERTY_AARCH64_FEATURE_1_GCS: u32 = 1 << 2;

/// `GNU_PROPERTY_MEMORY_SEAL`, emitted by newer linkers; harmless but recognised.
pub const GNU_PROPERTY_MEMORY_SEAL: u32 = 3;
