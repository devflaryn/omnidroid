//! **Page-size compatibility for an app's 4 KiB-aligned libraries** on a device whose page is
//! larger (the guest's page is the host's: 16 KiB on Apple silicon).
//!
//! A library linked for 4 KiB pages has `PT_LOAD` segments whose file offsets agree with their
//! addresses only modulo 4 KiB. bionic's linker maps each segment from the file at page
//! granularity, so on a 16 KiB device such a library lands shifted and fails to load
//! (`dlopen failed: empty/missing DT_HASH/DT_GNU_HASH` -- Roblox 2.740.931's `libzstd-jni-1.5.7-6.so`,
//! which its startup treats as fatal). Android 16's linker has a compatibility mode for this; the
//! Android 15 image here does not. This is that mode's effect, made where a kernel can make it: the
//! file an app opens is laid out again so that every `PT_LOAD`'s offset *equals* its address plus
//! one constant (zero padding inserted before each segment), with `p_align` the page. Pages two
//! segments share then hold the same bytes whichever mapping comes last. `PT_GNU_RELRO` is trimmed to
//! end on the page boundary below its end: bionic seals relro rounded *up* to the page, and the
//! page holding its end holds the start of `.data` too. What is left out of it (the `.got`, here)
//! stays writable, as a 4 KiB-page device's would be until relro; a library that looks its relro
//! header up still finds one.
//!
//! Only app libraries (`/data/app/**.so`) opened for reading, only where a segment's alignment is
//! below the page -- never on a 4 KiB-page host -- and once: the file is rewritten in place.
use std::path::Path;

const PT_LOAD: u32 = 1;
const PT_NULL: u32 = 0;
const PT_GNU_RELRO: u32 = 0x6474_e552;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// `bytes` laid out for `page`-sized pages, or `None` when it needs nothing (every `PT_LOAD` is
/// aligned to `page` already) or is not a 64-bit little-endian ELF this can lay out: segments out
/// of order, overlapping in the file, or packed tighter in the file than in memory.
#[must_use]
pub fn realign(bytes: &[u8], page: u64) -> Option<Vec<u8>> {
    if bytes.get(..6)? != b"\x7fELF\x02\x01" || !page.is_power_of_two() {
        return None;
    }
    let phoff = usize::try_from(u64_at(bytes, 0x20)?).ok()?;
    let shoff = u64_at(bytes, 0x28)?;
    let (phentsize, phnum) = (usize::from(u16_at(bytes, 0x36)?), usize::from(u16_at(bytes, 0x38)?));
    let (shentsize, shnum) = (usize::from(u16_at(bytes, 0x3a)?), usize::from(u16_at(bytes, 0x3c)?));
    if phentsize < 56 || (shnum > 0 && shentsize < 64) {
        return None;
    }
    let ph = |i: usize| phoff + i * phentsize;
    // (offset, vaddr, filesz) of each PT_LOAD, in program-header order.
    let mut loads = Vec::new();
    let mut below = false;
    for i in 0..phnum {
        if u32_at(bytes, ph(i))? == PT_LOAD {
            let (off, va, filesz, align) = (u64_at(bytes, ph(i) + 8)?, u64_at(bytes, ph(i) + 16)?, u64_at(bytes, ph(i) + 32)?, u64_at(bytes, ph(i) + 48)?);
            below |= align < page;
            loads.push((off, va, filesz));
        }
    }
    if !below || loads.is_empty() {
        return None;
    }
    // Every segment at `vaddr + delta`, `delta` the first's: padding before each is what that
    // takes, and must not be negative (the file ranges ordered and apart as the memory ones are).
    let delta = loads[0].0.wrapping_sub(loads[0].1);
    let mut shifts: Vec<(u64, u64)> = Vec::new(); // (old offset from which, added)
    let mut added = 0u64;
    let mut end = 0u64;
    for &(off, va, filesz) in &loads {
        if off < end {
            return None;
        }
        let want = va.wrapping_add(delta);
        let now = off.checked_add(added)?;
        if want < now {
            return None;
        }
        added += want - now;
        shifts.push((off, added));
        end = off.checked_add(filesz)?;
    }
    let map = |old: u64| -> u64 { old + shifts.iter().rev().find(|&&(from, _)| old >= from).map_or(0, |&(_, a)| a) };

    // The new file: each stretch copied at its mapped offset, zero padding between.
    let len = u64::try_from(bytes.len()).ok()?;
    let mut out = vec![0u8; usize::try_from(map(len)).ok()?];
    let mut from = 0u64;
    for &(boundary, _) in shifts.iter().chain(std::iter::once(&(len, 0))) {
        if boundary > from {
            let (a, b) = (usize::try_from(from).ok()?, usize::try_from(boundary).ok()?);
            let at = usize::try_from(map(from)).ok()?;
            out.get_mut(at..at + (b - a))?.copy_from_slice(bytes.get(a..b)?);
        }
        from = boundary;
    }
    // The headers, in the new file: offsets mapped, loads aligned to the page, relro gone.
    if shnum > 0 {
        out[0x28..0x30].copy_from_slice(&map(shoff).to_le_bytes());
    }
    let new_phoff = usize::try_from(map(phoff as u64)).ok()?;
    for i in 0..phnum {
        let at = new_phoff + i * phentsize;
        let kind = u32_at(&out, at)?;
        let off = u64_at(&out, at + 8)?;
        out[at + 8..at + 16].copy_from_slice(&map(off).to_le_bytes());
        if kind == PT_LOAD && u64_at(&out, at + 48)? < page {
            out[at + 48..at + 56].copy_from_slice(&page.to_le_bytes());
        }
        if kind == PT_GNU_RELRO {
            let (va, memsz) = (u64_at(&out, at + 16)?, u64_at(&out, at + 40)?);
            let end = va.checked_add(memsz)? & !(page - 1);
            if end <= va {
                out[at..at + 4].copy_from_slice(&PT_NULL.to_le_bytes());
            } else {
                out[at + 32..at + 40].copy_from_slice(&(end - va).to_le_bytes());
                out[at + 40..at + 48].copy_from_slice(&(end - va).to_le_bytes());
            }
        }
    }
    let new_shoff = usize::try_from(map(shoff)).ok()?;
    for i in 0..shnum {
        let at = new_shoff + i * shentsize;
        let off = u64_at(&out, at + 24)?;
        out[at + 24..at + 32].copy_from_slice(&map(off).to_le_bytes());
    }
    Some(out)
}

