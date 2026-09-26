//! What the kernel's `load_elf_binary` does for a PIE and its interpreter, and nothing more:
//! relocation, TLS and constructors are `linker64`'s.
use omni_elf::consts::{PT_LOAD, PT_PHDR};
use omni_elf::ProgramHeaders;

use crate::errno::*;
use crate::fd;
use crate::mm::MapRequest;
use crate::process::{Process, Task};

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_PLATFORM: u64 = 15;
pub const AT_HWCAP: u64 = 16;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_HWCAP2: u64 = 26;
pub const AT_EXECFN: u64 = 31;

/// FP, ASIMD, AES, PMULL, SHA1, SHA2, CRC32 -- and not ATOMICS (bit 8): D26 declines LSE.
pub const HWCAP: u64 = 0xFB;

const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const PROT_EXEC: u32 = 4;
const MAP_PRIVATE: u32 = 2;
const MAP_FIXED: u32 = 0x10;
const MAP_ANONYMOUS: u32 = 0x20;

#[derive(Debug, Clone)]
pub struct LoadedElf {
    pub bias: u64,
    pub entry: u64,
    pub phdr: u64,
    pub phnum: u64,
    pub interp: Option<Vec<u8>>,
}


pub fn load_elf(p: &Process, t: &Task, path: &[u8]) -> Result<LoadedElf, Errno> {
    let file = std::sync::Arc::new(fd::open(&p.vfs, b"/", path, 0)?);
    let len = fd::stat_of(&file)?.size as usize;
    let mut data = vec![0u8; len];
    fd::pread_all(&file, &mut data, 0)?;
    let ph = ProgramHeaders::parse(&data).map_err(|_| Errno(8))?; // ENOEXEC
    let interp = ph.interpreter(&data).map_err(|_| Errno(8))?.map(<[u8]>::to_vec);
    let loads: Vec<_> = ph.segments.iter().filter(|s| s.p_type == PT_LOAD).collect();
    if loads.is_empty() {
        return Err(Errno(8));
    }
    // The kernel's page, which is the host's (`Mm::page_size`): an ELF whose segments are aligned
    // below it cannot be mapped, and `mmap` refuses its offsets, as a 16 KiB kernel refuses them.
    let page = p.mm.page_size();
    let floor = |v: u64| v & !(page - 1);
    let ceil = |v: u64| (v + page - 1) & !(page - 1);
    let lo = floor(loads.iter().map(|s| s.p_vaddr).min().expect("non-empty"));
    let hi = ceil(loads.iter().map(|s| s.p_vaddr + s.p_memsz).max().expect("non-empty"));
    let fd_num = p.fds.insert(std::sync::Arc::clone(&file), true, 0)?;
    // Reserve the whole span first, as the kernel does, so the segments land at fixed offsets.
    let base = p.mm.map(p, t, MapRequest { addr: 0, len: hi - lo, prot: 0, flags: MAP_PRIVATE | MAP_ANONYMOUS, fd: -1, offset: 0 })?;
    let bias = base - lo;
    for s in &loads {
        let prot = (if s.p_flags.bits() & 4 != 0 { PROT_READ } else { 0 })
            | (if s.p_flags.bits() & 2 != 0 { PROT_WRITE } else { 0 })
            | (if s.p_flags.bits() & 1 != 0 { PROT_EXEC } else { 0 });
        if s.p_offset % page != s.p_vaddr % page {
            // Aligned below the page: no view of the file puts `p_offset` at `p_vaddr`.
            return Err(EINVAL);
        }
        let start = floor(s.p_vaddr);
        let file_end = s.p_vaddr + s.p_filesz;
        let mapped = ceil(file_end) - start;
        if s.p_filesz > 0 {
            p.mm.map(p, t, MapRequest { addr: bias + start, len: mapped, prot, flags: MAP_PRIVATE | MAP_FIXED, fd: fd_num, offset: floor(s.p_offset) })?;
        }
        let mem_end = ceil(s.p_vaddr + s.p_memsz);
        if s.p_memsz > s.p_filesz {
            // The bss: zero the rest of the last file page, then map anonymous pages beyond it.
            let tail = ceil(file_end) - file_end;
            if tail > 0 && s.p_filesz > 0 {
                p.mem.write(bias + file_end, &vec![0u8; tail as usize])?;
            }
            if mem_end > ceil(file_end) {
                p.mm.map(p, t, MapRequest { addr: bias + ceil(file_end), len: mem_end - ceil(file_end), prot, flags: MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, fd: -1, offset: 0 })?;
            }
        }
    }
    p.fds.remove(fd_num)?;
    let phdr = match ph.segments.iter().find(|s| s.p_type == PT_PHDR) {
        Some(s) => bias + s.p_vaddr,
        None => {
            let s = loads.iter().find(|s| s.p_offset <= ph.header.e_phoff && ph.header.e_phoff < s.p_offset + s.p_filesz).ok_or(Errno(8))?;
            bias + s.p_vaddr + (ph.header.e_phoff - s.p_offset)
        }
    };
    Ok(LoadedElf { bias, entry: bias + ph.header.e_entry, phdr, phnum: u64::from(ph.header.e_phnum), interp })
}

pub fn build_stack(top: u64, argv: &[Vec<u8>], envp: &[Vec<u8>], auxv: &[(u64, u64)], random: [u8; 16], execfn: &[u8]) -> (Vec<u8>, u64) {
    // Strings, laid out downward from `top`; record each one's address.
    let mut strings: Vec<u8> = Vec::new();
    let place = |s: &[u8], strings: &mut Vec<u8>| -> u64 {
        let mut with_nul = s.to_vec();
        with_nul.push(0);
        let mut next = with_nul;
        next.extend_from_slice(strings);
        *strings = next;
        top - strings.len() as u64
    };
    let platform = place(b"aarch64", &mut strings);
    let execfn_at = place(execfn, &mut strings);
    let env_at: Vec<u64> = envp.iter().rev().map(|e| place(e, &mut strings)).collect::<Vec<_>>().into_iter().rev().collect();
    let arg_at: Vec<u64> = argv.iter().rev().map(|a| place(a, &mut strings)).collect::<Vec<_>>().into_iter().rev().collect();
    let mut with_random = random.to_vec();
    with_random.extend_from_slice(&strings);
    strings = with_random;
    let random_at = top - strings.len() as u64;

    let mut aux: Vec<(u64, u64)> = auxv.to_vec();
    aux.extend([(AT_RANDOM, random_at), (AT_EXECFN, execfn_at), (AT_PLATFORM, platform), (AT_NULL, 0)]);
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * aux.len();
    let strings_start = top - strings.len() as u64;
    let sp = (strings_start - (words as u64) * 8) & !15;
    let mut vector: Vec<u64> = Vec::with_capacity(words);
    vector.push(argv.len() as u64);
    vector.extend(&arg_at);
    vector.push(0);
    vector.extend(&env_at);
    vector.push(0);
    for (k, v) in aux {
        vector.push(k);
        vector.push(v);
    }
    let mut bytes: Vec<u8> = vector.iter().flat_map(|w| w.to_le_bytes()).collect();
    bytes.resize((strings_start - sp) as usize, 0); // the alignment gap between vector and strings
    bytes.extend_from_slice(&strings);
    (bytes, sp)
}
