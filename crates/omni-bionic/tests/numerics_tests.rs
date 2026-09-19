//! Phase 4 tests: numeric conversion, with **hand-computed** LP64 expected values.
//!
//! Oracles:
//! * hand-computed values for every boundary (64-bit LONG_MAX/MIN, ULONG_MAX, bases,
//!   clamping, unsigned negation);
//! * the C11 standard's return/errno/endptr contracts;
//! * the published LCG sequence for `rand` after `srand(1)`;
//! * Rust's own `i64`/`u64`/`f64`/`f32` parsing where its semantics match the C standard
//!   exactly (checked per-case below — never for errno or endptr behaviour).
//!
//! The host is LLP64 with a 32-bit `long`: every `long`-shaped expected value here is
//! written as an explicit 64-bit literal, never cast from host state.

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts::{EINVAL, ERANGE};
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;
use omni_bionic::numerics::{atoi, atoll, atof, rand, srand, strtof, strtol, strtod, strtoul};

/// Context double with a string mapped at a known address.
struct Ctx {
    mem: MockMemory,
    errno: i32,
}
impl GuestMemory for Ctx {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.mem.read(addr, buf)
    }
    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        self.mem.write(addr, buf)
    }
}
impl GuestContext for Ctx {
    fn errno(&self) -> i32 {
        self.errno
    }
    fn set_errno(&mut self, v: i32) {
        self.errno = v;
    }
    fn rand_state(&self) -> u32 {
        0
    }
    fn set_rand_state(&mut self, _: u32) {}
    fn scratch(&mut self) -> Option<(u64, usize)> {
        None
    }
}

fn ctx(s: &str) -> (Ctx, u64) {
    let mut mem = MockMemory::new();
    let start = mem.map_str(0x1000, s);
    (Ctx { mem, errno: 0 }, start)
}

const LONG_MAX: i64 = 9223372036854775807;
const LONG_MIN: i64 = -9223372036854775808;

#[test]
fn strtol_decimal_basics_and_endptr() {
    let (mut c, s) = ctx("123 rest");
    // endptr slot mapped at 0x2000.
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtol(&mut c, s, 0x2000, 10), Ok(123));
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s + 3); // after "123"

    // Negative.
    let (mut c, s) = ctx("-456");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(-456));
    // Plus sign.
    let (mut c, s) = ctx("+7");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(7));
}

#[test]
fn strtol_skips_whitespace() {
    let (mut c, s) = ctx(" \t\n 42x");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(42));
}

#[test]
fn strtol_no_digits_endptr_is_original() {
    let (mut c, s) = ctx("abc");
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtol(&mut c, s, 0x2000, 10), Ok(0));
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s, "endptr must be the ORIGINAL nptr");
    // errno untouched on no-conversion.
    assert_eq!(c.errno(), 0);
}

#[test]
fn strtol_base_autodetect() {
    // 0x prefix -> hex.
    let (mut c, s) = ctx("0x1F");
    assert_eq!(strtol(&mut c, s, 0, 0), Ok(31));
    // Leading zero -> octal.
    let (mut c, s) = ctx("017");
    assert_eq!(strtol(&mut c, s, 0, 0), Ok(15));
    // Plain -> decimal.
    let (mut c, s) = ctx("99");
    assert_eq!(strtol(&mut c, s, 0, 0), Ok(99));
    // Base 16 with prefix.
    let (mut c, s) = ctx("0xAb");
    assert_eq!(strtol(&mut c, s, 0, 16), Ok(171));
    // Base 16 WITHOUT prefix.
    let (mut c, s) = ctx("Ab");
    assert_eq!(strtol(&mut c, s, 0, 16), Ok(171));
}

#[test]
fn strtol_bare_0x_prefix_parses_the_zero() {
    // C: "0x" with no hex digits converts the "0" and stops before 'x'.
    let (mut c, s) = ctx("0xg");
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtol(&mut c, s, 0x2000, 0), Ok(0));
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s + 1);
}

#[test]
fn strtol_64bit_long_max_and_overflow_clamp() {
    // Exactly LONG_MAX.
    let (mut c, s) = ctx("9223372036854775807");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(LONG_MAX));
    assert_eq!(c.errno(), 0);
    // LONG_MAX + 1: clamp + ERANGE.
    let (mut c, s) = ctx("9223372036854775808");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(LONG_MAX));
    assert_eq!(c.errno(), ERANGE);
    // Way over.
    let (mut c, s) = ctx("99999999999999999999999");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(LONG_MAX));
    assert_eq!(c.errno(), ERANGE);
}

#[test]
fn strtol_64bit_long_min_and_negative_overflow() {
    // Exactly LONG_MIN.
    let (mut c, s) = ctx("-9223372036854775808");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(LONG_MIN));
    assert_eq!(c.errno(), 0);
    // |LONG_MIN| - 1: clamp to LONG_MIN + ERANGE.
    let (mut c, s) = ctx("-9223372036854775809");
    assert_eq!(strtol(&mut c, s, 0, 10), Ok(LONG_MIN));
    assert_eq!(c.errno(), ERANGE);
}

