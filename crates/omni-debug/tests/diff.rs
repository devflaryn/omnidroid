//! The differential oracle (C4): run two sessions over the same corpus and compare observables.
//! Same library vs itself fully matches; two different functions (adler32 vs crc32) diverge on the
//! return value at the first input.

use omni_debug::diff::{diff_calls, diff_calls_named};
use omni_debug::{Arg, Session};
use std::path::PathBuf;

fn libz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/libz.so")
}

fn corpus() -> Vec<Vec<Arg>> {
    vec![
        vec![Arg::Scalar(1), Arg::InBuffer(b"Hello".to_vec()), Arg::Scalar(5)],
        vec![Arg::Scalar(0), Arg::InBuffer(b"".to_vec()), Arg::Scalar(0)],
        vec![Arg::Scalar(7), Arg::InBuffer(vec![0xAB; 32]), Arg::Scalar(32)],
    ]
}

#[test]
fn identical_library_fully_matches() {
    let mut a = Session::with_library(libz()).unwrap();
    let mut b = Session::with_library(libz()).unwrap();
    let r = diff_calls(&mut a, &mut b, "adler32", &corpus());
    assert_eq!(r.matched, r.total, "a library must match itself over the whole corpus");
    assert_eq!(r.total, 3);
    assert!(r.first_divergence.is_none(), "no divergence: {:?}", r.first_divergence);
}

#[test]
fn different_function_diverges_on_ret() {
    let mut a = Session::with_library(libz()).unwrap();
    let mut b = Session::with_library(libz()).unwrap();
    // adler32 and crc32 take the same (seed, buf, len) shape but return different checksums.
    let r = diff_calls_named(&mut a, "adler32", &mut b, "crc32", &corpus());
    assert!(r.matched < r.total, "different functions must not fully match");
    let d = r.first_divergence.expect("a divergence must be reported");
    assert_eq!(d.observable, "ret", "they first disagree on the return value");
    assert_eq!(d.index, 0, "divergence is reported at the first input");
}
