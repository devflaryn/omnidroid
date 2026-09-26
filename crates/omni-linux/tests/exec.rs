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