#[test]
fn strtol_invalid_base_einval_and_endptr() {
    let (mut c, s) = ctx("123");
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtol(&mut c, s, 0x2000, 1), Ok(0));
    assert_eq!(c.errno(), EINVAL);
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s);
    // base 37 is invalid too.
    let (mut c, s) = ctx("123");
    assert_eq!(strtol(&mut c, s, 0, 37), Ok(0));
    assert_eq!(c.errno(), EINVAL);
    // base 36 is valid.
    let (mut c, s) = ctx("z");
    assert_eq!(strtol(&mut c, s, 0, 36), Ok(35));
}

#[test]
fn strtoul_negates_in_unsigned_arithmetic() {
    // The C-standard quirk: strtoul("-1") == ULONG_MAX (64-bit).
    let (mut c, s) = ctx("-1");
    assert_eq!(strtoul(&mut c, s, 0, 10), Ok(u64::MAX));
    assert_eq!(c.errno(), 0);
    // "-2" == ULONG_MAX - 1.
    let (mut c, s) = ctx("-2");
    assert_eq!(strtoul(&mut c, s, 0, 10), Ok(u64::MAX - 1));
    // Overflow clamps to ULONG_MAX + ERANGE.
    let (mut c, s) = ctx("18446744073709551616"); // 2^64
    assert_eq!(strtoul(&mut c, s, 0, 10), Ok(u64::MAX));
    assert_eq!(c.errno(), ERANGE);
    // Exactly 2^64 - 1 fits.
    let (mut c, s) = ctx("18446744073709551615");
    assert_eq!(strtoul(&mut c, s, 0, 10), Ok(u64::MAX));
    assert_eq!(c.errno(), 0);
}

#[test]
fn strtol_digits_stopping_and_partial() {
    // "12a" with base 10 consumes "12", endptr at 'a'.
    let (mut c, s) = ctx("12a");
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtol(&mut c, s, 0x2000, 10), Ok(12));
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s + 2);
}

#[test]
fn atoi_and_atoll_truncate_like_bionic() {
    let (mut c, s) = ctx("2147483647"); // INT_MAX
    assert_eq!(atoi(&mut c, s), Ok(2147483647));
    // 2^31 truncates to INT_MIN via the C cast (bionic behaviour).
    let (mut c, s) = ctx("2147483648");
    assert_eq!(atoi(&mut c, s), Ok(-2147483648i32));
    // atoll is 64-bit.
    let (mut c, s) = ctx("9223372036854775807");
    assert_eq!(atoll(&mut c, s), Ok(9223372036854775807));
    // atoi never sets errno even on out-of-int range.
    // (int)99999999999999 = (int)(0x5AF3_107A_3FDF as i32) = (int)0x107A_3FDF? No:
    // low 32 bits of 99999999999999 = 0x5AF3107A3FDF & 0xFFFFFFFF = 0x107A3FDF... compute:
    // 99999999999999 mod 2^32 = 276447199, which as i32 is 276447199 (fits in positive i32).
    let (mut c, s) = ctx("99999999999999");
    assert_eq!(atoi(&mut c, s), Ok((99999999999999u64 % (1u64 << 32)) as u32 as i32));
    assert_eq!(c.errno(), 0);
}

// ---------------------------------------------------------------- strtod/strtof

#[test]
fn strtod_decimal_and_exponents() {
    // Hand-computed: 3.5, -2.25e2 = -225, 1e-3 = 0.001.
    let (mut c, s) = ctx("3.5");
    assert_eq!(strtod(&mut c, s, 0), Ok(3.5));
    let (mut c, s) = ctx("-2.25e2");
    assert_eq!(strtod(&mut c, s, 0), Ok(-225.0));
    let (mut c, s) = ctx("1e-3");
    assert_eq!(strtod(&mut c, s, 0), Ok(0.001));
    let (mut c, s) = ctx("  +.5x");
    assert_eq!(strtod(&mut c, s, 0), Ok(0.5));
}

#[test]
fn strtod_endptr_rules() {
    let (mut c, s) = ctx("12.5abc");
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtod(&mut c, s, 0x2000), Ok(12.5));
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s + 4);

    // No conversion: endptr = original, result 0.
    let (mut c, s) = ctx("xyz");
    c.mem.map(0x2000, &[0u8; 8]);
    assert_eq!(strtod(&mut c, s, 0x2000), Ok(0.0));
    let mut slot = [0u8; 8];
    c.mem.read(0x2000, &mut slot).unwrap();
    assert_eq!(u64::from_le_bytes(slot), s);
}

