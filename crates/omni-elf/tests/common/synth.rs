//! A minimal but genuinely loadable AArch64 `ET_DYN`, and the offsets needed to tamper with it.
//!
//! # Why this exists alongside the real library
//!
//! The golden tests load the real `libroblox.so`, which is the only thing that can prove M1. But the
//! hostile-input cases this loader has to survive are shapes the real library does not have and
//! cannot easily be edited into: a `DT_JMPREL` pointing outside the file, a relocation whose target
//! misses every `PT_LOAD`, a symbol index past the end of `.dynsym`, two `PT_LOAD`s claiming one
//! page. Producing those by mutating 109 MB also means writing 109 MB to disk per case, because the
//! loader maps the *file*, not the parsed bytes.
//!
//! So: an 8 KiB library with the same structure — two `PT_LOAD`s, `PT_DYNAMIC`, `PT_GNU_RELRO`,
//! `PT_PHDR`, a symbol table, `DT_RELA`, `DT_JMPREL` and `DT_INIT_ARRAY` whose slots are zero in the
//! file and produced by relocations, exactly as in the real thing. Every offset a test needs to
//! forge is a named constant, so a tamper case says what it is doing rather than counting bytes.
//!
//! It uses `DT_HASH` rather than `DT_GNU_HASH`, deliberately: `libroblox.so` has only `DT_GNU_HASH`
//! (D9), so the real-library tests are what cover the GNU-hash path, and building a valid GNU hash
//! table here would add a second thing that could be wrong without testing anything extra.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The layout's page: the host's, because a `PT_LOAD` aligned below the host page cannot be mapped
/// and is refused (`AlignBelowPageSize`) before any of the checks below could be reached. 4 KiB
/// everywhere but Apple silicon, whose 16 KiB pages are also what 16 KiB Android devices use, and
/// what the APK's own `libroblox.so` is aligned to. Every address past the first page below is
/// derived from it, so on a 4 KiB host the library is byte-for-byte what it always was.
pub const PAGE: u64 = if cfg!(all(target_os = "macos", target_arch = "aarch64")) { 0x4000 } else { 0x1000 };

// Program headers.
pub const PHOFF: usize = 0x40;
pub const PHENTSIZE: usize = 56;
pub const PHNUM: usize = 5;
pub const PH_PHDR: usize = 0;
pub const PH_LOAD_TEXT: usize = 1;
pub const PH_LOAD_DATA: usize = 2;
pub const PH_DYNAMIC: usize = 3;
pub const PH_RELRO: usize = 4;

// Field offsets inside one `Elf64_Phdr`.
pub const P_TYPE: usize = 0;
pub const P_FLAGS: usize = 4;
pub const P_OFFSET: usize = 8;
pub const P_VADDR: usize = 16;
pub const P_PADDR: usize = 24;
pub const P_FILESZ: usize = 32;
pub const P_MEMSZ: usize = 40;
pub const P_ALIGN: usize = 48;

// Where each table lives. File offset equals virtual address throughout, which keeps the
// `p_vaddr ≡ p_offset (mod p_align)` congruence trivially true and makes a tamper case readable.
pub const SYMTAB: u64 = 0x0160;
pub const SYMCOUNT: u32 = 3;
pub const STRTAB: u64 = 0x01b0;
pub const HASH: u64 = 0x0200;
pub const RELA: u64 = 0x0220;
pub const RELA_COUNT: usize = 6;
pub const JMPREL: u64 = 0x0400;
pub const JMPREL_COUNT: usize = 2;
/// Two bytes of "code" the `init_array` entries point at.
pub const INIT_FN_0: u64 = 0x0900;
pub const INIT_FN_1: u64 = 0x0908;

pub const DYNAMIC: u64 = PAGE;
pub const INIT_ARRAY: u64 = PAGE + 0x100;
pub const INIT_ARRAY_COUNT: usize = 2;
pub const GOT: u64 = PAGE + 0x200;
pub const GOT_PLT: u64 = PAGE + 0x300;
/// The address of the one defined data symbol, inside the writable segment.
pub const LOCAL_DATA: u64 = PAGE + 0x400;

