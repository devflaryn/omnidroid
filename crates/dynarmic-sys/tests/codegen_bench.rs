//! **What typical arm64 Android code costs in generated x64 code**, by construct. Measurements,
//! not assertions, `#[ignore]`d:
//!
//! ```text
//! cargo test -p dynarmic-sys --release --test codegen_bench -- --ignored --nocapture --test-threads 1
//! OD_TEST_SHARED_CACHE=1 ...   # the same on a shared code cache (patch 0022), as omni-cpu runs
//! ```
//!
//! Every loop runs as `omni-cpu` runs a guest: `check_halt_on_memory_access`, the inline exclusives
//! under the value-compare monitor, `INTERRUPTIBLE`, and -- with cycle counting on -- slices of a
//! million instructions refilled between `Run`s, as `omni_cpu::run` does.

mod harness;

use std::time::{Duration, Instant};

use dynarmic_sys::{od_jit_clear_halt, optimization, OD_HALT_CACHE_INVALIDATION};
use harness::a64::{self, cond};
use harness::{Vm, VmOptions, CODE_BASE, HALT_DONE};

/// Samples per configuration; odd, so the median is an observation.
const N: usize = 21;
/// `omni_cpu::run::SLICE_INSTRUCTIONS`.
const SLICE: u64 = 1_000_000;
/// Loop iterations per sample.
const ITERATIONS: u64 = 200_000;
/// A data area inside the harness arena (guest addresses are arena offsets).
const DATA: u64 = 0x1000;

fn options(cycle_counting: bool) -> VmOptions {
    VmOptions {
        cycle_counting,
        check_halt_on_memory_access: true,
        fastmem_exclusive: true,
        optimizations: optimization::INTERRUPTIBLE | optimization::UNSAFE_IGNORE_GLOBAL_MONITOR,
        ..VmOptions::default()
    }
}

/// Run from [`CODE_BASE`] to the program's `SVC`, in slices when counting.
fn run_once(vm: &Vm) {
    vm.set_pc(CODE_BASE);
    loop {
        vm.with_ctx(|c| {
            c.ticks_remaining = SLICE;
            c.ticks_used = 0;
        });
        let hr = vm.run();
        if hr & HALT_DONE != 0 {
            // SAFETY: the jit is live and not executing.
            unsafe { od_jit_clear_halt(vm.raw(), HALT_DONE) };
            return;
        }
        if hr & OD_HALT_CACHE_INVALIDATION != 0 {
            // SAFETY: as above.
            unsafe { od_jit_clear_halt(vm.raw(), OD_HALT_CACHE_INVALIDATION) };
            continue;
        }
        assert_eq!(hr, 0, "unexpected halt {hr:#x}");
    }
}

/// Median ns per loop iteration, after one warm-up run.
fn ns_per_iteration(code: &[u32], cycle_counting: bool) -> f64 {
    let vm = Vm::new(code.to_vec(), options(cycle_counting));
    run_once(&vm);
    let mut samples: Vec<Duration> = (0..N)
        .map(|_| {
            let t = Instant::now();
            run_once(&vm);
            t.elapsed()
        })
        .collect();
    samples.sort_unstable();
    samples[N / 2].as_secs_f64() * 1e9 / ITERATIONS as f64
}

/// `X0` = iterations, `X1` = [`DATA`], `body`, `SUBS X0, X0, #1; B.NE body`, `SVC #0`, then
/// `tail` (out-of-line code the body calls, at a known offset: `tail_at` is filled in).
fn looped(body: impl Fn(usize) -> Vec<u32>, tail: &[u32]) -> Vec<u32> {
    let mut code = a64::mov64(0, ITERATIONS);
    code.extend(a64::mov64(1, DATA));
    let start = code.len();
    // The body's length does not depend on where the tail is, so build it once to measure it.
    let len = body(0).len();
    let tail_at = start + len + 3;
    let b = body(tail_at);
    assert_eq!(b.len(), len);
    code.extend(b);
    code.push(a64::subs_imm(0, 0, 1));
    let here = code.len();
    code.push(a64::b_cond(cond::NE, start as i32 - here as i32));
    code.push(a64::svc(0));
    assert_eq!(code.len(), tail_at);
    code.extend_from_slice(tail);
    code
}

