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

use dynarmic_sys::{od_code_cache_stats_of, od_codegen_census, od_codegen_census_reset, od_jit_clear_halt, optimization, OdCodeCacheStats, CODEGEN_PARTS};
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
    translate_library_with(lib, blocks_per_function, optimization::ALL_SAFE).map(|t| (t.census, t.functions))
}

/// What [`translate_library_with`] measured.
struct Translated {
    census: Vec<u64>,
    functions: usize,
    /// The shared cache's counters (zero on a per-thread cache).
    stats: OdCodeCacheStats,
    /// Wall time of the whole pass (translation, the dispatcher, one tick of each block).
    wall: std::time::Duration,
}

/// As [`translate_library`], with `optimizations` (dynarmic's flags, within `INTERRUPTIBLE`, and
/// the global-monitor flag added, as omni-cpu runs).
fn translate_library_with(lib: &str, blocks_per_function: usize, optimizations: u32) -> Option<Translated> {
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
            optimizations: (optimizations & optimization::INTERRUPTIBLE) | optimization::UNSAFE_IGNORE_GLOBAL_MONITOR,
            code_cache_size: 256 << 20,
            ..VmOptions::default()
        },
    );
    // SAFETY: a process-wide counter reset; this binary runs one test.
    unsafe { od_codegen_census_reset() };
    let started = std::time::Instant::now();
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
    let wall = started.elapsed();
    let mut stats = OdCodeCacheStats::default();
    let cache = vm.code_cache();
    if !cache.is_null() {
        // SAFETY: the cache is live (the `Vm` holds it) and `stats` is writable.
        unsafe { od_code_cache_stats_of(cache, &mut stats) };
    }
    Some(Translated { census: census(), functions: funcs.len(), stats, wall })
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

/// **What first translation costs, on real Android code**, and how much of it the IR optimisation
/// passes are (the question for a cheaper first tier). Each library's functions' first blocks are
/// translated on a shared cache (`OD_TEST_SHARED_CACHE=1`, required: the counters are the cache's)
/// with dynarmic's safe optimisations on, as omni-cpu runs, and with GetSetElimination,
/// ConstProp and MiscIROpt off. `translate` is the frontend (decode to IR and every pass),
/// `emit` the x64 backend; `wall` includes the dispatcher and running one tick of each block.
///
/// ```text
/// OMNI_SYSROOT=<sysroot/aosp-35> OD_TEST_SHARED_CACHE=1 ///   cargo test -p dynarmic-sys --release --test code_size -- --ignored --nocapture the_cost_of
/// ```
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_first_translation() {
    if !harness::every_vm_on_a_shared_cache() {
        println!("needs OD_TEST_SHARED_CACHE=1: skipped");
        return;
    }
    let libs = [
        "/apex/com.android.runtime/lib64/bionic/libc.so",
        "/apex/com.android.art/lib64/libart.so",
        "/system/lib64/libhwui.so",
        "/system/lib64/libandroid_runtime.so",
    ];
    let unoptimised = optimization::ALL_SAFE & !(optimization::GET_SET_ELIMINATION | optimization::CONST_PROP | optimization::MISC_IR_OPT);
    for lib in libs {
        for (what, flags) in [("optimised  ", optimization::ALL_SAFE), ("no IR opts ", unoptimised)] {
            let Some(t) = translate_library_with(lib, 4, flags) else {
                println!("{lib}: not in the sysroot (OMNI_SYSROOT): skipped");
                break;
            };
            let s = &t.stats;
            let blocks = s.blocks_emitted.max(1) as f64;
            let guest = t.census[12].max(1) as f64;
            println!(
                "{lib} {what}: {} fns, {} blocks, {} guest insns, {:.1} B/block | translate {:.1} us/block, emit {:.1} us/block,                  {:.2} us/guest insn together | wall {:.2} s ({:.1} us/block; translate+emit {:.0}%)",
                t.functions,
                s.blocks_emitted,
                t.census[12],
                s.code_bytes_emitted as f64 / blocks,
                s.translate_ns as f64 / 1e3 / blocks,
                s.emit_ns as f64 / 1e3 / blocks,
                (s.translate_ns + s.emit_ns) as f64 / 1e3 / guest,
                t.wall.as_secs_f64(),
                t.wall.as_secs_f64() * 1e6 / blocks,
                100.0 * (s.translate_ns + s.emit_ns) as f64 / 1e9 / t.wall.as_secs_f64(),
            );
        }
    }
}

