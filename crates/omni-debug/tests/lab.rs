//! The lab-session gate, driven against a **real** AArch64 library: AOSP 15's `libz.so`.
//!
//! Why a checked-in fixture rather than the Roblox APK: the APK is gitignored and the sysroot is
//! content-addressed and lives only on the Windows host, but this suite must run byte-identically on
//! Windows, Linux and macOS over SSH (the cross-OS requirement of Phase B). `libz.so` is 117 KiB,
//! its licence permits redistribution, and its `adler32`/`crc32` are pure computation with an
//! independent oracle — so `call_function` is checked against an answer worked out here, not against
//! anything the guest produced (the M2 rule: a run that skipped every instruction must fail).
//!
//! Gated to the two host architectures the translating backend exists on (omni-cpu is always built
//! with its default `dynarmic` feature here, so the backend is present on both).
#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::path::PathBuf;

use omni_debug::{HookAction, Session, Stop, TraceKind};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so")
}

fn session() -> Session {
    Session::with_library(fixture()).expect("load libz.so into a lab session")
}

/// Build a crafted `funcA -> funcB` pair in guest memory and return their entry addresses plus the
/// address of the instruction after the `BL` (the real return site inside funcA). `funcA` sets
/// `x0 = 0x99` before calling; `funcB` returns `0x42`. Used to exercise interception with a *real*
/// caller — a function called directly returns to the sentinel, which is not an ordinary return site.
fn a_calls_b(s: &mut Session) -> (usize, usize, usize) {
    // funcB: mov x0,#0x42 ; ret
    let fb = s.load_code(&[0xD280_0840, 0xD65F_03C0]).unwrap();
    // funcA: stp x29,x30,[sp,#-16]! ; mov x29,sp ; mov x0,#0x99 ; bl <patch> ; ldp; ret
    let fa = s
        .load_code(&[0xA9BF_7BFD, 0x9100_03FD, 0xD280_1320, 0x9400_0000, 0xA8C1_7BFD, 0xD65F_03C0])
        .unwrap();
    let bl_site = fa + 12;
    let delta = fb as i64 - bl_site as i64;
    let bl = 0x9400_0000u32 | (((delta >> 2) as u32) & 0x03FF_FFFF);
    s.write_mem(bl_site, &bl.to_le_bytes()).unwrap();
    (fa, fb, bl_site + 4)
}

