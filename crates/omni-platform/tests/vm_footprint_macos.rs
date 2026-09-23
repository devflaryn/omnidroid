//! What the virtual-memory seam's operations **cost** on macOS, measured as `phys_footprint` --
//! the memory the kernel charges this process for (see `src/vm/macos.rs`). One test, so that
//! nothing else in this binary allocates while it measures.
//!
//! The properties asserted are the owner's memory requirement, in this host's terms: address space
//! is free, making memory accessible is free, only touching it costs, decommit gives it back, and a
//! read-only file view costs the process nothing of its own (its pages belong to the file cache and
//! are shared with every other mapper).
#![cfg(target_os = "macos")]

use omni_platform::vm::{self, MapExecutability, Protection};

const MIB: u64 = 1 << 20;

fn footprint() -> u64 {
    vm::process_commit_charge().expect("phys_footprint")
}

fn touch(base: *mut u8, len: usize) {
    let page = vm::page_size();
    for offset in (0..len).step_by(page) {
        // SAFETY: the caller made `[base, base + len)` read-write.
        unsafe { base.add(offset).write_volatile(1) };
    }
}

#[test]
fn memory_is_charged_on_touch_only_and_decommit_returns_it() {
    let before = footprint();
    let space = vm::reserve(16 << 30, vm::page_size()).expect("16 GiB of address space");
    let reserved = footprint();
    assert!(reserved.saturating_sub(before) < MIB, "reserving 16 GiB cost {}", reserved - before);

    let len = 256 << 20;
    // SAFETY: inside the reservation this test owns.
    unsafe { vm::commit(space.as_ptr(), len, Protection::ReadWrite) }.expect("commit 256 MiB");
    let committed = footprint();
    assert!(committed.saturating_sub(reserved) < MIB, "commit cost {}", committed - reserved);

    touch(space.as_ptr(), len);
    let touched = footprint();
    let charged = touched.saturating_sub(committed);
    assert!(
        charged >= 250 * MIB && charged <= 260 * MIB,
        "touching 256 MiB charged {} MiB",
        charged / MIB
    );

    // SAFETY: as above.
    unsafe { vm::decommit(space.as_ptr(), len) }.expect("decommit");
    let decommitted = footprint();
    let returned = touched.saturating_sub(decommitted);
    assert!(returned >= 250 * MIB, "decommit returned only {} MiB", returned / MIB);
    eprintln!(
        "phys_footprint MiB: before {:.2}, +16 GiB reserved {:.2}, +256 MiB committed {:.2}, \
         touched {:.2}, decommitted {:.2}",
        before as f64 / MIB as f64,
        reserved as f64 / MIB as f64,
        committed as f64 / MIB as f64,
        touched as f64 / MIB as f64,
        decommitted as f64 / MIB as f64
    );
    vm::release(space).expect("release");

    // A read-only file view: its pages are the file's, clean and shared, and charge nothing here.
    let dir = std::env::temp_dir().join("omnidroid-vm-tests");
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join(format!("footprint-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x5Au8; 64 << 20]).expect("64 MiB file");
    let file = vm::open_file_for_mapping(&path, MapExecutability::Executable).expect("open");
    let view = vm::reserve_placeholder(64 << 20, vm::page_size()).expect("placeholder");
    let quiet = footprint();
    // SAFETY: an exact-size placeholder this test owns.
    unsafe {
        vm::map_file(&file, 0, 64 << 20, view.as_ptr(), Protection::ReadExecute).expect("map");
        let mut sum = 0u64;
        for offset in (0..64 << 20).step_by(vm::page_size()) {
            sum += u64::from(view.as_ptr().add(offset).read_volatile());
        }
        assert_eq!(sum, 0x5A * (64 << 20) as u64 / vm::page_size() as u64);
        let read = footprint();
        assert!(
            read.saturating_sub(quiet) < 2 * MIB,
            "reading a 64 MiB read-only view charged {} bytes",
            read.saturating_sub(quiet)
        );
        vm::unmap_and_release(view.as_ptr(), 64 << 20).expect("unmap");
    }
    let _ = std::fs::remove_file(&path);
}