// Encodings the harness does not have.
/// `ADRP Xd, #pages` relative to this instruction's page.
const fn adrp(rd: u32, pages: i32) -> u32 {
    let imm = pages as u32;
    0x9000_0000 | ((imm & 3) << 29) | (((imm >> 2) & 0x7FFFF) << 5) | rd
}
/// `CSEL Xd, Xn, Xm, cond`.
const fn csel(rd: u32, rn: u32, rm: u32, c: u32) -> u32 {
    0x9A80_0000 | (rm << 16) | (c << 12) | (rn << 5) | rd
}
/// `CSINC Xd, XZR, XZR, !cond` = `CSET Xd, cond`.
const fn cset(rd: u32, c: u32) -> u32 {
    0x9A9F_07E0 | ((c ^ 1) << 12) | rd
}
/// `CMP Xn, Xm` = `SUBS XZR, Xn, Xm`.
const fn cmp(rn: u32, rm: u32) -> u32 {
    a64::subs_shifted(31, rn, rm)
}
/// `CBNZ Xt, offset`.
const fn cbnz(rt: u32, off: i32) -> u32 {
    0xB500_0000 | (((off as u32) & 0x7FFFF) << 5) | rt
}
/// `LDAXR`/`STLXR` X forms.
const fn ldaxr(rt: u32, rn: u32) -> u32 {
    a64::ldxr(rt, rn) | (1 << 15)
}
const fn stlxr(rs: u32, rt: u32, rn: u32) -> u32 {
    a64::stxr(rs, rt, rn) | (1 << 15)
}
/// `FMADD Dd, Dn, Dm, Da`.
const fn fmadd_d(rd: u32, rn: u32, rm: u32, ra: u32) -> u32 {
    0x1F40_0000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}
/// `FMUL Vd.4S, Vn.4S, Vm.4S`.
const fn fmul_4s(rd: u32, rn: u32, rm: u32) -> u32 {
    0x6E20_DC00 | (rm << 16) | (rn << 5) | rd
}
/// `FADD Vd.4S, Vn.4S, Vm.4S`.
const fn fadd_4s(rd: u32, rn: u32, rm: u32) -> u32 {
    0x4E20_D400 | (rm << 16) | (rn << 5) | rd
}
/// `FMLA Vd.4S, Vn.4S, Vm.4S`.
const fn fmla_4s(rd: u32, rn: u32, rm: u32) -> u32 {
    0x4E20_CC00 | (rm << 16) | (rn << 5) | rd
}
/// `UBFX Xd, Xn, #lsb, #width` (UBFM).
const fn ubfx(rd: u32, rn: u32, lsb: u32, width: u32) -> u32 {
    0xD340_0000 | (lsb << 16) | ((lsb + width - 1) << 10) | (rn << 5) | rd
}
/// `MADD Xd, Xn, Xm, Xa`.
const fn madd(rd: u32, rn: u32, rm: u32, ra: u32) -> u32 {
    0x9B00_0000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}
/// `LDR Wt, [Xn, #imm]` (32-bit).
const fn ldr_w(rt: u32, rn: u32, byte_offset: u32) -> u32 {
    0xB940_0000 | ((byte_offset / 4) << 10) | (rn << 5) | rt
}

