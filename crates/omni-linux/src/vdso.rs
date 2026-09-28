//! **The vDSO**: the arm64 Linux kernel's user-mode time functions, mapped into every process and
//! named to it by `AT_SYSINFO_EHDR`, so bionic's `clock_gettime`, `gettimeofday` and `time` read the
//! clock without a system call -- as on a device, where they never make one.
//!
//! Without it every read of the time is a system call: ~117,000 a second in a Roblox world
//! (HANDOFF "LIGHTER AND FASTER"), each a trip out of translated code and back. With it a read is a
//! dozen guest instructions and the counter read (`CNTVCT_EL0`, which the CPU backend answers from
//! the host's clock without leaving the translated code's thread).
//!
//! The image (`device/src/vdso/`, `vdso.S` and `vdso.lds`, built as its `build.txt` says) is laid
//! out as the kernel's: one segment, file offsets equal to addresses, the section headers inside
//! the image (bionic finds `.dynsym` through them), and its **data page** -- `__omni_vvar` -- the
//! last page of the segment, which the kernel fills here before the image is made read-only:
//!
//! | offset | value |
//! |---|---|
//! | 0 | 1: the page is filled |
//! | 8 | the counter's frequency, Hz ([`omni_cpu::CNTFRQ_HZ`]) |
//! | 16 | `CLOCK_MONOTONIC` when the counter read 0, ns ([`crate::sys::counter_offset`]) |
//! | 24 | `CLOCK_REALTIME` less `CLOCK_MONOTONIC`, ns ([`crate::sys::realtime_offset`]) |
//!
//! These are the numbers the system calls compute with (`crate::sys::monotonic`), so a clock read
//! through the vDSO and then through a system call (a raw `svc`, a clock the vDSO does not serve)
//! agree to the nanosecond: they are one function of the counter.
//!
//! `__kernel_rt_sigreturn` is the signal trampoline a handler returns through when its action has
//! no `SA_RESTORER`, as it was when the `[vdso]` page held only that.
use std::sync::OnceLock;

/// The image, as built.
pub const IMAGE: &[u8] = include_bytes!("../device/src/vdso/linux-vdso.so");

/// Where things are in the image.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// The data page's offset.
    pub vvar: u64,
    /// `__kernel_rt_sigreturn`'s offset.
    pub sigreturn: u64,
}

fn u16_at(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8"))
}

/// A dynamic symbol's value in `elf`, by name.
fn symbol(elf: &[u8], name: &str) -> Option<u64> {
    let shoff = usize::try_from(u64_at(elf, 0x28)).ok()?;
    let (shentsize, shnum) = (u16_at(elf, 0x3a), u16_at(elf, 0x3c));
    let section = |i: usize| shoff + i * shentsize;
    let dynsym = (0..shnum).map(section).find(|&s| u32_at(elf, s + 4) == 11)?; // SHT_DYNSYM
    let strtab = section(u32_at(elf, dynsym + 40) as usize);
    let (sym_off, sym_size) = (u64_at(elf, dynsym + 24) as usize, u64_at(elf, dynsym + 32) as usize);
    let str_off = u64_at(elf, strtab + 24) as usize;
    (0..sym_size / 24).map(|i| sym_off + i * 24).find_map(|sym| {
        let at = str_off + u32_at(elf, sym) as usize;
        let end = at + elf[at..].iter().position(|&c| c == 0)?;
        (&elf[at..end] == name.as_bytes()).then(|| u64_at(elf, sym + 8))
    })
}

/// The image's layout.
///
/// # Panics
/// The image lacks `__omni_vvar` or `__kernel_rt_sigreturn` (a unit test checks it does not).
#[must_use]
pub fn layout() -> Layout {
    static LAYOUT: OnceLock<Layout> = OnceLock::new();
    *LAYOUT.get_or_init(|| Layout {
        vvar: symbol(IMAGE, "__omni_vvar").expect("the vDSO's data page"),
        sigreturn: symbol(IMAGE, "__kernel_rt_sigreturn").expect("the vDSO's signal trampoline"),
    })
}

/// The image with its data page filled for this host process.
#[must_use]
pub fn filled() -> Vec<u8> {
    let mut image = IMAGE.to_vec();
    let at = layout().vvar as usize;
    let words: [u64; 4] = [1, u64::from(omni_cpu::CNTFRQ_HZ), crate::sys::counter_offset() as i64 as u64, crate::sys::realtime_offset() as i64 as u64];
    for (i, w) in words.iter().enumerate() {
        image[at + i * 8..at + i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_names_its_entry_points_and_its_data_page_is_the_last_of_its_segment() {
        for name in ["__kernel_clock_gettime", "__kernel_gettimeofday", "__kernel_clock_getres", "__kernel_rt_sigreturn", "__omni_vvar"] {
            assert!(symbol(IMAGE, name).is_some(), "{name}");
        }
        let l = layout();
        assert_eq!(l.vvar % 4096, 0);
        // The trampoline is the kernel's two instructions: mov x8, #139; svc #0.
        let s = l.sigreturn as usize;
        assert_eq!((u32_at(IMAGE, s), u32_at(IMAGE, s + 4)), (0xD280_1168, 0xD400_0001));
        // One PT_LOAD at offset 0, address 0, ending with the data page.
        let (phoff, phnum) = (u64_at(IMAGE, 0x20) as usize, u16_at(IMAGE, 0x38));
        let loads: Vec<usize> = (0..phnum).map(|i| phoff + i * 56).filter(|&p| u32_at(IMAGE, p) == 1).collect();
        assert_eq!(loads.len(), 1);
        assert_eq!((u64_at(IMAGE, loads[0] + 8), u64_at(IMAGE, loads[0] + 16)), (0, 0), "offset and address 0");
        assert_eq!(u64_at(IMAGE, loads[0] + 32), l.vvar + 4096, "the data page ends the segment");
    }

    #[test]
    fn the_data_page_holds_the_clocks_numbers() {
        let image = filled();
        let at = layout().vvar as usize;
        assert_eq!(u64_at(&image, at), 1);
        assert_eq!(u64_at(&image, at + 8), u64::from(omni_cpu::CNTFRQ_HZ));
        // What the vDSO computes from the counter is what the system call answers.
        let ticks = omni_cpu::cntpct();
        let vdso = i128::from(crate::sys::counter_ns(ticks)) + i128::from(u64_at(&image, at + 16) as i64);
        let sys = crate::sys::monotonic().as_nanos() as i128;
        assert!((0..50_000_000).contains(&(sys - vdso)), "the system call reads just after: {sys} vs {vdso}");
    }
}