/// [`realign`] the file at `host` in place, if it needs it: written beside it and renamed over it,
/// so a concurrent reader sees one file or the other. `true` if it was rewritten.
pub fn realign_file(host: &Path, page: u64) -> bool {
    let Ok(bytes) = std::fs::read(host) else { return false };
    let Some(out) = realign(&bytes, page) else { return false };
    let tmp = host.with_extension(format!("pagecompat-{}", std::process::id()));
    let done = std::fs::write(&tmp, &out).and_then(|()| {
        if let Ok(meta) = std::fs::metadata(host) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, host)
    });
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    eprintln!("[pagecompat] {} laid out for {} KiB pages ({} -> {} bytes)", host.display(), page >> 10, bytes.len(), out.len());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = 0x4000;

    fn put16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// `libzstd-jni-1.5.7-6.so`'s shape, scaled down: text at 0/0, data 0x1000 further in memory
    /// than in the file, a second data segment 0x2000 further, `.dynamic` inside the first data
    /// segment, relro over it, and section headers at the end naming each segment's start.
    fn zstd_shaped() -> Vec<u8> {
        let (text, data1, data2) = ((0x0u64, 0x0u64, 0x5f00u64), (0x5f00u64, 0x6f00u64, 0x1050u64), (0x6f50u64, 0x8f50u64, 0x440u64));
        let shoff = 0x7400usize;
        let mut b = vec![0u8; shoff + 4 * 64];
        b[..6].copy_from_slice(b"\x7fELF\x02\x01");
        put64(&mut b, 0x20, 0x40);
        put64(&mut b, 0x28, shoff as u64);
        put16(&mut b, 0x36, 56);
        put16(&mut b, 0x38, 5);
        put16(&mut b, 0x3a, 64);
        put16(&mut b, 0x3c, 4);
        let phdr = |b: &mut Vec<u8>, i: usize, kind: u32, off: u64, va: u64, filesz: u64, align: u64| {
            let at = 0x40 + i * 56;
            put32(b, at, kind);
            put64(b, at + 8, off);
            put64(b, at + 16, va);
            put64(b, at + 32, filesz);
            put64(b, at + 40, filesz);
            put64(b, at + 48, align);
        };
        phdr(&mut b, 0, PT_LOAD, text.0, text.1, text.2, 0x1000);
        phdr(&mut b, 1, PT_LOAD, data1.0, data1.1, data1.2, 0x1000);
        phdr(&mut b, 2, PT_LOAD, data2.0, data2.1, data2.2, 0x1000);
        phdr(&mut b, 3, 2, data1.0 + 0x100, data1.1 + 0x100, 0x40, 8); // PT_DYNAMIC
        phdr(&mut b, 4, PT_GNU_RELRO, data1.0, data1.1, data1.2, 1);
        for (i, off) in [0u64, text.0 + 0x300, data1.0, data2.0].into_iter().enumerate() {
            put64(&mut b, shoff + i * 64 + 24, off);
        }
        // A marker byte at each segment's start and at `.dynamic`.
        b[0x300] = 0xA1;
        b[data1.0 as usize] = 0xB2;
        b[data1.0 as usize + 0x100] = 0xD4;
        b[data2.0 as usize] = 0xC3;
        b
    }

    #[test]
    fn every_segment_ends_up_at_its_address_and_its_bytes_go_with_it() {
        let out = realign(&zstd_shaped(), PAGE).expect("realigned");
        let load = |i: usize| (u64_at(&out, 0x40 + i * 56 + 8).unwrap(), u64_at(&out, 0x40 + i * 56 + 16).unwrap(), u64_at(&out, 0x40 + i * 56 + 48).unwrap());
        for i in 0..3 {
            let (off, va, align) = load(i);
            assert_eq!(off, va, "segment {i}: offset is its address");
            assert_eq!(align, PAGE, "segment {i}: aligned to the page");
        }
        assert_eq!(out[0x300], 0xA1);
        assert_eq!(out[0x6f00], 0xB2, "the first data segment's bytes are at its address");
        assert_eq!(out[0x8f50], 0xC3, "the second's too");
        let dynamic = u64_at(&out, 0x40 + 3 * 56 + 8).unwrap();
        assert_eq!((dynamic, out[dynamic as usize]), (0x7000, 0xD4), "PT_DYNAMIC moved with its segment");
        // Relro [0x6f00, 0x7f50) ends inside the page [0x4000, 0x8000): nothing whole is left of
        // it below that page's start, so it goes.
        assert_eq!(u32_at(&out, 0x40 + 4 * 56).unwrap(), PT_NULL, "relro with no whole page left, dropped");
        let shoff = u64_at(&out, 0x28).unwrap() as usize;
        assert_eq!(shoff, 0x7400 + 0x2000, "the section headers moved with what follows the last segment");
        let sections: Vec<u64> = (0..4).map(|i| u64_at(&out, shoff + i * 64 + 24).unwrap()).collect();
        assert_eq!(sections, vec![0, 0x300, 0x6f00, 0x8f50], "each section header names its data's new offset");
    }

    #[test]
    fn relro_is_trimmed_to_the_page_below_its_end() {
        let mut b = zstd_shaped();
        // Relro from the data segment's start, 0x9000 long: [0x6f00, 0xff00) ends in the page
        // from 0xc000, which it keeps up to.
        put64(&mut b, 0x40 + 4 * 56 + 40, 0x9000);
        let out = realign(&b, PAGE).expect("realigned");
        let at = 0x40 + 4 * 56;
        assert_eq!(u32_at(&out, at).unwrap(), PT_GNU_RELRO);
        assert_eq!((u64_at(&out, at + 16).unwrap(), u64_at(&out, at + 40).unwrap()), (0x6f00, 0xc000 - 0x6f00));
    }

    #[test]
    fn a_library_already_aligned_to_the_page_is_left_alone() {
        let mut b = zstd_shaped();
        for i in 0..3 {
            put64(&mut b, 0x40 + i * 56 + 48, PAGE);
        }
        assert!(realign(&b, PAGE).is_none());
        assert!(realign(&zstd_shaped(), 0x1000).is_none(), "a 4 KiB device needs nothing");
    }

    #[test]
    fn what_cannot_be_laid_out_is_left_alone() {
        assert!(realign(b"not an elf", PAGE).is_none());
        let mut b = zstd_shaped();
        // The second segment closer in memory than in the file: padding would be negative.
        put64(&mut b, 0x40 + 56 + 16, 0x5000);
        assert!(realign(&b, PAGE).is_none());
    }
}
