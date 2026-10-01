//! Per-call observation (C2): `call_traced` runs one call with syscall (and optional call-site)
//! tracing on and returns the outcome together with the event stream, then restores the tracing
//! state. A pure function like `adler32` makes no syscalls, so its trace is empty of them.

use omni_debug::{Session, TraceKind};
use std::path::PathBuf;

fn libz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so")
}

#[test]
fn traced_pure_call_reports_outcome_and_no_syscalls() {
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let buf = s.alloc_data(b"Hello").unwrap();
    let (outcome, events) = s.call_traced(f, &[1, buf as u64, 5], &[]).unwrap();
    assert_eq!(outcome.ret as u32, 0x058c_01f5);
    assert!(
        events.iter().all(|e| e.kind != TraceKind::Syscall),
        "adler32 makes no syscalls; events: {events:?}"
    );
}

#[test]
fn syscall_tracing_is_off_again_after_a_traced_call() {
    // After call_traced returns, a plain call_function must not accumulate syscall events — i.e.
    // call_traced restored the per-call tracing state.
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let buf = s.alloc_data(b"Hi").unwrap();
    let _ = s.call_traced(f, &[1, buf as u64, 2], &[]).unwrap();
    let outcome = s.call_function(f, &[1, buf as u64, 2]).unwrap();
    assert!(
        outcome.events.iter().all(|e| e.kind != TraceKind::Syscall),
        "syscall tracing should be off after call_traced; events: {:?}",
        outcome.events
    );
}
