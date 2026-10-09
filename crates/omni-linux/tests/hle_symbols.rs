//! `crate::hle`'s entries resolve, by name, in the real `libc.so` of the pinned sysroot: the
//! `memcpy`/`memmove`/`memset` implementations bionic's ifunc resolvers can pick are local symbols
//! in `.symtab`, found without a hardcoded address. Skipped (passing) without a sysroot.
mod common;

use omni_linux::guestprof::Elf;

#[test]
fn the_libc_entries_resolve_by_name_in_the_real_libc() {
    let Some(dir) = common::sysroot() else { return };
    let sysroot = omni_linux::vfs::Sysroot::open(&dir).expect("the sysroot");
    let host = sysroot.host_path(b"/apex/com.android.runtime/lib64/bionic/libc.so").expect("libc.so in the sysroot");
    let elf = Elf::parse(&mut std::fs::File::open(&host).expect("open libc.so"), 0).expect("an ELF");
    for (names, _) in omni_linux::hle::ENTRIES {
        let vaddr = omni_linux::hle::find(&elf, names).unwrap_or_else(|| panic!("{names:?} is in libc.so's symbol table"));
        let off = elf.file_of(vaddr).unwrap_or_else(|| panic!("{names:?} at {vaddr:#x} is in a PT_LOAD"));
        let len = std::fs::metadata(&host).expect("stat").len();
        assert!(off < len, "{names:?}: file offset {off:#x} inside the file ({len:#x})");
    }
}
