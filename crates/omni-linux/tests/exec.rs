use omni_linux::exec::{build_stack, AT_EXECFN, AT_NULL, AT_PAGESZ, AT_PLATFORM, AT_RANDOM};

fn u64_at(bytes: &[u8], base: u64, addr: u64) -> u64 {
    let i = (addr - base) as usize;
    u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap())
}

fn cstr_at(bytes: &[u8], base: u64, addr: u64) -> Vec<u8> {
    let i = (addr - base) as usize;
    bytes[i..].split(|&b| b == 0).next().unwrap().to_vec()
}

#[test]
fn the_initial_stack_is_the_kernels_layout() {
    let top = 0x7000_0000u64;
    let argv = vec![b"/system/bin/toybox".to_vec(), b"echo".to_vec(), b"hello".to_vec()];
    let envp = vec![b"PATH=/system/bin".to_vec()];
    let (bytes, sp) = build_stack(top, &argv, &envp, &[(AT_PAGESZ, 4096)], [7; 16], b"/system/bin/toybox");
    let base = top - bytes.len() as u64;
    assert_eq!(sp % 16, 0, "sp is 16-byte aligned");
    assert_eq!(sp, base, "the vector starts the image");
    assert_eq!(u64_at(&bytes, base, sp), 3, "argc");
    assert_eq!(cstr_at(&bytes, base, u64_at(&bytes, base, sp + 8)), b"/system/bin/toybox");
    assert_eq!(cstr_at(&bytes, base, u64_at(&bytes, base, sp + 24)), b"hello");
    assert_eq!(u64_at(&bytes, base, sp + 32), 0, "argv NULL");
    assert_eq!(cstr_at(&bytes, base, u64_at(&bytes, base, sp + 40)), b"PATH=/system/bin");
    assert_eq!(u64_at(&bytes, base, sp + 48), 0, "envp NULL");
    let mut aux = Vec::new();
    let mut at = sp + 56;
    loop {
        let (k, v) = (u64_at(&bytes, base, at), u64_at(&bytes, base, at + 8));
        aux.push((k, v));
        at += 16;
        if k == AT_NULL {
            break;
        }
    }
    assert_eq!(aux[0], (AT_PAGESZ, 4096));
    let random = aux.iter().find(|(k, _)| *k == AT_RANDOM).unwrap().1;
    assert_eq!(&bytes[(random - base) as usize..(random - base) as usize + 16], &[7; 16]);
    assert_eq!(cstr_at(&bytes, base, aux.iter().find(|(k, _)| *k == AT_EXECFN).unwrap().1), b"/system/bin/toybox");
    assert_eq!(cstr_at(&bytes, base, aux.iter().find(|(k, _)| *k == AT_PLATFORM).unwrap().1), b"aarch64");
    assert!(bytes.len() as u64 + base == top);
}

/// A one-segment arm64 PIE whose `PT_LOAD` has the given file offset and address.
fn elf_with_one_load(p_offset: u64, p_vaddr: u64) -> Vec<u8> {
    let mut e = vec![0u8; 0x2000];
    e[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    e[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    e[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e[20..24].copy_from_slice(&1u32.to_le_bytes());
    e[24..32].copy_from_slice(&p_vaddr.to_le_bytes()); // e_entry
    e[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    e[52..54].copy_from_slice(&64u16.to_le_bytes());
    e[54..56].copy_from_slice(&56u16.to_le_bytes());
    e[56..58].copy_from_slice(&1u16.to_le_bytes());
    let ph = &mut e[64..120];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    ph[4..8].copy_from_slice(&5u32.to_le_bytes()); // R+X
    ph[8..16].copy_from_slice(&p_offset.to_le_bytes());
    ph[16..24].copy_from_slice(&p_vaddr.to_le_bytes());
    ph[24..32].copy_from_slice(&p_vaddr.to_le_bytes());
    ph[32..40].copy_from_slice(&0x100u64.to_le_bytes());
    ph[40..48].copy_from_slice(&0x100u64.to_le_bytes());
    ph[48..56].copy_from_slice(&0x1000u64.to_le_bytes());
    e
}

fn load(elf: &[u8]) -> Result<omni_linux::exec::LoadedElf, omni_linux::errno::Errno> {
    use omni_linux::{fd::Output, manifest, process::Process, vfs::{Sysroot, Vfs}};
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("omni-linux-exec-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("objects/cc")).unwrap();
    std::fs::write(dir.join("objects/cc/cc01"), elf).unwrap();
    let m = manifest::parse(&format!("d\t755\t/\nd\t755\t/bin\nf\t755\t{}\tcc01\t/bin/x\n", elf.len())).unwrap();
    let p = Process::for_tests(Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/bin/x".to_vec()), Output::Capture(Default::default()));
    let t = p.test_task();
    omni_linux::exec::load_elf(&p, &t, b"/bin/x")
}

#[test]
fn a_segment_whose_offset_and_address_agree_within_a_page_loads() {
    let loaded = load(&elf_with_one_load(0x10, 0x10)).expect("loads");
    assert_eq!(loaded.entry, loaded.bias + 0x10);
}

#[test]
fn a_segment_whose_offset_and_address_disagree_within_the_page_is_refused() {
    // As the kernel refuses it: the page holding the address cannot be a view of the file.
    // 0x800 apart disagrees at every page size a host has.
    assert_eq!(load(&elf_with_one_load(0, 0x800)).err(), Some(omni_linux::errno::EINVAL));
}
