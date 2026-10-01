//! Structured calls (C2 core): `Session::call_spec` takes an `arg_spec` — scalars and buffers —
//! allocates the buffers, passes their guest addresses in the right registers, and reads
//! out-buffers back after the call. Driven against libz's pure `adler32`.

use omni_debug::{Arg, Session};
use std::path::PathBuf;

fn libz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so")
}

#[test]
fn adler32_over_in_buffer_matches_known_value() {
    // uLong adler32(uLong adler, const Bytef *buf, uInt len); adler32(1, "Hello", 5) == 0x058c01f5.
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let r = s
        .call_spec(f, &[Arg::Scalar(1), Arg::InBuffer(b"Hello".to_vec()), Arg::Scalar(5)])
        .unwrap();
    assert_eq!(r.ret as u32, 0x058c_01f5, "adler32 of 'Hello' with seed 1");
}

#[test]
fn out_buffer_is_allocated_and_read_back() {
    // uncompress writes into an out-buffer; here we only assert the out-buffer is returned at the
    // requested length (zeroed when the function writes nothing it can reach without imports).
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    // adler32 ignores arg0 as a pointer here; we just exercise OutBuffer marshaling/readback.
    let r = s.call_spec(f, &[Arg::Scalar(1), Arg::OutBuffer(8), Arg::Scalar(0)]).unwrap();
    let out = r.out_buffers.iter().find(|(idx, _)| *idx == 1).map(|(_, b)| b.clone());
    assert_eq!(out, Some(vec![0u8; 8]), "an untouched out-buffer reads back as its zeroed length");
}

#[test]
fn zero_length_out_buffer_is_an_error() {
    let mut s = Session::with_library(libz()).unwrap();
    let f = s.resolve_symbol("adler32").unwrap().address;
    let err = s.call_spec(f, &[Arg::OutBuffer(0)]);
    assert!(err.is_err(), "a zero-length buffer must be a defined error, not a panic");
}