#[test]
fn strtod_inf_and_nan() {
    let (mut c, s) = ctx("inf");
    let v = strtod(&mut c, s, 0).unwrap();
    assert!(v.is_infinite() && v > 0.0);
    let (mut c, s) = ctx("-INFINITY");
    let v = strtod(&mut c, s, 0).unwrap();
    assert!(v.is_infinite() && v < 0.0);
    let (mut c, s) = ctx("nan");
    assert!(strtod(&mut c, s, 0).unwrap().is_nan());
    let (mut c, s) = ctx("NAN(123)");
    assert!(strtod(&mut c, s, 0).unwrap().is_nan());
}

#[test]
fn strtod_hex_floats() {
    // 0x1.8p3 = 1.5 * 8 = 12.0 (hand-computed).
    let (mut c, s) = ctx("0x1.8p3");
    assert_eq!(strtod(&mut c, s, 0), Ok(12.0));
    // 0x10p-2 = 16 / 4 = 4.0.
    let (mut c, s) = ctx("0x10p-2");
    assert_eq!(strtod(&mut c, s, 0), Ok(4.0));
    // "0x" alone converts the "0" (C: longest valid prefix).
    let (mut c, s) = ctx("0xzz");
    assert_eq!(strtod(&mut c, s, 0), Ok(0.0));
}

#[test]
fn strtod_overflow_clamps_and_sets_erange() {
    // 1e400 overflows f64: HUGE_VAL (inf) + ERANGE.
    let (mut c, s) = ctx("1e400");
    let v = strtod(&mut c, s, 0).unwrap();
    assert!(v.is_infinite());
    assert_eq!(c.errno(), ERANGE);
    // -1e400: -inf + ERANGE.
    let (mut c, s) = ctx("-1e400");
    let v = strtod(&mut c, s, 0).unwrap();
    assert!(v.is_infinite() && v < 0.0);
    assert_eq!(c.errno(), ERANGE);
}

#[test]
fn strtof_rounds_to_f32() {
    // 0.1 as f32 is 0.100000001490116...; widened back it differs from the f64 0.1.
    let (mut c, s) = ctx("0.1");
    let v = strtof(&mut c, s, 0).unwrap();
    assert_eq!(v, 0.1f32);
    // Distinguish f32 vs f64 rounding: 1e300 fits f64 but overflows f32.
    let (mut c, s) = ctx("1e300");
    let v = strtof(&mut c, s, 0).unwrap();
    assert!(v.is_infinite());
    assert_eq!(c.errno(), ERANGE);
}

// ---------------------------------------------------------------- rand/srand

#[test]
fn rand_srand_deterministic_published_prefix() {
    // The TYPE_0-style LCG r = (1103515245*r + 12345) mod 2^31, seeded r = seed & 0x7FFFFFFF,
    // returns the *updated* state: the sequence after srand(1) is hand-computed as
    // 1103527590, 377401575, 662824084, 1147902781, 2035015474 (verified by hand in the
    // report; the glibc TYPE_3 "1804289383..." sequence belongs to a different generator).
    let mut c2 = StatefulCtx { mem: MockMemory::new(), errno: 0, state: 0 };
    srand(&mut c2, 1);
    assert_eq!(rand(&mut c2), 1103527590);
    assert_eq!(rand(&mut c2), 377401575);
    assert_eq!(rand(&mut c2), 662824084);
    assert_eq!(rand(&mut c2), 1147902781);
    assert_eq!(rand(&mut c2), 2035015474);
    // Range: [0, RAND_MAX].
    let v = rand(&mut c2);
    assert!((0..=2147483647).contains(&v));
    // Same seed: same sequence (C requirement).
    let mut c3 = StatefulCtx { mem: MockMemory::new(), errno: 0, state: 0 };
    srand(&mut c3, 1);
    assert_eq!(rand(&mut c3), 1103527590);
}

struct StatefulCtx {
    mem: MockMemory,
    errno: i32,
    state: u32,
}
impl GuestMemory for StatefulCtx {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.mem.read(addr, buf)
    }
    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        self.mem.write(addr, buf)
    }
}
impl GuestContext for StatefulCtx {
    fn errno(&self) -> i32 {
        self.errno
    }
    fn set_errno(&mut self, v: i32) {
        self.errno = v;
    }
    fn rand_state(&self) -> u32 {
        self.state
    }
    fn set_rand_state(&mut self, s: u32) {
        self.state = s;
    }
    fn scratch(&mut self) -> Option<(u64, usize)> {
        None
    }
}

#[test]
fn strtol_null_nptr_is_named_error_not_crash() {
    let mut c = Ctx { mem: MockMemory::new(), errno: 0 };
    assert!(strtol(&mut c, 0, 0, 10).is_err());
    assert!(strtod(&mut c, 0, 0).is_err());
}

#[test]
fn atof_is_strtod_without_endptr() {
    let (mut c, s) = ctx("2.5rest");
    assert_eq!(atof(&mut c, s), Ok(2.5));
}
