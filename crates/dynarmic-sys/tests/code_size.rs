//! **Where the bytes of translated code go, on real Android code** (patch 0060's census). A
//! measurement, `#[ignore]`d:
//!
//! ```text
//! OMNI_SYSROOT=<sysroot/aosp-35> OD_TEST_SHARED_CACHE=1 \
//!   cargo test -p dynarmic-sys --release --test code_size -- --ignored --nocapture
//! ```
//!
//! A library from the pinned sysroot (`OMNI_CODE_SIZE_LIB`, default bionic's `libc.so`) is loaded
//! as guest code and each of its functions is entered and run a few blocks with a tiny cycle
//! budget and garbage registers: its first blocks get translated, as omni-cpu configures the jit
//! (memory-abort checks, value-compare monitor, inline exclusives, `INTERRUPTIBLE`, cycle
//! counting). The harness's guest memory is a mirrored arena, so no access faults and every one
//! keeps its fastmem path and out-of-line fault stub, as in a game. Skipped without a sysroot.

mod harness;

use std::path::PathBuf;

use dynarmic_sys::{od_codegen_census, od_codegen_census_reset, od_jit_clear_halt, optimization, CODEGEN_PARTS};
use harness::{Vm, VmOptions, CODE_BASE};

/// The host file of a guest path in the pinned sysroot (`sysroot.manifest`: `f mode size sha path`).
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

/// The executable PT_LOAD (file offset, vaddr, size) and every sized function symbol's vaddr.
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
            continue; // SHT_SYMTAB, SHT_DYNSYM
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

fn census() -> Vec<u64> {
    let mut out = vec![0u64; CODEGEN_PARTS.len()];
    // SAFETY: `out` holds `CODEGEN_PARTS.len()` writable `u64`s.
    unsafe { od_codegen_census(out.as_mut_ptr(), out.len() as u32) };
    out
}

/// Translate the first blocks of every function of `lib`; the census over it, and the functions.
fn translate_library(lib: &str, blocks_per_function: usize) -> Option<(Vec<u64>, usize)> {
    let path = sysroot_file(lib)?;
    let elf = std::fs::read(&path).ok()?;
    let ((off, vaddr, len), funcs) = text_and_functions(&elf);
    let bytes = &elf[off as usize..(off + len) as usize];
    let code: Vec<u32> = bytes.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
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
    // SAFETY: a process-wide counter reset; this binary runs one test.
    unsafe { od_codegen_census_reset() };
    for &f in &funcs {
        vm.set_pc(CODE_BASE + (f - vaddr));
        for _ in 0..blocks_per_function {
            vm.with_ctx(|c| {
                c.ticks_remaining = 1;
                c.ticks_used = 0;
            });
            let hr = vm.run();
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
    Some((census(), funcs.len()))
}

fn report(lib: &str, c: &[u64], functions: usize) {
    let blocks = c[10].max(1);
    let total: u64 = c[..10].iter().sum();
    println!(
        "\n== {lib}: {functions} functions, {} blocks, {} IR insts, {} guest insts, {} memory ops, {} deferred ==",
        c[10], c[11], c[12], c[13], c[14]
    );
    println!("  {:>10} bytes total, {:6.1} B/block, {:5.2} B/guest insn", total, total as f64 / blocks as f64, total as f64 / c[12].max(1) as f64);
    for (i, name) in CODEGEN_PARTS[..10].iter().enumerate() {
        println!(
            "  {name:>9}: {:6.1} B/block  {:5.1}%",
            c[i] as f64 / blocks as f64,
            100.0 * c[i] as f64 / total.max(1) as f64
        );
    }
    if c[13] > 0 {
        println!(
            "  per memory access: {:.1} B inline, {:.1} B of out-of-line code (all deferred, {} per access)",
            c[1] as f64 / c[13] as f64,
            c[8] as f64 / c[13] as f64,
            c[14] as f64 / c[13] as f64
        );
    }
}

#[test]
#[ignore = "measurement, not a test"]
fn where_the_bytes_of_translated_code_go() {
    let lib = std::env::var("OMNI_CODE_SIZE_LIB").unwrap_or_else(|_| "/apex/com.android.runtime/lib64/bionic/libc.so".into());
    let inline = std::env::var("OMNI_CODE_SIZE_FASTDISP").is_ok_and(|v| v == "1");
    let compact = std::env::var("OMNI_CODE_SIZE_COMPACT").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
    // SAFETY: store process-wide atomics.
    unsafe {
        dynarmic_sys::od_set_fast_dispatch_inline(u32::from(inline));
        dynarmic_sys::od_set_compact_code(compact);
    }
    let Some((c, functions)) = translate_library(&lib, 4) else {
        println!("no sysroot (OMNI_SYSROOT) or no {lib}: skipped");
        return;
    };
    println!(
        "(0042 inline dispatch {}, 0061 compact code {})",
        if inline { "on" } else { "off" },
        compact
    );
    report(&lib, &c, functions);
}
