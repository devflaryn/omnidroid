//! A private mapping of an image file is a view of the host file wherever a view can be (D42): at a
//! 4 KiB file offset that is not a host-page one (framework.jar's dex files, framework-res.apk's
//! resources.arsc, on a 16 KiB host), at a fixed address that agrees with its offset modulo the host
//! page, and over the file's last, partial host page where the host allows it. Its bytes, its
//! privacy, and what `munmap`, `mprotect`, `mremap` and the maps' names do to it are those of a
//! copy; only what it costs differs. On a 4 KiB host every one of these was a view already. Where
//! the guest's page is not 4 KiB (Linux on 16 or 64 KiB pages: no 4 KiB overlay without
//! `vm::supports_alias`, D42) every 4 KiB offset and address here is EINVAL: skipped.
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}, Task};
use omni_mem::RegionKind;
use sha2::{Digest, Sha256};

const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

/// A sysroot of one file, `/data.bin`: six 16 KiB pages and 5000 bytes, each 4 KiB page holding its
/// own number (from 1), and a process with it open; `None` where the guest's page is not 4 KiB.
struct Fixture {
    dir: std::path::PathBuf,
    object: std::path::PathBuf,
    bytes: Vec<u8>,
    p: Arc<Process>,
    t: Task,
    fd: u64,
}

impl Fixture {
    fn new(name: &str, tid: i32) -> Option<Self> {
        Self::of_len(name, tid, 6 * 16384 + 5000)
    }

