//! Deterministic input-corpus generation (C4 of the RE workbench).
//!
//! The differential oracle needs original and candidate run over the *same* inputs, and a run that
//! can be reproduced from its seed. This module turns a template — one [`ArgTemplate`] per
//! parameter — into `count` argument vectors with a tiny seeded PRNG (no external dependency). It
//! leads with boundary scalars (0, 1, `u64::MAX`) so the common edge cases are exercised first,
//! then fills with pseudo-random values.

use crate::argspec::Arg;

/// What a parameter is, for generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// An integer/pointer scalar.
    Scalar,
    /// An input buffer of this fixed length, filled with pseudo-random bytes.
    Buffer { len: usize },
    /// An output buffer of this fixed length.
    OutBuffer { len: usize },
}

/// One parameter's generation template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgTemplate {
    /// The kind of value to generate for this parameter.
    pub kind: ArgKind,
}

/// A xorshift64* PRNG — deterministic, dependency-free, good enough for fuzzing inputs.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // Avoid the zero state, which xorshift cannot leave.
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // The "*" scramble, so low bits are well-mixed.
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
}

/// The boundary scalar values, emitted (cycled) at the front of the corpus.
const BOUNDARY_SCALARS: [u64; 3] = [0, 1, u64::MAX];

/// Generate `count` argument vectors matching `templates`, deterministically from `seed`.
///
/// The first few vectors use boundary scalars (0, 1, `u64::MAX`) for every [`ArgKind::Scalar`]
/// parameter; later vectors use PRNG values. Buffer bytes are always PRNG-filled. `count == 0`
/// yields an empty corpus.
#[must_use]
pub fn generate(templates: &[ArgTemplate], seed: u64, count: usize) -> Vec<Vec<Arg>> {
    let mut rng = XorShift64::new(seed);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let mut vector = Vec::with_capacity(templates.len());
        for t in templates {
            let arg = match t.kind {
                ArgKind::Scalar => {
                    if i < BOUNDARY_SCALARS.len() {
                        Arg::Scalar(BOUNDARY_SCALARS[i])
                    } else {
                        Arg::Scalar(rng.next_u64())
                    }
                }
                ArgKind::Buffer { len } => {
                    Arg::InBuffer((0..len).map(|_| rng.next_byte()).collect())
                }
                ArgKind::OutBuffer { len } => Arg::OutBuffer(len),
            };
            vector.push(arg);
        }
        out.push(vector);
    }
    out
}