/// The reference adler-32, computed here so `call_function`'s answer is checked against arithmetic
/// worked out independently, not against anything the guest produced (the M2 rule).
fn adler32_oracle(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

// -------------------------------------------------------------------------------------------------
// Introspection
// -------------------------------------------------------------------------------------------------

#[test]
fn resolves_exported_symbols_to_guest_addresses() {
    let s = session();
    let (base, start, end) = s.module_span("libz.so").unwrap();
    for name in ["adler32", "crc32", "zlibVersion"] {
        let sym = s.resolve_symbol(name).unwrap_or_else(|e| panic!("resolve {name}: {e}"));
        assert!(
            sym.address >= start && sym.address < end,
            "{name} at {:#x} must be inside the module [{start:#x},{end:#x})",
            sym.address
        );
        assert!(sym.address >= base, "{name} must be at or above the load bias");
    }
    assert!(matches!(s.resolve_symbol("no_such_symbol"), Err(_)));
}

#[test]
fn the_memory_map_shows_an_executable_region_for_the_module() {
    let s = session();
    let maps = s.list_maps();
    assert!(!maps.is_empty(), "a loaded module must show mapped regions");
    let exec = maps.iter().filter(|m| m.perms[2] == b'x').count();
    assert!(exec >= 1, "libz.so must have at least one r-x region: {:#?}", maps);
    assert!(
        maps.iter().any(|m| m.what == "libz.so"),
        "at least one region must be attributed to the module"
    );
}

#[test]
fn read_mem_sees_the_elf_header_at_the_module_start() {
    let s = session();
    let (_base, start, _end) = s.module_span("libz.so").unwrap();
    let head = s.read_mem(start, 20).unwrap();
    assert_eq!(&head[0..4], b"\x7fELF", "the module image must start with the ELF magic");
    assert_eq!(head[4], 2, "ELFCLASS64");
    assert_eq!(u16::from_le_bytes([head[18], head[19]]), 183, "EM_AARCH64");
}

#[test]
fn dump_module_returns_the_in_memory_image() {
    let s = session();
    let (_b, start, end) = s.module_span("libz.so").unwrap();
    let dump = s.dump_module("libz.so").unwrap();
    assert_eq!(dump.len(), end - start, "the dump is the whole loaded span");
    assert_eq!(&dump[0..4], b"\x7fELF", "the dump is a recognisable image");
    // The dump agrees with a direct read of the same bytes: one path cannot silently diverge.
    let via_read = s.read_mem(start, 4096).unwrap();
    assert_eq!(&dump[0..4096], &via_read[..], "dump and read_mem must agree byte for byte");
    assert!(matches!(s.dump_module("nope"), Err(_)));
}

// -------------------------------------------------------------------------------------------------
// call_function: the differentiator
// -------------------------------------------------------------------------------------------------

#[test]
fn call_function_computes_adler32_matching_the_oracle() {
    let mut s = session();
    let adler = s.resolve_symbol("adler32").unwrap().address;
    // uLong adler32(uLong adler, const Bytef *buf, uInt len); the initial value is 1.
    // Real buffers of two sizes. (adler32(1, NULL, 0) is skipped: this zlib-ng build dispatches the
    // empty-input edge through a functable pointer a constructor sets, and the lab runs no
    // init_array — the same reason a wild branch is a typed error, tested separately.)
    for data in [&b"hello world from omnidroid's emulation-layer debugger"[..], &vec![0xABu8; 4096][..]]
    {
        let buf = s.alloc_data(data).unwrap() as u64;
        let out = s
            .call_function(adler, &[1, buf, data.len() as u64])
            .unwrap_or_else(|e| panic!("call adler32(len={}): {e}", data.len()));
        assert_eq!(out.ret as u32, adler32_oracle(data), "adler32(len={}) matches oracle", data.len());
        assert!(out.instructions > 0, "a real run executes instructions (M2 rule)");
    }
}

/// A function that branches to an unresolved address stops with a typed error rather than crashing
/// the host — the guest is untrusted (Global Constraint 11) and *will* reach garbage. A crafted stub
/// (`mov x0,#0 ; br x0`) makes this deterministic.
#[test]
fn a_wild_branch_is_reported_not_a_host_crash() {
    let mut s = session();
    // mov x0, #0 ; br x0  -> fetch at guest address 0
    let stub = s.load_code(&[0xD280_0000, 0xD61F_0000]).unwrap();
    let err = s.call_function(stub, &[]).expect_err("a branch to 0 must be a typed error");
    let msg = err.to_string();
    assert!(msg.contains("0x0") || msg.contains("fault"), "names the bad fetch: {msg}");
}

#[test]
fn call_function_returns_a_pointer_to_the_version_string() {
    let mut s = session();
    let f = s.resolve_symbol("zlibVersion").unwrap().address;
    let out = s.call_function(f, &[]).unwrap_or_else(|e| panic!("call zlibVersion: {e}"));
    assert!(out.ret != 0, "zlibVersion returns a non-null pointer");
    let bytes = s.read_mem(out.ret as usize, 8).unwrap();
    assert_eq!(bytes[0], b'1', "the version string starts with a major version of 1");
    assert_eq!(bytes[1], b'.', "then a dot: {:?}", String::from_utf8_lossy(&bytes));
}

// -------------------------------------------------------------------------------------------------
// Forced write into read-only code, and that it takes effect
// -------------------------------------------------------------------------------------------------

#[test]
fn write_mem_patches_read_only_code_and_the_patch_runs() {
    let mut s = session();
    let f = s.resolve_symbol("adler32").unwrap().address;

    let before = s.read_words(f, 1).unwrap().words[0];

    // Overwrite the first instruction with `RET` (0xD65F03C0). A forced write must copy-on-write the
    // read-only code page and invalidate the stale translation, or the old instruction keeps running.
    s.write_mem(f, &0xD65F_03C0u32.to_le_bytes()).unwrap();
    assert_eq!(s.read_words(f, 1).unwrap().words[0], 0xD65F_03C0, "the patch is in memory");

    // Now adler32 returns immediately, so X0 (arg0) is handed straight back — proof the patched
    // instruction executed rather than the original body running.
    let out = s.call_function(f, &[0x1234_5678, 0, 0]).unwrap();
    assert_eq!(out.ret, 0x1234_5678, "the patched RET returned arg0 unchanged");
    assert!(out.instructions <= 2, "only the RET (and sentinel) ran, not the body");

    // Restore and confirm the real function is back.
    s.write_mem(f, &before.to_le_bytes()).unwrap();
    let data = b"restored and summed";
    let buf = s.alloc_data(data).unwrap();
    let ok = s.call_function(f, &[1, buf as u64, data.len() as u64]).unwrap();
    assert_eq!(ok.ret as u32, adler32_oracle(data), "the original function works again");
}

// -------------------------------------------------------------------------------------------------
// Interception: entry/exit hooks and replace_return
// -------------------------------------------------------------------------------------------------

#[test]
fn intercept_records_entry_and_exit() {
    let mut s = session();
    let (fa, fb, _) = a_calls_b(&mut s);
    s.intercept(fb, HookAction { record_entry: true, record_exit: true, replace_return: None })
        .unwrap();
    let out = s.call_function(fa, &[]).unwrap();
    // The real answer is still produced: interception observes, it does not alter.
    assert_eq!(out.ret, 0x42, "funcB's real value still flows out through funcA");
    let enter = out.events.iter().find(|e| e.kind == TraceKind::Enter).expect("an entry event");
    assert_eq!(enter.address, fb, "the entry event names funcB");
    assert_eq!(enter.regs[0], 0x99, "the entry hook saw the argument funcA passed");
    let exit = out.events.iter().find(|e| e.kind == TraceKind::Exit).expect("an exit event");
    assert_eq!(exit.address, fb, "the exit event names funcB");
    assert_eq!(exit.regs[0], 0x42, "the exit hook saw funcB's return value");
}

#[test]
fn replace_return_skips_the_body() {
    let mut s = session();
    let (fa, fb, _) = a_calls_b(&mut s);
    s.intercept(
        fb,
        HookAction { record_entry: false, record_exit: false, replace_return: Some(0xDEAD_BEEF) },
    )
    .unwrap();
    let out = s.call_function(fa, &[]).unwrap();
    assert_eq!(out.ret, 0xDEAD_BEEF, "funcB's return was replaced, and funcA passed it through");
    assert!(
        out.events.iter().any(|e| e.kind == TraceKind::Replaced),
        "a Replaced event is recorded: {:?}",
        out.events
    );
}

// -------------------------------------------------------------------------------------------------
// Breakpoints, registers, backtrace (interactive stepping)
// -------------------------------------------------------------------------------------------------

#[test]
fn breakpoint_stops_and_backtrace_climbs_the_frame_chain() {
    let mut s = session();
    let (fa, fb, ret_site) = a_calls_b(&mut s);
    s.set_breakpoint(fb).unwrap();
    let stop = s.run_until_stop(fa, &[]).unwrap();
    match stop {
        Stop::Breakpoint { address, .. } => assert_eq!(address, fb, "stopped at funcB entry"),
        Stop::Returned(_) => panic!("the breakpoint should have stopped the run"),
    }
    // At the breakpoint, registers are valid.
    let regs = s.get_registers();
    assert_eq!(regs.pc, fb, "PC is at the breakpoint");

    // The backtrace climbs from funcB into funcA.
    let bt = s.backtrace();
    assert_eq!(bt[0], fb, "innermost frame is funcB");
    assert!(bt.len() >= 2, "the frame chain reaches funcA: {:#x?}", bt);
    assert_eq!(bt[1], ret_site, "the return site is the instruction after the BL");

    // Resuming runs to completion with funcB's result.
    match s.resume().unwrap() {
        Stop::Returned(out) => assert_eq!(out.ret, 0x42, "funcA returns funcB's value"),
        Stop::Breakpoint { .. } => panic!("no more breakpoints to hit"),
    }
}

// -------------------------------------------------------------------------------------------------
// Syscall tracing (crafted stub, since the lab has no kernel)
// -------------------------------------------------------------------------------------------------

#[test]
fn trace_syscalls_records_a_crafted_svc() {
    let mut s = session();
    s.trace_syscalls(true).unwrap();
    // mov x0,#7 ; mov x8,#93 (exit) ; svc #0 ; ret
    let stub = s.load_code(&[0xD280_00E0, 0xD280_0BA8, 0xD400_0001, 0xD65F_03C0]).unwrap();
    let out = s.call_function(stub, &[]).unwrap();
    let sys: Vec<_> = out.events.iter().filter(|e| e.kind == TraceKind::Syscall).collect();
    assert_eq!(sys.len(), 1, "exactly one syscall observed: {:?}", out.events);
    assert_eq!(sys[0].syscall_nr, 93, "the syscall number is X8");
    assert_eq!(sys[0].regs[0], 7, "arg0 was X0 at the SVC");
}