    fn of_len(name: &str, tid: i32, len: usize) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!("omni-file-views-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bytes: Vec<u8> = (0..len).map(|i| (i / 4096) as u8 + 1).collect();
        let sha = format!("{:x}", Sha256::digest(&bytes));
        let object = dir.join("objects").join(&sha[..2]).join(&sha);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, &bytes).unwrap();
        let m = manifest::parse(&format!("d\t755\t/\nf\t644\t{}\t{sha}\t/data.bin\n", bytes.len())).unwrap();
        let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/x".to_vec());
        let p = Process::for_tests(vfs, Output::Capture(Default::default()));
        if p.mm.page_size() != PAGE {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        let mut t = Task::new(tid, Arc::clone(&p));
        let path = p.scratch();
        p.mem.write(path, b"/data.bin\0").unwrap();
        let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, path, 0, 0, 0, 0]);
        assert!((fd as i64) >= 0, "open: {}", fd as i64);
        Some(Self { dir, object, bytes, p, t, fd })
    }

    fn host_page(&self) -> u64 {
        self.p.mem.space().page_size() as u64
    }

    fn map(&mut self, addr: u64, len: u64, prot: u64, flags: u64, offset: u64) -> u64 {
        let at = self.p.syscall(&mut self.t, nr::MMAP, [addr, len, prot, flags, self.fd, offset]);
        assert!((at as i64) > 0, "mmap {addr:#x}+{len:#x} offset {offset:#x}: {}", at as i64);
        at
    }

    fn reserve(&mut self, len: u64) -> u64 {
        let at = self.p.syscall(&mut self.t, nr::MMAP, [0, len, 0, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX, 0]);
        assert!((at as i64) > 0);
        at
    }

    fn file_at(&self, at: u64) -> bool {
        let r = self.p.mem.space().region_at(at as usize).expect("mapped");
        matches!(r.kind, RegionKind::File { .. })
    }

    /// The file's bytes `[offset, offset + len)`, zeros past its end.
    fn want(&self, offset: u64, len: u64) -> Vec<u8> {
        (offset..offset + len).map(|i| self.bytes.get(i as usize).copied().unwrap_or(0)).collect()
    }

    fn check(&self, at: u64, offset: u64, len: u64) {
        assert_eq!(self.p.mem.read(at, len as usize).unwrap(), self.want(offset, len), "{at:#x}+{len:#x} from offset {offset:#x}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A test's own directory, gone when the test is, passed or not.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("omni-file-views-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// framework.jar's way: `mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, offset & ~0xfff)` at an offset
/// that is not a host page's. A view placed so the address agrees with the offset modulo the host
/// page, named as the guest mapped it.
#[test]
fn a_private_mapping_at_a_4k_offset_is_a_view() {
    let Some(mut f) = Fixture::new("offset", 4101) else { return };
    let before = f.p.mem.space().stats().file_backed;
    let at = f.map(0, 8 * PAGE, PROT_READ, MAP_PRIVATE, PAGE);
    assert_eq!(at % f.host_page(), PAGE % f.host_page(), "placed to agree with the offset: {at:#x}");
    f.check(at, PAGE, 8 * PAGE);
    assert!(f.file_at(at) && f.file_at(at + 7 * PAGE), "a view, not a private copy");
    assert!(f.p.mem.space().stats().file_backed >= before + (8 * PAGE) as usize, "file-backed");
    assert_eq!(f.p.mm.describe(at + 0x10).as_deref(), Some("/data.bin+0x1010"));
    assert_eq!(f.p.mm.name_at(at + 7 * PAGE), Some((b"/data.bin".to_vec(), 8 * PAGE)));
    // The guest's mapping ends where it asked: the next 4 KiB is not its.
    assert!(f.p.mm.describe(at + 8 * PAGE).is_none());
    assert!(f.p.mm.describe(at - PAGE).is_none());
}

/// A write to a private view copies its host page: the mapping sees it, the file and every other
/// mapping of it do not.
#[test]
fn a_write_to_a_private_view_at_a_4k_offset_stays_private() {
    let Some(mut f) = Fixture::new("write", 4102) else { return };
    let at = f.map(0, 6 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, 3 * PAGE);
    assert!(f.file_at(at));
    f.p.mem.write(at + 0x10, &[0xEE]).unwrap();
    let mut want = f.want(3 * PAGE, 6 * PAGE);
    want[0x10] = 0xEE;
    assert_eq!(f.p.mem.read(at, 6 * PAGE as usize).unwrap(), want);
    let other = f.map(0, 6 * PAGE, PROT_READ, MAP_PRIVATE, 3 * PAGE);
    f.check(other, 3 * PAGE, 6 * PAGE);
    assert_eq!(std::fs::read(&f.object).unwrap(), f.bytes, "the host file is not written");
}

/// `MAP_FIXED` at an address that agrees with the offset, where the host pages around it hold
/// nothing of the guest's: a view with 4 KiB holes before and after.
#[test]
fn a_fixed_mapping_that_agrees_with_its_offset_is_a_view() {
    let Some(mut f) = Fixture::new("fixed", 4103) else { return };
    let host = f.host_page();
    let free = f.reserve(1 << 20);
    assert_eq!(f.p.syscall(&mut f.t, nr::MUNMAP, [free, 1 << 20, 0, 0, 0, 0]), 0);
    let want = free + 4 * host + PAGE;
    let at = f.map(want, 5 * PAGE, PROT_READ, MAP_PRIVATE | MAP_FIXED, PAGE);
    assert_eq!(at, want);
    f.check(at, PAGE, 5 * PAGE);
    assert!(f.file_at(at) && f.file_at(at + 4 * PAGE), "a view");
    assert!(f.p.mm.describe(at - PAGE).is_none() && f.p.mm.describe(at + 5 * PAGE).is_none());
}

/// `MAP_FIXED` into a reservation (the dynamic linker's way): every host page of the request is the
/// view -- the edge pages too, the reservation's parts of them staying its own over the view
/// (`GuestSpace::map_file_exact`) -- and the reservation around it stays.
#[test]
fn a_fixed_mapping_inside_a_reservation_is_a_view_to_its_edges() {
    let Some(mut f) = Fixture::new("reserved", 4104) else { return };
    let host = f.host_page();
    let r = f.reserve(1 << 20);
    let want = r + 4 * host + PAGE;
    let len = 3 * host;
    let committed = f.p.mem.space().stats().committed;
    let at = f.map(want, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, PAGE);
    assert_eq!(at, want);
    f.check(at, PAGE, len);
    assert!(f.file_at(r + 5 * host), "the whole host pages inside it are a view");
    assert!(f.file_at(at) && f.file_at(at + len - PAGE), "and its edges");
    assert_eq!(f.p.mem.space().stats().committed, committed, "no private copy");
    let space = f.p.mem.space();
    let reserved = |a: u64| space.region_at(a as usize).is_some_and(|r| r.mapping.is_some() && r.protection == omni_mem::Protection::None && r.kind == RegionKind::Anonymous);
    assert!(reserved(at - PAGE), "the reservation before it stays");
    assert!(reserved(at + len), "and after it");
    assert!(f.p.mem.read(at - PAGE, 1).is_err(), "PROT_NONE in the guest's view");
}

/// `MAP_FIXED` at an address that does not agree with its offset modulo the host page (the dynamic
/// linker's segments, at a 4 KiB-random start): no view of the file can be, so a view of the file
/// moved on by the difference, to its edges. A write is private: neither file is written.
#[test]
fn a_fixed_mapping_that_does_not_agree_with_its_offset_is_a_view_of_the_file_moved_on() {
    let Some(mut f) = Fixture::new("incongruent", 4105) else { return };
    let host = f.host_page();
    let r = f.reserve(1 << 20);
    let at = f.map(r + 4 * host + 2 * PAGE, 3 * host, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_FIXED, PAGE);
    f.check(at, PAGE, 3 * host);
    assert!(f.file_at(r + 5 * host), "a view between the edges");
    assert_eq!(f.p.mm.describe(at).as_deref(), Some("/data.bin+0x1000"));
    f.p.mem.write(r + 5 * host + 3, &[0xA5]).unwrap();
    let mut want = f.want(PAGE, 3 * host);
    want[(r + 5 * host + 3 - at) as usize] = 0xA5;
    assert_eq!(f.p.mem.read(at, 3 * host as usize).unwrap(), want);
    assert_eq!(std::fs::read(&f.object).unwrap(), f.bytes, "the host file is not written");
    if host > PAGE {
        let sha = f.object.file_name().unwrap().to_string_lossy().into_owned();
        let moved = f.dir.join("omni-shifted").join(format!("{sha}-{:x}", PAGE));
        let bytes = std::fs::read(&moved).expect("the file moved on by 4 KiB");
        assert!(bytes[..PAGE as usize].iter().all(|&b| b == 0) && bytes[PAGE as usize..] == f.bytes[..], "zeros, then the file, unwritten");
    }
}

/// Part of such a view unmapped (a 4 KiB hole in a host page, the head) and part of it protected
/// writable and written: the rest keeps its bytes and its view, the file is not written.
#[test]
fn part_of_a_view_at_a_4k_offset_unmapped_and_protected() {
    let Some(mut f) = Fixture::new("parts", 4106) else { return };
    let at = f.map(0, 12 * PAGE, PROT_READ, MAP_PRIVATE, PAGE);
    assert_eq!(f.p.syscall(&mut f.t, nr::MUNMAP, [at + 2 * PAGE, PAGE, 0, 0, 0, 0]), 0);
    assert_eq!(f.p.syscall(&mut f.t, nr::MUNMAP, [at, PAGE, 0, 0, 0, 0]), 0);
    assert!(f.p.mm.describe(at).is_none() && f.p.mm.describe(at + 2 * PAGE).is_none());
    f.check(at + PAGE, 2 * PAGE, PAGE);
    f.check(at + 3 * PAGE, 4 * PAGE, 9 * PAGE);
    assert_eq!(f.p.mm.describe(at + 3 * PAGE).as_deref(), Some("/data.bin+0x4000"));
    assert_eq!(f.p.syscall(&mut f.t, nr::MPROTECT, [at + 5 * PAGE, PAGE, PROT_READ | PROT_WRITE, 0, 0, 0]), 0);
    f.p.mem.write(at + 5 * PAGE + 7, &[0x5A]).unwrap();
    assert_eq!(f.p.syscall(&mut f.t, nr::MPROTECT, [at + 5 * PAGE, PAGE, PROT_READ, 0, 0, 0]), 0);
    let mut want = f.want(4 * PAGE, 9 * PAGE);
    want[2 * PAGE as usize + 7] = 0x5A;
    assert_eq!(f.p.mem.read(at + 3 * PAGE, 9 * PAGE as usize).unwrap(), want);
    assert!(f.file_at(at + 11 * PAGE), "the untouched rest is still a view");
    assert_eq!(std::fs::read(&f.object).unwrap(), f.bytes, "the host file is not written");
}

/// The file's last, partial host page: a view where the host lets a private view end inside it
/// (Linux, macOS), the bytes past the file's end reading as zero; a copy elsewhere.
#[test]
fn the_last_partial_host_page_of_a_file_is_a_view() {
    let Some(mut f) = Fixture::new("tail", 4107) else { return };
    let len = (f.bytes.len() as u64).next_multiple_of(PAGE);
    let at = f.map(0, len, PROT_READ, MAP_PRIVATE, 0);
    f.check(at, 0, len);
    let last = at + len - PAGE;
    assert_eq!(f.file_at(last), omni_platform::vm::private_view_may_end_in_last_page(), "the last page");
    // From a 4 KiB offset, past the file's end: anonymous zeros after the view.
    let at = f.map(0, 32 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, 21 * PAGE);
    f.check(at, 21 * PAGE, 32 * PAGE);
    f.p.mem.write(at + 4 * PAGE - 1, &[7]).unwrap();
    f.p.mem.write(at + 20 * PAGE, &[9]).unwrap();
    assert_eq!(f.p.mem.read(at + 4 * PAGE - 1, 1).unwrap(), vec![7]);
    assert_eq!(std::fs::read(&f.object).unwrap(), f.bytes, "the host file is not written");
}

/// Every 4 KiB offset and length, up to two pages past the file's end, of files ending in each 4 KiB
/// of a 16 KiB page and at its end (the dynamic linker maps a library's section headers, its last
/// bytes, so): the file's bytes, then zeros, and unmapped again.
#[test]
fn every_offset_and_length_maps_the_files_bytes() {
    for (k, file_len) in [0x5388usize, 0x6388, 0x7388, 0x7608, 0x8000, 0x8001].into_iter().enumerate() {
        let Some(mut f) = Fixture::of_len(&format!("every{k}"), 4109, file_len) else { return };
        let end = (file_len as u64).next_multiple_of(PAGE);
        for offset in (0..end).step_by(PAGE as usize) {
            for len in (PAGE..=end - offset + 2 * PAGE).step_by(PAGE as usize) {
                let at = f.p.syscall(&mut f.t, nr::MMAP, [0, len, PROT_READ, MAP_PRIVATE, f.fd, offset]);
                assert!((at as i64) > 0, "file of {file_len:#x}: mmap +{len:#x} at offset {offset:#x}: {}", at as i64);
                f.check(at, offset, len);
                assert_eq!(f.p.syscall(&mut f.t, nr::MUNMAP, [at, len, 0, 0, 0, 0]), 0);
            }
        }
    }
}

/// `MAP_FIXED` into a reservation at every 4 KiB of a host page, from every 4 KiB offset, every
/// length: the file's bytes, then zeros; the reservation on both sides stays; unmapped, the
/// reservation's own pages are back where it was asked.
#[test]
fn every_fixed_address_offset_and_length_maps_the_files_bytes() {
    let Some(mut f) = Fixture::of_len("everyfixed", 4111, 0x9388) else { return };
    let host = f.host_page();
    let r = f.reserve(1 << 20);
    let end = 0xA000u64;
    for skew in (0..host).step_by(PAGE as usize) {
        for offset in (0..end).step_by(PAGE as usize) {
            for len in (PAGE..=end - offset + PAGE).step_by(PAGE as usize) {
                let want = r + 4 * host + skew;
                let at = f.map(want, len, PROT_READ, MAP_PRIVATE | MAP_FIXED, offset);
                assert_eq!(at, want);
                f.check(at, offset, len);
                let space = f.p.mem.space();
                assert!(space.region_at((at - PAGE) as usize).is_some_and(|x| x.mapping.is_some()), "{skew:#x} {offset:#x}+{len:#x}: before");
                assert!(space.region_at((at + len) as usize).is_some_and(|x| x.mapping.is_some()), "{skew:#x} {offset:#x}+{len:#x}: after");
                let back = f.p.syscall(&mut f.t, nr::MMAP, [at, len, 0, MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, u64::MAX, 0]);
                assert_eq!(back, at);
            }
        }
    }
}

/// `MAP_FIXED_NOREPLACE` whose host pages between are free but whose last 4 KiB reach another
/// mapping: refused (EEXIST), what it had mapped of itself gone, the other mapping as it was.
#[test]
fn a_fixed_noreplace_mapping_over_another_is_refused_and_leaves_it() {
    const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
    const EEXIST: i64 = 17;
    let Some(mut f) = Fixture::new("noreplace", 4112) else { return };
    let host = f.host_page();
    let r = f.reserve(1 << 20);
    assert_eq!(f.p.syscall(&mut f.t, nr::MUNMAP, [r + 4 * host, 4 * host, 0, 0, 0, 0]), 0);
    let other = r + 7 * host + PAGE;
    assert_eq!(f.p.syscall(&mut f.t, nr::MMAP, [other, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, u64::MAX, 0]), other);
    f.p.mem.write(other, b"mine").unwrap();
    let at = r + 4 * host + PAGE;
    let got = f.p.syscall(&mut f.t, nr::MMAP, [at, 3 * host + PAGE, PROT_READ, MAP_PRIVATE | MAP_FIXED_NOREPLACE, f.fd, PAGE]);
    assert_eq!(got as i64, -EEXIST);
    assert_eq!(f.p.mem.read(other, 4).unwrap(), b"mine");
    let space = f.p.mem.space();
    for a in [at, r + 5 * host, r + 6 * host + PAGE] {
        assert!(space.region_at(a as usize).is_none_or(|x| x.mapping.is_none()), "{a:#x} is free again");
    }
    assert!(f.p.mm.describe(at).is_none());
}

/// An installed package's APK (an instance file, not the image's), mapped read-only `MAP_SHARED`
/// as AssetManager maps an asset: a view where a view does not pin its file (Linux, macOS) -- and
/// the file can still be renamed with its directory, as installd renames a staging directory --
/// else a copy, as before. Another file of the instance stays a copy.
#[test]
fn an_installed_apk_is_a_view_where_views_do_not_pin_files() {
    const MAP_SHARED: u64 = 1;
    let scratch = Scratch::new("apk");
    let dir = &scratch.0;
    let pkg = dir.join("data/app/vmdl1.tmp");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::create_dir_all(dir.join("data/local")).unwrap();
    let bytes: Vec<u8> = (0..20 * 4096 + 77).map(|i| (i / 4096) as u8 + 1).collect();
    std::fs::write(pkg.join("base.apk"), &bytes).unwrap();
    std::fs::write(dir.join("data/local/other.bin"), &bytes).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(dir, m), vec![(b"/data".to_vec(), dir.join("data"))], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    if p.mm.page_size() != PAGE {
        return;
    }
    let mut t = Task::new(4113, Arc::clone(&p));
    let mut open = |guest: &[u8]| {
        let path = p.scratch();
        p.mem.write(path, &[guest, b"\0"].concat()).unwrap();
        let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, path, 0, 0, 0, 0]);
        assert!((fd as i64) >= 0, "open {}: {}", String::from_utf8_lossy(guest), fd as i64);
        let at = p.syscall(&mut t, nr::MMAP, [0, 8 * PAGE, PROT_READ, MAP_SHARED, fd, 13 * PAGE]);
        assert!((at as i64) > 0);
        assert_eq!(p.mem.read(at, 8 * PAGE as usize).unwrap(), [&bytes[13 * PAGE as usize..], &vec![0; PAGE as usize - 77][..]].concat());
        matches!(p.mem.space().region_at(at as usize).unwrap().kind, RegionKind::File { .. })
    };
    assert_eq!(open(b"/data/app/vmdl1.tmp/base.apk"), omni_platform::vm::mapped_file_may_be_renamed());
    assert!(!open(b"/data/local/other.bin"), "not a package's file: a copy");
    if omni_platform::vm::mapped_file_may_be_renamed() {
        std::fs::rename(&pkg, dir.join("data/app/pkg-1")).expect("the staging directory renamed under a live view");
    }
}

