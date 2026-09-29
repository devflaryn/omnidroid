//! Roblox 2.740.931's `libzstd-jni-1.5.7-6.so` -- a packed library linked for 4 KiB pages, its data
//! segments' offsets and addresses agreeing modulo 4 KiB only -- mapped exactly as bionic's
//! `linker64` maps it, through this kernel's `mmap`/`mprotect`: the span reserved, every `PT_LOAD`
//! `MAP_FIXED` at `bias + p_vaddr` from its 4 KiB-aligned file offset, `.bss` zeroed, relro sealed.
//! On a 16 KiB host that is what the 4 KiB guest (D42) exists for; on a 16 KiB guest the kernel
//! refused the offsets, and `pagecompat` rewrote the file instead
//! (`docs/research/2026-09-29-libzstd-jni-16k.md`). Needs the APK (`OMNI_ZSTD_APK`, default
//! `~/Desktop/Roblox-2.740.931.apk`); skipped without it.
use std::io::Read;
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_PRIVATE: u64 = 2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANON: u64 = 0x20;
const PAGE: u64 = 4096;

struct Load {
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

fn library() -> Option<Vec<u8>> {
    let apk = std::env::var_os("OMNI_ZSTD_APK")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join("Desktop/Roblox-2.740.931.apk")))?;
    let data = std::fs::read(apk).ok()?;
    let u16_at = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    // The central directory, from its end record (as c2_apk_in_app_process.rs reads it).
    let eocd = (0..data.len() - 21).rev().find(|&i| data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])?;
    let (count, mut at) = (u16_at(eocd + 10), u32_at(eocd + 16));
    for _ in 0..count {
        let (method, csize, size) = (u16_at(at + 10), u32_at(at + 20), u32_at(at + 24));
        let (nlen, xlen, clen) = (u16_at(at + 28), u16_at(at + 30), u16_at(at + 32));
        let local = u32_at(at + 42);
        let name = &data[at + 46..at + 46 + nlen];
        at += 46 + nlen + xlen + clen;
        if name != b"lib/arm64-v8a/libzstd-jni-1.5.7-6.so" {
            continue;
        }
        let body = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let raw = &data[body..body + csize];
        return Some(match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(size);
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut v).ok()?;
                v
            }
            _ => return None,
        });
    }
    None
}

