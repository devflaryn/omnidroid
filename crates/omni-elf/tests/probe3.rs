mod common;
#[test]
fn probe() {
    let Some(bytes) = common::cached_main_lib_bytes() else { return };
    let elf = omni_elf::ElfImage::parse(bytes).unwrap();
    let t = elf.relocations().unwrap();
    for table in t.general.iter().chain(t.plt.iter()) {
        let mut desc = 0usize;
        let mut prev = 0u64;
        let mut dup = 0usize;
        for r in &table.relocations {
            if r.r_offset < prev { desc += 1 }
            if r.r_offset == prev { dup += 1 }
            prev = r.r_offset;
        }
        println!("{}: {} entries, {} descents, {} duplicates", table.tag, table.relocations.len(), desc, dup);
    }
}