/// An installed library mapped `MAP_FIXED` where its offset does not agree (the linker's way): a
/// view of the library moved on, its whole host pages -- made once under its key for every host
/// process that maps it (each Android user's instance of the app has the same `/data/app` path),
/// with no aside file left.
#[test]
fn an_installed_library_at_a_disagreeing_address_is_a_view_of_one_kept_file() {
    let scratch = Scratch::new("lib");
    let dir = &scratch.0;
    let lib = dir.join("data/app/pkg-1/lib/arm64");
    std::fs::create_dir_all(&lib).unwrap();
    let bytes: Vec<u8> = (0..40 * 4096 + 9).map(|i| (i / 4096) as u8 ^ 0x5C).collect();
    std::fs::write(lib.join("libx.so"), &bytes).unwrap();
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(dir, m), vec![(b"/data".to_vec(), dir.join("data"))], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    if p.mm.page_size() != PAGE {
        return;
    }
    let mut t = Task::new(4114, Arc::clone(&p));
    let path = p.scratch();
    p.mem.write(path, b"/data/app/pkg-1/lib/arm64/libx.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, path, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    let host = p.mem.space().page_size() as u64;
    let r = p.syscall(&mut t, nr::MMAP, [0, 1 << 20, 0, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX, 0]);
    let at = p.syscall(&mut t, nr::MMAP, [r + 4 * host + 3 * PAGE, 4 * host, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 2 * PAGE]);
    assert_eq!(at, r + 4 * host + 3 * PAGE);
    assert_eq!(p.mem.read(at, 4 * host as usize).unwrap(), bytes[2 * PAGE as usize..(2 * PAGE + 4 * host) as usize]);
    if omni_platform::vm::mapped_file_may_be_renamed() {
        assert!(matches!(p.mem.space().region_at((r + 6 * host) as usize).unwrap().kind, RegionKind::File { .. }), "a view");
    }
    // Put in place under its key (`<key>-<shift>`): zeros, then the library; the aside name gone.
    let shift = (at - 2 * PAGE) % host;
    if shift > 0 {
        let shifted = dir.join("omni-shifted");
        let names: Vec<String> = std::fs::read_dir(&shifted).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "an aside file left in {}: {names:?}", shifted.display());
        let moved = names.iter().find(|n| n.ends_with(&format!("-{shift:x}"))).expect("the library moved on, kept");
        let got = std::fs::read(shifted.join(moved)).unwrap();
        assert!(got[..shift as usize].iter().all(|&b| b == 0) && got[shift as usize..] == bytes[..], "zeros, then the library");
    }
}