/// **What `enable_cycle_counting` costs.** On, every block subtracts its length from the budget
/// on the stack and each link compares it; off, each link compares the halt word instead. Short
/// blocks are where the difference is largest, so the workloads are short-block shapes.
///
/// MEASURED 2026-10-09 (E-cores, idle priority): on the **shared** code cache, which is what
/// `omni-cpu` runs on x64, counting costs nothing measurable (-16% .. +6% across the four shapes,
/// two runs; eight 2-instruction blocks 13.3 ns on, 15.9 off) -- the shared cache's link (`cmp;
/// jne; jmp [rip+slot]`) is the cost there. On a per-thread cache counting off is 1.9x faster on
/// 2-instruction blocks (12.7 -> 6.7 ns), because the budget is a store-forwarded read-modify-write
/// chain through one stack slot. So a watchdog without counting buys nothing on x64 as it runs.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_cycle_counting() {
    let shared = harness::every_vm_on_a_shared_cache();
    println!("\n== cycle counting on vs off (n = {N}, {ITERATIONS} iterations, shared cache {shared}) ==");
    // Eight two-instruction blocks: `ADD; B.NE next` (both ways are the next block).
    let tiny = looped(|_| (0..8).flat_map(|i| [a64::add_imm(2 + i % 4, 2 + i % 4, 1), a64::b_cond(cond::NE, 1)]).collect(), &[]);
    // A call to a three-instruction leaf and back (return-stack buffer), twice. The leaf sits
    // before the loop, jumped over.
    let calls = {
        let mut code = a64::mov64(0, ITERATIONS);
        code.extend(a64::mov64(1, DATA));
        code.push(a64::b(4)); // over the leaf
        let leaf = code.len();
        code.extend([a64::add_imm(3, 3, 1), a64::add_imm(4, 4, 3), a64::ret(30)]);
        let start = code.len();
        code.push(a64::bl(leaf as i32 - code.len() as i32));
        code.push(a64::add_imm(5, 5, 1));
        code.push(a64::bl(leaf as i32 - code.len() as i32));
        code.push(a64::subs_imm(0, 0, 1));
        code.push(a64::b_cond(cond::NE, start as i32 - code.len() as i32));
        code.push(a64::svc(0));
        code
    };
    // Android-ish: five-instruction blocks with a load, an add, a store, a compare and a branch.
    let mixed = looped(
        |_| {
            (0..4)
                .flat_map(|i| {
                    [
                        a64::ldr_imm(5, 1, 8 * i),
                        a64::add_imm(5, 5, 1),
                        a64::str_imm(5, 1, 8 * i),
                        cmp(5, 0),
                        a64::b_cond(cond::NE, 1),
                    ]
                })
                .collect()
        },
        &[],
    );
    // One long block: twenty dependent adds.
    let long = looped(|_| (0..20).map(|i| a64::add_imm(2 + i % 6, 2 + (i + 1) % 6, 1)).collect(), &[]);

    for (name, code, insns) in [
        ("8 two-insn blocks", &tiny, 18.0),
        ("2 calls to a 3-insn leaf", &calls, 11.0),
        ("4 five-insn ld/st blocks", &mixed, 22.0),
        ("one 22-insn block", &long, 22.0),
    ] {
        let on = ns_per_iteration(code, true);
        let off = ns_per_iteration(code, false);
        println!(
            "  {name:26}: counting on {on:7.3} ns/iter, off {off:7.3} ns/iter  ({:+.1}%, {:.2} ns/block-ish, {:.0} vs {:.0} Minsn/s)",
            (on / off - 1.0) * 100.0,
            on - off,
            insns * 1e3 / on,
            insns * 1e3 / off,
        );
    }
}

