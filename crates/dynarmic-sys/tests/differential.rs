//! **What the guest sees, as a differential** (measurement, `#[ignore]`d): every register and
//! the PC after every block run over real code, hashed, to compare two configurations of the
//! translator run in separate processes (e.g. OMNI_JIT_GETSET_WIDTH=1 vs 0). One hash per library.
//!   OMNI_SYSROOT=... OD_EXTRA_LIB=/tmp/libroblox.so OD_TEST_SHARED_CACHE=1 \
//!   cargo test -p dynarmic-sys --release --test differential -- --ignored --nocapture
mod harness;

use dynarmic_sys::*;
use harness::{Vm, VmOptions, CODE_BASE};
use std::path::PathBuf;

fn sysroot_file(guest: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("OMNI_SYSROOT")?);
    let manifest = std::fs::read_to_string(dir.join("sysroot.manifest")).ok()?;
    let sha = manifest.lines().find_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        (f.len() >= 5 && f[0] == "f" && f[4] == guest).then(|| f[3].to_string())
    })?;
    Some(dir.join("objects").join(&sha[..2]).join(&sha))
}
fn u16le(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes(b[at..at + 2].try_into().unwrap()))
}
fn u32le(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()))
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn text_and_functions(elf: &[u8]) -> ((u64, u64, u64), Vec<u64>) {
    let (phoff, shoff) = (u64le(elf, 0x20) as usize, u64le(elf, 0x28) as usize);
    let (phentsize, phnum) = (u16le(elf, 0x36) as usize, u16le(elf, 0x38) as usize);
    let (shentsize, shnum) = (u16le(elf, 0x3a) as usize, u16le(elf, 0x3c) as usize);
    let mut text = (0, 0, 0);
    for i in 0..phnum {
        let at = phoff + i * phentsize;
        let (kind, flags) = (u32le(elf, at), u32le(elf, at + 4));
        if kind == 1 && flags & 1 != 0 {
            text = (u64le(elf, at + 8), u64le(elf, at + 0x10), u64le(elf, at + 0x20));
        }
    }
    let mut funcs = Vec::new();
    for i in 0..shnum {
        let at = shoff + i * shentsize;
        let kind = u32le(elf, at + 4);
        if kind != 2 && kind != 11 {
            continue;
        }
        let (off, size) = (u64le(elf, at + 0x18) as usize, u64le(elf, at + 0x20) as usize);
        for s in elf[off..off + size].chunks_exact(24) {
            let (kind, shndx) = (s[4] & 0xf, u16le(s, 6));
            let (value, sz) = (u64le(s, 8), u64le(s, 16));
            if kind == 2 && shndx != 0 && sz > 0 && value >= text.1 && value < text.1 + text.2 {
                funcs.push(value);
            }
        }
    }
    funcs.sort_unstable();
    funcs.dedup();
    (text, funcs)
}

fn pass(elf: &[u8], starts: &[u64], text: (u64, u64, u64)) -> (u64, usize) {
    let (off, vaddr, len) = text;
    let code: Vec<u32> = elf[off as usize..(off + len) as usize].chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
    let vm = Vm::new(
        code,
        VmOptions {
            cycle_counting: true,
            check_halt_on_memory_access: true,
            fastmem_exclusive: true,
            optimizations: optimization::INTERRUPTIBLE | optimization::UNSAFE_IGNORE_GLOBAL_MONITOR,
            code_cache_size: 256 << 20,
            ..VmOptions::default()
        },
    );
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x100_0000_01B3);
    };
    let mut runs = 0;
    for (i, &f) in starts.iter().enumerate() {
        vm.set_pc(CODE_BASE + (f - vaddr));
        // Registers seeded per start, so blocks see varied values (and the upper halves matter).
        for r in 0..31 {
            vm.set_reg(r, (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((r as u64) << 56) ^ 0x0000_0000_0010_0000);
        }
        for _ in 0..6 {
            vm.with_ctx(|c| {
                c.ticks_remaining = 1;
                c.ticks_used = 0;
            });
            let hr = vm.run();
            runs += 1;
            mix(u64::from(hr));
            mix(vm.pc());
            for r in 0..31 {
                mix(vm.reg(r));
            }
            if hr != 0 {
                // SAFETY: the jit is live and not executing.
                unsafe { od_jit_clear_halt(vm.raw(), hr) };
                break;
            }
            let pc = vm.pc();
            if pc < CODE_BASE || pc >= CODE_BASE + len {
                break;
            }
        }
    }
    (h, runs)
}

#[test]
#[ignore = "measurement"]
fn guest_visible_differential() {
    for lib in ["/apex/com.android.runtime/lib64/bionic/libc.so", "/apex/com.android.art/lib64/libart.so", "/system/lib64/libhwui.so", "/system/lib64/libandroid_runtime.so"] {
        let Some(path) = sysroot_file(lib) else {
            println!("{lib}: no sysroot");
            continue;
        };
        let elf = std::fs::read(path).unwrap();
        let (text, funcs) = text_and_functions(&elf);
        let (h, runs) = pass(&elf, &funcs, text);
        println!("HASH {lib} {h:016x} ({} starts, {runs} runs)", funcs.len());
    }
    if let Ok(extra) = std::env::var("OD_EXTRA_LIB") {
        let elf = std::fs::read(&extra).unwrap();
        let (text, _) = text_and_functions(&elf);
        // A stripped library: a start every 1 KiB of its code.
        let starts: Vec<u64> = (0..text.2 / 1024).map(|i| text.1 + i * 1024).collect();
        let (h, runs) = pass(&elf, &starts, text);
        println!("HASH {extra} {h:016x} ({} starts, {runs} runs)", starts.len());
    }
}