pub const TEXT_VADDR: u64 = 0x0000;
pub const TEXT_FILESZ: u64 = PAGE;
pub const DATA_VADDR: u64 = PAGE;
pub const DATA_FILESZ: u64 = PAGE;
/// One page of file content plus one page of `.bss`.
pub const DATA_MEMSZ: u64 = 2 * PAGE;
pub const RELRO_VADDR: u64 = PAGE;
pub const RELRO_MEMSZ: u64 = PAGE;
pub const FILE_LEN: usize = 2 * PAGE as usize;

/// Symbol indices.
pub const SYM_IMPORTED_FUNC: u32 = 1;
pub const SYM_LOCAL_DATA: u32 = 2;

/// `.rela.dyn` entry indices, so a test can name the one it forges.
pub const RELA_INIT_0: usize = 0;
pub const RELA_INIT_1: usize = 1;
pub const RELA_GOT_0: usize = 2;
pub const RELA_GOT_1: usize = 3;
pub const RELA_GLOB_DAT: usize = 4;
pub const RELA_ABS64: usize = 5;

const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_PHDR: u32 = 6;
const PT_GNU_RELRO: u32 = 0x6474_e552;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const DT_NULL: i64 = 0;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_SONAME: i64 = 14;
const DT_PLTREL: i64 = 20;
const DT_JMPREL: i64 = 23;
const DT_PLTRELSZ: i64 = 2;
const DT_INIT_ARRAY: i64 = 25;
const DT_INIT_ARRAYSZ: i64 = 27;

const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_RELATIVE: u32 = 1027;

/// String-table offsets, fixed so a test can forge an `st_name`.
pub const STR_IMPORTED_FUNC: u64 = 1;
pub const STR_LOCAL_DATA: u64 = 15;
pub const STR_SONAME: u64 = 26;
const STRINGS: &[u8] = b"\0imported_func\0local_data\0libsynth.so\0";

pub fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn get_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// File offset of one program header.
#[must_use]
pub fn phdr(index: usize) -> usize {
    PHOFF + index * PHENTSIZE
}

/// File offset of one `.rela.dyn` entry.
#[must_use]
pub fn rela(index: usize) -> usize {
    RELA as usize + index * 24
}

/// File offset of one `.rela.plt` entry.
#[must_use]
pub fn jmprel(index: usize) -> usize {
    JMPREL as usize + index * 24
}

/// File offset of one `Elf64_Sym`.
#[must_use]
pub fn sym(index: u32) -> usize {
    SYMTAB as usize + index as usize * 24
}

/// File offset of the `d_un` field of the first dynamic entry with this tag.
#[must_use]
pub fn dyn_value(bytes: &[u8], tag: i64) -> usize {
    let mut at = DYNAMIC as usize;
    loop {
        let t = get_u64(bytes, at) as i64;
        if t == tag {
            return at + 8;
        }
        assert_ne!(t, DT_NULL, "no dynamic entry with tag {tag}");
        at += 16;
    }
}

