//! A private writable mapping of an image file (`mmap(PROT_READ | PROT_WRITE, MAP_PRIVATE)` of a
//! sysroot file -- how ART maps an uncompressed boot image, `crate::boot_image`) is a view of the
//! host file, and a write copies **only the page written**: the rest stay the file's pages, shared
//! with every other mapping of the file, in this host process or another.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};
use sha2::{Digest, Sha256};

const PAGES: usize = 64;

#[test]
fn a_write_to_a_private_file_mapping_copies_one_page() {
    // A sysroot of one 256 KiB file, every page holding its own number.
    let dir = std::env::temp_dir().join(format!("omni-file-cow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bytes: Vec<u8> = (0..PAGES * 4096).map(|i| (i / 4096) as u8 + 1).collect();
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let object = dir.join("objects").join(&sha[..2]).join(&sha);
    std::fs::create_dir_all(object.parent().unwrap()).unwrap();
    std::fs::write(&object, &bytes).unwrap();
    let m = manifest::parse(&format!("d\t755\t/\nf\t644\t{}\t{sha}\t/image.art\n", bytes.len())).unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = Task::new(4001, Arc::clone(&p));
    let path = p.scratch();
    p.mem.write(path, b"/image.art\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, path, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0, "open: {}", fd as i64);
    let len = (PAGES * 4096) as u64;
    let at = p.syscall(&mut t, nr::MMAP, [0, len, 3, 2, fd, 0]); // PROT_READ|PROT_WRITE, MAP_PRIVATE
    assert!((at as i64) > 0, "mmap: {}", at as i64);
    let region = p.mem.space().region_at(at as usize).expect("mapped");
    assert!(matches!(region.kind, omni_mem::RegionKind::File { .. }), "a view of the file, not a private copy: {region:?}");

    // Every page read (resident), one byte of page 7 written.
    let all = p.mem.read(at, len as usize).unwrap();
    assert_eq!(all, bytes);
    p.mem.write(at + 7 * 4096 + 100, &[0xEE]).unwrap();
    let now = p.mem.read(at, len as usize).unwrap();
    for page in 0..PAGES {
        let want = page as u8 + 1;
        let got = &now[page * 4096..(page + 1) * 4096];
        if page == 7 {
            assert_eq!(got[100], 0xEE);
            assert!(got.iter().enumerate().all(|(i, &b)| i == 100 || b == want));
        } else {
            assert!(got.iter().all(|&b| b == want), "page {page}");
        }
    }
    assert_eq!(std::fs::read(&object).unwrap(), bytes, "the host file is not written");

    // On Windows, what of the mapping is this process's own: the page written, nothing more.
    if cfg!(windows) {
        let set = omni_platform::vm::resident_set().expect("the working set");
        let r = set.in_range(p.mem.space().host_addr(at as usize), len as usize).unwrap();
        assert!(r.resident >= 60 * 4096, "the pages read are resident: {r:?}");
        assert!(r.private <= 2 * 4096, "only the written page is private: {r:?}");
        eprintln!("{} KiB resident, {} KiB of it private", r.resident >> 10, r.private >> 10);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// ART's way: the whole boot image range reserved (`PROT_NONE`), then each component mapped
/// `MAP_FIXED` into it at its own address -- 16 KiB-aligned, not 64 KiB (`boot-okhttp.art` at
/// 0x70328000). Each must still be a view.
#[test]
fn a_fixed_mapping_into_a_reservation_at_16_kib_is_a_view() {
    let dir = std::env::temp_dir().join(format!("omni-file-cow-fixed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bytes: Vec<u8> = (0..PAGES * 4096).map(|i| (i / 4096) as u8 + 1).collect();
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let object = dir.join("objects").join(&sha[..2]).join(&sha);
    std::fs::create_dir_all(object.parent().unwrap()).unwrap();
    std::fs::write(&object, &bytes).unwrap();
    let m = manifest::parse(&format!("d\t755\t/\nf\t644\t{}\t{sha}\t/image.art\n", bytes.len())).unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = Task::new(4002, Arc::clone(&p));
    let path = p.scratch();
    p.mem.write(path, b"/image.art\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, path, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    let len = (PAGES * 4096) as u64;
    // The reservation: PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, 1 MiB.
    let reserved = p.syscall(&mut t, nr::MMAP, [0, 1 << 20, 0, 0x22, u64::MAX, 0]);
    assert!((reserved as i64) > 0);
    for (k, offset_in) in [0x4000u64, 0x4_8000].into_iter().enumerate() {
        let want = reserved + offset_in;
        let at = p.syscall(&mut t, nr::MMAP, [want, len, 3, 0x12, fd, 0]); // MAP_PRIVATE | MAP_FIXED
        assert_eq!(at, want, "mapping {k} where asked");
        let region = p.mem.space().region_at(at as usize).expect("mapped");
        assert!(matches!(region.kind, omni_mem::RegionKind::File { .. }), "mapping {k} at {at:#x}: a view: {region:?}");
        p.mem.write(at + 3 * 4096, &[0x5A]).unwrap();
        assert_eq!(p.mem.read(at + 3 * 4096, 2).unwrap(), vec![0x5A, 4]);
        assert_eq!(p.mem.read(at + 4 * 4096, 1).unwrap(), vec![5]);
    }
    assert_eq!(std::fs::read(&object).unwrap(), bytes, "the host file is not written");
    let _ = std::fs::remove_dir_all(&dir);
}