/// A mapping that runs on past the file's end, hinted at a hole that has room for its view and
/// not for the rest: placed where all of it fits (the rest was refused, "already a mapped 4 KiB
/// page", and `dlopen` failed with ENOMEM: "shdr mmap failed: Out of memory").
#[test]
fn a_mapping_past_the_files_end_is_placed_where_all_of_it_fits() {
    let Some(mut f) = Fixture::new("room", 4110) else { return };
    let host = f.host_page();
    let r = f.reserve(4 * host);
    assert_eq!(f.p.syscall(&mut f.t, nr::MUNMAP, [r, host, 0, 0, 0, 0]), 0);
    let at = f.map(r, 2 * host, PROT_READ, MAP_PRIVATE, 0x18000);
    f.check(at, 0x18000, 2 * host);
    let space = f.p.mem.space();
    assert!(space.region_at((r + host) as usize).is_some_and(|x| x.mapping.is_some() && !matches!(x.kind, RegionKind::File { .. })), "what was there stays");
}

/// `mremap` moves such a mapping by copy and keeps its bytes.
#[test]
fn a_view_at_a_4k_offset_is_moved_by_mremap() {
    let Some(mut f) = Fixture::new("mremap", 4108) else { return };
    let at = f.map(0, 8 * PAGE, PROT_READ, MAP_PRIVATE, PAGE);
    let moved = f.p.syscall(&mut f.t, nr::MREMAP, [at, 8 * PAGE, 16 * PAGE, 1, 0, 0]); // MREMAP_MAYMOVE
    assert!((moved as i64) > 0, "mremap: {}", moved as i64);
    f.check(moved, PAGE, 8 * PAGE);
    assert_eq!(f.p.mem.read(moved + 8 * PAGE, 8 * PAGE as usize).unwrap(), vec![0; 8 * PAGE as usize]);
}