/// **Common arm64 Android constructs, one loop each**, ns per iteration with cycle counting on (as
/// `omni-cpu` runs). Each body repeats its construct four times so the loop overhead (`SUBS; B.NE`,
/// about one block link) is amortised; the empty loop is printed first as the floor.
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_common_constructs() {
    let shared = harness::every_vm_on_a_shared_cache();
    println!("\n== common constructs, ns per iteration of 4 repeats (n = {N}, shared cache {shared}) ==");
    let reps = |words: &[u32]| -> Vec<u32> { (0..4).flat_map(|_| words.iter().copied()).collect() };
    let cases: Vec<(&str, Vec<u32>)> = vec![
        ("empty loop", vec![]),
        ("ADD x4", reps(&[a64::add_imm(2, 2, 1)])),
        ("LDR x4", reps(&[a64::ldr_imm(5, 1, 0)])),
        ("LDR W x4", reps(&[ldr_w(5, 1, 0)])),
        ("STR x4", reps(&[a64::str_imm(2, 1, 0)])),
        ("LDR+LDR (as a pair) x4", reps(&[a64::ldr_imm(5, 1, 0), a64::ldr_imm(6, 1, 8)])),
        ("LDP x4", reps(&[a64::ldp_imm(5, 6, 1, 0)])),
        ("STR+STR (as a pair) x4", reps(&[a64::str_imm(2, 1, 16), a64::str_imm(3, 1, 24)])),
        ("STP x4", reps(&[a64::stp_imm(2, 3, 1, 16)])),
        ("LDR Q x4", reps(&[a64::ldr_q_imm(1, 1, 32)])),
        ("STR Q x4", reps(&[a64::str_q_imm(1, 1, 32)])),
        ("ADRP+ADD x4", reps(&[adrp(7, 0), a64::add_imm(7, 7, 0x10)])),
        ("ADRP+LDR (code page) x4", reps(&[adrp(7, 0), a64::ldr_imm(8, 7, 0)])),
        ("CMP+CSEL x4", reps(&[cmp(2, 3), csel(4, 2, 3, cond::GT)])),
        ("CMP+CSET x4", reps(&[cmp(2, 3), cset(4, cond::EQ)])),
        ("UBFX x4", reps(&[ubfx(4, 2, 3, 7)])),
        ("MADD x4", reps(&[madd(4, 2, 3, 4)])),
        ("FADD D x4", reps(&[a64::fadd_d(1, 1, 2)])),
        ("FMUL D x4", reps(&[a64::fmul_d(1, 1, 2)])),
        ("FMADD D x4", reps(&[fmadd_d(1, 1, 2, 3)])),
        ("SCVTF+FCVTZS x4", reps(&[a64::scvtf_d_from_x(1, 2), a64::fcvtzs_x_from_d(4, 1)])),
        ("ADD .4S x4", reps(&[a64::add_vec_4s(1, 1, 2)])),
        ("FADD .4S x4", reps(&[fadd_4s(1, 1, 2)])),
        ("FMUL .4S x4", reps(&[fmul_4s(1, 1, 2)])),
        ("FMLA .4S x4", reps(&[fmla_4s(1, 2, 3)])),
        ("LDXR+ADD+STXR (no retry) x4", reps(&[a64::ldxr(5, 1), a64::add_imm(5, 5, 1), a64::stxr(6, 5, 1)])),
        ("LDAXR+ADD+STLXR x4", reps(&[ldaxr(5, 1), a64::add_imm(5, 5, 1), stlxr(6, 5, 1)])),
        ("CBNZ (not taken, own block) x4", reps(&[cbnz(31, 1)])),
    ];
    for (name, body, ) in &cases {
        let code = looped(|_| body.clone(), &[]);
        let on = ns_per_iteration(&code, true);
        println!("  {name:32}: {on:7.3} ns/iter");
    }
}

