//! `ProgramHeaders`: what a kernel reads of an ELF before mapping it.
use omni_elf::consts::{PT_INTERP, PT_LOAD, PT_TLS};
use omni_elf::ProgramHeaders;

/// A 64-bit little-endian AArch64 ET_DYN with three program headers and an interpreter string.
fn elf_with(interp: &[u8]) -> Vec<u8> {
    let phoff = 64u64;
    let interp_off = phoff + 3 * 56;
    let mut f = vec![0u8; interp_off as usize + interp.len() + 1];
    f[..4].copy_from_slice(b"\x7fELF");
    f[4] = 2; // ELFCLASS64
    f[5] = 1; // little endian
    f[6] = 1; // EV_CURRENT
    f[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    f[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    f[20..24].copy_from_slice(&1u32.to_le_bytes());
    f[24..32].copy_from_slice(&0x1000u64.to_le_bytes()); // e_entry
    f[32..40].copy_from_slice(&phoff.to_le_bytes());
    f[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    f[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    f[56..58].copy_from_slice(&3u16.to_le_bytes()); // e_phnum
    let ph = |f: &mut Vec<u8>, i: usize, p_type: u32, off: u64, vaddr: u64, filesz: u64, memsz: u64| {
        let at = phoff as usize + i * 56;
        f[at..at + 4].copy_from_slice(&p_type.to_le_bytes());
        f[at + 4..at + 8].copy_from_slice(&4u32.to_le_bytes()); // PF_R
        f[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
        f[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
        f[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
        f[at + 40..at + 48].copy_from_slice(&memsz.to_le_bytes());
        f[at + 48..at + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    };
    let len = f.len() as u64;
    ph(&mut f, 0, PT_INTERP, interp_off, interp_off, interp.len() as u64 + 1, interp.len() as u64 + 1);
    ph(&mut f, 1, PT_LOAD, 0, 0, len, len);
    ph(&mut f, 2, PT_TLS, 0, 0, 0, 16);
    f[interp_off as usize..interp_off as usize + interp.len()].copy_from_slice(interp);
    f
}

#[test]
fn a_tls_segment_and_no_dynamic_segment_are_accepted() {
    let data = elf_with(b"/system/bin/linker64");
    let ph = ProgramHeaders::parse(&data).expect("program headers");
    assert_eq!(ph.segments.len(), 3);
    assert_eq!(ph.header.e_entry, 0x1000);
    assert!(ph.segments.iter().any(|s| s.p_type == PT_TLS));
}

#[test]
fn the_interpreter_is_read_without_its_nul() {
    let data = elf_with(b"/system/bin/linker64");
    let ph = ProgramHeaders::parse(&data).expect("program headers");
    assert_eq!(ph.interpreter(&data).expect("readable"), Some(&b"/system/bin/linker64"[..]));
}

#[test]
fn a_truncated_header_table_is_an_error_not_a_panic() {
    let data = elf_with(b"/x");
    assert!(ProgramHeaders::parse(&data[..100]).is_err());
}