/// Build the library.
#[must_use]
pub fn build() -> Vec<u8> {
    let mut b = vec![0u8; FILE_LEN];

    // ELF header.
    b[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // ELFDATA2LSB
    b[6] = 1; // EV_CURRENT
    put_u16(&mut b, 16, ET_DYN);
    put_u16(&mut b, 18, EM_AARCH64);
    put_u32(&mut b, 20, 1); // e_version
    put_u64(&mut b, 24, 0); // e_entry
    put_u64(&mut b, 32, PHOFF as u64); // e_phoff
    put_u64(&mut b, 40, 0); // e_shoff
    put_u32(&mut b, 48, 0); // e_flags
    put_u16(&mut b, 52, 64); // e_ehsize
    put_u16(&mut b, 54, PHENTSIZE as u16);
    put_u16(&mut b, 56, PHNUM as u16);
    put_u16(&mut b, 58, 64); // e_shentsize
    put_u16(&mut b, 60, 0); // e_shnum
    put_u16(&mut b, 62, 0); // e_shstrndx

    let put_phdr =
        |b: &mut Vec<u8>, i: usize, ty: u32, flags: u32, off: u64, va: u64, fsz: u64, msz: u64, al: u64| {
            let at = phdr(i);
            put_u32(b, at + P_TYPE, ty);
            put_u32(b, at + P_FLAGS, flags);
            put_u64(b, at + P_OFFSET, off);
            put_u64(b, at + P_VADDR, va);
            put_u64(b, at + P_PADDR, va);
            put_u64(b, at + P_FILESZ, fsz);
            put_u64(b, at + P_MEMSZ, msz);
            put_u64(b, at + P_ALIGN, al);
        };
    let ph_bytes = (PHNUM * PHENTSIZE) as u64;
    put_phdr(&mut b, PH_PHDR, PT_PHDR, PF_R, PHOFF as u64, PHOFF as u64, ph_bytes, ph_bytes, 8);
    put_phdr(
        &mut b,
        PH_LOAD_TEXT,
        PT_LOAD,
        PF_R | PF_X,
        0,
        TEXT_VADDR,
        TEXT_FILESZ,
        TEXT_FILESZ,
        PAGE,
    );
    put_phdr(
        &mut b,
        PH_LOAD_DATA,
        PT_LOAD,
        PF_R | PF_W,
        DATA_VADDR,
        DATA_VADDR,
        DATA_FILESZ,
        DATA_MEMSZ,
        PAGE,
    );
    put_phdr(&mut b, PH_DYNAMIC, PT_DYNAMIC, PF_R | PF_W, DYNAMIC, DYNAMIC, 0x100, 0x100, 8);
    put_phdr(&mut b, PH_RELRO, PT_GNU_RELRO, PF_R, RELRO_VADDR, RELRO_VADDR, RELRO_MEMSZ, RELRO_MEMSZ, 1);

    // .dynstr
    b[STRTAB as usize..STRTAB as usize + STRINGS.len()].copy_from_slice(STRINGS);

    // .dynsym: index 0 is the reserved null entry.
    let put_sym = |b: &mut Vec<u8>, i: u32, name: u64, info: u8, shndx: u16, value: u64, size: u64| {
        let at = sym(i);
        put_u32(b, at, name as u32);
        b[at + 4] = info;
        b[at + 5] = 0;
        put_u16(b, at + 6, shndx);
        put_u64(b, at + 8, value);
        put_u64(b, at + 16, size);
    };
    // STB_GLOBAL | STT_FUNC, undefined: the one import.
    put_sym(&mut b, SYM_IMPORTED_FUNC, STR_IMPORTED_FUNC, 0x12, 0, 0, 0);
    // STB_GLOBAL | STT_OBJECT, defined in the writable segment.
    put_sym(&mut b, SYM_LOCAL_DATA, STR_LOCAL_DATA, 0x11, 1, LOCAL_DATA, 8);

    // DT_HASH: one bucket, three chain slots. `nchain` is the symbol count by construction, which
    // is the only thing the loader needs from it.
    let h = HASH as usize;
    put_u32(&mut b, h, 1); // nbucket
    put_u32(&mut b, h + 4, SYMCOUNT); // nchain
    put_u32(&mut b, h + 8, SYM_LOCAL_DATA); // bucket[0]
    put_u32(&mut b, h + 12, 0); // chain[0]
    put_u32(&mut b, h + 16, 0); // chain[1]
    put_u32(&mut b, h + 20, SYM_IMPORTED_FUNC); // chain[2]

    // .rela.dyn — note every target's file content stays zero, exactly as in the real library.
    let put_rela = |b: &mut Vec<u8>, at: usize, offset: u64, ty: u32, symndx: u32, addend: i64| {
        put_u64(b, at, offset);
        put_u64(b, at + 8, (u64::from(symndx) << 32) | u64::from(ty));
        put_u64(b, at + 16, addend as u64);
    };
    put_rela(&mut b, rela(RELA_INIT_0), INIT_ARRAY, R_AARCH64_RELATIVE, 0, INIT_FN_0 as i64);
    put_rela(&mut b, rela(RELA_INIT_1), INIT_ARRAY + 8, R_AARCH64_RELATIVE, 0, INIT_FN_1 as i64);
    put_rela(&mut b, rela(RELA_GOT_0), GOT, R_AARCH64_RELATIVE, 0, LOCAL_DATA as i64);
    put_rela(&mut b, rela(RELA_GOT_1), GOT + 8, R_AARCH64_RELATIVE, 0, 0x100);
    put_rela(
        &mut b,
        rela(RELA_GLOB_DAT),
        GOT + 16,
        R_AARCH64_GLOB_DAT,
        SYM_IMPORTED_FUNC,
        0,
    );
    put_rela(&mut b, rela(RELA_ABS64), GOT + 24, R_AARCH64_ABS64, SYM_IMPORTED_FUNC, 8);

    // .rela.plt
    put_rela(&mut b, jmprel(0), GOT_PLT, R_AARCH64_JUMP_SLOT, SYM_IMPORTED_FUNC, 0);
    put_rela(&mut b, jmprel(1), GOT_PLT + 8, R_AARCH64_JUMP_SLOT, SYM_LOCAL_DATA, 0);

    // PT_DYNAMIC
    let entries: &[(i64, u64)] = &[
        (DT_HASH, HASH),
        (DT_STRTAB, STRTAB),
        (DT_STRSZ, STRINGS.len() as u64),
        (DT_SYMTAB, SYMTAB),
        (DT_SYMENT, 24),
        (DT_SONAME, STR_SONAME),
        (DT_RELA, RELA),
        (DT_RELASZ, (RELA_COUNT * 24) as u64),
        (DT_RELAENT, 24),
        (DT_JMPREL, JMPREL),
        (DT_PLTRELSZ, (JMPREL_COUNT * 24) as u64),
        (DT_PLTREL, DT_RELA as u64),
        (DT_INIT_ARRAY, INIT_ARRAY),
        (DT_INIT_ARRAYSZ, (INIT_ARRAY_COUNT * 8) as u64),
        (DT_NULL, 0),
    ];
    let mut at = DYNAMIC as usize;
    for &(tag, value) in entries {
        put_u64(&mut b, at, tag as u64);
        put_u64(&mut b, at + 8, value);
        at += 16;
    }
    assert!(at <= DYNAMIC as usize + 0x100, "PT_DYNAMIC overflowed its declared p_filesz");

    // A recognisable byte at each "function" the init_array points at, so a test can prove the
    // collected pointer really addresses this and not something else.
    b[INIT_FN_0 as usize] = 0xa1;
    b[INIT_FN_1 as usize] = 0xa2;
    b
}

/// A synthetic library written to a temporary file, removed on drop.
pub struct SynthFile {
    dir: PathBuf,
    path: PathBuf,
    pub bytes: Vec<u8>,
}

impl SynthFile {
    /// Write `bytes` to a uniquely named temporary file.
    pub fn new(label: &str, bytes: Vec<u8>) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "omni-elf-synth-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create the synthetic library directory");
        // Labels are prose, so anything Windows will not accept in a file name is folded away.
        let safe: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let path = dir.join(format!("{safe}.so"));
        std::fs::write(&path, &bytes).expect("write the synthetic library");
        Self { dir, path, bytes }
    }

    /// The pristine library.
    pub fn pristine() -> Self {
        Self::new("libsynth", build())
    }

    /// A tampered library: `edit` gets the pristine bytes to modify before they are written.
    pub fn tampered(label: &str, edit: impl FnOnce(&mut Vec<u8>)) -> Self {
        let mut bytes = build();
        edit(&mut bytes);
        Self::new(label, bytes)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SynthFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