/// The emit observer's sink: every block's guest PC, total size and code bytes, in order.
struct Dump {
    out: std::io::BufWriter<std::fs::File>,
}

unsafe extern "C" fn dump_block(ctx: *mut std::ffi::c_void, guest_pc: u64, host: *const std::ffi::c_void, code_bytes: usize, total_bytes: usize) {
    use std::io::Write;
    // SAFETY: `ctx` is the test's live `Dump`, used by the one emitting thread; `host` is valid for
    // `code_bytes` reads (the block just emitted).
    let (dump, code) = unsafe { (&mut *(ctx as *mut Dump), std::slice::from_raw_parts(host as *const u8, code_bytes)) };
    let hex: String = code.iter().map(|b| format!("{b:02x}")).collect();
    let _ = writeln!(dump.out, "{guest_pc:x} {total_bytes} {hex}");
}

/// **How fast the emitter is, on real code -- and what it emits, for a differential check.** The
/// corpus is the first blocks of every function of libc, libart, libhwui and libandroid_runtime,
/// translated `OMNI_EMIT_PASSES` times (default 5) on a fresh shared cache each, as omni-cpu
/// configures the jit; the medians of the frontend's and the emitter's microseconds a block are
/// printed. `OMNI_EMIT_DUMP=<file>` also writes every block of the first pass (guest PC, total
/// size, code bytes up to the link slots), so two builds of the emitter can be compared
/// (`tools/compare_emit_dumps.py`).
///
/// ```text
/// OMNI_SYSROOT=<sysroot/aosp-35> OD_TEST_SHARED_CACHE=1 \
///   cargo test -p dynarmic-sys --release --test code_size -- --ignored --nocapture the_speed
/// ```
#[test]
#[ignore = "measurement, not a test"]
fn the_speed_of_emission() {
    if !harness::every_vm_on_a_shared_cache() {
        println!("needs OD_TEST_SHARED_CACHE=1: skipped");
        return;
    }
    let passes: usize = std::env::var("OMNI_EMIT_PASSES").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let libs = [
        "/apex/com.android.runtime/lib64/bionic/libc.so",
        "/apex/com.android.art/lib64/libart.so",
        "/system/lib64/libhwui.so",
        "/system/lib64/libandroid_runtime.so",
    ];
    let mut dump = std::env::var_os("OMNI_EMIT_DUMP")
        .map(|p| Box::new(Dump { out: std::io::BufWriter::new(std::fs::File::create(p).expect("the dump file")) }));
    let (mut frontend, mut emit, mut wall) = (Vec::new(), Vec::new(), Vec::new());
    let mut blocks = 0;
    for pass in 0..passes {
        if pass == 0 {
            if let Some(d) = dump.as_mut() {
                let ctx: *mut Dump = &mut **d;
                // SAFETY: `dump` outlives the observer, which is removed below; one thread emits.
                unsafe { dynarmic_sys::od_set_emit_observer(Some(dump_block), ctx.cast()) };
            }
        }
        let (mut t, mut e, mut w, mut b) = (0u64, 0u64, 0f64, 0u64);
        for lib in libs {
            let Some(r) = translate_library_with(lib, 4, optimization::ALL_SAFE) else {
                println!("{lib}: not in the sysroot (OMNI_SYSROOT): skipped");
                return;
            };
            t += r.stats.translate_ns;
            e += r.stats.emit_ns;
            w += r.wall.as_secs_f64();
            b += r.stats.blocks_emitted;
        }
        // SAFETY: removes the observer.
        unsafe { dynarmic_sys::od_set_emit_observer(None, std::ptr::null_mut()) };
        blocks = b;
        frontend.push(t as f64 / 1e3 / b as f64);
        emit.push(e as f64 / 1e3 / b as f64);
        wall.push(w * 1e6 / b as f64);
    }
    drop(dump);
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let emits: Vec<f64> = emit.iter().map(|x| (x * 100.0).round() / 100.0).collect();
    println!(
        "corpus {blocks} blocks, {passes} passes: frontend {:.2} us/block, emit {:.2} us/block, wall {:.2} us/block (medians; emit per pass {emits:?})",
        median(&mut frontend),
        median(&mut emit),
        median(&mut wall),
    );
}