/// Where scalar floating point goes: the same `FADD`s dependent and independent, a plain move,
/// and each under the unsafe floating-point flags (patch 0034's live switch).
#[test]
#[ignore = "measurement, not a test"]
fn the_cost_of_scalar_floating_point() {
    println!("\n== scalar floating point, ns per iteration of 4 repeats (n = {N}) ==");
    let reps = |words: &[u32]| -> Vec<u32> { (0..4).flat_map(|_| words.iter().copied()).collect() };
    let cases: Vec<(&str, Vec<u32>)> = vec![
        ("FADD D1,D1,D2 (chain)", reps(&[a64::fadd_d(1, 1, 2)])),
        ("FADD D1,D2,D3 (independent)", reps(&[a64::fadd_d(1, 2, 3)])),
        ("FADD D4..7 (4 chains)", vec![a64::fadd_d(4, 4, 2), a64::fadd_d(5, 5, 2), a64::fadd_d(6, 6, 2), a64::fadd_d(7, 7, 2)]),
        ("FADD S1,S1,S2 (chain)", reps(&[0x1E22_2821])),
        ("FADD S1,S2,S3 (independent)", reps(&[0x1E23_2841])),
        ("FMADD D1,D2,D3,D1 (chain)", reps(&[fmadd_d(1, 2, 3, 1)])),
        ("FMOV D1,X2", reps(&[a64::fmov_d_from_x(1, 2)])),
        ("FMOV X4,D2", reps(&[a64::fmov_x_from_d(4, 2)])),
        ("SCVTF D1,X2", reps(&[a64::scvtf_d_from_x(1, 2)])),
        ("FCVTZS X4,D2", reps(&[a64::fcvtzs_x_from_d(4, 2)])),
        ("ADD .2D (vector int)", reps(&[a64::add_vec_2d(1, 1, 2)])),
        ("FADD .4S (independent)", reps(&[fadd_4s(1, 2, 3)])),
        ("FMLA .4S (chain)", reps(&[fmla_4s(1, 2, 3)])),
    ];
    for (mask, xmm, label) in [
        (0u32, false, "upstream"),
        (0, true, "0039 xmm"),
        (optimization::UNSAFE_FP, false, "UNSAFE_FP"),
        (optimization::UNSAFE_FP, true, "xmm+UNSAFE"),
    ] {
        // SAFETY: stores process-wide atomics; this binary runs its tests one at a time.
        unsafe {
            dynarmic_sys::od_set_live_fp_optimizations(mask);
            dynarmic_sys::od_set_scalar_fp_in_xmm(u32::from(xmm));
        }
        for (name, body) in &cases {
            let code = looped(|_| body.clone(), &[]);
            println!("  {label:10} {name:30}: {:7.3} ns/iter", ns_per_iteration(&code, true));
        }
    }
    // SAFETY: as above.
    unsafe {
        dynarmic_sys::od_set_live_fp_optimizations(0);
        dynarmic_sys::od_set_scalar_fp_in_xmm(0);
    }
}

/// The scalar `FADD` chain against the host doing the same, and under other pass configurations.
#[test]
#[ignore = "measurement, not a test"]
fn the_scalar_fadd_chain_against_the_host() {
    println!("\n== FADD D1,D1,D2 x4 per iteration ==");
    // The host: four dependent adds per iteration.
    let mut acc = 0.0f64;
    let step = std::hint::black_box(1.0e-3f64);
    let mut samples = Vec::new();
    for _ in 0..N {
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            acc += step;
            acc += step;
            acc += step;
            acc += step;
            acc = std::hint::black_box(acc);
        }
        samples.push(t.elapsed());
    }
    samples.sort_unstable();
    println!("  host addsd chain                 : {:7.3} ns/iter ({acc})", samples[N / 2].as_secs_f64() * 1e9 / ITERATIONS as f64);
    let body: Vec<u32> = (0..4).map(|_| a64::fadd_d(1, 1, 2)).collect();
    let code = looped(|_| body.clone(), &[]);
    for (label, opts, xmm) in [
        ("omni options (precise GSE)", options(true), false),
        ("check_halt off (upstream GSE)", VmOptions { check_halt_on_memory_access: false, ..options(true) }, false),
        ("no GetSetElimination", VmOptions { optimizations: options(true).optimizations & !optimization::GET_SET_ELIMINATION, ..options(true) }, false),
        ("omni options + 0039 xmm", options(true), true),
    ] {
        // SAFETY: stores one process-wide atomic; this binary runs its tests one at a time.
        unsafe { dynarmic_sys::od_set_scalar_fp_in_xmm(u32::from(xmm)) };
        let vm = Vm::new(code.clone(), opts);
        // A non-zero, normal addend, so nothing here is a zero or a denormal.
        vm.set_vec(2, [1.0e-3f64.to_bits(), 0]);
        run_once(&vm);
        let mut samples: Vec<Duration> = (0..N)
            .map(|_| {
                vm.set_vec(1, [1.0f64.to_bits(), 0]);
                let t = Instant::now();
                run_once(&vm);
                t.elapsed()
            })
            .collect();
        samples.sort_unstable();
        println!(
            "  {label:32} : {:7.3} ns/iter (D1 = {})",
            samples[N / 2].as_secs_f64() * 1e9 / ITERATIONS as f64,
            f64::from_bits(vm.vec(1)[0])
        );
    }
    // SAFETY: as above.
    unsafe { dynarmic_sys::od_set_scalar_fp_in_xmm(0) };
}