fn loads(elf: &[u8]) -> (Vec<Load>, Option<(u64, u64)>) {
    let u16_at = |at: usize| u16::from_le_bytes([elf[at], elf[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(elf[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(elf[at..at + 8].try_into().unwrap());
    let (phoff, phentsize, phnum) = (u64_at(0x20) as usize, u16_at(0x36), u16_at(0x38));
    let mut out = Vec::new();
    let mut relro = None;
    for ph in (0..phnum).map(|i| phoff + i * phentsize) {
        match u32_at(ph) {
            1 => out.push(Load { flags: u32_at(ph + 4), offset: u64_at(ph + 8), vaddr: u64_at(ph + 16), filesz: u64_at(ph + 32), memsz: u64_at(ph + 40) }),
            0x6474_e552 => relro = Some((u64_at(ph + 16), u64_at(ph + 40))),
            _ => {}
        }
    }
    (out, relro)
}

/// A process whose `/data/app/.../libzstd.so` is the library.
fn process(lib: &[u8]) -> (Arc<Process>, omni_linux::Task, u64) {
    let dir = std::env::temp_dir().join(format!("omni-linux-zstd-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("objects/zz")).unwrap();
    std::fs::write(dir.join("objects/zz/zz01"), lib).unwrap();
    let m = manifest::parse(&format!("d\t755\t/\nd\t755\t/system\nf\t644\t{}\tzz01\t/system/libzstd.so\n", lib.len())).unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = p.scratch();
    (p, t, s)
}

fn prot(flags: u32) -> u64 {
    (if flags & 4 != 0 { PROT_READ } else { 0 }) | (if flags & 2 != 0 { PROT_WRITE } else { 0 }) | (if flags & 1 != 0 { PROT_EXEC } else { 0 })
}

#[test]
fn libzstd_jni_maps_where_its_headers_say_on_4_kib_pages() {
    let Some(lib) = library() else {
        eprintln!("no Roblox 2.740.931 APK: skipped");
        return;
    };
    let (segments, relro) = loads(&lib);
    let (p, mut t, s) = process(&lib);
    assert_eq!(p.mm.page_size(), PAGE, "the guest's page");
    p.mem.write(s, b"/system/libzstd.so\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    assert!((fd as i64) >= 0);
    let floor = |v: u64| v & !(PAGE - 1);
    let ceil = |v: u64| (v + PAGE - 1) & !(PAGE - 1);
    let span = ceil(segments.iter().map(|l| l.vaddr + l.memsz).max().unwrap());
    // linker64: reserve the span, then each segment over it.
    let base = p.syscall(&mut t, nr::MMAP, [0, span, 0, MAP_PRIVATE | MAP_ANON, u64::MAX, 0]) as i64;
    assert!(base > 0, "reserve: {base}");
    let base = base as u64;
    for l in &segments {
        let (start, file_end) = (floor(l.vaddr), l.vaddr + l.filesz);
        let at = p.syscall(&mut t, nr::MMAP, [base + start, ceil(file_end) - start, prot(l.flags) | PROT_WRITE, MAP_PRIVATE | MAP_FIXED, fd, floor(l.offset)]) as i64;
        assert_eq!(at as u64, base + start, "PT_LOAD at {:#x} from offset {:#x}", l.vaddr, l.offset);
        // .bss: the rest of the last file page zeroed, then anonymous pages.
        if l.memsz > l.filesz {
            let zero = (ceil(file_end) - file_end) as usize;
            p.mem.write(base + file_end, &vec![0; zero]).unwrap();
            if ceil(l.vaddr + l.memsz) > ceil(file_end) {
                let anon = p.syscall(&mut t, nr::MMAP, [base + ceil(file_end), ceil(l.vaddr + l.memsz) - ceil(file_end), prot(l.flags), MAP_PRIVATE | MAP_FIXED | MAP_ANON, u64::MAX, 0]) as i64;
                assert!(anon > 0, ".bss: {anon}");
            }
        }
        assert_eq!(p.syscall(&mut t, nr::MPROTECT, [base + start, ceil(file_end) - start, prot(l.flags), 0, 0, 0]), 0);
    }
    if let Some((vaddr, memsz)) = relro {
        // bionic seals relro rounded out to pages: exactly [floor(start), ceil(end)) at 4 KiB.
        assert_eq!(p.syscall(&mut t, nr::MPROTECT, [base + floor(vaddr), ceil(vaddr + memsz) - floor(vaddr), PROT_READ, 0, 0, 0]), 0);
    }
    // Every byte is the file's where the headers put it.
    for l in &segments {
        let got = p.mem.read(base + l.vaddr, l.filesz as usize).unwrap();
        assert!(got == lib[l.offset as usize..(l.offset + l.filesz) as usize], "segment at {:#x}: bytes differ", l.vaddr);
        if l.memsz > l.filesz {
            assert!(p.mem.read(base + l.vaddr + l.filesz, (l.memsz - l.filesz) as usize).unwrap().iter().all(|&b| b == 0), ".bss is zero");
        }
    }
    // Every 4 KiB has its own protection: relro sealed to its end and not a page further, .data
    // writable from the next 4 KiB -- which a 16 KiB page could not both give.
    let region = |a: u64| p.mem.space().region_at(a as usize).expect("mapped");
    if let Some((vaddr, memsz)) = relro {
        let end = ceil(vaddr + memsz);
        assert_eq!(region(base + end - PAGE).protection, omni_mem::Protection::Read, "relro's last 4 KiB is sealed");
        let data = segments.iter().find(|l| l.vaddr >= end - PAGE && l.flags & 2 != 0).expect("a data segment after relro");
        assert!(region(base + ceil(data.vaddr)).protection.is_writable(), ".data after relro is writable");
        assert!(p.mem.write(base + end - PAGE, b"x").is_err(), "a write to relro is refused");
    }
    let text = &segments[0];
    assert!(region(base + text.vaddr).protection.is_executable());
}
